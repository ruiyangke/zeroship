//! The workflow host thread: the creator engine, the ordinary job consumer and
//! this process's journal maintenance lane, all over the workflow journal and
//! the app's storage. Manager metadata stays on the manager thread, reached
//! through its client. Neither side opens the other's storage, and no loop here
//! scans the journal for runnable work - the queue names every row
//! either lane takes.

#![expect(
    clippy::future_not_send,
    reason = "the workflow host owns its compio thread"
)]

use super::manager::{
    self, LocalSweeps, LocalTransport, ManagerClient, ManagerThread,
};
use crate::deployment::AppDeployment;
use futures::{
    future::{Either, LocalBoxFuture, Shared},
    FutureExt,
};
use std::{
    cell::Cell,
    collections::HashMap,
    rc::Rc,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use zeroship_bundle::LoadedWorker;
use zeroship_core::{app_id::AppId, workflow_jobs::JobSpec};
use zeroship_runtime::{NativePlugin, RuntimeLimits};
use zeroship_workflow::{
    deployment_holds::AssignedHolds,
    service::{
        maintenance::{MaintenanceOptions, MaintenanceOutcome},
        schema,
        store::HostStorage,
        AppBackend, AppPolicy, AppWorkflows, CommitHint, HostPolicies, IngressEpochs,
        PolicyBinding, PolicySnapshot, WorkerIdentity, WorkflowService,
    },
    WorkflowServiceError,
};
use zeroship_workflow_runner::{
    consumer::JobConsumer,
    delivery::JobTransport,
    prepared::{CreatorFactory, CreatorRuntime, PreparedApps, PreparedOptions},
    ObjectStepOutputs, PayloadObjects, TaskExecutor, WorkerBinding,
};
use zeroship_workflow_v8::{AppRuntimeLoader, V8TaskExecutor};

const APPLIED_POLL: Duration = Duration::from_millis(20);

/// Wraps the delivery transport and executor built on the host thread.
/// The CLI uses both unchanged; tests observe them without another host.
pub trait Composition: Send + 'static {
    /// The journal is this process's own, so the transport must be one that
    /// takes it as a handle. The dev host OPENS the journal it serves -- `api`
    /// below is that handle -- and hands it to every claim, so a transport
    /// declaring `()` would be one whose journal is somewhere this host is not.
    type Transport: JobTransport<Journal = AppWorkflows> + 'static;

    fn transport(&self, transport: LocalTransport) -> Self::Transport;

    fn executor(&self, executor: Rc<dyn TaskExecutor>) -> Rc<dyn TaskExecutor> {
        executor
    }
}

#[derive(Debug)]
pub struct Production;

impl Composition for Production {
    type Transport = LocalTransport;

    fn transport(&self, transport: LocalTransport) -> LocalTransport {
        transport
    }
}

/// The host's publication hint. A signal already pending absorbs this one, so a
/// burst schedules one pass rather than one per signal. A disconnected channel
/// means this wake's task has ended; the host is told once, because a reconnect
/// would need a new wake anyway.
pub(super) fn publication_hint(wake: flume::Sender<()>, reported: Arc<AtomicBool>) -> CommitHint {
    Arc::new(move || {
        if wake.try_send(()).is_err() && !reported.swap(true, Ordering::Relaxed) {
            tracing::warn!("workflow publication wake is disconnected");
        }
    })
}

/// Everything the host needs, moved onto its thread.
pub struct Settings {
    pub app: AppId,
    pub config: super::LocalConfig,
    pub deployment: AppDeployment,
    pub storage: HostStorage,
    pub objects: PayloadObjects,
    pub env_vars: HashMap<String, String>,
    pub peers: Vec<Arc<dyn NativePlugin>>,
    pub limits: RuntimeLimits,
}

/// A host whose deployment selection and recovery responsibility are
/// established, ready to run its loops.
pub struct Opened<T: JobTransport<Journal = AppWorkflows>> {
    pub api: AppWorkflows,
    pub backend: AppBackend,
    pub executable: Option<LoadedWorker>,
    /// The selected deployment's Activation job, whose creator receipt
    /// confirms that new work uses that deployment.
    pub activation: Option<JobSpec>,
    pub host: Host<T>,
}

pub struct Host<T: JobTransport<Journal = AppWorkflows>> {
    api: AppWorkflows,
    manager: ManagerClient,
    thread: ManagerThread,
    consumer: JobConsumer<T, LocalCreator>,
    objects: PayloadObjects,
    ingress: Rc<LocalIngress>,
    app: AppId,
    sweeps: LocalSweeps,
    bounds: SweepBounds,
    wake: flume::Receiver<()>,
    report_every: Duration,
}

#[derive(Clone)]
struct LocalCreator {
    app: AppId,
    journal: AppWorkflows,
    executor: Rc<dyn TaskExecutor>,
}

impl std::fmt::Debug for LocalCreator {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("LocalCreator").finish_non_exhaustive()
    }
}

impl CreatorFactory for LocalCreator {
    type Journal = AppWorkflows;

    fn open<'a>(
        &'a self,
        app: &'a AppId,
    ) -> LocalBoxFuture<'a, Result<CreatorRuntime<Self::Journal>, WorkflowServiceError>> {
        async move {
            if app != &self.app {
                return Err(WorkflowServiceError::PermissionDenied);
            }
            Ok(CreatorRuntime {
                app: self.journal.clone(),
                executor: self.executor.clone(),
                residency: Rc::new(()),
            })
        }
        .boxed_local()
    }
}

/// What one turn of the maintenance lane runs under.
///
/// The four timing bounds are the consumer's own, because one thread serves both
/// lanes: a sweep nobody times would spend this host's only runtime on one row,
/// and both lanes claim, run and settle against the same queue and the same
/// journal on that thread. The dispatch bounds are the lane's own, since nothing
/// else in this process dispatches a sweep.
#[derive(Clone, Copy, Debug)]
struct SweepBounds {
    maintenance: MaintenanceOptions,
    /// Bound on one sweep's operation.
    execution_timeout: Duration,
    /// Bound on the claim and the settlement around it.
    operation_timeout: Duration,
    /// Delay before claiming again once the queue held no row this lane takes.
    idle_poll: Duration,
    /// Delay before claiming again after a refused claim, sweep or settlement.
    error_backoff: Duration,
}

type Stop<'a> = Shared<LocalBoxFuture<'a, ()>>;

/// Compose storage, manager and consumer, publish the retained deployment and
/// establish recovery responsibility. Nothing here waits for delivered work.
///
/// # Errors
/// Refuses incompatible storage, invalid bundles and unavailable metadata.
pub async fn open<C: Composition>(
    settings: Settings,
    composition: C,
) -> Result<Opened<C::Transport>, WorkflowServiceError> {
    let Settings {
        app,
        config,
        deployment,
        storage,
        objects,
        env_vars,
        peers,
        limits,
    } = settings;
    let store = storage.open().await?;
    schema::initialize_local(&store).await?;
    let host_policy = AppPolicy::default();
    let max_delivery_attempts = host_policy.max_delivery_attempts;
    let (manager, thread) = manager::spawn(
        deployment.platform().to_path_buf(),
        app.clone(),
        host_policy.clone(),
        config.manager_options(),
    )
    .await?;
    let policies = Arc::new(HostPolicies::default());
    let policy = policies.bind(app.clone())?;
    policy
        .begin_refresh()?
        .install(PolicySnapshot::configuration(
            POLICY_REVISION.try_into().expect("host policy revision"),
            host_policy.clone(),
        )?)?;
    let service = WorkflowService::open(Rc::new(store), policies)
        .await?
        .with_deployments(deployment.artifacts(
            config.max_source_bytes,
            Rc::new(AssignedHolds::new(Rc::new(manager.journal_holds(&app)))),
        )?);
    let api = service.register_app(&policy).await?;
    let installed = deployment
        .load(&app, config.max_archive_bytes, config.max_source_bytes)
        .await?;
    let activation = if let Some(installed) = &installed {
        let id = manager
            .record(&app, installed.hash.clone(), installed.manifest.clone())
            .await?;
        let schedules = installed.executable.declarations().manager_schedules();
        Some(manager.publish(&app, &id, schedules).await?)
    } else {
        manager.selected(&app).await?
    };
    let ingress = Rc::new(LocalIngress {
        manager: manager.clone(),
        app: app.clone(),
        binding: policy,
        policy: host_policy,
        exchanges: futures::lock::Mutex::new(()),
        used: Cell::new(false),
    });
    if let Some(activation) = &activation {
        manager.ensure_recovery(&app, activation).await?;
        // Recovery responsibility and its ingress epoch commit before the
        // host accepts a request; without an activation there is nothing to
        // accept work for.
        ingress.establish_epoch(None).await?;
    }
    let (wake_sender, wake) = flume::bounded(1);
    // Every mutating handle this process holds carries the wake: the creator
    // seam, the consumer's scoped journal and the maintenance lane all fire it
    // when their commits leave intents. One mechanism, not a hint per seam.
    let api = api
        .with_ingress(ingress.clone())
        .with_publication_hint(publication_hint(
            wake_sender.clone(),
            Arc::new(AtomicBool::new(false)),
        ));
    // The local host keeps the creator seam on the journal it opened above.
    let backend = api.clone().into_backend(
        &service,
        Arc::new(ObjectStepOutputs::new(
            objects.clone(),
            config.payloads.max_payload_bytes,
        )?),
        Arc::new(objects.clone()),
    )?;
    let env = zeroship_runtime::serve::app_env_from_prefixed_vars(&env_vars);
    let loader = Rc::new(AppRuntimeLoader::new(
        backend.clone(),
        env_vars,
        env,
        peers,
        limits,
    )?);
    let tasks = Rc::new(api.tasks(
        WorkerIdentity::new(manager.worker().as_str().to_owned())?,
        objects.clone(),
    ));
    let executor = composition.executor(Rc::new(V8TaskExecutor::new(
        loader,
        tasks,
        config.payloads,
    )?));
    let consumer_options = config.consumer_options();
    let bounds = SweepBounds {
        maintenance: MaintenanceOptions::default(),
        execution_timeout: consumer_options.delivery.execution_timeout,
        operation_timeout: consumer_options.delivery.operation_timeout,
        idle_poll: consumer_options.idle_poll,
        error_backoff: consumer_options.error_backoff,
    };
    let consumer = JobConsumer::new(
        Rc::new(composition.transport(manager.transport(api.clone()))),
        manager.worker().clone(),
        Rc::new(PreparedApps::new(
            LocalCreator {
                app: app.clone(),
                journal: api.clone(),
                executor: executor.clone(),
            },
            // The local host serves its one configured app and nothing else, so
            // that is the whole of its feed.
            Rc::new({
                let app = app.clone();
                move |candidate: &AppId| candidate == &app
            }),
            PreparedOptions {
                capacity: 1,
                operation_timeout: consumer_options.delivery.operation_timeout,
            },
        )?),
        consumer_options,
    )?;
    // A previous process may have committed intents it never published.
    let _ = wake_sender.try_send(());
    Ok(Opened {
        api: api.clone(),
        backend,
        executable: installed.map(|installed| installed.executable.into_executable()),
        activation: activation.map(|activation| activation.job),
        host: Host {
            sweeps: manager.sweeps(app.clone(), max_delivery_attempts),
            api,
            manager,
            thread,
            consumer,
            objects,
            ingress,
            app,
            bounds,
            wake,
            report_every: Duration::from_millis(config.manager.driver_interval_ms),
        },
    })
}

/// The only revision of the local host's configured policy.
const POLICY_REVISION: i64 = 1;

/// Establishes the app's ingress epoch through the local manager. The host is
/// its app's platform authority, so its configured policy decides admission
/// where the workflow service observes Control's, and the epoch is installed
/// into that same configured snapshot.
pub struct LocalIngress {
    manager: ManagerClient,
    app: AppId,
    binding: PolicyBinding,
    policy: AppPolicy,
    /// One exchange at a time, so no establishment supersedes another's
    /// refresh ticket while it waits on the manager.
    exchanges: futures::lock::Mutex<()>,
    /// Ingress accepted since the last report to the manager.
    used: Cell<bool>,
}

impl LocalIngress {
    /// Obtain and install an epoch above `after`, or any open epoch when it
    /// names none. A newer epoch another acceptance already installed
    /// satisfies the call without asking the manager.
    ///
    /// # Errors
    /// Reports refused establishment, unavailable manager storage and a
    /// retired policy binding.
    async fn establish_epoch(
        &self,
        after: Option<zeroship_core::workflow_coordination::Revision>,
    ) -> Result<(), WorkflowServiceError> {
        let _exchange = self.exchanges.lock().await;
        if self
            .binding
            .ingress_epoch()
            .is_some_and(|held| after.is_none_or(|after| held > after))
        {
            return Ok(());
        }
        let ticket = self.binding.begin_refresh()?;
        let epoch = self
            .manager
            .establish(&self.app, after, self.policy.admission)
            .await?;
        ticket.install(
            PolicySnapshot::configuration(
                POLICY_REVISION.try_into().expect("host policy revision"),
                self.policy.clone(),
            )?
            .with_ingress_epoch(Some(epoch)),
        )
    }

    /// Report ingress accepted since the previous report, so an app in use
    /// does not close as idle. A failed report is repeated by the next one.
    async fn report(&self) {
        if self.used.replace(false) {
            if let Err(error) = self.manager.note_ingress(&self.app).await {
                self.used.set(true);
                tracing::warn!(code = error.code(), "workflow ingress activity not reported");
            }
        }
    }
}

impl IngressEpochs for LocalIngress {
    fn establish(
        &self,
        after: Option<zeroship_core::workflow_coordination::Revision>,
    ) -> LocalBoxFuture<'_, Result<(), WorkflowServiceError>> {
        Box::pin(self.establish_epoch(after))
    }

    fn accepted(&self) {
        self.used.set(true);
    }
}

/// Wait until the creator has committed the delivered activation receipt.
/// Until then, new starts could still select an older deployment.
///
/// # Errors
/// Reports journal failures and an activation that did not finish in time.
pub async fn applied(
    api: &AppWorkflows,
    activation: &JobSpec,
    timeout: Duration,
) -> Result<(), WorkflowServiceError> {
    compio::time::timeout(timeout, async {
        loop {
            match api.job_receipt(activation).await {
                Ok(Some(_)) => return Ok(()),
                // Another connection's commit can refuse this read's writer
                // reservation for a moment; the receipt check simply repeats.
                Ok(None) | Err(WorkflowServiceError::Unavailable(_)) => {
                    compio::time::sleep(APPLIED_POLL).await;
                }
                Err(error) => return Err(error),
            }
        }
    })
    .await
    .map_err(|_| {
        WorkflowServiceError::Unavailable(
            "the app deployment's workflow activation was not delivered".into(),
        )
    })?
}

impl<T: JobTransport<Journal = AppWorkflows>> Host<T> {
    /// Consume delivered jobs, report ingress and publish committed intents
    /// until `stop`. Returns after execution joins and the manager stops.
    pub async fn run_until(self, stop: impl std::future::Future<Output = ()>) {
        let Self {
            api,
            manager,
            thread,
            mut consumer,
            objects,
            ingress,
            app,
            sweeps,
            bounds,
            wake,
            report_every,
        } = self;
        let stop = stop.boxed_local().shared();
        futures::join!(
            consumer.run_until(stop.clone()),
            report_ingress(&ingress, report_every, stop.clone()),
            publish(&api, &manager, &app, &wake, stop.clone()),
            sweep(
                &api,
                &manager,
                &sweeps,
                &objects,
                &app,
                bounds,
                stop.clone()
            ),
        );
        // The manager finishes in-flight operations and its current pass.
        drop(thread);
    }
}

async fn report_ingress(
    ingress: &LocalIngress,
    every: Duration,
    stop: Stop<'_>,
) {
    loop {
        if stopped(stop.clone(), compio::time::sleep(every))
            .await
            .is_none()
        {
            return;
        }
        if stopped(stop.clone(), ingress.report()).await.is_none() {
            return;
        }
    }
}

/// Publish intents committed by ingress and delivered jobs as soon as they are
/// hinted. Failure leaves them pending for the manager's reconciliation job.
async fn publish(
    api: &AppWorkflows,
    manager: &ManagerClient,
    app: &AppId,
    wake: &flume::Receiver<()>,
    stop: Stop<'_>,
) {
    while stopped(stop.clone(), wake.recv_async()).await == Some(Ok(())) {
        while wake.try_recv().is_ok() {}
        let publisher = manager.publisher(app);
        match stopped(stop.clone(), api.publish_pending_jobs(&publisher)).await {
            None => return,
            Some(Ok(())) => {}
            Some(Err(error)) => {
                tracing::warn!(
                    code = error.code(),
                    "workflow publication left to manager reconciliation"
                );
            }
        }
    }
}

/// Claim, run and settle this host's journal maintenance rows until `stop`.
///
/// The consumer beside this loop claims as `Claimant::Worker`, which admits only
/// the operation that executes creator code, so every sweep the journal needs
/// arrives here instead. This is the local host's counterpart of the workflow
/// service's maintenance driver: the same authority, the same dispatch, and the
/// journal it sweeps is the one this process already opened.
///
/// A refused visit is logged and retried after a backoff. Nothing here is
/// abandoned by that: the row keeps its lease until it lapses and the queue
/// offers it again.
async fn sweep(
    api: &AppWorkflows,
    manager: &ManagerClient,
    sweeps: &LocalSweeps,
    objects: &PayloadObjects,
    app: &AppId,
    bounds: SweepBounds,
    stop: Stop<'_>,
) {
    loop {
        let delay = match stopped(
            stop.clone(),
            swept(api, manager, sweeps, objects, app, bounds),
        )
        .await
        {
            None => return,
            // A settled row is a reason to look for the next one at once.
            Some(Ok(true)) => continue,
            Some(Ok(false)) => bounds.idle_poll,
            Some(Err(error)) => {
                tracing::warn!(code = error.code(), "workflow maintenance row retained");
                bounds.error_backoff
            }
        };
        if stopped(stop.clone(), compio::time::sleep(delay))
            .await
            .is_none()
        {
            return;
        }
    }
}

/// Take one maintenance row, run what it names and record the outcome it
/// committed. `true` reports that a row was settled.
async fn swept(
    api: &AppWorkflows,
    manager: &ManagerClient,
    sweeps: &LocalSweeps,
    objects: &PayloadObjects,
    app: &AppId,
    bounds: SweepBounds,
) -> Result<bool, WorkflowServiceError> {
    let Some(grant) = bounded(bounds.operation_timeout, sweeps.claim()).await? else {
        return Ok(false);
    };
    let publisher = manager.publisher(app);
    let receipt = match bounded(
        bounds.execution_timeout,
        api.maintenance_job(&grant, &publisher, objects, objects, bounds.maintenance),
    )
    .await?
    {
        MaintenanceOutcome::Settled(receipt) => *receipt,
        // A fanout page whose predecessor has not finished commits nothing, so
        // there is no outcome to record.
        MaintenanceOutcome::Deferred => return Ok(false),
        // The claim admits exactly the kinds this dispatch has an arm for, so
        // reaching here means the two disagree about one of them.
        MaintenanceOutcome::Unclaimed => {
            return Err(WorkflowServiceError::Internal(
                "local workflow maintenance claimed an operation its dispatch does not run".into(),
            ))
        }
    };
    let settlement = receipt.settlement(&grant)?;
    bounded(bounds.operation_timeout, sweeps.settle(&settlement)).await?;
    Ok(true)
}

async fn bounded<T>(
    timeout: Duration,
    future: impl std::future::Future<Output = Result<T, WorkflowServiceError>>,
) -> Result<T, WorkflowServiceError> {
    compio::time::timeout(timeout, Box::pin(future))
        .await
        .map_err(|_| WorkflowServiceError::Timeout)?
}

async fn stopped<T>(stop: Stop<'_>, work: impl std::future::Future<Output = T>) -> Option<T> {
    match futures::future::select(stop, work.boxed_local()).await {
        Either::Left(((), _)) => None,
        Either::Right((value, _)) => Some(value),
    }
}
