# V8 runtime

`crates/runtime` is the embedder layer for app code. `src/lib.rs` is mostly a re-export surface; the implementation now lives under `src/core/`, `src/transport/`, and `src/web/`.

## Request model

```text
worker thread
  -> resolve app_id to cached `Runtime`
  -> call `Runtime::call_fetch_handler(method, url, headers, body, env, ctx)`
  -> receive `FetchOutcome`
  -> return response, stream, websocket upgrade, or pending receiver
```

The worker is responsible for fetching the worker-entry blob from `BlobStore` and building the `Vec<ModuleEntry>` passed into `Runtime::builder()`.

## Layout

```text
src/lib.rs                    public re-exports
src/core/init.rs              V8 init, global installs, polyfill/module loading
src/core/runtime.rs           `Runtime`, `RuntimeBuilder`, `RuntimeInner`, async pump
src/core/modules.rs           `ModuleEntry` loading
src/core/plugin.rs            `NativePlugin` host surface
src/core/state.rs             shared per-isolate runtime state
src/transport/handler.rs      HTTP request/response bridge
src/fetch_outcome.rs          `FetchOutcome`, `SettledFetch`, `EnvSnapshot`, `RequestCtx`
src/rpc/                      RPC dispatch + abort/capability helpers
src/web/                      native Web APIs and stream machinery
src/storage.rs                app-storage abstraction
```

## Invariants

- V8 isolates are thread-bound. The worker keeps a thread-local cache in [cache.rs](crates/worker/src/cache.rs).
- That `thread_local!` cache shape is what lets each worker thread own, re-enter, and evict only its own isolates without crossing V8 thread affinity.
- `RuntimeInner` tracks `enter_depth`; isolates are entered for a V8 turn and exited afterwards so multiple isolates can live on one worker thread.
- `build()` leaves a new isolate entered, and the worker exits it after caching so later requests can re-enter it just in time for dispatch.
- Async native work resolves through the per-isolate pump started by `Runtime::start_pump()`.
- The pump batches ready timer and op completions into a single V8 re-entry so one burst of settled work does not pay one enter/exit cycle per completion.
- Initialization failures are stored on `RuntimeInner::init_error`; `call_fetch_handler` surfaces them as a 500 instead of pretending no handler exists.

## Native surface

Plugins register namespaces on `env` through `NativePlugin`:

- `env.db.*` -> `crates/plugin-db`
- `env.kv.*` -> `crates/plugin-kv`
- `env.storage.*` -> `crates/plugin-storage`

The `#[v8_class]` macro support lives in `crates/runtime-macros`.

## Web APIs and streams

`load_polyfills_and_modules` in [init.rs](crates/runtime/src/core/init.rs) installs the runtime surface before user code runs. The important current pieces are:

- native WHATWG Streams
- native Blob/File
- native `TextEncoderStream` and `TextDecoderStream`
- DOM/fetch/Request/Response/FormData
- native `CompressionStream` / `DecompressionStream`
- native `EventSource`
- WebSocket and crypto support

Stream responses are pumped by [response_forwarder.rs](crates/runtime/src/web/streams/response_forwarder.rs), not by the older JS shim.

## Limits

`RuntimeLimits` is carried by the worker per app. The current builder surface exposes:

- `RuntimeBuilder::heap_limit_mb(...)`
- `RuntimeBuilder::idle_gc_after_ms(...)`

See `docs/reference/runtime-limits.md` for the operator-facing contract.

## Current bundle shape

The runtime accepts many `ModuleEntry` values, but the current worker path still loads a single `index.js` module from `manifest.worker.modules[manifest.worker.entry]`. Multi-module manifests are represented on the wire and can be passed through once the worker-side loader starts materializing them.

## Where to start

| Working on | Start here |
| --- | --- |
| V8 boot / globals | [init.rs](crates/runtime/src/core/init.rs) |
| Runtime lifecycle / pump | [runtime.rs](crates/runtime/src/core/runtime.rs) |
| HTTP request shaping | [handler.rs](crates/runtime/src/transport/handler.rs) |
| Streams | [streams/mod.rs](crates/runtime/src/web/streams/mod.rs) |
| Native plugin wiring | [plugin.rs](crates/runtime/src/core/plugin.rs) |
| Bench tooling | `crates/runtime/benches/`, `docs/reference/zerobench.md` |

## Related docs

- [docs/architecture/overview.md](docs/architecture/overview.md) places the runtime in the full platform architecture.
- [docs/architecture/distributed.md](docs/architecture/distributed.md) expands the cross-service request and deploy flows around the worker/runtime boundary.
- [docs/reference/plugin-system.md](docs/reference/plugin-system.md) explains how `NativePlugin` surfaces become `env.*` namespaces inside an isolate.
- [docs/reference/websocket-design.md](docs/reference/websocket-design.md) covers the runtime's WebSocket model and upgrade handling in more detail.
- [docs/reference/node-compat.md](docs/reference/node-compat.md) documents how npm packages and Node-style resolution are exposed inside V8.
- [docs/reference/runtime-limits.md](docs/reference/runtime-limits.md) defines the operator-facing CPU, wall-clock, heap, and idle-GC controls referenced here.
