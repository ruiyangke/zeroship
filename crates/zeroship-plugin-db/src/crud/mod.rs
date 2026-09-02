//! The CRUD query pipeline — one `plan_*` / `run_*` pair per Collection method.
//!
//! **This module names no `v8` type.** Every operation is split in two, and
//! the split is a crate boundary, not a style:
//!
//! * `plan_*` is the **eager** half. It runs synchronously, before any
//!   `await`, because what it does cannot be moved past one: recording into
//!   the active read-set ([`crate::read_set`]) for subscription narrowing,
//!   and applying the DB-3 actor fence. It returns a plan - a `BuiltQuery`, a
//!   [`FindPlan`], a [`SearchPlan`] - and touches no connection.
//! * `run_*` is the **async** half. It takes the plan plus a
//!   [`crate::tx_route::TxRoute`] captured by the caller, drives the backend
//!   through [`crate::exec`], and returns data.
//!
//! Neither half allocates a promise or resolves one. That is the adapter's
//! job: [`crate::v8_classes::dispatch`] holds the 17 `dispatch_*` helpers
//! that mint the promise, freeze the route while the V8 scope is live, and
//! lower a `run_*` result into a `ResolveValue` via `settle` / `run_op`.
//!
//! Those helpers lived HERE until 2026-09-02. Splitting each operation into
//! `plan_*` / `run_*` came first and made them thin; moving them out came
//! second and is what let this file stop mentioning V8 at all.
//!
//! The capability gate (`refuse_if_query_capability`) is enforced by the
//! `#[v8_class]` methods *before* reaching a dispatch helper - write ops
//! trust their callers.

use serde_json::Value;

use zeroship_data_core::binding::DbBinding;
use zeroship_data_core::error::DbError;
use crate::exec::{exec_mutation_with_emit, exec_query};
use crate::query;
use crate::tx_route::TxRoute;

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


// Mask backfill / rewrite / removal jobs driven by the migration service.
// Same visibility pattern: `pub` under `test-helpers` so integration tests
// can drive the helpers directly without standing up the full orchestrator.
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
pub(crate) mod read_pipeline;
mod write_pipeline;

#[cfg(any(test, feature = "test-helpers"))]
#[allow(unused_imports)]
pub use write_pipeline::{
    WritePathCounters, reset_write_path_counters_for_tests, write_path_counters_for_tests,
};

// ---------------------------------------------------------------------------
// dispatch_op template

/// The ENGINE composition behind the three row-returning mutation dispatches -
/// `deleteOne`, `purgeOne`, `restoreOne`: execute, emit the change event, then
/// run the read pipeline over the RETURNING rows.
///
/// It takes `binding`, `coll` and `route` BY VALUE, which is the point rather
/// than an accident: it is handed to [`run_op`] as
/// `move |bq| exec_mutation_then_read(binding, coll, route, bq, op)`, and
/// `run_op`'s `EFut` cannot borrow from the closure it was produced by.
///
/// The three callers differ ONLY in `op` - Update, Delete, Update. Each was a
/// separate copy of this body until 2026-09-02, and the copies were read against
/// each other first: same `ApplyOptions::default()`, same
/// `first_row_or_null_masked` resolve. The three-line dispatches that call this
/// are NOT evidence the family is uniform elsewhere; `deleteMany`, `purgeMany`
/// and `restoreMany` run no read pipeline at all and are deliberately not folded
/// in here.
pub(crate) async fn exec_mutation_then_read(
    binding: DbBinding,
    coll: String,
    route: crate::tx_route::TxRoute,
    bq: query::BuiltQuery,
    op: zeroship_core::change_event::ChangeOp,
) -> Result<read_pipeline::ApplyResult, DbError> {
    let rows = exec_mutation_with_emit(bq, &route, &coll, op).await?;
    read_pipeline::apply(
        &binding,
        &coll,
        rows,
        read_pipeline::ApplyOptions::default(),
    )
    .await
}

/// The ENGINE composition behind `aggregate`. It gets its own function rather
/// than sharing [`exec_mutation_then_read`] because its `ApplyOptions` are not
/// the default ones and cannot be reached by a parameter on that signature.
///
/// `group_fields` and `result_columns` are owned rather than borrowed because
/// `ApplyOptions` holds SLICES of them, so both have to outlive the `apply`
/// call inside this future - a caller-side borrow could not.
pub(crate) async fn exec_aggregate_read(
    binding: DbBinding,
    coll: String,
    route: crate::tx_route::TxRoute,
    bq: query::BuiltQuery,
    group_fields: Vec<String>,
    result_columns: Option<Vec<String>>,
) -> Result<read_pipeline::ApplyResult, DbError> {
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
}

/// The ENGINE composition behind `distinct`. Also non-default `ApplyOptions`,
/// and different ones again from [`exec_aggregate_read`]: a DISTINCT over a
/// masked column selects the column holding the MASK, so the decrypt stage has
/// nothing to do and would be handed a mask string where it expects base64.
pub(crate) async fn exec_distinct_read(
    binding: DbBinding,
    coll: String,
    route: crate::tx_route::TxRoute,
    bq: query::BuiltQuery,
    reads_masked_sibling: bool,
) -> Result<read_pipeline::ApplyResult, DbError> {
    let rows = exec_query(&route, bq).await?;
    read_pipeline::apply(
        &binding,
        &coll,
        rows,
        read_pipeline::ApplyOptions {
            apply_decrypt: !reads_masked_sibling,
            wrap_masked: false,
            ..read_pipeline::ApplyOptions::default()
        },
    )
    .await
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
        Some(crate::backend::BackendHandle::Postgres(_)) => query::SqlDialect::Postgres,
        None => match crate::context::with(|c| c.backend_selection()) {
            Some(crate::BackendUrl::Sqlite { .. }) => query::SqlDialect::Sqlite,
            _ => query::SqlDialect::Postgres,
        },
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
    schema.as_object()?.get(field)?.get("type")?.as_str()
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

fn encode_sqlite_binary_scalar(
    field: &str,
    field_def: &Value,
    value: &mut Value,
) -> Result<(), DbError> {
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

fn encode_sqlite_binary_update_with_schema(
    schema: &Value,
    patch: &mut Value,
) -> Result<(), DbError> {
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

pub(crate) fn aggregate_group_fields(pipeline: &Value) -> Vec<String> {
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

/// The eagerly-evaluated inputs of a `find`, produced by [`plan_find`] and
/// consumed by [`run_find`].
///
/// This type exists because `find` CANNOT be cut the way the nine `plan_*`
/// functions above were. Those had a synchronous planning prologue that ran to
/// a `BuiltQuery` before the promise. `find` has no such prologue: its schema
/// resolution and SQL build sit BEHIND `authorize_query_hint(...).await`, so
/// they cannot be hoisted ahead of the V8 boundary at all. The engine half is
/// therefore an `async fn`, and this struct carries what must still be read
/// eagerly across into it.
pub(crate) struct FindPlan {
    limit: Option<i64>,
    offset: Option<i64>,
    order_by: Option<Value>,
    select: Option<Value>,
    unmask_columns: Vec<String>,
    unmask_actor: Option<Value>,
    unmask_rejected_claim: Option<Value>,
    unmask_reason: Option<String>,
    include_deleted: bool,
}

/// The EAGER half of `find`. Everything here must run while the dispatching
/// handler is still the active one on this thread.
///
/// `record_read_set` is the reason this is a separate function rather than the
/// head of [`run_find`]. It is ambient: `read_set::is_active` reads the
/// `CURRENT_BUFFER` thread-local (`read_set.rs:368-373`), which is `Some` only
/// inside a query handler. Moving it into the async body would defer it to
/// first poll, where the buffer is either gone - silently dropping the entry
/// the broker needs to narrow events - or belongs to a DIFFERENT query. That is
/// the same hazard the `dispatch_insert` actor_id read is documented against.
/// Verified by reading `read_set.rs`, not by a test.
pub(crate) fn plan_find(
    binding: &DbBinding,
    collection: &str,
    filter: &Value,
    opts: &Value,
) -> FindPlan {
    // Record into the active query's read-set so the broker can
    // narrow events to this filter. No-op outside `query()` handlers.
    record_read_set(binding, collection, filter);

    // DB-2: public `find` normalises an omitted limit here before calling the
    // builder. This does not protect internal builder callers; they must pass
    // their own explicit bound. Callers paginate past this page via `offset`.
    let limit = Some(query::effective_query_limit(
        opts.get("limit").and_then(Value::as_i64),
    ));
    // DB-3: strip an app-supplied reserved `auto` system actor — a find with
    // `{unmask, actor:{kind:"auto"}}` must not impersonate the platform.
    let unmask_sanitized = crate::crud::unmask::sanitize_app_actor(
        opts.get("actor").cloned().filter(|v| !v.is_null()),
    );

    FindPlan {
        limit,
        offset: opts.get("offset").and_then(Value::as_i64),
        order_by: opts.get("orderBy").cloned(),
        select: opts.get("select").cloned(),
        unmask_columns: parse_unmask_opt(opts.get("unmask")),
        unmask_actor: unmask_sanitized.actor,
        unmask_rejected_claim: unmask_sanitized.rejected_claim,
        unmask_reason: opts
            .get("unmaskReason")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        include_deleted: opts
            .get("include_deleted")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
    }
}

/// The DEFERRED half of `find`: no `scope`, no `v8::`, no `ResolveValue`.
///
/// `route` is passed in rather than captured because the routing decision must
/// be frozen while the scope is live; see `crate::tx_route`.
pub(crate) async fn run_find(
    binding: DbBinding,
    coll: String,
    route: crate::tx_route::TxRoute,
    filter: Value,
    plan: FindPlan,
) -> Result<read_pipeline::ApplyResult, DbError> {
    validate_unmask_projection(plan.select.as_ref(), &plan.unmask_columns)?;

    // Upfront auth fence for the unmask hint.
    if !plan.unmask_columns.is_empty() {
        crate::crud::unmask::authorize_query_hint(
            &binding,
            &coll,
            &plan.unmask_columns,
            &plan.unmask_actor,
            plan.unmask_rejected_claim.as_ref(),
            &plan.unmask_reason,
        )
        .await?;
    }

    // Resolve the descriptor entry BEFORE building SQL. It is the
    // projection allowlist: the SELECT clause expands to `"id"` plus one
    // term per declared field, with a masked column read through its
    // sibling (`"<col>_masked" AS "<col>"`) so the ciphertext column never
    // leaves the database on a default read. A collection this deploy does
    // not declare is refused here.
    let schema_hint = crate::descriptor::collection_schema(&binding, &coll)?;
    // Soft-delete auto-filter gate.
    let filter_soft_deleted = system_fields_pass::should_filter_soft_deleted(plan.include_deleted);
    let mut sql_filter = filter;
    maybe_lower_sqlite_boolean_filter(&schema_hint, &mut sql_filter);
    let bq = query::build_find_with_schema_and_unmask_and_soft_delete_with_dialect(
        binding.app_id(),
        &coll,
        &sql_filter,
        plan.limit,
        plan.offset,
        plan.order_by.as_ref(),
        plan.select.as_ref(),
        &schema_hint,
        &plan.unmask_columns,
        filter_soft_deleted,
        current_sql_dialect(),
    )
    .map_err(DbError::from)?;
    let rows = exec_query(&route, bq).await?;
    let result = read_pipeline::apply(
        &binding,
        &coll,
        rows,
        read_pipeline::ApplyOptions {
            unmask_columns: &plan.unmask_columns,
            schema_field_scope: read_pipeline::SchemaFieldScope::All,
            ..read_pipeline::ApplyOptions::default()
        },
    )
    .await?;
    // The audit runs AFTER the rows are in hand and BEFORE they are
    // lowered: a failure here must refuse the read, not log it and
    // return the plaintext anyway.
    if !plan.unmask_columns.is_empty() {
        crate::crud::unmask::audit_query_hint_granted(
            &binding,
            &coll,
            &plan.unmask_columns,
            &plan.unmask_actor,
            plan.unmask_rejected_claim.as_ref(),
            &plan.unmask_reason,
        )
        .await?;
    }
    Ok(result)
}


// ---------------------------------------------------------------------------
// insert / insertMany — write paths returning the row(s)
// ---------------------------------------------------------------------------

/// Shared dispatch for `insert`. The capability gate is the caller's
/// responsibility — `Collection::insert` calls
/// `refuse_if_query_capability` before reaching here.
/// The ENGINE half of `insert`: no `scope`, no `v8::`, no `ResolveValue`.
///
/// `actor_id` is a PARAMETER rather than something this function looks up, and
/// that is load-bearing. `system_fields_pass::current_actor_id` reads
/// `executing_request_id` off the runtime state, which is only guaranteed-set
/// on the pump turn that initiates the dispatch. This function awaits before it
/// writes (the encryption pass's `resolve_key` round-trip), so resolving the
/// actor in here would attribute the row to whichever request happens to be
/// current at first poll. [`dispatch_insert`] reads it eagerly and passes it in.
pub(crate) async fn run_insert(
    binding: DbBinding,
    coll: String,
    route: crate::tx_route::TxRoute,
    doc: Value,
    actor_id: Option<String>,
) -> Result<read_pipeline::ApplyResult, DbError> {
    let mut doc = doc;
    write_pipeline::apply(
        &binding,
        &coll,
        &mut doc,
        write_pipeline::ApplyMode::Insert {
            actor_id: actor_id.as_deref(),
        },
    )
    .await?;
    // `write_pipeline::apply` already refused an undeclared collection, so
    // this resolution cannot fail here; it re-reads the same store entry
    // rather than threading the schema back out through `apply`'s result.
    let schema = crate::descriptor::collection_schema(&binding, &coll)?;
    maybe_lower_sqlite_boolean_doc(&schema, &mut doc);
    let bq = query::build_insert_with_dialect(
        binding.app_id(),
        &coll,
        &schema,
        &doc,
        current_sql_dialect(),
    )
    .map_err(DbError::from)?;
    let rows = exec_mutation_with_emit(
        bq,
        &route,
        &coll,
        zeroship_core::change_event::ChangeOp::Insert,
    )
    .await?;
    read_pipeline::apply(
        &binding,
        &coll,
        rows,
        read_pipeline::ApplyOptions::default(),
    )
    .await
}


/// Shared dispatch for `insertMany`. See [`dispatch_insert`] for the
/// capability-gate contract.
/// The ENGINE half of `insertMany`. `actor_id` is eager for the reason given on
/// [`run_insert`]; it reaches the docs through
/// `prepare_insert_many_docs_for_binding`, not `write_pipeline::apply`.
pub(crate) async fn run_insert_many(
    binding: DbBinding,
    coll: String,
    route: crate::tx_route::TxRoute,
    docs: Value,
    actor_id: Option<String>,
) -> Result<read_pipeline::ApplyResult, DbError> {
    let mut docs = docs;
    prepare_insert_many_docs_for_binding(&mut docs, &binding, &coll, actor_id.as_deref()).await?;
    let schema = crate::descriptor::collection_schema(&binding, &coll)?;
    maybe_lower_sqlite_boolean_docs(&schema, &mut docs);

    let bq = query::build_insert_many_with_dialect(
        binding.app_id(),
        &coll,
        &schema,
        &docs,
        current_sql_dialect(),
    )
    .map_err(DbError::from)?;
    let rows = exec_mutation_with_emit(
        bq,
        &route,
        &coll,
        zeroship_core::change_event::ChangeOp::Insert,
    )
    .await?;
    read_pipeline::apply(
        &binding,
        &coll,
        rows,
        read_pipeline::ApplyOptions::default(),
    )
    .await
}


// ---------------------------------------------------------------------------
// updateOne / updateMany — write paths
// ---------------------------------------------------------------------------

/// The ENGINE half of `updateOne`.
///
/// Returns a `(rows, has_masked)` PAIR rather than the [`read_pipeline::ApplyResult`]
/// the insert halves return. That is not a stylistic difference: the
/// probe-found-nothing arm below returns `(Vec::new(), false)`, a shape no
/// `ApplyResult` produces, and the `false` is load-bearing - see the comment at
/// that return. `actor_id` is eager for the reason given on [`run_insert`].
pub(crate) async fn run_update_one(
    binding: DbBinding,
    coll: String,
    route: crate::tx_route::TxRoute,
    filter: Value,
    update: Value,
    actor_id: Option<String>,
) -> Result<(Vec<Value>, bool), DbError> {
    let mut update = update;
    write_pipeline::inspect_update(binding.app_id(), &coll, &mut update)?;
    // Detect creator-supplied CAS version + reject
    // the unsupported "version filter without id" shape eagerly.
    let cas_version = system_fields_pass::extract_cas_version(&filter, &coll)?;
    if cas_version.is_some() && !system_fields_pass::filter_has_id_predicate(&filter) {
        return Err(DbError::multi_row_version_filter_unsupported(&coll));
    }

    // The descriptor entry for this collection. Everything below reads it:
    // the per-row-randomised-encryption decision, the SQLite boolean
    // lowering, and the target-row probe's filter. An undeclared collection
    // rejects the op rather than silently skipping the per-row path.
    let schema = crate::descriptor::collection_schema(&binding, &coll)?;
    let per_row_encrypted_update =
        write_pipeline::update_requires_per_row_encryption(&schema, &update);
    let target_row = if per_row_encrypted_update {
        let target_rows =
            write_pipeline::resolve_target_row_ids(&route, &coll, &filter, 1, &schema)
                .await?;
        let Some(target_row) = target_rows.first().cloned() else {
            if let Some(expected_version) = cas_version {
                let row_id = filter
                    .as_object()
                    .and_then(|o| o.get("id"))
                    .and_then(|v| v.as_str());
                return Err(DbError::version_mismatch(&coll, row_id, expected_version));
            }
            // Probe found nothing and the caller supplied no CAS predicate:
            // resolve with JS `null`. Returned as an EMPTY ROW SET rather
            // than a bespoke `ResolveValue::Json("null")`, because
            // `first_row_or_null_masked(vec![], false)` lowers to exactly
            // that string - so the success path has one shape, not two.
            //
            // THE `false` IS LOAD-BEARING; DO NOT DERIVE IT. An empty row
            // vector does NOT imply "nothing was masked": `read_pipeline`
            // computes `has_masked` from the SCHEMA, not from the rows, so
            // it is `true` for zero rows on any collection with a masked
            // column. Deriving it here - which reads like a consistency fix,
            // since every other arm does derive it - would silently turn
            // this arm's `ResolveValue::Json` into `JsonWithRehydration`
            // and hand JS a rehydration pass over `null`.
            return Ok((Vec::new(), false));
        };
        Some(target_row)
    } else {
        None
    };

    let mut update = update;
    let row_pk = target_row.as_ref().map_or("", |row| row.row_pk.as_str());
    write_pipeline::apply(
        &binding,
        &coll,
        &mut update,
        write_pipeline::ApplyMode::Update { row_pk },
    )
    .await?;
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
    // No `skip_*` knob is set: the pass stripped every column the
    // charter re-assigns on write, so the patch cannot carry a
    // competing assignment for the builder to defer to.
    let autobump = query::SystemFieldAutoBump {
        dispatch_write: true,
        actor_id: actor_id.as_deref(),
        ..Default::default()
    };
    let built = query::build_update_one_with_system_fields(
        binding.app_id(),
        &coll,
        &schema,
        &sql_filter,
        &update,
        current_sql_dialect(),
        &autobump,
    );
    let bq = built.map_err(DbError::from)?;
    let rows = exec_mutation_with_emit(
        bq,
        &route,
        &coll,
        zeroship_core::change_event::ChangeOp::Update,
    )
    .await?;
    let result = read_pipeline::apply(
        &binding,
        &coll,
        rows,
        read_pipeline::ApplyOptions::default(),
    )
    .await?;
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
            return Err(DbError::version_mismatch(&coll, row_id, expected_version));
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
            return Err(DbError::internal("version_mismatch_unexpected_multi_row"));
        }
    }
    Ok((result.rows, result.has_masked))
}


/// The ENGINE half of `updateMany`. Resolves to a COUNT, so unlike
/// [`run_update_one`] it returns a plain `usize` and the adapter lowers once.
///
/// `route` is taken BY VALUE because the per-row-encrypted arm moves it into
/// `AtomicWriteFrame::begin`; the other arm only borrows it. `actor_id` is eager
/// for the reason given on [`run_insert`].
pub(crate) async fn run_update_many(
    binding: DbBinding,
    coll: String,
    route: crate::tx_route::TxRoute,
    filter: Value,
    update: Value,
    actor_id: Option<String>,
) -> Result<usize, DbError> {
    let mut update = update;
    write_pipeline::inspect_update(binding.app_id(), &coll, &mut update)?;
    let cas_version = system_fields_pass::extract_cas_version(&filter, &coll)?;
    if cas_version.is_some() && !system_fields_pass::filter_has_id_predicate(&filter) {
        return Err(DbError::multi_row_version_filter_unsupported(&coll));
    }

    // The descriptor entry, resolved once for the whole op: the per-row
    // randomised-encryption decision, the SQLite boolean lowering and the
    // target-row probe all read it. An undeclared collection rejects.
    let schema = crate::descriptor::collection_schema(&binding, &coll)?;
    let per_row_encrypted_update =
        write_pipeline::update_requires_per_row_encryption(&schema, &update);
    // No `skip_*` knob is set: the pass stripped every column the
    // charter re-assigns on write, so the patch cannot carry a
    // competing assignment for the builder to defer to.
    let autobump = query::SystemFieldAutoBump {
        dispatch_write: true,
        actor_id: actor_id.as_deref(),
        ..Default::default()
    };
    if per_row_encrypted_update {
        let frame = crate::transaction::AtomicWriteFrame::begin(route).await?;
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
                    return Err(DbError::version_mismatch(&coll, row_id, expected_version));
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
                    write_pipeline::ApplyMode::Update { row_pk: &row_pk },
                )
                .await?;
                maybe_lower_sqlite_boolean_update(&schema, &mut row_update);
                let mut row_filter = serde_json::json!({ "id": row_id });
                if let Some(expected_version) = cas_version {
                    row_filter["version"] = Value::from(expected_version);
                }
                // The probe resolved this row by its primary-key `id`, so
                // the per-row statement does not need a second bounded
                // subquery. Using the many builder here preserves the
                // ordinary column-grant surface while the primary key still
                // bounds the statement to this exact row.
                row_queries.push(
                    query::build_update_many_with_system_fields(
                        binding.app_id(),
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
                    zeroship_core::change_event::ChangeOp::Update,
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
                    return Err(DbError::version_mismatch(&coll, row_id, expected_version));
                }
            }
            Ok(affected)
        }
        .await;
        // `finish` is the commit/rollback boundary and already takes and
        // returns a `Result<usize, DbError>` - the same shape `settle`
        // wants - so the frame is committed or rolled back exactly once
        // whichever way the work went. Do NOT `?` the work_result above it.
        return frame.finish(work_result).await;
    }

    let mut update = update;
    write_pipeline::apply(
        &binding,
        &coll,
        &mut update,
        write_pipeline::ApplyMode::Update { row_pk: "" },
    )
    .await?;
    maybe_lower_sqlite_boolean_update(&schema, &mut update);
    let mut sql_filter = filter.clone();
    maybe_lower_sqlite_boolean_filter(&schema, &mut sql_filter);
    let bq = query::build_update_many_with_system_fields(
        binding.app_id(),
        &coll,
        &schema,
        &sql_filter,
        &update,
        current_sql_dialect(),
        &autobump,
    )
    .map_err(DbError::from)?;
    let rows = exec_mutation_with_emit(
        bq,
        &route,
        &coll,
        zeroship_core::change_event::ChangeOp::Update,
    )
    .await?;
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
            return Err(DbError::version_mismatch(&coll, row_id, expected_version));
        }
    }
    // Both arms resolve to a COUNT, so both return `usize` and the adapter
    // lowers once. The encrypted arm already did (`usize_count_as_f64`);
    // this arm used `row_count_as_f64(rows)`, which is `rows.len() as f64` -
    // the same `ResolveValue::F64`, so unifying changes no output.
    Ok(rows.len())
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
/// The ENGINE half of `delete_one`. Peer of [`plan_count`] and [`plan_purge_one`],
/// and the first that threads `actor_id`.
///
/// `actor_id` arrives as a PARAMETER rather than being read here, because reading
/// it needs the runtime state and therefore `scope`. The adapter reads it at the
/// synchronous boundary and passes it down - see the comment in `dispatch_insert`
/// for why that read must not drift into an async tail.
pub(crate) fn plan_delete_one(
    binding: &DbBinding,
    collection: &str,
    filter: Value,
    actor_id: Option<&str>,
) -> Result<query::BuiltQuery, DbError> {
    let app = binding.app_id();
    let autobump = query::SystemFieldAutoBump {
        actor_id,
        ..Default::default()
    };
    // Resolve-then-build, folded into the one `Result` `run_op` already
    // rejects on: an undeclared collection cannot be soft-deleted through a
    // filter this deploy has no schema to lower.
    crate::descriptor::collection_schema(binding, collection).and_then(|schema| {
        let mut filter = filter;
        maybe_lower_sqlite_boolean_filter(&schema, &mut filter);
        query::build_soft_delete_one_with_system_fields(
            app,
            collection,
            &schema,
            &filter,
            current_sql_dialect(),
            &autobump,
        )
        .map_err(DbError::from)
    })
}


/// Shared dispatch for `deleteMany`. Resolves with the count of
/// affected rows as a JS `number`.
/// The ENGINE half of `delete_many`. Identical in shape to [`plan_delete_one`];
/// only the builder differs.
pub(crate) fn plan_delete_many(
    binding: &DbBinding,
    collection: &str,
    filter: Value,
    actor_id: Option<&str>,
) -> Result<query::BuiltQuery, DbError> {
    let app = binding.app_id();
    let autobump = query::SystemFieldAutoBump {
        actor_id,
        ..Default::default()
    };
    crate::descriptor::collection_schema(binding, collection).and_then(|schema| {
        let mut filter = filter;
        maybe_lower_sqlite_boolean_filter(&schema, &mut filter);
        query::build_soft_delete_many_with_system_fields(
            app,
            collection,
            &schema,
            &filter,
            current_sql_dialect(),
            &autobump,
        )
        .map_err(DbError::from)
    })
}


/// Explicit hard-delete entry point. Always emits
/// `DELETE FROM ...` regardless of marker state. Used by the SDK's
/// `purge(filter)` for compliance / right-to-be-forgotten flows.
///
/// `purge` does NOT respect the `deleted_at IS NULL` auto-filter —
/// it removes both live and soft-deleted rows matching the filter.
/// The ENGINE half of `purge_one`. Peer of [`plan_count`]; see that function for
/// the seam this follows and the proposal lines that require it.
///
/// `purge_one` is a MUTATION that needs no `actor_id`: a hard delete stamps
/// nobody. That is why the 17-strong `dispatch_*` family divides into "needs
/// `current_actor_id(&state)`" (9) and "does not" (8) rather than read vs write -
/// this function is a write on the not-needed side.
pub(crate) fn plan_purge_one(
    binding: &DbBinding,
    collection: &str,
    filter: Value,
) -> Result<query::BuiltQuery, DbError> {
    let app_id = binding.app_id();
    crate::descriptor::collection_schema(binding, collection).and_then(|schema| {
        let mut filter = filter;
        maybe_lower_sqlite_boolean_filter(&schema, &mut filter);
        query::build_delete_one_with_dialect(
            app_id,
            collection,
            &schema,
            &filter,
            current_sql_dialect(),
        )
        .map_err(DbError::from)
    })
}


/// Bulk-purge entry point.
/// The ENGINE half of `purge_many`. Peer of [`plan_purge_one`]: a hard delete,
/// so no `actor_id`.
pub(crate) fn plan_purge_many(
    binding: &DbBinding,
    collection: &str,
    filter: Value,
) -> Result<query::BuiltQuery, DbError> {
    let app_id = binding.app_id();
    crate::descriptor::collection_schema(binding, collection).and_then(|schema| {
        let mut filter = filter;
        maybe_lower_sqlite_boolean_filter(&schema, &mut filter);
        query::build_delete_many(
            app_id,
            collection,
            &schema,
            &filter,
            current_sql_dialect(),
        )
        .map_err(DbError::from)
    })
}


/// Restore a soft-deleted row.
/// The ENGINE half of `restore_one`.
///
/// NOT a copy of [`plan_delete_one`]: the autobump here also sets
/// `dispatch_write: true`. Templating this family from a sibling would drop that
/// flag silently, so each plan is transcribed from its own dispatch.
pub(crate) fn plan_restore_one(
    binding: &DbBinding,
    collection: &str,
    filter: Value,
    actor_id: Option<&str>,
) -> Result<query::BuiltQuery, DbError> {
    let app = binding.app_id();
    let autobump = query::SystemFieldAutoBump {
        dispatch_write: true,
        actor_id,
        ..Default::default()
    };
    crate::descriptor::collection_schema(binding, collection).and_then(|schema| {
        let mut filter = filter;
        maybe_lower_sqlite_boolean_filter(&schema, &mut filter);
        query::build_restore_one_with_system_fields(
            app,
            collection,
            &schema,
            &filter,
            current_sql_dialect(),
            &autobump,
        )
        .map_err(DbError::from)
    })
}


/// Bulk-restore entry point.
/// The ENGINE half of `restore_many`. Like [`plan_restore_one`], the autobump
/// sets `dispatch_write: true`; only the builder differs.
pub(crate) fn plan_restore_many(
    binding: &DbBinding,
    collection: &str,
    filter: Value,
    actor_id: Option<&str>,
) -> Result<query::BuiltQuery, DbError> {
    let app = binding.app_id();
    let autobump = query::SystemFieldAutoBump {
        dispatch_write: true,
        actor_id,
        ..Default::default()
    };
    crate::descriptor::collection_schema(binding, collection).and_then(|schema| {
        let mut filter = filter;
        maybe_lower_sqlite_boolean_filter(&schema, &mut filter);
        query::build_restore_many_with_system_fields(
            app,
            collection,
            &schema,
            &filter,
            current_sql_dialect(),
            &autobump,
        )
        .map_err(DbError::from)
    })
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
/// The ENGINE half of `aggregate`.
///
/// Returns a PAIR, unlike the seven single-`BuiltQuery` plans in this file:
/// `build_aggregate_with_result_columns` yields the result-column list alongside
/// the query, and the adapter needs it to shape the response. `distinct` is the
/// other pair-returning member of this group.
///
/// `aggregate_group_fields(&pipeline)` deliberately stays on the adapter side for
/// now. It is pipeline analysis and belongs here, but moving it would make this a
/// three-value return; it travels with the outstanding second cut instead.
pub(crate) fn plan_aggregate(
    binding: &DbBinding,
    collection: &str,
    pipeline: &Value,
    opts: &Value,
) -> Result<(query::BuiltQuery, Option<Vec<String>>), DbError> {
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
        record_read_set(binding, collection, &captured_filter);
    }

    let include_deleted = opts
        .get("include_deleted")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let filter_soft_deleted = system_fields_pass::should_filter_soft_deleted(include_deleted);

    let app_id = binding.app_id();
    // The descriptor entry is the aggregate builder's identifier allowlist.
    // `$group.by` / `$sum` / `$sort` on a masked column read the field's own
    // column, which holds the mask - there is no sibling to lower to any more.
    crate::descriptor::collection_schema(binding, collection).and_then(|schema| {
        query::build_aggregate_with_result_columns(
            app_id,
            collection,
            pipeline,
            filter_soft_deleted,
            &schema,
            current_sql_dialect(),
        )
        .map_err(DbError::from)
    })
}


/// The ENGINE half of `distinct`: no `scope`, no `v8::`, no `ResolveValue`.
///
/// Returns a PAIR, like [`plan_aggregate`] and unlike the other seven plans in
/// this file. The second element is `distinct_reads_masked_sibling`, which the
/// adapter cannot recompute: it is derived from the descriptor `schema_hint`,
/// and the hint dies with this function.
///
/// The schema resolution that `dispatch_distinct` used to do inline, with its own
/// `reject_op` and an early `return promise`, is now the `?` below. That is
/// behaviour-preserving rather than a rewrite: `run_op`'s error arm at
/// `crud/mod.rs:191` is literally `return reject_op(resolver, request_id, e)`,
/// the same call the hand-rolled branch made, and both push one future onto
/// `spawned_ops`. Verified by reading `run_op`, not by test.
///
/// NOTE for anyone extending this: `distinct` does NOT `record_read_set`, though
/// [`plan_count`] and most siblings do. That asymmetry is transcribed from the
/// original body, not an omission - do not "restore" it.
pub(crate) fn plan_distinct(
    binding: &DbBinding,
    collection: &str,
    field: &str,
    filter: Value,
    opts: &Value,
) -> Result<(query::BuiltQuery, bool), DbError> {
    let app_id = binding.app_id();

    let include_deleted = opts
        .get("include_deleted")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let filter_soft_deleted = system_fields_pass::should_filter_soft_deleted(include_deleted);

    // A DISTINCT over a masked column returns MASKS - the column with the
    // field's own name is the one it selects, and that column holds the mask.
    // So the read pipeline's decrypt stage has nothing to do for it, and would
    // be handed a mask string where it expects base64. Derived from the same
    // descriptor entry the builder uses; an undeclared collection rejects
    // before either.
    let schema_hint = crate::descriptor::collection_schema(binding, collection)?;
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
    .map_err(DbError::from)?;

    Ok((built, distinct_reads_masked_sibling))
}


/// Shared dispatch for `count`. Resolves with a real JS `number`
/// (not a JSON-stringified integer).
///
/// `opts.include_deleted: true` opts out of the auto-
/// filter.
/// The ENGINE half of `count`: no `scope`, no `v8::`, no `ResolveValue`.
///
/// This is the cut the crate split requires, per
/// `docs/proposals/2026-08-31-data-crate-shape.md:138-142` - "the 39 V8-signature
/// functions belong in the thin layer, they ARE the boundary. Their `async move`
/// bodies are not - those bodies are query pipeline. The engine must stop
/// returning `OpResult`/`ResolveValue` and return data the adapter lowers."
///
/// It returns a `BuiltQuery`; `dispatch_count` below owns the promise, the route
/// capture and the `i64 -> ResolveValue` lowering. This is the worked example for
/// the other sixteen `dispatch_*` functions in this file.
///
/// ORDERING: `record_read_set` runs here, ahead of the V8 prologue, where it used
/// to run between `runtime_state` and `setup_js_promise`. That is safe rather than
/// merely convenient: it touches only the `read_set` thread-local and the
/// descriptor cache, and none of `runtime_state` / `setup_js_promise` /
/// `capture_route` reads or writes either. Reasoned from those three bodies, not
/// proven by a test.
pub(crate) fn plan_count(
    binding: &DbBinding,
    collection: &str,
    filter: Value,
    opts: &Value,
) -> Result<query::BuiltQuery, DbError> {
    // Record into the active query's read-set so the broker can
    // narrow events to this filter. No-op outside `query()` handlers.
    record_read_set(binding, collection, &filter);

    let include_deleted = opts
        .get("include_deleted")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let filter_soft_deleted = system_fields_pass::should_filter_soft_deleted(include_deleted);

    let app_id = binding.app_id();
    crate::descriptor::collection_schema(binding, collection).and_then(|schema| {
        let mut filter = filter;
        maybe_lower_sqlite_boolean_filter(&schema, &mut filter);
        query::build_count_with_soft_delete(
            app_id,
            collection,
            &schema,
            &filter,
            filter_soft_deleted,
            current_sql_dialect(),
        )
        .map_err(DbError::from)
    })
}


// ---------------------------------------------------------------------------
// upsert — INSERT … ON CONFLICT path
// ---------------------------------------------------------------------------

/// The ENGINE half of `upsert`. `actor_id` is eager for the reason given on
/// [`run_insert`]; `route` is borrowed twice here, so it is taken by value.
pub(crate) async fn run_upsert(
    binding: DbBinding,
    coll: String,
    route: crate::tx_route::TxRoute,
    doc: Value,
    conflict_fields: Value,
    actor_id: Option<String>,
) -> Result<read_pipeline::ApplyResult, DbError> {
    let mut doc = doc;
    prepare_upsert_doc_for_write(
        &mut doc,
        &binding,
        &route,
        &coll,
        actor_id.as_deref(),
        &conflict_fields,
    )
    .await?;
    let schema = crate::descriptor::collection_schema(&binding, &coll)?;
    maybe_lower_sqlite_boolean_doc(&schema, &mut doc);
    let bq = query::build_upsert_with_dialect(
        binding.app_id(),
        &coll,
        &schema,
        &doc,
        &conflict_fields,
        current_sql_dialect(),
    )
    .map_err(DbError::from)?;
    // Upsert can be either INSERT (new row) or UPDATE (existing).
    // We tag as Update because the subscriber's reaction is the
    // same -- re-fetch. Finer-grained read-set narrowing could
    // distinguish INSERT from UPDATE; this coarser tagging
    // doesn't need to.
    let rows = exec_mutation_with_emit(
        bq,
        &route,
        &coll,
        zeroship_core::change_event::ChangeOp::Update,
    )
    .await?;
    read_pipeline::apply(
        &binding,
        &coll,
        rows,
        read_pipeline::ApplyOptions::default(),
    )
    .await
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
/// The eagerly-decoded inputs of a vector `search`, produced by [`plan_search`]
/// and consumed by [`run_search`].
pub(crate) struct SearchPlan {
    vector: Vec<f32>,
    k: usize,
    metric: crate::backend::VectorMetric,
    column: String,
    filter: Value,
}

/// The EAGER half of `search`: argument decoding plus the descriptor lookup the
/// SQLite boolean lowering needs.
///
/// Every refusal here used to be an eagerly-spawned rejection future with its
/// own early `return promise`. They are plain `Err`s now, and the adapter feeds
/// them to `settle`, whose error arm calls the SAME [`reject_op`] those
/// rejections did - it is literally
/// `OpResult::JsValue { resolver, value: ResolveValue::RejectError(err.to_op_error()), request_id }`,
/// and the old helper was a one-line push of exactly that. Verified by reading
/// both before the fold, not by test; the helper is now deleted, since folding
/// the last two search-family dispatchers left it with no callers.
///
/// The decoding stays SYNCHRONOUS rather than moving into [`run_search`]: the
/// ordering of a descriptor read against the dispatching turn is the same
/// question [`plan_find`] documents, and this half is where the original put it.
pub(crate) fn plan_search(
    binding: &DbBinding,
    collection: &str,
    args: &Value,
) -> Result<SearchPlan, DbError> {
    // Presence of `vector` selects the pgvector path.
    let Some(raw_vector) = args.get("vector") else {
        // Use `Configuration` because the failure is shape-level, not
        // data-level.
        return Err(DbError::Configuration {
            code: "invalid_search_args",
            message: "search: args must include `vector`".to_string(),
            hint: Some(
                "pass `{ vector: number[], k?: number, metric?, column?, filter? }` for vector search"
                    .to_string(),
            ),
        });
    };

    // Decode `vector` into `Vec<f32>`. Reject anything that's not a
    // homogeneous number array at the boundary so the impl can stay
    // typed.
    let Some(arr) = raw_vector.as_array() else {
        return Err(DbError::Configuration {
            code: "invalid_vector_arg",
            message: "search: `vector` must be an array of numbers".to_string(),
            hint: None,
        });
    };
    let mut vector: Vec<f32> = Vec::with_capacity(arr.len());
    for elem in arr {
        let Some(n) = elem.as_f64() else {
            return Err(DbError::Configuration {
                code: "invalid_vector_arg",
                message: "search: every element of `vector` must be a number".to_string(),
                hint: None,
            });
        };
        vector.push(n as f32);
    }

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
    // The backend arms resolve the same entry for their projection; this one is
    // for the SQLite boolean lowering of the caller's filter.
    let schema = crate::descriptor::collection_schema(binding, collection)?;
    maybe_lower_sqlite_boolean_filter(&schema, &mut filter);

    Ok(SearchPlan {
        vector,
        k,
        metric,
        column,
        filter,
    })
}

/// The DEFERRED half of `search`: no `scope`, no `v8::`, no `OpResult`.
///
/// This body still names both backends by their accessors, which is the subject
/// of the backend-downcast inversion, not of this cut. Moving it here neither
/// helps nor worsens that; it relocates the same code to the tier that will be
/// fixed.
pub(crate) async fn run_search(
    binding: DbBinding,
    coll: String,
    plan: SearchPlan,
) -> Result<read_pipeline::ApplyResult, DbError> {
    let SearchPlan {
        vector,
        k,
        metric,
        column,
        filter,
    } = plan;

    // Reach the backend through the per-isolate context; runtime wiring stashes
    // a `BackendHandle` per isolate. `BackendHandle` itself implements
    // `VectorIndex`, so the vendor branch - and the SQLite ATTACH prelude that
    // used to sit here - lives in `backend/mod.rs` where naming a vendor is
    // legitimate. This function no longer knows either backend exists.
    let backend = crate::exec::ensure_backend_for_shared_sql().await?;
    use crate::backend::VectorIndex as _;
    let rows = backend
        .vector_search(&binding, &coll, &column, &vector, k, metric, &filter)
        .await?;

    // Metering, success arm only. The search family is a read op on
    // either backend and reaches the database WITHOUT passing
    // through `exec::run_sql` - the PG arm goes to
    // `PostgresBackend::query_roled_json`, the SQLite arm to its own
    // scan - so until 2026-09-01 it was billed as nothing at all.
    // Counted here at the op boundary rather than in either vendor:
    // the vendor tier must not reach up into the engine for the
    // meter handle, which is the cycle #110 just removed.
    //
    // It stays AFTER the search and BEFORE the read pipeline, exactly where the
    // hand-rolled `match result { Ok(rows) => ... }` put it: a read that
    // succeeds and then fails to decrypt is still a read that hit the database.
    crate::metrics::emit_db_metric(binding.app_id(), crate::metrics::DB_READS, 1);

    read_pipeline::apply(
        &binding,
        &coll,
        rows,
        read_pipeline::ApplyOptions::default(),
    )
    .await
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
/// The eagerly-decoded inputs of a spatial `near`, produced by [`plan_near`] and
/// consumed by [`run_near`].
pub(crate) struct NearPlan {
    field: String,
    point: crate::backend::GeoPoint,
    radius_m: f64,
    limit: Option<usize>,
    filter: Value,
}

/// The EAGER half of `near`. Same rejection-folding as [`plan_search`]: the five
/// eagerly-spawned rejections are plain `Err`s, and `settle`'s error arm makes
/// the same [`reject_op`] call they did.
///
/// All four argument refusals share the `invalid_near_args` code; only the
/// message distinguishes them. That is transcribed from the original, not
/// tidied - the SDK branches on the code.
pub(crate) fn plan_near(
    binding: &DbBinding,
    collection: &str,
    args: &Value,
) -> Result<NearPlan, DbError> {
    let field = match args.get("field").and_then(Value::as_str) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => {
            return Err(DbError::Configuration {
                code: "invalid_near_args",
                message: "near: `field` must be a non-empty string".to_string(),
                hint: Some("pass `{ field, point, radius, filter?, limit? }`".to_string()),
            });
        }
    };

    let point_obj = args.get("point");
    let lat = point_obj.and_then(|p| p.get("lat")).and_then(Value::as_f64);
    let lng = point_obj.and_then(|p| p.get("lng")).and_then(Value::as_f64);
    let (Some(lat), Some(lng)) = (lat, lng) else {
        return Err(DbError::Configuration {
            code: "invalid_near_args",
            message: "near: `point` must be `{ lat: number, lng: number }`".to_string(),
            hint: None,
        });
    };
    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lng) {
        return Err(DbError::Configuration {
            code: "invalid_near_args",
            message: format!(
                "near: `point` out of range: lat must be in [-90,90] and lng in [-180,180], got lat={lat} lng={lng}"
            ),
            hint: None,
        });
    }

    let radius_m = match args.get("radius").and_then(Value::as_f64) {
        Some(r) if r > 0.0 && r.is_finite() => r,
        _ => {
            return Err(DbError::Configuration {
                code: "invalid_near_args",
                message: "near: `radius` must be a positive number (metres)".to_string(),
                hint: None,
            });
        }
    };

    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .map(|n| n as usize);
    let mut filter = args
        .get("filter")
        .cloned()
        .unwrap_or_else(|| Value::Object(serde_json::Map::new()));

    // Same as `plan_search`: the backend arm resolves the entry again for its
    // own projection; this one lowers the caller's filter.
    let schema = crate::descriptor::collection_schema(binding, collection)?;
    maybe_lower_sqlite_boolean_filter(&schema, &mut filter);

    Ok(NearPlan {
        field,
        point: crate::backend::GeoPoint { lat, lng },
        radius_m,
        limit,
        filter,
    })
}

/// The DEFERRED half of `near`: no `scope`, no `v8::`, no `OpResult`.
///
/// Like [`run_search`], this still names both backends by their accessors. That
/// belongs to the backend-downcast inversion, not to this cut.
pub(crate) async fn run_near(
    binding: DbBinding,
    coll: String,
    plan: NearPlan,
) -> Result<read_pipeline::ApplyResult, DbError> {
    let NearPlan {
        field,
        point,
        radius_m,
        limit,
        filter,
    } = plan;

    // As in `run_search`: `BackendHandle` implements `SpatialIndex`, so the
    // vendor branch and the SQLite ATTACH prelude live in the vendor tier.
    let backend = crate::exec::ensure_backend_for_shared_sql().await?;
    use crate::backend::SpatialIndex as _;
    let rows = backend
        .spatial_near(&binding, &coll, &field, point, radius_m, &filter, limit)
        .await?;

    // Metering, success arm only. The search family is a read op on
    // either backend and reaches the database WITHOUT passing
    // through `exec::run_sql` - the PG arm goes to
    // `PostgresBackend::query_roled_json`, the SQLite arm to its own
    // scan - so until 2026-09-01 it was billed as nothing at all.
    // Counted here at the op boundary rather than in either vendor:
    // the vendor tier must not reach up into the engine for the
    // meter handle, which is the cycle #110 just removed.
    //
    // Position preserved from the hand-rolled `match result`: after the search,
    // before the read pipeline.
    crate::metrics::emit_db_metric(binding.app_id(), crate::metrics::DB_READS, 1);

    read_pipeline::apply(
        &binding,
        &coll,
        rows,
        read_pipeline::ApplyOptions::default(),
    )
    .await
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
    let backend = crate::exec::ensure_backend_for_shared_sql().await?;
    if let Some(pg) = backend.as_encrypted_column_pg() {
        return crate::crud::encryption_pass::encrypt_row_on_write_with_sidechannel(
            pg,
            app_id,
            collection,
            schema,
            row_pk,
            doc,
            sidechannel,
        )
        .await;
    }
    if let Some(sq) = backend.as_encrypted_column_sqlite() {
        return crate::crud::encryption_pass::encrypt_row_on_write_with_sidechannel(
            sq,
            app_id,
            collection,
            schema,
            row_pk,
            doc,
            sidechannel,
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
    fn configured_sqlite_dialect_does_not_require_an_open_backend() {
        crate::reset_context_for_tests();
        crate::set_db_url_for_tests("sqlite::memory:");
        assert!(crate::context::with(|context| context.backend()).is_none());
        assert_eq!(current_sql_dialect(), query::SqlDialect::Sqlite);
        crate::reset_context_for_tests();
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

        assert_eq!(
            filter["$and"][0]["active"]["$in"],
            serde_json::json!([1, 0])
        );
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
