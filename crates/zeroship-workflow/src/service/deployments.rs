//! Host-bound access to normal app manifests, blobs and deployment holds.

#![expect(
    clippy::future_not_send,
    reason = "deployment I/O uses its compio host thread"
)]

use super::{
    app::lock_app, deployment_retention::admission_generation, deploys, tasks::authorized_task,
    BundleExecutable, TaskToken, WorkerIdentity, WorkflowService,
};
use crate::{deployment_holds::DeploymentHoldClient, WorkflowServiceError};
use std::{collections::BTreeMap, rc::Rc, sync::Arc};
use zeroship_bundle::{
    verify_deployment_manifest, BlobError, BlobStore, ExecutableError, LoadedWorker,
};
use zeroship_core::app_id::AppId;

/// Normal app artifacts and retention clients supplied by an authenticated host.
/// Customer SQL cannot install clients or select another app's manifest scope.
#[derive(Clone)]
pub struct AppDeployments {
    source: Arc<dyn BlobStore>,
    max_source_bytes: usize,
    clients: BTreeMap<AppId, Rc<dyn DeploymentHoldClient>>,
}
impl std::fmt::Debug for AppDeployments {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppDeployments").finish_non_exhaustive()
    }
}
impl AppDeployments {
    /// Bind the normal app artifact store and a host source budget.
    ///
    /// # Errors
    /// Rejects an empty source budget.
    pub fn new(
        source: Arc<dyn BlobStore>,
        max_source_bytes: usize,
    ) -> Result<Self, WorkflowServiceError> {
        if max_source_bytes == 0 {
            return Err(WorkflowServiceError::InvalidRequest(
                "app source budget must be positive".into(),
            ));
        }
        Ok(Self {
            source,
            max_source_bytes,
            clients: BTreeMap::new(),
        })
    }

    /// Install retention authority obtained from the host's app assignment.
    #[must_use]
    pub fn with_hold_client(mut self, client: Rc<dyn DeploymentHoldClient>) -> Self {
        self.clients.insert(client.scope().app().clone(), client);
        self
    }

    pub(super) fn client(
        &self,
        app: &AppId,
    ) -> Result<Rc<dyn DeploymentHoldClient>, WorkflowServiceError> {
        self.clients
            .get(app)
            .cloned()
            .ok_or(WorkflowServiceError::PermissionDenied)
    }

    pub(super) async fn read(
        &self,
        app: &AppId,
        hash: &str,
    ) -> Result<BundleExecutable, ExecutableError> {
        let bytes = self
            .source
            .get_manifest(app, hash)
            .await
            .map_err(|error| match error {
                BlobError::TooLarge => ExecutableError::InvalidManifest,
                other => ExecutableError::Storage(other),
            })?;
        if bytes.len() as u64 > zeroship_bundle::MAX_MANIFEST_BYTES {
            return Err(ExecutableError::InvalidManifest);
        }
        let manifest = verify_deployment_manifest(&bytes, hash)?;
        BundleExecutable::load(&manifest, self.source.as_ref(), self.max_source_bytes).await
    }
}

pub(super) const fn damaged(error: &ExecutableError) -> bool {
    matches!(
        error,
        ExecutableError::InvalidManifest
            | ExecutableError::ManifestIdentity
            | ExecutableError::InvalidExecutable
            | ExecutableError::Storage(BlobError::NotFound(_) | BlobError::HashMismatch { .. })
    )
}
impl From<ExecutableError> for WorkflowServiceError {
    fn from(error: ExecutableError) -> Self {
        match error {
            ExecutableError::TooLarge => Self::PayloadTooLarge,
            _ => unavailable(),
        }
    }
}
pub(super) fn unavailable() -> WorkflowServiceError {
    WorkflowServiceError::Unavailable("app deployment is unavailable".into())
}

impl WorkflowService {
    /// Bind normal app artifacts and their host-authenticated retention clients.
    #[must_use]
    pub fn with_deployments(mut self, deployments: AppDeployments) -> Self {
        self.deployments = Some(deployments);
        self
    }

    /// Load the pinned normal app deployment of a live execution claim.
    /// Missing or corrupt artifacts park that deployment until host repair.
    ///
    /// # Errors
    /// Rejects stale or foreign claims, missing holds and unavailable artifacts.
    pub async fn task_executable(
        &self,
        worker: &WorkerIdentity,
        task: &str,
        token: &TaskToken,
    ) -> Result<LoadedWorker, WorkflowServiceError> {
        let source = self.deployments.as_ref().ok_or_else(unavailable)?;
        let mut tx = self.begin().await?;
        let claim = authorized_task(&mut tx, worker, task, token).await?;
        claim.validate_live()?;
        let app = claim.app;
        let client = source.client(&app)?;
        let id = claim.run.text("deploy_id")?;
        let record = deploys::read(&tx, &app, &id)
            .await?
            .ok_or_else(unavailable)?;
        record.available()?;
        let generation = admission_generation(&tx, &app, &id, &record.hash, client.scope()).await?;
        tx.commit().await?;
        let result = source.read(&app, &record.hash).await;
        if result.as_ref().is_err_and(damaged) {
            self.park_deployment(&app, &id, &record.hash, record.availability_epoch)
                .await?;
        }
        let executable = result?;
        if executable.registration(id.clone(), record.hash.clone()) != record.registration()? {
            return Err(unavailable());
        }
        let mut tx = self.begin().await?;
        authorized_task(&mut tx, worker, task, token)
            .await?
            .validate_live()?;
        let current = deploys::read(&tx, &app, &id)
            .await?
            .ok_or_else(unavailable)?;
        current.available()?;
        if current.hash != record.hash
            || admission_generation(&tx, &app, &id, &record.hash, client.scope()).await?
                != generation
        {
            return Err(unavailable());
        }
        tx.commit().await?;
        Ok(executable.into_executable())
    }

    pub(super) async fn park_deployment(
        &self,
        app: &AppId,
        id: &str,
        hash: &str,
        epoch: i64,
    ) -> Result<(), WorkflowServiceError> {
        use zeroship_data_orm::{
            orm::{Entity, Operation},
            value,
        };
        let mut tx = self.begin().await?;
        lock_app(&mut tx, app).await?;
        tx.database().collection(super::models::deploys::Entity::COLLECTION)?
            .execute(Operation::Update {
                filter: value!({"app_id":app.as_str(), "id":id, "hash":hash, "availability_epoch":epoch, "state":"available"}),
                patch: value!({"state":"unavailable"}), many:true,
            }).await?;
        tx.commit().await
    }
}
