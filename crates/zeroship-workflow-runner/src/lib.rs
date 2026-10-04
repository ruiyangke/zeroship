//! Workflow execution, separated from admission.
//!
//! `zeroship-workflow` admits work: it validates, authorises, reads the active
//! deploy and records intent. Nothing in it executes a workflow. This crate
//! holds the half that does, so a platform process can serve the creator
//! methods by depending on admission alone - which
//! `xtask/tests/workflow/mod.rs` requires of the manager and server.
//!
//! The execution half adds methods to `AppWorkflows` and `WorkflowService`,
//! which are defined in the admission crate. An inherent `impl` on a foreign
//! type does not compile, so those arrive as extension traits implemented
//! here: the trait is local, which is what the orphan rule asks.
//!
//! The crate root owns execution slots and the delivered-job consumption loop;
//! the modules beside it own assignment, delivery, publication and readiness.

pub mod assignments;
mod budget;
pub mod consumer;
pub mod delivery;
pub mod host;
pub mod journal_duties;
pub mod publication;
pub mod ready;
pub mod remote;
pub mod remote_tasks;
pub use remote_tasks::RemoteTasks;
pub use budget::{BudgetEnd, ExecutionBudget, ExecutionGuard};
mod payloads;
pub use payloads::{
    AppPayloads, HostPayloads, ObjectStepOutputs, PayloadObjects, PayloadRead, RunPayloads,
    TaskPayloadReader, TaskPayloads, UploadReceipt, WorkerPayloads, CHILD_OUTPUT_READS,
};
mod outputs;
pub use outputs::{PreparedExecution, TaskPayloadLimits};

#[cfg(test)]
pub use zeroship_workflow_fixtures::deployment as deployment_fixture;
#[cfg(test)]
pub use zeroship_workflow_fixtures::journal as journal_fixture;
#[cfg(test)]
pub use zeroship_workflow_fixtures::manager_queue;
#[cfg(test)]
pub use zeroship_workflow_fixtures::service_binding;
#[cfg(test)]
use zeroship_testkit::s3 as s3_fixture;

use async_trait::async_trait;
use zeroship_workflow::{
    service::{
        delivery::PayloadConfirmation, AppWorkflows, TaskAssignment, WorkerIdentity,
        WorkflowService,
    },
    WorkflowExecution, WorkflowServiceError,
};

/// Task operations bound to the worker identity supplied by the trusted host.
/// The CLI acts as a local worker and uses this same handle.
#[derive(Debug, Clone)]
pub struct WorkerTasks {
    service: WorkflowService,
    worker: WorkerIdentity,
    objects: PayloadObjects,
}
impl WorkerTasks {
    /// Read the deployment retained for this task's live execution claim.
    ///
    /// # Errors
    /// Rejects stale claims and unavailable or corrupt artifacts.
    #[expect(
        clippy::future_not_send,
        reason = "executable I/O runs on its owning compio thread"
    )]
    pub async fn executable(
        &self,
        task: &TaskAssignment,
    ) -> Result<zeroship_bundle::LoadedWorker, WorkflowServiceError> {
        self.service
            .task_executable(&self.worker, &task.id, &task.token)
            .await
    }
}
/// Binds task operations to a worker identity. `WorkflowService` and
/// `AppWorkflows` belong to the admission crate, so an inherent `impl` on
/// either is refused: this trait is local, which is what the orphan rule asks.
/// Callers `use` it to reach `tasks`.
pub trait WorkerBinding {
    #[must_use]
    fn tasks(&self, worker: WorkerIdentity, objects: PayloadObjects) -> WorkerTasks;
}

impl WorkerBinding for WorkflowService {
    fn tasks(&self, worker: WorkerIdentity, objects: PayloadObjects) -> WorkerTasks {
        WorkerTasks {
            service: self.clone(),
            worker,
            objects,
        }
    }
}

impl WorkerBinding for AppWorkflows {
    /// Bind task and payload operations to this app's retained policy generation.
    /// A shared host registry cannot broaden this handle to another app, and a
    /// replacement policy binding cannot renew the authority it captured.
    fn tasks(&self, worker: WorkerIdentity, objects: PayloadObjects) -> WorkerTasks {
        WorkerTasks {
            service: self.service().clone(),
            worker,
            objects,
        }
    }
}

/// Native code loading and execution lifecycle, owned by the worker or CLI.
pub trait TaskExecutor {
    /// Allocate an execution handle. Loading and app execution happen in `wait`
    /// so the runner maintains the lease throughout both. Task credentials stay
    /// in this Rust host; only `assignment.invocation` may enter app code.
    /// Executors that run synchronous app code must install a budget interrupt
    /// before entering it, so starvation of this thread cannot bypass expiry.
    fn start(
        &self,
        assignment: &TaskAssignment,
        budget: ExecutionBudget,
    ) -> Result<Box<dyn TaskExecution>, WorkflowServiceError>;
}

/// The handle owns its callbacks and host operations. Dropping it must cancel
/// native work and quarantine any execution that could outlive the handle;
/// merely rejecting the dispatch Promise does not satisfy this contract.
#[async_trait(?Send)]
pub trait TaskExecution {
    /// Resolve the frontier, without publishing it to the workflow service.
    async fn wait(&mut self) -> Result<WorkflowExecution, WorkflowServiceError>;
    /// Confirmations this execution's settlement still owes, from uploads it
    /// reserved and wrote but could not confirm itself.
    ///
    /// EMPTY FOR A HOST HOLDING THE OBJECT STORE, which confirms each upload
    /// under the lock it wrote under. A host writing across a request boundary
    /// owes one per upload, and they have to reach the settlement rather than a
    /// call of their own: `promote` resolves no `uploading` row, so the confirm
    /// must commit in the same transaction as the frontier referencing it.
    ///
    /// Read after [`Self::wait`]; before it there is nothing to owe.
    fn owed_confirmations(&self) -> Vec<PayloadConfirmation> {
        Vec::new()
    }
    /// Signal cancellation synchronously and idempotently, including on drop.
    fn cancel(&mut self);
    /// Return only after callbacks and host operations have stopped or their
    /// isolate has been quarantined. Must be idempotent and cancellation safe.
    /// An unresponsive executor keeps its slot occupied until this resolves.
    async fn stop(&mut self);
}

struct CancelOnDrop<'a>(&'a mut dyn TaskExecution);
impl Drop for CancelOnDrop<'_> {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

#[cfg(test)]
mod task_scope;
