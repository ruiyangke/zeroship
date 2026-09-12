use super::{build_runtime, dispatch_workflow};
use serde_json::{json, Value};

fn assert_failure_batch(result: &Value) {
    assert_eq!(result["kind"], "RunFailed");
    assert_eq!(
        result["outcomes"],
        json!([{"kind":"RunFailed", "error":result["error"]}])
    );
    assert!(result["error"]["message"]
        .as_str()
        .is_some_and(|s| !s.is_empty()));
}

#[test]
fn module_initialization_failure_returns_a_journal_outcome() {
    let runtime = build_runtime("export class Invalid {");
    let result = dispatch_workflow(&runtime, "{}");
    assert_failure_batch(&result);
    assert!(result["error"]["message"]
        .as_str()
        .unwrap()
        .contains("module init failed"));
}

#[test]
fn invalid_dispatch_envelope_returns_a_journal_outcome() {
    let runtime = build_runtime("export default {};");
    let result = dispatch_workflow(&runtime, "{");
    assert_failure_batch(&result);
    assert_eq!(result["error"]["message"], "invalid workflow dispatch JSON");
}

#[cfg(target_os = "linux")]
#[test]
fn cpu_termination_returns_a_journal_outcome() {
    use std::time::Duration;
    use zeroship_runtime::{init_v8, ModuleEntry, Runtime};
    init_v8();
    let runtime = Runtime::builder()
        .modules(vec![ModuleEntry {
            specifier: "index.js".into(),
            source: r"
                export class Example {
                    run() {
                        let n = 0;
                        for (let i = 0; i < 300000000; i++) n = (n + 1) % 2147483647;
                        return n;
                    }
                }
            "
            .into(),
        }])
        .cpu_limit(Duration::from_millis(20))
        .build();
    let result = dispatch_workflow(
        &runtime,
        r#"{"workflowName":"Example","journal":[],"phase":"running","trigger":{}}"#,
    );
    assert_failure_batch(&result);
    assert_eq!(result["error"]["message"], "CPU time limit exceeded");
}
