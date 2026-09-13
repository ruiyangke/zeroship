//! Execution slots for customer workers and local development.

pub mod assignments;
mod budget;
pub mod consumer;
pub mod delivery;
pub mod host;
pub use budget::{ExecutionBudget, ExecutionGuard};
mod worker;
pub use worker::{WorkerOptions, WorkflowWorker};
mod payloads;
mod retention;
pub use payloads::{TaskPayloadReader, TaskPayloads};
mod outputs;
pub use outputs::{PreparedExecution, TaskPayloadLimits};

use super::{
    CompletionReceipt, ControlIntent, Heartbeat, TaskAssignment, TaskToken, WorkerIdentity,
    WorkflowService,
};
use crate::{WorkflowExecution, WorkflowServiceError};
use async_trait::async_trait;
use futures::{future::Either, FutureExt};
use std::{
    cell::Cell,
    rc::Rc,
    time::{Duration, Instant},
};

/// A trusted host's task protocol. App code cannot construct this authority.
#[async_trait(?Send)]
pub trait TaskTransport {
    async fn poll(&self) -> Result<Option<TaskAssignment>, WorkflowServiceError>;
    async fn heartbeat(
        &self,
        task: &str,
        token: &TaskToken,
    ) -> Result<Heartbeat, WorkflowServiceError>;
    async fn complete(
        &self,
        task: &str,
        token: &TaskToken,
        execution: WorkflowExecution,
    ) -> Result<CompletionReceipt, WorkflowServiceError>;
    async fn release(&self, task: &str, token: &TaskToken) -> Result<(), WorkflowServiceError>;
}

/// Task operations bound to the worker identity supplied by the trusted host.
/// The CLI acts as a local worker and uses this same handle.
#[derive(Debug, Clone)]
pub struct WorkerTasks {
    service: WorkflowService,
    worker: WorkerIdentity,
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
impl WorkflowService {
    #[must_use]
    pub fn tasks(&self, worker: WorkerIdentity) -> WorkerTasks {
        WorkerTasks {
            service: self.clone(),
            worker,
        }
    }
}
#[async_trait(?Send)]
impl TaskTransport for WorkerTasks {
    async fn poll(&self) -> Result<Option<TaskAssignment>, WorkflowServiceError> {
        self.service.poll(&self.worker).await
    }
    async fn heartbeat(
        &self,
        task: &str,
        token: &TaskToken,
    ) -> Result<Heartbeat, WorkflowServiceError> {
        self.service.heartbeat(&self.worker, task, token).await
    }
    async fn complete(
        &self,
        task: &str,
        token: &TaskToken,
        execution: WorkflowExecution,
    ) -> Result<CompletionReceipt, WorkflowServiceError> {
        self.service
            .complete(&self.worker, task, token, execution)
            .await
    }
    async fn release(&self, task: &str, token: &TaskToken) -> Result<(), WorkflowServiceError> {
        self.service.release(&self.worker, task, token).await
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
    /// Signal cancellation synchronously and idempotently, including on drop.
    fn cancel(&mut self);
    /// Return only after callbacks and host operations have stopped or their
    /// isolate has been quarantined. Must be idempotent and cancellation safe.
    /// An unresponsive executor keeps its slot occupied until this resolves.
    async fn stop(&mut self);
}

#[derive(Debug)]
pub enum RunnerOutcome {
    Idle,
    Completed(CompletionReceipt),
    Interrupted(ControlIntent),
}

/// Owns execution capacity. An interrupted caller must drain its previous
/// execution before this slot can poll again.
pub struct RunnerSlot {
    transport: Rc<dyn TaskTransport>,
    executor: Rc<dyn TaskExecutor>,
    execution_timeout: Duration,
    active: Option<Active>,
}
impl std::fmt::Debug for RunnerSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunnerSlot")
            .field("execution_timeout", &self.execution_timeout)
            .field("active", &self.active.is_some())
            .finish_non_exhaustive()
    }
}
struct Active {
    assignment: TaskAssignment,
    execution: Box<dyn TaskExecution>,
}
impl Drop for Active {
    fn drop(&mut self) {
        self.execution.cancel();
    }
}
struct CancelOnDrop<'a>(&'a mut dyn TaskExecution);
impl Drop for CancelOnDrop<'_> {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

impl RunnerSlot {
    pub fn new(
        transport: Rc<dyn TaskTransport>,
        executor: Rc<dyn TaskExecutor>,
        execution_timeout: Duration,
    ) -> Result<Self, WorkflowServiceError> {
        if execution_timeout.is_zero() {
            return Err(WorkflowServiceError::InvalidRequest(
                "workflow execution timeout must be positive".into(),
            ));
        }
        Ok(Self {
            transport,
            executor,
            execution_timeout,
            active: None,
        })
    }

    pub async fn run_once(&mut self) -> Result<RunnerOutcome, WorkflowServiceError> {
        self.drain_interrupted().await;
        let started = Instant::now();
        let Some(assignment) = compio::time::timeout(self.execution_timeout, self.transport.poll())
            .await
            .map_err(|_| WorkflowServiceError::Timeout)??
        else {
            return Ok(RunnerOutcome::Idle);
        };
        let lease = Cell::new(LeaseWindow::new(assignment.lease_ms, started)?);
        let guard = ExecutionGuard::new(self.execution_timeout)?;
        guard.renew_lease(lease.get().expires)?;
        let execution = match self.executor.start(&assignment, guard.budget()) {
            Ok(execution) => execution,
            Err(error) => {
                let _ = release(self.transport.as_ref(), &assignment, &lease).await;
                return Err(error);
            }
        };
        self.active = Some(Active {
            assignment,
            execution,
        });
        let active = self.active.as_mut().expect("installed workflow execution");
        let result = {
            let execution = CancelOnDrop(active.execution.as_mut());
            let outcome = {
                let work = execute(
                    self.transport.as_ref(),
                    &active.assignment,
                    execution.0,
                    &lease,
                    &guard,
                    self.execution_timeout,
                )
                .boxed_local();
                let ownership = renew(self.transport.as_ref(), &active.assignment, &lease, &guard)
                    .boxed_local();
                match futures::future::select(work, ownership).await {
                    Either::Left((result, ownership)) => {
                        drop(ownership);
                        Either::Left(result)
                    }
                    Either::Right((control, work)) => {
                        drop(work);
                        Either::Right(control)
                    }
                }
            };
            match outcome {
                Either::Left(result) => result,
                Either::Right(control) => {
                    execution.0.cancel();
                    guard.finish();
                    execution.0.stop().await;
                    match control {
                        Ok(control) => release(self.transport.as_ref(), &active.assignment, &lease)
                            .await
                            .map(|()| RunnerOutcome::Interrupted(control)),
                        Err(error) => Err(error),
                    }
                }
            }
        };
        self.active = None;
        result
    }

    /// Drain on graceful shutdown, or before resuming a cancelled `run_once`.
    pub async fn drain_interrupted(&mut self) {
        if let Some(active) = self.active.as_mut() {
            let execution = CancelOnDrop(active.execution.as_mut());
            execution.0.cancel();
            execution.0.stop().await;
            // The previous owner may already have expired. Release is only a
            // progress hint after execution has stopped; expiry also recovers it.
            let _ = compio::time::timeout(
                self.execution_timeout,
                self.transport
                    .release(&active.assignment.id, &active.assignment.token),
            )
            .await;
        }
        self.active = None;
    }
}

#[derive(Clone, Copy)]
struct LeaseWindow {
    expires: Instant,
}
impl LeaseWindow {
    fn new(lease_ms: i64, started: Instant) -> Result<Self, WorkflowServiceError> {
        let duration = u64::try_from(lease_ms)
            .ok()
            .filter(|duration| *duration > 0)
            .map(Duration::from_millis)
            .ok_or_else(|| {
                WorkflowServiceError::Unavailable("invalid workflow lease duration".into())
            })?;
        let expires = started
            .checked_add(duration)
            .filter(|expires| *expires > Instant::now())
            .ok_or(WorkflowServiceError::Timeout)?;
        Ok(Self { expires })
    }
    fn remaining(self) -> Duration {
        self.expires.saturating_duration_since(Instant::now())
    }
}

async fn renew(
    transport: &dyn TaskTransport,
    task: &TaskAssignment,
    lease: &Cell<LeaseWindow>,
    guard: &ExecutionGuard,
) -> Result<ControlIntent, WorkflowServiceError> {
    loop {
        let remaining = lease.get().remaining();
        if remaining.is_zero() {
            return Err(WorkflowServiceError::Timeout);
        }
        compio::time::sleep(remaining / 3).await;
        let started = Instant::now();
        let heartbeat = compio::time::timeout(
            lease.get().remaining(),
            transport.heartbeat(&task.id, &task.token),
        )
        .await
        .map_err(|_| WorkflowServiceError::Timeout)??;
        lease.set(LeaseWindow::new(heartbeat.lease_ms, started)?);
        guard.renew_lease(lease.get().expires)?;
        if heartbeat.control != ControlIntent::None {
            return Ok(heartbeat.control);
        }
    }
}

async fn execute(
    transport: &dyn TaskTransport,
    task: &TaskAssignment,
    execution: &mut dyn TaskExecution,
    lease: &Cell<LeaseWindow>,
    guard: &ExecutionGuard,
    timeout: Duration,
) -> Result<RunnerOutcome, WorkflowServiceError> {
    let result = compio::time::timeout(timeout, execution.wait())
        .await
        .map_err(|_| WorkflowServiceError::Timeout)
        .and_then(|result| result)
        .and_then(|result| guard.budget().check().map(|()| result));
    execution.cancel();
    guard.finish();
    execution.stop().await;
    let outcomes = match result {
        Ok(outcomes) => outcomes,
        Err(error) => {
            let _ = release(transport, task, lease).await;
            return Err(error);
        }
    };
    let completion_deadline = Instant::now()
        .checked_add(timeout)
        .ok_or(WorkflowServiceError::Timeout)?;
    loop {
        let remaining = lease
            .get()
            .remaining()
            .min(completion_deadline.saturating_duration_since(Instant::now()));
        if remaining.is_zero() {
            return Err(WorkflowServiceError::Timeout);
        }
        let result = compio::time::timeout(
            remaining,
            transport.complete(&task.id, &task.token, outcomes.clone()),
        )
        .await;
        match result {
            Ok(Ok(receipt)) => return Ok(RunnerOutcome::Completed(receipt)),
            Ok(Err(WorkflowServiceError::Unavailable(_) | WorkflowServiceError::Timeout)) => {
                // The same outcome batch recovers a lost completion response.
                // Keep its lease alive while waiting; never execute it again.
                compio::time::sleep(lease.get().remaining().min(Duration::from_millis(100))).await;
            }
            Ok(Err(error)) => return Err(error),
            Err(_) => return Err(WorkflowServiceError::Timeout),
        }
    }
}

async fn release(
    transport: &dyn TaskTransport,
    task: &TaskAssignment,
    lease: &Cell<LeaseWindow>,
) -> Result<(), WorkflowServiceError> {
    compio::time::timeout(
        lease.get().remaining(),
        transport.release(&task.id, &task.token),
    )
    .await
    .map_err(|_| WorkflowServiceError::Timeout)?
}
