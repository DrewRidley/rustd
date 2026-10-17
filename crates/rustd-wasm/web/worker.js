// The compiler runs here, off the page's thread: the first build takes seconds.

import { Compiler } from "./compiler.js";

// `?pack=<url>` names a packed cargo workspace to compile instead of one file over `core`.
const pack = new URL(location.href).searchParams.get("pack");
let compiler;
let compiled;
let core;
try {
  const started = performance.now();
  let wasm;
  [wasm, core] = await Promise.all(
    ["pkg/rustd_wasm.wasm", pack ?? "pkg/core.bundle"].map(async (url) => {
      const response = await fetch(url);
      if (!response.ok) {
        throw new Error(`${url}: ${response.status}`);
      }
      return new Uint8Array(await response.arrayBuffer());
    }),
  );
  const fetched = performance.now();
  compiled = await WebAssembly.compile(wasm);
  compiler = await Compiler.load(compiled, pack ? null : core);
  let analysis;
  if (pack) {
    compiler.setPack(core);
    analysis = compiler.analyze();
  }
  postMessage({
    loaded: {
      wasm_bytes: wasm.length,
      core_bytes: core.length,
      fetch_ms: fetched - started,
      start_ms: performance.now() - fetched,
      analysis,
    },
  });
} catch (error) {
  // A worker's own error reaches the page without its message.
  postMessage({ error: `the compiler failed to load: ${error.message ?? error}` });
  throw error;
}

// A `fresh` build is of another program: nothing of the one before is kept.
// `start` is where the program may put its functions and data in a module it is linked into.
onmessage = async ({ data: { id, text, fresh, start } }) => {
  const began = performance.now();
  try {
    if (fresh && !pack) {
      compiler = await Compiler.load(compiled, core);
    }
    if (start) {
      compiler.setStart(start.table, start.memory);
    }
    compiler.setFile("/app/main.rs", text);
    const { module, report } = compiler.build();
    report.build_ms = performance.now() - began;
    report.compiler_bytes = compiler.memoryBytes();
    postMessage({ id, module, report }, [module.buffer]);
  } catch (error) {
    postMessage({ id, error: String(error.message ?? error) });
  }
};
