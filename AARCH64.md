# rustd on aarch64-unknown-linux-gnu

Branch `rustd-aarch64-linux`, off `rustd-spike`. Everything here was done on a Linux x86-64
machine (8 cores) as a cross target. rustd, cargo, rustc and the linker ran as x86-64
programs; only the programs they produced are AArch64, and those ran under `qemu-aarch64`
6.2 in user mode against the Ubuntu sysroot in `/usr/aarch64-linux-gnu`, linked by
`aarch64-linux-gnu-gcc` 11.4 (GNU ld 2.38). Nothing was run on AArch64 hardware or on an
AArch64 kernel. rustd never had to run as an AArch64 process, and I did not try to build it
as one.

## How the target is picked

The target is an input: `rustd --target <triple>`, else `CARGO_BUILD_TARGET`, else the host.
One process has one target, named before anything is compiled. `crates/hir-ty/src/mir/
codegen/target.rs` turns the triple into a `Target` with four properties: architecture,
object format, C calling convention and unwind format. The Cranelift ISA is built from it,
the C ABI is chosen by its architecture, the inline assembly wrapper is chosen by its
architecture, and the code cache key includes the triple so objects for two targets never
mix. The splice does not ask the daemon what the target is; it reads the machine from the
ELF header of the executable it is given.

Only two triples are accepted today, x86-64 and AArch64 Linux. Anything else is refused by
name. `ObjectFormat` and `UnwindFormat` have one variant each (ELF, `.eh_frame`), so they
are places for the macOS, Windows and wasm work to add to, not something this branch
proves works for a second format.

The harnesses take `RUSTD_TARGET=<triple>`. `difftest.py` holds, per target, the linker,
the emulator to run under and the binutils prefix, and the other harnesses use those.

## What works

All of these were run. "x86-64" is the same tree, same build, on the host.

| Harness | AArch64 (qemu) | x86-64 |
|---|---|---|
| `difftest.py corpus --bin runner --crate cases` | MATCH, 685 of 686 functions from rustd | MATCH, 684 of 685 |
| `programs.py` | 22 of 22 | 22 of 22 |
| `stress.py` | 99 of 99, 0 restarts | 99 of 99, 0 restarts |
| `breakers.py` | 15 of 15 | 15 of 15 |
| `unwinds.py` | 8 of 8 | 8 of 8 |
| `RUSTD_SPLICE=1 splices.py` | 71 of 71 | 71 of 71 |
| `RUSTD_SPLICE=1 worktrees.py` | 52 of 52 | 52 of 52 |
| `asmedits.py` | 10 of 10 | 10 of 10 |
| `deps.py` | 10 of 10 | 10 of 10 |
| `includes.py` | 9 of 9 | 9 of 9 |
| `RUSTD_SPLICE=1 farsplices.py` (new) | 6 of 6 | 6 of 6 |
| `RUSTD_SPLICE=1 programs.py` | 22 of 22 | 22 of 22 |
| `RUSTD_SPLICE=1 stress.py` | 99 of 99 | 99 of 99 |
| `RUSTD_SPLICE=1 unwinds.py` | 8 of 8 | 8 of 8 |

The one corpus function not from rustd is the same on both (a `TypeId` rustc has no symbol
for). The corpus counts are higher than the 679 of 680 on `rustd-spike` because I added
cases: NEON fixtures on AArch64, and C ABI cases on both. The table is from one full run of
everything on the final splice code; after it I added three more C ABI cases and reran only
the corpus on both targets, which is the count shown. The other rows were not rerun after
that, and that later change touched only the corpus sources and a guard in the LLVM
backend, which is not compiled in.

What is behind those numbers:

- Calling convention. AAPCS64 for `extern "C"`: small aggregates in one or two integer
  registers, homogeneous float aggregates in float registers, anything over 16 bytes by
  pointer, the result pointer in x8, 128-bit integers starting at an even register. The
  corpus has rustc-built code calling rustd-built code and back for each of these,
  including an HFA that no longer fits the float registers, a five-float struct returned
  through x8, and 128-bit values in registers and on the stack.
- Inline assembly. A second wrapper, `asm/aarch64.rs`, next to the x86-64 one. It is
  assembled by the pinned rustc for the target, so that toolchain needs the
  `aarch64-unknown-linux-gnu` target installed.
- Unwinding. `.eh_frame` and landing pads come from Cranelift's AArch64 backend through
  the same code as x86-64; `unwinds.py` passes, linked in full and spliced.
- Thread locals. The objects use the TLS descriptor relocations (`R_AARCH64_TLSDESC_*`); I
  saw them in the ripgrep object, and the programs with thread locals run correctly.
- NEON. No AArch64-specific intrinsic had to be written: the `core::arch::aarch64`
  functions the corpus and ripgrep reach go through the generic SIMD intrinsics that were
  already implemented. I did not survey which `llvm.aarch64.*` intrinsics std can reach; a
  program that hits one will report it as unsupported.
- The splice. AArch64 ELF is handled with the same contract as x86-64. Calls and jumps use
  `b`/`bl` when the target is within 128 MB. Past that, the patch reaches old code through
  a three-instruction stub (`adrp x16; add x16; br x16`). An old function is redirected
  by a single `b` over its entry when the new one is in reach, and by that
  three-instruction sequence when it is not; a function shorter than the 12 bytes that
  takes is not redirected and the splice declines. Imports go through
  `adrp; ldr; br` stubs on new GOT slots. Relocations handled: ABS64, PREL32, CALL26,
  JUMP26, ADR_PREL_PG_HI21, ADD_ABS_LO12_NC, LDST*_ABS_LO12_NC, ADR_GOT_PAGE,
  LD64_GOT_LO12_NC; any other declines. `farsplices.py` puts 160 MB of data between old
  and new code to force the far forms and checks both the far splice and the decline.
  Two unit tests check the instruction encodings against `aarch64-linux-gnu-as` and the
  reach limits.

Of 133 links in the spliced `worktrees.py` and `stress.py` runs on AArch64, 103 were
spliced. The 30 that were not gave the same reasons, in the same numbers, as on x86-64:
patch has thread-local data (14), no executable yet (11), whole object rewritten (5).

## What does not work, or was not shown to

- Not run on real hardware or a real AArch64 kernel. One thing in particular depends on
  that: qemu's loader crashed on the way the splice used to place the new program headers,
  and I moved them to their own aligned place in the file to fix it. x86-64 still passes
  with the new layout. I infer a real kernel accepts it, since it is the stricter of the
  two layouts, but I have not seen it.
- Replacing a binary that is running. On Linux the kernel refuses the write and the full
  link runs; `splices.py` checks that. Under qemu the binary is only a file qemu has open,
  so the write succeeds, and that one assertion is skipped when an emulator is set. On
  real AArch64 Linux I expect the x86-64 behaviour; not measured.
- BTI. An executable marked as needing `bti` landing pads is declined, because the jump
  written over a function's entry is not one. The check is written but nothing exercised
  it: no test builds a BTI executable.
- C ABI cases that are refused rather than compiled: a 128-bit argument when exactly one
  integer register is left, an aggregate aligned to 16 bytes that has to go on the stack,
  SIMD vectors, and enums by value. These functions are left to rustc, as on x86-64 for
  the cases it has.
- Inline assembly the wrapper refuses: `may_unwind`, templates that name a symbol,
  operand modifiers and register classes beyond general and vector registers. Refused
  with a reason, not miscompiled, but the set that is covered is what the corpus and
  `asmedits.py` use and no more.
- The LLVM backend emits x86-64 only. I added a refusal for any other target so it cannot
  emit the wrong architecture, but LLVM is not installed on this machine, so that change
  was not compiled and `backends.py` was not run.
- Valgrind's memcheck step in `difftest.py` is skipped for a cross target; it cannot run
  an emulated program.
- `breakers.py` has one x86-only fixture (carry-less multiply through `pclmulqdq`). On
  AArch64 it compiles to an empty result on both sides, so it passes without testing
  anything.

Harnesses I did not run for AArch64: `scoreboard.py`, `cratetest.py`, `agents.py`,
`scaling.py`, `backends.py`, `bench.py`, `linkbench.py`, `dxedits.py`, `uitest.py`,
`heap.py`, `alone.py`, `bintest.py`, `invalidation.py`, `layoutdiff.py`, `shapes.py`,
`sweepreport.py`. `cratetest.py` and `agents.py` had their run paths ported along with the
others but were not run. None of these were in the list I was given.

## Edit to running

Measured with the new `harness/edittimes.py`: one daemon without `--splice` and one with,
the same one-line edit swapped in and out 11 times each, `rustd sync` timed, then the
program run. Medians are of the last 10. The compile and the link or splice are native
x86-64 speed, since rustd and the linker are host programs. The run column is qemu and
says nothing about AArch64 hardware; it is there to show the program ran, and it is the
same in both rows.

| Program | Way | sync (ms) | of which link (ms) | run under qemu (ms) | edit to running (ms) |
|---|---|---:|---:|---:|---:|
| stress app, 3 crates, 1 MB | full link | 131.3 | 122.8 | 64.8 | 196.0 |
| stress app | splice | 6.1 | 0.17 | 65.1 | 71.2 |
| ripgrep 14.1.1, 9 crates, 69 MB | full link | 2018.5 | 2000 | 811.6 | 2830.1 |
| ripgrep | splice | 18.9 | 2.55 | 806.0 | 824.9 |

The ripgrep edit is an unused `let` in `main`, and the run is `rg -c "fn " crates/core`;
every run exited 0, but its output was not compared with cargo's the way the harnesses
above compare. The full link is GNU ld through gcc, the default for the cross toolchain;
a faster linker would shrink the full-link numbers, and I did not try one. The ripgrep
log reports about 2 MB added to the binary per splice, against about 130 KB for the small
program; I did not look into why.

What I would infer for real hardware: the sync column carries over in shape, since it is
compile plus link and neither is emulated here, but an AArch64 host would have its own
linker and CPU speeds. The run column does not carry over at all.

## What is left, most important first

1. Run the lot on AArch64 hardware. Confirms the program header layout under a real
   kernel, the busy-binary decline, and gives real timings. rustd as a native AArch64
   process is untested too; `Target::host()` names the triple but nothing built it.
2. BTI: build a `-z force-bti` executable in a test and see the decline. Then make the
   redirect a valid landing pad so those binaries can be spliced.
3. Find out why a ripgrep splice appends 2 MB each time and whether it should.
4. The refused C ABI cases: 16-byte-aligned aggregates on the stack and the 128-bit
   argument with one register left. Both need stack slots at 16-byte alignment that the
   current pass modes cannot express.
5. Splicing patches with thread-local data (declined on both architectures today; on
   AArch64 it means TLSDESC sequences in appended code).
6. Survey the `llvm.aarch64.*` intrinsics std and common crates reach, and give the
   carry-less multiply breaker an AArch64 body (`pmull`) so it tests something.
7. LLVM backend for a second architecture: take the triple and CPU from `Target`.
8. Inline assembly: symbols in templates, more register classes and modifiers.
9. `Target` for other systems: `parse` accepts Linux only, and object format and unwind
   format have one variant each.

## Round on 530ce12b8f: variadics, asm labels, LLVM

Everything below ran on an x86-64 host with the AArch64 programs under `qemu-aarch64` user
mode, stable 1.99.0. Nothing ran on AArch64 hardware.

Before any change, the Cranelift matrix on 530ce12b8f passed in full. The corpus was
788/791 from rustd; the three left to rustc were `sum_all`, `forward` and `weigh` in
`stabilized.rs`, each with "a function that takes `...` for this target".

**C variadic definitions.** `VariadicRegisters` now covers AAPCS64 as Linux has it as well
as x86-64 System V. The function is declared with all eight integer and eight vector
argument registers, saves the ones its named parameters left, and fills the five fields
of `VaListInner` (stack, end of the saved integer registers, end of the saved vector
registers, and the two negative offsets). `va_arg` counts the offset up to zero and then
reads the stack. Cranelift has no parameter that stands for the caller's stack on AArch64,
so the function takes the address from its frame pointer (the arguments start 16 bytes
above it). Cranelift only answers `get_frame_pointer` when the code generator keeps frame
pointers, so `emit` compiles a function that asks for it in its entry block with a third
code generator that does; other functions are compiled as before. Still refused, as on
x86-64: a named parameter on the stack, a named parameter that is not an integer or float
of at most 8 bytes, and `va_arg` of anything wider than 8 bytes.

**Asm labels and naked functions.** `stabilized.rs` has a module `arm` with the AArch64
equivalents of the x86 cases (`naked_functions`, `asm_goto`, `asm_cfg`,
`target_feature_11`, `naked_more`, `asm_goto_more`). They passed without a change to
`asm/aarch64.rs`. There was no `rustd-macos-variadic` branch to take cases from when I
looked.

**LLVM.** The read-back of an AArch64 ELF object now goes the way the Mach-O one does:
the function's section alone is the code, and every section it refers to becomes a data
object, since ADRP pairs are resolved by the linker. The ELF relocations read are CALL26,
JUMP26, ADR_PREL_PG_HI21, ADD_ABS_LO12_NC, ADR_GOT_PAGE, LD64_GOT_LO12_NC and the four
TLSDESC ones. On the corpus LLVM -O2 compiles 11307 of 11761 functions (96.1%). Left to
Cranelift: 448 because LLVM folds the low 12 bits of an address into a load
(`R_AARCH64_LDST8/64/128_ABS_LO12_NC`, 434 + 1 + 13), which Cranelift's relocation kinds
cannot say on ELF; 3 with more than two return values; 3 that take `...` and ask for the
frame pointer. The LLVM lowering used to treat `get_frame_pointer` as the SEH arrival slot
on every target; it now does that only where exceptions arrive in the frame.

Results after the changes:

| Harness | Cranelift | LLVM -O2 fast emit |
|---|---|---|
| corpus `--layouts` | MATCH, 802/802, 0 of 903 layouts differ | MATCH, 802/802, 0 of 903 |
| corpus `--mixed 6` | 6 splits match | not run |
| programs | 22/22 | 22/22 |
| stress | 99/99 | 99/99 |
| breakers | 25/25 | 25/25 |
| unwinds | 8/8 | 8/8 |
| splices | 71/71 (61 spliced) | 71/71 (61 spliced) |
| farsplices | 6/6 | 6/6 |
| asmedits | 10/10 | 10/10 |
| deps, includes | 10/10, 9/9 | not run |
| worktrees plain, `--join --together`, `--overlay`, with splice | 52/52, 52/52, 61/61, 52/52 | not run |

The Cranelift column was run one commit-worth of edits before the end: the only change
after it is the `get_frame_pointer` refusal in the LLVM lowering, which Cranelift does not
reach.

What is left from this round: the `LDST*_ABS_LO12_NC` relocations. Closing it needs
either those kinds in Cranelift's `Reloc` (and `cranelift-object`), or having LLVM reach
data in the program through the GOT on this target, which costs a load on every reference.
