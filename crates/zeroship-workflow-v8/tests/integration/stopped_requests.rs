//! A request stopped for CPU is refused the workflow calls its continuations
//! make.
//!
//! The binding here reaches no workflow service, so every call it admits fails
//! with `workflow_unavailable`, and a call it refuses for an ended request
//! fails with `request_cancelled` before it gets that far. The request leaves
//! one call in flight and burns CPU in its dispatch; the code after that call
//! tries a run handle's start and a run's status. The control is the same app
//! with no CPU limit, which answers on its own and leaves the same code running
//! with its calls admitted.

use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use zeroship_id::AppId;
use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::plugin::NativePlugin;
use zeroship_runtime::{
    init_v8, EnvSnapshot, FetchOutcome, ModuleEntry, RequestCtx, Runtime, SettledFetch,
};
use zeroship_workflow_v8::WorkflowBinding;

const APP: &str = r#"
let report;
const outcome = new Promise((resolve) => { report = resolve; });
async function attempt(call) {
    try {
        await call();
        return "admitted";
    } catch (error) {
        return error.code ?? String(error);
    }
}
export default {
    async fetch(request, env) {
        if (new URL(request.url).pathname === "/outcome") return Response.json(await outcome);
        const handle = env.workflows.Checkout;
        handle.start({}).catch(() => {}).then(async () => report({
            start: await attempt(() => handle.start({})),
            status: await attempt(() => handle.get("run_03gzjh07tkcod6b28kcxxc0mx").status()),
        }));
        let x = 0;
        for (let i = 0; i < 50000000; i++) { x = (x + 1) % 2147483647; }
        return Response.json({ survived: true, x });
    },
};
"#;

fn request(runtime: &Runtime, path: &str) -> FetchOutcome {
    runtime.call_fetch_handler(
        "GET",
        &format!("http://localhost{path}"),
        &[],
        "",
        &EnvSnapshot::empty(),
        RequestCtx::new(CancelFlag::new()),
    )
}

/// Run the app once under `cpu_limit`: the request's status, and what its
/// continuation reported.
async fn run(cpu_limit: Option<Duration>) -> (u16, Value) {
    init_v8();
    let app_id = AppId::mint();
    let plugin: Arc<dyn NativePlugin> = Arc::new(WorkflowBinding::unavailable());
    let mut builder = Runtime::builder()
        .modules(vec![ModuleEntry { specifier: "index.js".into(), source: APP.into() }])
        .env_vars(HashMap::from([("APP_ID".to_owned(), app_id.as_str().to_owned())]))
        .plugins(vec![plugin])
        .app_id(app_id);
    if let Some(limit) = cpu_limit {
        builder = builder.cpu_limit(limit);
    }
    let runtime = builder.build();
    runtime.start_pump();
    let FetchOutcome::Response { status, .. } = request(&runtime, "/") else {
        panic!("the request answers in its dispatch, stopped or not");
    };
    let body = match request(&runtime, "/outcome") {
        FetchOutcome::Response { body, .. } => body,
        FetchOutcome::Pending { rx, .. } => {
            match compio::time::timeout(Duration::from_secs(30), rx.recv())
                .await
                .expect("the outcome is answered")
                .expect("the outcome completes")
            {
                SettledFetch::Response { body, .. } => body,
                _ => panic!("the outcome is a buffered response"),
            }
        }
        _ => panic!("the outcome is a buffered response"),
    };
    (status, serde_json::from_slice(&body).expect("the outcome is JSON"))
}

#[compio::test]
async fn a_stopped_request_is_refused_its_workflow_calls() {
    let (status, outcome) = run(Some(Duration::from_millis(10))).await;
    assert_eq!(status, 503, "the request is stopped in its dispatch");
    assert_eq!(
        outcome,
        serde_json::json!({ "start": "request_cancelled", "status": "request_cancelled" }),
    );
}

#[compio::test]
async fn a_request_answered_on_its_own_keeps_its_workflow_calls() {
    let (status, outcome) = run(None).await;
    assert_eq!(status, 200, "with no CPU limit the request answers on its own");
    assert_eq!(
        outcome,
        serde_json::json!({ "start": "workflow_unavailable", "status": "workflow_unavailable" }),
        "an admitted call reaches the binding, which answers that it has no service",
    );
}
