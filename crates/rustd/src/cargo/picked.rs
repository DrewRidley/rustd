//! Which tests `cargo rustd test` runs: those an edit since the tests last all passed can
//! change the outcome of. What changed is told by the files, and which tests it reaches by
//! the daemon, which has the code loaded. Whatever can't be told runs: a whole program, or
//! every test.

use std::{collections::BTreeMap, path::PathBuf};

use super::{
    cli::Invocation,
    client::{self, Start},
    daemon::{Checkout, Reply, Request},
    passed::{Passed, is_rust},
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
    /// The source to keep as what passed, once everything ran and passed.
    passing: Option<Passed>,
    key: String,
}

impl Picks {
    /// Everything, as asked.
    pub(crate) fn all(state: &State, passing: Option<Passed>) -> Picks {
        let programs = vec![None; state.programs.len()];
        Picks { programs, names: names(state), doc_packages: None, passing, key: state.key.clone() }
    }

    /// What to run of `program`: `None` is all of it, also for one not known when the picks
    /// were made.
    pub(crate) fn of(&self, program: &Program) -> Option<&Picked> {
        let name = (program.package.clone(), program.kind, program.name.clone());
        let at = self.names.iter().position(|it| *it == name)?;
        self.programs[at].as_ref()
    }

    /// Keeps the source the tests ran on as what passed: they ran and all passed.
    pub(crate) fn passed(self) {
        if let Some(passing) = self.passing
            && let Err(why) = passing.write(&self.key)
        {
            say::fallback(&format!("{why:#}"), "the next run of tests runs them all");
        }
    }

    /// Whether only some tests run.
    pub(crate) fn some(&self) -> bool {
        self.doc_packages.is_some() || self.programs.iter().any(Option::is_some)
    }
}

/// What the tests of `state`, which the daemon built when `by_daemon`, run. With `unsynced`,
/// cargo builds them and the daemon has not read the source since the last build: it is
/// asked to first.
pub(crate) fn pick(asked: &Invocation, state: &State, by_daemon: bool, unsynced: bool) -> Picks {
    // The tests named are the ones asked for, and not all of them ran once they did.
    if !asked.positional.is_empty() || !harmless(&asked.trailing) {
        return Picks::all(state, None);
    }
    let before = Passed::read(&state.key);
    let now = Passed::now(state, before.as_ref());
    let all_asked = asked.flags.full || std::env::var_os("RUSTD_ALL_TESTS").is_some();
    if all_asked {
        return Picks::all(state, Some(now));
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
            if let Some(name) = before.env_differs(&now) {
                let why = format!("`{name}` is not what it was when the tests last passed");
                say::fallback(&why, "every test runs");
                return Picks::all(state, Some(now));
            }
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
    // What cargo built is all there is to run, and the daemon has not seen it.
    if !by_daemon {
        return Picks::all(state, keep.then_some(now));
    }
    let mut picks = Picks {
        programs: vec![None; state.programs.len()],
        names: names(state),
        doc_packages: None,
        passing: None,
        key: state.key.clone(),
    };
    let mut whole: Vec<Option<String>> = vec![None; state.programs.len()];
    let mut documented: Vec<String> = Vec::new();
    let mut sources: Vec<PathBuf> = Vec::new();
    for path in &changed {
        let reads: Vec<usize> = (0..state.programs.len())
            .filter(|&at| state.programs[at].inputs().contains(path))
            .collect();
        let owners: Vec<usize> = (0..state.programs.len())
            .filter(|&at| path.starts_with(&state.programs[at].package_dir))
            .collect();
        for &at in reads.iter().chain(&owners) {
            let program = &state.programs[at];
            if program.kind == Kind::Lib || owners.contains(&at) {
                documented.push(program.package.clone());
            }
        }
        if is_rust(path) {
            if !reads.is_empty() {
                sources.push(path.clone());
            }
            continue;
        }
        let (programs, why) = match reads.is_empty() {
            false => (reads, format!("{} changed, which rustc read", path.display())),
            // A test may read a file of its package as it runs.
            true => (owners, format!("{} changed, in its package", path.display())),
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
    let selected = match sources.is_empty() {
        true => Vec::new(),
        false => selected(state, &sources, &texts, &now, unsynced),
    };
    for (at, program) in state.programs.iter().enumerate() {
        let reads_source = sources.iter().any(|it| program.inputs().contains(it));
        picks.programs[at] = Some(match (whole[at].take(), program.asked_as) {
            (Some(why), _) => Picked::Whole(why),
            // What it is compiled from is not known, so neither is whether that changed.
            (None, _) if program.inputs().is_empty() && !changed.is_empty() => {
                Picked::Whole("rustd does not know what it is compiled from".to_owned())
            }
            (None, _) if !reads_source => {
                Picked::Nothing("nothing it is compiled from changed".to_owned())
            }
            (None, Some((checkout, place))) => selected
                .get(checkout)
                .and_then(|it| it.get(place))
                .cloned()
                .unwrap_or_else(|| Picked::Whole("the daemon did not say".to_owned())),
            (None, None) => Picked::Whole("the daemon does not build it".to_owned()),
        });
    }
    picks.passing = keep.then_some(now);
    picks
}

fn names(state: &State) -> Vec<(String, Kind, String)> {
    state.programs.iter().map(|it| (it.package.clone(), it.kind, it.name.clone())).collect()
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
    state
        .checkouts
        .iter()
        .map(|checkout| {
            let whole = |why: String| vec![Picked::Whole(why); checkout.programs.len()];
            let path = match &written {
                Ok(path) => path.clone(),
                Err(why) => return whole(format!("writing the changes: {why:#}")),
            };
            if checkout.programs.is_empty() {
                return Vec::new();
            }
            // What the worker builds here is not used: cargo builds.
            if unsynced {
                let _ = super::synced(checkout);
            }
            let asked = Checkout { sources: Vec::new(), ..super::asking(checkout) };
            match client::ask(Request::Select(asked, path), Start::Never) {
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
        })
        .collect()
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
