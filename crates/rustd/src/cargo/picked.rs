//! Which tests `cargo rustd test` runs: those an edit since the tests last all passed can
//! change the outcome of. What changed is told by the files, and which tests it reaches by
//! the daemon, which has the code loaded. Whatever can't be told runs: a whole program, or
//! every test.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use rustc_hash::FxHashSet;

use super::{
    cli::Invocation,
    client::{self, Start},
    daemon::{Checkout, Reply, Request},
    passed::{Passed, canonical, is_rust},
    say,
    state::{Kind, Program, State},
};
use crate::impact::{Change, Picked};

/// Flags of a test harness that leave which tests run as it is.
const HARMLESS: &[&str] =
    &["--nocapture", "--no-capture", "--show-output", "--quiet", "-q", "--report-time"];

/// The same, with a value in the next argument when it is not after `=`.
const HARMLESS_VALUED: &[&str] = &["--test-threads", "--color", "--format", "-Z"];

/// The files whose change has every test run: they say how everything is built.
const BUILDS_ALL: &[&str] =
    &["Cargo.toml", "Cargo.lock", "build.rs", "rust-toolchain", "rust-toolchain.toml"];

/// What the tests of a build run.
pub(crate) struct Picks {
    /// What to run of each program of the state, by its place there: `None` is all of it.
    programs: Vec<Option<Picked>>,
    /// Which program each is: its package, kind and name, which a build by cargo after
    /// the picks were made keeps.
    names: Vec<(String, Kind, String)>,
    /// The packages whose doc tests run; `None` is all that are asked for.
    pub(crate) doc_packages: Option<Vec<String>>,
    /// How long the daemon took to tell the tests, when it was asked.
    pub(crate) took: Option<std::time::Duration>,
    /// The source to keep as what passed, once everything ran and passed.
    passing: Option<Passed>,
    key: String,
}

impl Picks {
    /// Everything, as asked.
    pub(crate) fn all(state: &State, passing: Option<Passed>) -> Picks {
        let programs = vec![None; state.programs.len()];
        let (names, key) = (names(state), state.key.clone());
        Picks { programs, names, doc_packages: None, took: None, passing, key }
    }

    /// What to run of `program`: `None` is all of it, also for one not known when the picks
    /// were made.
    pub(crate) fn of(&self, program: &Program) -> Option<&Picked> {
        let name = (program.package.clone(), program.kind, program.name.clone());
        let at = self.names.iter().position(|it| *it == name)?;
        self.programs[at].as_ref()
    }

    /// Keeps the source the tests ran on as what passed: they ran and all passed. A file of
    /// `state` written since the source was looked at is not what they ran on, and nothing is
    /// kept.
    pub(crate) fn passed(self, state: &State) {
        let Some(passing) = self.passing else { return };
        if let Some(path) = passing.written_since(state) {
            let why = format!("{} was written while the tests were built or ran", path.display());
            say::fallback(&why, "the next run picks its tests from the run before");
            return;
        }
        if let Err(why) = passing.write(&self.key) {
            say::fallback(&format!("{why:#}"), "the next run of tests runs them all");
        }
    }

    /// Whether only some tests run.
    pub(crate) fn some(&self) -> bool {
        self.doc_packages.is_some() || self.programs.iter().any(Option::is_some)
    }
}

/// The files of `state` and the environment as they are before it is built, which is what
/// the tests that run are picked against and what is kept once they pass: a file written
/// after this is one the build may have read, and is not taken as passing.
pub(crate) fn looked_at(state: &State) -> Passed {
    Passed::now(state, Passed::read(&state.key).as_ref())
}

/// What the tests of `state`, which the daemon built when `by_daemon`, run, for the source
/// `now` as it was before the build; with `None`, nothing was looked at then. With
/// `unsynced`, cargo builds them and the daemon has not read the source since the last
/// build: it is asked to first.
pub(crate) fn pick(
    asked: &Invocation,
    state: &State,
    by_daemon: bool,
    unsynced: bool,
    now: Option<Passed>,
) -> Picks {
    // The tests named are the ones asked for, and not all of them ran once they did.
    if !asked.positional.is_empty() || !harmless(&asked.trailing) {
        return Picks::all(state, None);
    }
    // What cargo built the first time, with nothing looked at before, runs whole, and passes
    // nothing on.
    let Some(now) = now else { return Picks::all(state, None) };
    let before = Passed::read(&state.key);
    let all_asked = asked.flags.full || std::env::var_os("RUSTD_ALL_TESTS").is_some();
    if all_asked {
        return Picks::all(state, Some(now));
    }
    // The tests that passed did in another environment: any test may read a variable.
    if let Some(name) = before.as_ref().and_then(|it| it.env_differs(&now)) {
        let why = format!("`{name}` is not what it was when the tests last passed");
        say::fallback(&why, "every test runs");
        let keep = asked.flags.since.is_none();
        return Picks::all(state, keep.then_some(now));
    }
    let (changed, texts) = match (&asked.flags.since, &before) {
        (Some(since), _) => match Passed::at_commit(state, since, &now) {
            Ok(changed) => changed,
            Err(why) => {
                say::fallback(&format!("{why:#}"), "every test runs");
                return Picks::all(state, None);
            }
        },
        (None, None) => {
            say::fallback("no run of every test has passed yet", "every test runs");
            return Picks::all(state, Some(now));
        }
        (None, Some(before)) => {
            let changed = before.changed(&now);
            let texts =
                changed.iter().filter(|it| is_rust(it)).map(|it| (it.clone(), before.text(it)));
            let texts = texts.collect();
            (changed, texts)
        }
    };
    let keep = asked.flags.since.is_none();
    let builds_all = |path: &PathBuf| {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        BUILDS_ALL.contains(&&*name)
            || path.components().any(|it| it.as_os_str() == ".cargo") && name.starts_with("config")
    };
    if let Some(path) = changed.iter().find(|it| builds_all(it)) {
        let why = format!("{} changed", path.display());
        say::fallback(&why, "every test runs");
        return Picks::all(state, keep.then_some(now));
    }
    if let Some(why) = changed.iter().find_map(|it| builds_into_others(state, it)) {
        say::fallback(&why, "every test runs");
        return Picks::all(state, keep.then_some(now));
    }
    // What cargo built is all there is to run, and the daemon has not seen it.
    if !by_daemon {
        return Picks::all(state, keep.then_some(now));
    }
    let mut picks = Picks {
        programs: vec![None; state.programs.len()],
        names: names(state),
        doc_packages: None,
        took: None,
        passing: None,
        key: state.key.clone(),
    };
    let root = canonical(&state.root);
    let mut whole: Vec<Option<String>> = vec![None; state.programs.len()];
    let mut documented: Vec<String> = Vec::new();
    let mut sources: Vec<PathBuf> = Vec::new();
    // As `changed` names files: canonical.
    let inputs: Vec<FxHashSet<PathBuf>> = state
        .programs
        .iter()
        .map(|it| it.inputs().iter().map(|it| canonical(it)).collect())
        .collect();
    let package_dirs: Vec<PathBuf> =
        state.programs.iter().map(|it| canonical(&it.package_dir)).collect();
    for path in &changed {
        let reads: Vec<usize> =
            (0..state.programs.len()).filter(|&at| inputs[at].contains(path)).collect();
        let owners: Vec<usize> =
            (0..state.programs.len()).filter(|&at| path.starts_with(&package_dirs[at])).collect();
        for &at in reads.iter().chain(&owners) {
            let program = &state.programs[at];
            if program.kind == Kind::Lib || owners.contains(&at) {
                documented.push(program.package.clone());
            }
        }
        if is_rust(path) && !reads.is_empty() {
            sources.push(path.clone());
            continue;
        }
        let shown = path.display();
        let (programs, why) = match (reads.is_empty(), owners.is_empty()) {
            (false, _) => (reads, format!("{shown} changed, which rustc read")),
            // A test may read a file of its package as it runs, or one outside every
            // package, as a test of the workspace's data does.
            (true, false) => {
                (owners, format!("{shown} changed, in its package, which it may read"))
            }
            (true, true) if read_by_no_test(&root, path) => continue,
            (true, true) => {
                let tests = (0..state.programs.len()).filter(|&at| state.programs[at].test);
                (tests.collect(), format!("{shown} changed, which a test may read"))
            }
        };
        for at in programs {
            whole[at].get_or_insert_with(|| why.clone());
        }
    }
    // A package without the unit tests of a library is one whose doc tests rustd can't
    // tell from cargo's list; cargo refuses `--doc` for one with no library.
    let libraries =
        |name: &String| state.programs.iter().any(|it| it.kind == Kind::Lib && it.package == *name);
    documented.retain(libraries);
    documented.sort();
    documented.dedup();
    picks.doc_packages = Some(documented);
    let asked_at = std::time::Instant::now();
    let selected = match sources.is_empty() {
        true => Vec::new(),
        false => selected(state, &sources, &texts, &now, unsynced),
    };
    picks.took = (!sources.is_empty()).then(|| asked_at.elapsed());
    // An integration test runs the programs of its package, which no call of it names: it
    // runs whole when the `main` of one reaches an edit.
    let programs =
        state.programs.iter().enumerate().filter(|(_, it)| it.kind == Kind::Bin && !it.test);
    for (at, program) in programs {
        let Some(path) = changed.iter().find(|it| inputs[at].contains(*it)) else { continue };
        let said =
            program.asked_as.and_then(|(checkout, place)| selected.get(checkout)?.get(place));
        let why = match (&whole[at], said) {
            (None, Some(Picked::Program(None) | Picked::Nothing(_))) => continue,
            (None, Some(Picked::Program(Some(reaches)))) => {
                format!("it can run `{}`, whose `main` reaches {reaches}", program.name)
            }
            _ => format!("it can run `{}`, compiled from {}", program.name, path.display()),
        };
        for (at, test) in state.programs.iter().enumerate() {
            if matches!(test.kind, Kind::Test | Kind::Bench) && test.package == program.package {
                whole[at].get_or_insert_with(|| why.clone());
            }
        }
    }
    for (at, program) in state.programs.iter().enumerate() {
        let reads_source = sources.iter().any(|it| inputs[at].contains(it));
        picks.programs[at] = Some(match (whole[at].take(), program.asked_as) {
            (Some(why), _) => Picked::Whole(why),
            // What it is compiled from is not known, so neither is whether that changed.
            (None, _) if program.inputs().is_empty() && !changed.is_empty() => {
                Picked::Whole("rustd does not know what it is compiled from".to_owned())
            }
            (None, _) if !reads_source => {
                Picked::Nothing("nothing it is compiled from changed".to_owned())
            }
            (None, Some((checkout, place))) => {
                let said = selected.get(checkout).and_then(|it| it.get(place)).cloned();
                match said {
                    Some(Picked::Program(reaches)) => of_program(program.kind, reaches),
                    Some(picked) => picked,
                    None => Picked::Whole("the daemon did not say".to_owned()),
                }
            }
            (None, None) => Picked::Whole("the daemon does not build it".to_owned()),
        });
    }
    picks.passing = keep.then_some(now);
    picks
}

/// What to run of a test executable whose `main` reaches the edit `reaches`, by the kind of
/// its target: a test target with that is one without the harness, and runs whole or not
/// at all; unit tests with it are tests rustd could not find, and run as the harness lists.
fn of_program(kind: Kind, reaches: Option<String>) -> Picked {
    match (kind, reaches) {
        (Kind::Lib | Kind::Bin | Kind::Example, _) => {
            Picked::Tests { known: Vec::new(), run: Vec::new() }
        }
        (Kind::Test | Kind::Bench, Some(why)) => Picked::Whole(format!("its `main` reaches {why}")),
        (Kind::Test | Kind::Bench, None) => {
            Picked::Nothing("its `main` reaches no edited function".to_owned())
        }
    }
}

/// Whether a changed file outside every package is one no test reads: a Markdown file at the
/// root of the checkout, or a file of `.github`. `docs/cargo.md` lists these.
fn read_by_no_test(root: &Path, path: &Path) -> bool {
    let Ok(inside) = path.strip_prefix(root) else { return false };
    let markdown_at_root =
        inside.components().count() == 1 && inside.extension().is_some_and(|it| it == "md");
    markdown_at_root || inside.starts_with(".github")
}

fn names(state: &State) -> Vec<(String, Kind, String)> {
    state.programs.iter().map(|it| (it.package.clone(), it.kind, it.name.clone())).collect()
}

/// Why the change to `path` reaches code no program's dep-info names, when it does: it is
/// read by a build script, whose output any crate may include, or it is source of a crate that
/// is not compiled into the programs, as a proc macro or a build script's dependency is.
fn builds_into_others(state: &State, path: &Path) -> Option<String> {
    let read_by_rustc =
        state.programs.iter().any(|it| it.inputs().iter().any(|it| canonical(it) == path));
    if read_by_rustc && is_rust(path) {
        return None;
    }
    let (dir, manifest) = path
        .ancestors()
        .skip(1)
        .take_while(|it| it.starts_with(canonical(&state.root)))
        .find_map(|dir| Some((dir, std::fs::read_to_string(dir.join("Cargo.toml")).ok()?)))?;
    let words = |text: &str| text.split_whitespace().collect::<String>();
    let proc_macro = words(&manifest).contains("proc-macro=true");
    let script = dir.join("build.rs").exists()
        || words(&manifest).contains("build=\"") && !words(&manifest).contains("build=false");
    let shown = path.display();
    match () {
        _ if is_rust(path) && proc_macro => Some(format!("{shown} changed, of a proc macro")),
        _ if is_rust(path) && script => Some(format!(
            "{shown} changed, which no program is compiled from, in a package with a build script"
        )),
        _ if !is_rust(path) && script => {
            Some(format!("{shown} changed, in a package whose build script may read it"))
        }
        _ => None,
    }
}

/// What the daemon says to run of each program of each checkout of `state`, after the
/// source files `sources` changed from `texts`.
fn selected(
    state: &State,
    sources: &[PathBuf],
    texts: &BTreeMap<PathBuf, Option<String>>,
    now: &Passed,
    unsynced: bool,
) -> Vec<Vec<Picked>> {
    let changes: Vec<Change> = sources
        .iter()
        .map(|path| Change {
            path: path.clone(),
            was: texts.get(path).cloned().flatten(),
            now: now.hash(path),
        })
        .collect();
    let written = (|| {
        let path =
            super::daemon::home()?.join("passed").join(format!("{}.changes.json", state.key));
        std::fs::create_dir_all(path.parent().unwrap_or(&path))?;
        std::fs::write(&path, serde_json::to_vec(&changes)?)?;
        anyhow::Ok(path)
    })();
    let ask = |checkout: &Checkout| -> Vec<Picked> {
        let whole = |why: String| vec![Picked::Whole(why); checkout.programs.len()];
        let path = match &written {
            Ok(path) => path.clone(),
            Err(why) => return whole(format!("writing the changes: {why:#}")),
        };
        if checkout.programs.is_empty() {
            return Vec::new();
        }
        // What the worker builds here is not used: cargo builds.
        let started = std::time::Instant::now();
        if unsynced {
            let _ = super::synced(checkout);
        }
        let synced = started.elapsed();
        let asked = Checkout { sources: Vec::new(), ..super::asking(checkout) };
        let answer = client::ask(Request::Select(asked, path), Start::Never);
        if std::env::var_os("RUSTD_STATS").is_some() {
            let selected = started.elapsed() - synced;
            eprintln!("picked: synced in {synced:.1?}, told in {selected:.1?}");
        }
        match answer {
            Ok(Some(Reply::Selected { programs })) => programs
                .into_iter()
                .map(|answer| match answer {
                    Ok(line) => read_answer(&line),
                    Err(why) => Picked::Whole(why),
                })
                .collect(),
            Ok(Some(Reply::Cold { reason })) => whole(reason),
            Ok(other) => whole(format!("the daemon answered {other:?}")),
            Err(why) => whole(format!("{why:#}")),
        }
    };
    // The checkouts are of two workers, which tell their programs side by side.
    let answers = std::thread::scope(|scope| {
        let asking: Vec<_> =
            state.checkouts.iter().map(|checkout| scope.spawn(move || ask(checkout))).collect();
        let joined = std::iter::zip(asking, &state.checkouts).map(|(it, checkout)| {
            let panicked = || {
                vec![
                    Picked::Whole("asking the daemon panicked".to_owned());
                    checkout.programs.len()
                ]
            };
            it.join().unwrap_or_else(|_| panicked())
        });
        joined.collect()
    });
    if let Ok(path) = &written {
        let _ = std::fs::remove_file(path);
    }
    answers
}

/// What a worker's `ok` line says to run.
fn read_answer(line: &str) -> Picked {
    let said = line.strip_prefix("ok ").unwrap_or(line);
    serde_json::from_str(said)
        .unwrap_or_else(|_| Picked::Whole(format!("the daemon answered {said:?}")))
}

/// Whether the arguments for the test executables leave which tests run as it is.
fn harmless(trailing: &[String]) -> bool {
    let mut words = trailing.iter();
    while let Some(word) = words.next() {
        let flag = word.split_once('=').map_or(word.as_str(), |it| it.0);
        if HARMLESS_VALUED.contains(&flag) {
            if !word.contains('=') {
                words.next();
            }
            continue;
        }
        if !HARMLESS.contains(&flag) {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_harness_flag_that_picks_tests_is_not_harmless() {
        let words = |text: &str| text.split_whitespace().map(str::to_owned).collect::<Vec<_>>();
        assert!(harmless(&words("")));
        assert!(harmless(&words("--nocapture --test-threads 1 --color=never")));
        for picking in ["--ignored", "--include-ignored", "--skip a", "--exact", "name"] {
            assert!(!harmless(&words(picking)), "{picking}");
        }
    }

    #[test]
    fn a_line_that_is_not_an_answer_runs_everything() {
        assert!(matches!(read_answer("ok unchanged"), Picked::Whole(_)));
        let said = serde_json::to_string(&Picked::Nothing("x".to_owned())).unwrap();
        assert_eq!(read_answer(&format!("ok {said}")), Picked::Nothing("x".to_owned()));
    }
}
