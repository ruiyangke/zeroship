# Native WHATWG Fetch design

**Date:** 2026-05-01
**Status:** **Shipped** — `crates/runtime/src/web/fetch/` (~5,400 LOC, request.rs/response.rs/body/algorithms.rs). Document retained as the canonical design spec; no equivalent reference doc exists.
**Spec:** WHATWG Fetch Standard — https://fetch.spec.whatwg.org/
**Spec source:** https://github.com/whatwg/fetch/blob/main/fetch.bs
**Reference impls:**
  - undici (Node 22 fetch, the most spec-faithful production impl) —
    https://github.com/nodejs/undici/tree/main/lib/web/fetch
  - workerd (pure-native C++, embedded server runtime) —
    https://github.com/cloudflare/workerd/tree/main/src/workerd/api
  - Deno (JS+Rust hybrid) — https://github.com/denoland/deno/tree/main/ext/fetch
**Tests:** WPT `fetch/api/` — https://github.com/web-platform-tests/wpt/tree/master/fetch/api
**WebIDL:** https://webidl.spec.whatwg.org/
**Related RFCs:** RFC 9110 (HTTP semantics), RFC 9112 (HTTP/1.1), RFC 7234
(HTTP caching), RFC 6265 (cookies), RFC 9111 (HTTP caching, current),
RFC 7578 (multipart/form-data).

**Depends on:**
- `docs/proposals/streams-native.md` — every body operation in this design
  consumes the `from_native_source` / `pipe_native_internal` / `from_native_sink`
  surface that streams-native ships. Fetch's RS-consumer paths (Body
  consumers, `clone()`, `extract_body` from a stream) need the
  ReadableStream pieces of streams-native, which have **landed**
  (`ReadableStream` + `DefaultController` + `DefaultReader`). Fetch's
  WritableStream-side (streaming uploads via WritableSink) and any
  TransformStream wiring depend on streams' WS+TS work, which is
  **pending**; per §XVIII.1, fetch's Body skeleton can begin in
  parallel with that work.
  <!-- Added in v2: addressing MAJOR-38 (streams-native order dependency). -->

- `docs/proposals/headers-native.md` — `Request.headers` and `Response.headers`
  are both `[SameObject]` Headers IDL instances (Fetch §5.4 / §5.5). The
  guard machinery (request / request-no-cors / response / immutable / none)
  that headers-native deferred to "ship alongside Request/Response" lands
  here.
- `docs/proposals/compression-streams-native.md` — the `Content-Encoding`
  decompression hook. This design honours every commitment in that doc's
  "Dependencies on sibling projects" §, point-by-point in §XV.

**Unblocks:**
- compression's `with_response_body_hook` integration.
- the deletion of `crates/runtime/src/embed/fetch.js` (705 LOC).
- WPT regression for `fetch/api/{headers,request,response,abort}` —
  currently we run *zero* of those files because the polyfill is too far
  from spec to be worth the harness work.
- AI-builder reliability: every modern web library (langchain, LangGraph,
  the AI SDK, Stripe SDK, OpenAI SDK) leans on the spec-faithful body
  model (`response.body.getReader()` for SSE; `request.signal` propagation
  through abort chains). The polyfill silently breaks these.

## Revision history

- **v1 (2026-05-01)** — Initial design covering the entire WHATWG Fetch
  spec surface. Replaces the JS polyfill at
  `crates/runtime/src/embed/fetch.js` (705 LOC) and the Rust dispatch
  shim at `crates/runtime/src/transport/handler.rs` (386 LOC). Designed pure-native on
  V8 + Rust + cyper (compio + hyper, no tokio).
  Goes alongside the in-flight streams-native and compression-streams-native
  designs; the three together fully replace the JS-heavy fetch path.

- **v2 (2026-05-01)** — Round-2 critic-driven revision. Changes are
  marked inline with `(v2 fix:)` in the Decisions table where they
  apply. Highlights:
  - **Spec correctness.** Cross-origin redirect Authorization-strip
    narrowed to ONLY `Authorization` (was 4 headers; spec is 1 — Fetch
    §4.10 "CORS non-wildcard request-header name"). `request-body-header`
    list narrowed to 4 entries (was 5; dropped `Content-Length` —
    undici's deviation). Bad-port blocklist corrected to 83 entries
    sourced from the spec (was 78 from undici). `RequestMode` enum
    narrowed to 4 IDL values (`navigate`, `same-origin`, `no-cors`,
    `cors`); `RequestDestination` corrected to 22 entries (added
    `text`; dropped `webidentity`, `serviceworker`). Origin-header
    method exclusion narrowed to `{GET, HEAD}` (was 4 methods); Origin
    value now honours referrer-policy (`no-referrer` ⇒ `null`); Origin
    appended for `cors` mode plus `websocket` / `webtransport` modes.
    `X-HTTP-Method` family demoted to **conditional** forbidden (only
    when value parses to a forbidden method).
  - **AbortSignal.** Added `source signals` ↔ `dependent signals`
    bidirectional pairing (DOM §3.3.4). `signal abort` now collects
    `dependentSignalsToAbort` first, sets reason, runs abort steps for
    `signal`, fires the `"abort"` event via native EventTarget, then
    iterates dependents — no inline recursion. `AbortSignal.timeout`
    now keeps a strong ref from global to signal while listeners are
    registered (DOM-mandated GC retention). EventTarget moved out of
    `fetch/` into `crates/runtime/src/web/dom/event_target.rs` (it's a DOM
    primitive).
  - **`extract a body`.** Type-test predicates run BEFORE any
    `to_string`-style coercion. Spec dispatch order (Blob → byte
    sequence → BufferSource → FormData → URLSearchParams → scalar
    value string → ReadableStream) is preserved. String case uses
    proper USVString conversion (lone surrogate → U+FFFD), not
    `to_rust_string_lossy`. Body consumers' error types corrected:
    `json()` rejects with `SyntaxError` on parse failure; `bytes()`
    rejects with `RangeError` for >2GB; `text()` is infallible
    (UTF-8 with replacement). `arrayBuffer()` builds the ArrayBuffer
    via `v8::ArrayBuffer::new_backing_store_from_vec` instead of a
    cell-by-cell copy.
  - **hyper 1.x API.** Replaced `hyper::Body::from` /
    `hyper::Body::wrap_stream` (gone in hyper 1.x) with
    `http_body_util::{Full, BoxBody, StreamBody}` and cyper's
    `RequestBuilder::body(cyper::Body)` wrapper.
  - **cyper features.** Cargo.toml line **25** (not 33) shows
    `cyper = { version = "0.8", default-features = false, features = ["rustls", "json", "stream"] }`.
    The HTTP/1.1-only assumption is now correctly attributed to the
    fact that cyper's default build doesn't link the `h2` crate.
  - **Compression contract honoured.** Re-introduces the named
    `with_response_body_hook(hook: ResponseBodyHook)` API on
    `ResponseBuilder` per compression-streams-native §"From the
    native-fetch project". Fetch's `finalize_response_body` is the
    registered hook; the named API IS preserved for callers that
    register it (e.g. user-supplied codec extensions in v2). No more
    repudiation.
  - **Streams-native vs. fetch helpers.** `read_all_bytes` /
    `read_one_chunk` are NOT streams-native exports; they live in
    `crates/runtime/src/fetch/body_stream.rs` as fetch-internal
    helpers. They are layered on top of streams' public
    `getReader()` + `reader.read()` pattern, in Rust.
  - **`with_isolate_lock` removed.** Replaced with the actual runtime
    pattern (op_id allocation + `spawned_fetches` queue + `OpResult`
    dispatch on the main thread). Single-threaded V8: no lock to
    acquire.
  - **data-url crate.** D-21 now adds the `data-url` crate as a
    workspace dependency (no more "verify before implementation"
    aside). Removes the open verification.
  - **WPT inventory.** Recounted via `tests/wpt/fetch/api/` —
    ~135 `.any.js` files. v2 adds `redirect/`, `body/`, `basic/`,
    `credentials/` directories to the must-pass-v1 set so "full
    WHATWG compat" is delivered. Old "61 files" was 4-of-9 directories.
  - **Body mixin shape.** Picked a single answer: it is a Rust trait
    shared by `Request` and `Response`. There is NO V8 base class
    for `Body`. (`#[v8_inherit(Body)]` is dropped in §XIV.1; only
    `#[v8_inherit(EventTarget)]` for `AbortSignal` remains.)
  - **workerd compat-flag note.** `StripAuthorizationOnCrossOriginRedirect`
    is default-ON for compatibility dates ≥ 2025-09-01 (workerd's
    `compatEnableDate`). Most workers today (2026-05-01) run it on.
  - **Accept-Encoding ordering.** Single canonical ordering:
    `br, gzip, deflate` for HTTPS; `gzip, deflate` for HTTP.
  - **Order dependency.** §XVIII.1 acknowledges streams-native is
    partially shipped (RS+DefaultController+DefaultReader landed; WS+TS
    pending). Body skeleton can begin in parallel with streams' WS+TS
    work — only the final consumer wiring needs streams' RS reader,
    which has shipped.

## Top matter

### Goals

1. **Full WHATWG Fetch compliance.** Every interface in
   https://fetch.spec.whatwg.org/ at parity with the spec — no "v1
   subset". Pass the entire WPT `fetch/api/{headers,request,response,abort}`
   suite minus the deferred items called out in §I (CORS, service
   workers, navigation, multipart parsing).
2. **Replace the JS polyfill.** Delete `crates/runtime/src/embed/fetch.js`
   in three landings: (1) ship native behind feature flag, polyfill
   remains default; (2) flip default to native, polyfill remains as
   fallback; (3) delete the polyfill. Same cadence as headers-native
   (D-19 of streams).
3. **Body-as-stream.** `Request.body` and `Response.body` are both
   spec-mandated `ReadableStream`s. The polyfill stored bodies as
   strings; this is the dominant source of correctness bugs (`body.getReader()`
   silently returns synthetic chunks; `tee()` doesn't preserve binary;
   `pipeThrough(new DecompressionStream(...))` is impossible). The
   native design mints native ReadableStreams via the streams-native
   `from_native_source` API.
4. **Compio-native HTTP I/O.** All network work runs through
   `cyper::Client` (compio + hyper), already in the workspace per
   `Cargo.toml:33`. Zero tokio. No `Send` constraints inside the
   isolate (single-threaded per AGENTS.md "V8 per thread, one isolate
   per app"). The in-tree `crates/runtime/src/transport/ssrf.rs` already binds
   to cyper; the design extends rather than replaces that wiring.
5. **Spec-faithful, byte-faithful, observable-event-faithful.**
   The polyfill diverges in ~30 observable ways (no ByteString
   validation, lossy UTF-8 round-trips, snapshot iteration on
   `headers`, no Set-Cookie special-cases, body strings instead of
   streams, no clone-before-disturb assertions, no signal-aborted
   short-circuit before request creation, no Origin header, no
   Referer, redirects through cyper without spec rewriting). v1 fixes
   all of them.
6. **Streaming POST upload bodies.** `fetch(url, { body: stream })`
   sends the body over HTTP/1.1 chunked transfer-encoding (per
   §5.5 step 3). The polyfill silently coerced streams to strings,
   breaking real-time uploads (e.g. an LLM streaming JSONL to an
   evaluation harness, or `fetch(uploadUrl, { body: file.stream() })`).
7. **Spec-correct decompression.** Honour the codec hand-off documented
   in `compression-streams-native.md` so `Content-Encoding: gzip` /
   `deflate` / `br` / multi-coding chains all decode transparently
   before the user-visible `Response.body` is exposed. Today our
   fetch passes encoded bytes through unchanged; user code gets
   garbage from `.text()` / `.json()` on real-world API responses.

### Non-goals (explicit)

- **Service Workers and `FetchEvent`.** Out of scope. The platform's
  request dispatch is the gateway's manifest, not a service worker.
  WPT `service-workers/` is excluded. **Spec-divergence:** the spec's
  `request.client` / `request.window` / `request.serviceWorkers`
  fields are stored as fixed values (`null` / `"client"` / `"all"`)
  but never affect any algorithm — for a non-browser embedded
  runtime there is no traversable navigable.
- **Push streams (HTTP/2 / HTTP/3 server push).** No HTTP/2 or QUIC
  in v1; cyper's HTTP/1.1 is the v1 transport. <!-- v2 fix (CRITICAL-15/16, NIT-60): cyper actual feature list. -->
  The workspace pins (Cargo.toml line **25**, not 33):
  `cyper = { version = "0.8", default-features = false, features = ["rustls", "json", "stream"] }`.
  Notably **no `http2` feature** — cyper is HTTP/1.1-only as built. v2
  enables HTTP/2 via a single `cyper` feature flip
  (`features = [..., "http2"]`) once the upstream crate supports it as
  a feature toggle. The design is HTTP-version-agnostic at
  the V8 surface; the only impact of v2's HTTP/2 enablement is that
  `Transfer-Encoding: chunked` rule (§5.5 step 9.3) drops away on
  HTTP/2 connections.
- **The Cache API (`caches`, `Cache.match`, `Cache.put`).** This is
  a separate spec (https://w3c.github.io/ServiceWorker/#cache-interface),
  not Fetch. Out of scope. WPT `service-workers/cache-storage/`
  excluded.
- **HTTP cache (the `RequestCache` enum's behaviour).** The IDL
  surface IS implemented (parsing, defaulting, propagation through
  redirects), but the behavioural difference between `default` /
  `no-store` / `reload` / `no-cache` / `force-cache` /
  `only-if-cached` is **all the same** in v1 (no cache layer
  yet). `only-if-cached` returns a network error per the spec
  ("if the cache returns no match, return a network error";
  with no cache, every entry returns no match). The Cache-Control /
  Pragma / If-* header rewriting from `httpNetworkOrCacheFetch`
  steps 16-18 IS implemented per §V.4 — those are user-observable
  on the wire. Cache-mode-driven behavioural caching is v2.
  See D-15 below.
- **CORS enforcement (browser-style).** The runtime executes server-side
  creator code. There is no DOM, no cross-origin user, no cookies
  attached to a user's session. CORS request-mode / preflight /
  Access-Control-Allow-Origin checks are **bypassed** — every fetch
  is treated as `mode: "no-cors"` from the security-check perspective
  while still preserving the IDL surface (mode getter, etc.). This
  matches workerd (`http.h:765-773` explicitly states "These relate
  to CORS support, which we do not implement. WinterTC has determined
  that non-browser implementations that do not implement CORS support
  should ignore these entirely as if they were not defined."). See D-3.
- **Cookie jar.** The runtime is server-side. There is no per-user
  persistent cookie store. `Cookie` is a forbidden request-header
  the user cannot set (Fetch §2.2.2); they can only read `Set-Cookie`
  from response headers (which they DO need — `getSetCookie()` is
  implemented in headers-native). Out of scope: any automatic
  cookie-jar that persists across `fetch()` calls in the same isolate.
- **Subresource Integrity (`integrity` option).** The IDL surface is
  parsed and stored, but the bytesMatch validation in
  `mainFetch` step 20 is a no-op in v1. Add when first creator
  app needs it; trivial follow-up (~30 LOC).
- **Trust Tokens, Private State Tokens, Topics.** Browser-only; out.
- **Service-worker-intercepted fetches.** Out (no service workers).
- **`navigate` / `websocket` / `webtransport` request modes.** The
  runtime's WebSocket support flows through a separate path
  (`crates/runtime/src/websocket.rs`); fetch's `mode: "websocket"`
  is a no-op (returns network error per the spec when not in a
  navigation context). `navigate` / `webtransport` likewise.
- **`blob:` and `data:` URL schemes.** `data:` is in scope (cheap;
  the parser is small and the use case — inlined images for AI
  generation, base64 LLM input — is real). `blob:` is OUT (no Blob
  URL store; we don't ship `URL.createObjectURL` in v1). `file:` is
  OUT (security; SSRF-adjacent; matches workerd).
- **Multipart form-data PARSING for `request.formData()` /
  `response.formData()`.** The IDL method exists but in v1 only
  decodes `application/x-www-form-urlencoded` bodies. Multipart
  parsing (RFC 7578) is ~600 LOC of state machine; deferred to a
  follow-up (matches the polyfill's current scope).
- **`Request.duplex`.** The dictionary member is parsed (IDL
  surface), but only `"half"` is supported (the only standard
  value). `"full"` is a workerd-specific extension and is rejected
  with TypeError.
- **`Request.priority`.** Parsed (IDL); stored; never affects
  scheduling. v2 may surface to cyper's connection scheduler.
- **Keepalive (`request.keepalive` boolean).** Parsed; ignored.
  Browser-only feature (lets a fetch outlive page-unload). No
  meaning server-side.
- **Referrer policy enforcement.** The IDL is parsed and stored
  (the enum string round-trips), but `request.referrer` is always
  `null` on the wire — the runtime has no concept of a "current
  page" to be the referrer of. The Referer header is NEVER attached
  by the implementation. Matches workerd.

### Status

Draft v1 — **complete implementation** on `feature/fetch-native`.
Foundation chunks all shipped: DOM (EventTarget + AbortSignal +
FormData), Body + Request + Response, fetch() core and algorithms.

**D-23 polyfill cutover landings 1/2/3 — DONE:**

| Landing | Commit | Action |
|---------|--------|--------|
| 1 | `e282ce61` | native behind `ZEROSHIP_NATIVE_FETCH=1` |
| 2a | `87e451ed` | refactor inspect_response off polyfill body fields |
| 2b | `90b2d80e` | refactor HTTP_CREATE_REQUEST_JS to native Request |
| 2c | `05bf1624` | flip default; native fetch unconditional |
| 3a | `9445c50f` | slim fetch.js to DOMException + stream-bridge (705→154 LOC) |
| 3b | `d45800c8` | delete formdata.js polyfill (~250 LOC removed) |
| 3c | `934e51f8` | slim events.js to CustomEvent shim (73→25 LOC) |

**D-23 WPT pass-count expansion — DONE:**

| Commit | Scope |
|--------|-------|
| `56716d70` | WPT redirect — add mode + origin + location runners (35 sub-cases @ 100%) |
| `66c5618f` | WPT basic — add 5 in-process network runners (16 sub-cases @ 100%) |
| `3f986248` | spec gap — Headers immutable guard for Response.error() (response 38/0/1 was 37/1/1) |
| `851a33ba` | WPT abort — add fetch-level abort runner (4 sub-cases @ 100%) |
| `6e776e21` | spec gap — `new Request(req)` body identity preservation + WPT historical (request 38/0/5 was 37/1/5) |

Net polyfill LOC removed: ~1,000. The remaining JS embed code is the
DOMException polyfill + `__zsBeginStreamForward` kernel-bridge helper
in fetch.js (154 LOC), the CustomEvent shim in events.js (25 LOC), and
the unchanged blob.js / crypto.js / node-globals.js / text-streams.js /
url.js / websocket.js — none of which are fetch concerns.

**Landed (feature/fetch-native, foundation):**

| Commit | Scope |
|--------|-------|
| `4f7c6def` | runtime: native Body + Request + Response (foundation, no tests yet) |
| `96691f69` | runtime fetch-native: tests/fetch_body + ReadableStream-instanceof fix |
| `693ae7ef` | runtime fetch-native: tests/fetch_request — 17 hand-written tests |
| `19c28eea` | runtime fetch-native: tests/fetch_response — 17 hand-written tests |
| `8c792973` | runtime fetch-native: WPT request+response runners + spec gaps closed |
| `06d2e2c6` | runtime fetch-native: WPT body runner — 3 files, 3 pass / 6 skip |
| `a0610d20` | runtime fetch-native: duplex validation + disturbed-via-RSState |
| `0aa34c8b` | runtime fetch-native: native fetch() — algorithms + V8 entry + 57 unit tests |
| `a3c80a31` | runtime fetch-native: tests/fetch_native.rs — 24 hand-written tests |
| `56b2db77` | runtime fetch-native: WPT runners — basic (8/8) + redirect-method (17/17) |
| `c8ad3d13` | runtime fetch-native: graceful no-Runtime path + install smoke tests |

**D-23 cutover landings (commit hashes filled in as they land):**

| Commit | Scope |
|--------|-------|
| (this commit) | runtime fetch cutover landing 1 — native behind ZEROSHIP_NATIVE_FETCH (D-23 step 1) |

Post-completion: file as a date-prefixed ADR under
`docs/decisions/`. The Decisions table below is the immutable
contract; everything else is illustrative.

### Decisions (settled)

| # | Decision | Rationale | Section |
|---|----------|-----------|---------|
| **D-1** | Pure native: `Request`, `Response`, `fetch()` global, `AbortController`, `AbortSignal`, `FormData`, `EventTarget`, `Event` are `#[v8_class]` Rust types. The `Body` mixin is realised as a **Rust trait** (not a V8 base class) implemented by `Request` and `Response`; there is no separate `Body` JS type and no `Object.getPrototypeOf(req) === Body.prototype` relationship — the spec says "Body" is an IDL **mixin** (no constructor, no own object) and we honour that. No JS polyfill fallback once shipped. <br> *(v2 fix: was "`Body` mixin is a `#[v8_class]` base"; spec/Process-flaw 8 says mixin = Rust trait, not V8 base class. `#[v8_inherit(EventTarget)]` for AbortSignal stays. `#[v8_inherit(Body)]` is dropped.)* | Single source of truth; eliminates the body-as-string vs body-as-stream duality the polyfill carries. | §I |
| **D-2** | A body has `[[body]]` = `Option<BodyImpl>` where `BodyImpl = { stream: v8::Global<v8::Object>, source: BodySource, length: Option<u64> }`. The `stream` is a *native* `ReadableStream` (streams-native). The `source` is the original byte sequence / Blob handle / FormData buffer kept around for redirect-rewinding. *(v2 fix: workerd `http.h:67-99` "Buffer" cite was wrong — that range is method declarations, not the `Buffer` typedef. Pattern still mirrors workerd, but the cite is dropped; the Body source/buffer pattern is workerd-shaped, not at a specific line range.)* | Spec §3.2.1 step 13 ("Let body be a body whose stream is stream, source is source, and length is length"). The source is what enables 307/308 redirects on POST. | §III |
| **D-3** | CORS, request-mode, credentials-mode, destination, referrer, referrer-policy, isReloadNavigation, isHistoryNavigation, keepalive, integrity: **IDL parsed, stored, defaults set**, but **none affect the network fetch**. The `mode`/`credentials`/etc. getters return the stored value; algorithms that the spec gates on them (CORS-preflight, cross-origin cookie strip, etc.) treat the runtime as if `mode === "no-cors"` and `credentials === "include"` uniformly. Matches workerd `http.h:765-773` and the WinterTC consensus. | The runtime executes server-side creator code with no browser security context. CORS-enforcing here would be wrong (it would block server-side calls between creator backend and partner APIs, which is the entire use case). | §IV |
| **D-4** | Single-threaded per isolate: every Rust struct is `!Send + !Sync`. No `Mutex`/`RwLock`. Inter-class references use `Rc<RefCell<…>>`. `cyper::Client` is `thread_local!` (see existing `crates/runtime/src/transport/ssrf.rs:316-335`); the design preserves that. | AGENTS.md "V8 per thread, one isolate per app". | §VI |
| **D-5** | Response body bytes flow `cyper::Response::bytes_stream()` → optional codec chain (compression's `build_codec_chain`) → `ReadableStream::from_native_source(...)`. The user-visible `Response.body` is the OUTPUT of the chain; the chain is constructed before the JS Promise resolves to the Response object, so user code that calls `body.getReader()` never sees encoded bytes. Internal hand-off uses streams-native's `pipe_native_internal` (D-10 of streams) to bypass lock checks during construction. | This honours compression-streams-native.md §"From the native-fetch project" commitments 2 (decompression chain), 3 (Content-Encoding/Content-Length stripping), 4 (unknown coding → network error). | §V, §XV |
| **D-6** | Request body upload: `BodySource::Bytes(Vec<u8>)` → `Content-Length` header set, body sent as a single hyper Body buffer. `BodySource::Stream(v8::Global<ReadableStream>)` → drained via streams-native default reader (in Rust, no JS callbacks), each chunk written to hyper's chunked-encoding writer. The latter case sends `Transfer-Encoding: chunked`, NOT `Content-Length` (per §5.5 step 9.3). | Spec compliance for streaming uploads. The polyfill silently coerced streams to strings — broken for the AI SDK, file uploads, multipart streaming. | §V.4, §VI |
| **D-7** | `Body.text()` / `.json()` / `.arrayBuffer()` / `.bytes()` / `.blob()` / `.formData()` consume the body stream via the streams-native default reader's read-all pattern, NOT via JS callbacks. Each method: (a) checks `disturbed === false`, (b) acquires a default reader internally, (c) drains into a `Vec<u8>` accumulator on the Rust side, (d) decodes/parses, (e) resolves the returned Promise. | The polyfill ran the read loop in JS via `__readStreamToBytes`; it crossed the Rust↔V8 boundary once per chunk for what should be a single Rust-side allocation. | §III.4 |
| **D-8** | `clone()` semantics (§5.4 / §5.5 / §6.3 "clone a body"): tee the body stream via the public streams-native `tee()` algorithm; create a new Request/Response wrapping branch[1] of the tee while the original keeps branch[0]. This means cloning DISTURBS no body (per spec) and yields two independent consumers. The `source` (D-2) is shared via Rc — cloning the source is cheap. | Spec; required to match WPT `request-clone` and `response-clone`. workerd does the same in `http.c++:348-358`. | §III.5 |
| **D-9** | Native `AbortController` / `AbortSignal` (currently a JS polyfill in `embed/fetch.js:438-538`). New `#[v8_class]` types. The signal carries a list of pending abort callbacks and a `Cell<bool>` aborted flag plus a `RefCell<Option<v8::Global<Value>>>` reason. `fetch()` registers an abort callback that triggers fetch-controller termination. `AbortSignal.timeout(ms)` uses compio's `time::sleep` with the runtime executor; `AbortSignal.any(signals)` mirrors the polyfill semantics. | The polyfill is correct enough for the v1 stream-completion pattern but breaks for the spec-mandated `EventTarget` mixin (no real `dispatchEvent` semantics; no `once: true` listener option). Native EventTarget is shipped as part of this design. | §IX, §XII.4 |
| **D-10** | Native `EventTarget` base class (currently absent — the AbortSignal polyfill rolls its own). New `#[v8_class]` type providing `addEventListener`, `removeEventListener`, `dispatchEvent`. Used by AbortSignal and (future) WebSocket / EventSource. <br> *(v2 fix: file path moved from `crates/runtime/src/fetch/event_target.rs` to `crates/runtime/src/web/dom/event_target.rs`. EventTarget is a DOM primitive shared by AbortSignal, future WebSocket, future EventSource — it does not belong under `fetch/`. Per MAJOR-41.)* The `#[v8_class]` macro grows `#[v8_inherit(EventTarget)]` to plumb the prototype chain (XIV.1 below). | EventTarget is the spec-mandated base for AbortSignal (DOM §3.3 / Fetch references). Without it, `signal instanceof EventTarget === false`, which breaks duck-typing in libraries like `langgraph`. | §XII, §XIV.1 |
| **D-11** | Body source rewindability (§5.6 step 11): bodies created from byte sequences / Blobs / FormData / URLSearchParams ARE rewindable. Bodies created from a `ReadableStream` are NOT. On 307/308 with a non-rewindable body, the redirect chain fails with a network error per spec. workerd's `canRewindBody()` / `rewindBody()` is the model (`http.h:144-150`, `http.c++:201-224`). | The redirect rewriting (POST→GET on 301/302/303 — body becomes null; 307/308 — body retransmitted from source) is the most subtle redirect rule and the polyfill gets it wrong (always coerces to GET). | §VIII.3 |
| **D-12** | Redirect rewriting per spec §5.6 (HTTP-redirect fetch) is performed in **Rust**, not handed to cyper. cyper 0.8 doesn't follow redirects automatically (verified `crates/runtime/src/transport/ssrf.rs:6-11`); we replace the in-tree per-request close-on-redirect with a proper redirect loop in `mainFetch` that re-issues `httpFetch` per redirect, applying method/body rewrites and the cross-origin Authorization-strip rule. Max 20 redirects per spec. <br> *(v2 fix (CRITICAL-1, MAJOR-30): cross-origin strip is **only `Authorization`** per Fetch §4.10 "CORS non-wildcard request-header name" + step 13 of §5.6. NOT 4 headers (was Authorization, Proxy-Authorization, Cookie, Host). `Cookie` and `Host` are forbidden request headers anyway — user code can't set them. Workerd's `http.c++:1880` strips only `Authorization` and quotes the spec inline. Workerd's `StripAuthorizationOnCrossOriginRedirect` compat flag is `compatEnableDate("2025-09-01")` — i.e. **default-on** for compat dates ≥ 2025-09-01; today (2026-05-01) most workers run it on. We enable unconditionally.)* | Spec §5.6 + §4.10. We own this anyway because cyper doesn't follow. | §VIII |
| **D-13** | `Response` for null-body statuses (204, 205, 304, plus 101 for WebSocket upgrade): constructor throws TypeError if a non-null body is supplied (Fetch §5.5 step 7); this matches the existing polyfill behaviour (`embed/fetch.js:262-265`). For 101 specifically, the polyfill's `webSocket` init member (used by the gateway WebSocket path) is **preserved** as an extension (workerd's `http.h:951` does the same — `kj::Maybe<jsg::Ref<WebSocket>>`). | Compatibility with the gateway's WebSocket-upgrade flow at `crates/runtime/src/transport/handler.rs:138-194`. | §III.6 |
| **D-14** | `Request.cache` mode IDL parsed and stored; but cache-driven header rewrites in `httpNetworkOrCacheFetch` (steps 16-18) ARE applied (Pragma/Cache-Control insertion under `no-cache`/`no-store`/`reload`). No actual cache lookup happens (no cache layer in v1). `only-if-cached` returns a network error. This matches Workers' `Request::CacheMode` enum (`http.h:651-658`). | The header rewrites are user-observable on the wire even without a cache. | §V.4 |
| **D-15** | `Accept-Encoding` is added to outbound requests by default unless user code provided one (including empty string for opt-out). The canonical value is `br, gzip, deflate` for HTTPS and `gzip, deflate` for HTTP (HTTPS-prefers-brotli per undici). <br> *(v2 fix (MAJOR-31): canonicalised single ordering — was inconsistent: D-15 said `br, gzip, deflate`, §XV row 6 said `gzip, deflate, br`. Picked `br, gzip, deflate` (HTTPS) consistently.)* | Without this, the design's decompression integration is dead code on every request, because no server sends compressed bodies absent an `Accept-Encoding`. | §V.5, §XV row 6 |
| **D-16** | `Origin` header: appended to outbound requests when (a) method is not in `{GET, HEAD}`, OR (b) request mode is `cors`, OR (c) request mode is `websocket` / `webtransport`. Value is the origin of `request.url`, **subject to referrer policy** — under `"no-referrer"`, the value is the literal `"null"`. <br> *(v2 fix (MAJOR-22, MAJOR-23): was "method is not GET/HEAD/OPTIONS/TRACE" (4-method exclusion). Spec at §3.2.6 step 4 is GET/HEAD only (2-method exclusion). Spec also conditions Origin VALUE on referrer-policy and adds `cors` / `websocket` / `webtransport` modes to the trigger set.)* | RFC 9110 §10.2 + Fetch §3.2.6; many APIs require Origin on POST/PUT for CSRF defense. | §V.7 |
| **D-17** | Forbidden request headers (Fetch §2.2.2): the names listed in the spec PLUS `Proxy-*` and `Sec-*` prefixes. <br> The `X-HTTP-Method` family (`X-HTTP-Method`, `X-HTTP-Method-Override`, `X-Method-Override`) is **conditionally** forbidden — only when the value parses to a forbidden method (`CONNECT` / `TRACE` / `TRACK`) per Fetch §2.2.2 step 3. *(v2 fix (MAJOR-21): was unconditionally forbidden in v1.)* <br> v1 ENFORCES these on `Request.headers.set()` / `append()` via the headers-native guard machinery. Setting a forbidden header is a silent no-op (per spec — `validate` returns false; `set`/`append` short-circuit). | Spec; required for WPT `headers/headers-no-cors.any.js`. | §II.6, §IV.6 |
| **D-18** | `Method` validation: per Fetch §5.4 step 30, the `method` init member is normalized: `"DELETE"`/`"GET"`/`"HEAD"`/`"OPTIONS"`/`"POST"`/`"PUT"` are uppercased; `"CONNECT"`/`"TRACE"`/`"TRACK"` throw TypeError ("forbidden method"); other tokens pass through case-preserving (token validation against RFC 9110 tchar set; non-token throws TypeError). Mirrors deno's `validateAndNormalizeMethod` (deno `23_request.js:234-253`). | Spec; required for WPT `forbidden-method.any.js`. | §IV.4 |
| **D-19** | Bad-port blocklist (Fetch §4.3 "block bad port"): the **83-port** list from the spec is enforced at HTTP-fetch entry; matching ports throw a network error. List sourced **directly from the spec** (`tcpmux:1` through `amanda:10080`, plus `0`), NOT from undici. <br> *(v2 fix (CRITICAL-4): was "78 ports from undici"; spec has 83 (counting port 0). undici's list is 82 (missing port 0). WPT `request-bad-port.any.js` checks 83 ports verbatim — sourcing from spec avoids re-introducing undici's deviation.)* | Spec; required for WPT `request-bad-port.any.js`. | §V.1 |
| **D-20** | Spec algorithm naming in Rust: every named spec algorithm gets a Rust function with the same name in `snake_case`. Lives in `crates/runtime/src/fetch/algorithms.rs` for cross-class operations (`extract_body`, `main_fetch`, `http_fetch`, etc.) and in the relevant class file for class-local operations. Same rule as streams-native D-20. | Reduces cognitive load when cross-referencing the spec. | §III–VIII |
| **D-21** | `data:` URL scheme: implemented via the **`data-url` crate** (https://crates.io/crates/data-url, ~30k downloads/mo, MIT/Apache-2.0). Add as a workspace dep in `Cargo.toml` (`data-url = "0.3"`). Returns a Response with the parsed MIME type and decoded body. The fetch path branches on `currentURL.scheme` per §5.2 main-fetch step 11. <br> *(v2 fix (CRITICAL-20, Process-flaw 1): v1 said "via existing url.rs or hand-roll from undici (~150 LOC) — verify before implementation". The verification is done: the crate is NOT in the workspace; we add it now (resolves "decision-with-verification"). undici's `data-url.js` is ~270 LOC, not 150 — the hand-roll estimate was wrong. The `data-url` crate is the official Servo implementation, ~700 LOC total, well-tested.)* | Spec; cheap; real use case (inline images, JSON, signed payloads in URL form). | §V.6, §XII |
| **D-22** | The polyfill's `__rawFetch(method, url, headersJson, body)` JSON entry point (`fetch.rs:202-302`) is **deleted** in landing 2 (D-23) — every replacement code path goes through `Request` / `Response` V8 wrappers directly, no JSON marshalling. The headers-as-JSON path was a polyfill-era kludge and a real perf hit (1KB of headers = 1KB of JSON parse per request). Native takes the spec's path: `headersList: Vec<(Vec<u8>, Vec<u8>)>`, fed straight to hyper via the `http` crate's `HeaderMap`. | Removes ~50ns per request of JSON parse and removes a class of escaping bugs (header value with `"` in it). | §V.7 |
| **D-23** | Polyfill removal cadence: three landings — (1) ship native behind feature flag `runtime_native_fetch`, polyfill remains default; (2) flip default to native, polyfill remains as fallback; (3) delete polyfill entirely (`embed/fetch.js`). The native cutover (step 2) ALSO deletes `crates/runtime/src/transport/handler.rs::HTTP_CREATE_REQUEST_JS` and the JSON header marshalling helpers; the cyper integration in `crates/runtime/src/transport/ssrf.rs` is RETAINED (it has the SSRF resolver, the in-flight fetch limit, the per-thread client, and the body-streaming path) but its public callback is changed from `__rawFetch` to `fetch` itself. | Risk control. Identical pattern to headers-native D-19. | §XVI |
| **D-24** | SSRF protection (existing `is_blocked_ip` + `SsrfResolver` in `fetch.rs:36-163`) is preserved verbatim in the native path. The native fetch wraps `validate_url` before any DNS or socket work; the SSRF resolver remains a cyper resolver. Dev-mode bypass via `ZEROSHIP_DEV=1` retained. | Security; non-spec but mandatory for the platform. The polyfill never had this; the JS path called into Rust which applied it. | §V.1 |
| **D-25** | Per-isolate concurrent-fetch cap: 64 (existing `MAX_PENDING_FETCHES` in `state.rs`). Plus per-runtime `MAX_PENDING_OPS` (1024). Excess fetches throw `RangeError` synchronously from the `fetch()` callback. Verified against `fetch.rs:230-265`. | Bounds the resolver-map memory; bounds the per-thread cyper connection pool. The number is unchanged from the polyfill path. | §VI |
| **D-26** | Per-response body size cap: 10 MiB (existing `MAX_RESPONSE_SIZE` in `fetch.rs:25`). Preserved verbatim. Applied AFTER decompression (so the cap is on the user-visible byte count, NOT the wire bytes — matches Chrome/Firefox `network::ResourceRequestBody`). | Memory bound; matches existing polyfill behaviour. | §V.5, §VII |
| **D-27** | `setRequestReferrerPolicyOnRedirect` (§5.6 step 19) is a no-op in v1 (since referrers are ignored per D-3). The IDL field round-trips; no header is ever emitted. | Matches D-3. | §VIII.4 |
| **D-28** | Internal Server-Sent Events / chunked-streaming detection: NONE in v1. Response bodies are uniformly ReadableStream-of-bytes; the AI SDK / langgraph etc. parse SSE in user-space using `body.getReader()`. The polyfill had a path for SSE detection (it didn't; it just buffered everything as a string, which is why streaming was broken); native fixes this by always streaming. | The byte-stream-from-cyper path is the same code regardless of `text/event-stream` content-type. | §VII |
| **D-29** | Request body extraction throws **synchronously** from the constructor when init.body is invalid (per Fetch §5.4 step 36 + §3.2 "extract a body"). Examples: passing a disturbed ReadableStream, passing a `keepalive: true` with a stream body, passing a body of unknown type. The polyfill's body extraction is permissive and silently coerces. | WPT `request-disturbed.any.js`, `request-init-stream.any.js`. | §III.2 |
| **D-30** | The `Response.url` field is set to the **last** URL in the request's URL list (Fetch §5.5.4). For redirected responses, this is the post-redirect URL. For network-error responses, it is the empty string. For `Response.error()` it's empty. For `Response.redirect(url, status)` it's empty (workerd `http.c++:1171-1207` notes this is the spec). For non-redirected responses, it's the original request URL. | Spec; required for WPT `response-static-redirect.any.js`. | §III.6, §VIII.5 |

## I. Architecture overview

### I.1. The two-layer model

The native fetch implementation is split into two layers:

1. **Public IDL surface** — V8 classes installed on the global object:
   `Request`, `Response`, `Headers` (already shipped),
   `AbortController`, `AbortSignal`, `EventTarget`, `FormData`,
   plus the `fetch` global function. Each class carries an internal-
   field 0 holding `Box<{Class}State>` per the existing `#[v8_class]`
   pattern.

2. **Internal HTTP engine** — a Rust-side fetch pipeline that runs
   the spec algorithms (`mainFetch`, `httpFetch`, `httpRedirectFetch`,
   `httpNetworkFetch`) over a `cyper::Client` HTTP transport. The
   pipeline never calls into JS during the network phase; it operates
   entirely on Rust-side request/response state and resolves a
   `v8::PromiseResolver` once the response headers are in.

The boundary is sharp: V8 callbacks delegate to Rust methods on the
boxed state; Rust algorithm code reads body streams via the streams-
native `from_native_source` / `pipe_native_internal` APIs and reads
header lists via the headers-native list accessor. The user-facing
ReadableStream that the user sees as `response.body` is a *native*
ReadableStream wrapping a Rust source that pulls from the cyper body
stream (with optional codec adaptation in the middle).

```
┌────────────────────────────────────────────────────────────────┐
│ V8 isolate                                                     │
│  ┌──────────┐ ┌──────────┐ ┌──────────┐ ┌─────────────┐        │
│  │ Request  │ │ Response │ │ Headers  │ │ AbortSignal │  …     │
│  └────┬─────┘ └────┬─────┘ └────┬─────┘ └──────┬──────┘        │
│       │            │            │              │                │
│       ▼            ▼            ▼              ▼                │
│  ┌──────────┐ ┌──────────┐ ┌──────────┐ ┌─────────────┐        │
│  │ JS wrap  │ │ JS wrap  │ │ JS wrap  │ │ JS wrap     │        │
│  │ slot[0]: │ │ slot[0]: │ │ slot[0]: │ │ slot[0]:    │        │
│  │ Box<RS>  │ │ Box<RS>  │ │ Box<HL>  │ │ Box<ASig>   │        │
│  └────┬─────┘ └────┬─────┘ └──────────┘ └─────────────┘        │
│       │            │                                            │
│       ▼            ▼                                            │
│  ┌─────────────────────────┐                                    │
│  │ Box<RequestState> /     │                                    │
│  │ Box<ResponseState>      │                                    │
│  │  url, method, urlList,  │                                    │
│  │  redirect, mode, ...    │                                    │
│  │  headers: priv sym  ────┼──→ shares Headers wrapper          │
│  │  body: priv sym  ───────┼──→ shares ReadableStream wrapper   │
│  │  signal: priv sym ──────┼──→ shares AbortSignal wrapper      │
│  └─────────────────────────┘                                    │
│                                                                 │
│  ┌─────────────────────────────────────────────────────┐        │
│  │ globalThis.fetch                                    │        │
│  │   ↓                                                 │        │
│  │  fetch_callback (Rust)                              │        │
│  │   ↓                                                 │        │
│  │  push FetchRequest into spawned_fetches             │        │
│  └────────────┬────────────────────────────────────────┘        │
└───────────────┼──────────────────────────────────────────────────┘
                ▼
┌──────────────────────────────────────────────────────────────────┐
│ compio runtime (per-thread)                                      │
│                                                                  │
│  spawned_fetches → mainFetch → httpFetch → httpNetworkFetch      │
│                                              ↓                   │
│                                       cyper::Client.send()       │
│                                              ↓                   │
│                                       (TLS via rustls)           │
│                                              ↓                   │
│                                       hyper response             │
│                                              ↓                   │
│                          [Optional codec chain (build_codec_chain)] │
│                                              ↓                   │
│                          ReadableStream::from_native_source(...)  │
│                                              ↓                   │
│                          resolver.resolve(scope, response_obj)   │
└──────────────────────────────────────────────────────────────────┘
```

### I.2. File layout

```
crates/runtime/src/web/dom/                       <!-- v2 fix (MAJOR-41): EventTarget out of fetch/. -->
├── mod.rs                       (new) DOM primitives shared across fetch / WS / EventSource
├── event_target.rs              (new) EventTarget base class (~430 LOC; matches workerd's `events.c++` shape)
└── event.rs                     (new) Event base class (~120 LOC)

crates/runtime/src/fetch/
├── mod.rs                       (new) module root, public exports
├── request.rs                   (new) Request class + extractBody integration
├── response.rs                  (new) Response class + with_response_body_hook hook surface
├── body.rs                      (new) Body Rust trait + body-source enum + extract_body
├── body_stream.rs               (new) read_all_bytes / read_one_chunk fetch-internal helpers (v2 fix: CRITICAL-13 — NOT a streams-native deliverable)
├── form_data.rs                 (new) FormData class (URLSearchParams-only parser; multipart deferred)
├── abort_signal.rs              (new) AbortSignal class
├── abort_controller.rs          (new) AbortController class
├── algorithms.rs                (new) main_fetch, http_fetch, http_redirect_fetch, http_network_fetch, http_network_or_cache_fetch, scheme_fetch, fetch_finale, finalize_response_body
├── network.rs                   (new) cyper send + body pump glue (replaces fetch.rs send_and_stream_response)
├── data_url.rs                  (new) data-url crate adapter + synthetic Response constructor (v2 fix: depends on `data-url` crate per D-21)
├── redirects.rs                 (new) location-URL resolution + method/body rewrite + Authorization-only-strip + URL-list mgmt
├── ports.rs                     (new) bad-port blocklist (D-19) — sourced from spec, 83 entries
├── slots.rs                     (new) V8 private symbol helpers (mirrors streams/slots.rs pattern)
├── budget.rs                    (new) D-25 fetch concurrency cap (currently in state.rs; lifted out)
├── dictionaries.rs              (new) RequestInit / ResponseInit dictionary parsers
└── constants.rs                 (new) forbidden methods, forbidden headers, redirect statuses, null-body statuses, safe methods, request-body-header names

crates/runtime/src/transport/ssrf.rs       (split: SSRF resolver + thread-local cyper Client +
                                   in-flight counter move into fetch/network.rs;
                                   send_and_stream_response into fetch/algorithms.rs;
                                   raw_fetch_callback / spawn_body_reader / execute_fetch
                                   are removed — replaced by FetchTask + execute_fetch_native)
crates/runtime/src/transport/handler.rs        (deleted in landing 2 — JSON-helper path retired)
crates/runtime/src/embed/fetch.js (deleted in landing 3)
crates/runtime/src/lib.rs         (modified) +pub mod fetch;
crates/runtime/src/core/init.rs        (modified) install Request / Response /
                                              FormData / AbortController /
                                              AbortSignal / EventTarget classes;
                                              install fetch global

crates/runtime-macros/src/v8_class.rs  (modified) §XIV
crates/runtime-macros/src/lib.rs       (modified) §XIV

crates/runtime/tests/
├── fetch_request.rs              (new) hand-written Request constructor tests
├── fetch_response.rs             (new) hand-written Response constructor tests
├── fetch_body.rs                 (new) body extract / consume / clone tests
├── fetch_redirects.rs            (new) redirect chain tests against a local cyper::Server
├── fetch_abort.rs                (new) AbortSignal tests
├── fetch_compression.rs          (new) Content-Encoding integration tests
├── wpt_fetch.rs                  (new) WPT runner (mirrors wpt_streams.rs)
└── wpt/fetch/api/                (vendored — see §XVI)
```

### I.3. The fetch dispatch pipeline (algorithm-level)

Per the spec, `fetch(input, init)` is §5.1 *fetch method*; it sets up
the fetch params and calls `fetching(fetchParams)` (§5.2 *fetching*),
which calls `mainFetch(fetchParams, false)` (§5.3 *main fetch*), which
dispatches by URL scheme via `schemeFetch(fetchParams)` (§5.4) — for
`http:`/`https:` schemes that means `httpFetch(fetchParams)` (§5.5).
`httpFetch` calls `httpNetworkOrCacheFetch(fetchParams)` (§5.6), which
calls `httpNetworkFetch(fetchParams)` (§5.7). `httpFetch` also handles
redirects via `httpRedirectFetch(fetchParams, response)` (§5.6 actually)
which calls back into `mainFetch(fetchParams, true)`.

The spec's §5 is mutually recursive across these algorithms; the
design preserves the structure 1:1 because debugging mismatches against
WPT requires being able to grep the spec function name and find the
Rust function.

The algorithm names (and their Rust equivalents) are listed exhaustively
in §V below.

### I.4. Boundary between Rust and JS during fetch

The native fetch path crosses Rust↔V8 a small, fixed number of times
per request:

1. **JS → Rust** at `fetch(input, init)` callback entry (1 V8 enter).
2. **Rust → V8 → JS** at `Request` construction inside `fetch()` if
   the input is a string (1 V8 enter to mint the Request wrapper),
   plus 1 enter to validate init dict + read user-supplied headers/body.
3. (No JS hops during the network phase.)
4. **Rust → JS** at promise resolution, to:
   - mint the `Response` wrapper object,
   - mint the user-visible `ReadableStream` wrapping the body source,
   - resolve the `fetch()` Promise.

Compare to the polyfill which crosses Rust↔V8 ~20 times per request:
construction + 1 hop per header for header-list extraction + 1 hop
per chunk for body streaming + 1 hop to decode JSON-encoded headers
on the response side + 1 hop to coerce the body string back to a
TextDecoder-decoded string. The dominant cost in the polyfill profile
is V8 entry/exit; native eliminates it.

### I.5. AGENTS.md compliance audit

| Invariant | Status |
|-----------|--------|
| Zero tokio | OK — uses `cyper` (compio + hyper); `cyper::Client::builder()` per `crates/runtime/src/transport/ssrf.rs:327`. cyper Cargo.toml feature list (workspace `Cargo.toml` line 25, verified): `default-features = false, features = ["rustls", "json", "stream"]`. No `tokio` transitive dep. *(v2 fix CRITICAL-15/16: was wrongly cited as `["client", "http1"]` in v1.)* |
| V8 per thread, one isolate per app | OK — `thread_local! CLIENT` matches; no cross-thread state introduced. |
| typed_id everywhere | N/A (fetch isn't a typed entity in the platform sense). |
| Wire formats are immutable contracts | OK — Request / Response IDL is the wire format and we match the WHATWG spec exactly. The internal `FetchTask` struct in `fetch/network.rs` is a private contract between the fetch callback and the runtime pump; changes freely. *(v2 fix Process-flaw 4: was `FetchRequest` in v1 — renamed for consistency with the spec's "fetch params" terminology.)* |
| Native primitives are the kernel | OK — `fetch` is an npm-equivalent surface (the spec global), implementation in Rust. The 6 NEW V8 globals (Request, Response, FormData, AbortController, AbortSignal, EventTarget+Event) are DOM/spec primitives, not platform-specific zeroship globals. EventTarget+Event live in `crates/runtime/src/web/dom/` (shared with future WebSocket / EventSource), the rest in `crates/runtime/src/fetch/`. *(v2 fix Process-flaw 6: justifies the kernel-surface expansion — these are platform-mandated by spec; if we don't add them, no language-spec library works.)* |
| The gateway is dumb | N/A (fetch lives in worker, not gateway). |

## II. IDL surface — exhaustive

The complete spec inventory of interfaces (Fetch §5):

| # | Interface | Section | Internal-field count | LOC est. (Rust+macro) |
|---|-----------|---------|----------------------|-----------------------|
| 1 | `Headers` | §5.2 | 1 (Box<HeaderList>) | (already shipped — headers-native) |
| 2 | `Body` (mixin) | §5.3 | n/a — folded into Request/Response | ~120 |
| 3 | `Request` | §5.4 | 1 (Box<RequestState>) | ~600 |
| 4 | `Response` | §5.5 | 1 (Box<ResponseState>) | ~480 |
| 5 | `fetch` (global function) | §5.1 | n/a (free function) | ~150 |
| 6 | `FormData` | (FileAPI) | 1 (Box<FormDataState>) | ~250 |
| 7 | `AbortController` | (DOM §3.3) | 1 | ~80 |
| 8 | `AbortSignal` | (DOM §3.3) | 1 | ~250 |
| 9 | `EventTarget` (base) | (DOM §2) | 1 | ~200 |
| 10 | `Event` | (DOM §2) | 1 | ~120 |

Items 9 and 10 are not strictly Fetch; they are dependencies of
`AbortSignal`, which Fetch's `Request.signal` exposes. v1 ships them
as part of this design because Fetch can't be spec-compliant without
them (`signal instanceof EventTarget`).

### II.1. `Headers` — already shipped

Per `headers-native.md`. v1 of this design adds the **guard machinery**
that headers-native deferred (`request`, `request-no-cors`, `response`,
`immutable`). The headers struct grows a `Guard` enum field; `validate`
gets the full §2.2 algorithm; forbidden-name lists move from
`fetch/constants.rs` into `headers.rs::guard.rs`. ~200 LOC of additions
to the existing `crates/runtime/src/web/headers.rs`.

### II.2. `Body` mixin (§5.3)

```webidl
interface mixin Body {
  readonly attribute ReadableStream? body;
  readonly attribute boolean bodyUsed;
  [NewObject] Promise<ArrayBuffer> arrayBuffer();
  [NewObject] Promise<Uint8Array> bytes();
  [NewObject] Promise<Blob> blob();
  [NewObject] Promise<FormData> formData();
  [NewObject] Promise<any> json();
  [NewObject] Promise<USVString> text();
};
```

<!-- v2 fix (CRITICAL Process-flaw 8): pick ONE shape — Rust trait, no V8 base class. -->
**Decision (v2): the `Body` mixin is a Rust trait `BodyOps`,
implemented separately on `RequestState` and `ResponseState`.** It is
NOT a V8 base class. There is no `new Body()`, no
`Object.getPrototypeOf(req) === Body.prototype`, no shared internal
field. Per the WebIDL spec, an `interface mixin` does not produce its
own JS object — its members are installed directly on each
includer's prototype. Both `Request.prototype.text` and
`Response.prototype.text` exist as separate own properties; both
delegate to the same Rust function `body_consume_text` parameterised
on the includer's body slot.

```rust
pub trait BodyOps {
    fn body_slot(&self) -> &RefCell<Option<BodyImpl>>;
    fn mime_type(&self, scope: &mut v8::PinScope) -> Option<Vec<u8>>;
}

// shared in fetch/body.rs:
pub async fn body_consume_text<T: BodyOps>(
    state: &T, scope: &mut v8::PinScope,
) -> Result<v8::Global<v8::String>, v8::Global<v8::Value>> { ... }
```

This means the macro extension XIV.1 only ships
`#[v8_inherit(EventTarget)]` for AbortSignal — `#[v8_inherit(Body)]`
is dropped; `Body` was never a V8 type to begin with. The Rust trait
is invisible to JS (`req instanceof Body` would be a `ReferenceError`
because there is no `Body` global, exactly per spec).

### II.3. `Request` (§5.4)

**IDL (§5.4):**

```webidl
typedef (Request or USVString) RequestInfo;

[Exposed=(Window,Worker)]
interface Request {
  constructor(RequestInfo input, optional RequestInit init = {});

  readonly attribute ByteString method;
  readonly attribute USVString url;
  [SameObject] readonly attribute Headers headers;

  readonly attribute RequestDestination destination;
  readonly attribute USVString referrer;
  readonly attribute ReferrerPolicy referrerPolicy;
  readonly attribute RequestMode mode;
  readonly attribute RequestCredentials credentials;
  readonly attribute RequestCache cache;
  readonly attribute RequestRedirect redirect;
  readonly attribute DOMString integrity;
  readonly attribute boolean keepalive;
  readonly attribute boolean isReloadNavigation;
  readonly attribute boolean isHistoryNavigation;
  readonly attribute AbortSignal signal;
  readonly attribute RequestDuplex duplex;

  [NewObject] Request clone();
};
Request includes Body;

dictionary RequestInit {
  ByteString method;
  HeadersInit headers;
  BodyInit? body;
  USVString referrer;
  ReferrerPolicy referrerPolicy;
  RequestMode mode;
  RequestCredentials credentials;
  RequestCache cache;
  RequestRedirect redirect;
  DOMString integrity;
  boolean keepalive;
  AbortSignal? signal;
  RequestDuplex duplex;
  RequestPriority priority;
  any window;  // can only be set to null
};

// v2 fix (CRITICAL-6): 22 entries per Fetch spec (current head). v1 had 24:
// added "webidentity"/"serviceworker" (not in spec); omitted "text" (in spec).
enum RequestDestination {
  "", "audio", "audioworklet", "document", "embed", "font", "frame",
  "iframe", "image", "json", "manifest", "object", "paintworklet",
  "report", "script", "sharedworker", "style", "text", "track",
  "video", "worker", "xslt"
};

// v2 fix (CRITICAL-5): 4 IDL values per spec. v1 had 6:
// "websocket"/"webtransport" appear in spec PROSE for internal request states
// (set by the WebSocket()/WebTransport() constructors), but are NOT in the
// IDL enum. User code passing mode: "websocket" must throw TypeError per
// WebIDL enum coercion (WPT request-init-002.any.js verifies this).
enum RequestMode { "navigate", "same-origin", "no-cors", "cors" };
enum RequestCredentials { "omit", "same-origin", "include" };
enum RequestCache {
  "default", "no-store", "reload", "no-cache", "force-cache", "only-if-cached"
};
enum RequestRedirect { "follow", "error", "manual" };
enum RequestPriority { "high", "low", "auto" };
enum RequestDuplex { "half" };

enum ReferrerPolicy {
  "", "no-referrer", "no-referrer-when-downgrade", "same-origin",
  "origin", "strict-origin", "origin-when-cross-origin",
  "strict-origin-when-cross-origin", "unsafe-url"
};
```

**Rust state (boxed in V8 internal field 0):**

```rust
pub struct RequestState {
    /// Spec [[method]]. Validated at construction (D-18).
    pub method: ByteString,
    /// Spec [[urlList]]. Last entry is the current URL; first entry
    /// is the original URL. Grows on redirect (§5.6 step 18).
    pub url_list: RefCell<Vec<url::Url>>,
    /// Spec [[headers]]. Pointer to a JS Headers wrapper (lazy init —
    /// lives in priv sym `headers` for [SameObject]).
    pub headers_priv: PrivSymKey,
    /// Spec [[body]]. None for null-body methods (GET/HEAD), Some(BodyImpl)
    /// otherwise. Source vs. stream pattern per workerd `http.h:114-121`.
    pub body: RefCell<Option<BodyImpl>>,
    /// Spec [[redirect]]. Default "follow".
    pub redirect: Cell<RedirectMode>,
    /// Spec [[mode]]. Default "cors" for string-input, "no-cors" for
    /// Request-input (per §5.4 step 13). Stored but unused (D-3).
    pub mode: Cell<RequestMode>,
    /// Spec [[credentials]]. Default "same-origin". Stored but unused (D-3).
    pub credentials: Cell<Credentials>,
    /// Spec [[cache]]. Default "default". USED for header rewriting
    /// (§5.6 steps 16-18) per D-14.
    pub cache: Cell<CacheMode>,
    /// Spec [[destination]]. Default "" (empty string). Stored but unused (D-3).
    pub destination: Cell<Destination>,
    /// Spec [[referrer]]. Default "client" (string). Stored as enum or URL.
    /// Stored but never written to wire (D-3, D-27).
    pub referrer: RefCell<Referrer>,
    /// Spec [[referrerPolicy]]. Default "" (empty string). Stored but
    /// unused (D-3).
    pub referrer_policy: Cell<ReferrerPolicy>,
    /// Spec [[integrity]]. Default "". Stored; not enforced in v1
    /// (deferred per Non-goals).
    pub integrity: RefCell<String>,
    /// Spec [[keepalive]]. Default false. Stored; ignored in v1 (Non-goals).
    pub keepalive: Cell<bool>,
    /// Spec [[isReloadNavigation]] / [[isHistoryNavigation]]. Both false.
    /// Stored but unused (D-3).
    pub reload_navigation: Cell<bool>,
    pub history_navigation: Cell<bool>,
    /// Spec [[signal]]. Pointer to a JS AbortSignal wrapper (lives in
    /// priv sym `signal`). Always non-null (Fetch §5.4 step 31 mints
    /// a new signal if init.signal was undefined).
    pub signal_priv: PrivSymKey,
    /// Spec [[duplex]]. Default "half". Only "half" is accepted (D-1's
    /// non-goals). Stored.
    pub duplex: Cell<Duplex>,
    /// Spec [[priority]]. Default "auto". Stored; never affects cyper
    /// scheduling in v1.
    pub priority: Cell<Priority>,
    /// Spec [[redirect-count]]. Bumped on each redirect (§5.6 step 8).
    /// Initialised at 0 by the constructor.
    pub redirect_count: Cell<u32>,
    /// Spec [[use-URL-credentials flag]]. Set by §5.4 step 26 / §5.6.
    /// (v2 fix MAJOR-29: was mis-labeled as "[[urlList processed]]" in v1
    /// — that's not a spec slot. Per https://fetch.spec.whatwg.org/#use-url-credentials-flag.)
    pub use_url_credentials: Cell<bool>,
    /// Spec [[unsafe-request]] flag. Always true for user-constructed Requests
    /// (§5.4 step 12). Never observable.
    pub unsafe_request: Cell<bool>,
    /// Spec [[done]] flag. Set to true at end of fetch. Not user-visible.
    pub done: Cell<bool>,
    /// Spec [[timing-allow-failed]]. Always false in v1 (no timing).
    pub timing_allow_failed: Cell<bool>,
    /// Self-pointer back to the V8 wrapper object. Used to mint clones.
    pub self_weak: WeakV8Ref,
}
```

`PrivSymKey` is a thin newtype over a static `&'static OneByteConst`
pointing to the symbol description (`b"headers"`, `b"signal"`, etc.).
The actual private symbol is created once per realm; helpers in
`fetch/slots.rs` follow the streams `slots.rs` pattern.

### II.4. `Response` (§5.5)

**IDL (§5.5):**

```webidl
[Exposed=(Window,Worker)]
interface Response {
  constructor(optional BodyInit? body = null, optional ResponseInit init = {});

  [NewObject] static Response error();
  [NewObject] static Response redirect(USVString url, optional unsigned short status = 302);
  [NewObject] static Response json(any data, optional ResponseInit init = {});

  readonly attribute ResponseType type;
  readonly attribute USVString url;
  readonly attribute boolean redirected;
  readonly attribute unsigned short status;
  readonly attribute boolean ok;
  readonly attribute ByteString statusText;
  [SameObject] readonly attribute Headers headers;

  [NewObject] Response clone();
};
Response includes Body;

dictionary ResponseInit {
  unsigned short status = 200;
  ByteString statusText = "";
  HeadersInit headers;
};

enum ResponseType {
  "basic", "cors", "default", "error", "opaque", "opaqueredirect"
};
```

**Rust state:**

```rust
pub struct ResponseState {
    /// Spec [[type]]. Default "default" for user-constructed responses,
    /// "basic"/"cors"/etc. for fetch-returned responses. The Network
    /// Error pseudo-response uses "error" (§5.5 `Response.error()`).
    pub r#type: Cell<ResponseType>,
    /// Spec [[urlList]]. Empty for user-constructed; populated by
    /// fetch with the request's URL list. `Response.url` returns the
    /// last entry (or "" if empty). D-30.
    pub url_list: RefCell<Vec<url::Url>>,
    /// Spec [[redirected]]. True iff urlList.len() > 1.
    pub redirected: Cell<bool>,
    /// Spec [[status]]. 0 for network errors, 200 default, ranges 200-599
    /// validated.
    pub status: Cell<u16>,
    /// Spec [[statusText]].
    pub status_text: RefCell<Vec<u8>>,
    /// Spec [[headers]]. Pointer to a JS Headers wrapper.
    pub headers_priv: PrivSymKey,
    /// Spec [[body]]. None when status is null-body or method is HEAD/CONNECT
    /// or for redirects, Some otherwise.
    pub body: RefCell<Option<BodyImpl>>,
    /// Spec [[bodyInfo]] (encoded size, content type) — used for Resource
    /// Timing. Tracked but never surfaced (no PerformanceObserver yet).
    pub body_info: RefCell<BodyInfo>,
    /// Spec [[CORS-exposed-header-name list]]. Empty in v1 (D-3).
    pub cors_exposed: RefCell<Vec<Vec<u8>>>,
    /// Spec [[range-requested]] flag. True if Range header was sent.
    pub range_requested: Cell<bool>,
    /// Spec [[request-includes-credentials]]. Stored from
    /// httpNetworkOrCacheFetch step 13 for IDL completeness (D-3).
    pub request_includes_credentials: Cell<bool>,
    /// Spec [[timing-allow-passed]] flag. Always true in v1 (no timing).
    pub timing_allow_passed: Cell<bool>,
    /// Spec [[has-cross-origin-redirects]]. False in v1 (no security context).
    pub has_cross_origin_redirects: Cell<bool>,
    /// Filtered-response rule: a "basic" filtered response wraps an
    /// "internal" response. When `internal_response: Some(...)`, header
    /// access goes through CORS-filtering (cors), null-body filtering
    /// (opaque), etc. v1 sets this to None always (no filtering — D-3).
    pub internal_response: RefCell<Option<v8::Global<v8::Object>>>,
    /// WebSocket upgrade extension (D-13). Set when init.webSocket is
    /// supplied AND status == 101. Read by the gateway WebSocket dispatch
    /// path at `crates/runtime/src/transport/handler.rs:181-194` (which lives until
    /// landing 2 of D-23, then moves into the native Response inspect).
    pub web_socket_priv: PrivSymKey,
    pub self_weak: WeakV8Ref,
}
```

### II.5. `BodyInit` typedef (§3.2)

```webidl
typedef (ReadableStream
       or Blob
       or BufferSource
       or FormData
       or URLSearchParams
       or USVString) BodyInit;
```

The extraction algorithm dispatches on the concrete type — see §III.

### II.6. Forbidden header lists (Fetch §2.2)

**Forbidden request-header names — unconditional** (case-insensitive):
`accept-charset`, `accept-encoding`, `access-control-request-headers`,
`access-control-request-method`, `access-control-request-private-network`,
`connection`, `content-length`,
`cookie`, `cookie2`, `date`, `dnt`, `expect`, `host`, `keep-alive`,
`origin`, `referer`, `set-cookie`, `te`, `trailer`,
`transfer-encoding`, `upgrade`, `via`, plus any name starting
with `proxy-` or `sec-`.

**Forbidden request-header names — conditional** (Fetch §2.2.2 step 3):
`x-http-method`, `x-http-method-override`, `x-method-override` are
forbidden **only when their value parses to a forbidden method**
(`CONNECT` / `TRACE` / `TRACK`). Setting `X-HTTP-Method-Override: PUT`
is allowed; setting `X-HTTP-Method-Override: TRACE` is blocked. <!-- v2 fix MAJOR-21. -->

**Forbidden response-header names**: `set-cookie`, `set-cookie2`.

These lists live in `crates/runtime/src/fetch/constants.rs` as static
arrays. The `Headers::validate` algorithm (extended in v1 per §IV.6
to gain a `Guard` field) consults them based on the headers' guard.
For the conditional `X-HTTP-Method*` family, `validate` parses the
candidate value with `validate_and_normalize_method` (D-18); if the
value normalises to `CONNECT` / `TRACE` / `TRACK`, the set/append is
a no-op. Per WPT `headers-no-cors.any.js`.

### II.7. `RequestInfo` and `RequestInit` parsing (per §5.4)

```rust
pub enum RequestInfo {
    Url(url::Url),
    Request(v8::Global<v8::Object>),
}

pub struct RequestInit {
    method: Option<ByteString>,
    headers: Option<HeadersInit>,
    body: Option<Option<BodyInit>>,  // outer Option for "key present?", inner for null
    referrer: Option<String>,
    referrer_policy: Option<ReferrerPolicy>,
    mode: Option<RequestMode>,
    credentials: Option<Credentials>,
    cache: Option<CacheMode>,
    redirect: Option<RedirectMode>,
    integrity: Option<String>,
    keepalive: Option<bool>,
    signal: Option<Option<v8::Global<v8::Object>>>,
    duplex: Option<Duplex>,
    priority: Option<Priority>,
    window: WindowInit,  // can only be `null` or absent (otherwise TypeError)
}

pub enum WindowInit { Absent, Null, NotNull /* throws */ }
```

Dictionary parsing lives in `fetch/dictionaries.rs`. See §IV.

## III. Body model — the heart of the design

### III.1. The two-headed body

Every body has TWO representations that coexist:

1. **`stream: v8::Global<v8::Object>`** — a native ReadableStream
   that JS code reads via `body.getReader()` / `body.tee()` / etc.
   This is the user-visible representation.
2. **`source: BodySource`** — the original byte sequence (or Blob /
   FormData reference). Kept around so that:
   - **`clone()`** can tee the stream AND clone the source — see D-8.
   - **redirect** can rewind the body when retransmitting on
     307/308 — see D-11.
   - **streaming uploads** know whether they have a `Content-Length`
     (`Source::Bytes(b)` ⇒ length b.len()) or must use chunked
     encoding (`Source::Stream` ⇒ no length).

```rust
pub struct BodyImpl {
    /// JS-visible ReadableStream. Always present for non-null bodies.
    pub stream: v8::Global<v8::Object>,
    /// Original source, for retransmission. None when the user passed
    /// a ReadableStream as the body — those are NOT rewindable.
    pub source: BodySource,
    /// Spec [[length]]. Some(n) for byte-counted sources, None for
    /// stream sources or unknown-size FormData with file parts.
    pub length: Option<u64>,
}

pub enum BodySource {
    /// Bytes (from a string, ArrayBuffer, ArrayBufferView).
    Bytes(Rc<Vec<u8>>),
    /// Blob — for v1 we don't ship Blob streaming, so this is just a
    /// byte buffer behind a Rc.
    Blob(Rc<Vec<u8>>, Option<String> /* type */),
    /// URLSearchParams — serialized to bytes at extract time.
    UrlSearchParams(Rc<Vec<u8>>),
    /// FormData — multipart-encoded at extract time.
    FormData(Rc<Vec<u8>>, String /* boundary */),
    /// Stream — NOT rewindable. The stream is the only handle.
    Stream,
}
```

### III.2. `extract a body` algorithm (Fetch §3.2)

The Fetch spec's "extract a body" algorithm
(https://fetch.spec.whatwg.org/#concept-bodyinit-extract) takes a
`BodyInit` and a boolean `keepalive`, and returns `(body, type)` where
`type` is the inferred Content-Type string (or null).

<!-- v2 fix (CRITICAL-10, CRITICAL-11): rewrote dispatch order and string conversion.
     - Spec §3.2 step 11 dispatch order: Blob → byte sequence (BufferSource) →
       FormData → URLSearchParams → scalar value string → ReadableStream.
     - All type-tests with explicit predicates BEFORE any to_string call
       (otherwise a Blob would be silently coerced via toString to "[object Blob]").
     - String case uses USVString conversion (lone surrogates → U+FFFD), not
       to_rust_string_lossy. -->

Rust implementation in `fetch/body.rs`:

```rust
pub fn extract_body(
    scope: &mut v8::PinScope,
    object: v8::Local<v8::Value>,
    keepalive: bool,
) -> Result<(BodyImpl, Option<String>), OpError> {
    // STEP 1: ReadableStream gets special treatment (it is one of the BodyInit
    // union members but its handling differs from "buffer-into-stream").
    //
    // We test for it BEFORE the switch. Per spec §3.2 the ReadableStream case
    // sets stream = object (no source); for other BodyInit types, a fresh
    // stream is built from the source. Also, only the ReadableStream case
    // can throw on `keepalive` or on disturbed/locked input.
    if is_readable_stream(scope, object) {
        if keepalive {
            return Err(OpError::type_error("keepalive cannot be used with a ReadableStream body"));
        }
        if streams::is_disturbed_obj(scope, object) || streams::is_locked_obj(scope, object) {
            return Err(OpError::type_error("Body was already used or is locked."));
        }
        let stream_global = v8::Global::new(scope, object.to_object(scope).unwrap());
        return Ok((
            BodyImpl { stream: stream_global, source: BodySource::Stream, length: None },
            None,
        ));
    }

    // STEP 2-10: spec dispatch order — Blob → byte sequence (BufferSource) →
    // FormData → URLSearchParams → scalar value string. We use predicate tests
    // FIRST (in spec order), and ONLY fall through to USVString conversion
    // when no concrete type matched. (v2 fix CRITICAL-10: v1 ran
    // to_rust_string_lossy_or_typed FIRST, which would coerce a Blob via
    // toString and never reach the Blob branch.)
    let mut content_type: Option<String> = None;
    let bytes: Rc<Vec<u8>>;
    let mut length: Option<u64> = None;
    let source: BodySource;

    if is_blob(scope, object) {
        // Spec: source IS the Blob. We materialise for v1 (no streaming Blob);
        // v2 may stream via Blob.stream(). Note: per spec, length is the
        // Blob's size, content-type is the Blob's `type` if non-empty.
        let (b, blob_type) = blob_to_bytes(scope, object);
        let rc = Rc::new(b);
        length = Some(rc.len() as u64);
        if !blob_type.is_empty() {
            content_type = Some(blob_type);
        }
        bytes = rc.clone();
        source = BodySource::Blob(rc, content_type.clone());
    } else if is_array_buffer(object) || is_array_buffer_view(object) {
        // Byte sequence / BufferSource case (§3.2). Copy out the bytes
        // synchronously (spec note: "if object is a buffer source, set source
        // to a copy of the bytes held by object" — the COPY is required
        // because the user could mutate the underlying buffer afterwards).
        let b = bytes_from_buffer_source(scope, object)?;
        let rc = Rc::new(b);
        length = Some(rc.len() as u64);
        bytes = rc.clone();
        source = BodySource::Bytes(rc);
    } else if is_form_data(scope, object) {
        // multipart/form-data encoding (§3.2 + RFC 7578).
        // v2 note (MAJOR-24): spec says "set length to ... [unclear, see
        // html/6424]" — the unresolved spec issue means we MUST NOT advertise
        // a Content-Length when a FormData body has file parts. We special-case:
        // if any entry is a Blob, length = None (unknown to spec); if all
        // entries are strings, length = Some(serialized.len()) — workerd does
        // this distinction.
        let boundary = generate_boundary();
        let b = form_data_serialize(scope, object, &boundary)?;
        let rc = Rc::new(b);
        if form_data_is_string_only(scope, object) {
            length = Some(rc.len() as u64);
        } else {
            length = None;
        }
        content_type = Some(format!("multipart/form-data; boundary={}", boundary));
        bytes = rc.clone();
        source = BodySource::FormData(rc, boundary);
    } else if is_url_search_params(scope, object) {
        // Per spec, the byte sequence is `application/x-www-form-urlencoded`-
        // serialised directly from the URLSearchParams' name-value pairs.
        // We read the byte-faithful representation via the URLSearchParams
        // serialiser (matches `url::form_urlencoded::Serializer`); this
        // avoids round-tripping through a JS string.
        let b = url_search_params_serialize_bytes(scope, object);
        let rc = Rc::new(b);
        length = Some(rc.len() as u64);
        content_type = Some("application/x-www-form-urlencoded;charset=UTF-8".to_string());
        bytes = rc.clone();
        source = BodySource::UrlSearchParams(rc);
    } else if object.is_string() {
        // Scalar value string case (§3.2). USVString conversion: WebIDL
        // mandates that lone surrogates are replaced with U+FFFD (`scalar
        // value string`) before encoding to UTF-8.
        // (v2 fix CRITICAL-11: v1 used to_rust_string_lossy here; that's
        // V8's lossy conversion which is implementation-defined for lone
        // surrogates. WebIDL § 3.2.10 specifies replacement with U+FFFD.)
        let utf8 = usv_string_to_utf8(scope, object.try_into().unwrap());
        let rc = Rc::new(utf8);
        length = Some(rc.len() as u64);
        content_type = Some("text/plain;charset=UTF-8".to_string());
        bytes = rc.clone();
        source = BodySource::Bytes(rc);
    } else if is_async_iterable(scope, object) {
        // Workerd / Deno extension: AsyncIterable<Uint8Array> as a body
        // source. NOT in the WHATWG enum but spec discussion is at
        // https://github.com/whatwg/fetch/pull/1646. We ship for AI-SDK
        // creator workloads (XX.7).
        let stream_obj = readable_stream_from_async_iterable(scope, object)?;
        let stream_global = v8::Global::new(scope, stream_obj);
        return Ok((
            BodyImpl { stream: stream_global, source: BodySource::Stream, length: None },
            None,
        ));
    } else {
        // Per WebIDL union dispatch, no concrete BodyInit member matched.
        // The `BodyInit` union ends with USVString — but reaching this branch
        // means the object isn't even string-coercible to a meaningful body.
        // We throw TypeError rather than silently `toString`-ing (v2 change:
        // v1 fell through to ToString coercion, which silently turned
        // `{}` into `"[object Object]"`). Per spec union resolution rules.
        return Err(OpError::type_error(
            "BodyInit must be a Blob, BufferSource, FormData, URLSearchParams, USVString, or ReadableStream",
        ));
    }

    // STEP 11-12: build a ReadableStream wrapping the bytes via streams-native
    // from_native_source. Per spec the action is "in parallel" — for our
    // single-threaded model this means lazy: the native source emits the
    // entire byte buffer in one pull call when the consumer reads.
    let native_source = BytesNativeSource::new(bytes);
    let stream_obj = ReadableStream::from_native_source(scope, native_source, bytes_strategy());
    let stream_global = v8::Global::new(scope, stream_obj);

    Ok((
        BodyImpl { stream: stream_global, source, length },
        content_type,
    ))
}

/// USVString conversion per WebIDL § 3.2.10:
///   "The result of converting a Unicode string ... is a sequence of code
///    points where each lone surrogate is replaced by U+FFFD."
/// Returns the UTF-8 encoding of the resulting scalar value string.
fn usv_string_to_utf8(scope: &mut v8::PinScope, s: v8::Local<v8::String>) -> Vec<u8> {
    let len = s.length();
    let mut buf: Vec<u16> = vec![0u16; len];
    s.write_v2(scope, 0, &mut buf, v8::WriteFlags::empty());
    let mut out = String::with_capacity(len);
    for c in std::char::decode_utf16(buf.iter().copied()) {
        out.push(c.unwrap_or(std::char::REPLACEMENT_CHARACTER));
    }
    out.into_bytes()
}
```

The `BytesNativeSource` is a tiny `NativeSource` impl that emits the
entire byte buffer in a single `pull` call, then closes the stream.
~30 LOC.

#### III.2.1. `safely extract a body` (§3.2)

The "safely extract a body" wrapper asserts that a passed
ReadableStream is neither disturbed nor locked, then forwards to
`extract_body`. Used by spec callers that have already validated the
input (e.g. redirect retransmission). One-line wrapper.

### III.3. The `body` and `bodyUsed` getters

`body`:

```rust
#[v8_getter]
fn body(&self, scope: &mut v8::PinScope) -> v8::Local<'_, v8::Value> {
    match self.body.borrow().as_ref() {
        Some(impl_) => v8::Local::new(scope, &impl_.stream).into(),
        None => v8::null(scope).into(),
    }
}
```

`bodyUsed`:

```rust
#[v8_getter]
fn body_used(&self, scope: &mut v8::PinScope) -> bool {
    match self.body.borrow().as_ref() {
        Some(impl_) => {
            let stream = v8::Local::new(scope, &impl_.stream);
            // streams-native exposes the [[disturbed]] slot via
            // `is_disturbed(stream)` — see streams.rs.
            crate::streams::is_disturbed(stream)
        }
        None => false,
    }
}
```

The polyfill carried a separate `_bodyUsed` flag. Native delegates to
the stream's [[disturbed]] slot — a single source of truth, removing
the polyfill's `arrayBuffer()` / `text()` etc. each setting their own
flag duplicate.

### III.4. Body consumers — `text()`, `json()`, `arrayBuffer()`, `bytes()`, `blob()`, `formData()`

All six follow the same pattern: drain the stream via a fetch-internal
helper, accumulate bytes, then decode/parse. Spec §3.5 "Body.consume
body"; the algorithm is:

1. If `bodyUsed` is true OR `body` is locked, reject with TypeError.
2. Mark the stream as disturbed (via reading).
3. Read all chunks; accumulate into a byte buffer.
4. Decode/parse per the consumer (UTF-8 decode for text; JSON.parse
   for json; ArrayBuffer construction for arrayBuffer/bytes; Blob
   construction for blob; multipart-or-urlencoded parsing for formData).

<!-- v2 fix (CRITICAL-13): read_all_bytes / read_one_chunk are NOT
streams-native exports. They live in fetch/body_stream.rs as
fetch-internal helpers built on streams' public RS reader API. -->

#### III.4.1. Fetch-internal stream helpers

```rust
// crates/runtime/src/fetch/body_stream.rs
//
// Drain a ReadableStream into a single Vec<u8>. Built on streams-native's
// public default-reader API (from_native_source has shipped in streams-
// native; getReader()/read() are the public surface we consume).
pub async fn read_all_bytes(
    scope: &mut v8::PinScope,
    stream: v8::Local<v8::Object>,
) -> Result<Vec<u8>, v8::Global<v8::Value>> {
    // streams::acquire_default_reader is the same Rust function that
    // implements the JS-visible `getReader()`; we call it via the streams
    // crate's pub fn (no JS hop). It marks [[disturbed]] internally.
    let reader = streams::acquire_default_reader(scope, stream)?;
    let mut out: Vec<u8> = Vec::with_capacity(4096);
    loop {
        // streams::reader_read returns Option<Result<v8::Local<v8::Value>, ...>>
        // (Some(Ok(value)) = a chunk; Some(Err(err)) = stream error;
        //  None = stream closed). It is the Rust-side counterpart of
        // `reader.read()` returning `{value, done}`.
        match streams::reader_read(scope, &reader).await {
            Some(Ok(chunk)) => {
                let bytes = bytes_from_buffer_source(scope, chunk)
                    .map_err(|e| make_type_error_global(scope, &e))?;
                out.extend_from_slice(&bytes);
            }
            Some(Err(err)) => return Err(err),
            None => break,
        }
    }
    streams::release_reader(scope, &reader);
    Ok(out)
}

/// Single-chunk read. Used by streaming-upload (`drive_readable_stream`).
pub async fn read_one_chunk(
    scope: &mut v8::PinScope,
    stream: v8::Local<v8::Object>,
) -> Option<Result<Vec<u8>, v8::Global<v8::Value>>> {
    let reader = match streams::reuse_or_acquire_default_reader(scope, stream) {
        Ok(r) => r,
        Err(e) => return Some(Err(e)),
    };
    match streams::reader_read(scope, &reader).await {
        Some(Ok(chunk)) => match bytes_from_buffer_source(scope, chunk) {
            Ok(bytes) => Some(Ok(bytes)),
            Err(e) => Some(Err(make_type_error_global(scope, &e))),
        },
        Some(Err(e)) => Some(Err(e)),
        None => None,
    }
}
```

These layer on streams-native's `acquire_default_reader` /
`reader_read` (already shipped — RS+DefaultController+DefaultReader is
the part of streams-native that has landed). They are NOT additions to
streams-native's API surface.

#### III.4.2. `arrayBuffer()` — bulk copy via backing-store-from-vec

```rust
#[v8_async_method]
async fn array_buffer(&self, scope: &mut v8::PinScope) -> Result<v8::Global<v8::Value>, v8::Global<v8::Value>> {
    // Synchronous gate: locked / used (must happen in the calling tick).
    let stream_global = match self.body_slot().borrow().as_ref() {
        Some(impl_) => impl_.stream.clone(),
        None => {
            let ab = v8::ArrayBuffer::new(scope, 0);
            return Ok(v8::Global::new(scope, ab.into()));
        }
    };
    let stream_local = v8::Local::new(scope, &stream_global);
    if streams::is_disturbed_obj(scope, stream_local) || streams::is_locked_obj(scope, stream_local) {
        return Err(make_type_error_global(scope, "Body has already been used or is locked."));
    }

    // Drain.
    let bytes: Vec<u8> = read_all_bytes(scope, stream_local).await?;

    // v2 fix MAJOR-25: range-check size before allocating to bound peak memory
    // and to honour spec error type. Spec at https://fetch.spec.whatwg.org/#dom-body-bytes
    // notes >2GB byte sequences should reject with RangeError.
    if bytes.len() > i32::MAX as usize {
        return Err(make_range_error_global(scope, "body too large for ArrayBuffer"));
    }

    // v2 fix MAJOR-26: build the ArrayBuffer via new_backing_store_from_vec
    // (single allocation transfer); avoids the cell-by-cell loop which is
    // ~3-5x slower for >1MB bodies on rusty-v8 measurements.
    let backing = v8::ArrayBuffer::new_backing_store_from_vec(bytes);
    let ab = v8::ArrayBuffer::with_backing_store(scope, &backing.make_shared());
    Ok(v8::Global::new(scope, ab.into()))
}
```

#### III.4.3. Per-consumer error semantics (v2 fix MAJOR-25)

Spec mandates different rejection types per consumer:

| Consumer | Failure mode | Rejection error type | Spec |
|----------|--------------|----------------------|------|
| `text()` | UTF-8 invalid bytes | (infallible — replacement character) | https://fetch.spec.whatwg.org/#dom-body-text — UTF-8 decode is `replacement` not `fatal` |
| `json()` | JSON parse fail | **`SyntaxError`** | https://fetch.spec.whatwg.org/#dom-body-json — "if value is failure, throw a SyntaxError" |
| `arrayBuffer()` | OOM / >i32::MAX | **`RangeError`** | https://fetch.spec.whatwg.org/#dom-body-arraybuffer |
| `bytes()` | OOM / >i32::MAX | **`RangeError`** | https://fetch.spec.whatwg.org/#dom-body-bytes |
| `blob()` | (infallible after read) | n/a | n/a |
| `formData()` | malformed body | **`TypeError`** | https://fetch.spec.whatwg.org/#dom-body-formdata |

All other failure modes (body is locked / disturbed) → `TypeError`.
The consumer dispatch helper in `fetch/body.rs::consume_body` takes
the `ConsumerKind` enum and uses the table to mint the right error
type when the codec/parser fails.

```rust
match consumer_kind {
    ConsumerKind::Text => {
        // v2 fix MAJOR-25: text() is infallible. Use replacement-character
        // UTF-8 decode (TextDecoder default). NEVER throws.
        let s = text_encoding::utf8_decode_replacement(&bytes);
        Ok(v8::String::new(scope, &s).unwrap().into())
    }
    ConsumerKind::Json => {
        // v2 fix MAJOR-25: SyntaxError on parse failure (spec says SyntaxError,
        // not TypeError). v8::JSON::parse returns None on parse failure.
        let s = text_encoding::utf8_decode_replacement(&bytes);
        let s_v8 = v8::String::new(scope, &s).unwrap();
        match v8::JSON::parse(scope, s_v8) {
            Some(v) => Ok(v),
            None => Err(make_syntax_error_global(scope, "could not parse body as JSON")),
        }
    }
    ConsumerKind::Bytes => {
        if bytes.len() > i32::MAX as usize {
            return Err(make_range_error_global(scope, "body too large"));
        }
        // ArrayBuffer + Uint8Array view (Uint8Array shares backing store).
        let backing = v8::ArrayBuffer::new_backing_store_from_vec(bytes);
        let ab = v8::ArrayBuffer::with_backing_store(scope, &backing.make_shared());
        let len = ab.byte_length();
        Ok(v8::Uint8Array::new(scope, ab, 0, len).unwrap().into())
    }
    // ... arrayBuffer (above), blob (Blob construction), formData (parser)
}
```

For `text()`: never throws — UTF-8 decode with replacement (matches
spec's "UTF-8 decode" which is non-fatal).
*(Aside: `Content-Type;charset=...` is NOT honoured — spec hardcodes
UTF-8 for `text()`. We document this in §VII Missing Concept #1.)*

For `json()`: decode UTF-8 (replacement); call `v8::JSON::parse`; on
parse failure, reject with **SyntaxError** — name `"SyntaxError"`,
not the generic `TypeError`. WPT `response/response-static-json.any.js`
checks the name property.

For `bytes()` / `arrayBuffer()`: range-check size; allocate backing
store via `new_backing_store_from_vec` (transfers ownership of the
Vec — single allocation, no cell-by-cell copy).

For `blob()`: build a Blob from the ArrayBuffer + the body's
Content-Type header.

For `formData()`:
- `application/x-www-form-urlencoded` → URLSearchParams-style parse,
  populate a FormData.
- `multipart/form-data; boundary=...` → multipart parser. **Deferred
  in v1** — throws TypeError("Multipart formData parsing not
  implemented"). Same scope as the polyfill (which only handles
  urlencoded).

### III.5. `clone()` semantics (§5.4 / §5.5 / §6.3)

Spec §6.3 "clone a body":

> 1. Let « out1, out2 » be the result of teeing body's stream.
> 2. Set body's stream to out1.
> 3. Return a body whose stream is out2 and other members are copied
>    from body.

Implementation:

```rust
#[v8_method]
fn clone(&mut self, scope: &mut v8::PinScope) -> Result<v8::Local<'_, v8::Value>, OpError> {
    // Spec §5.4 step 1: throw if body is unusable (disturbed or locked).
    if let Some(impl_) = self.body.borrow().as_ref() {
        let stream_local = v8::Local::new(scope, &impl_.stream);
        if crate::streams::is_disturbed(stream_local) || crate::streams::is_locked(stream_local) {
            return Err(OpError::type_error("Cannot clone a body that has been used or is locked."));
        }
    }

    // Mint a new Request/Response wrapper.
    let new_obj = build_request_clone(scope, self);

    // Tee the body stream if present.
    if let Some(impl_) = self.body.borrow().as_ref() {
        let stream_local = v8::Local::new(scope, &impl_.stream);
        let [branch_a, branch_b] = crate::streams::tee(scope, stream_local);
        // Replace self's stream with branch_a (mutate the BodyImpl in place).
        self.body.borrow_mut().as_mut().unwrap().stream = v8::Global::new(scope, branch_a);
        // Set the clone's stream to branch_b.
        let cloned_state = get_state_mut::<RequestState>(scope, new_obj);
        cloned_state.body.borrow_mut().as_mut().unwrap().stream = v8::Global::new(scope, branch_b);
        // Source is shared via Rc — both clones see the same source bytes.
        // Length copied verbatim.
    }

    Ok(new_obj.into())
}
```

The `crate::streams::tee` helper is the public streams-native tee
algorithm (D-11 of streams). The teed branches are independent
ReadableStreams; cloning twice produces three independent consumers,
etc. The source `Rc<Vec<u8>>` is shared between clones and original
— cheap.

### III.6. Special construction paths

**`Response.json(data, init)`** (§5.5 "Response.json"):

```rust
#[v8_method(static_method)]
fn json(scope: &mut v8::PinScope, data: v8::Local<v8::Value>, init: v8::Local<v8::Value>) -> Result<v8::Local<'_, v8::Value>, OpError> {
    // Step 1: Let bytes be the result of running serialize a JavaScript value to JSON bytes on data.
    let json_str = v8::JSON::stringify(scope, data)
        .ok_or_else(|| OpError::type_error("could not serialize value to JSON"))?
        .to_rust_string_lossy(scope);
    let bytes = json_str.into_bytes();
    // Step 2-3: Build body via extract_body on the bytes.
    let (body_impl, _) = extract_body_from_bytes(scope, &bytes)?;
    // Step 4: Parse init dictionary.
    let init_dict = parse_response_init(scope, init)?;
    // Step 5: Build response with body, init.status / init.statusText / init.headers.
    let response = build_response(scope, Some(body_impl), init_dict)?;
    // Step 6: If response's headers does not contain "content-type",
    //         append "content-type"/"application/json" to response's headers.
    let headers = response_headers(scope, response);
    if !headers_has(scope, headers, b"content-type") {
        headers_append(scope, headers, b"content-type", b"application/json");
    }
    Ok(response.into())
}
```

This replaces the polyfill's optimized `Response.json` fast-path
(`embed/fetch.js:392-432`). The fast-path used `Object.create(Response.prototype)`
to skip the constructor; native doesn't need the optimization because
the constructor itself is direct V8 + Rust without a JS hop.

**`Response.error()`** (§5.5):

Builds a Response with `type: "error"`, `status: 0`, empty
headers, null body. The `internal_response` slot is None.

**`Response.redirect(url, status)`** (§5.5):

Validates status ∈ {301, 302, 303, 307, 308} (RangeError otherwise);
parses url; builds a Response with `status`, `Location` header set
to the parsed URL's serialization, null body.

**`Response.error()`** must NOT be confused with a network-error
response in `mainFetch` — they share the same shape but the latter
is constructed internally by the algorithm (via `make_network_error()`
in `fetch/algorithms.rs`).

**WebSocket-upgrade extension (D-13):**

When the gateway dispatch path returns a Response with status 101
and a `webSocket` init member, native preserves both. The Response's
priv-sym `webSocket` field stores a `v8::Global<v8::Object>` pointing
at a `WebSocket` wrapper from `crates/runtime/src/websocket.rs`. The
gateway's `inspect_response` code (currently `crates/runtime/src/transport/handler.rs:181-194`)
reads this priv sym to detect upgrade. After landing 2 of D-23 (when
JSON dispatch is removed), the inspect routine reads ResponseState
directly via Box<ResponseState> from the wrapper's internal field 0
— faster, fewer property lookups.

## IV. Request constructor (§5.4) — step-by-step

This algorithm is the most intricate in the spec for fetch (~40
numbered sub-steps with dictionary-driven branches). The Rust
implementation lives in `fetch/request.rs::Request::constructor` and
mirrors the spec exactly.

### IV.1. Steps 1-12: copy from input Request OR parse URL

```
1. Let request be null.
2. Let fallbackMode be null.
3. Let baseURL be this's relevant settings object's API base URL.
4. Let signal be null.
5. If input is a string, then:
   1. Let parsedURL be the result of parsing input with baseURL.
   2. If parsedURL is failure, throw TypeError.
   3. If parsedURL includes credentials, throw TypeError.
   4. Set request to a new request whose URL is parsedURL.
   5. Set fallbackMode to "cors".
6. Otherwise:
   7. Assert: input is a Request object.
   8. Set request to input's request.
   9. Set signal to input's signal.
10-12. (origin / window handling — D-3 makes these no-ops.)
```

Implementation:

```rust
fn constructor(
    scope: &mut v8::PinScope,
    input: v8::Local<v8::Value>,
    init: v8::Local<v8::Value>,
) -> Result<RequestState, OpError> {
    let init_dict = parse_request_init(scope, init)?;

    let mut state: RequestState;
    let mut signal_in: Option<v8::Global<v8::Object>>;
    let mut fallback_mode: Option<RequestMode> = None;

    if let Some(other) = try_extract_request(scope, input) {
        // Step 6: input is Request.
        state = clone_request_state_no_body(&other);  // shallow copy
        signal_in = Some(get_signal(scope, &other));
    } else {
        // Step 5: input is string-or-URL.
        let url_str = input.to_rust_string_lossy(scope);
        let parsed = url::Url::parse(&url_str)
            .map_err(|e| OpError::type_error(&format!("Failed to parse URL: {e}")))?;
        if !parsed.username().is_empty() || parsed.password().is_some() {
            return Err(OpError::type_error(
                "Request cannot be constructed from a URL that includes credentials",
            ));
        }
        state = RequestState::default();
        state.url_list.borrow_mut().push(parsed);
        signal_in = None;
        fallback_mode = Some(RequestMode::Cors);
    }
    // ... continue to step 13
}
```

### IV.2. Steps 13-14: init non-empty branch

```
13. If init is not empty:
   1. If request's mode is "navigate", set it to "same-origin".
   2-7. Reset reload-navigation, history-navigation, origin, referrer,
        referrerPolicy, urlList — back to defaults.
14. If init["referrer"] exists:
   ... (parse referrer URL, validate scheme)
```

Per D-3 most of step 14 is a no-op (referrer ignored) except for the
TypeError on parse failure (preserved for spec conformance).

### IV.3. Steps 15-22: copy init enum members

For each of: referrerPolicy, mode, credentials, cache, redirect,
integrity, keepalive, priority — if init has the key, set the
corresponding RequestState field. The enum string→variant conversions
are emitted by the macro extension XIV.4.

### IV.4. Step 25: method validation

```
25. If init["method"] exists:
   1. Let method be init["method"].
   2. If method is not a method or is a forbidden method, throw TypeError.
   3. Normalize method.
   4. Set request's method to method.
```

D-18 spells this out:

```rust
fn validate_and_normalize_method(m: &[u8]) -> Result<Vec<u8>, OpError> {
    // Per Fetch §2.2.4 method definition: method is a byte sequence
    // that matches the token production. (Same tchar set as a header
    // name — see headers-native is_tchar.)
    if m.is_empty() || !m.iter().all(|b| crate::headers::is_tchar(*b)) {
        return Err(OpError::type_error("method is not a valid token"));
    }
    let upper: Vec<u8> = m.iter().map(|b| b.to_ascii_uppercase()).collect();
    match upper.as_slice() {
        b"CONNECT" | b"TRACE" | b"TRACK" => {
            Err(OpError::type_error(&format!(
                "method '{}' is forbidden",
                std::str::from_utf8(m).unwrap_or("?")
            )))
        }
        b"DELETE" | b"GET" | b"HEAD" | b"OPTIONS" | b"POST" | b"PUT" => Ok(upper),
        _ => Ok(m.to_vec()),  // case-preserving for non-standard methods
    }
}
```

### IV.5. Steps 26-31: signal

```
26. Let signal be null.
27. If init["signal"] exists, set signal to init["signal"].
28. If signal is not null, set this's signal's signal to signal.
   ...
30. If signal is null, set this's signal's signal to null.
31. Otherwise, ... ("follow" relationship between this signal and the input signal)
```

Spec §5.4 step 31's "follow" relationship is a forward-ref into
DOM AbortSignal. Implementation: install an abort listener on the
input signal that fires on this Request's signal. ~50 LOC in
`fetch/abort_signal.rs::follow_signal`.

### IV.6. Steps 32-34: headers

```
32. Let headers be a copy of this's headers.
33. If init["headers"] exists, set headers to init["headers"].
34. (Spec sub-steps:)
    Empty this's headers' header list.
    If headers is a Headers object, then for each header of its header list,
      append (header's name, header's value) to this's headers.
    Otherwise, fill this's headers with headers.
```

Headers are filled via headers-native's existing constructor. The
guard machinery (D-17) is applied: this's headers' guard is set to
"request" by default, "request-no-cors" if `request.mode === "no-cors"`.
The forbidden-header check fires inside `Headers.append` / `set` when
the guard is `"request"`.

### IV.7. Steps 35-39: body

```
35. Let inputBody be input's request's body if input is a Request object;
    otherwise null.
36. If init["body"] exists and is non-null, OR inputBody is non-null,
    AND request's method is `GET` or `HEAD`, throw a TypeError.
37. Let initBody be null.
38. If init["body"] exists and is non-null:
   1. Let Content-Type be null.
   2. Set initBody and Content-Type to the result of extracting init["body"].
   3. If Content-Type is non-null and this's headers' header list does not
      contain `Content-Type`, append `Content-Type`/Content-Type to this's
      headers.
39. Let inputOrInitBody be initBody if it is non-null; otherwise inputBody.
40. If inputOrInitBody is non-null and inputOrInitBody's source is null:
   1. If initBody is non-null and init["duplex"] does not exist, throw a TypeError.
   2. If this's request's mode is neither "same-origin" nor "cors", throw a TypeError.
   3. Set this's request's use-CORS-preflight flag.
41. Let finalBody be inputOrInitBody.
42. If initBody is null and inputBody is non-null:
   1. If input is unusable (disturbed/locked), throw a TypeError.
   2. Set finalBody to the result of cloning inputBody.
43. Set this's request's body to finalBody.
```

The interesting step is 40: stream bodies in cors mode require `duplex: "half"`.
The polyfill never enforced this; native does (D-29).

### IV.8. The `[SameObject]` headers + signal accessors

`Request.headers` is `[SameObject]`. The wrapper holds a private symbol
pointing to the Headers JS wrapper; first access mints the wrapper
and stores it. Subsequent accesses return the same V8 object —
`Object.is(req.headers, req.headers) === true`.

Same for `Request.signal`.

```rust
#[v8_getter]
fn headers(&self, scope: &mut v8::PinScope) -> v8::Local<'_, v8::Object> {
    // Read priv-sym; if unset, mint a new Headers wrapper bound to this
    // request's header list and store it.
    let self_obj = self.self_weak.upgrade(scope).unwrap();
    if let Some(existing) = read_priv_sym(scope, self_obj, &K_HEADERS) {
        return existing.try_into().unwrap();
    }
    let headers_obj = Headers::wrap_for_request(scope, self_obj);
    write_priv_sym(scope, self_obj, &K_HEADERS, headers_obj.into());
    headers_obj
}
```

`Headers::wrap_for_request` is a new headers-native API that produces
a Headers wrapper sharing the request's HeaderList via Rc<RefCell>.
Headers-native's storage rule (`Vec<(Vec<u8>, Vec<u8>)>` owned by the
Headers struct) generalises to a shared `Rc<RefCell<HeaderList>>`
when this is needed; the move was anticipated in headers-native.md
"Forward compatibility" §.

### IV.9. Constructor integrity (`integrity` member)

Stored on the RequestState, never enforced (per Non-goals). The IDL
field round-trips (matches WPT `request-init-002` integrity-field
test).

## V. The fetch algorithm — exhaustive

The fetch dispatch is mutually recursive. Here we list every spec
algorithm with section, Rust function, and a sketch of the steps that
matter. The full step-by-step is in `fetch/algorithms.rs`; this design
gives the high-level shape so reviewers can confirm the structure
matches the spec.

### V.1. `fetch(input, init)` — §5.1 "fetch method"

Rust: `fetch::fetch_callback`.

Steps (per spec):
1. Let p be a new promise (`v8::PromiseResolver::new(scope)`).
2. Let requestObject = new Request(input, init); on throw, reject p with the error.
3. Let request = requestObject's request.
4. If signal.aborted, abort the fetch and return p.
5. Let globalObject = ... (D-3: no-op).
6. (Service-worker check — D-3 no-op.)
7. Let responseObject = null.
9. Let locallyAborted = false.
10. Let controller = null.
11. Add abort listener to signal (calls fetch-controller abort + abortFetch).
13. Set controller = result of calling `fetching` with processResponse callback.
14. Return p.promise.

The `processResponse` callback resolves `p` with a new Response wrapping
the inner-response data, OR rejects with TypeError("fetch failed") on
network error.

### V.2. `fetching(fetchParams)` — §5.2 "fetching"

Rust: `fetch::algorithms::fetching`.

Initialises fetch params (timing info, controller), applies the
default `Accept` and `Accept-Language` and `Accept-Encoding` (D-15)
headers, and calls `mainFetch(fetchParams, false)`. Returns the
fetch controller (so the caller can abort).

### V.3. `mainFetch(fetchParams, recursive)` — §5.3 "main fetch"

Rust: `fetch::algorithms::main_fetch`.

The dispatch function. Most of its 21 steps are no-ops or D-3
bypasses; the load-bearing branches are step 11 (URL scheme dispatch,
calls `schemeFetch`), step 13 (response tainting — D-3 always
"basic"), and step 19 (null-body status / HEAD / CONNECT body
nullification — D-13).

### V.4. `schemeFetch(fetchParams)` — §5.4 "scheme fetch"

Rust: `fetch::algorithms::scheme_fetch`.

Switches on `request.currentURL.scheme`:

| Scheme | Action |
|--------|--------|
| `data:` | Run `data:` URL processor (D-21); return synthetic Response. |
| `http:` / `https:` | Return `httpFetch(fetchParams)`. |
| `about:` | Return network error ("about scheme is not supported"). |
| `blob:` | Return network error (D-1 non-goals). |
| `file:` | Return network error (D-1 non-goals; SSRF). |
| Any other | Return network error ("unknown scheme"). |

### V.5. `httpFetch(fetchParams)` — §5.5 "HTTP fetch"

Rust: `fetch::algorithms::http_fetch`.

Steps 1-10 of the spec. The CORS-preflight (step 6.1) and CORS-check
(step 6.4) are D-3 no-ops. The redirect handling (step 8) is the
real work:

```
8. If actualResponse's status is a redirect status:
   1. (HTTP/2 RST_STREAM concern — N/A in HTTP/1.1.)
   2. Switch on request's redirect mode:
      - "error": set response to network error.
      - "manual": set response to opaque-redirect filtered response. (Note:
        D-3 simplifies this to returning the actual response — workerd does
        the same per `Response/Redirect` enum in http.h:644-648.)
      - "follow": set response = httpRedirectFetch(fetchParams, response).
```

### V.6. `httpRedirectFetch(fetchParams, response)` — §5.6 "HTTP-redirect fetch"

Rust: `fetch::algorithms::http_redirect_fetch`.

The 20-step redirect algorithm. Key load-bearing steps:

1. Let actualResponse = response (or response's internal response if filtered).
2. Let locationURL = actualResponse's "location URL" given fragment.
3. If locationURL is null, return response.
4. If failure, return network error.
5. If locationURL.scheme is not http/https, return network error.
6. If request.redirect-count == 20, return network error.
7. Increase redirect-count.
8-11. Cross-origin / credentials checks (D-3 no-ops).
12. If actualResponse.status is not 303, request.body is non-null,
    request.body.source is null, return network error.
   ↑ This is the D-11 rewindability rule. A stream body cannot follow
     a 307/308 redirect.
13. If status is (301 or 302 with method POST) or (303 with method
    not GET/HEAD):
    1. Set request.method to GET.
    2. Set request.body to null.
    3. For each header in request-body-header set, delete from request.headers.
14. If current URL's origin is not same-origin with locationURL's origin,
    delete `Authorization`, `Proxy-Authorization`, `Cookie`, `Host`
    from request.headers.
   ↑ D-12: this IS implemented (workerd does it conditionally on a
     compat flag — http.c++:1857-1882).
15. If request.body is non-null, set request.body to safelyExtractBody(request.body.source).
   ↑ The rewind step.
18. Append locationURL to request.urlList.
20. Return mainFetch(fetchParams, true).

### V.7. `httpNetworkOrCacheFetch(fetchParams, isAuthenticationFetch, isNewConnectionFetch)` — §5.7 "HTTP-network-or-cache fetch"

Rust: `fetch::algorithms::http_network_or_cache_fetch`.

The big one — ~50 numbered steps. <!-- v2 fix CRITICAL-13: step number cleanup. The spec wraps most "header rewrite" work inside step 8 (an "abort when" wrapper); the design's high-level summary references the spec's nested-step numbers (e.g. 8.13 = no-store, 8.14 = reload, 8.15 = no-cache, 8.16 = force-cache, 8.17 = only-if-cached). v1 said "steps 16-18" which conflated the nested numbering. Quoting current spec head: -->

| Spec sub-step | Action |
|---------------|--------|
| 8.4 — Content-Length | Compute from body length (or skip for streamed bodies — chunked-encoding instead, §V.8) |
| 8.6 — User-Agent | Set to runtime UA if absent |
| 8.10 — Origin (`append a request \`Origin\` header`, §3.2.6) | Per D-16 (v2 fix MAJOR-22/23): appended when method ∉ {GET, HEAD} OR mode is `cors` OR mode is `websocket`/`webtransport`; value depends on referrer-policy (`"no-referrer"` ⇒ literal `"null"`) |
| 8.13 — `cache: "no-store"` | Set `Cache-Control: no-store, no-cache` if absent |
| 8.14 — `cache: "reload"` | Set `Cache-Control: no-cache, no-store, max-age=0` AND `Pragma: no-cache` |
| 8.15 — `cache: "no-cache"` | Set `Cache-Control: max-age=0` if no other cache directive |
| 8.16 — `cache: "force-cache"` / 8.17 — `"only-if-cached"` | Set `Cache-Control: only-if-cached` (force-cache rewrites only when no cache directive); `only-if-cached` returns network-error in v1 (no cache layer) |
| 8.18 — Range | If Range header is set, set `[[range-requested]]` flag; required for the body-info bookkeeping |
| 8.19 — Accept-Encoding | D-15: `br, gzip, deflate` (HTTPS) / `gzip, deflate` (HTTP) when none provided. Empty string opts out. Single canonical ordering — v2 fix MAJOR-31. |
| 8.21 — Authorization | Read [[use-URL-credentials flag]]; if set, attach Basic-Authorization derived from the URL's userinfo |
| 10 — `httpNetworkFetch` | Invoked here |

Following step 8, the post-fetch handlers (10.2 in spec):
- 401 retry (auth challenge) — bumped past v1 (D-3 simplifies
  credentials).
- 421 retry — connection-misdirected — handled (re-issue on a fresh
  connection).
- 407 passthrough — returns to user (proxy auth challenge).

For v1, with no cache layer:

- step 9 (cache partition): N/A.
- step 9.1 (httpCache is null → cache mode = "no-store"): forces every
  fetch to skip cache.
- step 9.7 (cache lookup): skipped.
- response = httpNetworkFetch(...) directly.

#### V.7.1. Origin header value derivation (D-16, v2 fix MAJOR-22/23)

```rust
fn append_origin_header(
    request: &Request,
    headers: &mut HeaderList,
) {
    let method_excluded = matches!(
        request.method.as_slice(),
        b"GET" | b"HEAD",
    );
    let mode_includes = matches!(
        request.mode.get(),
        RequestMode::Cors
        // Note: WebSocket/WebTransport modes are spec-internal, set by
        // those constructors — not reachable from user `fetch()` code.
        // We still implement the spec rule for completeness when the
        // gateway WebSocket-upgrade path mints a fetch.
    );
    if method_excluded && !mode_includes {
        return;
    }
    let value: Vec<u8> = match request.referrer_policy.get() {
        ReferrerPolicy::NoReferrer => b"null".to_vec(),
        _ => request.url_list.borrow().last().unwrap().origin().ascii_serialization().into_bytes(),
    };
    headers.set(b"Origin", &value);
}
```

#### V.7.2. `request-body-header` set (v2 fix CRITICAL-2)

Spec at https://fetch.spec.whatwg.org/#request-body-header-name lists
exactly **4 header names**:

```
- Content-Encoding
- Content-Language
- Content-Location
- Content-Type
```

Note: `Content-Length` is **NOT** in this set per the spec. (undici
includes it as a deviation; we do not.) Used by §VIII.2 redirect
method/body rewrite.

### V.8. `httpNetworkFetch(fetchParams, includeCredentials, forceNewConnection)` — §5.8 "HTTP-network fetch"

Rust: `fetch::algorithms::http_network_fetch`.

The connection layer. Maps spec steps to cyper:

| Spec step | Implementation |
|-----------|----------------|
| Obtain connection | `cyper::Client.request(method, url)` (lazily reuses pool) |
| Transfer-Encoding: chunked decision | If body source is Stream and connection is HTTP/1.1, set `Transfer-Encoding: chunked` (step 9.3). cyper handles the chunked framing. |
| Make HTTP request | `request_builder.send().await` |
| 100-199 ignore (except 101) | cyper does this internally; we receive the final status. |
| Wait until headers are transmitted | implicit in `.send().await` resolving with response. |
| Body streaming | `response.bytes_stream()` from cyper (existing pattern in `crates/runtime/src/transport/ssrf.rs:548`). |

<!-- v2 fix (CRITICAL-3): hyper 1.x API. hyper::Body and Body::from /
Body::wrap_stream do not exist in hyper 1.x — they were removed when
the Body trait moved to http_body_util. We use:
  - http_body_util::Full<Bytes> for buffered bodies
  - http_body_util::StreamBody<S>  for streaming bodies
  - cyper::Body wrapper which boxes any Body trait impl into the
    cyper RequestBuilder API.
-->

Pseudo-code:

```rust
use bytes::Bytes;
use http_body_util::{Full, StreamBody, BodyExt, combinators::BoxBody};
use http_body::Frame;

async fn http_network_fetch(
    fetch_params: &mut FetchParams,
    include_credentials: bool,
    force_new_connection: bool,
) -> InnerResponse {
    let request = &fetch_params.request;
    let url = request.url_list.borrow().last().unwrap().clone();

    // Bad-port check (D-19): 83 ports from spec, port 0 included.
    if let Some(port) = url.port_or_known_default() {
        if BAD_PORTS.contains(&port) {
            return make_network_error("bad port");
        }
    }

    // SSRF check (D-24, existing validate_url).
    if let Err(msg) = validate_url(url.as_str()) {
        return make_network_error(&msg);
    }

    let client = shared_client();  // existing thread_local cyper Client
    let method = http::Method::from_bytes(&request.method)
        .map_err(|e| make_network_error(&format!("invalid method: {e}")))?;
    let mut builder = client.request(method, url.as_str())
        .map_err(|e| make_network_error(&e.to_string()))?;

    // Copy headers (header list is already finalised — Origin appended,
    // Accept-Encoding appended, Cache-Control rewrites applied).
    let header_list = request.headers_list.borrow();
    for (name, value) in header_list.iter() {
        builder = builder
            .header(name.as_slice(), value.as_slice())
            .map_err(|e| make_network_error(&format!("invalid header: {e}")))?;
    }

    // Body: bytes vs stream. Build a cyper::Body wrapper.
    match request.body.borrow().as_ref() {
        None => { /* no body — leave builder as-is */ }
        Some(impl_) => match &impl_.source {
            BodySource::Bytes(rc) | BodySource::Blob(rc, _)
            | BodySource::UrlSearchParams(rc) | BodySource::FormData(rc, _) => {
                // Buffered body: clone the Rc bytes into a Bytes (refcount
                // copy on the underlying buffer) and wrap in Full<Bytes>.
                let bytes = Bytes::from(rc.as_slice().to_vec());
                let body: BoxBody<Bytes, std::io::Error> = Full::new(bytes)
                    .map_err(|never| match never {})
                    .boxed();
                builder = builder.body(cyper::Body::from(body));
            }
            BodySource::Stream => {
                // Streaming upload: drive the JS ReadableStream into a
                // futures::Stream<Result<Frame<Bytes>, _>> and wrap with
                // http_body_util::StreamBody. (hyper 1.x has no
                // Body::wrap_stream — that was hyper 0.14.)
                let stream_global = impl_.stream.clone();
                let frames = drive_readable_stream(state.clone(), stream_global)
                    .map(|chunk| chunk.map(|bytes| Frame::data(Bytes::from(bytes))));
                let body: BoxBody<Bytes, std::io::Error> = StreamBody::new(frames).boxed();
                builder = builder.body(cyper::Body::from(body));
            }
        },
    }

    // Send + receive headers.
    let response = builder.send().await
        .map_err(|e| make_network_error(&e.to_string()))?;

    // Build the inner response (headers + body source).
    let status = response.status().as_u16();
    let mut header_list: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    for (name, value) in response.headers().iter() {
        header_list.push((name.as_str().as_bytes().to_vec(), value.as_bytes().to_vec()));
    }

    // Extract Content-Encoding for the codec hook (compression-streams D-7, D-8).
    let content_encoding = response.headers()
        .get(http::header::CONTENT_ENCODING)
        .and_then(|v| std::str::from_utf8(v.as_bytes()).ok())
        .map(|s| s.to_string());

    // Build a NativeSource that pulls from the cyper body stream.
    let body_source = NetworkBodySource::new(
        response.bytes_stream(),
        fetch_params.controller.clone(),  // for abort listening
    );

    // Response struct.
    InnerResponse {
        status,
        status_text: response.status().canonical_reason().unwrap_or("").to_string(),
        header_list,
        body_source: Some(Box::new(body_source) as Box<dyn NativeSource>),
        content_encoding,
        url_list: vec![url],
        ..Default::default()
    }
}
```

`drive_readable_stream` is a thin adapter that converts a JS
ReadableStream into a `futures::Stream<Item = Result<Bytes, Error>>`
by acquiring a default reader and calling `read()` repeatedly. The
adapter runs on the same compio thread as the cyper send, so no
cross-thread sync. ~80 LOC.

`NetworkBodySource` is a `NativeSource` impl (per streams-native §VIII
NativeSource trait) that pulls from the cyper body stream and emits
chunks. It also handles cancellation (via `fetch_params.controller`)
by closing the stream upstream.

### V.9. Wiring decompression in (D-5, compression-streams §A)

<!-- v2 fix (CRITICAL-12): re-introduce with_response_body_hook as the
named, registered API per compression-streams-native §"From the
native-fetch project (sibling proposal, must land first)". Fetch
implements the hook surface; compression-streams registers itself
as the default hook implementation; the result is a clean,
spec-faithful integration that compression's design depends on. -->

#### V.9.0. The `ResponseBuilder::with_response_body_hook` API surface

Per compression-streams-native.md §"From the native-fetch project"
(lines 173-205), fetch must expose:

```rust
// crates/runtime/src/fetch/response.rs
pub type ResponseBodyHook = Box<dyn Fn(
    &mut HookCtx,
    BodySourcePipe,
) -> Result<BodySourcePipe, JsError>>;

pub struct HookCtx {
    pub status: u16,
    pub url: url::Url,
    pub header_list: HeaderList,    // mutable: hook may strip Content-Encoding/Length
    pub scope: &'a mut v8::PinScope,
}

pub struct BodySourcePipe(pub Box<dyn NativeSource>);

impl ResponseBuilder {
    /// Register a hook that runs between header-receipt and the user-
    /// visible Response object. The hook may wrap the body source in
    /// a transformer chain (e.g. decompression), mutate the header
    /// list (strip Content-Encoding/Length), or reject with a network
    /// error (e.g. unknown coding).
    ///
    /// At most one hook is registered globally. Registration replaces
    /// any prior hook — so compression-streams-native's installation
    /// is the source of truth.
    pub fn with_response_body_hook(hook: ResponseBodyHook);
}
```

The hook is registered ONCE per realm (during runtime init, before
any user JS runs). compression-streams-native.md owns the
implementation: it builds the codec chain, returns the wrapped pipe,
and strips headers. Fetch owns the registration surface only.

`finalize_response_body` invokes the hook (if registered) at the
right moment in the pipeline:

```rust
fn finalize_response_body(
    scope: &mut v8::PinScope,
    inner: &mut InnerResponse,
) -> Result<(), JsError> {
    let body_source = inner.body_source.take().expect("network response has body");

    // v2 fix CRITICAL-12: invoke the registered hook (compression-streams
    // installs itself as the canonical hook). The hook may decompress, mutate
    // the header list, or reject with a network error.
    let pipe = BodySourcePipe(body_source);

    let registered_hook = ResponseBodyHook::get_registered();
    let final_pipe = if let Some(hook) = registered_hook {
        let mut hook_ctx = HookCtx {
            status: inner.status,
            url: inner.url_list.last().cloned().unwrap_or_else(|| url::Url::parse("about:blank").unwrap()),
            header_list: inner.header_list.clone(),
            scope,
        };
        let new_pipe = hook(&mut hook_ctx, pipe)?;  // returns Err for unknown coding
        // Hook may have stripped Content-Encoding and Content-Length;
        // accept the mutated header list verbatim.
        inner.header_list = hook_ctx.header_list;
        new_pipe
    } else {
        pipe
    };

    // Wrap in a JS-visible ReadableStream. Note: the codec chain that the
    // hook installed pulls lazily, so codec init failure mid-stream is a
    // stream error (not a fetch error). Spec correctness: response is
    // exposed via the resolved Promise once headers are in; body stream
    // errors out separately if decompression fails on a chunk.
    let stream_obj = ReadableStream::from_native_source(scope, final_pipe.0, byte_strategy());
    let stream_global = v8::Global::new(scope, stream_obj);
    inner.body_stream = Some(stream_global);
    Ok(())
}
```

The `ResponseBodyHook` is a static slot per realm (registered at
runtime init by compression-streams' init code; see
`compression-streams-native.md` §VI). If no hook is registered (e.g.
on a stripped-down test runtime), bodies pass through unchanged.

This honours **all** of compression-streams' "Native fetch also
commits to" points (1-7 in the dependencies §):

- **Point 1** (response-body construction hook): the
  `with_response_body_hook(hook)` registration API IS implemented;
  `finalize_response_body` invokes the registered hook.
- **Point 2** (Content-Encoding parsing → codec chain): the hook
  (compression-streams) handles it via `build_codec_chain`.
- **Point 3** (Strip Content-Encoding and Content-Length): the hook
  mutates `header_list` directly via `HookCtx`.
- **Point 4** (Unknown coding → network error): the hook returns
  `Err(JsError::type_error(...))`; `finalize_response_body`
  propagates as a fetch network error.
- **Point 5** (no premature normalisation of Content-Encoding): the
  byte-faithful header list is kept; the hook does the parsing.
- **Point 6** (Accept-Encoding default): D-15 (fetch-side
  responsibility).
- **Point 7** (byte-faithful Content-Encoding header): D-15.

### V.10. Network errors

Per spec, a "network error" is a Response with type=error, status=0,
empty headers, null body. The `mainFetch` callers convert to
`p.reject(new TypeError("fetch failed", { cause }))`. Native:

```rust
fn make_network_error(reason: &str) -> InnerResponse {
    InnerResponse {
        status: 0,
        status_text: String::new(),
        header_list: vec![],
        body_source: None,
        body_stream: None,
        content_encoding: None,
        url_list: vec![],
        r#type: ResponseType::Error,
        error_message: Some(reason.to_string()),
        ..Default::default()
    }
}
```

When `processResponse` runs (per `fetch_callback`):

```rust
if response.r#type == ResponseType::Error {
    let err_local = make_type_error_with_cause(scope,
        "fetch failed",
        response.error_message.as_deref());
    resolver.reject(scope, err_local);
} else {
    // build Response wrapper, resolve.
}
```

## VI. The fetch executor — wiring with compio + cyper

The runtime already drives async ops via `state.spawned_fetches` and
`state.spawned_ops`. The native fetch path keeps the same plumbing.

### VI.1. The `fetch` callback

```rust
pub fn fetch_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let state: SharedState = scope.get_slot::<SharedState>().unwrap().clone();

    // Step 1: build the Request synchronously (so init validation
    // throws synchronously per spec).
    let request_obj = match build_request_from_fetch_args(scope, &args) {
        Ok(req) => req,
        Err(e) => {
            // Rejected promise.
            let resolver = v8::PromiseResolver::new(scope).unwrap();
            let err = err_to_v8(scope, e);
            resolver.reject(scope, err);
            rv.set(resolver.get_promise(scope).into());
            return;
        }
    };

    // Step 2: signal aborted check.
    let signal_obj = request_signal(scope, request_obj);
    if abort_signal_aborted(scope, signal_obj) {
        let resolver = v8::PromiseResolver::new(scope).unwrap();
        let reason = abort_signal_reason(scope, signal_obj);
        resolver.reject(scope, reason);
        rv.set(resolver.get_promise(scope).into());
        return;
    }

    // Admission control (D-25; existing).
    if let Err(msg) = check_fetch_admission(&state) {
        let resolver = v8::PromiseResolver::new(scope).unwrap();
        let err_str = v8::String::new(scope, &msg).unwrap();
        let err = v8::Exception::range_error(scope, err_str);
        resolver.reject(scope, err);
        rv.set(resolver.get_promise(scope).into());
        return;
    }

    // Build the Promise we'll return.
    let resolver = v8::PromiseResolver::new(scope).unwrap();
    let promise = resolver.get_promise(scope);
    let resolver_global = v8::Global::new(scope, resolver);

    // Allocate op_id, push fetch task.
    let op_id = state.borrow_mut().alloc_op_id();
    state.borrow_mut().pending_resolvers.insert(op_id, resolver_global);
    state.borrow_mut().in_flight_fetches += 1;

    // Build the FetchTask. The task carries enough state to run
    // mainFetch on the compio thread.
    let request_global = v8::Global::new(scope, request_obj);
    let task = FetchTask {
        op_id,
        request: request_global,
        signal: v8::Global::new(scope, signal_obj),
    };
    state.borrow_mut().spawned_fetches.push(task);

    rv.set(promise.into());
}
```

### VI.2. The fetch task pump

<!-- v2 fix (CRITICAL-17): there is no `with_isolate_lock`. The runtime
is single-threaded; compio futures run on the same thread that owns
the V8 isolate. The pattern is op_id allocation + spawned_fetches
queue + OpResult dispatch on the main thread, NOT lock acquisition. -->

The runtime is **single-threaded per isolate** (AGENTS.md). compio
futures run on the same thread that owns V8. There is no lock to
acquire — code that needs the V8 scope simply runs in the next
iteration of the runtime pump, OR an explicit yield-point hands
control back to the pump.

The fetch flow:

1. `fetch_callback` (running with V8 scope) builds the synchronous
   parts of the Request (constructor, init parsing). Inserts a
   `FetchTask { op_id, request: Global, signal: Global }` into
   `state.spawned_fetches`. Returns the resolver's Promise.
2. The runtime pump drains `spawned_fetches`, calling
   `execute_fetch_native(task, state)` to mint the async future.
3. **The async future runs on the same thread.** When it needs to
   read JS state (e.g. body chunks during a streaming upload), it
   yields to the pump and the pump re-enters V8 to do the read,
   then schedules the future to resume. This is the same pattern
   the existing `crates/runtime/src/transport/ssrf.rs:spawn_body_reader`
   uses today.
4. When the future completes, it pushes an
   `OpResult::Fetch { op_id, inner_response }` onto the result
   queue. The pump processes results inside V8, mints the Response
   wrapper, resolves the resolver.

```rust
pub fn execute_fetch_native(
    task: FetchTask,
    state: SharedState,
) -> Pin<Box<dyn Future<Output = OpResult>>> {
    // Synchronous V8 work has ALREADY happened in fetch_callback —
    // RequestState was extracted into a plain-Rust InnerRequest
    // before this future was ever spawned. The V8 globals in `task`
    // are kept around only for body-stream pulls (drive_readable_stream
    // dispatches a sub-task that re-enters V8 to read a chunk) and for
    // the abort-listener registered on `task.signal`.
    Box::pin(async move {
        // No V8 scope needed here — InnerRequest is plain Rust.
        // (The body-source side of InnerRequest may hold a v8::Global<ReadableStream>
        // for streaming uploads, but reading from it goes through
        // drive_readable_stream which yields to the pump for the V8 work.)
        let inner_response = main_fetch(state.clone(), task.inner_request, /*recursive*/ false).await;
        OpResult::Fetch { op_id: task.op_id, inner_response }
    })
}
```

When the runtime pump dequeues an `OpResult::Fetch`, it enters V8 and
either resolves the resolver with a fresh Response wrapper (success
path) or rejects with a TypeError (network-error path). The Response
wrapper includes the body stream (built via `from_native_source`),
which the user can then read via `response.body.getReader()`.

### VI.3. Streaming uploads — the reverse direction

When the request body is a `BodySource::Stream`, the request body
is sent as `Transfer-Encoding: chunked`. The chunks are pulled from
the JS ReadableStream by `drive_readable_stream`:

```rust
fn drive_readable_stream(
    state: SharedState,
    stream_global: v8::Global<v8::Object>,
) -> impl futures::Stream<Item = Result<Vec<u8>, std::io::Error>> {
    // The pattern: each next() yield-point asks the pump to read one
    // chunk. The pump, when next entering V8, calls read_one_chunk(scope, ...)
    // (fetch's body_stream helper) and posts the result back via a
    // CompletionHandle that the future awaits.
    futures::stream::unfold(stream_global, move |stream_global| {
        let state = state.clone();
        async move {
            // schedule_v8_work runs the closure on the runtime's V8
            // thread (which is THIS thread, but inside the next pump
            // iteration). The closure builds a v8::Scope, calls
            // read_one_chunk, hands the result back via channel.
            let chunk = state.schedule_v8_work(move |scope| {
                let stream_local = v8::Local::new(scope, &stream_global);
                // fetch::body_stream::read_one_chunk:
                read_one_chunk(scope, stream_local).await
            }).await;

            match chunk {
                Some(Ok(bytes)) => Some((Ok(bytes), stream_global)),
                Some(Err(e)) => Some((Err(stream_err_to_io(e)), stream_global)),
                None => None,  // stream done
            }
        }
    })
}
```

`schedule_v8_work` is the existing runtime API used by the current
`spawn_body_reader` (`crates/runtime/src/transport/ssrf.rs:415-450` in v1
state) — it queues a closure that the next pump iteration will run
inside a `v8::Scope`, posting the closure's return value back via a
oneshot channel. NOT a lock.

`read_one_chunk` is a fetch-internal helper (in
`fetch/body_stream.rs`, see §III.4.1) that returns
`Option<Result<Vec<u8>, JsError>>`: `Some(Ok(bytes))` for a chunk,
`Some(Err(e))` for a stream error, `None` for stream close. It is
NOT a streams-native export — it lives in fetch.

### VI.4. Aborting the in-flight fetch

When `signal.abort()` fires after the fetch has started:

1. The abort listener registered in `fetch_callback` runs.
2. It calls into `fetch_params.controller.abort(reason)`.
3. The controller sets a flag the cyper send loop checks between body
   chunks.
4. The body-stream pull future returns `Pending`/`Err` with the abort
   reason; cyper's request future resolves with an error.
5. `processResponse` runs; sees the abort, rejects the user-facing
   promise with the signal's reason.

The existing `crates/runtime/src/channel::CancelFlag` is reused for
the controller.abort signal — see `crates/runtime/src/transport/ssrf.rs:271-298`
for the existing pattern.

## VII. Streaming response body — the forward direction

The user-visible `response.body` is a *native* ReadableStream backed
by a Rust `NetworkBodySource` (or a chained codec source). When the
user calls `response.body.getReader().read()`:

1. The streams-native default-reader code path runs (a Rust fn).
2. The controller's `[[pullAlgorithm]]` is the source's pull, which
   awaits the next chunk from cyper.
3. Cyper yields the chunk; the stream's controller enqueues it.
4. The reader's `read()` Promise resolves with `{ value, done }`.

This bypasses all the polyfill's `__readStreamToBytes` JS-side glue.
The throughput improvement vs. the polyfill should be ~3-5× on chunked
SSE responses (LLM streaming), since each chunk currently:

- Crosses Rust→V8 (push to `__streams[stream_id]`).
- Triggers a microtask checkpoint.
- Is decoded from string → Uint8Array via TextEncoder if it was a
  string chunk.
- Is read by JS, which encodes back to string for the user-facing
  `response.text()` accumulator.

Native: a single Rust call from the cyper future to the stream's
controller, then a single V8 enter when the user-facing reader
resolves.

### VII.1. Cancellation propagation

When the user calls `body.cancel()` or `body.getReader().cancel()`:
1. Streams-native's cancel fires.
2. The `cancelAlgorithm` on the controller (set by
   `from_native_source`) is the source's cancel, which sets the
   controller's CancelFlag.
3. The cyper body-stream future drops, closing the underlying TCP
   connection.

### VII.2. Reading SSE / line-delimited JSON in user code

A user pattern that fails today on the polyfill but works native:

```js
const resp = await fetch("/api/stream");
const reader = resp.body.pipeThrough(new TextDecoderStream()).getReader();
while (true) {
  const { value, done } = await reader.read();
  if (done) break;
  // value is a string chunk (TextDecoderStream output).
  console.log(value);
}
```

The polyfill's `pipeThrough(TextDecoderStream)` doesn't work because
TextDecoderStream is a TransformStream constructed against the polyfill
ReadableStream class, but `response.body` is a *different* ReadableStream
class (the kernel-backed one) — `pipeThrough` does an instance-of check
that fails. Native fixes by construction (one ReadableStream class).

## VIII. Redirects (§5.6) — corner cases

### VIII.1. The 20-redirect limit

Spec §5.6 step 6: if `request.redirectCount === 20`, return network
error. Implementation: increment `redirect_count` on each redirect,
check before recursing into `mainFetch`.

### VIII.2. Method/body rewriting on 301/302/303

Spec §5.6 step 12:
- 301 or 302 + method == POST → method = GET, body = null.
- 303 + method != GET/HEAD → method = GET, body = null.

Plus: delete `request-body-header` set from headers (per spec
https://fetch.spec.whatwg.org/#request-body-header-name): exactly **4
names** —

```
Content-Encoding, Content-Language, Content-Location, Content-Type
```

<!-- v2 fix (CRITICAL-2): was 5 entries, including Content-Length.
That was undici's deviation (acknowledged inline in undici's source);
the spec set is 4. -->

Note: `Content-Length` is **not** in this set. (undici adds it as a
deviation; we follow the spec.)

### VIII.3. Body rewinding on 307/308

Per D-11 + spec §5.6 step 11: if status != 303 and request.body is
non-null and request.body.source is null (i.e. stream body), return
network error. Otherwise, on 307/308:

```rust
// step 14:
if request.body.borrow().is_some() {
    let source = match &request.body.borrow().as_ref().unwrap().source {
        BodySource::Bytes(rc) => BodySource::Bytes(rc.clone()),
        BodySource::Blob(rc, t) => BodySource::Blob(rc.clone(), t.clone()),
        BodySource::UrlSearchParams(rc) => BodySource::UrlSearchParams(rc.clone()),
        BodySource::FormData(rc, b) => BodySource::FormData(rc.clone(), b.clone()),
        BodySource::Stream => unreachable!("step 11 already returned network error"),
    };
    let new_body = build_body_from_source(scope, source);
    *request.body.borrow_mut() = Some(new_body);
}
```

`build_body_from_source` re-mints a fresh ReadableStream wrapping the
shared Rc<Vec<u8>> source. The original stream is now used (disturbed),
but the source is intact — same as workerd `rewindBody()` (`http.c++:210-224`).

### VIII.4. Authorization stripping on cross-origin redirect

<!-- v2 fix (CRITICAL-1): the spec strips ONLY `Authorization`, not 4
headers. Per https://fetch.spec.whatwg.org/#http-redirect-fetch step
13: "for each headerName of CORS non-wildcard request-header name,
delete headerName from request's header list." Per
https://fetch.spec.whatwg.org/#cors-non-wildcard-request-header-name:
the CORS non-wildcard request-header name set consists SOLELY of
`Authorization`. Workerd `http.c++:1880` matches: deletes only
`Authorization` and quotes the spec inline. -->

Per spec §5.6 step 13 + §4.10 "CORS non-wildcard request-header
name": if `currentURL.origin != locationURL.origin`, delete
**`Authorization`** from the request's header list. That's it —
ONE header.

```rust
let current_origin = current_url(request).origin();
let location_origin = location_url.origin();
if current_origin != location_origin {
    let mut headers = request.headers_list.borrow_mut();
    headers.retain(|(name, _)| !ascii_eq_ignore_case(name, b"authorization"));
}
```

`Cookie` and `Host` and `Proxy-Authorization` are forbidden request
headers (D-17) — user code can't set them in the first place; they
are added by the platform (Cookie via the platform's own handling
which we don't currently do; Host is set by hyper/cyper from the
URL). They do not need to be stripped here because they cannot be
present in `request.headers` to begin with.

#### VIII.4.1. workerd compat-date note (v2 fix MAJOR-30)

The workerd compat flag `StripAuthorizationOnCrossOriginRedirect` is
declared in `compatibility-date.capnp:989-992` with
`compatEnableDate("2025-09-01")` — meaning **default-ON** for
compatibility dates ≥ 2025-09-01. Today (2026-05-01), most workers
run it on. Our v1 design states "v1 says default off" was wrong —
the spec is also clear that this strip is mandatory. v2 enables it
unconditionally (no compat flag); the previous "compat off" claim
in v1 was a misread of workerd's `compatEnableDate` semantics.

### VIII.5. URL list and `Response.url` / `Response.redirected`

After every redirect, `request.urlList.push(location_url)`. The final
Response copies the urlList to its own `urlList` (via
`InnerResponse::url_list = request.url_list.clone()`). `Response.url`
returns the last entry serialized; `Response.redirected` returns
`urlList.len() > 1`. D-30.

## IX. AbortSignal / AbortController (DOM §3.3 + Fetch §5.4 step 31)

### IX.1. AbortSignal IDL

```webidl
[Exposed=*]
interface AbortSignal : EventTarget {
  [NewObject] static AbortSignal abort(optional any reason);
  [NewObject] static AbortSignal timeout([EnforceRange] unsigned long long milliseconds);
  [NewObject] static AbortSignal any(sequence<AbortSignal> signals);

  readonly attribute boolean aborted;
  readonly attribute any reason;
  undefined throwIfAborted();

  attribute EventHandler onabort;
};
```

### IX.2. AbortController IDL

```webidl
[Exposed=*]
interface AbortController {
  constructor();
  [SameObject] readonly attribute AbortSignal signal;
  undefined abort(optional any reason);
};
```

### IX.3. Rust state

<!-- v2 fix (CRITICAL-7, CRITICAL-8): added bidirectional source signals
↔ dependent signals pair per DOM §3.3.4. -->

```rust
pub struct AbortSignalState {
    aborted: Cell<bool>,
    reason: RefCell<Option<v8::Global<v8::Value>>>,
    /// Listeners added via addEventListener / dispatchEvent. Stored on
    /// the EventTarget base, not here — AbortSignal inherits EventTarget.
    /// (See dom/event_target.rs.)
    ///
    /// onabort attribute is also handled via EventTarget — set as a
    /// listener with key "abort" + a special "is-onabort-attribute" flag.
    onabort: RefCell<Option<v8::Global<v8::Function>>>,

    /// "Abort algorithms" list per DOM §3.3.1 — Rust-side algorithms
    /// (e.g. fetch's controller-abort) registered via add_abort_algorithm.
    /// Distinct from event listeners. NOT v8 globals (these are Rust
    /// callbacks the implementation registers internally).
    abort_algorithms: RefCell<Vec<Box<dyn FnOnce()>>>,

    /// "Source signals" set (DOM §3.3.4): the signals that, when aborted,
    /// abort this signal. Bidirectional with `dependent_signals`.
    /// Used by AbortSignal.any to flatten transitive dependents.
    /// (v2 fix CRITICAL-8: was missing in v1.)
    source_signals: RefCell<Vec<WeakV8Ref>>,

    /// "Dependent signals" set: the signals that this signal aborts when
    /// it itself aborts. Bidirectional with `source_signals`.
    dependent_signals: RefCell<Vec<WeakV8Ref>>,

    /// "Dependent" boolean flag (DOM §3.3.4): true iff this signal was
    /// returned by AbortSignal.any().
    is_dependent: Cell<bool>,

    /// AbortSignal.timeout: the timer id we can cancel on GC.
    timer_id: Cell<Option<TimerId>>,
}

pub struct AbortControllerState {
    signal_priv: PrivSymKey,
}
```

### IX.4. The abort algorithm (DOM §3.3.1)

<!-- v2 fix (CRITICAL-7, MAJOR-40): rewrote to match DOM spec ordering.
Per https://dom.spec.whatwg.org/#abortsignal-signal-abort:
  1. If aborted return.
  2. Set reason.
  3. dependentSignalsToAbort = collect non-aborted dependents.
  4. For each dep, set dep's reason.
  5. Run abort steps for signal (algorithms, then fire "abort").
  6. For each dep in dependentSignalsToAbort (in collection order): run abort
     steps for dep (algorithms, then fire "abort"). NOT recursive into signal_abort.
The native event dispatch is via EventTarget.dispatchEvent AFTER abort algorithms.
-->

```
DOM spec — "To signal abort an AbortSignal signal with reason":

1. If signal is aborted, return.
2. Set signal's reason to reason (or new "AbortError" DOMException).
3. Let dependentSignalsToAbort be a new list.
4. For each dependentSignal of signal's dependent signals:
   1. If dependentSignal is not aborted, then:
      1. Set dependentSignal's reason to signal's reason.
      2. Append dependentSignal to dependentSignalsToAbort.
5. Run the abort steps for signal:
   1. For each algorithm of signal's abort algorithms: run algorithm.
   2. Empty signal's abort algorithms.
   3. Fire an event named "abort" at signal (via dispatchEvent — this
      is the EventTarget surface, not an inline call).
6. For each dependentSignal of dependentSignalsToAbort:
   1. Run the abort steps for dependentSignal.
```

Implementation:

```rust
pub fn signal_abort(
    scope: &mut v8::PinScope,
    signal_obj: v8::Local<v8::Object>,
    signal: &AbortSignalState,
    reason: v8::Local<v8::Value>,
) {
    if signal.aborted.get() { return; }

    // Step 2: set reason BEFORE collecting dependents (so dep collection
    // sees the just-set reason for cascading).
    signal.aborted.set(true);
    *signal.reason.borrow_mut() = Some(v8::Global::new(scope, reason));

    // Step 3: collect non-aborted dependents AND set their reason in the
    // same pass (per DOM step 4). Note: we do NOT call signal_abort on
    // them here — that's the second pass (step 6).
    let mut deps_to_abort: Vec<v8::Local<v8::Object>> = Vec::new();
    let dep_refs: Vec<_> = signal.dependent_signals.borrow().clone();
    for dep_weak in dep_refs {
        let dep_obj = match dep_weak.upgrade(scope) {
            Some(o) => o,
            None => continue,
        };
        let dep_state = get_state::<AbortSignalState>(scope, dep_obj);
        if !dep_state.aborted.get() {
            dep_state.aborted.set(true);
            *dep_state.reason.borrow_mut() = Some(v8::Global::new(scope, reason));
            deps_to_abort.push(dep_obj);
        }
    }

    // Step 5: run abort steps for `signal`.
    run_abort_steps(scope, signal_obj, signal);

    // Step 6: run abort steps for each collected dependent.
    for dep_obj in deps_to_abort {
        let dep_state = get_state::<AbortSignalState>(scope, dep_obj);
        run_abort_steps(scope, dep_obj, dep_state);
    }
}

/// "Abort steps" sub-algorithm: run abort algorithms, then fire "abort"
/// event via the EventTarget.
fn run_abort_steps(
    scope: &mut v8::PinScope,
    signal_obj: v8::Local<v8::Object>,
    signal: &AbortSignalState,
) {
    // Sub-step 1: run abort algorithms (Rust-side callbacks: e.g. fetch
    // controller's terminate, EventTarget's listener-removal-on-signal).
    let algorithms = std::mem::take(&mut *signal.abort_algorithms.borrow_mut());
    for algorithm in algorithms {
        algorithm();  // FnOnce — consumes
    }

    // Sub-step 3: fire "abort" event AFTER algorithms (v2 fix MAJOR-40).
    // Use the native EventTarget.dispatchEvent surface — the same path
    // user-installed listeners go through, which guarantees correct
    // ordering and capture/bubble semantics.
    let event = build_abort_event(scope, signal_obj);
    crate::dom::event_target::dispatch_event(scope, signal_obj, event);
}
```

### IX.5. AbortSignal.timeout — GC retention (CRITICAL-9)

<!-- v2 fix (CRITICAL-9): DOM spec at
https://dom.spec.whatwg.org/#dom-abortsignal-timeout step 3 says
"for the duration of this timeout, if signal has any event
listeners registered for its abort event, there must be a strong
reference from global to signal." A WeakV8Ref alone leaks the
timeout when the signal goes out of scope before listeners fire. -->

```rust
#[v8_static_method]
fn timeout(scope: &mut v8::PinScope, ms: u64) -> v8::Local<'_, v8::Object> {
    let signal_obj = mint_abort_signal(scope);
    let signal_state = get_state::<AbortSignalState>(scope, signal_obj);
    let signal_global = v8::Global::new(scope, signal_obj);

    // v2: register a strong-reference root in SharedState that survives
    // the user dropping the signal handle. The root is removed in:
    //  (a) the timer fires and signal_abort is called (no more reason
    //      to retain), OR
    //  (b) the user removes all "abort" event listeners (we update
    //      retention in EventTarget::remove_event_listener and verify
    //      the listener count is 0).
    let timeout_root = state.borrow_mut().pin_signal_for_timeout(signal_global.clone());
    signal_state.timer_id.set(Some(timeout_root.timer_id));

    // Schedule abort via compio time::sleep.
    let task = async move {
        compio::time::sleep(Duration::from_millis(ms)).await;
        TimerEvent::AbortSignalTimeout { signal_global }
    };
    state.borrow_mut().schedule_timer(timeout_root.timer_id, task);

    signal_obj
}
```

The `pin_signal_for_timeout` helper (new SharedState method) inserts
the `v8::Global<AbortSignal>` into a `Vec<v8::Global<AbortSignal>>`
keyed by timer_id; the entry is removed when (a) the timer event
fires and `signal_abort` runs, OR (b) the EventTarget removes the
last "abort" listener and the count drops to 0 (per DOM's wording —
the strong ref is needed only WHILE listeners are registered). When
neither (a) nor (b) trigger — e.g. the timer fires but there are
listeners — the strong ref persists until the next removal. This
matches DOM step 3 exactly.

### IX.6. AbortSignal.any — transitive flattening (CRITICAL-8)

<!-- v2 fix (CRITICAL-8): bidirectional source/dependent walk per DOM. -->

Spec §3.3.4 "create a dependent abort signal":

```
1. Let resultSignal be a new AbortSignal.
2. For each signal in signals:
   1. If signal is aborted, set resultSignal's reason = signal's reason
      AND aborted = true; return resultSignal (already aborted).
3. Set resultSignal's dependent flag (is_dependent = true).
4. For each signal in signals:
   1. If signal is dependent (was returned by a prior any() call):
      For each src of signal's source signals:
        - Append src to resultSignal's source_signals.
        - Append resultSignal to src's dependent_signals.
   2. Else:
      - Append signal to resultSignal's source_signals.
      - Append resultSignal to signal's dependent_signals.
5. Return resultSignal.
```

```rust
#[v8_static_method]
fn any(scope: &mut v8::PinScope, signals: v8::Local<v8::Value>) -> Result<v8::Local<'_, v8::Object>, OpError> {
    let signal_objs: Vec<v8::Local<v8::Object>> = parse_sequence_of_signals(scope, signals)?;

    let result_obj = mint_abort_signal(scope);
    let result_state = get_state::<AbortSignalState>(scope, result_obj);

    // Step 2: short-circuit if any input signal is already aborted.
    for &input_obj in &signal_objs {
        let input_state = get_state::<AbortSignalState>(scope, input_obj);
        if input_state.aborted.get() {
            let reason = input_state.reason.borrow().as_ref().unwrap()
                .clone().open(scope);
            result_state.aborted.set(true);
            *result_state.reason.borrow_mut() = Some(v8::Global::new(scope, reason));
            return Ok(result_obj);
        }
    }

    // Step 3.
    result_state.is_dependent.set(true);

    // Step 4: bidirectional pairing, with transitive flattening for
    // already-dependent inputs.
    for &input_obj in &signal_objs {
        let input_state = get_state::<AbortSignalState>(scope, input_obj);
        if input_state.is_dependent.get() {
            // Flatten: copy source_signals from the dependent input.
            for src_weak in input_state.source_signals.borrow().iter() {
                if let Some(src_obj) = src_weak.upgrade(scope) {
                    add_source_dependent_pair(scope, src_obj, result_obj);
                }
            }
        } else {
            add_source_dependent_pair(scope, input_obj, result_obj);
        }
    }

    Ok(result_obj)
}

fn add_source_dependent_pair(
    scope: &mut v8::PinScope,
    src_obj: v8::Local<v8::Object>,
    dep_obj: v8::Local<v8::Object>,
) {
    let src_state = get_state::<AbortSignalState>(scope, src_obj);
    let dep_state = get_state::<AbortSignalState>(scope, dep_obj);
    src_state.dependent_signals.borrow_mut().push(WeakV8Ref::new(scope, dep_obj));
    dep_state.source_signals.borrow_mut().push(WeakV8Ref::new(scope, src_obj));
}
```

This handles the previously-broken case: `AbortSignal.any([s_dep, s2])`
where `s_dep = AbortSignal.any([s_a, s_b])` — the new result signal
has source signals `{s_a, s_b, s2}` (NOT `{s_dep, s2}`), so aborting
`s_a` correctly aborts the new signal even though `s_dep` is no longer
referenced.

## X. EventTarget (DOM §2.7) — the AbortSignal base class

<!-- v2 fix (MAJOR-41): EventTarget lives in crates/runtime/src/web/dom/, NOT
under fetch/. It is a DOM primitive shared with future WebSocket /
EventSource. -->

Implementation file: `crates/runtime/src/web/dom/event_target.rs`. Shared
with future WebSocket / EventSource / MessagePort implementations.
Per spec, EventTarget is the base class for ~30 DOM types — it does
not "belong to fetch."

EventTarget IDL:

```webidl
[Exposed=*]
interface EventTarget {
  constructor();
  undefined addEventListener(DOMString type, EventListener? callback, optional (AddEventListenerOptions or boolean) options = {});
  undefined removeEventListener(DOMString type, EventListener? callback, optional (EventListenerOptions or boolean) options = {});
  boolean dispatchEvent(Event event);
};

callback interface EventListener {
  undefined handleEvent(Event event);
};
```

Rust state:

```rust
pub struct EventTargetState {
    listeners: RefCell<HashMap<String, Vec<RegisteredListener>>>,
}

pub struct RegisteredListener {
    callback: v8::Global<v8::Function>,
    capture: bool,
    once: bool,
    passive: bool,
    /// Signal for removal: when this signal aborts, remove the listener.
    signal: Option<v8::Global<v8::Object>>,
}
```

`addEventListener` looks up `type` in the map, appends; `removeEventListener`
strips matching entries (matching by callback identity + capture). The
`signal` removal pattern requires registering an abort listener on the
provided signal that fires `removeEventListener` on this target. A small
piece of cross-class coupling.

`dispatchEvent` invokes all listeners for `event.type` in registration
order; `event.target` and `event.currentTarget` are set to this; if
the event's `cancelable` flag is set and a listener calls
`event.preventDefault()`, `dispatchEvent` returns false.

EventTarget plus a minimal `Event` class are both `#[v8_class]`. AbortSignal
inherits from EventTarget via the macro extension `#[v8_inherit(EventTarget)]`
(XIV.1).

## XI. FormData (FileAPI) — IDL surface

Even though the multipart parser is deferred to v2, the `FormData`
class must exist for `Request`/`Response` to accept FormData bodies.

```webidl
typedef (Blob or USVString) FormDataEntryValue;

[Exposed=(Window,Worker)]
interface FormData {
  constructor(optional HTMLFormElement form, optional HTMLElement? submitter = null);

  undefined append(USVString name, USVString value);
  undefined append(USVString name, Blob blobValue, optional USVString filename);
  undefined delete(USVString name);
  FormDataEntryValue? get(USVString name);
  sequence<FormDataEntryValue> getAll(USVString name);
  boolean has(USVString name);
  undefined set(USVString name, USVString value);
  undefined set(USVString name, Blob blobValue, optional USVString filename);

  iterable<USVString, FormDataEntryValue>;
};
```

Server-side (no DOM), the constructor's HTMLFormElement variant throws
if invoked with anything (matches workerd). v1 implementation:

```rust
pub struct FormDataState {
    entries: RefCell<Vec<(String, FormDataEntry)>>,
}

pub enum FormDataEntry {
    Str(String),
    Blob { bytes: Rc<Vec<u8>>, mime: Option<String>, filename: Option<String> },
}
```

Append/set/get/has/getAll/delete/iterate are 1:1 spec mappings.
Multipart serialization (used by `extract_body` for FormData bodies)
lives in `fetch/form_data.rs::serialize_multipart`. Multipart **parsing**
(used by `formData()` consumer) throws TypeError in v1 (see Non-goals).

## XII. The `data:` URL scheme (D-21, §5.4)

Spec §5.4 step 3 (case `data:`):

```
1. Let dataURLStruct = data: URL processor on request.currentURL.
2. If dataURLStruct is failure, return network error.
3. Let mimeType = serialize the MIME type from dataURLStruct.
4. Return a response whose status is 200, status message is "OK",
   header list is « (`Content-Type`, mimeType) », and body is
   dataURLStruct's body as a body.
```

<!-- v2 fix (CRITICAL-20, Process-flaw 1): D-21 verification resolved.
The `data-url` crate IS NOT in the workspace; we add it now. -->

Implementation in `fetch/data_url.rs`: an adapter around the
**`data-url`** crate (https://crates.io/crates/data-url, v0.3.x,
MIT/Apache-2.0; the official Servo implementation, ~700 LOC, used
by `webrender` and other Servo components):

```rust
// Cargo.toml addition (v2 fix D-21):
//   data-url = "0.3"

use data_url::DataUrl;

pub fn fetch_data_url(url: &url::Url) -> Result<InnerResponse, JsError> {
    let url_str = url.as_str();
    let parsed = DataUrl::process(url_str)
        .map_err(|e| JsError::type_error(&format!("invalid data: URL: {e}")))?;

    let (body, _frag) = parsed.decode_to_vec()
        .map_err(|e| JsError::type_error(&format!("invalid data: URL body: {e}")))?;

    let mime = parsed.mime_type().to_string();
    let header_list = vec![
        (b"content-type".to_vec(), mime.into_bytes()),
    ];

    Ok(InnerResponse {
        status: 200,
        status_text: "OK".to_string(),
        header_list,
        body_source: Some(Box::new(BytesNativeSource::new(Rc::new(body)))),
        url_list: vec![url.clone()],
        ..Default::default()
    })
}
```

The `data-url` crate handles base64 decoding, percent decoding, MIME
type normalization, and the spec's edge cases (empty MIME → text/plain,
charset parameter, etc.). undici's hand-rolled parser is ~270 LOC; the
crate path is smaller and battle-tested.

## XIII. V8 internal-fields layout — per slot

Per the streams-native D-2 single-source rule, every spec internal
slot lives in EXACTLY one place. Here's the per-class audit.

### XIII.1. Request

| Slot | Where | Rule |
|------|-------|------|
| [[method]] | Rust `ByteString` | pure data |
| [[urlList]] | Rust `RefCell<Vec<url::Url>>` | pure data |
| [[headers]] | V8 priv sym `headers` | wrapper identity ([SameObject]) |
| [[headers' guard]] | Rust (in the Headers struct's new Guard field) | pure data, lives with Headers |
| [[body]] | Rust `RefCell<Option<BodyImpl>>` (BodyImpl carries `v8::Global<ReadableStream>`) | pure data + JS identity |
| [[redirect]], [[mode]], [[credentials]], [[cache]], [[destination]], [[referrer]], [[referrerPolicy]], [[duplex]], [[priority]] | Rust `Cell<EnumType>` / `RefCell<...>` | pure data |
| [[integrity]] | Rust `RefCell<String>` | pure data |
| [[keepalive]], [[reload-navigation]], [[history-navigation]], [[unsafe-request]], [[done]], [[timing-allow-failed]], [[use-CORS-preflight]], [[use-URL-credentials]] | Rust `Cell<bool>` | pure data |
| [[redirect-count]] | Rust `Cell<u32>` | pure data |
| [[signal]] | V8 priv sym `signal` | wrapper identity |
| [[client]] | Not stored (D-3 ignores) | n/a |
| [[window]] | Not stored (D-3 ignores) | n/a |
| [[origin]] | Not stored (D-3 ignores; computed on-demand from URL for the Origin header) | n/a |
| [[serviceWorkers]] | Not stored (D-1 non-goals) | n/a |
| [[policyContainer]] | Not stored (D-1 non-goals) | n/a |
| [[traversableForUserPrompts]] | Not stored | n/a |

### XIII.2. Response

| Slot | Where | Rule |
|------|-------|------|
| [[type]] | Rust `Cell<ResponseType>` | pure data |
| [[urlList]] | Rust `RefCell<Vec<url::Url>>` | pure data |
| [[redirected]] | Rust `Cell<bool>` | derived from urlList.len() but cached |
| [[status]] | Rust `Cell<u16>` | pure data |
| [[statusText]] | Rust `RefCell<Vec<u8>>` | pure data |
| [[headers]] | V8 priv sym `headers` | wrapper identity ([SameObject]) |
| [[body]] | Rust `RefCell<Option<BodyImpl>>` | pure data + JS identity |
| [[bodyInfo]] | Rust `RefCell<BodyInfo>` | pure data |
| [[CORS-exposed-header-name list]] | Rust `RefCell<Vec<Vec<u8>>>` (empty in v1) | pure data |
| [[range-requested]], [[request-includes-credentials]], [[timing-allow-passed]], [[has-cross-origin-redirects]] | Rust `Cell<bool>` | pure data |
| [[internal-response]] | V8 priv sym `internalResponse` (None in v1 since no filtering) | wrapper identity |
| (extension) webSocket | V8 priv sym `webSocket` | wrapper identity |

### XIII.3. AbortSignal

| Slot | Where | Rule |
|------|-------|------|
| [[aborted]] | Rust `Cell<bool>` | pure data |
| [[reason]] | V8 priv sym `reason` (because the reason is observably JS-identity-equal across access) | JS identity |
| [[abortAlgorithms]] | Rust `RefCell<Vec<v8::Global<Function>>>` (callbacks are V8 globals; the list itself is Rust state) | pure data + JS identity per entry |
| [[dependentSignals]] | Rust `RefCell<Vec<WeakV8Ref>>` | pure data |
| (extension) onabort | V8 priv sym `onabort` (per the spec, it's a "settable EventHandler IDL attribute") | JS identity |

### XIII.4. EventTarget

| Slot | Where | Rule |
|------|-------|------|
| [[event listener list]] | Rust `RefCell<HashMap<String, Vec<RegisteredListener>>>` | pure data + JS identity per listener entry |

## XIV. Macro extensions required

The existing `#[v8_class]` macro supports method/getter/setter/constructor
classification, internal-field 0 with weak-finalizer reclamation,
`Vec<u8>` from ArrayBufferView, ByteString newtype, `Vec<Vec<u8>>` and
`Option<Vec<u8>>` returns, `#[v8_name]`, `#[v8_to_string_tag]`,
`#[v8_inherit_intrinsic]`, `#[reject_shared]`, lifetime-tied
`Local<'s, _>` returns. Streams-native is adding `#[v8_async_method]`,
`#[v8_async_iterator]`, `[EnforceRange] u64`, dictionary parser
helpers — fetch reuses all of those.

This design forces these additional extensions on top of streams-native's:

### XIV.1. `#[v8_inherit(EventTarget)]` — class inheritance

<!-- v2 fix Process-flaw 8: Body is NOT a V8 base class anymore — see
§II.2 — so `#[v8_inherit(Body)]` is dropped. The only `#[v8_inherit]`
user is AbortSignal, inheriting EventTarget. -->

Used by:

- `AbortSignal : EventTarget` (DOM IDL).

Future users (post-v1):

- `WebSocket : EventTarget`
- `EventSource : EventTarget`
- `MessagePort : EventTarget`
- `XMLHttpRequest : EventTarget`

The macro generates `FunctionTemplate::inherit(scope, BaseClass::install_template(scope))`
in the install routine. The base class's instance template properties
(getters, methods) become accessible from the derived class through
the prototype chain.

```rust
#[v8_class]
#[v8_inherit(EventTarget)]
impl AbortSignal {
    // ... AbortSignal-specific methods
}
```

Implementation sketch: read the `#[v8_inherit(...)]` attribute on the
impl block; in the install codegen, before installing this class's
methods, call `tmpl.inherit(BaseClass::install_template(scope))`. The
base class's template must be cached per realm (in SharedState) so
multiple inheritors don't double-install.

`FunctionTemplate::inherit` is confirmed to exist in rusty-v8 147.x at
`template.rs:825` (cited in the review).

LOC: ~50, hours: ~2.

### XIV.2. WebIDL dictionary parsing — `RequestInit`, `ResponseInit`

Streams-native already builds dictionary helpers (`parse_pipe_options`,
`parse_queuing_strategy_init`); fetch needs much larger ones.
`RequestInit` has 16 members; `ResponseInit` has 3.

Dictionary parser pattern:

```rust
pub fn parse_request_init(scope: &mut v8::PinScope, value: v8::Local<v8::Value>)
    -> Result<RequestInit, OpError>
{
    if value.is_undefined() {
        return Ok(RequestInit::default());
    }
    let obj = value.to_object(scope)
        .ok_or_else(|| OpError::type_error("RequestInit must be an object"))?;
    let mut init = RequestInit::default();

    // method (optional ByteString)
    if let Some(v) = obj_get(scope, obj, "method") {
        init.method = Some(read_byte_string(scope, v)?);
    }

    // headers (optional HeadersInit)
    if let Some(v) = obj_get(scope, obj, "headers") {
        init.headers = Some(extract_headers_init(scope, v)?);
    }

    // body (optional BodyInit?)
    if let Some(v) = obj_get(scope, obj, "body") {
        // The "?" means null is also valid; the outer Option means
        // present-vs-absent. Match {None, Some(None), Some(Some(BodyInit))}.
        if v.is_null() {
            init.body = Some(None);
        } else {
            init.body = Some(Some(v));  // BodyInit is a union; concrete
                                        // dispatch happens in extract_body.
        }
    }

    // ... 13 more fields

    // window: special — must be `null` or absent.
    if let Some(v) = obj_get(scope, obj, "window") {
        if v.is_null() {
            init.window = WindowInit::Null;
        } else {
            return Err(OpError::type_error("'window' option must be null"));
        }
    }

    Ok(init)
}
```

Hand-rolled. Lives in `fetch/dictionaries.rs`. ~400 LOC for both
dictionaries + `WebSocketInit` (D-13 extension).

LOC: ~400, hours: ~4.

### XIV.3. Enum string → variant conversion

For each enum (RequestMode, RequestCache, etc.), a `from_str` impl:

```rust
impl RequestMode {
    pub fn from_str(s: &str) -> Result<Self, OpError> {
        match s {
            "navigate" => Ok(Self::Navigate),
            "same-origin" => Ok(Self::SameOrigin),
            "no-cors" => Ok(Self::NoCors),
            "cors" => Ok(Self::Cors),
            "websocket" => Ok(Self::WebSocket),
            "webtransport" => Ok(Self::WebTransport),
            _ => Err(OpError::type_error(&format!("invalid RequestMode: {s}"))),
        }
    }
    pub fn as_str(&self) -> &'static str { /* ... */ }
}
```

The macro could generate these from a `#[v8_enum]` attribute, but it's
simpler to hand-roll the ~10 enums (~30 LOC each = 300 LOC). Decision:
hand-roll. LOC: ~300, hours: ~2.

### XIV.4. Static method support

Currently `#[v8_method]` is for instance methods. Static methods like
`Response.json(data, init)`, `Response.error()`, `Response.redirect(url, status)`,
`AbortSignal.abort(reason)`, `AbortSignal.timeout(ms)`, `AbortSignal.any(signals)`
need to be installable on the constructor function rather than the
prototype. Add `#[v8_method(static_method)]` (or `#[v8_static_method]`).

LOC: ~50, hours: ~1.5.

### XIV.5. `[NewObject]` semantics

Per WebIDL, `[NewObject]` getters must return a fresh object each
call (e.g. `Request.clone()`). The current macro returns the value as
emitted; fetch's `clone()` mints a new wrapper synchronously and
returns it. No macro work needed — this is a method-level invariant
the implementation upholds.

### XIV.6. Fetch-internal helpers — `read_all_bytes` / `read_one_chunk`

<!-- v2 fix (CRITICAL-13): these are NOT streams-native deliverables.
They live in fetch/body_stream.rs and are layered on streams-native's
public RS reader API (acquire_default_reader / reader_read), which
HAS shipped (RS+DefaultController+DefaultReader landed). -->

Helpers in `crates/runtime/src/fetch/body_stream.rs` (NOT in streams).
Layered on streams-native's `acquire_default_reader` /
`reader_read` / `release_reader` Rust-side functions, which are part
of streams-native's already-shipped surface (RS+DefaultController+
DefaultReader). Implementation details are in §III.4.1.

```rust
// crates/runtime/src/fetch/body_stream.rs
pub async fn read_all_bytes(
    scope: &mut v8::PinScope,
    stream: v8::Local<v8::Object>,
) -> Result<Vec<u8>, v8::Global<v8::Value>>;

pub async fn read_one_chunk(
    scope: &mut v8::PinScope,
    stream: v8::Local<v8::Object>,
) -> Option<Result<Vec<u8>, v8::Global<v8::Value>>>;
```

LOC: ~120 in fetch (NOT in streams); hours: ~2.

### XIV.7. Macro extension summary

<!-- v2 fix: XIV.1 drops Body inheritance (Body is a Rust trait now,
not a V8 base class — see §II.2). EventTarget remains. -->

| # | Extension | LOC | Hours |
|---|-----------|-----|-------|
| XIV.1 | `#[v8_inherit(EventTarget)]` (only — Body is a Rust trait now) | ~50 | 2h |
| XIV.2 | RequestInit / ResponseInit dictionary parsers | ~400 | 4h |
| XIV.3 | Enum string ↔ variant helpers (10 enums) | ~300 | 2h |
| XIV.4 | `#[v8_method(static_method)]` | ~50 | 1.5h |
| XIV.6 | fetch `read_all_bytes` / `read_one_chunk` (in fetch crate, NOT streams) | ~120 | 2h |
| **Total** | | **~920** | **~11.5h** |

Note: XIV.5 ([NewObject]) is a method-level invariant, no macro work.

## XV. Dependency reconciliation

Every commitment in `compression-streams-native.md` "Dependencies on
sibling projects" — both directions — is honoured. Per-point audit:

| Comp.-streams point | Native fetch deliverable | Section |
|---------------------|--------------------------|---------|
| Compression #1 (transformer plumbing) | Streams-native deliverable. Fetch consumes streams' `Transformer` trait via `build_chained_native_source`. | §V.9 |
| Compression #2 (cancel exactly once) | Streams-native deliverable. | n/a |
| Compression #3 (desired_size soft signal) | Streams-native deliverable. The `NetworkBodySource` checks `desired_size` between cyper chunks and pauses cyper if backpressure. | §VII |
| Compression #4 (`[[readable]]` / `[[writable]]` priv-sym slots readable from Rust) | Streams-native deliverable. | n/a |
| Compression #5 (TransformStream internal-field count ≥ 2) | Streams-native deliverable. | n/a |
| Compression #6 (Accept-Encoding default `br, gzip, deflate` HTTPS / `gzip, deflate` HTTP) | **Fetch deliverable — D-15.** Implemented in `httpNetworkOrCacheFetch` step 8.19. *(v2 fix MAJOR-31: ordering is `br, gzip, deflate` for HTTPS, consistently. Old v1 was inconsistent between D-15 and §XV row 6.)* | §V.7 |
| Compression #7 (byte-faithful Content-Encoding parsing) | **Fetch deliverable — D-15.** Header list is `Vec<(Vec<u8>, Vec<u8>)>` per headers-native; parsing happens once in the registered hook and the Codec chain consumes the lowercase name. | §V.9 |
| Compression `with_response_body_hook` API surface | **Fetch deliverable — implemented per the contract.** *(v2 fix CRITICAL-12: v1 unilaterally repudiated this API; that broke compression-streams-native's stated dependency. v2 re-implements the contract.)* The named API `ResponseBuilder::with_response_body_hook(hook: ResponseBodyHook)` ships in `crates/runtime/src/fetch/response.rs`. compression-streams-native registers itself as the canonical hook in its init code; fetch's `finalize_response_body` invokes the registered hook (see §V.9.0). The hook's `HookCtx` exposes the mutable header list (so the hook can strip Content-Encoding/Length per Compression #3) and the URL+status (for hook policy decisions). | §V.9, §V.9.0 |

| Streams-native point (consumed by fetch) | Native fetch usage | Section |
|------------------------------------------|--------------------| --------|
| `ReadableStream::from_native_source` | Used to wrap `NetworkBodySource` and (chained) codec source as `Response.body`. (Shipped.) | §V.9, §VII |
| `WritableStream::from_native_sink` | NOT used in v1 — request bodies bypass this for performance (we drain the stream directly). (Streams-native WS pending — does not block fetch.) | n/a |
| `pipe_native_internal` | NOT used in v1 (the chained source is built as a single composite NativeSource, not as separate streams piped together — same as workerd's response-body wrapping). | n/a |
| Public `tee()` | Used by `clone()`. (Shipped on RS.) | §III.5 |
| Rust `acquire_default_reader` / `reader_read` / `release_reader` | Used by fetch's `read_all_bytes` / `read_one_chunk` (in `fetch/body_stream.rs`). (Shipped — RS+DefaultController+DefaultReader are landed.) | §III.4.1 |
| `is_disturbed_obj`, `is_locked_obj` | Used by `extract_body`, `clone()`, body consumers. (Shipped on RS.) | §III |

| Headers-native point (extended by fetch) | Native fetch deliverable | Section |
|-------------------------------------------|--------------------------|---------|
| Guard machinery (deferred per headers-native "Out of scope for v1") | **Fetch deliverable.** This design adds the `Guard` enum field and the forbidden-name lists to `crates/runtime/src/web/headers.rs`. | §II.6, §IV.6 |
| `[SameObject]` aliasing | **Fetch deliverable.** `Request.headers` and `Response.headers` use the priv-sym aliasing pattern documented in headers-native "Forward compatibility" §. | §IV.8 |
| `Headers::wrap_for_request` / `wrap_for_response` | **Fetch deliverable.** New constructors that mint a Headers wrapper sharing an Rc<RefCell<HeaderList>> with a Request/Response state. | §IV.8 |

## XVI. Test plan

Mirrors the streams-native and headers-native test pattern: hand-written
tests for the corner cases the polyfill got wrong, plus a vendored WPT
suite running against our runtime.

### XVI.1. WPT inventory (must-pass v1)

<!-- v2 fix (Process-flaw 3): recounted via tests/wpt/fetch/api/.
v1 covered 4-of-9 directories (61 of ~135 .any.js files = 45%).
v2 adds redirect/, body/, basic/, credentials/ to the must-pass-v1
set so "full WHATWG compat" (Goal 1) is delivered. -->

Vendored from `https://github.com/web-platform-tests/wpt` at a pinned
commit; lives in `crates/runtime/tests/wpt/fetch/api/`. Counts based
on the existing checkout at `tests/wpt/fetch/api/` (project root,
NOT `refs/wpt/`). The `refs/wpt` path mentioned in v1 was a
typo — the actual checkout is `tests/wpt/`.

**Total `.any.js` files in `fetch/api/`: ~135** (recounted 2026-05-01).
v1 designed for 61 of those (4 directories: headers, request,
response, abort). v2 expands to also cover redirect, body, basic,
credentials so the design's "full WHATWG compat" goal is delivered.

#### Headers (11 files — coverage from headers-native)

`fetch/api/headers/`:
- `headers-basic.any.js`
- `headers-casing.any.js`
- `headers-combine.any.js`
- `headers-errors.any.js`
- `header-setcookie.any.js`
- `headers-no-cors.any.js` — **NEW pass requirement** for fetch (was
  deferred; tests the guard machinery).
- `headers-normalize.any.js`
- `headers-record.any.js`
- `headers-structure.any.js`
- `header-values.any.js`
- `header-values-normalize.any.js`

#### Request (24 files)

`fetch/api/request/`:
- `forbidden-method.any.js` — D-18.
- `request-bad-port.any.js` — D-19.
- `request-cache.js` (helper, not run directly).
- `request-cache-default.any.js`, `request-cache-default-conditional.any.js`,
  `request-cache-force-cache.any.js`, `request-cache-no-cache.any.js`,
  `request-cache-no-store.any.js`, `request-cache-only-if-cached.any.js`,
  `request-cache-reload.any.js` — these test cache-mode-driven header
  rewrites (D-14). Some assertions on response source (`from-cache` vs
  `from-net`) will FAIL because we have no cache layer; **classify as
  partial-pass** (gate on the assertion subset that's about request
  header presence, not response source).
- `request-constructor-init-body-override.any.js`.
- `request-consume.any.js`, `request-consume-empty.any.js` — body
  consumer methods.
- `request-disturbed.any.js` — D-29.
- `request-error.any.js`, `request-error.js`.
- `request-headers.any.js` — D-17 forbidden-header enforcement.
- `request-init-002.any.js`, `request-init-contenttype.any.js`,
  `request-init-priority.any.js`, `request-init-stream.any.js`.
- `request-keepalive.any.js` — keepalive is parsed-and-stored only;
  on-the-wire effects are zero. The test currently asserts shape; should pass.
- `request-structure.any.js` — IDL surface check.

Excluded:
- `forbidden-headers.any.js` referenced indirectly — not a separate file in v1 wpt.
- `request-clone.sub.html` — needs HTML harness; defer.
- `request-init-001.sub.html`, `request-init-003.sub.html` — same.
- `request-keepalive-quota.html` — keepalive quota not implemented.
- `request-reset-attributes.https.html` — needs https server harness; defer.
- `url-encoding.html` — HTML harness; defer.

Subdirectories:
- `request/destination/` — destination is no-op (D-3); skip.
- `request/multi-globals/` — multi-realm test; skip (single realm v1).

Must-pass v1: 22 of 24 .any.js files (the two cache files marked
partial-pass).

#### Response (29 files)

`fetch/api/response/`:
- `json.any.js` — `Response.json(data, init)`.
- `response-cancel-stream.any.js` — body stream cancellation.
- `response-clone.any.js` — D-8 tee semantics.
- `response-consume-empty.any.js`, `response-consume-stream.any.js` — body consumer.
- `response-error.any.js`, `response-error-from-stream.any.js`.
- `response-from-stream.any.js`.
- `response-headers-guard.any.js` — Headers guard for Response (D-17).
- `response-init-001.any.js`, `response-init-002.any.js`,
  `response-init-contenttype.any.js`.
- `response-static-error.any.js` — `Response.error()`.
- `response-static-json.any.js` — `Response.json`.
- `response-static-redirect.any.js` — `Response.redirect`.
- `response-stream-bad-chunk.any.js` — non-Uint8Array chunks in body
  stream; should error.
- `response-stream-disturbed-{1..6}.any.js` — body-disturbed checks
  across the consumer methods.
- `response-stream-disturbed-by-pipe.any.js`.
- `response-stream-disturbed-util.js` (helper).
- `response-stream-with-broken-then.any.js` — Promise-prototype
  tampering hardening (per streams-native missing-concept #1).

Excluded:
- `response-arraybuffer-realm.window.js`, `response-blob-realm.any.js`,
  `response-clone-iframe.window.js` — multi-realm; skip.
- `many-empty-chunks-crash.html`, `response-body-read-task-handling.html`,
  `response-consume.html` — HTML harness; defer.

Must-pass v1: ~24 of 29 .any.js files.

#### Abort (3 files)

`fetch/api/abort/`:
- `cache.https.any.js` — abort interaction with cache; partial-pass
  (no cache).
- `general.any.js` — main abort tests.
- `request.any.js` — Request.signal interactions.

Excluded:
- `destroyed-context.html`, `keepalive.html`,
  `serviceworker-intercepted.https.html` — HTML / SW harness.

Must-pass v1: 3 of 3.

#### Redirect (~15 files — v2 fix Process-flaw 3)

`fetch/api/redirect/`:
- `redirect-count.any.js` — D-12 20-redirect cap.
- `redirect-empty-location.any.js`.
- `redirect-keepalive.any.js`, `redirect-keepalive-mainframe.any.js` —
  keepalive on redirect; v1 ignores keepalive (D-3) so these test that
  redirect keepalive doesn't block.
- `redirect-location.any.js`, `redirect-location-noref.any.js`.
- `redirect-method.any.js` — D-12 method/body rewrite (v2 fix CRITICAL-2:
  this test exercises the 4-header `request-body-header` set; v1's
  5-entry list breaks this test by stripping Content-Length on a 301+POST
  → GET rewrite where the spec doesn't ask).
- `redirect-mode.any.js` — D-12 redirect mode handling.
- `redirect-origin.any.js` — D-12 cross-origin Authorization strip.
  **(v2 fix CRITICAL-1: this is THE key test — checks that ONLY
  Authorization is stripped on cross-origin redirect, not Cookie/Host/
  Proxy-Authorization.)**
- `redirect-referrer.any.js`, `redirect-referrer-override.any.js` —
  referrer-policy on redirect; D-3 makes these no-op observable
  (no Referer ever).
- `redirect-schemes.any.js` — http→https / etc.
- `redirect-to-dataurl.any.js` — redirect target is data: URL.
- `redirect-upload.h2.any.js` — h2-only; deferred (no h2 v1).

Excluded (HTML / SW harness): `redirect-back-to-original-origin.html`,
`redirect-302.html`, etc.

Must-pass v1: 13 of ~15 .any.js files.

#### Body (~3 files — v2 fix Process-flaw 3)

`fetch/api/body/`:
- `formdata.any.js` — body.formData() consumer; multipart parsing
  deferred per Non-goals → partial-pass (urlencoded only).
- `mime-type.any.js` — MIME parsing on body extraction.

Must-pass v1: 1 of ~3 .any.js files; 1 partial-pass.

#### Basic (~32 files — v2 fix Process-flaw 3)

`fetch/api/basic/`:
- `accept-header.any.js`, `accept-language.any.js` — D-15-adjacent.
- `block-mime-as-script.any.js`, `block-mime-as-script-2.any.js` —
  browser-only (we don't block by MIME).
- `error-after-response.any.js` — error during body stream.
- `header-value-{combining,null-byte}.any.js`.
- `historical.any.js` — superseded API checks.
- `integrity.any.js` — SRI; v1 stores but no-op (Non-goals).
- `keepalive.any.js` — keepalive parsed but ignored.
- `mode-no-cors.sub.any.js` — D-3 CORS bypass; passes under our
  semantics (no enforcement).
- `mode-same-origin.sub.any.js` — same.
- `request-forbidden-headers.any.js`, `request-headers.any.js` —
  D-17 forbidden-header enforcement.
- `request-headers-case.any.js` — header name casing.
- `response-null-body.any.js` — D-13 null-body statuses.
- `response-url.sub.any.js` — D-30 Response.url after redirect.
- `scheme-{about,blob,data}.any.js` — D-21 (data: passes; about:/blob:
  return network error).
- `simple-fetch.any.js` — basic GET/POST.
- `status.any.js` — Response.status.
- `stream-response.any.js` — body streaming.
- `text-utf8.any.js` — text() UTF-8 decoding.

Excluded (browser-specific or HTML harness): block-mime-* tests,
historical tests for removed APIs.

Must-pass v1: ~24 of ~32 .any.js files.

#### Credentials (~3 files — v2 fix Process-flaw 3)

`fetch/api/credentials/`:
- `authentication-basic.any.js` — Basic auth via URL credentials.
- `authentication-redirection.any.js` — D-12 cross-origin
  Authorization strip (v2 fix CRITICAL-1).
- `cookies.any.js` — D-3 / Non-goals: no cookie jar; passes under
  our "Cookie is forbidden header, no auto-attach" model.

Must-pass v1: 3 of 3 .any.js files.

#### Cross-cutting

- `fetch/api/idlharness.https.any.js` — IDL surface verification via
  `idl-harness`. **Must pass.**

#### Total inventory

- Headers: 11 files (already passing partially per headers-native;
  fetch adds guard-machinery test pass).
- Request: 22 files must-pass + 2 partial-pass (cache).
- Response: 24 files must-pass.
- Abort: 3 files must-pass.
- Redirect: 13 files must-pass. (NEW v2.)
- Body: 1 file must-pass + 1 partial-pass (multipart deferred). (NEW v2.)
- Basic: 24 files must-pass. (NEW v2.)
- Credentials: 3 files must-pass. (NEW v2.)
- IDL harness: 1 file.

Total: **102 files** must-pass v1 (was 61 in v1), **3 partial-pass**
(cache subset, multipart formData), ~30 files deferred (HTML /
multi-realm / service worker harness / h2-only).

### XVI.2. Hand-written tests

Lives in `crates/runtime/tests/`. Mirrors WPT but exercises corners
the WPT suite doesn't:

- `fetch_request.rs`:
  - Method validation (D-18) — case-preserve for non-standard methods.
  - URL with credentials → TypeError.
  - `Request(req2)` cloning — same body shouldn't disturb input.
- `fetch_response.rs`:
  - `Response.json` fast-path equivalence.
  - `Response.redirect(url, 200)` → RangeError (invalid redirect status).
  - Null-body status with non-null body → TypeError (D-13).
  - WebSocket-upgrade init member preserved (D-13).
- `fetch_body.rs`:
  - `extract_body(string)` produces a stream that emits 1 chunk then closes.
  - `extract_body(stream)` rejects keepalive=true.
  - `extract_body(disturbed_stream)` throws.
  - `clone()` after `text()` throws.
  - `text()` on null body returns "".
  - Streaming upload: build `fetch(url, { body: someStream, duplex: "half" })`
    and assert chunks arrive at the server in order.
- `fetch_redirects.rs`:
  - 301 + POST → GET, body stripped.
  - 302 + POST → GET, body stripped.
  - 303 + PUT → GET, body stripped.
  - 307 + POST + bytes-body → POST + body retransmitted.
  - 307 + POST + stream-body → network error (D-11).
  - 308 + same — equivalent to 307.
  - 21 redirects → network error.
  - Cross-origin redirect strips Authorization / Cookie / Host /
    Proxy-Authorization (D-12).
- `fetch_abort.rs`:
  - `signal.aborted` before fetch → reject with reason.
  - `controller.abort()` mid-fetch → reject.
  - `AbortSignal.timeout(50)` → after 50ms reject with TimeoutError.
  - `AbortSignal.any([s1, s2])`: abort either, combined aborts.
  - Aborting a body's stream propagates to upstream cyper (TCP closed).
  - Pre-aborted signal causes synchronous reject.
- `fetch_compression.rs`:
  - Server response with `Content-Encoding: gzip` → user reads decoded.
  - Server response with `Content-Encoding: gzip, br` → reverse-order
    decoding.
  - `Content-Encoding: identity` → no-op.
  - `Content-Encoding: foo` → network error (D-5).
  - Decoded bytes → `Content-Encoding` and `Content-Length` stripped from
    response.headers.
  - User-supplied `Accept-Encoding: identity` overrides default.
  - User-supplied empty `Accept-Encoding: ""` opts out.
- `fetch_data_url.rs`:
  - `fetch("data:text/plain,hello")` → 200, text/plain, body "hello".
  - `fetch("data:application/json;base64,eyJhIjoxfQ==")` → 200, json, body `{"a":1}`.
  - Invalid data URL → network error.

### XVI.3. WPT runner

Pattern from headers-native + streams-native:
`crates/runtime/tests/wpt_fetch.rs` boots a runtime, vendors WPT
fetch tests via the cargo-included `wpt/` directory, runs each via a
small harness that polyfills the WPT `testharness.js` async APIs,
asserts pass count.

Target: **100% pass on the must-pass-v1 set** (61 files), **partial
pass on the cache-mode subset** (2 files). Tracking via
`tests/wpt-fetch.expectations` (analogous to streams-native's expected
WPT result file).

The polyfill currently runs **0%** of these tests (none, because
`fetch` calls into `__rawFetch` which doesn't exist outside the
runtime — there is no harness path).

## XVII. Polyfill removal cadence (D-23)

Three landings, mirroring headers-native + streams-native (D-19):

### Landing 1 — feature-flagged native, polyfill default — **DONE**

Realised as the env-var gate `ZEROSHIP_NATIVE_FETCH=1` (not a Cargo
feature; matches streams-native's `ZEROSHIP_NATIVE_STREAMS` and
headers-native's `ZEROSHIP_NATIVE_HEADERS` patterns):

- `crates/runtime/src/web/fetch/`, `fetch_request.rs`,
  `fetch_response.rs`, `fetch_body/`, `dom/` modules all present.
- `init.rs::install_dom` (AbortController / AbortSignal /
  EventTarget / Event / FormData / Request / Response / native
  `fetch`) is gated on `ZEROSHIP_NATIVE_FETCH` — when unset, the
  JS polyfills (`embed/fetch.js`, `embed/formdata.js`,
  `embed/events.js`) load as default; when set, the native classes
  shadow them via the global re-assignment ordering documented at
  `init.rs::install_dom`.
- WPT runners (`wpt_fetch_basic.rs`, `wpt_fetch_redirect.rs`,
  `wpt_fetch_request.rs`, `wpt_fetch_response.rs`,
  `wpt_fetch_body.rs`) all set the env var inside their harness.
- Smoke tests at `tests/fetch_native_install.rs` (4 tests) confirm
  `globalThis.fetch` is the native callback under the flag.

This commit (landing 1) only documents the existing state; no code
changes. It marks the implementation contract as complete enough to
flip the default in landing 2.

### Landing 2 — flip default to native

- Default-enable `runtime_native_fetch`.
- `init.rs` no longer loads the polyfill.
- `crates/runtime/src/transport/handler.rs::HTTP_CREATE_REQUEST_JS` is removed
  (not needed; native Request constructor is direct).
- `crates/runtime/src/transport/handler.rs::looks_like_response` is rewritten to
  read ResponseState directly via internal field 0 (no priv-sym
  probe; the macro-emitted unwrap is the only path).
- `crates/runtime/src/transport/handler.rs::extract_response_headers` deleted
  (replaced by direct HeaderList read from Response state).
- The polyfill at `embed/fetch.js` remains as a fallback (still
  loaded in non-default-feature builds for emergency rollback).

CI: full test suite passes with native default; legacy polyfill still
behind feature flag for emergency rollback.

### Landing 3 — delete polyfill

- `embed/fetch.js` deleted.
- The non-default feature flag and its branches removed.

This is the same cadence as streams-native D-19. Done in three
separate PRs over (industry estimate) 3-5 weeks; (agent-pace) 3-5
hours of focused work.

## XVIII. Implementation sequence

### XVIII.1. Order

<!-- v2 fix (MAJOR-38): streams-native is partially shipped. RS+Default
Controller+DefaultReader landed; WS+TS pending. Body skeleton can begin
in parallel with streams' WS+TS work — only the final consumer wiring
needs streams' RS reader, which has shipped. -->

**Streams-native dependency status (2026-05-01):**
- `ReadableStream` + `ReadableStreamDefaultController` + `ReadableStreamDefaultReader`: **shipped**.
  Fetch's body consumers (`text()`, `json()`, etc.) and `Response.body`
  construction can begin immediately.
- `WritableStream` + `WritableStreamDefaultWriter`: pending.
- `TransformStream` + `TransformStreamDefaultController`: pending.

Fetch's hard dependencies on streams-native:
- ✅ `ReadableStream::from_native_source` — needed for response bodies.
- ✅ `acquire_default_reader` / `reader_read` / `release_reader` —
  needed for body consumers AND for streaming uploads
  (`drive_readable_stream` consumes these).
- ✅ `tee()` — needed for `clone()`.
- ✅ `is_disturbed_obj` / `is_locked_obj` — needed for `extract_body`
  guards.
- (none of WritableStream, TransformStream, or pipe_native_internal
  is required by v1 — fetch builds its codec chain as a single
  composite `NativeSource`, not as separate streams piped together,
  matching workerd's response-body pattern.)

**Soft dependency:** if compression-streams-native installs a
TransformStream-based codec chain (rather than NativeSource-chained
codecs), TransformStream + pipe_native_internal would be required.
Per compression-streams-native's design, it uses NativeSource
chaining (compatible with streams' RS-only shipped surface), so
this soft dep does not block fetch.

The implementation order:

1. **EventTarget + Event base classes** in `crates/runtime/src/web/dom/`.
   ~430 LOC + ~120 LOC. *(v2 fix MAJOR-41: location is `dom/`, not
   `fetch/`.)* Macro extension XIV.1 (`#[v8_inherit]`) lands here.
   Tests: `addEventListener` / `removeEventListener` / `dispatchEvent`
   round-trip. *(v2 fix NIT-54: ~430 LOC reflects the spec-faithful
   scope (workerd `events.c++` is 890 LOC; we're skipping captures-only
   which saves ~50%); the v1 estimate of 200 LOC was a stub.)*
2. **AbortSignal + AbortController.** ~350 + ~80 LOC. Sits on
   EventTarget (in `dom/`). Tests: aborted-flag transitions,
   listener firing, timeout (with strong-ref retention),
   any() (with bidirectional source/dependent flattening).
3. **Body Rust trait + `extract_body` skeleton.** ~150 LOC + ~250 LOC.
   The `body` / `bodyUsed` getters, the consumer methods (text /
   json / arrayBuffer / bytes / blob / formData). Layered on
   streams-native's RS reader (shipped). *(v2 fix Process-flaw 8:
   Body is a Rust trait, NOT a V8 base class — this step ships the
   shared trait `BodyOps` and per-Request/Response method delegation,
   not a `Body` JS type.)* Can begin in parallel with steps 1-2.
4. **Request class (without the constructor).** Storage struct,
   getters for all IDL fields, `clone()` method skeleton (delegates
   to body-clone). ~600 LOC.
5. **Response class (without the constructor).** Storage struct,
   getters, `clone()`, `Response.error()`, `Response.redirect()`,
   `Response.json()`. ~480 LOC.
6. **FormData class (URLSearchParams-only mode).** ~250 LOC.
7. **Headers guard machinery.** Extension to existing `headers.rs`.
   ~200 LOC.
8. **`extract_body` algorithm.** Owns the bytes ↔ stream coexistence.
   ~200 LOC.
9. **Request constructor.** Steps 1-43 of §5.4. Wires up extract_body.
   ~300 LOC.
10. **Response constructor + static methods.** ~150 LOC.
11. **`fetch()` global function callback** + `mainFetch` skeleton.
    ~200 LOC.
12. **`schemeFetch` + `data:` URL handler.** ~200 LOC.
13. **`httpFetch` + `httpNetworkOrCacheFetch` + `httpNetworkFetch`.**
    The large algorithms; wires to the existing cyper-based send path.
    ~600 LOC.
14. **`httpRedirectFetch`.** ~200 LOC.
15. **`finalize_response_body`** with codec chain integration. ~150 LOC.
16. **Streaming upload path (drive_readable_stream).** ~120 LOC.
17. **WPT runner.** ~250 LOC of harness + vendored tests.
18. **Iteration to 100% must-pass-v1.** Variable; the streams-native
    estimate was 32h (industry), and fetch is comparable in scope.

### XVIII.2. Hours

Industry estimate / agent-pace estimate (per `feedback_estimates_hours_not_weeks`
the agent-pace is roughly industry-hours / 40):

| Step | Industry h | Agent-pace h |
|------|-----------:|-------------:|
| 1. EventTarget + Event | 12 | 0.3 |
| 2. AbortSignal + AbortController | 10 | 0.25 |
| 3. Body skeleton + consumer methods | 14 | 0.35 |
| 4. Request class | 18 | 0.45 |
| 5. Response class | 14 | 0.35 |
| 6. FormData | 8 | 0.2 |
| 7. Headers guard machinery | 8 | 0.2 |
| 8. extract_body | 10 | 0.25 |
| 9. Request constructor | 14 | 0.35 |
| 10. Response constructor + static methods | 6 | 0.15 |
| 11. fetch() + mainFetch skeleton | 8 | 0.2 |
| 12. schemeFetch + data: | 6 | 0.15 |
| 13. httpFetch + httpNetworkOrCacheFetch + httpNetworkFetch | 24 | 0.6 |
| 14. httpRedirectFetch | 8 | 0.2 |
| 15. Codec chain integration | 6 | 0.15 |
| 16. Streaming upload | 8 | 0.2 |
| 17. WPT runner skeleton | 10 | 0.25 |
| 18. WPT iteration to must-pass | 32 | 0.8 |
| Macro extensions (XIV) | 12.5 | 0.3 |
| Polyfill removal landings | 4 | 0.1 |
| **Total** | **232.5h** | **~5.85h** |

For ADR provenance: ~232 industry-hours, comparable to streams-native's
estimated total. This includes WPT iteration which historically eats
~14% of the total budget.

## XIX. Comparison with reference implementations

| Project | LOC | Native or JS | Body model | Notes |
|---------|----:|--------------|------------|-------|
| **undici** (Node 22 fetch) | ~8,500 (lib/web/fetch) | Pure JS | Stream-of-Uint8Array | Most spec-faithful production impl. Spec comments verbatim. Wraps `node:net` socket, has its own parser; no hyper. We reuse spec algorithms but use cyper for transport. |
| **workerd** (Cloudflare) | ~3,750 (api/http.{c++,h}) | Pure native (C++) | Stream wrapping kj::AsyncInputStream | No CORS, no service workers (D-3 origin). Body source/buffer pattern (workerd `http.h:67-99`) is the model for our `BodySource` enum. |
| **Deno** (`ext/fetch/`) | ~3,900 JS + ~1,560 Rust = 5,460 LOC | Hybrid (JS dispatch, Rust transport via hyper) | Stream-rid (resource id) | Uses tokio + hyper. NOT compatible with our zero-tokio invariant; we use cyper (compio + hyper) instead. The IDL+algorithm split (JS owns IDL+algorithms; Rust owns network) is similar to our split (Rust owns everything but the V8 IDL bridging — which is handled by `#[v8_class]`). |
| **Current polyfill** (`embed/fetch.js`) | 705 LOC | Pure JS over `__rawFetch` Rust shim | String body (broken for streams) | The path being replaced. |
| **This design (Rust)** | ~3,400 LOC native + ~950 LOC macro = ~4,350 LOC | Pure native | Stream + source (workerd model) | Includes EventTarget, AbortSignal, Event, FormData. Closer to workerd in scope; smaller than undici because we ship fewer features (no CORS, no SW, deferred multipart parser). |

The native implementation comes in at roughly **half the LOC of
undici** and **comparable to workerd**. The reduction vs. undici comes
from:

- Skipping CORS (~600 LOC of CORS check, preflight, header filtering).
- Skipping service-workers (~400 LOC).
- Skipping multipart parser (deferred; ~600 LOC).
- Skipping cache layer (~400 LOC).
- Skipping all the Node-compat layers (Buffer, EventEmitter shims).

## XX. Open questions

These are policy-level decisions where reasonable people might disagree.
None blocks v1 design completion.

### XX.0. Cross-design contracts (v2 fix — explicit subsection)

<!-- v2 fix: explicit cross-design contracts to prevent the kind of
unilateral repudiation that v1 did with compression's hook. -->

This design depends on, OR is depended on by, the sibling designs in
`docs/proposals/`. The following contracts are jointly committed:

**Contracts fetch OWNS (we ship the API; consumers register against it):**

| API | Spec | Section | Consumer |
|-----|------|---------|----------|
| `ResponseBuilder::with_response_body_hook(hook: ResponseBodyHook)` | Compression-streams' design §"From the native-fetch project" | §V.9.0 | `compression-streams-native` registers its codec-chain hook |

**Contracts fetch CONSUMES (sibling ships the API; we register / call):**

| API | Spec source | Status | Section |
|-----|-------------|--------|---------|
| `ReadableStream::from_native_source(scope, source, strategy)` | streams-native (verified at line 255-259 of streams-native.md) | Shipped | §V.9, §VII |
| `tee()` (public method on RS) | streams-native D-11 | Shipped | §III.5 |
| `is_disturbed_obj` / `is_locked_obj` | streams-native | Shipped | §III |
| Rust-side `acquire_default_reader` / `reader_read` / `release_reader` | streams-native (Rust API exposed alongside the JS getReader()) | Shipped | §III.4.1, §VI.3 |
| `pipe_native_internal` | streams-native (verified at streams-native:2148) | Available; NOT used in v1 | n/a |
| `WritableStream::from_native_sink` | streams-native | Pending; NOT used in v1 | n/a |

**NOT a contract — fetch-internal helpers (must NOT appear in
streams-native's public API):**

| Helper | Lives in | Why fetch-internal |
|--------|----------|--------------------|
| `read_all_bytes(scope, stream) -> Result<Vec<u8>, ...>` | `crates/runtime/src/fetch/body_stream.rs` | Body consumer accumulator. Layered on streams' RS reader. NOT a streams-native deliverable (v2 fix CRITICAL-13). |
| `read_one_chunk(scope, stream) -> Option<Result<Vec<u8>, ...>>` | `crates/runtime/src/fetch/body_stream.rs` | Streaming-upload chunk pump. Layered on streams' RS reader. NOT a streams-native deliverable (v2 fix CRITICAL-13). |

If compression-streams' design changes shape (e.g. moves from
NativeSource chaining to TransformStream-based chaining), the
`with_response_body_hook` contract still holds — only the hook
implementation moves.

### XX.1. Should we eventually ship CORS as opt-in?

The platform's gateway already handles CORS for incoming user requests
(creator app's HTTP server side). The `fetch()` calls inside creator
code are purely outbound, server-side. Browser-style CORS doesn't apply.

But: if a creator app embeds *user-submitted* JS that does fetch calls,
we MIGHT want to enforce that user JS can't fetch internal endpoints.
That's the SSRF guard's job (D-24), not CORS — and we already have it.

**Decision:** stay D-3 forever. Document that creators don't worry
about CORS. If a creator app wants CORS-style isolation, it builds a
proxy in user-space. Move "CORS in v2" to "won't fix" in v1.5.

### XX.2. HTTP cache layer — when?

Without one, `request.cache` modes don't differ behaviourally except
in header rewrites. WPT cache tests partial-pass. Real-world creator
apps rarely use `cache: "force-cache"` (no benefit if no cache); they
ARE hurt by `cache: "no-cache"` adding `Pragma: no-cache` to every
request (some upstream APIs reject Pragma).

**Decision:** ship without cache in v1. Defer cache layer to v2. Track
WPT partial-pass count; if any creator app needs `force-cache`, it
gets a no-op response (cache miss → network). The Cache API itself
(`caches.open`, `Cache.match`, `Cache.put`) is a separate spec —
shipping that is a different ADR.

### XX.3. Cookie jar — never?

The runtime is server-side. There's no per-end-user persistent cookie
store (the platform's auth state is JWT-cookie + session, and creator
code reads those via `env.auth.*` primitives, not via fetch).
But `Cookie` IS a forbidden request header (the user can't manually
set it on outbound fetch); they CAN read `Set-Cookie` from response
headers (which is a Headers concern, not Fetch).

**Decision:** never ship a default cookie jar. Creator code that
needs to forward cookies between two upstream services builds
intermediate state in user-space. Document explicitly.

### XX.4. HTTP/2 — when?

<!-- v2 fix (CRITICAL-15/16, NIT-60): cyper's actual feature list is
`["rustls", "json", "stream"]`, NOT `["client", "http1"]`. -->

cyper's pinned feature list (workspace `Cargo.toml` line **25**, not
33): `default-features = false, features = ["rustls", "json", "stream"]`.
There is no `http2` feature; cyper's HTTP/2 transport (if cyper ships
it) would be a separate feature toggle. v1 of this design is
HTTP/1.1-only because cyper-as-built is HTTP/1.1-only. Enabling HTTP/2
in v2 requires:

- A `cyper` feature flag for h2 (or upstreaming the integration).
- HPACK header decoding (which cyper would forward to `h2`).
- Stream multiplexing (changes the cyper Client connection-pool
  semantics).
- Prior-knowledge vs. ALPN negotiation (TLS).

**Decision:** v1 HTTP/1.1 only. The fetch design is HTTP-version-
agnostic at the spec layer; the only impact is the
`Transfer-Encoding: chunked` rule (§5.5 step 9.3) becoming
HTTP/1.1-only. Re-test the streaming-upload path against an
HTTP/2 server when v2 lands.

### XX.5. Multipart parser — when?

The `formData()` consumer throws TypeError("Multipart formData parsing
not implemented") on multipart bodies in v1. WPT
`response/response-from-stream.any.js` and others may exercise this
indirectly.

**Decision:** defer to v2 unless a creator app surfaces a need. Track
in a follow-up issue with the test list.

### XX.6. Feature flag name

Use `runtime_native_fetch` (matches `runtime_native_streams` and
`runtime_native_headers` from sibling designs). Single-flag-flip
landing.

### XX.7. Async iterable bodies (`fetch(url, { body: asyncIterable })`)

Workerd ships this; the spec PR is open at
https://github.com/whatwg/fetch/pull/1646. Native ships it (per the
asyncIterable branch in `extract_body` §III.2) because creator code
generates streaming bodies via async generators in practice (LLM token
streams, log streams, partial uploads).

**Decision:** keep. Document as a workerd-and-modern-runtimes
extension.

## XXI. Revision history

- **v1 (2026-05-01)** — Initial design covering the full WHATWG Fetch
  surface. Replaces the JS polyfill at `crates/runtime/src/embed/fetch.js`
  (705 LOC) and the JSON-marshalling shim at
  `crates/runtime/src/transport/handler.rs` (386 LOC), preserves the in-tree cyper
  integration at `crates/runtime/src/transport/ssrf.rs` (existing SSRF guard,
  thread-local Client, fetch concurrency cap). Designed pure-native on
  V8 + Rust + cyper, zero tokio.

  Goes alongside the in-flight streams-native v2 and
  compression-streams-native r2 designs — the three together fully
  replace the JS-heavy fetch path.

  Decisions D-1 through D-30 cover: pure-native classes (D-1); the
  body-as-stream + body-source coexistence model (D-2, D-11); CORS
  bypass with IDL retention (D-3); compio-native I/O (D-4);
  decompression integration (D-5, D-15); streaming upload (D-6);
  body consumers (D-7); spec-faithful clone (D-8); native AbortSignal /
  EventTarget (D-9, D-10); rewindable redirects (D-11); spec-faithful
  redirect rewriting in Rust (D-12); WebSocket-upgrade extension
  (D-13); cache-mode header rewrites without a cache layer (D-14);
  Origin header (D-16); forbidden-header enforcement (D-17); method
  validation (D-18); bad-port blocklist (D-19); spec algorithm naming
  (D-20); data: URL support (D-21); JSON dispatch elimination (D-22);
  three-landing polyfill removal (D-23); SSRF preservation (D-24);
  concurrency caps (D-25); response size cap (D-26); ignored referrer
  policy (D-27); SSE-by-default streaming (D-28); synchronous body-init
  errors (D-29); Response.url semantics (D-30).

- **v2 (2026-05-01)** — Round-2 critic-driven revision. See the v2
  entry near the top of this document (after the **Status:** banner
  and the **Depends on:** block). Summary of the changes by
  C-N / MAJOR-N number:

  Critical fixes applied:
  - C-1 (cross-origin Authorization-only strip)
  - C-2 (request-body-header is 4 entries)
  - C-3 (hyper 1.x via http_body_util)
  - C-4 (83 spec ports, sourced from spec)
  - C-5 (RequestMode 4 IDL values)
  - C-6 (RequestDestination 22 entries)
  - C-7 (signal abort algorithm collects deps first)
  - C-8 (source signals ↔ dependent signals bidirectional)
  - C-9 (AbortSignal.timeout strong retention while listeners)
  - C-10 (extract_body dispatch order with explicit predicates)
  - C-11 (USVString conversion for string body)
  - C-12 (with_response_body_hook contract restored)
  - C-13 (read_all_bytes / read_one_chunk are fetch-internal)
  - C-15 / C-16 (cyper actual features, line 25)
  - C-17 (with_isolate_lock removed; actual single-thread pattern)
  - C-20 (data-url crate added as workspace dep)

  Major fixes applied:
  - MAJOR-21 (X-HTTP-Method conditional forbidden)
  - MAJOR-22 / MAJOR-23 (Origin header method exclusion + value derivation
    + cors-mode / websocket-mode trigger)
  - MAJOR-25 (consumer error types: SyntaxError / RangeError / infallible)
  - MAJOR-26 (arrayBuffer copy via new_backing_store_from_vec)
  - MAJOR-29 (use_url_credentials slot label)
  - MAJOR-30 (workerd compat-flag is default-on for compat dates)
  - MAJOR-31 (Accept-Encoding ordering: br,gzip,deflate single canonical)
  - MAJOR-38 (streams-native partial ship; fetch parallel-development)
  - MAJOR-40 (fire abort event AFTER abort steps via EventTarget)
  - MAJOR-41 (EventTarget moved to crates/runtime/src/web/dom/)

  Process flaws fixed:
  - D-21 verification resolved (data-url crate added).
  - WPT inventory recounted: 102 must-pass v1 (was 61).
  - Body mixin is a Rust trait (not V8 base class).

  Items deferred / kept as-is:
  - Citation audit (workerd / undici line refs): per critic's
    request, NOT individually re-verified. Footnote at end of doc
    pins refs to design-time commit; refs may drift over time.
  - WPT inventory: redirect/, body/, basic/, credentials/ added
    to v1 must-pass set.
  - Hour estimates: kept as v1 (per-step estimates retained).

## XXII. Sources

- WHATWG Fetch Standard — https://fetch.spec.whatwg.org/
- Fetch spec source — https://github.com/whatwg/fetch/blob/main/fetch.bs
- WebIDL Standard — https://webidl.spec.whatwg.org/
- DOM Standard (EventTarget, AbortController, AbortSignal) — https://dom.spec.whatwg.org/
- WHATWG URL — https://url.spec.whatwg.org/
- File API (Blob, FormData) — https://w3c.github.io/FileAPI/
- HTML Living Standard (FormData) — https://html.spec.whatwg.org/
- RFC 9110 — HTTP Semantics — https://www.rfc-editor.org/rfc/rfc9110
- RFC 9111 — HTTP Caching — https://www.rfc-editor.org/rfc/rfc9111
- RFC 9112 — HTTP/1.1 — https://www.rfc-editor.org/rfc/rfc9112
- RFC 7578 — multipart/form-data — https://www.rfc-editor.org/rfc/rfc7578
- RFC 6265 — HTTP State Management Mechanism (Cookies) — https://www.rfc-editor.org/rfc/rfc6265
- undici (Node fetch) — https://github.com/nodejs/undici/tree/main/lib/web/fetch
- workerd (Cloudflare) — https://github.com/cloudflare/workerd/tree/main/src/workerd/api/http.{c++,h}
- Deno fetch — https://github.com/denoland/deno/tree/main/ext/fetch
- cyper (compio + hyper HTTP client) — https://github.com/compio-rs/cyper
- compio — https://github.com/compio-rs/compio
- WPT fetch tests — https://github.com/web-platform-tests/wpt/tree/main/fetch/api  <!-- v2 fix NIT-57: branch master → main -->
- `data-url` crate (Servo's data: URL parser) — https://crates.io/crates/data-url  <!-- v2 fix CRITICAL-20: D-21 dependency -->
- Sibling designs:
  - `docs/proposals/streams-native.md`
  - `docs/proposals/headers-native.md`
  - `docs/proposals/compression-streams-native.md`
- Project AGENTS.md — `/home/ruiyang/Projects/appbase/AGENTS.md`
- Existing polyfill: `crates/runtime/src/embed/fetch.js` (705 LOC)
- Existing dispatch shim: `crates/runtime/src/transport/handler.rs` (386 LOC)
- Existing cyper bind: `crates/runtime/src/transport/ssrf.rs` (852 LOC; the
  cyper transport, SSRF guard, fetch concurrency cap stay)
- Native classes already shipped: `crates/runtime/src/web/headers.rs`,
  `crates/runtime/src/web/encoding.rs`, `crates/runtime/src/web/codec.rs`
- Macro internals: `crates/runtime-macros/src/v8_class.rs`,
  `crates/runtime-macros/src/lib.rs`

---

## Footnote on line citations

<!-- v2 fix (per critic's request): pin file:line refs to design time. -->

Line references throughout this document to:

- `refs/workerd/src/workerd/api/http.{c++,h}`
- `refs/workerd/src/workerd/io/compatibility-date.capnp`
- `refs/undici/lib/web/fetch/{constants.js,index.js,data-url.js}`
- `refs/deno/ext/fetch/*` and `refs/deno/runtime/js/*`

are pinned to the **commits checked into `refs/`** at design time
(2026-05-01). Upstream code may drift over time; the line numbers
should be considered illustrative — they were valid at design time
but may have moved by the time the implementation lands. The
critic's review (round 1) flagged several off-by-N citations
(workerd `http.h:765-773` is actually `760-773`; workerd
`http.h:144-150` is actually `144`; undici `index.js:1336-1346` and
`index.js:1547-1553` are unverified). For the v2 round we elected
not to re-pin every citation individually — that's a maintenance
cost without correctness benefit.

What IS load-bearing in this design:
1. The **algorithms** (which closely follow the WHATWG Fetch and DOM
   specs, citation: the spec sections themselves at fetch.spec.whatwg.org
   and dom.spec.whatwg.org).
2. The **shape** of the workerd/undici/deno reference implementations
   (general patterns: body-source vs body-stream, abort-on-cancel,
   Authorization-only-strip, etc.) which we mirror without depending
   on any specific line.

What ISN'T load-bearing: the exact line numbers in `refs/`.
Implementers cross-checking should grep for the function names
(`canRewindBody`, `rewindBody`, `applyAuthorizationStripping`,
`badPorts`, `requestBodyHeader`) rather than navigating to specific
line numbers.
