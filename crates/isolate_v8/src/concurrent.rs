//! Concurrent V8 isolate — serial JS, concurrent I/O execution model.
//!
//! Multiple requests in-flight with I/O overlapping, but JS execution
//! is serial per-request for clean kill safety. The microtask queue
//! contains entries from exactly 1 request at any given moment.
//!
//! Event loop phases:
//!   1. ACCEPT — drain new requests + buffer completed ops
//!   2. TIMERS — fire ready timer callbacks (one at a time, checkpoint after each)
//!   3. RESOLVE — resolve completed ops ONE AT A TIME, checkpoint after each
//!   4. CHECK — see if any pending promises settled
//!   5. WAIT — recv_timeout on event channel (zero CPU while idle)

#![allow(unsafe_code)]

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::time::{Duration, Instant};

use crate::event_loop::{EventLoopState, SharedState};
use crate::globals::setup_globals;
use crate::runtime::{init_v8, thread_cpu_time, RequestResult, DISPATCH_JS, FETCH_JS};
use crate::timers::fire_ready_timers;

// ---------------------------------------------------------------------------
// Event types for the concurrent model
// ---------------------------------------------------------------------------

/// Events flowing into the worker thread's event loop.
pub enum Event {
    /// A new RPC request from the HTTP layer.
    NewRequest {
        id: u64,
        body: String,
        reply: tokio::sync::oneshot::Sender<Result<RequestResult, String>>,
    },
    /// An async op completed (e.g., fetch, DB query, tokio timer).
    OpCompleted {
        op_id: u32,
        value: String,
    },
    /// Graceful shutdown signal.
    Shutdown,
}

// All constituent types (u64, String, tokio::sync::oneshot::Sender) are Send,
// so Event auto-derives Send — no unsafe impl needed.

/// Tracking info for an in-flight request whose dispatch returned a Promise.
struct PendingRequest {
    #[allow(dead_code)]
    id: u64,
    reply: Option<tokio::sync::oneshot::Sender<Result<RequestResult, String>>>,
    promise: v8::Global<v8::Promise>,
    cpu_accumulated: Duration,
    wall_start: Instant,
}

// ---------------------------------------------------------------------------
// LoopState — all mutable state NOT including the V8 isolate itself.
// ---------------------------------------------------------------------------

struct LoopState {
    context: v8::Global<v8::Context>,
    dispatch_fn: Option<v8::Global<v8::Function>>,
    el_state: SharedState,

    pending_requests: HashMap<u64, PendingRequest>,
    completed_ops: Vec<(u32, String)>,

    event_rx: std::sync::mpsc::Receiver<Event>,
    #[allow(dead_code)]
    event_tx: std::sync::mpsc::Sender<Event>,

    buffered_events: VecDeque<Event>,

    server_js: String,
    initialized: bool,
    shutdown: bool,

    cpu_limit: Option<Duration>,
}

impl LoopState {
    /// Phase 1: ACCEPT — drain buffered events + channel.
    /// Returns true if any async work was queued.
    fn accept_requests(&mut self, scope: &mut v8::PinScope) -> bool {
        let had_pending_before = !self.pending_requests.is_empty();

        while let Some(event) = self.buffered_events.pop_front() {
            self.handle_event(scope, event);
        }

        loop {
            match self.event_rx.try_recv() {
                Ok(event) => self.handle_event(scope, event),
                Err(_) => break,
            }
        }

        !self.pending_requests.is_empty()
            || !self.completed_ops.is_empty()
            || had_pending_before
    }

    fn handle_event(&mut self, scope: &mut v8::PinScope, event: Event) {
        match event {
            Event::NewRequest { id, body, reply } => {
                self.dispatch_request(scope, id, body, reply);
            }
            Event::OpCompleted { op_id, value } => {
                self.completed_ops.push((op_id, value));
            }
            Event::Shutdown => {
                self.shutdown = true;
                for (_id, req) in self.pending_requests.drain() {
                    if let Some(reply) = req.reply {
                        let _ = reply.send(Err("Isolate shutting down".to_string()));
                    }
                }
            }
        }
    }

    fn dispatch_request(
        &mut self,
        scope: &mut v8::PinScope,
        id: u64,
        body: String,
        reply: tokio::sync::oneshot::Sender<Result<RequestResult, String>>,
    ) {
        let cpu_start = thread_cpu_time();
        let wall_start = Instant::now();

        let dispatch_fn = match &self.dispatch_fn {
            Some(f) => f,
            None => {
                let _ = reply.send(Err("Isolate not initialized".to_string()));
                return;
            }
        };

        let func = v8::Local::new(scope, dispatch_fn);
        let arg = match v8::String::new(scope, &body) {
            Some(s) => s,
            None => {
                let _ = reply.send(Err("Failed to create arg string".to_string()));
                return;
            }
        };
        let undefined = v8::undefined(scope).into();
        let result = func.call(scope, undefined, &[arg.into()]);
        let cpu_elapsed = thread_cpu_time().saturating_sub(cpu_start);

        match result {
            Some(val) if val.is_promise() => {
                let promise = v8::Local::<v8::Promise>::try_from(val).unwrap();
                match promise.state() {
                    v8::PromiseState::Fulfilled => {
                        let json = promise
                            .result(scope)
                            .to_string(scope)
                            .unwrap()
                            .to_rust_string_lossy(scope);
                        let _ = reply.send(Ok(RequestResult {
                            json,
                            cpu_time: cpu_elapsed,
                            wall_time: wall_start.elapsed(),
                        }));
                    }
                    v8::PromiseState::Rejected => {
                        let msg = promise
                            .result(scope)
                            .to_string(scope)
                            .unwrap()
                            .to_rust_string_lossy(scope);
                        let _ = reply.send(Err(msg));
                    }
                    v8::PromiseState::Pending => {
                        let global_promise = v8::Global::new(scope, promise);
                        self.pending_requests.insert(
                            id,
                            PendingRequest {
                                id,
                                reply: Some(reply),
                                promise: global_promise,
                                cpu_accumulated: cpu_elapsed,
                                wall_start,
                            },
                        );
                    }
                }
            }
            Some(val) => {
                let json = val
                    .to_string(scope)
                    .unwrap()
                    .to_rust_string_lossy(scope);
                let _ = reply.send(Ok(RequestResult {
                    json,
                    cpu_time: cpu_elapsed,
                    wall_time: wall_start.elapsed(),
                }));
            }
            None => {
                let _ = reply.send(Err("Dispatch call failed".to_string()));
            }
        }
    }

    /// Phase 3: RESOLVE completed ops one at a time, checkpoint after each.
    fn resolve_completed_ops(&mut self, scope: &mut v8::PinScope) {
        if self.completed_ops.is_empty() {
            return;
        }

        let mut completed = Vec::new();
        std::mem::swap(&mut completed, &mut self.completed_ops);

        for (op_id, value) in completed {
            let resolver = self.el_state.borrow_mut().pending_resolvers.remove(&op_id);
            if let Some(resolver) = resolver {
                let r = v8::Local::new(scope, &resolver);
                let val = v8::String::new(scope, &value).unwrap();
                r.resolve(scope, val.into());
            }

            scope.perform_microtask_checkpoint();
            self.check_settled_promises(scope);
        }
    }

    /// Check all pending requests — if their promise settled, send the reply.
    fn check_settled_promises(&mut self, scope: &mut v8::PinScope) {
        if self.pending_requests.is_empty() {
            return;
        }

        let mut settled: Vec<u64> = Vec::new();
        for (id, req) in &self.pending_requests {
            let promise = v8::Local::new(scope, &req.promise);
            if promise.state() != v8::PromiseState::Pending {
                settled.push(*id);
            }
        }

        for id in settled {
            if let Some(mut req) = self.pending_requests.remove(&id) {
                let promise = v8::Local::new(scope, &req.promise);
                match promise.state() {
                    v8::PromiseState::Fulfilled => {
                        let json = promise
                            .result(scope)
                            .to_string(scope)
                            .unwrap()
                            .to_rust_string_lossy(scope);
                        if let Some(reply) = req.reply.take() {
                            let _ = reply.send(Ok(RequestResult {
                                json,
                                cpu_time: req.cpu_accumulated,
                                wall_time: req.wall_start.elapsed(),
                            }));
                        }
                    }
                    v8::PromiseState::Rejected => {
                        let msg = promise
                            .result(scope)
                            .to_string(scope)
                            .unwrap()
                            .to_rust_string_lossy(scope);
                        if let Some(reply) = req.reply.take() {
                            let _ = reply.send(Err(msg));
                        }
                    }
                    v8::PromiseState::Pending => {
                        self.pending_requests.insert(id, req);
                    }
                }
            }
        }
    }

    /// Compute timeout for Phase 5 WAIT.
    fn compute_wait_timeout(&self) -> Duration {
        let s = self.el_state.borrow();

        if s.timers.callbacks.is_empty() {
            return if !self.pending_requests.is_empty()
                || !s.pending_resolvers.is_empty()
                || !self.completed_ops.is_empty()
            {
                Duration::from_secs(60)
            } else {
                Duration::from_secs(60)
            };
        }

        let mut next_fire = None;
        for std::cmp::Reverse(entry) in s.timers.heap.iter() {
            if s.timers.callbacks.contains_key(&entry.id) {
                next_fire = Some(entry.fire_at);
                break;
            }
        }

        match next_fire {
            Some(fire_at) => fire_at.saturating_duration_since(Instant::now()),
            None => Duration::from_secs(60),
        }
    }

    fn has_pending_work(&self) -> bool {
        if !self.pending_requests.is_empty() || !self.completed_ops.is_empty() {
            return true;
        }
        let s = self.el_state.borrow();
        !s.timers.callbacks.is_empty() || !s.pending_resolvers.is_empty()
    }
}

// ---------------------------------------------------------------------------
// ConcurrentIsolate
// ---------------------------------------------------------------------------

/// A V8 isolate that supports multiple in-flight requests with concurrent I/O.
///
/// JS execution is serial per-request: the microtask queue contains entries
/// from exactly 1 request at any moment, enabling clean per-request kill.
pub struct ConcurrentIsolate {
    isolate: v8::OwnedIsolate,
    ls: LoopState,
    #[cfg(target_os = "linux")]
    cpu_timer: Option<crate::cpu_timer::CpuTimer>,
    #[cfg(target_os = "linux")]
    cpu_timer_active: bool,
}

// SAFETY: ConcurrentIsolate is only used on a single dedicated worker thread.
// The event channel (std::sync::mpsc) handles cross-thread communication.
unsafe impl Send for ConcurrentIsolate {}

impl ConcurrentIsolate {
    /// Create a new concurrent isolate.
    ///
    /// `cpu_limit` — if `Some`, arms a POSIX CPU timer per request batch (Linux only).
    pub fn new(
        server_js: &str,
        event_rx: std::sync::mpsc::Receiver<Event>,
        event_tx: std::sync::mpsc::Sender<Event>,
        tokio_handle: Option<tokio::runtime::Handle>,
        cpu_limit: Option<Duration>,
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

        let el_state: SharedState = Rc::new(RefCell::new(EventLoopState::new()));
        el_state.borrow_mut().tokio_handle = tokio_handle;
        el_state.borrow_mut().concurrent_event_tx = Some(event_tx.clone());
        isolate.set_slot(el_state.clone());

        let context = {
            v8::scope!(let handle_scope, &mut isolate);
            let ctx = v8::Context::new(handle_scope, Default::default());
            v8::Global::new(handle_scope, ctx)
        };

        Self {
            isolate,
            ls: LoopState {
                context,
                dispatch_fn: None,
                el_state,
                pending_requests: HashMap::new(),
                completed_ops: Vec::new(),
                event_rx,
                event_tx,
                buffered_events: VecDeque::new(),
                server_js: server_js.to_string(),
                initialized: false,
                shutdown: false,
                cpu_limit,
            },
            #[cfg(target_os = "linux")]
            cpu_timer: None,
            #[cfg(target_os = "linux")]
            cpu_timer_active: false,
        }
    }

    /// Get a clone of the event sender (for passing to async op tasks).
    pub fn event_sender(&self) -> std::sync::mpsc::Sender<Event> {
        self.ls.event_tx.clone()
    }

    /// Initialize: load server JS + compile dispatch function (once).
    fn ensure_initialized(&mut self) {
        if self.ls.initialized {
            return;
        }

        {
            v8::scope!(let handle_scope, &mut self.isolate);
            let context = v8::Local::new(handle_scope, &self.ls.context);
            let scope = &mut v8::ContextScope::new(handle_scope, context);

            setup_globals(scope);

            // Load Fetch API polyfill
            {
                let fetch_code = v8::String::new(scope, FETCH_JS).unwrap();
                let fetch_script = v8::Script::compile(scope, fetch_code, None).unwrap();
                fetch_script.run(scope).unwrap();
            }

            if !self.ls.server_js.is_empty() {
                let code = v8::String::new(scope, &self.ls.server_js).unwrap();
                let script = v8::Script::compile(scope, code, None).unwrap();
                script.run(scope).unwrap();
            }

            let code = v8::String::new(scope, DISPATCH_JS).unwrap();
            let script = v8::Script::compile(scope, code, None).unwrap();
            let result = script.run(scope).unwrap();
            let func = v8::Local::<v8::Function>::try_from(result).unwrap();
            self.ls.dispatch_fn = Some(v8::Global::new(scope, func));
        }

        self.ls.initialized = true;

        // Create POSIX CPU timer (Linux only, must be on V8 thread)
        #[cfg(target_os = "linux")]
        if self.ls.cpu_limit.is_some() {
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

    /// Arm the CPU timer before entering V8.
    fn arm_cpu_timer(&mut self) {
        #[cfg(target_os = "linux")]
        if !self.cpu_timer_active {
            if let (Some(timer), Some(limit)) = (&self.cpu_timer, self.ls.cpu_limit) {
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

    /// Run one iteration of the event loop.
    fn tick(&mut self) -> bool {
        // Arm CPU timer BEFORE entering V8 — protects sync calls like fib(35)
        self.arm_cpu_timer();

        // All V8 interaction in a block so the scope drops before disarm
        let result = {
            v8::scope!(let handle_scope, &mut self.isolate);
            let context = v8::Local::new(handle_scope, &self.ls.context);
            let scope = &mut v8::ContextScope::new(handle_scope, context);

            // Phase 1: ACCEPT
            let has_async_work = self.ls.accept_requests(scope);

            if !has_async_work {
                false
            } else {
                scope.perform_microtask_checkpoint();
                self.ls.check_settled_promises(scope);

                // Phase 2: TIMERS
                {
                    let has_timers = !self.ls.el_state.borrow().timers.callbacks.is_empty();
                    if has_timers {
                        fire_ready_timers(scope, &self.ls.el_state);
                        self.ls.check_settled_promises(scope);
                    }
                }

                // Phase 3: RESOLVE
                self.ls.resolve_completed_ops(scope);

                // Phase 4: CHECK
                self.ls.has_pending_work()
            }
        };
        // V8 scope dropped — safe to access self.isolate for timer ops

        if !result {
            self.disarm_cpu_timer();
        }

        result
    }

    /// Run the event loop. Blocks the current thread.
    pub fn run_event_loop(&mut self) {
        self.ensure_initialized();

        loop {
            if self.ls.shutdown {
                break;
            }

            self.tick();

            // Handle V8 termination (CPU limit exceeded)
            if self.isolate.is_execution_terminating() {
                self.isolate.cancel_terminate_execution();
                self.disarm_cpu_timer();
                // Drain pending requests with error
                for (_id, req) in self.ls.pending_requests.drain() {
                    if let Some(reply) = req.reply {
                        let _ = reply.send(Err("CPU time limit exceeded".to_string()));
                    }
                }
            }

            let timeout = self.ls.compute_wait_timeout();

            if timeout.is_zero() {
                continue;
            }

            match self.ls.event_rx.recv_timeout(timeout) {
                Ok(event) => {
                    self.ls.buffered_events.push_back(event);
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    while self.tick() {
                        let timeout = self.ls.compute_wait_timeout();
                        if timeout.is_zero() {
                            continue;
                        }
                        std::thread::sleep(timeout.min(Duration::from_millis(100)));
                    }

                    for (_id, req) in self.ls.pending_requests.drain() {
                        if let Some(reply) = req.reply {
                            let _ = reply.send(Err(
                                "Event loop shut down with pending request".to_string(),
                            ));
                        }
                    }
                    break;
                }
            }
        }
    }

    /// Run the event loop until all pending requests are resolved or the channel
    /// disconnects. Useful for testing.
    pub fn run_until_idle(&mut self) {
        self.ensure_initialized();

        loop {
            let has_work = self.tick();

            if !has_work {
                match self.ls.event_rx.try_recv() {
                    Ok(event) => {
                        self.ls.buffered_events.push_back(event);
                        continue;
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
                }
            }

            let timeout = self.ls.compute_wait_timeout();
            let timeout = timeout.min(Duration::from_millis(100));

            match self.ls.event_rx.recv_timeout(timeout) {
                Ok(event) => {
                    self.ls.buffered_events.push_back(event);
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    self.tick();
                    break;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helper: spawn a concurrent worker thread
// ---------------------------------------------------------------------------

/// Spawn a worker thread running a `ConcurrentIsolate` event loop.
/// Returns the event sender for dispatching requests to this worker.
pub fn spawn_concurrent_worker(
    server_js: &str,
    tokio_handle: Option<tokio::runtime::Handle>,
    cpu_limit: Option<Duration>,
) -> std::sync::mpsc::Sender<Event> {
    let (event_tx, event_rx) = std::sync::mpsc::channel();
    let event_tx_clone = event_tx.clone();
    let js = server_js.to_string();

    std::thread::Builder::new()
        .name("v8-concurrent-worker".to_string())
        .spawn(move || {
            let mut isolate =
                ConcurrentIsolate::new(&js, event_rx, event_tx_clone, tokio_handle, cpu_limit);
            isolate.run_event_loop();
        })
        .expect("Failed to spawn V8 worker thread");

    event_tx
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_ID: AtomicU64 = AtomicU64::new(1);
    fn next_id() -> u64 {
        NEXT_ID.fetch_add(1, Ordering::Relaxed)
    }

    fn rpc_request(method: &str, params: &str) -> String {
        let id = next_id();
        format!(r#"{{"jsonrpc":"2.0","method":"{method}","params":{params},"id":{id}}}"#)
    }

    const TEST_JS: &str = r#"
var __rpc = {
    ping: function() { return "pong"; },
    add: function(a, b) { return a + b; },
    delayed: function(ms) {
        return new Promise(function(resolve) {
            setTimeout(function() { resolve("done_" + ms); }, ms || 10);
        });
    },
    chain: function() {
        return new Promise(function(resolve) {
            setTimeout(function() { resolve(1); }, 5);
        }).then(function(v) { return v + 10; }).then(function(v) { return v * 2; });
    },
    fib: function(n) {
        function f(n) { return n <= 1 ? n : f(n-1) + f(n-2); }
        return f(n);
    }
};
"#;

    #[test]
    fn concurrent_sync_request() {
        init_v8();
        let (event_tx, event_rx) = std::sync::mpsc::channel();
        let event_tx_clone = event_tx.clone();

        let handle = std::thread::spawn(move || {
            let mut isolate = ConcurrentIsolate::new(TEST_JS, event_rx, event_tx_clone, None, None);
            isolate.run_until_idle();
        });

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        event_tx
            .send(Event::NewRequest {
                id: 1,
                body: rpc_request("ping", "[]"),
                reply: reply_tx,
            })
            .unwrap();

        drop(event_tx);

        let result = reply_rx.blocking_recv().unwrap().unwrap();
        assert!(result.json.contains("pong"));
        handle.join().unwrap();
    }

    #[test]
    fn concurrent_async_request() {
        init_v8();
        let (event_tx, event_rx) = std::sync::mpsc::channel();
        let event_tx_clone = event_tx.clone();

        let handle = std::thread::spawn(move || {
            let mut isolate = ConcurrentIsolate::new(TEST_JS, event_rx, event_tx_clone, None, None);
            isolate.run_until_idle();
        });

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        event_tx
            .send(Event::NewRequest {
                id: 1,
                body: rpc_request("delayed", "[10]"),
                reply: reply_tx,
            })
            .unwrap();

        drop(event_tx);

        let result = reply_rx.blocking_recv().unwrap().unwrap();
        assert!(result.json.contains("done_10"));
        assert!(result.wall_time >= Duration::from_millis(5));
        handle.join().unwrap();
    }

    #[test]
    fn concurrent_multiple_requests_overlap() {
        init_v8();
        let (event_tx, event_rx) = std::sync::mpsc::channel();
        let event_tx_clone = event_tx.clone();

        let handle = std::thread::spawn(move || {
            let mut isolate = ConcurrentIsolate::new(TEST_JS, event_rx, event_tx_clone, None, None);
            isolate.run_until_idle();
        });

        let wall_start = Instant::now();

        let mut receivers = Vec::new();
        for i in 0..3 {
            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            event_tx
                .send(Event::NewRequest {
                    id: i + 100,
                    body: rpc_request("delayed", "[20]"),
                    reply: reply_tx,
                })
                .unwrap();
            receivers.push(reply_rx);
        }

        drop(event_tx);

        for rx in receivers {
            let result = rx.blocking_recv().unwrap().unwrap();
            assert!(result.json.contains("done_20"));
        }

        let total_wall = wall_start.elapsed();
        println!(
            "3 concurrent async requests: total wall={:.0}ms (should be ~20ms, not 60ms)",
            total_wall.as_millis()
        );
        assert!(
            total_wall < Duration::from_millis(500),
            "Concurrent requests took too long: {:?}",
            total_wall
        );

        handle.join().unwrap();
    }

    #[test]
    fn concurrent_sync_and_async_mixed() {
        init_v8();
        let (event_tx, event_rx) = std::sync::mpsc::channel();
        let event_tx_clone = event_tx.clone();

        let handle = std::thread::spawn(move || {
            let mut isolate = ConcurrentIsolate::new(TEST_JS, event_rx, event_tx_clone, None, None);
            isolate.run_until_idle();
        });

        let (reply_tx1, reply_rx1) = tokio::sync::oneshot::channel();
        event_tx
            .send(Event::NewRequest {
                id: 1,
                body: rpc_request("add", "[3, 4]"),
                reply: reply_tx1,
            })
            .unwrap();

        let (reply_tx2, reply_rx2) = tokio::sync::oneshot::channel();
        event_tx
            .send(Event::NewRequest {
                id: 2,
                body: rpc_request("delayed", "[10]"),
                reply: reply_tx2,
            })
            .unwrap();

        let (reply_tx3, reply_rx3) = tokio::sync::oneshot::channel();
        event_tx
            .send(Event::NewRequest {
                id: 3,
                body: rpc_request("ping", "[]"),
                reply: reply_tx3,
            })
            .unwrap();

        drop(event_tx);

        let r1 = reply_rx1.blocking_recv().unwrap().unwrap();
        assert!(r1.json.contains("\"result\":7"));

        let r2 = reply_rx2.blocking_recv().unwrap().unwrap();
        assert!(r2.json.contains("done_10"));

        let r3 = reply_rx3.blocking_recv().unwrap().unwrap();
        assert!(r3.json.contains("pong"));

        handle.join().unwrap();
    }

    #[test]
    fn concurrent_promise_chain() {
        init_v8();
        let (event_tx, event_rx) = std::sync::mpsc::channel();
        let event_tx_clone = event_tx.clone();

        let handle = std::thread::spawn(move || {
            let mut isolate = ConcurrentIsolate::new(TEST_JS, event_rx, event_tx_clone, None, None);
            isolate.run_until_idle();
        });

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        event_tx
            .send(Event::NewRequest {
                id: 1,
                body: rpc_request("chain", "[]"),
                reply: reply_tx,
            })
            .unwrap();

        drop(event_tx);

        let result = reply_rx.blocking_recv().unwrap().unwrap();
        assert!(result.json.contains("\"result\":22"));
        handle.join().unwrap();
    }

    #[test]
    fn concurrent_worker_spawn() {
        init_v8();
        let event_tx = spawn_concurrent_worker(TEST_JS, None, None);

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        event_tx
            .send(Event::NewRequest {
                id: 1,
                body: rpc_request("ping", "[]"),
                reply: reply_tx,
            })
            .unwrap();

        let result = reply_rx.blocking_recv().unwrap().unwrap();
        assert!(result.json.contains("pong"));

        let (reply_tx2, reply_rx2) = tokio::sync::oneshot::channel();
        event_tx
            .send(Event::NewRequest {
                id: 2,
                body: rpc_request("delayed", "[5]"),
                reply: reply_tx2,
            })
            .unwrap();

        let result2 = reply_rx2.blocking_recv().unwrap().unwrap();
        assert!(result2.json.contains("done_5"));

        drop(event_tx);
    }
}
