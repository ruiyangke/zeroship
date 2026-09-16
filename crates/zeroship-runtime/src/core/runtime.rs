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

use zeroship_core::app_id::AppId;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::rc::{Rc, Weak};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

use futures::stream::FuturesUnordered;

use crate::init::init_v8;
use crate::http::{self, ResponseInfo, SettledResult};
use crate::modules::ModuleEntry;
use crate::plugin::NativePlugin;
use super::startup::{StartupState, WaitingRequest};

#[path = "runtime_startup.rs"]
mod startup_driver;
use crate::state::{
    DispatchResult, OpResult, ResolveValue, RuntimeState, SharedState, SpawnedTimer,
    TimerResult,
};
use crate::transport::net_policy::NetPolicy;

use crate::channel::{
    self, CancelFlag, ResultSender,
};

/// Specifier of the host-only module that owns durable workflow replay.
///
/// `WorkflowBinding` in `zeroship-workflow-v8` supplies the source through
/// `NativePlugin::host_javascript_modules`; the runtime knows only this name
/// and the `dispatch` export it calls. A runtime that registers no plugin
/// supplying it cannot dispatch workflows, and creator code cannot import it.
pub const WORKFLOW_DISPATCH_MODULE: &str = "zeroship:workflows/dispatch";

/// Count of near-heap-limit callback invocations across every isolate in this
/// process, since start. Monotonic; never reset.
///
/// Exposed because the per-isolate hit counter lives behind a raw pointer that
/// is deliberately leaked for the isolate's lifetime, so nothing outside the
/// callback can read it. Without a process-wide count, a heap cap that fails to
/// bound an isolate is indistinguishable from one V8 never consults - and those
/// two have different fixes.
static HEAP_LIMIT_CALLBACK_HITS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Read the process-wide near-heap-limit callback count. See
/// [`HEAP_LIMIT_CALLBACK_HITS`]. Because it is process-wide and monotonic, a
/// caller comparing before/after must take a baseline rather than expect zero.
#[must_use]
pub fn heap_limit_callback_hits() -> u64 {
    HEAP_LIMIT_CALLBACK_HITS.load(std::sync::atomic::Ordering::Relaxed)
}

/// A snapshot of V8's own view of this isolate's heap: `(used, limit)` bytes.
///
/// `limit` is what V8 believes the cap to be, which is NOT necessarily the
/// value passed to `heap_limit_mb`: V8 clamps a `max_old_generation_size`
/// below its own floor, and the near-heap-limit callback raises the live limit
/// on each hit. Comparing the two answers "did the configured cap take effect"
/// separately from "was it enforced".
#[must_use]
pub fn heap_used_and_limit(runtime: &Runtime) -> (usize, usize) {
    let mut inner = runtime.inner.borrow_mut();
    let stats = inner.isolate.get_heap_statistics();
    (stats.used_heap_size(), stats.heap_size_limit())
}

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

/// Thread-safe, permanent interruption of an exclusive runtime. The owning
/// thread must still quarantine and join native work through `Runtime::shutdown`.
#[derive(Clone)]
pub struct RuntimeInterrupt {
    handle: v8::IsolateHandle,
    cancelled: Arc<AtomicBool>,
}
impl std::fmt::Debug for RuntimeInterrupt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeInterrupt").finish_non_exhaustive()
    }
}
impl RuntimeInterrupt {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.handle.terminate_execution();
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

impl Default for AsyncWork {
    fn default() -> Self {
        Self::new()
    }
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

/// How many zero-delay timer callbacks one pump pass may fire before it hands
/// control back to `pump_loop`. See [`RuntimeInner::fire_ready_timers_pump`].
///
/// `setTimeout(fn, 0)` is a YIELD, not a delay, and this bound must not turn it
/// into one. 64 is chosen to sit far above what real programs queue in a single
/// turn — promise-resolution ladders, `await new Promise(r => setTimeout(r, 0))`
/// hops and the like run one to a handful of zero-delay timers per turn, so
/// they drain in the first pass and observe byte-for-byte the behaviour they
/// did when the drain was unbounded. It only bites a callback chain that
/// re-arms itself faster than it is consumed, which is exactly the runaway the
/// bound exists to catch.
///
/// It can be this small because crossing a pass boundary is cheap: while
/// `ready_timers` is non-empty `pump_loop` waits only up to
/// [`READY_TIMER_PASS_TICK`] before re-entering PHASE 1, so a boundary costs one
/// isolate enter/exit plus one reactor turn rather than an unbounded park.
/// Measured at roughly 7 us per boundary against a pass of 64 callbacks. What
/// the boundary buys is that `bill_pump_cpu` runs, pending I/O gets looked at,
/// and other tasks on the thread get a scheduling slot.
///
/// The bound is on CALLBACK COUNT, not on time, so the wall-clock grip a runaway
/// keeps is `64 x per-callback cost`. Per-callback cost is what the CPU timer
/// armed around each callback bounds, so the two limits compose; lowering this
/// constant tightens the grip at the cost of paying the boundary more often.
const MAX_READY_TIMERS_PER_PASS: usize = 64;

/// Upper bound on how long the pump waits for I/O between two zero-delay timer
/// passes. Only reached when a chain outran [`MAX_READY_TIMERS_PER_PASS`], i.e.
/// never on the ordinary "a handful of `setTimeout(fn, 0)` per turn" path.
///
/// It has to be a real, non-zero deadline. `compio::time::sleep(Duration::ZERO)`
/// is NOT a yield: `TimerRuntime::insert` discards any deadline that is already
/// in the past and the future completes without ever returning `Pending`, so a
/// loop built on it never lets the executor reach `Runtime::poll_with` — the
/// only place completed I/O is reaped and elapsed timers are woken. Parking on
/// a genuine deadline instead guarantees exactly one reactor turn per pass.
///
/// 50 us is chosen to be negligible against a 64-callback pass while still
/// being far enough in the future that it cannot be rounded away. It is a
/// ceiling, not a delay: the same park resolves the instant any op, timer, or
/// pump notification becomes ready, so the wait is only ever paid when the
/// isolate genuinely has nothing else to do but run more zero-delay timers.
const READY_TIMER_PASS_TICK: Duration = Duration::from_micros(50);

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
    app_id: Option<AppId>,
}

/// Test-only strong-count probe over the inner `Rc<RefCell<RuntimeInner>>`.
/// Holds only a `Weak`, so it never keeps the inner alive. `strong_count()`
/// counts the remaining strong references (cache handle, in-flight request
/// handles, and — pre-fix — the leaked pump task). Returned by
/// [`Runtime::into_inner_probe_for_test`].
#[doc(hidden)]
pub struct InnerProbe(Weak<RefCell<RuntimeInner>>);

impl InnerProbe {
    /// Number of strong references still alive to the inner runtime.
    /// `0` means `RuntimeInner` (and its V8 isolate) has been dropped.
    pub fn strong_count(&self) -> usize {
        self.0.strong_count()
    }
}

/// RAII guard for an explicit isolate lease.
///
/// While at least one lease is alive, worker LRU eviction must treat the
/// runtime as un-evictable. The guard holds a `Runtime` clone so the isolate
/// itself also remains alive for the trusted caller holding the lease.
#[must_use = "dropping the guard releases the isolate lease"]
pub struct RuntimeLease {
    runtime: Runtime,
}

impl Drop for RuntimeLease {
    fn drop(&mut self) {
        let state = self.runtime.state();
        let mut s = state.borrow_mut();
        s.isolate_lease_count = s.isolate_lease_count.saturating_sub(1);
    }
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

    /// Obtain a permanent interrupt for a trusted host's deadline watchdog.
    /// This does not grant another thread access to the isolate or its state.
    #[must_use]
    pub fn interrupt_handle(&self) -> RuntimeInterrupt {
        let inner = self.inner.borrow();
        RuntimeInterrupt {
            handle: inner.isolate.thread_safe_handle(),
            cancelled: inner.host_interrupt.clone(),
        }
    }

    /// Module list this runtime was built with.
    pub fn modules(&self) -> &[ModuleEntry] {
        self.modules.as_ref().as_slice()
    }

    /// Initialize the module graph without invoking the app's `fetch` handler.
    ///
    /// The worker calls this at load time so corrupt boot artifacts (including
    /// a present-but-invalid `manifest.runtime_descriptor`) reject the load
    /// instead of producing a stored schema-less isolate that fails later.
    pub async fn initialize(&self, env: &crate::EnvSnapshot) -> Result<(), String> {
        let ready = {
            let mut inner = self.inner.borrow_mut();
            inner.enter_isolate();
            let result = inner.initialize_modules(self.modules.as_slice(), env);
            inner.exit_isolate();
            result?
        };
        if ready { return Ok(()); }
        self.start_pump();
        futures::future::poll_fn(|cx| {
            let mut inner = self.inner.borrow_mut();
            match inner.startup_result() {
                Ok(true) => std::task::Poll::Ready(Ok(())),
                Err(error) => std::task::Poll::Ready(Err(error)),
                Ok(false) => {
                    if !inner.startup_waiters.iter().any(|waker| waker.will_wake(cx.waker())) {
                        inner.startup_waiters.push(cx.waker().clone());
                    }
                    std::task::Poll::Pending
                }
            }
        }).await
    }

    /// Multi-tenant identity, if the builder was supplied one.
    /// Used by `crate::rpc::abort` to key the in-flight controller
    /// registry by `(app_id, request_id)`.
    pub fn app_id(&self) -> Option<&AppId> {
        self.app_id.as_ref()
    }

    /// Test-only: consume this handle and return a strong-count probe for
    /// the inner `Rc<RefCell<RuntimeInner>>`. Dropping the handle here
    /// releases the caller's strong reference, so the probe afterwards
    /// reflects only the references held *elsewhere* (e.g. the detached
    /// pump task). Used by the eviction-leak regression test to assert the
    /// inner is actually dropped once the handle goes away — which only
    /// holds if the pump holds a `Weak`, not a strong `Rc`. (The inner type
    /// is `pub(crate)`, so the probe type-erases it behind a count.)
    #[doc(hidden)]
    pub fn into_inner_probe_for_test(self) -> InnerProbe {
        InnerProbe(Rc::downgrade(&self.inner))
    }

    /// Run `f` inside this isolate's HandleScope + Context. Wraps the
    /// same primitive `enter_v8!` uses but exposes it to callers that
    /// need to invoke V8 APIs from outside `call_fetch_handler`.
    ///
    /// Used by the worker's eviction path (see
    /// `crates/zeroship-worker/src/cache.rs::evict_lru`) to walk the abort
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
            crate::core::init::perform_microtask_checkpoint(scope);
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

    /// Whether development startup has failed after touching the isolate.
    ///
    /// Creator evaluation and plugin finalization can run arbitrary callbacks
    /// before they fail, so the dev host must replace this runtime rather than
    /// retrying startup in the same isolate. A failed later entry generation
    /// does not set this state: the last published snapshot remains valid and
    /// another invalidation can retry through its existing loader.
    pub(crate) fn dev_runtime_requires_fresh_start(&self) -> bool {
        let inner = self.inner.borrow();
        inner.dev_entry_factory.is_some() && matches!(inner.startup, StartupState::Failed(_))
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
        body: impl AsRef<[u8]>,
        env: &crate::EnvSnapshot,
        ctx: crate::RequestCtx,
    ) -> crate::FetchOutcome {
        self.call_fetch_handler_with_user(method, url, headers, body, env, ctx, None)
    }

    /// Variant of [`Runtime::call_fetch_handler`] used by the worker once it
    /// has verified the gateway-issued `ZeroShip-User` envelope.
    // Public API consumed by crates/worker; bundling these into a request
    // struct would ripple across a crate outside this lint pass's scope.
    #[allow(clippy::too_many_arguments)]
    pub fn call_fetch_handler_with_user(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: impl AsRef<[u8]>,
        env: &crate::EnvSnapshot,
        ctx: crate::RequestCtx,
        user_json: Option<String>,
    ) -> crate::FetchOutcome {
        let body = body.as_ref();
        self.inner.borrow_mut().call_fetch_handler(
            self.modules.as_slice(),
            method,
            url,
            headers,
            body,
            env,
            ctx,
            user_json,
        )
    }

    /// Durable-workflow replay dispatch. Invokes the `dispatch` export of the
    /// host-only [`WORKFLOW_DISPATCH_MODULE`] against the creator entry's own
    /// namespace and returns the JSON `StepResult` object it produced.
    pub fn call_workflow_dispatch(
        &self,
        envelope_json: &str,
        env: &crate::EnvSnapshot,
        ctx: crate::RequestCtx,
    ) -> crate::WorkflowOutcome {
        self.inner
            .borrow_mut()
            .call_workflow_dispatch(self.modules.as_slice(), envelope_json, env, ctx)
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

    /// Permanently stop app dispatch and cancel this isolate's native work.
    /// Native task teardown retains the isolate until its futures are destroyed.
    /// Hosts may call this synchronously when a workflow loses its authority.
    pub fn quarantine(&self) {
        let tasks = self.state().borrow().tasks.clone();
        if !tasks.cancel() {
            return;
        }
        let queued = {
            let mut inner = self.inner.borrow_mut();
            inner.fail_startup("runtime has been quarantined".into());
            inner.advance_startup();
            for request in inner.pending_requests.values() {
                request.cancel.cancel();
            }
            inner.cleanup_cancelled_requests();
            let mut state = inner.state.borrow_mut();
            state.spawned_timers.clear();
            state.ready_timers.clear();
            std::mem::take(&mut state.spawned_ops)
        };
        drop(queued);
        self.close_native_sockets_for_eviction();
        if !tasks.is_idle() {
            let keep_alive = self.clone();
            // The supervisor is outside the cancelled group. It preserves V8
            // until pump and socket futures have dropped their native handles.
            compio::runtime::spawn(async move {
                tasks.join().await;
                drop(keep_alive);
            }).detach();
        }
    }

    /// Join native task teardown after permanently quarantining the isolate.
    /// Cancelling this wait leaves quarantine in force and a later call can join.
    pub async fn shutdown(&self) {
        self.quarantine();
        let tasks = self.state().borrow().tasks.clone();
        tasks.join().await;
    }

    /// Number of times the per-isolate idle-GC ticker has fired
    /// `low_memory_notification`. Increments on every GC hint; useful as
    /// a test-visible signal (the alternative — sampling V8 heap stats
    /// before/after — is flaky on a small heap).
    pub fn idle_gc_fire_count(&self) -> u64 {
        self.inner.borrow().idle_gc_fire_count.get()
    }

    /// Test-only: true while this isolate has fetch/RPC promises waiting for
    /// the pump to settle or cancel them.
    #[doc(hidden)]
    pub fn has_pending_requests_for_test(&self) -> bool {
        self.inner.borrow().has_pending_requests()
    }

    /// Hold an explicit lease that makes this isolate un-evictable by the
    /// worker cache until the returned guard is dropped.
    pub fn lease_isolate(&self) -> RuntimeLease {
        {
            let state = self.state();
            let mut s = state.borrow_mut();
            s.isolate_lease_count = s.isolate_lease_count.saturating_add(1);
        }
        RuntimeLease {
            runtime: self.clone(),
        }
    }

    /// Number of active isolate leases.
    pub fn isolate_lease_count(&self) -> u32 {
        self.state().borrow().isolate_lease_count
    }

    /// True when this runtime has at least one active isolate lease.
    pub fn is_isolate_leased(&self) -> bool {
        self.isolate_lease_count() > 0
    }

    /// Number of open native `node:net` sockets counted against this runtime.
    pub fn active_native_socket_count(&self) -> u32 {
        self.state().borrow().active_native_sockets
    }

    /// Last successful native socket activity timestamp, if any.
    pub fn last_native_socket_activity(&self) -> Option<Instant> {
        self.state().borrow().native_socket_last_activity
    }

    /// Gracefully close every native `node:net` socket before isolate eviction.
    ///
    /// This drives the same socket destroy path as JS `socket.destroy()`: the
    /// driver observes the destroyed flag and shuts down the underlying stream
    /// instead of having the runtime drop abruptly under an open DB socket.
    pub fn close_native_sockets_for_eviction(&self) -> usize {
        let state = self.state();
        crate::node::net::state::destroy_all_sockets(&state)
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
    app_id: Option<AppId>,
    meter: Option<Arc<zeroship_metering::Meter>>,
    net_policy: NetPolicy,
    /// Override for PHASE 2 of the egress evaluator. `None` leaves the
    /// platform's own `SystemResolver` in place.
    egress_resolver: Option<Rc<dyn crate::transport::egress::EgressResolver>>,
    js_driver_dsn_json: Option<String>,
    /// Idle-GC threshold override (ms). `None` → `DEFAULT_IDLE_GC_AFTER`.
    /// Lives on the builder (not `RuntimeLimits`) because it's a runtime
    /// scheduling knob, not a per-request cap.
    idle_gc_after_ms: Option<u64>,
    /// Host-supplied schema descriptor, validated and bound before creator evaluation.
    runtime_descriptor: Option<String>,
    validate_rpc_output: bool,
    dev_entry_loader: Option<String>,
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

    /// Bind lifecycle, native namespaces and metering to this app.
    /// Overrides `APP_ID` supplied through [`Self::env_vars`].
    pub fn app_id(mut self, id: AppId) -> Self {
        self.app_id = Some(id);
        self
    }

    /// Bind the process-wide infrastructure meter to this runtime's app.
    ///
    /// `node:net` uses this to stamp accepted outbound bytes into the fixed
    /// `egress_bytes` spend metric. The handle is built only when `app_id` is
    /// also set, preserving the server-injected attribution boundary.
    pub fn meter(mut self, meter: Arc<zeroship_metering::Meter>) -> Self {
        self.meter = Some(meter);
        self
    }

    /// Set the raw TCP policy for `node:net`. The default is
    /// `NetPolicy::Denied`, which makes `node:net` unresolvable.
    pub fn net_policy(mut self, policy: NetPolicy) -> Self {
        self.net_policy = policy;
        self
    }

    /// Replace PHASE 2 of the `node:net` egress evaluator - the DNS lookup and
    /// its timeout.
    ///
    /// Not a creator-app capability and not reachable from JS: it is a
    /// trusted-Rust construction knob, like `net_policy`. It exists so a test
    /// can see whether the SHIPPED connect path resolved a name, which is the
    /// only observable form the DNS gate has.
    pub fn egress_resolver(
        mut self,
        resolver: Rc<dyn crate::transport::egress::EgressResolver>,
    ) -> Self {
        self.egress_resolver = Some(resolver);
        self
    }

    /// Seed the Trusted JS-driver command channel for the migrate runtime.
    ///
    /// This is not a creator-app capability. Normal worker/CLI runtimes do not
    /// call this builder method, leaving `RuntimeState::js_driver` empty and
    /// the `__zsDriver*` globals absent.
    pub fn js_driver_dsn_json(mut self, dsn_json: String) -> Self {
        self.js_driver_dsn_json = Some(dsn_json);
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

    /// Set the host-supplied runtime schema descriptor.
    pub fn runtime_descriptor(mut self, descriptor: Option<String>) -> Self {
        self.runtime_descriptor = descriptor;
        self
    }

    /// Select a trusted dev-host export that constructs an entry loader.
    /// Production hosts leave this unset; creator exports cannot enable it.
    pub fn dev_entry_loader(mut self, factory_export: impl Into<String>) -> Self {
        self.dev_entry_loader = Some(factory_export.into());
        self
    }

    /// Enable output validator checks independently of creator globals.
    pub fn validate_rpc_output(mut self, enabled: bool) -> Self {
        self.validate_rpc_output = enabled;
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
        let mut inner = RuntimeInner::new_with_plugins(
            self.env_vars,
            limits.cpu_limit,
            limits.wall_timeout,
            limits.heap_limit_bytes,
            self.plugins,
            app_id.clone(),
            self.meter,
            self.net_policy,
            self.egress_resolver,
            self.js_driver_dsn_json,
            idle_gc_after,
            self.runtime_descriptor,
        );
        inner.state.borrow_mut().validate_rpc_output = self.validate_rpc_output;
        inner.dev_entry_factory = self.dev_entry_loader;
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
    /// A native procedure call is waiting for its loader or handler promise.
    Rpc,
    /// Promise came from the host workflow bridge — resolved value is the
    /// `StepResult` object its `dispatch` export returns.
    Workflow,
}

enum PendingReply {
    Fetch(ResultSender<Result<crate::SettledFetch, DispatchError>>),
    Workflow(ResultSender<Result<crate::SettledWorkflow, DispatchError>>),
}

/// Tracking info for an in-flight request whose dispatch returned a Promise.
struct PendingRequest {
    #[allow(dead_code)]
    id: u64,
    promise: v8::Global<v8::Promise>,
    /// Reply slot for the pending path. Fetch/RPC requests settle to
    /// `SettledFetch`; durable-workflow replay settles to `SettledWorkflow`.
    reply: PendingReply,
    cpu_accumulated: Duration,
    wall_start: Instant,
    cancel: CancelFlag,
    origin: PendingOrigin,
    rpc_call: Option<crate::rpc::dispatch::RpcCall>,
    /// Host cancellation and eviction ownership until settlement or transfer
    /// to the response forwarder.
    rpc_lifetime: Option<crate::rpc::lifetime::RequestLifetime>,
}

fn send_pending_error(req: PendingRequest, error: impl Into<DispatchError>) {
    let error = error.into();
    match req.reply {
        PendingReply::Fetch(tx) => tx.send(Err(error)),
        PendingReply::Workflow(tx) => tx.send(Err(error)),
    }
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
        crate::core::init::perform_microtask_checkpoint($scope);
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
    /// Published HTTP and RPC targets, captured together after validation.
    /// A dispatch holds this snapshot while later loading can replace it.
    pub(crate) application: Option<Rc<super::application_entry::ApplicationEntry>>,

    /// The creator entry module, retained from compilation so startup can
    /// publish its namespace without evaluating the entry a second time.
    pub(crate) creator_entry: Option<v8::Global<v8::Module>>,
    /// The creator entry's module namespace, published only once startup has
    /// evaluated it. Workflow replay passes this to the host bridge, so the
    /// module instance it sees is the one request dispatch imported.
    pub(crate) creator_namespace: Option<v8::Global<v8::Value>>,
    startup: StartupState,
    dev_entry_factory: Option<String>,
    dev_entry_loader: Option<super::dev_entry::DevEntryLoader>,
    dev_entry_cpu: Duration,
    dev_entry_cpu_started: Option<Duration>,
    dev_entry_cpu_generation: Option<u64>,
    dev_entry_cpu_running: bool,
    startup_cpu: Duration,
    startup_cpu_started: Option<Duration>,
    startup_waiters: Vec<std::task::Waker>,
    waiting_startup_requests: Vec<WaitingRequest>,
    pub(crate) state: SharedState,
    /// Plugins registered on the runtime at boot.
    plugins: Vec<Arc<dyn NativePlugin>>,

    pending_requests: HashMap<u64, PendingRequest>,
    #[cfg(feature = "runtime_native_websocket")]
    subscriptions: crate::rpc::subscription::Subscriptions,
    next_direct_request_id: u64,

    /// Notification channel to wake the pump task when new work is added.
    /// dispatch_start sends a signal here after spawning timers/ops so the
    /// pump doesn't have to poll on a 1ms sleep.
    pump_notify_tx: Option<futures::channel::mpsc::Sender<()>>,

    /// CPU budget for startup and request execution.
    cpu_limit: Option<Duration>,
    /// Startup wall deadline. Dispatch hosts use the same configured limit.
    wall_timeout: Option<Duration>,

    /// Set by `near_heap_limit_callback` when it terminates this isolate for
    /// exceeding its heap cap. Shared with the leaked callback data block.
    ///
    /// This exists because `Isolate::is_execution_terminating` CANNOT answer
    /// the question after the fact: V8 clears the terminating state once the
    /// termination exception has unwound out of JS, which has already happened
    /// by the time the dispatch path regains control. Measured - the check
    /// reads `false` on a dispatch whose callback fired the full five times.
    /// So the dispatch cannot ask V8 whether it was heap-terminated; the
    /// callback has to leave a note.
    terminated_note: Arc<std::sync::atomic::AtomicBool>,

    /// Which limit set `terminated_note`. Only the CPU timer writes `true`
    /// here, so an unset value with the note set means the heap callback.
    cpu_note: Arc<std::sync::atomic::AtomicBool>,

    /// Host interruption is permanent; limit recovery cannot resume app code.
    host_interrupt: Arc<AtomicBool>,

    /// Cause of the most recent detected termination, set by
    /// `check_v8_terminated` so call sites report the right limit.
    last_termination_was_heap: bool,

    /// POSIX CPU timer — kills V8 on CPU limit exceeded (Linux only).
    #[cfg(target_os = "linux")]
    cpu_timer: Option<crate::cpu_timer::CpuTimer>,
    /// Whether the CPU timer is currently armed.
    #[cfg(target_os = "linux")]
    cpu_timer_active: bool,

    /// Cumulative wall time this Runtime spent in the pump's event-handling
    /// window (op resolves, timer callbacks, stream pushes) since the last
    /// budget window reset. Compared against elapsed wall time to detect apps
    /// that monopolize the thread via long-running `setInterval` callbacks or
    /// promise chains — situations the per-REQUEST cpu timer doesn't catch
    /// because the work isn't attributed to any single request.
    ///
    /// ENFORCEMENT ONLY. Billing reads none of this; see `pump_cpu_unmetered`
    /// and `bill_pump_cpu`, which run on the thread CPU clock and additionally
    /// cover the pump's PHASE 1 window that this counter never sees.
    pump_cpu_accumulated: Duration,
    /// Wall-clock start of the current budget window.
    pump_wall_start: Instant,

    /// Sub-microsecond remainder of pump CPU not yet handed to the meter.
    ///
    /// The `cpu_us` metric is integral microseconds, so a pump slice shorter
    /// than 1 us would truncate to zero and vanish. Carrying the remainder
    /// forward makes the billed total exact to the microsecond no matter how
    /// the work is sliced — an app that resolves ten thousand sub-microsecond
    /// promise continuations is billed the same as one that does the identical
    /// work in a single slice. Distinct from `pump_cpu_accumulated`, which is
    /// the enforcement window's counter and is reset wholesale every 10 s;
    /// nothing is ever read back out of this one, it only holds the change.
    pump_cpu_unmetered: Duration,

    /// Multi-tenant identity. When `Some`, the RPC fast-path registers
    /// every in-flight `AbortController` with `crate::rpc::abort` keyed
    /// by `(app_id, request_id)` so the worker's eviction sweep can
    /// fire them. `None` for single-tenant callers.
    app_id: Option<AppId>,

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
        for request in self.waiting_startup_requests.drain(..) {
            request.reply.send(Err("Runtime dropped during startup".into()));
        }
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
    // Bundling these into a params struct is a real design decision, not
    // a mechanical lint fix, so it's left for a deliberate follow-up.
    #[allow(clippy::too_many_arguments)]
    fn new_with_plugins(
        env_vars: HashMap<String, String>,
        cpu_limit: Option<Duration>,
        wall_timeout: Option<Duration>,
        heap_limit_bytes: Option<usize>,
        plugins: Vec<Arc<dyn NativePlugin>>,
        app_id: Option<AppId>,
        meter: Option<Arc<zeroship_metering::Meter>>,
        net_policy: NetPolicy,
        egress_resolver: Option<Rc<dyn crate::transport::egress::EgressResolver>>,
        js_driver_dsn_json: Option<String>,
        idle_gc_after: Duration,
        runtime_descriptor: Option<String>,
    ) -> Self {
        init_v8();

        // Default 128 MB per isolate. Control-plane can tune per-app:
        // free-tier → 64 MB, paid → 256 MB. The old hardcoded 512 MB
        // meant MAX_ISOLATES=200 × 512 MB × threads could claim 100+ GB.
        const DEFAULT_HEAP: usize = 128 * 1024 * 1024;
        let heap_max = heap_limit_bytes.unwrap_or(DEFAULT_HEAP);
        let params = v8::CreateParams::default().heap_limits(0, heap_max);
        let mut isolate = v8::Isolate::new(params);
        isolate.set_microtasks_policy(v8::MicrotasksPolicy::Explicit);

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
            /// Note left for the dispatch path, which cannot ask V8 after the
            /// fact - see `RuntimeInner::heap_terminated`.
            terminated: Arc<std::sync::atomic::AtomicBool>,
        }
        const MAX_HEAP_LIMIT_HITS: u32 = 5;
        let heap_terminated = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let heap_data = Box::into_raw(Box::new(HeapLimitData {
            hits: 0,
            handle: isolate.thread_safe_handle(),
            initial_limit: heap_max,
            terminated: Arc::clone(&heap_terminated),
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
            // Process-wide, monotonic. `d.hits` is per-isolate and behind a
            // raw pointer no observer can reach, so without this there is no
            // way to tell "V8 never consulted the cap" from "it fired and the
            // growth outran the termination" - the two have completely
            // different fixes.
            HEAP_LIMIT_CALLBACK_HITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
                d.terminated.store(true, std::sync::atomic::Ordering::Relaxed);
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
        let meter_handle = app_id
            .as_ref()
            .zip(meter)
            .map(|(id, meter)| zeroship_metering::MeterHandle::new(meter, id.clone()));
        let state: SharedState = Rc::new(RefCell::new(RuntimeState::new(
            env_vars,
            app_id.clone(),
            meter_handle,
        )));
        state.borrow_mut().set_net_policy(net_policy);
        if let Some(resolver) = egress_resolver {
            state.borrow_mut().set_egress_resolver(resolver);
        }
        if let Some(dsn_json) = js_driver_dsn_json {
            state.borrow_mut().js_driver = Some(crate::state::JsDriverState::new(dsn_json));
        }
        // Stash the bundled descriptor for validation and direct plugin binding
        // during native startup.
        state.borrow_mut().runtime_descriptor = runtime_descriptor;
        isolate.set_slot(state.clone());
        isolate.set_slot(crate::plugin::RuntimeAppIdentity(app_id.clone()));

        let context = {
            v8::scope!(let handle_scope, &mut isolate);
            let ctx = v8::Context::new(handle_scope, Default::default());
            v8::Global::new(handle_scope, ctx)
        };

        Self {
            isolate,
            context,
            application: None,
            creator_entry: None,
            creator_namespace: None,
            startup: StartupState::Uninitialized,
            dev_entry_factory: None,
            dev_entry_loader: None,
            dev_entry_cpu: Duration::ZERO,
            dev_entry_cpu_started: None,
            dev_entry_cpu_generation: None,
            dev_entry_cpu_running: false,
            startup_cpu: Duration::ZERO,
            startup_cpu_started: None,
            startup_waiters: vec![],
            waiting_startup_requests: vec![],
            state,
            plugins,
            pending_requests: HashMap::new(),
            #[cfg(feature = "runtime_native_websocket")]
            subscriptions: crate::rpc::subscription::Subscriptions::default(),
            next_direct_request_id: 1,
            pump_notify_tx: None,
            cpu_limit,
            wall_timeout,
            terminated_note: heap_terminated,
            cpu_note: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            host_interrupt: Arc::new(AtomicBool::new(false)),
            last_termination_was_heap: false,
            #[cfg(target_os = "linux")]
            cpu_timer: None,
            #[cfg(target_os = "linux")]
            cpu_timer_active: false,

            pump_cpu_accumulated: Duration::ZERO,
            pump_wall_start: Instant::now(),
            pump_cpu_unmetered: Duration::ZERO,
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
        if self_ref.borrow().pump_notify_tx.is_some() { return; }
        let tasks = self_ref.borrow().state.borrow().tasks.clone();
        let (notify_tx, notify_rx) = futures::channel::mpsc::channel::<()>(1);
        let idle_gc_after = {
            let mut rt = self_ref.borrow_mut();
            rt.set_pump_notify(notify_tx);
            rt.idle_gc_after
        };

        // The pump task must hold a *Weak* back-reference, not a strong
        // `Rc`. A strong clone here forms a reference cycle: the cache's
        // `IsolateEntry` owns the `Runtime` handle (one strong `Rc`), and
        // the detached pump task would own another. The pump loops forever,
        // so on LRU eviction — when the cache drops its `Runtime` — the
        // pump's strong ref keeps `RuntimeInner` alive, the isolate is
        // never disposed, and memory grows. Downgrading to `Weak` (matching
        // the idle-GC ticker below) lets eviction actually drop the inner;
        // the pump upgrades transiently per iteration and exits the moment
        // `upgrade()` returns `None`.
        let weak = Rc::downgrade(&self_ref);
        tasks.spawn(async move {
            crate::panic_util::guard("pump_loop", async move {
                Self::pump_loop(weak, notify_rx).await;
            }).await;
        });

        // Idle-GC ticker — sibling task with a Weak handle so isolate
        // teardown drops it without a join. `idle_gc_after == 0` opts
        // out (used by tests that don't want the timer at all).
        if !idle_gc_after.is_zero() {
            let weak = Rc::downgrade(&self_ref);
            tasks.spawn(async move {
                crate::panic_util::guard("idle_gc_ticker", async move {
                    Self::idle_gc_ticker(weak, idle_gc_after).await;
                }).await;
            });
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
        runtime: Weak<RefCell<Self>>,
        mut notify_rx: futures::channel::mpsc::Receiver<()>,
    ) {
        use futures::{FutureExt, StreamExt};
        let mut work = AsyncWork::new();

        loop {
            // Set when PHASE 1's drain hit its per-pass bound and left zero-delay
            // timers on the queue. Those are RUNNABLE work with no I/O behind
            // them, so this iteration must not park waiting for an event —
            // nothing would ever wake it (`setTimeout` does not `notify_pump`).
            let mut ready_timers_pending = false;
            let request_deadline;

            // Upgrade the Weak back-reference for this iteration's synchronous
            // V8 work. If it returns `None`, the `Runtime` handle has been
            // dropped (LRU eviction or shutdown) and `RuntimeInner` is gone —
            // the pump must exit so it doesn't resurrect a disposed isolate
            // and so its task can be reaped. The strong `Rc` is held only
            // across the synchronous phases below; it is dropped before the
            // idle `notify_rx` await so eviction during an idle window drops
            // the inner immediately rather than waiting for the next event.
            {
                let Some(runtime) = runtime.upgrade() else { return; };
                {
                    let mut rt = runtime.borrow_mut();
                    if rt.startup.is_pending()
                        || rt
                            .dev_entry_loader
                            .as_ref()
                            .is_some_and(|loader| loader.is_pending())
                        || matches!(rt.startup, StartupState::Failed(_))
                    {
                        rt.enter_isolate();
                        rt.advance_startup();
                        rt.exit_isolate();
                    }
                    if matches!(rt.startup, StartupState::Failed(_)) { return; }
                    let cancellation_cpu_start = crate::core::init::thread_cpu_time();
                    rt.cleanup_cancelled_requests();
                    rt.bill_pump_cpu(crate::core::init::thread_cpu_time().saturating_sub(cancellation_cpu_start));
                    crate::streams::response_forwarder::queue_cancellations(&rt.state, Instant::now());
                    request_deadline = rt.startup_deadline().into_iter()
                        .chain(rt.dev_entry_deadline())
                        .chain(rt.waiting_startup_requests.iter().filter_map(|request| {
                            rt.wall_timeout
                                .and_then(|limit| request.started.checked_add(limit))
                        }))
                        .chain(rt.pending_requests.values().filter_map(|request| {
                            request.rpc_lifetime.as_ref().and_then(|request| request.deadline)
                        }))
                        .chain(crate::streams::response_forwarder::next_deadline(&rt.state))
                        .min();
                }

                if runtime.borrow().host_interrupt.load(Ordering::Acquire) {
                    return;
                }

                // PHASE 1 — drain new spawned ops/timers/fetches + flush
                // outbound streams. Only enter the V8 isolate if there's
                // actually work to do: v8::Isolate::enter/exit aren't free
                // (TLS swap + scheduling slot manipulation), and in
                // steady-state "await an op, handle it, await another" the
                // drain phase finds nothing new. Checking the shared-state
                // sizes behind a short immutable borrow lets us skip this
                // entire block when it would be a no-op.
                let needs_drain = {
                    let rt = runtime.borrow();
                    let s = rt.state().borrow();
                    !s.spawned_ops.is_empty()
                        || !s.spawned_timers.is_empty()
                        || !s.ready_timers.is_empty()
                        || !s.forwarder_resumes.is_empty()
                        || s.js_driver.as_ref().is_some_and(|driver| {
                            driver.next_command_resolver.is_some()
                                && !driver.command_queue.is_empty()
                        })
                };

                if needs_drain {
                    // This window runs real app JS, not just bookkeeping:
                    // `drain_new_tasks_into` fires zero-delay timers inline
                    // (`setTimeout(fn, 0)` lands in `ready_timers`, never in
                    // `pending_timers`), and the forwarder/JS-driver services
                    // enter V8 too. Its CPU has to be billed like any other
                    // app CPU. It is deliberately NOT fed to
                    // `record_pump_cpu`: that budget is a safety mechanism
                    // with its own calibration, and widening what it polices
                    // is a behaviour change to make on its own terms, not a
                    // side effect of a billing fix.
                    //
                    // The drain is bounded (`MAX_READY_TIMERS_PER_PASS`), which
                    // is what makes the billing below reachable at all: an
                    // unbounded drain let a self-rescheduling `setTimeout(fn, 0)`
                    // chain hold this call forever, so `bill_pump_cpu` never ran
                    // and the loop burned a core for free. Anything the bound
                    // left behind is picked up by the next iteration.
                    let cpu_start = crate::core::init::thread_cpu_time();
                    let mut rt = runtime.borrow_mut();
                    rt.enter_isolate();
                    rt.drain_new_tasks_into(&mut work);
                    rt.service_forwarder_resumes(&mut work);
                    rt.service_js_driver_commands(&mut work);
                    rt.advance_startup();
                    rt.exit_isolate();
                    rt.bill_pump_cpu(
                        crate::core::init::thread_cpu_time().saturating_sub(cpu_start),
                    );
                    ready_timers_pending = !rt.state().borrow().ready_timers.is_empty();
                }
                // `runtime` (strong Rc) dropped here — not held across the
                // event await below.
            }

            let wake_deadline = match (ready_timers_pending, request_deadline) {
                (true, Some(deadline)) => Some(deadline.min(Instant::now() + READY_TIMER_PASS_TICK)),
                (true, None) => Some(Instant::now() + READY_TIMER_PASS_TICK),
                (false, deadline) => deadline,
            };
            let cancel_runtime = runtime.clone();
            let mut request_cancel = futures::future::poll_fn(move |cx| {
                let Some(runtime) = cancel_runtime.upgrade() else {
                    return std::task::Poll::Ready(());
                };
                let rt = runtime.borrow();
                for request in &rt.waiting_startup_requests {
                    request.ctx.cancel.register_waker(cx.waker());
                    if request.ctx.cancel.is_cancelled() {
                        return std::task::Poll::Ready(());
                    }
                }
                for request in rt.pending_requests.values() {
                    request.cancel.register_waker(cx.waker());
                    if request.cancel.is_cancelled() { return std::task::Poll::Ready(()); }
                }
                if crate::streams::response_forwarder::poll_cancellation(&rt.state, cx) {
                    return std::task::Poll::Ready(());
                }
                std::task::Poll::Pending
            }).fuse();
            let event = if let Some(deadline) = wake_deadline {
                // PHASE 1's drain hit its bound and left zero-delay timers
                // queued, so this iteration must come back here promptly. The
                // wait below is therefore the same select as the steady-state
                // one plus a `READY_TIMER_PASS_TICK` ceiling.
                //
                // It must not become an unbounded park. `setTimeout` pushes onto
                // `ready_timers` without touching `notify_rx`, so on the
                // `(false, false)` arm — a zero-delay chain and no I/O at all,
                // exactly the runaway shape — waiting on `notify_rx` alone would
                // be a wake that is never sent: a deadlock, not a delay.
                //
                // It must not become a busy loop either. Going straight back to
                // PHASE 1 without awaiting anything real would starve every
                // other task on this cooperative single-threaded executor,
                // including the handler task waiting for the response the chain
                // is holding up, and would never let the executor reach
                // `Runtime::poll_with`, where completed I/O is reaped. Parking
                // on a real deadline yields both: one reactor turn per pass, and
                // any op/timer that completes in the meantime is picked up here
                // and taken through PHASE 2 on its normal path.
                let mut tick = compio::time::sleep(deadline.saturating_duration_since(Instant::now())).boxed_local().fuse();
                let has_ops = !work.pending_ops.is_empty();
                let has_timers = !work.pending_timers.is_empty();

                match (has_ops, has_timers) {
                    (true, true) => {
                        futures::select! {
                            r = work.pending_ops.select_next_some() => Some(AsyncEvent::Op(r)),
                            r = work.pending_timers.select_next_some() => Some(AsyncEvent::Timer(r)),
                            _ = notify_rx.next() => None,
                            _ = request_cancel => None,
                            _ = tick => None,
                        }
                    }
                    (true, false) => {
                        futures::select! {
                            r = work.pending_ops.select_next_some() => Some(AsyncEvent::Op(r)),
                            _ = notify_rx.next() => None,
                            _ = request_cancel => None,
                            _ = tick => None,
                        }
                    }
                    (false, true) => {
                        futures::select! {
                            r = work.pending_timers.select_next_some() => Some(AsyncEvent::Timer(r)),
                            _ = notify_rx.next() => None,
                            _ = request_cancel => None,
                            _ = tick => None,
                        }
                    }
                    (false, false) => {
                        futures::select! {
                            _ = notify_rx.next() => None,
                            _ = request_cancel => None,
                            _ = tick => None,
                        }
                    }
                }
            } else {
                let has_ops = !work.pending_ops.is_empty();
                let has_timers = !work.pending_timers.is_empty();

                match (has_ops, has_timers) {
                    (true, true) => {
                        futures::select! {
                            r = work.pending_ops.select_next_some() => Some(AsyncEvent::Op(r)),
                            r = work.pending_timers.select_next_some() => Some(AsyncEvent::Timer(r)),
                            _ = notify_rx.next() => None,
                            _ = request_cancel => None,
                        }
                    }
                    (true, false) => {
                        futures::select! {
                            r = work.pending_ops.select_next_some() => Some(AsyncEvent::Op(r)),
                            _ = notify_rx.next() => None,
                            _ = request_cancel => None,
                        }
                    }
                    (false, true) => {
                        futures::select! {
                            r = work.pending_timers.select_next_some() => Some(AsyncEvent::Timer(r)),
                            _ = notify_rx.next() => None,
                            _ = request_cancel => None,
                        }
                    }
                    (false, false) => {
                        futures::select! {
                            _ = notify_rx.next() => None,
                            _ = request_cancel => None,
                        }
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
                    // Re-upgrade for the batch: the strong `Rc` from phase 1
                    // was dropped before the event await. If the runtime was
                    // evicted while we were awaiting events, exit the pump —
                    // the disposed isolate must not be re-entered, and the
                    // batched results have nowhere to go.
                    let Some(runtime) = runtime.upgrade() else { return; };
                    let v8_start = Instant::now();
                    // Thread CPU clock, sampled alongside the wall clock: the
                    // wall delta polices the app's share of the machine, the
                    // CPU delta is what gets billed. Same clock the worker
                    // samples around synchronous dispatch, so both halves of
                    // an app's CPU land on `cpu_us` on one consistent basis.
                    let cpu_start = crate::core::init::thread_cpu_time();
                    let mut rt = runtime.borrow_mut();
                    rt.enter_isolate();
                    for ev in batch {
                        if rt.host_interrupt.load(Ordering::Acquire) {
                            break;
                        }
                        rt.handle_async_event(ev, &mut work);
                    }
                    rt.advance_startup();
                    rt.exit_isolate();

                    // Two independent things happen on this one window:
                    //
                    // 1. BILLING — the CPU this app's async continuations
                    //    burned is emitted to its `cpu_us` meter. It is not
                    //    attributable to any single request, but the meter is
                    //    keyed by app and this Runtime is one app's isolate,
                    //    so the attribution billing needs is already exact.
                    // 2. ENFORCEMENT — if those continuations (timer
                    //    callbacks, microtask chains) consume >80% of WALL
                    //    time over a 10 s window, terminate the isolate. The
                    //    per-request CPU timer doesn't catch pump-side work.
                    //
                    // Different clocks on purpose: money is charged on CPU,
                    // share-of-the-machine is policed on wall.
                    rt.bill_pump_cpu(
                        crate::core::init::thread_cpu_time().saturating_sub(cpu_start),
                    );
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
                // next batch.
                //
                // CAVEAT, established while fixing the zero-delay drain: this
                // is not actually a yield. `TimerRuntime::insert` drops any
                // deadline already in the past, so `sleep(Duration::ZERO)`
                // completes without ever returning `Pending` and the scheduler
                // is never reached. It is left alone here because this arm
                // always has real awaits around it (the next iteration parks on
                // the event select) and because the surrounding hot path was
                // calibrated with it in place — but it does NOT provide the
                // fairness the paragraph above describes. Anything that needs a
                // genuine yield must park on a non-zero deadline, as the
                // `ready_timers_pending` wait above does.
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
            } else {
                // No event: either a bare pump notification, or the
                // `READY_TIMER_PASS_TICK` ceiling expiring with more zero-delay
                // timers still queued. Both fall through to the next iteration,
                // which re-runs PHASE 1. No yield is needed here — reaching this
                // point means the wait above already awaited a real deadline.
                let Some(runtime) = runtime.upgrade() else { return; };
                let mut rt = runtime.borrow_mut();
                let cancellation_cpu_start = crate::core::init::thread_cpu_time();
                rt.cleanup_cancelled_requests();
                rt.bill_pump_cpu(crate::core::init::thread_cpu_time().saturating_sub(cancellation_cpu_start));
            }
        }
    }

    // -----------------------------------------------------------------------
    // CPU timer arm/disarm
    // -----------------------------------------------------------------------

    fn arm_cpu_timer(&mut self) {
        let startup = self.startup.is_pending();
        let dev_loading = !startup && self.dev_entry_cpu_running;
        if dev_loading && self.cpu_limit.is_some() && self.dev_entry_cpu_started.is_none() {
            let generation = self.dev_entry_loader.as_ref().unwrap().work_generation();
            if self.dev_entry_cpu_generation != Some(generation) {
                self.dev_entry_cpu_generation = Some(generation);
                self.dev_entry_cpu = Duration::ZERO;
            }
            self.dev_entry_cpu_started = Some(crate::core::init::thread_cpu_time());
        }
        if startup && self.cpu_limit.is_some() && self.startup_cpu_started.is_none() {
            self.startup_cpu_started = Some(crate::core::init::thread_cpu_time());
        }
        #[cfg(target_os = "linux")]
        if !self.cpu_timer_active
            && let (Some(timer), Some(limit)) = (&self.cpu_timer, self.cpu_limit)
        {
            let remaining = if startup {
                limit.saturating_sub(self.startup_cpu)
            } else if dev_loading {
                limit.saturating_sub(self.dev_entry_cpu)
            } else {
                limit
            };
            // A zero POSIX timer duration disarms it, so an exhausted startup
            // budget must retain an active interrupt deadline.
            timer.arm(remaining.max(Duration::from_nanos(1)));
            self.cpu_timer_active = true;
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
        if let Some(started) = self.startup_cpu_started.take() {
            self.startup_cpu += crate::core::init::thread_cpu_time().saturating_sub(started);
            if self.cpu_limit.is_some_and(|limit| self.startup_cpu >= limit) {
                self.cpu_note.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
        if let Some(started) = self.dev_entry_cpu_started.take() {
            self.dev_entry_cpu +=
                crate::core::init::thread_cpu_time().saturating_sub(started);
            if self
                .cpu_limit
                .is_some_and(|limit| self.dev_entry_cpu >= limit)
            {
                self.cpu_note
                    .store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }

    /// Report the detected termination cause. Host interruption is permanent;
    /// CPU and heap terminations can be recovered for an ordinary cached runtime.
    fn termination_message(&self) -> &'static str {
        if self.host_interrupt.load(Ordering::Acquire) {
            "runtime execution interrupted"
        } else if self.last_termination_was_heap {
            "memory limit exceeded"
        } else {
            "CPU time limit exceeded"
        }
    }

    fn check_v8_terminated(&mut self) -> bool {
        // Take both notes unconditionally. They must be cleared either way, so
        // a terminated dispatch does not leave a flag set and fail the NEXT
        // request on this isolate.
        let heap_terminated = self
            .terminated_note
            .swap(false, std::sync::atomic::Ordering::Relaxed);
        let cpu_terminated = self
            .cpu_note
            .swap(false, std::sync::atomic::Ordering::Relaxed);

        let v8_terminating = self.isolate.is_execution_terminating();
        let host_interrupted = self.host_interrupt.load(Ordering::Acquire);
        if !host_interrupted && !heap_terminated && !cpu_terminated && !v8_terminating {
            return false;
        }
        // Recorded so the call sites can name the actual cause. They all used
        // to say "CPU time limit exceeded", which is now reachable by a second
        // route and would misreport a heap kill as a CPU kill.
        self.last_termination_was_heap = heap_terminated && !cpu_terminated;
        if v8_terminating && !host_interrupted {
            self.isolate.cancel_terminate_execution();
        }
        #[cfg(target_os = "linux")]
        if self.cpu_timer.is_some() {
            self.disarm_cpu_timer();
        }
        if self.startup.is_pending() {
            self.fail_startup(self.termination_message().into());
        }
        true
    }

    // -----------------------------------------------------------------------
    // Direct dispatch (channel-free mode)
    // -----------------------------------------------------------------------

    // -----------------------------------------------------------------------
    // Kernel dispatch primitive — call_fetch_handler
    // -----------------------------------------------------------------------

    /// Kernel durable-workflow replay primitive. The trusted host passes replay
    /// input as JSON; both the interpreter and native failures return an outcome
    /// batch. Lease authority stays outside the isolate.
    pub fn call_workflow_dispatch(
        &mut self,
        modules: &[crate::ModuleEntry],
        envelope_json: &str,
        env: &crate::EnvSnapshot,
        ctx: crate::RequestCtx,
    ) -> crate::WorkflowOutcome {
        self.last_request_ts.set(Instant::now());
        crate::node::net::state::reset_dispatch_egress(&self.state);

        let init_result = self.initialize_modules(modules, env);
        if self.creator_namespace.is_none() || self.host_interrupt.load(Ordering::Acquire) {
            let msg = match init_result {
                Err(err) => err,
                Ok(_) if self.host_interrupt.load(Ordering::Acquire) => {
                    self.termination_message().to_string()
                }
                Ok(true) => "Startup published no creator module for workflow dispatch".to_string(),
                Ok(false) => "Runtime startup is pending; await initialize before workflow dispatch".to_string(),
            };
            return crate::WorkflowOutcome::Response {
                json: workflow_failure_json(&msg),
                logs: vec![],
            };
        }

        let request_id = self.next_direct_request_id;
        self.next_direct_request_id += 1;
        {
            let mut s = self.state.borrow_mut();
            s.executing_request_id = Some(request_id);
            s.executing_request_cancel = Some(ctx.cancel.clone());
        }

        let wall_start = Instant::now();
        let invocation_context =
            crate::core::invocation::InvocationContext::request(request_id, None);
        self.arm_cpu_timer();
        let dispatch_result: Result<Result<String, DispatchError>, v8::Global<v8::Promise>> =
            enter_v8!(self, |scope| {
                crate::core::invocation::with_context(scope, &invocation_context, |scope| {
                    let creator = v8::Local::new(scope, self.creator_namespace.as_ref().unwrap());
                    match parse_workflow_envelope(scope, envelope_json) {
                        Ok(envelope_arg) => {
                            let ctx_arg: v8::Local<v8::Value> = {
                                let maybe = self.state.borrow().ctx_obj.clone();
                                match maybe {
                                    Some(g) => v8::Local::new(scope, g).into(),
                                    None => v8::Object::new(scope).into(),
                                }
                            };
                            call_workflow_inner(scope, creator, envelope_arg, ctx_arg)
                        }
                        Err(e) => Ok(Err(e)),
                    }
                })
            });
        self.disarm_cpu_timer();

        if self.check_v8_terminated() {
            self.clear_executing_request();
            self.discard_request_state(request_id);
            return crate::WorkflowOutcome::Response {
                json: workflow_failure_json(self.termination_message()),
                logs: vec![],
            };
        }

        let cpu_elapsed = wall_start.elapsed();
        match dispatch_result {
            Ok(Ok(json)) => {
                self.clear_executing_request();
                let logs = self.drain_request_logs(request_id);
                crate::WorkflowOutcome::Response { json, logs }
            }
            Ok(Err(e)) => {
                self.clear_executing_request();
                self.discard_request_state(request_id);
                crate::WorkflowOutcome::Response {
                    json: workflow_failure_json(&e.message),
                    logs: vec![],
                }
            }
            Err(promise) => {
                self.clear_executing_request();
                self.store_workflow_pending(request_id, promise, ctx, cpu_elapsed, wall_start)
            }
        }
    }

    /// Invoke native RPC for matching procedure URLs, then fetchFast and fetch.
    /// A fetchFast null result falls through to the ordinary HTTP handler.
    // Mirrors the outer Runtime::call_fetch_handler_with_user wrapper's
    // params one-for-one; same rationale as that allow.
    #[allow(clippy::too_many_arguments)]
    pub fn call_fetch_handler(
        &mut self,
        modules: &[crate::ModuleEntry],
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: &[u8],
        env: &crate::EnvSnapshot,
        ctx: crate::RequestCtx,
        user_json: Option<String>,
    ) -> crate::FetchOutcome {
        self.call_fetch_handler_started(
            modules,
            method,
            url,
            headers,
            body,
            env,
            ctx,
            user_json,
            Instant::now(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn call_fetch_handler_started(
        &mut self,
        modules: &[crate::ModuleEntry],
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: &[u8],
        env: &crate::EnvSnapshot,
        ctx: crate::RequestCtx,
        user_json: Option<String>,
        request_started: Instant,
    ) -> crate::FetchOutcome {
        // Reset the idle-GC clock — every request entry is "activity".
        self.last_request_ts.set(Instant::now());
        crate::node::net::state::reset_dispatch_egress(&self.state);

        match self.initialize_modules(modules, env) {
            Err(error) => return super::startup::failure_response(&error),
            Ok(false) => {
                let (reply, rx) = channel::result_slot();
                let cancel = ctx.cancel.clone();
                self.waiting_startup_requests.push(WaitingRequest {
                    started: request_started,
                    method: method.into(), url: url.into(), headers: headers.to_vec(),
                    body: body.to_vec(), env: env.clone(), ctx, user_json, reply,
                });
                self.notify_pump();
                return crate::FetchOutcome::Pending { rx, cancel };
            }
            Ok(true) => {}
        }
        if self
            .wall_timeout
            .is_some_and(|limit| request_started.elapsed() >= limit)
        {
            return super::startup::failure_response("Request wall timeout while loading entry");
        }
        let application = self.application.as_ref().expect("ready application entry").clone();
        let dispatch_started = Instant::now();
        let request_id = self.next_direct_request_id;
        self.next_direct_request_id += 1;
        let invocation_context = crate::core::invocation::InvocationContext::request(
            request_id,
            user_json.clone(),
        );

        if user_json.is_some() {
            crate::auth::set_request_user(&self.state, request_id, user_json);
        }

        // Mark the request as executing, and wire the cancel flag through so
        // native ops spawned inside the handler can observe cancellation.
        {
            let mut s = self.state.borrow_mut();
            s.executing_request_id = Some(request_id);
            s.executing_request_cancel = Some(ctx.cancel.clone());
        }

        // Native RPC resolves a retained procedure and owns its eventual result.
        // Other requests try fetchFast before the ordinary fetch handler.
        // Subscription upgrades still use the existing WebSocket transport.
        let is_ws_upgrade = headers.iter().any(|(k, v)| {
            k.eq_ignore_ascii_case("upgrade") && v.eq_ignore_ascii_case("websocket")
        });
        let rpc_path = if !is_ws_upgrade {
            classify_zs_v1_path(url)
        } else {
            ZsV1Path::Other
        };
        #[cfg(feature = "runtime_native_websocket")]
        let subscription_id_str: Option<&str> = if application.rpc.is_some()
            && is_ws_upgrade
            && method.eq_ignore_ascii_case("GET")
        {
            match classify_zs_v1_path(url) {
                ZsV1Path::Procedure(id) => Some(id),
                ZsV1Path::Other | ZsV1Path::Missing => None,
            }
        } else {
            None
        };

        // NOTE: the env JSON is NOT marshalled here. Every tier that needs
        // the env reads the cached `state.env_obj` V8 global (built once in
        // `initialize_modules`). The raw JSON is only touched in the slow
        // path's *uncached* fallback (env_obj == None), where it is read
        // lazily via `env.as_json()` (a borrow, no per-request String clone).
        // Headers likewise need no JSON marshalling — the kernel-side
        // fast-path Request builder takes the headers slice directly.
        self.arm_cpu_timer();
        // Tracks which dispatch tier produced a pending promise so the
        // pump can pick the right settle path (envelope-wrap for Rpc,
        // inspect_response for Fetch). Default Fetch — only flipped
        // inside the RPC fast-path block.
        let mut pending_origin = PendingOrigin::Fetch;
        let mut pending_rpc_call = None;
        // A pending call or response body takes ownership of cancellation.
        let mut pending_rpc_lifetime: Option<crate::rpc::lifetime::RequestLifetime> = None;
        #[cfg(feature = "runtime_native_websocket")]
        let mut subscriptions = std::mem::take(&mut self.subscriptions);
        let dispatch_result: Result<DispatchResult, v8::Global<v8::Promise>> =
            enter_v8!(self, |scope| {
                crate::core::invocation::with_context(scope, &invocation_context, |scope| 'dispatch: {

                #[cfg(feature = "runtime_native_websocket")]
                if let Some(rpc_id) = subscription_id_str {
                    let registry = application.rpc.as_ref().unwrap().clone();
                    let user_json = self.state.borrow().per_request_user.get(&request_id).cloned();
                    let inputs = build_rpc_ctx_inputs(request_id, headers);
                    let headers = std::sync::Arc::new(headers.to_vec());
                    let (rpc_ctx, signal) = match crate::rpc::mint_rpc_ctx(
                        scope,
                        inputs.request_id,
                        inputs.trace_id,
                        method.to_string(),
                        url.to_string(),
                        headers,
                        user_json,
                        inputs.idempotency_key,
                    ) {
                        Ok(result) => result,
                        Err(error) => break 'dispatch Ok(DispatchResult::Error(error.to_string())),
                    };
                    let abort_guard = self.app_id.as_ref().map(|app_id| {
                        crate::rpc::abort::register_in_flight(app_id, request_id, signal.clone())
                    });
                    let lifetime = crate::rpc::lifetime::RequestLifetime {
                        request_id,
                        cancel: CancelFlag::new(),
                        deadline: None,
                        signal,
                        _abort_guard: abort_guard,
                    };
                    let info = subscriptions.open(
                        scope,
                        &self.state,
                        rpc_id.to_string(),
                        registry,
                        rpc_ctx,
                        lifetime,
                    );
                    break 'dispatch Ok(DispatchResult::HttpResponse(info));
                }

                // ---- Tier 1: RPC fast path ----
                // The reserved path is recognized independently of the HTTP
                // method. Procedure metadata decides whether that method is
                // allowed after the string name resolves.
                if matches!(rpc_path, ZsV1Path::Missing) {
                    break 'dispatch Ok(rpc_invalid_argument_response("missing wireId"));
                }
                if let ZsV1Path::Procedure(rpc_id) = rpc_path {
                    let Some(registry) = application.rpc.as_ref().cloned() else {
                        break 'dispatch Ok(crate::rpc::dispatch::response::error_value(
                            format!("Method not found: {rpc_id}"), 404, "NOT_FOUND",
                        ));
                    };
                    let input_arg: v8::Local<v8::Value> = match parse_rpc_input(scope, method, url, body) {
                        InputParse::Ok(v) => v,
                        InputParse::Reject400(msg) => {
                            break 'dispatch Ok(rpc_invalid_argument_response(msg));
                        }
                    };
                    // The native RpcCtx is the handler argument and its ambient context.
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
                    );
                    let (rpc_ctx_object, mut local_rpc_lifetime) = match mint_result {
                        Ok((ctx_obj, signal)) => {
                            let abort_guard = self.app_id.as_ref().map(|app_id| {
                                crate::rpc::abort::register_in_flight(app_id, request_id, signal.clone())
                            });
                            let request = crate::rpc::lifetime::RequestLifetime {
                                request_id, cancel: ctx.cancel.clone(),
                                deadline: self
                                    .wall_timeout
                                    .and_then(|timeout| request_started.checked_add(timeout)),
                                signal, _abort_guard: abort_guard,
                            };
                            (ctx_obj, Some(request))
                        }
                        Err(error) => break 'dispatch Ok(DispatchResult::Error(error.to_string())),
                    };
                    let rpc_path = format!("/__zeroship/v1/{rpc_id}");
                    let (result, call) = call_rpc_inner(
                        scope,
                        registry,
                        rpc_id,
                        input_arg,
                        rpc_ctx_object,
                        method,
                        &rpc_path,
                    );
                    if result.is_err() {
                        pending_origin = PendingOrigin::Rpc;
                        pending_rpc_call = call;
                        pending_rpc_lifetime = local_rpc_lifetime.take();
                    } else if let Ok(DispatchResult::HttpResponse(ResponseInfo::Stream {stream_id, ..})) = &result {
                        crate::streams::response_forwarder::retain_request(
                            scope, &self.state, *stream_id, local_rpc_lifetime.take().expect("RPC request lifetime"),
                        );
                    }
                    break 'dispatch result;
                }

                let fetch_fast_result = if let Some(handler) = application.fetch_fast.as_ref() {
                    let (ff_fn, receiver) = handler.locals(scope);
                    let method_arg = v8::String::new(scope, method).unwrap().into();
                    let url_arg = v8::String::new(scope, url).unwrap().into();
                    let body_arg = uint8_array_from_bytes(scope, body)
                        .unwrap_or_else(|| v8::undefined(scope).into());
                    let env_arg: v8::Local<v8::Value> = {
                        let maybe_global = self.state.borrow().env_obj.clone();
                        match maybe_global {
                            Some(g) => v8::Local::new(scope, g).into(),
                            None => v8::Object::new(scope).into(),
                        }
                    };
                    call_fetch_fast_inner(scope, ff_fn, receiver, method_arg, url_arg, body_arg, env_arg)
                } else {
                    FetchFastResult::FallThrough
                };

                if let FetchFastResult::Handled(res) = fetch_fast_result {
                    // ---- Fast path: fetchFast(method, url, bodyBytes, env) ----
                    // Returned a concrete result — use it directly, no
                    // Request/Response object construction needed.
                    res
                } else {
                    if application.fetch.is_none() {
                        break 'dispatch Ok(DispatchResult::HttpResponse(ResponseInfo::Complete {
                            status: 404,
                            headers: vec![("content-type".into(), "application/json".into())],
                            body: br#"{"message":"No default.fetch handler exported","name":"Error"}"#.to_vec(),
                        }));
                    }
                    // ---- Slow path: full default.fetch(request, env, ctx) ----
                    //
                    // Build the Request from the parsed HTTP data using the
                    // native Request and Headers classes.
                    let request_opt = crate::fetch_request::build_kernel_request(
                        scope, method, url, headers, body,
                    );
                    if let Some(request) = request_opt {
                        // Stash the Request so `getRequest()` can find it
                        // without forwarding it through a mutable JavaScript global.
                        // Cleared in drain_request_logs /
                        // discard_request_state together with the other per-request
                        // state (user, ctx, logs).
                        let global = v8::Global::new(scope, request);
                        self.state.borrow_mut().request_by_id.insert(request_id, global);

                        let env_val: v8::Local<v8::Value> = {
                            let maybe_global = self.state.borrow().env_obj.clone();
                            match maybe_global {
                                Some(g) => v8::Local::new(scope, g).into(),
                                None => {
                                    // Uncached fallback only — read the env JSON
                                    // lazily here (borrow, no per-request clone).
                                    let env_src = v8::String::new(scope, env.as_json()).unwrap();
                                    v8::json::parse(scope, env_src)
                                        .unwrap_or_else(|| v8::Object::new(scope).into())
                                }
                            }
                        };

                        // Reuse the frozen ctx singleton built in
                        // initialize_modules. Same V8 Object across every
                        // fetch request — no map transitions, no per-
                        // request Function allocations.
                        let ctx_val: v8::Local<v8::Value> = {
                            let maybe = self.state.borrow().ctx_obj.clone();
                            match maybe {
                                Some(g) => v8::Local::new(scope, g).into(),
                                None => v8::Object::new(scope).into(),
                            }
                        };

                        let (handler, receiver) = application.fetch.as_ref().unwrap().locals(scope);
                        call_fetch_inner(scope, handler, receiver, request.into(), env_val, ctx_val)
                    } else {
                        Ok(DispatchResult::Error("Failed to construct Request object".to_string()))
                    }
                }
                })
            });
        #[cfg(feature = "runtime_native_websocket")]
        {
            self.subscriptions = subscriptions;
        }
        self.disarm_cpu_timer();

        if self.check_v8_terminated() {
            self.clear_executing_request();
            self.discard_request_state(request_id);
            return crate::FetchOutcome::Response {
                status: 503,
                headers: vec![("content-type".into(), "application/json".into())],
                body: crate::dispatch::build_error_body(
                    503,
                    request_id,
                    self.termination_message(),
                    "Error",
                    crate::dispatch::ErrorExtras::default(),
                )
                .into_bytes(),
                logs: vec![],
            };
        }

        let cpu_elapsed = dispatch_started.elapsed();

        match dispatch_result {
            Ok(DispatchResult::HttpResponse(info)) => {
                self.clear_executing_request();
                self.build_fetch_outcome(request_id, info, cpu_elapsed)
            }
            Ok(DispatchResult::ErrorValue {
                message,
                name,
                stack,
                status,
                code,
                details_json,
                retryable,
            }) => {
                // Handler threw (or returned a rejected promise). Honor
                // `err.status` so `throw new HttpError(404)` yields 404,
                // not the previous hardcoded 500. Forward any structured-
                // error extras (code/details/retryable) verbatim.
                self.clear_executing_request();
                // DRAIN, do not discard. This arm used to call
                // `discard_request_state`, which REMOVES `per_request_logs[id]`
                // and returns nothing, and then emitted `logs: vec![]` - so
                // everything the procedure printed before it threw was destroyed
                // here, one layer above anything that could forward it (#334).
                //
                // Measured: `getMessages` and `boom` in the same app, same run,
                // same GET /api/apps/<id>/logs payload - the succeeding
                // procedure's line delivered, the throwing one's absent. The
                // capture was never the problem; the entry existed under a real
                // request id right up until this line removed it.
                //
                // `drain_request_logs` is a SUPERSET of `discard_request_state`:
                // it drops the same sibling per-request state (user, bound ctx,
                // Request) and additionally clears `executing_request_id`, which
                // is why the success arm calls it alone.
                let logs = self.drain_request_logs(request_id);
                let extras = crate::dispatch::ErrorExtras {
                    stack: stack.as_deref(),
                    code: code.as_deref(),
                    details_json: details_json.as_deref(),
                    retryable,
                };
                crate::FetchOutcome::Response {
                    status,
                    headers: vec![("content-type".into(), "application/json".into())],
                    body: crate::dispatch::build_error_body(
                        status, request_id, &message, &name, extras,
                    )
                    .into_bytes(),
                    logs,
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
                    body: crate::dispatch::build_error_body(
                        500,
                        request_id,
                        &msg,
                        "Error",
                        crate::dispatch::ErrorExtras::default(),
                    )
                    .into_bytes(),
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
                self.clear_executing_request();
                self.store_fetch_pending(
                    request_id, promise, ctx, cpu_elapsed, request_started, pending_origin,
                    pending_rpc_call, pending_rpc_lifetime,
                )
            }
        }
    }

    /// Track a pending fetch promise and return a `FetchOutcome::Pending`
    /// whose receiver is settled by the pump via `send_settled_reply_any`
    /// once the promise resolves or rejects.
    // Bundling these into a params struct is a real design decision, not
    // a mechanical lint fix, so it's left for a deliberate follow-up.
    #[allow(clippy::too_many_arguments)]
    fn store_fetch_pending(
        &mut self,
        request_id: u64,
        promise: v8::Global<v8::Promise>,
        ctx: crate::RequestCtx,
        cpu_accumulated: Duration,
        wall_start: Instant,
        origin: PendingOrigin,
        rpc_call: Option<crate::rpc::dispatch::RpcCall>,
        rpc_lifetime: Option<crate::rpc::lifetime::RequestLifetime>,
    ) -> crate::FetchOutcome {
        let (tx, rx) = channel::result_slot();

        self.pending_requests.insert(request_id, PendingRequest {
            id: request_id,
            promise,
            reply: PendingReply::Fetch(tx),
            cpu_accumulated,
            wall_start,
            cancel: ctx.cancel.clone(),
            origin,
            rpc_call,
            rpc_lifetime,
        });
        self.notify_pump();

        crate::FetchOutcome::Pending {
            rx,
            cancel: ctx.cancel,
        }
    }

    fn store_workflow_pending(
        &mut self,
        request_id: u64,
        promise: v8::Global<v8::Promise>,
        ctx: crate::RequestCtx,
        cpu_accumulated: Duration,
        wall_start: Instant,
    ) -> crate::WorkflowOutcome {
        let (tx, rx) = channel::result_slot();

        self.pending_requests.insert(request_id, PendingRequest {
            id: request_id,
            promise,
            reply: PendingReply::Workflow(tx),
            cpu_accumulated,
            wall_start,
            cancel: ctx.cancel.clone(),
            origin: PendingOrigin::Workflow,
            rpc_call: None,
            rpc_lifetime: None,
        });
        self.notify_pump();

        crate::WorkflowOutcome::Pending {
            rx,
            cancel: ctx.cancel,
        }
    }

    /// Convert a `ResponseInfo` into a `FetchOutcome`. Mirrors
    /// [`Self::build_fetch_outcome`] exactly — the writer-attachment logic
    /// for streaming responses is preserved verbatim. When the old HTTP
    /// path is deleted, this helper fully replaces it.
    fn build_fetch_outcome(
        &mut self,
        request_id: u64,
        info: ResponseInfo,
        _cpu_time: Duration,
    ) -> crate::FetchOutcome {
        let logs = if matches!(&info, ResponseInfo::Stream { stream_id, .. }
            if crate::streams::response_forwarder::owns_request(&self.state, *stream_id))
        {
            self.state.borrow_mut().per_request_logs.remove(&request_id).unwrap_or_default()
        } else {
            self.drain_request_logs(request_id)
        };
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
                // Wake the pump so it drives the response body's read loop to
                // completion in the background. The body's async generator
                // yields the remaining chunks via setTimeout/await, which only
                // advance while the pump is running; if the pump went idle after
                // a PRIOR request, the 2nd+ stream on this isolate would deliver
                // its first (sync) frame and then stall (ISS-71b). Mirrors the
                // `Pending` path, which already notifies (see `track_pending`).
                self.notify_pump();
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
        if self.host_interrupt.load(Ordering::Acquire) {
            return;
        }
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

    /// Drive resumed readers and cancellation through the guarded native-turn
    /// path. It settles promises resolved by callbacks and drains cleanup work
    /// before the pump can park again.
    fn service_forwarder_resumes(&mut self, work: &mut AsyncWork) {
        if self.host_interrupt.load(Ordering::Acquire) {
            return;
        }
        let pending: Vec<u32> = {
            let mut state = self.state.borrow_mut();
            if state.forwarder_resumes.is_empty() { return; }
            state.forwarder_resumes.drain(..).collect()
        };
        self.dispatch_native_turn(
            work,
            crate::core::invocation::InvocationContext::default(),
            |state| {
                let mut state = state.borrow_mut();
                state.executing_request_id = None;
                state.executing_request_cancel = None;
            },
            move |scope, state| {
                for stream_id in pending {
                    crate::streams::response_forwarder::resume_read(scope, state, stream_id);
                }
            },
        );
    }

    /// Resolve parked Trusted JS-driver command promises from the Rust mailbox.
    ///
    /// The migrate crate uses this only in a dedicated Trusted runtime. Normal
    /// creator runtimes leave `RuntimeState::js_driver` empty, so this hook is a
    /// no-op and the associated globals are never installed.
    fn service_js_driver_commands(&mut self, work: &mut AsyncWork) {
        if self.host_interrupt.load(Ordering::Acquire) {
            return;
        }
        loop {
            let next = {
                let mut s = self.state.borrow_mut();
                let Some(driver) = s.js_driver.as_mut() else {
                    return;
                };
                let Some(resolver) = driver.next_command_resolver.take() else {
                    return;
                };
                let Some(command_json) = driver.command_queue.pop_front() else {
                    driver.next_command_resolver = Some(resolver);
                    return;
                };
                (resolver, command_json)
            };

            let (resolver, command_json) = next;
            enter_v8!(self, |scope| {
                let resolver = v8::Local::new(scope, &resolver);
                let value = v8::String::new(scope, &command_json)
                    .and_then(|json| v8::json::parse(scope, json))
                    .unwrap_or_else(|| {
                        let msg = v8::String::new(
                            scope,
                            "__zsNextCommand: invalid command JSON from Rust",
                        )
                        .unwrap();
                        v8::Exception::error(scope, msg)
                    });
                resolver.resolve(scope, value);
            });

            self.drain_new_tasks_into(work);
        }
    }

    /// Handle an async event from the pump (op completed or timer fired).
    /// Enters V8 briefly to resolve the op/timer, checks settled promises,
    /// and sends results via oneshot channels.
    pub fn handle_async_event(&mut self, event: AsyncEvent, work: &mut AsyncWork) {
        if self.host_interrupt.load(Ordering::Acquire) {
            return;
        }
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
                let invocation_context =
                    crate::core::invocation::InvocationContext::from_request_id(
                        &self.state,
                        request_id,
                    );
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
                    crate::core::invocation::with_context(scope, &invocation_context, |scope| {
                        crate::dispatch::resolve_op(scope, &self.state, op_id, &value);
                        collect_settled_promises(
                            scope,
                            &mut self.pending_requests,
                            &self.state,
                        )
                    })
                });
                self.disarm_cpu_timer();

                if self.check_v8_terminated() {
                    // Only error the request whose JS was executing when the timer fired
                    if let Some(rid) = request_id
                        && let Some(req) = self.pending_requests.remove(&rid)
                    {
                        send_pending_error(req, self.termination_message());
                    }
                    self.clear_executing_request();
                    self.drain_new_tasks_into(work);
                    return;
                }

                let cpu_elapsed = start.elapsed();

                if let Some(rid) = request_id
                    && let Some(req) = self.pending_requests.get_mut(&rid)
                {
                    req.cpu_accumulated += cpu_elapsed;
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
                let invocation_context =
                    crate::core::invocation::InvocationContext::from_request_id(
                        &self.state,
                        request_id,
                    );
                if let Some(rid) = request_id {
                    let cancel = self.pending_requests.get(&rid).map(|r| r.cancel.clone());
                    let mut s = self.state.borrow_mut();
                    s.executing_request_id = Some(rid);
                    s.executing_request_cancel = cancel;
                }

                let start = Instant::now();

                self.arm_cpu_timer();
                let settled_results = enter_v8!(self, |scope| {
                    crate::core::invocation::with_context(scope, &invocation_context, |scope| {
                        crate::dispatch::reject_op(scope, &self.state, op_id, &error);
                        collect_settled_promises(
                            scope,
                            &mut self.pending_requests,
                            &self.state,
                        )
                    })
                });
                self.disarm_cpu_timer();

                if self.check_v8_terminated() {
                    if let Some(rid) = request_id
                        && let Some(req) = self.pending_requests.remove(&rid)
                    {
                        send_pending_error(req, self.termination_message());
                    }
                    self.clear_executing_request();
                    self.drain_new_tasks_into(work);
                    return;
                }

                let cpu_elapsed = start.elapsed();

                if let Some(rid) = request_id
                    && let Some(req) = self.pending_requests.get_mut(&rid)
                {
                    req.cpu_accumulated += cpu_elapsed;
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
                let invocation_context =
                    crate::core::invocation::InvocationContext::from_request_id(
                        &self.state,
                        request_id,
                    );
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
                    crate::core::invocation::with_context(scope, &invocation_context, |scope| {
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
                        ResolveValue::Native(value) => match value.into_v8(scope) {
                            Ok(value) => {
                                r.resolve(scope, value);
                            }
                            Err(error) => {
                                let exception = error.to_exception(scope);
                                r.reject(scope, exception);
                            }
                        },
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
                        ResolveValue::BigInt(n) => {
                            let v = v8::BigInt::new_from_i64(scope, n);
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
                    crate::core::init::perform_microtask_checkpoint(scope);
                    collect_settled_promises(
                        scope,
                        &mut self.pending_requests,
                        &self.state,
                    )
                    })
                });
                self.disarm_cpu_timer();

                if self.check_v8_terminated() {
                    if let Some(rid) = request_id
                        && let Some(req) = self.pending_requests.remove(&rid)
                    {
                        send_pending_error(req, self.termination_message());
                    }
                    self.clear_executing_request();
                    self.drain_new_tasks_into(work);
                    return;
                }

                let cpu_elapsed = start.elapsed();

                if let Some(rid) = request_id
                    && let Some(req) = self.pending_requests.get_mut(&rid)
                {
                    req.cpu_accumulated += cpu_elapsed;
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
                let invocation_context =
                    crate::core::invocation::InvocationContext::connection(
                        self.state.borrow().ws_user.get(&ws_id).cloned(),
                    );
                if self.subscriptions.owns(ws_id) {
                    self.dispatch_subscription_turn(
                        work,
                        invocation_context,
                        ws_id,
                        |subscriptions, scope, state| {
                            subscriptions.on_websocket_event(scope, state, ws_id);
                        },
                    );
                    return;
                }
                // Native WebSocket events: drain the per-WS event
                // queue and dispatch each event in FIFO order. Multiple
                // events may have been coalesced under one OpResult
                // (the network task pushes one OpResult per event,
                // but the drain takes them all at once — extras
                // resolve as no-op drains).
                // Re-establish THIS connection's authenticated identity for
                // the WS turn. WS events aren't attributed to a request id,
                // so without this `env.auth.getUser()` inside an `onmessage`
                // / `onclose` handler would read whatever user last touched
                // the pooled isolate (a stale leftover) or null. The user
                // was bound to `ws_user[ws_id]` at upgrade time; mirror the
                // request path by binding it for the duration of the turn.
                //
                // Critically, clear any leftover `executing_request_id`
                // first: WS turns have no owning request, and
                // `auth::current_user` resolves the request-id-keyed user
                // before the WS fallback — a stale id left over from a prior
                // turn would otherwise win and leak the wrong identity.
                self.dispatch_native_turn(
                    work,
                    invocation_context,
                    |state| {
                        let mut s = state.borrow_mut();
                        s.executing_request_id = None;
                        s.executing_request_cancel = None;
                        s.executing_ws_user = s.ws_user.get(&ws_id).cloned();
                    },
                    |scope, state| {
                        crate::websocket_native::dispatch::dispatch_pending_ws_events(
                            scope, state, ws_id,
                        );
                    },
                );
            }
            #[cfg(feature = "runtime_native_websocket")]
            OpResult::SubscriptionAdvance { ws_id } => {
                let invocation_context =
                    crate::core::invocation::InvocationContext::connection(
                        self.state.borrow().ws_user.get(&ws_id).cloned(),
                    );
                self.dispatch_subscription_turn(
                    work,
                    invocation_context,
                    ws_id,
                    |subscriptions, scope, state| subscriptions.advance(scope, state, ws_id),
                );
            }
            #[cfg(feature = "runtime_native_websocket")]
            OpResult::SubscriptionTimer(timer) => {
                let ws_id = timer.ws_id();
                let invocation_context =
                    crate::core::invocation::InvocationContext::connection(
                        self.state.borrow().ws_user.get(&ws_id).cloned(),
                    );
                self.dispatch_subscription_turn(
                    work,
                    invocation_context,
                    ws_id,
                    |subscriptions, scope, state| subscriptions.on_timer(scope, state, timer),
                );
            }
            OpResult::SocketEvent { socket_id } => {
                self.dispatch_native_turn(
                    work,
                    crate::core::invocation::InvocationContext::default(),
                    |state| {
                        let mut s = state.borrow_mut();
                        s.executing_request_id = None;
                        s.executing_request_cancel = None;
                        #[cfg(feature = "runtime_native_websocket")]
                        {
                            s.executing_ws_user = None;
                        }
                    },
                    |scope, state| {
                        crate::node::net::dispatch::dispatch_pending_socket_events(
                            scope, state, socket_id,
                        );
                    },
                );
            }
        }
    }

    fn dispatch_native_turn<Prep, Dispatch>(
        &mut self,
        work: &mut AsyncWork,
        invocation_context: crate::core::invocation::InvocationContext,
        prep: Prep,
        dispatch: Dispatch,
    ) where
        Prep: FnOnce(&SharedState),
        Dispatch: FnOnce(&mut v8::PinScope, &SharedState),
    {
        let state_clone = self.state.clone();
        prep(&self.state);

        self.arm_cpu_timer();
        let settled_results = enter_v8!(self, |scope| {
            crate::core::invocation::with_context(scope, &invocation_context, |scope| {
                dispatch(scope, &state_clone);
                crate::core::init::perform_microtask_checkpoint(scope);
                collect_settled_promises(scope, &mut self.pending_requests, &self.state)
            })
        });
        self.disarm_cpu_timer();

        // Clear any per-turn WS identity so it never bleeds into a subsequent
        // non-WS turn on this isolate.
        #[cfg(feature = "runtime_native_websocket")]
        {
            self.state.borrow_mut().executing_ws_user = None;
        }

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

    #[cfg(feature = "runtime_native_websocket")]
    fn dispatch_subscription_turn<Dispatch>(
        &mut self,
        work: &mut AsyncWork,
        invocation_context: crate::core::invocation::InvocationContext,
        ws_id: u32,
        dispatch: Dispatch,
    ) where
        Dispatch: FnOnce(
            &mut crate::rpc::subscription::Subscriptions,
            &mut v8::PinScope,
            &SharedState,
        ),
    {
        let mut subscriptions = std::mem::take(&mut self.subscriptions);
        self.dispatch_native_turn(
            work,
            invocation_context,
            |state| {
                let mut state = state.borrow_mut();
                state.executing_request_id = None;
                state.executing_request_cancel = None;
                state.executing_ws_user = state.ws_user.get(&ws_id).cloned();
            },
            |scope, state| dispatch(&mut subscriptions, scope, state),
        );
        self.subscriptions = subscriptions;
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
            collect_settled_promises(scope, &mut self.pending_requests, &self.state)
        });
        self.disarm_cpu_timer();

        if self.check_v8_terminated() {
            // Only error the request whose timer callback was executing
            if let Some(rid) = owner_request_id
                && let Some(req) = self.pending_requests.remove(&rid)
            {
                send_pending_error(req, self.termination_message());
            }
            self.clear_executing_request();
            self.drain_new_tasks_into(work);
            return;
        }

        let cpu_elapsed = start.elapsed();

        if let Some(rid) = owner_request_id
            && let Some(req) = self.pending_requests.get_mut(&rid)
        {
            req.cpu_accumulated += cpu_elapsed;
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
    ///
    /// BOUNDED BY DESIGN — fires at most [`MAX_READY_TIMERS_PER_PASS`] callbacks
    /// and then returns, leaving whatever is left (including anything the fired
    /// callbacks re-scheduled) on `ready_timers` for the next pass. Without the
    /// bound this was a liveness hole: a callback that re-arms itself with
    /// `setTimeout(fn, 0)` pushes onto the very queue this loop pops from, so
    /// `spin(){ work(); setTimeout(spin, 0) }` never let the loop reach its
    /// `break`. Everything that would have noticed sits AFTER the drain —
    /// `bill_pump_cpu` in the pump's PHASE 1, `record_pump_cpu` in PHASE 2 —
    /// so the loop pinned a core while being billed nothing and policed by
    /// nothing. Returning early puts the pump back in control of both.
    ///
    /// FIFO order is preserved across the pass boundary: entries are taken with
    /// `pop_front` and the untouched remainder keeps its position in the deque,
    /// so a bounded pass fires exactly the same callbacks in exactly the same
    /// order as the old unbounded one — it just gets there in several hops.
    fn fire_ready_timers_pump(&mut self, work: &mut AsyncWork) {
        for _ in 0..MAX_READY_TIMERS_PER_PASS {
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
                collect_settled_promises(scope, &mut self.pending_requests, &self.state)
            });
            self.disarm_cpu_timer();

            if self.check_v8_terminated() {
                // Only error the request whose timer callback was executing
                if let Some(rid) = owner_request_id
                    && let Some(req) = self.pending_requests.remove(&rid)
                {
                    send_pending_error(req, self.termination_message());
                }
                self.clear_executing_request();
                self.drain_new_tasks_into(work);
                return;
            }

            let cpu_elapsed = start.elapsed();

            if let Some(rid) = owner_request_id
                && let Some(req) = self.pending_requests.get_mut(&rid)
            {
                req.cpu_accumulated += cpu_elapsed;
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
            SettledResult::Rpc(Ok(json)) => match req.reply {
                PendingReply::Workflow(tx) => {
                    let logs = self.drain_request_logs(id);
                    tx.send(Ok(crate::SettledWorkflow { json, logs }));
                }
                PendingReply::Fetch(_) => {
                    unreachable!("SettledResult::Rpc no longer produced for fetch/RPC dispatch")
                }
            },
            SettledResult::Rpc(Err(msg)) => send_pending_error(req, msg),
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
                match req.reply {
                    PendingReply::Fetch(tx) => tx.send(Ok(settled)),
                    PendingReply::Workflow(_) => {
                        unreachable!("SettledResult::Http produced for workflow dispatch")
                    }
                }
            }
            SettledResult::Http(Err(msg)) => {
                send_pending_error(req, msg);
            }
        }
    }

    /// Returns true if there are pending async requests.
    #[allow(dead_code)]
    pub fn has_pending_requests(&self) -> bool {
        self.startup.is_pending()
            || self
                .dev_entry_loader
                .as_ref()
                .is_some_and(|loader| loader.is_pending())
            || !self.waiting_startup_requests.is_empty()
            || !self.pending_requests.is_empty()
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
            send_pending_error(req, self.termination_message());
        }
    }

    /// Record `elapsed` CPU time consumed by the pump for this Runtime.
    /// Returns `true` if the cumulative budget is exceeded (the caller
    /// should terminate the isolate).
    ///
    /// ENFORCEMENT ONLY. Billing is [`Self::bill_pump_cpu`], which the pump
    /// calls separately around every V8 window — including the PHASE 1 window
    /// this budget does not see. Keep the two apart: this one is a safety
    /// mechanism whose inputs and thresholds must not drift to suit billing.
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

    /// Bill one pump V8 window's CPU to this app's `cpu_us` meter.
    ///
    /// `cpu` is thread CPU time (`CLOCK_THREAD_CPUTIME_ID`) — the same clock
    /// the worker samples around the synchronous dispatch entry, so both
    /// halves of an app's CPU land on `cpu_us` on one consistent basis and
    /// time the thread spent descheduled is not charged to the creator.
    /// (Contrast [`Self::record_pump_cpu`], which is fed WALL time because a
    /// share-of-the-machine budget has to be denominated in real time.)
    ///
    /// The meter is keyed by APP, not by request, and a Runtime is one isolate
    /// per (app, live deploy) — so pump CPU is already measured at exactly the
    /// granularity billing consumes, even though it cannot be attributed to
    /// any single in-flight request. Each window is emitted as it arrives, so
    /// it is counted exactly once and the enforcement window's periodic reset
    /// of `pump_cpu_accumulated` is irrelevant here: nothing is read back out.
    ///
    /// Borrow discipline: `state` is a different `RefCell` from the
    /// `RuntimeInner` cell the pump call sites hold, and no `state` borrow is
    /// live at either — the `needs_drain` probe borrow is taken and dropped in
    /// its own block, and every borrow inside `drain_new_tasks_into` /
    /// `handle_async_event` / `enter_isolate` / `exit_isolate` is released
    /// when those calls return. The handle is still cloned out and the borrow
    /// dropped before `record` runs, so the window is a single field read.
    /// Deliberately NOT a `try_borrow` + silent skip: dropping the metric on
    /// contention would quietly recreate the under-billing this fixes.
    fn bill_pump_cpu(&mut self, cpu: Duration) {
        if cpu.is_zero() {
            return;
        }
        let meter = { self.state.borrow().meter.clone() };
        let Some(meter) = meter else {
            // Meter-less harness (CLI `zeroship serve`, unit tests). Skip the
            // carry too, so it cannot accumulate against a meter that will
            // never exist — the handle is stamped at Runtime construction and
            // never appears later.
            return;
        };

        self.pump_cpu_unmetered += cpu;
        let micros = u64::try_from(self.pump_cpu_unmetered.as_micros()).unwrap_or(u64::MAX);
        if micros == 0 {
            // Window was sub-microsecond; it stays in the carry for next time.
            return;
        }
        meter.record("cpu_us", micros);
        self.pump_cpu_unmetered -= Duration::from_micros(micros);
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
        use crate::rpc::lifetime::Cancellation;
        let now = Instant::now();
        let cancelled: Vec<_> = self.pending_requests.iter().filter_map(|(&id, request)| {
            request.rpc_lifetime.as_ref().and_then(|request| request.cancellation(now))
                .or_else(|| request.cancel.is_cancelled().then_some(Cancellation::Cancelled))
                .map(|reason| (id, reason))
        }).collect();

        for (id, reason) in cancelled {
            let Some(req) = self.pending_requests.remove(&id) else { continue; };
            if let Some(request) = &req.rpc_lifetime {
                request.cancel.cancel();
                self.enter_isolate();
                self.arm_cpu_timer();
                let settled = enter_v8!(self, |scope| {
                    let abort = |scope: &mut v8::PinScope| {
                        v8::tc_scope!(let tc, scope);
                        let reason = reason.exception(tc);
                        request.signal.abort(tc, reason);
                    };
                    if let Some(call) = &req.rpc_call { call.with_frame(scope, abort); }
                    else { abort(scope); }
                    crate::core::init::perform_microtask_checkpoint(scope);
                    collect_settled_promises(scope, &mut self.pending_requests, &self.state)
                });
                self.disarm_cpu_timer();
                self.check_v8_terminated();
                self.exit_isolate();
                for (id, req, result) in settled {
                    self.send_settled_reply_any(id, req, result, Duration::ZERO);
                }
                let response = crate::rpc::dispatch::response::into_http(reason.response(), id);
                self.send_settled_reply_any(id, req, SettledResult::Http(response), Duration::ZERO);
            } else {
                send_pending_error(req, "Request timed out");
                let _logs = self.drain_request_logs(id);
            }
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
    state: &SharedState,
) -> Vec<(u64, PendingRequest, SettledResult)> {
    let settled_ids: Vec<u64> = pending_requests
        .iter()
        .filter_map(|(&id, req)| {
            if req.cancel.is_cancelled() || req.rpc_lifetime.as_ref()
                .is_some_and(|request| request.cancellation(Instant::now()).is_some())
            { return None; }
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
            let mut req = pending_requests.remove(&id)?;
            let invocation_context =
                crate::core::invocation::InvocationContext::from_request_id(state, Some(id));
            let result = crate::core::invocation::with_context(
                scope,
                &invocation_context,
                |scope| match req.origin {
                    PendingOrigin::Fetch => {
                        Ok(http::extract_settled_result(scope, &req.promise, id))
                    }
                    PendingOrigin::Rpc => advance_rpc_call(scope, req.rpc_call.as_mut().expect("pending RPC owns a call"))
                        .map(|result| {
                            let response = crate::rpc::dispatch::response::into_http(result, id);
                            if let Ok(ResponseInfo::Stream {stream_id, ..}) = &response {
                                crate::streams::response_forwarder::retain_request(
                                    scope, state, *stream_id, req.rpc_lifetime.take().expect("RPC request lifetime"),
                                );
                            }
                            SettledResult::Http(response)
                        }),
                    PendingOrigin::Workflow => Ok(settle_workflow_promise(scope, &req.promise)),
                },
            );
            match result {
                Ok(result) => Some((id, req, result)),
                Err(promise) => {
                    req.promise = promise;
                    pending_requests.insert(id, req);
                    None
                }
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Free function: call onRequest handler and inspect result
// ---------------------------------------------------------------------------

/// Look up a WebSocket in the global `__wsRegistry` by ID and call a method on it.
#[cfg(not(feature = "runtime_native_websocket"))]
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
/// same reason as `call_fetch_inner` — avoids double-borrowing
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
///
/// Outcome of the `fetchFast(method, url, bodyBytes, env)` extension.
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
    receiver: v8::Local<v8::Value>,
    method_arg: v8::Local<v8::Value>,
    url_arg: v8::Local<v8::Value>,
    body_arg: v8::Local<v8::Value>,
    env_arg: v8::Local<v8::Value>,
) -> FetchFastResult {
    let (result_val, caught_exception) = {
        v8::tc_scope!(let tc, scope);
        let r = ff_fn.call(tc, receiver, &[method_arg, url_arg, body_arg, env_arg]);
        if tc.has_caught() {
            let exc = tc.exception();
            let exc_global = exc.map(|e| v8::Global::new(tc, e));
            (None, exc_global)
        } else {
            (r.map(|v| v8::Global::new(tc, v)), None)
        }
    };

    crate::core::init::perform_microtask_checkpoint(scope);

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

fn uint8_array_from_bytes<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    body: &[u8],
) -> Option<v8::Local<'s, v8::Value>> {
    let len = body.len();
    let store = v8::ArrayBuffer::new_backing_store_from_vec(body.to_vec()).make_shared();
    let ab = v8::ArrayBuffer::with_backing_store(scope, &store);
    let view = v8::Uint8Array::new(scope, ab, 0, len)?;
    Some(view.into())
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
        let body = val.to_rust_string_lossy(scope).into_bytes();
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
        let body = http::get_string_property(scope, obj, "body").into_bytes();
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
        .unwrap_or_else(|| "null".to_string())
        .into_bytes();
    FetchFastResult::Handled(Ok(DispatchResult::HttpResponse(
        http::ResponseInfo::Complete {
            status: 200,
            headers: vec![("content-type".into(), "application/json".into())],
            body,
        },
    )))
}

/// Byte offset of the path component of `url` — the index of the `/` that
/// begins the path. `None` when there is no path (`http://host`, `http://host?q`).
/// A scheme-less relative URL (`/__zeroship/v1/x`) starts at its own first `/`.
///
/// The `?`/`#` bound matters twice: a `://` is only a scheme delimiter when
/// nothing before it could already have ended the scheme (a URL scheme contains
/// no `/`, `?` or `#`), and the first `/` only begins the path when it precedes
/// the query. Without either bound, `/p?q=a://b` and `http://host?q=/x` would
/// both report a "path" that lives inside the query string.
fn url_path_start(url: &str) -> Option<usize> {
    let after_scheme = url
        .find("://")
        .filter(|&i| !url[..i].contains(['/', '?', '#']))
        .map(|i| i + 3)
        .unwrap_or(0);
    let rest = &url[after_scheme..];
    let authority_end = rest.find(['?', '#']).unwrap_or(rest.len());
    rest[..authority_end].find('/').map(|i| after_scheme + i)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ZsV1Path<'a> {
    Other,
    Missing,
    Procedure(&'a str),
}

/// Classify a URL whose path begins `/__zeroship/v1/`, independent of method.
/// The reserved path must not fall through to creator `fetch` merely because
/// its name is empty or the procedure kind rejects the request method.
///
/// The prefix anchor is load-bearing, not tidiness. This used to be
/// `url.find(TAG)` — a substring search — while the gateway's own RPC lookup
/// (`compiled.rs::lookup_canonical_resource_key`) fires only on a canonical
/// path that STARTS WITH the tag. Prefix on one side and substring on the other
/// is a gateway↔worker disagreement: measured end to end on 2026-08-10
/// (`tests/e2e_gateway_path_backslash.sh` T9), `GET /x/__zeroship/v1/secret`
/// was authorized by the gateway against the app's anonymous URL catch-all and then
/// EXECUTED the `rpc:secret` procedure here, which answers 401 on its own
/// canonical URL (T7a, the one-variable control). No exotic byte was needed —
/// any leading segment at all was enough.
///
/// This scans the request target directly and does not construct a URL object.
fn classify_zs_v1_path(url: &str) -> ZsV1Path<'_> {
    const ROOT: &str = "/__zeroship/v1";
    const TAG: &str = "/__zeroship/v1/";
    let Some(start) = url_path_start(url) else {
        return ZsV1Path::Other;
    };
    let path = &url[start..];
    let end = path.find(['?', '#']).unwrap_or(path.len());
    let path = &path[..end];
    if path == ROOT {
        return ZsV1Path::Missing;
    }
    let Some(rest) = path.strip_prefix(TAG) else {
        return ZsV1Path::Other;
    };
    if rest.is_empty() {
        return ZsV1Path::Missing;
    }
    ZsV1Path::Procedure(rest)
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
///   - POST:  body is superjson, expected canonical shape
///     `{"json":<v>,"meta"?:...}`.
///   - GET:   query string carries `?input=<base64url-of-JSON-body>`.
///
/// Empty body / missing query param → `undefined` (Ok). Malformed JSON
/// or malformed base64url → `Reject400` so the kernel returns a 400
/// instead of silently coercing to undefined.
fn parse_rpc_input<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    method: &str,
    url: &str,
    body: &[u8],
) -> InputParse<'s> {
    if method.eq_ignore_ascii_case("POST") {
        return parse_rpc_body(scope, body);
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
    parse_rpc_body(scope, s.as_bytes())
}

/// Parse a superjson body and revive rich values into V8.
#[inline]
fn parse_rpc_body<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    body: &[u8],
) -> InputParse<'s> {
    if body.is_empty() {
        return InputParse::Ok(v8::undefined(scope).into());
    }
    match crate::rpc::decode_from_bytes(scope, body) {
        Ok(v) => InputParse::Ok(v),
        Err(_) => InputParse::Reject400("invalid JSON body"),
    }
}

fn workflow_failure_json(message: &str) -> String {
    let error = serde_json::json!({"type": "Error", "message": message});
    serde_json::json!({
        "kind": "RunFailed",
        "error": error,
        "outcomes": [{"kind": "RunFailed", "error": error}],
    })
    .to_string()
}

fn parse_workflow_envelope<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    json: &str,
) -> Result<v8::Local<'s, v8::Value>, DispatchError> {
    let Some(src) = v8::String::new(scope, json) else {
        return Err(DispatchError::new("workflow envelope allocation failed", 500));
    };
    v8::json::parse(scope, src)
        .ok_or_else(|| DispatchError::new("invalid workflow dispatch JSON", 400))
}

fn stringify_json_value(
    scope: &mut v8::PinScope,
    value: v8::Local<v8::Value>,
) -> Result<String, DispatchError> {
    v8::json::stringify(scope, value)
        .map(|s| s.to_rust_string_lossy(scope))
        .ok_or_else(|| DispatchError::new("workflow dispatch result is not JSON-serializable", 500))
}

fn workflow_rejection_to_error(
    scope: &mut v8::PinScope,
    exc: v8::Local<v8::Value>,
) -> DispatchError {
    match crate::dispatch::v8_exception_to_error_value(scope, exc) {
        DispatchResult::ErrorValue { message, status, .. } => {
            DispatchError::new(message, status)
        }
        DispatchResult::Error(message) => DispatchError::new(message, 500),
        _ => DispatchError::new("workflow dispatch rejected", 500),
    }
}

/// Replay one dispatch through the host-only workflow bridge.
///
/// The bridge is a plugin-registered host module, so it is outside the
/// creator's module graph and nothing in that graph instantiates it.
/// `invoke_module_export` links and evaluates it before invoking, so a dispatch
/// cannot run against an uninstantiated module even if it is the first one this
/// isolate serves. `creator` is the creator entry's own namespace, which the
/// bridge reads workflow classes from.
fn call_workflow_inner<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    creator: v8::Local<'s, v8::Value>,
    envelope_arg: v8::Local<'s, v8::Value>,
    ctx_arg: v8::Local<'s, v8::Value>,
) -> Result<Result<String, DispatchError>, v8::Global<v8::Promise>> {
    let invoked = crate::core::modules::invoke_module_export(
        scope,
        crate::WORKFLOW_DISPATCH_MODULE,
        "dispatch",
        &[creator, envelope_arg, ctx_arg],
    );
    let promise = match invoked {
        Ok(promise) => promise,
        Err(error) => return Ok(Err(DispatchError::new(error, 500))),
    };

    crate::core::init::perform_microtask_checkpoint(scope);

    let promise = v8::Local::new(scope, &promise);
    match promise.state() {
        v8::PromiseState::Fulfilled => {
            let resolved = promise.result(scope);
            Ok(stringify_json_value(scope, resolved))
        }
        v8::PromiseState::Rejected => {
            let exc = promise.result(scope);
            Ok(Err(workflow_rejection_to_error(scope, exc)))
        }
        v8::PromiseState::Pending => Err(v8::Global::new(scope, promise)),
    }
}

fn settle_workflow_promise(
    scope: &mut v8::PinScope,
    promise: &v8::Global<v8::Promise>,
) -> SettledResult {
    let local = v8::Local::new(scope, promise);
    match local.state() {
        v8::PromiseState::Fulfilled => {
            let val = local.result(scope);
            SettledResult::Rpc(
                stringify_json_value(scope, val).map_err(|e| e.message),
            )
        }
        v8::PromiseState::Rejected => {
            let exc = local.result(scope);
            let err = workflow_rejection_to_error(scope, exc);
            SettledResult::Rpc(Err(err.message))
        }
        v8::PromiseState::Pending => {
            SettledResult::Rpc(Err("workflow settle on pending promise".to_string()))
        }
    }
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
        body: body.into_bytes(),
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

/// Invoke the retained procedure target under the native request context.
fn call_rpc_inner<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    registry: crate::rpc::dispatch::ProcedureRegistry,
    name: &str,
    input: v8::Local<'s, v8::Value>,
    context: v8::Local<'s, v8::Object>,
    method: &str,
    path: &str,
) -> (Result<DispatchResult, v8::Global<v8::Promise>>, Option<crate::rpc::dispatch::RpcCall>) {
    let mut call = crate::rpc::with_rpc_context(scope, context, |scope| {
        crate::rpc::dispatch::RpcCall::new_http(
            scope,
            registry,
            name.into(),
            input,
            context.into(),
            method.into(),
            path.into(),
        )
    });
    let result = advance_rpc_call(scope, &mut call);
    let retained = if result.is_err() { Some(call) } else { None };
    (result, retained)
}

/// Loading and handler promises use the same pump-owned call. A checkpoint
/// can finish either phase without needing another external wakeup.
fn advance_rpc_call(
    scope: &mut v8::PinScope,
    call: &mut crate::rpc::dispatch::RpcCall,
) -> Result<DispatchResult, v8::Global<v8::Promise>> {
    use crate::rpc::dispatch::{CallProgress, response};
    loop {
        match call.poll(scope) {
            Ok(CallProgress::Pending(promise)) => {
                crate::core::init::perform_microtask_checkpoint(scope);
                if v8::Local::new(scope, &promise).state() == v8::PromiseState::Pending {
                    return Err(promise);
                }
            }
            Ok(CallProgress::Complete { invocation, value }) => {
                let validate_output = scope.get_slot::<SharedState>()
                    .is_some_and(|state| state.borrow().validate_rpc_output);
                return Ok(response::classify(scope, invocation, &value, validate_output));
            }
            Ok(CallProgress::Missing(name)) => return Ok(response::error_value(
                format!("Method not found: {name}"), 404, "NOT_FOUND",
            )),
            Ok(CallProgress::MethodNotAllowed { method, path }) => {
                return Ok(response::error_value(
                    format!("method {method} not allowed on {path}"),
                    405,
                    "FAILED_PRECONDITION",
                ));
            }
            Err(error) => return Ok(response::failure(scope, error)),
        }
    }
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
    receiver: v8::Local<v8::Value>,
    request: v8::Local<v8::Value>,
    env: v8::Local<v8::Value>,
    ctx: v8::Local<v8::Value>,
) -> Result<DispatchResult, v8::Global<v8::Promise>> {
    // Use a TryCatch so synchronous throws surface the exception value
    // (needed for err.status / err.name / err.stack) rather than a bare
    // `None` return that drops all of it.
    let (result_val, caught_exception) = {
        v8::tc_scope!(let tc, scope);
        let r = handler.call(tc, receiver, &[request, env, ctx]);
        if tc.has_caught() {
            let exc = tc.exception();
            let exc_global = exc.map(|e| v8::Global::new(tc, e));
            (None, exc_global)
        } else {
            (r.map(|v| v8::Global::new(tc, v)), None)
        }
    };

    crate::core::init::perform_microtask_checkpoint(scope);

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

#[cfg(test)]
mod rpc_path_anchor_tests {
    use super::{ZsV1Path, classify_zs_v1_path, url_path_start};

    /// The bypass this anchor exists to close. Before the fix the tag was
    /// located with `url.find(TAG)` — a SUBSTRING search — while the gateway
    /// only recognises an RPC request whose canonical path STARTS WITH the tag.
    /// Prefix on one side, substring on the other: an `auth:user` procedure ran
    /// anonymously behind any leading segment, plain ASCII.
    ///
    /// Each case is paired with the canonical form it differs from by exactly
    /// one thing (the leading segment), so a green here separates "the anchor
    /// rejects" from "the function never matches anything".
    #[test]
    fn tag_must_be_at_the_path_root_not_anywhere_in_the_url() {
        // Control: the canonical shape still resolves.
        assert_eq!(
            classify_zs_v1_path("/__zeroship/v1/secret"),
            ZsV1Path::Procedure("secret")
        );
        assert_eq!(
            classify_zs_v1_path("http://h/__zeroship/v1/secret"),
            ZsV1Path::Procedure("secret")
        );

        // One variable changed: a leading segment. Captured on the wire as
        // GET /apps/bslash/x/__zeroship/v1/secret -> 200 RPC-SECRET-DATA.
        assert_eq!(
            classify_zs_v1_path("/x/__zeroship/v1/secret"),
            ZsV1Path::Other
        );
        assert_eq!(
            classify_zs_v1_path("/apps/a/x/__zeroship/v1/secret"),
            ZsV1Path::Other
        );
        assert_eq!(
            classify_zs_v1_path("http://h/x/__zeroship/v1/secret"),
            ZsV1Path::Other
        );

        // The tag appearing in the query or fragment is not a path either.
        assert_eq!(
            classify_zs_v1_path("/foo?u=/__zeroship/v1/secret"),
            ZsV1Path::Other
        );
        assert_eq!(
            classify_zs_v1_path("/foo#/__zeroship/v1/secret"),
            ZsV1Path::Other
        );

        // A leading segment that itself contains `://`. This reaches the bypass
        // through `url_path_start` rather than through the prefix compare: drop
        // the scheme-detection bound below and the origin moves past `/a:/`,
        // leaving the tag looking root-anchored. Found by mutating that filter.
        assert_eq!(
            classify_zs_v1_path("/a://b/__zeroship/v1/secret"),
            ZsV1Path::Other
        );
    }

    #[test]
    fn id_is_terminated_by_query_or_fragment_and_never_empty() {
        assert_eq!(
            classify_zs_v1_path("/__zeroship/v1/a?input=x"),
            ZsV1Path::Procedure("a")
        );
        assert_eq!(
            classify_zs_v1_path("/__zeroship/v1/a#f"),
            ZsV1Path::Procedure("a")
        );
        // A bare tag names no procedure.
        assert_eq!(
            classify_zs_v1_path("/__zeroship/v1/"),
            ZsV1Path::Missing
        );
        assert_eq!(classify_zs_v1_path("/__zeroship/v1"), ZsV1Path::Missing);
        assert_eq!(
            classify_zs_v1_path("/__zeroship/v1/?input=x"),
            ZsV1Path::Missing
        );
    }

    #[test]
    fn path_classification_is_independent_of_the_request_method() {
        assert_eq!(
            classify_zs_v1_path("/__zeroship/v1/a"),
            ZsV1Path::Procedure("a")
        );
    }

    /// `url_path_start` bounds scheme detection on `/ ? #` so a `://` that is
    /// not a scheme separator cannot shift the path origin past real path
    /// bytes — which would let a segment before the tag be skipped.
    #[test]
    fn path_start_skips_only_a_real_scheme_and_authority() {
        assert_eq!(url_path_start("/a/b"), Some(0));
        assert_eq!(url_path_start("http://h/a"), Some(8));
        // No path at all: origin-form and authority-only both have none.
        assert_eq!(url_path_start("http://h"), None);
        assert_eq!(url_path_start("http://h?q=/x"), None);
        assert_eq!(url_path_start("http://h#/x"), None);
        // `://` after a path/query/fragment byte is not a scheme separator.
        assert_eq!(url_path_start("/a://b/c"), Some(0));
        assert_eq!(url_path_start("/?x=a://b/c"), Some(0));
    }

    // What these do NOT cover: the gateway's own path canonicalisation (a
    // different crate, driven by tests/e2e_gateway_path_backslash.sh), percent
    // or backslash decoding — this function sees the raw request-target and
    // deliberately does no unescaping — and whether the resolved id names a
    // procedure at all, which is the dispatcher's job downstream.
}
