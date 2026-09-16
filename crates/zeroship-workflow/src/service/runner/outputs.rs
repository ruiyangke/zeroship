//! Prepare runtime results and retain upload identities through transport retries.

#![allow(
    clippy::future_not_send,
    reason = "payload transport uses the host's compio thread"
)]

use super::TaskPayloads;
use crate::{
    engine::{StepOutcome, WorkflowOutputRef},
    execution::decode_runtime_outcomes,
    service::{RequestId, TaskAssignment, TaskToken},
    WorkflowExecution, WorkflowServiceError,
};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use zeroship_storage::backend::OnceChunk;

/// Host memory and inline-journal budgets. Service policy remains authoritative.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskPayloadLimits {
    pub max_inline_bytes: usize,
    pub max_payload_bytes: usize,
    pub max_result_bytes: usize,
}
impl Default for TaskPayloadLimits {
    fn default() -> Self {
        Self {
            max_inline_bytes: 1024 * 1024,
            max_payload_bytes: 64 * 1024 * 1024,
            max_result_bytes: 128 * 1024 * 1024,
        }
    }
}
impl TaskPayloadLimits {
    /// # Errors
    /// Rejects empty, inverted or unrepresentable budgets.
    pub fn validate(self) -> Result<(), WorkflowServiceError> {
        if self.max_inline_bytes == 0
            || self.max_payload_bytes < self.max_inline_bytes
            || self.max_result_bytes < self.max_payload_bytes
            || i64::try_from(self.max_payload_bytes).is_err()
        {
            return Err(invalid("invalid workflow payload limits"));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum OutputMode {
    #[default]
    Auto,
    Inline,
    Ref,
    Blob,
    Stream,
}
impl OutputMode {
    fn requires_reference(self) -> bool {
        matches!(self, Self::Ref | Self::Blob | Self::Stream)
    }
}

#[derive(Deserialize)]
struct RuntimeOutcome {
    #[serde(flatten)]
    outcome: StepOutcome,
    #[serde(default, rename = "outputMode")]
    mode: Option<OutputMode>,
    #[serde(default, rename = "outputContentType")]
    content_type: Option<String>,
}

struct Upload {
    request: RequestId,
    reference: WorkflowOutputRef,
    bytes: Bytes,
    outcome: usize,
}

/// A decoded frontier and its task-bound, repeatable uploads.
///
/// Keep this object while retrying `stage`; decoding again would mint new upload
/// identities. No app callback participates in staging or a staging retry.
pub struct PreparedExecution {
    task: String,
    token: TaskToken,
    outcomes: Vec<StepOutcome>,
    uploads: Vec<Upload>,
}
impl std::fmt::Debug for PreparedExecution {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedExecution")
            .field("task", &self.task)
            .finish_non_exhaustive()
    }
}
impl PreparedExecution {
    /// Decode the complete batch before starting any upload.
    ///
    /// # Errors
    /// Rejects malformed runtime results or invalid output configuration.
    /// Exceeding a host data limit produces a terminal limit outcome instead.
    pub fn from_runtime_json(
        assignment: &TaskAssignment,
        source: &str,
        limits: TaskPayloadLimits,
    ) -> Result<Self, WorkflowServiceError> {
        limits.validate()?;
        let mut result = Self {
            task: assignment.id.clone(),
            token: assignment.token.clone(),
            outcomes: Vec::new(),
            uploads: Vec::new(),
        };
        if source.len() > limits.max_result_bytes {
            result.outcomes.push(limit_failure());
            return Ok(result);
        }
        let raw =
            serde_json::from_str(source).map_err(|_| invalid("invalid workflow execution JSON"))?;
        let raw = decode_runtime_outcomes(raw)?;
        let outcomes: Vec<RuntimeOutcome> = serde_json::from_value(Value::Array(raw))
            .map_err(|_| invalid("invalid workflow execution output configuration"))?;
        for entry in outcomes {
            let RuntimeOutcome {
                mut outcome,
                mode,
                content_type,
            } = entry;
            if (mode.is_some() || content_type.is_some())
                && !matches!(&outcome, StepOutcome::StepCompleted { step_kind, .. } if step_kind == "run")
            {
                return Err(invalid("output configuration requires step.run"));
            }
            let (value, reference, can_reference) = match &mut outcome {
                StepOutcome::StepCompleted {
                    output,
                    output_ref,
                    step_kind,
                    ..
                } => (output, output_ref, step_kind == "run"),
                StepOutcome::RunCompleted { output, output_ref } => (output, output_ref, true),
                StepOutcome::ContinueAsNew { input, input_ref } => (input, input_ref, true),
                _ => {
                    result.outcomes.push(outcome);
                    continue;
                }
            };
            if reference.is_some() {
                if value.is_some() {
                    return Err(invalid(
                        "workflow output cannot contain inline data and a reference",
                    ));
                }
                result.outcomes.push(outcome);
                continue;
            }
            let bytes =
                serde_json::to_vec(value).map_err(|_| invalid("invalid workflow output"))?;
            let mode = mode.unwrap_or_default();
            if bytes.len() > limits.max_payload_bytes
                || (bytes.len() > limits.max_inline_bytes
                    && (matches!(mode, OutputMode::Inline) || !can_reference))
            {
                result.outcomes.push(limit_failure());
                break;
            }
            if can_reference && (mode.requires_reference() || bytes.len() > limits.max_inline_bytes)
            {
                let descriptor = WorkflowOutputRef {
                    hash: crate::service::types::hash(&bytes),
                    size: i64::try_from(bytes.len())
                        .map_err(|_| WorkflowServiceError::PayloadTooLarge)?,
                    content_type: Some(content_type.unwrap_or_else(|| "application/json".into())),
                };
                crate::service::payloads::validate_reference(&descriptor)?;
                if !result
                    .uploads
                    .iter()
                    .any(|upload| upload.reference == descriptor)
                {
                    result.uploads.push(Upload {
                        request: RequestId::mint(),
                        reference: descriptor.clone(),
                        bytes: bytes.into(),
                        outcome: result.outcomes.len(),
                    });
                }
                *value = None;
                *reference = Some(descriptor);
            }
            result.outcomes.push(outcome);
        }
        Ok(result)
    }

    /// Confirm uploads before returning a frontier ready for journal commit.
    ///
    /// # Errors
    /// Reports transport failures or lost task authority. Callers may retry
    /// with this same batch while the execution lease permits it.
    pub async fn stage(
        &self,
        transport: &dyn TaskPayloads,
    ) -> Result<WorkflowExecution, WorkflowServiceError> {
        for upload in &self.uploads {
            let staged = transport
                .stage(
                    &self.task,
                    &self.token,
                    &upload.request,
                    upload.reference.clone(),
                    Box::new(OnceChunk::new(upload.bytes.clone())),
                )
                .await;
            match staged {
                Ok(receipt) if receipt.reference == upload.reference => {}
                Ok(_) => {
                    return Err(WorkflowServiceError::Unavailable(
                        "workflow upload receipt changed".into(),
                    ))
                }
                Err(WorkflowServiceError::PayloadTooLarge) => {
                    let mut outcomes = self.outcomes[..upload.outcome].to_vec();
                    outcomes.push(limit_failure());
                    return Ok(WorkflowExecution { outcomes });
                }
                Err(error) => return Err(error),
            }
        }
        Ok(WorkflowExecution {
            outcomes: self.outcomes.clone(),
        })
    }
}

fn invalid(message: &str) -> WorkflowServiceError {
    WorkflowServiceError::InvalidRequest(message.into())
}
fn limit_failure() -> StepOutcome {
    StepOutcome::RunFailed {
        ordinal: None,
        name: None,
        name_occurrence: 0,
        error: json!({
            "type":"LimitExceededError",
            "message":"workflow result exceeds the configured payload limits",
            "retryable":false
        }),
    }
}
