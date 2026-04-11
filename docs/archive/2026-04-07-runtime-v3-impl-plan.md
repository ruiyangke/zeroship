# Runtime v3 — Inverted Control Flow Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the hand-rolled tick/park event loop with a tokio `select!` loop where V8 is entered/exited as callbacks, modeled after Cloudflare workerd.

**Architecture:** Dedicated OS thread per isolate running a current-thread tokio runtime. A single `select!` loop waits on request channel + `FuturesUnordered` (fetch ops, timers). Each event enters V8 via `enter_v8()`, runs JS + microtasks, exits. No tick loop, no manual wake mechanism.

**Tech Stack:** Rust, v8 crate, tokio (current-thread), tokio-util (CancellationToken), reqwest, futures (FuturesUnordered)

**Spec:** `docs/plans/2026-04-07-runtime-v3-inverted-control-flow.md`

---

## File Structure

### New files
| File | Responsibility |
|------|---------------|
| `crates/runtime/src/runtime.rs` | `Runtime` struct, `enter_v8()`, `run()` select! loop, `collect_new_tasks()`, `handle_incoming_request()`, `handle_op_result()`, `handle_timer()`, `check_settled_promises_v8()`, `graceful_shutdown()` |
| `crates/runtime/src/state.rs` | `RuntimeState`, `SharedState` type alias, `RequestContext`, `PendingRequest`, `OpResult`, `TimerResult`, `DispatchResult`, `IncomingRequest` enums/structs, constants |
| `crates/runtime/src/request.rs` | Free functions: `dispatch_request()`, `resolve_op()`, `extract_promise_result()`, `fire_timer_callback()`, `push_stream_chunk_v3()` — all take `(scope, state, ...)` to avoid borrow conflicts |

### Deleted files
| File | Replaced by |
|------|------------|
| `crates/runtime/src/concurrent.rs` (943 LOC) | `runtime.rs` + `state.rs` + `request.rs` |
| `crates/runtime/src/event_loop.rs` (353 LOC) | `state.rs` (RuntimeState) + `runtime.rs` (enter_v8) |

### Modified files
| File | Changes |
|------|---------|
| `crates/runtime/src/lib.rs` | New module declarations, re-exports, test updates |
| `crates/runtime/src/fetch.rs` | Push future into `spawned_ops` instead of `handle.spawn()` + channel send |
| `crates/runtime/src/timers.rs` | Push into `spawned_timers` instead of heap. Remove `fire_ready_timers()`. |
| `crates/runtime/src/init.rs` | Use new `SharedState` from `state.rs` instead of `event_loop::SharedState` |
| `crates/runtime/src/streams.rs` | Import from `state` instead of `event_loop` |
| `crates/runtime/src/crypto.rs` | Import from `state` instead of `event_loop` |
| `crates/runtime/src/kv.rs` | Import from `state` instead of `event_loop` |
| `crates/runtime/src/env.rs` | Import from `state` instead of `event_loop` |
| `crates/runtime/src/url.rs` | Import from `state` instead of `event_loop` |
| `crates/runtime/src/server.rs` | Use `Runtime` + `IncomingRequest` instead of `ConcurrentIsolate` |
| `crates/platform/src/server/v8pool.rs` | Use `tokio::sync::mpsc` + `CancellationToken` instead of `EventSender` |

### Unchanged files
| File | LOC |
|------|-----|
| `crates/runtime/src/modules.rs` | 355 |
| `crates/runtime/src/cpu_timer.rs` | 322 |
| `crates/runtime/src/storage.rs` | 311 |
| `crates/runtime/src/ops.rs` | 53 |
| `crates/runtime/src/embed/*.js` | ~1,170 |

---

## Task 1: Create `state.rs` — RuntimeState and event types

**Files:**
- Create: `crates/runtime/src/state.rs`

This is the foundation that all other files depend on. It replaces `event_loop.rs`'s `EventLoopInner` and `LoopEvent` with the new types.

- [ ] **Step 1: Create `state.rs` with RuntimeState, event types, and constants**

```rust
// crates/runtime/src/state.rs
//! Runtime shared state and event types.
//!
//! RuntimeState is shared with V8 callbacks via Rc<RefCell<>> in the isolate slot.
//! All borrows are brief (microseconds), never held across await points.

use std::cell::RefCell;
use std::collections::HashMap;
use std::pin::Pin;
use std::rc::Rc;
use std::time::{Duration, Instant};

use futures::Future;
use tokio_util::sync::CancellationToken;

use crate::timers::TimerCallback;

/// Channel capacity for incoming requests from the server.
pub const REQUEST_CHANNEL_CAPACITY: usize = 256;

/// Maximum pending async ops (fetch, crypto, etc.).
pub const MAX_PENDING_OPS: usize = 1024;

/// Maximum pending timers.
pub const MAX_PENDING_TIMERS: usize = 256;

/// Shared state type alias — the single shared state pattern.
pub(crate) type SharedState = Rc<RefCell<RuntimeState>>;

/// State shared between V8 callbacks and the Runtime.
///
/// V8 callbacks access this via `scope.get_slot::<SharedState>()`.
/// The Runtime accesses it between V8 entries (never during).
#[allow(missing_debug_implementations)]
pub(crate) struct RuntimeState {
    // --- Promise resolvers for async ops ---
    pub(crate) pending_resolvers: HashMap<u32, v8::Global<v8::PromiseResolver>>,
    pub(crate) next_op_id: u32,

    // --- Timer callback storage (callbacks only; scheduling is tokio) ---
    pub(crate) timer_callbacks: HashMap<u32, TimerCallback>,
    pub(crate) next_timer_id: u32,

    // --- Timer ownership: timer_id → request_id (for CPU attribution) ---
    pub(crate) timer_owner: HashMap<u32, u64>,

    // --- Streams ---
    pub(crate) streams: HashMap<u32, StreamState>,
    pub(crate) next_stream_id: u32,

    // --- Task buffer: V8 callbacks push futures here ---
    // After each enter_v8(), the main loop drains these into FuturesUnordered.
    pub(crate) spawned_ops: Vec<Pin<Box<dyn Future<Output = OpResult>>>>,
    pub(crate) spawned_timers: Vec<SpawnedTimer>,

    // --- Per-request log routing ---
    pub(crate) executing_request_id: Option<u64>,
    pub(crate) executing_request_cancel: Option<CancellationToken>,
    pub(crate) per_request_logs: HashMap<u64, Vec<String>>,

    // --- App state (shared across requests — intentional) ---
    pub(crate) kv_store: HashMap<String, String>,
    pub(crate) env_vars: HashMap<String, String>,
    pub(crate) key_store: HashMap<u32, crate::crypto::KeyData>,
    pub(crate) next_key_id: u32,
}

/// Stream state for ReadableStream instances.
pub(crate) struct StreamState {
    pub(crate) pending_read: Option<v8::Global<v8::PromiseResolver>>,
    pub(crate) buffer: Vec<Vec<u8>>,
    pub(crate) closed: bool,
}

/// A timer to be spawned by the main loop.
pub(crate) struct SpawnedTimer {
    pub(crate) id: u32,
    pub(crate) delay: Duration,
    pub(crate) interval: Option<Duration>,
}

/// Result of an async operation (fetch, crypto, etc.).
pub enum OpResult {
    /// Op completed with a JSON string value.
    Completed {
        op_id: u32,
        value: String,
        /// Which request spawned this op (for CPU attribution).
        request_id: Option<u64>,
    },
    /// Stream chunk (fetch streaming, WebSocket, etc.).
    StreamChunk {
        stream_id: u32,
        data: Vec<u8>,
        done: bool,
    },
    /// Op was cancelled (client disconnect).
    Cancelled,
}

/// Result of a timer firing.
pub(crate) struct TimerResult {
    pub(crate) id: u32,
    /// If Some, this is a setInterval — re-register after firing.
    pub(crate) interval: Option<Duration>,
}

/// Result of dispatching a request into V8.
pub(crate) enum DispatchResult {
    /// Handler returned a value synchronously.
    Sync(String),
    /// Handler returned a Promise (async).
    Async(v8::Global<v8::Promise>),
    /// Dispatch failed.
    Error(String),
}

/// Incoming request from the server (crosses thread boundary).
pub struct IncomingRequest {
    pub id: u64,
    pub body: String,
    pub reply: tokio::sync::oneshot::Sender<Result<crate::init::RequestResult, String>>,
    pub cancel: CancellationToken,
}

impl RuntimeState {
    pub(crate) fn new(env_vars: HashMap<String, String>) -> Self {
        Self {
            pending_resolvers: HashMap::new(),
            next_op_id: 1,
            timer_callbacks: HashMap::new(),
            next_timer_id: 1,
            timer_owner: HashMap::new(),
            streams: HashMap::new(),
            next_stream_id: 1,
            spawned_ops: Vec::new(),
            spawned_timers: Vec::new(),
            executing_request_id: None,
            executing_request_cancel: None,
            per_request_logs: HashMap::new(),
            kv_store: HashMap::new(),
            env_vars,
            key_store: HashMap::new(),
            next_key_id: 1,
        }
    }
}
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo check -p appbase-runtime 2>&1 | head -20`
Expected: warnings about unused imports, but no errors (the module isn't wired into lib.rs yet, so we just check syntax)

Actually, we need to wire it in minimally:

- [ ] **Step 3: Add module declaration to lib.rs (alongside existing modules)**

Add `pub mod state;` to `crates/runtime/src/lib.rs` after line 36 (after `mod url;`):

```rust
pub mod state;
```

- [ ] **Step 4: Verify compilation**

Run: `cargo check -p appbase-runtime 2>&1 | tail -5`
Expected: Compiles (possibly with warnings about dead code — that's fine, consumers come later)

- [ ] **Step 5: Commit**

```bash
git add crates/runtime/src/state.rs crates/runtime/src/lib.rs
git commit -m "feat(runtime): add state.rs — RuntimeState, OpResult, IncomingRequest types"
```

---

## Task 2: Create `request.rs` — free functions for V8 dispatch and resolution

**Files:**
- Create: `crates/runtime/src/request.rs`
- Modify: `crates/runtime/src/lib.rs` (add module declaration)

These are free functions that take `(scope, state, ...)` as separate parameters to avoid borrow conflicts when the V8 scope is alive. They are the workhorses called from `enter_v8` callbacks.

- [ ] **Step 1: Create `request.rs` with dispatch and resolution functions**

```rust
// crates/runtime/src/request.rs
//! Free functions for V8 request dispatch and promise resolution.
//!
//! These are free functions (not methods) to avoid borrow conflicts:
//! when a V8 scope borrows &mut isolate, self methods can't access
//! other fields. Free functions take scope and state as separate params.

use crate::state::{SharedState, DispatchResult};

/// Dispatch a JSON-RPC request body into V8.
/// Calls the compiled dispatch function and returns sync/async/error.
pub(crate) fn dispatch_request(
    scope: &mut v8::ContextScope<v8::HandleScope>,
    state: &SharedState,
    dispatch_fn: &v8::Global<v8::Function>,
    body: &str,
) -> DispatchResult {
    let func = v8::Local::new(scope, dispatch_fn);
    let arg = match v8::String::new(scope, body) {
        Some(s) => s,
        None => return DispatchResult::Error("Request body too large for V8".into()),
    };
    let undefined = v8::undefined(scope).into();
    let result = func.call(scope, undefined, &[arg.into()]);

    scope.perform_microtask_checkpoint();

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
                    DispatchResult::Sync(json)
                }
                v8::PromiseState::Rejected => {
                    let msg = promise
                        .result(scope)
                        .to_string(scope)
                        .unwrap()
                        .to_rust_string_lossy(scope);
                    DispatchResult::Error(msg)
                }
                v8::PromiseState::Pending => {
                    let global_promise = v8::Global::new(scope, promise);
                    DispatchResult::Async(global_promise)
                }
            }
        }
        Some(val) => {
            let json = val
                .to_string(scope)
                .unwrap()
                .to_rust_string_lossy(scope);
            DispatchResult::Sync(json)
        }
        None => DispatchResult::Error("JS exception during dispatch".into()),
    }
}

/// Resolve an async op: look up the promise resolver by op_id and resolve it.
pub(crate) fn resolve_op(
    scope: &mut v8::ContextScope<v8::HandleScope>,
    state: &SharedState,
    op_id: u32,
    value: &str,
) {
    let resolver = state.borrow_mut().pending_resolvers.remove(&op_id);
    if let Some(resolver) = resolver {
        let r = v8::Local::new(scope, &resolver);
        let val = match v8::String::new(scope, value) {
            Some(s) => s,
            None => {
                let err_msg = v8::String::new(scope, "Op result too large for V8").unwrap();
                let exc = v8::Exception::error(scope, err_msg);
                r.reject(scope, exc);
                return;
            }
        };
        r.resolve(scope, val.into());
    }
    scope.perform_microtask_checkpoint();
}

/// Fire a timer callback by timer_id.
pub(crate) fn fire_timer_callback(
    scope: &mut v8::ContextScope<v8::HandleScope>,
    state: &SharedState,
    timer_id: u32,
) {
    // Take the callback out. For setTimeout, it's consumed.
    // For setInterval, it will be re-inserted after firing.
    let cb = state.borrow_mut().timer_callbacks.remove(&timer_id);
    if let Some(cb) = cb {
        let func = v8::Local::new(scope, &cb.callback);
        let undefined = v8::undefined(scope).into();
        func.call(scope, undefined, &[]);
        scope.perform_microtask_checkpoint();

        // Re-insert for setInterval
        if cb.interval.is_some() {
            state.borrow_mut().timer_callbacks.insert(timer_id, cb);
        }
    }
}

/// Extract the result from a settled promise.
pub(crate) fn extract_promise_result(
    scope: &mut v8::ContextScope<v8::HandleScope>,
    promise: &v8::Global<v8::Promise>,
) -> Result<String, String> {
    let p = v8::Local::new(scope, promise);
    match p.state() {
        v8::PromiseState::Fulfilled => {
            let val = p.result(scope)
                .to_string(scope)
                .unwrap()
                .to_rust_string_lossy(scope);
            Ok(val)
        }
        v8::PromiseState::Rejected => {
            let val = p.result(scope)
                .to_string(scope)
                .unwrap()
                .to_rust_string_lossy(scope);
            Err(val)
        }
        v8::PromiseState::Pending => {
            Err("Promise still pending".into())
        }
    }
}
```

- [ ] **Step 2: Add module declaration to lib.rs**

Add `mod request;` to `crates/runtime/src/lib.rs`:

```rust
mod request;
```

- [ ] **Step 3: Verify compilation**

Run: `cargo check -p appbase-runtime 2>&1 | tail -5`
Expected: Compiles with dead code warnings

- [ ] **Step 4: Commit**

```bash
git add crates/runtime/src/request.rs crates/runtime/src/lib.rs
git commit -m "feat(runtime): add request.rs — dispatch, resolve, timer free functions"
```

---

## Task 3: Create `runtime.rs` — the Runtime struct and select! loop

**Files:**
- Create: `crates/runtime/src/runtime.rs`
- Modify: `crates/runtime/src/lib.rs` (add module declaration + re-export)

This is the core of the redesign. The `Runtime` struct owns the V8 isolate, and `run()` is the tokio-driven select! loop.

- [ ] **Step 1: Create `runtime.rs` with Runtime struct, enter_v8, and the select! loop**

```rust
// crates/runtime/src/runtime.rs
//! Inverted-control-flow Runtime — tokio drives, V8 is entered as callback.
//!
//! Modeled after Cloudflare workerd's IoContext::run() pattern:
//! each event (request, fetch completion, timer) independently enters V8,
//! runs JS + microtasks, and exits. There is no tick loop.

use std::collections::HashMap;
use std::pin::Pin;
use std::time::{Duration, Instant};

use futures::stream::FuturesUnordered;
use futures::{Future, StreamExt};
use tokio_util::sync::CancellationToken;

use crate::init::{init_v8, load_polyfills_and_modules, thread_cpu_time, RequestResult};
use crate::modules::ModuleEntry;
use crate::request;
use crate::state::{
    DispatchResult, IncomingRequest, OpResult, RuntimeState, SharedState, SpawnedTimer,
    TimerResult, REQUEST_CHANNEL_CAPACITY,
};

/// Per-request tracking for async handlers (returned a Promise).
struct PendingRequest {
    id: u64,
    promise: v8::Global<v8::Promise>,
    reply: tokio::sync::oneshot::Sender<Result<RequestResult, String>>,
    cpu_accumulated: Duration,
    wall_start: Instant,
    cancel: CancellationToken,
}

/// The Runtime — owns the V8 isolate, driven by tokio select! loop.
pub struct Runtime {
    // V8 state
    isolate: v8::OwnedIsolate,
    context: v8::Global<v8::Context>,
    dispatch_fn: Option<v8::Global<v8::Function>>,
    initialized: bool,
    modules: Vec<ModuleEntry>,

    // Shared with V8 callbacks
    state: SharedState,

    // Request tracking
    pending_requests: HashMap<u64, PendingRequest>,

    // I/O futures (equivalent of workerd's IoContext task set)
    pending_ops: FuturesUnordered<Pin<Box<dyn Future<Output = OpResult>>>>,
    pending_timers: FuturesUnordered<Pin<Box<dyn Future<Output = TimerResult>>>>,

    // Stream forwarders (pass-through streaming without V8)
    stream_forwarders: HashMap<u32, tokio::sync::mpsc::Sender<Vec<u8>>>,

    // Incoming requests from server
    request_rx: tokio::sync::mpsc::Receiver<IncomingRequest>,

    // Lifecycle
    shutdown: CancellationToken,

    // CPU enforcement
    #[cfg(target_os = "linux")]
    cpu_timer: Option<crate::cpu_timer::CpuTimer>,
    #[cfg(target_os = "linux")]
    cpu_timer_active: bool,
    cpu_limit: Option<Duration>,
}

/// Single entry point for all V8 access.
/// Equivalent of workerd's context.run() → Worker::Lock → execute → runMicrotasks().
fn enter_v8<F, R>(
    isolate: &mut v8::OwnedIsolate,
    context: &v8::Global<v8::Context>,
    f: F,
) -> R
where
    F: FnOnce(&mut v8::ContextScope<v8::HandleScope>) -> R,
{
    let scope = &mut v8::HandleScope::new(isolate);
    let ctx = v8::Local::new(scope, context);
    let scope = &mut v8::ContextScope::new(scope, ctx);
    let result = f(scope);
    scope.perform_microtask_checkpoint();
    result
}

impl Runtime {
    /// Create a new Runtime. Must be called from the isolate's dedicated thread.
    pub fn new(
        modules: Vec<ModuleEntry>,
        request_rx: tokio::sync::mpsc::Receiver<IncomingRequest>,
        shutdown: CancellationToken,
        cpu_limit: Option<Duration>,
        env_vars: HashMap<String, String>,
    ) -> Self {
        init_v8();

        let params = v8::CreateParams::default().heap_limits(0, 128 * 1024 * 1024);
        let mut isolate = v8::Isolate::new(params);

        // Near-heap-limit callback
        unsafe extern "C" fn near_heap_limit(
            _data: *mut std::ffi::c_void,
            current: usize,
            _initial: usize,
        ) -> usize {
            eprintln!("[v8] Near heap limit: {}MB, not increasing", current / 1024 / 1024);
            current
        }
        isolate.add_near_heap_limit_callback(near_heap_limit, std::ptr::null_mut());

        let rs = RuntimeState::new(env_vars);
        let state: SharedState = std::rc::Rc::new(std::cell::RefCell::new(rs));
        isolate.set_slot(state.clone());

        let context = {
            v8::scope!(let hs, &mut isolate);
            let ctx = v8::Context::new(hs, Default::default());
            v8::Global::new(hs, ctx)
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

    /// Initialize: load polyfills + modules + compile dispatch function.
    fn ensure_initialized(&mut self) {
        if self.initialized {
            return;
        }
        let modules = self.modules.clone();
        let dispatch_fn = enter_v8(&mut self.isolate, &self.context, |scope| {
            load_polyfills_and_modules(scope, &modules)
        });
        self.dispatch_fn = Some(dispatch_fn);
        self.initialized = true;

        // CPU timer (Linux)
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

    /// Run the event loop. tokio drives everything.
    pub async fn run(&mut self) {
        self.ensure_initialized();

        loop {
            self.collect_new_tasks();

            // Check CPU termination from previous enter_v8
            if self.isolate.is_execution_terminating() {
                self.isolate.cancel_terminate_execution();
                for (_, req) in self.pending_requests.drain() {
                    let _ = req.reply.send(Err("CPU time limit exceeded".into()));
                }
                break;
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

                else => break,
            }
        }
    }

    /// Drain futures spawned by JS into FuturesUnordered.
    fn collect_new_tasks(&mut self) {
        let mut s = self.state.borrow_mut();
        for op in s.spawned_ops.drain(..) {
            self.pending_ops.push(op);
        }
        for t in s.spawned_timers.drain(..) {
            let SpawnedTimer { id, delay, interval } = t;
            self.pending_timers.push(Box::pin(async move {
                tokio::time::sleep(delay).await;
                TimerResult { id, interval }
            }));
        }
    }

    fn handle_incoming_request(&mut self, req: IncomingRequest) {
        let req_id = req.id;

        {
            let mut s = self.state.borrow_mut();
            s.executing_request_id = Some(req_id);
            s.executing_request_cancel = Some(req.cancel.clone());
        }

        let cpu_before = thread_cpu_time();
        self.arm_cpu_timer();

        let dispatch_fn = self.dispatch_fn.as_ref().unwrap().clone();
        let result = enter_v8(&mut self.isolate, &self.context, |scope| {
            request::dispatch_request(scope, &self.state, &dispatch_fn, &req.body)
        });

        self.disarm_cpu_timer();
        let cpu_elapsed = thread_cpu_time().saturating_sub(cpu_before);

        {
            let mut s = self.state.borrow_mut();
            s.executing_request_id = None;
            s.executing_request_cancel = None;
        }

        match result {
            DispatchResult::Sync(json) => {
                let logs = self.drain_request_logs(req_id);
                let _ = req.reply.send(Ok(RequestResult {
                    json,
                    cpu_time: cpu_elapsed,
                    wall_time: cpu_elapsed, // sync: wall ≈ cpu
                    logs,
                }));
            }
            DispatchResult::Async(promise) => {
                self.pending_requests.insert(req_id, PendingRequest {
                    id: req_id,
                    promise,
                    reply: req.reply,
                    cpu_accumulated: cpu_elapsed,
                    wall_start: Instant::now(),
                    cancel: req.cancel,
                });
            }
            DispatchResult::Error(msg) => {
                let _ = req.reply.send(Err(msg));
            }
        }

        self.check_settled_promises_v8();
    }

    fn handle_op_result(&mut self, result: OpResult) {
        match result {
            OpResult::Completed { op_id, value, request_id } => {
                {
                    let mut s = self.state.borrow_mut();
                    s.executing_request_id = request_id;
                    // Look up cancel token for this request
                    s.executing_request_cancel = request_id
                        .and_then(|rid| self.pending_requests.get(&rid))
                        .map(|pr| pr.cancel.clone());
                }

                let cpu_before = thread_cpu_time();
                self.arm_cpu_timer();

                enter_v8(&mut self.isolate, &self.context, |scope| {
                    request::resolve_op(scope, &self.state, op_id, &value);
                });

                self.disarm_cpu_timer();
                let cpu_elapsed = thread_cpu_time().saturating_sub(cpu_before);

                {
                    let mut s = self.state.borrow_mut();
                    s.executing_request_id = None;
                    s.executing_request_cancel = None;
                }

                if let Some(req_id) = request_id {
                    if let Some(pending) = self.pending_requests.get_mut(&req_id) {
                        pending.cpu_accumulated += cpu_elapsed;
                    }
                }

                self.check_settled_promises_v8();
            }
            OpResult::StreamChunk { stream_id, data, done } => {
                if let Some(fwd) = self.stream_forwarders.get(&stream_id) {
                    let _ = fwd.try_send(data);
                    if done {
                        self.stream_forwarders.remove(&stream_id);
                    }
                } else {
                    enter_v8(&mut self.isolate, &self.context, |scope| {
                        crate::streams::push_stream_chunk(scope, &self.state, stream_id, &data, done);
                    });
                }
            }
            OpResult::Cancelled => {}
        }
    }

    fn handle_timer(&mut self, result: TimerResult) {
        let timer_id = result.id;

        let request_id = self.state.borrow().timer_owner.get(&timer_id).copied();

        {
            let mut s = self.state.borrow_mut();
            s.executing_request_id = request_id;
            s.executing_request_cancel = request_id
                .and_then(|rid| self.pending_requests.get(&rid))
                .map(|pr| pr.cancel.clone());
        }

        let cpu_before = thread_cpu_time();
        self.arm_cpu_timer();

        enter_v8(&mut self.isolate, &self.context, |scope| {
            request::fire_timer_callback(scope, &self.state, timer_id);
        });

        self.disarm_cpu_timer();
        let cpu_elapsed = thread_cpu_time().saturating_sub(cpu_before);

        {
            let mut s = self.state.borrow_mut();
            s.executing_request_id = None;
            s.executing_request_cancel = None;
        }

        if let Some(req_id) = request_id {
            if let Some(pending) = self.pending_requests.get_mut(&req_id) {
                pending.cpu_accumulated += cpu_elapsed;
            }
        }

        // Re-register interval timers
        if let Some(interval) = result.interval {
            self.pending_timers.push(Box::pin(async move {
                tokio::time::sleep(interval).await;
                TimerResult { id: timer_id, interval: Some(interval) }
            }));
        } else {
            // One-shot: clean up timer owner tracking
            self.state.borrow_mut().timer_owner.remove(&timer_id);
        }

        self.check_settled_promises_v8();
    }

    fn check_settled_promises_v8(&mut self) {
        let settled: Vec<u64> = enter_v8(&mut self.isolate, &self.context, |scope| {
            self.pending_requests
                .iter()
                .filter(|(_, req)| {
                    let p = v8::Local::new(scope, &req.promise);
                    p.state() != v8::PromiseState::Pending
                })
                .map(|(id, _)| *id)
                .collect()
        });

        if settled.is_empty() {
            return;
        }

        for id in settled {
            if let Some(req) = self.pending_requests.remove(&id) {
                let result = enter_v8(&mut self.isolate, &self.context, |scope| {
                    request::extract_promise_result(scope, &req.promise)
                });
                let logs = self.drain_request_logs(id);
                let _ = req.reply.send(result.map(|json| RequestResult {
                    json,
                    cpu_time: req.cpu_accumulated,
                    wall_time: req.wall_start.elapsed(),
                    logs,
                }));
            }
        }
    }

    fn drain_request_logs(&self, request_id: u64) -> Vec<String> {
        self.state
            .borrow_mut()
            .per_request_logs
            .remove(&request_id)
            .unwrap_or_default()
    }

    fn graceful_shutdown(&mut self) {
        // Cancel all in-flight background tasks
        for (_, req) in &self.pending_requests {
            req.cancel.cancel();
        }

        // Close the request channel to stop new requests
        self.request_rx.close();

        // Error remaining requests
        for (id, req) in self.pending_requests.drain() {
            let _ = req.reply.send(Err(format!("shutdown: request {id} aborted")));
        }
    }
}
```

- [ ] **Step 2: Add module declaration and re-exports to lib.rs**

Add to `crates/runtime/src/lib.rs`:

```rust
pub mod runtime;
pub use runtime::Runtime;
pub use state::IncomingRequest;
```

- [ ] **Step 3: Verify compilation**

Run: `cargo check -p appbase-runtime 2>&1 | tail -10`
Expected: May have errors about `push_stream_chunk` signature mismatch with new `SharedState` — we fix those in Task 5. For now, verify the core structure compiles or identify which imports need updating.

- [ ] **Step 4: Commit (even if there are minor compilation issues to fix in later tasks)**

```bash
git add crates/runtime/src/runtime.rs crates/runtime/src/lib.rs
git commit -m "feat(runtime): add runtime.rs — Runtime struct with tokio select! loop"
```

---

## Task 4: Update `fetch.rs` — push future into spawned_ops

**Files:**
- Modify: `crates/runtime/src/fetch.rs`

The key change: instead of `handle.spawn(task)` + channel send + `waker.wake()`, the V8 callback pushes a future into `state.spawned_ops`. The `do_fetch_streaming` logic stays mostly the same.

- [ ] **Step 1: Update `raw_fetch_callback` to use spawned_ops**

Replace the current fetch callback to push into `spawned_ops` instead of spawning on tokio handle. The `do_fetch_streaming` function stays as an async function, but returns `OpResult` instead of sending through a channel.

Key changes in `fetch.rs`:
1. Remove `event_tx` and `waker` from the captured state
2. Remove `tokio_handle.spawn(task)` — push future into `state.spawned_ops`
3. The async future captures `cancel: Option<CancellationToken>` from `state.executing_request_cancel`
4. The future returns `OpResult::Completed` or `OpResult::StreamChunk`
5. For streaming: the future produces multiple `OpResult`s — use a channel or produce a stream

**Important design note for streaming:** A single future in `FuturesUnordered` produces one `OpResult`. For streaming (multiple chunks), we need a different approach: the fetch future pushes chunks into a local `tokio::sync::mpsc` channel, and a receiver future in `pending_ops` forwards them as `OpResult::StreamChunk`. Or simpler: for the initial implementation, keep the buffered path only (read full body) and add streaming in a follow-up.

For the initial implementation, keep the **buffered path** (read full body, return as `OpResult::Completed`). Streaming fetch bodies will be added in a follow-up task.

```rust
// Updated raw_fetch_callback — key changes only (full file too large for plan)
// Changes from current fetch.rs:

// 1. Capture from state:
let (op_id, request_id, cancel) = {
    let mut s = state.borrow_mut();
    let id = s.next_op_id;
    s.next_op_id += 1;
    s.pending_resolvers.insert(id, global_resolver);
    let req_id = s.executing_request_id;
    let cancel = s.executing_request_cancel.clone();
    (id, req_id, cancel)
};

// 2. Push future instead of spawning:
state.borrow_mut().spawned_ops.push(Box::pin(async move {
    let fetch_result = if let Some(cancel) = cancel {
        tokio::select! {
            r = do_fetch_buffered(&method, &url, &headers_json, body.as_deref()) => r,
            _ = cancel.cancelled() => Err(error_json("request cancelled")),
        }
    } else {
        do_fetch_buffered(&method, &url, &headers_json, body.as_deref()).await
    };
    match fetch_result {
        Ok(value) => OpResult::Completed { op_id, value, request_id },
        Err(err) => OpResult::Completed { op_id, value: err, request_id },
    }
}));

// 3. Remove: event_tx.send(), waker.wake(), tokio_handle.spawn()
// 4. Remove: do_fetch_streaming — replace with do_fetch_buffered for now
```

- [ ] **Step 2: Verify compilation**

Run: `cargo check -p appbase-runtime 2>&1 | tail -10`

- [ ] **Step 3: Commit**

```bash
git add crates/runtime/src/fetch.rs
git commit -m "refactor(fetch): push future into spawned_ops instead of channel"
```

---

## Task 5: Update `timers.rs` — push into spawned_timers

**Files:**
- Modify: `crates/runtime/src/timers.rs`
- Modify: `crates/runtime/src/init.rs` (update timer callback registration to use new state)

The timer min-heap and `fire_ready_timers()` are removed. Timer callbacks are stored in `RuntimeState.timer_callbacks`. When JS calls `setTimeout`, the callback pushes a `SpawnedTimer` into the buffer. The `select!` loop creates `tokio::time::sleep` futures.

- [ ] **Step 1: Simplify timers.rs**

Replace the current heap-based timer system. Keep `TimerCallback` struct. Remove `TimerState`, `TimerHeapEntry`, `fire_ready_timers()`.

```rust
// crates/runtime/src/timers.rs
//! Timer system — callbacks stored in RuntimeState, scheduling via tokio::time::sleep.

use std::time::Duration;

/// Backing storage for a timer's callback.
#[allow(missing_debug_implementations)]
pub(crate) struct TimerCallback {
    pub(crate) callback: v8::Global<v8::Function>,
    /// None = setTimeout (one-shot), Some(dur) = setInterval (repeating).
    pub(crate) interval: Option<Duration>,
}
```

- [ ] **Step 2: Update timer registration callbacks in init.rs**

The `set_timeout_callback` and `clear_timeout_callback` in `init.rs` (or wherever `setup_globals` registers them) need to push into `state.spawned_timers` and `state.timer_callbacks` instead of the heap.

The exact location depends on where `setup_globals` registers `setTimeout` — find it and update:

```rust
// In the setTimeout V8 callback:
let mut s = state.borrow_mut();
let id = s.next_timer_id;
s.next_timer_id += 1;
s.timer_callbacks.insert(id, TimerCallback { callback: global_callback, interval });
if let Some(req_id) = s.executing_request_id {
    s.timer_owner.insert(id, req_id);
}
s.spawned_timers.push(SpawnedTimer { id, delay, interval });

// In the clearTimeout V8 callback:
let mut s = state.borrow_mut();
s.timer_callbacks.remove(&timer_id);
s.timer_owner.remove(&timer_id);
// The tokio::time::sleep future will still fire but handle_timer()
// will find no callback and do nothing.
```

- [ ] **Step 3: Verify compilation**

Run: `cargo check -p appbase-runtime 2>&1 | tail -10`

- [ ] **Step 4: Commit**

```bash
git add crates/runtime/src/timers.rs crates/runtime/src/init.rs
git commit -m "refactor(timers): remove heap, push SpawnedTimer for tokio scheduling"
```

---

## Task 6: Update remaining modules to use new SharedState

**Files:**
- Modify: `crates/runtime/src/init.rs` — import from `state` instead of `event_loop`
- Modify: `crates/runtime/src/streams.rs` — import from `state`
- Modify: `crates/runtime/src/crypto.rs` — import from `state`
- Modify: `crates/runtime/src/kv.rs` — import from `state`
- Modify: `crates/runtime/src/env.rs` — import from `state`
- Modify: `crates/runtime/src/url.rs` — import from `state`

All these files import `use crate::event_loop::SharedState` or `use crate::event_loop::EventLoopInner`. They need to import from `crate::state` instead.

- [ ] **Step 1: Update imports in each file**

Search and replace across all files:

```
// BEFORE:
use crate::event_loop::SharedState;
use crate::event_loop::{SharedState, LoopEvent, StreamState};
use crate::event_loop::EventLoopInner;

// AFTER:
use crate::state::SharedState;
use crate::state::{SharedState, StreamState};
use crate::state::RuntimeState;
```

Field name changes:
- `s.event_tx` → removed (no channel in RuntimeState)
- `s.waker` → removed (no AtomicWaker)
- `s.tokio_handle` → removed (no handle needed, futures are local)
- `s.timers.next_id` → `s.next_timer_id`
- `s.timers.callbacks` → `s.timer_callbacks`
- `s.timers.heap` → removed
- `s.log_buffer` → `s.per_request_logs` (via `executing_request_id`)

The `console_log` callback in `init.rs` needs to use per-request logs:

```rust
// BEFORE:
s.log_buffer.push(msg);

// AFTER:
let req_id = s.executing_request_id.unwrap_or(0);
s.per_request_logs.entry(req_id).or_default().push(msg);
```

- [ ] **Step 2: Verify compilation of the full runtime crate**

Run: `cargo check -p appbase-runtime 2>&1 | tail -20`
Expected: Should compile. If there are errors, fix them — they'll be about renamed fields or missing imports.

- [ ] **Step 3: Commit**

```bash
git add crates/runtime/src/init.rs crates/runtime/src/streams.rs crates/runtime/src/crypto.rs crates/runtime/src/kv.rs crates/runtime/src/env.rs crates/runtime/src/url.rs
git commit -m "refactor: update all modules to use state::SharedState"
```

---

## Task 7: Delete old files and update lib.rs

**Files:**
- Delete: `crates/runtime/src/concurrent.rs`
- Delete: `crates/runtime/src/event_loop.rs`
- Modify: `crates/runtime/src/lib.rs` — remove old module declarations, keep `Isolate`/`IsolatePool` for now

- [ ] **Step 1: Remove old module declarations from lib.rs**

Remove these lines from lib.rs:
```rust
pub mod concurrent;
mod event_loop;
```

Keep `mod isolate;` — the per-request `Isolate` is still used by existing tests. It can be migrated to use the new Runtime later, but removing it now would break 50+ tests.

- [ ] **Step 2: Delete the old files**

```bash
git rm crates/runtime/src/concurrent.rs crates/runtime/src/event_loop.rs
```

- [ ] **Step 3: Verify compilation**

Run: `cargo check -p appbase-runtime 2>&1 | tail -20`
Expected: Errors about `isolate.rs` still importing from `event_loop`. Fix: update `isolate.rs` to import from `state` as well.

- [ ] **Step 4: Update isolate.rs imports**

The `Isolate` (per-request model) uses `EventLoopInner`. For now, make it import from `state::RuntimeState` and adapt its `new()` / `execute_request()` to use the new types. The per-request `Isolate` doesn't use the select! loop — it keeps its blocking `run_event_loop` but uses `RuntimeState` for shared state.

Alternatively, if `Isolate` is only used in tests and the benchmark, we can make it a thin wrapper around `Runtime` with a blocking `execute_request` that sends to the channel and blocks on the reply. This is cleaner but more work.

**Decision for this task:** Keep `Isolate` working with the new `RuntimeState` types. Minimal changes — just update imports and field names. Full migration to `Runtime`-based Isolate is a follow-up.

- [ ] **Step 5: Verify compilation**

Run: `cargo check -p appbase-runtime 2>&1 | tail -10`
Expected: Clean compilation

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "refactor: delete concurrent.rs + event_loop.rs, update isolate.rs imports"
```

---

## Task 8: Write tests for the new Runtime

**Files:**
- Modify: `crates/runtime/src/runtime.rs` (add `#[cfg(test)] mod tests`)

Port the 6 concurrent tests from the deleted `concurrent.rs` to use the new `Runtime` API.

- [ ] **Step 1: Write tests using the new Runtime API**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tokio_util::sync::CancellationToken;

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
"#.into(),
        }]
    }

    fn rpc(method: &str, params: &str) -> String {
        format!(r#"{{"jsonrpc":"2.0","method":"{method}","params":{params},"id":1}}"#)
    }

    /// Helper: spawn a Runtime on a dedicated thread, return request sender + shutdown token.
    fn spawn_runtime(
        modules: Vec<ModuleEntry>,
    ) -> (tokio::sync::mpsc::Sender<IncomingRequest>, CancellationToken, std::thread::JoinHandle<()>) {
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
                    let mut runtime = Runtime::new(
                        modules, rx, shutdown_inner, None, HashMap::new(),
                    );
                    runtime.run().await;
                });
            })
            .unwrap();

        (tx, shutdown, handle)
    }

    #[test]
    fn runtime_sync_request() {
        let (tx, shutdown, handle) = spawn_runtime(test_modules());
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        tx.blocking_send(IncomingRequest {
            id: 1,
            body: rpc("ping", "[]"),
            reply: reply_tx,
            cancel: CancellationToken::new(),
        }).unwrap();

        let result = reply_rx.blocking_recv().unwrap().unwrap();
        assert!(result.json.contains("pong"), "got: {}", result.json);

        shutdown.cancel();
        handle.join().unwrap();
    }

    #[test]
    fn runtime_async_request() {
        let (tx, shutdown, handle) = spawn_runtime(test_modules());
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        tx.blocking_send(IncomingRequest {
            id: 1,
            body: rpc("delayed", "[10]"),
            reply: reply_tx,
            cancel: CancellationToken::new(),
        }).unwrap();

        let result = reply_rx.blocking_recv().unwrap().unwrap();
        assert!(result.json.contains("done_10"), "got: {}", result.json);
        assert!(result.wall_time >= Duration::from_millis(5));

        shutdown.cancel();
        handle.join().unwrap();
    }

    #[test]
    fn runtime_concurrent_overlap() {
        let (tx, shutdown, handle) = spawn_runtime(test_modules());
        let wall_start = Instant::now();
        let mut receivers = Vec::new();

        for i in 0..3 {
            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            tx.blocking_send(IncomingRequest {
                id: 100 + i,
                body: rpc("delayed", "[20]"),
                reply: reply_tx,
                cancel: CancellationToken::new(),
            }).unwrap();
            receivers.push(reply_rx);
        }

        for rx in receivers {
            let result = rx.blocking_recv().unwrap().unwrap();
            assert!(result.json.contains("done_20"));
        }

        let total = wall_start.elapsed();
        assert!(total < Duration::from_millis(500), "took {:?}", total);

        shutdown.cancel();
        handle.join().unwrap();
    }

    #[test]
    fn runtime_mixed_sync_async() {
        let (tx, shutdown, handle) = spawn_runtime(test_modules());

        let (tx1, rx1) = tokio::sync::oneshot::channel();
        tx.blocking_send(IncomingRequest {
            id: 1, body: rpc("add", "[3, 4]"),
            reply: tx1, cancel: CancellationToken::new(),
        }).unwrap();

        let (tx2, rx2) = tokio::sync::oneshot::channel();
        tx.blocking_send(IncomingRequest {
            id: 2, body: rpc("delayed", "[10]"),
            reply: tx2, cancel: CancellationToken::new(),
        }).unwrap();

        let (tx3, rx3) = tokio::sync::oneshot::channel();
        tx.blocking_send(IncomingRequest {
            id: 3, body: rpc("ping", "[]"),
            reply: tx3, cancel: CancellationToken::new(),
        }).unwrap();

        assert!(rx1.blocking_recv().unwrap().unwrap().json.contains("\"result\":7"));
        assert!(rx2.blocking_recv().unwrap().unwrap().json.contains("done_10"));
        assert!(rx3.blocking_recv().unwrap().unwrap().json.contains("pong"));

        shutdown.cancel();
        handle.join().unwrap();
    }

    #[test]
    fn runtime_promise_chain() {
        let (tx, shutdown, handle) = spawn_runtime(test_modules());
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        tx.blocking_send(IncomingRequest {
            id: 1, body: rpc("chain", "[]"),
            reply: reply_tx, cancel: CancellationToken::new(),
        }).unwrap();

        let result = reply_rx.blocking_recv().unwrap().unwrap();
        assert!(result.json.contains("\"result\":22"), "got: {}", result.json);

        shutdown.cancel();
        handle.join().unwrap();
    }

    #[test]
    fn runtime_shutdown() {
        let (tx, shutdown, handle) = spawn_runtime(test_modules());

        // Send a long-running request
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        tx.blocking_send(IncomingRequest {
            id: 1, body: rpc("delayed", "[5000]"),
            reply: reply_tx, cancel: CancellationToken::new(),
        }).unwrap();

        // Wait for it to be dispatched
        std::thread::sleep(Duration::from_millis(50));

        // Shutdown
        shutdown.cancel();
        handle.join().unwrap();

        // The reply should be an error (shutdown aborted)
        let result = reply_rx.blocking_recv().unwrap();
        assert!(result.is_err());
    }
}
```

- [ ] **Step 2: Run new tests**

Run: `cargo test -p appbase-runtime runtime::tests -- --nocapture 2>&1 | tail -20`
Expected: All 6 tests pass

- [ ] **Step 3: Commit**

```bash
git add crates/runtime/src/runtime.rs
git commit -m "test(runtime): add tests for Runtime — sync, async, concurrent, shutdown"
```

---

## Task 9: Run all existing tests — verify zero regression

**Files:** None (verification only)

- [ ] **Step 1: Run the full runtime test suite**

Run: `cargo test -p appbase-runtime 2>&1 | tail -20`
Expected: All tests pass. The `lib.rs` tests use `Isolate` (per-request), which should still work since we kept `isolate.rs` and updated its imports.

- [ ] **Step 2: Run the integration tests**

Run: `cargo test -p appbase-runtime --test examples 2>&1 | tail -20`
Expected: All 29 example tests pass

- [ ] **Step 3: Run the full workspace**

Run: `cargo test --workspace 2>&1 | tail -20`
Expected: All tests pass. If platform tests fail (v8pool.rs), that's expected — Task 10 fixes it.

- [ ] **Step 4: Fix any failures, commit if all pass**

```bash
git add -A && git commit -m "fix: resolve test regressions from v3 migration"
```

---

## Task 10: Update V8Pool to use new Runtime API

**Files:**
- Modify: `crates/platform/src/server/v8pool.rs`

Replace `EventSender` + `std::sync::mpsc` with `tokio::sync::mpsc::Sender<IncomingRequest>` + `CancellationToken`.

- [ ] **Step 1: Update IsolateEntry and spawn_isolate**

```rust
// Key changes in v8pool.rs:

use appbase_runtime::{Runtime, IncomingRequest, init_v8};
use appbase_runtime::modules::ModuleEntry;
use tokio_util::sync::CancellationToken;

struct IsolateEntry {
    request_tx: tokio::sync::mpsc::Sender<IncomingRequest>,
    shutdown: CancellationToken,
    last_used_ms: AtomicU64,
    request_count: AtomicU64,
    logs: std::sync::Mutex<Vec<String>>,
}

fn spawn_isolate(&self, app_id: &str, server_js: &str) -> Result<IsolateEntry, String> {
    let (request_tx, request_rx) = tokio::sync::mpsc::channel(256);
    let shutdown = CancellationToken::new();
    let shutdown_inner = shutdown.clone();
    let modules = vec![ModuleEntry { specifier: "index.js".into(), source: server_js.into() }];
    let cpu_limit = self.config.cpu_limit();

    std::thread::Builder::new()
        .name(format!("v8-{app_id}"))
        .spawn(move || {
            let local_rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio runtime");
            local_rt.block_on(async {
                let mut runtime = Runtime::new(
                    modules, request_rx, shutdown_inner, cpu_limit,
                    std::collections::HashMap::new(),
                );
                runtime.run().await;
            });
        })
        .map_err(|e| format!("Failed to spawn: {e}"))?;

    // Warmup
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    request_tx.blocking_send(IncomingRequest {
        id: 0,
        body: r#"{"jsonrpc":"2.0","method":"__ping","params":[],"id":0}"#.into(),
        reply: reply_tx,
        cancel: CancellationToken::new(),
    }).map_err(|_| "Send failed")?;
    reply_rx.blocking_recv().map_err(|_| "Warmup failed")??;

    Ok(IsolateEntry {
        request_tx,
        shutdown,
        last_used_ms: AtomicU64::new(epoch_ms()),
        request_count: AtomicU64::new(0),
        logs: std::sync::Mutex::new(Vec::new()),
    })
}
```

- [ ] **Step 2: Update dispatch method**

```rust
pub async fn dispatch(&self, app_id: &str, server_js: &str, body: String) -> Result<RpcResult, String> {
    let entry = self.get_or_create(app_id, server_js)?;
    let id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();

    entry.request_tx.send(IncomingRequest {
        id, body, reply: reply_tx, cancel: CancellationToken::new(),
    }).await.map_err(|_| format!("Isolate for '{app_id}' is dead"))?;

    entry.request_count.fetch_add(1, Ordering::Relaxed);
    entry.last_used_ms.store(epoch_ms(), Ordering::Relaxed);

    match tokio::time::timeout(self.wall_timeout, reply_rx).await {
        Ok(Ok(Ok(result))) => {
            if !result.logs.is_empty() {
                let mut app_logs = entry.logs.lock().unwrap_or_else(|e| e.into_inner());
                app_logs.extend(result.logs.iter().cloned());
                if app_logs.len() > 100 { let drain = app_logs.len() - 100; app_logs.drain(..drain); }
            }
            Ok(RpcResult { json: result.json, cpu_time: result.cpu_time, logs: result.logs })
        }
        Ok(Ok(Err(e))) => Err(e),
        Ok(Err(_)) => Err("Worker dropped reply".into()),
        Err(_) => Err("Request timed out (30s)".into()),
    }
}
```

- [ ] **Step 3: Update eviction to use CancellationToken**

```rust
fn evict_lru(&self, isolates: &mut HashMap<String, Arc<IsolateEntry>>) {
    let oldest = isolates.iter()
        .min_by_key(|(_, e)| e.last_used_ms.load(Ordering::Relaxed))
        .map(|(id, _)| id.clone());
    if let Some(id) = oldest {
        if let Some(entry) = isolates.remove(&id) {
            entry.shutdown.cancel();
            eprintln!("[pool] Evicted '{id}' (LRU)");
        }
    }
}

pub fn evict_app(&self, app_id: &str) {
    let mut isolates = self.isolates.write().unwrap_or_else(|e| e.into_inner());
    if let Some(entry) = isolates.remove(app_id) {
        entry.shutdown.cancel();
        eprintln!("[pool] Evicted '{app_id}'");
    }
}
```

- [ ] **Step 4: Verify compilation and tests**

Run: `cargo check -p appbase-platform 2>&1 | tail -10`
Run: `cargo test --workspace 2>&1 | tail -20`

- [ ] **Step 5: Commit**

```bash
git add crates/platform/src/server/v8pool.rs
git commit -m "refactor(v8pool): use Runtime + IncomingRequest + CancellationToken"
```

---

## Task 11: Update benchmark server

**Files:**
- Modify: `crates/runtime/src/server.rs`

Update the benchmark `Dispatcher` to use `Runtime` instead of `ConcurrentIsolate`.

- [ ] **Step 1: Update Dispatcher to spawn Runtime per worker**

The benchmark server's `Dispatcher` currently uses `ConcurrentIsolate` with `spawn_concurrent_worker`. Update to spawn `Runtime` on dedicated threads with `tokio::sync::mpsc` channels.

```rust
// Key change: each worker is a thread with Runtime::run()
// Dispatch round-robins IncomingRequests across workers
```

- [ ] **Step 2: Run benchmark to verify no regression**

Run: `cargo bench -p appbase-runtime -- v8_qps 2>&1 | tail -20`
Expected: >=330K req/s for sync RPC

- [ ] **Step 3: Commit**

```bash
git add crates/runtime/src/server.rs
git commit -m "refactor(server): benchmark Dispatcher uses Runtime"
```

---

## Task 12: Final verification and cleanup

**Files:** Various

- [ ] **Step 1: Run full test suite**

Run: `cargo test --workspace 2>&1 | tail -20`
Expected: All tests pass

- [ ] **Step 2: Verify no old imports remain**

Run: `grep -r "event_loop::" crates/runtime/src/ --include="*.rs" | grep -v "//"`
Expected: No matches (all imports migrated to `state::`)

Run: `grep -r "EventSender\|concurrent::" crates/ --include="*.rs" | grep -v "//"`
Expected: No matches (old types removed)

- [ ] **Step 3: Run clippy**

Run: `cargo clippy -p appbase-runtime 2>&1 | tail -20`
Expected: No errors (warnings OK)

- [ ] **Step 4: Final commit**

```bash
git add -A && git commit -m "chore: v3 migration cleanup — remove dead imports and unused code"
```
