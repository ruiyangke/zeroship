//! Shared runtime state — types for the event loop.
//!
//! `RuntimeState` holds all V8 callback state, behind an `Rc<RefCell<>>` so
//! callbacks can borrow it without crossing thread boundaries.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::time::Duration;

use crate::v8::timers::TimerCallback;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Capacity of the channel that delivers requests to the runtime.
pub const REQUEST_CHANNEL_CAPACITY: usize = 256;

/// Maximum number of concurrent in-flight async ops (fetch, kv, ...).
pub const MAX_PENDING_OPS: usize = 1024;

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
    pub key_store: HashMap<u32, crate::v8::crypto::KeyData>,
    /// Monotonically increasing key-id counter.
    pub next_key_id: u32,

    /// Stream IDs that have outbound StreamForwarders (HTTP streaming responses).
    /// When a stream_id is in this set, `stream_enqueue_callback` forwards chunks
    /// via the stream events channel instead of buffering them for JS reads.
    pub outbound_streams: HashSet<u32>,
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
