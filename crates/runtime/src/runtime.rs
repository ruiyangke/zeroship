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

use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::stream::FuturesUnordered;

use crate::init::{init_v8, load_polyfills_and_modules};
use crate::http::{self, ResponseInfo, SettledResult, HTTP_CREATE_REQUEST_JS};
use crate::modules::ModuleEntry;
use crate::plugin::NativePlugin;
use crate::state::{
    DispatchResult, OpResult, RuntimeState, SharedState, SpawnedTimer, TimerResult,
};

use crate::channel::{
    self, CancelFlag, ResultSender, StreamWriter,
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
// StreamForwarder — overflow-buffered channel writer for outbound HTTP streams
// ---------------------------------------------------------------------------

struct StreamForwarder {
    writer: StreamWriter,
    /// Set once `writer.push` has returned `Full`. Further chunks are
    /// dropped rather than buffered — the reader already saw the overflow
    /// via `is_overflow()` and has terminated forwarding.
    overflowed: bool,
}

impl StreamForwarder {
    fn new(writer: StreamWriter) -> Self {
        Self { writer, overflowed: false }
    }

    /// Forward a chunk into the reader buffer. Returns `false` when the
    /// underlying `StreamWriter` rejected the chunk (cap exceeded or
    /// stream closed). Callers should stop producing on `false` — the
    /// downstream consumer is either gone or too slow, and buffering more
    /// data would just grow memory without delivering it.
    fn try_forward(&mut self, data: Vec<u8>) -> bool {
        if self.overflowed {
            return false;
        }
        match self.writer.push(data) {
            crate::channel::StreamPushResult::Ok => true,
            crate::channel::StreamPushResult::Full => {
                self.overflowed = true;
                // Close the stream so readers observe completion and exit
                // their drain loops cleanly.
                self.writer.close();
                eprintln!("[runtime] stream forwarder: buffer cap exceeded — dropping producer");
                false
            }
            crate::channel::StreamPushResult::Closed => false,
        }
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

    /// Build the runtime. Panics on V8 init failure (same as the underlying
    /// `v8::Isolate::new` call — not newly fallible here).
    pub fn build(self) -> Runtime {
        let limits = self.limits;
        let modules_rc = Rc::new(self.modules);
        let inner = RuntimeInner::new_with_plugins(
            self.env_vars,
            limits.cpu_limit,
            limits.wall_timeout,
            limits.heap_limit_bytes,
            self.plugins,
        );
        Runtime {
            inner: Rc::new(RefCell::new(inner)),
            limits,
            modules: modules_rc,
        }
    }
}

// ---------------------------------------------------------------------------
// PendingRequest — tracking for in-flight async requests
// ---------------------------------------------------------------------------

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
    pub(crate) fetch_handler_fn: Option<v8::Global<v8::Function>>,
    /// Cached JS helper that constructs a Request from Rust-supplied params.
    http_create_request_fn: Option<v8::Global<v8::Function>>,
    pub(crate) initialized: bool,
    pub(crate) state: SharedState,
    /// Plugins registered on the runtime at boot.
    plugins: Vec<Arc<dyn NativePlugin>>,

    /// Stream forwarders: stream_id -> StreamForwarder for outbound HTTP streams.
    stream_forwarders: HashMap<u32, StreamForwarder>,

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

impl RuntimeInner {
    /// Create a new runtime with plugins.
    /// Plugins register native functions on `zeroship.{namespace}.*`.
    fn new_with_plugins(
        env_vars: HashMap<String, String>,
        cpu_limit: Option<Duration>,
        wall_timeout: Option<Duration>,
        heap_limit_bytes: Option<usize>,
        plugins: Vec<Arc<dyn NativePlugin>>,
    ) -> Self {
        init_v8();

        // Default 128 MB per isolate. Control-plane can tune per-app:
        // free-tier → 64 MB, paid → 256 MB. The old hardcoded 512 MB
        // meant MAX_ISOLATES=200 × 512 MB × threads could claim 100+ GB.
        const DEFAULT_HEAP: usize = 128 * 1024 * 1024;
        let heap_max = heap_limit_bytes.unwrap_or(DEFAULT_HEAP);
        let params = v8::CreateParams::default().heap_limits(0, heap_max);
        let mut isolate = v8::Isolate::new(params);

        // Register near-heap-limit callback. After MAX_HEAP_LIMIT_HITS
        // consecutive invocations, terminate execution so the app doesn't
        // pin RSS at the cap forever — each hit means V8 tried to grow,
        // failed, GC'd, tried again, and still needs more memory.
        //
        // Counter is heap-allocated and leaked (static lifetime for the
        // extern callback). One allocation per Runtime, freed when the
        // process exits. Worth the 8 bytes to avoid an atomic-global or
        // unsafe thread-local dance.
        const MAX_HEAP_LIMIT_HITS: u32 = 5;
        let heap_hit_counter = Box::into_raw(Box::new(0u32));

        unsafe extern "C" fn near_heap_limit_callback(
            data: *mut std::ffi::c_void,
            current_heap_limit: usize,
            _initial_heap_limit: usize,
        ) -> usize {
            if data.is_null() {
                return current_heap_limit;
            }
            // SAFETY: `data` was set via `Box::into_raw(Box::new(0u32))` below
            // and is never freed during the isolate's lifetime. V8 invokes
            // this callback only from the isolate's owning thread per
            // `Isolate::add_near_heap_limit_callback` contract, so we have
            // exclusive access to the counter here.
            let counter = unsafe { &mut *(data as *mut u32) };
            *counter += 1;
            if *counter >= MAX_HEAP_LIMIT_HITS {
                eprintln!(
                    "[v8] Heap limit {}MB hit {} times — terminating isolate",
                    current_heap_limit / 1024 / 1024,
                    *counter
                );
                // V8 checks the termination flag after the callback returns,
                // so the allocation that triggered this callback will throw
                // a catchable exception first — the terminate fires on the
                // next microtask boundary.
            } else {
                eprintln!(
                    "[v8] Near heap limit: {}MB ({}/{})",
                    current_heap_limit / 1024 / 1024,
                    *counter,
                    MAX_HEAP_LIMIT_HITS
                );
            }
            current_heap_limit
        }
        isolate.add_near_heap_limit_callback(
            near_heap_limit_callback,
            heap_hit_counter as *mut std::ffi::c_void,
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
            http_create_request_fn: None,
            initialized: false,
            state,
            plugins,
            stream_forwarders: HashMap::new(),
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
        }
    }

    /// Exit the V8 isolate so another isolate can be entered on this thread.
    /// Must be called after `Runtime::builder().build()` when storing
    /// multiple runtimes on one thread.
    /// # Safety
    /// The isolate must not be used between `exit_isolate` and `enter_isolate`.
    pub fn exit_isolate(&mut self) {
        unsafe { self.isolate.exit(); }
    }

    /// Enter the V8 isolate before dispatching requests.
    /// Must be paired with `exit_isolate` after dispatch is done.
    /// # Safety
    /// Only one isolate can be entered at a time per thread.
    pub fn enter_isolate(&mut self) {
        unsafe { self.isolate.enter(); }
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
        {
            let mut rt = self_ref.borrow_mut();
            rt.set_pump_notify(notify_tx);
        }

        let rt = self_ref.clone();
        compio::runtime::spawn(async move {
            crate::panic_util::guard("pump_loop", async move {
                Self::pump_loop(rt, notify_rx).await;
            }).await;
        })
        .detach();
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
                    || !s.spawned_fetches.is_empty()
                    || !s.ready_timers.is_empty()
                    || !s.outbound_streams.is_empty()
            };

            if needs_drain {
                let mut rt = runtime.borrow_mut();
                rt.enter_isolate();
                rt.drain_new_tasks_into(&mut work);
                rt.flush_outbound_streams();
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
                    rt.flush_outbound_streams();
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

            // Load polyfills and the user's entry module. The returned global
            // is the entry module's Namespace Object; the kernel reads
            // `default.fetch` directly off it (no more `__rpc` reach-through).
            let namespace = load_polyfills_and_modules(scope, modules, &self.plugins);

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
                Err(e) => eprintln!("[cpu-timer] Failed: {e}"),
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

    /// Dispatch an RPC request synchronously into V8. Returns the result
    /// immediately for sync handlers (ping, fib, promiseChain, uuid, crypto).
    /// For async handlers that produce a pending promise, fires ready timers
    /// and checks settlement. Returns an error if the promise remains pending
    /// (truly async ops like fetch are not yet supported in channel-free mode).
    ///
    /// `method` is the URL-path-style method name (e.g. `"ping"` or
    /// `"src/index/ping"`). `args_json` is a JSON array of positional args.
    // -----------------------------------------------------------------------
    // Kernel dispatch primitive — call_fetch_handler
    // -----------------------------------------------------------------------

    /// Kernel's sole dispatch primitive. Invokes the user's
    /// `export default { fetch(request, env, ctx) }` handler and classifies
    /// the result as a `FetchOutcome`.
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
        // Stash the env JSON on state so the `__zs_env` native op returns the
        // same payload as the second arg of `fetch(req, env, ctx)`. Must run
        // BEFORE `ensure_initialized` so the `zeroship` module's top-level
        // `const env = Object.freeze(__zs_env())` — evaluated exactly once at
        // module-load during the first `ensure_initialized` call — captures
        // the real env rather than the default `{}`. Subsequent requests still
        // update `env_json` here so synchronous op reads from the handler see
        // the current request's env (not the previous one's) in case user
        // code calls `__zs_env()` directly.
        self.state.borrow_mut().set_env_snapshot(env);

        self.ensure_initialized(modules);

        if self.fetch_handler_fn.is_none() {
            return crate::FetchOutcome::Response {
                status: 404,
                headers: vec![("content-type".into(), "application/json".into())],
                body: r#"{"message":"No default.fetch handler exported","name":"Error"}"#.into(),
                logs: vec![],
            };
        }
        if self.http_create_request_fn.is_none() {
            return crate::FetchOutcome::Response {
                status: 500,
                headers: vec![("content-type".into(), "application/json".into())],
                body: r#"{"message":"HTTP request helper not compiled","name":"Error"}"#.into(),
                logs: vec![],
            };
        }

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

        // Serialize headers once for the HTTP_CREATE_REQUEST_JS helper
        // (which JSON.parses into a header array).
        let headers_json = serde_json::to_string(headers).unwrap_or_else(|_| "[]".into());
        let env_json = env.as_json().to_string();

        self.arm_cpu_timer();
        // `Ok` carries a fully-classified DispatchResult (HttpResponse /
        // ErrorValue / Error). The `Err` arm is a still-pending promise
        // that the outer match turns into 501 until Task B4 wires async.
        //
        // Using DispatchResult here (not Result<ResponseInfo, String>)
        // preserves `err.status` / `err.name` / `err.stack` from user
        // throws. See `call_fetch_inner` for the classification logic —
        // it mirrors `dispatch_request` in `dispatch.rs` for consistency.
        let dispatch_result: Result<DispatchResult, v8::Global<v8::Promise>> =
            enter_v8!(self, |scope| {
                let undefined = v8::undefined(scope).into();

                // 1. Construct JS Request via the same helper dispatch_http uses.
                let create_fn = v8::Local::new(scope, self.http_create_request_fn.as_ref().unwrap());
                let method_val = v8::String::new(scope, method).unwrap().into();
                let url_val = v8::String::new(scope, url).unwrap().into();
                let headers_val = v8::String::new(scope, &headers_json).unwrap().into();
                let body_val = v8::String::new(scope, body).unwrap().into();

                let request_opt = create_fn.call(scope, undefined, &[method_val, url_val, headers_val, body_val]);
                if request_opt.is_none() {
                    Ok(DispatchResult::Error("Failed to construct Request object".to_string()))
                } else {
                    let request = request_opt.unwrap();

                    // 2. Build env — JSON.parse the snapshot. `{}` for empty.
                    let env_src = v8::String::new(scope, &env_json).unwrap();
                    let env_val: v8::Local<v8::Value> = v8::json::parse(scope, env_src)
                        .unwrap_or_else(|| v8::Object::new(scope).into());

                    // 3. Build ctx — minimal stub. `waitUntil` swallows any
                    //    rejection via `.catch(() => {})` so we don't leak
                    //    unhandled-rejection warnings while real wiring lands
                    //    in Task C3; `passThroughOnException` is a true no-op.
                    let ctx_obj = v8::Object::new(scope);
                    let wu_key = v8::String::new(scope, "waitUntil").unwrap();
                    let wu_fn = v8::Function::new(scope, wait_until_noop_callback).unwrap();
                    ctx_obj.set(scope, wu_key.into(), wu_fn.into());
                    let pt_key = v8::String::new(scope, "passThroughOnException").unwrap();
                    let pt_fn = v8::Function::new(scope, pass_through_on_exception_noop_callback).unwrap();
                    ctx_obj.set(scope, pt_key.into(), pt_fn.into());
                    let ctx_val: v8::Local<v8::Value> = ctx_obj.into();

                    // 4. Call module.default.fetch(request, env, ctx)
                    let handler = v8::Local::new(scope, self.fetch_handler_fn.as_ref().unwrap());
                    call_fetch_inner(scope, handler, undefined, request, env_val, ctx_val)
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
            Ok(DispatchResult::ErrorValue { message, name, stack, status }) => {
                // Handler threw (or returned a rejected promise). Honor
                // `err.status` so `throw new HttpError(404)` yields 404,
                // not the previous hardcoded 500.
                self.clear_executing_request();
                self.discard_request_state(request_id);
                crate::FetchOutcome::Response {
                    status,
                    headers: vec![("content-type".into(), "application/json".into())],
                    body: crate::dispatch::build_error_body(&message, &name, stack.as_deref()),
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
                    body: crate::dispatch::build_error_body(&msg, "Error", None),
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
                self.clear_executing_request();
                self.store_fetch_pending(request_id, promise, ctx, cpu_elapsed, wall_start)
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
    ) -> crate::FetchOutcome {
        let (tx, rx) = channel::result_slot();

        self.pending_requests.insert(request_id, PendingRequest {
            id: request_id,
            promise,
            reply_fetch: tx,
            cpu_accumulated,
            wall_start,
            cancel: ctx.cancel.clone(),
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

                // Attach the writer directly to the stream state so future
                // enqueue() calls from JS write straight to the TCP-bound
                // channel — no buffer, no pump cycle. (Mirrors
                // `build_http_outcome`.)
                {
                    let mut s = self.state.borrow_mut();
                    if let Some(stream) = s.streams.get_mut(&stream_id) {
                        for chunk in stream.buffer.drain(..) {
                            let _ = writer.push(chunk);
                        }
                        if stream.closed {
                            writer.close();
                        } else {
                            stream.direct_writer = Some(writer);
                        }
                    } else {
                        writer.close();
                    }
                }
                crate::FetchOutcome::Stream { status, headers, body_reader: reader, logs }
            }
            ResponseInfo::WebSocket { ws_id, headers } => {
                crate::FetchOutcome::WebSocketUpgrade { ws_id, headers }
            }
        }
    }

    /// Drain newly spawned ops/timers/fetches from RuntimeState into the
    /// external `AsyncWork` (for the pump task).
    pub fn drain_new_tasks_into(&mut self, work: &mut AsyncWork) {
        // Fast path
        {
            let s = self.state.borrow();
            if s.spawned_ops.is_empty()
                && s.spawned_timers.is_empty()
                && s.spawned_fetches.is_empty()
                && s.ready_timers.is_empty()
            {
                return;
            }
        }

        // Drain spawned fetches
        let fetches: Vec<crate::state::FetchRequest> = {
            self.state.borrow_mut().spawned_fetches.drain(..).collect()
        };
        for fetch_req in fetches {
            // execute_fetch now needs SharedState because it spawns a
            // detached body-reader task that pushes `OpResult::StreamChunk`
            // entries back into `state.spawned_ops`. The task outlives the
            // header-resolve future, so it can't capture `self` directly.
            let future = crate::fetch::execute_fetch(fetch_req, self.state.clone());
            work.pending_ops.push(future);
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

    /// Move buffered chunks from RuntimeState.streams → StreamForwarder → StreamWriter.
    /// Must be called after any V8 execution that may have called __streams.enqueue().
    pub fn flush_outbound_streams(&mut self) {
        let outbound_ids: Vec<u32> = {
            self.state.borrow().outbound_streams.iter().copied().collect()
        };
        for stream_id in outbound_ids {
            let chunks: Vec<Vec<u8>> = {
                let mut s = self.state.borrow_mut();
                if let Some(stream) = s.streams.get_mut(&stream_id) {
                    stream.buffer.drain(..).collect()
                } else {
                    continue;
                }
            };
            if let Some(forwarder) = self.stream_forwarders.get_mut(&stream_id) {
                for chunk in chunks {
                    forwarder.try_forward(chunk);
                }
            }

            // Check if stream was closed
            let is_closed = {
                let s = self.state.borrow();
                s.streams.get(&stream_id).map(|st| st.closed).unwrap_or(true)
            };
            if is_closed {
                if let Some(forwarder) = self.stream_forwarders.remove(&stream_id) {
                    forwarder.writer.close();
                }
                self.state.borrow_mut().outbound_streams.remove(&stream_id);
            }
        }
    }

    /// Handle an async event from the pump (op completed or timer fired).
    /// Enters V8 briefly to resolve the op/timer, checks settled promises,
    /// and sends results via oneshot channels.
    pub fn handle_async_event(&mut self, event: AsyncEvent, work: &mut AsyncWork) {
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
            OpResult::StreamChunk { stream_id, data, done } => {
                // Fast path: if there's a stream forwarder, send directly (no V8 entry)
                if let Some(forwarder) = self.stream_forwarders.get_mut(&stream_id) {
                    if !data.is_empty() {
                        forwarder.try_forward(data);
                    }
                    if done {
                        // Signal completion to the reader, then remove
                        forwarder.writer.close();
                        self.stream_forwarders.remove(&stream_id);
                    }
                } else {
                    // Slow path: push into V8 ReadableStream
                    self.arm_cpu_timer();
                    enter_v8!(self, |scope| {
                        crate::streams::push_stream_chunk(scope, &self.state, stream_id, &data, done);
                    });
                    self.disarm_cpu_timer();
                    self.check_v8_terminated();
                }
            }
            OpResult::Cancelled => {}
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
        // ctx). Keeping these dropped together avoids "logs freed, user
        // still resident" asymmetries that otherwise leak memory for
        // long-lived workers.
        s.per_request_user.remove(&request_id);
        s.request_ctx_by_id.remove(&request_id);
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
            eprintln!(
                "[runtime] pump CPU budget exceeded: {:.1}% over {:.1}s — terminating isolate",
                fraction * 100.0,
                wall.as_secs_f64()
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
    /// Uses cached V8 handles (resolved at accept time) for zero-lookup dispatch.
    pub fn enter_v8_for_ws_message(&mut self, ws_id: u32, data: &str) {
        // Borrow cached handles before entering V8 (can't borrow state inside enter_v8!).
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
                // Fallback to dynamic lookup (shouldn't happen in normal flow).
                let data_val: v8::Local<v8::Value> = v8::String::new(scope, data).unwrap().into();
                call_ws_method(scope, ws_id, "_onMessage", &[data_val]);
            }
        });
    }

    /// Enter V8 to deliver a WebSocket close to the server-side WebSocket.
    /// Uses cached V8 handles for zero-lookup dispatch.
    pub fn enter_v8_for_ws_close(&mut self, ws_id: u32, code: u16, reason: &str) {
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
            let result = http::extract_settled_result(scope, &req.promise);
            Some((id, req, result))
        })
        .collect()
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
        return Ok(DispatchResult::ErrorValue {
            message: crate::dispatch::v8_exception_to_message(scope, exc_local),
            name: crate::dispatch::v8_exception_to_name(scope, exc_local),
            stack: crate::dispatch::v8_exception_to_stack(scope, exc_local),
            status: crate::dispatch::v8_exception_to_status(scope, exc_local).unwrap_or(500),
        });
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
                Ok(DispatchResult::ErrorValue {
                    message: crate::dispatch::v8_exception_to_message(scope, exc),
                    name: crate::dispatch::v8_exception_to_name(scope, exc),
                    stack: crate::dispatch::v8_exception_to_stack(scope, exc),
                    status: crate::dispatch::v8_exception_to_status(scope, exc).unwrap_or(500),
                })
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
