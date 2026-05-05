//! Insta snapshot tests for `#[derive(WebIdlDict)]` codegen.
//!
//! Wave 7 commit 2 (F9) — locks the WebIdlDict derive's emit shape
//! against drift, mirroring the pattern in
//! `v8_class/snapshot_tests.rs`. The snapshots live at the workspace
//! default `crates/runtime-macros/src/snapshots/` (insta resolves
//! relative to the test file).
//!
//! The 3 representative shapes:
//!
//! - `dict_basic` — a struct with one `Option<USVString>` field
//!   (the simplest possible dictionary). Locks the `from_v8` skeleton
//!   + the per-member tc-scoped extraction.
//! - `dict_with_reject_null` — a member carrying
//!   `#[webidl_dict_member(reject_null)]`. Locks the WebIDL §3.13.27
//!   nullable-member null-vs-undefined branching.
//! - `dict_with_renamed_member` — a member with `#[webidl_name = "..."]`
//!   to verify the rename lookup-key codegen. Common shape for fields
//!   whose JS name is a Rust keyword (e.g. JS `type` → Rust `type_field`).

use crate::webidl_dict::expand_tokens;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;

/// Format the macro output through prettyplease so the snapshot
/// stays diff-friendly across whitespace tweaks in `quote!`.
fn format_expansion(out: TokenStream2) -> String {
    let parsed: syn::File = syn::parse2(out).expect("macro output parses as items");
    prettyplease::unparse(&parsed)
}

/// Basic dictionary — a struct with one `Option<USVString>` field.
/// Locks the no-flag (default) extraction path.
#[test]
fn snapshot_dict_basic() {
    let item = quote! {
        struct RequestInit {
            method: Option<USVString>,
        }
    };
    let out = expand_tokens(item);
    insta::assert_snapshot!("dict_basic", format_expansion(out));
}

/// Dictionary with `#[webidl_dict_member(reject_null)]` — the
/// null-vs-undefined branching path. Locks the "null is not allowed"
/// TypeError emission per WebIDL §3.13.27.
#[test]
fn snapshot_dict_with_reject_null() {
    let item = quote! {
        struct AbortHolder {
            #[webidl_dict_member(reject_null)]
            signal: USVString,
        }
    };
    let out = expand_tokens(item);
    insta::assert_snapshot!("dict_with_reject_null", format_expansion(out));
}

/// Dictionary with `#[webidl_name = "..."]` rename — verifies the
/// JS-side property key uses the renamed value while the Rust field
/// keeps the original ident. The common shape for JS-keyword field
/// names like `type` (renamed to `type_field` on the Rust side).
#[test]
fn snapshot_dict_with_renamed_member() {
    let item = quote! {
        struct EventInit {
            #[webidl_name = "type"]
            type_field: USVString,
            bubbles: Option<bool>,
        }
    };
    let out = expand_tokens(item);
    insta::assert_snapshot!("dict_with_renamed_member", format_expansion(out));
}
