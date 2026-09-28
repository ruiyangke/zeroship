//! Payload authority bound to a trusted task assignment and its replay journal.

#![allow(
    clippy::future_not_send,
    reason = "task payload I/O runs on its compio host thread"
)]

mod objects;
#[cfg(test)]
mod tests;
pub use objects::{
    AppPayloads, HostPayloads, ObjectStepOutputs, PayloadObjects, PayloadRead, RunPayloads,
    WorkerPayloads,
};
pub(crate) use objects::ObjectWriter;

use crate::WorkerTasks;
use zeroship_workflow::{
    engine::{JournalStep, WorkflowOutputRef},
    service::{delivery::PayloadConfirmation, RequestId, TaskAssignment, TaskToken},
    validation, WorkflowServiceError,
};
use async_trait::async_trait;
use serde_json::Value;
use std::{
    cell::RefCell,
    rc::Rc,
    task::{Poll, Waker},
};
use zeroship_bundle::LoadedWorker;
use zeroship_core::app_id::AppId;
use zeroship_storage::backend::BoxChunkSource;

/// Host-only payload authority; task credentials never enter the app isolate.
#[async_trait(?Send)]
pub trait TaskPayloads {
    async fn executable(
        &self,
        task: &str,
        token: &TaskToken,
    ) -> Result<LoadedWorker, WorkflowServiceError>;
    async fn stage(
        &self,
        task: &str,
        token: &TaskToken,
        request: &RequestId,
        reference: WorkflowOutputRef,
        body: BoxChunkSource,
    ) -> Result<UploadReceipt, WorkflowServiceError>;
    async fn read(
        &self,
        task: &str,
        token: &TaskToken,
        reference: &WorkflowOutputRef,
    ) -> Result<PayloadRead, WorkflowServiceError>;
}

#[async_trait(?Send)]
impl TaskPayloads for WorkerTasks {
    async fn executable(
        &self,
        task: &str,
        token: &TaskToken,
    ) -> Result<LoadedWorker, WorkflowServiceError> {
        self.service
            .task_executable(&self.worker, task, token)
            .await
    }
    async fn stage(
        &self,
        task: &str,
        token: &TaskToken,
        request: &RequestId,
        reference: WorkflowOutputRef,
        body: BoxChunkSource,
    ) -> Result<UploadReceipt, WorkflowServiceError> {
        let staged = self
            .service
            .payloads(&self.objects)
            .stage(&self.worker, task, token, request, reference, body)
            .await?;
        // This host holds the object store, so staging confirmed under its own
        // lock before returning and there is nothing left for the settlement to
        // do. A host that writes across a boundary answers with a confirmation.
        Ok(UploadReceipt {
            id: staged.id,
            reference: staged.reference,
            confirm: None,
        })
    }
    async fn read(
        &self,
        task: &str,
        token: &TaskToken,
        reference: &WorkflowOutputRef,
    ) -> Result<PayloadRead, WorkflowServiceError> {
        self.service
            .payloads(&self.objects)
            .read_task(&self.worker, task, token, reference)
            .await
    }
}

/// Resolves names only in the assigned journal, including after run restart.
///
/// Customer object reads still require the assignment's live lease. Inline values
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
    failure: RefCell<Option<WorkflowServiceError>>,
    failure_waker: RefCell<Option<Waker>>,
}
impl std::fmt::Debug for TaskPayloadReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskPayloadReader")
            .field("task", &self.task)
            .finish_non_exhaustive()
    }
}
impl TaskPayloadReader {
    /// Load the immutable executable under the captured task authority.
    ///
    /// # Errors
    /// Rejects expired claims, missing artifacts and integrity failures.
    pub async fn executable(&self) -> Result<LoadedWorker, WorkflowServiceError> {
        self.transport.executable(&self.task, &self.token).await
    }
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
            failure: RefCell::new(None),
            failure_waker: RefCell::new(None),
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

    /// Check whether replay lost access to a required referenced payload.
    ///
    /// # Errors
    /// Once a reference read fails, this reader remains failed. The host must
    /// abandon this execution rather than commit a caught app exception as a
    /// new journal outcome. A fresh task attempt receives a fresh reader.
    pub fn check(&self) -> Result<(), WorkflowServiceError> {
        self.failure.borrow().clone().map_or(Ok(()), Err)
    }

    /// Wait for a failed replay dependency.
    ///
    /// The execution host owns this waiter
    /// and races it against the runtime result; stopping V8 need not settle its
    /// JavaScript promises.
    pub async fn failed(&self) -> WorkflowServiceError {
        std::future::poll_fn(|cx| {
            if let Err(error) = self.check() {
                Poll::Ready(error)
            } else {
                *self.failure_waker.borrow_mut() = Some(cx.waker().clone());
                Poll::Pending
            }
        })
        .await
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
        self.check()?;
        let result = self.read_verified(reference).await;
        if let Err(error) = &result {
            self.failure
                .borrow_mut()
                .get_or_insert_with(|| error.clone());
            let waker = self.failure_waker.borrow_mut().take();
            if let Some(waker) = waker {
                waker.wake();
            }
        }
        // Another concurrent read may have failed while this one was pending.
        self.check()?;
        result
    }

    async fn read_verified(
        &self,
        reference: &WorkflowOutputRef,
    ) -> Result<Vec<u8>, WorkflowServiceError> {
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

/// What one upload answered, and whether its settlement still owes a confirm.
///
/// # Why this is not `StagedPayload`
///
/// `StagedPayload` promises the object is written AND the row says `staged`. A
/// host holding the object store can promise both, because it confirms under the
/// same lock it wrote under. A host writing across a request boundary cannot: the
/// reservation and the confirm are separate calls, and the confirm has to land in
/// the transaction that commits the frontier referencing the object -- `promote`
/// resolves no `uploading` row, so an outcome naming an unconfirmed object is
/// refused as a missing payload.
///
/// So `confirm` is the difference between the two hosts, and it is deliberately
/// not an implementation detail of either: the settlement is what carries it, and
/// only the host that uploaded knows whether one is owed.
///
/// `reference` is answered rather than assumed, so the caller can refuse a
/// receipt describing another object -- a check that cannot fire in process,
/// where the descriptor is returned by value from the caller's own argument, and
/// can over a wire where the reply is decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadReceipt {
    pub id: String,
    pub reference: WorkflowOutputRef,
    pub confirm: Option<PayloadConfirmation>,
}
