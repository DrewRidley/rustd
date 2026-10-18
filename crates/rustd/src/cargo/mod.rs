//! `cargo rustd build|run|test|check`: cargo's commands, with the daemon building what
//! changed since cargo last built. `CARGO.md` at the root of the repository is the design.

pub(crate) mod daemon;
pub(crate) mod wrapper;

mod cli;
mod client;
mod clippy;
mod plan;
mod run;
mod say;
pub(crate) mod script;
mod state;
mod units;

use std::{path::Path, process::Command, time::Instant};

use anyhow::Context as _;

use cli::{Asked, Invocation, Subcommand};
use client::Start;
use daemon::{Checkout, Reply, Request};
use state::{Built, Counts, State};

/// Whether `rustd <sub>` is a cargo subcommand invocation rather than a worker invocation.
pub(crate) fn is_direct_subcommand(sub: &str) -> bool {
    matches!(
        sub,
        "build" | "b" | "run" | "r" | "test" | "t" | "check" | "c" | "clippy" | "status" | "stop" | "clean"
    ) && !std::env::args().any(|a| matches!(a.as_str(), "--crate" | "--out" | "--socket"))
}

/// `rustd cargo ...`: runs the command and ends with the status cargo would end with.
pub(crate) fn command(args: impl Iterator<Item = String>) -> anyhow::Result<()> {
    let code = match Asked::parse(args.collect()) {
        Asked::Build(asked) => match asked.only_cargo() {
            Some(reason) => {
                say::fallback(&reason, &format!("this is `cargo {}`", asked.subcommand.name()));
                let typed = std::iter::once(asked.subcommand.name().to_owned()).chain(asked.typed);
                plain_cargo(typed.collect())?
            }
            None => build(&asked)?,
        },
        Asked::Clippy(args) => clippy(args)?,
        Asked::Cargo(args) => plain_cargo(args)?,
        Asked::Status => status()?,
        Asked::Stop => stop()?,
    };
    std::process::exit(code)
}

/// Runs `cargo check` with `rustd` as `RUSTC_WORKSPACE_WRAPPER` driving `clippy-driver`
/// with outline-based caching.
fn clippy(args: Vec<String>) -> anyhow::Result<i32> {
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let sysroot = Command::new(&rustc)
        .arg("--print=sysroot")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned());
    let driver = sysroot
        .as_deref()
        .map(|s| Path::new(s).join("bin").join("clippy-driver"))
        .filter(|p| p.exists());
    let Some(driver) = driver else {
        return plain_cargo(std::iter::once("clippy".to_owned()).chain(args).collect());
    };
    let (cargo_args, extra_args) = match args.iter().position(|it| it == "--") {
        Some(at) => (&args[..at], &args[at + 1..]),
        None => (&args[..], &[][..]),
    };
    let mut command = Command::new(state::cargo_path());
    command.arg("check").args(cargo_args);
    command.env("RUSTC_WORKSPACE_WRAPPER", std::env::current_exe()?);
    if std::env::var_os("RUSTC_WRAPPER").is_none() {
        command.env("CARGO_BUILD_RUSTC_WRAPPER", "");
        command.env("RUSTC_WRAPPER", "");
    }
    command.env(wrapper::ENABLED, "1");
    command.env(wrapper::CLIPPY_ENABLED, "1");
    command.env(wrapper::CLIPPY_DRIVER, &driver);
    if !extra_args.is_empty() {
        command.env(wrapper::CLIPPY_EXTRA_ARGS, serde_json::to_string(extra_args)?);
    }
    if let Some(sysroot) = &sysroot {
        let lib_dir = Path::new(sysroot).join("lib");
        #[cfg(target_os = "macos")]
        {
            let prev = std::env::var("DYLD_LIBRARY_PATH").unwrap_or_default();
            let val = if prev.is_empty() {
                lib_dir.display().to_string()
            } else {
                format!("{}:{prev}", lib_dir.display())
            };
            command.env("DYLD_LIBRARY_PATH", val);
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            let prev = std::env::var("LD_LIBRARY_PATH").unwrap_or_default();
            let val = if prev.is_empty() {
                lib_dir.display().to_string()
            } else {
                format!("{}:{prev}", lib_dir.display())
            };
            command.env("LD_LIBRARY_PATH", val);
        }
    }
    let status = command.status().context("running cargo clippy")?;
    Ok(run::exit_code(status))
}

/// Runs cargo with `args` and nothing of rustd's.
fn plain_cargo(args: Vec<String>) -> anyhow::Result<i32> {
    let status = Command::new(state::cargo_path()).args(args).status().context("running cargo")?;
    Ok(run::exit_code(status))
}

fn prepare_since_cache(asked: &Invocation, state: &State) -> anyhow::Result<()> {
    let diff_files = match &asked.flags.since {
        Some(sha) => Some((sha, crate::ci::analyze_git_diff_since(&state.root, sha)?)),
        None => None,
    };
    for checkout in &state.checkouts {
        for plan in &checkout.programs {
            let baseline_cache = plan.object.with_extension("baseline.cache");
            if let Some((sha, files)) = &diff_files {
                if let Some(parent) = baseline_cache.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                crate::ci::write_since_baseline_cache(
                    &baseline_cache,
                    &format!("SINCE:{sha}"),
                    files,
                )?;
                let _ = std::fs::remove_file(plan.object.with_extension("tests.txt"));
            } else if baseline_cache.exists() {
                let _ = std::fs::remove_file(&baseline_cache);
                let _ = std::fs::remove_file(plan.object.with_extension("tests.txt"));
            }
        }
    }
    Ok(())
}

/// Builds what `asked` names, by the daemon when it can and by cargo otherwise, and then
/// runs what the subcommand runs.
fn build(asked: &Invocation) -> anyhow::Result<i32> {
    if let [pkg] = asked.flags.packages.as_slice()
        && asked.flags.excluded_packages.contains(pkg)
    {
        if let Some(junit_path) = &asked.flags.junit {
            crate::ci::write_junit_xml(junit_path, &[], std::time::Duration::ZERO)?;
        }
        if asked.flags.json {
            crate::ci::print_json_summary(&[], std::time::Duration::ZERO);
        } else {
            eprintln!("[rustd] package `{pkg}` excluded by --exclude; 0 tests to run");
        }
        return Ok(0);
    }

    let started = Instant::now();
    let cwd = std::env::current_dir()?;
    let mut owned_asked;
    let asked = if let Some(sha) = &asked.flags.since {
        let ws_root = cwd
            .ancestors()
            .find(|d| d.join(".git").exists())
            .unwrap_or(&cwd);
        let diff_files = crate::ci::analyze_git_diff_since(ws_root, sha)?;
        if asked.flags.packages.is_empty() && !asked.flags.workspace {
            let pkgs = crate::ci::infer_packages_from_diff(ws_root, &diff_files);
            if let Some(first_pkg) = pkgs.into_iter().find(|p| !asked.flags.excluded_packages.contains(p)) {
                owned_asked = Invocation {
                    subcommand: asked.subcommand,
                    typed: asked.typed.clone(),
                    build_args: asked.build_args.clone(),
                    positional: asked.positional.clone(),
                    trailing: asked.trailing.clone(),
                    flags: cli::Flags {
                        manifest_path: asked.flags.manifest_path.clone(),
                        packages: vec![first_pkg.clone()],
                        workspace: asked.flags.workspace,
                        excludes: false,
                        excluded_packages: asked.flags.excluded_packages.clone(),
                        features: asked.flags.features.clone(),
                        all_features: asked.flags.all_features,
                        no_default_features: asked.flags.no_default_features,
                        profile: asked.flags.profile.clone(),
                        targets: asked.flags.targets.clone(),
                        bin: asked.flags.bin.clone(),
                        example: asked.flags.example.clone(),
                        message_format: asked.flags.message_format,
                        unstable: asked.flags.unstable,
                        quiet: asked.flags.quiet,
                        no_run: asked.flags.no_run,
                        no_fail_fast: asked.flags.no_fail_fast,
                        doc: asked.flags.doc,
                        selects_targets: asked.flags.selects_targets,
                        since: asked.flags.since.clone(),
                        explain: asked.flags.explain.clone(),
                        list_changed: asked.flags.list_changed,
                        backtest: asked.flags.backtest,
                        faults: asked.flags.faults,
                        junit: asked.flags.junit.clone(),
                        json: asked.flags.json,
                        also_run: asked.flags.also_run.clone(),
                        retries: asked.flags.retries,
                        full_tests: asked.flags.full_tests,
                    },
                };
                owned_asked.build_args.retain(|a| a != "--exclude");
                owned_asked.build_args.extend(["-p".to_owned(), first_pkg]);
                &owned_asked
            } else {
                asked
            }
        } else {
            asked
        }
    } else {
        asked
    };

    let key = asked.key(&cwd);
    let known = State::read(&key);
    if let Some(ref state) = known {
        prepare_since_cache(asked, state)?;
    }
    let (state, mut served_by_daemon) = match known.map(by_daemon) {
        Some(Ok(state)) => {
            let took = started.elapsed().as_secs_f64();
            let finished = format!("`{}` profile [rustd] target(s) in {took:.2}s", state.profile);
            say::status(asked.flags.quiet, "Finished", &finished);
            (state, true)
        }
        Some(Err((state, reason))) => {
            say::fallback(&reason, "building with cargo");
            let mut counts = state.counts;
            *counts.cargo.entry(reason).or_default() += 1;
            match by_cargo(asked, &cwd, &key, counts)? {
                Ok(state) => (state, false),
                Err(code) => return Ok(code),
            }
        }
        None => match by_cargo(asked, &cwd, &key, Counts::default())? {
            Ok(state) => (state, false),
            Err(code) => return Ok(code),
        },
    };
    if !served_by_daemon && asked.has_ci_test_flags() && state.refusal.is_none() {
        for _ in 0..2400 {
            if state
                .checkouts
                .iter()
                .filter(|it| !it.programs.is_empty())
                .all(|checkout| synced(checkout).is_ok())
            {
                served_by_daemon = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
    }
    state.write()?;
    after_build(asked, &state, &cwd, served_by_daemon)
}

/// Has the daemon bring the programs of `state` up to date and puts them where cargo has
/// them. `Err` is the state and why the build is cargo's.
fn by_daemon(mut state: State) -> Result<State, (State, String)> {
    let refused = state.refusal.clone().or_else(|| state.broken_guard());
    let refused = refused.or_else(|| {
        let mut asked = state.checkouts.iter().filter(|it| !it.programs.is_empty());
        asked.find_map(|checkout| synced(checkout).err())
    });
    let refused = refused.or_else(|| state.install().err().map(|it| format!("{it:#}")));
    match refused {
        Some(reason) => Err((state, reason)),
        None => {
            state.counts.served += 1;
            Ok(state)
        }
    }
}

/// Asks the daemon for the programs of `checkout`. `Err` is why it built none or not all.
fn synced(checkout: &Checkout) -> Result<(), String> {
    // The daemon knows the source files from when it was told to load them.
    let asked = Checkout { sources: Vec::new(), ..checkout.clone() };
    let reply = client::ask(Request::Sync(asked), Start::Never).map_err(|it| format!("{it:#}"))?;
    match reply {
        None => Err("no daemon is running".to_owned()),
        Some(Reply::Synced { programs }) => programs.into_iter().try_for_each(|it| it.map(drop)),
        Some(Reply::Cold { reason }) => Err(reason),
        Some(other) => Err(format!("the daemon answered {other:?}")),
    }
}

/// Tells the daemon to load `checkout` as cargo just built it, starting one when there is
/// none. `Err` is why it does not.
fn warmed(checkout: &Checkout) -> Result<(), String> {
    match client::ask(Request::Warm(checkout.clone()), Start::IfMissing) {
        Ok(Some(Reply::Cold { reason })) => Err(reason),
        Ok(_) => Ok(()),
        Err(why) => Err(format!("{why:#}")),
    }
}

/// Builds with cargo, and has the daemon load what cargo built for the builds to come.
/// `Err` is cargo's exit status when it failed.
fn by_cargo(
    asked: &Invocation,
    cwd: &Path,
    key: &str,
    counts: Counts,
) -> anyhow::Result<Result<State, i32>> {
    State::give_back(key);
    let built = match Built::by_cargo(asked)? {
        Ok(built) => built,
        Err(code) => return Ok(Err(code)),
    };
    let mut state = State::after(asked, cwd, key, built, counts)?;
    prepare_since_cache(asked, &state)?;
    if state.refusal.is_none() {
        let asked = state.checkouts.iter().filter(|it| !it.programs.is_empty());
        let cold: Vec<String> = asked.filter_map(|it| warmed(it).err()).collect();
        for why in cold {
            say::fallback(&why, "the daemon has no part in the next build");
            state.count_cargo(&why);
        }
    }
    Ok(Ok(state))
}

/// What the subcommand does once the programs are built.
fn after_build(
    asked: &Invocation,
    state: &State,
    cwd: &Path,
    served_by_daemon: bool,
) -> anyhow::Result<i32> {
    match asked.subcommand {
        Subcommand::Build => Ok(0),
        Subcommand::Run => match run::program(asked, state, cwd)? {
            Some(code) => Ok(code),
            // Cargo says which programs there are to pick from.
            None => {
                plain_cargo(std::iter::once("run".to_owned()).chain(asked.typed.clone()).collect())
            }
        },
        Subcommand::Test if asked.flags.no_run => Ok(0),
        Subcommand::Test => {
            let code = run::tests(asked, state, cwd, served_by_daemon)?;
            let stopped = code != 0 && !asked.flags.no_fail_fast;
            // Cargo 1.99 runs no doc tests when tests are picked by target or by name.
            let picked = asked.flags.selects_targets || !asked.positional.is_empty();
            if stopped || picked || !state.doc_tests {
                return Ok(code);
            }
            say::fallback("doc tests are built by rustdoc", "running `cargo test --doc`");
            Ok(code.max(run::doc_tests(asked)?))
        }
    }
}

/// `cargo rustd status`: the daemon, its workers, and how the builds of each checkout went.
fn status() -> anyhow::Result<i32> {
    use std::fmt::Write as _;
    let mut said = String::new();
    match client::ask(Request::Status, Start::Never)? {
        Some(Reply::Status(daemon)) => {
            let idle =
                daemon.idle_secs.map_or("never".to_owned(), |it| format!("after {it}s idle"));
            let _ = writeln!(
                said,
                "daemon: pid {}, version {}, exits {idle}",
                daemon.pid, daemon.version
            );
            for worker in daemon.workers {
                let _ = writeln!(
                    said,
                    "  worker: pid {}, {}, log {}",
                    worker.pid,
                    worker.project.display(),
                    worker.log.display()
                );
                for (root, warmth) in worker.checkouts {
                    let _ = writeln!(said, "    {}: {warmth}", root.display());
                }
            }
        }
        _ => said.push_str("daemon: not running\n"),
    }
    for state in State::all() {
        let to_cargo: u64 = state.counts.cargo.values().sum();
        let _ = writeln!(
            said,
            "{} ({}, {}): {} builds by the daemon, {to_cargo} by cargo",
            state.root.display(),
            state.profile,
            state.key,
            state.counts.served
        );
        for (reason, count) in &state.counts.cargo {
            let _ = writeln!(said, "  {count} x {reason}");
        }
        if let Some(refusal) = &state.refusal {
            let _ = writeln!(said, "  every build is cargo's: {refusal}");
        }
    }
    // Whoever reads this may stop reading before its end.
    let _ = std::io::Write::write_all(&mut std::io::stdout(), said.as_bytes());
    Ok(0)
}

/// `cargo rustd stop`: has the daemon stop its workers and exit.
fn stop() -> anyhow::Result<i32> {
    match client::ask(Request::Stop, Start::Never)? {
        Some(_) => println!("daemon: stopping"),
        None => println!("daemon: not running"),
    }
    Ok(0)
}
