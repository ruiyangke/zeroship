//! CRUD dispatch helpers — one entry point per Collection method.
//!
//! Each `dispatch_*` here is the bridge between a v8_class method on
//! `Collection` (`v8_classes::collection`) and the async exec layer in
//! [`crate::exec`]. The shape is consistent across every helper:
//!
//! 1. Grab the runtime state slot.
//! 2. Optionally record into the active read-set ([`crate::read_set`])
//!    for subscription narrowing.
//! 3. `setup_js_promise` — allocate the promise + resolver.
//! 4. Build the SQL via `crate::query::build_*`.
//! 5. Hand off to `run_op` — the async tail that drives the exec
//!    helper, resolves the promise with the appropriate `ResolveValue`,
//!    or rejects via `DbError::to_op_error` (carries `.code` for the
//!    SDK).
//!
//! The template collapses the bottom half so each helper is ~15 LOC of
//! intent — "which builder, which exec, which resolve". No new public
//! API: each helper stays `pub(crate)` and is called from
//! `v8_classes::collection`.
//!
//! The capability gate (`refuse_if_query_capability`) is enforced by
//! the v8_class methods *before* reaching the dispatch helper — write
//! ops trust their callers.

use std::future::Future;

use serde_json::Value;
use zeroship_runtime::state::{OpResult, ResolveValue};

use crate::binding::DbBinding;
use crate::error::DbError;
use crate::exec::{exec_count, exec_mutation_with_emit, exec_query};
use crate::query;
use crate::tx_route::TxRoute;
use crate::v8_bridge::{runtime_state, setup_js_promise};

// Transparent column-encryption pass. The helpers in
// this module (`encrypt_row_on_write` / `decrypt_row_on_read`) sit
// around `query::build_*` and `exec_query` respectively.
//
// Visibility: crate-private in release builds; `pub` under
// `test-helpers` so `tests/sqlite_integration.rs` can drive the
// helpers directly for the end-to-end encrypted-column
// CRUD round-trip test (the orchestrator's CRUD entry today is PG-only,
// so the SQLite e2e gate composes the helpers itself).
#[cfg(not(feature = "test-helpers"))]
pub(crate) mod encryption_pass;
#[cfg(feature = "test-helpers")]
pub mod encryption_pass;

// Sibling-column-based mask transforms + dual-write CRUD pass.
// Same visibility pattern as `encryption_pass` so integration tests
// can reach the helpers when `test-helpers` is on.
#[cfg(not(feature = "test-helpers"))]
pub(crate) mod mask_pass;
#[cfg(feature = "test-helpers")]
pub mod mask_pass;

// `unmask()` RPC dispatch + audit row writer.
// Same visibility pattern as the sibling passes so integration tests
// can exercise `dispatch_unmask` directly when `test-helpers` is on.
#[cfg(not(feature = "test-helpers"))]
pub(crate) mod unmask;
#[cfg(feature = "test-helpers")]
pub mod unmask;

// `defineMaskPolicy()` storage + dispatcher + cache.
// Same visibility pattern: integration tests reach into the helpers
// via the `test-helpers` gate to drive `dispatch_set_mask_policy`
// directly without standing up V8.
#[cfg(not(feature = "test-helpers"))]
pub(crate) mod mask_policy;
#[cfg(feature = "test-helpers")]
pub mod mask_policy;

pub(crate) use mask_policy::dispatch_set_mask_policy_field;
pub(crate) use unmask::{dispatch_bulk_unmask_field, dispatch_unmask_field};

// Mask backfill / rewrite / removal jobs driven by the
// register-model apply pipeline. Same visibility pattern: `pub` under
// `test-helpers` so the integration tests can drive the helpers
// directly without standing up the full orchestrator.
#[cfg(any(test, feature = "test-helpers"))]
pub mod mask_backfill;

// Drift detection: sample masked-column siblings vs.
// recomputed mask of decrypt(parent). Same visibility pattern so the
// SQLite + PG integration suites can drive `run_drift_check_*`
// directly via the `test-helpers` gate.
#[cfg(any(test, feature = "test-helpers"))]
pub mod mask_drift;

// INSERT-time auto-population of platform system fields
// (`id`, `created_by`, `updated_by`). Same visibility pattern as the
// sibling encryption / mask passes so the integration tests can drive
// `apply_system_fields_on_insert*` directly under the `test-helpers`
// gate.
#[cfg(not(feature = "test-helpers"))]
pub(crate) mod system_fields_pass;
#[cfg(feature = "test-helpers")]
pub mod system_fields_pass;

mod bytes_pass;
mod read_pipeline;
mod write_pipeline;

#[cfg(any(test, feature = "test-helpers"))]
#[allow(unused_imports)]
pub use write_pipeline::{
    reset_write_path_counters_for_tests, write_path_counters_for_tests, WritePathCounters,
};

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
/// schema-resolution + `query::build_*` chain. Builder errors are `QueryError`
/// → `DbError::ValidationFailed` via the `From` impl — the resulting JS error
/// carries `code = "invalid_filter"` / `"invalid_collection"` /
/// `"invalid_identifier"`. It is a `DbError` rather than a `QueryError` so the
/// same arm carries the descriptor's `collection_not_declared` refusal, which
/// the sync half of a dispatcher folds in ahead of the builder call.
///
/// `exec` runs against either the pool or the active
/// `ThreadDbContext::tx_conns` (transparently —
/// `exec::run_sql` already handles that).
///
/// `resolve` lowers the exec's success value to the V8-bound
/// `ResolveValue` shape (typically `Json` for arrays/objects, `F64`
/// for counts).
async fn run_op<R, EFut, Resolve>(
    resolver: v8::Global<v8::PromiseResolver>,
    request_id: Option<u64>,
    build_result: Result<query::BuiltQuery, DbError>,
    exec: impl FnOnce(query::BuiltQuery) -> EFut,
    resolve: Resolve,
) -> OpResult
where
    EFut: Future<Output = Result<R, DbError>>,
    Resolve: FnOnce(R) -> ResolveValue,
{
    let bq = match build_result {
        Ok(bq) => bq,
        Err(e) => return reject_op(resolver, request_id, e),
    };
    match exec(bq).await {
        Ok(v) => OpResult::JsValue {
            resolver,
            value: resolve(v),
            request_id,
        },
        Err(e) => reject_op(resolver, request_id, e),
    }
}

/// Record `(collection, filter)` into the active query's read-set.
///
/// Resolves the descriptor entry the predicate has to be lowered against - a
/// conjunct on a masked column compares against the mask, not the value the
/// caller wrote. An undeclared collection records nothing rather than recording
/// an unlowered predicate: the dispatch this call precedes is about to reject
/// with `collection_not_declared`, so there is no subscription to narrow, and a
/// predicate built without a schema is exactly the silent false negative
/// `read_set` refuses to produce.
fn record_read_set(binding: &DbBinding, collection: &str, filter: &Value) {
    if !crate::read_set::is_active() {
        return;
    }
    let Ok(schema) = crate::descriptor::collection_schema(binding, collection) else {
        return;
    };
    crate::read_set::record_if_active(collection, filter, &schema);
}

fn current_sql_dialect() -> query::SqlDialect {
    match crate::context::with(|c| c.backend()) {
        Some(crate::backend::BackendHandle::Sqlite(_)) => query::SqlDialect::Sqlite,
        _ => query::SqlDialect::Postgres,
    }
}

// The four `maybe_lower_sqlite_boolean_*` helpers take the ALREADY-RESOLVED
// descriptor entry rather than resolving one of their own. Each dispatch site
// needs the schema anyway - the read builders take it, and the write pipeline
// keys its encrypt/mask stages off it - so resolving once per operation both
// removes a second store lookup and puts the `collection_not_declared` refusal
// at ONE place per dispatch instead of silently returning here.

fn maybe_lower_sqlite_boolean_doc(schema: &Value, doc: &mut Value) {
    if current_sql_dialect() != query::SqlDialect::Sqlite {
        return;
    }
    lower_boolean_doc_with_schema(schema, doc);
}

fn maybe_lower_sqlite_boolean_docs(schema: &Value, docs: &mut Value) {
    if current_sql_dialect() != query::SqlDialect::Sqlite {
        return;
    }
    let Some(arr) = docs.as_array_mut() else {
        return;
    };
    for doc in arr {
        lower_boolean_doc_with_schema(schema, doc);
    }
}

fn maybe_lower_sqlite_boolean_update(schema: &Value, patch: &mut Value) {
    if current_sql_dialect() != query::SqlDialect::Sqlite {
        return;
    }
    lower_boolean_update_with_schema(schema, patch);
}

fn maybe_lower_sqlite_boolean_filter(schema: &Value, filter: &mut Value) {
    if current_sql_dialect() != query::SqlDialect::Sqlite {
        return;
    }
    lower_boolean_filter_with_schema(schema, filter);
}

/// Pack a `DbError` into the rejection an already-allocated promise resolves
/// with. The dispatchers below hit this shape once per failure arm; naming it
/// keeps the schema-resolution arm to two lines.
fn reject_op(
    resolver: v8::Global<v8::PromiseResolver>,
    request_id: Option<u64>,
    err: DbError,
) -> OpResult {
    OpResult::JsValue {
        resolver,
        value: ResolveValue::RejectError(err.to_op_error()),
        request_id,
    }
}

fn lower_boolean_doc_with_schema(schema: &Value, doc: &mut Value) {
    let Some(obj) = doc.as_object_mut() else {
        return;
    };
    for (field, value) in obj {
        if field.starts_with("__zsbin__") {
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
        if field.starts_with('$') || field.starts_with("__zsbin__") {
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

fn schema_field<'a>(schema: &'a Value, field: &str) -> Option<&'a Value> {
    schema.as_object()?.get(field)
}

fn sqlite_blob_param(bytes: &[u8]) -> String {
    use base64::Engine as _;

    format!(
        "{}{}",
        crate::query::SQLITE_BINARY_BIND_PREFIX,
        base64::engine::general_purpose::STANDARD.encode(bytes),
    )
}

fn encode_sqlite_binary_scalar(field: &str, field_def: &Value, value: &mut Value) -> Result<(), DbError> {
    if value.is_null() {
        return Ok(());
    }
    if matches!(value, Value::String(s) if s.starts_with(crate::query::SQLITE_BINARY_BIND_PREFIX)) {
        return Ok(());
    }

    match field_def.get("type").and_then(Value::as_str) {
        Some("vector") => {
            let dims = field_def
                .get("vectorDims")
                .and_then(Value::as_u64)
                .ok_or_else(|| {
                    DbError::internal(format!(
                        "sqlite vector write encoding: schema for '{field}' is missing vectorDims"
                    ))
                })? as usize;
            let arr = value.as_array().ok_or_else(|| {
                DbError::validation(
                    "invalid_vector_arg",
                    format!("db: vector column '{field}' must be a number[]"),
                )
            })?;
            if arr.len() != dims {
                return Err(DbError::validation(
                    "vector_dimension_mismatch",
                    format!(
                        "db: vector column '{field}' expected {dims} dimensions, got {}",
                        arr.len()
                    ),
                ));
            }
            let mut vector = Vec::with_capacity(dims);
            for item in arr {
                let n = item.as_f64().ok_or_else(|| {
                    DbError::validation(
                        "invalid_vector_arg",
                        format!("db: vector column '{field}' must contain only numbers"),
                    )
                })?;
                vector.push(n as f32);
            }
            *value = Value::String(sqlite_blob_param(
                &crate::backend::sqlite::vector::vec_to_le_bytes(&vector),
            ));
            Ok(())
        }
        Some("geoPoint") => {
            let obj = value.as_object().ok_or_else(|| {
                DbError::validation(
                    "invalid_geo_arg",
                    format!("db: geoPoint column '{field}' must be an object with lat/lng"),
                )
            })?;
            let lat = obj.get("lat").and_then(Value::as_f64).ok_or_else(|| {
                DbError::validation(
                    "invalid_geo_arg",
                    format!("db: geoPoint column '{field}' is missing numeric lat"),
                )
            })?;
            let lng = obj.get("lng").and_then(Value::as_f64).ok_or_else(|| {
                DbError::validation(
                    "invalid_geo_arg",
                    format!("db: geoPoint column '{field}' is missing numeric lng"),
                )
            })?;
            *value = Value::String(sqlite_blob_param(
                &crate::backend::sqlite::spatial::point_to_blob(crate::backend::GeoPoint {
                    lat,
                    lng,
                }),
            ));
            Ok(())
        }
        _ => Ok(()),
    }
}

fn encode_sqlite_binary_doc_with_schema(schema: &Value, doc: &mut Value) -> Result<(), DbError> {
    let Some(obj) = doc.as_object_mut() else {
        return Ok(());
    };
    let Some(schema_obj) = schema.as_object() else {
        return Ok(());
    };
    for (field, field_def) in schema_obj {
        let Some(value) = obj.get_mut(field) else {
            continue;
        };
        encode_sqlite_binary_scalar(field, field_def, value)?;
    }
    Ok(())
}

fn encode_sqlite_binary_update_with_schema(schema: &Value, patch: &mut Value) -> Result<(), DbError> {
    let Some(obj) = patch.as_object_mut() else {
        return Ok(());
    };
    if let Some(set_doc) = obj.get_mut("$set") {
        encode_sqlite_binary_doc_with_schema(schema, set_doc)?;
    }
    for (field, value) in obj {
        if field.starts_with('$') || field.starts_with("__zsbin__") {
            continue;
        }
        let Some(field_def) = schema_field(schema, field) else {
            continue;
        };
        match value {
            Value::Array(_) | Value::String(_) | Value::Object(_) | Value::Null => {
                if let Some(set_val) = value.as_object_mut().and_then(|ops| ops.get_mut("$set")) {
                    encode_sqlite_binary_scalar(field, field_def, set_val)?;
                } else {
                    encode_sqlite_binary_scalar(field, field_def, value)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn aggregate_group_fields(pipeline: &Value) -> Vec<String> {
    let Some(stages) = pipeline.as_array() else {
        return Vec::new();
    };
    let Some(group_val) = stages
        .iter()
        .find_map(|stage| stage.as_object().and_then(|obj| obj.get("$group")))
    else {
        return Vec::new();
    };
    let Some(group_obj) = group_val.as_object() else {
        return Vec::new();
    };
    match group_obj.get("by") {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(Value::as_str)
            .map(ToString::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

/// `first_row_or_null` variant that, when `has_masked` is
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

#[allow(clippy::cast_precision_loss)]
fn usize_count_as_f64(count: usize) -> ResolveValue {
    ResolveValue::F64(count as f64)
}

/// `rows_as_json_array` variant that resolves via
/// [`ResolveValue::JsonWithRehydration`] when `has_masked` is set. See
/// [`first_row_or_null_masked`].
fn rows_as_json_array_masked(rows: Vec<Value>, has_masked: bool) -> ResolveValue {
    let value = Value::Array(rows).to_string();
    maybe_rehydrate(value, has_masked)
}

/// Pick `ResolveValue::JsonWithRehydration` (walk the
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

// ---------------------------------------------------------------------------
// find — read path
// ---------------------------------------------------------------------------

// `dispatch_find_one` does not exist: `Collection.findOne` was removed
// (Convex-style consolidation). The SDK reaches the same "first matching
// row" semantic via `find(filter).first()` / `.unique()` / `.last()` on
// the Query terminal, which composes the existing `dispatch_find` with
// `LIMIT 1` (or `LIMIT 2` for strict `.unique()`).

/// Extract `opts.unmask` into a `Vec<String>`. Returns
/// empty when the field is absent, null, or not an array of strings —
/// malformed `unmask` shapes are tolerated as
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

fn validate_unmask_projection(
    select: Option<&Value>,
    unmask_columns: &[String],
) -> Result<(), DbError> {
    if unmask_columns.is_empty() {
        return Ok(());
    }
    let Some(Value::Array(arr)) = select else {
        return Ok(());
    };
    if arr.is_empty() {
        return Ok(());
    }
    if arr.iter().any(|v| v.as_str() == Some("id")) {
        return Ok(());
    }
    Err(DbError::ValidationFailed {
        code: "unmask_requires_id_projection",
        message: "find: `opts.unmask` requires explicit `select` projections to include `id`"
            .to_string(),
        hint: Some(
            "Add `id` to `opts.select` or drop the explicit projection when using `opts.unmask`."
                .to_string(),
        ),
    })
}

/// Shared dispatch for `find`. Reads `limit`/`offset`/`orderBy`/
/// `select`/`unmask`/`actor` out of `opts`. The per-query unmask hint
/// honours an upfront authorisation fence — a single
/// unauthorised column refuses the whole find with
/// `unmask_not_permitted`.
pub(crate) fn dispatch_find<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    binding: DbBinding,
    collection: &str,
    filter: Value,
    opts: Value,
) -> v8::Local<'s, v8::Promise> {
    let app_id = binding.app_id();
    let state = runtime_state(scope);
    // Record into the active query's read-set so the broker can
    // narrow events to this filter. No-op outside `query()` handlers.
    record_read_set(&binding, collection, &filter);

    // DB-2: public `find` normalises an omitted limit here before calling the
    // builder. This does not protect internal builder callers; they must pass
    // their own explicit bound. Callers paginate past this page via `offset`.
    let limit = Some(query::effective_query_limit(
        opts.get("limit").and_then(Value::as_i64),
    ));
    let offset = opts.get("offset").and_then(Value::as_i64);
    let order_by = opts.get("orderBy").cloned();
    let select = opts.get("select").cloned();
    let unmask_columns = parse_unmask_opt(opts.get("unmask"));
    // DB-3: strip an app-supplied reserved `auto` system actor — a find with
    // `{unmask, actor:{kind:"auto"}}` must not impersonate the platform.
    let unmask_actor = crate::crud::unmask::sanitize_app_actor(
        opts.get("actor").cloned().filter(|v| !v.is_null()),
    );
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
    // Routing decision frozen HERE, while `scope` is live: see `crate::tx_route`.
    let route = TxRoute::capture(scope, app_id);
    let coll = collection.to_string();

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        if let Err(e) = validate_unmask_projection(select.as_ref(), &unmask_columns) {
            return OpResult::JsValue {
                resolver,
                value: ResolveValue::RejectError(e.to_op_error()),
                request_id,
            };
        }

        // Upfront auth fence for the unmask hint.
        if !unmask_columns.is_empty() {
            if let Err(e) = crate::crud::unmask::authorize_query_hint(
                &binding,
                &coll,
                &unmask_columns,
                &unmask_actor,
                &unmask_reason,
            )
            .await
            {
                return reject_op(resolver, request_id, e);
            }
        }

        // Resolve the descriptor entry BEFORE building SQL. It is the
        // projection allowlist: the SELECT clause expands to `"id"` plus one
        // term per declared field, with a masked column read through its
        // sibling (`"<col>_masked" AS "<col>"`) so the ciphertext column never
        // leaves the database on a default read. A collection this deploy does
        // not declare is refused here.
        let schema_hint = match crate::descriptor::collection_schema(&binding, &coll) {
            Ok(schema) => schema,
            Err(e) => return reject_op(resolver, request_id, e),
        };
        // Soft-delete auto-filter gate.
        let filter_soft_deleted = system_fields_pass::should_filter_soft_deleted(include_deleted);
        let mut sql_filter = filter;
        maybe_lower_sqlite_boolean_filter(&schema_hint, &mut sql_filter);
        let built = query::build_find_with_schema_and_unmask_and_soft_delete_with_dialect(
            &app,
            &coll,
            &sql_filter,
            limit,
            offset,
            order_by.as_ref(),
            select.as_ref(),
            &schema_hint,
            &unmask_columns,
            filter_soft_deleted,
            current_sql_dialect(),
        );
        let bq = match built {
            Ok(bq) => bq,
            Err(e) => return reject_op(resolver, request_id, DbError::from(e)),
        };
        match exec_query(&route, bq).await {
            Ok(rows) => {
                let result = match read_pipeline::apply(
                    &binding,
                    &coll,
                    rows,
                    read_pipeline::ApplyOptions {
                        unmask_columns: &unmask_columns,
                        schema_field_scope: read_pipeline::SchemaFieldScope::All,
                        ..read_pipeline::ApplyOptions::default()
                    },
                )
                .await
                {
                    Ok(result) => result,
                    Err(e) => return reject_op(resolver, request_id, e),
                };
                if !unmask_columns.is_empty() {
                    if let Err(e) = crate::crud::unmask::audit_query_hint_granted(
                        &binding,
                        &coll,
                        &unmask_columns,
                        &unmask_actor,
                        &unmask_reason,
                    )
                    .await
                    {
                        return reject_op(resolver, request_id, e);
                    }
                }
                OpResult::JsValue {
                    resolver,
                    value: rows_as_json_array_masked(result.rows, result.has_masked),
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
    binding: DbBinding,
    collection: &str,
    doc: Value,
) -> v8::Local<'s, v8::Promise> {
    let app_id = binding.app_id();
    let state = runtime_state(scope);
    let (resolver, request_id, promise) = setup_js_promise(scope, &state);

    let coll = collection.to_string();
    let app = app_id.to_string();
    // Routing decision frozen HERE, while `scope` is live: see `crate::tx_route`.
    let route = TxRoute::capture(scope, app_id);

    // Read the request-bound actor id at the synchronous
    // boundary BEFORE the async tail starts. The runtime's
    // `executing_request_id` is only guaranteed-set on the pump turn
    // that initiates the dispatch; once we `.await` (e.g. the
    // encryption pass's `resolve_key` round-trip), the pump may rotate
    // the slot. Reading here pins the actor to the request that
    // originated the insert.
    let actor_id = system_fields_pass::current_actor_id(&state);

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        let mut doc = doc;
        if let Err(e) = write_pipeline::apply(
            &binding,
            &coll,
            &mut doc,
            write_pipeline::ApplyMode::Insert {
                actor_id: actor_id.as_deref(),
            },
        )
        .await
        {
            return reject_op(resolver, request_id, e);
        }
        // `write_pipeline::apply` already refused an undeclared collection, so
        // this resolution cannot fail here; it re-reads the same store entry
        // rather than threading the schema back out through `apply`'s result.
        let schema = match crate::descriptor::collection_schema(&binding, &coll) {
            Ok(schema) => schema,
            Err(e) => return reject_op(resolver, request_id, e),
        };
        maybe_lower_sqlite_boolean_doc(&schema, &mut doc);
        let built = query::build_insert_with_dialect(&app, &coll, &schema, &doc, current_sql_dialect());
        let result = match built {
            Ok(bq) => {
                exec_mutation_with_emit(bq, &route, &coll, crate::broker::ChangeOp::Insert).await
            }
            Err(e) => Err(DbError::from(e)),
        };
        match result {
            Ok(rows) => {
                let result = match read_pipeline::apply(
                    &binding,
                    &coll,
                    rows,
                    read_pipeline::ApplyOptions::default(),
                )
                .await
                {
                    Ok(result) => result,
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
                    value: first_row_or_null_masked(result.rows, result.has_masked),
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
    binding: DbBinding,
    collection: &str,
    docs: Value,
) -> v8::Local<'s, v8::Promise> {
    let app_id = binding.app_id();
    let state = runtime_state(scope);
    let (resolver, request_id, promise) = setup_js_promise(scope, &state);
    let coll = collection.to_string();
    let app = app_id.to_string();
    // Routing decision frozen HERE, while `scope` is live: see `crate::tx_route`.
    let route = TxRoute::capture(scope, app_id);
    let actor_id = system_fields_pass::current_actor_id(&state);

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        let mut docs = docs;
        if let Err(e) = prepare_insert_many_docs_for_binding(
            &mut docs,
            &binding,
            &coll,
            actor_id.as_deref(),
        )
        .await
        {
            return reject_op(resolver, request_id, e);
        }
        let schema = match crate::descriptor::collection_schema(&binding, &coll) {
            Ok(schema) => schema,
            Err(e) => return reject_op(resolver, request_id, e),
        };
        maybe_lower_sqlite_boolean_docs(&schema, &mut docs);

        let built =
            query::build_insert_many_with_dialect(&app, &coll, &schema, &docs, current_sql_dialect());
        let result = match built {
            Ok(bq) => {
                exec_mutation_with_emit(bq, &route, &coll, crate::broker::ChangeOp::Insert).await
            }
            Err(e) => Err(DbError::from(e)),
        };
        match result {
            Ok(rows) => {
                let result = match read_pipeline::apply(
                    &binding,
                    &coll,
                    rows,
                    read_pipeline::ApplyOptions::default(),
                )
                .await
                {
                    Ok(result) => result,
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
                    value: rows_as_json_array_masked(result.rows, result.has_masked),
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
// updateOne / updateMany — write paths
// ---------------------------------------------------------------------------

/// Shared dispatch for `updateOne`. See [`dispatch_insert`] for the
/// capability-gate contract.
///
/// Every UPDATE auto-bumps `version` + `updated_at` +
/// `updated_by` (when an actor is in scope). When the caller's filter
/// carries `version: N`, the auto-bumped SQL still runs but the
/// affected-rows count is checked: 0 affected → typed
/// `version_mismatch` error. A `version` filter without an `id`
/// predicate refuses eagerly with `multi_row_version_filter_unsupported`
/// — the CAS semantics don't generalise to multi-row UPDATEs.
pub(crate) fn dispatch_update_one<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    binding: DbBinding,
    collection: &str,
    filter: Value,
    update: Value,
) -> v8::Local<'s, v8::Promise> {
    let app_id = binding.app_id();
    let state = runtime_state(scope);
    let (resolver, request_id, promise) = setup_js_promise(scope, &state);

    let coll = collection.to_string();
    let app = app_id.to_string();
    // Routing decision frozen HERE, while `scope` is live: see `crate::tx_route`.
    let route = TxRoute::capture(scope, app_id);

    // Read actor at the sync boundary (same rationale as
    // `dispatch_insert`'s actor pin: the runtime's `executing_request_id`
    // rotates on the next pump turn).
    let actor_id = system_fields_pass::current_actor_id(&state);

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        let hints = match write_pipeline::inspect_update(&app, &coll, &update) {
            Ok(hints) => hints,
            Err(e) => {
                return OpResult::JsValue {
                    resolver,
                    value: ResolveValue::RejectError(e.to_op_error()),
                    request_id,
                };
            }
        };
        // Detect creator-supplied CAS version + reject
        // the unsupported "version filter without id" shape eagerly.
        let cas_version = match system_fields_pass::extract_cas_version(&filter, &coll) {
            Ok(version) => version,
            Err(e) => {
                return OpResult::JsValue {
                    resolver,
                    value: ResolveValue::RejectError(e.to_op_error()),
                    request_id,
                };
            }
        };
        if cas_version.is_some() && !system_fields_pass::filter_has_id_predicate(&filter) {
            return OpResult::JsValue {
                resolver,
                value: ResolveValue::RejectError(
                    DbError::multi_row_version_filter_unsupported(&coll).to_op_error(),
                ),
                request_id,
            };
        }

        // The descriptor entry for this collection. Everything below reads it:
        // the per-row-randomised-encryption decision, the SQLite boolean
        // lowering, and the target-row probe's filter. An undeclared collection
        // rejects the op rather than silently skipping the per-row path.
        let schema = match crate::descriptor::collection_schema(&binding, &coll) {
            Ok(schema) => schema,
            Err(e) => return reject_op(resolver, request_id, e),
        };
        let per_row_encrypted_update =
            write_pipeline::update_requires_per_row_encryption(&schema, &update);
        let target_row = if per_row_encrypted_update {
            let target_rows = match write_pipeline::resolve_target_row_ids(
                &route,
                &coll,
                &filter,
                1,
                &schema,
            )
            .await
            {
                Ok(rows) => rows,
                Err(e) => return reject_op(resolver, request_id, e),
            };
            let Some(target_row) = target_rows.first().cloned() else {
                if let Some(expected_version) = cas_version {
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
                return OpResult::JsValue {
                    resolver,
                    value: ResolveValue::Json("null".to_string()),
                    request_id,
                };
            };
            Some(target_row)
        } else {
            None
        };

        let mut update = update;
        let row_pk = target_row.as_ref().map_or("", |row| row.row_pk.as_str());
        if let Err(e) = write_pipeline::apply(
            &binding,
            &coll,
            &mut update,
            write_pipeline::ApplyMode::Update { row_pk },
        )
        .await
        {
            return reject_op(resolver, request_id, e);
        }
        maybe_lower_sqlite_boolean_update(&schema, &mut update);
        let sql_filter = if let Some(target_row) = target_row {
            let mut sql_filter = serde_json::json!({ "id": target_row.id_value });
            if let Some(expected_version) = cas_version {
                sql_filter["version"] = Value::from(expected_version);
            }
            sql_filter
        } else {
            let mut sql_filter = filter.clone();
            maybe_lower_sqlite_boolean_filter(&schema, &mut sql_filter);
            sql_filter
        };
        // Auto-bump via the system-fields-aware builder.
        // Actor flows into the `updated_by` bind; the `hints` from the
        // pre-pass tell the builder which auto-bumps to suppress.
        let autobump = query::SystemFieldAutoBump {
            dispatch_write: true,
            actor_id: actor_id.as_deref(),
            skip_version: hints.creator_supplied_version,
            skip_updated_at: hints.creator_supplied_updated_at,
            skip_updated_by: hints.creator_supplied_updated_by,
        };
        let built = query::build_update_one_with_system_fields(
            &app,
            &coll,
            &schema,
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
        match exec_mutation_with_emit(bq, &route, &coll, crate::broker::ChangeOp::Update).await {
            Ok(rows) => {
                let result = match read_pipeline::apply(
                    &binding,
                    &coll,
                    rows,
                    read_pipeline::ApplyOptions::default(),
                )
                .await
                {
                    Ok(result) => result,
                    Err(e) => {
                        return OpResult::JsValue {
                            resolver,
                            value: ResolveValue::RejectError(e.to_op_error()),
                            request_id,
                        };
                    }
                };
                // Optimistic-concurrency check. When the
                // creator supplied a `version: N` predicate AND the
                // RETURNING set is empty, classify as a CAS failure
                // (the row exists at a different version, or the row
                // is missing — the SDK consumer retries either way).
                if let Some(expected_version) = cas_version {
                    if result.rows.is_empty() {
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
                    if result.rows.len() > 1 {
                        tracing::error!(
                            collection = %coll,
                            row_count = result.rows.len(),
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
                OpResult::JsValue {
                    resolver,
                    value: first_row_or_null_masked(result.rows, result.has_masked),
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
/// Same auto-bump rules as `dispatch_update_one`. CAS
/// semantics don't generalise to multi-row UPDATEs (the affected-row
/// count conflates "row missing" / "version mismatched" / "filter
/// didn't match"), so a `version` filter without `id` predicate
/// refuses eagerly with `multi_row_version_filter_unsupported`. The
/// affected-row count is returned as a plain number on the success
/// path.
pub(crate) fn dispatch_update_many<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    binding: DbBinding,
    collection: &str,
    filter: Value,
    update: Value,
) -> v8::Local<'s, v8::Promise> {
    let app_id = binding.app_id();
    let state = runtime_state(scope);
    let (resolver, request_id, promise) = setup_js_promise(scope, &state);

    let coll = collection.to_string();
    let app = app_id.to_string();
    // Routing decision frozen HERE, while `scope` is live: see `crate::tx_route`.
    let route = TxRoute::capture(scope, app_id);

    // Actor read at sync boundary (mirrors
    // `dispatch_update_one`'s rationale).
    let actor_id = system_fields_pass::current_actor_id(&state);

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        let hints = match write_pipeline::inspect_update(&app, &coll, &update) {
            Ok(hints) => hints,
            Err(e) => {
                return OpResult::JsValue {
                    resolver,
                    value: ResolveValue::RejectError(e.to_op_error()),
                    request_id,
                };
            }
        };
        let cas_version = match system_fields_pass::extract_cas_version(&filter, &coll) {
            Ok(version) => version,
            Err(e) => {
                return OpResult::JsValue {
                    resolver,
                    value: ResolveValue::RejectError(e.to_op_error()),
                    request_id,
                };
            }
        };
        if cas_version.is_some() && !system_fields_pass::filter_has_id_predicate(&filter) {
            return OpResult::JsValue {
                resolver,
                value: ResolveValue::RejectError(
                    DbError::multi_row_version_filter_unsupported(&coll).to_op_error(),
                ),
                request_id,
            };
        }

        // The descriptor entry, resolved once for the whole op: the per-row
        // randomised-encryption decision, the SQLite boolean lowering and the
        // target-row probe all read it. An undeclared collection rejects.
        let schema = match crate::descriptor::collection_schema(&binding, &coll) {
            Ok(schema) => schema,
            Err(e) => return reject_op(resolver, request_id, e),
        };
        let per_row_encrypted_update =
            write_pipeline::update_requires_per_row_encryption(&schema, &update);
        let autobump = query::SystemFieldAutoBump {
            dispatch_write: true,
            actor_id: actor_id.as_deref(),
            skip_version: hints.creator_supplied_version,
            skip_updated_at: hints.creator_supplied_updated_at,
            skip_updated_by: hints.creator_supplied_updated_by,
        };
        if per_row_encrypted_update {
            let frame = match crate::transaction::AtomicWriteFrame::begin(route).await {
                Ok(frame) => frame,
                Err(e) => return reject_op(resolver, request_id, e),
            };
            let work_result: Result<usize, DbError> = async {
                let target_rows = write_pipeline::resolve_target_row_ids(
                    frame.route(),
                    &coll,
                    &filter,
                    query::MAX_QUERY_LIMIT + 1,
                    &schema,
                )
                .await?;
                let target_limit = usize::try_from(query::MAX_QUERY_LIMIT)
                    .expect("MAX_QUERY_LIMIT must be a positive usize");
                if target_rows.len() > target_limit {
                    return Err(DbError::validation_hinted(
                        "update_many_target_limit_exceeded",
                        format!(
                            "updateMany matched more than {} rows; the maximum is {}",
                            query::MAX_QUERY_LIMIT,
                            query::MAX_QUERY_LIMIT
                        ),
                        format!(
                            "Narrow the updateMany filter so one call targets at most {} rows.",
                            query::MAX_QUERY_LIMIT
                        ),
                    ));
                }
                if target_rows.is_empty() {
                    if let Some(expected_version) = cas_version {
                        let row_id = filter
                            .as_object()
                            .and_then(|o| o.get("id"))
                            .and_then(|v| v.as_str());
                        return Err(DbError::version_mismatch(
                            &coll,
                            row_id,
                            expected_version,
                        ));
                    }
                    return Ok(0);
                }

                let target_count = target_rows.len();
                let mut row_queries = Vec::with_capacity(target_count);
                for target_row in &target_rows {
                    let row_pk = target_row.row_pk.clone();
                    let row_id = target_row.id_value.clone();
                    let mut row_update = update.clone();
                    write_pipeline::apply(
                        &binding,
                        &coll,
                        &mut row_update,
                        write_pipeline::ApplyMode::Update {
                            row_pk: &row_pk,
                        },
                    )
                    .await?;
                    maybe_lower_sqlite_boolean_update(&schema, &mut row_update);
                    let mut row_filter = serde_json::json!({ "id": row_id });
                    if let Some(expected_version) = cas_version {
                        row_filter["version"] = Value::from(expected_version);
                    }
                    row_queries.push(
                        query::build_update_one_with_system_fields(
                            &app,
                            &coll,
                            &schema,
                            &row_filter,
                            &row_update,
                            current_sql_dialect(),
                            &autobump,
                        )
                        .map_err(DbError::from)?,
                    );
                }

                let mut affected = 0usize;
                for built in row_queries {
                    affected += exec_mutation_with_emit(
                        built,
                        frame.route(),
                        &coll,
                        crate::broker::ChangeOp::Update,
                    )
                    .await?
                    .len();
                }

                if let Some(expected_version) = cas_version {
                    if affected != target_count {
                        let row_id = filter
                            .as_object()
                            .and_then(|o| o.get("id"))
                            .and_then(|v| v.as_str());
                        return Err(DbError::version_mismatch(
                            &coll,
                            row_id,
                            expected_version,
                        ));
                    }
                }
                Ok(affected)
            }
            .await;
            return match frame.finish(work_result).await {
                Ok(affected) => OpResult::JsValue {
                    resolver,
                    value: usize_count_as_f64(affected),
                    request_id,
                },
                Err(e) => OpResult::JsValue {
                    resolver,
                    value: ResolveValue::RejectError(e.to_op_error()),
                    request_id,
                },
            };
        }

        let mut update = update;
        if let Err(e) = write_pipeline::apply(
            &binding,
            &coll,
            &mut update,
            write_pipeline::ApplyMode::Update { row_pk: "" },
        )
        .await
        {
            return reject_op(resolver, request_id, e);
        }
        maybe_lower_sqlite_boolean_update(&schema, &mut update);
        let mut sql_filter = filter.clone();
        maybe_lower_sqlite_boolean_filter(&schema, &mut sql_filter);
        let built = query::build_update_many_with_system_fields(
            &app,
            &coll,
            &schema,
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
        match exec_mutation_with_emit(bq, &route, &coll, crate::broker::ChangeOp::Update).await {
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
// `delete()` soft-deletes by updating `deleted_at`.
// `purge()` remains the explicit hard-delete, and `restore()` clears
// `deleted_at` on a soft-deleted row.
// ---------------------------------------------------------------------------

/// Shared dispatch for `deleteOne`. See [`dispatch_insert`] for the
/// capability-gate contract.
pub(crate) fn dispatch_delete_one<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    binding: DbBinding,
    collection: &str,
    filter: Value,
) -> v8::Local<'s, v8::Promise> {
    let app_id = binding.app_id();
    let state = runtime_state(scope);
    let (resolver, request_id, promise) = setup_js_promise(scope, &state);

    let coll = collection.to_string();
    let app = app_id.to_string();
    // Routing decision frozen HERE, while `scope` is live: see `crate::tx_route`.
    let route = TxRoute::capture(scope, app_id);
    let actor_id = system_fields_pass::current_actor_id(&state);
    let autobump = query::SystemFieldAutoBump {
        actor_id: actor_id.as_deref(),
        ..Default::default()
    };
    // Resolve-then-build, folded into the one `Result` `run_op` already
    // rejects on: an undeclared collection cannot be soft-deleted through a
    // filter this deploy has no schema to lower.
    let built = crate::descriptor::collection_schema(&binding, &coll).and_then(|schema| {
        let mut filter = filter;
        maybe_lower_sqlite_boolean_filter(&schema, &mut filter);
        query::build_soft_delete_one_with_system_fields(
            &app,
            &coll,
            &schema,
            &filter,
            current_sql_dialect(),
            &autobump,
        )
        .map_err(DbError::from)
    });
    state.borrow_mut().spawned_ops.push(Box::pin(run_op(
        resolver,
        request_id,
        built,
        move |bq| async move {
            // Tagged as Update because soft-delete IS an UPDATE
            // setting `deleted_at`. Subscribers wanting to react
            // to soft-deletes inspect `new_tuple.deleted_at`.
            let rows =
                exec_mutation_with_emit(bq, &route, &coll, crate::broker::ChangeOp::Update)
                    .await?;
            read_pipeline::apply(&binding, &coll, rows, read_pipeline::ApplyOptions::default()).await
        },
        |result: read_pipeline::ApplyResult| {
            first_row_or_null_masked(result.rows, result.has_masked)
        },
    )));

    promise
}

/// Shared dispatch for `deleteMany`. Resolves with the count of
/// affected rows as a JS `number`.
pub(crate) fn dispatch_delete_many<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    binding: DbBinding,
    collection: &str,
    filter: Value,
) -> v8::Local<'s, v8::Promise> {
    let app_id = binding.app_id();
    let state = runtime_state(scope);
    let (resolver, request_id, promise) = setup_js_promise(scope, &state);

    let coll = collection.to_string();
    let app = app_id.to_string();
    // Routing decision frozen HERE, while `scope` is live: see `crate::tx_route`.
    let route = TxRoute::capture(scope, app_id);
    let actor_id = system_fields_pass::current_actor_id(&state);
    let autobump = query::SystemFieldAutoBump {
        actor_id: actor_id.as_deref(),
        ..Default::default()
    };
    let built = crate::descriptor::collection_schema(&binding, &coll).and_then(|schema| {
        let mut filter = filter;
        maybe_lower_sqlite_boolean_filter(&schema, &mut filter);
        query::build_soft_delete_many_with_system_fields(
            &app,
            &coll,
            &schema,
            &filter,
            current_sql_dialect(),
            &autobump,
        )
        .map_err(DbError::from)
    });
    state.borrow_mut().spawned_ops.push(Box::pin(run_op(
        resolver,
        request_id,
        built,
        move |bq| async move {
            exec_mutation_with_emit(bq, &route, &coll, crate::broker::ChangeOp::Update).await
        },
        row_count_as_f64,
    )));

    promise
}

/// Explicit hard-delete entry point. Always emits
/// `DELETE FROM ...` regardless of marker state. Used by the SDK's
/// `purge(filter)` for compliance / right-to-be-forgotten flows.
///
/// `purge` does NOT respect the `deleted_at IS NULL` auto-filter —
/// it removes both live and soft-deleted rows matching the filter.
pub(crate) fn dispatch_purge_one<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    binding: DbBinding,
    collection: &str,
    filter: Value,
) -> v8::Local<'s, v8::Promise> {
    let app_id = binding.app_id();
    let state = runtime_state(scope);
    let (resolver, request_id, promise) = setup_js_promise(scope, &state);

    let built = crate::descriptor::collection_schema(&binding, collection).and_then(|schema| {
        let mut filter = filter;
        maybe_lower_sqlite_boolean_filter(&schema, &mut filter);
        query::build_delete_one_with_dialect(app_id, collection, &schema, &filter, current_sql_dialect())
            .map_err(DbError::from)
    });
    let coll = collection.to_string();
    // Routing decision frozen HERE, while `scope` is live: see `crate::tx_route`.
    let route = TxRoute::capture(scope, app_id);

    state.borrow_mut().spawned_ops.push(Box::pin(run_op(
        resolver,
        request_id,
        built,
        move |bq| async move {
            let rows =
                exec_mutation_with_emit(bq, &route, &coll, crate::broker::ChangeOp::Delete)
                    .await?;
            read_pipeline::apply(
                &binding,
                &coll,
                rows,
                read_pipeline::ApplyOptions::default(),
            )
            .await
        },
        |result: read_pipeline::ApplyResult| {
            first_row_or_null_masked(result.rows, result.has_masked)
        },
    )));

    promise
}

/// Bulk-purge entry point.
pub(crate) fn dispatch_purge_many<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    binding: DbBinding,
    collection: &str,
    filter: Value,
) -> v8::Local<'s, v8::Promise> {
    let app_id = binding.app_id();
    let state = runtime_state(scope);
    let (resolver, request_id, promise) = setup_js_promise(scope, &state);

    let built = crate::descriptor::collection_schema(&binding, collection).and_then(|schema| {
        let mut filter = filter;
        maybe_lower_sqlite_boolean_filter(&schema, &mut filter);
        query::build_delete_many(app_id, collection, &schema, &filter).map_err(DbError::from)
    });
    let coll = collection.to_string();
    // Routing decision frozen HERE, while `scope` is live: see `crate::tx_route`.
    let route = TxRoute::capture(scope, app_id);

    state.borrow_mut().spawned_ops.push(Box::pin(run_op(
        resolver,
        request_id,
        built,
        move |bq| async move {
            exec_mutation_with_emit(bq, &route, &coll, crate::broker::ChangeOp::Delete).await
        },
        row_count_as_f64,
    )));

    promise
}

/// Restore a soft-deleted row.
pub(crate) fn dispatch_restore_one<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    binding: DbBinding,
    collection: &str,
    filter: Value,
) -> v8::Local<'s, v8::Promise> {
    let app_id = binding.app_id();
    let state = runtime_state(scope);
    let (resolver, request_id, promise) = setup_js_promise(scope, &state);

    let coll = collection.to_string();
    let app = app_id.to_string();
    // Routing decision frozen HERE, while `scope` is live: see `crate::tx_route`.
    let route = TxRoute::capture(scope, app_id);
    let actor_id = system_fields_pass::current_actor_id(&state);
    let autobump = query::SystemFieldAutoBump {
        dispatch_write: true,
        actor_id: actor_id.as_deref(),
        ..Default::default()
    };
    let built = crate::descriptor::collection_schema(&binding, &coll).and_then(|schema| {
        let mut filter = filter;
        maybe_lower_sqlite_boolean_filter(&schema, &mut filter);
        query::build_restore_one_with_system_fields(
            &app,
            &coll,
            &schema,
            &filter,
            current_sql_dialect(),
            &autobump,
        )
        .map_err(DbError::from)
    });
    state.borrow_mut().spawned_ops.push(Box::pin(run_op(
        resolver,
        request_id,
        built,
        move |bq| async move {
            let rows =
                exec_mutation_with_emit(bq, &route, &coll, crate::broker::ChangeOp::Update).await?;
            read_pipeline::apply(
                &binding,
                &coll,
                rows,
                read_pipeline::ApplyOptions::default(),
            )
            .await
        },
        |result: read_pipeline::ApplyResult| {
            first_row_or_null_masked(result.rows, result.has_masked)
        },
    )));

    promise
}

/// Bulk-restore entry point.
pub(crate) fn dispatch_restore_many<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    binding: DbBinding,
    collection: &str,
    filter: Value,
) -> v8::Local<'s, v8::Promise> {
    let app_id = binding.app_id();
    let state = runtime_state(scope);
    let (resolver, request_id, promise) = setup_js_promise(scope, &state);

    let coll = collection.to_string();
    let app = app_id.to_string();
    // Routing decision frozen HERE, while `scope` is live: see `crate::tx_route`.
    let route = TxRoute::capture(scope, app_id);
    let actor_id = system_fields_pass::current_actor_id(&state);
    let autobump = query::SystemFieldAutoBump {
        dispatch_write: true,
        actor_id: actor_id.as_deref(),
        ..Default::default()
    };
    let built = crate::descriptor::collection_schema(&binding, &coll).and_then(|schema| {
        let mut filter = filter;
        maybe_lower_sqlite_boolean_filter(&schema, &mut filter);
        query::build_restore_many_with_system_fields(
            &app,
            &coll,
            &schema,
            &filter,
            current_sql_dialect(),
            &autobump,
        )
        .map_err(DbError::from)
    });
    state.borrow_mut().spawned_ops.push(Box::pin(run_op(
        resolver,
        request_id,
        built,
        move |bq| async move {
            exec_mutation_with_emit(bq, &route, &coll, crate::broker::ChangeOp::Update).await
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
/// `opts.include_deleted: true` opts out of the auto
/// soft-delete `$match` (per Q-SF-J -- every read-side
/// method auto-filters for consistency).
pub(crate) fn dispatch_aggregate<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    binding: DbBinding,
    collection: &str,
    pipeline: Value,
    opts: Value,
) -> v8::Local<'s, v8::Promise> {
    let app_id = binding.app_id();
    let state = runtime_state(scope);

    // Record into the active query's read-set so the broker can
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
        record_read_set(&binding, collection, &captured_filter);
    }

    let include_deleted = opts
        .get("include_deleted")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let filter_soft_deleted = system_fields_pass::should_filter_soft_deleted(include_deleted);

    let (resolver, request_id, promise) = setup_js_promise(scope, &state);
    // Routing decision frozen HERE, while `scope` is live: see `crate::tx_route`.
    let route = TxRoute::capture(scope, app_id);
    let coll = collection.to_string();
    let group_fields = aggregate_group_fields(&pipeline);
    // The descriptor entry is the aggregate builder's identifier allowlist.
    // `$group.by` / `$sum` / `$sort` on a masked column read the field's own
    // column, which holds the mask - there is no sibling to lower to any more.
    let built = crate::descriptor::collection_schema(&binding, collection).and_then(|schema| {
        query::build_aggregate_with_result_columns(
            app_id,
            collection,
            &pipeline,
            filter_soft_deleted,
            &schema,
            current_sql_dialect(),
        )
        .map_err(DbError::from)
    });
    let (built, result_columns): (_, Option<Vec<String>>) = match built {
        Ok((bq, cols)) => (Ok(bq), cols),
        Err(e) => (Err(e), None),
    };

    state.borrow_mut().spawned_ops.push(Box::pin(run_op(
        resolver,
        request_id,
        built,
        move |bq| async move {
            let rows = exec_query(&route, bq).await?;
            read_pipeline::apply(
                &binding,
                &coll,
                rows,
                read_pipeline::ApplyOptions {
                    unmask_columns: &[],
                    schema_field_scope: if group_fields.is_empty() {
                        read_pipeline::SchemaFieldScope::All
                    } else {
                        read_pipeline::SchemaFieldScope::Only(group_fields.as_slice())
                    },
                    // A `$group` result's keys are accumulator aliases, which no
                    // descriptor declares, so the declared surface would drop
                    // every one of them. This is the ONLY call site in the crate
                    // that names a surface; every other one takes the default.
                    row_surface: match &result_columns {
                        Some(cols) => read_pipeline::RowSurface::Projected(cols.as_slice()),
                        None => read_pipeline::RowSurface::Declared,
                    },
                    ..read_pipeline::ApplyOptions::default()
                },
            )
            .await
        },
        |result: read_pipeline::ApplyResult| {
            rows_as_json_array_masked(result.rows, result.has_masked)
        },
    )));

    promise
}

/// Shared dispatch for `distinct`. `field` is the column name; `filter`
/// is the WHERE-clause JSON.
///
/// `opts.include_deleted: true` opts out of the auto-
/// filter; see [`dispatch_find`].
pub(crate) fn dispatch_distinct<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    binding: DbBinding,
    collection: &str,
    field: &str,
    filter: Value,
    opts: Value,
) -> v8::Local<'s, v8::Promise> {
    let app_id = binding.app_id();
    let state = runtime_state(scope);
    let (resolver, request_id, promise) = setup_js_promise(scope, &state);

    let include_deleted = opts
        .get("include_deleted")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let filter_soft_deleted = system_fields_pass::should_filter_soft_deleted(include_deleted);
    // Routing decision frozen HERE, while `scope` is live: see `crate::tx_route`.
    let route = TxRoute::capture(scope, app_id);
    let coll = collection.to_string();
    // A DISTINCT over a masked column returns MASKS - the column with the
    // field's own name is the one it selects, and that column holds the mask.
    // So the read pipeline's decrypt stage has nothing to do for it, and would
    // be handed a mask string where it expects base64. Derived from the same
    // descriptor entry the builder uses; an undeclared collection rejects
    // before either.
    let schema_hint = match crate::descriptor::collection_schema(&binding, collection) {
        Ok(schema) => schema,
        Err(e) => {
            let rejected = async move { reject_op(resolver, request_id, e) };
            state.borrow_mut().spawned_ops.push(Box::pin(rejected));
            return promise;
        }
    };
    let distinct_reads_masked_sibling = query::column_is_masked(field, &schema_hint);
    let mut filter = filter;
    maybe_lower_sqlite_boolean_filter(&schema_hint, &mut filter);
    let built = query::build_distinct_with_soft_delete_with_dialect(
        app_id,
        collection,
        field,
        &filter,
        filter_soft_deleted,
        &schema_hint,
        current_sql_dialect(),
    )
    .map_err(DbError::from);

    state.borrow_mut().spawned_ops.push(Box::pin(run_op(
        resolver,
        request_id,
        built,
        move |bq| async move {
            let rows = exec_query(&route, bq).await?;
            read_pipeline::apply(
                &binding,
                &coll,
                rows,
                read_pipeline::ApplyOptions {
                    apply_decrypt: !distinct_reads_masked_sibling,
                    wrap_masked: false,
                    ..read_pipeline::ApplyOptions::default()
                },
            )
            .await
        },
        |result: read_pipeline::ApplyResult| {
            // Extract single-column values into a flat array. `rows`
            // is the pre-decoded result set — no JSON parse needed
            // before reshaping.
            let flat: Vec<Value> = result
                .rows
                .into_iter()
                .filter_map(|row| {
                    if let Value::Object(map) = row {
                        map.into_values().next()
                    } else {
                        None
                    }
                })
                .collect();
            maybe_rehydrate(Value::Array(flat).to_string(), result.has_masked)
        },
    )));

    promise
}

/// Shared dispatch for `count`. Resolves with a real JS `number`
/// (not a JSON-stringified integer).
///
/// `opts.include_deleted: true` opts out of the auto-
/// filter.
pub(crate) fn dispatch_count<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    binding: DbBinding,
    collection: &str,
    filter: Value,
    opts: Value,
) -> v8::Local<'s, v8::Promise> {
    let app_id = binding.app_id();
    let state = runtime_state(scope);
    // Record into the active query's read-set so the broker can
    // narrow events to this filter. No-op outside `query()` handlers.
    record_read_set(&binding, collection, &filter);

    let include_deleted = opts
        .get("include_deleted")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let filter_soft_deleted = system_fields_pass::should_filter_soft_deleted(include_deleted);

    let (resolver, request_id, promise) = setup_js_promise(scope, &state);
    // Routing decision frozen HERE, while `scope` is live: see `crate::tx_route`.
    let route = TxRoute::capture(scope, app_id);
    let built = crate::descriptor::collection_schema(&binding, collection).and_then(|schema| {
        let mut filter = filter;
        maybe_lower_sqlite_boolean_filter(&schema, &mut filter);
        query::build_count_with_soft_delete(app_id, collection, &filter, filter_soft_deleted)
            .map_err(DbError::from)
    });

    state.borrow_mut().spawned_ops.push(Box::pin(run_op(
        resolver,
        request_id,
        built,
        move |bq| async move { exec_count(&route, bq).await },
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
    binding: DbBinding,
    collection: &str,
    doc: Value,
    conflict_fields: Value,
) -> v8::Local<'s, v8::Promise> {
    let app_id = binding.app_id();
    let state = runtime_state(scope);
    let (resolver, request_id, promise) = setup_js_promise(scope, &state);
    let actor_id = system_fields_pass::current_actor_id(&state);
    let coll = collection.to_string();
    let app = app_id.to_string();
    // Routing decision frozen HERE, while `scope` is live: see `crate::tx_route`.
    let route = TxRoute::capture(scope, app_id);

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        let mut doc = doc;
        if let Err(e) = prepare_upsert_doc_for_write(
            &mut doc,
            &binding,
            &route,
            &coll,
            actor_id.as_deref(),
            &conflict_fields,
        )
        .await
        {
            return reject_op(resolver, request_id, e);
        }
        let schema = match crate::descriptor::collection_schema(&binding, &coll) {
            Ok(schema) => schema,
            Err(e) => return reject_op(resolver, request_id, e),
        };
        maybe_lower_sqlite_boolean_doc(&schema, &mut doc);
        let built = query::build_upsert_with_dialect(
            &app,
            &coll,
            &schema,
            &doc,
            &conflict_fields,
            current_sql_dialect(),
        );
        let result = match built {
            Ok(bq) => {
            // Upsert can be either INSERT (new row) or UPDATE (existing).
            // We tag as Update because the subscriber's reaction is the
            // same -- re-fetch. Finer-grained read-set narrowing could
            // distinguish INSERT from UPDATE; this coarser tagging
            // doesn't need to.
                exec_mutation_with_emit(bq, &route, &coll, crate::broker::ChangeOp::Update).await
            }
            Err(e) => Err(DbError::from(e)),
        };
        match result {
            Ok(rows) => {
                let result = match read_pipeline::apply(
                    &binding,
                    &coll,
                    rows,
                    read_pipeline::ApplyOptions::default(),
                )
                .await
                {
                    Ok(result) => result,
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
                    value: first_row_or_null_masked(result.rows, result.has_masked),
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
// search - vector entry point
// ---------------------------------------------------------------------------

/// Shared dispatch for `collection.search(args)`.
///
/// - `{ vector, k?, metric?, column?, filter? }` -> `VectorIndex::vector_search`,
///   routed to pgvector on PG or the pure-Rust flat-scan implementation
///   on SQLite.
///
/// Resolves with a JSON array of rows; each row carries the
/// `_distance` synthetic column from pgvector. Errors are coded
/// (`vector_extension_missing` / `vector_unsupported` / standard
/// SQLSTATE) so the SDK can branch on `e.code`.
pub(crate) fn dispatch_search<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    binding: DbBinding,
    collection: &str,
    args: Value,
) -> v8::Local<'s, v8::Promise> {
    use zeroship_runtime::state::OpError;

    let state = runtime_state(scope);
    let (resolver, request_id, promise) = setup_js_promise(scope, &state);

    // Presence of `vector` selects the pgvector path.
    let has_vector = args.get("vector").is_some();

    if !has_vector {
        // Reject synchronously via the typed error path so the SDK sees
        // a coded error rather than a hang. Use `Configuration` because
        // the failure is shape-level, not data-level.
        let err = DbError::Configuration {
            code: "invalid_search_args",
            message: "search: args must include `vector`".to_string(),
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
    let coll = collection.to_string();
    // The backend arms below resolve the same entry for their projection; this
    // one is for the SQLite boolean lowering of the caller's filter. Refusing
    // here keeps the rejection on the synchronous half, before the promise is
    // handed a spawned op.
    let schema = match crate::descriptor::collection_schema(&binding, collection) {
        Ok(schema) => schema,
        Err(e) => {
            let op_err: OpError = e.to_op_error();
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
    maybe_lower_sqlite_boolean_filter(&schema, &mut filter);

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
                pg.vector_search(&binding, &coll, &column, &vector, k, metric, &filter)
                    .await
            };
            // SQLite arm routes through the pure-Rust flat-scan
            // `VectorIndex` impl on `SqliteBackend`. We short-circuit
            // BEFORE the PG path so a build with both arms compiled
            // in (`--features "pg sqlite"` for tests) dispatches
            // based on which arm the runtime is bound to, not on
            // Cargo-feature ordering.
            if let Some(sq) = backend.as_sqlite() {
                use crate::backend::VectorIndex as _;
                return sq
                    .vector_search(&binding, &coll, &column, &vector, k, metric, &filter)
                    .await;
            }
            pg_path().await
        }
        .await;

        match result {
            Ok(rows) => {
                let result = match read_pipeline::apply(
                    &binding,
                    &coll,
                    rows,
                    read_pipeline::ApplyOptions::default(),
                )
                .await
                {
                    Ok(result) => result,
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
                    value: rows_as_json_array_masked(result.rows, result.has_masked),
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

/// Shared dispatch for the `Collection.near()` v8_method.
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
/// Routes to `SpatialIndex::spatial_near`, dispatching to PG's
/// `geography(POINT, 4326)` support or SQLite's pure-Rust haversine
/// flat-scan implementation. Each returned row carries a synthetic
/// `_distance_m` (`f64`) column.
pub(crate) fn dispatch_near<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    binding: DbBinding,
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

    let coll = collection.to_string();
    // Same as `dispatch_search`: the backend arm resolves the entry again for
    // its own projection; this one lowers the caller's filter, and refusing an
    // undeclared collection here keeps the rejection synchronous.
    let schema = match crate::descriptor::collection_schema(&binding, collection) {
        Ok(schema) => schema,
        Err(e) => reject_sync!(e),
    };
    maybe_lower_sqlite_boolean_filter(&schema, &mut filter);
    let point = crate::backend::GeoPoint { lat, lng };

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        let backend = crate::context::with(|c| c.backend());
        let result: Result<Vec<Value>, DbError> = async {
            let backend = backend.ok_or_else(|| {
                DbError::config("not_configured", "db: backend not initialized".to_string())
            })?;
            // SQLite arm routes through the pure-Rust haversine
            // flat-scan `SpatialIndex` impl on `SqliteBackend`.
            // Short-circuit BEFORE the PG path so a build with both
            // arms compiled in dispatches based on which arm the
            // runtime is bound to.
            if let Some(sq) = backend.as_sqlite() {
                use crate::backend::SpatialIndex as _;
                return sq
                    .spatial_near(&binding, &coll, &field, point, radius_m, &filter, limit)
                    .await;
            }
            let pg = backend
                .as_postgres()
                .ok_or_else(|| DbError::backend_unsupported("spatial_near"))?;
            use crate::backend::SpatialIndex as _;
            pg.spatial_near(&binding, &coll, &field, point, radius_m, &filter, limit)
                .await
        }
        .await;

        match result {
            Ok(rows) => {
                let result = match read_pipeline::apply(
                    &binding,
                    &coll,
                    rows,
                    read_pipeline::ApplyOptions::default(),
                )
                .await
                {
                    Ok(result) => result,
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
                    value: rows_as_json_array_masked(result.rows, result.has_masked),
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

// `dispatch_find_or_create` was removed along with the
// `Collection.findOrCreate` v8_method (absorbed by `upsert`). The
// `query::build_find_or_create` SQL builder stays for now —
// `upsert({where, create})` shape lands in a follow-up.

// ===========================================================================
// Transparent column encryption hooks
// ===========================================================================
async fn prepare_insert_many_docs_for_binding(
    docs: &mut Value,
    binding: &DbBinding,
    collection: &str,
    actor_id: Option<&str>,
) -> Result<(), DbError> {
    write_pipeline::apply(
        binding,
        collection,
        docs,
        write_pipeline::ApplyMode::InsertMany { actor_id },
    )
    .await
}

#[cfg(feature = "test-helpers")]
pub async fn prepare_insert_many_docs_for_write(
    docs: &mut Value,
    app_id: &str,
    collection: &str,
    actor_id: Option<&str>,
) -> Result<(), DbError> {
    let binding = DbBinding::cold_start(app_id);
    prepare_insert_many_docs_for_binding(docs, &binding, collection, actor_id).await
}

/// Test helper that drives the REAL read pipeline
/// (`read_pipeline::apply` with default options: decrypt + mask-wrap on) over a
/// set of freshly-fetched rows, so a faithful round-trip e2e can exercise the
/// descriptor-sourced decrypt + mask-wrap path end-to-end (not an AEAD-unit
/// shim). Returns the finalized rows; `has_masked` is dropped (the caller
/// asserts on the row contents).
#[cfg(feature = "test-helpers")]
pub async fn finalize_rows_on_read_for_tests(
    app_id: &str,
    collection: &str,
    rows: Vec<Value>,
) -> Result<Vec<Value>, DbError> {
    let binding = DbBinding::cold_start(app_id);
    let result = read_pipeline::apply(
        &binding,
        collection,
        rows,
        read_pipeline::ApplyOptions::default(),
    )
    .await?;
    Ok(result.rows)
}

/// Test helper that resolves the runtime data-access schema the way the CRUD
/// passes do — through [`crate::descriptor::collection_schema`], the data
/// plane's sole schema authority. Lets a test assert that the metadata the
/// read/write passes will act on is exactly the descriptor entry the deploy
/// installed, and that an undeclared collection is a typed refusal rather than
/// an absent schema.
#[cfg(feature = "test-helpers")]
pub fn runtime_schema_for_tests(app_id: &str, collection: &str) -> Result<Value, DbError> {
    let binding = DbBinding::cold_start(app_id);
    // Deep-clone out of the shared store: the helper's callers own and mutate
    // their copy, and handing them the isolate's `Arc` would let one test
    // observe another's edit.
    crate::descriptor::collection_schema(&binding, collection).map(|facts| (*facts).clone())
}

/// Upsert's write-side prep. Unlike its `insert_many` sibling this is
/// private with no `test-helpers` twin — nothing outside this crate
/// called the `pub` arm, and the upsert path now needs the dispatch's
/// [`TxRoute`] (its deterministic-encryption conflict probe issues a
/// read that must land on the same connection as the write).
async fn prepare_upsert_doc_for_write(
    doc: &mut Value,
    binding: &DbBinding,
    route: &TxRoute,
    collection: &str,
    actor_id: Option<&str>,
    conflict_fields: &Value,
) -> Result<(), DbError> {
    write_pipeline::apply(
        binding,
        collection,
        doc,
        write_pipeline::ApplyMode::Upsert {
            actor_id,
            conflict_fields,
            route,
        },
    )
    .await
}

/// Run the write-side encryption pass over `doc` using the
/// backend-arm `EncryptedColumn` impl.
///
/// - **PG arm** (chosen at runtime, not compiled in): goes through
///   `PostgresBackend`'s `EncryptedColumn` impl. The SQL builder
///   emits `decode($N, 'base64')::bytea` so the BYTEA column receives
///   raw bytes.
/// - **SQLite arm** (chosen at runtime, not compiled in): goes through
///   `SqliteBackend`'s `EncryptedColumn` impl using env-var-sourced
///   keys. The SQL builder (when called with `SqlDialect::Sqlite`)
///   emits `$N` and tags the encrypted-column param with
///   `SQLITE_BINARY_BIND_PREFIX`; the session actor binds the raw bytes
///   as a BLOB.
///
/// Encrypted columns work end-to-end on both backends through the
/// SDK's CRUD path.
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

/// Cheap walk: does any field def on `schema` carry a
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

fn schema_has_sqlite_binary_columns(schema: &Value) -> bool {
    schema
        .as_object()
        .map(|o| {
            o.values().any(|def| {
                matches!(
                    def.get("type").and_then(Value::as_str),
                    Some("vector") | Some("geoPoint")
                )
            })
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

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
    fn validate_unmask_projection_rejects_explicit_select_without_id() {
        let err = validate_unmask_projection(
            Some(&serde_json::json!(["ssn", "email"])),
            &["ssn".to_string()],
        )
        .expect_err("explicit unmask projection without id must be refused");

        match err {
            DbError::ValidationFailed { code, message, .. } => {
                assert_eq!(code, "unmask_requires_id_projection");
                assert!(
                    message.contains("include `id`"),
                    "error should explain the missing id requirement: {message}"
                );
            }
            other => panic!("expected ValidationFailed, got {other:?}"),
        }
    }

    #[test]
    fn validate_unmask_projection_accepts_implicit_or_id_inclusive_select() {
        validate_unmask_projection(None, &["ssn".to_string()]).expect("implicit select ok");
        validate_unmask_projection(
            Some(&serde_json::json!(["id", "ssn"])),
            &["ssn".to_string()],
        )
        .expect("id-inclusive projection ok");
    }

    #[test]
    fn encode_sqlite_binary_doc_with_schema_packs_vector_and_geopoint() {
        let schema = serde_json::json!({
            "embedding": { "type": "vector", "vectorDims": 4 },
            "loc": { "type": "geoPoint" },
            "name": { "type": "string" }
        });
        let mut doc = serde_json::json!({
            "embedding": [1.0, 0.0, 0.5, -1.25],
            "loc": { "lat": 37.7749, "lng": -122.4194 },
            "name": "Alpha HQ"
        });

        encode_sqlite_binary_doc_with_schema(&schema, &mut doc).expect("encode sqlite blobs");

        let embedding = doc["embedding"]
            .as_str()
            .expect("embedding should be sentinel-wrapped base64");
        let loc = doc["loc"]
            .as_str()
            .expect("loc should be sentinel-wrapped base64");

        assert!(
            embedding.starts_with(crate::query::SQLITE_BINARY_BIND_PREFIX),
            "vector payload must use the sqlite blob sentinel: {embedding}"
        );
        assert!(
            loc.starts_with(crate::query::SQLITE_BINARY_BIND_PREFIX),
            "geo payload must use the sqlite blob sentinel: {loc}"
        );

        let embedding_bytes = base64::engine::general_purpose::STANDARD
            .decode(embedding.trim_start_matches(crate::query::SQLITE_BINARY_BIND_PREFIX))
            .expect("decode vector blob");
        let loc_bytes = base64::engine::general_purpose::STANDARD
            .decode(loc.trim_start_matches(crate::query::SQLITE_BINARY_BIND_PREFIX))
            .expect("decode geo blob");

        assert_eq!(
            embedding_bytes,
            crate::backend::sqlite::vector::vec_to_le_bytes(&[1.0, 0.0, 0.5, -1.25]),
        );
        assert_eq!(
            loc_bytes,
            crate::backend::sqlite::spatial::point_to_blob(crate::backend::GeoPoint {
                lat: 37.7749,
                lng: -122.4194,
            }),
        );
    }
}
