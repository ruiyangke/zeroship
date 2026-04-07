# Runtime v3 Design — Inverted Control Flow

**Date:** 2026-04-07
**Supersedes:** `2026-04-05-runtime-v2-design.md` (execution model only; v2's type decomposition along borrow boundaries is preserved)
**Goal:** Replace the hand-rolled tick/park event loop with tokio-driven inverted control flow, modeled after Cloudflare workerd's architecture.

## Why

The v1/v2 event loop is a hand-rolled `tick()` → `park_timeout()` loop on a dedicated V8 thread. Events from tokio cross a `std::sync::mpsc` channel, requiring a `Mutex`-wrapped thread handle for wake (`EventSender`). This creates:

1. **Two wake mechanisms** — `AtomicWaker` (unused in concurrent mode) and `EventSender` (Mutex + unpark). The `waker.wake()` calls in `fetch.rs` are dead code for `ConcurrentIsolate`.
2. **O(N) promise polling** — `check_settled_promises` scans all pending requests 3× per tick.
3. **Log attribution bug** — shared `log_buffer` drains all logs when any request completes, misattributing logs from concurrent requests.
4. **CPU time only captures initial dispatch** — async work across subsequent ticks is not accumulated.
5. **Warmup holds global write lock** — `spawn_isolate` blocks all V8Pool reads during isolate creation.
6. **Channel bridge overhead** — every I/O event crosses a thread boundary via mpsc + mutex + unpark (~1μs per event).

workerd solves all of these with a simpler model: the KJ event loop is the outer driver, and V8 is entered/exited as callbacks via `context.run()`. There is no tick loop. Each event (request arrival, fetch completion, timer fire) independently enters V8, runs JS + microtasks, and exits.

This design adapts that model to Rust/tokio.

## Architecture

### Design principle: tokio drives, V8 is entered as callback

The central change: instead of V8 owning a loop that pulls events from a channel, **tokio's event loop drives execution**. Each event source (requests, fetch completions, timers) is a branch in a `tokio::select!`. When an event fires, V8 is entered via `enter_v8()`, work is done, microtasks drain, V8 is exited. Tokio decides what runs next.

This is the direct equivalent of workerd's `IoContext::run()` → `takeAsyncLock()` → `Worker::Lock(v8::Locker)` → execute → `runMicrotasks()` → drop lock.

### Why inverted control flow

In the tick model, the V8 thread is a custom scheduler: drain events, classify them, process in phases, compute wait timeout, sleep. This reimplements what tokio already does — and worse, because:

- `compute_wait_timeout` is approximate (100ms fallback for "have async work but no timers")
- Event classification (requests vs I/O) adds latency
- The drain-all-then-process-all pattern delays processing of events that arrive during V8 execution
- Two wake mechanisms exist because the hand-rolled loop needs manual wake signaling

With inverted control flow, tokio handles all of this:
- No timeout computation — tokio wakes precisely when a future completes
- No event classification — each source is a separate `select!` branch
- No manual wake mechanism — tokio's waker system handles it
- Events that arrive during V8 execution are naturally queued by tokio

### Threading model

Each isolate runs on a **dedicated OS thread** with a **current-thread tokio runtime**. This preserves:

- `Rc<RefCell<>>` for `SharedState` (no Send/Sync rework, no atomic overhead)
- Single-threaded V8 execution (no `v8::Locker` needed)
- Cache locality (V8 heap + RuntimeState + I/O state on one core)

All I/O (fetch HTTP, timers, streams) runs on the same current-thread runtime. `reqwest` uses:
- Async non-blocking TCP via hyper (epoll-driven, same thread)
- Async TLS via tokio-rustls (non-blocking, same thread)
- DNS via `spawn_blocking` (separate blocking thread pool, works on current-thread runtime)

Nothing blocks the event loop. This matches workerd's single-threaded KJ event loop model.

### Core types

```rust
/// The Runtime — owns the V8 isolate, runs on a dedicated thread.
/// Equivalent of workerd's Worker + IoContext combined.
pub struct Runtime {
    // --- V8 state (exclusively borrowed when HandleScope exists) ---
    isolate: v8::OwnedIsolate,
    context: v8::Global<v8::Context>,
    dispatch_fn: Option<v8::Global<v8::Function>>,
    initialized: bool,
    modules: Vec<ModuleEntry>,

    // --- Shared with V8 callbacks via isolate slot ---
    state: SharedState,  // Rc<RefCell<RuntimeState>>

    // --- Request tracking ---
    pending_requests: HashMap<u64, PendingRequest>,

    // --- I/O futures (equivalent of workerd's IoContext task set) ---
    pending_ops: FuturesUnordered<Pin<Box<dyn Future<Output = OpResult>>>>,
    pending_timers: FuturesUnordered<Pin<Box<dyn Future<Output = TimerResult>>>>,

    // --- Stream forwarders (pass-through streaming, no V8 involvement) ---
    stream_forwarders: HashMap<u32, tokio::sync::mpsc::Sender<Vec<u8>>>,

    // --- Incoming requests (the ONE cross-thread channel) ---
    request_rx: tokio::sync::mpsc::Receiver<IncomingRequest>,

    // --- Lifecycle ---
    shutdown: CancellationToken,

    // --- CPU enforcement (Linux) ---
    #[cfg(target_os = "linux")]
    cpu_timer: Option<CpuTimer>,
    #[cfg(target_os = "linux")]
    cpu_timer_active: bool,
    cpu_limit: Option<Duration>,
}
```

```rust
/// Shared with V8 callbacks via Rc<RefCell<>> in isolate slot.
/// Borrows are brief (microseconds), never held across await points.
pub(crate) struct RuntimeState {
    // Timer registration
    pub(crate) timers: TimerState,

    // Promise resolvers for async ops
    pub(crate) pending_resolvers: HashMap<u32, v8::Global<v8::PromiseResolver>>,

    // Streams
    pub(crate) streams: HashMap<u32, StreamState>,
    pub(crate) next_stream_id: u32,

    // Op ID allocator
    pub(crate) next_op_id: u32,

    // Task buffer: V8 callbacks push futures here.
    // After each enter_v8(), the main loop drains these into FuturesUnordered.
    // This is the equivalent of workerd's IoContext::addTask().
    pub(crate) spawned_ops: Vec<Pin<Box<dyn Future<Output = OpResult>>>>,
    pub(crate) spawned_timers: Vec<(u32, Duration, Option<Duration>)>,

    // Per-request log routing
    pub(crate) executing_request_id: Option<u64>,
    pub(crate) per_request_logs: HashMap<u64, Vec<String>>,

    // Timer ownership: timer_id → request_id (for CPU attribution)
    pub(crate) timer_owner: HashMap<u32, u64>,

    // Cancel token for the currently-executing request.
    // Set before enter_v8, cleared after. V8 callbacks (fetch) clone this
    // to propagate cancellation into spawned futures.
    pub(crate) executing_request_cancel: Option<CancellationToken>,

    // App state (shared across requests — intentional)
    pub(crate) kv_store: HashMap<String, String>,
    pub(crate) env_vars: HashMap<String, String>,
    pub(crate) key_store: HashMap<u32, crate::crypto::KeyData>,
    pub(crate) next_key_id: u32,
}
```

```rust
/// Per-request context — lightweight equivalent of workerd's IoContext.
struct RequestContext {
    id: u64,
    cpu_accumulated: Duration,
    wall_start: Instant,
    cancel: CancellationToken,
}

/// Tracks an in-flight async request (handler returned a Promise).
struct PendingRequest {
    id: u64,
    promise: v8::Global<v8::Promise>,
    reply: oneshot::Sender<Result<RequestResult, String>>,
    context: RequestContext,
}

/// What crosses the thread boundary from the server.
pub struct IncomingRequest {
    pub id: u64,
    pub body: String,
    pub reply: oneshot::Sender<Result<RequestResult, String>>,
    pub cancel: CancellationToken,
}
```

### enter_v8 — the single V8 entry point

Every V8 access goes through this function. It is the equivalent of workerd's
`IoContext::run()` → `Worker::Lock` → execute → `runMicrotasks()` → drop lock.

```rust
/// Enter V8, run a callback, drain microtasks, exit.
/// All V8 access in the runtime goes through this function.
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
```

### The main loop — tokio select!

```rust
impl Runtime {
    /// Run the event loop. Called from the isolate's dedicated thread.
    /// tokio's current-thread runtime drives execution.
    pub async fn run(&mut self) {
        self.ensure_initialized();

        loop {
            // After each V8 entry, collect futures spawned by JS
            self.collect_new_tasks();

            tokio::select! {
                // Check shutdown first, then fair-poll the rest.
                // Without biased, tokio randomizes branch poll order,
                // preventing request starvation of ops/timers under load.

                // --- Shutdown (always checked) ---
                _ = self.shutdown.cancelled() => {
                    self.graceful_shutdown();
                    break;
                }

                // --- New request from server ---
                Some(req) = self.request_rx.recv() => {
                    self.handle_incoming_request(req);
                }

                // --- Async op completed (fetch, crypto, etc.) ---
                Some(result) = self.pending_ops.next() => {
                    self.handle_op_result(result);
                }

                // --- Timer fired ---
                Some(result) = self.pending_timers.next() => {
                    self.handle_timer(result);
                }

                // --- All sources exhausted ---
                else => break,
            }
        }
    }

    fn handle_incoming_request(&mut self, req: IncomingRequest) {
        let req_id = req.id;
        self.state.borrow_mut().executing_request_id = Some(req_id);

        let cpu_before = thread_cpu_time();
        self.arm_cpu_timer();

        let result = enter_v8(&mut self.isolate, &self.context, |scope| {
            dispatch_request(scope, &self.state, &req.body)
        });

        self.disarm_cpu_timer();
        let cpu_elapsed = thread_cpu_time().saturating_sub(cpu_before);
        self.state.borrow_mut().executing_request_id = None;

        match result {
            DispatchResult::Sync(value) => {
                let logs = self.drain_request_logs(req_id);
                let wall_start = Instant::now(); // for sync: wall ≈ cpu
                let _ = req.reply.send(Ok(RequestResult {
                    json: value,
                    cpu_time: cpu_elapsed,
                    wall_time: wall_start.elapsed(),
                    logs,
                }));
            }
            DispatchResult::Async(promise) => {
                self.pending_requests.insert(req_id, PendingRequest {
                    id: req_id,
                    promise,
                    reply: req.reply,
                    context: RequestContext {
                        id: req_id,
                        cpu_accumulated: cpu_elapsed,
                        wall_start: Instant::now(),
                        cancel: req.cancel,
                    },
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
                self.state.borrow_mut().executing_request_id = request_id;

                let cpu_before = thread_cpu_time();
                self.arm_cpu_timer();

                enter_v8(&mut self.isolate, &self.context, |scope| {
                    resolve_op(scope, &self.state, op_id, &value);
                });

                self.disarm_cpu_timer();
                let cpu_elapsed = thread_cpu_time().saturating_sub(cpu_before);
                self.state.borrow_mut().executing_request_id = None;

                // Accumulate CPU for the correct request
                if let Some(req_id) = request_id {
                    if let Some(pending) = self.pending_requests.get_mut(&req_id) {
                        pending.context.cpu_accumulated += cpu_elapsed;
                    }
                }

                self.check_settled_promises_v8();
            }
            OpResult::StreamChunk { stream_id, data, done } => {
                if let Some(fwd) = self.stream_forwarders.get(&stream_id) {
                    // Pass-through: no V8 entry, direct to HTTP body channel
                    // Use try_send to avoid blocking the event loop
                    let _ = fwd.try_send(data);
                    if done { self.stream_forwarders.remove(&stream_id); }
                } else {
                    // JS ReadableStream: needs V8
                    enter_v8(&mut self.isolate, &self.context, |scope| {
                        push_stream_chunk(scope, &self.state, stream_id, &data, done);
                    });
                }
            }
            OpResult::Cancelled => {}
        }
    }

    fn handle_timer(&mut self, result: TimerResult) {
        let timer_id = result.id;

        // Look up which request registered this timer (if any)
        let request_id = self.state.borrow().timer_owner.get(&timer_id).copied();
        self.state.borrow_mut().executing_request_id = request_id;

        let cpu_before = thread_cpu_time();
        self.arm_cpu_timer();

        enter_v8(&mut self.isolate, &self.context, |scope| {
            fire_timer_callback(scope, &self.state, timer_id);
        });

        self.disarm_cpu_timer();
        let cpu_elapsed = thread_cpu_time().saturating_sub(cpu_before);
        self.state.borrow_mut().executing_request_id = None;

        if let Some(req_id) = request_id {
            if let Some(pending) = self.pending_requests.get_mut(&req_id) {
                pending.context.cpu_accumulated += cpu_elapsed;
            }
        }

        // Re-register interval timers
        if let Some(interval) = result.interval {
            self.pending_timers.push(Box::pin(async move {
                tokio::time::sleep(interval).await;
                TimerResult { id: timer_id, interval: Some(interval) }
            }));
        }

        self.check_settled_promises_v8();
    }
}
```

### Task collection — the equivalent of workerd's addTask()

When JS calls `fetch()` or `setTimeout()`, V8 callbacks can't directly push into
`FuturesUnordered` (owned by the `select!` loop, not accessible from a V8 callback).
Instead, callbacks push into a buffer on `RuntimeState`. After each `enter_v8` returns,
the main loop drains the buffer.

```rust
impl Runtime {
    /// Drain futures spawned by JS into the FuturesUnordered sets.
    /// Called after each enter_v8() — the equivalent of workerd's addTask().
    fn collect_new_tasks(&mut self) {
        let mut s = self.state.borrow_mut();

        for op in s.spawned_ops.drain(..) {
            self.pending_ops.push(op);
        }

        for (id, delay, interval) in s.spawned_timers.drain(..) {
            self.pending_timers.push(Box::pin(async move {
                tokio::time::sleep(delay).await;
                TimerResult { id, interval }
            }));
        }
    }
}
```

### How fetch() works end-to-end

```
JS calls fetch(url)
  ↓
raw_fetch_callback (V8 callback, inside enter_v8):
  1. Create v8::Promise + resolver, store in pending_resolvers
  2. Build an async future: do_fetch(url).await → OpResult
  3. Push future into state.spawned_ops buffer
  4. Return promise to JS
  ↓
enter_v8 returns → collect_new_tasks():
  Drain spawned_ops into self.pending_ops (FuturesUnordered)
  ↓
select! loop: tokio polls the future
  reqwest::get(url).await completes (epoll, same thread)
  ↓
handle_op_result:
  enter_v8 → resolve promise → microtask checkpoint
  ↓
check_settled_promises_v8:
  If the request's top-level promise settled → send reply
```

The fetch V8 callback changes from today:

```rust
// BEFORE (fetch.rs):
// Spawns on server's multi-threaded tokio via handle.spawn(task)
// Sends result via std::sync::mpsc channel + waker.wake()

// AFTER:
// Pushes a future into state.spawned_ops
// No channel, no spawn, no wake mechanism
pub(crate) fn raw_fetch_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope.get_slot::<SharedState>().unwrap().clone();

    let method = args.get(0).to_rust_string_lossy(scope);
    let url = args.get(1).to_rust_string_lossy(scope);
    let headers_json = args.get(2).to_rust_string_lossy(scope);
    let body = if args.length() > 3 && !args.get(3).is_null_or_undefined() {
        Some(args.get(3).to_rust_string_lossy(scope))
    } else {
        None
    };

    let resolver = v8::PromiseResolver::new(scope).unwrap();
    let promise = resolver.get_promise(scope);
    let global_resolver = v8::Global::new(scope, resolver);

    let (op_id, request_id) = {
        let mut s = state.borrow_mut();
        let id = s.next_op_id;
        s.next_op_id += 1;
        s.pending_resolvers.insert(id, global_resolver);
        let req_id = s.executing_request_id;
        (id, req_id)
    };

    // Capture cancel token for the owning request (if any)
    let cancel = {
        let s = state.borrow();
        // Cancel token is threaded from IncomingRequest → RuntimeState → fetch future.
        // For fetches triggered outside a request (e.g., module-level), cancel is None.
        s.executing_request_cancel.clone()
    };

    // Push the future — it will be collected by collect_new_tasks()
    // after this enter_v8 scope exits.
    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        let fetch_result = if let Some(cancel) = cancel {
            tokio::select! {
                r = do_fetch(&method, &url, &headers_json, body.as_deref()) => r,
                _ = cancel.cancelled() => Err(error_json("request cancelled")),
            }
        } else {
            do_fetch(&method, &url, &headers_json, body.as_deref()).await
        };
        match fetch_result {
            Ok(value) => OpResult::Completed { op_id, value, request_id },
            Err(err) => OpResult::Completed { op_id, value: err, request_id },
        }
    }));

    rv.set(promise.into());
}
```

### How setTimeout works end-to-end

```
JS calls setTimeout(callback, delay)
  ↓
set_timeout_callback (V8 callback, inside enter_v8):
  1. Store callback in timers.callbacks HashMap
  2. Push (timer_id, delay, None) into state.spawned_timers
  3. Return timer_id to JS
  ↓
enter_v8 returns → collect_new_tasks():
  Create tokio::time::sleep(delay) future, push into self.pending_timers
  ↓
select! loop: tokio fires the timer
  sleep(delay).await completes
  ↓
handle_timer:
  enter_v8 → fire callback → microtask checkpoint
  For setInterval: push new sleep future (re-register)
```

The timer registration callback changes:

```rust
// BEFORE (timers.rs):
// Stores callback + pushes heap entry. fire_ready_timers() checks heap each tick.

// AFTER:
// Stores callback. Pushes (id, delay, interval) into spawned_timers buffer.
// The main loop creates a tokio::time::sleep future.
pub(crate) fn set_timeout_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope.get_slot::<SharedState>().unwrap().clone();

    let callback = v8::Local::<v8::Function>::try_from(args.get(0)).unwrap();
    let delay_ms = args.get(1).number_value(scope).unwrap_or(0.0) as u64;
    let is_interval = args.length() > 2 && args.get(2).boolean_value(scope);

    let global_callback = v8::Global::new(scope, callback);
    let delay = Duration::from_millis(delay_ms);
    let interval = if is_interval { Some(delay) } else { None };

    let mut s = state.borrow_mut();
    let id = s.timers.next_id;
    s.timers.next_id += 1;
    s.timers.callbacks.insert(id, TimerCallback { callback: global_callback, interval });

    // Track owner for CPU attribution
    if let Some(req_id) = s.executing_request_id {
        s.timer_owner.insert(id, req_id);
    }

    s.spawned_timers.push((id, delay, interval));

    rv.set(v8::Number::new(scope, id as f64).into());
}
```

### Per-request isolation

**Log attribution**: `executing_request_id` is set before each `enter_v8` call for a
specific request and cleared after. Since JS is single-threaded, this always identifies
the correct request. `console.log` routes to `per_request_logs[request_id]`.

```rust
fn console_log_callback(scope: &mut v8::HandleScope, args: v8::FunctionCallbackArguments, _rv: v8::ReturnValue) {
    let state: SharedState = scope.get_slot::<SharedState>().unwrap().clone();
    let mut s = state.borrow_mut();
    let msg = args.get(0).to_rust_string_lossy(scope);
    let request_id = s.executing_request_id.unwrap_or(0);
    s.per_request_logs.entry(request_id).or_default().push(msg);
}
```

**CPU time**: accumulated across all `enter_v8` calls for a request. Each `handle_op_result`
and `handle_timer` adds `thread_cpu_time()` delta to the correct `PendingRequest.context.cpu_accumulated`.

**Cancellation**: each `IncomingRequest` carries a `CancellationToken`. Fetch futures check it:

```rust
async fn do_fetch(url: &str, cancel: CancellationToken) -> Result<String, String> {
    tokio::select! {
        result = reqwest::get(url) => { /* process result */ }
        _ = cancel.cancelled() => Err("request cancelled".to_string())
    }
}
```

The cancel token is propagated from `IncomingRequest` → `RequestContext` → fetch futures
(via the `spawned_ops` buffer, which captures the token in the closure).

### Promise settlement

After each `enter_v8`, `check_settled_promises_v8` inspects pending promises:

```rust
fn check_settled_promises_v8(&mut self) {
    enter_v8(&mut self.isolate, &self.context, |scope| {
        let settled: Vec<u64> = self.pending_requests.iter()
            .filter(|(_, req)| {
                let p = v8::Local::new(scope, &req.promise);
                p.state() != v8::PromiseState::Pending
            })
            .map(|(id, _)| *id)
            .collect();

        for id in settled {
            if let Some(req) = self.pending_requests.remove(&id) {
                let promise = v8::Local::new(scope, &req.promise);
                let result = extract_promise_result(scope, promise);
                let logs = self.drain_request_logs(id);
                let _ = req.reply.send(Ok(RequestResult {
                    json: result,
                    cpu_time: req.context.cpu_accumulated,
                    wall_time: req.context.wall_start.elapsed(),
                    logs,
                }));
            }
        }
    });
}

fn drain_request_logs(&self, request_id: u64) -> Vec<String> {
    self.state.borrow_mut().per_request_logs.remove(&request_id).unwrap_or_default()
}
```

This is called once per event (not 3× per tick). Each event typically settles at most one promise.

**Future optimization**: Register `v8::Promise::Then()` native callback to push settled IDs
into a queue. Then `check_settled_promises_v8` drains the queue instead of scanning. This
is O(1) per settled promise but not required for the initial implementation.

### Streaming HTTP responses

Two-phase response from v2 is preserved. The change: stream chunks are local futures
instead of channel events.

```rust
/// Streaming HTTP result — headers arrive first, body streams.
pub struct HttpStreamResult {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body_rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    pub cpu_time: Duration,
    pub wall_time: Duration,
}
```

**Pass-through streaming** (fetch origin → client): the fetch future sends `OpResult::StreamChunk`
events. The main loop detects stream forwarders and routes chunks directly to the HTTP body
channel without entering V8:

```rust
OpResult::StreamChunk { stream_id, data, done } => {
    if let Some(fwd) = self.stream_forwarders.get(&stream_id) {
        let _ = fwd.try_send(data);  // backpressure via bounded channel
        if done { self.stream_forwarders.remove(&stream_id); }
    } else {
        enter_v8(/* push to JS ReadableStream */);
    }
}
```

**Backpressure**: `body_tx` is bounded (capacity 16). When the client reads slowly, `try_send`
returns `Full`. The chunk is buffered in an overflow `VecDeque` (max 64 entries) and retried
on the next stream chunk event. If overflow is also full, the stream is closed explicitly.
This matches v2's `StreamForwarder` design.

### V8Pool integration

The pool interface simplifies. `EventSender` disappears.

```rust
/// Per-app isolate entry in the pool.
struct IsolateEntry {
    request_tx: tokio::sync::mpsc::Sender<IncomingRequest>,
    shutdown: CancellationToken,
    last_used_ms: AtomicU64,
    request_count: AtomicU64,
    logs: std::sync::Mutex<Vec<String>>,
}
```

**Spawning an isolate:**

```rust
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
                let mut runtime = Runtime::new(modules, request_rx, shutdown_inner, cpu_limit);
                runtime.run().await;
            });
        })
        .map_err(|e| format!("Failed to spawn: {e}"))?;

    // Warmup — blocking_send works after spawn (no write lock held)
    let (reply_tx, reply_rx) = oneshot::channel();
    request_tx.blocking_send(IncomingRequest {
        id: 0,
        body: r#"{"jsonrpc":"2.0","method":"__ping","params":[],"id":0}"#.into(),
        reply: reply_tx,
        cancel: CancellationToken::new(),
    }).map_err(|_| "Send failed")?;
    reply_rx.blocking_recv().map_err(|_| "Warmup failed")??;

    Ok(IsolateEntry { request_tx, shutdown, /* ... */ })
}
```

**What changes:**
- `EventSender` (Arc<Mutex<Option<Thread>>> + unpark) → `tokio::sync::mpsc::Sender` (no mutex, no unpark)
- `std::sync::mpsc` → `tokio::sync::mpsc` (bounded with backpressure)
- Eviction: `shutdown.cancel()` instead of `LoopEvent::Shutdown` through channel
- Warmup: `blocking_send` + `blocking_recv` (no separate warmup thread)
- **Warmup no longer holds the write lock** — the channel exists before warmup runs

**Dispatch path (simplified):**

```rust
pub async fn dispatch(&self, app_id: &str, server_js: &str, body: String) -> Result<RpcResult, String> {
    let entry = self.get_or_create(app_id, server_js)?;
    let id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
    let (reply_tx, reply_rx) = oneshot::channel();
    let cancel = CancellationToken::new();

    entry.request_tx.send(IncomingRequest { id, body, reply: reply_tx, cancel })
        .await
        .map_err(|_| format!("Isolate for '{app_id}' is dead"))?;

    match tokio::time::timeout(self.wall_timeout, reply_rx).await {
        Ok(Ok(Ok(result))) => Ok(result.into()),
        Ok(Ok(Err(e))) => Err(e),
        Ok(Err(_)) => Err("Worker dropped reply".into()),
        Err(_) => Err("Request timed out".into()),
    }
}
```

### Graceful shutdown

```rust
fn graceful_shutdown(&mut self) {
    // Cancel all in-flight request background tasks
    for (_, req) in &self.pending_requests {
        req.context.cancel.cancel();
    }

    // Drain remaining ops with a deadline
    let deadline = Instant::now() + Duration::from_secs(5);
    // Run the select! loop without the request_rx branch until
    // pending_requests is empty or deadline passes.
    // (Implementation: set a flag that skips the request_rx branch,
    // or close the receiver.)

    // Error any remaining requests
    for (id, req) in self.pending_requests.drain() {
        let _ = req.reply.send(Err(format!("shutdown: request {} aborted", id)));
    }
}
```

### CPU enforcement

Same as v2 — POSIX CPU timer with SIGALRM. The arm/disarm pattern wraps each `enter_v8`:

```
arm_cpu_timer()
enter_v8(|scope| { ... })
disarm_cpu_timer()
```

If the CPU timer fires during V8 execution, `v8::Isolate::terminate_execution()` is called
from the signal handler. After `enter_v8` returns, `is_execution_terminating()` is checked
and all pending requests are errored with "CPU time limit exceeded".

### Backpressure

All channels are bounded:
- `request_rx`: capacity 256 (incoming requests)
- `body_tx`: capacity 16 (streaming response chunks)
- `spawned_ops` / `spawned_timers`: unbounded Vecs, but bounded by `MAX_PENDING_OPS` (1024) check in V8 callbacks

When `request_tx.send().await` blocks (channel full), the tokio task in the HTTP handler
pauses, which pauses the HTTP read, which applies TCP backpressure to the client.

## What disappears

| Component | LOC | Reason |
|-----------|-----|--------|
| `EventSender` | ~30 | Replaced by `tokio::sync::mpsc::Sender` |
| `std::sync::mpsc` channel | — | Replaced by `tokio::sync::mpsc` |
| `AtomicWaker` / `ParkWaker` | ~40 | Tokio waker handles wake |
| `tick()` with 5 phases | ~80 | Replaced by `select!` branches |
| `park_timeout()` / `compute_wait_timeout()` | ~30 | Tokio schedules wake precisely |
| `drain_all_events()` + classify | ~30 | Each source is a separate `select!` branch |
| `run_event_loop()` (park loop) | ~40 | Replaced by `run()` async |
| `buffered: Vec<LoopEvent>` | — | No intermediate buffer needed |
| `LoopEvent::NewRequest` / `LoopEvent::Shutdown` | — | `IncomingRequest` struct + `CancellationToken` |

## File structure

```
crates/runtime/src/
|
+-- lib.rs              Public API: Runtime, IncomingRequest, RequestResult, HttpStreamResult
|
+-- runtime.rs          Runtime struct, enter_v8(), run() select! loop,
|                       collect_new_tasks(), handle_*, check_settled_promises_v8,
|                       graceful_shutdown
|
+-- state.rs            RuntimeState, SharedState, RequestContext, PendingRequest
|                       OpResult, TimerResult, DispatchResult enums
|                       Channel capacity constants, memory limit constants
|
+-- request.rs          dispatch_request(), extract_promise_result(),
|                       resolve_op(), fire_timer_callback() — free functions
|                       taking (scope, state, ...) to avoid borrow conflicts
|
+-- init.rs             ensure_initialized, setup_globals, load_polyfills, load_modules
|
+-- modules.rs          ESM module loader (unchanged)
|
+-- timers.rs           TimerState, TimerCallback, set_timeout_callback
|                       (heap removed — tokio::time::sleep replaces it)
|
+-- streams.rs          StreamState, push_stream_chunk (unchanged except buffer limits)
|
+-- crypto.rs           13 crypto ops (unchanged)
+-- fetch.rs            raw_fetch_callback → pushes future into spawned_ops
|                       do_fetch() async function (reqwest, unchanged logic)
+-- url.rs              Unchanged
+-- kv.rs               Unchanged
+-- env.rs              Unchanged
+-- ops.rs              Unchanged
+-- cpu_timer.rs        Unchanged
+-- storage.rs          Unchanged
|
+-- embed/              Unchanged
|
+-- server.rs           Benchmark server: uses Runtime::run() directly
```

### What merges

| Before | After | Why |
|--------|-------|-----|
| `concurrent.rs` (943 LOC) | **Deleted** → `runtime.rs` | tick loop replaced by select! |
| `event_loop.rs` (353 LOC) | **Deleted** → `state.rs` + `runtime.rs` | EventLoopInner split into RuntimeState + Runtime |

### What stays

| File | LOC | Change |
|------|-----|--------|
| `crypto.rs` | 1,164 | None |
| `modules.rs` | 355 | None |
| `streams.rs` | 286 | Minor (buffer limits) |
| `cpu_timer.rs` | 322 | None |
| `storage.rs` | 311 | None |
| `url.rs` | 62 | None |
| `kv.rs` | 28 | None |
| `env.rs` | 16 | None |
| `ops.rs` | 53 | None |
| `embed/*.js` | ~1,170 | None |

## Public API

```rust
// Before:
use appbase_runtime::{ConcurrentIsolate, EventSender, LoopEvent, init_v8};
let (tx, rx) = std::sync::mpsc::channel();
let sender = EventSender::new(tx, v8_thread);
let mut iso = ConcurrentIsolate::new(modules, rx, tx, handle, limit, env);
iso.run_event_loop();  // blocks thread
sender.send(LoopEvent::NewRequest { id, body, reply });

// After:
use appbase_runtime::{Runtime, IncomingRequest, init_v8};
let (request_tx, request_rx) = tokio::sync::mpsc::channel(256);
let shutdown = CancellationToken::new();
// On dedicated thread:
let local_rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
local_rt.block_on(async {
    let mut runtime = Runtime::new(modules, request_rx, shutdown, cpu_limit);
    runtime.run().await;
});
// From server:
request_tx.send(IncomingRequest { id, body, reply, cancel }).await?;
```

## Success criteria

1. All 88 existing tests pass
2. Benchmark: >=330K req/s for sync RPC (no regression)
3. No `std::sync::mpsc` in the runtime crate
4. No `thread::park` / `thread::unpark` / `AtomicWaker` / `ParkWaker`
5. No `tick()` function — `select!` is the event loop
6. `enter_v8()` is the single entry point for all V8 access
7. Per-request log isolation: console.log from request A never appears in request B's logs
8. CPU time accumulated across all V8 entries per request (not just initial dispatch)
9. Cancellation: client disconnect cancels in-flight fetch within 100ms
10. Streaming: pass-through SSE never enters V8 after header extraction
11. Backpressure: slow client → bounded channel blocks → TCP backpressure upstream
12. Warmup does not hold the V8Pool write lock
