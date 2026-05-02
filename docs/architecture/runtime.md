# V8 runtime

Everything that runs end-user code lives in `crates/runtime`. This is the largest crate by line count and the most invariant-heavy.

## Mental model

```
                         ┌──────────────────────────┐
                         │  V8 isolate (per app)    │
                         │                          │
   compio event loop ───►│  fetch / WebSocket /     │
   (one per worker       │  streams / WebCrypto /   │
    thread, no tokio)    │  zeroship.* primitives   │
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
plugin.rs         NativePlugin trait, the extension point for zeroship.{db,kv,storage}.*
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

See `crates/runtime/src/runtime.rs:enter_isolate`/`exit_isolate`/`Drop for RuntimeInner`.

### 2. The pump task

V8 calls native ops (e.g. `fetch`, `setTimeout`) which return a promise. The native side starts the work on compio and stashes a sender into `RuntimeState::pending_ops`. When the work completes, the sender notifies the pump task, which re-enters V8 and resolves the promise.

Pump task = one compio task per isolate, parked on a flume receiver. See `start_pump` in `runtime.rs`.

### 3. `init_error` surfaces real diagnostics

If `load_polyfills_and_modules` fails (parse error, evaluation throw), the error is captured into `RuntimeInner::init_error`. `call_fetch_handler` reads it and returns 500 with the diagnostic, not a 404 "no fetch handler." Without this, a syntax error in user code looks like "no default.fetch exported" — totally misleading.

## Native primitives (`zeroship.*`)

Registered via the `NativePlugin` trait. Each plugin owns a namespace:

- `zeroship.db.*` → `crates/plugin-db`
- `zeroship.kv.*` → `crates/plugin-kv`
- `zeroship.storage.*` → `crates/plugin-storage`

The `#[zeroship_op]` proc macro (`crates/runtime-macros`) generates the V8-FFI glue. See `docs/reference/plugin-system.md`.

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

Stream chunks moving onto an HTTP wire go through the kernel-callable `__zsBeginStreamForward(response)` helper in `fetch.js`: it locks the body via `getReader()` and pumps each chunk into a Rust StreamState identified by `response._streamId`. `inspect_response` reads that id and forwards to the TCP writer.

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
| Streams correctness | `crates/runtime/src/init.rs` (look for `streams.js` and the polyfill) |
| Cold-start latency | `crates/runtime/src/runtime.rs::ensure_initialized`; consider V8 code-cache wiring (Tier 4 future) |
| HTTP request shaping | `crates/runtime/src/http.rs` |
| Multi-tenant isolation | `crates/worker/src/cache.rs` (LRU, eviction); `runtime.rs` (`enter_depth`, `Drop`) |
| Adding a Node compat shim | `crates/runtime/src/init.rs` (the polyfill prelude); `docs/reference/node-compat.md` |
