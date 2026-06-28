//! Connect authorization plus DNS, TCP, and direct-TLS connect tasks for `node:net`.

use std::net::SocketAddr;
use std::time::Duration;

use compio::net::TcpStream;
use futures::channel::mpsc;

use crate::state::{OpError, SharedState};
use crate::transport::byte_pump::SocketStream;

use super::caps::release_socket_slot;
#[cfg(feature = "runtime_tls")]
use super::driver::TlsOptions;
use super::driver::{WriteCmd, apply_keep_alive, run_socket_driver};
use super::registry::{
    SocketEvent, lookup_native_socket_state, mark_socket_activity, push_error_and_close,
    push_event,
};

pub(super) const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy)]
pub(super) enum ConnectKind {
    Net,
    #[cfg(feature = "runtime_tls")]
    Tls { reject_unauthorized: bool },
}

impl ConnectKind {
    fn violated(self) -> &'static str {
        match self {
            Self::Net => "node:net.connect",
            #[cfg(feature = "runtime_tls")]
            Self::Tls { .. } => "node:tls.connect",
        }
    }

    fn port_range_message(self) -> &'static str {
        match self {
            Self::Net => "Socket.connect port must be between 0 and 65535",
            #[cfg(feature = "runtime_tls")]
            Self::Tls { .. } => "tls.connect port must be between 0 and 65535",
        }
    }

    fn nonzero_port_message(self) -> &'static str {
        match self {
            Self::Net => "Socket.connect port must be between 1 and 65535",
            #[cfg(feature = "runtime_tls")]
            Self::Tls { .. } => "tls.connect port must be between 1 and 65535",
        }
    }

    fn capability_denied(self) -> &'static str {
        match self {
            Self::Net => "node:net capability denied",
            #[cfg(feature = "runtime_tls")]
            Self::Tls { .. } => "node:tls capability denied",
        }
    }

    fn connect_denied(self, host: &str, port: u16) -> String {
        match self {
            Self::Net => format!("node:net connect denied for {host}:{port}"),
            #[cfg(feature = "runtime_tls")]
            Self::Tls { .. } => format!("node:tls connect denied for {host}:{port}"),
        }
    }
}

pub(super) struct AuthorizedConnect {
    host: String,
    port: u16,
}

pub(super) fn authorize_connect(
    scope: &mut v8::PinScope,
    state: &SharedState,
    socket_id: u32,
    host: String,
    port: u32,
    kind: ConnectKind,
) -> Result<AuthorizedConnect, OpError> {
    validate_connect_kind(scope, kind.violated())?;
    let port = u16::try_from(port)
        .map_err(|_| OpError::range_error(kind.port_range_message()))?;
    if port == 0 {
        return Err(OpError::range_error(kind.nonzero_port_message()));
    }

    #[cfg(feature = "runtime_tls")]
    if let ConnectKind::Tls {
        reject_unauthorized,
    } = kind
    {
        validate_tls_policy(reject_unauthorized)?;
    }

    {
        let s = state.borrow();
        let socket = s
            .native_sockets
            .get(&socket_id)
            .ok_or_else(|| OpError::node("ERR_SOCKET_CLOSED", "Socket is closed"))?
            .borrow();
        if socket.connecting || socket.connected {
            return Err(OpError::node(
                "EISCONN",
                "Socket is already connecting or connected",
            ));
        }
        if !s.net_policy.module_allowed() {
            return Err(capability_violation(kind.capability_denied()));
        }
        if !s.net_policy.allows_host_port(&host, port) {
            return Err(capability_violation(kind.connect_denied(&host, port)));
        }
    }

    Ok(AuthorizedConnect { host, port })
}

#[cfg(feature = "runtime_tls")]
pub(super) fn authorize_start_tls(
    scope: &mut v8::PinScope,
    reject_unauthorized: bool,
) -> Result<(), OpError> {
    validate_connect_kind(scope, "node:tls.startTls")?;
    validate_tls_policy(reject_unauthorized)
}

#[cfg(feature = "runtime_tls")]
pub(super) fn validate_tls_policy(reject_unauthorized: bool) -> Result<(), OpError> {
    if reject_unauthorized {
        return Ok(());
    }
    let allowed = crate::transport::ssrf::dev_mode_enabled();
    if allowed {
        Ok(())
    } else {
        Err(OpError::node(
            "ERR_TLS_REJECT_UNAUTHORIZED_DISABLED",
            "rejectUnauthorized:false is only allowed when ZEROSHIP_DEV=1",
        ))
    }
}

pub(super) fn capability_violation(message: impl Into<String>) -> OpError {
    OpError::coded("capability_violation", message.into(), None::<String>)
}

fn validate_connect_kind(scope: &mut v8::PinScope, violated: &str) -> Result<(), OpError> {
    use crate::rpc::ProcedureKind;

    let Some(kind) = crate::rpc::current_kind() else {
        return Ok(());
    };
    let wrapper = match kind {
        ProcedureKind::Query => "query",
        ProcedureKind::Mutation => "mutation",
        ProcedureKind::Action | ProcedureKind::Stream | ProcedureKind::Subscription => {
            return Ok(());
        }
    };
    let remediation = "Move raw socket access into an action/stream/subscription handler or a trusted platform context.";
    let violation = crate::rpc::build_capability_violation(scope, wrapper, violated, remediation);
    Err(OpError::js_value(
        scope,
        violation.into(),
        format!("capability_violation: {wrapper} handlers cannot call {violated}"),
    ))
}

pub(super) fn spawn_connect_task(state: SharedState, socket_id: u32, target: AuthorizedConnect) {
    let task = async move {
        let Some((addr, tcp)) = connect_tcp(&state, socket_id, target).await else {
            return;
        };
        let (tx, rx) = mpsc::channel::<WriteCmd>(128);
        let (pending_writes, pending_end) =
            if let Some(socket) = lookup_native_socket_state(&state, socket_id) {
                let mut s = socket.borrow_mut();
                if s.destroyed {
                    release_socket_slot(&state, socket_id);
                    return;
                }
                s.remote = Some(addr);
                s.write_tx = Some(tx.clone());
                s.connecting = false;
                s.connected = true;
                let pending_writes: Vec<Vec<u8>> = s.pending_writes.drain(..).collect();
                let pending_end = s.pending_end;
                s.pending_end = false;
                (pending_writes, pending_end)
            } else {
                return;
            };
        if !drain_pending_to_writer(&state, socket_id, tx, pending_writes, pending_end) {
            return;
        }

        mark_socket_activity(&state);
        push_event(&state, socket_id, SocketEvent::Connect);
        push_event(&state, socket_id, SocketEvent::Ready);
        run_socket_driver(state, socket_id, SocketStream::Plain(tcp), rx).await;
    };
    compio::runtime::spawn(crate::panic_util::guard("node-net-connect", task)).detach();
}

#[cfg(feature = "runtime_tls")]
pub(super) fn spawn_tls_connect_task(
    state: SharedState,
    socket_id: u32,
    target: AuthorizedConnect,
    opts: TlsOptions,
) {
    let task = async move {
        let Some((addr, tcp)) = connect_tcp(&state, socket_id, target).await else {
            return;
        };
        let connector_opts = crate::transport::tls::TlsConnectorOptions {
            reject_unauthorized: opts.reject_unauthorized,
            ca_pem: opts.ca_pem.clone(),
        };
        let connector = match crate::transport::tls::build_tls_connector(&connector_opts) {
            Ok(connector) => connector,
            Err(e) => {
                push_error_and_close(
                    &state,
                    socket_id,
                    format!("TLS connector failed: {e}"),
                    "ERR_TLS_HANDSHAKE",
                );
                return;
            }
        };
        let tls = match compio::time::timeout(
            CONNECT_TIMEOUT,
            connector.connect(&opts.servername, tcp),
        )
        .await
        {
            Ok(Ok(tls)) => tls,
            Ok(Err(e)) => {
                push_error_and_close(
                    &state,
                    socket_id,
                    format!("TLS handshake failed: {e}"),
                    "ERR_TLS_HANDSHAKE",
                );
                return;
            }
            Err(_) => {
                push_error_and_close(
                    &state,
                    socket_id,
                    "TLS handshake timed out".to_string(),
                    "ERR_TLS_HANDSHAKE",
                );
                return;
            }
        };

        let (tx, rx) = mpsc::channel::<WriteCmd>(128);
        let (pending_writes, pending_end) =
            if let Some(socket) = lookup_native_socket_state(&state, socket_id) {
                let mut s = socket.borrow_mut();
                if s.destroyed {
                    release_socket_slot(&state, socket_id);
                    return;
                }
                s.remote = Some(addr);
                s.write_tx = Some(tx.clone());
                s.connecting = false;
                s.connected = true;
                s.encrypted = true;
                let pending_writes: Vec<Vec<u8>> = s.pending_writes.drain(..).collect();
                let pending_end = s.pending_end;
                s.pending_end = false;
                (pending_writes, pending_end)
            } else {
                return;
            };
        if !drain_pending_to_writer(&state, socket_id, tx, pending_writes, pending_end) {
            return;
        }

        mark_socket_activity(&state);
        push_event(&state, socket_id, SocketEvent::Connect);
        push_event(&state, socket_id, SocketEvent::Ready);
        push_event(
            &state,
            socket_id,
            SocketEvent::SecureConnect {
                authorized: opts.reject_unauthorized,
                authorization_error: if opts.reject_unauthorized {
                    None
                } else {
                    Some("TLS verification disabled".to_string())
                },
            },
        );
        run_socket_driver(state, socket_id, SocketStream::Tls(tls), rx).await;
    };
    compio::runtime::spawn(crate::panic_util::guard("node-tls-connect", task)).detach();
}

fn drain_pending_to_writer(
    state: &SharedState,
    socket_id: u32,
    mut tx: mpsc::Sender<WriteCmd>,
    pending_writes: Vec<Vec<u8>>,
    pending_end: bool,
) -> bool {
    for bytes in pending_writes {
        if let Err(e) = tx.try_send(WriteCmd::Data(bytes)) {
            push_error_and_close(
                state,
                socket_id,
                format!("node:net outbound write queue refused pending data: {e:?}"),
                "ERR_NET_WRITE_CAP",
            );
            return false;
        }
    }
    if pending_end && let Err(e) = tx.try_send(WriteCmd::End) {
        push_error_and_close(
            state,
            socket_id,
            format!("node:net outbound write queue refused pending end: {e:?}"),
            "ERR_NET_WRITE_CAP",
        );
        return false;
    }
    true
}

async fn connect_tcp(
    state: &SharedState,
    socket_id: u32,
    target: AuthorizedConnect,
) -> Option<(SocketAddr, TcpStream)> {
    let addr = resolve_authorized_target(state, socket_id, &target).await?;

    let tcp = match compio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr)).await {
        Ok(Ok(tcp)) => tcp,
        Ok(Err(e)) => {
            push_error_and_close(state, socket_id, format!("connect failed: {e}"), "ECONNREFUSED");
            return None;
        }
        Err(_) => {
            push_error_and_close(state, socket_id, "connect timed out".to_string(), "ETIMEDOUT");
            return None;
        }
    };

    let _ = tcp.set_nodelay(true);
    if let Some(socket) = lookup_native_socket_state(state, socket_id) {
        let s = socket.borrow();
        if let Some(on) = s.pending_no_delay {
            let _ = tcp.set_nodelay(on);
        }
        if let Some((on, delay)) = s.pending_keep_alive {
            let _ = apply_keep_alive(&tcp, on, delay);
        }
    }
    Some((addr, tcp))
}

async fn resolve_authorized_target(
    state: &SharedState,
    socket_id: u32,
    target: &AuthorizedConnect,
) -> Option<SocketAddr> {
    let resolve_host = target.host.clone();
    let port = target.port;
    let resolved = compio::time::timeout(
        resolve_timeout(),
        compio::runtime::spawn_blocking(move || {
            #[cfg(debug_assertions)]
            if std::env::var("ZEROSHIP_NET_TEST_DNS_HANG_HOST")
                .ok()
                .as_deref()
                == Some(resolve_host.as_str())
            {
                let ms = std::env::var("ZEROSHIP_NET_TEST_DNS_HANG_MS")
                    .ok()
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(250);
                std::thread::sleep(Duration::from_millis(ms));
            }
            crate::fetch::resolve_and_check_ssrf(&resolve_host, port)
        }),
    )
    .await;
    let addr = match resolved {
        Ok(Ok(Ok(addr))) => addr,
        Ok(Ok(Err(e))) => {
            push_error_and_close(state, socket_id, format!("SSRF: {e}"), "ERR_NET_SSRF");
            return None;
        }
        Ok(Err(_join)) => {
            push_error_and_close(
                state,
                socket_id,
                "DNS resolve task failed".to_string(),
                "ERR_NET_DNS",
            );
            return None;
        }
        Err(_) => {
            push_error_and_close(
                state,
                socket_id,
                "DNS resolve timed out".to_string(),
                "ERR_NET_DNS_TIMEOUT",
            );
            return None;
        }
    };
    Some(addr)
}

fn resolve_timeout() -> Duration {
    std::env::var("ZEROSHIP_NET_RESOLVE_TIMEOUT_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .map(Duration::from_millis)
        .unwrap_or(RESOLVE_TIMEOUT)
}
