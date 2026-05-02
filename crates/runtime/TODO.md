# runtime TODO

Backlog ordered roughly by leverage. **DO NOT start the reorg while in-flight
agents are working** — wait for fetch-js-delete, url-reviser, websocket-design,
and crypto-review to merge first; otherwise merge conflicts dominate.

## Reorg the source tree

Today `crates/runtime/src/` has 30+ top-level files with inconsistent grouping:
- Web APIs sprawl across the root (`headers.rs`, `codec.rs`, `crypto.rs`,
  `byte_string.rs`, `enforce_range.rs`, `websocket.rs`, `fetch_request.rs`,
  `fetch_response.rs`) AND in subdirs (`dom/`, `streams/`, `fetch_native/`,
  `fetch_body/`, `url_native/`, `blob_native/`, `text_encoding/`)
- `fetch_request.rs` and `fetch_response.rs` are top-level even though
  `fetch_body/` and `fetch_native/` are subdirs
- Macro-support utilities (`enforce_range.rs`, `byte_string.rs`) live at root
- `fetch.rs` is half legacy SSRF and half live cyper plumbing — name no longer fits
- `embed/` mixes JS shims with native install code

### Proposed layout

```
crates/runtime/src/
├── lib.rs
├── core/                       runtime infra
│   ├── runtime.rs              the V8 pump
│   ├── state.rs
│   ├── dispatch.rs
│   ├── init.rs
│   ├── channel.rs
│   ├── modules.rs
│   ├── panic_util.rs
│   ├── plugin.rs
│   ├── cpu_timer.rs
│   ├── server.rs
│   └── serve.rs
│
├── webidl/                     shared boundary types
│   ├── byte_string.rs
│   ├── usv_string.rs           (currently inside url_native/helpers.rs)
│   └── enforce_range.rs
│
├── web/                        Web APIs
│   ├── encoding/               TextEncoder + TextDecoder + streams
│   ├── streams/                unchanged
│   ├── url/                    was url_native — drop the _native suffix
│   ├── blob/                   was blob_native
│   ├── headers.rs
│   ├── dom/                    Event / EventTarget / CustomEvent /
│   │                           AbortController / AbortSignal / FormData
│   ├── fetch/                  consolidate everything fetch
│   │   ├── algorithms.rs       was fetch_native/algorithms.rs
│   │   ├── http_network.rs
│   │   ├── redirect.rs
│   │   ├── content_encoding.rs
│   │   ├── data_url.rs
│   │   ├── bad_ports.rs
│   │   ├── body/               was fetch_body/
│   │   ├── request.rs          was fetch_request.rs
│   │   └── response.rs         was fetch_response.rs
│   ├── codec.rs                used by fetch + future CompressionStream
│   ├── crypto.rs               stays until subtle goes native
│   └── websocket.rs            native framing
│
├── transport/                  Rust HTTP plumbing
│   ├── ssrf.rs                 was fetch.rs's SSRF guard
│   ├── client.rs               was fetch.rs's cyper Client + admission
│   └── handler.rs              was http.rs (kernel bridge)
│
├── storage.rs                  stays (different concern)
├── auth.rs                     stays
└── embed/                      shrinking JS shim set
    ├── crypto.js               until subtle goes native
    ├── websocket.js            until WebSocket goes native
    └── node-globals.js
```

### Why

1. All Web APIs under one tree — clear that they ship as part of the platform's
   IDL surface
2. Drop the `_native` suffix — every Web API is native by definition; the suffix
   was a transitional name during cutover
3. Fetch's three sibling areas (`fetch_native/`, `fetch_body/`,
   `fetch_request.rs`, `fetch_response.rs`) collapse into one `web/fetch/`
4. Runtime infra (`core/`) clearly separated from Web APIs (`web/`)
5. Shared boundary types (`webidl/`) have a home — used by multiple classes
6. `transport/` owns the wire (cyper client + SSRF + handler bridge) separately
   from `web/fetch/` which owns the spec algorithms
7. `embed/` shrinks to just remaining JS shims — once WebCrypto + WebSocket go
   native, `embed/` disappears entirely

### Cost

- ~50 files renamed via `git mv`
- Every `pub mod` in `lib.rs` updated
- Every `crate::X::Y` import updated (~hundreds)
- Re-exports in `lib.rs` adjusted
- Mostly mechanical; `cargo check` guides

### When

After all in-flight agents merge:
1. fetch-js-delete (touches `fetch_body/`, `fetch_native/`, `http.rs`, `fetch.rs`)
2. url-reviser (touches `url_native/`)
3. websocket-design (just writes a doc; no code)
4. crypto-review (just produces a review; no code)

Then: single dispatch, single PR, one big commit.

## Smaller items

- Per-class isolate-slot caching is per `__InstallSlot_X` types, but registration
  in `init.rs` happens via individual function calls. Could be unified via a
  `register_native_class!` macro — minor.
- `fetch.rs` post-cleanup-rawfetch is ~600 LOC of SSRF + cyper Client + admission
  cap. Split into `transport/ssrf.rs` + `transport/client.rs` as part of the reorg.
- `legacy_bridge.rs` (in `streams/`) is named misleadingly — still alive via
  `__zsBeginStreamForward`. Rename to `stream_bridge.rs` OR delete entirely if
  the Rust-only forwarder lands via fetch-js-delete.
- `state.rs` has `OpResult::StreamChunk` + `pending_fetches` etc. — some may be
  dead post-cleanup-rawfetch. Audit during reorg.

## Memory footprint

The runtime's per-isolate working set sits around 125 MB after warmup
(V8 baseline ~50 MB + native class init ~20 MB + scenarios bytecode
~20 MB + transient request state ~30 MB). At 16 workers per process,
that's ~2 GB resident. Three levers, listed by effort × impact:

### 1. Per-isolate `--max-old-space-size` cap

V8 has no heap cap today; it grows to multi-GB before GC pressure
kicks in. Capping old-gen forces earlier GC and bounds the worst
case.

- API: `v8::Isolate::CreateParams::heap_limits(initial, max)` —
  pass `max = 64 * 1024 * 1024` (or whatever the cap is).
- Wire via `RuntimeBuilder` so deployers can set it per-app.
- Risk: too low causes thrashing or OOM. Default off; opt-in via
  `RuntimeLimits::heap_limit_mb`. Document the trade-off in
  `docs/reference/runtime-limits.md` (file doesn't exist yet —
  create as part of this).
- Cuts total RSS from ~2 GB → ~1 GB at 16 workers.

### 2. Boot snapshot — `StartupData`

V8 supports startup snapshots: freeze the post-init heap (after
all native classes installed, after `scenarios.js`-equivalent
boot-time JS evaluated) into a binary blob. Each isolate boots
from the snapshot instead of re-running init.

- API: `v8::SnapshotCreator` build-time, `v8::Isolate::CreateParams::snapshot_blob`
  per-isolate boot.
- Two snapshots: (a) base — native classes + Web API surface;
  (b) per-app — base + the user's `default.fetch` + module
  graph evaluated. (b) is the bigger win for cold-start.
- Risk: snapshot must be re-built on every native API change.
  Add a build-time step in `crates/runtime/build.rs` that produces
  `target/zeroship-runtime-snapshot.bin`, included via `include_bytes!`.
- Cuts per-isolate boot from ~50–200 ms → ~5–10 ms. Saves
  ~20–30 MB per isolate (no init artifacts retained — already
  compiled into the snapshot).
- Cloudflare Workers technique. The biggest perf lever for
  multi-tenant cold starts.

### 4. Idle GC trigger

V8 only GCs under heap pressure or when the allocator hits a
threshold. During low-traffic windows the heap retains its
high-water-mark working set indefinitely — unfree-able from the
OS's point of view.

- API: `v8::Isolate::idle_notification_deadline(deadline_in_seconds)`
  hints V8 to spend up to N ms running incremental GC. Returns
  `true` when GC has caught up.
- Wire into the compio event loop as a "no requests for K seconds
  → fire idle GC" trigger. Per-isolate timer.
- Effort: ~30 LOC in `runtime.rs` — track `last_request_ts` per
  isolate, schedule idle ticks via `compio::time::interval`.
- Saves: depends on traffic profile. For per-app isolates that
  see bursty traffic, can free ~50–100 MB per isolate during
  idle windows.

(Note: original "drop --workers=16 to --workers=4" alternative
isn't a runtime concern — it's a deploy-time config.)

## Test infrastructure

- Hand tests + WPT runners are scattered across `tests/` flat. After reorg, mirror
  the new src layout: `tests/web/fetch/...`, `tests/web/url/...` etc.
- WPT runners share a lot of testharness shim code (sanitize, fetch-fixture stub,
  microtask draining loop). Extract to a shared `tests/wpt_harness.rs` module.
- The microtask drain loop (`for _ in 0..10 { scope.perform_microtask_checkpoint() }`)
  is hard-coded; flaky under chained-await patterns. Loop until stable.
