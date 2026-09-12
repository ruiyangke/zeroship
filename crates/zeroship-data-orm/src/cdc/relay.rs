//! Worker-side CDC transport. This module never opens a replication connection.

use super::{broker, ChangeEvent, ChangeOp};
use crate::error::DbError;
use compio_ws::tungstenite::{protocol::WebSocketConfig, Message};
use futures::FutureExt;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use zeroship_core::service_peers::{service_issuer, ServiceAuth};
use zeroship_data_cdc_wire::{Event, Operation, Subscribe, MAX_MESSAGE_BYTES, PATH};

const IO_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Clone)]
pub struct RelayConfig {
    url: String,
    auth: Arc<ServiceAuth>,
    tls: Option<compio_tls::TlsConnector>,
}

impl std::fmt::Debug for RelayConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelayConfig").finish_non_exhaustive()
    }
}

impl RelayConfig {
    /// The endpoint must use TLS. Trust roots come from the host trust store.
    ///
    /// # Errors
    /// Refuses endpoints outside the TLS subscription URL contract.
    pub fn new(url: String, auth: Arc<ServiceAuth>) -> Result<Self, DbError> {
        let parsed = url::Url::parse(&url).map_err(|_| failure("invalid CDC relay URL"))?;
        if parsed.scheme() != "wss"
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.path() != PATH
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return Err(failure(
                "CDC relay requires a wss endpoint at the subscription path",
            ));
        }
        Ok(Self {
            url,
            auth,
            tls: None,
        })
    }

    /// Supply trust configuration for a private relay certificate authority.
    #[must_use]
    pub fn with_tls_connector(mut self, connector: compio_tls::TlsConnector) -> Self {
        self.tls = Some(connector);
        self
    }

    /// Trust the PEM certificates in a private relay CA bundle.
    ///
    /// # Errors
    /// Rejects an unreadable, empty, or malformed bundle.
    pub fn with_ca_file(self, path: &std::path::Path) -> Result<Self, DbError> {
        use compio_tls::rustls::{self, pki_types::pem::PemObject};
        let certificates = rustls::pki_types::CertificateDer::pem_file_iter(path)
            .map_err(|_| failure("cannot read CDC relay CA bundle"))?;
        let mut roots = rustls::RootCertStore::empty();
        for certificate in certificates {
            roots
                .add(certificate.map_err(|_| failure("invalid CDC relay CA PEM"))?)
                .map_err(|_| failure("invalid CDC relay CA certificate"))?;
        }
        if roots.is_empty() {
            return Err(failure("CDC relay CA bundle contains no certificates"));
        }
        let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|_| failure("invalid CDC relay TLS protocols"))?
        .with_root_certificates(roots)
        .with_no_client_auth();
        Ok(self.with_tls_connector(compio_tls::TlsConnector::from(Arc::new(tls))))
    }

    /// Start delivery and wait until the relay has established capture.
    ///
    /// # Errors
    /// Refuses unavailable authentication, invalid requests, or failed startup.
    pub async fn spawn(&self, app_id: &str) -> Result<RelayHandle, DbError> {
        let (startup, ready) = flume::bounded(1);
        let (shutdown, stop) = flume::bounded(1);
        let exit = Arc::new(SharedExit::default());
        let handle = RelayHandle {
            app_id: app_id.into(),
            shutdown,
            exit: exit.clone(),
        };
        let config = self.clone();
        let app = app_id.to_owned();
        compio::runtime::spawn(async move {
            let task = async {
                let _suppression = broker::SuppressGuard::activate(&app);
                let stream = config.supervise(&app, &startup).fuse();
                let stop = stop.recv_async().fuse();
                futures::pin_mut!(stream, stop);
                futures::select! { result = stream => result, _ = stop => Ok(()) }
            };
            let result = std::panic::AssertUnwindSafe(task)
                .catch_unwind()
                .await
                .unwrap_or_else(|_| Err(failure("CDC relay client panicked")));
            let _ = startup.try_send(result.clone());
            exit.complete(result);
        })
        .detach();
        match compio::time::timeout(IO_TIMEOUT, ready.recv_async()).await {
            Ok(Ok(Ok(()))) => Ok(handle),
            result => {
                handle.request_shutdown();
                let _ = handle.wait().await;
                match result {
                    Ok(Ok(Err(error))) => Err(error),
                    _ => Err(failure("CDC relay startup failed")),
                }
            }
        }
    }

    async fn supervise(
        &self,
        app: &str,
        startup: &flume::Sender<Result<(), DbError>>,
    ) -> Result<(), DbError> {
        let mut ever_ready = false;
        let mut delay = Duration::from_millis(250);
        loop {
            let result = self.consume(app, startup, &mut ever_ready).await;
            if !ever_ready {
                return result;
            }
            broker::resume_app_with_resync(app);
            tracing::warn!(
                app_id = app,
                "CDC relay disconnected; reconnecting with a fresh snapshot"
            );
            compio::time::sleep(delay).await;
            delay = (delay * 2).min(Duration::from_secs(5));
        }
    }

    async fn consume(
        &self,
        app: &str,
        startup: &flume::Sender<Result<(), DbError>>,
        ever_ready: &mut bool,
    ) -> Result<(), DbError> {
        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_MESSAGE_BYTES))
            .max_frame_size(Some(MAX_MESSAGE_BYTES));
        let config = compio_ws::Config::from(config)
            .with_buffer_sizes(MAX_MESSAGE_BYTES, MAX_MESSAGE_BYTES * 2);
        let (mut socket, _) = compio::time::timeout(
            IO_TIMEOUT,
            compio_ws::connect_async_tls_with_config(self.url.as_str(), config, self.tls.clone()),
        )
        .await
        .map_err(|_| failure("CDC relay connection timed out"))?
        .map_err(|_| failure("CDC relay connection failed"))?;
        let audience = service_issuer("svc/cdc").map_err(|_| failure("invalid relay audience"))?;
        let authorization = self
            .auth
            .authorization_for(&audience)
            .ok_or_else(|| failure("CDC relay worker identity is unavailable"))?;
        let request = Subscribe {
            app_id: app.into(),
            authorization,
        }
        .encode()
        .map_err(|_| failure("invalid relay subscription"))?;
        compio::time::timeout(IO_TIMEOUT, socket.send(Message::Binary(request.into())))
            .await
            .map_err(|_| failure("CDC relay subscribe timed out"))?
            .map_err(|_| failure("CDC relay subscribe failed"))?;
        let mut ready = false;
        loop {
            let message = compio::time::timeout(IO_TIMEOUT, socket.read())
                .await
                .map_err(|_| failure("CDC relay heartbeat timed out"))?
                .map_err(|_| failure("CDC relay stream ended"))?;
            let Message::Binary(bytes) = message else {
                return Err(failure("invalid CDC relay message"));
            };
            match Event::decode(&bytes).map_err(|_| failure("invalid CDC relay event"))? {
                Event::Ready if !ready => {
                    ready = true;
                    if *ever_ready {
                        broker::resume_app_with_resync(app);
                    } else {
                        *ever_ready = true;
                        let _ = startup.try_send(Ok(()));
                    }
                }
                Event::Heartbeat => {}
                Event::Change {
                    collection,
                    operation,
                } if ready => {
                    let op = match operation {
                        Operation::Insert => ChangeOp::Insert,
                        Operation::Update => ChangeOp::Update,
                        Operation::Delete => ChangeOp::Delete,
                    };
                    broker::publish(&ChangeEvent {
                        app_id: app.into(),
                        collection,
                        op,
                        pk: None,
                        changed_columns: Vec::new(),
                        new_tuple: std::collections::HashMap::new(),
                        old_tuple: None,
                    });
                }
                Event::Resync if ready => broker::resume_app_with_resync(app),
                _ => return Err(failure("CDC relay event arrived outside a ready stream")),
            }
        }
    }
}

fn failure(message: &str) -> DbError {
    DbError::Transient {
        message: message.into(),
    }
}

#[derive(Debug, Default)]
struct ExitState {
    result: Option<Result<(), DbError>>,
    waiters: Vec<flume::Sender<Result<(), DbError>>>,
}

#[derive(Debug, Default)]
struct SharedExit(Mutex<ExitState>);

impl SharedExit {
    fn complete(&self, result: Result<(), DbError>) {
        let waiters = {
            let mut state = self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.result.is_some() {
                return;
            }
            state.result = Some(result.clone());
            std::mem::take(&mut state.waiters)
        };
        for waiter in waiters {
            let _ = waiter.try_send(result.clone());
        }
    }

    async fn wait(&self) -> Result<(), DbError> {
        let receiver = {
            let mut state = self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(result) = &state.result {
                return result.clone();
            }
            let (sender, receiver) = flume::bounded(1);
            state.waiters.push(sender);
            receiver
        };
        receiver.recv_async().await.unwrap_or_else(|_| {
            Err(DbError::Internal {
                message: "relay client: exit notification channel closed".to_string(),
            })
        })
    }
}

/// Cloneable control and completion handle for a running relay client.
///
/// Clones share a broadcast-style completion state. A lifecycle owner
/// can retain one clone while a detached monitor awaits another; no
/// receiver competes for the single exit result.
#[derive(Clone)]
pub struct RelayHandle {
    app_id: String,
    shutdown: flume::Sender<()>,
    exit: Arc<SharedExit>,
}

impl std::fmt::Debug for RelayHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelayHandle")
            .field("app_id", &self.app_id)
            .finish_non_exhaustive()
    }
}

impl RelayHandle {
    /// Signal shutdown without blocking. Idempotent and safe from any
    /// isolate thread.
    pub fn request_shutdown(&self) {
        match self.shutdown.try_send(()) {
            Ok(()) | Err(flume::TrySendError::Full(())) => {}
            Err(flume::TrySendError::Disconnected(())) => {}
        }
    }

    /// Wait for the relay client to finish.
    ///
    /// # Errors
    /// Returns the client task's terminal failure, if any.
    pub async fn wait(&self) -> Result<(), DbError> {
        self.exit.wait().await
    }

    /// Signal shutdown and wait until the relay connection has closed.
    ///
    /// # Errors
    /// Returns the client task's terminal failure, if any.
    pub async fn shutdown(self) -> Result<(), DbError> {
        self.request_shutdown();
        self.wait().await
    }
}
