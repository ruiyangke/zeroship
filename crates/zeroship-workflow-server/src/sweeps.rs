//! This service's own lane over the maintenance rows of the queue it owns.
//!
//! The rows it takes are the ones `maintenance_job` dispatches, which is every
//! operation but the one that executes creator code. A worker keeps taking
//! that one under its placement, and the claim predicate is what keeps the two
//! off each other's rows.
//!
//! The lane holds no payload store. Bytes belong to the process that holds the
//! store, and `workflow_process_dependencies_follow_crate_ownership` forbids
//! this crate both `zeroship-storage` and `zeroship-workflow-runner`, where the
//! only production implementations live. The operations that ask for one are
//! refused by name here rather than skipped.
#![allow(
    clippy::future_not_send,
    reason = "the lane stays on the runtime that opened the journal and the queue"
)]

use std::rc::Rc;

use async_trait::async_trait;
use zeroship_core::{
    app_id::AppId,
    workflow_coordination::WorkerId,
    workflow_jobs::{JobSpec, SettlementReceipt},
};
use zeroship_workflow::{
    backend::InputStager,
    engine::WorkflowOutputRef,
    service::{
        maintenance::{MaintenanceOptions, MaintenanceOutcome},
        publication::JobPublisher,
        AppWorkflows, PayloadDeleter, RequestId,
    },
    WorkflowServiceError,
};
use zeroship_workflow_manager::{
    maintenance::MaintenanceAuthority, policy::PolicySource, Error as ManagerError, Queue,
};

use crate::runs::RunService;

/// What one visit to an app's queue did.
#[derive(Debug)]
pub enum Swept {
    /// The app holds no maintenance row this lane can take.
    Idle,
    /// The operation committed its outcome and the queue recorded it.
    Settled(Box<SettlementReceipt>),
    /// A fanout page whose predecessor has not finished. Nothing was committed,
    /// and there is no outcome to record.
    Deferred,
}

/// Which side of the lane refused.
///
/// The two are kept apart rather than flattened: a queue refusal is about
/// authority or the row's state, and a journal refusal is about the operation.
/// An operator acts on them differently.
#[derive(Debug)]
pub enum SweepError {
    Queue(ManagerError),
    Journal(WorkflowServiceError),
}

/// Claims and runs this service's maintenance rows, one app at a time.
#[derive(Debug)]
pub struct MaintenanceLane {
    queue: Queue,
    runs: Rc<RunService>,
    policies: Rc<dyn PolicySource>,
    identity: WorkerId,
    options: MaintenanceOptions,
}

impl MaintenanceLane {
    /// `identity` names this process on every row the lane leases.
    ///
    /// # Errors
    /// Refuses an invalid page or batch bound on any maintenance operation.
    pub fn new(
        queue: Queue,
        runs: Rc<RunService>,
        policies: Rc<dyn PolicySource>,
        identity: WorkerId,
        options: MaintenanceOptions,
    ) -> Result<Self, WorkflowServiceError> {
        options.validate()?;
        Ok(Self {
            queue,
            runs,
            policies,
            identity,
            options,
        })
    }

    #[must_use]
    pub const fn identity(&self) -> &WorkerId {
        &self.identity
    }

    /// Take one maintenance row of `app`'s queue, run it and record what it
    /// committed.
    ///
    /// # Errors
    /// Reports a refused claim or settlement, an unavailable policy source, a
    /// journal this app may not bind, and whatever the named operation refuses.
    pub async fn sweep(&self, app: &AppId) -> Result<Swept, SweepError> {
        let engine = self
            .runs
            .app(self.policies.as_ref(), app)
            .await
            .map_err(SweepError::Journal)?;
        let authority = MaintenanceAuthority::new(app.clone(), self.identity.clone());
        let ceiling = self
            .policies
            .observe(app)
            .await
            .map(|observed| observed.policy().max_delivery_attempts);
        let Some(grant) = authority
            .claim(&self.queue, ceiling)
            .await
            .map_err(SweepError::Queue)?
        else {
            return Ok(Swept::Idle);
        };
        let publisher = LanePublisher {
            queue: &self.queue,
            app: app.clone(),
        };
        let receipt = match engine
            .maintenance_job(
                &grant,
                &publisher,
                &NoPayloadStore,
                &NoPayloadStore,
                self.options,
            )
            .await
            .map_err(SweepError::Journal)?
        {
            MaintenanceOutcome::Settled(receipt) => *receipt,
            MaintenanceOutcome::Deferred => return Ok(Swept::Deferred),
            // The claim admits exactly the kinds this dispatch has an arm for,
            // so reaching here means the two disagree about one of them.
            MaintenanceOutcome::Unclaimed => {
                return Err(SweepError::Journal(WorkflowServiceError::Internal(
                    "workflow maintenance claimed an operation its dispatch does not run".into(),
                )))
            }
        };
        let settlement = receipt.settlement(&grant).map_err(SweepError::Journal)?;
        authority
            .settle(&self.queue, &settlement)
            .await
            .map(|receipt| Swept::Settled(Box::new(receipt)))
            .map_err(SweepError::Queue)
    }
}

/// Publishes a sweep's successors straight into the queue this process owns.
///
/// `Queue::submit` is the manager-origin path and takes no authorizer, which is
/// what the recovery lane already publishes through. It does not compare the
/// job's app against a caller's, because a manager-origin caller has none, so
/// the scope check the worker path gets from its placement is made here.
struct LanePublisher<'a> {
    queue: &'a Queue,
    app: AppId,
}

impl JobPublisher for LanePublisher<'_> {
    fn app_id(&self) -> &AppId {
        &self.app
    }

    async fn submit(&self, job: &JobSpec) -> Result<JobSpec, WorkflowServiceError> {
        if job.app_id != self.app {
            return Err(WorkflowServiceError::PermissionDenied);
        }
        self.queue.submit(job).await.map_err(|error| {
            WorkflowServiceError::Unavailable(format!(
                "workflow maintenance publication was refused: {error:?}"
            ))
        })
    }
}

/// The payload capabilities `maintenance_job` takes and this service does not
/// hold. An operation that asks for one is told so.
#[derive(Debug)]
struct NoPayloadStore;

fn no_store() -> WorkflowServiceError {
    WorkflowServiceError::Unavailable("workflow maintenance holds no payload store".into())
}

#[async_trait(?Send)]
impl InputStager for NoPayloadStore {
    async fn stage_input(
        &self,
        _api: &AppWorkflows,
        _request: &RequestId,
        _input: &serde_json::Value,
    ) -> Result<WorkflowOutputRef, WorkflowServiceError> {
        Err(no_store())
    }
}

#[async_trait(?Send)]
impl PayloadDeleter for NoPayloadStore {
    async fn delete(&self, _app: &AppId, _id: &str) -> Result<(), WorkflowServiceError> {
        Err(no_store())
    }
}
