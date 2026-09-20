//! Manager-delivered deployment readiness in the creator journal.

#![expect(
    clippy::future_not_send,
    reason = "activation owns compio-local journal and artifact operations"
)]

use super::{
    app::{encode, lock_app_state},
    delivery::{self, CapturedLease, JobReceipt},
    deployment_retention::admission_generation,
    deployments::unavailable,
    deploys,
    models::{activation_scopes, activations, deploys as deployment_rows, job_receipts},
    store::Transaction,
    AppWorkflows, DeployRegistration,
};
use crate::WorkflowServiceError;
use zeroship_core::{
    app_id::AppId,
    workflow_jobs::{DeploymentId, JobLease, JobOperation, JobOutcome, JobSpec},
};
use zeroship_data_orm::{
    orm::{Entity, FindOptions, FromRow, Operation, Output},
    value,
};

#[derive(FromRow)]
#[orm(entity = activations)]
struct Activation {
    id: String,
    deploy_id: String,
}

#[derive(FromRow)]
#[orm(entity = activation_scopes)]
struct Selection {
    activation_id: String,
    revision: i64,
}

impl AppWorkflows {
    /// Verify a manager-selected app deployment and durably record readiness.
    /// Older activations can become ready without replacing newer selection.
    /// This operation runs no creator code and advances no calendar.
    ///
    /// # Errors
    /// Refuses foreign or expired authority, conflicting activation identities,
    /// unavailable artifacts and failed journal operations. Committed receipts
    /// replay without loading or retaining their artifact again.
    pub async fn activate_job(
        &self,
        lease: &impl JobLease,
    ) -> Result<JobReceipt, WorkflowServiceError> {
        let job = &lease.delivery().job;
        delivery::check_scope(self.app_id(), job)?;
        let JobOperation::Activate {
            deployment_id,
            revision,
        } = &job.operation
        else {
            return Err(WorkflowServiceError::InvalidRequest(
                "expected workflow activation job".into(),
            ));
        };
        let authority = CapturedLease::capture(self, lease);
        let budget = delivery::attempt_budget(authority.as_ref().ok(), None);
        delivery::run_attempt(
            authority.as_ref().ok().map(CapturedLease::cancelled),
            budget,
            Box::pin(async {
                let mut tx = self.service.begin().await?;
                lock_app_state(&mut tx, self.app_id()).await?;
                if let Some(receipt) = receipt(&tx, job).await? {
                    tx.commit().await?;
                    return Ok(receipt);
                }
                let authority = authority?;
                authority.check(self)?;
                require_fresh(&tx, job, deployment_id, revision.get()).await?;
                authority.check(self)?;
                tx.commit().await?;
                authority.check(self)?;

                let source = self.service.deployments.as_ref().ok_or_else(unavailable)?;
                let client = source.client(self.app_id())?;
                let held = self
                    .service
                    .acquire_deployment_hold_checked(
                        self.app_id(),
                        deployment_id.as_str(),
                        None,
                        client.as_ref(),
                        &|| authority.check(self),
                    )
                    .await?;
                authority.check(self)?;
                let executable = source.read(self.app_id(), &held.deploy_hash).await?;
                authority.check(self)?;
                let deploy = executable
                    .registration(deployment_id.as_str().to_owned(), held.deploy_hash.clone());

                let mut tx = self.service.begin().await?;
                let lock = lock_app_state(&mut tx, self.app_id()).await?;
                if let Some(receipt) = receipt(&tx, job).await? {
                    tx.commit().await?;
                    return Ok(receipt);
                }
                authority.check(self)?;
                let policy = authority.policy();
                require_fresh(&tx, job, deployment_id, revision.get()).await?;
                if admission_generation(
                    &tx,
                    self.app_id(),
                    &deploy.id,
                    &deploy.hash,
                    client.scope(),
                )
                .await?
                    != held.generation.get()
                {
                    return Err(conflict());
                }
                let now = tx.now().await?;
                super::schedules::validate_deployment(&deploy, policy, now)?;
                deploys::record_verified(&tx, lock, &deploy, now).await?;
                tx.database()
                    .collection(job_receipts::Entity::COLLECTION)?
                    .insert(value!({
                        "id":job.id.as_str(), "app_id":self.app_id().as_str(),
                        "specification":encode(job)?, "created_at":now,
                    }))
                    .await?;
                tx.database()
                    .collection(activations::Entity::COLLECTION)?
                    .insert(value!({
                        "id":job.id.as_str(), "app_id":self.app_id().as_str(),
                        "deploy_id":deploy.id, "revision":revision.get(),
                    }))
                    .await?;
                select(&tx, job, revision.get(), &deploy).await?;
                let receipt = delivery::finish(&tx, job, JobOutcome::Completed {}, now).await?;
                authority.check(self)?;
                tx.commit().await?;
                Ok(receipt)
            }),
        )
        .await
    }
}

pub(super) async fn receipt(
    tx: &Transaction,
    job: &JobSpec,
) -> Result<Option<JobReceipt>, WorkflowServiceError> {
    let Some(record) = delivery::read(tx, job).await? else {
        return Ok(None);
    };
    let receipt = record.receipt(job)?.ok_or_else(invalid)?;
    if receipt.outcome != (JobOutcome::Completed {}) {
        return Err(invalid());
    }
    let JobOperation::Activate {
        deployment_id,
        revision,
    } = &job.operation
    else {
        return Err(invalid());
    };
    let readiness = activation(tx, &job.app_id, revision.get())
        .await?
        .ok_or_else(invalid)?;
    if readiness.id != job.id.as_str() || readiness.deploy_id != deployment_id.as_str() {
        return Err(invalid());
    }
    Ok(Some(receipt))
}

async fn activation(
    tx: &Transaction,
    app: &AppId,
    revision: i64,
) -> Result<Option<Activation>, WorkflowServiceError> {
    Ok(tx
        .database()
        .entity::<activations::Entity>()?
        .find::<Activation>(
            activations::app_id
                .eq(app.as_str())?
                .and(activations::revision.eq(revision)?),
            FindOptions {
                limit: Some(1),
                ..Default::default()
            },
        )
        .await?
        .into_iter()
        .next())
}

async fn require_fresh(
    tx: &Transaction,
    job: &JobSpec,
    deployment_id: &DeploymentId,
    revision: i64,
) -> Result<(), WorkflowServiceError> {
    if let Some(existing) = activation(tx, &job.app_id, revision).await? {
        if existing.id != job.id.as_str() || existing.deploy_id != deployment_id.as_str() {
            return Err(conflict());
        }
        // Readiness and its completed receipt commit together.
        return Err(invalid());
    }
    Ok(())
}

async fn selection(
    tx: &Transaction,
    app: &AppId,
) -> Result<Option<Selection>, WorkflowServiceError> {
    let selected = tx
        .database()
        .entity::<activation_scopes::Entity>()?
        .find::<Selection>(
            activation_scopes::id.eq(app.as_str())?,
            FindOptions {
                limit: Some(1),
                ..Default::default()
            },
        )
        .await?
        .into_iter()
        .next();
    if let Some(selected) = &selected {
        if selected.revision <= 0 {
            return Err(invalid());
        }
        let existing = activation(tx, app, selected.revision)
            .await?
            .ok_or_else(invalid)?;
        if existing.id != selected.activation_id {
            return Err(invalid());
        }
    }
    Ok(selected)
}

async fn select(
    tx: &Transaction,
    job: &JobSpec,
    revision: i64,
    deploy: &DeployRegistration,
) -> Result<(), WorkflowServiceError> {
    let previous = selection(tx, &job.app_id).await?;
    if previous
        .as_ref()
        .is_some_and(|previous| previous.revision >= revision)
    {
        return Ok(());
    }
    let deployments = tx
        .database()
        .collection(deployment_rows::Entity::COLLECTION)?;
    deployments
        .execute(Operation::Update {
            filter: value!({"app_id":job.app_id.as_str()}),
            patch: value!({"active":0}),
            many: true,
        })
        .await?;
    deployments
        .update(
            value!({"app_id":job.app_id.as_str(), "id":deploy.id}),
            value!({"active":1}),
        )
        .await?;
    let scopes = tx
        .database()
        .collection(activation_scopes::Entity::COLLECTION)?;
    if let Some(previous) = previous {
        let changed = scopes
            .execute(Operation::Update {
                filter: value!({"id":job.app_id.as_str(), "revision":previous.revision,
                "activation_id":previous.activation_id}),
                patch: value!({"revision":revision, "activation_id":job.id.as_str()}),
                many: true,
            })
            .await?;
        if !matches!(changed, Output::Count(1)) {
            return Err(conflict());
        }
    } else {
        scopes
            .insert(value!({"id":job.app_id.as_str(), "revision":revision,
            "activation_id":job.id.as_str()}))
            .await?;
    }
    Ok(())
}

pub(super) async fn require_local_selection(
    tx: &Transaction,
    app: &AppId,
) -> Result<(), WorkflowServiceError> {
    if selection(tx, app).await?.is_some() {
        return Err(WorkflowServiceError::Conflict(
            "workflow deployment selection belongs to the manager".into(),
        ));
    }
    Ok(())
}

fn conflict() -> WorkflowServiceError {
    WorkflowServiceError::Conflict("workflow activation identity changed".into())
}
fn invalid() -> WorkflowServiceError {
    WorkflowServiceError::Internal("invalid workflow activation journal".into())
}
