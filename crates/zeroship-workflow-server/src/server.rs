//! Production host for platform coordination metadata.
#![allow(
    clippy::future_not_send,
    reason = "compio database and HTTP tasks run on their owning threads"
)]

use crate::{
    auth::{PostgresWorkerRegistry, WorkflowAuth},
    config::WorkflowSettings,
    coordinator::{connect_eligibility, Coordinator, Options},
    payloads::ServicePayloads,
    sweeps::{LaneOptions, MaintenanceDriver, MaintenanceLane, SweepReport},
    WorkflowHttpState,
};
use futures::future::{select, Either};
use ntex::web;
use std::{
    future::Future, net::SocketAddr, num::NonZeroUsize, pin::Pin, rc::Rc, sync::Arc, time::Duration,
};
use zeroship_authn::service_replay::SharedClientReplayStore;
use zeroship_core::{
    app_id::AppId,
    config::PlaintextPeers,
    service_assertion::ServiceAssertionVerifier,
    service_peers::{
        service_issuer, ServiceAuth, ServiceKeyring, CONTROL_SERVICE_NAME, WORKFLOW_SERVICE_NAME,
    },
    workflow_coordination::{FailureCode, WorkerId},
    workflow_deployments::{HoldGeneration, HoldReceipt, QueueHoldRequest},
    workflow_jobs::DeploymentId,
};
use zeroship_storage::StorageBackendConfig;
use zeroship_workflow::{
    deploy_registrations::RemoteDeployRegistrations,
    deployment_holds::ServiceHolds,
    service::{maintenance::MaintenanceOptions, AppDeployments},
};
use zeroship_workflow_client::{
    ControlAppFacts, Options as ClientOptions, QueueDeploymentHolds, Transport,
};
use zeroship_workflow_manager::{
    app_facts::{AppFactsFuture, AppFactsSource},
    capacity::{Options as CapacityOptions, StaticPool},
    driver::{Driver, Options as DriverOptions, TickReport},
    lifecycle::FactsLifecycle,
    policy::control::{self, ControlPolicies, ControlPolicyStore, PolicyObservations},
    recovery::Options as RecoveryOptions,
    retention::HoldClient,
    Error as ManagerError,
};

type Error = Box<dyn std::error::Error>;

#[derive(Debug)]
pub struct ServerOptions {
    pub listen: SocketAddr,
    pub http_threads: usize,
    pub max_connections: usize,
    pub policy_cache_entries: NonZeroUsize,
    pub max_request_bytes: usize,
    pub coordinator: Options,
    /// Where this service's payload objects live. Validated by
    /// `--check-config`, which opens nothing; the store itself is opened once
    /// where the sweep lane is composed and once per HTTP thread, because a
    /// store's connections belong to the runtime that opened them.
    pub storage: StorageBackendConfig,
    /// Origins this process may reach over plaintext HTTP. Empty by default.
    pub plaintext_peers: PlaintextPeers,
    replay_sweep: Duration,
    pub driver: DriverOptions,
    pub driver_interval: Duration,
    /// Whether this process composes a sweep lane at all.
    ///
    /// `false` leaves [`maintenance`] returning no [`MaintenanceDriver`], so the
    /// cadence has nothing to tick: the lane is absent rather than idle, and no
    /// identity is minted for rows nothing will claim.
    pub maintenance_sweeps: bool,
}
impl ServerOptions {
    /// Pure validation used by the read-only configuration check.
    ///
    /// # Errors
    /// Rejects invalid listeners, empty limits and missing metadata credentials.
    pub fn resolve(settings: &WorkflowSettings) -> Result<Self, Error> {
        let listen = settings.listen.get().parse()?;
        let http_threads = *settings.http_threads.get();
        let max_connections = *settings.max_connections.get();
        let policy_cache_entries = NonZeroUsize::new(*settings.policy_cache_entries.get())
            .ok_or("workflow policy cache capacity must be positive")?;
        let max_request_bytes = *settings.max_request_bytes.get();
        let replay_sweep = Duration::from_millis(*settings.replay_sweep_ms.get());
        let driver_interval = Duration::from_millis(*settings.driver_interval_ms.get());
        if http_threads == 0
            || max_connections == 0
            || max_request_bytes == 0
            || replay_sweep.is_zero()
            || driver_interval.is_zero()
        {
            return Err("workflow HTTP and maintenance limits must be positive".into());
        }
        if !settings.database_url.is_configured() {
            return Err("workflow.database_url is required for coordination metadata".into());
        }
        if settings.service_peers_file.get().as_os_str().is_empty() {
            return Err("workflow.service_peers_file is required".into());
        }
        if settings.service_key_file.get().as_os_str().is_empty() {
            return Err("workflow.service_key_file is required".into());
        }
        if settings.control_url.get().is_empty() {
            return Err("workflow.control_url is required for deployment queue retention".into());
        }
        if settings.storage_url.get().is_empty() {
            return Err("workflow.storage_url is required for workflow payload objects".into());
        }
        let storage = StorageBackendConfig::parse(settings.storage_url.get())?;
        let coordinator = Options {
            connections: *settings.database_connections.get(),
            acquire_timeout: Duration::from_millis(*settings.database_acquire_timeout_ms.get()),
            command_timeout: Duration::from_millis(*settings.database_command_timeout_ms.get()),
            worker_ttl: Duration::from_millis(*settings.worker_ttl_ms.get()),
            assignment_ttl: Duration::from_millis(*settings.assignment_ttl_ms.get()),
            batch_limit: *settings.batch_limit.get(),
            max_pending_management: *settings.max_pending_management.get(),
        };
        coordinator.validate()?;
        let driver = DriverOptions {
            page_limit: coordinator.batch_limit.try_into()?,
            lane_timeout: Duration::from_millis(*settings.driver_lane_timeout_ms.get()),
            recovery: RecoveryOptions {
                idle_after: Duration::from_millis(*settings.closing_idle_ms.get()),
                closing_timeout: Duration::from_millis(*settings.closing_timeout_ms.get()),
                closing_backoff: Duration::from_millis(*settings.closing_backoff_ms.get()),
                closing_backoff_max: Duration::from_millis(*settings.closing_backoff_max_ms.get()),
                ..RecoveryOptions::default()
            },
            capacity: CapacityOptions {
                min_slots: *settings.capacity_min_slots.get(),
                max_slots: *settings.capacity_max_slots.get(),
                idle_hold_down: Duration::from_millis(*settings.capacity_hold_down_ms.get()),
                request_timeout: Duration::from_millis(*settings.capacity_request_timeout_ms.get()),
                retry_interval: Duration::from_millis(*settings.capacity_retry_interval_ms.get()),
            },
            // Queue transactions run under the command timeout; a hold outlives
            // twice that budget before the retention lane may release it.
            hold_grace: DriverOptions::default()
                .hold_grace
                .max(coordinator.command_timeout.saturating_mul(2)),
            ..DriverOptions::default()
        };
        driver.validate()?;
        let plaintext_peers = PlaintextPeers::from(settings.plaintext_peers.get().clone());
        Transport::validate_config(
            settings.control_url.get(),
            &client_options(coordinator, &plaintext_peers),
        )?;
        Ok(Self {
            listen,
            http_threads,
            max_connections,
            policy_cache_entries,
            max_request_bytes,
            coordinator,
            storage,
            plaintext_peers,
            replay_sweep,
            driver,
            driver_interval,
            maintenance_sweeps: *settings.maintenance_sweeps.get(),
        })
    }
}

/// # Errors
/// Refuses unavailable metadata/identity stores or invalid peer keys. Loss of
/// the shared authentication connection stops this process for supervisor recovery.
pub async fn run(settings: WorkflowSettings, options: ServerOptions) -> Result<(), Error> {
    let mut keyring = ServiceKeyring::load(
        service_issuer(WORKFLOW_SERVICE_NAME)?,
        settings.service_key_file.get(),
        settings.service_peers_file.get(),
    )?;
    let peers = keyring
        .take_bundle()
        .ok_or("workflow peer bundle unavailable")?;
    if peers
        .public_keys_for(&service_issuer(CONTROL_SERVICE_NAME)?)
        .is_empty()
    {
        return Err("workflow peer bundle must contain Control's verification key".into());
    }
    let url = settings.database_url.expose_str().to_owned();
    let (client, connection) = compio::time::timeout(
        options.coordinator.acquire_timeout,
        compio_postgres::connect(&url, compio_postgres::NoTls),
    )
    .await?
    .map_err(|_| "workflow authentication database unavailable")?;
    let (closed, disconnected) = futures::channel::oneshot::channel();
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
        let _ = closed.send(());
    })
    .detach();
    let client = Arc::new(client);
    let replay = Arc::new(SharedClientReplayStore::new(client.clone()));
    let verifier = Arc::new(ServiceAssertionVerifier::new(peers, replay.clone()));
    let outbound = Arc::new(ServiceAuth::new(keyring, verifier.clone()));
    let control_url = settings.control_url.get().clone();
    let plaintext_peers = options.plaintext_peers.clone();
    let holds = ControlHolds::new(
        &control_url,
        outbound.clone(),
        options.coordinator,
        &plaintext_peers,
    )?;
    let facts = RemoteAppFacts::new(
        &control_url,
        outbound.clone(),
        options.coordinator,
        &plaintext_peers,
    )?;
    // ONE observation store for the whole process, cloned into every HTTP
    // thread's state. A thread's database pool is its own; the observation an
    // app's policy is granted from is not, because the deadline a worker
    // receives must not depend on which thread accepted its connection.
    let observations = PolicyObservations::new(options.policy_cache_entries);
    // Verify migration readiness before accepting connections. Each HTTP thread
    // constructs its own bounded pool and retention transport in the state factory.
    let (driver, sweeps) = maintenance(
        &url,
        &options,
        holds,
        Rc::new(facts),
        observations.clone(),
        deployments(
            &control_url,
            &outbound,
            options.coordinator,
            &plaintext_peers,
        )?,
    )
    .await?;
    compio::time::timeout(options.coordinator.command_timeout, replay.purge_expired()).await??;
    let auth = Arc::new(WorkflowAuth::new(
        verifier,
        Arc::new(PostgresWorkerRegistry::new(client)),
        replay.clone(),
    ));
    compio::time::timeout(options.coordinator.command_timeout, auth.ready()).await??;
    let assertion_sweep = compio::runtime::spawn(sweep_assertions(
        replay,
        options.coordinator.command_timeout,
        options.replay_sweep,
    ));
    let coordinator = options.coordinator;
    let recovery = options.driver.recovery;
    let max_request_bytes = options.max_request_bytes;
    let storage = options.storage.clone();
    let server = web::HttpServer::new(move || {
        let url = url.clone();
        let auth = auth.clone();
        let outbound = outbound.clone();
        let control_url = control_url.clone();
        let plaintext_peers = plaintext_peers.clone();
        let observations = observations.clone();
        let storage = storage.clone();
        async move {
            web::App::new()
                .state_factory(async move || {
                    let holds = ControlHolds::new(
                        &control_url,
                        outbound.clone(),
                        coordinator,
                        &plaintext_peers,
                    )?;
                    // Per thread, like every other client here: cyper's pooled
                    // connections belong to the thread that opened them. The
                    // OBSERVATIONS stay shared; only the transport is per
                    // thread, so an app's policy still comes from one
                    // observation whichever thread accepted the connection.
                    let facts = Rc::new(RemoteAppFacts::new(
                        &control_url,
                        outbound.clone(),
                        coordinator,
                        &plaintext_peers,
                    )?);
                    let deployments =
                        deployments(&control_url, &outbound, coordinator, &plaintext_peers)?;
                    let eligibility = Rc::new(connect_eligibility(&url, coordinator).await?);
                    let service =
                        Coordinator::connect(&url, coordinator, Rc::new(holds), eligibility)
                            .await?;
                    // The journal shares the service own database and login;
                    // it is the same url the coordinator opened, narrowed to a
                    // different schema by its binding. Its ingress epochs are
                    // established against the coordinator's own recovery
                    // scopes, the ones the driver's lanes also close, so
                    // acceptance and closure meet on the same rows.
                    let runs = Rc::new(
                        crate::runs::RunService::connect(&url, service.recovery(recovery)?)
                            .await
                            .map_err(|_| crate::coordinator::Error::Unavailable)?
                            .with_deployments(deployments),
                    );
                    Ok::<_, crate::coordinator::Error>(Rc::new(WorkflowHttpState {
                        service,
                        auth,
                        policy_source: Some(Rc::new(
                            connect_policies(facts, &url, coordinator, observations).await?,
                        )),
                        runs,
                        // The same store the sweep lane binds, opened again on
                        // this thread: a store's connections belong to the
                        // runtime that opened them, and the namespace both bind
                        // is declared once by the engine, so the object a
                        // creator's start writes here is the object a worker
                        // reads back.
                        payloads: ServicePayloads::open(&storage)
                            .map_err(|_| crate::coordinator::Error::Unavailable)?,
                    }))
                })
                .configure(move |config| {
                    crate::api::configure_with_limit(config, max_request_bytes);
                })
        }
    })
    .workers(options.http_threads)
    .maxconn(options.max_connections)
    .bind(options.listen)?
    .run();
    tracing::info!(listen = %options.listen, "workflow coordinator listening");
    let serving = async {
        match select(Box::pin(server), disconnected).await {
            Either::Left((result, _)) => result.map_err(Into::into),
            Either::Right(_) => Err("workflow authentication database disconnected".into()),
        }
    };
    let (stop, stopped) = futures::channel::oneshot::channel();
    let driving = drive(driver, sweeps, options.driver_interval, stopped);
    let result = match select(Box::pin(serving), Box::pin(driving)).await {
        Either::Left((result, driving)) => {
            let _ = stop.send(());
            // Finish the bounded pass already in progress before releasing its
            // database and outbound client. No further pass starts after stop.
            driving.await;
            result
        }
        Either::Right(_) => Err("workflow manager driver stopped".into()),
    };
    let _ = assertion_sweep.cancel().await;
    result
}

/// Verify migration readiness before accepting connections, then compose the
/// manager's maintenance driver and, when this process is the sweep authority
/// over its queue, its own sweep lane, over one startup coordinator. Each HTTP
/// thread constructs its own bounded pool and retention transport in the state
/// factory.
///
/// `options.maintenance_sweeps` decides the lane by its PRESENCE: off, nothing
/// here opens a journal, a policy ledger or a payload store for it, no identity
/// is minted, and the cadence receives `None`. There is no lane running rarely.
async fn maintenance(
    url: &str,
    options: &ServerOptions,
    holds: ControlHolds,
    facts: Rc<dyn AppFactsSource>,
    observations: PolicyObservations,
    deployments: AppDeployments,
) -> Result<(Driver, Option<MaintenanceDriver>), Error> {
    let startup = Coordinator::connect(
        url,
        options.coordinator,
        Rc::new(holds),
        Rc::new(connect_eligibility(url, options.coordinator).await?),
    )
    .await?;
    // The closing lane reads Control's deletion marker over the same capability
    // the policy ledger reads its inputs through. There is no second binding
    // and no second credential: one exchange answers both.
    let lifecycle = FactsLifecycle::new(facts.clone());
    let sweeps = if options.maintenance_sweeps {
        Some(sweep_lane(url, options, &startup, facts, observations, deployments).await?)
    } else {
        None
    };
    // A deployment that starts workers itself (compose replicas, a single
    // host) is a static pool: the manager never starts processes and reports
    // exhaustion durably. Adapters that start processes need an orchestrator.
    Ok((
        Driver::new(
            startup.manager.clone(),
            options.driver,
            Rc::new(lifecycle),
            Rc::new(StaticPool),
        )?,
        sweeps,
    ))
}

/// This process's own lane over the maintenance rows of the queue it owns.
///
/// The lane opens a journal of its own here, on the runtime that drives it,
/// because the journals the state factory opens belong to their HTTP threads.
/// Opening it before the listener binds also means a process whose journal is
/// not installed fails to start rather than refusing sweeps once it is serving.
async fn sweep_lane(
    url: &str,
    options: &ServerOptions,
    startup: &Coordinator,
    facts: Rc<dyn AppFactsSource>,
    observations: PolicyObservations,
    deployments: AppDeployments,
) -> Result<MaintenanceDriver, Error> {
    // ONE policy ledger for the lane and for the readiness check the lane's
    // startup owes: the lane needs an app's observed policy for the delivery
    // ceiling it claims under, and reading it from anywhere else would grant a
    // second authority over the same rows.
    let policies =
        Rc::new(connect_policies(facts, url, options.coordinator, observations).await?);
    let runs = Rc::new(
        crate::runs::RunService::connect(url, startup.recovery(options.driver.recovery)?)
            .await?
            .with_deployments(deployments),
    );
    let lane = MaintenanceLane::new(
        startup.manager.queue().clone(),
        runs,
        policies,
        // This process's own identity, minted once: every row the lane leases
        // carries it, and a restart is a different holder of the same lane.
        WorkerId::mint(),
        // Opened here rather than per HTTP thread, because the lane is what
        // writes and deletes objects and the lane lives on this runtime. A store
        // this process cannot open stops the startup instead of turning the
        // sweeps it claims into failures.
        ServicePayloads::open(&options.storage)?,
        MaintenanceOptions::default(),
    )?;
    Ok(MaintenanceDriver::new(
        lane,
        LaneOptions {
            page_limit: options.driver.page_limit,
            lane_timeout: options.driver.lane_timeout,
        },
    )?)
}

/// The journal's OWN deployment holds, on the same Control origin and the same
/// signer as the queue-scoped ones. A journal hold is decided beside the runs it
/// protects and applied to the deploy catalog by Control, so the process holding
/// the journal is the one that carries this. It is the retention authority
/// alone: the artifacts stay with the hosts that hold a blob store, and an
/// operation needing one is refused by name.
///
/// Beside it, the manifest SUMMARY those operations wanted the artifacts for.
/// Control parsed the bundle when it published, so it asserts the declarations
/// and this process records its own `deploys` row from them - no blob store, no
/// artifact read, and a manifest listing is strictly less than the policy this
/// service already takes from the same origin under the same role.
///
/// One per journal: the sweep lane and every HTTP thread's journal each take
/// their own, because a transport's pooled connections belong to the runtime
/// that opened them.
fn deployments(
    control_url: &str,
    outbound: &Arc<ServiceAuth>,
    options: Options,
    plaintext_peers: &PlaintextPeers,
) -> Result<AppDeployments, ManagerError> {
    Ok(AppDeployments::holds_only(Rc::new(ServiceHolds::new(
        control_url.to_owned(),
        outbound.clone(),
        client_options(options, plaintext_peers),
    )))
    .with_registrations(Rc::new(
        RemoteDeployRegistrations::asserted(
            control_url,
            outbound.clone(),
            client_options(options, plaintext_peers),
        )
        .map_err(|error| registration_error(&error))?,
    )))
}

async fn connect_policies(
    facts: Rc<dyn AppFactsSource>,
    url: &str,
    options: Options,
    observations: PolicyObservations,
) -> Result<ControlPolicies, ManagerError> {
    use zeroship_core::schema_name::SchemaName;
    use zeroship_data_orm::{
        binding::DbBinding, encryption::ProjectKeySource, orm::Database, ConnectOptions,
    };
    compio::time::timeout(options.command_timeout, async {
        let connections = NonZeroUsize::new(options.connections).ok_or(ManagerError::Invalid)?;
        // Its own tenant, so the publication transaction takes a lane of its
        // own. A platform route carries no database id, so every binding that
        // reused a tenant would share one lane, and a top-level transaction
        // reached from inside another callback on that lane cannot be admitted.
        //
        // The ONLY binding this store takes. Its policy inputs are Control's
        // and arrive over `facts`, so the serving path holds no Control schema
        // binding for them and needs no grant on the columns that carry them.
        let publication = Database::connect(
            DbBinding::platform(
                "workflow-policy-ledger",
                "workflow-policy-ledger",
                SchemaName::new("workflow_manager").map_err(|_| ManagerError::Invalid)?,
            ),
            ConnectOptions::new(url, ProjectKeySource::unavailable())
                .max_connections(connections)
                .connection_authority(),
            control::publication_collections()?,
        )
        .await?;
        let store = ControlPolicyStore::new(facts, publication)?;
        store.ready().await?;
        ControlPolicies::new(store, observations, options.command_timeout)
    })
    .await
    .map_err(|_| ManagerError::Unavailable)?
}

async fn sweep_assertions(
    replay: Arc<SharedClientReplayStore>,
    timeout: Duration,
    interval: Duration,
) {
    loop {
        if !matches!(
            compio::time::timeout(timeout, replay.purge_expired()).await,
            Ok(Ok(_))
        ) {
            tracing::warn!("workflow assertion replay cleanup unavailable");
        }
        compio::time::sleep(interval).await;
    }
}

/// This process's whole maintenance cadence, every `interval` until `stopped` or
/// until the future is dropped.
///
/// One bounded pass of the manager's lanes, then one of this service's own sweep
/// lane when it holds one. Each pass reports what it did and propagates nothing,
/// so a lane that refuses never costs the next lane its turn or ends the cadence.
/// The two share the runtime, so they take their turns in order rather than at
/// once.
///
/// `sweeps` is absent on a host that is not the sweep authority over its queue,
/// and then this cadence drives the manager's lanes alone. It is the whole of
/// what "no sweep lane" means: nothing to tick, rather than a turn taken rarely.
pub async fn drive(
    mut driver: Driver,
    mut sweeps: Option<MaintenanceDriver>,
    interval: Duration,
    mut stopped: futures::channel::oneshot::Receiver<()>,
) {
    loop {
        if !matches!(stopped.try_recv(), Ok(None)) {
            return;
        }
        report_tick(driver.tick().await);
        if let Some(sweeps) = sweeps.as_mut() {
            report_sweep(&sweeps.tick().await);
        }
        if matches!(
            select(Box::pin(compio::time::sleep(interval)), &mut stopped).await,
            Either::Right(_)
        ) {
            return;
        }
    }
}

fn report_tick(report: TickReport) {
    for (lane, progress) in report.lanes() {
        if let Some(error) = progress.scan_error {
            tracing::warn!(lane, %error, "workflow manager scan unavailable");
        }
        if progress.timed_out {
            tracing::warn!(
                lane,
                unvisited = progress.unvisited,
                "workflow manager lane deadline exhausted"
            );
        }
        for failure in &progress.failures {
            tracing::warn!(
                lane,
                ?failure,
                "workflow manager candidate retained for retry"
            );
        }
        if progress.visited != 0 {
            tracing::debug!(lane, ?progress, "workflow manager pass completed");
        }
    }
}

/// The sweep lane's turn, in the same records and under the same lane field as
/// every other lane's, so one query over a deployment's logs answers what this
/// process's maintenance did.
fn report_sweep(report: &SweepReport) {
    const LANE: &str = "maintenance";
    if let Some(error) = report.scan_error {
        tracing::warn!(lane = LANE, %error, "workflow manager scan unavailable");
    }
    if report.timed_out {
        tracing::warn!(
            lane = LANE,
            unvisited = report.unvisited,
            "workflow manager lane deadline exhausted"
        );
    }
    for failure in &report.failures {
        tracing::warn!(
            lane = LANE,
            ?failure,
            "workflow manager candidate retained for retry"
        );
    }
    if report.visited != 0 {
        tracing::debug!(lane = LANE, ?report, "workflow manager pass completed");
    }
}

fn client_options(options: Options, plaintext_peers: &PlaintextPeers) -> ClientOptions {
    ClientOptions {
        timeout: options.command_timeout,
        plaintext_peers: plaintext_peers.clone(),
        ..ClientOptions::default()
    }
}

/// A registration client refuses only for reasons that are this process's own
/// configuration: no signer, a signer that is not the workflow role, or an
/// origin the transport fence rejects. Each maps to the class `retention_error`
/// gives the same condition, so a misconfigured deployment fails to start with
/// one vocabulary.
///
/// It adds no startup requirement: `ControlAppFacts` above already refuses a
/// process holding no workflow-role signer, and refuses it before the server
/// binds.
fn registration_error(error: &zeroship_workflow::WorkflowServiceError) -> ManagerError {
    use zeroship_workflow::WorkflowServiceError as ServiceError;
    match error {
        ServiceError::InvalidRequest(_) => ManagerError::Invalid,
        ServiceError::Unauthenticated | ServiceError::PermissionDenied => ManagerError::Denied,
        _ => ManagerError::Unavailable,
    }
}

#[derive(Debug)]
struct ControlHolds(QueueDeploymentHolds);
impl ControlHolds {
    fn new(
        url: &str,
        auth: Arc<ServiceAuth>,
        options: Options,
        plaintext_peers: &PlaintextPeers,
    ) -> Result<Self, ManagerError> {
        QueueDeploymentHolds::new(url, auth, client_options(options, plaintext_peers))
            .map(Self)
            .map_err(retention_error)
    }
}
impl HoldClient for ControlHolds {
    fn acquire<'a>(
        &'a self,
        app: &'a AppId,
        deployment: &'a DeploymentId,
        generation: HoldGeneration,
    ) -> Pin<Box<dyn Future<Output = Result<HoldReceipt, ManagerError>> + 'a>> {
        Box::pin(async move {
            self.0
                .acquire(&QueueHoldRequest {
                    app_id: app.clone(),
                    deploy_id: deployment.clone(),
                    generation,
                })
                .await
                .map_err(retention_error)
        })
    }
    fn release<'a>(
        &'a self,
        app: &'a AppId,
        deployment: &'a DeploymentId,
        generation: HoldGeneration,
    ) -> Pin<Box<dyn Future<Output = Result<HoldReceipt, ManagerError>> + 'a>> {
        Box::pin(async move {
            self.0
                .release(&QueueHoldRequest {
                    app_id: app.clone(),
                    deploy_id: deployment.clone(),
                    generation,
                })
                .await
                .map_err(retention_error)
        })
    }
}

/// Control's policy inputs and deletion marker over the service transport.
///
/// One adapter for two consumers: the policy ledger asks about one app under
/// its publication lock, and the closing lane asks about a page. They share the
/// exchange, not the cadence.
#[derive(Debug)]
struct RemoteAppFacts(ControlAppFacts);
impl RemoteAppFacts {
    fn new(
        url: &str,
        auth: Arc<ServiceAuth>,
        options: Options,
        plaintext_peers: &PlaintextPeers,
    ) -> Result<Self, ManagerError> {
        ControlAppFacts::new(url, auth, client_options(options, plaintext_peers))
            .map(Self)
            .map_err(retention_error)
    }
}
impl AppFactsSource for RemoteAppFacts {
    fn observe<'a>(&'a self, apps: &'a [AppId]) -> AppFactsFuture<'a> {
        Box::pin(async move { self.0.observe(apps).await.map_err(retention_error) })
    }
}

const fn retention_error(error: zeroship_workflow_client::Error) -> ManagerError {
    use zeroship_workflow_client::Error;
    match error {
        Error::InvalidConfig | Error::Refused(FailureCode::Invalid) => ManagerError::Invalid,
        Error::Unauthenticated
        | Error::Refused(FailureCode::Unauthenticated | FailureCode::Denied) => {
            ManagerError::Denied
        }
        Error::Refused(FailureCode::Conflict) => ManagerError::Conflict,
        Error::Refused(FailureCode::Capacity) => ManagerError::Capacity,
        Error::Timeout => ManagerError::Timeout,
        Error::RequestTooLarge
        | Error::ResponseTooLarge
        | Error::InvalidResponse
        | Error::Unavailable
        | Error::Refused(FailureCode::RequestTooLarge | FailureCode::Unavailable) => {
            ManagerError::Unavailable
        }
    }
}
