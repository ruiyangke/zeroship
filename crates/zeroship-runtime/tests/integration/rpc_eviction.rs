//! Eviction-time abort tests for RPC.
//!
//! Covers the per-runtime `AbortRegistry` from
//! `crates/zeroship-runtime/src/rpc/abort.rs`: register-on-dispatch, fire-on-
//! eviction, automatic unregister via Drop, and the worker-style
//! integration smoke (1-slot LRU evicting app A when app B loads).

use crate::support;

use std::collections::HashMap;
use std::time::Duration;

use zeroship_core::app_id::AppId;

use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::runtime::Runtime;
use zeroship_runtime::{init_v8, EnvSnapshot, FetchOutcome, RequestCtx, SettledFetch};

use support::wrap_with_synthetic_entry;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a Runtime with `app_id` set + a tiny `default.{fetch,rpc}`
/// shim that exposes named exports of `user_source`. Mirrors
/// `support::wrap_with_synthetic_entry` but binds an `app_id`.
fn build_runtime_with_app(app_id: &AppId, user_source: &str, procs_block: &str) -> Runtime {
    init_v8();
    let modules = wrap_with_synthetic_entry(user_source, procs_block);
    Runtime::builder()
        .modules(modules)
        .env_vars(HashMap::new())
        .app_id(app_id.clone())
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
fn await_outcome(runtime: &Runtime, outcome: FetchOutcome) -> (u16, String) {
    if let FetchOutcome::Response { status, body, .. } = &outcome {
        return (*status, String::from_utf8_lossy(body).into_owned());
    }
    compio::runtime::Runtime::new().unwrap().block_on(async {
        runtime.start_pump();
        match outcome {
            FetchOutcome::Response { status, body, .. } => {
                (status, String::from_utf8_lossy(&body).into_owned())
            }
            FetchOutcome::Pending { rx, cancel: _ } => {
                let settled = compio::time::timeout(Duration::from_secs(5), rx.recv())
                    .await
                    .expect("pending timed out")
                    .expect("pending delivered DispatchError");
                match settled {
                    SettledFetch::Response { status, body, .. } => {
                        (status, String::from_utf8_lossy(&body).into_owned())
                    }
                    other => panic!("unexpected settle: {:?}", std::any::type_name_of_val(&other)),
                }
            }
            other => panic!("unexpected outcome: {:?}", std::any::type_name_of_val(&other)),
        }
    })
}

/// Dispatch the synthetic `readFlag` procedure and return `(status, body)`.
/// The caller must have entered the runtime's isolate and started its pump.
async fn read_flag(runtime: &Runtime) -> (u16, String) {
    match start_rpc(runtime, "readFlag", "[]") {
        FetchOutcome::Response { status, body, .. } => {
            (status, String::from_utf8_lossy(&body).into_owned())
        }
        FetchOutcome::Pending { rx, cancel: _ } => {
            let settled = compio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .expect("readFlag timed out")
                .expect("readFlag dispatch error");
            match settled {
                SettledFetch::Response { status, body, .. } => {
                    (status, String::from_utf8_lossy(&body).into_owned())
                }
                other => panic!("unexpected settle: {:?}", std::any::type_name_of_val(&other)),
            }
        }
        other => panic!("unexpected outcome: {:?}", std::any::type_name_of_val(&other)),
    }
}

// ---------------------------------------------------------------------------
// 1 — single in-flight controller fires on eviction
// ---------------------------------------------------------------------------

#[test]
fn eviction_fires_single_inflight_controller() {
    let app_id = AppId::mint();

    // Procedure: install an `abort` listener that flips a global flag,
    // then await a long timeout that won't resolve before the test
    // ends. The eviction sweep fires the controller; the listener
    // observes it and sets `globalThis.__zsAbortFired = true`.
    let rt = build_runtime_with_app(
        &app_id,
        r#"
        import { currentSignal } from "zeroship";
        export async function pending() {
            currentSignal().addEventListener("abort", () => {
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

        // The runtime's registry must hold exactly one entry.
        assert_eq!(rt_clone.abort_registry_len(), 1, "registry should hold one entry");

        // Fire eviction. After this, the controller's signal-abort
        // algorithm has run synchronously — the listener fires inside
        // `entered_for_eviction`'s scope.
        rt_clone.entered_for_eviction();

        // Yield once so any microtasks queued by the abort listener
        // get a chance to run before we check the flag. The abort
        // listener body is synchronous so a single zero-sleep is
        // sufficient.
        compio::time::sleep(Duration::ZERO).await;

        // Now run a sync probe through the same isolate to check the
        // global flag. The probe registers a NEW request, but the
        // `pending` request's controller has already been aborted +
        // cleared from the registry.
        let (status, body) = read_flag(&rt_clone).await;
        assert_eq!(status, 200, "readFlag failed: {body}");
        assert!(body.contains(r#""fired":true"#), "abort listener didn't fire: {body}");
    });
}

// ---------------------------------------------------------------------------
// 2 — multiple in-flight controllers all fire
// ---------------------------------------------------------------------------

#[test]
fn eviction_fires_all_inflight_controllers() {
    let app_id = AppId::mint();
    let rt = build_runtime_with_app(
        &app_id,
        r#"
        import { currentSignal } from "zeroship";
        globalThis.__zsAbortCount = 0;
        export async function p1() {
            currentSignal().addEventListener("abort", () => {
                globalThis.__zsAbortCount += 1;
            });
            await new Promise(r => setTimeout(r, 60_000));
        }
        export async function p2() {
            currentSignal().addEventListener("abort", () => {
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
        assert_eq!(rt_clone.abort_registry_len(), 2);

        rt_clone.entered_for_eviction();
        compio::time::sleep(Duration::ZERO).await;

        let outcome = start_rpc(&rt_clone, "readCount", "[]");
        let (status, body) = match outcome {
            FetchOutcome::Response { status, body, .. } => {
                (status, String::from_utf8_lossy(&body).into_owned())
            }
            FetchOutcome::Pending { rx, cancel: _ } => {
                let settled = compio::time::timeout(Duration::from_secs(2), rx.recv())
                    .await
                    .expect("readCount timed out")
                    .expect("readCount dispatch error");
                match settled {
                    SettledFetch::Response { status, body, .. } => {
                        (status, String::from_utf8_lossy(&body).into_owned())
                    }
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
    let app_id = AppId::mint();
    let rt = build_runtime_with_app(
        &app_id,
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
        assert_eq!(rt_clone.abort_registry_len(), 1);

        rt_clone.entered_for_eviction();
        assert_eq!(
            rt_clone.abort_registry_len(),
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
    let app_id = AppId::mint();
    let rt = build_runtime_with_app(
        &app_id,
        r#"
        import { currentSignal } from "zeroship";
        export function check() {
            return { aborted: currentSignal().aborted };
        }
        "#,
        "{ check }",
    );

    let outcome = start_rpc(&rt, "check", "[]");
    let (status, body) = await_outcome(&rt, outcome);
    assert_eq!(status, 200, "check failed: {body}");
    // Fresh controller, never aborted while the procedure ran.
    assert!(body.contains(r#""aborted":false"#), "got: {body}");

    // Drop has run for the AbortGuard — registry must be empty.
    assert_eq!(
        rt.abort_registry_len(),
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
//     1. runtime.entered_for_eviction()
//     2. cache.isolates.remove(&oldest_id)
//
// We can't import the worker crate (it's a binary) so we replay the
// same shape directly: build app A, register an abort listener, build
// app B (no eviction needed — we just need a second isolate as a
// realistic neighbor on the same thread), then call
// `entered_for_eviction()`. The listener fires; the registry
// entry for A is gone.

#[test]
fn integration_two_isolates_eviction_walks_correct_registry() {
    let app_a = AppId::mint();
    let app_b = AppId::mint();

    let rt_a = build_runtime_with_app(
        &app_a,
        r#"
        import { currentSignal } from "zeroship";
        globalThis.__zsAbortFired = false;
        export async function pending() {
            currentSignal().addEventListener("abort", () => {
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
        &app_b,
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

        // Each runtime contributes its own entry to its own registry.
        assert_eq!(rt_a_clone.abort_registry_len(), 1);
        assert_eq!(rt_b_clone.abort_registry_len(), 1);

        // Simulate the worker evicting app A. `entered_for_eviction`
        // enters app A's isolate for the abort dispatch and walks only
        // app A's runtime registry.
        rt_a_clone.entered_for_eviction();

        // App A's entries cleared; app B's entries still present
        // (the registry is per-runtime, never shared).
        assert_eq!(rt_a_clone.abort_registry_len(), 0);
        assert_eq!(rt_b_clone.abort_registry_len(), 1);

        compio::time::sleep(Duration::ZERO).await;

        // The abort listener fired in A. Dispatch a fresh RPC into A
        // to read the flag (matches what the worker would see if A
        // weren't actually about to be disposed).
        rt_a_clone.enter_isolate();
        let (status, body) = read_flag(&rt_a_clone).await;
        rt_a_clone.exit_isolate();
        assert_eq!(status, 200, "readFlag failed: {body}");
        assert!(
            body.contains(r#""fired":true"#),
            "app A abort listener didn't fire: {body}"
        );

        // Cleanup: abort B's still-pending request. B's registry is owned
        // by B's runtime, so nothing outlives this test.
        rt_b_clone.entered_for_eviction();
    });
}

// ---------------------------------------------------------------------------
// 6: two runtimes of the SAME app id on one thread keep independent
//     in-flight registries. Each runtime mints request id 1; evicting one
//     aborts only its own signal and leaves the other abortable.
// ---------------------------------------------------------------------------

const SAME_APP_FLAG_SOURCE: &str = r#"
    import { currentSignal } from "zeroship";
    globalThis.__zsAbortFired = false;
    export async function pending() {
        currentSignal().addEventListener("abort", () => {
            globalThis.__zsAbortFired = true;
        });
        await new Promise(r => setTimeout(r, 60_000));
    }
    export function readFlag() { return { fired: globalThis.__zsAbortFired }; }
"#;

#[test]
fn same_app_runtimes_keep_independent_in_flight_registries() {
    let app_id = AppId::mint();

    let rt_one = build_runtime_with_app(&app_id, SAME_APP_FLAG_SOURCE, "{ pending, readFlag }");
    rt_one.exit_isolate();
    let rt_two = build_runtime_with_app(&app_id, SAME_APP_FLAG_SOURCE, "{ pending, readFlag }");
    rt_two.exit_isolate();

    let one = rt_one.clone();
    let two = rt_two.clone();
    compio::runtime::Runtime::new().unwrap().block_on(async move {
        one.enter_isolate();
        one.start_pump();
        let _pending_one = start_rpc(&one, "pending", "[]");
        one.exit_isolate();

        two.enter_isolate();
        two.start_pump();
        let _pending_two = start_rpc(&two, "pending", "[]");
        two.exit_isolate();

        // Both runtimes minted request id 1. Each runtime owns its own
        // registry, so both entries exist at once.
        assert_eq!(one.abort_registry_len(), 1, "runtime one registry");
        assert_eq!(two.abort_registry_len(), 1, "runtime two registry");

        // Evict runtime one. Only its own signal is aborted and removed.
        one.entered_for_eviction();
        assert_eq!(one.abort_registry_len(), 0, "evicted runtime one cleared");
        assert_eq!(
            two.abort_registry_len(),
            1,
            "runtime two signal must survive runtime one's eviction"
        );

        one.enter_isolate();
        let (status, body) = read_flag(&one).await;
        one.exit_isolate();
        assert_eq!(status, 200, "runtime one readFlag failed: {body}");
        assert!(
            body.contains(r#""fired":true"#),
            "runtime one listener did not fire: {body}"
        );

        // Control: runtime two's signal has not fired under runtime one's
        // eviction.
        two.enter_isolate();
        let (status, body) = read_flag(&two).await;
        two.exit_isolate();
        assert_eq!(status, 200, "runtime two readFlag failed: {body}");
        assert!(
            body.contains(r#""fired":false"#),
            "runtime two listener fired before its own eviction: {body}"
        );

        // Runtime two is still registered and abortable on its own.
        two.entered_for_eviction();
        assert_eq!(two.abort_registry_len(), 0, "evicted runtime two cleared");

        two.enter_isolate();
        let (status, body) = read_flag(&two).await;
        two.exit_isolate();
        assert_eq!(status, 200, "runtime two readFlag failed: {body}");
        assert!(
            body.contains(r#""fired":true"#),
            "runtime two listener did not fire: {body}"
        );
    });
}

// ---------------------------------------------------------------------------
// 7: evicting the newer runtime while the older runtime of the same app id
//     still holds an in-flight request must not touch the older signal.
//
// The older runtime consumes request id 1 with a sync call, then holds id 2;
// the newer runtime holds its first request, id 1. A registry shared across
// runtimes on one thread and keyed by app id would evict the newer runtime by
// walking the older runtime's entry, converting its `v8::Global` under the
// newer isolate's scope, which asserts in the handle conversion.
// ---------------------------------------------------------------------------

#[test]
fn evicting_newer_same_app_runtime_leaves_older_signal_untouched() {
    let app_id = AppId::mint();

    let source = r#"
        import { currentSignal } from "zeroship";
        globalThis.__zsAbortFired = false;
        export function quick() { return { ok: true }; }
        export async function pending() {
            currentSignal().addEventListener("abort", () => {
                globalThis.__zsAbortFired = true;
            });
            await new Promise(r => setTimeout(r, 60_000));
        }
        export function readFlag() { return { fired: globalThis.__zsAbortFired }; }
    "#;

    let rt_older = build_runtime_with_app(&app_id, source, "{ quick, pending, readFlag }");
    rt_older.exit_isolate();
    let rt_newer = build_runtime_with_app(&app_id, source, "{ quick, pending, readFlag }");
    rt_newer.exit_isolate();

    let older = rt_older.clone();
    let newer = rt_newer.clone();
    compio::runtime::Runtime::new().unwrap().block_on(async move {
        older.enter_isolate();
        older.start_pump();
        let _ = start_rpc(&older, "quick", "[]");
        let _pending_older = start_rpc(&older, "pending", "[]");
        older.exit_isolate();

        newer.enter_isolate();
        newer.start_pump();
        let _pending_newer = start_rpc(&newer, "pending", "[]");
        newer.exit_isolate();

        // Evict the newer runtime. Its registry walk must not reach the
        // older runtime's signal: no cross-isolate handle conversion.
        newer.entered_for_eviction();
        assert_eq!(newer.abort_registry_len(), 0, "newer registry cleared");
        assert_eq!(
            older.abort_registry_len(),
            1,
            "older signal must remain registered"
        );

        older.enter_isolate();
        let (status, body) = read_flag(&older).await;
        older.exit_isolate();
        assert_eq!(status, 200, "older readFlag failed: {body}");
        assert!(
            body.contains(r#""fired":false"#),
            "older signal was aborted by the newer runtime's eviction: {body}"
        );

        // Cleanup: abort the older runtime's still-pending request and
        // observe that this runtime's own eviction does fire its listener.
        older.entered_for_eviction();
        assert_eq!(older.abort_registry_len(), 0, "older registry cleared");

        older.enter_isolate();
        let (status, body) = read_flag(&older).await;
        older.exit_isolate();
        assert_eq!(status, 200, "older readFlag failed after eviction: {body}");
        assert!(
            body.contains(r#""fired":true"#),
            "older signal did not fire on its own eviction: {body}"
        );
    });
}

// ---------------------------------------------------------------------------
// 8: a runtime with no app id still registers and aborts its in-flight
//    signal on eviction. The per-runtime registry needs no identity.
// ---------------------------------------------------------------------------

/// Like [`build_runtime_with_app`] but does not bind an app id.
fn build_runtime_without_app(user_source: &str, procs_block: &str) -> Runtime {
    init_v8();
    let modules = wrap_with_synthetic_entry(user_source, procs_block);
    Runtime::builder()
        .modules(modules)
        .env_vars(HashMap::new())
        .build()
}

#[test]
fn runtime_without_app_id_still_aborts_in_flight_signal_on_eviction() {
    let rt = build_runtime_without_app(SAME_APP_FLAG_SOURCE, "{ pending, readFlag }");
    let rt_clone = rt.clone();
    compio::runtime::Runtime::new().unwrap().block_on(async move {
        rt_clone.start_pump();
        let outcome = start_rpc(&rt_clone, "pending", "[]");
        assert!(
            matches!(outcome, FetchOutcome::Pending { .. }),
            "expected Pending, got {:?}",
            std::any::type_name_of_val(&outcome)
        );
        assert_eq!(
            rt_clone.abort_registry_len(),
            1,
            "unidentified runtime must register its in-flight signal"
        );

        rt_clone.entered_for_eviction();
        assert_eq!(
            rt_clone.abort_registry_len(),
            0,
            "unidentified runtime's registry cleared"
        );
        compio::time::sleep(Duration::ZERO).await;

        let (status, body) = read_flag(&rt_clone).await;
        assert_eq!(status, 200, "readFlag failed: {body}");
        assert!(
            body.contains(r#""fired":true"#),
            "unidentified runtime's listener did not fire: {body}"
        );
    });
}
