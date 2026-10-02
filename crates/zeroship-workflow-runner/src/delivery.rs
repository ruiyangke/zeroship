//! Bounded execution of an already delivered job. Scheduling remains with its owner.

#![expect(
    clippy::future_not_send,
    reason = "delivery slots own compio-local resources"
)]

use crate::{CancelOnDrop, ExecutionGuard, TaskExecution, TaskExecutor};
use zeroship_workflow::{
    service::{
        delivery::{
            AppJournal, DeliveredTask, JobAcceptance, JobReceipt, PayloadConfirmation,
            ReportedExecution, TaskRenewal,
        },
        AppWorkflows, ControlIntent, PolicyAuthority, PolicyBinding,
    },
    WorkflowExecution, WorkflowServiceError,
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
    workflow_jobs::{Delivery, JobLease, JobSpec, JournalSettlement, SettlementReceipt},
};
use zeroship_workflow_client::{LeasedJob, WorkerCoordinator};

/// One delivery exchange, whose two halves are the manager's queue and the
/// app's journal.
///
/// EACH OPERATION CARRIES BOTH HALVES OR NEITHER. A claim answers a lease and
/// the acceptance that authorizes executing under it; a renewal extends the
/// queue lease and the journal task together; a settlement commits the
/// execution and settles the delivery with what that commit decided. An
/// implementation either performs both halves in this process or sends one
/// request that does, and the journal it acts on arrives as an argument -- this
/// port grants no journal capability of its own.
///
/// Implementations preserve authenticated lease identity and monotonic expiry.
pub trait JobTransport {
    type Lease: JobLease + Clone;
    /// The journal this transport reaches, as this host holds it.
    ///
    /// `AppWorkflows` for a host whose journal is in its own process. `()` for the
    /// crossed transport, and that does NOT mean the journal is absent -- it means
    /// the journal is at the far end of the call, where the service establishes
    /// authority server-side from the credential that signed the request. A reader
    /// who takes the unit type for "no journal" will reach for the wrong extension
    /// later: there is a journal, and this host simply holds no handle to it.
    ///
    /// DECLARED RATHER THAN RETURNED, which is what makes it safe. A transport that
    /// declares `()` and then needs a journal does not misbehave quietly; it fails
    /// to compile, because there is no value of that type to use.
    type Journal;
    /// Bind one attempt's policy authority to this transport's journal.
    ///
    /// ONE SITE PER SHAPE, deliberately. The retained authority is not decoration:
    /// `CapturedLease::capture` reads it to choose the policy snapshot whose
    /// `lease_ms` bounds the attempt, so an unscoped journal yields a DIFFERENT
    /// lease budget rather than an error. An in-process transport therefore
    /// delegates to `scope_journal`, and the crossed one has nothing to scope.
    fn scope(
        &self,
        journal: &Self::Journal,
        authority: &PolicyAuthority,
    ) -> Result<Self::Journal, WorkflowServiceError>;

    fn claim(
        &self,
        journal: &Self::Journal,
        scope: &AssignedScope,
    ) -> impl Future<Output = Result<Option<Claimed<Self::Lease>>, WorkflowServiceError>>;
    fn heartbeat(
        &self,
        journal: &Self::Journal,
        lease: &Self::Lease,
        task: &DeliveredTask,
    ) -> impl Future<Output = Result<Renewed<Self::Lease>, WorkflowServiceError>>;
    /// Settle a delivery whose job the journal has already committed, with the
    /// receipt the journal holds for it.
    ///
    /// NO OUTCOME IS AN ARGUMENT. Whoever holds the journal reads the receipt at
    /// settlement time -- this process, through [`committed_settlement`], or the
    /// service at the far end of the call -- so every transport records the
    /// outcome the journal decided and no successor, whatever its caller holds.
    fn settle(
        &self,
        journal: &Self::Journal,
        lease: &Self::Lease,
    ) -> impl Future<Output = Result<SettlementReceipt, WorkflowServiceError>>;
    /// Commit a frontier, and any upload confirmations its settlement owes.
    ///
    /// `confirmed` is EMPTY for a holder whose object store is the journal's own
    /// process, which confirms each upload under the lock it wrote under. A
    /// holder writing across a request boundary owes one per upload, and they
    /// ride this call rather than one of their own because `promote` resolves no
    /// `uploading` row: the confirm has to commit in the same transaction as the
    /// frontier that references it.
    fn complete(
        &self,
        journal: &Self::Journal,
        lease: &Self::Lease,
        task: &DeliveredTask,
        execution: WorkflowExecution,
        confirmed: Vec<PayloadConfirmation>,
    ) -> impl Future<Output = Result<Completed, WorkflowServiceError>>;
    /// Hand a claimed task back without settling its delivery.
    ///
    /// ON THE TRANSPORT RATHER THAN THE JOURNAL, because a host that holds no
    /// journal still has to give work back. A release commits no execution, so it
    /// is not fenced on deployment admissibility: the row reopens, `reclaim`
    /// expires it, and `tasks::assign` re-checks availability before the next
    /// dispatch.
    fn release(
        &self,
        journal: &Self::Journal,
        lease: &Self::Lease,
        task: &DeliveredTask,
    ) -> impl Future<Output = Result<(), WorkflowServiceError>>;
    /// Read what an attempt of this logical job already committed, if any.
    ///
    /// The recovery read for an uncertain settlement, for the same reason: the
    /// reply a holder lost may have committed, and a holder with no journal
    /// cannot ask one directly. Absence is a fact rather than a refusal.
    fn receipt(
        &self,
        journal: &Self::Journal,
        job: &JobSpec,
    ) -> impl Future<Output = Result<Option<JobReceipt>, WorkflowServiceError>>;
}

/// A claimed delivery and the journal acceptance that rode with it.
#[derive(Debug)]
pub struct Claimed<L> {
    pub lease: L,
    /// Present for the one operation this port delivers. A claimant holding a
    /// placement is admitted to creator work alone, so a reply carrying none is
    /// a transport that dropped half of its own answer rather than a kind this
    /// slot settles without an executor.
    pub accepted: Option<JobAcceptance>,
}

/// A renewed delivery and the journal renewal that rode with it.
#[derive(Debug)]
pub struct Renewed<L> {
    pub lease: L,
    pub renewal: TaskRenewal,
}

/// A committed execution and the settlement its outcome produced.
#[derive(Debug)]
pub struct Completed {
    pub receipt: JobReceipt,
    pub settlement: SettlementReceipt,
}

impl JobTransport for WorkerCoordinator {
    type Lease = LeasedJob;
    /// The journal is the SERVICE'S, at the far end of every call here. This host
    /// holds no handle to it and needs none: the service binds the app's journal
    /// itself and establishes authority from the credential that signed the
    /// request, so there is nothing for this side to scope.
    type Journal = ();
    fn scope(
        &self,
        _journal: &Self::Journal,
        _authority: &PolicyAuthority,
    ) -> Result<Self::Journal, WorkflowServiceError> {
        Ok(())
    }

    async fn claim(
        &self,
        // The journal these deliveries are accepted into is the coordinator's,
        // at the far end of this call. A host that reaches the manager over HTTP
        // holds no credential to it, which is what merging the halves is for.
        _journal: &Self::Journal,
        scope: &AssignedScope,
    ) -> Result<Option<Claimed<Self::Lease>>, WorkflowServiceError> {
        let claimed = self
            .claim_job::<AppJournal>(scope)
            .await
            .map_err(metadata_error)?;
        claimed
            .map(|claimed| {
                Ok(Claimed {
                    accepted: claimed
                        .accepted
                        .map(|accepted| {
                            accepted.received(claimed.lease.delivery(), claimed.started)
                        })
                        .transpose()?,
                    lease: claimed.lease,
                })
            })
            .transpose()
    }

    async fn heartbeat(
        &self,
        _journal: &Self::Journal,
        lease: &Self::Lease,
        task: &DeliveredTask,
    ) -> Result<Renewed<Self::Lease>, WorkflowServiceError> {
        let renewed = self
            .heartbeat_job::<AppJournal>(lease, Some(&task.reported()?))
            .await
            .map_err(metadata_error)?;
        let renewal = renewed
            .renewal
            .ok_or_else(|| lossy("renewal"))?
            .received(renewed.lease.delivery(), renewed.started)?;
        Ok(Renewed {
            lease: renewed.lease,
            renewal,
        })
    }

    /// The service settles from its own journal's receipt, so only the delivery
    /// crosses.
    async fn settle(
        &self,
        _journal: &Self::Journal,
        lease: &Self::Lease,
    ) -> Result<SettlementReceipt, WorkflowServiceError> {
        self.settle_committed(lease.delivery())
            .await
            .map_err(metadata_error)
    }

    /// The journal half of a release crosses with the delivery it gives back.
    async fn release(
        &self,
        _journal: &Self::Journal,
        lease: &Self::Lease,
        task: &DeliveredTask,
    ) -> Result<(), WorkflowServiceError> {
        self.release_job::<AppJournal>(lease, &task.reported()?)
            .await
            .map_err(metadata_error)
    }

    /// Answered from the journal's own service, so a holder that holds none can
    /// still learn what an uncertain settlement committed.
    async fn receipt(
        &self,
        _journal: &Self::Journal,
        job: &JobSpec,
    ) -> Result<Option<JobReceipt>, WorkflowServiceError> {
        self.job_receipt::<AppJournal>(job)
            .await
            .map_err(metadata_error)
    }

    async fn complete(
        &self,
        _journal: &Self::Journal,
        lease: &Self::Lease,
        task: &DeliveredTask,
        execution: WorkflowExecution,
        confirmed: Vec<PayloadConfirmation>,
    ) -> Result<Completed, WorkflowServiceError> {
        let settlement = self
            .settle_execution::<AppJournal>(
                lease.delivery(),
                &ReportedExecution {
                    confirmed,
                    ..ReportedExecution::of(lease, task, execution)?
                },
            )
            .await
            .map_err(metadata_error)?;
        // RECONSTRUCTED, NOT RECEIVED. The journal's own receipt is the logical
        // job this call named and the outcome the journal committed, and the
        // settlement receipt carries that outcome because the far end derived the
        // settlement FROM the receipt. Both fields are ones this exchange already
        // validated, so a second copy on the wire would be a half with nothing to
        // check it against.
        Ok(Completed {
            receipt: JobReceipt {
                job: lease.delivery().job.clone(),
                outcome: settlement.outcome.clone(),
            },
            settlement,
        })
    }
}

/// A merged reply arrived without the half its request asked for.
fn lossy(half: &str) -> WorkflowServiceError {
    WorkflowServiceError::Unavailable(format!("workflow delivery reply carried no {half}"))
}

/// Execution and finalization have separate bounds; renewal never resets either.
#[derive(Debug, Clone, Copy)]
pub struct DeliveryOptions {
    pub execution_timeout: Duration,
    pub operation_timeout: Duration,
    pub retry_delay: Duration,
}

impl DeliveryOptions {
    pub(super) fn validate(self) -> Result<(), WorkflowServiceError> {
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

struct Active<L, J> {
    app: J,
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

impl<L, J> Drop for Active<L, J> {
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
    active: Option<Active<T::Lease, T::Journal>>,
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
        app: &T::Journal,
        policy: &PolicyBinding,
        claimed: Claimed<T::Lease>,
    ) -> Result<DeliveryOutcome, WorkflowServiceError> {
        let Claimed { lease, accepted } = claimed;
        self.drain_interrupted().await;
        // THE CAPTURE SITE, and the only one. `PolicyBinding::authority` captures a
        // FRESH authority; `AppWorkflows::captured_authority` answers with the
        // RETAINED one once `with_authority` has installed it. Same type, different
        // provenance, so they are interchangeable HERE and nowhere after: this runs
        // before anything is scoped, so there is nothing retained to answer with.
        // The scope call below CONSUMES this value rather than capturing again, so a
        // second capture placed after it would be visibly a second capture -- and
        // would take a fresh authority where the retained one decides the attempt's
        // lease budget.
        let authority = policy.authority();
        // The acceptance rode in with the claim. A claimant holding a placement
        // is admitted to creator work alone -- `Claimant::admits` in
        // `zeroship-workflow-manager` pairs each work class with exactly one
        // claimant -- so the journal accepts execution for every operation this
        // slot can be handed, and a claim that reaches here carrying none is a
        // transport that dropped half of its own reply.
        let accepted = accepted.ok_or_else(|| lossy("journal acceptance"))?;
        let task = match accepted {
            JobAcceptance::Deferred => return Ok(DeliveryOutcome::Deferred),
            JobAcceptance::Settled(receipt) => return self.acknowledge(app, *receipt, &lease).await,
            JobAcceptance::Execute(task) => *task,
        };
        let authority = match authority.and_then(|authority| {
            authority.check()?;
            Ok(authority)
        }) {
            Ok(authority) => authority,
            Err(error) => {
                release(self.transport.as_ref(), app, &task, &lease, self.options.operation_timeout).await;
                return Err(error);
            }
        };
        let timeout = authority
            .deadline()
            .map_or(self.options.execution_timeout, |deadline| {
                self.options
                    .execution_timeout
                    .min(deadline.saturating_duration_since(Instant::now()))
            });
        let guard = match ExecutionGuard::new(timeout) {
            Ok(guard) => guard,
            Err(error) => {
                release(self.transport.as_ref(), app, &task, &lease, self.options.operation_timeout).await;
                return Err(error);
            }
        };
        let claims = Claims { lease, task };
        if let Err(error) = guard
            .cancel_on(authority.cancelled())
            .and_then(|()| constrain(&guard, &claims))
        {
            release(
                self.transport.as_ref(),
                app,
                &claims.task,
                &claims.lease,
                self.options.operation_timeout,
            )
            .await;
            return Err(error);
        }
        // The scope site: one call, consuming the authority captured above.
        let scoped = self.transport.scope(app, &authority)?;
        let execution = match self
            .executor
            .start(claims.task.assignment(), guard.budget())
        {
            Ok(execution) => execution,
            Err(error) => {
                release(
                    self.transport.as_ref(),
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
            ExecutionResult::Settled(completed) => Ok(DeliveryOutcome::Settled {
                creator: Box::new(completed.receipt),
                manager: completed.settlement,
            }),
            ExecutionResult::Recovered(receipt) => self.acknowledge(app, *receipt, &lease).await,
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
            release(self.transport.as_ref(), &active.app, &task, &lease, self.options.operation_timeout).await;
        }
        self.active = None;
    }

    async fn acknowledge(
        &self,
        app: &T::Journal,
        receipt: JobReceipt,
        lease: &T::Lease,
    ) -> Result<DeliveryOutcome, WorkflowServiceError> {
        // The receipt this slot reports must belong to the delivery it settles:
        // one that arrived over a wire for another job, or in an outcome family
        // the operation does not admit, is refused before anything is settled.
        receipt.settlement(lease)?;
        let manager = bounded(self.options.operation_timeout, async {
            loop {
                match self.transport.settle(app, lease).await {
                    Ok(observed) => return Ok(observed),
                    Err(error) if retryable(&error) => {
                        compio::time::sleep(self.options.retry_delay).await;
                    }
                    Err(error) => return Err(error),
                }
            }
        })
        .await?;
        // The queue's receipt and the journal's must decide the same outcome. A
        // manager half that settled a different outcome answered a different
        // settlement, so reporting it as this delivery's result would hide the
        // disagreement rather than settle on it.
        agrees(&receipt, &manager)?;
        Ok(DeliveryOutcome::Settled {
            creator: Box::new(receipt),
            manager,
        })
    }
}

enum ExecutionResult {
    /// The execution committed and its outcome settled the delivery, in one
    /// exchange.
    Settled(Box<Completed>),
    /// A receipt the journal already holds, whose manager half is still open.
    Recovered(Box<JobReceipt>),
    Interrupted(ControlIntent),
}

async fn run_active<T: JobTransport>(
    transport: &T,
    active: &mut Active<T::Lease, T::Journal>,
    options: DeliveryOptions,
) -> Result<ExecutionResult, WorkflowServiceError> {
    let execution = CancelOnDrop(active.execution.as_mut());
    let result = {
        let work = execute(
            transport,
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
    match result {
        Ok(Ok(completed)) => Ok(ExecutionResult::Settled(Box::new(completed))),
        Ok(Err(error)) | Err(Err(error)) => {
            if let Ok(Some(receipt)) =
                recover(transport, &active.app, &lease, options.operation_timeout).await
            {
                Ok(ExecutionResult::Recovered(Box::new(receipt)))
            } else {
                let task = active.claims.borrow().task.clone();
                release(transport, &active.app, &task, &lease, options.operation_timeout).await;
                Err(active.phase.failure.borrow().clone().unwrap_or(error))
            }
        }
        Err(Ok(control)) => {
            if let Ok(Some(receipt)) =
                recover(transport, &active.app, &lease, options.operation_timeout).await
            {
                Ok(ExecutionResult::Recovered(Box::new(receipt)))
            } else {
                let task = active.claims.borrow().task.clone();
                release(transport, &active.app, &task, &lease, options.operation_timeout).await;
                Ok(ExecutionResult::Interrupted(control))
            }
        }
    }
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

/// Extend both leases until the run's control intent says to stop.
///
/// ONE EXCHANGE CARRIES BOTH LEASES, SO THIS SIDE READS ITS AUTHORITY BEFORE
/// ASKING AND NOT BETWEEN THE HALVES. `available` is the read: it takes the
/// smaller of the grant's remaining authority and the task's, and refuses to ask
/// at all once either is spent. Once the request is out, both halves commit at the
/// far end before this function resumes.
///
/// WHAT THAT ADMITS, AND WHAT REFUSES IT INSTEAD. The case is a reply that
/// arrives after this process's monotonic view of the grant has run out while the
/// manager's STORED lease deadline has not - transport delay inside the grant
/// rather than past it. Both ends refuse a delivery that is genuinely spent:
/// `heartbeat_authorized` checks the stored deadline with `live`, and the
/// journal's `CapturedLease::capture` refuses a grant with no remaining authority
/// before it opens a transaction and rechecks it around the commit. What this
/// side cannot do is withhold the journal write on its own clock.
async fn renew<T: JobTransport>(
    transport: &T,
    app: &T::Journal,
    claims: &RefCell<Claims<T::Lease>>,
    guard: &ExecutionGuard,
    phase: &Phase,
    options: DeliveryOptions,
) -> Result<ControlIntent, WorkflowServiceError> {
    loop {
        let delay = available(&claims.borrow())?.min(phase.remaining()?) / 3;
        compio::time::sleep(delay).await;
        let (mut task, original) = snapshot(claims);
        let timeout = available(&claims.borrow())?
            .min(options.operation_timeout)
            .min(phase.remaining()?);
        let (task, renewed, control) = bounded(timeout, async {
            let renewed = transport.heartbeat(app, &original, &task).await?;
            if !same_delivery(original.delivery(), renewed.lease.delivery()) {
                return Err(WorkflowServiceError::PermissionDenied);
            }
            let control = renewed.renewal.control();
            task.renew(renewed.renewal);
            Ok((task, renewed.lease, control))
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

async fn execute<T: JobTransport>(
    transport: &T,
    app: &T::Journal,
    claims: &RefCell<Claims<T::Lease>>,
    execution: &mut dyn TaskExecution,
    guard: &ExecutionGuard,
    phase: &Phase,
    options: DeliveryOptions,
) -> Result<Completed, WorkflowServiceError> {
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
    // Read AFTER `wait`, which is when an execution knows what it owes, and
    // before any retry below: the confirmations are the same on every attempt,
    // because a retried settlement confirms the same reservations.
    let confirmed = execution.owed_confirmations();
    // ONE BOUND, BECAUSE ONE CALL: the phase deadline `finalize` just set to
    // `operation_timeout` covers committing the execution and settling the
    // delivery with what that commit decided, retries included.
    //
    // THE RETRY READS THE JOURNAL BEFORE SLEEPING, because an uncertain reply
    // may have committed. A receipt already held for this attempt leaves only
    // the manager half open, and settling it under the same bound is the whole
    // of the recovery.
    bounded(phase.remaining()?, async {
        loop {
            let (task, lease) = snapshot(claims);
            match transport
                .complete(app, &lease, &task, outcome.clone(), confirmed.clone())
                .await
            {
                Ok(completed) => {
                    // The merged half must still agree with the receipt the
                    // journal holds; a transport that answered another
                    // settlement is a contract violation, not this delivery's
                    // result.
                    agrees(&completed.receipt, &completed.settlement)?;
                    return Ok(completed);
                }
                Err(error) if retryable(&error) => {
                    if let Ok(Some(receipt)) = transport.receipt(app, &lease.delivery().job).await {
                        receipt.settlement(&lease)?;
                        let settlement = transport.settle(app, &lease).await?;
                        agrees(&receipt, &settlement)?;
                        return Ok(Completed { settlement, receipt });
                    }
                    compio::time::sleep(options.retry_delay).await;
                }
                Err(error) => return Err(error),
            }
        }
    })
    .await
}

async fn recover<T: JobTransport>(
    transport: &T,
    app: &T::Journal,
    lease: &T::Lease,
    timeout: Duration,
) -> Result<Option<JobReceipt>, WorkflowServiceError> {
    bounded(timeout, transport.receipt(app, &lease.delivery().job)).await
}

async fn release<T: JobTransport>(
    transport: &T,
    app: &T::Journal,
    task: &DeliveredTask,
    lease: &T::Lease,
    timeout: Duration,
) {
    let _ = bounded(timeout, transport.release(app, lease, task)).await;
}

pub(super) async fn bounded<T>(
    timeout: Duration,
    future: impl Future<Output = Result<T, WorkflowServiceError>>,
) -> Result<T, WorkflowServiceError> {
    compio::time::timeout(timeout, Box::pin(future))
        .await
        .map_err(|_| WorkflowServiceError::Timeout)?
}

/// The manager settled the same outcome the journal receipt records.
fn agrees(receipt: &JobReceipt, manager: &SettlementReceipt) -> Result<(), WorkflowServiceError> {
    if manager.outcome != receipt.outcome {
        return Err(WorkflowServiceError::InvalidResponse(
            "workflow settlement receipt disagrees with the journal receipt".into(),
        ));
    }
    Ok(())
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

/// Bind an attempt's authority to an in-process journal.
///
/// The one implementation of the scoping every in-process transport needs, so a
/// transport delegates rather than restating it. Handing back an unscoped journal
/// would take a different policy snapshot's `lease_ms` for the attempt, which is
/// a behaviour change rather than a failure -- so the omission has to be a visible
/// act, and delegating here is what makes it one.
pub fn scope_journal(
    journal: &AppWorkflows,
    authority: &PolicyAuthority,
) -> Result<AppWorkflows, WorkflowServiceError> {
    journal.clone().with_authority(authority.clone())
}

/// The settlement an in-process journal holds for a committed delivery: the
/// outcome of the job's receipt, read now, and no successors.
///
/// The one implementation every transport that holds its journal settles from,
/// so it records exactly what the service records for a crossed transport rather
/// than an outcome its caller hands it.
///
/// # Errors
/// Refuses a job the journal holds no receipt for, a receipt for another job,
/// and the journal's own refusals.
pub async fn committed_settlement(
    journal: &AppWorkflows,
    lease: &impl JobLease,
) -> Result<JournalSettlement, WorkflowServiceError> {
    journal
        .job_receipt(&lease.delivery().job)
        .await?
        .ok_or_else(|| {
            WorkflowServiceError::Conflict("workflow job has no committed receipt".into())
        })?
        .settlement(lease)
        .map_err(WorkflowServiceError::from)
}
