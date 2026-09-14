//! The local host's normal app deployment, shared by HTTP and workflow execution.

#![expect(clippy::future_not_send, reason = "local deployment I/O uses compio")]

use compio::io::AsyncReadAtExt;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use zeroship_bundle::{verify_deployment_manifest, BlobStore, LocalDiskBlobStore};
use zeroship_core::app_id::AppId;
use zeroship_workflow::{
    service::{AppDeployments, BundleExecutable},
    WorkflowServiceError,
};

pub struct AppDeployment {
    archive: Option<PathBuf>,
    platform: PathBuf,
    blobs: Arc<dyn BlobStore>,
}

/// A verified app archive whose manifest and blobs are in the local store.
/// The platform catalog assigns its deployment identity separately.
pub struct IngestedApp {
    pub hash: String,
    pub manifest: String,
    pub executable: BundleExecutable,
}

impl AppDeployment {
    pub fn new(root: &Path, archive: Option<&Path>) -> Result<Self, String> {
        let archive = archive
            .map(|archive| std::fs::canonicalize(root.join(archive)))
            .transpose()
            .map_err(|error| format!("resolve app archive: {error}"))?;
        let state = root.join(".zeroship");
        let blobs = LocalDiskBlobStore::new(state.join("deployments"))
            .map_err(|error| format!("open local app deployments: {error}"))?;
        Ok(Self {
            archive,
            platform: state.join("platform/metadata.sqlite"),
            blobs: Arc::new(blobs),
        })
    }

    /// The local platform metadata file: the normal deployment catalog and
    /// the workflow manager's queue, placement, scheduling and recovery.
    pub fn platform(&self) -> &Path {
        &self.platform
    }

    pub fn artifacts(
        &self,
        max_source_bytes: usize,
    ) -> Result<AppDeployments, WorkflowServiceError> {
        AppDeployments::new(self.blobs.clone(), max_source_bytes)
    }

    /// Ingest and verify the served archive, if any, into the retained store.
    ///
    /// # Errors
    /// Refuses unreadable, oversized and invalid archives.
    pub async fn load(
        &self,
        app: &AppId,
        max_archive_bytes: usize,
        max_source_bytes: usize,
    ) -> Result<Option<IngestedApp>, WorkflowServiceError> {
        let Some(archive) = &self.archive else {
            return Ok(None);
        };
        let file = compio::fs::File::open(archive)
            .await
            .map_err(|_| unavailable())?;
        let size = usize::try_from(file.metadata().await.map_err(|_| unavailable())?.len())
            .map_err(|_| WorkflowServiceError::PayloadTooLarge)?;
        if size > max_archive_bytes {
            return Err(WorkflowServiceError::PayloadTooLarge);
        }
        let (result, archive) = file.read_exact_at(vec![0; size], 0).await.into();
        result.map_err(|_| unavailable())?;
        let ingested = zeroship_bundle::ingest(&self.blobs, app, &archive)
            .await
            .map_err(|_| unavailable())?;
        let manifest =
            verify_deployment_manifest(ingested.manifest_json.as_bytes(), &ingested.deploy_hash)
                .map_err(|_| unavailable())?;
        let executable =
            BundleExecutable::load(&manifest, self.blobs.as_ref(), max_source_bytes).await?;
        Ok(Some(IngestedApp {
            hash: ingested.deploy_hash,
            manifest: ingested.manifest_json,
            executable,
        }))
    }
}

fn unavailable() -> WorkflowServiceError {
    WorkflowServiceError::Unavailable("app deployment could not be read or validated".into())
}
