use crate::{operations::RunState, WorkflowInvocation, WorkflowServiceError};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use zeroship_core::{app_id::AppId, typed_id};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RequestId(String);
impl RequestId {
    #[must_use]
    pub fn mint() -> Self {
        Self(typed_id::generate(typed_id::WORKFLOW_REQUEST_PREFIX))
    }
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl TryFrom<String> for RequestId {
    type Error = WorkflowServiceError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        typed_id::parse_with_prefix(&value, typed_id::WORKFLOW_REQUEST_PREFIX).map_err(|_| {
            WorkflowServiceError::InvalidRequest("invalid workflow request identity".into())
        })?;
        Ok(Self(value))
    }
}
impl From<RequestId> for String {
    fn from(value: RequestId) -> Self {
        value.0
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TaskToken(String);
impl std::fmt::Debug for TaskToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TaskToken([redacted])")
    }
}
impl TaskToken {
    pub(crate) fn mint() -> Self {
        Self(format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        ))
    }
    pub(crate) fn hash(&self) -> String {
        hash(self.0.as_bytes())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct WorkerIdentity(String);
impl WorkerIdentity {
    pub fn new(value: String) -> Result<Self, WorkflowServiceError> {
        value.try_into()
    }
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl TryFrom<String> for WorkerIdentity {
    type Error = WorkflowServiceError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
            return Err(WorkflowServiceError::InvalidRequest(
                "invalid worker identity".into(),
            ));
        }
        Ok(Self(value))
    }
}
impl From<WorkerIdentity> for String {
    fn from(value: WorkerIdentity) -> Self {
        value.0
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AppPolicy {
    pub admission: bool,
    pub max_live_runs: i64,
    pub max_child_depth: i64,
    pub max_running: i64,
    pub max_input_bytes: usize,
    pub max_frontier: usize,
    pub max_journal_bytes: usize,
    pub max_compensation_attempts: i32,
    pub compensation_retry_ms: i64,
    pub max_schedules: usize,
    pub max_schedule_backfill: usize,
    pub min_schedule_interval_ms: i64,
    pub lease_ms: i64,
    pub request_retention_ms: i64,
}
impl Default for AppPolicy {
    fn default() -> Self {
        Self {
            admission: true,
            max_live_runs: 10_000,
            max_child_depth: 16,
            max_running: 16,
            max_input_bytes: 1024 * 1024,
            max_frontier: 256,
            max_journal_bytes: 16 * 1024 * 1024,
            max_compensation_attempts: 8,
            compensation_retry_ms: 1_000,
            max_schedules: 64,
            max_schedule_backfill: 32,
            min_schedule_interval_ms: 1_000,
            lease_ms: 60_000,
            request_retention_ms: 86_400_000,
        }
    }
}
impl AppPolicy {
    pub(crate) fn validate(&self) -> Result<(), WorkflowServiceError> {
        if self.max_live_runs < 0
            || self.max_child_depth < 0
            || self.max_running < 0
            || self.max_input_bytes == 0
            || self.max_frontier == 0
            || self.max_journal_bytes == 0
            || self.max_compensation_attempts <= 0
            || self.compensation_retry_ms <= 0
            || self.max_schedule_backfill == 0
            || self.min_schedule_interval_ms <= 0
            || self.lease_ms <= 0
            || self.request_retention_ms <= 0
        {
            return Err(WorkflowServiceError::InvalidRequest(
                "invalid workflow admission policy".into(),
            ));
        }
        Ok(())
    }
    pub(crate) fn admit(&self) -> Result<(), WorkflowServiceError> {
        if !self.admission {
            return Err(WorkflowServiceError::PermissionDenied);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeployRegistration {
    pub id: String,
    pub hash: String,
    pub workflows: BTreeSet<String>,
    #[serde(default)]
    pub schedules: Vec<super::ScheduleRegistration>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskAssignment {
    pub id: String,
    pub token: TaskToken,
    pub generation: i64,
    pub epoch: i64,
    pub deadline: i64,
    pub invocation: WorkflowInvocation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ControlIntent {
    None,
    Pause,
    Cancel,
}
impl ControlIntent {
    pub(crate) fn parse(value: &str) -> Result<Self, WorkflowServiceError> {
        match value {
            "none" => Ok(Self::None),
            "pause" => Ok(Self::Pause),
            "cancel" => Ok(Self::Cancel),
            _ => Err(WorkflowServiceError::Internal(
                "invalid workflow control intent".into(),
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Heartbeat {
    pub deadline: i64,
    pub control: ControlIntent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompletionReceipt {
    pub task_id: String,
    pub app_id: AppId,
    pub run_id: String,
    pub generation: i64,
    pub state: RunState,
    pub committed_at: i64,
}

pub(crate) fn hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(crate) fn digest<T: Serialize>(value: &T) -> Result<String, WorkflowServiceError> {
    fn canonical(value: serde_json::Value) -> serde_json::Value {
        match value {
            serde_json::Value::Object(value) => {
                let sorted: std::collections::BTreeMap<_, _> = value.into_iter().collect();
                serde_json::Value::Object(
                    sorted
                        .into_iter()
                        .map(|(key, value)| (key, canonical(value)))
                        .collect(),
                )
            }
            serde_json::Value::Array(value) => {
                serde_json::Value::Array(value.into_iter().map(canonical).collect())
            }
            other => other,
        }
    }
    let value = serde_json::to_value(value)
        .map_err(|_| WorkflowServiceError::Internal("encode workflow digest".into()))?;
    let bytes = serde_json::to_vec(&canonical(value))
        .map_err(|_| WorkflowServiceError::Internal("encode workflow digest".into()))?;
    Ok(hash(&bytes))
}
