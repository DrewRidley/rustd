//! Two floats returned in registers by a convention Cranelift returns one float in.
//!
//! rustc returns a pair of floats in `xmm0` and `xmm1` whatever the convention, and
//! Cranelift's `windows_fastcall` has `xmm0` alone. Before Cranelift compiles a function,
//! every such return and every call of such a function is made one of a single float, and
//! two functions of the object carry the second. [`asm::join_floats`] takes the pair as its
//! arguments, which are in those registers, and returns. [`asm::split_floats`] answers
//! what is in the registers as two integers.
//!
//! Cranelift does not know the second register holds anything. Nothing may be between a
//! call and the call that splits what it returned, which [`Lowered::hold`] checks in the
//! code; a function it does not hold for is not compiled. After the pair is joined comes
//! only what restores the registers the caller owns, of which `xmm1` is none.

use cranelift_codegen::{
    cursor::{Cursor, FuncCursor},
    ir::{self, InstBuilder, MemFlagsData, Opcode, Signature, types},
    isa::CallConv,
};
use cranelift_module::{FuncId, ModuleRelocTarget};

use super::{
    CodegenError, Result, asm,
    fragment::{Code, Recorder},
};

/// What [`lower`] changed of a function, to check and put back in its code.
pub(super) struct Lowered {
    /// The signature the callers of the function know.
    signature: Signature,
    split: Option<FuncId>,
}

/// The types of the pair `signature` returns, if Cranelift cannot return it.
fn pair(signature: &Signature) -> Option<[ir::Type; 2]> {
    let [a, b] = *signature.returns.as_slice() else { return None };
    let limited = signature.call_conv == CallConv::WindowsFastcall;
    (limited && a.value_type.is_float() && b.value_type.is_float())
        .then_some([a.value_type, b.value_type])
}

/// Has `func` return and be returned one float where it is two. `None` when it has neither,
/// or is returned two by what is not a plain call, which is left for Cranelift to refuse.
pub(super) fn lower(recorder: &mut Recorder<'_>, func: &mut ir::Function) -> Option<Lowered> {
    let returned = pair(&func.signature);
    let pairs: Vec<(ir::SigRef, [ir::Type; 2])> =
        func.dfg.signatures.iter().filter_map(|(id, it)| Some((id, pair(it)?))).collect();
    if returned.is_none() && pairs.is_empty() {
        return None;
    }
    let called = |func: &ir::Function, inst| {
        let signature = func.dfg.call_signature(inst)?;
        pairs.iter().find(|(id, _)| *id == signature).map(|&(_, types)| types)
    };
    let insts: Vec<ir::Inst> =
        func.layout.blocks().flat_map(|block| func.layout.block_insts(block)).collect();
    let plain = |opcode| matches!(opcode, Opcode::Call | Opcode::CallIndirect);
    if insts.iter().any(|&it| called(func, it).is_some() && !plain(func.dfg.insts[it].opcode())) {
        return None;
    }
    let mut lowered = Lowered { signature: func.signature.clone(), split: None };
    let convention = func.signature.call_conv;
    for &(signature, _) in &pairs {
        func.dfg.signatures[signature].returns.truncate(1);
    }
    if returned.is_some() {
        func.signature.returns.truncate(1);
    }
    for inst in insts {
        if let Some([a, b]) = called(func, inst) {
            let mut split = Signature::new(convention);
            split.params.push(ir::AbiParam::new(a));
            split.returns.extend([ir::AbiParam::new(types::I64); 2]);
            let id = recorder.assembly(asm::split_floats(), &split);
            lowered.split = Some(id);
            let split = recorder.declare_func_in_func(id, func);

            let &[first, second] = func.dfg.inst_results(inst) else {
                unreachable!("the signature has two results")
            };
            func.dfg.clear_results(inst);
            func.dfg.make_inst_results(inst, types::INVALID);
            let whole = func.dfg.first_result(inst);
            let mut cursor = FuncCursor::new(func).after_inst(inst);
            let halves = cursor.ins().call(split, &[whole]);
            let &[low, high] = cursor.func.dfg.inst_results(halves) else {
                unreachable!("the signature has two results")
            };
            for (was, bits, ty) in [(first, low, a), (second, high, b)] {
                let bits = match ty {
                    types::F64 => bits,
                    _ => cursor.ins().ireduce(ty.as_int(), bits),
                };
                let float = cursor.ins().bitcast(ty, MemFlagsData::new(), bits);
                cursor.func.dfg.change_to_alias(was, float);
            }
        } else if let Some([a, b]) = returned
            && func.dfg.insts[inst].opcode() == Opcode::Return
        {
            let &[first, second] = func.dfg.inst_args(inst) else {
                unreachable!("the signature has two results")
            };
            let mut join = Signature::new(convention);
            join.params.extend([ir::AbiParam::new(a), ir::AbiParam::new(b)]);
            join.returns.push(ir::AbiParam::new(a));
            let join = recorder.assembly(asm::join_floats(), &join);
            let join = recorder.declare_func_in_func(join, func);
            let mut cursor = FuncCursor::new(func).at_inst(inst);
            let call = cursor.ins().call(join, &[first, second]);
            let whole = cursor.func.dfg.first_result(call);
            cursor.ins().return_(&[whole]);
            cursor.remove_inst();
        }
    }
    Some(lowered)
}

impl Lowered {
    /// Gives `code` the signature its callers know, once nothing in it is between a call and
    /// the call that reads the pair it returned.
    pub(super) fn hold(self, context: &cranelift_codegen::Context, code: &mut Code) -> Result<()> {
        /// `call rel32`, which is how a function of the same image is called.
        const CALL: u8 = 0xe8;
        code.signature = self.signature;
        let Some(split) = self.split else { return Ok(()) };
        let Some(compiled) = context.compiled_code() else { return Ok(()) };
        for reloc in &code.relocs {
            let ModuleRelocTarget::User { namespace: 0, index } = reloc.name else { continue };
            if index != split.as_u32() {
                continue;
            }
            let call = reloc.offset.checked_sub(1).filter(|&at| code.bytes[at as usize] == CALL);
            if !compiled.buffer.call_sites().any(|site| Some(site.ret_addr) == call) {
                return Err(CodegenError::Unsupported(
                    "two floats returned by a call that something follows".to_owned(),
                ));
            }
        }
        Ok(())
    }
}
