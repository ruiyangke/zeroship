//! Next-tick ordering at module evaluation and inside an explicit promise job.
//! See `perform_microtask_checkpoint` for the native drain order.

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

/// Queue both callbacks from an explicit promise continuation.
#[compio::test]
async fn tick_queued_from_inside_a_microtask_runs_after_the_pending_promise_queue() {
    let body = run_js(
        r#"
export default {
    async fetch() {
        await Promise.resolve();
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
