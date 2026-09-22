//! Runtime-local lifecycle for an enrolled worker consuming manager-owned jobs.

#![expect(
    clippy::future_not_send,
    reason = "worker lifecycle futures retain their owning compio resources"
)]

use crate::{
    assignments::{AssignmentBindings, AssignmentOptions, CreatorFactory},
    consumer::{ConsumerOptions, JobConsumer},
    publication::{self, HostTransport},
    ready::ReadyApps,
};
use zeroship_workflow::{service::HostPolicies, WorkflowServiceError};
use futures::{future::Either, FutureExt};
use std::{
    future::Future,
    num::NonZeroU32,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};
use zeroship_core::workflow_coordination::{FailureCode, RegisterWorker, WorkerState};
use zeroship_workflow_client::{Error, WorkerCoordinator};

/// Local capacity and refresh cadence.
///
/// Each loop waits after its operation, so delayed work never produces a
/// catch-up burst. Remote grants keep their own
/// original deadlines, independently of these refresh settings.
#[derive(Clone, Copy, Debug)]
pub struct HostOptions {
    pub consumer: ConsumerOptions,
    pub assignments: AssignmentOptions,
    pub registration_interval: Duration,
    pub assignment_interval: Duration,
    pub policy_interval: Duration,
}

/// Stack reserved for the thread a workflow host runs on.
///
/// The host thread carries ONE unbroken call chain: the coordination lanes,
/// the delivered-job path, the journal engine, the ORM and the database
/// driver, plus the creator's own start path when applying a frontier that
/// spawns a child. It also enters V8 to run creator workflow bodies, and V8
/// derives its own limit from whatever is left. The platform default thread
/// stack is not a budget anyone chose for that chain, and overrunning it
/// aborts the entire process rather than failing one job - every app placed on
/// the worker goes down with it. Reserved address space is not resident
/// memory: pages commit only as the chain touches them.
pub const STACK_BYTES: usize = 16 * 1024 * 1024;

/// The thread a workflow host runs on, named for operators and carrying the
/// stack its call chain needs. Every host spawner uses this, so the name and
/// the budget are declared once.
pub fn thread() -> std::thread::Builder {
    std::thread::Builder::new()
        .name("workflow-host".into())
        .stack_size(STACK_BYTES)
}

/// Owns one enrolled process identity and its joined execution capacity.
///
/// The injected factory supplies independently authorized creator resources.
/// This host registers liveness, refreshes authorized placements, publishes
/// each prepared app's backend to request threads through [`ReadyApps`],
/// promptly submits intents those backends and settled deliveries commit, and
/// consumes delivered jobs. It does not decide placement, scan journals for
/// work or open the Control database. Registration is not an enrollment
/// bootstrap.
pub struct WorkerHost<F> {
    client: WorkerCoordinator,
    assignments: AssignmentBindings<F>,
    consumer: JobConsumer<HostTransport>,
    options: HostOptions,
    capacity: NonZeroU32,
    started: bool,
}

impl<F> std::fmt::Debug for WorkerHost<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerHost")
            .field("worker", self.client.worker_id())
            .field("options", &self.options)
            .field("started", &self.started)
            .finish_non_exhaustive()
    }
}

impl<F> WorkerHost<F> {
    /// The manager's advertised capacity counts app placements, while the
    /// consumer's slots independently bound concurrent execution. `ready` is
    /// the registry request threads resolve app backends from; this host is
    /// its only writer.
    ///
    /// # Errors
    /// Rejects invalid capacity, operation bounds and refresh intervals.
    pub fn new(
        client: WorkerCoordinator,
        policies: Arc<HostPolicies>,
        factory: F,
        ready: ReadyApps,
        options: HostOptions,
    ) -> Result<Self, WorkflowServiceError> {
        let capacity = u32::try_from(options.assignments.max_scopes)
            .ok()
            .and_then(NonZeroU32::new)
            .ok_or_else(invalid_options)?;
        if [
            options.registration_interval,
            options.assignment_interval,
            options.policy_interval,
        ]
        .into_iter()
        .any(|interval| interval.is_zero() || Instant::now().checked_add(interval).is_none())
        {
            return Err(invalid_options());
        }
        let (wake, marked) = publication::channel();
        let consumer = JobConsumer::new(
            Rc::new(HostTransport {
                client: client.clone(),
                settled: wake.clone(),
            }),
            client.worker_id().clone(),
            options.consumer,
        )?;
        let assignments = AssignmentBindings::with_publication(
            client.clone(),
            policies,
            consumer.bindings(),
            factory,
            ready,
            options.assignments,
            (wake, marked),
        )?;
        Ok(Self {
            client,
            assignments,
            consumer,
            options,
            capacity,
            started: false,
        })
    }

    /// Permanently stop admission, announce draining and join occupied slots.
    /// This also finishes shutdown after a cancelled `run_until` or `drain`.
    /// A slow executor retains its capacity until its native shutdown completes.
    ///
    /// # Errors
    /// Reports local revocation or manager registration failure after joining
    /// execution. Manager outage does not turn draining back into ready.
    pub async fn drain(&mut self) -> Result<(), WorkflowServiceError> {
        self.started = true;
        drain(
            &self.client,
            &self.assignments,
            &mut self.consumer,
            self.capacity,
        )
        .await
    }
}

impl<F: CreatorFactory> WorkerHost<F> {
    /// Register before discovering assignments, then drive independent liveness,
    /// placement, policy and consumer loops until shutdown or identity refusal.
    ///
    /// This invocation is terminal even when cancelled during startup. Dropping
    /// it revokes local bindings and cancels execution, but occupied slots remain
    /// owned by this host: call `drain` to join before discarding their capacity.
    /// No assignment release or recovery completion is fabricated on shutdown.
    ///
    /// # Errors
    /// Refuses reuse of the process lifecycle. Reports permanent registration
    /// refusal or shutdown failure after joining execution; transient metadata
    /// outages retry while existing authority retains its original expiry.
    pub async fn run_until(
        &mut self,
        shutdown: impl Future<Output = ()>,
    ) -> Result<(), WorkflowServiceError> {
        if self.started {
            return Err(WorkflowServiceError::Conflict(
                "workflow worker lifecycle has already started or drained".into(),
            ));
        }
        self.started = true;
        let _retire_on_drop = RetireOnDrop(&self.assignments);
        let outcome = {
            let work = drive(
                &self.client,
                &self.assignments,
                &mut self.consumer,
                self.capacity,
                self.options,
            );
            match futures::future::select(shutdown.boxed_local(), work.boxed_local()).await {
                Either::Left(((), work)) => {
                    drop(work);
                    Ok(())
                }
                Either::Right((result, shutdown)) => {
                    drop(shutdown);
                    result
                }
            }
        };
        // Pending Ready and refresh operations are dropped before Draining.
        // The manager's terminal tombstone fences an uncertain earlier commit.
        let drained = drain(
            &self.client,
            &self.assignments,
            &mut self.consumer,
            self.capacity,
        )
        .await;
        if let Err(error) = &drained {
            tracing::warn!(worker = %self.client.worker_id().as_str(), code = error.code(), "workflow worker drain failed");
        }
        outcome.and(drained)
    }
}

struct RetireOnDrop<'a, F>(&'a AssignmentBindings<F>);
impl<F> Drop for RetireOnDrop<'_, F> {
    fn drop(&mut self) {
        let _ = self.0.close();
    }
}

async fn drive<F: CreatorFactory>(
    client: &WorkerCoordinator,
    assignments: &AssignmentBindings<F>,
    consumer: &mut JobConsumer<HostTransport>,
    capacity: NonZeroU32,
    options: HostOptions,
) -> Result<(), WorkflowServiceError> {
    ready(client, capacity, options.registration_interval).await?;
    let registration = async {
        loop {
            compio::time::sleep(options.registration_interval).await;
            ready(client, capacity, options.registration_interval).await?;
        }
    };
    let publication = async {
        loop {
            let apps = assignments.marked().await;
            assignments.publish_marked(apps).await;
        }
    };
    let work = async {
        futures::join!(
            periodic(options.assignment_interval, async || assignments
                .reconcile()
                .await),
            periodic(options.policy_interval, async || assignments
                .refresh()
                .await),
            publication,
            consumer.run_until(std::future::pending()),
        );
    };
    match futures::future::select(registration.boxed_local(), work.boxed_local()).await {
        Either::Left((result, work)) => {
            drop(work);
            result
        }
        Either::Right(((), registration)) => {
            drop(registration);
            Err(WorkflowServiceError::Unavailable(
                "workflow worker loops stopped unexpectedly".into(),
            ))
        }
    }
}

async fn periodic(
    interval: Duration,
    operation: impl AsyncFn() -> Result<(), WorkflowServiceError>,
) {
    loop {
        if let Err(error) = operation().await {
            // The code alone cannot tell a manager outage from a retired
            // binding or an unreachable creator resource: every one of them is
            // unavailable. The message names which refusal this was.
            tracing::warn!(
                code = error.code(),
                %error,
                "workflow worker binding refresh failed"
            );
        }
        compio::time::sleep(interval).await;
    }
}

async fn ready(
    client: &WorkerCoordinator,
    capacity: NonZeroU32,
    interval: Duration,
) -> Result<(), WorkflowServiceError> {
    loop {
        match client
            .register(&RegisterWorker {
                capacity,
                state: WorkerState::Ready,
            })
            .await
        {
            Ok(_) => return Ok(()),
            Err(
                error @ (Error::Unavailable
                | Error::Timeout
                | Error::Refused(FailureCode::Unavailable)),
            ) => {
                tracing::warn!(worker = %client.worker_id().as_str(), %error, "workflow worker registration unavailable");
                compio::time::sleep(interval).await;
            }
            Err(error) => return Err(registration_error(error)),
        }
    }
}

async fn drain<F>(
    client: &WorkerCoordinator,
    assignments: &AssignmentBindings<F>,
    consumer: &mut JobConsumer<HostTransport>,
    capacity: NonZeroU32,
) -> Result<(), WorkflowServiceError> {
    let closed = assignments.close();
    let request = RegisterWorker {
        capacity,
        state: WorkerState::Draining,
    };
    // The HTTP client bounds this exchange; native shutdown is always joined,
    // even if the manager cannot observe the terminal registration.
    let (registered, ()) = futures::join!(client.register(&request), consumer.drain());
    closed?;
    registered.map(|_| ()).map_err(registration_error)
}

fn invalid_options() -> WorkflowServiceError {
    WorkflowServiceError::InvalidRequest("invalid workflow worker host bounds".into())
}

fn registration_error(error: Error) -> WorkflowServiceError {
    match error {
        Error::InvalidConfig | Error::Refused(FailureCode::Invalid) => {
            WorkflowServiceError::InvalidRequest("invalid workflow worker registration".into())
        }
        Error::Unauthenticated | Error::Refused(FailureCode::Unauthenticated) => {
            WorkflowServiceError::Unauthenticated
        }
        Error::Refused(FailureCode::Denied) => WorkflowServiceError::PermissionDenied,
        Error::Refused(FailureCode::Conflict) => {
            WorkflowServiceError::Conflict("workflow worker registration refused".into())
        }
        Error::Timeout => WorkflowServiceError::Timeout,
        Error::Refused(FailureCode::Capacity) => {
            WorkflowServiceError::ResourceExhausted("workflow worker registration is full".into())
        }
        Error::RequestTooLarge
        | Error::ResponseTooLarge
        | Error::Refused(FailureCode::RequestTooLarge) => WorkflowServiceError::PayloadTooLarge,
        Error::InvalidResponse | Error::Unavailable | Error::Refused(FailureCode::Unavailable) => {
            WorkflowServiceError::Unavailable("workflow worker registration failed".into())
        }
    }
}

#[cfg(test)]
mod tests;
