use proc_macro2::TokenStream;
use quote::quote;
use serde_json::Value;
use std::collections::HashSet;
use syn::{
    Ident, LitStr, Token, Visibility,
    parse::{Parse, ParseStream},
};

pub struct Input {
    visibility: Visibility,
    name: Ident,
    path: LitStr,
}
impl Parse for Input {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let visibility = input.parse()?;
        let name = input.parse()?;
        input.parse::<Token![=]>()?;
        let path = input.parse()?;
        if input.peek(Token![;]) {
            input.parse::<Token![;]>()?;
        }
        if !input.is_empty() {
            return Err(input.error("unexpected schema macro argument"));
        }
        Ok(Self {
            visibility,
            name,
            path,
        })
    }
}

pub fn expand(input: Input) -> syn::Result<TokenStream> {
    let source = input.path.span().unwrap().local_file().ok_or_else(|| {
        syn::Error::new(
            input.path.span(),
            "schema path must originate in a source file",
        )
    })?;
    let path = source
        .parent()
        .ok_or_else(|| syn::Error::new(input.path.span(), "schema source has no parent directory"))?
        .join(input.path.value());
    let text = std::fs::read_to_string(&path).map_err(|error| {
        syn::Error::new(
            input.path.span(),
            format!("cannot read runtime descriptor {}: {error}", path.display()),
        )
    })?;
    let descriptor: Value = serde_json::from_str(&text).map_err(|error| {
        syn::Error::new(
            input.path.span(),
            format!("invalid runtime descriptor: {error}"),
        )
    })?;
    let orm = crate::engine()?;
    let modules = generate(&descriptor, &orm, input.path.span())?;
    let Input {
        visibility,
        name,
        path,
    } = input;
    Ok(quote! {
        #visibility mod #name {
            // Track the artifact as a compiler input even though expansion reads it.
            const _: &str = include_str!(#path);
            #modules
        }
    })
}

fn flag(def: &Value, name: &str, fallback: bool, span: proc_macro2::Span) -> syn::Result<bool> {
    match def.get(name) {
        None => Ok(fallback),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err(syn::Error::new(
            span,
            format!("descriptor flag '{name}' must be boolean"),
        )),
    }
}

fn validate_identity(fields: &serde_json::Map<String, Value>) -> Result<(), &'static str> {
    let keys: Vec<_> = fields
        .iter()
        .filter(|(_, definition)| {
            definition.get("primaryKey").and_then(Value::as_bool) == Some(true)
        })
        .collect();
    if keys.is_empty() {
        return Err("collection requires a declared primary key");
    }
    for (_, key) in keys {
        if key.get("required").and_then(Value::as_bool) != Some(true) {
            return Err("primary key columns must be required and non-null");
        }
        if key.get("assign").is_some_and(|assignment| {
            assignment.get("on").and_then(Value::as_str) != Some("insert")
        }) {
            return Err("primary key columns can only be assigned on insertion");
        }
        if key.get("encrypted").and_then(Value::as_bool) == Some(true)
            || key
                .get("mask")
                .is_some_and(|mask| mask.get("kind").and_then(Value::as_str) != Some("none"))
        {
            return Err("primary key columns cannot be masked or encrypted");
        }
    }
    Ok(())
}

fn generate(
    descriptor: &Value,
    orm: &syn::Path,
    span: proc_macro2::Span,
) -> syn::Result<TokenStream> {
    if descriptor.get("version").and_then(Value::as_u64) != Some(2) {
        return Err(syn::Error::new(
            span,
            "expected the migration runtime descriptor version 2",
        ));
    }
    let collections = descriptor
        .get("collections")
        .and_then(Value::as_object)
        .ok_or_else(|| syn::Error::new(span, "runtime descriptor must contain collections"))?;
    let mut modules = Vec::new();
    let mut names = HashSet::new();
    for (name, collection) in collections {
        let module = crate::identifier(name, span)?;
        if !names.insert(module.to_string()) {
            return Err(syn::Error::new(span, "collection names collide in Rust"));
        }
        let fields = collection
            .get("fields")
            .and_then(Value::as_object)
            .ok_or_else(|| {
                syn::Error::new(span, format!("collection '{name}' has no field map"))
            })?;
        let schema = serde_json::to_string(fields).map_err(|error| syn::Error::new(span, error))?;
        validate_identity(fields)
            .map_err(|message| syn::Error::new(span, format!("{name}: {message}")))?;
        let mut columns = Vec::new();
        let mut constants = Vec::new();
        let mut required = Vec::new();
        let mut field_names = HashSet::new();
        for (field, def) in fields {
            if !def.is_object() {
                return Err(syn::Error::new(
                    span,
                    format!("{name}.{field}: expected a field descriptor"),
                ));
            }
            let column = crate::identifier(field, span)?;
            if !field_names.insert(column.to_string()) {
                return Err(syn::Error::new(span, "column names collide in Rust"));
            }
            let sql_type = logical_type(def, orm, span)
                .map_err(|error| syn::Error::new(span, format!("{name}.{field}: {error}")))?;
            let readable =
                flag(def, "readable", true, span)? && flag(def, "projectable", true, span)?;
            let filterable = flag(def, "filterable", true, span)?;
            let writable = flag(def, "writable", true, span)?
                && def.get("assign").is_none()
                && def.get("generated").is_none();
            let defaultable =
                writable && (def.get("default").is_some() || !flag(def, "required", false, span)?);
            let read = readable.then(|| quote!(impl #orm::ReadableColumn for #column {}));
            let filter = filterable.then(|| quote!(impl #orm::FilterableColumn for #column {}));
            let write = writable.then(|| quote!(impl #orm::WritableColumn for #column {}));
            let update = (writable && def.get("primaryKey").and_then(Value::as_bool) != Some(true))
                .then(|| quote!(impl #orm::UpdatableColumn for #column {}));
            let default = defaultable.then(|| quote!(impl #orm::DefaultableColumn for #column {}));
            if writable && !defaultable {
                required.push(quote!(#orm::HasColumn<columns::#column>));
            }
            columns.push(quote! {
                #[allow(non_camel_case_types)]
                #[derive(Debug)]
                pub enum #column {}
                impl #orm::Column for #column {
                    type Entity = super::Entity;
                    type SqlType = #sql_type;
                    const NAME: &'static str = #field;
                }
                #read #filter #write #update #default
            });
            constants.push(quote! {
                #[allow(non_upper_case_globals)]
                pub const #column: #orm::Field<columns::#column> = #orm::Field::new();
            });
        }
        let complete = if required.is_empty() {
            quote!(
                impl<T> CompleteInsert for T {}
            )
        } else {
            quote!(impl<T> CompleteInsert for T where T: #(#required)+* {})
        };
        modules.push(quote! {
            pub mod #module {
                #[derive(Debug)]
                pub enum Entity {}
                impl #orm::Entity for Entity {
                    const COLLECTION: &'static str = #name;
                    fn schema() -> &'static #orm::Value {
                        static SCHEMA: ::std::sync::OnceLock<#orm::Value> = ::std::sync::OnceLock::new();
                        SCHEMA.get_or_init(|| #orm::__private::schema_value(#schema))
                    }
                }
                pub mod columns { #(#columns)* }
                #(#constants)*
                #[doc(hidden)]
                pub trait CompleteInsert {}
                #complete
            }
        });
    }
    Ok(quote!(#(#modules)*))
}

fn logical_type(def: &Value, orm: &syn::Path, span: proc_macro2::Span) -> syn::Result<TokenStream> {
    let name = def
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| syn::Error::new(span, "missing logical field type"))?;
    let marker = match name {
        "string" | "text" | "id" | "ref" | "actor" => "Text",
        "int" | "integer" => "Integer",
        "bigInt" => "BigInt",
        "number" | "float" | "decimal" | "numeric" => "Number",
        "boolean" => "Boolean",
        "bytes" => "Bytes",
        "date" | "timestamp" => "Timestamp",
        "calendarDate" => "CalendarDate",
        "time" => "Time",
        "json" | "object" | "array" | "union" => "Json",
        "vector" => "Vector",
        "geoPoint" => "GeoPoint",
        _ => {
            return Err(syn::Error::new(
                span,
                format!("unsupported logical field type '{name}'"),
            ));
        }
    };
    let marker = syn::Ident::new(marker, span);
    let ty = quote!(#orm::sql_types::#marker);
    Ok(if flag(def, "required", false, span)? {
        ty
    } else {
        quote!(#orm::sql_types::Nullable<#ty>)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_generation_obeys_shared_identity_contract() {
        let corpus: Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/data/collection-identity.json"
        ))
        .unwrap();
        let orm = syn::parse_quote!(::zeroship_data_orm::orm);
        for group in ["valid", "invalid"] {
            let cases = corpus[group].as_object().unwrap();
            assert!(!cases.is_empty(), "{group} fixtures must not be empty");
            for (name, case) in cases {
                let descriptor = serde_json::json!({
                    "version": 2,
                    "collections": {"entries": {"fields": case["fields"]}}
                });
                let result = generate(&descriptor, &orm, proc_macro2::Span::call_site());
                if group == "valid" {
                    assert!(
                        !result
                            .unwrap_or_else(|error| panic!("{name}: {error}"))
                            .is_empty()
                    );
                } else {
                    assert_eq!(
                        result.expect_err(name).to_string(),
                        format!("entries: {}", case["error"].as_str().unwrap()),
                        "{name}"
                    );
                }
            }
        }
    }
}
