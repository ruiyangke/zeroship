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
use crate::state::{OpError, OpResult, ResolveValue, SharedState};

use zeroship_core::power_token::{
    error_code, PowerTokenRequest, POWER_TOKEN_APP_ID_HEADER,
};

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
        r.add("getAccessToken", get_access_token_callback);
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

/// Store the RAW, still-signed `ZeroShip-User` header for a specific request.
/// Called by the worker dispatch handler right after HMAC-verifying the header
/// (it verifies to decode the JSON, but keeps the original signed string so the
/// power-token op can echo it to control for stateless re-verification, R4).
///
/// Held Rust-side only — never surfaced to app JS. App code can read the
/// decoded identity via `getUser()`, but it can neither read the gateway
/// signature nor present a different one.
pub fn set_request_user_header(state: &SharedState, request_id: u64, header: Option<String>) {
    let mut s = state.borrow_mut();
    match header {
        Some(h) => {
            s.per_request_user_header.insert(request_id, h);
        }
        None => {
            s.per_request_user_header.remove(&request_id);
        }
    }
}

/// Drop the user entry for a finished request. Mirrors `drain_request_logs`
/// — every terminal path (success, error, cancellation) must call this so
/// long-running workers don't accumulate per-request state forever.
pub fn clear_request_user(state: &SharedState, request_id: u64) {
    let mut s = state.borrow_mut();
    s.per_request_user.remove(&request_id);
    s.per_request_user_header.remove(&request_id);
}

/// Look up the currently-executing request's RAW signed `ZeroShip-User`
/// header. Mirrors [`current_user`] but returns the verbatim signed string.
fn current_user_header(state: &SharedState) -> Option<String> {
    let s = state.borrow();
    let rid = s.executing_request_id?;
    s.per_request_user_header.get(&rid).cloned()
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

// ---------------------------------------------------------------------------
// env.auth.getAccessToken — the runtime-mediated power-token mint (R4)
// ---------------------------------------------------------------------------
//
// THE IDENTITY-BINDING CRUX. App SERVER code calls
// `env.auth.getAccessToken({ audience, scopes })`. This is a RUNTIME-MEDIATED
// async op (the way `fetch` is Rust-backed), NOT a plain JS `fetch()`:
//
//   - The Rust runtime — NOT app JS — attaches (a) the `control_key` from
//     `RuntimeState.power_control_key` (worker config; NEVER JS-visible), and
//     (b) the CURRENT request's RAW gateway-signed `ZeroShip-User` header that
//     the worker received and holds Rust-side, and (c) the app id (from the
//     Rust-stamped `APP_ID` env var, not from JS).
//   - App JS supplies ONLY `{ audience, scopes }`.
//
// So app code cannot exfiltrate `control_key` (it never enters JS), and it
// cannot assert an identity (it can neither read nor forge the signed header).
// Control re-verifies the header signature and caps scopes by the grant
// ceiling; an ordinary creator app is rejected for the control audience.

/// Parse the single `{ audience, scopes }` argument synchronously on the V8
/// thread. Returns a typed [`PowerTokenRequest`] or a JS-facing error message.
fn parse_power_token_request(
    scope: &mut v8::PinScope,
    args: &v8::FunctionCallbackArguments,
) -> Result<PowerTokenRequest, String> {
    let arg = args.get(0);
    if !arg.is_object() {
        return Err("getAccessToken expects an options object { audience, scopes }".to_string());
    }
    // Round-trip through JSON.stringify so we reuse serde for shape/validation
    // instead of hand-walking V8 properties.
    let json = match v8::json::stringify(scope, arg) {
        Some(s) => s.to_rust_string_lossy(scope),
        None => return Err("getAccessToken options are not serializable".to_string()),
    };
    let req: PowerTokenRequest = serde_json::from_str(&json)
        .map_err(|e| format!("getAccessToken options invalid: {e}"))?;
    if req.audience.trim().is_empty() {
        return Err("getAccessToken requires a non-empty `audience`".to_string());
    }
    Ok(req)
}

/// `env.auth.getAccessToken(opts)` — returns a Promise that resolves to
/// `{ accessToken, expiresAt, scopes }` or rejects with a coded error.
pub fn get_access_token_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();

    let resolver = v8::PromiseResolver::new(scope).unwrap();
    let promise = resolver.get_promise(scope);
    rv.set(promise.into());

    // --- Synchronous arg parse (must happen on the V8 thread) ---
    let req = match parse_power_token_request(scope, &args) {
        Ok(r) => r,
        Err(msg) => {
            let m = v8::String::new(scope, &msg).unwrap();
            let exc = v8::Exception::type_error(scope, m);
            resolver.reject(scope, exc);
            return;
        }
    };

    // --- Snapshot the Rust-held secrets + identity (NOT from JS) ---
    let (control_url, control_key, app_id, signed_header) = {
        let s = state.borrow();
        (
            s.power_control_url.clone(),
            s.power_control_key.clone(),
            s.env_vars.get("APP_ID").cloned().unwrap_or_default(),
            // the RAW signed header for the executing request
            s.executing_request_id
                .and_then(|rid| s.per_request_user_header.get(&rid).cloned()),
        )
    };

    let resolver_g = v8::Global::new(scope, resolver);
    let request_id = state.borrow().executing_request_id;

    // Fail closed if the runtime was not configured with control reach
    // (e.g. single-tenant `zeroship serve`).
    if control_url.is_empty() {
        reject_now(
            &state,
            resolver_g,
            request_id,
            OpError::coded(
                "not_configured",
                "getAccessToken is unavailable: this runtime has no control-plane mint configured",
                None::<String>,
            ),
        );
        return;
    }

    // Fail closed if there is no authenticated request identity to bind to.
    // App JS cannot supply one; absence means the request is anonymous.
    let Some(signed_header) = signed_header else {
        reject_now(
            &state,
            resolver_g,
            request_id,
            OpError::coded(
                error_code::UNAUTHENTICATED_IDENTITY,
                "getAccessToken requires an authenticated request (no signed user identity present)",
                None::<String>,
            ),
        );
        return;
    };

    let mint_url = format!("{}/internal/power-token", control_url.trim_end_matches('/'));
    let body = serde_json::to_vec(&req).unwrap_or_default();

    let fut: std::pin::Pin<Box<dyn std::future::Future<Output = OpResult>>> =
        Box::pin(async move {
            let value = match mint_power_token(
                &mint_url,
                &control_key,
                &signed_header,
                &app_id,
                body,
            )
            .await
            {
                Ok(json) => ResolveValue::String(json),
                Err(err) => ResolveValue::RejectError(err),
            };
            OpResult::JsValue {
                resolver: resolver_g,
                value,
                request_id,
            }
        });

    {
        let mut s = state.borrow_mut();
        s.spawned_ops.push(fut);
    }
    let notify = state.borrow().pump_notify_tx.clone();
    if let Some(mut tx) = notify {
        let _ = tx.try_send(());
    }
}

/// Reject the resolver via the async pump (so the rejection runs in a clean V8
/// turn, consistent with the success path), without doing a network round-trip.
fn reject_now(
    state: &SharedState,
    resolver: v8::Global<v8::PromiseResolver>,
    request_id: Option<u64>,
    err: OpError,
) {
    let fut: std::pin::Pin<Box<dyn std::future::Future<Output = OpResult>>> =
        Box::pin(async move {
            OpResult::JsValue {
                resolver,
                value: ResolveValue::RejectError(err),
                request_id,
            }
        });
    {
        let mut s = state.borrow_mut();
        s.spawned_ops.push(fut);
    }
    let notify = state.borrow().pump_notify_tx.clone();
    if let Some(mut tx) = notify {
        let _ = tx.try_send(());
    }
}

/// Internal cyper client for the worker→control mint channel. Deliberately
/// does NOT install the SSRF resolver: this is the same Rust-side internal
/// channel the worker already uses to reach control (`control_url`), which is
/// an intra-cluster private host the JS SSRF guard would otherwise block. App
/// JS never drives this client — only the runtime, with the Rust-held
/// `control_key`.
fn internal_control_client() -> cyper::Client {
    thread_local! {
        static CLIENT: cyper::Client = cyper::Client::new();
    }
    CLIENT.with(|c| c.clone())
}

/// Perform the worker→control mint POST. Attaches the Rust-held `control_key`,
/// the echoed signed `ZeroShip-User` header, and the Rust-stamped app id.
/// Returns the response JSON on 200, or a coded [`OpError`] mapping the
/// control-side error code so the SDK can branch on `e.code`.
async fn mint_power_token(
    mint_url: &str,
    control_key: &str,
    signed_header: &str,
    app_id: &str,
    body: Vec<u8>,
) -> Result<String, OpError> {
    let client = internal_control_client();
    let mut builder = client
        .post(mint_url)
        .map_err(|e| OpError::error(format!("power-token: invalid control URL: {e}")))?
        .header("content-type", "application/json")
        .map_err(|e| OpError::error(format!("power-token: header error: {e}")))?
        // The control_key is attached HERE, Rust-side — never in JS.
        .header("authorization", &format!("Bearer {control_key}"))
        .map_err(|e| OpError::error(format!("power-token: header error: {e}")))?
        // The echoed gateway-signed identity — control re-verifies its HMAC.
        .header("zeroship-user", signed_header)
        .map_err(|e| OpError::error(format!("power-token: header error: {e}")))?
        .header(POWER_TOKEN_APP_ID_HEADER, app_id)
        .map_err(|e| OpError::error(format!("power-token: header error: {e}")))?;
    builder = builder.body(body);

    let response = builder
        .send()
        .await
        .map_err(|e| OpError::error(format!("power-token: control request failed: {e}")))?;

    let status = response.status().as_u16();
    let bytes = response
        .bytes()
        .await
        .map_err(|e| OpError::error(format!("power-token: read body: {e}")))?;
    let text = String::from_utf8_lossy(&bytes).into_owned();

    if status == 200 {
        return Ok(text);
    }

    // Map the control-side `{ "error": <code> }` to a coded OpError so the SDK
    // surfaces `e.code` (scope_required / step_up_required / forbidden_audience
    // / consent_required / unauthenticated*). Fail closed on any non-200.
    let code = serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_owned))
        .unwrap_or_else(|| format!("power_token_http_{status}"));
    Err(OpError::coded(
        code,
        format!("power-token mint refused (HTTP {status})"),
        None::<String>,
    ))
}
