//! What cargo builds: the units of a build of one package, with the features and the profile
//! of each. `cargo metadata` has neither as a build of a single package has them.

use std::{
    collections::BTreeSet,
    path::Path,
    process::{Command, Stdio},
};

use anyhow::Context;
use cfg::{CfgAtom, CfgDiff};
use intern::{Symbol, sym};
use rustc_hash::{FxHashMap, FxHashSet};
use serde_derive::Deserialize;

/// Which build of a package the daemon stands in for, as the flags of `cargo build` say it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Selection {
    /// The name of the profile: `dev` unless `--release` or `--profile` says otherwise.
    pub(crate) profile: String,
    pub(crate) features: Vec<String>,
    pub(crate) no_default_features: bool,
    pub(crate) all_features: bool,
    /// The packages asked for with `-p`: none is what `cargo build` builds where it is run.
    pub(crate) packages: Vec<String>,
    /// The packages excluded with `--exclude`.
    pub(crate) exclude: Vec<String>,
    /// Whether every member of the workspace is built.
    pub(crate) workspace: bool,
    /// The triple built for, when it is not the host.
    pub(crate) target: Option<String>,
    /// Whether tests, examples and benchmarks are built, and so what they depend on.
    pub(crate) dev_deps: bool,
}

impl Default for Selection {
    fn default() -> Selection {
        Selection {
            profile: "dev".to_owned(),
            features: Vec::new(),
            no_default_features: false,
            all_features: false,
            packages: Vec::new(),
            exclude: Vec::new(),
            workspace: false,
            target: None,
            dev_deps: false,
        }
    }
}

/// How a package is built for the target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Built {
    pub(crate) features: BTreeSet<String>,
    pub(crate) profile: Profile,
    /// Whether the package is one cargo was asked to build, not one those depend on.
    pub(crate) primary: bool,
}

/// The settings of a profile that change what a program does.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub(crate) struct Profile {
    opt_level: String,
    lto: String,
    pub(crate) debug_assertions: bool,
    pub(crate) overflow_checks: bool,
    pub(crate) panic: PanicStrategy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum PanicStrategy {
    Unwind,
    Abort,
    ImmediateAbort,
}

impl PanicStrategy {
    fn name(self) -> &'static str {
        match self {
            PanicStrategy::Unwind => "unwind",
            PanicStrategy::Abort | PanicStrategy::ImmediateAbort => "abort",
        }
    }
}

impl Profile {
    /// The profile as rustc ends up with it once `flags` follow what cargo passes for it.
    fn with(&self, flags: &Flags) -> Profile {
        // cargo names a check only where it is not what rustc has for the level by itself, so
        // a level among the flags changes the ones cargo left unnamed.
        let unoptimized = self.opt_level == "0";
        let named_assertions =
            (self.debug_assertions != unoptimized).then_some(self.debug_assertions);
        let named_overflow =
            (self.overflow_checks != self.debug_assertions).then_some(self.overflow_checks);
        let unoptimized = std::env::var_os("RUSTD_INCREMENTAL_ONLY").is_some()
            || flags.opt_level.as_ref().map_or(unoptimized, |it| it == "0");
        let debug_assertions = flags.debug_assertions.or(named_assertions).unwrap_or(unoptimized);
        Profile {
            debug_assertions,
            overflow_checks: flags.overflow_checks.or(named_overflow).unwrap_or(debug_assertions),
            panic: flags.panic.unwrap_or(self.panic),
            ..self.clone()
        }
    }

    /// Whether rustc compiles the whole program again when it links it.
    fn links_from_bitcode(&self) -> bool {
        matches!(self.lto.as_str(), "fat" | "thin" | "true")
    }

    /// What the profile changes of the cfg `rustc --print cfg -O` reports.
    pub(crate) fn cfg(&self) -> CfgDiff {
        let flag = |name: Symbol| CfgAtom::Flag(name);
        let panic = |strategy: PanicStrategy| CfgAtom::KeyValue {
            key: sym::panic,
            value: Symbol::intern(strategy.name()),
        };
        let (mut enable, mut disable) = (Vec::new(), Vec::new());
        let mut set =
            |on: bool, atom: CfgAtom| if on { enable.push(atom) } else { disable.push(atom) };
        set(self.debug_assertions, flag(sym::debug_assertions));
        set(self.overflow_checks, flag(sym::overflow_checks));
        for strategy in [PanicStrategy::Unwind, PanicStrategy::Abort] {
            set(strategy.name() == self.panic.name(), panic(strategy));
        }
        CfgDiff::new(enable, disable)
    }
}

/// What the flags given to every rustc of a build say of what a profile has too. They come
/// after the profile's own on the command line and so win.
#[derive(Debug, Default, PartialEq, Eq)]
struct Flags {
    opt_level: Option<String>,
    debug_assertions: Option<bool>,
    overflow_checks: Option<bool>,
    panic: Option<PanicStrategy>,
    lto: bool,
}

impl Flags {
    /// The flags cargo passes in `dir` for `triple`: the first of `CARGO_ENCODED_RUSTFLAGS`,
    /// `RUSTFLAGS`, the `[target]` tables and `[build]` of the configuration that has any.
    fn of(dir: &Path, triple: &str) -> anyhow::Result<Flags> {
        if let Ok(encoded) = std::env::var("CARGO_ENCODED_RUSTFLAGS") {
            return Ok(Flags::parse(encoded.split('\u{1f}')));
        }
        if let Ok(flags) = std::env::var("RUSTFLAGS") {
            return Ok(Flags::parse(flags.split_whitespace()));
        }
        let mut command = Command::new("cargo");
        // `cargo config get` is how cargo says what its configuration files and variables
        // come to, and it is not stable yet.
        command.env("RUSTC_BOOTSTRAP", "1").current_dir(dir);
        command.args(["-Z", "unstable-options", "config", "get", "--format", "json"]);
        let output = command.stdin(Stdio::null()).output().context("running `cargo config`")?;
        anyhow::ensure!(
            output.status.success(),
            "`cargo config get` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        let config: Config =
            serde_json::from_slice(&output.stdout).context("reading cargo's configuration")?;
        let mut of_target = Vec::new();
        for (key, table) in &config.target {
            let Some(flags) = &table.rustflags else { continue };
            if key == triple {
                of_target.extend(flags.words());
            } else if key.starts_with("cfg(") {
                anyhow::bail!(
                    "rustflags of `[target.'{key}']` in cargo's configuration: rustd does not \
                     tell which targets a cfg there is of, name the triple"
                );
            }
        }
        if !of_target.is_empty() {
            return Ok(Flags::parse(of_target.into_iter()));
        }
        Ok(Flags::parse(config.build.rustflags.iter().flat_map(Words::words)))
    }

    fn parse<'a>(mut words: impl Iterator<Item = &'a str>) -> Flags {
        let mut flags = Flags::default();
        while let Some(word) = words.next() {
            let option = match word {
                "-O" => "opt-level=3",
                "-C" => words.next().unwrap_or_default(),
                _ => match word.strip_prefix("-C") {
                    Some(option) => option,
                    None => continue,
                },
            };
            let (name, value) = option.split_once('=').unwrap_or((option, ""));
            let on = match value {
                "y" | "yes" | "on" | "true" | "" => Some(true),
                "n" | "no" | "off" | "false" => Some(false),
                _ => None,
            };
            match name.replace('_', "-").as_str() {
                "opt-level" => flags.opt_level = Some(value.to_owned()),
                "debug-assertions" => flags.debug_assertions = on,
                "overflow-checks" => flags.overflow_checks = on,
                "panic" if value == "abort" => flags.panic = Some(PanicStrategy::Abort),
                "panic" if value == "unwind" => flags.panic = Some(PanicStrategy::Unwind),
                "lto" => flags.lto = on != Some(false),
                "linker-plugin-lto" => flags.lto = on != Some(false),
                _ => {}
            }
        }
        flags
    }
}

/// The part of cargo's configuration that has flags for rustc.
#[derive(Deserialize, Default)]
struct Config {
    #[serde(default)]
    build: ConfigFlags,
    #[serde(default)]
    target: FxHashMap<String, ConfigFlags>,
}

#[derive(Deserialize, Default)]
struct ConfigFlags {
    rustflags: Option<Words>,
}

/// Flags as a configuration has them: in one string, or one by one.
#[derive(Deserialize)]
#[serde(untagged)]
enum Words {
    Joined(String),
    Each(Vec<String>),
}

impl Words {
    fn words(&self) -> Box<dyn Iterator<Item = &str> + '_> {
        match self {
            Words::Joined(joined) => Box::new(joined.split_whitespace()),
            Words::Each(each) => Box::new(each.iter().map(String::as_str)),
        }
    }
}

/// The packages of a build by their id, each as it is built for the target.
pub(crate) struct Plan {
    packages: FxHashMap<String, Built>,
}

impl Plan {
    /// Asks cargo for the units of the build `selection` names of the package of `manifest`.
    pub(crate) fn of(manifest: &Path, selection: &Selection, triple: &str) -> anyhow::Result<Plan> {
        let mut command = Command::new("cargo");
        // The unit graph is the only place cargo says which features and which profile each
        // package of one build has, and it is not stable yet.
        let dir = manifest.parent().unwrap_or(Path::new("."));
        command.env("RUSTC_BOOTSTRAP", "1").current_dir(dir);
        command.args(["build", "--unit-graph", "-Z", "unstable-options"]);
        command.arg("--manifest-path").arg(manifest);
        command.arg("--profile").arg(&selection.profile);
        if let Some(target) = &selection.target {
            command.arg("--target").arg(target);
        }
        if !selection.features.is_empty() {
            command.arg("--features").arg(selection.features.join(","));
        }
        if selection.no_default_features {
            command.arg("--no-default-features");
        }
        if selection.all_features {
            command.arg("--all-features");
        }
        for package in &selection.packages {
            command.arg("--package").arg(package);
        }
        for exclude in &selection.exclude {
            command.arg("--exclude").arg(exclude);
        }
        if selection.workspace {
            command.arg("--workspace");
        }
        if selection.dev_deps {
            command.arg("--all-targets");
        }
        let output = command.stdin(Stdio::null()).output().context("running `cargo build`")?;
        anyhow::ensure!(
            output.status.success(),
            "`cargo build --unit-graph` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        let graph: UnitGraph =
            serde_json::from_slice(&output.stdout).context("reading cargo's unit graph")?;
        graph.plan(&Flags::of(dir, triple)?)
    }

    /// How the package cargo calls `id` is built, when the build has it.
    pub(crate) fn package(&self, id: &str) -> Option<&Built> {
        self.packages.get(id)
    }
}

#[derive(Deserialize)]
struct UnitGraph {
    units: Vec<Unit>,
    roots: Vec<usize>,
}

#[derive(Deserialize)]
struct Unit {
    pkg_id: String,
    target: UnitTarget,
    profile: Profile,
    features: Vec<String>,
    dependencies: Vec<UnitDependency>,
}

#[derive(Deserialize)]
struct UnitTarget {
    kind: Vec<String>,
}

#[derive(Deserialize)]
struct UnitDependency {
    index: usize,
}

impl Unit {
    /// Whether the unit runs while building: a build script or a proc macro. What it depends
    /// on is built for the host, with features and a profile of its own.
    fn runs_on_host(&self) -> bool {
        self.target.kind.iter().any(|it| it == "custom-build" || it == "proc-macro")
    }

    fn is_library(&self) -> bool {
        self.target.kind.iter().any(|it| it.ends_with("lib"))
    }
}

impl UnitGraph {
    /// The units that end up in the program: those the roots reach without going through one
    /// that runs on the host.
    fn for_target(&self) -> FxHashSet<usize> {
        let mut reached = FxHashSet::default();
        let mut queue: Vec<usize> =
            self.roots.iter().copied().filter(|&it| it < self.units.len()).collect();
        while let Some(index) = queue.pop() {
            let unit = &self.units[index];
            if unit.runs_on_host() || !reached.insert(index) {
                continue;
            }
            queue.extend(
                unit.dependencies.iter().map(|it| it.index).filter(|&it| it < self.units.len()),
            );
        }
        reached
    }

    fn plan(self, flags: &Flags) -> anyhow::Result<Plan> {
        let for_target = self.for_target();
        let from_bitcode =
            flags.lto || for_target.iter().any(|&it| self.units[it].profile.links_from_bitcode());
        anyhow::ensure!(
            !from_bitcode,
            "`lto` is on: rustc compiles the whole program again as it links it and leaves no \
             library on the link line for rustd's functions to be linked against"
        );
        // A package that is only built for the host, a proc macro, is as that build has it.
        let rank = |index: usize, unit: &Unit| (for_target.contains(&index), unit.is_library());
        let mut best: FxHashMap<&str, (usize, &Unit)> = FxHashMap::default();
        for (index, unit) in self.units.iter().enumerate() {
            let chosen = best.entry(unit.pkg_id.as_str()).or_insert((index, unit));
            if rank(index, unit) > rank(chosen.0, chosen.1) {
                *chosen = (index, unit);
            }
        }
        let roots: FxHashSet<&str> =
            self.roots.iter().filter_map(|&it| Some(self.units.get(it)?.pkg_id.as_str())).collect();
        let packages = best
            .into_iter()
            .map(|(id, (_, unit))| {
                let features = unit.features.iter().cloned().collect();
                let (profile, primary) = (unit.profile.with(flags), roots.contains(id));
                (id.to_owned(), Built { features, profile, primary })
            })
            .collect();
        Ok(Plan { packages })
    }
}
