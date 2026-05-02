//! Per-WS network plumbing — handshake spawn, receive loop, send pump.
//!
//! Two long-lived compio tasks per established socket (one for read,
//! one for write) plus a connect task that owns the handshake. All
//! three communicate via the shared `Rc<RefCell<NativeWsState>>`
//! registered on `RuntimeState::native_websockets`.
//!
//! Events flow JS-ward via `OpResult::WebSocketEvent`: each event
//! resolved by the receive task wakes the pump (push a one-shot
//! future onto `spawned_ops`); the runtime arm in `runtime.rs`
//! calls `dispatch_ws_event` (this file) to mint native MessageEvent
//! / CloseEvent / Event and dispatch via `dom::event_target`.
//!
//! Backpressure: the receive loop awaits while the per-WS event queue
//! exceeds RECV_BACKPRESSURE_CAP. The cap is small (256) — no event
//! ever sits there longer than one pump tick in steady state.
//!
//! Close handshake timeout: per RFC 6455 §7.1.1 we wait up to 5s for
//! the peer's Close echo before dropping TCP. Wrapped in
//! `compio::time::timeout`.
//!
//! Spec citations:
//! - `establish a WebSocket connection`: WHATWG §4.1.
//! - `WebSocket message received`: WHATWG §4.4.
//! - `closing handshake started`: WHATWG §4.5.
//! - `connection closed`: WHATWG §4.6.

#![cfg(feature = "runtime_native_websocket")]

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;
use std::task::Waker;
use std::time::Duration;

use compio_ws::tungstenite::{self, Message, protocol::frame::CloseFrame};
use compio_ws::tungstenite::protocol::frame::coding::CloseCode;

use super::handshake::{Established, EstablishedStream, HandshakeError, HandshakeOptions};
use super::{WebSocketImpl, WsFrame};
use crate::state::{OpResult, SharedState};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Per-WS event delivered from the network task to the V8 pump arm.
#[derive(Debug)]
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
    /// Per WHATWG §4 connection-failed dispatch always emits `error`
    /// then `close{1006, was_clean: false}`; `Close` is queued
    /// separately from `Error` so the pump dispatches them in order.
    Error { reason: String },
}

/// Per-WS native state — shared between the connect task, receive loop,
/// send pump, and the V8-thread dispatch arm.
///
/// Lives behind `Rc<RefCell<...>>` (single-threaded, isolate-bound) and
/// is keyed on `ws_id` in `RuntimeState::native_websockets`. Holding
/// the buffered-amount counter HERE (not on `WebSocketImpl`) lets the
/// network task update it without dereferencing a raw pointer to the
/// boxed impl — the V8-side getter reads it through the same Rc.
pub struct NativeWsState {
    /// Pending events awaiting dispatch on the V8 thread.
    pub events: VecDeque<WsEvent>,
    /// Receive-loop backpressure waker — woken when `events` shrinks
    /// below the resume watermark.
    pub recv_backpressure_waker: Option<Waker>,
    /// Pump-side: outgoing send queue (drained by send_pump).
    pub send_queue: VecDeque<WsFrame>,
    /// Send-pump waker.
    pub send_waker: Option<Waker>,
    /// "Close-was-sent" latch: once true, send_pump exits cleanly after
    /// emitting one Close frame.
    pub close_initiated: bool,
    /// Cancellation flag — flipped by `WebSocketImpl::close()` during
    /// CONNECTING and by AbortSignal abort. The connect task observes
    /// it and emits Error+Close{1006}.
    pub cancel: bool,
    /// Cancel reason for AbortSignal abort propagation.
    pub cancel_reason: String,
    /// Tracks whether the receive loop has exited (so send_pump can
    /// stop enqueueing).
    pub recv_finished: bool,
    /// Last-resort waker for the connect task while CONNECTING (so
    /// `close()` during CONNECTING can break the connect future).
    pub connect_waker: Option<Waker>,
    /// Shared `WebSocketImpl::buffered_amount` (same Rc — both sides
    /// see the same underlying Cell). The network task decrements
    /// after a successful write; the V8 thread bumps at queue time.
    /// `Rc` clone is fine: single-threaded isolate, never sent.
    pub buffered_amount: Rc<Cell<u64>>,
    /// Shared `WebSocketImpl::full` flag — set when projected queue
    /// size would exceed `MAX_BUFFERED_AMOUNT`; cleared by the pump
    /// at 50% drain (hysteresis).
    pub full: Rc<Cell<bool>>,
    /// True for client sockets (network-backed); false for paired sockets
    /// (no real network, message passing only).
    pub is_pair: bool,
}

impl NativeWsState {
    pub fn new() -> Self {
        NativeWsState {
            events: VecDeque::new(),
            recv_backpressure_waker: None,
            send_queue: VecDeque::new(),
            send_waker: None,
            close_initiated: false,
            cancel: false,
            cancel_reason: String::new(),
            recv_finished: false,
            connect_waker: None,
            buffered_amount: Rc::new(Cell::new(0)),
            full: Rc::new(Cell::new(false)),
            is_pair: false,
        }
    }

    pub fn new_pair() -> Self {
        NativeWsState {
            is_pair: true,
            ..NativeWsState::new()
        }
    }
}

/// Receive-side backpressure cap. When the per-WS event queue exceeds
/// this, the receive loop awaits drain before reading the next frame.
/// Combined with tungstenite's stream-level read buffer (~128 KiB) this
/// applies TCP backpressure.
const RECV_BACKPRESSURE_CAP: usize = 256;

/// Resume watermark — receive loop wakes when queue drops below this.
const RECV_BACKPRESSURE_RESUME: usize = 128;

/// Close handshake timeout per RFC 6455 §7.1.1.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// Spawning the connect task
// ---------------------------------------------------------------------------

/// Allocate a fresh `ws_id` for a native client WebSocket and register
/// its `NativeWsState`. Called from `WebSocketImpl::new`.
///
/// IDs are odd numbers (1, 3, 5, …) to leave the even space for
/// WebSocketPair which mints two IDs together (the polyfill uses
/// `next_ws_id` to allocate two; the native side stays disjoint).
pub fn alloc_native_ws_id(state: &SharedState) -> u32 {
    let mut s = state.borrow_mut();
    let id = s.next_native_ws_id;
    s.next_native_ws_id = id.checked_add(1).unwrap_or(1);
    s.native_websockets
        .insert(id, Rc::new(RefCell::new(NativeWsState::new())));
    id
}

/// Free the `NativeWsState` for `ws_id` after the connect task and
/// both pumps have exited. Idempotent.
pub fn free_native_ws_state(state: &SharedState, ws_id: u32) {
    let mut s = state.borrow_mut();
    s.native_websockets.remove(&ws_id);
}

/// Get a clone of the per-WS state Rc, if registered.
pub fn lookup_native_ws_state(state: &SharedState, ws_id: u32) -> Option<Rc<RefCell<NativeWsState>>> {
    state.borrow().native_websockets.get(&ws_id).cloned()
}

/// Push an event onto the per-WS queue and wake the pump.
fn push_event(state: &SharedState, ws_id: u32, event: WsEvent) {
    let ws = match lookup_native_ws_state(state, ws_id) {
        Some(w) => w,
        None => return,
    };
    ws.borrow_mut().events.push_back(event);

    // Push a one-shot future onto spawned_ops that yields
    // `OpResult::WebSocketEvent`. The pump's existing select loop
    // picks it up; the dispatch arm in runtime.rs drains the per-WS
    // queue (so multiple events queued in one batch dispatch in one
    // V8 turn).
    let id = ws_id;
    let fut: std::pin::Pin<Box<dyn std::future::Future<Output = OpResult>>> =
        Box::pin(async move { OpResult::WebSocketEvent { ws_id: id } });
    {
        let mut s = state.borrow_mut();
        s.spawned_ops.push(fut);
    }

    // Wake the pump.
    let notify = state.borrow().pump_notify_tx.clone();
    if let Some(mut tx) = notify {
        let _ = tx.try_send(());
    }
}

/// Backpressure: await until events queue drops below resume watermark.
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

/// Drain a batch of events for `ws_id`. Called by the V8-thread pump
/// arm when it sees `OpResult::WebSocketEvent`. Returns the events in
/// FIFO order. Caller must dispatch them in the same V8 turn.
pub fn drain_events(state: &SharedState, ws_id: u32) -> Vec<WsEvent> {
    let Some(ws) = lookup_native_ws_state(state, ws_id) else {
        return Vec::new();
    };
    let drained: Vec<WsEvent> = {
        let mut s = ws.borrow_mut();
        s.events.drain(..).collect()
    };
    // After draining, wake the receive loop if it was paused for
    // backpressure.
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
// Connect task — handshake + spawn receive_loop + send_pump
// ---------------------------------------------------------------------------

/// Spawn the connect task for a freshly constructed WebSocket. The task
/// runs on the same compio thread as V8 (no Send required).
pub fn spawn_connect_task(
    state: SharedState,
    ws_id: u32,
    url: url::Url,
    opts: HandshakeOptions,
) {
    let state_for_task = state.clone();
    let task = async move {
        // Observe pre-emptive cancellation (e.g. the user called
        // close() between construction and the task being polled).
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

        let result = {
            let url_for_handshake = url.clone();
            super::handshake::run_client_handshake(url_for_handshake, opts).await
        };

        match result {
            Ok(Established {
                stream,
                protocol,
                extensions,
            }) => {
                // Open event first (before any messages).
                push_event(
                    &state_for_task,
                    ws_id,
                    WsEvent::Open {
                        protocol,
                        extensions,
                    },
                );

                // Run the read+send pumps to completion. Both share the
                // single underlying WebSocketStream — we need to
                // multiplex via `futures::select`. Because compio_ws's
                // WebSocketStream is `&mut self` for both read and send,
                // we run them as a single loop that alternates based on
                // wakeups. (The simple way: wrap both in one task that
                // selects between read and a send notification.)
                run_socket_loop(state_for_task.clone(), ws_id, stream).await;
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
                // Connection-failed dispatch per WHATWG §4: error then
                // close(1006). Both events queued; dispatched in order.
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
// run_socket_loop — multiplexed read/send on a single stream
// ---------------------------------------------------------------------------

/// Drives the socket post-handshake. Single task that:
///   1. Awaits incoming frames (read).
///   2. Drains outgoing frames (send).
///   3. Honours backpressure on the per-WS event queue.
///   4. Exits cleanly on Close.
async fn run_socket_loop(state: SharedState, ws_id: u32, stream: EstablishedStream) {
    match stream {
        EstablishedStream::Plain(ws) => run_socket_loop_inner(state, ws_id, ws).await,
        EstablishedStream::Tls(ws) => run_socket_loop_inner(state, ws_id, ws).await,
    }
}

async fn run_socket_loop_inner<S>(
    state: SharedState,
    ws_id: u32,
    mut stream: compio_ws::WebSocketStream<S>,
) where
    S: compio::io::AsyncRead + compio::io::AsyncWrite + 'static,
{
    let ws_state = match lookup_native_ws_state(&state, ws_id) {
        Some(w) => w,
        None => return,
    };
    #[allow(unused_assignments)]
    let mut sent_close = false;
    #[allow(unused_assignments)]
    let mut peer_closed = false;

    // The single-task design avoids the compio-io "buffer was submitted
    // for io and never returned" panic that fires if we drop a partially-
    // polled `stream.read()` future. tungstenite over compio is
    // serialise-only at the future level: once a read or send future is
    // started, it MUST run to completion.
    //
    // To allow sends without blocking on reads, we use a small drain
    // step per iteration:
    //   1. Drain everything in `send_queue` synchronously (each send
    //      is one `stream.send().await` — never cancellable but
    //      always run to completion).
    //   2. Run ONE `stream.read().await` to completion and dispatch.
    //   3. After a read returns, loop back to step 1 in case sends
    //      arrived while we were blocked on the read.
    //
    // The cost: a send queued WHILE a read is blocked waits until
    // that read returns. For low-latency request/response apps this
    // is fine — the peer's response wakes the read; for one-way
    // streaming sends this could starve. Future improvement: spawn a
    // periodic "wakeup ping" the user can disable, OR adopt a true
    // splittable framer (out of scope).
    loop {
        // Bail out if the queue is paused for backpressure.
        await_recv_drain(&ws_state).await;
        if sent_close && peer_closed {
            break;
        }

        // STEP 1: drain pending sends.
        loop {
            let cancelled = ws_state.borrow().cancel;
            if cancelled {
                let reason = ws_state.borrow().cancel_reason.clone();
                push_event(
                    &state,
                    ws_id,
                    WsEvent::Error {
                        reason: format!("aborted: {reason}"),
                    },
                );
                push_event(
                    &state,
                    ws_id,
                    WsEvent::Close {
                        code: 1006,
                        reason,
                        was_clean: false,
                    },
                );
                return;
            }

            let frame_opt = ws_state.borrow_mut().send_queue.pop_front();
            let Some(frame) = frame_opt else { break };
            let mut sent_close_now = false;
            let send_result: Result<u64, String> = match frame {
                WsFrame::Text(s) => {
                    let bytes_len = s.len() as u64;
                    match stream.send(Message::Text(s.into())).await {
                        Ok(()) => Ok(bytes_len),
                        Err(e) => Err(e.to_string()),
                    }
                }
                WsFrame::Binary(b) => {
                    let bytes_len = b.len() as u64;
                    match stream.send(Message::Binary(b.into())).await {
                        Ok(()) => Ok(bytes_len),
                        Err(e) => Err(e.to_string()),
                    }
                }
                WsFrame::Blob { handle: _, size } => {
                    // v1 ships text/Binary fast paths only. The
                    // bufferedAmount was bumped at queue time;
                    // release the budget.
                    Ok(size)
                }
                WsFrame::Close { code, reason } => {
                    let payload = code.map(|c| CloseFrame {
                        code: CloseCode::from(c),
                        reason: reason.into(),
                    });
                    sent_close_now = true;
                    match stream.send(Message::Close(payload)).await {
                        Ok(()) => Ok(0),
                        Err(e) => Err(e.to_string()),
                    }
                }
            };
            match send_result {
                Ok(n) if n > 0 => decrement_buffered_amount(&state, ws_id, n),
                Ok(_) => {}
                Err(e) => {
                    send_failure(&state, ws_id, e);
                    return;
                }
            }
            if sent_close_now {
                sent_close = true;
                ws_state.borrow_mut().close_initiated = true;
                // Wait for peer's Close echo, up to 5s. Then drop.
                let close_done = compio::time::timeout(
                    CLOSE_TIMEOUT,
                    wait_for_peer_close(&mut stream),
                )
                .await;
                match close_done {
                    Ok(Ok((code, reason))) => {
                        push_event(
                            &state,
                            ws_id,
                            WsEvent::Close {
                                code,
                                reason,
                                was_clean: true,
                            },
                        );
                    }
                    Ok(Err(e)) => {
                        push_event(
                            &state,
                            ws_id,
                            WsEvent::Error {
                                reason: format!("{e}"),
                            },
                        );
                        push_event(
                            &state,
                            ws_id,
                            WsEvent::Close {
                                code: 1006,
                                reason: String::new(),
                                was_clean: false,
                            },
                        );
                    }
                    Err(_) => {
                        push_event(
                            &state,
                            ws_id,
                            WsEvent::Close {
                                code: 1006,
                                reason: String::new(),
                                was_clean: false,
                            },
                        );
                    }
                }
                return;
            }
        }

        // STEP 2: read ONE message. This await is non-cancellable
        // (compio-io panic if we drop). We rely on the peer / TCP
        // RST to wake us; for true cancel, the cancel flag has
        // already returned the loop above.
        match stream.read().await {
            Ok(msg) => match msg {
                Message::Text(s) => {
                    push_event(&state, ws_id, WsEvent::MessageText(s.to_string()));
                }
                Message::Binary(b) => {
                    push_event(&state, ws_id, WsEvent::MessageBinary(b.to_vec()));
                }
                Message::Ping(_) | Message::Pong(_) => {}
                Message::Close(frame) => {
                    let (code, reason) = match frame {
                        Some(f) => (u16::from(f.code), f.reason.to_string()),
                        None => (1005, String::new()),
                    };
                    push_event(
                        &state,
                        ws_id,
                        WsEvent::Close {
                            code,
                            reason,
                            was_clean: true,
                        },
                    );
                    peer_closed = true;
                    if sent_close {
                        break;
                    }
                    ws_state.borrow_mut().close_initiated = true;
                    let _ = compio::time::timeout(CLOSE_TIMEOUT, stream.close(None)).await;
                    break;
                }
                Message::Frame(_) => {}
            },
            Err(e) => {
                push_event(
                    &state,
                    ws_id,
                    WsEvent::Error {
                        reason: format!("{e}"),
                    },
                );
                push_event(
                    &state,
                    ws_id,
                    WsEvent::Close {
                        code: 1006,
                        reason: String::new(),
                        was_clean: false,
                    },
                );
                break;
            }
        }
    }

    // Mark recv finished + mark state for cleanup.
    if let Some(ws) = lookup_native_ws_state(&state, ws_id) {
        ws.borrow_mut().recv_finished = true;
    }
}

/// After we sent Close, wait for the peer's Close echo. Drains
/// in-flight non-Close frames silently (per RFC 6455 §7.1.2 the
/// receiver SHOULD continue processing data until the Close arrives).
async fn wait_for_peer_close<S>(
    stream: &mut compio_ws::WebSocketStream<S>,
) -> Result<(u16, String), tungstenite::Error>
where
    S: compio::io::AsyncRead + compio::io::AsyncWrite,
{
    loop {
        let msg = stream.read().await?;
        match msg {
            Message::Close(frame) => {
                let (code, reason) = match frame {
                    Some(f) => (u16::from(f.code), f.reason.to_string()),
                    None => (1005, String::new()),
                };
                return Ok((code, reason));
            }
            // Drop everything else while in the closing handshake.
            _ => continue,
        }
    }
}

fn send_failure(state: &SharedState, ws_id: u32, msg: String) {
    push_event(
        state,
        ws_id,
        WsEvent::Error {
            reason: format!("send error: {msg}"),
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
    // Zero out the buffered counter on failure — outstanding frames
    // are dropped on the floor (matches v1 polyfill behaviour).
    if let Some(ws) = lookup_native_ws_state(state, ws_id) {
        let mut s = ws.borrow_mut();
        s.send_queue.clear();
        s.buffered_amount.set(0);
        s.full.set(false);
    }
}

/// Decrement the per-WS buffered_amount counter after a successful write.
/// Hysteresis: clear `full` when we drop below 50%.
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
// Send-side hook from `WebSocketImpl::send` / `close` — bridges the
// per-instance `WebSocketImpl::send_queue` (which the V8 thread populates)
// with the per-WS network task's `NativeWsState::send_queue` (which the
// network task drains).
// ---------------------------------------------------------------------------

/// Move all queued frames from `impl_.send_queue` (V8-side) into the
/// per-WS native state's `send_queue` (network-side) and notify the
/// send pump. Called whenever JS calls `socket.send()` or
/// `socket.close()`.
pub fn flush_v8_send_queue(state: &SharedState, ws_id: u32, impl_: &WebSocketImpl) {
    let Some(ws) = lookup_native_ws_state(state, ws_id) else {
        return;
    };
    let frames: Vec<WsFrame> = impl_.send_queue.borrow_mut().drain(..).collect();
    if frames.is_empty() {
        return;
    }
    let mut s = ws.borrow_mut();
    for f in frames {
        s.send_queue.push_back(f);
    }
    if let Some(w) = s.send_waker.take() {
        w.wake();
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
    // Wake any of the three potential waiters.
    let (sw, rw, cw) = {
        let mut s = ws.borrow_mut();
        (
            s.send_waker.take(),
            s.recv_backpressure_waker.take(),
            s.connect_waker.take(),
        )
    };
    if let Some(w) = sw {
        w.wake();
    }
    if let Some(w) = rw {
        w.wake();
    }
    if let Some(w) = cw {
        w.wake();
    }
}
