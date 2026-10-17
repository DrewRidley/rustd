#!/usr/bin/env python3
"""What cargo built of the crates of a registry is sent to a remote and fetched by another
checkout, which then builds the crates of its workspace only.

A workspace with crates from the registry (a build script, a proc macro, a `-sys` crate) and
a crate at a path is built and pushed. A copy of it somewhere else pulls and builds: cargo
has to find every unit of the registry fresh, compile the local crates, and the program has
to print the same. An archive that is not what its name says is refused.

Needs the crates in cargo's local registry cache: it builds offline."""
import os, shutil, subprocess, sys, tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from difftest import RUNNER, TARGET, profile_dir
RUSTD = os.environ.get("RUSTD_BIN") or os.path.join(HERE, "..", "target", "release", "rustd")
LOCAL = ["app", "near"]

def write(path, text):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    open(path, "w").write(text)

def workspace(root):
    write(f"{root}/Cargo.toml", '[workspace]\nmembers = ["app"]\nresolver = "2"\n')
    write(f"{root}/app/Cargo.toml", '[package]\nname = "app"\nversion = "0.1.0"\nedition = "2021"\n\n[dependencies]\n'
          'serde = { version = "1", features = ["derive"] }\nlibc = "0.2"\nanyhow = "1"\nregex = "1"\nnear = { path = "../near" }\n')
    write(f"{root}/app/src/main.rs", '#[derive(serde::Serialize)]\nstruct S { a: u32 }\nfn main() {\n'
          '    let r = regex::Regex::new("a+").unwrap();\n    let e = anyhow::anyhow!("no");\n'
          '    println!("{} {} {} {}", r.is_match("aa"), e, near::near(), unsafe { libc::getpid() } > 0);\n    let _ = S { a: 1 }.a;\n}\n')
    write(f"{root}/near/Cargo.toml", '[package]\nname = "near"\nversion = "0.1.0"\nedition = "2021"\n')
    write(f"{root}/near/src/lib.rs", 'pub fn near() -> u32 { 7 }\n')

def cargo(root):
    out = subprocess.run(["cargo", "build", "--offline", "-v"], cwd=root, capture_output=True, text=True)
    assert out.returncode == 0, out.stderr
    said = {}
    for line in out.stderr.splitlines():
        words = line.split()
        if len(words) >= 2 and words[0] in ("Fresh", "Compiling", "Dirty"):
            said.setdefault(words[0], set()).add(words[1])
    return said, subprocess.run([*RUNNER, os.path.join(profile_dir(f"{root}/target"), "app")], capture_output=True, text=True).stdout

# The directories cargo builds a profile in, and the list each is sent under. A build for a
# target that is not the host has two: build scripts and proc macros are built for the host.
PROFILES = [("target/debug", "it")] + ([(profile_dir("target"), "it-" + TARGET)] if TARGET else [])

class Said:
    """What `rustd deps` said for each directory of the profile."""
    def __init__(self, outs):
        self.returncode = max(out.returncode for out in outs)
        self.stdout, self.stderr = ("".join(getattr(out, it) for out in outs) for it in ("stdout", "stderr"))
        self.nothing = all(out.stdout.startswith("0 units") for out in outs)
        self.something = not any(" 0 units" in out.stdout for out in outs)

def rustd(root, verb, remote):
    return Said([subprocess.run([RUSTD, "deps", verb, remote, directory, name], cwd=root, capture_output=True, text=True)
                 for directory, name in PROFILES])

right = total = 0
def check(what, ok, detail=""):
    global right, total
    total += 1; right += bool(ok)
    print(f"{what:55} {'right' if ok else 'WRONG'} {detail}", flush=True)

top = tempfile.mkdtemp(prefix="rustd-deps-")
first, second, third, remote = (f"{top}/{it}" for it in ("first", "deeper/second", "third", "remote"))
workspace(first)
_, printed = cargo(first)
shutil.copytree(first, second, ignore=shutil.ignore_patterns("target"))
shutil.copytree(first, third, ignore=shutil.ignore_patterns("target"))

out = rustd(first, "push", remote)
check("the units are pushed", out.returncode == 0 and out.something, out.stdout.strip() + out.stderr.strip())
listed = [line for _, name in PROFILES for line in open(f"{remote}/deps/{name}").read().split("\n")]
check("none of a crate at a path", not any(line.rsplit("-", 1)[0] in LOCAL for line in listed))
out = rustd(first, "push", remote)
check("a second push sends nothing", out.nothing, out.stdout.strip())

out = rustd(second, "pull", remote)
check("the units are pulled", out.returncode == 0, out.stdout.strip() + out.stderr.strip())
said, again = cargo(second)
check("cargo compiles the local crates only", said.get("Compiling") == set(LOCAL) and not said.get("Dirty"), str(said.get("Compiling")))
check("and finds the others fresh", len(said.get("Fresh", ())) >= 10, str(len(said.get("Fresh", ()))))
check("the program prints the same", again == printed and printed, repr(again))
out = rustd(second, "pull", remote)
check("a second pull fetches nothing", out.nothing, out.stdout.strip())

unit = sorted(os.listdir(f"{remote}/units"))[0]
open(f"{remote}/units/{unit}", "ab").write(b"x")
out = rustd(third, "pull", remote)
check("an archive that was changed is refused", out.returncode != 0 and "not what was sent" in out.stderr, out.stderr.strip()[-80:])
said, again = cargo(third)
check("and cargo builds what is missing", again == printed, repr(again))

print(f"{right} of {total} right; in {top}")
sys.exit(right != total)
