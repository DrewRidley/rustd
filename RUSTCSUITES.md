# rustc's own test suites through rustd

Branch `rustd-rustc-suites`, off `rustd-collate` at `6d12f21c86`. Everything below was measured
on one Linux x86-64 VM with stable `rustc 1.99.0` and the rust-lang/rust checkout at the
`1.99.0` tag. Each number is from a single run unless it says otherwise. "Cranelift" is
`cargo build -p rustd --release`; "LLVM" is the same with `--features llvm`
(`LLVM_SYS_201_PREFIX=/usr/lib/llvm-20`), run with `RUSTD_BACKEND=llvm RUSTD_OPT=2
RUSTD_EMIT=fast`. Both binaries are built from the head of this branch.

## Summary

| suite | backend | tests run | matching | differing | left out | functions from rustd | left to rustc |
|---|---|---|---|---|---|---|---|
| `tests/ui` run-pass | Cranelift | 2690 | 2686 | 4 | 1114 | 5558 of 5615 | 57 |
| `tests/ui` run-pass | LLVM | 2689 | 2685 | 4 | 1115 | 5554 of 5611 | 57 |
| `coretests` | Cranelift | 1197 | 1197 | 0 | see below | 1491 of 1499 | 8 |
| `coretests` | LLVM | 1197 | 1197 | 0 | | 1491 of 1499 | 8 |
| `alloctests` | Cranelift | 703 | 703 | 0 | | 964 of 966 | 2 |
| `alloctests` | LLVM | 703 | 703 | 0 | | 964 of 966 | 2 |
| std's tests (28 binaries) | Cranelift | 252 | 252 | 0 | | 380 of 381 | 1 |
| std's tests (28 binaries) | LLVM | 252 | 252 | 0 | | 380 of 381 | 1 |

No rustd crash, hang or link error in any of the eight runs.

Where it started, with `rustd-collate` as found and the harness as first extended:
Cranelift 2650 tests run, 2633 matching, 16 differing, 1 link error; LLVM 2649 run, 17
differing, 1 link error. The library suites did not finish at first: compiling `coretests`
hung in constant evaluation, and one test hung when run.

The four `tests/ui` tests that still differ are the same four on both backends. Each has a
reduced case and a cause under "Still differing".

## Part 1: `tests/ui` run-pass

    git clone --filter=blob:none --sparse https://github.com/rust-lang/rust -b 1.99.0
    git sparse-checkout set tests/ui tests/codegen-units library
    RUSTD_BIN=<rustd> python3 harness/uitest.py <rust> --work <dir> --fresh --json ui.json

`harness/uitest.py` was there; it ran tests one at a time and left out anything with an
auxiliary crate. It now accounts for every file marked `//@ run-pass` (3760 files, 3804 with
revisions counted separately): a test is either run, once per revision, or left out with a
reason that is counted. A test that runs becomes a cargo package whose library is the test
and whose binary calls its `main`. `aux-build`, `aux-crate` and `proc-macro` files become
packages it depends on; files named by `mod`, `#[path]` and `include!` are copied beside it;
`exec-env`, `run-flags` and `edition` are honoured. rustc builds the package, rustd rebuilds
the library's functions and they are linked in front of rustc's, and the two programs must
exit with the same status and print the same on stdout and stderr.

Tests are built a workspace at a time, several workspaces at once (`--jobs`). Each
workspace's result is kept under `<work>/results`, keyed by its tests, the rustd binary and
the backend, so a stopped run picks up where it was. A whole run takes about three minutes
on this machine.

Three things the harness does so that a difference means something:

- It relinks rustc's own objects with the link line it keeps and runs that as well. One test
  (`attributes/used_with_archive.rs`) differs there already, because the kept link line lacks
  rustc's `symbols.o`; it is left out with that reason.
- It runs rustc's program twice. If the two runs differ, the test is left out as "prints
  something else in every run" (addresses, hash order, thread interleaving). This is 8 to 10
  tests depending on the run.
- When rustd's program differs, it is run six more times, and rustc's program is run over
  and over for 20 seconds to see whether it ever does what rustd's did. If it does, the test
  races and is left out with the reason "races"; if rustd's program differs once and never
  again, it is counted as "differing in one run only" and fails the run. This came from
  `threads-sendsync/yield1.rs` and `panics/panic-handler-flail-wildly.rs`, which each differed
  in one sweep and not in the others. For `yield1.rs` I saw rustc's own binary print `2` before `1` twice in
  3000 runs, but that count was taken through a pipe that closed early and on a loaded
  machine, so take it as "it happens", not as a rate. In the final runs neither test differed.

### What was left out

From the final Cranelift run (`ui-cl8b`); the LLVM run differs only in the count of tests
that print something else in every run.

| count | reason |
|---|---|
| 703 | needs a nightly feature (`#![feature]`) |
| 76 | compile flag `-Z` (nightly only) |
| 73 | no `fn main()` written at the top level (it comes from a macro or is not there) |
| 48 | an auxiliary crate needs a nightly feature |
| 47 | compile flag sets an optimisation level |
| 18 | rustc does not build it as a package |
| 12 | an auxiliary crate has compile flags of its own |
| 10 | `needs-sanitizer` |
| 9 | fails as rustc builds it in a package |
| 8 | prints something else in every run |
| 8 | `#![no_std]` |
| 8 | compile flag asks for LTO |
| 7 | names a module file that is not there |
| 7 | directive `rustc-env` |
| 6 | compile flag `--extern` |
| 6 | compile flag `--test` |
| 5 | `#![no_main]` |
| 5 | compile flag turns debug assertions or overflow checks off |
| 4 | names a file that is not there |
| 29 | not for this target (`only-windows` 7, `only-aarch64` 5, `only-32bit` 5, `only-x86` 3, `only-msvc` 2, `ignore-test` 2, five others once each) |
| 22 | other compile flags and directives, one to three tests each (`-Cllvm-args`, `-Cno-prepopulate-passes`, `-C panic=abort`, `pp-exact`, `aux-bin`, `--crate-name`, target features, `-Ccontrol-flow-guard`, `unset-rustc-env`, `--env-set`, `-Clink-args`, `--remap-path-scope`, `-lstatic`, `unused-revision-names`) |
| 1 | two auxiliary crates of one name |
| 1 | `#![crate_type]` |
| 1 | differs when relinked from rustc's objects alone |

The full list with every count is at the end of the harness's output. Some of these are the
harness's limits and not rustd's: the 73 without a top-level `main`, the 18 + 9 that do not
survive being turned into a package, and the 12 auxiliary crates with their own flags could
be run with more work on the harness. The optimisation, LTO and assertion flags (60 tests)
are left out because the harness compares against a debug build; nothing was measured about
them.

### Functions left to rustc

57 functions, the same on both backends. The harness prints them under "functions left to
rustc by reason".

| functions | reason rustd gives | tests |
|---|---|---|
| 13 | a local that cannot be reached in an `async fn` taking `ref` pattern parameters (9), and the functions that share an `async` block with those (4) | `async-await/drop-order/drop-order-for-async-fn-parameters-by-ref-binding.rs` |
| 7 | pattern matching on a union | `union/*` (4 tests), `structs-enums/align-struct.rs` |
| 4 | upcast of a `dyn` type with bound regions | `traits/trait-upcasting/*` (3), `regions/regions-early-bound-trait-param.rs` |
| 4 | calling convention `win64` on Linux | 3 tests |
| 3 | inline assembly with a `sym` operand | 3 tests |
| 1 | an `asm!` constant that is not a literal | `asm/x86_64/const.rs` |
| 3 | a body with type errors rustc does not have | `binding/pat-ranges.rs`, `coercion/trait-object-arrays-11205.rs`, `parser/macro/statement-boundaries.rs` |
| 2 | incomplete pattern (range patterns over associated constants) | 2 tests |
| 2 | incomplete expression (reserved prefixes through a macro) | 2 tests |
| 2 | `%` on floats in a constant | `consts/const_let_eq_float.rs`, `numbers-arithmetic/apfloat-modulo-wrong.rs` |
| 1 | `type_id` of a type with lifetimes in a constant | `consts/const-typeid-of-rpass.rs` |
| 7 | a name or method that does not resolve (four of them behind a proc macro) | 7 tests |
| 3 | a constant or type that fails when the function is instantiated | `const-generics/min_const_generics/macro.rs`, `impl-trait/closure-in-impl-trait*.rs` |
| 1 | a `Self` constructor in pattern position | `rfcs/rfc-2302-self-struct-ctor` |
| 2 | a static with no symbol in rustc's object (extern static, writable `CACHE`) | 2 tests |
| 1 | call to `partial_cmp` with no body and no rustc symbol | `where-clauses/obligation-error-propagation.rs` |
| 1 | missing function definition under `#[cfg]` from an attribute macro | `proc-macro/attr-cfg.rs` |

Every one of these tests still matches: the function is refused with the reason above and
rustc's copy runs. I did not reduce these to breaker cases or work on them; the differing
tests and the library suites took the time. They are the next worklist, largest first.

## Part 2: the library suites

    python3 harness/libsuite.py <rust> coretests  --work <dir> -- --json core.json
    python3 harness/libsuite.py <rust> alloctests --work <dir> -- --json alloc.json
    python3 harness/libsuite.py <rust> stdtests   --work <dir> -- --json std.json

`coretests`, `alloctests` and `library/std/tests` are written against the unstable interface
of the library: each begins with a list of `#![feature]`s that a stable compiler turns down.
`harness/libsuite.py` (new) cuts a suite down to what stable builds. It removes the feature
lines and then, until `cargo check` is quiet, deletes every item an error points into: the
test function when the error is in one, otherwise the `use`, helper or macro, whose users go
in the next round. Nothing is rewritten, so a test either runs as written or is not there.
What is deleted is counted by error code in `<work>/<suite>/pruned.json`. The package that is
left goes to `harness/cratetest.py`, which builds each test binary with rustc and again with
rustd's objects in front, runs both and compares every test by name.

| suite | `#[test]` functions in the source | build on stable | deleted, by the error that deleted the item |
|---|---|---|---|
| `coretests` | 1443 | 937 | E0658 (unstable) 622, syntax 72, unresolved after earlier deletions 92, others 3 |
| `alloctests` | 951 | 643 | E0658 252, unresolved 190, others 14 |
| std (`library/std/tests`) | 421 | 259 | E0658 199, unresolved 27, E0554 1 |

The deletion counts are items, not tests, so they do not sum to the difference. "Tests run"
in the summary is what libtest reports, which is more than the `#[test]` count for `coretests`
because macros there write many tests each.

Six `coretests` tests are left out because rustc's own unoptimised binary does not finish
them in 60 seconds (`slice::split_off_*_max_range_*`).

std's unit tests (the `tests.rs` files under `library/std/src`) are not run. They are
compiled as part of `std` itself, which cannot be built on stable. The same holds for the
unit tests inside `core` and `alloc`.

`cratetest.py` used to take a package's test targets from the top of `tests/`; it now takes
their names and paths from `cargo metadata`, which is how std went from 15 binaries to 28.

Functions left to rustc, the same on both backends:

| suite | functions | reason | reduced case |
|---|---|---|---|
| `coretests` | 5 | `Location::caller()` in a constant: rustd does not know what rustc names the file there, so it refuses | `caller_in_constant` |
| `coretests` | 2 | `type_name` of a function item and of a `dyn` type with a binder | none |
| `coretests` | 1 | layout of `PinMacroHelper<[PhantomPinned]>` (an unsized coercion inside `pin!`) | none |
| `alloctests` | 2 | an empty array compared with a slice or `VecDeque`: inference leaves the element type unknown, so there is no layout | `empty_array_by_impl` |
| std | 1 | `type_name` of a `dyn` type with a binder | none |

## What was fixed

Each row was found by a differing or crashing test, reduced to a case under
`harness/breakers/` and fixed in rustd. `python3 harness/breakers.py` runs them all.

| cause | found by | reduced case |
|---|---|---|
| A `[x; 0]` repeat did not drop `x`, and a repeated constant was treated as moved | `drop/repeat-drop.rs` | `repeat_moved` |
| Temporaries of a `let ... else` were dropped after the `else` block ran | `let-else/let-else-drop-order.rs` | `let_else_drops` |
| Before 2024, the temporaries of an `if let` scrutinee live to the end of the whole `if`; nested ones leaked | `drop/drop-order-comparisons.rs` (2021) | `if_let_outlives`, `harness/breakers/edition2021` |
| An array moved out of by a guard and given back was dropped in the order of a partly moved one | `drop/if-let-guards.rs` (2021, 2024) | `guard_let_drops` |
| Unwinding dropped named locals in the wrong order | same sweep | `tail_tuple_drops`, `repeat_moved` |
| A closure capturing a `#[non_exhaustive]` enum of its own crate captured too little | `closures/2229_closure_analysis/match/partial-move-drop-order.rs` | `non_exhaustive_capture` |
| A tuple-variant constructor cast to a pointer was taken for a discriminant cast | `mir/mir_refs_correct.rs` | `constructor_as_pointer` |
| `for<'a>` on a function pointer type was bound after its parameters, so two types had one `TypeId` | `unboxed-closures/type-id-higher-rank.rs` | `nested_binder_type_id` |
| A proc macro was handed `-1` as one literal token | `proc-macro/negative-token.rs` | `negative_literal_token`, `harness/breakers/literals` |
| An `extern` block function of a dependency was called by its Rust name, not its `link_name` | 2 link errors | `foreign_of_dependency`, `harness/breakers/foreign` |
| The methods of a `#[global_allocator]` were not entry points, so a thread-local used inside one was rustc's copy | `threads-sendsync/tls-in-global-alloc.rs` | `allocator_static` (see below) |
| Constant evaluation walked every element of `[(); usize::MAX]` | `coretests` did not compile | `repeat_moved` |
| A repeat of a zero-sized value looped once per element at run time | `coretests` `slice::test_array_windows_count` hung | `repeat_moved` |
| A reference to an empty slice at a made-up address lost the address in a constant | `coretests` | `numbered_empty_slice` |
| A constant's allocation lost its alignment, so `#[repr(align(64))]` data could land unaligned | `coretests` | `aligned_constant_field` |
| Two uses of one constant string had two addresses | `alloctests` `str::const_str_ptr` | `shared_constant_address` |
| A trait alias got an implied `Self: Alias` clause, so `<*const T as Default>::default` did not resolve through `Thin` | `coretests`, 2 functions | `null_by_default` |
| `-4` matched as `$x:literal` and followed by `.pow(2)` came out as `-(4.pow(2))` | std `num` tests, a wrong result | `negative_literal_operand`, unit test `negative_literal_stays_one_operand` |
| `&(3 + N)` with a const parameter was a stack temporary, not promoted data | `const-generics/promotion.rs` (LLVM showed it) | `promoted_const_parameter` |
| A `VaList` passed to a C function was copied by value | `abi/variadic-ffi.rs` crashed (LLVM showed it) | `list_by_address` |

`allocator_static` does not reproduce the allocator bug in the `cases` crate, because that
crate has no `main` and so every function in it is already an entry point. The fix is
covered by the UI test only.

The last two rows showed only under LLVM, but both were wrong on both backends: Cranelift
happened to leave the stack bytes alone.

### rustc sources behind the analysis changes

`AI_POLICY.md` asks for the rustc source behind changes to analysis crates. Paths are in
rust-lang/rust at `1.99.0`.

- Trait aliases have no implied `Self: Trait` clause (`crates/hir-ty/src/lower.rs`).
  `compiler/rustc_hir_analysis/src/collect/clauses_of.rs:48` adds the clause only
  `if tcx.is_trait(def_id)`, and `compiler/rustc_middle/src/ty/util.rs:635` defines `is_trait`
  as `DefKind::Trait`, which a trait alias is not.
- A negative literal is one `literal` fragment and stays one operand
  (`crates/mbe/src/expander/matcher.rs`, `transcriber.rs`).
  `compiler/rustc_parse/src/parser/nonterminal.rs:166` parses `:literal` with
  `parse_literal_maybe_minus`, and `compiler/rustc_expand/src/mbe/transcribe.rs:541` puts it
  back inside an invisible `MetaVarKind::Literal` group.
- `VaList` goes by address in C calling conventions (`crates/hir-def/src/attrs.rs`,
  `crates/hir-ty/src/mir/codegen.rs`). `core` marks the type
  `#[rustc_pass_indirectly_in_non_rustic_abis]`;
  `compiler/rustc_abi/src/layout/ty.rs:244-264` describes the flag and follows transparent
  wrappers, and `compiler/rustc_target/src/callconv/x86_64.rs`, `aarch64.rs` and
  `x86_win64.rs` make such an argument indirect. rustd now does the same for SysV x86-64,
  AAPCS64 and Windows x64. Only the first was run here.
- A const parameter in a promotable expression (`crates/hir-ty/src/mir/lower.rs`). This
  follows what `tests/ui/const-generics/promotion.rs` requires; I did not trace it to a line
  in `rustc_const_eval`'s promotion pass.

The drop-order changes follow what the named UI tests print, not a reading of rustc's drop
elaboration.

## Still differing

Four tests, on both backends. Each is marked open in `harness/breakers.py`, which reports
"43 of 43 right, 5 open" (three open, two left to rustc).

| test | reduced case | cause |
|---|---|---|
| `async-await/async-fn-size-moved-locals.rs`, `async-fn-size-uninit-locals.rs` | `future_unwind_flags` | The tests assert the size of a future. rustc shares the unwind cleanup of a local between two calls and keeps a drop flag for it, which becomes a byte of the future. rustd drops by where the call is and keeps no flag, so its future is smaller. The program is otherwise right; matching it means reproducing rustc's coroutine layout decisions. |
| `drop/issue-90752-raw-ptr-shenanigans.rs` | `raw_write_then_match` | A local known to be `None` is written through a raw pointer and then partly moved out of. rustc's drop elaboration does not see the write and leaks the rest; the test pins that. rustd drops it. |
| `macros/metavar_cross_edition_recursive_macros.rs` | `fragment_edition_through_macro` | The test is marked `known-bug` for rust-lang/rust#130484. rustc reads an `expr` fragment by the edition of the macro that wrote the name; rustd reads it by the edition where it was written, which is what the test says rustc will do when the bug is fixed. |

In all three rustd is not doing what rustc 1.99 does. In the last two, what rustc does is
something the Rust project calls a bug or an accident; I left rustd as it is and say so
here instead of copying it. The first is a real gap if anything depends on the exact size
of a future.

## Regression gate

Run from `harness/` with `RUSTD_BIN` at each binary, once per backend. All passed.

| command | Cranelift | LLVM |
|---|---|---|
| `difftest.py corpus --bin runner --crate cases --layouts` | MATCH, 0 of 893 layouts differ | MATCH, 0 of 893 layouts differ |
| `programs.py` | 22 of 22 | 22 of 22 |
| `stress.py` | 99 of 99, 0 restarts | 99 of 99, 0 restarts |
| `breakers.py` | 43 of 43, 5 open | 43 of 43, 5 open |
| `unwinds.py` | 8 of 8 | 8 of 8 |
| `splices.py` | 71 of 71 | 71 of 71 |
| `asmedits.py` | 10 of 10 | 10 of 10 |
| `includes.py` | 9 of 9 | 9 of 9 |
| `worktrees.py` | 58 of 58 | 58 of 58 |

`cargo fmt --all -- --check` is clean. `cargo test --release -p hir-ty -p hir-def -p parser
-p syntax -p rustd`: 913 passed, 0 failed.

## What is open

- The four differing tests above.
- The 57 + 11 functions left to rustc. None is wrong, all are refused with a reason, and
  only the three library reasons with a case have been reduced.
- 1114 `tests/ui` run-pass tests are not run. About 830 need nightly. Roughly 110 are the
  harness's own limits (no top-level `main`, does not build as a package, auxiliary crates
  with flags), and 60 need a release-mode comparison the harness does not do.
- The unit tests inside `core`, `alloc` and `std` are not run, because those crates cannot be
  built on stable.
- Only Linux x86-64 was run. The `VaList` change touches the AArch64 and Windows argument
  code, which was not exercised.
- Tests that race are left out, not judged. The 20-second check can miss a race that rustc's
  program shows less often than that, in which case the test is reported as differing, which
  is the safe side.
