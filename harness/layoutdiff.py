"""Compares the size and alignment rust-analyzer computes for types against rustc's.

rustc's side is `-Zprint-type-sizes`; rustd's is the `RUSTD_LAYOUTS` log of every type its
backend laid out. The two print types differently, so both are reduced to a common spelling
and only types that reduce to one unambiguous rustc layout are compared. A mismatch here is a
miscompile found without running anything.
"""
import os, re, subprocess, time

DEFAULTS = (", Global", ", RandomState", ", BuildHasherDefault<DefaultHasher>")


def spell(name):
    name = re.sub(r"\b(?:[A-Za-z_]\w*::)+", "", name)
    name = re.sub(r"'(?:\{region error\}|static|_|[a-z]\w*) ?", "", name)
    name = re.sub(r"ValueConst \{ ty: \w+, value: ValTreeInterned\(Leaf\(0x([0-9a-f]+)\)\) \}",
                  lambda m: str(int(m[1], 16)), name)
    for default in DEFAULTS:
        name = name.replace(default, "")
    return name.replace(" ", "")


def rustc_layouts(workspace, crate):
    # Crate names have underscores where the package may have hyphens.
    for package in (crate, crate.replace("_", "-")):
        # cargo runs rustc again only for arguments it has not seen, and a crate it finds fresh
        # prints nothing.
        again = f"layouts_{time.time_ns()}"
        out = subprocess.run(["cargo", "rustc", "-p", package, "--lib", "--", "-Zprint-type-sizes",
                              "--cfg", again],
                             cwd=workspace, capture_output=True, text=True,
                             # The one unstable flag the harness uses, on the stable pin.
                             env={**os.environ, "RUSTC_BOOTSTRAP": "1", "CARGO_INCREMENTAL": "0"})
        if "did not match any packages" not in out.stderr:
            break
    if out.returncode != 0:
        raise SystemExit(f"cargo failed:\n{out.stderr[-3000:]}")
    layouts = {}
    for m in re.finditer(r"print-type-size type: `(.*)`: (\d+) bytes, alignment: (\d+) bytes", out.stdout):
        layouts.setdefault(spell(m[1]), set()).add((int(m[2]), int(m[3])))
    return layouts


def rustd_layouts(path):
    layouts = {}
    for line in open(path):
        name, size, align = line.rstrip("\n").rsplit("\t", 2)
        if any(s in name for s in ("Binder", "Closure(", "Re", "Param", "Alias", "{", "?")):
            continue
        layouts.setdefault(spell(name), set()).add((int(size), int(align)))
    return layouts


def diff(workspace, crates, log):
    theirs = {}
    for crate in crates:
        for name, sizes in rustc_layouts(workspace, crate).items():
            theirs.setdefault(name, set()).update(sizes)
    ours = rustd_layouts(log)
    compared, wrong = 0, []
    for name, sizes in sorted(ours.items()):
        want = theirs.get(name)
        if not want or len(want) != 1 or len(sizes) != 1:
            continue
        compared += 1
        if sizes != want:
            wrong.append((name, next(iter(want)), next(iter(sizes))))
    return compared, wrong
