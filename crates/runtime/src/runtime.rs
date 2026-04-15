//! Runtime — compio event loop with V8 isolate.
//!
//! Same architecture as runtime-tokio's Runtime, but uses compio for timers
//! and the outer event loop. V8 dispatch is identical (zeroship-v8-core).
//!
//! The main difference: `compio::time::sleep` replaces `tokio::time::sleep`,
//! and `futures::select!` replaces `tokio::select!`.
//!
//! ## Async dispatch architecture
//!
//! V8 is single-threaded. Multiple compio connection tasks share one Runtime
//! via `Rc<RefCell<Runtime>>`. The key constraint: the RefCell borrow must
//! NEVER be held across an `.await` point.
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
use std::time::{Duration, Instant};

use futures::stream::FuturesUnordered;

use crate::init::{init_v8, load_polyfills_and_modules, RequestResult};
use crate::http::{self, ResponseInfo, SettledResult, HTTP_CREATE_REQUEST_JS};
use crate::modules::ModuleEntry;
use crate::state::{
    DispatchResult, OpResult, RuntimeState, SharedState, SpawnedTimer, TimerResult,
};

use crate::channel::{
    self, CancelFlag, ResultReceiver, ResultSender, StreamReader, StreamWriter,
};

// ---------------------------------------------------------------------------
// DispatchOutcome — result of dispatch_start
// ---------------------------------------------------------------------------

/// Outcome of `dispatch_start` / `dispatch_http` — tells the connection handler what to do.
pub enum DispatchOutcome {
    /// Sync handler completed immediately. No pump involvement needed.
    Complete(Result<RequestResult, String>),
    /// Async handler: promise is pending. Poll the receiver for the result.
    Pending(ResultReceiver<Result<RequestResult, String>>),
    /// Sync HTTP response — complete buffered body.
    HttpComplete {
        status: u16,
        headers: Vec<(String, String)>,
        body: String,
        logs: Vec<String>,
    },
    /// Streaming HTTP response — headers ready, body arrives via shared buffer.
    HttpStream {
        status: u16,
        headers: Vec<(String, String)>,
        body: StreamReader,
        logs: Vec<String>,
    },
    /// Async HTTP handler: promise is pending. Poll the receiver for the reply.
    HttpPending(ResultReceiver<Result<HttpDispatchResult, String>>),
    /// WebSocket upgrade — JS returned Response with status 101 + webSocket property.
    WebSocketUpgrade {
        ws_id: u32,
        headers: Vec<(String, String)>,
    },
}

/// Result of an async HTTP dispatch (sent through the oneshot when promise settles).
pub enum HttpDispatchResult {
    Complete {
        status: u16,
        headers: Vec<(String, String)>,
        body: String,
        logs: Vec<String>,
    },
    Stream {
        status: u16,
        headers: Vec<(String, String)>,
        body: StreamReader,
        logs: Vec<String>,
    },
    WebSocket {
        ws_id: u32,
        headers: Vec<(String, String)>,
        logs: Vec<String>,
    },
}

// ---------------------------------------------------------------------------
// StreamForwarder — overflow-buffered channel writer for outbound HTTP streams
// ---------------------------------------------------------------------------

struct StreamForwarder {
    writer: StreamWriter,
}

impl StreamForwarder {
    fn new(writer: StreamWriter) -> Self {
        Self { writer }
    }

    fn try_forward(&mut self, data: Vec<u8>) -> bool {
        self.writer.push(data);
        true // no backpressure on single-threaded — just buffer
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

// ---------------------------------------------------------------------------
// PendingRequest — tracking for in-flight async requests
// ---------------------------------------------------------------------------

/// Tracking info for an in-flight request whose dispatch returned a Promise.
struct PendingRequest {
    #[allow(dead_code)]
    id: u64,
    promise: v8::Global<v8::Promise>,
    /// Reply slot for direct-dispatch async mode (pump task) — RPC path.
    reply_direct: Option<ResultSender<Result<RequestResult, String>>>,
    /// Reply slot for direct-dispatch async mode — HTTP path.
    reply_http: Option<ResultSender<Result<HttpDispatchResult, String>>>,
    /// Whether this is an HTTP request (affects response inspection).
    is_http: bool,
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
/// because V8 types are `!Send`.
pub struct Runtime {
    pub(crate) isolate: v8::OwnedIsolate,
    pub(crate) context: v8::Global<v8::Context>,
    pub(crate) dispatch_fn: Option<v8::Global<v8::Function>>,
    /// Cached reference to `__rpc.onRequest` — present when the app exports an HTTP handler.
    pub(crate) http_handler_fn: Option<v8::Global<v8::Function>>,
    /// Cached JS helper that constructs a Request from Rust-supplied params.
    http_create_request_fn: Option<v8::Global<v8::Function>>,
    pub(crate) initialized: bool,
    pub(crate) modules: Vec<ModuleEntry>,
    pub(crate) state: SharedState,
    /// Plugins registered on zeroship.* namespace.
    plugins: Vec<Box<dyn crate::plugin::NativePlugin>>,

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
    /// Optional per-request wall time limit.
    wall_timeout: Option<Duration>,

    /// POSIX CPU timer — kills V8 on CPU limit exceeded (Linux only).
    #[cfg(target_os = "linux")]
    cpu_timer: Option<crate::cpu_timer::CpuTimer>,
    /// Whether the CPU timer is currently armed.
    #[cfg(target_os = "linux")]
    cpu_timer_active: bool,
}

// SAFETY: Runtime is only used on a single compio thread.
unsafe impl Send for Runtime {}

impl Runtime {
    /// Create a new `Runtime` in direct-dispatch mode (no channel).
    pub fn new_direct(
        modules: Vec<ModuleEntry>,
        env_vars: HashMap<String, String>,
        cpu_limit: Option<Duration>,
        wall_timeout: Option<Duration>,
    ) -> Self {
        Self::new_with_plugins(modules, env_vars, cpu_limit, wall_timeout, Vec::new())
    }

    /// Create a new runtime with plugins.
    /// Plugins register native functions on `zeroship.{namespace}.*`.
    pub fn new_with_plugins(
        modules: Vec<ModuleEntry>,
        env_vars: HashMap<String, String>,
        cpu_limit: Option<Duration>,
        wall_timeout: Option<Duration>,
        plugins: Vec<Box<dyn crate::plugin::NativePlugin>>,
    ) -> Self {
        init_v8();

        // 512MB heap for dev (LangChain + deps need ~200MB). Production can be tuned lower.
        let params = v8::CreateParams::default().heap_limits(0, 512 * 1024 * 1024);
        let mut isolate = v8::Isolate::new(params);

        // Register near-heap-limit callback to prevent OOM crashes
        unsafe extern "C" fn near_heap_limit_callback(
            _data: *mut std::ffi::c_void,
            current_heap_limit: usize,
            _initial_heap_limit: usize,
        ) -> usize {
            eprintln!(
                "[v8] Near heap limit: {}MB, not increasing",
                current_heap_limit / 1024 / 1024
            );
            current_heap_limit
        }
        isolate.add_near_heap_limit_callback(near_heap_limit_callback, std::ptr::null_mut());

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
            dispatch_fn: None,
            http_handler_fn: None,
            http_create_request_fn: None,
            initialized: false,
            modules,
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
        }
    }

    /// Exit the V8 isolate so another isolate can be entered on this thread.
    /// Must be called after `new_direct` when storing multiple runtimes.
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
        self.pump_notify_tx = Some(tx);
    }

    /// Optional per-request CPU time limit.
    pub fn cpu_limit(&self) -> Option<Duration> {
        self.cpu_limit
    }

    /// Optional per-request wall time limit.
    pub fn wall_timeout(&self) -> Option<Duration> {
        self.wall_timeout
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
    /// Must be called **after** the runtime is wrapped in `Rc<RefCell<>>`
    /// and the isolate is initialised (i.e. after `new_direct` /
    /// `new_with_plugins`).
    pub fn start_pump(self_ref: Rc<RefCell<Self>>) {
        let (notify_tx, notify_rx) = futures::channel::mpsc::channel::<()>(1);
        {
            let mut rt = self_ref.borrow_mut();
            rt.set_pump_notify(notify_tx);
        }

        let rt = self_ref.clone();
        compio::runtime::spawn(async move {
            Self::pump_loop(rt, notify_rx).await;
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
        use futures::StreamExt;
        let mut work = AsyncWork::new();

        loop {
            {
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

            if let Some(event) = event {
                let mut rt = runtime.borrow_mut();
                rt.enter_isolate();
                rt.handle_async_event(event, &mut work);
                // Flush any stream chunks enqueued during V8 execution.
                rt.flush_outbound_streams();
                rt.exit_isolate();
            }
        }
    }

    // -----------------------------------------------------------------------
    // Initialization
    // -----------------------------------------------------------------------

    /// Returns true if an HTTP handler (`onRequest`) is available.
    pub fn has_http_handler(&self) -> bool {
        self.http_handler_fn.is_some()
    }

    /// Load polyfills, ES modules, and compile the dispatch function (once).
    pub(crate) fn ensure_initialized(&mut self) {
        if self.initialized {
            return;
        }

        let modules = self.modules.clone();

        {
            v8::scope!(let handle_scope, &mut self.isolate);
            let context = v8::Local::new(handle_scope, &self.context);
            let scope = &mut v8::ContextScope::new(handle_scope, context);
            self.dispatch_fn = Some(load_polyfills_and_modules(scope, &modules, &self.plugins));

            // Check if __rpc.onRequest is a function. If so, cache a Global ref
            // for the native HTTP dispatch path.
            let global = context.global(scope);
            let rpc_key = v8::String::new(scope, "__rpc").unwrap();
            if let Some(rpc_obj) = global
                .get(scope, rpc_key.into())
                .and_then(|v| v.to_object(scope))
            {
                let on_request_key = v8::String::new(scope, "onRequest").unwrap();
                if let Some(handler) = rpc_obj.get(scope, on_request_key.into()) {
                    if handler.is_function() {
                        let func = v8::Local::<v8::Function>::try_from(handler).unwrap();
                        self.http_handler_fn = Some(v8::Global::new(scope, func));
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
        #[cfg(target_os = "linux")]
        if self.cpu_limit.is_some() {
            let system = crate::cpu_timer::CpuTimerSystem::get_or_init();
            let app_id = 0u64;
            let v8_handle = self.isolate.thread_safe_handle();
            system.register(app_id, v8_handle);
            match crate::cpu_timer::CpuTimer::new(app_id) {
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
    fn check_v8_terminated(&mut self) -> bool {
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

    /// Dispatch a JSON-RPC request synchronously into V8. Returns the result
    /// immediately for sync handlers (ping, fib, promiseChain, uuid, crypto).
    /// For async handlers that produce a pending promise, fires ready timers
    /// and checks settlement. Returns an error if the promise remains pending
    /// (truly async ops like fetch are not yet supported in channel-free mode).
    pub fn dispatch_rpc(&mut self, body: &str) -> Result<RequestResult, String> {
        self.ensure_initialized();

        if self.dispatch_fn.is_none() {
            return Err("Isolate not initialized".to_string());
        }

        let request_id = self.next_direct_request_id;
        self.next_direct_request_id += 1;
        let wall_start = Instant::now();

        self.state.borrow_mut().executing_request_id = Some(request_id);

        self.arm_cpu_timer();
        let dispatch_result = {
            let dispatch_fn = self.dispatch_fn.as_ref().unwrap();
            enter_v8!(self, |scope| {
                crate::dispatch::dispatch_request(scope, &self.state, dispatch_fn, body)
            })
        };
        self.disarm_cpu_timer();

        if self.check_v8_terminated() {
            self.state.borrow_mut().executing_request_id = None;
            return Err("CPU time limit exceeded".into());
        }

        let cpu_dispatch = wall_start.elapsed();

        match dispatch_result {
            DispatchResult::Sync(json) => {
                self.state.borrow_mut().executing_request_id = None;
                let logs = self.drain_request_logs(request_id);
                Ok(RequestResult {
                    json,
                    cpu_time: cpu_dispatch,
                    wall_time: cpu_dispatch,
                    logs,
                })
            }
            DispatchResult::Async(promise) => {
                // Try to settle inline: fire ready timers
                self.fire_ready_timers_inline();

                // Check if the promise settled after microtask checkpoint + ready timers
                self.arm_cpu_timer();
                let result = enter_v8!(self, |scope| {
                    crate::dispatch::extract_promise_result(scope, &promise)
                });
                self.disarm_cpu_timer();

                if self.check_v8_terminated() {
                    return Err("CPU time limit exceeded".into());
                }

                let cpu_total = wall_start.elapsed();

                self.state.borrow_mut().executing_request_id = None;
                match result {
                    Ok(json) => {
                        let logs = self.drain_request_logs(request_id);
                        Ok(RequestResult {
                            json,
                            cpu_time: cpu_total,
                            wall_time: cpu_total,
                            logs,
                        })
                    }
                    Err(_) => {
                        // Promise still pending — async ops not supported in direct dispatch
                        Err("Promise did not settle synchronously (async ops not supported in channel-free mode)".to_string())
                    }
                }
            }
            DispatchResult::Error(msg) => {
                self.state.borrow_mut().executing_request_id = None;
                Err(msg)
            }
        }
    }

    // -----------------------------------------------------------------------
    // Two-phase async dispatch (used with pump task)
    // -----------------------------------------------------------------------

    /// Phase 1: Dispatch a request into V8. Returns immediately.
    ///
    /// - **Sync handler**: returns `DispatchOutcome::Complete(Ok(result))`
    /// - **Async handler**: stores a `PendingRequest`, returns
    ///   `DispatchOutcome::Pending(receiver)` — the pump task will send the
    ///   result when the promise settles.
    /// - **Error**: returns `DispatchOutcome::Complete(Err(msg))`
    ///
    /// The caller must NOT hold the RefCell borrow across any `.await`.
    pub fn dispatch_start(&mut self, body: &str) -> DispatchOutcome {
        self.ensure_initialized();

        if self.dispatch_fn.is_none() {
            return DispatchOutcome::Complete(Err("Isolate not initialized".to_string()));
        }

        let request_id = self.next_direct_request_id;
        self.next_direct_request_id += 1;

        let wall_start = Instant::now();

        // Set executing_request_id so console.log routes to this request
        self.state.borrow_mut().executing_request_id = Some(request_id);

        self.arm_cpu_timer();
        let dispatch_result = {
            let dispatch_fn = self.dispatch_fn.as_ref().unwrap();
            enter_v8!(self, |scope| {
                crate::dispatch::dispatch_request(scope, &self.state, dispatch_fn, body)
            })
        };
        self.disarm_cpu_timer();

        if self.check_v8_terminated() {
            return DispatchOutcome::Complete(Err("CPU time limit exceeded".into()));
        }

        let cpu_dispatch = wall_start.elapsed();

        match dispatch_result {
            DispatchResult::Sync(json) => {
                self.state.borrow_mut().executing_request_id = None;
                let logs = self.drain_request_logs(request_id);
                DispatchOutcome::Complete(Ok(RequestResult {
                    json,
                    cpu_time: cpu_dispatch,
                    wall_time: cpu_dispatch,
                    logs,
                }))
            }
            DispatchResult::Async(promise) => {
                // Fire zero-delay timers inline — this settles setTimeout(0) immediately
                // without a round-trip through the pump task.
                self.fire_ready_timers_inline();

                if self.check_v8_terminated() {
                    self.state.borrow_mut().executing_request_id = None;
                    return DispatchOutcome::Complete(Err("CPU time limit exceeded".into()));
                }

                // Check if promise settled after microtask checkpoint + ready timers
                self.arm_cpu_timer();
                let result = enter_v8!(self, |scope| {
                    crate::dispatch::extract_promise_result(scope, &promise)
                });
                self.disarm_cpu_timer();

                if self.check_v8_terminated() {
                    self.state.borrow_mut().executing_request_id = None;
                    return DispatchOutcome::Complete(Err("CPU time limit exceeded".into()));
                }

                let cpu_total = wall_start.elapsed();

                match result {
                    Ok(json) => {
                        // CPU limit check for inline-settled async requests
                        if let Some(limit) = self.cpu_limit {
                            if cpu_total > limit {
                                self.state.borrow_mut().executing_request_id = None;
                                return DispatchOutcome::Complete(Err("CPU time limit exceeded".into()));
                            }
                        }
                        // Promise settled synchronously (e.g. Promise.resolve chains, setTimeout(0))
                        self.state.borrow_mut().executing_request_id = None;
                        let logs = self.drain_request_logs(request_id);
                        DispatchOutcome::Complete(Ok(RequestResult {
                            json,
                            cpu_time: cpu_total,
                            wall_time: cpu_total,
                            logs,
                        }))
                    }
                    Err(_) => {
                        // Promise is truly pending — needs the pump to drive it
                        self.state.borrow_mut().executing_request_id = None;
                        let (tx, rx) = channel::result_slot();
                        self.pending_requests.insert(request_id, PendingRequest {
                            id: request_id,
                            promise,
                            reply_direct: Some(tx),
                            reply_http: None,
                            is_http: false,
                            cpu_accumulated: cpu_total,
                            wall_start,
                            cancel: CancelFlag::new(),
                        });
                        // Notify the pump that new work was added
                        self.notify_pump();
                        DispatchOutcome::Pending(rx)
                    }
                }
            }
            DispatchResult::Error(msg) => {
                self.state.borrow_mut().executing_request_id = None;
                DispatchOutcome::Complete(Err(msg))
            }
        }
    }

    // -----------------------------------------------------------------------
    // HTTP dispatch (direct mode, used with pump task)
    // -----------------------------------------------------------------------

    /// Dispatch an HTTP request by calling `onRequest(Request)` directly.
    /// Returns immediately with a DispatchOutcome variant.
    pub fn dispatch_http(
        &mut self,
        method: &str,
        url: &str,
        headers_json: &str,
        body: &str,
    ) -> DispatchOutcome {
        self.ensure_initialized();

        if self.http_handler_fn.is_none() {
            return DispatchOutcome::Complete(Err("No onRequest handler exported".to_string()));
        }
        if self.http_create_request_fn.is_none() {
            return DispatchOutcome::Complete(Err("HTTP request helper not compiled".to_string()));
        }

        let request_id = self.next_direct_request_id;
        self.next_direct_request_id += 1;

        let wall_start = Instant::now();

        // Enter V8: construct Request, call handler, inspect result
        self.arm_cpu_timer();
        let dispatch_result: Result<Result<ResponseInfo, String>, v8::Global<v8::Promise>> =
            enter_v8!(self, |scope| {
                let undefined = v8::undefined(scope).into();

                // 1. Construct JS Request via helper
                let create_fn = v8::Local::new(scope, self.http_create_request_fn.as_ref().unwrap());
                let method_val = v8::String::new(scope, method).unwrap().into();
                let url_val = v8::String::new(scope, url).unwrap().into();
                let headers_val = v8::String::new(scope, headers_json).unwrap().into();
                let body_val = v8::String::new(scope, body).unwrap().into();

                let request_opt = create_fn.call(scope, undefined, &[method_val, url_val, headers_val, body_val]);
                if request_opt.is_none() {
                    Ok(Err("Failed to construct Request object".to_string()))
                } else {
                    let request = request_opt.unwrap();

                    // 2. Call onRequest(request)
                    let handler = v8::Local::new(scope, self.http_handler_fn.as_ref().unwrap());
                    dispatch_http_inner(scope, handler, undefined, request)
                }
            });
        self.disarm_cpu_timer();

        if self.check_v8_terminated() {
            return DispatchOutcome::Complete(Err("CPU time limit exceeded".into()));
        }

        let cpu_elapsed = wall_start.elapsed();

        match dispatch_result {
            Ok(Ok(info)) => {
                self.build_http_outcome(request_id, info, cpu_elapsed)
            }
            Ok(Err(msg)) => DispatchOutcome::Complete(Err(msg)),
            Err(promise) => {
                // Async — store as PendingRequest with is_http=true
                let (tx, rx) = channel::result_slot();
                self.pending_requests.insert(request_id, PendingRequest {
                    id: request_id,
                    promise,
                    reply_direct: None,
                    reply_http: Some(tx),
                    is_http: true,
                    cpu_accumulated: cpu_elapsed,
                    wall_start,
                    cancel: CancelFlag::new(),
                });
                self.notify_pump();
                DispatchOutcome::HttpPending(rx)
            }
        }
    }

    /// Convert a ResponseInfo into the appropriate DispatchOutcome.
    fn build_http_outcome(
        &mut self,
        request_id: u64,
        info: ResponseInfo,
        _cpu_time: Duration,
    ) -> DispatchOutcome {
        let logs = self.drain_request_logs(request_id);
        match info {
            ResponseInfo::Complete { status, headers, body } => {
                DispatchOutcome::HttpComplete { status, headers, body, logs }
            }
            ResponseInfo::Stream { status, headers, stream_id } => {
                let (writer, reader) = channel::stream_buffer();
                let mut forwarder = StreamForwarder::new(writer);

                // Flush any chunks already buffered in the stream state.
                // If start() was async and already completed, chunks + close
                // may already be in the buffer.
                let already_closed = {
                    let mut s = self.state.borrow_mut();
                    let closed = if let Some(stream) = s.streams.get_mut(&stream_id) {
                        for chunk in stream.buffer.drain(..) {
                            forwarder.try_forward(chunk);
                        }
                        stream.closed
                    } else {
                        false
                    };
                    s.outbound_streams.insert(stream_id);
                    closed
                };

                if already_closed {
                    // Stream completed before we started reading — close writer
                    // so the reader sees is_done() immediately.
                    forwarder.writer.close();
                } else {
                    self.stream_forwarders.insert(stream_id, forwarder);
                }
                DispatchOutcome::HttpStream { status, headers, body: reader, logs }
            }
            ResponseInfo::WebSocket { ws_id, headers } => {
                DispatchOutcome::WebSocketUpgrade { ws_id, headers }
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
            let future = crate::fetch::execute_fetch(fetch_req);
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
                    let mut s = self.state.borrow_mut();
                    s.executing_request_id = Some(rid);
                    // compio fetch ignores cancellation — no CancellationToken needed
                    s.executing_request_cancel = None;
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
                            if let Some(tx) = req.reply_direct {
                                tx.send(Err("CPU time limit exceeded".into()));
                            } else if let Some(tx) = req.reply_http {
                                tx.send(Err("CPU time limit exceeded".into()));
                            }
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
                    let mut s = self.state.borrow_mut();
                    s.executing_request_id = Some(rid);
                    s.executing_request_cancel = None;
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
                            if let Some(tx) = req.reply_direct {
                                tx.send(Err("CPU time limit exceeded".into()));
                            } else if let Some(tx) = req.reply_http {
                                tx.send(Err("CPU time limit exceeded".into()));
                            }
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
            let mut s = self.state.borrow_mut();
            s.executing_request_id = Some(rid);
            s.executing_request_cancel = None;
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
                    if let Some(tx) = req.reply_direct {
                        tx.send(Err("CPU time limit exceeded".into()));
                    } else if let Some(tx) = req.reply_http {
                        tx.send(Err("CPU time limit exceeded".into()));
                    }
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
    fn fire_ready_timers_inline(&mut self) {
        loop {
            let timer_id = {
                let mut s = self.state.borrow_mut();
                if s.ready_timers.is_empty() { None } else { Some(s.ready_timers.remove(0)) }
            };
            let Some(timer_id) = timer_id else { break };

            self.arm_cpu_timer();
            enter_v8!(self, |scope| {
                crate::dispatch::fire_timer_callback(scope, &self.state, timer_id);
            });
            self.disarm_cpu_timer();

            if self.check_v8_terminated() {
                return;
            }

            self.state.borrow_mut().timer_owner.remove(&timer_id);
        }
    }

    /// Fire zero-delay timers, draining new tasks into external AsyncWork.
    fn fire_ready_timers_pump(&mut self, work: &mut AsyncWork) {
        loop {
            let timer_id = {
                let mut s = self.state.borrow_mut();
                if s.ready_timers.is_empty() { None } else { Some(s.ready_timers.remove(0)) }
            };
            let Some(timer_id) = timer_id else { break };

            let owner_request_id = self.state.borrow().timer_owner.get(&timer_id).copied();

            if let Some(rid) = owner_request_id {
                let mut s = self.state.borrow_mut();
                s.executing_request_id = Some(rid);
                s.executing_request_cancel = None;
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
                        if let Some(tx) = req.reply_direct {
                            tx.send(Err("CPU time limit exceeded".into()));
                        } else if let Some(tx) = req.reply_http {
                            tx.send(Err("CPU time limit exceeded".into()));
                        }
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
        let wall_time = req.wall_start.elapsed();

        match settled {
            SettledResult::Rpc(Ok(json)) => {
                let logs = self.drain_request_logs(id);
                let result = RequestResult {
                    json,
                    cpu_time,
                    wall_time,
                    logs,
                };
                if let Some(tx) = req.reply_direct {
                    tx.send(Ok(result));
                }
            }
            SettledResult::Rpc(Err(msg)) => {
                if let Some(tx) = req.reply_direct {
                    tx.send(Err(msg));
                }
            }
            SettledResult::Http(Ok(info)) => {
                self.send_http_settled(id, info, req.reply_http, cpu_time);
            }
            SettledResult::Http(Err(msg)) => {
                if let Some(tx) = req.reply_http {
                    tx.send(Err(msg));
                }
            }
        }
    }

    /// Send an HTTP response for a settled async HTTP request.
    fn send_http_settled(
        &mut self,
        id: u64,
        info: ResponseInfo,
        reply_http: Option<ResultSender<Result<HttpDispatchResult, String>>>,
        _cpu_time: Duration,
    ) {
        let logs = self.drain_request_logs(id);
        match info {
            ResponseInfo::Complete { status, headers, body } => {
                if let Some(tx) = reply_http {
                    tx.send(Ok(HttpDispatchResult::Complete { status, headers, body, logs }));
                }
            }
            ResponseInfo::Stream { status, headers, stream_id } => {
                let (writer, reader) = channel::stream_buffer();
                let mut forwarder = StreamForwarder::new(writer);

                // Flush any chunks already buffered
                {
                    let mut s = self.state.borrow_mut();
                    if let Some(stream) = s.streams.get_mut(&stream_id) {
                        for chunk in stream.buffer.drain(..) {
                            forwarder.try_forward(chunk);
                        }
                    }
                    s.outbound_streams.insert(stream_id);
                }

                self.stream_forwarders.insert(stream_id, forwarder);

                if let Some(tx) = reply_http {
                    tx.send(Ok(HttpDispatchResult::Stream { status, headers, body: reader, logs }));
                }
            }
            ResponseInfo::WebSocket { ws_id, headers } => {
                if let Some(tx) = reply_http {
                    tx.send(Ok(HttpDispatchResult::WebSocket { ws_id, headers, logs }));
                }
            }
        }
    }

    /// Returns true if there are pending async requests.
    pub fn has_pending_requests(&self) -> bool {
        !self.pending_requests.is_empty()
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn drain_request_logs(&mut self, request_id: u64) -> Vec<String> {
        self.state
            .borrow_mut()
            .per_request_logs
            .remove(&request_id)
            .unwrap_or_default()
    }

    fn clear_executing_request(&self) {
        let mut s = self.state.borrow_mut();
        s.executing_request_id = None;
        s.executing_request_cancel = None;
    }

    /// Check if a pending request has exceeded its CPU limit. If so, remove
    /// it and send an error via the reply slot.
    fn check_cpu_limit(&mut self, request_id: u64) {
        let Some(cpu_limit) = self.cpu_limit else { return };
        let Some(req) = self.pending_requests.get(&request_id) else { return };
        if req.cpu_accumulated > cpu_limit {
            let req = self.pending_requests.remove(&request_id).unwrap();
            let _logs = self.drain_request_logs(request_id);
            if let Some(tx) = req.reply_direct {
                tx.send(Err("CPU time limit exceeded".into()));
            } else if let Some(tx) = req.reply_http {
                tx.send(Err("CPU time limit exceeded".into()));
            }
        }
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
        for id in cancelled {
            if let Some(req) = self.pending_requests.remove(&id) {
                let _logs = self.drain_request_logs(id);
                if let Some(tx) = req.reply_direct {
                    tx.send(Err("Request timed out".into()));
                } else if let Some(tx) = req.reply_http {
                    tx.send(Err("Request timed out".into()));
                }
            }
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
            let result = http::extract_settled_result(scope, &req.promise, req.is_http);
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
/// borrow conflicts (self.http_handler_fn borrowed while enter_v8! borrows self).
fn dispatch_http_inner(
    scope: &mut v8::PinScope,
    handler: v8::Local<v8::Function>,
    undefined: v8::Local<v8::Value>,
    request: v8::Local<v8::Value>,
) -> Result<Result<ResponseInfo, String>, v8::Global<v8::Promise>> {
    let result_opt = handler.call(scope, undefined, &[request]);
    if result_opt.is_none() {
        Ok(Err("onRequest threw an exception".to_string()))
    } else {
        let result = result_opt.unwrap();
        scope.perform_microtask_checkpoint();
        if result.is_promise() {
            let promise = v8::Local::<v8::Promise>::try_from(result).unwrap();
            match promise.state() {
                v8::PromiseState::Fulfilled => {
                    let resolved = promise.result(scope);
                    Ok(http::inspect_response(scope, resolved))
                }
                v8::PromiseState::Rejected => {
                    let msg = promise.result(scope)
                        .to_string(scope)
                        .map(|s| s.to_rust_string_lossy(scope))
                        .unwrap_or_else(|| "Promise rejected".to_string());
                    Ok(Err(msg))
                }
                v8::PromiseState::Pending => {
                    Err(v8::Global::new(scope, promise))
                }
            }
        } else {
            Ok(http::inspect_response(scope, result))
        }
    }
}
