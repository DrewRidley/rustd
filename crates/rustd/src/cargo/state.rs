//! What a cargo build left for the builds after it: the programs and where cargo has them,
//! what the daemon is asked to build each from, and everything a change to which makes the
//! next build cargo's again.

use std::{
    collections::BTreeMap,
    io::BufReader,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::Context as _;
use cargo_metadata::{
    Artifact, BuildScript, Message, Metadata, MetadataCommand, Package, TargetKind,
};
use rustc_hash::FxHashSet;
use same_file::Handle;
use serde::{Deserialize, Serialize};

use super::{
    cli::{Invocation, Subcommand, worker_env},
    daemon::{self, Checkout, WorkerConfig},
    plan::Plan,
    script,
    units::{self, Record, stamp_now, stamp_of},
    wrapper,
};

/// The toolchain whose layouts and symbol names this rustd has: `harness/toolchain`.
const TOOLCHAIN: &str = include_str!("../../../../harness/toolchain");

/// The names cargo reads its configuration from in `.cargo` of a directory.
const CONFIGS: [&str; 2] = [".cargo/config.toml", ".cargo/config"];

/// The names rustup reads the toolchain of a directory from.
const TOOLCHAIN_FILES: [&str; 2] = ["rust-toolchain.toml", "rust-toolchain"];

/// The state of one build of a checkout: one command with one set of flags.
#[derive(Serialize, Deserialize)]
pub(crate) struct State {
    /// The version of the rustd that wrote it. Another one reads nothing of it.
    version: String,
    pub(crate) key: String,
    pub(crate) root: PathBuf,
    /// When the cargo build this is the state of started.
    built_at: u128,
    /// The profile, as cargo's `Finished` line names it.
    pub(crate) profile: String,
    pub(crate) programs: Vec<Program>,
    guards: Vec<Guard>,
    /// What the daemon is asked for: the programs that are not unit tests, and the ones
    /// that are.
    pub(crate) checkouts: Vec<Checkout>,
    /// Why every build of this is cargo's, when rustd has no part in it.
    pub(crate) refusal: Option<String>,
    /// Whether a library that was built has doc tests.
    pub(crate) doc_tests: bool,
    pub(crate) counts: Counts,
}

/// How many builds the daemon served and how many went to cargo, by the reason.
#[derive(Serialize, Deserialize, Default)]
pub(crate) struct Counts {
    pub(crate) served: u64,
    pub(crate) cargo: BTreeMap<String, u64>,
}

/// An executable cargo built.
#[derive(Serialize, Deserialize, Clone)]
pub(crate) struct Program {
    pub(crate) name: String,
    pub(crate) kind: Kind,
    pub(crate) package: String,
    /// Whether it is built as a test: `cargo test` runs it.
    pub(crate) test: bool,
    /// The root file of the target.
    pub(crate) source: PathBuf,
    /// The directory of the package, which a test runs in.
    pub(crate) package_dir: PathBuf,
    /// Where cargo has the executable.
    pub(crate) executable: PathBuf,
    /// A hard link to the executable rustc linked, to give cargo back before it builds.
    cargos: PathBuf,
    /// Where it is among the programs of its checkout in `checkouts`.
    pub(crate) asked_as: Option<(usize, usize)>,
    /// What cargo sets for the program when it runs it.
    pub(crate) env: Vec<(String, String)>,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) enum Kind {
    Lib,
    Bin,
    Example,
    Test,
    Bench,
}

impl Kind {
    fn of(kinds: &[TargetKind]) -> Kind {
        match kinds.first() {
            Some(TargetKind::Bin) => Kind::Bin,
            Some(TargetKind::Example) => Kind::Example,
            Some(TargetKind::Test) => Kind::Test,
            Some(TargetKind::Bench) => Kind::Bench,
            _ => Kind::Lib,
        }
    }
}

/// Something cargo acts on that is not source the daemon compiles. When one does not hold
/// the build is cargo's.
#[derive(Serialize, Deserialize)]
enum Guard {
    /// Files that are to be as they were when cargo built: there or not, and not written
    /// since.
    Files {
        what: String,
        files: Vec<(PathBuf, bool)>,
    },
    /// A directory with nothing in it written since cargo built.
    Tree {
        what: String,
        dir: PathBuf,
    },
    Env {
        name: String,
        value: Option<String>,
    },
    /// A file cargo writes itself while it builds, `Cargo.lock`: as cargo left it, by when
    /// it was last written, or not there.
    AsLeft {
        file: PathBuf,
        written: Option<u128>,
    },
    /// A build script that cargo runs again when one of `reruns_for` does not hold, and
    /// that the daemon then runs itself: the build is cargo's when the script prints or
    /// writes something else than it did.
    Script {
        package: String,
        out_dir: PathBuf,
        reruns_for: Vec<Guard>,
        checked_at: u128,
    },
}

impl Guard {
    /// What changed, when something did.
    fn broken(&mut self, built_at: u128) -> Option<String> {
        match self {
            Guard::Script { package, out_dir, reruns_for, checked_at } => {
                let since = built_at.max(*checked_at);
                reruns_for.iter_mut().find_map(|it| it.broken(since))?;
                let now = stamp_now();
                match script::rerun_is_same(out_dir) {
                    Ok(()) => {
                        *checked_at = now;
                        None
                    }
                    Err(what) => Some(format!("the build script of `{package}` {what}")),
                }
            }
            Guard::Files { what, files } => {
                let changed = files.iter().find(|(file, was_there)| {
                    match std::fs::metadata(file).and_then(|it| it.modified()) {
                        Ok(written) => !was_there || stamp_of(written) > built_at,
                        Err(_) => *was_there,
                    }
                })?;
                Some(format!("{what} {} changed", changed.0.display()))
            }
            Guard::Tree { what, dir } => {
                let changed = newer_in(dir, built_at)?;
                Some(format!("{what} and {} changed", changed.display()))
            }
            Guard::AsLeft { file, written } => {
                (written_at(file) != *written).then(|| format!("{} changed", file.display()))
            }
            Guard::Env { name, value } => (std::env::var(&*name).ok() != *value)
                .then(|| format!("the environment variable {name} changed")),
        }
    }

    fn files(what: &str, files: impl IntoIterator<Item = PathBuf>) -> Guard {
        let files = files.into_iter().map(|it| {
            let there = it.exists();
            (it, there)
        });
        Guard::Files { what: what.to_owned(), files: files.collect() }
    }
}

/// When the file was last written, as a stamp. `None` when it is not there.
fn written_at(file: &Path) -> Option<u128> {
    std::fs::metadata(file).and_then(|it| it.modified()).ok().map(stamp_of)
}

/// A file below `dir` that was written after `since`. What cargo and git keep there is not
/// the package's.
fn newer_in(dir: &Path, since: u128) -> Option<PathBuf> {
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let (path, name) = (entry.path(), entry.file_name());
        let Ok(kind) = entry.file_type() else { continue };
        if kind.is_dir() {
            if name != "target" && name != ".git" {
                if let Some(found) = newer_in(&path, since) {
                    return Some(found);
                }
            }
        } else if entry.metadata().and_then(|it| it.modified()).is_ok_and(|it| stamp_of(it) > since)
        {
            return Some(path);
        }
    }
    None
}

/// What cargo said while it built.
pub(crate) struct Built {
    /// When cargo started.
    started: u128,
    artifacts: Vec<Artifact>,
    scripts: Vec<BuildScript>,
}

impl Built {
    /// Runs cargo on what `asked` builds, with rustd as the wrapper of the workspace's
    /// crates. Progress and diagnostics are cargo's own. `Err` is cargo's exit status when
    /// it failed.
    pub(crate) fn by_cargo(asked: &Invocation) -> anyhow::Result<Result<Built, i32>> {
        let started = stamp_now();
        let mut cargo = Command::new(cargo_path());
        match asked.subcommand {
            Subcommand::Build | Subcommand::Run => cargo.arg("build"),
            Subcommand::Test => {
                cargo.args(["test", "--no-run"]);
                if asked.has_ci_test_flags() && !asked.flags.selects_targets {
                    cargo.args(["--lib", "--tests"]);
                }
                &mut cargo
            }
        };
        cargo.args(&asked.build_args).arg("--message-format=json-render-diagnostics");
        cargo.env("RUSTC_WORKSPACE_WRAPPER", std::env::current_exe()?);
        if std::env::var_os("RUSTC_WRAPPER").is_none() {
            cargo.env("CARGO_BUILD_RUSTC_WRAPPER", "");
            cargo.env("RUSTC_WRAPPER", "");
        }
        cargo.env(wrapper::ENABLED, "1");
        cargo.stdout(Stdio::piped());
        let mut cargo = cargo.spawn().context("running cargo")?;
        let said = cargo.stdout.take().context("cargo has no output")?;
        let mut built = Built { started, artifacts: Vec::new(), scripts: Vec::new() };
        for message in Message::parse_stream(BufReader::new(said)) {
            match message? {
                Message::CompilerArtifact(artifact) => built.artifacts.push(artifact),
                Message::BuildScriptExecuted(script) => built.scripts.push(script),
                Message::TextLine(line) => println!("{line}"),
                _ => {}
            }
        }
        let status = cargo.wait()?;
        Ok(if status.success() { Ok(built) } else { Err(status.code().unwrap_or(101)) })
    }
}

impl State {
    fn path(key: &str) -> anyhow::Result<PathBuf> {
        Ok(daemon::home()?.join("states").join(format!("{key}.json")))
    }

    /// The state of the build `key`, when this rustd wrote one and its files are there.
    pub(crate) fn read(key: &str) -> Option<State> {
        let text = std::fs::read_to_string(State::path(key).ok()?).ok()?;
        let state: State = serde_json::from_str(&text).ok()?;
        let whole = state.programs.iter().all(|it| it.cargos.exists());
        (state.version == daemon::version() && whole).then_some(state)
    }

    pub(crate) fn write(&self) -> anyhow::Result<()> {
        let path = State::path(&self.key)?;
        std::fs::create_dir_all(path.parent().context("a state without a directory")?)?;
        let new = path.with_extension(format!("{}.new", std::process::id()));
        std::fs::write(&new, serde_json::to_vec(self)?)?;
        std::fs::rename(&new, &path)?;
        let kept: Vec<(&Path, &Path)> =
            self.programs.iter().map(|it| (&*it.executable, &*it.cargos)).collect();
        Ok(std::fs::write(State::kept_path(&self.key)?, serde_json::to_vec(&kept)?)?)
    }

    /// Every state this rustd wrote for the user.
    pub(crate) fn all() -> Vec<State> {
        let dir = daemon::home().map(|it| it.join("states"));
        let entries = dir.ok().and_then(|it| std::fs::read_dir(it).ok()).into_iter().flatten();
        let keys = entries.flatten().filter_map(|it| {
            let name = it.file_name();
            let key = name.to_str()?.strip_suffix(".json")?;
            (!key.contains('.')).then(|| key.to_owned())
        });
        keys.filter_map(|it| State::read(&it)).collect()
    }

    /// Why the next build is cargo's: something cargo acts on changed since it built.
    pub(crate) fn broken_guard(&mut self) -> Option<String> {
        self.guards.iter_mut().find_map(|it| it.broken(self.built_at))
    }

    /// Counts a build that went to cargo.
    pub(crate) fn count_cargo(&mut self, reason: &str) {
        *self.counts.cargo.entry(reason.to_owned()).or_default() += 1;
    }

    /// The executables of the build `key` and the hard link kept to what rustc linked of
    /// each. A file of its own, in a shape that stays: it is read when the state can't be.
    fn kept_path(key: &str) -> anyhow::Result<PathBuf> {
        Ok(daemon::home()?.join("states").join(format!("{key}.kept.json")))
    }

    /// Puts the executables rustc linked back where cargo has them, for cargo to find what
    /// it built. One whose link is gone is deleted, and cargo links it again.
    pub(crate) fn give_back(key: &str) {
        let kept = State::kept_path(key).ok().and_then(|it| std::fs::read(it).ok());
        let kept: Vec<(PathBuf, PathBuf)> =
            kept.and_then(|it| serde_json::from_slice(&it).ok()).unwrap_or_default();
        for (executable, cargos) in kept {
            // Whatever goes wrong here, cargo finds a file that is not its own or none,
            // and builds the program again.
            let _ = match same_file::is_same_file(&cargos, &executable) {
                Ok(true) => Ok(()),
                Ok(false) => place(&cargos, &executable),
                Err(_) => std::fs::remove_file(&executable),
            };
        }
    }

    /// Puts the executable the daemon built of each program where cargo has the program.
    pub(crate) fn install(&self) -> anyhow::Result<()> {
        for program in &self.programs {
            let Some((checkout, at)) = program.asked_as else { continue };
            let built = &self.checkouts[checkout].programs[at].binary;
            if !same_file::is_same_file(built, &program.executable).unwrap_or(false) {
                place(built, &program.executable)
                    .with_context(|| format!("installing {}", program.executable.display()))?;
            }
        }
        Ok(())
    }

    /// The state after cargo built `asked` in `cwd`: what it built, what the daemon can
    /// build of it, and what the next build has to check.
    pub(crate) fn after(
        asked: &Invocation,
        cwd: &Path,
        key: &str,
        built: Built,
        counts: Counts,
    ) -> anyhow::Result<State> {
        let metadata = metadata(asked)?;
        let root = metadata.workspace_root.clone().into_std_path_buf();
        let dir = metadata.target_directory.as_std_path().join("rustd").join(key);
        let members: FxHashSet<_> = metadata.workspace_members.iter().collect();
        let artifacts: Vec<&Artifact> =
            built.artifacts.iter().filter(|it| members.contains(&it.package_id)).collect();
        let records = records_of(&artifacts)?;
        let used = used_by(&artifacts, &records);

        let mut state = State {
            version: daemon::version(),
            key: key.to_owned(),
            root: root.clone(),
            built_at: built.started,
            profile: if asked.subcommand == Subcommand::Test { "test" } else { "dev" }.to_owned(),
            programs: Vec::new(),
            guards: Vec::new(),
            checkouts: Vec::new(),
            refusal: None,
            doc_tests: false,
            counts,
        };
        let project = match project(asked, cwd, &metadata) {
            Ok(project) => project,
            Err(why) => {
                state.refusal = Some(why);
                root.clone()
            }
        };
        if state.refusal.is_none() {
            state.refusal = other_toolchain();
        }
        let config = |unit_test: bool| WorkerConfig {
            repository: repository(&root),
            project: project.strip_prefix(&root).unwrap_or(Path::new("")).to_owned(),
            cfg_test: unit_test,
            dev_deps: asked.subcommand == Subcommand::Test,
            features: asked.flags.features.clone(),
            no_default_features: asked.flags.no_default_features,
            all_features: asked.flags.all_features,
            workspace: asked.flags.workspace,
            packages: asked.flags.packages.clone(),
            exclude: asked.flags.excluded_packages.clone(),
            target: asked.target(),
            env: worker_env(),
        };
        let checkout = |unit_test: bool| Checkout {
            key: format!("{key}{}", if unit_test { "-unit" } else { "" }),
            generation: 0,
            built_at: built.started,
            config: config(unit_test),
            root: root.clone(),
            programs: Vec::new(),
            sources: Vec::new(),
        };
        let mut checkouts = [checkout(false), checkout(true)];
        let executables = artifacts.iter().filter(|it| !it.target.is_custom_build());
        for artifact in executables {
            let Some(executable) = &artifact.executable else { continue };
            let package = metadata.packages.iter().find(|it| it.id == artifact.package_id);
            let package = package.context("cargo built a package it does not list")?;
            let kind = Kind::of(&artifact.target.kind);
            let test = artifact.profile.test;
            let unit_test = test && !matches!(kind, Kind::Test | Kind::Bench);
            let name = artifact.target.name.clone();
            let unit = Record::of_artifact(
                &used,
                executable.as_std_path(),
                &name,
                artifact.target.src_path.as_std_path(),
                test,
                &artifact.features,
            );
            // Named as cargo names the unit, the same in every build of it.
            let files = dir.join("programs").join(unit.map_or(&name, |it| &it.id));
            std::fs::create_dir_all(&files)?;
            let cargos = files.join("cargo");
            place(executable.as_std_path(), &cargos)?;
            let plan = match unit {
                Some(unit) => Plan::of(unit, &used, &files, unit_test)?,
                None => Err(format!(
                    "cargo built `{name}` before rustd recorded what it is linked from; \
                     `cargo clean -p {}` has it built again",
                    package.name
                )),
            };
            let asked_as = match plan {
                Ok(plan) => {
                    let _ = std::fs::copy(executable.as_std_path(), &plan.binary);
                    let checkout = &mut checkouts[usize::from(unit_test)];
                    checkout.generation = checkout.generation.max(plan.stamp);
                    checkout.programs.push(plan);
                    Some((usize::from(unit_test), checkout.programs.len() - 1))
                }
                Err(why) => {
                    state.refusal.get_or_insert(why);
                    None
                }
            };
            state.programs.push(Program {
                name,
                kind,
                package: package.name.to_string(),
                test,
                source: artifact.target.src_path.clone().into_std_path_buf(),
                package_dir: package_dir(package),
                executable: executable.clone().into_std_path_buf(),
                cargos,
                asked_as,
                env: run_env(package, &built.scripts),
            });
        }
        state
            .programs
            .sort_by(|a, b| (&a.package, a.kind, &a.name).cmp(&(&b.package, b.kind, &b.name)));
        for checkout in &mut checkouts {
            let sources = checkout.programs.iter().flat_map(|plan| {
                let compiled = used.iter().filter(|it| plan.crates.contains(&it.source));
                compiled.flat_map(|it| it.inputs().files)
            });
            // What a build script wrote is cargo's, written while it built.
            let target_dir = metadata.target_directory.as_std_path();
            let sources: FxHashSet<PathBuf> =
                sources.filter(|it| !it.starts_with(target_dir)).collect();
            checkout.sources = sources.into_iter().collect();
        }
        state.checkouts = checkouts.into_iter().collect();
        // Cargo runs the doc tests of the packages it runs the tests of.
        let tested: FxHashSet<_> =
            artifacts.iter().filter(|it| it.profile.test).map(|it| &it.package_id).collect();
        let documented = |it: &&Package| it.targets.iter().any(|it| it.is_lib() && it.doctest);
        state.doc_tests =
            metadata.packages.iter().filter(|it| tested.contains(&it.id)).any(|it| documented(&it));
        state.guards = guards(cwd, &metadata, &used, &built.scripts, &state.checkouts);
        let kept: Vec<PathBuf> = State::all()
            .iter()
            .chain([&state])
            .flat_map(|it| it.checkouts.iter().flat_map(|it| &it.programs))
            .flat_map(|it| it.copies.clone())
            .collect();
        for dir in record_dirs(&artifacts) {
            units::collect_garbage(&dir, &kept)?;
        }
        Ok(state)
    }
}

/// Has `to` be the file `from` is, under a second name: what was at `to` stays as it is
/// for whoever runs it.
fn place(from: &Path, to: &Path) -> std::io::Result<()> {
    let mut new = to.as_os_str().to_owned();
    new.push(format!(".{}.new", std::process::id()));
    let _ = std::fs::remove_file(&new);
    if cfg!(target_os = "macos") || std::fs::hard_link(from, &new).is_err() {
        // On macOS, `std::fs::copy` uses `fclonefileat` (CoW clone with a new inode) so the
        // kernel's vnode code-signing cache is not invalidated; on another filesystem, copy too.
        std::fs::copy(from, &new)?;
    }
    std::fs::rename(&new, to)
}

/// The cargo to run: the one that ran this command, or the one on the path.
pub(crate) fn cargo_path() -> PathBuf {
    std::env::var_os("CARGO").map_or_else(|| PathBuf::from("cargo"), PathBuf::from)
}

fn metadata(asked: &Invocation) -> anyhow::Result<Metadata> {
    let mut command = MetadataCommand::new();
    command.cargo_path(cargo_path()).no_deps();
    if let Some(manifest) = &asked.flags.manifest_path {
        command.manifest_path(manifest);
    }
    command.exec().context("running `cargo metadata`")
}

fn package_dir(package: &Package) -> PathBuf {
    package.manifest_path.parent().map(|it| it.as_std_path().to_owned()).unwrap_or_default()
}

/// The directories the records of what cargo just built are in.
fn record_dirs(artifacts: &[&Artifact]) -> FxHashSet<PathBuf> {
    let files = artifacts.iter().flat_map(|it| &it.filenames);
    files.filter_map(|it| it.parent()).map(|it| Record::dir_for(it.as_std_path())).collect()
}

fn records_of(artifacts: &[&Artifact]) -> anyhow::Result<Vec<Record>> {
    let mut records = Vec::new();
    for dir in record_dirs(artifacts) {
        records.extend(Record::all(&dir)?);
    }
    Ok(records)
}

/// The records of the units of this build: a directory has those of other builds too. Cargo
/// names a program by its hard link or copy of what rustc wrote.
fn used_by(artifacts: &[&Artifact], records: &[Record]) -> Vec<Record> {
    let files = artifacts.iter().flat_map(|it| &it.filenames);
    let built: FxHashSet<Handle> = files.filter_map(|it| Handle::from_path(it).ok()).collect();
    let is_built = |file: &PathBuf| Handle::from_path(file).is_ok_and(|it| built.contains(&it));
    let mut used: Vec<Record> =
        records.iter().filter(|it| it.outputs().iter().any(is_built)).cloned().collect();
    for artifact in artifacts {
        let Some(executable) = &artifact.executable else { continue };
        if let Some(record) = Record::of_artifact(
            records,
            executable.as_std_path(),
            &artifact.target.name,
            artifact.target.src_path.as_std_path(),
            artifact.profile.test,
            &artifact.features,
        ) && !used.iter().any(|u| u.id == record.id)
        {
            used.push(record.clone());
        }
    }
    used
}

/// The directory of the package the build is of, whose features the daemon loads the
/// workspace with. `Err` is why the selection is one it does not load.
fn project(asked: &Invocation, cwd: &Path, metadata: &Metadata) -> Result<PathBuf, String> {
    let root = metadata.workspace_root.as_std_path();
    for named in &asked.flags.packages {
        let name = named.split('@').next().unwrap_or(named);
        if !metadata.workspace_packages().iter().any(|it| it.name.as_str() == name) {
            return Err(format!("`-p {named}` is not a package of the workspace"));
        }
    }
    if asked.flags.packages.len() == 1 && !asked.flags.workspace && !asked.flags.excludes {
        let named = &asked.flags.packages[0];
        let name = named.split('@').next().unwrap_or(named);
        let package = metadata.workspace_packages().into_iter().find(|it| it.name.as_str() == name);
        return package
            .map(package_dir)
            .ok_or(format!("`-p {named}` is not a package of the workspace"));
    }
    if asked.flags.workspace || asked.flags.packages.len() > 1 || asked.flags.excludes {
        return Ok(root.to_owned());
    }
    let start = match &asked.flags.manifest_path {
        Some(manifest) => cwd.join(manifest).parent().map(Path::to_owned).unwrap_or_default(),
        None => cwd.to_owned(),
    };
    let nearest = start.ancestors().find(|it| it.join("Cargo.toml").exists());
    nearest.map(Path::to_owned).ok_or("no Cargo.toml here".to_owned())
}

/// Why the toolchain cargo builds with is not one rustd compiles for, when it is not: it
/// is another release than the one rustd has the layouts of, or the source of its standard
/// library, which the daemon compiles from, is not installed.
fn other_toolchain() -> Option<String> {
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let said = |flag: &str| {
        let output = Command::new(&rustc).arg(flag).output().ok()?;
        Some(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    };
    let version = said("--version")?;
    let version = version.split_whitespace().nth(1).unwrap_or_default();
    let ours = TOOLCHAIN.trim();
    if std::env::var_os("RUSTD_STRICT_TOOLCHAIN").is_some() && version != ours {
        return Some(format!("rustc {version} is not the {ours} this rustd compiles for"));
    }
    let library = Path::new(&said("--print=sysroot")?).join("lib/rustlib/src/rust/library");
    (!library.exists()).then(|| {
        "the toolchain has no source of the standard library, \
         `rustup component add rust-src` installs it"
            .to_owned()
    })
}

/// What the checkouts of one repository share: the repository's own directory, which is
/// `.git` in the first checkout and is named by the `.git` file of a worktree. A directory
/// that is no checkout is its own.
fn repository(root: &Path) -> PathBuf {
    let git = root.join(".git");
    if git.is_dir() {
        return git;
    }
    let named = std::fs::read_to_string(&git).ok();
    let common = named.as_deref().and_then(|it| {
        // `gitdir: /repo/.git/worktrees/name`
        let dir = Path::new(it.strip_prefix("gitdir:")?.trim());
        dir.ancestors().find(|it| it.file_name().is_some_and(|it| it == "worktrees"))?.parent()
    });
    common.map_or_else(|| root.to_owned(), Path::to_owned)
}

/// The variables cargo sets for a program of `package` when it runs it.
fn run_env(package: &Package, scripts: &[BuildScript]) -> Vec<(String, String)> {
    let version = &package.version;
    let text = |it: &Option<String>| it.clone().unwrap_or_default();
    let mut env = vec![
        ("CARGO".to_owned(), cargo_path().display().to_string()),
        ("CARGO_MANIFEST_DIR".to_owned(), package_dir(package).display().to_string()),
        ("CARGO_MANIFEST_PATH".to_owned(), package.manifest_path.to_string()),
        ("CARGO_PKG_NAME".to_owned(), package.name.to_string()),
        ("CARGO_PKG_VERSION".to_owned(), version.to_string()),
        ("CARGO_PKG_VERSION_MAJOR".to_owned(), version.major.to_string()),
        ("CARGO_PKG_VERSION_MINOR".to_owned(), version.minor.to_string()),
        ("CARGO_PKG_VERSION_PATCH".to_owned(), version.patch.to_string()),
        ("CARGO_PKG_VERSION_PRE".to_owned(), version.pre.to_string()),
        ("CARGO_PKG_AUTHORS".to_owned(), package.authors.join(":")),
        ("CARGO_PKG_DESCRIPTION".to_owned(), text(&package.description)),
        ("CARGO_PKG_HOMEPAGE".to_owned(), text(&package.homepage)),
        ("CARGO_PKG_REPOSITORY".to_owned(), text(&package.repository)),
        ("CARGO_PKG_LICENSE".to_owned(), text(&package.license)),
        (
            "CARGO_PKG_LICENSE_FILE".to_owned(),
            package.license_file.as_ref().map(|it| it.to_string()).unwrap_or_default(),
        ),
        (
            "CARGO_PKG_RUST_VERSION".to_owned(),
            package.rust_version.as_ref().map(|it| it.to_string()).unwrap_or_default(),
        ),
        (
            "CARGO_PKG_README".to_owned(),
            package.readme.as_ref().map(|it| it.to_string()).unwrap_or_default(),
        ),
    ];
    for script in scripts.iter().filter(|it| it.package_id == package.id) {
        env.push(("OUT_DIR".to_owned(), script.out_dir.to_string()));
        env.extend(script.env.iter().cloned());
    }
    env
}

/// Everything a change to which makes the next build cargo's.
fn guards(
    cwd: &Path,
    metadata: &Metadata,
    used: &[Record],
    scripts: &[BuildScript],
    checkouts: &[Checkout],
) -> Vec<Guard> {
    let root = metadata.workspace_root.as_std_path();
    let manifests = metadata
        .workspace_packages()
        .into_iter()
        .map(|it| it.manifest_path.clone().into_std_path_buf());
    let mut guards = vec![
        Guard::files("the manifest", manifests.chain([root.join("Cargo.toml")])),
        Guard::AsLeft {
            written: written_at(&root.join("Cargo.lock")),
            file: root.join("Cargo.lock"),
        },
        Guard::files(
            "cargo's configuration",
            named_above(cwd, &CONFIGS).chain(cargo_home_configs()),
        ),
        Guard::files("the toolchain file", named_above(cwd, &TOOLCHAIN_FILES)),
    ];
    let in_build = runs_in_build(used);
    let sources = in_build.iter().flat_map(|it| it.inputs().files);
    guards.push(Guard::files("what runs during the build:", sources));
    // What cargo or a build script sets for rustc changes with the manifest or the script.
    let set_for_rustc = |name: &str| {
        name.starts_with("CARGO_")
            || name == "OUT_DIR"
            || scripts.iter().any(|it| it.env.iter().any(|(set, _)| set == name))
    };
    for unit in used {
        let env = unit.inputs().env.into_iter().filter(|(name, _)| !set_for_rustc(name));
        guards.extend(env.map(|(name, value)| Guard::Env { name, value }));
    }
    let members: FxHashSet<_> = metadata.workspace_members.iter().collect();
    for script in scripts.iter().filter(|it| members.contains(&it.package_id)) {
        let package = metadata.packages.iter().find(|it| it.id == script.package_id);
        guards.extend(package.map(|it| rerun_guards(it, script)).into_iter().flatten());
    }
    let left = checkouts.iter().flat_map(|it| &it.programs).flat_map(|it| it.left_to_rustc.clone());
    guards.push(Guard::files("a library its unit tests are linked with as rustc built it:", left));
    guards
}

/// The files named one of `names` in `dir` and in every directory above it, there or not.
fn named_above<'a>(dir: &'a Path, names: &'a [&str]) -> impl Iterator<Item = PathBuf> + 'a {
    dir.ancestors().flat_map(move |dir| names.iter().map(move |name| dir.join(name)))
}

fn cargo_home_configs() -> Vec<PathBuf> {
    let home = std::env::var_os("CARGO_HOME").map(PathBuf::from);
    let home = home.or_else(|| Some(Path::new(&std::env::var_os("HOME")?).join(".cargo")));
    let in_home = |home: PathBuf| ["config.toml", "config"].map(|it| home.join(it));
    home.map(in_home).into_iter().flatten().collect()
}

/// The units that run during the build, and the libraries of the workspace they are built
/// against, and the ones those are.
fn runs_in_build(used: &[Record]) -> Vec<&Record> {
    let mut found: Vec<&Record> = used.iter().filter(|it| it.runs_in_build()).collect();
    let mut at = 0;
    while let Some(unit) = found.get(at).copied() {
        at += 1;
        // A library is named by its metadata or by its rlib.
        let externs: FxHashSet<PathBuf> = unit.externs().map(|it| it.with_extension("")).collect();
        for other in used {
            let named = other.outputs().iter().any(|it| externs.contains(&it.with_extension("")));
            if named && !found.iter().any(|it| it.id == other.id) {
                found.push(other);
            }
        }
    }
    found
}

/// What the build script of `package` said it reruns for. One that said nothing reruns for
/// any change to the package. A variable is the user's to change, and then cargo builds; for
/// a file the daemon runs the script again, when it knows how cargo ran it.
fn rerun_guards(package: &Package, script: &BuildScript) -> Vec<Guard> {
    let dir = package_dir(package);
    let what = format!("the build script of `{}` reruns for", package.name);
    let said = script.out_dir.parent().map(|it| it.join("output"));
    let said = said.and_then(|it| std::fs::read_to_string(it).ok()).unwrap_or_default();
    let directive = |line: &str, name: &str| {
        let rest = line.strip_prefix("cargo::").or_else(|| line.strip_prefix("cargo:"))?;
        rest.strip_prefix(name)?.strip_prefix('=').map(str::to_owned)
    };
    let (mut files, mut variables) = (Vec::new(), Vec::new());
    for line in said.lines() {
        if let Some(path) = directive(line, "rerun-if-changed") {
            let path = dir.join(path);
            files.push(match path.is_dir() {
                true => Guard::Tree { what: what.clone(), dir: path },
                false => Guard::files(&what, [path]),
            });
        }
        if let Some(name) = directive(line, "rerun-if-env-changed") {
            let value = std::env::var(&name).ok();
            variables.push(Guard::Env { name, value });
        }
    }
    if files.is_empty() && variables.is_empty() {
        let build_rs = package
            .targets
            .iter()
            .find(|t| t.is_custom_build())
            .map(|t| t.src_path.clone().into_std_path_buf());
        let reads_tree = build_rs
            .as_deref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .is_none_or(|src| {
                [
                    "fs::",
                    "File::",
                    "read_to_string",
                    "read_dir",
                    "walkdir",
                    "Command",
                    "include_str!",
                    "include_bytes!",
                ]
                .iter()
                .any(|pat| src.contains(pat))
            });
        if !reads_tree
            && let Some(build_rs) = build_rs
        {
            files.push(Guard::files(&what, [build_rs]));
        } else {
            let what = format!("the build script of `{}` reruns for any change,", package.name);
            files.push(Guard::Tree { what, dir });
        }
    }
    let out_dir = script.out_dir.clone().into_std_path_buf();
    if script::can_rerun(&out_dir) {
        let package = package.name.to_string();
        files = vec![Guard::Script { package, out_dir, reruns_for: files, checked_at: 0 }];
    }
    files.extend(variables);
    files
}
