//! Eviction-time abort tests for RPC.
//!
//! Covers the per-isolate `AbortRegistry` from
//! `crates/runtime/src/rpc/abort.rs`: register-on-dispatch, fire-on-
//! eviction, automatic unregister via Drop, and the worker-style
//! integration smoke (1-slot LRU evicting app A when app B loads).
//!
//! See `docs/proposals/rpc.md` §3 ("Abort source plumbing").

mod common;

use std::collections::HashMap;
use std::time::Duration;

use uuid::Uuid;

use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::rpc::abort;
use zeroship_runtime::runtime::Runtime;
use zeroship_runtime::{init_v8, EnvSnapshot, FetchOutcome, RequestCtx, SettledFetch};

use common::wrap_with_synthetic_entry;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a Runtime with `app_id` set + a tiny `default.{fetch,rpc}`
/// shim that exposes named exports of `user_source`. Mirrors
/// `common::wrap_with_synthetic_entry` but binds an `app_id`.
fn build_runtime_with_app(app_id: Uuid, user_source: &str, procs_block: &str) -> Runtime {
    init_v8();
    let modules = wrap_with_synthetic_entry(user_source, procs_block);
    Runtime::builder()
        .modules(modules)
        .env_vars(HashMap::new())
        .app_id(app_id)
        .build()
}

/// Kick off an RPC call against `runtime` for `<id>` with the given
/// JSON input and return the `FetchOutcome` without driving the pump.
/// The caller decides whether to await it or to leave the procedure
/// pending (test 1 + 2 leave it pending so eviction can fire mid-flight).
fn start_rpc(runtime: &Runtime, id: &str, input_json: &str) -> FetchOutcome {
    let env = EnvSnapshot::empty();
    let ctx = RequestCtx::new(CancelFlag::new());
    let url = format!("http://localhost/__zeroship/v1/{}", id);
    let body = format!(r#"{{"json":{}}}"#, input_json);
    runtime.call_fetch_handler(
        "POST",
        &url,
        &[("content-type".into(), "application/json".into())],
        &body,
        &env,
        ctx,
    )
}

/// Drive a `FetchOutcome::Pending` to completion within `timeout`,
/// returning `(status, body)`. Spawns a compio runtime if needed.
fn await_outcome(runtime: Runtime, outcome: FetchOutcome) -> (u16, String) {
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
                    .expect("pending timed out")
                    .expect("pending delivered DispatchError");
                match settled {
                    SettledFetch::Response { status, body, .. } => (status, body),
                    other => panic!("unexpected settle: {:?}", std::any::type_name_of_val(&other)),
                }
            }
            other => panic!("unexpected outcome: {:?}", std::any::type_name_of_val(&other)),
        }
    })
}

// ---------------------------------------------------------------------------
// 1 — single in-flight controller fires on eviction
// ---------------------------------------------------------------------------

#[test]
fn eviction_fires_single_inflight_controller() {
    let app_id = Uuid::new_v4();

    // Procedure: install an `abort` listener that flips a global flag,
    // then await a long timeout that won't resolve before the test
    // ends. The eviction sweep fires the controller; the listener
    // observes it and sets `globalThis.__zsAbortFired = true`.
    let rt = build_runtime_with_app(
        app_id,
        r#"
        export async function pending() {
            const ctx = __zeroshipGetRpcCtx();
            ctx.signal.addEventListener("abort", () => {
                globalThis.__zsAbortFired = true;
            });
            await new Promise(r => setTimeout(r, 60_000));
            return "should-not-resolve";
        }
        export function readFlag() {
            return { fired: globalThis.__zsAbortFired === true };
        }
        "#,
        "{ pending, readFlag }",
    );

    let rt_clone = rt.clone();
    compio::runtime::Runtime::new().unwrap().block_on(async move {
        rt_clone.start_pump();

        // Kick off `pending` — leaves a pending Promise tracked by the
        // pump. The dispatch synchronously ran `addEventListener`, so
        // the abort listener is wired before we evict.
        let outcome = start_rpc(&rt_clone, "pending", "[]");
        match outcome {
            FetchOutcome::Pending { rx: _rx, cancel: _ } => { /* expected */ }
            other => panic!("expected Pending, got {:?}", std::any::type_name_of_val(&other)),
        }

        // The registry must hold exactly one entry for this app.
        assert_eq!(abort::entries_for_app(app_id), 1, "registry should hold one entry");

        // Fire eviction. After this, the controller's signal-abort
        // algorithm has run synchronously — the listener fires inside
        // `entered_for_eviction`'s scope.
        rt_clone.with_scope(|scope| abort::entered_for_eviction(scope, app_id));

        // Yield once so any microtasks queued by the abort listener
        // get a chance to run before we check the flag. The abort
        // listener body is synchronous so a single zero-sleep is
        // sufficient.
        compio::time::sleep(Duration::ZERO).await;

        // Now run a sync probe through the same isolate to check the
        // global flag. The probe registers a NEW request, but the
        // `pending` request's controller has already been aborted +
        // cleared from the registry.
        let outcome = start_rpc(&rt_clone, "readFlag", "[]");
        let (status, body) = match outcome {
            FetchOutcome::Response { status, body, .. } => (status, body),
            FetchOutcome::Pending { rx, cancel: _ } => {
                let settled = compio::time::timeout(Duration::from_secs(2), rx.recv())
                    .await
                    .expect("readFlag timed out")
                    .expect("readFlag dispatch error");
                match settled {
                    SettledFetch::Response { status, body, .. } => (status, body),
                    other => panic!("unexpected settle: {:?}", std::any::type_name_of_val(&other)),
                }
            }
            other => panic!("unexpected outcome: {:?}", std::any::type_name_of_val(&other)),
        };
        assert_eq!(status, 200, "readFlag failed: {body}");
        assert!(body.contains(r#""fired":true"#), "abort listener didn't fire: {body}");
    });
}

// ---------------------------------------------------------------------------
// 2 — multiple in-flight controllers all fire
// ---------------------------------------------------------------------------

#[test]
fn eviction_fires_all_inflight_controllers() {
    let app_id = Uuid::new_v4();
    let rt = build_runtime_with_app(
        app_id,
        r#"
        globalThis.__zsAbortCount = 0;
        export async function p1() {
            __zeroshipGetRpcCtx().signal.addEventListener("abort", () => {
                globalThis.__zsAbortCount += 1;
            });
            await new Promise(r => setTimeout(r, 60_000));
        }
        export async function p2() {
            __zeroshipGetRpcCtx().signal.addEventListener("abort", () => {
                globalThis.__zsAbortCount += 1;
            });
            await new Promise(r => setTimeout(r, 60_000));
        }
        export function readCount() {
            return { count: globalThis.__zsAbortCount };
        }
        "#,
        "{ p1, p2, readCount }",
    );

    let rt_clone = rt.clone();
    compio::runtime::Runtime::new().unwrap().block_on(async move {
        rt_clone.start_pump();

        let _o1 = start_rpc(&rt_clone, "p1", "[]");
        let _o2 = start_rpc(&rt_clone, "p2", "[]");

        // Two pending procedures → two registry entries.
        assert_eq!(abort::entries_for_app(app_id), 2);

        rt_clone.with_scope(|scope| abort::entered_for_eviction(scope, app_id));
        compio::time::sleep(Duration::ZERO).await;

        let outcome = start_rpc(&rt_clone, "readCount", "[]");
        let (status, body) = match outcome {
            FetchOutcome::Response { status, body, .. } => (status, body),
            FetchOutcome::Pending { rx, cancel: _ } => {
                let settled = compio::time::timeout(Duration::from_secs(2), rx.recv())
                    .await
                    .expect("readCount timed out")
                    .expect("readCount dispatch error");
                match settled {
                    SettledFetch::Response { status, body, .. } => (status, body),
                    other => panic!("unexpected: {}", std::any::type_name_of_val(&other)),
                }
            }
            other => panic!("unexpected: {}", std::any::type_name_of_val(&other)),
        };
        assert_eq!(status, 200, "readCount failed: {body}");
        assert!(body.contains(r#""count":2"#), "expected count=2, got: {body}");
    });
}

// ---------------------------------------------------------------------------
// 3 — registry cleared after eviction
// ---------------------------------------------------------------------------

#[test]
fn eviction_clears_registry() {
    let app_id = Uuid::new_v4();
    let rt = build_runtime_with_app(
        app_id,
        r#"
        export async function pending() {
            await new Promise(r => setTimeout(r, 60_000));
        }
        "#,
        "{ pending }",
    );

    let rt_clone = rt.clone();
    compio::runtime::Runtime::new().unwrap().block_on(async move {
        rt_clone.start_pump();
        let _outcome = start_rpc(&rt_clone, "pending", "[]");
        assert_eq!(abort::entries_for_app(app_id), 1);

        rt_clone.with_scope(|scope| abort::entered_for_eviction(scope, app_id));
        assert_eq!(
            abort::entries_for_app(app_id),
            0,
            "registry should be empty post-eviction"
        );
    });
}

// ---------------------------------------------------------------------------
// 4 — sync procedure unregisters automatically
// ---------------------------------------------------------------------------

#[test]
fn sync_procedure_unregisters_on_return() {
    let app_id = Uuid::new_v4();
    let rt = build_runtime_with_app(
        app_id,
        r#"
        export function check() {
            return { aborted: __zeroshipGetRpcCtx().signal.aborted };
        }
        "#,
        "{ check }",
    );

    let outcome = start_rpc(&rt, "check", "[]");
    let (status, body) = await_outcome(rt, outcome);
    assert_eq!(status, 200, "check failed: {body}");
    // Fresh controller, never aborted while the procedure ran.
    assert!(body.contains(r#""aborted":false"#), "got: {body}");

    // Drop has run for the AbortGuard — registry must be empty.
    assert_eq!(
        abort::entries_for_app(app_id),
        0,
        "sync procedure left a registry entry"
    );
}

// ---------------------------------------------------------------------------
// 5 — multi-isolate integration smoke: load A, load B (evicts A),
//     verify A's registry was walked.
// ---------------------------------------------------------------------------
//
// The worker's eviction path (cache.rs::evict_lru) runs:
//     1. runtime.with_scope(|scope| abort::entered_for_eviction(scope, app_id))
//     2. cache.isolates.remove(&oldest_id)
//
// We can't import the worker crate (it's a binary) so we replay the
// same shape directly: build app A, register an abort listener, build
// app B (no eviction needed — we just need a second isolate as a
// realistic neighbor on the same thread), then call
// `entered_for_eviction(app_a)`. The listener fires; the registry
// entry for A is gone.

#[test]
fn integration_two_isolates_eviction_walks_correct_registry() {
    let app_a = Uuid::new_v4();
    let app_b = Uuid::new_v4();

    let rt_a = build_runtime_with_app(
        app_a,
        r#"
        globalThis.__zsAbortFired = false;
        export async function pending() {
            __zeroshipGetRpcCtx().signal.addEventListener("abort", () => {
                globalThis.__zsAbortFired = true;
            });
            await new Promise(r => setTimeout(r, 60_000));
        }
        export function readFlag() { return { fired: globalThis.__zsAbortFired }; }
        "#,
        "{ pending, readFlag }",
    );
    rt_a.exit_isolate();

    let rt_b = build_runtime_with_app(
        app_b,
        r#"
        export async function pending() {
            await new Promise(r => setTimeout(r, 60_000));
        }
        "#,
        "{ pending }",
    );
    rt_b.exit_isolate();

    let rt_a_clone = rt_a.clone();
    let rt_b_clone = rt_b.clone();
    compio::runtime::Runtime::new().unwrap().block_on(async move {
        // Start procedures on both isolates. Each call enters its own
        // isolate inside `call_fetch_handler` and exits at the end of
        // the dispatch (so the next call can enter the other isolate).
        rt_a_clone.enter_isolate();
        rt_a_clone.start_pump();
        let _oa = start_rpc(&rt_a_clone, "pending", "[]");
        rt_a_clone.exit_isolate();

        rt_b_clone.enter_isolate();
        rt_b_clone.start_pump();
        let _ob = start_rpc(&rt_b_clone, "pending", "[]");
        rt_b_clone.exit_isolate();

        // Each app contributes one entry under its own key.
        assert_eq!(abort::entries_for_app(app_a), 1);
        assert_eq!(abort::entries_for_app(app_b), 1);

        // Simulate the worker evicting app A. `with_scope` enters
        // app A's isolate just for the registry walk + abort dispatch.
        rt_a_clone.with_scope(|scope| abort::entered_for_eviction(scope, app_a));

        // App A's entries cleared; app B's entries still present
        // (eviction is per-app_id, never wholesale).
        assert_eq!(abort::entries_for_app(app_a), 0);
        assert_eq!(abort::entries_for_app(app_b), 1);

        compio::time::sleep(Duration::ZERO).await;

        // The abort listener fired in A. Dispatch a fresh RPC into A
        // to read the flag (matches what the worker would see if A
        // weren't actually about to be disposed).
        rt_a_clone.enter_isolate();
        let outcome = start_rpc(&rt_a_clone, "readFlag", "[]");
        let (status, body) = match outcome {
            FetchOutcome::Response { status, body, .. } => (status, body),
            FetchOutcome::Pending { rx, cancel: _ } => {
                let settled = compio::time::timeout(Duration::from_secs(2), rx.recv())
                    .await
                    .expect("readFlag timed out")
                    .expect("readFlag error");
                match settled {
                    SettledFetch::Response { status, body, .. } => (status, body),
                    other => panic!("unexpected: {}", std::any::type_name_of_val(&other)),
                }
            }
            other => panic!("unexpected: {}", std::any::type_name_of_val(&other)),
        };
        rt_a_clone.exit_isolate();
        assert_eq!(status, 200, "readFlag failed: {body}");
        assert!(
            body.contains(r#""fired":true"#),
            "app A abort listener didn't fire: {body}"
        );

        // Cleanup: walk B's registry too so the test doesn't leak
        // entries into the next test running on this thread.
        rt_b_clone.with_scope(|scope| abort::entered_for_eviction(scope, app_b));
    });
}
