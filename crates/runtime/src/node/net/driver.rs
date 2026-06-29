//! Byte-pump consumers for established `node:net` sockets.

use std::rc::Rc;
use std::time::Duration;

use compio::io::{AsyncWrite, AsyncWriteExt};
use compio::net::TcpStream;
#[cfg(feature = "runtime_tls")]
use compio_tls::TlsStream;
use futures::SinkExt;
use futures::StreamExt;
use futures::channel::mpsc;
use socket2::{SockRef, TcpKeepalive};

use crate::state::SharedState;
use crate::transport::byte_pump::{
    self, SelectAction, SocketStream, TcpReadHalf, TcpWriteHalf,
};

use super::caps::{decrement_buffered_amount, record_net_ingress};
#[cfg(feature = "runtime_tls")]
use super::connect::CONNECT_TIMEOUT;
use super::registry::{
    SocketEvent, lookup_native_socket_state, mark_socket_activity, push_close_once,
    push_error_and_close, push_event,
};
#[cfg(feature = "runtime_tls")]
use super::registry::pending_data_events;

const READ_CHUNK_SIZE: usize = 16 * 1024;

#[cfg(feature = "runtime_tls")]
#[derive(Debug, Clone)]
pub struct TlsOptions {
    pub servername: String,
    pub reject_unauthorized: bool,
    pub ca_pem: Option<String>,
    pub verify_identity: bool,
}

#[derive(Debug)]
pub enum WriteCmd {
    Data(Vec<u8>),
    End,
    SetNoDelay(bool),
    SetKeepAlive(bool, u64),
    #[cfg(feature = "runtime_tls")]
    StartTls(TlsOptions),
}

pub(super) async fn run_socket_driver(
    state: SharedState,
    socket_id: u32,
    stream: SocketStream,
    rx: mpsc::Receiver<WriteCmd>,
) {
    match stream {
        SocketStream::Plain(tcp) => run_plain_driver(state, socket_id, tcp, rx).await,
        #[cfg(feature = "runtime_tls")]
        SocketStream::Tls(tls) => {
            run_tls_driver(state, socket_id, SocketStream::Tls(tls), rx).await
        }
        SocketStream::Closed => run_tls_driver(state, socket_id, SocketStream::Closed, rx).await,
    }
}

enum PlainControl {
    #[cfg(feature = "runtime_tls")]
    StartTls,
    CommandClosed,
}

enum PlainReadExit {
    #[cfg(feature = "runtime_tls")]
    StartTls,
    Closed,
}

enum PlainCommandExit {
    #[cfg(feature = "runtime_tls")]
    StartTls {
        opts: TlsOptions,
        rx: mpsc::Receiver<WriteCmd>,
    },
    Closed,
}

async fn run_plain_driver(
    state: SharedState,
    socket_id: u32,
    tcp: TcpStream,
    rx: mpsc::Receiver<WriteCmd>,
) {
    let tcp = Rc::new(tcp);
    let (writer_tx, writer_rx) = mpsc::channel::<WriteCmd>(128);
    let (control_tx, control_rx) = mpsc::unbounded::<PlainControl>();

    let writer_handle = {
        let state = state.clone();
        let tcp = tcp.clone();
        compio::runtime::spawn(crate::panic_util::guard(
            "node-net-plain-writer",
            async move {
                run_plain_writer_loop(&state, socket_id, tcp, writer_rx).await;
            },
        ))
    };

    let command_handle = compio::runtime::spawn(crate::panic_util::guard(
        "node-net-plain-command-router",
        run_plain_command_router(rx, writer_tx, control_tx),
    ));

    let read_exit = run_plain_reader_loop(&state, socket_id, tcp.clone(), control_rx).await;
    match read_exit {
        #[cfg(feature = "runtime_tls")]
        PlainReadExit::StartTls => {
            let command_exit = command_handle
                .await
                .unwrap_or(Some(PlainCommandExit::Closed))
                .unwrap_or(PlainCommandExit::Closed);
            let _ = writer_handle.await;
            let PlainCommandExit::StartTls { opts, rx } = command_exit else {
                finish_plain_driver_close(&state, socket_id, tcp).await;
                return;
            };
            let tcp = match Rc::try_unwrap(tcp) {
                Ok(tcp) => tcp,
                Err(_) => {
                    push_error_and_close(
                        &state,
                        socket_id,
                        "STARTTLS upgrade failed: TCP stream still shared".to_string(),
                        "ERR_TLS_HANDSHAKE",
                    );
                    return;
                }
            };
            let Some(tls) = start_tls_on_tcp(&state, socket_id, tcp, opts).await else {
                return;
            };
            run_tls_driver(state, socket_id, SocketStream::Tls(tls), rx).await;
        }
        PlainReadExit::Closed => {
            finish_plain_driver_close(&state, socket_id, tcp).await;
            let _ = command_handle.await;
            let _ = writer_handle.await;
        }
    }
}

async fn run_plain_command_router(
    mut rx: mpsc::Receiver<WriteCmd>,
    mut writer_tx: mpsc::Sender<WriteCmd>,
    control_tx: mpsc::UnboundedSender<PlainControl>,
) -> PlainCommandExit {
    while let Some(cmd) = rx.next().await {
        match cmd {
            #[cfg(feature = "runtime_tls")]
            WriteCmd::StartTls(opts) => {
                drop(writer_tx);
                let _ = control_tx.unbounded_send(PlainControl::StartTls);
                return PlainCommandExit::StartTls { opts, rx };
            }
            other => {
                if writer_tx.send(other).await.is_err() {
                    return PlainCommandExit::Closed;
                }
            }
        }
    }
    drop(writer_tx);
    let _ = control_tx.unbounded_send(PlainControl::CommandClosed);
    PlainCommandExit::Closed
}

async fn run_plain_writer_loop(
    state: &SharedState,
    socket_id: u32,
    tcp: Rc<TcpStream>,
    mut rx: mpsc::Receiver<WriteCmd>,
) {
    let mut writer = TcpWriteHalf::new(tcp);
    while let Some(cmd) = rx.next().await {
        if !handle_plain_writer_command(state, socket_id, &mut writer, cmd).await {
            return;
        }
    }
}

async fn run_plain_reader_loop(
    state: &SharedState,
    socket_id: u32,
    tcp: Rc<TcpStream>,
    mut control_rx: mpsc::UnboundedReceiver<PlainControl>,
) -> PlainReadExit {
    let mut reader = TcpReadHalf::new(tcp);
    let socket_state = match lookup_native_socket_state(state, socket_id) {
        Some(s) => s,
        None => return PlainReadExit::Closed,
    };

    loop {
        if socket_state.borrow().destroyed {
            break;
        }

        let action = byte_pump::select_read_or_command(
            &socket_state,
            &mut reader,
            control_rx.next(),
            READ_CHUNK_SIZE,
        )
        .await;

        match action {
            SelectAction::ReadPermit(true) => continue,
            SelectAction::ReadPermit(false) => break,
            #[cfg(feature = "runtime_tls")]
            SelectAction::Command(Some(PlainControl::StartTls)) => {
                return PlainReadExit::StartTls;
            }
            SelectAction::Command(Some(PlainControl::CommandClosed))
            | SelectAction::Command(None) => break,
            SelectAction::ReadCompleted(res) => {
                let n = match res.0 {
                    Ok(n) => n,
                    Err(e) => {
                        push_error_and_close(
                            state,
                            socket_id,
                            format!("read error: {e}"),
                            "ERR_NET_READ",
                        );
                        break;
                    }
                };
                if n == 0 {
                    if let Some(socket) = lookup_native_socket_state(state, socket_id) {
                        socket.borrow_mut().write_tx = None;
                    }
                    push_event(state, socket_id, SocketEvent::End);
                    break;
                }
                let chunk = res.1;
                let mut data = chunk;
                data.truncate(n);
                if let Some(socket) = lookup_native_socket_state(state, socket_id) {
                    socket.borrow_mut().bytes_read += n as u64;
                }
                record_net_ingress(state, n as u64);
                mark_socket_activity(state);
                push_event(state, socket_id, SocketEvent::Data(data));
            }
        }
    }

    PlainReadExit::Closed
}

async fn finish_plain_driver_close(state: &SharedState, socket_id: u32, tcp: Rc<TcpStream>) {
    let mut writer = TcpWriteHalf::new(tcp);
    let _ = writer.shutdown().await;
    if let Some(socket) = lookup_native_socket_state(state, socket_id) {
        socket.borrow_mut().write_tx = None;
    }
    let had_error = lookup_native_socket_state(state, socket_id)
        .map(|s| s.borrow().had_error)
        .unwrap_or(false);
    push_close_once(state, socket_id, had_error);
}

async fn run_tls_driver(
    state: SharedState,
    socket_id: u32,
    mut stream: SocketStream,
    mut rx: mpsc::Receiver<WriteCmd>,
) {
    let socket_state = match lookup_native_socket_state(&state, socket_id) {
        Some(s) => s,
        None => return,
    };

    loop {
        if socket_state.borrow().destroyed {
            break;
        }

        let action = byte_pump::select_read_or_command(
            &socket_state,
            &mut stream,
            rx.next(),
            READ_CHUNK_SIZE,
        )
        .await;

        match action {
            SelectAction::ReadPermit(true) => continue,
            SelectAction::ReadPermit(false) => break,
            SelectAction::Command(Some(cmd)) => {
                if !handle_driver_command(&state, socket_id, &mut stream, cmd).await {
                    break;
                }
            }
            SelectAction::Command(None) => break,
            SelectAction::ReadCompleted(res) => {
                let n = match res.0 {
                    Ok(n) => n,
                    Err(e) => {
                        push_error_and_close(
                            &state,
                            socket_id,
                            format!("read error: {e}"),
                            "ERR_NET_READ",
                        );
                        break;
                    }
                };
                if n == 0 {
                    if let Some(socket) = lookup_native_socket_state(&state, socket_id) {
                        socket.borrow_mut().write_tx = None;
                    }
                    push_event(&state, socket_id, SocketEvent::End);
                    break;
                }
                let chunk = res.1;
                let mut data = chunk;
                data.truncate(n);
                if let Some(socket) = lookup_native_socket_state(&state, socket_id) {
                    socket.borrow_mut().bytes_read += n as u64;
                }
                record_net_ingress(&state, n as u64);
                mark_socket_activity(&state);
                push_event(&state, socket_id, SocketEvent::Data(data));
            }
        }
    }

    let _ = stream.shutdown().await;
    if let Some(socket) = lookup_native_socket_state(&state, socket_id) {
        socket.borrow_mut().write_tx = None;
    }
    let had_error = lookup_native_socket_state(&state, socket_id)
        .map(|s| s.borrow().had_error)
        .unwrap_or(false);
    push_close_once(&state, socket_id, had_error);
}

async fn write_driver_data<W>(
    state: &SharedState,
    socket_id: u32,
    stream: &mut W,
    bytes: Vec<u8>,
) -> bool
where
    W: AsyncWrite + Unpin,
{
    let n = bytes.len() as u64;
    let res = stream.write_all(bytes).await;
    if let Err(e) = res.0 {
        push_error_and_close(
            state,
            socket_id,
            format!("write error: {e}"),
            "ERR_NET_WRITE",
        );
        return false;
    }
    if let Err(e) = stream.flush().await {
        push_error_and_close(
            state,
            socket_id,
            format!("write flush error: {e}"),
            "ERR_NET_WRITE",
        );
        return false;
    }
    if let Some(socket) = lookup_native_socket_state(state, socket_id) {
        let mut s = socket.borrow_mut();
        s.bytes_written = s.bytes_written.saturating_add(n);
    }
    mark_socket_activity(state);
    decrement_buffered_amount(state, socket_id, n);
    true
}

async fn handle_plain_writer_command(
    state: &SharedState,
    socket_id: u32,
    stream: &mut TcpWriteHalf,
    cmd: WriteCmd,
) -> bool {
    match cmd {
        WriteCmd::Data(bytes) => write_driver_data(state, socket_id, stream, bytes).await,
        WriteCmd::End => {
            let _ = stream.shutdown().await;
            true
        }
        WriteCmd::SetNoDelay(on) => {
            let _ = stream.tcp.set_nodelay(on);
            true
        }
        WriteCmd::SetKeepAlive(on, initial_delay_ms) => {
            let _ = apply_keep_alive(stream.tcp.as_ref(), on, initial_delay_ms);
            true
        }
        #[cfg(feature = "runtime_tls")]
        WriteCmd::StartTls(_) => true,
    }
}

async fn handle_driver_command(
    state: &SharedState,
    socket_id: u32,
    stream: &mut SocketStream,
    cmd: WriteCmd,
) -> bool {
    match cmd {
        WriteCmd::Data(bytes) => write_driver_data(state, socket_id, stream, bytes).await,
        WriteCmd::End => {
            let _ = stream.shutdown().await;
            true
        }
        WriteCmd::SetNoDelay(on) => {
            if let SocketStream::Plain(tcp) = stream {
                let _ = tcp.set_nodelay(on);
            }
            true
        }
        WriteCmd::SetKeepAlive(on, initial_delay_ms) => {
            if let SocketStream::Plain(tcp) = stream {
                let _ = apply_keep_alive(tcp, on, initial_delay_ms);
            }
            true
        }
        #[cfg(feature = "runtime_tls")]
        WriteCmd::StartTls(opts) => start_tls_in_driver(state, socket_id, stream, opts).await,
    }
}

#[cfg(feature = "runtime_tls")]
async fn start_tls_in_driver(
    state: &SharedState,
    socket_id: u32,
    stream: &mut SocketStream,
    opts: TlsOptions,
) -> bool {
    if pending_data_events(state, socket_id) {
        push_error_and_close(
            state,
            socket_id,
            "STARTTLS upgrade refused with pending plaintext bytes".to_string(),
            "ERR_TLS_HANDSHAKE",
        );
        return false;
    }

    let tcp = match std::mem::replace(stream, SocketStream::Closed) {
        SocketStream::Plain(tcp) => tcp,
        SocketStream::Tls(tls) => {
            *stream = SocketStream::Tls(tls);
            push_error_and_close(
                state,
                socket_id,
                "Socket is already encrypted".to_string(),
                "ERR_TLS_HANDSHAKE",
            );
            return false;
        }
        SocketStream::Closed => {
            push_error_and_close(
                state,
                socket_id,
                "Socket is closed".to_string(),
                "ERR_TLS_HANDSHAKE",
            );
            return false;
        }
    };

    let Some(tls) = start_tls_on_tcp(state, socket_id, tcp, opts).await else {
        return false;
    };
    *stream = SocketStream::Tls(tls);
    true
}

#[cfg(feature = "runtime_tls")]
async fn start_tls_on_tcp(
    state: &SharedState,
    socket_id: u32,
    tcp: TcpStream,
    opts: TlsOptions,
) -> Option<TlsStream<TcpStream>> {
    if pending_data_events(state, socket_id) {
        push_error_and_close(
            state,
            socket_id,
            "STARTTLS upgrade refused with pending plaintext bytes".to_string(),
            "ERR_TLS_HANDSHAKE",
        );
        return None;
    }

    let connector_opts = crate::transport::tls::TlsConnectorOptions {
        reject_unauthorized: opts.reject_unauthorized,
        ca_pem: opts.ca_pem.clone(),
        verify_identity: opts.verify_identity,
    };
    let connector = match crate::transport::tls::build_tls_connector(&connector_opts) {
        Ok(connector) => connector,
        Err(e) => {
            let code = super::connect::tls_connector_error_code(&e);
            push_error_and_close(
                state,
                socket_id,
                format!("TLS connector failed: {e}"),
                code,
            );
            return None;
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
                state,
                socket_id,
                format!("TLS handshake failed: {e}"),
                "ERR_TLS_HANDSHAKE",
            );
            return None;
        }
        Err(_) => {
            push_error_and_close(
                state,
                socket_id,
                "TLS handshake timed out".to_string(),
                "ERR_TLS_HANDSHAKE",
            );
            return None;
        }
    };

    if let Some(socket) = lookup_native_socket_state(state, socket_id) {
        socket.borrow_mut().encrypted = true;
    }
    push_event(
        state,
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
    Some(tls)
}

pub(super) fn apply_keep_alive(
    tcp: &TcpStream,
    on: bool,
    initial_delay_ms: u64,
) -> std::io::Result<()> {
    let sock = SockRef::from(tcp);
    if on {
        let delay = Duration::from_millis(initial_delay_ms.max(1));
        sock.set_tcp_keepalive(&TcpKeepalive::new().with_time(delay))?;
    } else {
        sock.set_keepalive(false)?;
    }
    Ok(())
}
