//! RPC v2 phase 1 — Wave E: per-isolate AbortRegistry + eviction-time
//! abort fan-out.
//!
//! See `docs/proposals/rpc-v2.md` §3 ("Abort source plumbing"). When the
//! worker's LRU cache evicts an isolate, every in-flight procedure must
//! get its `ctx.signal` aborted so user code (a `setTimeout` await, a
//! pending `fetch`, an `addEventListener("abort", ...)` consumer) can
//! observe the cancellation rather than vanish silently when the isolate
//! gets disposed.
//!
//! ## Shape
//!
//! - The registry is a thread-local `HashMap<(app_id, request_id),
//!   v8::Global<v8::Object>>`. Every V8 isolate is pinned to one
//!   worker thread; a per-thread map needs no locking. The Global
//!   retains the JS `AbortController` across V8 turns (the per-request
//!   `ctx.signal` is the controller's signal field).
//! - `register_in_flight` returns an `AbortGuard` (RAII) — Drop
//!   removes the entry, so resolve / reject / panic / sync return all
//!   unregister automatically without scattering cleanup code through
//!   the dispatch path.
//! - `entered_for_eviction(scope, app_id)` walks the registry, calls
//!   `controller.abort()` on every entry whose `app_id` matches, then
//!   clears those entries. Failures are swallowed + logged: eviction
//!   must not be fallible.
//!
//! ## Phase-1 deferrals
//!
//! The proposal calls for a 30-second hard-drain timer + a `Disposed`
//! state on the isolate post-drain (§15 OQ-2). Phase 1 ships only the
//! abort fan-out — the drain timer + Disposed transition are scaffolded
//! but not enforced. The runtime currently has no graceful-async-
//! cancellation primitive, so the timer would degenerate to "remove the
//! isolate after 30s no matter what" with no observable difference from
//! the existing `cache.isolates.remove` call.
//!
//! Lifetime detail: phase 1 DOES thread the `AbortGuard` through the
//! pump's `PendingRequest` entry, so the registry retains in-flight
//! controllers across `await` boundaries. Eviction during the awaited
//! continuation correctly fires the controller. What's deferred is the
//! "wait 30 seconds for in-flight procedures to drain before disposing
//! the isolate" timer — phase 1 disposes immediately after firing the
//! abort fan-out.

use std::cell::RefCell;
use std::collections::HashMap;

use uuid::Uuid;

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/// Per-isolate registry of in-flight `AbortController`s, keyed by
/// `(app_id, request_id)`. Lives in a thread-local because every V8
/// isolate is pinned to one worker thread; a per-thread map is the
/// natural shape (no locking, no Send/Sync requirement on the Global).
///
/// The HashMap value is the JS `AbortController` itself (NOT just the
/// signal) — `entered_for_eviction` calls `.abort()` on the controller,
/// which runs the spec's signal-abort algorithm on the owned signal.
thread_local! {
    static REGISTRY: RefCell<HashMap<RegistryKey, v8::Global<v8::Object>>> =
        RefCell::new(HashMap::new());
}

#[derive(Hash, Eq, PartialEq, Clone, Debug)]
struct RegistryKey {
    app_id: Uuid,
    request_id: u64,
}

// ---------------------------------------------------------------------------
// AbortGuard — RAII unregister
// ---------------------------------------------------------------------------

/// RAII handle returned by [`register_in_flight`]. Dropping it removes
/// the entry from the registry — covers every procedure exit path
/// (resolve, reject, panic, sync return) without scattering cleanup
/// code through the dispatch logic.
///
/// `entered_for_eviction` clears entries directly on the same map, so a
/// guard whose entry was already evicted is a no-op on Drop.
pub struct AbortGuard {
    key: RegistryKey,
}

impl Drop for AbortGuard {
    fn drop(&mut self) {
        REGISTRY.with(|r| {
            r.borrow_mut().remove(&self.key);
        });
    }
}

// ---------------------------------------------------------------------------
// register_in_flight
// ---------------------------------------------------------------------------

/// Register the per-request `AbortController` for the lifetime of a
/// procedure. Returns an `AbortGuard` whose Drop unregisters the entry.
///
/// `controller` is the JS `AbortController` minted by Wave D's
/// `RpcContext::build_js_object`. The registry retains a `Global<Object>`
/// (cloned from the supplied Local on `scope`) so the controller stays
/// alive across V8 turns until either the guard drops or
/// [`entered_for_eviction`] clears the entry.
pub fn register_in_flight(
    scope: &mut v8::PinScope,
    app_id: Uuid,
    request_id: u64,
    controller: v8::Local<v8::Object>,
) -> AbortGuard {
    let key = RegistryKey { app_id, request_id };
    let global = v8::Global::new(scope, controller);
    REGISTRY.with(|r| {
        r.borrow_mut().insert(key.clone(), global);
    });
    AbortGuard { key }
}

// ---------------------------------------------------------------------------
// entered_for_eviction
// ---------------------------------------------------------------------------

/// Fire `.abort()` on every in-flight `AbortController` for `app_id`.
/// Called by the worker's LRU eviction path BEFORE removing the
/// isolate from the cache (see `crates/worker/src/cache.rs::evict_lru`).
///
/// Each registered Global is upgraded to a Local on `scope`, the
/// `abort` property is looked up + called with no arguments. Failures
/// (the controller is GC'd, the abort method threw, etc.) are swallowed
/// and logged via `tracing::warn!` — eviction can't fail.
///
/// The registry entries for `app_id` are cleared after the walk; any
/// post-drain dispatch attempts on the (about-to-be-disposed) isolate
/// would reinsert via [`register_in_flight`], but the worker removes
/// the isolate immediately after this returns so that path is
/// unreachable in practice.
pub fn entered_for_eviction(scope: &mut v8::PinScope, app_id: Uuid) {
    // Snapshot keys + globals matching `app_id`. Done in a single
    // borrow + clear pass so the registry is empty BEFORE we re-enter
    // user code via `controller.abort()` — even if a hypothetical abort
    // listener tried to register a new procedure synchronously, it'd
    // see a fresh map.
    let to_abort: Vec<(RegistryKey, v8::Global<v8::Object>)> = REGISTRY.with(|r| {
        let mut map = r.borrow_mut();
        let matching: Vec<_> = map
            .keys()
            .filter(|k| k.app_id == app_id)
            .cloned()
            .collect();
        matching
            .into_iter()
            .map(|k| {
                // SAFETY: keys was just collected from the same map.
                let g = map.remove(&k).unwrap();
                (k, g)
            })
            .collect()
    });

    if to_abort.is_empty() {
        return;
    }

    let abort_key = v8::String::new(scope, "abort").unwrap();

    for (key, controller_global) in to_abort {
        let controller = v8::Local::new(scope, &controller_global);

        // Look up `controller.abort` on the wrapper. Wave D's frozen
        // ctx layout puts the controller behind ctx.signal — but we
        // stored the controller itself, not the signal, so the
        // method dispatch goes through the `#[v8_method]`-emitted
        // accessor on AbortController.prototype.
        let abort_fn_v = match controller.get(scope, abort_key.into()) {
            Some(v) => v,
            None => {
                tracing::warn!(
                    app_id = %key.app_id,
                    request_id = key.request_id,
                    "rpc abort: controller.abort getter returned None"
                );
                continue;
            }
        };
        let abort_fn = match v8::Local::<v8::Function>::try_from(abort_fn_v) {
            Ok(f) => f,
            Err(_) => {
                tracing::warn!(
                    app_id = %key.app_id,
                    request_id = key.request_id,
                    "rpc abort: controller.abort is not a function"
                );
                continue;
            }
        };

        // tc_scope swallows synchronous throws — eviction proceeds even
        // if a user-installed `abort` listener throws.
        v8::tc_scope!(let tc, scope);
        let _ = abort_fn.call(tc, controller.into(), &[]);
        if tc.has_caught() {
            let exc = tc.exception();
            let msg = exc
                .map(|e| e.to_rust_string_lossy(tc))
                .unwrap_or_else(|| "<no exception>".to_string());
            tracing::warn!(
                app_id = %key.app_id,
                request_id = key.request_id,
                error = %msg,
                "rpc abort: controller.abort threw during eviction"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

/// Test-only: count registry entries currently held for `app_id`.
/// Cheap full-map scan — only called from unit tests.
#[doc(hidden)]
pub fn entries_for_app(app_id: Uuid) -> usize {
    REGISTRY.with(|r| r.borrow().keys().filter(|k| k.app_id == app_id).count())
}
