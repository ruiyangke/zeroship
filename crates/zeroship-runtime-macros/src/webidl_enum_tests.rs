//! Insta snapshot tests for `#[derive(WebIdlEnum)]` codegen.
//!
//! Locks the WebIdlEnum derive's emit shape
//! against drift, mirroring the pattern in
//! `v8_class/snapshot_tests.rs`.
//!
//! The 3 representative shapes:
//!
//! - `enum_basic` — a unit-variant enum with default kebab-cased
//!   names. Locks the case-sensitive `match`-arm ladder + the
//!   throwing `from_v8` body.
//! - `enum_case_insensitive` — `#[webidl_enum(case_insensitive)]`
//!   switches `from_str` to `eq_ignore_ascii_case`. Locks the
//!   if/else-if ladder shape used by WebCrypto HashAlgo.
//! - `enum_silent_default` — `#[webidl_enum(silent_default)]` makes
//!   `from_v8` non-throwing (default-on-unknown). Locks the swallowed-
//!   exception path used by Fetch RedirectMode / CredentialsMode.

use crate::webidl_enum::expand_tokens;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;

/// Format the macro output through prettyplease so the snapshot
/// stays diff-friendly across whitespace tweaks in `quote!`.
fn format_expansion(out: TokenStream2) -> String {
    let parsed: syn::File = syn::parse2(out).expect("macro output parses as items");
    prettyplease::unparse(&parsed)
}

/// Basic enum — kebab-case derivation (`NoCors` → `"no-cors"`),
/// case-sensitive matching, throwing on unknown. The shape used by
/// most WebIDL enum types (e.g. `RequestMode`).
#[test]
fn snapshot_enum_basic() {
    let item = quote! {
        enum RequestMode {
            Navigate,
            SameOrigin,
            NoCors,
            Cors,
        }
    };
    let out = expand_tokens(item);
    insta::assert_snapshot!("enum_basic", format_expansion(out));
}

/// `#[webidl_enum(case_insensitive)]` — `from_str` uses
/// `eq_ignore_ascii_case`. Locks the if/else-if ladder shape and the
/// per-call `to_string` tc-scope.
#[test]
fn snapshot_enum_case_insensitive() {
    let item = quote! {
        #[webidl_enum(case_insensitive)]
        enum HashAlgo {
            #[webidl_name = "SHA-1"]
            Sha1,
            #[webidl_name = "SHA-256"]
            Sha256,
            #[webidl_name = "SHA-512"]
            Sha512,
        }
    };
    let out = expand_tokens(item);
    insta::assert_snapshot!("enum_case_insensitive", format_expansion(out));
}

/// `#[webidl_enum(silent_default)]` — unknown values fall through to
/// `Self::default()` rather than throwing. Locks the swallowed-
/// exception path used by Fetch's `RedirectMode` / `BinaryType`.
#[test]
fn snapshot_enum_silent_default() {
    let item = quote! {
        #[webidl_enum(silent_default)]
        enum RedirectMode {
            #[default]
            Follow,
            Error,
            Manual,
        }
    };
    let out = expand_tokens(item);
    insta::assert_snapshot!("enum_silent_default", format_expansion(out));
}
