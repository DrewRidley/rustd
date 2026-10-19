//! `cargo rustd <subcommand> [flags] [-- args]`: the flags are cargo's and go to cargo as they
//! are. The few read here decide whether rustd has a part in the build and which build of
//! the checkout it is.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use sha2::{Digest as _, Sha256};

use crate::plan::Selection;

/// The flags of cargo's build commands that take their value as the next argument.
const VALUED: &[&str] = &[
    "-p",
    "--package",
    "--exclude",
    "-j",
    "--jobs",
    "--bin",
    "--example",
    "--test",
    "--bench",
    "-F",
    "--features",
    "--profile",
    "--target",
    "--target-dir",
    "--artifact-dir",
    "--manifest-path",
    "--lockfile-path",
    "--message-format",
    "--color",
    "--config",
    "-Z",
    "-C",
    "--since",
];

/// The flags that pick targets of a package, after which cargo runs no doc tests.
const TARGET_SELECTION: &[&str] = &[
    "--lib",
    "--bins",
    "--bin",
    "--examples",
    "--example",
    "--tests",
    "--test",
    "--benches",
    "--bench",
    "--all-targets",
];

/// The flags of `cargo test` that are about running and not about building.
const RUNNING: &[&str] = &["--no-run", "--no-fail-fast"];

/// The flags of `cargo rustd test` that cargo does not have: which tests to run.
const RUSTD_ONLY: &[&str] = &["--since", "--full"];

/// The variables that change what cargo builds or what the daemon compiles with, by name
/// or by what the name starts with. Their values are part of which build this is.
const BUILD_ENV: &[&str] = &[
    "RUSTFLAGS",
    "RUSTC",
    "RUSTC_WRAPPER",
    "RUSTUP_TOOLCHAIN",
    "CARGO_ENCODED_RUSTFLAGS",
    "CARGO_TARGET_DIR",
    "CARGO_BUILD_",
    "CARGO_PROFILE_",
    "CARGO_INCREMENTAL",
    "RUSTD_BACKEND",
    "RUSTD_OPT",
    "RUSTD_EMIT",
    "RUSTD_SPLICE",
];

/// The variables a worker is started with: the ones that pick its toolchain and its backend,
/// and the ones cargo reads flags and profiles from.
const WORKER_ENV: &[&str] = &[
    "RUSTFLAGS",
    "CARGO_ENCODED_RUSTFLAGS",
    "CARGO_BUILD_",
    "CARGO_PROFILE_",
    "RUSTUP_TOOLCHAIN",
    "RUSTUP_HOME",
    "CARGO_HOME",
    "PATH",
    "RUSTD_BACKEND",
    "RUSTD_OPT",
    "RUSTD_EMIT",
    "RUSTD_SPLICE",
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Subcommand {
    Build,
    Run,
    Test,
}

impl Subcommand {
    fn named(name: &str) -> Option<Subcommand> {
        match name {
            "build" | "b" => Some(Subcommand::Build),
            "run" | "r" => Some(Subcommand::Run),
            "test" | "t" => Some(Subcommand::Test),
            _ => None,
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Subcommand::Build => "build",
            Subcommand::Run => "run",
            Subcommand::Test => "test",
        }
    }
}

/// What the user asked for.
#[derive(Debug, PartialEq)]
pub(crate) enum Asked {
    Build(Box<Invocation>),
    /// A subcommand that is cargo's alone, with its arguments.
    Cargo(Vec<String>),
    Status,
    Stop,
    Enable,
    Disable,
    Help,
}

/// A build command and its arguments, split into what is cargo's to read and what is the
/// program's.
#[derive(Debug, PartialEq)]
pub(crate) struct Invocation {
    pub(crate) subcommand: Subcommand,
    /// Everything after the subcommand, as typed.
    pub(crate) typed: Vec<String>,
    /// The flags that say what to build.
    pub(crate) build_args: Vec<String>,
    /// The words before `--` that are no flags: arguments of the program for `run`, the
    /// names of the tests to run for `test`.
    pub(crate) positional: Vec<String>,
    /// What follows `--`, for the program or the test executables.
    pub(crate) trailing: Vec<String>,
    pub(crate) flags: Flags,
}

/// The flags rustd reads.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Flags {
    pub(crate) manifest_path: Option<PathBuf>,
    pub(crate) packages: Vec<String>,
    pub(crate) workspace: bool,
    pub(crate) excludes: bool,
    pub(crate) features: Vec<String>,
    pub(crate) all_features: bool,
    pub(crate) no_default_features: bool,
    pub(crate) profile: Option<String>,
    pub(crate) targets: Vec<String>,
    pub(crate) bin: Option<String>,
    pub(crate) example: Option<String>,
    pub(crate) message_format: bool,
    pub(crate) unstable: bool,
    pub(crate) quiet: bool,
    pub(crate) no_run: bool,
    pub(crate) no_fail_fast: bool,
    pub(crate) doc: bool,
    /// Whether a flag picks targets, so that `cargo test` runs no doc tests.
    pub(crate) selects_targets: bool,
    /// `--since <commit>`: the tests to run are those the changes since the commit reach.
    pub(crate) since: Option<String>,
    /// `--full`: every test runs.
    pub(crate) full: bool,
}

impl Asked {
    /// From the arguments after `cargo rustd`.
    pub(crate) fn parse(args: Vec<String>) -> Asked {
        let Some((first, rest)) = args.split_first() else {
            return Asked::Help;
        };
        match (first.as_str(), Subcommand::named(first)) {
            ("--help" | "-h" | "help", _) => Asked::Help,
            (_, Some(subcommand)) => Asked::Build(Box::new(Invocation::parse(subcommand, rest))),
            ("status", _) => Asked::Status,
            ("stop", _) => Asked::Stop,
            ("enable", _) => Asked::Enable,
            ("disable", _) => Asked::Disable,
            _ => Asked::Cargo(args),
        }
    }
}

/// The build command a cargo was run for, from its arguments. Cargo takes its own flags
/// ahead of the subcommand as well as after it, and they are read as if they were after.
/// `None` when the command is not one that builds programs rustd has a part in, or is one
/// with a directory to change to first.
pub(crate) fn of_cargo(args: &[String]) -> Option<Invocation> {
    let mut ahead = Vec::new();
    let mut words = args.iter();
    let subcommand = loop {
        let word = words.next()?;
        if !word.starts_with('-') {
            break Subcommand::named(word)?;
        }
        if word == "-C" {
            return None;
        }
        ahead.push(word.clone());
        if VALUED.contains(&word.as_str()) {
            ahead.push(words.next()?.clone());
        }
    };
    let typed: Vec<String> = ahead.into_iter().chain(words.cloned()).collect();
    Some(Invocation::parse(subcommand, &typed))
}

impl Invocation {
    fn parse(subcommand: Subcommand, typed_all: &[String]) -> Invocation {
        let (before, after) = match typed_all.iter().position(|it| it == "--") {
            Some(at) => (&typed_all[..at], &typed_all[at + 1..]),
            None => (typed_all, &[][..]),
        };
        let mut flags = Flags::default();
        let mut build_args = Vec::new();
        let mut positional = Vec::new();
        // What cargo is handed of what was typed, when cargo builds.
        let mut typed: Vec<String> = Vec::new();
        let mut words = before.iter();
        while let Some(word) = words.next() {
            if !word.starts_with('-') {
                positional.push(word.clone());
                typed.push(word.clone());
                continue;
            }
            let (flag, joined) = match word.split_once('=') {
                Some((flag, value)) if flag.starts_with("--") => (flag, Some(value.to_owned())),
                // `-pname`, `-j8`.
                _ if !word.starts_with("--") && word.len() > 2 && VALUED.contains(&&word[..2]) => {
                    (&word[..2], Some(word[2..].to_owned()))
                }
                _ => (word.as_str(), None),
            };
            let was_joined = joined.is_some();
            let value = match joined {
                None if VALUED.contains(&flag) => words.next().cloned(),
                joined => joined,
            };
            flags.read(flag, value.as_deref());
            if RUSTD_ONLY.contains(&flag) {
                continue;
            }
            typed.push(word.clone());
            if !was_joined {
                typed.extend(value.clone());
            }
            if RUNNING.contains(&flag) {
                continue;
            }
            build_args.push(flag.to_owned());
            build_args.extend(value);
        }
        let trailing = after.to_vec();
        if before.len() < typed_all.len() {
            typed.push("--".to_owned());
            typed.extend(trailing.iter().cloned());
        }
        Invocation { subcommand, typed, build_args, positional, trailing, flags }
    }

    /// What the program or a test executable is run with: `cargo run a -- b` runs the
    /// program with `a b`, and `cargo test a -- b` the tests named like `a`, with `b`.
    pub(crate) fn program_args(&self) -> impl Iterator<Item = &String> {
        self.positional.iter().chain(&self.trailing)
    }

    /// Which build of the checkout this is: a hash of what cargo is asked to build, where,
    /// and with what in the environment.
    pub(crate) fn key(&self, cwd: &Path) -> String {
        let mut hash = Sha256::new();
        // `run` builds what `build` does.
        let builds = if self.subcommand == Subcommand::Test { "test" } else { "build" };
        hash.update(builds);
        hash.update(cwd.as_os_str().as_encoded_bytes());
        for arg in &self.build_args {
            hash.update([0]);
            hash.update(arg);
        }
        for (name, value) in build_env() {
            hash.update([1]);
            hash.update(name);
            hash.update([2]);
            hash.update(value);
        }
        hash.finalize().iter().take(8).map(|it| format!("{it:02x}")).collect()
    }

    /// Why this command is cargo's alone, when it is clear before anything is built.
    pub(crate) fn only_cargo(&self) -> Option<String> {
        let surroundings = match () {
            _ if another_wrapper() => "RUSTC_WORKSPACE_WRAPPER is set to another wrapper",
            _ if !wrapper_installed() => "`rustd-rustc` is not installed next to `rustd`",
            _ => return self.asks_for_cargo(),
        };
        Some(surroundings.to_owned())
    }

    /// Why what was typed is cargo's alone, wherever it is typed.
    fn asks_for_cargo(&self) -> Option<String> {
        let flags = &self.flags;
        let reason = match () {
            _ if flags.message_format => "--message-format is cargo's to answer",
            _ if flags.unstable => "unstable cargo flags",
            _ if flags.excludes => "--exclude is not loaded by the daemon yet",
            _ if flags.targets.len() > 1 => "several targets in one build",
            _ => return self.target().and_then(|it| unsupported_target(&it)),
        };
        Some(reason.to_owned())
    }

    /// The profile, as cargo's `Finished` line names it.
    pub(crate) fn profile(&self) -> &str {
        let of_command = if self.subcommand == Subcommand::Test { "test" } else { "dev" };
        self.flags.profile.as_deref().unwrap_or(of_command)
    }

    /// Which build of the package the command is, as the daemon plans it. With no package
    /// named it is the one of the directory the worker is started in.
    pub(crate) fn selection(&self) -> Selection {
        Selection {
            profile: self.flags.profile.clone().unwrap_or_else(|| "dev".to_owned()),
            features: self.flags.features.clone(),
            no_default_features: self.flags.no_default_features,
            all_features: self.flags.all_features,
            packages: self.flags.packages.clone(),
            workspace: self.flags.workspace,
            target: self.target(),
            dev_deps: self.subcommand == Subcommand::Test,
        }
    }

    /// The triple cargo builds for, when it is told one.
    pub(crate) fn target(&self) -> Option<String> {
        let told = std::env::var("CARGO_BUILD_TARGET").ok().filter(|it| !it.is_empty());
        self.flags.targets.first().cloned().or(told)
    }
}

impl Flags {
    fn read(&mut self, flag: &str, value: Option<&str>) {
        let owned = || value.map(str::to_owned);
        self.selects_targets |= TARGET_SELECTION.contains(&flag);
        match flag {
            "--manifest-path" => self.manifest_path = value.map(PathBuf::from),
            "-p" | "--package" => self.packages.extend(owned()),
            "--workspace" | "--all" => self.workspace = true,
            "--exclude" => self.excludes = true,
            "-F" | "--features" => {
                let features = value.unwrap_or_default().split([',', ' ']);
                self.features.extend(features.filter(|it| !it.is_empty()).map(str::to_owned));
            }
            "--all-features" => self.all_features = true,
            "--no-default-features" => self.no_default_features = true,
            "-r" | "--release" => self.profile = Some("release".to_owned()),
            "--profile" => self.profile = owned(),
            "--target" => self.targets.extend(owned()),
            "--bin" => self.bin = owned(),
            "--example" => self.example = owned(),
            "--message-format" | "--build-plan" | "--unit-graph" => self.message_format = true,
            "-Z" => self.unstable = true,
            "-q" | "--quiet" => self.quiet = true,
            "--no-run" => self.no_run = true,
            "--no-fail-fast" => self.no_fail_fast = true,
            "--doc" => self.doc = true,
            "--since" => self.since = owned(),
            "--full" => self.full = true,
            _ => {}
        }
    }
}

/// Whether the environment names a wrapper for the crates of the workspace that is not
/// rustd's.
fn another_wrapper() -> bool {
    let ours = super::switch::wrapper_path().ok();
    std::env::var_os(super::switch::VARIABLE)
        .is_some_and(|it| !it.is_empty() && Some(Path::new(&it)) != ours.as_deref())
}

/// Whether the wrapper cargo is to run in place of rustc is where rustd looks for it.
fn wrapper_installed() -> bool {
    super::switch::wrapper_path().is_ok_and(|it| it.is_file())
}

/// Why rustd does not build for `triple`, when it does not.
fn unsupported_target(triple: &str) -> Option<String> {
    use hir_ty::mir::codegen::{ObjectFormat, Target};
    match Target::select(triple) {
        Ok(target) if matches!(target.object_format, ObjectFormat::Elf | ObjectFormat::MachO) => {
            None
        }
        Ok(_) => Some(format!("programs for {triple} are not linked through `cargo rustd` yet")),
        Err(why) => Some(why),
    }
}

/// The variables of this process that are part of which build it is.
pub(crate) fn build_env() -> BTreeMap<String, String> {
    env_named(BUILD_ENV)
}

/// The variables of this process that a worker for its builds runs with.
pub(crate) fn worker_env() -> BTreeMap<String, String> {
    env_named(WORKER_ENV)
}

fn env_named(names: &[&str]) -> BTreeMap<String, String> {
    let named = |name: &str| {
        names.iter().any(|it| if it.ends_with('_') { name.starts_with(it) } else { name == *it })
    };
    std::env::vars().filter(|(name, _)| named(name)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(subcommand: &str, args: &str) -> Invocation {
        let words = std::iter::once(subcommand).chain(args.split_whitespace());
        match Asked::parse(words.map(str::to_owned).collect()) {
            Asked::Build(invocation) => *invocation,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn flags_in_both_spellings() {
        let asked =
            build("build", "-p a --features=x,y -F z --target t --manifest-path m/Cargo.toml -r");
        assert_eq!(asked.flags.packages, ["a"]);
        assert_eq!(build("build", "-pa -j8").flags.packages, ["a"]);
        assert_eq!(asked.flags.features, ["x", "y", "z"]);
        assert_eq!(asked.flags.targets, ["t"]);
        assert_eq!(asked.flags.manifest_path.as_deref(), Some(Path::new("m/Cargo.toml")));
        assert_eq!(asked.flags.profile.as_deref(), Some("release"));
        assert_eq!(asked.program_args().count(), 0);
    }

    #[test]
    fn what_is_the_programs() {
        let run = build("run", "--bin a one -- --two");
        assert_eq!(run.build_args, ["--bin", "a"]);
        assert_eq!(run.program_args().collect::<Vec<_>>(), ["one", "--two"]);
        let test = build("test", "--no-fail-fast -p a filter --no-run -- --nocapture");
        assert_eq!(test.build_args, ["-p", "a"]);
        assert_eq!(
            (&*test.positional, &*test.trailing),
            (&["filter".to_owned()][..], &["--nocapture".to_owned()][..])
        );
        assert!(test.flags.no_fail_fast && test.flags.no_run);
    }

    #[test]
    fn the_flags_that_pick_tests_are_rustds() {
        let test = build("test", "--since main -p a --full -- --nocapture");
        assert_eq!(test.flags.since.as_deref(), Some("main"));
        assert!(test.flags.full);
        assert_eq!(test.build_args, ["-p", "a"]);
        assert_eq!(test.typed, ["-p", "a", "--", "--nocapture"]);
        assert!(test.positional.is_empty());
        assert_eq!(build("test", "--since=HEAD~1").flags.since.as_deref(), Some("HEAD~1"));
        assert_eq!(build("test", "-p a").typed, ["-p", "a"]);
    }

    #[test]
    fn a_flag_without_its_value_is_cargos_to_refuse() {
        let asked = build("build", "--features");
        assert_eq!(asked.build_args, ["--features"]);
        assert!(asked.flags.features.is_empty());
    }

    #[test]
    fn what_is_cargos_alone() {
        assert_eq!(Asked::parse(vec!["doc".to_owned()]), Asked::Cargo(vec!["doc".to_owned()]));
        assert_eq!(Asked::parse(Vec::new()), Asked::Help);
        assert_eq!(Asked::parse(vec!["--help".to_owned()]), Asked::Help);
        for args in ["--message-format json", "--exclude a", "--target mips-unknown-none"] {
            assert!(build("build", args).asks_for_cargo().is_some(), "{args}");
        }
        for args in
            ["--release", "--profile bench", "-p a -p b", "--all-features", "--no-default-features"]
        {
            assert_eq!(build("build", args).asks_for_cargo(), None, "{args}");
        }
        assert_eq!(build("test", "--release").profile(), "release");
        assert_eq!(build("test", "").profile(), "test");
    }

    #[test]
    fn run_is_the_build_that_build_is() {
        let cwd = Path::new("/ws");
        assert_eq!(build("run", "--bin a x -- y").key(cwd), build("build", "--bin a").key(cwd));
        assert_ne!(build("test", "--bin a").key(cwd), build("build", "--bin a").key(cwd));
        assert_ne!(build("build", "").key(cwd), build("build", "").key(Path::new("/other")));
        assert_ne!(build("build", "--features a").key(cwd), build("build", "").key(cwd));
    }
}
