//! Attribute parsing + signature inspection for `#[v8_iterable]`.
//!
//! Wave 9 split (per design `docs/proposals/runtime-macros-refactor.md`
//! Wave 9 god-file decomposition + the v2 architecture-critic R2).
//! Pre-Wave-9 these types lived alongside the codegen in
//! `crates/runtime-macros/src/v8_iterable.rs` (1,368 LOC). The Wave 9
//! split mirrors `v8_class/`'s parse/emit/shared layout.
//!
//! Two responsibilities:
//!   1. Parse `#[v8_iterable(key = ..., value = ..., mode = ..., …)]`
//!      from impl-block attrs into [`IterableAttr`].
//!   2. Sniff the user-supplied `value_pairs` method's signature into
//!      [`ValuePairsSig`] so codegen knows which pointer/borrow
//!      recovery and arg-passing convention to emit.
//!
//! Codegen lives in `emit_factory.rs` (companion class + factory
//! callbacks) and `emit_iterator.rs` (forEach + next + value
//! marshalling). The orchestrator `generate` lives in `mod.rs`.

use syn::{Attribute, FnArg, ImplItem, Receiver};

/// Iteration model for the derive — snapshot or live (WebIDL §3.7.10.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IterMode {
    /// Default. Factory clones `value_pairs()` once at call time and the
    /// iterator walks the snapshot. Subsequent mutations to the parent
    /// are NOT observed. Suits read-only iterables (the common case).
    Snapshot,
    /// Spec-mandated live mode. Each `next()` re-calls `value_pairs()`
    /// on the parent and indexes at the cursor. Mutations between
    /// `next()` calls are visible. Required for Headers / FormData /
    /// URLSearchParams iterators where insertions during iteration must
    /// be observable per WebIDL §3.7.10.2.
    Live,
}

/// Parsed `#[v8_iterable(key = TY, value = TY [, mode = snapshot|live]
/// [, value_marshal = ident])]` attribute.
pub(crate) struct IterableAttr {
    pub key_ty: syn::Type,
    pub value_ty: syn::Type,
    /// Iteration mode. Defaults to `Snapshot` when `mode = ...` is
    /// omitted; back-compat for every existing consumer.
    pub mode: IterMode,
    /// Optional `value_marshal = some_fn` — a free-function path that
    /// the macro calls per-yield to convert a `&V` to
    /// `v8::Local<v8::Value>`. Skips the built-in
    /// USVString/ByteString/u32/Vec<u8> classification so users can
    /// surface arbitrary types (e.g. FormData's
    /// `(USVString or File)` union or any v8 Local).
    ///
    /// Required signature on the callee:
    /// ```ignore
    /// fn some_fn<'s>(
    ///     scope: &mut v8::PinScope<'s, '_>,
    ///     v: &V,
    /// ) -> v8::Local<'s, v8::Value>
    /// ```
    pub value_marshal: Option<syn::Path>,
}

/// Read `#[v8_iterable(key = ..., value = ..., mode = ...)]` from impl-
/// block attrs. Returns `None` if the attribute is absent. Returns
/// `Err` if the attribute is present but malformed (caller surfaces as
/// compile_error).
pub(crate) fn extract_iterable(attrs: &[Attribute]) -> Result<Option<IterableAttr>, syn::Error> {
    let mut found: Option<IterableAttr> = None;
    for attr in attrs {
        if !attr.path().is_ident("v8_iterable") {
            continue;
        }
        let mut key_ty: Option<syn::Type> = None;
        let mut value_ty: Option<syn::Type> = None;
        let mut mode: Option<IterMode> = None;
        let mut value_marshal: Option<syn::Path> = None;
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("key") {
                let ty: syn::Type = meta.value()?.parse()?;
                key_ty = Some(ty);
            } else if meta.path.is_ident("value") {
                let ty: syn::Type = meta.value()?.parse()?;
                value_ty = Some(ty);
            } else if meta.path.is_ident("mode") {
                // Accept a bare ident: `mode = live` / `mode = snapshot`.
                // String-literal form is rejected for consistency with
                // the rest of the IDL-shaped attrs in this codebase,
                // which use ident-or-path values.
                let ident: syn::Ident = meta.value()?.parse()?;
                mode = Some(match ident.to_string().as_str() {
                    "snapshot" => IterMode::Snapshot,
                    "live" => IterMode::Live,
                    other => {
                        return Err(meta.error(format!(
                            "#[v8_iterable]: unrecognised mode `{other}` (expected `snapshot` or `live`)"
                        )));
                    }
                });
            } else if meta.path.is_ident("value_marshal") {
                // Accept a path: `value_marshal = entry_value_to_v8` or
                // `value_marshal = crate::path::to_v8`. The path resolves
                // at call-site of the emitted code (inside the parent
                // class's module), so relative paths are fine for
                // local helpers.
                let path: syn::Path = meta.value()?.parse()?;
                value_marshal = Some(path);
            } else {
                return Err(meta.error(
                    "expected `key = TY`, `value = TY`, `mode = snapshot|live`, or `value_marshal = fn`",
                ));
            }
            Ok(())
        })?;
        let key_ty = key_ty.ok_or_else(|| {
            syn::Error::new_spanned(
                attr,
                "#[v8_iterable]: missing `key = TY` (e.g. `key = ByteString`)",
            )
        })?;
        let value_ty = value_ty.ok_or_else(|| {
            syn::Error::new_spanned(
                attr,
                "#[v8_iterable]: missing `value = TY` (e.g. `value = ByteString`)",
            )
        })?;
        if found.is_some() {
            return Err(syn::Error::new_spanned(
                attr,
                "#[v8_iterable]: only one occurrence allowed per impl block",
            ));
        }
        found = Some(IterableAttr {
            key_ty,
            value_ty,
            mode: mode.unwrap_or(IterMode::Snapshot),
            value_marshal,
        });
    }
    Ok(found)
}

/// Inspected shape of the user-supplied `value_pairs` method. We sniff
/// it once from the impl block items and thread the result through the
/// generator so we can pick the right pointer/borrow recovery and
/// argument-passing convention.
///
/// Supported shapes (selected by sniffing the receiver + arg list):
///
///   - `fn value_pairs(&self) -> Vec<(K, V)>` — original. `is_mut =
///     false, takes_scope = false`. Recovery is `*const Self` + `&*ptr`.
///   - `fn value_pairs(&mut self) -> Vec<(K, V)>` — Headers' lazy
///     sort-cache. `is_mut = true, takes_scope = false`. Recovery
///     promotes to `*mut Self` + `&mut *ptr`.
///   - `fn value_pairs(&self, scope: &mut PinScope) -> Vec<(K, V)>` —
///     scope-passed read-only. `is_mut = false, takes_scope = true`.
///   - `fn value_pairs(&mut self, scope: &mut PinScope) -> Vec<(K, V)>`
///     — URLSearchParams' sync-from-parent. `is_mut = true, takes_scope
///     = true`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ValuePairsSig {
    /// True if the receiver is `&mut self`. Drives `*mut Self` + `&mut *ptr`
    /// recovery and a per-method per-instance re-entrancy guard.
    pub is_mut: bool,
    /// True if the second argument is `&mut PinScope` (or any
    /// `PinScope` shape we accept). When set, the macro passes the
    /// outer scope through to `value_pairs`. The user's body may then
    /// call any scope-taking helper (e.g. `sync_from_parent(scope)`).
    pub takes_scope: bool,
}

impl Default for ValuePairsSig {
    fn default() -> Self {
        ValuePairsSig {
            is_mut: false,
            takes_scope: false,
        }
    }
}

/// Find the `value_pairs` method in the impl block items and inspect
/// its signature. Returns the default (`&self`, no scope) if no method
/// is found — the resulting codegen will fail at compile time with a
/// "no method `value_pairs` on `Self`" error pointing at the call site,
/// which is good enough.
///
/// Recognised receiver shapes:
///   - `&self` → `is_mut = false`
///   - `&mut self` → `is_mut = true`
///
/// Recognised second-arg shapes (everything else fails the codegen):
///   - none → `takes_scope = false`
///   - `&mut v8::PinScope<'…, '…>` (any path ending in `PinScope`) →
///     `takes_scope = true`
pub(crate) fn inspect_value_pairs(items: &[ImplItem]) -> ValuePairsSig {
    for item in items {
        let ImplItem::Fn(func) = item else {
            continue;
        };
        if func.sig.ident != "value_pairs" {
            continue;
        }
        let mut sig = ValuePairsSig::default();
        for arg in func.sig.inputs.iter() {
            match arg {
                FnArg::Receiver(Receiver {
                    mutability,
                    reference: Some(_),
                    ..
                }) => {
                    sig.is_mut = mutability.is_some();
                }
                FnArg::Typed(pt) => {
                    // Detect a `&mut PinScope`-shaped argument by
                    // sniffing the trailing path segment. We don't
                    // require a specific lifetime spelling — the user
                    // may write `&mut v8::PinScope<'s, '_>` or just
                    // `&mut PinScope` if they `use v8::PinScope`.
                    if takes_pin_scope(&pt.ty) {
                        sig.takes_scope = true;
                    }
                }
                _ => {}
            }
        }
        return sig;
    }
    ValuePairsSig::default()
}

/// True if `ty` is some flavour of `&mut PinScope<...>`. We accept any
/// path that ends in the `PinScope` segment so users can spell it as
/// `v8::PinScope`, `::v8::PinScope`, or a bare `PinScope` after `use`.
fn takes_pin_scope(ty: &syn::Type) -> bool {
    let syn::Type::Reference(r) = ty else {
        return false;
    };
    if r.mutability.is_none() {
        return false;
    }
    let syn::Type::Path(tp) = &*r.elem else {
        return false;
    };
    tp.path
        .segments
        .last()
        .map(|s| s.ident == "PinScope")
        .unwrap_or(false)
}
