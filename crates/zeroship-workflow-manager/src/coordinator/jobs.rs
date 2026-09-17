//! Worker delivery binds stored placement and host enrollment inside queue transactions.

use super::Coordinator;
use crate::{DeliveryGrant, Error};
use std::future::Future;
use zeroship_core::{
    workflow_coordination::{AssignedScope, Assignment, VerifyAssignment, WorkerId},
    workflow_jobs::{Delivery, JobOperation, JobSpec, Settlement, SettlementReceipt, SubmitJob},
};
use zeroship_data_orm::orm::Database;

impl Coordinator {
    /// Publish a creator intent while its enrolled worker still owns the app scope.
    /// The host callback revalidates the originally authenticated instance key.
    ///
    /// # Errors
    /// Rejects foreign scopes, manager-origin operations, revoked authority and storage failures.
    pub async fn submit_job<F, Fut>(
        &self,
        worker: &WorkerId,
        request: &SubmitJob,
        authorize: F,
    ) -> Result<JobSpec, Error>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Result<WorkerId, Error>>,
    {
        let scope = selector(worker, &request.scope);
        worker_operation(&request.job.operation)?;
        self.queue
            .submit_authorized(&scope, &request.job, |tx| {
                self.delivery_authority(tx, &scope, &authorize)
            })
            .await
    }

    /// Claim using current placement and enrollment, without accepting a caller expiry.
    ///
    /// The caller supplies the app's delivery ceiling from its own policy
    /// authority: a remote host reads it from the observed policy source, and a
    /// local host is its app's platform authority, exactly as for admission.
    ///
    /// # Errors
    /// Rejects revoked identity or placement, an invalid ceiling and failed
    /// queue transactions.
    pub async fn claim_job<F, Fut>(
        &self,
        worker: &WorkerId,
        scope: &AssignedScope,
        max_delivery_attempts: i64,
        authorize: F,
    ) -> Result<Option<DeliveryGrant>, Error>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Result<WorkerId, Error>>,
    {
        let scope = selector(worker, scope);
        self.queue
            .claim_authorized(&scope, max_delivery_attempts, |tx| {
                self.delivery_authority(tx, &scope, &authorize)
            })
            .await
    }

    /// Renew a live delivery; echoed identity cannot select another worker.
    ///
    /// # Errors
    /// Rejects stale deliveries, revoked authority and failed queue transactions.
    pub async fn heartbeat_job<F, Fut>(
        &self,
        worker: &WorkerId,
        delivery: &Delivery,
        authorize: F,
    ) -> Result<DeliveryGrant, Error>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Result<WorkerId, Error>>,
    {
        let scope = delivery_selector(worker, delivery)?;
        self.queue
            .heartbeat_authorized(&scope, delivery, |tx| {
                self.delivery_authority(tx, &scope, &authorize)
            })
            .await
    }

    /// Settle an active delivery or recover its exact receipt after placement expires.
    /// Receipt replay revalidates the original enrolled signer without renewing
    /// placement or granting fresh successor writes.
    ///
    /// # Errors
    /// Rejects foreign identities, conflicting settlements, unauthorized successors
    /// and unavailable queue or enrollment storage.
    pub async fn settle_job<F, Fut>(
        &self,
        worker: &WorkerId,
        settlement: &Settlement,
        authorize: F,
    ) -> Result<SettlementReceipt, Error>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Result<WorkerId, Error>>,
    {
        let scope = delivery_selector(worker, &settlement.delivery)?;
        for successor in &settlement.successors {
            worker_operation(&successor.operation)?;
        }
        self.queue
            .settle_authorized(
                &scope,
                settlement,
                |tx| self.delivery_authority(tx, &scope, &authorize),
                |_| async { enrolled(worker, &authorize).await },
            )
            .await
    }

    async fn delivery_authority<F, Fut>(
        &self,
        tx: Database,
        scope: &VerifyAssignment,
        authorize: &F,
    ) -> Result<Assignment, Error>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Result<WorkerId, Error>>,
    {
        let assigned = self.verify_assignment_in(&tx, scope).await?;
        enrolled(&scope.worker_id, authorize).await?;
        Ok(assigned)
    }
}

fn selector(worker: &WorkerId, scope: &AssignedScope) -> VerifyAssignment {
    VerifyAssignment {
        app_id: scope.app_id.clone(),
        worker_id: worker.clone(),
        assignment_revision: scope.assignment_revision,
    }
}

fn delivery_selector(worker: &WorkerId, delivery: &Delivery) -> Result<VerifyAssignment, Error> {
    if worker != &delivery.worker_id {
        return Err(Error::Denied);
    }
    Ok(VerifyAssignment {
        app_id: delivery.job.app_id.clone(),
        worker_id: worker.clone(),
        assignment_revision: delivery.assignment_revision,
    })
}

/// Workers publish only creator intents. Activation, calendar, management,
/// closure and maintenance jobs are manager-origin, so a worker can neither
/// forge closure evidence nor postpone closing with self-published duties.
const fn worker_operation(operation: &JobOperation) -> Result<(), Error> {
    match operation {
        JobOperation::Advance { .. } | JobOperation::Fanout { .. } | JobOperation::Propagate { .. } => {
            Ok(())
        }
        // A worker that could publish a release would be asking itself to give
        // code back, so retention stays the manager's decision.
        JobOperation::Activate { .. }
        | JobOperation::Cron { .. }
        | JobOperation::ReleaseHold { .. }
        | JobOperation::Management { .. }
        | JobOperation::Close { .. }
        | JobOperation::Reconcile {}
        | JobOperation::Collect {} => Err(Error::Denied),
    }
}

async fn enrolled<F, Fut>(worker: &WorkerId, authorize: &F) -> Result<WorkerId, Error>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<WorkerId, Error>>,
{
    let current = authorize().await?;
    if &current != worker {
        return Err(Error::Denied);
    }
    Ok(current)
}
