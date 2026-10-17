#!/usr/bin/env python3
"""Runs rustc's own run-pass tests through the differential harness.

usage: uitest.py <rust checkout> [--filter <substring>] [--limit N] [--jobs N] [--json out.json]

Each selected `tests/ui` program becomes a package whose library is the test and whose binary
calls its `main`. rustc builds it, rustd rebuilds the library, and the two binaries have to
behave the same. Failing tests keep their package under --work for a closer look with difftest.py. The summary lists mismatches first, then why functions were skipped, most
common reason first: that list is the worklist.
"""
import argparse, collections, concurrent.futures, glob, json, os, re, shlex, shutil, subprocess, sys, time

import difftest
import linkline

# Tests needing more than a plain `rustc file.rs && ./file` are left out.
DIRECTIVE = re.compile(
    r"^//@(?! *(run-pass|needs-unwind|needs-threads|check-run-results|reference|edition|only-linux|only-x86_64|only-unix"
    r"|only-64bit|ignore-backends|ignore-(?!linux|x86_64|unix|64bit|test|pass)[\w-]+)\b)", re.M)
EDITION = re.compile(r"^//@ *edition: *(\d+)", re.M)
UNSUITABLE = re.compile(
    r"^\s*(pub )?mod \w+;|extern crate|include(_str|_bytes)?!|env::args|process::|#!\[no_std\]|#!\[no_main\]"
    r"|#!\[crate_type|proc_macro|#\[path", re.M)
MAIN = re.compile(r"^(pub )?fn main\(\)", re.M)


def select(checkout, needle):
    for path in sorted(glob.glob(f"{checkout}/tests/ui/**/*.rs", recursive=True)):
        if needle not in path:
            continue
        text = open(path, errors="replace").read()
        if re.search(r"^//@ *run-pass", text, re.M) and not DIRECTIVE.search(text) \
                and not UNSUITABLE.search(text) and MAIN.search(text):
            yield path


CHUNK = 150
STALL = 20


def run_chunk(index, paths, root, base):
    """Builds a workspace of tests with one cargo run and one rustd run, then links, runs and
    compares each. Loading std into rustd is most of what a test costs, so it is shared."""
    ws = f"{base}/ws{index}"
    shutil.rmtree(ws, ignore_errors=True)
    names = {f"t{index}_{i}": path for i, path in enumerate(paths)}
    for name, path in names.items():
        os.makedirs(f"{ws}/crates/{name}/src")
        text = open(path).read()
        edition = EDITION.search(text)
        edition = edition.group(1) if edition else "2015"
        open(f"{ws}/crates/{name}/Cargo.toml", "w").write(
            f'[package]\nname = "{name}"\nversion = "0.0.0"\nedition = "{edition}"\n')
        open(f"{ws}/crates/{name}/src/lib.rs", "w").write(MAIN.sub("pub fn main()", text, count=1))
        open(f"{ws}/crates/{name}/src/main.rs", "w").write(f"fn main() {{\n    {name}::main()\n}}\n")
    open(f"{ws}/Cargo.toml", "w").write(
        '[workspace]\nresolver = "2"\nmembers = ["crates/*"]\n[profile.dev]\ndebug = false\n')
    out = subprocess.run(
        ["cargo", "build", "--workspace", "--keep-going"], cwd=ws, capture_output=True, text=True,
        env={**os.environ, "RUSTFLAGS": "--print link-args -C save-temps -C link-dead-code --cap-lints allow"})
    links = {}
    for line in (out.stdout + out.stderr).splitlines():
        if '"cc"' not in line:
            continue
        args = shlex.split(line)
        args = [a for a in args[args.index("cc"):] if not a.endswith("symbols.o")]
        binary = args[args.index("-o") + 1]
        links[os.path.basename(binary).rsplit("-", 1)[0]] = args
    results = {name: {"test": os.path.relpath(path, root), "outcome": "unbuildable", "skipped": {}, "reasons": {},
                      "compiled": 0, "roots": 0} for name, path in names.items()}
    built = sorted(links)
    if not built:
        return list(results.values())
    work = f"{ws}/rustd"
    os.makedirs(work)
    rlib = {name: next(a for a in links[name] if os.path.basename(a).startswith(f"lib{name}-")) for name in built}
    # One program can make rustd hang. The batch is watched for progress, and when it stalls the
    # program it was on is recorded and the rest are started again without it.
    pending, hung, failure = list(built), set(), None
    while pending:
        crates = [x for name in pending for x in ("--crate", name)]
        symbols = [x for file in [rlib[n] for n in pending] + difftest.sysroot_rlibs() for x in ("--symbols", file)]
        rustd = subprocess.Popen([difftest.RUSTD, ws, *crates, "--out", work, "--report", work, *symbols],
                                 stdout=subprocess.DEVNULL, stderr=open(f"{work}/stderr.txt", "w"))
        done = lambda: [n for n in pending if os.path.exists(f"{work}/{n}.tsv") or os.path.exists(f"{work}/{n}.error")]
        seen, since = 0, time.time()
        while rustd.poll() is None:
            time.sleep(0.5)
            if len(done()) != seen:
                seen, since = len(done()), time.time()
            elif time.time() - since > (STALL if seen else 300):
                rustd.kill()
                rustd.wait()
                break
        finished = set(done())
        rest = [n for n in pending if n not in finished]
        if rustd.returncode == 0 or not rest:
            break
        if rustd.returncode < 0:
            hung.add(rest[0])
        else:
            failure = open(f"{work}/stderr.txt").read()[-300:]
            open(f"{work}/{rest[0]}.error", "w").write(f"rustd exited: {failure}")
        pending = rest[1:]

    def finish(name):
        result, args = results[name], list(links[name])
        result["outcome"] = "match"
        if name in hung:
            result.update(outcome="hang", detail="rustd made no progress")
            return
        report, obj = f"{work}/{name}.tsv", f"{work}/{name}.o"
        if os.path.exists(f"{work}/{name}.error") or not os.path.exists(report):
            detail = open(f"{work}/{name}.error").read() if os.path.exists(f"{work}/{name}.error") \
                else f"batch ended early: {failure}"
            result.update(outcome="crash", detail=detail.strip()[:200])
            return
        rows = [l.rstrip("\n").split("\t") for l in open(report)]
        for row in rows:
            if row[0] == "skipped" and "no rustc symbol for function" not in row[2]:
                key = difftest.bucket(row[2])
                result["skipped"][key] = result["skipped"].get(key, 0) + 1
                full = re.sub(r"Id\(\w+\)", "Id", row[2])[:160]
                result["reasons"][full] = result["reasons"].get(full, 0) + 1
        result["compiled"] = sum(r[0] == "compiled" for r in rows)
        result["roots"] = result["compiled"] + sum(result["skipped"].values())
        reference = args[args.index("-o") + 1]
        args = linkline.ahead(args, [obj], difftest.RUSTD)
        binary = f"{work}/{name}"
        args[args.index("-o") + 1] = binary
        link = subprocess.run(args, capture_output=True, text=True)
        if link.returncode != 0:
            result.update(outcome="link", detail=" ".join(link.stderr.strip().splitlines()[-4:])[-300:])
            return

        def behaviour(binary):
            try:
                run = subprocess.run([binary], capture_output=True, timeout=10, cwd=work)
                # The thread id in a panic message differs from run to run.
                return run.returncode, run.stdout, re.sub(rb"' \(\d+\) panicked", b"' panicked", run.stderr)
            except subprocess.TimeoutExpired:
                return "timeout", b"", b""
        expected, got = behaviour(reference), behaviour(binary)
        # A test that fails as rustc built it does not survive being wrapped in a package, and one
        # that prints something else each time (an address, say) has no behaviour to compare.
        if expected[0] != 0 or behaviour(reference) != expected:
            result["outcome"] = "unbuildable"
        elif got == expected:
            pass
        # A rare difference between runs (the order of a hash map) takes a few more to show.
        elif any(behaviour(reference) != expected for _ in range(6)):
            result["outcome"] = "unbuildable"
        else:
            result.update(outcome="mismatch", detail=difftest.symptom(expected, got),
                          stderr=got[2].decode(errors="replace")[-300:], package=f"{ws}/crates/{name}")

    with concurrent.futures.ThreadPoolExecutor(4) as pool:
        list(pool.map(finish, built))
    for result in results.values():
        if result["outcome"] not in ("match", "unbuildable"):
            print(f"{result['outcome']} {result['test']}: {result.get('detail', '')}", file=sys.stderr, flush=True)
    return list(results.values())


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("checkout")
    ap.add_argument("--filter", default="")
    ap.add_argument("--limit", type=int)
    ap.add_argument("--jobs", type=int, default=3, help="workspaces at a time")
    ap.add_argument("--rerun", help="only the tests that failed in this earlier --json output")
    ap.add_argument("--work", default=os.path.expanduser("~/rustd-ui"), help="where the workspaces go")
    ap.add_argument("--json")
    a = ap.parse_args()

    root = f"{os.path.abspath(a.checkout)}/tests/ui"
    tests = list(select(os.path.abspath(a.checkout), a.filter))
    if a.rerun:
        again = {r["test"] for r in json.load(open(a.rerun)) if r["outcome"] not in ("match", "unbuildable")}
        tests = [t for t in tests if os.path.relpath(t, root) in again]
    tests = tests[:a.limit]
    os.makedirs(a.work, exist_ok=True)
    chunks = [tests[i:i + CHUNK] for i in range(0, len(tests), CHUNK)]
    with concurrent.futures.ThreadPoolExecutor(a.jobs) as pool:
        results = [r for chunk in pool.map(lambda c: run_chunk(c[0], c[1], root, a.work), enumerate(chunks))
                   for r in chunk]

    outcomes = collections.Counter(r["outcome"] for r in results)
    ran = [r for r in results if r["outcome"] != "unbuildable"]
    full = sum(r["outcome"] == "match" and not r["skipped"] for r in ran)
    print(f"{len(ran)} tests: {outcomes['match']} match ({full} with every function from rustd), "
          f"{outcomes['mismatch']} mismatch, {outcomes['crash']} rustd crashes, {outcomes['hang']} rustd hangs, {outcomes['link']} link errors"
          f"; {outcomes['unbuildable']} left out")
    print(f"functions: {sum(r['compiled'] for r in ran)} of {sum(r['roots'] for r in ran)} from rustd")
    for outcome in ("mismatch", "crash", "hang", "link"):
        for r in ran:
            if r["outcome"] == outcome:
                print(f"  {outcome.upper()} {r['test']}: {r.get('detail', '')}")
    reasons, example = collections.Counter(), {}
    for r in ran:
        for reason, n in r["skipped"].items():
            reasons[reason] += n
            example.setdefault(reason, r["test"])
    print("skipped functions by reason:")
    for reason, n in reasons.most_common(40):
        print(f"  {n:4}  {reason}  (e.g. {example[reason]})")
    if a.json:
        json.dump(results, open(a.json, "w"), indent=1)
    sys.exit(1 if outcomes["mismatch"] or outcomes["crash"] or outcomes["hang"] or outcomes["link"] else 0)


if __name__ == "__main__":
    main()
