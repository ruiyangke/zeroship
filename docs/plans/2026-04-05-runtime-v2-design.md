# Runtime v2 Design — Non-blocking Event Loop

**Date:** 2026-04-05
**Revision:** 6 (Round 4 fixes: stale forwarder cleanup, CPU timer disarm before wait, v8::String unwrap guards, clone-free overflow drain)
**Goal:** Refactor the runtime into a clean non-blocking architecture based on tokio, enabling streaming, WebSocket, and multi-isolate-per-thread.

## Why

The current runtime was built incrementally across multiple sessions:
- `Isolate` (per-request, blocking)
- `ConcurrentIsolate` (multi-request, blocking recv_timeout)
- `EventLoopInner` god struct (crypto keys, KV, env, timers, streams, resolvers)
- `OpResult` -> `LoopEvent` (bolted on for streaming)
- Two nearly identical event loop drivers sharing ~80% logic
- Event handling code duplicated 3 times

It works (88 tests, 337K req/s) but can't support:
- Chunked HTTP responses (true SSE/AI streaming to client)
- WebSocket
- Streaming request bodies (deferred to v3 — current apps use complete JSON bodies)
- Multiple isolates per thread
- Non-blocking I/O integration

## Architecture

### Design principle: borrow-boundary alignment

<!-- Added in round 1: addressing critic's point about mutable aliasing -->

The central design constraint is Rust's borrow checker. When you create a V8 scope via
`v8::HandleScope::new(&mut isolate)`, you hold an exclusive `&mut` borrow on the isolate.
If the event channel receiver lives in the same struct, you cannot access it while the scope
is alive. The current code works around this by passing `event_rx` as a separate parameter,
but the v2 design must make borrow boundaries explicit in the type system.

We split the runtime into three structs, each aligned with a different borrow lifetime:

1. **V8State** — exclusively borrowed when a V8 scope is alive
2. **EventDriver** — accessed only between ticks (no V8 scope alive)
3. **RuntimeState** — shared with V8 callbacks via `Rc<RefCell<>>`, brief borrows only

### Core types

```rust
/// V8 isolate + handles — V8 scope borrows this exclusively.
/// When a HandleScope exists on V8State.isolate, nothing else can touch V8State.
struct V8State {
    isolate: v8::OwnedIsolate,
    context: v8::Global<v8::Context>,
    dispatch_fn: Option<v8::Global<v8::Function>>,
    on_request_fn: Option<v8::Global<v8::Function>>,
    http_dispatch_fn: Option<v8::Global<v8::Function>>,
    modules: Vec<ModuleEntry>,
    initialized: bool,
}

/// Event channel + wait machinery — accessed between ticks, never while V8 scope lives.
struct EventDriver {
    event_rx: tokio::sync::mpsc::Receiver<LoopEvent>,  // bounded!
    local_rt: tokio::runtime::Runtime,                   // current-thread, owned per Runtime
    timer_sleep: Option<Pin<Box<tokio::time::Sleep>>>,
    buffer: Vec<LoopEvent>,  // events received during wait(), processed next tick
}

/// Shared with V8 callbacks via Rc<RefCell<>> in isolate slot.
/// Borrows are brief (microseconds), never held across await points.
pub(crate) struct RuntimeState {
    // Event loop
    pub(crate) timers: TimerState,
    pub(crate) pending_resolvers: HashMap<u32, v8::Global<v8::PromiseResolver>>,
    pub(crate) streams: HashMap<u32, StreamState>,
    pub(crate) next_op_id: u32,
    pub(crate) next_stream_id: u32,

    // Event channel (sender only — receiver is in EventDriver, outside RefCell)
    pub(crate) event_tx: tokio::sync::mpsc::Sender<LoopEvent>,  // bounded!

    // Tokio handle for spawning background tasks
    pub(crate) server_handle: Option<tokio::runtime::Handle>,

    // Concurrent model (optional) — bounded to EVENT_CHANNEL_CAPACITY.
    // Uses tokio::sync::mpsc (not std::sync::mpsc) so the bounded send can await backpressure.
    pub(crate) concurrent_event_tx: Option<tokio::sync::mpsc::Sender<ConcurrentEvent>>,

    // Per-request context (see "Per-request isolation" section)
    pub(crate) active_requests: HashMap<u32, RequestContext>,

    // Set before each JS dispatch, cleared after. Since JS is single-threaded,
    // exactly one request's code runs at a time. console_log reads this field.
    pub(crate) executing_request_id: Option<u32>,

    // Per-request log buffer (drained after each request completes).
    // Each entry is tagged with request_id for correct attribution in concurrent mode.
    // In per-request mode, all entries share the same request_id.
    pub(crate) log_buffer: Vec<(u32, String)>,

    // App state (shared across requests — intentionally)
    pub(crate) kv_store: HashMap<String, String>,
    pub(crate) env_vars: HashMap<String, String>,
    pub(crate) key_store: HashMap<u32, KeyData>,
    pub(crate) next_key_id: u32,
}

pub(crate) type SharedState = Rc<RefCell<RuntimeState>>;

/// The top-level runtime — composed of the three separated concerns.
pub struct Runtime {
    v8: V8State,
    driver: EventDriver,
    state: SharedState,

    // Request tracking (for concurrent mode)
    pending_requests: HashMap<u32, PendingRequest>,

    // Stream forwarders: stream_id -> HTTP body sender (for DeferredProxy streaming)
    // <!-- Added in round 3: addressing phantom field self.stream_forwarders -->
    stream_forwarders: HashMap<u32, StreamForwarder>,

    // CPU enforcement (Linux) — see cpu_timer.rs (322 LOC)
    // <!-- Added in round 3: addressing CPU enforcement gap -->
    #[cfg(target_os = "linux")]
    cpu_timer: Option<CpuTimer>,
    #[cfg(target_os = "linux")]
    cpu_timer_active: bool,
    cpu_limit: Option<Duration>,
}
```

**Why three structs:** When `tick()` creates a V8 scope, it borrows `&mut self.v8`. The
compiler knows `self.driver` and `self.state` are disjoint fields, so they remain accessible.
But `self.driver.event_rx` cannot be polled while a V8 scope borrows `self.v8.isolate` if they
were in the same struct — splitting them makes the borrow boundaries compile-time checked.

<!-- Added in round 1: addressing critic's point about per-isolate tokio runtime cost -->

**Why per-Runtime `local_rt`:** Creating a `tokio::runtime::Runtime` with `new_current_thread()`
costs approximately 1 microsecond — negligible for isolates that live minutes to hours. The alternative
(sharing a runtime across isolates via thread-local) introduces lifetime complexity with no
measurable benefit. `Handle::block_on` panics if called from within a tokio context, which
happens when the V8 thread is spawned from the server's tokio runtime — owning a separate
current-thread runtime avoids this entirely.

### Backpressure

<!-- Added in round 1: addressing critic's point about unbounded channels / OOM -->

Every channel in the system is bounded. This prevents OOM when producers outpace consumers
(e.g., AI streaming through a slow client connection). The mechanism is the same one used
by Cloudflare workerd (`co_await output.write()`) and Node.js streams (highWaterMark):
when the buffer is full, the producer blocks, which pauses the network read, which applies
TCP backpressure upstream.

```rust
/// All tunable runtime constants — passed to Runtime::new().
pub struct RuntimeConfig {
    pub event_channel_capacity: usize,      // default: 256
    pub stream_body_capacity: usize,        // default: 16
    pub max_stream_buffer_bytes: usize,     // default: 4 MB
    pub cpu_limit: Option<Duration>,        // default: None (unlimited)
    pub memory_limit_bytes: Option<usize>,  // default: None (V8 default)
    pub wall_timeout: Duration,             // default: 30s
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            event_channel_capacity: EVENT_CHANNEL_CAPACITY,
            stream_body_capacity: STREAM_BODY_CAPACITY,
            max_stream_buffer_bytes: MAX_STREAM_BUFFER_BYTES,
            cpu_limit: None,
            memory_limit_bytes: None,
            wall_timeout: Duration::from_secs(30),
        }
    }
}

/// Bounded channel capacity constants.
/// These are tuned for typical workloads; each can be overridden via RuntimeConfig.
const EVENT_CHANNEL_CAPACITY: usize = 256;
const STREAM_BODY_CAPACITY: usize = 16;       // ~16 chunks buffered before backpressure
const MAX_STREAM_BUFFER_BYTES: usize = 4 * 1024 * 1024;  // 4 MB max per stream

impl Runtime {
    pub fn new(modules: Vec<ModuleEntry>, env_vars: HashMap<String, String>) -> Self {
        // Bounded channel — when full, senders block on .send().await
        let (event_tx, event_rx) = tokio::sync::mpsc::channel(EVENT_CHANNEL_CAPACITY);
        // ...
    }
}
```

For streaming HTTP response bodies:
```rust
// Bounded at 16 chunks. When the client reads slowly, the channel fills up,
// the fetch tokio task pauses on body_tx.send().await, which pauses the upstream
// HTTP read, which applies TCP backpressure to the origin server.
let (body_tx, body_rx) = tokio::sync::mpsc::channel(STREAM_BODY_CAPACITY);
```

For the internal event channel: `event_tx.send()` is called from tokio tasks (fetch, WebSocket).
With a bounded channel, if the V8 event loop is slow to drain events, senders pause, which
pauses the network read, which applies TCP backpressure. This is the correct behavior — it
prevents the event queue from growing without bound during slow V8 execution.

### Per-request isolation

<!-- Added in round 1: addressing critic's point about shared log_buffer / kv leaking across requests -->

In concurrent mode, multiple requests execute interleaved on the same isolate. Shared mutable
state like `log_buffer` would leak across requests. The fix: per-request context for
request-scoped state.

```rust
/// Per-request context — tracks request-scoped state (logs, timing, cancellation).
/// Does NOT own the reply sender — that lives on PendingRequest (oneshot::Sender is !Clone).
struct RequestContext {
    id: u32,
    log_buffer: Vec<String>,
    cpu_start: Duration,
    wall_start: Instant,
    cancel: tokio_util::sync::CancellationToken,
}
```

<!-- Added in round 3: addressing current_request_id global overwrite -->

Console.log routing depends on the execution model:

- **Per-request mode** (`execute_request` / `execute_http`): Only one request executes at a
  time. `log_buffer` lives on `RequestContext` and is drained when the request completes.
  There is no ambiguity about which request a log belongs to — it is always the single
  active request.

- **Concurrent mode** (`accept_request`): Multiple requests are interleaved on the same
  isolate, but each JS dispatch is called with the request ID. The `check_settled_promises`
  free function attributes logs to the correct request via `PendingRequest` tracking when
  the promise settles.

There is no global `current_request_id` that persists across dispatches. Instead,
`executing_request_id` is set immediately before entering V8 for a specific request's
code and cleared immediately after. Since JS execution is serial (one request's code
runs at a time on the single-threaded event loop), this field is always correct.

```rust
// Before dispatching request N's JS code (in accept_request, execute_request, etc.):
self.state.borrow_mut().executing_request_id = Some(request_id);
// ... V8 execution (dispatch call) ...
self.state.borrow_mut().executing_request_id = None;

// In the console.log native callback:
fn console_log_callback(scope: &mut v8::HandleScope, args: v8::FunctionCallbackArguments, _rv: v8::ReturnValue) {
    let state = get_state(scope);
    let mut s = state.borrow_mut();
    let msg = args.get(0).to_rust_string_lossy(scope);

    // executing_request_id is set before each JS dispatch and cleared after.
    // JS is single-threaded, so this always identifies the correct request.
    let request_id = s.executing_request_id.unwrap_or(0);
    s.log_buffer.push((request_id, msg));
}
```

KV store and env vars remain shared — they represent app-level state, not request-level state.
This matches Cloudflare Workers where KV bindings are per-worker, not per-request.

### Cancellation

<!-- Added in round 1: addressing critic's point about no cancellation / client disconnect -->

Every request carries a `CancellationToken` (from `tokio_util::sync`). When the HTTP client
disconnects, the server layer cancels the token, which propagates to all background tasks
associated with that request.

```rust
// When client disconnects (detected by hyper):
request_ctx.cancel.cancel();

// Fetch tasks check cancellation:
async fn do_fetch(url: String, cancel: CancellationToken, event_tx: mpsc::Sender<LoopEvent>) {
    let client = reqwest::Client::new();
    let mut response = match client.get(&url).send().await {
        Ok(r) => r,
        Err(e) => { /* send error event */ return; }
    };

    loop {
        tokio::select! {
            chunk = response.chunk() => {
                match chunk {
                    Ok(Some(data)) => {
                        if event_tx.send(LoopEvent::StreamChunk {
                            stream_id, data: data.to_vec(), done: false
                        }).await.is_err() {
                            return; // event loop gone, stop fetching
                        }
                    }
                    Ok(None) => {
                        let _ = event_tx.send(LoopEvent::StreamChunk {
                            stream_id, data: vec![], done: true
                        }).await;
                        return;
                    }
                    Err(_) => { /* send error event */ return; }
                }
            }
            _ = cancel.cancelled() => {
                return; // client disconnected, stop fetching immediately
            }
        }
    }
}

// Event loop checks cancellation during tick (free function — see tick() PHASE 6):
// check_cancelled_requests(scope, &self.state, &mut self.pending_requests);
```

This matches the pattern used by Deno's `op_fetch` and Cloudflare Workers' automatic fetch
cancellation on client disconnect.

### Event types

<!-- Added in round 1: addressing critic's point about LoopEvent extensibility -->

The enum is appropriate for a small, known set of event types. When it grows beyond ~8 variants,
we would refactor to trait objects — but for the foreseeable scope (ops, streams, WebSocket,
cancellation), an enum gives exhaustive matching and zero allocation.

```rust
pub(crate) enum LoopEvent {
    // Core ops — value is JSON-encoded String for now. Future: typed OpValue enum
    // (e.g., OpValue::Bytes(Vec<u8>), OpValue::Json(String)) to avoid double-serialization
    // for binary ops like crypto.
    OpCompleted { id: u32, value: String },

    // Streaming
    StreamChunk { stream_id: u32, data: Vec<u8>, done: bool },

    // Phase 2: WebSocket
    WsMessage { ws_id: u32, data: WsMessageData },
    WsClose { ws_id: u32, code: u16, reason: String },
    WsPing { ws_id: u32 },

    // Phase 3: Request lifecycle
    RequestCancelled { request_id: u32 },
}

pub(crate) enum WsMessageData {
    Text(String),
    Binary(Vec<u8>),
}
```

### StreamForwarder — backpressure-safe body forwarding

<!-- Added in round 3: addressing data loss on try_send Full -->

When forwarding stream chunks to HTTP body channels, `try_send` may return `Full` if the
client reads slowly. The chunk must NOT be dropped. `StreamForwarder` buffers one pending
chunk and retries on the next tick. This matches Node.js streams' `highWaterMark` pattern
where the writable side buffers until drain.

```rust
/// Stream forwarder with overflow buffer for backpressure.
/// When the mpsc channel is full, chunks accumulate in the overflow VecDeque.
/// When the overflow is also full, the stream is closed (explicit failure, not silent data loss).
struct StreamForwarder {
    sender: tokio::sync::mpsc::Sender<Vec<u8>>,
    overflow: VecDeque<Vec<u8>>,        // buffered chunks waiting for retry
    max_overflow: usize,                 // from RuntimeConfig (default: 64)
}

/// Forward a stream chunk to the HTTP body channel.
/// If the channel is full, buffer in overflow. If overflow is full, close the stream.
fn forward_stream_chunk(
    forwarders: &mut HashMap<u32, StreamForwarder>,
    stream_id: u32,
    data: Vec<u8>,
    done: bool,
) {
    if let Some(fwd) = forwarders.get_mut(&stream_id) {
        // Drain overflow into the channel first (FIFO order)
        while let Some(chunk) = fwd.overflow.pop_front() {
            match fwd.sender.try_send(chunk) {
                Ok(()) => {} // sent, already popped
                Err(tokio::sync::mpsc::error::TrySendError::Full(data)) => {
                    fwd.overflow.push_front(data); // put it back
                    break;
                }
                Err(_) => { forwarders.remove(&stream_id); return; } // channel closed
            }
        }

        if done {
            // Drop sender to signal EOF to the HTTP body stream
            forwarders.remove(&stream_id);
        } else {
            match fwd.sender.try_send(data) {
                Ok(()) => {}
                Err(tokio::sync::mpsc::error::TrySendError::Full(data)) => {
                    if fwd.overflow.len() >= fwd.max_overflow {
                        // Overflow full AND channel full — client is hopeless.
                        // Explicit failure: close the stream, log error.
                        // This is better than silent data loss.
                        // Log: "stream {stream_id}: overflow full, closing (client too slow)"
                        forwarders.remove(&stream_id);
                        return;
                    }
                    fwd.overflow.push_back(data); // buffer for retry next tick
                }
                Err(_) => { forwarders.remove(&stream_id); } // client disconnected
            }
        }
    }
}
```

### The poll-based tick

<!-- Added in round 1: addressing critic's point about mutable aliasing in tick -->

The key insight: drain events into a `Vec<LoopEvent>` BEFORE creating the V8 scope.
Then process from the Vec. The V8 scope borrows `&mut self.v8` but the driver is accessed
before and after — never during.

<!-- Added in round 3: addressing borrow conflict — handle_event is now a free function -->

**Critical borrow-safety rule:** Any function that needs both a V8 scope and access to
`Runtime` fields (state, stream_forwarders, pending_requests) must be a **free function**
taking those fields as separate parameters. A method like `self.handle_event(scope, event)`
would fail to compile because `scope` borrows `&mut self.v8.isolate` while `self.handle_event`
takes `&self` — violating Rust's aliasing rules. Free functions make the disjoint borrows
explicit.

```rust
impl EventDriver {
    /// Drain all currently-ready events without blocking.
    fn drain_ready(&mut self) -> Vec<LoopEvent> {
        let mut events = std::mem::take(&mut self.buffer);
        while let Ok(event) = self.event_rx.try_recv() {
            events.push(event);
        }
        events
    }

    /// Block until at least one event arrives or deadline expires.
    /// Events are buffered in self.buffer; consumed by drain_ready() on next tick.
    fn wait_for_events(&mut self, deadline: Instant) {
        let timeout = deadline.saturating_duration_since(Instant::now());

        self.local_rt.block_on(async {
            tokio::select! {
                event = self.event_rx.recv() => {
                    if let Some(e) = event {
                        self.buffer.push(e);
                    }
                }
                _ = async {
                    if let Some(sleep) = self.timer_sleep.as_mut() {
                        sleep.as_mut().await;
                    } else {
                        tokio::time::sleep(timeout).await;
                    }
                } => {}
            }
        });

        // Also drain anything else that became ready during the wait
        while let Ok(event) = self.event_rx.try_recv() {
            self.buffer.push(event);
        }

        // Buffer is consumed by drain_ready() in next tick().
    }
}

/// Handle one event — FREE FUNCTION to avoid borrow conflicts.
/// Takes scope and state as separate parameters instead of &self.
/// <!-- Added in round 3: converted from method to free function -->
fn handle_event(
    scope: &mut v8::PinScope,
    state: &SharedState,
    stream_forwarders: &mut HashMap<u32, StreamForwarder>,
    event: LoopEvent,
) {
    match event {
        LoopEvent::OpCompleted { id, value } => {
            let resolver = state.borrow_mut().pending_resolvers.remove(&id);
            if let Some(resolver) = resolver {
                let r = v8::Local::new(scope, &resolver);
                let val = match v8::String::new(scope, &value) {
                    Some(s) => s,
                    None => {
                        let err_msg = v8::String::new(scope, "Op result too large").unwrap();
                        let exc = v8::Exception::error(scope, err_msg);
                        r.reject(scope, exc);
                        return;
                    }
                };
                r.resolve(scope, val.into());
            }
        }
        LoopEvent::StreamChunk { stream_id, data, done } => {
            if stream_forwarders.contains_key(&stream_id) {
                // DeferredProxy path — forward to HTTP body channel only
                forward_stream_chunk(stream_forwarders, stream_id, data, done);
            } else {
                // JS ReadableStream path — push to V8
                crate::streams::push_stream_chunk(scope, state, stream_id, &data, done);
            }
        }
        LoopEvent::WsMessage { ws_id, data } => {
            crate::ws::push_ws_message(scope, state, ws_id, data);
        }
        LoopEvent::WsClose { ws_id, code, reason } => {
            crate::ws::push_ws_close(scope, state, ws_id, code, &reason);
        }
        LoopEvent::WsPing { ws_id } => {
            crate::ws::push_ws_pong(scope, state, ws_id);
        }
        LoopEvent::RequestCancelled { request_id } => {
            cleanup_request_state(scope, state, request_id);
        }
    }
    scope.perform_microtask_checkpoint();
}

/// Check for settled promises — FREE FUNCTION (same borrow-safety reason).
fn check_settled_promises(
    scope: &mut v8::PinScope,
    state: &SharedState,
    pending_requests: &mut HashMap<u32, PendingRequest>,
) {
    // Iterate pending_requests, check if their promises have settled,
    // extract results and send via reply channel.
    let settled: Vec<u32> = pending_requests.iter()
        .filter(|(_, req)| is_promise_settled(scope, &req.promise))
        .map(|(id, _)| *id)
        .collect();

    for id in settled {
        if let Some(req) = pending_requests.remove(&id) {
            let result = extract_promise_result(scope, state, &req.promise);
            let _ = req.reply.send(result);
        }
    }
}

/// Check for cancelled requests — FREE FUNCTION (same borrow-safety reason).
fn check_cancelled_requests(
    scope: &mut v8::PinScope,
    state: &SharedState,
    pending_requests: &mut HashMap<u32, PendingRequest>,
) {
    let s = state.borrow();
    let cancelled: Vec<u32> = s.active_requests.iter()
        .filter(|(_, ctx)| ctx.cancel.is_cancelled())
        .map(|(id, _)| *id)
        .collect();
    drop(s);

    for id in cancelled {
        cleanup_request_state(scope, state, id);
        if let Some(req) = pending_requests.remove(&id) {
            let _ = req.reply.send(Err("request cancelled".to_string()));
        }
    }
}

/// Clean up state for a single request — FREE FUNCTION.
fn cleanup_request_state(
    scope: &mut v8::PinScope,
    state: &SharedState,
    request_id: u32,
) {
    let mut s = state.borrow_mut();
    s.active_requests.remove(&request_id);
    // Clean up any streams, resolvers, etc. associated with request_id
}

impl Runtime {
    /// One tick of the event loop.
    fn tick(&mut self) -> bool {
        // PHASE 0: ARM CPU TIMER — before entering V8
        // <!-- Added in round 3: addressing CPU enforcement gap -->
        #[cfg(target_os = "linux")]
        if let (Some(timer), Some(limit)) = (&self.cpu_timer, self.cpu_limit) {
            if !self.cpu_timer_active {
                timer.arm(limit);
                self.cpu_timer_active = true;
            }
        }

        // PHASE 1: DRAIN — get events from driver BEFORE creating V8 scope.
        let events = self.driver.drain_ready();

        // NOW create V8 scope — borrows &mut self.v8 exclusively.
        let handle_scope = &mut v8::HandleScope::new(&mut self.v8.isolate);
        let context = v8::Local::new(handle_scope, &self.v8.context);
        let scope = &mut v8::ContextScope::new(handle_scope, context);

        // PHASE 2: PROCESS — handle events via FREE FUNCTION (no &self conflict)
        for event in events {
            handle_event(scope, &self.state, &mut self.stream_forwarders, event);
        }

        // PHASE 3: TIMERS — fire all ready timers
        fire_ready_timers(scope, &self.state);

        // PHASE 4: MICROTASKS — flush V8 microtask queue
        scope.perform_microtask_checkpoint();

        // PHASE 5: CHECK — check settled promises via free function
        check_settled_promises(scope, &self.state, &mut self.pending_requests);

        // PHASE 6: CANCELLATION — clean up cancelled requests via free function
        check_cancelled_requests(scope, &self.state, &mut self.pending_requests);

        // PHASE 7: RETRY — drain overflow buffers into channels (backpressure)
        let closed_ids: Vec<u32> = Vec::new();
        for (stream_id, fwd) in self.stream_forwarders.iter_mut() {
            while let Some(chunk) = fwd.overflow.pop_front() {
                match fwd.sender.try_send(chunk) {
                    Ok(()) => {} // sent, already popped
                    Err(tokio::sync::mpsc::error::TrySendError::Full(data)) => {
                        fwd.overflow.push_front(data); // put it back
                        break;
                    }
                    Err(_) => { closed_ids.push(*stream_id); break; }
                }
            }
        }
        for id in closed_ids {
            self.stream_forwarders.remove(&id);
        }

        // V8 scope drops here — safe to access driver again

        // PHASE 8: CPU TIMER — kept armed across ticks to avoid arm/disarm overhead.
        // Disarmed before wait_for_events in the run loop (I/O wait should not
        // count as CPU time) and on loop exit. Re-armed in PHASE 0 of next tick.

        // PHASE 9: CHECK TERMINATION — CPU timer may have called TerminateExecution
        if self.v8.isolate.is_execution_terminating() {
            self.v8.isolate.cancel_terminate_execution();
            // Error all pending requests — CPU limit exceeded
            for (_, req) in self.pending_requests.drain() {
                let _ = req.reply.send(Err("CPU time limit exceeded".to_string()));
            }
            return false;
        }

        self.has_pending_work()
    }

    /// Is there pending work?
    fn has_pending_work(&self) -> bool {
        let s = self.state.borrow();
        !s.timers.callbacks.is_empty()
            || !s.pending_resolvers.is_empty()
            || s.streams.values().any(|st| st.pending_read.is_some() && !st.closed)
            || !self.pending_requests.is_empty()
            || self.stream_forwarders.values().any(|f| !f.overflow.is_empty())
    }
}
```

### Error types

```rust
/// Errors that can terminate the event loop.
#[derive(Debug)]
pub enum EventLoopError {
    WallTimeout,
    CpuExceeded,
    ChannelClosed,
    StreamError(String),
    NoMoreWork,
}

/// Result of a completed request (blocking mode).
pub struct RequestResult {
    pub body: String,
    pub logs: Vec<String>,
    pub cpu_time: Duration,
    pub wall_time: Duration,
}

/// Tracks an in-flight request in concurrent mode.
struct PendingRequest {
    id: u32,
    promise: v8::Global<v8::Promise>,
    reply: oneshot::Sender<Result<RequestResult, String>>,
    cancel: tokio_util::sync::CancellationToken,
    wall_start: Instant,
}
```

### Two running modes (same tick, different wait)

```rust
impl Runtime {
    // === Mode 1: Blocking (for simple per-request use) ===

    /// Drive event loop until promise settles. Blocks the thread.
    pub fn run_until_settled(
        &mut self,
        promise: &v8::Global<v8::Promise>,
        wall_timeout: Duration,
    ) -> Result<(), EventLoopError> {
        let deadline = Instant::now() + wall_timeout;
        loop {
            if self.is_settled(promise) { return Ok(()); }
            if Instant::now() > deadline { return Err(EventLoopError::WallTimeout); }
            if !self.tick() {
                return if self.is_settled(promise) {
                    Ok(())
                } else {
                    Err(EventLoopError::NoMoreWork)
                };
            }

            // Check immediately after tick — the tick may have settled the promise.
            // This avoids an unnecessary wait_for_events when the result is already ready.
            if self.is_settled(promise) { return Ok(()); }

            // Disarm CPU timer during I/O wait — waiting is not CPU usage.
            // PHASE 0 of the next tick() will re-arm it.
            #[cfg(target_os = "linux")]
            if self.cpu_timer_active {
                if let Some(timer) = &self.cpu_timer { timer.disarm(); }
                self.cpu_timer_active = false;
            }

            // Wait — block until event arrives or timer fires.
            // Events are buffered in self.driver.buffer and consumed
            // by drain_ready() at the start of the next tick().
            self.driver.wait_for_events(deadline);
        }
    }

    // === Mode 2: Non-blocking (for tokio integration) ===

    /// Poll-based tick for integration with tokio executor.
    /// Returns Poll::Pending when waiting, Poll::Ready when done.
    pub fn poll_event_loop(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<()> {
        if !self.tick() {
            return Poll::Ready(());
        }

        // Register wakers for event channel
        match self.driver.event_rx.poll_recv(cx) {
            Poll::Ready(Some(event)) => {
                self.driver.buffer.push(event);
                cx.waker().wake_by_ref(); // re-poll immediately
                return Poll::Pending;
            }
            Poll::Ready(None) => return Poll::Ready(()), // channel closed
            Poll::Pending => {} // waker registered
        }

        // Register waker for next timer deadline
        if let Some(deadline) = self.next_timer_deadline() {
            let sleep = self.driver.timer_sleep.get_or_insert_with(|| {
                Box::pin(tokio::time::sleep_until(deadline.into()))
            });
            sleep.as_mut().reset(deadline.into());
            let _ = sleep.as_mut().poll(cx);
        } else {
            // No timers — clear stale sleep to avoid spurious wakes
            self.driver.timer_sleep = None;
        }

        Poll::Pending
    }
}
```

<!-- Added in round 1: addressing critic's point about stale timer_sleep -->

Note: `timer_sleep` is cleared when no timers are pending. This prevents spurious wakeups
from a stale sleep future that was set for a timer that has since been cancelled or fired.

### Request handling

```rust
impl Runtime {
    /// Execute a JSON-RPC request (blocking, returns when done).
    pub fn execute_request(&mut self, json: &str) -> Result<RequestResult, String> {
        self.ensure_initialized();
        self.drain_stale_state();

        // Enter V8, dispatch, check sync/async
        let (result, is_promise) = self.dispatch_rpc(json)?;

        if is_promise {
            self.run_until_settled(&result, Duration::from_secs(30))?;
            self.extract_promise_result(&result)
        } else {
            Ok(result)
        }
    }

    /// Execute an HTTP request (blocking, returns when done).
    pub fn execute_http(
        &mut self, method: &str, url: &str, headers: &str, body: &str,
    ) -> Option<Result<HttpResult, String>> {
        self.ensure_initialized();
        self.drain_stale_state();

        let (result, is_promise) = self.dispatch_http(method, url, headers, body)?;

        if is_promise {
            self.run_until_settled(&result, Duration::from_secs(30)).ok()?;
        }

        Some(self.extract_http_result(&result))
    }

    /// Accept and dispatch a request in concurrent mode.
    /// Returns immediately — result sent via reply channel.
    pub fn accept_request(
        &mut self,
        id: u32,
        body: String,
        reply: oneshot::Sender<Result<RequestResult, String>>,
        cancel: tokio_util::sync::CancellationToken,
    ) {
        self.ensure_initialized();

        // Create per-request context (request-scoped state — no reply sender)
        let ctx = RequestContext {
            id,
            log_buffer: Vec::new(),
            cpu_start: self.cpu_time(),
            wall_start: Instant::now(),
            cancel: cancel.clone(),
        };
        self.state.borrow_mut().active_requests.insert(id, ctx);

        // Set executing_request_id so console.log routes correctly
        self.state.borrow_mut().executing_request_id = Some(id);

        // Dispatch into V8 — call the JS handler with the request body
        let handle_scope = &mut v8::HandleScope::new(&mut self.v8.isolate);
        let context = v8::Local::new(handle_scope, &self.v8.context);
        let scope = &mut v8::ContextScope::new(handle_scope, context);

        let dispatch_fn = match self.v8.http_dispatch_fn.as_ref() {
            Some(f) => v8::Local::new(scope, f),
            None => {
                let _ = reply.send(Err("No HTTP handler registered".to_string()));
                return;
            }
        };
        let undefined = v8::undefined(scope).into();
        let body_v8 = match v8::String::new(scope, &body) {
            Some(s) => s.into(),
            None => {
                let _ = reply.send(Err("Request body too large for V8".to_string()));
                return;
            }
        };
        let id_v8 = v8::Number::new(scope, id as f64).into();
        let result = dispatch_fn.call(scope, undefined, &[id_v8, body_v8]);

        scope.perform_microtask_checkpoint();

        self.state.borrow_mut().executing_request_id = None;

        // Check if result is a Promise (async handler) or immediate value
        if let Some(result) = result {
            let promise = v8::Local::<v8::Promise>::try_from(result);
            if let Ok(promise) = promise {
                // Async — track as PendingRequest (owns the reply sender)
                let promise_global = v8::Global::new(scope, promise);
                self.pending_requests.insert(id, PendingRequest {
                    id,
                    promise: promise_global,
                    reply,
                    cancel,
                    wall_start: Instant::now(),
                });
            } else {
                // Sync — extract result and reply immediately
                let result = extract_sync_result(scope, &self.state, id, result);
                let _ = reply.send(result);
            }
        } else {
            let _ = reply.send(Err("dispatch returned None (JS exception)".to_string()));
        }
    }
}
```

## Streaming HTTP (two-phase response)

<!-- Added in round 1: addressing critic's point about isolate held during streaming -->

The current `execute_http` returns `HttpResult { body: String }` — the complete body. For true
SSE/AI streaming, we need a two-phase pattern that releases the V8 isolate after headers are
ready, then streams body chunks through Rust-only channels.

**Phase 1 (JS):** Handler returns Response with ReadableStream body. V8 extracts headers +
stream_id and returns them to the HTTP layer immediately.

**Phase 2 (Rust-only):** Body chunks arrive via event channel, forwarded to the HTTP body
channel. V8 is not involved after Phase 1 for pass-through streaming.

```rust
/// Streaming HTTP result — headers arrive first, body streams.
pub struct HttpStreamResult {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body_rx: tokio::sync::mpsc::Receiver<Vec<u8>>,  // bounded at STREAM_BODY_CAPACITY
    pub cpu_time: Duration,
    pub wall_time: Duration,
}

impl Runtime {
    /// Execute HTTP with streaming response.
    /// Returns immediately when headers are ready.
    /// Body chunks arrive via body_rx.
    pub fn execute_http_streaming(
        &mut self, method: &str, url: &str, headers: &str, body: &str,
    ) -> Option<Result<HttpStreamResult, String>> {
        self.ensure_initialized();
        self.drain_stale_state();

        // Dispatch into V8 — handler returns Response
        let (result, is_promise) = self.dispatch_http(method, url, headers, body)?;

        if is_promise {
            self.run_until_settled(&result, Duration::from_secs(30)).ok()?;
        }

        // Extract headers + stream_id from the Response object
        let (status, resp_headers, stream_id) = self.extract_http_headers(&result)?;

        // Create bounded body channel
        let (body_tx, body_rx) = tokio::sync::mpsc::channel(STREAM_BODY_CAPACITY);

        // Register a stream forwarder: when StreamChunk events arrive for this
        // stream_id, forward data to body_tx instead of into JS.
        // This is the "DeferredProxy" — V8 is NOT involved after this point.
        self.register_stream_forwarder(stream_id, body_tx);

        Some(Ok(HttpStreamResult {
            status,
            headers: resp_headers,
            body_rx,
            cpu_time: self.cpu_time_since_start(),
            wall_time: self.wall_time_since_start(),
        }))
    }

    /// For pass-through streaming (fetch -> Response with same stream body):
    /// The body_rx connects directly from the fetch task's output to the HTTP
    /// response body. The data path is: origin server -> reqwest -> tokio task ->
    /// event_tx -> event_rx -> stream forwarder -> body_tx -> body_rx -> hyper -> client.
    /// V8 touches none of it after Phase 1.
    fn register_stream_forwarder(
        &mut self,
        stream_id: u32,
        body_tx: tokio::sync::mpsc::Sender<Vec<u8>>,
    ) {
        // Store as StreamForwarder with overflow buffer for backpressure
        self.stream_forwarders.insert(stream_id, StreamForwarder {
            sender: body_tx,
            overflow: VecDeque::new(),
            max_overflow: 64,  // from RuntimeConfig
        });
    }
}
```

The v8-server HTTP handler writes each chunk to the TCP connection:

```rust
let result = runtime.execute_http_streaming("GET", url, headers, body)?;

// Send headers immediately
let response = Response::builder()
    .status(result.status)
    .headers(result.headers)
    .body(StreamBody::new(result.body_rx))?;

// hyper writes chunks to client as they arrive via body_rx
// Backpressure: if client reads slowly, body_rx fills up, body_tx.send() blocks,
// which blocks the stream forwarder, which causes event_rx to fill, which blocks
// the fetch task's event_tx.send(), which pauses the upstream read.
```

## Memory limits

<!-- Added in round 1: addressing critic's point about Rust-side buffer limits -->

V8 heap limits are set via `v8::Isolate::set_heap_limits()`, but Rust-side buffers (stream
chunks, log buffers, pending events) also need limits to prevent OOM.

```rust
/// Per-stream buffer limit. If a stream accumulates more than this without being
/// read by JS, we pause the producer (backpressure) or abort the stream.
const MAX_STREAM_BUFFER_BYTES: usize = 4 * 1024 * 1024;  // 4 MB

/// Per-request log buffer limit.
const MAX_LOG_ENTRIES_PER_REQUEST: usize = 1_000;

/// Maximum pending resolvers (outstanding async ops).
const MAX_PENDING_OPS: usize = 1_024;

impl RuntimeState {
    fn check_stream_memory(&self, stream_id: u32) -> bool {
        if let Some(stream) = self.streams.get(&stream_id) {
            stream.buffered_bytes < MAX_STREAM_BUFFER_BYTES
        } else {
            false
        }
    }
}
```

## Error propagation for broken pipes

<!-- Added in round 1: addressing critic's point about broken pipe errors -->

When a stream chunk send fails (channel closed because client disconnected), we must
propagate the error back to close the stream in JS and cancel any upstream fetch.

The `forward_stream_chunk` free function (defined in the StreamForwarder section above)
handles all three cases:
- **Ok**: chunk forwarded successfully.
- **Full**: chunk buffered in `StreamForwarder.overflow` (bounded VecDeque), retried in tick() PHASE 7.
  No data is lost unless the overflow is also full, in which case the stream is explicitly closed.
- **Closed**: client disconnected. Forwarder removed, stream marked closed. The fetch task
  sees `event_tx.send()` fail on the next chunk and stops reading from the origin server.

## Graceful shutdown

<!-- Added in round 1: addressing critic's point about graceful shutdown -->

When the server receives SIGTERM or a shutdown signal:

```rust
impl Runtime {
    /// Initiate graceful shutdown.
    /// 1. Stop accepting new requests
    /// 2. Cancel all pending fetch/WS tasks (via CancellationToken)
    /// 3. Drain pending requests with a timeout
    /// 4. Flush log buffers
    /// 5. Drop V8 isolate
    pub fn shutdown(&mut self, timeout: Duration) {
        let deadline = Instant::now() + timeout;

        // Cancel all active requests' background tasks
        {
            let state = self.state.borrow();
            for (_, ctx) in &state.active_requests {
                ctx.cancel.cancel();
            }
        }

        // Drain remaining events until all requests settle or timeout
        while self.has_pending_work() && Instant::now() < deadline {
            self.tick();

            if self.has_pending_work() {
                // Wait buffers events internally; drain_ready() picks them up next tick
                self.driver.wait_for_events(deadline);
            }
        }

        // Flush logs for any remaining requests
        {
            self.state.borrow_mut().active_requests.clear();
            for (id, req) in self.pending_requests.drain() {
                // Send whatever we have — partial result is better than silence
                let _ = req.reply.send(Err(format!("shutdown: request {} aborted", id)));
            }
        }

        // V8 isolate dropped when self is dropped
    }
}
```

This matches the Deno pattern where `op_fetch` tasks are cancelled via `AbortSignal` on shutdown,
and Cloudflare Workers' 30-second grace period for in-flight requests.

## CPU enforcement

<!-- Added in round 3: addressing missing CPU enforcement -->

The existing `cpu_timer.rs` (322 LOC) implements a POSIX CPU timer with signal handler and
watchdog thread. The Runtime integrates it as follows:

1. **Arm** the CPU timer before entering V8 (tick() PHASE 0).
2. V8 executes JS. If CPU time exceeds the limit, the watchdog thread fires SIGALRM.
3. The SIGALRM handler calls `v8::Isolate::terminate_execution()`, which causes the
   current JS execution to throw an uncatchable exception.
4. **Disarm** the timer after V8 work completes (tick() PHASE 8).
5. **Check termination** (tick() PHASE 9): if `is_execution_terminating()` is true,
   cancel termination, error all pending requests with "CPU time limit exceeded".

This matches Cloudflare Workers' CPU time limits (10ms for free, 50ms for paid) and
Deno Deploy's per-request CPU budgets. The timer measures actual CPU time (not wall time),
so I/O waits do not count against the limit.

The `cpu_limit` field on `Runtime` is set from `RuntimeConfig` and can differ per app tier.

## WebSocket (future)

WebSocket fits naturally as additional LoopEvent variants (shown above in the Event types
section). The `handle_event` match arm dispatches to `crate::ws::push_ws_message` etc.
The WebSocket connection is managed by a tokio task that sends events through the same
bounded event channel, with cancellation support via the request's CancellationToken.

## File structure (after refactor)

```
crates/runtime/src/
|
+-- lib.rs              Public API: Runtime, ModuleEntry, RequestResult, HttpStreamResult
|
+-- runtime.rs          Runtime struct + tick + run_until_settled + poll_event_loop
|                       V8State, EventDriver, StreamForwarder structs
|                       Free functions: handle_event, check_settled_promises,
|                       check_cancelled_requests, cleanup_request_state, fire_ready_timers
|                       Methods: has_pending_work, is_settled (ONCE each)
|
+-- state.rs            RuntimeState, SharedState, RequestContext
|                       LoopEvent enum, WsMessageData enum
|                       Channel capacity constants, memory limit constants
|
+-- request.rs          execute_request, execute_http, execute_http_streaming, accept_request
|                       dispatch_rpc, dispatch_http, extract_http_result
|                       Stream forwarder logic
|
+-- init.rs             ensure_initialized, setup_globals, load_polyfills, load_modules
|
+-- modules.rs          ESM module loader (compile, instantiate, evaluate, resolve)
|
+-- timers.rs           TimerState, fire_ready_timers, setTimeout/setInterval callbacks
|
+-- streams.rs          StreamState, push_stream_chunk, stream native callbacks
|
+-- crypto.rs           13 crypto ops + CSPRNG buffer + volatile zeroize
+-- fetch.rs            __rawFetch native callback + streaming body + cancellation
+-- url.rs              __urlParse, __urlCanParse native callbacks
+-- kv.rs               kv.get/set/delete/list native callbacks
+-- env.rs              env.get native callback
+-- ops.rs              OpError/OpErrorKind
|
+-- cpu_timer.rs        POSIX CPU timer (Linux)
+-- storage.rs          AppStorage (versioned builds, deploy, rollback)
|
+-- embed/
|   +-- fetch.js        Headers, Request, Response, fetch, TextEncoder, btoa/atob
|   +-- url.js          URL, URLSearchParams
|   +-- crypto.js       SubtleCrypto, CryptoKey, getRandomValues
|   +-- streams.js      ReadableStream, ReadableStreamDefaultController/Reader
|
+-- server.rs           v8-server binary (hyper HTTP)
```

### What merges

| Before | After | Why |
|---|---|---|
| `isolate.rs` (493 LOC) | Split into `runtime.rs` + `request.rs` + `init.rs` | Isolate was a god struct |
| `concurrent.rs` (943 LOC) | Merged into `runtime.rs` | Same event loop, different running mode |
| `globals.rs` (356 LOC) | Merged into `init.rs` | Global setup is part of initialization |
| `runtime.rs` (120 LOC) | Merged into `init.rs` + `lib.rs` | V8 init + constants are init concerns |
| `event_loop.rs` (353 LOC) | Rewritten as `runtime.rs` + `state.rs` | Clean split along borrow boundaries |

### What stays the same

| File | LOC | Change |
|---|---|---|
| `crypto.rs` | 1,164 | None — already clean |
| `fetch.rs` | 405 | Add cancellation token, bounded channel send |
| `modules.rs` | 355 | None |
| `streams.rs` | 286 | Add MAX_STREAM_BUFFER_BYTES check |
| `cpu_timer.rs` | 322 | None |
| `storage.rs` | 311 | None |
| `timers.rs` | 107 | None |
| `url.rs` | 62 | None |
| `kv.rs` | 28 | None |
| `env.rs` | 16 | None |
| `ops.rs` | 53 | None |
| `embed/*.js` | ~1,170 | None |

## Public API (what changes for consumers)

```rust
// Before:
use appbase_runtime::{Isolate, IsolatePool, init_v8, ModuleEntry, RequestResult};
let mut isolate = Isolate::new(modules, env_vars);
let result = isolate.execute_request(json)?;
let http = isolate.execute_http(method, url, headers, body)?;

// After:
use appbase_runtime::{Runtime, init_v8, ModuleEntry, RequestResult};
let mut runtime = Runtime::new(modules, env_vars);
let result = runtime.execute_request(json)?;
let http = runtime.execute_http(method, url, headers, body)?;

// NEW: streaming mode
let stream = runtime.execute_http_streaming(method, url, headers, body)?;
// headers available immediately in stream.status / stream.headers
// body chunks arrive via stream.body_rx

// NEW: non-blocking mode (for tokio integration)
let cancel = CancellationToken::new();
runtime.accept_request(id, body, reply_tx, cancel.clone());
poll_fn(|cx| runtime.poll_event_loop(cx)).await;

// NEW: graceful shutdown
runtime.shutdown(Duration::from_secs(5));
```

## Migration plan

1. **Create `state.rs`** with RuntimeState, RequestContext, LoopEvent, constants
2. **Create `runtime.rs` v2** with Runtime struct (V8State + EventDriver + SharedState), tick, handle_event
3. **Extract `request.rs`** from isolate.rs (dispatch + extract logic + stream forwarder)
4. **Extract `init.rs`** from isolate.rs + globals.rs + old runtime.rs
5. **Update `fetch.rs`** — bounded channel send, cancellation token
6. **Update `streams.rs`** — buffer size checks
7. **Delete** old isolate.rs, concurrent.rs, globals.rs, old runtime.rs, old event_loop.rs
8. **Update lib.rs** — new module declarations, re-exports
9. **Update server.rs** — use Runtime instead of ConcurrentIsolate, add cancellation
10. **Update platform** — v8pool.rs uses Runtime
11. **Run all 88 tests** — zero regressions
12. **Add new tests** — backpressure, slow client, cancellation, shutdown (see below)
13. **Benchmark** — verify performance maintained

## Success criteria

- All 88 existing tests pass
- Benchmark: >=330K req/s for sync RPC (no regression)
- SSE example: tokens stream to client word-by-word
- `handle_event` written exactly ONCE (as a free function)
- `has_pending_work` written exactly ONCE
- No duplicated event loop logic
- No method calls on `&self` while a V8 scope is alive (borrow conflict prevention)
- Every `self.field` reference in code examples corresponds to a declared struct field
- Crypto/KV/env types not in runtime.rs or state.rs
- **Backpressure test** — slow consumer does not cause unbounded memory growth; memory stays flat when a client reads at 1 byte/sec while origin streams at 100 MB/s
- **Data integrity test** — `try_send(Full)` buffers in overflow VecDeque and retries; no silent data loss (overflow-full triggers explicit stream close)
- **Slow client test** — 100 concurrent SSE clients at varying speeds, no OOM, correct ordering
- **Cancellation test** — client disconnect cancels in-flight fetch within 100ms
- **Shutdown test** — graceful shutdown completes within timeout, all replies sent
- **Per-request isolation test** — console.log output from request A does not appear in request B's logs (no global current_request_id)
- **Memory limit test** — stream exceeding MAX_STREAM_BUFFER_BYTES triggers backpressure or abort, not OOM
- **CPU enforcement test** — infinite loop in JS terminates within cpu_limit; all pending requests receive "CPU time limit exceeded" error
