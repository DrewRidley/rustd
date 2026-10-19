//! The source of a build as it was when its tests last all passed: what `cargo rustd test`
//! picks the tests to run against. Each file is kept by a hash of it, and the text of each
//! source file, so that the daemon can tell which functions an edit since changed.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::SystemTime,
};

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

use super::{daemon, state::State};
use crate::impact::text_hash;

/// Variables that differ from one shell to the next and say nothing to a test.
const PASSING_ENV: &[&str] = &[
    "PWD",
    "OLDPWD",
    "SHLVL",
    "_",
    "TERM",
    "TERM_",
    "COLORTERM",
    "SSH_",
    "TMPDIR",
    "XPC_",
    "__CF_",
    "ITERM_",
    "LC_TERMINAL",
    "VSCODE_",
    "WINDOWID",
    "DISPLAY",
    "SECURITYSESSIONID",
    "LaunchInstanceID",
    "Apple_",
    "CLAUDE_",
    "CLAUDECODE",
    "COMMAND_MODE",
    "RUSTD_ALL_TESTS",
    "RUSTD_STATS",
    "CARGO_MAKEFLAGS",
];

/// The files of a build and the environment, at a run of its tests.
#[derive(Serialize, Deserialize, Default)]
pub(crate) struct Passed {
    files: BTreeMap<PathBuf, File>,
    env: BTreeMap<String, String>,
}

/// A file as it was, and when it was written then, which spares hashing it again.
#[derive(Serialize, Deserialize, Clone, PartialEq)]
struct File {
    hash: String,
    len: u64,
    written: u128,
}

impl Passed {
    fn dir() -> anyhow::Result<PathBuf> {
        Ok(daemon::home()?.join("passed"))
    }

    /// The source of the build `key` when its tests last all passed.
    pub(crate) fn read(key: &str) -> Option<Passed> {
        let text = std::fs::read(Passed::dir().ok()?.join(format!("{key}.json"))).ok()?;
        serde_json::from_slice(&text).ok()
    }

    /// The files of `state` and the environment as they are now. A file written when it was
    /// in `before` is not read again.
    pub(crate) fn now(state: &State, before: Option<&Passed>) -> Passed {
        let mut files = BTreeMap::new();
        for path in files_of(state) {
            let Ok(meta) = std::fs::metadata(&path) else { continue };
            let written = meta.modified().map(stamp).unwrap_or_default();
            let known = before.and_then(|it| it.files.get(&path));
            let file = match known {
                Some(file) if file.len == meta.len() && file.written == written => file.clone(),
                _ => match std::fs::read(&path) {
                    Ok(bytes) => File { hash: bytes_hash(&bytes), len: meta.len(), written },
                    Err(_) => continue,
                },
            };
            files.insert(path, file);
        }
        Passed { files, env: passing_env() }
    }

    /// The files that are not as they were in `self`.
    pub(crate) fn changed(&self, now: &Passed) -> Vec<PathBuf> {
        let mut changed: Vec<PathBuf> = now
            .files
            .iter()
            .filter(|(path, file)| self.files.get(*path).is_none_or(|it| it.hash != file.hash))
            .map(|(path, _)| path.clone())
            .collect();
        changed.extend(self.files.keys().filter(|it| !now.files.contains_key(*it)).cloned());
        changed.sort();
        changed
    }

    /// A variable the tests would see otherwise than in `self`.
    pub(crate) fn env_differs(&self, now: &Passed) -> Option<String> {
        let names = self.env.keys().chain(now.env.keys());
        names.into_iter().find(|it| self.env.get(*it) != now.env.get(*it)).cloned()
    }

    /// The hash `path` is kept by in `self`.
    pub(crate) fn hash(&self, path: &Path) -> Option<String> {
        self.files.get(path).map(|it| it.hash.clone())
    }

    /// The text of the source file `path` in `self`.
    pub(crate) fn text(&self, path: &Path) -> Option<String> {
        let file = self.files.get(path)?;
        std::fs::read_to_string(Passed::dir().ok()?.join("texts").join(&file.hash)).ok()
    }

    /// Keeps `self` as the source of the build `key` at its last run of tests that all passed,
    /// and forgets the texts no build has any more.
    pub(crate) fn write(&self, key: &str) -> anyhow::Result<()> {
        let dir = Passed::dir()?;
        let texts = dir.join("texts");
        std::fs::create_dir_all(&texts)?;
        let mut kept = BTreeMap::new();
        for (path, file) in self.files.iter().filter(|it| is_rust(it.0)) {
            let at = texts.join(&file.hash);
            if !at.exists() {
                // Read again: the file may be written while the tests ran.
                let Ok(bytes) = std::fs::read(path) else { continue };
                if bytes_hash(&bytes) != file.hash {
                    continue;
                }
                let part = texts.join(format!("{}.part", file.hash));
                std::fs::write(&part, &bytes)?;
                std::fs::rename(&part, &at)?;
            }
            kept.insert(path.clone(), file.clone());
        }
        // A source file whose text is not kept is not one the next run can compare.
        let files = self
            .files
            .iter()
            .filter(|(path, _)| !is_rust(path) || kept.contains_key(*path))
            .map(|(path, file)| (path.clone(), file.clone()))
            .collect();
        let passed = Passed { files, env: self.env.clone() };
        let path = dir.join(format!("{key}.json"));
        let part = dir.join(format!("{key}.json.part"));
        std::fs::write(&part, serde_json::to_vec(&passed)?)?;
        std::fs::rename(&part, &path).context("keeping what passed")?;
        collect_garbage(&dir, &texts);
        Ok(())
    }

    /// The source of `state` at the commit `since`, for the files that differ from it now:
    /// the files of `now` and their texts at the commit. `Err` is why git did not say.
    pub(crate) fn at_commit(
        state: &State,
        since: &str,
        now: &Passed,
    ) -> anyhow::Result<(Vec<PathBuf>, BTreeMap<PathBuf, Option<String>>)> {
        let root = &state.root;
        let listed = |args: &[&str]| -> anyhow::Result<Vec<PathBuf>> {
            let out = git(root, args)?;
            Ok(out.split('\0').filter(|it| !it.is_empty()).map(|it| root.join(it)).collect())
        };
        let mut changed =
            listed(&["diff", "--name-only", "--relative", "--no-renames", "-z", since, "--"])?;
        changed.extend(listed(&["ls-files", "-o", "--exclude-standard", "-z"])?);
        changed.retain(|it| now.files.contains_key(it) || !it.exists());
        changed.sort();
        changed.dedup();
        let mut texts = BTreeMap::new();
        for path in changed.iter().filter(|it| is_rust(it)) {
            let inside = path.strip_prefix(root).unwrap_or(path).to_string_lossy().into_owned();
            let text = git(root, &["show", &format!("{since}:./{inside}")]).ok();
            texts.insert(path.clone(), text);
        }
        Ok((changed, texts))
    }
}

/// Whether the file is Rust source, whose text the daemon reads.
pub(crate) fn is_rust(path: &Path) -> bool {
    path.extension().is_some_and(|it| it == "rs")
}

/// The files the tests of `state` can read: the ones rustc read for its programs, and every
/// file of the checkout that git does not ignore: the manifests and the lock file, what build
/// scripts read, and what a test may read as it runs.
fn files_of(state: &State) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> =
        state.programs.iter().flat_map(|it| it.inputs().iter().cloned()).collect();
    match git(&state.root, &["ls-files", "-co", "--exclude-standard", "-z"]) {
        Ok(listed) => {
            let listed = listed.split('\0').filter(|it| !it.is_empty());
            files.extend(listed.map(|it| state.root.join(it)));
        }
        Err(_) => walk(&state.root, &mut files),
    }
    files.sort();
    files.dedup();
    files
}

/// The files under `dir` but what a build writes and hidden ones, for a checkout git does not
/// know.
fn walk(dir: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let hidden = name.to_string_lossy().starts_with('.') && name != ".cargo";
        if hidden || name == "target" {
            continue;
        }
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => walk(&entry.path(), files),
            Ok(kind) if kind.is_file() => files.push(entry.path()),
            _ => {}
        }
    }
}

fn git(dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    let env: rustc_hash::FxHashMap<String, Option<String>> = Default::default();
    let out = toolchain::command("git", dir, &env).args(args).output().context("running git")?;
    anyhow::ensure!(
        out.status.success(),
        "git {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(String::from_utf8(out.stdout)?)
}

/// The hash a file is kept by: of its text as the daemon reads it, for a source file.
fn bytes_hash(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => text_hash(text),
        Err(_) => text_hash(&String::from_utf8_lossy(bytes)) + "-bytes",
    }
}

fn stamp(time: SystemTime) -> u128 {
    time.duration_since(SystemTime::UNIX_EPOCH).map(|it| it.as_nanos()).unwrap_or_default()
}

/// The variables of this process the tests see, but those that differ from one shell to
/// the next.
fn passing_env() -> BTreeMap<String, String> {
    let passing = |name: &str| {
        PASSING_ENV
            .iter()
            .any(|it| if it.ends_with('_') { name.starts_with(it) } else { name == *it })
    };
    std::env::vars().filter(|(name, _)| !passing(name)).collect()
}

/// Removes the texts no build keeps.
fn collect_garbage(dir: &Path, texts: &Path) {
    let mut kept = std::collections::BTreeSet::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|it| it == "json")
            && let Ok(text) = std::fs::read(&path)
            && let Ok(passed) = serde_json::from_slice::<Passed>(&text)
        {
            kept.extend(passed.files.into_values().map(|it| it.hash));
        }
    }
    for entry in std::fs::read_dir(texts).into_iter().flatten().flatten() {
        if !kept.contains(&*entry.file_name().to_string_lossy()) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}
