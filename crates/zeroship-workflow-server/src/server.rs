//! Production composition. This process executes no creator code.

use crate::{
    auth::{PostgresWorkerRegistry, WorkflowAuth},
    config::WorkflowSettings,
    WorkflowHttpState,
};
use ntex::web;
use std::{net::SocketAddr, sync::Arc, time::Duration};
use zeroship_authn::service_replay::SharedClientReplayStore;
use zeroship_core::{
    service_assertion::{ServiceAssertionVerifier, ServiceIssuer},
    service_peers::{load_peer_bundle, load_signing_key, service_issuer, CONTROL_SERVICE_NAME},
};
use zeroship_storage::{
    config::{build_backend, StorageBackendConfig},
    StorageStore,
};
use zeroship_workflow::service::{
    capability::WORKFLOW_AUDIENCE, store::PostgresStore, PlatformPolicy, SignalAuthority,
    WorkflowService,
};

type Error = Box<dyn std::error::Error>;

#[derive(Debug)]
pub struct ServerOptions {
    pub listen: SocketAddr,
    pub http_threads: usize,
    pub max_connections: usize,
    pub max_request_bytes: usize,
    pub storage: StorageBackendConfig,
    tick_interval: Duration,
    maintenance_batch: usize,
    policy: PlatformPolicy,
}
impl ServerOptions {
    /// Pure configuration validation, also used by the read-only CLI check.
    pub fn resolve(settings: &WorkflowSettings) -> Result<Self, Error> {
        let listen = settings.listen.get().parse()?;
        let http_threads = *settings.http_threads.get();
        let max_connections = *settings.max_connections.get();
        let max_request_bytes = *settings.max_request_bytes.get();
        let tick_ms = *settings.tick_interval_ms.get();
        let maintenance_batch = *settings.maintenance_batch.get();
        if http_threads == 0
            || max_connections == 0
            || max_request_bytes == 0
            || tick_ms == 0
            || maintenance_batch == 0
        {
            return Err(
                "workflow concurrency, request and maintenance limits must be positive".into(),
            );
        }
        if !settings.database_url.is_configured() {
            return Err("workflow.database_url is required".into());
        }
        if settings.service_key_file.get().as_os_str().is_empty()
            || settings.service_peers_file.get().as_os_str().is_empty()
        {
            return Err("workflow service key and peer files are required".into());
        }
        Ok(Self {
            listen,
            http_threads,
            max_connections,
            max_request_bytes,
            storage: StorageBackendConfig::parse(settings.payload_url.get())?,
            tick_interval: Duration::from_millis(tick_ms),
            maintenance_batch,
            policy: settings.policy()?,
        })
    }
}

pub async fn run(settings: WorkflowSettings, options: ServerOptions) -> Result<(), Error> {
    let key = Arc::new(load_signing_key(settings.service_key_file.get())?);
    let peers = load_peer_bundle(settings.service_peers_file.get())?;
    let issuer = ServiceIssuer::parse(WORKFLOW_AUDIENCE)?;
    if peers
        .public_keys_for(&service_issuer(CONTROL_SERVICE_NAME)?)
        .is_empty()
    {
        return Err("workflow peer bundle must contain Control's verification key".into());
    }
    if !peers
        .public_keys_for(&issuer)
        .iter()
        .any(|(_, public)| public == &key.verifying_key_bytes())
    {
        return Err(
            "publish the workflow signing key in the peer bundle before activating it".into(),
        );
    }
    // Load key material once. Every verifier and the signal signer use the same
    // immutable snapshot even if an operator replaces files during startup.
    let authority = Arc::new(SignalAuthority::new(key, peers.clone())?);
    let storage = StorageStore::from_backend(build_backend(&options.storage)?);
    let url = settings.database_url.expose_str().to_owned();
    let service = WorkflowService::open(Arc::new(PostgresStore::platform(
        url.clone(),
        options.policy,
    )))
    .await?
    .with_signal_authority(authority)
    .with_payload_storage(storage)?;
    let (client, connection) = compio_postgres::connect(&url, compio_postgres::NoTls).await?;
    compio::runtime::spawn(async move {
        if let Err(error) = connection.run().await {
            tracing::error!(%error, "workflow authentication database connection ended");
        }
    })
    .detach();
    let client = Arc::new(client);
    let replay = Arc::new(SharedClientReplayStore::new(client.clone()));
    let verifier = Arc::new(ServiceAssertionVerifier::new(peers.clone(), replay.clone()));
    // This also checks the assertion replay table before a listener is opened.
    replay.purge_expired().await?;
    let state = Arc::new(WorkflowHttpState {
        service: service.clone(),
        auth: WorkflowAuth::new(
            peers,
            verifier,
            Arc::new(PostgresWorkerRegistry::new(client.clone())),
            replay.clone(),
        ),
    });
    let maintenance = compio::runtime::spawn(async move {
        loop {
            if let Err(error) = service.tick_schedules().await {
                tracing::error!(%error, "workflow schedule sweep failed");
            }
            if let Err(error) = service.tick_broadcasts().await {
                tracing::error!(%error, "workflow broadcast sweep failed");
            }
            if let Err(error) = service.collect_payloads(options.maintenance_batch).await {
                tracing::error!(%error, "workflow payload sweep failed");
            }
            if let Err(error) = replay.purge_expired().await {
                tracing::error!(%error, "workflow replay sweep failed");
            }
            compio::time::sleep(options.tick_interval).await;
        }
    });
    let max_request_bytes = options.max_request_bytes;
    let server = web::HttpServer::new(move || {
        let state = state.clone();
        async move {
            web::App::new().state(state).configure(move |config| {
                crate::api::configure_with_limit(config, max_request_bytes)
            })
        }
    })
    .workers(options.http_threads)
    .maxconn(options.max_connections)
    .bind(options.listen)?
    .run();
    tracing::info!(listen = %options.listen, "workflow server listening");
    let result = server.await;
    drop(maintenance);
    result?;
    Ok(())
}
