//! Heap-cap tests for `RuntimeBuilder::heap_limit_mb` (TODO.md lever 1).
//!
//! V8's near-heap-limit callback fires before the allocator hard-fails;
//! after the runtime's hit-counter exceeds its threshold, the isolate
//! is terminated. The user-visible signal at the dispatch boundary is
//! a 500 Response whose body carries a V8-flavoured error message
//! (typically a `RangeError` — "Array buffer allocation failed",
//! "Invalid string length", etc.).
//!
//! These tests don't try to assert the exact message string — V8 picks
//! it based on which allocation site lost first. Instead they assert
//! the high-level user contract:
//!   1. unset → V8 default → 100 MB array stringifies fine.
//!   2. set & exceeded → 500 with an error indicator.
//!   3. set & not exceeded → handler runs to completion.

mod common;
use common::*;

use zeroship_runtime::{init_v8, EnvSnapshot, FetchOutcome, RequestCtx, Runtime};
use zeroship_runtime::channel::CancelFlag;

/// Run a single fetch handler against a freshly-built runtime and
/// return the (status, body) pair. Sync path only — heap allocation
/// inside `default.fetch` is synchronous, no pump needed.
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
        other => panic!(
            "expected sync Response, got {}",
            std::any::type_name_of_val(&other),
        ),
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
// V8 routes ArrayBuffer backing stores through its array-buffer allocator,
// which is NOT counted against the heap limit configured by
// `CreateParams::heap_limits`. Only allocations that live in the GC heap
// (plain JS objects, strings, closures, retained property tables) trip
// the near-heap-limit callback. The test grows a retained array of
// long strings — each `repeat(...)` produces a fresh in-heap string,
// and the outer Array keeps them alive so GC can't reclaim.

#[test]
fn runtime_heap_cap_enforces_oom() {
    init_v8();
    let modules = m(r#"
        export default {
            fetch(request, env, ctx) {
                const live = [];
                try {
                    // Push retained objects with many unique-keyed
                    // properties. Plain JS objects (and their hidden-
                    // class transitions / property tables) are old-gen
                    // heap-resident. Each iteration adds ~120 KB of
                    // retained heap; 100k iterations → ~12 GB target,
                    // well past any reasonable cap. The bound caps
                    // the loop so a misconfigured test fails loudly
                    // rather than hangs.
                    for (let i = 0; i < 100_000; i++) {
                        const o = {};
                        for (let k = 0; k < 1000; k++) {
                            // Unique key per (i, k) → V8 can't intern.
                            o["k_" + i + "_" + k] = "v_" + i + "_" + k;
                        }
                        live.push(o);
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
    let rt = Runtime::builder().modules(modules).heap_limit_mb(32).build();
    let (status, body) = dispatch_against(&rt);

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
