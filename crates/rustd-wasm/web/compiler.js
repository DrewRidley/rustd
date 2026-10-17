// The compiler: `rustd_wasm.wasm` with its exports as methods.

export class Compiler {
  // `wasm` is the compiler's module, `core` the bundle of `core`'s source.
  static async load(wasm, core) {
    if (!(wasm instanceof WebAssembly.Module)) {
      wasm = await WebAssembly.compile(wasm);
    }
    let memory;
    const text = (pointer, length) =>
      new TextDecoder().decode(new Uint8Array(memory.buffer, pointer >>> 0, length >>> 0).slice());
    // What the compiler needs of the page: a clock, and somewhere to say why it panicked.
    const rustd = {
      now: () => performance.now(),
      log: (pointer, length) => console.error(text(pointer, length)),
    };
    const instance = await WebAssembly.instantiate(wasm, { rustd });
    memory = instance.exports.memory;
    const compiler = new Compiler(instance.exports);
    if (core && !compiler.rustd.rustd_add_bundle(...compiler.argument(core))) {
      throw new Error(compiler.report());
    }
    return compiler;
  }

  constructor(rustd) {
    this.rustd = rustd;
  }

  // Copies bytes or text into the compiler, which frees them once it has read them.
  argument(value) {
    const bytes = typeof value === "string" ? new TextEncoder().encode(value) : value;
    const pointer = this.rustd.rustd_alloc(bytes.length);
    new Uint8Array(this.rustd.memory.buffer, pointer >>> 0, bytes.length).set(bytes);
    return [pointer, bytes.length];
  }

  // Sets a packed cargo workspace as what there is to compile.
  setPack(pack) {
    if (!this.rustd.rustd_set_pack(...this.argument(pack))) {
      throw new Error(this.report());
    }
  }

  // Where in the module the pack is linked into the program may put its functions and data.
  setStart(tableStart, memoryStart) {
    this.rustd.rustd_set_start(tableStart, memoryStart);
  }

  analyze() {
    this.rustd.rustd_analyze();
    return this.report();
  }

  setFile(path, text) {
    this.rustd.rustd_set_file(...this.argument(path), ...this.argument(text));
  }

  report() {
    const { memory, rustd_report, rustd_report_len } = this.rustd;
    const bytes = new Uint8Array(memory.buffer, rustd_report() >>> 0, rustd_report_len());
    return new TextDecoder().decode(bytes.slice());
  }

  // Builds what changed. Gives the module to instantiate and the build's report, or throws
  // with what the compiler said.
  build() {
    if (!this.rustd.rustd_build()) {
      throw new Error(this.report());
    }
    const { memory, rustd_module, rustd_module_len } = this.rustd;
    const module = new Uint8Array(memory.buffer, rustd_module() >>> 0, rustd_module_len()).slice();
    return { module, report: JSON.parse(this.report()) };
  }

  // How much memory the compiler has.
  memoryBytes() {
    return this.rustd.memory.buffer.byteLength;
  }
}
