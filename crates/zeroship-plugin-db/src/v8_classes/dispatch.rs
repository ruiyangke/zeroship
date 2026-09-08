//! Decode native arguments and adapt the ORM result to a V8 promise.

use std::future::Future;

use serde_json::Value;
use zeroship_runtime::state::{OpResult, ResolveValue, SharedState};

use zeroship_data_core::binding::DbBinding;
use zeroship_data_core::error::DbError;

use crate::compile;
use crate::crud::mask_policy::dispatch_set_mask_policy;
use crate::crud::unmask::{dispatch_bulk_unmask, dispatch_unmask, parse_args, parse_bulk_args};
use crate::op_error::ToOpError;
use crate::v8_bridge::{runtime_state, setup_js_promise};
use zeroship_data_engine::orm::{Operation, Output, PreparedOperation};

/// Look up the current request's authenticated actor id (typed_id
/// string), if any.
///
/// Reads the runtime's `per_request_user` slot using the request id
/// currently bound by the pump (see `crates/zeroship-runtime/src/auth.rs` for
/// the wire contract). The user JSON shape is gateway-defined and
/// carries at minimum `{ "id": "usr_..." }` for an authenticated
/// user; we extract the `id` field and discard the rest (Q-SF-A:
/// "typed_id only" for `created_by` - only the id flows to the row,
/// not the role / display name / etc.).
///
/// Returns `None` when no request is bound (module init, raw
/// background dispatch), when no user is attached to the request
/// (anonymous), or when the user JSON is malformed. NULL is the
/// design choice for `created_by` in that case (§2.3 of the
/// proposal); the column is nullable so the INSERT succeeds.
///
/// **Lives here, in the adapter, because per-request identity is runtime
/// state.** It sat in `crud/system_fields_pass.rs` until 2026-09-02, where its
/// `&SharedState` parameter was the LAST signature in the ENGINE tier naming
/// the V8 runtime crate - the final row on
/// `tests/lib/tier_signature_census.sh`. All nine of its callers were already
/// in this file, so the move relocated a definition and nothing else: the
/// engine's write pass takes the actor id as an ARGUMENT and never learns where
/// it came from.
pub(crate) fn current_actor_id(state: &SharedState) -> Option<String> {
    let s = state.borrow();
    let rid = s.executing_request_id?;
    let user_json = s.per_request_user.get(&rid)?;
    let parsed: Value = serde_json::from_str(user_json).ok()?;
    parsed
        .get("id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn dispatch_operation<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    binding: DbBinding,
    collection: &str,
    operation: Operation,
    one: bool,
) -> v8::Local<'s, v8::Promise> {
    crate::v8_bridge::ensure_read_set_capture();
    let state = runtime_state(scope);
    let route = crate::tx_scope::capture_route(scope, &binding);
    let prepared = PreparedOperation::new(
        binding,
        collection,
        route,
        current_actor_id(&state),
        operation,
    );
    let (resolver, request_id, promise) = setup_js_promise(scope, &state);
    state.borrow_mut().spawned_ops.push(Box::pin(settle(
        resolver,
        request_id,
        async move {
            let prepared = prepared?;
            prepared
                .execute(crate::tx_scope::ensure_backend().await?)
                .await
        },
        move |output| match output {
            Output::Count(count) => ResolveValue::F64(count as f64),
            Output::Rows { rows, has_masked } if one => {
                crate::v8_bridge::first_row_or_null_masked(rows, has_masked)
            }
            Output::Rows { rows, has_masked } => {
                crate::v8_bridge::rows_as_json_array_masked(rows, has_masked)
            }
        },
    )));
    promise
}

pub(crate) fn dispatch_find<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    binding: DbBinding,
    collection: &str,
    filter: Value,
    opts: Value,
) -> v8::Local<'s, v8::Promise> {
    dispatch_operation(
        scope,
        binding,
        collection,
        Operation::Find {
            filter,
            options: opts,
        },
        false,
    )
}
pub(crate) fn dispatch_insert<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    binding: DbBinding,
    collection: &str,
    doc: Value,
) -> v8::Local<'s, v8::Promise> {
    dispatch_operation(
        scope,
        binding,
        collection,
        Operation::Insert { document: doc },
        true,
    )
}
pub(crate) fn dispatch_insert_many<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    binding: DbBinding,
    collection: &str,
    docs: Value,
) -> v8::Local<'s, v8::Promise> {
    dispatch_operation(
        scope,
        binding,
        collection,
        Operation::InsertMany { documents: docs },
        false,
    )
}
pub(crate) fn dispatch_update_one<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    binding: DbBinding,
    collection: &str,
    filter: Value,
    update: Value,
) -> v8::Local<'s, v8::Promise> {
    dispatch_operation(
        scope,
        binding,
        collection,
        Operation::Update {
            filter,
            patch: update,
            many: false,
        },
        true,
    )
}
pub(crate) fn dispatch_update_many<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    binding: DbBinding,
    collection: &str,
    filter: Value,
    update: Value,
) -> v8::Local<'s, v8::Promise> {
    dispatch_operation(
        scope,
        binding,
        collection,
        Operation::Update {
            filter,
            patch: update,
            many: true,
        },
        false,
    )
}
pub(crate) fn dispatch_delete_one<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    binding: DbBinding,
    collection: &str,
    filter: Value,
) -> v8::Local<'s, v8::Promise> {
    dispatch_operation(
        scope,
        binding,
        collection,
        Operation::Delete {
            filter,
            many: false,
        },
        true,
    )
}
pub(crate) fn dispatch_delete_many<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    binding: DbBinding,
    collection: &str,
    filter: Value,
) -> v8::Local<'s, v8::Promise> {
    dispatch_operation(
        scope,
        binding,
        collection,
        Operation::Delete { filter, many: true },
        false,
    )
}
pub(crate) fn dispatch_purge_one<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    binding: DbBinding,
    collection: &str,
    filter: Value,
) -> v8::Local<'s, v8::Promise> {
    dispatch_operation(
        scope,
        binding,
        collection,
        Operation::Purge {
            filter,
            many: false,
        },
        true,
    )
}
pub(crate) fn dispatch_purge_many<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    binding: DbBinding,
    collection: &str,
    filter: Value,
) -> v8::Local<'s, v8::Promise> {
    dispatch_operation(
        scope,
        binding,
        collection,
        Operation::Purge { filter, many: true },
        false,
    )
}
pub(crate) fn dispatch_restore_one<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    binding: DbBinding,
    collection: &str,
    filter: Value,
) -> v8::Local<'s, v8::Promise> {
    dispatch_operation(
        scope,
        binding,
        collection,
        Operation::Restore {
            filter,
            many: false,
        },
        true,
    )
}
pub(crate) fn dispatch_restore_many<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    binding: DbBinding,
    collection: &str,
    filter: Value,
) -> v8::Local<'s, v8::Promise> {
    dispatch_operation(
        scope,
        binding,
        collection,
        Operation::Restore { filter, many: true },
        false,
    )
}
pub(crate) fn dispatch_aggregate<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    binding: DbBinding,
    collection: &str,
    pipeline: Value,
    opts: Value,
) -> v8::Local<'s, v8::Promise> {
    dispatch_operation(
        scope,
        binding,
        collection,
        Operation::Aggregate {
            pipeline,
            options: opts,
        },
        false,
    )
}
pub(crate) fn dispatch_distinct<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    binding: DbBinding,
    collection: &str,
    field: &str,
    filter: Value,
    opts: Value,
) -> v8::Local<'s, v8::Promise> {
    dispatch_operation(
        scope,
        binding,
        collection,
        Operation::Distinct {
            field: field.to_owned(),
            filter,
            options: opts,
        },
        false,
    )
}
pub(crate) fn dispatch_count<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    binding: DbBinding,
    collection: &str,
    filter: Value,
    opts: Value,
) -> v8::Local<'s, v8::Promise> {
    dispatch_operation(
        scope,
        binding,
        collection,
        Operation::Count {
            filter,
            options: opts,
        },
        false,
    )
}
pub(crate) fn dispatch_upsert<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    binding: DbBinding,
    collection: &str,
    doc: Value,
    conflict_fields: Value,
) -> v8::Local<'s, v8::Promise> {
    dispatch_operation(
        scope,
        binding,
        collection,
        Operation::Upsert {
            document: doc,
            conflict_fields,
        },
        true,
    )
}
pub(crate) fn dispatch_search<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    binding: DbBinding,
    collection: &str,
    args: Value,
) -> v8::Local<'s, v8::Promise> {
    dispatch_operation(
        scope,
        binding,
        collection,
        Operation::Search { arguments: args },
        false,
    )
}
pub(crate) fn dispatch_near<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    binding: DbBinding,
    collection: &str,
    args: Value,
) -> v8::Local<'s, v8::Promise> {
    dispatch_operation(
        scope,
        binding,
        collection,
        Operation::Near { arguments: args },
        false,
    )
}
// ---------------------------------------------------------------------------

/// The shared "build → exec → resolve" tail every CRUD dispatcher
/// shares. Drives the spawned-op future, packs the result into an
/// `OpResult::JsValue`, and stamps `.code` on any `DbError` via
/// `to_op_error()` so the SDK sees `err.code` regardless of which
/// dispatcher threw.
///
/// `build_result` is the (already-evaluated) output of the
/// schema-resolution + `compile::build_*` chain. Builder errors are `QueryError`
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
    build_result: Result<compile::BuiltQuery, DbError>,
    exec: impl FnOnce(compile::BuiltQuery) -> EFut,
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

// ---------------------------------------------------------------------------
// The platform-internal dispatches: unmask and mask policy
// ---------------------------------------------------------------------------
//
// Reached from `DbPlatform`, not from `Collection` - they hang off the
// capability handle set on `Db` under a private symbol, so creator JS cannot
// name them. They are here for the same reason as everything above: the
// signature returns a `v8::Local<v8::Promise>`, so the function is boundary.
//
// Their engine halves stay in `crud::unmask` and `crud::mask_policy`, which is
// where the DB-3 fence lives - `sanitize_app_actor` runs inside `parse_args` /
// `parse_bulk_args`, below this layer, precisely so no dispatch site can
// forget it.

/// V8-facing dispatch helper. Returns the unresolved Promise; the
/// `dispatch_unmask` body runs as a spawned op and resolves with
/// `{ plaintext }` on success or rejects with the typed `OpError`.
///
/// Called from `v8_classes::db::Db::unmask_field` (the `#[v8_method]`
/// wrapping this entry point).
pub(crate) fn dispatch_unmask_field<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    binding: &DbBinding,
    args_v: Value,
) -> v8::Local<'s, v8::Promise> {
    let state = crate::v8_bridge::runtime_state(scope);
    let (resolver, request_id, promise) = crate::v8_bridge::setup_js_promise(scope, &state);

    // Parse the args eagerly, off the V8 stack, so a malformed shape is decided
    // before anything is spawned and cannot race the spawn.
    let parsed = parse_args(&args_v);
    let binding = binding.clone();
    // The route is captured HERE, on the adapter side, while the V8 frame is
    // live, and handed to the engine. `crud::unmask` used to open a backend
    // itself through `exec::ensure_backend_for_shared_sql`, which read
    // `crate::context` from an ENGINE file.
    //
    // **This captured NOTHING until 2026-09-03**, on the stated grounds that
    // "an unmask is not a routed statement". It is: the ciphertext read is a
    // SELECT, and one issued inside a `db.transaction(fn)` callback has to run
    // on that transaction's connection or it cannot see a row the transaction
    // has just written.
    let route = crate::tx_scope::capture_route(scope, &binding);

    // The parse error folds into `settle`'s error arm via `?`; it made the same
    // `reject_op` call the hand-rolled arm here did.
    state
        .borrow_mut()
        .spawned_ops
        .push(Box::pin(crate::v8_classes::dispatch::settle(
            resolver,
            request_id,
            async move {
                // `parsed?` is taken BEFORE the first await, so a malformed payload
                // rejects with its own typed parse error rather than whatever
                // `bind_route` happens to say on an isolate that cannot open a
                // backend (`not_configured` / `lazy_init_failed`). Writing it as
                // `dispatch_unmask(.., parsed?)` reads the same and is not: the `?`
                // then runs behind the await, and the backend error wins.
                let args = parsed?;
                let route = crate::tx_scope::bind_route(route).await?;
                dispatch_unmask(&route, &binding, args).await
            },
            |result| {
                // Wire shape: `{ plaintext: <string> }`. The SDK reads
                // `result.plaintext` directly; for `wraps = bytes` the
                // SDK base64-decodes on its side.
                ResolveValue::Json(serde_json::json!({ "plaintext": result.plaintext }).to_string())
            },
        )));

    promise
}
/// V8-facing dispatch helper for `zeroship.db.bulkUnmaskFields`.
///
/// Mirrors [`dispatch_unmask_field`]: parses the args eagerly and takes
/// the error before any await, then spawns the bulk
/// dispatcher and resolves with `{ results: { <rowPk>: { <col>: <pt> } } }`
/// on success or rejects with the typed `OpError` on failure (most
/// commonly `bulk_unmask_partial_unauthorized`).
pub(crate) fn dispatch_bulk_unmask_field<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    binding: &DbBinding,
    args_v: Value,
) -> v8::Local<'s, v8::Promise> {
    let state = crate::v8_bridge::runtime_state(scope);
    let (resolver, request_id, promise) = crate::v8_bridge::setup_js_promise(scope, &state);

    let parsed = parse_bulk_args(&args_v);
    let binding = binding.clone();
    // Captured adapter-side, as in [`dispatch_unmask_field`].
    let route = crate::tx_scope::capture_route(scope, &binding);

    state
        .borrow_mut()
        .spawned_ops
        .push(Box::pin(crate::v8_classes::dispatch::settle(
            resolver,
            request_id,
            async move {
                // Ahead of the await, for the reason spelled out in
                // [`dispatch_unmask_field`]: the parse error must win over a
                // backend-open failure. Note this orders the PARSE only; the
                // descriptor validation inside `dispatch_bulk_unmask` still runs
                // after the route is bound, which is a separate deliberate
                // trade documented in `crud/unmask.rs`.
                let args = parsed?;
                let route = crate::tx_scope::bind_route(route).await?;
                dispatch_bulk_unmask(&route, &binding, args).await
            },
            |result| {
                // Wire shape: `{ results: { <rowPk>: { <col>: <plaintext> } } }`.
                // `BTreeMap` serialises as a JSON object with sorted
                // keys — deterministic for golden-snapshot tests. The reshaping is
                // JS-wire lowering, so it belongs on this side of the boundary.
                let mut obj = serde_json::Map::with_capacity(result.results.len());
                for (row_pk, cols) in result.results {
                    let mut col_obj = serde_json::Map::with_capacity(cols.len());
                    for (c, pt) in cols {
                        col_obj.insert(c, Value::String(pt));
                    }
                    obj.insert(row_pk, Value::Object(col_obj));
                }
                ResolveValue::Json(serde_json::json!({ "results": Value::Object(obj) }).to_string())
            },
        )));

    promise
}
/// V8-facing dispatch helper for `zeroship.db.setMaskPolicy`. Returns
/// the unresolved Promise; the dispatcher body runs as a spawned op
/// and resolves with `{}` on success or rejects with the typed
/// `OpError`.
///
/// Called from `v8_classes::db::Db::set_mask_policy` (the `#[v8_method]`
/// wrapping this entry point).
pub(crate) fn dispatch_set_mask_policy_field<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    app_id: &str,
    policy_v: Value,
) -> v8::Local<'s, v8::Promise> {
    let state = crate::v8_bridge::runtime_state(scope);
    let (resolver, request_id, promise) = crate::v8_bridge::setup_js_promise(scope, &state);
    let app = app_id.to_string();

    // The engine half already existed as a separate `async fn`; what was here
    // was a hand-rolled copy of `settle`'s two arms. Its error arm and
    // `settle`'s are the same `reject_op` call.
    //
    // The backend is resolved HERE, not by the engine installer. This dispatch
    // captures no route - a policy install routes no SQL of its own - so the
    // handle comes from `tx_scope::ensure_backend()`, adapter to adapter, and is
    // passed down. That call keeps the cold-init arm, and this is the site that
    // needs it: `installSchema` fires `setMaskPolicy` at boot, typically before
    // any other op has opened the backend, so a plain read of the context would
    // return `not_configured` on every fresh isolate.
    state
        .borrow_mut()
        .spawned_ops
        .push(Box::pin(crate::v8_classes::dispatch::settle(
            resolver,
            request_id,
            async move {
                let backend = crate::tx_scope::ensure_backend().await?;
                dispatch_set_mask_policy(&backend, &app, policy_v).await
            },
            |()| ResolveValue::Json("{}".to_string()),
        )));

    promise
}
