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
