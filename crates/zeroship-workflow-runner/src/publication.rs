//! The authenticated manager transport of a workflow host's consumer.

#![expect(
    clippy::future_not_send,
    reason = "the transport runs on the host's compio thread"
)]

use crate::delivery::{Claimed, Completed, JobTransport, Renewed};
use zeroship_workflow::{
    service::delivery::DeliveredTask, WorkflowExecution, WorkflowServiceError,
};
use zeroship_core::{
    workflow_coordination::AssignedScope,
    workflow_jobs::SettlementReceipt,
};
use zeroship_workflow_client::{LeasedJob, WorkerCoordinator};

/// A host that holds no journal has no outbox of its own, so nothing here
/// publishes: the service owns the journal and drains it.
#[derive(Clone)]
pub(crate) struct HostTransport {
    pub(crate) client: WorkerCoordinator,
}

impl JobTransport for HostTransport {
    type Lease = LeasedJob;
    type Journal = ();
    fn scope(
        &self,
        _journal: &Self::Journal,
        _authority: &zeroship_workflow::service::PolicyAuthority,
    ) -> Result<Self::Journal, WorkflowServiceError> {
        Ok(())
    }

    async fn claim(
        &self,
        journal: &Self::Journal,
        scope: &AssignedScope,
    ) -> Result<Option<Claimed<LeasedJob>>, WorkflowServiceError> {
        JobTransport::claim(&self.client, journal, scope).await
    }

    async fn heartbeat(
        &self,
        journal: &Self::Journal,
        lease: &LeasedJob,
        task: &DeliveredTask,
    ) -> Result<Renewed<LeasedJob>, WorkflowServiceError> {
        JobTransport::heartbeat(&self.client, journal, lease, task).await
    }

    async fn settle(
        &self,
        journal: &Self::Journal,
        lease: &LeasedJob,
    ) -> Result<SettlementReceipt, WorkflowServiceError> {
        JobTransport::settle(&self.client, journal, lease).await
    }

    async fn release(
        &self,
        journal: &Self::Journal,
        lease: &LeasedJob,
        task: &DeliveredTask,
    ) -> Result<(), WorkflowServiceError> {
        JobTransport::release(&self.client, journal, lease, task).await
    }

    async fn receipt(
        &self,
        journal: &Self::Journal,
        job: &zeroship_core::workflow_jobs::JobSpec,
    ) -> Result<Option<zeroship_workflow::service::delivery::JobReceipt>, WorkflowServiceError> {
        JobTransport::receipt(&self.client, journal, job).await
    }

    async fn complete(
        &self,
        journal: &Self::Journal,
        lease: &LeasedJob,
        task: &DeliveredTask,
        execution: WorkflowExecution,
        confirmed: Vec<zeroship_workflow::service::delivery::PayloadConfirmation>,
    ) -> Result<Completed, WorkflowServiceError> {
        JobTransport::complete(&self.client, journal, lease, task, execution, confirmed).await
    }
}
