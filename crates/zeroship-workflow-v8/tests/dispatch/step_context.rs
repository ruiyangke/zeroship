//! `StepContext` delivery: a step body receives journal-derived identity, so a
//! body that re-executes after its effect landed can suppress the duplicate.
//!
//! A step body runs before its journal row is committed. When the committing
//! worker crashes or its lease expires, the frontier is discarded, the run is
//! reassigned, and the body runs again against the same journal prefix. The
//! key each execution observes must therefore be identical.

use super::{build_runtime, dispatch_workflow};
use serde_json::{json, Value};

/// Returns whatever the body reports through the step output.
fn reporting_runtime(body: &str) -> zeroship_runtime::runtime::Runtime {
    build_runtime(&format!(
        r#"
        export class Probe {{
            async run(_trigger, step) {{
                {body}
            }}
        }}
        export default {{ workflows: {{ Probe }} }};
        "#
    ))
}

/// A redelivered task is a new task: it carries a new dispatch nonce. Only the
/// journal is held constant, so a key built from anything else comes apart.
fn envelope_as(generation: i64, journal: Value, nonce: &str) -> String {
    json!({
        "runId": "wfr_probe",
        "generation": generation,
        "workflowName": "Probe",
        "nonce": nonce,
        "phase": "forward",
        "trigger": {
            "runId": "wfr_probe",
            "workflowName": "Probe",
            "startedAt": "2026-09-16T00:00:00Z",
            "input": {},
        },
        "journal": journal,
    })
    .to_string()
}

fn envelope(generation: i64, journal: Value) -> String {
    envelope_as(generation, journal, "wfd_probe")
}

fn completed_step(ordinal: i64, name: &str, occurrence: i64, output: Value) -> Value {
    json!({
        "ordinal": ordinal, "name": name, "nameOccurrence": occurrence,
        "kind": "run", "state": "completed", "output": output,
    })
}

/// Reports the context the body received, as the step's own output.
const REPORT_CONTEXT: &str = r#"
    return await step.run("charge", (ctx) => ({
        runId: ctx.runId,
        workflowName: ctx.workflowName,
        ordinal: ctx.ordinal,
        name: ctx.name,
        occurrence: ctx.occurrence,
        idempotencyKey: ctx.idempotencyKey,
        triggerRunId: ctx.trigger.runId,
    }));
"#;

fn step_output(result: &Value) -> Value {
    assert_eq!(result["kind"], "StepCompleted", "dispatch result: {result}");
    result["output"].clone()
}

/// The whole point of the key: the same step, executed twice because the first
/// frontier was never committed, must present the same key both times.
#[test]
fn step_idempotency_key_is_stable_across_re_execution() {
    let runtime = reporting_runtime(REPORT_CONTEXT);
    // The crash case: the body ran, but its journal row never committed, so the
    // reassigned task replays the identical prefix under a new dispatch nonce.
    let first = step_output(&dispatch_workflow(
        &runtime,
        &envelope_as(7, json!([]), "wfd_first"),
    ));
    let second = step_output(&dispatch_workflow(
        &runtime,
        &envelope_as(7, json!([]), "wfd_redelivered"),
    ));

    let key = first["idempotencyKey"].as_str().expect("key is a string");
    assert!(!key.is_empty(), "context: {first}");
    assert_eq!(
        first, second,
        "a re-executed step body observed a different context"
    );
    assert_eq!(key, "step:wfr_probe:7:0:0", "context: {first}");
    assert_eq!(
        first,
        json!({
            "runId": "wfr_probe",
            "workflowName": "Probe",
            "ordinal": 0,
            "name": "charge",
            "occurrence": 0,
            "idempotencyKey": "step:wfr_probe:7:0:0",
            "triggerRunId": "wfr_probe",
        }),
        "context: {first}"
    );
}

/// A restart copies the retained prefix under a new generation and re-executes
/// the rest at the same ordinals. A key that omitted the generation would let a
/// creator's downstream deduplicate the restart away.
#[test]
fn step_idempotency_key_separates_restart_generations() {
    let runtime = reporting_runtime(REPORT_CONTEXT);
    let first = step_output(&dispatch_workflow(&runtime, &envelope(1, json!([]))));
    let restarted = step_output(&dispatch_workflow(&runtime, &envelope(2, json!([]))));

    assert_eq!(first["ordinal"], restarted["ordinal"], "{first} {restarted}");
    assert_eq!(first["name"], restarted["name"], "{first} {restarted}");
    assert_ne!(
        first["idempotencyKey"], restarted["idempotencyKey"],
        "a restarted generation reused the key of the execution it replaces"
    );
    assert_eq!(first["idempotencyKey"], "step:wfr_probe:1:0:0");
    assert_eq!(restarted["idempotencyKey"], "step:wfr_probe:2:0:0");
}

/// The ordinal is the journal cursor, not a count of bodies this dispatch ran.
/// A replayed prefix contributes ordinals without invoking any body.
#[test]
fn step_ordinal_counts_the_replayed_journal_prefix() {
    let runtime = reporting_runtime(
        r#"
        await step.run("reserve", () => { throw new Error("replayed body ran"); });
        await step.run("settle", () => { throw new Error("replayed body ran"); });
        return await step.run("charge", (ctx) => ctx.idempotencyKey);
        "#,
    );
    let journal = json!([
        completed_step(0, "reserve", 0, json!("reserved")),
        completed_step(1, "settle", 0, json!("settled")),
    ]);
    let result = dispatch_workflow(&runtime, &envelope(3, journal));

    assert_eq!(
        step_output(&result),
        json!("step:wfr_probe:3:2:0"),
        "dispatch result: {result}"
    );
}

/// Two issuances of one name are distinct steps with distinct journal rows, so
/// they must not share a key. The ordinal alone already separates them -- the
/// journal's own uniqueness is on (run, generation, ordinal) -- so the
/// occurrence assertions here pin the key's shape rather than its uniqueness.
#[test]
fn step_idempotency_key_separates_repeats_of_one_name() {
    let runtime = reporting_runtime(
        r#"
        const keys = await Promise.all([
            step.run("charge", (ctx) => ctx.idempotencyKey),
            step.run("charge", (ctx) => ctx.idempotencyKey),
        ]);
        return keys;
        "#,
    );
    let result = dispatch_workflow(&runtime, &envelope(5, json!([])));
    let outcomes = result["outcomes"]
        .as_array()
        .unwrap_or_else(|| panic!("dispatch result: {result}"));
    assert_eq!(outcomes.len(), 2, "dispatch result: {result}");

    let first = &outcomes[0]["output"];
    let second = &outcomes[1]["output"];
    assert_ne!(first, second, "dispatch result: {result}");
    assert_eq!(first, "step:wfr_probe:5:0:0", "dispatch result: {result}");
    assert_eq!(second, "step:wfr_probe:5:1:1", "dispatch result: {result}");
}

/// The config overload shuffles its arguments, so the body reached through it
/// must still be the one handed the context.
#[test]
fn config_overload_still_hands_the_body_its_context() {
    let runtime = reporting_runtime(
        r#"
        return await step.run(
            "charge",
            { retries: { maxAttempts: 3 }, timeout: "10s" },
            (ctx) => ctx.idempotencyKey,
        );
        "#,
    );
    let result = dispatch_workflow(&runtime, &envelope(6, json!([])));
    assert_eq!(
        step_output(&result),
        json!("step:wfr_probe:6:0:0"),
        "dispatch result: {result}"
    );
}

/// `sideEffect` bodies re-execute on the same window, so they receive the same
/// context rather than an arbitrarily different one.
#[test]
fn side_effect_bodies_receive_the_same_context() {
    let runtime = reporting_runtime(
        r#"
        return await step.sideEffect("stamp", (ctx) => ({
            key: ctx.idempotencyKey,
            ordinal: ctx.ordinal,
            name: ctx.name,
            occurrence: ctx.occurrence,
        }));
        "#,
    );
    let first = step_output(&dispatch_workflow(
        &runtime,
        &envelope_as(9, json!([]), "wfd_first"),
    ));
    let second = step_output(&dispatch_workflow(
        &runtime,
        &envelope_as(9, json!([]), "wfd_redelivered"),
    ));

    assert_eq!(first, second, "a re-executed sideEffect body saw a new context");
    assert_eq!(
        first,
        json!({"key":"step:wfr_probe:9:0:0", "ordinal":0, "name":"stamp", "occurrence":0}),
        "context: {first}"
    );
}

/// A compensator key names the same step in the same generation as the forward
/// execution it undoes, and is distinguishable from that forward key. Without
/// the generation a restart's rollback would reuse the key of the rollback it
/// replaces.
#[test]
fn compensation_key_names_the_same_step_in_the_same_generation() {
    // Reporting through the rollback failure is how a compensator's context
    // reaches the dispatch result at all: a completed compensation carries no
    // payload of its own.
    let runtime = reporting_runtime(
        r#"
        await step.run("charge", {
            compensate: (_out, ctx) => { throw new Error(ctx.idempotencyKey); },
        }, () => { throw new Error("replayed body ran"); });
        return "unreachable";
        "#,
    );
    let compensating = |generation: i64, nonce: &str| {
        let journal = json!([{
            "ordinal": 0, "name": "charge", "nameOccurrence": 0, "kind": "run",
            "state": "completed", "output": "charged", "compensationState": "pending",
        }]);
        let mut value: Value =
            serde_json::from_str(&envelope_as(generation, journal, nonce)).expect("envelope");
        value["phase"] = json!("compensating");
        let result = dispatch_workflow(&runtime, &value.to_string());
        assert_eq!(
            result["kind"], "CompensationFailed",
            "dispatch result: {result}"
        );
        result["error"]["message"]
            .as_str()
            .unwrap_or_else(|| panic!("dispatch result: {result}"))
            .to_owned()
    };

    let first = compensating(4, "wfd_first");
    let replayed = compensating(4, "wfd_redelivered");
    let restarted = compensating(5, "wfd_restarted");

    assert_eq!(first, "comp:wfr_probe:4:0:0");
    assert_eq!(first, replayed, "a re-run compensator saw a different key");
    assert_ne!(
        first, restarted,
        "a restarted generation reused the rollback key it replaces"
    );
    assert_eq!(restarted, "comp:wfr_probe:5:0:0");
    // The forward key for the same step is distinguishable from its rollback.
    assert_ne!(first, "step:wfr_probe:4:0:0");
}
