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
    deployment_holds::DeploymentHolds,
    service::{BundleExecutable, DeployRegistration},
    WorkflowServiceError,
};

pub struct AppDeployment {
    archive: Option<PathBuf>,
    index: PathBuf,
    blobs: Arc<dyn BlobStore>,
}
pub struct LoadedApp {
    pub registration: DeployRegistration,
    pub executable: BundleExecutable,
}
impl AppDeployment {
    pub fn new(root: &Path, archive: Option<&Path>) -> Result<Self, String> {
        let archive = archive
            .map(|archive| std::fs::canonicalize(root.join(archive)))
            .transpose()
            .map_err(|error| format!("resolve app archive: {error}"))?;
        let directory = root.join(".zeroship/deployments");
        let blobs = LocalDiskBlobStore::new(directory.clone())
            .map_err(|error| format!("open local app deployments: {error}"))?;
        Ok(Self {
            archive,
            index: directory.join("index.sqlite"),
            blobs: Arc::new(blobs),
        })
    }

    pub async fn catalog(&self) -> Result<DeploymentHolds, WorkflowServiceError> {
        DeploymentHolds::open_local(&self.index).await
    }

    pub async fn load(
        &self,
        app: &AppId,
        catalog: &DeploymentHolds,
        max_archive_bytes: usize,
        max_source_bytes: usize,
    ) -> Result<Option<LoadedApp>, WorkflowServiceError> {
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
        let ingested = zeroship_bundle::ingest(&self.blobs, &app.uuid(), &archive)
            .await
            .map_err(|_| unavailable())?;
        let deploy_hash = ingested.deploy_hash;
        let manifest = verify_deployment_manifest(ingested.manifest_json.as_bytes(), &deploy_hash)
            .map_err(|_| unavailable())?;
        let executable =
            BundleExecutable::load(&manifest, self.blobs.as_ref(), max_source_bytes).await?;
        let id = catalog
            .record_deployment(app, &deploy_hash, &ingested.manifest_json)
            .await?;
        Ok(Some(LoadedApp {
            registration: executable.registration(id, deploy_hash),
            executable,
        }))
    }
}

fn unavailable() -> WorkflowServiceError {
    WorkflowServiceError::Unavailable("app deployment could not be read or validated".into())
}
