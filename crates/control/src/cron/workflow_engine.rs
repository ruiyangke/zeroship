//! Durable-workflow engine scheduler.
//!
//! This cron owns only the control-plane scheduling core: due-run claiming,
//! lease heartbeats, the dispatch seam, and the idempotent outcome apply txn.

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use compio_postgres::error::SqlState;
use compio_postgres::GenericClient;
use serde::{de, Deserialize, Deserializer, Serialize};
use serde_json::Value;
use uuid::Uuid;
use zeroship_core::typed_id;

use crate::registry::RegistryError;
use crate::workflow_limits;
use crate::workflow_rollout;
use crate::{AppState, Registry};

/// Default tick cadence. Workflow wake latency is intentionally a scheduler
/// knob, not a correctness bound; DW-23 will measure and tune it.
pub const DEFAULT_TICK_SECS: u64 = 1;
const GATEWAY_WORKFLOW_DISPATCH_PATH: &str = "/__zeroship/internal/workflow-dispatch";
const GATEWAY_DISPATCH_TIMEOUT: Duration = Duration::from_secs(35);
const BACKPRESSURE_PARK_MS: i64 = 1_000;
const BLOB_REF_JOURNAL_BYTES: i64 = 160;
pub const DEFAULT_MAX_CHILD_DEPTH: i16 = 16;
pub const DEFAULT_MAX_LIVE_DESCENDANTS: i64 = 1_024;
pub const DEFAULT_MAX_START_MANY_BATCH: usize = 1_000;

static INFLIGHT_DISPATCHES: AtomicUsize = AtomicUsize::new(0);
static OWNER_ID: OnceLock<String> = OnceLock::new();

fn default_owner_id() -> String {
    OWNER_ID
        .get_or_init(|| format!("control-wf-{}", std::process::id()))
        .clone()
}

#[derive(Debug, Clone)]
pub struct WorkflowEngineConfig {
    /// Number of distinct apps considered in one tick.
    pub batch_apps: i64,
    /// Max due rows claimed per app in one tick.
    pub per_app_fair_limit: i64,
    /// Per-app live dispatch cap. Counts only fresh `running` + `claimed_by`
    /// rows, never queued/sleeping/waiting rows.
    pub max_inflight_per_app: i64,
    /// Process-local detached dispatch cap.
    pub max_inflight_dispatch: usize,
    /// Lease staleness duration in milliseconds.
    pub claim_ttl_ms: i64,
    /// Heartbeat cadence while a detached dispatch is in flight.
    pub heartbeat_ms: u64,
    /// Consecutive zero-progress dispatches before fail-closed `stalled`.
    ///
    /// G3 placeholder: DW-23 will measure wide-frontier rollover behavior and
    /// replace this conservative seed with operator-plan defaults.
    pub stuck_strike_limit: i16,
    /// Maximum parent/child depth for step.call trees.
    pub max_child_depth: i16,
    /// Maximum live descendants under a workflow tree root.
    pub max_live_descendants: i64,
    /// Maximum child starts committed by one frontier batch.
    pub max_start_many_batch: usize,
    /// Stable owner id written into `claimed_by`.
    pub owner_id: String,
}

impl Default for WorkflowEngineConfig {
    fn default() -> Self {
        Self {
            batch_apps: 32,
            per_app_fair_limit: 4,
            max_inflight_per_app: 16,
            max_inflight_dispatch: 64,
            claim_ttl_ms: 120_000,
            heartbeat_ms: 5_000,
            stuck_strike_limit: 3,
            max_child_depth: DEFAULT_MAX_CHILD_DEPTH,
            max_live_descendants: DEFAULT_MAX_LIVE_DESCENDANTS,
            max_start_many_batch: DEFAULT_MAX_START_MANY_BATCH,
            owner_id: default_owner_id(),
        }
    }
}

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
pub struct StepRequest {
    pub run_id: String,
    pub app_id: Uuid,
    pub workflow_name: String,
    pub deploy_id: String,
    pub deploy_hash: String,
    pub dispatch_nonce: String,
    #[serde(default = "default_dispatch_phase")]
    pub phase: String,
    #[serde(default)]
    pub input: Option<Value>,
    pub started_at: DateTime<Utc>,
    pub journal: Vec<JournalStep>,
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
        }
    }
}

fn default_step_kind() -> String {
    "run".to_string()
}

fn default_compensation_max_attempts() -> i32 {
    1
}

fn default_dispatch_phase() -> String {
    "running".to_string()
}

fn parse_workflow_duration_ms(raw: &str) -> Option<i64> {
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

fn parse_iso_duration_ms(raw: &str) -> Option<i64> {
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

fn parse_suffix_duration_ms(raw: &str) -> Option<i64> {
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

fn wake_at_from_str(raw: &str) -> Result<DateTime<Utc>, String> {
    if let Ok(ts) = DateTime::parse_from_rfc3339(raw) {
        return Ok(ts.with_timezone(&Utc));
    }
    parse_workflow_duration_ms(raw)
        .map(|ms| Utc::now() + chrono::Duration::milliseconds(ms))
        .ok_or_else(|| format!("invalid workflow wake/duration value {raw:?}"))
}

fn deserialize_wake_at<'de, D>(deserializer: D) -> Result<DateTime<Utc>, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    wake_at_from_str(&raw).map_err(de::Error::custom)
}

fn deserialize_optional_wake_at<'de, D>(
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

fn deserialize_optional_wake_duration<'de, D>(
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
    },
    RunCompleted {
        #[serde(default)]
        output: Option<Value>,
        #[serde(default, rename = "outputRef")]
        output_ref: Option<WorkflowOutputRef>,
    },
    RunFailed {
        #[serde(default)]
        ordinal: Option<i32>,
        #[serde(default)]
        name: Option<String>,
        #[serde(default, rename = "nameOccurrence")]
        name_occurrence: i32,
        error: Value,
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
    Failed { error: Value },
    Stalled { error: Value },
    Cancelled,
}

impl RunUpdate {
    fn state(&self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Sleeping { .. } => "sleeping",
            Self::Waiting { .. } => "waiting",
            Self::Completed { .. } => "completed",
            Self::Failed { .. } => "failed",
            Self::Stalled { .. } => "stalled",
            Self::Cancelled => "cancelled",
        }
    }

    fn wake_at(&self) -> Option<DateTime<Utc>> {
        match self {
            Self::Queued => Some(Utc::now()),
            Self::Sleeping { wake_at } | Self::Waiting { wake_at } => *wake_at,
            _ => None,
        }
    }

    fn output(&self) -> Option<Value> {
        match self {
            Self::Completed { output, .. } => output.clone(),
            _ => None,
        }
    }

    fn output_ref(&self) -> Option<WorkflowOutputRef> {
        match self {
            Self::Completed { output_ref, .. } => output_ref.clone(),
            _ => None,
        }
    }

    fn error(&self) -> Option<Value> {
        match self {
            Self::Failed { error } | Self::Stalled { error } => Some(error.clone()),
            _ => None,
        }
    }

    fn waiting_step_key(&self, checkpoints: &[StepCheckpoint]) -> Option<String> {
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

#[derive(Debug, Clone)]
pub struct StepResult {
    pub run_id: String,
    pub dispatch_nonce: String,
    pub outcomes: Vec<StepOutcome>,
    pub checkpoints: Vec<StepCheckpoint>,
    pub run_update: RunUpdate,
}

impl StepResult {
    #[must_use]
    pub fn requeue(run_id: String, dispatch_nonce: String) -> Self {
        Self {
            run_id,
            dispatch_nonce,
            outcomes: Vec::new(),
            checkpoints: Vec::new(),
            run_update: RunUpdate::Queued,
        }
    }

    #[must_use]
    pub fn from_checkpoints(
        run_id: String,
        dispatch_nonce: String,
        checkpoints: Vec<StepCheckpoint>,
        run_update: RunUpdate,
    ) -> Self {
        let outcomes = outcomes_from_apply_parts(&checkpoints, &run_update);
        Self {
            run_id,
            dispatch_nonce,
            outcomes,
            checkpoints,
            run_update,
        }
    }

    fn from_outcomes(
        run_id: String,
        dispatch_nonce: String,
        outcomes: Vec<StepOutcome>,
    ) -> Result<Self, String> {
        let (checkpoints, run_update) = fold_outcomes(&outcomes)?;
        Ok(Self {
            run_id,
            dispatch_nonce,
            outcomes,
            checkpoints,
            run_update,
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StepResultWire {
    run_id: String,
    dispatch_nonce: String,
    #[serde(default)]
    outcomes: Vec<StepOutcome>,
    #[serde(default)]
    checkpoints: Vec<StepCheckpoint>,
    #[serde(default = "queued_run_update")]
    run_update: RunUpdate,
}

fn queued_run_update() -> RunUpdate {
    RunUpdate::Queued
}

impl<'de> Deserialize<'de> for StepResult {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = StepResultWire::deserialize(deserializer)?;
        if !wire.outcomes.is_empty() {
            return StepResult::from_outcomes(wire.run_id, wire.dispatch_nonce, wire.outcomes)
                .map_err(serde::de::Error::custom);
        }
        Ok(StepResult::from_checkpoints(
            wire.run_id,
            wire.dispatch_nonce,
            wire.checkpoints,
            wire.run_update,
        ))
    }
}

impl Serialize for StepResult {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Wire<'a> {
            run_id: &'a str,
            dispatch_nonce: &'a str,
            outcomes: &'a [StepOutcome],
        }

        Wire {
            run_id: &self.run_id,
            dispatch_nonce: &self.dispatch_nonce,
            outcomes: &self.outcomes,
        }
        .serialize(serializer)
    }
}

fn fold_outcomes(outcomes: &[StepOutcome]) -> Result<(Vec<StepCheckpoint>, RunUpdate), String> {
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
                });
            }
            StepOutcome::StepFailed {
                ordinal,
                name,
                name_occurrence,
                error,
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
            StepOutcome::RunFailed {
                ordinal,
                name,
                name_occurrence,
                error,
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

fn stalled_error(strikes: i16, limit: i16) -> Value {
    serde_json::json!({
        "type": "StalledError",
        "message": "workflow made no durable progress before the liveness strike limit",
        "stuck_strikes": strikes,
        "stuck_strike_limit": limit,
    })
}

fn outcomes_from_apply_parts(
    checkpoints: &[StepCheckpoint],
    run_update: &RunUpdate,
) -> Vec<StepOutcome> {
    let mut outcomes = Vec::new();
    let mut failed_checkpoint_encoded = false;

    for checkpoint in checkpoints {
        match (checkpoint.kind.as_str(), checkpoint.state.as_str()) {
            ("run" | "sideEffect", "completed") => outcomes.push(StepOutcome::StepCompleted {
                ordinal: checkpoint.ordinal,
                name: checkpoint.name.clone(),
                name_occurrence: checkpoint.name_occurrence,
                step_kind: checkpoint.kind.clone(),
                compensable: checkpoint.compensation_state.is_some(),
                compensation_max_attempts: checkpoint.compensation_max_attempts,
                output: checkpoint.output.clone(),
                output_ref: checkpoint.output_ref.clone(),
            }),
            ("run", "failed") => {
                failed_checkpoint_encoded = true;
                outcomes.push(StepOutcome::StepFailed {
                    ordinal: checkpoint.ordinal,
                    name: checkpoint.name.clone(),
                    name_occurrence: checkpoint.name_occurrence,
                    error: checkpoint.error.clone().unwrap_or_else(|| {
                        serde_json::json!({"type": "Error", "message": "workflow step failed"})
                    }),
                });
            }
            ("sleep", "running") => {
                if let Some(wake_at) = checkpoint.wake_at {
                    outcomes.push(StepOutcome::Sleep {
                        ordinal: checkpoint.ordinal,
                        name: checkpoint.name.clone(),
                        name_occurrence: checkpoint.name_occurrence,
                        wake_at,
                    });
                }
            }
            ("wait_signal", "running") => {
                outcomes.push(StepOutcome::Wait {
                    ordinal: checkpoint.ordinal,
                    name: checkpoint.name.clone(),
                    name_occurrence: checkpoint.name_occurrence,
                    wake_at: checkpoint.wake_at,
                    timeout: None,
                    signal_type: checkpoint.signal_type.clone(),
                    max_signal_age_ms: checkpoint.max_signal_age_ms,
                    consumed_signal_id: checkpoint.consumed_signal_id.clone(),
                    topic: checkpoint.topic.clone(),
                });
            }
            ("child", "running") => {
                outcomes.push(StepOutcome::Child {
                    ordinal: checkpoint.ordinal,
                    name: checkpoint.name.clone(),
                    name_occurrence: checkpoint.name_occurrence,
                    child_workflow_name: checkpoint
                        .child_workflow_name
                        .clone()
                        .unwrap_or_else(|| checkpoint.name.clone()),
                    input: checkpoint.child_input.clone().unwrap_or(Value::Null),
                    options: checkpoint.child_options.clone().unwrap_or_default(),
                });
            }
            _ => {}
        }
    }

    match run_update {
        RunUpdate::Completed { output, output_ref } => outcomes.push(StepOutcome::RunCompleted {
            output: output.clone(),
            output_ref: output_ref.clone(),
        }),
        RunUpdate::Failed { error } if !failed_checkpoint_encoded => {
            outcomes.push(StepOutcome::RunFailed {
                ordinal: None,
                name: None,
                name_occurrence: 0,
                error: error.clone(),
            });
        }
        RunUpdate::Stalled { error } => outcomes.push(StepOutcome::RunFailed {
            ordinal: None,
            name: None,
            name_occurrence: 0,
            error: error.clone(),
        }),
        _ => {}
    }

    outcomes
}

#[async_trait(?Send)]
pub trait StepDispatcher: Send + Sync {
    async fn dispatch(&self, request: StepRequest) -> DispatchOutcome;
}

#[derive(Debug, Clone)]
pub enum DispatchOutcome {
    Completed(StepResult),
    Backpressure {
        run_id: String,
        dispatch_nonce: String,
        reason: String,
    },
}

impl DispatchOutcome {
    fn backpressure(request: &StepRequest, reason: impl Into<String>) -> Self {
        Self::Backpressure {
            run_id: request.run_id.clone(),
            dispatch_nonce: request.dispatch_nonce.clone(),
            reason: reason.into(),
        }
    }
}

#[derive(Debug, Default)]
pub struct StubStepDispatcher;

#[async_trait(?Send)]
impl StepDispatcher for StubStepDispatcher {
    async fn dispatch(&self, request: StepRequest) -> DispatchOutcome {
        DispatchOutcome::Completed(StepResult::requeue(request.run_id, request.dispatch_nonce))
    }
}

#[derive(Debug, Clone)]
pub struct GatewayStepDispatcher {
    gateway_url: String,
}

impl GatewayStepDispatcher {
    #[must_use]
    pub fn new(gateway_url: impl Into<String>) -> Self {
        Self {
            gateway_url: gateway_url.into().trim_end_matches('/').to_string(),
        }
    }
}

#[async_trait(?Send)]
impl StepDispatcher for GatewayStepDispatcher {
    async fn dispatch(&self, request: StepRequest) -> DispatchOutcome {
        if self.gateway_url.is_empty() {
            return DispatchOutcome::backpressure(&request, "gateway URL is not configured");
        }
        let body = match serde_json::to_vec(&request) {
            Ok(body) => body,
            Err(e) => {
                return DispatchOutcome::backpressure(
                    &request,
                    format!("serialize StepRequest: {e}"),
                );
            }
        };
        let url = format!("{}{}", self.gateway_url, GATEWAY_WORKFLOW_DISPATCH_PATH);
        let client = cyper::Client::new();
        let builder = match client.post(&url) {
            Ok(builder) => builder,
            Err(e) => {
                return DispatchOutcome::backpressure(
                    &request,
                    format!("build gateway workflow dispatch request: {e}"),
                );
            }
        };
        let builder = match builder.header("content-type", "application/json") {
            Ok(builder) => builder,
            Err(e) => {
                return DispatchOutcome::backpressure(
                    &request,
                    format!("set gateway workflow dispatch content-type: {e}"),
                );
            }
        };
        let response = match compio::time::timeout(
            GATEWAY_DISPATCH_TIMEOUT,
            builder.body(body).send(),
        )
        .await
        {
            Ok(Ok(response)) => response,
            Ok(Err(e)) => {
                return DispatchOutcome::backpressure(
                    &request,
                    format!("gateway workflow dispatch transport: {e}"),
                );
            }
            Err(_) => {
                return DispatchOutcome::backpressure(
                    &request,
                    "gateway workflow dispatch timeout",
                );
            }
        };

        let status = response.status().as_u16();
        let bytes = match response.bytes().await {
            Ok(bytes) => bytes,
            Err(e) => {
                return DispatchOutcome::backpressure(
                    &request,
                    format!("read gateway workflow dispatch body: {e}"),
                );
            }
        };

        if status == 402 || status >= 500 {
            let body_snippet: String = String::from_utf8_lossy(&bytes).chars().take(200).collect();
            return DispatchOutcome::backpressure(
                &request,
                format!("gateway workflow dispatch HTTP {status}: {body_snippet}"),
            );
        }
        if !(200..300).contains(&status) {
            let body_snippet: String = String::from_utf8_lossy(&bytes).chars().take(200).collect();
            return DispatchOutcome::backpressure(
                &request,
                format!("gateway workflow dispatch rejected HTTP {status}: {body_snippet}"),
            );
        }

        match serde_json::from_slice::<StepResult>(&bytes) {
            Ok(result) => DispatchOutcome::Completed(result),
            Err(e) => DispatchOutcome::backpressure(
                &request,
                format!("parse gateway StepResult: {e}"),
            ),
        }
    }
}

/// Cron entry point. Loops forever; one deterministic [`tick`] per cadence.
#[allow(clippy::future_not_send)]
pub async fn run(state: Arc<AppState>, tick_secs: u64) {
    tracing::info!(tick_secs, "control workflow_engine cron starting");
    loop {
        match tick(&state).await {
            Ok(n) if n > 0 => tracing::info!(claimed = n, "workflow_engine tick claimed runs"),
            Ok(_) => {}
            Err(e) => tracing::error!(error = %e, "workflow_engine tick failed"),
        }
        compio::time::sleep(Duration::from_secs(tick_secs)).await;
    }
}

/// Run one claim/dispatch scheduler pass. Public for deterministic tests.
#[allow(clippy::future_not_send)]
pub async fn tick(state: &AppState) -> Result<usize, RegistryError> {
    tick_with_dispatcher(
        state,
        Arc::new(GatewayStepDispatcher::new(state.gateway_url.clone())),
        WorkflowEngineConfig::default(),
    )
    .await
}

/// Testable tick variant with an injected dispatch seam.
#[allow(clippy::future_not_send)]
pub async fn tick_with_dispatcher<D>(
    state: &AppState,
    dispatcher: Arc<D>,
    config: WorkflowEngineConfig,
) -> Result<usize, RegistryError>
where
    D: StepDispatcher + 'static,
{
    {
        let conn = state.registry.conn().await?;
        if workflow_rollout::dispatch_paused(&conn).await? {
            return Ok(0);
        }
    }

    reap_parked_cancel_requested_batch(&state.registry, &config).await?;

    let current = INFLIGHT_DISPATCHES.load(Ordering::SeqCst);
    if current >= config.max_inflight_dispatch {
        return Ok(0);
    }
    let available = config.max_inflight_dispatch - current;
    let claims = claim_due_batch(&state.registry, &config, available).await?;
    let claimed = claims.len();
    for claim in claims {
        spawn_dispatch(state.registry.clone(), dispatcher.clone(), config.clone(), claim);
    }
    Ok(claimed)
}

#[derive(Debug, Clone)]
struct ClaimedRun {
    request: StepRequest,
}

#[derive(Debug, Clone)]
struct CandidateRun {
    run_id: String,
    app_id: Uuid,
    workflow_name: String,
    deploy_id: String,
    deploy_hash: String,
    state: String,
    input: Option<Value>,
    started_at: DateTime<Utc>,
    waiting_step_key: Option<String>,
    cancel_requested: bool,
}

async fn claim_due_batch(
    registry: &Registry,
    config: &WorkflowEngineConfig,
    available: usize,
) -> Result<Vec<ClaimedRun>, RegistryError> {
    let mut conn = registry.conn().await?;
    let tx = conn.transaction().await.map_err(RegistryError::from)?;
    rearm_waiting_runs_with_pending_signals(&tx).await?;
    let limit = i64::try_from(available)
        .unwrap_or(i64::MAX)
        .min(config.batch_apps.saturating_mul(config.per_app_fair_limit));
    if limit <= 0 {
        tx.commit().await.map_err(RegistryError::from)?;
        return Ok(Vec::new());
    }

    let rows = tx
        .query(
            "WITH due_apps AS ( \
               SELECT r.app_id, MIN(r.wake_at) AS first_wake \
                 FROM zeroship.workflow_runs r \
                 JOIN zeroship.apps app ON app.id = r.app_id \
                 JOIN zeroship.plans plan ON plan.id = app.plan_id \
                WHERE r.wake_at <= now() \
                  AND r.state IN ('queued','running','sleeping','waiting','compensating') \
                  AND (r.claimed_by IS NULL OR r.lease_expires IS NULL OR r.lease_expires <= now()) \
                  AND app.workflows_enabled \
                  AND plan.workflows_allowed \
                  AND NOT plan.archived \
                GROUP BY r.app_id \
                ORDER BY first_wake, r.app_id \
                LIMIT $1 \
             ) \
             SELECT r.id, r.app_id, r.workflow_name, r.deploy_id, d.deploy_hash, \
                    r.state, r.input, r.started_at, r.waiting_step_key, r.cancel_requested \
               FROM due_apps a \
               CROSS JOIN LATERAL ( \
                 SELECT id, app_id, workflow_name, deploy_id, state, input, started_at, waiting_step_key, wake_at, cancel_requested \
                   FROM zeroship.workflow_runs \
                  WHERE app_id = a.app_id \
                    AND wake_at <= now() \
                    AND state IN ('queued','running','sleeping','waiting','compensating') \
                    AND (claimed_by IS NULL OR lease_expires IS NULL OR lease_expires <= now()) \
                  ORDER BY wake_at, id \
                  LIMIT $2 \
                  FOR UPDATE SKIP LOCKED \
               ) r \
               JOIN zeroship.app_deploys d ON d.id = r.deploy_id \
              ORDER BY r.app_id, r.wake_at, r.id \
              LIMIT $3",
            &[
                &config.batch_apps,
                &config.per_app_fair_limit,
                &limit,
            ],
        )
        .await
        .map_err(RegistryError::from)?;

    let mut claimed = Vec::new();
    for row in rows {
        let candidate = CandidateRun {
            run_id: row.get("id"),
            app_id: row.get("app_id"),
            workflow_name: row.get("workflow_name"),
            deploy_id: row.get("deploy_id"),
            deploy_hash: row.get("deploy_hash"),
            state: row.get("state"),
            input: row.get("input"),
            started_at: row.get("started_at"),
            waiting_step_key: row.get("waiting_step_key"),
            cancel_requested: row.get("cancel_requested"),
        };

        tx.batch_execute("SAVEPOINT workflow_claim_row")
            .await
            .map_err(RegistryError::from)?;
        match claim_one_locked(&tx, config, candidate).await {
            Ok(Some(run)) => {
                tx.batch_execute("RELEASE SAVEPOINT workflow_claim_row")
                    .await
                    .map_err(RegistryError::from)?;
                claimed.push(run);
            }
            Ok(None) => {
                tx.batch_execute("RELEASE SAVEPOINT workflow_claim_row")
                    .await
                    .map_err(RegistryError::from)?;
            }
            Err(e) => {
                tracing::warn!(error = %e, "workflow_engine: quarantining poison claim row");
                tx.batch_execute("ROLLBACK TO SAVEPOINT workflow_claim_row")
                    .await
                    .map_err(RegistryError::from)?;
                tx.batch_execute("RELEASE SAVEPOINT workflow_claim_row")
                    .await
                    .map_err(RegistryError::from)?;
            }
        }
    }

    tx.commit().await.map_err(RegistryError::from)?;
    Ok(claimed)
}

async fn reap_parked_cancel_requested_batch(
    registry: &Registry,
    config: &WorkflowEngineConfig,
) -> Result<u64, RegistryError> {
    let limit = i64::try_from(config.batch_apps.saturating_mul(config.per_app_fair_limit))
        .unwrap_or(i64::MAX);
    if limit <= 0 {
        return Ok(0);
    }

    let mut conn = registry.conn().await?;
    let tx = conn.transaction().await.map_err(RegistryError::from)?;
    let reaped = reap_parked_cancel_requested_runs(&tx, limit).await?;
    tx.commit().await.map_err(RegistryError::from)?;
    Ok(reaped)
}

async fn reap_parked_cancel_requested_runs<C>(
    tx: &C,
    limit: i64,
) -> Result<u64, RegistryError>
where
    C: GenericClient + Sync,
{
    let rows = tx
        .query(
            "SELECT r.id \
               FROM zeroship.workflow_runs r \
               JOIN zeroship.apps app ON app.id = r.app_id \
               JOIN zeroship.plans plan ON plan.id = app.plan_id \
              WHERE r.cancel_requested \
                AND r.state IN ('queued','sleeping','waiting') \
                AND app.workflows_enabled \
                AND plan.workflows_allowed \
                AND NOT plan.archived \
              ORDER BY r.wake_at NULLS FIRST, r.id \
              LIMIT $1 \
              FOR UPDATE SKIP LOCKED",
            &[&limit],
        )
        .await
        .map_err(RegistryError::from)?;

    let mut reaped = 0;
    for row in rows {
        let run_id: String = row.get("id");
        if cancel_requested_run(tx, &run_id).await? {
            reaped += 1;
        }
    }
    Ok(reaped)
}

async fn rearm_waiting_runs_with_pending_signals<C>(tx: &C) -> Result<u64, RegistryError>
where
    C: GenericClient + Sync,
{
    tx.execute(
        "UPDATE zeroship.workflow_runs r \
            SET wake_at = now() \
           FROM zeroship.apps app \
           JOIN zeroship.plans plan ON plan.id = app.plan_id \
          WHERE r.state = 'waiting' \
            AND r.app_id = app.id \
            AND app.workflows_enabled \
            AND plan.workflows_allowed \
            AND NOT plan.archived \
            AND (r.wake_at IS NULL OR r.wake_at > now()) \
            AND EXISTS ( \
                SELECT 1 \
                  FROM zeroship.workflow_steps s \
                  JOIN zeroship.workflow_signals sig \
                    ON sig.run_id = r.id \
                   AND sig.consumed_by IS NULL \
                   AND sig.type = s.signal_type \
                 WHERE s.run_id = r.id \
                   AND s.state = 'running' \
                   AND s.kind IN ('wait_signal','child') \
            )",
        &[],
    )
    .await
    .map_err(RegistryError::from)
}

async fn claim_one_locked<C>(
    tx: &C,
    config: &WorkflowEngineConfig,
    candidate: CandidateRun,
) -> Result<Option<ClaimedRun>, RegistryError>
where
    C: GenericClient + Sync,
{
    let inflight = tx
        .query(
            "SELECT COUNT(*)::bigint AS n \
               FROM zeroship.workflow_runs \
              WHERE app_id = $1 \
                AND id <> $2 \
                AND state IN ('running','compensating') \
                AND claimed_by IS NOT NULL \
                AND lease_expires IS NOT NULL \
                AND lease_expires > now()",
            &[&candidate.app_id, &candidate.run_id],
        )
        .await
        .map_err(RegistryError::from)?;
    let inflight: i64 = inflight[0].get("n");
    if inflight >= config.max_inflight_per_app {
        return Ok(None);
    }

    if candidate.cancel_requested && candidate.state != "compensating" {
        cancel_requested_run(tx, &candidate.run_id).await?;
        return Ok(None);
    }

    if candidate.state == "compensating"
        && !has_due_compensation(tx, &candidate.run_id).await?
    {
        finalize_compensation_if_drained(tx, &candidate.run_id).await?;
        return Ok(None);
    }

    let dispatch_nonce = typed_id::new_workflow_dispatch_id();
    let lease_expires = Utc::now() + chrono::Duration::milliseconds(config.claim_ttl_ms);
    if let Some(key) = candidate.waiting_step_key.as_deref() {
        if !resolve_due_waiting_step(tx, &candidate.run_id, key, &dispatch_nonce).await? {
            return Ok(None);
        }
    }

    let rows = tx
        .query(
            "UPDATE zeroship.workflow_runs \
                SET claimed_by = $1, \
                    lease_expires = $2, \
                    dispatch_nonce = $3, \
                    claim_epoch = claim_epoch + 1, \
                    state = CASE WHEN state = 'compensating' THEN 'compensating' ELSE 'running' END, \
                    last_dispatch_at = now() \
              WHERE id = $4 \
                AND wake_at <= now() \
                AND state IN ('queued','running','sleeping','waiting','compensating') \
                AND (claimed_by IS NULL OR lease_expires IS NULL OR lease_expires <= now()) \
              RETURNING id",
            &[
                &config.owner_id,
                &lease_expires,
                &dispatch_nonce,
                &candidate.run_id,
            ],
        )
        .await
        .map_err(RegistryError::from)?;
    if rows.is_empty() {
        return Ok(None);
    }

    let journal = load_journal(tx, &candidate.run_id).await?;
    Ok(Some(ClaimedRun {
        request: StepRequest {
            run_id: candidate.run_id,
            app_id: candidate.app_id,
            workflow_name: candidate.workflow_name,
            deploy_id: candidate.deploy_id,
            deploy_hash: candidate.deploy_hash,
            dispatch_nonce,
            phase: if candidate.state == "compensating" {
                "compensating".to_string()
            } else {
                "running".to_string()
            },
            input: candidate.input,
            started_at: candidate.started_at,
            journal,
        },
    }))
}

async fn load_journal<C>(conn: &C, run_id: &str) -> Result<Vec<JournalStep>, RegistryError>
where
    C: GenericClient + Sync,
{
    let rows = conn
        .query(
            "SELECT ordinal, name, name_occurrence, kind, state, output, error, child_run_id, \
                    output_kind, output_hash, output_size, output_content_type, \
                    compensation_state \
               FROM zeroship.workflow_steps \
              WHERE run_id = $1 \
              ORDER BY ordinal",
            &[&run_id],
        )
        .await
        .map_err(RegistryError::from)?;
    Ok(rows
        .into_iter()
        .map(|row| JournalStep {
            output_ref: workflow_output_ref_from_row(
                row.get("output_kind"),
                row.get("output_hash"),
                row.get("output_size"),
                row.get("output_content_type"),
            ),
            ordinal: row.get("ordinal"),
            name: row.get("name"),
            name_occurrence: row.get("name_occurrence"),
            kind: row.get("kind"),
            state: row.get("state"),
            output: row.get("output"),
            error: row.get("error"),
            child_run_id: row.get("child_run_id"),
            compensation_state: row.get("compensation_state"),
        })
        .collect())
}

fn workflow_output_ref_from_row(
    output_kind: String,
    output_hash: Option<String>,
    output_size: Option<i64>,
    output_content_type: Option<String>,
) -> Option<WorkflowOutputRef> {
    if output_kind != "blob" {
        return None;
    }
    Some(WorkflowOutputRef {
        hash: output_hash?,
        size: output_size?,
        content_type: output_content_type,
    })
}

fn workflow_output_ref_from_value(value: &Value) -> Option<WorkflowOutputRef> {
    let hash = value.get("hash")?.as_str()?.to_string();
    let size = value.get("size")?.as_i64()?;
    let content_type = value
        .get("contentType")
        .or_else(|| value.get("content_type"))
        .and_then(Value::as_str)
        .map(str::to_string);
    Some(WorkflowOutputRef {
        hash,
        size,
        content_type,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum WaitingStep {
    Sleep {
        ordinal: i32,
        name: String,
    },
    WaitSignal {
        ordinal: i32,
        name: String,
        signal_type: String,
        max_signal_age_ms: Option<i64>,
    },
    Child {
        ordinal: i32,
        name: String,
    },
}

fn parse_waiting_step_key(key: &str) -> Result<WaitingStep, RegistryError> {
    let parts: Vec<&str> = key.split(':').collect();
    match parts.as_slice() {
        ["sleep", ordinal, name] => {
            let ordinal = ordinal.parse::<i32>().map_err(|_| {
                RegistryError::InvalidInput(format!("invalid sleep waiting_step_key ordinal: {key}"))
            })?;
            Ok(WaitingStep::Sleep {
                ordinal,
                name: (*name).to_string(),
            })
        }
        ["wait", ordinal, name, signal_type] => {
            let ordinal = ordinal.parse::<i32>().map_err(|_| {
                RegistryError::InvalidInput(format!("invalid wait waiting_step_key ordinal: {key}"))
            })?;
            Ok(WaitingStep::WaitSignal {
                ordinal,
                name: (*name).to_string(),
                signal_type: (*signal_type).to_string(),
                max_signal_age_ms: None,
            })
        }
        ["wait", ordinal, name, signal_type, max_age] => {
            let ordinal = ordinal.parse::<i32>().map_err(|_| {
                RegistryError::InvalidInput(format!("invalid wait waiting_step_key ordinal: {key}"))
            })?;
            let max_signal_age_ms = max_age.parse::<i64>().map_err(|_| {
                RegistryError::InvalidInput(format!("invalid wait waiting_step_key max age: {key}"))
            })?;
            Ok(WaitingStep::WaitSignal {
                ordinal,
                name: (*name).to_string(),
                signal_type: (*signal_type).to_string(),
                max_signal_age_ms: Some(max_signal_age_ms),
            })
        }
        ["child", ordinal, name] => {
            let ordinal = ordinal.parse::<i32>().map_err(|_| {
                RegistryError::InvalidInput(format!("invalid child waiting_step_key ordinal: {key}"))
            })?;
            Ok(WaitingStep::Child {
                ordinal,
                name: (*name).to_string(),
            })
        }
        _ => Err(RegistryError::InvalidInput(format!(
            "unrecognized waiting_step_key: {key}"
        ))),
    }
}

async fn resolve_due_waiting_step<C>(
    tx: &C,
    run_id: &str,
    key: &str,
    dispatch_nonce: &str,
) -> Result<bool, RegistryError>
where
    C: GenericClient + Sync,
{
    match parse_waiting_step_key(key)? {
        WaitingStep::Sleep { ordinal, name } => {
            if insert_resolved_step(
                tx,
                &StepCheckpoint {
                    ordinal,
                    name,
                    name_occurrence: 0,
                    kind: "sleep".to_string(),
                    state: "completed".to_string(),
                    output: None,
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
                },
                run_id,
                dispatch_nonce,
                1,
            )
            .await? == StepWriteOutcome::CapExceeded
            {
                return Ok(false);
            }
            tx.execute(
                "UPDATE zeroship.workflow_runs \
                    SET waiting_step_key = NULL, wake_at = now() \
                  WHERE id = $1",
                &[&run_id],
            )
            .await
            .map_err(RegistryError::from)?;
            Ok(true)
        }
        WaitingStep::WaitSignal {
            ordinal,
            name,
            signal_type,
            max_signal_age_ms,
        } => {
            let step_rows = tx
                .query(
                    "SELECT wake_at \
                       FROM zeroship.workflow_steps \
                      WHERE run_id = $1 \
                        AND ordinal = $2 \
                        AND name = $3 \
                        AND kind = 'wait_signal' \
                        AND state = 'running' \
                      FOR UPDATE",
                    &[&run_id, &ordinal, &name],
                )
                .await
                .map_err(RegistryError::from)?;
            let Some(step_row) = step_rows.first() else {
                return Err(RegistryError::InvalidInput(format!(
                    "waiting_step_key {key} has no running workflow_steps row"
                )));
            };
            let deadline: Option<DateTime<Utc>> = step_row.get("wake_at");
            let now = Utc::now();
            let min_created_at =
                max_signal_age_ms.map(|age| now - chrono::Duration::milliseconds(age));
            if let Some(stale_cutoff) = min_created_at.as_ref() {
                tx.execute(
                    "UPDATE zeroship.workflow_signals \
                        SET consumed_by = $1 \
                      WHERE run_id = $1 \
                        AND type = $2 \
                        AND consumed_by IS NULL \
                        AND created_at < $3",
                    &[&run_id, &signal_type, stale_cutoff],
                )
                .await
                .map_err(RegistryError::from)?;
            }
            let signal = tx
                .query(
                    "SELECT id, payload, created_at, origin, delivery, topic \
                       FROM zeroship.workflow_signals \
                      WHERE run_id = $1 \
                        AND type = $2 \
                        AND consumed_by IS NULL \
                        AND ($3::timestamptz IS NULL OR created_at >= $3) \
                      ORDER BY created_at, id \
                      LIMIT 1 \
                      FOR UPDATE SKIP LOCKED",
                    &[&run_id, &signal_type, &min_created_at],
                )
                .await
                .map_err(RegistryError::from)?;

            let Some(row) = signal.first() else {
                if deadline.is_some_and(|deadline| deadline <= now) {
                    if insert_resolved_step(
                        tx,
                        &StepCheckpoint {
                            ordinal,
                            name,
                            name_occurrence: 0,
                            kind: "wait_signal".to_string(),
                            state: "failed".to_string(),
                            output: None,
                            output_ref: None,
                            error: Some(serde_json::json!({
                                "type": "WorkflowTimeoutError",
                                "message": format!("workflow signal wait timed out for {signal_type}"),
                                "retryable": false,
                            })),
                            wake_at: None,
                            signal_type: Some(signal_type),
                            max_signal_age_ms,
                            consumed_signal_id: None,
                            topic: None,
                            child_run_id: None,
                            child_workflow_name: None,
                            child_input: None,
                            child_options: None,
                            compensation_state: None,
                            compensation_max_attempts: 1,
                        },
                        run_id,
                        dispatch_nonce,
                        1,
                    )
                    .await? == StepWriteOutcome::CapExceeded
                    {
                        return Ok(false);
                    }
                    tx.execute(
                        "UPDATE zeroship.workflow_runs \
                            SET waiting_step_key = NULL, wake_at = now() \
                          WHERE id = $1",
                        &[&run_id],
                    )
                    .await
                    .map_err(RegistryError::from)?;
                    delete_workflow_subscription(tx, run_id, ordinal).await?;
                    return Ok(true);
                }

                tx.execute(
                    "UPDATE zeroship.workflow_runs \
                        SET state = 'waiting', wake_at = $2 \
                      WHERE id = $1",
                    &[&run_id, &deadline],
                )
                .await
                .map_err(RegistryError::from)?;
                return Ok(false);
            };

            let signal_id: String = row.get("id");
            let payload: Option<Value> = row.get("payload");
            let created_at: DateTime<Utc> = row.get("created_at");
            let origin: String = row.get("origin");
            let delivery: String = row.get("delivery");
            let topic: Option<String> = row.get("topic");
            if insert_resolved_step(
                tx,
                &StepCheckpoint {
                    ordinal,
                    name,
                    name_occurrence: 0,
                    kind: "wait_signal".to_string(),
                    state: "completed".to_string(),
                    output: Some(serde_json::json!({
                        "id": signal_id.clone(),
                        "type": signal_type.clone(),
                        "payload": payload,
                        "createdAt": created_at.to_rfc3339(),
                        "receivedAt": created_at.to_rfc3339(),
                        "origin": origin,
                        "delivery": delivery,
                        "topic": topic.clone(),
                    })),
                    output_ref: None,
                    error: None,
                    wake_at: None,
                    signal_type: Some(signal_type),
                    max_signal_age_ms,
                    consumed_signal_id: Some(signal_id.clone()),
                    topic,
                    child_run_id: None,
                    child_workflow_name: None,
                    child_input: None,
                    child_options: None,
                    compensation_state: None,
                    compensation_max_attempts: 1,
                },
                run_id,
                dispatch_nonce,
                1,
            )
            .await? == StepWriteOutcome::CapExceeded
            {
                return Ok(false);
            }
            tx.execute(
                "UPDATE zeroship.workflow_signals \
                    SET consumed_by = $1 \
                  WHERE id = $2 AND consumed_by IS NULL",
                &[&run_id, &signal_id],
            )
            .await
            .map_err(RegistryError::from)?;
            tx.execute(
                "UPDATE zeroship.workflow_runs \
                    SET waiting_step_key = NULL, wake_at = now() \
                  WHERE id = $1",
                &[&run_id],
            )
            .await
            .map_err(RegistryError::from)?;
            delete_workflow_subscription(tx, run_id, ordinal).await?;
            Ok(true)
        }
        WaitingStep::Child { ordinal, name } => {
            let step_rows = tx
                .query(
                    "SELECT wake_at, child_run_id, name_occurrence \
                       FROM zeroship.workflow_steps \
                      WHERE run_id = $1 \
                        AND ordinal = $2 \
                        AND name = $3 \
                        AND kind = 'child' \
                        AND state = 'running' \
                      FOR UPDATE",
                    &[&run_id, &ordinal, &name],
                )
                .await
                .map_err(RegistryError::from)?;
            let Some(step_row) = step_rows.first() else {
                return Err(RegistryError::InvalidInput(format!(
                    "child waiting_step_key child:{ordinal}:{name} has no running workflow_steps row"
                )));
            };
            let deadline: Option<DateTime<Utc>> = step_row.get("wake_at");
            let child_run_id: Option<String> = step_row.get("child_run_id");
            let name_occurrence: i32 = step_row.get("name_occurrence");
            let mut signal_type = child_signal_type(ordinal);
            let now = Utc::now();
            let signal = tx
                .query(
                    "SELECT id, payload \
                       FROM zeroship.workflow_signals \
                      WHERE run_id = $1 \
                        AND type = $2 \
                        AND consumed_by IS NULL \
                      ORDER BY created_at, id \
                      LIMIT 1 \
                      FOR UPDATE SKIP LOCKED",
                    &[&run_id, &signal_type],
                )
                .await
                .map_err(RegistryError::from)?;

            let mut resolved_ordinal = ordinal;
            let mut resolved_name = name;
            let mut resolved_name_occurrence = name_occurrence;
            let mut resolved_child_run_id = child_run_id;
            let current_signal = signal
                .first()
                .map(|row| (row.get::<_, String>("id"), row.get::<_, Option<Value>>("payload")));
            let resolved_signal = if let Some(signal) = current_signal {
                Some(signal)
            } else {
                let any_signal = tx
                    .query(
                        "SELECT sig.id, sig.payload, s.ordinal, s.name, s.name_occurrence, s.child_run_id \
                           FROM zeroship.workflow_steps s \
                           JOIN zeroship.workflow_signals sig \
                             ON sig.run_id = s.run_id \
                            AND sig.type = s.signal_type \
                            AND sig.consumed_by IS NULL \
                          WHERE s.run_id = $1 \
                            AND s.kind = 'child' \
                            AND s.state = 'running' \
                          ORDER BY sig.created_at, sig.id \
                          LIMIT 1 \
                          FOR UPDATE OF sig SKIP LOCKED",
                        &[&run_id],
                    )
                    .await
                    .map_err(RegistryError::from)?;
                any_signal.first().map(|row| {
                    resolved_ordinal = row.get("ordinal");
                    resolved_name = row.get("name");
                    resolved_name_occurrence = row.get("name_occurrence");
                    resolved_child_run_id = row.get("child_run_id");
                    signal_type = child_signal_type(resolved_ordinal);
                    (row.get::<_, String>("id"), row.get::<_, Option<Value>>("payload"))
                })
            };

            let Some((signal_id, payload)) = resolved_signal else {
                if deadline.is_some_and(|deadline| deadline <= now) {
                    if let Some(child_run_id) = resolved_child_run_id.as_ref() {
                        tx.execute(
                            "UPDATE zeroship.workflow_runs \
                                SET cancel_requested = true, wake_at = now() \
                              WHERE id = $1 \
                                AND parent_cascade \
                                AND state NOT IN ('completed','failed','cancelled','stalled')",
                            &[child_run_id],
                        )
                        .await
                        .map_err(RegistryError::from)?;
                    }
                    if insert_resolved_step(
                        tx,
                            &StepCheckpoint {
                                ordinal: resolved_ordinal,
                                name: resolved_name,
                                name_occurrence: resolved_name_occurrence,
                                kind: "child".to_string(),
                                state: "failed".to_string(),
                            output: None,
                            output_ref: None,
                            error: Some(serde_json::json!({
                                "type": "ChildTimeoutError",
                                "message": format!("child workflow timed out for {signal_type}"),
                                "retryable": false,
                            })),
                            wake_at: None,
                            signal_type: Some(signal_type),
                            max_signal_age_ms: None,
                            consumed_signal_id: None,
                            topic: None,
                            child_run_id: resolved_child_run_id,
                            child_workflow_name: None,
                            child_input: None,
                            child_options: None,
                            compensation_state: None,
                            compensation_max_attempts: 1,
                        },
                        run_id,
                        dispatch_nonce,
                        1,
                    )
                    .await? == StepWriteOutcome::CapExceeded
                    {
                        return Ok(false);
                    }
                    tx.execute(
                        "UPDATE zeroship.workflow_runs \
                            SET waiting_step_key = NULL, wake_at = now() \
                          WHERE id = $1",
                        &[&run_id],
                    )
                    .await
                    .map_err(RegistryError::from)?;
                    return Ok(true);
                }

                tx.execute(
                    "UPDATE zeroship.workflow_runs \
                        SET state = 'waiting', wake_at = $2 \
                      WHERE id = $1",
                    &[&run_id, &deadline],
                )
                .await
                .map_err(RegistryError::from)?;
                return Ok(false);
            };

            let payload = payload.unwrap_or(Value::Null);
            let ok = payload
                .get("ok")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let output_ref = payload
                .get("outputRef")
                .or_else(|| payload.get("output_ref"))
                .and_then(workflow_output_ref_from_value);
            let checkpoint = StepCheckpoint {
                ordinal: resolved_ordinal,
                name: resolved_name,
                name_occurrence: resolved_name_occurrence,
                kind: "child".to_string(),
                state: if ok { "completed" } else { "failed" }.to_string(),
                output: if ok && output_ref.is_none() {
                    payload.get("output").cloned()
                } else {
                    None
                },
                output_ref,
                error: if ok {
                    None
                } else {
                    Some(payload.get("error").cloned().unwrap_or_else(|| {
                        serde_json::json!({
                            "type": "PermanentError",
                            "message": "child workflow failed",
                            "retryable": false,
                        })
                    }))
                },
                wake_at: None,
                signal_type: Some(signal_type),
                max_signal_age_ms: None,
                consumed_signal_id: Some(signal_id.clone()),
                topic: None,
                child_run_id: resolved_child_run_id,
                child_workflow_name: None,
                child_input: None,
                child_options: None,
                compensation_state: None,
                compensation_max_attempts: 1,
            };
            if insert_resolved_step(tx, &checkpoint, run_id, dispatch_nonce, 1).await?
                == StepWriteOutcome::CapExceeded
            {
                return Ok(false);
            }
            tx.execute(
                "UPDATE zeroship.workflow_signals \
                    SET consumed_by = $1 \
                  WHERE id = $2 AND consumed_by IS NULL",
                &[&run_id, &signal_id],
            )
            .await
            .map_err(RegistryError::from)?;
            tx.execute(
                "UPDATE zeroship.workflow_runs \
                    SET waiting_step_key = NULL, wake_at = now() \
                  WHERE id = $1",
                &[&run_id],
            )
            .await
            .map_err(RegistryError::from)?;
            Ok(true)
        }
    }
}

fn spawn_dispatch<D>(
    registry: Registry,
    dispatcher: Arc<D>,
    config: WorkflowEngineConfig,
    claim: ClaimedRun,
) where
    D: StepDispatcher + 'static,
{
    INFLIGHT_DISPATCHES.fetch_add(1, Ordering::SeqCst);
    let heartbeat = spawn_heartbeat(
        registry.clone(),
        config.owner_id.clone(),
        claim.request.run_id.clone(),
        claim.request.dispatch_nonce.clone(),
        config.claim_ttl_ms,
        Duration::from_millis(config.heartbeat_ms),
    );
    compio::runtime::spawn(async move {
        let outcome = dispatcher.dispatch(claim.request).await;
        heartbeat.store(false, Ordering::SeqCst);
        match outcome {
            DispatchOutcome::Completed(result) => {
                match apply_step_result_on_registry(&registry, &config, result.clone()).await {
                    Ok(_) => {}
                    Err(ApplyError::Deadlock(msg)) => {
                        tracing::warn!(error = %msg, run_id = %result.run_id, "workflow_engine: apply deadlock, requeueing claim");
                        if let Err(e) =
                            requeue_claim(&registry, &config.owner_id, &result.run_id, &result.dispatch_nonce).await
                        {
                            tracing::error!(error = %e, run_id = %result.run_id, "workflow_engine: deadlock requeue failed");
                        }
                    }
                    Err(ApplyError::Invalid(msg)) => {
                        tracing::error!(error = %msg, run_id = %result.run_id, "workflow_engine: invalid StepResult");
                    }
                    Err(ApplyError::Db(e)) => {
                        tracing::error!(error = %e, run_id = %result.run_id, "workflow_engine: apply failed");
                    }
                }
            }
            DispatchOutcome::Backpressure {
                run_id,
                dispatch_nonce,
                reason,
            } => {
                tracing::warn!(
                    run_id = %run_id,
                    reason = %reason,
                    "workflow_engine: gateway backpressure, parking claim"
                );
                if let Err(e) =
                    park_backpressure_claim(&registry, &config.owner_id, &run_id, &dispatch_nonce).await
                {
                    tracing::error!(error = %e, run_id = %run_id, "workflow_engine: backpressure park failed");
                }
            }
        }
        INFLIGHT_DISPATCHES.fetch_sub(1, Ordering::SeqCst);
    })
    .detach();
}

fn spawn_heartbeat(
    registry: Registry,
    owner_id: String,
    run_id: String,
    dispatch_nonce: String,
    claim_ttl_ms: i64,
    interval: Duration,
) -> Arc<AtomicBool> {
    let active = Arc::new(AtomicBool::new(true));
    let heartbeat_active = Arc::clone(&active);
    compio::runtime::spawn(async move {
        while heartbeat_active.load(Ordering::SeqCst) {
            compio::time::sleep(interval).await;
            if !heartbeat_active.load(Ordering::SeqCst) {
                break;
            }
            let beat = async {
                let conn = registry.conn().await?;
                let lease_expires = Utc::now() + chrono::Duration::milliseconds(claim_ttl_ms);
                conn.execute(
                    "UPDATE zeroship.workflow_runs \
                        SET lease_expires = $1 \
                      WHERE id = $2 \
                        AND claimed_by = $3 \
                        AND dispatch_nonce = $4",
                    &[&lease_expires, &run_id, &owner_id, &dispatch_nonce],
                )
                .await
                .map_err(RegistryError::from)?;
                Ok::<(), RegistryError>(())
            }
            .await;
            if let Err(e) = beat {
                tracing::warn!(error = %e, run_id = %run_id, "workflow_engine heartbeat failed");
            }
        }
    })
    .detach();
    active
}

#[derive(Debug)]
enum ApplyError {
    Invalid(String),
    Deadlock(String),
    Db(RegistryError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StepWriteOutcome {
    Wrote,
    Noop,
    CapExceeded,
}

#[derive(Debug, Clone)]
struct CompensationApplyOutcome {
    ordinal: i32,
    name: String,
    name_occurrence: i32,
    state: &'static str,
    error: Option<Value>,
}

#[derive(Debug, Clone, Copy)]
struct CompensationProgress {
    total: i64,
    completed: i64,
    failed: i64,
    pending: i64,
    running: i64,
}

impl CompensationProgress {
    fn remaining(self) -> i64 {
        self.pending + self.running
    }

    fn terminal_outcome(self) -> &'static str {
        if self.failed > 0 {
            "partial"
        } else {
            "completed"
        }
    }
}

impl fmt::Display for ApplyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(msg) => write!(f, "invalid workflow StepResult: {msg}"),
            Self::Deadlock(msg) => write!(f, "deadlock: {msg}"),
            Self::Db(e) => write!(f, "{e}"),
        }
    }
}

impl From<RegistryError> for ApplyError {
    fn from(value: RegistryError) -> Self {
        Self::Db(value)
    }
}

fn map_apply_error(e: compio_postgres::Error) -> ApplyError {
    if e.code() == Some(&SqlState::T_R_DEADLOCK_DETECTED) {
        ApplyError::Deadlock(e.to_string())
    } else {
        ApplyError::Db(RegistryError::from(e))
    }
}

fn compensation_outcomes_from_step_outcomes(
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

/// Public deterministic apply path for tests and future control handlers.
#[allow(clippy::future_not_send)]
pub async fn apply_step_result(
    state: &AppState,
    owner_id: &str,
    result: StepResult,
) -> Result<bool, RegistryError> {
    let mut config = WorkflowEngineConfig::default();
    config.owner_id = owner_id.to_string();
    apply_step_result_on_registry(&state.registry, &config, result)
        .await
        .map_err(|e| match e {
            ApplyError::Invalid(msg) => RegistryError::InvalidInput(msg),
            ApplyError::Deadlock(msg) => RegistryError::Database(format!("retryable deadlock: {msg}")),
            ApplyError::Db(e) => e,
        })
}

/// Public deterministic apply path with scheduler config overrides for tests.
#[allow(clippy::future_not_send)]
pub async fn apply_step_result_with_config(
    state: &AppState,
    config: WorkflowEngineConfig,
    result: StepResult,
) -> Result<bool, RegistryError> {
    apply_step_result_on_registry(&state.registry, &config, result)
        .await
        .map_err(|e| match e {
            ApplyError::Invalid(msg) => RegistryError::InvalidInput(msg),
            ApplyError::Deadlock(msg) => RegistryError::Database(format!("retryable deadlock: {msg}")),
            ApplyError::Db(e) => e,
        })
}

async fn apply_step_result_on_registry(
    registry: &Registry,
    config: &WorkflowEngineConfig,
    mut result: StepResult,
) -> Result<bool, ApplyError> {
    let compensation_outcomes =
        compensation_outcomes_from_step_outcomes(&result.outcomes).map_err(ApplyError::Invalid)?;
    let (checkpoints, run_update) = fold_outcomes(&result.outcomes).map_err(ApplyError::Invalid)?;
    result.checkpoints = checkpoints;
    result.run_update = run_update;
    result.checkpoints.sort_by_key(|s| s.ordinal);
    let batch_width = i16::try_from(result.checkpoints.len()).unwrap_or(i16::MAX);
    let mut conn = registry.conn().await.map_err(ApplyError::Db)?;
    let tx = conn.transaction().await.map_err(map_apply_error)?;

    // Row-lock-first: this is intentionally the first statement in the txn.
    let rows = tx
        .query(
            "SELECT app_id, deploy_id, claimed_by, state, dispatch_nonce, stuck_strikes, \
                    tree_depth, compensation_target, error \
               FROM zeroship.workflow_runs \
              WHERE id = $1 \
              FOR UPDATE",
            &[&result.run_id],
        )
        .await
        .map_err(map_apply_error)?;
    let Some(row) = rows.first() else {
        tx.commit().await.map_err(map_apply_error)?;
        return Ok(false);
    };
    let app_id: Uuid = row.get("app_id");
    let deploy_id: String = row.get("deploy_id");
    let claimed_by: Option<String> = row.get("claimed_by");
    let state: String = row.get("state");
    let dispatch_nonce: Option<String> = row.get("dispatch_nonce");
    let stuck_strikes: i16 = row.get("stuck_strikes");
    let tree_depth: i16 = row.get("tree_depth");
    let compensation_target: Option<String> = row.get("compensation_target");
    let current_error: Option<Value> = row.get("error");
    if claimed_by.as_deref() != Some(config.owner_id.as_str())
        || dispatch_nonce.as_deref() != Some(result.dispatch_nonce.as_str())
        || !matches!(state.as_str(), "running" | "paused" | "compensating")
    {
        tx.commit().await.map_err(map_apply_error)?;
        return Ok(false);
    }
    if state == "compensating" {
        let applied = apply_compensation_result(
            &tx,
            config,
            &result.run_id,
            &result.dispatch_nonce,
            compensation_target.as_deref(),
            current_error,
            &compensation_outcomes,
        )
        .await?;
        tx.commit().await.map_err(map_apply_error)?;
        return Ok(applied);
    }
    if !compensation_outcomes.is_empty() {
        tx.commit().await.map_err(map_apply_error)?;
        return Err(ApplyError::Invalid(
            "compensation outcome received outside compensating phase".to_string(),
        ));
    }

    let child_checkpoint_count = result
        .checkpoints
        .iter()
        .filter(|checkpoint| checkpoint.kind == "child" && checkpoint.state == "running")
        .count();
    let start_many_over_cap = child_checkpoint_count > config.max_start_many_batch;

    let mut wrote_checkpoints = 0usize;
    for checkpoint in &mut result.checkpoints {
        if checkpoint.kind == "child" && checkpoint.state == "running" {
            if start_many_over_cap {
                fail_child_checkpoint_with_limit(
                    checkpoint,
                    format!(
                        "child workflow batch exceeds maxStartManyBatch ({} > {})",
                        child_checkpoint_count, config.max_start_many_batch
                    ),
                );
                result.run_update = RunUpdate::Queued;
            } else if let Err(error) = prepare_child_spawn(
                &tx,
                config,
                &result.run_id,
                &app_id,
                &deploy_id,
                tree_depth,
                checkpoint,
            )
            .await
            .map_err(ApplyError::Db)?
            {
                fail_child_checkpoint_with_limit(checkpoint, error);
                result.run_update = RunUpdate::Queued;
            }
        }
        match insert_resolved_step(
            &tx,
            checkpoint,
            &result.run_id,
            &result.dispatch_nonce,
            batch_width,
        )
            .await
            .map_err(ApplyError::Db)?
        {
            StepWriteOutcome::CapExceeded => {
                tx.commit().await.map_err(map_apply_error)?;
                return Ok(true);
            }
            StepWriteOutcome::Wrote => {
                wrote_checkpoints += 1;
                if checkpoint.kind == "wait_signal" && checkpoint.state == "running" {
                    upsert_workflow_subscription(&tx, &app_id, &result.run_id, checkpoint)
                        .await
                        .map_err(ApplyError::Db)?;
                }
                if checkpoint.kind == "wait_signal"
                    && matches!(checkpoint.state.as_str(), "completed" | "failed")
                {
                    delete_workflow_subscription(&tx, &result.run_id, checkpoint.ordinal)
                        .await
                        .map_err(ApplyError::Db)?;
                }
                if let Some(signal_id) = checkpoint.consumed_signal_id.as_ref() {
                    tx.execute(
                        "UPDATE zeroship.workflow_signals \
                            SET consumed_by = $1 \
                          WHERE id = $2 AND consumed_by IS NULL",
                        &[&result.run_id, signal_id],
                    )
                    .await
                    .map_err(map_apply_error)?;
                }
            }
            StepWriteOutcome::Noop => {
                if checkpoint.kind == "wait_signal" && checkpoint.state == "running" {
                    upsert_workflow_subscription(&tx, &app_id, &result.run_id, checkpoint)
                        .await
                        .map_err(ApplyError::Db)?;
                }
                if checkpoint.kind == "wait_signal"
                    && matches!(checkpoint.state.as_str(), "completed" | "failed")
                {
                    delete_workflow_subscription(&tx, &result.run_id, checkpoint.ordinal)
                        .await
                        .map_err(ApplyError::Db)?;
                }
                if let Some(signal_id) = checkpoint.consumed_signal_id.as_ref() {
                    tx.execute(
                        "UPDATE zeroship.workflow_signals \
                            SET consumed_by = $1 \
                          WHERE id = $2 AND consumed_by IS NULL",
                        &[&result.run_id, signal_id],
                    )
                    .await
                    .map_err(map_apply_error)?;
                }
            }
        }
    }

    let next_ordinal = result
        .checkpoints
        .iter()
        .map(|s| s.ordinal.saturating_add(1))
        .max()
        .unwrap_or(0);
    let made_progress = wrote_checkpoints > 0
        || !matches!(result.run_update, RunUpdate::Queued);
    let zero_progress = !made_progress && state == "running";
    let next_stuck_strikes = if zero_progress {
        stuck_strikes.saturating_add(1)
    } else {
        0
    };
    if zero_progress && next_stuck_strikes >= config.stuck_strike_limit.max(1) {
        result.run_update = RunUpdate::Stalled {
            error: stalled_error(next_stuck_strikes, config.stuck_strike_limit.max(1)),
        };
    }

    let wake_at = result.run_update.wake_at();
    let waiting_step_key = result.run_update.waiting_step_key(&result.checkpoints);
    if state == "paused" {
        let paused_from_status = match result.run_update {
            RunUpdate::Queued
            | RunUpdate::Completed { .. }
            | RunUpdate::Failed { .. }
            | RunUpdate::Stalled { .. }
            | RunUpdate::Cancelled => {
                "queued"
            }
            RunUpdate::Sleeping { .. } => "sleeping",
            RunUpdate::Waiting { .. } => "waiting",
        };
        let paused_wake_at = wake_at.or_else(|| (paused_from_status == "queued").then(Utc::now));
        tx.execute(
            "UPDATE zeroship.workflow_runs \
                SET state = 'paused', \
                    wake_at = $1, \
                    next_ordinal = GREATEST(next_ordinal, $2), \
                    waiting_step_key = $3, \
                    paused_from_status = $4, \
                    stuck_strikes = 0, \
                    claimed_by = NULL, \
                    lease_expires = NULL, \
                    dispatch_nonce = NULL \
              WHERE id = $5 \
                AND claimed_by = $6 \
                AND dispatch_nonce = $7 \
                AND state = 'paused'",
            &[
                &paused_wake_at,
                &next_ordinal,
                &waiting_step_key,
                &paused_from_status,
                &result.run_id,
                &config.owner_id,
                &result.dispatch_nonce,
            ],
        )
        .await
        .map_err(map_apply_error)?;
    } else {
        let mut state = result.run_update.state().to_string();
        let output_ref = result.run_update.output_ref();
        let output = if output_ref.is_some() {
            None
        } else {
            result.run_update.output()
        };
        let output_kind = if output_ref.is_some() { "blob" } else { "inline" };
        let output_hash = output_ref.as_ref().map(|value| value.hash.clone());
        let output_size = output_ref.as_ref().map(|value| value.size);
        let output_content_type = output_ref
            .as_ref()
            .and_then(|value| value.content_type.clone())
            .or_else(|| output_ref.as_ref().map(|_| "application/json".to_string()));
        let run_journal_delta = run_output_journal_bytes(&tx, &output, output_ref.as_ref())
            .await
            .map_err(ApplyError::Db)?;
        let blob_bytes_delta = output_ref.as_ref().map_or(0, |value| value.size.max(0));
        let mut error = result.run_update.error();
        let mut compensation_target: Option<String> = None;
        let mut compensation_outcome: Option<String> = None;
        let mut wake_at = wake_at;
        if state == "failed"
            && error
                .as_ref()
                .is_some_and(should_enter_compensation_for_error)
            && pending_compensation_count(&tx, &result.run_id)
                .await
                .map_err(ApplyError::Db)?
                > 0
        {
            state = "compensating".to_string();
            compensation_target = Some("failed".to_string());
            compensation_outcome = None;
            wake_at = Some(Utc::now());
            let progress = compensation_progress(&tx, &result.run_id)
                .await
                .map_err(ApplyError::Db)?;
            error = Some(compensation_progress_error(error, progress, None));
        }
        let changed = tx.execute(
            "UPDATE zeroship.workflow_runs \
                SET state = $1, \
                    output = $2, \
                    error = $3, \
                    wake_at = $4, \
                    next_ordinal = GREATEST(next_ordinal, $5), \
                    waiting_step_key = $6, \
                    paused_from_status = NULL, \
                    stuck_strikes = $7, \
                    output_kind = $11, \
                    output_hash = $12, \
                    output_size = $13, \
                    output_content_type = $14, \
                    journal_bytes = journal_bytes + $15, \
                    blob_bytes = blob_bytes + $16, \
                    compensation_target = $17, \
                    compensation_outcome = $18, \
                    claimed_by = NULL, \
                    lease_expires = NULL, \
                    dispatch_nonce = NULL \
              WHERE id = $8 \
                AND claimed_by = $9 \
                AND dispatch_nonce = $10 \
                AND state = 'running'",
            &[
                &state,
                &output,
                &error,
                &wake_at,
                &next_ordinal,
                &waiting_step_key,
                &next_stuck_strikes,
                &result.run_id,
                &config.owner_id,
                &result.dispatch_nonce,
                &output_kind,
                &output_hash,
                &output_size,
                &output_content_type,
                &run_journal_delta,
                &blob_bytes_delta,
                &compensation_target,
                &compensation_outcome,
            ],
        )
        .await
        .map_err(map_apply_error)?;
        if changed > 0 {
            if let Some(output_ref) = output_ref.as_ref() {
                upsert_workflow_blob_ref(&tx, output_ref)
                    .await
                    .map_err(ApplyError::Db)?;
            }
            if matches!(state.as_str(), "completed" | "failed" | "cancelled" | "stalled") {
                emit_child_terminal_hook(
                    &tx,
                    &result.run_id,
                    ChildTerminalPayload {
                        state: &state,
                        output: output.clone(),
                        output_ref,
                        error: error.clone(),
                    },
                )
                .await
                .map_err(ApplyError::Db)?;
            }
            if matches!(state.as_str(), "failed" | "cancelled" | "stalled") {
                cascade_cancel_children(&tx, &result.run_id)
                    .await
                    .map_err(ApplyError::Db)?;
            }
        }
    }

    tx.commit().await.map_err(map_apply_error)?;
    Ok(true)
}

async fn apply_compensation_result<C>(
    conn: &C,
    config: &WorkflowEngineConfig,
    run_id: &str,
    dispatch_nonce: &str,
    compensation_target: Option<&str>,
    current_error: Option<Value>,
    outcomes: &[CompensationApplyOutcome],
) -> Result<bool, ApplyError>
where
    C: GenericClient + Sync,
{
    for outcome in outcomes {
        conn.execute(
            "UPDATE zeroship.workflow_steps s \
                SET compensation_state = $5, \
                    compensation_attempt = compensation_attempt + 1, \
                    compensation_wake_at = NULL, \
                    compensation_error = $6, \
                    compensation_batch_id = $7, \
                    compensation_finished_at = now() \
              WHERE s.run_id = $1 \
                AND s.ordinal = $2 \
                AND s.name = $3 \
                AND s.name_occurrence = $4 \
                AND s.kind = 'run' \
                AND s.compensation_state IN ('pending','running') \
                AND NOT EXISTS ( \
                    SELECT 1 \
                      FROM zeroship.workflow_steps higher \
                     WHERE higher.run_id = s.run_id \
                       AND higher.ordinal > s.ordinal \
                       AND higher.compensation_state IN ('pending','running') \
                )",
            &[
                &run_id,
                &outcome.ordinal,
                &outcome.name,
                &outcome.name_occurrence,
                &outcome.state,
                &outcome.error,
                &dispatch_nonce,
            ],
        )
        .await
        .map_err(map_apply_error)?;
    }

    update_compensating_run_progress(
        conn,
        config,
        run_id,
        dispatch_nonce,
        compensation_target,
        current_error,
    )
    .await
}

async fn update_compensating_run_progress<C>(
    conn: &C,
    config: &WorkflowEngineConfig,
    run_id: &str,
    dispatch_nonce: &str,
    compensation_target: Option<&str>,
    current_error: Option<Value>,
) -> Result<bool, ApplyError>
where
    C: GenericClient + Sync,
{
    let progress = compensation_progress(conn, run_id)
        .await
        .map_err(ApplyError::Db)?;
    let (next_state, wake_at, outcome) = if progress.remaining() > 0 {
        let wake_at = next_compensation_wake_at(conn, run_id)
            .await
            .map_err(ApplyError::Db)?;
        ("compensating".to_string(), wake_at, None)
    } else {
        let target = compensation_target.unwrap_or("failed");
        (
            target.to_string(),
            None,
            Some(progress.terminal_outcome().to_string()),
        )
    };
    let error = compensation_progress_error(current_error, progress, outcome.as_deref());
    let changed = conn
        .execute(
            "UPDATE zeroship.workflow_runs \
                SET state = $1, \
                    output = NULL, \
                    error = $2, \
                    wake_at = $3, \
                    waiting_step_key = NULL, \
                    compensation_outcome = $4, \
                    paused_from_status = NULL, \
                    stuck_strikes = 0, \
                    output_kind = 'inline', \
                    output_hash = NULL, \
                    output_size = NULL, \
                    output_content_type = NULL, \
                    claimed_by = NULL, \
                    lease_expires = NULL, \
                    dispatch_nonce = NULL \
              WHERE id = $5 \
                AND claimed_by = $6 \
                AND dispatch_nonce = $7 \
                AND state = 'compensating'",
            &[
                &next_state,
                &error,
                &wake_at,
                &outcome,
                &run_id,
                &config.owner_id,
                &dispatch_nonce,
            ],
        )
        .await
        .map_err(map_apply_error)?;
    if changed > 0 && matches!(next_state.as_str(), "failed" | "cancelled") {
        emit_child_terminal_hook(
            conn,
            run_id,
            ChildTerminalPayload {
                state: &next_state,
                output: None,
                output_ref: None,
                error: Some(error.clone()),
            },
        )
        .await
        .map_err(ApplyError::Db)?;
        cascade_cancel_children(conn, run_id)
            .await
            .map_err(ApplyError::Db)?;
    }
    Ok(changed > 0)
}

async fn has_due_compensation<C>(conn: &C, run_id: &str) -> Result<bool, RegistryError>
where
    C: GenericClient + Sync,
{
    let rows = conn
        .query(
            "SELECT 1 \
               FROM zeroship.workflow_steps \
              WHERE run_id = $1 \
                AND ( \
                    compensation_state = 'pending' \
                    OR (compensation_state = 'running' \
                        AND (compensation_wake_at IS NULL OR compensation_wake_at <= now())) \
                ) \
              ORDER BY ordinal DESC \
              LIMIT 1",
            &[&run_id],
        )
        .await
        .map_err(RegistryError::from)?;
    Ok(!rows.is_empty())
}

async fn pending_compensation_count<C>(conn: &C, run_id: &str) -> Result<i64, RegistryError>
where
    C: GenericClient + Sync,
{
    let row = conn
        .query_one(
            "SELECT COUNT(*)::bigint AS n \
               FROM zeroship.workflow_steps \
              WHERE run_id = $1 AND compensation_state = 'pending'",
            &[&run_id],
        )
        .await
        .map_err(RegistryError::from)?;
    Ok(row.get("n"))
}

async fn finalize_compensation_if_drained<C>(
    conn: &C,
    run_id: &str,
) -> Result<bool, RegistryError>
where
    C: GenericClient + Sync,
{
    let progress = compensation_progress(conn, run_id).await?;
    if progress.remaining() > 0 {
        return Ok(false);
    }
    let rows = conn
        .query(
            "SELECT compensation_target, error \
               FROM zeroship.workflow_runs \
              WHERE id = $1 AND state = 'compensating' \
              FOR UPDATE",
            &[&run_id],
        )
        .await
        .map_err(RegistryError::from)?;
    let Some(row) = rows.first() else {
        return Ok(false);
    };
    let target = row
        .get::<_, Option<String>>("compensation_target")
        .unwrap_or_else(|| "failed".to_string());
    let current_error: Option<Value> = row.get("error");
    let outcome = progress.terminal_outcome().to_string();
    let error = compensation_progress_error(current_error, progress, Some(outcome.as_str()));
    let changed = conn
        .execute(
            "UPDATE zeroship.workflow_runs \
                SET state = $2, \
                    error = $3, \
                    wake_at = NULL, \
                    waiting_step_key = NULL, \
                    compensation_outcome = $4, \
                    claimed_by = NULL, \
                    lease_expires = NULL, \
                    dispatch_nonce = NULL \
              WHERE id = $1 AND state = 'compensating'",
            &[&run_id, &target, &error, &outcome],
        )
        .await
        .map_err(RegistryError::from)?;
    if changed > 0 && matches!(target.as_str(), "failed" | "cancelled") {
        emit_child_terminal_hook(
            conn,
            run_id,
            ChildTerminalPayload {
                state: &target,
                output: None,
                output_ref: None,
                error: Some(error.clone()),
            },
        )
        .await?;
        cascade_cancel_children(conn, run_id).await?;
    }
    Ok(changed > 0)
}

async fn compensation_progress<C>(
    conn: &C,
    run_id: &str,
) -> Result<CompensationProgress, RegistryError>
where
    C: GenericClient + Sync,
{
    let row = conn
        .query_one(
            "SELECT \
                COUNT(*) FILTER (WHERE compensation_state IS NOT NULL)::bigint AS total, \
                COUNT(*) FILTER (WHERE compensation_state = 'completed')::bigint AS completed, \
                COUNT(*) FILTER (WHERE compensation_state = 'failed')::bigint AS failed, \
                COUNT(*) FILTER (WHERE compensation_state = 'pending')::bigint AS pending, \
                COUNT(*) FILTER (WHERE compensation_state = 'running')::bigint AS running \
               FROM zeroship.workflow_steps \
              WHERE run_id = $1",
            &[&run_id],
        )
        .await
        .map_err(RegistryError::from)?;
    Ok(CompensationProgress {
        total: row.get("total"),
        completed: row.get("completed"),
        failed: row.get("failed"),
        pending: row.get("pending"),
        running: row.get("running"),
    })
}

async fn next_compensation_wake_at<C>(
    conn: &C,
    run_id: &str,
) -> Result<Option<DateTime<Utc>>, RegistryError>
where
    C: GenericClient + Sync,
{
    let row = conn
        .query_one(
            "SELECT \
                EXISTS ( \
                    SELECT 1 FROM zeroship.workflow_steps \
                     WHERE run_id = $1 AND compensation_state = 'pending' \
                ) AS has_pending, \
                MIN(compensation_wake_at) FILTER (WHERE compensation_state = 'running') AS running_wake \
               FROM zeroship.workflow_steps \
              WHERE run_id = $1",
            &[&run_id],
        )
        .await
        .map_err(RegistryError::from)?;
    if row.get::<_, bool>("has_pending") {
        return Ok(Some(Utc::now()));
    }
    Ok(row
        .get::<_, Option<DateTime<Utc>>>("running_wake")
        .or_else(|| Some(Utc::now())))
}

fn compensation_progress_error(
    base: Option<Value>,
    progress: CompensationProgress,
    outcome: Option<&str>,
) -> Value {
    let mut error = match base {
        Some(Value::Object(map)) => Value::Object(map),
        Some(value) => serde_json::json!({
            "type": "Error",
            "message": "workflow failed during compensation",
            "cause": value,
        }),
        None => serde_json::json!({
            "type": "Error",
            "message": "workflow compensation is running",
        }),
    };
    let mut compensation = serde_json::json!({
        "total": progress.total,
        "completed": progress.completed,
        "failed": progress.failed,
    });
    if let Some(outcome) = outcome {
        compensation["outcome"] = Value::String(outcome.to_string());
    }
    if let Some(obj) = error.as_object_mut() {
        obj.insert("compensation".to_string(), compensation);
    }
    error
}

fn should_enter_compensation_for_error(error: &Value) -> bool {
    !matches!(
        error.get("type").and_then(Value::as_str),
        Some("NondeterministicError" | "StalledError")
    )
}

async fn upsert_workflow_subscription<C>(
    conn: &C,
    app_id: &Uuid,
    run_id: &str,
    checkpoint: &StepCheckpoint,
) -> Result<(), RegistryError>
where
    C: GenericClient + Sync,
{
    let Some(topic) = checkpoint.topic.as_ref().filter(|topic| !topic.is_empty()) else {
        return Ok(());
    };
    let id = typed_id::new_workflow_subscription_id();
    conn.execute(
        "INSERT INTO zeroship.workflow_subscriptions \
            (id, app_id, topic, run_id, signal_name, type_filter, ordinal, max_age_ms, expires_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
         ON CONFLICT (run_id, ordinal) DO UPDATE SET \
            app_id = EXCLUDED.app_id, \
            topic = EXCLUDED.topic, \
            signal_name = EXCLUDED.signal_name, \
            type_filter = EXCLUDED.type_filter, \
            max_age_ms = EXCLUDED.max_age_ms, \
            expires_at = EXCLUDED.expires_at",
        &[
            &id,
            app_id,
            topic,
            &run_id,
            &checkpoint.name,
            &checkpoint.signal_type,
            &checkpoint.ordinal,
            &checkpoint.max_signal_age_ms,
            &checkpoint.wake_at,
        ],
    )
    .await
    .map_err(RegistryError::from)?;
    Ok(())
}

fn child_limit_error(message: impl Into<String>) -> Value {
    serde_json::json!({
        "type": "LimitExceededError",
        "message": message.into(),
        "retryable": false,
    })
}

fn child_cancelled_error() -> Value {
    serde_json::json!({
        "type": "ChildCancelledError",
        "message": "child workflow was cancelled",
        "retryable": false,
    })
}

async fn cancel_requested_run<C>(conn: &C, run_id: &str) -> Result<bool, RegistryError>
where
    C: GenericClient + Sync,
{
    let changed = conn
        .execute(
            "UPDATE zeroship.workflow_runs \
                SET state = 'cancelled', \
                    cancel_requested = false, \
                    wake_at = NULL, \
                    waiting_step_key = NULL, \
                    claimed_by = NULL, \
                    lease_expires = NULL, \
                    dispatch_nonce = NULL, \
                    claim_epoch = claim_epoch + 1 \
              WHERE id = $1 \
                AND cancel_requested \
                AND state NOT IN ('completed','failed','cancelled','stalled')",
            &[&run_id],
        )
        .await
        .map_err(RegistryError::from)?;
    if changed == 0 {
        return Ok(false);
    }

    emit_child_terminal_hook(
        conn,
        run_id,
        ChildTerminalPayload {
            state: "cancelled",
            output: None,
            output_ref: None,
            error: Some(child_cancelled_error()),
        },
    )
    .await?;
    cascade_cancel_children(conn, run_id).await?;
    Ok(true)
}

fn fail_child_checkpoint_with_limit(checkpoint: &mut StepCheckpoint, message: String) {
    checkpoint.state = "failed".to_string();
    checkpoint.output = None;
    checkpoint.output_ref = None;
    checkpoint.error = Some(child_limit_error(message));
    checkpoint.wake_at = None;
}

async fn prepare_child_spawn<C>(
    conn: &C,
    config: &WorkflowEngineConfig,
    parent_run_id: &str,
    app_id: &Uuid,
    deploy_id: &str,
    parent_tree_depth: i16,
    checkpoint: &mut StepCheckpoint,
) -> Result<Result<(), String>, RegistryError>
where
    C: GenericClient + Sync,
{
    let existing_step = conn
        .query(
            "SELECT child_run_id, signal_type \
               FROM zeroship.workflow_steps \
              WHERE run_id = $1 \
                AND ordinal = $2 \
                AND kind = 'child' \
                AND state = 'running' \
              FOR UPDATE",
            &[&parent_run_id, &checkpoint.ordinal],
        )
        .await
        .map_err(RegistryError::from)?;
    if let Some(row) = existing_step.first() {
        checkpoint.child_run_id = row.get("child_run_id");
        checkpoint.signal_type = row
            .get::<_, Option<String>>("signal_type")
            .or_else(|| Some(child_signal_type(checkpoint.ordinal)));
        return Ok(Ok(()));
    }

    let Some(child_workflow_name) = checkpoint.child_workflow_name.clone() else {
        return Ok(Err("child workflow name is missing".to_string()));
    };
    if child_workflow_name.is_empty() || child_workflow_name.len() > 128 {
        return Ok(Err("child workflow name must be 1-128 bytes".to_string()));
    }
    if child_workflow_name.starts_with("__zs.") {
        return Ok(Err("child workflow name uses a reserved prefix".to_string()));
    }

    let child_depth = parent_tree_depth.saturating_add(1);
    if child_depth > config.max_child_depth.max(0) {
        return Ok(Err(format!(
            "child workflow depth exceeds maxChildDepth ({} > {})",
            child_depth,
            config.max_child_depth.max(0)
        )));
    }

    let live_descendants = live_descendant_count(conn, parent_run_id).await?;
    if live_descendants >= config.max_live_descendants.max(0) {
        return Ok(Err(format!(
            "child workflow tree exceeds maxLiveDescendants ({} >= {})",
            live_descendants,
            config.max_live_descendants.max(0)
        )));
    }

    let child_input = checkpoint.child_input.clone().unwrap_or(Value::Null);
    let input_journal_bytes = workflow_limits::json_column_size(conn, &child_input).await?;
    let child_key = child_dedup_key(parent_run_id, checkpoint.ordinal);
    let child_run_id = typed_id::new_workflow_run_id();
    let parent_wait_step_key = child_signal_type(checkpoint.ordinal);
    let cascade = checkpoint
        .child_options
        .as_ref()
        .is_some_and(|options| options.cascade);
    let rows = conn
        .query(
            "INSERT INTO zeroship.workflow_runs \
                (id, workflow_name, app_id, deploy_id, state, input, journal_bytes, dedup_key, \
                 wake_at, parent_run_id, parent_wait_step_key, parent_cascade, tree_depth, started_at) \
             VALUES ($1, $2, $3, $4, 'queued', $5, $6, $7, now(), $8, $9, $10, $11, now()) \
             ON CONFLICT (app_id, workflow_name, dedup_key) DO NOTHING \
             RETURNING id",
            &[
                &child_run_id,
                &child_workflow_name,
                app_id,
                &deploy_id,
                &child_input,
                &input_journal_bytes,
                &child_key,
                &parent_run_id,
                &parent_wait_step_key,
                &cascade,
                &child_depth,
            ],
        )
        .await
        .map_err(RegistryError::from)?;
    let actual_child_id = if let Some(row) = rows.first() {
        row.get("id")
    } else {
        conn.query_one(
            "SELECT id \
               FROM zeroship.workflow_runs \
              WHERE app_id = $1 AND workflow_name = $2 AND dedup_key = $3 \
              LIMIT 1",
            &[app_id, &child_workflow_name, &child_key],
        )
        .await
        .map_err(RegistryError::from)?
        .get("id")
    };

    checkpoint.child_run_id = Some(actual_child_id);
    checkpoint.signal_type = Some(parent_wait_step_key);
    Ok(Ok(()))
}

async fn live_descendant_count<C>(conn: &C, run_id: &str) -> Result<i64, RegistryError>
where
    C: GenericClient + Sync,
{
    let row = conn
        .query_one(
            "WITH RECURSIVE ancestors AS ( \
                 SELECT id, parent_run_id \
                   FROM zeroship.workflow_runs \
                  WHERE id = $1 \
                 UNION ALL \
                 SELECT p.id, p.parent_run_id \
                   FROM zeroship.workflow_runs p \
                   JOIN ancestors a ON a.parent_run_id = p.id \
             ), root AS ( \
                 SELECT id FROM ancestors WHERE parent_run_id IS NULL LIMIT 1 \
             ), tree AS ( \
                 SELECT id FROM root \
                 UNION ALL \
                 SELECT c.id \
                   FROM zeroship.workflow_runs c \
                   JOIN tree t ON c.parent_run_id = t.id \
             ) \
             SELECT COUNT(*)::bigint AS n \
               FROM zeroship.workflow_runs r \
               JOIN tree t ON t.id = r.id \
              WHERE r.id <> (SELECT id FROM root) \
                AND r.state NOT IN ('completed','failed','cancelled','stalled')",
            &[&run_id],
        )
        .await
        .map_err(RegistryError::from)?;
    Ok(row.get("n"))
}

#[derive(Debug, Clone)]
struct ChildTerminalPayload<'a> {
    state: &'a str,
    output: Option<Value>,
    output_ref: Option<WorkflowOutputRef>,
    error: Option<Value>,
}

async fn emit_child_terminal_hook<C>(
    conn: &C,
    child_run_id: &str,
    terminal: ChildTerminalPayload<'_>,
) -> Result<(), RegistryError>
where
    C: GenericClient + Sync,
{
    let rows = conn
        .query(
            "SELECT parent_run_id, parent_wait_step_key \
               FROM zeroship.workflow_runs \
              WHERE id = $1 \
                AND parent_run_id IS NOT NULL \
                AND parent_wait_step_key IS NOT NULL",
            &[&child_run_id],
        )
        .await
        .map_err(RegistryError::from)?;
    let Some(row) = rows.first() else {
        return Ok(());
    };
    let parent_run_id: String = row.get("parent_run_id");
    let parent_wait_step_key: String = row.get("parent_wait_step_key");
    let ok = terminal.state == "completed";
    let error = if ok {
        None
    } else if terminal.state == "cancelled" {
        Some(child_cancelled_error())
    } else {
        terminal.error
    };
    let output_ref = terminal.output_ref.map(|value| {
        serde_json::json!({
            "hash": value.hash,
            "size": value.size,
            "contentType": value.content_type,
        })
    });
    let payload = serde_json::json!({
        "ok": ok,
        "output": if ok { terminal.output } else { None },
        "outputRef": output_ref,
        "error": error,
        "state": terminal.state,
        "childRunId": child_run_id,
    });
    let signal_id = typed_id::new_workflow_signal_id();
    conn.execute(
        "INSERT INTO zeroship.workflow_signals \
            (id, run_id, type, payload, origin, delivery, idempotency_key, created_at) \
         VALUES ($1, $2, $3, $4, 'system', 'direct', $3, now()) \
         ON CONFLICT (run_id, type, idempotency_key) \
         WHERE idempotency_key IS NOT NULL AND delivery <> 'topic' \
         DO NOTHING",
        &[
            &signal_id,
            &parent_run_id,
            &parent_wait_step_key,
            &payload,
        ],
    )
    .await
    .map_err(RegistryError::from)?;
    conn.execute(
        "UPDATE zeroship.workflow_runs \
            SET wake_at = now() \
          WHERE id = $1 \
            AND state IN ('running','sleeping','waiting')",
        &[&parent_run_id],
    )
    .await
    .map_err(RegistryError::from)?;
    Ok(())
}

pub(crate) async fn cascade_cancel_children<C>(
    conn: &C,
    parent_run_id: &str,
) -> Result<u64, RegistryError>
where
    C: GenericClient + Sync,
{
    conn.execute(
        "UPDATE zeroship.workflow_runs \
            SET cancel_requested = true, wake_at = now() \
          WHERE parent_run_id = $1 \
            AND parent_cascade \
            AND state NOT IN ('completed','failed','cancelled','stalled')",
        &[&parent_run_id],
    )
    .await
    .map_err(RegistryError::from)
}

async fn delete_workflow_subscription<C>(
    conn: &C,
    run_id: &str,
    ordinal: i32,
) -> Result<(), RegistryError>
where
    C: GenericClient + Sync,
{
    conn.execute(
        "DELETE FROM zeroship.workflow_subscriptions \
          WHERE run_id = $1 AND ordinal = $2",
        &[&run_id, &ordinal],
    )
    .await
    .map_err(RegistryError::from)?;
    Ok(())
}

async fn insert_resolved_step<C>(
    conn: &C,
    checkpoint: &StepCheckpoint,
    run_id: &str,
    batch_id: &str,
    batch_width: i16,
) -> Result<StepWriteOutcome, RegistryError>
where
    C: GenericClient + Sync,
{
    let existing = conn
        .query(
            "SELECT state, name, kind \
               FROM zeroship.workflow_steps \
              WHERE run_id = $1 AND ordinal = $2 \
              FOR UPDATE",
            &[&run_id, &checkpoint.ordinal],
        )
        .await
        .map_err(RegistryError::from)?;
    let resolves_running = if let Some(row) = existing.first() {
        let state: String = row.get("state");
        let name: String = row.get("name");
        let kind: String = row.get("kind");
        let resolves_running = state == "running"
            && matches!(checkpoint.state.as_str(), "completed" | "failed")
            && name == checkpoint.name
            && kind == checkpoint.kind;
        if !resolves_running {
            return Ok(StepWriteOutcome::Noop);
        }
        true
    } else {
        false
    };

    let output_ref = checkpoint.output_ref.as_ref();
    let output_kind = if output_ref.is_some() { "blob" } else { "inline" };
    let output_value = if output_ref.is_some() {
        None
    } else {
        checkpoint.output.clone()
    };
    let output_hash = output_ref.map(|value| value.hash.clone());
    let output_size = output_ref.map(|value| value.size);
    let output_content_type = output_ref
        .and_then(|value| value.content_type.clone())
        .or_else(|| output_ref.map(|_| "application/json".to_string()));
    let blob_bytes_delta = output_ref.map_or(0, |value| value.size.max(0));
    let compensation_state = if checkpoint.kind == "run" && checkpoint.state == "completed" {
        checkpoint.compensation_state.as_deref()
    } else {
        None
    };
    let compensation_max_attempts = checkpoint.compensation_max_attempts.max(1);

    let delta = checkpoint_journal_bytes(conn, checkpoint).await?;
    if delta > 0 {
        let rows = conn
            .query(
                "SELECT app_id, journal_bytes \
                   FROM zeroship.workflow_runs \
                  WHERE id = $1 \
                  FOR UPDATE",
                &[&run_id],
            )
            .await
            .map_err(RegistryError::from)?;
        let Some(row) = rows.first() else {
            return Err(RegistryError::NotFound(format!(
                "workflow run {run_id} not found for journal accounting"
            )));
        };
        let app_id: Uuid = row.get("app_id");
        let current: i64 = row.get("journal_bytes");
        let limits = workflow_limits::limits_for_app(conn, &app_id).await?;
        if workflow_limits::cap_exceeded(current, delta, limits.run_max_bytes) {
            mark_run_state_cap_exceeded(conn, run_id, current, delta, limits.run_max_bytes)
                .await?;
            return Ok(StepWriteOutcome::CapExceeded);
        }
    }

    let changed = if resolves_running {
        conn.execute(
            "UPDATE zeroship.workflow_steps \
                SET state = $4, \
                    output = $5, \
                    error = $6, \
                    wake_at = $7, \
                    signal_type = $8, \
                    max_signal_age_ms = $9, \
                    consumed_signal_id = $10, \
                    output_kind = $12, \
                    output_hash = $13, \
                    output_size = $14, \
                    output_content_type = $15, \
                    child_run_id = COALESCE(child_run_id, $16), \
                    compensation_state = $17, \
                    compensation_max_attempts = $18, \
                    finished_at = now() \
              WHERE run_id = $1 \
                AND ordinal = $2 \
                AND name = $3 \
                AND kind = $11 \
                AND state = 'running'",
            &[
                &run_id,
                &checkpoint.ordinal,
                &checkpoint.name,
                &checkpoint.state,
                &output_value,
                &checkpoint.error,
                &checkpoint.wake_at,
                &checkpoint.signal_type,
                &checkpoint.max_signal_age_ms,
                &checkpoint.consumed_signal_id,
                &checkpoint.kind,
                &output_kind,
                &output_hash,
                &output_size,
                &output_content_type,
                &checkpoint.child_run_id,
                &compensation_state,
                &compensation_max_attempts,
            ],
        )
        .await
        .map_err(RegistryError::from)?
    } else {
        conn.execute(
            "INSERT INTO zeroship.workflow_steps \
            (run_id, ordinal, name, name_occurrence, kind, state, output, error, \
             output_kind, output_hash, output_size, output_content_type, \
             wake_at, signal_type, max_signal_age_ms, consumed_signal_id, \
             child_run_id, batch_id, batch_width, compensation_state, compensation_max_attempts, finished_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, \
                 $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21, now()) \
         ON CONFLICT (run_id, ordinal) DO NOTHING",
            &[
                &run_id,
                &checkpoint.ordinal,
                &checkpoint.name,
                &checkpoint.name_occurrence,
                &checkpoint.kind,
                &checkpoint.state,
                &output_value,
                &checkpoint.error,
                &output_kind,
                &output_hash,
                &output_size,
                &output_content_type,
                &checkpoint.wake_at,
                &checkpoint.signal_type,
                &checkpoint.max_signal_age_ms,
                &checkpoint.consumed_signal_id,
                &checkpoint.child_run_id,
                &batch_id,
                &batch_width,
                &compensation_state,
                &compensation_max_attempts,
            ],
        )
        .await
        .map_err(RegistryError::from)?
    };
    if changed > 0 {
        if let Some(output_ref) = output_ref {
            upsert_workflow_blob_ref(conn, output_ref).await?;
        }
    }
    if changed > 0 && (delta > 0 || blob_bytes_delta > 0) {
        conn.execute(
            "UPDATE zeroship.workflow_runs \
                SET journal_bytes = journal_bytes + $2, \
                    blob_bytes = blob_bytes + $3 \
              WHERE id = $1",
            &[&run_id, &delta, &blob_bytes_delta],
        )
        .await
        .map_err(RegistryError::from)?;
    }

    Ok(if changed > 0 {
        StepWriteOutcome::Wrote
    } else {
        StepWriteOutcome::Noop
    })
}

async fn checkpoint_journal_bytes<C>(
    conn: &C,
    checkpoint: &StepCheckpoint,
) -> Result<i64, RegistryError>
where
    C: GenericClient + Sync,
{
    if checkpoint.output_ref.is_some() {
        let rows = conn
            .query(
                "SELECT (COALESCE(pg_column_size($1::jsonb), 0))::bigint AS bytes",
                &[&checkpoint.error],
            )
            .await
            .map_err(RegistryError::from)?;
        let error_bytes: i64 = rows[0].get("bytes");
        return Ok(error_bytes + BLOB_REF_JOURNAL_BYTES);
    }
    let rows = conn
        .query(
            "SELECT (COALESCE(pg_column_size($1::jsonb), 0) \
                    + COALESCE(pg_column_size($2::jsonb), 0))::bigint AS bytes",
            &[&checkpoint.output, &checkpoint.error],
        )
        .await
        .map_err(RegistryError::from)?;
    Ok(rows[0].get("bytes"))
}

async fn run_output_journal_bytes<C>(
    conn: &C,
    output: &Option<Value>,
    output_ref: Option<&WorkflowOutputRef>,
) -> Result<i64, RegistryError>
where
    C: GenericClient + Sync,
{
    if output_ref.is_some() {
        return Ok(BLOB_REF_JOURNAL_BYTES);
    }
    let rows = conn
        .query(
            "SELECT (COALESCE(pg_column_size($1::jsonb), 0))::bigint AS bytes",
            &[output],
        )
        .await
        .map_err(RegistryError::from)?;
    Ok(rows[0].get("bytes"))
}

async fn upsert_workflow_blob_ref<C>(
    conn: &C,
    output_ref: &WorkflowOutputRef,
) -> Result<(), RegistryError>
where
    C: GenericClient + Sync,
{
    let content_type = output_ref
        .content_type
        .as_deref()
        .unwrap_or("application/json");
    conn.execute(
        "INSERT INTO zeroship.workflow_blobs \
            (hash, size, content_type, refcount, last_referenced_at) \
         VALUES ($1, $2, $3, 1, now()) \
         ON CONFLICT (hash) DO UPDATE SET \
            size = EXCLUDED.size, \
            content_type = EXCLUDED.content_type, \
            refcount = zeroship.workflow_blobs.refcount + 1, \
            last_referenced_at = now()",
        &[&output_ref.hash, &output_ref.size, &content_type],
    )
    .await
    .map_err(RegistryError::from)?;
    Ok(())
}

async fn mark_run_state_cap_exceeded<C>(
    conn: &C,
    run_id: &str,
    current: i64,
    delta: i64,
    cap: i64,
) -> Result<(), RegistryError>
where
    C: GenericClient + Sync,
{
    let error = workflow_limits::state_cap_error(current, delta, cap);
    conn.execute(
        "UPDATE zeroship.workflow_runs \
            SET state = 'failed', \
                output = NULL, \
                error = $2, \
                output_kind = 'inline', \
                output_hash = NULL, \
                output_size = NULL, \
                output_content_type = NULL, \
                wake_at = NULL, \
                waiting_step_key = NULL, \
                paused_from_status = NULL, \
                claimed_by = NULL, \
                lease_expires = NULL, \
                dispatch_nonce = NULL \
          WHERE id = $1",
        &[&run_id, &error],
    )
    .await
    .map_err(RegistryError::from)?;
    Ok(())
}

async fn requeue_claim(
    registry: &Registry,
    owner_id: &str,
    run_id: &str,
    dispatch_nonce: &str,
) -> Result<(), RegistryError> {
    let conn = registry.conn().await?;
    conn.execute(
        "UPDATE zeroship.workflow_runs \
            SET state = CASE \
                    WHEN state = 'paused' THEN 'paused' \
                    WHEN state = 'compensating' THEN 'compensating' \
                    ELSE 'queued' \
                END, \
                wake_at = CASE WHEN state = 'paused' THEN wake_at ELSE now() END, \
                claimed_by = NULL, \
                lease_expires = NULL, \
                dispatch_nonce = NULL \
          WHERE id = $1 \
            AND claimed_by = $2 \
            AND dispatch_nonce = $3 \
            AND state IN ('running','paused','compensating')",
        &[&run_id, &owner_id, &dispatch_nonce],
    )
    .await
    .map_err(RegistryError::from)?;
    Ok(())
}

async fn park_backpressure_claim(
    registry: &Registry,
    owner_id: &str,
    run_id: &str,
    dispatch_nonce: &str,
) -> Result<(), RegistryError> {
    let wake_at = Utc::now() + chrono::Duration::milliseconds(BACKPRESSURE_PARK_MS);
    let conn = registry.conn().await?;
    conn.execute(
        "UPDATE zeroship.workflow_runs \
            SET state = CASE \
                    WHEN state = 'paused' THEN 'paused' \
                    WHEN state = 'compensating' THEN 'compensating' \
                    ELSE 'queued' \
                END, \
                wake_at = CASE WHEN state = 'paused' THEN wake_at ELSE $1 END, \
                claimed_by = NULL, \
                lease_expires = NULL, \
                dispatch_nonce = NULL \
          WHERE id = $2 \
            AND claimed_by = $3 \
            AND dispatch_nonce = $4 \
            AND state IN ('running','paused','compensating')",
        &[&wake_at, &run_id, &owner_id, &dispatch_nonce],
    )
    .await
    .map_err(RegistryError::from)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ntex::web::{self, test};

    #[test]
    fn default_tick_is_one_second() {
        assert_eq!(DEFAULT_TICK_SECS, 1);
    }

    #[test]
    fn waiting_step_key_parses_sleep_and_wait() {
        assert_eq!(
            parse_waiting_step_key("sleep:7:cooldown").expect("sleep key"),
            WaitingStep::Sleep {
                ordinal: 7,
                name: "cooldown".to_string()
            }
        );
        assert_eq!(
            parse_waiting_step_key("wait:8:approved:approved:60000").expect("wait key"),
            WaitingStep::WaitSignal {
                ordinal: 8,
                name: "approved".to_string(),
                signal_type: "approved".to_string(),
                max_signal_age_ms: Some(60_000)
            }
        );
    }

    #[test]
    fn step_result_deserializes_sleeping_wake_at_contract() {
        let result: StepResult = serde_json::from_value(serde_json::json!({
            "runId": "run_test",
            "dispatchNonce": "wfd_test",
            "checkpoints": [{
                "ordinal": 1,
                "name": "sleep",
                "nameOccurrence": 0,
                "kind": "sleep",
                "state": "running",
                "output": null,
                "error": null,
                "wakeAt": "2026-07-06T10:24:10Z",
                "signalType": null,
                "maxSignalAgeMs": null,
                "consumedSignalId": null
            }],
            "runUpdate": {
                "state": "sleeping",
                "wakeAt": "2026-07-06T10:24:10Z"
            }
        }))
        .expect("StepResult JSON");
        assert_eq!(
            result.run_update.wake_at().expect("run wake_at").to_rfc3339(),
            "2026-07-06T10:24:10+00:00"
        );
        assert_eq!(
            result.checkpoints[0]
                .wake_at
                .expect("checkpoint wake_at")
                .to_rfc3339(),
            "2026-07-06T10:24:10+00:00"
        );
    }

    fn test_step_request() -> StepRequest {
        StepRequest {
            run_id: "run_test".to_string(),
            app_id: Uuid::new_v4(),
            workflow_name: "Checkout".to_string(),
            deploy_id: "dep_test".to_string(),
            deploy_hash: "hash_test".to_string(),
            dispatch_nonce: "wfd_test".to_string(),
            phase: "running".to_string(),
            input: Some(serde_json::json!({"orderId": "ord_1"})),
            started_at: Utc::now(),
            journal: Vec::new(),
        }
    }

    async fn capture_step_request(
        seen: web::types::State<Arc<std::sync::Mutex<Vec<Value>>>>,
        body: ntex::util::Bytes,
    ) -> web::HttpResponse {
        let request: Value = serde_json::from_slice(body.as_ref()).expect("StepRequest json");
        seen.lock().expect("seen lock").push(request.clone());
        web::HttpResponse::Ok().json(&serde_json::json!({
            "runId": request["runId"],
            "dispatchNonce": request["dispatchNonce"],
            "checkpoints": [{
                "ordinal": 0,
                "name": "done",
                "nameOccurrence": 0,
                "kind": "run",
                "state": "completed",
                "output": {"ok": true},
                "error": null,
                "wakeAt": null,
                "signalType": null,
                "maxSignalAgeMs": null,
                "consumedSignalId": null
            }],
            "runUpdate": {
                "state": "completed",
                "output": {"ok": true}
            }
        }))
    }

    #[ntex::test]
    async fn gateway_step_dispatcher_posts_and_parses_step_result() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
        let server_seen = Arc::clone(&seen);
        let gateway = test::server(move || {
            let seen = Arc::clone(&server_seen);
            async move {
                web::App::new().state(seen).service(
                    web::resource(GATEWAY_WORKFLOW_DISPATCH_PATH)
                        .route(web::post().to(capture_step_request)),
                )
            }
        })
        .await;

        let request = test_step_request();
        let dispatcher = GatewayStepDispatcher::new(gateway.url(""));
        let outcome = dispatcher.dispatch(request.clone()).await;
        let DispatchOutcome::Completed(result) = outcome else {
            panic!("expected completed dispatch outcome");
        };
        assert_eq!(result.run_id, request.run_id);
        assert_eq!(result.dispatch_nonce, request.dispatch_nonce);
        assert_eq!(result.checkpoints.len(), 1);
        assert_eq!(result.checkpoints[0].name, "done");

        let seen = seen.lock().expect("seen lock");
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0]["runId"], request.run_id);
        assert_eq!(seen[0]["appId"], request.app_id.to_string());
        assert_eq!(seen[0]["deployHash"], request.deploy_hash);
    }

    #[ntex::test]
    async fn gateway_step_dispatcher_maps_402_to_backpressure() {
        let gateway = test::server(|| async {
            web::App::new().service(
                web::resource(GATEWAY_WORKFLOW_DISPATCH_PATH)
                    .route(web::post().to(|| async {
                        web::HttpResponse::PaymentRequired()
                            .json(&serde_json::json!({"code": "SPEND_LIMIT"}))
                    })),
            )
        })
        .await;

        let request = test_step_request();
        let dispatcher = GatewayStepDispatcher::new(gateway.url(""));
        let outcome = dispatcher.dispatch(request.clone()).await;
        let DispatchOutcome::Backpressure {
            run_id,
            dispatch_nonce,
            reason,
        } = outcome
        else {
            panic!("expected backpressure dispatch outcome");
        };
        assert_eq!(run_id, request.run_id);
        assert_eq!(dispatch_nonce, request.dispatch_nonce);
        assert!(reason.contains("HTTP 402"));
    }
}
