//! RPC v2 ALS plumbing for the per-request `ctx`.
//!
//! See `docs/proposals/rpc.md` §3 (Ambient context). The per-request
//! `ctx` itself is now a native `RpcCtx` v8_class with lazy accessors —
//! see `crate::rpc::ctx_holder`. This file keeps the small surface that
//! pumps the holder into V8's `ContinuationPreservedEmbedderData` slot
//! for the duration of the user procedure, and exposes a private
//! `globalThis.__zeroshipGetRpcCtx()` so `@zeroship/server` helpers can
//! read it.
//!
//! ## Slot model — same primitive as `AsyncLocalStorage`
//!
//! V8 propagates the embedder-data slot across every async hop (await,
//! microtask, `.then`, native-Promise resolution). The map stored in
//! that slot is keyed by per-instance Symbols; the platform's RPC ctx
//! gets its OWN distinct Symbol (minted lazily per isolate) so user-
//! minted `new AsyncLocalStorage()` cannot collide with our key. See
//! `crate::node::async_hooks::als` for the canonical save/set/call/
//! restore pattern this file mirrors verbatim.
//!
//! ## L1 lazy install
//!
//! The eager strategy used to clone the per-isolate context Map and
//! install the platform symbol on EVERY dispatch — paying for the
//! Map clone + `Map::Set` + `set_continuation_preserved_embedder_data`
//! round-trip even on procedures that never read `ctx`. The current
//! design defers those steps until `__zeroshipGetRpcCtx()` is actually
//! called: we stash the ctx holder in a thread-local "pending" slot at
//! dispatch entry, snapshot the prior CPED so we can restore it, and
//! the intrinsic does the clone-and-set on first call. Procedures that
//! never call the intrinsic pay only one Global allocation (for the
//! restore snapshot) plus two `set_continuation_preserved_embedder_data`
//! ABI calls.

#![allow(unsafe_code)]

use std::cell::RefCell;

use crate::node::async_hooks::als::{clone_map, read_context_map};

// ---------------------------------------------------------------------------
// ALS slot wiring
// ---------------------------------------------------------------------------

/// Per-isolate slot caching the platform's ctx Symbol.
struct RpcCtxKeySlot {
    key: v8::Global<v8::Symbol>,
}

/// Per-isolate Symbol that keys the RPC ctx into the ALS map.
pub fn rpc_ctx_als_key<'s>(scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Symbol> {
    if let Some(slot) = scope.get_slot::<RpcCtxKeySlot>() {
        return v8::Local::new(scope, slot.key.clone());
    }
    let desc = v8::String::new(scope, "zs:RpcContext").unwrap();
    let sym = v8::Symbol::new(scope, Some(desc));
    let key_global = v8::Global::new(scope, sym);
    scope.set_slot(RpcCtxKeySlot { key: key_global });
    sym
}

// ---------------------------------------------------------------------------
// L1 lazy install — thread-local pending holder
// ---------------------------------------------------------------------------

thread_local! {
    /// Per-thread ctx holder waiting to be installed into the V8 CPED
    /// slot on first `__zeroshipGetRpcCtx()` call. `Some` for the
    /// duration of a dispatch frame, `None` outside one.
    ///
    /// Held as a `Global<Object>` so the holder stays rooted across
    /// the whole dispatch (in case the intrinsic is never called or
    /// is called multiple times after a user `als.run` rolled back the
    /// CPED slot — see [`get_rpc_ctx_callback`]).
    static PENDING_RPC_CTX: RefCell<Option<v8::Global<v8::Object>>> =
        const { RefCell::new(None) };
}

/// Run `body` with `ctx_object` available to `__zeroshipGetRpcCtx()`
/// via lazy install into V8's CPED slot. Replaces the eager
/// `with_rpc_context_in_als` — the Map clone + `Map::Set` + first
/// `set_continuation_preserved_embedder_data` only happen if user
/// code actually calls the intrinsic.
///
/// Restore order (matches the eager path's exit-path contract):
///   1. Body returns (JS exceptions from the user procedure are
///      caught by `call_rpc_inner`'s `tc_scope`, not bubbled here).
///   2. CPED slot is restored to the pre-dispatch value.
///   3. The thread-local `PENDING_RPC_CTX` is rolled back to its
///      prior value.
///
/// Step 2 must precede step 3 because the intrinsic checks CPED
/// first: clearing pending before CPED restore would leave a
/// transient window where a re-entrant call could see "no map, no
/// pending" and return undefined. We don't expect re-entrancy here,
/// but the ordering is cheap to keep right.
///
/// Panics inside `body` skip the restore. That's the same behaviour
/// the eager `with_rpc_context_in_als` had — a panic on a V8 thread
/// is already a fatal scenario; correctness inside V8 is gated by
/// `tc_scope`-style JS exception capture, which the caller owns.
pub fn with_rpc_context_lazy<'s, R>(
    scope: &mut v8::PinScope<'s, '_>,
    ctx_object: v8::Local<'s, v8::Object>,
    body: impl FnOnce(&mut v8::PinScope<'s, '_>) -> R,
) -> R {
    // Snapshot CPED so we can restore on every exit path. We
    // intentionally allocate a Global here (matches the eager path's
    // contract) — skipping it when prev is undefined is a separate
    // optimisation (C3 in `docs/perf/rpc-dispatch-followup-2026-05-07.md`).
    let prev_slot = scope.get_continuation_preserved_embedder_data();
    let prev_global = v8::Global::new(scope, prev_slot);

    // Promote the ctx holder to a Global and stash it in the
    // thread-local pending slot. Save any prior pending value (defensive
    // — nested dispatches aren't a thing today, but rolling back is
    // cheap).
    let holder_global = v8::Global::new(scope, ctx_object);
    let prev_pending = PENDING_RPC_CTX.with(|cell| cell.replace(Some(holder_global)));

    let result = body(scope);

    // Restore CPED first so the next intrinsic call (if any) sees the
    // pre-dispatch slot, then clear/restore the pending holder.
    let prev_local = v8::Local::new(scope, &prev_global);
    scope.set_continuation_preserved_embedder_data(prev_local);
    PENDING_RPC_CTX.with(|cell| {
        *cell.borrow_mut() = prev_pending;
    });

    result
}

// ---------------------------------------------------------------------------
// globalThis.__zeroshipGetRpcCtx
// ---------------------------------------------------------------------------

/// Read the platform ctx holder out of V8's CPED slot, lazily
/// installing it on first call.
///
/// Three cases:
///
/// 1. **Cached path (post-install).** The CPED slot already holds a
///    Map containing our key — return the cached holder. This is the
///    hot path for any procedure that calls the intrinsic more than
///    once, and for the post-`await` resumption (V8 propagates CPED
///    into the continuation automatically).
///
/// 2. **Lazy install (first call).** No platform key in CPED yet, but
///    the dispatch frame's `PENDING_RPC_CTX` is set. Clone the current
///    map (or allocate a fresh one), insert our key → holder, install.
///    Critically, we DO NOT clear `PENDING_RPC_CTX` here: a user
///    `als.run(...)` can synchronously roll back CPED to a state with
///    no platform key, and a subsequent intrinsic call needs to
///    re-install. The guard at the dispatch frame end is the unique
///    owner of that lifetime.
///
/// 3. **No dispatch (module-init / outside RPC).** No platform key,
///    no pending — return undefined. This is what the
///    `ctx_undefined_at_module_init` test relies on.
fn get_rpc_ctx_callback(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let key = rpc_ctx_als_key(scope);

    // Case 1 — cached path (Map already has our key).
    if let Some(map) = read_context_map(scope) {
        if let Some(v) = map.get(scope, key.into()) {
            if !v.is_undefined() {
                rv.set(v);
                return;
            }
        }
    }

    // Case 2 — lazy install. Clone the holder Global out of the
    // thread-local; the slot stays populated for the rest of the
    // dispatch so user-ALS rollback can re-install on subsequent calls.
    let holder_global = match PENDING_RPC_CTX.with(|cell| cell.borrow().clone()) {
        Some(g) => g,
        None => {
            // Case 3 — no dispatch frame, return undefined.
            rv.set(v8::undefined(scope).into());
            return;
        }
    };
    let holder_local = v8::Local::new(scope, &holder_global);

    let next_map = match read_context_map(scope) {
        Some(m) => clone_map(scope, m),
        None => v8::Map::new(scope),
    };
    next_map.set(scope, key.into(), holder_local.into());
    scope.set_continuation_preserved_embedder_data(next_map.into());

    rv.set(holder_local.into());
}

/// Wire `__zeroshipGetRpcCtx` onto `globalThis`. Called from
/// `setup_globals`. Also installs the `RpcCtx` v8_class template in
/// the isolate so `mint_rpc_ctx` can look up the cached template.
pub fn install_globals<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    global: v8::Local<v8::Object>,
) {
    let f = v8::Function::new(scope, get_rpc_ctx_callback).unwrap();
    let key = v8::String::new(scope, "__zeroshipGetRpcCtx").unwrap();
    global.set(scope, key.into(), f.into());

    // Pre-install the RpcCtx template (idempotent per isolate). The
    // class is NOT bound on `globalThis` — it's a private holder that
    // user code only reaches via the ctx accessors.
    let _ = crate::rpc::ctx_holder::RpcCtx::install(scope);
}
