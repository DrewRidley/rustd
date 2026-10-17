# cargo rustd: the drop-in entry point

`cargo rustd build|run|test|check` builds a workspace the way `cargo build|run|test|check`
does, with the same flags, and leaves the same files in the same places. The first build of a
workspace is cargo's. After it a daemon holds the workspace, and a build after an edit is the
daemon's: the changed functions are compiled and spliced or linked into the program cargo
built. Whenever the daemon cannot do that, cargo builds, and one line says why.

This file is the design (section 1, written before the code), then how it was measured and
what is still open.

## 1. Design

### What rustd needs from cargo, and where it comes from

rustd compiles the crates of the workspace and links its objects ahead of what rustc built of
the same crates. For that it needs, of every program (a binary, an example, a test
executable):

- the rlibs and objects rustc built, to read the symbols from and to link against;
- the line rustc ran the linker with;
- the crates of the workspace that are in it.

The harness (`Fixture.rustc_build` in `harness/difftest.py`) gets these with
`cargo rustc -- --print link-args -C save-temps`, which names one program at a time and has to
be told which. A user's `cargo build` names many, by flags only cargo understands. So the
source here is `RUSTC_WORKSPACE_WRAPPER`: cargo runs the wrapper in place of rustc for every
crate of the workspace and for none of the registry's, with the real command line.

`rustd` as the wrapper (`RUSTD_CARGO_UNITS` names a directory) runs rustc as asked, plus
`--print link-args=FILE -C save-temps` when the crate is linked into an executable. When rustc
succeeds it writes a record of the unit to the directory: the arguments, and copies of what
rustc wrote that a later build would write over (the rlib of a workspace crate, the objects
and the link line of a program). The copies are what the daemon reads and links against, so
a cargo build that runs beside the daemon never changes a file under it. Registry rlibs have
the hash of their inputs in their names and are read where they are.

Cargo keeps what it builds through a workspace wrapper apart from what it builds without one
(the wrapper is in the hash of the file names), and shares the registry's crates between the
two. `cargo build` and `cargo rustd build` in one checkout do not rebuild each other's crates.

The other design considered was the one in `stub.rs` (`RUSTD_WRAPS_RUSTC`), where rustc builds
a few lines in place of each workspace crate and all of the code is rustd's. It makes the cold
build faster, but a function rustd skips has no rustc copy to fall back on, so a crate with one
skip does not link. Until the skip list is empty for the crates people build, a drop-in entry
point cannot stand on it. The wrapper here is the same hook, so moving to it later changes the
wrapper and not the command.

### The command

`cargo-rustd` is a small binary next to `rustd` that runs `rustd cargo ...`. One invocation:

1. Reads the subcommand and its flags. The flags are cargo's and go to cargo unread, except
   the few that decide whether rustd has a part at all (`--release`, `--profile`, `--target`,
   `--message-format`), and the arguments after `--`, which are the program's.
2. Fast path. `target/rustd/<key>/state.json` is what the last cargo build of the same command
   left: the programs, where cargo put each, the daemon's files for each. `<key>` is a hash of
   the subcommand, the flags, the toolchain and the parts of the environment that change what
   cargo builds. When there is a state, nothing cargo would act on but source has changed (see
   "What only cargo can build"), and the daemon is warm for it, the daemon is asked for each
   program, and each one it built is put where cargo has it. No cargo process runs.
3. Slow path, when there is no state, or the daemon is not warm, or it turned the build down.
   One line says which: `rustd: <reason>; building with cargo`. Cargo runs with the user's
   flags, the wrapper, and `--message-format=json-render-diagnostics`: progress and
   diagnostics are cargo's and rustc's own on stderr, and the executables are read from the
   JSON on stdout. Cargo's exit status is the command's when it fails. When it succeeds the
   state is written and the daemon is told to load the workspace in the background; the
   command does not wait for it.
4. `run` runs the program and `test` runs the test executables, as cargo would: the working
   directory, the environment cargo sets, cargo's `Running` lines, the exit status. Doc tests
   are rustdoc's to build, so `cargo test --doc` runs them, and the line says so.

`check` is `cargo check`: rustd has no diagnostics of its own. A subcommand rustd knows nothing
of is passed to cargo.

A build that fails to compile takes the slow path by construction: the daemon turns down source
with errors, cargo runs, and the diagnostics and the exit status are rustc's and cargo's.

### What only cargo can build

rustd falls back to cargo, with the reason on one line, for:

- a profile that optimises (`--release`, `--profile`, `opt-level` above 0 in the manifest),
  `lto`, `panic = "abort"`, instrumentation: rustd's code is debug-quality and what rustc
  inlined of a function is not replaced by rustd's copy of it;
- `RUSTFLAGS` and `-C target-cpu`/`target-feature`: the daemon loads the workspace without
  them;
- a target rustd does not emit for, or links differently from ELF (the link line is rewritten
  for ELF linkers only so far);
- a change to a manifest, `Cargo.lock`, `.cargo/config.toml`, the toolchain file;
- a change to anything that runs during the build: a build script, a proc macro, a workspace
  crate one of those depends on, a file or variable a build script said to rerun for. A build
  script that names nothing to rerun for reruns on any change to its package, so any change
  to that package is cargo's;
- a function rustd skipped whose rustc copy is from before the edit (the daemon's own
  refusal);
- libraries that are not linked into an executable by this command (`cdylib`, `staticlib`):
  nothing to splice into.

These are counted: `cargo rustd status` prints, per workspace, how many builds the daemon
served and how many went to cargo for each reason.

### The daemon

One daemon per user. It listens on `$RUSTD_HOME/daemon.sock`, where `RUSTD_HOME` is
`$XDG_RUNTIME_DIR/rustd`, or `~/.cache/rustd` without one. Requests and answers are one JSON
object a line, typed on both sides.

- Start on demand. A client that finds no daemon starts `rustd daemon` in a session of its own
  (no terminal, output to `$RUSTD_HOME/daemon.log`), and waits for the socket. The daemon holds
  a lock on `$RUSTD_HOME/daemon.sock.lock` for as long as it lives, so two clients that start one at
  the same moment get one.
- Survives its clients. It is not in the client's process group or session, and a client that
  goes away mid-request costs a failed write.
- Version. Every request carries the client's version: the crate version and a hash of the
  executable. A daemon of another version answers `restart` to a client that builds, stops its workers
  and exits, and the client starts its own; `status` from another version is answered.
- Idle. With no request for `RUSTD_IDLE_SECS` (1800 by default, read when the daemon starts;
  0 is never) the daemon stops its workers and exits. A worker nobody asked for that long is
  stopped on its own.
- `cargo rustd status` and `cargo rustd stop` ask it what it holds and ask it to exit.

The daemon itself loads nothing. It owns workers: a worker is the existing `rustd <project>
--socket` process, which holds one database. One process has one target, one toolchain, one
`cfg(test)` setting and one feature set (they are inputs of loading), so a worker is keyed by
those and by the repository. Checkouts of one repository (git worktrees, which share a
`.git`) share a worker: the second joins the first's database with the existing `--project`
request, and what the two have in common is loaded once. Different repositories get different
workers: a crash or a reload of one does not cost the other its state.

A test build has up to two workers: programs and integration tests are built without
`cfg(test)` on the libraries, unit tests with it.

### When the daemon's state can be trusted

The daemon assumes the source it loads is the source rustc compiled. The client records the
time just before cargo starts; when the worker is loaded the daemon checks that no source file
rustc read (the dep-info of every recorded unit) is newer than that, and discards the worker
if one is. A later cargo build that recompiles a workspace crate makes a new generation of
records; a worker of an older generation is restarted on the new one.

### Layout

    $RUSTD_HOME/daemon.sock, daemon.sock.lock, daemon.log
    $RUSTD_HOME/workers/<id>/sock, sock.lock, log
    $RUSTD_HOME/states/<key>.json, <key>.kept.json
    <target-dir>/<profile>/.rustd-units/<crate>-<hash>/record.json, <stamp>/<copies>
    <target-dir>/rustd/<key>/programs/<name>/rustd.o, patch.o, link.args, bin

Code: `crates/rustd/src/cargo/` (`cli` the command line, `wrapper` the rustc wrapper, `units`
the records, `script` build scripts, `state` what a workspace was last built as and when that
still holds, `plan` what a program needs from the daemon, `daemon` the per-user daemon and
its protocol, `client` the connection, `run` running programs and tests, `say` the output),
`crates/rustd/src/socket.rs` (the sockets), `crates/rustd/src/bin/cargo-rustd.rs`.

### The sockets

Both the daemon's socket and each worker's go through `socket.rs`.

- The directory. `$RUSTD_HOME` is made with mode 0700. Before use it is checked: it must be
  a directory, owned by the user, with no access for group or others. Otherwise the client
  and the daemon refuse with the reason and nothing is bound. `$XDG_RUNTIME_DIR/rustd` is
  used when `XDG_RUNTIME_DIR` is set, `~/.cache/rustd` otherwise; the same check applies to
  both and to a `RUSTD_HOME` the user sets.
- The socket is set to 0600 after `bind`, whatever the umask is.
- The peer. Every accepted connection is asked for its user (`SO_PEERCRED` on Linux,
  `getpeereid` on macOS and the BSDs) and one that is not the daemon's user is refused
  before its request is read.
- A request is one line to the daemon and two to a worker (the request, then the variables
  of the client's process), of at most 16 MiB together, and the client has 10 seconds to send it. Each
  connection is read on a thread of its own, so a client that connects and sends nothing, or
  an endless line, delays nobody else and is dropped with an error.
- One server per socket. Whoever serves a socket holds an exclusive lock on `<socket>.lock`
  for as long as it lives, and only removes a stale socket file once it holds the lock. Of
  two daemons started together one gets the lock; the other exits and its client connects to
  the winner. A worker that loses is an error to whoever started it.

Windows. The code compiles the same paths there with `uds-windows`, with the directory
check and the peer check as no-ops, and I have not run it on Windows. The equivalent there
is: keep the socket under `%LOCALAPPDATA%\rustd`, whose access control list is inherited
from the user's profile and admits the user, SYSTEM and Administrators; verify the
directory's owner SID and DACL with `GetNamedSecurityInfoW` in place of the mode check; and
for the peer, since AF_UNIX on Windows has no `SO_PEERCRED`, either rely on the directory's
ACL (what the code does today) or move the transport to a named pipe created with a security
descriptor for the user's SID and check the client with `GetNamedPipeClientProcessId` and the
token of that process. The lock file works as it is (`File::try_lock` is `LockFileEx`).
This is open work, not done.

Tests, in `socket.rs`: `the_directory_and_the_socket_are_the_users_alone`,
`a_silent_or_endless_client_is_given_up_on`, `one_process_serves_a_socket`. The peer check
is tested for the same user only; refusing another user needs a second account and is not
tested.

## 2. What was built

`cargo rustd build|run|test|check`, `cargo rustd status` and `cargo rustd stop`, as the
`cargo-rustd` binary that sits next to `rustd` and runs `rustd cargo ...`.

The first build of a workspace is cargo's, with rustd as `RUSTC_WORKSPACE_WRAPPER`. The
wrapper runs the real rustc with the same arguments and records, per workspace unit, the
arguments, the rlibs and objects, and the link line. The daemon loads the workspace in the
background while that build's program already runs. Later invocations ask the daemon, which
compiles what changed and splices or links the program; cargo is not started at all on that
path. `run` and `test` then run the same executables cargo would, with the variables cargo
sets, and print cargo's `Running` lines.

What goes to plain cargo, each with a one-line `rustd: <reason>; building with cargo` on
stderr and a count in `cargo rustd status`:

- The whole invocation: `--release` and any profile but dev, `RUSTFLAGS`, `--all-features`
  and `--no-default-features`, several `-p`, several `--target`, a target that is not ELF,
  `--message-format`, `-Z` flags, `-q`, a `RUSTC_WORKSPACE_WRAPPER` that is already set.
  `check` is always `cargo check`.
- One build: the daemon is still loading; a manifest, lock file, cargo config, toolchain
  file or a build script's input changed; a `#[test]` was added, removed or given other
  attributes (the harness's list is rustc's); a library that unit tests link as rustc built
  it changed; the source has errors (cargo runs so the diagnostics are rustc's own and the
  exit status is cargo's); the worker refused or failed.
- Doc tests are always rustdoc's, through `cargo test --doc`.

`--target <host triple>`, `-p`, `--features`, `--bin`, `--example`, `--test`, `--lib`,
`--manifest-path`, `--workspace`, `--no-run`, `--no-fail-fast`, test filters and arguments
after `--` are handled by the daemon path.

This is narrower than the task asked for in two ways. The fallback is per invocation or per
program, not per compilation unit: when one program can't be the daemon's, cargo builds the
invocation. And `--release`, custom profiles and `RUSTFLAGS` are passed through to cargo
unchanged rather than served by the daemon; they build and behave as cargo's because they
are cargo's.

Fixes outside `cargo/` that the real projects needed, all general:

- A `#[test]` function is looked up under the name rustc gives it. rustc writes a constant of
  the same name before it, so the function is the second of its name; rustd looked for the
  first, found no symbol, took the tests for dead code and then made its own copy of a
  thread-local the tests set (fd's `filter::time` test failed for that reason).
- Under `cfg(test)` the crate's own `main` is not the entry point, and a symbol that is alone
  at its path but not the first of its name is not taken for the item in the source.
- A file the VFS lists that is not UTF-8 has no text and is not watched (bat has one; the
  worker panicked on it).
- Worktrees of one repository are recognised by the `.git` file of a worktree.
- The request for a target can name several crates.

## 3. What was measured

Machine: Linux x86-64, 8 CPUs, 31 GB. Toolchain 1.99.0. Cranelift backend, `target/release/rustd`
built with `cargo build -p rustd --release` at the head of this branch, which has
`rustd-collate` b43304222b merged in.

    cd harness && TMPDIR=/dev/shm python3 cargo_rustd.py

(`TMPDIR` because the disk of this machine had 12 GB free and nushell's two target
directories need more; a run on the disk stopped there with "No space left on device".)

clones the projects at fixed tags into `~/rustd-projects` (once), copies each to a scratch
directory, and for each runs the steps with `cargo rustd` and with plain cargo in a separate
target directory, comparing the program's output and exit status and every test's result.
The edit is a line added to a function; `broken` is a type error, `mended` is the error
removed. It then runs 22 flag combinations on a small generated workspace cold, warm and
after an edit (66 comparisons), and 11 daemon lifecycle checks. The run exits 0 only when
nothing differs. The last full run passed: 0 differences on 7 projects, 66 of 66 flag steps
the same as cargo, 11 of 11 lifecycle checks. Full output: `harness/CARGO-RUSTD.md`.

Seconds, `cargo rustd` / plain cargo, one run each. Bold is a build the daemon did; `(c)` is
one cargo did in its place.

| Project | cold build | warm-up | warm build | cold test | test warm-up | warm test | edit build | edit test | broken | mended | Differences |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| ripgrep 15.1.0 | 6.00 / 6.48 | 6.87 | **0.02** / 0.44 | 2.89 / 6.13 | 3.87 | **0.95** / 1.35 | **0.34** / 0.86 | 2.25 (c) / 1.60 | 0.38 (c) / 0.20 | **0.27** / 0.55 | 0 |
| fd v10.3.0 | 21.91 / 21.98 | 0.02 | **0.03** / 0.55 | 5.90 / 5.52 | 3.02 | **0.64** / 1.05 | **0.16** / 0.51 | **0.90** / 1.03 | 0.57 (c) / 0.23 | **0.17** / 0.41 | 0 |
| bat v0.26.0 | 17.47 / 20.90 | 8.50 | **0.02** / 0.08 | 10.68 / 15.38 | 8.51 | 1.68 (c) / 1.46 | **0.30** / 0.53 | **1.95** / 1.33 | 0.38 (c) / 0.26 | **0.09** / 0.50 | 0 |
| hyperfine v1.19.0 | 5.81 / 6.79 | 5.28 | **0.03** / 0.39 | 4.73 / 4.92 | 2.77 | **0.38** / 0.82 | **0.13** / 0.34 | **0.60** / 0.75 | 0.37 (c) / 0.20 | **0.19** / 0.34 | 0 |
| just 1.43.0 | 8.89 / 10.12 | 2.50 | **0.02** / 0.06 | 8.65 / 7.62 | 8.49 | **1.54** / 1.67 | **0.11** / 0.28 | **1.34** / 1.09 | 0.22 (c) / 0.12 | **0.09** / 0.27 | 0 |
| axum axum-v0.8.6 | 9.06 / 9.20 | 3.86 | **0.02** / 0.13 | 11.44 / 10.70 | 2.23 | **0.03** / 0.13 | **0.07** / 0.40 | **0.12** / 0.97 | 0.33 (c) / 0.19 | **0.06** / 0.44 | 0 |
| nushell 0.108.0 | 44.12 / 54.73 | 42.36 | **0.04** / 0.26 | 3.25 / 2.37 | 1.12 | **0.69** / 0.72 | **0.96** / 1.65 | **0.55** / 0.50 | 1.13 (c) / 0.65 | **1.12** / 1.63 | 0 |

Reading it: the cold build is cargo's in both columns (the wrapper adds copying of
artifacts; the differences there are run-to-run noise of a single run and I would not read
them as rustd being faster). Warm-up is the time until the daemon has loaded the workspace
after the cold build, during which builds go to cargo. Edit builds by the daemon took 0.07
to 0.96 s against 0.28 to 1.65 s for cargo. `broken` always goes to cargo, by design, and
costs 0.1 to 0.5 s more than cargo alone. Three steps went to cargo for other reasons:
ripgrep's edit test (the edit was in a library its unit tests link), bat's warm test (a
build script's output is newer than the build) and the cold run of most projects (daemon
still loading).

Runs that did not pass, before the fixes above: fd failed its warm and edit test step on
every run (the `#[test]` naming bug), and bat was never served (the UTF-8 panic). Two
differences are not explained, both in just's integration tests, in ten runs of just. In
one run `delimiters::mismatched_delimiter` failed in the daemon-built test executable and
passed under cargo. In another, `directories::data_directory` passed on the `cargo rustd`
side and did not on the plain cargo side, in the cold test step, where cargo built both
executables; that run later ran out of disk and its output was lost. The other eight runs
matched. Since the second difference is between two executables cargo built, I infer that
these tests are sensitive to load on this machine rather than to who compiled them, but I
have not shown that, and the first one may still be rustd's.

Lifecycle checks (in the harness, each observed): no daemon before the first build; the
first build starts one; a later build is served by the same pid; a git worktree is loaded by
the same daemon and the same worker; a client killed mid-request leaves the daemon serving;
a client built from another source replaces the daemon and the old workers exit; with
`RUSTD_IDLE_SECS` set low the daemon and workers exit; workers exit when the daemon is
killed; the next build starts a new daemon.

### The regression gate

Run on the head of this branch (with `rustd-collate` b43304222b merged), from `harness/`,
with `RUSTD_BIN=target/release/rustd` and `TMPDIR=/dev/shm`, once
with `RUSTD_BACKEND=cranelift` (binary from `cargo build -p rustd --release`) and once with
`RUSTD_BACKEND=llvm RUSTD_OPT=2 RUSTD_EMIT=fast` (binary from
`LLVM_SYS_201_PREFIX=/usr/lib/llvm-20 cargo build -p rustd --release --features llvm`; the
daemon log says `backend: llvm -O2 fast emit`). Every script exited 0 under both, and the
counts were the same under both:

| Script | Result |
|---|---|
| `difftest.py corpus --bin runner --crate cases --layouts` | MATCH, 803 of 803 functions from rustd, 0 of 892 layouts differ |
| `programs.py` | 22 of 22 |
| `stress.py` | 99 of 99, 0 restarts |
| `breakers.py` | 25 of 25, 0 open, 0 functions skipped |
| `unwinds.py` | 8 of 8 |
| `splices.py` | 71 of 71 |
| `asmedits.py` | 10 of 10 |
| `includes.py` | 9 of 9 |
| `worktrees.py` | 58 of 58 |
| `keys.py` | 36 of 36, 0 open |
| `debuginfo.py` | all right |

`buildmodel.py --jobs 6`, which came with that merge and runs both backends itself, gives
179 of 180 as expected, 0 WRONG. The one is `cross_aarch64` under LLVM: rustd does not
finish (in one run it exited without a reason, in another it spun inside LLVM's AArch64
`InstructionSelect`). It is the same row in the CI run of `rustd-collate` at b43304222b
(run 39031377253), and this branch changes nothing under `crates/hir-ty`, so I take it to
be the base's and left it.

CI (`.github/workflows/rustd.yaml`) on this branch: green at 810746319b, which has
`rustd-collate` f66cd13c10. At 13054e50d6, with b43304222b, it fails in `debuginfo.py`
(gdb does not stop at its breakpoints on the runner, under both backends; it passes on my
machine) and in `buildmodel.py` (the row above), and passes everything else. Those are the
same two failures, line for line, as the CI run of `rustd-collate` at b43304222b itself.

Merging b43304222b changed what a worker is asked: a request is now two lines, the second
the variables of the client's process. The bounded, timed read takes both lines, and the
daemon sends its worker the variables that worker was started with, so a build through
`cargo rustd` is always for the process the worker was loaded for; a variable rustc
said a crate read (`env-dep` in its dep-info) is a guard of `cargo rustd`, so when the user
changes one the build goes to cargo.

`cargo fmt --all -- --check` is clean. `cargo test --release -p hir-ty -p hir-def -p parser
-p syntax -p rustd` passed: 929 tests, 0 failed, 1 ignored, of which 28 are rustd's.
`cargo clippy -p rustd` reports no errors; the new code has warnings of the kinds the rest
of the crate has (`eprintln!`, `Command::new`), which I left.

## 4. What is open

- Fallback is per invocation or per program, not per unit; `--release`, other profiles and
  `RUSTFLAGS` are cargo's entirely. Serving them needs the worker keyed by profile and flags
  and the optimised backend checked against rustc's optimised output.
- The two just differences seen in ten runs are unexplained.
- Windows and macOS: the entry point is written to compile there (the link plan refuses
  anything but ELF, so every build would go to cargo) and has not been built or run on
  either. The Windows socket hardening is described above and not implemented.
- The peer check is not tested against a second user.
- Projects were checked with the Cranelift backend only. The LLVM backend was run through
  the regression gate, not through `cargo_rustd.py`.
- The first build after anything cargo owns changes (manifest, lock file, build script
  input) is cargo's, and the daemon reloads afterwards; on nushell that reload is 36 to 51 s
  during which builds are cargo's.
- A function rustc emitted no symbol for is assumed dead. The test naming bug was one way
  for that assumption to be wrong; I know of no other, and have not proved there is none.
- `cargo rustd` is not in the CI workflow. The harness needs network to clone the projects
  and about 20 minutes.
