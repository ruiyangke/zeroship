//! Decode native arguments and adapt the ORM result to a V8 promise.

use std::future::Future;

use zeroship_data_orm::value::Value;
use zeroship_runtime::state::{OpResult, ResolveValue, SharedState};

use zeroship_data_orm::binding::DbBinding;
use zeroship_data_orm::error::DbError;

use crate::op_error::ToOpError;
use crate::v8_bridge::{runtime_state, setup_js_promise};
use zeroship_data_orm::orm::{Operation, Output, PreparedOperation};
use zeroship_data_orm::protection::mask_policy::install_mask_policy;
use zeroship_data_orm::protection::unmask::{
    dispatch_bulk_unmask, dispatch_unmask, parse_args, parse_bulk_args,
};

/// Read the authenticated actor ID from the current V8 request.
///
/// The ORM receives the captured ID for descriptor-declared actor assignments;
/// it does not depend on V8 request state. Anonymous or malformed request
/// identity produces no actor assignment.
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

#[derive(Clone, Copy)]
pub(super) enum OutputMode {
    Many,
    One,
    Exists,
}

pub(super) fn dispatch_operation<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    binding: DbBinding,
    collection: &str,
    operation: Operation,
    mode: OutputMode,
) -> v8::Local<'s, v8::Promise> {
    crate::v8_bridge::ensure_read_set_capture(scope);
    let state = runtime_state(scope);
    let route = crate::tx_scope::capture_route(scope, &binding);
    let prepared = route.and_then(|route| {
        PreparedOperation::new(
            binding,
            collection,
            route,
            current_actor_id(&state),
            operation,
        )
    });
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
            Output::Count(count) if matches!(mode, OutputMode::Exists) => {
                ResolveValue::Bool(count != 0)
            }
            Output::Count(count) => ResolveValue::F64(count as f64),
            Output::Rows { rows, has_masked } if matches!(mode, OutputMode::One) => {
                crate::v8_bridge::first_row_or_null_masked(rows, has_masked)
            }
            Output::Rows { rows, has_masked } => {
                crate::v8_bridge::rows_as_array_masked(rows, has_masked)
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
        OutputMode::Many,
    )
}

pub(crate) fn dispatch_find_one<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    binding: DbBinding,
    collection: &str,
    filter: Value,
    options: Value,
) -> v8::Local<'s, v8::Promise> {
    dispatch_operation(
        scope,
        binding,
        collection,
        Operation::Find { filter, options },
        OutputMode::One,
    )
}

pub(crate) fn dispatch_exists<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    binding: DbBinding,
    collection: &str,
    filter: Value,
    options: Value,
) -> v8::Local<'s, v8::Promise> {
    dispatch_operation(
        scope,
        binding,
        collection,
        Operation::Count { filter, options },
        OutputMode::Exists,
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
        OutputMode::One,
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
        OutputMode::Many,
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
        OutputMode::One,
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
        OutputMode::Many,
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
        OutputMode::One,
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
        OutputMode::Many,
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
        OutputMode::One,
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
        OutputMode::Many,
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
        OutputMode::One,
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
        OutputMode::Many,
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
        OutputMode::Many,
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
        OutputMode::Many,
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
        OutputMode::Many,
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
        OutputMode::One,
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
        OutputMode::Many,
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
        OutputMode::Many,
    )
}
// ---------------------------------------------------------------------------

/// Settle ORM work into the originating V8 promise.
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
// Protection dispatch: unmask and deployment policy installation
// ---------------------------------------------------------------------------
//
// Collection exposes audited unmask operations. DbPlatform carries policy
// installation through the runtime's private capability handle. These helpers
// capture arguments and return V8 promises; authorization stays in the ORM.
//
// Their engine halves stay in `protection::unmask` and `protection::mask_policy`, which is
// where the DB-3 fence lives - `sanitize_app_actor` runs inside `parse_args` /
// `parse_bulk_args`, below this layer, precisely so no dispatch site can
// forget it.

/// V8-facing dispatch helper. Returns the unresolved Promise; the
/// `dispatch_unmask` body runs as a spawned op and resolves with
/// `{ plaintext }` on success or rejects with the typed `OpError`.
///
/// Called from `Collection::unmask_field`, which binds the collection identity.
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
    // live, and handed to the engine. `protection::unmask` used to open a backend
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
                let route = route?;
                let route = crate::tx_scope::bind_route(route).await?;
                dispatch_unmask(&route, &binding, args).await
            },
            |result| {
                // Wire shape: `{ plaintext: <string> }`. The SDK reads
                // `result.plaintext` directly; for `type = bytes` the
                // SDK base64-decodes on its side.
                crate::v8_values::resolve(
                    zeroship_data_orm::value!({ "plaintext": result.plaintext }),
                    false,
                )
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
                let route = route?;
                let route = crate::tx_scope::bind_route(route).await?;
                dispatch_bulk_unmask(&route, &binding, args).await
            },
            |result| {
                // Wire shape: `{ results: { <rowPk>: { <col>: <plaintext> } } }`.
                // `BTreeMap` serialises as a JSON object with sorted
                // keys — deterministic for golden-snapshot tests. The reshaping is
                // JS-wire lowering, so it belongs on this side of the boundary.
                let mut obj = zeroship_data_orm::value::Map::new();
                for (row_pk, cols) in result.results {
                    let mut col_obj = zeroship_data_orm::value::Map::new();
                    for (c, pt) in cols {
                        col_obj.insert(c, pt);
                    }
                    obj.insert(row_pk, Value::Object(col_obj));
                }
                crate::v8_values::resolve(
                    zeroship_data_orm::value!({ "results": Value::Object(obj) }),
                    false,
                )
            },
        )));

    promise
}
/// Install the startup policy captured from the framework-private handle.
/// No database connection or file I/O is needed.
pub(crate) fn dispatch_set_mask_policy_field<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    binding: &DbBinding,
    policy_v: Value,
) -> v8::Local<'s, v8::Promise> {
    let state = crate::v8_bridge::runtime_state(scope);
    let (resolver, request_id, promise) = crate::v8_bridge::setup_js_promise(scope, &state);
    let result = install_mask_policy(binding, policy_v);
    state.borrow_mut().spawned_ops.push(Box::pin(settle(
        resolver,
        request_id,
        async move { result },
        |()| crate::v8_values::resolve(Value::Object(Default::default()), false),
    )));
    promise
}
