//! The shape of a vtable, which has to agree with rustc's `vtable_entries` slot for slot:
//! objects cross between rustc-compiled code and ours in both directions.

use hir_def::{AssocItemId, FunctionId, TraitId};
use rustc_hash::FxHashSet;
use rustc_type_ir::{Interner, inherent::Clause as _};

use crate::{
    dyn_compatibility::generics_require_sized_self,
    next_solver::{DbInterner, TraitRef},
};

/// `drop_in_place`, size and align come before the slots.
pub(super) const HEADER: usize = 3;

#[derive(Debug, Clone, Copy)]
pub(super) enum Slot<'db> {
    Method(FunctionId, TraitRef<'db>),
    /// The vtable of a supertrait, for upcasting to one that is not a prefix of this vtable.
    Vptr(TraitRef<'db>),
}

pub(super) struct VtableLayout<'db> {
    pub(super) slots: Vec<Slot<'db>>,
    /// The supertraits whose vtable is a prefix of this one.
    prefixes: Vec<TraitRef<'db>>,
}

impl<'db> VtableLayout<'db> {
    pub(super) fn of(interner: DbInterner<'db>, trait_ref: TraitRef<'db>) -> Self {
        let mut walk = Walk {
            interner,
            visited: FxHashSet::from_iter([trait_ref]),
            slots: Vec::new(),
            prefixes: Vec::new(),
            emit_vptr: false,
        };
        walk.visit(trait_ref, false);
        VtableLayout { slots: walk.slots, prefixes: walk.prefixes }
    }

    /// The slot index, header included, of a trait method.
    pub(super) fn method(&self, func: FunctionId) -> Option<usize> {
        let index = self.slots.iter().position(|slot| match slot {
            Slot::Method(it, _) => *it == func,
            Slot::Vptr(_) => false,
        })?;
        Some(HEADER + index)
    }

    /// How a pointer to this vtable becomes one to the vtable of supertrait `target`.
    pub(super) fn upcast(&self, target: TraitRef<'db>) -> Option<Upcast> {
        let slot = self.slots.iter().position(|slot| match slot {
            Slot::Vptr(it) => *it == target,
            Slot::Method(..) => false,
        });
        match slot {
            Some(index) => Some(Upcast::Load(HEADER + index)),
            None if self.prefixes.contains(&target) => Some(Upcast::Same),
            None => None,
        }
    }
}

pub(super) enum Upcast {
    /// This vtable starts with the supertrait's.
    Same,
    /// The supertrait's vtable is behind the pointer in this slot.
    Load(usize),
}

struct Walk<'db> {
    interner: DbInterner<'db>,
    visited: FxHashSet<TraitRef<'db>>,
    slots: Vec<Slot<'db>>,
    prefixes: Vec<TraitRef<'db>>,
    /// Set once a trait with methods has been laid out: every later supertrait is no longer
    /// at the start of the vtable and needs a pointer to its own.
    emit_vptr: bool,
}

impl<'db> Walk<'db> {
    /// Supertraits first, depth first in declaration order, then the trait's own methods.
    fn visit(&mut self, trait_ref: TraitRef<'db>, vptr: bool) {
        let interner = self.interner;
        let supertraits: Vec<_> = interner
            .explicit_super_predicates_of(trait_ref.def_id)
            .iter_instantiated(interner, trait_ref.args.as_slice())
            .filter_map(|clause| clause.skip_norm_wip().0.as_trait_clause())
            .map(|pred| interner.instantiate_bound_regions_with_erased(pred).trait_ref)
            .collect();
        for supertrait in supertraits {
            if self.visited.insert(supertrait) {
                self.visit(supertrait, self.emit_vptr);
            }
        }
        let methods = own_methods(interner, trait_ref.def_id.0);
        let has_methods = !methods.is_empty();
        self.emit_vptr |= has_methods;
        self.slots.extend(methods.into_iter().map(|func| Slot::Method(func, trait_ref)));
        if !vptr {
            self.prefixes.push(trait_ref);
        } else if has_methods {
            // rustc gives a supertrait without methods no slot.
            self.slots.push(Slot::Vptr(trait_ref));
        }
    }
}

/// The methods of `trait_` that can be called on a trait object, in declaration order.
fn own_methods(interner: DbInterner<'_>, trait_: TraitId) -> Vec<FunctionId> {
    let db = interner.db;
    trait_
        .trait_items(db)
        .items
        .iter()
        .filter_map(|&(_, item)| match item {
            AssocItemId::FunctionId(func) => Some(func),
            _ => None,
        })
        .filter(|&func| !generics_require_sized_self(db, func.into()))
        .collect()
}
