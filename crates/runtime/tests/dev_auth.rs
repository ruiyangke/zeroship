//! Faithful integration test for the DEV-TIER auth provider's server-side
//! identity injection — the `pnpm dev` peer of `env.db`→SQLite / `env.kv`→redb.
//!
//! This drives the EXACT seam the dev serve path (`crates/runtime/src/core/
//! serve.rs::handle_request`) composes: `dev_auth::resolve_dev_user_json(headers)`
//! → `Runtime::call_fetch_handler_with_user(..., user_json)`. `handle_request`
//! itself is `resolve_dev_user_json` followed by `call_fetch_handler_with_user`
//! with no other logic in between, so exercising those two public functions in
//! sequence reproduces the real dev request flow — no shim.
//!
//! The `__zeroship_dev_session` cookie is signed with `dev_auth::sign_dev_session`,
//! which is byte-compatible with the JS `signDevSession` in
//! `@zeroship/bootstrap`'s `dev-auth.ts` (same `base64url(json).hex-hmac`
//! envelope) — so a cookie the JS dev-auth provider mints in the browser
//! verifies here. We assert BOTH server-side identity surfaces resolve the dev
//! user:
//!   1. `env.auth.getUser()`  — the kernel `AuthPlugin` per-request state.
//!   2. `currentUser()`       — the RPC ctx (`__zeroshipGetRpcCtx().user`).
//! Both are fed by the SAME `user_json`, exactly as the gateway header is in
//! production.

// `std::env::set_var` is `unsafe` (process-global mutation race). These tests
// run in their own integration binary; the `DevEnvGuard` restores the prior
// env on drop. Same posture as `fetch_native.rs` / `wpt_fetch_redirect.rs`.
#![allow(unsafe_code)]

mod common;

use std::sync::Arc;
use zeroship_runtime::auth::AuthPlugin;
use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::dev_auth;
use zeroship_runtime::plugin::NativePlugin;
use zeroship_runtime::{init_v8, EnvSnapshot, FetchOutcome, RequestCtx, Runtime};

const DEV_SECRET: &str = "test-dev-auth-secret-0123456789ab";

/// The canonical dev-user `ZeroShip-User` wire body: `pws_` id, snake_case
/// `email_verified`, granted `scopes`. Identical shape to what the JS dev-auth
/// provider packs into the cookie payload and to what the gateway emits in prod.
const DEV_USER_JSON: &str = r#"{"id":"pws_devalice0000000000","email":"alice@localhost","name":"Alice Dev","avatar":null,"email_verified":true,"scopes":["openid","profile","email"]}"#;

/// RAII guard that sets `ZEROSHIP_DEV=1` + the dev-auth secret for the duration
/// of a test and restores the prior environment on drop. The runtime reads
/// these via `std::env` in `resolve_dev_user_json`, so they must be live during
/// the dispatch.
struct DevEnvGuard {
    prev_dev: Option<std::ffi::OsString>,
    prev_secret: Option<std::ffi::OsString>,
}

impl DevEnvGuard {
    fn set() -> Self {
        let prev_dev = std::env::var_os("ZEROSHIP_DEV");
        let prev_secret = std::env::var_os("ZEROSHIP_DEV_AUTH_SECRET");
        unsafe {
            std::env::set_var("ZEROSHIP_DEV", "1");
            std::env::set_var("ZEROSHIP_DEV_AUTH_SECRET", DEV_SECRET);
        }
        DevEnvGuard { prev_dev, prev_secret }
    }
}

impl Drop for DevEnvGuard {
    fn drop(&mut self) {
        unsafe {
            match &self.prev_dev {
                Some(v) => std::env::set_var("ZEROSHIP_DEV", v),
                None => std::env::remove_var("ZEROSHIP_DEV"),
            }
            match &self.prev_secret {
                Some(v) => std::env::set_var("ZEROSHIP_DEV_AUTH_SECRET", v),
                None => std::env::remove_var("ZEROSHIP_DEV_AUTH_SECRET"),
            }
        }
    }
}

fn build_runtime(source: &str) -> Runtime {
    init_v8();
    Runtime::builder()
        .modules(common::m(source))
        .plugins(vec![Arc::new(AuthPlugin) as Arc<dyn NativePlugin>])
        .build()
}

/// Reproduce the dev serve path: resolve `user_json` from the request's `Cookie`
/// header via `dev_auth::resolve_dev_user_json`, then dispatch through
/// `call_fetch_handler_with_user` — exactly as `serve.rs::handle_request` does.
fn dispatch_dev(runtime: &Runtime, method: &str, url: &str, cookie: Option<&str>, body: &str) -> (u16, String) {
    let headers: Vec<(String, String)> = match cookie {
        Some(c) => vec![("Cookie".to_string(), c.to_string())],
        None => vec![],
    };
    let user_json = dev_auth::resolve_dev_user_json(&headers);
    let env = EnvSnapshot::empty();
    let ctx = RequestCtx::new(CancelFlag::new());
    let outcome =
        runtime.call_fetch_handler_with_user(method, url, &headers, body, &env, ctx, user_json);
    match outcome {
        FetchOutcome::Response { status, body, .. } => {
            (status, String::from_utf8_lossy(&body).into_owned())
        }
        other => {
            let name = match other {
                FetchOutcome::Stream { .. } => "Stream",
                FetchOutcome::Pending { .. } => "Pending",
                FetchOutcome::WebSocketUpgrade { .. } => "WebSocketUpgrade",
                FetchOutcome::Response { .. } => unreachable!(),
            };
            panic!("expected Response, got {name}");
        }
    }
}

/// Build a valid `__zeroship_dev_session` cookie header for `DEV_USER_JSON`.
fn dev_cookie() -> String {
    let token = dev_auth::sign_dev_session(DEV_SECRET.as_bytes(), DEV_USER_JSON);
    format!("{}={}", dev_auth::DEV_SESSION_COOKIE, token)
}

/// `env.auth.getUser()` resolves the dev user when a valid dev session cookie
/// is present — the server-side identity surface SDK packages read.
#[test]
fn env_auth_get_user_resolves_dev_user_from_cookie() {
    let _guard = DevEnvGuard::set();
    let runtime = build_runtime(
        r#"
        export default {
            fetch(request, env, ctx) {
                const u = env.auth.getUser();
                return Response.json({
                    id: u?.id ?? null,
                    email: u?.email ?? null,
                    name: u?.name ?? null,
                    emailVerified: u?.email_verified ?? null,
                    scopes: u?.scopes ?? null,
                });
            }
        };
    "#,
    );

    let (status, body) = dispatch_dev(&runtime, "GET", "http://localhost/", Some(&dev_cookie()), "");
    assert_eq!(status, 200, "body: {body}");
    let v: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(v["id"], "pws_devalice0000000000", "body: {body}");
    assert_eq!(v["email"], "alice@localhost", "body: {body}");
    assert_eq!(v["name"], "Alice Dev", "body: {body}");
    assert_eq!(v["emailVerified"], true, "body: {body}");
    assert_eq!(v["scopes"], serde_json::json!(["openid", "profile", "email"]), "body: {body}");
}

/// `currentUser()` (the RPC-ctx identity the `@zeroship/server` helpers expose)
/// resolves the same dev user, driven through the REAL `/__zeroship/v1/<id>` RPC path.
#[test]
fn current_user_resolves_dev_user_via_rpc_ctx() {
    let _guard = DevEnvGuard::set();
    // A synthetic RPC entry whose handler reads `currentUser()` from the
    // `zeroship` module — the same accessor `@zeroship/server` re-exports.
    let modules = common::wrap_with_synthetic_entry(
        r#"
        import { currentUser } from "zeroship";
        export function whoami() {
            const u = currentUser();
            return { id: u?.id ?? null, email: u?.email ?? null, scopes: u?.scopes ?? null };
        }
    "#,
        "{ whoami }",
    );
    init_v8();
    let runtime = Runtime::builder()
        .modules(modules)
        .plugins(vec![Arc::new(AuthPlugin) as Arc<dyn NativePlugin>])
        .build();

    let (status, body) = dispatch_dev(
        &runtime,
        "POST",
        "http://localhost/__zeroship/v1/whoami",
        Some(&dev_cookie()),
        r#"{"json":null}"#,
    );
    assert_eq!(status, 200, "body: {body}");
    assert!(body.contains("pws_devalice0000000000"), "currentUser().id should be the dev user: {body}");
    assert!(body.contains("alice@localhost"), "currentUser().email: {body}");
}

/// No cookie → anonymous: `env.auth.getUser()` is `null`, exactly as a
/// not-signed-in dev request (parity with prod's no-`ZeroShip-User` case).
#[test]
fn no_cookie_is_anonymous() {
    let _guard = DevEnvGuard::set();
    let runtime = build_runtime(
        r#"
        export default {
            fetch(request, env, ctx) {
                return Response.json({ isNull: env.auth.getUser() === null });
            }
        };
    "#,
    );
    let (status, body) = dispatch_dev(&runtime, "GET", "http://localhost/", None, "");
    assert_eq!(status, 200, "body: {body}");
    assert!(body.contains(r#""isNull":true"#), "body: {body}");
}

/// A cookie signed with the WRONG secret is rejected → anonymous (forgery
/// guard at the server boundary). Proves the dev cookie is integrity-checked,
/// not merely decoded.
#[test]
fn cookie_signed_with_wrong_secret_is_anonymous() {
    let _guard = DevEnvGuard::set();
    let runtime = build_runtime(
        r#"
        export default {
            fetch(request, env, ctx) {
                return Response.json({ isNull: env.auth.getUser() === null });
            }
        };
    "#,
    );
    let forged = dev_auth::sign_dev_session(b"a-different-secret", DEV_USER_JSON);
    let cookie = format!("{}={}", dev_auth::DEV_SESSION_COOKIE, forged);
    let (status, body) = dispatch_dev(&runtime, "GET", "http://localhost/", Some(&cookie), "");
    assert_eq!(status, 200, "body: {body}");
    assert!(body.contains(r#""isNull":true"#), "forged cookie must not authenticate: {body}");
}
