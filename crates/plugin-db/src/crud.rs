//! CRUD dispatch helpers — one entry point per Collection method.
//!
//! Each `dispatch_*` here is the bridge between a v8_class method on
//! `Collection` (`v8_classes::collection`) and the async exec layer in
//! [`crate::exec`]. The shape is consistent across every helper:
//!
//! 1. Grab the runtime state slot.
//! 2. Optionally record into the active read-set ([`crate::read_set`])
//!    for P8b subscription narrowing.
//! 3. [`setup_js_promise`] — allocate the promise + resolver.
//! 4. Build the SQL via [`crate::query::build_*`]; on builder error,
//!    reject the promise from the spawned op (we need a future scope
//!    so the rejection rides the same drain pump as success).
//! 5. Spawn an async op that calls into [`crate::exec`], maps the
//!    result, and resolves the promise with the appropriate
//!    `ResolveValue` shape.
//!
//! No new public API: each helper is `pub(crate)` and called from
//! `v8_classes::collection` (via the `callbacks::dispatch_*` re-export
//! that lives in [`crate::callbacks`]).
//!
//! The capability gate (`refuse_if_query_capability`) is enforced by
//! the v8_class methods *before* reaching the dispatch helper — write
//! ops trust their callers.

use serde_json::Value;
use zeroship_runtime::state::{OpError, OpResult, ResolveValue};

use crate::exec::{exec_count, exec_mutation_with_emit, exec_query};
use crate::query;
use crate::v8_bridge::{runtime_state, setup_js_promise};

// ---------------------------------------------------------------------------
// findOne / find — read paths
// ---------------------------------------------------------------------------

/// Shared dispatch for `findOne`, called by `Collection::find_one`
/// (the `#[v8_method]`). Filter arrives already decoded into
/// `serde_json::Value` via `v8_value_to_serde_json` — no JSON
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
