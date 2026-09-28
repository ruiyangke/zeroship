//! Consume manager-issued work through explicitly supplied creator bindings.

#![expect(
    clippy::future_not_send,
    reason = "consumer bindings own compio-local resources"
)]

use crate::{
    delivery::{bounded, DeliveryOptions, DeliveryOutcome, DeliverySlot, JobTransport},
    TaskExecutor,
};
use zeroship_workflow::{
    service::PolicyBinding,
    WorkflowServiceError,
};
use futures::{
    channel::oneshot,
    future::{Either, LocalBoxFuture, Shared},
    stream::FuturesUnordered,
    FutureExt, StreamExt,
};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    future::Future,
    rc::Rc,
    time::{Duration, Instant},
};
use zeroship_core::{
    app_id::AppId,
    workflow_coordination::{AssignedScope, WorkerId},
    workflow_jobs::JobLease,
};

/// An immutable trusted host binding. Clones retain the same local identity.
/// Placement metadata selects work; the app handle separately binds creator I/O.
///
/// An `Rc` wrapper, so cloning works whatever `J` is. The derive would demand
/// `J: Clone`, which neither journal shape needs to be.
pub struct ConsumerScope<J>(Rc<ScopeBinding<J>>);

impl<J> Clone for ConsumerScope<J> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<J> std::fmt::Debug for ConsumerScope<J> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ConsumerScope")
            .field("selection", &self.0.selection)
            .finish_non_exhaustive()
    }
}

struct ScopeBinding<J> {
    /// The journal handle and the placement stay ONE OBJECT.
    ///
    /// Asking a transport for a journal by app id instead would turn an identity
    /// into a lookup, and a lookup answering with another tenant's journal would
    /// answer with a perfectly legal handle. The journal's only separation is its
    /// `app_id` columns, so that is the tenant boundary, and a type parameter is
    /// the price of not putting a fallible lookup on it.
    app: J,
    /// What an attempt captures its authority from, and what names the app this
    /// binding may act for.
    policy: PolicyBinding,
    selection: AssignedScope,
    executor: Rc<dyn TaskExecutor>,
    retired: Cell<bool>,
}

impl<J> ConsumerScope<J> {
    /// The executor must use this app's creator storage and the consumer's worker identity.
    ///
    /// # Errors
    /// Refuses a placement for another app. Only trusted host code supplies bindings.
    pub fn new(
        app: J,
        policy: PolicyBinding,
        selection: AssignedScope,
        executor: Rc<dyn TaskExecutor>,
    ) -> Result<Self, WorkflowServiceError> {
        // COMPARED AGAINST THE POLICY BINDING, not against the journal handle.
        // Every host has a policy binding; a host whose journal is another
        // process's has no handle to compare, so a guard written against one would
        // hold here and pass vacuously there.
        if policy.app_id() != &selection.app_id {
            return Err(WorkflowServiceError::PermissionDenied);
        }
        Ok(Self(Rc::new(ScopeBinding {
            app,
            policy,
            selection,
            executor,
            retired: Cell::new(false),
        })))
    }

    /// Removal, replacement and rejected assignment authority retire this local
    /// binding permanently, including clones retained outside the consumer.
    /// A trusted host must authenticate authority again before constructing a
    /// replacement. Transient transport errors do not retire the binding.
    #[must_use]
    pub fn is_retired(&self) -> bool {
        self.0.retired.get()
    }
}

/// Host bounds, independent of manager scheduling and journal maintenance.
#[derive(Debug, Clone, Copy)]
pub struct ConsumerOptions {
    pub slots: usize,
    pub max_scopes: usize,
    pub idle_poll: Duration,
    pub error_backoff: Duration,
    pub delivery: DeliveryOptions,
}

impl ConsumerOptions {
    fn validate(self) -> Result<(), WorkflowServiceError> {
        self.delivery.validate()?;
        if self.slots == 0
            || self.max_scopes == 0
            || [self.idle_poll, self.error_backoff]
                .into_iter()
                .any(|delay| delay.is_zero() || Instant::now().checked_add(delay).is_none())
        {
            return Err(WorkflowServiceError::InvalidRequest(
                "invalid workflow consumer limits".into(),
            ));
        }
        Ok(())
    }
}

type Stop<'a> = Shared<LocalBoxFuture<'a, ()>>;

struct Scope<J> {
    binding: ConsumerScope<J>,
    revoke: RefCell<Option<oneshot::Sender<()>>>,
    stopped: Stop<'static>,
    claiming: Cell<bool>,
    ready_at: Cell<Instant>,
}

impl<J> Scope<J> {
    fn new(binding: ConsumerScope<J>) -> Self {
        let (revoke, stopped) = oneshot::channel();
        Self {
            binding,
            revoke: RefCell::new(Some(revoke)),
            stopped: async {
                let _ = stopped.await;
            }
            .boxed_local()
            .shared(),
            claiming: Cell::new(false),
            ready_at: Cell::new(Instant::now()),
        }
    }

    fn revoke(&self) {
        self.binding.0.retired.set(true);
        if let Some(revoke) = self.revoke.borrow_mut().take() {
            let _ = revoke.send(());
        }
    }

    fn delay(&self, delay: Duration) {
        // A later failure must not be undone by a simultaneous successful turn.
        self.ready_at
            .set(self.ready_at.get().max(Instant::now() + delay));
    }
}

struct Bindings<J> {
    scopes: BTreeMap<AppId, Rc<Scope<J>>>,
    cursor: Option<AppId>,
    limit: usize,
}

impl<J> Drop for Bindings<J> {
    fn drop(&mut self) {
        for scope in self.scopes.values() {
            scope.revoke();
        }
    }
}

/// Updated by the trusted placement/runtime host on the consumer's compio thread.
/// Removing or replacing a binding interrupts its claims and execution immediately.
pub struct ConsumerBindings<J>(Rc<RefCell<Bindings<J>>>);

/// Hand-written for the same reason as [`ConsumerScope`]'s: the derive would
/// require `J: Clone`, and the shared state is behind an `Rc` regardless.
impl<J> Clone for ConsumerBindings<J> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<J> std::fmt::Debug for ConsumerBindings<J> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ConsumerBindings")
            .field("limit", &self.0.borrow().limit)
            .finish_non_exhaustive()
    }
}

impl<J> ConsumerBindings<J> {
    pub(crate) fn limit(&self) -> usize {
        self.0.borrow().limit
    }

    /// Replace the complete authorized snapshot atomically. Reusing a cloned
    /// `ConsumerScope` keeps its executions; a new binding joins old execution
    /// before its occupied slot may claim again. Other free slots remain usable.
    /// Retired clones stay retired even if removed and later reinserted; renewed
    /// authority requires a newly constructed `ConsumerScope`.
    /// This does not retire the manager's durable recovery responsibility.
    ///
    /// # Errors
    /// Refuses duplicate apps and snapshots exceeding the host bound, without
    /// changing the current snapshot. The caller must authenticate its source.
    pub fn replace(&self, scopes: Vec<ConsumerScope<J>>) -> Result<(), WorkflowServiceError> {
        let mut state = self.0.borrow_mut();
        if scopes.len() > state.limit {
            return Err(WorkflowServiceError::ResourceExhausted(
                "workflow consumer scope capacity exceeded".into(),
            ));
        }
        let mut next = BTreeMap::new();
        for binding in scopes {
            let app = binding.0.selection.app_id.clone();
            if next.contains_key(&app) {
                return Err(WorkflowServiceError::InvalidRequest(
                    "duplicate workflow consumer app".into(),
                ));
            }
            let scope = state
                .scopes
                .get(&app)
                .filter(|old| Rc::ptr_eq(&old.binding.0, &binding.0))
                .cloned()
                .unwrap_or_else(|| Rc::new(Scope::new(binding)));
            next.insert(app, scope);
        }
        for (app, old) in &state.scopes {
            if !next.get(app).is_some_and(|new| Rc::ptr_eq(old, new)) {
                old.revoke();
            }
        }
        state.scopes = next;
        Ok(())
    }

    fn reserve(&self) -> Option<Claim<J>> {
        let mut state = self.0.borrow_mut();
        let now = Instant::now();
        let eligible = |(_, scope): &(&AppId, &Rc<Scope<J>>)| {
            !scope.binding.is_retired() && !scope.claiming.get() && scope.ready_at.get() <= now
        };
        let selected = state
            .scopes
            .iter()
            .filter(eligible)
            .find(|(app, _)| state.cursor.as_ref().is_none_or(|cursor| *app > cursor))
            .or_else(|| state.scopes.iter().find(eligible))
            .map(|(app, scope)| (app.clone(), scope.clone()));
        selected.map(|(app, scope)| {
            state.cursor = Some(app);
            scope.claiming.set(true);
            Claim(scope)
        })
    }
}

struct Claim<J>(Rc<Scope<J>>);
impl<J> Drop for Claim<J> {
    fn drop(&mut self) {
        self.0.claiming.set(false);
    }
}

/// A bounded job consumer with no customer journal discovery or scheduling loop.
/// Each occupied slot includes claim I/O, execution, settlement and joined shutdown.
pub struct JobConsumer<T: JobTransport> {
    transport: Rc<T>,
    worker: WorkerId,
    bindings: ConsumerBindings<T::Journal>,
    options: ConsumerOptions,
    slots: Vec<Option<DeliverySlot<T>>>,
}

impl<T: JobTransport> std::fmt::Debug for JobConsumer<T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("JobConsumer")
            .field("worker", &self.worker)
            .field("options", &self.options)
            .finish_non_exhaustive()
    }
}

impl<T: JobTransport> JobConsumer<T> {
    /// # Errors
    /// Refuses invalid capacity and time bounds. No app is authorized by construction.
    pub fn new(
        transport: Rc<T>,
        worker: WorkerId,
        options: ConsumerOptions,
    ) -> Result<Self, WorkflowServiceError> {
        options.validate()?;
        Ok(Self {
            transport,
            worker,
            bindings: ConsumerBindings(Rc::new(RefCell::new(Bindings {
                scopes: BTreeMap::new(),
                cursor: None,
                limit: options.max_scopes,
            }))),
            slots: (0..options.slots).map(|_| None).collect(),
            options,
        })
    }

    #[must_use]
    pub fn bindings(&self) -> ConsumerBindings<T::Journal> {
        self.bindings.clone()
    }

    /// Consume authorized advance and reconciliation jobs, then join on shutdown.
    /// Dropping this future cancels work but retains occupied slots. Call `drain`
    /// before discarding the consumer; calling this again also drains first.
    pub async fn run_until(&mut self, shutdown: impl Future<Output = ()>) {
        self.drain().await;
        let shutdown = shutdown.boxed_local().shared();
        let mut loops = FuturesUnordered::new();
        for slot in &mut self.slots {
            loops.push(
                run_slot(
                    slot,
                    self.transport.clone(),
                    &self.worker,
                    &self.bindings,
                    self.options,
                    shutdown.clone(),
                )
                .boxed_local(),
            );
        }
        while loops.next().await.is_some() {}
    }

    /// Join interrupted execution before reclaiming any local capacity.
    pub async fn drain(&mut self) {
        futures::future::join_all(
            self.slots
                .iter_mut()
                .flatten()
                .map(DeliverySlot::drain_interrupted),
        )
        .await;
    }
}

async fn run_slot<T: JobTransport>(
    slot: &mut Option<DeliverySlot<T>>,
    transport: Rc<T>,
    worker: &WorkerId,
    bindings: &ConsumerBindings<T::Journal>,
    options: ConsumerOptions,
    shutdown: Stop<'_>,
) {
    loop {
        if shutdown.clone().now_or_never().is_some() {
            break;
        }
        let Some(claim) = bindings.reserve() else {
            if stopped(shutdown.clone(), compio::time::sleep(options.idle_poll))
                .await
                .is_none()
            {
                break;
            }
            continue;
        };
        let scope = claim.0.clone();
        let cancelled = either_stop(shutdown.clone(), scope.stopped.clone());
        let claimed = stopped(
            cancelled.clone(),
            bounded(
                options.delivery.operation_timeout,
                transport.claim(&scope.binding.0.app, &scope.binding.0.selection),
            ),
        )
        .await;
        drop(claim);
        let claimed = match claimed {
            None => continue,
            Some(Ok(Some(claimed))) => claimed,
            Some(Ok(None)) => {
                scope.delay(options.idle_poll);
                continue;
            }
            Some(Err(error)) => {
                if matches!(
                    error,
                    WorkflowServiceError::PermissionDenied
                        | WorkflowServiceError::Unauthenticated
                        | WorkflowServiceError::Conflict(_)
                ) {
                    scope.revoke();
                }
                failed(&scope, options, &error);
                continue;
            }
        };
        let delivery = claimed.lease.delivery();
        if delivery.worker_id != *worker
            || delivery.job.app_id != scope.binding.0.selection.app_id
            || delivery.assignment_revision != scope.binding.0.selection.assignment_revision
        {
            scope.revoke();
            failed(&scope, options, &WorkflowServiceError::PermissionDenied);
            continue;
        }
        if scope.binding.is_retired() {
            continue;
        }
        if claimed
            .lease
            .remaining()
            .is_none_or(|remaining| remaining.is_zero())
        {
            failed(&scope, options, &WorkflowServiceError::Timeout);
            continue;
        }
        let created = DeliverySlot::new(
            transport.clone(),
            scope.binding.0.executor.clone(),
            options.delivery,
        );
        let delivery_slot = match created {
            Ok(created) => slot.insert(created),
            Err(error) => {
                failed(&scope, options, &error);
                continue;
            }
        };
        let result = stopped(cancelled, delivery_slot.run(&scope.binding.0.app, &scope.binding.0.policy, claimed)).await;
        // Even a cancelled run with an unresponsive executor retains this slot.
        delivery_slot.drain_interrupted().await;
        match result {
            Some(Err(error)) => failed(&scope, options, &error),
            Some(Ok(DeliveryOutcome::Deferred | DeliveryOutcome::Interrupted(_))) => {
                scope.delay(options.idle_poll);
            }
            Some(Ok(DeliveryOutcome::Settled { .. })) | None => {}
        }
        // A synchronous metadata implementation must not monopolize the runtime.
        yield_once().await;
    }
    if let Some(slot) = slot {
        slot.drain_interrupted().await;
    }
}

/// Record a consumption failure and back off.
///
/// The REASON is logged beside the code, not just the code. Several distinct
/// refusals share `workflow_unavailable` - a refused lease, a concurrent
/// assignment refresh, and a genuine coordinator outage among them - so a line
/// carrying only the code cannot tell an operator which of them stopped this
/// worker from consuming, and a worker that has stopped consuming looks from
/// outside like runs that simply never start.
fn failed<J>(scope: &Scope<J>, options: ConsumerOptions, error: &WorkflowServiceError) {
    tracing::warn!(
        code = error.code(),
        reason = %error,
        "workflow job consumption failed"
    );
    scope.delay(options.error_backoff);
}

fn either_stop<'a>(left: Stop<'a>, right: Stop<'static>) -> Stop<'a> {
    async move {
        futures::future::select(left, right).await;
    }
    .boxed_local()
    .shared()
}

async fn stopped<T>(stop: Stop<'_>, work: impl Future<Output = T>) -> Option<T> {
    match futures::future::select(stop, work.boxed_local()).await {
        Either::Left(((), work)) => {
            drop(work);
            None
        }
        Either::Right((value, _)) => Some(value),
    }
}

async fn yield_once() {
    let mut yielded = false;
    futures::future::poll_fn(|cx| {
        if yielded {
            std::task::Poll::Ready(())
        } else {
            yielded = true;
            cx.waker().wake_by_ref();
            std::task::Poll::Pending
        }
    })
    .await;
}
