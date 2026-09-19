//! Execution data shared by native hosts and V8 adapters.
//!
//! Claims and lease credentials stay with the host. An executor receives replay
//! data and returns outcomes; the host attaches the claim when applying them.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::engine::{JournalStep, StepOutcome, StepResult};
use crate::WorkflowServiceError;

/// Replay input without journal mutation authority.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowInvocation {
    pub app_id: String,
    pub deploy_id: String,
    pub deploy_hash: String,
    pub run_id: String,
    /// Scopes step identity to one attempt at the run. A restart copies the
    /// retained prefix under a new generation and re-executes the rest at the
    /// same ordinals, so step idempotency keys must carry it.
    pub generation: i64,
    pub workflow_name: String,
    pub phase: String,
    pub trigger: WorkflowTrigger,
    pub journal: Vec<JournalStep>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowTrigger {
    pub input: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_ref: Option<crate::engine::WorkflowOutputRef>,
    pub started_at: DateTime<Utc>,
    pub run_id: String,
    pub workflow_name: String,
}

/// The executor reports work, without choosing the run or lease to mutate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowExecution {
    pub outcomes: Vec<StepOutcome>,
}

impl WorkflowExecution {
    /// Decode the runtime's outcome batch and normalize SDK duration values.
    ///
    /// # Errors
    /// Rejects malformed JSON and invalid or empty outcome batches.
    pub fn from_runtime_json(json: &str) -> Result<Self, WorkflowServiceError> {
        let value = serde_json::from_str(json).map_err(|e| {
            WorkflowServiceError::InvalidRequest(format!("invalid workflow execution result: {e}"))
        })?;
        Self::from_runtime_value(value)
    }

    /// # Errors
    /// Rejects malformed or empty outcome batches.
    pub fn from_runtime_value(value: Value) -> Result<Self, WorkflowServiceError> {
        let outcomes = decode_runtime_outcomes(value)?;
        let outcomes = serde_json::from_value(Value::Array(outcomes)).map_err(|e| {
            WorkflowServiceError::InvalidRequest(format!("invalid workflow outcomes: {e}"))
        })?;
        Ok(Self { outcomes })
    }

    /// Bind outcomes to the host's claim, then fold them into journal changes.
    ///
    /// # Errors
    /// Rejects invalid outcome ordering or an empty completion.
    pub fn into_step_result(
        self,
        run_id: String,
        dispatch_nonce: String,
    ) -> Result<StepResult, WorkflowServiceError> {
        if self.outcomes.is_empty() {
            return Err(WorkflowServiceError::InvalidRequest(
                "workflow outcome batch is empty".into(),
            ));
        }
        StepResult::from_outcomes(run_id, dispatch_nonce, self.outcomes)
            .map_err(WorkflowServiceError::InvalidRequest)
    }
}

pub(crate) fn decode_runtime_outcomes(
    mut value: Value,
) -> Result<Vec<Value>, WorkflowServiceError> {
    let error = value.get_mut("error").map(Value::take);
    let outcomes = value.get_mut("outcomes").map(Value::take);
    let Some(Value::Array(outcomes)) = outcomes else {
        return Err(WorkflowServiceError::InvalidRequest(
            "workflow execution requires an outcome batch".into(),
        ));
    };
    normalize_workflow_outcomes(outcomes, error).map_err(WorkflowServiceError::InvalidRequest)
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
            return Err(
                "workflow suspension or terminal outcome must be the trailing batch entry"
                    .to_string(),
            );
        }
        match kind {
            "StepCompleted" => {
                let step_kind = workflow_step_kind_or_run(outcome)?;
                outcome["stepKind"] = step_kind;
                outcome["compensable"] = serde_json::json!(outcome
                    .get("compensable")
                    .and_then(Value::as_bool)
                    .unwrap_or(false));
                outcome["compensationMaxAttempts"] = serde_json::json!(outcome
                    .get("compensationMaxAttempts")
                    .and_then(Value::as_i64)
                    .unwrap_or(1));
                ensure_workflow_output(outcome);
            }
            "RunCompleted" => ensure_workflow_output(outcome),
            "ContinueAsNew" => ensure_continue_as_new_input(outcome),
            "RunFailed" => {
                if outcome.get("error").is_none()
                    || outcome.get("error").is_some_and(Value::is_null)
                {
                    let message =
                        if outcome.get("ordinal").is_some() && outcome.get("name").is_some() {
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
                    outcome
                        .get("wakeAt")
                        .filter(|value| !value.is_null())
                        .or_else(|| outcome.get("timeout")),
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
                outcome["nameOccurrence"] = serde_json::json!(outcome
                    .get("nameOccurrence")
                    .and_then(Value::as_i64)
                    .unwrap_or(0));
            }
            "CompensationFailed" => {
                let _ = required_i64(outcome, "ordinal")?;
                let _ = required_str(outcome, "name")?;
                outcome["nameOccurrence"] = serde_json::json!(outcome
                    .get("nameOccurrence")
                    .and_then(Value::as_i64)
                    .unwrap_or(0));
                if outcome.get("error").is_none()
                    || outcome.get("error").is_some_and(Value::is_null)
                {
                    outcome["error"] = workflow_error_or_default(
                        fallback_error.as_ref(),
                        "workflow compensator failed",
                    );
                }
            }
            other => return Err(format!("unknown worker workflow outcome kind {other:?}")),
        }
    }
    Ok(outcomes)
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

fn normalize_workflow_wake_at(raw: Option<&Value>) -> Option<Value> {
    let Some(value) = raw else {
        return Some(Value::Null);
    };
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
    let duration = chrono::Duration::try_milliseconds(ms)?;
    let wake_at = Utc::now().checked_add_signed(duration)?;
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

/// Accept the duration spellings emitted by the SDK and journal replay.
fn parse_workflow_duration_ms(raw: &str) -> Option<i64> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    parse_iso8601_duration_ms(trimmed).or_else(|| parse_suffix_duration_ms(trimmed))
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::StepOutcome;

    #[test]
    fn continue_as_new_worker_result_round_trips_to_step_outcome() {
        let result = WorkflowExecution::from_runtime_value(serde_json::json!({ "outcomes": [{
                "kind": "ContinueAsNew",
                "runId": "run_test",
                "nonce": "wfd_test",
                "workflowName": "Checkout",
                "input": {"generation": 1}
            }] }))
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

    // Exercise the spelling contract; V8 binding tests cover runtime decoding.
    #[test]
    fn documented_suffix_durations_normalize_to_a_wake_at() {
        for spelling in ["1500ms", "1.5s", "2m", "1h", "1d", "PT1.5S", "750"] {
            let wake = normalize_workflow_wake_at(Some(&serde_json::json!(spelling)));
            let wake = wake.unwrap_or_else(|| panic!("{spelling} did not normalize"));
            let text = wake
                .as_str()
                .unwrap_or_else(|| panic!("{spelling} was not a string"));
            DateTime::parse_from_rfc3339(text)
                .unwrap_or_else(|e| panic!("{spelling} produced a non-RFC3339 wakeAt: {e}"));
        }
    }

    // Match the most specific suffix before its prefix.
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
