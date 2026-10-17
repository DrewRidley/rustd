//! What a crate of a checkout is, by what is written in it: two checkouts with the same
//! sources have the same crate, and what was compiled for one is the code of the other.

use std::{
    path::Path,
    sync::{Mutex, OnceLock},
};

use ide_db::{
    RootDatabase,
    base_db::{Crate, SourceDatabase as _},
};
use rustc_hash::{FxHashMap, FxHashSet};
use sha2::{Digest, Sha256};
use triomphe::Arc;

type Hash = [u8; 32];

/// The crates of one build. An edit makes another crate of the one it is in and of every
/// crate of the checkout that depends on it.
#[derive(Default)]
pub(crate) struct Contents {
    crates: Mutex<FxHashMap<Crate, Option<u64>>>,
    /// Where the build scripts of the crates hashed so far wrote to.
    places: Mutex<FxHashSet<String>>,
}

impl Contents {
    /// `None` for a crate that depends on one `stable_crate_id` has nothing for and that is
    /// not of the checkout.
    pub(crate) fn id(
        &self,
        db: &RootDatabase,
        checkouts: (&Path, Option<&Path>),
        krate: Crate,
        stable_crate_id: &dyn Fn(Crate) -> Option<u64>,
        process_reads: &dyn Fn(Crate) -> Vec<(String, String)>,
    ) -> Option<u64> {
        if let Some(&known) = self.crates.lock().unwrap_or_else(|e| e.into_inner()).get(&krate) {
            return known;
        }
        let id = self.hash(db, checkouts, krate, stable_crate_id, process_reads);
        self.crates.lock().unwrap_or_else(|e| e.into_inner()).insert(krate, id);
        id
    }

    /// Whether data of a crate hashed here has in it where the checkout or the output of a
    /// build script is, as `env!("CARGO_MANIFEST_DIR")` puts it there: code with such data is
    /// of this checkout alone, whatever the crate is named by.
    pub(crate) fn names_place(&self, checkouts: (&Path, Option<&Path>), bytes: &[u8]) -> bool {
        let within =
            |place: &[u8]| !place.is_empty() && bytes.windows(place.len()).any(|it| it == place);
        within(checkouts.0.as_os_str().as_encoded_bytes())
            || checkouts.1.is_some_and(|it| within(it.as_os_str().as_encoded_bytes()))
            || self
                .places
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .any(|it| within(it.as_bytes()))
    }

    fn hash(
        &self,
        db: &RootDatabase,
        checkouts: (&Path, Option<&Path>),
        krate: Crate,
        stable_crate_id: &dyn Fn(Crate) -> Option<u64>,
        process_reads: &dyn Fn(Crate) -> Vec<(String, String)>,
    ) -> Option<u64> {
        let data = krate.data(db);
        let mut hasher = Sha256::new();
        let mut field = |bytes: &[u8]| {
            hasher.update((bytes.len() as u64).to_le_bytes());
            hasher.update(bytes);
        };
        field(b"rustd crate 2");
        let name = krate.extra_data(db).display_name.as_ref().map(|it| it.to_string());
        field(name.unwrap_or_default().as_bytes());
        field(format!("{:?} {}", data.edition, data.is_proc_macro).as_bytes());
        for attr in &data.crate_attrs {
            field(attr.as_bytes());
        }
        let mut cfgs: Vec<String> =
            krate.cfg_options(db).into_iter().map(|it| it.to_string()).collect();
        cfgs.sort_unstable();
        field(cfgs.join("\n").as_bytes());

        // The files of a source root can be of more crates than this one: two checkouts
        // whose build scripts write to one directory have their packages in one root.
        let env = krate.env(db);
        let manifest = env.get("CARGO_MANIFEST_DIR")?;
        let out = env.get("OUT_DIR");
        let root = db.file_source_root(data.root_file_id).source_root_id(db);
        let root = db.source_root(root).source_root(db);
        let mut files: Vec<(Written, String, Hash)> = Vec::new();
        for file in root.iter() {
            let Some(path) = root.path_for_file(&file).and_then(|it| it.as_path()) else {
                continue;
            };
            let path = path.to_string();
            let path = Path::new(&path);
            let (by, relative) = if let Ok(relative) = path.strip_prefix(&manifest) {
                (Written::InPackage, relative)
            } else if let Some(Ok(relative)) = out.as_ref().map(|out| path.strip_prefix(out)) {
                (Written::ByBuildScript, relative)
            } else {
                continue;
            };
            let hash = FileHashes::of(db, file);
            files.push((by, relative.to_string_lossy().into_owned(), hash));
        }
        files.sort_unstable();
        for (by, path, hash) in &files {
            field(&[*by as u8]);
            field(path.as_bytes());
            field(hash);
        }
        // Sorted by its `Debug`. Where the crate is stands for itself: code that has the place
        // in it is not kept, see [`Contents::names_place`].
        let mut env = format!("{env:?}");
        // The longer path first: the others are often what it starts with.
        let mut places = vec![(manifest.as_str(), "$PACKAGE")];
        places.extend(out.as_deref().map(|out| (out, "$OUT")));
        let (checkout, base) =
            (checkouts.0.to_string_lossy(), checkouts.1.map(Path::to_string_lossy));
        places.push((&*checkout, "$CHECKOUT"));
        places.extend(base.as_deref().map(|base| (base, "$CHECKOUT")));
        places.sort_unstable_by_key(|(path, _)| std::cmp::Reverse(path.len()));
        for (path, stands_for) in places {
            env = env.replace(path, stands_for);
        }
        if let Some(out) = out {
            self.places.lock().unwrap_or_else(|e| e.into_inner()).insert(out);
        }
        field(env.as_bytes());
        // What `env!` and `option_env!` in it read of the process the build is for.
        field(format!("{:?}", process_reads(krate)).as_bytes());

        // In the order of their names: the order they are listed in is the build's.
        let mut dependencies: Vec<_> = data.dependencies.iter().collect();
        dependencies.sort_unstable_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
        for dependency in dependencies {
            let on = dependency.crate_id;
            let id = match on.data(db).origin.is_local() {
                true => self.id(db, checkouts, on, stable_crate_id, process_reads)?,
                false => stable_crate_id(on).unwrap_or_else(|| Self::version(db, on)),
            };
            field(dependency.name.as_str().as_bytes());
            field(&id.to_le_bytes());
        }
        Some(u64::from_le_bytes(hasher.finalize()[..8].try_into().ok()?))
    }

    /// What stands for a crate from outside the checkout that rustc's build of the program
    /// has no id for, one that only exports others' items or a proc macro: where its source is,
    /// what it is configured with and what it is built on. `known` gives rustc's id of a crate,
    /// if there is one, and where its source starts.
    pub(crate) fn library(
        &self,
        db: &RootDatabase,
        krate: Crate,
        root: &Path,
        known: &dyn Fn(Crate) -> Option<(Option<u64>, std::path::PathBuf)>,
        process_reads: &dyn Fn(Crate) -> Vec<(String, String)>,
    ) -> u64 {
        if let Some(&Some(id)) = self.crates.lock().unwrap_or_else(|e| e.into_inner()).get(&krate) {
            return id;
        }
        let mut hasher = Sha256::new();
        let mut field = |bytes: &[u8]| {
            hasher.update((bytes.len() as u64).to_le_bytes());
            hasher.update(bytes);
        };
        field(b"rustd library 2");
        field(&Self::version(db, krate).to_le_bytes());
        field(root.as_os_str().as_encoded_bytes());
        let mut cfgs: Vec<String> =
            krate.cfg_options(db).into_iter().map(|it| it.to_string()).collect();
        cfgs.sort_unstable();
        field(cfgs.join("\n").as_bytes());
        field(format!("{:?}", process_reads(krate)).as_bytes());
        let mut dependencies: Vec<_> = krate.data(db).dependencies.iter().collect();
        dependencies.sort_unstable_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
        for dependency in dependencies {
            let on = dependency.crate_id;
            let id = match known(on) {
                Some((Some(id), _)) => id,
                Some((None, root)) => self.library(db, on, &root, known, process_reads),
                None => Self::version(db, on),
            };
            field(dependency.name.as_str().as_bytes());
            field(&id.to_le_bytes());
        }
        let hash = hasher.finalize();
        let id = u64::from_le_bytes(hash[..8].try_into().expect("a hash is 32 bytes"));
        self.crates.lock().unwrap_or_else(|e| e.into_inner()).insert(krate, Some(id));
        id
    }

    /// A crate rustc's build of the program has nothing of, a proc macro for one, which a
    /// version of is the same wherever it is.
    fn version(db: &RootDatabase, krate: Crate) -> u64 {
        let extra = krate.extra_data(db);
        let name = extra.display_name.as_ref().map(|it| it.to_string());
        let named = format!("{name:?} {:?}", extra.version);
        let hash = Sha256::digest(named.as_bytes());
        u64::from_le_bytes(hash[..8].try_into().expect("a hash is 32 bytes"))
    }
}

/// Where a file of a crate is, by what its path is hashed from.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Written {
    InPackage,
    ByBuildScript,
}

/// The hash of each file as it was last read, for as long as the daemon runs: a build hashes
/// only what was edited since the one before.
struct FileHashes {
    files: FxHashMap<vfs::FileId, (Arc<str>, Hash)>,
}

impl FileHashes {
    fn of(db: &RootDatabase, file: vfs::FileId) -> Hash {
        static HASHES: OnceLock<Mutex<FileHashes>> = OnceLock::new();
        let hashes = HASHES.get_or_init(|| Mutex::new(FileHashes { files: FxHashMap::default() }));
        let text = db.file_text(file).text(db);
        let mut hashes = hashes.lock().unwrap_or_else(|e| e.into_inner());
        // The text it was hashed from is kept, so no other text is ever where it is.
        if let Some((known, hash)) = hashes.files.get(&file)
            && Arc::ptr_eq(known, text)
        {
            return *hash;
        }
        let hash: Hash = Sha256::digest(text.as_bytes()).into();
        hashes.files.insert(file, (text.clone(), hash));
        hash
    }
}
