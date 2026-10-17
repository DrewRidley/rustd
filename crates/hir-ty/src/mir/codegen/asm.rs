//! `asm!` blocks. Cranelift has no inline assembly, so a block becomes a function of its own
//! that the function around it calls, as in rustc's Cranelift backend: the caller writes the
//! inputs to a stack slot and passes its address, and the function loads the registers from
//! it, runs the block, and writes the outputs back.

use std::{
    fmt::Write,
    process::Command,
    sync::{Arc, Mutex, OnceLock},
};

use hir_def::hir::{AsmOptions, AsmPiece, InlineAsmRegOrRegClass};
use object::{Object, ObjectSection, ObjectSymbol};
use rustc_hash::FxHashMap;
use sha2::{Digest, Sha256};

use super::{Arch, ObjectFormat, Target, store::Store};

mod aarch64;

/// The name of the function in the text of a [`Wrapper`].
const SYMBOL: &str = "rustd_asm_block";

/// A function of one instruction that Cranelift does not have: `xgetbv` reads the extended
/// control register its argument numbers.
pub(super) fn xgetbv() -> String {
    let [number, ..] = Convention::of_target().arguments;
    let number = GPR[number][1];
    format!(
        ".globl {SYMBOL}\n{SYMBOL}:\nmov ecx, {number}\nxgetbv\nshl rdx, 32\nor rax, rdx\nret\n"
    )
}

/// The hint a spin loop gives the processor between attempts.
pub(super) fn pause() -> String {
    format!(".globl {SYMBOL}\n{SYMBOL}:\npause\nret\n")
}

/// Returns its first float argument with the second left in the register it was passed in,
/// which is where a pair of floats is returned.
pub(super) fn join_floats() -> String {
    format!(".globl {SYMBOL}\n{SYMBOL}:\nret\n")
}

/// Answers what is in the two registers a pair of floats is returned in, as integers.
pub(super) fn split_floats() -> String {
    format!(".globl {SYMBOL}\n{SYMBOL}:\nmovq rax, xmm0\nmovq rdx, xmm1\nret\n")
}

/// `vcvtps2ph` makes half-precision floats of the single-precision ones at its first
/// argument and writes them to its second. The rounding is part of the instruction, so there
/// is one for each of the eight, and the third argument says which runs.
pub(super) fn vcvtps2ph(register: &str) -> String {
    let [[from, ..], [to, ..], [which, which_low, ..]] =
        Convention::of_target().arguments.map(|row| GPR[row]);
    let mut text = format!(
        ".globl {SYMBOL}\n{SYMBOL}:\nvmovups {register}, [{from}]\nand {which_low}, 7\n\
         shl {which_low}, 4\nlea rax, [rip + .Lrounding]\nadd rax, {which}\njmp rax\n\
         .p2align 4\n.Lrounding:\n"
    );
    for rounding in 0..8 {
        _ = write!(text, ".p2align 4\nvcvtps2ph xmm0, {register}, {rounding}\njmp .Lconverted\n");
    }
    _ = write!(text, ".Lconverted:\nvmovdqu [{to}], xmm0\nvzeroupper\nret\n");
    text
}

/// Calls the variadic function whose address is the last argument with the arguments before
/// it, and says that up to eight vector registers hold arguments. `ints` of those are in
/// integer registers and `stack` of them on the stack, which says where the address is.
pub(super) fn counted_variadic(ints: usize, stack: usize) -> String {
    const ARGUMENTS: [&str; 6] = ["rdi", "rsi", "rdx", "rcx", "r8", "r9"];
    let callee = match ARGUMENTS.get(ints) {
        Some(register) => format!("mov r11, {register}"),
        None => format!("mov r11, qword ptr [rsp + {}]", 8 + stack * 8),
    };
    format!(".globl {SYMBOL}\n{SYMBOL}:\n{callee}\nmov eax, 8\njmp r11\n")
}

/// One operand of a block, as the function around it sees it.
pub(super) enum Operand<'a> {
    Reg {
        reg: &'a InlineAsmRegOrRegClass,
        /// How many bytes go in, come out, or both: a register is not used for two sizes.
        size: Option<u32>,
        input: bool,
        output: bool,
    },
    Const(i128),
    /// A `label` block. The function returns which one the block jumped to, counted from 1,
    /// or 0 when the block ran off its end.
    Label,
}

/// The name in a [`Wrapper`] of the label that is operand `index`.
fn label(format: ObjectFormat, index: usize) -> String {
    format!("{}rustd_label_{index}", format.local_label())
}

/// The name in a [`Wrapper`] of where every way out of the block meets.
fn done(format: ObjectFormat) -> String {
    format!("{}rustd_done", format.local_label())
}

/// The labels among the operands, by index.
fn labels<'a>(operands: &'a [Operand<'_>]) -> impl Iterator<Item = usize> + 'a {
    operands.iter().enumerate().filter(|it| matches!(it.1, Operand::Label)).map(|it| it.0)
}

/// The text of a `#[naked]` function, which is its template and nothing else.
pub(super) fn naked(
    arch: Arch,
    template: &[AsmPiece],
    options: AsmOptions,
    operands: &[Operand<'_>],
) -> Result<String, String> {
    let att = options.contains(AsmOptions::ATT_SYNTAX) && arch == Arch::X86_64;
    let mut text = format!(".globl {SYMBOL}\n{SYMBOL}:\n");
    if att {
        text.push_str(".att_syntax\n");
    }
    for piece in template {
        match piece {
            AsmPiece::Lit(lit) => text.push_str(lit),
            AsmPiece::Operand { index, .. } => match operands.get(*index) {
                Some(Operand::Const(value)) => _ = write!(text, "{value}"),
                _ => return Err("a `naked_asm!` operand that is not a constant".to_owned()),
            },
        }
    }
    text.push('\n');
    if att {
        text.push_str(".intel_syntax noprefix\n");
    }
    match arch {
        Arch::X86_64 | Arch::Aarch64 => Ok(text),
        Arch::Wasm32 => Err("inline assembly in a wasm module".to_owned()),
    }
}

pub(super) struct Wrapper {
    /// Assembly of a function that takes the address of the slot.
    pub(super) text: String,
    pub(super) slot_size: u32,
    /// Where in the slot each operand that has a value is.
    pub(super) offsets: Vec<Option<u32>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Reg {
    /// By its row in `GPR`.
    Gpr(usize),
    /// `xmm`, `ymm` and `zmm` of one number are one register.
    Vector(usize),
    /// One a call does not keep and that holds no value here: nothing to do.
    Scratch,
}

const GPR: [[&str; 4]; 16] = [
    ["rax", "eax", "ax", "al"],
    ["rcx", "ecx", "cx", "cl"],
    ["rdx", "edx", "dx", "dl"],
    ["rsi", "esi", "si", "sil"],
    ["rdi", "edi", "di", "dil"],
    ["r8", "r8d", "r8w", "r8b"],
    ["r9", "r9d", "r9w", "r9b"],
    ["r10", "r10d", "r10w", "r10b"],
    ["r11", "r11d", "r11w", "r11b"],
    ["r12", "r12d", "r12w", "r12b"],
    ["r13", "r13d", "r13w", "r13b"],
    ["r14", "r14d", "r14w", "r14b"],
    ["r15", "r15d", "r15w", "r15b"],
    // `asm!` takes none of these as an operand: the first holds the slot here.
    ["rbx", "ebx", "bx", "bl"],
    ["rbp", "ebp", "bp", "bpl"],
    ["rsp", "esp", "sp", "spl"],
];
/// What a calling convention says of the registers a block may name.
struct Convention {
    /// The rows of `GPR` the first three arguments are in.
    arguments: [usize; 3],
    /// The rows of `GPR` a function has to leave as it found them.
    kept: &'static [usize],
    /// The vector registers a function has to leave the low sixteen bytes of as it found
    /// them.
    kept_vectors: std::ops::Range<usize>,
}

impl Convention {
    fn of_target() -> Convention {
        match super::Target::selected().map(|target| target.call_conv) {
            Ok(cranelift_codegen::isa::CallConv::WindowsFastcall) => Convention {
                arguments: [1, 2, 5],
                kept: &[3, 4, 9, 10, 11, 12],
                kept_vectors: 6..16,
            },
            _ => Convention { arguments: [4, 3, 2], kept: &[9, 10, 11, 12], kept_vectors: 0..0 },
        }
    }
}

const OPERANDS: usize = 13;
const HIGH: [&str; 3] = ["ah", "ch", "dh"];

fn named(name: &str) -> Result<Reg, String> {
    let name = name.trim_matches('"');
    if let Some(row) = GPR.iter().position(|row| row.contains(&name)) {
        if row >= OPERANDS {
            return Err(format!("inline assembly that uses `{name}`"));
        }
        return Ok(Reg::Gpr(row));
    }
    if let Some(row) = HIGH.iter().position(|&it| it == name) {
        return Ok(Reg::Gpr(row));
    }
    for prefix in ["xmm", "ymm", "zmm"] {
        if let Some(Ok(number)) = name.strip_prefix(prefix).map(str::parse::<usize>) {
            return Ok(Reg::Vector(number));
        }
    }
    let scratch = ["k", "mm", "st", "tmm"].iter().any(|prefix| {
        name.strip_prefix(prefix)
            .is_some_and(|rest| rest.trim_matches(['(', ')']).chars().all(|it| it.is_ascii_digit()))
    });
    if scratch { Ok(Reg::Scratch) } else { Err(format!("the register `{name}`")) }
}

fn vector_name(number: usize, size: u32) -> String {
    let prefix = match size {
        ..=16 => "xmm",
        32 => "ymm",
        _ => "zmm",
    };
    format!("{prefix}{number}")
}

/// The instruction that moves `size` bytes between `reg` and the slot, and the two as it
/// names them.
fn transfer(reg: Reg, size: u32, offset: u32) -> Result<(&'static str, String, String), String> {
    let width = match size {
        1 => "byte",
        2 => "word",
        4 => "dword",
        8 => "qword",
        16 => "xmmword",
        32 => "ymmword",
        64 => "zmmword",
        _ => return Err(format!("an inline assembly operand of {size} bytes")),
    };
    let memory = format!("{width} ptr [rbx + {offset}]");
    match reg {
        Reg::Gpr(row) if size <= 8 => {
            Ok(("mov", GPR[row][3 - size.trailing_zeros() as usize].to_owned(), memory))
        }
        Reg::Vector(number) if size >= 4 => {
            let instruction = match size {
                4 => "movd",
                8 => "movq",
                16 => "movups",
                _ => "vmovups",
            };
            Ok((instruction, vector_name(number, size), memory))
        }
        _ => Err(format!("an inline assembly operand of {size} bytes in such a register")),
    }
}

/// The function for a block, in the assembly of the architecture.
pub(super) fn wrapper(
    target: &Target,
    template: &[AsmPiece],
    options: AsmOptions,
    operands: &[Operand<'_>],
) -> Result<Wrapper, String> {
    if options.contains(AsmOptions::MAY_UNWIND) {
        return Err("inline assembly that unwinds".to_owned());
    }
    let format = target.object_format;
    match target.arch {
        Arch::X86_64 => wrapper_x86_64(format, template, options, operands),
        Arch::Aarch64 => aarch64::wrapper(format, template, options, operands),
        Arch::Wasm32 => Err("inline assembly in a wasm module".to_owned()),
    }
}

/// The function for a block on x86-64.
fn wrapper_x86_64(
    format: ObjectFormat,
    template: &[AsmPiece],
    options: AsmOptions,
    operands: &[Operand<'_>],
) -> Result<Wrapper, String> {
    // The registers the operands name come first: a class takes one of what is left.
    let mut regs = vec![None; operands.len()];
    for (index, operand) in operands.iter().enumerate() {
        if let Operand::Reg { reg: InlineAsmRegOrRegClass::Reg(name), .. } = operand {
            regs[index] = Some(named(name.as_str())?);
        }
    }
    let mut classes = vec![""; operands.len()];
    for (index, operand) in operands.iter().enumerate() {
        let Operand::Reg { reg: InlineAsmRegOrRegClass::RegClass(class), .. } = operand else {
            continue;
        };
        let class = class.as_str();
        classes[index] = class;
        let free = |reg: Reg| !regs.contains(&Some(reg));
        let found = match class {
            "reg" | "reg_byte" => (0..OPERANDS).map(Reg::Gpr).find(|&it| free(it)),
            "reg_abcd" => (0..3).map(Reg::Gpr).find(|&it| free(it)),
            "xmm_reg" | "ymm_reg" | "zmm_reg" => (0..16).map(Reg::Vector).find(|&it| free(it)),
            _ => return Err(format!("the register class `{class}`")),
        };
        let Some(found) = found else {
            return Err(format!("inline assembly with more `{class}` operands than registers"));
        };
        regs[index] = Some(found);
    }

    let convention = Convention::of_target();
    let kept: Vec<usize> = convention
        .kept
        .iter()
        .copied()
        .filter(|&row| regs.contains(&Some(Reg::Gpr(row))))
        .collect();
    let kept_vectors: Vec<usize> =
        convention.kept_vectors.filter(|&it| regs.contains(&Some(Reg::Vector(it)))).collect();
    let kept_at = kept_vectors.len() * 16;
    let mut slot_size = (kept_at + kept.len() * 8) as u32;
    let mut offsets = vec![None; operands.len()];
    for (index, operand) in operands.iter().enumerate() {
        let Operand::Reg { size: Some(size), .. } = *operand else { continue };
        if !size.is_power_of_two() {
            return Err(format!("an inline assembly operand of {size} bytes"));
        }
        slot_size = slot_size.next_multiple_of(size.min(16));
        offsets[index] = Some(slot_size);
        slot_size += size;
    }
    let values = || {
        operands.iter().enumerate().filter_map(|(index, operand)| match *operand {
            Operand::Reg { size: Some(size), input, output, .. } => {
                Some((regs[index]?, size, offsets[index]?, input, output))
            }
            _ => None,
        })
    };

    let slot = GPR[convention.arguments[0]][0];
    let mut text = format!(".globl {SYMBOL}\n{SYMBOL}:\npush rbx\nmov rbx, {slot}\n");
    for (at, &row) in kept.iter().enumerate() {
        _ = writeln!(text, "mov qword ptr [rbx + {}], {}", kept_at + at * 8, GPR[row][0]);
    }
    for (at, &number) in kept_vectors.iter().enumerate() {
        _ = writeln!(text, "movups xmmword ptr [rbx + {}], xmm{number}", at * 16);
    }
    for (reg, size, offset, input, _) in values() {
        if input {
            let (instruction, reg, memory) = transfer(reg, size, offset)?;
            _ = writeln!(text, "{instruction} {reg}, {memory}");
        }
    }
    let att = options.contains(AsmOptions::ATT_SYNTAX);
    if att {
        text.push_str(".att_syntax\n");
    }
    for piece in template {
        let (index, modifier) = match piece {
            AsmPiece::Lit(lit) => {
                text.push_str(lit);
                continue;
            }
            AsmPiece::Operand { index, modifier } => (*index, *modifier),
        };
        let name = match (operands.get(index), regs.get(index).copied().flatten()) {
            (Some(Operand::Const(value)), _) => {
                _ = write!(text, "{value}");
                continue;
            }
            (Some(Operand::Label), _) => {
                text.push_str(&label(format, index));
                continue;
            }
            (_, Some(Reg::Gpr(row))) => {
                let modifier = modifier.or((classes[index] == "reg_byte").then_some('l'));
                match modifier {
                    None | Some('r') => GPR[row][0],
                    Some('e') => GPR[row][1],
                    Some('x') => GPR[row][2],
                    Some('l') => GPR[row][3],
                    Some('h') if row < HIGH.len() => HIGH[row],
                    Some(other) => return Err(format!("the operand modifier `{other}`")),
                }
                .to_owned()
            }
            (_, Some(Reg::Vector(number))) => {
                let size = match (modifier, classes[index]) {
                    (Some('x'), _) | (None, "xmm_reg") => 16,
                    (Some('y'), _) | (None, "ymm_reg") => 32,
                    (Some('z'), _) | (None, _) => 64,
                    (Some(other), _) => return Err(format!("the operand modifier `{other}`")),
                };
                vector_name(number, size)
            }
            _ => return Err("an inline assembly template that names no operand".to_owned()),
        };
        if att {
            text.push('%');
        }
        text.push_str(&name);
    }
    text.push('\n');
    if att {
        text.push_str(".intel_syntax noprefix\n");
    }
    let returns = !options.contains(AsmOptions::NORETURN);
    if returns {
        for (reg, size, offset, _, output) in values() {
            if output {
                let (instruction, reg, memory) = transfer(reg, size, offset)?;
                _ = writeln!(text, "{instruction} {memory}, {reg}");
            }
        }
        if labels(operands).next().is_some() {
            text.push_str("xor eax, eax\n");
        }
    } else {
        text.push_str("ud2\n");
    }
    let mut jumped = false;
    for (at, index) in labels(operands).enumerate() {
        if returns || jumped {
            _ = writeln!(text, "jmp {}", done(format));
        }
        _ = writeln!(text, "{}:\nmov eax, {}", label(format, index), at + 1);
        jumped = true;
    }
    if returns || jumped {
        _ = writeln!(text, "{}:", done(format));
        for (at, &row) in kept.iter().enumerate() {
            _ = writeln!(text, "mov {}, qword ptr [rbx + {}]", GPR[row][0], kept_at + at * 8);
        }
        for (at, &number) in kept_vectors.iter().enumerate() {
            _ = writeln!(text, "movups xmm{number}, xmmword ptr [rbx + {}]", at * 16);
        }
        text.push_str("pop rbx\nret\n");
    }
    Ok(Wrapper { text, slot_size: slot_size.next_multiple_of(16).max(16), offsets })
}

/// The machine code of the function that `text` defines. Each text is assembled once: the
/// store keeps the code, as it does a compiled function's.
pub(super) fn assemble(triple: &str, text: &str) -> Result<Arc<[u8]>, String> {
    static ASSEMBLED: OnceLock<Mutex<FxHashMap<[u8; 32], Arc<[u8]>>>> = OnceLock::new();
    let mut hasher = Sha256::new();
    hasher.update(b"rustd asm 1");
    hasher.update(triple);
    hasher.update([0]);
    hasher.update(text);
    let key: [u8; 32] = hasher.finalize().into();
    let assembled = ASSEMBLED.get_or_init(Mutex::default);
    if let Some(code) = assembled.lock().ok().and_then(|it| it.get(&key).cloned()) {
        return Ok(code);
    }
    let store = Store::global();
    let code: Arc<[u8]> = match store.and_then(|it| it.get(&key)) {
        Some(code) => code.into(),
        None => {
            let code: Arc<[u8]> = Assembler::selected().assemble(triple, text, &key)?.into();
            if let Some(store) = store {
                store.insert(&key, &code);
            }
            code
        }
    };
    if let Ok(mut assembled) = assembled.lock() {
        assembled.insert(key, code.clone());
    }
    Ok(code)
}

/// What turns the text of a block into an object file.
#[derive(Clone, Copy)]
enum Assembler {
    /// rustc, run as a process: the one assembler sure to be there.
    Rustc,
    /// LLVM's, in this process, when LLVM is the producer of code.
    #[cfg(feature = "llvm")]
    Llvm,
}

impl Assembler {
    fn selected() -> Assembler {
        match super::Backend::from_env() {
            #[cfg(feature = "llvm")]
            Ok(super::Backend::Llvm(_)) => Assembler::Llvm,
            _ => Assembler::Rustc,
        }
    }

    fn assemble(self, triple: &str, text: &str, key: &[u8; 32]) -> Result<Vec<u8>, String> {
        match self {
            Assembler::Rustc => run_assembler(triple, text, key),
            #[cfg(feature = "llvm")]
            Assembler::Llvm => code(&super::llvm::assemble(text)?),
        }
    }
}

/// FIXME: this runs rustc, which is the one assembler sure to be there and to read what
/// `asm!` is written in. An assembler in this process would do without the files.
fn run_assembler(triple: &str, text: &str, key: &[u8; 32]) -> Result<Vec<u8>, String> {
    // Two threads may be assembling the same text.
    static RUNS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let run = RUNS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let name: String =
        key[..8].iter().map(|it| format!("{it:02x}")).chain([format!("-{run}")]).collect();
    let dir = std::env::temp_dir().join(format!("rustd-asm-{}", std::process::id()));
    let failed = |what: &str, e: &dyn std::fmt::Display| format!("inline assembly: {what}: {e}");
    std::fs::create_dir_all(&dir).map_err(|e| failed("no directory to assemble in", &e))?;
    let [assembly, source, object] = ["s", "rs", "o"].map(|it| dir.join(format!("{name}.{it}")));
    let remove = || {
        for file in [&assembly, &source, &object] {
            _ = std::fs::remove_file(file);
        }
    };
    let wrote = std::fs::write(&assembly, text).and_then(|()| {
        let source_text = format!(
            "#![no_std]\ncore::arch::global_asm!(include_str!({:?}), options(raw));\n",
            assembly.display().to_string()
        );
        std::fs::write(&source, source_text)
    });
    wrote.map_err(|e| failed("nothing written to assemble", &e))?;
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    #[allow(clippy::disallowed_methods)]
    let output = Command::new(rustc)
        .current_dir(&dir)
        .args(["--edition=2021", "--crate-type=lib", "--emit=obj", "-Cpanic=abort"])
        .args(["--crate-name=rustd_asm", "--target", triple, "-o"])
        .arg(&object)
        .arg(&source)
        .output();
    let code = match output {
        Ok(output) if output.status.success() => {
            std::fs::read(&object).map_err(|e| failed("no object", &e)).and_then(|it| code(&it))
        }
        Ok(output) => Err(failed("rustc", &String::from_utf8_lossy(&output.stderr))),
        Err(e) => Err(failed("rustc did not run", &e)),
    };
    remove();
    code
}

/// The bytes of the function in an object that defines it alone.
fn code(object: &[u8]) -> Result<Vec<u8>, String> {
    let failed = |e: &dyn std::fmt::Display| format!("inline assembly: {e}");
    let file = object::File::parse(object).map_err(|e| failed(&e))?;
    let symbol = file
        .symbols()
        .find(|it| it.name().is_ok_and(|name| name.trim_start_matches('_') == SYMBOL))
        .ok_or_else(|| failed(&"the object lacks the function"))?;
    let section = symbol
        .section_index()
        .and_then(|it| file.section_by_index(it).ok())
        .ok_or_else(|| failed(&"the function is in no section"))?;
    // FIXME: a block that names a symbol, or has data in another section.
    if section.relocations().next().is_some() {
        return Err("inline assembly that names a symbol".to_owned());
    }
    let bytes = section.data().map_err(|e| failed(&e))?;
    let start = (symbol.address() - section.address()) as usize;
    Ok(bytes[start..].to_vec())
}
