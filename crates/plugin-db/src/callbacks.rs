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

use zeroship_runtime::state::{OpError, OpResult, ResolveValue, SharedState};
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

/// Parse an optional integer argument.
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

/// Public wrapper around [`get_app_id`] for sibling modules
/// (`v8_classes::migration`) that need the same APP_ID convention.
pub(crate) fn get_app_id_pub(state: &SharedState) -> String {
    get_app_id(state)
}

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

/// Get the runtime state slot off the isolate. Shared by every callback
/// + dispatch helper; consolidated here to avoid copy-pasting the
/// expect.
pub(crate) fn runtime_state(scope: &mut v8::PinScope<'_, '_>) -> SharedState {
    scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone()
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
///
/// Returns the raw integer; callers wrap into the appropriate
/// `OpResult` shape (typically `ResolveValue::F64` so JS sees a real
/// `number`).
async fn exec_count(bq: BuiltQuery) -> Result<i64, String> {
    let param_refs: Vec<&str> = bq.params.iter().map(String::as_str).collect();
    let rows = run_sql(&bq.sql, &param_refs).await
        .map_err(|e| format!("db query error: {e}"))?;

    Ok(rows
        .first()
        .map(|r| r.get::<_, i64>("count"))
        .unwrap_or(0))
}

/// Execute an insert/update/delete query, returning the affected rows.
async fn exec_mutation(bq: BuiltQuery) -> Result<String, String> {
    let param_refs: Vec<&str> = bq.params.iter().map(String::as_str).collect();
    let rows = run_sql(&bq.sql, &param_refs).await
        .map_err(|e| format!("db mutation error: {e}"))?;

    Ok(rows_to_json(&rows))
}

/// Execute a mutation, then emit a [`crate::wal_consumer::emit_local`]
/// event into the in-process broker on success.
///
/// This is the P8a coarse-grained reactive-query bridge: every
/// successful INSERT/UPDATE/DELETE produces one or more events on
/// `(app_id, collection)` that wake any matching subscribers in the
/// same isolate.
///
/// On error the broker is untouched — partial writes produce no
/// events. The error message is forwarded verbatim.
///
/// `op` selects the [`crate::broker::ChangeOp`] tagged on the event;
/// the caller knows whether it called `build_insert`, `build_update_one`,
/// `build_delete_one`, etc. so we don't try to infer it from the SQL.
///
/// Future read-set narrowing (P8b) extends this helper to populate
/// `changed_columns` from the SET clause and `pk` from the RETURNING
/// row. For P8a we collect what's already in the result JSON.
async fn exec_mutation_with_emit(
    bq: BuiltQuery,
    app_id: &str,
    collection: &str,
    op: crate::broker::ChangeOp,
) -> Result<String, String> {
    let json = exec_mutation(bq).await?;
    // Parse the returned JSON to derive (pk per row, count of rows).
    // The query builders all use RETURNING * (insert/update) or
    // RETURNING id (delete) — see `query::build_*`. We pull `id` as
    // i64 when present and treat the missing case as a non-affecting
    // mutation (publish a single event with pk=None so subscribers
    // can still re-fetch).
    let rows: Vec<Value> = serde_json::from_str(&json).unwrap_or_default();
    if rows.is_empty() {
        // No rows affected — no broker event. UPDATE with a non-
        // matching filter falls here; subscribers should not see a
        // spurious change.
        return Ok(json);
    }
    for row in &rows {
        let pk = row
            .get("id")
            .and_then(|v| v.as_i64())
            .or_else(|| row.get("_id").and_then(|v| v.as_i64()));
        // changed_columns: the keys present in the returned row,
        // minus the system columns we never want to report. For
        // INSERT this is "every declared column" — for UPDATE it's
        // the post-image, which is a superset of what changed.
        // Filtering down to "what changed" requires a before/after
        // diff that we don't have here; P8b will compute it from the
        // mutation's SET clause directly.
        let (columns, tuple): (Vec<String>, std::collections::HashMap<String, String>) = match row {
            Value::Object(m) => {
                let cols = m
                    .keys()
                    .filter(|k| !matches!(k.as_str(), "created_at" | "updated_at"))
                    .cloned()
                    .collect();
                // P8b: render the full RETURNING row into a
                // `column → text` map for the broker's predicate
                // evaluation. Numbers / bools are stringified to
                // match the WAL-consumer path's text encoding so the
                // predicate-eval rules collapse to a single
                // comparison code path.
                let tuple = m
                    .iter()
                    .map(|(k, v)| {
                        let s = match v {
                            Value::String(s) => s.clone(),
                            Value::Null => "NULL".to_string(),
                            other => other.to_string(),
                        };
                        (k.clone(), s)
                    })
                    .collect();
                (cols, tuple)
            }
            _ => (Vec::new(), std::collections::HashMap::new()),
        };
        queue_or_emit(app_id, collection, op, pk, columns, tuple);
    }
    Ok(json)
}

/// If a transaction is active on this thread, queue the event in
/// [`crate::PENDING_EMITS`] for the settle path to drain on COMMIT.
/// Otherwise (autocommit), fire it immediately. Closes Gap B —
/// subscribers no longer observe pre-commit state.
fn queue_or_emit(
    app_id: &str,
    collection: &str,
    op: crate::broker::ChangeOp,
    pk: Option<i64>,
    changed_columns: Vec<String>,
    new_tuple: std::collections::HashMap<String, String>,
) {
    let in_tx = crate::TX_CONN.with(|tx| tx.borrow().is_some());
    if !in_tx {
        crate::wal_consumer::emit_local(app_id, collection, op, pk, changed_columns, new_tuple);
        return;
    }
    let ev = crate::broker::ChangeEvent {
        app_id: app_id.to_string(),
        collection: collection.to_string(),
        op,
        pk,
        changed_columns,
        new_tuple,
        old_tuple: None,
    };
    crate::PENDING_EMITS.with(|p| {
        let mut slot = p.borrow_mut();
        slot.get_or_insert_with(Vec::new).push(ev);
    });
}

/// Drain [`crate::PENDING_EMITS`] and fire every queued event through
/// the broker. Called by the transaction settle path on COMMIT.
pub(crate) fn drain_pending_emits_on_commit() {
    let queued: Vec<crate::broker::ChangeEvent> = crate::PENDING_EMITS
        .with(|p| p.borrow_mut().take().unwrap_or_default());
    for ev in queued {
        crate::wal_consumer::emit_local(
            &ev.app_id,
            &ev.collection,
            ev.op,
            ev.pk,
            ev.changed_columns,
            ev.new_tuple,
        );
    }
}

/// Clear [`crate::PENDING_EMITS`] without firing any events. Called by
/// the transaction settle path on ROLLBACK (and by `exec_begin` to
/// drop any stale residue from an interrupted prior run).
pub(crate) fn clear_pending_emits() {
    crate::PENDING_EMITS.with(|p| *p.borrow_mut() = None);
}

/// **Test-only**: end-to-end wrapper around [`exec_mutation_with_emit`]
/// so integration tests can drive the queue/drain machinery against a
/// real Postgres connection without spinning up a V8 isolate.
///
/// The caller is responsible for setting `TX_CONN` (via
/// [`crate::install_tx_marker_for_tests`]) when the test wants the
/// queueing path to fire.
#[doc(hidden)]
pub async fn exec_mutation_with_emit_for_tests(
    bq: crate::query::BuiltQuery,
    app_id: &str,
    collection: &str,
    op: crate::broker::ChangeOp,
) -> Result<String, String> {
    exec_mutation_with_emit(bq, app_id, collection, op).await
}

/// Convert rows to a JSON array string.
fn rows_to_json(rows: &[compio_postgres::Row]) -> String {
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

// ---------------------------------------------------------------------------
// Shared `findOne` dispatch
// ---------------------------------------------------------------------------

/// Shared dispatch for `findOne`, called by `Collection::find_one`
/// (the `#[v8_method]`). Filter arrives already decoded into
/// `serde_json::Value` via [`v8_value_to_serde_json`] — no JSON
/// round-trip on the hot path.
///
/// Resolves with the row as a real JS object or real JS `null` if no
/// row matched (via `ResolveValue::Json`); on error rejects with the
/// message.
pub(crate) fn dispatch_find_one<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    app_id: &str,
    collection: &str,
    filter: Value,
    opts: Value,
) -> v8::Local<'s, v8::Promise> {
    let state = runtime_state(scope);
    // P8b — record into the active query's read-set so the broker can
    // narrow events to this filter. No-op outside `query()` handlers.
    crate::read_set::record_if_active(collection, &filter);

    let order_by = opts.get("orderBy");
    let select = opts.get("select");

    let (resolver, request_id, promise) = setup_js_promise(scope, &state);
    let bq = match query::build_find(app_id, collection, &filter, Some(1), None, order_by, select) {
        Ok(q) => q,
        Err(e) => {
            state.borrow_mut().spawned_ops.push(Box::pin(async move {
                OpResult::JsValue {
                    resolver,
                    value: ResolveValue::RejectError(OpError::error(e.to_string())),
                    request_id,
                }
            }));
            return promise;
        }
    };

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        match exec_query(bq).await {
            Ok(json) => {
                let arr: Vec<Value> = serde_json::from_str(&json).unwrap_or_default();
                let value = arr.into_iter().next().unwrap_or(Value::Null).to_string();
                OpResult::JsValue {
                    resolver,
                    value: ResolveValue::Json(value),
                    request_id,
                }
            }
            Err(e) => OpResult::JsValue {
                resolver,
                value: ResolveValue::RejectError(OpError::error(e)),
                request_id,
            },
        }
    }));

    promise
}


// ---------------------------------------------------------------------------
// Callback: find(collection, filterJson, optsJson)
// ---------------------------------------------------------------------------

/// Shared dispatch for `find` — see [`dispatch_find_one`] for the
/// rationale. Reads `limit`/`offset`/`orderBy`/`select` out of `opts`.
pub(crate) fn dispatch_find<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    app_id: &str,
    collection: &str,
    filter: Value,
    opts: Value,
) -> v8::Local<'s, v8::Promise> {
    let state = runtime_state(scope);
    // P8b — record into the active query's read-set so the broker can
    // narrow events to this filter. No-op outside `query()` handlers.
    crate::read_set::record_if_active(collection, &filter);

    let limit = opts.get("limit").and_then(Value::as_i64);
    let offset = opts.get("offset").and_then(Value::as_i64);
    let order_by = opts.get("orderBy");
    let select = opts.get("select");
    let (resolver, request_id, promise) = setup_js_promise(scope, &state);

    let bq = match query::build_find(app_id, collection, &filter, limit, offset, order_by, select) {
        Ok(q) => q,
        Err(e) => {
            state.borrow_mut().spawned_ops.push(Box::pin(async move {
                OpResult::JsValue {
                    resolver,
                    value: ResolveValue::RejectError(OpError::error(e.to_string())),
                    request_id,
                }
            }));
            return promise;
        }
    };

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        match exec_query(bq).await {
            Ok(value) => OpResult::JsValue {
                resolver,
                value: ResolveValue::Json(value),
                request_id,
            },
            Err(e) => OpResult::JsValue {
                resolver,
                value: ResolveValue::RejectError(OpError::error(e)),
                request_id,
            },
        }
    }));

    promise
}


// ---------------------------------------------------------------------------
// Callback: insert(collection, docJson)
// ---------------------------------------------------------------------------

/// Shared dispatch for `insert`. The capability gate is the caller's
/// responsibility — `Collection::insert` calls
/// [`refuse_if_query_capability`] before reaching here.
pub(crate) fn dispatch_insert<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    app_id: &str,
    collection: &str,
    doc: Value,
) -> v8::Local<'s, v8::Promise> {
    let state = runtime_state(scope);
    let (resolver, request_id, promise) = setup_js_promise(scope, &state);

    let bq = match query::build_insert(app_id, collection, &doc) {
        Ok(q) => q,
        Err(e) => {
            state.borrow_mut().spawned_ops.push(Box::pin(async move {
                OpResult::JsValue {
                    resolver,
                    value: ResolveValue::RejectError(OpError::error(e.to_string())),
                    request_id,
                }
            }));
            return promise;
        }
    };

    let coll_for_emit = collection.to_string();
    let app_for_emit = app_id.to_string();
    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        match exec_mutation_with_emit(
            bq,
            &app_for_emit,
            &coll_for_emit,
            crate::broker::ChangeOp::Insert,
        )
        .await
        {
            Ok(json) => {
                let arr: Vec<Value> = serde_json::from_str(&json).unwrap_or_default();
                let value = arr.into_iter().next().unwrap_or(Value::Null).to_string();
                OpResult::JsValue {
                    resolver,
                    value: ResolveValue::Json(value),
                    request_id,
                }
            }
            Err(e) => OpResult::JsValue {
                resolver,
                value: ResolveValue::RejectError(OpError::error(e)),
                request_id,
            },
        }
    }));

    promise
}


// ---------------------------------------------------------------------------
// Callback: updateOne(collection, filterJson, updateJson)
// ---------------------------------------------------------------------------

/// Shared dispatch for `updateOne`. See [`dispatch_insert`] for the
/// capability-gate contract.
pub(crate) fn dispatch_update_one<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    app_id: &str,
    collection: &str,
    filter: Value,
    update: Value,
) -> v8::Local<'s, v8::Promise> {
    let state = runtime_state(scope);
    let (resolver, request_id, promise) = setup_js_promise(scope, &state);

    let bq = match query::build_update_one(app_id, collection, &filter, &update) {
        Ok(q) => q,
        Err(e) => {
            state.borrow_mut().spawned_ops.push(Box::pin(async move {
                OpResult::JsValue {
                    resolver,
                    value: ResolveValue::RejectError(OpError::error(e.to_string())),
                    request_id,
                }
            }));
            return promise;
        }
    };

    let coll_for_emit = collection.to_string();
    let app_for_emit = app_id.to_string();
    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        match exec_mutation_with_emit(
            bq,
            &app_for_emit,
            &coll_for_emit,
            crate::broker::ChangeOp::Update,
        )
        .await
        {
            Ok(json) => {
                let arr: Vec<Value> = serde_json::from_str(&json).unwrap_or_default();
                let value = arr.into_iter().next().unwrap_or(Value::Null).to_string();
                OpResult::JsValue {
                    resolver,
                    value: ResolveValue::Json(value),
                    request_id,
                }
            }
            Err(e) => OpResult::JsValue {
                resolver,
                value: ResolveValue::RejectError(OpError::error(e)),
                request_id,
            },
        }
    }));

    promise
}


// ---------------------------------------------------------------------------
// Callback: deleteOne(collection, filterJson)
// ---------------------------------------------------------------------------

/// Shared dispatch for `deleteOne`. See [`dispatch_insert`] for the
/// capability-gate contract.
pub(crate) fn dispatch_delete_one<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    app_id: &str,
    collection: &str,
    filter: Value,
) -> v8::Local<'s, v8::Promise> {
    let state = runtime_state(scope);
    let (resolver, request_id, promise) = setup_js_promise(scope, &state);

    let bq = match query::build_delete_one(app_id, collection, &filter) {
        Ok(q) => q,
        Err(e) => {
            state.borrow_mut().spawned_ops.push(Box::pin(async move {
                OpResult::JsValue {
                    resolver,
                    value: ResolveValue::RejectError(OpError::error(e.to_string())),
                    request_id,
                }
            }));
            return promise;
        }
    };

    let coll_for_emit = collection.to_string();
    let app_for_emit = app_id.to_string();
    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        match exec_mutation_with_emit(
            bq,
            &app_for_emit,
            &coll_for_emit,
            crate::broker::ChangeOp::Delete,
        )
        .await
        {
            Ok(json) => {
                let arr: Vec<Value> = serde_json::from_str(&json).unwrap_or_default();
                let value = arr.into_iter().next().unwrap_or(Value::Null).to_string();
                OpResult::JsValue {
                    resolver,
                    value: ResolveValue::Json(value),
                    request_id,
                }
            }
            Err(e) => OpResult::JsValue {
                resolver,
                value: ResolveValue::RejectError(OpError::error(e)),
                request_id,
            },
        }
    }));

    promise
}


// ---------------------------------------------------------------------------
// Callback: insertMany(collection, docsJson)
// ---------------------------------------------------------------------------

/// Shared dispatch for `insertMany`. See [`dispatch_insert`] for the
/// capability-gate contract.
pub(crate) fn dispatch_insert_many<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    app_id: &str,
    collection: &str,
    docs: Value,
) -> v8::Local<'s, v8::Promise> {
    let state = runtime_state(scope);
    let (resolver, request_id, promise) = setup_js_promise(scope, &state);

    let bq = match query::build_insert_many(app_id, collection, &docs) {
        Ok(q) => q,
        Err(e) => {
            state.borrow_mut().spawned_ops.push(Box::pin(async move {
                OpResult::JsValue {
                    resolver,
                    value: ResolveValue::RejectError(OpError::error(e.to_string())),
                    request_id,
                }
            }));
            return promise;
        }
    };

    let coll_for_emit = collection.to_string();
    let app_for_emit = app_id.to_string();
    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        match exec_mutation_with_emit(
            bq,
            &app_for_emit,
            &coll_for_emit,
            crate::broker::ChangeOp::Insert,
        )
        .await
        {
            Ok(value) => OpResult::JsValue {
                resolver,
                value: ResolveValue::Json(value),
                request_id,
            },
            Err(e) => OpResult::JsValue {
                resolver,
                value: ResolveValue::RejectError(OpError::error(e)),
                request_id,
            },
        }
    }));

    promise
}


// ---------------------------------------------------------------------------
// Callback: aggregate(collection, pipelineJson)
// ---------------------------------------------------------------------------

/// Shared dispatch for `aggregate`. Pipeline is a JSON array of stage
/// objects.
pub(crate) fn dispatch_aggregate<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    app_id: &str,
    collection: &str,
    pipeline: Value,
) -> v8::Local<'s, v8::Promise> {
    let state = runtime_state(scope);

    // P8b — record into the active query's read-set so the broker can
    // narrow events. If the first stage is `$match`, capture its filter;
    // otherwise record a coarse-grained entry (empty filter) — the
    // pipeline depends on the whole collection.
    {
        let captured_filter = pipeline
            .as_array()
            .and_then(|stages| stages.first())
            .and_then(|stage| stage.get("$match"))
            .cloned()
            .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
        crate::read_set::record_if_active(collection, &captured_filter);
    }

    let (resolver, request_id, promise) = setup_js_promise(scope, &state);

    let bq = match query::build_aggregate(app_id, collection, &pipeline) {
        Ok(q) => q,
        Err(e) => {
            state.borrow_mut().spawned_ops.push(Box::pin(async move {
                OpResult::JsValue {
                    resolver,
                    value: ResolveValue::RejectError(OpError::error(e.to_string())),
                    request_id,
                }
            }));
            return promise;
        }
    };

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        match exec_query(bq).await {
            Ok(value) => OpResult::JsValue {
                resolver,
                value: ResolveValue::Json(value),
                request_id,
            },
            Err(e) => OpResult::JsValue {
                resolver,
                value: ResolveValue::RejectError(OpError::error(e)),
                request_id,
            },
        }
    }));

    promise
}


// ---------------------------------------------------------------------------
// Callback: distinct(collection, field, filterJson)
// ---------------------------------------------------------------------------

/// Shared dispatch for `distinct`. `field` is the column name; `filter`
/// is the WHERE-clause JSON.
pub(crate) fn dispatch_distinct<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    app_id: &str,
    collection: &str,
    field: &str,
    filter: Value,
) -> v8::Local<'s, v8::Promise> {
    let state = runtime_state(scope);
    let (resolver, request_id, promise) = setup_js_promise(scope, &state);

    let bq = match query::build_distinct(app_id, collection, field, &filter) {
        Ok(q) => q,
        Err(e) => {
            state.borrow_mut().spawned_ops.push(Box::pin(async move {
                OpResult::JsValue {
                    resolver,
                    value: ResolveValue::RejectError(OpError::error(e.to_string())),
                    request_id,
                }
            }));
            return promise;
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
                OpResult::JsValue {
                    resolver,
                    value: ResolveValue::Json(value),
                    request_id,
                }
            }
            Err(e) => OpResult::JsValue {
                resolver,
                value: ResolveValue::RejectError(OpError::error(e)),
                request_id,
            },
        }
    }));

    promise
}


// ---------------------------------------------------------------------------
// Callback: updateMany(collection, filterJson, updateJson)
// ---------------------------------------------------------------------------

/// Shared dispatch for `updateMany`. Resolves with the count of
/// affected rows as a JS `number`.
pub(crate) fn dispatch_update_many<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    app_id: &str,
    collection: &str,
    filter: Value,
    update: Value,
) -> v8::Local<'s, v8::Promise> {
    let state = runtime_state(scope);
    let (resolver, request_id, promise) = setup_js_promise(scope, &state);

    let bq = match query::build_update_many(app_id, collection, &filter, &update) {
        Ok(q) => q,
        Err(e) => {
            state.borrow_mut().spawned_ops.push(Box::pin(async move {
                OpResult::JsValue {
                    resolver,
                    value: ResolveValue::RejectError(OpError::error(e.to_string())),
                    request_id,
                }
            }));
            return promise;
        }
    };

    let coll_for_emit = collection.to_string();
    let app_for_emit = app_id.to_string();
    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        match exec_mutation_with_emit(
            bq,
            &app_for_emit,
            &coll_for_emit,
            crate::broker::ChangeOp::Update,
        )
        .await
        {
            Ok(json) => {
                let arr: Vec<Value> = serde_json::from_str(&json).unwrap_or_default();
                #[allow(clippy::cast_precision_loss)]
                let n = arr.len() as f64;
                OpResult::JsValue {
                    resolver,
                    value: ResolveValue::F64(n),
                    request_id,
                }
            }
            Err(e) => OpResult::JsValue {
                resolver,
                value: ResolveValue::RejectError(OpError::error(e)),
                request_id,
            },
        }
    }));

    promise
}


// ---------------------------------------------------------------------------
// Callback: deleteMany(collection, filterJson)
// ---------------------------------------------------------------------------

/// Shared dispatch for `deleteMany`. Resolves with the count of
/// affected rows as a JS `number`.
pub(crate) fn dispatch_delete_many<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    app_id: &str,
    collection: &str,
    filter: Value,
) -> v8::Local<'s, v8::Promise> {
    let state = runtime_state(scope);
    let (resolver, request_id, promise) = setup_js_promise(scope, &state);

    let bq = match query::build_delete_many(app_id, collection, &filter) {
        Ok(q) => q,
        Err(e) => {
            state.borrow_mut().spawned_ops.push(Box::pin(async move {
                OpResult::JsValue {
                    resolver,
                    value: ResolveValue::RejectError(OpError::error(e.to_string())),
                    request_id,
                }
            }));
            return promise;
        }
    };

    let coll_for_emit = collection.to_string();
    let app_for_emit = app_id.to_string();
    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        match exec_mutation_with_emit(
            bq,
            &app_for_emit,
            &coll_for_emit,
            crate::broker::ChangeOp::Delete,
        )
        .await
        {
            Ok(json) => {
                let arr: Vec<Value> = serde_json::from_str(&json).unwrap_or_default();
                #[allow(clippy::cast_precision_loss)]
                let n = arr.len() as f64;
                OpResult::JsValue {
                    resolver,
                    value: ResolveValue::F64(n),
                    request_id,
                }
            }
            Err(e) => OpResult::JsValue {
                resolver,
                value: ResolveValue::RejectError(OpError::error(e)),
                request_id,
            },
        }
    }));

    promise
}


// ---------------------------------------------------------------------------
// Callback: count(collection, filterJson)
// ---------------------------------------------------------------------------

/// Shared dispatch for `count`. Resolves with a real JS `number`
/// (not a JSON-stringified integer).
pub(crate) fn dispatch_count<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    app_id: &str,
    collection: &str,
    filter: Value,
) -> v8::Local<'s, v8::Promise> {
    let state = runtime_state(scope);
    // P8b — record into the active query's read-set so the broker can
    // narrow events to this filter. No-op outside `query()` handlers.
    crate::read_set::record_if_active(collection, &filter);

    let (resolver, request_id, promise) = setup_js_promise(scope, &state);

    let bq = match query::build_count(app_id, collection, &filter) {
        Ok(q) => q,
        Err(e) => {
            state.borrow_mut().spawned_ops.push(Box::pin(async move {
                OpResult::JsValue {
                    resolver,
                    value: ResolveValue::RejectError(OpError::error(e.to_string())),
                    request_id,
                }
            }));
            return promise;
        }
    };

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        match exec_count(bq).await {
            Ok(n) => OpResult::JsValue {
                resolver,
                #[allow(clippy::cast_precision_loss)]
                value: ResolveValue::F64(n as f64),
                request_id,
            },
            Err(e) => OpResult::JsValue {
                resolver,
                value: ResolveValue::RejectError(OpError::error(e)),
                request_id,
            },
        }
    }));

    promise
}


// ---------------------------------------------------------------------------
// Callback: registerModel(collection, schemaJson)
// ---------------------------------------------------------------------------

/// `zeroship.db.registerModel(collection, schemaJson)` → Promise<void>
///
/// Creates the table and any missing columns. Idempotent — safe to call
/// on every cold start. Skips DDL if the model was already registered
/// for this app on this thread.
pub fn register_model_dispatch<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    app_id: &str,
    collection: &str,
    schema: Value,
) -> v8::Local<'s, v8::Promise> {
    let state = runtime_state(scope);

    // Fast path: already registered on this thread — skip DDL.
    if crate::is_model_registered(app_id, collection) {
        let resolver = v8::PromiseResolver::new(scope).unwrap();
        let promise = resolver.get_promise(scope);
        let undefined = v8::undefined(scope);
        resolver.resolve(scope, undefined.into());
        return promise;
    }

    let (op_id, request_id, promise) = setup_promise(scope, &state);
    let app_id_owned = app_id.to_string();
    let collection_owned = collection.to_string();

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        match exec_register_model(&app_id_owned, &collection_owned, &schema).await {
            Ok(()) => {
                crate::mark_model_registered(&app_id_owned, &collection_owned);
                OpResult::Completed { op_id, value: "null".to_string(), request_id }
            }
            Err(e) => OpResult::Failed { op_id, error: e, request_id },
        }
    }));

    promise
}

/// Execute DDL for registerModel. Implements the four-phase orchestrator
/// from proposal A2 (`docs/proposals/zeroship-db.md`):
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

    let declared_indexes = query::build_create_indexes(app_id, collection, schema)
        .map_err(|e| format!("db: {e}"))?;

    // -------------------------------------------------------------------
    // Diff phase: introspect pg_catalog, classify changes.
    // -------------------------------------------------------------------
    let mut live = crate::diff::read_live_schema(pool, app_id).await?;
    let rows_estimate = crate::diff::estimate_row_count(pool, app_id, collection).await?;
    live.row_counts.insert(collection.to_string(), rows_estimate);

    // B2 — build CREATE TABLE with Deferred FK emission keyed on the live
    // table set. Refs to tables that already exist inline their FK; refs
    // to tables that don't exist yet skip the inline clause, and the diff
    // engine emits a follow-on `ALTER TABLE … ADD CONSTRAINT` op. This
    // breaks the cross-table cold-start race: concurrent
    // `registerModel("users")` / `registerModel("todos")` calls serialize
    // on the advisory lock; whichever runs second sees the first table in
    // `live` and can inline the FK, or defers it to its own apply phase.
    let existing_tables: std::collections::HashSet<String> =
        live.tables.keys().cloned().collect();
    let create_table = query::build_create_table_with_fks(
        app_id,
        collection,
        schema,
        &query::FkEmission::Deferred(&existing_tables),
    )
    .map_err(|e| format!("db: {e}"))?;

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
    //
    // Split into two passes:
    //   1. Transactional ops (CREATE TABLE / ADD COLUMN / ADD/DROP FK) run
    //      while the advisory lock is held — they serialise per-app.
    //   2. CREATE INDEX CONCURRENTLY ops run AFTER releasing the advisory
    //      lock. CIC takes an internal snapshot and waits for all other
    //      open snapshots on the target table to finish; another
    //      orchestrator blocked on `pg_advisory_lock` holds a snapshot
    //      that CIC waits on → deadlock. CIC is idempotent via
    //      `IF NOT EXISTS` so it's safe to run unlocked.
    // -------------------------------------------------------------------
    let run_op = async |op: &crate::diff::DiffOp| -> Result<(), String> {
        let audit_id = match crate::audit::write_audit_row(
            pool,
            app_id,
            &crate::audit::AuditRow {
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
            },
        )
        .await
        {
            Ok(id) => Some(id),
            Err(e) => {
                tracing::warn!(error = %e, "audit: failed to insert running row");
                None
            }
        };

        let result = match &op.change_kind {
            crate::diff::ChangeKind::CreateTable
            | crate::diff::ChangeKind::AddColumn
            | crate::diff::ChangeKind::AddForeignKey
            | crate::diff::ChangeKind::DropForeignKey => {
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

        result
    };

    // Pass 1: transactional ops under advisory lock.
    for op in &ops {
        if op.class == crate::diff::ChangeClass::Destructive {
            continue;
        }
        if matches!(op.change_kind, crate::diff::ChangeKind::AddIndex) {
            continue;
        }
        run_op(op).await?;
    }

    // Release advisory lock BEFORE CIC. Two orchestrators racing on CIC
    // is safe (IF NOT EXISTS), but holding the lock through CIC
    // deadlocks: a second waiter blocked on pg_advisory_lock pins a
    // snapshot that CIC waits on.
    let unlock_sql =
        "SELECT pg_advisory_unlock(hashtext('zs_reg:' || $1)::int4, hashtext('register_model')::int4)";
    let _ = lock_client.query_text_params(unlock_sql, &[app_id]).await;
    drop(lock_client);

    // Pass 2: CIC ops, unlocked.
    for op in &ops {
        if op.class == crate::diff::ChangeClass::Destructive {
            continue;
        }
        if !matches!(op.change_kind, crate::diff::ChangeKind::AddIndex) {
            continue;
        }
        run_op(op).await?;
    }

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


// ---------------------------------------------------------------------------
// Transaction callbacks: begin / commit / rollback
// ---------------------------------------------------------------------------
//
// V8 is single-threaded per isolate, so only one transaction can be active
// at a time. We store the transaction connection in TX_CONN thread-local.
// All CRUD callbacks (exec_query, exec_mutation, etc.) automatically use
// TX_CONN when it's set, via the run_sql() helper.

/// `zeroship.db.beginTransaction(isolationLevel?)` → Promise<Transaction>
///
/// Opens a dedicated connection, runs BEGIN, stores it in TX_CONN, and
/// resolves with a fresh [`crate::v8_classes::transaction::Transaction`]
/// v8_class instance. The wrapper's Weak finalizer auto-rollbacks if
/// the handle is dropped without `.commit()` / `.rollback()` — closes
/// the connection-leak footgun the pre-wrapper API had.
///
/// All subsequent CRUD ops use the transaction connection until
/// the wrapper's `.commit()` or `.rollback()` runs (or the wrapper's
/// `Drop` finalizer auto-rollbacks on GC).
///
/// The Transaction wrapper is minted *synchronously* before the BEGIN
/// future runs (we need a V8 scope to allocate it). On BEGIN success
/// the future stamps the wrapper's pre-allocated `token` onto
/// [`crate::TX_TOKEN`] and resolves the promise with the wrapper; on
/// failure the wrapper is left with a token that never matches
/// TX_TOKEN, so its `Drop` is a no-op when V8 eventually collects it.
pub fn begin_transaction_dispatch<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    isolation_level: Option<String>,
    app_id: String,
) -> v8::Local<'s, v8::Promise> {
    let state = runtime_state(scope);

    // Allocate the ownership token + mint the wrapper synchronously.
    // We need a scope to allocate the V8 object; the spawned future
    // doesn't have one. On BEGIN success the future stamps TX_TOKEN
    // with this same token; on failure TX_TOKEN stays 0 so the
    // wrapper's Drop sees `current(0) != token` and no-ops.
    let token = crate::next_tx_token();
    let tx_obj = match crate::v8_classes::transaction::mint_transaction(scope, token, app_id) {
        Ok(obj) => obj,
        Err(e) => {
            let resolver = v8::PromiseResolver::new(scope).unwrap();
            let promise = resolver.get_promise(scope);
            let msg = v8::String::new(scope, &e.message).unwrap();
            let exc = v8::Exception::error(scope, msg);
            resolver.reject(scope, exc);
            return promise;
        }
    };

    let tx_obj_as_value: v8::Local<v8::Value> = tx_obj.into();
    let tx_global: v8::Global<v8::Value> = v8::Global::new(scope, tx_obj_as_value);

    let resolver = v8::PromiseResolver::new(scope).unwrap();
    let promise = resolver.get_promise(scope);
    let resolver_global = v8::Global::new(scope, resolver);
    let request_id = state.borrow().executing_request_id;

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        match exec_begin(isolation_level.as_deref()).await {
            Ok(()) => {
                // Stamp ownership now that TX_CONN holds the client —
                // the wrapper's commit / rollback / Drop all gate on
                // this matching the wrapper's `token`.
                crate::TX_TOKEN.with(|t| t.set(token));
                OpResult::JsValue {
                    resolver: resolver_global,
                    value: ResolveValue::JsGlobal(tx_global),
                    request_id,
                }
            }
            Err(e) => {
                // Wrapper's `token` never matches TX_TOKEN(=0), so when
                // V8 collects the wrapper (no JS reference survives a
                // rejected await) the Drop is a no-op.
                drop(tx_global);
                OpResult::JsValue {
                    resolver: resolver_global,
                    value: ResolveValue::RejectError(OpError::error(e)),
                    request_id,
                }
            }
        }
    }));

    promise
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
    // Defensive: any residue from a prior tx that didn't drain cleanly
    // (shouldn't happen — every settle path clears) must NOT leak into
    // the new tx's drain. Drop without firing.
    clear_pending_emits();
    Ok(())
}



// ---------------------------------------------------------------------------
// Callback: upsert(collection, docJson, conflictFieldsJson)
// ---------------------------------------------------------------------------

/// Shared dispatch for `upsert`. See [`dispatch_insert`] for the
/// capability-gate contract. `conflict_fields` is the JSON array of
/// column names that form the ON CONFLICT target.
pub(crate) fn dispatch_upsert<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    app_id: &str,
    collection: &str,
    doc: Value,
    conflict_fields: Value,
) -> v8::Local<'s, v8::Promise> {
    let state = runtime_state(scope);
    let (resolver, request_id, promise) = setup_js_promise(scope, &state);

    let bq = match query::build_upsert(app_id, collection, &doc, &conflict_fields) {
        Ok(q) => q,
        Err(e) => {
            state.borrow_mut().spawned_ops.push(Box::pin(async move {
                OpResult::JsValue {
                    resolver,
                    value: ResolveValue::RejectError(OpError::error(e.to_string())),
                    request_id,
                }
            }));
            return promise;
        }
    };

    let coll_for_emit = collection.to_string();
    let app_for_emit = app_id.to_string();
    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        // Upsert can be either INSERT (new row) or UPDATE (existing).
        // We tag as Update because the subscriber's reaction is the
        // same — re-fetch. The proposal's read-set narrowing (P8b)
        // will distinguish; P8a doesn't need to.
        match exec_mutation_with_emit(
            bq,
            &app_for_emit,
            &coll_for_emit,
            crate::broker::ChangeOp::Update,
        )
        .await
        {
            Ok(json) => {
                let arr: Vec<Value> = serde_json::from_str(&json).unwrap_or_default();
                let value = arr.into_iter().next().unwrap_or(Value::Null).to_string();
                OpResult::JsValue {
                    resolver,
                    value: ResolveValue::Json(value),
                    request_id,
                }
            }
            Err(e) => OpResult::JsValue {
                resolver,
                value: ResolveValue::RejectError(OpError::error(e)),
                request_id,
            },
        }
    }));

    promise
}

// ---------------------------------------------------------------------------
// Callback: findOrCreate(collection, docJson, conflictFieldsJson)
// ---------------------------------------------------------------------------

/// Shared dispatch for `findOrCreate`. Same SQL shape as upsert except
/// the ON CONFLICT branch is a no-op self-assignment (so RETURNING
/// fires without mutating the row) and the RETURNING list appends
/// `(xmax = 0) AS __created` — true when the row was a fresh insert,
/// false when the conflict path matched an existing row.
///
/// Resolves with a JSON object `{ "row": {...}, "created": bool }`;
/// the SDK consumes both fields verbatim.
pub(crate) fn dispatch_find_or_create<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    app_id: &str,
    collection: &str,
    doc: Value,
    conflict_fields: Value,
) -> v8::Local<'s, v8::Promise> {
    let state = runtime_state(scope);
    let (resolver, request_id, promise) = setup_js_promise(scope, &state);

    let bq = match query::build_find_or_create(app_id, collection, &doc, &conflict_fields) {
        Ok(q) => q,
        Err(e) => {
            state.borrow_mut().spawned_ops.push(Box::pin(async move {
                OpResult::JsValue {
                    resolver,
                    value: ResolveValue::RejectError(OpError::error(e.to_string())),
                    request_id,
                }
            }));
            return promise;
        }
    };

    let coll_for_emit = collection.to_string();
    let app_for_emit = app_id.to_string();
    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        // Tag as Insert — the broker reaction is the same as upsert
        // (subscribers re-fetch), and tagging conservatively keeps us
        // from missing wake-ups when the row really was created.
        match exec_mutation_with_emit(
            bq,
            &app_for_emit,
            &coll_for_emit,
            crate::broker::ChangeOp::Insert,
        )
        .await
        {
            Ok(json) => {
                let arr: Vec<Value> = serde_json::from_str(&json).unwrap_or_default();
                let mut row = arr.into_iter().next().unwrap_or(Value::Null);
                let created = match row.as_object_mut() {
                    Some(obj) => obj
                        .remove("__created")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                    None => false,
                };
                let payload = serde_json::json!({ "row": row, "created": created });
                OpResult::JsValue {
                    resolver,
                    value: ResolveValue::Json(payload.to_string()),
                    request_id,
                }
            }
            Err(e) => OpResult::JsValue {
                resolver,
                value: ResolveValue::RejectError(OpError::error(e)),
                request_id,
            },
        }
    }));

    promise
}

// ---------------------------------------------------------------------------
// Auto-tx wrappers — defense-in-depth around query() / mutation() handlers
// ---------------------------------------------------------------------------
//
// `__zsBeginAutoTx(kindStr): Promise<number>`
//   Resolves with a numeric token the JS shim hands back to
//   `__zsEndAutoTx(token, success)`.
//
//     token = 0  → no auto-tx opened (kind is not "query"/"mutation", or a
//                  user-driven `db.transaction(...)` is already active, or
//                  the DB plugin is configured with a never-dialed dummy
//                  URL — capability gate fired before we got here).
//     token = 1  → auto-tx opened successfully; commit/rollback owed.
//
// `__zsEndAutoTx(token, success): Promise<void>`
//   Token 0 → resolved promise, no-op. Token 1 → COMMIT on success,
//   ROLLBACK on failure. Errors during commit/rollback are surfaced
//   verbatim to JS; the SSR shim still re-throws the underlying handler
//   error so callers don't see commit failures mask handler errors.
//
// Picks isolation level by kind:
//   query    → BEGIN ISOLATION LEVEL READ COMMITTED READ ONLY
//   mutation → BEGIN ISOLATION LEVEL <override or READ COMMITTED> READ WRITE
//
// The mutation default is READ COMMITTED — same as Postgres's default
// for explicit BEGIN. Apps that need write-skew protection bump to
// `serializable` per-mutation via the wrapper config; apps that need
// consistent re-reads inside the handler bump to `repeatable read`.
// Stronger isolation costs throughput (SSI bookkeeping, more 40001
// retries) and is opt-in by design.
//
// `action`, `stream`, `subscription` and unknown kinds are not wrapped:
//   actions can hold open external IO, streams/subscriptions are long-
//   lived; both would starve the connection pool. The capability gate
//   (B3 runtime layer) is the primary enforcement; auto-tx is a second
//   line of defense at the Postgres level.

/// `globalThis.__zsBeginAutoTx(kindStr): Promise<number>` — see module
/// comment above.
pub fn auto_begin_transaction(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();

    let kind = get_string_arg(scope, &args, 0);
    // Optional second arg: per-mutation isolation override
    // ("read committed" | "repeatable read" | "serializable"). Empty /
    // missing → use the per-kind default in `auto_tx_begin_sql`.
    let isolation = get_string_arg(scope, &args, 1);
    let (op_id, request_id, promise) = setup_promise(scope, &state);

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        match exec_auto_begin(kind.as_deref(), isolation.as_deref()).await {
            Ok(token) => OpResult::Completed {
                op_id,
                value: token.to_string(),
                request_id,
            },
            Err(e) => OpResult::Failed { op_id, error: e, request_id },
        }
    }));

    rv.set(promise.into());
}

/// `globalThis.__zsEndAutoTx(token, success): Promise<void>` — see
/// module comment above.
pub fn auto_end_transaction(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();

    let token = get_i64_arg(scope, &args, 0).unwrap_or(0);
    // Second arg is a boolean. `is_true()` covers the literal `true`;
    // we deliberately do NOT treat truthy non-booleans as success — the
    // SSR shim always passes a real boolean and any other shape is a
    // bug we want surfaced as a rollback (defense in depth).
    let success = args.length() >= 2 && args.get(1).is_true();
    let (op_id, request_id, promise) = setup_promise(scope, &state);

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        match exec_auto_end(token, success).await {
            Ok(()) => OpResult::Completed { op_id, value: "null".to_string(), request_id },
            Err(e) => OpResult::Failed { op_id, error: e, request_id },
        }
    }));

    rv.set(promise.into());
}

/// Per-kind BEGIN SQL. Returns `None` for kinds we don't wrap. For
/// mutations, `isolation` overrides the default READ COMMITTED.
fn auto_tx_begin_sql(kind: Option<&str>, isolation: Option<&str>) -> Option<String> {
    match kind {
        Some("query") => {
            Some("BEGIN ISOLATION LEVEL READ COMMITTED READ ONLY".to_string())
        }
        Some("mutation") => {
            let level = normalize_isolation(isolation).unwrap_or("READ COMMITTED");
            Some(format!("BEGIN ISOLATION LEVEL {level} READ WRITE"))
        }
        _ => None,
    }
}

/// Case-fold + whitespace-collapse a user-supplied isolation string to
/// one of Postgres' four accepted values. Unknown / empty → `None`
/// (caller falls back to the default).
fn normalize_isolation(s: Option<&str>) -> Option<&'static str> {
    let raw = s?.trim();
    if raw.is_empty() {
        return None;
    }
    let upper: String = raw
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_uppercase();
    match upper.as_str() {
        "READ COMMITTED" => Some("READ COMMITTED"),
        "REPEATABLE READ" => Some("REPEATABLE READ"),
        "SERIALIZABLE" => Some("SERIALIZABLE"),
        // READ UNCOMMITTED is accepted by Postgres but silently upgrades
        // to READ COMMITTED — treat as alias.
        "READ UNCOMMITTED" => Some("READ COMMITTED"),
        _ => None,
    }
}

async fn exec_auto_begin(
    kind: Option<&str>,
    isolation: Option<&str>,
) -> Result<u32, String> {
    // Skip if this kind isn't wrapped (action / stream / subscription /
    // unknown). Token 0 → end is a no-op.
    let Some(sql) = auto_tx_begin_sql(kind, isolation) else {
        return Ok(0);
    };

    // Don't wrap when a user-driven `db.transaction(...)` already
    // holds TX_CONN — that would be a nested-tx attempt the user
    // already opted out of. Returning 0 keeps the auto-end callback
    // off the user's tx entirely.
    let has_tx = crate::TX_CONN.with(|tx| tx.borrow().is_some());
    if has_tx {
        return Ok(0);
    }

    // Open a dedicated connection (same pattern as user-driven
    // `begin_transaction`). compio-postgres splits the connection into
    // (Client, Connection); spawn the run loop on a detached task, hold
    // the Client in TX_CONN.
    let url = crate::DB_URL.with(|u| u.borrow().clone())
        .ok_or_else(|| "db: not configured".to_string())?;
    let (client, connection) = compio_postgres::connect(&url, compio_postgres::NoTls)
        .await
        .map_err(|e| format!("db: auto-tx connect failed: {e}"))?;
    compio::runtime::spawn(async move {
        if let Err(e) = connection.run().await {
            eprintln!("db: auto-tx connection task error: {e}");
        }
    })
    .detach();

    client.execute(&sql, &[])
        .await
        .map_err(|e| format!("db: auto-tx BEGIN failed: {e}"))?;

    crate::TX_CONN.with(|tx| { tx.borrow_mut().replace(client); });
    crate::AUTO_TX_OWNED.with(|f| f.set(true));
    clear_pending_emits();
    Ok(1)
}

async fn exec_auto_end(token: i64, success: bool) -> Result<(), String> {
    // No-op tokens (unwrapped kinds) — nothing to commit.
    if token == 0 {
        return Ok(());
    }

    // Defensive ownership check: if AUTO_TX_OWNED is false the tx is
    // either gone or owned by user code. Either way leave it alone.
    let owned = crate::AUTO_TX_OWNED.with(|f| f.get());
    if !owned {
        return Ok(());
    }

    // Take the client. We MUST clear AUTO_TX_OWNED before any await so
    // a re-entrant call (shouldn't happen — V8 is single-threaded —
    // but cheap insurance) doesn't see stale ownership.
    let client = crate::TX_CONN.with(|tx| tx.borrow_mut().take());
    crate::AUTO_TX_OWNED.with(|f| f.set(false));

    let Some(client) = client else {
        // Ownership flag said yes, conn slot is empty: state is
        // inconsistent. Treat as already-ended.
        return Ok(());
    };

    let cmd = if success { "COMMIT" } else { "ROLLBACK" };
    let result = client.execute(cmd, &[]).await;
    drop(client); // explicit — terminates the spawned connection task.

    // Settle the deferred broker queue. On a successful COMMIT, fire
    // every event we'd have published mid-tx; on ROLLBACK (or COMMIT
    // failure) drop them silently so subscribers never see writes
    // Postgres just undid.
    if success && result.is_ok() {
        drain_pending_emits_on_commit();
    } else {
        clear_pending_emits();
    }

    result
        .map(|_| ())
        .map_err(|e| {
            // Walk the source chain so deferred-FK / unique violations
            // surface with their SQLSTATE detail. compio-postgres' Display
            // for Error::Db writes only "db error"; the real message
            // (`ERROR: insert or update on table "todos" violates
            // foreign key constraint ...`) lives on the cause.
            let mut msg = format!("db: auto-tx {cmd} failed: {e}");
            let mut cur: &dyn std::error::Error = &e;
            while let Some(src) = std::error::Error::source(cur) {
                msg.push_str(&format!(" — caused by: {src}"));
                cur = src;
            }
            msg
        })
}

/// Install `__zsBeginAutoTx` / `__zsEndAutoTx` on `globalThis`. Called
/// once during plugin `register()` via [`NativeRegistrar::add_setup`].
pub fn install_auto_tx_globals(scope: &mut v8::PinScope<'_, '_>) {
    let global = scope.get_current_context().global(scope);
    {
        let f = v8::Function::new(scope, auto_begin_transaction).unwrap();
        let key = v8::String::new(scope, "__zsBeginAutoTx").unwrap();
        global.set(scope, key.into(), f.into());
    }
    {
        let f = v8::Function::new(scope, auto_end_transaction).unwrap();
        let key = v8::String::new(scope, "__zsEndAutoTx").unwrap();
        global.set(scope, key.into(), f.into());
    }
}

// ===========================================================================
// B1 — @zeroship/migrations primitives
//
// These callbacks are the V8 bridge for the migrations module
// (`crate::migrations`). Each callback parses its arguments, sets up
// a promise, and spawns an async op that delegates to the matching
// `exec_*` function. See `migrations.rs` for the semantics.
// ===========================================================================

/// Lazy pool accessor shared by every async helper that needs the
/// pooled connection. First call kicks off `init_pool_async` (Postgres
/// connect + warm-up); subsequent calls clone the `Rc<Pool>` out of
/// the per-thread cell.
pub(crate) async fn ensure_pool() -> Result<Rc<compio_postgres::Pool>, String> {
    let has_pool = DB_POOL.with(|p| p.borrow().is_some());
    if !has_pool {
        crate::init_pool_async().await.map_err(|e| format!("db: lazy init failed: {e}"))?;
    }
    DB_POOL
        .with(|p| p.borrow().as_ref().map(Rc::clone))
        .ok_or_else(|| "db: pool not initialized".to_string())
}




// ===========================================================================
// C1 (P8a) — reactive queries via the in-process subscription broker
//
// JS surface (defined in sdks/db/src/subscribe.ts):
//
//   const sub = env.db.openSubscription(collection)   // → Subscription wrapper
//   const msg = await sub.pollJson()                  // → JSON event | null
//   sub.close()                                       // synchronous, idempotent
//   await env.db.replicationSetup()                   // → JSON setup outcome
//   await env.db.replicationWatchdog()                // → JSON [{slot,...}]
//   await env.db.replicationDropAbandoned(seconds)    // → JSON [dropped slot names]
//
// The Subscription wrapper is a `#[v8_class]` instance — its Weak
// finalizer closes the broker entry on GC, so callers that drop the
// wrapper without `.close()` still release the slot.
// ===========================================================================

/// `zeroship.db.openSubscription(collection)` → `Subscription` wrapper
///
/// Synchronous: subscribes on the thread-local broker, mints a
/// `Subscription` v8_class instance, returns it directly. The
/// wrapper's Weak finalizer closes the broker entry on GC, so callers
/// that drop the JS reference without `.close()` still release the
/// slot. Use `.pollJson()` to drain events (returns
/// `Promise<string|null>`) and `.close()` for idempotent explicit
/// teardown.
/// `db.replication.setup(opts?)` dispatch — see [`Db::replication`].
pub fn replication_setup_dispatch<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    app_id: String,
) -> v8::Local<'s, v8::Promise> {
    let state = runtime_state(scope);
    let (op_id, request_id, promise) = setup_promise(scope, &state);

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        let pool = match ensure_pool().await {
            Ok(p) => p,
            Err(e) => return OpResult::Failed { op_id, error: e, request_id },
        };
        match crate::replication::ensure_publication_and_slot(&pool, &app_id).await {
            Ok(out) => OpResult::Completed {
                op_id,
                value: out.to_json(),
                request_id,
            },
            Err(e) => OpResult::Failed { op_id, error: e, request_id },
        }
    }));
    promise
}

/// `db.replication.watchdog()` dispatch — see [`Db::replication`].
pub fn replication_watchdog_dispatch<'s>(
    scope: &mut v8::PinScope<'s, '_>,
) -> v8::Local<'s, v8::Promise> {
    let state = runtime_state(scope);
    let (op_id, request_id, promise) = setup_promise(scope, &state);

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        let pool = match ensure_pool().await {
            Ok(p) => p,
            Err(e) => return OpResult::Failed { op_id, error: e, request_id },
        };
        match crate::replication::watchdog_query(&pool).await {
            Ok(rows) => OpResult::Completed {
                op_id,
                value: crate::replication::watchdog_to_json(&rows),
                request_id,
            },
            Err(e) => OpResult::Failed { op_id, error: e, request_id },
        }
    }));
    promise
}

/// `db.replication.dropAbandoned(opts?)` dispatch — see
/// [`Db::replication`].
pub fn replication_drop_abandoned_dispatch<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    inactive_seconds: i64,
) -> v8::Local<'s, v8::Promise> {
    let state = runtime_state(scope);
    let (op_id, request_id, promise) = setup_promise(scope, &state);

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        let pool = match ensure_pool().await {
            Ok(p) => p,
            Err(e) => return OpResult::Failed { op_id, error: e, request_id },
        };
        match crate::replication::drop_abandoned_slots(&pool, inactive_seconds).await {
            Ok(names) => OpResult::Completed {
                op_id,
                value: serde_json::to_string(&names).unwrap_or_else(|_| "[]".into()),
                request_id,
            },
            Err(e) => OpResult::Failed { op_id, error: e, request_id },
        }
    }));
    promise
}

// ---------------------------------------------------------------------------
// Auto-spawn — `zeroship.db.startReplicationConsumer()`
// ---------------------------------------------------------------------------
//
// Apps that opt into reactive queries call this once at module init
// (`await env.db.startReplicationConsumer()`). It:
//   1. Provisions the publication + slot (idempotent — same as
//      `replicationSetup`).
//   2. Spawns a supervised WAL consumer task on the isolate's compio
//      runtime. The task lives for the isolate's lifetime and reconnects
//      with exponential backoff on transient failure (see
//      [`crate::wal_consumer::run_supervised`]).
//   3. Returns the `SetupOutcome` JSON.
//
// Idempotent — second call returns a JSON envelope with
// `{"alreadyRunning": true}` and short-circuits without spawning a
// second task. Tracked per-thread because the consumer task is
// thread-bound (the compio runtime is one per worker, the broker is
// thread-local).
//
// We chose explicit opt-in (a) over implicit spawn-on-first-subscribe
// (b): the failure surfaces at the call site, not deep inside a
// subscribe Promise. Apps with no reactive surface skip the cost.

thread_local! {
    /// Per-thread "is the consumer already running for this app?"
    /// guard. Keyed by app_id (a single worker may host multiple apps
    /// over its lifetime via the LRU cache, but only one consumer per
    /// app at a time).
    static RUNNING_CONSUMERS: std::cell::RefCell<std::collections::HashSet<String>> =
        std::cell::RefCell::new(std::collections::HashSet::new());
}

/// `zeroship.db.startReplicationConsumer()` → Promise<SetupOutcome JSON>
///
/// Idempotent. The first call provisions the slot+publication, spawns
/// a supervised WAL consumer for the current app, and resolves once
/// `replicationSetup` returns (i.e. provisioning is durable). The
/// consumer continues running on the compio runtime in the background;
/// it suppresses local-emit for this app via the per-app suppression
/// gate so subscribers receive each event exactly once via WAL.
///
/// Subsequent calls short-circuit and resolve with the cached outcome
/// envelope plus `"alreadyRunning": true`.
pub fn start_replication_consumer_dispatch<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    app_id: String,
) -> v8::Local<'s, v8::Promise> {
    let state = runtime_state(scope);
    let (op_id, request_id, promise) = setup_promise(scope, &state);

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        // Idempotent: if a consumer is already running for this app on
        // this thread, return a short-circuit envelope.
        let already = RUNNING_CONSUMERS
            .with(|r| r.borrow().contains(&app_id));
        if already {
            let value = serde_json::json!({
                "alreadyRunning": true,
                "app_id": app_id,
            })
            .to_string();
            return OpResult::Completed { op_id, value, request_id };
        }

        // Step 1: provision (idempotent).
        let pool = match ensure_pool().await {
            Ok(p) => p,
            Err(e) => return OpResult::Failed { op_id, error: e, request_id },
        };
        let setup = match crate::replication::ensure_publication_and_slot(&pool, &app_id).await {
            Ok(s) => s,
            Err(e) => return OpResult::Failed { op_id, error: e, request_id },
        };

        // Step 2: build the consumer descriptor.
        let url = crate::DB_URL
            .with(|u| u.borrow().clone())
            .unwrap_or_default();
        let consumer = match crate::wal_consumer::WalConsumer::new(&app_id, &url) {
            Ok(c) => c.with_start_lsn(setup.confirmed_flush_lsn.clone()),
            Err(e) => {
                return OpResult::Failed {
                    op_id,
                    error: e.to_string(),
                    request_id,
                };
            }
        };

        // Step 3: spawn the supervised task. `detach()` lets it run
        // for the lifetime of the isolate's compio runtime — there's
        // nowhere to join it, and the supervisor exits cleanly on
        // CopyDone or a fatal error.
        //
        // Mark the app as running BEFORE the spawn so a racing second
        // call to startReplicationConsumer() short-circuits even if
        // the consumer task hasn't yet entered its decode loop.
        let app_for_task = app_id.clone();
        RUNNING_CONSUMERS.with(|r| {
            r.borrow_mut().insert(app_id.clone());
        });
        compio::runtime::spawn(async move {
            crate::wal_consumer::run_supervised(consumer).await;
            // When the supervisor exits (graceful or fatal), free the
            // slot so a later opt-in re-spawn is allowed.
            RUNNING_CONSUMERS.with(|r| {
                r.borrow_mut().remove(&app_for_task);
            });
        })
        .detach();

        // Resolve with the setup outcome plus the "running" marker.
        let mut env = serde_json::from_str::<serde_json::Value>(&setup.to_json())
            .unwrap_or(serde_json::Value::Null);
        if let serde_json::Value::Object(ref mut m) = env {
            m.insert(
                "consumerStarted".into(),
                serde_json::Value::Bool(true),
            );
            m.insert("alreadyRunning".into(), serde_json::Value::Bool(false));
        }
        OpResult::Completed {
            op_id,
            value: env.to_string(),
            request_id,
        }
    }));
    promise
}

/// **Test-only**: probe whether the auto-spawn registry holds an entry
/// for `app_id`. Used by `tests/integration.rs` to assert idempotency
/// without reaching into private state.
#[doc(hidden)]
pub fn is_consumer_registered_for_tests(app_id: &str) -> bool {
    RUNNING_CONSUMERS.with(|r| r.borrow().contains(app_id))
}

/// **Test-only**: clear the auto-spawn registry. Used to reset state
/// between integration tests that share a thread.
#[doc(hidden)]
pub fn clear_consumer_registry_for_tests() {
    RUNNING_CONSUMERS.with(|r| r.borrow_mut().clear());
}
