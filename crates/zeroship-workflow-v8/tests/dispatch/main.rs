use std::sync::Arc;
use std::time::Duration;

mod failure;
mod host_module;
mod step_context;

use serde_json::Value;
use zeroship_runtime::plugin::NativePlugin;
use zeroship_runtime::{
    init_v8, EnvSnapshot, ModuleEntry, RequestCtx, WorkflowOutcome,
};
use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::runtime::Runtime;
use zeroship_workflow::service::runner::ready::ReadyApps;
use zeroship_workflow_v8::WorkflowBinding;

/// The binding registers the host dispatch module, so a runtime without it
/// cannot replay a workflow at all.
fn workflow_plugin() -> Arc<dyn NativePlugin> {
    Arc::new(WorkflowBinding::ready(ReadyApps::default()))
}

fn build_runtime(user_src: &str) -> Runtime {
    build_runtime_with(vec![ModuleEntry {
        specifier: "index.js".into(),
        source: user_src.into(),
    }])
}

fn build_runtime_with(modules: Vec<ModuleEntry>) -> Runtime {
    workflow_builder().modules(modules).build()
}

/// Every runtime that replays a workflow needs the binding registered: it is
/// what supplies the host dispatch module the runtime calls into.
fn workflow_builder() -> zeroship_runtime::RuntimeBuilder {
    init_v8();
    Runtime::builder().plugins(vec![workflow_plugin()])
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
fn blob_replay_uses_the_native_run_reader() {
    let runtime = build_runtime(
        r#"
        let reads = 0;
        globalThis.__zs_env = () => ({ workflows: { BlobWorkflow: { get(id) {
          if (id !== "run_saved") throw new Error("wrong run");
          return { async readStepOutput(name, occurrence) {
            if (name !== "payload" || occurrence !== 0) throw new Error("wrong step");
            reads++;
            return new TextEncoder().encode('{"saved":true}');
          } };
        } } } });
        export class BlobWorkflow {
          async run(_trigger, step) {
            const output = await step.run("payload", () => { throw new Error("replayed callback"); });
            const value = await output.json();
            await output.bytes();
            return { value, reads };
          }
        }
        export default { workflows: { BlobWorkflow } };
    "#,
    );
    let envelope = serde_json::json!({
        "runId": "run_saved", "workflowName": "BlobWorkflow", "nonce": "dispatch_saved",
        "trigger": { "runId": "run_saved", "workflowName": "BlobWorkflow", "startedAt": "2026-09-11T00:00:00Z", "input": {} },
        "journal": [{ "ordinal": 0, "name": "payload", "kind": "run", "state": "completed",
            "outputRef": { "hash": "a".repeat(64), "size": 14, "contentType": "application/json" } }],
    });
    let result = dispatch_workflow(&runtime, &envelope.to_string());
    assert_eq!(result["kind"], "RunCompleted", "{result}");
    assert_eq!(
        result["output"],
        serde_json::json!({"value":{"saved":true},"reads":1})
    );
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

mod lookup;
