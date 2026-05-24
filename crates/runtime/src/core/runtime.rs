//! Runtime — compio event loop with V8 isolate.
//!
//! # Known limitations
//!
//! These are architectural constraints, not bugs. Fixing them requires
//! larger rewrites than fit any single patch. They're listed here so
//! operators and future contributors don't re-discover them the hard way.
//!
//! ## 1. One slow JS handler blocks the entire worker thread
//!
//! The pump holds `Runtime.borrow_mut()` for the duration of each V8 turn
//! (microtask checkpoint + user JS + promise resolution). Every ntex
//! handler on the same thread also takes `borrow_mut()` around its
//! synchronous dispatch. Single-threaded compio can't deadlock, but one
//! slow handler (big `JSON.parse`, heavy string build, CPU-bound loop)
//! head-of-line-blocks every other in-flight request on that thread until
//! it yields back to the pump. Per-thread worker count (`--workers N`)
//! bounds the blast radius but doesn't eliminate it.
//!
//! Mitigations attempted: CPU timer kills runaway JS (see `cpu_timer.rs`).
//! A proper fix would slice V8 turns cooperatively or move V8 work into a
//! blocking thread pool — both are significantly larger projects.
//!
//! ## 2. Deploys drop in-flight requests on the old isolate
//!
//! `cache.rs::load_app` removes the old `IsolateEntry` before inserting
//! the new one. In-flight requests holding a `Runtime` handle still see
//! their `Rc<RefCell<RuntimeInner>>` alive (so they complete on the OLD
//! isolate), but there's no versioned cache that says "route new traffic
//! to v2 while v1 drains". Zero-downtime deploys require a versioned
//! isolate map + per-version router, which isn't wired in yet.
//!
//! ## 3. `SharedState` is a single `Rc<RefCell<RuntimeState>>`
//!
//! Every V8 callback, pump path, and detached task borrows this one cell.
//! Nothing is enforcing "don't hold a borrow across an await" beyond
//! convention — a future bug that does will panic at runtime, not fail
//! to compile. Splitting `RuntimeState` into narrower sub-states (timers,
//! streams, fetches, request-scoped) would make the invariants local,
//! but it's a wide-blast-radius refactor.
//!
//! ## 4. `cache::CACHE` is `thread_local!`
//!
//! Each ntex worker thread owns its own isolate set. V8 isolates are
//! thread-bound, so this is a hard constraint — you can't evict a
//! thread's app from outside that thread. The sync-loop poller works
//! around this by writing into a shared `Arc<RwLock<VersionMap>>` and
//! letting each thread reconcile independently (see `worker/src/sync.rs`).
//! Admin operations (force-evict, introspection) still have to travel
//! through an ntex handler because that's the only code path that runs
//! on a worker thread.
//!
//! See also: `docs/specs/` for higher-level design docs.
//!
//!
//! Same architecture as runtime-tokio's Runtime, but uses compio for timers
//! and the outer event loop. V8 dispatch is identical (zeroship-v8-core).
//!
//! The main difference: `compio::time::sleep` replaces `tokio::time::sleep`,
//! and `futures::select!` replaces `tokio::select!`.
//!
//! ## Async dispatch architecture
//!
//! V8 is single-threaded. Multiple compio connection tasks share one
//! `Runtime` handle (which wraps `Rc<RefCell<RuntimeInner>>`). The key
//! constraint: the RefCell borrow must NEVER be held across an `.await`
//! point.
//!
//! **Sync handlers** (ping, fib, uuid): `dispatch_start` returns
//! `DispatchOutcome::Complete` — the connection handler gets the result
//! immediately, no channel, no pump involvement.
//!
//! **Async handlers** (setTimeout, fetch, crypto): `dispatch_start` returns
//! `DispatchOutcome::Pending` with a oneshot receiver. A background **pump
//! task** owns the `AsyncWork` (FuturesUnordered for ops + timers), polls
//! them, and briefly borrows Runtime to enter V8 and resolve promises.
//! When a promise settles, the pump sends the result via the oneshot.

#![allow(unsafe_code)]

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::stream::FuturesUnordered;

use crate::init::{init_v8, load_polyfills_and_modules};
use crate::http::{self, ResponseInfo, SettledResult, HTTP_CREATE_REQUEST_JS};
use crate::modules::ModuleEntry;
use crate::plugin::NativePlugin;
use crate::state::{
    DispatchResult, OpResult, ResolveValue, RuntimeState, SharedState, SpawnedTimer,
    TimerResult,
};

use crate::channel::{
    self, CancelFlag, ResultSender,
};

// ---------------------------------------------------------------------------
// DispatchOutcome — result of dispatch_start
// ---------------------------------------------------------------------------

/// Carries an error message and the HTTP status code it should map to.
/// `From<String>` and `From<&str>` default the status to 500 so existing
/// call sites that produced plain `Err("...".into())` continue to work
/// (they just get the generic 500 default).
#[derive(Debug, Clone)]
pub struct DispatchError {
    pub message: String,
    pub status: u16,
}

impl DispatchError {
    pub fn new(message: impl Into<String>, status: u16) -> Self {
        Self { message: message.into(), status }
    }
}

impl From<String> for DispatchError {
    fn from(s: String) -> Self {
        Self { message: s, status: 500 }
    }
}

impl From<&str> for DispatchError {
    fn from(s: &str) -> Self {
        Self { message: s.into(), status: 500 }
    }
}

// ---------------------------------------------------------------------------
// AsyncWork — owned by the pump task, NOT by Runtime
// ---------------------------------------------------------------------------

/// Async futures extracted from Runtime so the pump task can poll them
/// without holding a RefCell borrow on Runtime across await points.
pub struct AsyncWork {
    pub pending_ops: FuturesUnordered<Pin<Box<dyn Future<Output = OpResult>>>>,
    pub pending_timers: FuturesUnordered<Pin<Box<dyn Future<Output = TimerResult>>>>,
}

impl AsyncWork {
    pub fn new() -> Self {
        Self {
            pending_ops: FuturesUnordered::new(),
            pending_timers: FuturesUnordered::new(),
        }
    }
}

/// Event from AsyncWork that the pump delivers to Runtime for V8 processing.
pub enum AsyncEvent {
    Op(OpResult),
    Timer(TimerResult),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RuntimeLimits {
    pub cpu_limit: Option<Duration>,
    pub wall_timeout: Option<Duration>,
    /// V8 heap limit in bytes. `None` → 128 MB default.
    pub heap_limit_bytes: Option<usize>,
}

/// Idle GC threshold: after this much wall time without a request, the
/// per-isolate idle ticker fires `Isolate::low_memory_notification` so V8
/// reclaims the high-water-mark working set during quiet windows.
/// See `docs/reference/runtime-limits.md` § "Idle GC".
pub const DEFAULT_IDLE_GC_AFTER: Duration = Duration::from_millis(30_000);

/// Wake interval for the idle-GC ticker. Each tick checks elapsed time
/// since the last request — if `>= idle_gc_after`, fires the GC hint.
/// Conservative cadence (10s) so the ticker itself costs ~nothing.
const IDLE_GC_TICK: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// Runtime — the public handle
// ---------------------------------------------------------------------------

/// Public handle to a V8 isolate + its async pump.
///
/// Cheap to clone; every clone points at the same underlying isolate. All
/// dispatch methods take `&self` and borrow the inner `RefCell` internally
/// — callers never see the `Rc<RefCell<_>>` layout, and hot-path accessors
/// (`limits`, `modules`) read the outer struct directly without borrowing.
///
/// Construct via `Runtime::builder()`.
#[derive(Clone)]
pub struct Runtime {
    inner: Rc<RefCell<RuntimeInner>>,
    limits: RuntimeLimits,
    modules: Rc<Vec<ModuleEntry>>,
    /// Multi-tenant identity. Set by the worker via `RuntimeBuilder::app_id`
    /// so per-isolate registries (RPC abort, future telemetry) can key
    /// entries by the same Uuid the cache uses. `None` for single-tenant
    /// callers (the bench server, the dev `serve` CLI, most tests).
    app_id: Option<uuid::Uuid>,
}

impl Runtime {
    /// Start building a new `Runtime`. See [`RuntimeBuilder`].
    pub fn builder() -> RuntimeBuilder {
        RuntimeBuilder::default()
    }

    // ---- Borrow-free hot-path accessors ---------------------------------

    /// Effective runtime limits (cpu/wall/heap). Immutable snapshot.
    pub fn limits(&self) -> RuntimeLimits {
        self.limits
    }

    /// Per-request wall-clock timeout, if configured.
    pub fn wall_timeout(&self) -> Option<Duration> {
        self.limits.wall_timeout
    }

    /// Per-request CPU time limit, if configured.
    pub fn cpu_limit(&self) -> Option<Duration> {
        self.limits.cpu_limit
    }

    /// Module list this runtime was built with.
    pub fn modules(&self) -> &[ModuleEntry] {
        self.modules.as_ref().as_slice()
    }

    /// Multi-tenant identity, if the builder was supplied one.
    /// Used by `crate::rpc::abort` to key the in-flight controller
    /// registry by `(app_id, request_id)`.
    pub fn app_id(&self) -> Option<uuid::Uuid> {
        self.app_id
    }

    /// Run `f` inside this isolate's HandleScope + Context. Wraps the
    /// same primitive `enter_v8!` uses but exposes it to callers that
    /// need to invoke V8 APIs from outside `call_fetch_handler`.
    ///
    /// Used by the worker's eviction path (see
    /// `crates/worker/src/cache.rs::evict_lru`) to walk the abort
    /// registry inside the about-to-be-disposed isolate's scope.
    ///
    /// The runtime borrows the inner `RefCell` for the duration of the
    /// call — callers must not invoke another method that re-borrows
    /// inside `f` (e.g. `call_fetch_handler` would reentrantly borrow_mut).
    pub fn with_scope<R>(&self, f: impl FnOnce(&mut v8::PinScope) -> R) -> R {
        let mut inner = self.inner.borrow_mut();
        // Multi-tenant workers exit isolates between dispatches so other
        // isolates can be entered on the same thread (`enter_depth == 0`).
        // V8's HandleScope macro requires `Isolate::GetCurrent()` to be
        // THIS isolate, so re-enter just-in-time and exit afterwards
        // when we found ourselves in the exited state. Already-entered
        // callers (e.g. inside `call_fetch_handler`) skip the toggle.
        let entered_for_scope = if inner.enter_depth == 0 {
            inner.enter_isolate();
            true
        } else {
            false
        };
        let context_global = inner.context.clone();
        let r = {
            v8::scope!(let handle_scope, &mut inner.isolate);
            let context = v8::Local::new(handle_scope, &context_global);
            let scope = &mut v8::ContextScope::new(handle_scope, context);
            let r = f(scope);
            scope.perform_microtask_checkpoint();
            r
        };
        if entered_for_scope {
            inner.exit_isolate();
        }
        r
    }

    /// Wake the pump task immediately. Callers use this after flipping a
    /// request's cancel flag so the pump runs `cleanup_cancelled_requests`
    /// on the next cycle instead of waiting for an unrelated event.
    pub fn notify_pump(&self) {
        self.inner.borrow().notify_pump();
    }

    // ---- Methods that enter V8 — borrow internally -----------------------

    /// Kernel's sole dispatch primitive. Invokes the user's
    /// `export default { fetch(request, env, ctx) }` handler and returns
    /// a `FetchOutcome` describing the response.
    ///
    /// `method` / `url` / `headers` / `body`: the HTTP request shape — matches
    ///   what the worker crate already packages from the incoming envelope.
    /// `env`: module-singleton env snapshot (JSON-serialized once at boot).
    /// `ctx`: per-request execution context (cancel flag + waitUntil bookkeeping).
    pub fn call_fetch_handler(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: &str,
        env: &crate::EnvSnapshot,
        ctx: crate::RequestCtx,
    ) -> crate::FetchOutcome {
        self.inner.borrow_mut().call_fetch_handler(
            self.modules.as_slice(),
            method,
            url,
            headers,
            body,
            env,
            ctx,
        )
    }

    /// Enter the V8 isolate on this thread. Multi-tenant workers that keep
    /// several isolates per thread MUST call `enter_isolate` before each
    /// `dispatch_*` and `exit_isolate` after. Single-isolate callers can
    /// ignore — `dispatch_*` tolerates nested enter/exit.
    pub fn enter_isolate(&self) {
        self.inner.borrow_mut().enter_isolate();
    }

    /// Exit the V8 isolate. See [`Runtime::enter_isolate`].
    pub fn exit_isolate(&self) {
        self.inner.borrow_mut().exit_isolate();
    }

    /// Deliver a WebSocket text/binary message into V8.
    pub fn enter_v8_for_ws_message(&self, ws_id: u32, data: &str) {
        self.inner.borrow_mut().enter_v8_for_ws_message(ws_id, data);
    }

    /// Deliver a WebSocket close into V8.
    pub fn enter_v8_for_ws_close(&self, ws_id: u32, code: u16, reason: &str) {
        self.inner.borrow_mut().enter_v8_for_ws_close(ws_id, code, reason);
    }

    /// Access the shared runtime state. The borrow is brief; callers must
    /// not hold it across `.await`.
    pub fn state(&self) -> SharedState {
        self.inner.borrow().state().clone()
    }

    /// Start the internal event-loop pump. Must be called once after
    /// `build()` — on a compio runtime thread — before dispatch is driven.
    ///
    /// Clones `self` internally; callers don't need to juggle `Rc<RefCell>`.
    pub fn start_pump(&self) {
        RuntimeInner::start_pump(self.inner.clone());
    }

    /// Number of times the per-isolate idle-GC ticker has fired
    /// `low_memory_notification`. Increments on every GC hint; useful as
    /// a test-visible signal (the alternative — sampling V8 heap stats
    /// before/after — is flaky on a small heap).
    pub fn idle_gc_fire_count(&self) -> u64 {
        self.inner.borrow().idle_gc_fire_count.get()
    }
}

// ---------------------------------------------------------------------------
// RuntimeBuilder
// ---------------------------------------------------------------------------

/// Builder for [`Runtime`]. All fields are optional.
#[derive(Default)]
pub struct RuntimeBuilder {
    modules: Vec<ModuleEntry>,
    env_vars: HashMap<String, String>,
    limits: RuntimeLimits,
    plugins: Vec<Arc<dyn NativePlugin>>,
    app_id: Option<uuid::Uuid>,
    /// Idle-GC threshold override (ms). `None` → `DEFAULT_IDLE_GC_AFTER`.
    /// Lives on the builder (not `RuntimeLimits`) because it's a runtime
    /// scheduling knob, not a per-request cap.
    idle_gc_after_ms: Option<u64>,
}

impl RuntimeBuilder {
    /// Set the module list. See [`Runtime::modules`].
    pub fn modules(mut self, m: Vec<ModuleEntry>) -> Self {
        self.modules = m;
        self
    }

    /// Set the per-app environment variables (exposed as `process.env.*`).
    pub fn env_vars(mut self, e: HashMap<String, String>) -> Self {
        self.env_vars = e;
        self
    }

    /// Set the full limits struct.
    pub fn limits(mut self, l: RuntimeLimits) -> Self {
        self.limits = l;
        self
    }

    /// Shorthand: set only the CPU limit.
    pub fn cpu_limit(mut self, d: Duration) -> Self {
        self.limits.cpu_limit = Some(d);
        self
    }

    /// Shorthand: set only the wall timeout.
    pub fn wall_timeout(mut self, d: Duration) -> Self {
        self.limits.wall_timeout = Some(d);
        self
    }

    /// Shorthand: set only the heap limit (bytes).
    pub fn heap_limit_bytes(mut self, n: usize) -> Self {
        self.limits.heap_limit_bytes = Some(n);
        self
    }

    /// Cap the V8 isolate's heap (megabytes). Default off — V8 grows
    /// past multi-GB before GC pressure kicks in. When set, V8 forces
    /// earlier GC and surfaces OOM rather than growing past the cap.
    ///
    /// Recommended floor is 32 MB; lower caps thrash on burst load.
    /// The control plane / worker config surface this as a per-app
    /// setting (see `docs/reference/runtime-limits.md`).
    pub fn heap_limit_mb(mut self, mb: u32) -> Self {
        self.limits.heap_limit_bytes = Some((mb as usize) * 1024 * 1024);
        self
    }

    /// Register one plugin. Two plugins sharing a namespace will panic at
    /// Runtime construction — see [`register_plugins`](crate::plugin).
    pub fn plugin<P: NativePlugin>(mut self, p: P) -> Self {
        self.plugins.push(Arc::new(p));
        self
    }

    /// Replace the plugin list wholesale.
    pub fn plugins(mut self, ps: Vec<Arc<dyn NativePlugin>>) -> Self {
        self.plugins = ps;
        self
    }

    /// Multi-tenant identity. The worker passes the same `Uuid` it uses
    /// to key the per-thread isolate cache, so eviction can fire all
    /// in-flight `AbortController`s for a single app via
    /// `crate::rpc::abort::entered_for_eviction`.
    pub fn app_id(mut self, id: uuid::Uuid) -> Self {
        self.app_id = Some(id);
        self
    }

    /// Idle-GC threshold in milliseconds. After this much quiet time the
    /// per-isolate ticker fires a low-memory hint so V8 reclaims the
    /// high-water-mark working set. Default `30000` (30s); see
    /// `docs/reference/runtime-limits.md` § "Idle GC".
    pub fn idle_gc_after_ms(mut self, ms: u64) -> Self {
        self.idle_gc_after_ms = Some(ms);
        self
    }

    /// Build the runtime. Panics on V8 init failure (same as the underlying
    /// `v8::Isolate::new` call — not newly fallible here).
    pub fn build(self) -> Runtime {
        let limits = self.limits;
        let modules_rc = Rc::new(self.modules);
        let app_id = self.app_id;
        let idle_gc_after = self
            .idle_gc_after_ms
            .map(Duration::from_millis)
            .unwrap_or(DEFAULT_IDLE_GC_AFTER);
        let inner = RuntimeInner::new_with_plugins(
            self.env_vars,
            limits.cpu_limit,
            limits.wall_timeout,
            limits.heap_limit_bytes,
            self.plugins,
            app_id,
            idle_gc_after,
        );
        Runtime {
            inner: Rc::new(RefCell::new(inner)),
            limits,
            modules: modules_rc,
            app_id,
        }
    }
}

// ---------------------------------------------------------------------------
// PendingRequest — tracking for in-flight async requests
// ---------------------------------------------------------------------------

/// Origin of a pending request's promise — drives how the pump
/// translates the resolved value into a wire response.
#[derive(Clone, Copy)]
enum PendingOrigin {
    /// Promise came from `default.fetch` — resolved value is a Response,
    /// inspected via `http::inspect_response`.
    Fetch,
    /// Promise came from `default.rpc` — resolved value is the user's
    /// return, classified via `classify_rpc_return` (envelope-wrapped,
    /// inspected if Response, fall-through if AsyncIterator).
    Rpc,
}

/// Tracking info for an in-flight request whose dispatch returned a Promise.
struct PendingRequest {
    #[allow(dead_code)]
    id: u64,
    promise: v8::Global<v8::Promise>,
    /// Reply slot for the `call_fetch_handler` pending path. Carries a
    /// `SettledFetch` mirroring `FetchOutcome`'s three non-Pending variants.
    reply_fetch: ResultSender<Result<crate::SettledFetch, DispatchError>>,
    cpu_accumulated: Duration,
    wall_start: Instant,
    cancel: CancelFlag,
    origin: PendingOrigin,
    /// Keeps the per-request `AbortController` registered with
    /// `crate::rpc::abort` until the promise settles. Drop unregisters
    /// (covers normal settle, cancellation sweep, and pump-side
    /// timeout / CPU termination removals). `None` for non-RPC paths
    /// and for runtimes built without an `app_id`.
    #[allow(dead_code)]
    abort_guard: Option<crate::rpc::abort::AbortGuard>,
}

// ---------------------------------------------------------------------------
// enter_v8! macro
// ---------------------------------------------------------------------------

/// Enter V8 with a pinned scope, execute a block, then run a microtask checkpoint.
macro_rules! enter_v8 {
    ($this:expr, |$scope:ident| $body:expr) => {{
        v8::scope!(let handle_scope, &mut $this.isolate);
        let context = v8::Local::new(handle_scope, &$this.context);
        let $scope = &mut v8::ContextScope::new(handle_scope, context);
        let __result = { $body };
        $scope.perform_microtask_checkpoint();
        __result
    }};
}

// ---------------------------------------------------------------------------
// Runtime
// ---------------------------------------------------------------------------

/// A V8 isolate driven by a compio event loop.
///
/// Owns the isolate and all associated state. Must be run on a single thread
/// because V8 types are `!Send`. This is the inner type — the public handle
/// is [`Runtime`], which wraps `Rc<RefCell<RuntimeInner>>`.
pub(crate) struct RuntimeInner {
    pub(crate) isolate: v8::OwnedIsolate,
    pub(crate) context: v8::Global<v8::Context>,
    /// Cached reference to `module.default.fetch`, resolved once at module
    /// init. None if the module doesn't export a default.fetch handler.
    /// Slow path — runs when the request isn't claimed by `rpc_fn` (the
    /// RPC fast path) or `fetch_fast_fn` (the non-WinterCG HTTP fast
    /// path), or when those return a fall-through marker (null /
    /// AsyncIterator / Response).
    pub(crate) fetch_handler_fn: Option<v8::Global<v8::Function>>,
    /// Cached reference to `module.default.fetchFast` — the zeroship
    /// extension for bypassing the WinterCG Request/Response contract.
    /// Signature: `fetchFast(method, url, body, env) → object | string | null`.
    /// When non-null result: `{ status, headers, body }` plain object OR
    /// a string body (200 OK). When null: kernel falls through to the
    /// full `default.fetch(request, env, ctx)` path.
    pub(crate) fetch_fast_fn: Option<v8::Global<v8::Function>>,
    /// Cached reference to `module.default.rpc` — the RPC dispatcher.
    /// Signature: `rpc(name, input, ctx) → any | Promise<any> | AsyncIterator<any>`.
    /// When set AND the incoming URL matches `/_zs/v1/<id>` (POST or GET),
    /// the kernel slices the id, parses the body's superjson `{ json }`
    /// envelope in V8, and calls `rpc(id, input, ctx)` directly —
    /// bypassing Request construction, URL parsing, async body read,
    /// and Response wrap. Sync/async return values are envelope-wrapped
    /// (`{"json":<result>}`); AsyncIterator returns and Response objects
    /// fall through to the slow path which encodes them.
    pub(crate) rpc_fn: Option<v8::Global<v8::Function>>,
    /// Cached JS helper that constructs a Request from Rust-supplied params.
    http_create_request_fn: Option<v8::Global<v8::Function>>,
    pub(crate) initialized: bool,
    pub(crate) state: SharedState,
    /// Plugins registered on the runtime at boot.
    plugins: Vec<Arc<dyn NativePlugin>>,

    pending_requests: HashMap<u64, PendingRequest>,
    next_direct_request_id: u64,

    /// Notification channel to wake the pump task when new work is added.
    /// dispatch_start sends a signal here after spawning timers/ops so the
    /// pump doesn't have to poll on a 1ms sleep.
    pump_notify_tx: Option<futures::channel::mpsc::Sender<()>>,

    /// Optional per-request CPU time limit.
    cpu_limit: Option<Duration>,
    /// Optional per-request wall time limit. The outer `Runtime.limits`
    /// exposes this to external callers; the inner copy stays for
    /// symmetry with `cpu_limit` and for future dispatch-internal uses.
    #[allow(dead_code)]
    wall_timeout: Option<Duration>,

    /// POSIX CPU timer — kills V8 on CPU limit exceeded (Linux only).
    #[cfg(target_os = "linux")]
    cpu_timer: Option<crate::cpu_timer::CpuTimer>,
    /// Whether the CPU timer is currently armed.
    #[cfg(target_os = "linux")]
    cpu_timer_active: bool,

    /// Cumulative CPU time consumed by this Runtime's async pump work
    /// (op resolves, timer callbacks, stream pushes) since the last budget
    /// window reset. Compared against wall time to detect apps that
    /// monopolize the thread via long-running `setInterval` callbacks or
    /// promise chains — situations the per-REQUEST cpu timer doesn't catch
    /// because the work isn't attributed to any single request.
    pump_cpu_accumulated: Duration,
    /// Wall-clock start of the current budget window.
    pump_wall_start: Instant,

    /// Captured error message if `ensure_initialized` failed to load the
    /// user's module graph (e.g. parse error, evaluation throw). Surfaced
    /// to callers via the dispatch error path so a syntactically-broken
    /// deploy doesn't masquerade as "No default.fetch handler exported".
    init_error: Option<String>,

    /// Multi-tenant identity. When `Some`, the RPC fast-path registers
    /// every in-flight `AbortController` with `crate::rpc::abort` keyed
    /// by `(app_id, request_id)` so the worker's eviction sweep can
    /// fire them. `None` for single-tenant callers.
    app_id: Option<uuid::Uuid>,

    /// Net depth of `enter_isolate`/`exit_isolate` pairs. Tracks whether
    /// the V8 isolate is currently the topmost-entered on its thread.
    ///
    /// `v8::Isolate::new()` already enters the isolate (so depth starts
    /// at 1). Multi-tenant workers that hold N isolates per thread call
    /// `exit_isolate` after `build()` to drop back to depth=0, allowing
    /// other isolates to be entered for their own work.
    ///
    /// At drop time, `v8::OwnedIsolate::Drop` asserts the dropping
    /// isolate is `Isolate::GetCurrent()`. Our `Drop` impl uses this
    /// counter to re-enter the isolate just-in-time if it was sitting
    /// in the cache's exited state.
    enter_depth: u32,

    /// Wall-clock timestamp of the most recent request activity — both
    /// `call_fetch_handler` entry AND every settled async event. The
    /// idle-GC ticker compares `now() - last_request_ts` against
    /// `idle_gc_after` to decide whether to fire a GC hint. `Cell`
    /// because the field is mutated through `&self` accessors and
    /// `Instant: Copy`.
    last_request_ts: Cell<Instant>,
    /// Threshold (configurable via `RuntimeBuilder::idle_gc_after_ms`).
    /// `0` disables the ticker entirely.
    idle_gc_after: Duration,
    /// Counter incremented each time the idle-GC ticker fires the GC
    /// hint. Test-visible signal so the test suite can assert the
    /// ticker actually ran without sampling V8 heap statistics.
    idle_gc_fire_count: Cell<u64>,
}

// `RuntimeInner` is intentionally *not* `Send`.
//
// It owns a `v8::OwnedIsolate` (thread-bound) plus `Rc`/`RefCell` state that
// `SharedState` shares with V8 callbacks. Nothing in the current codebase
// attempts to move a `RuntimeInner` across threads — workers create one per
// ntex thread, the pump runs on the same thread, and the public `Runtime`
// handle only ever wraps `Rc<RefCell<RuntimeInner>>` (itself `!Send`).
//
// A previous revision carried `unsafe impl Send for Runtime {}` "for API
// compat". That impl was unused and only weakened the type system's ability
// to catch a future accidental cross-thread move, so it has been removed.
// If a new executor ever needs `Send`, switch to channel-based ownership
// transfer instead of re-adding this impl.

impl Drop for RuntimeInner {
    /// Make `OwnedIsolate::Drop`'s `current == self` assertion pass even
    /// when the isolate was sitting in the worker's per-thread cache in
    /// the exited state. We track depth in `enter_depth`; if it's zero we
    /// re-enter just-in-time so the subsequent field drop (which runs
    /// `OwnedIsolate::Drop`) sees this isolate as the topmost-entered.
    ///
    /// Without this, dropping N cached isolates in HashMap-iteration
    /// order would panic on the very first one because none of them is
    /// currently entered.
    fn drop(&mut self) {
        if self.enter_depth == 0 {
            // SAFETY: pushes `self` onto V8's per-thread isolate stack.
            // The OwnedIsolate field drop, which runs immediately after
            // this method returns, pops it. No other isolate may be
            // entered on this thread between this enter and the field
            // drop — the caller (typically a HashMap drop in the cache)
            // is single-threaded and synchronous, so that holds.
            unsafe { self.isolate.enter(); }
            self.enter_depth = 1;
        }
    }
}

impl RuntimeInner {
    /// Create a new runtime with plugins.
    /// Plugins register native functions on `zeroship.{namespace}.*`.
    fn new_with_plugins(
        env_vars: HashMap<String, String>,
        cpu_limit: Option<Duration>,
        wall_timeout: Option<Duration>,
        heap_limit_bytes: Option<usize>,
        plugins: Vec<Arc<dyn NativePlugin>>,
        app_id: Option<uuid::Uuid>,
        idle_gc_after: Duration,
    ) -> Self {
        init_v8();

        // Default 128 MB per isolate. Control-plane can tune per-app:
        // free-tier → 64 MB, paid → 256 MB. The old hardcoded 512 MB
        // meant MAX_ISOLATES=200 × 512 MB × threads could claim 100+ GB.
        const DEFAULT_HEAP: usize = 128 * 1024 * 1024;
        let heap_max = heap_limit_bytes.unwrap_or(DEFAULT_HEAP);
        let params = v8::CreateParams::default().heap_limits(0, heap_max);
        let mut isolate = v8::Isolate::new(params);

        // Register near-heap-limit callback. V8 invokes this when the
        // configured heap cap is approached. The callback's return
        // value is the *new* limit V8 should use:
        //   - Equal to `current_heap_limit` → V8 GCs and may fatal-
        //     abort the process if it still can't satisfy.
        //   - Greater than `current_heap_limit` → V8 grows and lets
        //     allocation succeed; useful as a one-off escape valve.
        //
        // Strategy: on every hit grow the cap modestly (avoids a hard
        // fatal-abort and lets the in-flight allocation surface as a
        // catchable JS RangeError on the very next allocation that
        // doesn't fit). After MAX_HEAP_LIMIT_HITS consecutive hits we
        // also call `Isolate::terminate_execution`, which fires on the
        // next interrupt check — guaranteeing the runaway handler
        // can't pin RSS at the cap forever.
        //
        // Data block is heap-allocated and intentionally leaked: one
        // allocation per Runtime, lifetime is the isolate's. The block
        // also carries the IsolateHandle so the callback can fire
        // termination from any thread (V8 contract: `terminate_execution`
        // is thread-safe).
        struct HeapLimitData {
            hits: u32,
            handle: v8::IsolateHandle,
            initial_limit: usize,
        }
        const MAX_HEAP_LIMIT_HITS: u32 = 5;
        let heap_data = Box::into_raw(Box::new(HeapLimitData {
            hits: 0,
            handle: isolate.thread_safe_handle(),
            initial_limit: heap_max,
        }));

        unsafe extern "C" fn near_heap_limit_callback(
            data: *mut std::ffi::c_void,
            current_heap_limit: usize,
            _initial_heap_limit: usize,
        ) -> usize {
            if data.is_null() {
                return current_heap_limit;
            }
            // SAFETY: `data` was set via `Box::into_raw(Box::new(...))`
            // below and is never freed during the isolate's lifetime.
            // V8 invokes this callback only from the isolate's owning
            // thread per `Isolate::add_near_heap_limit_callback` contract,
            // so we have exclusive access here.
            let d = unsafe { &mut *(data as *mut HeapLimitData) };
            d.hits += 1;
            if d.hits >= MAX_HEAP_LIMIT_HITS {
                tracing::error!(
                    heap_limit_mb = current_heap_limit / 1024 / 1024,
                    hits = d.hits,
                    "v8 heap limit hit threshold; terminating isolate"
                );
                // Fire termination — V8 checks the flag on the next
                // interrupt boundary, surfacing as a catchable
                // `RangeError` to JS or a terminated-state to the
                // dispatch loop.
                d.handle.terminate_execution();
            } else {
                tracing::warn!(
                    heap_limit_mb = current_heap_limit / 1024 / 1024,
                    hits = d.hits,
                    max_hits = MAX_HEAP_LIMIT_HITS,
                    "v8 near heap limit"
                );
            }
            // Grow modestly so V8 doesn't fatal-abort the process while
            // the JS exception / termination flag propagates. Cap the
            // expansion at 4× the original limit so a wedged isolate
            // can't claim unbounded RSS before the eviction sweep kills
            // it.
            let max_grow = d.initial_limit.saturating_mul(4);
            current_heap_limit.saturating_add(d.initial_limit / 4).min(max_grow)
        }
        isolate.add_near_heap_limit_callback(
            near_heap_limit_callback,
            heap_data as *mut std::ffi::c_void,
        );

        // `await import(spec)` — resolves through the per-isolate module
        // registry slot installed by `load_modules`. Bundle-resident
        // only: unknown specifiers reject with TypeError. Hooked here
        // (before any user JS) so the very first dynamic import goes
        // through this path.
        isolate.set_host_import_module_dynamically_callback(
            crate::core::dynamic_import::host_import_module_dynamically_callback,
        );

        // Create RuntimeState (no server_handle -- compio, not tokio)
        let state: SharedState = Rc::new(RefCell::new(RuntimeState::new(env_vars, None)));
        isolate.set_slot(state.clone());

        let context = {
            v8::scope!(let handle_scope, &mut isolate);
            let ctx = v8::Context::new(handle_scope, Default::default());
            v8::Global::new(handle_scope, ctx)
        };

        Self {
            isolate,
            context,
            fetch_handler_fn: None,
            fetch_fast_fn: None,
            rpc_fn: None,
            http_create_request_fn: None,
            initialized: false,
            state,
            plugins,
            pending_requests: HashMap::new(),
            next_direct_request_id: 1,
            pump_notify_tx: None,
            cpu_limit,
            wall_timeout,
            #[cfg(target_os = "linux")]
            cpu_timer: None,
            #[cfg(target_os = "linux")]
            cpu_timer_active: false,

            pump_cpu_accumulated: Duration::ZERO,
            pump_wall_start: Instant::now(),
            init_error: None,
            app_id,
            // `Isolate::new()` enters the isolate, so we boot with depth 1.
            enter_depth: 1,
            last_request_ts: Cell::new(Instant::now()),
            idle_gc_after,
            idle_gc_fire_count: Cell::new(0),
        }
    }

    /// Exit the V8 isolate so another isolate can be entered on this thread.
    /// Must be called after `Runtime::builder().build()` when storing
    /// multiple runtimes on one thread.
    /// # Safety
    /// The isolate must not be used between `exit_isolate` and `enter_isolate`.
    pub fn exit_isolate(&mut self) {
        unsafe { self.isolate.exit(); }
        debug_assert!(self.enter_depth > 0, "exit_isolate without matching enter");
        self.enter_depth = self.enter_depth.saturating_sub(1);
    }

    /// Enter the V8 isolate before dispatching requests.
    /// Must be paired with `exit_isolate` after dispatch is done.
    /// # Safety
    /// Only one isolate can be entered at a time per thread.
    pub fn enter_isolate(&mut self) {
        unsafe { self.isolate.enter(); }
        self.enter_depth = self.enter_depth.saturating_add(1);
    }

    /// Set the pump notification sender. The pump task holds the receiver.
    pub fn set_pump_notify(&mut self, tx: futures::channel::mpsc::Sender<()>) {
        // Also stash a clone on SharedState so detached tasks (streaming
        // fetch body readers) can wake the pump directly without holding a
        // reference back to Runtime.
        self.state.borrow_mut().pump_notify_tx = Some(tx.clone());
        self.pump_notify_tx = Some(tx);
    }

    /// Wake the pump task so it can drain newly added work.
    pub fn notify_pump(&self) {
        if let Some(tx) = &self.pump_notify_tx {
            let _ = tx.clone().try_send(());
        }
    }

    /// Start the internal event loop pump.  Spawns a compio task that
    /// processes async V8 operations (timers, fetch, streams) until the
    /// runtime is dropped.
    ///
    /// Must be called **after** the isolate is wrapped in `Rc<RefCell<>>`
    /// (handled by `Runtime::build`). Prefer calling `Runtime::start_pump`
    /// on the public handle — it hides the `Rc<RefCell<_>>` plumbing.
    pub(crate) fn start_pump(self_ref: Rc<RefCell<Self>>) {
        let (notify_tx, notify_rx) = futures::channel::mpsc::channel::<()>(1);
        let idle_gc_after = {
            let mut rt = self_ref.borrow_mut();
            rt.set_pump_notify(notify_tx);
            rt.idle_gc_after
        };

        let rt = self_ref.clone();
        compio::runtime::spawn(async move {
            crate::panic_util::guard("pump_loop", async move {
                Self::pump_loop(rt, notify_rx).await;
            }).await;
        })
        .detach();

        // Idle-GC ticker — sibling task with a Weak handle so isolate
        // teardown drops it without a join. `idle_gc_after == 0` opts
        // out (used by tests that don't want the timer at all).
        if !idle_gc_after.is_zero() {
            let weak = Rc::downgrade(&self_ref);
            compio::runtime::spawn(async move {
                crate::panic_util::guard("idle_gc_ticker", async move {
                    Self::idle_gc_ticker(weak, idle_gc_after).await;
                }).await;
            })
            .detach();
        }
    }

    /// Per-isolate idle-GC ticker. Wakes on `IDLE_GC_TICK` cadence; when
    /// `now() - last_request_ts >= idle_gc_after`, enters V8 and fires
    /// `low_memory_notification` (a full-GC hint — the v8-147 binding
    /// doesn't expose `idle_notification_deadline`, so this is the
    /// closest equivalent. See `docs/reference/runtime-limits.md`).
    ///
    /// Holds a `Weak`; once the runtime drops, `upgrade()` returns None
    /// and the loop exits naturally.
    async fn idle_gc_ticker(weak: Weak<RefCell<Self>>, idle_gc_after: Duration) {
        // Tick at min(IDLE_GC_TICK, idle_gc_after) so very short test
        // thresholds (e.g. 100 ms) still get a tick within the window.
        let period = IDLE_GC_TICK.min(idle_gc_after);
        let mut interval = compio::time::interval(period);
        loop {
            interval.tick().await;
            let Some(rt_rc) = weak.upgrade() else { return; };

            // Brief borrow to read last-activity. Released before
            // entering V8 — the pump may be holding the cell.
            let elapsed = {
                let rt = rt_rc.borrow();
                rt.last_request_ts.get().elapsed()
            };
            if elapsed < idle_gc_after {
                drop(rt_rc);
                continue;
            }

            // Borrow mut to drive V8 (enter / GC hint / exit). If the
            // pump has the lock right now, skip this tick — the next
            // tick (period later) will retry and the runtime is by
            // definition not idle anyway.
            let Ok(mut rt) = rt_rc.try_borrow_mut() else {
                drop(rt_rc);
                continue;
            };
            rt.enter_isolate();
            // `low_memory_notification` triggers a full GC synchronously
            // — V8's only exposed "free memory now" hook in this binding.
            rt.isolate.low_memory_notification();
            rt.exit_isolate();
            rt.idle_gc_fire_count.set(rt.idle_gc_fire_count.get() + 1);
            tracing::trace!(
                fires = rt.idle_gc_fire_count.get(),
                idle_ms = elapsed.as_millis() as u64,
                "idle GC fired",
            );
            // Re-arm the clock so we don't refire on the next tick if
            // no requests came in (idle_gc_after may be < period).
            rt.last_request_ts.set(Instant::now());
        }
    }

    /// The pump loop — drives `AsyncWork` (fetch, timers, streams) on the
    /// current compio thread.  Enters/exits the V8 isolate around every V8
    /// interaction so multi-isolate-per-thread setups (the worker) work
    /// correctly.  For single-isolate use (benchmark server) the extra
    /// enter/exit is a harmless nested push/pop.
    async fn pump_loop(
        runtime: Rc<RefCell<Self>>,
        mut notify_rx: futures::channel::mpsc::Receiver<()>,
    ) {
        use futures::{FutureExt, StreamExt};
        let mut work = AsyncWork::new();

        loop {
            // PHASE 1 — drain new spawned ops/timers/fetches + flush outbound
            // streams. Only enter the V8 isolate if there's actually work to
            // do: v8::Isolate::enter/exit aren't free (TLS swap + scheduling
            // slot manipulation), and in steady-state "await an op, handle
            // it, await another" the drain phase finds nothing new. Checking
            // the shared-state sizes behind a short immutable borrow lets us
            // skip this entire block when it would be a no-op.
            let needs_drain = {
                let rt = runtime.borrow();
                let s = rt.state().borrow();
                !s.spawned_ops.is_empty()
                    || !s.spawned_timers.is_empty()
                    || !s.ready_timers.is_empty()
            };

            if needs_drain {
                let mut rt = runtime.borrow_mut();
                rt.enter_isolate();
                rt.drain_new_tasks_into(&mut work);
                rt.exit_isolate();
            }

            let event = {
                let has_ops = !work.pending_ops.is_empty();
                let has_timers = !work.pending_timers.is_empty();

                match (has_ops, has_timers) {
                    (true, true) => {
                        futures::select! {
                            r = work.pending_ops.select_next_some() => Some(AsyncEvent::Op(r)),
                            r = work.pending_timers.select_next_some() => Some(AsyncEvent::Timer(r)),
                            _ = notify_rx.next() => None,
                        }
                    }
                    (true, false) => {
                        futures::select! {
                            r = work.pending_ops.select_next_some() => Some(AsyncEvent::Op(r)),
                            _ = notify_rx.next() => None,
                        }
                    }
                    (false, true) => {
                        futures::select! {
                            r = work.pending_timers.select_next_some() => Some(AsyncEvent::Timer(r)),
                            _ = notify_rx.next() => None,
                        }
                    }
                    (false, false) => {
                        let _ = notify_rx.next().await;
                        None
                    }
                }
            };

            if let Some(first_event) = event {
                // ----------------------------------------------------------
                // Event batching — the key latency improvement.
                //
                // Before: each pump iteration handled exactly one event,
                // each requiring its own borrow_mut + enter_isolate +
                // microtask_checkpoint + collect_settled_promises +
                // exit_isolate. When 10 fetch completions arrived in a
                // burst, that was 10 borrow cycles, each blocking every
                // handler on this thread for the full V8 turn.
                //
                // Now: after the first event fires, we greedily drain
                // every OTHER ready event from pending_ops/timers (via
                // `now_or_never()` — non-blocking), then enter V8 ONCE
                // to process the entire batch. One microtask checkpoint
                // covers all resolved promises, one collect_settled scan,
                // one borrow window.
                //
                // Net effect: borrow-hold time changes from
                //   O(burst_size × per_event_cost)
                // to
                //   O(burst_size + per_event_cost)
                //
                // For 10 concurrent fetch completions on a thread with
                // 125 queued requests (the c=2000 scenario), this alone
                // should cut p99.9 by roughly an order of magnitude.
                // ----------------------------------------------------------
                let mut batch = vec![first_event];

                // Drain any OTHER events that are already resolved. This
                // is cheap — FuturesUnordered::poll_next returns Poll::Ready
                // for items whose futures have already completed (streaming
                // fetch chunks, zero-delay timers, etc.). We stop as soon
                // as it returns Pending.
                loop {
                    // Try ops first (most common under load)
                    if let Some(Some(r)) = work.pending_ops.next().now_or_never() {
                        batch.push(AsyncEvent::Op(r));
                        continue;
                    }
                    // Then timers
                    if let Some(Some(r)) = work.pending_timers.next().now_or_never() {
                        batch.push(AsyncEvent::Timer(r));
                        continue;
                    }
                    break;
                }

                {
                    let v8_start = Instant::now();
                    let mut rt = runtime.borrow_mut();
                    rt.enter_isolate();
                    for ev in batch {
                        rt.handle_async_event(ev, &mut work);
                    }
                    rt.exit_isolate();

                    // Per-app pump CPU budget: if this app's async
                    // continuations (timer callbacks, microtask chains)
                    // consume >80% of wall time over a 10 s window,
                    // terminate the isolate. The per-request CPU timer
                    // doesn't catch pump-side work — this does.
                    if rt.record_pump_cpu(v8_start.elapsed()) {
                        // Isolate is terminated — all pending requests
                        // will get "CPU limit exceeded" on the next
                        // check_v8_terminated call. Break out of the pump
                        // loop; the Runtime will be dropped by cache
                        // eviction or process shutdown.
                        break;
                    }
                }

                // Yield to the compio scheduler so handler tasks that are
                // waiting on borrow_mut() get a chance to run before we
                // loop back and potentially grab the borrow again for the
                // next batch. compio doesn't expose a `yield_now()`, so a
                // zero-duration sleep serves the same purpose: it posts a
                // completion that fires on the next io_uring cycle, giving
                // ready handlers a scheduling slot.
                //
                // Skip the yield when there's no outstanding work: with empty
                // pending_ops + pending_timers, the next loop iteration will
                // immediately await `notify_rx.next()`, which naturally yields
                // to the scheduler. Posting an extra io_uring completion just
                // to repeat that wait is pure overhead — measurable in the
                // ping/pong hot path where every request re-enters this loop.
                let should_yield = !work.pending_ops.is_empty()
                    || !work.pending_timers.is_empty();
                if should_yield {
                    compio::time::sleep(Duration::ZERO).await;
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Initialization
    // -----------------------------------------------------------------------

    /// Load polyfills and ES modules, then resolve `default.fetch` (once).
    pub(crate) fn ensure_initialized(&mut self, modules: &[ModuleEntry]) {
        if self.initialized {
            return;
        }

        {
            v8::scope!(let handle_scope, &mut self.isolate);
            let context = v8::Local::new(handle_scope, &self.context);
            let scope = &mut v8::ContextScope::new(handle_scope, context);

            // Build the composite env object FIRST — plugin namespaces
            // overlaid on the scalar env JSON snapshot. The `zeroship`
            // module's top-level `const env = Object.freeze(__zs_env())`
            // captures this during module load; if the cache wasn't
            // populated by then, the import would see only the scalar JSON
            // and plugin namespaces would be invisible on `import { env }`.
            //
            // Cache on SharedState (not RuntimeInner) so the `__zs_env`
            // callback — a free function with only scope-slot access — can
            // retrieve the same V8 Global.
            if self.state.borrow().env_obj.is_none() {
                let env_json = self.state.borrow().env_json.clone();
                let global = crate::plugin::build_env_object(scope, &self.plugins, &env_json);
                self.state.borrow_mut().env_obj = Some(global);
            }

            // Load polyfills and the user's entry module. The returned global
            // is the entry module's Namespace Object; the kernel reads
            // `default.fetch` directly off it (no more `__rpc` reach-through).
            let namespace = match load_polyfills_and_modules(scope, modules, &self.plugins) {
                Ok(ns) => Some(ns),
                Err(e) => {
                    self.init_error = Some(e);
                    None
                }
            };

            // Resolve `export default { fetch(...) }` on the entry module's
            // namespace. This is the sole dispatch target of the new kernel:
            // `call_fetch_handler` invokes this cached function for every
            // incoming request, passing `(Request, env, ctx)` just like the
            // Cloudflare Workers / Bun / WinterCG module-worker contract.
            if let Some(ns_global) = namespace {
                let ns_local = v8::Local::new(scope, &ns_global);
                if let Some(ns_obj) = ns_local.to_object(scope) {
                    let default_key = v8::String::new(scope, "default").unwrap();
                    if let Some(default_val) = ns_obj.get(scope, default_key.into()) {
                        if !default_val.is_undefined() && !default_val.is_null() {
                            if let Some(default_obj) = default_val.to_object(scope) {
                                let fetch_key = v8::String::new(scope, "fetch").unwrap();
                                if let Some(fetch_val) =
                                    default_obj.get(scope, fetch_key.into())
                                {
                                    if fetch_val.is_function() {
                                        let func =
                                            v8::Local::<v8::Function>::try_from(fetch_val)
                                                .unwrap();
                                        self.fetch_handler_fn =
                                            Some(v8::Global::new(scope, func));
                                    }
                                }
                                // Zeroship extension: cache default.fetchFast
                                // for the non-RPC HTTP fast-path. Null when
                                // the user module doesn't opt into the
                                // extension.
                                let ff_key = v8::String::new(scope, "fetchFast").unwrap();
                                if let Some(ff_val) =
                                    default_obj.get(scope, ff_key.into())
                                {
                                    if ff_val.is_function() {
                                        let func =
                                            v8::Local::<v8::Function>::try_from(ff_val)
                                                .unwrap();
                                        self.fetch_fast_fn =
                                            Some(v8::Global::new(scope, func));
                                    }
                                }
                                // RPC standalone entry point: cache
                                // default.rpc for the kernel-side RPC
                                // fast path. When set, /_zs/v1/<id>
                                // requests skip Request construction
                                // and call rpc(id, input, ctx) directly.
                                let rpc_key = v8::String::new(scope, "rpc").unwrap();
                                if let Some(rpc_val) =
                                    default_obj.get(scope, rpc_key.into())
                                {
                                    if rpc_val.is_function() {
                                        let func =
                                            v8::Local::<v8::Function>::try_from(rpc_val)
                                                .unwrap();
                                        self.rpc_fn =
                                            Some(v8::Global::new(scope, func));
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // Compile a small JS helper that constructs a Request from Rust-supplied params.
            let helper_src = v8::String::new(scope, HTTP_CREATE_REQUEST_JS).unwrap();
            if let Some(script) = v8::Script::compile(scope, helper_src, None) {
                if let Some(val) = script.run(scope) {
                    if let Ok(func) = v8::Local::<v8::Function>::try_from(val) {
                        self.http_create_request_fn = Some(v8::Global::new(scope, func));
                    }
                }
            }

            // Build the shared `ctx` object once. Frozen so the user's
            // fetch handler can't mutate our callbacks; reused every
            // request. Eliminates the per-request Object::new + 2
            // Function::new + 2 Object::Set that showed up as ~5% of
            // fetch-path CPU in perf.
            if self.state.borrow().ctx_obj.is_none() {
                let obj = v8::Object::new(scope);

                let wu_key = v8::String::new(scope, "waitUntil").unwrap();
                let wu_fn = v8::Function::new(scope, wait_until_noop_callback).unwrap();
                obj.set(scope, wu_key.into(), wu_fn.into());

                let pt_key = v8::String::new(scope, "passThroughOnException").unwrap();
                let pt_fn = v8::Function::new(scope, pass_through_on_exception_noop_callback).unwrap();
                obj.set(scope, pt_key.into(), pt_fn.into());

                // Freeze via Object.freeze to prevent user code from
                // pointing our callbacks at their own impls (which
                // would be a security hazard + a cache-invalidation
                // nightmare across concurrent requests on this
                // worker).
                let freeze_src = v8::String::new(scope, "Object.freeze").unwrap();
                if let Some(freeze_fn_val) = scope
                    .get_current_context()
                    .global(scope)
                    .get(scope, v8::String::new(scope, "Object").unwrap().into())
                    .and_then(|o| o.to_object(scope))
                    .and_then(|o| o.get(scope, v8::String::new(scope, "freeze").unwrap().into()))
                    && let Ok(freeze_fn) = v8::Local::<v8::Function>::try_from(freeze_fn_val)
                {
                    let undefined = v8::undefined(scope).into();
                    let _ = freeze_fn.call(scope, undefined, &[obj.into()]);
                }
                let _ = freeze_src; // silence unused

                self.state.borrow_mut().ctx_obj = Some(v8::Global::new(scope, obj));
            }
        }

        self.initialized = true;

        // Create POSIX CPU timer if cpu_limit is configured (Linux only).
        //
        // Each Runtime must register with a UNIQUE app_id so the watchdog
        // terminates the right isolate. An earlier revision hardcoded
        // `app_id = 0` which meant every Runtime on the same thread
        // overwrote the previous one's handle in the watchdog map — if
        // App A's timer fired, the watchdog killed whichever app
        // registered LAST (likely B), not A.
        //
        // We use the isolate's raw pointer as a unique key. It's stable
        // for the lifetime of the Runtime and unique per thread.
        #[cfg(target_os = "linux")]
        if self.cpu_limit.is_some() {
            let system = crate::cpu_timer::CpuTimerSystem::get_or_init();
            let isolate_id = std::ptr::addr_of!(self.isolate) as u64;
            let v8_handle = self.isolate.thread_safe_handle();
            system.register(isolate_id, v8_handle);
            match crate::cpu_timer::CpuTimer::new(isolate_id) {
                Ok(timer) => self.cpu_timer = Some(timer),
                Err(e) => tracing::error!(error = %e, "cpu-timer initialisation failed"),
            }
        }
    }

    // -----------------------------------------------------------------------
    // CPU timer arm/disarm
    // -----------------------------------------------------------------------

    fn arm_cpu_timer(&mut self) {
        #[cfg(target_os = "linux")]
        if !self.cpu_timer_active {
            if let (Some(timer), Some(limit)) = (&self.cpu_timer, self.cpu_limit) {
                timer.arm(limit);
                self.cpu_timer_active = true;
            }
        }
    }

    fn disarm_cpu_timer(&mut self) {
        #[cfg(target_os = "linux")]
        if self.cpu_timer_active {
            if let Some(timer) = &self.cpu_timer {
                timer.disarm();
            }
            self.cpu_timer_active = false;
        }
    }

    /// Check if V8 was terminated by the CPU timer. If so, cancel termination,
    /// disarm the timer, and drain all pending requests with an error.
    /// Returns `true` if termination was detected.
    /// Check if V8 was terminated by the CPU timer. If so, cancel the
    /// termination so the isolate can continue serving other requests.
    /// Returns true if termination was detected.
    ///
    /// Does NOT drain pending requests — the caller decides which request
    /// to error (only the one that was executing when the timer fired).
    ///
    /// Fast path: when `cpu_limit` is None, no CPU timer exists, so V8 can
    /// never be terminated by us. Skipping the isolate state read saves a
    /// vdso syscall on every dispatch in the common (no-limit) case — this
    /// is the benchmark configuration and also the default for many deploys.
    fn check_v8_terminated(&mut self) -> bool {
        // If no timer is configured, V8 cannot have been terminated by us.
        // (Other code paths never call terminate_execution.)
        if self.cpu_timer.is_none() {
            return false;
        }
        if !self.isolate.is_execution_terminating() {
            return false;
        }
        self.isolate.cancel_terminate_execution();
        self.disarm_cpu_timer();
        true
    }

    // -----------------------------------------------------------------------
    // Direct dispatch (channel-free mode)
    // -----------------------------------------------------------------------

    // -----------------------------------------------------------------------
    // Kernel dispatch primitive — call_fetch_handler
    // -----------------------------------------------------------------------

    /// Kernel's sole HTTP dispatch primitive. Three tiers, in order:
    ///   1. `default.rpc(name, input, ctx)` when set + URL matches
    ///      `/_zs/v1/<id>` and the request isn't a WS upgrade.
    ///   2. `default.fetchFast(method, url, body, env)` when set.
    ///   3. `default.fetch(request, env, ctx)` (WinterCG slow path).
    ///
    /// Tiers 1 and 2 can fall through to (3) by returning a sentinel
    /// (AsyncIterator from rpc, `null` from fetchFast). Pending Promises
    /// from any tier hand off to the pump. The result is classified as
    /// a `FetchOutcome`.
    pub fn call_fetch_handler(
        &mut self,
        modules: &[crate::ModuleEntry],
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: &str,
        env: &crate::EnvSnapshot,
        ctx: crate::RequestCtx,
    ) -> crate::FetchOutcome {
        // Reset the idle-GC clock — every request entry is "activity".
        self.last_request_ts.set(Instant::now());

        // Stash the env JSON on state so the very first `ensure_initialized`
        // builds the composite env object (plugin namespaces + scalar JSON)
        // with the real scalars instead of the default `{}`. Must run BEFORE
        // `ensure_initialized` — that call is where `build_env_object` reads
        // `env_json`, caches the result on `state.env_obj`, and where the
        // `zeroship` module's `const env = Object.freeze(__zs_env())`
        // top-level binding captures that same cached object.
        //
        // After the first call, `env_obj` is populated and further updates
        // to `env_json` do not refresh the cached V8 object — the scalars
        // are effectively per-app, not per-request, matching Cloudflare /
        // Bun / Workers semantics. Subsequent requests still overwrite
        // `env_json` for the degraded fallback path in `zs_env_callback`
        // (only reached if `ensure_initialized` has not yet completed,
        // which shouldn't happen under the normal dispatch flow).
        self.state.borrow_mut().set_env_snapshot(env);

        self.ensure_initialized(modules);

        if self.fetch_handler_fn.is_none() {
            // If module init failed (parse/runtime error) we have a real
            // diagnostic; surface it as 500 so the deployer sees the cause
            // rather than the symptom. Fall through to 404 only when the
            // module loaded but didn't export `default.fetch`.
            if let Some(err) = &self.init_error {
                let payload = serde_json::json!({
                    "message": format!("module init failed: {err}"),
                    "name": "Error",
                });
                return crate::FetchOutcome::Response {
                    status: 500,
                    headers: vec![("content-type".into(), "application/json".into())],
                    body: payload.to_string(),
                    logs: vec![],
                };
            }
            return crate::FetchOutcome::Response {
                status: 404,
                headers: vec![("content-type".into(), "application/json".into())],
                body: r#"{"message":"No default.fetch handler exported","name":"Error"}"#.into(),
                logs: vec![],
            };
        }
        // Note: `http_create_request_fn` is no longer required for
        // dispatch (the kernel-side fast-path Request builder
        // `build_kernel_request` is unconditional), but the slot stays
        // around for back-compat with any path still referencing it.

        let request_id = self.next_direct_request_id;
        self.next_direct_request_id += 1;

        let wall_start = Instant::now();

        // Mark the request as executing, and wire the cancel flag through so
        // native ops spawned inside the handler can observe cancellation.
        {
            let mut s = self.state.borrow_mut();
            s.executing_request_id = Some(request_id);
            s.executing_request_cancel = Some(ctx.cancel.clone());
        }

        // Kernel dispatch — three tiers, in order of preference:
        //   1. RPC fast path: URL matches /_zs/v1/<id> AND `default.rpc`
        //      is exported. Slice id in Rust, parse body envelope in V8,
        //      call rpc(id, input, ctx). No Request construction. Sync
        //      and Promise returns are envelope-wrapped; AsyncIterator
        //      and Response returns fall through to (3).
        //   2. fetchFast: when user code opts in via default.fetchFast
        //      (raw HTTP fast path, e.g., the bench fixture's /ping).
        //      Existing zeroship extension. Independent of RPC.
        //   3. Slow path: full default.fetch(request, env, ctx). WinterCG.
        //
        // Compute the wireId only when the cache says rpc is wired up
        // AND the request isn't a WebSocket upgrade (those route through
        // default.fetch → fallbackFetch → dispatchSubscription, which
        // owns the WS handshake). Stored as `Option<&str>` so the borrow
        // on `self.rpc_fn` is released before the mutable
        // `self.arm_cpu_timer()` below; the dispatch block re-borrows
        // inside `enter_v8!`.
        let is_ws_upgrade = headers.iter().any(|(k, v)| {
            k.eq_ignore_ascii_case("upgrade") && v.eq_ignore_ascii_case("websocket")
        });
        let rpc_id_str: Option<&str> = if self.rpc_fn.is_some() && !is_ws_upgrade {
            extract_zs_v1_id(method, url)
        } else {
            None
        };

        // Serialize env JSON only when the slow path will need it.
        // (Headers no longer need JSON marshalling: the kernel-side
        // fast-path Request builder takes the headers slice directly.)
        let env_json = if rpc_id_str.is_some() {
            String::new()
        } else {
            env.as_json().to_string()
        };

        self.arm_cpu_timer();
        // Tracks which dispatch tier produced a pending promise so the
        // pump can pick the right settle path (envelope-wrap for Rpc,
        // inspect_response for Fetch). Default Fetch — only flipped
        // inside the RPC fast-path block.
        let mut pending_origin = PendingOrigin::Fetch;
        // When the RPC fast path returns a pending Promise, this
        // carries the per-request `AbortGuard` from inside the
        // V8 scope out to `store_fetch_pending`. Otherwise the guard
        // would drop at the end of the `enter_v8!` block, leaving
        // the registry empty for async procedures.
        let mut pending_abort_guard: Option<crate::rpc::abort::AbortGuard> = None;
        let dispatch_result: Result<DispatchResult, v8::Global<v8::Promise>> =
            enter_v8!(self, |scope| 'dispatch: {
                let undefined = v8::undefined(scope).into();

                // ---- Tier 1: RPC fast path ----
                // `rpc_id_str.is_some()` implies `self.rpc_fn.is_some()`
                // by construction above, so we can unwrap the Global.
                if let Some(rpc_id) = rpc_id_str {
                    let rpc_fn = v8::Local::new(scope, self.rpc_fn.as_ref().unwrap());
                    let id_arg: v8::Local<v8::Value> = v8::String::new(scope, rpc_id).unwrap().into();
                    let input_arg: v8::Local<v8::Value> = match parse_rpc_input(scope, method, url, body) {
                        InputParse::Ok(v) => v,
                        InputParse::Reject400(msg) => {
                            break 'dispatch Ok(rpc_invalid_argument_response(msg));
                        }
                    };
                    let ctx_arg: v8::Local<v8::Value> = {
                        let maybe = self.state.borrow().ctx_obj.clone();
                        match maybe {
                            Some(g) => v8::Local::new(scope, g).into(),
                            None => v8::Object::new(scope).into(),
                        }
                    };

                    // Build the per-request RpcCtx holder (Rust state +
                    // V8 wrapper), and let `call_rpc_inner` install it in
                    // the ALS slot. On any build failure we drop ALS
                    // support and fall through to a no-ALS call (degrades
                    // to undefined for `__zeroshipGetRpcCtx`, never breaks
                    // the dispatch).
                    //
                    // The AbortController is minted EAGERLY inside
                    // `mint_rpc_ctx` so the abort registry can register it
                    // before user code runs. Headers / URL / signal V8
                    // wrappers are deferred to first accessor read.
                    //
                    // When `app_id` is configured (multi-tenant worker),
                    // register the controller with `crate::rpc::abort` so
                    // the LRU eviction sweep can fire `ctx.signal` for
                    // every in-flight procedure before the isolate is
                    // disposed. The guard drops on sync return / throw;
                    // on a pending promise we hand it off to
                    // `store_fetch_pending` via `pending_abort_guard`.
                    let user_json = {
                        let s = self.state.borrow();
                        if s.per_request_user.is_empty() {
                            None
                        } else {
                            s.per_request_user.get(&request_id).cloned()
                        }
                    };
                    let inputs = build_rpc_ctx_inputs(request_id, headers);
                    // Wrap the request headers in an `Arc` so the holder can
                    // share the same backing `Vec` (refcount-only clone) with
                    // any future borrower instead of doing a full O(N) copy.
                    // The Vec materialization is unavoidable here because the
                    // upstream caller passes a borrowed slice — the
                    // measurable savings come from collapsing the previous
                    // `headers.to_vec()` call into a single allocation that
                    // can be cheaply shared, not from skipping the clone
                    // entirely.
                    let headers_arc: std::sync::Arc<Vec<(String, String)>> =
                        std::sync::Arc::new(headers.to_vec());
                    let mint_result = crate::rpc::mint_rpc_ctx(
                        scope,
                        inputs.request_id,
                        inputs.trace_id,
                        method.to_string(),
                        url.to_string(),
                        headers_arc,
                        user_json,
                        inputs.idempotency_key,
                        self.app_id.is_some(),
                    );
                    let (rpc_ctx_object, mut local_abort_guard) = match mint_result {
                        Ok((ctx_obj, controller)) => {
                            let guard = match (self.app_id, controller) {
                                (Some(aid), Some(c)) => Some(crate::rpc::abort::register_in_flight(
                                    scope,
                                    aid,
                                    request_id,
                                    c,
                                )),
                                _ => None,
                            };
                            (Some(ctx_obj), guard)
                        }
                        Err(_) => (None, None),
                    };
                    match call_rpc_inner(scope, rpc_fn, id_arg, input_arg, ctx_arg, rpc_ctx_object) {
                        RpcCallResult::Handled(res) => {
                            // If the call returned a pending promise,
                            // the pump must settle it as RPC (envelope-
                            // wrap the value, not inspect as Response).
                            // Hand the AbortGuard off to the pump so the
                            // registry entry survives across `await`s.
                            if res.is_err() {
                                pending_origin = PendingOrigin::Rpc;
                                pending_abort_guard = local_abort_guard.take();
                            }
                            break 'dispatch res;
                        }
                        // AsyncIterator return → falls through to the
                        // slow path (default.fetch / synthetic entry),
                        // which wraps it in an SSE Response.
                        RpcCallResult::FallThrough => {}
                    }
                    // Sync return / FallThrough: drop the guard at the
                    // end of the V8 turn (the unused `_` binding here is
                    // explicit — we want the Drop to run).
                    drop(local_abort_guard);
                }

                let fetch_fast_result = if let Some(ff_fn_global) = self.fetch_fast_fn.as_ref() {
                    let ff_fn = v8::Local::new(scope, ff_fn_global);
                    let method_arg = v8::String::new(scope, method).unwrap().into();
                    let url_arg = v8::String::new(scope, url).unwrap().into();
                    let body_arg = v8::String::new(scope, body).unwrap().into();
                    let env_arg: v8::Local<v8::Value> = {
                        let maybe_global = self.state.borrow().env_obj.clone();
                        match maybe_global {
                            Some(g) => v8::Local::new(scope, g).into(),
                            None => v8::Object::new(scope).into(),
                        }
                    };
                    call_fetch_fast_inner(scope, ff_fn, method_arg, url_arg, body_arg, env_arg)
                } else {
                    FetchFastResult::FallThrough
                };

                if let FetchFastResult::Handled(res) = fetch_fast_result {
                    // ---- Fast path: fetchFast(method, url, body, env) ----
                    // Returned a concrete result — use it directly, no
                    // Request/Response object construction needed.
                    res
                } else {
                    // ---- Slow path: full default.fetch(request, env, ctx) ----
                    //
                    // Build the Request directly in Rust via
                    // `fetch_request::build_kernel_request`. Skips:
                    //   - the JS helper compile/run (HTTP_CREATE_REQUEST_JS),
                    //   - JSON.parse on the headers list,
                    //   - the WebIDL constructor algorithm (URL re-parse,
                    //     init union dispatch, body extraction, signal
                    //     minting).
                    // The earlier "3–4% slower" measurement predated full-
                    // native Request + Headers; with both classes now
                    // backed by Box<State> in internal field 0, the V8
                    // round trips collapse to one per Request.
                    let request_opt = crate::fetch_request::build_kernel_request(
                        scope, method, url, headers, body,
                    );
                    if let Some(request) = request_opt {
                        // Stash the Request so `getRequest()` can find it
                        // without the bootstrap having to push `ctx.__zs_request`
                        // through JS on every call. Cleared in drain_request_logs /
                        // discard_request_state together with the other per-request
                        // state (user, ctx, logs).
                        let global = v8::Global::new(scope, request);
                        self.state.borrow_mut().request_by_id.insert(request_id, global);

                        let env_val: v8::Local<v8::Value> = {
                            let maybe_global = self.state.borrow().env_obj.clone();
                            match maybe_global {
                                Some(g) => v8::Local::new(scope, g).into(),
                                None => {
                                    let env_src = v8::String::new(scope, &env_json).unwrap();
                                    v8::json::parse(scope, env_src)
                                        .unwrap_or_else(|| v8::Object::new(scope).into())
                                }
                            }
                        };

                        // Reuse the frozen ctx singleton built in
                        // ensure_initialized. Same V8 Object across every
                        // fetch request — no map transitions, no per-
                        // request Function allocations.
                        let ctx_val: v8::Local<v8::Value> = {
                            let maybe = self.state.borrow().ctx_obj.clone();
                            match maybe {
                                Some(g) => v8::Local::new(scope, g).into(),
                                None => v8::Object::new(scope).into(),
                            }
                        };

                        let handler = v8::Local::new(scope, self.fetch_handler_fn.as_ref().unwrap());
                        call_fetch_inner(scope, handler, undefined, request.into(), env_val, ctx_val)
                    } else {
                        Ok(DispatchResult::Error("Failed to construct Request object".to_string()))
                    }
                }
            });
        self.disarm_cpu_timer();

        if self.check_v8_terminated() {
            self.clear_executing_request();
            self.discard_request_state(request_id);
            return crate::FetchOutcome::Response {
                status: 503,
                headers: vec![("content-type".into(), "application/json".into())],
                body: r#"{"message":"CPU time limit exceeded","name":"Error"}"#.into(),
                logs: vec![],
            };
        }

        let cpu_elapsed = wall_start.elapsed();

        match dispatch_result {
            Ok(DispatchResult::HttpResponse(info)) => {
                self.clear_executing_request();
                self.build_fetch_outcome(request_id, info, cpu_elapsed)
            }
            Ok(DispatchResult::ErrorValue { message, name, stack, status, code, details_json, retryable }) => {
                // Handler threw (or returned a rejected promise). Honor
                // `err.status` so `throw new HttpError(404)` yields 404,
                // not the previous hardcoded 500. Forward any structured-
                // error extras (code/details/retryable) verbatim.
                self.clear_executing_request();
                self.discard_request_state(request_id);
                let extras = crate::dispatch::ErrorExtras {
                    stack: stack.as_deref(),
                    code: code.as_deref(),
                    details_json: details_json.as_deref(),
                    retryable,
                };
                crate::FetchOutcome::Response {
                    status,
                    headers: vec![("content-type".into(), "application/json".into())],
                    body: crate::dispatch::build_error_body(&message, &name, extras),
                    logs: vec![],
                }
            }
            Ok(DispatchResult::Error(msg)) => {
                // Hard dispatch-layer error (couldn't inspect Response,
                // Request construction failed, etc). Not user-thrown, so
                // no stack/name — generic 500.
                self.clear_executing_request();
                self.discard_request_state(request_id);
                crate::FetchOutcome::Response {
                    status: 500,
                    headers: vec![("content-type".into(), "application/json".into())],
                    body: crate::dispatch::build_error_body(&msg, "Error", crate::dispatch::ErrorExtras::default()),
                    logs: vec![],
                }
            }
            Err(promise) => {
                // Pending promise — hand off to the pump. Clear
                // `executing_request_*` so the next synchronous dispatch
                // can enter V8 cleanly, but keep per-request state
                // (logs, user, timers) alive: the pump still needs it
                // when the promise settles. DO NOT call
                // `discard_request_state` here — only after settle.
                //
                // `pending_abort_guard` is `Some` only on the RPC fast
                // path with `app_id` configured; the guard rides
                // alongside the PendingRequest entry and unregisters
                // when the request settles or is cancelled.
                self.clear_executing_request();
                self.store_fetch_pending(
                    request_id, promise, ctx, cpu_elapsed, wall_start, pending_origin,
                    pending_abort_guard,
                )
            }
        }
    }

    /// Track a pending fetch promise and return a `FetchOutcome::Pending`
    /// whose receiver is settled by the pump via `send_settled_reply_any`
    /// once the promise resolves or rejects.
    fn store_fetch_pending(
        &mut self,
        request_id: u64,
        promise: v8::Global<v8::Promise>,
        ctx: crate::RequestCtx,
        cpu_accumulated: Duration,
        wall_start: Instant,
        origin: PendingOrigin,
        abort_guard: Option<crate::rpc::abort::AbortGuard>,
    ) -> crate::FetchOutcome {
        let (tx, rx) = channel::result_slot();

        self.pending_requests.insert(request_id, PendingRequest {
            id: request_id,
            promise,
            reply_fetch: tx,
            cpu_accumulated,
            wall_start,
            cancel: ctx.cancel.clone(),
            origin,
            abort_guard,
        });
        self.notify_pump();

        crate::FetchOutcome::Pending {
            rx,
            cancel: ctx.cancel,
        }
    }

    /// Convert a `ResponseInfo` into a `FetchOutcome`. Mirrors
    /// [`Self::build_http_outcome`] exactly — the writer-attachment logic
    /// for streaming responses is preserved verbatim. When the old HTTP
    /// path is deleted, this helper fully replaces it.
    fn build_fetch_outcome(
        &mut self,
        request_id: u64,
        info: ResponseInfo,
        _cpu_time: Duration,
    ) -> crate::FetchOutcome {
        let logs = self.drain_request_logs(request_id);
        match info {
            ResponseInfo::Complete { status, headers, body } => {
                crate::FetchOutcome::Response { status, headers, body, logs }
            }
            ResponseInfo::Stream { status, headers, stream_id } => {
                let (writer, reader) = channel::stream_buffer();
                // Hand the writer to the response forwarder. It drains
                // any chunks buffered between `begin_forward` and now,
                // then either closes the writer (if the body already
                // completed) or stashes the writer so future chunks
                // pump straight to the TCP-bound channel.
                crate::streams::response_forwarder::attach_writer(
                    &self.state,
                    stream_id,
                    writer,
                );
                crate::FetchOutcome::Stream { status, headers, body_reader: reader, logs }
            }
            ResponseInfo::WebSocket { ws_id, headers } => {
                crate::FetchOutcome::WebSocketUpgrade { ws_id, headers }
            }
        }
    }

    /// Drain newly spawned ops/timers from RuntimeState into the external
    /// `AsyncWork` (for the pump task).
    pub fn drain_new_tasks_into(&mut self, work: &mut AsyncWork) {
        // Fast path
        {
            let s = self.state.borrow();
            if s.spawned_ops.is_empty()
                && s.spawned_timers.is_empty()
                && s.ready_timers.is_empty()
            {
                return;
            }
        }

        {
            let mut s = self.state.borrow_mut();

            for op_future in s.spawned_ops.drain(..) {
                work.pending_ops.push(op_future);
            }

            for timer in s.spawned_timers.drain(..) {
                let SpawnedTimer { id, delay, interval } = timer;
                work.pending_timers.push(Box::pin(async move {
                    compio::time::sleep(delay).await;
                    TimerResult { id, interval }
                }));
            }
        }

        // Fire zero-delay timers inline
        self.fire_ready_timers_pump(work);
    }

    /// Handle an async event from the pump (op completed or timer fired).
    /// Enters V8 briefly to resolve the op/timer, checks settled promises,
    /// and sends results via oneshot channels.
    pub fn handle_async_event(&mut self, event: AsyncEvent, work: &mut AsyncWork) {
        // Pump-driven activity counts — a long-running async procedure
        // resetting the idle clock keeps the GC ticker from firing while
        // user JS is making forward progress.
        self.last_request_ts.set(Instant::now());
        match event {
            AsyncEvent::Op(result) => self.handle_op_result_pump(result, work),
            AsyncEvent::Timer(timer) => self.handle_timer_pump(timer, work),
        }
    }

    /// Handle a completed op result (pump path). Enters V8 to resolve the
    /// promise, then checks if any pending requests settled.
    fn handle_op_result_pump(&mut self, result: OpResult, work: &mut AsyncWork) {
        match result {
            OpResult::Completed { op_id, value, request_id } => {
                if let Some(rid) = request_id {
                    // Restore the owning request's cancel flag so any new fetches
                    // spawned by the V8 callback inherit the same cancellation.
                    let cancel = self.pending_requests.get(&rid).map(|r| r.cancel.clone());
                    let mut s = self.state.borrow_mut();
                    s.executing_request_id = Some(rid);
                    s.executing_request_cancel = cancel;
                }

                let start = Instant::now();

                self.arm_cpu_timer();
                let settled_results = enter_v8!(self, |scope| {
                    crate::dispatch::resolve_op(scope, &self.state, op_id, &value);
                    collect_settled_promises(scope, &mut self.pending_requests)
                });
                self.disarm_cpu_timer();

                if self.check_v8_terminated() {
                    // Only error the request whose JS was executing when the timer fired
                    if let Some(rid) = request_id {
                        if let Some(req) = self.pending_requests.remove(&rid) {
                            req.reply_fetch.send(Err("CPU time limit exceeded".into()));
                        }
                    }
                    self.clear_executing_request();
                    self.drain_new_tasks_into(work);
                    return;
                }

                let cpu_elapsed = start.elapsed();

                if let Some(rid) = request_id {
                    if let Some(req) = self.pending_requests.get_mut(&rid) {
                        req.cpu_accumulated += cpu_elapsed;
                    }
                }

                for (id, req, settled) in settled_results {
                    self.send_settled_reply_any(id, req, settled, cpu_elapsed);
                }

                // CPU limit check for the owning request
                if let Some(rid) = request_id {
                    self.check_cpu_limit(rid);
                }

                self.cleanup_cancelled_requests();
                self.clear_executing_request();

                // Drain new tasks spawned by the V8 callback
                self.drain_new_tasks_into(work);
            }
            OpResult::Failed { op_id, error, request_id } => {
                if let Some(rid) = request_id {
                    let cancel = self.pending_requests.get(&rid).map(|r| r.cancel.clone());
                    let mut s = self.state.borrow_mut();
                    s.executing_request_id = Some(rid);
                    s.executing_request_cancel = cancel;
                }

                let start = Instant::now();

                self.arm_cpu_timer();
                let settled_results = enter_v8!(self, |scope| {
                    crate::dispatch::reject_op(scope, &self.state, op_id, &error);
                    collect_settled_promises(scope, &mut self.pending_requests)
                });
                self.disarm_cpu_timer();

                if self.check_v8_terminated() {
                    if let Some(rid) = request_id {
                        if let Some(req) = self.pending_requests.remove(&rid) {
                            req.reply_fetch.send(Err("CPU time limit exceeded".into()));
                        }
                    }
                    self.clear_executing_request();
                    self.drain_new_tasks_into(work);
                    return;
                }

                let cpu_elapsed = start.elapsed();

                if let Some(rid) = request_id {
                    if let Some(req) = self.pending_requests.get_mut(&rid) {
                        req.cpu_accumulated += cpu_elapsed;
                    }
                }

                for (id, req, settled) in settled_results {
                    self.send_settled_reply_any(id, req, settled, cpu_elapsed);
                }

                if let Some(rid) = request_id {
                    self.check_cpu_limit(rid);
                }

                self.cleanup_cancelled_requests();
                self.clear_executing_request();

                self.drain_new_tasks_into(work);
            }
            OpResult::JsValue { resolver, value, request_id } => {
                // Class-method async result: resolve/reject the bound
                // resolver with a real V8 value.
                if let Some(rid) = request_id {
                    let cancel = self.pending_requests.get(&rid).map(|r| r.cancel.clone());
                    let mut s = self.state.borrow_mut();
                    s.executing_request_id = Some(rid);
                    s.executing_request_cancel = cancel;
                }

                let start = Instant::now();

                self.arm_cpu_timer();
                let settled_results = enter_v8!(self, |scope| {
                    let r = v8::Local::new(scope, &resolver);
                    match value {
                        ResolveValue::Undefined => {
                            r.resolve(scope, v8::undefined(scope).into());
                        }
                        ResolveValue::JsGlobal(g) => {
                            let v = v8::Local::new(scope, &g);
                            r.resolve(scope, v);
                        }
                        ResolveValue::Bytes(bytes) => {
                            // Native-fetch marker — bytes encode a
                            // pending registry id; materialise the
                            // Response (or rejection) inside V8.
                            if let Some(id) = crate::fetch_native::unpack_pending(&bytes) {
                                match crate::fetch_native::materialise_pending(scope, id) {
                                    Ok(v) => { r.resolve(scope, v); }
                                    Err(v) => { r.reject(scope, v); }
                                }
                            } else {
                                let len = bytes.len();
                                let ab = v8::ArrayBuffer::new(scope, len);
                                let store = ab.get_backing_store();
                                for (i, &b) in bytes.iter().enumerate() {
                                    store[i].set(b);
                                }
                                let u8a = v8::Uint8Array::new(scope, ab, 0, len).unwrap();
                                r.resolve(scope, u8a.into());
                            }
                        }
                        ResolveValue::Reject(g) => {
                            let v = v8::Local::new(scope, &g);
                            r.reject(scope, v);
                        }
                        ResolveValue::String(s) => {
                            let v = v8::String::new(scope, &s).unwrap();
                            r.resolve(scope, v.into());
                        }
                        ResolveValue::Json(s) => {
                            let v = v8::String::new(scope, &s)
                                .and_then(|js| v8::json::parse(scope, js))
                                .unwrap_or_else(|| v8::null(scope).into());
                            r.resolve(scope, v);
                        }
                        ResolveValue::JsonWithRehydration { json, transform } => {
                            // **P9 PR 2** — `JSON.parse` first, then walk the
                            // parsed value through plugin-supplied `transform`
                            // to mint v8_class instances for any sentinel
                            // sub-objects (MaskedValue for `__zsmask__`).
                            let parsed = v8::String::new(scope, &json)
                                .and_then(|js| v8::json::parse(scope, js))
                                .unwrap_or_else(|| v8::null(scope).into());
                            let final_v = transform(scope, parsed).unwrap_or(parsed);
                            r.resolve(scope, final_v);
                        }
                        ResolveValue::Bool(b) => {
                            let v = v8::Boolean::new(scope, b);
                            r.resolve(scope, v.into());
                        }
                        ResolveValue::U32(n) => {
                            let v = v8::Integer::new_from_unsigned(scope, n);
                            r.resolve(scope, v.into());
                        }
                        ResolveValue::I32(n) => {
                            let v = v8::Integer::new(scope, n);
                            r.resolve(scope, v.into());
                        }
                        ResolveValue::F64(n) => {
                            let v = v8::Number::new(scope, n);
                            r.resolve(scope, v.into());
                        }
                        ResolveValue::RejectError(e) => {
                            // Materialise the typed exception per
                            // OpError::kind via the shared lowering (the
                            // same one plugin-db's native tx orchestrator
                            // uses to reject directly) so a throw and a
                            // Promise rejection produce identical error
                            // objects. Includes the JsValue passthrough
                            // that re-throws the captured user exception
                            // verbatim.
                            let exc = e.to_exception(scope);
                            r.reject(scope, exc);
                        }
                        ResolveValue::Continuation(run) => {
                            // **P9 PR 3** — the orchestrator's begin/savepoint
                            // step finished; run the plugin-supplied
                            // continuation in this live scope. It owns
                            // whichever resolver it settles (it does NOT
                            // touch `r`, the throwaway resolver on this
                            // envelope) — typically it mints the tx-view,
                            // calls the creator callback, and attaches
                            // commit/rollback handlers to the returned
                            // promise. `state` is the SharedState the
                            // continuation needs to push the follow-up
                            // commit/rollback `spawned_ops`.
                            let state = self.state.clone();
                            run(scope, &state);
                        }
                    }
                    scope.perform_microtask_checkpoint();
                    collect_settled_promises(scope, &mut self.pending_requests)
                });
                self.disarm_cpu_timer();

                if self.check_v8_terminated() {
                    if let Some(rid) = request_id {
                        if let Some(req) = self.pending_requests.remove(&rid) {
                            req.reply_fetch.send(Err("CPU time limit exceeded".into()));
                        }
                    }
                    self.clear_executing_request();
                    self.drain_new_tasks_into(work);
                    return;
                }

                let cpu_elapsed = start.elapsed();

                if let Some(rid) = request_id {
                    if let Some(req) = self.pending_requests.get_mut(&rid) {
                        req.cpu_accumulated += cpu_elapsed;
                    }
                }

                for (id, req, settled) in settled_results {
                    self.send_settled_reply_any(id, req, settled, cpu_elapsed);
                }

                if let Some(rid) = request_id {
                    self.check_cpu_limit(rid);
                }

                self.cleanup_cancelled_requests();
                self.clear_executing_request();
                self.drain_new_tasks_into(work);
            }
            OpResult::Cancelled => {}
            #[cfg(feature = "runtime_native_websocket")]
            OpResult::WebSocketEvent { ws_id } => {
                // Native WebSocket events: drain the per-WS event
                // queue and dispatch each event in FIFO order. Multiple
                // events may have been coalesced under one OpResult
                // (the network task pushes one OpResult per event,
                // but the drain takes them all at once — extras
                // resolve as no-op drains).
                let state_clone = self.state.clone();

                self.arm_cpu_timer();
                let settled_results = enter_v8!(self, |scope| {
                    crate::websocket_native::dispatch::dispatch_pending_ws_events(
                        scope, &state_clone, ws_id,
                    );
                    scope.perform_microtask_checkpoint();
                    collect_settled_promises(scope, &mut self.pending_requests)
                });
                self.disarm_cpu_timer();

                if self.check_v8_terminated() {
                    self.clear_executing_request();
                    self.drain_new_tasks_into(work);
                    return;
                }

                for (id, req, settled) in settled_results {
                    self.send_settled_reply_any(id, req, settled, std::time::Duration::ZERO);
                }

                self.cleanup_cancelled_requests();
                self.clear_executing_request();
                self.drain_new_tasks_into(work);
            }
        }
    }

    /// Handle a timer firing (pump path). Enters V8 to fire the callback,
    /// then checks settled promises.
    fn handle_timer_pump(&mut self, timer: TimerResult, work: &mut AsyncWork) {
        let TimerResult { id, interval } = timer;

        let owner_request_id = self.state.borrow().timer_owner.get(&id).copied();

        if let Some(rid) = owner_request_id {
            let cancel = self.pending_requests.get(&rid).map(|r| r.cancel.clone());
            let mut s = self.state.borrow_mut();
            s.executing_request_id = Some(rid);
            s.executing_request_cancel = cancel;
        }

        let start = Instant::now();

        self.arm_cpu_timer();
        let settled_results = enter_v8!(self, |scope| {
            crate::dispatch::fire_timer_callback(scope, &self.state, id);
            collect_settled_promises(scope, &mut self.pending_requests)
        });
        self.disarm_cpu_timer();

        if self.check_v8_terminated() {
            // Only error the request whose timer callback was executing
            if let Some(rid) = owner_request_id {
                if let Some(req) = self.pending_requests.remove(&rid) {
                    req.reply_fetch.send(Err("CPU time limit exceeded".into()));
                }
            }
            self.clear_executing_request();
            self.drain_new_tasks_into(work);
            return;
        }

        let cpu_elapsed = start.elapsed();

        if let Some(rid) = owner_request_id {
            if let Some(req) = self.pending_requests.get_mut(&rid) {
                req.cpu_accumulated += cpu_elapsed;
            }
        }

        // CPU limit check for the owning request
        if let Some(rid) = owner_request_id {
            self.check_cpu_limit(rid);
        }

        // Re-arm interval timers
        if let Some(interval_dur) = interval {
            let timer_id = id;
            work.pending_timers.push(Box::pin(async move {
                compio::time::sleep(interval_dur).await;
                TimerResult { id: timer_id, interval: Some(interval_dur) }
            }));
        } else {
            self.state.borrow_mut().timer_owner.remove(&id);
        }

        for (id, req, settled) in settled_results {
            self.send_settled_reply_any(id, req, settled, cpu_elapsed);
        }

        self.cleanup_cancelled_requests();
        self.clear_executing_request();

        // Drain new tasks spawned by the V8 callback
        self.drain_new_tasks_into(work);
    }

    // collect_settled_promises is a free function below (avoids double-borrow
    // when called inside enter_v8! which already borrows self.isolate).

    /// Fire zero-delay timers inline during dispatch_start (no AsyncWork needed).
    /// Spawned ops/timers from callbacks remain in RuntimeState for the pump to drain.
    /// Fire zero-delay timers, draining new tasks into external AsyncWork.
    fn fire_ready_timers_pump(&mut self, work: &mut AsyncWork) {
        loop {
            let timer_id = {
                let mut s = self.state.borrow_mut();
                s.ready_timers.pop_front()
            };
            let Some(timer_id) = timer_id else { break };

            let owner_request_id = self.state.borrow().timer_owner.get(&timer_id).copied();

            if let Some(rid) = owner_request_id {
                let cancel = self.pending_requests.get(&rid).map(|r| r.cancel.clone());
                let mut s = self.state.borrow_mut();
                s.executing_request_id = Some(rid);
                s.executing_request_cancel = cancel;
            }

            let start = Instant::now();

            self.arm_cpu_timer();
            let settled_results = enter_v8!(self, |scope| {
                crate::dispatch::fire_timer_callback(scope, &self.state, timer_id);
                collect_settled_promises(scope, &mut self.pending_requests)
            });
            self.disarm_cpu_timer();

            if self.check_v8_terminated() {
                // Only error the request whose timer callback was executing
                if let Some(rid) = owner_request_id {
                    if let Some(req) = self.pending_requests.remove(&rid) {
                        req.reply_fetch.send(Err("CPU time limit exceeded".into()));
                    }
                }
                self.clear_executing_request();
                self.drain_new_tasks_into(work);
                return;
            }

            let cpu_elapsed = start.elapsed();

            if let Some(rid) = owner_request_id {
                if let Some(req) = self.pending_requests.get_mut(&rid) {
                    req.cpu_accumulated += cpu_elapsed;
                }
            }

            // CPU limit check for the owning request
            if let Some(rid) = owner_request_id {
                self.check_cpu_limit(rid);
            }

            self.state.borrow_mut().timer_owner.remove(&timer_id);

            for (id, req, settled) in settled_results {
                self.send_settled_reply_any(id, req, settled, cpu_elapsed);
            }

            self.cleanup_cancelled_requests();
            self.clear_executing_request();

            // Drain new spawned ops/timers from the callback
            {
                let mut s = self.state.borrow_mut();
                for op_future in s.spawned_ops.drain(..) {
                    work.pending_ops.push(op_future);
                }
                for timer in s.spawned_timers.drain(..) {
                    let SpawnedTimer { id, delay, interval } = timer;
                    work.pending_timers.push(Box::pin(async move {
                        compio::time::sleep(delay).await;
                        TimerResult { id, interval }
                    }));
                }
            }
        }
    }

    /// Send a settled reply via whichever channel is present (direct or legacy).
    fn send_settled_reply_any(
        &mut self,
        id: u64,
        req: PendingRequest,
        settled: SettledResult,
        cpu_elapsed: Duration,
    ) {
        let cpu_time = req.cpu_accumulated + cpu_elapsed;
        let _wall_time = req.wall_start.elapsed();

        match settled {
            SettledResult::Rpc(_) => {
                // RPC path is gone; the pump should never produce this.
                unreachable!("SettledResult::Rpc no longer produced after dispatch_rpc removal");
            }
            SettledResult::Http(Ok(info)) => {
                // `build_fetch_outcome` attaches the stream writer + drains
                // logs; we translate its variants 1:1 into SettledFetch.
                let outcome = self.build_fetch_outcome(id, info, cpu_time);
                let settled = match outcome {
                    crate::FetchOutcome::Response { status, headers, body, logs } =>
                        crate::SettledFetch::Response { status, headers, body, logs },
                    crate::FetchOutcome::Stream { status, headers, body_reader, logs } =>
                        crate::SettledFetch::Stream { status, headers, body_reader, logs },
                    crate::FetchOutcome::WebSocketUpgrade { ws_id, headers } =>
                        crate::SettledFetch::WebSocketUpgrade { ws_id, headers, logs: vec![] },
                    crate::FetchOutcome::Pending { .. } =>
                        unreachable!("build_fetch_outcome never returns Pending"),
                };
                req.reply_fetch.send(Ok(settled));
            }
            SettledResult::Http(Err(msg)) => {
                req.reply_fetch.send(Err(msg.into()));
            }
        }
    }

    /// Returns true if there are pending async requests.
    #[allow(dead_code)]
    pub fn has_pending_requests(&self) -> bool {
        !self.pending_requests.is_empty()
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn drain_request_logs(&mut self, request_id: u64) -> Vec<String> {
        let mut s = self.state.borrow_mut();
        // Every terminal path for a request calls this exactly once, so it
        // also owns cleanup of sibling per-request state (auth user, bound
        // ctx, Request object). Keeping these dropped together avoids
        // "logs freed, user still resident" asymmetries that otherwise
        // leak memory for long-lived workers.
        //
        // RPC fast path skips Request construction and ctx binding, so
        // only call .remove() when there's actually something to remove.
        // Each .remove() on an empty HashMap still hashes the key + does
        // a probe; ~150ns × 4 maps = 600ns/req we save when the maps are
        // empty (the common case for the bench fixture's no-side-effect
        // procedures).
        if !s.per_request_user.is_empty() {
            s.per_request_user.remove(&request_id);
        }
        if !s.request_ctx_by_id.is_empty() {
            s.request_ctx_by_id.remove(&request_id);
        }
        if !s.request_by_id.is_empty() {
            s.request_by_id.remove(&request_id);
        }
        if s.per_request_logs.is_empty() {
            return Vec::new();
        }
        s.per_request_logs
            .remove(&request_id)
            .unwrap_or_default()
    }

    fn clear_executing_request(&self) {
        let mut s = self.state.borrow_mut();
        s.executing_request_id = None;
        s.executing_request_cancel = None;
    }

    /// Drop all per-request state (user, bound ctx, logs) for `request_id`.
    /// Called by error-return paths that bail before `drain_request_logs`
    /// would have run. Without this, a request that fails during its
    /// initial V8 turn (CPU termination, isolate init error) leaves its
    /// auth user, bound ctx, and log buffer in `RuntimeState` forever.
    fn discard_request_state(&self, request_id: u64) {
        let mut s = self.state.borrow_mut();
        s.per_request_user.remove(&request_id);
        s.request_ctx_by_id.remove(&request_id);
        s.request_by_id.remove(&request_id);
        s.per_request_logs.remove(&request_id);
    }

    /// Check if a pending request has exceeded its CPU limit. If so, remove
    /// it and send an error via the reply slot.
    fn check_cpu_limit(&mut self, request_id: u64) {
        let Some(cpu_limit) = self.cpu_limit else { return };
        let Some(req) = self.pending_requests.get(&request_id) else { return };
        if req.cpu_accumulated > cpu_limit {
            let req = self.pending_requests.remove(&request_id).unwrap();
            let _logs = self.drain_request_logs(request_id);
            req.reply_fetch.send(Err("CPU time limit exceeded".into()));
        }
    }

    /// Record `elapsed` CPU time consumed by the pump for this Runtime.
    /// Returns `true` if the cumulative budget is exceeded (the caller
    /// should terminate the isolate).
    ///
    /// Budget: an app may consume at most 80% of real wall time over any
    /// 10-second window. A `setInterval(() => { while(...) {} }, 100)`
    /// loop that burns 99 ms of every 100 ms would cross this in ~10 s.
    /// The per-request CPU timer catches synchronous dispatch overruns,
    /// but it doesn't see pump-side work (timer callbacks, microtask
    /// checkpoints) — this budget does.
    pub fn record_pump_cpu(&mut self, elapsed: Duration) -> bool {
        const BUDGET_WINDOW: Duration = Duration::from_secs(10);
        const MAX_CPU_FRACTION: f64 = 0.80;

        self.pump_cpu_accumulated += elapsed;
        let wall = self.pump_wall_start.elapsed();

        if wall < BUDGET_WINDOW {
            return false;
        }

        let fraction = self.pump_cpu_accumulated.as_secs_f64() / wall.as_secs_f64();
        if fraction > MAX_CPU_FRACTION {
            tracing::warn!(
                cpu_fraction = fraction,
                wall_secs = wall.as_secs_f64(),
                "runtime pump CPU budget exceeded; terminating isolate"
            );
            return true;
        }

        // Reset window for the next period.
        self.pump_cpu_accumulated = Duration::ZERO;
        self.pump_wall_start = Instant::now();
        false
    }

    /// Get a clone of the shared state handle.
    pub fn state(&self) -> &SharedState {
        &self.state
    }

    /// Enter V8 to deliver a WebSocket message to the server-side WebSocket.
    /// With the native impl on, pushes a `WsEvent::MessageText` onto the
    /// per-WS event queue and the V8 dispatch arm fires a MessageEvent
    /// through `dom::event_target::dispatch_event`.
    pub fn enter_v8_for_ws_message(&mut self, ws_id: u32, data: &str) {
        #[cfg(feature = "runtime_native_websocket")]
        {
            use crate::websocket_native::network as nw;
            let state = self.state.clone();
            nw::push_event_pub(&state, ws_id, nw::WsEvent::MessageText(data.to_string()));
            return;
        }
        #[cfg(not(feature = "runtime_native_websocket"))]
        {
            // Polyfill path: cached `_onMessage` direct call.
            let cached = {
                let s = self.state.borrow();
                s.websockets.get(&ws_id).and_then(|ws| {
                    ws.cached_handles.as_ref().map(|h| (h.ws_obj.clone(), h.on_message.clone()))
                })
            };
            enter_v8!(self, |scope| {
                if let Some((ws_obj_global, on_message_global)) = cached {
                    let ws_val: v8::Local<v8::Value> = v8::Local::new(scope, &ws_obj_global).into();
                    let func = v8::Local::new(scope, &on_message_global);
                    let data_val: v8::Local<v8::Value> = v8::String::new(scope, data).unwrap().into();
                    func.call(scope, ws_val, &[data_val]);
                } else {
                    let data_val: v8::Local<v8::Value> = v8::String::new(scope, data).unwrap().into();
                    call_ws_method(scope, ws_id, "_onMessage", &[data_val]);
                }
            });
        }
    }

    /// Enter V8 to deliver a WebSocket close to the server-side WebSocket.
    pub fn enter_v8_for_ws_close(&mut self, ws_id: u32, code: u16, reason: &str) {
        #[cfg(feature = "runtime_native_websocket")]
        {
            use crate::websocket_native::network as nw;
            let state = self.state.clone();
            nw::push_event_pub(
                &state,
                ws_id,
                nw::WsEvent::Close {
                    code,
                    reason: reason.to_string(),
                    was_clean: code == 1000,
                },
            );
            return;
        }
        #[cfg(not(feature = "runtime_native_websocket"))]
        {
            let cached = {
                let s = self.state.borrow();
                s.websockets.get(&ws_id).and_then(|ws| {
                    ws.cached_handles.as_ref().map(|h| (h.ws_obj.clone(), h.on_close.clone()))
                })
            };
            enter_v8!(self, |scope| {
                if let Some((ws_obj_global, on_close_global)) = cached {
                    let ws_val: v8::Local<v8::Value> = v8::Local::new(scope, &ws_obj_global).into();
                    let func = v8::Local::new(scope, &on_close_global);
                    let code_val: v8::Local<v8::Value> = v8::Integer::new(scope, code as i32).into();
                    let reason_val: v8::Local<v8::Value> = v8::String::new(scope, reason).unwrap().into();
                    func.call(scope, ws_val, &[code_val, reason_val]);
                } else {
                    let code_val: v8::Local<v8::Value> = v8::Integer::new(scope, code as i32).into();
                    let reason_val: v8::Local<v8::Value> = v8::String::new(scope, reason).unwrap().into();
                    call_ws_method(scope, ws_id, "_onClose", &[code_val, reason_val]);
                }
            });
        }
    }

    fn cleanup_cancelled_requests(&mut self) {
        let cancelled: Vec<u64> = self
            .pending_requests
            .iter()
            .filter(|(_, req)| req.cancel.is_cancelled())
            .map(|(&id, _)| id)
            .collect();

        if cancelled.is_empty() {
            return;
        }

        for id in cancelled {
            let Some(req) = self.pending_requests.remove(&id) else {
                continue;
            };

            // Notify the caller. If the handler already timed out, the
            // receiver is dropped and this send is a no-op — that's fine,
            // it just means we don't double-error.
            req.reply_fetch.send(Err("Request timed out".into()));

            // Drop every piece of per-request state that was still live
            // when cancellation fired. Before this fix, only `logs` got
            // drained; the rest leaked until the isolate was torn down.
            //
            //   - `per_request_user` / `per_request_logs`: owned by the
            //     HashMap keyed on request_id. Covered by drain_request_logs.
            //   - Timers owned by the request: `timer_owner` maps timer_id
            //     → request_id. We walk that map, pull out the matching
            //     timer callbacks, and drop them. The compio `sleep` future
            //     the pump is holding will still fire, but when
            //     `fire_timer_callback` runs there's no callback to
            //     invoke, so no user JS executes.
            //   - Orphan promise resolvers: `pending_resolvers` keyed by
            //     op_id. We don't maintain a request_id → op_id index,
            //     but `executing_request_cancel` short-circuits any op
            //     that checks it (fetch does). For ops that don't check,
            //     the resolver just holds a handle — freed when the
            //     isolate next GCs, bounded memory.
            let _logs = self.drain_request_logs(id);
            self.drop_timers_owned_by(id);
        }
    }

    /// Remove every timer callback owned by `request_id`. The associated
    /// `compio::time::sleep` futures in the pump's `pending_timers` pool
    /// still run to completion (we don't have a handle to abort them),
    /// but `fire_timer_callback` at `dispatch.rs:141` looks the timer up
    /// by id and finds no entry — so no user JS runs.
    fn drop_timers_owned_by(&mut self, request_id: u64) {
        let mut s = self.state.borrow_mut();
        let dead_timers: Vec<u32> = s
            .timer_owner
            .iter()
            .filter(|&(_, &rid)| rid == request_id)
            .map(|(&tid, _)| tid)
            .collect();
        for tid in dead_timers {
            s.timer_owner.remove(&tid);
            s.timer_callbacks.remove(&tid);
        }
    }
}

// ---------------------------------------------------------------------------
// Free function: collect settled promises (avoids double-borrow in enter_v8!)
// ---------------------------------------------------------------------------

/// Check which pending requests have settled promises and extract their results.
/// Takes the pending_requests map directly to avoid borrowing all of `self`
/// inside an `enter_v8!` block (which already borrows `self.isolate`).
fn collect_settled_promises(
    scope: &mut v8::PinScope,
    pending_requests: &mut HashMap<u64, PendingRequest>,
) -> Vec<(u64, PendingRequest, SettledResult)> {
    let settled_ids: Vec<u64> = pending_requests
        .iter()
        .filter_map(|(&id, req)| {
            let p = v8::Local::new(scope, &req.promise);
            if p.state() != v8::PromiseState::Pending {
                Some(id)
            } else {
                None
            }
        })
        .collect();

    settled_ids
        .into_iter()
        .filter_map(|id| {
            let req = pending_requests.remove(&id)?;
            let result = match req.origin {
                PendingOrigin::Fetch => http::extract_settled_result(scope, &req.promise),
                PendingOrigin::Rpc => settle_rpc_promise(scope, &req.promise),
            };
            Some((id, req, result))
        })
        .collect()
}

/// Settle a pending RPC promise into a `SettledResult`. The resolved
/// value goes through `classify_rpc_return` so we get the same wire
/// shape (envelope-wrapped value, inspected Response, fall-through for
/// AsyncIterator) as the synchronous fast path.
fn settle_rpc_promise(
    scope: &mut v8::PinScope,
    promise: &v8::Global<v8::Promise>,
) -> SettledResult {
    let local = v8::Local::new(scope, promise);
    match local.state() {
        v8::PromiseState::Fulfilled => {
            let val = local.result(scope);
            match classify_rpc_return(scope, val) {
                RpcCallResult::Handled(Ok(DispatchResult::HttpResponse(info))) => {
                    SettledResult::Http(Ok(info))
                }
                RpcCallResult::Handled(Ok(DispatchResult::ErrorValue {
                    message, name: _, stack: _, status: _, code: _, details_json: _, retryable: _
                })) => {
                    // Promote to Http(Err) — caller renders a 500 with this
                    // message. Structured-error fields are dropped here; the
                    // settle path's wire only carries a string. (Procedures
                    // returning a thrown Error object as a value rather than
                    // throwing is an unusual shape — most error paths land
                    // via Promise rejection below.)
                    SettledResult::Http(Err(message))
                }
                RpcCallResult::Handled(Ok(DispatchResult::Error(msg))) => {
                    SettledResult::Http(Err(msg))
                }
                RpcCallResult::Handled(Err(_)) => {
                    // Re-pending after settle is a no-op shape — unreachable
                    // from `classify_rpc_return` which only inspects sync
                    // values.
                    SettledResult::Http(Err("rpc re-pending after settle".to_string()))
                }
                RpcCallResult::FallThrough => {
                    // AsyncIterator returned from a Promise: the procedure
                    // already ran and we hold the iterator, but we have no
                    // native SSE encoder here. Surface an explicit error so
                    // the failure mode is visible rather than silent garbage.
                    // (Procedures that stream should use sync `async function*`
                    // returns, which the kernel's sync-tier fall-through
                    // routes through the JS encoder.)
                    SettledResult::Http(Err(
                        "rpc returned AsyncIterator from a Promise — unsupported; use `async function*` for streams".to_string()
                    ))
                }
            }
        }
        v8::PromiseState::Rejected => {
            let exc = local.result(scope);
            match crate::dispatch::v8_exception_to_error_value(scope, exc) {
                DispatchResult::ErrorValue { message, name, stack, status, code, details_json, retryable } => {
                    let extras = crate::dispatch::ErrorExtras {
                        stack: stack.as_deref(),
                        code: code.as_deref(),
                        details_json: details_json.as_deref(),
                        retryable,
                    };
                    SettledResult::Http(Ok(http::ResponseInfo::Complete {
                        status,
                        headers: vec![("content-type".into(), "application/json".into())],
                        body: crate::dispatch::build_error_body(&message, &name, extras),
                    }))
                }
                _ => SettledResult::Http(Err("rpc rejected".to_string())),
            }
        }
        v8::PromiseState::Pending => {
            // collect_settled_promises only invokes us for non-pending
            // promises, so this branch is unreachable in practice.
            SettledResult::Http(Err("rpc settle on pending promise".to_string()))
        }
    }
}

// ---------------------------------------------------------------------------
// Free function: call onRequest handler and inspect result
// ---------------------------------------------------------------------------

/// Look up a WebSocket in the global `__wsRegistry` by ID and call a method on it.
fn call_ws_method(
    scope: &mut v8::PinScope,
    ws_id: u32,
    method: &str,
    args: &[v8::Local<v8::Value>],
) {
    let global = scope.get_current_context().global(scope);

    // Access __wsRegistry
    let registry_key = v8::String::new(scope, "__wsRegistry").unwrap();
    let Some(registry_val) = global.get(scope, registry_key.into()) else { return };
    let Some(registry_obj) = registry_val.to_object(scope) else { return };

    // Look up the WebSocket by its string ID (JS object keys are strings)
    let id_key = v8::String::new(scope, &ws_id.to_string()).unwrap();
    let Some(ws_val) = registry_obj.get(scope, id_key.into()) else { return };
    if ws_val.is_undefined() || ws_val.is_null() { return; }
    let Some(ws_obj) = ws_val.to_object(scope) else { return };

    // Call the method
    let method_key = v8::String::new(scope, method).unwrap();
    let Some(method_val) = ws_obj.get(scope, method_key.into()) else { return };
    let Ok(func) = v8::Local::<v8::Function>::try_from(method_val) else { return };

    func.call(scope, ws_val, args);
}

/// Call the onRequest handler and inspect the result. Separated out to avoid
/// No-op V8 callback for `ctx.passThroughOnException`. Takes no arguments
/// and has no side effects — it's here only so the JS side can call the
/// method without a TypeError. Whether we ever honor the semantic
/// ("if the worker throws, bypass normal error handling and reach the
/// origin") is a separate design question.
fn pass_through_on_exception_noop_callback(
    _scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
}

/// V8 callback for `ctx.waitUntil(promise)`. Real wiring (register the
/// promise into `RuntimeState.wait_until_by_request` so the kernel
/// awaits before releasing the isolate) lands in Task C3. Until then,
/// we still need to prevent unhandled-rejection spam: if user code
/// passes a rejecting promise and we drop it silently, V8 emits an
/// unhandled-rejection for every call.
///
/// The minimum-safe no-op attaches `.catch(() => {})` to the argument
/// when it's a promise, so rejections are suppressed. This matches the
/// observable behavior a real waitUntil implementation would exhibit
/// to the caller (promise settles, nothing blocks response).
fn wait_until_noop_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let arg = args.get(0);
    if !arg.is_promise() {
        return;
    }
    let promise = match v8::Local::<v8::Promise>::try_from(arg) {
        Ok(p) => p,
        Err(_) => return,
    };
    // Empty rust closure marshalled as a V8 function — nothing to do,
    // we just need to register *some* rejection handler so V8 doesn't
    // flag the promise as unhandled.
    let Some(noop) = v8::Function::new(scope, pass_through_on_exception_noop_callback) else {
        return;
    };
    let _ = promise.catch(scope, noop);
}

/// Call the module.default.fetch handler with (request, env, ctx) and
/// classify the result into a [`DispatchResult`]. Separated out for the
/// same reason as [`dispatch_http_inner`] — avoids double-borrowing
/// `self` while the `enter_v8!` macro already holds `&mut self.isolate`.
///
/// Returns:
/// - `Ok(DispatchResult::HttpResponse)` — handler returned a Response
///   (sync or fulfilled promise) that was inspected successfully.
/// - `Ok(DispatchResult::ErrorValue { status, .. })` — handler threw
///   synchronously or rejected a promise. `status` is lifted from
///   `err.status` (400–599) per the same convention `dispatch_request`
///   uses, so `throw new HttpError(404)` yields 404, not hardcoded 500.
/// - `Ok(DispatchResult::Error)` — the Response object couldn't be
///   inspected (malformed shape, non-Response return). Maps to 500.
/// - `Err(v8::Global<v8::Promise>)` — handler returned a still-pending
///   promise. The real async path lands in Task B4; caller serves 501
///   in the meantime.
/// Outcome of the `fetchFast(method, url, body, env)` extension.
///
/// `Handled` means the user produced a definitive result (sync or
/// promise-based). `FallThrough` means the user returned `null` /
/// `undefined` — the kernel should continue to the slow `default.fetch`
/// path. A rejected promise OR a synchronous throw surfaces through
/// `Handled(Ok(DispatchResult::ErrorValue))` so the error status code
/// reaches the client just like the regular fetch path.
enum FetchFastResult {
    Handled(Result<DispatchResult, v8::Global<v8::Promise>>),
    FallThrough,
}

/// Materialize a `fetchFast` return value into a `DispatchResult`.
///
/// Accepted JS return shapes (sync or via fulfilled Promise):
///   - `null` / `undefined` → `FetchFastResult::FallThrough` (kernel
///     runs the full `default.fetch(request, env, ctx)` path).
///   - `{ status, headers, body }` plain object → `ResponseInfo::Complete`
///     with those exact fields. `headers` is an object `{ k: v }` or
///     omitted. `body` is a string.
///   - String → 200 OK with the string as the body, content-type
///     `application/json` (matching the RPC path's default).
///   - Response instance → standard inspect_response path (user mixed
///     fast + slow).
///   - anything else → JSON.stringify as 200 OK body.
fn call_fetch_fast_inner(
    scope: &mut v8::PinScope,
    ff_fn: v8::Local<v8::Function>,
    method_arg: v8::Local<v8::Value>,
    url_arg: v8::Local<v8::Value>,
    body_arg: v8::Local<v8::Value>,
    env_arg: v8::Local<v8::Value>,
) -> FetchFastResult {
    let undefined = v8::undefined(scope).into();
    let (result_val, caught_exception) = {
        v8::tc_scope!(let tc, scope);
        let r = ff_fn.call(tc, undefined, &[method_arg, url_arg, body_arg, env_arg]);
        if tc.has_caught() {
            let exc = tc.exception();
            let exc_global = exc.map(|e| v8::Global::new(tc, e));
            (None, exc_global)
        } else {
            (r.map(|v| v8::Global::new(tc, v)), None)
        }
    };

    scope.perform_microtask_checkpoint();

    if let Some(exc_global) = caught_exception {
        let exc_local = v8::Local::new(scope, &exc_global);
        return FetchFastResult::Handled(Ok(
            crate::dispatch::v8_exception_to_error_value(scope, exc_local),
        ));
    }

    let Some(result_global) = result_val else {
        return FetchFastResult::Handled(Ok(DispatchResult::Error(
            "fetchFast returned no value".to_string(),
        )));
    };

    let result = v8::Local::new(scope, &result_global);

    // Handle promise return (rare for fetchFast which is designed to be
    // sync-friendly, but valid for async handlers).
    if result.is_promise() {
        let promise = v8::Local::<v8::Promise>::try_from(result).unwrap();
        return match promise.state() {
            v8::PromiseState::Fulfilled => {
                let resolved = promise.result(scope);
                classify_fetch_fast_return(scope, resolved)
            }
            v8::PromiseState::Rejected => {
                let exc = promise.result(scope);
                FetchFastResult::Handled(Ok(
                    crate::dispatch::v8_exception_to_error_value(scope, exc),
                ))
            }
            v8::PromiseState::Pending => {
                FetchFastResult::Handled(Err(v8::Global::new(scope, promise)))
            }
        };
    }

    classify_fetch_fast_return(scope, result)
}

/// Turn a resolved `fetchFast` return value into a DispatchResult
/// (after any promise unwrap).
fn classify_fetch_fast_return(
    scope: &mut v8::PinScope,
    val: v8::Local<v8::Value>,
) -> FetchFastResult {
    if val.is_null() || val.is_undefined() {
        return FetchFastResult::FallThrough;
    }

    // String → 200 OK with body, application/json content-type.
    if val.is_string() {
        let body = val.to_rust_string_lossy(scope);
        return FetchFastResult::Handled(Ok(DispatchResult::HttpResponse(
            http::ResponseInfo::Complete {
                status: 200,
                headers: vec![("content-type".into(), "application/json".into())],
                body,
            },
        )));
    }

    // Response instance → inspect (user mixed fast + slow).
    if crate::http::looks_like_response(scope, val) {
        return match crate::http::inspect_response(scope, val) {
            Ok(info) => FetchFastResult::Handled(Ok(DispatchResult::HttpResponse(info))),
            Err(e) => FetchFastResult::Handled(Ok(DispatchResult::Error(e))),
        };
    }

    // Plain object with { status, headers, body }.
    if let Some(obj) = val.to_object(scope) {
        let status = http::get_u32_property(scope, obj, "status") as u16;
        let body = http::get_string_property(scope, obj, "body");
        let headers = extract_plain_headers(scope, obj);
        let effective_status = if status == 0 { 200 } else { status };
        return FetchFastResult::Handled(Ok(DispatchResult::HttpResponse(
            http::ResponseInfo::Complete {
                status: effective_status,
                headers,
                body,
            },
        )));
    }

    // Fallback: JSON.stringify.
    let body = v8::json::stringify(scope, val)
        .map(|s| s.to_rust_string_lossy(scope))
        .unwrap_or_else(|| "null".to_string());
    FetchFastResult::Handled(Ok(DispatchResult::HttpResponse(
        http::ResponseInfo::Complete {
            status: 200,
            headers: vec![("content-type".into(), "application/json".into())],
            body,
        },
    )))
}

// ─── RPC fast path ─────────────────────────────────────────────────────────
//
// Standalone kernel entry point for `default.rpc(name, input, ctx)`.
// Activates when:
//   - `default.rpc` is exported by the user module (cached as `rpc_fn`)
//   - The incoming URL contains `/_zs/v1/<id>` (POST or GET)
//
// The kernel slices the id in Rust (no URL-object construction), parses
// the body's superjson `{ json }` envelope in V8, and calls
// `rpc(id, input, ctx)`. Resolved values are encoded inline:
//   - Plain JSON-serializable → `{"json":<result>}` envelope, 200 OK
//   - Response object → inspect_response (status, headers, body)
//   - AsyncIterator → fall through to default.fetch (whose synthetic
//     entry wraps it in an SSE Response)
//
// Synchronous handlers complete entirely in this block; async handlers
// (Promise) hand off to the existing pump path on pending; the resolved
// fast path applies to fulfilled-on-checkpoint promises too.

// External one-byte string constants so per-request envelope unwrap
// reuses the same V8 string identity (lets V8's hidden-class IC fire
// on `obj.get(json_key)` after warmup instead of re-interning the key
// every call). Same pattern as http.rs's K_STATUS / K_HEADERS etc.
static K_JSON: v8::OneByteConst = v8::String::create_external_onebyte_const(b"json");

#[inline(always)]
fn key<'s>(scope: &mut v8::PinScope<'s, '_>, k: &'static v8::OneByteConst) -> v8::Local<'s, v8::String> {
    v8::String::new_from_onebyte_const(scope, k).unwrap()
}

/// Outcome of the RPC fast-path attempt.
enum RpcCallResult {
    /// rpc(...) returned a serializable value, a Response, or threw
    /// — we have a concrete DispatchResult (or pending Promise) to
    /// return.
    Handled(Result<DispatchResult, v8::Global<v8::Promise>>),
    /// rpc(...) returned an AsyncIterator — the kernel can't encode
    /// it inline (no native SSE encoder; the synthetic SSR entry's
    /// JS-side encoder owns that), so we fall through to the slow
    /// `default.fetch` path, which re-invokes the procedure to wrap
    /// it in a Response.
    ///
    /// Re-invocation is benign for `async function*` (the body only
    /// runs when iterated, and the discarded generator is GC'd). For
    /// hand-rolled AsyncIterators that do work in the synchronous
    /// constructor, the work runs twice. Procedures should use
    /// `async function*` (the kind=stream convention) for this lane.
    FallThrough,
}

/// Slice `<id>` from a URL whose path contains `/_zs/v1/<id>`. Returns
/// None for non-POST/GET methods or malformed URLs. Cheaper than
/// constructing a URL object: a single `find` for the tag plus a single
/// `find` for the first query/fragment terminator.
fn extract_zs_v1_id<'a>(method: &str, url: &'a str) -> Option<&'a str> {
    if !(method.eq_ignore_ascii_case("POST") || method.eq_ignore_ascii_case("GET")) {
        return None;
    }
    const TAG: &str = "/_zs/v1/";
    let start = url.find(TAG)? + TAG.len();
    let rest = &url[start..];
    let end = start + rest.find(|c: char| c == '?' || c == '#').unwrap_or(rest.len());
    if start >= end { return None; }
    Some(&url[start..end])
}

/// Outcome of `parse_rpc_input`. `Reject400` carries a static message
/// that the call site renders into a 400 INVALID_ARGUMENT response —
/// matches the JS-side `_zsErrResponse(400, "INVALID_ARGUMENT", ...)`
/// shape so a malformed wire produces the same body whether the
/// kernel fast path or the slow path's synthetic entry caught it.
enum InputParse<'s> {
    Ok(v8::Local<'s, v8::Value>),
    Reject400(&'static str),
}

/// Materialize the `input` arg for `rpc(name, input, ctx)`.
///
/// Wire shapes:
///   - POST:  body is JSON, expected canonical shape `{"json":<v>}`.
///   - GET:   query string carries `?input=<base64url-of-JSON-body>`.
///
/// Empty body / missing query param → `undefined` (Ok). Malformed JSON
/// or malformed base64url → `Reject400` so the kernel returns a 400
/// instead of silently coercing to undefined.
fn parse_rpc_input<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    method: &str,
    url: &str,
    body: &str,
) -> InputParse<'s> {
    if method.eq_ignore_ascii_case("POST") {
        return parse_envelope_body(scope, body);
    }
    if !method.eq_ignore_ascii_case("GET") {
        return InputParse::Ok(v8::undefined(scope).into());
    }
    // GET: locate `input=<base64url>` in the query string. Missing
    // param → empty input (undefined), not a 400.
    let Some(raw) = url
        .split_once('?')
        .map(|(_, qs)| qs.split('#').next().unwrap_or(qs))
        .and_then(|qs| qs.split('&').find_map(|pair| pair.strip_prefix("input=")))
    else {
        return InputParse::Ok(v8::undefined(scope).into());
    };
    use base64::Engine as _;
    let Ok(decoded) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(raw) else {
        return InputParse::Reject400("invalid base64url input");
    };
    let Ok(s) = std::str::from_utf8(&decoded) else {
        return InputParse::Reject400("invalid base64url input");
    };
    parse_envelope_body(scope, s)
}

/// Parse a JSON body wrapped in the `{"json":<v>}` envelope.
///
/// Fast path: when the body matches the canonical shape exactly
/// (no whitespace, no extra keys), slice the inner value in Rust and
/// JSON-parse only that — saves one V8 object allocation + one property
/// get per request vs. the general path. zerobench/SuperJSON/our own
/// vite plugin all emit this exact shape, so it's the common case.
///
/// Slow path: full `JSON.parse(body).json`. Handles whitespace,
/// reordered keys, or missing envelope (raw JSON value pass-through).
/// On total parse failure, returns `Reject400`.
#[inline]
fn parse_envelope_body<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    body: &str,
) -> InputParse<'s> {
    if body.is_empty() { return InputParse::Ok(v8::undefined(scope).into()); }

    const PREFIX: &[u8] = b"{\"json\":";
    let bytes = body.as_bytes();
    if bytes.len() >= PREFIX.len() + 1
        && bytes.starts_with(PREFIX)
        && bytes.last() == Some(&b'}')
    {
        let inner = &body[PREFIX.len()..body.len() - 1];
        if !inner.is_empty() {
            // `null` is the common no-arg payload — skip V8 entirely.
            if inner == "null" { return InputParse::Ok(v8::null(scope).into()); }
            if let Some(s) = v8::String::new(scope, inner) {
                if let Some(v) = v8::json::parse(scope, s) { return InputParse::Ok(v); }
            }
            // Parse failure on the sliced inner → fall through. Either
            // the inner isn't valid JSON (e.g. trailing extra keys
            // before the outer `}`) or v8::String::new rejected the
            // input. The slow path's full envelope parse is the safety
            // net — it's slower but catches anything the fast slice
            // can't.
        }
    }

    // Slow path: parse the full envelope + property get.
    let Some(body_str) = v8::String::new(scope, body) else {
        return InputParse::Reject400("invalid JSON body");
    };
    let Some(parsed) = v8::json::parse(scope, body_str) else {
        return InputParse::Reject400("invalid JSON body");
    };
    if !parsed.is_object() { return InputParse::Ok(parsed); }
    let obj: v8::Local<v8::Object> = parsed.try_into().unwrap();
    // Cached external one-byte "json" key — same V8 string identity per
    // request, so V8's hidden-class IC fires on the property get.
    let json_key = key(scope, &K_JSON);
    let v = obj.get(scope, json_key.into()).unwrap_or(parsed);
    InputParse::Ok(if v.is_undefined() { parsed } else { v })
}

/// Build a 400 INVALID_ARGUMENT response body. Format matches the
/// JS-side `_zsErrResponse(400, "INVALID_ARGUMENT", message)` so the
/// wire is identical whether the kernel or the synthetic entry
/// produces it.
fn rpc_invalid_argument_response(message: &str) -> DispatchResult {
    let body = format!(
        r#"{{"message":"{}","name":"Error","code":"INVALID_ARGUMENT"}}"#,
        message,
    );
    DispatchResult::HttpResponse(http::ResponseInfo::Complete {
        status: 400,
        headers: vec![("content-type".into(), "application/json".into())],
        body,
    })
}

/// Per-request scalar inputs the RPC ctx holder needs. Headers stays
/// borrowed; the caller hands them to `mint_rpc_ctx` directly so we
/// avoid a redundant clone.
struct RpcCtxInputs {
    request_id: String,
    trace_id: String,
    idempotency_key: Option<String>,
}

/// Extract `(request_id, trace_id, idempotency_key)` from the request
/// headers. Cheap (one header scan, ~50 ns). Materialization of Headers /
/// URL / signal / user is deferred to the holder's accessors.
fn build_rpc_ctx_inputs(request_id: u64, headers: &[(String, String)]) -> RpcCtxInputs {
    let mut idempotency_key: Option<String> = None;
    let mut trace_id_from_header: Option<String> = None;
    for (k, v) in headers {
        if idempotency_key.is_none() && k.eq_ignore_ascii_case("Idempotency-Key") {
            idempotency_key = Some(v.clone());
        }
        if trace_id_from_header.is_none() && k.eq_ignore_ascii_case("traceparent") {
            let parts: Vec<&str> = v.split('-').collect();
            if parts.len() >= 2 && parts[1].len() == 32 {
                trace_id_from_header = Some(parts[1].to_string());
            }
        }
    }
    RpcCtxInputs {
        request_id: format_req_id_hex(request_id),
        trace_id: trace_id_from_header.unwrap_or_else(|| format_trace_id_hex(request_id)),
        idempotency_key,
    }
}

/// Hand-rolled `format!("req_{:016x}", id)`. Avoids two heap-allocations
/// and the `core::fmt` LowerHex stack the dispatch path used to spend
/// 9.22% inclusive CPU in (perf 2026-05-07). Output bit-identical to
/// `format!`: `req_` + 16 lowercase hex chars (length 20).
#[inline]
fn format_req_id_hex(id: u64) -> String {
    let mut s = String::with_capacity(20);
    s.push_str("req_");
    for shift in (0..64).rev().step_by(4) {
        let nib = ((id >> shift) & 0xF) as u8;
        let c = if nib < 10 { b'0' + nib } else { b'a' + (nib - 10) };
        s.push(c as char);
    }
    s
}

/// Hand-rolled `format!("trace_{:016x}", id)`. Output bit-identical to
/// `format!`: `trace_` + 16 lowercase hex chars (length 22).
#[inline]
fn format_trace_id_hex(id: u64) -> String {
    let mut s = String::with_capacity(22);
    s.push_str("trace_");
    for shift in (0..64).rev().step_by(4) {
        let nib = ((id >> shift) & 0xF) as u8;
        let c = if nib < 10 { b'0' + nib } else { b'a' + (nib - 10) };
        s.push(c as char);
    }
    s
}

/// Invoke `default.rpc(id, input, ctx)` and classify the return value.
///
/// `als_ctx_object`, when `Some`, is installed into V8's
/// `ContinuationPreservedEmbedderData` slot under the platform's
/// RPC-ctx Symbol for the duration of the call. The slot is
/// restored on every exit path (sync return, JS throw, panic). When
/// `None`, the call runs without an ALS frame — used by paths that
/// don't have a populated `RpcContext` yet (synthetic-entry tests
/// using the older wire shape).
fn call_rpc_inner<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    rpc_fn: v8::Local<'s, v8::Function>,
    id_arg: v8::Local<'s, v8::Value>,
    input_arg: v8::Local<'s, v8::Value>,
    ctx_arg: v8::Local<'s, v8::Value>,
    als_ctx_object: Option<v8::Local<'s, v8::Object>>,
) -> RpcCallResult {
    let undefined: v8::Local<v8::Value> = v8::undefined(scope).into();
    let invoke = |scope: &mut v8::PinScope<'s, '_>| {
        v8::tc_scope!(let tc, scope);
        let r = rpc_fn.call(tc, undefined, &[id_arg, input_arg, ctx_arg]);
        if tc.has_caught() {
            let exc = tc.exception();
            let exc_global = exc.map(|e| v8::Global::new(tc, e));
            (None, exc_global)
        } else {
            (r.map(|v| v8::Global::new(tc, v)), None)
        }
    };
    let (result_val, caught_exception) = match als_ctx_object {
        Some(ctx_object) => crate::rpc::with_rpc_context_lazy(scope, ctx_object, invoke),
        None => invoke(scope),
    };

    scope.perform_microtask_checkpoint();

    if let Some(exc_global) = caught_exception {
        let exc_local = v8::Local::new(scope, &exc_global);
        return RpcCallResult::Handled(Ok(
            crate::dispatch::v8_exception_to_error_value(scope, exc_local),
        ));
    }

    let Some(result_global) = result_val else {
        return RpcCallResult::Handled(Ok(DispatchResult::Error(
            "rpc returned no value".to_string(),
        )));
    };
    let result: v8::Local<v8::Value> = v8::Local::new(scope, &result_global);

    if result.is_promise() {
        let promise = v8::Local::<v8::Promise>::try_from(result).unwrap();
        return match promise.state() {
            v8::PromiseState::Fulfilled => {
                let resolved = promise.result(scope);
                classify_rpc_return(scope, resolved)
            }
            v8::PromiseState::Rejected => {
                let exc = promise.result(scope);
                RpcCallResult::Handled(Ok(
                    crate::dispatch::v8_exception_to_error_value(scope, exc),
                ))
            }
            v8::PromiseState::Pending => {
                RpcCallResult::Handled(Err(v8::Global::new(scope, promise)))
            }
        };
    }

    classify_rpc_return(scope, result)
}

/// Materialize a resolved RPC return value into a DispatchResult.
///
/// Object-shape probes (skipped for primitives):
///   - Response (branded via `__zsResponse` on Response.prototype) →
///     inspect inline and return its ResponseInfo. No fall-through —
///     the user procedure is NOT re-invoked.
///   - AsyncIterator → fall through. The kernel has no native SSE
///     encoder; the synthetic SSR entry's JS encoder takes over via
///     re-invocation (see RpcCallResult::FallThrough).
///
/// Everything else: JSON.stringify and wrap in `{"json":<value>}`.
fn classify_rpc_return<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    val: v8::Local<'s, v8::Value>,
) -> RpcCallResult {
    if val.is_object() {
        // Response: branded via `__zsResponse = 1` on the prototype by
        // the fetch polyfill. One property get (cached IC after
        // warmup) — much cheaper than constructor.name probing.
        if http::looks_like_response(scope, val) {
            return match http::inspect_response(scope, val) {
                Ok(info) => RpcCallResult::Handled(Ok(DispatchResult::HttpResponse(info))),
                Err(e) => RpcCallResult::Handled(Ok(DispatchResult::Error(e))),
            };
        }
        // AsyncIterator detection: cheap, single-symbol probe.
        let obj: v8::Local<v8::Object> = val.try_into().unwrap();
        let async_iter_sym = v8::Symbol::get_async_iterator(scope);
        if obj.has(scope, async_iter_sym.into()).unwrap_or(false) {
            return RpcCallResult::FallThrough;
        }
    }

    // Plain value → JSON.stringify and wrap in `{"json":<value>}`
    // envelope. Pre-size the buffer so the prefix + body + suffix push
    // is one allocation. `s.length()` is UTF-16 code units; for ASCII
    // JSON it matches UTF-8 bytes exactly, for non-ASCII it under-
    // counts and the String reallocates once — still cheaper than
    // format!'s build-then-realloc-into-final pass.
    let body = match v8::json::stringify(scope, val) {
        Some(s) => {
            let mut buf = String::with_capacity(s.length() + 9);
            buf.push_str("{\"json\":");
            buf.push_str(&s.to_rust_string_lossy(scope));
            buf.push('}');
            buf
        }
        None => String::from("{\"json\":null}"),
    };
    RpcCallResult::Handled(Ok(DispatchResult::HttpResponse(
        http::ResponseInfo::Complete {
            status: 200,
            headers: vec![("content-type".into(), "application/json".into())],
            body,
        },
    )))
}

/// Extract headers from a plain `{ k: v }` object. For the fetchFast
/// extension — not a Headers instance, just a JS object literal.
fn extract_plain_headers(
    scope: &mut v8::PinScope,
    obj: v8::Local<v8::Object>,
) -> Vec<(String, String)> {
    let headers_key = v8::String::new(scope, "headers").unwrap();
    let Some(headers_val) = obj.get(scope, headers_key.into()) else {
        return Vec::new();
    };
    let Some(headers_obj) = headers_val.to_object(scope) else {
        return Vec::new();
    };
    let Some(names) = headers_obj.get_own_property_names(scope, Default::default()) else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(names.length() as usize);
    for i in 0..names.length() {
        let Some(name_val) = names.get_index(scope, i) else {
            continue;
        };
        let name_str = name_val.to_rust_string_lossy(scope);
        let Some(value_val) = headers_obj.get(scope, name_val) else {
            continue;
        };
        let value_str = value_val.to_rust_string_lossy(scope);
        out.push((name_str, value_str));
    }
    out
}

fn call_fetch_inner(
    scope: &mut v8::PinScope,
    handler: v8::Local<v8::Function>,
    undefined: v8::Local<v8::Value>,
    request: v8::Local<v8::Value>,
    env: v8::Local<v8::Value>,
    ctx: v8::Local<v8::Value>,
) -> Result<DispatchResult, v8::Global<v8::Promise>> {
    // Use a TryCatch so synchronous throws surface the exception value
    // (needed for err.status / err.name / err.stack) rather than a bare
    // `None` return that drops all of it.
    let (result_val, caught_exception) = {
        v8::tc_scope!(let tc, scope);
        let r = handler.call(tc, undefined, &[request, env, ctx]);
        if tc.has_caught() {
            let exc = tc.exception();
            let exc_global = exc.map(|e| v8::Global::new(tc, e));
            (None, exc_global)
        } else {
            (r.map(|v| v8::Global::new(tc, v)), None)
        }
    };

    scope.perform_microtask_checkpoint();

    if let Some(exc_global) = caught_exception {
        let exc_local = v8::Local::new(scope, &exc_global);
        return Ok(crate::dispatch::v8_exception_to_error_value(scope, exc_local));
    }

    let Some(result_global) = result_val else {
        // `call` returned None but TryCatch saw no exception — defensive
        // fallback; should be unreachable.
        return Ok(DispatchResult::Error(
            "default.fetch returned no value".to_string(),
        ));
    };

    let result = v8::Local::new(scope, &result_global);

    if result.is_promise() {
        let promise = v8::Local::<v8::Promise>::try_from(result).unwrap();
        match promise.state() {
            v8::PromiseState::Fulfilled => {
                let resolved = promise.result(scope);
                match http::inspect_response(scope, resolved) {
                    Ok(info) => Ok(DispatchResult::HttpResponse(info)),
                    Err(e) => Ok(DispatchResult::Error(e)),
                }
            }
            v8::PromiseState::Rejected => {
                let exc = promise.result(scope);
                Ok(crate::dispatch::v8_exception_to_error_value(scope, exc))
            }
            v8::PromiseState::Pending => Err(v8::Global::new(scope, promise)),
        }
    } else {
        match http::inspect_response(scope, result) {
            Ok(info) => Ok(DispatchResult::HttpResponse(info)),
            Err(e) => Ok(DispatchResult::Error(e)),
        }
    }
}
