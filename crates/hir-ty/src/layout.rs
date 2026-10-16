//! Compute the binary representation of a type

use std::fmt;

use hir_def::{
    AdtId, LocalFieldId, StructId,
    attrs::AttrFlags,
    layout::{LayoutCalculatorError, LayoutData},
};
use la_arena::{Idx, RawIdx};

use rustc_abi::{
    AddressSpace, BackendRepr, FieldsShape, Float, Integer, LayoutCalculator, Niche, Primitive,
    ReprOptions, Scalar, Size, StructKind, TargetDataLayout, WrappingRange,
};
use rustc_index::IndexVec;
use rustc_type_ir::{
    FloatTy, IntTy, TypeVisitableExt as _, UintTy,
    inherent::{GenericArgs as _, IntoKind},
};
use triomphe::Arc;

use crate::{
    ParamEnvAndCrate,
    consteval::try_const_usize,
    db::HirDatabase,
    next_solver::{
        Const, ConstKind, DbInterner, GenericArgs, PatternKind, StoredTy, Ty, TyKind, TypingMode,
        ValueConst,
        infer::{DbInternerInferExt, traits::ObligationCause},
    },
    traits::StoredParamEnvAndCrate,
};

pub use self::{adt::layout_of_adt_query, target::target_data_layout_query};

pub(crate) mod adt;
pub(crate) mod target;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RustcEnumVariantIdx(pub usize);

impl rustc_index::Idx for RustcEnumVariantIdx {
    fn new(idx: usize) -> Self {
        RustcEnumVariantIdx(idx)
    }

    fn index(self) -> usize {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RustcFieldIdx(pub LocalFieldId);

impl RustcFieldIdx {
    pub fn new(idx: usize) -> Self {
        RustcFieldIdx(Idx::from_raw(RawIdx::from(idx as u32)))
    }
}

impl rustc_index::Idx for RustcFieldIdx {
    fn new(idx: usize) -> Self {
        RustcFieldIdx(Idx::from_raw(RawIdx::from(idx as u32)))
    }

    fn index(self) -> usize {
        u32::from(self.0.into_raw()) as usize
    }
}

pub type Layout = LayoutData<RustcFieldIdx, RustcEnumVariantIdx>;
pub type TagEncoding = hir_def::layout::TagEncoding<RustcEnumVariantIdx>;
pub type Variants = hir_def::layout::Variants<RustcFieldIdx, RustcEnumVariantIdx>;

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum LayoutError {
    // FIXME: Remove more variants once they get added to LayoutCalculatorError
    BadCalc(LayoutCalculatorError<()>),
    HasErrorConst,
    HasErrorType,
    HasPlaceholder,
    InvalidSimdType,
    NotImplemented,
    RecursiveTypeWithoutIndirection,
    TargetLayoutNotAvailable,
    Unknown,
    UserReprTooSmall,
}

impl std::error::Error for LayoutError {}
impl fmt::Display for LayoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LayoutError::BadCalc(err) => err.fallback_fmt(f),
            LayoutError::HasErrorConst => write!(f, "type contains an unevaluatable const"),
            LayoutError::HasErrorType => write!(f, "type contains an error"),
            LayoutError::HasPlaceholder => write!(f, "type contains placeholders"),
            LayoutError::InvalidSimdType => write!(f, "invalid simd type definition"),
            LayoutError::NotImplemented => write!(f, "not implemented"),
            LayoutError::RecursiveTypeWithoutIndirection => {
                write!(f, "recursive type without indirection")
            }
            LayoutError::TargetLayoutNotAvailable => write!(f, "target layout not available"),
            LayoutError::Unknown => write!(f, "unknown"),
            LayoutError::UserReprTooSmall => {
                write!(f, "the `#[repr]` hint is too small to hold the discriminants of the enum")
            }
        }
    }
}

impl<F> From<LayoutCalculatorError<F>> for LayoutError {
    fn from(err: LayoutCalculatorError<F>) -> Self {
        LayoutError::BadCalc(err.without_payload())
    }
}

struct LayoutCx<'a> {
    calc: LayoutCalculator<&'a TargetDataLayout>,
}

impl<'a> LayoutCx<'a> {
    fn new(target: &'a TargetDataLayout) -> Self {
        Self { calc: LayoutCalculator::new(target) }
    }
}

// FIXME: move this to the `rustc_abi`.
fn layout_of_simd_ty<'db>(
    db: &'db dyn HirDatabase,
    id: StructId,
    repr_packed: bool,
    args: &GenericArgs<'db>,
    env: ParamEnvAndCrate<'db>,
    dl: &TargetDataLayout,
) -> Result<Arc<Layout>, LayoutError> {
    // Supported SIMD vectors are homogeneous ADTs with exactly one array field:
    //
    // * #[repr(simd)] struct S([T; 4])
    //
    // where T is a primitive scalar (integer/float/pointer).
    let fields = db.field_types(id.into());
    let mut fields = fields.iter();
    let Some(TyKind::Array(e_ty, e_len)) =
        fields.next().filter(|_| fields.next().is_none()).map(|f| {
            (*f.1).ty().instantiate(DbInterner::new_no_crate(db), args).skip_norm_wip().kind()
        })
    else {
        return Err(LayoutError::InvalidSimdType);
    };

    let e_len = try_const_usize(db, e_len).ok_or(LayoutError::HasErrorConst)? as u64;
    let e_ly = db.layout_of_ty(e_ty.store(), env.store())?;

    let cx = LayoutCx::new(dl);
    Ok(Arc::new(cx.calc.simd_type(e_ly, e_len, repr_packed)?))
}

#[salsa::tracked(cycle_result = layout_of_ty_cycle_result, returns(clone))]
pub fn layout_of_ty_query(
    db: &dyn HirDatabase,
    ty: StoredTy,
    trait_env: StoredParamEnvAndCrate,
) -> Result<Arc<Layout>, LayoutError> {
    let krate = trait_env.krate;
    let interner = DbInterner::new_with(db, krate);
    let Ok(target) = db.target_data_layout(krate) else {
        return Err(LayoutError::TargetLayoutNotAvailable);
    };
    let dl = target;
    let cx = LayoutCx::new(dl);
    let infer_ctxt = interner.infer_ctxt().build(TypingMode::PostAnalysis);
    let cause = ObligationCause::dummy();
    let ty = infer_ctxt
        .at(&cause, trait_env.param_env(db))
        .deeply_normalize(ty.as_ref())
        .unwrap_or(ty.as_ref());
    let result = match ty.kind() {
        TyKind::Adt(def, args) => {
            match def.def_id() {
                hir_def::AdtId::StructId(s) => {
                    let repr = AttrFlags::repr(db, s.into()).unwrap_or_default();
                    if repr.simd() {
                        return layout_of_simd_ty(
                            db,
                            s,
                            repr.packed(),
                            &args,
                            trait_env.as_ref(db),
                            target,
                        );
                    }
                }
                _ => {}
            }
            return db.layout_of_adt(def.def_id(), args.store(), trait_env);
        }
        TyKind::Bool => Layout::scalar(
            dl,
            Scalar::Initialized {
                value: Primitive::Int(Integer::I8, false),
                valid_range: WrappingRange { start: 0, end: 1 },
            },
        ),
        TyKind::Char => Layout::scalar(
            dl,
            Scalar::Initialized {
                value: Primitive::Int(Integer::I32, false),
                valid_range: WrappingRange { start: 0, end: 0x10FFFF },
            },
        ),
        TyKind::Int(i) => Layout::scalar(
            dl,
            scalar_unit(
                dl,
                Primitive::Int(
                    match i {
                        IntTy::Isize => dl.ptr_sized_integer(),
                        IntTy::I8 => Integer::I8,
                        IntTy::I16 => Integer::I16,
                        IntTy::I32 => Integer::I32,
                        IntTy::I64 => Integer::I64,
                        IntTy::I128 => Integer::I128,
                    },
                    true,
                ),
            ),
        ),
        TyKind::Uint(i) => Layout::scalar(
            dl,
            scalar_unit(
                dl,
                Primitive::Int(
                    match i {
                        UintTy::Usize => dl.ptr_sized_integer(),
                        UintTy::U8 => Integer::I8,
                        UintTy::U16 => Integer::I16,
                        UintTy::U32 => Integer::I32,
                        UintTy::U64 => Integer::I64,
                        UintTy::U128 => Integer::I128,
                    },
                    false,
                ),
            ),
        ),
        TyKind::Float(f) => Layout::scalar(
            dl,
            scalar_unit(
                dl,
                Primitive::Float(match f {
                    FloatTy::F16 => Float::F16,
                    FloatTy::F32 => Float::F32,
                    FloatTy::F64 => Float::F64,
                    FloatTy::F128 => Float::F128,
                }),
            ),
        ),
        TyKind::Tuple(tys) => {
            let kind =
                if tys.is_empty() { StructKind::AlwaysSized } else { StructKind::MaybeUnsized };

            let fields = tys
                .iter()
                .map(|k| db.layout_of_ty(k.store(), trait_env.clone()))
                .collect::<Result<Vec<_>, _>>()?;
            let fields = fields.iter().map(|it| &**it).collect::<Vec<_>>();
            let fields = fields.iter().collect::<IndexVec<_, _>>();
            cx.calc.univariant(&fields, &ReprOptions::default(), kind)?
        }
        TyKind::Array(element, count) => {
            let count = try_const_usize(db, count).ok_or(LayoutError::HasErrorConst)? as u64;
            let element = db.layout_of_ty(element.store(), trait_env)?;
            cx.calc.array_like::<_, _, ()>(&element, Some(count))?
        }
        TyKind::Slice(element) => {
            let element = db.layout_of_ty(element.store(), trait_env)?;
            cx.calc.array_like::<_, _, ()>(&element, None)?
        }
        TyKind::Str => {
            let element = scalar_unit(dl, Primitive::Int(Integer::I8, false));
            cx.calc.array_like::<_, _, ()>(&Layout::scalar(dl, element), None)?
        }
        // Potentially-wide pointers.
        TyKind::Ref(_, pointee, _) | TyKind::RawPtr(pointee, _) => {
            let mut data_ptr = scalar_unit(dl, Primitive::Pointer(AddressSpace::ZERO));
            if matches!(ty.kind(), TyKind::Ref(..)) {
                data_ptr.valid_range_mut().start = 1;
            }

            // FIXME(next-solver)
            // let pointee = tcx.normalize_erasing_regions(param_env, pointee);
            // if pointee.is_sized(tcx.at(DUMMY_SP), param_env) {
            //     return Ok(tcx.mk_layout(LayoutS::scalar(cx, data_ptr)));
            // }

            // The tail can be behind an associated type, such as `A::Dyn`.
            let normalize = |ty| {
                infer_ctxt.at(&cause, trait_env.param_env(db)).deeply_normalize(ty).unwrap_or(ty)
            };
            let unsized_part = struct_tail_erasing_lifetimes(db, pointee, &normalize);
            // FIXME(next-solver)
            /*
            if let TyKind::AssociatedType(id, subst) = unsized_part.kind(Interner) {
                unsized_part = TyKind::Alias(chalk_ir::AliasTy::Projection(ProjectionTy {
                    associated_ty_id: *id,
                    substitution: subst.clone(),
                }))
                .intern(Interner);
            }
            unsized_part = normalize(db, trait_env, unsized_part);
            */
            let metadata = match unsized_part.kind() {
                TyKind::Slice(_) | TyKind::Str => {
                    scalar_unit(dl, Primitive::Int(dl.ptr_sized_integer(), false))
                }
                TyKind::Dynamic(..) => {
                    let mut vtable = scalar_unit(dl, Primitive::Pointer(AddressSpace::ZERO));
                    vtable.valid_range_mut().start = 1;
                    vtable
                }
                _ => {
                    // pointee is sized
                    return Ok(Arc::new(Layout::scalar(dl, data_ptr)));
                }
            };

            // Effectively a (ptr, meta) tuple.
            LayoutData::scalar_pair(dl, data_ptr, metadata)
        }
        TyKind::Never => LayoutData::never_type(dl),
        TyKind::FnDef(..) => LayoutData::unit(dl, true),
        TyKind::Dynamic(..) | TyKind::Foreign(_) => LayoutData::unit(dl, false),
        TyKind::FnPtr(..) => {
            let mut ptr = scalar_unit(dl, Primitive::Pointer(dl.instruction_address_space));
            ptr.valid_range_mut().start = 1;
            Layout::scalar(dl, ptr)
        }
        TyKind::Closure(_, args) => {
            // Not the layout of the tuple of captures: a tuple's last field can be unsized and
            // so stays last, where a closure's fields are all free to move.
            let fields =
                rustc_type_ir::inherent::Ty::tuple_fields(args.as_closure().tupled_upvars_ty())
                    .iter()
                    .map(|k| db.layout_of_ty(k.store(), trait_env.clone()))
                    .collect::<Result<Vec<_>, _>>()?;
            let fields = fields.iter().map(|it| &**it).collect::<Vec<_>>();
            let fields = fields.iter().collect::<IndexVec<_, _>>();
            cx.calc.univariant(&fields, &ReprOptions::default(), StructKind::AlwaysSized)?
        }
        TyKind::Coroutine(id, args) => {
            // What was captured, then everything `CoroutineFrame` describes. A local only
            // holds a value some of the time, so nothing here is a niche.
            let args = args.as_coroutine();
            let parent_args = GenericArgs::new_from_slice(args.parent_args());
            let body = db
                .monomorphized_mir_body_for_coroutine(id.0, parent_args.store(), trait_env.clone())
                .map_err(|_| LayoutError::Unknown)?;
            let opaque = |size, align| {
                let mut layout = LayoutData::unit(dl, true);
                layout.size = size;
                layout.align = align;
                layout
            };
            let word = Size::from_bytes(8);
            let word =
                opaque(word, rustc_abi::AbiAlign::new(rustc_abi::Align::from_bytes(8).unwrap()));
            let mut fields = Vec::new();
            for ty in rustc_type_ir::inherent::Ty::tuple_fields(args.tupled_upvars_ty()) {
                let layout = db.layout_of_ty(ty.store(), trait_env.clone())?;
                fields.push(opaque(layout.size, layout.align));
            }
            // The body declares a local for every binding of the function around it.
            let mut used = rustc_hash::FxHashSet::default();
            let mut places = body.clone();
            places.walk_places(|place| {
                used.insert(place.local);
            });
            // A local that holds nothing whenever the body yields lives on the stack of `poll`.
            let held = body.locals_held_across_yields(|local| {
                crate::drop::has_drop_glue(
                    &infer_ctxt,
                    body.locals[local].ty.as_ref(),
                    trait_env.param_env(db),
                ) != crate::drop::DropGlue::None
            });
            // The locals come after everything else, where two that never hold a value at once
            // share memory: the futures of the arms of a `match`, each awaited in its arm.
            let first_local = fields.len();
            let mut in_frame = Vec::new();
            for (at, &id) in held.locals.iter().enumerate() {
                if used.contains(&id) {
                    let layout = db.layout_of_ty(body.locals[id].ty.clone(), trait_env.clone())?;
                    if layout.size.bytes() != 0 {
                        in_frame.push((at, id, layout.size, layout.align));
                    }
                }
            }
            for _ in body.locals.iter() {
                fields.push(opaque(Size::ZERO, rustc_abi::AbiAlign::new(dl.i8_align)));
            }
            fields.extend(body.locals.iter().map(|_| word.clone()));
            fields.push(word);
            let fields = fields.iter().collect::<Vec<_>>();
            let fields = fields.iter().collect::<IndexVec<_, _>>();
            let mut layout =
                cx.calc.univariant(&fields, &ReprOptions::default(), StructKind::AlwaysSized)?;
            layout.largest_niche = None;
            layout.backend_repr = BackendRepr::Memory { sized: true };
            // The largest first, each at the lowest offset clear of the ones it can't share
            // memory with.
            in_frame.sort_by_key(|&(at, _, size, align)| {
                (std::cmp::Reverse(size), std::cmp::Reverse(align.abi), at)
            });
            let FieldsShape::Arbitrary { offsets, .. } = &mut layout.fields else {
                return Err(LayoutError::Unknown);
            };
            let start = layout.size;
            let mut placed: Vec<(usize, Size, Size)> = Vec::new();
            for &(at, id, size, align) in &in_frame {
                let mut taken: Vec<(Size, Size)> = placed
                    .iter()
                    .filter(|&&(other, ..)| held.conflict(at, other))
                    .map(|&(_, from, to)| (from, to))
                    .collect();
                taken.sort();
                let mut offset = start.align_to(align.abi);
                for (from, to) in taken {
                    if from < offset + size && offset < to {
                        offset = to.align_to(align.abi);
                    }
                }
                placed.push((at, offset, offset + size));
                let field = first_local + id.into_raw().into_u32() as usize;
                offsets[RustcFieldIdx::new(field)] = offset;
                layout.size = layout.size.max(offset + size);
                layout.align = layout.align.max(align);
            }
            layout.size = layout.size.align_to(layout.align.abi);
            layout
        }
        TyKind::CoroutineClosure(_, args) => {
            return db
                .layout_of_ty(args.as_coroutine_closure().tupled_upvars_ty().store(), trait_env);
        }
        TyKind::CoroutineWitness(_, _) => {
            return Err(LayoutError::NotImplemented);
        }

        TyKind::Pat(ty, pat) => {
            let mut layout = (*db.layout_of_ty(ty.store(), trait_env.clone())?).clone();
            match pat.kind() {
                PatternKind::Range { start, end } => {
                    if let BackendRepr::Scalar(scalar) = &mut layout.backend_repr {
                        scalar.valid_range_mut().start = extract_const_value(start)
                            .ok()
                            .and_then(|v| v.try_to_bits(db, trait_env.as_ref(db)))
                            .unwrap_or(0);

                        scalar.valid_range_mut().end = extract_const_value(end)
                            .ok()
                            .and_then(|v| v.try_to_bits(db, trait_env.as_ref(db)))
                            .unwrap_or_else(|| {
                                if scalar.is_signed() {
                                    layout.size.signed_int_max() as u128
                                } else {
                                    layout.size.unsigned_int_max()
                                }
                            });

                        // FIXME(pattern_types): create implied bounds from pattern types in signatures
                        // that require that the range end is >= the range start so that we can't hit
                        // this error anymore without first having hit a trait solver error.
                        // Very fuzzy on the details here, but pattern types are an internal impl detail,
                        // so we can just go with this for now
                        if scalar.is_signed() {
                            let range = scalar.valid_range_mut();
                            let start = layout.size.sign_extend(range.start);
                            let end = layout.size.sign_extend(range.end);
                            if end < start {
                                return Err(LayoutError::HasErrorType);
                            }
                        } else {
                            let range = scalar.valid_range_mut();
                            if range.end < range.start {
                                return Err(LayoutError::HasErrorType);
                            }
                        };

                        let niche = Niche {
                            offset: Size::ZERO,
                            value: scalar.primitive(),
                            valid_range: scalar.valid_range(target),
                        };

                        layout.largest_niche = Some(niche);
                    } else {
                        panic!("pattern type with range but not scalar layout: {ty:?}, {layout:?}")
                    }
                }
                PatternKind::NotNull => {
                    if let BackendRepr::Scalar(scalar) | BackendRepr::ScalarPair(scalar, _) =
                        &mut layout.backend_repr
                    {
                        scalar.valid_range_mut().start = 1;
                        let niche = Niche {
                            offset: Size::ZERO,
                            value: scalar.primitive(),
                            valid_range: scalar.valid_range(target),
                        };

                        layout.largest_niche = Some(niche);
                    } else {
                        panic!(
                            "pattern type with `!null` pattern but not scalar/pair layout: {ty:?}, {layout:?}"
                        )
                    }
                }

                PatternKind::Or(variants) => match variants[0].kind() {
                    PatternKind::Range { .. } => {
                        if let BackendRepr::Scalar(scalar) = &mut layout.backend_repr {
                            let variants: Result<Vec<_>, LayoutError> = variants
                                .iter()
                                .map(|pat| match pat.kind() {
                                    PatternKind::Range { start, end } => Ok::<_, LayoutError>((
                                        extract_const_value(start)?
                                            .try_to_bits(db, trait_env.as_ref(db))
                                            .ok_or(LayoutError::Unknown)?,
                                        extract_const_value(end)?
                                            .try_to_bits(db, trait_env.as_ref(db))
                                            .ok_or(LayoutError::Unknown)?,
                                    )),
                                    PatternKind::NotNull | PatternKind::Or(_) => {
                                        Err(LayoutError::Unknown)
                                    }
                                })
                                .collect();
                            if !scalar.is_signed() {
                                return Err(LayoutError::HasErrorType);
                            }
                            let (second_start, first_end) = if let Ok(mut variants) = variants {
                                variants.sort();
                                if variants.len() != 2 {
                                    return Err(LayoutError::HasErrorType);
                                }
                                let mut first = variants[0];
                                let mut second = variants[1];
                                if second.0
                                    == layout.size.truncate(layout.size.signed_int_min() as u128)
                                {
                                    (second, first) = (first, second);
                                }
                                if layout.size.sign_extend(first.1)
                                    >= layout.size.sign_extend(second.0)
                                {
                                    return Err(LayoutError::HasErrorType);
                                }
                                if layout.size.signed_int_max() as u128 != second.1 {
                                    return Err(LayoutError::HasErrorType);
                                }
                                (second.0, first.1)
                            } else {
                                // `NonZeroI*` (`..=-1 | 1..`) when range endpoint consts are unevaluated.
                                (1u128, layout.size.unsigned_int_max())
                            };

                            // Now generate a wrapping range (which aren't allowed in surface syntax).
                            scalar.valid_range_mut().start = second_start;
                            scalar.valid_range_mut().end = first_end;

                            let niche = Niche {
                                offset: Size::ZERO,
                                value: scalar.primitive(),
                                valid_range: scalar.valid_range(target),
                            };

                            layout.largest_niche = Some(niche);
                        } else {
                            panic!(
                                "pattern type with range but not scalar layout: {ty:?}, {layout:?}"
                            )
                        }
                    }
                    PatternKind::NotNull => panic!("or patterns can't contain `!null` patterns"),
                    PatternKind::Or(..) => panic!("patterns cannot have nested or patterns"),
                },
            }
            // Pattern types contain their base as their sole field.
            // This allows the rest of the compiler to process pattern types just like
            // single field transparent Adts, and only the parts of the compiler that
            // specifically care about pattern types will have to handle it.
            layout.fields = FieldsShape::Arbitrary {
                offsets: [Size::ZERO].into_iter().collect(),
                in_memory_order: [RustcFieldIdx::new(0)].into_iter().collect(),
            };
            layout
        }
        TyKind::UnsafeBinder(_) => {
            return Err(LayoutError::NotImplemented);
        }

        TyKind::Error(_) => return Err(LayoutError::HasErrorType),
        TyKind::Placeholder(_)
        | TyKind::Bound(..)
        | TyKind::Infer(..)
        | TyKind::Param(..)
        | TyKind::Alias(..) => {
            return Err(LayoutError::HasPlaceholder);
        }
    };
    Ok(Arc::new(result))
}

fn layout_of_ty_cycle_result(
    _: &dyn HirDatabase,
    _: salsa::Id,
    _: StoredTy,
    _: StoredParamEnvAndCrate,
) -> Result<Arc<Layout>, LayoutError> {
    Err(LayoutError::RecursiveTypeWithoutIndirection)
}

fn extract_const_value<'db>(ct: Const<'db>) -> Result<ValueConst<'db>, LayoutError> {
    match ct.kind() {
        ConstKind::Value(cv) => Ok(cv),
        ConstKind::Param(_)
        | ConstKind::Expr(_)
        | ConstKind::Unevaluated(_)
        | ConstKind::Infer(_)
        | ConstKind::Bound(..)
        | ConstKind::Placeholder(_) => {
            if ct.has_param() {
                Err(LayoutError::HasPlaceholder)
            } else {
                Err(LayoutError::Unknown)
            }
        }
        ConstKind::Error(_) => Err(LayoutError::HasErrorConst),
    }
}

fn struct_tail_erasing_lifetimes<'a>(
    db: &'a dyn HirDatabase,
    pointee: Ty<'a>,
    normalize: &dyn Fn(Ty<'a>) -> Ty<'a>,
) -> Ty<'a> {
    match pointee.kind() {
        TyKind::Adt(def, args) => {
            let struct_id = match def.def_id() {
                AdtId::StructId(id) => id,
                _ => return pointee,
            };
            let data = struct_id.fields(db);
            let mut it = data.fields().iter().rev();
            match it.next() {
                Some((f, _)) => {
                    let last_field_ty = normalize(field_ty(db, struct_id.into(), f, args));
                    struct_tail_erasing_lifetimes(db, last_field_ty, normalize)
                }
                None => pointee,
            }
        }
        TyKind::Tuple(tys) => {
            if let Some(last_field_ty) = tys.iter().next_back() {
                struct_tail_erasing_lifetimes(db, last_field_ty, normalize)
            } else {
                pointee
            }
        }
        _ => pointee,
    }
}

fn field_ty<'a>(
    db: &'a dyn HirDatabase,
    def: hir_def::VariantId,
    fd: LocalFieldId,
    args: GenericArgs<'a>,
) -> Ty<'a> {
    db.field_types(def)[fd].ty().instantiate(DbInterner::new_no_crate(db), args).skip_norm_wip()
}

fn scalar_unit(dl: &TargetDataLayout, value: Primitive) -> Scalar {
    Scalar::Initialized { value, valid_range: WrappingRange::full(value.size(dl)) }
}

/// Where an `async` block keeps what has to survive from one `poll` to the next, as fields of
/// its layout that follow the captures: every local of its body, a word of drop flags for each
/// of those, and a word saying where the body left off.
#[derive(Debug, Clone, Copy)]
pub struct CoroutineFrame {
    pub upvars: usize,
    pub locals: usize,
}

impl CoroutineFrame {
    pub fn local(self, index: usize) -> usize {
        self.upvars + index
    }

    pub fn drop_flag(self, index: usize) -> usize {
        self.upvars + self.locals + index
    }

    pub fn state(self) -> usize {
        self.upvars + 2 * self.locals
    }
}
