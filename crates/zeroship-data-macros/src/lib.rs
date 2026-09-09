//! Generate Rust ORM contracts from migration artifacts and native Rust structs.
use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as Tokens};
use syn::{parse_macro_input, DeriveInput};

mod derive;
mod schema;

fn engine() -> syn::Result<syn::Path> {
    use proc_macro_crate::{crate_name, FoundCrate};
    let name = match crate_name("zeroship-data-engine") {
        Ok(FoundCrate::Name(name)) => name,
        Ok(FoundCrate::Itself) => "zeroship_data_engine".into(),
        Err(error) => return Err(syn::Error::new(Span::call_site(), error)),
    };
    syn::parse_str(&format!("::{name}::orm"))
}

fn finish(result: syn::Result<Tokens>) -> TokenStream {
    result.unwrap_or_else(syn::Error::into_compile_error).into()
}

/// Read a migration-generated runtime descriptor relative to the Rust source file.
///
/// `schema!(pub app_schema = "../generated/zeroship/schema.runtime.json");`
#[proc_macro]
pub fn schema(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as schema::Input);
    finish(schema::expand(input))
}

/// Map a returned row through the named entity's generated columns.
#[proc_macro_derive(FromRow, attributes(orm))]
pub fn from_row(input: TokenStream) -> TokenStream {
    finish(derive::expand(
        parse_macro_input!(input as DeriveInput),
        derive::Kind::Read,
    ))
}

/// Encode an insert input, checking columns, required fields, and defaults.
#[proc_macro_derive(Insertable, attributes(orm))]
pub fn insertable(input: TokenStream) -> TokenStream {
    finish(derive::expand(
        parse_macro_input!(input as DeriveInput),
        derive::Kind::Insert,
    ))
}

/// Encode explicit `Change` fields into a partial update.
#[proc_macro_derive(Changeset, attributes(orm))]
pub fn changeset(input: TokenStream) -> TokenStream {
    finish(derive::expand(
        parse_macro_input!(input as DeriveInput),
        derive::Kind::Update,
    ))
}

fn identifier(name: &str, span: Span) -> syn::Result<syn::Ident> {
    if name.is_empty() || !name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_') {
        return Err(syn::Error::new(
            span,
            "expected an ASCII database identifier",
        ));
    }
    let mut identifier = [name.to_owned(), format!("r#{name}"), format!("_zs_{name}")]
        .into_iter()
        .find_map(|name| syn::parse_str::<syn::Ident>(&name).ok())
        .ok_or_else(|| syn::Error::new(span, "identifier cannot be represented in Rust"))?;
    identifier.set_span(span);
    Ok(identifier)
}
