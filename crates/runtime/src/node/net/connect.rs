//! Connect authorization plus DNS, TCP, and direct-TLS connect tasks for `node:net`.

use std::net::SocketAddr;
use std::time::Duration;

use compio::net::TcpStream;
use futures::channel::mpsc;

use crate::state::{OpError, SharedState};
use crate::transport::byte_pump::SocketStream;
use crate::transport::egress::{EgressRefusal, PreDns};

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

    /// The refusal REASON is carried through: 5.7 requires a creator to be
    /// able to tell "no address survived the platform floor" from "your own
    /// REJECT rule refused it" from "nothing ACCEPTed it", because a v4-only
    /// range grant silently drops every AAAA answer and would otherwise look
    /// exactly like a broken name.
    fn connect_denied(self, host: &str, port: u16, reason: &str) -> String {
        match self {
            Self::Net => format!("node:net connect denied for {host}:{port}: {reason}"),
            #[cfg(feature = "runtime_tls")]
            Self::Tls { .. } => format!("node:tls connect denied for {host}:{port}: {reason}"),
        }
    }
}

/// A connect that cleared PHASE 1 of the egress evaluator.
///
/// `plan` carries phase 1's conclusion forward so phase 3 does not re-derive it.
/// Re-deriving `name_accepted` from the resolved address would be a reverse
/// lookup, which whoever owns the address controls.
pub(super) struct AuthorizedConnect {
    host: String,
    port: u16,
    plan: PreDns,
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

    let plan: PreDns;
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
        // PHASE 1 - synchronous, and it performs NO lookup. Running it here
        // rather than inside the resolve task is the DNS gate: a name this
        // refuses never reaches a nameserver.
        plan = match crate::transport::egress::pre_dns(&s.net_policy, &host, port) {
            Ok(plan) => plan,
            Err(EgressRefusal::ModuleDenied) => {
                return Err(capability_violation(kind.capability_denied()));
            }
            Err(refusal) => {
                return Err(capability_violation(kind.connect_denied(
                    &host,
                    port,
                    &refusal.to_string(),
                )));
            }
        };

        // An IP literal needs no resolution at all, so PHASE 3 can run here too
        // and the refusal stays synchronous.
        if let PreDns::Literal(ip) = plan {
            if let Err(refusal) = crate::transport::egress::filter_answer(
                &s.net_policy,
                port,
                false,
                vec![SocketAddr::new(ip, port)],
            ) {
                return Err(capability_violation(kind.connect_denied(
                    &host,
                    port,
                    &refusal.to_string(),
                )));
            }
        }
    }

    Ok(AuthorizedConnect { host, port, plan })
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
            verify_identity: opts.verify_identity,
        };
        let connector = match crate::transport::tls::build_tls_connector(&connector_opts) {
            Ok(connector) => connector,
            Err(e) => {
                let code = tls_connector_error_code(&e);
                push_error_and_close(
                    &state,
                    socket_id,
                    format!("TLS connector failed: {e}"),
                    code,
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

#[cfg(feature = "runtime_tls")]
pub(super) fn tls_connector_error_code(error: &std::io::Error) -> &'static str {
    if crate::transport::tls::is_tls_pin_required(error) {
        crate::transport::tls::TLS_PIN_REQUIRED_CODE
    } else {
        "ERR_TLS_HANDSHAKE"
    }
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
    let port = target.port;

    // PHASE 1 already settled an IP literal, including its address phase.
    // INVARIANT ONE-RESOLUTION: no query is made for one, ever.
    let name_accepted = match target.plan {
        PreDns::Literal(ip) => return Some(SocketAddr::new(ip, port)),
        PreDns::Resolve { name_accepted } => name_accepted,
    };

    let resolve_host = target.host.clone();
    // PHASE 2 - the single resolution. Everything downstream consumes THIS
    // answer; an implementation that resolves again has taken a wrong turn.
    let resolved = compio::time::timeout(
        resolve_timeout(),
        compio::runtime::spawn_blocking(move || {
            #[cfg(debug_assertions)]
            if zeroship_core::test_env!("ZEROSHIP_NET_TEST_DNS_HANG_HOST").as_deref()
                == Some(resolve_host.as_str())
            {
                let ms = zeroship_core::test_env!("ZEROSHIP_NET_TEST_DNS_HANG_MS")
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(250);
                std::thread::sleep(Duration::from_millis(ms));
            }
            crate::transport::egress::SystemResolver.resolve(&resolve_host, port)
        }),
    )
    .await;
    let answer = match resolved {
        Ok(Ok(Ok(answer))) => answer,
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

    // PHASE 3 - the platform floor first, then the creator's rules, applied to
    // EVERY member of the answer. The floor runs inside `filter_answer` and no
    // verdict here can move it (INVARIANT GRANTS-NARROW).
    let survivors = {
        let s = state.borrow();
        crate::transport::egress::filter_answer(&s.net_policy, port, name_accepted, answer)
    };
    let addr = match survivors {
        Ok(kept) => kept[0],
        Err(refusal) => {
            push_error_and_close(
                state,
                socket_id,
                format!("SSRF: {refusal}"),
                "ERR_NET_SSRF",
            );
            return None;
        }
    };
    Some(addr)
}

fn resolve_timeout() -> Duration {
    zeroship_core::declared_env!(
        platform,
        "ZEROSHIP_NET_RESOLVE_TIMEOUT_MS",
        crate::RuntimeConsumer
    )
    .and_then(|s| s.parse::<u64>().ok())
    .filter(|ms| *ms > 0)
    .map(Duration::from_millis)
    .unwrap_or(RESOLVE_TIMEOUT)
}
