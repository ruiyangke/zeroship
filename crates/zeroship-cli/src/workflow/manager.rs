//! The native workflow manager of the local host, on its own thread.
//!
//! The CLI acts as the trusted platform host for its one app: it records the
//! normal deployment, registers this process as a worker, places the app on
//! it and publishes deployment schedules. Delivery uses the same queue grants,
//! fences and receipts as production; only enrollment and network transport
//! are omitted. The manager thread owns the platform metadata file and runs
//! no app code, just as production keeps the manager out of the creator zone.
//! Thread-local ORM state of app isolates therefore never reaches it.

#![expect(
    clippy::future_not_send,
    reason = "manager ORM handles stay on the manager thread"
)]

use futures::{
    channel::oneshot,
    future::{FutureExt, LocalBoxFuture, Shared},
    StreamExt,
};
use std::{
    future::ready, num::NonZeroU32, path::PathBuf, rc::Rc, thread::JoinHandle, time::Duration,
};
use zeroship_core::{
    app_id::AppId,
    workflow_coordination::{AssignedScope, RegisterWorker, Revision, WorkerId, WorkerState},
    workflow_deployments::{HoldGeneration, HoldReceipt, HoldScope, HoldState},
    workflow_jobs::{Delivery, DeploymentId, JobSpec, Settlement, SettlementReceipt, SubmitJob},
    workflow_schedules::{ActivateSchedules, RegisterSchedules, ScheduleDescriptor},
};
use zeroship_workflow::{
    deployment_holds::DeploymentHoldClient,
    service::publication::JobPublisher,
    WorkflowServiceError,
};
use zeroship_workflow_runner::delivery::JobTransport;
use zeroship_workflow_manager::{
    capacity::LocalCapacity,
    coordinator::{Coordinator, Options as CoordinatorOptions, Placed},
    deployments,
    driver::{Driver, Options as DriverOptions},
    eligibility::{SoleWorker, ZoneId},
    lifecycle::Undeletable,
    local::LocalPlatform,
    recovery::{Options as RecoveryOptions, Recovery},
    scheduling::{Options as SchedulingOptions, Scheduler, SelectedActivation},
    DeliveryGrant, Error, Options as QueueOptions,
};

const MAX_QUEUED_REQUESTS: usize = 64;

/// Local placement and maintenance bounds for the native manager.
#[derive(Debug, Clone, Copy)]
pub struct ManagerOptions {
    pub lease: Duration,
    pub placement_ttl: Duration,
    pub recovery_interval: Duration,
    pub hold_grace: Duration,
    pub lane_timeout: Duration,
    pub driver_interval: Duration,
    pub idle_close: Duration,
    pub closing_timeout: Duration,
    pub closing_backoff: Duration,
    pub closing_backoff_max: Duration,
}

type Request = Box<dyn FnOnce(Rc<LocalManager>) -> LocalBoxFuture<'static, ()> + Send>;

/// A `Send` handle to the manager thread. Each call is one native manager
/// operation; a dropped caller does not cancel an operation already begun.
#[derive(Debug, Clone)]
pub struct ManagerClient {
    requests: flume::Sender<Request>,
    worker: WorkerId,
}

/// Stops and joins the manager thread when dropped.
#[derive(Debug)]
pub struct ManagerThread {
    stop: Option<oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for ManagerThread {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Start the manager over the platform metadata file and mint this process's
/// worker identity. A restarted process never reuses an earlier identity, so a
/// drained predecessor cannot become ready again.
///
/// # Errors
/// Refuses incompatible metadata, invalid bounds and unavailable storage.
pub async fn spawn(
    platform: PathBuf,
    options: ManagerOptions,
) -> Result<(ManagerClient, ManagerThread), WorkflowServiceError> {
    let (requests, receiver) = flume::bounded::<Request>(MAX_QUEUED_REQUESTS);
    let (started, ready) = oneshot::channel();
    let (stop, stopped) = oneshot::channel::<()>();
    let thread = std::thread::Builder::new()
        .name("workflow-manager".into())
        .spawn(move || {
            let Ok(runtime) = compio::runtime::Runtime::new() else {
                let _ = started.send(Err(unavailable()));
                return;
            };
            runtime.block_on(async move {
                let manager = match LocalManager::open(&platform, options).await {
                    Ok(manager) => Rc::new(manager),
                    Err(error) => {
                        let _ = started.send(Err(error));
                        return;
                    }
                };
                let mut driver = match manager.driver() {
                    Ok(driver) => driver,
                    Err(error) => {
                        let _ = started.send(Err(error));
                        return;
                    }
                };
                if started.send(Ok(manager.worker.clone())).is_err() {
                    return;
                }
                let stop = stopped.map(|_| ()).boxed_local().shared();
                let serving = receiver
                    .into_stream()
                    .take_until(stop.clone())
                    .for_each_concurrent(None, |request| request(manager.clone()));
                // In-flight operations and the current maintenance pass finish.
                futures::join!(serving, drive(&mut driver, options.driver_interval, stop));
            });
        })
        .map_err(|_| unavailable())?;
    let handle = ManagerThread {
        stop: Some(stop),
        thread: Some(thread),
    };
    match ready.await {
        Ok(Ok(worker)) => Ok((ManagerClient { requests, worker }, handle)),
        Ok(Err(error)) => Err(error),
        Err(_) => Err(unavailable()),
    }
}

async fn drive(driver: &mut Driver, interval: Duration, stop: Shared<LocalBoxFuture<'_, ()>>) {
    loop {
        if stop.clone().now_or_never().is_some() {
            return;
        }
        // A pass is bounded by its lanes' deadlines and joins before shutdown.
        let report = driver.tick().await;
        for (lane, progress) in report.lanes() {
            if let Some(error) = progress.scan_error {
                tracing::warn!(lane, %error, "workflow manager scan unavailable");
            }
            for failure in progress.failures {
                tracing::warn!(lane, error = %failure.error, "workflow manager candidate retained");
            }
        }
        let sleep = compio::time::sleep(interval).boxed_local();
        if let futures::future::Either::Left(_) = futures::future::select(stop.clone(), sleep).await
        {
            return;
        }
    }
}

/// Platform state owned by the manager thread. It opens no creator database.
struct LocalManager {
    platform: LocalPlatform,
    coordinator: Coordinator,
    scheduler: Scheduler,
    recovery: Recovery,
    worker: WorkerId,
    options: ManagerOptions,
}

impl LocalManager {
    async fn open(
        path: &std::path::Path,
        options: ManagerOptions,
    ) -> Result<Self, WorkflowServiceError> {
        let platform = LocalPlatform::open(path).await.map_err(catalog_error)?;
        let queue = platform
            .queue(QueueOptions {
                lease: options.lease,
                ..QueueOptions::default()
            })
            .await
            .map_err(manager_error)?;
        // The trusted in-process worker shares the host's single zone and
        // performs no enrollment, so the local catalog needs no Control rows.
        // It is also the only live worker: a registration left ready by a
        // process that died is a predecessor, not capacity this host has.
        let worker = WorkerId::mint();
        let coordinator = Coordinator::new(
            queue.clone(),
            CoordinatorOptions {
                worker_ttl: options.placement_ttl,
                assignment_ttl: options.placement_ttl,
                ..CoordinatorOptions::default()
            },
            Rc::new(SoleWorker::new(ZoneId::default_zone(), worker.clone())),
        )
        .map_err(manager_error)?;
        let scheduler =
            Scheduler::new(queue.clone(), SchedulingOptions::default()).map_err(manager_error)?;
        let recovery = Recovery::new(queue, recovery_options(options)).map_err(manager_error)?;
        Ok(Self {
            platform,
            coordinator,
            scheduler,
            recovery,
            worker,
            options,
        })
    }

    /// The local host is its app's only platform authority, so no deletion
    /// can abandon the app's responsibility; idleness still closes it.
    fn driver(&self) -> Result<Driver, WorkflowServiceError> {
        // The in-process worker is the local host's capacity.
        Driver::new(
            self.coordinator.clone(),
            DriverOptions {
                recovery: recovery_options(self.options),
                lane_timeout: self.options.lane_timeout,
                hold_grace: self.options.hold_grace,
                ..DriverOptions::default()
            },
            Rc::new(Undeletable),
            Rc::new(LocalCapacity),
        )
        .map_err(manager_error)
    }

    async fn register(&self, state: WorkerState) -> Result<(), WorkflowServiceError> {
        self.coordinator
            .register(
                &self.worker,
                &RegisterWorker {
                    capacity: NonZeroU32::MIN,
                    state,
                },
            )
            .await
            .map(|_| ())
            .map_err(manager_error)
    }

    /// Register this worker as ready and let the manager place the app on it.
    /// Selection can choose no other worker, because the only other
    /// registration a local catalog can hold is a dead predecessor's and that
    /// is not eligible; an app this process already owns keeps its placement
    /// rather than being given a second revision.
    async fn place_app(&self, app: &AppId) -> Result<AssignedScope, WorkflowServiceError> {
        self.register(WorkerState::Ready).await?;
        let assignment = match self.coordinator.place(app).await.map_err(manager_error)? {
            Placed::Assigned(assignment) => assignment,
            Placed::Owned => self
                .coordinator
                .assignments(&self.worker, None)
                .await
                .map_err(manager_error)?
                .into_iter()
                .find(|assignment| assignment.app_id == *app)
                .ok_or_else(|| manager_error(Error::Denied))?,
            Placed::Unplaced(_) | Placed::Ineligible => return Err(manager_error(Error::Denied)),
        };
        Ok(AssignedScope {
            app_id: assignment.app_id,
            assignment_revision: assignment.revision,
        })
    }

    async fn renew(&self, scope: &AssignedScope) -> Result<(), WorkflowServiceError> {
        self.register(WorkerState::Ready).await?;
        self.coordinator
            .renew(&self.worker, scope)
            .await
            .map(|_| ())
            .map_err(manager_error)
    }

    async fn publish(
        &self,
        app: &AppId,
        deployment: &DeploymentId,
        schedules: Vec<ScheduleDescriptor>,
    ) -> Result<SelectedActivation, WorkflowServiceError> {
        self.scheduler
            .prepare(&RegisterSchedules {
                app_id: app.clone(),
                deployment_id: deployment.clone(),
                schedules,
            })
            .await
            .map_err(manager_error)?;
        let selection = self.scheduler.selection(app).await.map_err(manager_error)?;
        if let Some(activation) = selection
            .as_ref()
            .filter(|selection| selection.enabled)
            .and_then(|selection| selection.activation.as_ref())
            .filter(|activation| &activation.deployment_id == deployment)
        {
            return Ok(activation.clone());
        }
        // The CLI is this app's only platform authority, so the next revision
        // follows the stored selection. A concurrent host activating another
        // deployment at the same revision is refused rather than overwritten.
        let revision = selection
            .map_or(Some(1), |selection| selection.revision.get().checked_add(1))
            .and_then(|revision| Revision::try_from(revision).ok())
            .ok_or_else(|| {
                WorkflowServiceError::ResourceExhausted(
                    "workflow activation revision exhausted".into(),
                )
            })?;
        let job = self
            .scheduler
            .activate(&ActivateSchedules {
                app_id: app.clone(),
                deployment_id: deployment.clone(),
                revision,
            })
            .await
            .map_err(manager_error)?;
        Ok(SelectedActivation {
            job,
            deployment_id: deployment.clone(),
            revision,
            ready: false,
        })
    }

    async fn submit(
        &self,
        scope: &AssignedScope,
        job: &JobSpec,
    ) -> Result<JobSpec, WorkflowServiceError> {
        self.coordinator
            .submit_job(
                &self.worker,
                &SubmitJob {
                    scope: scope.clone(),
                    job: job.clone(),
                },
                || ready(Ok(self.worker.clone())),
            )
            .await
            .map_err(manager_error)
    }
}

pub fn recovery_options(options: ManagerOptions) -> RecoveryOptions {
    RecoveryOptions {
        interval: options.recovery_interval,
        idle_after: options.idle_close,
        closing_timeout: options.closing_timeout,
        closing_backoff: options.closing_backoff,
        closing_backoff_max: options.closing_backoff_max,
        ..RecoveryOptions::default()
    }
}

impl ManagerClient {
    #[must_use]
    pub const fn worker(&self) -> &WorkerId {
        &self.worker
    }

    async fn call<T: Send + 'static>(
        &self,
        operation: impl FnOnce(Rc<LocalManager>) -> LocalBoxFuture<'static, Result<T, WorkflowServiceError>>
            + Send
            + 'static,
    ) -> Result<T, WorkflowServiceError> {
        let (reply, receive) = oneshot::channel();
        let request: Request = Box::new(move |manager| {
            async move {
                let _ = reply.send(operation(manager).await);
            }
            .boxed_local()
        });
        self.requests
            .send_async(request)
            .await
            .map_err(|_| unavailable())?;
        receive.await.map_err(|_| unavailable())?
    }

    /// Register this worker as ready and place the app on it. Local
    /// eligibility is exactly the configured app.
    ///
    /// # Errors
    /// Refuses unavailable storage and conflicting placement records.
    pub async fn place(&self, app: &AppId) -> Result<AssignedScope, WorkflowServiceError> {
        let app = app.clone();
        self.call(move |manager| async move { manager.place_app(&app).await }.boxed_local())
            .await
    }

    /// Extend registration and the current placement. A missing, replaced or
    /// expired placement is refused; the caller must place the app again.
    ///
    /// # Errors
    /// Reports refused placement authority and unavailable storage.
    pub async fn renew(&self, scope: &AssignedScope) -> Result<(), WorkflowServiceError> {
        let scope = scope.clone();
        self.call(move |manager| async move { manager.renew(&scope).await }.boxed_local())
            .await
    }

    /// Replace an expired or refused placement with the next revision.
    ///
    /// # Errors
    /// Refuses conflicting placement records and unavailable storage.
    pub async fn replace(
        &self,
        previous: &AssignedScope,
    ) -> Result<AssignedScope, WorkflowServiceError> {
        let previous = previous.clone();
        self.call(move |manager| {
            async move { manager.place_app(&previous.app_id).await }.boxed_local()
        })
        .await
    }

    /// Report terminal draining so nothing new is placed on this process.
    /// Durable work and recovery responsibility remain.
    ///
    /// # Errors
    /// Reports unavailable storage.
    pub async fn drain(&self) -> Result<(), WorkflowServiceError> {
        self.call(|manager| {
            async move { manager.register(WorkerState::Draining).await }.boxed_local()
        })
        .await
    }

    /// Record an ingested normal deployment in the platform catalog.
    ///
    /// # Errors
    /// Refuses invalid manifests, reclaimed deployments and unavailable storage.
    pub async fn record(
        &self,
        app: &AppId,
        hash: String,
        manifest: String,
    ) -> Result<DeploymentId, WorkflowServiceError> {
        let app = app.clone();
        self.call(move |manager| {
            async move {
                let id = manager
                    .platform
                    .deployments()
                    .record_deployment(&app, &hash, &manifest)
                    .await
                    .map_err(catalog_error)?;
                DeploymentId::parse(&id).map_err(|_| {
                    WorkflowServiceError::Internal("invalid local deployment identity".into())
                })
            }
            .boxed_local()
        })
        .await
    }

    /// Publish a deployment's input-free schedules and select it for new
    /// work, unless it is already the enabled selection. The returned
    /// Activation job delivers creator readiness; existing runs keep their
    /// pinned code.
    ///
    /// # Errors
    /// Refuses changed schedule metadata, stale revisions and unavailable storage.
    pub async fn publish(
        &self,
        app: &AppId,
        deployment: &DeploymentId,
        schedules: Vec<ScheduleDescriptor>,
    ) -> Result<SelectedActivation, WorkflowServiceError> {
        let (app, deployment) = (app.clone(), deployment.clone());
        self.call(move |manager| {
            async move { manager.publish(&app, &deployment, schedules).await }.boxed_local()
        })
        .await
    }

    /// The app's currently selected activation, if a deployment was published.
    ///
    /// # Errors
    /// Reports malformed scheduling metadata and unavailable storage.
    pub async fn selected(
        &self,
        app: &AppId,
    ) -> Result<Option<SelectedActivation>, WorkflowServiceError> {
        let app = app.clone();
        self.call(move |manager| {
            async move {
                Ok(manager
                    .scheduler
                    .selection(&app)
                    .await
                    .map_err(manager_error)?
                    .and_then(|selection| selection.activation))
            }
            .boxed_local()
        })
        .await
    }

    /// Establish reconciliation and collection responsibility under the
    /// selected activation's provenance. Repeating it preserves deadlines.
    ///
    /// # Errors
    /// Refuses stale provenance and unavailable storage.
    pub async fn ensure_recovery(
        &self,
        app: &AppId,
        activation: &SelectedActivation,
    ) -> Result<(), WorkflowServiceError> {
        let (app, deployment, revision) = (
            app.clone(),
            activation.deployment_id.clone(),
            activation.revision,
        );
        self.call(move |manager| {
            async move {
                manager
                    .recovery
                    .ensure(&app, &deployment, revision)
                    .await
                    .map_err(manager_error)
            }
            .boxed_local()
        })
        .await
    }

    /// Establish an open ingress epoch above `after`, committed before it is
    /// returned. The local host is its app's platform authority, so its own
    /// policy's admission decides establishment in place of a policy lease.
    ///
    /// # Errors
    /// Refuses an app without activated responsibility, an epoch the manager
    /// never issued, disabled admission and unavailable storage.
    pub async fn establish(
        &self,
        app: &AppId,
        after: Option<Revision>,
        admission: bool,
    ) -> Result<Revision, WorkflowServiceError> {
        let app = app.clone();
        self.call(move |manager| {
            async move {
                manager
                    .recovery
                    .establish(&app, after, admission)
                    .await
                    .map_err(manager_error)
            }
            .boxed_local()
        })
        .await
    }

    /// Report ingress the host accepted since its previous report, keeping
    /// the app's responsibility from closing as idle.
    ///
    /// # Errors
    /// Reports unavailable storage.
    pub async fn note_ingress(&self, app: &AppId) -> Result<(), WorkflowServiceError> {
        let app = app.clone();
        self.call(move |manager| {
            async move {
                manager
                    .recovery
                    .note_ingress(&app)
                    .await
                    .map_err(manager_error)
            }
            .boxed_local()
        })
        .await
    }

    async fn hold(
        &self,
        scope: &HoldScope,
        deployment: &str,
        generation: HoldGeneration,
        state: HoldState,
    ) -> Result<HoldReceipt, WorkflowServiceError> {
        let (scope, deployment) = (scope.clone(), deployment.to_owned());
        self.call(move |manager| {
            async move {
                let ledger = manager.platform.deployments();
                match state {
                    HoldState::Held => ledger.acquire(&scope, &deployment, generation).await,
                    HoldState::Released => ledger.release(&scope, &deployment, generation).await,
                }
                .map_err(catalog_error)
            }
            .boxed_local()
        })
        .await
    }

    async fn submit(
        &self,
        scope: &AssignedScope,
        job: &JobSpec,
    ) -> Result<JobSpec, WorkflowServiceError> {
        let (scope, job) = (scope.clone(), job.clone());
        self.call(move |manager| async move { manager.submit(&scope, &job).await }.boxed_local())
            .await
    }

    /// Delivery operations for this worker. Each settled delivery wakes the
    /// host's publication of creator intents committed by that job. The local
    /// host is its app's platform authority, so its own configured policy
    /// supplies the delivery ceiling in place of a policy lease.
    #[must_use]
    pub fn transport(
        &self,
        settled: flume::Sender<()>,
        max_delivery_attempts: i64,
    ) -> LocalTransport {
        LocalTransport {
            client: self.clone(),
            settled,
            max_delivery_attempts,
        }
    }

    /// Submission of committed creator intents under one placement revision.
    #[must_use]
    pub const fn publisher(&self, scope: AssignedScope) -> LocalPublisher<'_> {
        LocalPublisher {
            client: self,
            scope,
        }
    }

    /// Journal-holder retention for the creator engine of `app`.
    #[must_use]
    pub fn journal_holds(&self, app: &AppId) -> JournalHolds {
        JournalHolds {
            client: self.clone(),
            scope: HoldScope::for_app(app.clone()),
        }
    }
}

/// Native coordinator delivery for the trusted local worker. Grants are the
/// queue's own monotonic leases; placement is rechecked inside each queue
/// transaction exactly as for an authenticated remote worker.
#[derive(Debug)]
pub struct LocalTransport {
    client: ManagerClient,
    settled: flume::Sender<()>,
    max_delivery_attempts: i64,
}

impl JobTransport for LocalTransport {
    type Lease = DeliveryGrant;

    async fn claim(
        &self,
        scope: &AssignedScope,
    ) -> Result<Option<DeliveryGrant>, WorkflowServiceError> {
        let scope = scope.clone();
        let ceiling = self.max_delivery_attempts;
        self.client
            .call(move |manager| {
                async move {
                    manager
                        .coordinator
                        .claim_job(&manager.worker, &scope, Ok(ceiling), || {
                            ready(Ok(manager.worker.clone()))
                        })
                        .await
                        .map_err(manager_error)
                }
                .boxed_local()
            })
            .await
    }

    async fn submit(
        &self,
        scope: &AssignedScope,
        job: &JobSpec,
    ) -> Result<JobSpec, WorkflowServiceError> {
        self.client.submit(scope, job).await
    }

    async fn heartbeat(
        &self,
        lease: &DeliveryGrant,
    ) -> Result<DeliveryGrant, WorkflowServiceError> {
        let delivery: Delivery = lease.delivery().clone();
        self.client
            .call(move |manager| {
                async move {
                    manager
                        .coordinator
                        .heartbeat_job(&manager.worker, &delivery, || {
                            ready(Ok(manager.worker.clone()))
                        })
                        .await
                        .map_err(manager_error)
                }
                .boxed_local()
            })
            .await
    }

    async fn settle(
        &self,
        settlement: &Settlement,
    ) -> Result<SettlementReceipt, WorkflowServiceError> {
        let settlement = settlement.clone();
        let receipt = self
            .client
            .call(move |manager| {
                async move {
                    manager
                        .coordinator
                        .settle_job(&manager.worker, &settlement, || {
                            ready(Ok(manager.worker.clone()))
                        })
                        .await
                        .map_err(manager_error)
                }
                .boxed_local()
            })
            .await?;
        // Settled work may have committed successor intents in the creator
        // outbox. A full channel already holds a pending wake.
        let _ = self.settled.try_send(());
        Ok(receipt)
    }
}

/// Publishes creator intents under one placement revision.
#[derive(Debug)]
pub struct LocalPublisher<'a> {
    client: &'a ManagerClient,
    scope: AssignedScope,
}

impl JobPublisher for LocalPublisher<'_> {
    fn app_id(&self) -> &AppId {
        &self.scope.app_id
    }

    async fn submit(&self, job: &JobSpec) -> Result<JobSpec, WorkflowServiceError> {
        self.client.submit(&self.scope, job).await
    }
}

/// The creator engine's journal holds on the local deployment catalog.
/// The holder scope is fixed to this app; the queue's holds use another scope.
#[derive(Debug, Clone)]
pub struct JournalHolds {
    client: ManagerClient,
    scope: HoldScope,
}

#[async_trait::async_trait(?Send)]
impl DeploymentHoldClient for JournalHolds {
    fn scope(&self) -> &HoldScope {
        &self.scope
    }

    async fn acquire(
        &self,
        deployment: &str,
        generation: HoldGeneration,
    ) -> Result<HoldReceipt, WorkflowServiceError> {
        self.client
            .hold(&self.scope, deployment, generation, HoldState::Held)
            .await
    }

    async fn release(
        &self,
        deployment: &str,
        generation: HoldGeneration,
    ) -> Result<HoldReceipt, WorkflowServiceError> {
        self.client
            .hold(&self.scope, deployment, generation, HoldState::Released)
            .await
    }
}

fn unavailable() -> WorkflowServiceError {
    WorkflowServiceError::Unavailable("local workflow manager is unavailable".into())
}

pub fn manager_error(error: Error) -> WorkflowServiceError {
    match error {
        Error::Invalid => {
            WorkflowServiceError::InvalidRequest("invalid workflow manager request".into())
        }
        Error::Denied => WorkflowServiceError::PermissionDenied,
        Error::Conflict => {
            WorkflowServiceError::Conflict("workflow manager metadata is no longer current".into())
        }
        Error::Capacity => {
            WorkflowServiceError::ResourceExhausted("workflow manager capacity exhausted".into())
        }
        Error::Timeout => WorkflowServiceError::Timeout,
        Error::Unavailable => {
            WorkflowServiceError::Unavailable("workflow manager storage is unavailable".into())
        }
        Error::Storage => {
            WorkflowServiceError::Internal("workflow manager storage contract failed".into())
        }
    }
}

pub fn catalog_error(error: deployments::Error) -> WorkflowServiceError {
    use deployments::Error;
    match error {
        Error::InvalidRequest(message) => WorkflowServiceError::InvalidRequest(message),
        Error::Unauthenticated => WorkflowServiceError::Unauthenticated,
        Error::PermissionDenied => WorkflowServiceError::PermissionDenied,
        Error::Conflict(message) => WorkflowServiceError::Conflict(message),
        Error::ResourceExhausted(message) => WorkflowServiceError::ResourceExhausted(message),
        Error::Unavailable(message) => WorkflowServiceError::Unavailable(message),
        Error::Timeout => WorkflowServiceError::Timeout,
        Error::Internal(message) => WorkflowServiceError::Internal(message),
    }
}
