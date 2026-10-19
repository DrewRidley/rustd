//! The daemon of a user: one process that every `cargo rustd` of the user finds through one
//! socket. It loads nothing itself. It owns the workers, each a `rustd <project> --socket`
//! process with one database, starts them when a build asks, passes the builds on to them,
//! and stops them and itself when nobody has asked for a while.

use std::{
    collections::BTreeMap,
    fs::OpenOptions,
    io::{BufRead as _, BufReader, Write as _},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::{Duration, Instant, SystemTime},
};

use anyhow::Context as _;
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

#[cfg(unix)]
use std::os::unix::net::UnixStream;
#[cfg(windows)]
use uds_windows::UnixStream;

use super::{plan::Plan, units::stamp_of};
use crate::socket::{self, NotServed, Served, private_dir, private_file, read_request};

/// How long the daemon and each worker live without being asked, unless
/// `RUSTD_IDLE_SECS` says otherwise.
const IDLE: Duration = Duration::from_secs(1800);

/// How often a worker that is loading is looked at.
const POLL: Duration = Duration::from_millis(20);

/// The variable that has a worker end when its standard input does, which is when the
/// daemon that started it is gone.
pub(crate) const ENDS_WITH_INPUT: &str = "RUSTD_ENDS_WITH_INPUT";

/// A request with the version of the client that sends it.
#[derive(Serialize, Deserialize)]
pub(crate) struct Envelope {
    pub(crate) version: String,
    /// Whether the client starts a daemon of its own version when this one is of another.
    pub(crate) replaces: bool,
    pub(crate) request: Request,
}

#[derive(Serialize, Deserialize)]
pub(crate) enum Request {
    /// Bring the programs of a checkout up to date with its source, if the daemon is warm
    /// for it.
    Sync(Checkout),
    /// Load a checkout that cargo has just built, for the builds to come.
    Warm(Checkout),
    /// Say which tests of each program of a checkout the changes written to a file reach,
    /// as the programs were last synced.
    Select(Checkout, PathBuf),
    Status,
    Stop,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) enum Reply {
    /// The daemon is of another version. It exits when the client said it replaces it.
    Restart {
        version: String,
    },
    /// The daemon has no part in this build, and why. Cargo builds.
    Cold {
        reason: String,
    },
    /// What the worker said of each program, in the order asked: its `ok` line, or why it
    /// did not build it.
    Synced {
        programs: Vec<Result<String, String>>,
    },
    /// What the worker said of each program, in the order asked: its `ok` line with what of
    /// it to run, or why it can't tell.
    Selected {
        programs: Vec<Result<String, String>>,
    },
    Warming,
    Status(Status),
    Stopping,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct Status {
    pub(crate) pid: u32,
    pub(crate) version: String,
    pub(crate) idle_secs: Option<u64>,
    pub(crate) workers: Vec<WorkerStatus>,
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct WorkerStatus {
    pub(crate) pid: u32,
    pub(crate) project: PathBuf,
    pub(crate) log: PathBuf,
    /// Each checkout the worker holds and how warm it is.
    pub(crate) checkouts: Vec<(PathBuf, String)>,
}

/// What a worker is loaded with. Two builds with the same one share a worker.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub(crate) struct WorkerConfig {
    /// What the checkouts of one repository have in common: the directory of its `.git`.
    pub(crate) repository: PathBuf,
    /// The directory of the package being built, from the root of the checkout.
    pub(crate) project: PathBuf,
    pub(crate) cfg_test: bool,
    /// Which build the worker stands in for, as the daemon's own plan of it has it.
    pub(crate) selection: crate::plan::Selection,
    /// The variables the worker runs with that decide what it compiles and with what.
    pub(crate) env: BTreeMap<String, String>,
}

impl WorkerConfig {
    fn id(&self) -> String {
        let text = serde_json::to_vec(self).unwrap_or_default();
        Sha256::digest(text).iter().take(16).map(|it| format!("{it:02x}")).collect()
    }
}

/// A checkout as cargo last built it, and the programs of it the daemon is asked for.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub(crate) struct Checkout {
    /// The build this is: the checkout, the command and its flags.
    pub(crate) key: String,
    /// The newest of rustc's output the programs are linked against. A worker that loaded
    /// the checkout with another one starts again.
    pub(crate) generation: u128,
    /// When cargo started, as a stamp: source newer than this is not what rustc compiled.
    pub(crate) built_at: u128,
    pub(crate) config: WorkerConfig,
    /// The root of the checkout.
    pub(crate) root: PathBuf,
    pub(crate) programs: Vec<Plan>,
    /// The source files rustc read. Only a request to warm names them.
    pub(crate) sources: Vec<PathBuf>,
    /// The variables `env!` reads in the checkout: the ones cargo set to say where its
    /// programs are, and in a request under them the ones of the process that asks.
    #[serde(default)]
    pub(crate) env: BTreeMap<String, String>,
}

impl Checkout {
    fn project(&self) -> PathBuf {
        self.root.join(&self.config.project)
    }

    /// The first source file that was written after cargo started.
    fn newer_source(&self) -> Option<&Path> {
        let newer = |path: &&PathBuf| {
            let written = std::fs::metadata(path).and_then(|it| it.modified());
            written.is_ok_and(|it| stamp_of(it) > self.built_at)
        };
        self.sources.iter().find(newer).map(PathBuf::as_path)
    }

    /// The line that asks a worker for `plan`.
    fn sync_line(&self, plan: &Plan) -> String {
        self.line("sync", plan)
    }

    /// The line that asks a worker which tests of `plan` the changes in `changes` reach.
    fn select_line(&self, plan: &Plan, changes: &Path) -> String {
        self.line(&format!("select\t{}", changes.display()), plan)
    }

    /// The line of a request that names `plan`, after the words of `request`.
    fn line(&self, request: &str, plan: &Plan) -> String {
        let named =
            |path: &PathBuf| path.strip_prefix(&self.root).unwrap_or(path).display().to_string();
        // Each with a comma after it: the program is of these crates and no others.
        let crates: String = plan.crates.iter().map(|it| named(it) + ",").collect();
        let mut line = format!(
            "{request}\t{}\t{}\t{}\t{}\t{}\t{}",
            self.project().display(),
            crates,
            plan.object.display(),
            plan.patch.display(),
            plan.link_cmd,
            plan.binary.display()
        );
        for file in &plan.symbol_files {
            line.push('\t');
            line.push_str(&file.display().to_string());
        }
        line.push('\n');
        line
    }
}

/// How far a worker is with a checkout.
#[derive(Clone)]
enum Warmth {
    Loading(Instant),
    Ready,
    /// Why the worker has no part in the checkout as cargo last built it.
    Failed(String),
}

impl Warmth {
    fn describe(&self) -> String {
        match self {
            Warmth::Loading(since) => format!("loading for {:.0?}", since.elapsed()),
            Warmth::Ready => "ready".to_owned(),
            Warmth::Failed(why) => format!("failed: {why}"),
        }
    }
}

struct Loaded {
    generation: u128,
    root: PathBuf,
    warmth: Warmth,
}

/// A `rustd <project> --socket` process and the checkouts it holds.
struct Worker {
    project: PathBuf,
    socket: PathBuf,
    log: PathBuf,
    /// The variables the process runs with. A request says them with the ones of its
    /// checkout over them.
    env: ide_db::base_db::Env,
    child: Mutex<Child>,
    checkouts: Mutex<FxHashMap<String, Loaded>>,
    asked: Mutex<Instant>,
}

impl Worker {
    /// Starts a worker on the first program of `checkout`, with its files in `dir`.
    fn start(checkout: &Checkout, dir: PathBuf) -> anyhow::Result<Worker> {
        let first = checkout.programs.first().context("a checkout without programs")?;
        private_dir(&dir)?;
        let (socket, log) = (dir.join("sock"), dir.join("log"));
        let config = &checkout.config;
        let mut command = Command::new(std::env::current_exe()?);
        command.arg(checkout.project()).arg("--own-target");
        match store() {
            Some(store) => command.env("RUSTD_CACHE", store),
            None => command.env_remove("RUSTD_CACHE"),
        };
        command.args(config.selection.flags());
        if config.cfg_test {
            command.arg("--cfg-test");
        }
        if config.selection.dev_deps {
            command.arg("--dev-deps");
        }
        for krate in &first.crates {
            command.arg("--crate").arg(krate.strip_prefix(&checkout.root).unwrap_or(krate));
        }
        command.arg("--out").arg(&first.object).arg("--patch").arg(&first.patch);
        command.arg("--link-cmd").arg(&first.link_cmd).arg("--splice").arg(&first.binary);
        command.arg("--socket").arg(&socket);
        // In a file: a program of many crates has more of them than fit the command line
        // of a process on Windows.
        let symbols = dir.join("symbols");
        let listed: String =
            first.symbol_files.iter().map(|it| format!("{}\n", it.display())).collect();
        std::fs::write(&symbols, listed)?;
        command.arg("--symbols-in").arg(&symbols);
        let output =
            private_file(OpenOptions::new().write(true).create(true).truncate(true), &log)?;
        command.envs(&config.env).env(ENDS_WITH_INPUT, "1");
        let mut env = crate::process::own();
        for (name, value) in &config.env {
            env.set(name, value);
        }
        env.set(ENDS_WITH_INPUT, "1");
        command.current_dir(&checkout.root);
        command.stdin(Stdio::piped()).stdout(output.try_clone()?).stderr(output);
        let child = command.spawn().context("starting a worker")?;
        let loading = Loaded {
            generation: checkout.generation,
            root: checkout.root.clone(),
            warmth: Warmth::Loading(Instant::now()),
        };
        Ok(Worker {
            project: checkout.project(),
            socket,
            log,
            env,
            child: Mutex::new(child),
            checkouts: Mutex::new(FxHashMap::from_iter([(checkout.key.clone(), loading)])),
            asked: Mutex::new(Instant::now()),
        })
    }

    /// Whether the process has ended, and how.
    fn ended(&self) -> Option<String> {
        let status = lock(&self.child).try_wait();
        match status {
            Ok(None) => None,
            Ok(Some(status)) => Some(format!("{status}: {}", self.last_words())),
            Err(error) => Some(error.to_string()),
        }
    }

    /// The last line the worker logged that says what went wrong.
    fn last_words(&self) -> String {
        let log = std::fs::read_to_string(&self.log).unwrap_or_default();
        let last = log.lines().rev().find(|it| it.starts_with("Error") || it.contains("panicked"));
        last.or(log.lines().last()).unwrap_or("no output").to_owned()
    }

    fn stop(&self) {
        let mut child = lock(&self.child);
        // One that has exited already has nothing to kill.
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_file(&self.socket);
    }

    fn set(&self, key: &str, warmth: Warmth) {
        if let Some(loaded) = lock(&self.checkouts).get_mut(key) {
            loaded.warmth = warmth;
        }
    }

    /// Waits for the process to listen, then has it build every program of `checkout` once.
    /// `started_on` is whether the worker was started on the checkout's first program, which
    /// is then built already.
    fn warm(&self, checkout: &Checkout, started_on: bool) {
        let warmth = match self.warmed(checkout, started_on) {
            Ok(()) => Warmth::Ready,
            Err(why) => Warmth::Failed(format!("{why:#}")),
        };
        eprintln!("{}: {}", checkout.root.display(), warmth.describe());
        self.set(&checkout.key, warmth);
        // Nobody could ask it for a build until now.
        *lock(&self.asked) = Instant::now();
    }

    /// Whether a checkout is still being loaded.
    fn loads(&self) -> bool {
        lock(&self.checkouts).values().any(|it| matches!(it.warmth, Warmth::Loading(_)))
    }

    fn warmed(&self, checkout: &Checkout, started_on: bool) -> anyhow::Result<()> {
        while socket::connect(&self.socket).is_err() {
            if let Some(how) = self.ended() {
                anyhow::bail!("the daemon could not load the workspace ({how})");
            }
            std::thread::sleep(POLL);
        }
        let stale = |file: &Path| {
            anyhow::anyhow!("{} changed while cargo built or the daemon loaded", file.display())
        };
        if let Some(file) = checkout.newer_source() {
            return Err(stale(file));
        }
        for plan in checkout.programs.iter().skip(usize::from(started_on)) {
            self.ask(checkout, plan)?.map_err(anyhow::Error::msg)?;
        }
        Ok(())
    }

    /// Asks the process for `plan` of `checkout`. The outer error is the worker's failing,
    /// the inner one its answer that it does not build this.
    fn ask(&self, checkout: &Checkout, plan: &Plan) -> anyhow::Result<Result<String, String>> {
        self.ask_line(checkout, &checkout.sync_line(plan))
    }

    /// Sends `line` about `checkout` to the process and reads its answer, as [`Worker::ask`].
    fn ask_line(&self, checkout: &Checkout, line: &str) -> anyhow::Result<Result<String, String>> {
        let failed = || match self.ended() {
            Some(how) => format!("the daemon's worker ended ({how})"),
            None => "the daemon's worker did not answer".to_owned(),
        };
        let mut stream = socket::connect(&self.socket).with_context(failed)?;
        let mut env = self.env.clone();
        for (name, value) in &checkout.env {
            env.set(name, value);
        }
        stream.write_all(line.as_bytes()).with_context(failed)?;
        stream
            .write_all(format!("{}\n", crate::process::encode(&env)).as_bytes())
            .with_context(failed)?;
        let mut reply = String::new();
        BufReader::new(stream).read_line(&mut reply).with_context(failed)?;
        let reply = reply.trim_end();
        match reply.strip_prefix("error: ") {
            _ if reply.is_empty() => Err(anyhow::anyhow!(failed())),
            _ if reply.starts_with("ok") => Ok(Ok(reply.to_owned())),
            Some(why) => Ok(Err(why.to_owned())),
            None => Ok(Err(reply.to_owned())),
        }
    }

    /// Brings the programs of `checkout` up to date, all at once: the worker builds the
    /// requests that wait for it together.
    fn sync(&self, checkout: &Checkout) -> Reply {
        let asked: Vec<anyhow::Result<Result<String, String>>> = std::thread::scope(|scope| {
            let asking: Vec<_> = checkout
                .programs
                .iter()
                .map(|plan| scope.spawn(|| self.ask(checkout, plan)))
                .collect();
            let panicked = || Err(anyhow::anyhow!("asking the worker panicked"));
            asking.into_iter().map(|it| it.join().unwrap_or_else(|_| panicked())).collect()
        });
        match asked.into_iter().collect::<anyhow::Result<Vec<_>>>() {
            Ok(programs) => Reply::Synced { programs },
            // The worker is gone or stuck, which says nothing of the checkout: the next
            // build that has nothing to compile has it loaded again.
            Err(why) => {
                lock(&self.checkouts).remove(&checkout.key);
                Reply::Cold { reason: format!("{why:#}") }
            }
        }
    }

    /// Asks which tests of each program of `checkout` the changes in `changes` reach.
    fn select(&self, checkout: &Checkout, changes: &Path) -> Reply {
        let asked: Vec<Result<String, String>> = checkout
            .programs
            .iter()
            .map(|plan| match self.ask_line(checkout, &checkout.select_line(plan, changes)) {
                Ok(answer) => answer,
                Err(why) => Err(format!("{why:#}")),
            })
            .collect();
        Reply::Selected { programs: asked }
    }

    /// Whether it is loading a checkout.
    fn loading(&self) -> bool {
        lock(&self.checkouts).values().any(|it| matches!(it.warmth, Warmth::Loading(_)))
    }

    fn status(&self) -> WorkerStatus {
        let checkouts = lock(&self.checkouts);
        WorkerStatus {
            pid: lock(&self.child).id(),
            project: self.project.clone(),
            log: self.log.clone(),
            checkouts: checkouts
                .values()
                .map(|it| (it.root.clone(), it.warmth.describe()))
                .collect(),
        }
    }
}

/// The daemon: its version, its workers, and when it was last asked.
struct Daemon {
    version: String,
    home: PathBuf,
    idle: Option<Duration>,
    workers: Mutex<FxHashMap<String, Arc<Worker>>>,
    asked: Mutex<Instant>,
}

/// Where the daemon of this user keeps its socket, its lock, its log and its workers.
pub(crate) fn home() -> anyhow::Result<PathBuf> {
    if let Some(home) = std::env::var_os("RUSTD_HOME").filter(|it| !it.is_empty()) {
        return Ok(PathBuf::from(home));
    }
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR").filter(|it| !it.is_empty()) {
        return Ok(Path::new(&runtime).join("rustd"));
    }
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    Ok(Path::new(&home.context("no RUSTD_HOME, XDG_RUNTIME_DIR or HOME")?).join(".cache/rustd"))
}

/// The disk store of the user's builds: the directory `RUSTD_CACHE` names, none when it
/// says `off`, and without it `rustd/store` in the user's cache directory. Unlike `home`
/// it is not in `XDG_RUNTIME_DIR`, which does not outlive the session.
pub(crate) fn store() -> Option<PathBuf> {
    if let Some(named) = std::env::var_os("RUSTD_CACHE") {
        return (!hir_ty::mir::codegen::store_is_off(&named)).then(|| PathBuf::from(named));
    }
    let caches = std::env::var_os("XDG_CACHE_HOME").filter(|it| !it.is_empty()).map(PathBuf::from);
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    let caches = caches.or_else(|| Some(PathBuf::from(home?).join(".cache")))?;
    Some(caches.join("rustd/store"))
}

pub(crate) fn socket_in(home: &Path) -> PathBuf {
    home.join("daemon.sock")
}

/// What two processes have to agree on to talk: the version of the crate and the executable
/// itself, so that a daemon of an older build of the same version is replaced too.
pub(crate) fn version() -> String {
    let executable = std::env::current_exe().and_then(std::fs::metadata);
    let built = executable.map(|it| (it.len(), it.modified().map_or(0, stamp_of)));
    let (len, written) = built.unwrap_or_default();
    format!("{}-{len:x}-{written:x}", env!("CARGO_PKG_VERSION"))
}

/// `rustd daemon`: serves the user's builds until it has been idle for long enough or is
/// told to stop. It starts:
///
/// 1. the thread that accepts connections, and a thread per connection that answers its
///    one request;
/// 2. the thread that stops idle workers and ends the daemon when it is idle itself;
/// 3. per worker, the process, and a thread that waits for it to load and has it build
///    its programs once.
pub(crate) fn serve() -> anyhow::Result<()> {
    let home = home()?;
    private_dir(&home).with_context(|| format!("the daemon's directory {}", home.display()))?;
    let socket = socket_in(&home);
    let served = match Served::bind(&socket) {
        Ok(served) => served,
        // Another daemon is the user's: the client that started this one finds it.
        Err(NotServed::Taken) => return Ok(()),
        Err(NotServed::Failed(error)) => {
            return Err(error).with_context(|| format!("listening on {}", socket.display()));
        }
    };
    let idle = match std::env::var("RUSTD_IDLE_SECS").ok().and_then(|it| it.parse::<u64>().ok()) {
        Some(0) => None,
        Some(secs) => Some(Duration::from_secs(secs)),
        None => Some(IDLE),
    };
    let daemon = Arc::new(Daemon {
        version: version(),
        home,
        idle,
        workers: Mutex::default(),
        asked: Mutex::new(Instant::now()),
    });
    eprintln!(
        "daemon {} of pid {} listens on {}",
        daemon.version,
        std::process::id(),
        socket.display()
    );
    {
        let daemon = daemon.clone();
        std::thread::spawn(move || daemon.reap());
    }
    for (stream, open) in served.incoming() {
        let daemon = daemon.clone();
        std::thread::spawn(move || {
            let _open = open;
            if let Err(why) = daemon.answer(stream) {
                eprintln!("a request is not answered: {why:#}");
            }
        });
    }
    Ok(())
}

impl Daemon {
    /// Reads the one request of a connection and answers it.
    fn answer(&self, stream: UnixStream) -> anyhow::Result<()> {
        let line = read_request(&stream)?;
        let envelope = serde_json::from_str::<Envelope>(&line);
        // A request that can't be read is one of another version too, and of a client
        // that waits for an answer it can read: it gets none, and the daemon stays.
        let replaced = envelope.as_ref().is_ok_and(|it| it.replaces && it.version != self.version);
        let reply = match envelope {
            Ok(envelope) if envelope.version == self.version => self.handle(envelope.request),
            _ => Reply::Restart { version: self.version.clone() },
        };
        let mut text = serde_json::to_string(&reply)?;
        text.push('\n');
        let sent = (&stream).write_all(text.as_bytes());
        if replaced || matches!(reply, Reply::Stopping) {
            self.end();
        }
        Ok(sent?)
    }

    fn handle(&self, request: Request) -> Reply {
        *lock(&self.asked) = Instant::now();
        match request {
            Request::Sync(checkout) => self.sync(&checkout),
            Request::Select(checkout, changes) => self.select(&checkout, &changes),
            Request::Warm(checkout) => match self.warm(checkout) {
                Ok(()) => Reply::Warming,
                Err(why) => Reply::Cold { reason: format!("{why:#}") },
            },
            Request::Status => Reply::Status(Status {
                pid: std::process::id(),
                version: self.version.clone(),
                idle_secs: self.idle.map(|it| it.as_secs()),
                workers: lock(&self.workers).values().map(|it| it.status()).collect(),
            }),
            Request::Stop => Reply::Stopping,
        }
    }

    fn select(&self, checkout: &Checkout, changes: &Path) -> Reply {
        let worker = lock(&self.workers).get(&checkout.config.id()).cloned();
        let Some(worker) = worker else {
            return Reply::Cold { reason: "the daemon has not loaded this workspace".to_owned() };
        };
        let warmth = match lock(&worker.checkouts).get(&checkout.key) {
            Some(loaded) if loaded.generation == checkout.generation => loaded.warmth.clone(),
            Some(_) => Warmth::Failed("the daemon has another build of the checkout".to_owned()),
            None => Warmth::Failed("the daemon has not loaded this build".to_owned()),
        };
        match warmth {
            Warmth::Ready => worker.select(checkout, changes),
            Warmth::Loading(_) => Reply::Cold { reason: "the daemon is still loading".to_owned() },
            Warmth::Failed(reason) => Reply::Cold { reason },
        }
    }

    fn sync(&self, checkout: &Checkout) -> Reply {
        let worker = lock(&self.workers).get(&checkout.config.id()).cloned();
        let Some(worker) = worker else {
            return Reply::Cold { reason: "the daemon has not loaded this workspace".to_owned() };
        };
        *lock(&worker.asked) = Instant::now();
        let warmth = match lock(&worker.checkouts).get(&checkout.key) {
            Some(loaded) if loaded.generation == checkout.generation => loaded.warmth.clone(),
            _ => Warmth::Failed("the daemon has not loaded this build of the workspace".to_owned()),
        };
        match warmth {
            Warmth::Ready => worker.sync(checkout),
            Warmth::Loading(since) => Reply::Cold {
                reason: format!(
                    "the daemon is still loading the workspace ({:.0?} so far)",
                    since.elapsed()
                ),
            },
            Warmth::Failed(reason) => Reply::Cold { reason },
        }
    }

    /// Has a worker load `checkout`: the one of its configuration when that has not loaded
    /// another build of the same checkout, and a new one otherwise.
    fn warm(&self, checkout: Checkout) -> anyhow::Result<()> {
        let id = checkout.config.id();
        let mut workers = lock(&self.workers);
        // What a worker could not load once it can't load again, until cargo builds anew.
        let failed = workers.get(&id).and_then(|it| match lock(&it.checkouts).get(&checkout.key) {
            Some(Loaded { generation, warmth: Warmth::Failed(why), .. })
                if *generation == checkout.generation =>
            {
                Some(why.clone())
            }
            _ => None,
        });
        if let Some(why) = failed {
            anyhow::bail!(why);
        }
        let running = workers.get(&id).filter(|it| it.ended().is_none()).cloned();
        if let Some(worker) = &running {
            let mut checkouts = lock(&worker.checkouts);
            match checkouts.get(&checkout.key) {
                Some(loaded) if loaded.generation == checkout.generation => return Ok(()),
                // What it linked against is not what cargo has now.
                Some(_) => {}
                None => {
                    let loading = Loaded {
                        generation: checkout.generation,
                        root: checkout.root.clone(),
                        warmth: Warmth::Loading(Instant::now()),
                    };
                    checkouts.insert(checkout.key.clone(), loading);
                    let worker = worker.clone();
                    std::thread::spawn(move || worker.warm(&checkout, false));
                    return Ok(());
                }
            }
        }
        if let Some(old) = workers.remove(&id) {
            old.stop();
        }
        let worker = Arc::new(Worker::start(&checkout, self.home.join("workers").join(&id))?);
        eprintln!(
            "worker {id} of pid {} for {}",
            lock(&worker.child).id(),
            worker.project.display()
        );
        workers.insert(id, worker.clone());
        std::thread::spawn(move || worker.warm(&checkout, true));
        Ok(())
    }

    /// Stops the workers nobody has asked for the idle time, and the daemon when nobody has
    /// asked it.
    fn reap(&self) {
        let Some(idle) = self.idle else { return };
        loop {
            std::thread::sleep(idle.min(Duration::from_secs(1)));
            // Loading is what the last build asked for: the idle time starts after it.
            for worker in lock(&self.workers).values().filter(|it| it.loading()) {
                *lock(&worker.asked) = Instant::now();
                *lock(&self.asked) = Instant::now();
            }
            if lock(&self.asked).elapsed() >= idle {
                eprintln!("idle for {idle:.0?}");
                self.end();
            }
            lock(&self.workers).retain(|_, worker| {
                // Loading can take longer than the idle time, and is what was asked for.
                let keep = worker.loads() || lock(&worker.asked).elapsed() < idle;
                if !keep {
                    worker.stop();
                }
                keep
            });
        }
    }

    /// Stops the workers and the daemon.
    fn end(&self) -> ! {
        for worker in lock(&self.workers).values() {
            worker.stop();
        }
        let _ = std::fs::remove_file(socket_in(&self.home));
        eprintln!("daemon of pid {} ends at {:?}", std::process::id(), SystemTime::now());
        std::process::exit(0)
    }
}

/// The lock, also after a thread panicked with it: what it guards is whole between any two
/// statements here.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
