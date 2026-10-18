//! `cargo rustd <subcommand> [flags] [-- args]`: the flags are cargo's and go to cargo as they
//! are. The few read here decide whether rustd has a part in the build and which build of
//! the checkout it is.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use sha2::{Digest as _, Sha256};

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
    "--explain",
    "--faults",
    "--junit",
    "--also-run",
    "--retries",
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
const RUNNING: &[&str] = &[
    "--no-run",
    "--no-fail-fast",
    "--since",
    "--explain",
    "--list-changed",
    "--backtest",
    "--faults",
    "--junit",
    "--json",
    "--also-run",
    "--retries",
    "--full",
    "--all-tests",
];

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

/// The variables a worker is started with: the ones that pick its toolchain and its backend.
const WORKER_ENV: &[&str] = &[
    "RUSTUP_TOOLCHAIN",
    "RUSTUP_HOME",
    "CARGO_HOME",
    "PATH",
    "RUSTFLAGS",
    "CARGO_ENCODED_RUSTFLAGS",
    "CARGO_BUILD_RUSTFLAGS",
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
    Build(Invocation),
    /// `cargo rustd clippy ...`: incremental Clippy using `RUSTC_WORKSPACE_WRAPPER`.
    Clippy(Vec<String>),
    /// A subcommand that is cargo's alone, with its arguments.
    Cargo(Vec<String>),
    Status,
    Stop,
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
    pub(crate) excluded_packages: Vec<String>,
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
    pub(crate) since: Option<String>,
    pub(crate) explain: Option<String>,
    pub(crate) list_changed: bool,
    pub(crate) backtest: bool,
    pub(crate) faults: Option<usize>,
    pub(crate) junit: Option<PathBuf>,
    pub(crate) json: bool,
    pub(crate) also_run: Option<PathBuf>,
    pub(crate) retries: Option<usize>,
    pub(crate) full_tests: bool,
}

impl Asked {
    /// From the arguments after `cargo rustd`.
    pub(crate) fn parse(args: Vec<String>) -> Asked {
        let Some((first, rest)) = args.split_first() else {
            return Asked::Cargo(vec!["help".to_owned()]);
        };
        match (first.as_str(), Subcommand::named(first)) {
            (_, Some(subcommand)) => Asked::Build(Invocation::parse(subcommand, rest)),
            ("clippy", _) => Asked::Clippy(rest.to_vec()),
            ("status", _) => Asked::Status,
            ("stop", _) => Asked::Stop,
            _ => Asked::Cargo(args),
        }
    }
}

impl Invocation {
    fn parse(subcommand: Subcommand, typed: &[String]) -> Invocation {
        let (before, after) = match typed.iter().position(|it| it == "--") {
            Some(at) => (&typed[..at], &typed[at + 1..]),
            None => (typed, &[][..]),
        };
        let mut flags = Flags::default();
        let mut build_args = Vec::new();
        let mut positional = Vec::new();
        let mut words = before.iter();
        while let Some(word) = words.next() {
            if !word.starts_with('-') {
                positional.push(word.clone());
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
            let value = match joined {
                None if VALUED.contains(&flag) => words.next().cloned(),
                joined => joined,
            };
            flags.read(flag, value.as_deref());
            if RUNNING.contains(&flag) {
                continue;
            }
            build_args.push(flag.to_owned());
            build_args.extend(value);
        }
        let trailing = after.to_vec();
        Invocation { subcommand, typed: typed.to_vec(), build_args, positional, trailing, flags }
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
        let flags = &self.flags;
        let set = |name: &str| std::env::var_os(name).is_some_and(|it| !it.is_empty());
        let has_cfg = |name: &str| {
            std::env::var(name).is_ok_and(|it| it.contains("--cfg"))
        };
        let reason = match () {
            _ if flags.doc => "doc tests are built by rustdoc",
            _ if flags.profile.as_deref().is_some_and(|it| it != "dev" && it != "test") => {
                "rustd builds the dev profile only"
            }
            _ if flags.message_format => "--message-format is cargo's to answer",
            _ if flags.unstable => "unstable cargo flags",
            _ if flags.targets.len() > 1 => "several targets in one build",
            _ if ["RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "CARGO_BUILD_RUSTFLAGS"]
                .iter()
                .any(|it| has_cfg(it)) =>
            {
                "RUSTFLAGS is set, and the daemon loads the workspace without it"
            }
            _ if set("RUSTC_WORKSPACE_WRAPPER") => "RUSTC_WORKSPACE_WRAPPER is set already",
            _ => return self.target().and_then(|it| unsupported_target(&it)),
        };
        Some(reason.to_owned())
    }

    /// Arguments to pass to plain `cargo` when falling back, without `rustd`-only flags.
    pub(crate) fn cargo_args(&self) -> Vec<String> {
        let mut args = vec![self.subcommand.name().to_owned()];
        args.extend(self.build_args.iter().cloned());
        if self.flags.no_run {
            args.push("--no-run".to_owned());
        }
        if self.flags.no_fail_fast {
            args.push("--no-fail-fast".to_owned());
        }
        if self.flags.doc {
            args.push("--doc".to_owned());
        }
        args.extend(self.positional.iter().cloned());
        if !self.trailing.is_empty() {
            args.push("--".to_owned());
            args.extend(self.trailing.iter().cloned());
        }
        args
    }

    /// The triple cargo builds for, when it is told one.
    pub(crate) fn target(&self) -> Option<String> {
        let told = std::env::var("CARGO_BUILD_TARGET").ok().filter(|it| !it.is_empty());
        self.flags.targets.first().cloned().or(told)
    }

    pub(crate) fn has_ci_test_flags(&self) -> bool {
        self.flags.since.is_some()
            || self.flags.explain.is_some()
            || self.flags.list_changed
            || self.flags.backtest
            || self.flags.junit.is_some()
            || self.flags.json
            || self.flags.also_run.is_some()
            || self.flags.retries.is_some()
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
            "--exclude" => {
                self.excludes = true;
                self.excluded_packages.extend(owned());
            }
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
            "--explain" => self.explain = owned(),
            "--list-changed" => self.list_changed = true,
            "--backtest" => self.backtest = true,
            "--faults" => self.faults = value.and_then(|v| v.parse().ok()),
            "--junit" => self.junit = value.map(PathBuf::from),
            "--json" => self.json = true,
            "--also-run" => self.also_run = value.map(PathBuf::from),
            "--retries" => self.retries = value.and_then(|v| v.parse().ok()),
            "--full" | "--all-tests" => self.full_tests = true,
            _ => {}
        }
    }
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
            Asked::Build(invocation) => invocation,
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
    fn a_flag_without_its_value_is_cargos_to_refuse() {
        let asked = build("build", "--features");
        assert_eq!(asked.build_args, ["--features"]);
        assert!(asked.flags.features.is_empty());
    }

    #[test]
    fn what_is_cargos_alone() {
        assert_eq!(Asked::parse(vec!["doc".to_owned()]), Asked::Cargo(vec!["doc".to_owned()]));
        assert_eq!(Asked::parse(Vec::new()), Asked::Cargo(vec!["help".to_owned()]));
        for args in [
            "--release",
            "--profile bench",
            "--message-format json",
            "--target mips-unknown-none",
        ] {
            assert!(build("build", args).only_cargo().is_some(), "{args}");
        }
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
