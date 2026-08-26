//! V8 bridge for replication diagnostics.
//!
//! Consumer provisioning is intentionally absent. Native Subscription.ready()
//! owns the only provision-and-spawn path through `cdc_lifecycle`, so an app
//! cannot create a logical slot with no task responsible for it. This module
//! retains only app-scoped watchdog and abandoned-slot maintenance operations.

use zeroship_runtime::state::{OpResult, ResolveValue};

use crate::exec::ensure_pool;
use crate::v8_bridge::{runtime_state, setup_js_promise};

/// `db.replication.watchdog()` dispatch.
pub fn replication_watchdog_dispatch<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    app_id: String,
) -> v8::Local<'s, v8::Promise> {
    let state = runtime_state(scope);
    let (resolver, request_id, promise) = setup_js_promise(scope, &state);

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        let pool = match ensure_pool().await {
            Ok(pool) => pool,
            Err(error) => {
                return OpResult::JsValue {
                    resolver,
                    value: ResolveValue::RejectError(error.to_op_error()),
                    request_id,
                };
            }
        };
        match crate::replication::watchdog_query(&pool, &app_id).await {
            Ok(rows) => OpResult::JsValue {
                resolver,
                value: ResolveValue::String(crate::replication::watchdog_to_json(&rows)),
                request_id,
            },
            Err(error) => OpResult::JsValue {
                resolver,
                value: ResolveValue::RejectError(error.to_op_error()),
                request_id,
            },
        }
    }));
    promise
}

/// `db.replication.dropAbandoned(opts?)` dispatch.
pub fn replication_drop_abandoned_dispatch<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    app_id: String,
    inactive_seconds: i64,
) -> v8::Local<'s, v8::Promise> {
    let state = runtime_state(scope);
    let (resolver, request_id, promise) = setup_js_promise(scope, &state);

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        let pool = match ensure_pool().await {
            Ok(pool) => pool,
            Err(error) => {
                return OpResult::JsValue {
                    resolver,
                    value: ResolveValue::RejectError(error.to_op_error()),
                    request_id,
                };
            }
        };
        match crate::replication::drop_abandoned_slots(&pool, &app_id, inactive_seconds).await {
            Ok(names) => OpResult::JsValue {
                resolver,
                value: ResolveValue::String(
                    serde_json::to_string(&names).unwrap_or_else(|_| "[]".to_string()),
                ),
                request_id,
            },
            Err(error) => OpResult::JsValue {
                resolver,
                value: ResolveValue::RejectError(error.to_op_error()),
                request_id,
            },
        }
    }));
    promise
}
