use serde_json::{json, Value};

use super::{build_runtime, build_runtime_with, dispatch_workflow};

fn invoke(source: &str, workflow: &str, journal: &Value) -> Value {
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
    let result = invoke(FROZEN_CHILD, "Parent", &json!([]));
    assert_eq!(result["kind"], "Child", "{result}");
    assert_eq!(result["childWorkflowName"], "Child", "{result}");
    assert_eq!(result["name"], "Child", "{result}");
    assert_eq!(result["input"], json!({ "value": "input" }));
}

#[test]
fn synthetic_entry_forwards_workflows_as_named_module_exports() {
    let runtime = build_runtime_with(vec![
        zeroship_runtime::ModuleEntry {
            specifier: "index.js".into(),
            source: "export * from './app.js'; export default { rpc: {} };".into(),
        },
        zeroship_runtime::ModuleEntry {
            specifier: "app.js".into(),
            source: FROZEN_CHILD.into(),
        },
    ]);
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
        &json!([{
            "ordinal": 0, "name": "Child", "nameOccurrence": 0,
            "kind": "child", "state": "completed", "output": "saved-child",
        }]),
    );
    assert_eq!(result["kind"], "RunCompleted", "{result}");
    assert_eq!(result["output"], "saved-child", "{result}");
}

#[test]
fn synthetic_entry_preserves_explicit_workflow_dictionary_data() {
    let runtime = build_runtime_with(vec![
        zeroship_runtime::ModuleEntry {
            specifier: "index.js".into(),
            source: "import * as user from './app.js'; export * from './app.js'; export default { rpc: {}, workflows: user.default.workflows };".into(),
        },
        zeroship_runtime::ModuleEntry {
            specifier: "app.js".into(),
            source: r"
                const x = Object.freeze(class x { run() {} });
                class Parent { run(_trigger, step) { return step.call(x, null); } }
                export default { workflows: { Parent, Child: x } };
            ".into(),
        },
    ]);
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
        &json!([]),
    );
    assert_eq!(result["kind"], "Child", "{result}");
    assert_eq!(result["childWorkflowName"], "Child", "{result}");
}

#[test]
fn an_unexported_constructor_cannot_select_a_child_by_name() {
    let result = invoke(
        r"
        export class Child { run() {} }
        const impostor = class Child { run() {} };
        export class Parent {
            run(_trigger, step) { return step.call(impostor, null); }
        }
        ",
        "Parent",
        &json!([]),
    );
    assert_eq!(result["kind"], "RunFailed", "{result}");
    assert_eq!(
        result["error"]["message"], "child workflow must be an exported workflow constructor",
        "{result}"
    );
}

#[test]
fn conflicting_names_for_a_constructor_are_rejected() {
    for declaration in ["", "export default { workflows: { First: x } };"] {
        let source = format!(
            r"
            class x {{ run() {{}} }}
            export {{ x as First, x as Second }};
            {declaration}
            export class Parent {{
                run(_trigger, step) {{ return step.call(x, null); }}
            }}
            "
        );
        let result = invoke(&source, "Parent", &json!([]));
        assert_eq!(result["kind"], "RunFailed", "{result}");
        assert_eq!(
            result["error"]["message"], "workflow export bindings must be unambiguous",
            "{result}"
        );
    }
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
        &json!([]),
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
        &json!([]),
    );
    assert_eq!(result["kind"], "RunCompleted", "{result}");
    assert_eq!(result["output"], "target", "{result}");
}

#[test]
fn missing_workflow_does_not_fall_back_to_a_default_constructor() {
    let result = invoke(
        "export default class Present { run() { return 'wrong target'; } }",
        "Missing",
        &json!([]),
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
        &json!([]),
    );
    assert_eq!(result["kind"], "RunFailed", "{result}");
    assert_eq!(
        result["error"]["message"], "Workflow not found: Hidden",
        "{result}"
    );
}

#[test]
fn instance_field_run_implementations_keep_export_identity() {
    for declarations in [
        "export { x as Child, Parent };",
        "export default { workflows: { Child: x, Parent } };",
    ] {
        let source = format!(
            r#"
            const x = Object.freeze(class x {{ run = () => "child"; }});
            class Parent {{ run = (_trigger, step) => step.call(x, null); }}
            {declarations}
            "#,
        );
        let result = invoke(&source, "Parent", &json!([]));
        assert_eq!(result["kind"], "Child", "{result}");
        assert_eq!(result["childWorkflowName"], "Child", "{result}");
        let child = invoke(&source, "Child", &json!([]));
        assert_eq!(child["kind"], "RunCompleted", "{child}");
        assert_eq!(child["output"], "child", "{child}");
    }
}

#[test]
fn unrelated_aliased_callables_are_not_constructed_or_resolved() {
    let result = invoke(
        r#"
        function unrelated() { throw new Error("unrelated constructor ran"); }
        export { unrelated as First, unrelated as Second };
        export class Target { run = () => "target"; }
        "#,
        "Target",
        &json!([]),
    );
    assert_eq!(result["kind"], "RunCompleted", "{result}");
    assert_eq!(result["output"], "target", "{result}");
}

#[test]
fn start_many_uses_export_identity_for_frontiers_and_replay() {
    let source = r#"
        const x = Object.freeze(class x { run = () => "child"; });
        export { x as Child };
        export class Parent {
            run(_trigger, step) {
                return step.startMany(x, [{ input: "first" }, { input: "second" }]);
            }
        }
    "#;
    let frontier = invoke(source, "Parent", &json!([]));
    let outcomes = frontier["outcomes"].as_array().expect("child frontiers");
    assert_eq!(outcomes.len(), 2, "{frontier}");
    for (ordinal, outcome) in outcomes.iter().enumerate() {
        assert_eq!(outcome["kind"], "Child", "{frontier}");
        assert_eq!(outcome["childWorkflowName"], "Child", "{frontier}");
        assert_eq!(outcome["name"], "Child", "{frontier}");
        assert_eq!(outcome["ordinal"], ordinal, "{frontier}");
        assert_eq!(outcome["nameOccurrence"], ordinal, "{frontier}");
    }
    let replay = invoke(
        source,
        "Parent",
        &json!([
            { "ordinal": 0, "nameOccurrence": 0, "name": "Child", "kind": "child", "state": "completed", "output": "first" },
            { "ordinal": 1, "nameOccurrence": 1, "name": "Child", "kind": "child", "state": "completed", "output": "second" },
        ]),
    );
    assert_eq!(replay["kind"], "RunCompleted", "{replay}");
    assert_eq!(replay["output"], json!(["first", "second"]), "{replay}");
}
