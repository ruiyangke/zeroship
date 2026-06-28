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
use std::time::{Duration, Instant};

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
///
/// Used to be `Copy`; now `Clone` only because the `JsValue` variant
/// carries a `v8::Global<v8::Value>` that's `Clone` but not `Copy`.
/// Existing match-on-`err.kind` sites must update to `match &err.kind`
/// (two known consumers in the runtime, swept in the same commit that
/// introduced this variant).
#[derive(Debug, Clone)]
pub enum OpErrorKind {
    /// `TypeError` — wrong argument types, missing arguments
    TypeError,
    /// `RangeError` — value out of bounds
    RangeError,
    /// Generic `Error`
    Error,
    /// A WebIDL `DOMException`. The `&'static str` carries the spec
    /// `name` (e.g. `"OperationError"`, `"DataError"`,
    /// `"NotSupportedError"`, `"InvalidAccessError"`,
    /// `"QuotaExceededError"`, `"TypeMismatchError"`, `"DataCloneError"`,
    /// `"SyntaxError"`). The macro's `gen_throw_error` arm constructs a
    /// real `globalThis.DOMException(message, name)` instance via the
    /// native `DOMException` class installed in `setup_globals`.
    DomException(&'static str),
    /// A Node.js-style error code (e.g. `"ERR_CRYPTO_HASH_FINALIZED"`,
    /// `"ERR_INVALID_ARG_TYPE"`). The macro's `gen_throw_error` arm
    /// constructs a JS `Error`, `TypeError`, or `RangeError` per the
    /// per-code class table (see `core/error.rs::node_error_class_for`)
    /// and assigns the `code` property as a static string. npm packages
    /// branch on `e.code === "ERR_..."`.
    NodeError(&'static str),
    /// A plugin-side coded error: a plain JS `Error` with a dynamic
    /// `e.code` (and optional `e.hint`) property attached. Use this
    /// when the code is determined at runtime (e.g. plugin-db migration
    /// lifecycle errors like `"migration_already_running"`) and so
    /// can't be expressed as the `&'static str` payload `NodeError`
    /// carries. The plumbing in `core::runtime::OpResult::JsValue`
    /// builds a JS `Error`, sets `e.code = code`, and if `hint` is
    /// non-empty also sets `e.hint = hint`. The optional `hint` is a
    /// human-facing recovery note (one line) for messages we know
    /// rejecters often need.
    CodedError { code: String, hint: Option<String> },
    /// A pre-built JS exception value, captured from a user-thrown
    /// exception in a nested V8 callback (custom `toString`,
    /// `Symbol.toPrimitive`, throwing `valueOf`, etc.). The macro's
    /// throw-error arms call `scope.throw_exception(local_from_global)`
    /// directly so the user's original thrown value reaches `catch`
    /// blocks verbatim — preserves Error subclasses, custom
    /// properties (`e.code`), the `instanceof` chain, all of it.
    ///
    /// Constructed by `OpError::js_value(scope, exception)` and emitted
    /// by the macro's per-member dict / enum extraction whenever a
    /// `WebIdlConvertible::from_v8` call's TryCatch caught a pending
    /// exception. The `OpError.message` carries a debug stringification
    /// of the exception for log surfaces; the actual JS-visible value
    /// is the `Global<Value>` payload.
    JsValue(v8::Global<v8::Value>),
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

    /// Construct a `DOMException`-flavoured `OpError`. The `name`
    /// argument must be a spec DOMException name (e.g.
    /// `"OperationError"`, `"DataError"`, `"NotSupportedError"`,
    /// `"InvalidAccessError"`, `"QuotaExceededError"`,
    /// `"TypeMismatchError"`, `"DataCloneError"`, `"SyntaxError"`).
    /// The macro emits a `new DOMException(msg, name)` instance.
    pub fn dom(name: &'static str, msg: impl Into<String>) -> Self {
        Self {
            kind: OpErrorKind::DomException(name),
            message: msg.into(),
        }
    }

    /// Construct a Node.js-style error with the given `code`. The macro
    /// arm picks Error / TypeError / RangeError per the per-code table
    /// at `crate::node_error::class_for(code)` and sets `e.code = code`
    /// on the resulting JS exception.
    pub fn node(code: &'static str, msg: impl Into<String>) -> Self {
        Self {
            kind: OpErrorKind::NodeError(code),
            message: msg.into(),
        }
    }

    /// Construct a plugin-side coded error. Builds a JS `Error` with
    /// `e.code = code` (and `e.hint = hint` when provided). Use for
    /// runtime-determined codes that don't fit `OpError::node`'s
    /// `&'static str` constraint — e.g. plugin-db migration lifecycle
    /// codes (`"migration_already_running"`, `"migration_cancelled"`,
    /// …) that the SDK branches on with `if (e.code === "...")`.
    pub fn coded(
        code: impl Into<String>,
        msg: impl Into<String>,
        hint: Option<impl Into<String>>,
    ) -> Self {
        Self {
            kind: OpErrorKind::CodedError {
                code: code.into(),
                hint: hint.map(Into::into),
            },
            message: msg.into(),
        }
    }

    /// Capture a user-thrown JS exception verbatim. The macro's
    /// per-member dict / enum extraction wraps each
    /// `WebIdlConvertible::from_v8` call in a `v8::tc_scope!`; if the
    /// inner call left a pending V8 exception (e.g. user code threw
    /// from a custom `toString`), this constructor stashes the
    /// exception value as a `Global` so the surrounding throw machinery
    /// re-throws it verbatim — `catch` blocks observe the original
    /// thrown value (Error subclass, `e.code`, custom properties).
    ///
    /// `message` is a debug stringification used by logs / Display.
    /// The actual JS-visible value is the captured exception.
    pub fn js_value(
        scope: &mut v8::PinScope,
        exception: v8::Local<v8::Value>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            kind: OpErrorKind::JsValue(v8::Global::new(scope, exception)),
            message: message.into(),
        }
    }

    /// Materialise this `OpError` as a JS exception value of the right
    /// class (`TypeError` / `RangeError` / `Error` / `DOMException` /
    /// Node-coded / plugin-coded), or the captured value verbatim for
    /// the `JsValue` variant.
    ///
    /// This is the single source of truth for the OpError → JS-exception
    /// lowering used by (a) the async pump's `OpResult::JsValue` →
    /// `ResolveValue::RejectError` arm, and (b) any caller that needs to
    /// reject a `v8::PromiseResolver` with a typed error directly from
    /// Rust (e.g. plugin-db's native `Db.transaction(fn)` orchestrator).
    /// It mirrors the macro's `gen_throw_op_error_arms` shape so a throw
    /// and a Promise rejection produce byte-identical JS error objects.
    pub fn to_exception<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
    ) -> v8::Local<'s, v8::Value> {
        match &self.kind {
            OpErrorKind::JsValue(global) => v8::Local::new(scope, global),
            _ => {
                let msg = v8::String::new(scope, &self.message).unwrap();
                match &self.kind {
                    OpErrorKind::TypeError => v8::Exception::type_error(scope, msg),
                    OpErrorKind::RangeError => v8::Exception::range_error(scope, msg),
                    OpErrorKind::Error => v8::Exception::error(scope, msg),
                    OpErrorKind::DomException(name) => {
                        crate::dom::exception::build(scope, &self.message, name).into()
                    }
                    OpErrorKind::NodeError(code) => {
                        crate::node_error::build_node_exception(scope, code, &self.message)
                    }
                    OpErrorKind::CodedError { code, hint } => {
                        let exc = v8::Exception::error(scope, msg);
                        if let Ok(obj) = v8::Local::<v8::Object>::try_from(exc) {
                            let code_key = v8::String::new(scope, "code").unwrap();
                            let code_val = v8::String::new(scope, code).unwrap();
                            obj.set(scope, code_key.into(), code_val.into());
                            if let Some(h) = hint {
                                let hint_key = v8::String::new(scope, "hint").unwrap();
                                let hint_val = v8::String::new(scope, h).unwrap();
                                obj.set(scope, hint_key.into(), hint_val.into());
                            }
                        }
                        exc
                    }
                    OpErrorKind::JsValue(_) => unreachable!("handled above"),
                }
            }
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
    /// Trusted JS-driver command channel for platform-owned migrate isolates.
    ///
    /// This is opt-in by construction: normal worker/CLI creator runtimes leave
    /// it `None`, so the driver globals are never installed on the creator
    /// `env`/global surface. The migrate crate seeds it only for its dedicated
    /// Trusted driver Runtime.
    pub js_driver: Option<JsDriverState>,

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

    /// Stream-ids of paused upload forwarders awaiting resume. A forwarder
    /// pauses its `reader.read()` loop when its downstream `StreamWriter`
    /// buffer crosses the high-water mark (backpressure); the consumer of the
    /// paired `StreamReader` (e.g. `env.storage.putStream` → S3 multipart)
    /// enqueues the id here once it has drained the buffer below the low-water
    /// mark. The pump services these inside its V8 scope (`resume_read`),
    /// re-arming the read loop. This is what keeps a large streaming upload
    /// bounded by the buffer cap instead of overflowing it.
    pub forwarder_resumes: VecDeque<u32>,

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

    /// Per-WebSocket-connection authenticated user JSON, keyed by native
    /// `ws_id`. Captured at WS upgrade time (the `WebSocketPair` mint runs
    /// inside the upgrading `fetch` handler, where `executing_request_id`
    /// still resolves the connection's user). Both sockets of a pair are
    /// bound to the same user — a connection has one identity.
    ///
    /// The WS-event pump (`OpResult::WebSocketEvent`) reads this to
    /// re-establish the connection's identity for every WS turn
    /// (`onmessage` / `onclose`) via `executing_ws_user`, so
    /// `env.auth.getUser()` inside a WS handler returns THIS connection's
    /// user — never null, never a stale leftover from a prior request on
    /// the pooled isolate. Dropped in `free_native_ws_state`.
    #[cfg(feature = "runtime_native_websocket")]
    pub ws_user: HashMap<u32, String>,

    /// The connection user the WS-event pump has bound for the current WS
    /// turn (set from `ws_user[ws_id]` immediately before entering V8 to
    /// dispatch `onmessage` / `onclose`, cleared right after). Read by
    /// `auth::current_user` as the per-turn identity when no
    /// `executing_request_id`-keyed user resolves. This is the WS analogue
    /// of the `executing_request_id` → `per_request_user` lookup the normal
    /// fetch / op / timer turns use.
    #[cfg(feature = "runtime_native_websocket")]
    pub executing_ws_user: Option<String>,

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

    /// **Migration-first cutover (P4b/P5 S2)** — the bundled
    /// `RuntimeSchemaDescriptor` JSON (`schema.runtime.json`; v1 is
    /// `{ version, collections: { fields, options, indexes } }`) carried in
    /// `manifest.runtime_descriptor`. The worker
    /// resolves the descriptor blob via `BlobStore` at bundle-load and stamps
    /// it here through `RuntimeBuilder::runtime_descriptor`. `setup_globals`
    /// parses it and exposes it to JS as `globalThis.__zsRuntimeDescriptor` so
    /// `@zeroship/bootstrap`'s entry sources the schema from the migration fold
    /// when present. `None` for apps that ship no migrations/descriptor; those
    /// apps install no schema.
    pub runtime_descriptor: Option<String>,

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

    /// WebSocket instances, keyed by ws_id.
    pub websockets: HashMap<u32, WebSocketState>,
    /// Monotonically increasing WebSocket ID counter (incremented by 2 for pairs).
    pub next_ws_id: u32,

    /// Native WebSocket per-id state (events queue, send queue, cancel flag).
    /// Disjoint from `websockets`/`next_ws_id` (which are polyfill-side).
    /// Removed once the polyfill is deleted in cutover landing 3.
    #[cfg(feature = "runtime_native_websocket")]
    pub native_websockets: HashMap<u32, std::rc::Rc<std::cell::RefCell<crate::websocket_native::network::NativeWsState>>>,
    /// Monotonically increasing native WebSocket id counter.
    #[cfg(feature = "runtime_native_websocket")]
    pub next_native_ws_id: u32,
    /// Cached JS wrapper Global per native ws_id. Captured by the
    /// constructor so the dispatch arm can resolve the wrapper from
    /// `ws_id` alone (no need to walk listener registries). Removed
    /// when the WebSocket transitions to CLOSED + the per-WS state
    /// is freed; the wrapper Global keeps the underlying
    /// `WebSocketImpl` alive until the JS GC collects the JS object.
    #[cfg(feature = "runtime_native_websocket")]
    pub native_ws_wrappers: HashMap<u32, v8::Global<v8::Object>>,

    /// Raw TCP policy for `node:net`. Default is `Denied`, which also
    /// makes the synthetic module unresolvable.
    pub net_policy: crate::transport::net_policy::NetPolicy,
    /// Native `node:net.Socket` states, keyed by native socket id.
    pub native_sockets: HashMap<
        u32,
        std::rc::Rc<std::cell::RefCell<crate::node::net::state::NativeSocketState>>,
    >,
    /// Monotonically increasing native socket id counter.
    pub next_native_socket_id: u32,
    /// Cached JS `Socket` facade per native socket id. The native host
    /// object is only the kernel handle; events are emitted on this
    /// EventEmitter facade.
    pub native_socket_wrappers: HashMap<u32, v8::Global<v8::Object>>,
    /// Per-runtime concurrent raw TCP sockets that have passed the
    /// connect-time policy check and have not yet closed.
    pub active_native_sockets: u32,
    /// Most recent successful read/write/connect activity on any open
    /// native `node:net` socket. Worker LRU eviction folds this into the
    /// isolate's `last_used` timestamp so an active DB connection does not
    /// look idle just because no HTTP request is currently entering V8.
    pub native_socket_last_activity: Option<Instant>,
    /// Per-runtime accepted outbound bytes for Allowlist egress ceiling.
    pub native_net_egress_bytes: u64,
    /// Once the runtime crosses its hard native-net egress ceiling, refuse
    /// additional socket opens/writes for this isolate.
    pub native_net_egress_exhausted: bool,
    /// Server-stamped per-app meter handle. Native `node:net` records accepted
    /// outbound bytes into the fixed `egress_bytes` spend metric through this
    /// handle; absent in meter-less test harnesses.
    pub meter: Option<zeroship_metering::MeterHandle>,

    /// Explicit isolate leases held by trusted callers such as the future
    /// migrate executor. A leased runtime is un-evictable by the worker LRU.
    pub isolate_lease_count: u32,

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
    /// Wake the pump task so it drains newly added work (spawned ops,
    /// forwarder resumes, …). Cheap and lossy: the channel is a 1-slot
    /// notification, so a `try_send` that fails because one is already queued
    /// is fine — the pump will see the work on its next turn.
    pub fn notify_pump(&self) {
        if let Some(tx) = &self.pump_notify_tx {
            let _ = tx.clone().try_send(());
        }
    }

    /// Create a new `RuntimeState` seeded with the given environment variables.
    pub fn new(
        env_vars: HashMap<String, String>,
        _server_handle: Option<()>,
        meter: Option<zeroship_metering::MeterHandle>,
    ) -> Self {
        Self {
            js_driver: None,

            pending_resolvers: HashMap::new(),
            next_op_id: 1,

            timer_callbacks: HashMap::new(),
            next_timer_id: 1,
            timer_owner: HashMap::new(),
            timeout_pinned_signals: HashMap::new(),

            next_stream_id: 1,
            response_forwarders: HashMap::new(),
            forwarder_resumes: VecDeque::new(),

            spawned_ops: Vec::new(),
            spawned_timers: Vec::new(),
            ready_timers: VecDeque::new(),
            in_flight_fetches: 0,

            executing_request_id: None,
            executing_request_cancel: None,

            per_request_logs: HashMap::new(),
            per_request_user: HashMap::new(),
            #[cfg(feature = "runtime_native_websocket")]
            ws_user: HashMap::new(),
            #[cfg(feature = "runtime_native_websocket")]
            executing_ws_user: None,
            wait_until_by_request: HashMap::new(),
            request_by_id: HashMap::new(),
            request_ctx_by_id: HashMap::new(),

            kv_store: HashMap::new(),
            env_vars,
            runtime_descriptor: None,
            env_app_vars: BTreeMap::new(),
            env_app_secrets: BTreeMap::new(),
            env_expose_keys: Vec::new(),

            env_json: r#"{"vars":{},"secrets":{},"expose":[]}"#.into(),
            env_obj: None,
            ctx_obj: None,


            websockets: HashMap::new(),
            next_ws_id: 1,

            #[cfg(feature = "runtime_native_websocket")]
            native_websockets: HashMap::new(),
            #[cfg(feature = "runtime_native_websocket")]
            next_native_ws_id: 1,
            #[cfg(feature = "runtime_native_websocket")]
            native_ws_wrappers: HashMap::new(),

            net_policy: crate::transport::net_policy::NetPolicy::Denied,
            native_sockets: HashMap::new(),
            next_native_socket_id: 1,
            native_socket_wrappers: HashMap::new(),
            active_native_sockets: 0,
            native_socket_last_activity: None,
            native_net_egress_bytes: 0,
            native_net_egress_exhausted: false,
            meter,
            isolate_lease_count: 0,

            perf_epoch: std::time::Instant::now(),
            pump_notify_tx: None,
        }
    }

    pub fn set_net_policy(&mut self, policy: crate::transport::net_policy::NetPolicy) {
        self.net_policy = policy;
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

/// Command mailbox for a platform-authored JS driver loop.
///
/// The state is intentionally generic JSON at the runtime boundary. Migrate's
/// `JsDriverConn` owns the typed protocol and result decoding; the runtime only
/// resolves the parked `__zsNextCommand()` promise and relays
/// `__zsResolve(id, payload)` back to Rust.
#[allow(missing_debug_implementations)]
pub struct JsDriverState {
    pub dsn_json: String,
    pub command_queue: VecDeque<String>,
    pub next_command_resolver: Option<v8::Global<v8::PromiseResolver>>,
    pub result_senders: HashMap<u64, crate::channel::ResultSender<String>>,
}

impl JsDriverState {
    pub fn new(dsn_json: String) -> Self {
        Self {
            dsn_json,
            command_queue: VecDeque::new(),
            next_command_resolver: None,
            result_senders: HashMap::new(),
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
/// The existing `OpResult::Completed.value: String` channel is wrong for
/// streams because it round-trips chunks through UTF-8 and silently
/// mangles binary. This variant keeps the dispatch loop in `runtime.rs`
/// but lets async class methods resolve or reject with real JS values.
pub enum ResolveValue {
    /// Resolve with `undefined`.
    Undefined,
    /// Resolve with the given V8 value.
    JsGlobal(v8::Global<v8::Value>),
    /// Resolve with a Uint8Array view over the given bytes (zero-extra-copy:
    /// the bytes are moved into a fresh ArrayBuffer at dispatch time).
    Bytes(Vec<u8>),
    /// Resolve with a JS string materialised from this UTF-8 buffer.
    String(String),
    /// Resolve with the JS value produced by `JSON.parse(<this>)`.
    /// Used by callers (e.g. plugin-db CRUD) that need to hand back
    /// real JS objects / `null` / numbers without pre-building the
    /// `v8::Global<Value>` (which would require carrying a V8 scope
    /// across the spawned async op).
    Json(String),
    /// **P9 PR 2** — resolve with `JSON.parse(<this>)` and then run
    /// `transform` over the parsed value to mint plugin-supplied v8_class
    /// instances (today: `MaskedValue` for `__zsmask__`-tagged sentinels
    /// emitted by `crud::mask_pass::wrap_row_on_read`).
    ///
    /// `transform` is a `fn` (not a closure) so the variant stays `Send`
    /// across the spawned-op future boundary. The pump invokes it inside
    /// the V8 scope where the parsed value is live; the function may
    /// recursively walk the value and replace sub-objects with
    /// v8_class-backed instances. A `None` return means "leave the value
    /// unchanged" — the pump still resolves with the post-parse Local.
    ///
    /// Plugin-db is the only producer; the runtime itself has no
    /// knowledge of which sentinels exist — that lives in the supplied
    /// `transform` function pointer.
    JsonWithRehydration {
        json: String,
        transform:
            for<'s, 'a> fn(
                &mut v8::PinScope<'s, 'a>,
                v8::Local<'s, v8::Value>,
            ) -> Option<v8::Local<'s, v8::Value>>,
    },
    /// Resolve with a JS Boolean.
    Bool(bool),
    /// Resolve with a JS Number from an unsigned 32-bit integer.
    U32(u32),
    /// Resolve with a JS Number from a signed 32-bit integer.
    I32(i32),
    /// Resolve with a JS Number from a 64-bit float.
    F64(f64),
    /// Resolve with a JS `BigInt` from a signed 64-bit integer. Used by
    /// callers (e.g. `env.kv.incr`) whose counter can exceed the
    /// `Number.MAX_SAFE_INTEGER` (2^53) range an `f64` represents
    /// exactly — beyond that, an `f64` silently loses precision, so the
    /// value is handed back as a `BigInt` instead.
    BigInt(i64),
    /// Reject with the given V8 value.
    Reject(v8::Global<v8::Value>),
    /// Reject with a typed Error (TypeError / RangeError / Error)
    /// materialised by the pump from this `OpError`. Used by
    /// `#[v8_async_method]` codegen so user methods can return
    /// `Result<T, OpError>` and the pump constructs the right JS
    /// exception kind without the future needing a scope.
    RejectError(OpError),
    /// **P9 PR 3** — run a plugin-supplied continuation inside the
    /// pump's V8 scope **instead of** resolving the bound resolver.
    ///
    /// Unlike every other variant, this one does not settle a promise by
    /// itself: the pump simply invokes `run(scope, state)` inside the
    /// `enter_v8!` block (where a live `ContextScope` and the
    /// `SharedState` are both in hand) and lets the closure decide what
    /// to do — typically: build more V8 objects, call a user `Function`,
    /// and attach `.then(...)` handlers whose own callbacks push fresh
    /// `spawned_ops`.
    ///
    /// This is the seam the native `Db.transaction(fn)` orchestrator
    /// uses: after the async `BEGIN` / `SAVEPOINT` completes, the spawned
    /// op hands back a `Continuation` that (1) mints the tx-view object,
    /// (2) calls the creator's async callback, (3) coerces the return to
    /// a Promise, and (4) attaches Rust-backed commit / rollback
    /// handlers. The runtime stays oblivious to all of that — it only
    /// knows "run this closure in a scope."
    ///
    /// The closure is a `Box<dyn FnOnce>` (not a bare `fn`) so it can
    /// capture the orchestrator's owned state — the user-callback
    /// `Global<Function>`, the outer `Global<PromiseResolver>`, the
    /// savepoint name, the app id. The whole `OpResult` chain is already
    /// `!Send` (it carries `v8::Global` handles and is polled on a
    /// single-threaded compio executor), so the boxed closure adds no
    /// new thread-safety constraint.
    ///
    /// The bound resolver on the `OpResult::JsValue` envelope carrying
    /// this variant is unused (the continuation owns whichever resolver
    /// it intends to settle); callers pass a throwaway resolver to keep
    /// the envelope shape uniform.
    Continuation(Box<dyn FnOnce(&mut v8::PinScope, &SharedState)>),
}

/// Convert a Rust value into a `ResolveValue` so async methods can
/// hand back arbitrary primitives (and Vec<u8>) through the
/// `OpResult::JsValue` channel without each call site reinventing the
/// dispatch shape.
///
/// The macro `#[v8_async_method]` calls `result.into_resolve_value()`
/// after the user's `async fn` body runs; the trait fans out per
/// return type into the right `ResolveValue` variant. The pump
/// (`runtime.rs::OpResult::JsValue` arm) then materialises the V8
/// value and resolves (or rejects) the bound promise.
///
/// `Result<T: IntoResolveValue, OpError>` is supported transparently:
/// `Err` becomes `ResolveValue::RejectError`, which the pump turns
/// into the matching `TypeError` / `RangeError` / `Error` exception.
pub trait IntoResolveValue {
    fn into_resolve_value(self) -> ResolveValue;
}

impl IntoResolveValue for () {
    fn into_resolve_value(self) -> ResolveValue {
        ResolveValue::Undefined
    }
}

impl IntoResolveValue for Vec<u8> {
    fn into_resolve_value(self) -> ResolveValue {
        ResolveValue::Bytes(self)
    }
}

impl IntoResolveValue for String {
    fn into_resolve_value(self) -> ResolveValue {
        ResolveValue::String(self)
    }
}

impl IntoResolveValue for bool {
    fn into_resolve_value(self) -> ResolveValue {
        ResolveValue::Bool(self)
    }
}

impl IntoResolveValue for u32 {
    fn into_resolve_value(self) -> ResolveValue {
        ResolveValue::U32(self)
    }
}

impl IntoResolveValue for i32 {
    fn into_resolve_value(self) -> ResolveValue {
        ResolveValue::I32(self)
    }
}

impl IntoResolveValue for f64 {
    fn into_resolve_value(self) -> ResolveValue {
        ResolveValue::F64(self)
    }
}

impl IntoResolveValue for v8::Global<v8::Value> {
    fn into_resolve_value(self) -> ResolveValue {
        ResolveValue::JsGlobal(self)
    }
}

/// Newtype wrapper that resolves a JS Promise by `JSON.parse`-ing the
/// inner UTF-8 buffer. Use this from `#[v8_async_method]` bodies that
/// want to hand back a real JS object / `null` / number — instead of
/// `String` (which resolves with the raw JS string).
#[derive(Debug)]
pub struct JsonValue(pub String);

impl IntoResolveValue for JsonValue {
    fn into_resolve_value(self) -> ResolveValue {
        ResolveValue::Json(self.0)
    }
}

impl<T: IntoResolveValue> IntoResolveValue for Result<T, OpError> {
    fn into_resolve_value(self) -> ResolveValue {
        match self {
            Ok(v) => v.into_resolve_value(),
            Err(e) => ResolveValue::RejectError(e),
        }
    }
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
    JsValue {
        resolver: v8::Global<v8::PromiseResolver>,
        value: ResolveValue,
        request_id: Option<u64>,
    },
    /// The op was cancelled (e.g. request was killed).
    Cancelled,
    /// A native WebSocket event is ready for dispatch on the V8 thread.
    /// The actual event payload sits in `RuntimeState::native_websockets[ws_id].events`
    /// — the pump arm drains and dispatches in FIFO order. One queued
    /// `OpResult::WebSocketEvent` corresponds to ONE pushed event;
    /// extras are coalesced (drain returns all queued events in one
    /// shot, leaving subsequent OpResult::WebSocketEvent occurrences
    /// to be no-ops). See `websocket_native::network::drain_events`.
    #[cfg(feature = "runtime_native_websocket")]
    WebSocketEvent { ws_id: u32 },
    /// A native `node:net.Socket` event is ready for EventEmitter
    /// dispatch on the V8 thread. Payload is queued under
    /// `RuntimeState::native_sockets[socket_id].events`.
    SocketEvent { socket_id: u32 },
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
