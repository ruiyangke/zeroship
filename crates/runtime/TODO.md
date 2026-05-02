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

## Test infrastructure

- Hand tests + WPT runners are scattered across `tests/` flat. After reorg, mirror
  the new src layout: `tests/web/fetch/...`, `tests/web/url/...` etc.
- WPT runners share a lot of testharness shim code (sanitize, fetch-fixture stub,
  microtask draining loop). Extract to a shared `tests/wpt_harness.rs` module.
- The microtask drain loop (`for _ in 0..10 { scope.perform_microtask_checkpoint() }`)
  is hard-coded; flaky under chained-await patterns. Loop until stable.
