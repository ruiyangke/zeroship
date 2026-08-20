//! Heap-cap tests for `RuntimeBuilder::heap_limit_mb`.
//!
//! The intended contract: V8's near-heap-limit callback fires before the
//! allocator hard-fails, the runtime grows the cap a few times, and after
//! `MAX_HEAP_LIMIT_HITS` it calls `terminate_execution`. The app is supposed to
//! see a non-2xx, never a success and never a hang.
//!
//! ## An allocation that is never read is never allocated
//!
//! `runtime_heap_cap_enforces_oom` spent a while failing while I recorded a
//! defect that does not exist: "the cap does not cover large-object space,
//! because 400 retained 1 MiB strings return 200 and the callback fires zero
//! times". Both observations were real. The conclusion was wrong.
//!
//! Building `"x".repeat(1024 * 1024) + i` does not consume heap. V8 leaves the
//! value unmaterialised until something reads it, so the loop retained 400
//! nominal megabytes while `used_heap_size` sat at 1.5 MB and the near-heap-
//! limit callback had nothing to fire about. Adding one `charCodeAt` per
//! iteration takes the SAME loop to 58 MB used, five callback hits, and a 503.
//!
//! So the cap does cover these allocations, and the test was asserting against
//! a no-op. The lesson worth keeping is that a test which allocates must prove
//! it allocated - `used_heap_size` is the check, not the size of the values the
//! source appears to build.
//!
//! What was real, and is fixed: heap-limit termination fired correctly and then
//! left the request hanging, because nothing converted a terminated isolate
//! into a response. The callback hit `MAX_HEAP_LIMIT_HITS` (5), called
//! `terminate_execution` as designed, and the dispatch returned a `Pending`
//! that never settled at a 30s or a 150s deadline.
//!
//! ## Still worth an operator decision, by design rather than by defect
//!
//! `heap_limit_mb(32)` does not cap the isolate at 32 MB. The callback grows
//! the limit by a quarter of the original on each hit, up to 4x, so the
//! observed ceiling is 128 MB and a dispatch was measured at 72 MB. That is
//! deliberate - it avoids a hard V8 fatal-abort - but it means the configured
//! number is a floor that buys headroom, not a bound.

use crate::common;
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
// The cap DOES bound this allocation, once the allocation is real: see the
// module header for why an earlier version of this test measured otherwise.
//
// V8 routes ArrayBuffer backing stores through its array-buffer allocator,
// which is NOT counted against the heap limit configured by
// `CreateParams::heap_limits`, so a test that allocates buffers would pass
// against a cap that never fires. Strings live in the GC heap and do count,
// which is why the allocation below is strings - provided each one is read,
// per the module header.
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
                    // 1 MiB strings, retained. Building them is NOT enough:
                    // until something reads one, V8 leaves the value
                    // unmaterialised and the heap is never actually consumed -
                    // measured at 1.5 MB used after 400 iterations, with the
                    // near-heap-limit callback firing zero times. Touching a
                    // byte forces materialisation and the same loop reaches
                    // 58 MB used with the callback firing its full five times.
                    let sink = 0;
                    for (let i = 0; i < 400; i++) {
                        const s = "x".repeat(1024 * 1024) + i;
                        sink += s.charCodeAt(s.length - 2);
                        live.push(s);
                    }
                    if (sink < 0) { throw new Error("unreachable"); }
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

    // WITNESS, asserted before the outcome: the heap cap is what refused this
    // request. A non-2xx alone is satisfied by any failure - a CPU kill, a
    // module error, a panic mapped to 500 - none of which exercise the cap.
    // This test previously "failed" for a reason unrelated to the cap and I
    // read the failure as a cap defect, so the same confusion in the passing
    // direction is exactly what needs closing off.
    assert!(
        fired > 0,
        "the near-heap-limit callback never fired, so whatever produced status \
         {status} was not the heap cap and this test did not exercise it; \
         body: {body}",
    );

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
