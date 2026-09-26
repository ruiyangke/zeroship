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

/// Which deployment's declarations a journal holder is asking Control for.
///
/// The answer is a [`DeployRegistration`], and both halves of the exchange live
/// here rather than in `zeroship-core` because the response cannot: its
/// `schedules` carry the creator's inline input, which
/// `zeroship_core::workflow_schedules` deliberately excludes - "business input
/// stays in the app bundle". Splitting one contract across two crates would put
/// its halves where neither reader finds both.
///
/// It names no holder, no generation and no hash. The caller's credential says
/// which service is asking, the hold it already took says which generation it
/// holds, and the hash is what the answer has to AGREE with - so accepting one
/// here would let a caller name the value it is about to check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeployRegistrationRequest {
    pub app_id: AppId,
    pub deploy_id: zeroship_core::workflow_jobs::DeploymentId,
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

#[cfg(test)]
mod tests {
    use super::{DeployRegistration, DeployRegistrationRequest};
    use zeroship_core::{app_id::AppId, typed_id, workflow_jobs::DeploymentId};

    /// The registration request is closed and typed on both fields.
    ///
    /// An unknown field is refused rather than ignored, so a caller cannot
    /// smuggle a holder, a generation or a hash past a handler that reads only
    /// the two it knows. And each id is parsed as its own type, so neither can
    /// stand in for the other.
    #[test]
    fn the_registration_request_is_closed_and_typed() {
        let request = DeployRegistrationRequest {
            app_id: AppId::mint(),
            deploy_id: DeploymentId::mint(),
        };
        let encoded = serde_json::to_value(&request).unwrap();
        assert_eq!(
            serde_json::from_value::<DeployRegistrationRequest>(encoded.clone()).unwrap(),
            request
        );
        for field in ["holderId", "generation", "hash", "workflows", "assignmentRevision"] {
            let mut invalid = encoded.clone();
            invalid[field] = serde_json::json!("x");
            assert!(
                serde_json::from_value::<DeployRegistrationRequest>(invalid).is_err(),
                "{field}"
            );
        }
        for (field, value) in [
            ("appId", serde_json::json!(DeploymentId::mint())),
            ("deployId", serde_json::json!(AppId::mint())),
            ("deployId", serde_json::json!("dep_invalid")),
        ] {
            let mut invalid = encoded.clone();
            invalid[field] = value;
            assert!(
                serde_json::from_value::<DeployRegistrationRequest>(invalid).is_err(),
                "{field}"
            );
        }
        for field in ["appId", "deployId"] {
            let mut invalid = encoded.clone();
            invalid.as_object_mut().unwrap().remove(field);
            assert!(
                serde_json::from_value::<DeployRegistrationRequest>(invalid).is_err(),
                "{field}"
            );
        }
        // Rejection control: the snake_case spelling is refused too, so the
        // camelCase rename is what the wire carries.
        assert!(serde_json::from_value::<DeployRegistrationRequest>(serde_json::json!({
            "app_id": AppId::mint(), "deploy_id": DeploymentId::mint()
        }))
        .is_err());
    }

    /// The answer is closed on the same wire, and its schedules keep the
    /// creator's inline input. That input is the whole reason this response is
    /// the engine's type rather than the manager's schedule projection.
    #[test]
    fn the_registration_answer_is_closed_and_keeps_inline_schedule_input() {
        let registration = DeployRegistration {
            id: typed_id::generate("dep"),
            hash: "a".repeat(64),
            workflows: ["Example".into()].into(),
            schedules: vec![super::super::ScheduleRegistration {
                name: "periodic".into(),
                workflow_name: "Example".into(),
                schedule: super::super::ScheduleTiming::Interval {
                    interval_ms: 60_000,
                    anchor: super::super::IntervalAnchor::Epoch,
                },
                input: serde_json::json!({"creator": "input"}),
                overlap: super::super::ScheduleOverlap::Allow,
                catch_up: super::super::ScheduleCatchUp::default(),
            }],
        };
        let encoded = serde_json::to_value(&registration).unwrap();
        assert_eq!(
            encoded["schedules"][0]["input"],
            serde_json::json!({"creator": "input"})
        );
        assert_eq!(
            serde_json::from_value::<DeployRegistration>(encoded.clone()).unwrap(),
            registration
        );
        let mut invalid = encoded;
        invalid["appId"] = serde_json::json!(AppId::mint());
        assert!(serde_json::from_value::<DeployRegistration>(invalid).is_err());
    }
}
