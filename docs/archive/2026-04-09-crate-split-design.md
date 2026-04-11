# Crate Split Design — v8-core / runtime-tokio / runtime-compio

**Date:** 2026-04-09
**Goal:** Extract V8 dispatch primitives into a runtime-agnostic `v8-core` crate. Create `runtime-tokio` (current event loop + hyper server) and `runtime-compio` (compio event loop + httparse server, io_uring). The platform depends on `runtime-tokio`.

## Architecture

```
v8-core                     ← runtime-agnostic V8 primitives
  ↑               ↑
runtime-tokio    runtime-compio
  ↑
platform
```

### v8-core — what V8 needs, nothing about how it's driven

Contains everything that runs synchronously inside V8 or describes work for the runtime to execute:
- V8 initialization (init_v8, setup_globals, load_polyfills_and_modules)
- State types (RuntimeState, SharedState, OpResult, RequestReply, IncomingRequest, etc.)
- Synchronous V8 dispatch functions (dispatch_request, resolve_op, fire_timer_callback, extract_promise_result)
- V8 callbacks that push descriptors into state (fetch → FetchRequest, setTimeout → SpawnedTimer)
- ESM module loader
- Crypto, KV, env, URL, streams — all V8 callbacks
- CPU timer (POSIX, Linux only)
- Storage (versioned deploys)
- JS polyfills (embed/*.js)

Does NOT contain: event loop, select!, sleep, channels, TCP, HTTP server, reqwest.

### runtime-tokio — tokio event loop + hyper server

Repackages the current `runtime/src/runtime.rs` + `isolate.rs` + `server.rs`:
- `Runtime` struct with `tokio::select!` loop
- `enter_v8!` macro
- `collect_new_tasks` — drains spawned_ops, spawned_timers, **spawned_fetches**
- Fetch execution via `reqwest` on tokio
- Timer execution via `tokio::time::sleep`
- `Isolate` sync wrapper
- `v8-server` binary (hyper benchmark server)

### runtime-compio — compio event loop + httparse server

New implementation:
- `Runtime` struct with compio-based event loop
- `enter_v8!` macro (same pattern, different block_on)
- Fetch execution via compio HTTP client (or `hyper-util` with compio adapter, or raw TCP + httparse for outbound)
- Timer execution via `compio::time::sleep`
- `v8-server-compio` binary (httparse + compio TCP, io_uring)

## The fetch boundary

v8-core's `raw_fetch_callback` pushes a `FetchRequest` descriptor instead of executing the HTTP request:

```rust
// v8-core/src/state.rs
pub struct FetchRequest {
    pub op_id: u32,
    pub stream_id: u32,
    pub request_id: Option<u64>,
    pub method: String,
    pub url: String,
    pub headers_json: String,
    pub body: Option<String>,
    pub cancel: Option<CancellationToken>,
}

// v8-core/src/state.rs — RuntimeState
pub spawned_fetches: Vec<FetchRequest>,
```

v8-core also keeps SSRF validation (`validate_url`) and header parsing (`parse_headers`) — these are pure functions with no runtime dependency.

runtime-tokio's `collect_new_tasks` drains `spawned_fetches` and spawns reqwest futures. runtime-compio drains and spawns its own HTTP client futures.

## File mapping

### v8-core (from current runtime/)

| Current file | Destination | Changes |
|---|---|---|
| `state.rs` | `v8-core/src/state.rs` | Add `FetchRequest`, `spawned_fetches`. Remove `stream_events_tx` (runtime-specific). All types `pub`. |
| `request.rs` | `v8-core/src/request.rs` | All functions `pub`. No changes to logic. |
| `init.rs` | `v8-core/src/init.rs` | `load_polyfills_and_modules` becomes `pub`. No other changes. |
| `fetch.rs` | `v8-core/src/fetch.rs` | Push `FetchRequest` into state instead of executing. Keep validate_url, parse_headers, error_json. Remove reqwest, do_fetch_buffered, do_fetch_streaming. |
| `modules.rs` | `v8-core/src/modules.rs` | No changes. |
| `crypto.rs` | `v8-core/src/crypto.rs` | No changes. |
| `streams.rs` | `v8-core/src/streams.rs` | Remove stream_events_tx usage (runtime handles forwarding). |
| `timers.rs` | `v8-core/src/timers.rs` | No changes. |
| `kv.rs` | `v8-core/src/kv.rs` | No changes. |
| `env.rs` | `v8-core/src/env.rs` | No changes. |
| `url.rs` | `v8-core/src/url.rs` | No changes. |
| `ops.rs` | `v8-core/src/ops.rs` | No changes. |
| `cpu_timer.rs` | `v8-core/src/cpu_timer.rs` | No changes. |
| `storage.rs` | `v8-core/src/storage.rs` | No changes. |
| `embed/*.js` | `v8-core/src/embed/*.js` | No changes. |
| `runtime_macros/` | stays as `runtime_macros/` | Updated to depend on v8-core's types. |

### runtime-tokio (from current runtime/)

| Current file | Destination | Changes |
|---|---|---|
| `runtime.rs` | `runtime-tokio/src/runtime.rs` | Import from v8_core. Add fetch execution (reqwest). Add stream_events channel. |
| `isolate.rs` | `runtime-tokio/src/isolate.rs` | Import from v8_core. |
| `server.rs` | `runtime-tokio/src/server.rs` | Import from v8_core + self. |
| `lib.rs` tests | `runtime-tokio/src/lib.rs` | All existing tests move here. |
| `tests/examples.rs` | `runtime-tokio/tests/examples.rs` | Import from runtime_tokio. |
| `benches/v8_qps.rs` | `runtime-tokio/benches/v8_qps.rs` | Import from runtime_tokio. |

### runtime-compio (new)

| File | Contents |
|---|---|
| `runtime-compio/src/runtime.rs` | compio event loop, enter_v8!, fetch via compio HTTP |
| `runtime-compio/src/fetch.rs` | Execute FetchRequest via compio TCP/TLS |
| `runtime-compio/src/server.rs` | httparse + compio TcpListener binary |
| `runtime-compio/src/lib.rs` | Public API |

## Dependencies

```
v8-core:
  v8, serde_json, libc, url, ada-url, aws-lc-rs, base64,
  tokio-util (CancellationToken only), futures (Future trait only),
  appbase-runtime-macros

runtime-tokio:
  v8-core, tokio, hyper, hyper-util, http-body-util, bytes,
  reqwest, mimalloc, futures

runtime-compio:
  v8-core, compio, httparse, mimalloc, futures
  (+ compio TLS/HTTP client for fetch — TBD)

platform:
  runtime-tokio (unchanged dependency, just renamed)
```

Note: `tokio-util` (CancellationToken) is in v8-core because `FetchRequest.cancel` uses it. CancellationToken is runtime-agnostic (uses `std::task::Waker`).

## Migration strategy

1. Create `v8-core` crate, move files
2. Create `runtime-tokio` crate, move event loop + tests
3. Delete old `runtime/` (or keep as thin re-export shim for backwards compat)
4. Update `platform/` to depend on `runtime-tokio`
5. Verify all tests pass
6. Create `runtime-compio` crate

Steps 1-5 are a mechanical refactor (no logic changes). Step 6 is the new implementation.

## Success criteria

1. All 91 lib tests pass in runtime-tokio
2. All 27 example tests pass (excluding weather)
3. Benchmark: >=200K req/s for ping (no regression from crate split)
4. `v8-core` has no dependency on tokio runtime (only tokio-util for CancellationToken)
5. `runtime-compio` compiles and runs a basic ping benchmark
6. Platform compiles and works unchanged
