//! `type_name` as rustc spells it: full paths from the crate root, no lifetimes, and no
//! generic arguments that only repeat a default.

use std::fmt::Write;

use hir_def::{
    AdtId, CallableDefId, DefWithBodyId, FunctionId, HasModule, ItemContainerId, Lookup,
    ModuleDefId, ModuleIdLt,
    expr_store::Body,
    hir::{Expr, ExprId},
    signatures::{
        EnumSignature, FunctionSignature, StructSignature, TraitSignature, UnionSignature,
    },
};
use rustc_ast_ir::Mutability;
use rustc_type_ir::inherent::{BoundExistentialPredicates as _, GenericArgs as _, IntoKind};

use crate::{
    InferBodyId,
    consteval::try_const_usize,
    db::{HirDatabase, InternedClosure},
    generics::generics,
    next_solver::{DbInterner, GenericArg, TraitRef, Ty, TyKind},
};

/// `None` is a type whose rustc spelling this does not reproduce.
pub(crate) fn rustc_type_name<'db>(db: &'db dyn HirDatabase, ty: Ty<'db>) -> Option<String> {
    let mut name = String::new();
    write_ty(db, &mut name, ty)?;
    Some(name)
}

fn write_module<'db>(
    db: &'db dyn HirDatabase,
    out: &mut String,
    module: ModuleIdLt<'db>,
) -> Option<()> {
    // rustc names an item in a body after the function the body is of, and the closures of
    // it the item is written in.
    if module.is_block_module(db) {
        let block = module.block(db)?;
        let mut around = block.module(db);
        // FIXME: a method's body is named after the `impl`, which is not looked through here.
        let owner = loop {
            let scope = around.def_map(db).module(db, around).scope(db);
            let owner = scope.declarations().find_map(|def| match def {
                ModuleDefId::FunctionId(it) => {
                    Body::of(db, it.into()).blocks(db).any(|(id, _)| id == block).then_some(it)
                }
                _ => None,
            });
            match owner {
                Some(owner) => break owner,
                // A block in a block, of the same body.
                None => around = around.block(db)?.module(db),
            }
        };
        let body = Body::of(db, owner.into());
        let (at, _) = body
            .exprs()
            .find(|(_, expr)| matches!(expr, Expr::Block { id: Some(id), .. } if *id == block))?;
        write_module(db, out, around)?;
        write!(out, "::{}", FunctionSignature::of(db, owner).name.as_str()).ok()?;
        for _ in 0..closures_around(body, body.root_expr(), at)? {
            out.push_str("::{{closure}}");
        }
        return Some(());
    }
    match module.containing_module(db) {
        Some(parent) => {
            write_module(db, out, parent)?;
            write!(out, "::{}", module.name(db)?.as_str()).ok()
        }
        None => {
            let krate = module.krate(db);
            let name = krate.extra_data(db).display_name.as_ref()?;
            out.push_str(name.crate_name().as_str());
            Some(())
        }
    }
}

fn write_list<'db>(
    db: &'db dyn HirDatabase,
    out: &mut String,
    tys: impl IntoIterator<Item = Ty<'db>>,
) -> Option<()> {
    for (i, ty) in tys.into_iter().enumerate() {
        if i != 0 {
            out.push_str(", ");
        }
        write_ty(db, out, ty)?;
    }
    Some(())
}

fn write_ty<'db>(db: &'db dyn HirDatabase, out: &mut String, ty: Ty<'db>) -> Option<()> {
    match ty.kind() {
        TyKind::Bool => out.push_str("bool"),
        TyKind::Char => out.push_str("char"),
        TyKind::Str => out.push_str("str"),
        TyKind::Never => out.push('!'),
        TyKind::Int(it) => out.push_str(it.name_str()),
        TyKind::Uint(it) => out.push_str(it.name_str()),
        TyKind::Float(it) => out.push_str(it.name_str()),
        TyKind::Ref(_, pointee, mutability) => {
            out.push_str(match mutability {
                Mutability::Not => "&",
                Mutability::Mut => "&mut ",
            });
            write_ty(db, out, pointee)?;
        }
        TyKind::RawPtr(pointee, mutability) => {
            out.push_str(match mutability {
                Mutability::Not => "*const ",
                Mutability::Mut => "*mut ",
            });
            write_ty(db, out, pointee)?;
        }
        TyKind::Slice(element) => {
            out.push('[');
            write_ty(db, out, element)?;
            out.push(']');
        }
        TyKind::Array(element, len) => {
            out.push('[');
            write_ty(db, out, element)?;
            write!(out, "; {}]", try_const_usize(db, len)?).ok()?;
        }
        TyKind::Tuple(tys) => {
            out.push('(');
            write_list(db, out, tys.iter())?;
            out.push_str(if tys.len() == 1 { ",)" } else { ")" });
        }
        TyKind::Adt(adt, args) => {
            let adt = adt.def_id();
            let name = match adt {
                AdtId::StructId(it) => &StructSignature::of(db, it).name,
                AdtId::UnionId(it) => &UnionSignature::of(db, it).name,
                AdtId::EnumId(it) => &EnumSignature::of(db, it).name,
            };
            write_module(db, out, adt.module(db))?;
            write!(out, "::{}", name.as_str()).ok()?;
            let interner = DbInterner::new_no_crate(db);
            let defaults = db.generic_defaults(adt.into());
            let mut args = args.as_slice();
            while let Some((last, rest)) = args.split_last()
                && let Some(default) = defaults.get(rest.len())
                && default.instantiate(interner, rest).skip_norm_wip() == *last
            {
                args = rest;
            }
            write_args(db, out, args)?;
        }
        TyKind::Dynamic(bounds, _) => {
            let principal = bounds.principal()?.skip_binder();
            if !principal.args.is_empty() || bounds.projection_bounds().into_iter().count() != 0 {
                return None;
            }
            out.push_str("dyn ");
            let trait_ = principal.def_id.0;
            write_module(db, out, trait_.module(db))?;
            write!(out, "::{}", TraitSignature::of(db, trait_).name.as_str()).ok()?;
            for auto in bounds.auto_traits() {
                out.push_str(" + ");
                write_module(db, out, auto.0.module(db))?;
                write!(out, "::{}", TraitSignature::of(db, auto.0).name.as_str()).ok()?;
            }
        }
        TyKind::FnDef(def, args) => {
            let CallableDefId::FunctionId(func) = def.0 else { return None };
            write_function(db, out, func, args.as_slice())?;
        }
        TyKind::Closure(id, args) => {
            let InternedClosure { owner, expr, .. } = id.0.loc(db);
            let InferBodyId::DefWithBodyId(owner @ DefWithBodyId::FunctionId(func)) = owner else {
                return None;
            };
            write_function(db, out, func, args.as_closure().parent_args())?;
            let body = Body::of(db, owner);
            for _ in 0..=closures_around(body, body.root_expr(), expr)? {
                out.push_str("::{{closure}}");
            }
        }
        _ => return None,
    }
    Some(())
}

/// `<A, B>`, or nothing for no arguments. rustc writes every lifetime as `'_`.
fn write_args<'db>(
    db: &'db dyn HirDatabase,
    out: &mut String,
    args: &[GenericArg<'db>],
) -> Option<()> {
    for (i, arg) in args.iter().enumerate() {
        out.push_str(if i == 0 { "<" } else { ", " });
        match (arg.ty(), arg.konst()) {
            (Some(ty), _) => write_ty(db, out, ty)?,
            (_, Some(konst)) => write!(out, "{}", try_const_usize(db, konst)?).ok()?,
            _ => out.push_str("'_"),
        }
    }
    if !args.is_empty() {
        out.push('>');
    }
    Some(())
}

/// The module of the item rustc takes a type to be about, when it has one.
fn characteristic_module<'db>(db: &'db dyn HirDatabase, ty: Ty<'db>) -> Option<ModuleIdLt<'db>> {
    match ty.kind() {
        TyKind::Adt(adt, _) => Some(adt.def_id().module(db)),
        TyKind::Dynamic(bounds, _) => Some(bounds.principal()?.skip_binder().def_id.0.module(db)),
        TyKind::Ref(_, inner, _)
        | TyKind::RawPtr(inner, _)
        | TyKind::Slice(inner)
        | TyKind::Array(inner, _) => characteristic_module(db, inner),
        TyKind::Tuple(tys) => tys.iter().find_map(|ty| characteristic_module(db, ty)),
        // FIXME: a function or a closure is about itself, in what it is written in.
        _ => None,
    }
}

fn write_trait<'db>(
    db: &'db dyn HirDatabase,
    out: &mut String,
    trait_ref: TraitRef<'db>,
) -> Option<()> {
    let trait_ = trait_ref.def_id.0;
    write_module(db, out, trait_.module(db))?;
    write!(out, "::{}", TraitSignature::of(db, trait_).name.as_str()).ok()?;
    // FIXME: rustc leaves out the arguments that repeat a default.
    write_args(db, out, trait_ref.args.as_slice().get(1..)?)
}

/// A function with its generic arguments: `path::f<A>`, and for one of an `impl`
/// `path::Type<T>::f`, `<T as Trait>::f`, or `path::<impl Trait for T>::f` when the `impl` is
/// in the module of neither the type nor the trait.
fn write_function<'db>(
    db: &'db dyn HirDatabase,
    out: &mut String,
    func: FunctionId,
    args: &[GenericArg<'db>],
) -> Option<()> {
    let (parent_args, own_args) = args.split_at_checked(generics(db, func.into()).len_parent())?;
    // A lifetime of a function is mostly late bound, and then no argument to rustc.
    if own_args.iter().any(|arg| arg.region().is_some()) {
        return None;
    }
    match func.lookup(db).container {
        ItemContainerId::ModuleId(module) => write_module(db, out, module)?,
        ItemContainerId::ImplId(impl_) => {
            let interner = DbInterner::new_no_crate(db);
            let self_ty = db.impl_self_ty(impl_).instantiate(interner, parent_args).skip_norm_wip();
            let trait_ref = db
                .impl_trait(impl_)
                .map(|it| it.instantiate(interner, parent_args).skip_norm_wip());
            let module = impl_.lookup(db).container;
            let in_self_module = characteristic_module(db, self_ty) == Some(module);
            let in_trait_module = trait_ref.is_some_and(|it| it.def_id.0.module(db) == module);
            if !in_self_module && !in_trait_module {
                write_module(db, out, module)?;
                out.push_str("::<impl ");
                if let Some(trait_ref) = trait_ref {
                    write_trait(db, out, trait_ref)?;
                    out.push_str(" for ");
                }
                write_ty(db, out, self_ty)?;
                out.push('>');
            } else if let Some(trait_ref) = trait_ref {
                out.push('<');
                write_ty(db, out, self_ty)?;
                out.push_str(" as ");
                write_trait(db, out, trait_ref)?;
                out.push('>');
            } else if matches!(
                self_ty.kind(),
                TyKind::Adt(..)
                    | TyKind::Bool
                    | TyKind::Char
                    | TyKind::Str
                    | TyKind::Int(_)
                    | TyKind::Uint(_)
                    | TyKind::Float(_)
            ) {
                write_ty(db, out, self_ty)?;
            } else {
                out.push('<');
                write_ty(db, out, self_ty)?;
                out.push('>');
            }
        }
        // FIXME: a provided method is `<T as Trait>::f`.
        ItemContainerId::TraitId(_) | ItemContainerId::ExternBlockId(_) => return None,
    }
    write!(out, "::{}", FunctionSignature::of(db, func).name.as_str()).ok()?;
    write_args(db, out, own_args)
}

/// How many closures and `async` blocks `target` is written in, looking for it from `at` down.
fn closures_around(body: &Body, at: ExprId, target: ExprId) -> Option<usize> {
    if at == target {
        return Some(0);
    }
    let mut found = None;
    body.walk_child_exprs(at, |child| {
        found = found.or_else(|| closures_around(body, child, target));
    });
    let nests = matches!(body[at], Expr::Closure { .. });
    found.map(|depth| depth + usize::from(nests))
}
