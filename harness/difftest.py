#!/usr/bin/env python3
"""Differential test: build a binary with rustc, rebuild chosen crates with rustd, compare behaviour.

usage: difftest.py <workspace> --bin <package> --crate <name> [--crate <name> ...] [--bisect]

Exit code 0 when stdout, stderr and exit status match. With --bisect, each rustd function is
also linked in on its own to name the ones that change behaviour.
"""
import re
import argparse, glob, json, os, shlex, subprocess, sys, tempfile, time

# rustd computes layouts with rust-analyzer's copy of rustc_abi, so everything it links against
# has to come from the rustc that copy was taken from.
os.environ.setdefault("RUSTUP_TOOLCHAIN", open(os.path.join(os.path.dirname(os.path.abspath(__file__)), "toolchain")).read().strip())

# rustc makes what only one codegen unit uses local to it unless it is told to keep unused code.
# rustd needs the symbols: a private `static mut` has to be the one object for both compilers.
os.environ.setdefault("RUSTFLAGS", "-C link-dead-code")
# Incremental caches of a sweep over many crates fill the disk.
os.environ.setdefault("CARGO_INCREMENTAL", "0")

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
# A sweep runs against a copy, so rebuilding rustd meanwhile does not change it under the sweep.
RUSTD = os.environ.get("RUSTD_BIN") or os.path.join(ROOT, "target/release/rustd")


def weaken_macho(buf, off=0):
    import struct
    if buf[off:off+4] != b"\xcf\xfa\xed\xfe":
        return False
    ncmds = struct.unpack_from("<I", buf, off + 16)[0]
    pos = off + 32
    for _ in range(ncmds):
        cmd, cmdsize = struct.unpack_from("<II", buf, pos)
        if cmd == 2:  # LC_SYMTAB
            symoff, nsyms = struct.unpack_from("<II", buf, pos + 8)
            for i in range(nsyms):
                s = off + symoff + i * 16
                n_type = buf[s + 4]
                if (n_type & 0x0f) == 0x0f:  # N_SECT | N_EXT
                    n_desc = struct.unpack_from("<H", buf, s + 6)[0]
                    struct.pack_into("<H", buf, s + 6, n_desc | 0x0080)  # N_WEAK_DEF
        pos += cmdsize
    return True


def weaken_file(path):
    buf = bytearray(open(path, "rb").read())
    if not weaken_macho(buf, 0) and buf[:8] == b"!<arch>\n":
        pos = 8
        while pos + 60 <= len(buf):
            size = int(bytes(buf[pos+48:pos+58]).strip())
            body = pos + 60
            if bytes(buf[pos:pos+3]) == b"#1/":
                name_len = int(bytes(buf[pos+3:pos+16]).strip())
                weaken_macho(buf, body + name_len)
            else:
                weaken_macho(buf, body)
            pos = body + size + (size & 1)
    open(path, "wb").write(buf)
def sysroot_rlibs():
    sysroot = subprocess.check_output(["rustc", "--print", "sysroot"], text=True).strip()
    host = subprocess.check_output(["rustc", "-vV"], text=True).split("host: ")[1].split()[0]
    lib = f"{sysroot}/lib/rustlib/{host}/lib"
    return [p for n in ("core", "alloc", "std") for p in glob.glob(f"{lib}/lib{n}-*.rlib")]


class Fixture:
    def __init__(self, workspace, crates, target, touch=None):
        """`target` selects what cargo links: ["-p", "app"] or ["--test", "name"]."""
        self.ws, self.crates, self.target_args = os.path.abspath(workspace), crates, target
        self.work = tempfile.mkdtemp(prefix="rustd-diff-")
        self.run_args = []
        self.rustd_args = []
        self.rustd_timeout = None
        self.layout_log = os.path.join(self.work, "layouts.tsv")
        if touch is None:
            meta = json.loads(subprocess.check_output(
                ["cargo", "metadata", "--format-version", "1", "--no-deps"], cwd=self.ws))
            touch = next(t["src_path"] for p in meta["packages"] if p["name"] == target[1]
                         for t in p["targets"] if "bin" in t["kind"])
        self.touch = touch

    def rustc_build(self):
        """Builds with cargo and captures the linker line so rustd objects can be spliced in."""
        os.utime(self.touch)
        out = subprocess.run(
            ["cargo", "rustc", *self.target_args, "--", "--print", "link-args", "-C", "save-temps"],
            cwd=self.ws, capture_output=True, text=True)
        if out.returncode != 0:
            raise SystemExit(f"cargo failed:\n{out.stderr[-3000:]}")
        line = next(l for l in (out.stdout + out.stderr).splitlines() if '"cc"' in l)
        args = shlex.split(line)
        self.link_args = [a for a in args[args.index("cc"):] if not a.endswith("symbols.o")]
        self.reference = self.link_args[self.link_args.index("-o") + 1]
        # The target's own objects too: a crate rustd rebuilds may be the one being linked.
        self.rlibs = [a for a in self.link_args if a.endswith((".rlib", ".rcgu.o"))]
        if sys.platform == "darwin":
            for rlib in self.rlibs:
                weaken_file(rlib)

    def rustd_build(self, deny=()):
        """Compiles each crate with rustd and links the objects ahead of rustc's rlibs."""
        deny_file = os.path.join(self.work, "deny.txt")
        open(deny_file, "w").write("\n".join(deny))
        symbols = [x for rlib in self.rlibs + sysroot_rlibs() for x in ("--symbols", rlib)]
        objects, self.reports = [], {}
        for crate in self.crates:
            obj, report = (os.path.join(self.work, f"{crate}.{ext}") for ext in ("o", "tsv"))
            try:
                run = subprocess.run(
                    [RUSTD, self.ws, *(x for c in crate.split(",") for x in ("--crate", c)), "--out", obj, "--report", report,
                     "--deny", deny_file, "--isolate", os.path.join(self.work, f"{crate}.isolated"),
                     *self.rustd_args, *symbols],
                    capture_output=True, text=True, timeout=self.rustd_timeout,
                    env={**os.environ, "RUSTD_LAYOUTS": self.layout_log})
            except subprocess.TimeoutExpired:
                raise SystemExit(f"rustd crashed on {crate}:\nerror: no result after {self.rustd_timeout}s")
            open(os.path.join(self.work, f"{crate}.stderr"), "w").write(run.stderr)
            if os.environ.get("RUSTD_STDERR"): open(os.environ["RUSTD_STDERR"], "a").write(run.stderr)
            if run.returncode != 0:
                raise SystemExit(f"rustd crashed on {crate}:\n{run.stderr[-2000:]}")
            objects.append(obj)
            self.reports[crate] = [l.rstrip("\n").split("\t") for l in open(report)]
        self.objects = objects
        self.link(objects)

    def link(self, objects):
        args = list(self.link_args)
        first = next(i for i, a in enumerate(args) if a.endswith((".rlib", ".rcgu.o")))
        args[first:first] = objects + ([] if sys.platform == "darwin" else ["-Wl,--allow-multiple-definition"])
        self.binary = os.path.join(self.work, "bin-rustd")
        args[args.index("-o") + 1] = self.binary
        run = subprocess.run(args, capture_output=True, text=True)
        if run.returncode != 0:
            raise SystemExit(f"link failed:\n{run.stderr[-3000:]}")

    def compiled(self):
        return [r[1] for rows in self.reports.values() for r in rows if r[0] == "compiled"]

    def behaviour(self, binary):
        # A loaded machine slows both builds alike, so rustc's own run sets the allowance.
        allowed = 900 if binary == self.reference else max(120, 30 * getattr(self, "reference_secs", 0))
        start = time.monotonic()
        try:
            run = subprocess.run([binary, *self.run_args], capture_output=True, timeout=allowed)
            if binary == self.reference:
                self.reference_secs = time.monotonic() - start
            # The thread id in a panic message differs from run to run.
            stderr = re.sub(rb"' \(\d+\) panicked", b"' panicked", run.stderr)
            # Loggers stamp their lines with the time.
            stderr = re.sub(rb"\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(\.\d+)?Z?", b"<time>", stderr)
            return run.returncode, self.normalize(run.stdout), stderr
        except subprocess.TimeoutExpired:
            return "timeout", b"", b""

    def normalize(self, stdout):
        return stdout

    def matches(self, expected):
        return self.behaviour(self.binary) == expected


def isolate(fx, expected):
    """Links each rustd function into the rustc binary on its own; returns the ones that change behaviour."""
    culprits = []
    for crate in fx.crates:
        directory = os.path.join(fx.work, f"{crate}.isolated")
        for line in open(os.path.join(directory, "index.tsv")):
            index, name = line.rstrip("\n").split("\t")
            fx.link([os.path.join(directory, f"{index}.o")])
            if not fx.matches(expected):
                got = fx.behaviour(fx.binary)
                culprits.append({"function": name, "symptom": symptom(expected, got)})
    return culprits


def minimize(items, fails):
    """Delta debugging: a smaller list for which `fails` still holds."""
    n = 2
    while len(items) >= 2:
        size = max(len(items) // n, 1)
        chunks = [items[i:i + size] for i in range(0, len(items), size)]
        for i, chunk in enumerate(chunks):
            rest = [x for j, c in enumerate(chunks) if j != i for x in c]
            if fails(chunk):
                items, n = chunk, 2
                break
            if len(chunks) > 2 and fails(rest):
                items, n = rest, max(n - 1, 2)
                break
        else:
            if size == 1:
                break
            n = min(n * 2, len(items))
    return items


def symptom(expected, got):
    if got[0] != expected[0]:
        return f"exit status {got[0]}"
    want, have = expected[1].decode(errors="replace").splitlines(), got[1].decode(errors="replace").splitlines()
    for w, h in zip(want, have):
        if w != h:
            return f"expected `{w}`, got `{h}`"
    return "output length differs"


def bucket(error):
    """Failure reason without the payload, so the same gap in different functions counts once."""
    import re
    reason = error.rsplit("`: ", 1)[-1]
    reason = re.sub(r"^(unsupported|frontend|backend): ", "", reason)
    for prefix in ("drop glue", "unsizing", "aggregate of", "expected scalar", "cast "):
        if reason.startswith(prefix):
            return {"aggregate of": "odd-sized aggregate in a register", "cast ": reason.split("(")[-1].rstrip(")") + " cast"}.get(prefix, prefix)
    return re.split(r"[({]", reason)[0].strip()[:60]


def memcheck(fx):
    """Runs the rustd binary under valgrind: the first invalid access is usually the miscompile,
    where the crash or wrong output is only what it led to. Returns the error blocks."""
    import re, shutil
    if not shutil.which("valgrind"):
        return None
    log = os.path.join(fx.work, "valgrind.log")
    try:
        subprocess.run(["valgrind", "-q", "--num-callers=30", f"--log-file={log}", fx.binary, *fx.run_args],
                       capture_output=True, timeout=900)
    except subprocess.TimeoutExpired:
        pass
    text = re.sub(r"^==\d+== ?", "", open(log).read(), flags=re.M)
    errors = [e for e in text.split("\n\n") if e.strip()]
    print(f"  valgrind: {len(errors)} errors ({log})")
    if errors:
        print("\n".join("    " + line[:200] for line in errors[0].splitlines()))
    return errors


def compare(fx, name, do_bisect, do_memcheck=False, do_layouts=False, mixed=0):
    fx.rustc_build()
    expected = fx.behaviour(fx.reference)
    fx.rustd_build()
    ok = fx.matches(expected)
    result = {"fixture": name, "match": ok, "crates": {}}
    for crate, rows in fx.reports.items():
        n = sum(r[0] == "compiled" for r in rows)
        total = sum(r[0] in ("compiled", "skipped") for r in rows)
        reasons = {}
        for r in rows:
            if r[0] == "skipped":
                key = bucket(r[2])
                reasons[key] = reasons.get(key, 0) + 1
        no_symbol = reasons.pop("no rustc symbol for function", 0)
        result["crates"][crate] = {
            "compiled": n, "roots": total - no_symbol, "not_roots": no_symbol,
            "instances": int(rows[0][2]), "codegen_ms": int(rows[0][4]) / 1000, "skipped": reasons}
        print(f"{crate}: {n}/{total - no_symbol} functions from rustd")
        for r in rows:
            if r[0] == "skipped" and bucket(r[2]) != "no rustc symbol for function":
                why = r[2] if len(r[2]) < 400 else r[2][:100] + " ... " + r[2][-300:]
                print(f"  skipped {r[1]}: {why}")
    print("MATCH" if ok else "MISMATCH")
    if not ok:
        got = fx.behaviour(fx.binary)
        import difflib
        want, have = (x[1].decode(errors='replace').splitlines() for x in (expected, got))
        if len(want) + len(have) > 40:
            # Long output: only where the two part ways.
            diff = list(difflib.unified_diff(want, have, "rustc", "rustd", lineterm="", n=1))
            print(f"  rustc: status {expected[0]}, rustd: status {got[0]}")
            print("\n".join("  " + l[:300] for l in diff[:60]))
        else:
            print(f"  rustc: status {expected[0]}\n{expected[1].decode(errors='replace')}")
            print(f"  rustd: status {got[0]}\n{got[1].decode(errors='replace')}")
        stderr = got[2].decode(errors='replace')
        print(stderr if len(stderr) < 1500 else stderr[:700] + "\n  ...\n" + stderr[-700:])
        do_memcheck = True
        if do_bisect:
            result["culprits"] = isolate(fx, expected)
            for c in result["culprits"]:
                print(f"  MISCOMPILED {c['function']}: {c['symptom']}")
    if ok and mixed:
        # The boundary between rustc's code and ours: leave a random half of the functions to
        # rustc, so that values made on one side (closures, futures, vtables) are used on the other.
        import random
        compiled = sorted(r[1] for rows in fx.reports.values() for r in rows if r[0] == "compiled")
        for seed in range(mixed):
            deny = random.Random(seed).sample(compiled, len(compiled) // 2)
            fx.rustd_build(deny)
            if not fx.matches(expected):
                got = fx.behaviour(fx.binary)
                print(f"  MIXED MISMATCH (seed {seed}): {symptom(expected, got)}")
                deny = minimize(deny, lambda d: fx.rustd_build(d) or not fx.matches(expected))
                fx.rustd_build(deny)
                print("    smallest set left to rustc that still differs:\n" + "\n".join("      " + d for d in deny))
                result["mixed_deny"] = deny
                result["match"] = False
                result["mixed_seed"] = seed
                do_memcheck = True
                break
        else:
            print(f"  mixed: {mixed} random splits match")
    if do_memcheck:
        errors = memcheck(fx)
        result["valgrind_errors"] = None if errors is None else len(errors)
        if errors:
            result["match"] = False
    if do_layouts:
        import layoutdiff
        compared, wrong = layoutdiff.diff(fx.ws, fx.crates, fx.layout_log)
        print(f"  layouts: {len(wrong)} of {compared} differ from rustc")
        for name, want, have in wrong:
            print(f"    {name}: rustc (size, align) {want}, rust-analyzer {have}")
        result["layouts"] = {"compared": compared, "wrong": [w[0] for w in wrong]}
        if wrong:
            result["match"] = False
    result["work"] = fx.work
    return result


def discard(results):
    """Scratch directories are only worth keeping for what did not match."""
    import shutil
    for r in results:
        if r.get("match"):
            shutil.rmtree(r["work"], ignore_errors=True)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("workspace")
    ap.add_argument("--bin", required=True)
    ap.add_argument("--crate", action="append", required=True)
    ap.add_argument("--bisect", action="store_true")
    ap.add_argument("--memcheck", action="store_true", help="also fail on valgrind errors when the output matches")
    ap.add_argument("--layouts", action="store_true", help="compare type sizes and alignments with rustc's")
    ap.add_argument("--mixed", type=int, default=0, help="also run this many builds with a random half of the functions left to rustc")
    ap.add_argument("--json")
    a = ap.parse_args()

    fx = Fixture(a.workspace, a.crate, ["-p", a.bin])
    result = compare(fx, a.bin, a.bisect, a.memcheck, a.layouts, a.mixed)
    if a.json:
        json.dump(result, open(a.json, "w"), indent=1)
    discard([result])
    sys.exit(0 if result["match"] else 1)


if __name__ == "__main__":
    main()
