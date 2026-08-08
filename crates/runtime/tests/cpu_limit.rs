//! CPU-limit termination, which had no test at all until now.
//!
//! `cargo test -p zeroship-runtime cpu` matched zero tests while
//! `check_v8_terminated` - the function BOTH the CPU timer and the heap-limit
//! callback depend on - was being rewritten. The heap route gained coverage in
//! `heap_limits.rs`; this file covers the older route, so a change made for one
//! cannot silently break the other.
//!
//! Linux-only: the CPU timer is a POSIX timer and `RuntimeInner` only carries
//! one under `#[cfg(target_os = "linux")]`. On other targets there is nothing
//! to test, so the test compiles out rather than passing vacuously.

#![cfg(target_os = "linux")]

mod common;
use common::*;

use std::time::Duration;

use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::{init_v8, EnvSnapshot, FetchOutcome, RequestCtx, Runtime, SettledFetch};

/// Drive a dispatch to settlement, mapping a DispatchError to 500. Mirrors the
/// helper in `heap_limits.rs`: a terminated isolate may answer synchronously or
/// through the pending channel, and both are valid.
fn dispatch_against(runtime: &Runtime) -> (u16, String) {
    let env = EnvSnapshot::empty();
    let ctx = RequestCtx::new(CancelFlag::new());
    match runtime.call_fetch_handler("GET", "http://localhost/", &[], "", &env, ctx) {
        FetchOutcome::Response { status, body, .. } => {
            (status, String::from_utf8_lossy(&body).into_owned())
        }
        FetchOutcome::Stream { status, .. } => (status, "<stream>".to_string()),
        FetchOutcome::WebSocketUpgrade { .. } => panic!("unexpected WebSocketUpgrade"),
        FetchOutcome::Pending { rx, cancel: _ } => compio::runtime::Runtime::new()
            .unwrap()
            .block_on(async {
                runtime.start_pump();
                let settled = compio::time::timeout(Duration::from_secs(30), rx.recv())
                    .await
                    .expect("cpu-limited dispatch never settled");
                match settled {
                    Ok(SettledFetch::Response { status, body, .. }) => {
                        (status, String::from_utf8_lossy(&body).into_owned())
                    }
                    Ok(SettledFetch::Stream { status, .. }) => (status, "<stream>".to_string()),
                    Ok(SettledFetch::WebSocketUpgrade { .. }) => {
                        panic!("unexpected WebSocketUpgrade")
                    }
                    Err(e) => (500, format!("DispatchError: {e:?}")),
                }
            }),
    }
}

/// A handler that burns CPU past its limit must be terminated and the request
/// settled with a non-2xx, rather than running to completion or hanging.
///
/// The loop is BOUNDED on purpose, and the bound is the point: it completes on
/// its own in about a second (see the control below), so if the timer never
/// fired this test would see a 200 and fail. An unbounded loop cannot make that
/// distinction - it would hang either way, reporting "cap broken" and "cap
/// works but nothing settles" as the same timeout.
#[test]
fn cpu_limit_terminates_a_runaway_handler() {
    init_v8();
    let modules = m(r#"
        export default {
            fetch(request, env, ctx) {
                // Pure CPU, no allocation: this must trip the CPU timer and not
                // the heap cap, so the two termination routes stay separable.
                let x = 0;
                for (let i = 0; i < 300000000; i++) { x = (x + 1) % 2147483647; }
                return Response.json({ completed: true, x });
            }
        };
    "#);
    let heap_before = zeroship_runtime::heap_limit_callback_hits();
    let rt = Runtime::builder()
        .modules(modules)
        .cpu_limit(Duration::from_millis(200))
        .build();

    let (status, body) = dispatch_against(&rt);
    // WITNESS that the CPU limit is what refused this, not the heap cap. Both
    // routes now end in the same non-2xx through the same function, so status
    // alone cannot tell them apart, and a heap kill here would mean the test
    // is measuring the wrong limit. The loop allocates nothing, so any
    // heap-callback activity at all is a signal that it does.
    //
    // Exact zero is safe ONLY because the counter is process-wide and this
    // binary holds two tests, neither of which allocates. A future test here
    // that does allocate would make this racy against the baseline; move to a
    // per-runtime counter before adding one.
    assert_eq!(
        zeroship_runtime::heap_limit_callback_hits() - heap_before,
        0,
        "the heap callback fired during a pure-CPU handler, so status {status} \
         may be a memory kill rather than the CPU limit this test names",
    );
    assert!(
        !(200..300).contains(&status),
        "a runaway handler under cpu_limit(200ms) returned {status}; body: {body}",
    );
}

// Control for the test above: the SAME bounded loop with no cpu_limit. If this
// returns 200 promptly, the loop completes on its own, so a Pending-that-never-
// settles under cpu_limit is caused by the termination and not by slow JS.
#[test]
fn control_same_loop_without_a_cpu_limit_completes() {
    init_v8();
    let modules = m(r#"
        export default {
            fetch(request, env, ctx) {
                let x = 0;
                for (let i = 0; i < 300000000; i++) { x = (x + 1) % 2147483647; }
                return Response.json({ completed: true, x });
            }
        };
    "#);
    let rt = Runtime::builder().modules(modules).build();
    let (status, body) = dispatch_against(&rt);
    assert_eq!(status, 200, "body: {body}");
    assert!(body.contains(r#""completed":true"#), "body: {body}");
}
