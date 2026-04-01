# Runtime V2 — Custom V8 Runtime Specification

> Built from scratch on raw V8 (rusty_v8). No deno_core.

## 1. Goals

1. **Concurrent I/O** — multiple requests in-flight per isolate, I/O waits overlap
2. **Per-request CPU** — exact CPU attribution via V8 promise hooks
3. **Per-request kill** — terminate one request without affecting others
4. **Multi-thread scaling** — 1 isolate per app, apps across threads
5. **Minimal dependencies** — v8, tokio, reqwest only
6. **Full control** — we own every line of the event loop, no opaque abstractions
7. **~1,500 LOC** — lean, auditable, no dead code

## 2. Non-Goals (V2)

- ESM module loading (use Script::compile)
- TypeScript at runtime (pre-compile via SWC at build time)
- Node.js compatibility
- WebAssembly
- Inspector/debugger protocol
- Snapshot-based cold start (future V3)

## 3. Execution Model

### 3.1 Request Lifecycle

```
HTTP request arrives (tokio)
    |
    v
Dispatcher (round-robin or per-app affinity)
    |
    v
Worker thread (1 per app, persistent V8 isolate)
    |
    v
Inject request into V8 event loop (fire-and-forget)
    |
    +---> JS handler called (async or sync)
    |       |
    |       +---> await fetch(url) ──> event loop yields
    |       |                          other requests run
    |       |                          fetch completes
    |       +---> return result
    |
    v
Response sent via oneshot channel
```

### 3.2 Concurrency Within One Isolate

```
1 V8 thread, N concurrent requests, event loop multiplexes:

  time -->

  [Before(A)] A runs JS   [After(A)] A yields (await fetch)
                [Before(B)] B runs JS   [After(B)] B yields (await db)
                                [Before(C)] C runs JS   [After(C)] C returns
  [Before(A)] A resumes JS [After(A)] A returns
                [Before(B)] B resumes JS [After(B)] B returns

  All I/O overlaps. JS execution serialized.
  Promise hooks see every transition.
```

### 3.3 Promise Hooks — Per-Request Tracking

```rust
// V8 calls this on every promise lifecycle event
fn promise_hook(type: PromiseHookType, promise, parent) {
    match type {
        Init => {
            // New promise created. Tag with parent's request_id.
            let parent_id = get_request_id(parent);
            set_request_id(promise, parent_id);
        }
        Before => {
            // About to run this promise's handler.
            let req_id = get_request_id(promise);
            CURRENT_REQUEST.set(req_id);
            CPU_START.set(thread_cpu_time());
        }
        After => {
            // Handler finished (yielded or returned).
            let delta = thread_cpu_time() - CPU_START.get();
            let req_id = CURRENT_REQUEST.get();
            requests[req_id].cpu += delta;

            // Check budget
            if requests[req_id].cpu > budget {
                kill_request(req_id);
            }
            CURRENT_REQUEST.set(0);
        }
        Resolve => {
            // Promise resolved. Not needed for CPU tracking.
        }
    }
}
```

### 3.4 Per-Request Kill

```
Soft kill (budget exceeded during a yield point):
  After hook detects: request A over budget
  → reject A's promise chain with "CPU limit exceeded"
  → A's reply gets error
  → B, C continue unaffected

Hard kill (infinite loop — hooks can't fire):
  POSIX CPU timer fires (50ms)
  → signal handler → pipe → watchdog thread
  → terminate_execution()
  → V8 unwinds current stack
  → cancel_terminate_execution()
  → promise hook tells us it was request A
  → drain A's reply with error
  → re-enter event loop
  → B, C continue
```

## 4. Event Loop

### 4.1 Phases (Node.js/libuv inspired)

```
loop {
    // Phase 1: Microtasks
    isolate.perform_microtask_checkpoint();

    // Phase 2: Timers
    while timer_heap.peek().fire_at <= now {
        fire timer callback
        microtask checkpoint
    }

    // Phase 3: Pending async ops (fetch, DB results)
    while let Ok(result) = op_rx.try_recv() {
        resolve promise
        microtask checkpoint
    }

    // Phase 4: Incoming requests
    while let Ok(request) = request_rx.try_recv() {
        call dispatch function (fire-and-forget)
        microtask checkpoint
    }

    // Phase 5: Check done
    if no timers && no pending ops && no pending requests {
        // idle — wait for next request
    }

    // Phase 6: Wait
    // Block until: timer fires, op completes, or new request arrives
    select {
        timer_timeout => continue
        op_result => continue
        new_request => continue
    }
}
```

### 4.2 Wait Mechanism

Phase 6 blocks the V8 thread with zero CPU. Three wake sources:

```
Timer:       std::thread::sleep(duration_until_next_timer)
Async op:    op_rx.recv_timeout(timeout)
New request: request_rx.recv_timeout(timeout)
```

All three channels merged into one `std::sync::mpsc`:

```rust
enum Event {
    TimerFired(u32),              // from timer heap (immediate)
    OpCompleted(u32, String),     // from tokio async op thread
    NewRequest(u64, String, oneshot::Sender<Result>),  // from HTTP
}
```

Single `recv_timeout` handles all wake sources.

### 4.3 Timer Implementation

```
Storage: BinaryHeap<Reverse<TimerEntry>> (min-heap)
Insert: O(log n)
Peek next: O(1)
Fire: O(log n) pop
Cancel: lazy deletion (remove callback, skip stale entries)
```

No spawned tasks per timer. Compute sleep duration from heap.peek().

## 5. Async Op System

### 5.1 Architecture

```
JS calls op:
  globalThis.fetch(url)
    → Rust callback creates Promise + Resolver
    → Stores Resolver as Global<PromiseResolver> keyed by op_id
    → Spawns async task on shared tokio runtime
    → Returns Promise to JS (handler continues or awaits)

Async task completes:
  tokio task → op_tx.send(Event::OpCompleted(op_id, json))
  → event loop wakes from recv_timeout
  → Phase 3: resolve promise with result
  → microtask checkpoint (runs .then chains)
```

### 5.2 Registering an Op

```rust
fn register_fetch(scope: &mut v8::HandleScope) {
    let fetch_fn = v8::Function::new(scope, |scope, args, rv| {
        let url = args.get(0).to_string(scope);
        let state = get_state(scope);

        // Create promise
        let resolver = v8::PromiseResolver::new(scope);
        let promise = resolver.get_promise(scope);

        // Store resolver
        let op_id = state.next_op_id();
        state.pending_resolvers.insert(op_id, Global::new(scope, resolver));

        // Spawn async work
        let tx = state.op_tx.clone();
        tokio_runtime().spawn(async move {
            let resp = reqwest::get(&url).await;
            let json = /* serialize response */;
            tx.send(Event::OpCompleted(op_id, json)).await;
        });

        // Return promise to JS
        rv.set(promise.into());
    });

    let key = v8::String::new(scope, "fetch").unwrap();
    scope.global().set(scope, key, fetch_fn);
}
```

### 5.3 Built-in Ops (V2 Scope)

| Op | JS API | Rust implementation |
|---|---|---|
| `fetch` | `fetch(url, opts)` | reqwest on tokio |
| `setTimeout` | `setTimeout(fn, ms)` | min-heap timer |
| `clearTimeout` | `clearTimeout(id)` | lazy deletion |
| `setInterval` | `setInterval(fn, ms)` | repeating timer |
| `console.log` | `console.log(...)` | stdout print |
| `TextEncoder` | `new TextEncoder()` | V8 built-in or polyfill |
| `TextDecoder` | `new TextDecoder()` | V8 built-in or polyfill |
| `URL` | `new URL(...)` | Rust url crate or polyfill |
| `crypto.randomUUID` | `crypto.randomUUID()` | uuid crate |
| `atob/btoa` | `atob(s)` / `btoa(s)` | base64 encode/decode |

### 5.4 Plugin Ops (Extensible)

```rust
trait RuntimePlugin {
    fn name(&self) -> &str;
    fn register(&self, scope: &mut v8::HandleScope, state: &SharedState);
}

// DB plugin registers:
//   globalThis.db.find(table, filter) → async op
//   globalThis.db.insert(table, data) → async op
```

## 6. Isolate Management

### 6.1 Per-App Isolate (Long-Lived)

```
App "todo-app":
  1 V8 isolate (created once)
  1 persistent context (JS state persists across requests)
  1 worker thread (dedicated)
  N concurrent requests in-flight (event loop multiplexes)
```

### 6.2 Isolate Pool

```
IsolateManager:
  apps: HashMap<AppId, WorkerHandle>

  dispatch(app_id, request) → WorkerHandle.send(request)

  WorkerHandle:
    request_tx: mpsc::Sender<Event>  // inject requests
    thread: JoinHandle               // the V8 worker thread
```

### 6.3 Lifecycle

```
1. App first request → create isolate + worker thread
2. Load server.js (compiled once, persistent context)
3. Start event loop (blocks on recv_timeout)
4. Requests arrive → inject via channel → fire-and-forget dispatch
5. Idle timeout → evict isolate (configurable)
6. Memory pressure → evict LRU isolates
7. Shutdown → drain in-flight, close channel, join thread
```

## 7. CPU Enforcement

### 7.1 Three Layers

```
Layer 1: Promise hooks (soft, per-request, every yield point)
  Before/After: measure CPU slice, accumulate per request
  At After: if request.cpu > budget → reject promise chain
  Precision: per-yield-point (typically every few ms)

Layer 2: POSIX CPU timer (hard, per-isolate, exact)
  timer_create(CLOCK_THREAD_CPUTIME_ID)
  Fires signal when total isolate CPU exceeds limit
  Catches infinite loops (promise hooks can't fire)
  Precision: exact (kernel-level)

Layer 3: Global watchdog (fallback, all platforms)
  Polls every 500ms for liveness
  Catches hung event loops
  Non-Linux fallback
```

### 7.2 Budget Model

```
Per-request:
  soft_cpu_limit: 50ms (from plan, checked at yield points)
  hard_cpu_limit: 200ms (POSIX timer, catches tight loops)

Per-isolate:
  aggregate_cpu_limit: 5s per billing period (metering)
```

## 8. Memory Layout

```
Per isolate (~3MB):
  V8 isolate:           ~2.4MB (engine shared via OS COW)
  Persistent context:   ~100KB
  Global handles:       ~10KB
  Timer heap:           ~1KB (typical)
  Op state:             ~5KB
  Request tracking:     ~1KB per in-flight request

Per 1000 apps: ~3GB V8 + ~100MB Rust state
```

## 9. Dependencies

```toml
[dependencies]
v8 = "147"                           # V8 engine
libc = "0.2"                         # POSIX timers, CPU time
tokio = { version = "1", features = ["full"] }  # async I/O runtime
hyper = "1"                          # HTTP server
reqwest = "0.12"                     # HTTP client (fetch)
serde_json = "1"                     # JSON
uuid = "1"                           # crypto.randomUUID
base64 = "0.22"                      # atob/btoa
url = "2"                            # URL parsing
```

No deno_core. No deno_fetch. No deno_web. No deno_net. No deno_tls.
No TypeScript transpiler at runtime. No ESM module loader.

## 10. File Structure

```
crates/runtime/
  src/
    lib.rs              — public API (Isolate, IsolateManager)
    event_loop.rs       — min-heap timers, phase-based loop, recv_timeout
    promise_hooks.rs    — per-request CPU tracking, request tagging
    ops/
      mod.rs            — async op system (spawn, resolve, channel)
      fetch.rs          — fetch() via reqwest
      timers.rs         — setTimeout/clearTimeout/setInterval
      console.rs        — console.log/warn/error
      crypto.rs         — crypto.randomUUID, atob, btoa
      encoding.rs       — TextEncoder, TextDecoder
      url.rs            — URL, URLSearchParams
    globals.rs          — register all globals on context
    isolate.rs          — V8 isolate + persistent context
    worker.rs           — worker thread + event loop driver
    manager.rs          — per-app isolate lifecycle, pool, eviction
    cpu_timer.rs        — POSIX CPU timer (Linux)
    watchdog.rs         — global watchdog (fallback)
  benches/
    qps.rs              — throughput benchmark
  tests/
    cpu_limit.rs        — CPU enforcement tests
    async_ops.rs        — fetch, timers, promises
    concurrent.rs       — multi-request concurrency tests
```

## 11. Migration Path

```
Phase 1 (current): isolate_v8 crate with per-request model
  ✅ V8 isolate + persistent context
  ✅ Event loop with timers
  ✅ Per-request CPU measurement
  ✅ HTTP server + wrk benchmark

Phase 2: Add concurrent dispatch
  - Channel-based request injection (fire-and-forget)
  - Event loop Phase 4 (drain incoming requests)
  - Multiple in-flight requests per isolate

Phase 3: Promise hooks
  - V8 promise hook registration
  - Per-request CPU accumulation
  - Promise tagging (request_id propagation)

Phase 4: Async ops + fetch
  - Op system (JS → Promise → tokio task → resolve)
  - fetch() via reqwest
  - Integration with event loop Phase 3

Phase 5: Per-request kill
  - Soft kill (reject promise chain at yield point)
  - Hard kill (terminate + selective recovery)
  - Integration with POSIX CPU timer

Phase 6: Web APIs
  - TextEncoder/TextDecoder, URL, crypto, atob/btoa
  - Plugin system for DB, KV, etc.

Phase 7: Production hardening
  - V8 snapshots for fast cold start
  - Memory pressure eviction
  - Graceful shutdown
  - Structured logging
```

## 12. Performance Targets

```
Lightweight RPC (ping):     >100K req/s per thread (wrk)
Async I/O (fetch):          >50K req/s (concurrent, I/O overlapping)
CPU-heavy (fib(30)):        ~60 req/s per thread (V8 limit)
Cold start:                 <50ms (without snapshots), <5ms (with)
Per-request latency:        <5us overhead (V8 dispatch only)
Event loop idle CPU:        0% (recv_timeout blocking)
Memory per isolate:         <5MB
```
