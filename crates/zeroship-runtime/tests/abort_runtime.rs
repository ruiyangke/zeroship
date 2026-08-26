//! `AbortSignal.timeout` against a live `Runtime` — the piece
//! `abort.rs` and `wpt_abort.rs` explicitly can't cover, because
//! neither drives the event loop that fires timers.
//!
//! `abort.rs` runs a bare V8 isolate with no `SharedState` slot, so
//! `timeout_static` (`crates/runtime/src/web/dom/abort_signal.rs`)
//! takes its early return ("no runtime pump -> no timer") and the
//! signal it returns never aborts. `wpt_abort.rs` stubs `async_test`
//! to a no-op for the same reason. Both files say the firing path is
//! covered here instead - this file makes that true.
//!
//! Harness shape copied from `next_tick_ordering.rs`: build a real
//! `Runtime`, `start_pump()` it, drive `call_fetch_handler`, and for
//! a `Pending` outcome await the settled response off the channel
//! compio hands back.

use std::time::Duration;

use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::{
    EnvSnapshot, FetchOutcome, ModuleEntry, RequestCtx, Runtime, SettledFetch,
};

async fn run_js(module_src: &str) -> String {
    let modules = vec![ModuleEntry {
        specifier: "index.js".to_string(),
        source: module_src.to_string(),
    }];
    let runtime = Runtime::builder().modules(modules).build();
    runtime.start_pump();

    let env = EnvSnapshot::empty();
    let ctx = RequestCtx::new(CancelFlag::new());
    let outcome = runtime.call_fetch_handler("GET", "http://localhost/", &[], "", &env, ctx);
    match outcome {
        FetchOutcome::Response { body, .. } => String::from_utf8_lossy(&body).into_owned(),
        FetchOutcome::Pending { rx, .. } => {
            match compio::time::timeout(Duration::from_secs(10), rx.recv()).await {
                Ok(Ok(SettledFetch::Response { body, .. })) => {
                    String::from_utf8_lossy(&body).into_owned()
                }
                // `SettledFetch` / `FetchOutcome` are not `Debug`, so name the
                // case rather than formatting the value.
                Ok(Ok(_)) => panic!("handler settled as a stream or error, expected a Response"),
                Ok(Err(_)) => panic!("settled-fetch channel closed before a Response arrived"),
                Err(_) => panic!("handler did not settle within 10s"),
            }
        }
        _ => panic!("handler neither returned nor pended a Response"),
    }
}

/// 1. `AbortSignal.timeout(n)` actually fires once the event loop has
/// had time to pump: `aborted` flips to `true` and `reason` is a
/// `TimeoutError` DOMException, not just "some" reason.
#[compio::test]
async fn timeout_fires_and_reason_is_timeout_error() {
    let body = run_js(
        r#"
export default {
    async fetch() {
        const sig = AbortSignal.timeout(20);
        // Give the timer well past its 20ms deadline to fire.
        await new Promise((r) => setTimeout(r, 200));
        return new Response(JSON.stringify({
            aborted: sig.aborted,
            reasonIsDOMException: sig.reason instanceof DOMException,
            reasonName: sig.reason ? sig.reason.name : null,
        }));
    }
};
"#,
    )
    .await;

    assert_eq!(
        body,
        r#"{"aborted":true,"reasonIsDOMException":true,"reasonName":"TimeoutError"}"#,
        "AbortSignal.timeout did not fire on a live Runtime - the timer-fired path is broken"
    );
}

/// 2. It does NOT fire early. A signal with a much longer timeout is
/// still unaborted at a point where a short-timeout sibling has
/// already fired - this is the assertion that would catch a signal
/// that aborts immediately (e.g. at construction, or for the wrong
/// reason) rather than genuinely waiting out its deadline.
#[compio::test]
async fn shorter_timeout_fires_before_longer_one_is_still_pending() {
    let body = run_js(
        r#"
export default {
    async fetch() {
        const short = AbortSignal.timeout(20);
        const long = AbortSignal.timeout(5000);
        // Long past `short`'s deadline, nowhere near `long`'s.
        await new Promise((r) => setTimeout(r, 200));
        return new Response(JSON.stringify({
            shortAborted: short.aborted,
            longAborted: long.aborted,
        }));
    }
};
"#,
    )
    .await;

    assert_eq!(
        body,
        r#"{"shortAborted":true,"longAborted":false}"#,
        "a longer AbortSignal.timeout must still be pending while a shorter \
         sibling has already fired - a signal that aborts immediately \
         (construction-time, not deadline-time) would pass a same-signal \
         check but fails this one"
    );
}

/// 3. An `abort` event listener added BEFORE the timer fires is
/// actually invoked when it does, and sees the TimeoutError reason.
#[compio::test]
async fn abort_listener_added_before_firing_is_invoked() {
    let body = run_js(
        r#"
export default {
    async fetch() {
        const sig = AbortSignal.timeout(20);
        let fired = 0;
        let eventType;
        let reasonNameInListener;
        sig.addEventListener("abort", (ev) => {
            fired++;
            eventType = ev.type;
            reasonNameInListener = sig.reason.name;
        });
        await new Promise((r) => setTimeout(r, 200));
        return new Response(JSON.stringify({ fired, eventType, reasonNameInListener }));
    }
};
"#,
    )
    .await;

    assert_eq!(
        body,
        r#"{"fired":1,"eventType":"abort","reasonNameInListener":"TimeoutError"}"#,
        "the abort listener registered before the timer fired was not invoked \
         (or fired the wrong number of times / saw the wrong reason)"
    );
}

/// Bonus: `AbortSignal.any([...])` composed with a live
/// `AbortSignal.timeout` - the dependent signal must transition to
/// aborted when its timed-out source fires, inheriting the
/// TimeoutError reason. This exercises `any_static`'s
/// source/dependent wiring against a REAL timer fire, not just the
/// synchronous `controller.abort()` paths `abort.rs` covers.
#[compio::test]
async fn any_of_a_timeout_signal_aborts_when_the_timeout_fires() {
    let body = run_js(
        r#"
export default {
    async fetch() {
        const t = AbortSignal.timeout(20);
        const combined = AbortSignal.any([t]);
        await new Promise((r) => setTimeout(r, 200));
        return new Response(JSON.stringify({
            combinedAborted: combined.aborted,
            reasonName: combined.reason ? combined.reason.name : null,
        }));
    }
};
"#,
    )
    .await;

    assert_eq!(
        body,
        r#"{"combinedAborted":true,"reasonName":"TimeoutError"}"#,
        "AbortSignal.any([timeoutSignal]) did not observe the live timer fire"
    );
}

// NOT covered here: the GC-retention behaviour described in
// `abort_signal.rs`'s `timeout_static` doc comment (DOM step 3 - a
// pending timeout must pin its signal alive even with no other JS
// references). Forcing a real V8 GC cycle deterministically from a
// test isn't something any existing runtime test does (no
// `--expose-gc` / `request_garbage_collection` wiring is present
// anywhere in this crate to build on), and `SharedState::
// timeout_pinned_signals` already gives the pin unconditionally
// rather than only-while-listening, so there's no reachable failure
// mode to assert against from JS without adding that GC-forcing
// machinery first. Left for a follow-up that wires in V8's
// low-memory-notification / forced-GC hook.
