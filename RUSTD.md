# rustd: project plan and harness

rustd is a build daemon that keeps a Rust workspace loaded in rust-analyzer's database and
compiles functions straight from RA's MIR with Cranelift. During the prototype rustc still
builds everything once, and it is the reference we test against. This file says what we are aiming for, how we know whether
we are getting there, and how the work is split up.

Current numbers live in [harness/SCOREBOARD.md](harness/SCOREBOARD.md). They are generated; this
file should not repeat them.

## 1. Goal

Make the edit-to-running-code loop on a real Rust project take tens of milliseconds instead of
seconds, without ever running code that behaves differently from what rustc would have built.

The target is the Rust ecosystem in general, not any one project.

Not goals, at least for now:

- Release builds and optimised code. rustd produces debug-quality output.
- Diagnostics. Errors and borrow checking come from `cargo check` running beside the daemon.
- Hot patching. The output is a relinked binary.

Later, not now: producing `.rlib`/`.rmeta` so other crates can depend on rustd's output. Once
rustd compiles whole crates this becomes necessary.

## 2. The two rules

**Rule 1: rustc is the reference.** A function compiled by rustd must be observably the same as
rustc's version. When they differ, rustd is wrong.

**Rule 2: a skipped function is a bug.** The backend returns `unsupported` for anything it has no
explicit support for, and that function is left to rustc so the rest of the crate can still be
linked and tested. That mechanism is scaffolding. In the prototype phase we do not design around
it or count on it: the target for every crate on the ladder is 100% of functions compiled by
rustd, and the scoreboard's skip list is the bug list. Whether a production version keeps a
fallback is a later decision.

What the backend must never do is guess. Unknown input is an explicit skip (visible, counted),
not a best-effort lowering (invisible, wrong).

## 3. Metrics

Four numbers, in priority order. A change that improves a lower one while hurting a higher one
does not land.

| # | Metric | What it measures | How |
|---|---|---|---|
| 1 | Mismatches | Targets whose behaviour differs from rustc. Must be 0. | `harness/difftest.py` |
| 2 | Skips | Functions rustd did not compile, per crate, by reason. Target 0. | `harness/scoreboard.py` |
| 3 | Edit latency | Time from file save to runnable binary, split into re-analysis, codegen, link | `harness/bench.py` |
| 4 | Footprint | Daemon RSS, cold load time | same |

Coverage is counted over functions rustc emitted a symbol for (the ones rustd can replace).
Generic and `#[inline]` functions have no symbol of their own; they are compiled as instances
when something reachable calls them and are reported separately.

Latency numbers are only comparable to cargo once a crate is at 100%. Before that rustd is doing
less work than cargo. Get things working first, then optimise.

## 4. The harness

### What exists

- `harness/difftest.py <workspace> --bin <pkg> --crate <name>...`
  Builds the binary with cargo, compiles the named crates with rustd, links rustd's objects in
  front of rustc's rlibs, runs both binaries and compares stdout, stderr and exit status.
  With `--bisect` it also links each rustd function in on its own and names the ones that change
  behaviour, with the first differing output line.
- `harness/corpus/`: a lib crate with one module per backend area (scalars, control flow,
  aggregates, enums, slices, constants, generics, closures, trait objects, heap) and a runner that
  calls every function and prints the result. This is the unit-test suite for the backend.
- `harness/scoreboard.py`: runs every target in `targets.json`, writes `scoreboard.json` and
  `SCOREBOARD.md`, fails on any mismatch or on coverage going down. `--update` accepts new numbers.
- `rustd --report <tsv>` (per-function outcome), `--deny <file>` (leave these functions to rustc),
  `--isolate <dir>` (one object per function).

- `harness/cratetest.py <crate-dir>`: the same comparison using a crate's own integration tests.
  Each test binary is relinked with rustd's objects and must report the same result per test.
  `vendor.sh` unpacks a crates.io release for it.
- `harness/bench.py`: starts the daemon, flips a line back and forth, records the daemon's
  timings and RSS, then times cargo on the same edits. Writes `BENCH.md`.
- A Cranelift panic costs one function (reported as `backend panic`), not the run.

Bugs it has found so far:

- 16-byte aggregates were returned in registers; rustc returns them through a pointer. Found by
  the first real-crate run, then six corpus functions showed the same cause.
- RA lowered `<` on raw pointers to a `PartialOrd::lt` call that recursed into itself. Found by
  httparse's test suite. Fixed in RA's MIR lowering.
- The daemon rediscovered uncompilable instances over nine restart rounds on every edit. Found by
  the bench (rustd was slower than cargo on two of three crates).

### What to build next

1. **Panic and abort fixtures.** The corpus only has functions that return. Overflow, bounds
   checks, `unwrap` on `None` and explicit `panic!` need their own binaries, compared on exit
   status and panic message.
2. **Unit tests, not only integration tests.** `cratetest.py` uses `tests/*.rs`. Tests inside
   `src/` are compiled with `cfg(test)` into a different crate instance and need the symbol table
   pointed at that build.
3. **Layout agreement check.** Compare RA's layout for every type in a target against
   `rustc -Zprint-type-sizes` and count disagreements.
4. **Random programs.** Rustlantis generates programs for this kind of differential testing.
   Worth wiring in once the corpus is at 100%.
5. **CI.** `scoreboard.py` on every push to the fork.

### Target ladder

Each rung is a set of entries in `targets.json`. The bar for every rung is the same: no
mismatches, no skipped functions, the crates' own tests pass. We start the next rung when the
skip reasons on the current one are down to ones the next rung also needs.

| Rung | Targets |
|---|---|
| 0 | corpus, toy |
| 1 | small crates: semver, itoa, hex, httparse, heck |
| 2 | mid-size sync crates: regex-syntax, serde_json, toml, syn |
| 3 | a multi-crate workspace built end to end: ripgrep |
| 4 | async code: a tokio-based service |
| 5 | very large workspaces: cargo, rust-analyzer itself |

## 5. Workstreams

Each one has an owner (a subagent on the same machine), a list of scoreboard reasons it is responsible for removing, and
corpus modules it must keep green. They touch different parts of `codegen.rs` (which should be
split into a directory along these lines before parallel work starts).

| | Workstream | Removes these scoreboard reasons | Depends on |
|---|---|---|---|
| A | Harness (section 4 list) | n/a | nothing; goes first |
| B | Calls and ABI: odd-sized aggregates, fn pointers, indirect calls, closures, floats | `odd-sized aggregate`, `indirect call`, `ReifyFnPointer cast`, `closure aggregate` | A |
| C | Trait objects: vtable layout matching rustc, unsizing, dyn calls | `call through a vtable`, `unsizing` | B |
| D | Drop glue and unwinding: drop shims, cleanup blocks, landing pads | `drop glue` | B |
| E | Core intrinsics and builtin derives | `const_eval_select`, `builtin derive method`, other intrinsics | A |
| F | Constants and statics: thread locals, statics with relocations, promoted consts | `static without a rustc symbol` | A |
| G | MIR gaps in RA itself: range-pattern consts, `offset_of!`, and later async | `mir: NotSupported` | A |
| H | Daemon: file watching, cargo integration, private artifact dir, per-function reuse, several crates at once | n/a (latency) | rung 1 |
| I | Link time: faster linker, incremental relink | n/a (latency) | H |
| J | Async: coroutine state-machine transform on RA MIR | async bodies | D, G |
| K | Crate output: `.rlib`/`.rmeta` so rustd can build dependencies too | n/a | rung 2 |
| L | CAS: content-addressed store for rustc artifacts and rustd objects, remote backend | n/a (clean builds) | H |

B through G are independent of each other apart from the arrows and can run as parallel sessions
once A is in place. H and I are the path to a usable daemon. J is the largest single
item and the one most likely to need changes inside RA rather than beside it.

How a change lands, for every workstream:

1. Add or extend a corpus function that needs the feature. It shows up as skipped.
2. Implement. The function moves to compiled.
3. `scoreboard.py` passes: no mismatch anywhere, no coverage drop. `cratetest.py` on the
   affected crates still matches.
4. Commit code and the updated scoreboard together, so the history of the scoreboard is the
   history of the project.

## 6. Integration points

**rustc.** We link against its rlibs, so we must agree with it on symbol names (v0 mangling),
type layout, the unstable Rust call ABI, vtable layout and panic entry points. All of these can
change between rustc versions. rustd is pinned to one toolchain at a time, the pin is recorded
in the repo, and a toolchain bump is a scoreboard run like any other change. Layout agreement
deserves its own check: compare RA's computed layout for every type in a target against
`rustc -Zprint-type-sizes` and count disagreements. Any disagreement is a silent miscompile
waiting to happen, and today we do not measure it.

The pin is `harness/toolchain`, and it has to be the rustc that rust-analyzer's `ra-ap-rustc_abi`
was cut from. This is not hypothetical: with stable 1.99 and `ra-ap-rustc_abi` 0.166 the two
picked different niche values for `Result<(Comparator, ..), Error>` in semver, and every `?` on
it took the wrong branch.

**cargo.** The daemon needs the unit graph, the rustc command line for each crate and the final
link line. Today that is scraped from `cargo rustc -- --print link-args`. The cleaner source is
`cargo build --unit-graph` / `--build-plan` or a `RUSTC_WORKSPACE_WRAPPER` that records
invocations. The daemon must keep its own copy of the artifacts it links against: sharing
`target/` with a concurrent `cargo build` already broke one link.

**rust-analyzer.** We use `hir-ty`'s MIR, monomorphisation, layout and const-eval queries, and
`load-cargo` for workspace loading. The backend lives in `crates/hir-ty/src/mir/codegen.rs`
because it needs crate-private access; the goal is to shrink that to a small set of `pub`
additions so the backend can sit in its own crate.

**Cranelift.** Used as a library through `cranelift-object`. No changes needed so far.

**CAS.** Keys are (toolchain, crate source hash, flags, dependency keys) for rustc artifacts and
(function MIR hash, instance args, toolchain) for rustd objects. Independent of the backend
and useful on its own for clean builds.

## 7. Tracking upstream rust-analyzer

The fork is RA plus two things: the `rustd` crate and the codegen module. Policy:

- Rebase onto upstream weekly. The scoreboard is the acceptance test for the rebase.
- Keep changes to existing RA files minimal and mechanical (visibility, a module declaration).
  Count them; that number is the rebase cost and should go down.
- MIR fixes that are simply correct (workstream G) are candidates for upstreaming by a human.
  RA's AI policy does not allow us to open PRs or issues there ourselves.

## 8. Known unknowns

- Whether RA's inference and MIR are right often enough. "Lowered" is not "lowered correctly":
  one RA lowering bug has turned up already. Crate test suites are how we find the rest.
- Warm re-analysis time on a large workspace. 3 ms on a small crate says little.
- RA's memory use: 0.5 to 0.9 GB on small crates, 8 GB on a 43k-line workspace with ~690
  dependencies. That was RA loading the workspace the way an IDE does, not what rustd needs
  per crate, so measure it on a mid-size crate first. Then look at cutting it: there is a
  write-up by a compiler engineer on how RA's memory could be reduced (not yet located) that
  should be read before designing anything here.
- Unwinding. RA's MIR has no cleanup edges at all; adding them is real work inside RA's lowering.
- Debug info. None today.
