# V8 runtime

Everything that runs end-user code lives in `crates/runtime`. This is the largest crate by line count and the most invariant-heavy.

## Mental model

```
                         ┌──────────────────────────┐
                         │  V8 isolate (per app)    │
                         │                          │
   compio event loop ───►│  fetch / WebSocket /     │
   (one per worker       │  streams / WebCrypto /   │
    thread, no tokio)    │  env.* primitives        │
                         │                          │
                         │  user JS code            │
                         └──────────────────────────┘
```

A worker thread owns:
1. A compio `Runtime` (the I/O scheduler).
2. A LRU cache of V8 isolates keyed by `app_id`.
3. A "pump" task that drains async-op responses back into V8.

When a request arrives:
- The worker resolves `app_id` → V8 isolate (cache hit) or fetches `manifest.worker.modules[entry]` from `BlobStore` and creates the isolate (cache miss).
- Calls `Runtime::call_fetch_handler(modules, method, url, headers, body, ctx)` which invokes the user's exported `default.fetch`.
- Returns a `FetchOutcome` (status, headers, body or stream).

## Crate layout (`crates/runtime/src/`)

```
init.rs           V8 init, polyfill loading, fallback fetch (when user has no default.fetch)
runtime.rs        Runtime + RuntimeBuilder + RuntimeInner (the per-isolate state)
serve.rs          Standalone server entry — multi-worker SO_REUSEPORT
server.rs         Bin entry (used by `zeroship serve`)
modules.rs        Module loading; ModuleEntry; ModuleRegistry
state.rs          SharedState + RuntimeState (cross-isolate cell)
http.rs           HTTP request/response shaping (envelope, ResponseInfo, SettledResult)
plugin.rs         NativePlugin trait, the extension point for env.{db,kv,storage}.*
storage.rs        AppStorage abstraction
channel.rs        Compio-friendly channels (CancelFlag, etc.)
panic_util.rs     V8-safe panic handling
benches/          zerobench scenarios + Node baselines + perf result snapshots
```

Plus `init.rs` carries a hefty chunk of JS — the polyfill prelude (streams, WebSocket, etc.) and the fallback fetch handler.

## Three invariants worth knowing

### 1. V8 per thread, isolates `enter`/`exit`

V8 has thread-local state (`Isolate::GetCurrent()`). Multiple isolates can live on one thread, but only one is "entered" at a time. The worker enters before each request, exits after, so other isolates can run between calls.

`RuntimeInner` tracks `enter_depth: u32`. The custom `Drop` impl re-enters at depth 0 just-in-time so V8's `OwnedIsolate::Drop` assertion (`current == self`) passes when a cached isolate finally drops. Without this, the first cached isolate to drop would panic the worker.

See `crates/runtime/src/core/runtime.rs:enter_isolate`/`exit_isolate`/`Drop for RuntimeInner`.

### 2. The pump task

V8 calls native ops (e.g. `fetch`, `setTimeout`) which return a promise. The native side starts the work on compio and stashes a sender into `RuntimeState::pending_ops`. When the work completes, the sender notifies the pump task, which re-enters V8 and resolves the promise.

Pump task = one compio task per isolate, parked on a flume receiver. See `start_pump` in `runtime.rs`.

### 3. `init_error` surfaces real diagnostics

If `load_polyfills_and_modules` fails (parse error, evaluation throw), the error is captured into `RuntimeInner::init_error`. `call_fetch_handler` reads it and returns 500 with the diagnostic, not a 404 "no fetch handler." Without this, a syntax error in user code looks like "no default.fetch exported" — totally misleading.

## Native primitives (`env.*`)

Registered via the `NativePlugin` trait. Each plugin owns a namespace on
the `env` handler arg (the 2nd arg to `fetch(req, env, ctx)` / the `env`
named export of the `zeroship` module):

- `env.db.*` → `crates/plugin-db`
- `env.kv.*` → `crates/plugin-kv`
- `env.storage.*` → `crates/plugin-storage`

The `#[v8_class]` proc macro (`crates/runtime-macros`) generates the V8-FFI glue. See `docs/reference/plugin-system.md`.

## Module loading

Modules come from a `Vec<ModuleEntry>` (specifier + source). Today every deploy is a single `index.js` (post-bundling), so the vec usually has one entry. The module loader supports multi-entry as a future scenario; see `docs/reference/appbundle-format.md` for what's actually emitted today.

## Polyfills

`init.rs` evaluates a stack of polyfills before user code:

- WHATWG Streams (native — see `docs/proposals/streams-native.md`; ReadableStream, WritableStream, TransformStream, *Controller, *Reader, *Writer, BYOBReader, BYOBRequest, ByteLengthQueuingStrategy, CountQueuingStrategy, async-iter prototype patches)
- `TextEncoderStream` / `TextDecoderStream` (small JS wrapper over native TransformStream + TextEncoder/TextDecoder, in `embed/text-streams.js`)
- TextEncoder/Decoder, Headers, URL, URLSearchParams, fetch, Request, Response
- WebSocket (RFC 6455)
- WebCrypto subset
- Node compat shims (`process.versions`, `process.nextTick`, `AbortSignal.any`, ICU data)

Stream chunks moving onto an HTTP wire go through the Rust-side response forwarder in `crates/runtime/src/web/streams/response_forwarder.rs`: when `inspect_response` sees a Response with a stream body, `begin_forward` locks it via `getReader()` and drives `read()` in a Rust promise-reaction loop, pushing each chunk into a per-stream forwarder. The kernel attaches a `direct_writer` (StreamWriter to the TCP-bound channel) in `build_fetch_outcome`; from then on chunks pump straight to the wire. This used to be a JS shim (`__zsBeginStreamForward` + `__streams.{create,enqueue,close,error}` namespace) — both deleted.

## Cold-start budget (and why we don't snapshot)

Measured cold-start (Criterion microbench, `crates/runtime/benches/cold_start.rs`):

- `isolate_only` p50 ≈ **1.24 ms** (V8 isolate construction + native class installs).
- `boot_to_first` p50 ≈ **2.4 ms** (above + polyfill JS evaluation + first user-fetch dispatch).

This is fast enough that a V8 boot snapshot (`SnapshotCreator` /
`Isolate::CreateParams::snapshot_blob`) doesn't help — investigated
through three implementation phases on `feature/boot-snapshot` (deleted
2026-05-06), all perf-neutral or slightly regressed. See
`crates/runtime/TODO.md` §"Memory footprint" lever 2 for the writeup.

The platform's discipline of small native primitives + lazy lib imports
+ minimal polyfill JS already wins most of the cold-start battle. The
snapshot lever only pays off when init exceeds ~10 ms, which would
require a much heavier native surface or richer polyfill prelude. If
that day comes, the lazy-in-process pattern (build snapshot at first
`Runtime::builder().build()`, cache in `OnceLock<Vec<u8>>`) is the
right shape — V8 won't accept flag changes post-`init_v8`, so the
build.rs blob route has ergonomic problems.

What's wired today:

- **Heap cap** (`RuntimeBuilder::heap_limit_mb`) — bounds per-isolate
  RSS; default 128 MB. See `docs/reference/runtime-limits.md`.
- **Idle GC** (`RuntimeBuilder::idle_gc_after_ms`) — fires
  `low_memory_notification` after a configurable idle window;
  default 30 s. Frees ~50–100 MB per isolate during quiet windows.

## Bench infrastructure

`crates/runtime/benches/`:
- `zeroship-bench.rhai` — the scenario DSL (`rpc`, `httpGet`, `sseHold`, `wsEchoRtt`, `fetchEcho`)
- `node_server.js` — Node baseline implementing the same scenarios over `POST /_rpc/<method>`
- `run_zerobench.sh` — runner with NUMA pinning, warmup, multi-port comparison
- `results-2026-04-*.txt` — frozen snapshots (perf regression tracking)

See `docs/reference/zerobench.md` for the tool.

## Where to start by sub-task

| Working on… | Read first |
| --- | --- |
| A new native primitive | `docs/reference/plugin-system.md`, then `crates/plugin-kv/src/lib.rs` (smallest existing example) |
| Streams correctness | `crates/runtime/src/core/init.rs` (look for `streams.js` and the polyfill) |
| Cold-start latency | `crates/runtime/src/core/runtime.rs::ensure_initialized`. Boot snapshot investigated 2026-05-06 — not worth it at our 2.4 ms baseline (see "Cold-start budget" above). V8 code-cache wiring is the next lever if needed. |
| HTTP request shaping | `crates/runtime/src/transport/handler.rs` |
| Multi-tenant isolation | `crates/worker/src/cache.rs` (LRU, eviction); `runtime.rs` (`enter_depth`, `Drop`) |
| Adding a Node compat shim | `crates/runtime/src/core/init.rs` (the polyfill prelude); `docs/reference/node-compat.md` |
