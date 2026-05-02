//! Shared runtime state — types for the event loop.
//!
//! `RuntimeState` holds all V8 callback state, behind an `Rc<RefCell<>>` so
//! callbacks can borrow it without crossing thread boundaries.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap, VecDeque};
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

/// Maximum number of live timers (setTimeout + setInterval) per runtime.
/// Each timer holds a `v8::Global<v8::Function>` (~200 bytes) + a compio
/// sleep future. Without a cap, `for(;;) setTimeout(f, 1e9)` grows memory
/// indefinitely. 10,000 matches the V8 guideline for reasonable timer
/// density (Chrome DevTools warns at 10K pending timers).
pub const MAX_PENDING_TIMERS: usize = 10_000;

/// Separate, tighter cap on concurrent outbound fetches per runtime/app.
///
/// The shared cyper client is cross-app: if one app fires 1024 fetches at
/// a slow upstream, it can monopolize the connection pool and the DNS
/// resolver for every other app on the same worker thread. `MAX_PENDING_OPS`
/// alone doesn't distinguish fetches from cheap in-memory ops, so a
/// dedicated fetch cap is required for multi-tenant safety.
///
/// 64 is a heuristic: the Node.js default `http.globalAgent.maxSockets`
/// is infinity but Undici's dispatcher defaults to 128 per origin. We err
/// lower — platform apps are expected to reach only a handful of upstreams
/// at once, and a runaway loop (e.g. accidentally-recursive fetch) hits
/// this cap quickly enough to give the operator a clean error instead of
/// an OOM or a control-plane outage.
pub const MAX_PENDING_FETCHES: usize = 64;

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

/// Parse the EnvSnapshot wire JSON `{ vars, secrets, expose }` into typed
/// maps + the expose list. Defensive — missing or malformed fields
/// degrade to empty values. Non-string entries inside `vars` / `secrets`
/// are dropped (env values are string-to-string by contract).
fn parse_env_snapshot(json: &str) -> (BTreeMap<String, String>, BTreeMap<String, String>, Vec<String>) {
    let Ok(serde_json::Value::Object(root)) = serde_json::from_str::<serde_json::Value>(json) else {
        return (BTreeMap::new(), BTreeMap::new(), Vec::new());
    };

    fn pull_map(root: &serde_json::Map<String, serde_json::Value>, key: &str) -> BTreeMap<String, String> {
        let Some(serde_json::Value::Object(map)) = root.get(key) else {
            return BTreeMap::new();
        };
        map.iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
            .collect()
    }

    let vars = pull_map(&root, "vars");
    let secrets = pull_map(&root, "secrets");
    let expose = match root.get("expose") {
        Some(serde_json::Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    };
    (vars, secrets, expose)
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
    /// AbortSignal.timeout's strong-ref pin map (DOM §3.3 step 3 — for
    /// the duration of the timeout, the signal must be retained even
    /// if no JS reference exists). Keyed by the signal's timer ID;
    /// removed when the timer fires (in `dom::abort_signal::run_abort_steps`).
    pub timeout_pinned_signals: HashMap<u32, v8::Global<v8::Object>>,

    /// Monotonically increasing stream-id counter, shared across all
    /// stream-id-keyed maps (currently just `response_forwarders`).
    pub next_stream_id: u32,

    /// Response-body forwarders — Rust-side replacements for the legacy
    /// JS pump in `__zsBeginStreamForward`. Populated by
    /// `streams::response_forwarder::begin_forward` when the kernel's
    /// `inspect_response` decides a Response with a ReadableStream body
    /// should ship to the wire; drained via `attach_writer` /
    /// `is_closed` / `drain_into_complete` in `runtime.rs`.
    pub response_forwarders: HashMap<u32, crate::streams::response_forwarder::ResponseForwarder>,

    /// Futures for in-flight async ops (fetch, kv, ...).
    pub spawned_ops: Vec<Pin<Box<dyn Future<Output = OpResult>>>>,
    /// Timers queued to be armed on the next event-loop iteration.
    pub spawned_timers: Vec<SpawnedTimer>,
    /// Timer IDs ready to fire immediately (delay == 0).
    /// Drained by the Runtime after each enter_v8, avoiding sleep overhead.
    pub ready_timers: VecDeque<u32>,

    /// Number of fetches currently executing for this runtime.
    /// Incremented in the native `fetch_native::fetch_callback` when a
    /// fetch task is spawned, decremented when the algorithm chain
    /// returns (success or failure). Guards against one app exhausting
    /// the shared cyper client's connection pool — `MAX_PENDING_OPS`
    /// alone would let this climb to 1024 which is well within
    /// OOM-by-sockets territory when the upstream is slow.
    pub in_flight_fetches: usize,

    /// The request currently being executed (None between requests).
    pub executing_request_id: Option<u64>,
    /// Cancellation flag for the request currently being executed. Captured
    /// by V8 callbacks (e.g. `fetch_native::fetch_callback`) so that async
    /// ops they spawn inherit the same flag and abort early if the handler
    /// gives up.
    pub executing_request_cancel: Option<crate::channel::CancelFlag>,

    /// Per-request log lines accumulated during execution.
    pub per_request_logs: HashMap<u64, Vec<String>>,

    /// Per-request authenticated user JSON. Keyed by `request_id` so that
    /// async continuations (promise .then handlers, timer callbacks) read
    /// the identity of *the request that scheduled them* — not whatever
    /// user the thread happened to be handling at the moment the callback
    /// fires. An earlier revision used a thread-local here; that leaks
    /// across every `.await` boundary in a single-threaded async runtime.
    pub per_request_user: HashMap<u64, String>,

    /// For each in-flight request, the list of promises registered via
    /// `ctx.waitUntil(p)` from JS. The kernel keeps the isolate alive past
    /// the response body write until every promise settles or the wall
    /// timeout fires. Cleared when the request is discarded (e.g. after
    /// the wall budget elapses or the client cancels).
    ///
    /// TODO(PR 2): when the `__zs_wait_until` native op lands, also clear
    /// entries in `drain_request_logs`, `discard_request_state`, and the
    /// cancellation sweep in runtime.rs, alongside `per_request_user`.
    /// Otherwise leaked entries pin `v8::Global<v8::Promise>` for the
    /// isolate's lifetime.
    pub wait_until_by_request: HashMap<u64, Vec<v8::Global<v8::Promise>>>,

    /// Per-request Request JS object, keyed by request_id. Stored by the
    /// kernel when `call_fetch_handler` builds the Request; read by the
    /// `__zs_get_request` native op so user code can do
    /// `import { getRequest } from 'zeroship'; getRequest()` without the
    /// bootstrap having to call `__bindRequest(ctx, request)` on every
    /// request.
    ///
    /// Empty when the request is served through the RPC fast-path (no
    /// Request is constructed). Callers of `getRequest()` inside a
    /// "use server" function receive null/throw in that case — use the
    /// `default.fetch` contract if header/URL access is needed.
    pub request_by_id: HashMap<u64, v8::Global<v8::Object>>,

    /// Per-request JS-exposed `ctx` object, keyed by request_id.
    /// Populated via `__zs_bind_request_ctx(ctxObj)` from bootstrap JS;
    /// read via `__zs_get_request_ctx()` from any nested module that
    /// needs waitUntil/passThroughOnException without threading ctx
    /// through every function call. Lightweight replacement for
    /// AsyncLocalStorage — single-threaded isolate, request_id tracked
    /// by the pump across await boundaries.
    ///
    /// TODO(PR 2): clear entries in drain_request_logs /
    /// discard_request_state / cancellation sweep alongside
    /// per_request_user and wait_until_by_request.
    pub request_ctx_by_id: std::collections::HashMap<u64, v8::Global<v8::Object>>,

    /// In-memory KV store.
    pub kv_store: HashMap<String, String>,
    /// Worker-internal environment variables — NOT user-facing.
    ///
    /// Carries hand-injected slots like `APP_ID` (set by
    /// `crates/worker/src/cache.rs` so plugin-db / plugin-storage /
    /// plugin-kv can resolve the per-tenant scope). Distinct from the
    /// per-app `env_app_vars` / `env_app_secrets` which are the
    /// user-controlled environment.
    pub env_vars: HashMap<String, String>,

    /// User-controlled `vars` half of the EnvSnapshot. Plaintext. Always
    /// surfaced via `process.env`, the `zeroship.env` import, and
    /// `env.get()`. `BTreeMap` for deterministic iteration order.
    pub env_app_vars: BTreeMap<String, String>,

    /// User-controlled `secrets` half of the EnvSnapshot. Decrypted on
    /// the control plane, transmitted to the worker over an authenticated
    /// channel. NEVER lands in `process.env` unless the secret's name is
    /// explicitly listed in `env_expose_keys`. Visible via the
    /// `zeroship.env` import and `env.get()`.
    pub env_app_secrets: BTreeMap<String, String>,

    /// Per-app opt-in list: secret names the creator has explicitly
    /// allowed to surface in `process.env`. Empty by default. Lets
    /// libraries like LangChain that defensively read
    /// `process.env.OPENAI_API_KEY` continue to work, while keeping
    /// the rest of the secret namespace out of `process.env`.
    pub env_expose_keys: Vec<String>,

    /// Frozen env JSON snapshot — JSON.parse-able string in the
    /// `{ vars, secrets, expose }` wire shape. Passed across the worker
    /// → runtime boundary as bytes; mirrored on `RuntimeState` so the
    /// `__zs_env` callback's degraded fallback path (used only if
    /// `ensure_initialized` hasn't run yet) can hand back at least the
    /// merged map without rebuilding from the typed fields.
    pub env_json: String,

    /// Composite `env` object surfaced to user code — plugin namespaces
    /// overlaid on the scalar `env_json`. Built once by
    /// `RuntimeInner::ensure_initialized` and cached here so the
    /// `__zs_env` native op (a free function with only `SharedState`
    /// access) and `call_fetch_handler` both hand back the same frozen
    /// V8 Object reference. `None` until `ensure_initialized` has run.
    pub env_obj: Option<v8::Global<v8::Object>>,

    /// Frozen `ctx` object reused across every fetch dispatch. Constructed
    /// once at init with stateless `waitUntil` / `passThroughOnException`
    /// callbacks (both resolve current-request state via
    /// `executing_request_id`), then frozen. Passed as the third arg to
    /// `default.fetch(request, env, ctx)`.
    ///
    /// Why cache: V8 allocates a fresh Object + 2 Functions for every
    /// request when constructed inline, which triggers `JSObject::MigrateToMap`
    /// + `Object::Set` + `ApplyTransitionToDataProperty` hotspots (visible
    /// in perf report). A cached, frozen singleton is the same V8 object
    /// every time — no new allocations, no map transitions.
    pub ctx_obj: Option<v8::Global<v8::Object>>,

    /// WebCrypto key store, keyed by key-id.
    pub key_store: HashMap<u32, crate::crypto::KeyData>,
    /// Monotonically increasing key-id counter.
    pub next_key_id: u32,

    /// WebSocket instances, keyed by ws_id.
    pub websockets: HashMap<u32, WebSocketState>,
    /// Monotonically increasing WebSocket ID counter (incremented by 2 for pairs).
    pub next_ws_id: u32,

    /// Per-isolate time origin for `performance.now()`. Set once at Runtime
    /// creation. Prevents cross-app timing side-channels.
    pub perf_epoch: std::time::Instant,

    /// Clone of the pump's notification channel. Stored here so detached
    /// compio tasks (streaming fetch bodies, in particular) can wake the
    /// pump after pushing new entries into `spawned_ops` without needing a
    /// reference to `Runtime`, which is single-owner and guarded by a
    /// `RefCell` that can't be held across `.await`.
    pub pump_notify_tx: Option<futures::channel::mpsc::Sender<()>>,
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
            timeout_pinned_signals: HashMap::new(),

            next_stream_id: 1,
            response_forwarders: HashMap::new(),

            spawned_ops: Vec::new(),
            spawned_timers: Vec::new(),
            ready_timers: VecDeque::new(),
            in_flight_fetches: 0,

            executing_request_id: None,
            executing_request_cancel: None,

            per_request_logs: HashMap::new(),
            per_request_user: HashMap::new(),
            wait_until_by_request: HashMap::new(),
            request_by_id: HashMap::new(),
            request_ctx_by_id: HashMap::new(),

            kv_store: HashMap::new(),
            env_vars,
            env_app_vars: BTreeMap::new(),
            env_app_secrets: BTreeMap::new(),
            env_expose_keys: Vec::new(),

            env_json: r#"{"vars":{},"secrets":{},"expose":[]}"#.into(),
            env_obj: None,
            ctx_obj: None,

            key_store: HashMap::new(),
            next_key_id: 1,

            websockets: HashMap::new(),
            next_ws_id: 1,

            perf_epoch: std::time::Instant::now(),
            pump_notify_tx: None,
        }
    }

    /// Register a promise passed to `ctx.waitUntil(p)`. The promise is
    /// keyed by the currently executing request_id; returns false if no
    /// request is active (the caller's native op should throw to JS in
    /// that case). Promises stay alive until cleaned up when the request
    /// completes.
    pub fn register_wait_until(&mut self, promise: v8::Global<v8::Promise>) -> bool {
        let Some(rid) = self.executing_request_id else {
            return false;
        };
        self.wait_until_by_request
            .entry(rid)
            .or_default()
            .push(promise);
        true
    }

    /// Stash the env snapshot JSON so the `__zs_env` native op and
    /// `setup_globals`' `process.env` builder both see the same payload
    /// as the second arg of `fetch(req, env, ctx)`. Called by
    /// `call_fetch_handler` immediately after `ensure_initialized` and
    /// before dispatching into JS.
    ///
    /// Parses the `{ vars, secrets, expose }` wire shape into the typed
    /// fields. Malformed JSON degrades to empty maps — the runtime is
    /// the consumer of last resort and shouldn't panic on a bad payload
    /// from the control plane; the producer should already have
    /// validated.
    pub fn set_env_snapshot(&mut self, env: &crate::EnvSnapshot) {
        let new_json = env.as_json();
        // The env JSON is per-app, not per-request. Same string → skip
        // the JSON allocation + serde_json::from_str. The bench loop
        // hits this path on every request; for typical apps env stays
        // stable for the isolate's lifetime.
        if self.env_json == new_json {
            return;
        }
        self.env_json = new_json.to_string();
        let (vars, secrets, expose) = parse_env_snapshot(&self.env_json);
        self.env_app_vars = vars;
        self.env_app_secrets = secrets;
        self.env_expose_keys = expose;
    }

    /// Allocate a stream-id that is not currently held by an active forwarder or
    /// pending resolver. Uses the monotonic `next_stream_id` counter with
    /// collision-avoidance after wrap, so long-running runtimes (>12 hours at
    /// 100k fetches/sec) don't silently cross-wire a new stream with an
    /// in-flight one. Skips 0 so callers can treat it as a sentinel.
    pub fn alloc_stream_id(&mut self) -> u32 {
        loop {
            let sid = self.next_stream_id;
            self.next_stream_id = self.next_stream_id.wrapping_add(1);
            if sid == 0 {
                continue;
            }
            if self.response_forwarders.contains_key(&sid)
                || self.pending_resolvers.contains_key(&sid)
            {
                continue;
            }
            return sid;
        }
    }
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

/// A value to resolve or reject a promise with, materialized in V8 by the
/// runtime loop. Used by [`OpResult::JsValue`] so async class methods can
/// hand back arbitrary V8 chunks (not just UTF-8 strings).
///
/// Per streams design D-3: the existing `OpResult::Completed.value: String`
/// channel is wrong for streams (it round-trips chunks through UTF-8 and
/// silently mangles binary). Either we add this variant or maintain an
/// out-of-band registry; the variant is simpler and reuses the existing
/// dispatch loop in `runtime.rs`.
pub enum ResolveValue {
    /// Resolve with `undefined`.
    Undefined,
    /// Resolve with the given V8 value.
    JsGlobal(v8::Global<v8::Value>),
    /// Resolve with a Uint8Array view over the given bytes (zero-extra-copy:
    /// the bytes are moved into a fresh ArrayBuffer at dispatch time).
    Bytes(Vec<u8>),
    /// Reject with the given V8 value.
    Reject(v8::Global<v8::Value>),
}

/// Result produced by a spawned async op future.
#[allow(missing_debug_implementations)]
pub enum OpResult {
    /// A regular async op finished — resolve its promise with `value`.
    Completed {
        op_id: u32,
        value: String,
        /// The request that owns this op (used to route logs / cancellation).
        request_id: Option<u64>,
    },
    /// An async op failed — reject its promise with the error message.
    Failed {
        op_id: u32,
        error: String,
        request_id: Option<u64>,
    },
    /// A class-method async op finished — resolve/reject with a V8 value.
    ///
    /// Used by `#[v8_async_method]` and any direct caller that needs to
    /// hand back a real JS value (object, typed array, Promise, …) rather
    /// than a UTF-8 string. The resolver is stored directly on the
    /// variant (vs the `pending_resolvers` map keyed by op-id) so the
    /// runtime loop can resolve it without an extra lookup.
    ///
    /// Streams design §VII.5 / D-3.
    JsValue {
        resolver: v8::Global<v8::PromiseResolver>,
        value: ResolveValue,
        request_id: Option<u64>,
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

/// Classification of what `call_fetch_handler`'s sync turn produced.
/// `Pending` (handler returned an unsettled Promise) is tracked via the
/// outer `Result<DispatchResult, v8::Global<v8::Promise>>` in call_fetch_inner.
pub enum DispatchResult {
    /// Handler returned a `Response` (user-constructed or from the
    /// async-generator wrapper). Forwarded via the HTTP streaming /
    /// inspection path.
    HttpResponse(crate::http::ResponseInfo),
    /// Handler threw. All fields come from the JS exception. `status` is
    /// 500 by default, overridable by setting `err.status` to an integer
    /// in 400-599. `code`, `details_json`, `retryable` are zeroship's
    /// structured-error extension — carried only when the thrown value
    /// has them with the right type (string `code`, any-JSON `details`,
    /// boolean `retryable`); otherwise they're omitted from the wire.
    ErrorValue {
        message: String,
        name: String,
        stack: Option<String>,
        status: u16,
        code: Option<String>,
        details_json: Option<String>,
        retryable: Option<bool>,
    },
    /// Hard dispatch-layer error (Response inspection failed, Request
    /// object construction failed). Not a user-thrown value — no stack/name.
    Error(String),
}
