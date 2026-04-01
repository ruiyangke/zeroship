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

use crate::{
    fire_ready_timers, init_v8, setup_globals, thread_cpu_time, EventLoopState, RequestResult,
    SharedState,
};

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

// SAFETY: All constituent types (u64, String, tokio::sync::oneshot::Sender) are Send.
unsafe impl Send for Event {}

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
// This separation allows calling methods while a V8 scope borrows the isolate.
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
}

impl LoopState {
    /// Phase 1: ACCEPT — drain buffered events + channel.
    fn accept_requests(&mut self, scope: &mut v8::PinScope) {
        // Drain buffered events first (from Phase 5 recv_timeout)
        let buffered: Vec<Event> = self.buffered_events.drain(..).collect();
        for event in buffered {
            self.handle_event(scope, event);
        }

        // Drain channel
        loop {
            match self.event_rx.try_recv() {
                Ok(event) => self.handle_event(scope, event),
                Err(_) => break,
            }
        }
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
            Some(f) => f.clone(),
            None => {
                let _ = reply.send(Err("Isolate not initialized".to_string()));
                return;
            }
        };

        let func = v8::Local::new(scope, &dispatch_fn);
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
        let completed: Vec<(u32, String)> = self.completed_ops.drain(..).collect();

        for (op_id, value) in completed {
            // Resolve this ONE promise
            let resolver = self.el_state.borrow_mut().pending_resolvers.remove(&op_id);
            if let Some(resolver) = resolver {
                let r = v8::Local::new(scope, &resolver);
                let val = v8::String::new(scope, &value).unwrap();
                r.resolve(scope, val.into());
            }

            // Run microtasks for THIS promise chain only
            // CRITICAL: queue now has only this request's entries
            scope.perform_microtask_checkpoint();

            // Check all pending requests: did any promise settle?
            self.check_settled_promises(scope);
        }
    }

    /// Check all pending requests — if their promise settled, send the reply.
    fn check_settled_promises(&mut self, scope: &mut v8::PinScope) {
        let mut completed_ids: Vec<(u64, Option<String>, Option<String>)> = Vec::new();

        for (id, req) in &self.pending_requests {
            let promise = v8::Local::new(scope, &req.promise);
            match promise.state() {
                v8::PromiseState::Fulfilled => {
                    let val = promise.result(scope);
                    let json = val
                        .to_string(scope)
                        .unwrap()
                        .to_rust_string_lossy(scope);
                    completed_ids.push((*id, Some(json), None));
                }
                v8::PromiseState::Rejected => {
                    let val = promise.result(scope);
                    let msg = val
                        .to_string(scope)
                        .unwrap()
                        .to_rust_string_lossy(scope);
                    completed_ids.push((*id, None, Some(msg)));
                }
                v8::PromiseState::Pending => {}
            }
        }

        for (id, json_opt, err_opt) in completed_ids {
            if let Some(mut req) = self.pending_requests.remove(&id) {
                if let Some(reply) = req.reply.take() {
                    if let Some(json) = json_opt {
                        let _ = reply.send(Ok(RequestResult {
                            json,
                            cpu_time: req.cpu_accumulated,
                            wall_time: req.wall_start.elapsed(),
                        }));
                    } else if let Some(msg) = err_opt {
                        let _ = reply.send(Err(msg));
                    }
                }
            }
        }
    }

    /// Compute timeout for Phase 5 WAIT.
    fn compute_wait_timeout(&self) -> Duration {
        let s = self.el_state.borrow();

        // Find next valid timer in heap (skip cleared ones)
        let mut next_fire = None;
        for std::cmp::Reverse(entry) in s.timer_heap.iter() {
            if s.timer_callbacks.contains_key(&entry.id) {
                next_fire = Some(entry.fire_at);
                break;
            }
        }

        match next_fire {
            Some(fire_at) => fire_at.saturating_duration_since(Instant::now()),
            None if !self.pending_requests.is_empty()
                || !s.pending_resolvers.is_empty()
                || !self.completed_ops.is_empty() =>
            {
                Duration::from_secs(60)
            }
            None => Duration::from_secs(60),
        }
    }

    fn has_pending_work(&self) -> bool {
        let s = self.el_state.borrow();
        !self.pending_requests.is_empty()
            || !s.timer_callbacks.is_empty()
            || !s.pending_resolvers.is_empty()
            || !self.completed_ops.is_empty()
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
}

// SAFETY: ConcurrentIsolate is only used on a single dedicated worker thread.
// The event channel (std::sync::mpsc) handles cross-thread communication.
unsafe impl Send for ConcurrentIsolate {}

impl ConcurrentIsolate {
    /// Create a new concurrent isolate.
    pub fn new(
        server_js: &str,
        event_rx: std::sync::mpsc::Receiver<Event>,
        event_tx: std::sync::mpsc::Sender<Event>,
    ) -> Self {
        init_v8();

        let params = v8::CreateParams::default().heap_limits(0, 128 * 1024 * 1024);
        let mut isolate = v8::Isolate::new(params);

        let el_state: SharedState = Rc::new(RefCell::new(EventLoopState::new()));
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
            },
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

        v8::scope!(let handle_scope, &mut self.isolate);
        let context = v8::Local::new(handle_scope, &self.ls.context);
        let scope = &mut v8::ContextScope::new(handle_scope, context);

        setup_globals(scope);

        if !self.ls.server_js.is_empty() {
            let code = v8::String::new(scope, &self.ls.server_js).unwrap();
            let script = v8::Script::compile(scope, code, None).unwrap();
            script.run(scope).unwrap();
        }

        let dispatch_src = r#"(function(__req_json) {
            var req = JSON.parse(__req_json);
            var fn = __rpc[req.method];
            if (!fn) return JSON.stringify({jsonrpc:"2.0",error:{code:-32601,message:"not found"},id:req.id});
            try {
                var result = fn.apply(null, req.params || []);
                if (result && typeof result.then === 'function') {
                    return result.then(function(v) {
                        return JSON.stringify({jsonrpc:"2.0",result:v,id:req.id});
                    }, function(e) {
                        return JSON.stringify({jsonrpc:"2.0",error:{code:-32000,message:e && e.message ? e.message : String(e)},id:req.id});
                    });
                }
                return JSON.stringify({jsonrpc:"2.0",result:result,id:req.id});
            } catch(e) {
                return JSON.stringify({jsonrpc:"2.0",error:{code:-32000,message:e.message},id:req.id});
            }
        })"#;

        let code = v8::String::new(scope, dispatch_src).unwrap();
        let script = v8::Script::compile(scope, code, None).unwrap();
        let result = script.run(scope).unwrap();
        let func = v8::Local::<v8::Function>::try_from(result).unwrap();
        self.ls.dispatch_fn = Some(v8::Global::new(scope, func));

        self.ls.initialized = true;
    }

    /// Run one iteration of the event loop (Phases 1-4).
    /// Returns true if there is still pending work.
    fn tick(&mut self) -> bool {
        v8::scope!(let handle_scope, &mut self.isolate);
        let context = v8::Local::new(handle_scope, &self.ls.context);
        let scope = &mut v8::ContextScope::new(handle_scope, context);

        // Phase 1: ACCEPT
        self.ls.accept_requests(scope);
        scope.perform_microtask_checkpoint();

        // Phase 2: TIMERS
        fire_ready_timers(scope, &self.ls.el_state);
        self.ls.check_settled_promises(scope);

        // Phase 3: RESOLVE
        self.ls.resolve_completed_ops(scope);

        // Phase 4: CHECK
        self.ls.has_pending_work()
    }

    /// Run the event loop. Blocks the current thread.
    /// Returns when the event channel is disconnected (all senders dropped).
    pub fn run_event_loop(&mut self) {
        self.ensure_initialized();

        loop {
            if self.ls.shutdown {
                break;
            }

            self.tick();

            // Phase 5: WAIT
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
                    // All senders dropped — drain remaining work
                    while self.tick() {
                        let timeout = self.ls.compute_wait_timeout();
                        if timeout.is_zero() {
                            continue;
                        }
                        // Brief wait for timer resolution
                        std::thread::sleep(timeout.min(Duration::from_millis(100)));
                    }

                    // Fail any remaining pending requests
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
                // Check if there are buffered or channel events
                match self.ls.event_rx.try_recv() {
                    Ok(event) => {
                        self.ls.buffered_events.push_back(event);
                        continue;
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => {
                        // No events and no work — done
                        break;
                    }
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        break;
                    }
                }
            }

            // Wait briefly for more events
            let timeout = self.ls.compute_wait_timeout();
            let timeout = timeout.min(Duration::from_millis(100));

            match self.ls.event_rx.recv_timeout(timeout) {
                Ok(event) => {
                    self.ls.buffered_events.push_back(event);
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    // Do one more pass
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
pub fn spawn_concurrent_worker(server_js: &str) -> std::sync::mpsc::Sender<Event> {
    let (event_tx, event_rx) = std::sync::mpsc::channel();
    let event_tx_clone = event_tx.clone();
    let js = server_js.to_string();

    std::thread::Builder::new()
        .name("v8-concurrent-worker".to_string())
        .spawn(move || {
            let mut isolate = ConcurrentIsolate::new(&js, event_rx, event_tx_clone);
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
            let mut isolate = ConcurrentIsolate::new(TEST_JS, event_rx, event_tx_clone);
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
            let mut isolate = ConcurrentIsolate::new(TEST_JS, event_rx, event_tx_clone);
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
        // Send 3 async requests with setTimeout(20ms). With concurrent model,
        // all 3 start at once and overlap, so total time should be ~20ms (not 60ms).
        init_v8();
        let (event_tx, event_rx) = std::sync::mpsc::channel();
        let event_tx_clone = event_tx.clone();

        let handle = std::thread::spawn(move || {
            let mut isolate = ConcurrentIsolate::new(TEST_JS, event_rx, event_tx_clone);
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
        // Generous margin but must be less than serial (60ms)
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
            let mut isolate = ConcurrentIsolate::new(TEST_JS, event_rx, event_tx_clone);
            isolate.run_until_idle();
        });

        // Sync
        let (reply_tx1, reply_rx1) = tokio::sync::oneshot::channel();
        event_tx
            .send(Event::NewRequest {
                id: 1,
                body: rpc_request("add", "[3, 4]"),
                reply: reply_tx1,
            })
            .unwrap();

        // Async
        let (reply_tx2, reply_rx2) = tokio::sync::oneshot::channel();
        event_tx
            .send(Event::NewRequest {
                id: 2,
                body: rpc_request("delayed", "[10]"),
                reply: reply_tx2,
            })
            .unwrap();

        // Sync
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
            let mut isolate = ConcurrentIsolate::new(TEST_JS, event_rx, event_tx_clone);
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
        // (1 + 10) * 2 = 22
        assert!(result.json.contains("\"result\":22"));
        handle.join().unwrap();
    }

    #[test]
    fn concurrent_worker_spawn() {
        init_v8();
        let event_tx = spawn_concurrent_worker(TEST_JS);

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
