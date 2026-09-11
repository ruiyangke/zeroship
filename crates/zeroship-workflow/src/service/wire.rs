//! Requests shared by remote clients and the workflow HTTP host.

use super::{PayloadSlot, RequestId, TaskToken};
use crate::{engine::WorkflowOutputRef, WorkflowExecution, WorkflowServiceError};
use serde::{Deserialize, Serialize};

pub const TASK_TOKEN_HEADER: &str = "zeroship-workflow-task";
pub const REQUEST_ID_HEADER: &str = "zeroship-workflow-request";
pub const PAYLOAD_HEADER: &str = "zeroship-workflow-payload";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadTaskPayload {
    pub token: TaskToken,
    pub reference: WorkflowOutputRef,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadAppPayload {
    pub generation: i64,
    pub slot: PayloadSlot,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PollTask {}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Mutation<T> {
    pub request_id: RequestId,
    pub options: T,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskCredential {
    pub token: TaskToken,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompleteTask {
    pub token: TaskToken,
    pub execution: WorkflowExecution,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Failure {
    pub code: String,
    pub message: String,
}
impl Failure {
    #[must_use]
    pub fn from_error(error: &WorkflowServiceError) -> Self {
        Self {
            code: error.code().into(),
            message: match error {
                WorkflowServiceError::Internal(_) => "workflow service failed".into(),
                WorkflowServiceError::Unavailable(_) => "workflow service unavailable".into(),
                other => other.to_string(),
            },
        }
    }
    #[must_use]
    pub fn into_error(self) -> WorkflowServiceError {
        match self.code.as_str() {
            "workflow_invalid_request" => WorkflowServiceError::InvalidRequest(self.message),
            "workflow_unauthenticated" => WorkflowServiceError::Unauthenticated,
            "workflow_permission_denied" => WorkflowServiceError::PermissionDenied,
            "workflow_not_found" => WorkflowServiceError::NotFound(self.message),
            "workflow_conflict" => WorkflowServiceError::Conflict(self.message),
            "workflow_resource_exhausted" => WorkflowServiceError::ResourceExhausted(self.message),
            "workflow_payload_too_large" => WorkflowServiceError::PayloadTooLarge,
            "workflow_unavailable" => WorkflowServiceError::Unavailable(self.message),
            "workflow_timeout" => WorkflowServiceError::Timeout,
            _ => WorkflowServiceError::Internal("invalid workflow error response".into()),
        }
    }
}
