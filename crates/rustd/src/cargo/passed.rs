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

/// Variables that differ from one shell to the next and say nothing to a test, so that a run
/// in another terminal is not one in another environment. A name that ends in `_` stands for
/// every variable it starts. They are: the shell's own (`PWD`, `OLDPWD`, `SHLVL`, `_`), the
/// terminal's and the session's (`TERM*`, `COLORTERM`, `SSH_*`, `XPC_*`, `__CF_*`, `ITERM_*`,
/// `LC_TERMINAL*`, `VSCODE_*`, `WINDOWID`, `DISPLAY`, `SECURITYSESSIONID`, `LaunchInstanceID`,
/// `Apple_*`, `COMMAND_MODE`), an agent's (`CLAUDE_*`, `CLAUDECODE`), rustd's own switches
/// that leave the tests as they are (`RUSTD_ALL_TESTS`, `RUSTD_STATS`), and the jobserver cargo
/// hands down (`CARGO_MAKEFLAGS`).
const PASSING_ENV: &[&str] = &[
    "PWD",
    "OLDPWD",
    "SHLVL",
    "_",
    "TERM",
    "TERM_",
    "COLORTERM",
    "SSH_",
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

/// How long before a snapshot a file has to have been written for its time to say it is as it
/// was: a time is coarse, and a file written within it may be written again with the same time.
const SETTLED: u128 = 2_000_000_000;

/// The files of a build and the environment, at a run of its tests. Paths are canonical.
#[derive(Serialize, Deserialize, Default)]
pub(crate) struct Passed {
    files: BTreeMap<PathBuf, File>,
    env: BTreeMap<String, String>,
    /// When the files were looked at.
    #[serde(default)]
    taken: u128,
}

/// A file as it was, and what says it was not written since, which spares hashing it again:
/// its size, when its contents and when its inode last changed, and the inode. A copy that
/// keeps the time of the file it replaces changes the second.
#[derive(Serialize, Deserialize, Clone, PartialEq)]
struct File {
    hash: String,
    len: u64,
    written: u128,
    #[serde(default)]
    changed: u128,
    #[serde(default)]
    inode: u64,
}

impl File {
    /// What says the file at `meta` is the one it was, but its hash.
    fn stat(meta: &std::fs::Metadata) -> File {
        let written = meta.modified().map(stamp).unwrap_or_default();
        #[cfg(unix)]
        let (changed, inode) = {
            use std::os::unix::fs::MetadataExt;
            let changed = (meta.ctime() as u128) * 1_000_000_000 + meta.ctime_nsec() as u128;
            (changed, meta.ino())
        };
        #[cfg(not(unix))]
        let (changed, inode) = (written, 0);
        File { hash: String::new(), len: meta.len(), written, changed, inode }
    }

    /// Whether the file `now` is, by what it says of itself, this one, which was looked at
    /// `taken`.
    fn same_as(&self, now: &File, taken: u128) -> bool {
        let settled = self.written.max(self.changed) + SETTLED < taken;
        settled
            && (self.len, self.written, self.changed, self.inode)
                == (now.len, now.written, now.changed, now.inode)
    }
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
        let taken = stamp(SystemTime::now());
        let mut files = BTreeMap::new();
        for path in files_of(state) {
            let Ok(meta) = std::fs::metadata(&path) else { continue };
            let stat = File::stat(&meta);
            let known = before.and_then(|it| Some((it.files.get(&path)?, it.taken)));
            let file = match known {
                Some((file, taken)) if file.same_as(&stat, taken) => file.clone(),
                _ => match std::fs::read(&path) {
                    Ok(bytes) => File { hash: bytes_hash(&bytes), ..stat },
                    Err(_) => continue,
                },
            };
            files.insert(path, file);
        }
        Passed { files, env: passing_env(), taken }
    }

    /// The first file of `state` that is not as it was in `self`, looked at again: one written
    /// while the tests ran was not what they ran on.
    pub(crate) fn written_since(&self, state: &State) -> Option<PathBuf> {
        let again = Passed::now(state, Some(self));
        self.changed(&again).into_iter().next()
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
        let passed = Passed { files, env: self.env.clone(), taken: self.taken };
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
        let root = &canonical(&state.root);
        let listed = |args: &[&str]| -> anyhow::Result<Vec<PathBuf>> {
            let out = git(root, args)?;
            let listed = out.split('\0').filter(|it| !it.is_empty());
            Ok(listed.map(|it| canonical(&root.join(it))).collect())
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

/// The path as the file system has it: with no link in it, so that git, cargo's dep-info and
/// cargo's metadata name a file alike. One that is not there is its directory's, canonical,
/// and its name.
pub(crate) fn canonical(path: &Path) -> PathBuf {
    if let Ok(it) = std::fs::canonicalize(path) {
        return it;
    }
    match (path.parent(), path.file_name()) {
        (Some(dir), Some(name)) if !dir.as_os_str().is_empty() => canonical(dir).join(name),
        _ => path.to_owned(),
    }
}

/// How many files of a directory git ignores are looked at, at most. Past that the directory
/// is what a tool made, and taken as it is.
const IGNORED_FILES: usize = 10_000;

/// The files the tests of `state` can read: the ones rustc read for its programs, every file
/// of the checkout that git does not ignore (the manifests and the lock file, what build
/// scripts read, what a test may read as it runs), and the files of its packages that git
/// ignores, which a test may read too, but what a build writes.
fn files_of(state: &State) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> =
        state.programs.iter().flat_map(|it| it.inputs().iter().map(|it| canonical(it))).collect();
    let root = canonical(&state.root);
    match git(&root, &["ls-files", "-co", "--exclude-standard", "-z"]) {
        Ok(listed) => {
            let listed = listed.split('\0').filter(|it| !it.is_empty());
            files.extend(listed.map(|it| root.join(it)));
        }
        Err(_) => {
            let mut every = usize::MAX;
            walk(&root, &mut files, &mut every, state);
        }
    }
    let mut packages: Vec<PathBuf> =
        state.programs.iter().map(|it| canonical(&it.package_dir)).collect();
    packages.sort();
    packages.dedup();
    for package in packages {
        let args = ["ls-files", "-oi", "--exclude-standard", "--directory", "-z", "--", "."];
        let Ok(listed) = git(&package, &args) else { continue };
        for ignored in listed.split('\0').filter(|it| !it.is_empty()) {
            let path = package.join(ignored.trim_end_matches('/'));
            match ignored.ends_with('/') {
                true => {
                    let mut left = IGNORED_FILES;
                    walk(&path, &mut files, &mut left, state);
                }
                false => files.push(path),
            }
        }
    }
    files.sort();
    files.dedup();
    files
}

/// The files under `dir` but what a build writes and hidden directories, at most `left` of
/// them.
fn walk(dir: &Path, files: &mut Vec<PathBuf>, left: &mut usize, state: &State) {
    let built = |dir: &Path| {
        dir.file_name().is_some_and(|it| it == "target")
            || state.programs.iter().any(|it| canonical(&it.executable).starts_with(dir))
    };
    if built(dir) {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        if *left == 0 {
            return;
        }
        let name = entry.file_name();
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => {
                let hidden = name.to_string_lossy().starts_with('.') && name != ".cargo";
                if !hidden {
                    walk(&entry.path(), files, left, state);
                }
            }
            Ok(kind) if kind.is_file() => {
                *left -= 1;
                files.push(entry.path());
            }
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

/// Removes the texts no build keeps, and the changes told to the daemon.
fn collect_garbage(dir: &Path, texts: &Path) {
    let mut kept = std::collections::BTreeSet::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.to_string_lossy().ends_with(".changes.json") {
            let _ = std::fs::remove_file(&path);
        } else if path.extension().is_some_and(|it| it == "json")
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
