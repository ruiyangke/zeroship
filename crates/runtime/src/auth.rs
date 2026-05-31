//! Auth primitives — `zeroship.auth.getUser()` and `zeroship.auth.requireUser()`.
//!
//! The gateway extracts the authenticated user from the `__zs_session` cookie
//! and forwards it as the `ZeroShip-User` header (base64-encoded JSON, HMAC-
//! signed with the shared worker key). The worker decodes + verifies the
//! header before dispatching to V8 and stores the user JSON in the runtime's
//! per-request state, keyed by `request_id`.
//!
//! ## Why per-request, not thread-local
//!
//! An earlier revision used a `thread_local!` here. That is structurally
//! wrong for an async runtime: `set_auth_user(userA)` → `.await` lets the
//! compio scheduler run handler B → handler B calls `set_auth_user(userB)`
//! → pump fires A's pending promise → A's `.then` callback reads the
//! thread-local → sees **userB**. Any `onRequest` handler that called
//! `getUser()` after an `await` saw whichever user last touched the thread.
//!
//! The callbacks below look up the user via the currently-executing
//! request id stored in `RuntimeState`. The pump sets that id every time
//! it enters V8 to drive a specific request (see `runtime.rs`
//! `handle_op_result_pump` / `handle_timer_pump`), so async continuations
//! resolve to the correct identity.

use crate::plugin::{NativePlugin, NativeRegistrar};
use crate::state::SharedState;

/// The auth plugin — registers `env.auth.getUser()` / `env.auth.requireUser()`.
///
/// `AuthPlugin` is **stateless**: it carries no construction args. Both
/// callbacks read the current request's user from `RuntimeState` via the
/// isolate scope slot (see [`current_user`]), so a single shared instance
/// is correct for every app on every worker thread. It is registered at
/// both plugin-construction sites — the worker `create_plugins()`
/// (`crates/worker/src/cache.rs`, the path every production end-user app
/// runs on) and the CLI `zeroship serve` plugin vector
/// (`crates/cli/src/main.rs`) — so `env.auth.getUser()` resolves on both.
#[derive(Debug, Default, Clone, Copy)]
pub struct AuthPlugin;

impl NativePlugin for AuthPlugin {
    fn namespace(&self) -> &str {
        "auth"
    }

    fn name(&self) -> &str {
        "auth"
    }

    fn register(&self, r: &mut NativeRegistrar) {
        r.add("getUser", get_user_callback);
        r.add("requireUser", require_user_callback);
    }
}

/// Store the user JSON for a specific request. Called by the worker
/// dispatch handler after HMAC-verifying the `ZeroShip-User` header,
/// right before V8 enters for the initial dispatch.
pub fn set_request_user(state: &SharedState, request_id: u64, user_json: Option<String>) {
    let mut s = state.borrow_mut();
    match user_json {
        Some(j) => {
            s.per_request_user.insert(request_id, j);
        }
        None => {
            s.per_request_user.remove(&request_id);
        }
    }
}

/// Drop the user entry for a finished request. Mirrors `drain_request_logs`
/// — every terminal path (success, error, cancellation) must call this so
/// long-running workers don't accumulate per-request state forever.
pub fn clear_request_user(state: &SharedState, request_id: u64) {
    state.borrow_mut().per_request_user.remove(&request_id);
}

/// Look up the currently-executing request's user JSON.
///
/// `executing_request_id` is set by the runtime immediately before every
/// V8 turn that belongs to a specific request (dispatch, op resolve, op
/// reject, timer fire). If no request is currently attributed — e.g. a
/// module-init callback, or a stream pump with no owning request —
/// returns `None`.
fn current_user(state: &SharedState) -> Option<String> {
    let s = state.borrow();
    let rid = s.executing_request_id?;
    s.per_request_user.get(&rid).cloned()
}

// ---------------------------------------------------------------------------
// V8 callbacks
// ---------------------------------------------------------------------------

/// `zeroship.auth.getUser()` — returns the authenticated user object or null.
pub fn get_user_callback(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();

    match current_user(&state) {
        Some(json) => {
            let Some(json_str) = v8::String::new(scope, &json) else {
                rv.set(v8::null(scope).into());
                return;
            };
            match v8::json::parse(scope, json_str) {
                Some(val) => rv.set(val),
                None => rv.set(v8::null(scope).into()),
            }
        }
        None => {
            rv.set(v8::null(scope).into());
        }
    }
}

/// `zeroship.auth.requireUser()` — returns the authenticated user or throws.
pub fn require_user_callback(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();

    let throw_auth_required = |scope: &mut v8::PinScope| {
        let msg = v8::String::new(scope, "Authentication required").unwrap();
        let exc = v8::Exception::error(scope, msg);
        scope.throw_exception(exc);
    };

    match current_user(&state) {
        Some(json) => {
            let Some(json_str) = v8::String::new(scope, &json) else {
                throw_auth_required(scope);
                return;
            };
            match v8::json::parse(scope, json_str) {
                Some(val) => rv.set(val),
                None => throw_auth_required(scope),
            }
        }
        None => throw_auth_required(scope),
    }
}
