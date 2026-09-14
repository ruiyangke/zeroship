use serde_json::{Value, json};

use super::{build_runtime, dispatch_workflow};

fn invoke(source: &str, workflow: &str, journal: Value) -> Value {
    let runtime = build_runtime(source);
    dispatch_workflow(
        &runtime,
        &json!({
            "runId": "wfr_lookup", "nonce": "lookup", "workflowName": workflow,
            "phase": "running", "trigger": { "input": null }, "journal": journal,
        })
        .to_string(),
    )
}

const FROZEN_CHILD: &str = r#"
    const x = Object.freeze(class x { run() { return "child"; } });
    export { x as Child };
    export class Parent {
        run(_trigger, step) { return step.call(x, { value: "input" }); }
    }
"#;

#[test]
fn frozen_minified_child_uses_its_export_binding() {
    let result = invoke(FROZEN_CHILD, "Parent", json!([]));
    assert_eq!(result["kind"], "Child", "{result}");
    assert_eq!(result["childWorkflowName"], "Child", "{result}");
    assert_eq!(result["name"], "Child", "{result}");
    assert_eq!(result["input"], json!({ "value": "input" }));
}

#[test]
fn synthetic_entry_forwards_workflows_as_named_module_exports() {
    zeroship_runtime::init_v8();
    let runtime = zeroship_runtime::Runtime::builder()
        .modules(vec![
            zeroship_runtime::ModuleEntry {
                specifier: "index.js".into(),
                source: "export * from './app.js'; export default { rpc: {} };".into(),
            },
            zeroship_runtime::ModuleEntry {
                specifier: "app.js".into(),
                source: FROZEN_CHILD.into(),
            },
        ])
        .build();
    let result = dispatch_workflow(
        &runtime,
        &json!({
            "runId": "wfr_forwarded", "nonce": "lookup", "workflowName": "Parent",
            "phase": "running", "trigger": { "input": null }, "journal": [],
        })
        .to_string(),
    );
    assert_eq!(result["kind"], "Child", "{result}");
    assert_eq!(result["childWorkflowName"], "Child", "{result}");
}

#[test]
fn frozen_minified_child_replays_the_export_named_journal() {
    let result = invoke(
        FROZEN_CHILD,
        "Parent",
        json!([{
            "ordinal": 0, "name": "Child", "nameOccurrence": 0,
            "kind": "child", "state": "completed", "output": "saved-child",
        }]),
    );
    assert_eq!(result["kind"], "RunCompleted", "{result}");
    assert_eq!(result["output"], "saved-child", "{result}");
}

#[test]
fn declared_child_binding_survives_function_name_changes() {
    let result = invoke(
        r#"
        class x { run() {} }
        class Parent {
            run(_trigger, step) {
                Object.defineProperty(x, "name", { value: "Changed" });
                return step.call(x, null);
            }
        }
        export default { workflows: { Parent, Child: x } };
        "#,
        "Parent",
        json!([]),
    );
    assert_eq!(result["kind"], "Child", "{result}");
    assert_eq!(result["childWorkflowName"], "Child", "{result}");
}

#[test]
fn an_unexported_constructor_cannot_select_a_child_by_name() {
    let result = invoke(
        r#"
        export class Child { run() {} }
        const impostor = class Child { run() {} };
        export class Parent {
            run(_trigger, step) { return step.call(impostor, null); }
        }
        "#,
        "Parent",
        json!([]),
    );
    assert_eq!(result["kind"], "RunFailed", "{result}");
    assert_eq!(
        result["error"]["message"], "child workflow must be an exported workflow constructor",
        "{result}"
    );
}

#[test]
fn conflicting_names_for_a_constructor_are_rejected() {
    let result = invoke(
        r#"
        class x { run() {} }
        export { x as First, x as Second };
        export class Parent {
            run(_trigger, step) { return step.call(x, null); }
        }
        "#,
        "Parent",
        json!([]),
    );
    assert_eq!(result["kind"], "RunFailed", "{result}");
    assert_eq!(
        result["error"]["message"], "workflow export bindings must be unambiguous",
        "{result}"
    );
}

#[test]
fn conflicting_constructors_for_an_export_are_rejected() {
    let result = invoke(
        r#"
        export class Target { run() { return "named"; } }
        class Different { run() { return "declared"; } }
        export default { workflows: { Target: Different } };
        "#,
        "Target",
        json!([]),
    );
    assert_eq!(result["kind"], "RunFailed", "{result}");
    assert_eq!(
        result["error"]["message"], "workflow export bindings must be unambiguous",
        "{result}"
    );
}

#[test]
fn matching_named_and_declared_exports_share_constructor_identity() {
    let result = invoke(
        r#"
        export class Target { run() { return "target"; } }
        export default { workflows: { Target } };
        "#,
        "Target",
        json!([]),
    );
    assert_eq!(result["kind"], "RunCompleted", "{result}");
    assert_eq!(result["output"], "target", "{result}");
}

#[test]
fn missing_workflow_does_not_fall_back_to_a_default_constructor() {
    let result = invoke(
        "export default class Present { run() { return 'wrong target'; } }",
        "Missing",
        json!([]),
    );
    assert_eq!(result["kind"], "RunFailed", "{result}");
    assert_eq!(
        result["error"]["message"], "Workflow not found: Missing",
        "{result}"
    );
}

#[test]
fn inherited_workflow_properties_are_not_exports() {
    let result = invoke(
        r#"
        class Hidden { run() { return "inherited"; } }
        export default { workflows: Object.create({ Hidden }) };
        "#,
        "Hidden",
        json!([]),
    );
    assert_eq!(result["kind"], "RunFailed", "{result}");
    assert_eq!(
        result["error"]["message"], "Workflow not found: Hidden",
        "{result}"
    );
}
