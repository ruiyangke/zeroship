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
    // (default) is the existing behaviour, augmented with a tc_scope
    // around `value.to_string(scope)` to capture user-thrown
    // exceptions (custom `toString` / `Symbol.toPrimitive`); the
    // silent-default path swallows everything (deliberately —
    // tolerance for off-spec inputs is the whole point).
    let from_v8_body = if flags.silent_default {
        quote! {
            // Symbol or throwing toString → fall through to default.
            // The captured exception is intentionally discarded — the
            // spec sections using this flag say "if not one of the
            // listed values, use the default", which subsumes "the
            // value couldn't be coerced". We use a tc_scope so the
            // exception state doesn't leak out to the caller.
            let __opt_str: ::std::option::Option<::std::string::String> = {
                ::v8::tc_scope!(let __tc, scope);
                let __r = value.to_string(__tc).map(|__s| __s.to_rust_string_lossy(__tc));
                let _ = __tc.exception(); // clear pending state
                __r
            };
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
            // Step 1: ToString in a tc_scope so user-thrown exceptions
            // (custom toString, Symbol.toPrimitive) propagate verbatim.
            // Pre-fix, value.to_string returning None forced us to
            // fabricate a TypeError, hiding the user's original throw.
            let __coerced: ::std::result::Result<::std::string::String, ::zeroship_runtime::macro_runtime::state::OpError> = {
                ::v8::tc_scope!(let __tc, scope);
                match value.to_string(__tc) {
                    ::std::option::Option::Some(__s) => {
                        ::std::result::Result::Ok(__s.to_rust_string_lossy(__tc))
                    }
                    ::std::option::Option::None => {
                        if __tc.has_caught() {
                            let __exc = __tc.exception().expect("has_caught implies Some");
                            ::std::result::Result::Err(
                                ::zeroship_runtime::macro_runtime::state::OpError::js_value(
                                    __tc,
                                    __exc,
                                    concat!(
                                        "Cannot convert value to enum `",
                                        stringify!(#name),
                                        "` (user code threw)",
                                    ),
                                ),
                            )
                        } else {
                            // ToString returned None without raising —
                            // shouldn't normally happen, but handle as
                            // a generic TypeError for safety.
                            ::std::result::Result::Err(
                                ::zeroship_runtime::macro_runtime::state::OpError::type_error(concat!(
                                    "Cannot convert value to enum `",
                                    stringify!(#name),
                                    "` (ToString failed)",
                                )),
                            )
                        }
                    }
                }
            };
            let __rust_str = __coerced?;

            // Step 2: name lookup. Unknown → TypeError per
            // §3.13.7 step 4. We include both the offending value
            // AND the accepted set in the message — saves a round
            // of "what enum values are valid?" debugging.
            Self::from_str(&__rust_str).ok_or_else(|| {
                ::zeroship_runtime::macro_runtime::state::OpError::type_error(format!(
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

        impl #impl_generics ::zeroship_runtime::macro_runtime::convert::WebIdlConvertible for #name #ty_generics #where_clause {
            fn from_v8(
                scope: &mut ::v8::PinScope,
                value: ::v8::Local<::v8::Value>,
            ) -> ::std::result::Result<Self, ::zeroship_runtime::macro_runtime::state::OpError> {
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
/// `Navigate` → `navigate`, `Iso8859Text` → `iso8859-text`,
/// `APIKey` → `api-key`, `XMLHttpRequest` → `xml-http-request`,
/// `URL` → `url`, `IP` → `ip`.
///
/// Rule: emit a `-` before each uppercase ASCII letter `c` at index
/// `i > 0` if EITHER:
///   1. `chars[i - 1]` is lowercase or a digit  (lower→Upper boundary;
///      handles `NoCors`, `Iso8859Text`).
///   2. `chars[i - 1]` is uppercase AND `chars[i + 1]` exists and is
///      lowercase  (Upper→Upper-then-lower boundary; handles
///      `APIKey` → `api-key`, `XMLHttpRequest` → `xml-http-request`).
///
/// Pre-2026-05-05 the function only implemented rule (1), so trailing
/// initialism + word combinations like `APIKey` collapsed to `apikey`
/// (per WHATWG conventions: should be `api-key`). Single-word
/// initialisms like `URL` and `IP` correctly stay as `url` / `ip` —
/// rule (2) requires a following lowercase, which a trailing
/// uppercase or end-of-string doesn't satisfy.
fn pascal_to_kebab(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    let chars: Vec<char> = s.chars().collect();
    for (i, ch) in chars.iter().enumerate() {
        if i > 0 && ch.is_ascii_uppercase() {
            let prev = chars[i - 1];
            // Rule 1: lower→Upper or digit→Upper boundary.
            let lower_to_upper = prev.is_ascii_lowercase() || prev.is_ascii_digit();
            // Rule 2: Upper→Upper-then-lower (initialism's last
            // letter starts a new word). The bound check on `i + 1`
            // is intentional — at end-of-string we keep the run
            // collapsed (so `URL` stays `url`, not `ur-l`).
            let trailing_initialism = prev.is_ascii_uppercase()
                && chars.get(i + 1).is_some_and(|n| n.is_ascii_lowercase());
            if lower_to_upper || trailing_initialism {
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
        // URL → url (no dash; consecutive uppercase doesn't split when
        // there's no following lowercase)
        assert_eq!(pascal_to_kebab("URL"), "url");
    }

    #[test]
    fn digit_then_upper() {
        // Iso8859Text → iso8859-text (digit precedes T; insert dash)
        assert_eq!(pascal_to_kebab("Iso8859Text"), "iso8859-text");
    }

    #[test]
    fn single_letter_initialism() {
        // IP → ip (single-letter initialism; no dash needed)
        assert_eq!(pascal_to_kebab("IP"), "ip");
    }

    #[test]
    fn trailing_initialism_word() {
        // APIKey → api-key (initialism + word; the new rule)
        assert_eq!(pascal_to_kebab("APIKey"), "api-key");
        // Multi-segment shape: XMLHttpRequest → xml-http-request
        assert_eq!(pascal_to_kebab("XMLHttpRequest"), "xml-http-request");
    }

    #[test]
    fn initialism_at_end() {
        // Ends in initialism with no following lowercase — stays
        // collapsed. Examples: AsURL → as-url (URL run preserved).
        assert_eq!(pascal_to_kebab("AsURL"), "as-url");
    }
}
