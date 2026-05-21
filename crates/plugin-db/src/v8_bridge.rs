//! V8 ↔ Rust marshaling layer for `zeroship.db.*` callbacks.
//!
//! This module is the seam between V8 and the rest of the plugin —
//! nothing here knows about SQL or schema. Callers above (`crud`,
//! `orchestrator::*`, `replication_ops`) parse args via these helpers,
//! mint promises, and hand the work to the async layer.
//!
//! Contents:
//!
//! - **Promise plumbing**: [`setup_promise`] for the `OpResult::Completed`
//!   path (string-typed value), [`setup_js_promise`] for the
//!   `OpResult::JsValue` path (real JS values via `ResolveValue::Json` /
//!   `ResolveValue::F64` / `ResolveValue::JsGlobal`).
//! - **Argument decoders**: [`get_string_arg`], [`get_i64_arg`],
//!   [`read_json_arg`], [`v8_value_to_serde_json`] (the hot-path walker
//!   that avoids a `JSON.stringify` round-trip).
//! - **State accessors**: [`runtime_state`] (read the `SharedState` off
//!   the isolate slot), [`get_app_id_pub`] (read APP_ID out of env_vars).
//! - **Capability gate**: [`refuse_if_query_capability`] — the B3 gate
//!   that rejects writes from inside a `query()` handler.
//! - **Row decoding**: [`row_to_json`], [`column_to_json`],
//!   [`rows_to_json`] — Postgres OID → JSON conversion, used by every
//!   exec path. [`fmt_db_err`] walks the source chain so DbError
//!   messages reach JS instead of bare wrapper kinds; this is a thin
//!   shim over [`crate::error::DbError::from_pg`] that returns the
//!   flattened message string for callers still on the `Result<_,
//!   String>` rail.

use serde_json::Value;
use zeroship_runtime::state::SharedState;

// ---------------------------------------------------------------------------
// Argument decoders
// ---------------------------------------------------------------------------

/// Extract a string argument from V8, returning None if undefined/null.
pub(crate) fn get_string_arg(
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

/// Parse an optional integer argument.
pub(crate) fn get_i64_arg(
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

// ---------------------------------------------------------------------------
// State accessors
// ---------------------------------------------------------------------------

/// Get the app_id from RuntimeState env_vars.
fn get_app_id(state: &SharedState) -> String {
    state
        .borrow()
        .env_vars
        .get("APP_ID")
        .cloned()
        .unwrap_or_else(|| "default".to_string())
}

/// Public wrapper around [`get_app_id`] for sibling modules
/// (`v8_classes::migration`) that need the same APP_ID convention.
pub(crate) fn get_app_id_pub(state: &SharedState) -> String {
    get_app_id(state)
}

/// Get the runtime state slot off the isolate. Shared by every callback
/// + dispatch helper; consolidated here to avoid copy-pasting the
/// expect.
pub(crate) fn runtime_state(scope: &mut v8::PinScope<'_, '_>) -> SharedState {
    scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone()
}

// ---------------------------------------------------------------------------
// Capability gate
// ---------------------------------------------------------------------------

/// B3 capability gate. Returns `true` if the caller refused the write
/// because the active procedure kind is `query()` — in which case the
/// callback has already set a rejected promise on `rv` and the caller
/// must return immediately.
///
/// Refuse a write op from inside a `query()` handler. Returns
/// `Some(rejected_promise)` to the caller (which returns it as the JS
/// value); `None` when the write is allowed.
///
/// The error envelope matches the `capability_violation` shape
/// (`code`, `wrapper`, `violated`, `remediation`) so the dispatch
/// path can render a structured 500 instead of a generic exception.
pub(crate) fn refuse_if_query_capability<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    op: &str,
) -> Option<v8::Local<'s, v8::Promise>> {
    if !matches!(
        zeroship_runtime::rpc::current_kind(),
        Some(zeroship_runtime::rpc::ProcedureKind::Query)
    ) {
        return None;
    }
    let resolver = v8::PromiseResolver::new(scope).unwrap();
    let promise = resolver.get_promise(scope);
    let exc = zeroship_runtime::rpc::build_capability_violation(
        scope,
        "query",
        op,
        "Use mutation() if you need to write to the database. Queries are read-only.",
    );
    resolver.reject(scope, exc.into());
    Some(promise)
}

// ---------------------------------------------------------------------------
// V8 value walker
// ---------------------------------------------------------------------------

/// Walk a `v8::Local<v8::Value>` directly into a `serde_json::Value`,
/// skipping the JSON.stringify / serde_json::from_str round trip used by
/// [`parse_json_arg`]. Used by the v8_class `Collection` methods on the
/// hot path so we don't pay two parse costs per CRUD call.
///
/// Mirrors the small walker in `runtime/src/rpc/superjson.rs`. We
/// duplicate rather than re-export because plugin-db must not pull in
/// the entire `rpc` subtree (cyclic dep risk).
///
/// Mapping:
/// - `undefined` / `null` → `Value::Null`
/// - boolean → `Value::Bool`
/// - number → `Value::Number` (lossless integer when representable,
///   otherwise f64)
/// - string → `Value::String`
/// - array → `Value::Array` (recurse on each element)
/// - object → `Value::Object` (recurse on each enumerable own property)
/// - anything else (functions, symbols) → `Value::Null`
pub(crate) fn v8_value_to_serde_json(
    scope: &mut v8::PinScope<'_, '_>,
    v: v8::Local<v8::Value>,
) -> Value {
    if v.is_null_or_undefined() {
        return Value::Null;
    }
    if v.is_boolean() {
        return Value::Bool(v.is_true());
    }
    if v.is_number() {
        let n = v.number_value(scope).unwrap_or(0.0);
        if n.fract() == 0.0 && n >= i64::MIN as f64 && n <= i64::MAX as f64 {
            let i = n as i64;
            if (i as f64) == n {
                return Value::Number(serde_json::Number::from(i));
            }
        }
        if let Some(num) = serde_json::Number::from_f64(n) {
            return Value::Number(num);
        }
        return Value::Null;
    }
    if v.is_string() {
        return Value::String(v.to_rust_string_lossy(scope));
    }
    // Date — `JSON.stringify(new Date())` calls `Date.prototype.toJSON`
    // which returns an ISO string. Mirror that here so date fields in
    // filters/docs round-trip the same way they did under the legacy
    // `JSON.stringify` boundary. Without this branch the object walk
    // below sees `new Date()` as a plain object with no own properties
    // and produces `{}` — silently losing the value.
    if v.is_date() {
        if let Ok(obj) = v8::Local::<v8::Object>::try_from(v) {
            let to_iso_key = v8::String::new(scope, "toISOString").unwrap();
            if let Some(fn_v) = obj.get(scope, to_iso_key.into()) {
                if let Ok(to_iso) = v8::Local::<v8::Function>::try_from(fn_v) {
                    if let Some(result) = to_iso.call(scope, v, &[]) {
                        if result.is_string() {
                            return Value::String(result.to_rust_string_lossy(scope));
                        }
                    }
                }
            }
        }
        // Fallback: produce the Unix-ms number (no ISO formatter
        // reachable). The SDK accepts numeric dates anyway.
        if let Ok(date) = v8::Local::<v8::Date>::try_from(v) {
            let ms = date.value_of();
            if ms.is_finite() {
                if let Some(n) = serde_json::Number::from_f64(ms) {
                    return Value::Number(n);
                }
            }
        }
        return Value::Null;
    }
    if v.is_array() {
        let arr: v8::Local<v8::Array> = v.try_into().unwrap();
        let n = arr.length();
        let mut out = Vec::with_capacity(n as usize);
        for i in 0..n {
            let elem = arr
                .get_index(scope, i)
                .unwrap_or_else(|| v8::null(scope).into());
            out.push(v8_value_to_serde_json(scope, elem));
        }
        return Value::Array(out);
    }
    if v.is_object() {
        let obj: v8::Local<v8::Object> = match v.try_into() {
            Ok(o) => o,
            Err(_) => return Value::Null,
        };
        if let Some(names) =
            obj.get_own_property_names(scope, v8::GetPropertyNamesArgs::default())
        {
            let mut map = serde_json::Map::new();
            for i in 0..names.length() {
                let key_v = match names.get_index(scope, i) {
                    Some(k) => k,
                    None => continue,
                };
                let key = key_v.to_rust_string_lossy(scope);
                let val_v = match obj.get(scope, key_v) {
                    Some(v) => v,
                    None => continue,
                };
                map.insert(key, v8_value_to_serde_json(scope, val_v));
            }
            return Value::Object(map);
        }
    }
    Value::Null
}

/// Read a CRUD method's object/array argument directly from V8 into a
/// `serde_json::Value`. `undefined`/missing → empty object (matches
/// [`parse_json_arg`]'s default).
pub(crate) fn read_json_arg(
    scope: &mut v8::PinScope<'_, '_>,
    v: Option<v8::Local<v8::Value>>,
) -> Value {
    match v {
        Some(val) if !val.is_null_or_undefined() => v8_value_to_serde_json(scope, val),
        _ => Value::Object(serde_json::Map::new()),
    }
}

// ---------------------------------------------------------------------------
// Promise plumbing
// ---------------------------------------------------------------------------

/// Create a promise, allocate an op_id, store the resolver, and return
/// (op_id, request_id, promise).
pub(crate) fn setup_promise<'s>(
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

/// Create a promise for the `OpResult::JsValue` resolution path —
/// returns `(resolver_global, request_id, promise)`. Use when the
/// dispatch helper resolves with a real JS value (number, object,
/// `null`, `undefined`) rather than a JSON string.
pub(crate) fn setup_js_promise<'s>(
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

// ---------------------------------------------------------------------------
// Postgres row decoding
// ---------------------------------------------------------------------------

/// Format a `compio_postgres::Error` as a flat string with its full
/// source chain — surfaces the underlying Postgres `DbError` message
/// instead of the bare wrapper kinds ("db error", "unexpected message
/// from server").
///
/// Equivalent to `crate::error::DbError::from_pg(e).into_string()` —
/// retained as the legacy entry point for callers still on the
/// `Result<_, String>` rail. New code should prefer
/// [`crate::error::DbError::from_pg`] directly so the SQLSTATE
/// classification reaches the V8 boundary intact.
pub(crate) fn fmt_db_err(e: &compio_postgres::Error) -> String {
    crate::error::DbError::from_pg(e).into_string()
}

/// Convert rows to a JSON array string.
pub(crate) fn rows_to_json(rows: &[compio_postgres::Row]) -> String {
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
pub(crate) fn row_to_json(row: &compio_postgres::Row) -> Value {
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
        // NUMERIC = 1700 — Postgres' arbitrary-precision decimal. Map to
        // a JSON number when it fits exactly; fall back to string (lossy
        // float would corrupt big decimals). The SDK's `t.number()` maps
        // to NUMERIC, so user-facing `doc.field` should be a number, not
        // a string. Postgres serialises NUMERIC over the text protocol
        // as a decimal string; parse it.
        1700 => match row.try_get::<_, String>(name) {
            Ok(s) => {
                if let Ok(i) = s.parse::<i64>() {
                    Value::Number(serde_json::Number::from(i))
                } else if let Ok(f) = s.parse::<f64>() {
                    serde_json::Number::from_f64(f)
                        .map_or(Value::String(s), Value::Number)
                } else {
                    Value::String(s)
                }
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
