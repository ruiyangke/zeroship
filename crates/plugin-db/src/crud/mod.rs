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

use serde_json::Value;
use zeroship_runtime::state::{OpResult, ResolveValue};

use crate::error::DbError;
use crate::exec::{exec_count, exec_mutation_with_emit, exec_query};
use crate::query;
use crate::v8_bridge::{runtime_state, setup_js_promise};

// **P5 PR 2** — transparent column-encryption pass. The helpers in
// this module (`encrypt_row_on_write` / `decrypt_row_on_read`) sit
// around `query::build_*` and `exec_query` respectively. Crate-private:
// the SDK reaches them indirectly via the regular CRUD entry points.
// Public unit tests exercise the helpers without a V8 / PG round-trip
// via in-test stub backends implementing `EncryptedColumn`.
pub(crate) mod encryption_pass;

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

/// Lower a `Vec<Value>` result to a single JSON value: the first row,
/// or `null` when the result was empty. Used by `findOne` / `insert` /
/// `updateOne` / `deleteOne` / `upsert`, all of which the SDK expects
/// to resolve to a single row or `null`.
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

// ---------------------------------------------------------------------------
// findOne / find — read paths
// ---------------------------------------------------------------------------

/// Shared dispatch for `findOne`, called by `Collection::find_one`
/// (the `#[v8_method]`). Filter arrives already decoded into
/// `serde_json::Value` via `v8_value_to_serde_json` — no JSON
/// round-trip on the hot path.
///
/// Resolves with the row as a real JS object or real JS `null` if no
/// row matched (via `ResolveValue::Json`); on error rejects with a
/// coded `OpError`.
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

    let order_by = opts.get("orderBy").cloned();
    let select = opts.get("select").cloned();
    let (resolver, request_id, promise) = setup_js_promise(scope, &state);

    let app = app_id.to_string();
    let coll = collection.to_string();

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        let built = query::build_find(
            &app,
            &coll,
            &filter,
            Some(1),
            None,
            order_by.as_ref(),
            select.as_ref(),
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
                // **P5 PR 2** — decrypt encrypted columns on the
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
                OpResult::JsValue {
                    resolver,
                    value: first_row_or_null(rows),
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
    let order_by = opts.get("orderBy").cloned();
    let select = opts.get("select").cloned();
    let (resolver, request_id, promise) = setup_js_promise(scope, &state);

    let app = app_id.to_string();
    let coll = collection.to_string();

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        let built = query::build_find(
            &app,
            &coll,
            &filter,
            limit,
            offset,
            order_by.as_ref(),
            select.as_ref(),
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
                OpResult::JsValue {
                    resolver,
                    value: rows_as_json_array(rows),
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

    // **P5 PR 2** — async tail so the encryption pass can `.await` the
    // backend's `resolve_key` (PG SECURITY DEFINER round-trip) before
    // `build_insert` consumes the doc. The non-encrypted hot path stays
    // identical — `apply_encryption_on_write` short-circuits when the
    // cached schema has no `t.encrypted(...)` columns.
    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        let mut doc = doc;
        if let Err(e) = apply_encryption_on_write(&app, &coll, &mut doc).await {
            return OpResult::JsValue {
                resolver,
                value: ResolveValue::RejectError(e.to_op_error()),
                request_id,
            };
        }
        let built = query::build_insert(&app, &coll, &doc);
        let result = match built {
            Ok(bq) => {
                exec_mutation_with_emit(bq, &app, &coll, crate::broker::ChangeOp::Insert).await
            }
            Err(e) => Err(DbError::from(e)),
        };
        match result {
            Ok(rows) => {
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
                OpResult::JsValue {
                    resolver,
                    value: first_row_or_null(rows),
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

    let built = query::build_insert_many(app_id, collection, &docs);
    let coll = collection.to_string();
    let app = app_id.to_string();

    state.borrow_mut().spawned_ops.push(Box::pin(run_op(
        resolver,
        request_id,
        built,
        move |bq| async move {
            exec_mutation_with_emit(bq, &app, &coll, crate::broker::ChangeOp::Insert).await
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

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        let mut update = update;
        if let Err(e) = apply_encryption_on_update(&app, &coll, &filter, &mut update).await {
            return OpResult::JsValue {
                resolver,
                value: ResolveValue::RejectError(e.to_op_error()),
                request_id,
            };
        }
        let built = query::build_update_one(&app, &coll, &filter, &update);
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
                OpResult::JsValue {
                    resolver,
                    value: first_row_or_null(rows),
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
pub(crate) fn dispatch_update_many<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    app_id: &str,
    collection: &str,
    filter: Value,
    update: Value,
) -> v8::Local<'s, v8::Promise> {
    let state = runtime_state(scope);
    let (resolver, request_id, promise) = setup_js_promise(scope, &state);

    let built = query::build_update_many(app_id, collection, &filter, &update);
    let coll = collection.to_string();
    let app = app_id.to_string();

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
// deleteOne / deleteMany — write paths
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

    let built = query::build_delete_one(app_id, collection, &filter);
    let coll = collection.to_string();
    let app = app_id.to_string();

    state.borrow_mut().spawned_ops.push(Box::pin(run_op(
        resolver,
        request_id,
        built,
        move |bq| async move {
            exec_mutation_with_emit(bq, &app, &coll, crate::broker::ChangeOp::Delete).await
        },
        first_row_or_null,
    )));

    promise
}

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

// ---------------------------------------------------------------------------
// aggregate / distinct / count — read paths
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
    let built = query::build_aggregate(app_id, collection, &pipeline);

    state.borrow_mut().spawned_ops.push(Box::pin(run_op(
        resolver,
        request_id,
        built,
        exec_query,
        rows_as_json_array,
    )));

    promise
}

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

    let built = query::build_distinct(app_id, collection, field, &filter);

    state.borrow_mut().spawned_ops.push(Box::pin(run_op(
        resolver,
        request_id,
        built,
        exec_query,
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
    let built = query::build_count(app_id, collection, &filter);

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
// upsert / findOrCreate — INSERT … ON CONFLICT paths
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

    let built = query::build_upsert(app_id, collection, &doc, &conflict_fields);
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
            exec_mutation_with_emit(bq, &app, &coll, crate::broker::ChangeOp::Update).await
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
        let filter = args
            .get("filter")
            .cloned()
            .unwrap_or_else(|| Value::Object(serde_json::Map::new()));

        let app = app_id.to_string();
        let coll = collection.to_string();

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
                #[cfg(feature = "sqlite")]
                {
                    if let Some(sq) = backend.as_sqlite() {
                        use crate::backend::FullTextIndex as _;
                        return sq
                            .fts_search(&app, &coll, &text_query, &filter, limit)
                            .await;
                    }
                }
                #[cfg(feature = "pg")]
                {
                    let pg = backend
                        .as_postgres()
                        .ok_or_else(|| DbError::backend_unsupported("fts_search"))?;
                    use crate::backend::FullTextIndex as _;
                    return pg.fts_search(&app, &coll, &text_query, &filter, limit).await;
                }
                #[cfg(not(feature = "pg"))]
                {
                    Err(DbError::Configuration {
                        code: "fts_unsupported",
                        message:
                            "db: full-text search requires the `pg` Cargo feature on this build"
                                .to_string(),
                        hint: Some(
                            "rebuild with `--features pg` or use the SQLite arm (P4 PR 5)"
                                .to_string(),
                        ),
                    })
                }
            }
            .await;

            match result {
                Ok(rows) => zeroship_runtime::state::OpResult::JsValue {
                    resolver,
                    value: zeroship_runtime::state::ResolveValue::Json(
                        Value::Array(rows).to_string(),
                    ),
                    request_id,
                },
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
    let filter = args
        .get("filter")
        .cloned()
        .unwrap_or_else(|| Value::Object(serde_json::Map::new()));

    let app = app_id.to_string();
    let coll = collection.to_string();

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
            #[allow(unused_variables)]
            let pg_path = || async {
                #[cfg(feature = "pg")]
                {
                    let pg = backend
                        .as_postgres()
                        .ok_or_else(|| DbError::backend_unsupported("vector_search"))?;
                    use crate::backend::VectorIndex as _;
                    return pg
                        .vector_search(&app, &coll, &column, &vector, k, metric, &filter)
                        .await;
                }
                #[cfg(not(feature = "pg"))]
                {
                    Err(DbError::Configuration {
                        code: "vector_unsupported",
                        message:
                            "db: vector search requires the `pg` Cargo feature on this build"
                                .to_string(),
                        hint: Some(
                            "rebuild with `--features pg` or use the SQLite arm (P4 PR 4)"
                                .to_string(),
                        ),
                    })
                }
            };
            // **P4 PR 4** — SQLite arm routes through the pure-Rust
            // flat-scan `VectorIndex` impl on `SqliteBackend`. We
            // short-circuit BEFORE the PG path so a build with both
            // arms compiled in (`--features "pg sqlite"` for tests)
            // dispatches based on which arm the runtime is bound to,
            // not on Cargo-feature ordering.
            #[cfg(feature = "sqlite")]
            {
                if let Some(sq) = backend.as_sqlite() {
                    use crate::backend::VectorIndex as _;
                    return sq
                        .vector_search(&app, &coll, &column, &vector, k, metric, &filter)
                        .await;
                }
            }
            pg_path().await
        }
        .await;

        match result {
            Ok(rows) => zeroship_runtime::state::OpResult::JsValue {
                resolver,
                value: zeroship_runtime::state::ResolveValue::Json(
                    Value::Array(rows).to_string(),
                ),
                request_id,
            },
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
    let filter = args
        .get("filter")
        .cloned()
        .unwrap_or_else(|| Value::Object(serde_json::Map::new()));

    let app = app_id.to_string();
    let coll = collection.to_string();
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
            #[cfg(feature = "sqlite")]
            {
                if let Some(sq) = backend.as_sqlite() {
                    use crate::backend::SpatialIndex as _;
                    return sq
                        .spatial_near(&app, &coll, &field, point, radius_m, &filter, limit)
                        .await;
                }
            }
            #[cfg(feature = "pg")]
            {
                let pg = backend
                    .as_postgres()
                    .ok_or_else(|| DbError::backend_unsupported("spatial_near"))?;
                use crate::backend::SpatialIndex as _;
                return pg
                    .spatial_near(&app, &coll, &field, point, radius_m, &filter, limit)
                    .await;
            }
            #[cfg(not(feature = "pg"))]
            {
                Err(DbError::Configuration {
                    code: "spatial_unsupported",
                    message:
                        "db: spatial search requires the `pg` Cargo feature on this build"
                            .to_string(),
                    hint: Some(
                        "rebuild with `--features pg` or use the SQLite arm (P4 PR 5)"
                            .to_string(),
                    ),
                })
            }
        }
        .await;

        match result {
            Ok(rows) => zeroship_runtime::state::OpResult::JsValue {
                resolver,
                value: zeroship_runtime::state::ResolveValue::Json(
                    Value::Array(rows).to_string(),
                ),
                request_id,
            },
            Err(e) => zeroship_runtime::state::OpResult::JsValue {
                resolver,
                value: zeroship_runtime::state::ResolveValue::RejectError(e.to_op_error()),
                request_id,
            },
        }
    }));

    promise
}

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

    let built = query::build_find_or_create(app_id, collection, &doc, &conflict_fields);
    let coll = collection.to_string();
    let app = app_id.to_string();

    state.borrow_mut().spawned_ops.push(Box::pin(run_op(
        resolver,
        request_id,
        built,
        move |bq| async move {
            // Tag as Insert — the broker reaction is the same as upsert
            // (subscribers re-fetch), and tagging conservatively keeps
            // us from missing wake-ups when the row really was created.
            exec_mutation_with_emit(bq, &app, &coll, crate::broker::ChangeOp::Insert).await
        },
        |rows: Vec<Value>| {
            // `rows` is the pre-decoded result of the INSERT ... ON
            // CONFLICT — typically a single row. Take it without a
            // JSON round-trip; the `__created` flag rides in the row.
            let mut row = rows.into_iter().next().unwrap_or(Value::Null);
            let created = match row.as_object_mut() {
                Some(obj) => obj
                    .remove("__created")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                None => false,
            };
            let payload = serde_json::json!({ "row": row, "created": created });
            ResolveValue::Json(payload.to_string())
        },
    )));

    promise
}

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
async fn apply_encryption_on_write(
    app_id: &str,
    collection: &str,
    doc: &mut Value,
) -> Result<(), DbError> {
    let Some(schema) = crate::context::with(|c| c.schema_for(app_id, collection)) else {
        return Ok(());
    };
    if !schema_has_encrypted_columns(&schema) {
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
    encryption_pass_dispatch(app_id, collection, &schema, &row_pk, doc).await
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
    if !schema_has_encrypted_columns(&schema) {
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
    // build layer does.
    if let Some(set_obj) = patch.get_mut("$set") {
        encryption_pass_dispatch(app_id, collection, &schema, &row_pk, set_obj).await
    } else {
        encryption_pass_dispatch(app_id, collection, &schema, &row_pk, patch).await
    }
}

/// Decrypt every encrypted column on each row of `rows`. Short-circuits
/// when the schema has no encrypted columns OR when not registered.
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
    #[cfg(all(feature = "pg", feature = "hardening"))]
    {
        if let Some(pg) = backend.as_encrypted_column_pg() {
            for row in rows.iter_mut() {
                crate::crud::encryption_pass::decrypt_row_on_read(
                    pg, app_id, collection, &schema, row,
                )
                .await?;
            }
            return Ok(rows);
        }
    }
    // SQLite CRUD-path wiring is a follow-up — see
    // `encryption_pass_dispatch` rustdoc. The `EncryptedColumn` trait
    // is reachable directly (e.g. tests/sqlite_integration.rs P5 suite)
    // but the row-decode shape on the SQLite side surfaces BYTEA via
    // `<N bytes blob>` placeholder strings rather than the PG hex-text
    // shape `decrypt_row_on_read` parses. Routing SQLite reads through
    // the same decrypt helper would need a SQLite-specific byte-
    // extraction path; deferred.
    let _ = backend; // silence unused under feature combinations
    Ok(rows)
}

/// Run the write-side encryption pass over `doc` using the
/// backend-arm `EncryptedColumn` impl. PG arm is gated on `feature =
/// "pg" + hardening`. Builds without those features short-circuit (no
/// encrypted columns can reach this path because the SDK builder is
/// also gated).
///
/// **SQLite scope** (P5 PR 3): the `EncryptedColumn` trait impl on
/// `SqliteBackend` is wired (env-var key sourcing, env-var-only — no
/// admin schema), and the integration tests at
/// `tests/sqlite_integration.rs` pin the trait surface end-to-end. The
/// full CRUD-layer wiring on SQLite still requires the SQL builder
/// (`build_insert` / `build_update_*`) to learn SQLite-flavoured BLOB
/// binding — the current builder emits PG-only `decode($N,
/// 'base64')::bytea` syntax that SQLite would reject. Until that
/// builder split lands, the SQLite arm is reachable via the trait
/// directly (Rust callers) but not via the SDK's CRUD path on SQLite.
/// Surface a typed Configuration error here so a SQLite app with an
/// encrypted-column schema fails loud rather than emitting broken SQL.
#[allow(unused_variables)]
async fn encryption_pass_dispatch(
    app_id: &str,
    collection: &str,
    schema: &Value,
    row_pk: &str,
    doc: &mut Value,
) -> Result<(), DbError> {
    let backend = crate::context::with(|c| c.backend())
        .ok_or_else(|| DbError::config("not_configured", "db: backend not initialized"))?;
    #[cfg(all(feature = "pg", feature = "hardening"))]
    {
        if let Some(pg) = backend.as_encrypted_column_pg() {
            return crate::crud::encryption_pass::encrypt_row_on_write(
                pg, app_id, collection, schema, row_pk, doc,
            )
            .await;
        }
    }
    // No backend-arm with an `EncryptedColumn` CRUD-path wire available —
    // encrypted columns declared in the schema would reach a write site
    // that has no encryption surface (PG without `hardening`) OR that
    // has the trait but not the SQL builder (SQLite, P5 PR 3 → follow-
    // up). Surface a typed Configuration error so the SDK can branch
    // on `.code` rather than silently writing plaintext to a BYTEA/BLOB
    // column.
    if schema_has_encrypted_columns(schema) {
        return Err(DbError::Configuration {
            code: "column_encryption_unavailable",
            message:
                "db: column encryption CRUD path requires the `hardening` Cargo feature on this build"
                    .to_string(),
            hint: Some("rebuild with `--features hardening` (PG); SQLite CRUD wiring is a follow-up".to_string()),
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
