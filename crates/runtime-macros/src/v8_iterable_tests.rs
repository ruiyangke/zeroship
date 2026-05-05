//! Insta snapshot tests for `#[v8_iterable(...)]` codegen.
//!
//! Wave 7 commit 2 (F9) — locks the v8_iterable codegen's emit shape
//! against drift, mirroring the pattern in
//! `v8_class/snapshot_tests.rs`.
//!
//! The 2 representative shapes:
//!
//! - `iterable_snapshot_mode` — default mode; factory clones
//!   `value_pairs()` once and the iterator walks the snapshot.
//!   Locks the snapshot-mode iterator state + `next()` codegen.
//! - `iterable_live_mode` — `mode = live`; each `next()` re-reads
//!   `value_pairs` on the parent. Locks the live-mode parent-deref
//!   + per-call cursor recovery.
//!
//! We invoke the internal `generate` function directly with a
//! synthesised `IterableAttr` + `ValuePairsSig` rather than going
//! through the full `#[v8_class]` driver — that decouples the
//! snapshot from unrelated install/brand codegen and keeps the
//! emitted bytes focused on the iterable surface.

use crate::v8_iterable::{generate, IterMode, IterableAttr, ValuePairsSig};
use proc_macro2::TokenStream as TokenStream2;
use quote::format_ident;

/// Format the macro output through prettyplease so the snapshot
/// stays diff-friendly across whitespace tweaks in `quote!`.
fn format_expansion(out: TokenStream2) -> String {
    let parsed: syn::File = syn::parse2(out).expect("macro output parses as items");
    prettyplease::unparse(&parsed)
}

fn parse_ty(s: &str) -> syn::Type {
    syn::parse_str::<syn::Type>(s).expect("type parses")
}

/// Snapshot mode (the default). Factory clones `value_pairs()` once
/// at iterator-factory call time and the iterator walks the
/// `Vec<(K, V)>` baked into its state. Locks the snapshot-state
/// shape + the iterator's `next()` indexing codegen.
#[test]
fn snapshot_iterable_snapshot_mode() {
    let class_ty = format_ident!("ReadOnlyMap");
    let state_ty = format_ident!("ReadOnlyMap");
    let attr = IterableAttr {
        key_ty: parse_ty("ByteString"),
        value_ty: parse_ty("ByteString"),
        mode: IterMode::Snapshot,
        value_marshal: None,
    };
    let sig = ValuePairsSig {
        is_mut: false,
        takes_scope: false,
    };
    let out = generate(&class_ty, &state_ty, &attr, sig).expect("generate ok");
    insta::assert_snapshot!("iterable_snapshot_mode", format_expansion(out));
}

/// Live mode (per WebIDL §3.7.10.2). Factory stashes a
/// `Global<Object>` reference to the parent and `next()` re-reads
/// `value_pairs()` afresh each call. Locks the live-mode parent-
/// deref + per-call cursor recovery codegen.
#[test]
fn snapshot_iterable_live_mode() {
    let class_ty = format_ident!("Headers");
    let state_ty = format_ident!("Headers");
    let attr = IterableAttr {
        key_ty: parse_ty("ByteString"),
        value_ty: parse_ty("ByteString"),
        mode: IterMode::Live,
        value_marshal: None,
    };
    let sig = ValuePairsSig {
        is_mut: true,
        takes_scope: false,
    };
    let out = generate(&class_ty, &state_ty, &attr, sig).expect("generate ok");
    insta::assert_snapshot!("iterable_live_mode", format_expansion(out));
}
