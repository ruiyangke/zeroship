//! Per-WS network plumbing — handshake spawn + bidirectional reader/writer.
//!
//! ## Architecture
//!
//! Each established WebSocket runs **bidirectionally without starvation**:
//! a recv from the V8 thread can be processed at any time, even while
//! a frame is being assembled from the wire. Sends and reads NEVER
//! block each other.
//!
//! ### Plain TCP (`ws://`) — true two-task split
//!
//! Reader and writer are independent compio tasks sharing
//! `Rc<TcpStream>`. They borrow the stream immutably via
//! `&TcpStream: AsyncRead + AsyncWrite` — io_uring multiplexes
//! concurrent submissions on the same fd, so reads and writes proceed
//! in parallel.
//!
//! ### TLS (`wss://`) — single-task cooperative interleave
//!
//! `compio_tls::TlsStream` is not aliasable (cipher state is unique).
//! A single owner task runs a loop:
//!
//!   1. Try to decode a frame from `read_buffer` (sans-IO; pure
//!      function on the buffer + reader state).
//!   2. If a frame: dispatch.
//!   3. Otherwise: `select(tls.read(chunk), rx.next())`.
//!      - read resolved → append bytes to `read_buffer`, retry decode.
//!      - recv resolved → encode, write to `tls`, retry decode.
//!
//! The `tls.read(chunk)` future reads INTO a fresh `chunk` buffer per
//! iteration. If we cancel mid-read (because recv resolved first),
//! the future drops without losing any state from `read_buffer` —
//! `chunk` gets discarded but `read_buffer` is untouched. The recv
//! future resolves only when V8 actually has a frame to send.
//!
//! In practice the `read` is almost always cheap (often returns
//! `WouldBlock` or one chunk) so the cancellation cost is negligible.
//!
//! ## Leftover bytes from the handshake
//!
//! The HTTP handshake reads a 4 KiB chunk at a time; the server may
//! pipeline a frame past the 101 response. Those bytes are returned
//! from the handshake in `Established::leftover` and seeded into the
//! reader's buffer BEFORE the first network read.
//!
//! For the plain-TCP driver we use a `ChainReader` adapter that yields
//! the leftover slice first, then the underlying stream. For the TLS
//! driver we just prepend to `read_buffer`.
//!
//! ## Event flow JS-ward
//!
//! Every queued `WsEvent` triggers a one-shot
//! `OpResult::WebSocketEvent` future; the runtime arm in `runtime.rs`
//! calls `dispatch_ws_event`, which drains the per-WS queue (so
//! multiple events queued in one batch dispatch in one V8 turn).
//!
//! ## Backpressure
//!
//! Receive: when the per-WS event queue exceeds `RECV_BACKPRESSURE_CAP`
//! the reader awaits drain (the V8 pump wakes it once the queue drops
//! below `RECV_BACKPRESSURE_RESUME`). Combined with TCP's flow control
//! this propagates to the peer.
//!
//! Send: bounded by `bufferedAmount` cap (per WHATWG §3.1). The V8
//! wrapper short-circuits `send()` when `full` is set.
//!
//! ## Close handshake (RFC 6455 §7.1)
//!
//! - Local close: V8 calls `close()` → enqueues `WsFrame::Close` on
//!   `send_tx` → writer encodes and sends. The reader continues until
//!   it sees the peer's Close echo.
//! - Peer close: reader sees a `Close` frame, queues a `Close` echo
//!   on the writer's channel, fires CloseEvent, then exits. The
//!   writer drains the echo and exits when its channel closes.
//!
//! ## Spec citations
//! - `establish a WebSocket connection`: WHATWG §4.1.
//! - `WebSocket message received`: WHATWG §4.4.
//! - `closing handshake started`: WHATWG §4.5.
//! - `connection closed`: WHATWG §4.6.

#![cfg(feature = "runtime_native_websocket")]

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;
use std::task::Waker;

use compio::buf::IoBuf;
use compio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use compio::net::TcpStream;
use compio_tls::TlsStream;
use futures::channel::mpsc;
use futures::StreamExt;

use super::frame_reader::{DecodedFrame, FrameReader, StepResult};
use super::frame_writer::{
    encode_binary_frame, encode_close_frame, encode_pong_frame, encode_text_frame,
};
use super::handshake::{Established, EstablishedStream, HandshakeError, HandshakeOptions};
use super::{WebSocketImpl, WsFrame};
use crate::state::{OpResult, SharedState};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Per-WS event delivered from the network task to the V8 pump arm.
#[derive(Debug, Clone)]
pub enum WsEvent {
    /// Handshake completed. Fires `open` event on the wrapper.
    Open {
        protocol: String,
        extensions: String,
    },
    /// Text frame received.
    MessageText(String),
    /// Binary frame received.
    MessageBinary(Vec<u8>),
    /// Connection closed (peer Close, our Close ACK'd, or abnormal).
    Close {
        code: u16,
        reason: String,
        was_clean: bool,
    },
    /// Connection-failed signal — fires `error` event.
    Error { reason: String },
}

/// Per-WS native state — shared between the connect task, both pumps,
/// and the V8-thread dispatch arm.
pub struct NativeWsState {
    pub events: VecDeque<WsEvent>,
    pub recv_backpressure_waker: Option<Waker>,
    pub cancel: bool,
    pub cancel_reason: String,
    pub connect_waker: Option<Waker>,
    pub buffered_amount: Rc<Cell<u64>>,
    pub full: Rc<Cell<bool>>,
    pub is_pair: bool,
    /// Sender end of the writer channel. `None` until the connect task
    /// has finished the handshake.
    pub send_tx: Option<mpsc::UnboundedSender<WsFrame>>,
    /// Cumulative event log for test inspection. Every event pushed
    /// onto `events` is also appended here BEFORE dispatch drains it.
    /// Empty in production builds; only the tests crate populates it
    /// because it's `RefCell<None>` by default and needs an explicit
    /// opt-in via `enable_event_log()` (called by test setup).
    pub event_log: Option<Vec<WsEvent>>,
    /// Kernel-owned WebSocket outbound channel. When set, events that
    /// would otherwise dispatch to a JS wrapper are siphoned to this
    /// channel instead. Used by `serve.rs`'s dev-mode WS pump to
    /// forward server-side `socket.send()` frames out to the TCP
    /// client. The dispatch arm checks this BEFORE attempting V8
    /// dispatch.
    pub kernel_outbound: Option<mpsc::UnboundedSender<WsEvent>>,
}

impl NativeWsState {
    pub fn new() -> Self {
        NativeWsState {
            events: VecDeque::new(),
            recv_backpressure_waker: None,
            cancel: false,
            cancel_reason: String::new(),
            connect_waker: None,
            buffered_amount: Rc::new(Cell::new(0)),
            full: Rc::new(Cell::new(false)),
            is_pair: false,
            send_tx: None,
            event_log: None,
            kernel_outbound: None,
        }
    }

    pub fn new_pair() -> Self {
        NativeWsState {
            is_pair: true,
            ..NativeWsState::new()
        }
    }

    /// Test helper: enable cumulative event-log capture. Every event
    /// pushed onto `events` is also appended to `event_log` BEFORE
    /// dispatch drains it. Production code never calls this; the
    /// integration test in `tests/subscription.rs` flips this on for
    /// each test isolate to inspect event flow without racing the
    /// pump's dispatch.
    pub fn enable_event_log(&mut self) {
        if self.event_log.is_none() {
            self.event_log = Some(Vec::new());
        }
    }
}

const RECV_BACKPRESSURE_CAP: usize = 256;
const RECV_BACKPRESSURE_RESUME: usize = 128;

/// Outcome of one iteration of the TLS driver's `select` between a
/// network read and a user send. Used to break the borrow on `tls`
/// before writing.
enum Action {
    ReadCompleted(compio::buf::BufResult<usize, Vec<u8>>),
    RecvCompleted(Option<WsFrame>),
}

// ---------------------------------------------------------------------------
// State registration
// ---------------------------------------------------------------------------

pub fn alloc_native_ws_id(state: &SharedState) -> u32 {
    let mut s = state.borrow_mut();
    let id = s.next_native_ws_id;
    s.next_native_ws_id = id.checked_add(1).unwrap_or(1);
    s.native_websockets
        .insert(id, Rc::new(RefCell::new(NativeWsState::new())));
    id
}

pub fn free_native_ws_state(state: &SharedState, ws_id: u32) {
    let mut s = state.borrow_mut();
    s.native_websockets.remove(&ws_id);
}

pub fn lookup_native_ws_state(
    state: &SharedState,
    ws_id: u32,
) -> Option<Rc<RefCell<NativeWsState>>> {
    state.borrow().native_websockets.get(&ws_id).cloned()
}

fn push_event(state: &SharedState, ws_id: u32, event: WsEvent) {
    push_event_pub(state, ws_id, event);
}

/// Public test-facing event push: identical to `push_event` but
/// callable from outside the crate. Used by the subscription test
/// suite to simulate inbound frames on the server-side WS.
///
/// If the WS is kernel-owned (i.e. `kernel_outbound` is set —
/// see `serve.rs`'s dev-mode WS pump), the event is routed to the
/// kernel channel BYPASSING V8 dispatch, since the kernel-side
/// wrapper has no JS listeners attached.
pub fn push_event_pub(state: &SharedState, ws_id: u32, event: WsEvent) {
    let ws = match lookup_native_ws_state(state, ws_id) {
        Some(w) => w,
        None => return,
    };
    let kernel_tx = {
        let s = ws.borrow();
        s.kernel_outbound.clone()
    };
    if let Some(tx) = kernel_tx {
        // Mirror into log first (so tests still see the event).
        if let Some(log) = ws.borrow_mut().event_log.as_mut() {
            log.push(event.clone());
        }
        let _ = tx.unbounded_send(event);
        return;
    }

    {
        let mut s = ws.borrow_mut();
        if let Some(log) = s.event_log.as_mut() {
            log.push(event.clone());
        }
        s.events.push_back(event);
    }

    let id = ws_id;
    let fut: std::pin::Pin<Box<dyn std::future::Future<Output = OpResult>>> =
        Box::pin(async move { OpResult::WebSocketEvent { ws_id: id } });
    {
        let mut s = state.borrow_mut();
        s.spawned_ops.push(fut);
    }

    let notify = state.borrow().pump_notify_tx.clone();
    if let Some(mut tx) = notify {
        let _ = tx.try_send(());
    }
}

async fn await_recv_drain(ws_state: &Rc<RefCell<NativeWsState>>) {
    use std::future::poll_fn;
    poll_fn(|cx| {
        let mut s = ws_state.borrow_mut();
        if s.events.len() < RECV_BACKPRESSURE_CAP {
            std::task::Poll::Ready(())
        } else {
            s.recv_backpressure_waker = Some(cx.waker().clone());
            std::task::Poll::Pending
        }
    })
    .await
}

pub fn drain_events(state: &SharedState, ws_id: u32) -> Vec<WsEvent> {
    let Some(ws) = lookup_native_ws_state(state, ws_id) else {
        return Vec::new();
    };
    let drained: Vec<WsEvent> = {
        let mut s = ws.borrow_mut();
        s.events.drain(..).collect()
    };
    let waker = {
        let mut s = ws.borrow_mut();
        if s.events.len() < RECV_BACKPRESSURE_RESUME {
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

// ---------------------------------------------------------------------------
// Connect task
// ---------------------------------------------------------------------------

pub fn spawn_connect_task(
    state: SharedState,
    ws_id: u32,
    url: url::Url,
    opts: HandshakeOptions,
) {
    let state_for_task = state.clone();
    let task = async move {
        if let Some(ws) = lookup_native_ws_state(&state_for_task, ws_id) {
            if ws.borrow().cancel {
                let reason = ws.borrow().cancel_reason.clone();
                push_event(
                    &state_for_task,
                    ws_id,
                    WsEvent::Error {
                        reason: format!("aborted: {reason}"),
                    },
                );
                push_event(
                    &state_for_task,
                    ws_id,
                    WsEvent::Close {
                        code: 1006,
                        reason,
                        was_clean: false,
                    },
                );
                return;
            }
        }

        let max_frame_size = opts.max_frame_size;
        let max_message_size = opts.max_message_size;

        let result = super::handshake::run_handshake(url, opts).await;

        match result {
            Ok(Established {
                stream,
                leftover,
                protocol,
                extensions,
            }) => {
                push_event(
                    &state_for_task,
                    ws_id,
                    WsEvent::Open {
                        protocol,
                        extensions,
                    },
                );

                let (tx, rx) = mpsc::unbounded::<WsFrame>();
                if let Some(ws) = lookup_native_ws_state(&state_for_task, ws_id) {
                    ws.borrow_mut().send_tx = Some(tx.clone());
                }

                run_socket_driver(
                    state_for_task.clone(),
                    ws_id,
                    stream,
                    leftover,
                    rx,
                    tx,
                    max_frame_size,
                    max_message_size,
                )
                .await;
            }
            Err(HandshakeError::Aborted { signal_reason }) => {
                let reason = signal_reason.unwrap_or_default();
                push_event(
                    &state_for_task,
                    ws_id,
                    WsEvent::Error {
                        reason: format!("aborted: {reason}"),
                    },
                );
                push_event(
                    &state_for_task,
                    ws_id,
                    WsEvent::Close {
                        code: 1006,
                        reason,
                        was_clean: false,
                    },
                );
            }
            Err(e) => {
                push_event(
                    &state_for_task,
                    ws_id,
                    WsEvent::Error {
                        reason: format!("{e}"),
                    },
                );
                push_event(
                    &state_for_task,
                    ws_id,
                    WsEvent::Close {
                        code: 1006,
                        reason: String::new(),
                        was_clean: false,
                    },
                );
            }
        }
    };

    compio::runtime::spawn(crate::panic_util::guard("websocket-connect", task)).detach();
}

// ---------------------------------------------------------------------------
// run_socket_driver — dispatch by stream variant
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn run_socket_driver(
    state: SharedState,
    ws_id: u32,
    stream: EstablishedStream,
    leftover: Vec<u8>,
    rx: mpsc::UnboundedReceiver<WsFrame>,
    tx: mpsc::UnboundedSender<WsFrame>,
    max_frame_size: usize,
    max_message_size: usize,
) {
    match stream {
        EstablishedStream::Plain(tcp) => {
            run_plain_driver(
                state,
                ws_id,
                tcp,
                leftover,
                rx,
                tx,
                max_frame_size,
                max_message_size,
            )
            .await
        }
        EstablishedStream::Tls(tls) => {
            run_tls_driver(
                state,
                ws_id,
                tls,
                leftover,
                rx,
                tx,
                max_frame_size,
                max_message_size,
            )
            .await
        }
    }
}

// ---------------------------------------------------------------------------
// Plain-TCP driver — true two-task split
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn run_plain_driver(
    state: SharedState,
    ws_id: u32,
    tcp: TcpStream,
    leftover: Vec<u8>,
    rx: mpsc::UnboundedReceiver<WsFrame>,
    tx: mpsc::UnboundedSender<WsFrame>,
    max_frame_size: usize,
    max_message_size: usize,
) {
    let tcp = Rc::new(tcp);

    let reader_task = {
        let state = state.clone();
        let tcp = tcp.clone();
        let tx_for_reader = tx.clone();
        async move {
            let r = TcpReadHalf { tcp };
            run_reader_loop(
                &state,
                ws_id,
                r,
                leftover,
                tx_for_reader,
                max_frame_size,
                max_message_size,
            )
            .await;
        }
    };

    let writer_task = {
        let state = state.clone();
        let tcp = tcp.clone();
        async move {
            let w = TcpWriteHalf { tcp };
            run_writer_loop(&state, ws_id, w, rx).await;
        }
    };

    let reader_handle = compio::runtime::spawn(crate::panic_util::guard("ws-reader", reader_task));
    let writer_handle = compio::runtime::spawn(crate::panic_util::guard("ws-writer", writer_task));

    drop(tx);
    let _ = reader_handle.await;
    let _ = writer_handle.await;
    // Free per-WS state — the JS side may still hold the wrapper, but
    // the network is dead. The state remove itself happens during
    // wrapper finalisation; we just clear the send channel.
    if let Some(ws) = lookup_native_ws_state(&state, ws_id) {
        ws.borrow_mut().send_tx = None;
    }
}

// ---------------------------------------------------------------------------
// TLS driver — single-task cooperative interleave
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn run_tls_driver(
    state: SharedState,
    ws_id: u32,
    mut tls: TlsStream<TcpStream>,
    leftover: Vec<u8>,
    mut rx: mpsc::UnboundedReceiver<WsFrame>,
    _tx: mpsc::UnboundedSender<WsFrame>,
    max_frame_size: usize,
    max_message_size: usize,
) {
    use futures::future::{select, Either};

    let mut reader = FrameReader::new(max_frame_size, max_message_size);
    let mut read_buffer: Vec<u8> = leftover;
    let ws_state = match lookup_native_ws_state(&state, ws_id) {
        Some(w) => w,
        None => return,
    };

    let mut sent_close = false;
    let mut peer_closed = false;
    // Self-feeding pong / close-echo queue: written to by the read
    // pipeline, consumed BEFORE we touch the user's `rx`. (Otherwise
    // a fast peer that sends only Pings could starve user sends.)
    let mut internal_outbound: VecDeque<WsFrame> = VecDeque::new();

    loop {
        await_recv_drain(&ws_state).await;
        if sent_close && peer_closed {
            break;
        }

        // Attempt to drain frames from the in-memory buffer until we
        // either need more bytes or run out of work.
        loop {
            match reader.decode_step(&read_buffer) {
                Ok(StepResult::Frame { frame, consumed }) => {
                    read_buffer.drain(..consumed);
                    let cont = handle_frame_tls(
                        &state,
                        ws_id,
                        frame,
                        &mut internal_outbound,
                        sent_close,
                        &mut peer_closed,
                    );
                    if !cont {
                        // Flush internal_outbound (close echo / pong)
                        // before exiting so the peer sees a clean
                        // handshake.
                        while let Some(f) = internal_outbound.pop_front() {
                            if let Err(e) =
                                write_frame(&mut tls, &state, ws_id, f, &mut sent_close).await
                            {
                                fail_connection(&state, ws_id, format!("write error: {e}"));
                                return;
                            }
                        }
                        let _ = tls.shutdown().await;
                        return;
                    }
                    continue;
                }
                Ok(StepResult::NeedMoreContinuation { consumed }) => {
                    read_buffer.drain(..consumed);
                    continue;
                }
                Ok(StepResult::NeedMoreBytes) => break,
                Err(e) => {
                    fail_connection(&state, ws_id, e);
                    let _ = tls.shutdown().await;
                    return;
                }
            }
        }

        // Drain pending internal_outbound BEFORE waiting for either
        // a network read OR a user send.
        while let Some(f) = internal_outbound.pop_front() {
            if let Err(e) = write_frame(&mut tls, &state, ws_id, f, &mut sent_close).await {
                fail_connection(&state, ws_id, format!("write error: {e}"));
                return;
            }
        }

        // Issue ONE read into a fresh buffer chunk OR receive ONE
        // frame from the V8 thread, whichever resolves first.
        //
        // Cancellation safety: if the read is cancelled mid-call (a
        // recv resolved first), compio cancels the underlying io_uring
        // submission. Any bytes that arrived but didn't get into our
        // chunk are still TCP-buffered at the kernel — the next read
        // picks them up. `read_buffer` is untouched. The recv future
        // is independent of the stream borrow, so dropping it has no
        // I/O side effects.
        //
        // Why we recreate the read future each iteration instead of
        // preserving the surviving half: the borrow checker won't let
        // us hold the surviving read (which carries `&mut tls`) and
        // also write to `tls` in the same scope. By dropping the
        // surviver before the write, we release the borrow. The cost
        // is a wasted io_uring submission per recv-wins iteration
        // (typically <1 syscall — the cancellation is async).
        let chunk = vec![0u8; 4096];
        let action = {
            let read_fut = AsyncRead::read(&mut tls, chunk);
            let recv_fut = rx.next();
            let read_fut = std::pin::pin!(read_fut);
            let recv_fut = std::pin::pin!(recv_fut);
            match select(read_fut, recv_fut).await {
                Either::Left((res, _surviving_recv)) => Action::ReadCompleted(res),
                Either::Right((maybe_frame, _surviving_read)) => {
                    Action::RecvCompleted(maybe_frame)
                }
            }
        };
        // Both futures are dropped here — the `&mut tls` borrow is
        // released and we can write below.

        match action {
            Action::ReadCompleted(res) => {
                let n = match res.0 {
                    Ok(n) => n,
                    Err(e) => {
                        fail_connection(&state, ws_id, e);
                        return;
                    }
                };
                if n == 0 {
                    fail_connection(&state, ws_id, "TCP EOF without Close frame");
                    return;
                }
                let chunk = res.1;
                read_buffer.extend_from_slice(&chunk.as_slice()[..n]);
            }
            Action::RecvCompleted(maybe_frame) => {
                let Some(frame) = maybe_frame else {
                    if !sent_close {
                        let close = WsFrame::Close {
                            code: None,
                            reason: String::new(),
                        };
                        if let Err(e) =
                            write_frame(&mut tls, &state, ws_id, close, &mut sent_close).await
                        {
                            fail_connection(&state, ws_id, format!("write error: {e}"));
                            return;
                        }
                    }
                    if peer_closed {
                        break;
                    }
                    continue;
                };
                if let Err(e) = write_frame(&mut tls, &state, ws_id, frame, &mut sent_close).await {
                    fail_connection(&state, ws_id, format!("write error: {e}"));
                    return;
                }
                if sent_close && peer_closed {
                    break;
                }
            }
        }
    }

    let _ = tls.shutdown().await;
    if let Some(ws) = lookup_native_ws_state(&state, ws_id) {
        ws.borrow_mut().send_tx = None;
    }
}

/// Side-effect interpreter for a decoded TLS frame. Pushes user-facing
/// events to V8 and queues internal outbound frames (Pong / Close echo).
/// Returns `false` to break the outer loop.
fn handle_frame_tls(
    state: &SharedState,
    ws_id: u32,
    frame: DecodedFrame,
    internal_outbound: &mut VecDeque<WsFrame>,
    sent_close: bool,
    peer_closed: &mut bool,
) -> bool {
    match frame {
        DecodedFrame::Text(s) => {
            push_event(state, ws_id, WsEvent::MessageText(s));
            true
        }
        DecodedFrame::Binary(b) => {
            push_event(state, ws_id, WsEvent::MessageBinary(b));
            true
        }
        DecodedFrame::Ping(payload) => {
            internal_outbound.push_back(WsFrame::Pong(payload));
            true
        }
        DecodedFrame::Pong(_) => true,
        DecodedFrame::Close { code, reason } => {
            push_event(
                state,
                ws_id,
                WsEvent::Close {
                    code,
                    reason: reason.clone(),
                    was_clean: true,
                },
            );
            *peer_closed = true;
            if !sent_close {
                // Echo the close back — clean handshake.
                internal_outbound.push_back(WsFrame::Close {
                    code: if code == 1005 { None } else { Some(code) },
                    reason: String::new(),
                });
            }
            // Exit the outer loop; the writer drains internal_outbound
            // before shutdown.
            false
        }
    }
}

// ---------------------------------------------------------------------------
// Plain-TCP read/write halves (shared `Rc<TcpStream>`)
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Reader / writer loops (plain TCP)
// ---------------------------------------------------------------------------

async fn run_reader_loop<R>(
    state: &SharedState,
    ws_id: u32,
    mut r: R,
    leftover: Vec<u8>,
    tx: mpsc::UnboundedSender<WsFrame>,
    max_frame_size: usize,
    max_message_size: usize,
) where
    R: AsyncRead + Unpin,
{
    let mut reader = FrameReader::new(max_frame_size, max_message_size);
    let mut read_buffer: Vec<u8> = leftover;
    let ws_state = match lookup_native_ws_state(state, ws_id) {
        Some(w) => w,
        None => return,
    };

    loop {
        await_recv_drain(&ws_state).await;

        // Drain frames already in the buffer.
        loop {
            match reader.decode_step(&read_buffer) {
                Ok(StepResult::Frame { frame, consumed }) => {
                    read_buffer.drain(..consumed);
                    if !handle_frame_plain(state, ws_id, frame, &tx) {
                        return;
                    }
                    continue;
                }
                Ok(StepResult::NeedMoreContinuation { consumed }) => {
                    read_buffer.drain(..consumed);
                    continue;
                }
                Ok(StepResult::NeedMoreBytes) => break,
                Err(e) => {
                    fail_connection(state, ws_id, e);
                    return;
                }
            }
        }

        // Issue a read.
        let chunk = vec![0u8; 4096];
        let res = AsyncRead::read(&mut r, chunk).await;
        let n = match res.0 {
            Ok(n) => n,
            Err(e) => {
                fail_connection(state, ws_id, e);
                return;
            }
        };
        if n == 0 {
            fail_connection(state, ws_id, "TCP EOF without Close frame");
            return;
        }
        let chunk = res.1;
        read_buffer.extend_from_slice(&chunk.as_slice()[..n]);
    }
}

/// Side-effect interpreter for a decoded plain-TCP frame. Returns
/// `false` to break the read loop.
fn handle_frame_plain(
    state: &SharedState,
    ws_id: u32,
    frame: DecodedFrame,
    tx: &mpsc::UnboundedSender<WsFrame>,
) -> bool {
    match frame {
        DecodedFrame::Text(s) => {
            push_event(state, ws_id, WsEvent::MessageText(s));
            true
        }
        DecodedFrame::Binary(b) => {
            push_event(state, ws_id, WsEvent::MessageBinary(b));
            true
        }
        DecodedFrame::Ping(payload) => {
            let _ = tx.unbounded_send(WsFrame::Pong(payload));
            true
        }
        DecodedFrame::Pong(_) => true,
        DecodedFrame::Close { code, reason } => {
            // Echo Close back via the writer channel.
            let echo = WsFrame::Close {
                code: if code == 1005 { None } else { Some(code) },
                reason: String::new(),
            };
            let _ = tx.unbounded_send(echo);
            push_event(
                state,
                ws_id,
                WsEvent::Close {
                    code,
                    reason,
                    was_clean: true,
                },
            );
            false
        }
    }
}

async fn run_writer_loop<W>(
    state: &SharedState,
    ws_id: u32,
    mut w: W,
    mut rx: mpsc::UnboundedReceiver<WsFrame>,
) where
    W: AsyncWrite + Unpin,
{
    let mut sent_close = false;
    while let Some(frame) = rx.next().await {
        if let Err(e) = write_frame(&mut w, state, ws_id, frame, &mut sent_close).await {
            fail_connection(state, ws_id, format!("write error: {e}"));
            return;
        }
    }
    let _ = w.shutdown().await;
}

async fn write_frame<W>(
    w: &mut W,
    state: &SharedState,
    ws_id: u32,
    frame: WsFrame,
    sent_close: &mut bool,
) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    match frame {
        WsFrame::Text(s) => {
            let n = s.len() as u64;
            let bytes = encode_text_frame(&s);
            let res = w.write_all(bytes).await;
            res.0?;
            decrement_buffered_amount(state, ws_id, n);
        }
        WsFrame::Binary(b) => {
            let n = b.len() as u64;
            let bytes = encode_binary_frame(&b);
            let res = w.write_all(bytes).await;
            res.0?;
            decrement_buffered_amount(state, ws_id, n);
        }
        WsFrame::Blob { handle: _, size } => {
            // v1 ships text/Binary fast paths only.
            decrement_buffered_amount(state, ws_id, size);
        }
        WsFrame::Pong(payload) => {
            let bytes = encode_pong_frame(&payload);
            let res = w.write_all(bytes).await;
            res.0?;
        }
        WsFrame::Close { code, reason } => {
            *sent_close = true;
            let bytes = encode_close_frame(code, &reason);
            let res = w.write_all(bytes).await;
            res.0?;
        }
    }
    Ok(())
}

fn fail_connection<E: std::fmt::Display>(state: &SharedState, ws_id: u32, err: E) {
    push_event(
        state,
        ws_id,
        WsEvent::Error {
            reason: format!("{err}"),
        },
    );
    push_event(
        state,
        ws_id,
        WsEvent::Close {
            code: 1006,
            reason: String::new(),
            was_clean: false,
        },
    );
    if let Some(ws) = lookup_native_ws_state(state, ws_id) {
        let mut s = ws.borrow_mut();
        s.buffered_amount.set(0);
        s.full.set(false);
        s.send_tx = None;
    }
}

fn decrement_buffered_amount(state: &SharedState, ws_id: u32, n: u64) {
    let Some(ws) = lookup_native_ws_state(state, ws_id) else {
        return;
    };
    let s = ws.borrow();
    let cur = s.buffered_amount.get();
    s.buffered_amount.set(cur.saturating_sub(n));
    if s.buffered_amount.get() < super::constants::MAX_BUFFERED_AMOUNT / 2 {
        s.full.set(false);
    }
}

// ---------------------------------------------------------------------------
// Send-side hook from `WebSocketImpl::send` / `close`
// ---------------------------------------------------------------------------

/// Move all queued frames from `impl_.send_queue` (V8-side) onto the
/// writer channel.
pub fn flush_v8_send_queue(state: &SharedState, ws_id: u32, impl_: &WebSocketImpl) {
    let Some(ws) = lookup_native_ws_state(state, ws_id) else {
        return;
    };

    let tx_opt = ws.borrow().send_tx.clone();
    let Some(tx) = tx_opt else {
        // Handshake hasn't finished yet — leave frames on the impl's
        // queue. The connect task drains it once it spawns the writer
        // (via the V8 dispatch arm calling flush again on next pump
        // turn after Open fires).
        return;
    };

    let frames: Vec<WsFrame> = impl_.send_queue.borrow_mut().drain(..).collect();
    for f in frames {
        if tx.unbounded_send(f).is_err() {
            // Channel closed — writer has exited; drop frames.
            break;
        }
    }
}

/// Cancel the connect / network task and emit Error+Close{1006}.
/// Called by `close()` during CONNECTING and by AbortSignal abort.
pub fn cancel_native_ws(state: &SharedState, ws_id: u32, reason: String) {
    let Some(ws) = lookup_native_ws_state(state, ws_id) else {
        return;
    };
    {
        let mut s = ws.borrow_mut();
        if s.cancel {
            return;
        }
        s.cancel = true;
        s.cancel_reason = reason;
    }
    let (rw, cw) = {
        let mut s = ws.borrow_mut();
        (
            s.recv_backpressure_waker.take(),
            s.connect_waker.take(),
        )
    };
    if let Some(w) = rw {
        w.wake();
    }
    if let Some(w) = cw {
        w.wake();
    }
    // For established sockets, drop the writer channel — the writer
    // task exits, the reader unblocks via TCP RST when its peer
    // disconnects.
    if let Some(ws) = lookup_native_ws_state(state, ws_id) {
        ws.borrow_mut().send_tx = None;
    }
}
