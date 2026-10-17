//! Compiled code kept on disk between runs of the daemon, by what it was compiled from.
//!
//! Nothing here knows about Rust: a key is a hash of what the value was made from, and the
//! value is whatever the caller serialized.
//!
//! The store is a directory of packs. A sealed pack is named by the hash of its bytes and never
//! changes, so the same file can be on any machine or in a bucket, and is checked against its
//! name where it arrives. A daemon appends to an open pack of its own, which it holds locked,
//! and seals it once there is enough in it; an open pack nobody holds was left by a daemon
//! that ended, and whoever finds it seals what is whole of it.

use std::{
    borrow::Cow,
    fs::File,
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    sync::{
        Mutex, OnceLock, RwLock,
        atomic::{AtomicUsize, Ordering},
    },
};

use cranelift_codegen::incremental_cache::CacheKvStore;
use rustc_hash::{FxHashMap, FxHashSet};
use sha2::{Digest, Sha256};

pub(super) const KEY: usize = 32;
const MAGIC: &[u8; 8] = b"rustdpk\x01";
/// The key, the length of the value and its checksum.
const HEADER: usize = KEY + 4 + 4;
/// An open pack smaller than this is left open: a pack per edit would be a pack per function.
const SEAL_AT: u64 = 4 << 20;
/// An open pack is sealed in the middle of a build at this size, so that no pack is too much
/// to send or fetch in one piece.
const ROLL_AT: u64 = 64 << 20;

pub(super) struct Store {
    dir: PathBuf,
    sealed: RwLock<Sealed>,
    new: Mutex<Option<New>>,
    hits: AtomicUsize,
    misses: AtomicUsize,
}

/// The sealed packs this process has read.
#[derive(Default)]
struct Sealed {
    packs: FxHashSet<String>,
    index: FxHashMap<[u8; KEY], Value>,
}

/// A value in a pack that stays mapped for as long as the process runs.
#[derive(Clone, Copy)]
struct Value {
    bytes: &'static [u8],
    checksum: u32,
}

/// The pack this process appends to. What is in it is found once it is sealed.
struct New {
    path: PathBuf,
    /// Locked for as long as it is ours.
    pack: BufWriter<File>,
    hash: Sha256,
    len: u64,
    keys: FxHashSet<[u8; KEY]>,
}

/// The file name of a sealed pack with these bytes, or `None` if they are not a pack.
pub fn pack_name(bytes: &[u8]) -> Option<String> {
    bytes.starts_with(MAGIC).then(|| format!("{}.pack", hex(&Sha256::digest(bytes))))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
        _ = write!(out, "{byte:02x}");
        out
    })
}

/// The records of a pack that are whole, and how many bytes they and the magic take.
fn records(pack: &[u8]) -> (Vec<([u8; KEY], std::ops::Range<usize>, u32)>, usize) {
    let mut found = Vec::new();
    let mut at = MAGIC.len();
    if !pack.starts_with(MAGIC) {
        return (found, 0);
    }
    while let Some(header) = pack.get(at..at + HEADER) {
        let (key, rest) = header.split_at(KEY);
        let (len, checksum) = rest.split_at(4);
        let len = u32::from_le_bytes(len.try_into().expect("four bytes")) as usize;
        let checksum = u32::from_le_bytes(checksum.try_into().expect("four bytes"));
        let start = at + HEADER;
        if pack.len() < start + len {
            break;
        }
        found.push((key.try_into().expect("a key"), start..start + len, checksum));
        at = start + len;
    }
    (found, at)
}

/// Seals the open packs in a store that daemons that are gone left behind.
pub fn seal_left_open(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for path in entries.flatten().map(|it| it.path()) {
        if path.extension().is_none_or(|it| it != "open") {
            continue;
        }
        let Ok(file) = File::open(&path) else { continue };
        // Its daemon holds it for as long as it runs.
        if file.try_lock().is_err() {
            continue;
        }
        if let Ok(pack) = std::fs::read(&path) {
            let (records, whole) = records(&pack);
            if let Some(name) = pack_name(&pack[..whole]).filter(|_| !records.is_empty()) {
                // Readers only ever see all of a sealed pack.
                let partial = dir.join(format!("{name}.{}.partial", std::process::id()));
                if std::fs::write(&partial, &pack[..whole]).is_ok() {
                    _ = std::fs::rename(partial, dir.join(name));
                }
            }
        }
        _ = std::fs::remove_file(&path);
    }
}

impl Store {
    /// The store of this process: the directory `RUSTD_CACHE` names, when it is set.
    pub(super) fn global() -> Option<&'static Store> {
        static STORE: OnceLock<Option<Store>> = OnceLock::new();
        STORE
            .get_or_init(|| {
                let dir = std::env::var_os("RUSTD_CACHE")?;
                Store::open(Path::new(&dir)).ok()
            })
            .as_ref()
    }

    pub(super) fn open(dir: &Path) -> std::io::Result<Store> {
        std::fs::create_dir_all(dir)?;
        let store = Store {
            dir: dir.to_owned(),
            sealed: RwLock::default(),
            new: Mutex::new(None),
            hits: AtomicUsize::new(0),
            misses: AtomicUsize::new(0),
        };
        seal_left_open(dir);
        store.refresh();
        Ok(store)
    }

    /// Reads the sealed packs that were added to the directory since the last call, by this
    /// daemon or any other.
    pub(super) fn refresh(&self) {
        let Ok(entries) = std::fs::read_dir(&self.dir) else { return };
        let mut added = Vec::new();
        for entry in entries.flatten() {
            let Ok(name) = entry.file_name().into_string() else { continue };
            if !name.ends_with(".pack")
                || self.sealed.read().is_ok_and(|it| it.packs.contains(&name))
            {
                continue;
            }
            let Ok(file) = File::open(entry.path()) else { continue };
            // SAFETY: a sealed pack is never written to again; one that is removed stays
            // mapped.
            let Ok(pack) = (unsafe { memmap2::Mmap::map(&file) }) else { continue };
            let pack: &'static [u8] = Box::leak(Box::new(pack));
            added.push((name, pack));
        }
        if added.is_empty() {
            return;
        }
        let Ok(mut sealed) = self.sealed.write() else { return };
        for (name, pack) in added {
            for (key, range, checksum) in records(pack).0 {
                sealed.index.entry(key).or_insert(Value { bytes: &pack[range], checksum });
            }
            sealed.packs.insert(name);
        }
    }

    /// Writes out what has been added, and seals it when there is enough of it. What is not
    /// written by the time the daemon ends is compiled again by the next.
    pub(super) fn flush(&self) {
        if let Ok(mut new) = self.new.lock() {
            let sealing = new.take_if(|it| it.len >= SEAL_AT);
            if let Some(new) = new.as_mut() {
                _ = new.pack.flush();
            }
            if let Some(new) = sealing {
                _ = new.seal(&self.dir);
            }
        }
        self.refresh();
    }

    /// The value of a key, if a sealed pack has it as it was written.
    pub(super) fn get(&self, key: &[u8; KEY]) -> Option<&'static [u8]> {
        let value = *self.sealed.read().ok()?.index.get(key)?;
        (crc32fast::hash(value.bytes) == value.checksum).then_some(value.bytes)
    }

    /// The code of a function by what it was compiled from, counted as found or not.
    pub(super) fn lookup(&self, key: &[u8; KEY]) -> Option<&'static [u8]> {
        let found = self.get(key);
        let counted = if found.is_some() { &self.hits } else { &self.misses };
        counted.fetch_add(1, Ordering::Relaxed);
        found
    }

    /// Adds a value, unless the key has one.
    pub(super) fn insert(&self, key: &[u8; KEY], value: &[u8]) {
        if self.sealed.read().is_ok_and(|it| it.index.contains_key(key)) {
            return;
        }
        let (Ok(mut slot), Ok(len)) = (self.new.lock(), u32::try_from(value.len())) else {
            return;
        };
        if slot.is_none() {
            *slot = New::create(&self.dir).ok();
        }
        let Some(new) = slot.as_mut() else { return };
        if !new.keys.insert(*key) {
            return;
        }
        // One write a record: a pack cut short loses its last record and no other.
        let mut record = Vec::with_capacity(HEADER + value.len());
        record.extend_from_slice(key);
        record.extend_from_slice(&len.to_le_bytes());
        record.extend_from_slice(&crc32fast::hash(value).to_le_bytes());
        record.extend_from_slice(value);
        new.write(&record);
        if new.len >= ROLL_AT
            && let Some(mut full) = slot.take()
        {
            // The next pack is of the same build, which does not read the one sealed here.
            let keys = std::mem::take(&mut full.keys);
            _ = full.seal(&self.dir);
            *slot = New::create(&self.dir).ok().map(|next| New { keys, ..next });
        }
    }

    /// How many functions were found, and how many were not, since the last call.
    pub(super) fn take_counts(&self) -> (usize, usize) {
        (self.hits.swap(0, Ordering::Relaxed), self.misses.swap(0, Ordering::Relaxed))
    }
}

impl New {
    fn create(dir: &Path) -> std::io::Result<New> {
        let name = format!(
            "{}-{}.open",
            std::process::id(),
            std::time::UNIX_EPOCH.elapsed().map_or(0, |it| it.as_nanos())
        );
        let path = dir.join(name);
        let file = File::create(&path)?;
        file.lock()?;
        let mut new = New {
            path,
            pack: BufWriter::new(file),
            hash: Sha256::new(),
            len: 0,
            keys: FxHashSet::default(),
        };
        new.write(MAGIC);
        Ok(new)
    }

    fn write(&mut self, bytes: &[u8]) {
        if self.pack.write_all(bytes).is_ok() {
            self.hash.update(bytes);
            self.len += bytes.len() as u64;
        }
    }

    /// Gives the pack the name of its bytes, which is when other daemons start to read it.
    fn seal(mut self, dir: &Path) -> std::io::Result<()> {
        self.pack.flush()?;
        let name = format!("{}.pack", hex(&self.hash.finalize()));
        std::fs::rename(&self.path, dir.join(name))
    }
}

/// The store as Cranelift asks for it: one of these per function compiled.
pub(super) struct Lookup(pub(super) &'static Store);

impl CacheKvStore for Lookup {
    fn get(&self, key: &[u8]) -> Option<Cow<'_, [u8]>> {
        let key = <&[u8; KEY]>::try_from(key).ok()?;
        self.0.lookup(key).map(Cow::Borrowed)
    }

    fn insert(&mut self, key: &[u8], val: Vec<u8>) {
        if let Ok(key) = <&[u8; KEY]>::try_from(key) {
            self.0.insert(key, &val);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rustd-store-{name}-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn packs(dir: &Path, extension: &str) -> Vec<PathBuf> {
        let entries = std::fs::read_dir(dir).unwrap();
        let mut found: Vec<_> = entries
            .map(|it| it.unwrap().path())
            .filter(|it| it.extension().is_some_and(|it| it == extension))
            .collect();
        found.sort();
        found
    }

    #[test]
    fn sealed_pack_is_named_by_its_bytes_and_read_back() {
        let dir = scratch("seal");
        let store = Store::open(&dir).unwrap();
        let big = vec![7; SEAL_AT as usize];
        store.insert(&[1; KEY], b"one");
        store.insert(&[2; KEY], &big);
        assert_eq!(store.get(&[1; KEY]), None);
        store.flush();
        assert_eq!(store.get(&[1; KEY]), Some(&b"one"[..]));
        assert_eq!(store.get(&[2; KEY]), Some(&big[..]));
        let sealed = packs(&dir, "pack");
        assert_eq!(sealed.len(), 1);
        let name = sealed[0].file_name().unwrap().to_str().unwrap().to_owned();
        assert_eq!(pack_name(&std::fs::read(&sealed[0]).unwrap()), Some(name));
        assert!(packs(&dir, "open").is_empty());

        let other = Store::open(&dir).unwrap();
        assert_eq!(other.get(&[1; KEY]), Some(&b"one"[..]));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn open_pack_of_a_daemon_that_ended_is_sealed_up_to_its_last_whole_record() {
        let dir = scratch("adopt");
        let store = Store::open(&dir).unwrap();
        store.insert(&[1; KEY], b"one");
        store.insert(&[2; KEY], b"two");
        store.flush();
        let open = packs(&dir, "open");
        assert_eq!(open.len(), 1);
        // Held: another daemon leaves it alone.
        let other = Store::open(&dir).unwrap();
        assert_eq!(other.get(&[1; KEY]), None);
        assert_eq!(packs(&dir, "open"), open);

        drop(store);
        let pack = std::fs::read(&open[0]).unwrap();
        std::fs::write(&open[0], &pack[..pack.len() - 1]).unwrap();
        let next = Store::open(&dir).unwrap();
        assert_eq!(next.get(&[1; KEY]), Some(&b"one"[..]));
        assert_eq!(next.get(&[2; KEY]), None);
        assert!(packs(&dir, "open").is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn value_that_is_not_what_was_written_is_not_found() {
        let dir = scratch("checksum");
        let store = Store::open(&dir).unwrap();
        store.insert(&[1; KEY], b"one");
        drop(store);
        let open = packs(&dir, "open");
        let mut pack = std::fs::read(&open[0]).unwrap();
        *pack.last_mut().unwrap() ^= 1;
        std::fs::write(&open[0], pack).unwrap();
        let next = Store::open(&dir).unwrap();
        assert_eq!(next.get(&[1; KEY]), None);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
