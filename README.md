# `rustd` — Sub-50ms Incremental Rust Compiler & Live Hot-Patcher

`rustd` is a persistent incremental Rust compiler built on top of `rust-analyzer`'s Salsa database and Cranelift, paired with [`wild`](https://github.com/DrewRidley/wild) and a pure-Rust in-process hot-patch runtime.

Instead of re-running `rustc` and relinking a multi-hundred-megabyte binary on every edit, `rustd` keeps your Cargo workspace loaded in memory, compiles only the function(s) you edit into a tiny `patch.o` in **~5–50 ms**, and hot-patches running binaries in-place in **< 1 ms**.

## Installation

Install [`wild`](https://github.com/DrewRidley/wild) (with Mach-O, ELF, and Wasm support enabled by default) and `rustd`:

```bash
cargo +nightly install --git https://github.com/DrewRidley/wild.git wild-linker
cargo +nightly install --git https://github.com/DrewRidley/rustd.git rustd
```

Ensure `rust-src` is installed for your workspace's toolchain (so `rustd` can read `core` / `alloc` / `std` sources):

```bash
rustup component add rust-src
```

## Quick Start (Zero-Config)

From inside any Cargo workspace (for example, `surrealdb`):

```bash
# Build and run the workspace binary (starts the background rustd watcher on first run):
rustd run -- sql --endpoint memory --ns test --db test

# Or start the background hot-patch daemon without running the binary yet:
rustd build --watch
```

Once the daemon is running:
- **Live in-process hot-patching**: While your binary is running (e.g. `rustd run -- start memory` or `rustd run -- sql`), saving any `.rs` file in **any crate in the workspace** automatically compiles the changed function(s) into `target/rustd/<bin>/patch.o` and hot-swaps them into the running process in-place without restarting or losing in-memory state.
- **Instant re-runs**: When you exit and re-run `rustd run -- ...` after editing code, `rustd` updates `patch.o` in milliseconds and launches the binary immediately without relinking the executable (avoiding macOS `syspolicyd` / Gatekeeper re-scans).
- **Revert**: Reverting a file (`git checkout -- .` or undo in your editor) automatically restores the original `rustc`-compiled instructions in-memory.

### CLI Commands

```bash
rustd run    [--manifest-path PATH] [--bin NAME] [--crate CRATE]... [--target TRIPLE] [-- ARGS...]
rustd test   [FILTER] [-p PACKAGE] [--manifest-path PATH] [-- ARGS...]
rustd build  [--manifest-path PATH] [--bin NAME] [--crate CRATE]... [--target TRIPLE] [--watch]
rustd patch  [--manifest-path PATH] [--bin NAME]
rustd sync   [--manifest-path PATH] [--bin NAME]
rustd status [--manifest-path PATH] [--bin NAME]
rustd stop   [--manifest-path PATH] [--bin NAME]
```

- `--bin <NAME>` is optional when the workspace has a default binary target (inferred automatically from `Cargo.toml`).
- `--crate <CRATE>` is optional: by default, `rustd` watches **all local workspace crates** and compiles only edited functions on demand.
- `rustd test [FILTER] [-p PACKAGE]` keeps the package's unit test binary warm and hot-patches edits to `#[test]` functions, library functions, closures, and `async` blocks in **~15–45 ms** without relinking.
- `rustd sync` incrementally links `patch.o` directly into the on-disk executable via APFS Copy-on-Write (`clonefile`), in-binary code/data caves, and incremental `LC_CODE_SIGNATURE` SHA-256 page-hash updates in **~1–10 ms** (falling back to a full `wild` link only when `patch.o` exceeds cave capacity).

## How It Works

1. **Incremental Salsa + Cranelift Codegen (`crates/hir-ty/src/mir/codegen.rs`)**:
   `rustd` loads the Cargo workspace once into `rust-analyzer`'s Salsa database and primes the active crate's HIR/trait caches in the background on startup. When a `.rs` file is saved, `rustd` checks `function_tree_serial` (covering functions, closures, and `async` blocks) and `function_mentions_adts` (automatically recompiling callers when a `struct`/`enum`/`union` layout changes) and compiles only the affected functions into `target/rustd/<bin>/patch.o`.
2. **Pure-Rust In-Process Hot-Patch Runtime (`crates/rustd/src/hotpatch_rt.rs`)**:
   During the initial `wild` link, `rustd` embeds a tiny `std`-only Rust runtime (`base.rt.o`) with a `__DATA,__mod_init_func` / `.init_array` constructor, executable code/veneer caves (`VENEER_CAVE_START` and `__rustd_veneer_cave_end`), and an O(1) in-memory `__LINKEDIT` `LC_SYMTAB` hash table.
   When `patch.o` updates:
   - The runtime maps `patch.o` into memory, resolves symbols in O(1) (~120 µs even on a 120 MB `surreal` binary with 280,000 symbols), and applies Mach-O (`aarch64`) or ELF (`x86_64`) relocations in pure Rust.
   - For each patched function, it writes a 16-byte veneer into the nearest veneer cave and patches the entry point of the original function in `__TEXT,__text` / `.text` with a single `b <veneer>` (`aarch64`) or `jmp rel32 <veneer>` (`x86_64`) instruction (or updates `<sym>$rustd_ptr` when jump-table trampolines are enabled).
3. **Sub-10ms Incremental Binary Linker (`crates/rustd/src/incremental_link.rs`)**:
   When baking `patch.o` into the executable on disk (`rustd sync`), `rustd` clones the baseline binary via `clonefile`, relocates `patch.o` into `VENEER_CAVE_START` and `PATCH_DATA_CAVE` with GOT-to-ADR/ADD relaxation, writes inline branch detours, and recomputes SHA-256 hashes only for the touched 4 KB pages in `LC_CODE_SIGNATURE`.
4. **WebAssembly (`wasm32-unknown-unknown`) Support (`crates/hir-ty/src/mir/codegen/wasm.rs`)**:
   Lowers Cranelift IR directly to relocatable `.wasm.o` files and `WebAssembly.Table` indirect trampolines (`RUSTD_WASM_TRAMPOLINES=1`) with `wild -flavor wasm --allow-multiple-definition`.

## License

Dual-licensed under [Apache-2.0](LICENSE-APACHE) and [MIT](LICENSE-MIT).
