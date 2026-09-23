//! Prepare runtime results and retain upload identities through transport retries.

#![allow(
    clippy::future_not_send,
    reason = "payload transport uses the host's compio thread"
)]

use crate::TaskPayloads;
use zeroship_workflow::{
    engine::{StepOutcome, WorkflowOutputRef},
    execution::decode_runtime_outcomes,
    service::{RequestId, TaskAssignment, TaskToken},
    WorkflowExecution, WorkflowServiceError,
};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use zeroship_core::workflow_policy::MAX_PAYLOAD_BYTES_CEILING;
use zeroship_storage::backend::OnceChunk;

/// Host memory and inline-journal budgets. `max_payload_bytes` and the policy
/// bound of the same name describe one quantity and answer to one platform
/// ceiling; the other two are this host's own.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskPayloadLimits {
    pub max_inline_bytes: usize,
    pub max_payload_bytes: usize,
    pub max_result_bytes: usize,
}
impl Default for TaskPayloadLimits {
    /// Only `max_payload_bytes` derives from a platform ceiling, because it is
    /// the only one of these three measuring the same bytes as a policy bound:
    /// it is the budget a staged payload is read back through, and
    /// `AppPolicy::max_payload_bytes` is the budget the same payload was
    /// admitted under, so one constant serves both.
    ///
    /// `max_result_bytes` bounds the runtime's whole result text and no policy
    /// bound measures it. `max_inline_bytes` decides whether a value is carried
    /// inline or replaced by a reference, and an inline value then rides inside
    /// the checkpoint `AppPolicy::max_input_bytes` bounds - a part of that
    /// quantity rather than the same one, by an amount only the checkpoint's
    /// own framing settles. A single constant cannot serve both ends of a
    /// containment, so this one stays the host's.
    fn default() -> Self {
        Self {
            max_inline_bytes: 1024 * 1024,
            max_payload_bytes: MAX_PAYLOAD_BYTES_CEILING,
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

    /// Refuse a configured host budget that cannot carry what the platform
    /// admits, before this host starts serving.
    ///
    /// This is the startup half of one invariant whose other half is
    /// [`zeroship_core::workflow_policy::AppPolicy::validate`]. Startup knows
    /// the configured budget and has observed no policy; admission knows the
    /// policy and cannot re-read what this host was configured with. Either
    /// check alone leaves one direction open.
    ///
    /// For a host that takes [`Self::default`] the budget is at the ceiling by
    /// derivation, so this belongs to a host whose budget arrives from
    /// configuration and could be anything.
    ///
    /// Distinct from [`Self::validate`], which compares a budget against
    /// itself and runs on every execution, including the deliberately narrow
    /// budgets that exercise the limit outcomes.
    ///
    /// # Errors
    /// Rejects an unusable budget, and one whose payload read budget is below
    /// the platform ceiling.
    pub fn validate_configured(self) -> Result<(), WorkflowServiceError> {
        self.validate()?;
        if self.max_payload_bytes < MAX_PAYLOAD_BYTES_CEILING {
            return Err(invalid(
                "workflow payload read budget is below the platform ceiling",
            ));
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
    /// Exceeding a host data limit produces a limit outcome instead, named for
    /// the `run` step whose output it was, or for nothing when no step owns it.
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
            result.outcomes.push(limit_failure(None));
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
            // Read before the borrow below: a refused payload is recorded
            // against the step that produced it, and that step's identity is
            // still here while the outcome is.
            let step = limit_step(&outcome);
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
                result.outcomes.push(limit_failure(step));
                break;
            }
            if can_reference && (mode.requires_reference() || bytes.len() > limits.max_inline_bytes)
            {
                let descriptor = WorkflowOutputRef {
                    hash: zeroship_workflow::service::hash(&bytes),
                    size: i64::try_from(bytes.len())
                        .map_err(|_| WorkflowServiceError::PayloadTooLarge)?,
                    content_type: Some(content_type.unwrap_or_else(|| "application/json".into())),
                };
                zeroship_workflow::service::validate_reference(&descriptor)?;
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
                    // The outcome that owns this upload is still held, one past
                    // the prefix the service accepted. Uploads deduplicate by
                    // reference, so a payload several outcomes share is named
                    // for the first of them, which is where the batch stops.
                    outcomes.push(limit_failure(
                        self.outcomes.get(upload.outcome).and_then(limit_step),
                    ));
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
/// The journal identity a refused payload is recorded against.
struct LimitStep {
    ordinal: i32,
    name: String,
    name_occurrence: i32,
}

/// The step that owns an outcome's payload, when one does.
///
/// Only a `run` step has a journal row a failure can be written to:
/// `fold_outcomes` records a named `RunFailed` as `kind: "run"`, so naming a
/// `sideEffect` ordinal would replay as a kind mismatch. A run output and a
/// continuation seed belong to the run, not to any step.
fn limit_step(outcome: &StepOutcome) -> Option<LimitStep> {
    match outcome {
        StepOutcome::StepCompleted {
            ordinal,
            name,
            name_occurrence,
            step_kind,
            ..
        } if step_kind == "run" => Some(LimitStep {
            ordinal: *ordinal,
            name: name.clone(),
            name_occurrence: *name_occurrence,
        }),
        _ => None,
    }
}

/// The outcome a payload over a host or service budget is replaced by.
///
/// Named for a step, this is the step's terminal failure: the body ran and
/// produced a value the platform refuses, so `retryable` is false and the one
/// attempt it declares is the one already spent. The run keeps going, and the
/// body resumes at that row, so a `catch` around the step can take another
/// path. Named for nothing, it is the run's verdict and the run rests failed.
fn limit_failure(step: Option<LimitStep>) -> StepOutcome {
    let (ordinal, name, name_occurrence, message) = match step {
        Some(step) => (
            Some(step.ordinal),
            Some(step.name),
            step.name_occurrence,
            "workflow step output exceeds the configured payload limits",
        ),
        None => (
            None,
            None,
            0,
            "workflow result exceeds the configured payload limits",
        ),
    };
    StepOutcome::RunFailed {
        ordinal,
        name,
        name_occurrence,
        error: json!({
            "type":"LimitExceededError",
            "message":message,
            "retryable":false
        }),
        max_attempts: 1,
    }
}

#[cfg(test)]
mod tests;
