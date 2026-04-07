//! Runtime v3 — tokio::select! event loop with V8 isolate.
//!
//! Replaces `ConcurrentIsolate` from `concurrent.rs`. The key architectural
//! change: instead of a hand-rolled tick() loop that drains events from an
//! `mpsc` channel, this uses `tokio::select!` where each event source is a
//! separate branch.
//!
//! The `Runtime` struct owns the V8 isolate. The `run()` method is async,
//! driven by the tokio runtime on a `LocalSet` (V8 is `!Send`).

#![allow(unsafe_code)]

use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::time::{Duration, Instant};

use futures::stream::FuturesUnordered;
use futures::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::init::{init_v8, load_polyfills_and_modules, thread_cpu_time, RequestResult};
use crate::modules::ModuleEntry;
use crate::state::{
    DispatchResult, IncomingRequest, OpResult, RuntimeState, SharedState, SpawnedTimer,
    TimerResult,
};

// ---------------------------------------------------------------------------
// PendingRequest — tracking for in-flight async requests
// ---------------------------------------------------------------------------

/// Tracking info for an in-flight request whose dispatch returned a Promise.
struct PendingRequest {
    #[allow(dead_code)]
    id: u64,
    promise: v8::Global<v8::Promise>,
    reply: tokio::sync::oneshot::Sender<Result<RequestResult, String>>,
    cpu_accumulated: Duration,
    wall_start: Instant,
    cancel: CancellationToken,
}

// ---------------------------------------------------------------------------
// enter_v8! macro — create PinScope + ContextScope, run a block, checkpoint
// ---------------------------------------------------------------------------

/// Enter V8 with a pinned scope, execute a block, then run a microtask checkpoint.
///
/// Usage:
/// ```ignore
/// enter_v8!(self, |scope| {
///     // scope is &mut v8::PinScope (via ContextScope deref)
///     ...
/// });
/// ```
///
/// The macro uses `v8::scope!` to create a `PinScope`, opens a `ContextScope`
/// from the stored context, runs the closure, then calls `perform_microtask_checkpoint`.
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

/// A V8 isolate driven by a `tokio::select!` event loop.
///
/// Owns the isolate and all associated state. Must be run on a `LocalSet`
/// because V8 types are `!Send`.
pub struct Runtime {
    isolate: v8::OwnedIsolate,
    context: v8::Global<v8::Context>,
    dispatch_fn: Option<v8::Global<v8::Function>>,
    initialized: bool,
    modules: Vec<ModuleEntry>,
    state: SharedState,

    pending_requests: HashMap<u64, PendingRequest>,
    pending_ops: FuturesUnordered<Pin<Box<dyn Future<Output = OpResult>>>>,
    pending_timers: FuturesUnordered<Pin<Box<dyn Future<Output = TimerResult>>>>,

    /// Senders for stream forwarders — when a stream chunk arrives and a
    /// forwarder exists, we send directly without entering V8.
    stream_forwarders: HashMap<u32, tokio::sync::mpsc::Sender<Vec<u8>>>,

    request_rx: tokio::sync::mpsc::Receiver<IncomingRequest>,
    shutdown: CancellationToken,

    #[cfg(target_os = "linux")]
    cpu_timer: Option<crate::cpu_timer::CpuTimer>,
    #[cfg(target_os = "linux")]
    cpu_timer_active: bool,

    cpu_limit: Option<Duration>,
}

// SAFETY: Runtime is only used on a single tokio LocalSet task.
// The mpsc channels handle cross-thread communication.
unsafe impl Send for Runtime {}

impl Runtime {
    /// Create a new `Runtime`.
    ///
    /// - `modules` — ES modules to load (the first is the entry point).
    /// - `request_rx` — channel that delivers `IncomingRequest`s.
    /// - `shutdown` — token cancelled to trigger graceful shutdown.
    /// - `cpu_limit` — optional per-request CPU time limit (Linux only).
    /// - `env_vars` — environment variables exposed to JS via `env.get()`.
    /// - `server_handle` — optional handle to the server's multi-threaded tokio
    ///   runtime. When set, fetch I/O is spawned on this handle for parallel
    ///   network I/O instead of running on the isolate's single-threaded runtime.
    pub fn new(
        modules: Vec<ModuleEntry>,
        request_rx: tokio::sync::mpsc::Receiver<IncomingRequest>,
        shutdown: CancellationToken,
        cpu_limit: Option<Duration>,
        env_vars: HashMap<String, String>,
        server_handle: Option<tokio::runtime::Handle>,
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

        // Create RuntimeState and set as isolate slot
        let rt_state = RuntimeState::new(env_vars, server_handle);
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
            initialized: false,
            modules,
            state,
            pending_requests: HashMap::new(),
            pending_ops: FuturesUnordered::new(),
            pending_timers: FuturesUnordered::new(),
            stream_forwarders: HashMap::new(),
            request_rx,
            shutdown,
            #[cfg(target_os = "linux")]
            cpu_timer: None,
            #[cfg(target_os = "linux")]
            cpu_timer_active: false,
            cpu_limit,
        }
    }

    // -----------------------------------------------------------------------
    // Initialization
    // -----------------------------------------------------------------------

    /// Load polyfills, ES modules, and compile the dispatch function (once).
    fn ensure_initialized(&mut self) {
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

        // Create POSIX CPU timer (Linux only, must be on the isolate thread)
        #[cfg(target_os = "linux")]
        if self.cpu_limit.is_some() {
            let system = crate::cpu_timer::CpuTimerSystem::get_or_init();
            let app_id = 0u64; // single-app mode for now
            let v8_handle = self.isolate.thread_safe_handle();
            system.register(app_id, v8_handle);
            match crate::cpu_timer::CpuTimer::new(app_id) {
                Ok(timer) => self.cpu_timer = Some(timer),
                Err(e) => eprintln!("[cpu-timer] Failed: {e}"),
            }
        }
    }

    // -----------------------------------------------------------------------
    // CPU timer helpers
    // -----------------------------------------------------------------------

    /// Arm the CPU timer before entering V8.
    fn arm_cpu_timer(&mut self) {
        #[cfg(target_os = "linux")]
        if !self.cpu_timer_active {
            if let (Some(timer), Some(limit)) = (&self.cpu_timer, self.cpu_limit) {
                timer.arm(limit);
                self.cpu_timer_active = true;
            }
        }
    }

    /// Disarm the CPU timer after V8 returns.
    fn disarm_cpu_timer(&mut self) {
        #[cfg(target_os = "linux")]
        if self.cpu_timer_active {
            if let Some(timer) = &self.cpu_timer {
                timer.disarm();
            }
            self.cpu_timer_active = false;
        }
    }

    // -----------------------------------------------------------------------
    // Main event loop
    // -----------------------------------------------------------------------

    /// Run the event loop. Consumes events from all sources via `tokio::select!`.
    ///
    /// Must be called from a `tokio::task::LocalSet` because V8 types are `!Send`.
    pub async fn run(&mut self) {
        self.ensure_initialized();

        loop {
            self.collect_new_tasks();

            // Check CPU termination (V8 was killed by the CPU timer signal handler)
            if self.isolate.is_execution_terminating() {
                self.isolate.cancel_terminate_execution();
                self.disarm_cpu_timer();
                // Drain pending requests with error
                for (_id, req) in self.pending_requests.drain() {
                    let _ = req.reply.send(Err("CPU time limit exceeded".to_string()));
                }
                // Continue the loop — new requests can still arrive
            }

            tokio::select! {
                _ = self.shutdown.cancelled() => {
                    self.graceful_shutdown();
                    break;
                }
                Some(req) = self.request_rx.recv() => {
                    self.handle_incoming_request(req);
                }
                Some(result) = self.pending_ops.next() => {
                    self.handle_op_result(result);
                }
                Some(result) = self.pending_timers.next() => {
                    self.handle_timer(result);
                }
                // All branches disabled (no pending work, channel closed) → exit
                else => break,
            }
        }
    }

    // -----------------------------------------------------------------------
    // collect_new_tasks — drain spawned ops/timers from RuntimeState
    // -----------------------------------------------------------------------

    /// Move newly spawned ops and timers from `RuntimeState` into the
    /// `FuturesUnordered` collections so `tokio::select!` can poll them.
    fn collect_new_tasks(&mut self) {
        {
            let mut s = self.state.borrow_mut();

            // Drain spawned ops → pending_ops
            for op_future in s.spawned_ops.drain(..) {
                self.pending_ops.push(op_future);
            }

            // Drain spawned timers → pending_timers (create tokio::time::sleep futures)
            for timer in s.spawned_timers.drain(..) {
                let SpawnedTimer { id, delay, interval } = timer;
                self.pending_timers.push(Box::pin(async move {
                    tokio::time::sleep(delay).await;
                    TimerResult { id, interval }
                }));
            }
        }

        // Fire zero-delay timers inline (avoids tokio scheduling overhead).
        // Loops because a timer callback may enqueue more ready_timers via
        // nested setTimeout(0).
        self.fire_ready_timers();
    }

    // -----------------------------------------------------------------------
    // fire_ready_timers — inline execution of zero-delay timers
    // -----------------------------------------------------------------------

    /// Drain `ready_timers` and fire each callback inline, without going
    /// through tokio::time::sleep. Loops until no more ready timers remain
    /// (nested setTimeout(0) calls are batched in the same pass).
    fn fire_ready_timers(&mut self) {
        loop {
            let timer_id = {
                let mut s = self.state.borrow_mut();
                s.ready_timers.pop()
            };
            let Some(timer_id) = timer_id else { break };

            // Look up the owning request so we can set executing context.
            let owner_request_id = self.state.borrow().timer_owner.get(&timer_id).copied();

            if let Some(rid) = owner_request_id {
                let cancel = self.pending_requests.get(&rid).map(|r| r.cancel.clone());
                let mut s = self.state.borrow_mut();
                s.executing_request_id = Some(rid);
                s.executing_request_cancel = cancel;
            }

            let cpu_start = if self.cpu_limit.is_some() { thread_cpu_time() } else { Duration::ZERO };
            self.arm_cpu_timer();

            // ONE enter_v8 for fire_timer + check settled + extract results
            let settled_results: Vec<(u64, PendingRequest, Result<String, String>)> =
                enter_v8!(self, |scope| {
                    crate::request::fire_timer_callback(scope, &self.state, timer_id);

                    // Check settled promises IN THE SAME SCOPE
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

                    // Extract results IN THE SAME SCOPE
                    settled_ids
                        .into_iter()
                        .filter_map(|id| {
                            let req = self.pending_requests.remove(&id)?;
                            let result =
                                crate::request::extract_promise_result(scope, &req.promise);
                            Some((id, req, result))
                        })
                        .collect()
                });

            let cpu_elapsed = if self.cpu_limit.is_some() {
                thread_cpu_time().saturating_sub(cpu_start)
            } else {
                Duration::ZERO
            };
            self.disarm_cpu_timer();

            // Accumulate CPU time on owning request.
            if let Some(rid) = owner_request_id {
                if let Some(req) = self.pending_requests.get_mut(&rid) {
                    req.cpu_accumulated += cpu_elapsed;
                }
            }

            // One-shot timer: remove from timer_owner.
            // (setInterval with delay 0 goes through spawned_timers, not ready_timers,
            // so we won't see interval timers here.)
            self.state.borrow_mut().timer_owner.remove(&timer_id);

            // Send replies OUTSIDE V8 scope (drain_request_logs borrows state)
            for (id, req, result) in settled_results {
                let logs = self.drain_request_logs(id);
                let cpu_time = req.cpu_accumulated + cpu_elapsed;
                let _ = req.reply.send(result.map(|json| RequestResult {
                    json,
                    cpu_time,
                    wall_time: req.wall_start.elapsed(),
                    logs,
                }));
            }

            self.clear_executing_request();

            // Drain any new spawned ops/timers that the callback may have created
            // (but NOT recursing into fire_ready_timers — we handle ready_timers
            // via the outer loop).
            {
                let mut s = self.state.borrow_mut();
                for op_future in s.spawned_ops.drain(..) {
                    self.pending_ops.push(op_future);
                }
                for timer in s.spawned_timers.drain(..) {
                    let SpawnedTimer { id, delay, interval } = timer;
                    self.pending_timers.push(Box::pin(async move {
                        tokio::time::sleep(delay).await;
                        TimerResult { id, interval }
                    }));
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // handle_incoming_request
    // -----------------------------------------------------------------------

    /// Dispatch an incoming HTTP/RPC request into the JS runtime.
    fn handle_incoming_request(&mut self, req: IncomingRequest) {
        let IncomingRequest { id, body, reply, cancel } = req;

        // Set executing_request_id on state so ops/timers know which request owns them
        {
            let mut s = self.state.borrow_mut();
            s.executing_request_id = Some(id);
            s.executing_request_cancel = Some(cancel.clone());
        }

        let cpu_start = if self.cpu_limit.is_some() { thread_cpu_time() } else { Duration::ZERO };
        let wall_start = Instant::now();

        self.arm_cpu_timer();

        let dispatch_result = {
            let dispatch_fn = match &self.dispatch_fn {
                Some(f) => f,
                None => {
                    let _ = reply.send(Err("Isolate not initialized".to_string()));
                    self.clear_executing_request();
                    self.disarm_cpu_timer();
                    return;
                }
            };

            enter_v8!(self, |scope| {
                crate::request::dispatch_request(scope, &self.state, dispatch_fn, &body)
            })
        };

        let cpu_elapsed = if self.cpu_limit.is_some() {
            thread_cpu_time().saturating_sub(cpu_start)
        } else {
            Duration::ZERO
        };
        self.disarm_cpu_timer();

        match dispatch_result {
            DispatchResult::Sync(json) => {
                let logs = self.drain_request_logs(id);
                let _ = reply.send(Ok(RequestResult {
                    json,
                    cpu_time: cpu_elapsed,
                    wall_time: wall_start.elapsed(),
                    logs,
                }));
                self.clear_executing_request();
                // No check_settled_promises_v8 — sync dispatch adds no pending promise
            }
            DispatchResult::Async(promise) => {
                self.pending_requests.insert(id, PendingRequest {
                    id,
                    promise,
                    reply,
                    cpu_accumulated: cpu_elapsed,
                    wall_start,
                    cancel,
                });
                self.clear_executing_request();
                self.check_settled_promises_v8();
            }
            DispatchResult::Error(msg) => {
                let _ = reply.send(Err(msg));
                self.clear_executing_request();
                // No check_settled_promises_v8 — error dispatch adds no pending promise
            }
        }
    }

    // -----------------------------------------------------------------------
    // handle_op_result
    // -----------------------------------------------------------------------

    /// Process a completed async op (fetch, kv, etc.).
    fn handle_op_result(&mut self, result: OpResult) {
        match result {
            OpResult::Completed { op_id, value, request_id } => {
                // Set executing request context
                if let Some(rid) = request_id {
                    let cancel = self.pending_requests.get(&rid).map(|r| r.cancel.clone());
                    let mut s = self.state.borrow_mut();
                    s.executing_request_id = Some(rid);
                    s.executing_request_cancel = cancel;
                }

                let cpu_start = if self.cpu_limit.is_some() { thread_cpu_time() } else { Duration::ZERO };
                self.arm_cpu_timer();

                // ONE enter_v8 for resolve + check settled + extract results
                let settled_results: Vec<(u64, PendingRequest, Result<String, String>)> =
                    enter_v8!(self, |scope| {
                        // Resolve the op promise
                        crate::request::resolve_op(scope, &self.state, op_id, &value);

                        // Check settled promises IN THE SAME SCOPE
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

                        // Extract results IN THE SAME SCOPE
                        settled_ids
                            .into_iter()
                            .filter_map(|id| {
                                let req = self.pending_requests.remove(&id)?;
                                let result =
                                    crate::request::extract_promise_result(scope, &req.promise);
                                Some((id, req, result))
                            })
                            .collect()
                    });

                let cpu_elapsed = if self.cpu_limit.is_some() {
                    thread_cpu_time().saturating_sub(cpu_start)
                } else {
                    Duration::ZERO
                };
                self.disarm_cpu_timer();

                // Accumulate CPU time on the owning PendingRequest (if it wasn't settled)
                if let Some(rid) = request_id {
                    if let Some(req) = self.pending_requests.get_mut(&rid) {
                        req.cpu_accumulated += cpu_elapsed;
                    }
                }

                // Send replies OUTSIDE V8 scope (drain_request_logs borrows state)
                for (id, req, result) in settled_results {
                    let logs = self.drain_request_logs(id);
                    let cpu_time = req.cpu_accumulated + cpu_elapsed;
                    let _ = req.reply.send(result.map(|json| RequestResult {
                        json,
                        cpu_time,
                        wall_time: req.wall_start.elapsed(),
                        logs,
                    }));
                }

                self.clear_executing_request();
            }
            OpResult::StreamChunk { stream_id, data, done } => {
                // Fast path: if there's a stream forwarder, send directly (no V8 entry)
                if let Some(sender) = self.stream_forwarders.get(&stream_id) {
                    if !data.is_empty() {
                        let _ = sender.try_send(data);
                    }
                    if done {
                        self.stream_forwarders.remove(&stream_id);
                    }
                } else {
                    // Slow path: push into V8 ReadableStream.
                    enter_v8!(self, |scope| {
                        crate::streams::push_stream_chunk(scope, &self.state, stream_id, &data, done);
                    });
                }
            }
            OpResult::Cancelled => {
                // No-op — the op was cancelled, nothing to resolve.
            }
        }
    }

    // -----------------------------------------------------------------------
    // handle_timer
    // -----------------------------------------------------------------------

    /// Process a fired timer.
    fn handle_timer(&mut self, timer: TimerResult) {
        let TimerResult { id, interval } = timer;

        // Find the owning request from timer_owner
        let owner_request_id = self.state.borrow().timer_owner.get(&id).copied();

        if let Some(rid) = owner_request_id {
            let cancel = self.pending_requests.get(&rid).map(|r| r.cancel.clone());
            let mut s = self.state.borrow_mut();
            s.executing_request_id = Some(rid);
            s.executing_request_cancel = cancel;
        }

        let cpu_start = if self.cpu_limit.is_some() { thread_cpu_time() } else { Duration::ZERO };
        self.arm_cpu_timer();

        // ONE enter_v8 for fire_timer + check settled + extract results
        let settled_results: Vec<(u64, PendingRequest, Result<String, String>)> =
            enter_v8!(self, |scope| {
                crate::request::fire_timer_callback(scope, &self.state, id);

                // Check settled promises IN THE SAME SCOPE
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

                // Extract results IN THE SAME SCOPE
                settled_ids
                    .into_iter()
                    .filter_map(|id| {
                        let req = self.pending_requests.remove(&id)?;
                        let result =
                            crate::request::extract_promise_result(scope, &req.promise);
                        Some((id, req, result))
                    })
                    .collect()
            });

        let cpu_elapsed = if self.cpu_limit.is_some() {
            thread_cpu_time().saturating_sub(cpu_start)
        } else {
            Duration::ZERO
        };
        self.disarm_cpu_timer();

        // Accumulate CPU time on owning request (if it wasn't settled)
        if let Some(rid) = owner_request_id {
            if let Some(req) = self.pending_requests.get_mut(&rid) {
                req.cpu_accumulated += cpu_elapsed;
            }
        }

        // Re-arm interval timers
        if let Some(interval_dur) = interval {
            let timer_id = id;
            self.pending_timers.push(Box::pin(async move {
                tokio::time::sleep(interval_dur).await;
                TimerResult { id: timer_id, interval: Some(interval_dur) }
            }));
        } else {
            // One-shot timer: remove from timer_owner
            self.state.borrow_mut().timer_owner.remove(&id);
        }

        // Send replies OUTSIDE V8 scope (drain_request_logs borrows state)
        for (id, req, result) in settled_results {
            let logs = self.drain_request_logs(id);
            let cpu_time = req.cpu_accumulated + cpu_elapsed;
            let _ = req.reply.send(result.map(|json| RequestResult {
                json,
                cpu_time,
                wall_time: req.wall_start.elapsed(),
                logs,
            }));
        }

        self.clear_executing_request();
    }

    // -----------------------------------------------------------------------
    // check_settled_promises_v8
    // -----------------------------------------------------------------------

    /// Enter V8 and check if any pending promises have settled.
    /// If so, extract results and send replies.
    fn check_settled_promises_v8(&mut self) {
        if self.pending_requests.is_empty() {
            return;
        }

        // Collect settled request IDs
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

        // Extract results and send replies
        for id in settled {
            if let Some(req) = self.pending_requests.remove(&id) {
                let result = enter_v8!(self, |scope| {
                    crate::request::extract_promise_result(scope, &req.promise)
                });

                let logs = self.drain_request_logs(id);
                match result {
                    Ok(json) => {
                        let _ = req.reply.send(Ok(RequestResult {
                            json,
                            cpu_time: req.cpu_accumulated,
                            wall_time: req.wall_start.elapsed(),
                            logs,
                        }));
                    }
                    Err(msg) => {
                        let _ = req.reply.send(Err(msg));
                    }
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    /// Drain per-request log lines for the given request ID.
    fn drain_request_logs(&mut self, request_id: u64) -> Vec<String> {
        self.state
            .borrow_mut()
            .per_request_logs
            .remove(&request_id)
            .unwrap_or_default()
    }

    /// Clear the executing request context from RuntimeState.
    fn clear_executing_request(&self) {
        let mut s = self.state.borrow_mut();
        s.executing_request_id = None;
        s.executing_request_cancel = None;
    }

    /// Graceful shutdown: cancel all pending requests, close the channel.
    fn graceful_shutdown(&mut self) {
        // Cancel all pending requests' cancel tokens
        for (_, req) in &self.pending_requests {
            req.cancel.cancel();
        }

        // Close the request channel by dropping via recv (it will return None)
        self.request_rx.close();

        // Error remaining pending requests
        for (_id, req) in self.pending_requests.drain() {
            let _ = req.reply.send(Err("Isolate shutting down".to_string()));
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init::RequestResult;
    use crate::modules::ModuleEntry;

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    /// Build the test JS module with ping, add, delayed, and chain exports.
    fn test_modules() -> Vec<ModuleEntry> {
        vec![ModuleEntry {
            specifier: "index.js".into(),
            source: r#"
export function ping() { return "pong"; }
export function add(a, b) { return a + b; }
export function delayed(ms) {
    return new Promise(function(resolve) {
        setTimeout(function() { resolve("done_" + ms); }, ms || 10);
    });
}
export function chain() {
    return new Promise(function(resolve) {
        setTimeout(function() { resolve(1); }, 5);
    }).then(function(v) { return v + 10; }).then(function(v) { return v * 2; });
}
"#
            .into(),
        }]
    }

    /// Spawn a Runtime on a dedicated OS thread with its own single-threaded
    /// Tokio runtime.  Returns (request sender, shutdown token, thread handle).
    fn spawn_runtime(
        modules: Vec<ModuleEntry>,
    ) -> (
        tokio::sync::mpsc::Sender<IncomingRequest>,
        CancellationToken,
        std::thread::JoinHandle<()>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::channel(256);
        let shutdown = CancellationToken::new();
        let shutdown_inner = shutdown.clone();

        let handle = std::thread::Builder::new()
            .name("v8-test".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                rt.block_on(async {
                    let mut runtime =
                        Runtime::new(modules, rx, shutdown_inner, None, HashMap::new(), None);
                    runtime.run().await;
                });
            })
            .unwrap();

        (tx, shutdown, handle)
    }

    /// Build a minimal JSON-RPC 2.0 request string.
    fn rpc(method: &str, params: &str) -> String {
        format!(
            r#"{{"jsonrpc":"2.0","method":"{method}","params":{params},"id":1}}"#
        )
    }

    // -----------------------------------------------------------------------
    // 1. Sync request: ping → "pong"
    // -----------------------------------------------------------------------

    #[test]
    fn runtime_sync_request() {
        let (tx, shutdown, handle) = spawn_runtime(test_modules());

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        tx.blocking_send(IncomingRequest {
            id: 1,
            body: rpc("ping", "[]"),
            reply: reply_tx,
            cancel: CancellationToken::new(),
        })
        .unwrap();

        let result: RequestResult = reply_rx.blocking_recv().unwrap().unwrap();
        assert!(
            result.json.contains("pong"),
            "expected 'pong' in: {}",
            result.json
        );

        shutdown.cancel();
        handle.join().unwrap();
    }

    // -----------------------------------------------------------------------
    // 2. Async request: delayed(5) → wall_time >= 5ms
    // -----------------------------------------------------------------------

    #[test]
    fn runtime_async_request() {
        let (tx, shutdown, handle) = spawn_runtime(test_modules());

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        tx.blocking_send(IncomingRequest {
            id: 2,
            body: rpc("delayed", "[5]"),
            reply: reply_tx,
            cancel: CancellationToken::new(),
        })
        .unwrap();

        let result: RequestResult = reply_rx.blocking_recv().unwrap().unwrap();
        assert!(
            result.json.contains("done_5"),
            "expected 'done_5' in: {}",
            result.json
        );
        assert!(
            result.wall_time >= Duration::from_millis(5),
            "wall_time {:?} should be >= 5ms",
            result.wall_time
        );

        shutdown.cancel();
        handle.join().unwrap();
    }

    // -----------------------------------------------------------------------
    // 3. Concurrency: 3 × delayed(20) should complete well under 500ms total
    // -----------------------------------------------------------------------

    #[test]
    fn runtime_concurrent_overlap() {
        let (tx, shutdown, handle) = spawn_runtime(test_modules());

        let wall_start = Instant::now();

        // Send all three requests before waiting for any reply.
        let mut receivers = Vec::new();
        for id in 1u64..=3 {
            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            tx.blocking_send(IncomingRequest {
                id,
                body: rpc("delayed", "[20]"),
                reply: reply_tx,
                cancel: CancellationToken::new(),
            })
            .unwrap();
            receivers.push(reply_rx);
        }

        // Collect all replies.
        for rx in receivers {
            let result: RequestResult = rx.blocking_recv().unwrap().unwrap();
            assert!(
                result.json.contains("done_20"),
                "expected 'done_20' in: {}",
                result.json
            );
        }

        let total = wall_start.elapsed();
        assert!(
            total < Duration::from_millis(500),
            "total wall {:?} should be < 500ms (concurrency expected)",
            total
        );

        shutdown.cancel();
        handle.join().unwrap();
    }

    // -----------------------------------------------------------------------
    // 4. Mixed sync + async + sync interleaved
    // -----------------------------------------------------------------------

    #[test]
    fn runtime_mixed_sync_async() {
        let (tx, shutdown, handle) = spawn_runtime(test_modules());

        // Send sync ping first.
        let (r1_tx, r1_rx) = tokio::sync::oneshot::channel();
        tx.blocking_send(IncomingRequest {
            id: 10,
            body: rpc("ping", "[]"),
            reply: r1_tx,
            cancel: CancellationToken::new(),
        })
        .unwrap();

        // Immediately queue an async delayed(10).
        let (r2_tx, r2_rx) = tokio::sync::oneshot::channel();
        tx.blocking_send(IncomingRequest {
            id: 11,
            body: rpc("delayed", "[10]"),
            reply: r2_tx,
            cancel: CancellationToken::new(),
        })
        .unwrap();

        // And another sync add(3,4) behind it.
        let (r3_tx, r3_rx) = tokio::sync::oneshot::channel();
        tx.blocking_send(IncomingRequest {
            id: 12,
            body: rpc("add", "[3,4]"),
            reply: r3_tx,
            cancel: CancellationToken::new(),
        })
        .unwrap();

        let r1 = r1_rx.blocking_recv().unwrap().unwrap();
        let r2 = r2_rx.blocking_recv().unwrap().unwrap();
        let r3 = r3_rx.blocking_recv().unwrap().unwrap();

        assert!(r1.json.contains("pong"), "r1: {}", r1.json);
        assert!(r2.json.contains("done_10"), "r2: {}", r2.json);
        assert!(r3.json.contains("7"), "r3: {}", r3.json);

        shutdown.cancel();
        handle.join().unwrap();
    }

    // -----------------------------------------------------------------------
    // 5. Promise chain: setTimeout(1ms) → +10 → ×2 → 22
    // -----------------------------------------------------------------------

    #[test]
    fn runtime_promise_chain() {
        let (tx, shutdown, handle) = spawn_runtime(test_modules());

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        tx.blocking_send(IncomingRequest {
            id: 20,
            body: rpc("chain", "[]"),
            reply: reply_tx,
            cancel: CancellationToken::new(),
        })
        .unwrap();

        let result: RequestResult = reply_rx.blocking_recv().unwrap().unwrap();
        assert!(
            result.json.contains("22"),
            "expected result 22 in: {}",
            result.json
        );

        shutdown.cancel();
        handle.join().unwrap();
    }

    // -----------------------------------------------------------------------
    // 6. Shutdown: long-running request gets error on cancel
    // -----------------------------------------------------------------------

    #[test]
    fn runtime_shutdown() {
        let (tx, shutdown, handle) = spawn_runtime(test_modules());

        // delayed(5000) — will never complete before we shut down.
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        tx.blocking_send(IncomingRequest {
            id: 30,
            body: rpc("delayed", "[5000]"),
            reply: reply_tx,
            cancel: CancellationToken::new(),
        })
        .unwrap();

        // Give the runtime a moment to receive and start the request.
        std::thread::sleep(Duration::from_millis(50));

        // Trigger graceful shutdown.
        shutdown.cancel();

        // The pending request must receive an error.
        let outcome = reply_rx.blocking_recv().unwrap();
        assert!(
            outcome.is_err(),
            "expected Err on shutdown, got Ok({:?})",
            outcome.ok()
        );

        handle.join().unwrap();
    }
}

