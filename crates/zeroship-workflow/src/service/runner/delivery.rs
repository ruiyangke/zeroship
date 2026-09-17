//! Bounded execution of an already delivered job. Scheduling remains with its owner.

#![expect(
    clippy::future_not_send,
    reason = "delivery slots own compio-local resources"
)]

use super::{CancelOnDrop, ExecutionGuard, TaskExecution, TaskExecutor};
use crate::{
    service::{
        collection::CollectionOptions,
        delivery::{DeliveredTask, JobAcceptance, JobReceipt},
        fanout::FanoutOptions,
        policy::PolicyAuthority,
        propagation::PropagationOptions,
        publication::JobPublisher,
        reconciliation::ReconciliationOptions,
        AppWorkflows, ControlIntent,
    },
    WorkflowServiceError,
};
use futures::{future::Either, FutureExt};
use std::{
    cell::{Cell, RefCell},
    future::Future,
    rc::Rc,
    time::{Duration, Instant},
};
use zeroship_core::{
    workflow_coordination::{AssignedScope, FailureCode},
    workflow_jobs::{
        Delivery, JobLease, JobOperation, JobSpec, Settlement, SettlementReceipt, SubmitJob,
    },
};
use zeroship_workflow_client::{LeasedJob, WorkerCoordinator};

/// Metadata operations supplied by the host; no creator data crosses this port.
/// Implementations preserve authenticated lease identity and monotonic expiry.
pub trait JobTransport {
    type Lease: JobLease + Clone;

    fn claim(
        &self,
        scope: &AssignedScope,
    ) -> impl Future<Output = Result<Option<Self::Lease>, WorkflowServiceError>>;
    fn submit(
        &self,
        scope: &AssignedScope,
        job: &JobSpec,
    ) -> impl Future<Output = Result<JobSpec, WorkflowServiceError>>;
    fn heartbeat(
        &self,
        lease: &Self::Lease,
    ) -> impl Future<Output = Result<Self::Lease, WorkflowServiceError>>;
    fn settle(
        &self,
        settlement: &Settlement,
    ) -> impl Future<Output = Result<SettlementReceipt, WorkflowServiceError>>;
}

impl JobTransport for WorkerCoordinator {
    type Lease = LeasedJob;

    async fn submit(
        &self,
        scope: &AssignedScope,
        job: &JobSpec,
    ) -> Result<JobSpec, WorkflowServiceError> {
        self.submit_job(&SubmitJob {
            scope: scope.clone(),
            job: job.clone(),
        })
        .await
        .map_err(metadata_error)
    }

    async fn claim(
        &self,
        scope: &AssignedScope,
    ) -> Result<Option<Self::Lease>, WorkflowServiceError> {
        self.claim_job(scope).await.map_err(metadata_error)
    }
    async fn heartbeat(&self, lease: &Self::Lease) -> Result<Self::Lease, WorkflowServiceError> {
        self.heartbeat_job(lease).await.map_err(metadata_error)
    }
    async fn settle(
        &self,
        settlement: &Settlement,
    ) -> Result<SettlementReceipt, WorkflowServiceError> {
        self.settle_job(settlement).await.map_err(metadata_error)
    }
}

/// Execution and finalization have separate bounds; renewal never resets either.
#[derive(Debug, Clone, Copy)]
pub struct DeliveryOptions {
    pub execution_timeout: Duration,
    pub operation_timeout: Duration,
    pub retry_delay: Duration,
    pub reconciliation: ReconciliationOptions,
    pub collection: CollectionOptions,
    pub fanout: FanoutOptions,
    pub propagation: PropagationOptions,
}

impl DeliveryOptions {
    pub(super) fn validate(self) -> Result<(), WorkflowServiceError> {
        self.reconciliation.validate()?;
        self.collection.validate()?;
        self.fanout.validate()?;
        self.propagation.validate()?;
        if self.execution_timeout.is_zero()
            || self.operation_timeout.is_zero()
            || self.retry_delay.is_zero()
            || Instant::now().checked_add(self.execution_timeout).is_none()
            || Instant::now().checked_add(self.operation_timeout).is_none()
            || Instant::now().checked_add(self.retry_delay).is_none()
        {
            return Err(WorkflowServiceError::InvalidRequest(
                "invalid workflow delivery bounds".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug)]
pub enum DeliveryOutcome {
    /// No executor started and no scheduling outcome was acknowledged.
    Deferred,
    Interrupted(ControlIntent),
    Settled {
        creator: Box<JobReceipt>,
        manager: SettlementReceipt,
    },
}

struct Claims<L> {
    lease: L,
    task: DeliveredTask,
}

struct Active<L> {
    app: AppWorkflows,
    claims: RefCell<Claims<L>>,
    execution: Box<dyn TaskExecution>,
    authority: PolicyAuthority,
    guard: ExecutionGuard,
    phase: Phase,
}

struct Phase {
    deadline: Cell<Instant>,
    failure: RefCell<Option<WorkflowServiceError>>,
}

impl Phase {
    fn new(timeout: Duration) -> Self {
        Self {
            deadline: Cell::new(Instant::now() + timeout),
            failure: RefCell::new(None),
        }
    }

    fn remaining(&self) -> Result<Duration, WorkflowServiceError> {
        self.deadline
            .get()
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or(WorkflowServiceError::Timeout)
    }

    fn finalize(&self, timeout: Duration, failure: Option<&WorkflowServiceError>) {
        self.deadline.set(Instant::now() + timeout);
        *self.failure.borrow_mut() = failure.cloned();
    }
}

impl<L> Drop for Active<L> {
    fn drop(&mut self) {
        self.guard.finish();
        self.execution.cancel();
    }
}

/// A slot retains cancelled execution until its native operations have joined.
/// Hosts supply an authorized app and a claimed job; this slot discovers no work.
pub struct DeliverySlot<T: JobTransport> {
    transport: Rc<T>,
    executor: Rc<dyn TaskExecutor>,
    options: DeliveryOptions,
    active: Option<Active<T::Lease>>,
}

impl<T: JobTransport> std::fmt::Debug for DeliverySlot<T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DeliverySlot")
            .field("options", &self.options)
            .field("active", &self.active.is_some())
            .finish_non_exhaustive()
    }
}

impl<T: JobTransport> DeliverySlot<T> {
    /// # Errors
    /// Refuses empty or unrepresentable execution and I/O bounds.
    pub fn new(
        transport: Rc<T>,
        executor: Rc<dyn TaskExecutor>,
        options: DeliveryOptions,
    ) -> Result<Self, WorkflowServiceError> {
        options.validate()?;
        Ok(Self {
            transport,
            executor,
            options,
            active: None,
        })
    }

    /// Execute a supported delivery, or replay its committed receipt.
    /// Dropping this future cancels execution. Drain before reusing the slot.
    ///
    /// # Errors
    /// Refuses stale authority, invalid jobs, failed execution and unavailable
    /// storage or transport. Committed creator receipts survive failed ACKs.
    pub async fn run(
        &mut self,
        app: &AppWorkflows,
        lease: T::Lease,
    ) -> Result<DeliveryOutcome, WorkflowServiceError> {
        self.drain_interrupted().await;
        if matches!(
            lease.delivery().job.operation,
            JobOperation::Activate { .. }
        ) {
            let receipt = bounded(self.options.execution_timeout, app.activate_job(&lease)).await?;
            return self.acknowledge(receipt, &lease).await;
        }
        if matches!(lease.delivery().job.operation, JobOperation::Reconcile {}) {
            let publisher = Submission {
                transport: self.transport.as_ref(),
                scope: AssignedScope {
                    app_id: lease.delivery().job.app_id.clone(),
                    assignment_revision: lease.delivery().assignment_revision,
                },
            };
            let receipt = bounded(
                self.options.execution_timeout,
                app.reconcile_job(&lease, &publisher, self.options.reconciliation),
            )
            .await?;
            return self.acknowledge(receipt, &lease).await;
        }
        if matches!(lease.delivery().job.operation, JobOperation::Cron { .. }) {
            let receipt = bounded(self.options.execution_timeout, app.cron_job(&lease)).await?;
            return self.acknowledge(receipt, &lease).await;
        }
        if matches!(
            lease.delivery().job.operation,
            JobOperation::Management { .. }
        ) {
            let receipt =
                bounded(self.options.execution_timeout, app.management_job(&lease)).await?;
            return self.acknowledge(receipt, &lease).await;
        }
        if matches!(
            lease.delivery().job.operation,
            JobOperation::ReleaseHold { .. }
        ) {
            let receipt =
                bounded(self.options.execution_timeout, app.release_hold_job(&lease)).await?;
            return self.acknowledge(receipt, &lease).await;
        }
        if matches!(lease.delivery().job.operation, JobOperation::Collect {}) {
            let receipt = bounded(
                self.options.execution_timeout,
                app.collect_job(&lease, self.options.collection),
            )
            .await?;
            return self.acknowledge(receipt, &lease).await;
        }
        if matches!(lease.delivery().job.operation, JobOperation::Close { .. }) {
            let receipt = bounded(self.options.execution_timeout, app.close_job(&lease)).await?;
            return self.acknowledge(receipt, &lease).await;
        }
        if matches!(lease.delivery().job.operation, JobOperation::Fanout { .. }) {
            let receipt = bounded(
                self.options.execution_timeout,
                app.fanout_job(&lease, self.options.fanout),
            )
            .await?;
            return match receipt {
                Some(receipt) => self.acknowledge(receipt, &lease).await,
                None => Ok(DeliveryOutcome::Deferred),
            };
        }
        if matches!(
            lease.delivery().job.operation,
            JobOperation::Propagate { .. }
        ) {
            let receipt = bounded(
                self.options.execution_timeout,
                app.propagation_job(&lease, self.options.propagation),
            )
            .await?;
            return self.acknowledge(receipt, &lease).await;
        }
        let authority = app.capture_policy().authority().cloned();
        let accepted = app.accept_job(&lease).await?;
        let task = match accepted {
            JobAcceptance::Deferred => return Ok(DeliveryOutcome::Deferred),
            JobAcceptance::Settled(receipt) => return self.acknowledge(receipt, &lease).await,
            JobAcceptance::Execute(task) => *task,
        };
        let authority = match authority.and_then(|authority| {
            authority.check()?;
            Ok(authority)
        }) {
            Ok(authority) => authority,
            Err(error) => {
                release(app, &task, &lease, self.options.operation_timeout).await;
                return Err(error);
            }
        };
        let timeout = authority
            .deadline
            .map_or(self.options.execution_timeout, |deadline| {
                self.options
                    .execution_timeout
                    .min(deadline.saturating_duration_since(Instant::now()))
            });
        let guard = match ExecutionGuard::new(timeout) {
            Ok(guard) => guard,
            Err(error) => {
                release(app, &task, &lease, self.options.operation_timeout).await;
                return Err(error);
            }
        };
        let claims = Claims { lease, task };
        if let Err(error) = guard
            .cancel_on(authority.cancelled())
            .and_then(|()| constrain(&guard, &claims))
        {
            release(
                app,
                &claims.task,
                &claims.lease,
                self.options.operation_timeout,
            )
            .await;
            return Err(error);
        }
        let scoped = app.clone().with_authority(authority.clone())?;
        let execution = match self
            .executor
            .start(claims.task.assignment(), guard.budget())
        {
            Ok(execution) => execution,
            Err(error) => {
                release(
                    app,
                    &claims.task,
                    &claims.lease,
                    self.options.operation_timeout,
                )
                .await;
                return Err(error);
            }
        };
        let active = self.active.insert(Active {
            app: scoped,
            claims: RefCell::new(claims),
            execution,
            authority,
            guard,
            // The SAME bound the guard holds, so the renewal delay computed
            // from this phase cannot fall past the end of the attempt. The
            // delay is a fraction of the smallest bound that can end an
            // attempt, and captured host authority is one of them: the
            // manager counts an attempt into its delivery ceiling only on
            // that attempt's first renewal, so an attempt the authority
            // window cuts short before any renewal leaves the ceiling where
            // it was while redelivery continues.
            phase: Phase::new(timeout),
        });
        let result = Box::pin(run_active(self.transport.as_ref(), active, self.options)).await;
        let lease = active.claims.borrow().lease.clone();
        self.active = None;
        match result? {
            ExecutionResult::Complete(receipt) => self.acknowledge(receipt, &lease).await,
            ExecutionResult::Interrupted(control) => Ok(DeliveryOutcome::Interrupted(control)),
        }
    }

    /// Stop and join a cancelled invocation before releasing its creator claim.
    /// A missing manager job-release endpoint is intentional: abandoned delivery
    /// authority expires and the logical job remains available for redelivery.
    pub async fn drain_interrupted(&mut self) {
        if let Some(active) = &mut self.active {
            let execution = CancelOnDrop(active.execution.as_mut());
            active.guard.finish();
            execution.0.cancel();
            execution.0.stop().await;
            let (task, lease) = snapshot(&active.claims);
            release(&active.app, &task, &lease, self.options.operation_timeout).await;
        }
        self.active = None;
    }

    async fn acknowledge(
        &self,
        receipt: JobReceipt,
        lease: &T::Lease,
    ) -> Result<DeliveryOutcome, WorkflowServiceError> {
        // Construct once. In particular, an outbox retry must not change this
        // attempt's successor set after the manager might have committed it.
        let settlement = receipt.settlement(lease)?;
        let manager = bounded(self.options.operation_timeout, async {
            loop {
                match self.transport.settle(&settlement).await {
                    Ok(observed) => {
                        if observed.job_id != settlement.delivery.job.id
                            || observed.app_id != settlement.delivery.job.app_id
                            || observed.attempt != settlement.delivery.attempt
                            || observed.outcome != settlement.outcome
                        {
                            return Err(WorkflowServiceError::Unavailable(
                                "workflow settlement identity changed".into(),
                            ));
                        }
                        return Ok(observed);
                    }
                    Err(error) if retryable(&error) => {
                        compio::time::sleep(self.options.retry_delay).await;
                    }
                    Err(error) => return Err(error),
                }
            }
        })
        .await?;
        Ok(DeliveryOutcome::Settled {
            creator: Box::new(receipt),
            manager,
        })
    }
}

struct Submission<'a, T> {
    transport: &'a T,
    scope: AssignedScope,
}
impl<T: JobTransport> JobPublisher for Submission<'_, T> {
    fn app_id(&self) -> &zeroship_core::app_id::AppId {
        &self.scope.app_id
    }
    async fn submit(&self, job: &JobSpec) -> Result<JobSpec, WorkflowServiceError> {
        self.transport.submit(&self.scope, job).await
    }
}

enum ExecutionResult {
    Complete(JobReceipt),
    Interrupted(ControlIntent),
}

async fn run_active<T: JobTransport>(
    transport: &T,
    active: &mut Active<T::Lease>,
    options: DeliveryOptions,
) -> Result<ExecutionResult, WorkflowServiceError> {
    let execution = CancelOnDrop(active.execution.as_mut());
    let result = {
        let work = execute(
            &active.app,
            &active.claims,
            execution.0,
            &active.guard,
            &active.phase,
            options,
        )
        .boxed_local();
        let ownership = active.authority.run(renew(
            transport,
            &active.app,
            &active.claims,
            &active.guard,
            &active.phase,
            options,
        ));
        match futures::future::select(work, ownership).await {
            Either::Left((result, ownership)) => {
                drop(ownership);
                Ok(result)
            }
            Either::Right((control, work)) => {
                drop(work);
                Err(control)
            }
        }
    };
    // Every path joins before release, ACK or capacity reuse. Cancellation
    // while joining leaves Active installed for drain_interrupted.
    execution.0.cancel();
    active.guard.finish();
    execution.0.stop().await;
    let lease = active.claims.borrow().lease.clone();
    let result = match result {
        Ok(Ok(receipt)) => Ok(receipt),
        Ok(Err(error)) | Err(Err(error)) => {
            if let Ok(Some(receipt)) = recover(&active.app, &lease, options.operation_timeout).await
            {
                Ok(receipt)
            } else {
                let task = active.claims.borrow().task.clone();
                release(&active.app, &task, &lease, options.operation_timeout).await;
                Err(active.phase.failure.borrow().clone().unwrap_or(error))
            }
        }
        Err(Ok(control)) => {
            if let Ok(Some(receipt)) = recover(&active.app, &lease, options.operation_timeout).await
            {
                Ok(receipt)
            } else {
                let task = active.claims.borrow().task.clone();
                release(&active.app, &task, &lease, options.operation_timeout).await;
                return Ok(ExecutionResult::Interrupted(control));
            }
        }
    };
    result.map(ExecutionResult::Complete)
}

fn snapshot<L: Clone>(claims: &RefCell<Claims<L>>) -> (DeliveredTask, L) {
    let claims = claims.borrow();
    (claims.task.clone(), claims.lease.clone())
}

fn available<L: JobLease>(claims: &Claims<L>) -> Result<Duration, WorkflowServiceError> {
    let remaining = claims
        .lease
        .remaining()
        .filter(|remaining| !remaining.is_zero())
        .ok_or(WorkflowServiceError::Timeout)?;
    Ok(remaining.min(claims.task.remaining()?))
}

fn constrain<L: JobLease>(
    guard: &ExecutionGuard,
    claims: &Claims<L>,
) -> Result<(), WorkflowServiceError> {
    let started = Instant::now();
    let deadline = started
        .checked_add(available(claims)?)
        .ok_or(WorkflowServiceError::Timeout)?;
    guard.renew_lease(deadline)
}

async fn renew<T: JobTransport>(
    transport: &T,
    app: &AppWorkflows,
    claims: &RefCell<Claims<T::Lease>>,
    guard: &ExecutionGuard,
    phase: &Phase,
    options: DeliveryOptions,
) -> Result<ControlIntent, WorkflowServiceError> {
    loop {
        let delay = available(&claims.borrow())?.min(phase.remaining()?) / 3;
        compio::time::sleep(delay).await;
        let (task, original) = snapshot(claims);
        let timeout = available(&claims.borrow())?
            .min(options.operation_timeout)
            .min(phase.remaining()?);
        let (task, renewed, control) = bounded(timeout, async {
            let renewed = transport.heartbeat(&original).await?;
            if !same_delivery(original.delivery(), renewed.delivery()) {
                return Err(WorkflowServiceError::PermissionDenied);
            }
            original
                .remaining()
                .filter(|remaining| !remaining.is_zero())
                .ok_or(WorkflowServiceError::Timeout)?;
            task.remaining()?;
            let (task, control) = app.heartbeat_job(&task, &renewed).await?;
            Ok((task, renewed, control))
        })
        .await?;
        *claims.borrow_mut() = Claims {
            lease: renewed,
            task,
        };
        constrain(guard, &claims.borrow())?;
        if control != ControlIntent::None {
            return Ok(control);
        }
    }
}

async fn execute<L: JobLease + Clone>(
    app: &AppWorkflows,
    claims: &RefCell<Claims<L>>,
    execution: &mut dyn TaskExecution,
    guard: &ExecutionGuard,
    phase: &Phase,
    options: DeliveryOptions,
) -> Result<JobReceipt, WorkflowServiceError> {
    // A resolved frontier describes effects that already reached the world, so
    // local expiry does not withdraw the right to publish it; the completion
    // loop below stays bounded by the phase deadline and the creator lease.
    // Revoked delivery authority does withdraw it, and publishes nothing.
    let result = bounded(options.execution_timeout, execution.wait())
        .await
        .and_then(|outcome| guard.budget().check_authority().map(|()| outcome));
    // Joining remains mandatory after this deadline, but it must not keep
    // renewing manager or creator authority while native shutdown is stuck.
    phase.finalize(options.operation_timeout, result.as_ref().err());
    execution.cancel();
    guard.finish();
    execution.stop().await;
    let outcome = result?;
    bounded(phase.remaining()?, async {
        loop {
            let (task, lease) = snapshot(claims);
            match app.complete_job(&task, &lease, outcome.clone()).await {
                Ok(receipt) => return Ok(receipt),
                Err(error) if retryable(&error) => {
                    if let Ok(Some(receipt)) = app.job_receipt(&lease.delivery().job).await {
                        return Ok(receipt);
                    }
                    compio::time::sleep(options.retry_delay).await;
                }
                Err(error) => return Err(error),
            }
        }
    })
    .await
}

async fn recover<L: JobLease>(
    app: &AppWorkflows,
    lease: &L,
    timeout: Duration,
) -> Result<Option<JobReceipt>, WorkflowServiceError> {
    bounded(timeout, app.job_receipt(&lease.delivery().job)).await
}

async fn release<L: JobLease>(
    app: &AppWorkflows,
    task: &DeliveredTask,
    lease: &L,
    timeout: Duration,
) {
    let _ = bounded(timeout, app.release_job(task, lease)).await;
}

pub(super) async fn bounded<T>(
    timeout: Duration,
    future: impl Future<Output = Result<T, WorkflowServiceError>>,
) -> Result<T, WorkflowServiceError> {
    compio::time::timeout(timeout, Box::pin(future))
        .await
        .map_err(|_| WorkflowServiceError::Timeout)?
}

fn same_delivery(left: &Delivery, right: &Delivery) -> bool {
    left.job == right.job
        && left.worker_id == right.worker_id
        && left.assignment_revision == right.assignment_revision
        && left.attempt == right.attempt
}

const fn retryable(error: &WorkflowServiceError) -> bool {
    matches!(
        error,
        WorkflowServiceError::Timeout | WorkflowServiceError::Unavailable(_)
    )
}

fn metadata_error(error: zeroship_workflow_client::Error) -> WorkflowServiceError {
    use zeroship_workflow_client::Error;
    match error {
        Error::InvalidConfig | Error::Refused(FailureCode::Invalid) => {
            WorkflowServiceError::InvalidRequest(
                "invalid workflow metadata request or configuration".into(),
            )
        }
        Error::Unauthenticated | Error::Refused(FailureCode::Unauthenticated) => {
            WorkflowServiceError::Unauthenticated
        }
        Error::Refused(FailureCode::Denied) => WorkflowServiceError::PermissionDenied,
        Error::Refused(FailureCode::Conflict) => {
            WorkflowServiceError::Conflict("workflow delivery is no longer current".into())
        }
        Error::Refused(FailureCode::Capacity) => {
            WorkflowServiceError::ResourceExhausted("workflow coordinator is full".into())
        }
        Error::RequestTooLarge
        | Error::ResponseTooLarge
        | Error::Refused(FailureCode::RequestTooLarge) => WorkflowServiceError::PayloadTooLarge,
        Error::Timeout => WorkflowServiceError::Timeout,
        Error::Unavailable | Error::InvalidResponse | Error::Refused(FailureCode::Unavailable) => {
            WorkflowServiceError::Unavailable("workflow coordinator is unavailable".into())
        }
    }
}

#[cfg(test)]
mod tests;
