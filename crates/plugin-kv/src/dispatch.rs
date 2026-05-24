//! Async dispatch for `env.kv.*`.
//!
//! Each `Kv` v8_class method validates its arguments synchronously,
//! then calls one of the `dispatch_*` helpers here. Every helper
//! follows the same shape, mirroring `plugin-db`'s `crud::dispatch_*`:
//!
//! 1. Build a `PromiseResolver` + capture the `request_id` off the
//!    runtime state slot ([`setup_kv_promise`]).
//! 2. Push a `spawned_ops` future that runs the backend call and packs
//!    the result into an `OpResult::JsValue` carrying a typed
//!    [`ResolveValue`].
//! 3. Return the `Promise` immediately.
//!
//! Errors resolve via the typed `OpError` channel
//! ([`ResolveValue::RejectError`]) so `KvError`'s `.code`
//! (`kv_non_numeric`, `kv_overflow`, …) reaches JS — the SDK branches
//! on `err.code` instead of substring-matching.

use std::sync::Arc;

use serde_json::json;
use zeroship_runtime::state::{OpResult, ResolveValue, SharedState};

use crate::backend::{Backend, TtlState};

/// `Number.MAX_SAFE_INTEGER` — the largest integer an `f64` represents
/// exactly. `incr` results inside `±2^53` resolve as a JS `number`;
/// beyond it, as a `BigInt` to avoid silent precision loss.
const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;

/// Build the promise + resolver and capture the owning request id.
/// Returns `(resolver_global, request_id, promise)`.
fn setup_kv_promise<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    state: &SharedState,
) -> (
    v8::Global<v8::PromiseResolver>,
    Option<u64>,
    v8::Local<'s, v8::Promise>,
) {
    let resolver = v8::PromiseResolver::new(scope).unwrap();
    let promise = resolver.get_promise(scope);
    let global_resolver = v8::Global::new(scope, resolver);
    let request_id = state.borrow().executing_request_id;
    (global_resolver, request_id, promise)
}

/// Get the runtime state slot off the isolate.
fn runtime_state(scope: &mut v8::PinScope<'_, '_>) -> SharedState {
    scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone()
}

/// Resolve an `incr` result as a JS `number` when it fits exactly in an
/// `f64`, else as a `BigInt`.
#[allow(clippy::cast_precision_loss)]
fn incr_resolve(n: i64) -> ResolveValue {
    if n.abs() <= MAX_SAFE_INTEGER {
        ResolveValue::F64(n as f64)
    } else {
        ResolveValue::BigInt(n)
    }
}

/// Macro: spawn a backend op future, mapping `Ok(v) -> resolve(v)` and
/// `Err(KvError) -> RejectError`. Keeps each `dispatch_*` helper a
/// two-liner without repeating the `OpResult::JsValue` boilerplate.
macro_rules! spawn_kv_op {
    ($scope:expr, $backend:expr, |$b:ident| $call:expr, $resolve:expr) => {{
        let state = runtime_state($scope);
        let (resolver, request_id, promise) = setup_kv_promise($scope, &state);
        let $b = $backend;
        state.borrow_mut().spawned_ops.push(Box::pin(async move {
            let value = match $call.await {
                Ok(v) => ($resolve)(v),
                Err(e) => ResolveValue::RejectError(e.to_op_error()),
            };
            OpResult::JsValue { resolver, value, request_id }
        }));
        promise
    }};
}

/// `kv.get(key)` → resolves `string | null`.
pub(crate) fn dispatch_get<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    backend: Arc<dyn Backend>,
    app_id: String,
    key: String,
) -> v8::Local<'s, v8::Promise> {
    spawn_kv_op!(scope, backend, |b| b.get(&app_id, &key), |v: Option<String>| {
        match v {
            Some(s) => ResolveValue::String(s),
            None => ResolveValue::Json("null".to_string()),
        }
    })
}

/// `kv.set(key, value, {ttlMs?})` → resolves `{ ok: true }`.
pub(crate) fn dispatch_set<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    backend: Arc<dyn Backend>,
    app_id: String,
    key: String,
    value: String,
    ttl_ms: Option<u64>,
) -> v8::Local<'s, v8::Promise> {
    spawn_kv_op!(
        scope,
        backend,
        |b| b.set(&app_id, &key, &value, ttl_ms),
        |_: ()| ResolveValue::Json(json!({ "ok": true }).to_string())
    )
}

/// `kv.delete(key)` → resolves `{ deleted: boolean }`.
pub(crate) fn dispatch_delete<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    backend: Arc<dyn Backend>,
    app_id: String,
    key: String,
) -> v8::Local<'s, v8::Promise> {
    spawn_kv_op!(scope, backend, |b| b.delete(&app_id, &key), |deleted: bool| {
        ResolveValue::Json(json!({ "deleted": deleted }).to_string())
    })
}

/// `kv.incr(key, {by?, ttlMs?})` → resolves `number` (BigInt if large).
pub(crate) fn dispatch_incr<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    backend: Arc<dyn Backend>,
    app_id: String,
    key: String,
    delta: i64,
    ttl_ms: Option<u64>,
) -> v8::Local<'s, v8::Promise> {
    spawn_kv_op!(
        scope,
        backend,
        |b| b.incr(&app_id, &key, delta, ttl_ms),
        incr_resolve
    )
}

/// `kv.setIfAbsent(key, value, {ttlMs?})` → resolves `{ stored: boolean }`.
pub(crate) fn dispatch_set_if_absent<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    backend: Arc<dyn Backend>,
    app_id: String,
    key: String,
    value: String,
    ttl_ms: Option<u64>,
) -> v8::Local<'s, v8::Promise> {
    spawn_kv_op!(
        scope,
        backend,
        |b| b.set_if_absent(&app_id, &key, &value, ttl_ms),
        |stored: bool| ResolveValue::Json(json!({ "stored": stored }).to_string())
    )
}

/// `kv.expire(key, ttlMs)` → resolves `{ updated: boolean }`.
pub(crate) fn dispatch_expire<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    backend: Arc<dyn Backend>,
    app_id: String,
    key: String,
    ttl_ms: u64,
) -> v8::Local<'s, v8::Promise> {
    spawn_kv_op!(
        scope,
        backend,
        |b| b.expire(&app_id, &key, ttl_ms),
        |updated: bool| ResolveValue::Json(json!({ "updated": updated }).to_string())
    )
}

/// `kv.ttl(key)` → resolves `{ ttlMs: number | null }` for an existing
/// key, or `null` for a missing key.
///
/// - missing key      → `null`
/// - exists, no expiry → `{ ttlMs: null }`
/// - exists, expiring  → `{ ttlMs: <ms> }`
pub(crate) fn dispatch_ttl<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    backend: Arc<dyn Backend>,
    app_id: String,
    key: String,
) -> v8::Local<'s, v8::Promise> {
    spawn_kv_op!(scope, backend, |b| b.ttl(&app_id, &key), |state: TtlState| {
        let v = match state {
            TtlState::Missing => serde_json::Value::Null,
            TtlState::NoExpiry => json!({ "ttlMs": serde_json::Value::Null }),
            TtlState::ExpiresInMs(ms) => json!({ "ttlMs": ms }),
        };
        ResolveValue::Json(v.to_string())
    })
}

/// `kv.persist(key)` → resolves `{ updated: boolean }`.
pub(crate) fn dispatch_persist<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    backend: Arc<dyn Backend>,
    app_id: String,
    key: String,
) -> v8::Local<'s, v8::Promise> {
    spawn_kv_op!(
        scope,
        backend,
        |b| b.persist(&app_id, &key),
        |updated: bool| ResolveValue::Json(json!({ "updated": updated }).to_string())
    )
}

/// `kv.list(prefix?, {cursor?, limit?})` → resolves
/// `{ keys: string[], cursor: string | null }`.
pub(crate) fn dispatch_list<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    backend: Arc<dyn Backend>,
    app_id: String,
    prefix: String,
    cursor: Option<String>,
    limit: usize,
) -> v8::Local<'s, v8::Promise> {
    spawn_kv_op!(
        scope,
        backend,
        |b| b.list(&app_id, &prefix, cursor.as_deref(), limit),
        |(keys, next): (Vec<String>, Option<String>)| {
            let next_v = match next {
                Some(c) => serde_json::Value::String(c),
                None => serde_json::Value::Null,
            };
            ResolveValue::Json(json!({ "keys": keys, "cursor": next_v }).to_string())
        }
    )
}
