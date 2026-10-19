//! `cargo rustd build|run|test|check`: cargo's commands, with the daemon building what
//! changed since cargo last built. `CARGO.md` at the root of the repository is the design.

pub(crate) mod daemon;
pub(crate) mod wrapper;

mod ancestor;
mod cli;
mod client;
mod package;
mod passed;
mod picked;
pub(crate) mod plan;
pub(crate) mod run;
mod say;
pub(crate) mod script;
mod state;
mod switch;
mod units;

use std::{
    collections::BTreeMap,
    path::Path,
    process::{Command, Stdio},
    time::Instant,
};

use anyhow::Context as _;

use cli::{Asked, Invocation, Subcommand};
use client::Start;
use daemon::{Checkout, Reply, Request};
use state::{Built, Counts, State};

/// `rustd cargo ...`: runs the command and ends with the status cargo would end with.
pub(crate) fn command(args: impl Iterator<Item = String>) -> anyhow::Result<()> {
    let args: Vec<String> = args.collect();
    if let [word, cargo, rest @ ..] = args.as_slice()
        && word == SETTLE
    {
        return settle(cargo.parse().context("`settle` is told the cargo to wait for")?, rest);
    }
    let asked = Asked::parse(args);
    if !matches!(asked, Asked::Build(_) | Asked::Cargo(_)) {
        crate::end_with_reader();
    }
    let code = match asked {
        Asked::Build(asked) => match asked.only_cargo() {
            None if asked.flags.doc => run::only_doc_tests(&asked.typed)?,
            Some(reason) => {
                say::fallback(&reason, &format!("this is `cargo {}`", asked.subcommand.name()));
                let typed = std::iter::once(asked.subcommand.name().to_owned()).chain(asked.typed);
                plain_cargo(typed.collect())?
            }
            None => build(&asked)?,
        },
        Asked::Cargo(args) => plain_cargo(args)?,
        Asked::Status => status()?,
        Asked::Stop => stop()?,
        Asked::Enable => switch::enable()?,
        Asked::Disable => switch::disable()?,
        Asked::Help => {
            println!("{HELP}");
            0
        }
    };
    std::process::exit(code)
}

const HELP: &str = "\
cargo rustd: builds with the rustd daemon, with cargo's flags

usage: cargo rustd <command> [cargo's flags]

  build, run, test   by the daemon when it has the workspace loaded, by cargo otherwise
  check              `cargo check`
  status             whether rustd is enabled, what the daemon has loaded, what fell back
  stop               ends the daemon; the next build starts one
  enable, disable    plain `cargo build|run|test|check` through rustd, and back

Any other command is cargo's own. CARGO.md is the guide.";

/// What a wrapper runs rustd with once rustc has linked a program under plain cargo.
pub(super) const SETTLE: &str = "settle";

/// Has the daemon load the build `typed` is the cargo command of, which the cargo `cargo`
/// is running: the build is asked of cargo again, which waits for that cargo to be done
/// with the target directory, has nothing left to compile and says what it built. This
/// process has that cargo's directory and environment. Nobody reads what it says but the
/// log.
fn settle(cargo: u32, typed: &[String]) -> anyhow::Result<()> {
    let settled = (|| {
        let asked = cli::of_cargo(typed).context("`settle` is told a build command")?;
        let cwd = std::env::current_dir()?;
        let key = asked.key(&cwd);
        eprintln!("settling {key}: cargo {} in {}", typed.join(" "), cwd.display());
        if let Ok(state) = by_cargo(&asked, &cwd, &key, Counts::default())? {
            state.write()?;
        }
        anyhow::Ok(())
    })();
    wrapper::settled(cargo);
    settled
}

/// Runs cargo with `args` and nothing of rustd's.
fn plain_cargo(args: Vec<String>) -> anyhow::Result<i32> {
    let status = Command::new(state::cargo_path()).args(args).status().context("running cargo")?;
    Ok(run::exit_code(status))
}

/// Builds what `asked` names, by the daemon when it can and by cargo otherwise, and then
/// runs what the subcommand runs.
fn build(asked: &Invocation) -> anyhow::Result<i32> {
    let started = Instant::now();
    let cwd = std::env::current_dir()?;
    let key = asked.key(&cwd);
    let known = State::read(&key);
    let mut served = false;
    // The tests to run, when the daemon told them before cargo built: it had read the edits
    // that sent the build to cargo.
    let mut picked_first = None;
    let state = match known.map(|state| by_daemon(state, asked)) {
        Ok(Err(Left::WithErrors(code))) => return Ok(code),
        Ok(Ok(state)) => {
            let took = started.elapsed().as_secs_f64();
            let finished = format!("`{}` profile [rustd] target(s) in {took:.2}s", state.profile);
            say::status(asked.flags.quiet, "Finished", &finished);
            served = true;
            state
        }
        Ok(Err(Left::ToCargo(state, reason))) => {
            say::fallback(&reason, "building with cargo");
            if asked.subcommand == Subcommand::Test && !asked.flags.no_run {
                picked_first = Some(picked::pick(asked, &state, true, true));
            }
            let mut counts = state.counts;
            *counts.cargo.entry(reason).or_default() += 1;
            match by_cargo(asked, &cwd, &key, counts)? {
                Ok(state) => state,
                Err(code) => return Ok(code),
            }
        }
        Err(unread) => {
            if let Some(why) = unread {
                say::fallback(why, "building with cargo");
            }
            match by_cargo(asked, &cwd, &key, Counts::default())? {
                Ok(state) => state,
                Err(code) => return Ok(code),
            }
        }
    };
    state.write()?;
    let picks = match (asked.subcommand, asked.flags.no_run) {
        (Subcommand::Test, false) => {
            Some(picked_first.unwrap_or_else(|| picked::pick(asked, &state, served, false)))
        }
        _ => None,
    };
    after_build(asked, &state, &cwd, picks)
}

/// Why the daemon's programs are not the build.
enum Left {
    /// The build is cargo's, with the state so far and the reason.
    ToCargo(Box<State>, String),
    /// `cargo check` turned the source down and said why; this is its exit status.
    WithErrors(i32),
}

/// Has the daemon bring the programs of `state` up to date and puts them where cargo has
/// them, once `cargo check` has passed the source as it is now: the daemon has no borrow
/// checker and builds programs rustc turns down, and the warnings are rustc's to give. A program the
/// daemon does not build stays as cargo built it while its source is what cargo compiled.
fn by_daemon(mut state: State, typed: &Invocation) -> Result<State, Left> {
    if let Some(reason) = state.refusal.clone().or_else(|| state.broken_guard()) {
        return Err(Left::ToCargo(Box::new(state), reason));
    }
    let checking = state.unchecked().then(|| (units::stamp_now(), check_of(typed).spawn()));
    let asked: Vec<Result<Vec<Result<(), String>>, String>> =
        state.checkouts.iter().map(synced).collect();
    if let Some((began, checking)) = checking {
        match checking.and_then(|mut check| check.wait()) {
            Ok(status) if status.success() => state.checked(began),
            Ok(status) => return Err(Left::WithErrors(run::exit_code(status))),
            Err(error) => {
                let reason = format!("`cargo check` did not run: {error}");
                return Err(Left::ToCargo(Box::new(state), reason));
            }
        }
    }
    let answers: Vec<Vec<Result<(), String>>> = std::iter::zip(&asked, &state.checkouts)
        .map(|(answer, checkout)| match answer {
            Ok(programs) => programs.clone(),
            Err(cold) => vec![Err(cold.clone()); checkout.programs.len()],
        })
        .collect();
    let cargos = match state.settle(&answers) {
        Ok(cargos) => cargos,
        Err(reason) => return Err(Left::ToCargo(Box::new(state), reason)),
    };
    let unchanged = "it is as cargo built it, and its source has not changed";
    for (answer, checkout) in std::iter::zip(&asked, &state.checkouts) {
        let (Err(cold), false) = (answer, checkout.programs.is_empty()) else { continue };
        // Loaded now, the daemon builds what changes next.
        match warmed(checkout) {
            Ok(()) => say::fallback(cold, "nothing has changed since cargo built"),
            Err(why) => say::fallback(&why, "nothing has changed since cargo built"),
        }
    }
    for (program, why) in &cargos {
        let asked_of_worker = program.asked_as.is_some_and(|(checkout, _)| asked[checkout].is_ok());
        if asked_of_worker {
            say::fallback(&format!("`{}`: {why}", program.name), unchanged);
        }
    }
    let served = cargos.len() < state.programs.len() || state.programs.is_empty();
    state.counts.served += u64::from(served);
    Ok(state)
}

/// Asks the daemon for the programs of `checkout`, and says how each went. `Err` is why it
/// has no part in any.
pub(super) fn synced(checkout: &Checkout) -> Result<Vec<Result<(), String>>, String> {
    if checkout.programs.is_empty() {
        return Ok(Vec::new());
    }
    // The daemon knows the source files from when it was told to load them.
    let asked = Checkout { sources: Vec::new(), ..asking(checkout) };
    let reply = client::ask(Request::Sync(asked), Start::Never).map_err(|it| format!("{it:#}"))?;
    match reply {
        None => Err(client::why_none()),
        Some(Reply::Synced { programs }) => {
            Ok(programs.into_iter().map(|it| it.map(drop)).collect())
        }
        Some(Reply::Cold { reason }) => Err(reason),
        Some(Reply::Restart { version }) => {
            Err(format!("the daemon running is another build of rustd ({version}), it is replaced"))
        }
        Some(other) => Err(format!("the daemon answered {other:?}")),
    }
}

/// `checkout` as a request names it: with the variables of this process, which `env!`
/// reads where cargo set none. The ones that make rustd the wrapper are cargo's alone.
fn asking(checkout: &Checkout) -> Checkout {
    let wraps = |name: &str| name == wrapper::DRIVEN || name == switch::VARIABLE;
    let mut env: BTreeMap<String, String> =
        std::env::vars().filter(|(name, _)| !wraps(name)).collect();
    env.extend(checkout.env.clone());
    Checkout { env, ..checkout.clone() }
}

/// Tells the daemon to load `checkout` as cargo just built it, starting one when there is
/// none. `Err` is why it does not.
fn warmed(checkout: &Checkout) -> Result<(), String> {
    match client::ask(Request::Warm(asking(checkout)), Start::IfMissing) {
        Ok(Some(Reply::Cold { reason })) => Err(reason),
        Ok(_) => Ok(()),
        Err(why) => Err(format!("{why:#}")),
    }
}

/// The `cargo check` of what `asked` builds: the same packages, targets, features and
/// profile, and for `test` the test targets with `cfg(test)`. It says what rustc has to
/// say and nothing of its own.
fn check_of(asked: &Invocation) -> Command {
    let mut check = Command::new(state::cargo_path());
    check.arg("check").args(&asked.build_args).stdin(Stdio::null()).stdout(Stdio::null());
    if !asked.flags.quiet {
        check.arg("--quiet");
    }
    if asked.subcommand == Subcommand::Test {
        if !asked.flags.selects_targets {
            check.arg("--tests");
        }
        if asked.flags.profile.is_none() {
            check.args(["--profile", "test"]);
        }
    }
    check
}

/// Has cargo check the dependencies of what `asked` builds while the daemon loads, so
/// that the check of the first build by the daemon has only the workspace to look at.
/// Nobody waits for it: a check that comes while it runs waits for cargo's lock.
fn check_ahead(asked: &Invocation) {
    let mut check = check_of(asked);
    check.stderr(Stdio::null());
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut check, 0);
    let _ = check.spawn();
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
    if state.refusal.is_none() {
        check_ahead(asked);
        let asked = state.checkouts.iter().filter(|it| !it.programs.is_empty());
        let cold: Vec<String> = asked.filter_map(|it| warmed(it).err()).collect();
        for why in cold {
            say::fallback(&why, "the daemon has no part in the next build");
            state.count_cargo(&why);
        }
        for (program, why) in state.left_to_cargo() {
            say::fallback(why, &format!("`{}` is cargo's to build", program.name));
        }
    }
    Ok(Ok(state))
}

/// What the subcommand does once the programs are built: for `test`, runs what `picks` says.
fn after_build(
    asked: &Invocation,
    state: &State,
    cwd: &Path,
    picks: Option<picked::Picks>,
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
            let picks = picks.unwrap_or_else(|| picked::Picks::all(state, None));
            let code = run::tests(asked, state, cwd, &picks)?;
            let stopped = code != 0 && !asked.flags.no_fail_fast;
            // Cargo 1.99 runs no doc tests when tests are picked by target or by name.
            let picked = asked.flags.selects_targets || !asked.positional.is_empty();
            if stopped || picked || !state.doc_tests {
                if code == 0 {
                    picks.passed();
                }
                return Ok(code);
            }
            let code = code.max(run::doc_tests(asked, picks.doc_packages.as_deref())?);
            if code == 0 {
                picks.passed();
            }
            Ok(code)
        }
    }
}

/// `cargo rustd status`: the daemon, its workers, and how the builds of each checkout went.
fn status() -> anyhow::Result<i32> {
    use std::fmt::Write as _;
    let mut said = String::new();
    for line in switch::status(&std::env::current_dir()?).into_iter().chain(wrapper::tallied()) {
        let _ = writeln!(said, "{line}");
    }
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
        Some(Reply::Restart { version }) => {
            let _ = writeln!(
                said,
                "daemon: another build of rustd, version {version}; the next build replaces it"
            );
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
        Some(_) => {
            client::gone(&daemon::socket_in(&daemon::home()?));
            println!("daemon: stopped");
        }
        None => println!("daemon: not running"),
    }
    Ok(0)
}
