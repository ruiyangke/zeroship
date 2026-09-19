//! The delivered manager request to give a deployment's journal hold back.

#![expect(
    clippy::future_not_send,
    reason = "hold release owns compio-local journal and platform operations"
)]

use super::{
    app::{encode, lock_app_state},
    delivery::{self, CapturedLease, JobReceipt},
    deployments::unavailable,
    models::job_receipts,
    store::Transaction,
    AppWorkflows,
};
use crate::WorkflowServiceError;
use zeroship_core::workflow_jobs::{JobLease, JobOperation, JobOutcome, JobSpec};
use zeroship_data_orm::{orm::Entity, value};

impl AppWorkflows {
    /// Close admission for a deployment the app no longer selects, check every
    /// customer dependency under the app lock, and commit the release intent.
    /// The manager publishes this job; a worker cannot, and neither decides the
    /// journal's dependencies on its behalf.
    ///
    /// A journal that still depends on the deployment settles `Waiting`: the
    /// hold stays held and a later release may succeed. Release is never forced.
    /// A deployment this journal never held is already given back, so it settles
    /// `Completed` without contacting the platform.
    ///
    /// # Errors
    /// Refuses foreign or changed jobs, expired authority, an unavailable
    /// deployment client, damaged hold journals and failed platform operations.
    /// Committed receipts replay without repeating the release.
    pub async fn release_hold_job(
        &self,
        lease: &impl JobLease,
    ) -> Result<JobReceipt, WorkflowServiceError> {
        let job = &lease.delivery().job;
        delivery::check_scope(self.app_id(), job)?;
        let JobOperation::ReleaseHold { deployment_id } = &job.operation else {
            return Err(WorkflowServiceError::InvalidRequest(
                "expected workflow hold release job".into(),
            ));
        };
        let authority = CapturedLease::capture(self, lease);
        let budget = delivery::attempt_budget(authority.as_ref().ok(), None);
        delivery::run_attempt(
            authority.as_ref().ok().map(CapturedLease::cancelled),
            budget,
            Box::pin(async {
                let mut tx = self.service.begin_history().await?;
                lock_app_state(&mut tx, self.app_id()).await?;
                if let Some(receipt) = receipt(&tx, job).await? {
                    tx.commit().await?;
                    return Ok(receipt);
                }
                tx.commit().await?;
                let authority = authority?;
                authority.check(self)?;
                let source = self.service.deployments.as_ref().ok_or_else(unavailable)?;
                let client = source.client(self.app_id())?;
                let outcome = match self
                    .service
                    .release_deployment_hold_checked(
                        self.app_id(),
                        deployment_id.as_str(),
                        client.as_ref(),
                        &|| authority.check(self),
                    )
                    .await
                {
                    Ok(_) | Err(WorkflowServiceError::NotFound(_)) => JobOutcome::Completed {},
                    Err(WorkflowServiceError::Conflict(_)) => JobOutcome::Waiting {},
                    Err(error) => return Err(error),
                };
                authority.check(self)?;

                let mut tx = self.service.begin().await?;
                lock_app_state(&mut tx, self.app_id()).await?;
                if let Some(receipt) = receipt(&tx, job).await? {
                    tx.commit().await?;
                    return Ok(receipt);
                }
                authority.check(self)?;
                let now = tx.now().await?;
                tx.database()
                    .collection(job_receipts::Entity::COLLECTION)?
                    .insert(value!({
                        "id":job.id.as_str(), "app_id":self.app_id().as_str(),
                        "specification":encode(job)?, "created_at":now,
                    }))
                    .await?;
                let receipt = delivery::finish(&tx, job, outcome, now).await?;
                authority.check(self)?;
                tx.commit().await?;
                Ok(receipt)
            }),
        )
        .await
    }
}

async fn receipt(
    tx: &Transaction,
    job: &JobSpec,
) -> Result<Option<JobReceipt>, WorkflowServiceError> {
    Ok(delivery::read(tx, job)
        .await?
        .map(|record| record.receipt(job))
        .transpose()?
        .flatten())
}
