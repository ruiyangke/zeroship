//! Native `node:net.Socket` state and compio TCP driver.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::task::Waker;
use std::time::Duration;

use compio::buf::IoBuf;
use compio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use compio::net::TcpStream;
use futures::channel::mpsc;
use futures::StreamExt;
use socket2::{SockRef, TcpKeepalive};

use crate::state::{OpResult, SharedState};

const RECV_BACKPRESSURE_CAP: usize = 256;
const RECV_BACKPRESSURE_RESUME: usize = 128;
const READ_CHUNK_SIZE: usize = 16 * 1024;
pub(crate) const HIGH_WATER_MARK: u64 = 16 * 1024;
const OUTBOUND_HARD_CAP: u64 = 1024 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub enum SocketEvent {
    Connect,
    Ready,
    Data(Vec<u8>),
    Drain,
    End,
    Error { message: String, code: String },
    Close { had_error: bool },
}

#[derive(Debug)]
pub enum WriteCmd {
    Data(Vec<u8>),
    End,
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
    pub tcp: Option<Rc<TcpStream>>,
    pub buffered_amount: u64,
    pub drain_pending: bool,
    pub egress_total: u64,
    pub remote: Option<std::net::SocketAddr>,
    pub bytes_read: u64,
    pub bytes_written: u64,
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
            tcp: None,
            buffered_amount: 0,
            drain_pending: false,
            egress_total: 0,
            remote: None,
            bytes_read: 0,
            bytes_written: 0,
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

fn push_error_and_close(state: &SharedState, socket_id: u32, message: String, code: &str) {
    if let Some(socket) = lookup_native_socket_state(state, socket_id) {
        let mut s = socket.borrow_mut();
        s.had_error = true;
        s.destroyed = true;
        s.write_tx = None;
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
            s.tcp = None;
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
    if let Some(socket) = lookup_native_socket_state(state, socket_id)
        && let Some(tcp) = socket.borrow().tcp.as_ref()
    {
        tcp.set_nodelay(on)?;
    }
    Ok(())
}

pub fn set_keep_alive(
    state: &SharedState,
    socket_id: u32,
    on: bool,
    initial_delay_ms: u64,
) -> std::io::Result<()> {
    if let Some(socket) = lookup_native_socket_state(state, socket_id)
        && let Some(tcp) = socket.borrow().tcp.as_ref()
    {
        let sock = SockRef::from(&**tcp);
        if on {
            let delay = Duration::from_millis(initial_delay_ms.max(1));
            sock.set_tcp_keepalive(&TcpKeepalive::new().with_time(delay))?;
        } else {
            sock.set_keepalive(false)?;
        }
    }
    Ok(())
}

pub fn queue_write(
    state: &SharedState,
    socket_id: u32,
    bytes: Vec<u8>,
) -> Result<bool, String> {
    let n = bytes.len() as u64;
    let (mut tx, over_hwm) = {
        let socket = lookup_native_socket_state(state, socket_id)
            .ok_or_else(|| "Socket is closed".to_string())?;
        let mut sock = socket.borrow_mut();
        if sock.destroyed || sock.ended {
            return Err("write after end".to_string());
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
            let next_app = { state.borrow().native_net_egress_bytes.saturating_add(n) };
            if next_app > ceiling {
                drop(sock);
                push_error_and_close(
                    state,
                    socket_id,
                    "node:net egress ceiling exceeded".to_string(),
                    "ERR_NET_EGRESS_CAP",
                );
                return Ok(false);
            }
            state.borrow_mut().native_net_egress_bytes = next_app;
        }
        sock.egress_total = sock.egress_total.saturating_add(n);
        sock.buffered_amount = next_buffered;
        let over_hwm = sock.buffered_amount >= HIGH_WATER_MARK;
        if over_hwm {
            sock.drain_pending = true;
        }
        let tx = sock
            .write_tx
            .clone()
            .ok_or_else(|| "Socket is not connected".to_string())?;
        (tx, over_hwm)
    };

    match tx.try_send(WriteCmd::Data(bytes)) {
        Ok(()) => Ok(!over_hwm),
        Err(e) => {
            decrement_buffered_amount(state, socket_id, n);
            Err(format!("write queue closed: {e:?}"))
        }
    }
}

pub fn queue_end(state: &SharedState, socket_id: u32) -> Result<(), String> {
    let mut tx = {
        let socket = lookup_native_socket_state(state, socket_id)
            .ok_or_else(|| "Socket is closed".to_string())?;
        let mut s = socket.borrow_mut();
        s.ended = true;
        s.write_tx
            .clone()
            .ok_or_else(|| "Socket is not connected".to_string())?
    };
    tx.try_send(WriteCmd::End)
        .map_err(|e| format!("end queue closed: {e:?}"))
}

pub fn spawn_connect_task(state: SharedState, socket_id: u32, host: String, port: u16) {
    let task = async move {
        let resolve_host = host.clone();
        let resolved = compio::time::timeout(
            RESOLVE_TIMEOUT,
            compio::runtime::spawn_blocking(move || {
                crate::fetch::resolve_and_check_ssrf(&resolve_host, port)
            }),
        )
        .await;
        let addr = match resolved {
            Ok(Ok(Ok(addr))) => addr,
            Ok(Ok(Err(e))) => {
                push_error_and_close(&state, socket_id, format!("SSRF: {e}"), "ERR_NET_SSRF");
                return;
            }
            Ok(Err(_join)) => {
                push_error_and_close(
                    &state,
                    socket_id,
                    "DNS resolve task failed".to_string(),
                    "ERR_NET_DNS",
                );
                return;
            }
            Err(_) => {
                push_error_and_close(
                    &state,
                    socket_id,
                    "DNS resolve timed out".to_string(),
                    "ERR_NET_DNS_TIMEOUT",
                );
                return;
            }
        };

        let tcp = match compio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr)).await {
            Ok(Ok(tcp)) => tcp,
            Ok(Err(e)) => {
                push_error_and_close(&state, socket_id, format!("connect failed: {e}"), "ECONNREFUSED");
                return;
            }
            Err(_) => {
                push_error_and_close(
                    &state,
                    socket_id,
                    "connect timed out".to_string(),
                    "ETIMEDOUT",
                );
                return;
            }
        };

        let _ = tcp.set_nodelay(true);
        let tcp = Rc::new(tcp);
        let (tx, rx) = mpsc::channel::<WriteCmd>(128);
        if let Some(socket) = lookup_native_socket_state(&state, socket_id) {
            let mut s = socket.borrow_mut();
            if s.destroyed {
                release_socket_slot(&state, socket_id);
                return;
            }
            s.remote = Some(addr);
            s.tcp = Some(tcp.clone());
            s.write_tx = Some(tx);
            s.connecting = false;
            s.connected = true;
        }

        push_event(&state, socket_id, SocketEvent::Connect);
        push_event(&state, socket_id, SocketEvent::Ready);
        run_plain_driver(state, socket_id, tcp, rx).await;
    };
    compio::runtime::spawn(crate::panic_util::guard("node-net-connect", task)).detach();
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

async fn run_plain_driver(
    state: SharedState,
    socket_id: u32,
    tcp: Rc<TcpStream>,
    rx: mpsc::Receiver<WriteCmd>,
) {
    let reader_task = {
        let state = state.clone();
        let tcp = tcp.clone();
        async move {
            let r = TcpReadHalf { tcp };
            run_reader_loop(&state, socket_id, r).await;
        }
    };
    let writer_task = {
        let state = state.clone();
        async move {
            let w = TcpWriteHalf { tcp };
            run_writer_loop(&state, socket_id, w, rx).await;
        }
    };

    let reader = compio::runtime::spawn(crate::panic_util::guard("node-net-reader", reader_task));
    let writer = compio::runtime::spawn(crate::panic_util::guard("node-net-writer", writer_task));
    let _ = reader.await;
    if let Some(socket) = lookup_native_socket_state(&state, socket_id) {
        socket.borrow_mut().write_tx = None;
    }
    let _ = writer.await;
    let had_error = lookup_native_socket_state(&state, socket_id)
        .map(|s| s.borrow().had_error)
        .unwrap_or(false);
    push_close_once(&state, socket_id, had_error);
}

struct TcpReadHalf {
    tcp: Rc<TcpStream>,
}

impl AsyncRead for TcpReadHalf {
    async fn read<B: compio::buf::IoBufMut>(
        &mut self,
        buf: B,
    ) -> compio::buf::BufResult<usize, B> {
        let r: &TcpStream = &self.tcp;
        let mut r = r;
        r.read(buf).await
    }
}

struct TcpWriteHalf {
    tcp: Rc<TcpStream>,
}

impl AsyncWrite for TcpWriteHalf {
    async fn write<T: IoBuf>(&mut self, buf: T) -> compio::buf::BufResult<usize, T> {
        let w: &TcpStream = &self.tcp;
        let mut w = w;
        w.write(buf).await
    }

    async fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }

    async fn shutdown(&mut self) -> std::io::Result<()> {
        let w: &TcpStream = &self.tcp;
        let mut w = w;
        w.shutdown().await
    }
}

async fn run_reader_loop<R>(state: &SharedState, socket_id: u32, mut r: R)
where
    R: AsyncRead + Unpin,
{
    let socket_state = match lookup_native_socket_state(state, socket_id) {
        Some(s) => s,
        None => return,
    };

    loop {
        if !await_recv_ready(&socket_state).await {
            return;
        }
        let chunk = vec![0u8; READ_CHUNK_SIZE];
        let res = AsyncRead::read(&mut r, chunk).await;
        let n = match res.0 {
            Ok(n) => n,
            Err(e) => {
                push_error_and_close(state, socket_id, format!("read error: {e}"), "ERR_NET_READ");
                return;
            }
        };
        if n == 0 {
            if let Some(socket) = lookup_native_socket_state(state, socket_id) {
                socket.borrow_mut().write_tx = None;
            }
            push_event(state, socket_id, SocketEvent::End);
            return;
        }
        let chunk = res.1;
        let data = chunk.as_slice()[..n].to_vec();
        if let Some(socket) = lookup_native_socket_state(state, socket_id) {
            socket.borrow_mut().bytes_read += n as u64;
        }
        push_event(state, socket_id, SocketEvent::Data(data));
    }
}

async fn run_writer_loop<W>(
    state: &SharedState,
    socket_id: u32,
    mut w: W,
    mut rx: mpsc::Receiver<WriteCmd>,
) where
    W: AsyncWrite + Unpin,
{
    while let Some(cmd) = rx.next().await {
        match cmd {
            WriteCmd::Data(bytes) => {
                let n = bytes.len() as u64;
                let res = w.write_all(bytes).await;
                if let Err(e) = res.0 {
                    push_error_and_close(
                        state,
                        socket_id,
                        format!("write error: {e}"),
                        "ERR_NET_WRITE",
                    );
                    return;
                }
                if let Some(socket) = lookup_native_socket_state(state, socket_id) {
                    let mut s = socket.borrow_mut();
                    s.bytes_written = s.bytes_written.saturating_add(n);
                }
                decrement_buffered_amount(state, socket_id, n);
            }
            WriteCmd::End => {
                let _ = w.shutdown().await;
                return;
            }
        }
    }
    let _ = w.shutdown().await;
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

pub fn reserve_socket_slot(state: &SharedState, socket_id: u32) -> Result<(), String> {
    let max = state.borrow().net_policy.max_sockets();
    if max == 0 {
        return Err("node:net capability denied".to_string());
    }
    {
        let s = state.borrow();
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
