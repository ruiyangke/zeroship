//! Production host for platform coordination metadata.
#![allow(
    clippy::future_not_send,
    reason = "compio database and HTTP tasks run on their owning threads"
)]

use crate::{
    auth::{PostgresWorkerRegistry, WorkflowAuth},
    config::WorkflowSettings,
    coordinator::{connect_eligibility, Coordinator, Options},
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
    service_assertion::ServiceAssertionVerifier,
    service_peers::{
        service_issuer, ServiceAuth, ServiceKeyring, CONTROL_SERVICE_NAME, WORKFLOW_SERVICE_NAME,
    },
    workflow_coordination::FailureCode,
    workflow_deployments::{HoldGeneration, HoldReceipt, QueueHoldRequest},
    workflow_jobs::DeploymentId,
};
use zeroship_workflow_client::{Options as ClientOptions, QueueDeploymentHolds, Transport};
use zeroship_workflow_manager::{
    capacity::{Options as CapacityOptions, StaticPool},
    driver::{Driver, Options as DriverOptions, TickReport},
    lifecycle::{self, ControlLifecycle},
    policy::control::{self, ControlPolicies, ControlPolicyStore},
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
    replay_sweep: Duration,
    pub driver: DriverOptions,
    pub driver_interval: Duration,
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
        Transport::validate_config(settings.control_url.get(), client_options(coordinator))?;
        Ok(Self {
            listen,
            http_threads,
            max_connections,
            policy_cache_entries,
            max_request_bytes,
            coordinator,
            replay_sweep,
            driver,
            driver_interval,
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
    let migrate_url = settings.migrate_url.get().clone();
    let holds = ControlHolds::new(&control_url, outbound.clone(), options.coordinator)?;
    // Verify migration readiness before accepting connections. Each HTTP thread
    // constructs its own bounded pool and retention transport in the state factory.
    let driver = driver(&url, &options, holds).await?;
    compio::time::timeout(options.coordinator.command_timeout, replay.purge_expired()).await??;
    let auth = Arc::new(WorkflowAuth::new(
        verifier,
        Arc::new(PostgresWorkerRegistry::new(client)),
        replay.clone(),
    ));
    compio::time::timeout(options.coordinator.command_timeout, auth.ready()).await??;
    let maintenance = compio::runtime::spawn(sweep_assertions(
        replay,
        options.coordinator.command_timeout,
        options.replay_sweep,
    ));
    let coordinator = options.coordinator;
    let max_request_bytes = options.max_request_bytes;
    let policy_cache_entries = options.policy_cache_entries;
    let server = web::HttpServer::new(move || {
        let url = url.clone();
        let auth = auth.clone();
        let outbound = outbound.clone();
        let control_url = control_url.clone();
        let migrate_url = migrate_url.clone();
        async move {
            web::App::new()
                .state_factory(async move || {
                    let holds = ControlHolds::new(&control_url, outbound.clone(), coordinator)?;
                    // Absent when no migration-service origin is configured. The
                    // journal endpoint then refuses, rather than answering as
                    // though a journal had been provisioned.
                    let journal = if migrate_url.is_empty() {
                        None
                    } else {
                        Some(crate::journal::Journal::new(
                            &migrate_url,
                            outbound,
                            client_options(coordinator),
                        )?)
                    };
                    let eligibility = Rc::new(connect_eligibility(&url, coordinator).await?);
                    Ok::<_, crate::coordinator::Error>(Rc::new(WorkflowHttpState {
                        service: Coordinator::connect(
                            &url,
                            coordinator,
                            Rc::new(holds),
                            eligibility,
                        )
                        .await?,
                        auth,
                        policy_source: Some(Rc::new(
                            connect_policies(&url, coordinator, policy_cache_entries).await?,
                        )),
                        journal,
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
    let driving = drive(driver, options.driver_interval, stopped);
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
    let _ = maintenance.cancel().await;
    result
}

/// Verify migration readiness before accepting connections, then compose the
/// maintenance driver. Each HTTP thread constructs its own bounded pool and
/// retention transport in the state factory.
async fn driver(url: &str, options: &ServerOptions, holds: ControlHolds) -> Result<Driver, Error> {
    let startup = Coordinator::connect(
        url,
        options.coordinator,
        Rc::new(holds),
        Rc::new(connect_eligibility(url, options.coordinator).await?),
    )
    .await?;
    connect_policies(url, options.coordinator, options.policy_cache_entries).await?;
    let lifecycle = connect_lifecycle(url, options.coordinator).await?;
    // A deployment that starts workers itself (compose replicas, a single
    // host) is a static pool: the manager never starts processes and reports
    // exhaustion durably. Adapters that start processes need an orchestrator.
    Ok(Driver::new(
        startup.manager.clone(),
        options.driver,
        Rc::new(lifecycle),
        Rc::new(StaticPool),
    )?)
}

async fn connect_policies(
    url: &str,
    options: Options,
    capacity: NonZeroUsize,
) -> Result<ControlPolicies, ManagerError> {
    use zeroship_core::schema_name::SchemaName;
    use zeroship_data_orm::{
        binding::DbBinding, encryption::ProjectKeySource, orm::Database, ConnectOptions,
    };
    compio::time::timeout(options.command_timeout, async {
        let database = Database::connect(
            DbBinding::platform(
                "platform",
                "workflow-policy",
                SchemaName::new("zeroship").map_err(|_| ManagerError::Invalid)?,
            ),
            ConnectOptions::new(url, ProjectKeySource::unavailable())
                .max_connections(
                    NonZeroUsize::new(options.connections).ok_or(ManagerError::Invalid)?,
                )
                .connection_authority(),
            control::collections()?,
        )
        .await?;
        let store = ControlPolicyStore::new(database)?;
        store.ready().await?;
        ControlPolicies::new(store, capacity, options.command_timeout)
    })
    .await
    .map_err(|_| ManagerError::Unavailable)?
}

/// Control's app catalog for the closing lane: identity and the deletion
/// marker only, through the manager's column grants.
async fn connect_lifecycle(url: &str, options: Options) -> Result<ControlLifecycle, ManagerError> {
    use zeroship_core::schema_name::SchemaName;
    use zeroship_data_orm::{
        binding::DbBinding, encryption::ProjectKeySource, orm::Database, ConnectOptions,
    };
    compio::time::timeout(options.command_timeout, async {
        let database = Database::connect(
            DbBinding::platform(
                "platform",
                "workflow-lifecycle",
                SchemaName::new("zeroship").map_err(|_| ManagerError::Invalid)?,
            ),
            ConnectOptions::new(url, ProjectKeySource::unavailable())
                .max_connections(
                    NonZeroUsize::new(options.connections).ok_or(ManagerError::Invalid)?,
                )
                .connection_authority(),
            lifecycle::collections()?,
        )
        .await?;
        let lifecycle = ControlLifecycle::new(database)?;
        lifecycle.ready().await?;
        Ok(lifecycle)
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

async fn drive(
    mut driver: Driver,
    interval: Duration,
    mut stopped: futures::channel::oneshot::Receiver<()>,
) {
    loop {
        if !matches!(stopped.try_recv(), Ok(None)) {
            return;
        }
        report_tick(driver.tick().await);
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

fn client_options(options: Options) -> ClientOptions {
    ClientOptions {
        timeout: options.command_timeout,
        ..ClientOptions::default()
    }
}

#[derive(Debug)]
struct ControlHolds(QueueDeploymentHolds);
impl ControlHolds {
    fn new(url: &str, auth: Arc<ServiceAuth>, options: Options) -> Result<Self, ManagerError> {
        QueueDeploymentHolds::new(url, auth, client_options(options))
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
