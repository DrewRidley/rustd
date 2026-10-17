//! What a build was loaded from besides the source: cargo's configuration, the flags for
//! rustc in the environment, and what the build scripts read. The daemon has loaded all three
//! once. Before each build it looks whether they are still what it loaded, runs the build
//! scripts again where cargo would, and turns the build down when it would be another one.

use std::{
    hash::{Hash as _, Hasher as _},
    path::{Path, PathBuf},
};

use ide_db::base_db::Env;
use project_model::{CargoConfig, ProjectWorkspace, ProjectWorkspaceKind, TargetKind};
use rustc_hash::FxHasher;

/// The variables cargo takes flags for rustc from.
const FLAGS: &[&str] = &["RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "CARGO_BUILD_RUSTFLAGS"];

/// What the daemon's checkouts were loaded from.
#[derive(Default)]
pub(crate) struct Inputs {
    /// The files cargo's configuration is read from, those that are not there too.
    configs: Vec<Watched>,
    /// The variables of `FLAGS` as the daemon has them.
    flags: Vec<(&'static str, Option<String>)>,
    scripts: Vec<Scripts>,
}

/// A file or a directory, and what it was when the daemon read it.
struct Watched {
    path: PathBuf,
    /// Whether the directory is a package, of which cargo leaves out what a build wrote and
    /// the packages inside.
    package: bool,
    stamp: u64,
}

impl Watched {
    fn new(path: PathBuf, package: bool) -> Watched {
        let stamp = stamp(&path, package);
        Watched { path, package, stamp }
    }

    fn changed(&self) -> bool {
        stamp(&self.path, self.package) != self.stamp
    }
}

/// The build scripts of one workspace and what they said.
struct Scripts {
    workspace: ProjectWorkspace,
    config: CargoConfig,
    /// What cargo runs a script again for a change of.
    files: Vec<Watched>,
    /// The variables it runs one again for a change of, as they were when they last ran.
    variables: Vec<(String, Option<String>)>,
}

impl Inputs {
    /// Has the workspace at `project`, loaded with `config`, be one whose inputs are watched.
    pub(crate) fn watch(
        &mut self,
        project: &Path,
        workspace: &ProjectWorkspace,
        config: &CargoConfig,
    ) {
        for dir in project.ancestors() {
            for name in ["config.toml", "config"] {
                let path = dir.join(".cargo").join(name);
                if !self.configs.iter().any(|it| it.path == path) {
                    self.configs.push(Watched::new(path, false));
                }
            }
        }
        if self.flags.is_empty() {
            self.flags = FLAGS.iter().map(|&it| (it, std::env::var(it).ok())).collect();
        }
        self.scripts.extend(Scripts::of(workspace, config));
    }

    /// Why a build for a process with the variables `env` is not one of what the daemon
    /// loaded, when it is not.
    pub(crate) fn changed(&mut self, env: &Env) -> Option<String> {
        let restart = "is not what the daemon loaded, start it again";
        if let Some(config) = self.configs.iter().find(|it| it.changed()) {
            return Some(format!("{} {restart}", config.path.display()));
        }
        if let Some((name, _)) = self.flags.iter().find(|(name, was)| env.get(name) != *was) {
            return Some(format!("`{name}` {restart}"));
        }
        self.scripts.iter_mut().find_map(|it| it.changed(env))
    }
}

impl Scripts {
    fn of(workspace: &ProjectWorkspace, config: &CargoConfig) -> Option<Scripts> {
        let ProjectWorkspaceKind::Cargo { cargo, build_scripts, .. } = &workspace.kind else {
            return None;
        };
        let (mut files, mut variables) = (Vec::new(), Vec::new());
        for package in cargo.packages() {
            let Some(out_dir) = build_scripts.out_dir(package) else { continue };
            let root: &Path = cargo[package].manifest.parent().as_ref();
            let said = Said::read(out_dir.as_ref());
            variables.extend(said.variables.into_iter().map(|it| {
                let value = std::env::var(&it).ok();
                (it, value)
            }));
            if !cargo[package].is_local {
                continue;
            }
            let script = cargo[package].targets.iter().map(|&it| &cargo[it]);
            let script = script.filter(|it| it.kind == TargetKind::BuildScript);
            files.extend(script.map(|it| Watched::new(PathBuf::from(it.root.as_str()), false)));
            match said.watches {
                // A script that names nothing is run again when any file of its package changes.
                false => files.push(Watched::new(root.to_owned(), true)),
                true => files
                    .extend(said.files.into_iter().map(|it| Watched::new(root.join(it), false))),
            }
        }
        variables.sort();
        variables.dedup();
        if files.is_empty() && variables.is_empty() {
            return None;
        }
        Some(Scripts { workspace: workspace.clone(), config: config.clone(), files, variables })
    }

    /// Runs the scripts again where cargo would for a process with `env`: why what they say
    /// is no longer what was loaded, when it is not.
    fn changed(&mut self, env: &Env) -> Option<String> {
        let files = self.files.iter().any(Watched::changed);
        let variables = self.variables.iter().any(|(name, was)| env.get(name) != *was);
        if !files && !variables {
            return None;
        }
        let ProjectWorkspaceKind::Cargo { build_scripts: loaded, .. } = &self.workspace.kind else {
            return None;
        };
        let mut config = self.config.clone();
        for (name, _) in &self.variables {
            config.extra_env.insert(name.clone(), env.get(name));
        }
        let said = match self.workspace.run_build_scripts(&config, &|_| {}) {
            Ok(said) => said,
            Err(error) => return Some(format!("{error:#}")),
        };
        if let Some(error) = said.error() {
            return Some(format!("a build script failed: {}", error.trim().replace('\n', " ")));
        }
        if said != *loaded {
            return Some(
                "what a build script reads has changed and it says something else than when \
                 the daemon loaded it, start it again"
                    .to_owned(),
            );
        }
        for file in &mut self.files {
            file.stamp = stamp(&file.path, file.package);
        }
        for (name, was) in &mut self.variables {
            *was = env.get(name);
        }
        None
    }
}

/// What a build script asked cargo to watch.
#[derive(Default)]
struct Said {
    /// Whether it named anything: files or variables.
    watches: bool,
    files: Vec<String>,
    variables: Vec<String>,
}

impl Said {
    /// Reads what the script that wrote to `out_dir` printed: cargo keeps it next to it.
    fn read(out_dir: &Path) -> Said {
        let mut said = Said::default();
        let output = out_dir.parent().map(|it| it.join("output"));
        let Some(Ok(output)) = output.map(std::fs::read_to_string) else { return said };
        for line in output.lines() {
            let Some(line) = line.strip_prefix("cargo:") else { continue };
            let line = line.strip_prefix(':').unwrap_or(line);
            if let Some(file) = line.strip_prefix("rerun-if-changed=") {
                said.watches = true;
                said.files.push(file.to_owned());
            } else if let Some(variable) = line.strip_prefix("rerun-if-env-changed=") {
                said.watches = true;
                said.variables.push(variable.to_owned());
            }
        }
        said
    }
}

/// A number that changes when the file at `path`, or a file below the directory there, is
/// written, added or removed.
fn stamp(path: &Path, package: bool) -> u64 {
    let mut hasher = FxHasher::default();
    stamp_into(path, package, &mut hasher);
    hasher.finish()
}

fn stamp_into(path: &Path, package: bool, hasher: &mut FxHasher) {
    let Ok(metadata) = std::fs::metadata(path) else { return };
    if !metadata.is_dir() {
        (metadata.len(), metadata.modified().ok()).hash(hasher);
        return;
    }
    let Ok(entries) = std::fs::read_dir(path) else { return };
    let mut entries: Vec<PathBuf> = entries.filter_map(|it| Some(it.ok()?.path())).collect();
    entries.sort();
    for entry in entries {
        let built = entry.join("CACHEDIR.TAG").exists() || entry.ends_with(".git");
        if package && (built || entry.join("Cargo.toml").exists()) {
            continue;
        }
        entry.hash(hasher);
        stamp_into(&entry, package, hasher);
    }
}
