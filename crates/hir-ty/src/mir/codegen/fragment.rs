//! What compiling one function gives: machine code that names the functions and data it uses
//! by what they are, not by where an object file put them. That makes the result a function of
//! the database alone, so it is a query, and a build stitches the results into an object.

use std::{cell::RefCell, iter};

use base_db::FileId;
use cranelift_codegen::{
    ir::{self, Signature},
    isa::TargetIsa,
};
use cranelift_frontend::FunctionBuilderContext;
use cranelift_module::{DataId, FuncId, ModuleReloc, ModuleRelocTarget};
use hir_def::{DefWithBodyId, FunctionId, StaticId, TraitId, hir::BindingId};
use rustc_hash::FxHashMap;
use rustc_type_ir::inherent::IntoKind;

use crate::{
    ParamEnvAndCrate,
    db::HirDatabase,
    mir::MirSpan,
    next_solver::{DbInterner, GenericArgs, Region, StoredGenericArgs, StoredTy, Ty},
    traits::StoredParamEnvAndCrate,
};

use super::{
    CodegenError, ConstBlob, DataIdentity, Instance, InstanceDef, Result, Target,
    debuginfo::LineRow, unwind::FunctionUnwind,
};

/// A function that compiled code calls or takes the address of.
#[derive(Clone, PartialEq, Eq, Hash)]
pub(super) enum FuncTarget {
    /// A symbol by its link name.
    Symbol(String),
    /// A rustc symbol by its demangled path.
    Path(String),
    /// Whatever stands for the instance in the build: rustc's copy or ours.
    Instance(Instance, StoredParamEnvAndCrate),
    /// The function an `asm!` block is assembled as, by its assembly.
    Asm(String),
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub(super) struct FuncRequest {
    pub(super) target: FuncTarget,
    pub(super) sig: Signature,
}

/// A data object that compiled code or other data points to. The ids inside a blob index the
/// requests of the same [`Requests`].
#[derive(PartialEq, Eq)]
pub(super) enum DataRequest {
    Blob {
        blob: ConstBlob,
        identity: DataIdentity,
    },
    /// One object for the key in the whole build, whichever function asks first.
    Vtable {
        ty: StoredTy,
        trait_ref: Option<(TraitId, StoredGenericArgs)>,
        blob: ConstBlob,
    },
    Static(StaticId),
    /// A `core::panic::Location` for a place in a body. Looked up when stitching, so that code
    /// does not change when lines above it are added.
    Location(Option<(DefWithBodyId, MirSpan)>),
    /// A `core::panic::Location` of where a function is declared: what a pointer to a
    /// `#[track_caller]` function passes it.
    Declaration(FunctionId),
    /// The path of a file as rustc reports it, without a terminator.
    FilePath(FileId),
}

/// Everything a compiled function or static refers to. `FuncId` and `DataId` index these
/// until the stitcher replaces them with the ids of its module.
#[derive(Default, PartialEq, Eq)]
pub(super) struct Requests {
    pub(super) funcs: Vec<FuncRequest>,
    pub(super) datas: Vec<DataRequest>,
}

pub(super) struct Code {
    pub(super) signature: Signature,
    pub(super) bytes: Vec<u8>,
    pub(super) align: u64,
    pub(super) relocs: Vec<ModuleReloc>,
    pub(super) unwind: Option<FunctionUnwind>,
    /// Which statement the code is of, from each offset where that changes.
    pub(super) lines: Vec<LineRow>,
}

impl PartialEq for Code {
    fn eq(&self, other: &Code) -> bool {
        let same_target = |a: &ModuleRelocTarget, b: &ModuleRelocTarget| match (a, b) {
            (
                ModuleRelocTarget::User { namespace, index },
                ModuleRelocTarget::User { namespace: other_namespace, index: other_index },
            ) => namespace == other_namespace && index == other_index,
            (ModuleRelocTarget::LibCall(a), ModuleRelocTarget::LibCall(b)) => a == b,
            (ModuleRelocTarget::KnownSymbol(a), ModuleRelocTarget::KnownSymbol(b)) => a == b,
            (
                ModuleRelocTarget::FunctionOffset(func, offset),
                ModuleRelocTarget::FunctionOffset(other_func, other_offset),
            ) => func == other_func && offset == other_offset,
            _ => false,
        };
        self.bytes == other.bytes
            && self.align == other.align
            && self.signature == other.signature
            && self.unwind == other.unwind
            && self.lines == other.lines
            && self.relocs.len() == other.relocs.len()
            && iter::zip(&self.relocs, &other.relocs).all(|(a, b)| {
                a.offset == b.offset
                    && a.kind == b.kind
                    && a.addend == b.addend
                    && same_target(&a.name, &b.name)
            })
    }
}

/// Compared by the database when a function is compiled again: one that came out the same
/// leaves what was built from it alone.
#[derive(PartialEq)]
pub struct CompiledFunction {
    pub(super) requests: Requests,
    pub(super) code: Code,
    /// The body the function was compiled from and the statements of it that
    /// [`Code::lines`] index. Looked up when stitching, like a [`DataRequest::Location`].
    pub(super) source: Option<(DefWithBodyId, Vec<MirSpan>)>,
    /// The named locals of the body a debugger can be told of, whose types are of `.0`.
    pub(super) variables: Option<(StoredParamEnvAndCrate, Vec<Variable>)>,
    /// Size and alignment of every type laid out, collected when `RUSTD_LAYOUTS` is set.
    pub(super) layouts: Vec<(String, u64, u64)>,
}

/// A local written with a name that is kept in one slot of the frame for as long as the
/// function runs.
#[derive(PartialEq)]
pub(super) struct Variable {
    pub(super) binding: BindingId,
    pub(super) param: bool,
    pub(super) ty: StoredTy,
    /// Where the slot is from the frame pointer.
    pub(super) offset: i64,
}

/// The value of a static rustc exports no symbol for.
#[derive(PartialEq)]
pub struct CompiledStatic {
    pub(super) requests: Requests,
    pub(super) blob: ConstBlob,
    pub(super) writable: bool,
}

/// How many requests a [`Recorder`] had at some point, to tell the ones made since.
#[derive(Clone, Copy)]
#[cfg_attr(not(feature = "llvm"), expect(dead_code))]
pub(super) struct Mark {
    pub(super) funcs: usize,
    pub(super) datas: usize,
}

/// Collects the requests of one function while its IR is built.
pub(super) struct Recorder<'db> {
    pub(super) db: &'db dyn HirDatabase,
    pub(super) isa: &'static dyn TargetIsa,
    /// What `isa` generates code for.
    pub(super) target: &'static Target,
    requests: Requests,
    funcs: FxHashMap<FuncRequest, FuncId>,
    shared: FxHashMap<ConstBlob, DataId>,
    vtables: FxHashMap<(StoredTy, Option<(TraitId, StoredGenericArgs)>), DataId>,
    statics: FxHashMap<StaticId, DataId>,
    pub(super) layouts: Option<RefCell<FxHashMap<Ty<'db>, (u64, u64)>>>,
    pub(super) builder_ctx: FunctionBuilderContext,
    /// The assembly of the function, when it is a `#[naked]` one.
    pub(super) naked: Option<String>,
    /// The statements instructions were built for, which their source locations index.
    spans: Vec<MirSpan>,
    span_ids: FxHashMap<MirSpan, u32>,
    /// The locals written with a name that have a stack slot of their own, and whether each
    /// is a parameter.
    pub(super) named: Vec<(BindingId, bool, StoredTy, ir::StackSlot)>,
}

impl<'db> Recorder<'db> {
    pub(super) fn new(
        db: &'db dyn HirDatabase,
        isa: &'static dyn TargetIsa,
        target: &'static Target,
    ) -> Self {
        Recorder {
            db,
            isa,
            target,
            requests: Requests::default(),
            funcs: FxHashMap::default(),
            shared: FxHashMap::default(),
            vtables: FxHashMap::default(),
            statics: FxHashMap::default(),
            layouts: std::env::var_os("RUSTD_LAYOUTS").map(|_| Default::default()),
            builder_ctx: FunctionBuilderContext::new(),
            naked: None,
            spans: Vec::new(),
            span_ids: FxHashMap::default(),
            named: Vec::new(),
        }
    }

    /// What marks an instruction as built for the statement at `span`.
    pub(super) fn source_loc(&mut self, span: MirSpan) -> ir::SourceLoc {
        if span == MirSpan::Unknown {
            return ir::SourceLoc::default();
        }
        let next = self.spans.len() as u32;
        let id = *self.span_ids.entry(span).or_insert(next);
        if id == next {
            self.spans.push(span);
        }
        LineRow::source_loc(id)
    }

    /// The statements that [`Self::source_loc`] gave marks for, in the order of the marks.
    pub(super) fn take_spans(&mut self) -> Vec<MirSpan> {
        std::mem::take(&mut self.spans)
    }

    pub(super) fn finish(self) -> (Requests, Vec<(String, u64, u64)>) {
        let layouts = self.layouts.map_or_else(Vec::new, |layouts| {
            layouts
                .into_inner()
                .into_iter()
                .map(|(ty, (size, align))| (format!("{ty:?}"), size, align))
                .collect()
        });
        (self.requests, layouts)
    }

    #[cfg(feature = "llvm")]
    pub(super) fn mark(&self) -> Mark {
        Mark { funcs: self.requests.funcs.len(), datas: self.requests.datas.len() }
    }

    /// The requests made since `mark`.
    #[cfg(feature = "llvm")]
    pub(super) fn since(&self, mark: Mark) -> (&[FuncRequest], &[DataRequest]) {
        (&self.requests.funcs[mark.funcs..], &self.requests.datas[mark.datas..])
    }

    fn func(&mut self, target: FuncTarget, sig: &Signature) -> FuncId {
        let request = FuncRequest { target, sig: sig.clone() };
        if let Some(&id) = self.funcs.get(&request) {
            return id;
        }
        let id = FuncId::from_u32(self.requests.funcs.len() as u32);
        self.requests.funcs.push(request.clone());
        self.funcs.insert(request, id);
        id
    }

    fn data(&mut self, request: DataRequest) -> DataId {
        let id = DataId::from_u32(self.requests.datas.len() as u32);
        self.requests.datas.push(request);
        id
    }

    /// The function to call for `def` instantiated with `args`.
    pub(super) fn callee(
        &mut self,
        def: InstanceDef,
        args: GenericArgs<'db>,
        env: ParamEnvAndCrate<'db>,
        sig: &Signature,
    ) -> Result<FuncId> {
        // Lifetimes change nothing about the code, and the ones a caller passes on are its
        // own parameters, which mean nothing in the callee.
        let interner = DbInterner::new_with(self.db, env.krate);
        let args = rustc_type_ir::fold_regions(interner, args, |region, _| match region.kind() {
            rustc_type_ir::RegionKind::ReBound(..) => region,
            _ => Region::new_erased(interner),
        });
        let instance = Instance { def, args: args.store() };
        Ok(self.func(FuncTarget::Instance(instance, env.store()), sig))
    }

    pub(super) fn import(&mut self, symbol: &str, sig: &Signature) -> Result<FuncId> {
        Ok(self.func(FuncTarget::Symbol(symbol.to_owned()), sig))
    }

    pub(super) fn import_path(&mut self, path: &str, sig: &Signature) -> FuncId {
        self.func(FuncTarget::Path(path.to_owned()), sig)
    }

    pub(super) fn assembly(&mut self, text: String, sig: &Signature) -> FuncId {
        self.func(FuncTarget::Asm(text), sig)
    }

    pub(super) fn define_const(&mut self, blob: ConstBlob) -> Result<DataId> {
        self.define_data(blob, false)
    }

    pub(super) fn define_data(&mut self, blob: ConstBlob, writable: bool) -> Result<DataId> {
        let identity = if writable { DataIdentity::Writable } else { DataIdentity::Shared };
        Ok(self.define_data_as(blob, identity))
    }

    pub(super) fn define_data_as(&mut self, blob: ConstBlob, identity: DataIdentity) -> DataId {
        if identity != DataIdentity::Shared {
            return self.data(DataRequest::Blob { blob, identity });
        }
        if let Some(&id) = self.shared.get(&blob) {
            return id;
        }
        let id = self.data(DataRequest::Blob { blob: blob.clone(), identity });
        self.shared.insert(blob, id);
        id
    }

    pub(super) fn vtable(
        &self,
        key: &(StoredTy, Option<(TraitId, StoredGenericArgs)>),
    ) -> Option<DataId> {
        self.vtables.get(key).copied()
    }

    pub(super) fn define_vtable(
        &mut self,
        key: (StoredTy, Option<(TraitId, StoredGenericArgs)>),
        blob: ConstBlob,
    ) -> DataId {
        let (ty, trait_ref) = key.clone();
        let id = self.data(DataRequest::Vtable { ty, trait_ref, blob });
        self.vtables.insert(key, id);
        id
    }

    pub(super) fn static_data(&mut self, id: StaticId) -> DataId {
        if let Some(&data) = self.statics.get(&id) {
            return data;
        }
        let data = self.data(DataRequest::Static(id));
        self.statics.insert(id, data);
        data
    }

    pub(super) fn location(&mut self, at: Option<(DefWithBodyId, MirSpan)>) -> DataId {
        self.data(DataRequest::Location(at))
    }

    pub(super) fn declaration(&mut self, func: FunctionId) -> DataId {
        self.data(DataRequest::Declaration(func))
    }

    pub(super) fn file_path(&mut self, file: FileId) -> DataId {
        self.data(DataRequest::FilePath(file))
    }

    pub(super) fn make_signature(&self) -> Signature {
        Signature::new(self.isa.default_call_conv())
    }

    pub(super) fn target_config(&self) -> cranelift_codegen::isa::TargetFrontendConfig {
        let config = self.isa.frontend_config();
        // The ISA is only there for its calling convention: WebAssembly's pointers are 32 bits.
        #[cfg(feature = "wasm")]
        let config = cranelift_codegen::isa::TargetFrontendConfig {
            pointer_width: target_lexicon::PointerWidth::U32,
            ..config
        };
        config
    }

    /// Whether what another object defines is reached as what this one does: the linker of
    /// a PE image puts a stub where an import is called, and there is no other way.
    fn within_image(&self) -> bool {
        self.isa.triple().binary_format == target_lexicon::BinaryFormat::Coff
    }

    /// Names a requested function in `func`. Whether the function ends up in this object is
    /// not known here, so it is reached the way a function of another object is.
    pub(super) fn declare_func_in_func(&self, id: FuncId, func: &mut ir::Function) -> ir::FuncRef {
        let signature =
            func.import_signature(self.requests.funcs[id.as_u32() as usize].sig.clone());
        let name = func.declare_imported_user_function(ir::UserExternalName {
            namespace: 0,
            index: id.as_u32(),
        });
        func.import_function(ir::ExtFuncData {
            name: ir::ExternalName::user(name),
            signature,
            colocated: self.within_image(),
            patchable: false,
        })
    }

    pub(super) fn declare_data_in_func(
        &self,
        id: DataId,
        func: &mut ir::Function,
    ) -> ir::GlobalValue {
        let request = &self.requests.datas[id.as_u32() as usize];
        let tls = match request {
            DataRequest::Static(id) => super::is_thread_local(self.db, *id),
            _ => false,
        };
        let name = func.declare_imported_user_function(ir::UserExternalName {
            namespace: 1,
            index: id.as_u32(),
        });
        func.create_global_value(ir::GlobalValueData::Symbol {
            name: ir::ExternalName::user(name),
            offset: ir::immediates::Imm64::new(0),
            // Only a static can be another object's.
            colocated: self.within_image() || !matches!(request, DataRequest::Static(_)),
            tls,
        })
    }
}

impl From<cranelift_module::ModuleError> for CodegenError {
    fn from(e: cranelift_module::ModuleError) -> Self {
        CodegenError::Backend(e.to_string())
    }
}
