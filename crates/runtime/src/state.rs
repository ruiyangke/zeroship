//! Shared runtime state — types for the event loop.
//!
//! `RuntimeState` holds all V8 callback state, behind an `Rc<RefCell<>>` so
//! callbacks can borrow it without crossing thread boundaries.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::Waker;
use std::time::Duration;

// ---------------------------------------------------------------------------
// TimerCallback (absorbed from v8/timers.rs)
// ---------------------------------------------------------------------------

/// Backing storage for a timer's callback.
#[allow(missing_debug_implementations)]
pub struct TimerCallback {
    pub callback: v8::Global<v8::Function>,
    /// None = setTimeout (one-shot), Some(dur) = setInterval (repeating).
    pub interval: Option<Duration>,
}

// ---------------------------------------------------------------------------
// OpError (absorbed from v8/ops.rs)
// ---------------------------------------------------------------------------

/// Error kind — maps to JS exception types.
#[derive(Debug, Clone, Copy)]
pub enum OpErrorKind {
    /// `TypeError` — wrong argument types, missing arguments
    TypeError,
    /// `RangeError` — value out of bounds
    RangeError,
    /// Generic `Error`
    Error,
}

/// An error from a V8 op.
#[derive(Debug, Clone)]
pub struct OpError {
    pub kind: OpErrorKind,
    pub message: String,
}

impl OpError {
    pub fn type_error(msg: impl Into<String>) -> Self {
        Self {
            kind: OpErrorKind::TypeError,
            message: msg.into(),
        }
    }

    pub fn range_error(msg: impl Into<String>) -> Self {
        Self {
            kind: OpErrorKind::RangeError,
            message: msg.into(),
        }
    }

    pub fn error(msg: impl Into<String>) -> Self {
        Self {
            kind: OpErrorKind::Error,
            message: msg.into(),
        }
    }
}

impl std::fmt::Display for OpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.message)
    }
}

impl std::error::Error for OpError {}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Capacity of the channel that delivers requests to the runtime.
pub const REQUEST_CHANNEL_CAPACITY: usize = 256;

/// Maximum number of concurrent in-flight async ops (fetch, kv, ...).
pub const MAX_PENDING_OPS: usize = 1024;

// ---------------------------------------------------------------------------
// WebSocket state
// ---------------------------------------------------------------------------

/// A message on a WebSocket channel.
#[derive(Debug, Clone)]
pub enum WsMessage {
    /// UTF-8 text frame.
    Text(String),
    /// Binary frame.
    Binary(Vec<u8>),
    /// Close frame with code + reason.
    Close(u16, String),
}

/// Cached V8 handles for fast WebSocket dispatch (avoids 3 property lookups per message).
pub struct WsCachedHandles {
    /// The JS WebSocket object itself.
    pub ws_obj: v8::Global<v8::Object>,
    /// Cached `ws._onMessage` function.
    pub on_message: v8::Global<v8::Function>,
    /// Cached `ws._onClose` function.
    pub on_close: v8::Global<v8::Function>,
}

impl std::fmt::Debug for WsCachedHandles {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WsCachedHandles").finish()
    }
}

/// Per-WebSocket state tracked in RuntimeState.
#[derive(Debug)]
pub struct WebSocketState {
    /// The other end of a WebSocketPair (None for standalone WebSockets).
    pub peer_id: Option<u32>,
    /// Whether `accept()` has been called (server-side).
    pub accepted: bool,
    /// Whether the WebSocket has been closed.
    pub closed: bool,
    /// Messages arriving TO this WebSocket (from peer or TCP).
    pub incoming: VecDeque<WsMessage>,
    /// Messages FROM this WebSocket (to peer or TCP).
    pub outgoing: VecDeque<WsMessage>,
    /// Close code (set when close is initiated).
    pub close_code: Option<u16>,
    /// Close reason (set when close is initiated).
    pub close_reason: Option<String>,
    /// Notification flag: set to true when outgoing messages are queued.
    pub outgoing_ready: Rc<Cell<bool>>,
    /// Waker for the bidirectional pump (woken when outgoing_ready is set).
    pub pump_waker: Rc<RefCell<Option<Waker>>>,
    /// Cached V8 handles — resolved once at accept, used for every message.
    pub cached_handles: Option<WsCachedHandles>,
}

impl WebSocketState {
    /// Create a new WebSocket state in the CONNECTING state.
    pub fn new() -> Self {
        Self {
            peer_id: None,
            accepted: false,
            closed: false,
            incoming: VecDeque::new(),
            outgoing: VecDeque::new(),
            close_code: None,
            close_reason: None,
            outgoing_ready: Rc::new(Cell::new(false)),
            pump_waker: Rc::new(RefCell::new(None)),
            cached_handles: None,
        }
    }

    /// Signal that outgoing data is available — wake the pump if sleeping.
    pub fn notify_outgoing(&self) {
        self.outgoing_ready.set(true);
        if let Some(waker) = self.pump_waker.borrow_mut().take() {
            waker.wake();
        }
    }
}

// ---------------------------------------------------------------------------
// Core state
// ---------------------------------------------------------------------------

/// State shared between V8 callbacks and the event loop driver.
///
/// Wrapped in `Rc<RefCell<>>` so it can be cloned cheaply into closures.
/// Never send across threads — kept on the isolate thread.
#[allow(missing_debug_implementations)]
pub struct RuntimeState {
    /// Pending Promise resolvers keyed by op-id.
    pub pending_resolvers: HashMap<u32, v8::Global<v8::PromiseResolver>>,
    /// Monotonically increasing op-id counter.
    pub next_op_id: u32,

    /// Callbacks for live timers (setTimeout / setInterval), keyed by timer-id.
    pub timer_callbacks: HashMap<u32, TimerCallback>,
    /// Monotonically increasing timer-id counter.
    pub next_timer_id: u32,
    /// Maps timer-id -> request-id that owns it (for per-request cleanup).
    pub timer_owner: HashMap<u32, u64>,

    /// Active ReadableStream instances, keyed by stream-id.
    pub streams: HashMap<u32, StreamState>,
    /// Monotonically increasing stream-id counter.
    pub next_stream_id: u32,

    /// Futures for in-flight async ops (fetch, kv, ...).
    pub spawned_ops: Vec<Pin<Box<dyn Future<Output = OpResult>>>>,
    /// Timers queued to be armed on the next event-loop iteration.
    pub spawned_timers: Vec<SpawnedTimer>,
    /// Timer IDs ready to fire immediately (delay == 0).
    /// Drained by the Runtime after each enter_v8, avoiding sleep overhead.
    pub ready_timers: Vec<u32>,

    /// Fetch requests queued by V8 callbacks, drained by the runtime executor.
    pub spawned_fetches: Vec<FetchRequest>,

    /// The request currently being executed (None between requests).
    pub executing_request_id: Option<u64>,
    /// Cancellation flag for the current request (unused in compio path, kept for API compat).
    pub executing_request_cancel: Option<()>,

    /// Per-request log lines accumulated during execution.
    pub per_request_logs: HashMap<u64, Vec<String>>,

    /// In-memory KV store.
    pub kv_store: HashMap<String, String>,
    /// Process / runtime environment variables surfaced to JS.
    pub env_vars: HashMap<String, String>,

    /// WebCrypto key store, keyed by key-id.
    pub key_store: HashMap<u32, crate::crypto::KeyData>,
    /// Monotonically increasing key-id counter.
    pub next_key_id: u32,

    /// Stream IDs that have outbound StreamForwarders (HTTP streaming responses).
    /// When a stream_id is in this set, `stream_enqueue_callback` forwards chunks
    /// via the stream events channel instead of buffering them for JS reads.
    pub outbound_streams: HashSet<u32>,

    /// WebSocket instances, keyed by ws_id.
    pub websockets: HashMap<u32, WebSocketState>,
    /// Monotonically increasing WebSocket ID counter (incremented by 2 for pairs).
    pub next_ws_id: u32,
}

/// Convenience alias — the shared handle passed into V8 callbacks.
pub type SharedState = Rc<RefCell<RuntimeState>>;

impl RuntimeState {
    /// Create a new `RuntimeState` seeded with the given environment variables.
    pub fn new(env_vars: HashMap<String, String>, _server_handle: Option<()>) -> Self {
        Self {
            pending_resolvers: HashMap::new(),
            next_op_id: 1,

            timer_callbacks: HashMap::new(),
            next_timer_id: 1,
            timer_owner: HashMap::new(),

            streams: HashMap::new(),
            next_stream_id: 1,

            spawned_ops: Vec::new(),
            spawned_timers: Vec::new(),
            ready_timers: Vec::new(),
            spawned_fetches: Vec::new(),

            executing_request_id: None,
            executing_request_cancel: None,

            per_request_logs: HashMap::new(),

            kv_store: HashMap::new(),
            env_vars,

            key_store: HashMap::new(),
            next_key_id: 1,

            outbound_streams: HashSet::new(),

            websockets: HashMap::new(),
            next_ws_id: 1,
        }
    }
}

// ---------------------------------------------------------------------------
// FetchRequest — queued by V8 callback, executed by runtime
// ---------------------------------------------------------------------------

/// A fetch request queued by the V8 `__rawFetch` callback.
/// The runtime executor drains these and spawns the actual HTTP I/O.
pub struct FetchRequest {
    pub op_id: u32,
    pub stream_id: u32,
    pub request_id: Option<u64>,
    pub method: String,
    pub url: String,
    pub headers_json: String,
    pub body: Option<String>,
    pub cancel: Option<()>,
}

// ---------------------------------------------------------------------------
// Stream state
// ---------------------------------------------------------------------------

/// State for a single `ReadableStream` instance.
#[allow(missing_debug_implementations)]
pub struct StreamState {
    /// Resolver waiting on the next `read()` call, if any.
    pub pending_read: Option<v8::Global<v8::PromiseResolver>>,
    /// Buffered chunks not yet consumed by JS.
    pub buffer: Vec<Vec<u8>>,
    /// Whether the stream has been closed/errored.
    pub closed: bool,
}

// ---------------------------------------------------------------------------
// Timer types
// ---------------------------------------------------------------------------

/// A timer that has been requested but not yet armed in the event loop.
#[derive(Debug)]
pub struct SpawnedTimer {
    /// Timer ID (matches the JS handle returned to the caller).
    pub id: u32,
    /// Initial delay before the first fire.
    pub delay: Duration,
    /// Repeat interval; `None` means one-shot (setTimeout).
    pub interval: Option<Duration>,
}

// ---------------------------------------------------------------------------
// Op result
// ---------------------------------------------------------------------------

/// Result produced by a spawned async op future.
pub enum OpResult {
    /// A regular async op finished — resolve its promise with `value`.
    Completed {
        op_id: u32,
        value: String,
        /// The request that owns this op (used to route logs / cancellation).
        request_id: Option<u64>,
    },
    /// A streaming body chunk arrived.
    StreamChunk {
        stream_id: u32,
        data: Vec<u8>,
        /// `true` signals end-of-stream.
        done: bool,
    },
    /// The op was cancelled (e.g. request was killed).
    Cancelled,
}

// ---------------------------------------------------------------------------
// Timer result
// ---------------------------------------------------------------------------

/// Produced when a timer fires; carries enough information to re-arm intervals.
#[derive(Debug)]
pub struct TimerResult {
    /// The timer ID that fired.
    pub id: u32,
    /// Present if this is a setInterval timer (the repeat period).
    pub interval: Option<Duration>,
}

// ---------------------------------------------------------------------------
// Dispatch result
// ---------------------------------------------------------------------------

/// Return value from JS op dispatch — tells the event loop what to do next.
pub enum DispatchResult {
    /// Op completed synchronously; value is the JSON result string.
    Sync(String),
    /// Op started asynchronously; the promise will be resolved later.
    Async(v8::Global<v8::Promise>),
    /// Dispatch failed; value is the error message.
    Error(String),
}
