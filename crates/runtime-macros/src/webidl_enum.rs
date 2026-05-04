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
//!
//! # Type-level flags
//!
//! `#[webidl_enum(case_insensitive)]` switches `from_str` and `from_v8`
//! to ASCII case-insensitive matching (variant names are stored as
//! literals; lookup uses `eq_ignore_ascii_case`). Used by WebCrypto
//! `HashAlgo` where `"SHA-256"` / `"sha-256"` / `"Sha-256"` all match.
//! WebIDL is ASCII for enum names so we deliberately use the ASCII
//! comparison — Unicode case folding is not in scope.
//!
//! `#[webidl_enum(silent_default)]` makes `from_str` return
//! `Some(Self::default())` on unknown and `from_v8` return
//! `Self::default()` (never throws). Used by Fetch / WebSocket sections
//! that fall through to the default on unknown (`RedirectMode`,
//! `CredentialsMode`, `BinaryType`). Requires `Self: Default` — the
//! generated code references `<Self as Default>::default()` so the
//! compiler errors on `silent_default` enums lacking the impl.
//!
//! Flags combine: `#[webidl_enum(silent_default, case_insensitive)]`.

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

    // Parse type-level `#[webidl_enum(...)]` flags. Unknown idents
    // surface as a syn error so typos don't silently downgrade
    // behaviour.
    let flags = match parse_enum_flags(&input.attrs) {
        Ok(f) => f,
        Err(err) => return err.to_compile_error().into(),
    };

    // Per-variant arms. Case-sensitive mode dispatches via a `match`
    // against literal names (cheapest, what the macro emitted before
    // case-insensitive support). Case-insensitive mode emits an
    // if/else-if ladder using `eq_ignore_ascii_case` — slightly
    // costlier than a perfect-hash match but still O(n) on a closed
    // set, and the n is tiny (hash algos top out at ~5 variants).
    let from_str_body: TokenStream2 = if flags.case_insensitive {
        let arms: Vec<TokenStream2> = variants
            .iter()
            .map(|v| {
                let id = &v.ident;
                let webidl_name = webidl_name_for(v);
                quote! {
                    if __s.eq_ignore_ascii_case(#webidl_name) {
                        return ::std::option::Option::Some(Self::#id);
                    }
                }
            })
            .collect();
        let unknown_arm = if flags.silent_default {
            quote! {
                ::std::option::Option::Some(<Self as ::core::default::Default>::default())
            }
        } else {
            quote! { ::std::option::Option::None }
        };
        quote! {
            #(#arms)*
            #unknown_arm
        }
    } else {
        let arms: Vec<TokenStream2> = variants
            .iter()
            .map(|v| {
                let id = &v.ident;
                let webidl_name = webidl_name_for(v);
                quote! { #webidl_name => ::std::option::Option::Some(Self::#id), }
            })
            .collect();
        let unknown_arm = if flags.silent_default {
            quote! {
                _ => ::std::option::Option::Some(<Self as ::core::default::Default>::default()),
            }
        } else {
            quote! { _ => ::std::option::Option::None, }
        };
        quote! {
            match __s {
                #(#arms)*
                #unknown_arm
            }
        }
    };

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

    // `from_v8` body diverges on `silent_default`. The throwing path
    // (default) is the existing behaviour; the silent-default path
    // never throws — it returns `Self::default()` on every off-spec
    // input, including ToString failures (Symbols, throwing toString).
    // The fetch / WebSocket consumers expect this tolerance per spec.
    let from_v8_body = if flags.silent_default {
        quote! {
            // Symbol or throwing toString → fall through to default.
            // We deliberately do NOT propagate the V8 exception: the
            // spec sections using this flag say "if not one of the
            // listed values, use the default" — coercion failure
            // counts as "not one of" too.
            let __opt_str = value
                .to_string(scope)
                .map(|__s| __s.to_rust_string_lossy(scope));
            ::std::result::Result::Ok(match __opt_str {
                ::std::option::Option::Some(__rust_str) => Self::from_str(&__rust_str)
                    .unwrap_or_else(<Self as ::core::default::Default>::default),
                ::std::option::Option::None => {
                    <Self as ::core::default::Default>::default()
                }
            })
        }
    } else {
        quote! {
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
    };

    let expanded = quote! {
        impl #impl_generics #name #ty_generics #where_clause {
            /// WebIDL name → variant. None on unknown name (or
            /// `Some(Self::default())` if the type carries
            /// `#[webidl_enum(silent_default)]`).
            ///
            /// Codegen exhaustively maps the variant set; adding or
            /// removing a variant updates this function automatically.
            #[allow(dead_code)]
            pub fn from_str(__s: &str) -> ::std::option::Option<Self> {
                #from_str_body
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
                #from_v8_body
            }
        }
    };

    expanded.into()
}

/// Type-level flags parsed from `#[webidl_enum(...)]` on the enum.
#[derive(Default, Debug, Clone, Copy)]
struct EnumFlags {
    case_insensitive: bool,
    silent_default: bool,
}

/// Parse `#[webidl_enum(case_insensitive)]` /
/// `#[webidl_enum(silent_default)]` /
/// `#[webidl_enum(silent_default, case_insensitive)]`.
///
/// Surface unknown flags as a compile error so a typo
/// (`#[webidl_enum(case_isensitive)]`) doesn't silently fall back to
/// case-sensitive matching.
fn parse_enum_flags(attrs: &[syn::Attribute]) -> syn::Result<EnumFlags> {
    let mut out = EnumFlags::default();
    for attr in attrs {
        if !attr.path().is_ident("webidl_enum") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("case_insensitive") {
                out.case_insensitive = true;
            } else if meta.path.is_ident("silent_default") {
                out.silent_default = true;
            } else {
                return Err(meta.error(
                    "unknown #[webidl_enum] flag (expected `case_insensitive` or `silent_default`)",
                ));
            }
            Ok(())
        })?;
    }
    Ok(out)
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
