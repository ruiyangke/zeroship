use crate::{operations::RunState, WorkflowInvocation, WorkflowServiceError};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use zeroship_core::app_id::AppId;

pub use zeroship_core::workflow_coordination::RequestId;

#[derive(Clone, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct TaskToken(String);
impl std::fmt::Debug for TaskToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TaskToken([redacted])")
    }
}
impl TaskToken {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
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
impl TryFrom<String> for TaskToken {
    type Error = WorkflowServiceError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.len() != 64
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(WorkflowServiceError::Unauthenticated);
        }
        Ok(Self(value))
    }
}
impl From<TaskToken> for String {
    fn from(value: TaskToken) -> Self {
        value.0
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
    /// Granted lease duration; runners subtract local transport elapsed time.
    pub lease_ms: i64,
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
    pub lease_ms: i64,
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

pub fn hash(bytes: &[u8]) -> String {
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

/// Identity of a journal row whose app-scoped domain key is separate.
pub(crate) fn storage_id() -> String {
    zeroship_core::typed_id::generate("wjr")
}
