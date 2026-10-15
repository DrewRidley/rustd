//! WebAssembly (`wasm32-unknown-unknown`) backend for `rustd`.
//!
//! Translates Cranelift IR (`cranelift_codegen::ir::Function`) produced by `FunctionCx` into
//! WebAssembly bytecode and stitches compiled functions and data into a relocatable Wasm `.o`
//! object file (`linking` + `reloc.CODE` + `reloc.DATA`) that links with `wild -flavor wasm`
//! or `wasm-ld`.

use cranelift_codegen::{
    binemit::{CodeOffset, Reloc},
    ir::{
        self, Block, ExternalName, GlobalValue, GlobalValueData, InstructionData,
        JumpTableData, Opcode, StackSlot, Type, Value,
        condcodes::{FloatCC, IntCC},
        immediates::Offset32,
        types,
    },
};
use cranelift_module::{
    DataId, FuncId, Linkage, ModuleDeclarations, ModuleReloc, ModuleRelocTarget,
};
use rustc_hash::FxHashMap;

use super::{Code, CodegenError, ConstBlob, Result};

const WASM_VAL_I32: u8 = 0x7f;
const WASM_VAL_I64: u8 = 0x7e;
const WASM_VAL_F32: u8 = 0x7d;
const WASM_VAL_F64: u8 = 0x7c;
const WASM_BLOCK_EMPTY: u8 = 0x40;

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
struct WasmSig {
    params: Vec<WasmType>,
    returns: Vec<WasmType>,
}

impl WasmSig {
    fn of(sig: &ir::Signature) -> std::result::Result<Self, String> {
        let mut params = Vec::new();
        for p in &sig.params {
            if p.value_type == types::I128 {
                params.push(WasmType::I64);
                params.push(WasmType::I64);
            } else {
                params.push(WasmType::of(p.value_type)?);
            }
        }
        let mut returns = Vec::new();
        for p in &sig.returns {
            if p.value_type == types::I128 {
                returns.push(WasmType::I64);
                returns.push(WasmType::I64);
            } else {
                returns.push(WasmType::of(p.value_type)?);
            }
        }
        Ok(WasmSig { params, returns })
    }
}

static INTERNED_SIGS: std::sync::OnceLock<std::sync::Mutex<Vec<WasmSig>>> =
    std::sync::OnceLock::new();

fn intern_global_wsig(sig: &ir::Signature) -> std::result::Result<u32, String> {
    let wsig = WasmSig::of(sig)?;
    let mut list = INTERNED_SIGS.get_or_init(Default::default).lock().unwrap();
    if let Some(pos) = list.iter().position(|s| s == &wsig) {
        return Ok(pos as u32);
    }
    let idx = list.len() as u32;
    list.push(wsig);
    Ok(idx)
}

fn get_global_wsig(idx: u32) -> WasmSig {
    let list = INTERNED_SIGS.get_or_init(Default::default).lock().unwrap();
    list[idx as usize].clone()
}
const R_WASM_FUNCTION_INDEX_LEB: u8 = 0;
const R_WASM_TABLE_INDEX_SLEB: u8 = 1;
const R_WASM_TABLE_INDEX_I32: u8 = 2;
const R_WASM_MEMORY_ADDR_SLEB: u8 = 4;
const R_WASM_MEMORY_ADDR_I32: u8 = 5;
const R_WASM_TYPE_INDEX_LEB: u8 = 6;
const R_WASM_GLOBAL_INDEX_LEB: u8 = 7;

// Internal Reloc tags used inside `ModuleReloc` to carry Wasm relocation kinds.
const RELOC_FN_LEB: Reloc = Reloc::X86CallPLTRel4;
const RELOC_TABLE_SLEB: Reloc = Reloc::Abs4;
const RELOC_MEM_SLEB: Reloc = Reloc::Abs8;
const RELOC_TYPE_LEB: Reloc = Reloc::S390xTlsGdCall;
const RELOC_SP_GLOBAL_LEB: Reloc = Reloc::ElfX86_64TlsGd;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum WasmType {
    I32,
    I64,
    F32,
    F64,
}

impl WasmType {
    fn of(ty: Type) -> std::result::Result<Self, String> {
        if ty == types::I8 || ty == types::I16 || ty == types::I32 {
            Ok(WasmType::I32)
        } else if ty == types::I64 {
            Ok(WasmType::I64)
        } else if ty == types::F32 {
            Ok(WasmType::F32)
        } else if ty == types::F64 {
            Ok(WasmType::F64)
        } else {
            Err(format!("unsupported type {ty} in wasm backend"))
        }
    }

    fn byte(self) -> u8 {
        match self {
            WasmType::I32 => WASM_VAL_I32,
            WasmType::I64 => WASM_VAL_I64,
            WasmType::F32 => WASM_VAL_F32,
            WasmType::F64 => WASM_VAL_F64,
        }
    }
}

fn write_uleb(buf: &mut Vec<u8>, mut val: u64) {
    loop {
        let mut byte = (val & 0x7f) as u8;
        val >>= 7;
        if val != 0 {
            byte |= 0x80;
        }
        buf.push(byte);
        if val == 0 {
            break;
        }
    }
}

fn write_sleb(buf: &mut Vec<u8>, mut val: i64) {
    loop {
        let byte = (val & 0x7f) as u8;
        val >>= 7;
        let done = (val == 0 && (byte & 0x40) == 0) || (val == -1 && (byte & 0x40) != 0);
        if done {
            buf.push(byte);
            break;
        }
        buf.push(byte | 0x80);
    }
}

fn write_uleb_5(buf: &mut [u8], val: u32) {
    buf[0] = ((val & 0x7f) as u8) | 0x80;
    buf[1] = (((val >> 7) & 0x7f) as u8) | 0x80;
    buf[2] = (((val >> 14) & 0x7f) as u8) | 0x80;
    buf[3] = (((val >> 21) & 0x7f) as u8) | 0x80;
    buf[4] = ((val >> 28) & 0x0f) as u8;
}

fn write_sleb_5(buf: &mut [u8], val: i32) {
    let v = val as u32;
    buf[0] = ((v & 0x7f) as u8) | 0x80;
    buf[1] = (((v >> 7) & 0x7f) as u8) | 0x80;
    buf[2] = (((v >> 14) & 0x7f) as u8) | 0x80;
    buf[3] = (((v >> 21) & 0x7f) as u8) | 0x80;
    let last = ((v >> 28) & 0x0f) as u8;
    let sign = if val < 0 { 0x70 } else { 0x00 };
    buf[4] = last | sign;
}

fn write_str(buf: &mut Vec<u8>, s: &str) {
    write_uleb(buf, s.len() as u64);
    buf.extend_from_slice(s.as_bytes());
}

/// Compiles a single Cranelift IR `Function` into WebAssembly function body bytes + relocations.
pub(super) fn emit_function(func: &ir::Function) -> std::result::Result<Code, String> {
    if func.signature.returns.len() > 1 {
        return Err("multi-value return unsupported in wasm backend".to_owned());
    }
    let blocks: Vec<Block> = func.layout.blocks().collect();
    if blocks.is_empty() {
        return Err("empty function".to_owned());
    }
    let entry_block = blocks[0];
    let entry_params = func.dfg.block_params(entry_block);
    if entry_params.len() != func.signature.params.len() {
        return Err("entry block param count mismatch".to_owned());
    }

    let mut block_indices: FxHashMap<Block, u32> = FxHashMap::default();
    for (idx, &b) in blocks.iter().enumerate() {
        block_indices.insert(b, idx as u32);
    }

    // Stack frame layout for StackSlots.
    let mut slot_offsets: FxHashMap<StackSlot, u32> = FxHashMap::default();
    let mut frame_size = 0u32;
    for (slot, data) in func.sized_stack_slots.iter() {
        let align = (1u32 << data.align_shift).max(1);
        frame_size = (frame_size + align - 1) & !(align - 1);
        slot_offsets.insert(slot, frame_size);
        frame_size = frame_size.saturating_add(data.size);
    }
    if !func.sized_stack_slots.is_empty() && frame_size == 0 {
        frame_size = 16;
    }
    frame_size = (frame_size + 15) & !15;

    // Assign Wasm local indices to all SSA values.
    // An `I128` value occupies two consecutive `I64` locals `(lo, lo + 1)`.
    let mut value_locals: FxHashMap<Value, u32> = FxHashMap::default();
    let mut local_types: Vec<WasmType> = Vec::new();
    let mut num_params = 0u32;

    for &val in entry_params {
        let ty = func.dfg.value_type(val);
        value_locals.insert(val, num_params);
        if ty == types::I128 {
            num_params += 2;
        } else {
            let _ = WasmType::of(ty)?;
            num_params += 1;
        }
    }

    let mut alloc_local = |wty: WasmType| -> u32 {
        let idx = num_params + local_types.len() as u32;
        local_types.push(wty);
        idx
    };

    for &b in &blocks {
        if b != entry_block {
            for &val in func.dfg.block_params(b) {
                let ty = func.dfg.value_type(val);
                if ty == types::I128 {
                    let lo = alloc_local(WasmType::I64);
                    let _hi = alloc_local(WasmType::I64);
                    value_locals.insert(val, lo);
                } else {
                    let wty = WasmType::of(ty)?;
                    let l = alloc_local(wty);
                    value_locals.insert(val, l);
                }
            }
        }
        let mut inst_opt = func.layout.first_inst(b);
        while let Some(inst) = inst_opt {
            for &res in func.dfg.inst_results(inst) {
                let ty = func.dfg.value_type(res);
                if ty == types::I128 {
                    let lo = alloc_local(WasmType::I64);
                    let _hi = alloc_local(WasmType::I64);
                    value_locals.insert(res, lo);
                } else {
                    let wty = WasmType::of(ty)?;
                    let l = alloc_local(wty);
                    value_locals.insert(res, l);
                }
            }
            inst_opt = func.layout.next_inst(inst);
        }
    }

    let needs_cfg = blocks.len() > 1
        || func
            .layout
            .last_inst(blocks[0])
            .is_some_and(|i| func.dfg.insts[i].opcode().is_branch());
    let fp_local = if frame_size > 0 { Some(alloc_local(WasmType::I32)) } else { None };
    let bb_local = if needs_cfg { Some(alloc_local(WasmType::I32)) } else { None };

    // Encode locals header (run-length encoded).
    let mut body = Vec::new();
    let mut runs: Vec<(u32, WasmType)> = Vec::new();
    for &wty in &local_types {
        if let Some(last) = runs.last_mut()
            && last.1 == wty
        {
            last.0 += 1;
        } else {
            runs.push((1, wty));
        }
    }
    write_uleb(&mut body, runs.len() as u64);
    for (count, wty) in runs {
        write_uleb(&mut body, u64::from(count));
        body.push(wty.byte());
    }

    let mut relocs: Vec<ModuleReloc> = Vec::new();

    // Prologue: allocate shadow stack frame if needed.
    if let Some(fp) = fp_local {
        // global.get __stack_pointer
        body.push(0x23);
        relocs.push(ModuleReloc {
            kind: RELOC_SP_GLOBAL_LEB,
            offset: body.len() as CodeOffset,
            name: ModuleRelocTarget::user(2, 0),
            addend: 0,
        });
        body.extend_from_slice(&[0x80, 0x80, 0x80, 0x80, 0x00]);
        // i32.const frame_size
        body.push(0x41);
        write_sleb(&mut body, i64::from(frame_size));
        // i32.sub
        body.push(0x6b);
        // local.tee $fp
        body.push(0x22);
        write_uleb(&mut body, u64::from(fp));
        // global.set __stack_pointer
        body.push(0x24);
        relocs.push(ModuleReloc {
            kind: RELOC_SP_GLOBAL_LEB,
            offset: body.len() as CodeOffset,
            name: ModuleRelocTarget::user(2, 0),
            addend: 0,
        });
        body.extend_from_slice(&[0x80, 0x80, 0x80, 0x80, 0x00]);
    }

    let mut emitter = WasmFuncEmitter {
        func,
        body: &mut body,
        relocs: &mut relocs,
        value_locals: &value_locals,
        slot_offsets: &slot_offsets,
        block_indices: &block_indices,
        fp_local,
        frame_size,
        bb_local,
        num_blocks: blocks.len() as u32,
    };

    if !needs_cfg {
        emitter.emit_block(blocks[0], 0)?;
    } else {
        let n = blocks.len();
        // loop $dispatch (blocktype empty)
        emitter.body.push(0x03);
        emitter.body.push(WASM_BLOCK_EMPTY);
        for _ in 0..n {
            emitter.body.push(0x02);
            emitter.body.push(WASM_BLOCK_EMPTY);
        }
        // local.get $bb
        emitter.body.push(0x20);
        write_uleb(emitter.body, u64::from(bb_local.unwrap()));
        // br_table 0 1 2 ... (n-1) (n-1)
        emitter.body.push(0x0e);
        write_uleb(emitter.body, n as u64);
        for i in 0..n {
            write_uleb(emitter.body, i as u64);
        }
        write_uleb(emitter.body, (n - 1) as u64);

        for (k, &block) in blocks.iter().enumerate() {
            // end of $b_k
            emitter.body.push(0x0b);
            emitter.emit_block(block, k as u32)?;
        }
        // end of loop $dispatch
        emitter.body.push(0x0b);
        // unreachable
        emitter.body.push(0x00);
    }

    // End of function body.
    body.push(0x0b);

    Ok(Code {
        signature: func.signature.clone(),
        bytes: body,
        align: 1,
        relocs,
        unwind: None,
    })
}

struct WasmFuncEmitter<'a> {
    func: &'a ir::Function,
    body: &'a mut Vec<u8>,
    relocs: &'a mut Vec<ModuleReloc>,
    value_locals: &'a FxHashMap<Value, u32>,
    slot_offsets: &'a FxHashMap<StackSlot, u32>,
    block_indices: &'a FxHashMap<Block, u32>,
    fp_local: Option<u32>,
    frame_size: u32,
    bb_local: Option<u32>,
    num_blocks: u32,
}

impl<'a> WasmFuncEmitter<'a> {
    fn resolve_val(&self, v: Value) -> Value {
        self.func.dfg.resolve_aliases(v)
    }

    fn val_local(&self, v: Value) -> std::result::Result<u32, String> {
        let v = self.resolve_val(v);
        self.value_locals
            .get(&v)
            .copied()
            .ok_or_else(|| format!("missing wasm local for value {v}"))
    }

    fn get_val(&mut self, v: Value) -> std::result::Result<(), String> {
        let l = self.val_local(v)?;
        self.body.push(0x20); // local.get
        write_uleb(self.body, u64::from(l));
        Ok(())
    }

    fn get_val_hi(&mut self, v: Value) -> std::result::Result<(), String> {
        let l = self.val_local(v)? + 1;
        self.body.push(0x20); // local.get
        write_uleb(self.body, u64::from(l));
        Ok(())
    }

    fn get_val_unsigned(&mut self, v: Value) -> std::result::Result<(), String> {
        let v = self.resolve_val(v);
        let ty = self.func.dfg.value_type(v);
        self.get_val(v)?;
        if ty == types::I8 {
            self.body.push(0x41);
            write_sleb(self.body, 0xff);
            self.body.push(0x71); // i32.and
        } else if ty == types::I16 {
            self.body.push(0x41);
            write_sleb(self.body, 0xffff);
            self.body.push(0x71); // i32.and
        }
        Ok(())
    }

    fn get_val_signed(&mut self, v: Value) -> std::result::Result<(), String> {
        let v = self.resolve_val(v);
        let ty = self.func.dfg.value_type(v);
        self.get_val(v)?;
        if ty == types::I8 {
            self.body.push(0xc0); // i32.extend8_s
        } else if ty == types::I16 {
            self.body.push(0xc1); // i32.extend16_s
        }
        Ok(())
    }

    fn set_val(&mut self, v: Value) -> std::result::Result<(), String> {
        let l = self.val_local(v)?;
        self.body.push(0x21); // local.set
        write_uleb(self.body, u64::from(l));
        Ok(())
    }

    fn set_val_hi(&mut self, v: Value) -> std::result::Result<(), String> {
        let l = self.val_local(v)? + 1;
        self.body.push(0x21); // local.set
        write_uleb(self.body, u64::from(l));
        Ok(())
    }

    fn resolve_gv(&self, gv: GlobalValue) -> std::result::Result<(ModuleRelocTarget, i64), String> {
        match self.func.global_values[gv] {
            GlobalValueData::Symbol { ref name, offset, .. } => {
                let target = self.reloc_target_from_ext_name(name)?;
                Ok((target, offset.into()))
            }
            GlobalValueData::IAddImm { base, offset, .. } => {
                let (target, base_off) = self.resolve_gv(base)?;
                let off: i64 = offset.into();
                Ok((target, base_off + off))
            }
            ref other => Err(format!("unsupported GlobalValue {other:?} in wasm backend")),
        }
    }

    fn reloc_target_from_ext_name(
        &self,
        name: &ExternalName,
    ) -> std::result::Result<ModuleRelocTarget, String> {
        match *name {
            ExternalName::User(user_ref) => {
                let u = &self.func.params.user_named_funcs()[user_ref];
                Ok(ModuleRelocTarget::user(u.namespace, u.index))
            }
            ExternalName::LibCall(lc) => Ok(ModuleRelocTarget::LibCall(lc)),
            ref other => Err(format!("unsupported ExternalName {other:?} in wasm backend")),
        }
    }

    fn emit_jump_to(
        &mut self,
        target_block: Block,
        args: &[Value],
        current_block_idx: u32,
        extra_depth: u32,
    ) -> std::result::Result<(), String> {
        for &arg in args {
            let ty = self.func.dfg.value_type(self.resolve_val(arg));
            self.get_val(arg)?;
            if ty == types::I128 {
                self.get_val_hi(arg)?;
            }
        }
        let target_params = self.func.dfg.block_params(target_block);
        if args.len() != target_params.len() {
            return Err("branch arg count mismatch".to_owned());
        }
        for &param in target_params.iter().rev() {
            let ty = self.func.dfg.value_type(self.resolve_val(param));
            if ty == types::I128 {
                self.set_val_hi(param)?;
            }
            self.set_val(param)?;
        }
        let target_idx = self.block_indices[&target_block];
        self.body.push(0x41); // i32.const
        write_sleb(self.body, i64::from(target_idx));
        self.body.push(0x21); // local.set $bb
        write_uleb(self.body, u64::from(self.bb_local.unwrap()));
        // Branch to loop $dispatch.
        let depth = (self.num_blocks - 1 - current_block_idx) + extra_depth;
        self.body.push(0x0c); // br
        write_uleb(self.body, u64::from(depth));
        Ok(())
    }

    fn emit_block(&mut self, block: Block, block_idx: u32) -> std::result::Result<(), String> {
        let mut inst_opt = self.func.layout.first_inst(block);
        while let Some(inst) = inst_opt {
            self.emit_inst(inst, block_idx)?;
            inst_opt = self.func.layout.next_inst(inst);
        }
        Ok(())
    }

    fn emit_load(
        &mut self,
        addr: Value,
        offset: Offset32,
        res: Value,
        opcode: Opcode,
    ) -> std::result::Result<(), String> {
        let off: i64 = offset.into();
        let (base_off, extra_add) = if off >= 0 && off <= i64::from(u32::MAX - 8) {
            (off as u32, None)
        } else {
            (0u32, Some(off as i32))
        };
        let res_ty = self.func.dfg.value_type(res);
        if res_ty == types::I128 {
            self.get_val(addr)?;
            if let Some(delta) = extra_add {
                self.body.push(0x41);
                write_sleb(self.body, i64::from(delta));
                self.body.push(0x6a); // i32.add
            }
            self.body.push(0x29); // i64.load
            write_uleb(self.body, 0);
            write_uleb(self.body, u64::from(base_off));
            self.set_val(res)?;

            self.get_val(addr)?;
            if let Some(delta) = extra_add {
                self.body.push(0x41);
                write_sleb(self.body, i64::from(delta));
                self.body.push(0x6a); // i32.add
            }
            self.body.push(0x29); // i64.load
            write_uleb(self.body, 0);
            write_uleb(self.body, u64::from(base_off + 8));
            return self.set_val_hi(res);
        }
        self.get_val(addr)?;
        if let Some(delta) = extra_add {
            self.body.push(0x41);
            write_sleb(self.body, i64::from(delta));
            self.body.push(0x6a); // i32.add
        }
        let op = match opcode {
            Opcode::Load | Opcode::AtomicLoad => match res_ty {
                types::I8 => 0x2d,  // i32.load8_u
                types::I16 => 0x2f, // i32.load16_u
                types::I32 => 0x28, // i32.load
                types::I64 => 0x29, // i64.load
                types::F32 => 0x2a, // f32.load
                types::F64 => 0x2b, // f64.load
                _ => return Err(format!("unsupported load type {res_ty}")),
            },
            Opcode::Uload8 => {
                if res_ty == types::I64 {
                    0x31
                } else {
                    0x2d
                }
            }
            Opcode::Sload8 => {
                if res_ty == types::I64 {
                    0x30
                } else {
                    0x2c
                }
            }
            Opcode::Uload16 => {
                if res_ty == types::I64 {
                    0x33
                } else {
                    0x2f
                }
            }
            Opcode::Sload16 => {
                if res_ty == types::I64 {
                    0x32
                } else {
                    0x2e
                }
            }
            Opcode::Uload32 => 0x35, // i64.load32_u
            Opcode::Sload32 => 0x34, // i64.load32_s
            _ => return Err(format!("unexpected load opcode {opcode:?}")),
        };
        self.body.push(op);
        write_uleb(self.body, 0); // align = 0 (byte-aligned safe)
        write_uleb(self.body, u64::from(base_off));
        self.set_val(res)
    }

    fn emit_store(
        &mut self,
        addr: Value,
        val: Value,
        offset: Offset32,
        opcode: Opcode,
    ) -> std::result::Result<(), String> {
        let off: i64 = offset.into();
        let (base_off, extra_add) = if off >= 0 && off <= i64::from(u32::MAX - 8) {
            (off as u32, None)
        } else {
            (0u32, Some(off as i32))
        };
        let val_ty = self.func.dfg.value_type(self.resolve_val(val));
        if val_ty == types::I128 {
            self.get_val(addr)?;
            if let Some(delta) = extra_add {
                self.body.push(0x41);
                write_sleb(self.body, i64::from(delta));
                self.body.push(0x6a); // i32.add
            }
            self.get_val(val)?;
            self.body.push(0x37); // i64.store
            write_uleb(self.body, 0);
            write_uleb(self.body, u64::from(base_off));

            self.get_val(addr)?;
            if let Some(delta) = extra_add {
                self.body.push(0x41);
                write_sleb(self.body, i64::from(delta));
                self.body.push(0x6a); // i32.add
            }
            self.get_val_hi(val)?;
            self.body.push(0x37); // i64.store
            write_uleb(self.body, 0);
            write_uleb(self.body, u64::from(base_off + 8));
            return Ok(());
        }
        self.get_val(addr)?;
        if let Some(delta) = extra_add {
            self.body.push(0x41);
            write_sleb(self.body, i64::from(delta));
            self.body.push(0x6a); // i32.add
        }
        self.get_val(val)?;
        let op = match opcode {
            Opcode::Store | Opcode::AtomicStore => match val_ty {
                types::I8 => 0x3a,  // i32.store8
                types::I16 => 0x3b, // i32.store16
                types::I32 => 0x36, // i32.store
                types::I64 => 0x37, // i64.store
                types::F32 => 0x38, // f32.store
                types::F64 => 0x39, // f64.store
                _ => return Err(format!("unsupported store type {val_ty}")),
            },
            Opcode::Istore8 => {
                if val_ty == types::I64 {
                    0x3c
                } else {
                    0x3a
                }
            }
            Opcode::Istore16 => {
                if val_ty == types::I64 {
                    0x3d
                } else {
                    0x3b
                }
            }
            Opcode::Istore32 => 0x3e, // i64.store32
            _ => return Err(format!("unexpected store opcode {opcode:?}")),
        };
        self.body.push(op);
        write_uleb(self.body, 0); // align = 0
        write_uleb(self.body, u64::from(base_off));
        Ok(())
    }

    fn emit_inst(&mut self, inst: ir::Inst, block_idx: u32) -> std::result::Result<(), String> {
        let data = &self.func.dfg.insts[inst];
        let opcode = data.opcode();
        let results = self.func.dfg.inst_results(inst);
        let res = results.first().copied();
        let res_ty = res.map(|r| self.func.dfg.value_type(r));

        match *data {
            InstructionData::UnaryImm { opcode: Opcode::Iconst, imm } => {
                let r = res.unwrap();
                let ty = res_ty.unwrap();
                let raw: i64 = imm.into();
                if ty == types::I64 {
                    self.body.push(0x42);
                    write_sleb(self.body, raw);
                } else {
                    self.body.push(0x41);
                    write_sleb(self.body, i64::from(raw as i32));
                }
                self.set_val(r)?;
            }
            InstructionData::UnaryIeee32 { opcode: Opcode::F32const, imm } => {
                let r = res.unwrap();
                self.body.push(0x43);
                self.body.extend_from_slice(&imm.bits().to_le_bytes());
                self.set_val(r)?;
            }
            InstructionData::UnaryIeee64 { opcode: Opcode::F64const, imm } => {
                let r = res.unwrap();
                self.body.push(0x44);
                self.body.extend_from_slice(&imm.bits().to_le_bytes());
                self.set_val(r)?;
            }
            InstructionData::StackAddr { opcode: Opcode::StackAddr, stack_slot, offset } => {
                let r = res.unwrap();
                let base = self.slot_offsets[&stack_slot];
                let off: i64 = offset.into();
                self.body.push(0x20); // local.get $fp
                write_uleb(self.body, u64::from(self.fp_local.unwrap()));
                let total = (i64::from(base) + off) as i32;
                if total != 0 {
                    self.body.push(0x41);
                    write_sleb(self.body, i64::from(total));
                    self.body.push(0x6a); // i32.add
                }
                self.set_val(r)?;
            }
            InstructionData::UnaryGlobalValue {
                opcode: Opcode::SymbolValue,
                global_value,
            } => {
                let r = res.unwrap();
                let (target, addend) = self.resolve_gv(global_value)?;
                self.body.push(0x41); // i32.const
                self.relocs.push(ModuleReloc {
                    kind: RELOC_MEM_SLEB,
                    offset: self.body.len() as CodeOffset,
                    name: target,
                    addend,
                });
                self.body.extend_from_slice(&[0x80, 0x80, 0x80, 0x80, 0x00]);
                self.set_val(r)?;
            }
            InstructionData::FuncAddr { opcode: Opcode::FuncAddr, func_ref } => {
                let r = res.unwrap();
                let target = self.reloc_target_from_ext_name(&self.func.dfg.ext_funcs[func_ref].name)?;
                self.body.push(0x41); // i32.const
                self.relocs.push(ModuleReloc {
                    kind: RELOC_TABLE_SLEB,
                    offset: self.body.len() as CodeOffset,
                    name: target,
                    addend: 0,
                });
                self.body.extend_from_slice(&[0x80, 0x80, 0x80, 0x80, 0x00]);
                self.set_val(r)?;
            }
            InstructionData::LoadNoOffset { opcode, arg, .. } => {
                self.emit_load(arg, Offset32::new(0), res.unwrap(), opcode)?;
            }
            InstructionData::Load { opcode, arg, offset, .. } => {
                self.emit_load(arg, offset, res.unwrap(), opcode)?;
            }
            InstructionData::StoreNoOffset { opcode, args: [val, addr], .. } => {
                self.emit_store(addr, val, Offset32::new(0), opcode)?;
            }
            InstructionData::Store { opcode, args: [val, addr], offset, .. } => {
                self.emit_store(addr, val, offset, opcode)?;
            }
            InstructionData::Binary { opcode: Opcode::Iconcat, args: [lo, hi] } => {
                let r = res.unwrap();
                self.get_val(lo)?;
                self.set_val(r)?;
                self.get_val(hi)?;
                self.set_val_hi(r)?;
            }
            InstructionData::Unary { opcode: Opcode::Isplit, arg } => {
                let r_lo = results[0];
                let r_hi = results[1];
                self.get_val(arg)?;
                self.set_val(r_lo)?;
                self.get_val_hi(arg)?;
                self.set_val(r_hi)?;
            }
            InstructionData::Binary { opcode, args: [a, b] }
                if self.func.dfg.value_type(self.resolve_val(a)) == types::I128 =>
            {
                let r = res.unwrap();
                match opcode {
                    Opcode::Band | Opcode::Bor | Opcode::Bxor => {
                        let op = match opcode {
                            Opcode::Band => 0x83,
                            Opcode::Bor => 0x84,
                            _ => 0x85,
                        };
                        self.get_val(a)?;
                        self.get_val(b)?;
                        self.body.push(op);
                        self.set_val(r)?;
                        self.get_val_hi(a)?;
                        self.get_val_hi(b)?;
                        self.body.push(op);
                        self.set_val_hi(r)?;
                    }
                    Opcode::Iadd | Opcode::UaddOverflow | Opcode::SaddOverflow => {
                        self.get_val(a)?;
                        self.get_val(b)?;
                        self.body.push(0x7c); // i64.add
                        self.set_val(r)?;
                        self.get_val_hi(a)?;
                        self.get_val_hi(b)?;
                        self.body.push(0x7c); // i64.add
                        // + carry (r.lo <_u a.lo)
                        self.get_val(r)?;
                        self.get_val(a)?;
                        self.body.push(0x54); // i64.lt_u
                        self.body.push(0xad); // i64.extend_i32_u
                        self.body.push(0x7c); // i64.add
                        self.set_val_hi(r)?;
                        if opcode == Opcode::UaddOverflow {
                            let r_ovf = results[1];
                            // overflow iff (a.hi + b.hi + carry) overflowed
                            self.get_val_hi(r)?;
                            self.get_val_hi(a)?;
                            self.body.push(0x54); // r.hi <_u a.hi
                            self.get_val_hi(r)?;
                            self.get_val_hi(a)?;
                            self.body.push(0x51); // r.hi == a.hi
                            self.get_val(r)?;
                            self.get_val(a)?;
                            self.body.push(0x54); // r.lo <_u a.lo
                            self.body.push(0x71); // i32.and
                            self.body.push(0x72); // i32.or
                            self.set_val(r_ovf)?;
                        } else if opcode == Opcode::SaddOverflow {
                            let r_ovf = results[1];
                            self.get_val_hi(a)?;
                            self.get_val_hi(r)?;
                            self.body.push(0x85); // xor
                            self.get_val_hi(b)?;
                            self.get_val_hi(r)?;
                            self.body.push(0x85); // xor
                            self.body.push(0x83); // and
                            self.body.push(0x42);
                            write_sleb(self.body, 0);
                            self.body.push(0x53); // i64.lt_s
                            self.set_val(r_ovf)?;
                        }
                    }
                    Opcode::Isub | Opcode::UsubOverflow | Opcode::SsubOverflow => {
                        self.get_val_hi(a)?;
                        self.get_val_hi(b)?;
                        self.body.push(0x7d); // i64.sub
                        // - borrow (a.lo <_u b.lo)
                        self.get_val(a)?;
                        self.get_val(b)?;
                        self.body.push(0x54); // i64.lt_u
                        self.body.push(0xad); // i64.extend_i32_u
                        self.body.push(0x7d); // i64.sub
                        self.set_val_hi(r)?;
                        self.get_val(a)?;
                        self.get_val(b)?;
                        self.body.push(0x7d); // i64.sub
                        self.set_val(r)?;
                        if opcode == Opcode::UsubOverflow {
                            let r_ovf = results[1];
                            self.get_val_hi(a)?;
                            self.get_val_hi(b)?;
                            self.body.push(0x54); // a.hi <_u b.hi
                            self.get_val_hi(a)?;
                            self.get_val_hi(b)?;
                            self.body.push(0x51); // a.hi == b.hi
                            self.get_val(a)?;
                            self.get_val(b)?;
                            self.body.push(0x54); // a.lo <_u b.lo
                            self.body.push(0x71); // i32.and
                            self.body.push(0x72); // i32.or
                            self.set_val(r_ovf)?;
                        } else if opcode == Opcode::SsubOverflow {
                            let r_ovf = results[1];
                            self.get_val_hi(a)?;
                            self.get_val_hi(b)?;
                            self.body.push(0x85); // xor
                            self.get_val_hi(a)?;
                            self.get_val_hi(r)?;
                            self.body.push(0x85); // xor
                            self.body.push(0x83); // and
                            self.body.push(0x42);
                            write_sleb(self.body, 0);
                            self.body.push(0x53); // i64.lt_s
                            self.set_val(r_ovf)?;
                        }
                    }
                    Opcode::Imul => {
                        // 128-bit wrapping multiply:
                        // r.lo = a.lo * b.lo
                        // r.hi = mulhu64(a.lo, b.lo) + a.lo * b.hi + a.hi * b.lo
                        self.get_val(a)?;
                        self.get_val(b)?;
                        self.body.push(0x7e); // i64.mul
                        self.set_val(r)?;

                        // Compute mulhu64(a.lo, b.lo):
                        // a0 = a.lo & 0xffffffff, a1 = a.lo >>_u 32
                        // b0 = b.lo & 0xffffffff, b1 = b.lo >>_u 32
                        // p11 = a1 * b1
                        self.get_val(a)?;
                        self.body.push(0x42);
                        write_sleb(self.body, 32);
                        self.body.push(0x88);
                        self.get_val(b)?;
                        self.body.push(0x42);
                        write_sleb(self.body, 32);
                        self.body.push(0x88);
                        self.body.push(0x7e); // p11

                        // + (a.lo * b.hi) + (a.hi * b.lo)
                        self.get_val(a)?;
                        self.get_val_hi(b)?;
                        self.body.push(0x7e);
                        self.body.push(0x7c);
                        self.get_val_hi(a)?;
                        self.get_val(b)?;
                        self.body.push(0x7e);
                        self.body.push(0x7c);

                        // + (((((a0*b0)>>32) + (a0*b1 & 0xffffffff) + (a1*b0 & 0xffffffff)) >> 32) + (a0*b1 >> 32) + (a1*b0 >> 32))
                        // First: (a0 * b1) >> 32
                        self.get_val(a)?;
                        self.body.push(0x42);
                        write_sleb(self.body, 0xffff_ffff);
                        self.body.push(0x83);
                        self.get_val(b)?;
                        self.body.push(0x42);
                        write_sleb(self.body, 32);
                        self.body.push(0x88);
                        self.body.push(0x7e); // p01
                        self.body.push(0x42);
                        write_sleb(self.body, 32);
                        self.body.push(0x88); // p01 >> 32
                        self.body.push(0x7c);

                        // + (a1 * b0) >> 32
                        self.get_val(a)?;
                        self.body.push(0x42);
                        write_sleb(self.body, 32);
                        self.body.push(0x88);
                        self.get_val(b)?;
                        self.body.push(0x42);
                        write_sleb(self.body, 0xffff_ffff);
                        self.body.push(0x83);
                        self.body.push(0x7e); // p10
                        self.body.push(0x42);
                        write_sleb(self.body, 32);
                        self.body.push(0x88); // p10 >> 32
                        self.body.push(0x7c);

                        // + (((p00 >> 32) + (p01 & 0xffffffff) + (p10 & 0xffffffff)) >> 32)
                        self.get_val(a)?;
                        self.body.push(0x42);
                        write_sleb(self.body, 0xffff_ffff);
                        self.body.push(0x83);
                        self.get_val(b)?;
                        self.body.push(0x42);
                        write_sleb(self.body, 0xffff_ffff);
                        self.body.push(0x83);
                        self.body.push(0x7e); // p00
                        self.body.push(0x42);
                        write_sleb(self.body, 32);
                        self.body.push(0x88); // p00 >> 32

                        self.get_val(a)?;
                        self.body.push(0x42);
                        write_sleb(self.body, 0xffff_ffff);
                        self.body.push(0x83);
                        self.get_val(b)?;
                        self.body.push(0x42);
                        write_sleb(self.body, 32);
                        self.body.push(0x88);
                        self.body.push(0x7e); // p01
                        self.body.push(0x42);
                        write_sleb(self.body, 0xffff_ffff);
                        self.body.push(0x83);
                        self.body.push(0x7c);

                        self.get_val(a)?;
                        self.body.push(0x42);
                        write_sleb(self.body, 32);
                        self.body.push(0x88);
                        self.get_val(b)?;
                        self.body.push(0x42);
                        write_sleb(self.body, 0xffff_ffff);
                        self.body.push(0x83);
                        self.body.push(0x7e); // p10
                        self.body.push(0x42);
                        write_sleb(self.body, 0xffff_ffff);
                        self.body.push(0x83);
                        self.body.push(0x7c);

                        self.body.push(0x42);
                        write_sleb(self.body, 32);
                        self.body.push(0x88); // mid >> 32
                        self.body.push(0x7c);
                        self.set_val_hi(r)?;
                    }
                    Opcode::Ishl | Opcode::Ushr | Opcode::Sshr => {
                        // Convert shift amount `b` to i64 masked by 127.
                        let push_shift_64 = |this: &mut Self| -> std::result::Result<(), String> {
                            let b_ty = this.func.dfg.value_type(this.resolve_val(b));
                            this.get_val(b)?;
                            if b_ty != types::I64 {
                                if b_ty == types::I128 {
                                    // `get_val(b)` already pushed `b.lo` (i64)
                                } else {
                                    this.body.push(0xad); // i64.extend_i32_u
                                }
                            }
                            this.body.push(0x42);
                            write_sleb(this.body, 127);
                            this.body.push(0x83); // i64.and
                            Ok(())
                        };
                        let push_shift_32 = |this: &mut Self| -> std::result::Result<(), String> {
                            let b_ty = this.func.dfg.value_type(this.resolve_val(b));
                            this.get_val(b)?;
                            if b_ty == types::I64 || b_ty == types::I128 {
                                this.body.push(0xa7); // i32.wrap_i64
                            }
                            this.body.push(0x41);
                            write_sleb(this.body, 127);
                            this.body.push(0x71); // i32.and
                            Ok(())
                        };
                        match opcode {
                            Opcode::Ishl => {
                                // if s == 0: (a.lo, a.hi)
                                // else if s < 64: (a.lo << s, (a.hi << s) | (a.lo >>_u (64 - s)))
                                // else: (0, a.lo << (s - 64))
                                push_shift_32(self)?;
                                self.body.push(0x45); // i32.eqz
                                self.body.push(0x04); // if
                                self.body.push(WASM_BLOCK_EMPTY);
                                self.get_val(a)?;
                                self.set_val(r)?;
                                self.get_val_hi(a)?;
                                self.set_val_hi(r)?;
                                self.body.push(0x05); // else
                                push_shift_32(self)?;
                                self.body.push(0x41);
                                write_sleb(self.body, 64);
                                self.body.push(0x49); // i32.lt_u
                                self.body.push(0x04); // if (s < 64)
                                self.body.push(WASM_BLOCK_EMPTY);
                                self.get_val_hi(a)?;
                                push_shift_64(self)?;
                                self.body.push(0x86); // a.hi << s
                                self.get_val(a)?;
                                self.body.push(0x42);
                                write_sleb(self.body, 64);
                                push_shift_64(self)?;
                                self.body.push(0x7d); // 64 - s
                                self.body.push(0x88); // a.lo >>_u (64 - s)
                                self.body.push(0x84); // i64.or
                                self.set_val_hi(r)?;
                                self.get_val(a)?;
                                push_shift_64(self)?;
                                self.body.push(0x86); // a.lo << s
                                self.set_val(r)?;
                                self.body.push(0x05); // else (s >= 64)
                                self.get_val(a)?;
                                push_shift_64(self)?;
                                self.body.push(0x42);
                                write_sleb(self.body, 64);
                                self.body.push(0x7d); // s - 64
                                self.body.push(0x86); // a.lo << (s - 64)
                                self.set_val_hi(r)?;
                                self.body.push(0x42);
                                write_sleb(self.body, 0);
                                self.set_val(r)?;
                                self.body.push(0x0b); // end
                                self.body.push(0x0b); // end
                            }
                            Opcode::Ushr | Opcode::Sshr => {
                                let shr_hi_op = if opcode == Opcode::Sshr { 0x87 } else { 0x88 };
                                push_shift_32(self)?;
                                self.body.push(0x45); // i32.eqz
                                self.body.push(0x04); // if
                                self.body.push(WASM_BLOCK_EMPTY);
                                self.get_val(a)?;
                                self.set_val(r)?;
                                self.get_val_hi(a)?;
                                self.set_val_hi(r)?;
                                self.body.push(0x05); // else
                                push_shift_32(self)?;
                                self.body.push(0x41);
                                write_sleb(self.body, 64);
                                self.body.push(0x49); // i32.lt_u
                                self.body.push(0x04); // if (s < 64)
                                self.body.push(WASM_BLOCK_EMPTY);
                                self.get_val(a)?;
                                push_shift_64(self)?;
                                self.body.push(0x88); // a.lo >>_u s
                                self.get_val_hi(a)?;
                                self.body.push(0x42);
                                write_sleb(self.body, 64);
                                push_shift_64(self)?;
                                self.body.push(0x7d); // 64 - s
                                self.body.push(0x86); // a.hi << (64 - s)
                                self.body.push(0x84); // i64.or
                                self.set_val(r)?;
                                self.get_val_hi(a)?;
                                push_shift_64(self)?;
                                self.body.push(shr_hi_op);
                                self.set_val_hi(r)?;
                                self.body.push(0x05); // else (s >= 64)
                                self.get_val_hi(a)?;
                                push_shift_64(self)?;
                                self.body.push(0x42);
                                write_sleb(self.body, 64);
                                self.body.push(0x7d); // s - 64
                                self.body.push(shr_hi_op);
                                self.set_val(r)?;
                                if opcode == Opcode::Sshr {
                                    self.get_val_hi(a)?;
                                    self.body.push(0x42);
                                    write_sleb(self.body, 63);
                                    self.body.push(0x87); // a.hi >>_s 63
                                } else {
                                    self.body.push(0x42);
                                    write_sleb(self.body, 0);
                                }
                                self.set_val_hi(r)?;
                                self.body.push(0x0b); // end
                                self.body.push(0x0b); // end
                            }
                            _ => unreachable!(),
                        }
                    }
                    _ => return Err(format!("unsupported 128-bit binary op {opcode:?}")),
                }
            }
            InstructionData::Binary { opcode, args: [a, b] }
                if matches!(
                    opcode,
                    Opcode::UaddOverflow
                        | Opcode::UsubOverflow
                        | Opcode::SaddOverflow
                        | Opcode::SsubOverflow
                        | Opcode::UmulOverflow
                        | Opcode::SmulOverflow
                ) =>
            {
                let r_val = results[0];
                let r_ovf = results[1];
                let ty = self.func.dfg.value_type(self.resolve_val(a));
                let is_64 = ty == types::I64;
                match opcode {
                    Opcode::UaddOverflow => {
                        self.get_val(a)?;
                        self.get_val(b)?;
                        self.body.push(if is_64 { 0x7c } else { 0x6a }); // add
                        self.set_val(r_val)?;
                        self.get_val_unsigned(r_val)?;
                        self.get_val_unsigned(a)?;
                        self.body.push(if is_64 { 0x54 } else { 0x49 }); // lt_u
                        self.set_val(r_ovf)?;
                    }
                    Opcode::UsubOverflow => {
                        self.get_val(a)?;
                        self.get_val(b)?;
                        self.body.push(if is_64 { 0x7d } else { 0x6b }); // sub
                        self.set_val(r_val)?;
                        self.get_val_unsigned(a)?;
                        self.get_val_unsigned(b)?;
                        self.body.push(if is_64 { 0x54 } else { 0x49 }); // lt_u
                        self.set_val(r_ovf)?;
                    }
                    Opcode::SaddOverflow => {
                        self.get_val(a)?;
                        self.get_val(b)?;
                        self.body.push(if is_64 { 0x7c } else { 0x6a }); // add
                        self.set_val(r_val)?;
                        // ((a_s ^ sum_s) & (b_s ^ sum_s)) < 0
                        self.get_val_signed(a)?;
                        self.get_val_signed(r_val)?;
                        self.body.push(if is_64 { 0x85 } else { 0x73 }); // xor
                        self.get_val_signed(b)?;
                        self.get_val_signed(r_val)?;
                        self.body.push(if is_64 { 0x85 } else { 0x73 }); // xor
                        self.body.push(if is_64 { 0x83 } else { 0x71 }); // and
                        if is_64 {
                            self.body.push(0x42);
                            write_sleb(self.body, 0);
                            self.body.push(0x53); // i64.lt_s
                        } else {
                            self.body.push(0x41);
                            write_sleb(self.body, 0);
                            self.body.push(0x48); // i32.lt_s
                        }
                        self.set_val(r_ovf)?;
                    }
                    Opcode::SsubOverflow => {
                        self.get_val(a)?;
                        self.get_val(b)?;
                        self.body.push(if is_64 { 0x7d } else { 0x6b }); // sub
                        self.set_val(r_val)?;
                        // ((a_s ^ b_s) & (a_s ^ diff_s)) < 0
                        self.get_val_signed(a)?;
                        self.get_val_signed(b)?;
                        self.body.push(if is_64 { 0x85 } else { 0x73 }); // xor
                        self.get_val_signed(a)?;
                        self.get_val_signed(r_val)?;
                        self.body.push(if is_64 { 0x85 } else { 0x73 }); // xor
                        self.body.push(if is_64 { 0x83 } else { 0x71 }); // and
                        if is_64 {
                            self.body.push(0x42);
                            write_sleb(self.body, 0);
                            self.body.push(0x53); // i64.lt_s
                        } else {
                            self.body.push(0x41);
                            write_sleb(self.body, 0);
                            self.body.push(0x48); // i32.lt_s
                        }
                        self.set_val(r_ovf)?;
                    }
                    Opcode::UmulOverflow => {
                        self.get_val(a)?;
                        self.get_val(b)?;
                        self.body.push(if is_64 { 0x7e } else { 0x6c }); // mul
                        self.set_val(r_val)?;
                        if !is_64 {
                            self.get_val_unsigned(a)?;
                            self.body.push(0xad); // i64.extend_i32_u
                            self.get_val_unsigned(b)?;
                            self.body.push(0xad); // i64.extend_i32_u
                            self.body.push(0x7e); // i64.mul
                            self.get_val_unsigned(r_val)?;
                            self.body.push(0xad); // i64.extend_i32_u
                            self.body.push(0x52); // i64.ne
                            self.set_val(r_ovf)?;
                        } else {
                            // b != 0 && (r_val /_u b) != a
                            self.get_val(b)?;
                            self.body.push(0x42);
                            write_sleb(self.body, 0);
                            self.body.push(0x52); // i64.ne
                            self.body.push(0x04); // if (result i32)
                            self.body.push(WASM_VAL_I32);
                            self.get_val(r_val)?;
                            self.get_val(b)?;
                            self.body.push(0x80); // i64.div_u
                            self.get_val(a)?;
                            self.body.push(0x52); // i64.ne
                            self.body.push(0x05); // else
                            self.body.push(0x41);
                            write_sleb(self.body, 0);
                            self.body.push(0x0b); // end
                            self.set_val(r_ovf)?;
                        }
                    }
                    Opcode::SmulOverflow => {
                        self.get_val(a)?;
                        self.get_val(b)?;
                        self.body.push(if is_64 { 0x7e } else { 0x6c }); // mul
                        self.set_val(r_val)?;
                        if !is_64 {
                            self.get_val_signed(a)?;
                            self.body.push(0xac); // i64.extend_i32_s
                            self.get_val_signed(b)?;
                            self.body.push(0xac); // i64.extend_i32_s
                            self.body.push(0x7e); // i64.mul
                            self.get_val_signed(r_val)?;
                            self.body.push(0xac); // i64.extend_i32_s
                            self.body.push(0x52); // i64.ne
                            self.set_val(r_ovf)?;
                        } else {
                            // if b == 0 -> 0; else if a == i64::MIN && b == -1 -> 1; else (r_val /_s b) != a
                            self.get_val(b)?;
                            self.body.push(0x50); // i64.eqz
                            self.body.push(0x04); // if (result i32)
                            self.body.push(WASM_VAL_I32);
                            self.body.push(0x41);
                            write_sleb(self.body, 0);
                            self.body.push(0x05); // else
                            self.get_val(a)?;
                            self.body.push(0x42);
                            write_sleb(self.body, i64::MIN);
                            self.body.push(0x51); // i64.eq
                            self.get_val(b)?;
                            self.body.push(0x42);
                            write_sleb(self.body, -1);
                            self.body.push(0x51); // i64.eq
                            self.body.push(0x71); // i32.and
                            self.body.push(0x04); // if (result i32)
                            self.body.push(WASM_VAL_I32);
                            self.body.push(0x41);
                            write_sleb(self.body, 1);
                            self.body.push(0x05); // else
                            self.get_val(r_val)?;
                            self.get_val(b)?;
                            self.body.push(0x7f); // i64.div_s
                            self.get_val(a)?;
                            self.body.push(0x52); // i64.ne
                            self.body.push(0x0b); // end
                            self.body.push(0x0b); // end
                            self.set_val(r_ovf)?;
                        }
                    }
                    _ => unreachable!(),
                }
            }
            InstructionData::Binary { opcode, args: [a, b] }
                if matches!(opcode, Opcode::Umax | Opcode::Umin | Opcode::Smax | Opcode::Smin) =>
            {
                let r = res.unwrap();
                let ty = self.func.dfg.value_type(self.resolve_val(a));
                let is_64 = ty == types::I64;
                self.get_val(a)?;
                self.get_val(b)?;
                match opcode {
                    Opcode::Umax => {
                        self.get_val_unsigned(a)?;
                        self.get_val_unsigned(b)?;
                        self.body.push(if is_64 { 0x56 } else { 0x4b }); // gt_u
                    }
                    Opcode::Umin => {
                        self.get_val_unsigned(a)?;
                        self.get_val_unsigned(b)?;
                        self.body.push(if is_64 { 0x54 } else { 0x49 }); // lt_u
                    }
                    Opcode::Smax => {
                        self.get_val_signed(a)?;
                        self.get_val_signed(b)?;
                        self.body.push(if is_64 { 0x55 } else { 0x4a }); // gt_s
                    }
                    Opcode::Smin => {
                        self.get_val_signed(a)?;
                        self.get_val_signed(b)?;
                        self.body.push(if is_64 { 0x53 } else { 0x48 }); // lt_s
                    }
                    _ => unreachable!(),
                }
                self.body.push(0x1b); // select
                self.set_val(r)?;
            }
            InstructionData::Binary { opcode, args: [a, b] } => {
                let r = res.unwrap();
                let ty = self.func.dfg.value_type(self.resolve_val(a));
                let is_64 = ty == types::I64;
                match opcode {
                    Opcode::Udiv | Opcode::Urem | Opcode::Ushr => {
                        self.get_val_unsigned(a)?;
                        if opcode == Opcode::Ushr && is_64 && self.func.dfg.value_type(self.resolve_val(b)) != types::I64 {
                            self.get_val_unsigned(b)?;
                            self.body.push(0xad); // i64.extend_i32_u
                        } else if opcode == Opcode::Ushr && !is_64 && self.func.dfg.value_type(self.resolve_val(b)) == types::I64 {
                            self.get_val(b)?;
                            self.body.push(0xa7); // i32.wrap_i64
                        } else {
                            self.get_val_unsigned(b)?;
                        }
                    }
                    Opcode::Sdiv | Opcode::Srem | Opcode::Sshr => {
                        self.get_val_signed(a)?;
                        if opcode == Opcode::Sshr && is_64 && self.func.dfg.value_type(self.resolve_val(b)) != types::I64 {
                            self.get_val_unsigned(b)?;
                            self.body.push(0xad); // i64.extend_i32_u
                        } else if opcode == Opcode::Sshr && !is_64 && self.func.dfg.value_type(self.resolve_val(b)) == types::I64 {
                            self.get_val(b)?;
                            self.body.push(0xa7); // i32.wrap_i64
                        } else {
                            self.get_val_signed(b)?;
                        }
                    }
                    Opcode::Ishl | Opcode::Rotl | Opcode::Rotr => {
                        self.get_val(a)?;
                        let b_ty = self.func.dfg.value_type(self.resolve_val(b));
                        if is_64 && b_ty != types::I64 {
                            self.get_val_unsigned(b)?;
                            self.body.push(0xad); // i64.extend_i32_u
                        } else if !is_64 && b_ty == types::I64 {
                            self.get_val(b)?;
                            self.body.push(0xa7); // i32.wrap_i64
                        } else {
                            self.get_val(b)?;
                        }
                    }
                    _ => {
                        self.get_val(a)?;
                        self.get_val(b)?;
                    }
                }
                let op = match (opcode, ty) {
                    (Opcode::Iadd, types::I64) => 0x7c,
                    (Opcode::Iadd, _) => 0x6a,
                    (Opcode::Isub, types::I64) => 0x7d,
                    (Opcode::Isub, _) => 0x6b,
                    (Opcode::Imul, types::I64) => 0x7e,
                    (Opcode::Imul, _) => 0x6c,
                    (Opcode::Sdiv, types::I64) => 0x7f,
                    (Opcode::Sdiv, _) => 0x6d,
                    (Opcode::Udiv, types::I64) => 0x80,
                    (Opcode::Udiv, _) => 0x6e,
                    (Opcode::Srem, types::I64) => 0x81,
                    (Opcode::Srem, _) => 0x6f,
                    (Opcode::Urem, types::I64) => 0x82,
                    (Opcode::Urem, _) => 0x70,
                    (Opcode::Band, types::I64) => 0x83,
                    (Opcode::Band, _) => 0x71,
                    (Opcode::Bor, types::I64) => 0x84,
                    (Opcode::Bor, _) => 0x72,
                    (Opcode::Bxor, types::I64) => 0x85,
                    (Opcode::Bxor, _) => 0x73,
                    (Opcode::Ishl, types::I64) => 0x86,
                    (Opcode::Ishl, _) => 0x74,
                    (Opcode::Sshr, types::I64) => 0x87,
                    (Opcode::Sshr, _) => 0x75,
                    (Opcode::Ushr, types::I64) => 0x88,
                    (Opcode::Ushr, _) => 0x76,
                    (Opcode::Rotl, types::I64) => 0x89,
                    (Opcode::Rotl, _) => 0x77,
                    (Opcode::Rotr, types::I64) => 0x8a,
                    (Opcode::Rotr, _) => 0x78,
                    (Opcode::Fadd, types::F32) => 0x92,
                    (Opcode::Fadd, types::F64) => 0xa0,
                    (Opcode::Fsub, types::F32) => 0x93,
                    (Opcode::Fsub, types::F64) => 0xa1,
                    (Opcode::Fmul, types::F32) => 0x94,
                    (Opcode::Fmul, types::F64) => 0xa2,
                    (Opcode::Fdiv, types::F32) => 0x95,
                    (Opcode::Fdiv, types::F64) => 0xa3,
                    (Opcode::Fmin, types::F32) => 0x96,
                    (Opcode::Fmin, types::F64) => 0xa4,
                    (Opcode::Fmax, types::F32) => 0x97,
                    (Opcode::Fmax, types::F64) => 0xa5,
                    (Opcode::Fcopysign, types::F32) => 0x98,
                    (Opcode::Fcopysign, types::F64) => 0xa6,
                    _ => return Err(format!("unsupported binary op {opcode:?} on {ty}")),
                };
                self.body.push(op);
                self.set_val(r)?;
            }
            InstructionData::IntCompare { opcode: Opcode::Icmp, cond, args: [a, b] } => {
                let r = res.unwrap();
                let ty = self.func.dfg.value_type(self.resolve_val(a));
                if ty == types::I128 {
                    match cond {
                        IntCC::Equal => {
                            self.get_val(a)?;
                            self.get_val(b)?;
                            self.body.push(0x51); // i64.eq
                            self.get_val_hi(a)?;
                            self.get_val_hi(b)?;
                            self.body.push(0x51); // i64.eq
                            self.body.push(0x71); // i32.and
                        }
                        IntCC::NotEqual => {
                            self.get_val(a)?;
                            self.get_val(b)?;
                            self.body.push(0x52); // i64.ne
                            self.get_val_hi(a)?;
                            self.get_val_hi(b)?;
                            self.body.push(0x52); // i64.ne
                            self.body.push(0x72); // i32.or
                        }
                        _ => {
                            let hi_strict = match cond {
                                IntCC::SignedLessThan | IntCC::SignedLessThanOrEqual => 0x53,
                                IntCC::UnsignedLessThan | IntCC::UnsignedLessThanOrEqual => 0x54,
                                IntCC::SignedGreaterThan | IntCC::SignedGreaterThanOrEqual => 0x55,
                                IntCC::UnsignedGreaterThan | IntCC::UnsignedGreaterThanOrEqual => 0x56,
                                _ => unreachable!(),
                            };
                            let lo_cmp = match cond {
                                IntCC::SignedLessThan | IntCC::UnsignedLessThan => 0x54,
                                IntCC::SignedLessThanOrEqual | IntCC::UnsignedLessThanOrEqual => 0x58,
                                IntCC::SignedGreaterThan | IntCC::UnsignedGreaterThan => 0x56,
                                IntCC::SignedGreaterThanOrEqual | IntCC::UnsignedGreaterThanOrEqual => 0x5a,
                                _ => unreachable!(),
                            };
                            // (a.hi < b.hi) | ((a.hi == b.hi) & (a.lo <_u b.lo))
                            self.get_val_hi(a)?;
                            self.get_val_hi(b)?;
                            self.body.push(hi_strict);
                            self.get_val_hi(a)?;
                            self.get_val_hi(b)?;
                            self.body.push(0x51); // i64.eq
                            self.get_val(a)?;
                            self.get_val(b)?;
                            self.body.push(lo_cmp);
                            self.body.push(0x71); // i32.and
                            self.body.push(0x72); // i32.or
                        }
                    }
                    self.set_val(r)?;
                    return Ok(());
                }
                let signed = matches!(
                    cond,
                    IntCC::SignedLessThan
                        | IntCC::SignedGreaterThanOrEqual
                        | IntCC::SignedGreaterThan
                        | IntCC::SignedLessThanOrEqual
                );
                if signed {
                    self.get_val_signed(a)?;
                    self.get_val_signed(b)?;
                } else {
                    self.get_val_unsigned(a)?;
                    self.get_val_unsigned(b)?;
                }
                let op = Self::int_cmp_opcode(cond, ty == types::I64);
                self.body.push(op);
                self.set_val(r)?;
            }
            InstructionData::FloatCompare { opcode: Opcode::Fcmp, cond, args: [a, b] } => {
                let r = res.unwrap();
                let ty = self.func.dfg.value_type(self.resolve_val(a));
                let is_64 = ty == types::F64;
                self.get_val(a)?;
                self.get_val(b)?;
                let op = match (cond, is_64) {
                    (FloatCC::Equal, false) => 0x5b,
                    (FloatCC::Equal, true) => 0x61,
                    (FloatCC::NotEqual, false) => 0x5c,
                    (FloatCC::NotEqual, true) => 0x62,
                    (FloatCC::LessThan, false) => 0x5d,
                    (FloatCC::LessThan, true) => 0x63,
                    (FloatCC::GreaterThan, false) => 0x5e,
                    (FloatCC::GreaterThan, true) => 0x64,
                    (FloatCC::LessThanOrEqual, false) => 0x5f,
                    (FloatCC::LessThanOrEqual, true) => 0x65,
                    (FloatCC::GreaterThanOrEqual, false) => 0x60,
                    (FloatCC::GreaterThanOrEqual, true) => 0x66,
                    _ => return Err(format!("unsupported FloatCC {cond:?}")),
                };
                self.body.push(op);
                self.set_val(r)?;
            }
            InstructionData::Ternary { opcode: Opcode::Select, args: [cond, t_val, f_val] } => {
                let r = res.unwrap();
                let dst_ty = res_ty.unwrap();
                let cond_ty = self.func.dfg.value_type(self.resolve_val(cond));
                self.get_val(t_val)?;
                self.get_val(f_val)?;
                self.get_val(cond)?;
                if cond_ty == types::I64 {
                    self.body.push(0x42);
                    write_sleb(self.body, 0);
                    self.body.push(0x52); // i64.ne
                }
                self.body.push(0x1b); // select
                self.set_val(r)?;
                if dst_ty == types::I128 {
                    self.get_val_hi(t_val)?;
                    self.get_val_hi(f_val)?;
                    self.get_val(cond)?;
                    if cond_ty == types::I64 {
                        self.body.push(0x42);
                        write_sleb(self.body, 0);
                        self.body.push(0x52); // i64.ne
                    }
                    self.body.push(0x1b); // select
                    self.set_val_hi(r)?;
                }
            }
            InstructionData::Unary { opcode, arg } => {
                let r = res.unwrap();
                let dst_ty = res_ty.unwrap();
                let src_ty = self.func.dfg.value_type(self.resolve_val(arg));
                if dst_ty == types::I128 {
                    match opcode {
                        Opcode::Uextend => {
                            self.get_val_unsigned(arg)?;
                            if src_ty != types::I64 {
                                self.body.push(0xad); // i64.extend_i32_u
                            }
                            self.set_val(r)?;
                            self.body.push(0x42);
                            write_sleb(self.body, 0);
                            self.set_val_hi(r)?;
                        }
                        Opcode::Sextend => {
                            self.get_val_signed(arg)?;
                            if src_ty != types::I64 {
                                self.body.push(0xac); // i64.extend_i32_s
                            }
                            self.set_val(r)?;
                            self.get_val(r)?;
                            self.body.push(0x42);
                            write_sleb(self.body, 63);
                            self.body.push(0x87); // i64.shr_s
                            self.set_val_hi(r)?;
                        }
                        Opcode::Bnot => {
                            self.get_val(arg)?;
                            self.body.push(0x42);
                            write_sleb(self.body, -1);
                            self.body.push(0x85);
                            self.set_val(r)?;
                            self.get_val_hi(arg)?;
                            self.body.push(0x42);
                            write_sleb(self.body, -1);
                            self.body.push(0x85);
                            self.set_val_hi(r)?;
                        }
                        Opcode::Ineg => {
                            self.body.push(0x42);
                            write_sleb(self.body, 0);
                            self.get_val_hi(arg)?;
                            self.body.push(0x7d); // 0 - a.hi
                            self.body.push(0x42);
                            write_sleb(self.body, 0);
                            self.get_val(arg)?;
                            self.body.push(0x54); // 0 <_u a.lo
                            self.body.push(0xad);
                            self.body.push(0x7d); // - borrow
                            self.set_val_hi(r)?;
                            self.body.push(0x42);
                            write_sleb(self.body, 0);
                            self.get_val(arg)?;
                            self.body.push(0x7d); // 0 - a.lo
                            self.set_val(r)?;
                        }
                        Opcode::Bswap => {
                            // Swap hi -> lo and lo -> hi, each 64-bit bswapped.
                            for (in_hi, out_hi) in [(true, false), (false, true)] {
                                for b in 0..8usize {
                                    if in_hi {
                                        self.get_val_hi(arg)?;
                                    } else {
                                        self.get_val(arg)?;
                                    }
                                    if b > 0 {
                                        self.body.push(0x42);
                                        write_sleb(self.body, (b * 8) as i64);
                                        self.body.push(0x88);
                                    }
                                    self.body.push(0x42);
                                    write_sleb(self.body, 0xff);
                                    self.body.push(0x83);
                                    let shl = (7 - b) * 8;
                                    if shl > 0 {
                                        self.body.push(0x42);
                                        write_sleb(self.body, shl as i64);
                                        self.body.push(0x86);
                                    }
                                    if b > 0 {
                                        self.body.push(0x84);
                                    }
                                }
                                if out_hi {
                                    self.set_val_hi(r)?;
                                } else {
                                    self.set_val(r)?;
                                }
                            }
                        }
                        Opcode::Clz => {
                            // if a.hi == 0 -> 64 + clz(a.lo) else clz(a.hi)
                            self.get_val_hi(arg)?;
                            self.body.push(0x50); // i64.eqz
                            self.body.push(0x04);
                            self.body.push(WASM_VAL_I64);
                            self.get_val(arg)?;
                            self.body.push(0x79); // i64.clz
                            self.body.push(0x42);
                            write_sleb(self.body, 64);
                            self.body.push(0x7c);
                            self.body.push(0x05);
                            self.get_val_hi(arg)?;
                            self.body.push(0x79);
                            self.body.push(0x0b);
                            self.set_val(r)?;
                            self.body.push(0x42);
                            write_sleb(self.body, 0);
                            self.set_val_hi(r)?;
                        }
                        Opcode::Ctz => {
                            // if a.lo == 0 -> 64 + ctz(a.hi) else ctz(a.lo)
                            self.get_val(arg)?;
                            self.body.push(0x50); // i64.eqz
                            self.body.push(0x04);
                            self.body.push(WASM_VAL_I64);
                            self.get_val_hi(arg)?;
                            self.body.push(0x7a); // i64.ctz
                            self.body.push(0x42);
                            write_sleb(self.body, 64);
                            self.body.push(0x7c);
                            self.body.push(0x05);
                            self.get_val(arg)?;
                            self.body.push(0x7a);
                            self.body.push(0x0b);
                            self.set_val(r)?;
                            self.body.push(0x42);
                            write_sleb(self.body, 0);
                            self.set_val_hi(r)?;
                        }
                        Opcode::Popcnt => {
                            self.get_val(arg)?;
                            self.body.push(0x7b);
                            self.get_val_hi(arg)?;
                            self.body.push(0x7b);
                            self.body.push(0x7c);
                            self.set_val(r)?;
                            self.body.push(0x42);
                            write_sleb(self.body, 0);
                            self.set_val_hi(r)?;
                        }
                        _ => return Err(format!("unsupported 128-bit unary op {opcode:?}")),
                    }
                    return Ok(());
                }
                match opcode {
                    Opcode::Uextend => {
                        self.get_val_unsigned(arg)?;
                        if dst_ty == types::I64 && src_ty != types::I64 {
                            self.body.push(0xad); // i64.extend_i32_u
                        }
                    }
                    Opcode::Sextend => {
                        self.get_val_signed(arg)?;
                        if dst_ty == types::I64 && src_ty != types::I64 {
                            self.body.push(0xac); // i64.extend_i32_s
                        }
                    }
                    Opcode::Ireduce => {
                        self.get_val(arg)?;
                        if (src_ty == types::I64 || src_ty == types::I128) && dst_ty != types::I64 {
                            self.body.push(0xa7); // i32.wrap_i64
                        }
                    }
                    Opcode::Bnot => {
                        self.get_val(arg)?;
                        if dst_ty == types::I64 {
                            self.body.push(0x42);
                            write_sleb(self.body, -1);
                            self.body.push(0x85); // i64.xor
                        } else {
                            self.body.push(0x41);
                            write_sleb(self.body, -1);
                            self.body.push(0x73); // i32.xor
                        }
                    }
                    Opcode::Ineg => {
                        if dst_ty == types::I64 {
                            self.body.push(0x42);
                            write_sleb(self.body, 0);
                            self.get_val(arg)?;
                            self.body.push(0x7d); // i64.sub
                        } else {
                            self.body.push(0x41);
                            write_sleb(self.body, 0);
                            self.get_val(arg)?;
                            self.body.push(0x6b); // i32.sub
                        }
                    }
                    Opcode::Clz => {
                        self.get_val_unsigned(arg)?;
                        if dst_ty == types::I64 {
                            self.body.push(0x79); // i64.clz
                        } else {
                            self.body.push(0x67); // i32.clz
                            if src_ty == types::I8 {
                                self.body.push(0x41);
                                write_sleb(self.body, 24);
                                self.body.push(0x6b);
                            } else if src_ty == types::I16 {
                                self.body.push(0x41);
                                write_sleb(self.body, 16);
                                self.body.push(0x6b);
                            }
                        }
                    }
                    Opcode::Ctz => {
                        self.get_val(arg)?;
                        if dst_ty == types::I64 {
                            self.body.push(0x7a); // i64.ctz
                        } else {
                            if src_ty == types::I8 {
                                self.body.push(0x41);
                                write_sleb(self.body, 0x100);
                                self.body.push(0x72); // i32.or
                            } else if src_ty == types::I16 {
                                self.body.push(0x41);
                                write_sleb(self.body, 0x10000);
                                self.body.push(0x72); // i32.or
                            }
                            self.body.push(0x68); // i32.ctz
                        }
                    }
                    Opcode::Popcnt => {
                        self.get_val_unsigned(arg)?;
                        if dst_ty == types::I64 {
                            self.body.push(0x7b); // i64.popcnt
                        } else {
                            self.body.push(0x69); // i32.popcnt
                        }
                    }
                    Opcode::Bswap => {
                        let bytes = dst_ty.bytes() as usize;
                        for b in 0..bytes {
                            self.get_val_unsigned(arg)?;
                            if dst_ty == types::I64 {
                                if b > 0 {
                                    self.body.push(0x42);
                                    write_sleb(self.body, (b * 8) as i64);
                                    self.body.push(0x88); // i64.shr_u
                                }
                                self.body.push(0x42);
                                write_sleb(self.body, 0xff);
                                self.body.push(0x83); // i64.and
                                let shl = (bytes - 1 - b) * 8;
                                if shl > 0 {
                                    self.body.push(0x42);
                                    write_sleb(self.body, shl as i64);
                                    self.body.push(0x86); // i64.shl
                                }
                                if b > 0 {
                                    self.body.push(0x84); // i64.or
                                }
                            } else {
                                if b > 0 {
                                    self.body.push(0x41);
                                    write_sleb(self.body, (b * 8) as i64);
                                    self.body.push(0x76); // i32.shr_u
                                }
                                self.body.push(0x41);
                                write_sleb(self.body, 0xff);
                                self.body.push(0x71); // i32.and
                                let shl = (bytes - 1 - b) * 8;
                                if shl > 0 {
                                    self.body.push(0x41);
                                    write_sleb(self.body, shl as i64);
                                    self.body.push(0x74); // i32.shl
                                }
                                if b > 0 {
                                    self.body.push(0x72); // i32.or
                                }
                            }
                        }
                    }
                    Opcode::Bitcast => {
                        self.get_val(arg)?;
                        match (src_ty, dst_ty) {
                            (types::F32, types::I32) => self.body.push(0xbc),
                            (types::F64, types::I64) => self.body.push(0xbd),
                            (types::I32, types::F32) => self.body.push(0xbe),
                            (types::I64, types::F64) => self.body.push(0xbf),
                            (a, b) if a == b => {}
                            _ => return Err(format!("unsupported bitcast {src_ty} -> {dst_ty}")),
                        }
                    }
                    Opcode::Fpromote => {
                        self.get_val(arg)?;
                        self.body.push(0xbb); // f64.promote_f32
                    }
                    Opcode::Fdemote => {
                        self.get_val(arg)?;
                        self.body.push(0xb6); // f32.demote_f64
                    }
                    Opcode::Fneg => {
                        self.get_val(arg)?;
                        self.body.push(if dst_ty == types::F64 { 0x9a } else { 0x8c });
                    }
                    Opcode::Fabs => {
                        self.get_val(arg)?;
                        self.body.push(if dst_ty == types::F64 { 0x99 } else { 0x8b });
                    }
                    Opcode::Sqrt => {
                        self.get_val(arg)?;
                        self.body.push(if dst_ty == types::F64 { 0x9f } else { 0x91 });
                    }
                    Opcode::Ceil => {
                        self.get_val(arg)?;
                        self.body.push(if dst_ty == types::F64 { 0x9b } else { 0x8d });
                    }
                    Opcode::Floor => {
                        self.get_val(arg)?;
                        self.body.push(if dst_ty == types::F64 { 0x9c } else { 0x8e });
                    }
                    Opcode::Trunc => {
                        self.get_val(arg)?;
                        self.body.push(if dst_ty == types::F64 { 0x9d } else { 0x8f });
                    }
                    Opcode::Nearest => {
                        self.get_val(arg)?;
                        self.body.push(if dst_ty == types::F64 { 0x9e } else { 0x90 });
                    }
                    Opcode::FcvtFromSint => {
                        self.get_val_signed(arg)?;
                        let op = match (src_ty == types::I64, dst_ty == types::F64) {
                            (false, false) => 0xb2,
                            (true, false) => 0xb4,
                            (false, true) => 0xb7,
                            (true, true) => 0xb9,
                        };
                        self.body.push(op);
                    }
                    Opcode::FcvtFromUint => {
                        self.get_val_unsigned(arg)?;
                        let op = match (src_ty == types::I64, dst_ty == types::F64) {
                            (false, false) => 0xb3,
                            (true, false) => 0xb5,
                            (false, true) => 0xb8,
                            (true, true) => 0xba,
                        };
                        self.body.push(op);
                    }
                    Opcode::FcvtToSint | Opcode::FcvtToSintSat => {
                        self.get_val(arg)?;
                        let sat = match (src_ty == types::F64, dst_ty == types::I64) {
                            (false, false) => 0u8,
                            (true, false) => 2u8,
                            (false, true) => 4u8,
                            (true, true) => 6u8,
                        };
                        self.body.push(0xfc);
                        write_uleb(self.body, u64::from(sat));
                    }
                    Opcode::FcvtToUint | Opcode::FcvtToUintSat => {
                        self.get_val(arg)?;
                        let sat = match (src_ty == types::F64, dst_ty == types::I64) {
                            (false, false) => 1u8,
                            (true, false) => 3u8,
                            (false, true) => 5u8,
                            (true, true) => 7u8,
                        };
                        self.body.push(0xfc);
                        write_uleb(self.body, u64::from(sat));
                    }
                    _ => return Err(format!("unsupported unary op {opcode:?}")),
                }
                self.set_val(r)?;
            }
            InstructionData::Call { opcode: Opcode::Call, func_ref, ref args } => {
                let args_slice = args.as_slice(&self.func.dfg.value_lists);
                let ext = &self.func.dfg.ext_funcs[func_ref];
                if let ExternalName::LibCall(lc) = ext.name {
                    match lc {
                        ir::LibCall::Memcpy | ir::LibCall::Memmove => {
                            self.get_val(args_slice[0])?;
                            self.get_val(args_slice[1])?;
                            self.get_val(args_slice[2])?;
                            self.body.extend_from_slice(&[0xfc, 0x0a, 0x00, 0x00]); // memory.copy
                            return Ok(());
                        }
                        ir::LibCall::Memset => {
                            self.get_val(args_slice[0])?;
                            self.get_val(args_slice[1])?;
                            self.get_val(args_slice[2])?;
                            self.body.extend_from_slice(&[0xfc, 0x0b, 0x00]); // memory.fill
                            return Ok(());
                        }
                        _ => return Err(format!("unsupported LibCall {lc:?} in wasm backend")),
                    }
                }
                let target = self.reloc_target_from_ext_name(&ext.name)?;
                for &arg in args_slice {
                    self.get_val(arg)?;
                    if self.func.dfg.value_type(self.resolve_val(arg)) == types::I128 {
                        self.get_val_hi(arg)?;
                    }
                }
                self.body.push(0x10); // call
                self.relocs.push(ModuleReloc {
                    kind: RELOC_FN_LEB,
                    offset: self.body.len() as CodeOffset,
                    name: target,
                    addend: 0,
                });
                self.body.extend_from_slice(&[0x80, 0x80, 0x80, 0x80, 0x00]);
                if let Some(r) = res {
                    self.set_val(r)?;
                }
            }
            InstructionData::CallIndirect { opcode: Opcode::CallIndirect, sig_ref, ref args } => {
                let args_slice = args.as_slice(&self.func.dfg.value_lists);
                let (&callee, call_args) =
                    args_slice.split_first().ok_or_else(|| "empty CallIndirect".to_owned())?;
                let wsig_id = intern_global_wsig(&self.func.dfg.signatures[sig_ref])?;
                for &arg in call_args {
                    self.get_val(arg)?;
                    if self.func.dfg.value_type(self.resolve_val(arg)) == types::I128 {
                        self.get_val_hi(arg)?;
                    }
                }
                self.get_val(callee)?;
                self.body.push(0x11); // call_indirect
                self.relocs.push(ModuleReloc {
                    kind: RELOC_TYPE_LEB,
                    offset: self.body.len() as CodeOffset,
                    name: ModuleRelocTarget::user(3, wsig_id),
                    addend: 0,
                });
                self.body.extend_from_slice(&[0x80, 0x80, 0x80, 0x80, 0x00]);
                self.body.push(0x00); // table 0
                if let Some(r) = res {
                    self.set_val(r)?;
                }
            }
            InstructionData::Jump { opcode: Opcode::Jump, destination } => {
                let target = destination.block(&self.func.dfg.value_lists);
                let args: Vec<Value> = destination
                    .args(&self.func.dfg.value_lists)
                    .filter_map(|a| a.as_value())
                    .collect();
                self.emit_jump_to(target, &args, block_idx, 0)?;
            }
            InstructionData::Brif {
                opcode: Opcode::Brif,
                arg,
                blocks: [then_call, else_call],
            } => {
                let arg_ty = self.func.dfg.value_type(self.resolve_val(arg));
                self.get_val(arg)?;
                if arg_ty == types::I128 {
                    self.get_val_hi(arg)?;
                    self.body.push(0x84); // i64.or
                    self.body.push(0x42);
                    write_sleb(self.body, 0);
                    self.body.push(0x52); // i64.ne
                } else if arg_ty == types::I64 {
                    self.body.push(0x42);
                    write_sleb(self.body, 0);
                    self.body.push(0x52); // i64.ne
                }
                // if (empty)
                self.body.push(0x04);
                self.body.push(WASM_BLOCK_EMPTY);
                let then_b = then_call.block(&self.func.dfg.value_lists);
                let then_args: Vec<Value> = then_call
                    .args(&self.func.dfg.value_lists)
                    .filter_map(|a| a.as_value())
                    .collect();
                self.emit_jump_to(then_b, &then_args, block_idx, 1)?;
                // else
                self.body.push(0x05);
                let else_b = else_call.block(&self.func.dfg.value_lists);
                let else_args: Vec<Value> = else_call
                    .args(&self.func.dfg.value_lists)
                    .filter_map(|a| a.as_value())
                    .collect();
                self.emit_jump_to(else_b, &else_args, block_idx, 1)?;
                // end
                self.body.push(0x0b);
            }
            InstructionData::BranchTable { opcode: Opcode::BrTable, arg, table } => {
                let jt_data: &JumpTableData = &self.func.stencil.dfg.jump_tables[table];
                let all = jt_data.all_branches();
                let (&default_call, rest) =
                    all.split_first().ok_or_else(|| "empty jump table".to_owned())?;
                for (i, branch_call) in rest.iter().enumerate() {
                    self.get_val(arg)?;
                    self.body.push(0x41);
                    write_sleb(self.body, i as i64);
                    self.body.push(0x46); // i32.eq
                    self.body.push(0x04); // if
                    self.body.push(WASM_BLOCK_EMPTY);
                    let b = branch_call.block(&self.func.dfg.value_lists);
                    let args: Vec<Value> = branch_call
                        .args(&self.func.dfg.value_lists)
                        .filter_map(|a| a.as_value())
                        .collect();
                    self.emit_jump_to(b, &args, block_idx, 1)?;
                    self.body.push(0x0b); // end if
                }
                let def_b = default_call.block(&self.func.dfg.value_lists);
                let def_args: Vec<Value> = default_call
                    .args(&self.func.dfg.value_lists)
                    .filter_map(|a| a.as_value())
                    .collect();
                self.emit_jump_to(def_b, &def_args, block_idx, 0)?;
            }
            InstructionData::MultiAry { opcode: Opcode::Return, ref args } => {
                let args_slice = args.as_slice(&self.func.dfg.value_lists);
                if let Some(&ret_val) = args_slice.first() {
                    self.get_val(ret_val)?;
                }
                if let Some(fp) = self.fp_local {
                    self.body.push(0x20); // local.get $fp
                    write_uleb(self.body, u64::from(fp));
                    self.body.push(0x41); // i32.const frame_size
                    write_sleb(self.body, i64::from(self.frame_size));
                    self.body.push(0x6a); // i32.add
                    self.body.push(0x24); // global.set __stack_pointer
                    self.relocs.push(ModuleReloc {
                        kind: RELOC_SP_GLOBAL_LEB,
                        offset: self.body.len() as CodeOffset,
                        name: ModuleRelocTarget::user(2, 0),
                        addend: 0,
                    });
                    self.body.extend_from_slice(&[0x80, 0x80, 0x80, 0x80, 0x00]);
                }
                self.body.push(0x0f); // return
            }
            InstructionData::Trap { opcode: Opcode::Trap, .. } => {
                self.body.push(0x00); // unreachable
            }
            InstructionData::CondTrap { opcode: Opcode::Trapz | Opcode::Trapnz, arg, .. } => {
                self.get_val(arg)?;
                let ty = self.func.dfg.value_type(self.resolve_val(arg));
                if opcode == Opcode::Trapz {
                    self.body.push(if ty == types::I64 { 0x50 } else { 0x45 }); // eqz
                } else if ty == types::I64 {
                    self.body.push(0x42);
                    write_sleb(self.body, 0);
                    self.body.push(0x52); // i64.ne
                }
                self.body.push(0x04); // if
                self.body.push(WASM_BLOCK_EMPTY);
                self.body.push(0x00); // unreachable
                self.body.push(0x0b); // end
            }
            InstructionData::NullAry { opcode: Opcode::Fence } => {}
            InstructionData::AtomicRmw { opcode: Opcode::AtomicRmw, op, args: [addr, val], .. } => {
                let r = res.unwrap();
                let ty = res_ty.unwrap();
                let is_64 = ty == types::I64;
                // Load old value into `r`.
                self.emit_load(addr, Offset32::new(0), r, Opcode::Load)?;
                // Push address for store, then compute new value on stack.
                self.get_val(addr)?;
                match op {
                    ir::AtomicRmwOp::Xchg => {
                        self.get_val(val)?;
                    }
                    ir::AtomicRmwOp::Add => {
                        self.get_val(r)?;
                        self.get_val(val)?;
                        self.body.push(if is_64 { 0x7c } else { 0x6a });
                    }
                    ir::AtomicRmwOp::Sub => {
                        self.get_val(r)?;
                        self.get_val(val)?;
                        self.body.push(if is_64 { 0x7d } else { 0x6b });
                    }
                    ir::AtomicRmwOp::And => {
                        self.get_val(r)?;
                        self.get_val(val)?;
                        self.body.push(if is_64 { 0x83 } else { 0x71 });
                    }
                    ir::AtomicRmwOp::Or => {
                        self.get_val(r)?;
                        self.get_val(val)?;
                        self.body.push(if is_64 { 0x84 } else { 0x72 });
                    }
                    ir::AtomicRmwOp::Xor => {
                        self.get_val(r)?;
                        self.get_val(val)?;
                        self.body.push(if is_64 { 0x85 } else { 0x73 });
                    }
                    ir::AtomicRmwOp::Nand => {
                        self.get_val(r)?;
                        self.get_val(val)?;
                        self.body.push(if is_64 { 0x83 } else { 0x71 });
                        if is_64 {
                            self.body.push(0x42);
                            write_sleb(self.body, -1);
                            self.body.push(0x85);
                        } else {
                            self.body.push(0x41);
                            write_sleb(self.body, -1);
                            self.body.push(0x73);
                        }
                    }
                    _ => return Err(format!("unsupported AtomicRmwOp {op:?}")),
                }
                let store_op = match ty {
                    types::I8 => 0x3a,
                    types::I16 => 0x3b,
                    types::I32 => 0x36,
                    types::I64 => 0x37,
                    _ => return Err(format!("unsupported atomic rmw type {ty}")),
                };
                self.body.push(store_op);
                write_uleb(self.body, 0);
                write_uleb(self.body, 0);
            }
            InstructionData::AtomicCas {
                opcode: Opcode::AtomicCas,
                args: [addr, expected, replacement],
                ..
            } => {
                let r = res.unwrap();
                let ty = res_ty.unwrap();
                self.emit_load(addr, Offset32::new(0), r, Opcode::Load)?;
                self.get_val_unsigned(r)?;
                self.get_val_unsigned(expected)?;
                self.body.push(if ty == types::I64 { 0x51 } else { 0x46 }); // eq
                self.body.push(0x04); // if
                self.body.push(WASM_BLOCK_EMPTY);
                self.emit_store(addr, replacement, Offset32::new(0), Opcode::Store)?;
                self.body.push(0x0b); // end
            }
            _ => return Err(format!("unsupported Cranelift instruction {opcode:?} in wasm backend")),
        }
        Ok(())
    }

    fn int_cmp_opcode(cond: IntCC, is_64: bool) -> u8 {
        match (cond, is_64) {
            (IntCC::Equal, false) => 0x46,
            (IntCC::Equal, true) => 0x51,
            (IntCC::NotEqual, false) => 0x47,
            (IntCC::NotEqual, true) => 0x52,
            (IntCC::SignedLessThan, false) => 0x48,
            (IntCC::SignedLessThan, true) => 0x53,
            (IntCC::UnsignedLessThan, false) => 0x49,
            (IntCC::UnsignedLessThan, true) => 0x54,
            (IntCC::SignedGreaterThan, false) => 0x4a,
            (IntCC::SignedGreaterThan, true) => 0x55,
            (IntCC::UnsignedGreaterThan, false) => 0x4b,
            (IntCC::UnsignedGreaterThan, true) => 0x56,
            (IntCC::SignedLessThanOrEqual, false) => 0x4c,
            (IntCC::SignedLessThanOrEqual, true) => 0x57,
            (IntCC::UnsignedLessThanOrEqual, false) => 0x4d,
            (IntCC::UnsignedLessThanOrEqual, true) => 0x58,
            (IntCC::SignedGreaterThanOrEqual, false) => 0x4e,
            (IntCC::SignedGreaterThanOrEqual, true) => 0x59,
            (IntCC::UnsignedGreaterThanOrEqual, false) => 0x4f,
            (IntCC::UnsignedGreaterThanOrEqual, true) => 0x5a,
        }
    }
}

/// Accumulates defined functions and data objects and serializes a relocatable Wasm `.o` file.
#[derive(Default)]
pub(super) struct WasmObjectBuilder {
    functions: FxHashMap<FuncId, (Vec<u8>, Vec<ModuleReloc>)>,
    datas: FxHashMap<DataId, ConstBlob>,
}

impl WasmObjectBuilder {
    pub(super) fn define_function(
        &mut self,
        id: FuncId,
        bytes: &[u8],
        relocs: &[ModuleReloc],
    ) {
        self.functions.insert(id, (bytes.to_vec(), relocs.to_vec()));
    }

    pub(super) fn define_data(&mut self, id: DataId, blob: ConstBlob) {
        self.datas.insert(id, blob);
    }

    pub(super) fn finish(
        self,
        decls: &ModuleDeclarations,
        is_base: bool,
        patching: bool,
    ) -> Result<Vec<u8>> {
        let map_err = |e: String| CodegenError::Backend(e);
        let emit_trampolines =
            is_base && patching && std::env::var_os("RUSTD_WASM_TRAMPOLINES").is_some();
        let emit_patch_exports = !is_base;

        let mut types: Vec<WasmSig> = Vec::new();
        let mut type_map: FxHashMap<WasmSig, u32> = FxHashMap::default();
        let intern_wsig = |wsig: WasmSig, types: &mut Vec<WasmSig>, type_map: &mut FxHashMap<WasmSig, u32>| -> u32 {
            if let Some(&idx) = type_map.get(&wsig) {
                return idx;
            }
            let idx = types.len() as u32;
            types.push(wsig.clone());
            type_map.insert(wsig, idx);
            idx
        };

        let mut imported_funcs: Vec<(FuncId, String, u32)> = Vec::new();
        let mut defined_funcs: Vec<(FuncId, String, Linkage, u32)> = Vec::new();
        for (func_id, decl) in decls.get_functions() {
            let name = decl.linkage_name(func_id).into_owned();
            if self.functions.contains_key(&func_id) {
                let wsig = WasmSig::of(&decl.signature).map_err(map_err)?;
                let tidx = intern_wsig(wsig, &mut types, &mut type_map);
                defined_funcs.push((func_id, name, decl.linkage, tidx));
            } else {
                let wsig = WasmSig::of(&decl.signature).map_err(map_err)?;
                let tidx = intern_wsig(wsig, &mut types, &mut type_map);
                imported_funcs.push((func_id, name, tidx));
            }
        }

        let mut imported_datas: Vec<(DataId, String)> = Vec::new();
        let mut defined_datas: Vec<(DataId, String, Linkage, u32, ConstBlob)> = Vec::new();
        let mut mem_cursor = if emit_patch_exports { 60 * 1024 * 1024 } else { 0u32 };
        for (data_id, decl) in decls.get_data_objects() {
            let name = decl.linkage_name(data_id).into_owned();
            if let Some(blob) = self.datas.get(&data_id) {
                let align = (blob.align as u32).max(1);
                mem_cursor = (mem_cursor + align - 1) & !(align - 1);
                let mem_offset = mem_cursor;
                mem_cursor = mem_cursor.saturating_add(blob.bytes.len() as u32);
                defined_datas.push((data_id, name, decl.linkage, mem_offset, blob.clone()));
            } else {
                imported_datas.push((data_id, name));
            }
        }

        // Symbol table layout:
        //   0: __stack_pointer (Global, undefined, element_index = 0)
        //   1..=imported_funcs.len(): imported functions (element_index = 0..imported_funcs.len())
        //   next..: defined functions (element_index = imported_funcs.len()..)
        //   (if emit_trampolines: next..: __rustd_impl_<name> functions)
        //   next..: imported datas
        //   next..: defined datas
        let mut func_sym_index: FxHashMap<FuncId, (u32, u32)> = FxHashMap::default(); // FuncId -> (sym_idx, wasm_func_idx)
        let mut data_sym_index: FxHashMap<DataId, (u32, u32)> = FxHashMap::default(); // DataId -> (sym_idx, mem_offset)
        let mut impl_sym_indices: Vec<u32> = Vec::new();

        let mut next_sym = 1u32; // 0 is __stack_pointer
        for (i, &(fid, _, _)) in imported_funcs.iter().enumerate() {
            func_sym_index.insert(fid, (next_sym, i as u32));
            next_sym += 1;
        }
        let num_func_imports = imported_funcs.len() as u32;
        for (i, &(fid, _, _, _)) in defined_funcs.iter().enumerate() {
            func_sym_index.insert(fid, (next_sym, num_func_imports + i as u32));
            next_sym += 1;
        }
        if emit_trampolines {
            for _ in 0..defined_funcs.len() {
                impl_sym_indices.push(next_sym);
                next_sym += 1;
            }
        }
        for &(did, _) in &imported_datas {
            data_sym_index.insert(did, (next_sym, 0));
            next_sym += 1;
        }
        for &(did, _, _, mem_off, _) in &defined_datas {
            data_sym_index.insert(did, (next_sym, mem_off));
            next_sym += 1;
        }

        // 1. Type section (id = 1) — built after Code section so CallIndirect types are included.

        // 2. Import section (id = 2)
        // Imports:
        //   - env.__linear_memory (memory)
        //   - env.__indirect_function_table (table)
        //   - env.__stack_pointer (global 0)
        //   - imported functions
        let mut import_sec = Vec::new();
        write_uleb(&mut import_sec, (3 + imported_funcs.len()) as u64);
        // env.__linear_memory
        write_str(&mut import_sec, "env");
        write_str(&mut import_sec, "__linear_memory");
        import_sec.push(0x02); // Memory
        import_sec.push(0x00); // limits: flags=0
        write_uleb(&mut import_sec, 1); // initial=1
        // env.__indirect_function_table
        write_str(&mut import_sec, "env");
        write_str(&mut import_sec, "__indirect_function_table");
        import_sec.push(0x01); // Table
        import_sec.push(0x70); // funcref
        import_sec.push(0x00); // flags=0
        write_uleb(&mut import_sec, 0); // initial=0
        // env.__stack_pointer
        write_str(&mut import_sec, "env");
        write_str(&mut import_sec, "__stack_pointer");
        import_sec.push(0x03); // Global
        import_sec.push(WASM_VAL_I32);
        import_sec.push(0x01); // mutable
        for (_, name, tidx) in &imported_funcs {
            write_str(&mut import_sec, "env");
            write_str(&mut import_sec, name);
            import_sec.push(0x00); // Function
            write_uleb(&mut import_sec, u64::from(*tidx));
        }

        // 3. Function section (id = 3)
        let mut func_sec = Vec::new();
        let total_defined_funcs = if emit_trampolines {
            defined_funcs.len() * 2
        } else {
            defined_funcs.len()
        };
        write_uleb(&mut func_sec, total_defined_funcs as u64);
        for &(_, _, _, tidx) in &defined_funcs {
            write_uleb(&mut func_sec, u64::from(tidx));
        }
        if emit_trampolines {
            for &(_, _, _, tidx) in &defined_funcs {
                write_uleb(&mut func_sec, u64::from(tidx));
            }
        }

        // 7. Export section (id = 7) for patch objects so `new WebAssembly.Instance` exposes patched fns.
        let mut export_sec = Vec::new();
        if emit_patch_exports {
            write_uleb(&mut export_sec, defined_funcs.len() as u64);
            for (i, (_, name, _, _)) in defined_funcs.iter().enumerate() {
                write_str(&mut export_sec, name);
                export_sec.push(0x00); // Func
                write_uleb(&mut export_sec, u64::from(num_func_imports + i as u32));
            }
        }

        let code_section_index = if emit_patch_exports { 4u32 } else { 3u32 };
        let data_section_index = code_section_index + 1;

        // 10. Code section (id = 10) + reloc.CODE entries
        // Each reloc entry: (reloc_type: u8, offset_in_code_sec: u32, index: u32, addend: Option<i64>)
        let mut code_sec = Vec::new();
        let mut code_relocs: Vec<(u8, u32, u32, Option<i64>)> = Vec::new();
        write_uleb(&mut code_sec, total_defined_funcs as u64);
        if emit_trampolines {
            for (i, &(_, _, _, tidx)) in defined_funcs.iter().enumerate() {
                let num_params = types[tidx as usize].params.len();
                let mut tramp = Vec::new();
                tramp.push(0x00); // 0 locals
                for p in 0..num_params {
                    tramp.push(0x20); // local.get
                    write_uleb(&mut tramp, p as u64);
                }
                tramp.push(0x41); // i32.const <table_slot>
                let slot_reloc_off = tramp.len() as u32;
                tramp.extend_from_slice(&[0x80, 0x80, 0x80, 0x80, 0x00]);
                tramp.push(0x11); // call_indirect
                let type_reloc_off = tramp.len() as u32;
                let mut tidx_buf = [0u8; 5];
                write_uleb_5(&mut tidx_buf, tidx);
                tramp.extend_from_slice(&tidx_buf);
                tramp.push(0x00); // table 0
                tramp.push(0x0b); // end

                let mut len_buf = Vec::new();
                write_uleb(&mut len_buf, tramp.len() as u64);
                let body_start = (code_sec.len() + len_buf.len()) as u32;
                code_relocs.push((
                    R_WASM_TABLE_INDEX_SLEB,
                    body_start + slot_reloc_off,
                    impl_sym_indices[i],
                    None,
                ));
                code_relocs.push((
                    R_WASM_TYPE_INDEX_LEB,
                    body_start + type_reloc_off,
                    tidx,
                    None,
                ));
                code_sec.extend_from_slice(&len_buf);
                code_sec.extend_from_slice(&tramp);
            }
        }
        for &(fid, _, _, _) in &defined_funcs {
            let (raw_body, f_relocs) = &self.functions[&fid];
            let mut patched_body = raw_body.clone();
            let mut body_len_buf = Vec::new();
            write_uleb(&mut body_len_buf, patched_body.len() as u64);
            let body_start_in_sec = (code_sec.len() + body_len_buf.len()) as u32;

            for reloc in f_relocs {
                let slot_off = reloc.offset as usize;
                let sec_off = body_start_in_sec + reloc.offset;
                match (reloc.kind, &reloc.name) {
                    (RELOC_SP_GLOBAL_LEB, _) => {
                        write_uleb_5(&mut patched_body[slot_off..slot_off + 5], 0);
                        code_relocs.push((R_WASM_GLOBAL_INDEX_LEB, sec_off, 0, None));
                    }
                    (RELOC_FN_LEB, &ModuleRelocTarget::User { namespace: 0, index }) => {
                        let target_fid = FuncId::from_u32(index);
                        let (sym_idx, wasm_fidx) = func_sym_index[&target_fid];
                        write_uleb_5(&mut patched_body[slot_off..slot_off + 5], wasm_fidx);
                        code_relocs.push((R_WASM_FUNCTION_INDEX_LEB, sec_off, sym_idx, None));
                    }
                    (RELOC_TABLE_SLEB, &ModuleRelocTarget::User { namespace: 0, index }) => {
                        let target_fid = FuncId::from_u32(index);
                        let (sym_idx, _) = func_sym_index[&target_fid];
                        write_sleb_5(&mut patched_body[slot_off..slot_off + 5], 0);
                        code_relocs.push((R_WASM_TABLE_INDEX_SLEB, sec_off, sym_idx, None));
                    }
                    (RELOC_MEM_SLEB, &ModuleRelocTarget::User { namespace: 1, index }) => {
                        let target_did = DataId::from_u32(index);
                        let (sym_idx, mem_off) = data_sym_index[&target_did];
                        let val = (i64::from(mem_off) + reloc.addend) as i32;
                        write_sleb_5(&mut patched_body[slot_off..slot_off + 5], val);
                        code_relocs.push((
                            R_WASM_MEMORY_ADDR_SLEB,
                            sec_off,
                            sym_idx,
                            Some(reloc.addend),
                        ));
                    }
                    (RELOC_TYPE_LEB, &ModuleRelocTarget::User { namespace: 3, index }) => {
                        let wsig = get_global_wsig(index);
                        let tidx = intern_wsig(wsig, &mut types, &mut type_map);
                        write_uleb_5(&mut patched_body[slot_off..slot_off + 5], tidx);
                        code_relocs.push((R_WASM_TYPE_INDEX_LEB, sec_off, tidx, None));
                    }
                    other => {
                        return Err(CodegenError::Backend(format!(
                            "unsupported wasm code reloc {other:?}"
                        )));
                    }
                }
            }
            code_sec.extend_from_slice(&body_len_buf);
            code_sec.extend_from_slice(&patched_body);
        }

        // 11. Data section (id = 11) + reloc.DATA entries
        let mut data_sec = Vec::new();
        let mut data_relocs: Vec<(u8, u32, u32, Option<i64>)> = Vec::new();
        if !defined_datas.is_empty() {
            write_uleb(&mut data_sec, defined_datas.len() as u64);
            for (_, _, _, mem_off, blob) in &defined_datas {
                data_sec.push(0x00); // active segment, memory 0
                data_sec.push(0x41); // i32.const
                write_sleb(&mut data_sec, i64::from(*mem_off as i32));
                data_sec.push(0x0b); // end
                let mut patched_bytes = blob.bytes.clone();
                let mut len_buf = Vec::new();
                write_uleb(&mut len_buf, patched_bytes.len() as u64);
                let payload_start = (data_sec.len() + len_buf.len()) as u32;

                for &(off, target_did, addend) in &blob.relocs {
                    let (sym_idx, target_mem_off) = data_sym_index[&target_did];
                    let slot = off as usize;
                    if slot + 4 <= patched_bytes.len() {
                        let val = (i64::from(target_mem_off) + addend) as u32;
                        patched_bytes[slot..slot + 4].copy_from_slice(&val.to_le_bytes());
                        if slot + 8 <= patched_bytes.len() {
                            patched_bytes[slot + 4..slot + 8].fill(0);
                        }
                        data_relocs.push((
                            R_WASM_MEMORY_ADDR_I32,
                            payload_start + off,
                            sym_idx,
                            Some(addend),
                        ));
                    }
                }
                for &(off, target_fid) in &blob.fn_relocs {
                    let (sym_idx, _) = func_sym_index[&target_fid];
                    let slot = off as usize;
                    if slot + 4 <= patched_bytes.len() {
                        patched_bytes[slot..slot + 4].fill(0);
                        data_relocs.push((
                            R_WASM_TABLE_INDEX_I32,
                            payload_start + off,
                            sym_idx,
                            None,
                        ));
                    }
                }
                data_sec.extend_from_slice(&len_buf);
                data_sec.extend_from_slice(&patched_bytes);
            }
        }

        // Custom section: "linking"
        let mut linking_payload = Vec::new();
        write_str(&mut linking_payload, "linking");
        write_uleb(&mut linking_payload, 2); // version 2

        if !defined_datas.is_empty() {
            // WASM_SEGMENT_INFO (subsection 5)
            let mut seg_info = Vec::new();
            write_uleb(&mut seg_info, defined_datas.len() as u64);
            for (_, name, _, _, blob) in &defined_datas {
                write_str(&mut seg_info, &format!(".rodata.{name}"));
                let p2align = blob.align.max(1).trailing_zeros();
                write_uleb(&mut seg_info, u64::from(p2align));
                write_uleb(&mut seg_info, 0); // flags
            }
            linking_payload.push(5);
            write_uleb(&mut linking_payload, seg_info.len() as u64);
            linking_payload.extend_from_slice(&seg_info);
        }

        // WASM_SYMBOL_TABLE (subsection 8)
        let mut symtab = Vec::new();
        write_uleb(&mut symtab, u64::from(next_sym));
        // Symbol 0: __stack_pointer
        symtab.push(2); // SYMTAB_GLOBAL
        write_uleb(&mut symtab, 0x10); // WASM_SYM_UNDEFINED
        write_uleb(&mut symtab, 0); // element_index = 0
        // Imported functions
        for (i, _) in imported_funcs.iter().enumerate() {
            symtab.push(0); // SYMTAB_FUNCTION
            write_uleb(&mut symtab, 0x10); // WASM_SYM_UNDEFINED
            write_uleb(&mut symtab, i as u64);
        }
        // Defined functions
        for (i, (_, name, linkage, _)) in defined_funcs.iter().enumerate() {
            symtab.push(0); // SYMTAB_FUNCTION
            let flags = match linkage {
                Linkage::Local => 0x02u64,  // WASM_SYM_BINDING_LOCAL
                Linkage::Hidden => 0x04u64, // WASM_SYM_VISIBILITY_HIDDEN
                _ => 0x00u64,               // global default
            };
            write_uleb(&mut symtab, flags);
            write_uleb(&mut symtab, (num_func_imports as usize + i) as u64);
            write_str(&mut symtab, name);
        }
        if emit_trampolines {
            let m = defined_funcs.len();
            for (i, (_, name, _, _)) in defined_funcs.iter().enumerate() {
                symtab.push(0); // SYMTAB_FUNCTION
                write_uleb(&mut symtab, 0x00u64); // global default
                write_uleb(&mut symtab, (num_func_imports as usize + m + i) as u64);
                write_str(&mut symtab, &format!("__rustd_impl_{name}"));
            }
        }
        // Imported datas
        for (_, name) in &imported_datas {
            symtab.push(1); // SYMTAB_DATA
            write_uleb(&mut symtab, 0x10); // WASM_SYM_UNDEFINED
            write_str(&mut symtab, name);
        }
        // Defined datas
        for (seg_idx, (_, name, linkage, _, blob)) in defined_datas.iter().enumerate() {
            symtab.push(1); // SYMTAB_DATA
            let flags = match linkage {
                Linkage::Local => 0x02u64,
                Linkage::Hidden => 0x04u64,
                _ => 0x00u64,
            };
            write_uleb(&mut symtab, flags);
            write_str(&mut symtab, name);
            write_uleb(&mut symtab, seg_idx as u64);
            write_uleb(&mut symtab, 0); // offset within segment
            write_uleb(&mut symtab, blob.bytes.len() as u64);
        }
        linking_payload.push(8);
        write_uleb(&mut linking_payload, symtab.len() as u64);
        linking_payload.extend_from_slice(&symtab);

        // 1. Type section (id = 1)
        let mut type_sec = Vec::new();
        write_uleb(&mut type_sec, types.len() as u64);
        for wsig in &types {
            type_sec.push(0x60);
            write_uleb(&mut type_sec, wsig.params.len() as u64);
            for p in &wsig.params {
                type_sec.push(p.byte());
            }
            write_uleb(&mut type_sec, wsig.returns.len() as u64);
            for r in &wsig.returns {
                type_sec.push(r.byte());
            }
        }

        // Assemble final Wasm `.o` file.
        let mut out = Vec::new();
        out.extend_from_slice(b"\0asm\x01\x00\x00\x00");

        let push_sec = |out: &mut Vec<u8>, id: u8, payload: &[u8]| {
            out.push(id);
            write_uleb(out, payload.len() as u64);
            out.extend_from_slice(payload);
        };
        push_sec(&mut out, 1, &type_sec);
        push_sec(&mut out, 2, &import_sec);
        push_sec(&mut out, 3, &func_sec);
        if emit_patch_exports {
            push_sec(&mut out, 7, &export_sec);
        }
        push_sec(&mut out, 10, &code_sec);
        if !defined_datas.is_empty() {
            push_sec(&mut out, 11, &data_sec);
        }
        push_sec(&mut out, 0, &linking_payload);

        let encode_reloc_sec =
            |sec_name: &str, target_sec: u32, entries: &[(u8, u32, u32, Option<i64>)]| -> Vec<u8> {
                let mut p = Vec::new();
                write_str(&mut p, sec_name);
                write_uleb(&mut p, u64::from(target_sec));
                write_uleb(&mut p, entries.len() as u64);
                for &(rty, off, idx, addend) in entries {
                    p.push(rty);
                    write_uleb(&mut p, u64::from(off));
                    write_uleb(&mut p, u64::from(idx));
                    if let Some(add) = addend {
                        write_sleb(&mut p, add);
                    }
                }
                p
            };

        if !code_relocs.is_empty() {
            let reloc_code = encode_reloc_sec("reloc.CODE", code_section_index, &code_relocs);
            push_sec(&mut out, 0, &reloc_code);
        }
        if !data_relocs.is_empty() {
            let reloc_data = encode_reloc_sec("reloc.DATA", data_section_index, &data_relocs);
            push_sec(&mut out, 0, &reloc_data);
        }

        Ok(out)
    }
}
