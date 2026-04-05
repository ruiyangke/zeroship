# Runtime v2 Design — Non-blocking Event Loop

**Date:** 2026-04-05
**Revision:** 2 (Round 1 review incorporated)
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
- Streaming request bodies
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

    // Concurrent model (optional)
    pub(crate) concurrent_event_tx: Option<std::sync::mpsc::Sender<ConcurrentEvent>>,

    // Per-request context (see "Per-request isolation" section)
    pub(crate) active_requests: HashMap<u64, RequestContext>,
    pub(crate) current_request_id: Option<u64>,

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
    pending_requests: HashMap<u64, PendingRequest>,

    // CPU enforcement (Linux)
    #[cfg(target_os = "linux")]
    cpu_timer: Option<CpuTimer>,
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
/// Per-request context — created on accept, destroyed on reply.
struct RequestContext {
    id: u64,
    log_buffer: Vec<String>,
    cpu_start: Duration,
    wall_start: Instant,
    reply: oneshot::Sender<Result<RequestResult, String>>,
    cancel: tokio_util::sync::CancellationToken,
}
```

Console.log routes to the active request's buffer:
```rust
// In the console.log native callback:
fn console_log_callback(scope: &mut v8::HandleScope, args: v8::FunctionCallbackArguments, _rv: v8::ReturnValue) {
    let state = get_state(scope);
    let mut s = state.borrow_mut();
    let msg = args.get(0).to_rust_string_lossy(scope);

    if let Some(req_id) = s.current_request_id {
        if let Some(ctx) = s.active_requests.get_mut(&req_id) {
            ctx.log_buffer.push(msg);
        }
    }
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

// Event loop checks cancellation during tick:
impl Runtime {
    fn check_cancelled_requests(&mut self, scope: &mut v8::HandleScope) {
        let state = self.state.borrow();
        let cancelled: Vec<u64> = state.active_requests.iter()
            .filter(|(_, ctx)| ctx.cancel.is_cancelled())
            .map(|(id, _)| *id)
            .collect();
        drop(state);

        for id in cancelled {
            self.cleanup_request(scope, id);
        }
    }
}
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
    // Core ops
    OpCompleted { id: u32, value: String },

    // Streaming
    StreamChunk { stream_id: u32, data: Vec<u8>, done: bool },

    // Phase 2: WebSocket
    WsMessage { ws_id: u32, data: WsMessageData },
    WsClose { ws_id: u32, code: u16, reason: String },
    WsPing { ws_id: u32 },

    // Phase 3: Request lifecycle
    RequestCancelled { request_id: u64 },
}

pub(crate) enum WsMessageData {
    Text(String),
    Binary(Vec<u8>),
}
```

### The poll-based tick

<!-- Added in round 1: addressing critic's point about mutable aliasing in tick -->

The key insight: drain events into a `Vec<LoopEvent>` BEFORE creating the V8 scope.
Then process from the Vec. The V8 scope borrows `&mut self.v8` but the driver is accessed
before and after — never during.

```rust
impl EventDriver {
    /// Drain all currently-ready events without blocking.
    fn drain_ready(&mut self) -> Vec<LoopEvent> {
        let mut events = Vec::new();
        while let Ok(event) = self.event_rx.try_recv() {
            events.push(event);
        }
        events
    }

    /// Block until at least one event arrives or deadline expires.
    fn wait_for_events(&mut self, deadline: Instant) -> Vec<LoopEvent> {
        let timeout = deadline.saturating_duration_since(Instant::now());
        let mut events = Vec::new();

        self.local_rt.block_on(async {
            tokio::select! {
                event = self.event_rx.recv() => {
                    if let Some(e) = event {
                        events.push(e);
                    }
                }
                _ = async {
                    // Timer sleep: wake at next timer deadline or wall timeout
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
            events.push(event);
        }

        events
    }
}

impl Runtime {
    /// One tick of the event loop.
    fn tick(&mut self) -> bool {
        // PHASE 0: DRAIN — get events from driver BEFORE creating V8 scope.
        // This is safe because no V8 scope exists yet.
        let events = self.driver.drain_ready();

        // NOW create V8 scope — borrows &mut self.v8 exclusively.
        let handle_scope = &mut v8::HandleScope::new(&mut self.v8.isolate);
        let context = v8::Local::new(handle_scope, &self.v8.context);
        let scope = &mut v8::ContextScope::new(handle_scope, context);

        // PHASE 1: PROCESS — handle buffered events (no driver access needed)
        for event in events {
            self.handle_event(scope, event);
        }

        // PHASE 2: TIMERS — fire all ready timers
        fire_ready_timers(scope, &self.state);

        // PHASE 3: MICROTASKS — flush V8 microtask queue
        scope.perform_microtask_checkpoint();

        // PHASE 4: CHECK — check settled promises, send replies
        self.check_settled_promises(scope);

        // PHASE 5: CANCELLATION — clean up cancelled requests
        self.check_cancelled_requests(scope);

        // V8 scope drops here — safe to access driver again
        self.has_pending_work()
    }

    /// Handle one event (written ONCE, used everywhere).
    fn handle_event(&self, scope: &mut v8::HandleScope, event: LoopEvent) {
        match event {
            LoopEvent::OpCompleted { id, value } => {
                let resolver = self.state.borrow_mut().pending_resolvers.remove(&id);
                if let Some(resolver) = resolver {
                    let r = v8::Local::new(scope, &resolver);
                    let val = v8::String::new(scope, &value).unwrap();
                    r.resolve(scope, val.into());
                }
            }
            LoopEvent::StreamChunk { stream_id, data, done } => {
                crate::streams::push_stream_chunk(scope, &self.state, stream_id, &data, done);
            }
            LoopEvent::WsMessage { ws_id, data } => {
                crate::ws::push_ws_message(scope, &self.state, ws_id, data);
            }
            LoopEvent::WsClose { ws_id, code, reason } => {
                crate::ws::push_ws_close(scope, &self.state, ws_id, code, &reason);
            }
            LoopEvent::WsPing { ws_id } => {
                crate::ws::push_ws_pong(scope, &self.state, ws_id);
            }
            LoopEvent::RequestCancelled { request_id } => {
                self.cleanup_request_state(scope, request_id);
            }
        }
        scope.perform_microtask_checkpoint();
    }

    /// Is there pending work?
    fn has_pending_work(&self) -> bool {
        let s = self.state.borrow();
        !s.timers.callbacks.is_empty()
            || !s.pending_resolvers.is_empty()
            || s.streams.values().any(|st| st.pending_read.is_some() && !st.closed)
            || !self.pending_requests.is_empty()
    }
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
            if !self.tick() { return Ok(()); }

            // Wait — block until event arrives or timer fires
            let events = self.driver.wait_for_events(deadline);
            // Events will be picked up by drain_ready() in next tick()
            // But we can also buffer them directly:
            self.driver.buffer.extend(events);
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
        id: u64,
        body: String,
        reply: oneshot::Sender<Result<RequestResult, String>>,
        cancel: tokio_util::sync::CancellationToken,
    ) {
        self.ensure_initialized();

        // Create per-request context
        let ctx = RequestContext {
            id,
            log_buffer: Vec::new(),
            cpu_start: self.cpu_time(),
            wall_start: Instant::now(),
            reply,
            cancel,
        };
        self.state.borrow_mut().active_requests.insert(id, ctx);
        self.state.borrow_mut().current_request_id = Some(id);

        // Dispatch into V8
        // ... dispatch, store pending request
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
        // Store in a separate map — checked in handle_event before JS dispatch
        self.stream_forwarders.insert(stream_id, body_tx);
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

```rust
// In the stream forwarder:
fn forward_stream_chunk(
    forwarders: &mut HashMap<u32, mpsc::Sender<Vec<u8>>>,
    stream_id: u32,
    data: Vec<u8>,
    done: bool,
    state: &SharedState,
) {
    if let Some(tx) = forwarders.get(&stream_id) {
        // try_send: non-blocking, returns error if channel full or closed
        match tx.try_send(data) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Closed(_)) => {
                // Client disconnected — clean up the stream
                forwarders.remove(&stream_id);
                let mut s = state.borrow_mut();
                if let Some(stream) = s.streams.get_mut(&stream_id) {
                    stream.closed = true;
                }
                // The fetch task will see event_tx.send() fail on next chunk
                // and stop reading from the origin server.
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                // Backpressure — buffer is full, chunk will be retried next tick.
                // In practice, the bounded event channel already applies backpressure
                // before we get here.
            }
        }
    }

    if done {
        forwarders.remove(&stream_id);
    }
}
```

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
                let events = self.driver.wait_for_events(deadline);
                self.driver.buffer.extend(events);
            }
        }

        // Flush logs for any remaining requests
        {
            let mut state = self.state.borrow_mut();
            for (id, ctx) in state.active_requests.drain() {
                // Send whatever we have — partial result is better than silence
                let _ = ctx.reply.send(Err(format!("shutdown: request {} aborted", id)));
            }
        }

        // V8 isolate dropped when self is dropped
    }
}
```

This matches the Deno pattern where `op_fetch` tasks are cancelled via `AbortSignal` on shutdown,
and Cloudflare Workers' 30-second grace period for in-flight requests.

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
|                       V8State, EventDriver structs
|                       handle_event (ONCE), has_pending_work (ONCE), is_settled (ONCE)
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
- `handle_event` written exactly ONCE
- `has_pending_work` written exactly ONCE
- No duplicated event loop logic
- Crypto/KV/env types not in runtime.rs or state.rs
- **NEW: Backpressure test** — slow consumer does not cause unbounded memory growth; memory stays flat when a client reads at 1 byte/sec while origin streams at 100 MB/s
- **NEW: Slow client test** — 100 concurrent SSE clients at varying speeds, no OOM, correct ordering
- **NEW: Cancellation test** — client disconnect cancels in-flight fetch within 100ms
- **NEW: Shutdown test** — graceful shutdown completes within timeout, all replies sent
- **NEW: Per-request isolation test** — console.log output from request A does not appear in request B's logs
- **NEW: Memory limit test** — stream exceeding MAX_STREAM_BUFFER_BYTES triggers backpressure or abort, not OOM
