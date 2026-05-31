//! Faithful integration test for the `env.auth` native namespace (Slice 1a).
//!
//! Boots a real `Runtime` with `AuthPlugin` registered (the same plugin
//! the worker `create_plugins()` and CLI `zeroship serve` vectors push),
//! sets the per-request user exactly as the worker dispatch path does —
//! `call_fetch_handler_with_user(..., user_json)`, which feeds
//! `crate::auth::set_request_user` — then runs app JS that calls
//! `env.auth.getUser()` / `env.auth.requireUser()` and asserts the result.
//!
//! No unit stub: the env object is built by the real `build_env_object`
//! overlay, the user flows through the real `per_request_user` /
//! `executing_request_id` plumbing, and the callbacks are the real
//! `get_user_callback` / `require_user_callback`.

mod common;
use common::*;

use std::sync::Arc;
use std::time::Duration;
use zeroship_runtime::auth::AuthPlugin;
use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::plugin::NativePlugin;
use zeroship_runtime::{init_v8, EnvSnapshot, FetchOutcome, RequestCtx, Runtime, SettledFetch};

/// The `WorkerUser` projection the gateway forwards as the `ZeroShip-User`
/// header body: `{ id, email, name, avatar?, email_verified, scopes }`. The
/// callbacks `JSON.parse` this verbatim, so the JS sees exactly this shape.
/// `scopes` is the Slice-3 granted-scope array the gateway now always emits.
const USER_JSON: &str = r#"{"id":"usr_abc123","email":"jane@example.com","name":"Jane Doe","avatar":"https://cdn/x.png","email_verified":true,"scopes":["openid","read:billing"]}"#;

fn build_runtime_with_auth(source: &str) -> Runtime {
    init_v8();
    Runtime::builder()
        .modules(m(source))
        .plugins(vec![Arc::new(AuthPlugin) as Arc<dyn NativePlugin>])
        .build()
}

fn dispatch_with_user(runtime: &Runtime, user_json: Option<String>) -> (u16, String) {
    let env = EnvSnapshot::empty();
    let ctx = RequestCtx::new(CancelFlag::new());
    let outcome = runtime.call_fetch_handler_with_user(
        "GET",
        "http://localhost/",
        &[],
        "",
        &env,
        ctx,
        user_json,
        None,
    );
    match outcome {
        FetchOutcome::Response { status, body, .. } => (status, body),
        other => {
            let name = match other {
                FetchOutcome::Stream { .. } => "Stream",
                FetchOutcome::Pending { .. } => "Pending",
                FetchOutcome::WebSocketUpgrade { .. } => "WebSocketUpgrade",
                FetchOutcome::Response { .. } => unreachable!(),
            };
            panic!("expected Response outcome, got {name}");
        }
    }
}

/// `env.auth.getUser()` returns the per-request user projection (the
/// gateway-forwarded `WorkerUser` shape) when the request is authenticated.
#[test]
fn get_user_returns_request_user() {
    let runtime = build_runtime_with_auth(
        r#"
        export default {
            fetch(request, env, ctx) {
                const u = env.auth.getUser();
                return Response.json({
                    isObject: u !== null && typeof u === "object",
                    id: u?.id ?? null,
                    email: u?.email ?? null,
                    name: u?.name ?? null,
                    avatar: u?.avatar ?? null,
                    email_verified: u?.email_verified ?? null,
                    scopes: u?.scopes ?? null,
                });
            }
        };
    "#,
    );

    let (status, body) = dispatch_with_user(&runtime, Some(USER_JSON.to_string()));
    assert_eq!(status, 200, "body: {body}");
    let v: serde_json::Value = serde_json::from_str(&body).expect("body is JSON");
    assert_eq!(v["isObject"], true, "body: {body}");
    assert_eq!(v["id"], "usr_abc123", "body: {body}");
    assert_eq!(v["email"], "jane@example.com", "body: {body}");
    assert_eq!(v["name"], "Jane Doe", "body: {body}");
    assert_eq!(v["avatar"], "https://cdn/x.png", "body: {body}");
    assert_eq!(v["email_verified"], true, "body: {body}");
    // Slice 3: the granted scopes flow through to env.auth.getUser().scopes.
    assert_eq!(
        v["scopes"],
        serde_json::json!(["openid", "read:billing"]),
        "body: {body}"
    );
}

/// Slice 3: `env.auth.getUser().scopes` is the app's granted-scope array —
/// app code can read it and (e.g.) gate a feature on a declared scope. Driven
/// through the REAL worker plumbing (header JSON → per_request_user → V8
/// JSON.parse), so this proves the kernel-contract field reaches user code.
#[test]
fn get_user_exposes_scopes_to_app_code() {
    let runtime = build_runtime_with_auth(
        r#"
        export default {
            fetch(request, env, ctx) {
                const u = env.auth.getUser();
                return Response.json({
                    isArray: Array.isArray(u?.scopes),
                    count: u?.scopes?.length ?? -1,
                    hasBilling: (u?.scopes ?? []).includes("read:billing"),
                    first: u?.scopes?.[0] ?? null,
                });
            }
        };
    "#,
    );

    let (status, body) = dispatch_with_user(&runtime, Some(USER_JSON.to_string()));
    assert_eq!(status, 200, "body: {body}");
    let v: serde_json::Value = serde_json::from_str(&body).expect("body is JSON");
    assert_eq!(v["isArray"], true, "scopes must be an array: {body}");
    assert_eq!(v["count"], 2, "body: {body}");
    assert_eq!(v["hasBilling"], true, "body: {body}");
    assert_eq!(v["first"], "openid", "body: {body}");
}

/// Slice 4 (§6.2/§6.3): after the pairwise projection the gateway ALWAYS
/// emits a per-app `pws_…` id in `ZeroShip-User.id` (never the global
/// `usr_…` UUID), and the worker treats `User.id` as an OPAQUE string.
/// This feeds the exact post-Slice-4 header shape (`id: "pws_…"`) through
/// the REAL plumbing and asserts `env.auth.getUser().id` is that `pws_`
/// string verbatim — the global UUID is absent from anything app code can
/// read.
#[test]
fn get_user_id_is_the_per_app_pairwise_pws() {
    // The gateway projects the global UUID to this opaque per-app id; app
    // code only ever sees the pws_.
    const PAIRWISE_USER_JSON: &str = r#"{"id":"pws_3Qk7xWf2bN0aLpZrT9cD","email":"alias@relay.zeroship.ai","name":"Jane Doe","email_verified":true,"scopes":["openid"]}"#;
    let runtime = build_runtime_with_auth(
        r#"
        export default {
            fetch(request, env, ctx) {
                const u = env.auth.getUser();
                return Response.json({
                    id: u?.id ?? null,
                    idIsString: typeof u?.id === "string",
                    isPws: (u?.id ?? "").startsWith("pws_"),
                });
            }
        };
    "#,
    );

    let (status, body) = dispatch_with_user(&runtime, Some(PAIRWISE_USER_JSON.to_string()));
    assert_eq!(status, 200, "body: {body}");
    let v: serde_json::Value = serde_json::from_str(&body).expect("body is JSON");
    assert_eq!(v["id"], "pws_3Qk7xWf2bN0aLpZrT9cD", "body: {body}");
    assert_eq!(v["idIsString"], true, "User.id must be an opaque string: {body}");
    assert_eq!(v["isPws"], true, "env.auth.getUser().id must be the pws_: {body}");
}

/// `env.auth.getUser()` returns `null` when there is no authenticated user
/// for the request (no `ZeroShip-User` header was forwarded).
#[test]
fn get_user_returns_null_when_anonymous() {
    let runtime = build_runtime_with_auth(
        r#"
        export default {
            fetch(request, env, ctx) {
                const u = env.auth.getUser();
                return Response.json({ isNull: u === null });
            }
        };
    "#,
    );

    let (status, body) = dispatch_with_user(&runtime, None);
    assert_eq!(status, 200, "body: {body}");
    assert!(body.contains(r#""isNull":true"#), "body: {body}");
}

/// `env.auth.requireUser()` returns the user when authenticated.
#[test]
fn require_user_returns_user_when_authenticated() {
    let runtime = build_runtime_with_auth(
        r#"
        export default {
            fetch(request, env, ctx) {
                const u = env.auth.requireUser();
                return Response.json({ id: u.id, email: u.email });
            }
        };
    "#,
    );

    let (status, body) = dispatch_with_user(&runtime, Some(USER_JSON.to_string()));
    assert_eq!(status, 200, "body: {body}");
    assert!(body.contains(r#""id":"usr_abc123""#), "body: {body}");
    assert!(body.contains(r#""email":"jane@example.com""#), "body: {body}");
}

/// `env.auth.requireUser()` throws when there is no authenticated user.
/// The thrown `Error` ("Authentication required") propagates through the
/// handler; the dispatch error rail surfaces it as a non-2xx response.
#[test]
fn require_user_throws_when_anonymous() {
    let runtime = build_runtime_with_auth(
        r#"
        export default {
            fetch(request, env, ctx) {
                // Catch in-handler so we can assert the throw shape directly
                // rather than relying on the 500 sanitizer blanking it.
                try {
                    env.auth.requireUser();
                    return Response.json({ threw: false });
                } catch (e) {
                    return Response.json({ threw: true, message: e?.message ?? String(e) });
                }
            }
        };
    "#,
    );

    let (status, body) = dispatch_with_user(&runtime, None);
    assert_eq!(status, 200, "body: {body}");
    assert!(body.contains(r#""threw":true"#), "body: {body}");
    assert!(
        body.contains("Authentication required"),
        "requireUser should throw 'Authentication required', body: {body}"
    );
}

/// The `auth` namespace is actually exposed on the composite `env` object
/// (both the `fetch` arg and the `zeroship` module export), with the two
/// expected callables. Guards the plugin wiring itself, independent of the
/// per-request user state.
#[test]
fn env_auth_namespace_is_exposed() {
    let runtime = build_runtime_with_auth(
        r#"
        import { env as moduleEnv } from "zeroship";
        export default {
            fetch(request, env, ctx) {
                return Response.json({
                    hasAuthArg: typeof env.auth === "object" && env.auth !== null,
                    hasAuthImport: typeof moduleEnv.auth === "object" && moduleEnv.auth !== null,
                    getUserIsFn: typeof env.auth.getUser === "function",
                    requireUserIsFn: typeof env.auth.requireUser === "function",
                    sameRef: env.auth === moduleEnv.auth,
                });
            }
        };
    "#,
    );

    let (status, body) = dispatch_with_user(&runtime, None);
    assert_eq!(status, 200, "body: {body}");
    assert!(body.contains(r#""hasAuthArg":true"#), "body: {body}");
    assert!(body.contains(r#""hasAuthImport":true"#), "body: {body}");
    assert!(body.contains(r#""getUserIsFn":true"#), "body: {body}");
    assert!(body.contains(r#""requireUserIsFn":true"#), "body: {body}");
    assert!(body.contains(r#""sameRef":true"#), "body: {body}");
}

// ---------------------------------------------------------------------------
// R4 — env.auth.getAccessToken (the runtime-mediated power-token op)
// ---------------------------------------------------------------------------

/// Drive a (possibly async) dispatch carrying both the decoded user JSON and
/// the raw signed `ZeroShip-User` header, returning `(status, body)`. The
/// `getAccessToken` op returns a Promise, so the handler resolves via the pump
/// (the `Pending` arm).
fn dispatch_async_with_user_header(
    runtime: Runtime,
    user_json: Option<String>,
    user_header: Option<String>,
) -> (u16, String) {
    let env = EnvSnapshot::empty();
    let ctx = RequestCtx::new(CancelFlag::new());
    let outcome = runtime.call_fetch_handler_with_user(
        "GET",
        "http://localhost/",
        &[],
        "",
        &env,
        ctx,
        user_json,
        user_header,
    );
    if let FetchOutcome::Response { status, body, .. } = &outcome {
        return (*status, body.clone());
    }
    compio::runtime::Runtime::new().unwrap().block_on(async {
        runtime.start_pump();
        match outcome {
            FetchOutcome::Response { status, body, .. } => (status, body),
            FetchOutcome::Pending { rx, cancel: _ } => {
                let settled = compio::time::timeout(Duration::from_secs(5), rx.recv())
                    .await
                    .expect("getAccessToken pending timed out")
                    .expect("getAccessToken pending delivered DispatchError");
                match settled {
                    SettledFetch::Response { status, body, .. } => (status, body),
                    SettledFetch::Stream { .. } => panic!("unexpected Stream settled variant"),
                    SettledFetch::WebSocketUpgrade { .. } => {
                        panic!("unexpected WebSocketUpgrade settled variant")
                    }
                }
            }
            other => {
                let name = match other {
                    FetchOutcome::Stream { .. } => "Stream",
                    FetchOutcome::WebSocketUpgrade { .. } => "WebSocketUpgrade",
                    _ => "?",
                };
                panic!("expected Response/Pending, got {name}");
            }
        }
    })
}

/// `env.auth.getAccessToken` is registered as an async function. App JS calls
/// it with ONLY `{ audience, scopes }` — it never names a user or a key. This
/// proves the op exists on the live `env.auth` namespace and is callable.
#[test]
fn get_access_token_is_a_registered_async_fn() {
    let runtime = build_runtime_with_auth(
        r#"
        export default {
            async fetch(request, env, ctx) {
                const isFn = typeof env.auth.getAccessToken === "function";
                let returnsPromise = false;
                try {
                    const p = env.auth.getAccessToken({ audience: "zeroship:control", scopes: ["apps:read"] });
                    returnsPromise = p instanceof Promise;
                    await p.catch(() => {});  // swallow the (expected) rejection
                } catch (_e) { /* sync throw — still a function */ }
                return Response.json({ isFn, returnsPromise });
            }
        };
    "#,
    );
    let (status, body) =
        dispatch_async_with_user_header(runtime, Some(USER_JSON.to_string()), Some("hdr".into()));
    assert_eq!(status, 200, "body: {body}");
    let v: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(v["isFn"], true, "getAccessToken must be a function: {body}");
    assert_eq!(v["returnsPromise"], true, "getAccessToken must return a Promise: {body}");
}

/// When the runtime has NO control-plane mint configured (e.g. `zeroship
/// serve`, or any runtime built without `power_token_config`), the op FAILS
/// CLOSED with a coded `not_configured` error. App JS supplied only
/// `{ audience, scopes }`; it never touched a `control_key`. This is the
/// fail-closed half of the boundary at the runtime layer.
#[test]
fn get_access_token_fails_closed_without_control_config() {
    let runtime = build_runtime_with_auth(
        r#"
        export default {
            async fetch(request, env, ctx) {
                try {
                    await env.auth.getAccessToken({ audience: "zeroship:control", scopes: ["apps:read"] });
                    return Response.json({ rejected: false });
                } catch (e) {
                    return Response.json({ rejected: true, code: e?.code ?? null, message: e?.message ?? String(e) });
                }
            }
        };
    "#,
    );
    let (status, body) =
        dispatch_async_with_user_header(runtime, Some(USER_JSON.to_string()), Some("signed-hdr".into()));
    assert_eq!(status, 200, "body: {body}");
    let v: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(v["rejected"], true, "op must reject when no control mint configured: {body}");
    assert_eq!(v["code"], "not_configured", "coded error surfaced to JS: {body}");
}

/// CONTROL_KEY IS NOT JS-REACHABLE. The op is configured with a control_key
/// here, but app JS cannot read it anywhere: not on `env`, not on `env.auth`,
/// not via any getAccessToken return shape (the op fails closed before any
/// network call because there is no authenticated identity, and even on
/// success only `{ accessToken, ... }` is returned — never the control_key).
/// We assert the control_key string literally does not appear in ANY
/// JS-serializable surface the handler can reach.
#[test]
fn control_key_is_never_js_reachable() {
    const SECRET_CONTROL_KEY: &str = "SUPER-SECRET-CONTROL-KEY-zzz";
    init_v8();
    let runtime = Runtime::builder()
        .modules(m(r#"
        import { env as moduleEnv } from "zeroship";
        export default {
            async fetch(request, env, ctx) {
                // Walk every JS-reachable auth surface and serialize it.
                const surfaces = {
                    env: safeStringify(env),
                    auth: safeStringify(env.auth),
                    authKeys: Object.keys(env.auth ?? {}),
                    moduleAuth: safeStringify(moduleEnv.auth),
                };
                // Also attempt the op and capture whatever it throws/returns.
                let opResult = null;
                try {
                    opResult = await env.auth.getAccessToken({ audience: "zeroship:control", scopes: ["apps:read"] });
                } catch (e) {
                    opResult = { error: e?.code ?? null, message: e?.message ?? String(e) };
                }
                return Response.json({ surfaces, opResult: safeJson(opResult) });
                function safeStringify(o) {
                    try { return JSON.stringify(o, Object.keys(o ?? {})); } catch { return String(o); }
                }
                function safeJson(o) { try { return JSON.parse(JSON.stringify(o)); } catch { return String(o); } }
            }
        };
        "#))
        .plugins(vec![Arc::new(AuthPlugin) as Arc<dyn NativePlugin>])
        .power_token_config("http://127.0.0.1:1", SECRET_CONTROL_KEY)
        .build();

    let (status, body) =
        dispatch_async_with_user_header(runtime, Some(USER_JSON.to_string()), Some("signed-hdr".into()));
    assert_eq!(status, 200, "body: {body}");
    // The headline assertion: the control_key never appears in anything JS
    // could read or serialize.
    assert!(
        !body.contains(SECRET_CONTROL_KEY),
        "control_key leaked into a JS-reachable surface! body: {body}"
    );
}
