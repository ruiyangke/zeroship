//! The workflow host's payload object store and every read and write over it.
//!
//! Admission records which payload a run owns and proves that ownership; it
//! never moves a payload byte. This module supplies the effects admission
//! orchestrates, and the verified stream handles execution reads them through.

use async_trait::async_trait;
use sha2::{Digest, Sha256};
use std::{cell::Cell, rc::Rc, sync::Arc, time::Duration};
use zeroship_core::{app_id::AppId, workflow_jobs::JobLease};
use zeroship_storage::{
    backend::{BoxByteStream, BoxChunkSource, ChunkResult, ChunkSource, OnceChunk},
    Namespace, Storage, StorageError, StorageStore,
};
use zeroship_workflow::{
    engine::WorkflowOutputRef,
    service::{
        collection::CollectionOptions, delivery::JobReceipt, validate_reference, AppWorkflows,
        PayloadDeleter, PayloadOpener, PayloadSlot, PayloadTarget, PayloadWriter, PolicyAuthority,
        RequestId, StagedPayload, StepOutput, TaskToken, WorkerIdentity, WorkflowService,
    },
    StepOutputReader, WorkflowServiceError,
};

/// The payload store, namespaced under credentials private to the workflow
/// host. App code reaches its own buckets through its own storage binding.
#[derive(Clone, Debug)]
pub struct PayloadObjects(Storage);

impl PayloadObjects {
    /// Use a store whose credentials are private to the workflow host.
    ///
    /// # Errors
    /// Reports a store that refuses the platform namespace.
    pub fn open(store: StorageStore) -> Result<Self, WorkflowServiceError> {
        Ok(Self(store.namespace(
            Namespace::platform("workflow").map_err(storage_error)?,
        )))
    }
}

#[cfg(test)]
impl PayloadObjects {
    /// A store on a directory that lives as long as the returned guard.
    pub(crate) fn temporary() -> (Self, tempfile::TempDir) {
        let directory = tempfile::tempdir().unwrap();
        let store = StorageStore::from_backend(std::sync::Arc::new(
            zeroship_storage::LocalFs::new(directory.path()),
        ));
        (Self::open(store).unwrap(), directory)
    }
}

/// A payload body opened against its descriptor. The stream verifies size and
/// digest as it is consumed, so a truncated or substituted object fails at the
/// point of use rather than being handed on as content.
pub struct PayloadRead {
    pub reference: WorkflowOutputRef,
    pub body: BoxByteStream,
}
impl PayloadRead {
    fn guarded(mut self, authority: Option<Arc<PolicyAuthority>>) -> Self {
        if let Some(authority) = authority {
            self.body = Box::new(AuthorizedSource {
                inner: Some(self.body),
                authority,
            });
        }
        self
    }

    /// Collect a verified payload within the host's memory budget.
    ///
    /// # Errors
    /// Rejects oversized descriptors, interrupted bodies and corrupt content.
    pub async fn into_bytes(mut self, limit: usize) -> Result<Vec<u8>, WorkflowServiceError> {
        let size = usize::try_from(self.reference.size)
            .ok()
            .filter(|size| *size <= limit)
            .ok_or(WorkflowServiceError::PayloadTooLarge)?;
        let mut bytes = Vec::new();
        while let Some(chunk) = self.body.next_chunk().await {
            let chunk = chunk.map_err(|_| {
                WorkflowServiceError::Unavailable(
                    "workflow payload read failed integrity verification".into(),
                )
            })?;
            if bytes
                .len()
                .checked_add(chunk.len())
                .is_none_or(|size| size > limit)
            {
                return Err(WorkflowServiceError::PayloadTooLarge);
            }
            bytes.extend_from_slice(&chunk);
        }
        if bytes.len() != size {
            return Err(WorkflowServiceError::Unavailable(
                "workflow payload size changed".into(),
            ));
        }
        Ok(bytes)
    }

    /// Wrap a body in the verification its descriptor demands.
    ///
    /// # Errors
    /// Rejects a malformed descriptor.
    pub fn checked(
        reference: WorkflowOutputRef,
        body: BoxByteStream,
    ) -> Result<Self, WorkflowServiceError> {
        validate_reference(&reference)?;
        Ok(Self {
            reference: reference.clone(),
            body: Box::new(VerifiedSource {
                inner: body,
                expected: reference,
                bytes: 0,
                hash: Sha256::new(),
                verified: Rc::new(Cell::new(false)),
                finished: false,
            }),
        })
    }
}
impl std::fmt::Debug for PayloadRead {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PayloadRead")
            .field("reference", &self.reference)
            .finish_non_exhaustive()
    }
}

struct ObjectWriter<'a> {
    objects: &'a PayloadObjects,
    body: BoxChunkSource,
}
#[async_trait(?Send)]
impl PayloadWriter for ObjectWriter<'_> {
    async fn write(
        self,
        target: PayloadTarget<'_>,
        budget: Duration,
    ) -> Result<(), WorkflowServiceError> {
        let verified = Rc::new(Cell::new(false));
        let source = VerifiedSource {
            inner: self.body,
            expected: target.reference.clone(),
            bytes: 0,
            hash: Sha256::new(),
            verified: verified.clone(),
            finished: false,
        };
        let written = compio::time::timeout(
            budget,
            self.objects.0.put_stream(
                target.app.as_str(),
                target.id,
                Box::new(source),
                target.reference.content_type.as_deref(),
            ),
        )
        .await
        .map_err(|_| WorkflowServiceError::Timeout)?
        .map_err(storage_error)?;
        if !verified.get() || written != target.reference.size as u64 {
            return Err(WorkflowServiceError::Unavailable(
                "workflow payload store did not verify the upload".into(),
            ));
        }
        Ok(())
    }
}

struct ObjectOpener<'a>(&'a PayloadObjects);
#[async_trait(?Send)]
impl PayloadOpener for ObjectOpener<'_> {
    type Read = PayloadRead;
    async fn open(self, target: PayloadTarget<'_>) -> Result<PayloadRead, WorkflowServiceError> {
        let ObjectOpener(objects) = self;
        let (meta, body) = objects
            .0
            .get_stream(target.app.as_str(), target.id)
            .await
            .map_err(storage_error)?
            .ok_or_else(|| {
                WorkflowServiceError::Unavailable("committed workflow payload is missing".into())
            })?;
        if meta.size != target.reference.size as u64 {
            return Err(WorkflowServiceError::Unavailable(
                "committed workflow payload size changed".into(),
            ));
        }
        Ok(PayloadRead::checked(target.reference.clone(), body)?.guarded(target.authority.cloned()))
    }
}

#[async_trait(?Send)]
impl PayloadDeleter for PayloadObjects {
    async fn delete(&self, app: &AppId, id: &str) -> Result<(), WorkflowServiceError> {
        self.0
            .delete(app.as_str(), id)
            .await
            .map(|_| ())
            .map_err(storage_error)
    }
}

fn storage_error(error: StorageError) -> WorkflowServiceError {
    match error {
        StorageError::InvalidArgument(_) => WorkflowServiceError::InvalidRequest(
            "workflow payload did not match its descriptor".into(),
        ),
        StorageError::LimitExceeded(_) => WorkflowServiceError::PayloadTooLarge,
        _ => WorkflowServiceError::Unavailable("workflow payload storage failed".into()),
    }
}

/// Binds a host's payload byte operations to its object store.
///
/// `WorkflowService` belongs to the admission crate, so an inherent `impl` on
/// it is refused: this trait is local, which is what the orphan rule asks.
/// Callers `use` it to reach `payloads`.
pub trait WorkerPayloads {
    #[must_use]
    fn payloads<'a>(&'a self, objects: &'a PayloadObjects) -> HostPayloads<'a>;
}
impl WorkerPayloads for WorkflowService {
    fn payloads<'a>(&'a self, objects: &'a PayloadObjects) -> HostPayloads<'a> {
        HostPayloads {
            service: self,
            objects,
        }
    }
}

/// Host-wide payload operations: worker uploads, task reads and collection.
#[derive(Debug, Clone, Copy)]
pub struct HostPayloads<'a> {
    service: &'a WorkflowService,
    objects: &'a PayloadObjects,
}
impl HostPayloads<'_> {
    /// Upload against current task ownership.
    ///
    /// # Errors
    /// Rejects stale task or policy authority, invalid content and store failures.
    pub async fn stage(
        &self,
        worker: &WorkerIdentity,
        task_id: &str,
        token: &TaskToken,
        request: &RequestId,
        reference: WorkflowOutputRef,
        body: BoxChunkSource,
    ) -> Result<StagedPayload, WorkflowServiceError> {
        self.service
            .stage_payload(
                worker,
                task_id,
                token,
                request,
                reference,
                ObjectWriter {
                    objects: self.objects,
                    body,
                },
            )
            .await
    }

    /// Read along a committed replay edge or this task's own staged object.
    ///
    /// # Errors
    /// Rejects stale authority, unrelated payloads and unavailable or corrupt objects.
    pub async fn read_task(
        &self,
        worker: &WorkerIdentity,
        task_id: &str,
        token: &TaskToken,
        reference: &WorkflowOutputRef,
    ) -> Result<PayloadRead, WorkflowServiceError> {
        self.service
            .read_task_payload(worker, task_id, token, reference, ObjectOpener(self.objects))
            .await
    }

    /// Collect unreferenced expired uploads across this host's assigned apps.
    ///
    /// # Errors
    /// Rejects an invalid batch size and reports journal or deletion failures.
    pub async fn collect(&self, limit: usize) -> Result<usize, WorkflowServiceError> {
        self.service.collect_payloads(limit, self.objects).await
    }
}

/// Binds an app's payload byte operations to its host's object store.
///
/// `AppWorkflows` belongs to the admission crate, so an inherent `impl` on it
/// is refused: this trait is local, which is what the orphan rule asks.
pub trait RunPayloads {
    #[must_use]
    fn payloads<'a>(&'a self, objects: &'a PayloadObjects) -> AppPayloads<'a>;
}
impl RunPayloads for AppWorkflows {
    fn payloads<'a>(&'a self, objects: &'a PayloadObjects) -> AppPayloads<'a> {
        AppPayloads { app: self, objects }
    }
}

/// App-scoped payload operations: retained reads and delivered collection.
#[derive(Debug, Clone, Copy)]
pub struct AppPayloads<'a> {
    app: &'a AppWorkflows,
    objects: &'a PayloadObjects,
}
impl AppPayloads<'_> {
    /// Read a completed step from the run's current generation. A step whose
    /// output stayed in the journal answers from the journal.
    ///
    /// # Errors
    /// Rejects invalid names, unavailable steps and object failures.
    pub async fn read_step_output(
        &self,
        run_id: &str,
        name: &str,
        occurrence: u32,
    ) -> Result<PayloadRead, WorkflowServiceError> {
        match self
            .app
            .read_step_output(run_id, name, occurrence, ObjectOpener(self.objects))
            .await?
        {
            StepOutput::Object(read) => Ok(read),
            StepOutput::Inline(value) => {
                let bytes = serde_json::to_vec(&value)
                    .map_err(|_| WorkflowServiceError::Internal("invalid step output".into()))?;
                let reference = WorkflowOutputRef {
                    hash: format!("{:x}", Sha256::digest(&bytes)),
                    size: i64::try_from(bytes.len())
                        .map_err(|_| WorkflowServiceError::PayloadTooLarge)?,
                    content_type: Some("application/json".into()),
                };
                PayloadRead::checked(reference, Box::new(OnceChunk::new(bytes.into())))
            }
        }
    }

    /// Read a retained payload through this app's original policy authority.
    ///
    /// # Errors
    /// Rejects missing history, unavailable policy and failed object reads.
    pub async fn read(
        &self,
        run_id: &str,
        generation: i64,
        slot: PayloadSlot,
    ) -> Result<PayloadRead, WorkflowServiceError> {
        self.app
            .read_payload(run_id, generation, slot, ObjectOpener(self.objects))
            .await
    }

    /// Visit a durable page of abandoned preparations and deletion tombstones.
    ///
    /// # Errors
    /// Refuses foreign or changed jobs, damaged page metadata, refused deletion
    /// and exhausted authority.
    pub async fn collect_job<L: JobLease>(
        &self,
        grant: &L,
        options: CollectionOptions,
    ) -> Result<JobReceipt, WorkflowServiceError> {
        self.app.collect_job(grant, options, self.objects).await
    }
}

/// Resolves a step's stored output to bytes for the creator seam, inside the
/// host memory budget. Admission proves the step owns the payload and hands
/// this the app handle already bound to the call's policy generation.
#[derive(Debug, Clone)]
pub struct ObjectStepOutputs {
    objects: PayloadObjects,
    limit: usize,
}
impl ObjectStepOutputs {
    /// # Errors
    /// Rejects an empty read budget.
    pub fn new(objects: PayloadObjects, limit: usize) -> Result<Self, WorkflowServiceError> {
        if limit == 0 {
            return Err(WorkflowServiceError::InvalidRequest(
                "workflow output read limit must be positive".into(),
            ));
        }
        Ok(Self { objects, limit })
    }
}
#[async_trait(?Send)]
impl StepOutputReader for ObjectStepOutputs {
    async fn read(
        &self,
        api: &AppWorkflows,
        run_id: &str,
        name: &str,
        occurrence: u32,
    ) -> Result<Vec<u8>, WorkflowServiceError> {
        api.payloads(&self.objects)
            .read_step_output(run_id, name, occurrence)
            .await?
            .into_bytes(self.limit)
            .await
    }

    async fn read_output(
        &self,
        api: &AppWorkflows,
        run_id: &str,
    ) -> Result<Vec<u8>, WorkflowServiceError> {
        api.read_output(run_id, ObjectOpener(&self.objects))
            .await?
            .into_bytes(self.limit)
            .await
    }
}

struct AuthorizedSource {
    inner: Option<BoxByteStream>,
    authority: Arc<PolicyAuthority>,
}

#[async_trait(?Send)]
impl ChunkSource for AuthorizedSource {
    async fn next_chunk(&mut self) -> Option<ChunkResult> {
        let inner = self.inner.as_mut()?;
        let read = self
            .authority
            .run(async { Ok(inner.next_chunk().await) })
            .await;
        if let Ok(chunk) = read {
            if !matches!(chunk, Some(Ok(_))) {
                self.inner = None;
            }
            chunk
        } else {
            self.inner = None;
            Some(Err(StorageError::Stream(
                "workflow payload authority is unavailable".into(),
            )))
        }
    }
}

struct VerifiedSource {
    inner: BoxChunkSource,
    expected: WorkflowOutputRef,
    bytes: u64,
    hash: Sha256,
    verified: Rc<Cell<bool>>,
    finished: bool,
}
#[async_trait(?Send)]
impl ChunkSource for VerifiedSource {
    async fn next_chunk(&mut self) -> Option<ChunkResult> {
        if self.finished {
            return None;
        }
        match self.inner.next_chunk().await {
            Some(Ok(chunk)) => {
                if self
                    .bytes
                    .checked_add(chunk.len() as u64)
                    .is_none_or(|size| size > self.expected.size as u64)
                {
                    self.finished = true;
                    return Some(Err(StorageError::InvalidArgument(
                        "workflow payload size mismatch".into(),
                    )));
                }
                self.bytes += chunk.len() as u64;
                self.hash.update(&chunk);
                Some(Ok(chunk))
            }
            Some(Err(error)) => {
                self.finished = true;
                Some(Err(error))
            }
            None => {
                self.finished = true;
                let hash = format!("{:x}", self.hash.clone().finalize());
                if self.bytes != self.expected.size as u64 || hash != self.expected.hash {
                    Some(Err(StorageError::InvalidArgument(
                        "workflow payload digest mismatch".into(),
                    )))
                } else {
                    self.verified.set(true);
                    None
                }
            }
        }
    }
}
