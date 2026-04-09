//! Runtime — compio event loop with V8 isolate.
//!
//! Same architecture as runtime-tokio's Runtime, but uses compio for timers
//! and the outer event loop. V8 dispatch is identical (appbase-v8-core).
//!
//! The main difference: `compio::time::sleep` replaces `tokio::time::sleep`,
//! and `futures::select!` replaces `tokio::select!`.

#![allow(unsafe_code)]

use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::time::{Duration, Instant};

use futures::stream::FuturesUnordered;
use futures::{FutureExt, StreamExt};
use tokio_util::sync::CancellationToken;

use appbase_v8_core::init::{init_v8, load_polyfills_and_modules, RequestResult};
use appbase_v8_core::modules::ModuleEntry;
use appbase_v8_core::state::{
    DispatchResult, IncomingRequest, OpResult, RequestKind, RequestReply,
    RuntimeState, SharedState, SpawnedTimer, TimerResult,
};

// ---------------------------------------------------------------------------
// PendingRequest — tracking for in-flight async requests
// ---------------------------------------------------------------------------

/// Tracking info for an in-flight request whose dispatch returned a Promise.
struct PendingRequest {
    #[allow(dead_code)]
    id: u64,
    promise: v8::Global<v8::Promise>,
    reply: tokio::sync::oneshot::Sender<Result<RequestReply, String>>,
    cpu_accumulated: Duration,
    wall_start: Instant,
    cancel: CancellationToken,
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
    pub(crate) initialized: bool,
    pub(crate) modules: Vec<ModuleEntry>,
    pub(crate) state: SharedState,

    pending_requests: HashMap<u64, PendingRequest>,
    pending_ops: FuturesUnordered<Pin<Box<dyn Future<Output = OpResult>>>>,
    pending_timers: FuturesUnordered<Pin<Box<dyn Future<Output = TimerResult>>>>,

    request_rx: tokio::sync::mpsc::Receiver<IncomingRequest>,
    shutdown: CancellationToken,
}

// SAFETY: Runtime is only used on a single compio thread.
unsafe impl Send for Runtime {}

impl Runtime {
    /// Create a new `Runtime`.
    pub fn new(
        modules: Vec<ModuleEntry>,
        request_rx: tokio::sync::mpsc::Receiver<IncomingRequest>,
        shutdown: CancellationToken,
        env_vars: HashMap<String, String>,
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
            initialized: false,
            modules,
            state,
            pending_requests: HashMap::new(),
            pending_ops: FuturesUnordered::new(),
            pending_timers: FuturesUnordered::new(),
            request_rx,
            shutdown,
        }
    }

    // -----------------------------------------------------------------------
    // Initialization
    // -----------------------------------------------------------------------

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
        }

        self.initialized = true;
    }

    // -----------------------------------------------------------------------
    // Main event loop
    // -----------------------------------------------------------------------

    /// Run the event loop. Consumes events from all sources via `futures::select!`.
    pub async fn run(&mut self) {
        self.ensure_initialized();

        loop {
            self.collect_new_tasks();

            // Check V8 termination
            if self.isolate.is_execution_terminating() {
                self.isolate.cancel_terminate_execution();
                for (_id, req) in self.pending_requests.drain() {
                    let _ = req.reply.send(Err("CPU time limit exceeded".to_string()));
                }
            }

            // We need to handle the case where FuturesUnordered is empty
            // (select_next_some would never resolve). Use a helper that
            // returns a future resolving to None when empty.
            futures::select! {
                _ = self.shutdown.cancelled().fuse() => {
                    self.graceful_shutdown();
                    break;
                }
                req = self.request_rx.recv().fuse() => {
                    match req {
                        Some(req) => self.handle_incoming_request(req),
                        None => break, // channel closed
                    }
                }
                result = self.pending_ops.select_next_some() => {
                    self.handle_op_result(result);
                }
                result = self.pending_timers.select_next_some() => {
                    self.handle_timer(result);
                }
                complete => break, // all branches disabled
            }
        }
    }

    // -----------------------------------------------------------------------
    // collect_new_tasks
    // -----------------------------------------------------------------------

    /// Move newly spawned ops and timers from `RuntimeState` into the
    /// `FuturesUnordered` collections.
    fn collect_new_tasks(&mut self) {
        // Fast path: skip all work when nothing was spawned (common for sync requests).
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
            self.pending_ops.push(future);
        }

        {
            let mut s = self.state.borrow_mut();

            // Drain spawned ops
            for op_future in s.spawned_ops.drain(..) {
                self.pending_ops.push(op_future);
            }

            // Drain spawned timers -- use compio::time::sleep
            for timer in s.spawned_timers.drain(..) {
                let SpawnedTimer { id, delay, interval } = timer;
                self.pending_timers.push(Box::pin(async move {
                    compio::time::sleep(delay).await;
                    TimerResult { id, interval }
                }));
            }
        }

        // Fire zero-delay timers inline
        self.fire_ready_timers();
    }

    // -----------------------------------------------------------------------
    // fire_ready_timers
    // -----------------------------------------------------------------------

    fn fire_ready_timers(&mut self) {
        loop {
            let timer_id = {
                let mut s = self.state.borrow_mut();
                if s.ready_timers.is_empty() { None } else { Some(s.ready_timers.remove(0)) }
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

            let settled_results: Vec<(u64, PendingRequest, Result<String, String>)> =
                enter_v8!(self, |scope| {
                    appbase_v8_core::request::fire_timer_callback(scope, &self.state, timer_id);

                    let settled_ids: Vec<u64> = self
                        .pending_requests
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
                            let req = self.pending_requests.remove(&id)?;
                            let result =
                                appbase_v8_core::request::extract_promise_result(scope, &req.promise);
                            Some((id, req, result))
                        })
                        .collect()
                });

            let cpu_elapsed = start.elapsed();

            if let Some(rid) = owner_request_id {
                if let Some(req) = self.pending_requests.get_mut(&rid) {
                    req.cpu_accumulated += cpu_elapsed;
                }
            }

            self.state.borrow_mut().timer_owner.remove(&timer_id);

            for (id, req, settled) in settled_results {
                self.send_settled_reply(id, req, settled, cpu_elapsed);
            }

            self.cleanup_cancelled_requests();
            self.clear_executing_request();

            // Drain new spawned ops/timers from the callback
            {
                let mut s = self.state.borrow_mut();
                for op_future in s.spawned_ops.drain(..) {
                    self.pending_ops.push(op_future);
                }
                for timer in s.spawned_timers.drain(..) {
                    let SpawnedTimer { id, delay, interval } = timer;
                    self.pending_timers.push(Box::pin(async move {
                        compio::time::sleep(delay).await;
                        TimerResult { id, interval }
                    }));
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // handle_incoming_request
    // -----------------------------------------------------------------------

    fn handle_incoming_request(&mut self, req: IncomingRequest) {
        let IncomingRequest { id, kind, reply, cancel } = req;

        match kind {
            RequestKind::Rpc(body) => {
                self.handle_rpc_request(id, body, reply, cancel);
            }
            RequestKind::Http { .. } => {
                // HTTP handler not supported in compio runtime yet
                let _ = reply.send(Err("HTTP handler not supported in compio runtime".to_string()));
            }
        }
    }

    fn handle_rpc_request(
        &mut self,
        id: u64,
        body: String,
        reply: tokio::sync::oneshot::Sender<Result<RequestReply, String>>,
        cancel: CancellationToken,
    ) {
        {
            let mut s = self.state.borrow_mut();
            s.executing_request_id = Some(id);
            s.executing_request_cancel = Some(cancel.clone());
        }

        let wall_start = Instant::now();

        let dispatch_result = {
            let dispatch_fn = match &self.dispatch_fn {
                Some(f) => f,
                None => {
                    let _ = reply.send(Err("Isolate not initialized".to_string()));
                    self.clear_executing_request();
                    return;
                }
            };

            enter_v8!(self, |scope| {
                appbase_v8_core::request::dispatch_request(scope, &self.state, dispatch_fn, &body)
            })
        };

        // Single elapsed measurement used for both cpu_time and wall_time
        // (single-threaded: cpu time == wall time for V8 execution).
        let elapsed = wall_start.elapsed();

        match dispatch_result {
            DispatchResult::Sync(json) => {
                let logs = self.drain_request_logs(id);
                let _ = reply.send(Ok(RequestReply::Complete(RequestResult {
                    json,
                    cpu_time: elapsed,
                    wall_time: elapsed,
                    logs,
                })));
                self.clear_executing_request();
            }
            DispatchResult::Async(promise) => {
                self.pending_requests.insert(id, PendingRequest {
                    id,
                    promise,
                    reply,
                    cpu_accumulated: elapsed,
                    wall_start,
                    cancel: cancel.clone(),
                });

                self.clear_executing_request();
                self.check_settled_promises_v8();
            }
            DispatchResult::Error(msg) => {
                let _ = reply.send(Err(msg));
                self.clear_executing_request();
            }
        }
    }

    // -----------------------------------------------------------------------
    // handle_op_result
    // -----------------------------------------------------------------------

    fn handle_op_result(&mut self, result: OpResult) {
        match result {
            OpResult::Completed { op_id, value, request_id } => {
                if let Some(rid) = request_id {
                    let cancel = self.pending_requests.get(&rid).map(|r| r.cancel.clone());
                    let mut s = self.state.borrow_mut();
                    s.executing_request_id = Some(rid);
                    s.executing_request_cancel = cancel;
                }

                let start = Instant::now();

                let settled_results: Vec<(u64, PendingRequest, Result<String, String>)> =
                    enter_v8!(self, |scope| {
                        appbase_v8_core::request::resolve_op(scope, &self.state, op_id, &value);

                        let settled_ids: Vec<u64> = self
                            .pending_requests
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
                                let req = self.pending_requests.remove(&id)?;
                                let result =
                                    appbase_v8_core::request::extract_promise_result(scope, &req.promise);
                                Some((id, req, result))
                            })
                            .collect()
                    });

                let cpu_elapsed = start.elapsed();

                if let Some(rid) = request_id {
                    if let Some(req) = self.pending_requests.get_mut(&rid) {
                        req.cpu_accumulated += cpu_elapsed;
                    }
                }

                for (id, req, settled) in settled_results {
                    self.send_settled_reply(id, req, settled, cpu_elapsed);
                }

                self.cleanup_cancelled_requests();
                self.clear_executing_request();
            }
            OpResult::StreamChunk { stream_id, data, done } => {
                // Push into V8 ReadableStream (slow path, no forwarder in compio runtime)
                enter_v8!(self, |scope| {
                    appbase_v8_core::streams::push_stream_chunk(scope, &self.state, stream_id, &data, done);
                });
            }
            OpResult::Cancelled => {
                // No-op
            }
        }
    }

    // -----------------------------------------------------------------------
    // handle_timer
    // -----------------------------------------------------------------------

    fn handle_timer(&mut self, timer: TimerResult) {
        let TimerResult { id, interval } = timer;

        let owner_request_id = self.state.borrow().timer_owner.get(&id).copied();

        if let Some(rid) = owner_request_id {
            let cancel = self.pending_requests.get(&rid).map(|r| r.cancel.clone());
            let mut s = self.state.borrow_mut();
            s.executing_request_id = Some(rid);
            s.executing_request_cancel = cancel;
        }

        let start = Instant::now();

        let settled_results: Vec<(u64, PendingRequest, Result<String, String>)> =
            enter_v8!(self, |scope| {
                appbase_v8_core::request::fire_timer_callback(scope, &self.state, id);

                let settled_ids: Vec<u64> = self
                    .pending_requests
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
                        let req = self.pending_requests.remove(&id)?;
                        let result =
                            appbase_v8_core::request::extract_promise_result(scope, &req.promise);
                        Some((id, req, result))
                    })
                    .collect()
            });

        let cpu_elapsed = start.elapsed();

        if let Some(rid) = owner_request_id {
            if let Some(req) = self.pending_requests.get_mut(&rid) {
                req.cpu_accumulated += cpu_elapsed;
            }
        }

        // Re-arm interval timers
        if let Some(interval_dur) = interval {
            let timer_id = id;
            self.pending_timers.push(Box::pin(async move {
                compio::time::sleep(interval_dur).await;
                TimerResult { id: timer_id, interval: Some(interval_dur) }
            }));
        } else {
            self.state.borrow_mut().timer_owner.remove(&id);
        }

        for (id, req, settled) in settled_results {
            self.send_settled_reply(id, req, settled, cpu_elapsed);
        }

        self.cleanup_cancelled_requests();
        self.clear_executing_request();
    }

    // -----------------------------------------------------------------------
    // check_settled_promises_v8
    // -----------------------------------------------------------------------

    fn check_settled_promises_v8(&mut self) {
        if self.pending_requests.is_empty() {
            return;
        }

        let settled: Vec<u64> = enter_v8!(self, |scope| {
            self.pending_requests
                .iter()
                .filter_map(|(&id, req)| {
                    let promise = v8::Local::new(scope, &req.promise);
                    if promise.state() != v8::PromiseState::Pending {
                        Some(id)
                    } else {
                        None
                    }
                })
                .collect()
        });

        if settled.is_empty() {
            return;
        }

        for id in settled {
            if let Some(req) = self.pending_requests.remove(&id) {
                let settled_result = enter_v8!(self, |scope| {
                    appbase_v8_core::request::extract_promise_result(scope, &req.promise)
                });
                self.send_settled_reply(id, req, settled_result, Duration::ZERO);
            }
        }
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn send_settled_reply(
        &mut self,
        id: u64,
        req: PendingRequest,
        settled: Result<String, String>,
        cpu_elapsed: Duration,
    ) {
        let cpu_time = req.cpu_accumulated + cpu_elapsed;
        match settled {
            Ok(json) => {
                let logs = self.drain_request_logs(id);
                let _ = req.reply.send(Ok(RequestReply::Complete(RequestResult {
                    json,
                    cpu_time,
                    wall_time: req.wall_start.elapsed(),
                    logs,
                })));
            }
            Err(msg) => {
                let _ = req.reply.send(Err(msg));
            }
        }
    }

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
                let _ = req.reply.send(Err("Request timed out".into()));
            }
        }
    }

    fn graceful_shutdown(&mut self) {
        for (_, req) in &self.pending_requests {
            req.cancel.cancel();
        }
        self.request_rx.close();
        for (_id, req) in self.pending_requests.drain() {
            let _ = req.reply.send(Err("Isolate shutting down".to_string()));
        }
    }
}
