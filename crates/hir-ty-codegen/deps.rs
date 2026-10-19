//! What a body reaches, for telling which tests an edit can change the outcome of: the
//! functions it calls or names, the types its values have, and the statics and constants it
//! reads. It is read from inference, which every body has, also one MIR is not lowered for.
//!
//! Nothing here is precise in the other direction: a body reaches at least this, and the
//! caller adds what dispatch and drops can reach from it.

use hir_def::{
    AdtId, CallableDefId, DefWithBodyId, FunctionId, ImplId, ItemContainerId, Lookup, VariantId,
    expr_store::Body,
    hir::{Expr, Pat},
    resolver::{HasResolver, ValueNs},
};
use rustc_hash::FxHashSet;
use rustc_type_ir::{TypeSuperVisitable, TypeVisitable, TypeVisitor, inherent::IntoKind};

use crate::{
    InferBodyId, InferenceResult,
    db::HirDatabase,
    method_resolution::CandidateId,
    next_solver::{DbInterner, GenericArgs, Ty, TyKind},
};

/// What one body reaches directly.
#[derive(Default, PartialEq, Eq, Debug)]
pub struct BodyDeps {
    /// The functions it calls or names, a trait's methods as the trait declares them.
    pub callees: FxHashSet<FunctionId>,
    /// Each trait method it calls, with the type it is called on when that is a named type:
    /// the impl of that type is the one that runs. `None` is any.
    pub trait_calls: FxHashSet<(FunctionId, Option<AdtId>)>,
    /// The types its values, and the types inside those, are of.
    pub adts: FxHashSet<AdtId>,
    /// The other bodies it reads: statics and constants it names, and the functions whose
    /// closures and `async` blocks it has values of.
    pub bodies: FxHashSet<DefWithBodyId>,
}

/// What the body `def` reaches directly.
#[salsa::tracked(returns(ref))]
pub fn body_deps(db: &dyn HirDatabase, def: DefWithBodyId) -> BodyDeps {
    let infer = InferenceResult::of(db, def);
    let mut deps = BodyDeps::default();
    let mut seen = Seen { db, deps: &mut deps };
    for (expr, ty) in infer.type_of_expr.iter() {
        ty.as_ref().visit_with(&mut seen);
        if let Some((func, args)) = infer.method_resolution(expr) {
            seen.called(func, args);
        }
        if let Some((CandidateId::FunctionId(func), args)) = infer.assoc_resolutions_for_expr(expr)
        {
            seen.called(func, args);
        }
    }
    for (pat, ty) in infer.type_of_pat.iter() {
        ty.as_ref().visit_with(&mut seen);
        if let Some((CandidateId::FunctionId(func), args)) = infer.assoc_resolutions_for_pat(pat) {
            seen.called(func, args);
        }
    }
    for (_, ty) in infer.type_of_binding.iter() {
        ty.as_ref().visit_with(&mut seen);
    }
    named(db, def, infer, &mut deps);
    deps
}

/// Adds the statics and constants the body names to `deps`.
fn named(
    db: &dyn HirDatabase,
    def: DefWithBodyId,
    infer: &InferenceResult<'_>,
    deps: &mut BodyDeps,
) {
    let body = Body::of(db, def);
    let mut resolver = def.resolver(db);
    let read = |value: Option<ValueNs>, deps: &mut BodyDeps| match value {
        Some(ValueNs::StaticId(id)) => _ = deps.bodies.insert(id.into()),
        Some(ValueNs::ConstId(id)) => _ = deps.bodies.insert(id.into()),
        _ => {}
    };
    for (expr, it) in body.exprs() {
        let Expr::Path(path) = it else { continue };
        // An associated constant is the one inference picked.
        if let Some((CandidateId::ConstId(id), _)) = infer.assoc_resolutions_for_expr(expr) {
            deps.bodies.insert(id.into());
            continue;
        }
        let guard = resolver.update_to_inner_scope(db, def, expr);
        let value = resolver.resolve_path_in_value_ns_fully(db, path, body.expr_path_hygiene(expr));
        resolver.reset_to_guard(guard);
        read(value, deps);
    }
    let resolver = def.resolver(db);
    for (pat, it) in body.pats() {
        let Pat::Path(path) = it else { continue };
        if let Some((CandidateId::ConstId(id), _)) = infer.assoc_resolutions_for_pat(pat) {
            deps.bodies.insert(id.into());
            continue;
        }
        read(resolver.resolve_path_in_value_ns_fully(db, path, body.pat_path_hygiene(pat)), deps);
    }
}

struct Seen<'a, 'db> {
    db: &'db dyn HirDatabase,
    deps: &'a mut BodyDeps,
}

impl<'db> Seen<'_, 'db> {
    fn called(&mut self, func: FunctionId, args: GenericArgs<'db>) {
        self.deps.callees.insert(func);
        if matches!(func.lookup(self.db).container, ItemContainerId::TraitId(_)) {
            let on = args.types().next().and_then(|it| match it.kind() {
                TyKind::Adt(adt, _) => Some(adt.def_id()),
                _ => None,
            });
            self.deps.trait_calls.insert((func, on));
        }
        args.visit_with(self);
    }

    /// The body a closure or `async` block is written in. One in an anonymous constant is
    /// in the body that constant is in, which is the one looked through.
    fn owner(&mut self, owner: InferBodyId<'db>) {
        if let InferBodyId::DefWithBodyId(def) = owner {
            self.deps.bodies.insert(def);
        }
    }
}

impl<'db> TypeVisitor<DbInterner<'db>> for Seen<'_, 'db> {
    type Result = ();

    fn visit_ty(&mut self, ty: Ty<'db>) {
        match ty.kind() {
            TyKind::Adt(adt, _) => _ = self.deps.adts.insert(adt.def_id()),
            TyKind::FnDef(def, args) => {
                if let CallableDefId::FunctionId(func) = def.0 {
                    self.called(func, args);
                }
            }
            TyKind::Closure(id, _) => self.owner(id.0.loc(self.db).owner),
            TyKind::CoroutineClosure(id, _) => self.owner(id.0.loc(self.db).owner),
            TyKind::Coroutine(id, _) => self.owner(id.0.loc(self.db).owner),
            _ => {}
        }
        ty.super_visit_with(self)
    }
}

/// The named types an impl's header mentions: the type it is for and the arguments of its
/// trait. A value of one of them can have the impl's methods run on it without a call that
/// names them: an operator, a drop, a conversion by `?`.
pub fn impl_header_adts(db: &dyn HirDatabase, id: ImplId) -> FxHashSet<AdtId> {
    let mut adts = FxHashSet::default();
    let mut collect = Adts(&mut adts);
    db.impl_self_ty(id).skip_binder().visit_with(&mut collect);
    if let Some(trait_ref) = db.impl_trait(id) {
        trait_ref.skip_binder().args.visit_with(&mut collect);
    }
    adts
}

/// The named types the fields of `adt` are of, through their type arguments: what dropping a
/// value of it drops.
pub fn field_adts(db: &dyn HirDatabase, adt: AdtId) -> FxHashSet<AdtId> {
    let variants: Vec<VariantId> = match adt {
        AdtId::StructId(it) => vec![it.into()],
        AdtId::UnionId(it) => vec![it.into()],
        AdtId::EnumId(it) => {
            it.enum_variants(db).variants.values().map(|&(variant, _)| variant.into()).collect()
        }
    };
    let mut adts = FxHashSet::default();
    for variant in variants {
        for (_, field) in db.field_types(variant).iter() {
            field.ty().skip_binder().visit_with(&mut Adts(&mut adts));
        }
    }
    adts
}

struct Adts<'a>(&'a mut FxHashSet<AdtId>);

impl<'db> TypeVisitor<DbInterner<'db>> for Adts<'_> {
    type Result = ();

    fn visit_ty(&mut self, ty: Ty<'db>) {
        if let TyKind::Adt(adt, _) = ty.kind() {
            self.0.insert(adt.def_id());
        }
        ty.super_visit_with(self)
    }
}
