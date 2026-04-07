//! Non-blocking event loop — poll-based, waker-driven.
//!
//! The event loop never blocks. Each tick:
//! 1. Drain all ready events (try_recv, non-blocking)
//! 2. Fire expired timers
//! 3. Flush V8 microtasks
//! 4. If no work remains → done
//! 5. Register waker → return Poll::Pending (yield to executor)
//!
//! The blocking `run_event_loop` wrapper drives this via a manual `ParkWaker`
//! loop with `std::thread::park_timeout`, so existing callers don't change.

use std::cell::RefCell;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::rc::Rc;
use std::sync::mpsc;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use futures::task::AtomicWaker;

use crate::timers::TimerCallback;

// ---------------------------------------------------------------------------
// Heap-based timer types (used only by the old event loop / isolate path)
// ---------------------------------------------------------------------------

/// Entry in the timer min-heap. Ordered by (fire_at, id).
#[derive(Eq, PartialEq)]
pub(crate) struct TimerHeapEntry {
    pub(crate) fire_at: Instant,
    pub(crate) id: u32,
}

impl Ord for TimerHeapEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.fire_at
            .cmp(&other.fire_at)
            .then(self.id.cmp(&other.id))
    }
}

impl PartialOrd for TimerHeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// A timer min-heap + callback store.
#[allow(missing_debug_implementations)]
pub(crate) struct TimerState {
    /// Min-heap: next-to-fire on top (via Reverse for BinaryHeap).
    pub(crate) heap: BinaryHeap<Reverse<TimerHeapEntry>>,
    /// Callback storage, keyed by timer ID.
    pub(crate) callbacks: HashMap<u32, TimerCallback>,
    pub(crate) next_id: u32,
}

impl TimerState {
    pub(crate) fn new() -> Self {
        Self {
            heap: BinaryHeap::new(),
            callbacks: HashMap::new(),
            next_id: 1,
        }
    }
}

/// Fire all timers whose fire_at <= now. Returns true if any timer fired.
pub(crate) fn fire_ready_timers(scope: &mut v8::PinScope, state: &SharedState) -> bool {
    let mut any_fired = false;
    let now = Instant::now();

    loop {
        let should_fire = {
            let s = state.borrow();
            s.timers
                .heap
                .peek()
                .map(|Reverse(e)| e.fire_at <= now)
                .unwrap_or(false)
        };
        if !should_fire {
            break;
        }

        let entry = state.borrow_mut().timers.heap.pop().unwrap().0;

        // Take callback out (lazy deletion: cleared timers won't have an entry)
        let cb_opt = state.borrow_mut().timers.callbacks.remove(&entry.id);

        if let Some(cb) = cb_opt {
            any_fired = true;

            let func = v8::Local::new(scope, &cb.callback);
            let undefined = v8::undefined(scope).into();
            func.call(scope, undefined, &[]);
            scope.perform_microtask_checkpoint();

            if let Some(dur) = cb.interval {
                // setInterval: re-insert callback + new heap entry
                state.borrow_mut().timers.callbacks.insert(entry.id, cb);
                state.borrow_mut().timers.heap.push(Reverse(TimerHeapEntry {
                    fire_at: Instant::now() + dur,
                    id: entry.id,
                }));
            }
            // setTimeout: cb drops here, Global handle freed — no leak
        }
    }

    any_fired
}

// ---------------------------------------------------------------------------
// Event loop state
// ---------------------------------------------------------------------------

/// State shared between V8 callbacks and the event loop driver.
///
/// The event channel receiver (`event_rx`) is kept OUTSIDE this struct
/// so recv operations never hold a borrow on shared state.
#[allow(missing_debug_implementations)]
pub(crate) struct EventLoopInner {
    pub(crate) timers: TimerState,
    pub(crate) pending_resolvers: HashMap<u32, v8::Global<v8::PromiseResolver>>,
    pub(crate) event_tx: mpsc::Sender<LoopEvent>,
    pub(crate) next_op_id: u32,
    pub(crate) tokio_handle: Option<tokio::runtime::Handle>,
    pub(crate) log_buffer: Vec<String>,
    pub(crate) kv_store: HashMap<String, String>,
    pub(crate) env_vars: HashMap<String, String>,
    pub(crate) key_store: HashMap<u32, crate::crypto::KeyData>,
    pub(crate) next_key_id: u32,
    pub(crate) streams: HashMap<u32, StreamState>,
    pub(crate) next_stream_id: u32,
    /// Waker — background tasks (fetch, etc.) wake the event loop when results arrive.
    pub(crate) waker: Arc<AtomicWaker>,
}

/// Events flowing through the event channel.
///
/// Single unified channel for both per-request and concurrent models.
/// In concurrent mode, `NewRequest` and `Shutdown` are sent by the HTTP layer;
/// `OpCompleted` and `StreamChunk` are sent by background I/O tasks (fetch, etc.).
pub enum LoopEvent {
    OpCompleted { id: u32, value: String },
    StreamChunk { stream_id: u32, data: Vec<u8>, done: bool },
    /// A new RPC request from the HTTP layer (concurrent mode only).
    NewRequest {
        id: u64,
        body: String,
        reply: tokio::sync::oneshot::Sender<Result<crate::init::RequestResult, String>>,
    },
    /// Graceful shutdown signal (concurrent mode only).
    Shutdown,
}

/// State for a single ReadableStream instance.
pub(crate) struct StreamState {
    pub(crate) pending_read: Option<v8::Global<v8::PromiseResolver>>,
    pub(crate) buffer: Vec<Vec<u8>>,
    pub(crate) closed: bool,
}

impl EventLoopInner {
    pub(crate) fn new() -> (Self, mpsc::Receiver<LoopEvent>) {
        let (event_tx, event_rx) = mpsc::channel();
        let waker = Arc::new(AtomicWaker::new());
        (Self {
            timers: TimerState::new(),
            pending_resolvers: HashMap::new(),
            event_tx,
            next_op_id: 1,
            tokio_handle: None,
            log_buffer: Vec::new(),
            kv_store: HashMap::new(),
            env_vars: HashMap::new(),
            key_store: HashMap::new(),
            next_key_id: 1,
            streams: HashMap::new(),
            next_stream_id: 1,
            waker,
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
// Event loop helpers (each written ONCE)
// ---------------------------------------------------------------------------

/// Handle one I/O event (OpCompleted or StreamChunk).
///
/// `NewRequest` and `Shutdown` are NOT handled here — they are concurrent-mode
/// concerns handled by `ConcurrentIsolate`. This function only processes I/O
/// completion events that resolve promises or deliver stream data.
///
/// Returns `true` if the event was handled, `false` if it was a concurrent-mode
/// event that needs to be handled by the caller.
pub(crate) fn handle_one_event(scope: &mut v8::PinScope, state: &SharedState, event: LoopEvent) -> bool {
    match event {
        LoopEvent::OpCompleted { id, value } => {
            let resolver = state.borrow_mut().pending_resolvers.remove(&id);
            if let Some(resolver) = resolver {
                let r = v8::Local::new(scope, &resolver);
                let val = v8::String::new(scope, &value).unwrap();
                r.resolve(scope, val.into());
                scope.perform_microtask_checkpoint();
            }
            true
        }
        LoopEvent::StreamChunk { stream_id, data, done } => {
            crate::streams::push_stream_chunk(scope, state, stream_id, &data, done);
            scope.perform_microtask_checkpoint();
            true
        }
        // Concurrent-mode events — not handled by the per-request event loop.
        // Caller must handle these.
        LoopEvent::NewRequest { .. } | LoopEvent::Shutdown => false,
    }
}

/// Drain all ready I/O events (non-blocking).
///
/// Skips `NewRequest`/`Shutdown` events (which should never appear in per-request mode).
fn drain_events(scope: &mut v8::PinScope, state: &SharedState, event_rx: &mpsc::Receiver<LoopEvent>) {
    while let Ok(event) = event_rx.try_recv() {
        handle_one_event(scope, state, event);
    }
}

/// Check if there is any pending work.
pub(crate) fn has_pending_work(state: &SharedState) -> bool {
    let s = state.borrow();
    !s.timers.callbacks.is_empty()
        || !s.pending_resolvers.is_empty()
        || s.streams.values().any(|st| st.pending_read.is_some() && !st.closed)
}

/// Check if a promise has settled.
fn is_settled(scope: &mut v8::PinScope, promise: &v8::Global<v8::Promise>) -> bool {
    let local = v8::Local::new(scope, promise);
    local.state() != v8::PromiseState::Pending
}

/// Find the next valid timer fire time.
pub(crate) fn next_timer_fire(state: &SharedState) -> Option<Instant> {
    let s = state.borrow();
    for std::cmp::Reverse(entry) in s.timers.heap.iter() {
        if s.timers.callbacks.contains_key(&entry.id) {
            return Some(entry.fire_at);
        }
    }
    None
}

/// Compute wait timeout until next timer.
pub(crate) fn compute_wait_timeout(state: &SharedState) -> Option<Duration> {
    let has_pending_streams = state.borrow().streams.values().any(|st| st.pending_read.is_some());
    match next_timer_fire(state) {
        Some(fire_at) => Some(fire_at.saturating_duration_since(Instant::now())),
        None if !state.borrow().pending_resolvers.is_empty() || has_pending_streams => Some(Duration::from_secs(60)),
        None => None,
    }
}

// ---------------------------------------------------------------------------
// Non-blocking poll (the core — like Deno's poll_event_loop)
// ---------------------------------------------------------------------------

/// One non-blocking tick of the event loop.
///
/// Drains ready events, fires timers, flushes microtasks.
/// Returns `Poll::Ready(())` when done, `Poll::Pending` when waiting for more work.
/// Registers the waker so background tasks can re-trigger a poll.
pub(crate) fn poll_event_loop(
    scope: &mut v8::PinScope,
    state: &SharedState,
    event_rx: &mpsc::Receiver<LoopEvent>,
    cx: &mut Context<'_>,
    promise: Option<&v8::Global<v8::Promise>>,
) -> Poll<()> {
    // Register waker — background tasks (fetch, etc.) will wake us
    state.borrow().waker.register(cx.waker());

    // Check if promise already settled
    if let Some(p) = promise {
        if is_settled(scope, p) { return Poll::Ready(()); }
    }

    // Tick: microtasks → timers → drain events
    scope.perform_microtask_checkpoint();
    fire_ready_timers(scope, state);
    drain_events(scope, state, event_rx);

    // Re-check promise after tick
    if let Some(p) = promise {
        if is_settled(scope, p) { return Poll::Ready(()); }
    }

    // Check if any work remains
    if !has_pending_work(state) {
        scope.perform_microtask_checkpoint();
        return Poll::Ready(());
    }

    // Schedule timer wake — so we re-poll when next timer fires
    if let Some(fire_at) = next_timer_fire(state) {
        let delay = fire_at.saturating_duration_since(Instant::now());
        if delay.is_zero() {
            // Timer already ready — wake immediately for another tick
            cx.waker().wake_by_ref();
        } else {
            let waker = cx.waker().clone();
            // Use tokio timer to wake at the right time
            let handle = state.borrow().tokio_handle.clone();
            if let Some(handle) = handle {
                handle.spawn(async move {
                    tokio::time::sleep(delay).await;
                    waker.wake();
                });
            } else {
                // Fallback: spawn thread (for tests without tokio runtime)
                std::thread::spawn(move || {
                    std::thread::sleep(delay);
                    waker.wake();
                });
            }
        }
    }

    Poll::Pending
}

// ---------------------------------------------------------------------------
// Blocking wrapper (backward-compatible API for existing callers)
// ---------------------------------------------------------------------------

/// Drive the event loop to completion (blocking).
///
/// Internally drives `poll_event_loop` with a manual `ParkWaker`. The thread
/// parks via `std::thread::park_timeout` and is woken by:
/// 1. `AtomicWaker` — background tasks (fetch, etc.) call `waker.wake()`
///    which unparks this thread via `ParkWaker`.
/// 2. Timeout — `park_timeout` returns when the next timer should fire.
/// 3. Spurious wakes — harmless, just re-polls.
///
/// No tokio runtime needed. No `recv_timeout`.
///
/// - `promise: Some(p)` → stop when promise settles (or wall-time exceeded)
/// - `promise: None` → drive to exhaustion
pub(crate) fn run_event_loop(
    scope: &mut v8::PinScope,
    state: &SharedState,
    event_rx: &mpsc::Receiver<LoopEvent>,
    promise: Option<&v8::Global<v8::Promise>>,
    wall_timeout: Duration,
) {
    use std::sync::Arc;
    use std::task::{Context, Wake};

    // Thread-parker waker: background tasks unpark this thread
    // when they send events via the channel + wake the AtomicWaker.
    struct ParkWaker(std::thread::Thread);
    impl Wake for ParkWaker {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }

    let parker = Arc::new(ParkWaker(std::thread::current()));
    let waker = std::task::Waker::from(parker);
    let mut cx = Context::from_waker(&waker);
    let deadline = Instant::now() + wall_timeout;
    let tracking_promise = promise.is_some();

    loop {
        // Non-blocking poll: drain events, fire timers, flush microtasks
        match poll_event_loop(scope, state, event_rx, &mut cx, promise) {
            Poll::Ready(()) => return,
            Poll::Pending => {}
        }

        // Wall-time check
        if tracking_promise && Instant::now() > deadline {
            return;
        }

        // Compute how long to park
        let timeout = match compute_wait_timeout(state) {
            Some(d) => {
                if tracking_promise {
                    d.min(deadline.saturating_duration_since(Instant::now()))
                } else {
                    d
                }
            }
            None => return, // no more work possible
        };

        if timeout.is_zero() {
            continue; // timer ready, re-poll immediately
        }

        // Park thread — woken by AtomicWaker (ParkWaker::wake unparks)
        // or timeout (next timer fire).
        std::thread::park_timeout(timeout);
    }
}
