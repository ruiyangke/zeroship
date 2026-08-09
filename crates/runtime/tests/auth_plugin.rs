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
use zeroship_runtime::plugin::{NativePlugin, NativeRegistrar};
use zeroship_runtime::rpc::error::ZsErrorCode;
use zeroship_runtime::state::{OpResult, SharedState};
use zeroship_runtime::{
    init_v8, EnvSnapshot, FetchOutcome, RequestCtx, Runtime, SettledFetch,
};

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

struct TurnPlugin;

impl NativePlugin for TurnPlugin {
    fn namespace(&self) -> &str {
        "turn"
    }

    fn register(&self, r: &mut NativeRegistrar) {
        r.add("fulfill", fulfill_turn);
        r.add("reject", reject_turn);
    }
}

fn enqueue_turn_result(
    scope: &mut v8::PinScope,
    mut rv: v8::ReturnValue,
    reject: bool,
) {
    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();
    let resolver = v8::PromiseResolver::new(scope).expect("promise resolver");
    let promise = resolver.get_promise(scope);
    let resolver = v8::Global::new(scope, resolver);

    let mut state_mut = state.borrow_mut();
    let op_id = state_mut.next_op_id;
    state_mut.next_op_id += 1;
    state_mut.pending_resolvers.insert(op_id, resolver);
    let request_id = state_mut.executing_request_id;
    state_mut.spawned_ops.push(Box::pin(async move {
        compio::time::sleep(Duration::from_millis(1)).await;
        if reject {
            OpResult::Failed {
                op_id,
                error: "turn rejected".to_string(),
                request_id,
            }
        } else {
            OpResult::Completed {
                op_id,
                value: "turn fulfilled".to_string(),
                request_id,
            }
        }
    }));
    drop(state_mut);

    rv.set(promise.into());
}

fn fulfill_turn(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    rv: v8::ReturnValue,
) {
    enqueue_turn_result(scope, rv, false);
}

fn reject_turn(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    rv: v8::ReturnValue,
) {
    enqueue_turn_result(scope, rv, true);
}

fn build_runtime_with_auth_and_turn(source: &str) -> Runtime {
    init_v8();
    Runtime::builder()
        .modules(m(source))
        .plugins(vec![
            Arc::new(AuthPlugin) as Arc<dyn NativePlugin>,
            Arc::new(TurnPlugin) as Arc<dyn NativePlugin>,
        ])
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
    );
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

/// ISS-67: an UNCAUGHT `env.auth.requireUser()` throw on an anonymous request
/// must surface as a clean **401** through the real dispatch error rail — NOT a
/// masked 500. The throw carries an explicit `status: 401` (+ `code`), so the
/// kernel dispatcher honors it as a 4xx and the body-sanitization rail (which
/// only blanks 5xx) leaves the "Authentication required" message intact.
///
/// This drives the REAL path: the handler does NOT catch, so the exception
/// propagates through `call_fetch_handler_with_user` → `build_error_body`
/// exactly as a production anon RPC would. Pre-fix this returned 500 with a
/// `{"message":"internal error"}` body.
#[test]
fn require_user_anonymous_surfaces_as_401_not_masked_500() {
    let runtime = build_runtime_with_auth(
        r#"
        export default {
            fetch(request, env, ctx) {
                // Uncaught on purpose: let the dispatch error rail render it.
                env.auth.requireUser();
                return Response.json({ unreachable: true });
            }
        };
    "#,
    );

    let (status, body) = dispatch_with_user(&runtime, None);
    assert_eq!(
        status, 401,
        "anon requireUser() must surface as 401, not a masked 500; body: {body}"
    );
    let v: serde_json::Value = serde_json::from_str(&body).expect("body is JSON");
    assert_eq!(
        v["message"], "Authentication required",
        "the 401 body must carry the real message (4xx is not masked); body: {body}"
    );
    assert_ne!(
        v["message"], "internal error",
        "the message must NOT be the 5xx mask sentinel; body: {body}"
    );
    // This previously asserted the lowercase "unauthenticated", which ENCODED
    // the dev-vs-deployed divergence: no consumer in the tree compares against
    // that spelling. Compare against the wire enum rather than a literal so a
    // drift in either direction fails here.
    assert_eq!(
        v["code"],
        ZsErrorCode::Unauthenticated.as_wire_str(),
        "the throw should carry the canonical machine code; body: {body}"
    );
}

/// The dev-vs-deployed seam, Rust half. The `code` the native `requireUser()`
/// throw carries must be a MEMBER of the canonical RPC wire-code set and must
/// classify as `Unauthenticated`, not merely be "some stable string".
///
/// Why that is behaviour and not spelling: `@zeroship/rpc`'s
/// `parseErrorResponse` lifts the body's `code` VERBATIM, falling back to the
/// status-derived "UNAUTHENTICATED" only when the body carries NONE. A code
/// outside the canonical set therefore does not merely fail to match the
/// client's `onAuthExpired` guard: it OVERRIDES the correct status-derived
/// one. Deployed, the gateway's `unauthenticated_response` answers 401 with the
/// canonical code before the worker ever runs, so the hook fires; in dev this
/// throw IS the 401 source, so a divergent code silently disables an app's
/// re-authentication. JS half: `sdks/auth/tests/auth-expired-seam.test.ts`.
#[test]
fn require_user_anonymous_code_is_canonical_unauthenticated() {
    let runtime = build_runtime_with_auth(
        r#"
        export default {
            fetch(request, env, ctx) {
                env.auth.requireUser();
                return Response.json({ unreachable: true });
            }
        };
    "#,
    );

    let (status, body) = dispatch_with_user(&runtime, None);
    assert_eq!(status, 401, "body: {body}");
    let v: serde_json::Value = serde_json::from_str(&body).expect("body is JSON");
    let code = v["code"].as_str().unwrap_or_else(|| {
        panic!("the 401 body must carry a string `code`; body: {body}");
    });
    assert_eq!(
        ZsErrorCode::from_wire_str(code),
        Some(ZsErrorCode::Unauthenticated),
        "`{code}` is not the canonical UNAUTHENTICATED wire code: a client \
         that classifies by the canonical set cannot recognise it, and it \
         overrides the status-derived code rather than deferring to it; \
         body: {body}"
    );
}

/// One-variable control for the test above. Identical throw shape, identical
/// 401 status, identical dispatch rail; only the `code` string differs. It
/// separates "the producer emits the auth code" from "`from_wire_str` maps
/// everything to `Unauthenticated`" / "the rail rewrites every 401's code".
#[test]
fn non_auth_code_at_the_same_401_status_does_not_classify_as_unauthenticated() {
    let runtime = build_runtime_with_auth(
        r#"
        export default {
            fetch(request, env, ctx) {
                throw Object.assign(new Error("Authentication required"), {
                    status: 401,
                    code: "INVALID_ARGUMENT",
                });
            }
        };
    "#,
    );

    let (status, body) = dispatch_with_user(&runtime, None);
    assert_eq!(status, 401, "the control must hold the status variable fixed; body: {body}");
    let v: serde_json::Value = serde_json::from_str(&body).expect("body is JSON");
    let code = v["code"].as_str().unwrap_or_else(|| {
        panic!("the 401 body must carry a string `code`; body: {body}");
    });
    assert_eq!(
        ZsErrorCode::from_wire_str(code),
        Some(ZsErrorCode::InvalidArgument),
        "the rail must carry the thrown code through unchanged; body: {body}"
    );
    assert_ne!(
        ZsErrorCode::from_wire_str(code),
        Some(ZsErrorCode::Unauthenticated),
        "a non-auth code at a 401 status must NOT classify as UNAUTHENTICATED, \
         else the assertion above would pass for any 401; body: {body}"
    );
}

/// Sibling guard: an authenticated `requireUser()` that returns its value
/// uncaught still yields a normal 200 — the 401 path is anon-only and the
/// happy path is unchanged.
#[test]
fn require_user_authenticated_uncaught_is_200() {
    let runtime = build_runtime_with_auth(
        r#"
        export default {
            fetch(request, env, ctx) {
                const u = env.auth.requireUser();
                return Response.json({ id: u.id });
            }
        };
    "#,
    );

    let (status, body) = dispatch_with_user(&runtime, Some(USER_JSON.to_string()));
    assert_eq!(status, 200, "body: {body}");
    assert!(body.contains(r#""id":"usr_abc123""#), "body: {body}");
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

#[test]
fn concurrent_rpc_continuation_keeps_request_user() {
    const USER_A_JSON: &str = r#"{"id":"user-a"}"#;
    const USER_B_JSON: &str = r#"{"id":"user-b"}"#;

    let runtime = build_runtime_with_auth(
        r#"
        import { env } from "zeroship";

        let releaseB;
        let finishB;
        const bFinished = new Promise((resolve) => { finishB = resolve; });
        let bObservedUser;

        async function suspendB() {
            await new Promise((resolve) => { releaseB = resolve; });
            bObservedUser = env.auth.getUser()?.id ?? null;
            finishB();
            return bObservedUser;
        }

        async function releaseFromA() {
            const aObservedUser = env.auth.getUser()?.id ?? null;
            releaseB();
            await bFinished;
            return { aObservedUser, bObservedUser };
        }

        export default {
            rpc: { suspendB, releaseFromA },
        };
    "#,
    );

    let env = EnvSnapshot::empty();
    let b_outcome = runtime.call_fetch_handler_with_user(
        "POST",
        "http://localhost/__zeroship/v1/suspendB",
        &[("content-type".into(), "application/json".into())],
        r#"{"json":null}"#,
        &env,
        RequestCtx::new(CancelFlag::new()),
        Some(USER_B_JSON.to_string()),
    );
    assert!(
        matches!(&b_outcome, FetchOutcome::Pending { .. }),
        "request B must remain pending before request A releases it"
    );

    let a_outcome = runtime.call_fetch_handler_with_user(
        "POST",
        "http://localhost/__zeroship/v1/releaseFromA",
        &[("content-type".into(), "application/json".into())],
        r#"{"json":null}"#,
        &env,
        RequestCtx::new(CancelFlag::new()),
        Some(USER_A_JSON.to_string()),
    );
    let FetchOutcome::Response { status, body, .. } = a_outcome else {
        panic!("request A must settle while draining request B's continuation");
    };
    let body = String::from_utf8(body).expect("response body must be UTF-8");
    assert_eq!(status, 200, "body: {body}");
    let value: serde_json::Value = serde_json::from_str(&body).expect("body is JSON");
    assert_eq!(value["json"]["aObservedUser"], "user-a", "body: {body}");
    assert_eq!(
        value["json"]["bObservedUser"], "user-b",
        "request B's continuation must resolve request B's user; body: {body}"
    );
}

#[test]
fn concurrent_fetch_continuation_keeps_request_user() {
    const USER_A_JSON: &str = r#"{"id":"user-a"}"#;
    const USER_B_JSON: &str = r#"{"id":"user-b"}"#;

    let runtime = build_runtime_with_auth(
        r#"
        let releaseB;
        let finishB;
        const bFinished = new Promise((resolve) => { finishB = resolve; });
        let bObservedUser;

        export default {
            async fetch(request, env) {
                const path = new URL(request.url).pathname;
                if (path === "/b") {
                    await new Promise((resolve) => { releaseB = resolve; });
                    bObservedUser = env.auth.getUser()?.id ?? null;
                    finishB();
                    return Response.json({ bObservedUser });
                }

                const aBefore = env.auth.getUser()?.id ?? null;
                releaseB();
                await bFinished;
                const aAfter = env.auth.getUser()?.id ?? null;
                return Response.json({ aBefore, bObservedUser, aAfter });
            }
        };
    "#,
    );

    let env = EnvSnapshot::empty();
    let b_outcome = runtime.call_fetch_handler_with_user(
        "GET",
        "http://localhost/b",
        &[],
        "",
        &env,
        RequestCtx::new(CancelFlag::new()),
        Some(USER_B_JSON.to_string()),
    );
    assert!(
        matches!(&b_outcome, FetchOutcome::Pending { .. }),
        "request B must remain pending before request A releases it"
    );

    let a_outcome = runtime.call_fetch_handler_with_user(
        "GET",
        "http://localhost/a",
        &[],
        "",
        &env,
        RequestCtx::new(CancelFlag::new()),
        Some(USER_A_JSON.to_string()),
    );
    let FetchOutcome::Response { status, body, .. } = a_outcome else {
        panic!("request A must settle while draining request B's continuation");
    };
    let body = String::from_utf8(body).expect("response body must be UTF-8");
    assert_eq!(status, 200, "body: {body}");
    let value: serde_json::Value = serde_json::from_str(&body).expect("body is JSON");
    assert_eq!(value["aBefore"], "user-a", "body: {body}");
    assert_eq!(
        value["bObservedUser"], "user-b",
        "request B's fetch continuation must retain request B's user; body: {body}"
    );
    assert_eq!(value["aAfter"], "user-a", "body: {body}");
}

#[test]
fn next_tick_scheduled_by_foreign_rpc_continuation_keeps_owner() {
    const USER_A_JSON: &str = r#"{"id":"user-a"}"#;
    const USER_B_JSON: &str = r#"{"id":"user-b"}"#;

    let runtime = build_runtime_with_auth(
        r#"
        import { env } from "zeroship";

        let releaseB;
        let finishB;
        const bFinished = new Promise((resolve) => { finishB = resolve; });
        let bContinuationUser;
        let bTickUser;

        async function requestB() {
            await new Promise((resolve) => { releaseB = resolve; });
            bContinuationUser = env.auth.getUser()?.id ?? null;
            process.nextTick(() => {
                bTickUser = env.auth.getUser()?.id ?? null;
                finishB();
            });
            await bFinished;
            return { bContinuationUser, bTickUser };
        }

        async function requestA() {
            const aBefore = env.auth.getUser()?.id ?? null;
            releaseB();
            await bFinished;
            const aAfter = env.auth.getUser()?.id ?? null;
            return { aBefore, aAfter, bContinuationUser, bTickUser };
        }

        export default {
            rpc: { requestB, requestA },
        };
    "#,
    );

    let env = EnvSnapshot::empty();
    let b_outcome = runtime.call_fetch_handler_with_user(
        "POST",
        "http://localhost/__zeroship/v1/requestB",
        &[("content-type".into(), "application/json".into())],
        r#"{"json":null}"#,
        &env,
        RequestCtx::new(CancelFlag::new()),
        Some(USER_B_JSON.to_string()),
    );
    assert!(matches!(&b_outcome, FetchOutcome::Pending { .. }));

    let a_outcome = runtime.call_fetch_handler_with_user(
        "POST",
        "http://localhost/__zeroship/v1/requestA",
        &[("content-type".into(), "application/json".into())],
        r#"{"json":null}"#,
        &env,
        RequestCtx::new(CancelFlag::new()),
        Some(USER_A_JSON.to_string()),
    );
    let FetchOutcome::Response { status, body, .. } = a_outcome else {
        panic!("request A must settle after draining request B's next tick");
    };
    let body = String::from_utf8(body).expect("response body must be UTF-8");
    assert_eq!(status, 200, "body: {body}");
    let value: serde_json::Value = serde_json::from_str(&body).expect("body is JSON");
    assert_eq!(value["json"]["aBefore"], "user-a", "body: {body}");
    assert_eq!(value["json"]["aAfter"], "user-a", "body: {body}");
    assert_eq!(
        value["json"]["bContinuationUser"], "user-b",
        "body: {body}"
    );
    assert_eq!(
        value["json"]["bTickUser"], "user-b",
        "the next-tick callback must retain request B's invocation; body: {body}"
    );
}

#[test]
fn module_scope_continuation_stays_anonymous() {
    const USER_A_JSON: &str = r#"{"id":"user-a"}"#;

    let runtime = build_runtime_with_auth(
        r#"
        import { env } from "zeroship";

        let releaseModule;
        const moduleGate = new Promise((resolve) => { releaseModule = resolve; });
        let finishModule;
        const moduleFinished = new Promise((resolve) => { finishModule = resolve; });
        let moduleObservedUser = "UNSET";

        moduleGate.then(() => {
            moduleObservedUser = env.auth.getUser()?.id ?? null;
            finishModule();
        });

        async function requestA() {
            const aBefore = env.auth.getUser()?.id ?? null;
            releaseModule();
            await moduleFinished;
            const aAfter = env.auth.getUser()?.id ?? null;
            return { aBefore, aAfter, moduleObservedUser };
        }

        export default {
            rpc: { requestA },
        };
    "#,
    );

    let env = EnvSnapshot::empty();
    let outcome = runtime.call_fetch_handler_with_user(
        "POST",
        "http://localhost/__zeroship/v1/requestA",
        &[("content-type".into(), "application/json".into())],
        r#"{"json":null}"#,
        &env,
        RequestCtx::new(CancelFlag::new()),
        Some(USER_A_JSON.to_string()),
    );
    let FetchOutcome::Response { status, body, .. } = outcome else {
        panic!("request A must settle after the module continuation");
    };
    let body = String::from_utf8(body).expect("response body must be UTF-8");
    assert_eq!(status, 200, "body: {body}");
    let value: serde_json::Value = serde_json::from_str(&body).expect("body is JSON");
    assert_eq!(value["json"]["aBefore"], "user-a", "body: {body}");
    assert_eq!(value["json"]["aAfter"], "user-a", "body: {body}");
    assert_eq!(
        value["json"]["moduleObservedUser"],
        serde_json::Value::Null,
        "module-scoped work must not inherit the request that resolves it; body: {body}"
    );
}

#[test]
fn module_level_enter_with_remains_ambient_for_dispatches() {
    const USER_A_JSON: &str = r#"{"id":"user-a"}"#;

    let runtime = build_runtime_with_auth(
        r#"
        import { AsyncLocalStorage } from "node:async_hooks";
        import { env } from "zeroship";

        const moduleAls = new AsyncLocalStorage();
        moduleAls.enterWith("module-store");

        async function requestA() {
            const beforeAwait = moduleAls.getStore();
            await Promise.resolve();
            return {
                beforeAwait,
                afterAwait: moduleAls.getStore(),
                user: env.auth.getUser()?.id ?? null,
            };
        }

        export default {
            rpc: { requestA },
        };
    "#,
    );

    let env = EnvSnapshot::empty();
    let outcome = runtime.call_fetch_handler_with_user(
        "POST",
        "http://localhost/__zeroship/v1/requestA",
        &[("content-type".into(), "application/json".into())],
        r#"{"json":null}"#,
        &env,
        RequestCtx::new(CancelFlag::new()),
        Some(USER_A_JSON.to_string()),
    );
    let FetchOutcome::Response { status, body, .. } = outcome else {
        panic!("request A must settle synchronously");
    };
    let body = String::from_utf8(body).expect("response body must be UTF-8");
    assert_eq!(status, 200, "body: {body}");
    let value: serde_json::Value = serde_json::from_str(&body).expect("body is JSON");
    assert_eq!(value["json"]["beforeAwait"], "module-store", "body: {body}");
    assert_eq!(value["json"]["afterAwait"], "module-store", "body: {body}");
    assert_eq!(value["json"]["user"], "user-a", "body: {body}");
}

#[test]
fn anonymous_rpc_frame_does_not_fall_back_to_foreign_user() {
    const USER_A_JSON: &str = r#"{"id":"user-a"}"#;

    let runtime = build_runtime_with_auth(
        r#"
        import { env } from "zeroship";

        let releaseB;
        let finishB;
        const bFinished = new Promise((resolve) => { finishB = resolve; });
        let bObservation;

        async function suspendB() {
            await new Promise((resolve) => { releaseB = resolve; });
            const getUserIsNull = env.auth.getUser() === null;
            let requireUserCode = null;
            try {
                env.auth.requireUser();
            } catch (error) {
                requireUserCode = error?.code ?? null;
            }
            bObservation = { getUserIsNull, requireUserCode };
            finishB();
            return bObservation;
        }

        async function releaseFromA() {
            const aObservedUser = env.auth.getUser()?.id ?? null;
            releaseB();
            await bFinished;
            return { aObservedUser, bObservation };
        }

        export default {
            rpc: { suspendB, releaseFromA },
        };
    "#,
    );

    let env = EnvSnapshot::empty();
    let b_outcome = runtime.call_fetch_handler_with_user(
        "POST",
        "http://localhost/__zeroship/v1/suspendB",
        &[("content-type".into(), "application/json".into())],
        r#"{"json":null}"#,
        &env,
        RequestCtx::new(CancelFlag::new()),
        None,
    );
    assert!(matches!(&b_outcome, FetchOutcome::Pending { .. }));

    let a_outcome = runtime.call_fetch_handler_with_user(
        "POST",
        "http://localhost/__zeroship/v1/releaseFromA",
        &[("content-type".into(), "application/json".into())],
        r#"{"json":null}"#,
        &env,
        RequestCtx::new(CancelFlag::new()),
        Some(USER_A_JSON.to_string()),
    );
    let FetchOutcome::Response { status, body, .. } = a_outcome else {
        panic!("request A must settle while draining request B's continuation");
    };
    let body = String::from_utf8(body).expect("response body must be UTF-8");
    assert_eq!(status, 200, "body: {body}");
    let value: serde_json::Value = serde_json::from_str(&body).expect("body is JSON");
    assert_eq!(value["json"]["aObservedUser"], "user-a", "body: {body}");
    assert_eq!(
        value["json"]["bObservation"]["getUserIsNull"], true,
        "body: {body}"
    );
    // Was "unauthenticated", the same defect-encoding spelling; the code the
    // native throw carries is the canonical wire token.
    assert_eq!(
        value["json"]["bObservation"]["requireUserCode"],
        ZsErrorCode::Unauthenticated.as_wire_str(),
        "body: {body}"
    );
}

fn assert_op_checkpoint_users(mode: &str, expected_rejected: bool) {
    const USER_A_JSON: &str = r#"{"id":"user-a"}"#;
    const USER_B_JSON: &str = r#"{"id":"user-b"}"#;

    let runtime = build_runtime_with_auth_and_turn(
        r#"
        import { env } from "zeroship";

        let releaseA;
        const bFinished = new Promise((resolve) => { releaseA = resolve; });
        let bObservedUser;
        let opRejected;

        async function requestB(mode) {
            try {
                if (mode === "reject") {
                    await env.turn.reject();
                } else {
                    await env.turn.fulfill();
                }
                opRejected = false;
            } catch (_error) {
                opRejected = true;
            }
            bObservedUser = env.auth.getUser()?.id ?? null;
            releaseA();
            return { bObservedUser, opRejected };
        }

        async function requestA() {
            await bFinished;
            return {
                aObservedUser: env.auth.getUser()?.id ?? null,
                bObservedUser,
                opRejected,
            };
        }

        export default {
            rpc: { requestB, requestA },
        };
    "#,
    );

    compio::runtime::Runtime::new().unwrap().block_on(async {
        let env = EnvSnapshot::empty();
        let b_body = format!(r#"{{"json":"{mode}"}}"#);
        let b_outcome = runtime.call_fetch_handler_with_user(
            "POST",
            "http://localhost/__zeroship/v1/requestB",
            &[("content-type".into(), "application/json".into())],
            &b_body,
            &env,
            RequestCtx::new(CancelFlag::new()),
            Some(USER_B_JSON.to_string()),
        );
        assert!(matches!(&b_outcome, FetchOutcome::Pending { .. }));

        let a_outcome = runtime.call_fetch_handler_with_user(
            "POST",
            "http://localhost/__zeroship/v1/requestA",
            &[("content-type".into(), "application/json".into())],
            r#"{"json":null}"#,
            &env,
            RequestCtx::new(CancelFlag::new()),
            Some(USER_A_JSON.to_string()),
        );
        let FetchOutcome::Pending { rx: a_rx, .. } = a_outcome else {
            panic!("request A must wait for request B's native op");
        };

        runtime.start_pump();
        let settled = compio::time::timeout(Duration::from_secs(5), a_rx.recv())
            .await
            .expect("request A timed out")
            .expect("request A delivered DispatchError");
        let SettledFetch::Response { status, body, .. } = settled else {
            panic!("request A must settle to a response");
        };
        let body = String::from_utf8(body).expect("response body must be UTF-8");
        assert_eq!(status, 200, "body: {body}");
        let value: serde_json::Value = serde_json::from_str(&body).expect("body is JSON");
        assert_eq!(value["json"]["aObservedUser"], "user-a", "body: {body}");
        assert_eq!(value["json"]["bObservedUser"], "user-b", "body: {body}");
        assert_eq!(value["json"]["opRejected"], expected_rejected, "body: {body}");
    });
}

#[test]
fn op_resolve_checkpoint_restores_each_rpc_user() {
    assert_op_checkpoint_users("fulfill", false);
}

#[test]
fn op_reject_checkpoint_restores_each_rpc_user() {
    assert_op_checkpoint_users("reject", true);
}

#[test]
fn settled_fetch_inspection_uses_own_request_user() {
    const USER_A_JSON: &str = r#"{"id":"user-a"}"#;
    const USER_B_JSON: &str = r#"{"id":"user-b"}"#;

    let runtime = build_runtime_with_auth_and_turn(
        r#"
        let releaseA;
        const aGate = new Promise((resolve) => { releaseA = resolve; });

        export default {
            async fetch(request, env) {
                const path = new URL(request.url).pathname;
                if (path === "/a") {
                    await aGate;
                    return {
                        get status() {
                            return env.auth.getUser()?.id === "user-a" ? 207 : 418;
                        }
                    };
                }

                await env.turn.fulfill();
                releaseA();
                return Response.json({ user: env.auth.getUser()?.id ?? null });
            }
        };
    "#,
    );

    compio::runtime::Runtime::new().unwrap().block_on(async {
        let env = EnvSnapshot::empty();
        let a_outcome = runtime.call_fetch_handler_with_user(
            "GET",
            "http://localhost/a",
            &[],
            "",
            &env,
            RequestCtx::new(CancelFlag::new()),
            Some(USER_A_JSON.to_string()),
        );
        let FetchOutcome::Pending { rx: a_rx, .. } = a_outcome else {
            panic!("request A must wait for request B");
        };

        let b_outcome = runtime.call_fetch_handler_with_user(
            "GET",
            "http://localhost/b",
            &[],
            "",
            &env,
            RequestCtx::new(CancelFlag::new()),
            Some(USER_B_JSON.to_string()),
        );
        assert!(matches!(&b_outcome, FetchOutcome::Pending { .. }));

        runtime.start_pump();
        let settled = compio::time::timeout(Duration::from_secs(5), a_rx.recv())
            .await
            .expect("request A timed out")
            .expect("request A delivered DispatchError");
        let SettledFetch::Response { status, .. } = settled else {
            panic!("request A must settle to a response");
        };
        assert_eq!(
            status, 207,
            "request A's settlement-time getter must observe request A"
        );
    });
}

#[test]
fn timer_checkpoint_restores_each_rpc_user() {
    const USER_A_JSON: &str = r#"{"id":"user-a"}"#;
    const USER_B_JSON: &str = r#"{"id":"user-b"}"#;

    let runtime = build_runtime_with_auth(
        r#"
        import { env } from "zeroship";

        let releaseA;
        const timerFired = new Promise((resolve) => { releaseA = resolve; });
        let timerObservedUser;

        async function timerB() {
            await new Promise((resolve) => {
                setTimeout(() => {
                    timerObservedUser = env.auth.getUser()?.id ?? null;
                    releaseA();
                    resolve();
                }, 1);
            });
            return timerObservedUser;
        }

        async function waitingA() {
            await timerFired;
            return {
                aObservedUser: env.auth.getUser()?.id ?? null,
                timerObservedUser,
            };
        }

        export default {
            rpc: { timerB, waitingA },
        };
    "#,
    );

    compio::runtime::Runtime::new().unwrap().block_on(async {
        let env = EnvSnapshot::empty();
        let b_outcome = runtime.call_fetch_handler_with_user(
            "POST",
            "http://localhost/__zeroship/v1/timerB",
            &[("content-type".into(), "application/json".into())],
            r#"{"json":null}"#,
            &env,
            RequestCtx::new(CancelFlag::new()),
            Some(USER_B_JSON.to_string()),
        );
        assert!(matches!(&b_outcome, FetchOutcome::Pending { .. }));

        let a_outcome = runtime.call_fetch_handler_with_user(
            "POST",
            "http://localhost/__zeroship/v1/waitingA",
            &[("content-type".into(), "application/json".into())],
            r#"{"json":null}"#,
            &env,
            RequestCtx::new(CancelFlag::new()),
            Some(USER_A_JSON.to_string()),
        );
        let FetchOutcome::Pending { rx: a_rx, .. } = a_outcome else {
            panic!("request A must wait for request B's timer");
        };

        runtime.start_pump();
        let settled = compio::time::timeout(Duration::from_secs(5), a_rx.recv())
            .await
            .expect("request A timed out")
            .expect("request A delivered DispatchError");
        let SettledFetch::Response { status, body, .. } = settled else {
            panic!("request A must settle to a response");
        };
        let body = String::from_utf8(body).expect("response body must be UTF-8");
        assert_eq!(status, 200, "body: {body}");
        let value: serde_json::Value = serde_json::from_str(&body).expect("body is JSON");
        assert_eq!(value["json"]["aObservedUser"], "user-a", "body: {body}");
        assert_eq!(
            value["json"]["timerObservedUser"], "user-b",
            "body: {body}"
        );
    });
}

#[test]
fn timer_scheduled_by_foreign_rpc_continuation_keeps_owner() {
    const USER_A_JSON: &str = r#"{"id":"user-a"}"#;
    const USER_B_JSON: &str = r#"{"id":"user-b"}"#;

    let runtime = build_runtime_with_auth(
        r#"
        import { env } from "zeroship";

        let releaseB;
        let finishScheduled;
        const bScheduled = new Promise((resolve) => { finishScheduled = resolve; });
        let finishTimer;
        const timerFinished = new Promise((resolve) => { finishTimer = resolve; });
        let bSchedulingUser;
        let timerObservedUser;

        async function requestB() {
            await new Promise((resolve) => { releaseB = resolve; });
            bSchedulingUser = env.auth.getUser()?.id ?? null;
            setTimeout(() => {
                timerObservedUser = env.auth.getUser()?.id ?? null;
                finishTimer();
            }, 1);
            finishScheduled();
            await timerFinished;
            return { bSchedulingUser, timerObservedUser };
        }

        async function requestA() {
            const aBefore = env.auth.getUser()?.id ?? null;
            releaseB();
            await bScheduled;
            const aAfterScheduling = env.auth.getUser()?.id ?? null;
            await timerFinished;
            const aAfterTimer = env.auth.getUser()?.id ?? null;
            return {
                aBefore,
                aAfterScheduling,
                aAfterTimer,
                bSchedulingUser,
                timerObservedUser,
            };
        }

        export default {
            rpc: { requestB, requestA },
        };
    "#,
    );

    compio::runtime::Runtime::new().unwrap().block_on(async {
        let env = EnvSnapshot::empty();
        let b_outcome = runtime.call_fetch_handler_with_user(
            "POST",
            "http://localhost/__zeroship/v1/requestB",
            &[("content-type".into(), "application/json".into())],
            r#"{"json":null}"#,
            &env,
            RequestCtx::new(CancelFlag::new()),
            Some(USER_B_JSON.to_string()),
        );
        assert!(matches!(&b_outcome, FetchOutcome::Pending { .. }));

        let a_outcome = runtime.call_fetch_handler_with_user(
            "POST",
            "http://localhost/__zeroship/v1/requestA",
            &[("content-type".into(), "application/json".into())],
            r#"{"json":null}"#,
            &env,
            RequestCtx::new(CancelFlag::new()),
            Some(USER_A_JSON.to_string()),
        );
        let FetchOutcome::Pending { rx: a_rx, .. } = a_outcome else {
            panic!("request A must wait for request B's timer");
        };

        {
            let state = runtime.state();
            let state = state.borrow();
            let b_request_id = state
                .per_request_user
                .iter()
                .find_map(|(request_id, user)| (user == USER_B_JSON).then_some(*request_id))
                .expect("request B's user must remain registered");
            let owners: Vec<u64> = state.timer_owner.values().copied().collect();
            assert_eq!(
                owners,
                vec![b_request_id],
                "the timer must be owned by request B, not the checkpoint caller"
            );
        }

        runtime.start_pump();
        let settled = compio::time::timeout(Duration::from_secs(5), a_rx.recv())
            .await
            .expect("request A timed out")
            .expect("request A delivered DispatchError");
        let SettledFetch::Response { status, body, .. } = settled else {
            panic!("request A must settle to a response");
        };
        let body = String::from_utf8(body).expect("response body must be UTF-8");
        assert_eq!(status, 200, "body: {body}");
        let value: serde_json::Value = serde_json::from_str(&body).expect("body is JSON");
        assert_eq!(value["json"]["aBefore"], "user-a", "body: {body}");
        assert_eq!(
            value["json"]["aAfterScheduling"], "user-a",
            "body: {body}"
        );
        assert_eq!(value["json"]["aAfterTimer"], "user-a", "body: {body}");
        assert_eq!(
            value["json"]["bSchedulingUser"], "user-b",
            "request B must own work scheduled from its foreign continuation; body: {body}"
        );
        assert_eq!(
            value["json"]["timerObservedUser"], "user-b",
            "the timer callback must run with its scheduling invocation; body: {body}"
        );
    });
}

#[test]
fn async_timer_continuation_keeps_owner() {
    const USER_A_JSON: &str = r#"{"id":"user-a"}"#;
    const USER_B_JSON: &str = r#"{"id":"user-b"}"#;

    let runtime = build_runtime_with_auth(
        r#"
        import { env } from "zeroship";

        let releaseTimer;
        const timerGate = new Promise((resolve) => { releaseTimer = resolve; });
        let finishTimer;
        const timerFinished = new Promise((resolve) => { finishTimer = resolve; });
        let timerInitialUser;
        let timerAfterAwaitUser;

        async function requestB() {
            setTimeout(async () => {
                timerInitialUser = env.auth.getUser()?.id ?? null;
                await timerGate;
                timerAfterAwaitUser = env.auth.getUser()?.id ?? null;
                finishTimer();
            }, 1);
            await timerFinished;
            return { timerInitialUser, timerAfterAwaitUser };
        }

        async function releaseFromA() {
            const aBefore = env.auth.getUser()?.id ?? null;
            releaseTimer();
            await timerFinished;
            const aAfter = env.auth.getUser()?.id ?? null;
            return { aBefore, aAfter, timerInitialUser, timerAfterAwaitUser };
        }

        export default {
            rpc: { requestB, releaseFromA },
        };
    "#,
    );

    compio::runtime::Runtime::new().unwrap().block_on(async {
        let env = EnvSnapshot::empty();
        let b_outcome = runtime.call_fetch_handler_with_user(
            "POST",
            "http://localhost/__zeroship/v1/requestB",
            &[("content-type".into(), "application/json".into())],
            r#"{"json":null}"#,
            &env,
            RequestCtx::new(CancelFlag::new()),
            Some(USER_B_JSON.to_string()),
        );
        assert!(matches!(&b_outcome, FetchOutcome::Pending { .. }));

        let state = runtime.state();
        assert!(
            !state.borrow().timer_owner.is_empty(),
            "request B must register its timer before the pump starts"
        );
        runtime.start_pump();

        let mut timer_fired = false;
        for _ in 0..200 {
            if state.borrow().timer_owner.is_empty() {
                timer_fired = true;
                break;
            }
            compio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(timer_fired, "request B's timer did not fire");

        let a_outcome = runtime.call_fetch_handler_with_user(
            "POST",
            "http://localhost/__zeroship/v1/releaseFromA",
            &[("content-type".into(), "application/json".into())],
            r#"{"json":null}"#,
            &env,
            RequestCtx::new(CancelFlag::new()),
            Some(USER_A_JSON.to_string()),
        );
        let FetchOutcome::Response { status, body, .. } = a_outcome else {
            panic!("request A must settle while releasing the timer continuation");
        };
        let body = String::from_utf8(body).expect("response body must be UTF-8");
        assert_eq!(status, 200, "body: {body}");
        let value: serde_json::Value = serde_json::from_str(&body).expect("body is JSON");
        assert_eq!(value["json"]["aBefore"], "user-a", "body: {body}");
        assert_eq!(value["json"]["aAfter"], "user-a", "body: {body}");
        assert_eq!(
            value["json"]["timerInitialUser"], "user-b",
            "body: {body}"
        );
        assert_eq!(
            value["json"]["timerAfterAwaitUser"], "user-b",
            "the timer continuation must retain request B after suspension; body: {body}"
        );
    });
}
