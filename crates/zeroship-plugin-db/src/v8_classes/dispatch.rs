//! The V8 boundary for every `Collection` CRUD method.
//!
//! Each `dispatch_*` here is the bridge between a `#[v8_class]` method on
//! [`super::collection`] and the query pipeline in [`crate::crud`]. Every one
//! has the same three-part shape, and the split between the parts is the
//! crate boundary this module exists to draw:
//!
//! 1. **Eager, on the V8 scope.** Grab the runtime state slot, allocate the
//!    promise + resolver, and freeze the transaction route while `scope` is
//!    still live ([`crate::tx_scope::capture_route`]). Anything that must be
//!    observed synchronously - the read-set record, the DB-3 actor fence -
//!    happens here, in a `plan_*` call, because it cannot be moved past an
//!    `await`.
//! 2. **The async tail**, which names no `v8::` type at all: a `run_*` future
//!    from [`crate::crud`] that talks to the backend.
//! 3. **The lowering**, a closure handed to `settle` / `run_op` that turns the
//!    engine's data into a `ResolveValue`.
//!
//! **Parts 2 and 3 are the engine; part 1 is the adapter, and it is the only
//! part that may name `v8`.** These functions lived in `crud/mod.rs` until
//! 2026-09-02, which put 20 `v8::` signature positions inside an
//! ENGINE-tiered file and was the single largest entry in the tier census -
//! 40 of 80 violations, counting the `zeroship_runtime` and upward-dependency
//! rows they dragged along. Nothing about them changed in the move; they were
//! already thin. They were simply in the wrong file to be compiled into a
//! vendor-neutral crate.
//!
//! The capability gate (`refuse_if_query_capability`) is enforced by the
//! `#[v8_class]` methods *before* reaching a helper here - write ops trust
//! their callers.

use std::future::Future;

use serde_json::Value;
use zeroship_runtime::state::{OpResult, ResolveValue};

use zeroship_data_core::binding::DbBinding;
use zeroship_data_core::error::DbError;

use crate::crud::{
    aggregate_group_fields, exec_aggregate_read, exec_distinct_read, exec_mutation_then_read,
    plan_aggregate, plan_count, plan_delete_many, plan_delete_one, plan_distinct, plan_find,
    plan_near, plan_purge_many, plan_purge_one, plan_restore_many, plan_restore_one, plan_search,
    read_pipeline, run_find, run_insert, run_insert_many, run_near, run_search, run_update_many,
    run_update_one, run_upsert, system_fields_pass,
};
use crate::exec::{exec_count, exec_mutation_with_emit};
use crate::op_error::ToOpError;
use crate::query;
use crate::v8_bridge::{runtime_state, setup_js_promise};

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
    let plan = plan_find(&binding, collection, &filter, &opts);

    let app_id = binding.app_id();
    let state = runtime_state(scope);
    let (resolver, request_id, promise) = setup_js_promise(scope, &state);
    // Routing decision frozen HERE, while `scope` is live: see `crate::tx_route`.
    let route = crate::tx_scope::capture_route(scope, app_id);
    let coll = collection.to_string();

    state.borrow_mut().spawned_ops.push(Box::pin(settle(
        resolver,
        request_id,
        run_find(binding, coll, route, filter, plan),
        |result| crate::v8_bridge::rows_as_json_array_masked(result.rows, result.has_masked),
    )));

    promise
}
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
    // Routing decision frozen HERE, while `scope` is live: see `crate::tx_route`.
    let route = crate::tx_scope::capture_route(scope, app_id);

    // Read the request-bound actor id at the synchronous
    // boundary BEFORE the async tail starts. The runtime's
    // `executing_request_id` is only guaranteed-set on the pump turn
    // that initiates the dispatch; once we `.await` (e.g. the
    // encryption pass's `resolve_key` round-trip), the pump may rotate
    // the slot. Reading here pins the actor to the request that
    // originated the insert.
    let actor_id = system_fields_pass::current_actor_id(&state);

    state.borrow_mut().spawned_ops.push(Box::pin(settle(
        resolver,
        request_id,
        run_insert(binding, coll, route, doc, actor_id),
        |result| crate::v8_bridge::first_row_or_null_masked(result.rows, result.has_masked),
    )));

    promise
}
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
    // Routing decision frozen HERE, while `scope` is live: see `crate::tx_route`.
    let route = crate::tx_scope::capture_route(scope, app_id);
    let actor_id = system_fields_pass::current_actor_id(&state);

    state.borrow_mut().spawned_ops.push(Box::pin(settle(
        resolver,
        request_id,
        run_insert_many(binding, coll, route, docs, actor_id),
        |result| crate::v8_bridge::rows_as_json_array_masked(result.rows, result.has_masked),
    )));

    promise
}
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
    // Routing decision frozen HERE, while `scope` is live: see `crate::tx_route`.
    let route = crate::tx_scope::capture_route(scope, app_id);

    // Read actor at the sync boundary (same rationale as
    // `dispatch_insert`'s actor pin: the runtime's `executing_request_id`
    // rotates on the next pump turn).
    let actor_id = system_fields_pass::current_actor_id(&state);

    state.borrow_mut().spawned_ops.push(Box::pin(settle(
        resolver,
        request_id,
        run_update_one(binding, coll, route, filter, update, actor_id),
        |(rows, has_masked)| crate::v8_bridge::first_row_or_null_masked(rows, has_masked),
    )));

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
    // Routing decision frozen HERE, while `scope` is live: see `crate::tx_route`.
    let route = crate::tx_scope::capture_route(scope, app_id);

    // Actor read at sync boundary (mirrors
    // `dispatch_update_one`'s rationale).
    let actor_id = system_fields_pass::current_actor_id(&state);

    state.borrow_mut().spawned_ops.push(Box::pin(settle(
        resolver,
        request_id,
        run_update_many(binding, coll, route, filter, update, actor_id),
        crate::v8_bridge::usize_count_as_f64,
    )));

    promise
}
/// The ADAPTER half of `delete_one`. See [`dispatch_purge_one`] for the
/// outstanding second cut on the `async move` body below.
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
    // Routing decision frozen HERE, while `scope` is live: see `crate::tx_route`.
    let route = crate::tx_scope::capture_route(scope, app_id);
    let actor_id = system_fields_pass::current_actor_id(&state);

    let built = plan_delete_one(&binding, &coll, filter, actor_id.as_deref());
    state.borrow_mut().spawned_ops.push(Box::pin(run_op(
        resolver,
        request_id,
        built,
        // Tagged as Update because soft-delete IS an UPDATE
        // setting `deleted_at`. Subscribers wanting to react
        // to soft-deletes inspect `new_tuple.deleted_at`.
        move |bq| {
            exec_mutation_then_read(
                binding,
                coll,
                route,
                bq,
                zeroship_core::change_event::ChangeOp::Update,
            )
        },
        |result: read_pipeline::ApplyResult| {
            crate::v8_bridge::first_row_or_null_masked(result.rows, result.has_masked)
        },
    )));

    promise
}
/// The ADAPTER half of `delete_many`. See [`dispatch_purge_one`] for the
/// outstanding second cut on the `async move` body.
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
    // Routing decision frozen HERE, while `scope` is live: see `crate::tx_route`.
    let route = crate::tx_scope::capture_route(scope, app_id);
    let actor_id = system_fields_pass::current_actor_id(&state);

    let built = plan_delete_many(&binding, &coll, filter, actor_id.as_deref());
    state.borrow_mut().spawned_ops.push(Box::pin(run_op(
        resolver,
        request_id,
        built,
        move |bq| async move {
            exec_mutation_with_emit(
                bq,
                &route,
                &coll,
                zeroship_core::change_event::ChangeOp::Update,
            )
            .await
        },
        crate::v8_bridge::row_count_as_f64,
    )));

    promise
}
/// The ADAPTER half of `purge_one`.
///
/// STILL CARRYING PIPELINE: the `async move` body below runs
/// `exec_mutation_with_emit` and `read_pipeline::apply`, which
/// `docs/proposals/2026-08-31-data-crate-shape.md:139-140` puts in the engine -
/// "their `async move` bodies are not [the boundary] - those bodies are query
/// pipeline". Extracting the plan is the first cut; hoisting these bodies into
/// named engine functions is a SECOND cut this family still needs, and
/// `dispatch_count` has the same debt.
pub(crate) fn dispatch_purge_one<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    binding: DbBinding,
    collection: &str,
    filter: Value,
) -> v8::Local<'s, v8::Promise> {
    let built = plan_purge_one(&binding, collection, filter);

    let app_id = binding.app_id();
    let state = runtime_state(scope);
    let (resolver, request_id, promise) = setup_js_promise(scope, &state);
    let coll = collection.to_string();
    // Routing decision frozen HERE, while `scope` is live: see `crate::tx_route`.
    let route = crate::tx_scope::capture_route(scope, app_id);

    state.borrow_mut().spawned_ops.push(Box::pin(run_op(
        resolver,
        request_id,
        built,
        move |bq| {
            exec_mutation_then_read(
                binding,
                coll,
                route,
                bq,
                zeroship_core::change_event::ChangeOp::Delete,
            )
        },
        |result: read_pipeline::ApplyResult| {
            crate::v8_bridge::first_row_or_null_masked(result.rows, result.has_masked)
        },
    )));

    promise
}
/// The ADAPTER half of `purge_many`. See [`dispatch_purge_one`] for the
/// outstanding second cut on the `async move` body.
pub(crate) fn dispatch_purge_many<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    binding: DbBinding,
    collection: &str,
    filter: Value,
) -> v8::Local<'s, v8::Promise> {
    let built = plan_purge_many(&binding, collection, filter);

    let app_id = binding.app_id();
    let state = runtime_state(scope);
    let (resolver, request_id, promise) = setup_js_promise(scope, &state);
    let coll = collection.to_string();
    // Routing decision frozen HERE, while `scope` is live: see `crate::tx_route`.
    let route = crate::tx_scope::capture_route(scope, app_id);

    state.borrow_mut().spawned_ops.push(Box::pin(run_op(
        resolver,
        request_id,
        built,
        move |bq| async move {
            exec_mutation_with_emit(
                bq,
                &route,
                &coll,
                zeroship_core::change_event::ChangeOp::Delete,
            )
            .await
        },
        crate::v8_bridge::row_count_as_f64,
    )));

    promise
}
/// The ADAPTER half of `restore_one`. See [`dispatch_purge_one`] for the
/// outstanding second cut on the `async move` body.
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
    // Routing decision frozen HERE, while `scope` is live: see `crate::tx_route`.
    let route = crate::tx_scope::capture_route(scope, app_id);
    let actor_id = system_fields_pass::current_actor_id(&state);

    let built = plan_restore_one(&binding, &coll, filter, actor_id.as_deref());
    state.borrow_mut().spawned_ops.push(Box::pin(run_op(
        resolver,
        request_id,
        built,
        move |bq| {
            exec_mutation_then_read(
                binding,
                coll,
                route,
                bq,
                zeroship_core::change_event::ChangeOp::Update,
            )
        },
        |result: read_pipeline::ApplyResult| {
            crate::v8_bridge::first_row_or_null_masked(result.rows, result.has_masked)
        },
    )));

    promise
}
/// The ADAPTER half of `restore_many`. See [`dispatch_purge_one`] for the
/// outstanding second cut on the `async move` body.
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
    // Routing decision frozen HERE, while `scope` is live: see `crate::tx_route`.
    let route = crate::tx_scope::capture_route(scope, app_id);
    let actor_id = system_fields_pass::current_actor_id(&state);

    let built = plan_restore_many(&binding, &coll, filter, actor_id.as_deref());
    state.borrow_mut().spawned_ops.push(Box::pin(run_op(
        resolver,
        request_id,
        built,
        move |bq| async move {
            exec_mutation_with_emit(
                bq,
                &route,
                &coll,
                zeroship_core::change_event::ChangeOp::Update,
            )
            .await
        },
        crate::v8_bridge::row_count_as_f64,
    )));

    promise
}
/// The ADAPTER half of `aggregate`. See [`dispatch_purge_one`] for the
/// outstanding second cut on the `async move` body.
pub(crate) fn dispatch_aggregate<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    binding: DbBinding,
    collection: &str,
    pipeline: Value,
    opts: Value,
) -> v8::Local<'s, v8::Promise> {
    let planned = plan_aggregate(&binding, collection, &pipeline, &opts);

    let app_id = binding.app_id();
    let state = runtime_state(scope);
    let (resolver, request_id, promise) = setup_js_promise(scope, &state);
    // Routing decision frozen HERE, while `scope` is live: see `crate::tx_route`.
    let route = crate::tx_scope::capture_route(scope, app_id);
    let coll = collection.to_string();
    let group_fields = aggregate_group_fields(&pipeline);
    let (built, result_columns): (_, Option<Vec<String>>) = match planned {
        Ok((bq, cols)) => (Ok(bq), cols),
        Err(e) => (Err(e), None),
    };

    state.borrow_mut().spawned_ops.push(Box::pin(run_op(
        resolver,
        request_id,
        built,
        move |bq| exec_aggregate_read(binding, coll, route, bq, group_fields, result_columns),
        |result: read_pipeline::ApplyResult| {
            crate::v8_bridge::rows_as_json_array_masked(result.rows, result.has_masked)
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
    let planned = plan_distinct(&binding, collection, field, filter, &opts);

    let app_id = binding.app_id();
    let state = runtime_state(scope);
    let (resolver, request_id, promise) = setup_js_promise(scope, &state);
    // Routing decision frozen HERE, while `scope` is live: see `crate::tx_route`.
    let route = crate::tx_scope::capture_route(scope, app_id);
    let coll = collection.to_string();
    // The `false` is unobservable, not a default: on the error arm `run_op`
    // rejects before it ever calls the closure that reads this flag.
    let (built, distinct_reads_masked_sibling) = match planned {
        Ok((bq, masked)) => (Ok(bq), masked),
        Err(e) => (Err(e), false),
    };

    state.borrow_mut().spawned_ops.push(Box::pin(run_op(
        resolver,
        request_id,
        built,
        move |bq| exec_distinct_read(binding, coll, route, bq, distinct_reads_masked_sibling),
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
            crate::v8_bridge::maybe_rehydrate(Value::Array(flat).to_string(), result.has_masked)
        },
    )));

    promise
}
/// The ADAPTER half of `count`: the V8 boundary and nothing else.
///
/// Owns the promise, freezes the route while `scope` is live, spawns the op, and
/// lowers the engine's `i64` into a `ResolveValue`. Every line of query pipeline
/// lives in [`plan_count`].
pub(crate) fn dispatch_count<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    binding: DbBinding,
    collection: &str,
    filter: Value,
    opts: Value,
) -> v8::Local<'s, v8::Promise> {
    let built = plan_count(&binding, collection, filter, &opts);

    let app_id = binding.app_id();
    let state = runtime_state(scope);
    let (resolver, request_id, promise) = setup_js_promise(scope, &state);
    // Routing decision frozen HERE, while `scope` is live: see `crate::tx_route`.
    let route = crate::tx_scope::capture_route(scope, app_id);

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
    // Routing decision frozen HERE, while `scope` is live: see `crate::tx_route`.
    let route = crate::tx_scope::capture_route(scope, app_id);

    state.borrow_mut().spawned_ops.push(Box::pin(settle(
        resolver,
        request_id,
        run_upsert(binding, coll, route, doc, conflict_fields, actor_id),
        |result| crate::v8_bridge::first_row_or_null_masked(result.rows, result.has_masked),
    )));

    promise
}
pub(crate) fn dispatch_search<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    binding: DbBinding,
    collection: &str,
    args: Value,
) -> v8::Local<'s, v8::Promise> {
    let planned = plan_search(&binding, collection, &args);

    let state = runtime_state(scope);
    let (resolver, request_id, promise) = setup_js_promise(scope, &state);
    let coll = collection.to_string();

    state.borrow_mut().spawned_ops.push(Box::pin(settle(
        resolver,
        request_id,
        async move { run_search(binding, coll, planned?).await },
        |result| crate::v8_bridge::rows_as_json_array_masked(result.rows, result.has_masked),
    )));

    promise
}
pub(crate) fn dispatch_near<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    binding: DbBinding,
    collection: &str,
    args: Value,
) -> v8::Local<'s, v8::Promise> {
    let planned = plan_near(&binding, collection, &args);

    let state = runtime_state(scope);
    let (resolver, request_id, promise) = setup_js_promise(scope, &state);
    let coll = collection.to_string();

    state.borrow_mut().spawned_ops.push(Box::pin(settle(
        resolver,
        request_id,
        async move { run_near(binding, coll, planned?).await },
        |result| crate::v8_bridge::rows_as_json_array_masked(result.rows, result.has_masked),
    )));

    promise
}
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
/// Settle one dispatch: run the engine's work, then let the ADAPTER decide how
/// its result reaches V8.
///
/// This is the whole of the completion protocol, and the shape every dispatch
/// should have. `work` is engine code - it returns `Result<R, DbError>` and
/// names no runtime type. `resolve` is adapter code - it is the only thing that
/// may build a `ResolveValue`. Nothing in between knows about V8.
///
/// [`run_op`] is the special case where the query can be built synchronously in
/// the prologue, before the future starts. Nine dispatches are shaped that way.
/// The other eight cannot be: they `await` before a query exists - `insert_many`
/// prepares documents, `find` resolves a descriptor - so a `build_result`
/// computed up front is not available to them. That is the only reason they
/// hand-rolled `OpResult` construction, and this is what they hand-rolled it
/// INTO, badly: every one of them repeated the same three-arm match over
/// `Ok`/`Err` and rebuilt `OpResult::JsValue` by hand.
///
/// Prefer this over `run_op` in new code. `run_op` is expressible in terms of it
/// and is kept only because nine call sites read well with the build/exec split.
pub(crate) async fn settle<R, Fut, Resolve>(
    resolver: v8::Global<v8::PromiseResolver>,
    request_id: Option<u64>,
    work: Fut,
    resolve: Resolve,
) -> OpResult
where
    Fut: Future<Output = Result<R, DbError>>,
    Resolve: FnOnce(R) -> ResolveValue,
{
    match work.await {
        Ok(v) => OpResult::JsValue {
            resolver,
            value: resolve(v),
            request_id,
        },
        Err(e) => reject_op(resolver, request_id, e),
    }
}

pub(crate) async fn run_op<R, EFut, Resolve>(
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
pub(crate) fn reject_op(
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
