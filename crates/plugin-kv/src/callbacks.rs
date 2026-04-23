//! V8 callbacks for `zeroship.kv.*` methods. In-memory ops — no async
//! backend needed, but we still return Promises to match the rest of the
//! platform's "everything is async" contract and keep door open for
//! Redis backend later.

use serde_json::json;
use zeroship_runtime::state::{OpResult, SharedState};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn require_string(
    scope: &mut v8::PinScope<'_, '_>,
    args: &v8::FunctionCallbackArguments,
    index: i32,
    name: &str,
) -> Option<String> {
    if args.length() <= index { return throw(scope, name); }
    let val = args.get(index);
    if val.is_null_or_undefined() { return throw(scope, name); }
    let s = val.to_rust_string_lossy(scope);
    if s.is_empty() { return throw(scope, name); }
    Some(s)
}

fn optional_string(
    scope: &mut v8::PinScope<'_, '_>,
    args: &v8::FunctionCallbackArguments,
    index: i32,
) -> Option<String> {
    if args.length() <= index { return None; }
    let val = args.get(index);
    if val.is_null_or_undefined() { return None; }
    let s = val.to_rust_string_lossy(scope);
    if s.is_empty() { None } else { Some(s) }
}

fn optional_u64(
    scope: &mut v8::PinScope<'_, '_>,
    args: &v8::FunctionCallbackArguments,
    index: i32,
) -> Option<u64> {
    if args.length() <= index { return None; }
    let val = args.get(index);
    if val.is_null_or_undefined() { return None; }
    val.integer_value(scope).map(|i| i.max(0) as u64)
}

fn optional_i64(
    scope: &mut v8::PinScope<'_, '_>,
    args: &v8::FunctionCallbackArguments,
    index: i32,
) -> Option<i64> {
    if args.length() <= index { return None; }
    let val = args.get(index);
    if val.is_null_or_undefined() { return None; }
    val.integer_value(scope)
}

fn throw(scope: &mut v8::PinScope, name: &str) -> Option<String> {
    let msg = v8::String::new(scope, &format!("kv: missing required argument '{name}'")).unwrap();
    let exc = v8::Exception::type_error(scope, msg);
    scope.throw_exception(exc);
    None
}

fn get_app_id(state: &SharedState) -> String {
    state.borrow().env_vars.get("APP_ID").cloned().unwrap_or_else(|| "default".to_string())
}

fn setup_promise<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    state: &SharedState,
) -> (u32, Option<u64>, v8::Local<'s, v8::Promise>) {
    let resolver = v8::PromiseResolver::new(scope).unwrap();
    let promise = resolver.get_promise(scope);
    let global_resolver = v8::Global::new(scope, resolver);

    let mut s = state.borrow_mut();
    let op_id = s.next_op_id;
    s.next_op_id += 1;
    s.pending_resolvers.insert(op_id, global_resolver);
    let request_id = s.executing_request_id;
    (op_id, request_id, promise)
}

// ---------------------------------------------------------------------------
// get(key)
// ---------------------------------------------------------------------------

pub fn get(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope.get_slot::<SharedState>().expect("state").clone();
    let Some(key) = require_string(scope, &args, 0, "key") else { return };
    let app_id = get_app_id(&state);
    let scoped = crate::scoped_key(&app_id, &key);

    // Sync in-memory op — still resolve via promise for API consistency.
    let (op_id, request_id, promise) = setup_promise(scope, &state);
    let value = crate::store_get(&scoped);
    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        let json = match value {
            Some(v) => serde_json::Value::String(v).to_string(),
            None => "null".into(),
        };
        OpResult::Completed { op_id, value: json, request_id }
    }));
    rv.set(promise.into());
}

// ---------------------------------------------------------------------------
// set(key, value, ttlMs?)
// ---------------------------------------------------------------------------

pub fn set(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope.get_slot::<SharedState>().expect("state").clone();
    let Some(key) = require_string(scope, &args, 0, "key") else { return };
    let Some(value) = optional_string(scope, &args, 1).or_else(|| {
        // Allow empty-string values but not null/undefined.
        if args.length() > 1 && !args.get(1).is_null_or_undefined() {
            Some(args.get(1).to_rust_string_lossy(scope))
        } else {
            let msg = v8::String::new(scope, "kv: missing required argument 'value'").unwrap();
            let exc = v8::Exception::type_error(scope, msg);
            scope.throw_exception(exc);
            None
        }
    }) else { return };
    let ttl_ms = optional_u64(scope, &args, 2);

    let app_id = get_app_id(&state);
    let scoped = crate::scoped_key(&app_id, &key);
    let (op_id, request_id, promise) = setup_promise(scope, &state);

    crate::store_set(scoped, value, ttl_ms);
    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        OpResult::Completed {
            op_id,
            value: json!({ "ok": true }).to_string(),
            request_id,
        }
    }));
    rv.set(promise.into());
}

// ---------------------------------------------------------------------------
// delete(key)
// ---------------------------------------------------------------------------

pub fn delete(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope.get_slot::<SharedState>().expect("state").clone();
    let Some(key) = require_string(scope, &args, 0, "key") else { return };

    let app_id = get_app_id(&state);
    let scoped = crate::scoped_key(&app_id, &key);
    let (op_id, request_id, promise) = setup_promise(scope, &state);

    let deleted = crate::store_delete(&scoped);
    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        OpResult::Completed {
            op_id,
            value: json!({ "deleted": deleted }).to_string(),
            request_id,
        }
    }));
    rv.set(promise.into());
}

// ---------------------------------------------------------------------------
// incr(key, delta?)
// ---------------------------------------------------------------------------

pub fn incr(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope.get_slot::<SharedState>().expect("state").clone();
    let Some(key) = require_string(scope, &args, 0, "key") else { return };
    let delta = optional_i64(scope, &args, 1).unwrap_or(1);

    let app_id = get_app_id(&state);
    let scoped = crate::scoped_key(&app_id, &key);
    let (op_id, request_id, promise) = setup_promise(scope, &state);

    // Read-modify-write under the single-threaded V8 isolate — no race.
    let current = crate::store_get(&scoped)
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(0);
    let next = current.saturating_add(delta);
    crate::store_set(scoped, next.to_string(), None);

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        OpResult::Completed {
            op_id,
            value: next.to_string(),
            request_id,
        }
    }));
    rv.set(promise.into());
}

// ---------------------------------------------------------------------------
// list(prefix?)
// ---------------------------------------------------------------------------

pub fn list(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope.get_slot::<SharedState>().expect("state").clone();
    let prefix = optional_string(scope, &args, 0).unwrap_or_default();
    let app_id = get_app_id(&state);
    let scoped_prefix = crate::scoped_key(&app_id, &prefix);
    let (op_id, request_id, promise) = setup_promise(scope, &state);

    let keys = crate::store_list_prefix(&scoped_prefix);
    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        OpResult::Completed {
            op_id,
            value: serde_json::Value::Array(
                keys.into_iter().map(serde_json::Value::String).collect()
            ).to_string(),
            request_id,
        }
    }));
    rv.set(promise.into());
}
