//! Admit pinned deployments through normal app artifacts and durable holds.

#![expect(
    clippy::future_not_send,
    reason = "deployment I/O uses its compio host thread"
)]

use super::{
    app::{decode, encode, lock_app},
    deployment_retention::admission_generation,
    deployments::{damaged, unavailable},
    models::deploys,
    store::Transaction,
    DeployRegistration, WorkflowService,
};
use crate::{validation, WorkflowServiceError};
use zeroship_core::{app_id::AppId, typed_id};
use zeroship_data_orm::{
    orm::{Entity, FindOptions, FromRow, Operation},
    value,
};

#[derive(FromRow)]
#[orm(entity = deploys)]
pub(super) struct Deployment {
    pub hash: String,
    manifest: String,
    state: String,
    pub availability_epoch: i64,
}
impl Deployment {
    pub(super) fn available(&self) -> Result<(), WorkflowServiceError> {
        if self.state != "available" {
            return Err(unavailable());
        }
        Ok(())
    }
    pub(super) fn registration(&self) -> Result<DeployRegistration, WorkflowServiceError> {
        decode(&self.manifest)
    }
    fn check(&self, expected: &DeployRegistration) -> Result<(), WorkflowServiceError> {
        if self.registration()? != *expected
            || self.hash != expected.hash
            || self.availability_epoch < 0
            || !matches!(
                self.state.as_str(),
                "available" | "unavailable" | "retiring"
            )
        {
            return Err(conflict());
        }
        Ok(())
    }
}

pub(super) async fn read(
    tx: &Transaction,
    app: &AppId,
    id: &str,
) -> Result<Option<Deployment>, WorkflowServiceError> {
    Ok(tx
        .database()
        .entity::<deploys::Entity>()?
        .find::<Deployment>(
            deploys::app_id
                .eq(app.as_str().to_owned())?
                .and(deploys::id.eq(id.to_owned())?),
            FindOptions {
                limit: Some(1),
                ..Default::default()
            },
        )
        .await?
        .into_iter()
        .next())
}

impl WorkflowService {
    /// Verify and hold the normal app artifact before selecting it for new work.
    /// The host serializes its desired-deployment updates.
    ///
    /// # Errors
    /// Rejects conflicting registrations, missing hold authority and unavailable artifacts.
    pub async fn activate_deploy(
        &self,
        app: &AppId,
        deploy: &DeployRegistration,
    ) -> Result<(), WorkflowServiceError> {
        let generation = self.prepare_deploy(app, deploy).await?;
        let client = self
            .deployments
            .as_ref()
            .ok_or_else(unavailable)?
            .client(app)?;
        let mut tx = self.begin().await?;
        let policy = lock_app(&mut tx, app).await?;
        super::activation::require_local_selection(&tx, app).await?;
        if admission_generation(&tx, app, &deploy.id, &deploy.hash, client.scope()).await?
            != generation
        {
            return Err(conflict());
        }
        let record = read(&tx, app, &deploy.id).await?.ok_or_else(unavailable)?;
        record.check(deploy)?;
        record.available()?;
        let collection = tx.database().collection(deploys::Entity::COLLECTION)?;
        collection
            .execute(Operation::Update {
                filter: value!({"app_id":app.as_str()}),
                patch: value!({"active":0}),
                many: true,
            })
            .await?;
        collection
            .update(
                value!({"app_id":app.as_str(), "id":deploy.id}),
                value!({"active":1}),
            )
            .await?;
        let now = tx.now().await?;
        super::schedules::validate_deployment(deploy, &policy, now)?;
        tx.commit().await
    }

    /// Verify or repair a held deployment without changing the active deployment.
    /// Artifact publication and repair belong to the normal app deployment host.
    ///
    /// # Errors
    /// Rejects conflicting registrations, missing hold authority and unavailable artifacts.
    pub async fn retain_deploy(
        &self,
        app: &AppId,
        deploy: &DeployRegistration,
    ) -> Result<(), WorkflowServiceError> {
        self.prepare_deploy(app, deploy).await.map(|_| ())
    }

    async fn prepare_deploy(
        &self,
        app: &AppId,
        deploy: &DeployRegistration,
    ) -> Result<i64, WorkflowServiceError> {
        validate(deploy)?;
        let source = self.deployments.as_ref().ok_or_else(unavailable)?;
        let client = source.client(app)?;
        let receipt = self
            .acquire_deployment_hold(app, &deploy.id, &deploy.hash, client.as_ref())
            .await?;
        let mut tx = self.begin().await?;
        lock_app(&mut tx, app).await?;
        let generation =
            admission_generation(&tx, app, &deploy.id, &deploy.hash, client.scope()).await?;
        if generation != receipt.generation.get() {
            return Err(conflict());
        }
        let previous = read(&tx, app, &deploy.id).await?;
        if let Some(record) = &previous {
            record.check(deploy)?;
        }
        tx.commit().await?;
        // Artifact I/O cannot hold the app lock and block execution heartbeats.
        let result = source.read(app, &deploy.hash).await;
        if result.as_ref().is_err_and(damaged) {
            if let Some(previous) = previous {
                self.park_deployment(app, &deploy.id, &deploy.hash, previous.availability_epoch)
                    .await?;
            }
        }
        let executable = result?;
        if executable.registration(deploy.id.clone(), deploy.hash.clone()) != *deploy {
            return Err(conflict());
        }
        let mut tx = self.begin().await?;
        lock_app(&mut tx, app).await?;
        if admission_generation(&tx, app, &deploy.id, &deploy.hash, client.scope()).await?
            != generation
        {
            return Err(conflict());
        }
        let now = tx.now().await?;
        record_verified(&tx, app, deploy, now).await?;
        tx.commit().await?;
        Ok(generation)
    }
}

/// The caller has verified the normal bundle and its current retained generation.
pub(super) async fn record_verified(
    tx: &Transaction,
    app: &AppId,
    deploy: &DeployRegistration,
    now: i64,
) -> Result<(), WorkflowServiceError> {
    validate(deploy)?;
    let collection = tx.database().collection(deploys::Entity::COLLECTION)?;
    if let Some(existing) = read(tx, app, &deploy.id).await? {
        existing.check(deploy)?;
        let epoch = existing.availability_epoch.checked_add(1).ok_or_else(|| {
            WorkflowServiceError::ResourceExhausted(
                "deployment availability epoch exhausted".into(),
            )
        })?;
        collection
            .update(
                value!({"app_id":app.as_str(), "id":deploy.id}),
                value!({"state":"available", "availability_epoch":epoch}),
            )
            .await?;
    } else {
        collection.insert(value!({"app_id":app.as_str(), "id":deploy.id, "hash":deploy.hash,
            "manifest":encode(deploy)?, "created_at":now, "active":0, "state":"available", "availability_epoch":1})).await?;
    }
    Ok(())
}

fn conflict() -> WorkflowServiceError {
    WorkflowServiceError::Conflict("workflow deployment is immutable or being retired".into())
}
fn validate(deploy: &DeployRegistration) -> Result<(), WorkflowServiceError> {
    typed_id::parse_with_prefix(&deploy.id, "dep")
        .map_err(|_| WorkflowServiceError::InvalidRequest("invalid deployment identity".into()))?;
    if !zeroship_bundle::validate_hash_format(&deploy.hash) {
        return Err(WorkflowServiceError::InvalidRequest(
            "invalid deployment hash".into(),
        ));
    }
    for name in &deploy.workflows {
        validation::workflow_name(name)?;
    }
    Ok(())
}
