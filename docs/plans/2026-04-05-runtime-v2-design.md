# Runtime v2 Design — Non-blocking Event Loop

**Date:** 2026-04-05
**Goal:** Refactor the runtime into a clean non-blocking architecture based on tokio, enabling streaming, WebSocket, and multi-isolate-per-thread.

## Why

The current runtime was built incrementally across multiple sessions:
- `Isolate` (per-request, blocking)
- `ConcurrentIsolate` (multi-request, blocking recv_timeout)
- `EventLoopInner` god struct (crypto keys, KV, env, timers, streams, resolvers)
- `OpResult` → `LoopEvent` (bolted on for streaming)
- Two nearly identical event loop drivers sharing ~80% logic
- Event handling code duplicated 3 times

It works (88 tests, 337K req/s) but can't support:
- Chunked HTTP responses (true SSE/AI streaming to client)
- WebSocket
- Streaming request bodies
- Multiple isolates per thread
- Non-blocking I/O integration

## Architecture

### Core type: `EventLoop`

One type replaces `Isolate`, `ConcurrentIsolate`, and the free event loop functions:

```rust
pub struct EventLoop {
    // --- V8 (owned, !Send, never crosses threads) ---
    isolate: v8::OwnedIsolate,
    context: v8::Global<v8::Context>,
    dispatch_fn: Option<v8::Global<v8::Function>>,
    on_request_fn: Option<v8::Global<v8::Function>>,
    http_dispatch_fn: Option<v8::Global<v8::Function>>,
    modules: Vec<ModuleEntry>,
    initialized: bool,

    // --- Event sources (tokio-integrated) ---
    event_rx: tokio::sync::mpsc::UnboundedReceiver<LoopEvent>,
    event_tx: tokio::sync::mpsc::UnboundedSender<LoopEvent>,
    local_rt: tokio::runtime::Runtime,        // current-thread, on V8 thread

    // --- Shared with V8 callbacks (Rc<RefCell<>>) ---
    state: SharedState,

    // --- Timer integration ---
    timer_sleep: Option<Pin<Box<tokio::time::Sleep>>>,

    // --- Request tracking (for concurrent mode) ---
    pending_requests: HashMap<u64, PendingRequest>,

    // --- CPU enforcement (Linux) ---
    #[cfg(target_os = "linux")]
    cpu_timer: Option<CpuTimer>,
}
```

### State separation

```rust
/// Shared with V8 callbacks via isolate slot.
/// Borrows are brief (microseconds), never held during blocking.
pub(crate) struct RuntimeState {
    // Event loop
    pub(crate) timers: TimerState,
    pub(crate) pending_resolvers: HashMap<u32, v8::Global<v8::PromiseResolver>>,
    pub(crate) streams: HashMap<u32, StreamState>,
    pub(crate) next_op_id: u32,
    pub(crate) next_stream_id: u32,

    // Event channel (sender only — receiver is outside RefCell)
    pub(crate) event_tx: tokio::sync::mpsc::UnboundedSender<LoopEvent>,

    // Tokio handle for spawning background tasks
    pub(crate) server_handle: Option<tokio::runtime::Handle>,

    // Concurrent model (optional)
    pub(crate) concurrent_event_tx: Option<std::sync::mpsc::Sender<ConcurrentEvent>>,

    // App state
    pub(crate) log_buffer: Vec<String>,
    pub(crate) kv_store: HashMap<String, String>,
    pub(crate) env_vars: HashMap<String, String>,
    pub(crate) key_store: HashMap<u32, KeyData>,
    pub(crate) next_key_id: u32,
}

pub(crate) type SharedState = Rc<RefCell<RuntimeState>>;
```

### Event types

```rust
pub(crate) enum LoopEvent {
    OpCompleted { id: u32, value: String },
    StreamChunk { stream_id: u32, data: Vec<u8>, done: bool },
}
```

### The poll-based tick

```rust
impl EventLoop {
    /// One tick of the event loop.
    /// Called by tokio executor via poll_fn, or directly in blocking mode.
    fn tick(&mut self, cx: Option<&mut Context<'_>>) -> bool {
        v8::scope!(let handle_scope, &mut self.isolate);
        let context = v8::Local::new(handle_scope, &self.context);
        let scope = &mut v8::ContextScope::new(handle_scope, context);

        // Phase 1: DRAIN — non-blocking try_recv of all ready events
        while let Ok(event) = self.event_rx.try_recv() {
            self.handle_event(scope, event);
        }

        // Phase 2: TIMERS — fire all ready timers
        fire_ready_timers(scope, &self.state);

        // Phase 3: MICROTASKS — flush V8 microtask queue
        scope.perform_microtask_checkpoint();

        // Phase 4: CHECK — check settled promises, send replies
        self.check_settled_promises(scope);

        // V8 scope drops here — safe to yield/block after this point
        self.has_pending_work()
    }

    /// Handle one event (written ONCE, used everywhere)
    fn handle_event(&self, scope: &mut v8::PinScope, event: LoopEvent) {
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
        }
        scope.perform_microtask_checkpoint();
    }

    /// Is there pending work?
    fn has_pending_work(&self) -> bool {
        let s = self.state.borrow();
        !s.timers.callbacks.is_empty()
            || !s.pending_resolvers.is_empty()
            || s.streams.values().any(|st| st.pending_read.is_some() && !st.closed)
    }
}
```

### Two running modes (same tick, different wait)

```rust
impl EventLoop {
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
            if !self.tick(None) { return Ok(()); }

            // Wait — block_on select! for instant wake
            let timeout = self.compute_timeout(deadline);
            self.local_rt.block_on(async {
                tokio::select! {
                    event = self.event_rx.recv() => {
                        if let Some(e) = event { self.buffer_event(e); }
                    }
                    _ = tokio::time::sleep(timeout) => {}
                }
            });
        }
    }

    // === Mode 2: Non-blocking (for tokio integration) ===

    /// Poll-based tick for integration with tokio executor.
    /// Returns Poll::Pending when waiting, Poll::Ready when done.
    pub fn poll_event_loop(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<()> {
        if !self.tick(Some(cx)) {
            return Poll::Ready(());
        }

        // Register wakers
        // Channel waker: fires when event arrives
        match self.event_rx.poll_recv(cx) {
            Poll::Ready(Some(event)) => {
                self.buffer_event(event);
                cx.waker().wake_by_ref(); // re-poll immediately
                return Poll::Pending;
            }
            Poll::Ready(None) => return Poll::Ready(()), // channel closed
            Poll::Pending => {} // waker registered
        }

        // Timer waker: fires at next timer deadline
        if let Some(deadline) = self.next_timer_deadline() {
            let sleep = self.timer_sleep.get_or_insert_with(|| {
                Box::pin(tokio::time::sleep_until(deadline.into()))
            });
            sleep.as_mut().reset(deadline.into());
            let _ = sleep.as_mut().poll(cx);
        }

        Poll::Pending
    }
}
```

### Request handling

```rust
impl EventLoop {
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
    pub fn accept_request(&mut self, id: u64, body: String, reply: oneshot::Sender<Result<RequestResult, String>>) {
        self.ensure_initialized();
        // ... dispatch, store pending request
    }
}
```

## File structure (after refactor)

```
crates/runtime/src/
│
├── lib.rs              Public API: EventLoop, ModuleEntry, RequestResult
│
├── event_loop.rs       EventLoop struct + poll_tick + run_until_settled + poll_event_loop
│                       LoopEvent enum, RuntimeState, SharedState
│                       handle_event (ONCE), has_pending_work (ONCE), is_settled (ONCE)
│
├── request.rs          execute_request, execute_http, accept_request
│                       dispatch_rpc, dispatch_http, extract_http_result
│                       (all request handling, extracted from isolate.rs)
│
├── init.rs             ensure_initialized, setup_globals, load_polyfills, load_modules
│                       (all initialization, extracted from isolate.rs + globals.rs)
│
├── modules.rs          ESM module loader (compile, instantiate, evaluate, resolve)
│
├── timers.rs           TimerState, fire_ready_timers, setTimeout/setInterval callbacks
│
├── streams.rs          StreamState, push_stream_chunk, stream native callbacks
│
├── crypto.rs           13 crypto ops + CSPRNG buffer + volatile zeroize
├── fetch.rs            __rawFetch native callback + streaming body
├── url.rs              __urlParse, __urlCanParse native callbacks
├── kv.rs               kv.get/set/delete/list native callbacks
├── env.rs              env.get native callback
├── ops.rs              OpError/OpErrorKind
│
├── cpu_timer.rs        POSIX CPU timer (Linux)
├── storage.rs          AppStorage (versioned builds, deploy, rollback)
│
├── embed/
│   ├── fetch.js        Headers, Request, Response, fetch, TextEncoder, btoa/atob
│   ├── url.js          URL, URLSearchParams
│   ├── crypto.js       SubtleCrypto, CryptoKey, getRandomValues
│   └── streams.js      ReadableStream, ReadableStreamDefaultController/Reader
│
└── server.rs           v8-server binary (hyper HTTP)
```

### What merges

| Before | After | Why |
|---|---|---|
| `isolate.rs` (493 LOC) | Split into `event_loop.rs` + `request.rs` + `init.rs` | Isolate was a god struct — init, dispatch, event loop, request handling |
| `concurrent.rs` (943 LOC) | Merged into `event_loop.rs` | Same event loop, different running mode (blocking vs non-blocking) |
| `globals.rs` (356 LOC) | Merged into `init.rs` | Global setup is part of initialization |
| `runtime.rs` (120 LOC) | Merged into `init.rs` + `lib.rs` | V8 init + constants are init concerns |
| `event_loop.rs` (353 LOC) | Rewritten as `event_loop.rs` | Clean EventLoop struct with poll_tick |

### What stays the same

| File | LOC | Change |
|---|---|---|
| `crypto.rs` | 1,164 | None — already clean |
| `fetch.rs` | 405 | Minor — use new event_tx type |
| `modules.rs` | 355 | None |
| `streams.rs` | 286 | Minor — push_stream_chunk stays same |
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
use appbase_runtime::{EventLoop, init_v8, ModuleEntry, RequestResult};
let mut event_loop = EventLoop::new(modules, env_vars);
let result = event_loop.execute_request(json)?;
let http = event_loop.execute_http(method, url, headers, body)?;

// NEW: non-blocking mode (for tokio integration)
let mut event_loop = EventLoop::new(modules, env_vars);
event_loop.accept_request(id, body, reply_tx);
poll_fn(|cx| event_loop.poll_event_loop(cx)).await;
```

## Streaming HTTP (chunked response)

The current `execute_http` returns `HttpResult { body: String }` — the complete body. For true SSE:

```rust
/// Streaming HTTP result — headers arrive first, body streams.
pub struct HttpStreamResult {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body_rx: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    pub cpu_time: Duration,
    pub wall_time: Duration,
}

impl EventLoop {
    /// Execute HTTP with streaming response.
    /// Returns immediately when headers are ready.
    /// Body chunks arrive via body_rx.
    pub fn execute_http_streaming(
        &mut self, method: &str, url: &str, headers: &str, body: &str,
    ) -> Option<Result<HttpStreamResult, String>> {
        // ...
    }
}
```

The v8-server HTTP handler writes each chunk to the TCP connection:

```rust
let result = event_loop.execute_http_streaming("GET", url, headers, body)?;

// Send headers
let response = Response::builder()
    .status(result.status)
    .headers(result.headers)
    .body(StreamBody::new(result.body_rx))?;

// Chunks written to client as they arrive
```

## WebSocket (future)

```rust
enum LoopEvent {
    OpCompleted { id: u32, value: String },
    StreamChunk { stream_id: u32, data: Vec<u8>, done: bool },
    WebSocketMessage { ws_id: u32, data: Vec<u8> },     // NEW
    WebSocketClose { ws_id: u32, code: u16 },            // NEW
}
```

WebSocket fits naturally — just another LoopEvent variant. The `poll_tick` handles it like any other event.

## Migration plan

1. **Create `event_loop.rs` v2** with EventLoop struct, poll_tick, handle_event
2. **Extract `request.rs`** from isolate.rs (dispatch + extract logic)
3. **Extract `init.rs`** from isolate.rs + globals.rs + runtime.rs
4. **Delete** old isolate.rs, concurrent.rs, globals.rs, runtime.rs
5. **Update lib.rs** — new module declarations, re-exports
6. **Update server.rs** — use EventLoop instead of ConcurrentIsolate
7. **Update platform** — v8pool.rs uses EventLoop
8. **Run all 88 tests** — zero regressions
9. **Benchmark** — verify performance maintained

## Success criteria

- All 88 existing tests pass
- Benchmark: ≥330K req/s for sync RPC (no regression)
- SSE example: tokens stream to client word-by-word
- `handle_event` written exactly ONCE
- `has_pending_work` written exactly ONCE
- No duplicated event loop logic
- Crypto/KV/env types not in event_loop.rs
