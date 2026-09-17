//! RPC v2 ALS plumbing for the per-request `ctx`.
//!
//! The per-request `ctx` itself is a native `RpcCtx` v8_class with lazy
//! accessors — see `crate::rpc::ctx_holder`. This file keeps the small
//! surface that pumps the holder into V8's `ContinuationPreservedEmbedderData` slot
//! for the duration of the user procedure. The native `zeroship` module reads
//! the slot directly; creator code has no global callback for this state.
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
//! The platform frame is installed before invoking user code. This is
//! required even when a procedure does not read `ctx` synchronously: its
//! first request-scoped read may happen after an `await`, and V8 can only
//! preserve a frame that existed when the continuation was created. The
//! expensive `RpcCtx` fields themselves remain lazily materialized.

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

/// Run `body` with `ctx_object` available through V8's CPED slot. The current
/// Map is cloned so user-created
/// `AsyncLocalStorage` entries survive and sibling continuations keep their
/// own immutable snapshots.
///
/// Panics inside `body` skip the restore. That's the same behaviour
/// the prior implementation had — a panic on a V8 thread is already a
/// fatal scenario; correctness inside V8 is gated by
/// `tc_scope`-style JS exception capture, which the caller owns.
pub fn with_rpc_context<'s, R>(
    scope: &mut v8::PinScope<'s, '_>,
    ctx_object: v8::Local<'s, v8::Object>,
    body: impl FnOnce(&mut v8::PinScope<'s, '_>) -> R,
) -> R {
    // Snapshot CPED so the exact caller frame can be restored on exit.
    let prev_slot = scope.get_continuation_preserved_embedder_data();
    let prev_global = v8::Global::new(scope, prev_slot);

    let next_map = match read_context_map(scope) {
        Some(map) => clone_map(scope, map),
        None => v8::Map::new(scope),
    };
    let key = rpc_ctx_als_key(scope);
    next_map.set(scope, key.into(), ctx_object.into());
    scope.set_continuation_preserved_embedder_data(next_map.into());

    let result = body(scope);

    let prev_local = v8::Local::new(scope, &prev_global);
    scope.set_continuation_preserved_embedder_data(prev_local);

    result
}

pub(crate) fn current_rpc_ctx_object<'s>(
    scope: &mut v8::PinScope<'s, '_>,
) -> Option<v8::Local<'s, v8::Object>> {
    let map = read_context_map(scope)?;
    let key = rpc_ctx_als_key(scope);
    let value = map.get(scope, key.into())?;
    if value.is_undefined() {
        return None;
    }
    v8::Local::<v8::Object>::try_from(value).ok()
}

mod procedure;
mod call;
pub(crate) mod response;
pub(crate) mod stream;
pub(crate) use call::{CallProgress, RpcCall};
pub(crate) use procedure::ProcedureRegistry;
