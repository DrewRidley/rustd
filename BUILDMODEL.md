# Cargo's build model in rustd

Branch `rustd-build-model`, from `rustd-collate` at 6d12f21c86. Linux x86-64, Rust 1.99.0,
LLVM 20.

This is a survey of which parts of cargo's build model rustd follows, with a small project
for each part under `harness/buildmodel/` and a script, `harness/buildmodel.py`, that builds
every one with cargo and rustc alone and again with rustd's code linked in, runs both and
compares what they print and return. Where a case has steps, one input is changed at a time
under the same daemon and each step is compared with cargo again.

## Result

Measured with

    LLVM_SYS_201_PREFIX=/usr/lib/llvm-20 cargo build -p rustd --release --features llvm
    cd harness && RUSTD_BIN=$PWD/../target/release/rustd python3 buildmodel.py --jobs 6 --table table.md

Result: 180 of 180 runs as expected, 0 WRONG. That is 90 cases under each of Cranelift and
LLVM (`RUSTD_OPT=2 RUSTD_EMIT=fast`), and the two backends agree on every row: 82 supported
and 8 refused. Of the 8, 2 are projects cargo itself turns down. The cases have 16
invalidation steps between them, run under each backend: 9 come out the same as cargo's
rebuild and 7 are refused with "start it again".

A row is one of three things. "supported": the two programs print and return the same, and
every function of the crates the case names was compiled by rustd. "refused": rustd said why
it does not build this and nothing ran differently; that includes a function it left to
rustc with a reason. "WRONG": a different output, a crash, or a refusal without a reason. The
script fails on a WRONG row and on any row that is not what its `case.json` expects, so a
feature that stops working, or a refusal that starts to pass, is seen.

## What was wrong and what was done

The first run of the suite, before any fix, had 19 WRONG rows of 87 under Cranelift. They fell
into the groups below; each says whether it was fixed or turned into a refusal.

Fixed at the root:

- Profiles. rustd had no notion of one and compiled everything as dev. It now takes
  `--release` and `--profile`, asks cargo for the unit graph of the build (`plan.rs`), and
  gives each package the `debug_assertions`, overflow checks and panic strategy cargo builds
  it with. Those are part of the crate's cfg, so of its hash in `content.rs`, and the
  overflow setting is in `cached.rs` `Entry::key`, so a dev entry is never served to a
  release build. `harness/keys.py` "the release profile" is closed and has three more
  cases (overflow checks, debug assertions, a custom profile).
- `env!` and `option_env!` of a variable of the process. They only saw what cargo and build
  scripts set. The client now sends its environment with each request, the macros read the
  crate's own variables and then those of the client, and the values a crate read are part
  of its identity. A variable nothing reads does not invalidate anything; two clients with
  different values do not share code. `keys.py` "a variable of the process read by
  option_env!" is closed.
- `RUSTFLAGS`, `CARGO_ENCODED_RUSTFLAGS`, and `[build] rustflags` / `[target.<triple>]
  rustflags` of `.cargo/config.toml`. `-C debug-assertions`, `-C overflow-checks`,
  `-C panic` and `-C opt-level` come after the profile's flags on rustc's command line and
  win; rustd ignored them. They are now read the way cargo orders them and applied over the
  profile of every package.
- `--features`, `--no-default-features`, `--all-features`, `-p` and `--workspace`. The
  features of a package are now those of the unit cargo builds, not those of `cargo
  metadata`, which unifies over the whole workspace. That is what makes resolver 1, 2 and 3
  and "one member built" against "the workspace built" come out as cargo has them.
- `CARGO_PRIMARY_PACKAGE` was never set. It is set for the packages the build was asked for.
- `CARGO_PKG_RUST_VERSION` was `1.70.0` where the manifest says `1.70`. It is now what the
  manifest has, as cargo passes it.
- `--target aarch64-unknown-linux-gnu`: `std::env::consts::ARCH` printed
  `UNRESOLVED_ENV_VAR`, because the build script of `std`, which sets `STD_ENV_ARCH`, is
  never run. The variable is now set on the sysroot's `std` from the target's cfg.
- `#![no_main]`: rustd wrote a `main` that calls `lang_start` for any crate with a function
  named `main` at its root, which is wrong for a crate that exports its own. A crate with
  `#![no_main]` gets none.
- `#![no_std]` binary that defines `rust_eh_personality` itself (the usual way on stable):
  the Cranelift backend declared the runtime's personality up front, with a signature the
  program's own function did not have, and failed. The personality is now named once every
  function is declared, and the program's own is used when it has one.
- Files a build script writes into `OUT_DIR` were taken for files that never change, so
  generated code was stale after the script ran again. They are watched now.

Turned into a refusal, with the reason rustd gives:

- `lto = true` / `"thin"` (also `-C lto` in the flags): the link failed with undefined
  symbols. Now: "`lto` is on: rustc compiles the whole program again as it links it and
  leaves no library on the link line for rustd's functions to be linked against".
- Overflow checks that differ between packages of one program (`[profile.dev.package.x]
  overflow-checks`): a generic function instantiated in one package and defined in another
  takes the setting of the one that instantiates it, and rustd's code for it is shared.
  Functions that reach across such a border are left to rustc with that reason. Doing it
  properly means keying shared instantiations on the setting, which is a larger change.
- `[target.'cfg(..)'] rustflags`: rustd does not evaluate the cfg against the target, and
  says to name the triple.
- A change, under a running daemon, of `.cargo/config.toml`, of `RUSTFLAGS`, of a
  `Cargo.toml`, or of something a build script reads (`rerun-if-changed`,
  `rerun-if-env-changed`, the script's own source) when the script then says something
  else (another cfg, another `rustc-env`, another `OUT_DIR`). Before, the daemon went on
  with what it had loaded and the program differed from cargo's. `inputs.rs` now watches
  these: when a watched thing changes it runs the build scripts again, and if what they say
  is what was loaded the build goes on (and generated files are read again); otherwise the
  request is answered with "... is not what the daemon loaded, start it again". Loading the
  checkout again in place is the real fix and is not done; the daemon already refused a
  changed `Cargo.toml` the same way.
- Doctests: rustdoc compiles them itself and never asks rustd. The row says so.

Two rows are refusals by cargo itself (`env!` of an unset variable, a `rust-version` newer
than the toolchain); the case checks that rustd turns the project down too.

## What is measured and what is inferred

Measured: the table below; the regression gate; `keys.py`.

Inferred, not measured:

- "supported" for `opt-level`, `codegen-units` and `strip` means the program behaves the
  same. rustd's own optimisation level is its own (`RUSTD_OPT`), not the profile's, and the
  suite does not compare speed or size.
- `-C target-cpu` and `-C target-feature` are supported in that `cfg(target_feature)` and
  the program's output agree with rustc. Whether rustd's backends emit the wider
  instructions was not looked at.
- The cross rows run under qemu (aarch64) and natively (musl). Other targets were not run.
- Two clients with different environments asking one daemon at the same moment is covered
  by a unit test of the protocol and by the single-client steps here, not by a test with
  two clients racing.

## Still open

- Reloading a checkout in place when its manifest, cargo configuration or build-script
  output changes, in place of asking for a restart.
- Per-package overflow checks across generic instantiations.
- LTO.
- `[target.'cfg(..)']` tables of cargo's configuration.
- Doctests.
- `Flags::of` in `plan.rs` asks `cargo config get` for the configuration, which is how
  project-model already reads it; it is an unstable cargo command.

## Regression gate

Run from `harness/` with `RUSTD_BIN` set to the release binary built with `--features llvm`,
once with `RUSTD_BACKEND=cranelift` and once with `RUSTD_BACKEND=llvm RUSTD_OPT=2
RUSTD_EMIT=fast`. The numbers were the same under both.

| command | result |
|---|---|
| `python3 difftest.py corpus --bin runner --crate cases --layouts` | MATCH, 803/803 functions from rustd, 0 of 892 layouts differ |
| `python3 programs.py` | 22 of 22 right |
| `python3 stress.py` | 99 of 99 right, 0 restarts |
| `python3 breakers.py` | 25 of 25 right, 0 open, 0 functions skipped |
| `python3 unwinds.py` | 8 of 8 right |
| `python3 splices.py` | 71 of 71 right |
| `python3 asmedits.py` | 10 of 10 right |
| `python3 includes.py` | 9 of 9 right |
| `python3 worktrees.py` | 58 of 58 right |
| `python3 keys.py` | 36 of 36 right, 0 open (was 26 with 2 open) |
| `cargo fmt --all -- --check` | clean |
| `cargo test --release -p hir-ty -p hir-def -p parser -p syntax -p rustd` | passes, 0 failed |
| `cargo test --release -p project-model -p base-db` | passes, 0 failed |

The gate ran on the tree as committed except for two later edits that change no behaviour: a
doc comment moved in `base-db`, and how `buildmodel.py` prints reasons in its table. The
binary was built again and `buildmodel.py` run again after them.

## The table

One row per case. "invalidation" is what each step of the case came to under one daemon.

| feature | case | backend | result | detail | invalidation |
|---|---|---|---|---|---|
| benches (`harness = false`, the `bench` profile) | `bench` | both | supported |  |  |
| `codegen-units = 1` | `codegen_units` | both | supported |  |  |
| `codegen-units = 1` in dev | `codegen_units_dev` | both | supported |  |  |
| `.cargo/config.toml`: `[env]` | `config_env` | both | supported |  | the value in the configuration changes: refused (.cargo/config.toml is not what the daemon loaded, start it again) |
| `.cargo/config.toml`: `[env]` under a variable of the process, and `force` | `config_env_overridden` | both | supported |  |  |
| `.cargo/config.toml`: `[target.*] linker` and `runner` | `config_linker_runner` | both | supported |  |  |
| `.cargo/config.toml`: `[build] rustflags` | `config_rustflags` | both | supported |  |  |
| `.cargo/config.toml` `[target.'cfg(..)'] rustflags` | `config_rustflags_cfg` | both | refused | rustflags of `[target.'cfg(unix)']` in cargo's configuration: rustd does not tell which targets a cfg there is of, name the triple |  |
| `.cargo/config.toml` `[target.<triple>] rustflags` | `config_rustflags_target` | both | supported |  |  |
| crate types `bin` and `lib` | `crate_bin_lib` | both | supported |  |  |
| crate type `cdylib` | `crate_cdylib` | both | supported |  |  |
| crate type `dylib` | `crate_dylib` | both | supported |  |  |
| crate types `dylib` and `rlib` together | `crate_dylib_rlib` | both | supported |  |  |
| crate type `proc-macro` (rustc's build of it is loaded; its expansion is compiled by rustd) | `crate_proc_macro` | both | supported |  |  |
| crate type `rlib` | `crate_rlib` | both | supported |  |  |
| crate type `staticlib` | `crate_staticlib` | both | supported |  |  |
| `--target aarch64-unknown-linux-gnu` | `cross_aarch64` | both | supported |  |  |
| `--target x86_64-unknown-linux-musl` | `cross_musl` | both | supported |  |  |
| `debug-assertions = false` in dev | `debug_assertions_off` | both | supported |  |  |
| `debug-assertions = true` in release | `debug_assertions_on` | both | supported |  |  |
| per-package `debug-assertions` | `debug_assertions_package` | both | supported |  |  |
| git dependency | `dep_git` | both | supported |  |  |
| `[patch]` | `dep_patch` | both | supported |  | the patched source changes: same |
| path dependency outside the workspace | `dep_path` | both | supported |  | the dependency's source changes: same |
| a workspace member that is also a registry dependency at the same version | `dep_registry_member` | both | supported |  |  |
| renamed dependency (`package = ...`) | `dep_renamed` | both | supported |  |  |
| `[replace]` | `dep_replace` | both | supported |  |  |
| vendored dependency (`[source] directory`) | `dep_vendored` | both | supported |  |  |
| doctests | `doctest` | both | refused | rustdoc compiles and runs doctests itself, a crate for each: nothing asks rustd for them, and it has no way to be asked |  |
| editions 2015, 2018, 2021 and 2024 in one graph | `editions_mixed` | both | supported |  |  |
| `CARGO_PKG_*`, `CARGO_MANIFEST_DIR`, `OUT_DIR`, `CARGO_CRATE_NAME`, `CARGO_BIN_NAME` | `env_cargo_pkg` | both | supported |  | the version in the manifest changes: refused (what a build script reads has changed and it says something else than when the daemon loaded it, start it again) |
| `env!` of a variable of the process | `env_env` | both | supported |  | the variable changes: same |
| `env!` of a variable that is not set | `env_env_unset` | both | refused | cargo turns it down too |  |
| `option_env!` of a variable of the process | `env_option_env` | both | supported |  | the variable changes: same; another variable, which nothing reads, changes: same |
| examples | `example` | both | supported |  |  |
| `--all-features` | `features_all` | both | supported |  |  |
| `[features]`: defaults, `default-features = false` on a dependency | `features_default` | both | supported |  |  |
| `--no-default-features` | `features_none` | both | supported |  |  |
| optional dependency through `dep:` | `features_optional_dep` | both | supported |  |  |
| `--features` enabling a dependency's feature (`lib/fancy`) | `features_selected` | both | supported |  |  |
| weak `opt?/loud` with the optional dependency off | `features_weak_off` | both | supported |  |  |
| weak `opt?/loud` with the optional dependency on | `features_weak_on` | both | supported |  |  |
| integration tests | `integration_test` | both | supported |  |  |
| `links` and `DEP_<name>_<key>` metadata | `links` | both | supported |  |  |
| `lto = true` | `lto_fat` | both | refused | `lto` is on: rustc compiles the whole program again as it links it and leaves no library on the link line for rustd's functions to be linked against |  |
| `lto = "off"` | `lto_off` | both | supported |  |  |
| `lto = "thin"` | `lto_thin` | both | refused | `lto` is on: rustc compiles the whole program again as it links it and leaves no library on the link line for rustd's functions to be linked against |  |
| proc macro: attribute | `macro_attribute` | both | supported |  |  |
| proc macro: derive, with a dependency of its own | `macro_derive` | both | supported |  |  |
| proc macro: function-like | `macro_function` | both | supported |  |  |
| multiple `[[bin]]` targets | `multiple_bins` | both | supported |  |  |
| `#![no_main]` with std | `no_main` | both | supported |  |  |
| `#![no_std]` binary with `#![no_main]` | `no_std_no_main` | both | supported |  |  |
| per-dependency `opt-level` (`[profile.dev.package."*"]`) | `opt_level_dependencies` | both | supported |  |  |
| `opt-level = 3` in dev | `opt_level_dev` | both | supported |  |  |
| per-package `opt-level` (`[profile.dev.package.dep]`) | `opt_level_package` | both | supported |  |  |
| `overflow-checks = false` in dev | `overflow_checks_off` | both | supported |  |  |
| `overflow-checks = true` in release | `overflow_checks_on` | both | supported |  |  |
| per-package `overflow-checks` | `overflow_checks_package` | both | refused | 1 functions left to rustc: app::main: unsupported: callee `tried`: callee `catch_unwind`: callee `catch_unwind`: callee `do_call`: callee `{closure}`: unsupported: overflow checks differ between the |  |
| `panic = "abort"` | `panic_abort` | both | supported |  |  |
| `panic = "abort"` in release | `panic_abort_release` | both | supported |  |  |
| `panic = "unwind"` (the default) | `panic_unwind` | both | supported |  |  |
| profile: custom (`--profile fast`, inherits release) | `profile_custom` | both | supported |  |  |
| profile: dev | `profile_dev` | both | supported |  |  |
| profile: release | `profile_release` | both | supported |  |  |
| per-package checks turned on in release | `release_package_checks` | both | refused | 2 functions left to rustc: app::main: unsupported: callee `tried`: callee `catch_unwind`: callee `catch_unwind`: callee `do_call`: callee `{closure}`: callee `add_generic`: callee `add`: unsupported |  |
| `rust-version` the toolchain meets | `rust_version_met` | both | supported |  |  |
| `rust-version` newer than the toolchain | `rust_version_unmet` | both | refused | cargo turns it down too |  |
| `RUSTFLAGS` with `--cfg` | `rustflags_cfg` | both | supported |  |  |
| `RUSTFLAGS=-C debug-assertions=off` over the profile | `rustflags_debug_assertions` | both | supported |  | `RUSTFLAGS` of the client is another: refused (`RUSTFLAGS` is not what the daemon loaded, start it again) |
| `RUSTFLAGS=-C opt-level=3` in dev: the checks cargo left to rustc go off | `rustflags_opt_level` | both | supported |  |  |
| `RUSTFLAGS=-C overflow-checks=off` over the profile | `rustflags_overflow_checks` | both | supported |  |  |
| `RUSTFLAGS=-C panic=abort` over the profile | `rustflags_panic_abort` | both | supported |  |  |
| `RUSTFLAGS=-C target-cpu=x86-64-v3` | `rustflags_target_cpu` | both | supported |  |  |
| `RUSTFLAGS=-C target-feature=+avx2,+fma` | `rustflags_target_feature` | both | supported |  |  |
| build script: `cargo:rustc-link-lib` and `rustc-link-search` | `script_link_lib` | both | supported |  |  |
| build script: generated code through `OUT_DIR` | `script_out_dir` | both | supported |  | `rerun-if-changed` input gains a row: same |
| build script: `cargo:rerun-if-changed` | `script_rerun_if_changed` | both | supported |  | the named file changes: same; the build script itself changes: same |
| build script: `cargo:rerun-if-env-changed` | `script_rerun_if_env_changed` | both | supported |  | the variable is set: refused (what a build script reads has changed and it says something else than when the daemon loaded it, start it again); the variable changes: refused (what a build script reads has changed and it says something else than when the daemon loaded it, start it again) |
| build script: `cargo:rustc-cfg` | `script_rustc_cfg` | both | supported |  | the file the script reads turns the cfg off: refused (what a build script reads has changed and it says something else than when the daemon loaded it, start it again); and on again: same |
| build script: `cargo:rustc-env` | `script_rustc_env` | both | supported |  | the file the script reads has another value: refused (what a build script reads has changed and it says something else than when the daemon loaded it, start it again) |
| `strip = "debuginfo"` with `debug = true` | `strip_debuginfo` | both | supported |  |  |
| `strip = true` | `strip_symbols` | both | supported |  |  |
| target-specific dependencies and `cfg(target_*)` on the host | `target_specific_deps` | both | supported |  |  |
| feature unification across members, one package built (`-p app`) | `unify_members` | both | supported |  |  |
| feature unification, resolver 1 (host and target unify) | `unify_resolver1` | both | supported |  |  |
| feature unification, resolver 2 (host apart from target) | `unify_resolver2` | both | supported |  |  |
| feature unification, resolver 3 (edition 2024) | `unify_resolver3` | both | supported |  |  |
| feature unification across members, whole workspace built | `unify_workspace` | both | supported |  |  |
| unit tests | `unit_test` | both | supported |  |  |

`buildmodel.py` is not in `.github/workflows/rustd.yaml`: the cross rows need
`aarch64-linux-gnu-gcc`, `qemu-aarch64` and `musl-gcc` and the two rustup targets, which the
workflow does not install.
