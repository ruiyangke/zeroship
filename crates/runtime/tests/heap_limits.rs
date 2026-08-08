//! Heap-cap tests for `RuntimeBuilder::heap_limit_mb`.
//!
//! The intended contract: V8's near-heap-limit callback fires before the
//! allocator hard-fails, the runtime grows the cap a few times, and after
//! `MAX_HEAP_LIMIT_HITS` it calls `terminate_execution`. The app is supposed to
//! see a non-2xx, never a success and never a hang.
//!
//! `runtime_heap_cap_enforces_oom` DOES NOT PASS. It is failing on purpose
//! rather than ignored, because the arm it covers is the only one where the cap
//! has to do anything: the other two tests allocate comfortably under their
//! limits, so they would pass against a cap that never fires at all.
//!
//! Two distinct failures were measured against `heap_limit_mb(32)`, and the
//! callback counter separates them into different defects rather than one:
//!
//!   Large-object space is never checked. 400 retained 1 MiB strings return
//!   HTTP 200 with `oom:false`, the process peaks at 229 MB RSS - about 7x the
//!   cap - and the near-heap-limit callback fires ZERO times. V8 does not
//!   consult it for this allocation shape, so nothing in the growth/terminate
//!   logic ever runs. This is the case asserted below.
//!
//!   Regular old space is checked, and the enforcement hangs. The callback
//!   fires exactly `MAX_HEAP_LIMIT_HITS` (5) times, `terminate_execution` is
//!   called as designed, and the dispatch then returns a `Pending` that never
//!   settles - observed at a 30s and a 150s deadline. Termination works; what
//!   is missing is anything that turns a terminated isolate into a response.
//!
//! So the first is a hole in what the cap covers, and the second is a missing
//! completion path after it fires. A single fix will not address both.

mod common;
use common::*;

use std::time::Duration;

use zeroship_runtime::{init_v8, EnvSnapshot, FetchOutcome, RequestCtx, Runtime, SettledFetch};
use zeroship_runtime::channel::CancelFlag;

/// Run a single fetch handler against a freshly-built runtime and return the
/// (status, body) pair.
///
/// This drives `Pending` to settlement instead of rejecting it. A handler that
/// exhausts the heap does NOT come back synchronously: V8 terminates the
/// isolate mid-allocation, the dispatch cannot produce a Response on the spot,
/// and the outcome arrives as `Pending` carrying a `DispatchError`. A sync-only
/// helper reports that as a harness panic, which reads as "the cap is broken"
/// when the cap is in fact the thing that fired.
///
/// A `DispatchError` is mapped to status 500 rather than panicking: for these
/// tests it IS the result under test, not a harness failure.
fn dispatch_against(runtime: &Runtime) -> (u16, String) {
    let env = EnvSnapshot::empty();
    let ctx = RequestCtx::new(CancelFlag::new());
    let outcome = runtime.call_fetch_handler(
        "GET",
        "http://localhost/",
        &[],
        "",
        &env,
        ctx,
    );
    match outcome {
        FetchOutcome::Response { status, body, .. } => {
            (status, String::from_utf8_lossy(&body).into_owned())
        }
        // Name the VARIANT. `type_name_of_val` on the binding prints the enum's
        // own path for every arm, so a mismatch here used to report only
        // "got FetchOutcome" - true of all four and a description of none.
        FetchOutcome::Stream { status, .. } => {
            panic!("expected sync Response, got Stream (status {status})")
        }
        FetchOutcome::Pending { rx, cancel: _ } => compio::runtime::Runtime::new()
            .unwrap()
            .block_on(async {
                runtime.start_pump();
                let settled = compio::time::timeout(Duration::from_secs(30), rx.recv())
                    .await
                    .expect("pending heap-limit dispatch never settled");
                match settled {
                    Ok(SettledFetch::Response { status, body, .. }) => {
                        (status, String::from_utf8_lossy(&body).into_owned())
                    }
                    Ok(SettledFetch::Stream { status, .. }) => {
                        (status, "<stream>".to_string())
                    }
                    Ok(SettledFetch::WebSocketUpgrade { .. }) => {
                        panic!("unexpected WebSocketUpgrade from a heap-limit dispatch")
                    }
                    // The OOM surface. Not a harness failure - the contract
                    // under test is that the app does not get a 2xx.
                    Err(e) => (500, format!("DispatchError: {e:?}")),
                }
            }),
        FetchOutcome::WebSocketUpgrade { ws_id, .. } => {
            panic!("expected sync Response, got WebSocketUpgrade (ws_id {ws_id})")
        }
    }
}

// ---------------------------------------------------------------------------
// 1 — default builder: no per-app cap, just the runtime's 128 MB default.
//     A modest in-heap allocation completes normally.
// ---------------------------------------------------------------------------

#[test]
fn runtime_default_no_heap_cap() {
    init_v8();
    let modules = m(r#"
        export default {
            fetch(request, env, ctx) {
                // ~10 MB of in-heap objects — comfortably under 128 MB default.
                // (Plain JS objects live in the V8 heap, unlike ArrayBuffer
                // backing stores which V8 routes through the array-buffer
                // allocator and excludes from the GC budget.)
                const objs = [];
                for (let i = 0; i < 100_000; i++) {
                    objs.push({ id: i, name: "item-" + i, payload: "x".repeat(64) });
                }
                return Response.json({ count: objs.length });
            }
        };
    "#);
    let rt = Runtime::builder().modules(modules).build();
    let (status, body) = dispatch_against(&rt);
    assert_eq!(status, 200, "body: {body}");
    assert!(body.contains(r#""count":100000"#), "body: {body}");
}

// ---------------------------------------------------------------------------
// 2 — heap_limit_mb(32): allocate well past the cap, expect a non-2xx.
// ---------------------------------------------------------------------------
//
// KNOWN FAILING. The cap does not bound this allocation: see the module header
// for the two measured behaviours and what was not measured.
//
// V8 routes ArrayBuffer backing stores through its array-buffer allocator,
// which is NOT counted against the heap limit configured by
// `CreateParams::heap_limits`. Strings do live in the GC heap - but a 1 MiB
// string exceeds V8's max regular object size, so it lands in large-object
// space, and that is the allocation shape this test shows the cap failing to
// bound.
//
// The allocation is deliberately a few hundred large strings rather than the
// 100M small property stores this test used before. That version could not
// finish inside any reasonable deadline, so the test reported a timeout whose
// message named neither the cap nor the allocation - it looked like a hang in
// the harness rather than a result about the runtime.

#[test]
fn runtime_heap_cap_enforces_oom() {
    init_v8();
    let modules = m(r#"
        export default {
            fetch(request, env, ctx) {
                const live = [];
                try {
                    // 1 MiB in-heap strings, retained. Reaches a 32 MiB cap in
                    // a few dozen iterations instead of 100M property stores.
                    for (let i = 0; i < 400; i++) {
                        live.push("x".repeat(1024 * 1024) + i);
                    }
                } catch (e) {
                    return new Response(JSON.stringify({
                        oom: true,
                        name: e?.name ?? "unknown",
                        message: String(e?.message ?? e),
                        iterations: live.length,
                    }), { status: 500, headers: { "content-type": "application/json" } });
                }
                return Response.json({ oom: false, iterations: live.length });
            }
        };
    "#);
    let before = zeroship_runtime::heap_limit_callback_hits();
    let rt = Runtime::builder().modules(modules).heap_limit_mb(32).build();
    let (status, body) = dispatch_against(&rt);
    let fired = zeroship_runtime::heap_limit_callback_hits() - before;

    // Reported unconditionally, including on the passing path: "the cap held"
    // and "V8 never consulted the cap" produce the same green here, and only
    // this number separates them.
    println!("near-heap-limit callback fired {fired} time(s) during this dispatch");

    // Either:
    //   (a) JS observed the throw → user-handler 500 with `oom:true`.
    //   (b) V8 terminated mid-allocation before JS regained control →
    //       runtime 500 with a terminated/exception message.
    // Both are valid OOM surfaces; the contract is "non-2xx".
    assert!(
        !(200..300).contains(&status),
        "expected non-2xx with heap_limit_mb(32), got {status}; body: {body}",
    );
}

// ---------------------------------------------------------------------------
// 3 — heap_limit_mb(64): typical handler completes normally.
// ---------------------------------------------------------------------------

#[test]
fn runtime_heap_cap_normal_load_passes() {
    init_v8();
    let modules = m(r#"
        export default {
            fetch(request, env, ctx) {
                // Small JSON in / JSON out — well under any reasonable cap.
                const items = [];
                for (let i = 0; i < 100; i++) {
                    items.push({ id: i, name: "item-" + i });
                }
                return Response.json({ count: items.length, first: items[0] });
            }
        };
    "#);
    let rt = Runtime::builder().modules(modules).heap_limit_mb(64).build();
    let (status, body) = dispatch_against(&rt);
    assert_eq!(status, 200, "body: {body}");
    assert!(body.contains(r#""count":100"#), "body: {body}");
}

// Positive control for the counter used above. A zero from a freshly-written
// counter is indistinguishable from a counter that can never increment, so
// this asserts the instrument can move at all before the zero above is read as
// a result about V8.
#[test]
fn near_heap_limit_callback_counter_can_fire() {
    init_v8();
    let modules = m(r#"
        export default {
            fetch(request, env, ctx) {
                const live = [];
                try {
                    // Regular old-space objects, sized to cross a 32 MiB cap
                    // without running away: small unique-keyed property bags.
                    for (let i = 0; i < 3000; i++) {
                        const o = {};
                        for (let k = 0; k < 100; k++) {
                            o["k_" + i + "_" + k] = "v_" + i + "_" + k;
                        }
                        live.push(o);
                    }
                } catch (e) {
                    return Response.json({ oom: true, iterations: live.length });
                }
                return Response.json({ oom: false, iterations: live.length });
            }
        };
    "#);
    let before = zeroship_runtime::heap_limit_callback_hits();
    let rt = Runtime::builder().modules(modules).heap_limit_mb(32).build();
    let (status, body) = dispatch_against(&rt);
    let fired = zeroship_runtime::heap_limit_callback_hits() - before;
    println!("control: status {status}, fired {fired}, body {body}");
    assert!(
        fired > 0,
        "the near-heap-limit counter never incremented even for a regular \
         old-space allocation past the cap, so a zero elsewhere says nothing \
         about V8 - it may just mean this instrument is dead"
    );
}

/// Defect B: heap-limit termination fires as designed and then nothing settles
/// the request.
///
/// This is a REGULAR old-space allocation, so unlike the large-string case
/// above the near-heap-limit callback does run, reaches its hit threshold, and
/// calls `terminate_execution`. The isolate really is terminated; the failure
/// is that no one converts that into a result, so the caller waits forever.
///
/// The assertion is only "settles, non-2xx". Which error surfaces is not
/// pinned - a terminated isolate can be reported as a DispatchError or as a
/// 500, and both satisfy the contract that an app never hangs.
#[test]
fn heap_termination_settles_the_request_instead_of_hanging() {
    init_v8();
    let modules = m(r#"
        export default {
            fetch(request, env, ctx) {
                const live = [];
                for (let i = 0; i < 20000; i++) {
                    const o = {};
                    for (let k = 0; k < 100; k++) { o["k_" + i + "_" + k] = "v_" + i + "_" + k; }
                    live.push(o);
                }
                return Response.json({ oom: false, iterations: live.length });
            }
        };
    "#);
    let before = zeroship_runtime::heap_limit_callback_hits();
    let rt = Runtime::builder().modules(modules).heap_limit_mb(32).build();
    let (status, body) = dispatch_against(&rt);
    let fired = zeroship_runtime::heap_limit_callback_hits() - before;

    // Reported so a future failure can distinguish "termination never fired"
    // from "it fired and the result still did not arrive".
    println!("callback fired {fired} time(s); status {status}");
    assert!(
        fired > 0,
        "this test is meant to exercise the path where termination DOES fire; \
         it fired 0 times, so the allocation no longer trips the cap and the \
         test is measuring something else"
    );
    assert!(
        !(200..300).contains(&status),
        "expected non-2xx after heap termination, got {status}; body: {body}",
    );
}
