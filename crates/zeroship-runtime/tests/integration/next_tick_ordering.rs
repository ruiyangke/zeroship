//! `process.nextTick` against promise microtasks, per arrangement. Each
//! arrangement runs the same JavaScript in this runtime and in Node, so the
//! Node answer is measured by the test rather than recalled in a comment.
//! See `perform_microtask_checkpoint` for the native drain order.

use std::process::Command;
use std::time::Duration;

use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::{EnvSnapshot, FetchOutcome, ModuleEntry, RequestCtx, Runtime, SettledFetch};

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

/// Run an ES module under Node and return what it wrote to stdout.
fn node_module(source: &str) -> String {
    let output = Command::new("node")
        .args(["--input-type=module", "--eval", source])
        .output()
        .expect("Node is required: it is the reference these orderings are checked against");
    assert!(
        output.status.success(),
        "node exited with {}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("node wrote UTF-8")
}

/// `main` as this runtime's `default.fetch`: the host calls the handler
/// directly, so `main`'s synchronous prefix runs inside that host call.
async fn runtime_fetch_handler(main_src: &str) -> String {
    run_js(&format!(
        r#"
{main_src}
export default {{
    async fetch() {{
        return new Response(JSON.stringify(await main()));
    }}
}};
"#
    ))
    .await
}

/// `main` as a Node `http` request listener, which Node invokes from its event
/// loop - the Node counterpart of a host-invoked `default.fetch`.
fn node_request_listener(main_src: &str) -> String {
    node_module(&format!(
        r#"
import http from "node:http";
{main_src}
const server = http.createServer(async (_request, response) => {{
    response.end(JSON.stringify(await main()));
}});
server.listen(0, "127.0.0.1", async () => {{
    try {{
        const {{ port }} = server.address();
        const response = await fetch(`http://127.0.0.1:${{port}}/`);
        process.stdout.write(await response.text());
    }} finally {{
        server.close();
    }}
}});
"#
    ))
}

/// Both queued from the handler's synchronous prefix - the arrangement
/// `node_pg_e2e.rs` builds.
const QUEUED_FROM_THE_HANDLER_PREFIX: &str = r#"
async function main() {
    const ordering = [];
    Promise.resolve().then(() => ordering.push("promise"));
    process.nextTick(() => ordering.push("tick"));
    await Promise.resolve();
    await new Promise((resolve) => setTimeout(resolve, 0));
    return ordering;
}
"#;

/// Both queued after the handler's first `await`, so from inside a promise job.
const QUEUED_FROM_INSIDE_A_MICROTASK: &str = r#"
async function main() {
    await Promise.resolve();
    const ordering = [];
    Promise.resolve().then(() => ordering.push("promise"));
    process.nextTick(() => ordering.push("tick"));
    await Promise.resolve();
    await new Promise((resolve) => setTimeout(resolve, 0));
    return ordering;
}
"#;

/// The handler's synchronous prefix is not a promise job: the host calls
/// `default.fetch` from its event loop, as Node calls a request listener, so
/// the tick queue drains before the pending promise queue.
#[compio::test]
async fn tick_queued_from_the_handler_prefix_runs_before_the_pending_promise_queue() {
    let expected = r#"["tick","promise"]"#;
    assert_eq!(
        node_request_listener(QUEUED_FROM_THE_HANDLER_PREFIX),
        expected,
        "Node's request listener is the reference for a host-invoked handler"
    );
    assert_eq!(
        runtime_fetch_handler(QUEUED_FROM_THE_HANDLER_PREFIX).await,
        expected,
        "a nextTick queued from the handler's synchronous prefix must drain \
         before the promise queue, as it does in Node's request listener"
    );
}

/// Inside a promise job the tick waits for the whole pending promise queue,
/// in Node and here alike.
#[compio::test]
async fn tick_queued_from_inside_a_microtask_runs_after_the_pending_promise_queue() {
    let expected = r#"["promise","tick"]"#;
    assert_eq!(
        node_request_listener(QUEUED_FROM_INSIDE_A_MICROTASK),
        expected,
        "Node's request listener is the reference for a host-invoked handler"
    );
    assert_eq!(
        runtime_fetch_handler(QUEUED_FROM_INSIDE_A_MICROTASK).await,
        expected,
        "a nextTick queued from inside a microtask must run after the pending \
         promise queue drains, as it does in Node"
    );
}

/// ESM module top level is the arrangement where this runtime and Node part.
/// Node evaluates an ES module from inside a job, so the tick waits for the
/// promise queue there. This runtime evaluates the module from the host and
/// drains the tick queue first, as Node does only for CommonJS. Both answers
/// are asserted so the divergence stays measured and visible.
#[compio::test]
async fn tick_queued_at_esm_top_level_runs_before_promise_microtasks_unlike_node_esm() {
    assert_eq!(
        node_module(
            r#"
const ordering = [];
Promise.resolve().then(() => ordering.push("promise"));
process.nextTick(() => ordering.push("tick"));
setTimeout(() => process.stdout.write(JSON.stringify(ordering)), 0);
"#
        ),
        r#"["promise","tick"]"#,
        "Node's ES module top level is the reference this runtime diverges from"
    );

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
        "this runtime drains a top-level nextTick before the promise queue"
    );
}
