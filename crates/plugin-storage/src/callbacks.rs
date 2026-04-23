//! V8 callbacks for `zeroship.storage.*` methods.
//!
//! Each callback mirrors the plugin-db pattern: parse args, allocate a
//! promise, push an async op into the runtime pump's spawned-ops queue,
//! return the promise. The pump resolves/rejects via OpResult.

use base64::Engine;
use serde_json::json;
use zeroship_runtime::state::{OpResult, SharedState};

use crate::STORAGE_ROOT;

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn require_string_arg(
    scope: &mut v8::PinScope<'_, '_>,
    args: &v8::FunctionCallbackArguments,
    index: i32,
    name: &str,
) -> Option<String> {
    let val = if args.length() > index { args.get(index) } else { return throw_type(scope, name); };
    if val.is_null_or_undefined() { return throw_type(scope, name); }
    let s = val.to_rust_string_lossy(scope);
    if s.is_empty() { return throw_type(scope, name); }
    Some(s)
}

fn optional_string_arg(
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

fn throw_type(scope: &mut v8::PinScope, arg_name: &str) -> Option<String> {
    let msg = v8::String::new(scope, &format!("storage: missing required argument '{arg_name}'")).unwrap();
    let exc = v8::Exception::type_error(scope, msg);
    scope.throw_exception(exc);
    None
}

fn get_app_id(state: &SharedState) -> String {
    state
        .borrow()
        .env_vars
        .get("APP_ID")
        .cloned()
        .unwrap_or_else(|| "default".to_string())
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

fn current_root() -> Result<std::path::PathBuf, String> {
    STORAGE_ROOT.with(|c| c.borrow().clone())
        .ok_or_else(|| "storage: not configured — StoragePlugin::new() not registered".to_string())
}

// ---------------------------------------------------------------------------
// put(bucket, key, bytesBase64, contentType?)
// ---------------------------------------------------------------------------

pub fn put(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope.get_slot::<SharedState>().expect("state").clone();

    let Some(bucket) = require_string_arg(scope, &args, 0, "bucket") else { return };
    let Some(key) = require_string_arg(scope, &args, 1, "key") else { return };
    let Some(b64) = require_string_arg(scope, &args, 2, "bytesBase64") else { return };
    let content_type = optional_string_arg(scope, &args, 3);

    let app_id = get_app_id(&state);
    let (op_id, request_id, promise) = setup_promise(scope, &state);

    let bytes = match base64::engine::general_purpose::STANDARD.decode(b64.as_bytes()) {
        Ok(b) => b,
        Err(e) => {
            state.borrow_mut().spawned_ops.push(Box::pin(async move {
                OpResult::Failed {
                    op_id,
                    error: format!("storage: invalid base64: {e}"),
                    request_id,
                }
            }));
            rv.set(promise.into());
            return;
        }
    };

    let root = match current_root() {
        Ok(r) => r,
        Err(e) => {
            state.borrow_mut().spawned_ops.push(Box::pin(async move {
                OpResult::Failed { op_id, error: e, request_id }
            }));
            rv.set(promise.into());
            return;
        }
    };

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        match crate::backend::put(&root, &app_id, &bucket, &key, &bytes, content_type.as_deref()).await {
            Ok(size) => OpResult::Completed {
                op_id,
                value: json!({ "bucket": bucket, "key": key, "size": size }).to_string(),
                request_id,
            },
            Err(e) => OpResult::Failed { op_id, error: e, request_id },
        }
    }));

    rv.set(promise.into());
}

// ---------------------------------------------------------------------------
// get(bucket, key) → { bytesBase64, contentType, size } | null
// ---------------------------------------------------------------------------

pub fn get(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope.get_slot::<SharedState>().expect("state").clone();

    let Some(bucket) = require_string_arg(scope, &args, 0, "bucket") else { return };
    let Some(key) = require_string_arg(scope, &args, 1, "key") else { return };

    let app_id = get_app_id(&state);
    let (op_id, request_id, promise) = setup_promise(scope, &state);

    let root = match current_root() {
        Ok(r) => r,
        Err(e) => {
            state.borrow_mut().spawned_ops.push(Box::pin(async move {
                OpResult::Failed { op_id, error: e, request_id }
            }));
            rv.set(promise.into());
            return;
        }
    };

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        match crate::backend::get(&root, &app_id, &bucket, &key).await {
            Ok(None) => OpResult::Completed {
                op_id,
                value: "null".into(),
                request_id,
            },
            Ok(Some((bytes, meta))) => {
                let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                let out = json!({
                    "bytesBase64": b64,
                    "contentType": meta.content_type,
                    "size": meta.size,
                });
                OpResult::Completed { op_id, value: out.to_string(), request_id }
            }
            Err(e) => OpResult::Failed { op_id, error: e, request_id },
        }
    }));

    rv.set(promise.into());
}

// ---------------------------------------------------------------------------
// delete(bucket, key) → { deleted: bool }
// ---------------------------------------------------------------------------

pub fn delete(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope.get_slot::<SharedState>().expect("state").clone();

    let Some(bucket) = require_string_arg(scope, &args, 0, "bucket") else { return };
    let Some(key) = require_string_arg(scope, &args, 1, "key") else { return };

    let app_id = get_app_id(&state);
    let (op_id, request_id, promise) = setup_promise(scope, &state);

    let root = match current_root() {
        Ok(r) => r,
        Err(e) => {
            state.borrow_mut().spawned_ops.push(Box::pin(async move {
                OpResult::Failed { op_id, error: e, request_id }
            }));
            rv.set(promise.into());
            return;
        }
    };

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        match crate::backend::delete(&root, &app_id, &bucket, &key).await {
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
// list(bucket, prefix?) → [{ key, size, modifiedAt }]
// ---------------------------------------------------------------------------

pub fn list(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope.get_slot::<SharedState>().expect("state").clone();

    let Some(bucket) = require_string_arg(scope, &args, 0, "bucket") else { return };
    let prefix = optional_string_arg(scope, &args, 1).unwrap_or_default();

    let app_id = get_app_id(&state);
    let (op_id, request_id, promise) = setup_promise(scope, &state);

    let root = match current_root() {
        Ok(r) => r,
        Err(e) => {
            state.borrow_mut().spawned_ops.push(Box::pin(async move {
                OpResult::Failed { op_id, error: e, request_id }
            }));
            rv.set(promise.into());
            return;
        }
    };

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        match crate::backend::list(&root, &app_id, &bucket, &prefix).await {
            Ok(entries) => {
                let arr: Vec<serde_json::Value> = entries.into_iter().map(|e| {
                    let modified = e.modified_at
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0);
                    json!({ "key": e.key, "size": e.size, "modifiedAt": modified })
                }).collect();
                OpResult::Completed {
                    op_id,
                    value: serde_json::Value::Array(arr).to_string(),
                    request_id,
                }
            }
            Err(e) => OpResult::Failed { op_id, error: e, request_id },
        }
    }));

    rv.set(promise.into());
}
