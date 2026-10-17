#!/usr/bin/env python3
"""cargo_rustd.py: `cargo rustd` against plain cargo on real projects, unmodified.

    cargo_rustd.py [PROJECT...] [--only-projects] [--only-checks] [--keep]

Three parts, all of which have to pass.

Projects. Each one below is cloned at its tag into `RUSTD_PROJECTS` (~/rustd-projects) when
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

Flags. A small workspace made here is built and run with each flag `cargo rustd` passes
through, cold and after an edit, and compared with cargo the same way.

Daemon. Started on demand, reused, shared by a git worktree, alive after its client is
killed, replaced when the client is another build of rustd, gone when idle, and its workers
gone with it.

Writes CARGO-RUSTD.md and cargo_rustd.json next to this file.
"""
import collections, json, os, re, shutil, signal, subprocess, sys, tempfile, time, urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
RUSTD = os.environ.get("RUSTD_BIN") or os.path.join(ROOT, "target/release/rustd")
TOOLCHAIN = open(os.path.join(HERE, "toolchain")).read().strip()
SOURCES = os.environ.get("RUSTD_PROJECTS") or os.path.expanduser("~/rustd-projects")
WARMUP_SECS = 1800

PROJECTS = {
    "ripgrep": dict(
        repo="BurntSushi/ripgrep", tag="15.1.0",
        run=["-n", "--no-heading", "--sort", "path", "fn main", "crates/core/main.rs", "build.rs"],
        edits=[("crates/core/main.rs", "fn main() -> ExitCode {", 'fn main() -> ExitCode {\n    eprintln!("edited main");'),
               ("crates/printer/src/standard.rs", "pub fn new() -> StandardBuilder {", 'pub fn new() -> StandardBuilder {\n        eprintln!("edited printer");')],
    ),
    "fd": dict(
        repo="sharkdp/fd", tag="v10.3.0",
        run=["--extension", "rs", "main", "src"],
        edits=[("src/main.rs", "fn main() {", 'fn main() {\n    eprintln!("edited main");')],
    ),
    "bat": dict(
        repo="sharkdp/bat", tag="v0.26.0",
        run=["--plain", "--color=never", "rustfmt.toml"],
        edits=[("src/bin/bat/main.rs", "fn main() {", 'fn main() {\n    eprintln!("edited main");')],
    ),
    "hyperfine": dict(
        repo="sharkdp/hyperfine", tag="v1.19.0",
        run=["--help"],
        edits=[("src/main.rs", "fn main() {", 'fn main() {\n    eprintln!("edited main");')],
    ),
    "just": dict(
        repo="casey/just", tag="1.43.0",
        run=["--list", "--unsorted"],
        edits=[("src/main.rs", "fn main() {", 'fn main() {\n  eprintln!("edited main");')],
    ),
    "axum": dict(
        repo="tokio-rs/axum", tag="axum-v0.8.6",
        build=["-p", "example-hello-world"], serves="http://127.0.0.1:3000/",
        test=["-p", "example-testing"],
        edits=[("examples/hello-world/src/main.rs", "<h1>Hello, World!</h1>", "<h1>Hello, edited world!</h1>"),
               ("examples/testing/src/main.rs", "fn app() -> Router {", 'fn app() -> Router {\n    eprintln!("edited app");')],
        broken="examples/hello-world/src/main.rs",
    ),
    "nushell": dict(
        repo="nushell/nushell", tag="0.108.0",
        run=["-n", "-c", "[3 1 2] | sort | str join ','"],
        test=["-p", "nu-path"],
        edits=[("src/main.rs", "fn main() -> Result<()> {", 'fn main() -> Result<()> {\n    eprintln!("edited main");')],
    ),
}

# What cargo and `cargo rustd` print about the build, which is not the program's output.
STATUS = re.compile(r"^\s*((Compiling|Finished|Running|Downloading|Downloaded|Blocking|Locking|Updating|Adding|Fresh|Dirty|Doc-tests)\b|rustd:|warning:)")


def base_env(home):
    env = dict(os.environ, RUSTUP_TOOLCHAIN=TOOLCHAIN, RUSTD_HOME=home, CARGO_TERM_COLOR="never", RUSTD_ALL_TESTS="1")
    env["PATH"] = os.path.dirname(RUSTD) + os.pathsep + env["PATH"]
    for name in ("RUSTFLAGS", "CARGO_TARGET_DIR", "RUSTC_WORKSPACE_WRAPPER", "CARGO_BUILD_TARGET"):
        env.pop(name, None)
    return env


def timed(argv, cwd, env, timeout=7200):
    began = time.time()
    ran = subprocess.run(argv, cwd=cwd, env=env, capture_output=True, text=True, timeout=timeout, stdin=subprocess.DEVNULL)
    return time.time() - began, ran


class Side:
    """One of the two builds of a checkout: `cargo rustd`, or cargo into `target-ref`."""

    def __init__(self, ws, env, rustd):
        self.ws, self.rustd = ws, rustd
        self.env = env if rustd else dict(env, CARGO_TARGET_DIR=os.path.join(ws, "target-ref"))

    def cargo(self, command, *args, env=None, timeout=7200):
        argv = ["cargo", *(["rustd"] if self.rustd else []), command, *args]
        return timed(argv, self.ws, dict(self.env, **(env or {})), timeout)


def how_built(stderr):
    """Whether the daemon built it, or why cargo did."""
    reason = re.search(r"^rustd: (.*); (building with cargo|this is `cargo \w+`)$", stderr, re.M)
    if reason:
        return "cargo: " + reason[1]
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
    tests = collections.Counter(re.findall(r"^test (.+?) \.\.\. (ok|FAILED|ignored)", ran.stdout, re.M))
    totals = sorted(re.findall(r"^test result: (\w+)\. (\d+ passed; \d+ failed; \d+ ignored; \d+ measured; \d+ filtered out)", ran.stdout, re.M))
    return ran.returncode, tests, totals


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


def served(side, command, url):
    """What the server `cargo run` starts on `side` answers on `url`."""
    argv = ["cargo", *(["rustd"] if side.rustd else []), "run", *command]
    server = subprocess.Popen(argv, cwd=side.ws, env=side.env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True, start_new_session=True)
    try:
        for _ in range(3000):
            if server.poll() is not None:
                return f"exited {server.returncode}: {server.stderr.read()[-300:]}"
            try:
                return urllib.request.urlopen(url, timeout=5).read().decode()
            except OSError:
                time.sleep(0.1)
        return "no answer"
    finally:
        os.killpg(server.pid, signal.SIGKILL)
        server.wait()
        # The port is free again before the other side's server asks for it.
        for _ in range(100):
            try:
                urllib.request.urlopen(url, timeout=1)
                time.sleep(0.05)
            except OSError:
                break


def wait_warm(side, root):
    """Waits until the daemon has every build of `root` loaded or has given up on one. The
    result is the seconds it took and how each ended."""
    began = time.time()
    while time.time() - began < WARMUP_SECS:
        _, status = side.cargo("status")
        states = re.findall(rf"^    {re.escape(root)}: (.*)$", status.stdout, re.M)
        if states and not any(s.startswith("loading") for s in states):
            return time.time() - began, states
        time.sleep(0.25)
    return time.time() - began, ["not loaded in time"]


def rewrite(path, old, new):
    """Replaces the first `old` in the file by `new`. The result is the text it had."""
    text = open(path).read()
    assert old in text, f"{old!r} is not in {path}"
    open(path, "w").write(text.replace(old, new, 1))
    return text


def fetch(name, spec):
    source = os.path.join(SOURCES, name)
    if not os.path.isdir(source):
        os.makedirs(SOURCES, exist_ok=True)
        subprocess.run(["git", "clone", "-q", "--depth", "1", "--branch", spec["tag"], f"https://github.com/{spec['repo']}", source], check=True)
    return source


def project(name, spec, work):
    """Runs the steps of one project. The result is the row of the table: times, who built
    each step, and every difference from cargo."""
    source = fetch(name, spec)
    ws = os.path.realpath(os.path.join(work, name))
    shutil.copytree(source, ws, symlinks=True, ignore=shutil.ignore_patterns("target", "target-ref"))
    env = base_env(os.path.join(work, "home"))
    ours, theirs = Side(ws, env, True), Side(ws, env, False)
    build, run, test = spec.get("build", []), spec.get("run", []), spec.get("test", [])
    row = dict(project=name, tag=spec["tag"], wrong=[], times={}, built={})
    subprocess.run(["cargo", "fetch"], cwd=ws, env=env, capture_output=True)
    ours.cargo("stop")

    def step(label, command, args, compare):
        """Runs `command` on both sides and compares what `compare` reads from each."""
        (took, mine), (reference, other) = ours.cargo(command, *args), theirs.cargo(command, *args)
        row["times"][label] = (round(took, 2), round(reference, 2))
        row["built"][label] = how_built(mine.stderr)
        wrong = difference(compare(mine), compare(other))
        if wrong:
            row["wrong"].append(f"{label}: {wrong}")
            open(os.path.join(work, f"{name}-{label}.rustd.txt"), "w").write(mine.stdout + mine.stderr)
            open(os.path.join(work, f"{name}-{label}.cargo.txt"), "w").write(other.stdout + other.stderr)
        print(f"  {label:12} rustd {took:7.2f}s  cargo {reference:7.2f}s  {row['built'][label]:8.80}  {wrong or 'same'}", flush=True)

    def runs(label):
        if "serves" in spec:
            mine, other = served(ours, build, spec["serves"]), served(theirs, build, spec["serves"])
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
    _, counted = ours.cargo("status")
    row["status"] = counted.stdout
    ours.cargo("stop")
    return row


FLAG_WORKSPACE = {
    "Cargo.toml": '[workspace]\nmembers = ["app", "util"]\nresolver = "2"\n\n[profile.quick]\ninherits = "dev"\n',
    "util/Cargo.toml": '[package]\nname = "util"\nversion = "0.1.0"\nedition = "2021"\n\n[features]\nloud = []\n',
    "util/src/lib.rs": 'pub fn greeting(name: &str) -> String {\n    let text = format!("hello {name}");\n    if cfg!(feature = "loud") { text.to_uppercase() } else { text }\n}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn greets() {\n        assert!(super::greeting("a").to_lowercase().starts_with("hello"));\n    }\n}\n',
    "app/Cargo.toml": '[package]\nname = "app"\nversion = "0.1.0"\nedition = "2021"\ndefault-run = "app"\n\n[features]\nloud = ["util/loud"]\n\n[dependencies]\nutil = { path = "../util" }\n',
    "app/src/main.rs": 'fn main() {\n    let name = std::env::args().nth(1).unwrap_or("world".to_string());\n    println!("{}", util::greeting(&name));\n    if name == "fail" {\n        std::process::exit(3);\n    }\n}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn in_the_program() {\n        assert_eq!(util::greeting("x").len(), 7);\n    }\n}\n',
    "app/src/bin/other.rs": 'fn main() {\n    println!("other: {}", util::greeting("other"));\n}\n',
    "app/examples/demo.rs": 'fn main() {\n    println!("demo: {}", util::greeting("demo"));\n}\n',
    "app/tests/it.rs": '#[test]\nfn greets_it() {\n    assert_eq!(util::greeting("it").to_lowercase(), "hello it");\n}\n\n#[test]\nfn fails_when_asked() {\n    assert!(std::env::var("FLAGS_FAIL").is_err());\n}\n',
}

# Each: the subcommand, its arguments, the variables it runs with.
FLAG_CASES = [
    ("run", [], {}),
    ("run", ["--", "fail"], {}),
    ("run", ["--release"], {}),
    ("run", ["--target", "x86_64-unknown-linux-gnu"], {}),
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
            differs = difference(read(mine), read(other))
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
    round_of("edit")
    _, counted = ours.cargo("status")
    ours.cargo("stop")
    return rows, wrong, counted.stdout


def daemon(work):
    """The life of the daemon. The result is each check with whether it held."""
    ws = os.path.realpath(os.path.join(work, "life"))
    for file, text in FLAG_WORKSPACE.items():
        os.makedirs(os.path.dirname(os.path.join(ws, file)), exist_ok=True)
        open(os.path.join(ws, file), "w").write(text)
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

    def alive(pid):
        if sys.platform == "darwin":
            r = subprocess.run(["ps", "-o", "state=", "-p", str(pid)], capture_output=True, text=True)
            return r.returncode == 0 and bool(r.stdout.strip()) and not r.stdout.strip().startswith("Z")
        return os.path.exists(f"/proc/{pid}") and "Z" not in open(f"/proc/{pid}/stat").read().split(")")[-1].split()[0]
    edit = lambda root, text: open(os.path.join(root, "app/src/main.rs"), "a").write(f"\n// {text}\n")

    check("no daemon before the first build", status()[0] is None)
    _, first = here.cargo("build")
    pid, workers, _ = status()
    check("the first build starts one", first.returncode == 0 and pid is not None, f"pid {pid}")
    wait_warm(here, ws)
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
    client = subprocess.Popen(["cargo", "rustd", "build"], cwd=ws, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
    time.sleep(0.01)
    os.killpg(client.pid, signal.SIGKILL)
    client.wait()
    edit(ws, "four")
    _, after_kill = here.cargo("run")
    check("the daemon outlives a client that is killed", status()[0] == pid and after_kill.returncode == 0 and "hello world" in after_kill.stdout, how_built(after_kill.stderr))

    other = os.path.join(work, "other-build")
    os.makedirs(other, exist_ok=True)
    for name in ("rustd", "cargo-rustd"):
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
    os.kill(pid, signal.SIGKILL)
    time.sleep(1)
    check("workers exit when the daemon is killed", workers and not any(alive(p) for p in workers), f"workers {workers}")
    edit(ws, "six")
    _, again = here.cargo("run")
    check("the build after that starts a daemon again and succeeds", again.returncode == 0 and "hello world" in again.stdout and status()[0] not in (None, pid))
    here.cargo("stop")
    return checks


def table(rows):
    """The table of the report: for every step the seconds of `cargo rustd` and of cargo."""
    steps = ["cold build", "warm-up", "warm build", "cold test", "test warm-up", "warm test", "edit build", "edit test", "broken", "mended"]
    out = ["| Project | " + " | ".join(steps) + " | Differences |", "|---|" + "---:|" * len(steps) + "---|"]
    for row in rows:
        cells = []
        for step in steps:
            ours, theirs = row["times"].get(step, (None, None))
            who = row["built"].get(step, "")
            mark = "" if who in ("daemon", "cargo") or "warm-up" in step else " (c)" if who.startswith("cargo") else ""
            if "warm-up" in step:
                mark = "" if who and all(s == "ready" for s in who.split("; ")) else " (f)"
            cells.append("-" if ours is None else f"{ours:.2f}{mark}" if theirs is None else f"{'**' if who == 'daemon' else ''}{ours:.2f}{'**' if who == 'daemon' else ''}{mark} / {theirs:.2f}")
        out.append(f"| {row['project']} {row['tag']} | " + " | ".join(cells) + f" | {len(row['wrong'])} |")
    return "\n".join(out)


def main():
    argv = sys.argv[1:]
    names = [a for a in argv if not a.startswith("--")] or list(PROJECTS)
    work = tempfile.mkdtemp(prefix="rustd-cargo-")
    print(f"work {work}; rustd {RUSTD}", flush=True)
    result = dict(rustd=RUSTD, toolchain=TOOLCHAIN, projects=[], flags=[], daemon=[], wrong=[])
    if "--only-checks" not in argv:
        for name in names:
            print(f"{name}:", flush=True)
            row = project(name, PROJECTS[name], work)
            result["projects"].append(row)
            result["wrong"] += [f"{name}: {w}" for w in row["wrong"]]
            if "--keep" not in argv:
                shutil.rmtree(os.path.join(work, name), ignore_errors=True)
    if "--only-projects" not in argv:
        print("flags:", flush=True)
        rows, wrong, counted = flags(work)
        result["flags"], result["flags_status"] = rows, counted
        result["wrong"] += wrong
        print("daemon:", flush=True)
        result["daemon"] = daemon(work)
        result["wrong"] += [f"daemon: {name} {detail}" for name, held, detail in result["daemon"] if not held]
    json.dump(result, open(os.path.join(HERE, "cargo_rustd.json"), "w"), indent=1)
    with open(os.path.join(HERE, "CARGO-RUSTD.md"), "w") as md:
        md.write("# cargo rustd against cargo\n\nGenerated by `harness/cargo_rustd.py`. Seconds, `cargo rustd` / plain cargo for the same step. "
                 "Bold is a build the daemon did; `(c)` one cargo did in its place, with the reason below; `(f)` a warm-up that failed. "
                 "Differences counts the steps whose program output, exit status or test results were not cargo's.\n\n")
        md.write(table(result["projects"]) + "\n\n")
        for row in result["projects"]:
            reasons = [f"{step}: {who}" for step, who in row["built"].items() if who.startswith("cargo: ") or "failed" in who]
            if reasons or row["wrong"]:
                md.write(f"**{row['project']}**\n\n" + "".join(f"- {r}\n" for r in reasons + row["wrong"]) + "\n")
        if result["flags"]:
            md.write("## Flags\n\n| Round | Command | Built by | Exit | Against cargo |\n|---|---|---|---:|---|\n")
            md.writelines(f"| {label} | `{shown}` | {who} | {code} | {same} |\n" for label, shown, who, code, same in result["flags"])
        if result["daemon"]:
            md.write("\n## Daemon\n\n" + "".join(f"- {'ok' if held else 'NOT'}: {name} {detail}\n" for name, held, detail in result["daemon"]))
    if "--keep" not in argv:
        shutil.rmtree(work, ignore_errors=True)
    print("PASS" if not result["wrong"] else "FAIL:\n" + "\n".join(result["wrong"]))
    sys.exit(1 if result["wrong"] else 0)


if __name__ == "__main__":
    main()
