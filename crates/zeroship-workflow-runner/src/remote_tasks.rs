//! The `TaskPayloads` seam for a host that holds no journal.
//!
//! Every method here is the same shape: ask the journal's service for the one
//! fact only it has, then do the byte work locally with the store this host
//! already binds. Nothing large crosses -- an executable's module source answers
//! to a budget twice the journal reply ceiling, and a payload answers to a larger
//! one still, so neither could cross even if it were desirable.
//!
//! # Why `stage` is two calls and the others are one
//!
//! Reading needs one crossing because the journal contributes only the object
//! key. Staging needs two because the journal contributes a key BEFORE the write
//! and a state change AFTER it, and in one process those sit inside a lock held
//! across the write so collection cannot race a live writer. No request boundary
//! can hold that lock, so the confirm becomes a compare-and-swap that rides the
//! settlement -- see `PayloadConfirmation` and the fence in
//! `zeroship_workflow::service::payloads::collection`.

#[cfg(test)]
mod tests;

use crate::payloads::{ObjectWriter, PayloadObjects, PayloadRead, TaskPayloads, UploadReceipt};
use async_trait::async_trait;
use std::{sync::Arc, time::Duration};
use zeroship_bundle::{verify_deployment_manifest, BlobStore, LoadedWorker};
use zeroship_core::{
    app_id::AppId,
    workflow_coordination::{
        PayloadReservation, ReadTaskPayload, ReservePayload, ResolveTaskExecutable,
    },
};
use zeroship_workflow::{
    engine::WorkflowOutputRef,
    service::{delivery::PayloadConfirmation, RequestId, TaskToken},
    WorkflowServiceError,
};
use zeroship_workflow_client::WorkerCoordinator;

/// Task payload authority for a host whose journal is another process's.
pub struct RemoteTasks {
    client: WorkerCoordinator,
    app: AppId,
    objects: PayloadObjects,
    artifacts: Arc<dyn BlobStore>,
    max_source_bytes: usize,
    read_limit: usize,
    write_budget: Duration,
}

impl std::fmt::Debug for RemoteTasks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteTasks")
            .field("app", &self.app)
            .finish_non_exhaustive()
    }
}

impl RemoteTasks {
    /// # Errors
    /// Rejects an empty read budget, source budget or write budget.
    pub fn new(
        client: WorkerCoordinator,
        app: AppId,
        objects: PayloadObjects,
        artifacts: Arc<dyn BlobStore>,
        max_source_bytes: usize,
        read_limit: usize,
        write_budget: Duration,
    ) -> Result<Self, WorkflowServiceError> {
        if read_limit == 0 || max_source_bytes == 0 || write_budget.is_zero() {
            return Err(WorkflowServiceError::InvalidRequest(
                "remote task payload budgets must be positive".into(),
            ));
        }
        Ok(Self {
            client,
            app,
            objects,
            artifacts,
            max_source_bytes,
            read_limit,
            write_budget,
        })
    }

    const fn read_limit(&self) -> usize {
        self.read_limit
    }
}

#[async_trait(?Send)]
impl TaskPayloads for RemoteTasks {
    /// Resolve the pin, then load that deployment from this host's own store.
    ///
    /// THE JOURNAL DECIDES WHICH DEPLOYMENT AND THIS HOST READS IT. The pin is
    /// resolved from the run's current row under the journal's locks, including
    /// the availability check that a parked deployment fails -- a fence this host
    /// could not apply for itself, and the reason the resolution crosses rather
    /// than being derived from the assignment.
    ///
    /// The bytes are content-addressed by the deploy hash, so the object this
    /// opens is the same one the journal's own host would read.
    async fn executable(
        &self,
        task: &str,
        token: &TaskToken,
    ) -> Result<LoadedWorker, WorkflowServiceError> {
        let pinned = self
            .client
            .resolve_task_executable(&ResolveTaskExecutable {
                app_id: self.app.clone(),
                task_id: task.to_owned(),
                token: token.as_str().to_owned(),
            })
            .await
            .map_err(transport)?;
        let bytes = self
            .artifacts
            .get_manifest(&self.app, &pinned.deploy_hash)
            .await
            .map_err(|_| unavailable("app deployment artifact is unavailable"))?;
        if bytes.len() as u64 > zeroship_bundle::MAX_MANIFEST_BYTES {
            return Err(unavailable("app deployment manifest is invalid"));
        }
        // Verified against the hash the journal named, so bytes that are not the
        // pinned deployment's cannot be loaded under its identity.
        let manifest = verify_deployment_manifest(&bytes, &pinned.deploy_hash)
            .map_err(|_| unavailable("app deployment manifest is invalid"))?;
        LoadedWorker::load(&manifest, self.artifacts.as_ref(), self.max_source_bytes)
            .await
            .map_err(Into::into)
    }

    /// Reserve the row, write the object, and report the confirm the settlement
    /// owes.
    ///
    /// `confirm` IS ALWAYS PRESENT for a reserved upload, and that is the whole
    /// difference between this host and one holding the journal. Answering `None`
    /// would compile and pass, and every confirmation would be silently dropped:
    /// the settlement would then carry a frontier referencing rows still
    /// `uploading`, which `promote` refuses as a missing payload.
    ///
    /// An upload an earlier attempt already confirmed owes nothing, because the
    /// reservation answers `Staged` and there is no object left to write.
    async fn stage(
        &self,
        task: &str,
        token: &TaskToken,
        request: &RequestId,
        reference: WorkflowOutputRef,
        body: zeroship_storage::backend::BoxChunkSource,
    ) -> Result<UploadReceipt, WorkflowServiceError> {
        let reserved = self
            .client
            .reserve_task_payload(&ReservePayload {
                app_id: self.app.clone(),
                task_id: task.to_owned(),
                token: token.as_str().to_owned(),
                request_id: request.clone(),
                reference: reference.clone(),
            })
            .await
            .map_err(transport)?;
        match reserved {
            PayloadReservation::Staged { payload_id } => Ok(UploadReceipt {
                id: payload_id,
                reference,
                confirm: None,
            }),
            PayloadReservation::Reserved {
                payload_id,
                expires_at,
            } => {
                zeroship_workflow::service::PayloadWriter::write(
                    ObjectWriter {
                        objects: &self.objects,
                        body,
                    },
                    zeroship_workflow::service::PayloadTarget {
                        app: &self.app,
                        id: &payload_id,
                        reference: &reference,
                        authority: None,
                    },
                    self.write_budget,
                )
                .await?;
                Ok(UploadReceipt {
                    id: payload_id.clone(),
                    reference,
                    confirm: Some(PayloadConfirmation {
                        payload_id,
                        expires_at,
                    }),
                })
            }
        }
    }

    /// Locate the object, then open it from this host's own store.
    async fn read(
        &self,
        task: &str,
        token: &TaskToken,
        reference: &WorkflowOutputRef,
    ) -> Result<PayloadRead, WorkflowServiceError> {
        let located = self
            .client
            .read_task_payload(&ReadTaskPayload {
                app_id: self.app.clone(),
                task_id: task.to_owned(),
                token: token.as_str().to_owned(),
                reference: reference.clone(),
            })
            .await
            .map_err(transport)?;
        let _ = self.read_limit();
        self.objects
            .open_located(&self.app, &located.payload_id, &located.reference)
            .await
    }
}

fn unavailable(message: &str) -> WorkflowServiceError {
    WorkflowServiceError::Unavailable(message.into())
}

/// Carry a client refusal into the journal's contract.
///
/// A REFUSAL crosses as the code the journal chose, so a caller branches on the
/// same thing it would in process. A TRANSPORT failure establishes nothing about
/// whether the call took effect, so it becomes `Unavailable` and never a durable
/// refusal.
fn transport(error: zeroship_workflow_client::Error) -> WorkflowServiceError {
    use zeroship_core::workflow_coordination::FailureCode;
    use zeroship_workflow_client::Error;
    match error {
        Error::Refused(FailureCode::Invalid) => {
            WorkflowServiceError::InvalidRequest("workflow task payload request".into())
        }
        Error::Refused(FailureCode::Unauthenticated) | Error::Unauthenticated => {
            WorkflowServiceError::Unauthenticated
        }
        Error::Refused(FailureCode::Denied) => WorkflowServiceError::PermissionDenied,
        Error::Refused(FailureCode::Conflict) => {
            WorkflowServiceError::Conflict("workflow task payload".into())
        }
        Error::Refused(FailureCode::Capacity) => {
            WorkflowServiceError::ResourceExhausted("workflow task payload".into())
        }
        Error::Refused(FailureCode::RequestTooLarge) | Error::RequestTooLarge => {
            WorkflowServiceError::PayloadTooLarge
        }
        Error::Timeout => WorkflowServiceError::Timeout,
        Error::Refused(FailureCode::Unavailable)
        | Error::Unavailable
        | Error::InvalidConfig
        | Error::InvalidResponse
        | Error::ResponseTooLarge => unavailable("workflow service is unavailable"),
    }
}
