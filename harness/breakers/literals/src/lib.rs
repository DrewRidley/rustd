//! Macros that hand back literals built from values, not parsed from text.

use proc_macro::{Literal, TokenStream, TokenTree};

#[proc_macro]
pub fn minus_one(_input: TokenStream) -> TokenStream {
    TokenTree::Literal(Literal::i32_suffixed(-1)).into()
}

#[proc_macro]
pub fn minus_half(_input: TokenStream) -> TokenStream {
    TokenTree::Literal(Literal::f32_suffixed(-0.5)).into()
}

#[proc_macro]
pub fn dashed(_input: TokenStream) -> TokenStream {
    TokenTree::Literal(Literal::string("-1")).into()
}
