//! Socket ids, event queues, and lifecycle transitions for `node:net`.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::task::Waker;
use std::time::Instant;

use futures::channel::mpsc;

use crate::state::{OpResult, SharedState};
use crate::transport::byte_pump::{self, EventQueue, RecvBackpressure};

use super::caps::release_socket_slot;
use super::driver::WriteCmd;

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

impl RecvBackpressure for NativeSocketState {
    fn queued_event_len(&self) -> usize {
        self.events.len()
    }

    fn recv_backpressure_waker_mut(&mut self) -> &mut Option<Waker> {
        &mut self.recv_backpressure_waker
    }

    fn recv_paused(&self) -> bool {
        self.paused
    }

    fn recv_closed(&self) -> bool {
        self.destroyed
    }
}

impl EventQueue<SocketEvent> for NativeSocketState {
    fn events_mut(&mut self) -> &mut VecDeque<SocketEvent> {
        &mut self.events
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
    byte_pump::drain_events(lookup_native_socket_state(state, socket_id))
}

pub(super) fn push_event(state: &SharedState, socket_id: u32, event: SocketEvent) {
    let Some(socket) = lookup_native_socket_state(state, socket_id) else {
        return;
    };
    byte_pump::enqueue_event(&socket, event);
    byte_pump::schedule_event_op(state, OpResult::SocketEvent { socket_id });
}

pub(super) fn mark_socket_activity(state: &SharedState) {
    state.borrow_mut().native_socket_last_activity = Some(Instant::now());
}

pub(super) fn push_error_and_close(
    state: &SharedState,
    socket_id: u32,
    message: String,
    code: &str,
) {
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

pub(super) fn push_close_once(state: &SharedState, socket_id: u32, had_error: bool) {
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

#[cfg(feature = "runtime_native_websocket")]
pub(super) fn pending_data_events(state: &SharedState, socket_id: u32) -> bool {
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

pub fn destroy_all_sockets(state: &SharedState) -> usize {
    let ids: Vec<u32> = state.borrow().native_sockets.keys().copied().collect();
    let count = ids.len();
    for socket_id in ids {
        destroy_socket(state, socket_id);
    }
    count
}
