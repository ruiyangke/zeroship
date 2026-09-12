//! TLS transport and process ownership for the relay.

use crate::{auth, hub::Hub, source};
use compio::net::TcpListener;
use compio_postgres::Pool;
use compio_tls::TlsAcceptor;
use compio_ws::tungstenite::{self, Message};
use rustls::pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer};
use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};
use zeroship_core::service_assertion::InMemoryReplayStore;
use zeroship_data_cdc_server::config::CdcServerSettings;
use zeroship_data_cdc_wire::{Event, Subscribe, MAX_MESSAGE_BYTES, PATH};

type Error = Box<dyn std::error::Error>;
const IO_TIMEOUT: Duration = Duration::from_secs(10);
const RECHECK_INTERVAL: Duration = Duration::from_secs(5);
const LEADER_LOCK: i64 = 0x7a73636463726c79;

struct State {
    hub: Rc<Hub>,
    pool: Pool,
    url: String,
    replay: Arc<InMemoryReplayStore>,
    max_apps: usize,
    max_clients: usize,
    queue: usize,
    max_bytes: usize,
    max_changes: usize,
    max_relations: usize,
}

pub(crate) async fn run(settings: CdcServerSettings) -> Result<(), Error> {
    for value in [
        *settings.max_apps.get(),
        *settings.max_connections.get(),
        *settings.clients_per_app.get(),
        *settings.queue_capacity.get(),
        *settings.transaction_bytes.get(),
        *settings.transaction_changes.get(),
        *settings.max_relations.get(),
    ] {
        if value == 0 {
            return Err("relay capacity settings must be positive".into());
        }
    }
    let certs = CertificateDer::pem_file_iter(settings.tls_cert_file.get())?
        .collect::<Result<Vec<_>, _>>()?;
    let key = PrivateKeyDer::from_pem_file(settings.tls_key_file.get())?;
    // Workspace builds can also enable ring through another dependency.
    let tls = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_no_client_auth()
    .with_single_cert(certs, key)?;
    let acceptor = TlsAcceptor::from(Arc::new(tls));
    let url = settings.database_url.expose_str().to_owned();
    let pool = Pool::connect(&url, 16).await?;
    let leader = pool.acquire().await?;
    let posture = leader.query_one("SELECT rolreplication, rolsuper, rolbypassrls, rolcreaterole, rolcreatedb FROM pg_roles WHERE rolname = current_user", &[]).await?;
    if !posture.try_get::<_, bool>(0)?
        || (1..5).any(|i| posture.try_get::<_, bool>(i).unwrap_or(true))
    {
        return Err("relay login requires REPLICATION and refuses superuser, BYPASSRLS, CREATEROLE and CREATEDB".into());
    }
    let retention: i64 = leader
        .query_one(
            "SELECT setting::bigint FROM pg_settings WHERE name = 'max_slot_wal_keep_size'",
            &[],
        )
        .await?
        .try_get(0)?;
    if retention < 0 {
        return Err("relay requires finite max_slot_wal_keep_size".into());
    }
    let locked: bool = leader
        .query_one("SELECT pg_try_advisory_lock($1)", &[&LEADER_LOCK])
        .await?
        .try_get(0)?;
    if !locked {
        return Err("another CDC relay owns this database".into());
    }
    // This login never reads creator tables. Prove registry access at startup.
    leader
        .query(
            "SELECT public_key FROM zeroship.worker_instances LIMIT 0",
            &[],
        )
        .await?;
    pool.query("SELECT pg_drop_replication_slot(slot_name) FROM pg_replication_slots WHERE left(slot_name, length($1)) = $1 AND database = current_database() AND NOT active", &[&source::SLOT_PREFIX]).await?;
    let listener = TcpListener::bind(settings.listen.get()).await?;
    tracing::info!(listen = %settings.listen.get(), "CDC relay listening");
    let state = Rc::new(State {
        hub: Rc::new(Hub::default()),
        pool: pool.clone(),
        url,
        replay: Arc::new(InMemoryReplayStore::new()),
        max_apps: *settings.max_apps.get(),
        max_clients: *settings.clients_per_app.get(),
        queue: *settings.queue_capacity.get(),
        max_bytes: *settings.transaction_bytes.get(),
        max_changes: *settings.transaction_changes.get(),
        max_relations: *settings.max_relations.get(),
    });
    let count = Rc::new(Cell::new(0usize));
    let accepting = async {
        loop {
            let (socket, _) = listener.accept().await?;
            if count.get() >= *settings.max_connections.get() {
                drop(socket);
                continue;
            }
            count.set(count.get() + 1);
            let count = count.clone();
            let state = state.clone();
            let acceptor = acceptor.clone();
            compio::runtime::spawn(async move {
                struct Count(Rc<Cell<usize>>);
                impl Drop for Count {
                    fn drop(&mut self) {
                        self.0.set(self.0.get() - 1);
                    }
                }
                let _count = Count(count);
                let result = async {
                    let socket =
                        compio::time::timeout(IO_TIMEOUT, acceptor.accept(socket)).await??;
                    serve(socket, state).await
                }
                .await;
                if result.is_err() {
                    tracing::debug!("CDC connection closed");
                }
            })
            .detach();
        }
        #[allow(unreachable_code)]
        Ok::<(), Error>(())
    };
    let monitor = async {
        loop {
            compio::time::sleep(RECHECK_INTERVAL).await;
            // Losing this physical session loses the singleton lock. Fail the
            // process instead of reconnecting it behind live source tasks.
            compio::time::timeout(IO_TIMEOUT, leader.query_one("SELECT 1", &[])).await??;
        }
        #[allow(unreachable_code)]
        Ok::<(), Error>(())
    };
    futures::pin_mut!(accepting, monitor);
    match futures::future::select(accepting, monitor).await {
        futures::future::Either::Left((result, _))
        | futures::future::Either::Right((result, _)) => result,
    }
}

async fn serve<S>(socket: S, state: Rc<State>) -> Result<(), Error>
where
    S: compio::io::AsyncRead + compio::io::AsyncWrite,
{
    let config = tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE_BYTES))
        .max_frame_size(Some(MAX_MESSAGE_BYTES));
    let config =
        compio_ws::Config::from(config).with_buffer_sizes(MAX_MESSAGE_BYTES, MAX_MESSAGE_BYTES * 2);
    // Tungstenite fixes this callback's error type to an HTTP response.
    #[allow(clippy::result_large_err)]
    let callback = |request: &tungstenite::handshake::server::Request,
                    response: tungstenite::handshake::server::Response| {
        if request.uri().path() != PATH || request.uri().query().is_some() {
            let mut refusal = tungstenite::handshake::server::ErrorResponse::new(None);
            *refusal.status_mut() = tungstenite::http::StatusCode::NOT_FOUND;
            return Err(refusal);
        }
        Ok(response)
    };
    let mut socket = compio::time::timeout(
        IO_TIMEOUT,
        compio_ws::accept_hdr_with_config_async(socket, callback, config),
    )
    .await??;
    let Message::Binary(bytes) = compio::time::timeout(IO_TIMEOUT, socket.read()).await?? else {
        return Err("binary subscription required".into());
    };
    let request = Subscribe::decode(&bytes).map_err(|_| "invalid subscription")?;
    let (worker, public) = compio::time::timeout(
        IO_TIMEOUT,
        auth::verify(&state.pool, state.replay.clone(), &request.authorization),
    )
    .await??;
    drop(bytes);
    let (lease, start) = state.hub.subscribe(
        &request.app_id,
        state.max_apps,
        state.max_clients,
        state.queue,
    )?;
    if let Some(start) = start {
        let state = state.clone();
        let app = request.app_id.clone();
        compio::runtime::spawn(async move {
            source::run(
                state.hub.clone(),
                app,
                start,
                state.pool.clone(),
                state.url.clone(),
                source::Limits {
                    max_bytes: state.max_bytes,
                    max_changes: state.max_changes,
                    max_relations: state.max_relations,
                },
            )
            .await;
        })
        .detach();
    }
    drop(request);
    let mut next_check = Instant::now() + RECHECK_INTERVAL;
    loop {
        if Instant::now() >= next_check {
            let registered =
                compio::time::timeout(IO_TIMEOUT, auth::public_key(&state.pool, &worker)).await??;
            if registered != Some(public) {
                return Err("worker instance revoked".into());
            }
            next_check = Instant::now() + RECHECK_INTERVAL;
        }
        let event = match compio::time::timeout(
            next_check.saturating_duration_since(Instant::now()),
            lease.events.recv_async(),
        )
        .await
        {
            Ok(Ok(event)) => event,
            Ok(Err(_)) => return Err("source disconnected".into()),
            Err(_) => Event::Heartbeat,
        };
        let bytes = event.encode().map_err(|_| "invalid source event")?;
        compio::time::timeout(IO_TIMEOUT, socket.send(Message::Binary(bytes.into()))).await??;
    }
}
