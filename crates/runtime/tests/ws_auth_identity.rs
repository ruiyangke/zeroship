//! Regression test for P4-B-2: the WS-event pump must bind THIS
//! connection's authenticated user for every WS turn (onmessage /
//! onclose), so `env.auth.getUser()` inside a WebSocket handler returns
//! the connection's user — never null, never a stale leftover from a
//! prior request on the pooled isolate.
//!
//! This drives the REAL path:
//!   - a `default.fetch` handler authenticated as USER_A builds a
//!     `WebSocketPair`, accepts the server side, registers `onmessage`
//!     that calls `env.auth.getUser()` and records the id in a
//!     module-scoped variable, and returns
//!     `new Response(null, { status: 101, webSocket: client })`;
//!   - we construct the identity-bleed window by leaving a *different*
//!     user (USER_B) resident as the isolate's currently-attributed
//!     request (exactly what a prior request on a pooled isolate
//!     leaves behind);
//!   - we push a real inbound text frame onto the server socket's event
//!     queue (`push_event_pub`, the same call the network task makes),
//!     which spawns the real `OpResult::WebSocketEvent` op the pump
//!     drains and dispatches through `dispatch_pending_ws_events`;
//!   - a follow-up `GET /result` fetch returns the id the WS handler
//!     observed, which we assert in Rust.
//!
//! Pre-fix: the WS arm dispatches without setting the per-turn auth
//! context, so `getUser()` returns USER_B (the stale leftover) — the
//! identity-bleed bug. Post-fix: it returns USER_A.

#![cfg(feature = "runtime_native_websocket")]

mod common;
use common::*;

use std::sync::Arc;
use std::time::{Duration, Instant};

use zeroship_runtime::auth::AuthPlugin;
use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::plugin::NativePlugin;
use zeroship_runtime::websocket_native::network::{push_event_pub, WsEvent};
use zeroship_runtime::{init_v8, EnvSnapshot, FetchOutcome, RequestCtx, Runtime};

const USER_A: &str = r#"{"id":"usr_AAA","email":"a@example.com","scopes":["openid"]}"#;
const USER_B: &str = r#"{"id":"usr_BBB","email":"b@example.com","scopes":["openid"]}"#;

/// Dispatch `GET /result` (anonymous) and return the `id` the WS handler
/// recorded into the module-scoped `lastSeenUserId`.
fn result_id(runtime: &Runtime) -> String {
    let env = EnvSnapshot::empty();
    let ctx = RequestCtx::new(CancelFlag::new());
    let outcome = runtime.call_fetch_handler_with_user(
        "GET",
        "http://localhost/result",
        &[],
        "",
        &env,
        ctx,
        None,
    );
    match outcome {
        FetchOutcome::Response { body, .. } => {
            let v: serde_json::Value = serde_json::from_slice(&body).expect("/result body is JSON");
            v["id"].as_str().unwrap_or("UNSET").to_string()
        }
        _ => panic!("/result should return a Response"),
    }
}

/// App that, on WS upgrade (authenticated as the connecting user),
/// registers an `onmessage` handler that reads `env.auth.getUser()` and
/// records the observed id in a module-scoped variable. A follow-up
/// `GET /result` returns it so the test can assert which identity the
/// handler saw.
const APP_SRC: &str = r#"
let lastSeenUserId = "UNSET";
export default {
    fetch(request, env, ctx) {
        const url = new URL(request.url);
        if (url.pathname === "/result") {
            return Response.json({ id: lastSeenUserId });
        }
        const pair = new WebSocketPair();
        const client = pair[0];
        const server = pair[1];
        server.accept();
        server.addEventListener("message", (e) => {
            const u = env.auth.getUser();
            lastSeenUserId = (u && u.id) ? u.id : "NULL";
        });
        return new Response(null, { status: 101, webSocket: client });
    }
};
"#;

#[test]
fn ws_handler_getuser_is_connection_user_not_stale_leftover() {
    init_v8();
    let runtime = Runtime::builder()
        .modules(m(APP_SRC))
        .plugins(vec![Arc::new(AuthPlugin) as Arc<dyn NativePlugin>])
        .build();

    compio::runtime::Runtime::new().unwrap().block_on(async {
        runtime.start_pump();

        // Upgrade request authenticated as USER_A.
        let env = EnvSnapshot::empty();
        let ctx = RequestCtx::new(CancelFlag::new());
        let outcome = runtime.call_fetch_handler_with_user(
            "GET",
            "http://localhost/",
            &[],
            "",
            &env,
            ctx,
            Some(USER_A.to_string()),
        );

        let client_id = match outcome {
            FetchOutcome::WebSocketUpgrade { ws_id, .. } => ws_id,
            other => {
                let name = match other {
                    FetchOutcome::Response { status, body, .. } => {
                        let body = String::from_utf8_lossy(&body).into_owned();
                        format!("Response(status={status}, body={body})")
                    }
                    FetchOutcome::Stream { .. } => "Stream".into(),
                    FetchOutcome::Pending { .. } => "Pending".into(),
                    FetchOutcome::WebSocketUpgrade { .. } => unreachable!(),
                };
                panic!("expected WebSocketUpgrade outcome, got {name}");
            }
        };

        // Exactly one pair was minted, so the two native ws ids are
        // {1, 2}. The returned (client) socket is `client_id`; the
        // server socket the app holds is the other one.
        let server_id = if client_id == 1 { 2 } else { 1 };

        // Construct the identity-bleed window: leave a *different* user
        // (USER_B) resident as the isolate's currently-attributed
        // request — exactly what a prior request on a pooled isolate
        // leaves behind. A WS turn that fails to re-establish THIS
        // connection's identity would read this stale user.
        {
            let state = runtime.state();
            let mut s = state.borrow_mut();
            let stale_rid = 999_999u64;
            s.per_request_user.insert(stale_rid, USER_B.to_string());
            s.executing_request_id = Some(stale_rid);
        }

        // Push a real inbound text frame onto the server socket — the
        // same call the network task makes. This spawns the real
        // `OpResult::WebSocketEvent` op the pump drains + dispatches.
        let state = runtime.state();
        push_event_pub(&state, server_id, WsEvent::MessageText("ping".into()));
        runtime.notify_pump();

        // Let the pump process the WS event (it runs the onmessage
        // handler, which records the observed id).
        let start = Instant::now();
        let mut seen: Option<String> = None;
        while start.elapsed() < Duration::from_secs(5) {
            compio::time::sleep(Duration::from_millis(20)).await;
            let id = result_id(&runtime);
            if id != "UNSET" {
                seen = Some(id);
                break;
            }
        }

        let seen = seen.expect("server onmessage handler never ran (lastSeenUserId still UNSET)");
        assert_eq!(
            seen, "usr_AAA",
            "WS onmessage env.auth.getUser() must see THIS connection's user (USER_A), \
             not the stale leftover (USER_B) nor null; got {seen}"
        );
    });
}
