//! Pure durable-workflow fold and DTO contracts.

use chrono::{DateTime, Utc};
use serde::{de, Deserialize, Deserializer, Serialize};
use serde_json::Value;

pub const DEFAULT_TICK_SECS: u64 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JournalStep {
    pub ordinal: i32,
    pub name: String,
    #[serde(default, rename = "nameOccurrence")]
    pub name_occurrence: i32,
    pub kind: String,
    pub state: String,
    pub output: Option<Value>,
    #[serde(default, rename = "outputRef")]
    pub output_ref: Option<WorkflowOutputRef>,
    pub error: Option<Value>,
    #[serde(default, rename = "childRunId")]
    pub child_run_id: Option<String>,
    #[serde(default, rename = "compensationState")]
    pub compensation_state: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowOutputRef {
    pub hash: String,
    pub size: i64,
    #[serde(default)]
    pub content_type: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChildWorkflowOptions {
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub cascade: bool,
    #[serde(default, deserialize_with = "deserialize_optional_wake_duration")]
    pub timeout: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StepCheckpoint {
    pub ordinal: i32,
    pub name: String,
    pub name_occurrence: i32,
    pub kind: String,
    pub state: String,
    pub output: Option<Value>,
    #[serde(default, rename = "outputRef")]
    pub output_ref: Option<WorkflowOutputRef>,
    pub error: Option<Value>,
    pub wake_at: Option<DateTime<Utc>>,
    pub signal_type: Option<String>,
    pub max_signal_age_ms: Option<i64>,
    pub consumed_signal_id: Option<String>,
    pub topic: Option<String>,
    pub child_run_id: Option<String>,
    pub child_workflow_name: Option<String>,
    pub child_input: Option<Value>,
    pub child_options: Option<ChildWorkflowOptions>,
    #[serde(default, rename = "compensationState")]
    pub compensation_state: Option<String>,
    #[serde(default = "default_compensation_max_attempts", rename = "compensationMaxAttempts")]
    pub compensation_max_attempts: i32,
    /// Executions of this step's body that have reported an outcome.
    ///
    /// A completed row records what it cost; a `retrying` row records what has
    /// been spent so far, and is what the next failure counts from.
    #[serde(default)]
    pub attempts: i32,
    /// The ceiling the body declared through `StepConfig.retries`, already
    /// checked against the app's own ceiling.
    #[serde(default = "default_max_attempts", rename = "maxAttempts")]
    pub max_attempts: i32,
}

impl StepCheckpoint {
    #[must_use]
    pub fn completed_run(ordinal: i32, name: impl Into<String>, output: Value) -> Self {
        Self {
            ordinal,
            name: name.into(),
            name_occurrence: 0,
            kind: "run".to_string(),
            state: "completed".to_string(),
            output: Some(output),
            output_ref: None,
            error: None,
            wake_at: None,
            signal_type: None,
            max_signal_age_ms: None,
            consumed_signal_id: None,
            topic: None,
            child_run_id: None,
            child_workflow_name: None,
            child_input: None,
            child_options: None,
            compensation_state: None,
            compensation_max_attempts: 1,
            attempts: 0,
            max_attempts: 1,
        }
    }
}

pub fn default_step_kind() -> String {
    "run".to_string()
}

pub fn default_compensation_max_attempts() -> i32 {
    1
}

/// One execution: a step with no declared `retries` is attempted once.
pub fn default_max_attempts() -> i32 {
    1
}

pub fn parse_workflow_duration_ms(raw: &str) -> Option<i64> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Some(ms) = parse_iso_duration_ms(trimmed).or_else(|| parse_suffix_duration_ms(trimmed))
    {
        return Some(ms);
    }
    None
}

pub fn parse_iso_duration_ms(raw: &str) -> Option<i64> {
    let rest = raw.strip_prefix('P')?;
    let (date, time) = rest.split_once('T').unwrap_or((rest, ""));
    let mut total_ms = 0f64;
    if let Some(days) = date.strip_suffix('D') {
        if days.is_empty() {
            return None;
        }
        total_ms += days.parse::<f64>().ok()? * 86_400_000.0;
    } else if !date.is_empty() {
        return None;
    }
    let mut number = String::new();
    for ch in time.chars() {
        if ch.is_ascii_digit() || ch == '.' {
            number.push(ch);
            continue;
        }
        if number.is_empty() {
            return None;
        }
        let value = number.parse::<f64>().ok()?;
        number.clear();
        match ch {
            'H' => total_ms += value * 3_600_000.0,
            'M' => total_ms += value * 60_000.0,
            'S' => total_ms += value * 1_000.0,
            _ => return None,
        }
    }
    if !number.is_empty() || total_ms <= 0.0 || !total_ms.is_finite() {
        return None;
    }
    Some(total_ms.ceil() as i64)
}

pub fn parse_suffix_duration_ms(raw: &str) -> Option<i64> {
    let units = [
        ("ms", 1.0),
        ("s", 1_000.0),
        ("m", 60_000.0),
        ("h", 3_600_000.0),
        ("d", 86_400_000.0),
    ];
    for (suffix, multiplier) in units {
        let Some(number) = raw.strip_suffix(suffix) else {
            continue;
        };
        if number.is_empty() {
            return None;
        }
        let value = number.parse::<f64>().ok()?;
        let ms = value * multiplier;
        if ms <= 0.0 || !ms.is_finite() {
            return None;
        }
        return Some(ms.ceil() as i64);
    }
    raw.parse::<i64>().ok().filter(|v| *v > 0)
}

pub fn wake_at_from_str(raw: &str) -> Result<DateTime<Utc>, String> {
    if let Ok(ts) = DateTime::parse_from_rfc3339(raw) {
        return Ok(ts.with_timezone(&Utc));
    }
    parse_workflow_duration_ms(raw)
        .map(|ms| Utc::now() + chrono::Duration::milliseconds(ms))
        .ok_or_else(|| format!("invalid workflow wake/duration value {raw:?}"))
}

pub fn deserialize_wake_at<'de, D>(deserializer: D) -> Result<DateTime<Utc>, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    wake_at_from_str(&raw).map_err(de::Error::custom)
}

pub fn deserialize_optional_wake_at<'de, D>(
    deserializer: D,
) -> Result<Option<DateTime<Utc>>, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = Option::<String>::deserialize(deserializer)?;
    raw.as_deref()
        .map(wake_at_from_str)
        .transpose()
        .map_err(de::Error::custom)
}

pub fn deserialize_optional_wake_duration<'de, D>(
    deserializer: D,
) -> Result<Option<DateTime<Utc>>, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_optional_wake_at(deserializer)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum StepOutcome {
    StepCompleted {
        ordinal: i32,
        name: String,
        #[serde(default, rename = "nameOccurrence")]
        name_occurrence: i32,
        #[serde(default = "default_step_kind", rename = "stepKind")]
        step_kind: String,
        #[serde(default)]
        compensable: bool,
        #[serde(default = "default_compensation_max_attempts", rename = "compensationMaxAttempts")]
        compensation_max_attempts: i32,
        #[serde(default)]
        output: Option<Value>,
        #[serde(default, rename = "outputRef")]
        output_ref: Option<WorkflowOutputRef>,
    },
    StepFailed {
        ordinal: i32,
        name: String,
        #[serde(default, rename = "nameOccurrence")]
        name_occurrence: i32,
        #[serde(default)]
        error: Value,
        #[serde(default = "default_max_attempts", rename = "maxAttempts")]
        max_attempts: i32,
    },
    RunCompleted {
        #[serde(default)]
        output: Option<Value>,
        #[serde(default, rename = "outputRef")]
        output_ref: Option<WorkflowOutputRef>,
    },
    ContinueAsNew {
        #[serde(default)]
        input: Option<Value>,
        #[serde(default, rename = "inputRef")]
        input_ref: Option<WorkflowOutputRef>,
    },
    RunFailed {
        #[serde(default)]
        ordinal: Option<i32>,
        #[serde(default)]
        name: Option<String>,
        #[serde(default, rename = "nameOccurrence")]
        name_occurrence: i32,
        error: Value,
        /// Only meaningful with an `ordinal`: a terminal run failure names no
        /// step, so there is nothing to attempt again.
        #[serde(default = "default_max_attempts", rename = "maxAttempts")]
        max_attempts: i32,
    },
    Sleep {
        ordinal: i32,
        name: String,
        #[serde(default, rename = "nameOccurrence")]
        name_occurrence: i32,
        #[serde(rename = "wakeAt", deserialize_with = "deserialize_wake_at")]
        wake_at: DateTime<Utc>,
    },
    Wait {
        ordinal: i32,
        name: String,
        #[serde(default, rename = "nameOccurrence")]
        name_occurrence: i32,
        #[serde(default, rename = "wakeAt", deserialize_with = "deserialize_optional_wake_at")]
        wake_at: Option<DateTime<Utc>>,
        #[serde(default, deserialize_with = "deserialize_optional_wake_duration")]
        timeout: Option<DateTime<Utc>>,
        #[serde(default, rename = "signalType")]
        signal_type: Option<String>,
        #[serde(default, rename = "maxSignalAgeMs")]
        max_signal_age_ms: Option<i64>,
        #[serde(default, rename = "consumedSignalId")]
        consumed_signal_id: Option<String>,
        #[serde(default)]
        topic: Option<String>,
    },
    Child {
        ordinal: i32,
        name: String,
        #[serde(default, rename = "nameOccurrence")]
        name_occurrence: i32,
        #[serde(rename = "childWorkflowName", alias = "workflowName")]
        child_workflow_name: String,
        #[serde(default)]
        input: Value,
        #[serde(default)]
        options: ChildWorkflowOptions,
    },
    CompensationCompleted {
        ordinal: i32,
        name: String,
        #[serde(default, rename = "nameOccurrence")]
        name_occurrence: i32,
    },
    CompensationFailed {
        ordinal: i32,
        name: String,
        #[serde(default, rename = "nameOccurrence")]
        name_occurrence: i32,
        #[serde(default)]
        error: Value,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum RunUpdate {
    Queued,
    Sleeping {
        #[serde(rename = "wakeAt")]
        wake_at: Option<DateTime<Utc>>,
    },
    Waiting {
        #[serde(rename = "wakeAt")]
        wake_at: Option<DateTime<Utc>>,
    },
    Completed {
        output: Option<Value>,
        #[serde(default, rename = "outputRef")]
        output_ref: Option<WorkflowOutputRef>,
    },
    ContinuedAsNew {
        #[serde(default, rename = "seedInput")]
        seed_input: Option<Value>,
        #[serde(default, rename = "seedInputRef")]
        seed_input_ref: Option<WorkflowOutputRef>,
    },
    Failed { error: Value },
    Stalled { error: Value },
    Cancelled,
}

impl RunUpdate {
    pub fn state(&self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Sleeping { .. } => "sleeping",
            Self::Waiting { .. } => "waiting",
            Self::Completed { .. } => "completed",
            Self::ContinuedAsNew { .. } => "continuedAsNew",
            Self::Failed { .. } => "failed",
            Self::Stalled { .. } => "stalled",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn wake_at(&self) -> Option<DateTime<Utc>> {
        match self {
            Self::Queued => Some(Utc::now()),
            Self::Sleeping { wake_at } | Self::Waiting { wake_at } => *wake_at,
            Self::ContinuedAsNew { .. } => None,
            _ => None,
        }
    }

    pub fn output(&self) -> Option<Value> {
        match self {
            Self::Completed { output, .. } => output.clone(),
            _ => None,
        }
    }

    pub fn output_ref(&self) -> Option<WorkflowOutputRef> {
        match self {
            Self::Completed { output_ref, .. } => output_ref.clone(),
            _ => None,
        }
    }

    pub fn error(&self) -> Option<Value> {
        match self {
            Self::Failed { error } | Self::Stalled { error } => Some(error.clone()),
            _ => None,
        }
    }

    pub fn waiting_step_key(&self, checkpoints: &[StepCheckpoint]) -> Option<String> {
        match self {
            Self::Sleeping { .. } => checkpoints
                .iter()
                .find(|s| s.kind == "sleep" && s.state == "running")
                .map(|s| format!("sleep:{}:{}", s.ordinal, s.name)),
            Self::Waiting { .. } => checkpoints
                .iter()
                .find(|s| matches!(s.kind.as_str(), "wait_signal" | "child") && s.state == "running")
                .map(|s| {
                    if s.kind == "child" {
                        format!("child:{}:{}", s.ordinal, s.name)
                    } else {
                        format!(
                            "wait:{}:{}:{}{}",
                            s.ordinal,
                            s.name,
                            s.signal_type.as_deref().unwrap_or(s.name.as_str()),
                            s.max_signal_age_ms
                                .map(|age| format!(":{age}"))
                                .unwrap_or_default()
                        )
                    }
                }),
            _ => None,
        }
    }
}

pub fn child_signal_type(ordinal: i32) -> String {
    format!("__zs.child:{ordinal}")
}

pub fn child_dedup_key(parent_run_id: &str, ordinal: i32) -> String {
    format!("child:{parent_run_id}:{ordinal}")
}

pub fn fold_outcomes(outcomes: &[StepOutcome]) -> Result<(Vec<StepCheckpoint>, RunUpdate), String> {
    let mut checkpoints = Vec::new();
    let mut run_update = RunUpdate::Queued;
    let mut trailing_seen = false;
    let mut saw_step_failure = false;

    for (idx, outcome) in outcomes.iter().enumerate() {
        let is_step_checkpoint = matches!(
            outcome,
            StepOutcome::StepCompleted { .. }
                | StepOutcome::StepFailed { .. }
                | StepOutcome::Child { .. }
                | StepOutcome::RunFailed {
                    ordinal: Some(_),
                    name: Some(_),
                    ..
                }
        );
        if trailing_seen {
            return Err("workflow outcome batch has entries after a suspension or terminal outcome".to_string());
        }
        if !is_step_checkpoint {
            if idx + 1 != outcomes.len() {
                return Err("workflow suspension or terminal outcome must be the trailing batch entry".to_string());
            }
            trailing_seen = true;
        }

        match outcome {
            StepOutcome::StepCompleted {
                ordinal,
                name,
                name_occurrence,
                step_kind,
                compensable,
                compensation_max_attempts,
                output,
                output_ref,
            } => {
                if !matches!(step_kind.as_str(), "run" | "sideEffect") {
                    return Err(format!(
                        "workflow StepCompleted stepKind must be run or sideEffect, got {step_kind:?}"
                    ));
                }
                checkpoints.push(StepCheckpoint {
                    ordinal: *ordinal,
                    name: name.clone(),
                    name_occurrence: *name_occurrence,
                    kind: step_kind.clone(),
                    state: "completed".to_string(),
                    output: output.clone(),
                    output_ref: output_ref.clone(),
                    error: None,
                    wake_at: None,
                    signal_type: None,
                    max_signal_age_ms: None,
                    consumed_signal_id: None,
                    topic: None,
                    child_run_id: None,
                    child_workflow_name: None,
                    child_input: None,
                    child_options: None,
                    compensation_state: (*compensable && step_kind == "run")
                        .then(|| "pending".to_string()),
                    compensation_max_attempts: (*compensation_max_attempts).max(1),
                    attempts: 0,
                    max_attempts: 1,
                });
            }
            StepOutcome::StepFailed {
                ordinal,
                name,
                name_occurrence,
                error,
                max_attempts,
            } => {
                checkpoints.push(StepCheckpoint {
                    ordinal: *ordinal,
                    name: name.clone(),
                    name_occurrence: *name_occurrence,
                    kind: "run".to_string(),
                    state: "failed".to_string(),
                    output: None,
                    output_ref: None,
                    error: Some(error.clone()),
                    wake_at: None,
                    signal_type: None,
                    max_signal_age_ms: None,
                    consumed_signal_id: None,
                    topic: None,
                    child_run_id: None,
                    child_workflow_name: None,
                    child_input: None,
                    child_options: None,
                    compensation_state: None,
                    compensation_max_attempts: 1,
                    attempts: 0,
                    max_attempts: (*max_attempts).max(1),
                });
                saw_step_failure = true;
                run_update = RunUpdate::Queued;
            }
            StepOutcome::RunCompleted { output, output_ref } => {
                run_update = RunUpdate::Completed {
                    output: output.clone(),
                    output_ref: output_ref.clone(),
                };
            }
            StepOutcome::ContinueAsNew { input, input_ref } => {
                run_update = RunUpdate::ContinuedAsNew {
                    seed_input: input.clone(),
                    seed_input_ref: input_ref.clone(),
                };
            }
            StepOutcome::RunFailed {
                ordinal,
                name,
                name_occurrence,
                error,
                max_attempts,
            } => {
                match (ordinal, name) {
                    (Some(ordinal), Some(name)) => {
                        checkpoints.push(StepCheckpoint {
                            ordinal: *ordinal,
                            name: name.clone(),
                            name_occurrence: *name_occurrence,
                            kind: "run".to_string(),
                            state: "failed".to_string(),
                            output: None,
                            output_ref: None,
                            error: Some(error.clone()),
                            wake_at: None,
                            signal_type: None,
                            max_signal_age_ms: None,
                            consumed_signal_id: None,
                            topic: None,
                            child_run_id: None,
                            child_workflow_name: None,
                            child_input: None,
                            child_options: None,
                            compensation_state: None,
                            compensation_max_attempts: 1,
                            attempts: 0,
                            max_attempts: (*max_attempts).max(1),
                        });
                        saw_step_failure = true;
                        run_update = RunUpdate::Queued;
                    }
                    (None, None) => {
                        run_update = RunUpdate::Failed {
                            error: error.clone(),
                        };
                    }
                    _ => {
                        return Err(
                            "workflow RunFailed outcome must include both ordinal and name for a failed step, or neither for terminal run failure"
                                .to_string(),
                        );
                    }
                }
            }
            StepOutcome::Sleep {
                ordinal,
                name,
                name_occurrence,
                wake_at,
            } => {
                checkpoints.push(StepCheckpoint {
                    ordinal: *ordinal,
                    name: name.clone(),
                    name_occurrence: *name_occurrence,
                    kind: "sleep".to_string(),
                    state: "running".to_string(),
                    output: None,
                    output_ref: None,
                    error: None,
                    wake_at: Some(*wake_at),
                    signal_type: None,
                    max_signal_age_ms: None,
                    consumed_signal_id: None,
                    topic: None,
                    child_run_id: None,
                    child_workflow_name: None,
                    child_input: None,
                    child_options: None,
                    compensation_state: None,
                    compensation_max_attempts: 1,
                    attempts: 0,
                    max_attempts: 1,
                });
                if !saw_step_failure {
                    run_update = RunUpdate::Sleeping {
                        wake_at: Some(*wake_at),
                    };
                }
            }
            StepOutcome::Wait {
                ordinal,
                name,
                name_occurrence,
                wake_at,
                timeout,
                signal_type,
                max_signal_age_ms,
                consumed_signal_id,
                topic,
            } => {
                let wake_at = wake_at.or(*timeout);
                checkpoints.push(StepCheckpoint {
                    ordinal: *ordinal,
                    name: name.clone(),
                    name_occurrence: *name_occurrence,
                    kind: "wait_signal".to_string(),
                    state: "running".to_string(),
                    output: None,
                    output_ref: None,
                    error: None,
                    wake_at,
                    signal_type: signal_type.clone().or_else(|| Some(name.clone())),
                    max_signal_age_ms: *max_signal_age_ms,
                    consumed_signal_id: consumed_signal_id.clone(),
                    topic: topic.clone(),
                    child_run_id: None,
                    child_workflow_name: None,
                    child_input: None,
                    child_options: None,
                    compensation_state: None,
                    compensation_max_attempts: 1,
                    attempts: 0,
                    max_attempts: 1,
                });
                if !saw_step_failure {
                    run_update = RunUpdate::Waiting {
                        wake_at,
                    };
                }
            }
            StepOutcome::Child {
                ordinal,
                name,
                name_occurrence,
                child_workflow_name,
                input,
                options,
            } => {
                checkpoints.push(StepCheckpoint {
                    ordinal: *ordinal,
                    name: name.clone(),
                    name_occurrence: *name_occurrence,
                    kind: "child".to_string(),
                    state: "running".to_string(),
                    output: None,
                    output_ref: None,
                    error: None,
                    wake_at: options.timeout,
                    signal_type: Some(child_signal_type(*ordinal)),
                    max_signal_age_ms: None,
                    consumed_signal_id: None,
                    topic: None,
                    child_run_id: None,
                    child_workflow_name: Some(child_workflow_name.clone()),
                    child_input: Some(input.clone()),
                    child_options: Some(options.clone()),
                    compensation_state: None,
                    compensation_max_attempts: 1,
                    attempts: 0,
                    max_attempts: 1,
                });
                if !saw_step_failure {
                    run_update = RunUpdate::Waiting {
                        wake_at: options.timeout,
                    };
                }
            }
            StepOutcome::CompensationCompleted { .. }
            | StepOutcome::CompensationFailed { .. } => {}
        }
    }

    if saw_step_failure {
        run_update = RunUpdate::Queued;
    }

    Ok((checkpoints, run_update))
}

/// The creator-visible reason a generation that asked to continue instead failed.
///
/// A compensator belongs to the generation whose step registered it, and a
/// successor starts from an empty journal, so carrying the obligation across
/// the transition would strand an undo nothing could ever run.
pub fn compensable_carry_error() -> Value {
    serde_json::json!({
        "type": "CompensableCarryError",
        "message": "cannot continue as new while compensable steps are pending",
    })
}

/// The creator-visible reason a run rests in `stalled`: its dispatches kept
/// being reclaimed with no outcome reported, against one unchanged frontier.
pub fn stalled_error(dispatches: i64, limit: i64) -> Value {
    serde_json::json!({
        "type": "StalledError",
        "message": "workflow made no durable progress before the liveness dispatch limit",
        "stuckDispatches": dispatches,
        "maxStuckDispatches": limit,
    })
}

#[derive(Debug, Clone)]
pub struct CompensationApplyOutcome {
    pub ordinal: i32,
    pub name: String,
    pub name_occurrence: i32,
    pub state: &'static str,
    pub error: Option<Value>,
}

pub fn compensation_outcomes_from_step_outcomes(
    outcomes: &[StepOutcome],
) -> Result<Vec<CompensationApplyOutcome>, String> {
    let mut compensation = Vec::new();
    for outcome in outcomes {
        match outcome {
            StepOutcome::CompensationCompleted {
                ordinal,
                name,
                name_occurrence,
            } => compensation.push(CompensationApplyOutcome {
                ordinal: *ordinal,
                name: name.clone(),
                name_occurrence: *name_occurrence,
                state: "completed",
                error: None,
            }),
            StepOutcome::CompensationFailed {
                ordinal,
                name,
                name_occurrence,
                error,
            } => compensation.push(CompensationApplyOutcome {
                ordinal: *ordinal,
                name: name.clone(),
                name_occurrence: *name_occurrence,
                state: "failed",
                error: Some(error.clone()),
            }),
            _ => {}
        }
    }
    if !compensation.is_empty() && compensation.len() != outcomes.len() {
        return Err("compensation outcomes cannot be mixed with forward outcomes".to_string());
    }
    Ok(compensation)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::operations::RunState;
    use serde_json::json;

    #[test]
    fn fold_continue_as_new_is_its_own_terminal_update() {
        let input = json!({"generation": 1, "carry": "state"});
        let (checkpoints, update) = fold_outcomes(&[StepOutcome::ContinueAsNew {
            input: Some(input.clone()),
            input_ref: None,
        }])
        .expect("continue-as-new should fold");

        assert!(checkpoints.is_empty());
        assert_eq!(update.state(), RunState::ContinuedAsNew.as_str());
        assert_ne!(update.state(), RunState::Completed.as_str());
        assert!(update.output().is_none());
        match update {
            RunUpdate::ContinuedAsNew {
                seed_input,
                seed_input_ref,
            } => {
                assert_eq!(seed_input, Some(input));
                assert!(seed_input_ref.is_none());
            }
            other => panic!("unexpected run update: {other:?}"),
        }
    }

    #[test]
    fn continue_as_new_must_be_trailing() {
        let err = fold_outcomes(&[
            StepOutcome::ContinueAsNew {
                input: Some(json!({"generation": 1})),
                input_ref: None,
            },
            StepOutcome::RunCompleted {
                output: Some(json!({"done": true})),
                output_ref: None,
            },
        ])
        .expect_err("terminal outcome must reject trailing entries");

        assert!(err.contains("trailing"));
    }
}
