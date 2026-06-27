//! Native `node:net.Socket` state and compio TCP/TLS driver.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::io;
use std::rc::Rc;
use std::task::Waker;
use std::time::{Duration, Instant};

use compio::buf::{IoBuf, IoBufMut};
use compio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use compio::net::TcpStream;
#[cfg(feature = "runtime_native_websocket")]
use compio_tls::TlsStream;
use futures::channel::mpsc;
use futures::future::{Either, select};
use futures::StreamExt;
use socket2::{SockRef, TcpKeepalive};

use crate::state::{OpResult, SharedState};

const RECV_BACKPRESSURE_CAP: usize = 256;
const RECV_BACKPRESSURE_RESUME: usize = 128;
const READ_CHUNK_SIZE: usize = 16 * 1024;
pub(crate) const HIGH_WATER_MARK: u64 = 16 * 1024;
const OUTBOUND_HARD_CAP: u64 = 1024 * 1024;
const OUTBOUND_QUEUE_CAP: usize = 127;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub enum SocketEvent {
    Connect,
    Ready,
    SecureConnect {
        authorized: bool,
        authorization_error: Option<String>,
    },
    Data(Vec<u8>),
    Drain,
    End,
    Error { message: String, code: String },
    Close { had_error: bool },
}

#[cfg(feature = "runtime_native_websocket")]
#[derive(Debug, Clone)]
pub struct TlsOptions {
    pub servername: String,
    pub reject_unauthorized: bool,
    pub ca_pem: Option<String>,
}

#[derive(Debug)]
pub enum WriteCmd {
    Data(Vec<u8>),
    End,
    SetNoDelay(bool),
    SetKeepAlive(bool, u64),
    #[cfg(feature = "runtime_native_websocket")]
    StartTls(TlsOptions),
}

pub struct NativeSocketState {
    pub events: VecDeque<SocketEvent>,
    pub recv_backpressure_waker: Option<Waker>,
    pub paused: bool,
    pub destroyed: bool,
    pub connecting: bool,
    pub connected: bool,
    pub ended: bool,
    pub close_emitted: bool,
    pub counted: bool,
    pub had_error: bool,
    pub write_tx: Option<mpsc::Sender<WriteCmd>>,
    pub pending_writes: VecDeque<Vec<u8>>,
    pub pending_end: bool,
    pub buffered_amount: u64,
    pub drain_pending: bool,
    pub egress_total: u64,
    pub remote: Option<std::net::SocketAddr>,
    pub bytes_read: u64,
    pub bytes_written: u64,
    pub pending_no_delay: Option<bool>,
    pub pending_keep_alive: Option<(bool, u64)>,
    pub encrypted: bool,
}

impl NativeSocketState {
    pub fn new() -> Self {
        Self {
            events: VecDeque::new(),
            recv_backpressure_waker: None,
            paused: false,
            destroyed: false,
            connecting: false,
            connected: false,
            ended: false,
            close_emitted: false,
            counted: false,
            had_error: false,
            write_tx: None,
            pending_writes: VecDeque::new(),
            pending_end: false,
            buffered_amount: 0,
            drain_pending: false,
            egress_total: 0,
            remote: None,
            bytes_read: 0,
            bytes_written: 0,
            pending_no_delay: None,
            pending_keep_alive: None,
            encrypted: false,
        }
    }
}

pub fn alloc_native_socket_id(state: &SharedState) -> u32 {
    let mut s = state.borrow_mut();
    let id = s.next_native_socket_id;
    s.next_native_socket_id = id.checked_add(1).unwrap_or(1);
    s.native_sockets
        .insert(id, Rc::new(RefCell::new(NativeSocketState::new())));
    id
}

pub fn free_native_socket_state(state: &SharedState, socket_id: u32) {
    release_socket_slot(state, socket_id);
    let mut s = state.borrow_mut();
    s.native_sockets.remove(&socket_id);
    s.native_socket_wrappers.remove(&socket_id);
}

pub fn lookup_native_socket_state(
    state: &SharedState,
    socket_id: u32,
) -> Option<Rc<RefCell<NativeSocketState>>> {
    state.borrow().native_sockets.get(&socket_id).cloned()
}

pub fn attach_wrapper(
    state: &SharedState,
    socket_id: u32,
    wrapper: v8::Global<v8::Object>,
) {
    state
        .borrow_mut()
        .native_socket_wrappers
        .insert(socket_id, wrapper);
}

pub fn drain_events(state: &SharedState, socket_id: u32) -> Vec<SocketEvent> {
    let Some(socket) = lookup_native_socket_state(state, socket_id) else {
        return Vec::new();
    };
    let drained: Vec<SocketEvent> = {
        let mut s = socket.borrow_mut();
        s.events.drain(..).collect()
    };
    let waker = {
        let mut s = socket.borrow_mut();
        if s.events.len() < RECV_BACKPRESSURE_RESUME && !s.paused {
            s.recv_backpressure_waker.take()
        } else {
            None
        }
    };
    if let Some(w) = waker {
        w.wake();
    }
    drained
}

fn push_event(state: &SharedState, socket_id: u32, event: SocketEvent) {
    let Some(socket) = lookup_native_socket_state(state, socket_id) else {
        return;
    };
    socket.borrow_mut().events.push_back(event);

    let id = socket_id;
    let fut: std::pin::Pin<Box<dyn std::future::Future<Output = OpResult>>> =
        Box::pin(async move { OpResult::SocketEvent { socket_id: id } });
    state.borrow_mut().spawned_ops.push(fut);
    state.borrow().notify_pump();
}

fn mark_socket_activity(state: &SharedState) {
    state.borrow_mut().native_socket_last_activity = Some(Instant::now());
}

fn push_error_and_close(state: &SharedState, socket_id: u32, message: String, code: &str) {
    if let Some(socket) = lookup_native_socket_state(state, socket_id) {
        let mut s = socket.borrow_mut();
        s.had_error = true;
        s.destroyed = true;
        s.write_tx = None;
        s.pending_writes.clear();
        s.pending_end = false;
    }
    push_event(
        state,
        socket_id,
        SocketEvent::Error {
            message,
            code: code.to_string(),
        },
    );
    push_close_once(state, socket_id, true);
}

fn push_close_once(state: &SharedState, socket_id: u32, had_error: bool) {
    let should_push = if let Some(socket) = lookup_native_socket_state(state, socket_id) {
        let mut s = socket.borrow_mut();
        if s.close_emitted {
            false
        } else {
            s.close_emitted = true;
            s.destroyed = true;
            s.connecting = false;
            s.connected = false;
            s.write_tx = None;
            s.pending_writes.clear();
            s.pending_end = false;
            s.buffered_amount = 0;
            s.drain_pending = false;
            true
        }
    } else {
        false
    };
    if should_push {
        release_socket_slot(state, socket_id);
        push_event(state, socket_id, SocketEvent::Close { had_error });
    }
}

pub fn pause_socket(state: &SharedState, socket_id: u32) {
    if let Some(socket) = lookup_native_socket_state(state, socket_id) {
        socket.borrow_mut().paused = true;
    }
}

pub fn resume_socket(state: &SharedState, socket_id: u32) {
    let waker = if let Some(socket) = lookup_native_socket_state(state, socket_id) {
        let mut s = socket.borrow_mut();
        s.paused = false;
        s.recv_backpressure_waker.take()
    } else {
        None
    };
    if let Some(w) = waker {
        w.wake();
    }
}

pub fn destroy_socket(state: &SharedState, socket_id: u32) {
    let waker = if let Some(socket) = lookup_native_socket_state(state, socket_id) {
        let mut s = socket.borrow_mut();
        if s.destroyed {
            return;
        }
        s.destroyed = true;
        s.write_tx = None;
        s.pending_writes.clear();
        s.pending_end = false;
        s.recv_backpressure_waker.take()
    } else {
        None
    };
    if let Some(w) = waker {
        w.wake();
    }
    push_close_once(state, socket_id, false);
}

pub fn set_no_delay(state: &SharedState, socket_id: u32, on: bool) -> std::io::Result<()> {
    let mut tx = {
        let Some(socket) = lookup_native_socket_state(state, socket_id) else {
            return Ok(());
        };
        let mut s = socket.borrow_mut();
        s.pending_no_delay = Some(on);
        s.write_tx.clone()
    };
    if let Some(ref mut tx) = tx {
        tx.try_send(WriteCmd::SetNoDelay(on)).map_err(|e| {
            io::Error::new(io::ErrorKind::BrokenPipe, format!("control queue closed: {e:?}"))
        })?;
    }
    Ok(())
}

pub fn set_keep_alive(
    state: &SharedState,
    socket_id: u32,
    on: bool,
    initial_delay_ms: u64,
) -> std::io::Result<()> {
    let mut tx = {
        let Some(socket) = lookup_native_socket_state(state, socket_id) else {
            return Ok(());
        };
        let mut s = socket.borrow_mut();
        s.pending_keep_alive = Some((on, initial_delay_ms));
        s.write_tx.clone()
    };
    if let Some(ref mut tx) = tx {
        tx.try_send(WriteCmd::SetKeepAlive(on, initial_delay_ms))
            .map_err(|e| {
                io::Error::new(io::ErrorKind::BrokenPipe, format!("control queue closed: {e:?}"))
            })?;
    }
    Ok(())
}

pub fn queue_write(
    state: &SharedState,
    socket_id: u32,
    bytes: Vec<u8>,
) -> Result<bool, String> {
    let n = bytes.len() as u64;
    let (tx, over_hwm) = {
        let socket = lookup_native_socket_state(state, socket_id)
            .ok_or_else(|| "Socket is closed".to_string())?;
        let mut sock = socket.borrow_mut();
        if sock.destroyed || sock.ended {
            return Err("write after end".to_string());
        }
        if state.borrow().native_net_egress_exhausted {
            drop(sock);
            push_error_and_close(
                state,
                socket_id,
                "node:net egress ceiling exceeded".to_string(),
                "ERR_NET_EGRESS_CAP",
            );
            return Ok(false);
        }
        let next_buffered = sock.buffered_amount.saturating_add(n);
        if next_buffered > OUTBOUND_HARD_CAP {
            drop(sock);
            push_error_and_close(
                state,
                socket_id,
                "node:net outbound buffer hard cap exceeded".to_string(),
                "ERR_NET_WRITE_CAP",
            );
            return Ok(false);
        }
        let ceiling = { state.borrow().net_policy.egress_ceiling_bytes() };
        if let Some(ceiling) = ceiling {
            let next_socket = sock.egress_total.saturating_add(n);
            let next_app = { state.borrow().native_net_egress_bytes.saturating_add(n) };
            if next_socket > ceiling || next_app > ceiling {
                state.borrow_mut().native_net_egress_exhausted = true;
                drop(sock);
                push_error_and_close(
                    state,
                    socket_id,
                    "node:net egress ceiling exceeded".to_string(),
                    "ERR_NET_EGRESS_CAP",
                );
                return Ok(false);
            }
        }
        sock.egress_total = sock.egress_total.saturating_add(n);
        sock.buffered_amount = next_buffered;
        let over_hwm = sock.buffered_amount >= HIGH_WATER_MARK;
        if over_hwm {
            sock.drain_pending = true;
        }
        if let Some(tx) = sock.write_tx.clone() {
            if let Some(ceiling) = ceiling {
                let next_app = state.borrow().native_net_egress_bytes.saturating_add(n);
                debug_assert!(next_app <= ceiling);
                state.borrow_mut().native_net_egress_bytes = next_app;
            }
            (Some(tx), over_hwm)
        } else if sock.connecting {
            if sock.pending_writes.len() >= OUTBOUND_QUEUE_CAP {
                sock.egress_total = sock.egress_total.saturating_sub(n);
                sock.buffered_amount = sock.buffered_amount.saturating_sub(n);
                sock.drain_pending = sock.buffered_amount >= HIGH_WATER_MARK;
                drop(sock);
                push_error_and_close(
                    state,
                    socket_id,
                    "node:net outbound write queue hard cap exceeded".to_string(),
                    "ERR_NET_WRITE_CAP",
                );
                return Ok(false);
            }
            if let Some(ceiling) = ceiling {
                let next_app = state.borrow().native_net_egress_bytes.saturating_add(n);
                debug_assert!(next_app <= ceiling);
                state.borrow_mut().native_net_egress_bytes = next_app;
            }
            sock.pending_writes.push_back(bytes);
            record_net_egress(state, n);
            return Ok(!over_hwm);
        } else {
            sock.egress_total = sock.egress_total.saturating_sub(n);
            sock.buffered_amount = sock.buffered_amount.saturating_sub(n);
            return Err("Socket is not connected".to_string());
        }
    };

    let Some(mut tx) = tx else {
        return Err("Socket is not connected".to_string());
    };
    match tx.try_send(WriteCmd::Data(bytes)) {
        Ok(()) => {
            record_net_egress(state, n);
            Ok(!over_hwm)
        }
        Err(e) => {
            decrement_buffered_amount(state, socket_id, n);
            rollback_egress(state, socket_id, n);
            push_error_and_close(
                state,
                socket_id,
                format!("node:net outbound write queue refused data: {e:?}"),
                "ERR_NET_WRITE_CAP",
            );
            Ok(false)
        }
    }
}

pub fn queue_end(state: &SharedState, socket_id: u32) -> Result<(), String> {
    let mut tx = {
        let socket = lookup_native_socket_state(state, socket_id)
            .ok_or_else(|| "Socket is closed".to_string())?;
        let mut s = socket.borrow_mut();
        s.ended = true;
        if let Some(tx) = s.write_tx.clone() {
            tx
        } else if s.connecting {
            s.pending_end = true;
            return Ok(());
        } else {
            return Err("Socket is not connected".to_string());
        }
    };
    tx.try_send(WriteCmd::End)
        .map_err(|e| format!("end queue closed: {e:?}"))
}

#[cfg(feature = "runtime_native_websocket")]
pub fn queue_start_tls(
    state: &SharedState,
    socket_id: u32,
    opts: TlsOptions,
) -> Result<(), String> {
    if pending_data_events(state, socket_id) {
        push_error_and_close(
            state,
            socket_id,
            "STARTTLS upgrade refused with pending plaintext bytes".to_string(),
            "ERR_TLS_HANDSHAKE",
        );
        return Ok(());
    }
    let mut tx = {
        let socket = lookup_native_socket_state(state, socket_id)
            .ok_or_else(|| "Socket is closed".to_string())?;
        let s = socket.borrow();
        if s.destroyed {
            return Err("Socket is closed".to_string());
        }
        if !s.connected {
            return Err("Socket is not connected".to_string());
        }
        if s.encrypted {
            return Err("Socket is already encrypted".to_string());
        }
        s.write_tx
            .clone()
            .ok_or_else(|| "Socket is not connected".to_string())?
    };
    tx.try_send(WriteCmd::StartTls(opts))
        .map_err(|e| format!("TLS upgrade queue closed: {e:?}"))
}

pub fn spawn_connect_task(state: SharedState, socket_id: u32, host: String, port: u16) {
    let task = async move {
        let Some((addr, tcp)) = connect_tcp(&state, socket_id, host, port).await else {
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

#[cfg(feature = "runtime_native_websocket")]
pub fn spawn_tls_connect_task(
    state: SharedState,
    socket_id: u32,
    host: String,
    port: u16,
    opts: TlsOptions,
) {
    let task = async move {
        let Some((addr, tcp)) = connect_tcp(&state, socket_id, host, port).await else {
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
    host: String,
    port: u16,
) -> Option<(std::net::SocketAddr, TcpStream)> {
    let resolve_host = host.clone();
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

fn resolve_timeout() -> Duration {
    std::env::var("ZEROSHIP_NET_RESOLVE_TIMEOUT_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .map(Duration::from_millis)
        .unwrap_or(RESOLVE_TIMEOUT)
}

async fn await_recv_ready(socket_state: &Rc<RefCell<NativeSocketState>>) -> bool {
    use std::future::poll_fn;
    poll_fn(|cx| {
        let mut s = socket_state.borrow_mut();
        if s.destroyed {
            std::task::Poll::Ready(false)
        } else if !s.paused && s.events.len() < RECV_BACKPRESSURE_CAP {
            std::task::Poll::Ready(true)
        } else {
            s.recv_backpressure_waker = Some(cx.waker().clone());
            std::task::Poll::Pending
        }
    })
    .await
}

fn recv_ready_now(socket_state: &Rc<RefCell<NativeSocketState>>) -> bool {
    let s = socket_state.borrow();
    !s.destroyed && !s.paused && s.events.len() < RECV_BACKPRESSURE_CAP
}

#[allow(clippy::large_enum_variant)]
enum SocketStream {
    Plain(TcpStream),
    #[cfg(feature = "runtime_native_websocket")]
    Tls(TlsStream<TcpStream>),
    Closed,
}

impl AsyncRead for SocketStream {
    async fn read<B: IoBufMut>(&mut self, buf: B) -> compio::buf::BufResult<usize, B> {
        match self {
            SocketStream::Plain(tcp) => tcp.read(buf).await,
            #[cfg(feature = "runtime_native_websocket")]
            SocketStream::Tls(tls) => tls.read(buf).await,
            SocketStream::Closed => compio::buf::BufResult(
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "socket closed")),
                buf,
            ),
        }
    }
}

impl AsyncWrite for SocketStream {
    async fn write<T: IoBuf>(&mut self, buf: T) -> compio::buf::BufResult<usize, T> {
        match self {
            SocketStream::Plain(tcp) => tcp.write(buf).await,
            #[cfg(feature = "runtime_native_websocket")]
            SocketStream::Tls(tls) => tls.write(buf).await,
            SocketStream::Closed => compio::buf::BufResult(
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "socket closed")),
                buf,
            ),
        }
    }

    async fn flush(&mut self) -> std::io::Result<()> {
        match self {
            SocketStream::Plain(_) => Ok(()),
            #[cfg(feature = "runtime_native_websocket")]
            SocketStream::Tls(tls) => tls.flush().await,
            SocketStream::Closed => Ok(()),
        }
    }

    async fn shutdown(&mut self) -> std::io::Result<()> {
        match self {
            SocketStream::Plain(tcp) => tcp.shutdown().await,
            #[cfg(feature = "runtime_native_websocket")]
            SocketStream::Tls(tls) => tls.shutdown().await,
            SocketStream::Closed => Ok(()),
        }
    }
}

enum DriverAction {
    ReadCompleted(compio::buf::BufResult<usize, Vec<u8>>),
    Command(Option<WriteCmd>),
    ReadPermit(bool),
}

async fn run_socket_driver(
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

        let action = if recv_ready_now(&socket_state) {
            let chunk = vec![0u8; READ_CHUNK_SIZE];
            let read_fut = AsyncRead::read(&mut stream, chunk);
            let recv_fut = rx.next();
            let read_fut = std::pin::pin!(read_fut);
            let recv_fut = std::pin::pin!(recv_fut);
            match select(read_fut, recv_fut).await {
                Either::Left((res, _)) => DriverAction::ReadCompleted(res),
                Either::Right((cmd, _)) => DriverAction::Command(cmd),
            }
        } else {
            let permit_fut = await_recv_ready(&socket_state);
            let recv_fut = rx.next();
            let permit_fut = std::pin::pin!(permit_fut);
            let recv_fut = std::pin::pin!(recv_fut);
            match select(permit_fut, recv_fut).await {
                Either::Left((ready, _)) => DriverAction::ReadPermit(ready),
                Either::Right((cmd, _)) => DriverAction::Command(cmd),
            }
        };

        match action {
            DriverAction::ReadPermit(true) => continue,
            DriverAction::ReadPermit(false) => break,
            DriverAction::Command(Some(cmd)) => {
                if !handle_driver_command(&state, socket_id, &mut stream, cmd).await {
                    break;
                }
            }
            DriverAction::Command(None) => break,
            DriverAction::ReadCompleted(res) => {
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
                let data = chunk.as_slice()[..n].to_vec();
                if let Some(socket) = lookup_native_socket_state(&state, socket_id) {
                    socket.borrow_mut().bytes_read += n as u64;
                }
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

async fn handle_driver_command(
    state: &SharedState,
    socket_id: u32,
    stream: &mut SocketStream,
    cmd: WriteCmd,
) -> bool {
    match cmd {
        WriteCmd::Data(bytes) => {
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
        #[cfg(feature = "runtime_native_websocket")]
        WriteCmd::StartTls(opts) => start_tls_in_driver(state, socket_id, stream, opts).await,
    }
}

#[cfg(feature = "runtime_native_websocket")]
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

    let connector_opts = crate::transport::tls::TlsConnectorOptions {
        reject_unauthorized: opts.reject_unauthorized,
        ca_pem: opts.ca_pem.clone(),
    };
    let connector = match crate::transport::tls::build_tls_connector(&connector_opts) {
        Ok(connector) => connector,
        Err(e) => {
            push_error_and_close(
                state,
                socket_id,
                format!("TLS connector failed: {e}"),
                "ERR_TLS_HANDSHAKE",
            );
            return false;
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
            return false;
        }
        Err(_) => {
            push_error_and_close(
                state,
                socket_id,
                "TLS handshake timed out".to_string(),
                "ERR_TLS_HANDSHAKE",
            );
            return false;
        }
    };

    *stream = SocketStream::Tls(tls);
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
    true
}

fn decrement_buffered_amount(state: &SharedState, socket_id: u32, n: u64) {
    let should_drain = if let Some(socket) = lookup_native_socket_state(state, socket_id) {
        let mut s = socket.borrow_mut();
        s.buffered_amount = s.buffered_amount.saturating_sub(n);
        if s.drain_pending && s.buffered_amount < HIGH_WATER_MARK {
            s.drain_pending = false;
            true
        } else {
            false
        }
    } else {
        false
    };
    if should_drain {
        push_event(state, socket_id, SocketEvent::Drain);
    }
}

fn rollback_egress(state: &SharedState, socket_id: u32, n: u64) {
    if let Some(socket) = lookup_native_socket_state(state, socket_id) {
        let mut socket = socket.borrow_mut();
        socket.egress_total = socket.egress_total.saturating_sub(n);
    }
    let mut s = state.borrow_mut();
    s.native_net_egress_bytes = s.native_net_egress_bytes.saturating_sub(n);
}

fn record_net_egress(state: &SharedState, n: u64) {
    let meter = { state.borrow().meter.clone() };
    if let Some(meter) = meter {
        meter.record("egress_bytes", n);
        meter.record("net_egress_bytes", n);
    }
}

fn pending_data_events(state: &SharedState, socket_id: u32) -> bool {
    lookup_native_socket_state(state, socket_id)
        .map(|socket| {
            socket
                .borrow()
                .events
                .iter()
                .any(|event| matches!(event, SocketEvent::Data(_)))
        })
        .unwrap_or(false)
}

fn apply_keep_alive(tcp: &TcpStream, on: bool, initial_delay_ms: u64) -> std::io::Result<()> {
    let sock = SockRef::from(tcp);
    if on {
        let delay = Duration::from_millis(initial_delay_ms.max(1));
        sock.set_tcp_keepalive(&TcpKeepalive::new().with_time(delay))?;
    } else {
        sock.set_keepalive(false)?;
    }
    Ok(())
}

pub fn reserve_socket_slot(state: &SharedState, socket_id: u32) -> Result<(), String> {
    let max = state.borrow().net_policy.max_sockets();
    if max == 0 {
        return Err("node:net capability denied".to_string());
    }
    {
        let s = state.borrow();
        if s.native_net_egress_exhausted {
            return Err("node:net egress ceiling exceeded".to_string());
        }
        if s.active_native_sockets >= max {
            return Err(format!("per-app node:net socket cap exceeded ({max})"));
        }
    }
    crate::transport::net_policy::try_acquire_global_socket()?;
    {
        let mut s = state.borrow_mut();
        s.active_native_sockets += 1;
    }
    if let Some(socket) = lookup_native_socket_state(state, socket_id) {
        let mut sock = socket.borrow_mut();
        sock.counted = true;
        sock.connecting = true;
    }
    Ok(())
}

pub fn release_socket_slot(state: &SharedState, socket_id: u32) {
    let counted = if let Some(socket) = lookup_native_socket_state(state, socket_id) {
        let mut s = socket.borrow_mut();
        if s.counted {
            s.counted = false;
            true
        } else {
            false
        }
    } else {
        false
    };
    if counted {
        {
            let mut s = state.borrow_mut();
            s.active_native_sockets = s.active_native_sockets.saturating_sub(1);
        }
        crate::transport::net_policy::release_global_socket();
    }
}

pub fn destroy_all_sockets(state: &SharedState) -> usize {
    let ids: Vec<u32> = state.borrow().native_sockets.keys().copied().collect();
    let count = ids.len();
    for socket_id in ids {
        destroy_socket(state, socket_id);
    }
    count
}
