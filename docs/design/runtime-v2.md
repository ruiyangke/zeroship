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

### 3.1 Core Principle: Concurrent I/O, Serial JS, Clean Kill

```
I/O:  CONCURRENT — all requests' async ops run in parallel on tokio
JS:   SERIAL per-request — only 1 request's .then chain executes at a time
Kill: CLEAN — microtask queue has only 1 request's entries, zero collateral

This gives us workerd's kill safety with event loop simplicity.
No V8 Locker. No thread pool. No lock contention.
```

### 3.2 Request Lifecycle

```
HTTP request arrives (tokio)
    |
    v
Dispatcher (round-robin across workers)
    |
    v
Worker thread (1 per app, persistent V8 isolate)
    |
    v
Event loop accepts request
    |
    +---> Call dispatch function → returns Promise
    |       |
    |       +---> Handler calls fetch(url) → starts async op on tokio
    |       |     (I/O runs concurrently with other requests' I/O)
    |       |
    |       +---> Handler calls db.find() → starts async op on tokio
    |
    v
Request is now "pending" (waiting for async ops)
Other requests accepted and dispatched (concurrent I/O)
    |
    v
Async op completes → event loop resolves THIS request's promise
    (microtask queue: ONLY this request's .then entries)
    |
    v
Response sent via oneshot channel
```

### 3.3 Concurrency Model: Serial JS, Concurrent I/O

```
3 requests arrive:

  Phase: ACCEPT (all start I/O concurrently)
    A → dispatch → handler calls fetch(api1) → op starts on tokio
    B → dispatch → handler calls fetch(api2) → op starts on tokio
    C → dispatch → handler calls fetch(api3) → op starts on tokio

    All 3 fetches run in parallel. V8 thread waits.

  Phase: RESOLVE (one at a time, serial JS)

    api2 responds first:
      Enter "B's JS scope"
      Resolve B's promise → run B's .then → run B's .then
      Microtask queue: [B.then1, B.then2] — ONLY B's entries
      B completes → send response

    api1 responds:
      Enter "A's JS scope"
      Resolve A's promise → run A's .then chain
      Microtask queue: [A.then1] — ONLY A's entries
      A completes → send response

    api3 responds:
      Enter "C's JS scope"
      Resolve C's promise → C enters tight loop!
      POSIX timer fires → terminate_execution()
      Microtask queue: [C.then1, C.then2] — ONLY C's entries
      → ALL lost entries belong to C. Zero collateral.
      cancel_terminate → isolate recovers
      C gets error. A and B already responded. No damage.
```

### 3.4 Why This Is Safe

```
At any moment, the microtask queue contains entries from EXACTLY 1 request.

Why? Because we resolve promises ONE REQUEST AT A TIME:
  1. Pick 1 completed async op (for request B)
  2. Resolve B's promise
  3. Run microtask checkpoint → processes ONLY B's .then chain
  4. B's .then may create more microtasks → still B's entries
  5. B completes (or yields with new async op)
  6. Microtask queue is now EMPTY
  7. Pick next completed async op (for request A)
  8. Repeat

Between step 6 and 7: queue is empty. Clean slate.
If terminate_execution fires during step 3: only B's entries are lost.
```

### 3.5 Comparison with workerd

```
workerd achieves the same guarantee via V8 Locker:
  Lock → run 1 request's JS → unlock
  Queue has only that request's microtasks

We achieve it via serial promise resolution:
  Resolve 1 request's promise → checkpoint → queue drained
  Queue has only that request's microtasks

Same safety. Different mechanism.
workerd: needs V8 Locker, thread pool, C++ complexity
Ours: needs careful event loop ordering, single thread, Rust simplicity
```

### 3.6 Promise Hooks — Per-Request CPU Tracking

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
                kill_request(req_id); // reject promise, send error
            }
            CURRENT_REQUEST.set(0);
        }
        Resolve => {
            // Promise resolved. Not needed for CPU tracking.
        }
    }
}
```

### 3.7 Per-Request Kill — Three Layers

```
Layer 1: Soft kill (at yield points, zero collateral)
  After hook detects: request A cpu > budget
  → reject A's promise chain with "CPU limit exceeded"
  → A's reply gets error
  → Microtask queue was A-only → nothing else affected
  → Event loop continues with next request

Layer 2: Hard kill (tight loops, zero collateral)
  POSIX CPU timer fires (50ms hard limit)
  → signal handler → pipe → watchdog thread
  → terminate_execution()
  → V8 unwinds current stack
  → Microtask queue: only current request's entries → safe to clear
  → cancel_terminate_execution()
  → Current request gets error
  → Isolate recovers, serves next request

Layer 3: Nuclear kill (corrupted state, last resort)
  V8 heap exceeded, unrecoverable error
  → Evict entire isolate
  → All pending requests get error
  → New isolate created on next request

Layers 1+2: zero collateral damage to other requests.
Layer 3: only for unrecoverable situations (rare).
```

## 4. Event Loop

### 4.1 Phases

```
loop {
    // Phase 1: ACCEPT new requests
    //   Call dispatch function for each → creates Promise → starts async ops
    //   All async ops begin immediately (concurrent I/O on tokio)
    //   DO NOT run microtask checkpoint yet (batch all dispatches first)
    while let Ok(request) = request_rx.try_recv() {
        dispatch(request)   // fire-and-forget, returns Promise
    }
    // Now run microtask checkpoint to let initial async ops start
    isolate.perform_microtask_checkpoint();

    // Phase 2: TIMERS
    //   Fire each ready timer's callback individually
    //   Microtask checkpoint after each (drain that timer's chain)
    while timer_heap.peek().fire_at <= now {
        fire timer callback
        isolate.perform_microtask_checkpoint();  // drain this callback's chain
    }

    // Phase 3: RESOLVE async ops (ONE AT A TIME — the key to clean kill)
    //   For each completed op, resolve its promise and drain its chain.
    //   Microtask queue has ONLY this request's entries after each resolve.
    while let Ok(result) = op_rx.try_recv() {
        resolve_promise(result.request_id, result.value);
        isolate.perform_microtask_checkpoint();  // drain THIS request's chain only
        // If terminated during checkpoint → only this request's entries lost
    }

    // Phase 4: CHECK done
    if no timers && no pending ops && no pending requests {
        // idle — wait for next request
    }

    // Phase 5: WAIT (zero CPU)
    //   Block until: timer fires, op completes, or new request arrives
    //   Single recv_timeout on merged event channel
    let timeout = duration_until_next_timer();
    match event_rx.recv_timeout(timeout) {
        Ok(Event::OpCompleted(..)) => continue,
        Ok(Event::NewRequest(..)) => continue,
        Err(Timeout) => continue,             // timer ready
        Err(Disconnected) => break,           // shutdown
    }
}
```

### 4.2 The Critical Invariant

```
INVARIANT: Between each perform_microtask_checkpoint() call in Phase 3,
exactly ONE promise is resolved. Therefore the microtask queue contains
entries from AT MOST one request at any time.

This guarantees: terminate_execution() during a microtask checkpoint
can only destroy the current request's .then chain. All other requests'
state (pending promises in the V8 heap) is untouched.
```

### 4.3 Wait Mechanism

Phase 5 blocks the V8 thread with zero CPU. All events merged into one channel:

```rust
enum Event {
    NewRequest { id: u64, body: String, reply: oneshot::Sender<Result> },
    OpCompleted { op_id: u32, value: String },
}
```

Timers use the min-heap, not the channel. The wait timeout is computed from
the heap's smallest fire_at. `recv_timeout(timeout)` blocks for exactly the
right duration — wakes on op completion OR timer expiry, whichever first.

### 4.4 Timer Implementation

```
Storage: BinaryHeap<Reverse<TimerEntry>> (min-heap)
Insert: O(log n)
Peek next: O(1)
Fire: O(log n) pop
Cancel: lazy deletion (remove callback, skip stale entries)
```

No spawned tasks per timer. No channel messages for timers.
Compute sleep duration from heap.peek().

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

### 6.1 Per-App Worker Group (Scalable)

```
App "todo-app" (Pro plan, 4 workers):
  4 V8 isolates (same server.js loaded in each)
  4 persistent contexts (JS state independent per worker)
  4 worker threads (dedicated)
  Each worker: N concurrent requests in-flight (event loop multiplexes)
  Load balancer: round-robin across workers

App "blog" (Free plan, 1 worker):
  1 V8 isolate, 1 thread, concurrent event loop
```

Configurable per app via plan:
```toml
[plans.free]
max_workers = 1

[plans.pro]
max_workers = 4

[plans.enterprise]
max_workers = 16

[apps.high_traffic_api]
workers = 8          # override (up to plan max)
```

### 6.2 IsolateManager

```rust
struct WorkerHandle {
    request_tx: mpsc::Sender<Event>,
    thread: JoinHandle<()>,
}

struct AppWorkerGroup {
    workers: Vec<WorkerHandle>,
    next: AtomicUsize,  // round-robin
}

impl AppWorkerGroup {
    fn dispatch(&self, request: Request) {
        let idx = self.next.fetch_add(1, Relaxed) % self.workers.len();
        self.workers[idx].send(request);
    }
}

struct IsolateManager {
    apps: HashMap<AppId, AppWorkerGroup>,
}
```

### 6.3 Shared State Model

Workers share NOTHING in memory. Each isolate has its own JS global state.
Shared state goes through external storage:

```
Worker 0: var counter = 0;  // counter++ → 1
Worker 1: var counter = 0;  // counter++ → 1 (not 2!)

Correct for shared state:
  await db.update("counter", { $inc: 1 });  // atomic in DB
  await kv.get("session:abc");               // shared KV store
```

This matches: Node.js cluster, Cloudflare Workers, Supabase Edge Runtime.

### 6.4 Scaling Math

| Plan | Workers | Lightweight RPC | fib(30) ~16ms | fetch(100ms I/O) |
|------|---------|----------------|---------------|-------------------|
| Free (1) | 1 | 100K req/s | 60 req/s | 10K req/s |
| Pro (4) | 4 | 400K req/s | 240 req/s | 40K req/s |
| Enterprise (16) | 16 | 1.6M req/s | 960 req/s | 160K req/s |

Linear scaling — workers share nothing.

### 6.5 Lifecycle

```
1. App first request → create N workers (N from plan config)
2. Each worker: load server.js (compiled once, persistent context)
3. Each worker: start event loop (blocks on recv_timeout)
4. Requests arrive → round-robin to workers → fire-and-forget dispatch
5. Idle timeout → evict app (all workers)
6. Memory pressure → evict LRU apps
7. Scale down → reduce workers (drain in-flight first)
8. Shutdown → drain all, close channels, join threads
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
