# rustd on Windows x86-64 (MSVC)

Branch `rustd-windows`, off `rustd-spike`. Everything below marked "measured" was run on this
machine: Windows Server 2022 (10.0.20348), x86-64, 8 cores of a Xeon 8559C, 32 GB, MSVC
`link.exe` 14.44, the harness toolchain `nightly-2026-05-10`, rustd built with rustc 1.99.1.
Anything I did not run is marked "inferred" or "not run".

The sections from "Harness results" down describe that first port. What changed after it
(the LLVM backend, two-float returns, C variadic functions, naked functions) and the current
harness results are in "Since the first port" at the end, which overrides them where they
disagree.

## What works

rustd builds with `cargo build -p rustd --release` and runs as the daemon on Windows. It emits
COFF objects, they link with MSVC `link.exe` (the linker rustc itself picks for this target; the
harnesses take the line from rustc, and also accept `rust-lld`/`lld-link` lines), and the
programs behave as rustc's do. With `--splice` the daemon appends a patch into the linked
`.exe`, and declines with a reason when it cannot, after which the full link runs.

What changed, by area:

- Target. The triple is picked from the host in one place (`TRIPLE` in `codegen.rs`); the
  calling convention, PIC, TLS model, stack probing and frame pointers follow from the target's
  object format, OS and default calling convention. The hardcoded `CallConv::SystemV` are now
  `call_conv()`.
- Calling convention. Rust's own ABI goes through Cranelift's `WindowsFastcall`. The C ABI has a
  Win64 classifier (`win64_pass_mode`, mirroring rustc's `x86_win64.rs`): what is not 1, 2, 4
  or 8 bytes goes by reference. 128-bit integer helpers of compiler-builtins are called the way
  C has them there.
- COFF. Symbols another object defines are reached directly (no GOT on PE). Frames larger than
  a page probe the stack inline, which Windows needs to commit it.
- Unwinding. `.pdata`/`.xdata` are written per function (`codegen/unwind/seh.rs`). Cranelift
  0.136 cannot make a call that names its landing pad for `WindowsFastcall` (it panics in the
  x64 ABI code), so a call with a pad is a plain call followed by a look at a slot of the
  frame. Each object carries its own personality function, written as Cranelift IR: it claims
  C++ exceptions (what a Rust panic is on MSVC), moves the thrown object to the heap, has the
  frames below unwound with `RtlUnwindEx`, and resumes at the call's return address with the
  object in the slot. Cleanup pads throw it again. Cranelift's own Windows unwind codes were
  wrong for frames with a frame pointer plus saved registers; `seh.rs` rewrites them.
- `asm.rs` wrappers take their argument and callee-saved registers from the convention, and
  save xmm6 to xmm15 on Windows.
- Daemon. The socket is an AF_UNIX socket through `uds_windows`. `--link-cmd` is run without
  a shell on Windows. Files are compared with `same-file` rather than device and inode, paths
  are canonicalized without the `\?\` prefix, and the rustc stub spawns rather than execs.
- A running `.exe` cannot be written, not even by the linker (LNK1104). When the executable
  is open elsewhere, the daemon renames it aside (`name.<pid>-<n>.running`, removed on a later
  link once it is free) and the full link writes a new file. The splice declines with "the
  executable is running", as on ELF.

## The PE splice

`crates/rustd/src/splice/pe.rs`. The contract is the ELF one: `Baseline::read` and `Addition`
are shared, and the format is chosen by what the executable is.

- Symbols. A PE has no symbol table and `link.exe` cannot write one, so names come from the
  public symbols of the PDB next to the executable, checked against the GUID in the
  executable's CodeView record. No PDB, or one of another link, declines.
- Sizes. Neither the PDB publics nor COFF say how large anything is. A function is as long as
  its `.pdata` entry. A static has the room it had in the object it was linked from, up to the
  next symbol of its section; a static that shares its place with another name has none.
- Layout. Four sections are appended after the linker's last: read-only (constants, `.xdata`,
  and a merged, sorted `.pdata`), code (the patch's functions and stubs for imports the linker
  made none for), data, and one holding only the merged `.reloc` table. The exception and
  base-relocation directories are pointed at the new tables; the old ones stay, unused. The
  section headers go in the room the linker left after its own; no room declines.
- Old functions get a jump to the new one. A static whose value changed is overwritten in
  place when the new one fits and is aligned; otherwise the splice declines.
- ASLR. Every absolute address the patch stores gets a `DIR64` base relocation in the merged
  table. An image that is moved but has no `.reloc` declines.
- It declines for: a signed image, a .NET image, Control Flow Guard, thread-local data or
  `.CRT` initializers in the patch, a relocation only the linker handles, a name that is
  missing from or ambiguous in the executable, a function too short to jump from or folded
  with another, and a static that grew.
- The PDB is not touched and goes stale: a debugger sees the old functions and nothing of the
  appended sections. Unwinding through the new code works (the unwinds are in the merged
  `.pdata`), which the splice harness checks by panicking through it.
- Every spliced executable in `splices.py` is also checked with `dumpbin` (headers,
  relocations, unwind info, imports) and by walking the tables.

## Harness results on Windows (measured, final run on the last code commit)

Run from `harness/` with `python`. The harnesses were ported, not skipped: `difftest.py` has
the helpers for either kind of link line and the others use them.

| Harness | Result |
|---|---|
| `difftest.py corpus --bin runner --crate cases` | MATCH, 674/680 functions from rustd |
| `programs.py` | 22 of 22 |
| `stress.py` | 99 of 99, 0 restarts |
| `breakers.py` | 15 of 15, 1 open (`future_size`, open on Linux too) |
| `unwinds.py` | 8 of 8 |
| `RUSTD_SPLICE=1 splices.py` | 71 of 71 |
| `RUSTD_SPLICE=1 worktrees.py` | 52 of 52 |
| `asmedits.py` | 10 of 10 |
| `deps.py` | 10 of 10 |
| `includes.py` | 9 of 9 |

Corpus is 674 where Linux has 679: five functions return two floats in registers, which
Cranelift's `WindowsFastcall` cannot do ("Too many return values to fit in registers",
bytecodealliance/wasmtime#9510). They are left to rustc and the output still matches.

Of the 71 splice steps, 60 were spliced and 11 took the full link, each with the result
rustc gives: 4 whole-object rewrites, 3 executables written by someone else, 2 statics that
grew, 1 `abs` not in the executable, 1 running
executable.

`deps.py` needs the registry crates in cargo's cache (it builds offline); the first run
failed until I fetched them.

Not run on Windows: `scoreboard.py`, `cratetest.py`, `agents.py`, `dxedits.py`, `uitest.py`,
`scaling.py`, `invalidation.py`, `linkbench.py`, `backends.py`, `heap.py`, `layoutdiff.py`,
`shapes.py`, `alone.py`, `bintest.py`, `cratesweep.sh`. They were not in the list of checks
and I did not port them; several still build a `cc` link line by hand and will not work as
they are. The LLVM backend (`--features llvm`) was not built or tried on Windows.

## Linux x86-64 (measured, by a second session on a Linux VM)

Commit `a5aedad842` of this branch on Linux x86-64, same harness toolchain: corpus 679/680
MATCH, programs 22/22, stress 99/99, breakers 15/15 (1 open), unwinds 8/8, splices 71/71,
worktrees 52/52, asmedits 10/10, deps 10/10, includes 9/9, and `bench.py toy corpus` ran in
both modes (10/10 spliced with `RUSTD_SPLICE`). These are the numbers of `rustd-spike`. The
two commits after `a5aedad842` change a harness JSON path, a docstring and parentheses in the
Windows-only `seh.rs`; they were not rerun on Linux.

`cargo clippy -p rustd --all-targets` fails on Linux with one `large_enum_variant` error in
`remote.rs`; `rustd-spike` fails the same way. The branch adds three style warnings in
`pe.rs` (`chunks_exact` with a constant).

## Edit to binary: full link vs splice (measured)

`harness/bench.py`, 10 edits each, medians, in ms. "Total" is the daemon's own
`edit-to-binary`: from noticing the file change to the executable being ready to start. It
does not include starting the program. The edit flips a literal in a function body.

| Program | Mode | Codegen | Link | Total | cargo build |
|---|---|---:|---:|---:|---:|
| toy (`demo`, 7 functions) | full link | 6.4 | 87.1 | 98 | 251 |
| toy | splice | 4.3 | 0.22 | 9.4 | 245 |
| regex (`bench2`: regex-automata, 1989 of 2494 functions from rustd) | full link | 241 | 303 | 548 | 2927 |
| regex | splice | 6.3 | 0.53 | 18 | 2938 |

The full-link mode of `bench.py` is the one on `rustd-spike`: no `--patch`, so each edit also
writes the whole object again, which is the codegen difference on regex. The link column is
the cleaner comparison: 87 ms to 0.2 ms and 303 ms to 0.5 ms. All 10 of 10 edits were spliced
in both programs. The first edit after a start is slower (toy 6.8 ms, regex 294 ms with
splice).

The `corpus` bench could not be measured with splice, and that is a real limitation, below.
Without splice it ran (link 339 ms).

## What does not work, and why

1. Fixed since, see "Since the first port": functions that return two floats in registers
   (`(f64, f32)`, a struct of two `f64`) were left to rustc, and `--patch` refused every
   body edit to a file that held one.
2. A static whose value grows is not spliced (2 steps of `splices.py` take the full link).
   PE has no symbol sizes, so the splice only overwrites in place and has no way to move a
   static.
3. Thread-local data in a patch declines the splice.
4. The PDB is stale after a splice; debugging spliced code shows the wrong source.
5. Exceptions other than C++ ones (structured exceptions, foreign unwinds) pass through rustd
   frames without running their cleanup. A Rust panic is a C++ exception on MSVC, so panics
   are not affected. Inferred from the personality's filter, not tested.
6. Executables with Control Flow Guard, signed, or linked without a PDB always take the full
   link. Inferred from the checks in `pe.rs`; the harness links none of these.
7. `.running` files left by a daemon that was killed while the program ran stay on disk until
   a later link by a daemon for the same executable removes them. Inferred from the code.

One thing I saw and did not explain: `stress.py` on Windows answers steps 56 and 58 (a
static's value changed) with a patch where the `STRESS.md` committed on `rustd-spike` shows a
full build. Both give rustc's output. I left the committed `STRESS.md` as it is.

## What is left, ranked

1. Done: two-float returns (item 1 above).
2. Port the remaining harnesses (`scoreboard.py`, `cratetest.py` and the others listed) and
   run the real-crate ladder on Windows. Only the corpus, the daemon fixtures and
   regex-automata have been through this backend there.
3. Moving a grown static: add it in the new data section and redirect every reference to it.
4. Real landing pads once Cranelift has exception support for `WindowsFastcall`. The current
   scheme costs a load and a branch after every call that has a pad, and a heap allocation per
   caught panic.
5. Updating or regenerating the PDB after a splice, or at least marking it stale.
6. Done: the LLVM backend on Windows.
7. Thread-local data in patches.

## Other Windows targets (inferred, nothing implemented or run)

`x86_64-pc-windows-gnu`: codegen would mostly carry over (same calling convention, COFF,
`.pdata`). The differences: panics unwind with the GCC-style personality over SEH
(`rust_eh_personality` with an LSDA, and `_Unwind_Resume`), so the tables of the ELF side
would be reused with Windows unwind codes instead of the personality in `seh.rs`; TLS depends
on the toolchain; the linker is `gcc`/`ld` with a `cc`-style line; and the linked image keeps
a COFF symbol table and has no PDB, so the splice would read names from the image itself,
which is simpler than the PDB route. Symbol sizes are still missing.

`aarch64-pc-windows-msvc`: the splice's container work (sections, directories, PDB names) is
the same, but everything about instructions is not. ARM64 unwind codes in `.xdata` are a
different format, the jump written over an old function is a different instruction with a
128 MB reach, the COFF relocations are the ARM64 set (page-relative pairs), and the SEH
workaround would need redoing for Cranelift's AArch64 backend. The ELF splice is x86-64 only
too, and the `asm.rs` wrappers are x86 text, so this is an architecture port first and a
Windows port second.

## Since the first port

This section is newer than the rest of the file and was measured on the same machine, on
`rustd-windows-variadic` (commit `250adbbcb2`, on `rustd-collate` `530ce12b8f`), with stable
1.99.0 as the harness toolchain and LLVM 20.1.8.

### The LLVM backend

`RUSTD_BACKEND=llvm RUSTD_OPT=2 RUSTD_EMIT=fast` compiles every function Cranelift does on
Windows; nothing is left to Cranelift in any harness. What it took:

- `emitted.rs` reads COFF relocations back (REL32 and its `_1` to `_5` forms, SECREL,
  ADDR64, with the addend in the field), and takes each function's `UNWIND_INFO` from LLVM's
  `.pdata`/`.xdata` rather than looking for `.eh_frame`.
- A call with a landing pad is an `invoke` under LLVM. The pad stores the exception in a
  slot of the function's own, and the table of pads is read back from `.xdata`. The pad table
  rustd writes is three `u32` per entry (the range of return addresses and the pad), and the
  personality enters the pad rather than returning to the call.
- LLVM writes no `.pdata` entry for a leaf that leaves the stack alone, and the splice tells
  where a function ends from that entry, so such a function gets a frameless `UNWIND_INFO`.
  Code shorter than the jump the splice writes over an old function is padded, as
  Cranelift's is.
- `probe-stack=inline-asm` made LLVM call a function of that name on PE; on COFF the probe is
  `__chkstk`, through `Target::stack_probe`.

Building it is not in the repository: the LLVM release for Windows is built against the
static C runtime, so rustd needs `-C target-feature=+crt-static`, the Windows SDK library
directory on `-L native=`, and a stub `libxml2s.lib`.

### Two floats returned in registers

rustc returns a pair of floats in `xmm0` and `xmm1`, and Cranelift's `windows_fastcall` has
only `xmm0`. Before Cranelift compiles a function, a return or a call of two floats is made
one of a single float, and two small assembled helpers carry the second through `xmm1`
(`codegen/float_pair.rs`). The call side relies on Cranelift putting nothing between a call
and the helper that reads `xmm1`; the emitted code is checked for that, and a function that
fails the check is left to rustc. The return side relies on the epilogue not touching `xmm1`,
which is not checked. LLVM returns the pair itself.

With this the five corpus functions are compiled, and `--patch` no longer refuses edits to
files that hold such a function.

### C variadic functions

A function that takes `...` is compiled for Windows x64 under both backends. `VaList` is one
pointer there and every argument is an 8-byte slot. The function takes the registers its
named parameters left as integers (the caller duplicates a float in the integer register)
and writes them into the room the caller leaves above the return address, after which the
registers and the stack are one run of slots; `va_arg` reads a slot and steps over it. Under
Cranelift the address of that room is a `StructArgument` parameter. LLVM takes `byval` to be
a pointer on Windows, so there it is `llvm.addressofreturnaddress` plus the offset.

Left to rustc: a variadic function with no named parameter (the first slot is where the
exception of a guarded call arrives), with a named parameter wider than 8 bytes, or of an ABI
other than `C` and `win64`; and `va_arg` of a type wider than 8 bytes. None is in the corpus.

### Naked functions and asm labels

These needed nothing in rustd. The corpus cases were System V only because two of them name
`rdi` and `rsi`; they have Windows variants now, and `varargs_abi` calls `snprintf` as
`extern "win64"` there. `snprintf` is in `legacy_stdio_definitions.lib` on MSVC.

### Harness results (measured)

One binary built with `--features llvm`; the Cranelift column is that binary with the
default backend. "Compiled / left" is how many functions LLVM compiled and how many it left
to Cranelift.

| Harness | LLVM | LLVM compiled / left | Cranelift |
|---|---|---|---|
| `difftest.py corpus --layouts` | MATCH, 806/806, 0 of 897 layouts differ | 11748 / 0 | MATCH, 806/806, 0 of 897 |
| `scoreboard.py` | 6 targets match | 18591 / 0 | 6 targets match |
| `programs.py` | 22 of 22 | 1289 / 0 | 22 of 22 |
| `stress.py` | 99 of 99, 0 restarts | 5425 / 0 | 99 of 99, 0 restarts |
| `breakers.py` | 25 of 25, 0 open | 3065 / 0 | 25 of 25, 0 open |
| `unwinds.py` | 8 of 8 | 102 / 0 | 8 of 8 |
| `RUSTD_SPLICE=1 splices.py` | 71 of 71 | 2337 / 0 | 71 of 71 |
| `asmedits.py` | 10 of 10 | 20 / 0 | 10 of 10 |
| `deps.py` | 10 of 10 | 0 / 0 | 10 of 10 |
| `includes.py` | 9 of 9 | 76 / 0 | 9 of 9 |
| `RUSTD_SPLICE=1 worktrees.py` | 52 of 52 | 2369 / 0 | 52 of 52 |
| `worktrees.py --join --together` | 52 of 52 | 2385 / 0 | 52 of 52 |
| `worktrees.py --overlay` | 61 of 61 | 1950 / 0 | 61 of 61 |

The corpus has 806 functions on Windows and rustd compiles all of them. `deps.py` compiles
no function; it pushes and pulls dependency archives.

`splices.py` splices 59 to 61 of the 71 steps and the rest take the full link. Repeated runs
gave 61, 60, 59, 60 under LLVM and 60, 60, 60 under Cranelift, all 71 of 71 right. Nine
declines are the same every time (4 whole-object rewrites, 3 executables written by someone
else, 1 `abs` not in the executable, 1 running executable). The rest are "the static has
another value that is larger", and which steps get it, and the sizes it names, differ from
run to run under both backends. I did not find out why.
