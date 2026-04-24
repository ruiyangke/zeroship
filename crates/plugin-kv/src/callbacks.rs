//! V8 callbacks for `zeroship.kv.*`.
//!
//! Each callback: parse args, allocate a promise, push an async op into
//! the runtime pump, return the promise. The pump resolves via OpResult.

use std::sync::Arc;

use serde_json::json;
use zeroship_runtime::state::{OpResult, SharedState};

use crate::{Backend, KV_BACKEND};

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

fn current_backend() -> Result<Arc<dyn Backend>, String> {
    KV_BACKEND.with(|c| c.borrow().as_ref().map(Arc::clone))
        .ok_or_else(|| "kv: not configured — KvPlugin not registered".to_string())
}

// ---------------------------------------------------------------------------
// Callback: get(key)
// ---------------------------------------------------------------------------

pub fn get(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope.get_slot::<SharedState>().expect("state").clone();
    let Some(key) = require_string(scope, &args, 0, "key") else { return };
    let app_id = get_app_id(&state);
    let (op_id, request_id, promise) = setup_promise(scope, &state);

    let backend = match current_backend() {
        Ok(b) => b,
        Err(e) => {
            state.borrow_mut().spawned_ops.push(Box::pin(async move {
                OpResult::Failed { op_id, error: e, request_id }
            }));
            rv.set(promise.into());
            return;
        }
    };

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        match backend.get(&app_id, &key).await {
            Ok(Some(v)) => OpResult::Completed {
                op_id,
                value: serde_json::Value::String(v).to_string(),
                request_id,
            },
            Ok(None) => OpResult::Completed {
                op_id,
                value: "null".into(),
                request_id,
            },
            Err(e) => OpResult::Failed { op_id, error: e, request_id },
        }
    }));
    rv.set(promise.into());
}

// ---------------------------------------------------------------------------
// Callback: set(key, value, ttlMs?)
// ---------------------------------------------------------------------------

pub fn set(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope.get_slot::<SharedState>().expect("state").clone();
    let Some(key) = require_string(scope, &args, 0, "key") else { return };
    // Accept empty-string values; require at least "defined" non-null for pos 1.
    let Some(value) = ({
        if args.length() <= 1 || args.get(1).is_null_or_undefined() {
            let msg = v8::String::new(scope, "kv: missing required argument 'value'").unwrap();
            let exc = v8::Exception::type_error(scope, msg);
            scope.throw_exception(exc);
            None
        } else {
            Some(args.get(1).to_rust_string_lossy(scope))
        }
    }) else { return };
    let ttl_ms = optional_u64(scope, &args, 2);

    let app_id = get_app_id(&state);
    let (op_id, request_id, promise) = setup_promise(scope, &state);

    let backend = match current_backend() {
        Ok(b) => b,
        Err(e) => {
            state.borrow_mut().spawned_ops.push(Box::pin(async move {
                OpResult::Failed { op_id, error: e, request_id }
            }));
            rv.set(promise.into());
            return;
        }
    };

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        match backend.set(&app_id, &key, &value, ttl_ms).await {
            Ok(()) => OpResult::Completed {
                op_id,
                value: json!({ "ok": true }).to_string(),
                request_id,
            },
            Err(e) => OpResult::Failed { op_id, error: e, request_id },
        }
    }));
    rv.set(promise.into());
}

// ---------------------------------------------------------------------------
// Callback: delete(key)
// ---------------------------------------------------------------------------

pub fn delete(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope.get_slot::<SharedState>().expect("state").clone();
    let Some(key) = require_string(scope, &args, 0, "key") else { return };
    let app_id = get_app_id(&state);
    let (op_id, request_id, promise) = setup_promise(scope, &state);

    let backend = match current_backend() {
        Ok(b) => b,
        Err(e) => {
            state.borrow_mut().spawned_ops.push(Box::pin(async move {
                OpResult::Failed { op_id, error: e, request_id }
            }));
            rv.set(promise.into());
            return;
        }
    };

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        match backend.delete(&app_id, &key).await {
            Ok(deleted) => OpResult::Completed {
                op_id,
                value: json!({ "deleted": deleted }).to_string(),
                request_id,
            },
            Err(e) => OpResult::Failed { op_id, error: e, request_id },
        }
    }));
    rv.set(promise.into());
}

// ---------------------------------------------------------------------------
// Callback: incr(key, delta?)
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
    let (op_id, request_id, promise) = setup_promise(scope, &state);

    let backend = match current_backend() {
        Ok(b) => b,
        Err(e) => {
            state.borrow_mut().spawned_ops.push(Box::pin(async move {
                OpResult::Failed { op_id, error: e, request_id }
            }));
            rv.set(promise.into());
            return;
        }
    };

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        match backend.incr(&app_id, &key, delta).await {
            Ok(n) => OpResult::Completed {
                op_id,
                value: n.to_string(),
                request_id,
            },
            Err(e) => OpResult::Failed { op_id, error: e, request_id },
        }
    }));
    rv.set(promise.into());
}

// ---------------------------------------------------------------------------
// Callback: list(prefix?)
// ---------------------------------------------------------------------------

pub fn list(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope.get_slot::<SharedState>().expect("state").clone();
    let prefix = optional_string(scope, &args, 0).unwrap_or_default();
    let app_id = get_app_id(&state);
    let (op_id, request_id, promise) = setup_promise(scope, &state);

    let backend = match current_backend() {
        Ok(b) => b,
        Err(e) => {
            state.borrow_mut().spawned_ops.push(Box::pin(async move {
                OpResult::Failed { op_id, error: e, request_id }
            }));
            rv.set(promise.into());
            return;
        }
    };

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        match backend.list(&app_id, &prefix).await {
            Ok(keys) => OpResult::Completed {
                op_id,
                value: serde_json::Value::Array(
                    keys.into_iter().map(serde_json::Value::String).collect()
                ).to_string(),
                request_id,
            },
            Err(e) => OpResult::Failed { op_id, error: e, request_id },
        }
    }));
    rv.set(promise.into());
}
