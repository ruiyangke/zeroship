//! Immutable executable images retained in the customer's object storage.

#![expect(
    clippy::future_not_send,
    reason = "snapshot I/O runs on its owning compio thread"
)]

use super::{app::lock_app, tasks::authorized_task, TaskToken, WorkerIdentity, WorkflowService};
use crate::WorkflowServiceError;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, io::Write};
use zeroship_core::app_id::AppId;
use zeroship_storage::{backend::OnceChunk, Namespace, Storage, StorageStore};

/// Complete executable module graph and its deploy-pinned runtime descriptor.
/// Runtime variables and credentials are supplied separately by the trusted host.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExecutableSnapshot {
    format: u16,
    entry: String,
    modules: BTreeMap<String, String>,
    runtime_descriptor: Option<serde_json::Value>,
}
impl std::fmt::Debug for ExecutableSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecutableSnapshot").finish_non_exhaustive()
    }
}
impl ExecutableSnapshot {
    /// Construct an image from the built deployment's complete module graph.
    ///
    /// # Errors
    /// Rejects an absent entry or invalid module names.
    pub fn new(
        entry: String,
        modules: BTreeMap<String, String>,
        runtime_descriptor: Option<serde_json::Value>,
    ) -> Result<Self, WorkflowServiceError> {
        let image = Self {
            format: 1,
            entry,
            modules,
            runtime_descriptor,
        };
        image.validate()?;
        Ok(image)
    }

    /// Entry module evaluated by the runtime host.
    #[must_use]
    pub fn entry(&self) -> &str {
        &self.entry
    }
    /// Built source keyed by its bundle-relative module specifier.
    #[must_use]
    pub const fn modules(&self) -> &BTreeMap<String, String> {
        &self.modules
    }
    /// Runtime schema descriptor retained alongside the executable graph.
    #[must_use]
    pub const fn runtime_descriptor(&self) -> Option<&serde_json::Value> {
        self.runtime_descriptor.as_ref()
    }

    fn validate(&self) -> Result<(), WorkflowServiceError> {
        if self.format != 1
            || !self.modules.contains_key(&self.entry)
            || self.modules.keys().any(|name| {
                name.is_empty()
                    || name.starts_with('/')
                    || name.contains(['\\', '\0', ':'])
                    || name.split('/').any(|part| matches!(part, "" | "." | ".."))
            })
        {
            return Err(WorkflowServiceError::InvalidRequest(
                "invalid workflow executable image".into(),
            ));
        }
        Ok(())
    }
}

/// Customer object storage for retained workflow executables.
#[derive(Clone, Debug)]
pub struct SnapshotStore {
    storage: Storage,
    max_bytes: usize,
}
impl SnapshotStore {
    /// Bind customer storage and the host's encoded-image size limit.
    ///
    /// # Errors
    /// Rejects an empty size limit or invalid storage namespace.
    pub fn new(store: &StorageStore, max_bytes: usize) -> Result<Self, WorkflowServiceError> {
        if max_bytes == 0 {
            return Err(WorkflowServiceError::InvalidRequest(
                "workflow snapshot limit must be positive".into(),
            ));
        }
        Ok(Self {
            storage: store
                .namespace(Namespace::platform("workflow-snapshots").map_err(|_| unavailable())?),
            max_bytes,
        })
    }

    pub(super) fn encode(
        &self,
        image: &ExecutableSnapshot,
    ) -> Result<Vec<u8>, WorkflowServiceError> {
        image.validate()?;
        let mut writer = BoundedBytes {
            bytes: Vec::new(),
            limit: self.max_bytes,
        };
        serde_json::to_writer(&mut writer, image)
            .map_err(|_| WorkflowServiceError::PayloadTooLarge)?;
        Ok(writer.bytes)
    }

    pub(super) async fn put(
        &self,
        app: &AppId,
        deploy: &str,
        bytes: Vec<u8>,
        hash: &str,
    ) -> Result<(), WorkflowServiceError> {
        let size = i64::try_from(bytes.len()).map_err(|_| WorkflowServiceError::PayloadTooLarge)?;
        let written = self
            .storage
            .put_stream(
                app.as_str(),
                deploy,
                Box::new(OnceChunk::new(bytes::Bytes::from(bytes))),
                Some("application/json"),
            )
            .await
            .map_err(|_| unavailable())?;
        if written != u64::try_from(size).map_err(|_| WorkflowServiceError::PayloadTooLarge)? {
            return Err(unavailable());
        }
        self.read(app, deploy, hash, size)
            .await
            .map_err(ReadError::into_service)?;
        Ok(())
    }

    async fn read(
        &self,
        app: &AppId,
        deploy: &str,
        hash: &str,
        size: i64,
    ) -> Result<ExecutableSnapshot, ReadError> {
        let expected = usize::try_from(size)
            .ok()
            .filter(|size| *size <= self.max_bytes)
            .ok_or(ReadError::Service(WorkflowServiceError::PayloadTooLarge))?;
        let Some((meta, mut body)) = self
            .storage
            .get_stream(app.as_str(), deploy)
            .await
            .map_err(|_| ReadError::Service(unavailable()))?
        else {
            return Err(ReadError::Damaged);
        };
        if meta.size != u64::try_from(expected).map_err(|_| ReadError::Damaged)? {
            return Err(ReadError::Damaged);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = body.next_chunk().await {
            let chunk = chunk.map_err(|_| ReadError::Service(unavailable()))?;
            if bytes
                .len()
                .checked_add(chunk.len())
                .is_none_or(|size| size > expected)
            {
                return Err(ReadError::Damaged);
            }
            bytes.extend_from_slice(&chunk);
        }
        if bytes.len() != expected || content_hash(&bytes) != hash {
            return Err(ReadError::Damaged);
        }
        let image: ExecutableSnapshot =
            serde_json::from_slice(&bytes).map_err(|_| ReadError::Damaged)?;
        image.validate().map_err(|_| ReadError::Damaged)?;
        Ok(image)
    }
}

pub(super) fn content_hash(bytes: &[u8]) -> String {
    use sha2::Digest;
    format!("{:x}", sha2::Sha256::digest(bytes))
}

struct BoundedBytes {
    bytes: Vec<u8>,
    limit: usize,
}
impl Write for BoundedBytes {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self
            .bytes
            .len()
            .checked_add(bytes.len())
            .is_none_or(|size| size > self.limit)
        {
            return Err(std::io::Error::other(
                "workflow snapshot exceeds its encoded size limit",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

enum ReadError {
    Damaged,
    Service(WorkflowServiceError),
}
impl ReadError {
    fn into_service(self) -> WorkflowServiceError {
        match self {
            Self::Damaged => WorkflowServiceError::Unavailable(
                "workflow executable snapshot is missing or corrupt".into(),
            ),
            Self::Service(error) => error,
        }
    }
}
fn unavailable() -> WorkflowServiceError {
    WorkflowServiceError::Unavailable("workflow snapshot storage unavailable".into())
}

impl WorkflowService {
    /// Bind the customer's retained executable storage before activating work.
    #[must_use]
    pub fn with_snapshots(mut self, snapshots: SnapshotStore) -> Self {
        self.snapshots = Some(snapshots);
        self
    }

    /// Load the immutable deployment owned by a live execution claim.
    ///
    /// # Errors
    /// Rejects stale or foreign claims, unavailable storage and corrupt images.
    /// Missing or corrupt images park their deployment until the host repairs it.
    pub async fn task_snapshot(
        &self,
        worker: &WorkerIdentity,
        task: &str,
        token: &TaskToken,
    ) -> Result<ExecutableSnapshot, WorkflowServiceError> {
        let store = self.snapshots.as_ref().ok_or_else(unavailable)?;
        let mut tx = self.begin().await?;
        let claim = authorized_task(&mut tx, worker, task, token).await?;
        claim.validate_live()?;
        let app = claim.app;
        let deploy = claim.run.text("deploy_id")?;
        let rows = tx.query(&format!("SELECT snapshot_hash,snapshot_size,snapshot_epoch FROM {} WHERE app_id=$1 AND id=$2 AND state='available'", tx.table("deploys")), &[app.as_str().into(), deploy.clone().into()]).await?;
        let row = rows.first().ok_or_else(unavailable)?;
        let hash = row.text("snapshot_hash")?;
        let size = row.integer("snapshot_size")?;
        let epoch = row.integer("snapshot_epoch")?;
        tx.commit().await?;
        let result = store.read(&app, &deploy, &hash, size).await;
        if matches!(result, Err(ReadError::Damaged)) {
            let mut tx = self.begin().await?;
            lock_app(&mut tx, &app).await?;
            tx.execute(&format!("UPDATE {} SET state='unavailable' WHERE app_id=$1 AND id=$2 AND snapshot_hash=$3 AND snapshot_epoch=$4 AND state='available'", tx.table("deploys")), &[app.as_str().into(), deploy.into(), hash.into(), epoch.into()]).await?;
            tx.commit().await?;
        }
        let image = result.map_err(ReadError::into_service)?;
        let mut tx = self.begin().await?;
        authorized_task(&mut tx, worker, task, token)
            .await?
            .validate_live()?;
        tx.commit().await?;
        Ok(image)
    }
}
