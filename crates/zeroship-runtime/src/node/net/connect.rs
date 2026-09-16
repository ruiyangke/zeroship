//! Connect authorization plus DNS, TCP, and direct-TLS connect tasks for `node:net`.

use std::net::SocketAddr;
use std::time::Duration;

use compio::net::TcpStream;
use futures::channel::mpsc;

use crate::state::{OpError, SharedState};
use crate::transport::byte_pump::SocketStream;
use crate::transport::egress::EgressRefusal;

use super::caps::release_socket_slot;
#[cfg(feature = "runtime_tls")]
use super::driver::TlsOptions;
use super::driver::{WriteCmd, apply_keep_alive, run_socket_driver};
use super::registry::{
    SocketEvent, lookup_native_socket_state, mark_socket_activity, push_error_and_close,
    push_event,
};

pub(super) const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

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
}

/// A connect that cleared the checks a `connect()` call can answer
/// synchronously: handler kind, port range, socket state, and whether the app
/// holds `node:net` at all.
///
/// It carries NO policy conclusion. The egress verdict is
/// `transport::egress::evaluate`'s to reach, in one place, once, on the connect
/// task - see INVARIANT ONE-COMPOSITION.
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

        // No phase of the egress evaluator runs here - not phase 1 either,
        // even though it is synchronous and would let a name refusal throw
        // straight out of `connect()`. Running it here as well as on the
        // connect task would put the DNS gate in two places, and a gate with
        // two implementations is a gate one of whose implementations is
        // untested. The cost is that every policy refusal now reports through
        // the socket's `error` event, which is also what Node does for an
        // address it cannot reach.
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
    // The dev relaxation, which only a dev-tier binary states through
    // `set_dev_mode` - `zeroship serve`, and nothing else
    // (`crates/zeroship-cli/src/main.rs`, `cmd_serve`). It used to resolve
    // from `ZEROSHIP_DEV` in the process environment, so the message named
    // that variable; it no longer does, and a worker that inherited it is
    // refused here exactly like any other production process.
    let allowed = crate::transport::ssrf::dev_mode_enabled();
    if allowed {
        Ok(())
    } else {
        Err(OpError::node(
            "ERR_TLS_REJECT_UNAUTHORIZED_DISABLED",
            "rejectUnauthorized:false is only allowed in the dev runtime (`zeroship serve`)",
        ))
    }
}

pub(super) fn capability_violation(message: impl Into<String>) -> OpError {
    OpError::coded("capability_violation", message.into(), None::<String>)
}

fn validate_connect_kind(scope: &mut v8::PinScope, violated: &str) -> Result<(), OpError> {
    use crate::rpc::ProcedureKind;

    let Some(kind) = crate::rpc::current_kind(scope) else {
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
    let tasks = state.borrow().tasks.clone();
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
    tasks.spawn(async move {
        crate::panic_util::guard("node-net-connect", task).await;
    });
}

#[cfg(feature = "runtime_tls")]
pub(super) fn spawn_tls_connect_task(
    state: SharedState,
    socket_id: u32,
    target: AuthorizedConnect,
    opts: TlsOptions,
) {
    let tasks = state.borrow().tasks.clone();
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
    tasks.spawn(async move {
        crate::panic_util::guard("node-tls-connect", task).await;
    });
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

/// The whole egress decision for this connect, in ONE call.
///
/// Phases 1, 2 and 3 are `transport::egress::evaluate`'s, including the DNS
/// gate and the platform floor. Nothing here re-derives any of them; this
/// function borrows the policy and the resolver, awaits the verdict, and turns
/// a refusal into a socket `error` event.
async fn resolve_authorized_target(
    state: &SharedState,
    socket_id: u32,
    target: &AuthorizedConnect,
) -> Option<SocketAddr> {
    // The policy and the resolver are lifted out of the RefCell BEFORE the
    // await: a `Ref` held across it would panic the next `borrow_mut` on this
    // thread, and the whole point of one composition is that the await is
    // inside it.
    let (policy, resolver) = {
        let s = state.borrow();
        (s.net_policy.clone(), std::rc::Rc::clone(&s.egress_resolver))
    };

    match crate::transport::egress::evaluate(&policy, &target.host, target.port, &*resolver).await {
        // `evaluate` returns EVERY survivor in the resolver's order; taking the
        // first is happy-eyeballs preference, not a filter.
        Ok(kept) => kept.first().copied(),
        Err(refusal) => {
            report_refusal(state, socket_id, &refusal);
            None
        }
    }
}

/// Render a refusal onto the socket.
///
/// The floor, the creator's own rules and a broken lookup get DIFFERENT codes.
/// They are different refusals - one the creator cannot change, one they wrote,
/// one that is not policy at all - and 5.7 asks for exactly this distinction
/// because a v4-only range grant dropping every AAAA answer otherwise looks
/// identical to a broken name.
fn report_refusal(state: &SharedState, socket_id: u32, refusal: &EgressRefusal) {
    let (code, message) = crate::transport::egress::refusal_report(refusal);
    push_error_and_close(state, socket_id, message, code);
}
