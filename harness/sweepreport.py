#!/usr/bin/env python3
"""Summary of a crate sweep: sweepreport.py <dir of cratetest --json files>.

One line per crate (match, tests, share of functions rustd compiled), then the reasons functions
were left to rustc, most common first, over all crates.
"""
import collections, glob, json, os, sys

reasons, where = collections.Counter(), collections.defaultdict(set)
compiled = roots = tests = 0
for path in sorted(glob.glob(os.path.join(sys.argv[1], "*.json"))):
    crate = json.load(open(path))
    done = total = 0
    seen = set()
    for binary in crate.get("binaries", []):
        for name, stats in binary["crates"].items():
            # A crate's objects are shared by its test binaries: count them once.
            if (name, stats["roots"], stats["compiled"]) in seen:
                continue
            seen.add((name, stats["roots"], stats["compiled"]))
            done, total = done + stats["compiled"], total + stats["roots"]
            for reason, n in stats["skipped"].items():
                reasons[reason] += n
                where[reason].add(crate["fixture"])
    differing = [t for b in crate.get("binaries", []) for t in b.get("differing_tests", [])]
    errors = [b["error"].splitlines()[0][:80] for b in crate.get("binaries", []) if "error" in b]
    print(f"{'ok  ' if crate['match'] else 'FAIL'} {crate['fixture']:24} {crate['tests_run']:5} tests  "
          f"{done:5}/{total:<5} functions  {' '.join(differing[:6])} {' '.join(errors[:2])}")
    compiled, roots, tests = compiled + done, roots + total, tests + crate["tests_run"]
print(f"\n{tests} tests, {compiled}/{roots} functions from rustd\n")
for reason, n in reasons.most_common(40):
    print(f"{n:6}  {reason}  ({', '.join(sorted(where[reason])[:4])})")
