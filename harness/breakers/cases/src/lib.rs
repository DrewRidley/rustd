//! Reductions of what broke rustd in the crate sweep, one module each, with the cause on top.
//! `breakers.py` runs every one on its own, so that a crash in one does not hide the others.

macro_rules! cases {
    ($($name:ident),* $(,)?) => {
        $(pub mod $name;)*
        pub const NAMES: &[&str] = &[$(stringify!($name)),*];
        pub fn run(name: &str) -> Option<String> {
            match name {
                $(stringify!($name) => Some(format!("{:?}", $name::run())),)*
                _ => None,
            }
        }
    };
}

cases![
    niche_eq,
    simd_cast,
    big_frame,
    big_promoted,
    future_size,
    future_states,
    macro_item_location,
    clmul,
    cast_then_shift,
    xgetbv,
    unsize_assoc_field,
    const_bswap,
    format_blanket_new,
    f16c,
    unsized_tail_cmp,
    named_const_impl,
    macro_same_name_vars,
    safe_macro,
    path_where_clause,
    impl_in_fn,
    fragment_token,
    blanket_in_fn,
    lifetime_only_callee,
];
