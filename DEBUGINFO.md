# Debug info

Branch `rustd-debuginfo`, from `rustd-collate`. Everything here is Linux x86-64 ELF. Mach-O
and Windows are design notes at the end.

## What there is now

Every function rustd compiles is described in DWARF 4: one compilation unit per object, a
`DW_TAG_subprogram` per function with its name, linkage name, declaration file and line and
its range of code, and a `.debug_line` program that gives a file, line and column to every
instruction that came from a statement. The producer string starts with `rustd`, which is how
the harness tells rustd's units from the ones rustc wrote for the dependencies.

The two backends get there differently.

Cranelift: each MIR statement and terminator sets a `SourceLoc` on the instructions built for
it. The location is an index into a table of spans kept per function, not a line, so an edit
above a function does not change its compiled code or its cache entry. After emission the
source-location ranges of the machine buffer become rows of (code offset, span index). Lines
are looked up from spans only when the function is put into an object, and gimli's writer
produces `.debug_info`, `.debug_abbrev`, `.debug_line`, `.debug_str`, `.debug_line_str` and
`.debug_ranges` with their relocations (`crates/hir-ty/src/mir/codegen/debuginfo.rs`).

LLVM: the lowering from Cranelift IR to LLVM IR creates a `DISubprogram` per function through
LLVM's DIBuilder and puts a debug location on every instruction, with the same span index in
the place of the line number. LLVM writes a `.debug_line` for its module, rustd reads it back
with gimli into the same (offset, span) rows, and from there the path is the one Cranelift
uses. So the optimizer decides which instruction belongs to which statement, and the final
sections are written by the same code for both backends.

Names are what rustc's demangler prints for its v0 symbols: `krate::module::f::<u8>`,
`<T as Trait>::f`, `f::{closure#0}`, with default generic arguments left out and closures
numbered in rustc's order.

The end of a scope, where its drops run, is given the place of the closing brace, as rustc
does. Before this the drops had the line where the scope's expression begins.

Debug info follows the profile. `crates/rustd/src/profile.rs` reads, in this order,
`RUSTD_DEBUGINFO` (an override for measuring), `CARGO_PROFILE_<NAME>_DEBUG`,
`[profile.<name>] debug` in the workspace manifest, the profiles it `inherits` from, and
cargo's defaults (on for `dev` and `test`). `--profile NAME` picks the profile, `dev` if it
is not given. With debug off no debug section is written. The setting is part of the cache
key.

Variables: under Cranelift a parameter or a `let` binding is described
(`DW_TAG_formal_parameter`, `DW_TAG_variable`) when its value lives in one stack slot for the
whole function, by `DW_OP_fbreg` from `rbp`. The types described in full are integers, floats,
`bool`, `char`, references and raw pointers, `&str` and slices (as `data_ptr` and `length`),
arrays, tuples and structs with their fields at their real offsets. Enums, unions, closures
and everything else are described by name and size only, so a debugger shows them as a value
without fields.

## The splice path

Decision: append, do not patch.

The sections a debugger reads are not loaded, and each one is a single run of the file found
through the section headers. There is no room to grow one in place, and reserving room at
link time would mean guessing a size and still running out. A splice already starts from the
linker's file every time (it undoes the previous splice and truncates) and the patch object
is cumulative, so nothing has to be patched incrementally either.

So after the splice has written its loaded segments, for every `.debug_*` section the patch
object has, it writes a new copy at the end of the file: the executable's bytes first, then
the patch's, with the patch's relocations applied. A reference into another debug section
becomes the old size of that section plus the addend; a reference to code becomes the address
the splice placed that code at. A new section header table goes after that, with those
sections pointing at their new copies, and `e_shoff` in the ELF header is updated. The patch
is a compilation unit of its own, so the executable's units are untouched. The old code of a
replaced function is still in the file behind its jump and what the executable says about it
stays true.

If the executable has no section of a kind the patch has, or the section is compressed
(`SHF_COMPRESSED`), the splice is refused with that reason and rustd links in full instead.

A function is put in the patch again not only when its code changes but when any of its
lines change, so adding a line at the top of a file re-describes the functions below it. Its
compiled code comes from the cache; only the line lookup is redone.

The code is in `crates/rustd/src/splice/debug.rs`.

## What was measured

All on one machine, Linux x86-64, Rust 1.99.0, LLVM 20, at the head of this branch.

    cargo build -p rustd --release
    LLVM_SYS_201_PREFIX=/usr/lib/llvm-20 cargo build -p rustd --release --features llvm --target-dir target-llvm
    cd harness
    RUSTD_BIN=$PWD/../target/release/rustd python3 debuginfo.py
    RUSTD_BACKEND=llvm RUSTD_OPT=2 RUSTD_EMIT=fast RUSTD_BIN=$PWD/../target-llvm/release/rustd python3 debuginfo.py

`debuginfo.py` exits 0 under both backends ("all right"). `--only corpus|stress|rung2|edits|locals|cost`
runs one part. There is no `harness/programs` directory in the repository; the programs used
are the corpus runner, the `stress` workspace (three crates), `rung2`, and two small programs
the script writes itself.

What it checks, and what it printed:

Backtraces. Each program is built by cargo and by rustd, each deliberate panic is run with
`RUST_BACKTRACE=1`, and the frames that belong to the workspace's crates are compared by
name, file, line and column. The panic location itself is compared as well, which is the
`#[track_caller]` check: the corpus and the edits program panic through `#[track_caller]`
functions.

| program | Cranelift | LLVM -O2 |
|---|---|---|
| corpus runner | 84 panics, 444 frames, all the same | 84 panics, 443 frames, the same except 1 panic with one frame fewer |
| stress | 1 panic, 3 frames, same | 1 panic, 2 frames, one frame fewer |
| rung2 | 1 panic, 3 frames, same | same |

Under Cranelift the comparison is exact. Under LLVM at -O2 a panic passes if rustd's frames
are a subsequence of rustc's, and the script counts those: a tail call turned into a jump has
no frame. Nothing else differs.

One name is normalized by the script: rustc calls the code that drops a coroutine
`core::ptr::drop_glue::<X>` and rustd has the coroutine's own poll function do it, so the
frame is named `X`. The file, line and column agree. This affects 14 of the 84 corpus panics.

`llvm-dwarfdump --verify`: "No errors." for every binary, including after each splice.

Unwind tables against debug ranges. For every range in a rustd unit the script looks for the
frame the unwinder would find (through `PT_GNU_EH_FRAME` and the search table, not the
section header) and checks that it starts at the same address and does not end after the
range. Corpus: 11647 functions, 0 without a frame, 0 where the frame ends past the range.
Three functions have neither a frame nor lines; they are `#[naked]` functions, which are
assembly only. Stress: 1834 functions, rung2: 3039, edits: 71 to 79 after each splice, all
agree.

gdb, batch mode: break at the file:line of the call into the panicking function, check gdb
stops there, that the pc is in a rustd unit, that `info line` gives that line, then `step`
four times and check it reaches other lines. Passes for corpus, stress, rung2 and for the
binary after the last splice under both backends. `print` of locals is the `locals` part
below.

perf: `perf record -F 5000 --call-graph dwarf` on the corpus runner, then
`perf script -F ip,sym,srcline`. Cranelift: 62 frames of rustd functions in 34 samples, none
without a line, 21 samples unwound back to `main`. LLVM: 36 frames in 23 samples, none without
a line, 14 back to `main`. The machine needed `sudo sysctl kernel.perf_event_paranoid=1`;
where perf is not allowed the script says "not run" and does not fail. No flamegraph tool is
installed here, so no SVG was produced; the `perf script` output is what `stackcollapse-perf`
and `inferno` read.

Splice (`edits`): a program is built once under the daemon and edited six times: nothing, a
line added above everything, the panicking function made longer, its caller changed, a new
function added in the call chain, the added line removed. All 6 steps were spliced, and after
each the backtrace matches a fresh cargo build of the same source (7 or 8 frames),
dwarfdump verifies, and the unwind check agrees. Same under both backends.

Variables (`locals`): gdb stops in a function with three parameters and six locals and
prints each. Cranelift: 9 of 9 print the right value (`u32`, `f32`, `&str`, a struct, a tuple
struct, an array, a tuple, and a dereferenced reference). In the larger programs rustd
describes 12779 parameters and locals across 11647 functions in the corpus, 2503 in 1834 for
stress, 2474 in 3039 for rung2. Under LLVM none are described and the script reports that as
"not told of" (it would fail if any were, since they would be guesses).

lldb and addr2line were tried by hand on the locals program and are not in the script:
`addr2line -f -C -e bin 0x47387` gave `sline::area` and `src/main.rs:5`; `lldb-20 -b` with a
breakpoint on a line stopped, `frame variable` printed parameters and locals, `next` stepped
to the next line.

Cost (`cost`), corpus runner, best of three, compile and link of the `cases` crate object:

| | Cranelift, no debug | Cranelift, debug | LLVM, no debug | LLVM, debug |
|---|---|---|---|---|
| compile + link | 5.47 s | 6.67 s (+21.9%) | 6.55 s | 7.08 s (+8.0%) |
| object | 41,547,792 | 46,010,432 (+10.7%) | 37,258,400 | 40,728,472 (+9.3%) |
| debug sections | 0 | 2,398,617 | 0 | 1,817,651 |
| their relocations | 0 | 2,064,023 | 0 | 1,652,421 |
| linked binary | 86,975,096 | 89,373,712 | 83,597,832 | 85,415,480 |

The timing moves by a few points between runs (three runs at this head gave +19.4%, +20.9%
and +21.9% for Cranelift, and +8.0%, +8.1% and +12.0% for LLVM). Before variables were added the Cranelift object was 45,176,696
bytes with debug, so variables and types are about 0.83 MB of the 4.46 MB. With
`debug = false` the script checks that no debug section is written; with `true` and
`line-tables-only` that they are.

Regression gate, both backends, at the head:

    python3 difftest.py corpus --bin runner --crate cases --layouts   MATCH, 0 of 892 layouts differ
    python3 programs.py     22 of 22
    python3 stress.py       99 of 99, 0 restarts
    python3 breakers.py     25 of 25, 0 open, 0 functions skipped
    python3 unwinds.py      8 of 8
    python3 splices.py      71 of 71
    python3 asmedits.py     10 of 10
    python3 includes.py     9 of 9
    python3 worktrees.py    52 of 52
    cargo fmt --all -- --check                                         clean
    cargo test --release -p hir-ty -p hir-def -p parser -p syntax -p rustd   912 passed, 0 failed

The numbers are the same for Cranelift and LLVM.

## What is open

Inline frames. There are no `DW_TAG_inlined_subroutine` entries. rustd compiles each function
on its own, so as far as I can tell neither backend inlines one rustd function into another
and there is no inlined rustd code to describe. What was measured is that the frames match a
rustc dev build, apart from the tail calls counted above. If rustd starts inlining (LLVM across functions, or MIR inlining) this has to be added
at the same time, or backtraces will lose frames.

Variables under LLVM. LLVM chooses where locals live and rustd does not read that back. The
way to do it is `dbg.declare` on the allocas through DIBuilder and then reading LLVM's
`.debug_info` locations, or having LLVM write the whole unit. I did not start this. A few
days.

Scopes. Variables have no `DW_TAG_lexical_block`, so a local shows in the debugger from the
start of the function, with garbage in it until it is assigned. Because of that, a name that
the body binds more than once is not described at all (the debugger could not tell which one
is meant). Locals that Cranelift keeps in a register, locals of closures and of `async`
bodies, and unsized locals are not described either. In the corpus this leaves 12779
described; I did not count how many rustc describes for the same code.

Enums. Described by name and size only. Needs `DW_TAG_variant_part` with the discriminant
and niche layout from the layout query; the data is there. Two or three days with tests.

`line-tables-only` and `limited` get the same as `true`: the setting is read as on or off.
Variables should be left out for those levels. Small, but it adds a level to the cache key.

A `[profile]` in `.cargo/config.toml` is not read (FIXME in `profile.rs`).

After a splice, the `.eh_frame` section header still covers only the linker's frames. The
frames of spliced code are in the appended segment and are in the `.eh_frame_hdr` search
table, which is what the unwinder and perf use, and backtraces through spliced code are
right. A tool that reads `.eh_frame` by section name (`llvm-dwarfdump --eh-frame`) does not
see them. Spliced functions also have no `.symtab` entry; gdb names them from DWARF.

A breakpoint set by function name can land on the old copy of a replaced function, which is
still described by the executable's unit. Breaking by file:line finds the new code.

An edit that changes only a type (say a field's name) without changing the code of a function
that has a local of that type does not re-describe that function in the patch, so the
debugger shows the old field name until the next full link.

The coroutine drop frame name (`X` where rustc says `core::ptr::drop_glue::<X>`), described
above. Fixing it means emitting a real drop-glue function for coroutines instead of polling
with a null context.

DWARF 5 and split debug info (`split-debuginfo`, `.dwo`/`.dwp`) are not done. Compressed
debug sections in the executable make the splice fall back to a full link.

gdb prints a warning about `.debug_gdb_scripts` for these binaries; it comes from rustc's
std objects and is the same for the cargo build.

## Mach-O (design only)

On macOS the linker does not copy DWARF into the executable. It writes a debug map (`N_OSO`
stabs naming each object file, `N_FUN` for functions) and `dsymutil` later gathers the DWARF
from the objects into a `.dSYM` bundle; lldb can also read the objects directly through the
debug map.

For a full link this is mostly free: the gimli writer already produces the sections, and
they need to go into `__DWARF,__debug_*` with Mach-O relocations (`object` supports this; the
relocation kinds for section-relative references differ, `X86_64_RELOC_UNSIGNED` /
`ARM64_RELOC_UNSIGNED` and subtractor pairs). The backtrace in a Rust binary on macOS reads
the dSYM or the objects through the debug map, so rustd's object has to stay on disk where
`N_OSO` says or `dsymutil` has to be run after the link.

For a splice the debug map is the natural place: add an `N_OSO` entry for the patch object
plus `N_FUN` entries at the spliced addresses, and leave the DWARF in the patch object.
The Mach-O splice would have to rewrite the symbol table in `__LINKEDIT` for this; I have
not checked how much of that it already does. A stale `.dSYM` next
to the binary would win over the debug map, so the splice has to remove or regenerate it.

Estimate: one week for full-link objects with backtrace and lldb checks on aarch64 macOS, one
more for the splice and for a macOS variant of `debuginfo.py` (lldb in place of gdb,
`dwarfdump --verify`, `atos`). I have not run any of this.

## Windows (design only)

MSVC targets use CodeView: `.debug$S` (symbols and line tables per function) and `.debug$T`
(types) in the COFF object, which `link.exe` or `lld-link` merge into a PDB.

LLVM can write CodeView itself (module flag `CodeView`, the same DIBuilder metadata), so the
LLVM backend is the short path, but today rustd reads LLVM's line table back and writes the
sections itself, and that part would need a CodeView reader or a mode where LLVM's sections
are kept as they are. Cranelift has no CodeView writer, and as far as I know rustc's Cranelift
backend does not have one either. Writing `.debug$S` line tables and `S_GPROC32` records by hand is
well specified (the `C13` line format) and small for lines and names; types (`.debug$T`,
TPI/IPI hashing) are a lot more work.

The splice is the hard part. A PDB is a multi-stream file that is not designed to be
appended to; the realistic options are to rewrite the PDB after each splice (the `pdb` crate
only reads; LLVM's PDB writer is C++ and not exposed through the C API), or to emit
`*-windows-gnu`-style DWARF in the PE for spliced code only, which WinDbg and Visual Studio
do not read. Windows backtraces in Rust go through `dbghelp` and need the PDB.

Estimate: two weeks for line tables and procedure symbols in full links under both backends
with a `cdb`-driven harness, two to four more for the splice depending on which PDB route is
taken, and types on top of that. This is the least certain number here.
