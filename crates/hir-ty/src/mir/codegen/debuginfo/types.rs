//! What a debugger is told of the type of a variable: [`DebugType`]s from the types of a
//! function's locals.
//!
//! A number, a `bool`, a `char`, a pointer, an array, a tuple and a struct are told in full.
//! An enum, a union, a closure and whatever else there is are told by name and size alone,
//! which a debugger shows as a value without fields.

use hir_def::{AdtId, VariantId, item_tree::FieldsShape};
use rustc_type_ir::{UintTy, inherent::IntoKind};

use super::{DebugType, Field};
use crate::{
    ParamEnvAndCrate,
    consteval::try_const_usize,
    db::HirDatabase,
    mir::type_name::rustc_type_name,
    next_solver::{
        DbInterner, Ty, TyKind, TypingMode,
        infer::{DbInternerInferExt, InferCtxt, traits::ObligationCause},
        obligation_ctxt::ObligationCtxt,
    },
};

/// `DW_ATE_*`: how the bytes of a [`DebugType::Base`] are read.
mod encoding {
    pub(super) const BOOLEAN: u8 = 0x02;
    pub(super) const FLOAT: u8 = 0x04;
    pub(super) const SIGNED: u8 = 0x05;
    pub(super) const UNSIGNED: u8 = 0x07;
    pub(super) const UTF: u8 = 0x10;
}

/// How many pointers are followed from a variable before what one points to is left untold:
/// a type may point to itself.
const POINTERS: u32 = 2;

/// The types of one function's variables, which are of its instance.
pub(in super::super) struct Types<'db> {
    db: &'db dyn HirDatabase,
    env: ParamEnvAndCrate<'db>,
    interner: DbInterner<'db>,
    infcx: InferCtxt<'db>,
}

impl<'db> Types<'db> {
    pub(in super::super) fn new(db: &'db dyn HirDatabase, env: ParamEnvAndCrate<'db>) -> Self {
        let interner = DbInterner::new_with(db, env.krate);
        let infcx = interner.infer_ctxt().build(TypingMode::PostAnalysis);
        Types { db, env, interner, infcx }
    }

    /// `None` is a type that has no layout or no name rustc would spell.
    pub(in super::super) fn describe(&self, ty: Ty<'db>) -> Option<DebugType> {
        self.ty(ty, POINTERS)
    }

    fn ty(&self, ty: Ty<'db>, pointers: u32) -> Option<DebugType> {
        let ty = self.normalize(ty);
        let layout = self.db.layout_of_ty(ty.store(), self.env.store()).ok()?;
        let name = rustc_type_name(self.db, ty)?;
        let size = layout.size.bytes();
        let base = |encoding| Some(DebugType::Base { name: name.clone(), size, encoding });
        let field = |name: String, index: usize, ty: Ty<'db>| {
            let offset = layout.fields.offset(index).bytes();
            Some(Field { name, offset, ty: self.ty(ty, pointers)? })
        };
        let fields = match ty.kind() {
            TyKind::Bool => return base(encoding::BOOLEAN),
            TyKind::Char => return base(encoding::UTF),
            TyKind::Int(_) => return base(encoding::SIGNED),
            TyKind::Uint(_) => return base(encoding::UNSIGNED),
            TyKind::Float(_) => return base(encoding::FLOAT),
            TyKind::Ref(_, to, _) | TyKind::RawPtr(to, _) => {
                return self.pointer(name, size, to, pointers);
            }
            TyKind::Array(of, len) => {
                let len = try_const_usize(self.db, len)? as u64;
                return Some(DebugType::Array { of: Box::new(self.ty(of, pointers)?), len });
            }
            TyKind::Tuple(tys) => tys
                .iter()
                .enumerate()
                .map(|(index, ty)| field(format!("__{index}"), index, ty))
                .collect::<Option<Vec<_>>>()?,
            TyKind::Adt(def, args) => match def.def_id() {
                AdtId::StructId(id) => {
                    let variant = VariantId::from(id);
                    let written = variant.fields(self.db);
                    let tuple = written.shape == FieldsShape::Tuple;
                    let mut fields = Vec::new();
                    for (id, ty) in self.db.field_types(variant).iter() {
                        let index = u32::from(id.into_raw()) as usize;
                        let name = written.fields()[id].name.as_str();
                        let name = if tuple { format!("__{name}") } else { name.to_owned() };
                        let ty = ty.ty().instantiate(self.interner, args).skip_norm_wip();
                        fields.push(field(name, index, ty)?);
                    }
                    fields
                }
                AdtId::UnionId(_) | AdtId::EnumId(_) => Vec::new(),
            },
            _ => Vec::new(),
        };
        Some(DebugType::Struct { name, size, fields })
    }

    /// A pointer of one word, or of two as the struct rustc has for it when it is to a slice
    /// or a `str`: where the first of them is and how many there are.
    fn pointer(&self, name: String, size: u64, to: Ty<'db>, pointers: u32) -> Option<DebugType> {
        let to = self.normalize(to);
        let followed = |to: Ty<'db>| match pointers.checked_sub(1) {
            Some(pointers) => self.ty(to, pointers).map(Box::new),
            None => None,
        };
        let word = self
            .db
            .layout_of_ty(Ty::new_uint(self.interner, UintTy::Usize).store(), self.env.store())
            .ok()?;
        let word = word.size.bytes();
        if size == word {
            return Some(DebugType::Pointer { name, to: followed(to) });
        }
        let element = match to.kind() {
            TyKind::Str => Ty::new_uint(self.interner, UintTy::U8),
            TyKind::Slice(element) => element,
            _ => return Some(DebugType::Struct { name, size, fields: Vec::new() }),
        };
        let first = DebugType::Pointer {
            name: format!("*const {}", rustc_type_name(self.db, element)?),
            to: followed(element),
        };
        let count =
            DebugType::Base { name: "usize".to_owned(), size: word, encoding: encoding::UNSIGNED };
        let fields = vec![
            Field { name: "data_ptr".to_owned(), offset: 0, ty: first },
            Field { name: "length".to_owned(), offset: word, ty: count },
        ];
        Some(DebugType::Struct { name, size, fields })
    }

    fn normalize(&self, ty: Ty<'db>) -> Ty<'db> {
        ObligationCtxt::new(&self.infcx)
            .structurally_normalize_ty(&ObligationCause::dummy(), self.env.param_env, ty)
            .unwrap_or(ty)
    }
}
