//! Faithful integration test for the DEV-TIER auth provider's server-side
//! identity injection — the `pnpm dev` peer of `env.db`→SQLite / `env.kv`→redb.
//!
//! This drives the EXACT seam the dev serve path (`crates/runtime/src/core/
//! serve.rs::handle_request`) composes:
//! `dev_auth::resolve_dev_user_json(headers, &settings)`
//! → `Runtime::call_fetch_handler_with_user(..., user_json)`. `handle_request`
//! itself is `resolve_dev_user_json` followed by `call_fetch_handler_with_user`
//! with no other logic in between, so exercising those two public functions in
//! sequence reproduces the real dev request flow - no shim. The `settings` are
//! the ones `run_single_worker` resolves once at startup and hands down.
//!
//! The `__zeroship_dev_session` cookie is signed with `dev_auth::sign_dev_session`,
//! which is byte-compatible with `signDevSession` in the Vite development auth
//! provider (the same `base64url(json).hex-hmac` envelope), so its browser cookie
//! verifies here. We assert BOTH server-side identity surfaces resolve the dev
//! user:
//!   1. `env.auth.getUser()`  — the kernel `AuthPlugin` per-request state.
//!   2. `currentUser()`       — the native RPC context accessor.
//!
//! Both are fed by the SAME `user_json`, exactly as the gateway header is in
//! production.

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

/// The dev-auth settings `serve.rs::run_single_worker` resolves once at
/// startup and hands to every `resolve_dev_user_json` call. Both conditions
/// are met here, so a cookie that verifies against `DEV_SECRET` resolves.
///
/// These used to be `ZEROSHIP_DEV=1` + `ZEROSHIP_DEV_AUTH_SECRET` in the
/// process environment, mutated with `std::env::set_var` and restored by an
/// RAII guard on drop. They are inputs now: nothing in this file touches an
/// environment, so nothing here races libc `getenv` and no test can leave a
/// value behind for the next one.
fn dev_settings() -> dev_auth::DevAuthSettings {
    dev_auth::DevAuthSettings {
        dev_mode: true,
        secret: Some(DEV_SECRET.to_string()),
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
    dispatch_with(runtime, method, url, cookie, body, &dev_settings())
}

/// The same path with the dev-auth settings stated explicitly, so a test can
/// vary ONE of the two conditions and see what the whole dispatch does.
fn dispatch_with(
    runtime: &Runtime,
    method: &str,
    url: &str,
    cookie: Option<&str>,
    body: &str,
    settings: &dev_auth::DevAuthSettings,
) -> (u16, String) {
    let headers: Vec<(String, String)> = match cookie {
        Some(c) => vec![("Cookie".to_string(), c.to_string())],
        None => vec![],
    };
    let user_json = dev_auth::resolve_dev_user_json(&headers, settings);
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

/// Dev-only by construction, through the WHOLE dispatch rather than only
/// through `resolve_dev_user_json`: with dev mode off, or with no secret
/// provisioned, the very cookie that authenticates above leaves
/// `env.auth.getUser()` null.
///
/// The control is `env_auth_get_user_resolves_dev_user_from_cookie` above,
/// which runs the same cookie and the same module through the same path with
/// both conditions met - so `isNull` here is attributable to the setting and
/// not to a cookie that never worked.
#[test]
fn dev_mode_off_or_no_secret_is_anonymous() {
    let runtime = build_runtime(
        r#"
        export default {
            fetch(request, env, ctx) {
                return Response.json({ isNull: env.auth.getUser() === null });
            }
        };
    "#,
    );
    let cookie = dev_cookie();
    let cases = [
        (
            "dev mode off",
            dev_auth::DevAuthSettings {
                dev_mode: false,
                secret: Some(DEV_SECRET.to_string()),
            },
        ),
        (
            "no secret provisioned",
            dev_auth::DevAuthSettings { dev_mode: true, secret: None },
        ),
        (
            "empty secret",
            dev_auth::DevAuthSettings {
                dev_mode: true,
                secret: Some(String::new()),
            },
        ),
    ];
    for (label, settings) in cases {
        let (status, body) = dispatch_with(
            &runtime,
            "GET",
            "http://localhost/",
            Some(&cookie),
            "",
            &settings,
        );
        assert_eq!(status, 200, "{label}: body: {body}");
        assert!(
            body.contains(r#""isNull":true"#),
            "{label} must not authenticate: {body}"
        );
    }
}
