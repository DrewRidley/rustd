//! Sending what cargo built of the crates that are not the workspace's to a place other
//! machines read it from, and fetching theirs: `rustd deps push REMOTE DIR` and
//! `rustd deps pull REMOTE DIR`, in the directory of `Cargo.lock`, where `DIR` is the one
//! cargo builds a profile in, such as `target/debug`.
//!
//! Cargo names all it builds of a crate, with one set of features and dependencies by one
//! compiler, after the crate and a hash of those: the fingerprint, the build script and what
//! it wrote, the rlib. Such a unit of a crate from a registry or from git is the same
//! wherever it is built, so the remote holds one archive per unit, and a list per project of
//! the units its builds used. A machine that starts with nothing fetches the units of the
//! list and cargo then builds the crates of the workspace only. Cargo checks the fingerprint
//! of each unit it is given as it would one of its own, so a unit the build has no use for
//! costs its transfer and nothing else.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::{Arc, Mutex},
};

use anyhow::{Context, bail};
use rustc_hash::FxHashMap;

use crate::remote::{Digest, Fetched, Remote, Tally, project_name, runtime};

/// The directories of a profile that hold units, the one cargo trusts last.
const PLACES: [&str; 3] = ["deps", "build", ".fingerprint"];

/// The units a project's builds used and the archive of each: `unit hash` a line.
struct Listed(BTreeMap<String, Digest>);

impl Listed {
    fn at(project: &str) -> String {
        format!("deps/{project}")
    }

    fn parse(lines: &str) -> Listed {
        let units = lines
            .lines()
            .filter_map(|line| line.split_once(' '))
            .filter(|(unit, _)| hash_of(unit).is_some())
            .filter_map(|(unit, archive)| Some((unit.to_owned(), Digest::parse(archive)?)));
        Listed(units.collect())
    }

    fn lines(&self) -> String {
        self.0.iter().map(|(unit, archive)| format!("{unit} {archive}\n")).collect()
    }

    async fn fetch(remote: &Remote, project: &str) -> anyhow::Result<Listed> {
        let lines = remote.read_list(&Listed::at(project)).await?;
        Ok(Listed::parse(&lines.unwrap_or_default()))
    }
}

fn is_hex(text: &str, len: usize) -> bool {
    text.len() == len && text.bytes().all(|it| it.is_ascii_hexdigit())
}

/// The hash at the end of the name of a unit, `serde-33c9cbd578789acd`. A name that has one
/// is safe as the name of a file.
fn hash_of(unit: &str) -> Option<&str> {
    let (krate, hash) = unit.rsplit_once('-')?;
    let plain = |it: u8| it.is_ascii_alphanumeric() || it == b'-' || it == b'_';
    (!krate.is_empty() && krate.bytes().all(plain) && is_hex(hash, 16)).then_some(hash)
}

/// The packages of the lockfile that are read from a path: the workspace's, and the ones a
/// checkout has next to it. Cargo rebuilds those by the times of their files, which say
/// nothing on another machine.
fn local_packages(lock: &str) -> BTreeSet<String> {
    let mut local = BTreeSet::new();
    for package in lock.split("[[package]]").skip(1) {
        let field = |name: &str| {
            let line = package.lines().find_map(|it| it.strip_prefix(name)?.strip_prefix(" = "))?;
            Some(line.trim_matches('"').to_owned())
        };
        if let (Some(name), None) = (field("name"), field("source")) {
            local.insert(name);
        }
    }
    local
}

/// The units in `dir` that are not of a local package, by name.
fn units(dir: &Path, local: &BTreeSet<String>) -> anyhow::Result<BTreeSet<String>> {
    let mut units = BTreeSet::new();
    let fingerprints = dir.join(".fingerprint");
    if !fingerprints.exists() {
        return Ok(units);
    }
    for entry in std::fs::read_dir(fingerprints)? {
        if let Ok(unit) = entry?.file_name().into_string()
            && hash_of(&unit).is_some()
            && let Some((package, _)) = unit.rsplit_once('-')
            && !local.contains(package)
        {
            units.insert(unit);
        }
    }
    Ok(units)
}

/// What `deps` holds of each unit, by the hash of the unit: `libserde-HASH.rlib`.
fn outputs(dir: &Path) -> anyhow::Result<FxHashMap<String, Vec<String>>> {
    let mut outputs: FxHashMap<String, Vec<String>> = FxHashMap::default();
    let Ok(entries) = std::fs::read_dir(dir.join("deps")) else { return Ok(outputs) };
    for entry in entries {
        let Ok(name) = entry?.file_name().into_string() else { continue };
        let stem = name.split('.').next().unwrap_or(&name);
        if let Some(hash) = hash_of(stem) {
            outputs.entry(hash.to_owned()).or_default().push(name);
        }
    }
    Ok(outputs)
}

/// The archive of `unit`: what each of the places holds of it, the fingerprint last.
fn archive(dir: &Path, unit: &str, outputs: &[String]) -> anyhow::Result<Vec<u8>> {
    let packed = zstd::Encoder::new(Vec::new(), 3)?;
    let mut tar = tar::Builder::new(packed);
    tar.follow_symlinks(false);
    for output in outputs {
        tar.append_path_with_name(dir.join("deps").join(output), format!("deps/{output}"))?;
    }
    for place in &PLACES[1..] {
        let at = dir.join(place).join(unit);
        if at.exists() {
            tar.append_dir_all(format!("{place}/{unit}"), at)?;
        }
    }
    Ok(tar.into_inner()?.finish()?)
}

/// Sends the units of `dir` the project's list lacks, and lists the ones that arrived.
async fn push(
    dir: &Path,
    remote: &Remote,
    project: &str,
    local: &BTreeSet<String>,
) -> anyhow::Result<String> {
    remote.may_write()?;
    // A list this machine does not trust is one it does not add to: see `extend_list`.
    let listed = Listed::fetch(remote, project).await.unwrap_or_else(|_| Listed(BTreeMap::new()));
    let (units, outputs) = (units(dir, local)?, Arc::new(outputs(dir)?));
    let missing = units.into_iter().filter(|it| !listed.0.contains_key(it));
    let sent = Mutex::new(BTreeMap::new());
    let send = |unit: String| {
        let (outputs, sent) = (outputs.clone(), &sent);
        async move {
            let (dir, of) = (dir.to_owned(), unit.clone());
            let pack = move || {
                let outputs = hash_of(&of).and_then(|hash| outputs.get(hash));
                archive(&dir, &of, outputs.map_or(&[], |it| it))
            };
            let bytes = tokio::task::spawn_blocking(pack).await??;
            let (hash, len) = (Digest::of(&bytes), bytes.len() as u64);
            remote.send_named_by_hash(&format!("units/{hash}"), bytes).await?;
            if let Ok(mut sent) = sent.lock() {
                sent.insert(unit, hash);
            }
            Ok(Fetched::Whole(len))
        }
    };
    let tally = Tally::of(missing, remote.jobs(), send).await;
    let sent = sent.into_inner().unwrap_or_default();
    if !sent.is_empty() {
        let extend = |lines: &str| {
            let mut listed = Listed::parse(lines);
            listed.0.extend(sent.iter().map(|(unit, hash)| (unit.clone(), *hash)));
            listed.lines()
        };
        remote.extend_list(&Listed::at(project), extend).await?;
    }
    let said = format!("{} units added to `{project}`, {} bytes sent", tally.done, tally.bytes);
    tally.settle(said, remote.traffic())
}

/// Unpacks the archive of `unit` into `dir`, and only what is of this unit: an archive is
/// not trusted to say where it goes.
fn unpack(dir: &Path, unit: &str, archive: &Path) -> anyhow::Result<()> {
    let mut tar = tar::Archive::new(zstd::Decoder::new(std::fs::File::open(archive)?)?);
    tar.set_preserve_mtime(true);
    for entry in tar.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let mut parts = path.components().map(|it| it.as_os_str().to_str());
        let (Some(Some(place)), Some(Some(name))) = (parts.next(), parts.next()) else {
            bail!("{unit} holds {}", path.display());
        };
        let of_unit = match place {
            "deps" => hash_of(name.split('.').next().unwrap_or(name)) == hash_of(unit),
            "build" | ".fingerprint" => name == unit,
            _ => false,
        };
        if !of_unit {
            bail!("{unit} holds {}", path.display());
        }
        entry.unpack_in(dir)?;
    }
    Ok(())
}

/// Fetches the units of the project's list that `dir` has no fingerprint of.
async fn pull(
    dir: &Path,
    remote: &Remote,
    project: &str,
    local: &BTreeSet<String>,
) -> anyhow::Result<String> {
    tokio::fs::create_dir_all(dir).await?;
    let listed = Listed::fetch(remote, project).await?;
    let missing =
        listed.0.iter().filter(|(unit, _)| !dir.join(".fingerprint").join(unit).exists()).filter(
            |(unit, _)| unit.rsplit_once('-').is_some_and(|(package, _)| !local.contains(package)),
        );
    let fetch = |(unit, hash): (&String, &Digest)| {
        let (unit, hash) = (unit.clone(), *hash);
        async move {
            let archive = dir.join(format!("{hash}.unit"));
            let fetched =
                remote.fetch_file(&format!("units/{hash}"), &hash, &archive, |_| true).await?;
            if let Fetched::Whole(_) = fetched {
                let (dir, from) = (dir.to_owned(), archive.clone());
                let unpacked = tokio::task::spawn_blocking(move || unpack(&dir, &unit, &from));
                let unpacked = unpacked.await;
                tokio::fs::remove_file(&archive).await?;
                unpacked??;
            }
            Ok(fetched)
        }
    };
    let tally = Tally::of(missing, remote.jobs(), fetch).await;
    let said = format!("{} units of `{project}` fetched, {} bytes", tally.done, tally.bytes);
    tally.settle(said, remote.traffic())
}

/// `rustd deps push|pull REMOTE DIR [PROJECT]`, in the directory of `Cargo.lock`.
pub(crate) fn command(mut args: impl Iterator<Item = String>) -> anyhow::Result<()> {
    const USAGE: &str = "usage: rustd deps push|pull REMOTE DIR [PROJECT]";
    let (Some(verb), Some(remote), Some(dir)) = (args.next(), args.next(), args.next()) else {
        bail!(USAGE);
    };
    let project = project_name(args.next())?;
    let lock = std::fs::read_to_string("Cargo.lock").context("there is no Cargo.lock here")?;
    let local = local_packages(&lock);
    let dir = Path::new(&dir);
    let start = std::time::Instant::now();
    let done = runtime()?.block_on(async {
        let remote = Remote::open(&remote).await?;
        let done = match verb.as_str() {
            "push" => push(dir, &remote, &project, &local).await,
            "pull" => pull(dir, &remote, &project, &local).await,
            _ => bail!(USAGE),
        };
        eprintln!("remote: {}", remote.traffic());
        done
    })?;
    println!("{done} in {:.1?}", start.elapsed());
    Ok(())
}
