//! V8 callbacks for `appbase.db.*` methods.
//!
//! Each callback:
//! 1. Reads arguments from V8
//! 2. Creates a Promise + resolver
//! 3. Builds an async query future
//! 4. Pushes the future into `state.spawned_ops`
//! 5. Returns the Promise to JS
//!
//! The runtime pump drains `spawned_ops`, polls the futures, and resolves
//! promises via `OpResult::Completed`.

use std::rc::Rc;

use appbase_runtime::state::{OpResult, SharedState};
use serde_json::Value;

use crate::query::{self, BuiltQuery};
use crate::DB_POOL;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Extract a string argument from V8, returning None if undefined/null.
fn get_string_arg(
    scope: &mut v8::PinScope<'_, '_>,
    args: &v8::FunctionCallbackArguments,
    index: i32,
) -> Option<String> {
    if args.length() <= index {
        return None;
    }
    let val = args.get(index);
    if val.is_null_or_undefined() {
        return None;
    }
    Some(val.to_rust_string_lossy(scope))
}

/// Extract a required string argument, or set an error on rv and return None.
fn require_string_arg(
    scope: &mut v8::PinScope<'_, '_>,
    args: &v8::FunctionCallbackArguments,
    index: i32,
    name: &str,
) -> Option<String> {
    match get_string_arg(scope, args, index) {
        Some(s) if !s.is_empty() => Some(s),
        _ => {
            let msg = v8::String::new(scope, &format!("db: missing required argument '{name}'"))
                .unwrap();
            let exc = v8::Exception::type_error(scope, msg);
            scope.throw_exception(exc);
            None
        }
    }
}

/// Parse a JSON string argument, defaulting to `{}` if absent.
fn parse_json_arg(
    scope: &mut v8::PinScope<'_, '_>,
    args: &v8::FunctionCallbackArguments,
    index: i32,
) -> Option<Value> {
    let raw = get_string_arg(scope, args, index).unwrap_or_default();
    if raw.is_empty() {
        return Some(Value::Object(serde_json::Map::new()));
    }
    match serde_json::from_str(&raw) {
        Ok(v) => Some(v),
        Err(e) => {
            let msg = v8::String::new(scope, &format!("db: invalid JSON: {e}")).unwrap();
            let exc = v8::Exception::type_error(scope, msg);
            scope.throw_exception(exc);
            None
        }
    }
}

/// Parse an optional integer argument.
#[allow(dead_code)]
fn get_i64_arg(
    scope: &mut v8::PinScope<'_, '_>,
    args: &v8::FunctionCallbackArguments,
    index: i32,
) -> Option<i64> {
    if args.length() <= index {
        return None;
    }
    let val = args.get(index);
    if val.is_null_or_undefined() {
        return None;
    }
    val.integer_value(scope)
}

/// Get the app_id from RuntimeState env_vars.
fn get_app_id(state: &SharedState) -> String {
    state
        .borrow()
        .env_vars
        .get("APP_ID")
        .cloned()
        .unwrap_or_else(|| "default".to_string())
}

/// Create a promise, allocate an op_id, store the resolver, and return
/// (op_id, request_id, promise).
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

/// Execute a built query via the pool and return JSON string result.
/// Lazily creates the pool on first use if not yet initialized.
async fn exec_query(bq: BuiltQuery) -> Result<String, String> {
    // Lazy pool init: if no pool yet, create one now
    let has_pool = DB_POOL.with(|p| p.borrow().is_some());
    if !has_pool {
        crate::init_pool_async().await.map_err(|e| format!("db: lazy init failed: {e}"))?;
    }

    let pool = DB_POOL.with(|p| {
        let borrow = p.borrow();
        borrow.as_ref().map(Rc::clone)
    });

    let pool = pool.ok_or_else(|| "db: pool not initialized".to_string())?;

    // Convert String params to references for the query call.
    // appbase-pg uses text params via ToSql trait — &str implements ToSql.
    let param_refs: Vec<&(dyn appbase_pg::ToSql + Sync)> =
        bq.params.iter().map(|s| s as &(dyn appbase_pg::ToSql + Sync)).collect();

    let rows = pool
        .query(&bq.sql, &param_refs)
        .await
        .map_err(|e| format!("db query error: {e}"))?;

    Ok(rows_to_json(&rows))
}

/// Execute a built query expecting a count result.
async fn exec_count(bq: BuiltQuery) -> Result<String, String> {
    let pool = DB_POOL.with(|p| {
        let borrow = p.borrow();
        borrow.as_ref().map(Rc::clone)
    });

    let pool = pool.ok_or_else(|| "db: pool not initialized".to_string())?;

    let param_refs: Vec<&(dyn appbase_pg::ToSql + Sync)> =
        bq.params.iter().map(|s| s as &(dyn appbase_pg::ToSql + Sync)).collect();

    let rows = pool
        .query(&bq.sql, &param_refs)
        .await
        .map_err(|e| format!("db query error: {e}"))?;

    let count: i64 = rows
        .first()
        .map(|r| r.get::<i64>("count"))
        .unwrap_or(0);

    Ok(serde_json::json!({ "count": count }).to_string())
}

/// Execute an insert/update/delete query, returning the affected rows.
async fn exec_mutation(bq: BuiltQuery) -> Result<String, String> {
    let pool = DB_POOL.with(|p| {
        let borrow = p.borrow();
        borrow.as_ref().map(Rc::clone)
    });

    let pool = pool.ok_or_else(|| "db: pool not initialized".to_string())?;

    let param_refs: Vec<&(dyn appbase_pg::ToSql + Sync)> =
        bq.params.iter().map(|s| s as &(dyn appbase_pg::ToSql + Sync)).collect();

    let rows = pool
        .query(&bq.sql, &param_refs)
        .await
        .map_err(|e| format!("db mutation error: {e}"))?;

    Ok(rows_to_json(&rows))
}

/// Convert rows to a JSON array string.
fn rows_to_json(rows: &[appbase_pg::Row]) -> String {
    let arr: Vec<Value> = rows.iter().map(row_to_json).collect();
    Value::Array(arr).to_string()
}

/// Convert a single Row to a JSON object.
///
/// Uses column OIDs to determine the appropriate JSON type:
/// - INT2/INT4/INT8 → number
/// - FLOAT4/FLOAT8 → number
/// - BOOL → boolean
/// - TEXT/VARCHAR → string
/// - UUID → string
/// - JSONB/JSON → parsed JSON value
/// - Everything else → string (via text representation)
fn row_to_json(row: &appbase_pg::Row) -> Value {
    let mut obj = serde_json::Map::new();
    for col in row.columns() {
        let key = col.name.clone();
        let value = column_to_json(row, &col.name, col.oid);
        obj.insert(key, value);
    }
    Value::Object(obj)
}

/// Convert a single column value to JSON based on its OID.
fn column_to_json(row: &appbase_pg::Row, name: &str, oid: u32) -> Value {
    // Try to get the value — if it's NULL, return null
    // OIDs from postgres_types::Type constants
    match oid {
        // BOOL = 16
        16 => match row.try_get::<bool>(name) {
            Ok(v) => Value::Bool(v),
            Err(_) => Value::Null,
        },
        // INT2 = 21
        21 => match row.try_get::<i16>(name) {
            Ok(v) => Value::Number(serde_json::Number::from(v)),
            Err(_) => Value::Null,
        },
        // INT4 = 23
        23 => match row.try_get::<i32>(name) {
            Ok(v) => Value::Number(serde_json::Number::from(v)),
            Err(_) => Value::Null,
        },
        // INT8 = 20
        20 => match row.try_get::<i64>(name) {
            Ok(v) => Value::Number(serde_json::Number::from(v)),
            Err(_) => Value::Null,
        },
        // FLOAT4 = 700
        700 => match row.try_get::<f32>(name) {
            Ok(v) => serde_json::Number::from_f64(f64::from(v))
                .map_or(Value::Null, Value::Number),
            Err(_) => Value::Null,
        },
        // FLOAT8 = 701
        701 => match row.try_get::<f64>(name) {
            Ok(v) => serde_json::Number::from_f64(v)
                .map_or(Value::Null, Value::Number),
            Err(_) => Value::Null,
        },
        // UUID = 2950
        2950 => match row.try_get::<uuid::Uuid>(name) {
            Ok(v) => Value::String(v.to_string()),
            Err(_) => Value::Null,
        },
        // JSON = 114, JSONB = 3802
        114 | 3802 => match row.try_get::<String>(name) {
            Ok(s) => serde_json::from_str(&s).unwrap_or(Value::String(s)),
            Err(_) => Value::Null,
        },
        // TEXT = 25, VARCHAR = 1043, CHAR = 18, BPCHAR = 1042, NAME = 19
        // and everything else: treat as text
        _ => match row.try_get::<String>(name) {
            Ok(v) => Value::String(v),
            Err(_) => Value::Null,
        },
    }
}

/// Build an error JSON string.
fn error_json(msg: &str) -> String {
    serde_json::json!({ "error": msg }).to_string()
}

// ---------------------------------------------------------------------------
// Callback: findOne(collection, filterJson, optsJson)
// ---------------------------------------------------------------------------

/// `appbase.db.findOne(collection, filterJson)` → Promise<object|null>
pub fn find_one(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();

    let Some(collection) = require_string_arg(scope, &args, 0, "collection") else {
        return;
    };
    let Some(filter) = parse_json_arg(scope, &args, 1) else {
        return;
    };


    let app_id = get_app_id(&state);
    let (op_id, request_id, promise) = setup_promise(scope, &state);

    let bq = match query::build_find(&app_id, &collection, &filter, Some(1), None, None) {
        Ok(q) => q,
        Err(e) => {
            // Resolve immediately with error
            state.borrow_mut().spawned_ops.push(Box::pin(async move {
                OpResult::Completed {
                    op_id,
                    value: error_json(&e.to_string()),
                    request_id,
                }
            }));
            rv.set(promise.into());
            return;
        }
    };

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        let value = match exec_query(bq).await {
            Ok(json) => {
                // findOne returns the first element or null
                let arr: Vec<Value> = serde_json::from_str(&json).unwrap_or_default();
                arr.into_iter().next().unwrap_or(Value::Null).to_string()
            }
            Err(e) => error_json(&e),
        };
        OpResult::Completed { op_id, value, request_id }
    }));

    rv.set(promise.into());
}

// ---------------------------------------------------------------------------
// Callback: find(collection, filterJson, optsJson)
// ---------------------------------------------------------------------------

/// `appbase.db.find(collection, filterJson, optsJson)` → Promise<array>
///
/// optsJson: `{ "limit": N, "offset": N, "orderBy": {...} }`
pub fn find(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();

    let Some(collection) = require_string_arg(scope, &args, 0, "collection") else {
        return;
    };
    let Some(filter) = parse_json_arg(scope, &args, 1) else {
        return;
    };
    let opts = parse_json_arg(scope, &args, 2).unwrap_or(Value::Object(serde_json::Map::new()));


    let app_id = get_app_id(&state);
    let limit = opts.get("limit").and_then(Value::as_i64);
    let offset = opts.get("offset").and_then(Value::as_i64);
    let order_by = opts.get("orderBy");
    let (op_id, request_id, promise) = setup_promise(scope, &state);

    let bq = match query::build_find(&app_id, &collection, &filter, limit, offset, order_by) {
        Ok(q) => q,
        Err(e) => {
            state.borrow_mut().spawned_ops.push(Box::pin(async move {
                OpResult::Completed {
                    op_id,
                    value: error_json(&e.to_string()),
                    request_id,
                }
            }));
            rv.set(promise.into());
            return;
        }
    };

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        let value = match exec_query(bq).await {
            Ok(json) => json,
            Err(e) => error_json(&e),
        };
        OpResult::Completed { op_id, value, request_id }
    }));

    rv.set(promise.into());
}

// ---------------------------------------------------------------------------
// Callback: insert(collection, docJson)
// ---------------------------------------------------------------------------

/// `appbase.db.insert(collection, docJson)` → Promise<object>
pub fn insert(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();

    let Some(collection) = require_string_arg(scope, &args, 0, "collection") else {
        return;
    };
    let Some(doc) = parse_json_arg(scope, &args, 1) else {
        return;
    };


    let app_id = get_app_id(&state);
    let (op_id, request_id, promise) = setup_promise(scope, &state);

    let bq = match query::build_insert(&app_id, &collection, &doc) {
        Ok(q) => q,
        Err(e) => {
            state.borrow_mut().spawned_ops.push(Box::pin(async move {
                OpResult::Completed {
                    op_id,
                    value: error_json(&e.to_string()),
                    request_id,
                }
            }));
            rv.set(promise.into());
            return;
        }
    };

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        let value = match exec_mutation(bq).await {
            Ok(json) => {
                // Return the first (inserted) row
                let arr: Vec<Value> = serde_json::from_str(&json).unwrap_or_default();
                arr.into_iter().next().unwrap_or(Value::Null).to_string()
            }
            Err(e) => error_json(&e),
        };
        OpResult::Completed { op_id, value, request_id }
    }));

    rv.set(promise.into());
}

// ---------------------------------------------------------------------------
// Callback: updateOne(collection, filterJson, updateJson)
// ---------------------------------------------------------------------------

/// `appbase.db.updateOne(collection, filterJson, updateJson)` → Promise<object|null>
pub fn update_one(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();

    let Some(collection) = require_string_arg(scope, &args, 0, "collection") else {
        return;
    };
    let Some(filter) = parse_json_arg(scope, &args, 1) else {
        return;
    };
    let Some(update) = parse_json_arg(scope, &args, 2) else {
        return;
    };


    let app_id = get_app_id(&state);
    let (op_id, request_id, promise) = setup_promise(scope, &state);

    let bq = match query::build_update_one(&app_id, &collection, &filter, &update) {
        Ok(q) => q,
        Err(e) => {
            state.borrow_mut().spawned_ops.push(Box::pin(async move {
                OpResult::Completed {
                    op_id,
                    value: error_json(&e.to_string()),
                    request_id,
                }
            }));
            rv.set(promise.into());
            return;
        }
    };

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        let value = match exec_mutation(bq).await {
            Ok(json) => {
                let arr: Vec<Value> = serde_json::from_str(&json).unwrap_or_default();
                arr.into_iter().next().unwrap_or(Value::Null).to_string()
            }
            Err(e) => error_json(&e),
        };
        OpResult::Completed { op_id, value, request_id }
    }));

    rv.set(promise.into());
}

// ---------------------------------------------------------------------------
// Callback: deleteOne(collection, filterJson)
// ---------------------------------------------------------------------------

/// `appbase.db.deleteOne(collection, filterJson)` → Promise<object|null>
pub fn delete_one(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();

    let Some(collection) = require_string_arg(scope, &args, 0, "collection") else {
        return;
    };
    let Some(filter) = parse_json_arg(scope, &args, 1) else {
        return;
    };


    let app_id = get_app_id(&state);
    let (op_id, request_id, promise) = setup_promise(scope, &state);

    let bq = match query::build_delete_one(&app_id, &collection, &filter) {
        Ok(q) => q,
        Err(e) => {
            state.borrow_mut().spawned_ops.push(Box::pin(async move {
                OpResult::Completed {
                    op_id,
                    value: error_json(&e.to_string()),
                    request_id,
                }
            }));
            rv.set(promise.into());
            return;
        }
    };

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        let value = match exec_mutation(bq).await {
            Ok(json) => {
                let arr: Vec<Value> = serde_json::from_str(&json).unwrap_or_default();
                arr.into_iter().next().unwrap_or(Value::Null).to_string()
            }
            Err(e) => error_json(&e),
        };
        OpResult::Completed { op_id, value, request_id }
    }));

    rv.set(promise.into());
}

// ---------------------------------------------------------------------------
// Callback: count(collection, filterJson)
// ---------------------------------------------------------------------------

/// `appbase.db.count(collection, filterJson)` → Promise<{ count: number }>
pub fn count(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();

    let Some(collection) = require_string_arg(scope, &args, 0, "collection") else {
        return;
    };
    let Some(filter) = parse_json_arg(scope, &args, 1) else {
        return;
    };


    let app_id = get_app_id(&state);
    let (op_id, request_id, promise) = setup_promise(scope, &state);

    let bq = match query::build_count(&app_id, &collection, &filter) {
        Ok(q) => q,
        Err(e) => {
            state.borrow_mut().spawned_ops.push(Box::pin(async move {
                OpResult::Completed {
                    op_id,
                    value: error_json(&e.to_string()),
                    request_id,
                }
            }));
            rv.set(promise.into());
            return;
        }
    };

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        let value = match exec_count(bq).await {
            Ok(json) => json,
            Err(e) => error_json(&e),
        };
        OpResult::Completed { op_id, value, request_id }
    }));

    rv.set(promise.into());
}
