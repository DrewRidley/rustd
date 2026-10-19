#!/usr/bin/env python3
"""cargo_rustd.py: `cargo rustd` against plain cargo on real projects, unmodified.

    cargo_rustd.py [PROJECT...] [--small] [--only-projects] [--only-checks] [--rustd] [--plain] [--keep]

Four parts, all of which have to pass.

Projects. Each one below is cloned at its tag, which has to be the commit it is pinned to, into `RUSTD_PROJECTS` (~/rustd-projects) when
it is not there, and copied; the clone is never written to. In the copy, with plain cargo
building into a target directory of its own as the reference:
  cold      `cargo rustd build` from nothing, with no daemon; the program is run
  warm      once the daemon has loaded the workspace, `cargo rustd build` again; the program,
            which is now the one the daemon linked, is run
  test      `cargo rustd test`, cold and again warm
  edit      the source is edited; `cargo rustd build`, the program is run, `cargo rustd test`
  broken    a type error is added; both have to fail with the same status and the same error
After every step the program's output and exit status and the result of every test have to be
what plain cargo gives for the same source. A build cargo did in place of the daemon passes
(that is the contract) and is in the table with the reason.

Every project is run twice, in a fresh copy each time: with `cargo rustd`, and with plain
cargo in a cargo home in which `rustd enable` was run. `--rustd` or `--plain` runs the one.
Then a second checkout of the repository is built, edited and built again, and the
functions the daemon compiled itself are counted.

Flags. A small workspace made here is built and run with each flag `cargo rustd` passes
through, cold and after an edit, and compared with cargo the same way.

Picks. In a workspace made here, a failure planted where one kind of test alone reaches it
(a unit test, an integration test, a target without the harness, a doc test, a test of a
downstream crate, an impl called through `dyn`, a new test) is run and fails while the tests
it does not reach are not run, and an edit to `build.rs`, a manifest, an `include_str!` file
or a `macro_rules!` runs every test. A test step of a project or of the flags compares as a
subset of cargo's tests with the same outcomes, among them every test that failed.

Daemon. Started on demand, reused, shared by a git worktree, alive after its client is
killed, replaced when the client is another build of rustd, gone when idle, and its workers
gone with it.

Plain cargo. `rustd enable` and `rustd disable` over a cargo configuration that is there,
that is not, and that is edited in between; a wrapper the user had; what a project's own
configuration and the environment do; and plain `cargo build`, `run`, `test`, `check`,
`install` and doc tests with the daemon there, killed, and of another version.

Writes CARGO-RUSTD.md and cargo_rustd.json next to this file, or CARGO-RUSTD-LLVM.md and
cargo_rustd_llvm.json under `RUSTD_BACKEND=llvm`. A run of some projects replaces their rows
and keeps the others. `--small` is the three projects of the CI job.
"""
import collections, json, os, re, shutil, signal, subprocess, sys, tempfile, time, urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
# `difftest` turns incremental compilation off for the harnesses that build with rustc
# alone. Here it stays as the caller has it: without it the tests of every project are
# cargo's, and that case is one of the flags.
INCREMENTAL = os.environ.get("CARGO_INCREMENTAL")
from difftest import host
if INCREMENTAL is None:
    os.environ.pop("CARGO_INCREMENTAL", None)
else:
    os.environ["CARGO_INCREMENTAL"] = INCREMENTAL
ROOT = os.path.dirname(HERE)
WINDOWS = os.name == "nt"
EXE = ".exe" if WINDOWS else ""
RUSTD = os.environ.get("RUSTD_BIN") or os.path.join(ROOT, "target/release/rustd" + EXE)
TOOLCHAIN = open(os.path.join(HERE, "toolchain")).read().strip()
SOURCES = os.environ.get("RUSTD_PROJECTS") or os.path.expanduser("~/rustd-projects")
WARMUP_SECS = 1800

PROJECTS = {
    "ripgrep": dict(
        repo="BurntSushi/ripgrep", tag="15.1.0", rev="af60c2de9d85e7f3d81c78601669468cf02dabab",
        run=["-n", "--no-heading", "--sort", "path", "fn main", "crates/core/main.rs", "build.rs"],
        edits=[("crates/core/main.rs", "fn main() -> ExitCode {", 'fn main() -> ExitCode {\n    eprintln!("edited main");'),
               ("crates/printer/src/standard.rs", "pub fn new() -> StandardBuilder {", 'pub fn new() -> StandardBuilder {\n        eprintln!("edited printer");')],
    ),
    "fd": dict(
        repo="sharkdp/fd", tag="v10.3.0", rev="d38148f0aabdd073b4080cde770f679f3197b920",
        run=["--extension", "rs", "main", "src"],
        edits=[("src/main.rs", "fn main() {", 'fn main() {\n    eprintln!("edited main");')],
    ),
    "bat": dict(
        repo="sharkdp/bat", tag="v0.26.0", rev="c734087e1d15fb730d7653389c841fd9e3016556",
        run=["--plain", "--color=never", "rustfmt.toml"],
        edits=[("src/bin/bat/main.rs", "fn main() {", 'fn main() {\n    eprintln!("edited main");')],
    ),
    "hyperfine": dict(
        repo="sharkdp/hyperfine", tag="v1.19.0", rev="12fec42098642a19855ead34c8cb1e0be28c8ead",
        run=["--help"],
        edits=[("src/main.rs", "fn main() {", 'fn main() {\n    eprintln!("edited main");')],
    ),
    "just": dict(
        repo="casey/just", tag="1.43.0", rev="b7e7c46495b34e9407982ffbbb25eb2808ec40b7",
        run=["--list", "--unsorted"],
        edits=[("src/main.rs", "fn main() {", 'fn main() {\n  eprintln!("edited main");')],
    ),
    "axum": dict(
        repo="tokio-rs/axum", tag="axum-v0.8.6", rev="c1bb9c3e90625fb8de4f9c9750f669cb4edac3df",
        build=["-p", "example-hello-world"], serves="http://127.0.0.1:3000/",
        test=["-p", "example-testing"],
        edits=[("examples/hello-world/src/main.rs", "<h1>Hello, World!</h1>", "<h1>Hello, edited world!</h1>"),
               ("examples/testing/src/main.rs", "fn app() -> Router {", 'fn app() -> Router {\n    eprintln!("edited app");')],
        broken="examples/hello-world/src/main.rs",
    ),
    "nushell": dict(
        repo="nushell/nushell", tag="0.108.0", rev="da141be11b3acaa55b39321e5f9651d02aa2ed6f",
        run=["-n", "-c", "[3 1 2] | sort | str join ','"],
        test=["-p", "nu-path"],
        edits=[("src/main.rs", "fn main() -> Result<()> {", 'fn main() -> Result<()> {\n    eprintln!("edited main");')],
    ),
    "cargo": dict(
        repo="rust-lang/cargo", tag="0.92.0", rev="ea2d97820c16195b0ca3fadb4319fe512c199a43",
        run=["metadata", "--format-version", "1", "--no-deps", "--offline", "--manifest-path", "crates/cargo-platform/Cargo.toml"],
        test=["-p", "cargo-platform"],
        edits=[("src/bin/cargo/main.rs", "fn main() {", 'fn main() {\n    eprintln!("edited main");')],
    ),
    # The unit tests of cargo's own library, which is most of cargo.
    "cargo-lib": dict(
        repo="rust-lang/cargo", tag="0.99.0", rev="797e8a9bca276c1c9f9f738d2a20f484fa4eea9d",
        run=["--list"], test=["--lib"],
        edits=[("src/bin/cargo/main.rs", "fn main() {", 'fn main() {\n    eprintln!("edited main");')],
    ),
    "rust-analyzer": dict(
        repo="rust-lang/rust-analyzer", tag="2026-10-12", rev="2ad81e72b7e978cb800dffe1bcce5c9f98032acf",
        build=["-p", "rust-analyzer"], run=["--version"], test=["-p", "stdx"],
        edits=[("crates/rust-analyzer/src/bin/main.rs", "fn main() -> anyhow::Result<ExitCode> {", 'fn main() -> anyhow::Result<ExitCode> {\n    eprintln!("edited main");')],
    ),
    "tokei": dict(
        repo="XAMPPRocky/tokei", tag="v15.0.0", rev="c14f744716272fadeb27a74443cdffa0af35f82f",
        run=["src", "--sort", "code"],
        edits=[("src/main.rs", "fn main() -> Result<(), Box<dyn Error>> {", 'fn main() -> Result<(), Box<dyn Error>> {\n    eprintln!("edited main");')],
    ),
    "eza": dict(
        repo="eza-community/eza", tag="v0.23.5", rev="98442ab17c2c3738701b62a7e060b1431ae2d6ea",
        run=["--oneline", "--color=never", "src"],
        edits=[("src/main.rs", "fn main() {", 'fn main() {\n    eprintln!("edited main");')],
    ),
    "starship": dict(
        repo="starship/starship", tag="v1.27.0", rev="f8e4e2155542cc84f49b342122d371f74e0ad253",
        run=["module", "character"], test=["--lib", "modules::character"],
        edits=[("src/main.rs", "fn main() {", 'fn main() {\n    eprintln!("edited main");')],
    ),
    "delta": dict(
        repo="dandavison/delta", tag="0.20.1", rev="b88d2290468449d75950bfa150ea7c1291fe2977",
        run=["--list-languages"],
        edits=[("src/main.rs", "fn main() -> std::io::Result<()> {", 'fn main() -> std::io::Result<()> {\n    eprintln!("edited main");')],
    ),
    "helix": dict(
        repo="helix-editor/helix", tag="25.07.1", rev="a05c151bb6e8e9c65ec390b0ae2afe7a5efd619b",
        env=dict(HELIX_DISABLE_AUTO_GRAMMAR_BUILD="1"),
        run=["--version"], test=["-p", "helix-stdx"],
        edits=[("helix-term/src/main.rs", "fn main() -> Result<()> {", 'fn main() -> Result<()> {\n    eprintln!("edited main");')],
    ),
    "tonic": dict(
        repo="hyperium/tonic", tag="v0.14.5", rev="3f7caf3171393734ef19e12d010bd9c945c9e242",
        build=["-p", "examples", "--bin", "helloworld-server"], client="helloworld-client",
        test=["-p", "tonic-types"],
        edits=[("examples/src/helloworld/server.rs", '"Hello {}!"', '"Hello edited {}!"')],
    ),
    "wasmtime": dict(
        repo="bytecodealliance/wasmtime", tag="v49.0.3", rev="f96eabaa11a06086e4fb779ed1c1688481067ab9",
        run=["wast", "tests/misc_testsuite/add.wast"], test=["-p", "wasmtime-environ"],
        # With debug info one target directory of it is 8.5 GB and the wrapper's copies as
        # much again; the two builds side by side are more than the 20 GB this was run on.
        env={"CARGO_PROFILE_DEV_DEBUG": "0"},
        edits=[("src/bin/wasmtime.rs", "fn main() -> Result<()> {", 'fn main() -> Result<()> {\n    eprintln!("edited main");')],
    ),
}

# The projects of the CI job: small, and a build script, a workspace and a test suite among them.
SMALL = ["ripgrep", "fd", "hyperfine"]
# How a project is built: the name of the mode, and how the report names it.
MODES = {"rustd": "`cargo rustd`", "plain": "plain cargo after `rustd enable`"}

# What cargo and `cargo rustd` print about the build, which is not the program's output.
STATUS = re.compile(r"^\s*((Compiling|Checking|Finished|Running|Downloading|Downloaded|Blocking|Locking|Updating|Adding|Fresh|Dirty|Doc-tests)\b|rustd:|warning:)")


def base_env(home):
    env = dict(os.environ, RUSTUP_TOOLCHAIN=TOOLCHAIN, RUSTD_HOME=home, CARGO_TERM_COLOR="never")
    # The store is on by default, in the user's cache directory: a run has one of its own.
    env["RUSTD_CACHE"] = os.path.join(home, "store")
    env["PATH"] = os.path.dirname(RUSTD) + os.pathsep + env["PATH"]
    for name in ("RUSTFLAGS", "CARGO_TARGET_DIR", "RUSTC_WORKSPACE_WRAPPER", "CARGO_BUILD_TARGET"):
        env.pop(name, None)
    return env


# A process started with this is the head of a group or tree that `kill_all` ends whole.
OWN_GROUP = dict(creationflags=subprocess.CREATE_NEW_PROCESS_GROUP) if WINDOWS else dict(start_new_session=True)


def kill_all(process):
    """Ends `process` and what it started, with no chance to clean up."""
    if WINDOWS:
        subprocess.run(["taskkill", "/F", "/T", "/PID", str(process.pid)], capture_output=True)
    else:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass  # it and its group have already ended


def kill(pid):
    """Ends the one process `pid`. Windows has no signals; `SIGTERM` there ends it at once."""
    os.kill(pid, signal.SIGTERM if WINDOWS else signal.SIGKILL)


def alive(pid):
    """Whether `pid` is a process that still runs."""
    if os.path.isdir("/proc/self"):
        return os.path.exists(f"/proc/{pid}") and "Z" not in open(f"/proc/{pid}/stat").read().split(")")[-1].split()[0]
    if not WINDOWS:
        # No /proc on macOS: `ps` says the state, and nothing of a process that is gone.
        state = subprocess.run(["ps", "-o", "stat=", "-p", str(pid)], capture_output=True, text=True).stdout.strip()
        return bool(state) and "Z" not in state
    listed = subprocess.run(["tasklist", "/FI", f"PID eq {pid}", "/FO", "CSV", "/NH"], capture_output=True, text=True)
    return f'"{pid}"' in listed.stdout


def timed(argv, cwd, env, timeout=7200):
    began = time.time()
    ran = subprocess.run(argv, cwd=cwd, env=env, capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=timeout, stdin=subprocess.DEVNULL)
    return time.time() - began, ran


def enabled_home(work, env):
    """A cargo home in which `rustd enable` was run, with what the user's has downloaded.
    The result is the environment that has cargo use it."""
    home = os.path.join(work, "cargo-home")
    env = dict(env, CARGO_HOME=home)
    if not os.path.isdir(home):
        os.makedirs(home)
        downloads(home)
        subprocess.run([RUSTD, "enable"], env=env, check=True, capture_output=True)
    return env


def downloads(home):
    """Has the cargo home `home` use what the user's has downloaded."""
    users = os.environ.get("CARGO_HOME") or os.path.expanduser("~/.cargo")
    for kept in ("registry", "git"):
        if os.path.isdir(os.path.join(users, kept)):
            os.symlink(os.path.join(users, kept), os.path.join(home, kept))


class Side:
    """One of the builds of a checkout: `cargo rustd` (`True`), plain cargo once `rustd
    enable` was run (`"plain"`), or cargo alone into `target-ref` (`False`)."""

    def __init__(self, ws, env, rustd):
        self.ws, self.rustd, self.plain = ws, bool(rustd), rustd == "plain"
        self.env = env if rustd else dict(env, CARGO_TARGET_DIR=os.path.join(ws, "target-ref"))
        self.tally = os.path.join(env["RUSTD_HOME"], "plain-cargo.log")
        self.read = 0

    def argv(self, command, *args):
        if self.plain:
            return [RUSTD, command] if command in ("status", "stop") else ["cargo", command, *args]
        return ["cargo", *(["rustd"] if self.rustd else []), command, *args]

    def cargo(self, command, *args, env=None, timeout=7200):
        return timed(self.argv(command, *args), self.ws, dict(self.env, **(env or {})), timeout)

    def how_built(self, stderr):
        """Whether the daemon built what the last command asked for, or why cargo did."""
        if not self.plain:
            return how_built(stderr)
        lines = open(self.tally).read().splitlines() if os.path.exists(self.tally) else []
        new, self.read = lines[self.read:], len(lines)
        reasons = [l.split("\t", 1)[1] for l in new if l.startswith("rustc\t")]
        if reasons:
            return "cargo: " + reasons[0]
        if not new:
            return "cargo" if re.search(r"^\s*Compiling ", stderr, re.M) else "nothing to build"
        return "daemon"

    def settled(self):
        """Waits until no build of plain cargo is still being handed to the daemon."""
        settling = os.path.join(self.env["RUSTD_HOME"], "settling")
        began = time.time()
        while self.plain and os.path.isdir(settling) and os.listdir(settling) and time.time() - began < WARMUP_SECS:
            time.sleep(0.05)

    def share(self):
        """The functions of the largest load the daemon compiled itself, of all it has."""
        loads = []
        workers = os.path.join(self.env["RUSTD_HOME"], "workers")
        for worker in os.listdir(workers) if os.path.isdir(workers) else []:
            log = open(os.path.join(workers, worker, "log"), errors="replace").read()
            loads += [(int(of), int(done)) for done, of in re.findall(r"^cold: compiled (\d+) of (\d+) fns", log, re.M)]
        return max(loads, default=(0, 0))[::-1]


def how_built(stderr):
    """Whether the daemon built it, or why cargo did."""
    reason = re.search(r"^rustd: (.*); (building with cargo|this is `cargo \w+`)$", stderr, re.M)
    if reason:
        return "cargo: " + reason[1]
    kept = re.search(r"^rustd: (.*); nothing has changed since cargo built$", stderr, re.M)
    if kept:
        return "unchanged: " + kept[1]
    return "daemon" if "[rustd]" in stderr else "cargo"


def program_output(ran):
    """What the program said, without what cargo said, and with the two target directories
    named alike."""
    def clean(text):
        # What rustc warned of, which cargo shows again with every build, comes before
        # cargo says what it runs.
        lines = text.splitlines()
        running = [at for at, l in enumerate(lines) if re.match(r"^\s*Running `", l)]
        lines = [l for l in lines[running[-1] + 1 if running else 0:] if not STATUS.match(l)]
        text = "\n".join(lines).replace("target-ref", "target")
        # A panic message has the id of the thread, and a logger the time.
        text = re.sub(r"' \(\d+\) panicked", "' panicked", text)
        return re.sub(r"\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(\.\d+)?Z?", "<time>", text)
    return ran.returncode, clean(ran.stdout), clean(ran.stderr)


def test_results(ran):
    """The outcome of each test and of each test executable, and the exit status."""
    # A doc test in a generated file is named by its path, in a directory cargo names
    # with a hash of how it was built.
    names = re.sub(r"-[0-9a-f]{16}/", "-HASH/", ran.stdout.replace("target-ref", "target"))
    # A test's result is not always on the line of its name: what a program started by
    # another test prints lands between the two. So the names are read from the lines and
    # the failures from the list at the end, and the totals say the rest.
    failed = [name for listed in re.findall(r"^failures:\n((?:    \S.*\n)+)", names, re.M) for name in listed.split("\n")]
    tests = collections.Counter([(name, "ran") for name in re.findall(r"^test (.+?) \.\.\. ", names, re.M)] + [(name.strip(), "FAILED") for name in failed if name])
    totals = sorted(re.findall(r"^test result: (\w+)\. (\d+ passed; \d+ failed; \d+ ignored; \d+ measured; \d+ filtered out)", ran.stdout, re.M))
    return ran.returncode, tests, totals


def picked_difference(ours, theirs):
    """`None` when `cargo rustd test`, which runs the tests the edits since they last all
    passed reach, ran no test cargo did not, each as cargo's ended, among them every one that
    failed under cargo, and ended as cargo did; or the first thing that is not so."""
    (code, mine, _), (cargo_code, other, _) = ours, theirs
    if code != cargo_code:
        return f"exit {code!r} for {cargo_code!r}"
    extra = list((mine - other).items())[:3]
    if extra:
        return f"tests differ: rustd only {extra}"
    missed = [name for name, outcome in other if outcome == "FAILED" and (name, "FAILED") not in mine][:3]
    if missed:
        return f"tests that fail under cargo were not run: {missed}"
    return None


def difference(ours, theirs):
    """`None` when the two are the same, or the first thing that is not."""
    if ours == theirs:
        return None
    if not isinstance(ours, tuple):
        return f"{ours!r} for {theirs!r}"
    for mine, other in zip(ours, theirs):
        if mine != other:
            if isinstance(mine, collections.Counter):
                only = list((mine - other).items())[:3], list((other - mine).items())[:3]
                return f"tests differ: rustd only {only[0]}, cargo only {only[1]}"
            if isinstance(mine, str):
                pairs = [(a, b) for a, b in zip(mine.splitlines(), other.splitlines()) if a != b][:2]
                return f"output differs: {pairs or (mine[-200:], other[-200:])}"
            return f"{mine!r} for {other!r}"
    return "differ"


def asked_by(client):
    """What the server says to the client program `client`, which cargo built: the message
    of its answer."""
    def ask(timeout):
        ran = subprocess.run([client], capture_output=True, text=True, timeout=timeout + 5)
        if ran.returncode != 0:
            raise OSError(ran.stderr[-200:])
        return "; ".join(re.findall(r'message: "[^"]*"', ran.stdout))
    return ask


def fetched(url):
    return lambda timeout: urllib.request.urlopen(url, timeout=timeout).read().decode()


def served(side, command, ask):
    """What the server `cargo run` starts on `side` answers when `ask` asks it."""
    server = subprocess.Popen(side.argv("run", *command), cwd=side.ws, env=side.env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True, **OWN_GROUP)
    try:
        for _ in range(3000):
            if server.poll() is not None:
                return f"exited {server.returncode}: {server.stderr.read()[-300:]}"
            try:
                return ask(5)
            except OSError:
                time.sleep(0.1)
        return "no answer"
    finally:
        kill_all(server)
        server.wait()
        # The port is free again before the other side's server asks for it.
        for _ in range(100):
            try:
                ask(1)
                time.sleep(0.05)
            except OSError:
                break


def checking(root):
    """Whether a `cargo check` runs below `root`: the one `cargo rustd` starts after cargo
    built, for the dependencies to be checked by the time the daemon builds."""
    if not os.path.isdir("/proc/self"):
        # No /proc on macOS: `ps` has the commands and `lsof` where each one runs.
        listed = subprocess.run(["ps", "-axo", "pid=,command="], capture_output=True, text=True).stdout
        checks = [l.split()[0] for l in listed.splitlines() if l.split()[2:3] == ["check"]]
        cwd = lambda pid: subprocess.run(["lsof", "-a", "-d", "cwd", "-p", pid, "-Fn"], capture_output=True, text=True).stdout
        return any(os.path.realpath(l[1:]).startswith(root) for pid in checks for l in cwd(pid).splitlines() if l.startswith("n"))
    for pid in filter(str.isdigit, os.listdir("/proc")):
        try:
            words = open(f"/proc/{pid}/cmdline", "rb").read().split(b"\0")
            if b"check" in words[1:2] and os.readlink(f"/proc/{pid}/cwd").startswith(root):
                return True
        except OSError:
            pass
    return False


def wait_warm(side, root):
    """Waits until the daemon has every build of `root` loaded or has given up on one. The
    result is the seconds it took and how each ended."""
    began = time.time()
    side.settled()
    while time.time() - began < WARMUP_SECS:
        _, status = side.cargo("status")
        states = re.findall(rf"^    {re.escape(root)}: (.*)$", status.stdout, re.M)
        if states and not any(s.startswith("loading") for s in states) and not checking(root):
            return time.time() - began, states
        # No daemon loads what every build so far was left to cargo for.
        left = re.search(r"^daemon: not running$(?s:.*)^  every build is cargo's: (.*)$", status.stdout, re.M)
        if left:
            return time.time() - began, ["left to cargo: " + left[1]]
        time.sleep(0.25)
    return time.time() - began, ["not loaded in time"]


def rewrite(path, old, new):
    """Replaces the first `old` in the file by `new`. The result is the text it had."""
    text = open(path).read()
    assert old in text, f"{old!r} is not in {path}"
    open(path, "w").write(text.replace(old, new, 1))
    return text


def fetch(name, spec):
    """The checkout of the project, cloned at its tag when it is not there. The tag has to
    be the commit the project is pinned to."""
    source = os.path.join(SOURCES, name)
    if not os.path.isdir(source):
        os.makedirs(SOURCES, exist_ok=True)
        subprocess.run(["git", "clone", "-q", "--depth", "1", "--branch", spec["tag"], f"https://github.com/{spec['repo']}", source], check=True)
    head = subprocess.run(["git", "rev-parse", "HEAD"], cwd=source, capture_output=True, text=True).stdout.strip()
    assert head == spec["rev"], f"{name} is at {head}, not at the {spec['rev']} it is pinned to"
    return source


def project(name, spec, work, mode):
    """Runs the steps of one project with `cargo rustd` (`mode` "rustd") or with plain cargo
    after `rustd enable` ("plain"). The result is the row of the table: times, who built
    each step, and every difference from cargo."""
    source = fetch(name, spec)
    ws = os.path.realpath(os.path.join(work, name))
    shutil.copytree(source, ws, symlinks=True, ignore=shutil.ignore_patterns("target", "target-ref"))
    env = dict(base_env(os.path.join(work, "home-" + mode)), **spec.get("env", {}))
    plain = mode == "plain"
    ours, theirs = Side(ws, enabled_home(work, env) if plain else env, "plain" if plain else True), Side(ws, env, False)
    build, run, test = spec.get("build", []), spec.get("run", []), spec.get("test", [])
    row = dict(project=name, mode=mode, tag=spec["tag"], wrong=[], times={}, built={})
    subprocess.run(["cargo", "fetch"], cwd=ws, env=env, capture_output=True)
    ours.cargo("stop")

    def step(label, command, args, compare):
        """Runs `command` on both sides and compares what `compare` reads from each."""
        (took, mine), (reference, other) = ours.cargo(command, *args), theirs.cargo(command, *args)
        row["times"][label] = (round(took, 2), round(reference, 2))
        row["built"][label] = ours.how_built(mine.stderr)
        compared = picked_difference if compare is test_results else difference
        wrong = compared(compare(mine), compare(other))
        if wrong:
            row["wrong"].append(f"{label}: {wrong}")
            open(os.path.join(work, f"{name}-{label}.rustd.txt"), "w", encoding="utf-8").write(mine.stdout + mine.stderr)
            open(os.path.join(work, f"{name}-{label}.cargo.txt"), "w", encoding="utf-8").write(other.stdout + other.stderr)
        print(f"  {label:12} rustd {took:7.2f}s  cargo {reference:7.2f}s  {row['built'][label]:8.80}  {wrong or 'same'}", flush=True)
        # What the tests that failed here alone said.
        said = wrong and re.search(r"^failures:\n(.*?)^failures:", mine.stdout, re.M | re.S)
        if said:
            print("".join(f"      {line}\n" for line in said[1].splitlines()[:60]), end="", flush=True)

    def runs(label):
        if ask:
            mine, other = served(ours, build, ask), served(theirs, build, ask)
            if mine != other:
                row["wrong"].append(f"{label}: served {mine[:80]!r} for {other[:80]!r}")
            print(f"  {label:12} served {mine[:50]!r}  {'same' if mine == other else 'WRONG'}", flush=True)
        else:
            step(label, "run", [*build, "--", *run], program_output)

    def warm(label):
        took, states = wait_warm(ours, ws)
        row["times"][label] = (round(took, 2), None)
        row["built"][label] = "; ".join(states)
        print(f"  {label:12} {took:7.2f}s  {'; '.join(states)[:150]}", flush=True)

    status = lambda ran: ran.returncode
    step("cold build", "build", build, status)
    ask = fetched(spec["serves"]) if "serves" in spec else None
    if "client" in spec:
        theirs.cargo("build", "-p", build[1], "--bin", spec["client"])
        ask = asked_by(os.path.join(ws, "target-ref/debug", spec["client"]))
    runs("cold run")
    warm("warm-up")
    step("warm build", "build", build, status)
    runs("warm run")
    step("cold test", "test", test, test_results)
    warm("test warm-up")
    step("warm test", "test", test, test_results)
    for file, old, new in spec["edits"]:
        rewrite(os.path.join(ws, file), old, new)
    step("edit build", "build", build, status)
    runs("edit run")
    step("edit test", "test", test, test_results)
    path = os.path.join(ws, spec.get("broken") or spec["edits"][0][0])
    edited = open(path).read()
    open(path, "w").write(edited + '\nfn rustd_type_error() -> u32 {\n    "not a number"\n}\n')
    errors = lambda ran: (ran.returncode, sorted(set(re.findall(r"^error(?:\[E\d+\])?: .*$", ran.stderr, re.M))))
    step("broken", "build", build, errors)
    open(path, "w").write(edited)
    step("mended", "build", build, status)
    runs("mended run")
    worktree(ours, row, build, [*build, "--", *run] if not ask else None, spec["edits"], work)
    row["share"] = ours.share()
    print(f"  functions    {row['share'][0]} of {row['share'][1]} compiled by rustd", flush=True)
    _, counted = ours.cargo("status")
    row["status"] = counted.stdout
    ours.cargo("stop")
    return row


FLAG_WORKSPACE = {
    "Cargo.toml": '[workspace]\nmembers = ["app", "util"]\nresolver = "2"\n\n[profile.quick]\ninherits = "dev"\n\n[profile.release]\npanic = "abort"\n\n[profile.quick.package.util]\nopt-level = 3\noverflow-checks = false\n',
    "util/Cargo.toml": '[package]\nname = "util"\nversion = "0.1.0"\nedition = "2021"\n\n[features]\nloud = []\n',
    "util/src/lib.rs": '/// ```\n/// assert_eq!(util::greeting("it").len(), 8);\n/// ```\n///\n/// ```should_panic\n/// assert_eq!(util::greeting("it"), "");\n/// ```\npub fn greeting(name: &str) -> String {\n    let text = format!("hello {name}");\n    if cfg!(feature = "loud") { text.to_uppercase() } else { text }\n}\n\nthread_local! {\n    static SAID: std::cell::RefCell<Option<String>> = None.into();\n}\n\npub fn said() -> String {\n    SAID.with_borrow(|it| it.clone().unwrap_or_default())\n}\n\n#[cfg(test)]\nmod tests {\n    // A test and the library see one thread local, whoever compiled either.\n    #[test]\n    fn says_what_a_test_set() {\n        super::SAID.with_borrow_mut(|it| *it = Some("set".to_string()));\n        assert_eq!(super::said(), "set");\n    }\n\n    #[test]\n    fn greets() {\n        assert!(super::greeting("a").to_lowercase().starts_with("hello"));\n    }\n}\n',
    "app/Cargo.toml": '[package]\nname = "app"\nversion = "0.1.0"\nedition = "2021"\ndefault-run = "app"\n\n[features]\nloud = ["util/loud"]\n\n[dependencies]\nutil = { path = "../util" }\n',
    "app/src/main.rs": '#![allow(unexpected_cfgs)]\n\nfn main() {\n    let name = std::env::args().nth(1).unwrap_or("world".to_string());\n    println!("{}", util::greeting(&name));\n    println!("{} {} {}", cfg!(debug_assertions), cfg!(extra), cfg!(target_feature = "avx2"));\n    let sum = std::panic::catch_unwind(|| name.len() as u8 + 251);\n    println!("{:?}", sum.ok());\n    if name == "fail" {\n        std::process::exit(3);\n    }\n}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn in_the_program() {\n        assert_eq!(util::greeting("x").len(), 7);\n    }\n}\n',
    "app/src/bin/other.rs": 'fn main() {\n    println!("other: {}", util::greeting("other"));\n}\n',
    "app/examples/demo.rs": 'fn main() {\n    println!("demo: {}", util::greeting("demo"));\n}\n',
    "app/tests/it.rs": '#[test]\nfn greets_it() {\n    assert_eq!(util::greeting("it").to_lowercase(), "hello it");\n}\n\n#[test]\nfn fails_when_asked() {\n    assert!(std::env::var("FLAGS_FAIL").is_err());\n}\n\n// A cargo run by a test has the units of the build: the program stays the file it was.\n#[test]\nfn a_cargo_inside_leaves_the_program() {\n    use std::os::unix::fs::MetadataExt;\n    let program = env!("CARGO_BIN_EXE_app");\n    let before = std::fs::metadata(program).unwrap().ino();\n    let cargo = std::env::var("CARGO").unwrap_or("cargo".to_string());\n    let built = std::process::Command::new(cargo).args(["build", "-q", "-p", "app"]).status().unwrap();\n    assert!(built.success());\n    assert_eq!(before, std::fs::metadata(program).unwrap().ino());\n}\n\n// What cargo tells a test of the places of the build is what the daemon tells it.\n#[test]\nfn the_places_of_the_build_are_cargos() {\n    assert!(std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).is_dir());\n    let ran = std::process::Command::new(env!("CARGO_BIN_EXE_other")).output().unwrap();\n    assert_eq!(String::from_utf8_lossy(&ran.stdout).lines().count(), 1);\n}\n',
}

# Each: the subcommand, its arguments, the variables it runs with.
FLAG_CASES = [
    ("run", [], {}),
    ("run", ["--", "fail"], {}),
    ("run", ["--release"], {}),
    ("run", ["--release", "--", "fail"], {}),
    ("run", ["--", "world"], {"RUSTFLAGS": "-C overflow-checks=off"}),
    # A processor of x86-64 is one only there.
    *([("run", ["--release"], {"RUSTFLAGS": "-C target-cpu=x86-64-v3"})] if host().startswith("x86_64") else []),
    ("run", ["--no-default-features", "-p", "app"], {}),
    # The host named: what is built for is the same, where it is put is not.
    ("run", ["--target", host()], {}),
    ("run", ["-p", "app"], {}),
    ("run", ["--features", "loud", "-p", "app"], {}),
    ("run", ["--bin", "other"], {}),
    ("run", ["--example", "demo"], {}),
    ("run", ["--manifest-path", "app/Cargo.toml"], {}),
    ("run", ["--profile", "quick"], {}),
    ("run", ["-q", "name"], {}),
    ("run", [], {"RUSTFLAGS": "--cfg extra"}),
    ("build", ["--workspace"], {}),
    ("build", ["--all-features"], {}),
    ("check", [], {}),
    ("test", [], {}),
    ("test", ["--test", "it"], {}),
    ("test", ["-p", "util", "--lib"], {}),
    ("test", ["greets"], {}),
    ("test", ["--no-fail-fast"], {"FLAGS_FAIL": "1"}),
    ("test", [], {"FLAGS_FAIL": "1"}),
    ("test", ["--no-run"], {}),
    ("test", ["--doc"], {}),
    ("test", ["--doc", "--features", "loud", "-p", "util"], {}),
    ("test", [], {"CARGO_INCREMENTAL": "0"}),
]


def flags(work):
    """Every flag case on both sides, cold, warm and after an edit. The result is the rows
    and what differed."""
    ws = os.path.realpath(os.path.join(work, "flags"))
    for file, text in FLAG_WORKSPACE.items():
        os.makedirs(os.path.dirname(os.path.join(ws, file)), exist_ok=True)
        open(os.path.join(ws, file), "w").write(text)
    env = base_env(os.path.join(work, "home-flags"))
    ours, theirs = Side(ws, env, True), Side(ws, env, False)
    rows, wrong = [], []

    def round_of(label):
        for command, args, extra in FLAG_CASES:
            (_, mine), (_, other) = ours.cargo(command, *args, env=extra), theirs.cargo(command, *args, env=extra)
            read = test_results if command == "test" else program_output
            compared = picked_difference if command == "test" else difference
            differs = compared(read(mine), read(other))
            shown = " ".join([*(f"{k}={v}" for k, v in extra.items()), "cargo rustd", command, *args])
            rows.append((label, shown, how_built(mine.stderr), mine.returncode, differs or "same"))
            if differs:
                wrong.append(f"{label}: {shown}: {differs}")
            print(f"  {label:6} {shown:50} exit {mine.returncode:3}  {how_built(mine.stderr):8.60}  {differs or 'same'}", flush=True)

    round_of("cold")
    wait_warm(ours, ws)
    round_of("warm")
    rewrite(os.path.join(ws, "util/src/lib.rs"), 'format!("hello {name}")', 'format!("hello {name}!")')
    rewrite(os.path.join(ws, "app/src/main.rs"), ".len(), 7)", ".len(), 8)")
    rewrite(os.path.join(ws, "app/tests/it.rs"), '"hello it"', '"hello it!"')
    round_of("edit")
    _, counted = ours.cargo("status")
    ours.cargo("stop")
    return rows, wrong, counted.stdout


PICK_LIB = """pub mod extra;

pub static GREETING: &str = include_str!("../greeting.txt");

pub fn version() -> &'static str {
    env!("CORELIB_VERSION")
}

pub fn answer() -> u32 {
    macros::answer!()
}

pub fn unit_only(x: u32) -> u32 {
    x + 1
}

pub fn integration_only(x: u32) -> u32 {
    x + 1
}

pub fn harnessless_only(x: u32) -> u32 {
    x + 1
}

/// ```
/// assert_eq!(corelib::doc_only(1), 2);
/// ```
pub fn doc_only(x: u32) -> u32 {
    x + 1
}

pub fn downstream_only(x: u32) -> u32 {
    x + 1
}

pub fn program_only(x: u32) -> u32 {
    x + 1
}

pub trait Shape {
    fn area(&self) -> u32;
}

struct Square(u32);

impl Shape for Square {
    fn area(&self) -> u32 {
        self.0 * self.0
    }
}

fn square() -> Box<dyn Shape> {
    Box::new(Square(3))
}

pub fn shapes() -> Vec<Box<dyn Shape>> {
    vec![square()]
}

pub fn total(shapes: &[Box<dyn Shape>]) -> u32 {
    shapes.iter().map(|it| it.area()).sum()
}

macro_rules! three {
    () => {
        3
    };
}

pub fn macro_value() -> u32 {
    three!()
}

pub fn configured() -> u32 {
    7
}

#[cfg(test)]
mod tests {
    #[test]
    fn unit() {
        assert_eq!(super::unit_only(1), 2);
    }

    #[test]
    fn unrelated() {
        assert_eq!(1 + 1, 2);
    }
}
"""

PICK_WORKSPACE = {
    "Cargo.toml": '[workspace]\nmembers = ["corelib", "app", "macros"]\nresolver = "2"\n',
    "corelib/Cargo.toml": '[package]\nname = "corelib"\nversion = "0.1.0"\nedition = "2021"\n\n[dependencies]\nmacros = { path = "../macros" }\n\n[[test]]\nname = "plain"\nharness = false\n',
    "corelib/build.rs": 'fn main() {\n    println!("cargo:rerun-if-changed=build.rs");\n    println!("cargo:rerun-if-changed=version.txt");\n    let version = std::fs::read_to_string("version.txt").unwrap();\n    println!("cargo:rustc-env=CORELIB_VERSION={}", version.trim());\n}\n',
    "corelib/version.txt": "1\n",
    "macros/Cargo.toml": '[package]\nname = "macros"\nversion = "0.1.0"\nedition = "2021"\n\n[lib]\nproc-macro = true\n',
    "macros/src/lib.rs": 'use proc_macro::TokenStream;\n\nfn value() -> u32 {\n    42\n}\n\n#[proc_macro]\npub fn answer(_: TokenStream) -> TokenStream {\n    value().to_string().parse().unwrap()\n}\n',
    "corelib/greeting.txt": "hi\n",
    "corelib/src/extra.rs": "pub fn extra() -> u32 {\n    1\n}\n",
    "corelib/src/lib.rs": PICK_LIB,
    "corelib/tests/it.rs": '#[test]\nfn integration() {\n    assert_eq!(corelib::integration_only(1), 2);\n}\n\n#[test]\nfn other() {\n    assert_eq!(2, 2);\n}\n',
    "corelib/tests/plain.rs": 'fn main() {\n    assert_eq!(corelib::harnessless_only(1), 2);\n}\n',
    "corelib/tests/shapes.rs": '#[test]\nfn dyn_area() {\n    assert_eq!(corelib::total(&corelib::shapes()), 9);\n}\n',
    "corelib/tests/greet.rs": '#[test]\nfn greets() {\n    assert_eq!(corelib::GREETING, "hi\\n");\n}\n\n#[test]\nfn macro_three() {\n    assert_eq!(corelib::macro_value(), 3);\n}\n\n#[test]\nfn configured() {\n    assert_eq!(corelib::configured(), 7);\n}\n',
    "app/Cargo.toml": '[package]\nname = "app"\nversion = "0.1.0"\nedition = "2021"\n\n[dependencies]\ncorelib = { path = "../corelib" }\n',
    "app/src/lib.rs": 'pub fn app_value() -> u32 {\n    corelib::downstream_only(1)\n}\n',
    "app/src/main.rs": 'fn main() {\n    println!("{}", corelib::program_only(1));\n}\n',
    "app/tests/cli.rs": '#[test]\nfn runs_the_program() {\n    let ran = std::process::Command::new(env!("CARGO_BIN_EXE_app")).output().unwrap();\n    assert_eq!(String::from_utf8_lossy(&ran.stdout).trim(), "2");\n}\n',
    "testdata/golden.txt": "gold\n",
    "app/tests/golden.rs": '#[test]\nfn golden() {\n    let text = std::fs::read_to_string("../testdata/golden.txt").unwrap();\n    assert_eq!(text.trim(), "gold");\n}\n',
    "corelib/fixture.local": "ok\n",
    "corelib/tests/ui/case.rs": "// ok\nfn main() {}\n",
    "corelib/tests/local.rs": '#[test]\nfn local_fixture() {\n    assert_eq!(std::fs::read_to_string("fixture.local").unwrap().trim(), "ok");\n}\n\n#[test]\nfn ui_case() {\n    assert!(std::fs::read_to_string("tests/ui/case.rs").unwrap().contains("ok"));\n}\n\n#[test]\nfn env_free() {\n    assert!(std::env::var("PICKS_FAIL").is_err());\n}\n',
    "app/tests/app.rs": '#[test]\nfn downstream() {\n    assert_eq!(app::app_value(), 2);\n}\n\n#[test]\nfn versioned() {\n    assert_eq!(corelib::version(), "1");\n}\n\n#[test]\nfn answered() {\n    assert_eq!(corelib::answer(), 42);\n}\n',
}

ALL_PICK_TESTS = {"golden", "local_fixture", "ui_case", "env_free", "configured", "tests::unit", "tests::unrelated", "integration", "other", "dyn_area", "greets", "macro_three", "downstream", "versioned", "answered", "runs_the_program"}

# Each: what is edited, the file, the text and what takes its place, the test that is to fail
# (`plain` for the target without the harness, `doc` for the doc test), and whether every
# test is to run.
PICK_CASES = [
    ("unit test", "corelib/src/lib.rs", "unit_only(x: u32) -> u32 {\n    x + 1", "unit_only(x: u32) -> u32 {\n    x + 2", "tests::unit", False),
    ("integration test", "corelib/src/lib.rs", "integration_only(x: u32) -> u32 {\n    x + 1", "integration_only(x: u32) -> u32 {\n    x + 2", "integration", False),
    ("harness = false", "corelib/src/lib.rs", "harnessless_only(x: u32) -> u32 {\n    x + 1", "harnessless_only(x: u32) -> u32 {\n    x + 2", "plain", False),
    ("doc test", "corelib/src/lib.rs", "doc_only(x: u32) -> u32 {\n    x + 1", "doc_only(x: u32) -> u32 {\n    x + 2", "doc", False),
    ("downstream crate", "corelib/src/lib.rs", "downstream_only(x: u32) -> u32 {\n    x + 1", "downstream_only(x: u32) -> u32 {\n    x + 2", "downstream", False),
    ("a program an integration test runs", "corelib/src/lib.rs", "program_only(x: u32) -> u32 {\n    x + 1", "program_only(x: u32) -> u32 {\n    x + 2", "runs_the_program", False),
    ("impl called through dyn", "corelib/src/lib.rs", "self.0 * self.0", "self.0 * self.0 + 1", "dyn_area", False),
    ("new test", "corelib/src/lib.rs", "    #[test]\n    fn unrelated()", "    #[test]\n    fn added() {\n        assert_eq!(1, 2);\n    }\n\n    #[test]\n    fn unrelated()", "tests::added", True),
    ("include_str! data", "corelib/greeting.txt", "hi\n", "hello\n", "greets", True),
    ("macro_rules!", "corelib/src/lib.rs", "        3\n    };", "        4\n    };", "macro_three", True),
    ("cfg", "corelib/src/lib.rs", "pub fn configured() -> u32 {\n    7\n}", "#[cfg(not(test))]\npub fn configured() -> u32 {\n    8\n}\n\n#[cfg(test)]\npub fn configured() -> u32 {\n    7\n}", "configured", True),
    ("proc macro", "macros/src/lib.rs", "    42", "    43", "answered", True),
    ("a file a build script reads", "corelib/version.txt", "1\n", "2\n", "versioned", True),
    ("build.rs", "corelib/build.rs", "fn main() {", "fn main() {\n    // edited", None, True),
    ("workspace Cargo.toml", "Cargo.toml", 'resolver = "2"\n', 'resolver = "2"\n\n[workspace.metadata]\nedited = true\n', None, True),
    # A file a test reads as it runs, which rustc does not: outside every package, ignored
    # by git, and Rust source no target is compiled from.
    ("a file outside every package", "testdata/golden.txt", "gold\n", "lead\n", "golden", "any"),
    ("a file git ignores", "corelib/fixture.local", "ok\n", "no\n", "local_fixture", "any"),
    ("Rust source no target is compiled from", "corelib/tests/ui/case.rs", "// ok", "// no", "ui_case", "any"),
    ("Cargo.toml", "corelib/Cargo.toml", 'edition = "2021"\n', 'edition = "2021"\ndescription = "edited"\n', None, True),
]


def picks(work):
    """Which tests `cargo rustd test` runs after an edit: a failure planted where one kind of
    test alone reaches it is run and fails, the tests it does not reach are not run, and an
    edit rustd can't attribute runs every test. The result is each check with whether it
    held."""
    ws = os.path.realpath(os.path.join(work, "picks"))
    for file, text in PICK_WORKSPACE.items():
        os.makedirs(os.path.dirname(os.path.join(ws, file)), exist_ok=True)
        open(os.path.join(ws, file), "w").write(text)
    git = lambda *args: subprocess.run(["git", "-c", "user.name=rustd", "-c", "user.email=rustd@localhost", *args], cwd=ws, check=True, capture_output=True)
    open(os.path.join(ws, ".gitignore"), "w").write("target\ncorelib/fixture.local\n")
    git("init", "-q"), git("add", "."), git("commit", "-qm", "first")
    ours = Side(ws, base_env(os.path.join(work, "home-picks")), True)
    unit = os.path.join(ws, "corelib/src/lib.rs")
    failing_unit = lambda text: text.replace(PICK_CASES[0][2], PICK_CASES[0][2].replace("+ 1", "+ 3"), 1)
    checks = []

    def check(name, held, detail=""):
        checks.append((name, bool(held), detail))
        print(f"  {'ok ' if held else 'NOT'} {name}  {detail}", flush=True)

    def test():
        # The next edit waits for the daemon to load what cargo built: one written while it
        # loads has every test run, rightly but not what is checked here.
        _, ran = ours.cargo("test", "--no-fail-fast")
        wait_warm(ours, ws)
        tests = dict(re.findall(r"^test (\S+) \.\.\. (\w+)$", ran.stdout, re.M))
        doc = re.findall(r"^test \S*src/lib\.rs - doc_only .*\.\.\. (\w+)$", ran.stdout, re.M)
        if doc:
            tests["doc"] = doc[0]
        if re.search(r"Running tests/plain\.rs", ran.stderr):
            tests["plain"] = "ran"
        return ran.returncode, tests, ran

    code, tests, _ = test()
    check("the first run runs every test", code == 0 and ALL_PICK_TESTS <= set(tests), f"{sorted(tests)}")
    # Nothing was looked at before cargo first built, so that run is not kept as passing.
    code, tests, _ = test()
    check("the second run runs every test", code == 0 and ALL_PICK_TESTS <= set(tests), f"{sorted(tests)}")
    code, tests, _ = test()
    check("a run with nothing changed runs no test", code == 0 and not set(tests) & ALL_PICK_TESTS, f"{sorted(tests)}")
    for label, file, old, new, failing, everything in PICK_CASES:
        path = os.path.join(ws, file)
        before = open(path).read()
        open(path, "w").write(before.replace(old, new, 1))
        code, tests, ran = test()
        ran_names = {name for name in tests if name in ALL_PICK_TESTS}
        if failing == "plain":
            failed = code != 0 and re.search(r"panicked at \S*tests/plain\.rs", ran.stderr) is not None
        elif failing:
            failed = code != 0 and tests.get(failing) == "FAILED"
        else:
            failed = code == 0
        if everything == "any":
            picked = True
        elif everything:
            picked = ALL_PICK_TESTS <= ran_names
        else:
            # The test that fails, and none that the edit does not reach.
            # The tests of `app` run whole when its program changed, which the test of the
            # program is for.
            unreached = {"tests::unrelated", "other", "greets", "macro_three", "configured", "local_fixture", "ui_case", "env_free"}
            if failing != "runs_the_program":
                unreached |= {"versioned", "answered", "runs_the_program", "golden"}
            picked = not ran_names & unreached
        runs = {True: "every test runs", False: "the others do not", "any": "it runs"}[everything]
        check(f"{label}: the edit's test {'fails' if failing else 'passes'}, {runs}", failed and picked, f"exit {code}, ran {sorted(ran_names)}")
        if failing:
            again, _, _ = test()
            check(f"{label}: run again, it fails again", again != 0, f"exit {again}")
        open(path, "w").write(before)
        code, tests, _ = test()
        check(f"{label}: undone, the tests pass", code == 0, f"exit {code}")
    # An edit that leaves the size and the time of the file as they were, with one to
    # another file of the crate that has it compiled again.
    before = open(unit).read()
    extra = os.path.join(ws, "corelib/src/extra.rs")
    held = os.stat(unit)
    open(unit, "w").write(failing_unit(before))
    os.utime(unit, ns=(held.st_atime_ns, held.st_mtime_ns))
    open(extra, "w").write(PICK_WORKSPACE["corelib/src/extra.rs"].replace("1", "2"))
    code, tests, _ = test()
    check("an edit that keeps the size and the time: the edit's test fails", code != 0 and tests.get("tests::unit") == "FAILED", f"exit {code}, ran {sorted(set(tests) & ALL_PICK_TESTS)}")
    open(unit, "w").write(before)
    open(extra, "w").write(PICK_WORKSPACE["corelib/src/extra.rs"])
    test()
    # An edit saved once the build is done, while the tests are picked and run: the tests ran on
    # the build before it, so it is not taken as passing.
    command = ours.argv("test", "--no-fail-fast")
    running = subprocess.Popen(command, cwd=ws, env=ours.env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    for line in running.stderr:
        if "Finished" in line:
            open(unit, "w").write(failing_unit(before))
            break
    running.communicate()
    wait_warm(ours, ws)
    code, tests, _ = test()
    check("an edit saved while the tests run: the next run runs its test, which fails", code != 0 and tests.get("tests::unit") == "FAILED", f"exit {code}, ran {sorted(set(tests) & ALL_PICK_TESTS)}")
    open(unit, "w").write(before)
    test()
    # The lock file as cargo writes it, once it is there: whatever changes in it runs all.
    lock = os.path.join(ws, "Cargo.lock")
    before = open(lock).read()
    open(lock, "w").write(before + "\n")
    code, tests, _ = test()
    check("Cargo.lock: every test runs", ALL_PICK_TESTS <= set(tests), f"exit {code}, ran {sorted(set(tests) & ALL_PICK_TESTS)}")
    open(lock, "w").write(before)
    test()
    # `--since` picks what the changes since a commit reach, and leaves what passed as it was.
    git("add", "."), git("commit", "-qm", "second")
    path = os.path.join(ws, "corelib/src/lib.rs")
    before = open(path).read()
    open(path, "w").write(before.replace(PICK_CASES[0][2], PICK_CASES[0][3], 1))
    _, ran = ours.cargo("test", "--no-fail-fast", "--since", "HEAD")
    since = dict(re.findall(r"^test (\S+) \.\.\. (\w+)$", ran.stdout, re.M))
    check("--since HEAD: the edit's test fails, the others do not", ran.returncode != 0 and set(since) & ALL_PICK_TESTS == {"tests::unit"}, f"ran {sorted(since)}")
    open(path, "w").write(before)
    wait_warm(ours, ws)
    # A variable that was not there when the tests passed, which a test reads.
    _, ran = ours.cargo("test", "--no-fail-fast", "--since", "HEAD", env={"PICKS_FAIL": "1"})
    since = dict(re.findall(r"^test (\S+) \.\.\. (\w+)$", ran.stdout, re.M))
    check("--since HEAD with another variable: every test runs, and the one that reads it fails", ran.returncode != 0 and since.get("env_free") == "FAILED", f"exit {ran.returncode}")
    wait_warm(ours, ws)
    _, ran = ours.cargo("test", "--no-fail-fast", "--since", "no-such-commit")
    since = dict(re.findall(r"^test (\S+) \.\.\. (\w+)$", ran.stdout, re.M))
    check("--since an unknown commit: every test runs", ran.returncode == 0 and ALL_PICK_TESTS <= set(since), f"exit {ran.returncode}")
    ours.cargo("stop")
    return checks


def daemon(work):
    """The life of the daemon. The result is each check with whether it held."""
    ws = os.path.realpath(os.path.join(work, "life"))
    for file, text in FLAG_WORKSPACE.items():
        os.makedirs(os.path.dirname(os.path.join(ws, file)), exist_ok=True)
        open(os.path.join(ws, file), "w").write(text)
    # A file that is no text, beside the sources: the daemon loads the workspace all the same.
    open(os.path.join(ws, "util/src/not-text.bin"), "wb").write(b"\xff\xfe\x00\x80")
    with open(os.path.join(ws, "app/src/bin/nested.rs"), "w") as nested:
        nested.write('fn main() {\n    let cargo = std::process::Command::new("cargo").arg("build").status();\n    std::process::exit(cargo.ok().and_then(|it| it.code()).unwrap_or(1));\n}\n')
    git = lambda *args, cwd=ws: subprocess.run(["git", "-c", "user.name=rustd", "-c", "user.email=rustd@localhost", *args], cwd=cwd, check=True, capture_output=True)
    open(os.path.join(ws, ".gitignore"), "w").write("target\ntarget-ref\n")
    git("init", "-q"), git("add", "."), git("commit", "-qm", "first")
    tree = os.path.realpath(os.path.join(work, "life-worktree"))
    git("worktree", "add", "-q", tree)
    home = os.path.join(work, "home-life")
    env = base_env(home)
    here, there = Side(ws, env, True), Side(tree, env, True)
    checks = []

    def check(name, held, detail=""):
        checks.append((name, bool(held), detail))
        print(f"  {'ok ' if held else 'NOT'} {name}  {detail}", flush=True)

    def status(side=None):
        text = (side or here).cargo("status")[1].stdout
        pid = re.search(r"^daemon: pid (\d+)", text, re.M)
        return (int(pid[1]) if pid else None), [int(p) for p in re.findall(r"worker: pid (\d+)", text)], text

    edit = lambda root, text: open(os.path.join(root, "app/src/main.rs"), "a").write(f"\n// {text}\n")

    check("no daemon before the first build", status()[0] is None)
    _, first = here.cargo("build")
    pid, workers, _ = status()
    check("the first build starts one", first.returncode == 0 and pid is not None, f"pid {pid}")
    wait_warm(here, ws)
    # A program that runs plain `cargo build` on its own checkout, as a test suite may, after
    # the daemon built it: cargo's own artifacts are behind the sources, so cargo builds
    # them again and its executable takes the place of the daemon's. Both are right.
    edit(ws, "nested")
    _, installed = here.cargo("build")
    _, nested = here.cargo("run", "--bin", "nested")
    _, after = here.cargo("run", "--bin", "app")
    _, reference = timed(["cargo", "run", "--bin", "app"], ws, dict(env, CARGO_TARGET_DIR=os.path.join(ws, "target-ref")))
    check("a program that runs cargo on its checkout builds, and the next run is right", how_built(installed.stderr) == "daemon" and nested.returncode == 0 and after.returncode == reference.returncode and after.stdout == reference.stdout, f"{how_built(installed.stderr)}, nested {nested.returncode}, then {how_built(after.stderr)}")
    edit(ws, "one")
    _, second = here.cargo("build")
    check("a later build is the daemon's, by the same daemon", how_built(second.stderr) == "daemon" and status()[0] == pid, how_built(second.stderr))

    there.cargo("build")
    wait_warm(there, tree)
    _, workers_now, text = status()
    shared = re.search(rf"worker: pid \d+,[^\n]*\n(    [^\n]*\n)*    {re.escape(tree)}: ready", text)
    check("a worktree is loaded by the same daemon and the same worker", status()[0] == pid and len(workers_now) == 1 and shared, f"workers {workers_now}")
    edit(tree, "two")
    _, in_tree = there.cargo("run")
    check("a build in the worktree is the daemon's", how_built(in_tree.stderr) == "daemon" and in_tree.returncode == 0, how_built(in_tree.stderr))

    edit(ws, "three")
    client = subprocess.Popen(["cargo", "rustd", "build"], cwd=ws, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, **OWN_GROUP)
    time.sleep(0.01)
    kill_all(client)
    client.wait()
    edit(ws, "four")
    _, after_kill = here.cargo("run")
    check("the daemon outlives a client that is killed", status()[0] == pid and after_kill.returncode == 0 and "hello world" in after_kill.stdout, how_built(after_kill.stderr))

    other = os.path.join(work, "other-build")
    os.makedirs(other, exist_ok=True)
    for name in ("rustd" + EXE, "cargo-rustd" + EXE, "rustd-rustc" + EXE):
        shutil.copy(os.path.join(os.path.dirname(RUSTD), name), os.path.join(other, name))
    newer = Side(ws, dict(env, PATH=other + os.pathsep + env["PATH"]), True)
    edit(ws, "five")
    _, by_newer = newer.cargo("run")
    replaced = status(newer)[0]
    ran = by_newer.returncode == 0 and "hello world" in by_newer.stdout
    check("a client of another build replaces the daemon", ran and replaced not in (None, pid) and not alive(pid), f"pid {pid} then {replaced}")
    check("the workers of the daemon it replaced are gone", not any(alive(p) for p in workers_now), f"{workers_now}")
    newer.cargo("stop")

    quick = Side(ws, dict(env, RUSTD_IDLE_SECS="3"), True)
    quick.cargo("build")
    pid, _, _ = status()
    wait_warm(quick, ws)
    _, workers, _ = status()
    time.sleep(6)
    gone = not alive(pid) and not any(alive(p) for p in workers)
    check("idle for RUSTD_IDLE_SECS, the daemon and its workers exit", pid is not None and workers and gone, f"daemon {pid}, workers {workers}")

    here.cargo("build")
    wait_warm(here, ws)
    pid, workers, _ = status()
    kill(pid)
    time.sleep(1)
    check("workers exit when the daemon is killed", workers and not any(alive(p) for p in workers), f"workers {workers}")
    edit(ws, "six")
    _, again = here.cargo("run")
    check("the build after that starts a daemon again and succeeds", again.returncode == 0 and "hello world" in again.stdout and status()[0] not in (None, pid))
    here.cargo("stop")
    manners(work, ws, env, here, check, edit)
    abuse(work, ws, tree, env, check, status, alive)
    return checks


USE_AFTER_MOVE = """
fn taken(text: String) -> usize { text.len() }
pub fn moved() -> usize { let text = String::new(); taken(text) + text.len() }
"""


def manners(work, ws, env, here, check, edit):
    """What the commands say for themselves, a daemon that is going away, the store that is
    on without being asked for, and an error only rustc's borrow checker finds."""
    for argv in (["rustd", "--help"], ["rustd", "-h"], ["cargo", "rustd", "--help"], ["cargo", "rustd", "-h"]):
        ran = subprocess.run([RUSTD, *argv[1:]] if argv[0] == "rustd" else argv, cwd=ws, env=env, capture_output=True, text=True)
        listed = all(word in ran.stdout for word in ("enable", "disable", "status", "stop"))
        check(f"`{' '.join(argv)}` lists the commands and exits 0", ran.returncode == 0 and listed, f"exit {ran.returncode}")

    here.cargo("build")
    wait_warm(here, ws)
    for argv in ([RUSTD, "status"], [RUSTD, "cache", "stats"], [RUSTD, "--help"], ["cargo", "rustd", "status"], ["cargo", "rustd", "--help"]):
        shown = subprocess.Popen(argv, cwd=ws, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        shown.stdout.close()
        said = shown.stderr.read()
        code = shown.wait()
        name = " ".join(["rustd" if argv[0] == RUSTD else argv[0], *argv[1:]])
        # Cargo's subcommand ends by the signal; `cargo-rustd` has no status to pass on and says 1.
        check(f"`{name}` whose reader went away ends without a word", not said.strip() and code in (0, 1, -signal.SIGPIPE), f"exit {code} {said.strip()[:120]}")

    left = []
    for turn in range(5):
        here.cargo("stop")
        edit(ws, f"stopped {turn}")
        _, after = here.cargo("run")
        left.append(after.returncode == 0 and "hello world" in after.stdout and "answer" not in after.stderr and "EOF" not in after.stderr)
        wait_warm(here, ws)
    check("a build straight after `stop` starts a daemon and is not a fallback over the one leaving", all(left), f"{sum(left)} of 5")
    here.cargo("stop")

    lib = os.path.join(ws, "util/src/lib.rs")
    main = os.path.join(ws, "app/src/main.rs")
    for file, name in ((main, "the program's crate"), (lib, "a library of the workspace")):
        kept = open(file).read()
        here.cargo("build")
        wait_warm(here, ws)
        open(file, "a").write(USE_AFTER_MOVE)
        _, wrong = here.cargo("run")
        check(f"a use after a move in {name} fails a warm `cargo rustd run` with rustc's error", wrong.returncode == 101 and "E0382" in wrong.stderr, f"exit {wrong.returncode}")
        _, checked = here.cargo("check")
        check(f"and `cargo rustd check` of it", checked.returncode == 101 and "E0382" in checked.stderr, f"exit {checked.returncode}")
        open(file, "w").write(kept)
        _, mended = here.cargo("run")
        check("and the build once it is mended is right", mended.returncode == 0 and "hello world" in mended.stdout, how_built(mended.stderr))
    here.cargo("stop")

    for value, kept in ((None, True), ("off", False), ("0", False), ("", False)):
        cache = os.path.join(work, f"cache-{value}")
        home = os.path.join(work, f"home-store-{value}")
        own = base_env(home)
        own.pop("RUSTD_CACHE")
        own["XDG_CACHE_HOME"] = cache
        if value is not None:
            own["RUSTD_CACHE"] = value
        side = Side(ws, own, True)
        edit(ws, f"store {value}")
        side.cargo("build")
        wait_warm(side, ws)
        store = os.path.join(cache, "rustd", "store")
        filled = os.path.isdir(store) and any(files for _, _, files in os.walk(store))
        stats = subprocess.run([RUSTD, "cache", "stats"], cwd=ws, env=own, capture_output=True, text=True)
        side.cargo("stop")
        asked = "unset" if value is None else repr(value)
        check(f"RUSTD_CACHE {asked}: the store is {'on, in the user cache directory' if kept else 'off'}", filled == kept and (stats.returncode == 0) == kept, f"`rustd cache stats` exit {stats.returncode}")


def greets(out, greeting):
    """Whether the program of the workspace said `greeting` before what it says of its build."""
    return out.splitlines()[:1] == [greeting]


def abuse(work, ws, tree, env, check, status, alive):
    """The daemon under what it is not asked for politely: many clients at once, a client
    and a daemon killed while a build is asked for, and a client that is another user."""
    here = Side(ws, env, True)
    git = lambda *args: subprocess.run(["git", *args], cwd=ws, check=True, capture_output=True)
    trees = [ws, tree]
    for n in (2, 3):
        trees.append(os.path.realpath(os.path.join(work, f"life-worktree{n}")))
        git("worktree", "add", "-q", trees[-1])
    sides = [Side(root, env, True) for root in trees]
    for side in sides:
        side.cargo("build")
    for side in sides:
        wait_warm(side, side.ws)
    pid, workers, _ = status()
    for n, root in enumerate(trees):
        rewrite(os.path.join(root, "util/src/lib.rs"), 'format!("hello {name}")', f'format!("tree{n} {{name}}")')
    spawn = lambda root, *args: subprocess.Popen(["cargo", "rustd", *args], cwd=root, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, stdin=subprocess.DEVNULL, start_new_session=True)
    clients = [(n, spawn(root, "run", f"c{i}")) for i in range(4) for n, root in enumerate(trees)]
    ended = [(n, i // 4, client.communicate(timeout=600), client.returncode) for i, (n, client) in enumerate(clients)]
    right = [code == 0 and greets(out[0], f"tree{n} c{i}") for n, i, out, code in ended]
    by_daemon = sum(how_built(out[1]) == "daemon" for _, _, out, _ in ended)
    check("16 clients at once on 4 worktrees each run their own tree's program", all(right) and status()[0] == pid and status()[1] == workers, f"{sum(right)} of 16 right, {by_daemon} built by the daemon, {len(workers)} worker" + "".join(f"; tree{n} c{i}: exit {code}, {out[0][-80:]!r} {out[1][-200:]!r}" for (n, i, out, code), ok in zip(ended, right) if not ok))

    # A worker that is stopped holds the build a client asks for until it is continued.
    edit = lambda text: open(os.path.join(ws, "app/src/main.rs"), "a").write(f"\n// {text}\n")
    held = lambda: [os.kill(p, signal.SIGSTOP) for p in workers]
    freed = lambda: [os.kill(p, signal.SIGCONT) for p in workers if alive(p)]
    edit("held one")
    held()
    client = spawn(ws, "build")
    time.sleep(1)
    waits = client.poll() is None
    os.killpg(client.pid, signal.SIGKILL)
    client.wait()
    freed()
    _, after = here.cargo("run", "again")
    check("a client killed while its build is held leaves the daemon serving", waits and after.returncode == 0 and greets(after.stdout, "tree0 again") and status()[0] == pid and how_built(after.stderr) == "daemon", how_built(after.stderr))

    edit("held two")
    held()
    client = spawn(ws, "run", "killed")
    time.sleep(1)
    waits = client.poll() is None
    os.kill(pid, signal.SIGKILL)
    freed()
    out, err = client.communicate(timeout=600)
    said = how_built(err)
    check("the build whose daemon is killed is cargo's, is right, and says so", waits and client.returncode == 0 and greets(out, "tree0 killed") and "the daemon ended before it answered" in said, said)
    time.sleep(1)
    check("the workers of the killed daemon are gone", not any(alive(p) for p in workers), f"{workers}")
    wait_warm(here, ws)
    edit("after")
    _, after = here.cargo("run", "after")
    check("the build after that is a new daemon's", after.returncode == 0 and greets(after.stdout, "tree0 after") and status()[0] not in (None, pid) and how_built(after.stderr) == "daemon", how_built(after.stderr))

    other = os.path.join(work, "other-build")
    older = Side(ws, dict(env, PATH=other + os.pathsep + env["PATH"]), True)
    pid = status()[0]
    seen = next((line for line in older.cargo("status")[1].stdout.splitlines() if line.startswith("daemon:")), "")
    check("status by another build says the daemon is another build, and leaves it", "another build of rustd" in seen and status()[0] == pid, seen)
    edit("skew")
    _, skewed = older.cargo("run", "skew")
    said = how_built(skewed.stderr)
    check("a build by another build of rustd is cargo's, says why, and replaces the daemon", skewed.returncode == 0 and greets(skewed.stdout, "tree0 skew") and "another build of rustd" in said and status(older)[0] not in (None, pid) and not alive(pid), said)
    wait_warm(older, ws)
    edit("back")
    _, back = here.cargo("run", "back")
    check("and the first build of rustd replaces that one the same way", back.returncode == 0 and greets(back.stdout, "tree0 back") and "another build of rustd" in how_built(back.stderr) and status()[0] is not None, how_built(back.stderr))
    wait_warm(here, ws)

    stranger(env["RUSTD_HOME"], check)
    here.cargo("stop")


CONNECT = "import socket, sys\ns = socket.socket(socket.AF_UNIX)\ns.settimeout(5)\ntry:\n    s.connect(sys.argv[1])\nexcept OSError as e:\n    print('connect:', e.strerror)\n    sys.exit()\ns.sendall(sys.argv[2].encode() + b'\\n')\ntry:\n    print('answer:', s.recv(65536).decode())\nexcept OSError as e:\n    print('answer:', e)\n"

LISTEN = "import os, socket, sys\ns = socket.socket(socket.AF_UNIX)\ns.bind(sys.argv[1])\nos.chmod(sys.argv[1], 0o666)\ns.listen()\nprint('listening', flush=True)\ns.settimeout(20)\ntry:\n    c, _ = s.accept()\n    c.settimeout(5)\n    print('heard:', len(c.recv(65536)))\nexcept OSError as e:\n    print('heard: nothing,', e)\n"


def stranger(home, check):
    """A second user at the daemon's socket: kept out by the directory and the socket's
    modes, and turned away by the daemon when those are opened to it."""
    name = "rustd-stranger"
    sudo = lambda *argv: subprocess.run(["sudo", "-n", *argv], capture_output=True, text=True)
    if sudo("true").returncode != 0:
        check("a second user is turned away (needs sudo, which is not here)", os.environ.get("RUSTD_NO_SUDO") == "1", "not run")
        return
    # macOS has no `useradd`, and has a user with nothing of its own.
    DARWIN = sys.platform == "darwin"
    if DARWIN:
        name, made = "nobody", sudo("-u", "nobody", "true")
    else:
        sudo("userdel", name)
        made = sudo("useradd", "--system", "--no-create-home", "--shell", "/usr/sbin/nologin", name)
    sock = os.path.join(home, "daemon.sock")
    version = re.search(r"version (\S+),", subprocess.run(["cargo", "rustd", "status"], env=dict(os.environ, RUSTD_HOME=home, PATH=os.path.dirname(RUSTD) + os.pathsep + os.environ["PATH"]), capture_output=True, text=True).stdout)[1]
    request = json.dumps({"version": version, "replaces": False, "request": "Status"})
    knock = lambda user: subprocess.run([*(["sudo", "-n", "-u", user] if user else []), sys.executable, "-c", CONNECT, sock, request], capture_output=True, text=True, cwd="/").stdout.strip()
    try:
        mine = knock(None)
        check("the user's own raw request is answered", made.returncode == 0 and '"Status"' in mine, mine[:60])
        modes = (os.stat(home).st_mode & 0o777, os.stat(sock).st_mode & 0o777)
        closed = knock(name)
        check("a second user can't reach the socket", modes == (0o700, 0o600) and closed == "connect: Permission denied", f"modes {modes[0]:o} {modes[1]:o}, {closed}")
        # Every directory above has to let the second user through for the daemon to be the one that refuses.
        opened, at = [], home
        while at != "/" and os.stat(at).st_uid == os.getuid():
            opened.append((at, os.stat(at).st_mode & 0o7777))
            os.chmod(at, opened[-1][1] | 0o011)
            at = os.path.dirname(at)
        os.chmod(sock, 0o666)
        try:
            refused = knock(name)
        finally:
            os.chmod(sock, 0o600)
            for at, mode in opened:
                os.chmod(at, mode)
        log = open(os.path.join(home, "daemon.log")).read()
        check("with the modes opened, the daemon turns the second user away by its uid", refused.startswith("answer:") and "Status" not in refused and "the peer is user" in log, f"{refused!r}")
        mine = knock(None)
        check("and still answers its own user", '"Status"' in mine, mine[:60])
        # The other way round: the second user listens where a command line points the
        # user's client, which sends its request and its variables to whoever serves it.
        trap = os.path.join(tempfile.mkdtemp(prefix="rustd-trap-", dir="/tmp"), "sock")
        os.chmod(os.path.dirname(trap), 0o777)
        listener = subprocess.Popen(["sudo", "-n", "-u", name, sys.executable, "-c", LISTEN, trap], stdout=subprocess.PIPE, text=True, cwd="/")
        try:
            listener.stdout.readline()
            asked = subprocess.run([RUSTD, "sync", trap], capture_output=True, text=True, timeout=60)
            heard = listener.communicate(timeout=30)[0].strip()
        finally:
            listener.kill()
            sudo("rm", "-rf", os.path.dirname(trap))
        check("the client says nothing to a socket a second user serves", asked.returncode != 0 and "the peer is user" in asked.stderr and heard == "heard: 0", f"status {asked.returncode}, {heard}")
    finally:
        if not DARWIN:
            sudo("userdel", name)


WRAPPER = '#!/bin/sh\necho "$1" >> "{log}"\nexec "$@"\n'
DOCTEST = '/// ```\n/// assert_eq!(util::doubled(2), 4);\n/// ```\npub fn doubled(n: u32) -> u32 {\n    n * 2\n}\n'


def switches(work, env, check):
    """`rustd enable` and `rustd disable` over the cargo configurations a user may have."""
    rustd = lambda home, *args, cwd=None, **more: subprocess.run([RUSTD, *args], env=dict(env, CARGO_HOME=home, **more), cwd=cwd or work, capture_output=True, text=True)
    cases = [
        ("no configuration", "config.toml", None),
        ("a configuration with other settings", "config.toml", '# mine\n[net]\noffline = false\n\n[build]\njobs = 4 # all of them\n'),
        ("a configuration in the old file name", "config", '[term]\ncolor = "never"\n'),
        ("a dotted `build.rustc-wrapper`", "config.toml", 'build.rustc-wrapper = "/usr/bin/env"\n'),
        ("a workspace wrapper of the user's", "config.toml", '[build]\nrustc-workspace-wrapper = "/usr/bin/env"\n'),
    ]
    for n, (name, file, text) in enumerate(cases):
        home = os.path.join(work, f"switch-{n}")
        os.makedirs(home)
        config = os.path.join(home, file)
        if text is not None:
            open(config, "w").write(text)
        on = rustd(home, "enable")
        named = os.path.exists(config) and "rustd-rustc" in open(config).read()
        again = rustd(home, "enable")
        seen = rustd(home, "status").stdout.splitlines()[0]
        off = rustd(home, "disable")
        same = (open(config).read() if os.path.exists(config) else None) == text
        left = sorted(os.listdir(home)) == ([file] if text is not None else [])
        check(f"enable and disable over {name} leave it as it was", on.returncode == 0 and named and again.returncode == 0 and "through rustd" in seen and off.returncode == 0 and same and left, f"{seen[:70]}; {'same bytes' if same else 'DIFFERENT'}")

    home = os.path.join(work, "switch-edited")
    os.makedirs(home)
    config = os.path.join(home, "config.toml")
    open(config, "w").write('[build]\njobs = 2\n')
    rustd(home, "enable")
    open(config, "a").write('\n[net]\noffline = false\n')
    off = rustd(home, "disable")
    text = open(config).read()
    check("disable after the user edited the file takes rustd's line out and keeps the edit", off.returncode == 0 and "rustd" not in text and "offline = false" in text and "jobs = 2" in text, repr(text))

    rustd(home, "enable")
    project = os.path.join(work, "switch-project")
    os.makedirs(os.path.join(project, ".cargo"))
    open(os.path.join(project, ".cargo/config.toml"), "w").write('[build]\nrustc-workspace-wrapper = "/usr/bin/env"\n')
    below = rustd(home, "status", cwd=project).stdout
    check("status says a project's own configuration has cargo not run rustd there", "not below " + project in below, below.splitlines()[1] if below.count("\n") > 1 else below)
    shell = rustd(home, "status", RUSTC_WORKSPACE_WRAPPER="/usr/bin/env").stdout
    check("status says a RUSTC_WORKSPACE_WRAPPER of the shell has cargo not run rustd", "not in this shell" in shell)
    rustd(home, "disable")


def plain(work):
    """Plain cargo once `rustd enable` was run. The result is each check with whether it held."""
    checks = []

    def check(name, held, detail=""):
        checks.append((name, bool(held), detail))
        print(f"  {'ok ' if held else 'NOT'} {name}  {detail}", flush=True)

    env = base_env(os.path.join(work, "home-plain-checks"))
    switches(work, env, check)

    ws = os.path.realpath(os.path.join(work, "plain"))
    for file, text in FLAG_WORKSPACE.items():
        os.makedirs(os.path.dirname(os.path.join(ws, file)), exist_ok=True)
        open(os.path.join(ws, file), "w").write(text)
    open(os.path.join(ws, "util/src/lib.rs"), "a").write(DOCTEST)
    open(os.path.join(ws, "util/build.rs"), "w").write('fn main() {\n    println!("cargo:rustc-cfg=built");\n}\n')
    open(os.path.join(ws, ".gitignore"), "w").write("target\ntarget-ref\n")
    git = lambda *args: subprocess.run(["git", "-c", "user.name=rustd", "-c", "user.email=rustd@localhost", *args], cwd=ws, check=True, capture_output=True)
    git("init", "-q"), git("add", "."), git("commit", "-qm", "first")

    # A wrapper of the user's around rustc, as sccache is, and one around the workspace's crates.
    home = os.path.join(work, "plain-cargo-home")
    os.makedirs(home)
    logs = {}
    for name in ("outer", "inner"):
        logs[name] = os.path.join(work, f"plain-{name}.log")
        script = os.path.join(work, f"plain-{name}.sh")
        open(script, "w").write(WRAPPER.format(log=logs[name]))
        os.chmod(script, 0o755)
    before = f'[build]\nrustc-wrapper = "{work}/plain-outer.sh"\nrustc-workspace-wrapper = "{work}/plain-inner.sh"\n'
    open(os.path.join(home, "config.toml"), "w").write(before)
    downloads(home)
    env = dict(env, CARGO_HOME=home)
    on = subprocess.run([RUSTD, "enable"], env=env, capture_output=True, text=True)
    here, alone = Side(ws, env, "plain"), Side(ws, base_env(env["RUSTD_HOME"]), False)
    calls = lambda name: open(logs[name]).read().splitlines() if os.path.exists(logs[name]) else []
    edit = lambda text, root=ws: rewrite(os.path.join(root, "util/src/lib.rs"), 'format!("', f'format!("{text} ')
    touch = lambda text, root=ws: open(os.path.join(root, "app/src/main.rs"), "a").write(f"\n// {text}\n")

    def status(rustd=RUSTD):
        text = subprocess.run([rustd, "status"], env=env, cwd=ws, capture_output=True, text=True).stdout
        pid = re.search(r"^daemon: pid (\d+)", text, re.M)
        return (int(pid[1]) if pid else None), [int(p) for p in re.findall(r"worker: pid (\d+)", text)], text

    def same(command, *args, more=None):
        """One command by plain cargo with rustd enabled and by cargo alone: what differs."""
        _, mine = here.cargo(command, *args, env=more)
        _, other = alone.cargo(command, *args, env=more)
        compare = test_results if command == "test" else program_output
        return mine, here.how_built(mine.stderr), difference(compare(mine), compare(other))

    first, how, wrong = same("run")
    outer, inner = calls("outer"), calls("inner")
    check("the first plain `cargo run` is rustc's and is right", first.returncode == 0 and not wrong and how.startswith("cargo"), f"{how}; {wrong or 'same as cargo'}")
    check("the user's `rustc-wrapper` still runs, around rustd's wrapper", "chained" not in on.stdout and any("rustd-rustc" in c for c in outer), f"{len(outer)} calls")
    check("the user's workspace wrapper still runs, under rustd's", "plain-inner.sh" in on.stdout and len(inner) > 0 and not any("rustd-rustc" in c for c in inner), f"{len(inner)} calls")
    took, states = wait_warm(here, ws)
    pid, workers, _ = status()
    check("the daemon was started by that build and has loaded it", pid is not None and "ready" in " ".join(states), f"pid {pid}, {'; '.join(states)}, {took:.1f}s")

    touch("one")
    mine, how, wrong = same("run", "one")
    check("an edit of the program is linked by the daemon", mine.returncode == 0 and not wrong and how == "daemon", f"{how}; {wrong or 'same as cargo'}")
    edit("lib")
    mine, how, wrong = same("run", "lib")
    check("an edit of a library of the workspace is built by the daemon into the program", mine.returncode == 0 and not wrong and how == "daemon" and greets(mine.stdout, "lib hello lib"), f"{how}; {wrong or 'same as cargo'}")
    took, noop = here.cargo("run")
    check("a build with nothing changed runs nothing", noop.returncode == 0 and "Compiling" not in noop.stderr, f"{took:.2f}s")

    cases = [
        ("test", [], None), ("test", ["--doc"], None), ("test", ["--no-fail-fast"], {"FLAGS_FAIL": "1"}),
        ("run", ["--example", "demo"], None), ("run", ["--release"], None),
        ("run", ["--target", "x86_64-unknown-linux-gnu"], None), ("run", ["--bin", "other"], None),
        ("run", ["--features", "loud", "-p", "app"], None), ("run", ["--", "fail"], None),
        ("check", [], None), ("check", ["--all-targets"], None), ("build", ["--workspace", "--all-targets"], None),
    ]
    for round in ("first", "edited"):
        for command, args, more in cases:
            mine, how, wrong = same(command, *args, more=more)
            name = " ".join(["cargo", command, *args, *(f"{k}={v}" for k, v in (more or {}).items())])
            check(f"`{name}`, {round}", not wrong, f"{how[:90]}; {wrong or 'same as cargo'}")
        wait_warm(here, ws)
        touch(round)
        edit(round)

    broken = os.path.join(ws, "app/src/main.rs")
    kept = open(broken).read()
    open(broken, "a").write("\nfn broken() -> u32 { \"text\" }\n")
    mine, how, wrong = same("build")
    check("a build that does not compile fails as cargo's does, with rustc's error", mine.returncode == 101 and "error[E0308]" in mine.stderr and not wrong, how[:90])
    open(broken, "w").write(kept)
    mine, how, wrong = same("run", "mended")
    check("and the build after it is mended is right", mine.returncode == 0 and not wrong, f"{how[:90]}; {wrong or 'same as cargo'}")

    wait_warm(here, ws)
    for file, name in ((broken, "the program's crate"), (os.path.join(ws, "util/src/lib.rs"), "a library of the workspace")):
        kept = open(file).read()
        open(file, "a").write(USE_AFTER_MOVE)
        mine, how, wrong = same("run", "moved")
        check(f"a use after a move in {name} fails a warm `cargo run` with rustc's error", mine.returncode == 101 and "error[E0382]" in mine.stderr and not wrong, how[:90])
        open(file, "w").write(kept)
        mine, how, wrong = same("run", "mended")
        check("and the build after it is mended is right", mine.returncode == 0 and not wrong, f"{how[:90]}; {wrong or 'same as cargo'}")
        wait_warm(here, ws)

    root = os.path.join(work, "plain-installed")
    _, installed = here.cargo("install", "--path", "app", "--root", root, "--bin", "app")
    ran = subprocess.run([os.path.join(root, "bin/app"), "installed"], capture_output=True, text=True)
    check("`cargo install` installs a program that runs", installed.returncode == 0 and ran.returncode == 0 and ran.stdout.splitlines()[0].endswith("hello installed"), ran.stdout.strip() or installed.stderr[-200:])

    # What rust-analyzer does while the user builds: `cargo check` with JSON messages.
    wait_warm(here, ws)
    touch("checked")
    checker = subprocess.Popen(["cargo", "check", "--workspace", "--all-targets", "--message-format=json"], cwd=ws, env=here.env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    mine, how, wrong = same("run", "checked")
    out, _ = checker.communicate(timeout=600)
    whole = all(line.startswith("{") for line in out.splitlines())
    check("a `cargo check` with JSON messages at the same time as a build: both are right", checker.returncode == 0 and whole and mine.returncode == 0 and not wrong, f"{how[:60]}; {len(out.splitlines())} messages; {wrong or 'same as cargo'}")

    trees = [ws]
    for n in (1, 2, 3):
        trees.append(os.path.realpath(os.path.join(work, f"plain-worktree{n}")))
        git("worktree", "add", "-q", "--detach", trees[-1])
    sides = [Side(root, env, "plain") for root in trees]
    for side in sides[1:]:
        side.cargo("build")
    for side in sides:
        wait_warm(side, side.ws)
    pid, workers, text = status()
    check("the worktrees are loaded by the one daemon", pid is not None and all(f"    {root}: ready" in text for root in trees), f"{len(workers)} workers")
    for n, root in enumerate(trees[1:], 1):
        rewrite(os.path.join(root, "util/src/lib.rs"), 'format!("hello {name}")', f'format!("tree{n} {{name}}")')
    here.how_built("")
    spawn = lambda root, *args: subprocess.Popen(["cargo", *args], cwd=root, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, stdin=subprocess.DEVNULL, start_new_session=True)
    clients = [(n, i, spawn(root, "run", f"c{i}")) for i in range(4) for n, root in enumerate(trees) if n]
    ended = [(n, i, client.communicate(timeout=600), client.returncode) for n, i, client in clients]
    right = [code == 0 and greets(out[0], f"tree{n} c{i}") for n, i, out, code in ended]
    check("12 plain cargo builds at once on 3 worktrees each run their own tree's program", all(right) and status()[0] == pid, f"{sum(right)} of 12 right; {here.how_built('')[:80]}")

    for side in sides:
        wait_warm(side, side.ws)
    pid, workers, _ = status()
    touch("killed")
    signalled = lambda sent: [os.kill(p, sent) for p in workers if alive(p)]
    signalled(signal.SIGSTOP)
    client = spawn(ws, "run", "killed")
    time.sleep(1.5)
    waits = client.poll() is None
    os.kill(pid, signal.SIGKILL)
    signalled(signal.SIGCONT)
    out, err = client.communicate(timeout=600)
    said = here.how_built(err)
    check("the build whose daemon is killed is rustc's, succeeds, and says so", waits and client.returncode == 0 and out.splitlines()[0].endswith("killed") and "the daemon ended" in said and "rustd: " in err, said[:120])
    wait_warm(here, ws)
    touch("after")
    mine, how, wrong = same("run", "after")
    check("the build after that is a new daemon's", mine.returncode == 0 and not wrong and how == "daemon" and status()[0] not in (None, pid), how[:90])

    # Another build of rustd: the same files in another place have another version.
    other = os.path.join(work, "plain-other-build")
    os.makedirs(other)
    for name in ("rustd", "cargo-rustd", "rustd-rustc"):
        shutil.copy(os.path.join(os.path.dirname(RUSTD), name), os.path.join(other, name))
    pid = status()[0]
    subprocess.run([RUSTD, "disable"], env=env, capture_output=True)
    subprocess.run([os.path.join(other, "rustd"), "enable"], env=env, capture_output=True)
    touch("skew")
    mine, how, wrong = same("run", "skew")
    theirs = os.path.join(other, "rustd")
    began = time.time()
    while "    " + ws + ": ready" not in status(theirs)[2] and time.time() - began < 120:
        time.sleep(0.25)
    newer = status(theirs)[0]
    check("plain cargo by another build of rustd is rustc's and replaces the daemon", mine.returncode == 0 and not wrong and how.startswith("cargo") and newer not in (None, pid) and not alive(pid), f"{how[:120]}; daemon {pid} then {newer}")
    touch("skewed")
    mine, how, wrong = same("run", "skewed")
    check("and the build after that is the new daemon's", mine.returncode == 0 and not wrong and how == "daemon" and status(theirs)[0] == newer, how[:90])
    subprocess.run([os.path.join(other, "rustd"), "disable"], env=env, capture_output=True)

    seen = subprocess.run([RUSTD, "status"], env=env, cwd=ws, capture_output=True, text=True).stdout
    counted = re.search(r"(\d+) programs? linked by the daemon, (\d+) by rustc", seen)
    check("status counts what the daemon linked and why rustc linked the rest", counted and int(counted[1]) > 0 and "the daemon ended" in seen, counted[0] if counted else seen[:200])
    back = open(os.path.join(home, "config.toml")).read()
    check("after `rustd disable` the configuration is the user's again, byte for byte", back == before)
    touch("off")
    n = len(calls("outer"))
    mine, how, wrong = same("run", "off")
    check("and plain cargo is cargo alone again, through the user's wrappers", mine.returncode == 0 and not wrong and not re.search(r"^rustd:|\[rustd\]", mine.stderr, re.M) and not any("rustd-rustc" in c for c in calls("outer")[n:]) and len(calls("outer")) > n, f"exit {mine.returncode}, {wrong or 'same as cargo'}, {len(calls('outer')) - n} calls of the wrapper; {mine.stderr.strip()[-300:]}")
    subprocess.run([RUSTD, "stop"], env=env, capture_output=True)
    subprocess.run([os.path.join(other, "rustd"), "stop"], env=env, capture_output=True)
    return checks


def worktree(ours, row, build, run, edits, work):
    """A second checkout of the project's repository, at the commit the first started from:
    built from nothing, which is cargo's, then edited as the first was and built again. What
    its program says after the edit has to be what the first one's says."""
    git = lambda *args: subprocess.run(["git", "-c", "user.name=rustd", "-c", "user.email=rustd@localhost", *args], cwd=ours.ws, capture_output=True, text=True)
    # One place for each way of building: a daemon still loading the first writes there
    # while it is removed, and what that leaves is not the second's.
    mode = "plain" if ours.plain else "rustd"
    tree = os.path.realpath(os.path.join(work, f"{row['project']}-worktree-{mode}"))
    added = git("worktree", "add", "-q", "--detach", tree)
    if added.returncode != 0:
        row["wrong"].append(f"worktree: {added.stderr[-200:]}")
        return
    there = Side(tree, ours.env, "plain" if ours.plain else True)
    ours.how_built("")
    there.read = ours.read

    def step(label, command, args):
        took, ran = there.cargo(command, *args)
        row["times"][label] = (round(took, 2), None)
        row["built"][label] = there.how_built(ran.stderr)
        print(f"  {label:12} rustd {took:7.2f}s  {row['built'][label]:8.80}  exit {ran.returncode}", flush=True)
        return ran

    if step("tree build", "build", build).returncode != 0:
        row["wrong"].append("tree build: failed")
    took, states = wait_warm(there, tree)
    row["times"]["tree warm-up"], row["built"]["tree warm-up"] = (round(took, 2), None), "; ".join(states)
    print(f"  tree warm-up {took:7.2f}s  {'; '.join(states)[:150]}", flush=True)
    for file, old, new in edits:
        rewrite(os.path.join(tree, file), old, new)
    if step("tree edit", "build", build).returncode != 0:
        row["wrong"].append("tree edit: failed")
    if run:
        _, first = ours.cargo("run", *run)
        ours.how_built(first.stderr)
        second = step("tree run", "run", run)
        wrong = difference(program_output(second), program_output(first))
        if wrong:
            row["wrong"].append(f"tree run: {wrong}")
    git("worktree", "remove", "--force", tree)
    shutil.rmtree(tree, ignore_errors=True)


def table(rows):
    """The table of the report: for every step the seconds of `cargo rustd` and of cargo."""
    steps = ["cold build", "warm-up", "warm build", "cold test", "test warm-up", "warm test", "edit build", "edit test", "broken", "mended", "tree build", "tree warm-up", "tree edit"]
    out = ["| Project | " + " | ".join(steps) + " | Functions by rustd | Differences |", "|---|" + "---:|" * (len(steps) + 1) + "---|"]
    for row in rows:
        cells = []
        for step in steps:
            ours, theirs = row["times"].get(step, (None, None))
            who = row["built"].get(step, "")
            mark = "" if who in ("daemon", "cargo") or "warm-up" in step else " (c)" if who.startswith("cargo") else ""
            if "warm-up" in step:
                mark = "" if who and all(s == "ready" for s in who.split("; ")) else " (f)"
            cells.append("-" if ours is None else f"{ours:.2f}{mark}" if theirs is None else f"{'**' if who == 'daemon' else ''}{ours:.2f}{'**' if who == 'daemon' else ''}{mark} / {theirs:.2f}")
        done, of = row.get("share", (0, 0))
        share = f"{done} of {of} ({100 * done // of}%)" if of else "-"
        out.append(f"| {row['project']} {row['tag']} | " + " | ".join(cells) + f" | {share} | {len(row['wrong'])} |")
    return "\n".join(out)


def main():
    argv = sys.argv[1:]
    names = [a for a in argv if not a.startswith("--")] or (SMALL if "--small" in argv else list(PROJECTS))
    modes = [m for m in MODES if f"--{m}" in argv] or list(MODES)
    # As the daemon names it: with no link and no short name in it.
    # The path of a socket is at most 104 bytes on macOS, and its temporary directory is long.
    work = os.path.realpath(tempfile.mkdtemp(prefix="rustd-cargo-", dir="/tmp" if sys.platform == "darwin" else None))
    print(f"work {work}; rustd {RUSTD}", flush=True)
    result = dict(rustd=RUSTD, toolchain=TOOLCHAIN, projects=[], flags=[], daemon=[], wrong=[])
    if "--only-checks" not in argv:
        for name in names:
            for mode in modes:
                print(f"{name}, {MODES[mode]}:", flush=True)
                row = project(name, PROJECTS[name], work, mode)
                result["projects"].append(row)
                result["wrong"] += [f"{name}, {mode}: {w}" for w in row["wrong"]]
                if "--keep" not in argv:
                    shutil.rmtree(os.path.join(work, name), ignore_errors=True)
    if "--only-projects" not in argv:
        print("flags:", flush=True)
        rows, wrong, counted = flags(work)
        result["flags"], result["flags_status"] = rows, counted
        result["wrong"] += wrong
        print("picks:", flush=True)
        result["picks"] = picks(work)
        result["wrong"] += [f"picks: {name} {detail}" for name, held, detail in result["picks"] if not held]
        print("daemon:", flush=True)
        result["daemon"] = daemon(work)
        result["wrong"] += [f"daemon: {name} {detail}" for name, held, detail in result["daemon"] if not held]
        print("plain cargo:", flush=True)
        result["plain"] = plain(work)
        result["wrong"] += [f"plain cargo: {name} {detail}" for name, held, detail in result["plain"] if not held]
    # A run of some of it leaves the rows of the rest as the run before wrote them.
    llvm = os.environ.get("RUSTD_BACKEND") == "llvm"
    stored = os.path.join(HERE, "cargo_rustd_llvm.json" if llvm else "cargo_rustd.json")
    before = json.load(open(stored)) if os.path.exists(stored) else {}
    rows = {(row["project"], row.get("mode", "rustd")): row for row in before.get("projects", []) + result["projects"]}
    result["projects"] = [rows[name, mode] for name in PROJECTS for mode in MODES if (name, mode) in rows]
    if "--only-projects" in argv:
        result.update({part: before.get(part, []) for part in ("flags", "flags_status", "picks", "daemon", "plain")})
    json.dump(result, open(stored, "w"), indent=1)
    with open(os.path.join(HERE, "CARGO-RUSTD-LLVM.md" if llvm else "CARGO-RUSTD.md"), "w") as md:
        md.write(f"# cargo rustd against cargo, {'LLVM at -O2' if llvm else 'Cranelift'}\n\nGenerated by `harness/cargo_rustd.py`. Seconds, rustd / cargo alone for the same step; the steps in the second checkout (`tree`) are rustd's only. "
                 "Bold is a build the daemon did; `(c)` one cargo did in its place, with the reason below; `(f)` a warm-up that failed. "
                 "Differences counts the steps whose program output, exit status or test results were not cargo's.\n\n")
        for mode, title in MODES.items():
            md.write(f"## {title}\n\n" + table([r for r in result["projects"] if r.get("mode", "rustd") == mode]) + "\n\n")
        for row in result["projects"]:
            reasons = [f"{step}: {who}" for step, who in row["built"].items() if who.startswith(("cargo: ", "unchanged: ")) or "failed" in who]
            if reasons or row["wrong"]:
                md.write(f"**{row['project']}, {MODES[row.get('mode', 'rustd')]}**\n\n" + "".join(f"- {r}\n" for r in reasons + row["wrong"]) + "\n")
        if result["flags"]:
            md.write("## Flags\n\n| Round | Command | Built by | Exit | Against cargo |\n|---|---|---|---:|---|\n")
            md.writelines(f"| {label} | `{shown}` | {who} | {code} | {same} |\n" for label, shown, who, code, same in result["flags"])
        if result.get("picks"):
            md.write("\n## Which tests run\n\n" + "".join(f"- {'ok' if held else 'NOT'}: {name} {detail}\n" for name, held, detail in result["picks"]))
        if result["daemon"]:
            md.write("\n## Daemon\n\n" + "".join(f"- {'ok' if held else 'NOT'}: {name} {detail}\n" for name, held, detail in result["daemon"]))
        if result.get("plain"):
            md.write("\n## Plain cargo after `rustd enable`\n\n" + "".join(f"- {'ok' if held else 'NOT'}: {name} {detail}\n" for name, held, detail in result["plain"]))
    if "--keep" not in argv:
        shutil.rmtree(work, ignore_errors=True)
    print("PASS" if not result["wrong"] else "FAIL:\n" + "\n".join(result["wrong"]))
    sys.exit(1 if result["wrong"] else 0)


if __name__ == "__main__":
    main()
