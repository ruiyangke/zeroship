//! Regression test for RT-7: LRU eviction must actually drop the isolate.
//!
//! The event-loop pump runs in a detached compio task. If that task holds a
//! *strong* `Rc<RefCell<RuntimeInner>>`, it forms a reference cycle with the
//! cache's `Runtime` handle: the pump loops forever, so when the cache evicts
//! an app (drops its handle), the pump's strong ref keeps `RuntimeInner` — and
//! its V8 isolate — alive. Memory grows; eviction frees nothing.
//!
//! The fix downgrades the pump's back-reference to `Weak` (matching the
//! idle-GC ticker). This test drives the real path — build a runtime, start
//! the real pump, then drop the handle (simulating cache eviction) — and
//! asserts the inner `Rc` strong count falls to zero.
//!
//! Pre-fix this FAILS: the pump's strong ref pins the count at >= 1 forever.

use std::time::Duration;

use zeroship_runtime::init_v8;
use zeroship_runtime::runtime::Runtime;
use zeroship_runtime::ModuleEntry;

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

/// The COMPLEMENT of the test below, and the mechanism under e2e scenario 22
/// (the creator redeploys while end users are mid-request).
///
/// `worker/src/cache.rs` keys isolates by app_id ALONE, so a redeploy replaces
/// the entry an in-flight request is running on, and `evict_app` does an
/// unconditional `remove` with no lease check -- unlike `evict_lru`, which has
/// `evict_lru_never_evicts_leased_isolate`. That asymmetry reads as a defect.
/// It is not one, and this test is what says so by RUNNING rather than by
/// reading the types: `cache::get_runtime` hands out a CLONE, so a dispatch in
/// flight holds its own strong `Rc`. Dropping the cache's handle must therefore
/// leave the isolate alive until the request finishes.
///
/// TWO ARMS, differing in ONE variable -- whether a second handle exists:
///   A. cache handle dropped, in-flight handle alive  -> strong_count >= 1
///   B. then the in-flight handle dropped too         -> strong_count == 0
/// Arm B is the control. Without it, arm A would also pass if the probe were
/// simply incapable of reaching 0 here (a leaked pump would do exactly that),
/// which is the failure the sibling test below was written for.
///
/// WHAT THIS DOES NOT CATCH: it proves the isolate OUTLIVES the eviction, not
/// that the end user receives a correct response. Whether an in-flight request
/// completes cleanly through gateway and worker across a real redeploy is
/// scenario 22 and is still unwalked; nothing here measures a status code.
#[test]
fn eviction_with_live_request_handle_keeps_isolate_alive() {
    init_v8();
    compio::runtime::Runtime::new().unwrap().block_on(async {
        let rt = Runtime::builder()
            .modules(trivial_module())
            .idle_gc_after_ms(0)
            .build();
        rt.exit_isolate();
        rt.start_pump();
        compio::time::sleep(Duration::from_millis(20)).await;

        // The handle an in-flight dispatch holds. `cache::get_runtime` returns
        // exactly this: a clone, not a borrow of the map entry.
        let in_flight = rt.clone();

        // The cache drops ITS handle. This is `evict_app`'s unconditional
        // `isolates.remove(app_id)` on the redeploy path.
        let probe = rt.into_inner_probe_for_test();

        // ARM A. Give the pump every chance to observe the drop and tear down,
        // the same window the sibling test uses to observe the opposite. If the
        // isolate died here, an in-flight request would be running on freed
        // state.
        for _ in 0..20 {
            compio::time::sleep(Duration::from_millis(10)).await;
        }
        let during = probe.strong_count();
        assert!(
            during >= 1,
            "isolate died while a request handle was still live: \
             strong_count = {during} (a redeploy would cut in-flight requests)"
        );

        // ARM B, the control: the request finishes and drops its handle. Now
        // nothing holds the isolate and the count must reach 0 -- proving arm
        // A measured the extra handle rather than an unfalsifiable probe.
        drop(in_flight);
        let mut after = probe.strong_count();
        for _ in 0..50 {
            if after == 0 {
                break;
            }
            compio::time::sleep(Duration::from_millis(10)).await;
            after = probe.strong_count();
        }
        assert_eq!(
            after, 0,
            "isolate leaked after the last handle dropped: strong_count = {after}"
        );
    });
}

#[test]
fn eviction_drops_isolate_after_pump_started() {
    init_v8();
    compio::runtime::Runtime::new().unwrap().block_on(async {
        // Build a runtime and start the REAL pump task (detached compio task
        // that loops on the runtime). Opt out of the idle-GC ticker so the
        // only background holder under test is the pump itself.
        let rt = Runtime::builder()
            .modules(trivial_module())
            .idle_gc_after_ms(0)
            .build();
        rt.exit_isolate();
        rt.start_pump();

        // Give the pump task a scheduling slot so it actually runs its first
        // loop iteration (upgrade -> drain -> await). This proves the pump is
        // live and parked on `notify_rx`, not merely spawned.
        compio::time::sleep(Duration::from_millis(20)).await;

        // Simulate LRU eviction: the cache drops the `Runtime` handle. We
        // convert it into a Weak-backed probe first so dropping the handle
        // releases the ONLY strong ref a correct ownership graph should have.
        let probe = rt.into_inner_probe_for_test();

        // The handle's strong ref is gone. If the pump holds a Weak (fixed),
        // the inner drops immediately -> count 0. If the pump holds a strong
        // Rc (pre-fix), the count stays >= 1 forever.
        //
        // Wake the pump a few times and yield so it has every chance to notice
        // the drop and exit; a correct pump exits on the next `upgrade()` ==
        // None. We poll the count over a short window to avoid racing the
        // scheduler.
        let mut count = probe.strong_count();
        for _ in 0..50 {
            if count == 0 {
                break;
            }
            compio::time::sleep(Duration::from_millis(10)).await;
            count = probe.strong_count();
        }

        assert_eq!(
            count, 0,
            "RuntimeInner leaked after eviction: strong_count = {count} \
             (the pump task is holding a strong Rc and pinning the isolate)"
        );
    });
}
