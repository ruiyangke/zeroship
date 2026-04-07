//! Per-request V8 isolate with persistent context.
//!
//! Context and compiled code persist across requests (like workerd).
//! Each request enters the existing context, calls a pre-stored handler.
//! CPU time measured per-request via CLOCK_THREAD_CPUTIME_ID.
//!
//! Uses `state::RuntimeState` + `state::SharedState` for all V8 callback state.
//! Async ops are driven by a small blocking event loop that polls futures from
//! `RuntimeState::spawned_ops` via a one-shot tokio runtime.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use std::collections::HashMap;

use crate::modules::ModuleEntry;
use crate::init::{load_polyfills_and_modules, thread_cpu_time, HttpResult, RequestResult};
use crate::state::{DispatchResult, OpResult, RuntimeState, SharedState, SpawnedTimer};

/// A V8 isolate with persistent context -- compiled code stays across requests.
/// ES modules are compiled ONCE. Each request just calls the handler function.
pub struct Isolate {
    isolate: v8::OwnedIsolate,
    context: v8::Global<v8::Context>,
    dispatch_fn: Option<v8::Global<v8::Function>>,
    /// Cached direct handle to the user's `onRequest` function.
    on_request_fn: Option<v8::Global<v8::Function>>,
    /// Single JS dispatch: `(handler, method, url, headers_json, body) → Response object`
    /// Handler is passed as first arg — no property lookup needed.
    http_dispatch_fn: Option<v8::Global<v8::Function>>,
    initialized: bool,
    modules: Vec<ModuleEntry>,
    state: SharedState,
}

impl Isolate {
    /// Create a new isolate for ES module format (`export function ...`).
    /// Call `init_v8()` before creating isolates.
    ///
    /// `env_vars` — per-app environment variables accessible via `env.get(key)`.
    pub fn new(modules: Vec<ModuleEntry>, env_vars: HashMap<String, String>) -> Self {
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

        let rt_state = RuntimeState::new(env_vars);
        let state: SharedState = Rc::new(RefCell::new(rt_state));
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
            on_request_fn: None,
            http_dispatch_fn: None,
            initialized: false,
            modules,
            state,
        }
    }

    /// Lazy initialization: load ES modules + compile dispatch function (once).
    fn ensure_initialized(&mut self) {
        if self.initialized {
            return;
        }

        // Clone modules so we don't borrow self during V8 scope
        let modules = self.modules.clone();

        v8::scope!(let handle_scope, &mut self.isolate);
        let context = v8::Local::new(handle_scope, &self.context);
        let scope = &mut v8::ContextScope::new(handle_scope, context);

        self.dispatch_fn = Some(load_polyfills_and_modules(scope, &modules));

        // Cache onRequest handler directly as Global<Function> (if exported).
        // Eliminates the __rpc.onRequest property lookup on every request.
        {
            let global = context.global(scope);
            let rpc_key = v8::String::new(scope, "__rpc").unwrap();
            if let Some(rpc_obj) = global.get(scope, rpc_key.into()) {
                let on_request_key = v8::String::new(scope, "onRequest").unwrap();
                if let Some(handler) = rpc_obj.to_object(scope).and_then(|obj| obj.get(scope, on_request_key.into())) {
                    if handler.is_function() {
                        let func = v8::Local::<v8::Function>::try_from(handler).unwrap();
                        self.on_request_fn = Some(v8::Global::new(scope, func));
                    }
                }
            }
        }

        // Compile optimized HTTP dispatch function
        if self.on_request_fn.is_some() {
            let code = v8::String::new(scope, r#"(function(__handler, __method, __url, __headers_json, __body) {
    function __ensureBody(resp) {
        if (resp && resp._isStreamBody && resp.body) {
            return resp.text().then(function(body) {
                resp._bodyText = body;
                resp._isStreamBody = false;
                return resp;
            });
        }
        return resp;
    }

    try {
        var map = Object.create(null);
        if (__headers_json) {
            var arr = JSON.parse(__headers_json);
            for (var i = 0; i < arr.length; i++) {
                var k = arr[i][0].toLowerCase(), v = arr[i][1];
                if (map[k]) map[k].push(v); else map[k] = [v];
            }
        }
        var reqInit = { method: __method, headers: Headers._fromTrusted(map) };
        if (__body && __method !== "GET" && __method !== "HEAD") reqInit.body = __body;
        var req = new Request(__url, reqInit);

        var result = __handler(req);

        if (result && typeof result.then === "function") {
            return result.then(function(resp) {
                if (!resp || resp.status === undefined) return new Response(String(resp), { status: 200 });
                return __ensureBody(resp);
            }, function(e) {
                return new Response(e.message || String(e), { status: 500 });
            });
        }
        if (!result || result.status === undefined) {
            return new Response(String(result), { status: 200 });
        }
        return __ensureBody(result);
    } catch(e) {
        return new Response(e.message || String(e), { status: 500 });
    }
})"#).unwrap();
            let s = v8::Script::compile(scope, code, None).unwrap();
            let r = s.run(scope).unwrap();
            self.http_dispatch_fn = Some(v8::Global::new(scope, v8::Local::<v8::Function>::try_from(r).unwrap()));
        }

        self.initialized = true;
    }

    /// Check if the app exports an onRequest handler.
    pub fn has_http_handler(&mut self) -> bool {
        self.ensure_initialized();
        self.on_request_fn.is_some()
    }

    /// Execute an HTTP request via cached onRequest `Global<Function>`.
    /// Single Rust→JS call: handler passed as first arg, no property lookup.
    /// Returns None if onRequest is not exported.
    pub fn execute_http(
        &mut self,
        method: &str,
        url: &str,
        headers_json: &str,
        body: &str,
    ) -> Option<Result<HttpResult, String>> {
        self.ensure_initialized();

        if self.on_request_fn.is_none() {
            return None;
        }

        // Drain stale state
        {
            let mut s = self.state.borrow_mut();
            s.timer_callbacks.clear();
            s.pending_resolvers.clear();
            s.streams.clear();
            s.spawned_ops.clear();
            s.spawned_timers.clear();
            s.ready_timers.clear();
        }

        // Set request context for logging
        self.state.borrow_mut().executing_request_id = Some(0);

        let wall_start = std::time::Instant::now();
        let cpu_start = thread_cpu_time();

        v8::scope!(let handle_scope, &mut self.isolate);
        let context = v8::Local::new(handle_scope, &self.context);
        let scope = &mut v8::ContextScope::new(handle_scope, context);
        let undefined = v8::undefined(scope).into();

        // Single JS call: dispatch_fn(handler, method, url, headers_json, body)
        let dispatch = v8::Local::new(scope, self.http_dispatch_fn.as_ref().unwrap());
        let handler = v8::Local::new(scope, self.on_request_fn.as_ref().unwrap());
        let arg_method = v8::String::new(scope, method).unwrap();
        let arg_url = v8::String::new(scope, url).unwrap();
        let arg_headers = v8::String::new(scope, headers_json).unwrap();
        let arg_body = v8::String::new(scope, body).unwrap();

        let result = dispatch.call(scope, undefined, &[
            handler.into(), arg_method.into(), arg_url.into(), arg_headers.into(), arg_body.into()
        ]);

        // Resolve the dispatch result to a V8 Response object.
        let resp_val = match result {
            Some(val) if val.is_null_or_undefined() => return None,
            Some(val) if val.is_promise() => {
                let promise = v8::Local::<v8::Promise>::try_from(val).unwrap();
                let global_promise = v8::Global::new(scope, promise);
                run_blocking_event_loop(scope, &self.state, Some(&global_promise), Duration::from_secs(30));
                let promise = v8::Local::new(scope, &global_promise);
                match promise.state() {
                    v8::PromiseState::Fulfilled => promise.result(scope),
                    v8::PromiseState::Rejected => {
                        let msg = promise.result(scope).to_rust_string_lossy(scope);
                        return Some(Err(format!("onRequest rejected: {msg}")));
                    }
                    v8::PromiseState::Pending => return Some(Err("onRequest promise still pending".into())),
                }
            }
            Some(val) => val,
            None => return Some(Err("onRequest call failed".into())),
        };

        // Extract response fields directly from V8 object — no JSON serialization.
        Some(extract_http_result(scope, resp_val, cpu_start, wall_start, &self.state))
    }

    /// Execute a single RPC request. Returns the JSON response + timing info.
    pub fn execute_request(&mut self, request_json: &str) -> Result<RequestResult, String> {
        self.ensure_initialized();

        // Drain stale state from prior requests
        {
            let mut s = self.state.borrow_mut();
            s.timer_callbacks.clear();
            s.pending_resolvers.clear();
            s.streams.clear();
            s.spawned_ops.clear();
            s.spawned_timers.clear();
            s.ready_timers.clear();
        }

        // Set request context for logging
        self.state.borrow_mut().executing_request_id = Some(0);

        let wall_start = Instant::now();
        let cpu_start = thread_cpu_time();

        v8::scope!(let handle_scope, &mut self.isolate);
        let context = v8::Local::new(handle_scope, &self.context);
        let scope = &mut v8::ContextScope::new(handle_scope, context);

        let dispatch_fn = self.dispatch_fn.as_ref().unwrap();

        let dispatch_result = crate::request::dispatch_request(scope, &self.state, dispatch_fn, request_json);

        let json = match dispatch_result {
            DispatchResult::Sync(json) => {
                // Run any pending timers/ops (fire-and-forget side effects)
                run_blocking_event_loop(scope, &self.state, None, Duration::from_secs(60));
                json
            }
            DispatchResult::Async(promise) => {
                run_blocking_event_loop(scope, &self.state, Some(&promise), Duration::from_secs(30));

                match crate::request::extract_promise_result(scope, &promise) {
                    Ok(json) => json,
                    Err(msg) => return Err(msg),
                }
            }
            DispatchResult::Error(msg) => return Err(msg),
        };

        let cpu_time = thread_cpu_time().saturating_sub(cpu_start);
        let wall_time = wall_start.elapsed();
        let logs = self.state.borrow_mut().per_request_logs.remove(&0).unwrap_or_default();

        Ok(RequestResult {
            json,
            cpu_time,
            wall_time,
            logs,
        })
    }
}

// ---------------------------------------------------------------------------
// Blocking event loop for per-request Isolate
// ---------------------------------------------------------------------------

use std::cmp::Reverse;
use std::collections::BinaryHeap;

/// Entry in the timer min-heap. Ordered by (fire_at, id).
#[derive(Eq, PartialEq)]
struct TimerHeapEntry {
    fire_at: Instant,
    id: u32,
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

/// Drive the event loop to completion (blocking).
///
/// Polls spawned ops and timers from `RuntimeState`. Uses a thread-local tokio
/// runtime for async ops (fetch, etc.). Fires timer callbacks, resolves promises.
///
/// - `promise: Some(p)` → stop when promise settles (or wall-time exceeded)
/// - `promise: None` → drive to exhaustion
fn run_blocking_event_loop(
    scope: &mut v8::PinScope,
    state: &SharedState,
    promise: Option<&v8::Global<v8::Promise>>,
    wall_timeout: Duration,
) {
    use std::sync::mpsc;
    use std::sync::Arc;
    use futures::task::AtomicWaker;

    let deadline = Instant::now() + wall_timeout;
    let tracking_promise = promise.is_some();

    // Event channel: spawned async ops send their results here.
    // This bridges the async op futures (driven by tokio) with the blocking V8 loop.
    let (event_tx, event_rx) = mpsc::channel::<OpResult>();
    let waker = Arc::new(AtomicWaker::new());

    // Timer heap for tracking absolute fire times
    let mut timer_heap: BinaryHeap<Reverse<TimerHeapEntry>> = BinaryHeap::new();

    loop {
        // Check if promise already settled
        if let Some(p) = promise {
            let local = v8::Local::new(scope, p);
            if local.state() != v8::PromiseState::Pending {
                return;
            }
        }

        // Wall-time check
        if tracking_promise && Instant::now() > deadline {
            return;
        }

        // Collect newly spawned timers into the heap
        {
            let now = Instant::now();
            let mut s = state.borrow_mut();
            let timers: Vec<SpawnedTimer> = s.spawned_timers.drain(..).collect();
            for timer in timers {
                timer_heap.push(Reverse(TimerHeapEntry {
                    fire_at: now + timer.delay,
                    id: timer.id,
                }));
            }
            // Drain ready_timers (zero-delay) — schedule them to fire immediately.
            for id in s.ready_timers.drain(..) {
                timer_heap.push(Reverse(TimerHeapEntry {
                    fire_at: now,
                    id,
                }));
            }
        }

        // Spawn new async ops: drain from state, spawn on tokio, send results to event_tx.
        spawn_async_ops(state, &event_tx, &waker);

        // Drain completed op results from the event channel
        let mut processed_any = false;
        while let Ok(result) = event_rx.try_recv() {
            processed_any = true;
            match result {
                OpResult::Completed { op_id, value, .. } => {
                    crate::request::resolve_op(scope, state, op_id, &value);
                }
                OpResult::StreamChunk { stream_id, data, done } => {
                    crate::streams::push_stream_chunk(scope, state, stream_id, &data, done);
                    scope.perform_microtask_checkpoint();
                }
                OpResult::Cancelled => {}
            }
        }

        // Fire ready timers
        let any_timer_fired = fire_ready_timers(scope, state, &mut timer_heap);

        // Flush microtasks
        scope.perform_microtask_checkpoint();

        // Collect any new timers/ops spawned by timer callbacks or promise continuations
        {
            let now = Instant::now();
            let mut s = state.borrow_mut();
            let timers: Vec<SpawnedTimer> = s.spawned_timers.drain(..).collect();
            for timer in timers {
                timer_heap.push(Reverse(TimerHeapEntry {
                    fire_at: now + timer.delay,
                    id: timer.id,
                }));
            }
            for id in s.ready_timers.drain(..) {
                timer_heap.push(Reverse(TimerHeapEntry {
                    fire_at: now,
                    id,
                }));
            }
        }
        spawn_async_ops(state, &event_tx, &waker);

        // Re-check if promise settled after processing
        if let Some(p) = promise {
            let local = v8::Local::new(scope, p);
            if local.state() != v8::PromiseState::Pending {
                return;
            }
        }

        // Check if any work remains
        let has_work = {
            let s = state.borrow();
            !s.timer_callbacks.is_empty()
                || !s.pending_resolvers.is_empty()
                || !s.spawned_ops.is_empty()
                || !s.spawned_timers.is_empty()
                || !s.ready_timers.is_empty()
                || s.streams.values().any(|st| st.pending_read.is_some() && !st.closed)
        };
        let has_timers = !timer_heap.is_empty();

        if !has_work && !has_timers {
            return;
        }

        // If we processed something this tick, loop immediately
        if processed_any || any_timer_fired {
            continue;
        }

        // Compute wait timeout
        let next_timer_fire = timer_heap.peek().map(|Reverse(e)| e.fire_at);
        let wait_timeout = match next_timer_fire {
            Some(fire_at) => {
                let delay = fire_at.saturating_duration_since(Instant::now());
                if tracking_promise {
                    delay.min(deadline.saturating_duration_since(Instant::now()))
                } else {
                    delay
                }
            }
            None if has_work => {
                if tracking_promise {
                    Duration::from_secs(30).min(deadline.saturating_duration_since(Instant::now()))
                } else {
                    Duration::from_secs(60)
                }
            }
            None => return, // no more work possible
        };

        if wait_timeout.is_zero() {
            continue;
        }

        // Park the thread — woken by AtomicWaker when an op completes,
        // or by timeout when the next timer should fire.
        {
            use std::task::{Context, Wake};
            struct ParkWaker(std::thread::Thread);
            impl Wake for ParkWaker {
                fn wake(self: Arc<Self>) {
                    self.0.unpark();
                }
            }
            let parker = Arc::new(ParkWaker(std::thread::current()));
            let w = std::task::Waker::from(parker);
            let cx = Context::from_waker(&w);
            waker.register(cx.waker());
            std::thread::park_timeout(wait_timeout);
        }
    }
}

/// Drain spawned ops from `RuntimeState` and spawn them on a tokio runtime.
/// Results are sent through `event_tx`. The waker is triggered when a result arrives.
fn spawn_async_ops(
    state: &SharedState,
    event_tx: &std::sync::mpsc::Sender<OpResult>,
    waker: &std::sync::Arc<futures::task::AtomicWaker>,
) {
    let ops: Vec<_> = state.borrow_mut().spawned_ops.drain(..).collect();
    if ops.is_empty() {
        return;
    }

    // SAFETY: The spawned_ops futures are `dyn Future` (not `+ Send`) because
    // `RuntimeState` is `!Send`. However, the actual futures created by fetch.rs
    // and `#[appbase_op(async)]` only capture owned data (Strings, u32s, etc.)
    // and are in practice Send. We assert Send here to spawn them on tokio.
    struct SendFuture(std::pin::Pin<Box<dyn std::future::Future<Output = OpResult>>>);
    unsafe impl Send for SendFuture {}
    impl std::future::Future for SendFuture {
        type Output = OpResult;
        fn poll(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Self::Output> {
            self.0.as_mut().poll(cx)
        }
    }

    // Get or create a tokio handle
    let tokio_handle = tokio::runtime::Handle::try_current().ok();

    for op_future in ops {
        let event_tx = event_tx.clone();
        let waker = waker.clone();
        let send_future = SendFuture(op_future);

        match &tokio_handle {
            Some(handle) => {
                handle.spawn(async move {
                    let result = send_future.await;
                    let _ = event_tx.send(result);
                    waker.wake();
                });
            }
            None => {
                // No tokio runtime — spawn a thread with its own runtime
                std::thread::spawn(move || {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("Failed to create tokio runtime for async op");
                    let result = rt.block_on(send_future);
                    let _ = event_tx.send(result);
                    waker.wake();
                });
            }
        }
    }
}

/// Fire all timers whose fire_at <= now.
fn fire_ready_timers(
    scope: &mut v8::PinScope,
    state: &SharedState,
    timer_heap: &mut BinaryHeap<Reverse<TimerHeapEntry>>,
) -> bool {
    let mut any_fired = false;
    let now = Instant::now();

    loop {
        let should_fire = timer_heap
            .peek()
            .map(|Reverse(e)| e.fire_at <= now)
            .unwrap_or(false);
        if !should_fire {
            break;
        }

        let entry = timer_heap.pop().unwrap().0;

        // Check if the timer callback still exists (may have been cleared)
        let has_cb = state.borrow().timer_callbacks.contains_key(&entry.id);
        if !has_cb {
            continue; // lazy deletion — timer was cleared
        }

        any_fired = true;
        crate::request::fire_timer_callback(scope, state, entry.id);
        scope.perform_microtask_checkpoint();

        // Re-arm interval timers (fire_timer_callback re-inserts the callback for intervals)
        let is_interval = state
            .borrow()
            .timer_callbacks
            .get(&entry.id)
            .and_then(|cb| cb.interval)
            .is_some();
        if is_interval {
            let interval = state.borrow().timer_callbacks.get(&entry.id).unwrap().interval.unwrap();
            timer_heap.push(Reverse(TimerHeapEntry {
                fire_at: Instant::now() + interval,
                id: entry.id,
            }));
        }
    }

    any_fired
}

/// Extract HTTP response fields directly from a V8 Response object.
fn extract_http_result(
    scope: &mut v8::PinScope,
    resp_val: v8::Local<v8::Value>,
    cpu_start: std::time::Duration,
    wall_start: std::time::Instant,
    state: &SharedState,
) -> Result<HttpResult, String> {
    let resp = resp_val.to_object(scope).ok_or("Response is not an object")?;

    // status
    let status_key = v8::String::new(scope, "status").unwrap();
    let status = resp.get(scope, status_key.into())
        .and_then(|v| v.uint32_value(scope))
        .unwrap_or(200) as u16;

    // body
    let body_key = v8::String::new(scope, "_bodyText").unwrap();
    let body = resp.get(scope, body_key.into())
        .filter(|v| !v.is_null_or_undefined())
        .map(|v| v.to_rust_string_lossy(scope))
        .unwrap_or_default();

    // headers
    let headers_key = v8::String::new(scope, "headers").unwrap();
    let mut headers: Vec<(String, String)> = Vec::new();
    if let Some(headers_obj) = resp.get(scope, headers_key.into()) {
        if let Some(headers_obj) = headers_obj.to_object(scope) {
            let map_key = v8::String::new(scope, "_map").unwrap();
            if let Some(map_val) = headers_obj.get(scope, map_key.into()) {
                if let Some(map_obj) = map_val.to_object(scope) {
                    if let Some(names) = map_obj.get_own_property_names(scope, Default::default()) {
                        for i in 0..names.length() {
                            let key = names.get_index(scope, i).unwrap();
                            let key_str = key.to_rust_string_lossy(scope);
                            if let Some(val_arr) = map_obj.get(scope, key) {
                                if let Ok(arr) = v8::Local::<v8::Array>::try_from(val_arr) {
                                    for j in 0..arr.length() {
                                        if let Some(v) = arr.get_index(scope, j) {
                                            headers.push((key_str.clone(), v.to_rust_string_lossy(scope)));
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    let logs = state.borrow_mut().per_request_logs.remove(&0).unwrap_or_default();

    Ok(HttpResult {
        status,
        headers,
        body,
        cpu_time: thread_cpu_time().saturating_sub(cpu_start),
        wall_time: wall_start.elapsed(),
        logs,
    })
}

// ---------------------------------------------------------------------------
// Isolate pool
// ---------------------------------------------------------------------------

/// Pool of V8 isolates for per-request model.
/// Each isolate has a persistent context with pre-compiled handlers.
pub struct IsolatePool {
    available: std::sync::Mutex<Vec<Isolate>>,
    modules: Vec<ModuleEntry>,
    env_vars: HashMap<String, String>,
    max_size: usize,
}

// SAFETY: Isolates are only accessed by one thread at a time via the Mutex.
#[allow(unsafe_code)]
unsafe impl Send for IsolatePool {}
#[allow(unsafe_code)]
unsafe impl Sync for IsolatePool {}

impl IsolatePool {
    pub fn new(modules: Vec<ModuleEntry>, env_vars: HashMap<String, String>, max_size: usize) -> Self {
        Self {
            available: std::sync::Mutex::new(Vec::new()),
            modules,
            env_vars,
            max_size,
        }
    }

    pub fn execute(&self, request_json: &str) -> Result<RequestResult, String> {
        let mut isolate = {
            let mut pool = self.available.lock().unwrap();
            pool.pop()
        }
        .unwrap_or_else(|| Isolate::new(self.modules.clone(), self.env_vars.clone()));

        let result = isolate.execute_request(request_json);

        {
            let mut pool = self.available.lock().unwrap();
            if pool.len() < self.max_size {
                pool.push(isolate);
            }
        }

        result
    }
}
