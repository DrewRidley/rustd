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
};

use anyhow::{Context, bail};
use rustc_hash::FxHashMap;
use sha2::{Digest, Sha256};

use crate::remote::{Remote, transfer};

/// The directories of a profile that hold units, the one cargo trusts last.
const PLACES: [&str; 3] = ["deps", "build", ".fingerprint"];

/// The units a project's builds used and the archive of each: `unit hash` a line.
struct Listed(BTreeMap<String, String>);

impl Listed {
    fn at(project: &str) -> String {
        format!("deps/{project}")
    }

    fn fetch(remote: &Remote, project: &str) -> anyhow::Result<Listed> {
        let bytes = remote.fetch(&Listed::at(project))?.unwrap_or_default();
        let text = String::from_utf8(bytes).context("the list of units is not text")?;
        let units = text
            .lines()
            .filter_map(|line| line.split_once(' '))
            .filter(|(unit, archive)| hash_of(unit).is_some() && is_hex(archive, 64))
            .map(|(unit, archive)| (unit.to_owned(), archive.to_owned()));
        Ok(Listed(units.collect()))
    }

    fn send(&self, remote: &Remote, project: &str) -> anyhow::Result<()> {
        let text: String =
            self.0.iter().map(|(unit, archive)| format!("{unit} {archive}\n")).collect();
        remote.send(&Listed::at(project), text.as_bytes())
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

/// Sends the units of `dir` the project's list lacks, and lists them.
fn push(
    dir: &Path,
    remote: &Remote,
    project: &str,
    local: &BTreeSet<String>,
) -> anyhow::Result<String> {
    let mut listed = Listed::fetch(remote, project)?;
    let (units, outputs) = (units(dir, local)?, outputs(dir)?);
    let missing: Vec<&String> = units.iter().filter(|it| !listed.0.contains_key(*it)).collect();
    let sent = std::sync::Mutex::new(Vec::new());
    let bytes = transfer(&missing, |unit| {
        let none = Vec::new();
        let outputs = hash_of(unit).and_then(|hash| outputs.get(hash)).unwrap_or(&none);
        let bytes = archive(dir, unit, outputs)?;
        let hash = format!("{:x}", Sha256::digest(&bytes));
        remote.send(&format!("units/{hash}"), &bytes)?;
        if let Ok(mut sent) = sent.lock() {
            sent.push((unit.to_owned(), hash));
        }
        Ok(bytes.len() as u64)
    })?;
    listed.0.extend(sent.into_inner().unwrap_or_default());
    listed.send(remote, project)?;
    Ok(format!("{} units added to `{project}`, {} MB sent", missing.len(), bytes >> 20))
}

/// Fetches the units of the project's list that `dir` has no fingerprint of.
fn pull(
    dir: &Path,
    remote: &Remote,
    project: &str,
    local: &BTreeSet<String>,
) -> anyhow::Result<String> {
    std::fs::create_dir_all(dir)?;
    let listed = Listed::fetch(remote, project)?;
    let missing: Vec<&String> = listed
        .0
        .keys()
        .filter(|unit| !dir.join(".fingerprint").join(unit).exists())
        .filter(|unit| unit.rsplit_once('-').is_some_and(|(package, _)| !local.contains(package)))
        .collect();
    let bytes = transfer(&missing, |unit| {
        let hash = &listed.0[unit];
        let Some(bytes) = remote.fetch(&format!("units/{hash}"))? else {
            bail!("`{project}` lists {unit}, which is not there");
        };
        if format!("{:x}", Sha256::digest(&bytes)) != *hash {
            bail!("{unit} is not what was sent under that name");
        }
        let mut tar = tar::Archive::new(zstd::Decoder::new(&bytes[..])?);
        tar.set_preserve_mtime(true);
        for entry in tar.entries()? {
            let mut entry = entry?;
            // Only what is of this unit: an archive is not trusted to say where it goes.
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
        Ok(bytes.len() as u64)
    })?;
    Ok(format!("{} units of `{project}` fetched, {} MB", missing.len(), bytes >> 20))
}

/// `rustd deps push|pull REMOTE DIR [PROJECT]`, in the directory of `Cargo.lock`.
pub(crate) fn command(mut args: impl Iterator<Item = String>) -> anyhow::Result<()> {
    const USAGE: &str = "usage: rustd deps push|pull REMOTE DIR [PROJECT]";
    let (Some(verb), Some(remote), Some(dir)) = (args.next(), args.next(), args.next()) else {
        bail!(USAGE);
    };
    let project = args.next().unwrap_or_else(|| "default".to_owned());
    if project.contains(['/', '\\']) || project.starts_with('.') {
        bail!("`{project}` is not the name of a project");
    }
    let lock = std::fs::read_to_string("Cargo.lock").context("there is no Cargo.lock here")?;
    let local = local_packages(&lock);
    let (remote, dir) = (Remote::parse(&remote)?, Path::new(&dir));
    let start = std::time::Instant::now();
    let done = match verb.as_str() {
        "push" => push(dir, &remote, &project, &local)?,
        "pull" => pull(dir, &remote, &project, &local)?,
        _ => bail!(USAGE),
    };
    println!("{done} in {:.1?}", start.elapsed());
    Ok(())
}
