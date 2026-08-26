use std::collections::HashSet;

use chrono::{DateTime, Utc};
use compio_postgres::{Client, GenericClient, NoTls, Row};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::engine::StepResult;
use crate::errors::WorkflowError;
use crate::store::pg::WorkflowTables;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WorkflowAdvanceNackKind {
    Deadlock,
    Invalid,
    ApplyFailed,
    Backpressure,
    ClaimLost,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowRunDispatchRequest {
    pub run_id: String,
    pub app_id: Uuid,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowAdvanceRegistration {
    pub run_id: String,
    pub app_id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_wake_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub terminal: bool,
}

impl WorkflowAdvanceRegistration {
    #[must_use]
    pub fn next(run_id: impl Into<String>, app_id: Uuid, next_wake_at: DateTime<Utc>) -> Self {
        Self {
            run_id: run_id.into(),
            app_id,
            next_wake_at: Some(next_wake_at),
            terminal: false,
        }
    }

    #[must_use]
    pub fn terminal(run_id: impl Into<String>, app_id: Uuid) -> Self {
        Self {
            run_id: run_id.into(),
            app_id,
            next_wake_at: None,
            terminal: true,
        }
    }

    #[must_use]
    pub fn preserve(run_id: impl Into<String>, app_id: Uuid) -> Self {
        Self {
            run_id: run_id.into(),
            app_id,
            next_wake_at: None,
            terminal: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowAdvanceResponse {
    #[serde(default, skip_serializing_if = "is_false")]
    pub ack: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub nack: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_wake_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub registrations: Vec<WorkflowAdvanceRegistration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nack_kind: Option<WorkflowAdvanceNackKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl WorkflowAdvanceResponse {
    #[must_use]
    pub fn ack(run_id: impl Into<String>, registrations: Vec<WorkflowAdvanceRegistration>) -> Self {
        let run_id = run_id.into();
        let next_wake_at = registrations
            .iter()
            .find(|registration| registration.run_id == run_id)
            .and_then(|registration| registration.next_wake_at);
        Self {
            ack: true,
            nack: false,
            run_id: Some(run_id),
            next_wake_at,
            registrations,
            nack_kind: None,
            reason: None,
        }
    }

    #[must_use]
    pub fn nack(
        run_id: impl Into<String>,
        kind: WorkflowAdvanceNackKind,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            ack: false,
            nack: true,
            run_id: Some(run_id.into()),
            next_wake_at: None,
            registrations: Vec::new(),
            nack_kind: Some(kind),
            reason: Some(reason.into()),
        }
    }

    #[must_use]
    pub fn is_ack(&self) -> bool {
        self.ack && !self.nack
    }

    #[must_use]
    pub fn is_nack(&self) -> bool {
        self.nack && !self.ack
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}

pub fn worker_json_to_step_result(
    run_id: &str,
    dispatch_nonce: &str,
    worker_json: &str,
) -> Result<StepResult, String> {
    let value: Value = serde_json::from_str(worker_json).map_err(|e| e.to_string())?;
    worker_value_to_step_result(run_id, dispatch_nonce, value)
}

pub fn worker_value_to_step_result(
    run_id: &str,
    dispatch_nonce: &str,
    result: Value,
) -> Result<StepResult, String> {
    let value = worker_value_to_step_result_value(run_id, dispatch_nonce, result)?;
    serde_json::from_value(value).map_err(|e| e.to_string())
}

fn worker_value_to_step_result_value(
    run_id: &str,
    dispatch_nonce: &str,
    result: Value,
) -> Result<Value, String> {
    if result.get("runUpdate").is_some() && result.get("dispatchNonce").is_some() {
        let normalized = normalize_workflow_step_result(result)?;
        let outcomes = legacy_step_result_to_outcomes(&normalized)?;
        return Ok(serde_json::json!({
            "runId": normalized
                .get("runId")
                .and_then(Value::as_str)
                .unwrap_or(run_id),
            "dispatchNonce": normalized
                .get("dispatchNonce")
                .and_then(Value::as_str)
                .unwrap_or(dispatch_nonce),
            "outcomes": outcomes,
        }));
    }

    let normalized_run_id = result
        .get("runId")
        .and_then(Value::as_str)
        .unwrap_or(run_id);
    let nonce = result
        .get("dispatchNonce")
        .or_else(|| result.get("nonce"))
        .and_then(Value::as_str)
        .unwrap_or(dispatch_nonce);

    let outcomes = if let Some(outcomes) = result.get("outcomes") {
        let outcomes = outcomes
            .as_array()
            .ok_or_else(|| "outcomes must be an array".to_string())?
            .clone();
        normalize_workflow_outcomes(outcomes, result.get("error").cloned())?
    } else {
        normalize_workflow_outcomes(vec![single_worker_result_to_outcome(&result)?], None)?
    };

    Ok(serde_json::json!({
        "runId": normalized_run_id,
        "dispatchNonce": nonce,
        "outcomes": outcomes,
    }))
}

fn single_worker_result_to_outcome(result: &Value) -> Result<Value, String> {
    let kind = result
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing kind".to_string())?;
    match kind {
        "StepCompleted" => {
            let mut outcome = serde_json::json!({
                "kind": "StepCompleted",
                "ordinal": required_i64(result, "ordinal")?,
                "name": required_str(result, "name")?,
                "nameOccurrence": result.get("nameOccurrence").and_then(Value::as_i64).unwrap_or(0),
                "stepKind": workflow_step_kind_or_run(result)?,
                "compensable": result.get("compensable").and_then(Value::as_bool).unwrap_or(false),
                "compensationMaxAttempts": result.get("compensationMaxAttempts").and_then(Value::as_i64).unwrap_or(1),
            });
            copy_workflow_output(result, &mut outcome);
            Ok(outcome)
        }
        "RunCompleted" => {
            let mut outcome = serde_json::json!({ "kind": "RunCompleted" });
            copy_workflow_output(result, &mut outcome);
            Ok(outcome)
        }
        "ContinueAsNew" => {
            let mut outcome = serde_json::json!({ "kind": "ContinueAsNew" });
            copy_continue_as_new_input(result, &mut outcome);
            Ok(outcome)
        }
        "RunFailed" => {
            let mut outcome = serde_json::json!({
                "kind": "RunFailed",
                "error": workflow_error_or_default(result.get("error"), "workflow run failed"),
            });
            if result.get("ordinal").is_some() && result.get("name").is_some() {
                outcome["ordinal"] = serde_json::json!(required_i64(result, "ordinal")?);
                outcome["name"] = serde_json::json!(required_str(result, "name")?);
                outcome["nameOccurrence"] =
                    serde_json::json!(result.get("nameOccurrence").and_then(Value::as_i64).unwrap_or(0));
            }
            Ok(outcome)
        }
        "Sleep" => Ok(serde_json::json!({
            "kind": "Sleep",
            "ordinal": required_i64(result, "ordinal")?,
            "name": required_str(result, "name")?,
            "nameOccurrence": result.get("nameOccurrence").and_then(Value::as_i64).unwrap_or(0),
            "wakeAt": result.get("wakeAt").cloned().unwrap_or(Value::Null),
        })),
        "Wait" => Ok(serde_json::json!({
            "kind": "Wait",
            "ordinal": required_i64(result, "ordinal")?,
            "name": required_str(result, "name")?,
            "nameOccurrence": result.get("nameOccurrence").and_then(Value::as_i64).unwrap_or(0),
            "signalType": result.get("signalType")
                .cloned()
                .unwrap_or_else(|| result.get("name").cloned().unwrap_or(Value::Null)),
            "timeout": result.get("timeout").cloned().unwrap_or(Value::Null),
            "wakeAt": result.get("wakeAt").cloned().unwrap_or(Value::Null),
            "maxSignalAge": result.get("maxSignalAge").cloned().unwrap_or(Value::Null),
            "maxSignalAgeMs": result.get("maxSignalAgeMs").cloned().unwrap_or(Value::Null),
            "topic": result.get("topic").cloned().unwrap_or(Value::Null),
        })),
        "Child" => Ok(serde_json::json!({
            "kind": "Child",
            "ordinal": required_i64(result, "ordinal")?,
            "name": required_str(result, "name")?,
            "nameOccurrence": result.get("nameOccurrence").and_then(Value::as_i64).unwrap_or(0),
            "childWorkflowName": result.get("childWorkflowName")
                .cloned()
                .or_else(|| result.get("workflowName").cloned())
                .unwrap_or(Value::Null),
            "input": result.get("input").cloned().unwrap_or(Value::Null),
            "options": result.get("options").cloned().unwrap_or_else(|| serde_json::json!({})),
        })),
        "CompensationCompleted" => Ok(serde_json::json!({
            "kind": "CompensationCompleted",
            "ordinal": required_i64(result, "ordinal")?,
            "name": required_str(result, "name")?,
            "nameOccurrence": result.get("nameOccurrence").and_then(Value::as_i64).unwrap_or(0),
        })),
        "CompensationFailed" => Ok(serde_json::json!({
            "kind": "CompensationFailed",
            "ordinal": required_i64(result, "ordinal")?,
            "name": required_str(result, "name")?,
            "nameOccurrence": result.get("nameOccurrence").and_then(Value::as_i64).unwrap_or(0),
            "error": workflow_error_or_default(result.get("error"), "workflow compensator failed"),
        })),
        other => Err(format!("unknown worker workflow result kind {other:?}")),
    }
}

fn workflow_step_kind_or_run(value: &Value) -> Result<Value, String> {
    let step_kind = value
        .get("stepKind")
        .filter(|value| !value.is_null())
        .and_then(Value::as_str)
        .unwrap_or("run");
    match step_kind {
        "run" | "sideEffect" | "child" => Ok(Value::String(step_kind.to_string())),
        other => Err(format!("unknown workflow stepKind {other:?}")),
    }
}

fn copy_workflow_output(source: &Value, target: &mut Value) {
    if let Some(output_ref) = source.get("outputRef").filter(|value| !value.is_null()) {
        target["outputRef"] = output_ref.clone();
    } else {
        target["output"] = source.get("output").cloned().unwrap_or(Value::Null);
    }
}

fn copy_continue_as_new_input(source: &Value, target: &mut Value) {
    if let Some(input_ref) = source.get("inputRef").filter(|value| !value.is_null()) {
        target["inputRef"] = input_ref.clone();
    } else {
        target["input"] = source.get("input").cloned().unwrap_or(Value::Null);
    }
}

fn ensure_workflow_output(outcome: &mut Value) {
    let has_ref = outcome
        .get("outputRef")
        .is_some_and(|value| !value.is_null());
    let has_output = outcome.get("output").is_some();
    if !has_ref && !has_output {
        outcome["output"] = Value::Null;
    }
}

fn ensure_continue_as_new_input(outcome: &mut Value) {
    let has_ref = outcome
        .get("inputRef")
        .is_some_and(|value| !value.is_null());
    let has_input = outcome.get("input").is_some();
    if !has_ref && !has_input {
        outcome["input"] = Value::Null;
    }
}

fn workflow_error_or_default(error: Option<&Value>, message: &str) -> Value {
    error
        .filter(|value| !value.is_null())
        .cloned()
        .unwrap_or_else(|| serde_json::json!({"type": "Error", "message": message}))
}

fn normalize_workflow_outcomes(
    mut outcomes: Vec<Value>,
    fallback_error: Option<Value>,
) -> Result<Vec<Value>, String> {
    if outcomes.is_empty() {
        return Err("workflow outcome batch is empty".to_string());
    }
    let fallback_error = fallback_error.filter(|value| !value.is_null());
    let last = outcomes.len() - 1;
    for (idx, outcome) in outcomes.iter_mut().enumerate() {
        let kind = outcome
            .get("kind")
            .and_then(Value::as_str)
            .ok_or_else(|| "outcome missing kind".to_string())?;
        if kind != "StepCompleted" && kind != "Child" && idx != last {
            return Err("workflow suspension or terminal outcome must be the trailing batch entry".to_string());
        }
        match kind {
            "StepCompleted" => {
                let step_kind = workflow_step_kind_or_run(outcome)?;
                outcome["stepKind"] = step_kind;
                outcome["compensable"] = serde_json::json!(
                    outcome.get("compensable").and_then(Value::as_bool).unwrap_or(false)
                );
                outcome["compensationMaxAttempts"] = serde_json::json!(
                    outcome.get("compensationMaxAttempts").and_then(Value::as_i64).unwrap_or(1)
                );
                ensure_workflow_output(outcome);
            }
            "RunCompleted" => ensure_workflow_output(outcome),
            "ContinueAsNew" => ensure_continue_as_new_input(outcome),
            "RunFailed" => {
                if outcome.get("error").is_none() || outcome.get("error").is_some_and(Value::is_null) {
                    let message = if outcome.get("ordinal").is_some() && outcome.get("name").is_some() {
                        "workflow step failed"
                    } else {
                        "workflow run failed"
                    };
                    outcome["error"] = workflow_error_or_default(fallback_error.as_ref(), message);
                }
            }
            "Sleep" => {
                let wake_at = normalize_workflow_wake_at(outcome.get("wakeAt"))
                    .ok_or_else(|| "invalid sleep wakeAt".to_string())?;
                outcome["wakeAt"] = wake_at;
            }
            "Wait" => {
                let wake_at = normalize_workflow_wake_at(
                    outcome.get("wakeAt").filter(|value| !value.is_null()).or_else(|| outcome.get("timeout")),
                )
                .ok_or_else(|| "invalid wait timeout".to_string())?;
                outcome["wakeAt"] = wake_at;
                outcome["signalType"] = outcome
                    .get("signalType")
                    .cloned()
                    .unwrap_or_else(|| outcome.get("name").cloned().unwrap_or(Value::Null));
                outcome["maxSignalAgeMs"] = normalize_workflow_duration_ms(
                    outcome
                        .get("maxSignalAgeMs")
                        .filter(|value| !value.is_null())
                        .or_else(|| outcome.get("maxSignalAge")),
                )
                .ok_or_else(|| "invalid wait maxSignalAge".to_string())?;
            }
            "Child" => {
                if let Some(obj) = outcome.as_object_mut() {
                    if !obj.contains_key("childWorkflowName") {
                        if let Some(workflow_name) = obj.get("workflowName").cloned() {
                            obj.insert("childWorkflowName".to_string(), workflow_name);
                        }
                    }
                    obj.remove("workflowName");
                }
            }
            "CompensationCompleted" => {
                let _ = required_i64(outcome, "ordinal")?;
                let _ = required_str(outcome, "name")?;
                outcome["nameOccurrence"] =
                    serde_json::json!(outcome.get("nameOccurrence").and_then(Value::as_i64).unwrap_or(0));
            }
            "CompensationFailed" => {
                let _ = required_i64(outcome, "ordinal")?;
                let _ = required_str(outcome, "name")?;
                outcome["nameOccurrence"] =
                    serde_json::json!(outcome.get("nameOccurrence").and_then(Value::as_i64).unwrap_or(0));
                if outcome.get("error").is_none() || outcome.get("error").is_some_and(Value::is_null) {
                    outcome["error"] = workflow_error_or_default(fallback_error.as_ref(), "workflow compensator failed");
                }
            }
            other => return Err(format!("unknown worker workflow outcome kind {other:?}")),
        }
    }
    Ok(outcomes)
}

fn legacy_step_result_to_outcomes(result: &Value) -> Result<Vec<Value>, String> {
    let mut outcomes = Vec::new();
    let mut failed_checkpoint_encoded = false;
    for checkpoint in result
        .get("checkpoints")
        .and_then(Value::as_array)
        .ok_or_else(|| "legacy StepResult missing checkpoints".to_string())?
    {
        let kind = checkpoint.get("kind").and_then(Value::as_str).unwrap_or_default();
        let state = checkpoint.get("state").and_then(Value::as_str).unwrap_or_default();
        match (kind, state) {
            ("run" | "sideEffect", "completed") => {
                let compensable = checkpoint
                    .get("compensable")
                    .and_then(Value::as_bool)
                    .unwrap_or_else(|| {
                        checkpoint
                            .get("compensationState")
                            .and_then(Value::as_str)
                            .is_some_and(|state| {
                                matches!(state, "pending" | "running" | "completed" | "failed")
                            })
                    });
                let mut outcome = serde_json::json!({
                    "kind": "StepCompleted",
                    "ordinal": required_i64(checkpoint, "ordinal")?,
                    "name": required_str(checkpoint, "name")?,
                    "nameOccurrence": checkpoint.get("nameOccurrence").and_then(Value::as_i64).unwrap_or(0),
                    "stepKind": kind,
                    "compensable": compensable,
                    "compensationMaxAttempts": checkpoint
                        .get("compensationMaxAttempts")
                        .and_then(Value::as_i64)
                        .unwrap_or(1),
                });
                copy_workflow_output(checkpoint, &mut outcome);
                outcomes.push(outcome);
            }
            ("run", "failed") => {
                failed_checkpoint_encoded = true;
                outcomes.push(serde_json::json!({
                    "kind": "RunFailed",
                    "ordinal": required_i64(checkpoint, "ordinal")?,
                    "name": required_str(checkpoint, "name")?,
                    "nameOccurrence": checkpoint.get("nameOccurrence").and_then(Value::as_i64).unwrap_or(0),
                    "error": checkpoint.get("error").cloned().unwrap_or_else(|| {
                        serde_json::json!({"type": "Error", "message": "workflow step failed"})
                    }),
                }));
            }
            ("sleep", "running") => outcomes.push(serde_json::json!({
                "kind": "Sleep",
                "ordinal": required_i64(checkpoint, "ordinal")?,
                "name": required_str(checkpoint, "name")?,
                "nameOccurrence": checkpoint.get("nameOccurrence").and_then(Value::as_i64).unwrap_or(0),
                "wakeAt": checkpoint.get("wakeAt").cloned().unwrap_or(Value::Null),
            })),
            ("wait_signal", "running") => outcomes.push(serde_json::json!({
                "kind": "Wait",
                "ordinal": required_i64(checkpoint, "ordinal")?,
                "name": required_str(checkpoint, "name")?,
                "nameOccurrence": checkpoint.get("nameOccurrence").and_then(Value::as_i64).unwrap_or(0),
                "wakeAt": checkpoint.get("wakeAt").cloned().unwrap_or(Value::Null),
                "signalType": checkpoint.get("signalType").cloned().unwrap_or(Value::Null),
                "maxSignalAgeMs": checkpoint.get("maxSignalAgeMs").cloned().unwrap_or(Value::Null),
                "consumedSignalId": checkpoint.get("consumedSignalId").cloned().unwrap_or(Value::Null),
            })),
            _ => {}
        }
    }

    match result.pointer("/runUpdate/state").and_then(Value::as_str) {
        Some("completed") => outcomes.push(serde_json::json!({
            "kind": "RunCompleted",
            "output": result.pointer("/runUpdate/output").cloned().unwrap_or(Value::Null),
        })),
        Some("failed") if !failed_checkpoint_encoded => outcomes.push(serde_json::json!({
            "kind": "RunFailed",
            "error": result.pointer("/runUpdate/error").cloned().unwrap_or_else(|| {
                serde_json::json!({"type": "Error", "message": "workflow run failed"})
            }),
        })),
        _ => {}
    }

    normalize_workflow_outcomes(outcomes, None)
}

fn required_i64(value: &Value, key: &str) -> Result<i64, String> {
    value
        .get(key)
        .and_then(Value::as_i64)
        .ok_or_else(|| format!("missing {key}"))
}

fn required_str<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing {key}"))
}

fn normalize_workflow_step_result(mut result: Value) -> Result<Value, String> {
    let state = result
        .pointer("/runUpdate/state")
        .and_then(Value::as_str)
        .unwrap_or_default();

    if state == "sleeping" {
        let wake_at = normalize_workflow_wake_at(result.pointer("/runUpdate/wakeAt"))
            .filter(|value| !value.is_null())
            .or_else(|| {
                result
                    .get("checkpoints")
                    .and_then(Value::as_array)
                    .and_then(|checkpoints| {
                        checkpoints.iter().find_map(|checkpoint| {
                            let is_sleep =
                                checkpoint.get("kind").and_then(Value::as_str) == Some("sleep");
                            let is_running =
                                checkpoint.get("state").and_then(Value::as_str) == Some("running");
                            (is_sleep && is_running)
                                .then(|| normalize_workflow_wake_at(checkpoint.get("wakeAt")))
                                .flatten()
                                .filter(|value| !value.is_null())
                        })
                    })
            })
            .ok_or_else(|| "sleeping workflow StepResult missing wakeAt".to_string())?;

        if let Some(update) = result.get_mut("runUpdate").and_then(Value::as_object_mut) {
            update.insert("wakeAt".to_string(), wake_at.clone());
        }
        if let Some(checkpoints) = result.get_mut("checkpoints").and_then(Value::as_array_mut) {
            for checkpoint in checkpoints {
                let is_sleep = checkpoint.get("kind").and_then(Value::as_str) == Some("sleep");
                let is_running = checkpoint.get("state").and_then(Value::as_str) == Some("running");
                if is_sleep && is_running {
                    checkpoint["wakeAt"] = wake_at.clone();
                }
            }
        }
    } else if state == "waiting" {
        let wake_at = normalize_workflow_wake_at(result.pointer("/runUpdate/wakeAt"))
            .ok_or_else(|| "invalid waiting workflow wakeAt".to_string())?;
        if let Some(update) = result.get_mut("runUpdate").and_then(Value::as_object_mut) {
            update.insert("wakeAt".to_string(), wake_at.clone());
        }
        if let Some(checkpoints) = result.get_mut("checkpoints").and_then(Value::as_array_mut) {
            for checkpoint in checkpoints {
                let is_wait =
                    checkpoint.get("kind").and_then(Value::as_str) == Some("wait_signal");
                let is_running = checkpoint.get("state").and_then(Value::as_str) == Some("running");
                if is_wait && is_running {
                    checkpoint["wakeAt"] = wake_at.clone();
                    checkpoint["maxSignalAgeMs"] =
                        normalize_workflow_duration_ms(checkpoint.get("maxSignalAgeMs"))
                            .ok_or_else(|| "invalid wait maxSignalAgeMs".to_string())?;
                }
            }
        }
    }

    Ok(result)
}

fn normalize_workflow_wake_at(raw: Option<&Value>) -> Option<Value> {
    let value = raw?;
    if value.is_null() {
        return Some(Value::Null);
    }
    let Some(s) = value.as_str() else {
        return Some(value.clone());
    };
    if DateTime::parse_from_rfc3339(s).is_ok() {
        return Some(Value::String(s.to_string()));
    }
    let ms = parse_workflow_duration_ms(s)?;
    let wake_at = Utc::now() + chrono::Duration::milliseconds(ms);
    Some(Value::String(wake_at.to_rfc3339()))
}

fn normalize_workflow_duration_ms(raw: Option<&Value>) -> Option<Value> {
    let Some(value) = raw else {
        return Some(Value::Null);
    };
    if value.is_null() {
        return Some(Value::Null);
    }
    if let Some(ms) = value.as_i64() {
        return Some(Value::Number(ms.into()));
    }
    let s = value.as_str()?;
    let ms = parse_workflow_duration_ms(s)?;
    Some(Value::Number(ms.into()))
}

/// Parse a workflow duration string to milliseconds.
///
/// Accepts BOTH forms, because both reach here from creator code:
///
/// - the suffix form `docs/reference/workflows.md` documents -- "Duration
///   strings accepted by workflow sleeps and timeouts include suffixes such as
///   `ms`, `s`, `m`, `h`, and `d`; plain positive numbers are milliseconds";
/// - ISO-8601 (`PT1.5S`), which the journal round-trips.
///
/// It used to accept only ISO-8601. `step.sleep("nap", "1500ms")` -- the exact
/// spelling the reference doc shows -- therefore produced `invalid sleep
/// wakeAt` on the deployed path, and the control plane nacked the advance as
/// `Invalid` and left the run wedged in `running` forever, with no terminal
/// state and nothing surfaced to the app. `step.waitForSignal(..., { timeout:
/// "1h" })` failed the same way through `normalize_workflow_wake_at`'s `Wait`
/// arm.
///
/// The local dev engine already accepted both (`dev.rs`
/// `parse_workflow_duration_ms`), so the documented spelling worked under
/// `pnpm dev` and hung deployed -- found by walking both sides in
/// `tests/e2e_dev_vs_deployed_workflows.sh`.
fn parse_workflow_duration_ms(raw: &str) -> Option<i64> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    parse_iso8601_duration_ms(trimmed).or_else(|| parse_suffix_duration_ms(trimmed))
}

/// Suffix durations (`500ms`, `1.5s`, `10m`, `2h`, `1d`) and bare milliseconds.
///
/// `ms` must be tried before `s`, and the order of the rest does not matter.
/// `strip_suffix` only matches at the END of the string, so `"500ms"` can
/// never be caught by `m` -- it does not end in `m`. It IS caught by `s`,
/// leaving `"500m"`, which is not a number, and the `?` below then abandons
/// the whole parse rather than trying another unit. The result is `None`, so
/// the failure is a rejected duration and a run that never wakes, not a
/// mis-scaled one.
///
/// Verified by mutation, because the two orderings are not equally load
/// bearing: swapping `ms` and `m` leaves every test passing, while moving `s`
/// ahead of `ms` fails with `1500ms did not normalize` and
/// `left: None, right: Some(500)`.
fn parse_suffix_duration_ms(raw: &str) -> Option<i64> {
    const UNITS: [(&str, f64); 5] = [
        ("ms", 1.0),
        ("s", 1_000.0),
        ("m", 60_000.0),
        ("h", 3_600_000.0),
        ("d", 86_400_000.0),
    ];
    for (suffix, multiplier) in UNITS {
        let Some(number) = raw.strip_suffix(suffix) else {
            continue;
        };
        let value = parse_duration_number(number)?;
        let ms = value * multiplier;
        if ms < 0.0 || !ms.is_finite() {
            return None;
        }
        return Some(ms.ceil() as i64);
    }
    raw.parse::<i64>().ok().filter(|v| *v >= 0)
}

fn parse_iso8601_duration_ms(raw: &str) -> Option<i64> {
    let s = raw.strip_prefix('P')?;
    let (date_part, time_part) = match s.split_once('T') {
        Some((date, time)) => (date, time),
        None => (s, ""),
    };
    let mut total_ms = 0_i64;
    let mut num = String::new();
    for ch in date_part.chars() {
        if ch.is_ascii_digit() || ch == '.' {
            num.push(ch);
            continue;
        }
        let value = parse_duration_number(&num)?;
        num.clear();
        match ch {
            'D' => total_ms = total_ms.checked_add((value * 86_400_000.0).round() as i64)?,
            _ => return None,
        }
    }
    if !num.is_empty() {
        return None;
    }
    for ch in time_part.chars() {
        if ch.is_ascii_digit() || ch == '.' {
            num.push(ch);
            continue;
        }
        let value = parse_duration_number(&num)?;
        num.clear();
        match ch {
            'H' => total_ms = total_ms.checked_add((value * 3_600_000.0).round() as i64)?,
            'M' => total_ms = total_ms.checked_add((value * 60_000.0).round() as i64)?,
            'S' => total_ms = total_ms.checked_add((value * 1_000.0).round() as i64)?,
            _ => return None,
        }
    }
    if !num.is_empty() {
        return None;
    }
    Some(total_ms.max(0))
}

fn parse_duration_number(raw: &str) -> Option<f64> {
    if raw.is_empty() {
        return None;
    }
    let value = raw.parse::<f64>().ok()?;
    value.is_finite().then_some(value)
}

#[allow(clippy::future_not_send)]
pub async fn collect_post_apply_registrations(
    db_url: &str,
    app_id: Uuid,
    run_id: &str,
    family: bool,
) -> Result<Vec<WorkflowAdvanceRegistration>, WorkflowError> {
    let conn = open_conn(db_url).await?;
    collect_post_apply_registrations_on_conn(&conn, app_id, run_id, family).await
}

pub async fn collect_post_apply_registrations_on_conn<C>(
    conn: &C,
    app_id: Uuid,
    run_id: &str,
    family: bool,
) -> Result<Vec<WorkflowAdvanceRegistration>, WorkflowError>
where
    C: GenericClient + Sync,
{
    let tables = WorkflowTables::for_app_id(&app_id);
    if family {
        collect_family_registrations(conn, &tables, run_id).await
    } else {
        collect_single_registration(conn, &tables, run_id).await
    }
}

#[allow(clippy::future_not_send)]
async fn open_conn(url: &str) -> Result<Client, WorkflowError> {
    let (client, connection) = compio_postgres::connect(url, NoTls).await?;
    compio::runtime::spawn(async move {
        if let Err(e) = connection.run().await {
            tracing::error!(error = %e, "workflow advance pg connection error");
        }
    })
    .detach();
    Ok(client)
}

async fn collect_single_registration<C>(
    conn: &C,
    tables: &WorkflowTables,
    run_id: &str,
) -> Result<Vec<WorkflowAdvanceRegistration>, WorkflowError>
where
    C: GenericClient + Sync,
{
    let rows = conn
        .query(
            &format!(
                "SELECT id, app_id, state, wake_at, cancel_requested, claimed_by, dispatch_nonce, waiting_step_key \
                   FROM {} \
                  WHERE id = $1",
                tables.runs
            ),
            &[&run_id],
        )
        .await?;
    let Some(row) = rows.first() else {
        return Ok(vec![WorkflowAdvanceRegistration::terminal(run_id, tables.app_id)]);
    };
    Ok(vec![registration_for_row(conn, tables, row).await?])
}

async fn collect_family_registrations<C>(
    conn: &C,
    tables: &WorkflowTables,
    run_id: &str,
) -> Result<Vec<WorkflowAdvanceRegistration>, WorkflowError>
where
    C: GenericClient + Sync,
{
    let sql = format!(
        "WITH RECURSIVE ancestors AS ( \
                 SELECT id, parent_run_id \
                   FROM {runs} \
                  WHERE id = $1 \
                 UNION ALL \
                 SELECT p.id, p.parent_run_id \
                   FROM {runs} p \
                   JOIN ancestors a ON a.parent_run_id = p.id \
             ), root AS ( \
                 SELECT id \
                   FROM ancestors \
                  WHERE parent_run_id IS NULL \
                  LIMIT 1 \
             ), family AS ( \
                 SELECT id, app_id, state, wake_at, cancel_requested, claimed_by, dispatch_nonce, waiting_step_key, tree_depth \
                   FROM {runs} \
                  WHERE id = (SELECT id FROM root) \
                 UNION ALL \
                 SELECT c.id, c.app_id, c.state, c.wake_at, c.cancel_requested, c.claimed_by, c.dispatch_nonce, c.waiting_step_key, c.tree_depth \
                   FROM {runs} c \
                   JOIN family f ON c.parent_run_id = f.id \
             ) \
             SELECT id, app_id, state, wake_at, cancel_requested, claimed_by, dispatch_nonce, waiting_step_key \
               FROM family \
              ORDER BY tree_depth, id",
        runs = tables.runs
    );
    let rows = conn.query(&sql, &[&run_id]).await?;
    if rows.is_empty() {
        return Ok(vec![WorkflowAdvanceRegistration::terminal(run_id, tables.app_id)]);
    }

    let mut seen = HashSet::new();
    let mut registrations = Vec::new();
    for row in &rows {
        let registration = registration_for_row(conn, tables, row).await?;
        seen.insert(registration.run_id.clone());
        registrations.push(registration);
    }
    collect_parent_after_child_apply(conn, tables, run_id, &mut seen, &mut registrations).await?;
    collect_continued_as_new_successor(conn, tables, run_id, &mut seen, &mut registrations)
        .await?;
    Ok(registrations)
}

async fn collect_parent_after_child_apply<C>(
    conn: &C,
    tables: &WorkflowTables,
    child_run_id: &str,
    seen: &mut HashSet<String>,
    registrations: &mut Vec<WorkflowAdvanceRegistration>,
) -> Result<(), WorkflowError>
where
    C: GenericClient + Sync,
{
    let rows = conn
        .query(
            &format!(
                "SELECT parent_run_id \
                   FROM {} \
                  WHERE id = $1 \
                    AND parent_run_id IS NOT NULL",
                tables.runs
            ),
            &[&child_run_id],
        )
        .await?;
    let Some(row) = rows.first() else {
        return Ok(());
    };
    let parent_run_id: String = row.get("parent_run_id");
    if seen.contains(&parent_run_id) {
        return Ok(());
    }

    let parent_rows = conn
        .query(
            &format!(
                "SELECT id, app_id, state, wake_at, cancel_requested, claimed_by, dispatch_nonce, waiting_step_key \
                   FROM {} \
                  WHERE id = $1",
                tables.runs
            ),
            &[&parent_run_id],
        )
        .await?;
    if let Some(row) = parent_rows.first() {
        let registration = registration_for_row(conn, tables, row).await?;
        seen.insert(registration.run_id.clone());
        registrations.push(registration);
    }
    Ok(())
}

async fn collect_continued_as_new_successor<C>(
    conn: &C,
    tables: &WorkflowTables,
    run_id: &str,
    seen: &mut HashSet<String>,
    registrations: &mut Vec<WorkflowAdvanceRegistration>,
) -> Result<(), WorkflowError>
where
    C: GenericClient + Sync,
{
    let rows = conn
        .query(
            &format!(
                "SELECT continued_as_new_run_id \
                   FROM {} \
                  WHERE id = $1 \
                    AND continued_as_new_run_id IS NOT NULL",
                tables.runs
            ),
            &[&run_id],
        )
        .await?;
    let Some(row) = rows.first() else {
        return Ok(());
    };
    let successor_run_id: String = row.get("continued_as_new_run_id");
    if seen.contains(&successor_run_id) {
        return Ok(());
    }

    let successor_rows = conn
        .query(
            &format!(
                "SELECT id, app_id, state, wake_at, cancel_requested, claimed_by, dispatch_nonce, waiting_step_key \
                   FROM {} \
                  WHERE id = $1",
                tables.runs
            ),
            &[&successor_run_id],
        )
        .await?;
    if let Some(row) = successor_rows.first() {
        let registration = registration_for_row(conn, tables, row).await?;
        seen.insert(registration.run_id.clone());
        registrations.push(registration);
    }
    Ok(())
}

async fn registration_for_row<C>(
    conn: &C,
    tables: &WorkflowTables,
    row: &Row,
) -> Result<WorkflowAdvanceRegistration, WorkflowError>
where
    C: GenericClient + Sync,
{
    let run_id: String = row.get("id");
    let app_id: Uuid = row.get("app_id");
    let state: String = row.get("state");
    let mut wake_at: Option<DateTime<Utc>> = row.get("wake_at");
    let cancel_requested: bool = row.get("cancel_requested");
    let waiting_step_key: Option<String> = row.get("waiting_step_key");

    if is_schedulable_state(&state) {
        if cancel_requested {
            return Ok(WorkflowAdvanceRegistration::next(run_id, app_id, Utc::now()));
        }
        if state == "waiting" && wake_at.is_none() {
            wake_at =
                rearm_waiting_run_if_pending_signal(conn, tables, &run_id, waiting_step_key.as_deref())
                    .await?;
        }
        if let Some(wake_at) = wake_at {
            return Ok(WorkflowAdvanceRegistration::next(run_id, app_id, wake_at));
        }
        if state == "waiting"
            && waiting_run_has_live_resume_source(conn, tables, &run_id, waiting_step_key.as_deref()).await?
        {
            return Ok(WorkflowAdvanceRegistration::preserve(run_id, app_id));
        }
    }

    Ok(WorkflowAdvanceRegistration::terminal(run_id, app_id))
}

fn is_schedulable_state(state: &str) -> bool {
    matches!(
        state,
        "queued" | "running" | "sleeping" | "waiting" | "compensating"
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum WaitingStep {
    Sleep,
    WaitSignal {
        ordinal: i32,
        name: String,
        signal_type: String,
    },
    Child,
}

fn parse_waiting_step_key(key: &str) -> Result<WaitingStep, WorkflowError> {
    let parts: Vec<&str> = key.split(':').collect();
    match parts.as_slice() {
        ["sleep", ordinal, _name] => {
            let _ = ordinal.parse::<i32>().map_err(|_| {
                WorkflowError::Invalid(format!("invalid sleep waiting_step_key ordinal: {key}"))
            })?;
            Ok(WaitingStep::Sleep)
        }
        ["wait", ordinal, name, signal_type] | ["wait", ordinal, name, signal_type, _] => {
            let ordinal = ordinal.parse::<i32>().map_err(|_| {
                WorkflowError::Invalid(format!("invalid wait waiting_step_key ordinal: {key}"))
            })?;
            Ok(WaitingStep::WaitSignal {
                ordinal,
                name: (*name).to_string(),
                signal_type: (*signal_type).to_string(),
            })
        }
        ["child", ordinal, _name] => {
            let _ = ordinal.parse::<i32>().map_err(|_| {
                WorkflowError::Invalid(format!("invalid child waiting_step_key ordinal: {key}"))
            })?;
            Ok(WaitingStep::Child)
        }
        _ => Err(WorkflowError::Invalid(format!(
            "unrecognized waiting_step_key: {key}"
        ))),
    }
}

async fn waiting_run_has_live_resume_source<C>(
    conn: &C,
    tables: &WorkflowTables,
    run_id: &str,
    waiting_step_key: Option<&str>,
) -> Result<bool, WorkflowError>
where
    C: GenericClient + Sync,
{
    match waiting_step_key {
        Some(key) => match parse_waiting_step_key(key)? {
            WaitingStep::Sleep => Ok(false),
            WaitingStep::Child => waiting_run_has_running_step(conn, tables, run_id, "child").await,
            WaitingStep::WaitSignal { ordinal, name, .. } => {
                let rows = conn
                    .query(
                        &format!(
                            "SELECT 1 \
                               FROM {} \
                              WHERE run_id = $1 \
                                AND ordinal = $2 \
                                AND name = $3 \
                                AND kind = 'wait_signal' \
                                AND state = 'running' \
                              LIMIT 1",
                            tables.steps
                        ),
                        &[&run_id, &ordinal, &name],
                    )
                    .await?;
                if !rows.is_empty() {
                    return Ok(true);
                }
                waiting_run_has_subscription(conn, tables, run_id, Some(ordinal)).await
            }
        },
        None => waiting_run_has_running_resume_step_or_subscription(conn, tables, run_id).await,
    }
}

async fn waiting_run_has_running_resume_step_or_subscription<C>(
    conn: &C,
    tables: &WorkflowTables,
    run_id: &str,
) -> Result<bool, WorkflowError>
where
    C: GenericClient + Sync,
{
    let rows = conn
        .query(
            &format!(
                "SELECT 1 \
                   FROM {} \
                  WHERE run_id = $1 \
                    AND kind IN ('child', 'wait_signal') \
                    AND state = 'running' \
                  LIMIT 1",
                tables.steps
            ),
            &[&run_id],
        )
        .await?;
    if !rows.is_empty() {
        return Ok(true);
    }
    waiting_run_has_subscription(conn, tables, run_id, None).await
}

async fn waiting_run_has_running_step<C>(
    conn: &C,
    tables: &WorkflowTables,
    run_id: &str,
    kind: &str,
) -> Result<bool, WorkflowError>
where
    C: GenericClient + Sync,
{
    let rows = conn
        .query(
            &format!(
                "SELECT 1 \
                   FROM {} \
                  WHERE run_id = $1 \
                    AND kind = $2 \
                    AND state = 'running' \
                  LIMIT 1",
                tables.steps
            ),
            &[&run_id, &kind],
        )
        .await?;
    Ok(!rows.is_empty())
}

async fn waiting_run_has_subscription<C>(
    conn: &C,
    tables: &WorkflowTables,
    run_id: &str,
    ordinal: Option<i32>,
) -> Result<bool, WorkflowError>
where
    C: GenericClient + Sync,
{
    let rows = conn
        .query(
            &format!(
                "SELECT 1 \
                   FROM {} \
                  WHERE run_id = $1 \
                    AND ($2::integer IS NULL OR ordinal = $2) \
                  LIMIT 1",
                tables.subscriptions
            ),
            &[&run_id, &ordinal],
        )
        .await?;
    Ok(!rows.is_empty())
}

async fn rearm_waiting_run_if_pending_signal<C>(
    conn: &C,
    tables: &WorkflowTables,
    run_id: &str,
    waiting_step_key: Option<&str>,
) -> Result<Option<DateTime<Utc>>, WorkflowError>
where
    C: GenericClient + Sync,
{
    let Some(key) = waiting_step_key else {
        return Ok(None);
    };
    let pending = match parse_waiting_step_key(key)? {
        WaitingStep::Sleep => false,
        WaitingStep::WaitSignal { signal_type, .. } => {
            let rows = conn
                .query(
                    &format!(
                        "SELECT id \
                           FROM {} \
                          WHERE run_id = $1 \
                            AND type = $2 \
                            AND consumed_by IS NULL \
                          LIMIT 1",
                        tables.signals
                    ),
                    &[&run_id, &signal_type],
                )
                .await?;
            !rows.is_empty()
        }
        WaitingStep::Child => {
            let rows = conn
                .query(
                    &format!(
                        "SELECT sig.id \
                           FROM {} s \
                           JOIN {} sig \
                             ON sig.run_id = s.run_id \
                            AND sig.type = s.signal_type \
                            AND sig.consumed_by IS NULL \
                          WHERE s.run_id = $1 \
                            AND s.kind = 'child' \
                            AND s.state = 'running' \
                          LIMIT 1",
                        tables.steps, tables.signals
                    ),
                    &[&run_id],
                )
                .await?;
            !rows.is_empty()
        }
    };
    if !pending {
        return Ok(None);
    }

    let wake_at = Utc::now();
    let changed = conn
        .execute(
            &format!(
                "UPDATE {} \
                    SET wake_at = $2 \
                  WHERE id = $1 \
                    AND state = 'waiting' \
                    AND wake_at IS NULL",
                tables.runs
            ),
            &[&run_id, &wake_at],
        )
        .await?;
    if changed > 0 {
        Ok(Some(wake_at))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::StepOutcome;

    #[test]
    fn continue_as_new_worker_result_round_trips_to_step_outcome() {
        let result = worker_value_to_step_result(
            "run_test",
            "wfd_test",
            serde_json::json!({
                "kind": "ContinueAsNew",
                "runId": "run_test",
                "nonce": "wfd_test",
                "workflowName": "Checkout",
                "input": {"generation": 1}
            }),
        )
        .expect("continue-as-new result");

        assert_eq!(result.outcomes.len(), 1);
        match &result.outcomes[0] {
            StepOutcome::ContinueAsNew { input, input_ref } => {
                assert_eq!(input.as_ref(), Some(&serde_json::json!({"generation": 1})));
                assert!(input_ref.is_none());
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    /// The suffix durations `docs/reference/workflows.md` documents must reach
    /// a wakeAt on the DEPLOYED path, not only in the dev engine.
    ///
    /// Before the fix this arm accepted ISO-8601 only, so `step.sleep("nap",
    /// "1500ms")` normalised to `None`, the worker nacked the advance with
    /// `invalid sleep wakeAt`, and the run sat in `running` forever. Deleting
    /// the `parse_suffix_duration_ms` fallback fails every case below except
    /// `PT1.5S`.
    ///
    /// What this does NOT catch: it exercises the parser, not the wiring. If a
    /// caller stopped routing through `normalize_workflow_wake_at`, or the JS
    /// side started sending a different field, this still passes. That path is
    /// covered by `tests/e2e_dev_vs_deployed_workflows.sh`, which drives a real
    /// `step.sleep` through a deployed app.
    #[test]
    fn documented_suffix_durations_normalize_to_a_wake_at() {
        for spelling in ["1500ms", "1.5s", "2m", "1h", "1d", "PT1.5S", "750"] {
            let wake = normalize_workflow_wake_at(Some(&serde_json::json!(spelling)));
            let wake = wake.unwrap_or_else(|| panic!("{spelling} did not normalize"));
            let text = wake.as_str().unwrap_or_else(|| panic!("{spelling} was not a string"));
            DateTime::parse_from_rfc3339(text)
                .unwrap_or_else(|e| panic!("{spelling} produced a non-RFC3339 wakeAt: {e}"));
        }
    }

    /// `ms` must be matched before `m`, or `"500ms"` leaves a `"500"` remainder
    /// on the `m` arm and silently becomes 500 MINUTES -- a run that sleeps for
    /// eight hours instead of half a second, with nothing to show for it.
    #[test]
    fn ms_is_not_parsed_as_minutes() {
        assert_eq!(parse_workflow_duration_ms("500ms"), Some(500));
        assert_eq!(parse_workflow_duration_ms("500m"), Some(30_000_000));
    }

    #[test]
    fn a_non_duration_string_is_still_rejected() {
        assert_eq!(parse_workflow_duration_ms("soon"), None);
        assert_eq!(parse_workflow_duration_ms(""), None);
        assert_eq!(parse_workflow_duration_ms("ms"), None);
        assert!(normalize_workflow_wake_at(Some(&serde_json::json!("soon"))).is_none());
    }
}
