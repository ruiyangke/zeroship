//! `#[derive(WebIdlEnum)]` — WebIDL §3.7.10 enum types.
//!
//! WebIDL enums are a closed set of string values:
//!
//! ```ignore
//! enum RequestMode { "navigate", "same-origin", "no-cors", "cors" };
//! ```
//!
//! The Rust mapping is a unit-variant enum where each variant maps to
//! one WebIDL string name. By default, the WebIDL name is the variant
//! ident kebab-cased (`NoCors` → `no-cors`); override per-variant with
//! `#[webidl_name = "..."]`.
//!
//! Codegen emits:
//!   - `fn from_str(s: &str) -> Option<Self>` — name → variant, None
//!     on unknown name.
//!   - `fn as_str(&self) -> &'static str` — variant → name.
//!   - `impl WebIdlConvertible for E` — the JS-boundary path:
//!     1. Coerce value to a string (V::ToString, surfaces as TypeError
//!        if e.g. Symbol).
//!     2. Look up via `from_str`. Unknown → TypeError per §3.13.7
//!        step 4 ("if S is not one of E's enumeration values, throw
//!        a TypeError").
//!
//! The derive does NOT require `Self: Default` — but if the user adds
//! `#[derive(Default)]` themselves the WebIdlDict-as-member path can
//! still default-construct on missing dict keys.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{parse_macro_input, Data, DeriveInput, Fields, Variant};

pub fn expand(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let name = &input.ident;

    let variants = match &input.data {
        Data::Enum(e) => &e.variants,
        _ => {
            return syn::Error::new_spanned(
                name,
                "#[derive(WebIdlEnum)] requires an enum",
            )
            .to_compile_error()
            .into();
        }
    };

    // Reject non-unit variants. WebIDL enum variants carry no payload.
    for v in variants {
        if !matches!(v.fields, Fields::Unit) {
            return syn::Error::new_spanned(
                v,
                "#[derive(WebIdlEnum)]: variants must be unit-style (no payload)",
            )
            .to_compile_error()
            .into();
        }
    }

    let from_str_arms: Vec<TokenStream2> = variants
        .iter()
        .map(|v| {
            let id = &v.ident;
            let webidl_name = webidl_name_for(v);
            quote! { #webidl_name => ::std::option::Option::Some(Self::#id), }
        })
        .collect();

    let as_str_arms: Vec<TokenStream2> = variants
        .iter()
        .map(|v| {
            let id = &v.ident;
            let webidl_name = webidl_name_for(v);
            quote! { Self::#id => #webidl_name, }
        })
        .collect();

    // Build the human-readable list of accepted names for the
    // TypeError message — debuggers will thank us.
    let accepted_list: Vec<String> = variants
        .iter()
        .map(|v| format!("'{}'", webidl_name_for(v)))
        .collect();
    let accepted_str = accepted_list.join(", ");

    // Generic-pass-through (rare for enums, but let's not box callers in).
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();

    let expanded = quote! {
        impl #impl_generics #name #ty_generics #where_clause {
            /// WebIDL name → variant. None on unknown name.
            ///
            /// Codegen exhaustively maps the variant set; adding or
            /// removing a variant updates this function automatically.
            #[allow(dead_code)]
            pub fn from_str(__s: &str) -> ::std::option::Option<Self> {
                match __s {
                    #(#from_str_arms)*
                    _ => ::std::option::Option::None,
                }
            }

            /// Variant → WebIDL name. Round-trips with `from_str`.
            #[allow(dead_code)]
            pub fn as_str(&self) -> &'static str {
                match self {
                    #(#as_str_arms)*
                }
            }
        }

        impl #impl_generics ::zeroship_runtime::convert::WebIdlConvertible for #name #ty_generics #where_clause {
            fn from_v8(
                scope: &mut ::v8::PinScope,
                value: ::v8::Local<::v8::Value>,
            ) -> ::std::result::Result<Self, ::zeroship_runtime::state::OpError> {
                // Step 1: ToString. Symbols → TypeError naturally.
                let __s = value.to_string(scope).ok_or_else(|| {
                    ::zeroship_runtime::state::OpError::type_error(
                        concat!(
                            "Cannot convert value to enum `",
                            stringify!(#name),
                            "` (ToString failed)",
                        ),
                    )
                })?;
                let __rust_str = __s.to_rust_string_lossy(scope);

                // Step 2: name lookup. Unknown → TypeError per
                // §3.13.7 step 4. We include both the offending value
                // AND the accepted set in the message — saves a round
                // of "what enum values are valid?" debugging.
                Self::from_str(&__rust_str).ok_or_else(|| {
                    ::zeroship_runtime::state::OpError::type_error(format!(
                        "Cannot convert value `{}` to enum `{}`. Expected one of: {}",
                        __rust_str,
                        stringify!(#name),
                        #accepted_str,
                    ))
                })
            }
        }
    };

    expanded.into()
}

/// Resolve a variant's WebIDL name.
///
/// Priority:
///   1. `#[webidl_name = "..."]` on the variant — wins.
///   2. Otherwise: ident kebab-cased — `NoCors` → `no-cors`,
///      `SameOrigin` → `same-origin`, `Navigate` → `navigate`.
///
/// The kebab-case rule reflects the convention in
/// `https://fetch.spec.whatwg.org/` where enums use lower-kebab. Single-
/// word PascalCase variants stay single-word lowercase (`Navigate` →
/// `navigate`).
fn webidl_name_for(v: &Variant) -> String {
    if let Some(name) = extract_webidl_name(&v.attrs) {
        return name;
    }
    pascal_to_kebab(&v.ident.to_string())
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

/// Convert PascalCase → kebab-case. `NoCors` → `no-cors`,
/// `Navigate` → `navigate`, `Iso8859Text` → `iso8859-text`.
///
/// Rule: emit a `-` before each uppercase ASCII letter that follows a
/// lowercase ASCII letter or a digit. (Doesn't insert before runs of
/// uppercase — `IO` stays `io` after first-letter handling.)
fn pascal_to_kebab(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    let chars: Vec<char> = s.chars().collect();
    for (i, ch) in chars.iter().enumerate() {
        if i > 0 && ch.is_ascii_uppercase() {
            // Insert `-` before this uppercase letter if the previous
            // char was lowercase or a digit. Skip the dash if the
            // previous char was also uppercase (handles initialisms
            // like `URL` → `url`, not `u-r-l`).
            let prev = chars[i - 1];
            if prev.is_ascii_lowercase() || prev.is_ascii_digit() {
                out.push('-');
            }
        }
        out.extend(ch.to_lowercase());
    }
    out
}

#[cfg(test)]
mod kebab_tests {
    use super::pascal_to_kebab;

    #[test]
    fn single_word() {
        assert_eq!(pascal_to_kebab("Navigate"), "navigate");
        assert_eq!(pascal_to_kebab("Cors"), "cors");
    }

    #[test]
    fn two_words() {
        assert_eq!(pascal_to_kebab("NoCors"), "no-cors");
        assert_eq!(pascal_to_kebab("SameOrigin"), "same-origin");
    }

    #[test]
    fn initialism_at_start() {
        // URL → url (no dash; consecutive uppercase doesn't split)
        assert_eq!(pascal_to_kebab("URL"), "url");
    }

    #[test]
    fn digit_then_upper() {
        // Iso8859Text → iso8859-text (digit precedes T; insert dash)
        assert_eq!(pascal_to_kebab("Iso8859Text"), "iso8859-text");
    }
}
