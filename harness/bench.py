#!/usr/bin/env python3
"""Edit-loop latency: rustd daemon versus cargo, on the same one-line edits.

usage: bench.py [name ...] [--edits N]

Benchmarks are listed in benches.json. For each one the daemon is started in watch mode, a
literal in a function body is flipped back and forth N times, and the daemon's own timings are
collected. Then the same edits are timed against cargo. Results go to bench.json and BENCH.md.
"""
import json, os, re, shlex, statistics, subprocess, sys, time

# rustd computes layouts with rust-analyzer's copy of rustc_abi, so everything it links against
# has to come from the rustc that copy was taken from.
os.environ.setdefault("RUSTUP_TOOLCHAIN", open(os.path.join(os.path.dirname(os.path.abspath(__file__)), "toolchain")).read().strip())
from difftest import Fixture, RUSTD, sysroot_rlibs

HERE = os.path.dirname(os.path.abspath(__file__))
UNITS = {"ns": 1e-6, "µs": 1e-3, "ms": 1, "s": 1000}


def millis(text):
    value, unit = re.fullmatch(r"([0-9.]+)(ns|µs|ms|s)", text).groups()
    return float(value) * UNITS[unit]


class Edit:
    def __init__(self, path, find, replace):
        self.path, self.find, self.replace = path, find, replace
        self.original = open(path).read()
        assert find in self.original, f"{find!r} not in {path}"
        self.flipped = False

    def flip(self):
        self.flipped = not self.flipped
        text = self.original.replace(self.find, self.replace, 1) if self.flipped else self.original
        open(self.path, "w").write(text)

    def restore(self):
        open(self.path, "w").write(self.original)


def rustd_loop(fx, crate, edit, n):
    obj = os.path.join(fx.work, "rustd.o")
    args = list(fx.link_args)
    first = next(i for i, a in enumerate(args) if a.endswith(".rlib"))
    args[first:first] = [obj, "-Wl,--allow-multiple-definition"]
    args[args.index("-o") + 1] = os.path.join(fx.work, "bin-rustd")
    symbols = [x for rlib in fx.rlibs + sysroot_rlibs() for x in ("--symbols", rlib)]
    log = open(os.path.join(fx.work, "daemon.log"), "w+")
    daemon = subprocess.Popen(
        [RUSTD, fx.ws, "--crate", crate, "--out", obj, "--watch", "--link-cmd", shlex.join(args), *symbols],
        stderr=log, stdout=log)

    def wait_for(pattern, seen):
        deadline = time.time() + 300
        while time.time() < deadline:
            log.seek(0)
            found = re.findall(pattern, log.read())
            if len(found) > seen:
                return found
            if daemon.poll() is not None:
                log.seek(0)
                raise SystemExit(f"daemon died:\n{log.read()[-2000:]}")
            time.sleep(0.002)
        raise SystemExit("daemon timed out")

    try:
        wait_for(r"watching \d+ files", 0)
        log.seek(0)
        text = log.read()
        cold = {"load_ms": millis(re.search(r"load workspace: (\S+)", text)[1]),
                "codegen_ms": millis(re.search(r"cold: compiled .* in (\S+) \(", text)[1])}
        coverage = re.search(r"cold: compiled (\d+) of (\d+)", text)
        for i in range(n):
            time.sleep(0.05)
            edit.flip()
            wait_for(r"edit-to-binary: \S+", i)
        log.seek(0)
        text = log.read()
        rss = int(re.search(r"VmRSS:\s+(\d+)", open(f"/proc/{daemon.pid}/status").read())[1]) // 1024
    finally:
        daemon.kill()
        edit.restore()
    series = {
        "apply": [millis(m) for m in re.findall(r"edit \(apply (\S+)\)", text)],
        "codegen": [millis(m) for m in re.findall(r"edit \(apply \S+\): compiled .* in (\S+) \(", text)],
        "link": [millis(m) for m in re.findall(r"^link: ([0-9.]+(?:ns|µs|ms|s)) \(", text, re.M)][1:],
        "total": [millis(m) for m in re.findall(r"edit-to-binary: (\S+)", text)],
    }
    return {"cold": cold, "rss_mb": rss, "compiled": int(coverage[1]), "functions": int(coverage[2]),
            **{f"{k}_ms": statistics.median(v) for k, v in series.items()},
            "first_edit_ms": series["total"][0]}


def cargo_loop(fx, edit, n, cargo_args):
    times = []
    try:
        for _ in range(n):
            edit.flip()
            start = time.time()
            subprocess.run(["cargo", *cargo_args], cwd=fx.ws, capture_output=True, check=True)
            times.append((time.time() - start) * 1000)
    finally:
        edit.restore()
    return statistics.median(times)


def main():
    names = [a for a in sys.argv[1:] if not a.startswith("--")]
    n = int(sys.argv[sys.argv.index("--edits") + 1]) if "--edits" in sys.argv else 10
    benches = json.load(open(os.path.join(HERE, "benches.json")))
    out_path = os.path.join(HERE, "bench.json")
    results = json.load(open(out_path)) if os.path.exists(out_path) else {}
    for name, b in benches.items():
        if names and name not in names:
            continue
        ws = os.path.join(HERE, b["workspace"])
        touch = os.path.join(ws, b["touch"]) if "touch" in b else None
        fx = Fixture(ws, [b["crate"]], b["target"], touch=touch)
        fx.rustc_build()
        edit = Edit(os.path.join(ws, b["file"]), b["find"], b["replace"])
        result = rustd_loop(fx, b["crate"], edit, n)
        result["cargo_ms"] = cargo_loop(fx, edit, n, b["cargo"])
        results[name] = result
        print(name, json.dumps(result))
    json.dump(results, open(out_path, "w"), indent=1, sort_keys=True)
    lines = ["# rustd edit-loop bench", "", "Generated by `harness/bench.py`. Medians over the edits; times in ms.", "",
             "| Bench | Fast-tier functions | Apply edit | Re-analysis + codegen | Link | rustd total | cargo | Daemon RSS | Cold load + codegen |",
             "|---|---|---|---|---|---|---|---|---|"]
    for name, r in results.items():
        lines.append(
            f"| {name} | {r['compiled']} / {r['functions']} | {r['apply_ms']:.2f} | {r['codegen_ms']:.1f} | "
            f"{r['link_ms']:.0f} | {r['total_ms']:.0f} | {r['cargo_ms']:.0f} | {r['rss_mb']} MB | "
            f"{r['cold']['load_ms'] + r['cold']['codegen_ms']:.0f} |")
    open(os.path.join(HERE, "BENCH.md"), "w").write("\n".join(lines) + "\n")


if __name__ == "__main__":
    main()
