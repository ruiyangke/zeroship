//! The local host's normal app deployment, shared by HTTP and workflow execution.

#![expect(clippy::future_not_send, reason = "local deployment I/O uses compio")]

use compio::io::AsyncReadAtExt;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use zeroship_bundle::{BlobStore, LocalDiskBlobStore, Manifest};
use zeroship_core::app_id::AppId;
use zeroship_workflow::{service::BundleExecutable, WorkflowServiceError};

pub struct AppDeployment {
    archive: PathBuf,
    blobs: Arc<dyn BlobStore>,
}
pub struct LoadedApp {
    pub deploy_hash: String,
    pub executable: BundleExecutable,
}
impl AppDeployment {
    pub fn new(root: &Path, archive: &Path) -> Result<Self, String> {
        let archive = std::fs::canonicalize(root.join(archive))
            .map_err(|error| format!("resolve app archive: {error}"))?;
        let blobs = LocalDiskBlobStore::new(root.join(".zeroship/deployments"))
            .map_err(|error| format!("open local app deployments: {error}"))?;
        Ok(Self {
            archive,
            blobs: Arc::new(blobs),
        })
    }

    pub async fn load(
        &self,
        app: &AppId,
        max_archive_bytes: usize,
        max_source_bytes: usize,
    ) -> Result<LoadedApp, WorkflowServiceError> {
        let file = compio::fs::File::open(&self.archive)
            .await
            .map_err(|_| unavailable())?;
        let size = usize::try_from(file.metadata().await.map_err(|_| unavailable())?.len())
            .map_err(|_| WorkflowServiceError::PayloadTooLarge)?;
        if size > max_archive_bytes {
            return Err(WorkflowServiceError::PayloadTooLarge);
        }
        let (result, archive) = file.read_exact_at(vec![0; size], 0).await.into();
        result.map_err(|_| unavailable())?;
        let ingested = zeroship_bundle::ingest(&self.blobs, &app.uuid(), &archive)
            .await
            .map_err(|_| unavailable())?;
        let manifest: Manifest =
            serde_json::from_str(&ingested.manifest_json).map_err(|_| unavailable())?;
        let deploy_hash = manifest.deploy_hash.clone().ok_or_else(unavailable)?;
        let executable =
            BundleExecutable::load(&manifest, self.blobs.as_ref(), max_source_bytes).await?;
        Ok(LoadedApp {
            deploy_hash,
            executable,
        })
    }
}

fn unavailable() -> WorkflowServiceError {
    WorkflowServiceError::Unavailable("app deployment could not be read or validated".into())
}
