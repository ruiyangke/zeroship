//! Production host for platform coordination metadata.
#![allow(
    clippy::future_not_send,
    reason = "compio database and HTTP tasks run on their owning threads"
)]

use crate::{
    auth::{PostgresWorkerRegistry, WorkflowAuth},
    config::WorkflowSettings,
    coordinator::{Coordinator, Options},
    WorkflowHttpState,
};
use futures::future::{select, Either};
use ntex::web;
use std::{net::SocketAddr, rc::Rc, sync::Arc, time::Duration};
use zeroship_authn::service_replay::SharedClientReplayStore;
use zeroship_core::{
    service_assertion::ServiceAssertionVerifier,
    service_peers::{load_peer_bundle, service_issuer, CONTROL_SERVICE_NAME},
};

type Error = Box<dyn std::error::Error>;

#[derive(Debug)]
pub struct ServerOptions {
    pub listen: SocketAddr,
    pub http_threads: usize,
    pub max_connections: usize,
    pub max_request_bytes: usize,
    pub coordinator: Options,
    replay_sweep: Duration,
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
        let max_request_bytes = *settings.max_request_bytes.get();
        let replay_sweep = Duration::from_millis(*settings.replay_sweep_ms.get());
        if http_threads == 0
            || max_connections == 0
            || max_request_bytes == 0
            || replay_sweep.is_zero()
        {
            return Err("workflow HTTP and maintenance limits must be positive".into());
        }
        if !settings.database_url.is_configured() {
            return Err("workflow.database_url is required for coordination metadata".into());
        }
        if settings.service_peers_file.get().as_os_str().is_empty() {
            return Err("workflow.service_peers_file is required".into());
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
        Ok(Self {
            listen,
            http_threads,
            max_connections,
            max_request_bytes,
            coordinator,
            replay_sweep,
        })
    }
}

/// # Errors
/// Refuses unavailable metadata/identity stores or invalid peer keys. Loss of
/// the shared authentication connection stops this process for supervisor recovery.
pub async fn run(settings: WorkflowSettings, options: ServerOptions) -> Result<(), Error> {
    let peers = load_peer_bundle(settings.service_peers_file.get())?;
    if peers
        .public_keys_for(&service_issuer(CONTROL_SERVICE_NAME)?)
        .is_empty()
    {
        return Err("workflow peer bundle must contain Control's verification key".into());
    }
    let url = settings.database_url.expose_str().to_owned();
    // Verify migration readiness before accepting connections. Each HTTP thread
    // subsequently constructs its own bounded pool through the state factory.
    Coordinator::connect(&url, options.coordinator).await?;
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
    compio::time::timeout(options.coordinator.command_timeout, replay.purge_expired()).await??;
    let auth = Arc::new(WorkflowAuth::new(
        verifier,
        Arc::new(PostgresWorkerRegistry::new(client)),
        replay.clone(),
    ));
    compio::time::timeout(options.coordinator.command_timeout, auth.ready()).await??;
    let maintenance = compio::runtime::spawn(async move {
        loop {
            if !matches!(
                compio::time::timeout(options.coordinator.command_timeout, replay.purge_expired())
                    .await,
                Ok(Ok(_))
            ) {
                tracing::warn!("workflow assertion replay cleanup unavailable");
            }
            compio::time::sleep(options.replay_sweep).await;
        }
    });
    let coordinator = options.coordinator;
    let max_request_bytes = options.max_request_bytes;
    let server = web::HttpServer::new(move || {
        let url = url.clone();
        let auth = auth.clone();
        async move {
            web::App::new()
                .state_factory(async move || {
                    Ok::<_, crate::coordinator::Error>(Rc::new(WorkflowHttpState {
                        service: Coordinator::connect(&url, coordinator).await?,
                        auth,
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
    let result = match select(Box::pin(server), disconnected).await {
        Either::Left((result, _)) => result.map_err(Into::into),
        Either::Right(_) => Err("workflow authentication database disconnected".into()),
    };
    drop(maintenance);
    result
}
