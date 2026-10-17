#!/usr/bin/env python3
"""buildmodel.py: which parts of cargo's build model rustd honours.

usage: buildmodel.py [case ...] [--backend cranelift|llvm] [--table FILE] [--keep]

Each directory under `harness/buildmodel/` is a small cargo project that depends on one feature
of the build model, with a `case.json` that says how cargo is asked to build it and which
crates are rustd's. The project is built twice: by cargo and rustc alone, which is the
reference, and by cargo with rustd's code for the named crates linked in front of rustc's.
Both programs run and what they print and return is compared. A case may then change one of
its inputs a step at a time under one daemon, and each step is compared with cargo the same way.

A row is one of
  supported  the same output, every function of the named crates compiled by rustd
  refused    rustd said why it does not build this, and built nothing that runs differently
  WRONG      different output, a crash, or a refusal without a reason
and the run fails on any WRONG row and on any row that is not what its `case.json` expects.

`case.json`:
  row       what the case is of, as the table names it
  package   the package cargo is asked for (`-p`)
  select    what of the package is built when not its one program: ["--example", "demo"]
  verb      "build" (the default), "test" or "bench": what cargo is asked to do with it
  artifact  the name of the target that is run, when it is not the selected one or the package
  archive   the build is an archive and has no link line: rustd's objects are given to `run`
  unbuilt   why nothing of rustd can be asked to build this at all: the row is that refusal
  crates    the crates rustd compiles
  cargo     more arguments to cargo: ["--release"], ["--features", "x"]
  rustd     more arguments to rustd
  target    the triple to build for when it is not the host
  env       the environment of both builds
  run       a script of the case that runs what was built in place of running it directly;
            it is given ARTIFACT, and RUSTD_OBJECTS with rustd's objects for the second build
  args      what the program is run with
  expect    "supported" (the default) or "refused"
  fails     cargo itself turns the project down, and so must rustd
  steps     [{"what": .., "edit": [[file, old, new]], "write": {file: text}, "env": {..},
              "expect": "same" | "refused"}]
"""
from concurrent.futures import ThreadPoolExecutor
import argparse, glob, json, os, re, shlex, shutil, subprocess, sys, tempfile, time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
os.environ.pop("RUSTD_TARGET", None)
import linkline
from difftest import RUSTD, host, linker_line

# A case names its own flags: difftest's default would replace the ones of its configuration.
os.environ.pop("RUSTFLAGS", None)

CASES = os.path.join(HERE, "buildmodel")
# rustd needs the symbols of what only one codegen unit uses: see difftest.py.
KEEP = ["-C", "link-dead-code"]
CROSS = {
    "aarch64-unknown-linux-gnu": {"linker": "aarch64-linux-gnu-gcc", "runner": ["qemu-aarch64", "-L", "/usr/aarch64-linux-gnu"]},
    "x86_64-unknown-linux-musl": {"linker": "musl-gcc", "runner": []},
}


class Refused(Exception):
    """rustd turned the build down and said why."""


class Wrong(Exception):
    """rustd's build is not cargo's, or it failed without a reason."""


def sysroot_rlibs(triple, env):
    sysroot = subprocess.check_output(["rustc", "--print", "sysroot"], text=True, env=env).strip()
    return [p for n in ("core", "alloc", "std") for p in glob.glob(f"{sysroot}/lib/rustlib/{triple}/lib/lib{n}-*.rlib")]


def reason_of(stderr):
    """What rustd gave as the reason it stopped: its last line, unless it crashed."""
    lines = [l for l in stderr.strip().splitlines() if l.strip()]
    if not lines:
        raise Wrong("rustd failed and said nothing")
    if any("panicked at" in l for l in lines) or "stack backtrace" in stderr:
        raise Wrong("rustd crashed: " + next(l for l in lines if "panicked at" in l or "backtrace" in l)[:200])
    return re.sub(r"^Error: ", "", lines[-1])[:300]


class Case:
    def __init__(self, name, backend, keep):
        self.name, self.backend, self.keep = name, backend, keep
        self.source = os.path.join(CASES, name)
        self.spec = json.load(open(os.path.join(self.source, "case.json")))
        self.target = self.spec.get("target")
        cross = CROSS.get(self.target, {})
        self.runner = cross.get("runner", [])
        self.env = {**os.environ, **self.spec.get("env", {})}
        if "linker" in cross:
            self.env["CARGO_TARGET_" + self.target.upper().replace("-", "_") + "_LINKER"] = cross["linker"]
        if backend == "llvm":
            self.env.update(RUSTD_BACKEND="llvm", RUSTD_OPT="2", RUSTD_EMIT="fast")
        else:
            self.env["RUSTD_BACKEND"] = "cranelift"
        self.select = ["-p", self.spec["package"], *self.spec.get("select", [])]
        self.cargo_args = [*self.spec.get("cargo", []), *(["--target", self.target] if self.target else [])]
        self.rustd_args = ["--package", self.spec["package"], *self.spec.get("rustd", []), *(["--target", self.target] if self.target else [])]
        self.crates = self.spec["crates"]
        self.verb = self.spec.get("verb", "build")

    def prepare(self):
        self.ws = tempfile.mkdtemp(prefix=f"rustd-bm-{self.name}-")
        shutil.copytree(self.source, self.ws, dirs_exist_ok=True, ignore=shutil.ignore_patterns("target*", "case.json"))
        self.work = os.path.join(self.ws, "target-rustd-out")
        os.makedirs(self.work)
        if os.path.exists(os.path.join(self.ws, "setup.sh")):
            setup = subprocess.run(["sh", "setup.sh"], cwd=self.ws, capture_output=True, text=True, env=self.env)
            if setup.returncode != 0:
                raise Wrong("the case's setup.sh failed: " + (setup.stderr + setup.stdout)[-400:])

    # The reference.

    def cargo(self, env, target_dir="target-ref"):
        """Builds with cargo alone: the artifact, or None when cargo turns the project down."""
        verb = {"build": ["build"], "test": ["test", "--no-run"], "bench": ["bench", "--no-run"]}[self.verb]
        run = subprocess.run(["cargo", *verb, *self.select, *self.cargo_args, "--message-format=json"], cwd=self.ws,
                             capture_output=True, text=True, env={**env, "CARGO_TARGET_DIR": os.path.join(self.ws, target_dir)})
        if run.returncode != 0:
            self.cargo_error = run.stderr[-1500:]
            return None
        made = [json.loads(l) for l in run.stdout.splitlines() if l.startswith("{")]
        made = [m for m in made if m.get("reason") == "compiler-artifact" and m["target"]["name"] == self.artifact_name()]
        last = made[-1]
        return last.get("executable") or last["filenames"][0]

    def sysroot(self, env):
        return subprocess.check_output(["rustc", "--print", "sysroot"], text=True, env=env, cwd=self.ws).strip()

    def artifact_name(self):
        return self.spec.get("artifact") or (self.select[3] if len(self.select) > 3 else self.spec["package"])

    def behaviour(self, artifact, env, objects=()):
        """What running the build prints and returns."""
        if "run" in self.spec:
            command = ["sh", self.spec["run"]]
            env = {**env, "ARTIFACT": artifact, "RUSTD_OBJECTS": " ".join(objects)}
        else:
            command = [*self.runner, artifact, *self.spec.get("args", [])]
        # A program linked to a `dylib` finds it where cargo left it, as `cargo run` has it.
        mine, other = ("target-ref", "target") if "/target-ref/" in artifact else ("target", "target-ref")
        libraries = [d for t in (mine, other) for d in glob.glob(os.path.join(self.ws, t, "**", "deps"), recursive=True)]
        libraries += glob.glob(os.path.join(self.sysroot(env), "lib", "rustlib", "*", "lib"))
        env = {**env, "LD_LIBRARY_PATH": ":".join(libraries)}
        try:
            run = subprocess.run(command, cwd=self.ws, capture_output=True, timeout=120, env=env)
        except subprocess.TimeoutExpired:
            return ("timeout", b"", b"")
        clean = lambda out: re.sub(rb"finished in [0-9.]+s", b"finished", re.sub(rb"' \(\d+\) panicked", b"' panicked", out))
        return (run.returncode, clean(run.stdout), clean(run.stderr))

    # rustd's build.

    def link_line(self, env):
        """cargo's build of what rustd links against, and the line that links it."""
        env = dict(env)
        # A build of the whole workspace has no one rustc to hand arguments: every one prints.
        whole = "--workspace" in self.cargo_args
        ask = ["--print", "link-args", "-C", "save-temps"]
        flags = KEEP + (ask if whole else [])
        if "RUSTFLAGS" in env:
            env["RUSTFLAGS"] += " " + " ".join(flags)
            config = []
        else:
            # Joined with what the project's own configuration has, which RUSTFLAGS would replace.
            config = ["--config", "build.rustflags=" + json.dumps(flags)]
        profile = ["--profile", self.verb] if self.verb != "build" else []
        command = ["cargo", "build", *config, *self.cargo_args] if whole else \
            ["cargo", "rustc", *config, *self.select, *profile, *self.cargo_args, "--", *ask]
        run = subprocess.run(command, cwd=self.ws, capture_output=True, text=True, env=env)
        if run.returncode != 0:
            raise Wrong("cargo failed with what rustd needs of it: " + run.stderr[-600:])
        printed = run.stdout + run.stderr
        if whole:
            printed = "\n".join(l for l in printed.splitlines() if f"/{self.artifact_name()}-" in l)
        try:
            line = [a for a in linker_line(printed) if not a.endswith("symbols.o")]
        except SystemExit:
            return None
        return line

    def archive(self, env):
        """cargo's archive of the package with every symbol kept, in place of a link line."""
        built = self.cargo({**env, "RUSTFLAGS": " ".join([env.get("RUSTFLAGS", ""), *KEEP]).strip()}, "target")
        if built is None:
            raise Wrong("cargo failed with what rustd needs of it: " + self.cargo_error[-600:])
        return [built]

    def link_dir(self):
        """Where cargo runs the linker: the root of the workspace."""
        return self.ws

    def symbols(self, link, env):
        rlibs = [a for a in link if a.endswith((".rlib", ".rcgu.o", ".a"))]
        return [x for rlib in rlibs + sysroot_rlibs(self.target or host(), env) for x in ("--symbols", rlib)]

    def left_to_rustc(self, report):
        rows = [l.rstrip("\n").split("\t") for l in open(report)]
        compiled = sum(r[0] == "compiled" for r in rows)
        skipped = [r for r in rows if r[0] == "skipped" and "no rustc symbol for function" not in r[2]]
        return compiled, skipped

    def batch(self, env):
        """Builds once with rustd, not as a daemon: the artifact and how many functions are rustd's."""
        link = self.link = self.archive(env) if self.spec.get("archive") else self.link_line(env)
        if link is None:
            raise Wrong("cargo printed no link line for rustd's objects to go on")
        obj, report = os.path.join(self.work, "rustd.o"), os.path.join(self.work, "report.tsv")
        run = subprocess.run(
            [RUSTD, self.ws, *(x for c in self.crates for x in ("--crate", c)), "--out", obj, "--report", report,
             *self.rustd_args, *self.symbols(link, env)], capture_output=True, text=True, env=env, timeout=600)
        open(os.path.join(self.work, "rustd.stderr"), "w").write(run.stderr)
        if run.returncode != 0:
            raise Refused(reason_of(run.stderr))
        compiled, skipped = self.left_to_rustc(report)
        if skipped:
            raise Refused(f"{len(skipped)} functions left to rustc: {skipped[0][1]}: {skipped[0][2][:160]}")
        if not compiled:
            raise Wrong("rustd compiled nothing, so nothing was compared")
        if self.spec.get("archive"):
            return link[0], [obj], compiled
        out = os.path.join(self.work, "out")
        os.makedirs(out, exist_ok=True)
        binary = os.path.join(out, os.path.basename(linkline.output_of(link)))
        linked = subprocess.run(linkline.with_output(linkline.ahead(link, [obj], RUSTD), binary), cwd=self.link_dir(), capture_output=True, text=True, env=env)
        if linked.returncode != 0:
            raise Wrong("the link with rustd's objects failed: " + (linked.stderr + linked.stdout)[-400:])
        return binary, [], compiled

    # One input changed at a time under one daemon.

    def follow(self, env):
        """Runs the steps of the case against one daemon: (what, verdict, detail) of each."""
        link = self.link
        obj, patch, binary = (os.path.join(self.work, f"daemon.{n}") for n in ("o", "patch.o", "bin"))
        socket, log_path = os.path.join(self.work, "rustd.sock"), os.path.join(self.work, "daemon.log")
        line = linkline.join(linkline.with_output(linkline.ahead(link, [patch, obj], RUSTD), binary))
        ask = ["--out", obj, "--patch", patch, "--link-cmd", line, "--binary", binary, *self.symbols(link, env)]
        log = open(log_path, "a")
        daemon = subprocess.Popen(
            [RUSTD, self.ws, *(x for c in self.crates for x in ("--crate", c)), *self.rustd_args, "--socket", socket, *ask],
            stderr=log, stdout=log, env=env)
        try:
            while "watching" not in open(log_path).read():
                if daemon.poll() is not None:
                    return [("the daemon starts", "WRONG", open(log_path).read()[-300:])]
                time.sleep(0.05)
            return [self.step(step, env, daemon, socket, binary) for step in self.spec["steps"]]
        finally:
            daemon.kill()

    def step(self, step, env, daemon, socket, binary):
        for path, old, new in step.get("edit", []):
            text = open(os.path.join(self.ws, path)).read()
            assert old in text, (self.name, step["what"], old)
            open(os.path.join(self.ws, path), "w").write(text.replace(old, new, 1))
        for path, text in step.get("write", {}).items():
            open(os.path.join(self.ws, path), "w").write(text)
        env.update(step.get("env", {}))
        time.sleep(0.02)
        reply = subprocess.run([RUSTD, "sync", socket], capture_output=True, text=True, timeout=300, env=env)
        reply = (reply.stdout + reply.stderr).strip()
        artifact = self.cargo(env)
        expect = step.get("expect", "same")
        if daemon.poll() is not None:
            return (step["what"], "WRONG", "the daemon died")
        if artifact is None:
            verdict = ("refused", reply) if not reply.startswith("ok") else ("WRONG", "cargo turns it down, rustd answered ok")
        elif not reply.startswith("ok"):
            verdict = ("refused", reply)
        else:
            want, got = self.behaviour(artifact, env), self.behaviour(binary, env)
            verdict = ("same", "") if want == got else ("WRONG", differ(want, got))
        if verdict[0] != "WRONG" and verdict[0] != expect:
            verdict = ("WRONG", f"expected {expect}, got {verdict[0]}: {verdict[1]}")
        return (step["what"], verdict[0], verdict[1][:300])

    def row(self):
        """The case's line of the table."""
        steps = []
        if "unbuilt" in self.spec:
            return ("refused", self.spec["unbuilt"], steps)
        env = dict(self.env)
        try:
            self.prepare()
            artifact = self.cargo(env)
            if artifact is None and not self.spec.get("fails"):
                return ("WRONG", "the case itself does not build with cargo: " + self.cargo_error[-300:], steps)
            try:
                built, objects, compiled = self.batch(env)
            except Wrong as wrong:
                if artifact is None:
                    return ("refused", "cargo turns it down too", steps)
                raise
            if artifact is None:
                return ("WRONG", "cargo turns the project down, rustd built it", steps)
            want, got = self.behaviour(artifact, env), self.behaviour(built, env, objects)
            if want != got:
                return ("WRONG", differ(want, got), steps)
            if self.spec.get("steps"):
                steps = self.follow(env)
            return ("supported", f"{compiled} functions", steps)
        except Refused as refused:
            return ("refused", str(refused), steps)
        except Wrong as wrong:
            return ("WRONG", str(wrong), steps)
        except Exception as error:
            return ("WRONG", f"the harness failed: {error!r}"[:300], steps)
        finally:
            if not self.keep and hasattr(self, "ws"):
                shutil.rmtree(self.ws, ignore_errors=True)


def differ(want, got):
    if want[0] != got[0]:
        return f"cargo's exits with {want[0]}, rustd's with {got[0]}: {got[2].decode(errors='replace').strip()[-160:]}"
    for stream in (1, 2):
        a, b = (x[stream].decode(errors="replace").splitlines() for x in (want, got))
        for w, g in zip(a, b):
            if w != g:
                return f"cargo's prints `{w[:120]}`, rustd's `{g[:120]}`"
        if len(a) != len(b):
            return f"cargo's prints {len(a)} lines, rustd's {len(b)}"
    return "differs"


def said(why):
    """A reason as the table has it: without the scratch directory, which is another each run."""
    why = re.sub(r"^(Error: )?(daemon: )?(error: )?", "", why)
    return re.sub(r"/tmp/rustd-bm-[^/ ]*/", "", why).replace("|", "/")[:240]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("cases", nargs="*")
    ap.add_argument("--backend", action="append", choices=["cranelift", "llvm"])
    ap.add_argument("--table", help="write the table as Markdown")
    ap.add_argument("--keep", action="store_true", help="keep the scratch copies")
    ap.add_argument("--jobs", type=int, default=max(1, (os.cpu_count() or 2) // 4), help="cases built at once")
    a = ap.parse_args()
    names = a.cases or sorted(n for n in os.listdir(CASES) if os.path.exists(os.path.join(CASES, n, "case.json")))
    backends = a.backend or ["cranelift", "llvm"]
    table, bad = [], 0
    cases = [Case(name, backend, a.keep) for name in names for backend in backends]
    with ThreadPoolExecutor(a.jobs) as pool:
        rows = pool.map(Case.row, cases)
        for case, (verdict, detail, steps) in zip(cases, rows):
            name, backend = case.name, case.backend
            expect = case.spec.get("expect", "supported")
            unexpected = verdict != "WRONG" and verdict != expect
            failed = verdict == "WRONG" or unexpected or any(s[1] == "WRONG" for s in steps)
            bad += failed
            print(f"{name:34} {backend:9} {verdict:9} {'(expected ' + expect + ') ' if unexpected else ''}{detail[:200]}", flush=True)
            for what, how, why in steps:
                print(f"{'':34} {'':9}   {how:8} {what}{': ' + why[:200] if why else ''}", flush=True)
            if a.keep and failed:
                print(f"{'':34} kept {case.ws}")
            table.append((case.spec["row"], name, backend, verdict, detail, steps))
    if a.table:
        write_table(a.table, table)
    wrong = sum(t[3] == "WRONG" or any(s[1] == "WRONG" for s in t[5]) for t in table)
    print(f"{len(table) - bad} of {len(table)} as expected, {wrong} WRONG")
    sys.exit(1 if bad else 0)


def write_table(path, table):
    lines = ["| feature | case | backend | result | detail | invalidation |", "|---|---|---|---|---|---|"]
    by_case = {}
    for row, name, backend, verdict, detail, steps in table:
        by_case.setdefault((row, name), []).append((backend, verdict, detail, steps))
    for (row, name), runs in by_case.items():
        alike = len({(v, tuple((s[0], s[1]) for s in st)) for _, v, _, st in runs}) == 1
        for backend, verdict, detail, steps in (runs[:1] if alike else runs):
            both = "both" if alike and len(runs) > 1 else backend
            changed = "; ".join(f"{what}: {how}" + (f" ({said(why)})" if how != "same" and why else "") for what, how, why in steps)
            detail = detail if verdict != "supported" else ""
            lines.append(f"| {row} | `{name}` | {both} | {verdict} | {said(detail)} | {changed.replace('|', '/')} |")
    open(path, "w").write("\n".join(lines) + "\n")


if __name__ == "__main__":
    main()
