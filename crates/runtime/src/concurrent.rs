//! Concurrent V8 isolate — serial JS, concurrent I/O execution model.
//!
//! Multiple requests in-flight with I/O overlapping, but JS execution
//! is serial per-request for clean kill safety. The microtask queue
//! contains entries from exactly 1 request at any given moment.
//!
//! Uses the unified `LoopEvent` channel from `event_loop.rs`. All events
//! (new requests, op completions, stream chunks, shutdown) flow through
//! a single `mpsc::channel<LoopEvent>`. No separate concurrent event channel.
//!
//! Event loop phases:
//!   1. DRAIN — try_recv all events from the unified channel
//!   2. DISPATCH — process NewRequest events (runs user JS)
//!   3. RESOLVE — process OpCompleted/StreamChunk via shared handle_one_event
//!   4. TIMERS — fire ready timer callbacks
//!   5. CHECK — see if any pending promises settled
//!   6. WAIT — park_timeout (zero CPU while idle, woken by EventSender)

#![allow(unsafe_code)]

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::event_loop::{self, EventLoopInner, SharedState};

// Re-export LoopEvent so callers can construct NewRequest/Shutdown events.
pub use crate::event_loop::LoopEvent;
use crate::modules::ModuleEntry;
use crate::init::{init_v8, load_polyfills_and_modules, thread_cpu_time, RequestResult};
use crate::event_loop::fire_ready_timers;

// ---------------------------------------------------------------------------
// EventSender — wraps mpsc::Sender<LoopEvent> + unparks the V8 thread
// ---------------------------------------------------------------------------

/// Sender that automatically unparks the V8 worker thread after every send.
///
/// The V8 thread uses `park_timeout` instead of `recv_timeout`. Without an
/// explicit unpark, new events would only be noticed at the next timeout
/// expiry. `EventSender` ensures zero-latency wake on every send.
#[derive(Clone)]
pub struct EventSender {
    tx: std::sync::mpsc::Sender<LoopEvent>,
    v8_thread: Arc<Mutex<Option<std::thread::Thread>>>,
}

impl EventSender {
    /// Create a new `EventSender` wrapping a bare `mpsc::Sender` and a shared
    /// thread handle. The handle may be `None` initially — the V8 thread
    /// publishes itself via `run_event_loop`.
    pub fn new(
        tx: std::sync::mpsc::Sender<LoopEvent>,
        v8_thread: Arc<Mutex<Option<std::thread::Thread>>>,
    ) -> Self {
        Self { tx, v8_thread }
    }

    /// Send an event and unpark the V8 thread so it processes immediately.
    pub fn send(&self, event: LoopEvent) -> Result<(), std::sync::mpsc::SendError<LoopEvent>> {
        let result = self.tx.send(event);
        if let Some(t) = self.v8_thread.lock().unwrap().as_ref() {
            t.unpark();
        }
        result
    }
}

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

    event_rx: std::sync::mpsc::Receiver<LoopEvent>,
    /// Small buffer for events received outside of tick (e.g., during idle check).
    buffered: Vec<LoopEvent>,

    modules: Vec<ModuleEntry>,
    initialized: bool,
    shutdown: bool,

    cpu_limit: Option<Duration>,
}

impl LoopState {
    /// Drain all events from the buffer + channel, separating by type.
    /// NewRequest/Shutdown are dispatched first (runs user JS), then I/O events
    /// are resolved via shared handle_one_event.
    fn drain_all_events(&mut self) -> (Vec<LoopEvent>, Vec<LoopEvent>) {
        let mut requests = Vec::new();
        let mut io_events = Vec::new();

        let classify = |event: LoopEvent, requests: &mut Vec<LoopEvent>, io_events: &mut Vec<LoopEvent>| {
            match &event {
                LoopEvent::NewRequest { .. } | LoopEvent::Shutdown => requests.push(event),
                LoopEvent::OpCompleted { .. } | LoopEvent::StreamChunk { .. } => io_events.push(event),
            }
        };

        // Drain buffered events first (from run_until_idle / run_event_loop)
        for event in self.buffered.drain(..) {
            classify(event, &mut requests, &mut io_events);
        }

        // Then drain the channel
        loop {
            match self.event_rx.try_recv() {
                Ok(event) => classify(event, &mut requests, &mut io_events),
                Err(_) => break,
            }
        }

        (requests, io_events)
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
                        let logs = self.el_state.borrow_mut().log_buffer.drain(..).collect();
                        let _ = reply.send(Ok(RequestResult {
                            json,
                            cpu_time: cpu_elapsed,
                            wall_time: wall_start.elapsed(),
                            logs,
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
                let logs = self.el_state.borrow_mut().log_buffer.drain(..).collect();
                let _ = reply.send(Ok(RequestResult {
                    json,
                    cpu_time: cpu_elapsed,
                    wall_time: wall_start.elapsed(),
                    logs,
                }));
            }
            None => {
                let _ = reply.send(Err("Dispatch call failed".to_string()));
            }
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
                            let logs = self.el_state.borrow_mut().log_buffer.drain(..).collect();
                            let _ = reply.send(Ok(RequestResult {
                                json,
                                cpu_time: req.cpu_accumulated,
                                wall_time: req.wall_start.elapsed(),
                                logs,
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

    /// Compute timeout for WAIT phase.
    fn compute_wait_timeout(&self) -> Duration {
        // Reuse shared timer scanning from event_loop
        match event_loop::next_timer_fire(&self.el_state) {
            Some(fire_at) => fire_at.saturating_duration_since(Instant::now()),
            None if self.has_pending_work() => {
                // Have async work pending — short wait for quick response
                Duration::from_millis(100)
            }
            None => {
                // Nothing pending — long wait for new requests
                Duration::from_secs(60)
            }
        }
    }

    fn has_pending_work(&self) -> bool {
        !self.pending_requests.is_empty()
            || event_loop::has_pending_work(&self.el_state)
    }
}

// ---------------------------------------------------------------------------
// ConcurrentIsolate
// ---------------------------------------------------------------------------

/// A V8 isolate that supports multiple in-flight requests with concurrent I/O.
///
/// JS execution is serial per-request: the microtask queue contains entries
/// from exactly 1 request at any moment, enabling clean per-request kill.
///
/// All events (new requests, op completions, stream chunks, shutdown) flow
/// through a single unified `LoopEvent` channel. Fetch tasks send directly
/// to the same channel — no separate concurrent event path.
pub struct ConcurrentIsolate {
    isolate: v8::OwnedIsolate,
    ls: LoopState,
    v8_thread: Arc<Mutex<Option<std::thread::Thread>>>,
    #[cfg(target_os = "linux")]
    cpu_timer: Option<crate::cpu_timer::CpuTimer>,
    #[cfg(target_os = "linux")]
    cpu_timer_active: bool,
}

// SAFETY: ConcurrentIsolate is only used on a single dedicated worker thread.
// The event channel (std::sync::mpsc) handles cross-thread communication.
unsafe impl Send for ConcurrentIsolate {}

impl ConcurrentIsolate {
    /// Create a new concurrent isolate for ES module format (`export function ...`).
    ///
    /// `cpu_limit` — if `Some`, arms a POSIX CPU timer per request batch (Linux only).
    pub fn new(
        modules: Vec<ModuleEntry>,
        event_rx: std::sync::mpsc::Receiver<LoopEvent>,
        event_tx: std::sync::mpsc::Sender<LoopEvent>,
        tokio_handle: Option<tokio::runtime::Handle>,
        cpu_limit: Option<Duration>,
        env_vars: std::collections::HashMap<String, String>,
    ) -> Self {
        let v8_thread = Arc::new(Mutex::new(None));
        Self::new_with_thread_handle(modules, event_rx, event_tx, tokio_handle, cpu_limit, env_vars, v8_thread)
    }

    /// Like `new`, but accepts a pre-created thread-handle Arc so callers
    /// (e.g., `spawn_concurrent_worker`) can share it with the returned `EventSender`.
    pub fn new_with_thread_handle(
        modules: Vec<ModuleEntry>,
        event_rx: std::sync::mpsc::Receiver<LoopEvent>,
        event_tx: std::sync::mpsc::Sender<LoopEvent>,
        tokio_handle: Option<tokio::runtime::Handle>,
        cpu_limit: Option<Duration>,
        env_vars: std::collections::HashMap<String, String>,
        v8_thread: Arc<Mutex<Option<std::thread::Thread>>>,
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

        // Use the unified LoopEvent channel. The EventLoopInner's own event_tx
        // is the SAME channel that the ConcurrentIsolate's event_rx reads from.
        // We replace the auto-created inner channel with the caller's channel.
        let (mut inner, _discard_rx) = EventLoopInner::with_env(env_vars);
        // Overwrite the inner event_tx with the caller's tx so fetch tasks send
        // to the same channel that the ConcurrentIsolate drains.
        inner.event_tx = event_tx;
        inner.tokio_handle = tokio_handle;

        let el_state: SharedState = Rc::new(RefCell::new(inner));
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
                event_rx,
                buffered: Vec::new(),
                modules,
                initialized: false,
                shutdown: false,
                cpu_limit,
            },
            v8_thread,
            #[cfg(target_os = "linux")]
            cpu_timer: None,
            #[cfg(target_os = "linux")]
            cpu_timer_active: false,
        }
    }

    /// Initialize: load ES modules + compile dispatch function (once).
    fn ensure_initialized(&mut self) {
        if self.ls.initialized {
            return;
        }

        // Clone modules so we don't borrow self.ls during V8 scope
        let modules = self.ls.modules.clone();

        {
            v8::scope!(let handle_scope, &mut self.isolate);
            let context = v8::Local::new(handle_scope, &self.ls.context);
            let scope = &mut v8::ContextScope::new(handle_scope, context);

            self.ls.dispatch_fn = Some(load_polyfills_and_modules(scope, &modules));
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
    ///
    /// Phases:
    /// 1. DRAIN all events from unified channel (non-blocking)
    /// 2. DISPATCH new requests (runs user JS)
    /// 3. RESOLVE I/O events via shared handle_one_event (resolves promises, delivers stream data)
    /// 4. TIMERS — fire ready timer callbacks
    /// 5. CHECK — see if any pending promises settled
    fn tick(&mut self) -> bool {
        // PHASE 1: DRAIN — get all events BEFORE creating V8 scope
        let (requests, io_events) = self.ls.drain_all_events();

        // Check if there's actually work to do
        let has_work = !requests.is_empty()
            || !io_events.is_empty()
            || !self.ls.pending_requests.is_empty()
            || !self.ls.el_state.borrow().timers.callbacks.is_empty();

        if !has_work {
            return false;
        }

        // Arm CPU timer BEFORE entering V8
        self.arm_cpu_timer();

        // All V8 interaction in a block so the scope drops before disarm
        let result = {
            v8::scope!(let handle_scope, &mut self.isolate);
            let context = v8::Local::new(handle_scope, &self.ls.context);
            let scope = &mut v8::ContextScope::new(handle_scope, context);

            // PHASE 2: DISPATCH new requests (runs user JS)
            for event in requests {
                match event {
                    LoopEvent::NewRequest { id, body, reply } => {
                        self.ls.dispatch_request(scope, id, body, reply);
                    }
                    LoopEvent::Shutdown => {
                        self.ls.shutdown = true;
                        for (_id, req) in self.ls.pending_requests.drain() {
                            if let Some(reply) = req.reply {
                                let _ = reply.send(Err("Isolate shutting down".to_string()));
                            }
                        }
                    }
                    _ => {} // unreachable — drain_all_events separates by type
                }
            }

            scope.perform_microtask_checkpoint();
            self.ls.check_settled_promises(scope);

            // PHASE 3: RESOLVE I/O events via shared handle_one_event
            for event in io_events {
                event_loop::handle_one_event(scope, &self.ls.el_state, event);
                self.ls.check_settled_promises(scope);
            }

            // PHASE 4: TIMERS
            {
                let has_timers = !self.ls.el_state.borrow().timers.callbacks.is_empty();
                if has_timers {
                    fire_ready_timers(scope, &self.ls.el_state);
                    self.ls.check_settled_promises(scope);
                }
            }

            // PHASE 5: CHECK
            self.ls.has_pending_work()
        };

        self.disarm_cpu_timer();

        result
    }

    /// Run the event loop. Blocks the current thread.
    ///
    /// Uses `park_timeout` instead of `recv_timeout`. The V8 thread is woken by:
    /// 1. `EventSender.send()` — unparks after every send (new requests, op completions)
    /// 2. Timeout — `park_timeout` returns when the next timer should fire
    /// 3. Spurious wakes — harmless, just re-polls
    pub fn run_event_loop(&mut self) {
        // Publish our thread handle so EventSenders can unpark us.
        *self.v8_thread.lock().unwrap() = Some(std::thread::current());

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

            // Park the thread — woken by EventSender.send() → thread.unpark()
            // or by timeout when the next timer should fire.
            std::thread::park_timeout(timeout);
        }
    }

    /// Run the event loop until all pending requests are resolved or the channel
    /// disconnects. Useful for testing.
    pub fn run_until_idle(&mut self) {
        // Publish our thread handle so EventSenders can unpark us.
        *self.v8_thread.lock().unwrap() = Some(std::thread::current());

        self.ensure_initialized();

        loop {
            let has_work = self.tick();

            if !has_work {
                // Check if there are any new events (race between tick's drain and new sends)
                match self.ls.event_rx.try_recv() {
                    Ok(event) => {
                        // Event arrived between drain and this check. Buffer it
                        // for the next tick to process.
                        self.ls.buffered.push(event);
                        continue;
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
                }
            }

            let timeout = self.ls.compute_wait_timeout();
            let timeout = timeout.min(Duration::from_millis(100));

            // Park instead of recv_timeout — woken by EventSender.send() unpark.
            std::thread::park_timeout(timeout);
        }
    }
}

// ---------------------------------------------------------------------------
// Helper: spawn a concurrent worker thread
// ---------------------------------------------------------------------------

/// Spawn a worker thread running a `ConcurrentIsolate` event loop.
/// Returns an `EventSender` for dispatching requests to this worker.
/// The sender automatically unparks the V8 thread after every send.
pub fn spawn_concurrent_worker(
    modules: Vec<ModuleEntry>,
    tokio_handle: Option<tokio::runtime::Handle>,
    cpu_limit: Option<Duration>,
    env_vars: std::collections::HashMap<String, String>,
) -> EventSender {
    let (event_tx, event_rx) = std::sync::mpsc::channel();
    let event_tx_for_caller = event_tx.clone();

    // Create a shared v8_thread handle. The ConcurrentIsolate will publish
    // the actual thread handle once run_event_loop starts. All EventSenders
    // (returned here AND cloned inside fetch tasks) share this Arc.
    let v8_thread: Arc<Mutex<Option<std::thread::Thread>>> = Arc::new(Mutex::new(None));
    let v8_thread_inner = v8_thread.clone();

    std::thread::Builder::new()
        .name("v8-concurrent-worker".to_string())
        .spawn(move || {
            let mut isolate = ConcurrentIsolate::new_with_thread_handle(
                modules, event_rx, event_tx, tokio_handle, cpu_limit, env_vars,
                v8_thread_inner,
            );
            isolate.run_event_loop();
        })
        .expect("Failed to spawn V8 worker thread");

    EventSender {
        tx: event_tx_for_caller,
        v8_thread,
    }
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
export function fib(n) {
    function f(n) { return n <= 1 ? n : f(n-1) + f(n-2); }
    return f(n);
}
"#.into(),
        }]
    }

    #[test]
    fn concurrent_sync_request() {
        init_v8();
        let (event_tx, event_rx) = std::sync::mpsc::channel();
        let event_tx_clone = event_tx.clone();

        let handle = std::thread::spawn(move || {
            let mut isolate = ConcurrentIsolate::new(test_modules(), event_rx, event_tx_clone, None, None, HashMap::new());
            isolate.run_until_idle();
        });

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        event_tx
            .send(LoopEvent::NewRequest {
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
            let mut isolate = ConcurrentIsolate::new(test_modules(), event_rx, event_tx_clone, None, None, HashMap::new());
            isolate.run_until_idle();
        });

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        event_tx
            .send(LoopEvent::NewRequest {
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
            let mut isolate = ConcurrentIsolate::new(test_modules(), event_rx, event_tx_clone, None, None, HashMap::new());
            isolate.run_until_idle();
        });

        let wall_start = Instant::now();

        let mut receivers = Vec::new();
        for i in 0..3 {
            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            event_tx
                .send(LoopEvent::NewRequest {
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
            let mut isolate = ConcurrentIsolate::new(test_modules(), event_rx, event_tx_clone, None, None, HashMap::new());
            isolate.run_until_idle();
        });

        let (reply_tx1, reply_rx1) = tokio::sync::oneshot::channel();
        event_tx
            .send(LoopEvent::NewRequest {
                id: 1,
                body: rpc_request("add", "[3, 4]"),
                reply: reply_tx1,
            })
            .unwrap();

        let (reply_tx2, reply_rx2) = tokio::sync::oneshot::channel();
        event_tx
            .send(LoopEvent::NewRequest {
                id: 2,
                body: rpc_request("delayed", "[10]"),
                reply: reply_tx2,
            })
            .unwrap();

        let (reply_tx3, reply_rx3) = tokio::sync::oneshot::channel();
        event_tx
            .send(LoopEvent::NewRequest {
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
            let mut isolate = ConcurrentIsolate::new(test_modules(), event_rx, event_tx_clone, None, None, HashMap::new());
            isolate.run_until_idle();
        });

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        event_tx
            .send(LoopEvent::NewRequest {
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
        let event_tx = spawn_concurrent_worker(test_modules(), None, None, HashMap::new());

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        event_tx
            .send(LoopEvent::NewRequest {
                id: 1,
                body: rpc_request("ping", "[]"),
                reply: reply_tx,
            })
            .unwrap();

        let result = reply_rx.blocking_recv().unwrap().unwrap();
        assert!(result.json.contains("pong"));

        let (reply_tx2, reply_rx2) = tokio::sync::oneshot::channel();
        event_tx
            .send(LoopEvent::NewRequest {
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
