//! Running what was built, as cargo does: the program of `cargo run`, the test executables
//! of `cargo test`.

use std::{
    path::Path,
    process::{Command, ExitStatus},
};

use anyhow::Context as _;

use crate::impact::Picked;

use super::{
    cli::Invocation,
    picked::Picks,
    say,
    state::{Kind, Program, State, as_built, cargo_path},
};

/// The exit status of cargo when a test fails or a program can't be run.
const FAILED: i32 = 101;

/// Runs the program `asked` names, and ends as it ended. `None` when cargo has to say
/// which program that is: there are several, or none.
pub(crate) fn program(
    asked: &Invocation,
    state: &State,
    cwd: &Path,
) -> anyhow::Result<Option<i32>> {
    let runnable = |it: &&Program| !it.test && matches!(it.kind, Kind::Bin | Kind::Example);
    let mut programs: Vec<&Program> = state.programs.iter().filter(runnable).collect();
    if let Some(name) = asked.flags.bin.as_ref().or(asked.flags.example.as_ref()) {
        programs.retain(|it| it.name == *name);
    }
    let &[program] = programs.as_slice() else { return Ok(None) };
    let shown = shown_path(&program.executable, cwd);
    let args = asked.program_args().map(|it| format!(" {it}")).collect::<String>();
    say::status(asked.flags.quiet, "Running", &format!("`{shown}{args}`"));
    let mut command = Command::new(&program.executable);
    command.args(asked.program_args()).envs(program.env.iter().cloned());
    as_built(&mut command)?;
    let status = command.status().with_context(|| format!("could not execute {shown}"))?;
    Ok(Some(exit_code(status)))
}

/// Runs the test executables one after another, each in the directory of its package, and
/// stops at the first that fails unless told not to. Of each, the tests `picks` has run. The
/// result is cargo's exit status.
pub(crate) fn tests(
    asked: &Invocation,
    state: &State,
    cwd: &Path,
    picks: &Picks,
) -> anyhow::Result<i32> {
    let mut failed = Vec::new();
    let (mut ran, mut skipped) = (0, 0);
    for (test, picked) in std::iter::zip(&state.programs, &picks.programs) {
        if !test.test {
            continue;
        }
        let source = test.source.strip_prefix(&test.package_dir).unwrap_or(&test.source);
        let unit = if matches!(test.kind, Kind::Test | Kind::Bench) { "" } else { "unittests " };
        let shown = shown_path(&test.executable, cwd);
        let named = match picked {
            None | Some(Picked::Whole(_)) => None,
            Some(Picked::Nothing(why)) => {
                let said = format!("{unit}{} ({why})", source.display());
                say::status(asked.flags.quiet, "Skipping", &said);
                skipped += 1;
                continue;
            }
            Some(Picked::Tests { known, run }) => match to_run(test, known, run) {
                Ok(named) if named.is_empty() => {
                    let said =
                        format!("{unit}{} (no test of it reaches an edit)", source.display());
                    say::status(asked.flags.quiet, "Skipping", &said);
                    skipped += 1;
                    continue;
                }
                Ok(named) => Some(named),
                Err(why) => {
                    say::fallback(&format!("{shown}: {why}"), "all of its tests run");
                    None
                }
            },
        };
        let of = match (&named, picked) {
            (Some(named), _) => format!(", {} of its tests", named.len()),
            (None, Some(Picked::Whole(why))) if picks.some() => format!(", all: {why}"),
            _ => String::new(),
        };
        say::status(
            asked.flags.quiet,
            "Running",
            &format!("{unit}{} ({shown}){of}", source.display()),
        );
        ran += 1;
        let runs: Vec<Vec<&str>> = match &named {
            // An argument list has a limit; a few thousand names are well under it.
            Some(named) => {
                named.chunks(2000).map(|it| it.iter().map(String::as_str).collect()).collect()
            }
            None => vec![Vec::new()],
        };
        let mut passed = true;
        for names in runs {
            let mut command = Command::new(&test.executable);
            if names.is_empty() {
                command.args(asked.program_args());
            } else {
                command.arg("--exact").args(names).args(&asked.trailing);
            }
            command.envs(test.env.iter().cloned()).current_dir(&test.package_dir);
            as_built(&mut command)?;
            let status = command.status().with_context(|| format!("could not execute {shown}"))?;
            passed &= status.success();
        }
        if !passed {
            failed.push(rerun_flags(test));
            if !asked.flags.no_fail_fast {
                break;
            }
        }
    }
    if picks.some() {
        let said = format!(
            "{ran} test executables, {skipped} not reached by the edits; `--full` runs all"
        );
        say::status(asked.flags.quiet, "Picked", &said);
    }
    match failed.as_slice() {
        [] => Ok(0),
        [one] => {
            say::error(&format!("test failed, to rerun pass `{one}`"));
            Ok(FAILED)
        }
        several => {
            say::error(&format!("{} targets failed:", several.len()));
            several.iter().for_each(|it| eprintln!("    `{it}`"));
            Ok(FAILED)
        }
    }
}

/// The tests of `test` to run, by name: the ones of `run` and those its harness lists that
/// are not `known`, which a macro made. `Err` is why all of them run.
fn to_run(
    test: &Program,
    known: &[String],
    run: &[(String, String)],
) -> Result<Vec<String>, String> {
    let mut command = Command::new(&test.executable);
    command.args(["--list", "--format", "terse"]).envs(test.env.iter().cloned());
    command.current_dir(&test.package_dir).stderr(std::process::Stdio::null());
    as_built(&mut command).map_err(|it| it.to_string())?;
    let out = command.output().map_err(|it| it.to_string())?;
    if !out.status.success() {
        return Err("its harness did not list its tests".to_owned());
    }
    let listed = String::from_utf8_lossy(&out.stdout);
    let listed: Vec<&str> = listed
        .lines()
        .filter_map(|it| it.strip_suffix(": test").or_else(|| it.strip_suffix(": benchmark")))
        .collect();
    let picked: Vec<&str> = run.iter().map(|it| it.0.as_str()).collect();
    if let Some(missing) = picked.iter().find(|it| !listed.contains(it)) {
        return Err(format!("its harness does not list the test `{missing}`"));
    }
    let made = |name: &&str| !known.iter().any(|it| it == name);
    Ok(listed.into_iter().filter(|it| picked.contains(it) || made(it)).map(str::to_owned).collect())
}

/// Runs the doc tests with cargo, which hands them to rustd in rustdoc's place: rustd
/// builds each example as its own program over its code of the library. With `packages`,
/// the doc tests of those packages alone, when no package was named.
pub(crate) fn doc_tests(asked: &Invocation, packages: Option<&[String]>) -> anyhow::Result<i32> {
    let named = asked.flags.workspace || !asked.flags.packages.is_empty();
    let packages = packages.filter(|_| !named);
    if packages.is_some_and(|it| it.is_empty()) {
        say::status(asked.flags.quiet, "Skipping", "doc tests (no edit reaches them)");
        return Ok(0);
    }
    let mut args = vec!["--doc".to_owned()];
    args.extend(asked.build_args.iter().cloned());
    for package in packages.into_iter().flatten() {
        args.extend(["-p".to_owned(), package.clone()]);
    }
    if asked.flags.no_fail_fast {
        args.push("--no-fail-fast".to_owned());
    }
    args.extend(asked.positional.iter().cloned());
    args.push("--".to_owned());
    args.extend(asked.trailing.iter().cloned());
    only_doc_tests(&args)
}

/// Runs `cargo test` with `args`, which ask for the doc tests alone, with rustd in
/// rustdoc's place.
pub(crate) fn only_doc_tests(args: &[String]) -> anyhow::Result<i32> {
    let rustdoc = std::env::var_os("RUSTDOC").unwrap_or_else(|| "rustdoc".into());
    let mut cargo = crate::doctest::as_asked(cargo_path());
    cargo
        .env("RUSTDOC", std::env::current_exe().context("finding rustd itself")?)
        .env(crate::doctest::ENABLED, rustdoc);
    cargo.arg("test").args(args);
    as_built(&mut cargo)?;
    Ok(exit_code(cargo.status().context("running `cargo test --doc`")?))
}

/// The flags that have cargo run this test target alone.
fn rerun_flags(test: &Program) -> String {
    let target = match test.kind {
        Kind::Lib => "--lib".to_owned(),
        Kind::Bin => format!("--bin {}", test.name),
        Kind::Example => format!("--example {}", test.name),
        Kind::Test => format!("--test {}", test.name),
        Kind::Bench => format!("--bench {}", test.name),
    };
    format!("-p {} {target}", test.package)
}

/// The path as cargo shows it: from the directory the command runs in, when it is below.
fn shown_path(path: &Path, cwd: &Path) -> String {
    path.strip_prefix(cwd).unwrap_or(path).display().to_string()
}

/// What this command ends with after a process that ended with `status`: its code, or
/// the code a shell gives one a signal ended.
pub(crate) fn exit_code(status: ExitStatus) -> i32 {
    #[cfg(unix)]
    if let Some(signal) = std::os::unix::process::ExitStatusExt::signal(&status) {
        return 128 + signal;
    }
    status.code().unwrap_or(1)
}
