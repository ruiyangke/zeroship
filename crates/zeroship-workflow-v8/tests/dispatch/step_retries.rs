//! `StepConfig.retries` as the replay bridge reports it.
//!
//! The bridge owns none of the retry decision: it counts nothing and schedules
//! nothing, because neither survives the isolate. What it owns is carrying what
//! the body declared to the host that does decide, and refusing a declaration
//! it cannot carry honestly.

use super::{build_runtime, dispatch_workflow};
use serde_json::{json, Value};

/// A workflow whose one step fails, configured however the case needs.
fn failing_runtime(config: &str) -> zeroship_runtime::runtime::Runtime {
    build_runtime(&format!(
        r#"
        export class Probe {{
            async run(_trigger, step) {{
                return await step.run("charge", {config}, () => {{
                    throw new Error("intentional failure");
                }});
            }}
        }}
        export default {{ workflows: {{ Probe }} }};
        "#
    ))
}

fn envelope() -> String {
    json!({
        "runId": "wfr_probe", "generation": 0, "workflowName": "Probe",
        "nonce": "wfd_probe", "phase": "forward",
        "trigger": {"runId":"wfr_probe", "workflowName":"Probe",
            "startedAt":"2026-09-16T00:00:00Z", "input":{}},
        "journal": [],
    })
    .to_string()
}

fn failure(result: &Value) -> Value {
    assert_eq!(result["kind"], "RunFailed", "dispatch result: {result}");
    assert_eq!(result["ordinal"], json!(0), "dispatch result: {result}");
    result.clone()
}

/// A declared ceiling reaches the host on the failure that would spend it.
///
/// This is the whole wire: the host cannot ask the isolate what the body
/// declared, because by the time it decides, the isolate is gone.
#[test]
fn a_declared_ceiling_rides_the_failure_it_bounds() {
    let result = dispatch_workflow(
        &failing_runtime("{ retries: { maxAttempts: 5 } }"),
        &envelope(),
    );
    assert_eq!(failure(&result)["maxAttempts"], json!(5), "{result}");
}

/// The control differing in exactly one variable: no declared `retries`, the
/// same failing body, and the failure names one attempt.
#[test]
fn a_step_without_declared_retries_reports_one_attempt() {
    let result = dispatch_workflow(&failing_runtime("{ timeout: \"10s\" }"), &envelope());
    assert_eq!(failure(&result)["maxAttempts"], json!(1), "{result}");
}

/// The bodyless overload has no config at all, and must report the same one
/// attempt rather than omitting the field and leaving the host to guess.
#[test]
fn a_step_declared_without_config_reports_one_attempt() {
    let runtime = build_runtime(
        r#"
        export class Probe {
            async run(_trigger, step) {
                return await step.run("charge", () => { throw new Error("intentional failure"); });
            }
        }
        export default { workflows: { Probe } };
        "#,
    );
    let result = dispatch_workflow(&runtime, &envelope());
    assert_eq!(failure(&result)["maxAttempts"], json!(1), "{result}");
}

/// A spelling the bridge cannot carry fails the run before the body is invoked,
/// the way an unreadable `timeout` already does. Rounding it to one attempt
/// would hand back exactly the silence this option is being wired to end.
#[test]
fn an_unusable_ceiling_fails_before_the_body_runs() {
    for declared in ["0", "-1", "\"3\"", "2.5"] {
        let runtime = build_runtime(&format!(
            r#"
            globalThis.__ran = false;
            export class Probe {{
                async run(_trigger, step) {{
                    return await step.run("charge", {{ retries: {{ maxAttempts: {declared} }} }}, () => {{
                        globalThis.__ran = true;
                        return "ok";
                    }});
                }}
            }}
            export default {{ workflows: {{ Probe }} }};
            "#
        ));
        let result = dispatch_workflow(&runtime, &envelope());
        assert_eq!(result["kind"], "RunFailed", "declared {declared}: {result}");
        assert!(
            result["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains("retries.maxAttempts")),
            "declared {declared}: {result}"
        );
    }
}

/// The two halves of the decision travel together on one outcome: the ceiling
/// the body declared, and whether this failure says running it again could
/// clear it. The host needs both, and neither is recoverable afterwards.
///
/// `packages/workflows/tests/dispatch.test.ts` owns the other end of this, that
/// the SDK's own permanent condition declares `retryable` at all.
#[test]
fn a_failure_carries_both_its_ceiling_and_its_own_verdict() {
    let runtime = build_runtime(
        r#"
        export class Probe {
            async run(_trigger, step) {
                return await step.run("charge", { retries: { maxAttempts: 5 } }, () => {
                    const error = new Error("declined");
                    error.name = "PermanentError";
                    error.retryable = false;
                    throw error;
                });
            }
        }
        export default { workflows: { Probe } };
        "#,
    );
    let result = dispatch_workflow(&runtime, &envelope());
    let failed = failure(&result);
    assert_eq!(failed["maxAttempts"], json!(5), "{result}");
    assert_eq!(failed["error"]["type"], json!("PermanentError"), "{result}");
    assert_eq!(failed["error"]["retryable"], json!(false), "{result}");
}
