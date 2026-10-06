//! The native workflow manager of the local host, on its own thread.
//!
//! The CLI acts as the trusted platform host for its one app: it records the
//! normal deployment and publishes deployment schedules. Delivery uses the same queue grants,
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
use std::{future::ready, path::PathBuf, rc::Rc, thread::JoinHandle, time::Duration};
use zeroship_core::{
    app_id::AppId,
    workflow_coordination::{Revision, WorkerId},
    workflow_deployments::{HoldGeneration, HoldReceipt, HoldScope, HoldState},
    workflow_jobs::{
        ClaimJobs, Delivery, DeploymentId, JobSpec, JournalSettlement, SettlementReceipt,
    },
    workflow_schedules::{ActivateSchedules, RegisterSchedules, ScheduleDescriptor},
    zone_id::ZoneId,
};
use zeroship_workflow::{
    deployment_holds::DeploymentHoldClient,
    service::{delivery::DeliveredTask, publication::JobPublisher, AppWorkflows},
    WorkflowExecution, WorkflowServiceError,
};
use zeroship_workflow_runner::delivery::{
    committed_settlement, Claimed, ClaimedBatch, Completed, JobTransport, Renewed, Unstarted,
};
use zeroship_workflow_manager::{
    capacity::LocalCapacity,
    coordinator::{Admission, Coordinator, Options as CoordinatorOptions, ZoneClaim},
    deployments,
    driver::{Driver, Options as DriverOptions},
    lifecycle::Undeletable,
    local::{ConfiguredPolicies, LocalPlatform},
    maintenance::MaintenanceAuthority,
    recovery::{Options as RecoveryOptions, Recovery},
    scheduling::{Options as SchedulingOptions, Scheduler, SelectedActivation},
    DeliveryGrant, Error, Options as QueueOptions,
};

const MAX_QUEUED_REQUESTS: usize = 64;

/// Local claim and maintenance bounds for the native manager.
#[derive(Debug, Clone, Copy)]
pub struct ManagerOptions {
    pub lease: Duration,
    pub max_attempt: Duration,
    pub claim_budget: Duration,
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
    app: AppId,
    policy: zeroship_core::workflow_policy::AppPolicy,
    options: ManagerOptions,
) -> Result<(ManagerClient, ManagerThread), WorkflowServiceError> {
    let (requests, receiver) = flume::bounded::<Request>(MAX_QUEUED_REQUESTS);
    let (started, ready) = oneshot::channel();
    let (stop, stopped) = oneshot::channel::<()>();
    let thread = std::thread::Builder::new()
        .name("workflow-manager".into())
        .spawn(move || {
            let runtime =
                match compio::runtime::Runtime::new().map_err(zeroship_memlock::explain) {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = started.send(Err(WorkflowServiceError::Unavailable(format!(
                            "local workflow manager is unavailable: {error}"
                        ))));
                        return;
                    }
                };
            runtime.block_on(async move {
                let manager = match LocalManager::open(&platform, app, policy, options).await {
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
    policies: ConfiguredPolicies,
    worker: WorkerId,
    options: ManagerOptions,
}

impl LocalManager {
    async fn open(
        path: &std::path::Path,
        app: AppId,
        policy: zeroship_core::workflow_policy::AppPolicy,
        options: ManagerOptions,
    ) -> Result<Self, WorkflowServiceError> {
        let platform = LocalPlatform::open(path).await.map_err(catalog_error)?;
        let queue = platform
            .queue(queue_options(options))
            .await
            .map_err(manager_error)?;
        let worker = WorkerId::mint();
        let coordinator = Coordinator::new(
            queue.clone(),
            CoordinatorOptions {
                claim_budget: options.claim_budget,
                ..CoordinatorOptions::default()
            },
        )
        .map_err(manager_error)?;
        let policies = ConfiguredPolicies::new(app, policy).map_err(manager_error)?;
        let scheduler =
            Scheduler::new(queue.clone(), SchedulingOptions::default()).map_err(manager_error)?;
        let recovery = Recovery::new(queue, recovery_options(options)).map_err(manager_error)?;
        Ok(Self {
            platform,
            coordinator,
            scheduler,
            recovery,
            policies,
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
            Rc::new(self.policies.clone()),
            Rc::new(LocalCapacity),
        )
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
                execution_zone_id: ZoneId::default_zone(),
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
                execution_zone_id: ZoneId::default_zone(),
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

    /// Publish a creator intent through the same trusted queue submission the
    /// workflow service uses. The local host is its app's platform authority,
    /// so nothing rechecks the app's zone here; the publisher asserts the app.
    ///
    /// # Errors
    /// Refuses reused identities with different content and unavailable storage.
    async fn submit(&self, job: &JobSpec) -> Result<JobSpec, WorkflowServiceError> {
        self.coordinator
            .queue()
            .submit(job)
            .await
            .map_err(manager_error)
    }

    /// The authority this process asserts over the maintenance rows of `app`'s
    /// queue.
    ///
    /// The identity is this process's own worker id rather than a second minted
    /// one: the local host is both the worker that claims its app's jobs and
    /// the service that owns the journal, so one process leaves one identity on
    /// every row it leases.
    /// The two lanes stay apart by the kinds their claimants admit, not by whose
    /// name is on the row.
    fn maintenance(&self, app: &AppId) -> MaintenanceAuthority {
        MaintenanceAuthority::new(app.clone(), self.worker.clone())
    }
}

/// The bounds of the queue the local manager opens.
#[must_use]
pub fn queue_options(options: ManagerOptions) -> QueueOptions {
    QueueOptions {
        lease: options.lease,
        max_attempt: options.max_attempt,
        ..QueueOptions::default()
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
                    .ensure(&app, &ZoneId::default_zone(), &deployment, revision)
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

    async fn submit(&self, job: &JobSpec) -> Result<JobSpec, WorkflowServiceError> {
        let job = job.clone();
        self.call(move |manager| async move { manager.submit(&job).await }.boxed_local())
            .await
    }

    /// Delivery operations for this worker. The local host is its app's
    /// platform authority, so its own configured policy supplies the delivery
    /// ceiling in place of a policy lease. A committed delivery wakes the
    /// host's publication through the journal handle's own publication hint,
    /// not through this transport.
    #[must_use]
    pub fn transport(&self, journal: AppWorkflows) -> LocalTransport {
        LocalTransport {
            client: self.clone(),
            journal,
        }
    }

    /// The queue half of this process's journal maintenance, for `app`. The
    /// ceiling is the host's own configured policy, exactly as for delivery.
    #[must_use]
    pub fn sweeps(&self, app: AppId, max_delivery_attempts: i64) -> LocalSweeps {
        LocalSweeps {
            client: self.clone(),
            app,
            max_delivery_attempts,
        }
    }

    /// Submission of committed creator intents for the configured app.
    #[must_use]
    pub const fn publisher<'a>(&'a self, app: &'a AppId) -> LocalPublisher<'a> {
        LocalPublisher {
            client: self,
            app,
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

/// The queue half of the local host's journal maintenance lane.
///
/// `Claimant::Worker` admits only `advance`, so the consumer's transport below
/// never sees a sweep. This process holds the journal those sweeps maintain and
/// the payload store behind it, so it asserts maintenance authority over its own
/// queue exactly as the workflow service's lane does.
///
/// It is a pair of calls rather than that service's `MaintenanceLane` because
/// the lane holds the queue and the journal on one runtime, and this process
/// keeps them on two threads: the queue is the manager thread's, the journal is
/// the host thread's, and the exchange crosses between them the way delivery
/// already does.
#[derive(Debug, Clone)]
pub struct LocalSweeps {
    client: ManagerClient,
    app: AppId,
    max_delivery_attempts: i64,
}

impl LocalSweeps {
    /// Take the next maintenance row of this app's queue, if it has one.
    ///
    /// # Errors
    /// Reports a refused claim, exhausted attempt numbering and unavailable
    /// manager storage.
    pub async fn claim(&self) -> Result<Option<DeliveryGrant>, WorkflowServiceError> {
        let (app, ceiling) = (self.app.clone(), self.max_delivery_attempts);
        self.client
            .call(move |manager| {
                async move {
                    manager
                        .maintenance(&app)
                        .claim(manager.coordinator.queue(), Ok(ceiling))
                        .await
                        .map_err(manager_error)
                }
                .boxed_local()
            })
            .await
    }

    /// Discharge a delivery this lane claimed, with what its operation committed.
    ///
    /// # Errors
    /// Refuses a lapsed lease and a conflicting settlement, and reports
    /// unavailable manager storage.
    pub async fn settle(
        &self,
        settlement: &JournalSettlement,
    ) -> Result<SettlementReceipt, WorkflowServiceError> {
        let (app, settlement) = (self.app.clone(), settlement.clone());
        self.client
            .call(move |manager| {
                async move {
                    manager
                        .maintenance(&app)
                        .settle(manager.coordinator.queue(), &settlement)
                        .await
                        .map_err(manager_error)
                }
                .boxed_local()
            })
            .await
    }
}

/// Native coordinator delivery for the trusted local worker.
#[derive(Debug)]
pub struct LocalTransport {
    client: ManagerClient,
    journal: AppWorkflows,
}

impl JobTransport for LocalTransport {
    type Lease = DeliveryGrant;
    /// This host holds the journal, so an attempt is scoped here rather than
    /// server-side.
    type Journal = AppWorkflows;
    async fn claim(
        &self,
        request: &ClaimJobs,
    ) -> Result<ClaimedBatch<DeliveryGrant>, WorkflowServiceError> {
        let request = request.clone();
        let arrival = std::time::Instant::now();
        let batch = self
            .client
            .call(move |manager| {
                async move {
                    let claim = ZoneClaim {
                        worker: &manager.worker,
                        zone: &ZoneId::default_zone(),
                        request: &request,
                        deadline: manager
                            .coordinator
                            .claim_deadline(arrival, &request)
                            .map_err(manager_error)?,
                    };
                    // The journal is this host's own and lives on the caller's
                    // thread, so each grant is admitted as it stands and
                    // accepted below.
                    manager
                        .coordinator
                        .claim_in_zone(
                            &claim,
                            &manager.policies,
                            || ready(Ok(manager.worker.clone())),
                            |_| ready(Admission::Deliver(())),
                        )
                        .await
                        .map(|(batch, _)| batch)
                        .map_err(manager_error)
                }
                .boxed_local()
            })
            .await?;
        let mut deliveries = Vec::with_capacity(batch.grants.len());
        for (lease, ()) in batch.grants {
            let accepted = if lease.delivery().job.operation.accepts_execution() {
                Some(self.journal.accept_job(&lease).await?)
            } else {
                None
            };
            if let Some(zeroship_workflow::service::delivery::JobAcceptance::Deferred {
                reason,
            }) = &accepted
            {
                let defer = match reason {
                    zeroship_workflow::service::delivery::DeferredReason::NotDue { until } => {
                        zeroship_workflow_manager::GiveBack::Exact(until.get())
                    }
                    zeroship_workflow::service::delivery::DeferredReason::PolicyOff
                    | zeroship_workflow::service::delivery::DeferredReason::AtCap => {
                        zeroship_workflow_manager::GiveBack::After(Duration::from_millis(100))
                    }
                    zeroship_workflow::service::delivery::DeferredReason::DeploymentUnavailable => {
                        zeroship_workflow_manager::GiveBack::Backoff
                    }
                };
                let delivery = lease.delivery().clone();
                self.client
                    .call(move |manager| {
                        async move {
                            manager
                                .coordinator
                                .give_back_job(&manager.worker, &delivery, defer, || {
                                    ready(Ok(manager.worker.clone()))
                                })
                                .await
                                .map_err(manager_error)
                        }
                        .boxed_local()
                    })
                    .await?;
                continue;
            }
            deliveries.push(Claimed { lease, accepted });
        }
        Ok(ClaimedBatch {
            deliveries,
            after: batch.after,
            lap_complete: batch.lap_complete,
        })
    }

    async fn heartbeat(
        &self,
        journal: &AppWorkflows,
        lease: &DeliveryGrant,
        task: &DeliveredTask,
    ) -> Result<Renewed<DeliveryGrant>, WorkflowServiceError> {
        let delivery: Delivery = lease.delivery().clone();
        let lease = self
            .client
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
            .await?;
        let renewal = journal.heartbeat_job(task, &lease).await?;
        Ok(Renewed { lease, renewal })
    }

    /// Settled from this process's own journal, as the service settles a
    /// delivery that crossed to it: the receipt is read here, not taken from the
    /// caller.
    async fn settle(
        &self,
        journal: &AppWorkflows,
        lease: &DeliveryGrant,
    ) -> Result<SettlementReceipt, WorkflowServiceError> {
        self.settle_queue(committed_settlement(journal, lease).await?)
            .await
    }

    /// An attempt began and stopped without a receipt. The journal half is this
    /// process's own journal, released first; the queue half returns the row
    /// as interrupted, ready at once and counted toward the delivery budget, as
    /// the service does for a worker's release.
    async fn release(
        &self,
        journal: &AppWorkflows,
        lease: &DeliveryGrant,
        task: &DeliveredTask,
    ) -> Result<(), WorkflowServiceError> {
        journal.release_job(task, lease).await?;
        self.return_row(lease, zeroship_workflow_manager::GiveBack::Interrupted)
            .await
    }

    /// The journal half is this process's own journal, released first, so the
    /// redelivery the queue half allows is not deferred behind the task.
    async fn give_back(
        &self,
        claimed: &Claimed<DeliveryGrant>,
        why: Unstarted,
    ) -> Result<(), WorkflowServiceError> {
        if let Some(task) = claimed.task() {
            self.journal.release_job(task, &claimed.lease).await?;
        }
        let defer = match why {
            Unstarted::Unprepared => zeroship_workflow_manager::GiveBack::Backoff,
            Unstarted::Stopped => zeroship_workflow_manager::GiveBack::Unsent,
        };
        self.return_row(&claimed.lease, defer).await
    }

    async fn receipt(
        &self,
        journal: &AppWorkflows,
        job: &zeroship_core::workflow_jobs::JobSpec,
    ) -> Result<Option<zeroship_workflow::service::delivery::JobReceipt>, WorkflowServiceError> {
        journal.job_receipt(job).await
    }

    /// The journal commits first here, because its commit is what decides the
    /// outcome the queue is then settled with. Two stores and no shared
    /// transaction, so a failure between them leaves the journal holding a
    /// receipt whose delivery is unsettled -- which is what `job_receipt` and
    /// the manager's settlement replay recover.
    async fn complete(
        &self,
        journal: &AppWorkflows,
        lease: &DeliveryGrant,
        task: &DeliveredTask,
        execution: WorkflowExecution,
        confirmed: Vec<zeroship_workflow::service::delivery::PayloadConfirmation>,
    ) -> Result<Completed, WorkflowServiceError> {
        let receipt = journal
            .complete_reported_job(task, lease, execution, &confirmed)
            .await?;
        let settlement = receipt.settlement(lease)?;
        Ok(Completed {
            settlement: self.settle_queue(settlement).await?,
            receipt,
        })
    }
}

impl LocalTransport {
    /// Return `lease`'s row to this host's queue as `defer` says.
    async fn return_row(
        &self,
        lease: &DeliveryGrant,
        defer: zeroship_workflow_manager::GiveBack,
    ) -> Result<(), WorkflowServiceError> {
        let delivery = lease.delivery().clone();
        self.client
            .call(move |manager| {
                async move {
                    manager
                        .coordinator
                        .give_back_job(
                            &manager.worker,
                            &delivery,
                            defer,
                            || ready(Ok(manager.worker.clone())),
                        )
                        .await
                        .map_err(manager_error)
                }
                .boxed_local()
            })
            .await
    }

    /// The queue half every settlement on this host ends in. The successor
    /// intents a completion committed are woken by the journal handle's own
    /// hint, so this reports only what the manager recorded.
    async fn settle_queue(
        &self,
        settlement: JournalSettlement,
    ) -> Result<SettlementReceipt, WorkflowServiceError> {
        self.client
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
            .await
    }
}

/// Publishes the one app's creator intents through the local manager.
#[derive(Debug)]
pub struct LocalPublisher<'a> {
    client: &'a ManagerClient,
    app: &'a AppId,
}

impl JobPublisher for LocalPublisher<'_> {
    fn app_id(&self) -> &AppId {
        self.app
    }

    async fn submit(&self, job: &JobSpec) -> Result<JobSpec, WorkflowServiceError> {
        self.client.submit(job).await
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
        Error::Unavailable | Error::Contended => {
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
