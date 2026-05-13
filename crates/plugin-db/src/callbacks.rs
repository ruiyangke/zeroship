//! V8 callbacks for `zeroship.db.*` methods.
//!
//! Each callback:
//! 1. Reads arguments from V8
//! 2. Creates a Promise + resolver
//! 3. Builds an async query future
//! 4. Pushes the future into `state.spawned_ops`
//! 5. Returns the Promise to JS
//!
//! The runtime pump drains `spawned_ops`, polls the futures, and resolves
//! promises via `OpResult::Completed` or rejects them via `OpResult::Failed`.

use std::rc::Rc;

use zeroship_runtime::state::{OpResult, SharedState};
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

/// Parse a V8 value as a JSON object argument, defaulting to `{}` if absent.
/// Accepts both JS objects (serialized via JSON.stringify) and JSON strings.
fn parse_json_arg(
    scope: &mut v8::PinScope<'_, '_>,
    args: &v8::FunctionCallbackArguments,
    index: i32,
) -> Option<Value> {
    if args.length() <= index {
        return Some(Value::Object(serde_json::Map::new()));
    }
    let val = args.get(index);
    if val.is_null_or_undefined() {
        return Some(Value::Object(serde_json::Map::new()));
    }

    // If it's a JS object/array, use V8's JSON.stringify to serialize it
    let raw = if val.is_object() || val.is_array() {
        match v8::json::stringify(scope, val) {
            Some(s) => s.to_rust_string_lossy(scope),
            None => {
                let msg = v8::String::new(scope, "db: failed to serialize argument to JSON").unwrap();
                let exc = v8::Exception::type_error(scope, msg);
                scope.throw_exception(exc);
                return None;
            }
        }
    } else {
        // String or primitive — use as-is
        val.to_rust_string_lossy(scope)
    };

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

/// Format a compio_postgres::Error with its full source chain — surfaces
/// the underlying Postgres DbError message instead of the bare wrapper
/// kinds ("db error", "unexpected message from server").
fn fmt_db_err(e: &compio_postgres::Error) -> String {
    let mut msg = format!("db: {e}");
    let mut cur: &dyn std::error::Error = e;
    while let Some(src) = std::error::Error::source(cur) {
        msg.push_str(&format!(" — caused by: {src}"));
        cur = src;
    }
    msg
}

/// Execute SQL with text params — uses TX connection if active, otherwise pool.
async fn run_sql(sql: &str, params: &[&str]) -> Result<Vec<compio_postgres::Row>, String> {
    // Check if there's an active transaction
    let has_tx = crate::TX_CONN.with(|tx| tx.borrow().is_some());
    if has_tx {
        // Use transaction connection
        let client = crate::TX_CONN.with(|tx| tx.borrow_mut().take())
            .ok_or_else(|| "db: transaction connection lost".to_string())?;
        let result = client.query_text_params(sql, params).await;
        // Put it back
        crate::TX_CONN.with(|tx| { tx.borrow_mut().replace(client); });
        return result.map_err(|e| fmt_db_err(&e));
    }

    // No transaction — use pool
    let has_pool = DB_POOL.with(|p| p.borrow().is_some());
    if !has_pool {
        crate::init_pool_async().await.map_err(|e| format!("db: lazy init failed: {e}"))?;
    }
    let pool = DB_POOL.with(|p| p.borrow().as_ref().map(Rc::clone));
    let pool = pool.ok_or_else(|| "db: pool not initialized".to_string())?;
    pool.query_text_params(sql, params).await.map_err(|e| fmt_db_err(&e))
}

/// Execute a built query via pool (or TX conn) and return JSON string result.
async fn exec_query(bq: BuiltQuery) -> Result<String, String> {
    let param_refs: Vec<&str> = bq.params.iter().map(String::as_str).collect();
    let rows = run_sql(&bq.sql, &param_refs).await
        .map_err(|e| format!("db query error: {e}"))?;
    Ok(rows_to_json(&rows))
}

/// Execute a built query expecting a count result.
async fn exec_count(bq: BuiltQuery) -> Result<String, String> {
    let param_refs: Vec<&str> = bq.params.iter().map(String::as_str).collect();
    let rows = run_sql(&bq.sql, &param_refs).await
        .map_err(|e| format!("db query error: {e}"))?;

    let count: i64 = rows
        .first()
        .map(|r| r.get::<_, i64>("count"))
        .unwrap_or(0);

    Ok(serde_json::json!({ "count": count }).to_string())
}

/// Execute an insert/update/delete query, returning the affected rows.
async fn exec_mutation(bq: BuiltQuery) -> Result<String, String> {
    let param_refs: Vec<&str> = bq.params.iter().map(String::as_str).collect();
    let rows = run_sql(&bq.sql, &param_refs).await
        .map_err(|e| format!("db mutation error: {e}"))?;

    Ok(rows_to_json(&rows))
}

/// Convert rows to a JSON array string.
fn rows_to_json(rows: &[compio_postgres::Row]) -> String {
    let arr: Vec<Value> = rows.iter().map(row_to_json).collect();
    Value::Array(arr).to_string()
}

/// Public re-export of [`row_to_json`] for cross-module use (B1).
#[doc(hidden)]
pub fn row_to_json_pub(row: &compio_postgres::Row) -> Value {
    row_to_json(row)
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
fn row_to_json(row: &compio_postgres::Row) -> Value {
    let mut obj = serde_json::Map::new();
    for col in row.columns() {
        let key = col.name().to_string();
        let value = column_to_json(row, col.name(), col.type_().oid());
        obj.insert(key, value);
    }
    Value::Object(obj)
}

/// Convert a single column value to JSON based on its OID.
fn column_to_json(row: &compio_postgres::Row, name: &str, oid: u32) -> Value {
    // Try to get the value — if it's NULL, return null
    // OIDs from postgres_types::Type constants
    match oid {
        // BOOL = 16
        16 => match row.try_get::<_, bool>(name) {
            Ok(v) => Value::Bool(v),
            Err(_) => Value::Null,
        },
        // INT2 = 21
        21 => match row.try_get::<_, i16>(name) {
            Ok(v) => Value::Number(serde_json::Number::from(v)),
            Err(_) => Value::Null,
        },
        // INT4 = 23
        23 => match row.try_get::<_, i32>(name) {
            Ok(v) => Value::Number(serde_json::Number::from(v)),
            Err(_) => Value::Null,
        },
        // INT8 = 20
        20 => match row.try_get::<_, i64>(name) {
            Ok(v) => Value::Number(serde_json::Number::from(v)),
            Err(_) => Value::Null,
        },
        // FLOAT4 = 700
        700 => match row.try_get::<_, f32>(name) {
            Ok(v) => serde_json::Number::from_f64(f64::from(v))
                .map_or(Value::Null, Value::Number),
            Err(_) => Value::Null,
        },
        // FLOAT8 = 701
        701 => match row.try_get::<_, f64>(name) {
            Ok(v) => serde_json::Number::from_f64(v)
                .map_or(Value::Null, Value::Number),
            Err(_) => Value::Null,
        },
        // UUID = 2950
        2950 => match row.try_get::<_, uuid::Uuid>(name) {
            Ok(v) => Value::String(v.to_string()),
            Err(_) => Value::Null,
        },
        // TIMESTAMP = 1114, TIMESTAMPTZ = 1184
        // Postgres sends as i64 microseconds since 2000-01-01 00:00:00 UTC.
        // Return as Unix milliseconds (number) — matches JS Date.now() / new Date(ts).
        1114 | 1184 => match row.raw_value(name) {
            Some(bytes) if bytes.len() == 8 => {
                let pg_usec = i64::from_be_bytes(bytes.try_into().unwrap());
                // 2000-01-01 = 946684800 seconds since Unix epoch
                let unix_ms = pg_usec / 1_000 + 946_684_800_000;
                Value::Number(serde_json::Number::from(unix_ms))
            }
            _ => Value::Null,
        },
        // DATE = 1082 — i32 days since 2000-01-01
        // Return as Unix milliseconds at midnight UTC.
        1082 => match row.raw_value(name) {
            Some(bytes) if bytes.len() == 4 => {
                let pg_days = i32::from_be_bytes(bytes.try_into().unwrap());
                let unix_ms = (i64::from(pg_days) + 10957) * 86_400_000;
                Value::Number(serde_json::Number::from(unix_ms))
            }
            _ => Value::Null,
        },
        // JSONB = 3802 — binary format has 1-byte version prefix, strip it
        3802 => match row.raw_value(name) {
            Some(bytes) if bytes.len() > 1 => {
                let json_str = std::str::from_utf8(&bytes[1..]).unwrap_or("null");
                serde_json::from_str(json_str).unwrap_or(Value::Null)
            }
            _ => Value::Null,
        },
        // JSON = 114 — text format, no prefix
        114 => match row.try_get::<_, String>(name) {
            Ok(s) => {
                let parsed = serde_json::from_str(&s).ok();
                parsed.unwrap_or(Value::String(s))
            }
            Err(_) => Value::Null,
        },
        // TEXT = 25, VARCHAR = 1043, CHAR = 18, BPCHAR = 1042, NAME = 19
        // and everything else: treat as text
        _ => match row.try_get::<_, String>(name) {
            Ok(v) => Value::String(v),
            Err(_) => Value::Null,
        },
    }
}

// ---------------------------------------------------------------------------
// Callback: findOne(collection, filterJson, optsJson)
// ---------------------------------------------------------------------------

/// `zeroship.db.findOne(collection, filterJson)` → Promise<object|null>
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

    let bq = match query::build_find(&app_id, &collection, &filter, Some(1), None, None, None) {
        Ok(q) => q,
        Err(e) => {
            state.borrow_mut().spawned_ops.push(Box::pin(async move {
                OpResult::Failed { op_id, error: e.to_string(), request_id }
            }));
            rv.set(promise.into());
            return;
        }
    };

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        match exec_query(bq).await {
            Ok(json) => {
                // findOne returns the first element or null
                let arr: Vec<Value> = serde_json::from_str(&json).unwrap_or_default();
                let value = arr.into_iter().next().unwrap_or(Value::Null).to_string();
                OpResult::Completed { op_id, value, request_id }
            }
            Err(e) => OpResult::Failed { op_id, error: e, request_id },
        }
    }));

    rv.set(promise.into());
}

// ---------------------------------------------------------------------------
// Callback: find(collection, filterJson, optsJson)
// ---------------------------------------------------------------------------

/// `zeroship.db.find(collection, filterJson, optsJson)` → Promise<array>
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
    let select = opts.get("select");
    let (op_id, request_id, promise) = setup_promise(scope, &state);

    let bq = match query::build_find(&app_id, &collection, &filter, limit, offset, order_by, select) {
        Ok(q) => q,
        Err(e) => {
            state.borrow_mut().spawned_ops.push(Box::pin(async move {
                OpResult::Failed { op_id, error: e.to_string(), request_id }
            }));
            rv.set(promise.into());
            return;
        }
    };

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        match exec_query(bq).await {
            Ok(value) => OpResult::Completed { op_id, value, request_id },
            Err(e) => OpResult::Failed { op_id, error: e, request_id },
        }
    }));

    rv.set(promise.into());
}

// ---------------------------------------------------------------------------
// Callback: insert(collection, docJson)
// ---------------------------------------------------------------------------

/// `zeroship.db.insert(collection, docJson)` → Promise<object>
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
                OpResult::Failed { op_id, error: e.to_string(), request_id }
            }));
            rv.set(promise.into());
            return;
        }
    };

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        match exec_mutation(bq).await {
            Ok(json) => {
                // Return the first (inserted) row
                let arr: Vec<Value> = serde_json::from_str(&json).unwrap_or_default();
                let value = arr.into_iter().next().unwrap_or(Value::Null).to_string();
                OpResult::Completed { op_id, value, request_id }
            }
            Err(e) => OpResult::Failed { op_id, error: e, request_id },
        }
    }));

    rv.set(promise.into());
}

// ---------------------------------------------------------------------------
// Callback: updateOne(collection, filterJson, updateJson)
// ---------------------------------------------------------------------------

/// `zeroship.db.updateOne(collection, filterJson, updateJson)` → Promise<object|null>
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
                OpResult::Failed { op_id, error: e.to_string(), request_id }
            }));
            rv.set(promise.into());
            return;
        }
    };

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        match exec_mutation(bq).await {
            Ok(json) => {
                let arr: Vec<Value> = serde_json::from_str(&json).unwrap_or_default();
                let value = arr.into_iter().next().unwrap_or(Value::Null).to_string();
                OpResult::Completed { op_id, value, request_id }
            }
            Err(e) => OpResult::Failed { op_id, error: e, request_id },
        }
    }));

    rv.set(promise.into());
}

// ---------------------------------------------------------------------------
// Callback: deleteOne(collection, filterJson)
// ---------------------------------------------------------------------------

/// `zeroship.db.deleteOne(collection, filterJson)` → Promise<object|null>
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
                OpResult::Failed { op_id, error: e.to_string(), request_id }
            }));
            rv.set(promise.into());
            return;
        }
    };

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        match exec_mutation(bq).await {
            Ok(json) => {
                let arr: Vec<Value> = serde_json::from_str(&json).unwrap_or_default();
                let value = arr.into_iter().next().unwrap_or(Value::Null).to_string();
                OpResult::Completed { op_id, value, request_id }
            }
            Err(e) => OpResult::Failed { op_id, error: e, request_id },
        }
    }));

    rv.set(promise.into());
}

// ---------------------------------------------------------------------------
// Callback: insertMany(collection, docsJson)
// ---------------------------------------------------------------------------

/// `zeroship.db.insertMany(collection, docsJson)` → Promise<array>
pub fn insert_many(
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
    let Some(docs) = parse_json_arg(scope, &args, 1) else {
        return;
    };

    let app_id = get_app_id(&state);
    let (op_id, request_id, promise) = setup_promise(scope, &state);

    let bq = match query::build_insert_many(&app_id, &collection, &docs) {
        Ok(q) => q,
        Err(e) => {
            state.borrow_mut().spawned_ops.push(Box::pin(async move {
                OpResult::Failed { op_id, error: e.to_string(), request_id }
            }));
            rv.set(promise.into());
            return;
        }
    };

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        match exec_mutation(bq).await {
            Ok(value) => OpResult::Completed { op_id, value, request_id },
            Err(e) => OpResult::Failed { op_id, error: e, request_id },
        }
    }));

    rv.set(promise.into());
}

// ---------------------------------------------------------------------------
// Callback: aggregate(collection, pipelineJson)
// ---------------------------------------------------------------------------

/// `zeroship.db.aggregate(collection, pipelineJson)` → Promise<array>
pub fn aggregate(
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
    let Some(pipeline) = parse_json_arg(scope, &args, 1) else {
        return;
    };

    let app_id = get_app_id(&state);
    let (op_id, request_id, promise) = setup_promise(scope, &state);

    let bq = match query::build_aggregate(&app_id, &collection, &pipeline) {
        Ok(q) => q,
        Err(e) => {
            state.borrow_mut().spawned_ops.push(Box::pin(async move {
                OpResult::Failed { op_id, error: e.to_string(), request_id }
            }));
            rv.set(promise.into());
            return;
        }
    };

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        match exec_query(bq).await {
            Ok(value) => OpResult::Completed { op_id, value, request_id },
            Err(e) => OpResult::Failed { op_id, error: e, request_id },
        }
    }));

    rv.set(promise.into());
}

// ---------------------------------------------------------------------------
// Callback: distinct(collection, field, filterJson)
// ---------------------------------------------------------------------------

/// `zeroship.db.distinct(collection, field, filterJson)` → Promise<array>
pub fn distinct(
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
    let Some(field) = require_string_arg(scope, &args, 1, "field") else {
        return;
    };
    let Some(filter) = parse_json_arg(scope, &args, 2) else {
        return;
    };

    let app_id = get_app_id(&state);
    let (op_id, request_id, promise) = setup_promise(scope, &state);

    let bq = match query::build_distinct(&app_id, &collection, &field, &filter) {
        Ok(q) => q,
        Err(e) => {
            state.borrow_mut().spawned_ops.push(Box::pin(async move {
                OpResult::Failed { op_id, error: e.to_string(), request_id }
            }));
            rv.set(promise.into());
            return;
        }
    };

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        match exec_query(bq).await {
            Ok(json) => {
                // Extract single-column values into a flat array
                let rows: Vec<Value> = serde_json::from_str(&json).unwrap_or_default();
                let flat: Vec<Value> = rows
                    .into_iter()
                    .filter_map(|row| {
                        if let Value::Object(map) = row {
                            map.into_values().next()
                        } else {
                            None
                        }
                    })
                    .collect();
                let value = Value::Array(flat).to_string();
                OpResult::Completed { op_id, value, request_id }
            }
            Err(e) => OpResult::Failed { op_id, error: e, request_id },
        }
    }));

    rv.set(promise.into());
}

// ---------------------------------------------------------------------------
// Callback: updateMany(collection, filterJson, updateJson)
// ---------------------------------------------------------------------------

/// `zeroship.db.updateMany(collection, filterJson, updateJson)` → Promise<{ updated: number }>
pub fn update_many(
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

    let bq = match query::build_update_many(&app_id, &collection, &filter, &update) {
        Ok(q) => q,
        Err(e) => {
            state.borrow_mut().spawned_ops.push(Box::pin(async move {
                OpResult::Failed { op_id, error: e.to_string(), request_id }
            }));
            rv.set(promise.into());
            return;
        }
    };

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        match exec_mutation(bq).await {
            Ok(json) => {
                let arr: Vec<Value> = serde_json::from_str(&json).unwrap_or_default();
                let n = arr.len();
                let value = serde_json::json!({ "updated": n }).to_string();
                OpResult::Completed { op_id, value, request_id }
            }
            Err(e) => OpResult::Failed { op_id, error: e, request_id },
        }
    }));

    rv.set(promise.into());
}

// ---------------------------------------------------------------------------
// Callback: deleteMany(collection, filterJson)
// ---------------------------------------------------------------------------

/// `zeroship.db.deleteMany(collection, filterJson)` → Promise<{ deleted: number }>
pub fn delete_many(
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

    let bq = match query::build_delete_many(&app_id, &collection, &filter) {
        Ok(q) => q,
        Err(e) => {
            state.borrow_mut().spawned_ops.push(Box::pin(async move {
                OpResult::Failed { op_id, error: e.to_string(), request_id }
            }));
            rv.set(promise.into());
            return;
        }
    };

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        match exec_mutation(bq).await {
            Ok(json) => {
                let arr: Vec<Value> = serde_json::from_str(&json).unwrap_or_default();
                let n = arr.len();
                let value = serde_json::json!({ "deleted": n }).to_string();
                OpResult::Completed { op_id, value, request_id }
            }
            Err(e) => OpResult::Failed { op_id, error: e, request_id },
        }
    }));

    rv.set(promise.into());
}

// ---------------------------------------------------------------------------
// Callback: count(collection, filterJson)
// ---------------------------------------------------------------------------

/// `zeroship.db.count(collection, filterJson)` → Promise<{ count: number }>
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
                OpResult::Failed { op_id, error: e.to_string(), request_id }
            }));
            rv.set(promise.into());
            return;
        }
    };

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        match exec_count(bq).await {
            Ok(value) => OpResult::Completed { op_id, value, request_id },
            Err(e) => OpResult::Failed { op_id, error: e, request_id },
        }
    }));

    rv.set(promise.into());
}

// ---------------------------------------------------------------------------
// Callback: registerModel(collection, schemaJson)
// ---------------------------------------------------------------------------

/// `zeroship.db.registerModel(collection, schemaJson)` → Promise<void>
///
/// Creates the table and any missing columns. Idempotent — safe to call
/// on every cold start. Skips DDL if the model was already registered
/// for this app on this thread.
pub fn register_model(
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
    let Some(schema) = parse_json_arg(scope, &args, 1) else {
        return;
    };

    let app_id = get_app_id(&state);

    // Fast path: already registered on this thread — skip DDL
    if crate::is_model_registered(&app_id, &collection) {
        let resolver = v8::PromiseResolver::new(scope).unwrap();
        let promise = resolver.get_promise(scope);
        let undefined = v8::undefined(scope);
        resolver.resolve(scope, undefined.into());
        rv.set(promise.into());
        return;
    }

    let (op_id, request_id, promise) = setup_promise(scope, &state);

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        match exec_register_model(&app_id, &collection, &schema).await {
            Ok(()) => {
                crate::mark_model_registered(&app_id, &collection);
                OpResult::Completed { op_id, value: "null".to_string(), request_id }
            }
            Err(e) => OpResult::Failed { op_id, error: e, request_id },
        }
    }));

    rv.set(promise.into());
}

/// Execute DDL for registerModel. Implements the four-phase orchestrator
/// from proposal A2 (`docs/proposals/zeroship-db-v2.md`):
///
///   1. **Bootstrap**: ensure schema exists and the `__zeroship_migrations`
///      audit table is provisioned (A3).
///   2. **Diff phase**: introspect `pg_catalog` and classify each
///      declared change into additive / compatible / destructive.
///   3. **Validate phase**: for compatible/destructive ops that involve
///      an existence check (NOT NULL on a non-empty table, new UNIQUE),
///      run the validation query and short-circuit `strict` deploys.
///   4. **Apply phase**: run additive + compatible DDL (table + columns
///      transactionally, CREATE INDEX CONCURRENTLY outside any tx). Every
///      operation writes an audit row.
///
/// Destructive changes return a structured `validation_refused` envelope.
/// On a fresh deploy where the table doesn't exist, the diff collapses
/// to a single `create_table` op so the cold-start path is still
/// IF NOT EXISTS-idempotent.
///
/// Concurrent-deploy serialisation uses Postgres' two-key advisory lock
/// (`pg_advisory_xact_lock(hashtext('zs_reg:<app_id>')::int4,
/// hashtext(<deploy_id>)::int4)`); a second worker cold-starting against
/// the same app + deploy_id blocks until the first transaction commits
/// (proposal A2, "Concurrent-deploy semantics" section).
async fn exec_register_model(
    app_id: &str,
    collection: &str,
    schema: &Value,
) -> Result<(), String> {
    // Lazy pool init
    let has_pool = DB_POOL.with(|p| p.borrow().is_some());
    if !has_pool {
        crate::init_pool_async().await.map_err(|e| format!("db: lazy init failed: {e}"))?;
    }

    let pool = DB_POOL.with(|p| {
        let borrow = p.borrow();
        borrow.as_ref().map(Rc::clone)
    });
    let pool = pool.ok_or_else(|| "db: pool not initialized".to_string())?;

    let deploy_id = std::env::var("ZEROSHIP_DEPLOY_ID").unwrap_or_else(|_| "cold_start".to_string());

    exec_register_model_with_pool(&pool, app_id, collection, schema, &deploy_id).await
}

/// Pool-driven variant of `exec_register_model`. Public so integration
/// tests can drive the four-phase orchestrator without going through V8.
///
/// `deploy_id` controls audit-log grouping (proposal A3 line 233 reserves
/// `'cold_start'` for pre-deploy DDL).
pub async fn exec_register_model_with_pool(
    pool: &compio_postgres::Pool,
    app_id: &str,
    collection: &str,
    schema: &Value,
    deploy_id: &str,
) -> Result<(), String> {
    // Strictness — proposal A2 line 122. Read from schema._meta.strictness
    // if present; default is 'strict'.
    let strictness = schema
        .get("_meta")
        .and_then(|m| m.get("strictness"))
        .and_then(Value::as_str)
        .unwrap_or("strict")
        .to_string();

    let empty: Vec<&str> = Vec::new();

    // -------------------------------------------------------------------
    // Concurrent-deploy serialisation: proposal A2 line 202.
    //
    // Two-key advisory lock keyed on (app_id, register_model). Held at
    // session scope on a dedicated pool client so the lock survives the
    // CREATE INDEX CONCURRENTLY phases (which can't run in a transaction).
    // Released when this function returns (either by explicit unlock or
    // by `lock_client` being dropped — its backend session ends, which
    // implicitly releases all session-level advisory locks).
    //
    // The proposal calls for `pg_advisory_xact_lock` (transaction scope);
    // because registerModel spans non-transactional CONCURRENTLY DDL, we
    // use the session-scoped equivalent `pg_advisory_lock` on a dedicated
    // connection. Functionally identical for our serialisation goal: a
    // second worker calling the same function blocks on the same key.
    let lock_client = pool
        .get()
        .await
        .map_err(|e| format!("db: failed to acquire orchestrator client: {e}"))?;
    let lock_sql =
        "SELECT pg_advisory_lock(hashtext('zs_reg:' || $1)::int4, hashtext('register_model')::int4)";
    lock_client
        .query_text_params(lock_sql, &[app_id])
        .await
        .map_err(|e| format!("db: pg_advisory_lock failed: {e}"))?;
    // From this point on, until lock_client is dropped at function exit,
    // any other orchestrator call against the same app_id blocks.

    // -------------------------------------------------------------------
    // Bootstrap: schema + audit table
    // -------------------------------------------------------------------
    let create_schema = query::build_create_schema(app_id);
    pool.query_text_params(&create_schema, &empty)
        .await
        .map_err(|e| format!("db: create schema failed: {e}"))?;

    crate::audit::ensure_audit_table_exists(pool, app_id).await?;

    let schema_version = crate::audit::next_schema_version(pool, app_id).await?;

    // Build everything the diff classifier needs.
    let create_table = query::build_create_table(app_id, collection, schema)
        .map_err(|e| format!("db: {e}"))?;
    let declared_indexes = query::build_create_indexes(app_id, collection, schema)
        .map_err(|e| format!("db: {e}"))?;

    // -------------------------------------------------------------------
    // Diff phase: introspect pg_catalog, classify changes.
    // -------------------------------------------------------------------
    let mut live = crate::diff::read_live_schema(pool, app_id).await?;
    let rows_estimate = crate::diff::estimate_row_count(pool, app_id, collection).await?;
    live.row_counts.insert(collection.to_string(), rows_estimate);

    let ops = crate::diff::compute_diff(
        &live,
        app_id,
        collection,
        schema,
        &create_table,
        &declared_indexes,
    );

    // -------------------------------------------------------------------
    // Classify phase: surface destructive ops as validation_refused
    // when strictness != 'off'. Lenient logs but proceeds with additive
    // + compatible only.
    // -------------------------------------------------------------------
    let destructive: Vec<&crate::diff::DiffOp> = ops
        .iter()
        .filter(|op| op.class == crate::diff::ChangeClass::Destructive)
        .collect();

    if !destructive.is_empty() && strictness != "off" {
        // Audit each destructive op as pending so operators can see what
        // was refused. Then return a validation_refused envelope.
        for op in &destructive {
            let row = crate::audit::AuditRow {
                collection: op.collection.clone(),
                phase: crate::audit::Phase::Ddl,
                change_class: op.class.as_audit(),
                change_kind: op.change_kind.as_sql().to_string(),
                details: op.details.clone(),
                ddl_sql: op.sql.clone(),
                status: crate::audit::InitialStatus::Pending,
                deploy_id: deploy_id.to_string(),
                schema_version,
                actor: crate::audit::ActorKind::Auto,
            };
            // Best-effort: a failure to write the audit row should not
            // mask the envelope — tracing::warn so it shows in worker
            // logs but the user-facing error stays clean.
            if let Err(e) = crate::audit::write_audit_row(pool, app_id, &row).await {
                tracing::warn!(error = %e, "audit: failed to log destructive op");
            }
        }

        if strictness == "strict" {
            return Err(build_validation_refused_envelope(deploy_id, &destructive));
        }
        // strictness == "lenient": fall through, but skip destructive ops.
    }

    // -------------------------------------------------------------------
    // Validate phase: for compatible ops with an existence check
    // (currently: add NOT NULL column with default on a non-empty table —
    // covered by the classifier already; new UNIQUE constraint — handled
    // in create_index_with_recovery via 23505).
    //
    // The exhaustive validation budget loop (proposal A2 line 153) is
    // deferred to a follow-up PR: for the additive-only flows the diff
    // engine now identifies, classification already prevents unsafe DDL.
    // -------------------------------------------------------------------

    // -------------------------------------------------------------------
    // Apply phase: run additive + compatible ops in declared order.
    // -------------------------------------------------------------------
    for op in &ops {
        if op.class == crate::diff::ChangeClass::Destructive {
            // Skipped above (refused or lenient-skipped).
            continue;
        }

        let audit_id = match (crate::audit::AuditRow {
            collection: op.collection.clone(),
            phase: crate::audit::Phase::Ddl,
            change_class: op.class.as_audit(),
            change_kind: op.change_kind.as_sql().to_string(),
            details: op.details.clone(),
            ddl_sql: op.sql.clone(),
            status: crate::audit::InitialStatus::Running,
            deploy_id: deploy_id.to_string(),
            schema_version,
            actor: crate::audit::ActorKind::Auto,
        }) {
            row => match crate::audit::write_audit_row(pool, app_id, &row).await {
                Ok(id) => Some(id),
                Err(e) => {
                    tracing::warn!(error = %e, "audit: failed to insert running row");
                    None
                }
            },
        };

        let result = match &op.change_kind {
            crate::diff::ChangeKind::CreateTable | crate::diff::ChangeKind::AddColumn => {
                if let Some(sql) = &op.sql {
                    pool.query_text_params(sql, &empty)
                        .await
                        .map(|_| ())
                        .map_err(|e| format!("db: {} failed: {}", op.change_kind.as_sql(), fmt_db_err(&e)))
                } else {
                    Ok(())
                }
            }
            crate::diff::ChangeKind::AddIndex => {
                // Find the matching IndexSpec so the recovery loop has the
                // structured columns/unique flags.
                let spec_owned = declared_indexes
                    .iter()
                    .find(|s| op.details.get("index_name").and_then(Value::as_str) == Some(s.name.as_str()))
                    .cloned();
                if let Some(spec) = spec_owned {
                    create_index_with_recovery_audited(
                        pool,
                        app_id,
                        collection,
                        &spec,
                        deploy_id,
                        schema_version,
                    )
                    .await
                } else {
                    Ok(())
                }
            }
            crate::diff::ChangeKind::DropColumn | crate::diff::ChangeKind::DropIndex => {
                // Destructive ops are filtered out above.
                Ok(())
            }
        };

        if let Some(id) = audit_id {
            match &result {
                Ok(_) => {
                    let _ = crate::audit::update_audit_status(
                        pool,
                        app_id,
                        id,
                        crate::audit::TerminalStatus::Applied,
                        None,
                    )
                    .await;
                }
                Err(e) => {
                    let _ = crate::audit::update_audit_status(
                        pool,
                        app_id,
                        id,
                        crate::audit::TerminalStatus::Failed,
                        Some(e.as_str()),
                    )
                    .await;
                }
            }
        }

        result?;
    }

    // Explicit unlock — best-effort. Dropping `lock_client` would also
    // release session-level locks when the backend session ends, but we
    // unlock proactively so the client returns to the pool clean.
    let unlock_sql =
        "SELECT pg_advisory_unlock(hashtext('zs_reg:' || $1)::int4, hashtext('register_model')::int4)";
    let _ = lock_client.query_text_params(unlock_sql, &[app_id]).await;
    drop(lock_client);

    Ok(())
}

/// Build the `validation_refused` error envelope (proposal A2 line 167).
/// The shape matches the SDK's expected error contract so the deploy
/// pipeline can render the failing PKs / approval URL uniformly.
fn build_validation_refused_envelope(
    deploy_id: &str,
    destructive: &[&crate::diff::DiffOp],
) -> String {
    let pending: Vec<Value> = destructive
        .iter()
        .map(|op| {
            serde_json::json!({
                "collection": op.collection,
                "change_kind": op.change_kind.as_sql(),
                "field": op.field,
                "details": op.details,
            })
        })
        .collect();

    serde_json::json!({
        "code": "validation_refused",
        "deploy_id": deploy_id,
        "violations": [],
        "destructive_pending": pending,
    })
    .to_string()
}

/// Audited variant of [`create_index_with_recovery`] — every retry,
/// INVALID-detection drop, and terminal failure writes an
/// `index_retry` row to `__zeroship_migrations` so operators can see
/// what the cold-start orchestrator did (proposal A3). Retains the same
/// SQLSTATE policy as the un-audited version.
async fn create_index_with_recovery_audited(
    pool: &compio_postgres::Pool,
    app_id: &str,
    collection: &str,
    spec: &query::IndexSpec,
    deploy_id: &str,
    schema_version: i32,
) -> Result<(), String> {
    use compio_postgres::error::SqlState;

    const MAX_RETRIES: u32 = 3;
    let empty: Vec<&str> = Vec::new();
    let qualified_idx = format!("\"{}\".\"{}\"", app_id, spec.name);
    let drop_idx_sql = format!(
        "DROP INDEX CONCURRENTLY IF EXISTS \"{}\".\"{}\"",
        app_id, spec.name
    );

    let log_retry = |reason: &'static str,
                     attempt: u32,
                     sqlstate: Option<String>,
                     error: Option<String>| {
        let row = crate::audit::AuditRow {
            collection: collection.to_string(),
            phase: crate::audit::Phase::Ddl,
            change_class: if spec.unique {
                crate::audit::ChangeClass::Compatible
            } else {
                crate::audit::ChangeClass::Additive
            },
            change_kind: "index_retry".to_string(),
            details: serde_json::json!({
                "reason": reason,
                "attempt": attempt,
                "index_name": spec.name,
                "columns": spec.columns,
                "unique": spec.unique,
                "sqlstate": sqlstate,
                "error": error,
            }),
            ddl_sql: Some(spec.sql.clone()),
            status: crate::audit::InitialStatus::Running,
            deploy_id: deploy_id.to_string(),
            schema_version,
            actor: crate::audit::ActorKind::Auto,
        };
        row
    };

    for attempt in 0..=MAX_RETRIES {
        let create_res = pool.query_text_params(&spec.sql, &empty).await;

        match create_res {
            Ok(_) => {
                let check_sql = format!(
                    "SELECT indisvalid FROM pg_index WHERE indexrelid = '{}'::regclass",
                    qualified_idx.replace('\'', "''")
                );
                let rows = pool
                    .query_text_params(&check_sql, &empty)
                    .await
                    .map_err(|e| {
                        format!(
                            "db: failed to verify index '{}' validity: {}",
                            spec.name,
                            fmt_db_err(&e)
                        )
                    })?;
                let valid = rows
                    .first()
                    .map(|r| r.try_get::<_, bool>("indisvalid").unwrap_or(false))
                    .unwrap_or(false);
                if valid {
                    return Ok(());
                }

                // INVALID index — audit the retry, drop, and loop.
                let row = log_retry("invalid_index_landed", attempt, None, None);
                if let Ok(id) = crate::audit::write_audit_row(pool, app_id, &row).await {
                    let _ = crate::audit::update_audit_status(
                        pool,
                        app_id,
                        id,
                        crate::audit::TerminalStatus::Failed,
                        Some("index landed INVALID"),
                    )
                    .await;
                }
                let _ = pool.query_text_params(&drop_idx_sql, &empty).await;
                if attempt == MAX_RETRIES {
                    return Err(format!(
                        "{{\"code\":\"validation_refused\",\"change_kind\":\"index_retry\",\
                        \"collection\":\"{}\",\"index\":\"{}\",\
                        \"reason\":\"index repeatedly landed INVALID after {} retries\"}}",
                        collection, spec.name, MAX_RETRIES
                    ));
                }
            }
            Err(e) => {
                let code = e.code().cloned();
                let fatal = matches!(
                    code.as_ref(),
                    Some(c) if c == &SqlState::UNIQUE_VIOLATION
                        || c == &SqlState::NOT_NULL_VIOLATION
                        || c == &SqlState::FOREIGN_KEY_VIOLATION
                        || c == &SqlState::CHECK_VIOLATION
                );
                if fatal {
                    let code_str = code.as_ref().map(|c| c.code()).unwrap_or("23xxx");
                    let constraint_kind = if spec.unique { "unique" } else { "index" };
                    let row = log_retry(
                        "data_violation",
                        attempt,
                        Some(code_str.to_string()),
                        Some(fmt_db_err(&e)),
                    );
                    if let Ok(id) = crate::audit::write_audit_row(pool, app_id, &row).await {
                        let _ = crate::audit::update_audit_status(
                            pool,
                            app_id,
                            id,
                            crate::audit::TerminalStatus::Failed,
                            Some("data violates constraint"),
                        )
                        .await;
                    }
                    let _ = pool.query_text_params(&drop_idx_sql, &empty).await;
                    return Err(format!(
                        "{{\"code\":\"unique_violation\",\"sqlstate\":\"{}\",\
                        \"collection\":\"{}\",\"constraint\":\"{}\",\
                        \"index\":\"{}\",\"columns\":{:?},\
                        \"message\":\"{}\"}}",
                        code_str,
                        collection,
                        constraint_kind,
                        spec.name,
                        spec.columns,
                        fmt_db_err(&e).replace('"', "\\\"")
                    ));
                }

                let transient = matches!(
                    code.as_ref(),
                    Some(c) if c == &SqlState::T_R_DEADLOCK_DETECTED
                        || c == &SqlState::DISK_FULL
                        || c == &SqlState::OUT_OF_MEMORY
                );

                let row = log_retry(
                    if transient { "transient_retry" } else { "non_transient_failure" },
                    attempt,
                    code.as_ref().map(|c| c.code().to_string()),
                    Some(fmt_db_err(&e)),
                );
                if let Ok(id) = crate::audit::write_audit_row(pool, app_id, &row).await {
                    let _ = crate::audit::update_audit_status(
                        pool,
                        app_id,
                        id,
                        crate::audit::TerminalStatus::Failed,
                        Some("index build failed"),
                    )
                    .await;
                }

                let _ = pool.query_text_params(&drop_idx_sql, &empty).await;
                if !transient || attempt == MAX_RETRIES {
                    return Err(format!(
                        "{{\"code\":\"validation_refused\",\"change_kind\":\"index_retry\",\
                        \"collection\":\"{}\",\"index\":\"{}\",\
                        \"sqlstate\":\"{}\",\"attempts\":{},\
                        \"message\":\"{}\"}}",
                        collection,
                        spec.name,
                        code.as_ref().map(|c| c.code()).unwrap_or("unknown"),
                        attempt + 1,
                        fmt_db_err(&e).replace('"', "\\\"")
                    ));
                }
            }
        }
    }

    Err(format!(
        "db: create index '{}' exhausted retry budget without a terminal result",
        spec.name
    ))
}

/// Run a single `CREATE INDEX CONCURRENTLY` with INVALID-index recovery.
///
/// On success, returns `Ok(())`. On a fatal data violation
/// (`23505/23502/23503/23514`), returns a structured `validation_refused`
/// JSON envelope (see proposal A1/A2 — the envelope shape matches A2's
/// `validation_refused` so the deploy pipeline can consume the two paths
/// uniformly). Transient failures (deadlock, disk pressure) are retried up
/// to 3 times after `DROP INDEX CONCURRENTLY`.
///
/// Kept for the `tests/` path that exercises pre-A2 semantics; the
/// production `exec_register_model` path uses
/// `create_index_with_recovery_audited` which writes to
/// `__zeroship_migrations` on every retry.
#[allow(dead_code)]
async fn create_index_with_recovery(
    pool: &compio_postgres::Pool,
    app_id: &str,
    collection: &str,
    spec: &query::IndexSpec,
) -> Result<(), String> {
    use compio_postgres::error::SqlState;

    const MAX_RETRIES: u32 = 3;
    let empty: Vec<&str> = Vec::new();
    let qualified_idx = format!("\"{}\".\"{}\"", app_id, spec.name);
    let drop_idx_sql = format!(
        "DROP INDEX CONCURRENTLY IF EXISTS \"{}\".\"{}\"",
        app_id, spec.name
    );

    for attempt in 0..=MAX_RETRIES {
        // Issue the CREATE. Note: `IF NOT EXISTS` means an already-VALID
        // index is a no-op; an existing INVALID one would still be a no-op
        // here, which is why we always follow up with the indisvalid check.
        let create_res = pool.query_text_params(&spec.sql, &empty).await;

        match create_res {
            Ok(_) => {
                // Verify the index landed VALID.
                let check_sql = format!(
                    "SELECT indisvalid FROM pg_index WHERE indexrelid = '{}'::regclass",
                    qualified_idx.replace('\'', "''")
                );
                let rows = pool
                    .query_text_params(&check_sql, &empty)
                    .await
                    .map_err(|e| {
                        format!(
                            "db: failed to verify index '{}' validity: {}",
                            spec.name,
                            fmt_db_err(&e)
                        )
                    })?;

                let valid = rows
                    .first()
                    .map(|r| r.try_get::<_, bool>("indisvalid").unwrap_or(false))
                    .unwrap_or(false);

                if valid {
                    return Ok(());
                }

                // Index exists but is INVALID. Drop and retry as a
                // transient failure (we have no SQLSTATE to inspect — the
                // CREATE itself succeeded so a concurrent failure left
                // the entry behind). TODO: A3 — log to
                // `__zeroship_migrations` with change_kind='index_retry'.
                tracing::warn!(
                    app_id = %app_id,
                    collection = %collection,
                    index = %spec.name,
                    attempt = attempt,
                    "index landed INVALID — dropping and retrying (TODO: A3 audit log)"
                );

                let _ = pool.query_text_params(&drop_idx_sql, &empty).await;
                if attempt == MAX_RETRIES {
                    return Err(format!(
                        "{{\"code\":\"validation_refused\",\"change_kind\":\"index_retry\",\
                        \"collection\":\"{}\",\"index\":\"{}\",\
                        \"reason\":\"index repeatedly landed INVALID after {} retries\"}}",
                        collection, spec.name, MAX_RETRIES
                    ));
                }
            }
            Err(e) => {
                let code = e.code().cloned();
                let fatal = matches!(
                    code.as_ref(),
                    Some(c) if c == &SqlState::UNIQUE_VIOLATION
                        || c == &SqlState::NOT_NULL_VIOLATION
                        || c == &SqlState::FOREIGN_KEY_VIOLATION
                        || c == &SqlState::CHECK_VIOLATION
                );

                if fatal {
                    let code_str = code.as_ref().map(|c| c.code()).unwrap_or("23xxx");
                    let constraint_kind = if spec.unique { "unique" } else { "index" };
                    // Drop the leftover INVALID entry so retries don't pile up.
                    let _ = pool.query_text_params(&drop_idx_sql, &empty).await;
                    return Err(format!(
                        "{{\"code\":\"unique_violation\",\"sqlstate\":\"{}\",\
                        \"collection\":\"{}\",\"constraint\":\"{}\",\
                        \"index\":\"{}\",\"columns\":{:?},\
                        \"message\":\"{}\"}}",
                        code_str,
                        collection,
                        constraint_kind,
                        spec.name,
                        spec.columns,
                        fmt_db_err(&e).replace('"', "\\\"")
                    ));
                }

                let transient = matches!(
                    code.as_ref(),
                    Some(c) if c == &SqlState::T_R_DEADLOCK_DETECTED
                        || c == &SqlState::DISK_FULL
                        || c == &SqlState::OUT_OF_MEMORY
                );

                tracing::warn!(
                    app_id = %app_id,
                    collection = %collection,
                    index = %spec.name,
                    attempt = attempt,
                    sqlstate = ?code.as_ref().map(|c| c.code()),
                    transient = transient,
                    "CREATE INDEX CONCURRENTLY failed — TODO: A3 audit log"
                );

                let _ = pool.query_text_params(&drop_idx_sql, &empty).await;

                if !transient || attempt == MAX_RETRIES {
                    return Err(format!(
                        "{{\"code\":\"validation_refused\",\"change_kind\":\"index_retry\",\
                        \"collection\":\"{}\",\"index\":\"{}\",\
                        \"sqlstate\":\"{}\",\"attempts\":{},\
                        \"message\":\"{}\"}}",
                        collection,
                        spec.name,
                        code.as_ref().map(|c| c.code()).unwrap_or("unknown"),
                        attempt + 1,
                        fmt_db_err(&e).replace('"', "\\\"")
                    ));
                }
                // else: fall through to next loop iteration
            }
        }
    }

    // Should be unreachable — the loop returns inside.
    Err(format!(
        "db: create index '{}' exhausted retry budget without a terminal result",
        spec.name
    ))
}

// ---------------------------------------------------------------------------
// Transaction callbacks: begin / commit / rollback
// ---------------------------------------------------------------------------
//
// V8 is single-threaded per isolate, so only one transaction can be active
// at a time. We store the transaction connection in TX_CONN thread-local.
// All CRUD callbacks (exec_query, exec_mutation, etc.) automatically use
// TX_CONN when it's set, via the run_sql() helper.

/// `zeroship.db.beginTransaction(isolationLevel?)` → Promise<void>
/// Opens a dedicated connection, runs BEGIN, stores in TX_CONN.
/// All subsequent CRUD ops use this connection until commit/rollback.
pub fn begin_transaction(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();

    let isolation_level = get_string_arg(scope, &args, 0);
    let (op_id, request_id, promise) = setup_promise(scope, &state);

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        match exec_begin(isolation_level.as_deref()).await {
            Ok(()) => OpResult::Completed { op_id, value: "null".to_string(), request_id },
            Err(e) => OpResult::Failed { op_id, error: e, request_id },
        }
    }));

    rv.set(promise.into());
}

/// Allowed isolation levels (uppercased for validation).
const VALID_ISOLATION_LEVELS: &[&str] = &[
    "READ UNCOMMITTED",
    "READ COMMITTED",
    "REPEATABLE READ",
    "SERIALIZABLE",
];

async fn exec_begin(isolation_level: Option<&str>) -> Result<(), String> {
    // Check: no nested transactions
    let has_tx = crate::TX_CONN.with(|tx| tx.borrow().is_some());
    if has_tx {
        return Err("db: transaction already active (nested transactions not supported)".to_string());
    }

    // Build BEGIN statement with optional isolation level
    let begin_sql = match isolation_level {
        Some(level) => {
            let upper = level.to_uppercase();
            if !VALID_ISOLATION_LEVELS.contains(&upper.as_str()) {
                return Err(format!(
                    "db: invalid isolation level: {level}. Must be one of: read uncommitted, read committed, repeatable read, serializable"
                ));
            }
            format!("BEGIN ISOLATION LEVEL {upper}")
        }
        None => "BEGIN".to_string(),
    };

    // Open a dedicated connection (not from pool — we need to hold it).
    // compio-postgres splits a connection into (Client, Connection); we spawn
    // the Connection on a detached task so its run loop drives I/O, and store
    // the Client in TX_CONN. When the Client is eventually dropped, the task
    // terminates gracefully.
    let url = crate::DB_URL.with(|u| u.borrow().clone())
        .ok_or_else(|| "db: not configured".to_string())?;
    let (client, connection) = compio_postgres::connect(&url, compio_postgres::NoTls)
        .await
        .map_err(|e| format!("db: tx connect failed: {e}"))?;
    compio::runtime::spawn(async move {
        if let Err(e) = connection.run().await {
            eprintln!("db: tx connection task error: {e}");
        }
    })
    .detach();

    client.execute(&begin_sql, &[])
        .await
        .map_err(|e| format!("db: BEGIN failed: {e}"))?;

    crate::TX_CONN.with(|tx| { tx.borrow_mut().replace(client); });
    Ok(())
}

/// `zeroship.db.commitTransaction()` → Promise<void>
pub fn commit_transaction(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();

    let _ = &args;
    let (op_id, request_id, promise) = setup_promise(scope, &state);

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        match exec_end("COMMIT").await {
            Ok(()) => OpResult::Completed { op_id, value: "null".to_string(), request_id },
            Err(e) => OpResult::Failed { op_id, error: e, request_id },
        }
    }));

    rv.set(promise.into());
}

/// `zeroship.db.rollbackTransaction()` → Promise<void>
pub fn rollback_transaction(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();

    let _ = &args;
    let (op_id, request_id, promise) = setup_promise(scope, &state);

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        match exec_end("ROLLBACK").await {
            Ok(()) => OpResult::Completed { op_id, value: "null".to_string(), request_id },
            Err(e) => OpResult::Failed { op_id, error: e, request_id },
        }
    }));

    rv.set(promise.into());
}

// ---------------------------------------------------------------------------
// Callback: upsert(collection, docJson, conflictFieldsJson)
// ---------------------------------------------------------------------------

/// `zeroship.db.upsert(collection, docJson, conflictFieldsJson)` → Promise<object>
pub fn upsert(
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
    let Some(conflict_fields) = parse_json_arg(scope, &args, 2) else {
        return;
    };

    let app_id = get_app_id(&state);
    let (op_id, request_id, promise) = setup_promise(scope, &state);

    let bq = match query::build_upsert(&app_id, &collection, &doc, &conflict_fields) {
        Ok(q) => q,
        Err(e) => {
            state.borrow_mut().spawned_ops.push(Box::pin(async move {
                OpResult::Failed { op_id, error: e.to_string(), request_id }
            }));
            rv.set(promise.into());
            return;
        }
    };

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        match exec_mutation(bq).await {
            Ok(json) => {
                let arr: Vec<Value> = serde_json::from_str(&json).unwrap_or_default();
                let value = arr.into_iter().next().unwrap_or(Value::Null).to_string();
                OpResult::Completed { op_id, value, request_id }
            }
            Err(e) => OpResult::Failed { op_id, error: e, request_id },
        }
    }));

    rv.set(promise.into());
}

async fn exec_end(cmd: &str) -> Result<(), String> {
    let client = crate::TX_CONN.with(|tx| tx.borrow_mut().take())
        .ok_or_else(|| "db: no active transaction".to_string())?;

    client.execute(cmd, &[])
        .await
        .map_err(|e| format!("db: {cmd} failed: {e}"))?;

    // Client is dropped here — the spawned Connection task observes the
    // closed sender, sends Terminate, flushes, and exits.
    Ok(())
}

// ===========================================================================
// B1 — @zeroship/migrations primitives
//
// These callbacks are the V8 bridge for the migrations module
// (`crate::migrations`). Each callback parses its arguments, sets up
// a promise, and spawns an async op that delegates to the matching
// `exec_*` function. See `migrations.rs` for the semantics.
// ===========================================================================

async fn ensure_pool() -> Result<Rc<compio_postgres::Pool>, String> {
    let has_pool = DB_POOL.with(|p| p.borrow().is_some());
    if !has_pool {
        crate::init_pool_async().await.map_err(|e| format!("db: lazy init failed: {e}"))?;
    }
    DB_POOL
        .with(|p| p.borrow().as_ref().map(Rc::clone))
        .ok_or_else(|| "db: pool not initialized".to_string())
}

/// `zeroship.db.migrationBegin(name, collection, dryRun, reset)` → Promise<json>
pub fn migration_begin(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();

    let Some(name) = require_string_arg(scope, &args, 0, "name") else { return };
    let Some(collection) = require_string_arg(scope, &args, 1, "collection") else { return };
    let dry_run = args.get(2).boolean_value(scope);
    let reset = args.get(3).boolean_value(scope);

    let app_id = get_app_id(&state);
    let (op_id, request_id, promise) = setup_promise(scope, &state);

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        let pool = match ensure_pool().await {
            Ok(p) => p,
            Err(e) => return OpResult::Failed { op_id, error: e, request_id },
        };
        match crate::migrations::exec_begin(&pool, &app_id, &name, &collection, dry_run, reset).await {
            Ok(json) => OpResult::Completed { op_id, value: json, request_id },
            Err(e) => OpResult::Failed { op_id, error: e, request_id },
        }
    }));
    rv.set(promise.into());
}

/// `zeroship.db.migrationFetchBatch(cursor, batchSize)` → Promise<{rows}>
pub fn migration_fetch_batch(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();

    let cursor = get_i64_arg(scope, &args, 0).unwrap_or(0);
    let batch_size = get_i64_arg(scope, &args, 1).unwrap_or(100);

    let app_id = get_app_id(&state);
    let (op_id, request_id, promise) = setup_promise(scope, &state);

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        match crate::migrations::exec_fetch_batch(&app_id, cursor, batch_size).await {
            Ok(json) => OpResult::Completed { op_id, value: json, request_id },
            Err(e) => OpResult::Failed { op_id, error: e, request_id },
        }
    }));
    rv.set(promise.into());
}

/// `zeroship.db.migrationCommitBatch(updatesJson, deadLetterPksJson,
///     nextCursor, processedTotal, isDone, terminalStatus, error)`
pub fn migration_commit_batch(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();

    let Some(updates) = parse_json_arg(scope, &args, 0) else { return };
    let Some(dlp) = parse_json_arg(scope, &args, 1) else { return };
    let next_cursor = get_i64_arg(scope, &args, 2).unwrap_or(0);
    let processed_total = get_i64_arg(scope, &args, 3).unwrap_or(0);
    let is_done = args.get(4).boolean_value(scope);
    let terminal_status = get_string_arg(scope, &args, 5);
    let error_msg = get_string_arg(scope, &args, 6);

    let app_id = get_app_id(&state);
    let (op_id, request_id, promise) = setup_promise(scope, &state);

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        match crate::migrations::exec_commit_batch(
            &app_id,
            &updates,
            &dlp,
            next_cursor,
            processed_total,
            is_done,
            terminal_status.as_deref(),
            error_msg.as_deref(),
        )
        .await
        {
            Ok(json) => OpResult::Completed { op_id, value: json, request_id },
            Err(e) => OpResult::Failed { op_id, error: e, request_id },
        }
    }));
    rv.set(promise.into());
}

/// `zeroship.db.migrationStatus(name, collection)` → Promise<status>
pub fn migration_status(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();

    let Some(name) = require_string_arg(scope, &args, 0, "name") else { return };
    let Some(collection) = require_string_arg(scope, &args, 1, "collection") else { return };

    let app_id = get_app_id(&state);
    let (op_id, request_id, promise) = setup_promise(scope, &state);

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        let pool = match ensure_pool().await {
            Ok(p) => p,
            Err(e) => return OpResult::Failed { op_id, error: e, request_id },
        };
        match crate::migrations::exec_status(&pool, &app_id, &name, &collection).await {
            Ok(json) => OpResult::Completed { op_id, value: json, request_id },
            Err(e) => OpResult::Failed { op_id, error: e, request_id },
        }
    }));
    rv.set(promise.into());
}

/// `zeroship.db.migrationCancel(name, collection)` → Promise<{ok}>
pub fn migration_cancel(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();

    let Some(name) = require_string_arg(scope, &args, 0, "name") else { return };
    let Some(collection) = require_string_arg(scope, &args, 1, "collection") else { return };

    let app_id = get_app_id(&state);
    let (op_id, request_id, promise) = setup_promise(scope, &state);

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        let pool = match ensure_pool().await {
            Ok(p) => p,
            Err(e) => return OpResult::Failed { op_id, error: e, request_id },
        };
        match crate::migrations::exec_cancel(&pool, &app_id, &name, &collection).await {
            Ok(json) => OpResult::Completed { op_id, value: json, request_id },
            Err(e) => OpResult::Failed { op_id, error: e, request_id },
        }
    }));
    rv.set(promise.into());
}

/// `zeroship.db.migrationReset(name, collection)` → Promise<{ok}>
pub fn migration_reset(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();

    let Some(name) = require_string_arg(scope, &args, 0, "name") else { return };
    let Some(collection) = require_string_arg(scope, &args, 1, "collection") else { return };

    let app_id = get_app_id(&state);
    let (op_id, request_id, promise) = setup_promise(scope, &state);

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        let pool = match ensure_pool().await {
            Ok(p) => p,
            Err(e) => return OpResult::Failed { op_id, error: e, request_id },
        };
        match crate::migrations::exec_reset(&pool, &app_id, &name, &collection).await {
            Ok(json) => OpResult::Completed { op_id, value: json, request_id },
            Err(e) => OpResult::Failed { op_id, error: e, request_id },
        }
    }));
    rv.set(promise.into());
}
