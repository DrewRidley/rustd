//! Running what was built, as cargo does: the program of `cargo run`, the test executables
//! of `cargo test`.

use std::{
    collections::BTreeSet,
    io::{BufRead as _, BufReader, Write as _},
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    time::Instant,
};

use anyhow::Context as _;

use super::{
    cli::Invocation,
    say,
    state::{Kind, Program, State, cargo_path},
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
    let status = command.status().with_context(|| format!("could not execute {shown}"))?;
    Ok(Some(exit_code(status)))
}

fn plan_object(state: &State, test: &Program) -> Option<PathBuf> {
    let (checkout, at) = test.asked_as?;
    Some(state.checkouts.get(checkout)?.programs.get(at)?.object.clone())
}

fn read_lines(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            if let Some(rest) = s.strip_prefix("@target=")
                && let Some((_, rel)) = rest.split_once('\t')
            {
                rel.to_owned()
            } else {
                s.to_owned()
            }
        })
        .collect()
}

fn run_and_track_failures(
    command: &mut Command,
    prev_failed: &mut BTreeSet<String>,
) -> std::io::Result<ExitStatus> {
    command.stdout(Stdio::piped());
    let mut child = command.spawn()?;
    if let Some(stdout) = child.stdout.take() {
        let mut out = std::io::stdout();
        for line in BufReader::new(stdout).lines() {
            let line = line?;
            writeln!(out, "{line}")?;
            let _ = out.flush();
            if let Some(rest) = line.strip_prefix("test ") {
                if let Some(name) = rest.strip_suffix(" ... FAILED") {
                    prev_failed.insert(name.trim().to_owned());
                } else if let Some(name) = rest.strip_suffix(" ... ok") {
                    prev_failed.remove(name.trim());
                }
            }
        }
    }
    child.wait()
}

/// Runs the test executables one after another, each in the directory of its package, and
/// stops at the first that fails unless told not to. The result is cargo's exit status.
pub(crate) fn tests(
    asked: &Invocation,
    state: &State,
    cwd: &Path,
    served_by_daemon: bool,
) -> anyhow::Result<i32> {
    if let Some(query) = &asked.flags.explain {
        if !served_by_daemon {
            let why = state.refusal.as_deref().unwrap_or("the build fell back to cargo");
            say::fallback(why, "cannot explain incremental test selection");
            return Ok(10);
        }
        let mut matched = false;
        for test in state
            .programs
            .iter()
            .filter(|it| it.test && !asked.flags.excluded_packages.contains(&it.package))
        {
            let Some(obj) = plan_object(state, test) else { continue };
            let explain_txt = obj.with_extension("explain.txt");
            if let Ok(txt) = std::fs::read_to_string(&explain_txt) {
                for line in txt.lines() {
                    if let Some((test_name, why)) = line.split_once('\t')
                        && (test_name == query || test_name.contains(query.as_str()))
                    {
                        println!("explain `{test_name}`:");
                        for (idx, step) in why.split(" <- ").enumerate() {
                            if idx == 0 {
                                println!("  {test_name}");
                                println!("  <- {step}");
                            } else {
                                println!("  <- {step}");
                            }
                        }
                        matched = true;
                    }
                }
            }
        }
        if !matched {
            println!("explain `{query}`: not selected (no path from modified items to `{query}`)");
        }
        return Ok(0);
    }

    if asked.flags.list_changed {
        if !served_by_daemon {
            let why = state.refusal.as_deref().unwrap_or("the build fell back to cargo");
            say::fallback(why, "cannot list impacted tests incrementally");
            return Ok(10);
        }
        let mut printed = BTreeSet::new();
        for test in state
            .programs
            .iter()
            .filter(|it| it.test && !asked.flags.excluded_packages.contains(&it.package))
        {
            let Some(obj) = plan_object(state, test) else { continue };
            for line in read_lines(&obj.with_extension("tests.txt")) {
                if printed.insert(line.clone()) {
                    println!("{line}");
                }
            }
        }
        return Ok(0);
    }

    let is_ci_runner = asked.flags.since.is_some()
        || asked.flags.junit.is_some()
        || asked.flags.json
        || asked.flags.also_run.is_some()
        || asked.flags.retries.is_some();

    if is_ci_runner {
        return run_ci_tests(asked, state, served_by_daemon);
    }

    let mut failed = Vec::new();
    let allow_incremental = !asked.flags.full_tests
        && std::env::var_os("RUSTD_ALL_TESTS").is_none()
        && asked.positional.is_empty();
    let any_incremental = allow_incremental
        && state
            .programs
            .iter()
            .filter(|it| it.test && !asked.flags.excluded_packages.contains(&it.package))
            .any(|test| {
                let Some(obj) = plan_object(state, test) else { return false };
                !read_lines(&obj.with_extension("failed.txt")).is_empty()
                    || (served_by_daemon
                        && !read_lines(&obj.with_extension("tests.txt")).is_empty())
            });

    for test in state
        .programs
        .iter()
        .filter(|it| it.test && !asked.flags.excluded_packages.contains(&it.package))
    {
        let source = test.source.strip_prefix(&test.package_dir).unwrap_or(&test.source);
        let unit = if matches!(test.kind, Kind::Test | Kind::Bench) { "" } else { "unittests " };
        let shown = shown_path(&test.executable, cwd);
        let obj = plan_object(state, test);
        let failed_path = obj.as_ref().map(|o| o.with_extension("failed.txt"));
        let mut prev_failed: BTreeSet<String> = failed_path
            .as_deref()
            .map(read_lines)
            .unwrap_or_default()
            .into_iter()
            .collect();
        let impacted: Vec<String> = if served_by_daemon {
            obj.as_ref()
                .map(|o| read_lines(&o.with_extension("tests.txt")))
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        let mut selected = Vec::new();
        if allow_incremental && (!impacted.is_empty() || !prev_failed.is_empty()) {
            for t in &impacted {
                if !selected.contains(t) {
                    selected.push(t.clone());
                }
            }
            for t in &prev_failed {
                if !selected.contains(t) {
                    selected.push(t.clone());
                }
            }
        }

        let exact = if !selected.is_empty() {
            crate::ci::expand_exact_tests(&test.executable, &selected)
        } else {
            Vec::new()
        };

        if !exact.is_empty() {
            say::status(
                asked.flags.quiet,
                "Running",
                &format!(
                    "{unit}{} ({shown}) [{} impacted, {} failed last run]",
                    source.display(),
                    impacted.len(),
                    prev_failed.len()
                ),
            );
            let mut target_failed = false;
            for test_name in &exact {
                let mut command = Command::new(&test.executable);
                command
                    .arg("--exact")
                    .arg(test_name)
                    .args(&asked.trailing)
                    .envs(test.env.iter().cloned())
                    .current_dir(&test.package_dir);
                let status = run_and_track_failures(&mut command, &mut prev_failed)
                    .with_context(|| format!("could not execute {shown}"))?;
                if !status.success() {
                    target_failed = true;
                    if !asked.flags.no_fail_fast {
                        break;
                    }
                }
            }
            if let Some(fp) = &failed_path {
                let body: Vec<&str> = prev_failed.iter().map(String::as_str).collect();
                let _ = std::fs::write(fp, body.join("\n"));
            }
            if target_failed {
                failed.push(rerun_flags(test));
                if !asked.flags.no_fail_fast {
                    break;
                }
            }
            continue;
        }
        if any_incremental {
            continue;
        }

        say::status(asked.flags.quiet, "Running", &format!("{unit}{} ({shown})", source.display()));
        let mut command = Command::new(&test.executable);
        command.args(asked.program_args()).envs(test.env.iter().cloned());
        command.current_dir(&test.package_dir);
        let status = run_and_track_failures(&mut command, &mut prev_failed)
            .with_context(|| format!("could not execute {shown}"))?;
        if let Some(fp) = &failed_path {
            let body: Vec<&str> = prev_failed.iter().map(String::as_str).collect();
            let _ = std::fs::write(fp, body.join("\n"));
        }
        if !status.success() {
            failed.push(rerun_flags(test));
            if !asked.flags.no_fail_fast {
                break;
            }
        }
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

fn run_ci_tests(
    asked: &Invocation,
    state: &State,
    served_by_daemon: bool,
) -> anyhow::Result<i32> {
    let mut target_runs: Vec<(String, PathBuf, Vec<String>)> = Vec::new();
    let mut reasons = std::collections::HashMap::new();
    let also: Vec<String> = asked
        .flags
        .also_run
        .as_deref()
        .map(read_lines)
        .unwrap_or_default();

    let fallback_pkgs: Option<Vec<String>> = if !served_by_daemon
        && let Some(sha) = &asked.flags.since
        && let Ok(diff_files) = crate::ci::analyze_git_diff_since(&state.root, sha)
    {
        Some(crate::ci::infer_packages_from_diff(&state.root, &diff_files))
    } else {
        None
    };

    for test in state
        .programs
        .iter()
        .filter(|it| it.test && !asked.flags.excluded_packages.contains(&it.package))
    {
        let obj = plan_object(state, test);
        if served_by_daemon && let Some(obj) = obj {
            if let Ok(txt) = std::fs::read_to_string(obj.with_extension("explain.txt")) {
                for line in txt.lines() {
                    if let Some((t, r)) = line.split_once('\t') {
                        reasons.insert(t.to_owned(), r.to_owned());
                    }
                }
            }
            let mut wanted = read_lines(&obj.with_extension("tests.txt"));
            for f in read_lines(&obj.with_extension("failed.txt")) {
                if !wanted.contains(&f) {
                    wanted.push(f);
                }
            }
            for a in &also {
                if !wanted.contains(a) {
                    wanted.push(a.clone());
                }
            }
            let exact = crate::ci::expand_exact_tests(&test.executable, &wanted);
            if !exact.is_empty() {
                target_runs.push((test.package.clone(), test.executable.clone(), exact));
            }
        } else {
            // Fallback path: the daemon did not compute `tests.txt` for this binary.
            // Never silently select 0 tests and exit 0 — run all tests in the affected package(s).
            if let Some(ref pkgs) = fallback_pkgs
                && !pkgs.is_empty()
                && !pkgs.contains(&test.package)
            {
                continue;
            }
            if let Some(all_tests) = crate::ci::list_tests_cached(&test.executable)
                && !all_tests.is_empty()
            {
                target_runs.push((test.package.clone(), test.executable.clone(), all_tests));
            }
        }
    }

    let total_tests: usize = target_runs.iter().map(|(_, _, t)| t.len()).sum();
    let cfg = crate::ci::NextestConfig::load(&state.root, asked.flags.retries);
    let t0 = Instant::now();
    if !asked.flags.json {
        eprintln!(
            "    \x1b[1;32mStarting\x1b[0m {} impacted test(s) across {} binary(s) (isolated processes)",
            total_tests,
            target_runs.len().max(1)
        );
    }
    let mut outcomes = Vec::with_capacity(total_tests);
    for (run_pkg, run_bin, run_tests) in &target_runs {
        let bin_outcomes = crate::ci::run_isolated_test_suite(
            run_pkg,
            run_bin,
            run_tests,
            &reasons,
            &cfg,
            &asked.trailing,
            asked.flags.json,
        );
        if let Some(test_prog) = state.programs.iter().find(|p| p.executable == *run_bin)
            && let Some(obj) = plan_object(state, test_prog)
        {
            let fp = obj.with_extension("failed.txt");
            let mut prev_failed: BTreeSet<String> =
                read_lines(&fp).into_iter().collect();
            for o in &bin_outcomes {
                if o.passed {
                    prev_failed.remove(&o.name);
                } else if !o.zero_matched {
                    prev_failed.insert(o.name.clone());
                }
            }
            let body: Vec<&str> = prev_failed.iter().map(String::as_str).collect();
            let _ = std::fs::write(&fp, body.join("\n"));
        }
        outcomes.extend(bin_outcomes);
    }
    let total_dur = t0.elapsed();
    let zero_matched = outcomes.iter().filter(|o| o.zero_matched).count();
    let passed = outcomes.iter().filter(|o| o.passed).count();
    let failed = outcomes.iter().filter(|o| !o.passed && !o.zero_matched).count();
    if let Some(junit_path) = &asked.flags.junit {
        crate::ci::write_junit_xml(junit_path, &outcomes, total_dur)?;
    }
    if asked.flags.json {
        crate::ci::print_json_summary(&outcomes, total_dur);
    } else {
        eprintln!(
            "------------\n     \x1b[1;32mSummary\x1b[0m [{:>7.3}s] {} tests run: {} passed, {} failed{}",
            total_dur.as_secs_f64(),
            outcomes.len(),
            passed,
            failed,
            if zero_matched > 0 {
                format!(", {zero_matched} unmatched (runner error)")
            } else {
                String::new()
            }
        );
    }
    for test in state.programs.iter().filter(|it| it.test) {
        let Some(obj) = plan_object(state, test) else { continue };
        let failed_names: Vec<&str> = outcomes
            .iter()
            .filter(|o| !o.passed && !o.zero_matched)
            .map(|o| o.name.as_str())
            .collect();
        let _ = std::fs::write(obj.with_extension("failed.txt"), failed_names.join("\n"));
    }
    if zero_matched > 0 {
        return Ok(2);
    }
    if failed > 0 {
        return Ok(FAILED);
    }
    Ok(0)
}

/// Runs the doc tests with cargo, which has rustdoc build them.
pub(crate) fn doc_tests(asked: &Invocation) -> anyhow::Result<i32> {
    let mut cargo = Command::new(cargo_path());
    cargo.args(["test", "--doc"]).args(&asked.build_args);
    if asked.flags.no_fail_fast {
        cargo.arg("--no-fail-fast");
    }
    cargo.args(&asked.positional).arg("--").args(&asked.trailing);
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
