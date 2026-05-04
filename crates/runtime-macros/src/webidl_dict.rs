//! `#[derive(WebIdlDict)]` — WebIDL §3.10 dictionary parsing.
//!
//! Generates a `from_v8(scope, value) -> Result<Self, OpError>` impl
//! that reads each dictionary member from a JS object. Coercion uses
//! the [`WebIdlConvertible`] trait, which is auto-impl'd for primitives
//! and (via this derive + WebIdlEnum) for user types.
//!
//! # Spec mapping
//!
//! Per §3.10:
//!   1. If `V` is `undefined` or `null`, the dictionary is initialised
//!      with all-defaults. We require `Self: Default` for this.
//!   2. Otherwise `V` MUST be an Object (Boolean / Number / String /
//!      Symbol → TypeError).
//!   3. For each declared dictionary member, in declaration order:
//!      a. Read the property by its WebIDL name. Default = the Rust
//!         field name; override with `#[webidl_name = "..."]`.
//!      b. If the property is missing or `undefined`, the spec falls
//!         back to "absent" (or to the default if specified). We map:
//!           - `Option<T>` field → `None` on absent.
//!           - `T` field with `#[derive(Default)]` available → use
//!             `T::default()`. Required for non-Option fields.
//!      c. Otherwise convert via `T::from_v8(scope, prop_value)`. Errors
//!         propagate as TypeError.
//!
//! # User types as dictionary members
//!
//! Nested dicts work recursively because the derive emits both an
//! inherent `from_v8` AND an `impl WebIdlConvertible for Self`. So a
//! field `Option<Inner>` where `Inner: WebIdlDict` lifts naturally
//! through the `Option<T: WebIdlConvertible>` blanket impl.
//!
//! # Example
//!
//! ```ignore
//! #[derive(WebIdlDict, Default)]
//! struct RequestInit {
//!     method: Option<USVString>,
//!     headers: Option<v8::Local<'s, v8::Value>>,
//!     body: Option<v8::Local<'s, v8::Value>>,
//!     keepalive: Option<bool>,
//!     #[webidl_name = "credentials"]
//!     credentials_field: Option<USVString>,
//! }
//!
//! // User code:
//! let init = RequestInit::from_v8(scope, init_arg)?;
//! ```

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{
    parse_macro_input, Data, DeriveInput, Field, Fields, GenericParam, Lifetime, LifetimeParam,
};

pub fn expand(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let name = &input.ident;

    // Reject anything that's not a struct with named fields. WebIDL
    // dictionaries are conceptually a collection of named members; tuple
    // structs and enums map to the wrong shape.
    let fields = match &input.data {
        Data::Struct(s) => match &s.fields {
            Fields::Named(fields) => &fields.named,
            _ => {
                return syn::Error::new_spanned(
                    name,
                    "#[derive(WebIdlDict)] requires a struct with named fields",
                )
                .to_compile_error()
                .into();
            }
        },
        _ => {
            return syn::Error::new_spanned(
                name,
                "#[derive(WebIdlDict)] requires a struct (not enum / union)",
            )
            .to_compile_error()
            .into();
        }
    };

    // Per-field extraction lines. The spec says "in declaration order"
    // — we honour that by iterating `fields` in source order.
    let field_extractions: Vec<TokenStream2> = match fields
        .iter()
        .map(gen_field_extraction)
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(v) => v,
        Err(err) => return err.to_compile_error().into(),
    };
    let field_assignments: Vec<TokenStream2> = fields
        .iter()
        .map(|f| {
            let id = f.ident.as_ref().unwrap();
            quote! { #id, }
        })
        .collect();

    // Generics handling. The struct may carry a single lifetime param
    // `'s` (e.g. `Option<v8::Local<'s, v8::Value>>` fields). We pass
    // generics straight through to the impl block.
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();

    // For the WebIdlConvertible blanket impl, we need the same generics.
    // If the type has a single lifetime, we re-use it; if it has none,
    // we keep impl<…> empty and the trait method's lifetime erases.
    let has_lifetime = input
        .generics
        .params
        .iter()
        .any(|p| matches!(p, GenericParam::Lifetime(_)));
    let _ = has_lifetime; // suppress unused-warning if we don't branch here

    let expanded = quote! {
        impl #impl_generics #name #ty_generics #where_clause {
            /// Parse a JS value as a WebIDL dictionary of this type.
            ///
            /// Returns `Self::default()` for `null` / `undefined`.
            /// Returns `Err(TypeError)` if `value` is a non-null,
            /// non-undefined non-Object, or if any member fails its
            /// own conversion.
            #[allow(unused_variables, clippy::needless_borrow)]
            pub fn from_v8(
                scope: &mut ::v8::PinScope,
                value: ::v8::Local<::v8::Value>,
            ) -> ::std::result::Result<Self, ::zeroship_runtime::state::OpError> {
                // Step 1: undefined / null → default-construct. Per
                // WebIDL §3.10 step 1, this matches the "no value" path
                // with all members at their declared defaults.
                if value.is_null_or_undefined() {
                    return ::std::result::Result::Ok(<Self as ::core::default::Default>::default());
                }
                // Step 2: require Object. Booleans, numbers, strings,
                // symbols all reject. Functions / Arrays / Promises etc.
                // are Objects so they're permitted (the per-member
                // converters handle the actual type check).
                let __obj: ::v8::Local<::v8::Object> = match value.try_into() {
                    Ok(o) => o,
                    Err(_) => return ::std::result::Result::Err(
                        ::zeroship_runtime::state::OpError::type_error(
                            concat!(
                                "Cannot convert value to dictionary `",
                                stringify!(#name),
                                "` — value is not an object",
                            ),
                        ),
                    ),
                };

                // Step 3: per-member extraction in declaration order.
                #(#field_extractions)*

                ::std::result::Result::Ok(Self { #(#field_assignments)* })
            }
        }

        // Blanket WebIdlConvertible impl so dicts compose inside
        // sequence<T>, record<K, V>, and other dicts. Just delegates
        // to the inherent `from_v8`.
        impl #impl_generics ::zeroship_runtime::convert::WebIdlConvertible for #name #ty_generics #where_clause {
            fn from_v8(
                scope: &mut ::v8::PinScope,
                value: ::v8::Local<::v8::Value>,
            ) -> ::std::result::Result<Self, ::zeroship_runtime::state::OpError> {
                <Self>::from_v8(scope, value)
            }
        }
    };

    expanded.into()
}

/// Extract a single dictionary member.
///
/// Code shape:
/// ```ignore
/// let __key = v8::String::new(scope, "<webidl-name>").unwrap();
/// let <field>: <Ty> = match __obj.get(scope, __key.into()) {
///     Some(__v) if !__v.is_undefined() => {
///         <Ty as WebIdlConvertible>::from_v8(scope, __v)?
///     }
///     _ => <Ty as Default>::default(),
/// };
/// ```
///
/// The Default fallback covers:
///   - missing property (`None` from V8) → default
///   - `undefined`-valued property → default (matches WebIDL §3.10
///     step 5 sub-step "if dict entry is missing" which also fires
///     when the member is present but undefined)
///
/// `Option<T>` lifts via the blanket WebIdlConvertible impl so
/// `Option<USVString>` reads cleanly: the blanket impl returns `None`
/// for null/undefined, `Some(T::from_v8(...))` otherwise. We still
/// need the Default fallback for missing keys (no V8 prop) — it's
/// equivalent for Option but distinct for non-Option fields.
fn gen_field_extraction(field: &Field) -> syn::Result<TokenStream2> {
    let id = field
        .ident
        .as_ref()
        .ok_or_else(|| syn::Error::new_spanned(field, "tuple struct fields not supported"))?;

    // Default WebIDL name = field ident verbatim. Override via
    // `#[webidl_name = "..."]`. Useful when the JS-side name differs
    // from a Rust keyword or convention (e.g. JS `type` → Rust
    // `type_field`).
    let webidl_name = extract_webidl_name(&field.attrs).unwrap_or_else(|| id.to_string());
    let ty = &field.ty;

    Ok(quote! {
        let #id: #ty = {
            let __key = ::v8::String::new(scope, #webidl_name).unwrap();
            match __obj.get(scope, __key.into()) {
                ::std::option::Option::Some(__v) if !__v.is_undefined() => {
                    <#ty as ::zeroship_runtime::convert::WebIdlConvertible>::from_v8(scope, __v)?
                }
                _ => <#ty as ::core::default::Default>::default(),
            }
        };
    })
}

fn extract_webidl_name(attrs: &[syn::Attribute]) -> Option<String> {
    for attr in attrs {
        if !attr.path().is_ident("webidl_name") {
            continue;
        }
        if let syn::Meta::NameValue(nv) = &attr.meta {
            if let syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(s),
                ..
            }) = &nv.value
            {
                return Some(s.value());
            }
        }
    }
    None
}

/// Reserved for future extension — `[EnforceRange]` attr on integer
/// fields. Today integer fields use the bare `u32` / `i32` impl which
/// does ToUint32 / ToInt32 (modular). For dicts that need spec-strict
/// EnforceRange behaviour, a `#[webidl_dict(enforce_range)]` attr could
/// flip the per-field reader. Not implemented in v1; deferred until a
/// concrete consumer needs it.
#[allow(dead_code)]
fn extract_enforce_range(_attrs: &[syn::Attribute]) -> bool {
    // Stub — no consumer yet.
    false
}

/// Reserved for future extension — letting users plug a custom reader
/// for non-WebIdlConvertible field types. Today every field type MUST
/// implement WebIdlConvertible (which means: primitives, Option<T>,
/// Local<Value>, plus user types via WebIdlDict / WebIdlEnum derives).
/// A `#[webidl_dict(custom_extractor = "fn_name")]` attr could route
/// the read through a user-supplied function for special cases. Not
/// implemented in v1.
#[allow(dead_code)]
fn extract_custom_extractor(_attrs: &[syn::Attribute]) -> Option<String> {
    None
}

// Suppress unused-warning on imports kept for forward-compat hooks.
#[allow(dead_code)]
fn _suppress_unused() {
    let _ = std::any::type_name::<Lifetime>();
    let _ = std::any::type_name::<LifetimeParam>();
}
