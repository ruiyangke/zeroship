//! V8 bridge for replication diagnostics.
//!
//! Consumer provisioning is intentionally absent. Native Subscription.ready()
//! owns the only provision-and-spawn path through `cdc_lifecycle`, so an app
//! cannot create a logical slot with no task responsible for it. This module
//! retains only the app-scoped watchdog operation. Abandoned-slot cleanup is
//! operator-owned and runs from the worker process, outside V8.

use crate::op_error::ToOpError;
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
