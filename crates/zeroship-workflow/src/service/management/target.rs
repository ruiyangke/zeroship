use super::{
    conflict, AppStateLock, AppWorkflows, CapturedLease, Command, ManagementCommand, Transaction,
    WorkflowServiceError,
};
use crate::{
    deployment_holds::{HoldGeneration, HoldScope},
    service::{
        deployment_retention::admission_generation, deployments::unavailable, deploys,
        DeployRegistration,
    },
};
use zeroship_core::workflow_jobs::DeploymentId;

pub(super) struct Verified {
    registration: DeployRegistration,
    generation: HoldGeneration,
    scope: HoldScope,
}

pub(super) async fn load(
    app: &AppWorkflows,
    deployment: &DeploymentId,
    authority: &CapturedLease,
) -> Result<Verified, WorkflowServiceError> {
    authority.check(app)?;
    let source = app.service.deployments.as_ref().ok_or_else(unavailable)?;
    let client = source.client(app.app_id())?;
    let scope = HoldScope::for_app(app.app_id().clone());
    if client.scope().app() != scope.app() || client.scope().holder() != scope.holder() {
        return Err(WorkflowServiceError::PermissionDenied);
    }
    let held = app
        .service
        .acquire_deployment_hold_checked(
            app.app_id(),
            deployment.as_str(),
            None,
            client.as_ref(),
            &|| authority.check(app),
        )
        .await?;
    authority.check(app)?;
    let executable = source.read(app.app_id(), &held.deploy_hash).await?;
    authority.check(app)?;
    Ok(Verified {
        registration: executable.registration(deployment.as_str().to_owned(), held.deploy_hash),
        generation: held.generation,
        scope,
    })
}

impl Verified {
    pub(super) const fn registration(&self) -> &DeployRegistration {
        &self.registration
    }

    pub(super) async fn install(
        &self,
        tx: &Transaction,
        lock: AppStateLock<'_>,
        command: Command<'_>,
        now: i64,
    ) -> Result<(), WorkflowServiceError> {
        let ManagementCommand::RestartLatest { deployment_id } = command.operation else {
            return Err(conflict());
        };
        if deployment_id.as_str() != self.registration.id || self.scope.app() != lock.app() {
            return Err(conflict());
        }
        if admission_generation(
            tx,
            lock.app(),
            &self.registration.id,
            &self.registration.hash,
            &self.scope,
        )
        .await?
            != self.generation.get()
        {
            return Err(conflict());
        }
        deploys::record_verified(tx, lock, &self.registration, now).await
    }
}
