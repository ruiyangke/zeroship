//! `#[v8_iterable(key = K, value = V [, mode = snapshot|live])]` —
//! emit the WebIDL pair-iterator surface (keys / values / entries /
//! forEach / @@iterator) from a single user-supplied
//! `value_pairs(&self) -> Vec<(K, V)>` method.
//!
//! Per WebIDL §3.7.10.2 (default iterators) and §3.7.10.3 (forEach):
//!
//! ```ignore
//! interface Foo {
//!   iterable<K, V>;
//!   // Emits:
//!   //   keys()    -> FooIterator (kind = Keys)
//!   //   values()  -> FooIterator (kind = Values)
//!   //   entries() -> FooIterator (kind = Entries)  [also @@iterator]
//!   //   forEach(callback, thisArg?) -> undefined
//!   //   FooIterator class with next() -> { value, done }
//! };
//! ```
//!
//! # Iteration model — snapshot vs. live
//!
//! The derive supports BOTH iteration models, selected via the `mode =`
//! flag on the attribute (default = `snapshot` for back-compat):
//!
//!   - **Snapshot** (`mode = snapshot` or omitted): at iterator-factory
//!     call time, the macro calls `value_pairs` ONCE, clones its return
//!     into a `Vec<(K, V)>` baked into the iterator's state, and walks
//!     that snapshot on each `next()`. Subsequent mutations to the
//!     parent collection are NOT visible through the running iterator.
//!     This is a deliberate simplification of the spec — it works
//!     correctly for read-only iterables (the common case) and avoids
//!     re-entering the parent's locked state from inside `next()`.
//!
//!   - **Live** (`mode = live`): each `next()` re-reads
//!     `value_pairs` on the parent and indexes at the current cursor.
//!     Mutations between `next()` calls ARE observable. `forEach`
//!     likewise re-reads `value_pairs()` between callbacks. If the
//!     parent state shrinks below the cursor, `next()` yields `done`.
//!     If it grows, the cursor walks the new entries — that's the spec
//!     behaviour. WebIDL §3.7.10.2 mandates this for collections whose
//!     contents are user-mutable mid-iteration (Headers, FormData,
//!     URLSearchParams).
//!
//! The deviation from spec for snapshot mode is documented per-class
//! via the macro's emitted doc comment so consumers can opt out of
//! the derive when they need live iteration.
//!
//! # Live-mode contract on `value_pairs` (closes design §13.6)
//!
//! Live-mode codegen calls `value_pairs(&[mut] self [, scope])` once
//! per `next()` and once per `forEach` callback iteration. The macro
//! treats the call as **observationally pure** — i.e. the
//! key→value entries it returns must reflect the parent's state at
//! call time without any externally-visible side effect.
//!
//! What this means in practice for the supported live-mode consumers
//! (Headers, FormData, URLSearchParams) and any future ones:
//!
//!   - **Allowed**: lazy materialisation behind a Cell/RefCell (e.g.
//!     `Headers` sorts its entries on first iteration and caches the
//!     result; subsequent `value_pairs()` calls return the cached
//!     `Vec<(K, V)>` without re-sorting). The mutation is internal
//!     and does not flow to JS-visible state.
//!   - **Allowed**: returning a fresh `Vec<(K, V)>` per call — clones
//!     are cheap relative to the V8 callback overhead, and the macro
//!     drops the previous vec at the next call boundary.
//!   - **NOT allowed**: emitting Events, calling user-supplied
//!     callbacks, mutating user-observable state, or returning
//!     entries that vary across calls without a corresponding parent
//!     mutation. The cursor advance in `next()` assumes
//!     `value_pairs()[idx]` resolves to a stable entry given a
//!     stable parent.
//!
//! Today's enforcement is convention-only — the macro doesn't emit
//! runtime asserts because:
//!   1. A correct user wrap (the `&mut self` re-entry guard already
//!      catches concurrent re-entry; a non-mutating implementation
//!      passes the guard trivially).
//!   2. The `cargo test --release` suite for Headers / FormData /
//!      URLSearchParams (`v8_iterable_live_smoke.rs`) covers the
//!      observable shape — any drift from the contract surfaces as
//!      a smoke-test failure, not silent UB.
//!
//! A future debug-build instrumentation hook could `assert!` that
//! two consecutive `value_pairs()` calls with no JS mutation
//! between them return equal vecs (`#[cfg(debug_assertions)]`
//! gated, behind a thread-local guard). Deferred — current
//! consumer count (3) doesn't justify the per-call overhead.
//!
//! # Type bounds on K, V
//!
//! The macro emits `v8::String::new_from_one_byte(scope, &k)` for
//! `ByteString` and `v8::String::new(scope, &k)` for stringly types.
//! It supports:
//!
//! - `K: ByteString | USVString | String | u32`
//! - `V: same set, plus Vec<u8>`
//!
//! Other types are rejected with a compile_error in the codegen below.
//!
//! # Wave 9 split
//!
//! Pre-Wave-9 this module was a 1,368-LOC god file. Per design's
//! Wave 9 architectural decomposition (mirroring `v8_class/`'s
//! parse/emit/shared layout), the file split into:
//!
//!   - `parse.rs`         — `IterableAttr`, `ValuePairsSig`,
//!                          `IterMode`, `extract_iterable`,
//!                          `inspect_value_pairs`.
//!   - `value_marshal.rs` — `SupportedTy`, `classify_ty`, `gen_to_v8`.
//!   - `emit_factory.rs`  — companion `<Class>Iterator` struct +
//!                          install + factory callbacks + install
//!                          bridge (`__zs_install_iterable_methods`).
//!   - `emit_iterator.rs` — forEach + next callbacks + reentry guard.
//!   - `mod.rs`           — orchestrator + the `EmitCtx` shared state.

use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::Ident;

use crate::must_str;

mod emit_factory;
mod emit_iterator;
mod parse;
mod reentry;
mod value_marshal;

pub(crate) use parse::{extract_iterable, inspect_value_pairs, IterMode, IterableAttr, ValuePairsSig};

/// Shared codegen context threaded through the per-section helpers in
/// `emit_factory` and `emit_iterator`. Pre-Wave-9 these locals were
/// hand-passed inside one giant `generate` function; the context
/// struct collects them once at orchestrator entry and lets the
/// helpers read whichever fields they need.
///
/// `#[allow(dead_code)]` is intentional — several fields (e.g.
/// `state_ty`, `is_mut`, `takes_scope`, the `iter_*_name_str`
/// strings) are kept on the context for symmetry with the
/// orchestrator's intermediate state and for any future emit
/// fragment to read without changing the context API. Today's
/// helpers happen to splice the must-str-rendered token form
/// (`class_name_init` etc.) instead of the raw String.
#[allow(dead_code)]
pub(super) struct EmitCtx<'a> {
    pub class_ty: &'a Ident,
    pub state_ty: &'a Ident,
    pub key_ty: &'a syn::Type,
    pub value_ty: &'a syn::Type,
    pub value_marshal: Option<&'a syn::Path>,
    pub live: bool,
    pub is_mut: bool,
    pub takes_scope: bool,

    // Derived idents.
    pub iter_class_ty: Ident,
    pub iter_class_name_str: String,
    pub iter_to_string_tag_str: String,
    pub iter_install_slot_ty: Ident,
    pub iter_brand_slot_ty: Ident,
    pub iter_brand_check_fn: Ident,
    pub factory_keys_ident: Ident,
    pub factory_values_ident: Ident,
    pub factory_entries_ident: Ident,
    pub for_each_ident: Ident,
    pub next_ident: Ident,

    // Iterator-kind discriminants.
    pub iter_kind_keys: i32,
    pub iter_kind_values: i32,
    pub iter_kind_entries: i32,

    // Receiver-flavoured pointer + borrow tokens. `&mut self` requires
    // `*mut Self` + `&mut *ptr` (matches the regular `&mut self` method
    // recovery in v8_class/mod.rs::gen_method_callback). The cast is to
    // `state_ty` — that's what's actually in the Box at internal field
    // 0. Under `#[v8_state_marker(M)] impl S`, `class_ty == M` (naming)
    // but `state_ty == S` (storage + receiver of `value_pairs`).
    pub self_ptr_ty: TokenStream2,
    pub self_borrow: TokenStream2,
    pub self_borrow_ty: TokenStream2,
    pub value_pairs_args: TokenStream2,

    // Per-call re-entrancy guard (no-op for `&self` shapes).
    pub reentry_guard: TokenStream2,

    // Templates for converting K/V back to V8 (shared across modes).
    pub key_src: Ident,
    pub key_out: Ident,
    pub key_to_v8: TokenStream2,
    pub val_src: Ident,
    pub val_out: Ident,
    pub val_to_v8: TokenStream2,

    // Brand-check + recovery preamble fragments (delegated to
    // `v8_class::shared::recover_box`).
    pub factory_brand_check: TokenStream2,
    pub factory_external_recovery: TokenStream2,
    pub for_each_brand_check: TokenStream2,
    pub for_each_external_recovery: TokenStream2,
    pub next_external_recovery: TokenStream2,
    /// Brand check for `<Class>Iterator.prototype.next()`. Walks the
    /// receiver's prototype chain looking for the cached
    /// `<Class>Iterator.prototype` and throws TypeError on miss with
    /// the shape `"<Class>Iterator.prototype.next called on
    /// incompatible receiver"`. Closes Wave 10 NS6.
    pub next_brand_check: TokenStream2,

    // Pre-rendered must_str token bindings for every literal V8 string
    // the iterable codegen emits. Centralised in Wave 9 NS5 / H10.
    pub class_name_init: TokenStream2,
    pub to_string_tag_init: TokenStream2,
    pub next_key_init: TokenStream2,
    pub proto_key_init: TokenStream2,
    pub value_key_init: TokenStream2,
    pub done_key_init: TokenStream2,
    pub keys_key_init: TokenStream2,
    pub values_key_init: TokenStream2,
    pub entries_key_init: TokenStream2,
    pub foreach_key_init: TokenStream2,
    pub alloc_fail_msg_init: TokenStream2,
    pub foreach_not_callable_init: TokenStream2,
    pub iter_proto_walk_js_init: TokenStream2,
    pub illegal_ctor_msg_init: TokenStream2,
}

/// Generate the iterable surface for a class.
///
/// Emits:
///   - 4 free callback functions (`__<Class>_iter_factory_<kind>` for
///     keys/values/entries) and `__<Class>_for_each` and `__<Class>_iter_next`.
///   - The `<Class>Iterator` struct + its install_template helper.
///   - A `<Class>::install_iterable_methods(scope, proto)` method that
///     the existing install() codegen calls automatically.
///
/// The shape of the iterator's internal state, the `next()` callback,
/// and `forEach` depend on `attr.mode`:
///
///   - **Snapshot** (default): factory clones `value_pairs()` once and
///     stashes the `Vec<(K, V)>` in the iterator. `next()` indexes
///     into the vec directly. Mutations to the parent are NOT visible.
///
///   - **Live** (per WebIDL §3.7.10.2): factory stashes a
///     `Global<Object>` reference to the parent. `next()` re-localises
///     the parent, recovers `&Self`, calls `value_pairs()` afresh, and
///     indexes at the cursor. Mutations between `next()` calls ARE
///     observable. `forEach` likewise re-reads `value_pairs()` between
///     callbacks. If the parent state shrinks below the cursor,
///     `next()` yields `done`. If it grows, the cursor walks the new
///     entries — that's the spec behaviour.
///
/// `class_ty` is the parent class's marker ident (e.g. `Headers`, or
/// `Bag` under `#[v8_state_marker(Bag)] impl BagState`). Used for
/// JS-visible naming: install slot, brand-check fn, factory fn names,
/// iterator companion class name, install impl block.
/// `state_ty` is the type stored in V8 internal field 0 as
/// `Box<StateTy>` — the same type the impl block's `value_pairs`
/// receiver desugars against. Without `#[v8_state_marker]` this equals
/// `class_ty`; with it, it's the impl-block receiver (e.g. `BagState`).
/// Used for the `*mut`/`*const` casts on parent-state recovery and the
/// borrow type that calls `value_pairs`.
/// `attr` is the parsed `#[v8_iterable(key=..., value=..., mode=...)]`.
/// `sig` is the user's `value_pairs` receiver/arg shape, sniffed from
/// the impl block by `inspect_value_pairs`. It picks between
/// `*const Self`/`*mut Self` recovery and decides whether to pass the
/// outer `scope` through.
pub(crate) fn generate(
    class_ty: &Ident,
    state_ty: &Ident,
    attr: &IterableAttr,
    sig: ValuePairsSig,
) -> Result<TokenStream2, syn::Error> {
    let ctx = build_ctx(class_ty, state_ty, attr, sig)?;

    // Compose the four emitted sections in document order. Each
    // helper returns a self-contained TokenStream; the orchestrator
    // splices them at the end.
    let companion = emit_factory::gen_iterator_companion(&ctx);
    let ctor_throws = emit_factory::gen_constructor_throws(&ctx);
    let factories = emit_factory::gen_factory_callbacks(&ctx);
    let for_each = emit_iterator::gen_for_each_callback(&ctx);
    let next_cb = emit_iterator::gen_next_callback(&ctx);
    let install_bridge = emit_factory::gen_install_bridge(&ctx);

    Ok(quote! {
        #companion
        #ctor_throws
        #factories
        #for_each
        #next_cb
        #install_bridge
    })
}

/// Build the [`EmitCtx`] shared by the per-section emit helpers. All
/// of the original `generate` function's local-binding setup lives
/// here; the helpers in `emit_factory.rs` and `emit_iterator.rs` read
/// only from `ctx`.
fn build_ctx<'a>(
    class_ty: &'a Ident,
    state_ty: &'a Ident,
    attr: &'a IterableAttr,
    sig: ValuePairsSig,
) -> Result<EmitCtx<'a>, syn::Error> {
    let key_ty = &attr.key_ty;
    let value_ty = &attr.value_ty;
    let live = matches!(attr.mode, IterMode::Live);
    let is_mut = sig.is_mut;
    let takes_scope = sig.takes_scope;

    // Receiver-flavoured pointer + borrow tokens. `&mut self` requires
    // `*mut Self` + `&mut *ptr`; `&self` uses `*const` + `&*`.
    let self_ptr_ty = if is_mut {
        quote! { *mut #state_ty }
    } else {
        quote! { *const #state_ty }
    };
    let self_borrow = if is_mut {
        quote! { &mut * }
    } else {
        quote! { &* }
    };
    let self_borrow_ty = if is_mut {
        quote! { &mut #state_ty }
    } else {
        quote! { & #state_ty }
    };
    let value_pairs_args = if takes_scope {
        quote! { (scope) }
    } else {
        quote! { () }
    };

    let scope_tok = quote! { scope };

    // Per-call re-entrancy guard for `&mut self` value_pairs. Wave 8
    // closes design §13 / C5/H13 with the multi-slot Cell shape;
    // matches `v8_class/emit/reentry_guard.rs`'s design. No-op for
    // `&self` shapes (helper returns an empty TokenStream).
    let reentry_guard = reentry::gen_iter_reentry_guard(class_ty, is_mut);

    // Sanity check K/V classify (V can opt out via `value_marshal`).
    value_marshal::require_classified(key_ty, "key")?;
    if attr.value_marshal.is_none() {
        value_marshal::require_classified(value_ty, "value")?;
    }

    let iter_class_ty = format_ident!("{}Iterator", class_ty);
    let iter_class_name_str = iter_class_ty.to_string();
    let iter_to_string_tag_str = format!("{} Iterator", class_ty);

    let factory_keys_ident = format_ident!("__{}_iter_factory_keys", class_ty);
    let factory_values_ident = format_ident!("__{}_iter_factory_values", class_ty);
    let factory_entries_ident = format_ident!("__{}_iter_factory_entries", class_ty);
    let for_each_ident = format_ident!("__{}_for_each", class_ty);
    let next_ident = format_ident!("__{}_iter_next", class_ty);

    let key_src = Ident::new("__k", proc_macro2::Span::call_site());
    let key_out = Ident::new("__k_v", proc_macro2::Span::call_site());
    let key_to_v8 = value_marshal::gen_to_v8(key_ty, &key_src, &key_out, None)?;

    let val_src = Ident::new("__v", proc_macro2::Span::call_site());
    let val_out = Ident::new("__v_v", proc_macro2::Span::call_site());
    let val_to_v8 =
        value_marshal::gen_to_v8(value_ty, &val_src, &val_out, attr.value_marshal.as_ref())?;

    let iter_install_slot_ty = format_ident!("__InstallSlot_{}", iter_class_ty);
    let iter_brand_slot_ty = format_ident!("__BrandSlot_{}", iter_class_ty);
    // Iterator-class brand-check helper. Walks `__this`'s prototype
    // chain looking for the cached `<Class>Iterator.prototype`. Closes
    // Wave 10 NS6: a caller could previously hand `<Class>Iterator
    // .prototype.next` a *different* `#[v8_class]` wrapper as `this`
    // (any wrapper has `internal_field(0) = External(Box<X>)`), causing
    // the recovery `__ext.value() as *mut <Class>Iterator` to
    // reinterpret a `Box<Other>` as `*mut <Class>Iterator` — UB.
    let iter_brand_check_fn = format_ident!("__brand_check_{}", iter_class_ty);

    // Parent class's brand-check ident (see Wave 9 N1 note in
    // class_config.rs — the iterable codegen runs BEFORE ClassConfig
    // is built, so we still recompute here; same emitted token).
    let parent_brand_check_fn = format_ident!("__brand_check_{}", class_ty);
    let factory_brand_check =
        crate::v8_class::shared::recover_box::gen_brand_check_throw(&parent_brand_check_fn);
    let factory_external_recovery =
        crate::v8_class::shared::recover_box::gen_recover_external();
    let for_each_brand_check =
        crate::v8_class::shared::recover_box::gen_brand_check_throw(&parent_brand_check_fn);
    let for_each_external_recovery =
        crate::v8_class::shared::recover_box::gen_recover_external();
    let next_external_recovery =
        crate::v8_class::shared::recover_box::gen_recover_external();

    // Iterator-class brand check for `next()` — distinct from the
    // generic "Illegal invocation" thrown by parent-class checks. The
    // message shape matches the receiver-mismatch wording the v8
    // built-in iterators use (e.g. `Map Iterator.prototype.next called
    // on incompatible receiver`).
    let iter_next_msg = format!(
        "{}.prototype.next called on incompatible receiver",
        iter_class_ty
    );
    let next_brand_check = quote! {
        let __this = args.this();
        if !#iter_brand_check_fn(scope, __this) {
            let __msg = v8::String::new(scope, #iter_next_msg).unwrap();
            let __exc = v8::Exception::type_error(scope, __msg);
            scope.throw_exception(__exc);
            return;
        }
    };

    // Pre-render must_str token bindings (Wave 9 NS5 / H10).
    let class_name_init = must_str(&scope_tok, &quote! { #iter_class_name_str });
    let to_string_tag_init = must_str(&scope_tok, &quote! { #iter_to_string_tag_str });
    let next_key_init = must_str(&scope_tok, &quote! { "next" });
    let proto_key_init = must_str(&scope_tok, &quote! { "prototype" });
    let value_key_init = must_str(&scope_tok, &quote! { "value" });
    let done_key_init = must_str(&scope_tok, &quote! { "done" });
    let keys_key_init = must_str(&scope_tok, &quote! { "keys" });
    let values_key_init = must_str(&scope_tok, &quote! { "values" });
    let entries_key_init = must_str(&scope_tok, &quote! { "entries" });
    let foreach_key_init = must_str(&scope_tok, &quote! { "forEach" });
    let alloc_fail_msg_init = must_str(&scope_tok, &quote! { "Failed to allocate iterator" });
    let foreach_not_callable_init = must_str(
        &scope_tok,
        &quote! { "forEach callback is not callable" },
    );
    let iter_proto_walk_js_init = must_str(
        &scope_tok,
        &quote! { "Object.getPrototypeOf(Object.getPrototypeOf([][Symbol.iterator]()))" },
    );
    let illegal_ctor_msg_init = must_str(
        &scope_tok,
        &quote! {
            ::std::concat!(
                "Illegal constructor: ",
                #iter_class_name_str,
                " can only be created via the parent's keys() / values() / entries()",
            )
        },
    );

    Ok(EmitCtx {
        class_ty,
        state_ty,
        key_ty,
        value_ty,
        value_marshal: attr.value_marshal.as_ref(),
        live,
        is_mut,
        takes_scope,
        iter_class_ty,
        iter_class_name_str,
        iter_to_string_tag_str,
        iter_install_slot_ty,
        iter_brand_slot_ty,
        iter_brand_check_fn,
        factory_keys_ident,
        factory_values_ident,
        factory_entries_ident,
        for_each_ident,
        next_ident,
        iter_kind_keys: 0,
        iter_kind_values: 1,
        iter_kind_entries: 2,
        self_ptr_ty,
        self_borrow,
        self_borrow_ty,
        value_pairs_args,
        reentry_guard,
        key_src,
        key_out,
        key_to_v8,
        val_src,
        val_out,
        val_to_v8,
        factory_brand_check,
        factory_external_recovery,
        for_each_brand_check,
        for_each_external_recovery,
        next_external_recovery,
        next_brand_check,
        class_name_init,
        to_string_tag_init,
        next_key_init,
        proto_key_init,
        value_key_init,
        done_key_init,
        keys_key_init,
        values_key_init,
        entries_key_init,
        foreach_key_init,
        alloc_fail_msg_init,
        foreach_not_callable_init,
        iter_proto_walk_js_init,
        illegal_ctor_msg_init,
    })
}
