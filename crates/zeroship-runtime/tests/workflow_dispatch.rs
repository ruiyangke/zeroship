use std::time::Duration;

use serde_json::Value;
use zeroship_runtime::{
    init_v8, EnvSnapshot, ModuleEntry, RequestCtx, WorkflowOutcome,
};
use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::runtime::Runtime;

fn build_runtime(user_src: &str) -> Runtime {
    init_v8();
    Runtime::builder()
        .modules(vec![ModuleEntry {
            specifier: "index.js".into(),
            source: user_src.into(),
        }])
        .build()
}

fn dispatch_workflow(runtime: &Runtime, envelope: &str) -> Value {
    let env = EnvSnapshot::empty();
    let ctx = RequestCtx::new(CancelFlag::new());
    match runtime.call_workflow_dispatch(envelope, &env, ctx) {
        WorkflowOutcome::Response { json, .. } => {
            serde_json::from_str(&json).unwrap_or_else(|err| panic!("workflow JSON parse failed: {err}; body={json}"))
        }
        WorkflowOutcome::Pending { rx, .. } => compio::runtime::Runtime::new()
            .expect("compio runtime")
            .block_on(async {
                runtime.start_pump();
                let settled = compio::time::timeout(Duration::from_secs(5), rx.recv())
                    .await
                    .expect("workflow dispatch timed out")
                    .expect("workflow dispatch returned error");
                serde_json::from_str(&settled.json).unwrap_or_else(|err| {
                    panic!("workflow JSON parse failed: {err}; body={}", settled.json)
                })
            }),
    }
}

#[test]
fn unawaited_step_frontier_fails_closed() {
    let runtime = build_runtime(
        r#"
        export class FireAndForgetWorkflow {
            run(_trigger, step) {
                step.run("unawaited", () => "side output");
                return "done";
            }
        }
        "#,
    );
    let result = dispatch_workflow(
        &runtime,
        r#"{"runId":"wfr_test","nonce":"nonce_test","workflowName":"FireAndForgetWorkflow","journal":[],"phase":"running","trigger":{"input":null}}"#,
    );

    assert_eq!(result["kind"], "RunFailed", "workflow result: {result}");
    assert_ne!(result["kind"], "RunCompleted", "workflow result: {result}");
    assert_eq!(result["error"]["type"], "NondeterministicError", "workflow result: {result}");
    assert_eq!(
        result["error"]["message"],
        "workflow completed while a frontier was pending",
        "workflow result: {result}"
    );
}
