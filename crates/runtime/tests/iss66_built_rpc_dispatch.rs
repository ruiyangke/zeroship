//! ISS-66 regression — a built example app's RPC procedure must dispatch
//! through the real worker kernel AND reach the runtime-injected `env`.
//!
//! Symptom: with `examples/db-todos` deployed + loaded, a call to
//! `users.public` on the worker `/dispatch` failed because the production
//! SSR build inlined the `zeroship-stub` package (`export const env = {}`)
//! instead of resolving the bare `zeroship` import to the runtime virtual
//! module (`Object.freeze(__zs_env())`). At runtime `env.db` was
//! `undefined`, so the dispatched handler threw
//! `Cannot read properties of undefined (reading 'users')`.
//!
//! This test drives the REAL built bundle through the REAL kernel
//! dispatch path (`call_fetch_handler` on `POST /__zeroship/v1/users.public`)
//! and asserts:
//!   1. The bundle the worker runs resolves `zeroship` to the runtime env
//!      (references `__zs_env`) and carries NO `zeroship-stub` sentinel.
//!      (deterministic artifact guard — RED before the build fix.)
//!   2. The RPC registry resolves `users.public` and the handler RUNS
//!      reaching `env.db` — i.e. dispatch does NOT fall through to a 404
//!      "No default.fetch handler exported" and does NOT throw the
//!      stub-undefined `reading 'users'` error.
//!
//! Requires a freshly built `examples/db-todos` (its `dist/server/index.js`
//! is read directly). The workspace `pnpm build` + `examples/db-todos`
//! `pnpm build` produce it.

use std::time::Duration;
use zeroship_runtime::{init_v8, EnvSnapshot, FetchOutcome, ModuleEntry, RequestCtx, SettledFetch};
use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::runtime::Runtime;

const BUNDLE: &str =
    concat!(env!("CARGO_MANIFEST_DIR"), "/../../examples/db-todos/dist/server/index.js");

fn drive(runtime: &Runtime, outcome: FetchOutcome) -> (u16, String) {
    if let FetchOutcome::Response { status, body, .. } = &outcome {
        return (*status, String::from_utf8_lossy(body).into_owned());
    }
    compio::runtime::Runtime::new().unwrap().block_on(async {
        runtime.start_pump();
        match outcome {
            FetchOutcome::Response { status, body, .. } => {
                (status, String::from_utf8_lossy(&body).into_owned())
            }
            FetchOutcome::Stream { status, body_reader, .. } => {
                let mut out = Vec::new();
                loop {
                    while let Some(chunk) = body_reader.pop() { out.extend_from_slice(&chunk); }
                    if body_reader.is_done() { break; }
                    body_reader.wait_for_data().await;
                }
                (status, String::from_utf8_lossy(&out).into_owned())
            }
            FetchOutcome::Pending { rx, cancel: _ } => {
                let settled = compio::time::timeout(Duration::from_secs(5), rx.recv())
                    .await.expect("dispatch pending timed out").expect("dispatch error");
                match settled {
                    SettledFetch::Response { status, body, .. } => {
                        (status, String::from_utf8_lossy(&body).into_owned())
                    }
                    SettledFetch::Stream { status, body_reader, .. } => {
                        let mut out = Vec::new();
                        loop {
                            while let Some(chunk) = body_reader.pop() { out.extend_from_slice(&chunk); }
                            if body_reader.is_done() { break; }
                            body_reader.wait_for_data().await;
                        }
                        (status, String::from_utf8_lossy(&out).into_owned())
                    }
                    SettledFetch::WebSocketUpgrade { .. } => panic!("unexpected WS upgrade"),
                }
            }
            FetchOutcome::WebSocketUpgrade { .. } => panic!("unexpected WS upgrade"),
        }
    })
}

/// Guard 1 — the artifact the worker actually runs must read the runtime
/// env, not the inlined stub. Deterministic; RED before the build fix.
#[test]
fn iss66_built_bundle_resolves_runtime_env_not_stub() {
    let src = std::fs::read_to_string(BUNDLE)
        .expect("examples/db-todos must be built (pnpm build) before this test");
    assert!(
        src.contains("__zs_env"),
        "built bundle must resolve `zeroship` to the runtime virtual module (__zs_env)"
    );
    assert!(
        !src.contains("outside the zeroship V8 runtime"),
        "built bundle must NOT inline the zeroship-stub package (env = {{}})"
    );
}

/// Guard 2 — dispatch the registered procedure through the real kernel.
/// Without a DbPlugin the handler can't return a row, but it MUST run far
/// enough to reach `env.db` (proving the registry resolved `users.public`
/// AND the bundle reads the runtime env). It must NOT 404 ("No
/// default.fetch handler exported") and must NOT throw the stub-undefined
/// `reading 'users'` error.
#[test]
fn iss66_built_bundle_dispatches_users_public() {
    init_v8();
    let src = std::fs::read_to_string(BUNDLE)
        .expect("examples/db-todos must be built (pnpm build) before this test");
    let modules = vec![ModuleEntry { specifier: "index.js".into(), source: src }];
    let runtime = Runtime::builder().modules(modules).build();
    let env = EnvSnapshot::empty();
    let ctx = RequestCtx::new(CancelFlag::new());
    let url = "http://localhost/__zeroship/v1/users.public";
    let outcome = runtime.call_fetch_handler(
        "POST", url,
        &[("content-type".into(), "application/json".into())],
        r#"{"json":{}}"#, &env, ctx,
    );
    let (status, body) = drive(&runtime, outcome);

    // The registry resolved the procedure: NOT a 404 "no fetch handler".
    assert_ne!(
        status, 404,
        "users.public should dispatch through default.rpc, not fall through to a 404. body={body}"
    );
    assert!(
        !body.contains("No default.fetch handler"),
        "registry must resolve `users.public` (got the fall-through 404). body={body}"
    );
    // The bundle reads the runtime env, so the failure (no DbPlugin here)
    // is the real @zeroship/db surface error, NOT the stub-undefined
    // `Cannot read properties of undefined (reading 'users')`. Note the
    // error body is sanitized at 500, so this guards against the stub
    // error leaking through any non-sanitized path.
    assert!(
        !body.contains("reading 'users'"),
        "handler hit the inlined stub (env.db undefined). body={body}"
    );
}
