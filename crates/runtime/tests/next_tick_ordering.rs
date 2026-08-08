//! What `process.nextTick` orders against promise microtasks, in the two
//! arrangements that behave differently.
//!
//! `node_pg_e2e.rs` asserted `["tick","promise"]` from inside an async
//! handler's prefix and was RED on HEAD. Separating the arrangements shows
//! why, and the reference values below were MEASURED against node v22.22.2
//! rather than reasoned from the docs - reasoning got it wrong twice.
//!
//! Node, measured:
//!
//!   CommonJS, synchronous top level      ["tick","promise"]
//!   ESM, top level                       ["promise","tick"]
//!   inside a microtask (either module)   ["promise","tick"]
//!
//! ESM top level is NOT a synchronous context: module evaluation is itself
//! driven from a job, so a tick queued there lands after the pending
//! promise queue. Only CommonJS gives the textbook "nextTick runs first".
//!
//! This runtime, measured by the two tests below:
//!
//!   ESM top level                        ["tick","promise"]   <- diverges
//!   inside a microtask                   ["promise","tick"]   <- matches
//!
//! So the in-microtask case, the one the pg e2e actually constructs, does
//! NOT diverge from Node. The real divergence is at ESM top level, where
//! this runtime behaves like Node's CommonJS.
//!
//! These tests assert the MEASURED behaviour, not the wished-for one, so a
//! change to the drain order fails here with the arrangement named instead
//! of as one ambiguous assertion buried in a 200-line database e2e.
//!
//! See `core/init.rs` (`perform_microtask_checkpoint`) for the drain shape.

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

/// Arrangement A: both queued from ESM module top level.
///
/// This runtime drains the tick queue first here. Node ESM does NOT
/// (measured: ["promise","tick"]); only Node CommonJS does. Asserting our
/// behaviour so the divergence is pinned and visible rather than implied.
#[compio::test]
async fn tick_queued_at_esm_top_level_runs_before_promise_microtasks_unlike_node_esm() {
    let body = run_js(
        r#"
const ordering = [];
Promise.resolve().then(() => ordering.push("promise"));
process.nextTick(() => ordering.push("tick"));
export default {
    async fetch() {
        return new Response(JSON.stringify(ordering));
    }
};
"#,
    )
    .await;

    assert_eq!(
        body, r#"["tick","promise"]"#,
        "this runtime drains a top-level nextTick before the promise queue; \
         node v22 ESM gives [\"promise\",\"tick\"] here, so this is a MEASURED \
         divergence, not a match"
    );
}

/// Arrangement B: both queued from inside a microtask.
///
/// The handler's synchronous prefix is itself a promise job, so the
/// pre-drain has already passed when `nextTick` is called; the rest of the
/// microtask queue runs before the tick queue is revisited. Node behaves
/// the same way here - MEASURED on v22.22.2, both ESM and CommonJS. This
/// is NOT a divergence, and the pg e2e asserting `["tick","promise"]` for
/// this arrangement demanded something Node does not do either.
#[compio::test]
async fn tick_queued_from_inside_a_microtask_runs_after_the_pending_promise_queue() {
    let body = run_js(
        r#"
export default {
    async fetch() {
        const ordering = [];
        Promise.resolve().then(() => ordering.push("promise"));
        process.nextTick(() => ordering.push("tick"));
        await Promise.resolve();
        await new Promise((r) => setTimeout(r, 0));
        return new Response(JSON.stringify(ordering));
    }
};
"#,
    )
    .await;

    assert_eq!(
        body, r#"["promise","tick"]"#,
        "a nextTick queued from inside a microtask runs after the pending \
         promise queue drains - Node included"
    );
}
