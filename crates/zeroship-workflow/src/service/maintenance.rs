//! Dispatch of the delivered operations that maintain the fold.
//!
//! Every operation reached from here runs no creator code. Each one is a
//! complete sweep in its own module; this module only decides which of them a
//! delivery names, so the decision exists once for every host that owns the
//! journal. Settlement is the delivering transport's, not the engine's: a sweep
//! reports what it committed and its caller acknowledges that.

#![expect(
    clippy::future_not_send,
    reason = "maintenance owns compio-local journal and object operations"
)]

use super::{
    collection::CollectionOptions, delivery::JobReceipt, fanout::FanoutOptions,
    payloads::PayloadDeleter, propagation::PropagationOptions, publication::JobPublisher,
    reconciliation::ReconciliationOptions, AppWorkflows,
};
use crate::{InputStager, WorkflowServiceError};
use zeroship_core::workflow_jobs::{JobLease, JobOperation};

/// The page and batch bounds of the maintenance operations that take one.
#[derive(Debug, Clone, Copy, Default)]
pub struct MaintenanceOptions {
    pub reconciliation: ReconciliationOptions,
    pub collection: CollectionOptions,
    pub fanout: FanoutOptions,
    pub propagation: PropagationOptions,
}

impl MaintenanceOptions {
    /// # Errors
    /// Refuses an invalid page or batch bound on any operation.
    pub fn validate(self) -> Result<(), WorkflowServiceError> {
        self.reconciliation.validate()?;
        self.collection.validate()?;
        self.fanout.validate()?;
        self.propagation.validate()
    }
}

/// What a dispatch decided about the delivery it was given.
#[derive(Debug)]
pub enum MaintenanceOutcome {
    /// Not a maintenance operation. The delivery carries creator work, and its
    /// caller owns accepting and executing it.
    Unclaimed,
    /// The operation committed `receipt`, which its caller settles.
    Settled(Box<JobReceipt>),
    /// A fanout page whose predecessor has not finished. Nothing was committed
    /// and there is no scheduling outcome to acknowledge.
    Deferred,
}

impl AppWorkflows {
    /// Run the maintenance operation this delivery names, if it names one.
    ///
    /// `publisher`, `inputs` and `deleter` are the host capabilities the
    /// reconciliation, cron and collection sweeps respectively need; a delivery
    /// naming any other operation asks nothing of them. The caller bounds this
    /// call and settles what it reports.
    ///
    /// # Errors
    /// Reports whatever the named operation refuses: foreign or changed jobs,
    /// exhausted authority, damaged journal state, unavailable artifacts and
    /// failed journal, publication, staging or deletion operations.
    pub async fn maintenance_job(
        &self,
        lease: &impl JobLease,
        publisher: &impl JobPublisher,
        inputs: &dyn InputStager,
        deleter: &impl PayloadDeleter,
        options: MaintenanceOptions,
    ) -> Result<MaintenanceOutcome, WorkflowServiceError> {
        let settled = |receipt| MaintenanceOutcome::Settled(Box::new(receipt));
        let outcome = match lease.delivery().job.operation {
            JobOperation::Activate { .. } => self.activate_job(lease).await.map(settled),
            JobOperation::Reconcile {} => self
                .reconcile_job(lease, publisher, options.reconciliation)
                .await
                .map(settled),
            JobOperation::Cron { .. } => self.cron_job(lease, inputs).await.map(settled),
            JobOperation::Management { .. } => self.management_job(lease).await.map(settled),
            JobOperation::ReleaseHold { .. } => self.release_hold_job(lease).await.map(settled),
            JobOperation::Collect {} => self
                .collect_job(lease, options.collection, deleter)
                .await
                .map(settled),
            JobOperation::Close { .. } => self.close_job(lease).await.map(settled),
            JobOperation::Fanout { .. } => self
                .fanout_job(lease, options.fanout)
                .await
                .map(|receipt| receipt.map_or(MaintenanceOutcome::Deferred, settled)),
            JobOperation::Propagate { .. } => self
                .propagation_job(lease, options.propagation)
                .await
                .map(settled),
            JobOperation::Advance { .. } => Ok(MaintenanceOutcome::Unclaimed),
        };
        // A committed sweep can leave publication intents -- a propagated
        // parent's advance, a persisted fanout page, a cron run's first
        // frontier or a management application -- and its own caller settles
        // the queue row, not the host's publication. Firing the host wake here
        // covers every arm once, on the lane that owns this journal.
        if matches!(outcome, Ok(MaintenanceOutcome::Settled(_))) {
            self.publication_commit();
        }
        outcome
    }
}
