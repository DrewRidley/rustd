#!/usr/bin/env python3
"""surreal_hotpatch.py — Instant (<10 ms) hot-patch + query/benchmark loop for SurrealDB.

Keeps a warm `rustd --watch` daemon and a persistent Bun WebAssembly worker (with
SurrealDB loaded in memory and `WebAssembly.Table` trampolines active) so edits or
worktree overlays compile to a ~800-byte `.wasm.o` patch in ~6-10 ms and hot-swap
into the live Wasm instance in ~0.2 ms.

Commands:
  surreal_hotpatch.py start [--crates surrealdb_types,...] [--worktree PATH]
  surreal_hotpatch.py query "<SurrealQL>" [--repeat N] [--relink]
  surreal_hotpatch.py apply-worktree <OTHER_WORKTREE_PATH> [--query "<SurrealQL>"] [--repeat N]
  surreal_hotpatch.py reset
  surreal_hotpatch.py status
  surreal_hotpatch.py stop
"""

import argparse
import glob
import json
import os
import shlex
import shutil
import socket
import statistics
import subprocess
import sys
import time

STATE_DIR = "/private/var/folders/fs/wg97w2sx2k730pr2ptnmqcf80000gp/T/opencode/surreal_hotpatch"
RUSTD_BIN = "/Users/drewr/Documents/Projects/rustd/target/release/rustd"
WILD_BIN = "/Users/drewr/.cargo/bin/wild"
DEFAULT_WORKTREE = "/Users/drewr/Documents/Projects/surrealdb-wt-wasm-s0"
WASM_DEPS_DIR = "/Users/drewr/Documents/Projects/size-target-6mb/codeshape-remarks/wasm32-unknown-unknown/edge/deps"
WASM_SYSROOT_DIR = "/Users/drewr/.rustup/toolchains/nightly-2026-05-11-aarch64-apple-darwin/lib/rustlib/wasm32-unknown-unknown/lib"
LINK_ARGS_TXT = "/private/var/folders/fs/wg97w2sx2k730pr2ptnmqcf80000gp/T/opencode/wasm_link_args_nolto.txt"

RUSTD_SOCK = f"{STATE_DIR}/rustd.sock"
BUN_SOCK = f"{STATE_DIR}/bun_worker.sock"
BASE_O = f"{STATE_DIR}/base.wasm.o"
PATCH_O = f"{STATE_DIR}/patch.wasm.o"
OUT_WASM = f"{STATE_DIR}/surreal_hotpatch.wasm"
HOTPATCH_MAP = f"{STATE_DIR}/hotpatch_map.json"
RUSTD_LOG = f"{STATE_DIR}/rustd.log"
BUN_LOG = f"{STATE_DIR}/bun.log"
STATE_JSON = f"{STATE_DIR}/state.json"

BUN_WORKER_JS = r"""
import { readFileSync, existsSync } from "node:fs";
import net from "node:net";

const [wasmPath, mapPath, patchPath, sockPath] = process.argv.slice(2);

let wasm = null;
let hotpatchMap = null;
let table = null;
let lastPatchMtime = 0;

function loadFullModule() {
  const t0 = performance.now();
  const wasmBytes = readFileSync(wasmPath);
  const mod = new WebAssembly.Module(wasmBytes);
  const placeholder = new Proxy({}, {
    get(_, prop) {
      if (prop.startsWith("__wbg_now") || prop.startsWith("__wbg_getTime")) return () => Date.now();
      return () => 0;
    }
  });
  const inst = new WebAssembly.Instance(mod, {
    __wbindgen_placeholder__: placeholder,
    __wbindgen_externref_xform__: {
      __wbindgen_externref_table_grow: () => 0,
      __wbindgen_externref_table_set_null: () => {},
    },
  });
  wasm = inst.exports;
  hotpatchMap = JSON.parse(readFileSync(mapPath, "utf8"));
  table = wasm.__indirect_function_table;
  return performance.now() - t0;
}

function applyPatch() {
  const t0 = performance.now();
  const patchBytes = readFileSync(patchPath);
  const env = new Proxy(wasm, {
    get(target, prop) {
      if (prop === "__linear_memory") return target.memory;
      return target[prop];
    }
  });
  const patchInst = new WebAssembly.Instance(new WebAssembly.Module(patchBytes), { env });
  const patched = [];
  for (const [name, fn] of Object.entries(patchInst.exports)) {
    const slot = hotpatchMap.table_slots[name];
    if (slot !== undefined) {
      table.set(slot, fn);
      patched.push(name);
    }
  }
  return { swap_ms: performance.now() - t0, patched_functions: patched.length, symbols: patched };
}

function runQuery(sqlStr, repeat = 1) {
  const sql = new TextEncoder().encode(sqlStr);
  const times = [];
  let statements = 0;
  let trapped = null;
  for (let i = 0; i < repeat; i++) {
    const ptr = wasm.__wbindgen_malloc(sql.length, 1);
    new Uint8Array(wasm.memory.buffer, ptr, sql.length).set(sql);
    const t0 = performance.now();
    try {
      statements = wasm.run(ptr, sql.length);
    } catch (e) {
      trapped = e.message;
      break;
    }
    times.push(performance.now() - t0);
  }
  return { statements, times_ms: times, trapped };
}

const initMs = loadFullModule();
console.log(`Loaded ${wasmPath} in ${initMs.toFixed(1)} ms (${Object.keys(hotpatchMap.table_slots).length} hot-patchable fns)`);

const server = net.createServer((conn) => {
  let buf = "";
  conn.on("data", (chunk) => {
    buf += chunk.toString();
    let nl = buf.indexOf("\n");
    if (nl !== -1) {
      const line = buf.slice(0, nl);
      buf = buf.slice(nl + 1);
      try {
        const req = JSON.parse(line);
        let res = { ok: true };
        if (req.reload_full) {
          res.reload_ms = loadFullModule();
        } else if (req.hotpatch) {
          Object.assign(res, applyPatch());
        }
        if (req.sql) {
          Object.assign(res, runQuery(req.sql, req.repeat || 1));
        }
        conn.end(JSON.stringify(res) + "\n");
      } catch (e) {
        conn.end(JSON.stringify({ ok: false, error: e.message }) + "\n");
      }
    }
  });
});

server.listen(sockPath);
"""


def send_unix_line(sock_path: str, line: str) -> str:
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.connect(sock_path)
    s.sendall((line + "\n").encode())
    chunks = []
    while True:
        data = s.recv(4096)
        if not data:
            break
        chunks.append(data)
        if b"\n" in data:
            break
    s.close()
    return b"".join(chunks).decode().strip()


def cmd_start(args):
    os.makedirs(STATE_DIR, exist_ok=True)
    cmd_stop(args, quiet=True)

    worktree = os.path.abspath(args.worktree)
    crates = [c.strip() for c in args.crates.split(",") if c.strip()]
    project_toml = os.path.join(worktree, "Cargo.toml")

    text = open(LINK_ARGS_TXT).read()
    line = next(l for l in text.splitlines() if "rust-lld" in l)
    largs = shlex.split(line)
    idx = largs.index("rust-lld")
    link_args = largs[idx:]
    first_obj = next(i for i, a in enumerate(link_args) if a.endswith(".o") or a.endswith(".rlib"))
    wild_cmd = (
        [WILD_BIN]
        + link_args[1:first_obj]
        + ["--allow-multiple-definition", PATCH_O, BASE_O]
        + link_args[first_obj:]
    )
    wild_cmd[wild_cmd.index("-o") + 1] = OUT_WASM
    link_sh = f"{STATE_DIR}/wild_link.sh"
    with open(link_sh, "w") as f:
        f.write(
            f"#!/bin/sh\nexport WILD_HOTPATCH_MAP={shlex.quote(HOTPATCH_MAP)}\nexec "
            + " ".join(shlex.quote(x) for x in wild_cmd)
            + "\n"
        )
    os.chmod(link_sh, 0o755)

    rlibs = glob.glob(WASM_DEPS_DIR + "/*.rlib") + glob.glob(WASM_SYSROOT_DIR + "/*.rlib")
    rustd_cmd = [
        RUSTD_BIN,
        project_toml,
        "--target",
        "wasm32-unknown-unknown",
    ]
    for c in crates:
        rustd_cmd.extend(["--crate", c])
    rustd_cmd.extend(
        [
            "--out",
            BASE_O,
            "--patch",
            PATCH_O,
            "--watch",
            "--socket",
            RUSTD_SOCK,
            "--link-cmd",
            link_sh,
        ]
    )
    for r in rlibs:
        rustd_cmd.extend(["--symbols", r])

    print(f"Starting rustd Wasm daemon on {worktree} (crates: {', '.join(crates)})...")
    t0 = time.perf_counter()
    rustd_log = open(RUSTD_LOG, "w")
    rustd_proc = subprocess.Popen(
        rustd_cmd,
        stdout=rustd_log,
        stderr=subprocess.STDOUT,
        start_new_session=True,
        env={
            **os.environ,
            "RUSTUP_TOOLCHAIN": "nightly-2026-05-11",
            "RUSTD_STATS": "1",
            "RUSTD_WASM_TRAMPOLINES": "1",
        },
    )

    while not os.path.exists(RUSTD_SOCK):
        if rustd_proc.poll() is not None:
            print("rustd exited early:\n" + open(RUSTD_LOG).read()[-2000:])
            sys.exit(1)
        time.sleep(0.1)

    worker_js_path = f"{STATE_DIR}/bun_worker.js"
    open(worker_js_path, "w").write(BUN_WORKER_JS)
    bun_log = open(BUN_LOG, "w")
    bun_proc = subprocess.Popen(
        ["bun", worker_js_path, OUT_WASM, HOTPATCH_MAP, PATCH_O, BUN_SOCK],
        stdout=bun_log,
        stderr=subprocess.STDOUT,
        start_new_session=True,
    )
    while not os.path.exists(BUN_SOCK):
        if bun_proc.poll() is not None:
            print("bun worker exited early:\n" + open(BUN_LOG).read()[-2000:])
            sys.exit(1)
        time.sleep(0.05)

    state = {
        "rustd_pid": rustd_proc.pid,
        "bun_pid": bun_proc.pid,
        "worktree": worktree,
        "crates": crates,
        "overlaid_files": [],
    }
    json.dump(state, open(STATE_JSON, "w"), indent=2)
    hmap = json.load(open(HOTPATCH_MAP))
    print(
        f"Ready in {time.perf_counter() - t0:.1f}s! "
        f"(rustd pid={rustd_proc.pid}, bun pid={bun_proc.pid}, "
        f"{len(hmap['table_slots']):,} hot-patchable Wasm functions)"
    )


def sync_and_query(sql: str, repeat: int = 1, relink: bool = False):
    t0 = time.perf_counter()
    req_kind = "sync" if relink else "patch"
    rustd_reply = send_unix_line(RUSTD_SOCK, req_kind)
    rustd_ms = (time.perf_counter() - t0) * 1000.0
    if rustd_reply.startswith("error:"):
        print(f"rustd refused edit ({rustd_ms:.2f} ms): {rustd_reply}")
        return

    did_patch = "patch=" in rustd_reply
    did_full_obj = "object=" in rustd_reply
    if did_full_obj and not relink:
        # Base object changed (e.g. signature/new item): run link_sh to update OUT_WASM
        subprocess.run([f"{STATE_DIR}/wild_link.sh"], check=True)

    bun_req = {
        "hotpatch": did_patch and not relink,
        "reload_full": relink or did_full_obj,
        "sql": sql,
        "repeat": repeat,
    }
    t1 = time.perf_counter()
    bun_reply = json.loads(send_unix_line(BUN_SOCK, json.dumps(bun_req)))
    bun_wall_ms = (time.perf_counter() - t1) * 1000.0

    swap_ms = bun_reply.get("swap_ms", bun_reply.get("reload_ms", 0.0))
    patched_fns = bun_reply.get("patched_functions", 0)
    print(f"rustd ({req_kind}): {rustd_ms:.2f} ms -> {rustd_reply}")
    if did_patch and not relink:
        print(
            f"wasm table hot-swap: {swap_ms:.3f} ms ({patched_fns} fn(s) updated in-place, "
            f"total edit-to-live-wasm: {rustd_ms + swap_ms:.2f} ms)"
        )
    if sql:
        times = bun_reply.get("times_ms", [])
        trapped = bun_reply.get("trapped")
        if trapped:
            print(f"query trapped: {trapped}")
        elif times:
            med = statistics.median(times)
            mn = min(times)
            mean = statistics.mean(times)
            print(
                f"query ({repeat}x): statements={bun_reply.get('statements')} | "
                f"min={mn:.3f} ms, median={med:.3f} ms, mean={mean:.3f} ms"
            )


def cmd_query(args):
    sync_and_query(args.sql, repeat=args.repeat, relink=args.relink)


def cmd_apply_worktree(args):
    state = json.load(open(STATE_JSON))
    base_wt = state["worktree"]
    other_wt = os.path.abspath(args.other_worktree)

    # Find modified/changed .rs files in `other_wt` vs `base_wt` inside `surrealdb/types` (or watched crates)
    diff_cmd = ["git", "-C", other_wt, "diff", "--name-only", "HEAD"]
    changed = subprocess.run(diff_cmd, capture_output=True, text=True).stdout.splitlines()
    # Also check diff between base_wt HEAD and other_wt HEAD for surrealdb/types
    base_head = subprocess.run(
        ["git", "-C", base_wt, "rev-parse", "HEAD"], capture_output=True, text=True
    ).stdout.strip()
    other_head = subprocess.run(
        ["git", "-C", other_wt, "rev-parse", "HEAD"], capture_output=True, text=True
    ).stdout.strip()
    if base_head != other_head:
        branch_diff = subprocess.run(
            ["git", "-C", other_wt, "diff", "--name-only", base_head, other_head, "--", "surrealdb/types"],
            capture_output=True,
            text=True,
        ).stdout.splitlines()
        changed.extend(branch_diff)

    changed = sorted(set(f for f in changed if f.endswith(".rs")))
    overlaid = set(state.get("overlaid_files", []))
    copied = []
    for rel in changed:
        src = os.path.join(other_wt, rel)
        dst = os.path.join(base_wt, rel)
        if os.path.exists(src) and os.path.exists(dst):
            if open(src, "rb").read() != open(dst, "rb").read():
                shutil.copyfile(src, dst)
                overlaid.add(rel)
                copied.append(rel)

    state["overlaid_files"] = sorted(overlaid)
    json.dump(state, open(STATE_JSON, "w"), indent=2)
    print(f"Overlaid {len(copied)} changed .rs file(s) from {other_wt}: {copied}")
    sync_and_query(args.query or "", repeat=args.repeat, relink=False)


def cmd_reset(args):
    if not os.path.exists(STATE_JSON):
        return
    state = json.load(open(STATE_JSON))
    base_wt = state["worktree"]
    subprocess.run(["git", "-C", base_wt, "checkout", "--", "."], check=True)
    state["overlaid_files"] = []
    json.dump(state, open(STATE_JSON, "w"), indent=2)
    print(f"Reset {base_wt} to clean HEAD.")
    if os.path.exists(RUSTD_SOCK) and os.path.exists(BUN_SOCK):
        sync_and_query(args.query or "", repeat=1, relink=False)


def cmd_status(args):
    if not os.path.exists(STATE_JSON):
        print("Not running.")
        return
    state = json.load(open(STATE_JSON))
    print(json.dumps(state, indent=2))


def cmd_stop(args, quiet=False):
    if os.path.exists(STATE_JSON):
        try:
            state = json.load(open(STATE_JSON))
            for k in ("rustd_pid", "bun_pid"):
                pid = state.get(k)
                if pid:
                    try:
                        os.kill(pid, 15)
                    except OSError:
                        pass
        except Exception:
            pass
    for p in (RUSTD_SOCK, BUN_SOCK):
        if os.path.exists(p):
            try:
                os.remove(p)
            except OSError:
                pass
    if not quiet:
        print("Stopped surreal_hotpatch daemons.")


def main():
    p = argparse.ArgumentParser(description="SurrealDB <10ms Wasm hot-patch + query runner")
    sub = p.add_subparsers(dest="sub", required=True)

    s_start = sub.add_parser("start")
    s_start.add_argument("--worktree", default=DEFAULT_WORKTREE)
    s_start.add_argument("--crates", default="surrealdb_types")
    s_start.set_defaults(func=cmd_start)

    s_query = sub.add_parser("query")
    s_query.add_argument("sql")
    s_query.add_argument("--repeat", type=int, default=1)
    s_query.add_argument("--relink", action="store_true")
    s_query.set_defaults(func=cmd_query)

    s_wt = sub.add_parser("apply-worktree")
    s_wt.add_argument("other_worktree")
    s_wt.add_argument("--query", default="")
    s_wt.add_argument("--repeat", type=int, default=1)
    s_wt.set_defaults(func=cmd_apply_worktree)

    s_reset = sub.add_parser("reset")
    s_reset.add_argument("--query", default="")
    s_reset.set_defaults(func=cmd_reset)

    s_status = sub.add_parser("status")
    s_status.set_defaults(func=cmd_status)

    s_stop = sub.add_parser("stop")
    s_stop.set_defaults(func=cmd_stop)

    args = p.parse_args()
    args.func(args)


if __name__ == "__main__":
    main()
