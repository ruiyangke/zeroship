//! This service's payload object store, and the byte capabilities the journal
//! sweeps take as arguments.
//!
//! Blob storage is how the storage layer keeps large objects out of the
//! database, so the process that owns the journal is the one that writes them.
//! The engine holds no store: a writer and a deleter arrive as arguments, and
//! this is where this host supplies them.
//!
//! The namespace is not spelled here. `PAYLOAD_NAMESPACE`
//! (`crates/zeroship-workflow/src/service/payloads.rs`) is the one declaration
//! of it, because a host that reads an object back has to bind the same name as
//! the host that wrote it.

use std::time::Duration;

use async_trait::async_trait;
use zeroship_core::app_id::AppId;
use zeroship_storage::{
    backend::OnceChunk, Namespace, Storage, StorageBackendConfig, StorageError, StorageStore,
};
use zeroship_workflow::{
    engine::WorkflowOutputRef,
    service::{
        input_object, AppWorkflows, PayloadDeleter, PayloadTarget, PayloadWriter, RequestId,
        PAYLOAD_NAMESPACE,
    },
    InputStager, WorkflowServiceError,
};

/// The payload store, under credentials private to this service.
///
/// Creator code reaches its own buckets through its own storage binding and
/// never this one: the namespace is a platform one, and the process holding
/// these credentials runs no creator code.
#[derive(Clone, Debug)]
pub struct ServicePayloads(Storage);

impl ServicePayloads {
    /// Open the configured object store and bind the payload namespace.
    ///
    /// # Errors
    /// Reports a location this host cannot open and a store that refuses the
    /// platform namespace.
    pub fn open(config: &StorageBackendConfig) -> Result<Self, WorkflowServiceError> {
        let store = StorageStore::open(config).map_err(|error| {
            WorkflowServiceError::Unavailable(format!(
                "workflow payload storage is unavailable: {error}"
            ))
        })?;
        Ok(Self(store.namespace(
            Namespace::platform(PAYLOAD_NAMESPACE).map_err(storage_error)?,
        )))
    }
}

/// The object a run started by the cron sweep begins from.
///
/// A generation row keeps no inline slot for a run's input, so the value the
/// schedule carries becomes an object before the run that names it exists. The
/// row lands ownerless: the generation takes the edge that owns it in the same
/// transaction that admits the run, and a start that never commits leaves an
/// object collection reclaims.
#[async_trait(?Send)]
impl InputStager for ServicePayloads {
    async fn stage_input(
        &self,
        api: &AppWorkflows,
        request: &RequestId,
        input: &serde_json::Value,
    ) -> Result<WorkflowOutputRef, WorkflowServiceError> {
        let (bytes, reference) = input_object(input)?;
        api.stage_input(
            request,
            reference.clone(),
            InputWriter {
                objects: self,
                bytes,
            },
        )
        .await?;
        Ok(reference)
    }
}

/// Writes a staged run input, whose bytes this process serialized itself.
///
/// It verifies the length the store reports against the descriptor and nothing
/// more. There is no stream to verify: the digest in the descriptor was taken
/// over these bytes one call earlier, so re-hashing them would compare a value
/// against itself. A host writing a body it did not produce owes the verified
/// stream instead, which is why the upload path is not this.
struct InputWriter<'a> {
    objects: &'a ServicePayloads,
    bytes: Vec<u8>,
}

#[async_trait(?Send)]
impl PayloadWriter for InputWriter<'_> {
    async fn write(
        self,
        target: PayloadTarget<'_>,
        budget: Duration,
    ) -> Result<(), WorkflowServiceError> {
        let expected = u64::try_from(target.reference.size)
            .map_err(|_| WorkflowServiceError::PayloadTooLarge)?;
        let written = compio::time::timeout(
            budget,
            self.objects.0.put_stream(
                target.app.as_str(),
                target.id,
                Box::new(OnceChunk::new(self.bytes.into())),
                target.reference.content_type.as_deref(),
            ),
        )
        .await
        .map_err(|_| WorkflowServiceError::Timeout)?
        .map_err(storage_error)?;
        if written != expected {
            return Err(WorkflowServiceError::Unavailable(
                "workflow payload store did not write the staged input".into(),
            ));
        }
        Ok(())
    }
}

#[async_trait(?Send)]
impl PayloadDeleter for ServicePayloads {
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
