//! Event loop state and unified driver.
//!
//! Single `run_event_loop` function handles both modes:
//! - `promise: None` — drive to exhaustion (fire-and-forget side effects)
//! - `promise: Some(p)` — drive until the promise settles (or wall-time exceeded)
//!
//! Uses the min-heap timer system and async op channel.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

use crate::timers::{fire_ready_timers, TimerState};

// ---------------------------------------------------------------------------
// Event loop state
// ---------------------------------------------------------------------------

/// State shared between V8 callbacks and the event loop driver.
///
/// The event channel receiver (`event_rx`) is intentionally kept OUTSIDE this
/// struct (and therefore outside the `Rc<RefCell<>>`) so that blocking
/// `recv_timeout()` calls never hold a mutable borrow on shared state.
/// Whoever drives the event loop owns `event_rx` separately.
#[allow(missing_debug_implementations)]
pub(crate) struct EventLoopInner {
    pub(crate) timers: TimerState,
    /// Pending promise resolvers for async ops (fetch, DB, etc.)
    pub(crate) pending_resolvers: HashMap<u32, v8::Global<v8::PromiseResolver>>,
    /// Event channel sender (cloned into background tasks for async ops and streaming)
    pub(crate) event_tx: mpsc::Sender<LoopEvent>,
    pub(crate) next_op_id: u32,
    /// Tokio runtime handle for spawning async ops (fetch, etc.)
    pub(crate) tokio_handle: Option<tokio::runtime::Handle>,
    /// Optional sender for ConcurrentIsolate event channel.
    /// When set, async ops send Event::OpCompleted here instead of event_tx.
    pub(crate) concurrent_event_tx: Option<mpsc::Sender<crate::concurrent::Event>>,
    /// Per-isolate log buffer. Console output is appended here.
    pub(crate) log_buffer: Vec<String>,
    /// Per-isolate key-value store (persistent across requests, lost on evict)
    pub(crate) kv_store: HashMap<String, String>,
    /// Per-app environment variables (injected at isolate creation).
    pub(crate) env_vars: HashMap<String, String>,
    /// Crypto key store — key material stays in Rust, JS holds opaque u32 handles.
    pub(crate) key_store: HashMap<u32, crate::crypto::KeyData>,
    pub(crate) next_key_id: u32,
    /// Active readable streams: stream_id -> StreamState.
    pub(crate) streams: HashMap<u32, StreamState>,
    pub(crate) next_stream_id: u32,
}

/// Events flowing through the per-request event loop channel.
///
/// `OpCompleted` carries the result of an async op (fetch headers, DB query, etc.).
/// `StreamChunk` carries a body chunk from a background I/O task (streaming fetch).
pub(crate) enum LoopEvent {
    /// Async op completed — resolve the pending promise.
    OpCompleted { id: u32, value: String },
    /// Stream chunk arrived from background I/O (e.g., streaming fetch body).
    StreamChunk { stream_id: u32, data: Vec<u8>, done: bool },
}

// ---------------------------------------------------------------------------
// Stream state (for ReadableStream backing)
// ---------------------------------------------------------------------------

/// State for a single ReadableStream instance.
pub(crate) struct StreamState {
    /// Pending read promise resolver (JS is waiting for next chunk).
    pub(crate) pending_read: Option<v8::Global<v8::PromiseResolver>>,
    /// Buffered chunks waiting to be read.
    pub(crate) buffer: Vec<Vec<u8>>,
    /// Whether the stream has been closed.
    pub(crate) closed: bool,
}

impl EventLoopInner {
    pub(crate) fn new() -> (Self, mpsc::Receiver<LoopEvent>) {
        let (event_tx, event_rx) = mpsc::channel();
        (Self {
            timers: TimerState::new(),
            pending_resolvers: HashMap::new(),
            event_tx,
            next_op_id: 1,
            tokio_handle: None,
            concurrent_event_tx: None,
            log_buffer: Vec::new(),
            kv_store: HashMap::new(),
            env_vars: HashMap::new(),
            key_store: HashMap::new(),
            next_key_id: 1,
            streams: HashMap::new(),
            next_stream_id: 1,
        }, event_rx)
    }

    pub(crate) fn with_env(env_vars: HashMap<String, String>) -> (Self, mpsc::Receiver<LoopEvent>) {
        let (mut state, rx) = Self::new();
        state.env_vars = env_vars;
        (state, rx)
    }
}

pub(crate) type SharedState = Rc<RefCell<EventLoopInner>>;

// ---------------------------------------------------------------------------
// Event loop helpers
// ---------------------------------------------------------------------------

/// Handle one event — written ONCE, used by drain + wait phases.
fn handle_one_event(scope: &mut v8::PinScope, state: &SharedState, event: LoopEvent) {
    match event {
        LoopEvent::OpCompleted { id, value } => {
            let resolver = state.borrow_mut().pending_resolvers.remove(&id);
            if let Some(resolver) = resolver {
                let r = v8::Local::new(scope, &resolver);
                let val = v8::String::new(scope, &value).unwrap();
                r.resolve(scope, val.into());
                scope.perform_microtask_checkpoint();
            }
        }
        LoopEvent::StreamChunk { stream_id, data, done } => {
            crate::streams::push_stream_chunk(scope, state, stream_id, &data, done);
            scope.perform_microtask_checkpoint();
        }
    }
}

/// Drain completed events from the channel: resolve async ops and push stream chunks.
///
/// `event_rx` is outside the `RefCell` — no borrow on shared state needed for recv.
fn drain_events(scope: &mut v8::PinScope, state: &SharedState, event_rx: &mpsc::Receiver<LoopEvent>) {
    while let Ok(event) = event_rx.try_recv() {
        handle_one_event(scope, state, event);
    }
}

/// Check if there is any pending work (timers, async ops, or open streams).
pub(crate) fn has_pending_work(state: &SharedState) -> bool {
    let s = state.borrow();
    !s.timers.callbacks.is_empty()
        || !s.pending_resolvers.is_empty()
        || s.streams.values().any(|st| st.pending_read.is_some() && !st.closed)
}

/// Check if a promise has settled (fulfilled or rejected).
fn is_settled(scope: &mut v8::PinScope, promise: &v8::Global<v8::Promise>) -> bool {
    let local = v8::Local::new(scope, promise);
    local.state() != v8::PromiseState::Pending
}

/// Find the next valid timer fire time (skipping cleared timers via lazy deletion).
/// Returns `None` if no valid timers exist.
pub(crate) fn next_timer_fire(state: &SharedState) -> Option<std::time::Instant> {
    let s = state.borrow();
    for std::cmp::Reverse(entry) in s.timers.heap.iter() {
        if s.timers.callbacks.contains_key(&entry.id) {
            return Some(entry.fire_at);
        }
    }
    None
}

/// Compute the wait duration until the next valid timer fires.
pub(crate) fn compute_wait_timeout(state: &SharedState) -> Option<Duration> {
    let has_pending_streams = state.borrow().streams.values().any(|st| st.pending_read.is_some());

    match next_timer_fire(state) {
        Some(fire_at) => Some(fire_at.saturating_duration_since(std::time::Instant::now())),
        None if !state.borrow().pending_resolvers.is_empty() || has_pending_streams => Some(Duration::from_secs(60)),
        None => None,
    }
}

// ---------------------------------------------------------------------------
// Event loop driver
// ---------------------------------------------------------------------------

/// Drive the event loop.
///
/// - If `promise` is `Some`, stop when the promise settles (or wall-time exceeded).
/// - If `promise` is `None`, drive to exhaustion (fire-and-forget side effects)
///   with a generous 60s wall-time cap.
pub(crate) fn run_event_loop(
    scope: &mut v8::PinScope,
    state: &SharedState,
    event_rx: &mpsc::Receiver<LoopEvent>,
    promise: Option<&v8::Global<v8::Promise>>,
    wall_timeout: Duration,
) {
    let deadline = std::time::Instant::now() + wall_timeout;

    loop {
        // Check promise settled
        if let Some(p) = promise {
            if is_settled(scope, p) { return; }
        }

        // Check wall-time
        if std::time::Instant::now() > deadline { return; }

        // Tick: microtasks, timers, drain events
        scope.perform_microtask_checkpoint();
        if let Some(p) = promise {
            if is_settled(scope, p) { return; }
        }

        fire_ready_timers(scope, state);
        if let Some(p) = promise {
            if is_settled(scope, p) { return; }
        }

        drain_events(scope, state, event_rx);
        if let Some(p) = promise {
            if is_settled(scope, p) { return; }
        }

        // Check if done (no work left)
        if !has_pending_work(state) {
            scope.perform_microtask_checkpoint();
            return;
        }

        // Compute wait timeout
        let timeout = match compute_wait_timeout(state) {
            Some(d) => d,
            None => {
                scope.perform_microtask_checkpoint();
                return;
            }
        };

        if timeout.is_zero() {
            continue;
        }

        // Cap wait at remaining wall-time
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        let timeout = timeout.min(remaining);

        // Wait for next event or timer
        if has_pending_work(state) {
            if let Ok(event) = event_rx.recv_timeout(timeout) {
                handle_one_event(scope, state, event);
            }
        } else {
            std::thread::sleep(timeout);
        }
    }
}
