//! Runtime — compio event loop with V8 isolate.
//!
//! Same architecture as runtime-tokio's Runtime, but uses compio for timers
//! and the outer event loop. V8 dispatch is identical (appbase-v8-core).
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

use appbase_v8_core::init::{init_v8, load_polyfills_and_modules, RequestResult};
use appbase_v8_core::http::{self, ResponseInfo, SettledResult, HTTP_CREATE_REQUEST_JS};
use appbase_v8_core::modules::ModuleEntry;
use appbase_v8_core::state::{
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
    cpu_timer: Option<appbase_v8_core::cpu_timer::CpuTimer>,
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
        init_v8();

        let params = v8::CreateParams::default().heap_limits(0, 128 * 1024 * 1024);
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
    fn notify_pump(&self) {
        if let Some(tx) = &self.pump_notify_tx {
            let _ = tx.clone().try_send(());
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
            self.dispatch_fn = Some(load_polyfills_and_modules(scope, &modules));

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
            let system = appbase_v8_core::cpu_timer::CpuTimerSystem::get_or_init();
            let app_id = 0u64;
            let v8_handle = self.isolate.thread_safe_handle();
            system.register(app_id, v8_handle);
            match appbase_v8_core::cpu_timer::CpuTimer::new(app_id) {
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
    fn check_v8_terminated(&mut self) -> bool {
        if !self.isolate.is_execution_terminating() {
            return false;
        }
        self.isolate.cancel_terminate_execution();
        self.disarm_cpu_timer();
        // Drain ALL pending requests with CPU limit error
        for (_id, req) in self.pending_requests.drain() {
            if let Some(tx) = req.reply_direct {
                tx.send(Err("CPU time limit exceeded".into()));
            } else if let Some(tx) = req.reply_http {
                tx.send(Err("CPU time limit exceeded".into()));
            }
        }
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
                appbase_v8_core::request::dispatch_request(scope, &self.state, dispatch_fn, body)
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
                    appbase_v8_core::request::extract_promise_result(scope, &promise)
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
                appbase_v8_core::request::dispatch_request(scope, &self.state, dispatch_fn, body)
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
                    appbase_v8_core::request::extract_promise_result(scope, &promise)
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

                // Flush any chunks already buffered in the stream state
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
                DispatchOutcome::HttpStream { status, headers, body: reader, logs }
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
        let fetches: Vec<appbase_v8_core::state::FetchRequest> = {
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
                    appbase_v8_core::request::resolve_op(scope, &self.state, op_id, &value);
                    collect_settled_promises(scope, &mut self.pending_requests)
                });
                self.disarm_cpu_timer();

                if self.check_v8_terminated() {
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
                        appbase_v8_core::streams::push_stream_chunk(scope, &self.state, stream_id, &data, done);
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
            appbase_v8_core::request::fire_timer_callback(scope, &self.state, id);
            collect_settled_promises(scope, &mut self.pending_requests)
        });
        self.disarm_cpu_timer();

        if self.check_v8_terminated() {
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
                appbase_v8_core::request::fire_timer_callback(scope, &self.state, timer_id);
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
                appbase_v8_core::request::fire_timer_callback(scope, &self.state, timer_id);
                collect_settled_promises(scope, &mut self.pending_requests)
            });
            self.disarm_cpu_timer();

            if self.check_v8_terminated() {
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
