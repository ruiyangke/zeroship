//! Claiming this app's frontier the way a worker claims it.
//!
//! The journal has two doors onto a task and they are mutually exclusive by
//! construction. The creator task protocol (`poll`, `heartbeat`, `complete`,
//! `release`) leaves `job_id` null and refuses anything a job owns; the
//! delivered-job protocol (`accept_job`, `heartbeat_job`, `complete_job`,
//! `release_job`) needs a grant and refuses anything it did not deliver. A case
//! picks its door in its SETUP, not per call.
//!
//! The creator door also never PUBLISHES. `publication::record` writes an intent
//! with `confirmed_at` null and only `publish_job` confirms it, so a case on that
//! door watches `pending_jobs` accumulate intents a real host would already have
//! handed to a manager - a shape production never occupies. This module is the
//! other door, so cases can assert the ledger from the state a host puts it in.
//!
//! What follows from that, for a case being moved across: claiming publishes, and
//! publishing confirms, so every job claimed here LEAVES `pending_jobs`. An
//! expectation written on the creator door counts intents this door has already
//! spent.

use super::publication::{Manager, Publisher};
use crate::service::{
    delivery::{DeliveredTask, JobAcceptance},
    AppWorkflows, TaskAssignment, WorkerIdentity,
};
use crate::WorkflowServiceError;
use zeroship_core::{
    app_id::AppId,
    workflow_coordination::{Assignment, WorkerId},
    workflow_jobs::JobSpec,
};
use zeroship_workflow_manager::DeliveryGrant;

/// One worker holding a placement on this app, claiming through its manager.
pub(in crate::service) struct Worker {
    manager: Manager,
    owner: Assignment,
}

impl Worker {
    /// Register a worker against `app` with a placement that outlasts a case.
    pub(in crate::service) async fn new(app: &AppId) -> Self {
        Self {
            manager: Manager::new(app).await,
            owner: Assignment {
                app_id: app.clone(),
                worker_id: WorkerId::mint(),
                revision: 1.try_into().unwrap(),
                expires_at: (chrono::Utc::now().timestamp_millis() + 120_000)
                    .try_into()
                    .unwrap(),
            },
        }
    }

    /// The identity this worker authenticates as, for the assertions about scope.
    pub(in crate::service) fn identity(&self) -> WorkerIdentity {
        WorkerIdentity::new(self.owner.worker_id.as_str().into()).unwrap()
    }

    /// Publish the app's next pending job, claim it, and accept it for execution.
    ///
    /// Panics rather than answering an error, because a case that cannot claim
    /// has not reached its subject.
    pub(in crate::service) async fn claim(&self, scope: &AppWorkflows) -> Claimed {
        let job = self.publish(scope).await;
        let grant = self
            .manager
            .queue
            .claim(&self.owner)
            .await
            .unwrap()
            .expect("the published job is claimable by this worker's placement");
        let task = match scope.accept_job(&grant).await.unwrap() {
            JobAcceptance::Execute(task) => *task,
            other => panic!("expected execution, got {other:?}"),
        };
        Claimed { task, grant, job }
    }

    /// Hand the app's next pending job to the manager without claiming it.
    ///
    /// Asserts the publication is the job the journal named, so a case that goes
    /// on to claim is claiming the frontier it meant to.
    pub(in crate::service) async fn publish(&self, scope: &AppWorkflows) -> JobSpec {
        let job = scope.pending_jobs(None, 1).await.unwrap().remove(0);
        assert_eq!(
            scope
                .publish_job(&job.id, &Publisher::new(&scope.app, self.manager.queue.clone()))
                .await
                .unwrap(),
            job
        );
        job
    }

    /// How many deliveries this worker's manager is holding.
    pub(in crate::service) fn delivered(&self) -> i64 {
        self.manager.count()
    }
}

/// A claimed task and the grant that authorizes acting on it.
pub(in crate::service) struct Claimed {
    pub(in crate::service) task: DeliveredTask,
    pub(in crate::service) grant: DeliveryGrant,
    pub(in crate::service) job: JobSpec,
}

impl Claimed {
    /// The assignment app code is given, for the fields a case reads off it.
    pub(in crate::service) fn assignment(&self) -> &TaskAssignment {
        self.task.assignment()
    }

    /// Renew the lease, answering the control the journal has recorded.
    pub(in crate::service) async fn renew(
        &self,
        scope: &AppWorkflows,
    ) -> Result<crate::service::delivery::TaskRenewal, WorkflowServiceError> {
        scope.heartbeat_job(&self.task, &self.grant).await
    }

    /// Commit this attempt's frontier.
    pub(in crate::service) async fn finish(
        &self,
        scope: &AppWorkflows,
        execution: crate::WorkflowExecution,
    ) -> Result<crate::service::delivery::JobReceipt, WorkflowServiceError> {
        scope.complete_job(&self.task, &self.grant, execution).await
    }

    /// Give the claim back un-advanced, leaving the frontier retryable.
    pub(in crate::service) async fn give_back(
        &self,
        scope: &AppWorkflows,
    ) -> Result<(), WorkflowServiceError> {
        scope.release_job(&self.task, &self.grant).await
    }
}
