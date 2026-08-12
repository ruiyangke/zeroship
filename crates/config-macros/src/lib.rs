//! Proc macros for canonical zeroship configuration declarations.
//!
//! This crate only emits glue. The configuration types, transforms, source
//! resolution, and linked registries live in `zeroship-core` so every consumer
//! shares one contract.

use proc_macro::TokenStream;

mod zeroship_config;

/// Expand a resolved configuration struct into its inert source machinery.
///
/// The macro accepts a named-field struct whose fields are
/// `Operational<T>` or `Secret<T>` and carry `#[config(name = "...")]`.
/// It preserves that resolved struct, and emits a separate clap source carrier,
/// a consumer marker, configuration specs, linked read-site registrations, and
/// a resolver implementation.
#[proc_macro_attribute]
pub fn zeroship_config(attr: TokenStream, item: TokenStream) -> TokenStream {
    zeroship_config::expand(attr.into(), item.into())
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}
