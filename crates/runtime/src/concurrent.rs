//! Concurrent V8 isolate — serial JS, concurrent I/O execution model.
//!
//! Multiple requests in-flight with I/O overlapping, but JS execution
//! is serial per-request for clean kill safety. The microtask queue
//! contains entries from exactly 1 request at any given moment.
//!
//! Uses a `std::sync::mpsc` channel for receiving events (NewRequest, OpCompleted,
//! StreamChunk, Shutdown). V8 callback state lives in `state::RuntimeState`, set
//! on the isolate slot as `state::SharedState`.
//!
//! Event loop phases:
//!   1. DRAIN — try_recv all events from the unified channel
//!   2. DISPATCH — process NewRequest events (runs user JS)
//!   3. RESOLVE — process OpCompleted/StreamChunk events
//!   4. TIMERS — fire ready timer callbacks
//!   5. CHECK — see if any pending promises settled
//!   6. WAIT — park_timeout (zero CPU while idle, woken by EventSender)

#![allow(unsafe_code)]

use std::cell::RefCell;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::state::{RuntimeState, SharedState, SpawnedTimer};
use crate::modules::ModuleEntry;
use crate::init::{init_v8, load_polyfills_and_modules, thread_cpu_time, RequestResult};

// ---------------------------------------------------------------------------
// LoopEvent — events flowing through the event channel
// ---------------------------------------------------------------------------

/// Events flowing through the event channel.
pub enum LoopEvent {
    OpCompleted { id: u32, value: String },
    StreamChunk { stream_id: u32, data: Vec<u8>, done: bool },
    /// A new RPC request from the HTTP layer.
    NewRequest {
        id: u64,
        body: String,
        reply: tokio::sync::oneshot::Sender<Result<RequestResult, String>>,
    },
    /// Graceful shutdown signal.
    Shutdown,
}

// ---------------------------------------------------------------------------
// Timer heap (for blocking event loop — not tokio)
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// EventSender — wraps mpsc::Sender<LoopEvent> + unparks the V8 thread
// ---------------------------------------------------------------------------

/// Sender that automatically unparks the V8 worker thread after every send.
#[derive(Clone)]
pub struct EventSender {
    tx: std::sync::mpsc::Sender<LoopEvent>,
    v8_thread: Arc<Mutex<Option<std::thread::Thread>>>,
}

impl EventSender {
    pub fn new(
        tx: std::sync::mpsc::Sender<LoopEvent>,
        v8_thread: Arc<Mutex<Option<std::thread::Thread>>>,
    ) -> Self {
        Self { tx, v8_thread }
    }

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
    state: SharedState,

    /// Timer heap for the blocking event loop.
    timer_heap: BinaryHeap<Reverse<TimerHeapEntry>>,

    pending_requests: HashMap<u64, PendingRequest>,

    event_rx: std::sync::mpsc::Receiver<LoopEvent>,
    event_tx: std::sync::mpsc::Sender<LoopEvent>,
    tokio_handle: Option<tokio::runtime::Handle>,
    /// Small buffer for events received outside of tick.
    buffered: Vec<LoopEvent>,

    modules: Vec<ModuleEntry>,
    initialized: bool,
    shutdown: bool,

    cpu_limit: Option<Duration>,
}

impl LoopState {
    /// Drain all events from the buffer + channel, separating by type.
    fn drain_all_events(&mut self) -> (Vec<LoopEvent>, Vec<LoopEvent>) {
        let mut requests = Vec::new();
        let mut io_events = Vec::new();

        let classify = |event: LoopEvent, requests: &mut Vec<LoopEvent>, io_events: &mut Vec<LoopEvent>| {
            match &event {
                LoopEvent::NewRequest { .. } | LoopEvent::Shutdown => requests.push(event),
                LoopEvent::OpCompleted { .. } | LoopEvent::StreamChunk { .. } => io_events.push(event),
            }
        };

        for event in self.buffered.drain(..) {
            classify(event, &mut requests, &mut io_events);
        }

        loop {
            match self.event_rx.try_recv() {
                Ok(event) => classify(event, &mut requests, &mut io_events),
                Err(_) => break,
            }
        }

        (requests, io_events)
    }

    /// Collect spawned timers from RuntimeState into the timer heap.
    fn collect_spawned_timers(&mut self) {
        let mut s = self.state.borrow_mut();
        let timers: Vec<SpawnedTimer> = s.spawned_timers.drain(..).collect();
        let now = Instant::now();
        for timer in timers {
            self.timer_heap.push(Reverse(TimerHeapEntry {
                fire_at: now + timer.delay,
                id: timer.id,
            }));
        }
        // Drain ready_timers (zero-delay) — schedule them to fire immediately.
        for id in s.ready_timers.drain(..) {
            self.timer_heap.push(Reverse(TimerHeapEntry {
                fire_at: now,
                id,
            }));
        }
    }

    /// Collect spawned ops from RuntimeState — spawn them on tokio and route
    /// results back through the event channel.
    ///
    /// SAFETY: The spawned_ops futures are `dyn Future` (not `+ Send`) because
    /// `RuntimeState` is `!Send`. However, the actual futures created by
    /// `#[appbase_op(async)]` only capture owned data (Strings, u32s, etc.)
    /// and are in practice Send. We assert Send here to spawn them on tokio.
    /// This code path will be removed when concurrent.rs is replaced by Runtime.
    fn collect_spawned_ops(&mut self) {
        let ops: Vec<_> = self.state.borrow_mut().spawned_ops.drain(..).collect();
        if ops.is_empty() {
            return;
        }

        for op_future in ops {
            let event_tx = self.event_tx.clone();

            // Wrap the !Send future in a Send wrapper.
            // SAFETY: The actual async op futures (fetch, etc.) only capture
            // owned data and are effectively Send.
            struct SendFuture(std::pin::Pin<Box<dyn std::future::Future<Output = crate::state::OpResult>>>);
            unsafe impl Send for SendFuture {}
            impl std::future::Future for SendFuture {
                type Output = crate::state::OpResult;
                fn poll(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Self::Output> {
                    self.0.as_mut().poll(cx)
                }
            }

            let send_future = SendFuture(op_future);

            if let Some(handle) = &self.tokio_handle {
                handle.spawn(async move {
                    let result = send_future.await;
                    match result {
                        crate::state::OpResult::Completed { op_id, value, .. } => {
                            let _ = event_tx.send(LoopEvent::OpCompleted { id: op_id, value });
                        }
                        crate::state::OpResult::StreamChunk { stream_id, data, done } => {
                            let _ = event_tx.send(LoopEvent::StreamChunk { stream_id, data, done });
                        }
                        crate::state::OpResult::Cancelled => {}
                    }
                });
            } else {
                // No tokio handle — spawn thread as fallback
                std::thread::spawn(move || {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("Failed to create tokio runtime for async op");
                    let result = rt.block_on(send_future);
                    match result {
                        crate::state::OpResult::Completed { op_id, value, .. } => {
                            let _ = event_tx.send(LoopEvent::OpCompleted { id: op_id, value });
                        }
                        crate::state::OpResult::StreamChunk { stream_id, data, done } => {
                            let _ = event_tx.send(LoopEvent::StreamChunk { stream_id, data, done });
                        }
                        crate::state::OpResult::Cancelled => {}
                    }
                });
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
                        let logs = self.state.borrow_mut().per_request_logs.remove(&0).unwrap_or_default();
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
                let logs = self.state.borrow_mut().per_request_logs.remove(&0).unwrap_or_default();
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
                            let logs = self.state.borrow_mut().per_request_logs.remove(&0).unwrap_or_default();
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

    /// Fire all timers whose fire_at <= now.
    fn fire_ready_timers(&mut self, scope: &mut v8::PinScope) -> bool {
        let mut any_fired = false;
        let now = Instant::now();

        loop {
            let should_fire = self
                .timer_heap
                .peek()
                .map(|Reverse(e)| e.fire_at <= now)
                .unwrap_or(false);
            if !should_fire {
                break;
            }

            let entry = self.timer_heap.pop().unwrap().0;

            // Take callback out (lazy deletion: cleared timers won't have an entry)
            let cb_opt = self.state.borrow_mut().timer_callbacks.remove(&entry.id);

            if let Some(cb) = cb_opt {
                any_fired = true;

                let func = v8::Local::new(scope, &cb.callback);
                let undefined = v8::undefined(scope).into();
                func.call(scope, undefined, &[]);
                scope.perform_microtask_checkpoint();

                if let Some(dur) = cb.interval {
                    // setInterval: re-insert callback + new heap entry
                    self.state.borrow_mut().timer_callbacks.insert(entry.id, cb);
                    self.timer_heap.push(Reverse(TimerHeapEntry {
                        fire_at: Instant::now() + dur,
                        id: entry.id,
                    }));
                }
            }
        }

        any_fired
    }

    /// Compute timeout for WAIT phase.
    fn compute_wait_timeout(&self) -> Duration {
        // Find next timer fire time
        let next_timer = {
            let s = self.state.borrow();
            let mut result = None;
            for Reverse(entry) in self.timer_heap.iter() {
                if s.timer_callbacks.contains_key(&entry.id) {
                    result = Some(entry.fire_at);
                    break;
                }
            }
            result
        };

        match next_timer {
            Some(fire_at) => fire_at.saturating_duration_since(Instant::now()),
            None if self.has_pending_work() => Duration::from_millis(100),
            None => Duration::from_secs(60),
        }
    }

    fn has_pending_work(&self) -> bool {
        let s = self.state.borrow();
        !self.pending_requests.is_empty()
            || !s.timer_callbacks.is_empty()
            || !s.pending_resolvers.is_empty()
            || s.streams.values().any(|st| st.pending_read.is_some() && !st.closed)
    }

    /// Handle an I/O event (OpCompleted or StreamChunk).
    fn handle_io_event(&self, scope: &mut v8::PinScope, event: LoopEvent) {
        match event {
            LoopEvent::OpCompleted { id, value } => {
                crate::request::resolve_op(scope, &self.state, id, &value);
            }
            LoopEvent::StreamChunk { stream_id, data, done } => {
                crate::streams::push_stream_chunk(scope, &self.state, stream_id, &data, done);
                scope.perform_microtask_checkpoint();
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// ConcurrentIsolate
// ---------------------------------------------------------------------------

/// A V8 isolate that supports multiple in-flight requests with concurrent I/O.
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
unsafe impl Send for ConcurrentIsolate {}

impl ConcurrentIsolate {
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
        let rt_state = RuntimeState::new(env_vars, None);
        let state: SharedState = Rc::new(RefCell::new(rt_state));
        isolate.set_slot(state.clone());

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
                state,
                timer_heap: BinaryHeap::new(),
                pending_requests: HashMap::new(),
                event_rx,
                event_tx,
                tokio_handle,
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

    fn ensure_initialized(&mut self) {
        if self.ls.initialized {
            return;
        }

        let modules = self.ls.modules.clone();

        {
            v8::scope!(let handle_scope, &mut self.isolate);
            let context = v8::Local::new(handle_scope, &self.ls.context);
            let scope = &mut v8::ContextScope::new(handle_scope, context);
            self.ls.dispatch_fn = Some(load_polyfills_and_modules(scope, &modules));
        }

        self.ls.initialized = true;

        #[cfg(target_os = "linux")]
        if self.ls.cpu_limit.is_some() {
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

    fn arm_cpu_timer(&mut self) {
        #[cfg(target_os = "linux")]
        if !self.cpu_timer_active {
            if let (Some(timer), Some(limit)) = (&self.cpu_timer, self.ls.cpu_limit) {
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

    fn tick(&mut self) -> bool {
        // Collect spawned ops/timers from RuntimeState before draining events
        self.ls.collect_spawned_ops();
        self.ls.collect_spawned_timers();

        // PHASE 1: DRAIN
        let (requests, io_events) = self.ls.drain_all_events();

        let has_work = !requests.is_empty()
            || !io_events.is_empty()
            || !self.ls.pending_requests.is_empty()
            || !self.ls.state.borrow().timer_callbacks.is_empty();

        if !has_work {
            return false;
        }

        self.arm_cpu_timer();

        let result = {
            v8::scope!(let handle_scope, &mut self.isolate);
            let context = v8::Local::new(handle_scope, &self.ls.context);
            let scope = &mut v8::ContextScope::new(handle_scope, context);

            // Set request context for logging
            self.ls.state.borrow_mut().executing_request_id = Some(0);

            // PHASE 2: DISPATCH new requests
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
                    _ => {}
                }
            }

            scope.perform_microtask_checkpoint();

            // Collect any new ops/timers spawned during dispatch
            self.ls.collect_spawned_ops();
            self.ls.collect_spawned_timers();

            self.ls.check_settled_promises(scope);

            // PHASE 3: RESOLVE I/O events
            for event in io_events {
                self.ls.handle_io_event(scope, event);
                self.ls.check_settled_promises(scope);
            }

            // PHASE 4: TIMERS
            if !self.ls.state.borrow().timer_callbacks.is_empty() {
                self.ls.fire_ready_timers(scope);
                // Collect timers spawned by timer callbacks
                self.ls.collect_spawned_timers();
                self.ls.check_settled_promises(scope);
            }

            // PHASE 5: CHECK
            self.ls.has_pending_work()
        };

        self.disarm_cpu_timer();

        result
    }

    /// Run the event loop. Blocks the current thread.
    pub fn run_event_loop(&mut self) {
        *self.v8_thread.lock().unwrap() = Some(std::thread::current());
        self.ensure_initialized();

        loop {
            if self.ls.shutdown {
                break;
            }

            self.tick();

            if self.isolate.is_execution_terminating() {
                self.isolate.cancel_terminate_execution();
                self.disarm_cpu_timer();
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

            std::thread::park_timeout(timeout);
        }
    }

    /// Run the event loop until all pending requests are resolved or the channel
    /// disconnects. Useful for testing.
    pub fn run_until_idle(&mut self) {
        *self.v8_thread.lock().unwrap() = Some(std::thread::current());
        self.ensure_initialized();

        loop {
            let has_work = self.tick();

            if !has_work {
                match self.ls.event_rx.try_recv() {
                    Ok(event) => {
                        self.ls.buffered.push(event);
                        continue;
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
                }
            }

            let timeout = self.ls.compute_wait_timeout();
            let timeout = timeout.min(Duration::from_millis(100));

            std::thread::park_timeout(timeout);
        }
    }
}

// ---------------------------------------------------------------------------
// Helper: spawn a concurrent worker thread
// ---------------------------------------------------------------------------

pub fn spawn_concurrent_worker(
    modules: Vec<ModuleEntry>,
    tokio_handle: Option<tokio::runtime::Handle>,
    cpu_limit: Option<Duration>,
    env_vars: std::collections::HashMap<String, String>,
) -> EventSender {
    let (event_tx, event_rx) = std::sync::mpsc::channel();
    let event_tx_for_caller = event_tx.clone();

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
