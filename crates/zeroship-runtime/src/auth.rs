//! Auth primitives — `zeroship.auth.getUser()` and `zeroship.auth.requireUser()`.
//!
//! The gateway extracts the authenticated user from the `__Host-zeroship_app_session` cookie
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
//! User-code continuations resolve through V8's continuation-preserved
//! invocation frame. Explicit runtime bindings remain as host-turn fallbacks.

use crate::plugin::{NativePlugin, NativeRegistrar};
use crate::rpc::error::ZsErrorCode;
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

/// Look up the currently-executing turn's user JSON.
///
/// User-code continuations carry an invocation context in V8's CPED slot, so
/// that value wins even when a different request caused the isolate-wide
/// microtask checkpoint. An anonymous frame is authoritative and does not
/// fall through to another request's user.
///
/// WebSocket turns (`onmessage` / `onclose`) are not attributed to a
/// request id — they belong to a long-lived connection. For those, the
/// WS-event pump binds the connection's user in `executing_ws_user`
/// (sourced from `ws_user[ws_id]`, captured at upgrade time). We prefer
/// the request-id-keyed user when present, then fall back to the bound
/// WS-connection user.
///
/// If neither resolves — e.g. a module-init callback, or a stream pump
/// with no owning request — returns `None`.
fn current_user(scope: &mut v8::PinScope, state: &SharedState) -> Option<String> {
    if let Some(context) = crate::core::invocation::current_context(scope) {
        return context.user_json;
    }

    let s = state.borrow();
    if let Some(rid) = s.executing_request_id
        && let Some(u) = s.per_request_user.get(&rid)
    {
        return Some(u.clone());
    }
    #[cfg(feature = "runtime_native_websocket")]
    {
        if let Some(u) = s.executing_ws_user.as_ref() {
            return Some(u.clone());
        }
    }
    None
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

    match current_user(scope, &state) {
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

    // The thrown error carries an explicit `status: 401` and the canonical
    // `code: "UNAUTHENTICATED"` as own properties. The kernel dispatch error
    // rail (`core/dispatch.rs::v8_exception_to_*` -> `build_error_body`) reads
    // `.status`/`.code` off the exception and renders the envelope: a 4xx so
    // the 5xx body-sanitizer leaves the message intact (ISS-67). Without
    // `.status`, a bare `Error` defaults to 500 and the anon case gets masked
    // as "internal error", a misleading 500 for what is plainly an
    // authentication failure. Mirrors the `rpc::build_capability_violation`
    // shape.
    //
    // The `code` is taken from [`ZsErrorCode`], not spelled out here, because
    // it is a WIRE token, not a private label. `@zeroship/rpc`'s
    // `parseErrorResponse` lifts the body's `code` VERBATIM (falling back to
    // the status-derived "UNAUTHENTICATED" only when the body carries none),
    // and the client's `onAuthExpired` hook branches on exactly
    // `"UNAUTHENTICATED"`. A private spelling here would therefore not merely
    // fail to match: it would OVERRIDE the otherwise-correct status-derived
    // code, so an app that re-authenticates from `onAuthExpired` would work
    // deployed (where the gateway's `unauthenticated_response` answers before
    // the worker runs) and silently not in dev, where this throw is the 401
    // source. Seam coverage: `require_user_anonymous_code_is_canonical_*`
    // below, and `sdks/auth/tests/auth-expired-seam.test.ts` on the JS side.
    let throw_auth_required = |scope: &mut v8::PinScope| {
        let msg = v8::String::new(scope, "Authentication required").unwrap();
        let exc = v8::Exception::error(scope, msg);
        if let Ok(obj) = v8::Local::<v8::Object>::try_from(exc) {
            let status_key = v8::String::new(scope, "status").unwrap();
            let status_val = v8::Integer::new_from_unsigned(scope, 401);
            obj.set(scope, status_key.into(), status_val.into());

            let code_key = v8::String::new(scope, "code").unwrap();
            let code_val =
                v8::String::new(scope, ZsErrorCode::Unauthenticated.as_wire_str()).unwrap();
            obj.set(scope, code_key.into(), code_val.into());
        }
        scope.throw_exception(exc);
    };

    match current_user(scope, &state) {
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
