//! Names for instances that are the same in every run of the daemon on the same sources, so
//! that code compiled by one run can be found by the next. Only for instances of crates we do
//! not own at types we do not own: nothing an edit can change.

use std::{cell::RefCell, fmt::Write};

use hir_def::{AdtId, CallableDefId, DefWithBodyId, FunctionId, HasModule, Lookup, TraitId};
use hir_expand::{HirFileId, MacroCallKind};
use rustc_ast_ir::Mutability;
use rustc_hash::{FxHashMap, FxHashSet};
use rustc_type_ir::{
    BoundVarIndexKind, ExistentialPredicate, GenericArgKind, RegionKind,
    inherent::{IntoKind, Safety as _, Term as _},
};
use salsa::plumbing::FromId;
use span::ErasedFileAstId;

use base_db::Crate;

use super::{Instance, InstanceDef, SymbolResolver};
use crate::{
    InferBodyId,
    consteval::try_const_usize,
    db::{HirDatabase, InternedClosure, InternedClosureId, InternedCoroutineId},
    next_solver::{GenericArgs, Region, TermId, Ty, TyKind},
};

pub(super) struct StableNames<'a, 'db> {
    db: &'db dyn HirDatabase,
    symbols: &'a dyn SymbolResolver,
    /// The crates whose code changes under the daemon.
    owned: &'a FxHashSet<Crate>,
    files: RefCell<FxHashMap<HirFileId, Option<String>>>,
}

impl<'a, 'db> StableNames<'a, 'db> {
    pub(super) fn new(
        db: &'db dyn HirDatabase,
        symbols: &'a dyn SymbolResolver,
        owned: &'a FxHashSet<Crate>,
    ) -> Self {
        StableNames { db, symbols, owned, files: RefCell::default() }
    }

    /// `None` for an instance that names anything of a crate we own, or anything this has no
    /// stable name for.
    pub(super) fn instance(&self, instance: &Instance) -> Option<String> {
        let mut out = String::new();
        match instance.def {
            InstanceDef::Fn(func) => {
                out.push_str("fn ");
                self.function(&mut out, func)?;
            }
            InstanceDef::Closure(id) => {
                out.push_str("closure ");
                self.closure(&mut out, InternedClosureId::from_id(id).loc(self.db))?;
            }
            InstanceDef::Coroutine(id) => {
                out.push_str("coroutine ");
                self.closure(&mut out, InternedCoroutineId::from_id(id).loc(self.db))?;
            }
            InstanceDef::DropGlue => out.push_str("drop"),
            InstanceDef::CallOnceShim => out.push_str("call_once"),
            InstanceDef::FnPointerShim => out.push_str("fn_pointer"),
            InstanceDef::ReifyShim { receiver } => write!(out, "reify {receiver}").ok()?,
        }
        self.args(&mut out, instance.args.as_ref())?;
        Some(out)
    }

    /// An item by where it is written: names do not tell two `impl`s apart.
    fn item(
        &self,
        out: &mut String,
        krate: Crate,
        file: HirFileId,
        id: ErasedFileAstId,
    ) -> Option<()> {
        if self.owned.contains(&krate) {
            return None;
        }
        write!(out, "{:x}:", self.symbols.stable_crate_id(krate)?).ok()?;
        self.ast(out, file, id)
    }

    fn ast(&self, out: &mut String, file: HirFileId, id: ErasedFileAstId) -> Option<()> {
        if let Some(known) = self.files.borrow().get(&file) {
            out.push_str(known.as_deref()?);
        } else {
            let mut name = String::new();
            let named = self.file(&mut name, file);
            let name = named.map(|()| name);
            out.push_str(name.as_deref().unwrap_or_default());
            self.files.borrow_mut().insert(file, name);
            named?;
        }
        write!(out, "#{}", id.into_raw()).ok()
    }

    fn file(&self, out: &mut String, file: HirFileId) -> Option<()> {
        let db = self.db;
        match file {
            HirFileId::FileId(file) => {
                out.push_str(&self.symbols.source_path(file.file_id(db))?);
            }
            HirFileId::MacroFile(call) => match &call.loc(db).kind {
                MacroCallKind::FnLike { ast_id, .. } => {
                    self.ast(out, ast_id.file_id, ast_id.value.erase())?;
                    out.push('!');
                }
                MacroCallKind::Derive { ast_id, derive_attr_index, derive_index, .. } => {
                    self.ast(out, ast_id.file_id, ast_id.value.erase())?;
                    write!(out, "!derive {derive_attr_index:?} {derive_index}").ok()?;
                }
                MacroCallKind::Attr { ast_id, censored_attr_ids, .. } => {
                    self.ast(out, ast_id.file_id, ast_id.value.erase())?;
                    write!(out, "!attr {censored_attr_ids:?}").ok()?;
                }
            },
        }
        Some(())
    }

    fn function(&self, out: &mut String, func: FunctionId) -> Option<()> {
        let id = func.lookup(self.db).id;
        self.item(out, func.krate(self.db), id.file_id, id.value.erase())
    }

    fn adt(&self, out: &mut String, adt: AdtId) -> Option<()> {
        let db = self.db;
        let (file, id) = match adt {
            AdtId::StructId(it) => {
                let id = it.lookup(db).id;
                (id.file_id, id.value.erase())
            }
            AdtId::UnionId(it) => {
                let id = it.lookup(db).id;
                (id.file_id, id.value.erase())
            }
            AdtId::EnumId(it) => {
                let id = it.lookup(db).id;
                (id.file_id, id.value.erase())
            }
        };
        self.item(out, adt.krate(db), file, id)
    }

    fn trait_(&self, out: &mut String, trait_: TraitId) -> Option<()> {
        let id = trait_.lookup(self.db).id;
        self.item(out, trait_.krate(self.db), id.file_id, id.value.erase())
    }

    /// A closure or an `async` block by the body it is written in and where.
    fn closure(&self, out: &mut String, closure: InternedClosure<'db>) -> Option<()> {
        let db = self.db;
        let InferBodyId::DefWithBodyId(owner) = closure.owner else { return None };
        match owner {
            DefWithBodyId::FunctionId(it) => self.function(out, it)?,
            DefWithBodyId::ConstId(it) => {
                let id = it.lookup(db).id;
                self.item(out, it.krate(db), id.file_id, id.value.erase())?
            }
            DefWithBodyId::StaticId(it) => {
                let id = it.lookup(db).id;
                self.item(out, it.krate(db), id.file_id, id.value.erase())?
            }
            DefWithBodyId::VariantId(it) => {
                let id = it.lookup(db).id;
                self.item(out, it.krate(db), id.file_id, id.value.erase())?
            }
        }
        write!(out, "@{}", closure.expr.into_raw().into_u32()).ok()
    }

    fn args(&self, out: &mut String, args: GenericArgs<'db>) -> Option<()> {
        out.push('<');
        for arg in args.iter() {
            match arg.kind() {
                GenericArgKind::Lifetime(region) => self.region(out, region)?,
                GenericArgKind::Type(ty) => self.ty(out, ty)?,
                GenericArgKind::Const(konst) => {
                    write!(out, "{}", try_const_usize(self.db, konst)?).ok()?
                }
            }
            out.push(',');
        }
        out.push('>');
        Some(())
    }

    /// Erased, unless a binder in the type binds it.
    fn region(&self, out: &mut String, region: Region<'db>) -> Option<()> {
        match region.kind() {
            RegionKind::ReBound(BoundVarIndexKind::Bound(depth), bound) => {
                write!(out, "'{}.{}", depth.as_u32(), bound.var.as_u32()).ok()
            }
            RegionKind::ReBound(..) => None,
            _ => {
                out.push_str("'_");
                Some(())
            }
        }
    }

    fn tys(&self, out: &mut String, tys: impl IntoIterator<Item = Ty<'db>>) -> Option<()> {
        for ty in tys {
            self.ty(out, ty)?;
            out.push(',');
        }
        Some(())
    }

    fn ty(&self, out: &mut String, ty: Ty<'db>) -> Option<()> {
        let db = self.db;
        match ty.kind() {
            TyKind::Bool => out.push_str("bool"),
            TyKind::Char => out.push_str("char"),
            TyKind::Str => out.push_str("str"),
            TyKind::Never => out.push('!'),
            TyKind::Int(it) => out.push_str(it.name_str()),
            TyKind::Uint(it) => out.push_str(it.name_str()),
            TyKind::Float(it) => out.push_str(it.name_str()),
            TyKind::Ref(region, pointee, mutability) => {
                out.push('&');
                self.region(out, region)?;
                out.push_str(match mutability {
                    Mutability::Not => " ",
                    Mutability::Mut => " mut ",
                });
                self.ty(out, pointee)?;
            }
            TyKind::RawPtr(pointee, mutability) => {
                out.push_str(match mutability {
                    Mutability::Not => "*const ",
                    Mutability::Mut => "*mut ",
                });
                self.ty(out, pointee)?;
            }
            TyKind::Slice(element) => {
                out.push('[');
                self.ty(out, element)?;
                out.push(']');
            }
            TyKind::Array(element, len) => {
                out.push('[');
                self.ty(out, element)?;
                write!(out, ";{}]", try_const_usize(db, len)?).ok()?;
            }
            TyKind::Tuple(tys) => {
                out.push('(');
                self.tys(out, tys.iter())?;
                out.push(')');
            }
            TyKind::Adt(adt, args) => {
                out.push_str("adt ");
                self.adt(out, adt.def_id())?;
                self.args(out, args)?;
            }
            TyKind::FnDef(def, args) => {
                match def.0 {
                    CallableDefId::FunctionId(it) => {
                        out.push_str("fn ");
                        self.function(out, it)?;
                    }
                    CallableDefId::StructId(it) => {
                        out.push_str("ctor ");
                        self.adt(out, it.into())?;
                    }
                    CallableDefId::EnumVariantId(it) => {
                        out.push_str("ctor ");
                        let id = it.lookup(db).id;
                        self.item(out, it.krate(db), id.file_id, id.value.erase())?;
                    }
                }
                self.args(out, args)?;
            }
            TyKind::FnPtr(signature, header) => {
                write!(
                    out,
                    "fnptr {:?} {} {} (",
                    header.abi(),
                    header.safety().is_safe(),
                    header.c_variadic()
                )
                .ok()?;
                self.tys(out, signature.skip_binder().inputs_and_output.iter())?;
                out.push(')');
            }
            TyKind::Closure(id, args) => {
                out.push_str("closure ");
                self.closure(out, id.0.loc(db))?;
                self.args(out, args)?;
            }
            TyKind::Coroutine(id, args) => {
                out.push_str("coroutine ");
                self.closure(out, id.0.loc(db))?;
                self.args(out, args)?;
            }
            TyKind::Dynamic(predicates, region) => {
                // In an order of their own: the list is sorted by ids of this run.
                let mut written = Vec::new();
                for predicate in predicates.iter() {
                    let mut out = String::new();
                    match predicate.skip_binder() {
                        ExistentialPredicate::Trait(it) => {
                            out.push_str("trait ");
                            self.trait_(&mut out, it.def_id.0)?;
                            self.args(&mut out, it.args)?;
                        }
                        ExistentialPredicate::Projection(it) => {
                            let TermId::TypeAliasId(alias) = it.def_id.0 else { return None };
                            out.push_str("type ");
                            let id = alias.lookup(db).id;
                            self.item(&mut out, alias.krate(db), id.file_id, id.value.erase())?;
                            self.args(&mut out, it.args)?;
                            out.push('=');
                            self.ty(&mut out, it.term.as_type()?)?;
                        }
                        ExistentialPredicate::AutoTrait(it) => {
                            out.push_str("auto ");
                            self.trait_(&mut out, it.0)?;
                        }
                    }
                    written.push(out);
                }
                written.sort();
                out.push_str("dyn (");
                for it in written {
                    out.push_str(&it);
                    out.push('+');
                }
                self.region(out, region)?;
                out.push(')');
            }
            _ => return None,
        }
        Some(())
    }
}
