//! Tests for the per-isolate idle-GC ticker (TODO.md "Memory footprint"
//! lever 4). The ticker watches `last_request_ts`; once the runtime has
//! been quiet for `RuntimeBuilder::idle_gc_after_ms`, it enters V8 and
//! fires `low_memory_notification` to reclaim the high-water-mark heap.
//!
//! The fire-count counter on `Runtime` is the test-visible signal.
//! Sampling V8 heap stats before/after is too noisy on a small heap.

use std::time::Duration;

use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::runtime::{Runtime, DEFAULT_IDLE_GC_AFTER};
use zeroship_runtime::{init_v8, EnvSnapshot, ModuleEntry, RequestCtx};

mod common;

/// Minimal `default.fetch` so `call_fetch_handler` is non-pending.
fn trivial_module() -> Vec<ModuleEntry> {
    vec![ModuleEntry {
        specifier: "index.js".into(),
        source: r#"
            export default {
                fetch(req, env, ctx) {
                    return new Response("ok", { status: 200 });
                },
            };
        "#
        .into(),
    }]
}

fn one_request(rt: &Runtime) {
    let env = EnvSnapshot::empty();
    let ctx = RequestCtx::new(CancelFlag::new());
    let _ = rt.call_fetch_handler(
        "GET",
        "http://localhost/",
        &[],
        "",
        &env,
        ctx,
    );
}

#[test]
fn idle_gc_fires_after_threshold() {
    init_v8();
    compio::runtime::Runtime::new().unwrap().block_on(async {
        // 50 ms threshold — `idle_gc_ticker` ticks at min(IDLE_GC_TICK,
        // idle_gc_after) == 50 ms here, so the very first wake hits
        // the deadline.
        let rt = Runtime::builder()
            .modules(trivial_module())
            .idle_gc_after_ms(50)
            .build();
        rt.start_pump();
        one_request(&rt);

        // Quiet window: 350 ms — gives the ticker (~50 ms cadence) at
        // least three chances to fire even on a slow CI box.
        compio::time::sleep(Duration::from_millis(350)).await;

        let fires = rt.idle_gc_fire_count();
        assert!(fires >= 1, "expected idle GC to fire at least once, got {fires}");
    });
}

#[test]
fn request_resets_idle_clock() {
    init_v8();
    compio::runtime::Runtime::new().unwrap().block_on(async {
        // 200 ms threshold; we kick a request every 30 ms for 250 ms
        // (≈ 8 requests). Each request resets `last_request_ts` so
        // elapsed never reaches 200 ms — counter must stay at 0.
        let rt = Runtime::builder()
            .modules(trivial_module())
            .idle_gc_after_ms(200)
            .build();
        rt.start_pump();

        for _ in 0..8 {
            one_request(&rt);
            compio::time::sleep(Duration::from_millis(30)).await;
        }

        let fires = rt.idle_gc_fire_count();
        assert_eq!(fires, 0, "idle GC fired during steady traffic: {fires}");
    });
}

#[test]
fn default_threshold_present() {
    // No-arg builder must wire the documented default. Don't sleep
    // 30 s — just confirm the constant via the public re-export.
    init_v8();
    assert_eq!(DEFAULT_IDLE_GC_AFTER, Duration::from_millis(30_000));

    // And confirm the ticker is opt-out-able via 0 (used by tests
    // that don't want the timer threading at all). Building with 0
    // must not panic and must produce a working Runtime.
    let rt = Runtime::builder()
        .modules(trivial_module())
        .idle_gc_after_ms(0)
        .build();
    // Counter starts at zero either way.
    assert_eq!(rt.idle_gc_fire_count(), 0);
}
