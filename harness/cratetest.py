#!/usr/bin/env python3
"""Differential test using a crate's own integration tests.

usage: cratetest.py <crate-dir> [--crate <name>] [--features a,b] [--bisect] [--json out.json]

Each `tests/*.rs` target is built by cargo, then relinked with rustd's objects for the crate in
front of rustc's rlib. Both binaries must report the same result for every test.
"""
import argparse, json, os, re, subprocess, sys
from difftest import Fixture, compare, discard


UNIT = "(unit tests)"


class TestFixture(Fixture):
    def __init__(self, crate_dir, crate, test, shared, features=None, lib=None):
        if test == UNIT:
            # The crate itself, built with `cfg(test)`: its `#[cfg(test)]` modules are the tests.
            super().__init__(crate_dir, [crate], ["--lib", "--profile", "test"],
                             touch=lib)
            self.rustd_args = ["--cfg-test"]
        else:
            path = os.path.join(crate_dir, "tests", f"{test}.rs")
            super().__init__(crate_dir, [crate, test.replace("-", "_")], ["--test", test], touch=path)
        if features:
            self.target_args += ["--features", features]
            self.rustd_args += ["--features", features]
        self.run_args = ["--test-threads=1"]
        self.shared = shared

    def rustd_build(self, deny=()):
        # The crate's objects are the same whichever test binary they are linked into.
        # rustd links to what the rlibs already hold, so that only goes for the same rlibs.
        key = tuple(self.rlibs)
        if deny or key not in self.shared:
            super().rustd_build(deny)
            if not deny:
                self.shared[key] = (self.objects, self.reports)
        else:
            objects, self.reports = self.shared[key]
            self.link(objects)

    def normalize(self, stdout):
        stdout = re.sub(rb"' \(\d+\) panicked", b"' panicked", stdout)
        return re.sub(rb"finished in [0-9.]+s", b"finished", stdout)


def differing_tests(fx):
    """Runs every test on its own in both binaries: one that aborts hides the ones after it."""
    listed = subprocess.run([fx.reference, "--list"], capture_output=True, text=True).stdout
    names = [l[:-len(": test")] for l in listed.splitlines() if l.endswith(": test")]
    differing = []
    for name in names:
        fx.run_args = ["--test-threads=1", "--exact", name]
        want, got = fx.behaviour(fx.reference), fx.behaviour(fx.binary)
        if want[:2] != got[:2]:
            tail = got[2].decode(errors="replace").strip().splitlines()[-3:]
            differing.append(name)
            print(f"  DIFFERS {name}: rustc status {want[0]}, rustd status {got[0]}: {' | '.join(tail)[:300]}")
    fx.run_args = ["--test-threads=1"]
    return differing


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("crate_dir")
    ap.add_argument("--crate")
    ap.add_argument("--bisect", action="store_true")
    ap.add_argument("--test", action="append", help="only these test binaries; `unit` is the crate's own")
    ap.add_argument("--features", help="cargo features to turn on, comma separated")
    ap.add_argument("--json")
    a = ap.parse_args()
    crate = a.crate or os.path.basename(os.path.abspath(a.crate_dir)).replace("-", "_")
    tests_dir = os.path.join(a.crate_dir, "tests")
    tests = sorted(f[:-3] for f in os.listdir(tests_dir) if f.endswith(".rs")) if os.path.isdir(tests_dir) else []
    meta = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--format-version", "1", "--no-deps"], cwd=a.crate_dir))
    package = next(p for p in meta["packages"] if p["name"].replace("-", "_") == crate
                   or any(t["name"].replace("-", "_") == crate for t in p["targets"]))
    lib = next((t["src_path"] for t in package["targets"] if "lib" in t["kind"]), None)
    if lib:
        tests.append(UNIT)
    tests = [t for t in tests if not a.test or t in a.test or (t == UNIT and "unit" in a.test)]
    results, shared = [], {}
    for test in tests:
        print(f"--- {test}", flush=True)
        fx = TestFixture(a.crate_dir, crate, test, shared, a.features, lib)
        try:
            result = compare(fx, test, a.bisect)
        except SystemExit as e:
            # One test target that does not build or link must not hide the others.
            # rustd sees the crate with its default features, so only those are tested.
            if "requires the features" in str(e):
                print("left out: needs features that are off by default")
                continue
            # rustc could not build it either: a dev-dependency the published crate lacks, say.
            if str(e).startswith("cargo failed"):
                print(f"left out: rustc does not build it: {str(e)[-300:]}")
                continue
            print(f"ERROR: {str(e)[:1500]}")
            results.append({"match": False, "tests_run": 0, "error": str(e)[:300], "crates": {},
                            "work": fx.work})
            continue
        if not result["match"]:
            result["differing_tests"] = differing_tests(fx)
        expected = fx.behaviour(fx.reference)[1]
        result["tests_run"] = len(re.findall(rb"^test .* \.\.\. ", expected, re.M))
        results.append(result)
    ok = all(r["match"] for r in results)
    total = sum(r["tests_run"] for r in results)
    # A crate rustd compiled nothing of matches rustc by construction, which is no result.
    if ok and not sum(c["compiled"] for r in results for c in r["crates"].values()):
        print(f"UNTESTED: {len(results)} test binaries, {total} tests, no function from rustd")
        sys.exit(2)
    print(f"{'MATCH' if ok else 'MISMATCH'}: {len(results)} test binaries, {total} tests")
    if a.json:
        merged = {"fixture": crate, "match": ok, "tests_run": total,
                  "crates": results[0]["crates"] if results else {},
                  "binaries": results,
                  "culprits": [c for r in results for c in r.get("culprits", [])]}
        json.dump(merged, open(a.json, "w"), indent=1)
    discard(results)
    sys.exit(0 if ok else 1)


main()
