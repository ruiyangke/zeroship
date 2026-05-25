//! CRUD dispatch helpers — one entry point per Collection method.
//!
//! Each `dispatch_*` here is the bridge between a v8_class method on
//! `Collection` (`v8_classes::collection`) and the async exec layer in
//! [`crate::exec`]. The shape is consistent across every helper:
//!
//! 1. Grab the runtime state slot.
//! 2. Optionally record into the active read-set ([`crate::read_set`])
//!    for P8b subscription narrowing.
//! 3. `setup_js_promise` — allocate the promise + resolver.
//! 4. Build the SQL via `crate::query::build_*`.
//! 5. Hand off to `run_op` — the async tail that drives the exec
//!    helper, resolves the promise with the appropriate `ResolveValue`,
//!    or rejects via `DbError::to_op_error` (carries `.code` for the
//!    SDK).
//!
//! Pre-stage-8b each helper was ~50 LOC of boilerplate; the template
//! collapses the bottom half so each helper is ~15 LOC of intent —
//! "which builder, which exec, which resolve". No new public API: each
//! helper stays `pub(crate)` and is called from
//! `v8_classes::collection`.
//!
//! The capability gate (`refuse_if_query_capability`) is enforced by
//! the v8_class methods *before* reaching the dispatch helper — write
//! ops trust their callers.

use std::future::Future;

use base64::Engine as _;
use serde_json::Value;
use zeroship_runtime::state::{OpResult, ResolveValue};

use crate::error::DbError;
use crate::exec::{exec_count, exec_mutation_with_emit, exec_query};
use crate::query;
use crate::v8_bridge::{runtime_state, setup_js_promise};

// **P5 PR 2** — transparent column-encryption pass. The helpers in
// this module (`encrypt_row_on_write` / `decrypt_row_on_read`) sit
// around `query::build_*` and `exec_query` respectively.
//
// Visibility: crate-private in release builds; `pub` under
// `test-helpers` so `tests/sqlite_integration.rs` can drive the
// helpers directly for the P5 PR 3.5 end-to-end encrypted-column
// CRUD round-trip test (the orchestrator's CRUD entry today is PG-only,
// so the SQLite e2e gate composes the helpers itself).
#[cfg(not(feature = "test-helpers"))]
pub(crate) mod encryption_pass;
#[cfg(feature = "test-helpers")]
pub mod encryption_pass;

// **P5.5 PR 2** — Path B mask transforms + dual-write CRUD pass.
// Same visibility pattern as `encryption_pass` so integration tests
// can reach the helpers when `test-helpers` is on.
#[cfg(not(feature = "test-helpers"))]
pub(crate) mod mask_pass;
#[cfg(feature = "test-helpers")]
pub mod mask_pass;

// **P5.5 PR 4** — `unmask()` RPC dispatch + audit row writer.
// Same visibility pattern as the sibling passes so integration tests
// can exercise `dispatch_unmask` directly when `test-helpers` is on.
#[cfg(not(feature = "test-helpers"))]
pub(crate) mod unmask;
#[cfg(feature = "test-helpers")]
pub mod unmask;

// **P5.5 PR 5** — `defineMaskPolicy()` storage + dispatcher + cache.
// Same visibility pattern: integration tests reach into the helpers
// via the `test-helpers` gate to drive `dispatch_set_mask_policy`
// directly without standing up V8.
#[cfg(not(feature = "test-helpers"))]
pub(crate) mod mask_policy;
#[cfg(feature = "test-helpers")]
pub mod mask_policy;

pub(crate) use mask_policy::dispatch_set_mask_policy_field;
pub(crate) use unmask::{dispatch_bulk_unmask_field, dispatch_unmask_field};

// **P5.5 PR 6** — mask backfill / rewrite / removal jobs driven by the
// register-model apply pipeline. Same visibility pattern: `pub` under
// `test-helpers` so the integration tests can drive the helpers
// directly without standing up the full orchestrator.
#[cfg(not(feature = "test-helpers"))]
pub(crate) mod mask_backfill;
#[cfg(feature = "test-helpers")]
pub mod mask_backfill;

// **P5.5 PR 7** — drift detection: sample masked-column siblings vs.
// recomputed mask of decrypt(parent). Same visibility pattern so the
// SQLite + PG integration suites can drive `run_drift_check_*`
// directly via the `test-helpers` gate.
#[cfg(not(feature = "test-helpers"))]
pub(crate) mod mask_drift;
#[cfg(feature = "test-helpers")]
pub mod mask_drift;

// **P7 PR 3** — INSERT-time auto-population of platform system fields
// (`id`, `created_by`, `updated_by`). Same visibility pattern as the
// sibling encryption / mask passes so the integration tests can drive
// `apply_system_fields_on_insert*` directly under the `test-helpers`
// gate.
#[cfg(not(feature = "test-helpers"))]
pub(crate) mod system_fields_pass;
#[cfg(feature = "test-helpers")]
pub mod system_fields_pass;

// ---------------------------------------------------------------------------
// dispatch_op template
// ---------------------------------------------------------------------------

/// The shared "build → exec → resolve" tail every CRUD dispatcher
/// shares. Drives the spawned-op future, packs the result into an
/// `OpResult::JsValue`, and stamps `.code` on any `DbError` via
/// `to_op_error()` so the SDK sees `err.code` regardless of which
/// dispatcher threw.
///
/// `build_result` is the (already-evaluated) output of the
/// `query::build_*` call. Builder errors are `QueryError` →
/// `DbError::ValidationFailed` via the `From` impl — the resulting
/// JS error carries `code = "invalid_filter"` / `"invalid_collection"`
/// / `"invalid_identifier"`.
///
/// `exec` runs against either the pool or the active
/// [`crate::context::IsolateDbContext::tx_conn`] (transparently —
/// `exec::run_sql` already handles that).
///
/// `resolve` lowers the exec's success value to the V8-bound
/// `ResolveValue` shape (typically `Json` for arrays/objects, `F64`
/// for counts).
async fn run_op<R, EFut, Resolve>(
    resolver: v8::Global<v8::PromiseResolver>,
    request_id: Option<u64>,
    build_result: Result<query::BuiltQuery, query::QueryError>,
    exec: impl FnOnce(query::BuiltQuery) -> EFut,
    resolve: Resolve,
) -> OpResult
where
    EFut: Future<Output = Result<R, DbError>>,
    Resolve: FnOnce(R) -> ResolveValue,
{
    let bq = match build_result {
        Ok(bq) => bq,
        Err(e) => {
            return OpResult::JsValue {
                resolver,
                value: ResolveValue::RejectError(DbError::from(e).to_op_error()),
                request_id,
            };
        }
    };
    match exec(bq).await {
        Ok(v) => OpResult::JsValue {
            resolver,
            value: resolve(v),
            request_id,
        },
        Err(e) => OpResult::JsValue {
            resolver,
            value: ResolveValue::RejectError(e.to_op_error()),
            request_id,
        },
    }
}

fn current_sql_dialect() -> query::SqlDialect {
    match crate::context::with(|c| c.backend()) {
        Some(crate::backend::BackendHandle::Sqlite(_)) => query::SqlDialect::Sqlite,
        _ => query::SqlDialect::Postgres,
    }
}

fn maybe_lower_sqlite_boolean_doc(
    app_id: &str,
    collection: &str,
    doc: &mut Value,
) {
    if current_sql_dialect() != query::SqlDialect::Sqlite {
        return;
    }
    let Some(schema) = crate::context::with(|c| c.schema_for(app_id, collection)) else {
        return;
    };
    lower_boolean_doc_with_schema(&schema, doc);
}

fn maybe_lower_sqlite_boolean_docs(
    app_id: &str,
    collection: &str,
    docs: &mut Value,
) {
    if current_sql_dialect() != query::SqlDialect::Sqlite {
        return;
    }
    let Some(schema) = crate::context::with(|c| c.schema_for(app_id, collection)) else {
        return;
    };
    let Some(arr) = docs.as_array_mut() else {
        return;
    };
    for doc in arr {
        lower_boolean_doc_with_schema(&schema, doc);
    }
}

fn maybe_lower_sqlite_boolean_update(
    app_id: &str,
    collection: &str,
    patch: &mut Value,
) {
    if current_sql_dialect() != query::SqlDialect::Sqlite {
        return;
    }
    let Some(schema) = crate::context::with(|c| c.schema_for(app_id, collection)) else {
        return;
    };
    lower_boolean_update_with_schema(&schema, patch);
}

fn maybe_lower_sqlite_boolean_filter(
    app_id: &str,
    collection: &str,
    filter: &mut Value,
) {
    if current_sql_dialect() != query::SqlDialect::Sqlite {
        return;
    }
    let Some(schema) = crate::context::with(|c| c.schema_for(app_id, collection)) else {
        return;
    };
    lower_boolean_filter_with_schema(&schema, filter);
}

fn lower_boolean_doc_with_schema(schema: &Value, doc: &mut Value) {
    let Some(obj) = doc.as_object_mut() else {
        return;
    };
    for (field, value) in obj {
        if field.starts_with("__zsenc__") {
            continue;
        }
        if schema_field_type(schema, field) == Some("boolean") {
            lower_boolean_scalar(value);
        }
    }
}

fn lower_boolean_update_with_schema(schema: &Value, patch: &mut Value) {
    let Some(obj) = patch.as_object_mut() else {
        return;
    };
    if let Some(set_doc) = obj.get_mut("$set") {
        lower_boolean_doc_with_schema(schema, set_doc);
    }
    for (field, value) in obj {
        if field.starts_with('$') || field.starts_with("__zsenc__") {
            continue;
        }
        if schema_field_type(schema, field) != Some("boolean") {
            continue;
        }
        match value {
            Value::Bool(_) => lower_boolean_scalar(value),
            Value::Object(ops) => {
                if let Some(set_val) = ops.get_mut("$set") {
                    lower_boolean_scalar(set_val);
                }
            }
            _ => {}
        }
    }
}

fn lower_boolean_filter_with_schema(schema: &Value, filter: &mut Value) {
    let Some(obj) = filter.as_object_mut() else {
        return;
    };
    for (key, value) in obj {
        if key.starts_with('$') {
            match key.as_str() {
                "$and" | "$or" => {
                    if let Some(arr) = value.as_array_mut() {
                        for clause in arr {
                            lower_boolean_filter_with_schema(schema, clause);
                        }
                    }
                }
                "$not" => lower_boolean_filter_with_schema(schema, value),
                _ => {}
            }
            continue;
        }
        if schema_field_type(schema, key) == Some("boolean") {
            lower_boolean_filter_value(value);
        }
    }
}

fn lower_boolean_filter_value(value: &mut Value) {
    match value {
        Value::Bool(_) => lower_boolean_scalar(value),
        Value::Object(ops) => {
            for (op, operand) in ops {
                match op.as_str() {
                    "$eq" | "$ne" | "$gt" | "$gte" | "$lt" | "$lte" => {
                        lower_boolean_scalar(operand);
                    }
                    "$in" | "$nin" => {
                        if let Some(arr) = operand.as_array_mut() {
                            for item in arr {
                                lower_boolean_scalar(item);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

fn lower_boolean_scalar(value: &mut Value) {
    if let Value::Bool(b) = value {
        *value = Value::Number(serde_json::Number::from(i64::from(u8::from(*b))));
    }
}

fn schema_field_type<'a>(schema: &'a Value, field: &str) -> Option<&'a str> {
    schema
        .as_object()?
        .get(field)?
        .get("type")?
        .as_str()
}

/// Lower a `Vec<Value>` result to a single JSON value: the first row,
/// or `null` when the result was empty. Used by `insert` / `update` /
/// `delete` / `upsert`, all of which the SDK expects to resolve to a
/// single row or `null`.
///
/// The `Vec<Value>` arrives already decoded from `compio_postgres::Row`
/// — see [`crate::v8_bridge::rows_to_json_value`]. We serialise the
/// single row once here; V8 then parses it via `JSON.parse` inside
/// `ResolveValue::Json`. Net cost: one `to_string` + one
/// `JSON.parse`, down from the pre-fix four
/// (rows→string→parse→string→parse).
fn first_row_or_null(rows: Vec<Value>) -> ResolveValue {
    let value = rows.into_iter().next().unwrap_or(Value::Null).to_string();
    ResolveValue::Json(value)
}

/// **P9 PR 2** — `first_row_or_null` variant that, when `has_masked` is
/// set, resolves via [`ResolveValue::JsonWithRehydration`] so the pump
/// walks the parsed value and replaces `__zsmask__` sentinels with
/// native `MaskedValue` instances. When `has_masked` is `false` this is
/// identical to `first_row_or_null` (plain `JSON.parse`, no walk).
fn first_row_or_null_masked(rows: Vec<Value>, has_masked: bool) -> ResolveValue {
    let value = rows.into_iter().next().unwrap_or(Value::Null).to_string();
    maybe_rehydrate(value, has_masked)
}

/// Lower a `Vec<Value>` result to the row count, as a JS `number`.
/// Used by `updateMany` / `deleteMany` (resolves to the affected-row
/// count).
#[allow(clippy::cast_precision_loss)]
fn row_count_as_f64(rows: Vec<Value>) -> ResolveValue {
    ResolveValue::F64(rows.len() as f64)
}

/// Lower a `Vec<Value>` result to a JSON-array string. Used by `find`
/// / `insertMany` / `aggregate` where the SDK expects an array of
/// rows. The serialisation happens exactly once — at the V8 boundary
/// — replacing the pre-fix "stringify the result set → parse it →
/// re-stringify it" round-trip.
fn rows_as_json_array(rows: Vec<Value>) -> ResolveValue {
    ResolveValue::Json(Value::Array(rows).to_string())
}

/// **P9 PR 2** — `rows_as_json_array` variant that resolves via
/// [`ResolveValue::JsonWithRehydration`] when `has_masked` is set. See
/// [`first_row_or_null_masked`].
fn rows_as_json_array_masked(rows: Vec<Value>, has_masked: bool) -> ResolveValue {
    let value = Value::Array(rows).to_string();
    maybe_rehydrate(value, has_masked)
}

/// **P9 PR 2** — pick `ResolveValue::JsonWithRehydration` (walk the
/// parsed value, mint `MaskedValue` for `__zsmask__` sentinels) when the
/// result is known to carry masked columns; otherwise the plain
/// `ResolveValue::Json` fast path (bulk `JSON.parse`, no walk).
fn maybe_rehydrate(json: String, has_masked: bool) -> ResolveValue {
    if has_masked {
        ResolveValue::JsonWithRehydration {
            json,
            transform: crate::v8_classes::masked_value::rehydrate_masked_values,
        }
    } else {
        ResolveValue::Json(json)
    }
}

fn normalize_rows_on_read(
    app_id: &str,
    collection: &str,
    mut rows: Vec<Value>,
) -> Result<Vec<Value>, DbError> {
    let schema = crate::context::with(|c| c.schema_for(app_id, collection));
    for row in rows.iter_mut() {
        normalize_row_on_read(schema.as_ref(), row)?;
    }
    Ok(rows)
}

fn normalize_row_on_read(schema: Option<&Value>, row: &mut Value) -> Result<(), DbError> {
    let Some(obj) = row.as_object_mut() else {
        return Ok(());
    };
    for (key, value) in obj.iter_mut() {
        if matches!(key.as_str(), "created_at" | "updated_at" | "deleted_at") {
            normalize_timestamp_value(value)?;
            continue;
        }

        let Some(def) = schema
            .and_then(Value::as_object)
            .and_then(|schema_obj| schema_obj.get(key))
            .and_then(Value::as_object)
        else {
            continue;
        };

        if def.get("encrypted").is_some() {
            continue;
        }

        match def.get("type").and_then(Value::as_str) {
            Some("boolean") => normalize_boolean_value(value),
            Some("json") | Some("object") | Some("array") | Some("union") => {
                normalize_json_value(value)
            }
            Some("bytes") => normalize_bytes_value(value)?,
            Some("date") | Some("calendarDate") => normalize_timestamp_value(value)?,
            _ => {}
        }
    }
    Ok(())
}

fn normalize_boolean_value(value: &mut Value) {
    match value {
        Value::Bool(_) | Value::Null => {}
        Value::Number(n) => {
            if n.as_i64() == Some(0) {
                *value = Value::Bool(false);
            } else if n.as_i64() == Some(1) {
                *value = Value::Bool(true);
            }
        }
        Value::String(s) => match s.as_str() {
            "0" | "false" => *value = Value::Bool(false),
            "1" | "true" => *value = Value::Bool(true),
            _ => {}
        },
        _ => {}
    }
}

fn normalize_json_value(value: &mut Value) {
    if let Value::String(s) = value {
        if let Ok(parsed) = serde_json::from_str::<Value>(s) {
            *value = parsed;
        }
    }
}

fn normalize_bytes_value(value: &mut Value) -> Result<(), DbError> {
    match value {
        Value::Null | Value::String(_) => Ok(()),
        Value::Array(arr) => {
            let mut raw = Vec::with_capacity(arr.len());
            for cell in arr.iter() {
                let Some(n) = cell.as_u64() else {
                    return Err(DbError::internal(format!(
                        "normalize_row_on_read: bytes field expected byte array, got {cell:?}"
                    )));
                };
                let byte = u8::try_from(n).map_err(|_| {
                    DbError::internal(format!(
                        "normalize_row_on_read: bytes field byte out of range: {n}"
                    ))
                })?;
                raw.push(byte);
            }
            *value = Value::String(base64::engine::general_purpose::STANDARD.encode(raw));
            Ok(())
        }
        other => Err(DbError::internal(format!(
            "normalize_row_on_read: bytes field expected string/array/null, got {other:?}"
        ))),
    }
}

fn normalize_timestamp_value(value: &mut Value) -> Result<(), DbError> {
    match value {
        Value::Null | Value::Number(_) => Ok(()),
        Value::String(s) => {
            if let Some(ms) = parse_timestamp_millis(s) {
                *value = Value::Number(serde_json::Number::from(ms));
            }
            Ok(())
        }
        other => Err(DbError::internal(format!(
            "normalize_row_on_read: timestamp field expected string/number/null, got {other:?}"
        ))),
    }
}

fn parse_timestamp_millis(s: &str) -> Option<i64> {
    if let Some(ms) = crate::backend::sqlite::session_minter::parse_iso_to_millis(s) {
        return Some(ms);
    }

    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }
    if b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b' ' | b'T') || b[13] != b':' || b[16] != b':' {
        return None;
    }

    let year: i32 = std::str::from_utf8(&b[0..4]).ok()?.parse().ok()?;
    let month: u32 = std::str::from_utf8(&b[5..7]).ok()?.parse().ok()?;
    let day: u32 = std::str::from_utf8(&b[8..10]).ok()?.parse().ok()?;
    let hour: i64 = std::str::from_utf8(&b[11..13]).ok()?.parse().ok()?;
    let minute: i64 = std::str::from_utf8(&b[14..16]).ok()?.parse().ok()?;
    let second: i64 = std::str::from_utf8(&b[17..19]).ok()?.parse().ok()?;
    if !(0..=23).contains(&hour) || !(0..=59).contains(&minute) || !(0..=59).contains(&second) {
        return None;
    }

    let mut millis = 0i64;
    let mut tz_offset_minutes = 0i64;
    let mut idx = 19usize;

    if idx < b.len() && b[idx] == b'.' {
        idx += 1;
        let frac_start = idx;
        while idx < b.len() && b[idx].is_ascii_digit() {
            idx += 1;
        }
        if idx == frac_start {
            return None;
        }
        let frac = &b[frac_start..idx];
        let frac_digits = std::str::from_utf8(frac).ok()?;
        let mut milli_digits = frac_digits.chars().take(3).collect::<String>();
        while milli_digits.len() < 3 {
            milli_digits.push('0');
        }
        millis = milli_digits.parse().ok()?;
    }

    if idx < b.len() {
        tz_offset_minutes = parse_timestamp_offset_minutes(&b[idx..])?;
    }

    let days = days_from_civil(year, month, day)?;
    let total_secs = days * 86_400 + hour * 3600 + minute * 60 + second;
    Some(total_secs * 1000 + millis - tz_offset_minutes * 60 * 1000)
}

fn parse_timestamp_offset_minutes(rest: &[u8]) -> Option<i64> {
    match rest {
        b"Z" | b"z" => Some(0),
        [sign @ (b'+' | b'-'), h1, h2] => {
            let hours: i64 = std::str::from_utf8(&[*h1, *h2]).ok()?.parse().ok()?;
            if hours > 23 {
                return None;
            }
            let sign = if *sign == b'-' { -1 } else { 1 };
            Some(sign * hours * 60)
        }
        [sign @ (b'+' | b'-'), h1, h2, b':', m1, m2] => {
            let hours: i64 = std::str::from_utf8(&[*h1, *h2]).ok()?.parse().ok()?;
            let minutes: i64 = std::str::from_utf8(&[*m1, *m2]).ok()?.parse().ok()?;
            if hours > 23 || minutes > 59 {
                return None;
            }
            let sign = if *sign == b'-' { -1 } else { 1 };
            Some(sign * (hours * 60 + minutes))
        }
        [sign @ (b'+' | b'-'), h1, h2, m1, m2] => {
            let hours: i64 = std::str::from_utf8(&[*h1, *h2]).ok()?.parse().ok()?;
            let minutes: i64 = std::str::from_utf8(&[*m1, *m2]).ok()?.parse().ok()?;
            if hours > 23 || minutes > 59 {
                return None;
            }
            let sign = if *sign == b'-' { -1 } else { 1 };
            Some(sign * (hours * 60 + minutes))
        }
        _ => None,
    }
}

fn days_from_civil(y: i32, m: u32, d: u32) -> Option<i64> {
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let y = if m <= 2 { i64::from(y) - 1 } else { i64::from(y) };
    let era = y.div_euclid(400);
    let yoe = (y - era * 400) as u64;
    let m = m as i64;
    let d = d as i64;
    let doy = ((153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1) as u64;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe as i64 - 719_468)
}

// ---------------------------------------------------------------------------
// find — read path
// ---------------------------------------------------------------------------

// P9 PR 1: `dispatch_find_one` was deleted along with `Collection.findOne`
// (Convex-style consolidation). The SDK reaches the same "first matching
// row" semantic via `find(filter).first()` / `.unique()` / `.last()` on
// the Query terminal, which composes the existing `dispatch_find` with
// `LIMIT 1` (or `LIMIT 2` for strict `.unique()`).

/// **P5.5 PR 7** — extract `opts.unmask` into a `Vec<String>`. Returns
/// empty when the field is absent, null, or not an array of strings —
/// per the proposal, malformed `unmask` shapes are tolerated as
/// "no hint" rather than an error so a stale SDK build doesn't bring
/// down the find path.
fn parse_unmask_opt(opt: Option<&Value>) -> Vec<String> {
    let Some(Value::Array(arr)) = opt else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect()
}

/// Shared dispatch for `find`. Reads `limit`/`offset`/`orderBy`/
/// `select`/`unmask`/`actor` out of `opts`. The per-query unmask hint
/// (P5.5 PR 7) honours an upfront authorisation fence — a single
/// unauthorised column refuses the whole find with
/// `unmask_not_permitted`.
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
    let order_by = opts.get("orderBy").cloned();
    let select = opts.get("select").cloned();
    let unmask_columns = parse_unmask_opt(opts.get("unmask"));
    let unmask_actor = opts.get("actor").cloned().filter(|v| !v.is_null());
    let unmask_reason = opts
        .get("unmaskReason")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let include_deleted = opts
        .get("include_deleted")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let (resolver, request_id, promise) = setup_js_promise(scope, &state);

    let app = app_id.to_string();
    let coll = collection.to_string();

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        // **P5.5 PR 7** — upfront auth fence for the unmask hint.
        if !unmask_columns.is_empty() {
            if let Err(e) = crate::crud::unmask::authorize_query_hint(
                &app,
                &coll,
                &unmask_columns,
                &unmask_actor,
                &unmask_reason,
            )
            .await
            {
                return OpResult::JsValue {
                    resolver,
                    value: ResolveValue::RejectError(e.to_op_error()),
                    request_id,
                };
            }
        }

        // **P5.5 PR 3** — fetch the cached schema BEFORE building SQL.
        // When the schema declares masked columns, the SELECT clause
        // emits `"<col>_masked" AS "<col>"` so the ciphertext column
        // never leaves Postgres on a default read.
        let schema_hint = crate::context::with(|c| c.schema_for(&app, &coll));
        // **P7 PR 5** — soft-delete auto-filter gate.
        let filter_soft_deleted =
            system_fields_pass::should_filter_soft_deleted(&app, &coll, include_deleted);
        let mut sql_filter = filter;
        maybe_lower_sqlite_boolean_filter(&app, &coll, &mut sql_filter);
        let built = query::build_find_with_schema_and_unmask_and_soft_delete_with_dialect(
            &app,
            &coll,
            &sql_filter,
            limit,
            offset,
            order_by.as_ref(),
            select.as_ref(),
            schema_hint.as_ref(),
            &unmask_columns,
            filter_soft_deleted,
            current_sql_dialect(),
        );
        let bq = match built {
            Ok(bq) => bq,
            Err(e) => {
                return OpResult::JsValue {
                    resolver,
                    value: ResolveValue::RejectError(DbError::from(e).to_op_error()),
                    request_id,
                };
            }
        };
        match exec_query(bq).await {
            Ok(rows) => {
                let rows = match normalize_rows_on_read(&app, &coll, rows) {
                    Ok(rows) => rows,
                    Err(e) => {
                        return OpResult::JsValue {
                            resolver,
                            value: ResolveValue::RejectError(e.to_op_error()),
                            request_id,
                        };
                    }
                };
                // **P5 PR 2** — decrypt encrypted columns on every
                // returned row. No-op when the schema declares none.
                let rows = match apply_encryption_on_read(&app, &coll, rows).await {
                    Ok(r) => r,
                    Err(e) => {
                        return OpResult::JsValue {
                            resolver,
                            value: ResolveValue::RejectError(e.to_op_error()),
                            request_id,
                        };
                    }
                };
                // **P5.5 PR 3** — wrap masked columns in the
                // `__zsmask__`-tagged wire shape.
                let (mut rows, has_masked) = match apply_mask_wrap_on_read(&app, &coll, rows) {
                    Ok(r) => r,
                    Err(e) => {
                        return OpResult::JsValue {
                            resolver,
                            value: ResolveValue::RejectError(e.to_op_error()),
                            request_id,
                        };
                    }
                };
                if !unmask_columns.is_empty() {
                    if let Err(e) = crate::crud::unmask::dispatch_unmask_for_query(
                        &app,
                        &coll,
                        &unmask_columns,
                        &mut rows,
                    )
                    .await
                    {
                        return OpResult::JsValue {
                            resolver,
                            value: ResolveValue::RejectError(e.to_op_error()),
                            request_id,
                        };
                    }
                    if let Err(e) = crate::crud::unmask::audit_query_hint_granted(
                        &app,
                        &coll,
                        &unmask_columns,
                        &unmask_actor,
                        &unmask_reason,
                    )
                    .await
                    {
                        return OpResult::JsValue {
                            resolver,
                            value: ResolveValue::RejectError(e.to_op_error()),
                            request_id,
                        };
                    }
                }
                OpResult::JsValue {
                    resolver,
                    value: rows_as_json_array_masked(rows, has_masked),
                    request_id,
                }
            }
            Err(e) => OpResult::JsValue {
                resolver,
                value: ResolveValue::RejectError(e.to_op_error()),
                request_id,
            },
        }
    }));

    promise
}

// ---------------------------------------------------------------------------
// insert / insertMany — write paths returning the row(s)
// ---------------------------------------------------------------------------

/// Shared dispatch for `insert`. The capability gate is the caller's
/// responsibility — `Collection::insert` calls
/// `refuse_if_query_capability` before reaching here.
pub(crate) fn dispatch_insert<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    app_id: &str,
    collection: &str,
    doc: Value,
) -> v8::Local<'s, v8::Promise> {
    let state = runtime_state(scope);
    let (resolver, request_id, promise) = setup_js_promise(scope, &state);

    let coll = collection.to_string();
    let app = app_id.to_string();

    // **P7 PR 3** — read the request-bound actor id at the synchronous
    // boundary BEFORE the async tail starts. The runtime's
    // `executing_request_id` is only guaranteed-set on the pump turn
    // that initiates the dispatch; once we `.await` (e.g. the
    // encryption pass's `resolve_key` round-trip), the pump may rotate
    // the slot. Reading here pins the actor to the request that
    // originated the insert.
    let actor_id = system_fields_pass::current_actor_id(&state);

    // **P5 PR 2** — async tail so the encryption pass can `.await` the
    // backend's `resolve_key` (PG SECURITY DEFINER round-trip) before
    // `build_insert` consumes the doc. The non-encrypted hot path stays
    // identical — `apply_encryption_on_write` short-circuits when the
    // cached schema has no `t.encrypted(...)` columns.
    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        let mut doc = doc;
        // **P7 PR 3** — populate `id` (auto-mint when absent) and
        // `created_by` / `updated_by` (from the session actor when
        // present). Runs BEFORE the encryption pass so encrypted
        // columns declared on the same row see a fully-populated doc;
        // runs BEFORE `build_insert` so the SQL `RETURNING *` carries
        // every system field back to the SDK.
        system_fields_pass::apply_system_fields_on_insert(
            &mut doc,
            &app,
            &coll,
            actor_id.as_deref(),
        );
        if let Err(e) = apply_encryption_on_write(&app, &coll, &mut doc).await {
            return OpResult::JsValue {
                resolver,
                value: ResolveValue::RejectError(e.to_op_error()),
                request_id,
            };
        }
        maybe_lower_sqlite_boolean_doc(&app, &coll, &mut doc);
        let built = query::build_insert_with_dialect(&app, &coll, &doc, current_sql_dialect());
        let result = match built {
            Ok(bq) => {
                exec_mutation_with_emit(bq, &app, &coll, crate::broker::ChangeOp::Insert).await
            }
            Err(e) => Err(DbError::from(e)),
        };
        match result {
            Ok(rows) => {
                let rows = match normalize_rows_on_read(&app, &coll, rows) {
                    Ok(rows) => rows,
                    Err(e) => {
                        return OpResult::JsValue {
                            resolver,
                            value: ResolveValue::RejectError(e.to_op_error()),
                            request_id,
                        };
                    }
                };
                let rows = apply_encryption_on_read(&app, &coll, rows).await;
                let rows = match rows {
                    Ok(rows) => rows,
                    Err(e) => {
                        return OpResult::JsValue {
                            resolver,
                            value: ResolveValue::RejectError(e.to_op_error()),
                            request_id,
                        };
                    }
                };
                // **P5.5 PR 3** — wrap masked columns from RETURNING *
                // so the SDK sees `MaskedValue<T>`, not the raw
                // ciphertext / plaintext parent slot.
                let (rows, has_masked) = match apply_mask_wrap_on_read(&app, &coll, rows) {
                    Ok(r) => r,
                    Err(e) => {
                        return OpResult::JsValue {
                            resolver,
                            value: ResolveValue::RejectError(e.to_op_error()),
                            request_id,
                        };
                    }
                };
                OpResult::JsValue {
                    resolver,
                    value: first_row_or_null_masked(rows, has_masked),
                    request_id,
                }
            }
            Err(e) => OpResult::JsValue {
                resolver,
                value: ResolveValue::RejectError(e.to_op_error()),
                request_id,
            },
        }
    }));

    promise
}

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

    // **P7 PR 3** — populate `id` per-row + `created_by` / `updated_by`
    // for the whole batch under one actor stamp BEFORE `build_insert_many`
    // collects column unions. Same actor-binding rationale as
    // `dispatch_insert`: pin to the originating request's actor at the
    // sync boundary.
    let actor_id = system_fields_pass::current_actor_id(&state);
    let mut docs = docs;
    system_fields_pass::apply_system_fields_on_insert_many(
        &mut docs,
        app_id,
        collection,
        actor_id.as_deref(),
    );
    maybe_lower_sqlite_boolean_docs(app_id, collection, &mut docs);

    let built =
        query::build_insert_many_with_dialect(app_id, collection, &docs, current_sql_dialect());
    let coll = collection.to_string();
    let app = app_id.to_string();

    state.borrow_mut().spawned_ops.push(Box::pin(run_op(
        resolver,
        request_id,
        built,
        move |bq| async move {
            let rows =
                exec_mutation_with_emit(bq, &app, &coll, crate::broker::ChangeOp::Insert).await?;
            normalize_rows_on_read(&app, &coll, rows)
        },
        rows_as_json_array,
    )));

    promise
}

// ---------------------------------------------------------------------------
// updateOne / updateMany — write paths
// ---------------------------------------------------------------------------

/// Shared dispatch for `updateOne`. See [`dispatch_insert`] for the
/// capability-gate contract.
///
/// **P7 PR 4** — every UPDATE auto-bumps `version` + `updated_at` +
/// `updated_by` (when an actor is in scope). When the caller's filter
/// carries `version: N`, the auto-bumped SQL still runs but the
/// affected-rows count is checked: 0 affected → typed
/// `version_mismatch` error. A `version` filter without an `id`
/// predicate refuses eagerly with `multi_row_version_filter_unsupported`
/// — the CAS semantics don't generalise to multi-row UPDATEs.
pub(crate) fn dispatch_update_one<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    app_id: &str,
    collection: &str,
    filter: Value,
    update: Value,
) -> v8::Local<'s, v8::Promise> {
    let state = runtime_state(scope);
    let (resolver, request_id, promise) = setup_js_promise(scope, &state);

    let coll = collection.to_string();
    let app = app_id.to_string();

    // **P7 PR 4** — read actor at the sync boundary (same rationale as
    // `dispatch_insert`'s actor pin: the runtime's `executing_request_id`
    // rotates on the next pump turn).
    let actor_id = system_fields_pass::current_actor_id(&state);

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        // **P7 PR 4** — validate immutable system fields + check the
        // pre-migration marker. Runs BEFORE the encryption pass so a
        // bad patch fails fast before we round-trip to the key
        // resolver.
        let hints = match system_fields_pass::apply_system_fields_on_update(
            &update,
            &app,
            &coll,
        ) {
            Ok(h) => h,
            Err(e) => {
                return OpResult::JsValue {
                    resolver,
                    value: ResolveValue::RejectError(e.to_op_error()),
                    request_id,
                };
            }
        };
        // **P7 PR 4** — detect creator-supplied CAS version + reject
        // the unsupported "version filter without id" shape eagerly.
        let cas_version = system_fields_pass::extract_cas_version(&filter);
        if cas_version.is_some() && !system_fields_pass::filter_has_id_predicate(&filter) {
            return OpResult::JsValue {
                resolver,
                value: ResolveValue::RejectError(
                    DbError::multi_row_version_filter_unsupported(&coll).to_op_error(),
                ),
                request_id,
            };
        }

        let mut update = update;
        if let Err(e) = apply_encryption_on_update(&app, &coll, &filter, &mut update).await {
            return OpResult::JsValue {
                resolver,
                value: ResolveValue::RejectError(e.to_op_error()),
                request_id,
            };
        }
        maybe_lower_sqlite_boolean_update(&app, &coll, &mut update);
        let mut sql_filter = filter.clone();
        maybe_lower_sqlite_boolean_filter(&app, &coll, &mut sql_filter);
        // **P7 PR 4** — auto-bump via the system-fields-aware builder.
        // Actor flows into the `updated_by` bind; the `hints` from the
        // pre-pass tell the builder which auto-bumps to suppress.
        let autobump = query::SystemFieldAutoBump {
            actor_id: actor_id.as_deref(),
            skip_version: hints.creator_supplied_version,
            skip_updated_at: hints.creator_supplied_updated_at,
            skip_updated_by: hints.creator_supplied_updated_by,
        };
        let built = query::build_update_one_with_system_fields(
            &app,
            &coll,
            &sql_filter,
            &update,
            current_sql_dialect(),
            &autobump,
        );
        let bq = match built {
            Ok(bq) => bq,
            Err(e) => {
                return OpResult::JsValue {
                    resolver,
                    value: ResolveValue::RejectError(DbError::from(e).to_op_error()),
                    request_id,
                };
            }
        };
        match exec_mutation_with_emit(bq, &app, &coll, crate::broker::ChangeOp::Update).await {
            Ok(rows) => {
                let rows = match normalize_rows_on_read(&app, &coll, rows) {
                    Ok(rows) => rows,
                    Err(e) => {
                        return OpResult::JsValue {
                            resolver,
                            value: ResolveValue::RejectError(e.to_op_error()),
                            request_id,
                        };
                    }
                };
                // **P7 PR 4** — optimistic-concurrency check. When the
                // creator supplied a `version: N` predicate AND the
                // RETURNING set is empty, classify as a CAS failure
                // (the row exists at a different version, or the row
                // is missing — the SDK consumer retries either way).
                if let Some(expected_version) = cas_version {
                    if rows.is_empty() {
                        let row_id = filter
                            .as_object()
                            .and_then(|o| o.get("id"))
                            .and_then(|v| v.as_str());
                        return OpResult::JsValue {
                            resolver,
                            value: ResolveValue::RejectError(
                                DbError::version_mismatch(&coll, row_id, expected_version)
                                    .to_op_error(),
                            ),
                            request_id,
                        };
                    }
                    // The `id` PK ensures at most one row matches
                    // `{ id: ..., version: N }`; a result set >1 is
                    // a regression in the dispatcher contract.
                    if rows.len() > 1 {
                        tracing::error!(
                            collection = %coll,
                            row_count = rows.len(),
                            "version_mismatch_unexpected_multi_row: CAS update returned >1 row"
                        );
                        return OpResult::JsValue {
                            resolver,
                            value: ResolveValue::RejectError(
                                DbError::internal("version_mismatch_unexpected_multi_row")
                                    .to_op_error(),
                            ),
                            request_id,
                        };
                    }
                }
                let rows = match apply_encryption_on_read(&app, &coll, rows).await {
                    Ok(r) => r,
                    Err(e) => {
                        return OpResult::JsValue {
                            resolver,
                            value: ResolveValue::RejectError(e.to_op_error()),
                            request_id,
                        };
                    }
                };
                // **P5.5 PR 3** — wrap masked columns from RETURNING *.
                let (rows, has_masked) = match apply_mask_wrap_on_read(&app, &coll, rows) {
                    Ok(r) => r,
                    Err(e) => {
                        return OpResult::JsValue {
                            resolver,
                            value: ResolveValue::RejectError(e.to_op_error()),
                            request_id,
                        };
                    }
                };
                OpResult::JsValue {
                    resolver,
                    value: first_row_or_null_masked(rows, has_masked),
                    request_id,
                }
            }
            Err(e) => OpResult::JsValue {
                resolver,
                value: ResolveValue::RejectError(e.to_op_error()),
                request_id,
            },
        }
    }));

    promise
}

/// Shared dispatch for `updateMany`. Resolves with the count of
/// affected rows as a JS `number`.
///
/// **P7 PR 4** — same auto-bump rules as `dispatch_update_one`. CAS
/// semantics don't generalise to multi-row UPDATEs (the affected-row
/// count conflates "row missing" / "version mismatched" / "filter
/// didn't match"), so a `version` filter without `id` predicate
/// refuses eagerly with `multi_row_version_filter_unsupported`. The
/// pre-PR-4 contract that returned the affected-row count as a plain
/// number is preserved on the success path.
pub(crate) fn dispatch_update_many<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    app_id: &str,
    collection: &str,
    filter: Value,
    update: Value,
) -> v8::Local<'s, v8::Promise> {
    let state = runtime_state(scope);
    let (resolver, request_id, promise) = setup_js_promise(scope, &state);

    let coll = collection.to_string();
    let app = app_id.to_string();

    // **P7 PR 4** — actor read at sync boundary (mirrors
    // `dispatch_update_one`'s rationale).
    let actor_id = system_fields_pass::current_actor_id(&state);

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        // **P7 PR 4** — same immutable-field + marker checks as
        // updateOne. Also refuse multi-row CAS UPDATE eagerly.
        let hints = match system_fields_pass::apply_system_fields_on_update(
            &update,
            &app,
            &coll,
        ) {
            Ok(h) => h,
            Err(e) => {
                return OpResult::JsValue {
                    resolver,
                    value: ResolveValue::RejectError(e.to_op_error()),
                    request_id,
                };
            }
        };
        let cas_version = system_fields_pass::extract_cas_version(&filter);
        if cas_version.is_some() && !system_fields_pass::filter_has_id_predicate(&filter) {
            return OpResult::JsValue {
                resolver,
                value: ResolveValue::RejectError(
                    DbError::multi_row_version_filter_unsupported(&coll).to_op_error(),
                ),
                request_id,
            };
        }

        let mut update = update;
        if let Err(e) = apply_encryption_on_update(&app, &coll, &filter, &mut update).await {
            return OpResult::JsValue {
                resolver,
                value: ResolveValue::RejectError(e.to_op_error()),
                request_id,
            };
        }
        let autobump = query::SystemFieldAutoBump {
            actor_id: actor_id.as_deref(),
            skip_version: hints.creator_supplied_version,
            skip_updated_at: hints.creator_supplied_updated_at,
            skip_updated_by: hints.creator_supplied_updated_by,
        };
        maybe_lower_sqlite_boolean_update(&app, &coll, &mut update);
        let mut sql_filter = filter.clone();
        maybe_lower_sqlite_boolean_filter(&app, &coll, &mut sql_filter);
        let built = query::build_update_many_with_system_fields(
            &app,
            &coll,
            &sql_filter,
            &update,
            current_sql_dialect(),
            &autobump,
        );
        let bq = match built {
            Ok(bq) => bq,
            Err(e) => {
                return OpResult::JsValue {
                    resolver,
                    value: ResolveValue::RejectError(DbError::from(e).to_op_error()),
                    request_id,
                };
            }
        };
        match exec_mutation_with_emit(bq, &app, &coll, crate::broker::ChangeOp::Update).await {
            Ok(rows) => {
                // CAS path on updateMany: with `{ id, version: N }` the
                // RETURNING is at most one row. Same empty-check as
                // updateOne so the SDK's CAS contract holds for both
                // entry points.
                if let Some(expected_version) = cas_version {
                    if rows.is_empty() {
                        let row_id = filter
                            .as_object()
                            .and_then(|o| o.get("id"))
                            .and_then(|v| v.as_str());
                        return OpResult::JsValue {
                            resolver,
                            value: ResolveValue::RejectError(
                                DbError::version_mismatch(&coll, row_id, expected_version)
                                    .to_op_error(),
                            ),
                            request_id,
                        };
                    }
                }
                OpResult::JsValue {
                    resolver,
                    value: row_count_as_f64(rows),
                    request_id,
                }
            }
            Err(e) => OpResult::JsValue {
                resolver,
                value: ResolveValue::RejectError(e.to_op_error()),
                request_id,
            },
        }
    }));

    promise
}

// ---------------------------------------------------------------------------
// deleteOne / deleteMany / purge / restore — write paths
//
// **P7 PR 5** — `delete()` becomes soft-delete on post-migration tables
// (Path C from §11 of the proposal). The dispatch helpers route through
// `system_fields_pass::schema_has_system_fields_marker` to decide:
//
//   - Marker present → soft-delete via
//     `build_soft_delete_*_with_system_fields`. The emitted broker
//     event is `ChangeOp::Update` (soft-delete IS an UPDATE setting
//     `deleted_at`) — see §6 of the proposal.
//   - Marker absent → legacy hard `DELETE` with a one-shot
//     `tracing::warn!` on the `zeroship_plugin_db::soft_delete_legacy`
//     target.
//
// `purge()` (new in PR 5) always hard-deletes regardless of marker
// state. `restore()` (also new) clears `deleted_at` on a soft-deleted
// row.
// ---------------------------------------------------------------------------

/// Shared dispatch for `deleteOne`. See [`dispatch_insert`] for the
/// capability-gate contract.
///
/// **P7 PR 5** — Path C semantics: soft-delete on post-migration
/// tables, legacy hard-delete with `tracing::warn!` on pre-migration
/// tables.
pub(crate) fn dispatch_delete_one<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    app_id: &str,
    collection: &str,
    filter: Value,
) -> v8::Local<'s, v8::Promise> {
    let state = runtime_state(scope);
    let (resolver, request_id, promise) = setup_js_promise(scope, &state);

    let coll = collection.to_string();
    let app = app_id.to_string();
    let actor_id = system_fields_pass::current_actor_id(&state);
    let has_marker = system_fields_pass::schema_has_system_fields_marker(&app, &coll);

    if has_marker {
        let mut filter = filter;
        maybe_lower_sqlite_boolean_filter(&app, &coll, &mut filter);
        let autobump = query::SystemFieldAutoBump {
            actor_id: actor_id.as_deref(),
            ..Default::default()
        };
        let built = query::build_soft_delete_one_with_system_fields(
            &app,
            &coll,
            &filter,
            current_sql_dialect(),
            &autobump,
        );
        state.borrow_mut().spawned_ops.push(Box::pin(run_op(
            resolver,
            request_id,
            built,
            move |bq| async move {
                // Tagged as Update because soft-delete IS an UPDATE
                // setting `deleted_at`. Subscribers wanting to react
                // to soft-deletes inspect `new_tuple.deleted_at`.
                let rows =
                    exec_mutation_with_emit(bq, &app, &coll, crate::broker::ChangeOp::Update)
                        .await?;
                normalize_rows_on_read(&app, &coll, rows)
            },
            first_row_or_null,
        )));
    } else {
        system_fields_pass::warn_legacy_hard_delete(&app, &coll);
        let mut filter = filter;
        maybe_lower_sqlite_boolean_filter(&app, &coll, &mut filter);
        let built = query::build_delete_one_with_dialect(
            &app,
            &coll,
            &filter,
            current_sql_dialect(),
        );
        state.borrow_mut().spawned_ops.push(Box::pin(run_op(
            resolver,
            request_id,
            built,
            move |bq| async move {
                let rows =
                    exec_mutation_with_emit(bq, &app, &coll, crate::broker::ChangeOp::Delete)
                        .await?;
                normalize_rows_on_read(&app, &coll, rows)
            },
            first_row_or_null,
        )));
    }

    promise
}

/// Shared dispatch for `deleteMany`. Resolves with the count of
/// affected rows as a JS `number`.
///
/// **P7 PR 5** — same Path C semantics as [`dispatch_delete_one`].
pub(crate) fn dispatch_delete_many<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    app_id: &str,
    collection: &str,
    filter: Value,
) -> v8::Local<'s, v8::Promise> {
    let state = runtime_state(scope);
    let (resolver, request_id, promise) = setup_js_promise(scope, &state);

    let coll = collection.to_string();
    let app = app_id.to_string();
    let actor_id = system_fields_pass::current_actor_id(&state);
    let has_marker = system_fields_pass::schema_has_system_fields_marker(&app, &coll);

    if has_marker {
        let mut filter = filter;
        maybe_lower_sqlite_boolean_filter(&app, &coll, &mut filter);
        let autobump = query::SystemFieldAutoBump {
            actor_id: actor_id.as_deref(),
            ..Default::default()
        };
        let built = query::build_soft_delete_many_with_system_fields(
            &app,
            &coll,
            &filter,
            current_sql_dialect(),
            &autobump,
        );
        state.borrow_mut().spawned_ops.push(Box::pin(run_op(
            resolver,
            request_id,
            built,
            move |bq| async move {
                exec_mutation_with_emit(bq, &app, &coll, crate::broker::ChangeOp::Update).await
            },
            row_count_as_f64,
        )));
    } else {
        system_fields_pass::warn_legacy_hard_delete(&app, &coll);
        let mut filter = filter;
        maybe_lower_sqlite_boolean_filter(&app, &coll, &mut filter);
        let built = query::build_delete_many(&app, &coll, &filter);
        state.borrow_mut().spawned_ops.push(Box::pin(run_op(
            resolver,
            request_id,
            built,
            move |bq| async move {
                exec_mutation_with_emit(bq, &app, &coll, crate::broker::ChangeOp::Delete).await
            },
            row_count_as_f64,
        )));
    }

    promise
}

/// **P7 PR 5** — explicit hard-delete entry point. Always emits
/// `DELETE FROM ...` regardless of marker state. Used by the SDK's
/// `purge(filter)` for compliance / right-to-be-forgotten flows.
///
/// `purge` does NOT respect the `deleted_at IS NULL` auto-filter —
/// it removes both live and soft-deleted rows matching the filter.
pub(crate) fn dispatch_purge_one<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    app_id: &str,
    collection: &str,
    filter: Value,
) -> v8::Local<'s, v8::Promise> {
    let state = runtime_state(scope);
    let (resolver, request_id, promise) = setup_js_promise(scope, &state);

    let mut filter = filter;
    maybe_lower_sqlite_boolean_filter(app_id, collection, &mut filter);
    let built =
        query::build_delete_one_with_dialect(app_id, collection, &filter, current_sql_dialect());
    let coll = collection.to_string();
    let app = app_id.to_string();

    state.borrow_mut().spawned_ops.push(Box::pin(run_op(
        resolver,
        request_id,
        built,
        move |bq| async move {
            let rows =
                exec_mutation_with_emit(bq, &app, &coll, crate::broker::ChangeOp::Delete)
                    .await?;
            normalize_rows_on_read(&app, &coll, rows)
        },
        first_row_or_null,
    )));

    promise
}

/// **P7 PR 5** — bulk-purge entry point.
pub(crate) fn dispatch_purge_many<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    app_id: &str,
    collection: &str,
    filter: Value,
) -> v8::Local<'s, v8::Promise> {
    let state = runtime_state(scope);
    let (resolver, request_id, promise) = setup_js_promise(scope, &state);

    let mut filter = filter;
    maybe_lower_sqlite_boolean_filter(app_id, collection, &mut filter);
    let built = query::build_delete_many(app_id, collection, &filter);
    let coll = collection.to_string();
    let app = app_id.to_string();

    state.borrow_mut().spawned_ops.push(Box::pin(run_op(
        resolver,
        request_id,
        built,
        move |bq| async move {
            exec_mutation_with_emit(bq, &app, &coll, crate::broker::ChangeOp::Delete).await
        },
        row_count_as_f64,
    )));

    promise
}

/// **P7 PR 5** — restore a soft-deleted row. Refuses with
/// `restore_unsupported_legacy_table` when the cached schema lacks
/// the system-fields marker.
pub(crate) fn dispatch_restore_one<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    app_id: &str,
    collection: &str,
    filter: Value,
) -> v8::Local<'s, v8::Promise> {
    let state = runtime_state(scope);
    let (resolver, request_id, promise) = setup_js_promise(scope, &state);

    let coll = collection.to_string();
    let app = app_id.to_string();
    let actor_id = system_fields_pass::current_actor_id(&state);
    let has_marker = system_fields_pass::schema_has_system_fields_marker(&app, &coll);

    if !has_marker {
        let err = DbError::restore_unsupported_legacy_table(&coll);
        state.borrow_mut().spawned_ops.push(Box::pin(async move {
            OpResult::JsValue {
                resolver,
                value: ResolveValue::RejectError(err.to_op_error()),
                request_id,
            }
        }));
        return promise;
    }

    let autobump = query::SystemFieldAutoBump {
        actor_id: actor_id.as_deref(),
        ..Default::default()
    };
    let mut filter = filter;
    maybe_lower_sqlite_boolean_filter(&app, &coll, &mut filter);
    let built = query::build_restore_one_with_system_fields(
        &app,
        &coll,
        &filter,
        current_sql_dialect(),
        &autobump,
    );
    state.borrow_mut().spawned_ops.push(Box::pin(run_op(
        resolver,
        request_id,
        built,
        move |bq| async move {
            let rows =
                exec_mutation_with_emit(bq, &app, &coll, crate::broker::ChangeOp::Update).await?;
            normalize_rows_on_read(&app, &coll, rows)
        },
        first_row_or_null,
    )));

    promise
}

/// **P7 PR 5** — bulk-restore entry point.
pub(crate) fn dispatch_restore_many<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    app_id: &str,
    collection: &str,
    filter: Value,
) -> v8::Local<'s, v8::Promise> {
    let state = runtime_state(scope);
    let (resolver, request_id, promise) = setup_js_promise(scope, &state);

    let coll = collection.to_string();
    let app = app_id.to_string();
    let actor_id = system_fields_pass::current_actor_id(&state);
    let has_marker = system_fields_pass::schema_has_system_fields_marker(&app, &coll);

    if !has_marker {
        let err = DbError::restore_unsupported_legacy_table(&coll);
        state.borrow_mut().spawned_ops.push(Box::pin(async move {
            OpResult::JsValue {
                resolver,
                value: ResolveValue::RejectError(err.to_op_error()),
                request_id,
            }
        }));
        return promise;
    }

    let autobump = query::SystemFieldAutoBump {
        actor_id: actor_id.as_deref(),
        ..Default::default()
    };
    let mut filter = filter;
    maybe_lower_sqlite_boolean_filter(&app, &coll, &mut filter);
    let built = query::build_restore_many_with_system_fields(
        &app,
        &coll,
        &filter,
        current_sql_dialect(),
        &autobump,
    );
    state.borrow_mut().spawned_ops.push(Box::pin(run_op(
        resolver,
        request_id,
        built,
        move |bq| async move {
            exec_mutation_with_emit(bq, &app, &coll, crate::broker::ChangeOp::Update).await
        },
        row_count_as_f64,
    )));

    promise
}

// ---------------------------------------------------------------------------
// aggregate / distinct / count — read paths
// ---------------------------------------------------------------------------

/// Shared dispatch for `aggregate`. Pipeline is a JSON array of stage
/// objects.
///
/// **P7 PR 5** — `opts.include_deleted: true` opts out of the auto
/// soft-delete `$match` (per Q-SF-J in the proposal — every read-side
/// method auto-filters for consistency).
pub(crate) fn dispatch_aggregate<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    app_id: &str,
    collection: &str,
    pipeline: Value,
    opts: Value,
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

    let include_deleted = opts
        .get("include_deleted")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let filter_soft_deleted =
        system_fields_pass::should_filter_soft_deleted(app_id, collection, include_deleted);

    let (resolver, request_id, promise) = setup_js_promise(scope, &state);
    let app = app_id.to_string();
    let coll = collection.to_string();
    let built = query::build_aggregate_with_soft_delete_with_dialect(
        app_id,
        collection,
        &pipeline,
        filter_soft_deleted,
        current_sql_dialect(),
    );

    state.borrow_mut().spawned_ops.push(Box::pin(run_op(
        resolver,
        request_id,
        built,
        move |bq| async move {
            let rows = exec_query(bq).await?;
            normalize_rows_on_read(&app, &coll, rows)
        },
        rows_as_json_array,
    )));

    promise
}

/// Shared dispatch for `distinct`. `field` is the column name; `filter`
/// is the WHERE-clause JSON.
///
/// **P7 PR 5** — `opts.include_deleted: true` opts out of the auto-
/// filter; see [`dispatch_find`].
pub(crate) fn dispatch_distinct<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    app_id: &str,
    collection: &str,
    field: &str,
    filter: Value,
    opts: Value,
) -> v8::Local<'s, v8::Promise> {
    let state = runtime_state(scope);
    let (resolver, request_id, promise) = setup_js_promise(scope, &state);

    let include_deleted = opts
        .get("include_deleted")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let filter_soft_deleted =
        system_fields_pass::should_filter_soft_deleted(app_id, collection, include_deleted);
    let mut filter = filter;
    maybe_lower_sqlite_boolean_filter(app_id, collection, &mut filter);
    let app = app_id.to_string();
    let coll = collection.to_string();
    let built = query::build_distinct_with_soft_delete_with_dialect(
        app_id,
        collection,
        field,
        &filter,
        filter_soft_deleted,
        current_sql_dialect(),
    );

    state.borrow_mut().spawned_ops.push(Box::pin(run_op(
        resolver,
        request_id,
        built,
        move |bq| async move {
            let rows = exec_query(bq).await?;
            normalize_rows_on_read(&app, &coll, rows)
        },
        |rows: Vec<Value>| {
            // Extract single-column values into a flat array. `rows`
            // is the pre-decoded result set — no JSON parse needed
            // before reshaping.
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
            ResolveValue::Json(Value::Array(flat).to_string())
        },
    )));

    promise
}

/// Shared dispatch for `count`. Resolves with a real JS `number`
/// (not a JSON-stringified integer).
///
/// **P7 PR 5** — `opts.include_deleted: true` opts out of the auto-
/// filter.
pub(crate) fn dispatch_count<'s>(
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

    let include_deleted = opts
        .get("include_deleted")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let filter_soft_deleted =
        system_fields_pass::should_filter_soft_deleted(app_id, collection, include_deleted);
    let mut filter = filter;
    maybe_lower_sqlite_boolean_filter(app_id, collection, &mut filter);

    let (resolver, request_id, promise) = setup_js_promise(scope, &state);
    let built =
        query::build_count_with_soft_delete(app_id, collection, &filter, filter_soft_deleted);

    state.borrow_mut().spawned_ops.push(Box::pin(run_op(
        resolver,
        request_id,
        built,
        exec_count,
        |n: i64| {
            #[allow(clippy::cast_precision_loss)]
            ResolveValue::F64(n as f64)
        },
    )));

    promise
}

// ---------------------------------------------------------------------------
// upsert — INSERT … ON CONFLICT path
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

    let mut doc = doc;
    maybe_lower_sqlite_boolean_doc(app_id, collection, &mut doc);
    let built = query::build_upsert_with_dialect(
        app_id,
        collection,
        &doc,
        &conflict_fields,
        current_sql_dialect(),
    );
    let coll = collection.to_string();
    let app = app_id.to_string();

    state.borrow_mut().spawned_ops.push(Box::pin(run_op(
        resolver,
        request_id,
        built,
        move |bq| async move {
            // Upsert can be either INSERT (new row) or UPDATE (existing).
            // We tag as Update because the subscriber's reaction is the
            // same — re-fetch. The proposal's read-set narrowing (P8b)
            // will distinguish; P8a doesn't need to.
            let rows =
                exec_mutation_with_emit(bq, &app, &coll, crate::broker::ChangeOp::Update).await?;
            normalize_rows_on_read(&app, &coll, rows)
        },
        first_row_or_null,
    )));

    promise
}

// ---------------------------------------------------------------------------
// search — vector / FTS unified entry point (P4)
// ---------------------------------------------------------------------------

/// Shared dispatch for `collection.search(args)` — the P4 unified
/// search entry. Inspects `args` for a discriminator key:
///
/// - `{ vector, k?, metric?, column?, filter? }` → pgvector
///   `VectorIndex::vector_search` (PG); SQLite returns a typed
///   `vector_unsupported` configuration error (PR 4 lands the SQLite
///   impl).
/// - `{ text, ... }` → reserved for P4 PR 3 FTS. PR 2 returns a typed
///   `fts_unsupported` until PR 3 lands.
///
/// Resolves with a JSON array of rows; each row carries the
/// `_distance` synthetic column from pgvector. Errors are coded
/// (`vector_extension_missing` / `vector_unsupported` / standard
/// SQLSTATE) so the SDK can branch on `e.code`.
pub(crate) fn dispatch_search<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    app_id: &str,
    collection: &str,
    args: Value,
) -> v8::Local<'s, v8::Promise> {
    use zeroship_runtime::state::OpError;

    let state = runtime_state(scope);
    let (resolver, request_id, promise) = setup_js_promise(scope, &state);

    // Discriminator: presence of `vector` selects the pgvector path.
    let has_vector = args.get("vector").is_some();
    let has_text = args.get("text").is_some();

    if !has_vector && !has_text {
        // Reject synchronously via the typed error path so the SDK sees
        // a coded error rather than a hang. Use `Configuration` because
        // the failure is shape-level, not data-level.
        let err = DbError::Configuration {
            code: "invalid_search_args",
            message: "search: args must include `vector` (P4 PR 2) or `text` (P4 PR 3+)"
                .to_string(),
            hint: Some(
                "pass `{ vector: number[], k?: number, metric?, column?, filter? }` for vector search"
                    .to_string(),
            ),
        };
        let op_err: OpError = err.to_op_error();
        state.borrow_mut().spawned_ops.push(Box::pin(async move {
            zeroship_runtime::state::OpResult::JsValue {
                resolver,
                value: zeroship_runtime::state::ResolveValue::RejectError(op_err),
                request_id,
            }
        }));
        return promise;
    }

    if has_text && !has_vector {
        // **P4 PR 3** — FTS branch. Pull the query string, limit, and
        // filter from args; route to `FullTextIndex::fts_search` on the
        // PG arm; SQLite returns `fts_unsupported` until PR 5 lands.
        let text_query = match args.get("text").and_then(Value::as_str) {
            Some(s) => s.to_string(),
            None => {
                let err = DbError::Configuration {
                    code: "invalid_text_arg",
                    message: "search: `text` must be a string".to_string(),
                    hint: None,
                };
                let op_err: OpError = err.to_op_error();
                state.borrow_mut().spawned_ops.push(Box::pin(async move {
                    zeroship_runtime::state::OpResult::JsValue {
                        resolver,
                        value: zeroship_runtime::state::ResolveValue::RejectError(op_err),
                        request_id,
                    }
                }));
                return promise;
            }
        };
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .map(|n| n as usize)
            .or_else(|| {
                // Accept the SDK's `k` alias too — the vector branch
                // uses `k` and the SDK passes the same name through for
                // FTS in some cases. The native trait signature carries
                // `limit: Option<usize>` either way.
                args.get("k").and_then(Value::as_u64).map(|n| n as usize)
            });
        let mut filter = args
            .get("filter")
            .cloned()
            .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
        let app = app_id.to_string();
        let coll = collection.to_string();
        maybe_lower_sqlite_boolean_filter(&app, &coll, &mut filter);

        state.borrow_mut().spawned_ops.push(Box::pin(async move {
            let backend = crate::context::with(|c| c.backend());
            let result: Result<Vec<Value>, DbError> = async {
                let backend = backend.ok_or_else(|| {
                    DbError::config("not_configured", "db: backend not initialized".to_string())
                })?;
                // **P4 PR 5** — SQLite arm routes through the FTS5
                // vtable + bm25 ranking. We short-circuit BEFORE the
                // PG path so a build with both arms compiled in
                // dispatches based on which arm the runtime is bound
                // to, not on Cargo-feature ordering.
                if let Some(sq) = backend.as_sqlite() {
                    use crate::backend::FullTextIndex as _;
                    return sq
                        .fts_search(&app, &coll, &text_query, &filter, limit)
                        .await;
                }
                let pg = backend
                    .as_postgres()
                    .ok_or_else(|| DbError::backend_unsupported("fts_search"))?;
                use crate::backend::FullTextIndex as _;
                pg.fts_search(&app, &coll, &text_query, &filter, limit).await
            }
            .await;

            match result {
                Ok(rows) => {
                    let rows = match normalize_rows_on_read(&app, &coll, rows) {
                        Ok(rows) => rows,
                        Err(e) => {
                            return zeroship_runtime::state::OpResult::JsValue {
                                resolver,
                                value: zeroship_runtime::state::ResolveValue::RejectError(
                                    e.to_op_error(),
                                ),
                                request_id,
                            };
                        }
                    };
                    zeroship_runtime::state::OpResult::JsValue {
                        resolver,
                        value: zeroship_runtime::state::ResolveValue::Json(
                            Value::Array(rows).to_string(),
                        ),
                        request_id,
                    }
                }
                Err(e) => zeroship_runtime::state::OpResult::JsValue {
                    resolver,
                    value: zeroship_runtime::state::ResolveValue::RejectError(e.to_op_error()),
                    request_id,
                },
            }
        }));
        return promise;
    }

    // Vector branch.
    //
    // Decode `vector` into `Vec<f32>`. Reject anything that's not a
    // homogeneous number array at the boundary so the impl can stay
    // typed.
    let vector: Vec<f32> = match args.get("vector").and_then(Value::as_array) {
        Some(arr) => {
            let mut v: Vec<f32> = Vec::with_capacity(arr.len());
            for elem in arr {
                if let Some(n) = elem.as_f64() {
                    v.push(n as f32);
                } else {
                    let err = DbError::Configuration {
                        code: "invalid_vector_arg",
                        message: "search: every element of `vector` must be a number"
                            .to_string(),
                        hint: None,
                    };
                    let op_err: OpError = err.to_op_error();
                    state.borrow_mut().spawned_ops.push(Box::pin(async move {
                        zeroship_runtime::state::OpResult::JsValue {
                            resolver,
                            value: zeroship_runtime::state::ResolveValue::RejectError(op_err),
                            request_id,
                        }
                    }));
                    return promise;
                }
            }
            v
        }
        None => {
            let err = DbError::Configuration {
                code: "invalid_vector_arg",
                message: "search: `vector` must be an array of numbers".to_string(),
                hint: None,
            };
            let op_err: OpError = err.to_op_error();
            state.borrow_mut().spawned_ops.push(Box::pin(async move {
                zeroship_runtime::state::OpResult::JsValue {
                    resolver,
                    value: zeroship_runtime::state::ResolveValue::RejectError(op_err),
                    request_id,
                }
            }));
            return promise;
        }
    };

    let k = args
        .get("k")
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .unwrap_or(10);
    let metric_str = args
        .get("metric")
        .and_then(Value::as_str)
        .unwrap_or("cosine");
    let metric = match metric_str {
        "l2" => crate::backend::VectorMetric::L2,
        "innerProduct" | "ip" => crate::backend::VectorMetric::InnerProduct,
        _ => crate::backend::VectorMetric::Cosine,
    };
    let column = args
        .get("column")
        .and_then(Value::as_str)
        .unwrap_or("embedding")
        .to_string();
    let mut filter = args
        .get("filter")
        .cloned()
        .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
    let app = app_id.to_string();
    let coll = collection.to_string();
    maybe_lower_sqlite_boolean_filter(&app, &coll, &mut filter);

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        // Reach the backend through the per-isolate context. The
        // dispatch helper isn't generic over the backend; runtime
        // wiring stashes a `BackendHandle` per isolate that we route
        // through the existing `as_postgres()` accessor.
        let backend = crate::context::with(|c| c.backend());
        let result: Result<Vec<Value>, DbError> = async {
            let backend = backend.ok_or_else(|| {
                DbError::config("not_configured", "db: backend not initialized".to_string())
            })?;
            let pg_path = || async {
                let pg = backend
                    .as_postgres()
                    .ok_or_else(|| DbError::backend_unsupported("vector_search"))?;
                use crate::backend::VectorIndex as _;
                pg.vector_search(&app, &coll, &column, &vector, k, metric, &filter)
                    .await
            };
            // **P4 PR 4** — SQLite arm routes through the pure-Rust
            // flat-scan `VectorIndex` impl on `SqliteBackend`. We
            // short-circuit BEFORE the PG path so a build with both
            // arms compiled in (`--features "pg sqlite"` for tests)
            // dispatches based on which arm the runtime is bound to,
            // not on Cargo-feature ordering.
            if let Some(sq) = backend.as_sqlite() {
                use crate::backend::VectorIndex as _;
                return sq
                    .vector_search(&app, &coll, &column, &vector, k, metric, &filter)
                    .await;
            }
            pg_path().await
        }
        .await;

        match result {
            Ok(rows) => {
                let rows = match normalize_rows_on_read(&app, &coll, rows) {
                    Ok(rows) => rows,
                    Err(e) => {
                        return zeroship_runtime::state::OpResult::JsValue {
                            resolver,
                            value: zeroship_runtime::state::ResolveValue::RejectError(
                                e.to_op_error(),
                            ),
                            request_id,
                        };
                    }
                };
                zeroship_runtime::state::OpResult::JsValue {
                    resolver,
                    value: zeroship_runtime::state::ResolveValue::Json(
                        Value::Array(rows).to_string(),
                    ),
                    request_id,
                }
            }
            Err(e) => zeroship_runtime::state::OpResult::JsValue {
                resolver,
                value: zeroship_runtime::state::ResolveValue::RejectError(e.to_op_error()),
                request_id,
            },
        }
    }));

    promise
}

/// Shared dispatch for the `Collection.near()` v8_method (P4 PR 3 — PG arm).
///
/// `args` shape (validated SDK-side):
/// ```js
/// { field: "location",
///   point: { lat: 51.5, lng: -0.1 },
///   radius: 1000,           // metres
///   filter?: {...},
///   limit?: 100 }
/// ```
///
/// Routes to `SpatialIndex::spatial_near` on the PG arm. SQLite returns
/// `spatial_unsupported` until P4 PR 5 lands the haversine impl. Each
/// returned row carries a synthetic `_distance_m` (`f64`) column.
pub(crate) fn dispatch_near<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    app_id: &str,
    collection: &str,
    args: Value,
) -> v8::Local<'s, v8::Promise> {
    use zeroship_runtime::state::OpError;

    let state = runtime_state(scope);
    let (resolver, request_id, promise) = setup_js_promise(scope, &state);

    // Local helper macro: reject the freshly-allocated promise with a
    // typed `DbError` and return early. We use a macro instead of a
    // closure because each call site needs to MOVE `resolver` (V8
    // `Global<PromiseResolver>` is not `Copy`) into the spawned future,
    // and the macro lets us early-return the same `promise` value the
    // outer scope keeps a reference to.
    macro_rules! reject_sync {
        ($err:expr) => {{
            let op_err: OpError = $err.to_op_error();
            state.borrow_mut().spawned_ops.push(Box::pin(async move {
                zeroship_runtime::state::OpResult::JsValue {
                    resolver,
                    value: zeroship_runtime::state::ResolveValue::RejectError(op_err),
                    request_id,
                }
            }));
            return promise;
        }};
    }

    let field = match args.get("field").and_then(Value::as_str) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => reject_sync!(DbError::Configuration {
            code: "invalid_near_args",
            message: "near: `field` must be a non-empty string".to_string(),
            hint: Some("pass `{ field, point, radius, filter?, limit? }`".to_string()),
        }),
    };

    let point_obj = args.get("point");
    let lat = point_obj.and_then(|p| p.get("lat")).and_then(Value::as_f64);
    let lng = point_obj.and_then(|p| p.get("lng")).and_then(Value::as_f64);
    let (lat, lng) = match (lat, lng) {
        (Some(la), Some(ln)) => (la, ln),
        _ => reject_sync!(DbError::Configuration {
            code: "invalid_near_args",
            message: "near: `point` must be `{ lat: number, lng: number }`".to_string(),
            hint: None,
        }),
    };
    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lng) {
        reject_sync!(DbError::Configuration {
            code: "invalid_near_args",
            message: format!(
                "near: `point` out of range: lat must be in [-90,90] and lng in [-180,180], got lat={lat} lng={lng}"
            ),
            hint: None,
        });
    }

    let radius_m = match args.get("radius").and_then(Value::as_f64) {
        Some(r) if r > 0.0 && r.is_finite() => r,
        _ => reject_sync!(DbError::Configuration {
            code: "invalid_near_args",
            message: "near: `radius` must be a positive number (metres)".to_string(),
            hint: None,
        }),
    };

    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .map(|n| n as usize);
    let mut filter = args
        .get("filter")
        .cloned()
        .unwrap_or_else(|| Value::Object(serde_json::Map::new()));

    let app = app_id.to_string();
    let coll = collection.to_string();
    maybe_lower_sqlite_boolean_filter(&app, &coll, &mut filter);
    let point = crate::backend::GeoPoint { lat, lng };

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        let backend = crate::context::with(|c| c.backend());
        let result: Result<Vec<Value>, DbError> = async {
            let backend = backend.ok_or_else(|| {
                DbError::config("not_configured", "db: backend not initialized".to_string())
            })?;
            // **P4 PR 5** — SQLite arm routes through the pure-Rust
            // haversine flat-scan `SpatialIndex` impl on
            // `SqliteBackend`. Short-circuit BEFORE the PG path so a
            // build with both arms compiled in dispatches based on
            // which arm the runtime is bound to.
            if let Some(sq) = backend.as_sqlite() {
                use crate::backend::SpatialIndex as _;
                return sq
                    .spatial_near(&app, &coll, &field, point, radius_m, &filter, limit)
                    .await;
            }
            let pg = backend
                .as_postgres()
                .ok_or_else(|| DbError::backend_unsupported("spatial_near"))?;
            use crate::backend::SpatialIndex as _;
            pg.spatial_near(&app, &coll, &field, point, radius_m, &filter, limit)
                .await
        }
        .await;

        match result {
            Ok(rows) => {
                let rows = match normalize_rows_on_read(&app, &coll, rows) {
                    Ok(rows) => rows,
                    Err(e) => {
                        return zeroship_runtime::state::OpResult::JsValue {
                            resolver,
                            value: zeroship_runtime::state::ResolveValue::RejectError(
                                e.to_op_error(),
                            ),
                            request_id,
                        };
                    }
                };
                zeroship_runtime::state::OpResult::JsValue {
                    resolver,
                    value: zeroship_runtime::state::ResolveValue::Json(
                        Value::Array(rows).to_string(),
                    ),
                    request_id,
                }
            }
            Err(e) => zeroship_runtime::state::OpResult::JsValue {
                resolver,
                value: zeroship_runtime::state::ResolveValue::RejectError(e.to_op_error()),
                request_id,
            },
        }
    }));

    promise
}

// P9 PR 1: `dispatch_find_or_create` was deleted along with the
// `Collection.findOrCreate` v8_method (absorbed by `upsert`). The
// `query::build_find_or_create` SQL builder stays for now —
// `upsert({where, create})` shape lands in a follow-up.

// ===========================================================================
// P5 PR 2 — transparent column encryption hooks
// ===========================================================================

/// **P5 PR 2** — encrypt every `t.encrypted(...)`-declared column on
/// `doc` before the query builder reads it. Short-circuits when:
///   - the cached schema for `(app_id, collection)` is absent (the
///     collection wasn't registered on this isolate yet), OR
///   - no column on the schema carries the `encrypted` metadata.
///
/// `row_pk` defaults to `doc["id"]` (typed_ids minted SDK-side per
/// Camp A, ALWAYS available before INSERT). When the doc has no `id`
/// field (e.g. partial update), the empty string is used; this is OK
/// because the encryption pass also runs on UPDATE where row_pk comes
/// from the filter — and a Randomised column with an empty row_pk
/// would still encrypt consistently (the AAD just doesn't bind a row
/// identity, which is a known limitation for callers who explicitly
/// pass a doc without an `id`).
///
/// **P5.5 PR 2** — also runs the mask pass after the encryption pass
/// using the plaintext sidechannel produced by
/// `encrypt_row_on_write_with_sidechannel`. The mask pass appends
/// `<col>_masked` siblings to `doc` for every masked column; the SQL
/// builder picks them up naturally because the row is iterated as a
/// map. Non-encrypted-but-masked columns are handled by the mask pass
/// reading `doc[col]` directly (empty sidechannel for those columns).
async fn apply_encryption_on_write(
    app_id: &str,
    collection: &str,
    doc: &mut Value,
) -> Result<(), DbError> {
    let Some(schema) = crate::context::with(|c| c.schema_for(app_id, collection)) else {
        return Ok(());
    };
    let has_enc = schema_has_encrypted_columns(&schema);
    let has_mask = schema_has_masked_columns(&schema);
    if !has_enc && !has_mask {
        return Ok(());
    }
    // Row PK lookup: prefer `doc["id"]` (typed_id string), fall back to
    // empty. Numeric ids are also accepted for legacy collections.
    let row_pk = doc
        .get("id")
        .and_then(|v| match v {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        })
        .unwrap_or_default();
    let mut sidechannel = mask_pass::MaskPlaintextSidechannel::new();
    if has_enc {
        encryption_pass_dispatch(app_id, collection, &schema, &row_pk, doc, &mut sidechannel).await?;
    }
    if has_mask {
        mask_pass::apply_mask_on_write(&schema, &sidechannel, doc)?;
    }
    Ok(())
}

/// **P5 PR 2** — UPDATE variant that pulls `row_pk` from a filter
/// object (`{ id: ... }`). Used by `dispatch_update_one` and
/// `dispatch_update_many` so the AAD binds the target row's PK.
///
/// For multi-row updates (no `id` in the filter) `row_pk` falls back to
/// the empty string — the Randomised path will then encrypt under an
/// AAD that doesn't bind a specific row; the resulting ciphertext only
/// decrypts back if every target row carries the same row_pk on read
/// (which is generally NOT the case for bulk updates). The SDK's
/// filter-validation layer refuses range / regex predicates on
/// encrypted columns, but bulk-updating an encrypted column via
/// `{ status: "active" }` (a non-encrypted filter) → `{ ssn: "X" }` is
/// a footgun that we surface conservatively rather than silently
/// breaking decrypt later.
async fn apply_encryption_on_update(
    app_id: &str,
    collection: &str,
    filter: &Value,
    patch: &mut Value,
) -> Result<(), DbError> {
    let Some(schema) = crate::context::with(|c| c.schema_for(app_id, collection)) else {
        return Ok(());
    };
    let has_enc = schema_has_encrypted_columns(&schema);
    let has_mask = schema_has_masked_columns(&schema);
    if !has_enc && !has_mask {
        return Ok(());
    }
    let row_pk = filter
        .get("id")
        .and_then(|v| match v {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        })
        .unwrap_or_default();

    // Encrypt fields nested under `$set` if present, otherwise the
    // top-level field map. We mirror the SET-clause flattening the
    // build layer does. Same target object feeds both the encryption
    // pass and the mask pass so the sibling `<col>_masked` lands on
    // the same `$set` (or top-level) the SQL builder iterates.
    let target: &mut Value = if patch.get("$set").is_some() {
        patch.get_mut("$set").expect("checked above")
    } else {
        patch
    };
    let mut sidechannel = mask_pass::MaskPlaintextSidechannel::new();
    if has_enc {
        encryption_pass_dispatch(app_id, collection, &schema, &row_pk, target, &mut sidechannel).await?;
    }
    if has_mask {
        // **P5.5 PR 2** — mask pass derives `<col>_masked` ONLY for
        // columns that are present in the patch (`apply_mask_on_write`
        // skips absent fields). This achieves "when the parent column
        // is NOT in the UPDATE SET, don't touch the sibling" — partial
        // updates that don't touch a masked field leave the existing
        // sibling untouched on disk.
        mask_pass::apply_mask_on_write(&schema, &sidechannel, target)?;
    }
    Ok(())
}

/// Decrypt every encrypted column on each row of `rows`. Short-circuits
/// when the schema has no encrypted columns OR when not registered.
///
/// Read rows are normalised before this pass runs, so encrypted-column
/// values arrive in a lossless text envelope on both backends:
/// Postgres `BYTEA` now decodes to base64, and SQLite BLOBs are
/// base64-encoded by the typed-row adapter. The shared decrypt helper
/// accepts either the legacy `\x...` PG text shape or base64.
async fn apply_encryption_on_read(
    app_id: &str,
    collection: &str,
    mut rows: Vec<Value>,
) -> Result<Vec<Value>, DbError> {
    let Some(schema) = crate::context::with(|c| c.schema_for(app_id, collection)) else {
        return Ok(rows);
    };
    if !schema_has_encrypted_columns(&schema) {
        return Ok(rows);
    }
    let backend = crate::context::with(|c| c.backend())
        .ok_or_else(|| DbError::config("not_configured", "db: backend not initialized"))?;
    if let Some(pg) = backend.as_encrypted_column_pg() {
        for row in rows.iter_mut() {
            crate::crud::encryption_pass::decrypt_row_on_read(
                pg, app_id, collection, &schema, row,
            )
            .await?;
        }
        return Ok(rows);
    }
    if let Some(sq) = backend.as_encrypted_column_sqlite() {
        for row in rows.iter_mut() {
            crate::crud::encryption_pass::decrypt_row_on_read(
                sq, app_id, collection, &schema, row,
            )
            .await?;
        }
        return Ok(rows);
    }
    Ok(rows)
}

/// Run the write-side encryption pass over `doc` using the
/// backend-arm `EncryptedColumn` impl.
///
/// - **PG arm** (gated on `feature = "pg"`): goes through
///   `PostgresBackend`'s `EncryptedColumn` impl (PR 2). The SQL builder
///   emits `decode($N, 'base64')::bytea` so the BYTEA column receives
///   raw bytes.
/// - **SQLite arm** (gated on `feature = "sqlite"`, P5 PR 3.5): goes
///   through `SqliteBackend`'s `EncryptedColumn` impl (PR 3) using
///   env-var-sourced keys. The SQL builder (when called with
///   `SqlDialect::Sqlite`) emits `$N` and tags the encrypted-column
///   param with `SQLITE_ENC_BLOB_PREFIX`; the session actor binds the
///   raw bytes as a BLOB.
///
/// PR 3.5 closes the SQLite gap PR 3 left open — encrypted columns
/// now work end-to-end on both backends through the SDK's CRUD path.
///
/// If neither backend arm is compiled in (no `pg`, no `sqlite`) and the
/// schema declares an encrypted column, surface a typed Configuration
/// error so the SDK can branch on `.code` rather than silently writing
/// plaintext to the BYTEA/BLOB column.
async fn encryption_pass_dispatch(
    app_id: &str,
    collection: &str,
    schema: &Value,
    row_pk: &str,
    doc: &mut Value,
    sidechannel: &mut mask_pass::MaskPlaintextSidechannel,
) -> Result<(), DbError> {
    let backend = crate::context::with(|c| c.backend())
        .ok_or_else(|| DbError::config("not_configured", "db: backend not initialized"))?;
    if let Some(pg) = backend.as_encrypted_column_pg() {
        return crate::crud::encryption_pass::encrypt_row_on_write_with_sidechannel(
            pg, app_id, collection, schema, row_pk, doc, sidechannel,
        )
        .await;
    }
    if let Some(sq) = backend.as_encrypted_column_sqlite() {
        return crate::crud::encryption_pass::encrypt_row_on_write_with_sidechannel(
            sq, app_id, collection, schema, row_pk, doc, sidechannel,
        )
        .await;
    }
    if schema_has_encrypted_columns(schema) {
        return Err(DbError::Configuration {
            code: "column_encryption_unavailable",
            message: "db: no backend arm available for column encryption CRUD path".to_string(),
            hint: None,
        });
    }
    Ok(())
}

/// Cheap walk: does any field def on `schema` carry `encrypted`?
fn schema_has_encrypted_columns(schema: &Value) -> bool {
    schema
        .as_object()
        .map(|o| o.values().any(|def| def.get("encrypted").is_some()))
        .unwrap_or(false)
}

/// **P5.5 PR 3** — wrap masked columns on every row in `rows` so the
/// SDK can construct `MaskedValue<T>` from the wire payload.
///
/// Synchronous (no backend round-trip) — `mask_pass::wrap_row_on_read`
/// reads from the row map directly. Short-circuits when:
///   - the cached schema for `(app_id, collection)` is absent (the
///     collection wasn't registered on this isolate yet), OR
///   - the schema declares no masked columns.
///
/// Two row shapes are accepted (mirrors the `wrap_row_on_read` docs):
///
/// 1. **Aliased-SELECT** (`find`): the SELECT clause already
///    aliased `<col>_masked AS <col>` (via
///    `query::build_find_with_schema`). The parent slot carries the
///    masked string; no `<col>_masked` key is present on the row.
///    `wrap_row_on_read` wraps the parent slot in place.
///
/// 2. **RETURNING-`*`** (`insert`, `update_one`): the row carries both
///    the parent (ciphertext / plaintext) AND the sibling. The wrap
///    prefers the sibling's value, drops the sibling key, and wraps
///    the parent slot.
///
/// **P9 PR 2** — returns `(rows, has_masked)`. The `has_masked` flag is
/// `true` iff the schema declared at least one masked column (i.e. the
/// wrap pass ran and the rows may carry `__zsmask__` sentinels). The
/// caller threads this into the lowering helper so the result resolves
/// via `ResolveValue::JsonWithRehydration` — `JSON.parse` then a Rust
/// walk that mints native `MaskedValue` instances. When `false`, the
/// caller keeps the plain `ResolveValue::Json` path (no walk overhead).
fn apply_mask_wrap_on_read(
    app_id: &str,
    collection: &str,
    mut rows: Vec<Value>,
) -> Result<(Vec<Value>, bool), DbError> {
    let Some(schema) = crate::context::with(|c| c.schema_for(app_id, collection)) else {
        return Ok((rows, false));
    };
    if !schema_has_masked_columns(&schema) {
        return Ok((rows, false));
    }
    for row in rows.iter_mut() {
        crate::crud::mask_pass::wrap_row_on_read(&schema, collection, row)?;
    }
    Ok((rows, true))
}

/// **P5.5 PR 2** — cheap walk: does any field def on `schema` carry a
/// `mask` entry with `kind != "none"`? Drives the per-write decision
/// to invoke `mask_pass::apply_mask_on_write`. A `kind: "none"` opt-out
/// returns false (no sibling column to populate).
fn schema_has_masked_columns(schema: &Value) -> bool {
    schema
        .as_object()
        .map(|o| {
            o.values().any(|def| {
                def.get("mask")
                    .and_then(|v| v.as_object())
                    .map(|m| {
                        m.get("kind")
                            .and_then(|k| k.as_str())
                            .map(|k| k != "none")
                            .unwrap_or(true) // missing kind defaults to "full" → masked
                    })
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_row_on_read_coerces_sqlite_wire_shapes() {
        let schema = serde_json::json!({
            "active": { "type": "boolean" },
            "prefs": { "type": "object" },
            "avatar": { "type": "bytes" },
            "published_at": { "type": "date" }
        });
        let mut row = serde_json::json!({
            "active": 1,
            "prefs": "{\"theme\":\"dark\"}",
            "avatar": [222, 173, 190, 239],
            "published_at": "2026-05-24T12:34:56.789Z",
            "created_at": "2026-05-24 12:34:56"
        });

        normalize_row_on_read(Some(&schema), &mut row).expect("normalize");

        assert_eq!(row.get("active"), Some(&Value::Bool(true)));
        assert_eq!(row.pointer("/prefs/theme"), Some(&Value::String("dark".to_string())));
        assert_eq!(
            row.get("avatar"),
            Some(&Value::String(
                base64::engine::general_purpose::STANDARD.encode([222, 173, 190, 239]),
            )),
        );
        assert!(row.get("published_at").and_then(Value::as_i64).is_some());
        assert!(row.get("created_at").and_then(Value::as_i64).is_some());
    }

    #[test]
    fn parse_timestamp_millis_accepts_iso_z_and_variable_fraction() {
        let expected = 1_779_626_096_789i64;
        assert_eq!(
            parse_timestamp_millis("2026-05-24T12:34:56.789Z"),
            Some(expected)
        );
        assert_eq!(
            parse_timestamp_millis("2026-05-24T12:34:56.789123Z"),
            Some(expected)
        );
        assert_eq!(
            parse_timestamp_millis("2026-05-24T14:34:56.789+02:00"),
            Some(expected)
        );
    }

    #[test]
    fn lower_boolean_filter_with_schema_keeps_json_booleans_untouched() {
        let schema = serde_json::json!({
            "active": { "type": "boolean" },
            "payload": { "type": "json" }
        });
        let mut filter = serde_json::json!({
            "$and": [
                { "active": { "$in": [true, false] } },
                { "payload": true }
            ]
        });

        lower_boolean_filter_with_schema(&schema, &mut filter);

        assert_eq!(filter["$and"][0]["active"]["$in"], serde_json::json!([1, 0]));
        assert_eq!(filter["$and"][1]["payload"], Value::Bool(true));
    }

    #[test]
    fn normalize_row_on_read_skips_encrypted_columns() {
        let schema = serde_json::json!({
            "secret": {
                "type": "bytes",
                "encrypted": { "mode": "randomised", "wraps": "bytes" }
            }
        });
        let mut row = serde_json::json!({
            "secret": "c2VjcmV0"
        });

        normalize_row_on_read(Some(&schema), &mut row).expect("normalize");

        assert_eq!(row.get("secret"), Some(&Value::String("c2VjcmV0".to_string())));
    }
}
