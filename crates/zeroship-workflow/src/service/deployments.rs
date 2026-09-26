//! Host-bound access to normal app manifests, blobs and deployment holds.

#![expect(
    clippy::future_not_send,
    reason = "deployment I/O uses its compio host thread"
)]

use super::{
    app::lock_app, deployment_retention::admission_generation, deploys, tasks::authorized_task,
    BundleExecutable, DeployRegistration, TaskToken, WorkerIdentity, WorkflowService,
};
use crate::{
    deploy_registrations::DeployRegistrationSource,
    deployment_holds::{DeploymentHoldAuthority, DeploymentHoldClient},
    WorkflowServiceError,
};
use std::{rc::Rc, sync::Arc};
use zeroship_bundle::{
    verify_deployment_manifest, BlobError, BlobStore, ExecutableError, LoadedWorker,
};
use zeroship_core::{app_id::AppId, workflow_jobs::DeploymentId};

/// The normal app artifact store a host holds, with the budget it reads under.
#[derive(Clone)]
struct Artifacts {
    source: Arc<dyn BlobStore>,
    max_source_bytes: usize,
}

/// Normal app artifacts and the retention authority of an authenticated host.
/// Customer SQL cannot widen that authority or select another app's manifest
/// scope.
///
/// The two are separate capabilities, and a host may hold the retention
/// authority without the artifact store: retention is a catalog decision about
/// a deployment's identity, while reading a manifest needs the bytes. A host
/// with no store is refused BY NAME at the read rather than treated as a host
/// with no deployments at all, so an operation that needs an artifact says what
/// is missing and one that needs only a hold proceeds.
///
/// A third capability answers what most of those operations actually wanted:
/// the manifest SUMMARY a `deploys` row records. A host holding the artifacts
/// derives it from the bytes; a host holding neither store nor assertion is
/// refused by name, as at the read.
#[derive(Clone)]
pub struct AppDeployments {
    artifacts: Option<Artifacts>,
    registrations: Option<Rc<dyn DeployRegistrationSource>>,
    holds: Rc<dyn DeploymentHoldAuthority>,
}
impl std::fmt::Debug for AppDeployments {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppDeployments")
            .field("artifacts", &self.artifacts.is_some())
            .field("registrations", &self.registrations.is_some())
            .finish_non_exhaustive()
    }
}
impl AppDeployments {
    /// Bind the normal app artifact store, a host source budget and the
    /// retention authority the host was granted.
    ///
    /// # Errors
    /// Rejects an empty source budget.
    pub fn new(
        source: Arc<dyn BlobStore>,
        max_source_bytes: usize,
        holds: Rc<dyn DeploymentHoldAuthority>,
    ) -> Result<Self, WorkflowServiceError> {
        if max_source_bytes == 0 {
            return Err(WorkflowServiceError::InvalidRequest(
                "app source budget must be positive".into(),
            ));
        }
        Ok(Self {
            artifacts: Some(Artifacts {
                source,
                max_source_bytes,
            }),
            registrations: None,
            holds,
        })
    }

    /// Bind the retention authority alone, for a host that holds no app
    /// artifact store.
    #[must_use]
    pub fn holds_only(holds: Rc<dyn DeploymentHoldAuthority>) -> Self {
        Self {
            artifacts: None,
            registrations: None,
            holds,
        }
    }

    /// Add the asserted manifest summary, for a host that holds no artifacts.
    ///
    /// This is not a second route to the bytes: `source` answers with the
    /// declarations alone, and [`Self::read`] still refuses by name. A host that
    /// holds the artifacts derives the same summary from them and takes that arm
    /// instead, so giving one both changes nothing.
    #[must_use]
    pub fn with_registrations(mut self, source: Rc<dyn DeployRegistrationSource>) -> Self {
        self.registrations = Some(source);
        self
    }

    pub(super) fn client(
        &self,
        app: &AppId,
    ) -> Result<Rc<dyn DeploymentHoldClient>, WorkflowServiceError> {
        self.holds.client(app)
    }

    /// Load the manifest and executable of `hash`.
    ///
    /// A host holding no artifact store refuses as a BACKEND failure, which is
    /// deliberately not one of the conditions `damaged` names: a capability this
    /// process was never given says nothing about the deployment, and parking it
    /// would take a live deployment away from the hosts that can read it.
    pub(super) async fn read(
        &self,
        app: &AppId,
        hash: &str,
    ) -> Result<BundleExecutable, ExecutableError> {
        let artifacts = self.artifacts.as_ref().ok_or_else(|| {
            ExecutableError::Storage(BlobError::Backend(
                "this host holds no app deployment artifact store".into(),
            ))
        })?;
        let source = artifacts.source.as_ref();
        let bytes = source
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
        BundleExecutable::load(&manifest, source, artifacts.max_source_bytes).await
    }

    /// The workflow declarations of `hash`, from whichever source this host
    /// holds.
    ///
    /// The artifact arm wins when a host holds both, because the bytes are the
    /// stronger evidence: they are verified against `hash` on the way in, and
    /// the assertion is not. Nothing in the tree holds both.
    ///
    /// Whichever arm answers, the returned `id` and `hash` are the caller's own:
    /// they come from the hold this operation already took, so an assertion
    /// cannot move the deployment a `deploys` row is about. Only `workflows` and
    /// `schedules` come from the source, which is what the row needs and what
    /// the caller has no other way to learn.
    ///
    /// # Errors
    /// Refuses a host holding neither capability by name, an assertion about
    /// another deployment or hash, and whatever the source refuses.
    pub(super) async fn registration(
        &self,
        app: &AppId,
        deployment: &DeploymentId,
        hash: &str,
    ) -> Result<DeployRegistration, WorkflowServiceError> {
        if self.artifacts.is_some() {
            let executable = self.read(app, hash).await?;
            return Ok(executable.registration(deployment.as_str().to_owned(), hash.to_owned()));
        }
        let source = self.registrations.as_ref().ok_or_else(|| {
            WorkflowServiceError::Unavailable(
                "this host holds neither an app deployment artifact store nor an asserted \
                 registration source"
                    .into(),
            )
        })?;
        let registration = source.registration(app, deployment).await?;
        // The hash this host already holds decides which deployment the row is
        // about. An assertion naming another one is a disagreement between
        // Control's catalog and the hold it issued, not a value to record.
        if registration.hash != hash {
            return Err(WorkflowServiceError::Conflict(
                "asserted deployment registration names another deployment hash".into(),
            ));
        }
        Ok(registration)
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
        self.run_bound(|service| {
            Box::pin(async move { service.task_executable_inner(worker, task, token).await })
        })
        .await
    }

    async fn task_executable_inner(
        &self,
        worker: &WorkerIdentity,
        task: &str,
        token: &TaskToken,
    ) -> Result<LoadedWorker, WorkflowServiceError> {
        let source = self.deployments.as_ref().ok_or_else(unavailable)?;
        let mut tx = self.begin().await?;
        let claim = authorized_task(&mut tx, worker, task, token).await?;
        claim.validate_live()?;
        let app = claim.app.clone();
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
