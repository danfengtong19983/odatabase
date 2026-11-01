//! `#[derive(ODataEntity)]`, `#[derive(ODataComplex)]` and `#[derive(ODataEnum)]`.
//!
//! The derives emit model registration code only. Rows are described by the model at runtime,
//! so the structs themselves never hold query results.

mod attrs;
mod gen_enum;
mod gen_struct;

use proc_macro::TokenStream;
use syn::{DeriveInput, parse_macro_input};

/// Declares an entity type backed by a table.
#[proc_macro_derive(ODataEntity, attributes(odata))]
pub fn derive_entity(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    gen_struct::entity(&input)
        .unwrap_or_else(|e| e.to_compile_error())
        .into()
}

/// Declares a complex type, embedded in its owner's table or held in a collection table.
#[proc_macro_derive(ODataComplex, attributes(odata))]
pub fn derive_complex(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    gen_struct::complex(&input)
        .unwrap_or_else(|e| e.to_compile_error())
        .into()
}

/// Declares an enum type from a fieldless Rust enum.
#[proc_macro_derive(ODataEnum, attributes(odata))]
pub fn derive_enum(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    gen_enum::derive(&input)
        .unwrap_or_else(|e| e.to_compile_error())
        .into()
}
