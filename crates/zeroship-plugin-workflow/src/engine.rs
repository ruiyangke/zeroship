//! Pure durable-workflow fold and DTO contracts.

use std::sync::OnceLock;

use chrono::{DateTime, Utc};
use serde::{de, Deserialize, Deserializer, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;
use zeroship_core::app_id::AppId;
use zeroship_core::typed_id;

pub const DEFAULT_TICK_SECS: u64 = 1;
pub const DEFAULT_MAX_CHILD_DEPTH: i16 = 16;
pub const DEFAULT_MAX_LIVE_DESCENDANTS: i64 = 1_024;
pub const DEFAULT_MAX_START_MANY_BATCH: usize = 1_000;
pub const FREE_WORKFLOW_JOURNAL_MAX_BYTES: i64 = 100 * 1024 * 1024;
pub const PAID_WORKFLOW_JOURNAL_MAX_BYTES: i64 = 1024 * 1024 * 1024;
pub const RUN_JOURNAL_LIMIT_FIELD: &str = "workflow_journal_max_bytes";
pub const APP_JOURNAL_LIMIT_FIELD: &str = "workflow_app_journal_max_bytes";
pub const MAX_CHILD_DEPTH_FIELD: &str = "workflow_max_child_depth";
pub const MAX_LIVE_DESCENDANTS_FIELD: &str = "workflow_max_live_descendants";
pub const MAX_START_MANY_BATCH_FIELD: &str = "workflow_max_start_many_batch";
pub const STUCK_STRIKE_LIMIT_FIELD: &str = "workflow_stuck_strike_limit";

static OWNER_ID: OnceLock<String> = OnceLock::new();

fn default_owner_id() -> String {
    OWNER_ID
        .get_or_init(|| format!("control-wf-{}", std::process::id()))
        .clone()
}

fn default_stuck_strike_limit() -> i16 {
    WorkflowEngineConfig::default().stuck_strike_limit
}

fn default_max_child_depth() -> i16 {
    DEFAULT_MAX_CHILD_DEPTH
}

fn default_max_live_descendants() -> i64 {
    DEFAULT_MAX_LIVE_DESCENDANTS
}

fn default_max_start_many_batch() -> usize {
    DEFAULT_MAX_START_MANY_BATCH
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowJournalLimits {
    pub run_max_bytes: i64,
    pub app_max_bytes: i64,
}

impl Default for WorkflowJournalLimits {
    fn default() -> Self {
        Self {
            run_max_bytes: PAID_WORKFLOW_JOURNAL_MAX_BYTES,
            app_max_bytes: PAID_WORKFLOW_JOURNAL_MAX_BYTES,
        }
    }
}

pub fn workflow_journal_limits_from_plan(
    plan_id: &str,
    plan_name: Option<&str>,
    runtime_limits: Option<&Value>,
) -> WorkflowJournalLimits {
    let default = default_journal_cap(plan_id, plan_name);
    let run_max_bytes = runtime_limits
        .and_then(|json| positive_i64_field(json, RUN_JOURNAL_LIMIT_FIELD))
        .unwrap_or(default);
    let app_max_bytes = runtime_limits
        .and_then(|json| positive_i64_field(json, APP_JOURNAL_LIMIT_FIELD))
        .unwrap_or(default);

    WorkflowJournalLimits {
        run_max_bytes,
        app_max_bytes,
    }
}

pub fn workflow_engine_limits_from_plan(
    defaults: &WorkflowEngineConfig,
    plan_id: &str,
    plan_name: Option<&str>,
    runtime_limits: Option<&Value>,
) -> WorkflowEngineConfig {
    let mut config = defaults.clone();
    config.journal_limits = workflow_journal_limits_from_plan(plan_id, plan_name, runtime_limits);
    if let Some(limit) = runtime_limits
        .and_then(|json| nonnegative_i64_field(json, MAX_CHILD_DEPTH_FIELD))
        .and_then(|limit| i16::try_from(limit).ok())
    {
        config.max_child_depth = limit;
    }
    if let Some(limit) =
        runtime_limits.and_then(|json| nonnegative_i64_field(json, MAX_LIVE_DESCENDANTS_FIELD))
    {
        config.max_live_descendants = limit;
    }
    if let Some(limit) = runtime_limits
        .and_then(|json| nonnegative_i64_field(json, MAX_START_MANY_BATCH_FIELD))
        .and_then(|limit| usize::try_from(limit).ok())
    {
        config.max_start_many_batch = limit;
    }
    if let Some(limit) = runtime_limits
        .and_then(|json| positive_i64_field(json, STUCK_STRIKE_LIMIT_FIELD))
        .and_then(|limit| i16::try_from(limit).ok())
    {
        config.stuck_strike_limit = limit;
    }
    config
}

fn default_journal_cap(plan_id: &str, plan_name: Option<&str>) -> i64 {
    if plan_name == Some("free") || plan_id == free_plan_id() {
        FREE_WORKFLOW_JOURNAL_MAX_BYTES
    } else {
        PAID_WORKFLOW_JOURNAL_MAX_BYTES
    }
}

fn positive_i64_field(json: &Value, field: &str) -> Option<i64> {
    let value = json.get(field)?;
    match value {
        Value::Number(n) => n.as_i64().filter(|v| *v > 0),
        Value::String(s) => s.parse::<i64>().ok().filter(|v| *v > 0),
        _ => None,
    }
}

fn nonnegative_i64_field(json: &Value, field: &str) -> Option<i64> {
    let value = json.get(field)?;
    match value {
        Value::Number(n) => n.as_i64().filter(|v| *v >= 0),
        Value::String(s) => s.parse::<i64>().ok().filter(|v| *v >= 0),
        _ => None,
    }
}

fn free_plan_id() -> String {
    let uuid = derive_uuid("zeroship:plan:free:v1", "builtin");
    typed_id::from_uuid_string(typed_id::PLAN_PREFIX, &uuid.to_string())
        .expect("derived uuid is a valid uuid string")
}

fn derive_uuid(label: &str, host: &str) -> Uuid {
    let mut hasher = Sha256::new();
    hasher.update(label.as_bytes());
    hasher.update([0u8]);
    hasher.update(host.as_bytes());
    let digest = hasher.finalize();

    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
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
    /// Journal size caps resolved by the control plane for this dispatch.
    pub journal_limits: WorkflowJournalLimits,
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
            journal_limits: WorkflowJournalLimits::default(),
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
    pub app_id: AppId,
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
    #[serde(default = "default_owner_id")]
    pub owner_id: String,
    #[serde(default = "default_stuck_strike_limit")]
    pub stuck_strike_limit: i16,
    #[serde(default = "default_max_child_depth")]
    pub max_child_depth: i16,
    #[serde(default = "default_max_live_descendants")]
    pub max_live_descendants: i64,
    #[serde(default = "default_max_start_many_batch")]
    pub max_start_many_batch: usize,
    #[serde(default)]
    pub journal_limits: WorkflowJournalLimits,
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

pub fn default_step_kind() -> String {
    "run".to_string()
}

pub fn default_compensation_max_attempts() -> i32 {
    1
}

pub fn default_dispatch_phase() -> String {
    "running".to_string()
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
            Self::ContinuedAsNew { .. } => "completed",
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

pub fn queued_run_update() -> RunUpdate {
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

pub fn stalled_error(strikes: i16, limit: i16) -> Value {
    serde_json::json!({
        "type": "StalledError",
        "message": "workflow made no durable progress before the liveness strike limit",
        "stuck_strikes": strikes,
        "stuck_strike_limit": limit,
    })
}

pub fn outcomes_from_apply_parts(
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
        RunUpdate::ContinuedAsNew {
            seed_input,
            seed_input_ref,
        } => outcomes.push(StepOutcome::ContinueAsNew {
            input: seed_input.clone(),
            input_ref: seed_input_ref.clone(),
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

pub const WORKFLOW_STATE_CAP_ERROR_CODE: &str = "workflow_state_cap_exceeded";

pub fn cap_exceeded(current: i64, delta: i64, cap: i64) -> bool {
    i128::from(current) + i128::from(delta) > i128::from(cap)
}

pub fn state_cap_error(current: i64, delta: i64, cap: i64) -> Value {
    serde_json::json!({
        "type": "LimitExceededError",
        "error_code": WORKFLOW_STATE_CAP_ERROR_CODE,
        "message": "workflow journal state cap exceeded",
        "current_bytes": current,
        "delta_bytes": delta,
        "max_bytes": cap,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn fold_continue_as_new_is_completed_terminal_update() {
        let input = json!({"generation": 1, "carry": "state"});
        let (checkpoints, update) = fold_outcomes(&[StepOutcome::ContinueAsNew {
            input: Some(input.clone()),
            input_ref: None,
        }])
        .expect("continue-as-new should fold");

        assert!(checkpoints.is_empty());
        assert_eq!(update.state(), "completed");
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
    fn continue_as_new_round_trips_from_apply_parts() {
        let seed = json!({"next": true});
        let update = RunUpdate::ContinuedAsNew {
            seed_input: Some(seed.clone()),
            seed_input_ref: None,
        };

        let outcomes = outcomes_from_apply_parts(&[], &update);

        assert_eq!(outcomes.len(), 1);
        match &outcomes[0] {
            StepOutcome::ContinueAsNew { input, input_ref } => {
                assert_eq!(input.as_ref(), Some(&seed));
                assert!(input_ref.is_none());
            }
            other => panic!("unexpected outcome: {other:?}"),
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
