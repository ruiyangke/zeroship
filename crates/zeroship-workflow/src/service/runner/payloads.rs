//! Payload reads bound to a trusted task assignment and its replay journal.

#![allow(
    clippy::future_not_send,
    reason = "task payload I/O runs on its compio host thread"
)]

use super::{EmbeddedTasks, RemoteTasks};
use crate::{
    engine::{JournalStep, WorkflowOutputRef},
    service::{PayloadRead, TaskAssignment, TaskToken},
    validation, WorkflowServiceError,
};
use async_trait::async_trait;
use serde_json::Value;
use std::rc::Rc;
use zeroship_core::app_id::AppId;

/// Host-only payload authority; task credentials never enter the app isolate.
#[async_trait(?Send)]
pub trait TaskPayloads {
    async fn read(
        &self,
        task: &str,
        token: &TaskToken,
        reference: &WorkflowOutputRef,
    ) -> Result<PayloadRead, WorkflowServiceError>;
}

#[async_trait(?Send)]
impl TaskPayloads for EmbeddedTasks {
    async fn read(
        &self,
        task: &str,
        token: &TaskToken,
        reference: &WorkflowOutputRef,
    ) -> Result<PayloadRead, WorkflowServiceError> {
        self.service
            .read_task_payload(&self.worker, task, token, reference)
            .await
    }
}

#[async_trait(?Send)]
impl TaskPayloads for RemoteTasks {
    async fn read(
        &self,
        task: &str,
        token: &TaskToken,
        reference: &WorkflowOutputRef,
    ) -> Result<PayloadRead, WorkflowServiceError> {
        self.read_payload(task, token, reference).await
    }
}

/// Resolves names only in the assigned journal, including after run restart.
///
/// Remote object reads still require the assignment's live lease. Inline values
/// are already part of the trusted replay envelope and need no further I/O.
pub struct TaskPayloadReader {
    transport: Rc<dyn TaskPayloads>,
    task: String,
    token: TaskToken,
    app: AppId,
    run: String,
    journal: Vec<JournalStep>,
    input_ref: Option<WorkflowOutputRef>,
    max_bytes: usize,
}
impl std::fmt::Debug for TaskPayloadReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskPayloadReader")
            .field("task", &self.task)
            .finish_non_exhaustive()
    }
}
impl TaskPayloadReader {
    /// Capture the replay journal and read budget from a host assignment.
    ///
    /// # Errors
    /// Rejects an empty budget or invalid app identity.
    pub fn new(
        transport: Rc<dyn TaskPayloads>,
        assignment: &TaskAssignment,
        max_bytes: usize,
    ) -> Result<Self, WorkflowServiceError> {
        if max_bytes == 0 {
            return Err(WorkflowServiceError::InvalidRequest(
                "workflow payload read limit must be positive".into(),
            ));
        }
        Ok(Self {
            transport,
            task: assignment.id.clone(),
            token: assignment.token.clone(),
            app: AppId::parse(&assignment.invocation.app_id).map_err(|_| {
                WorkflowServiceError::InvalidRequest(
                    "invalid workflow assignment app identity".into(),
                )
            })?,
            run: assignment.invocation.run_id.clone(),
            journal: assignment.invocation.journal.clone(),
            input_ref: assignment.invocation.trigger.input_ref.clone(),
            max_bytes,
        })
    }

    #[must_use]
    pub fn run_id(&self) -> &str {
        &self.run
    }

    #[must_use]
    pub const fn app_id(&self) -> &AppId {
        &self.app
    }

    /// Hydrate only the input reference captured from this assignment.
    ///
    /// # Errors
    /// Rejects oversized, unavailable, corrupt or non-JSON input payloads.
    pub async fn input(&self) -> Result<Option<Value>, WorkflowServiceError> {
        let Some(reference) = &self.input_ref else {
            return Ok(None);
        };
        let bytes = self.bytes(reference).await?;
        serde_json::from_slice(&bytes).map(Some).map_err(|_| {
            WorkflowServiceError::Unavailable("workflow input payload is not valid JSON".into())
        })
    }

    /// Read a completed occurrence from the assigned journal.
    ///
    /// # Errors
    /// Rejects invalid selectors, absent outputs, expired task authority and
    /// oversized, unavailable or corrupt payloads.
    pub async fn read_step_output(
        &self,
        name: &str,
        occurrence: u32,
    ) -> Result<Vec<u8>, WorkflowServiceError> {
        validation::step_name(name)?;
        let occurrence = i32::try_from(occurrence).map_err(|_| {
            WorkflowServiceError::InvalidRequest("invalid step name occurrence".into())
        })?;
        let step = self
            .journal
            .iter()
            .find(|step| {
                step.name == name && step.name_occurrence == occurrence && step.state == "completed"
            })
            .ok_or_else(|| {
                WorkflowServiceError::NotFound(
                    "workflow step output not found in assigned journal".into(),
                )
            })?;
        if let Some(reference) = &step.output_ref {
            return self.bytes(reference).await;
        }
        let bytes = serde_json::to_vec(&step.output).map_err(|_| {
            WorkflowServiceError::Internal("invalid assigned workflow output".into())
        })?;
        if bytes.len() > self.max_bytes {
            return Err(WorkflowServiceError::PayloadTooLarge);
        }
        Ok(bytes)
    }

    async fn bytes(&self, reference: &WorkflowOutputRef) -> Result<Vec<u8>, WorkflowServiceError> {
        // Refuse before asking the transport to open an oversized object.
        if usize::try_from(reference.size)
            .ok()
            .is_none_or(|size| size > self.max_bytes)
        {
            return Err(WorkflowServiceError::PayloadTooLarge);
        }
        let read = self
            .transport
            .read(&self.task, &self.token, reference)
            .await?;
        if read.reference != *reference {
            return Err(WorkflowServiceError::Unavailable(
                "workflow replay payload descriptor changed".into(),
            ));
        }
        read.into_bytes(self.max_bytes).await
    }
}
