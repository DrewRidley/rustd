#!/usr/bin/env python3
"""heap.py FOLDED [ROWS]: attributes a heap profile to salsa queries and to rustd's own structures.

FOLDED is what `jeprof --collapsed` makes of a profile the daemon wrote: build it with
`--features heap-profile`, run it with `RUSTD_MEMORY=DIR _RJEM_MALLOC_CONF=prof:true`.
"""
import re, sys, collections
QUERY = re.compile(r"<(.+?) as salsa::function::Configuration>::execute")
INTERN = re.compile(r"salsa::interned::IngredientImpl<(.+?)>>::intern")
def short(name):
    name = re.sub(r"::\{closure#\d+\}", "", name)
    name = re.sub(r"_?::Configuration_?$", "", name)
    name = re.sub(r"::\w+_shim$", "", name)
    return name.replace("<", "").replace(">", "")
OWN = [("symbol table", "SymbolTable"), ("vfs / file loading", "vfs::"), ("vfs / file loading", "load_cargo"),
       ("project model", "project_model"), ("object emission", "cranelift_object"), ("object emission", "object::write"),
       ("codegen driver (outside queries)", "hir_ty::mir::codegen"), ("rustd main (other)", "rustd::")]
def bucket(stack):
    frames = stack.split(";")
    for frame in reversed(frames):
        if m := QUERY.search(frame):
            leaf = "interned " if any(INTERN.search(f) for f in frames[frames.index(frame):]) else ""
            return "query", short(m[1]), leaf
        if "salsa::input" in frame or "set_file_text" in frame or "FileText" in frame:
            return "input", "salsa inputs (file text, crate graph)", ""
    for frame in reversed(frames):
        for label, needle in OWN:
            if needle in frame:
                return "own", label, ""
    return "other", next((f for f in frames[::-1] if "::" in f and "alloc" not in f and "prof_" not in f), frames[-1])[:90], ""
totals = collections.Counter(); interned = collections.Counter()
for line in open(sys.argv[1]):
    stack, _, size = line.rstrip().rpartition(" ")
    kind, name, leaf = bucket(stack)
    totals[kind, name] += int(size)
    if leaf: interned[kind, name] += int(size)
whole = sum(totals.values())
print(f"total sampled live: {whole / 2**20:.0f} MB")
for kind in ("query", "input", "own", "other"):
    print(f"{kind}: {sum(v for (k, _), v in totals.items() if k == kind) / 2**20:.0f} MB")
top = int(sys.argv[2]) if len(sys.argv) > 2 else 45
for (kind, name), size in totals.most_common(top):
    print(f"{size / 2**20:8.0f} MB {100 * size / whole:5.1f}%  {kind:6} {name}")
