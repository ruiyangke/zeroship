# Native WHATWG WebSocket design

**Date:** 2026-05-02 (v1) · 2026-05-02 (v2 — post-review revision)
**Status:** Draft v2 (post-review) — implementation pending
**Spec:** WHATWG WebSockets Standard — https://websockets.spec.whatwg.org/
**Spec source:** https://github.com/whatwg/websockets/blob/main/index.bs
**Wire protocol:** RFC 6455 — https://datatracker.ietf.org/doc/html/rfc6455
**WebSocketStream draft (deferred):** https://github.com/ricea/websocketstream-explainer
**Reference impls:**
  - **undici** (Node 22 WebSocket — pure JS, the most spec-faithful production impl) —
    https://github.com/nodejs/undici/tree/main/lib/web/websocket
  - **workerd** (pure-native C++, Cloudflare Workers — production) —
    https://github.com/cloudflare/workerd/tree/main/src/workerd/api (`web-socket.{c++,h}`,
    `hibernatable-web-socket.{c++,h}`)
  - **Deno** (Rust + JS hybrid; uses `fastwebsockets`) —
    https://github.com/denoland/deno/tree/main/ext/websocket
**Tests:** WPT `websockets/` — https://github.com/web-platform-tests/wpt/tree/master/websockets
**Related specs:** DOM §2 (Event/EventTarget — both shipped),
  DOM §3.3 (AbortSignal — shipped); URL Standard §4.5 (special-scheme parser);
  HTML §11 (Origin); WebIDL §3.1 (DOMString / USVString / ByteString /
  EventHandler typedef); RFC 9110 §7.8 (HTTP Upgrade).
**WebIDL:** https://webidl.spec.whatwg.org/

## Changes from v1 (post-review)

The v1 draft scored 56/100 with 10 CRITICAL spec violations (per the
critic review at `/tmp/zeroship-reviews/websocket-review.md`). v2
addresses every CRITICAL and every MAJOR. The substantive deltas:

1. **send() type-dispatch order corrected to spec.** WHATWG §3.1 lists
   String → Blob → ArrayBuffer → ArrayBufferView in that order
   (https://websockets.spec.whatwg.org/#dom-websocket-send, send algorithm
   steps 3-6). v1 inverted this and claimed the spec listed string last;
   v2 re-orders the test predicates and removes the bogus rationale.
   (CRITICAL #1)
2. **`[Clamp]` conversion replaced with the actual WebIDL algorithm.**
   v1's `code.uint32_value(scope).unwrap_or(0).min(65535)` is not a
   `[Clamp]` conversion — WebIDL §3.2.5 (per
   https://webidl.spec.whatwg.org/#abstract-opdef-converttoint, Clamp
   case) requires NaN → 0, sign-aware clamp to `[0, 65535]`, then
   round-half-to-even. v2 uses an explicit `clamp_unsigned_short`
   helper. (CRITICAL #2)
3. **Origin header is no longer sent unconditionally.** RFC 6455 §10.2
   (https://datatracker.ietf.org/doc/html/rfc6455#section-10.2) says
   non-browser clients SHOULD NOT send Origin. v2 sends Origin only when
   the user explicitly sets it via the `init` dict, matching undici and
   workerd defaults. (CRITICAL #3)
4. **CONNECTING vs OPEN close() paths split.** Per WHATWG §3.1 close
   algorithm and RFC 6455 §7.1.7, a CONNECTING-state close must "fail
   the WebSocket connection" (no wire frame; no socket may even exist),
   not enqueue a Close frame. v2 distinguishes the two cases in §V.5.
   (CRITICAL #4)
5. **AbortSignal-during-CONNECTING fires `error` THEN `close`.** Per
   WHATWG §4 "feedback from the protocol"
   (https://websockets.spec.whatwg.org/#feedback-from-the-protocol,
   "if the connection-failed state is reached"), every connection-failed
   path fires Error then Close. v1 emitted only Close on Aborted; v2
   restores the spec-mandated pair. (CRITICAL #5)
6. **Close frame default no longer encodes 1005.** Per RFC 6455 §7.4.1
   (https://datatracker.ietf.org/doc/html/rfc6455#section-7.4.1), 1005
   is reserved as an internal sentinel and MUST NOT appear in a Close
   control frame. v2's send-pump emits an empty-payload Close frame when
   the user calls `close()` with no code argument. (CRITICAL #6)
7. **Sec-WebSocket-Extensions response is now strictly validated.** Per
   RFC 6455 §9.1
   (https://datatracker.ietf.org/doc/html/rfc6455#section-9.1), any
   extension in the response that the client did not request MUST fail
   the connection. v2 advertises an empty extension set, and any
   non-empty Sec-WebSocket-Extensions response now hard-fails the
   handshake. (CRITICAL #7)
8. **Two-phase SSRF resolution for the WS path.** v1 only checked the
   URL string (catching IP-literal hostnames). DNS rebinding attacks
   bypass that. v2 mirrors fetch's resolve-then-revalidate-then-bind
   pattern from `crates/runtime/src/fetch.rs:36-163`. (CRITICAL #8)
9. **Sec-WebSocket-Protocol response is validated against the offered
   set.** Per RFC 6455 §4.1 step 6 of the response checks
   (https://datatracker.ietf.org/doc/html/rfc6455#section-4.1), the
   client MUST fail the connection if the server echoes a subprotocol
   the client did not offer. v2 adds a defence-in-depth check after
   tungstenite's own validator. (CRITICAL #9)
10. **D-23 budget overflow now uniformly async-fails.** v1 contradicted
    itself: the Decisions table said "throws RangeError" while §V.3
    used an async error+close dispatch. v2 commits to async-fail
    (matches WHATWG §3.1's "establish in parallel" framing — failures
    surface as queued connection-failed events) and updates D-23
    accordingly. (CRITICAL #10)

The MAJOR fixes — receive-side backpressure, dead `[[full]]` flag,
EventHandler null-coercion, RFC 6455 §7.1.1 5-second close-handshake
timeout, ping-keepalive interval, MessageEvent.ports identity caching,
make_disappear close-code semantics, max_message_size pinning,
WebSocketPair bufferedAmount decrement, send-pump error reporting,
WsCachedHandles re-dispatch race, AbortSignal reason propagation,
permessage-deflate Cargo feature lockdown, CloseEventInit code
conversion, receive_loop use-after-move, accept() prototype hygiene —
are documented inline at their respective sections.

The MINOR pseudocode hygiene items (set_integrity_level return value,
echo-server endpoint enumeration, hours estimate refresh, ADR
spec-citation structure) are addressed inline where they appear.

The "Missing concepts" the critic flagged — per-app WebSocket metering,
worker-shutdown drain protocol, sticky routing for outbound `new
WebSocket(url)` on multi-node, TLS root-trust + ALPN config,
WebSocketStream-deprecated-form enumeration — get explicit Open
Questions in §XVII (XVII.11 through XVII.16).

**Score target after v2 revision:** ≥80 (production-ready). Remaining
items in §XVII are policy choices, not spec defects.

**Depends on:**
- `docs/proposals/streams-native.md` — landed RS+RSDefaultController+RSDefaultReader
  used implicitly via `Blob.stream()` for `binaryType="blob"` consumers; the
  WebSocket itself is NOT exposed as a stream in v1 (WebSocketStream API is
  out of scope — see Non-goals). No new stream-native dependency beyond
  what `Blob.stream()` already pulls in.
- `docs/proposals/fetch-native.md` — the `Headers` IDL surface (already
  shipped via headers-native), `Origin`/`Referer` machinery (already shipped
  for fetch), and the cyper HTTP/1.1 transport (used by both fetch and
  WebSocket handshake). The WebSocket connection-establish algorithm
  (§4.1) IS a fetch — undici literally calls `fetching(request)` to issue
  the HTTP/1.1 GET — but the runtime takes a more direct path (see §V) to
  avoid pulling the full main-fetch / redirect / decompression chain into
  the upgrade hot path.
- `crates/runtime/src/dom/event_target.rs` — landed; `WebSocket : EventTarget`
  reuses the same `#[v8_inherit(EventTarget)]` macro that AbortSignal uses
  (DOM §3.3 / fetch-native.md §X). Same dispatch pipeline, same listener
  storage model.
- `crates/runtime/src/dom/event.rs` — landed; CloseEvent and MessageEvent
  inherit Event via the same `#[v8_inherit(Event)]` pattern that
  CustomEvent uses (`dom/custom_event.rs`). `#[repr(C)]` with `Event` as
  the first field, layout-compatible cast to `*mut Event` for inherited
  getters.
- `crates/runtime/src/dom/abort_signal.rs` — landed; `WebSocket(url, { signal })`
  is a workerd extension we ship in v1 (see Non-goals discussion +
  Open questions). The signal-cancellation hook reuses `add_abort_algorithm`
  from `dom::abort_signal`.
- DOMException (in flight via fetch-js-delete agent) — `InvalidAccessError`
  for close-code validation, `SyntaxError` for protocol/URL validation,
  `InvalidStateError` for send-before-open. Until DOMException ships
  natively, the existing JS DOMException shim (in `embed/fetch.js`) is
  reused.
- compio + cyper (`cyper = { workspace = true }`, currently
  HTTP/1.1-only with `default-features = false, features = ["rustls", "json", "stream"]`
  per the workspace `Cargo.toml`). The WebSocket client handshake
  reuses cyper's `Client` for DNS + TLS + HTTP/1.1 GET; once the server
  returns 101 we hand the upgraded TCP/TLS stream off to the WebSocket
  framer. **The `compio-ws 0.3.1` crate IS already pulled in transitively
  via the `compio` umbrella crate** (verified `Cargo.lock:608`), with a
  `tungstenite 0.28` framer underneath. v1 uses `compio-ws::WebSocketStream`
  directly for client-side framing rather than reimplementing RFC 6455
  in-tree; the RFC 6455 framing lives outside the polyfill cutover scope
  (see §VII.1).
- sha1 (`sha1 = "0.10"`, already in workspace `Cargo.toml`) — used to
  compute the `Sec-WebSocket-Accept` server-response validator
  (RFC 6455 §4.1 step 6 of the response checks).

**Unblocks:**
- Deletion of the 166-LOC JS polyfill at `crates/runtime/src/embed/websocket.js`
  and the 5 thin `__ws*` callbacks at `crates/runtime/src/websocket.rs`
  (the JS class surface + the per-message JS-Rust hop).
- Native MessageEvent + CloseEvent classes — currently the polyfill builds
  these as plain Event objects with expandos (`makeMessageEvent`,
  `makeCloseEvent` in `embed/websocket.js:27-41`). Native MessageEvent
  is also the IDL-correct event type for a future EventSource impl —
  same `data` / `origin` / `lastEventId` shape per HTML §9.4.2.
- `new WebSocket(url)` client-side connections — currently the polyfill
  ONLY supports `WebSocketPair` (server-side, workerd extension).
  Client-side `new WebSocket("wss://api.example.com/ws")` is a hard
  prerequisite for AI-builder reliability (every realtime feature in
  every modern app uses it: Supabase realtime, Pusher, Ably, Stripe
  Connect Webhooks, OpenAI Realtime API, etc.).
- WPT regression for `websockets/` — currently we run ZERO of those
  files because the polyfill is incomplete (no client-side, no `binaryType`
  semantics, no proper `bufferedAmount`).
- Future WebSocketStream API (deferred to v2). The native shape of
  `WebSocket` here is the foundation for that.

## Top matter

### Goals

1. **Full WHATWG WebSockets compliance.** Every interface in
   https://websockets.spec.whatwg.org/ at parity with the spec — no
   "v1 subset". Pass the entire WPT `websockets/` suite minus the
   genuinely-out-of-scope items (back-forward cache, mixed-content
   from a navigated document, multi-globals across realm-transferred
   ports — see Non-goals). For client-side functional WPT cases we
   ship an in-process echo server (modelled on
   `crates/runtime/src/echo_server.rs`) so the constructor-success
   tests can run in CI.
2. **Replace the JS polyfill.** Delete `crates/runtime/src/embed/websocket.js`
   (166 LOC) and the five `__ws*` callbacks in `crates/runtime/src/websocket.rs`
   in three landings: (1) ship native behind feature flag, polyfill remains
   default; (2) flip default to native, polyfill remains as fallback;
   (3) delete polyfill. Same cadence as streams-native (D-19) and
   fetch-native (D-23).
3. **Native MessageEvent and CloseEvent.** Both are IDL classes with
   their own internal-field layout, sharing Event via `#[v8_inherit(Event)]`
   (the same pattern CustomEvent uses today). The polyfill's "build a
   plain Event with expandos" pattern is replaced. `new MessageEvent(...)`
   and `new CloseEvent(...)` from JS produce real instances that pass
   `instanceof MessageEvent` / `instanceof CloseEvent` AND `instanceof Event`.
4. **Client-side WebSocket.** `new WebSocket(url, protocols)` opens a real
   TCP/TLS connection to `url`, performs the RFC 6455 handshake (with
   our SSRF resolver from `crates/runtime/src/fetch.rs:36-163`), and
   begins reading frames. Frames arrive as MessageEvent dispatches on
   the user's listeners.
5. **Server-side WebSocket (preserve `WebSocketPair`).** The Cloudflare-
   Workers-style `new WebSocketPair()` + `Response { status: 101, webSocket: client }`
   pattern is retained verbatim. The native WebSocket class is
   construction-mode-aware (see §IV.1) — both directions land on the
   same `#[v8_class] impl WebSocket` block.
6. **Compio-native async, zero tokio.** All network work runs through
   `cyper::Client` (compio + hyper) for the HTTP/1.1 GET handshake, then
   `compio_ws::WebSocketStream` (compio + tungstenite) for the framed
   message phase. Zero tokio, no `Send`/`Sync` constraints inside the
   isolate (single-threaded per AGENTS.md "V8 per thread, one isolate
   per app").
7. **Spec-faithful, byte-faithful, observable-event-faithful.** The
   polyfill diverges in ~12 observable ways (no client-side at all;
   `bufferedAmount` always returns 0; no close-code validation; no
   binary support; no Sec-WebSocket-Accept verification on response;
   no fragment-rejection on URL parse; no protocol-uniqueness check;
   no `binaryType="blob"` Blob construction; `send()` coerces ANY
   non-string via `String(data)` instead of dispatching on type;
   `wasClean` only reflects code===1000 instead of "received CloseFrame";
   `onclose` deletes registry entry but doesn't drop the underlying
   connection; no UTF-8 validation on text frames). v1 fixes all
   of them.

### Non-goals (explicit)

- **WebSocketStream API.** The newer streams-based API
  (https://github.com/ricea/websocketstream-explainer; spec drafted
  in https://websockets.spec.whatwg.org/#websocketstream — currently
  in `tentative/` in WPT) is OUT for v1. WPT `websockets/stream/tentative/`
  excluded from v1 pass target. v2 will add it on top of the v1 native
  shape: a WebSocketStream wraps a native WebSocket and exposes
  `readable`/`writable` ReadableStream/WritableStream pairs over the
  same RFC 6455 framing. Streams-native already ships
  `from_native_source` / `from_native_sink` (D-9 of streams-native);
  WebSocketStream's `readable`/`writable` are constructed via those
  hooks. v1's WebSocket internal-state shape is forward-compatible
  with v2 (the receive-loop driver and the send queue are the same in
  both).
- **`permessage-deflate` extension (RFC 7692).** Out for v1. v1 does
  not advertise `Sec-WebSocket-Extensions` on the client handshake
  (matches workerd's default). undici DOES advertise
  `permessage-deflate; client_max_window_bits` (`undici/lib/web/websocket/connection.js:84-88`)
  but flags compressed frames as a hard fail in the receiver if the
  extension wasn't negotiated. Servers MUST NOT use RSV1 on client
  connections that didn't negotiate; per RFC 6455 §5.2 we fail the
  connection on any unexpected RSV1. v2 ships permessage-deflate via
  the existing `flate2` workspace dep — well-defined drop-in once
  needed (an AI-builder app rarely sees compression-significant message
  sizes; SSE-style streaming over WS is the dominant pattern and is
  fine uncompressed).
- **HTTP/2 + RFC 8441 (Bootstrapping WebSockets with HTTP/2).** Out
  for v1. Deno's hybrid path supports it (`deno/ext/websocket/lib.rs:305-342`);
  cyper is HTTP/1.1-only as built (`Cargo.toml:25`,
  `default-features = false, features = ["rustls", "json", "stream"]` —
  no `http2` feature). When cyper enables HTTP/2 (and we flip the feature
  flag in fetch-native v2), this v1 design grows an `Upgrade: websocket`
  vs `:protocol websocket` branch in `network.rs::connect`. The IDL
  surface is unaffected.
- **HTTP/3 (RFC 9220 — WebSockets over QUIC).** Out indefinitely; cyper
  doesn't ship QUIC.
- **Server-side WebSocket on the gateway.** OUT — but ONLY in the sense
  of "the gateway accepts inbound WebSockets and proxies them to a
  worker". The CURRENT gateway path
  (`docs/reference/websocket-design.md`) IS preserved: when a worker's
  fetch handler returns `Response { status: 101, webSocket: client }`
  with `client` being one half of a `WebSocketPair`, the runtime detects
  status 101 + a `webSocket` slot on the response (existing
  `crates/runtime/src/http.rs:171-179`), the gateway accepts the inbound
  WebSocket via tungstenite (`compio_ws::accept_async`), and pumps
  messages bidirectionally between the WebSocketPair's "client" half
  and the inbound socket. The `WebSocketPair` machinery is tightly
  coupled to that flow and KEEPS WORKING in v1 — see §VI for the
  coexistence strategy.
- **WebSocket-Hibernation (workerd-specific extension).** OUT. workerd's
  hibernation surface (`web-socket.h:200-216`, `serializeAttachment`,
  `deserializeAttachment`, `state.acceptWebSocket()` from a Durable
  Object) only makes sense in workerd's stateful Durable-Object model.
  zeroship's worker is stateless per request; the `state.acceptWebSocket`
  method is a no-op-throws-TypeError on construction. Future work if
  zeroship ever ships a Durable-Object equivalent.
- **The `back-forward-cache-*.window.js` WPT files.** OUT — they require
  a navigable document and BFCache, both of which are browser-only.
  Excluded from the WPT target list.
- **`mixed-content.https.any.js` enforcement.** OUT — server-side
  runtime, no mixed-content concept. The IDL parsing of `wss:` from
  `https:` upgrade still happens (URL parse rules); the
  "block insecure connections" enforcement does not.
- **`referrer.any.js`.** OUT — referrer policy is a no-op in zeroship
  (no document context). Matches fetch-native D-3 / D-27.
- **`cookies/` subdirectory.** OUT — server-side runtime with no
  per-user cookie jar. Matches fetch-native (cookies non-goal).
- **Multi-globals (`multi-globals/`).** OUT — single isolate per app
  per AGENTS.md. WebSocket-cross-realm-transfer would need MessagePort
  which we don't ship.

### Status

Draft v1 — design only; nothing has shipped yet.

Post-completion: file as a date-prefixed ADR under `docs/decisions/`.
The Decisions table below is the immutable contract; everything else
is illustrative.

### Decisions (settled)

| # | Decision | Rationale | Section |
|---|----------|-----------|---------|
| **D-1** | Pure native: `WebSocket`, `MessageEvent`, `CloseEvent` are `#[v8_class]` Rust types. The 166-LOC `embed/websocket.js` polyfill and the five `__ws*` callbacks in `crates/runtime/src/websocket.rs` are deleted in cutover landing 3. No JS polyfill fallback once shipped. | Single source of truth; eliminates the data-as-string duality that today forces every binary message through `String(data)` and silently corrupts UTF-8. | §I |
| **D-2** | `WebSocket : EventTarget` via `#[v8_inherit(EventTarget)]` — same pattern AbortSignal uses (`crates/runtime/src/dom/abort_signal.rs:124`). The class wrapper holds `Box<WebSocketImpl>` in V8 internal field 0 (where `WebSocketImpl` is `#[repr(C)]` with `EventTarget` as the first field for layout-compatible casts via `event_target::listeners_of` — but EventTarget is empty, so this is a zero-byte field; the cast still works because `attach_listeners` hangs the listener Rc off the wrapper as a private symbol). All other slots live in the boxed Rust state. | Spec mandates `instanceof EventTarget`; existing macro extension covers it. | §V, §XIII |
| **D-3** | `MessageEvent : Event` and `CloseEvent : Event` via `#[v8_inherit(Event)]` — same pattern CustomEvent uses today (`crates/runtime/src/dom/custom_event.rs:83`). Both new classes use `#[repr(C)]` with `Event` as the first field; the inherited Event getters (`event.type`, `event.target`, `event.bubbles`, …) cast `*mut MessageEvent`/`*mut CloseEvent` directly to `*mut Event` per the offset-zero layout. | Spec compliance: `messageEvent instanceof Event === true`, `closeEvent instanceof Event === true`. The polyfill's "plain Event with expandos" produces FALSE for `instanceof MessageEvent`, breaking duck-typed library code. | §III, §XIII |
| **D-4** | Single-threaded per isolate: every Rust struct is `!Send + !Sync`. No `Mutex`/`RwLock` anywhere. Inter-class references use `Rc<RefCell<…>>`. The compio + cyper transport is per-thread (`thread_local! CLIENT` in `crates/runtime/src/fetch.rs`); the WebSocket reuses that. | AGENTS.md "V8 per thread, one isolate per app". A `Send` constraint would force `Arc<Mutex<…>>` and serialise the receive fast-path. | §V, §VII |
| **D-5** | Internal-slot storage rule (sharpened from streams D-2): each spec slot lives in EXACTLY ONE location. Numeric / Cell-flag slots live in the boxed Rust struct (`Box<WebSocketImpl>` in internal field 0); slots that need observable JS identity preservation (e.g. cached Headers, the once-built MessageEvent template) live in V8 private symbols. There is NO mirror; no shadow-copy. The ready-state slot lives ONLY in the Rust enum `Cell<ReadyState>` — not also in `[[readyState]]` getter cache. | Spec algorithms must be observably indistinguishable from "directly modify `[[…]]`". The single-source rule is the entire consistency model — same model that streams-native and fetch-native use. | §V, §XIII |
| **D-6** | `bufferedAmount` is real, observable, and updated synchronously in `send()` and the send-pump. Held as `Cell<u64>` on the Rust state. The polyfill returns 0 always (it had no concept of "queued bytes"); v1 increments on every send-call by the byte count of the encoded frame payload (UTF-8 encoded for strings, raw byte length for binary), and decrements as the send-pump drains the wire. WPT `Send-before-open.any.js` checks the increment-before-OPEN behaviour explicitly. | Spec §3.1 attribute `bufferedAmount`; required for backpressure-aware uploads. | §V.5 |
| **D-7** | `binaryType` defaults to `"blob"` per spec §3.1. The polyfill defaults to `"arraybuffer"` (likely a workerd-historical default; workerd had a `websocket_standard_binary_type` compat flag — `web-socket.h:421-422`). v1 ships the spec-correct default. WPT `Create-valid-url-binaryType-blob.any.js` checks the default value. The polyfill cutover landings (§XIV) flip this default; one landing is dedicated to documenting the behaviour change in the upgrade notes (some apps may rely on `arraybuffer` default — they break loudly via `dataView.something is not a function`, which is the desired failure mode rather than silent bytes-→string drift). | Spec compliance. The "loudly break" failure mode is preferable to silent silent-binary-corruption. | §V.4 |
| **D-8** | Close-code validation per WHATWG §3.1 close algorithm (https://websockets.spec.whatwg.org/#dom-websocket-close): codes 1000 and 3000-4999 are valid; everything else throws InvalidAccessError. The `code` argument is `[Clamp] unsigned short` and is processed through the proper WebIDL ConvertToInt[Clamp] algorithm (see §V.5 — `clamp_unsigned_short`; this resolves CRITICAL #2 from the v1 review). NO bypass for legacy code-ranges (workerd has a `pedantic_wpt` compat flag — `web-socket.c++:629-644`; v2 picks "spec-strict"). Reason length cap: 123 bytes UTF-8 encoded; longer throws SyntaxError. Both validations happen BEFORE the readyState dispatch (per spec close steps 1-3). On the wire: `Option<u16>` semantics — when the user calls `close()` with no code argument, the Close frame is sent with empty payload per RFC 6455 §5.5.1; we MUST NOT serialise 1005, which RFC 6455 §7.4.1 reserves as an internal sentinel (CRITICAL #6). The CONNECTING-state path runs `fail_the_websocket_connection` (RFC 6455 §7.1.7), distinguishing "no socket yet" from "socket open but JS hasn't seen open" sub-cases via the connection-handle slot (CRITICAL #4). | Spec; covered by WPT verbatim. | §V.5 |
| **D-9** | URL parse uses the existing native URL class (ada-url backed). Scheme MUST normalise to `ws` or `wss` (`http`→`ws`, `https`→`wss`). Fragment MUST be empty (post-parse `urlRecord.hash === ""` AND the original input did not end with `#`). Both cases throw SyntaxError. The URL is stored as a `url::Url` (the parsed record) plus a separate `String` for the "input as serialized for `.url` getter" — per spec §3.1 the `url` attribute returns the URL "serialized" via the URL Standard's serializer, which is essentially the same as `urlRecord.href` for non-fragment-bearing URLs. | Spec; the URL parser path is shared with fetch (`fetch_native::dictionaries::parse_url`). | §V.1 |
| **D-10** | Protocol validation per spec / RFC 6455: each `protocol` element must be a non-empty token whose codepoints are in U+0021..U+007E excluding the RFC 7230 separator characters (`"(),/:;<=>?@[\]{}` plus space and HT). Duplicates (case-insensitive) throw SyntaxError. The undici utility `isValidSubprotocol` (`undici/lib/web/websocket/util.js:101-141`) is the literal implementation; we port it 1:1 in Rust. | Spec; WPT `Create-protocols-repeated.any.js`, `Create-protocols-repeated-case-insensitive.any.js`, `Create-protocol-with-space.any.js`, `Create-asciiSep-protocol-string.any.js`, `Create-nonAscii-protocol-string.any.js`, `Create-extensions-empty.any.js` cover the validation. | §V.2 |
| **D-11** | Construction kicks off the connection asynchronously ("in parallel" per spec §3.1 step 13). The constructor returns immediately with `readyState === CONNECTING (0)`. A compio task is spawned via `state.spawned_ops.push(...)` that performs the HTTP/1.1 GET upgrade and then runs the receive loop. The constructor synchronously installs the `signal_priv` slot (for the optional `signal: AbortSignal` extension) and the underlying connection-handle private symbol; the connection itself materialises later. | Spec; WPT `Create-valid-url.any.js` constructs many sockets and asserts they're `CONNECTING` synchronously. | §V.3, §VII |
| **D-12** | The HTTP/1.1 client handshake reuses the existing `cyper::Client` thread-local from `crates/runtime/src/fetch.rs:316-335` (already SSRF-resolver-equipped, already TLS-enabled via rustls). The handshake builds a plain `http::Request` with method GET, the seven WebSocket headers (Upgrade, Connection, Sec-WebSocket-Key, Sec-WebSocket-Version, Sec-WebSocket-Protocol, Origin, Host), sends via cyper, validates the 101 response (RFC 6455 §4.1 steps 2-6), and on success extracts the Upgraded I/O object. **cyper 0.8 does NOT expose `Upgraded`** (not in its public API; verified docs.rs); the design therefore **bypasses cyper for the WebSocket path** and uses `compio_ws::client_async` (`compio-ws 0.3.1`, already in `Cargo.lock:608` as a transitive dep), which performs the handshake and frame phase end-to-end on a `compio::TcpStream` / `compio_tls::Stream` we open ourselves. SSRF + TLS verification are duplicated for the WS case (the resolver lives one level higher than cyper for this path; the `is_blocked_ip` blocklist in `fetch.rs:36` is shared verbatim). v2 may unify if cyper grows an `Upgraded` API. | Pragmatic — `compio-ws` is already a transitive workspace dep, has a tungstenite-based receiver, and exposes the framing as a `Stream<Item=Result<Message>>`. v1 doesn't reimplement the wire protocol in-tree. | §VII |
| **D-13** | Receive loop architecture: a compio task per WebSocket. The task holds the `compio_ws::WebSocketStream` and reads frames. On each frame, it pushes an `OpResult::WebSocketEvent { ws_id, kind: WsEvent }` onto the runtime's event queue. The runtime loop dispatches the event by entering the V8 isolate, looking up the cached `WsCachedHandles { ws_obj, on_message, on_close }` from `WebSocketImpl`, building MessageEvent / CloseEvent / Event, and calling `dispatch_event`. The receive task exits when (a) it reads a Close frame, (b) the underlying TCP/TLS stream errors out, or (c) the runtime cancels the task on isolate teardown. | Mirrors fetch's `spawn_body_reader` pattern. The compio task is `!Send` (single-threaded), the receive-loop borrows the stream &mut without locks. | §VII.2, §VII.3 |
| **D-14** | A new `OpResult::WebSocketEvent` variant carries `{ ws_id: u32, kind: WsEvent }` where `WsEvent` is one of `Open { protocol, extensions }`, `Message(WsMessage)`, `Close { code, reason, was_clean }`, `Error { reason }`. Three variants are needed (Open is separate from Message because it transitions state and updates two attributes; Close carries the wasClean flag computed from "did we receive a Close frame matching our sent code?"; Error is the abnormal-closure / handshake-failed path). The existing `OpResult::Completed { value: String }` cannot carry a binary `Vec<u8>`; the existing `OpResult::JsValue { resolver, value: ResolveValue }` is for class-method async returns and would need a fresh resolver per event. The dedicated variant is simpler. | Same justification as streams-native D-3 (existing OpResult variants are misshaped). The dispatch loop in `runtime.rs` already pattern-matches on OpResult; one new arm. | §V.5, §VII.3 |
| **D-15** | Send queue: a `VecDeque<WsFrame>` on the boxed `WebSocketImpl`, drained by the send-pump. Adding a frame increments `bufferedAmount` by the encoded payload byte count; draining decrements by the same. The send-pump is woken via the `outgoing_ready: Rc<Cell<bool>>` flag + `pump_waker: Rc<RefCell<Option<Waker>>>` pair the existing `crates/runtime/src/state.rs:160-163` already carries; the same pump pattern works for client sockets and WebSocketPair-coupled sockets. | Reuses the existing waker plumbing for the WebSocketPair pump. | §V.5, §VII.4 |
| **D-16** | Backpressure on receive: NO. The runtime delivers MessageEvents eagerly; user code that doesn't drain its message queue causes events to pile up in the JS handler's microtask queue, NOT in our Rust queue. This matches the polyfill's behaviour and undici's default (no auto-pause). workerd does pause the read loop on `pendingAutoResponseTimestamp` accumulation but only in the hibernation path. v2 may add a "max in-flight events" cap if AI-builder apps demonstrate a need. | The TCP backpressure is sufficient — V8's microtask queue is the natural buffer. Adding receive-side pause adds complexity for no observable spec benefit. | §VII.2 |
| **D-17** | UTF-8 validation on text frames: enforced. Per RFC 6455 §8.1, a text frame whose payload is not valid UTF-8 MUST cause the connection to fail with code 1007 ("Invalid frame payload data"). `compio-ws` / tungstenite handle this internally (tungstenite returns `WebSocketError::Utf8` and yields a Close frame with code 1007 on the next poll); the receive task converts that into a `Close { code: 1007, reason: "Invalid UTF-8", was_clean: false }` event. JS observers see `error` event followed by `close` event with `wasClean: false`. | Spec §3.2 step 2.1 ("If the bytes are not a valid UTF-8 sequence: fail the WebSocket connection"); WPT covers this via the autobahn fuzzers in `websockets/autobahn/` (which we are NOT yet running in CI but may add as a separate job). | §V.4, §VII.2 |
| **D-18** | `binaryType="blob"` builds a Blob via `crates/runtime/src/blob.rs::Blob::from_bytes` (already shipped). The MessageEvent's `data` is a `v8::Global<v8::Object>` pointing to the Blob wrapper. `binaryType="arraybuffer"` builds an ArrayBuffer via `v8::ArrayBuffer::new_backing_store_from_vec` (zero-copy where possible — same pattern as fetch's `Body.arrayBuffer()`). The choice is read at MessageEvent-construction time, NOT at frame-arrival time, so a JS user can flip `binaryType` between two binary frames and the SECOND frame uses the new value. | Spec §3.1 attribute `binaryType` ("on getting, must return the value to which it was last set"); the read-at-dispatch-time semantics is observable (and tested by WPT — `Send-binary-blob.any.js` and `Send-binary-arraybuffer.any.js`). | §V.4 |
| **D-19** | `send(data)` dispatch order matches the spec EXACTLY: **String first**, then Blob, then ArrayBuffer, then ArrayBufferView. Per WHATWG §3.1 send algorithm steps 3-6 (https://websockets.spec.whatwg.org/#dom-websocket-send), the spec literally tests `if data is a string`, `if data is a Blob object`, `if data is an ArrayBuffer object`, `if data is an ArrayBufferView object` in that order. (v1 inverted this and claimed string went last; v2 corrects.) Type-test predicates are by branding (`v8::Local::<v8::ArrayBuffer>::try_from`, `is_blob` via internal-field tag), NOT by `data.toString` — so the "Blob with custom toString" hazard the v1 rationale invoked is a non-issue: `is_blob(blob)` is true regardless of any user-defined `toString`. (The real polyfill bug — `String(data)` coercion at `embed/websocket.js:77` that silently corrupts binary — is fixed by branding-based dispatch in any order; v2 picks spec order for clarity.) Mirrors undici (`undici/lib/web/websocket/websocket.js`, String first) and workerd (`workerd/api/web-socket.c++`, String first). (addresses critic CRITICAL #1) | Spec §3.1; WPT `Send-data.any.js`, `Send-unicode-data.any.js`, `Send-binary-blob.any.js`, `Send-binary-arraybuffer.any.js`, `Send-binary-arraybufferview-*.any.js` (one file per typed-array variant — ~12 files). | §V.4 |
| **D-20** | `WebSocketPair` (workerd extension) is preserved — same JS surface, same gateway dispatch path. Internally, both halves of a `WebSocketPair` are `WebSocket` instances with a `peer_id: Option<u32>` slot set on each (the existing `WebSocketState::peer_id` carries forward verbatim). When `send()` is called on one half, the message is enqueued on BOTH the local outgoing queue (for the gateway pump) AND the peer's incoming queue (for the in-process JS-side delivery). The polyfill's existing logic at `crates/runtime/src/websocket.rs:140-148` is the model. | Backwards compatibility with the existing gateway WebSocket flow. The same `#[v8_class] WebSocket` covers both modes; the union of slot semantics fits naturally. | §VI |
| **D-21** | `accept()` method — workerd extension preserved. Required by `WebSocketPair[1].accept()` to begin local message delivery on the server-side half. For client-side WebSockets created via `new WebSocket(url)`, `accept()` throws TypeError (matches workerd `web-socket.c++:406-407`). The accept transition is read-only: once `accepted=true`, subsequent `accept()` calls are silent no-ops (matches workerd `web-socket.c++:417`). | Backwards compatibility. The IDL surface is `accept(): undefined` — no return value, no options dictionary in v1 (workerd's `AllowHalfOpen` option is a workerd-Durable-Object detail). | §V.7, §VI |
| **D-22** | Spec algorithm naming in Rust: every named spec algorithm gets a Rust function with the same name in `snake_case`. Lives in `crates/runtime/src/websocket/algorithms.rs` for cross-class operations (`establish_a_websocket_connection`, `feedback_the_establish_algorithm`, `make_disappear`, `fail_the_websocket_connection`, `close_the_websocket_connection`, `validate_close_code_and_reason`) and in `websocket/websocket.rs` for class-local methods. Same rule as streams-native D-20 and fetch-native D-20. | Reduces cognitive load when cross-referencing the spec. | §V, §IX |
| **D-23** | Per-isolate concurrent-WebSocket cap: 1024. Excess constructions DO NOT throw — the spec's "establish a WebSocket connection" runs in parallel (WHATWG §3.1 step 12: "Run this step in parallel"), and a connection-budget failure is observably a connection failure, not a constructor error. The constructor returns a normal `WebSocket` object in CONNECTING; the spawned connect task observes the budget overflow and queues an immediate `Error` then `Close{1006, was_clean: false}` event sequence (matching every other connection-failed path per WHATWG §4 "feedback from the protocol"). v1 contradicted itself here: the Decisions row said "throws RangeError"; §V.3 used async error+close. v2 commits to async-fail and updates §V.3's wording to match. (addresses critic CRITICAL #10) | Aligns with WHATWG §3.1's "in parallel" framing — every connection failure is an event, not an exception. 1024-cap matches `MAX_PENDING_OPS` in fetch; bounded memory at saturation. | §V.3, §XVII.10 |
| **D-24** | The `signal: AbortSignal` extension is shipped in v1 (workerd-style; spec doesn't require it). Constructor accepts `new WebSocket(url, { signal })` as a non-spec WebSocket-init dict member (parsed via the same dict-parser fetch uses). When the signal aborts during CONNECTING, the connection is cancelled with code 1000 (clean if accepted, abrupt otherwise) and any pending fetch is dropped. When it aborts after OPEN, the equivalent of `socket.close(1000)` runs. Until the WebSocket fires `open` or `close`, a strong ref keeps the signal alive (mirrors fetch's `signal.timeout` GC retention pattern). | Real-world demand: every undici-based library uses `AbortSignal` for fetch cancellation, and developers expect parity for WebSocket. v1 ships it because it's near-zero implementation cost given AbortSignal is already wired up for fetch. | §V.3, §VII.5 |
| **D-25** | Polyfill removal cadence: three landings — (1) ship native behind feature flag `runtime_native_websocket`, polyfill remains default; (2) flip default to native, polyfill remains as fallback; (3) delete polyfill JS file + the five `__ws*` callbacks in `crates/runtime/src/websocket.rs`. The native cutover (step 2) ALSO renames `crates/runtime/src/websocket.rs` to `crates/runtime/src/websocket/legacy.rs` to mark it deprecated; the cutover re-points the gateway WebSocket coupling logic to the new native module. Same cadence as streams D-19, fetch D-23. | Risk control. | §XIV |
| **D-26** | `ErrorEvent` IDL: NOT shipped natively in v1. The spec §3.1 step "fire a connection-failed event" uses a plain `Event("error")`, not `ErrorEvent`. (HTML's `ErrorEvent` is for script-load and uncaught-exception events — different shape.) workerd has its own `ErrorEvent` (`workerd/api/events.h`) that some Cloudflare Workers code uses; we match the SPEC default (plain Event), not workerd's extension. Existing app code that does `socket.addEventListener('error', e => e.message)` reads `undefined` — same as the polyfill's current behaviour at `embed/websocket.js:111-115`. | Spec compliance over workerd-extension-compat. ErrorEvent is a follow-up if a real creator app needs it (cheap; ~80 LOC). | §III, §V.5 |
| **D-27** | Sec-WebSocket-Key generation: 16 random bytes, base64-encoded (`base64::engine::general_purpose::STANDARD`). Random source: `aws_lc_rs::rand::fill` (already used by `crypto.getRandomValues`). The `Sec-WebSocket-Accept` server response is verified by computing `base64(SHA1(key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"))` and comparing to the received header value byte-by-byte. The GUID is the literal RFC 6455 §1.3 magic. | Spec; mismatch fails the connection per RFC 6455 §4.1 step 6 of the response checks. | §V.3, §IX.3 |

## I. Architecture overview

### I.1. The two-layer model

The native WebSocket is split into two layers, mirroring streams-native and
fetch-native:

1. **Public IDL surface** — V8 classes installed on the global object:
   `WebSocket`, `MessageEvent`, `CloseEvent`. Each class carries an
   internal-field-0 holding `Box<{Class}State>` per the existing
   `#[v8_class]` pattern. `WebSocket : EventTarget` and
   `{Message,Close}Event : Event` use `#[v8_inherit(...)]`.

2. **Internal connection engine** — a Rust-side WebSocket pipeline that
   runs the spec algorithms (`establish a WebSocket connection`,
   `feedback the establish algorithm`, `close the WebSocket connection`)
   over a `compio_ws::WebSocketStream`. The pipeline never calls into JS
   during the network phase; it operates entirely on Rust-side connection
   state and dispatches events into V8 via the runtime pump's
   `OpResult::WebSocketEvent` variant (D-14).

The boundary is sharp: V8 callbacks delegate to Rust methods on the boxed
state; Rust algorithm code spawns compio tasks and feeds events back via
the OpResult queue. The user-facing MessageEvent / CloseEvent objects are
constructed on the V8 side at dispatch time (each event is a fresh native
class instance, not a reused one — per spec §3.1 the events are created
on every fire).

```
┌───────────────────────────────────────────────────────────────────┐
│ V8 isolate                                                        │
│  ┌──────────────────┐ ┌──────────────────┐ ┌──────────────────┐   │
│  │ WebSocket        │ │ MessageEvent     │ │ CloseEvent       │   │
│  └────────┬─────────┘ └────────┬─────────┘ └────────┬─────────┘   │
│           │                    │                    │              │
│           ▼                    ▼                    ▼              │
│  ┌──────────────────┐ ┌──────────────────┐ ┌──────────────────┐   │
│  │ JS wrapper obj   │ │ JS wrapper obj   │ │ JS wrapper obj   │   │
│  │ slot[0]:         │ │ slot[0]:         │ │ slot[0]:         │   │
│  │  Box<WSImpl>     │ │  Box<MsgEvent>   │ │  Box<CloseEvent> │   │
│  │ priv syms:       │ │  (event @0,      │ │  (event @0,      │   │
│  │   signal,        │ │   data, origin,  │ │   code,          │   │
│  │   binaryType,    │ │   lastEventId,   │ │   reason,        │   │
│  │   peer (pair),   │ │   ports, source) │ │   wasClean)      │   │
│  │   conn           │ │                  │ │                  │   │
│  └────────┬─────────┘ └──────────────────┘ └──────────────────┘   │
│           │                                                       │
│           ▼                                                       │
│  ┌─────────────────────────┐                                      │
│  │ Box<WebSocketImpl>      │                                      │
│  │  ready_state            │                                      │
│  │  url                    │                                      │
│  │  url_serialized         │                                      │
│  │  protocol               │                                      │
│  │  extensions             │                                      │
│  │  buffered_amount        │                                      │
│  │  binary_type            │                                      │
│  │  cached_handles         │                                      │
│  │  send_queue             │                                      │
│  │  peer_id (Option)       │                                      │
│  │  accepted (Cell<bool>)  │                                      │
│  │  ws_id                  │                                      │
│  └─────────────────────────┘                                      │
│                                                                   │
└─────────────────────────────────────┬─────────────────────────────┘
                                      │ V8 callback hands an op
                                      ▼ to the runtime pump
┌───────────────────────────────────────────────────────────────────┐
│ compio runtime (per-thread)                                       │
│                                                                   │
│  spawned_ops → establish_a_websocket_connection                   │
│                 → cyper / direct compio TCP → TLS                 │
│                 → HTTP/1.1 GET handshake                          │
│                 → 101 + verify Sec-WebSocket-Accept               │
│                 → compio_ws::client_async                         │
│                 → WebSocketStream                                 │
│                                                                   │
│  receive_loop (per WS) ─ frame stream → OpResult::WebSocketEvent  │
│                          { ws_id, kind: WsEvent::{Open,Message,Close,Error} } │
│                                                                   │
│  send_pump (per WS)  ─ outgoing queue → WebSocketStream::send     │
│                                                                   │
└───────────────────────────────────────────────────────────────────┘
```

### I.2. File layout

```
crates/runtime/src/dom/                    [existing — append]
├── message_event.rs                  (new) MessageEvent class (~150 LOC)
└── close_event.rs                    (new) CloseEvent class (~120 LOC)

crates/runtime/src/websocket/         (new directory; replaces flat websocket.rs)
├── mod.rs                            (new) module root, public exports
├── websocket.rs                      (new) WebSocket class + IDL surface (~600 LOC)
├── algorithms.rs                     (new) spec named algorithms — establish_a_websocket_connection,
│                                          feedback_the_establish_algorithm, make_disappear,
│                                          fail_the_websocket_connection, close_the_websocket_connection,
│                                          validate_close_code_and_reason (~250 LOC)
├── handshake.rs                      (new) RFC 6455 §4.1 client handshake — request build,
│                                          Sec-WebSocket-Key gen, Sec-WebSocket-Accept verify (~200 LOC)
├── network.rs                        (new) cyper / direct compio TCP+TLS path; the upgrade
│                                          extraction (~200 LOC)
├── receive_loop.rs                   (new) per-WS compio task that reads frames and pushes
│                                          OpResult::WebSocketEvent (~180 LOC)
├── send_pump.rs                      (new) per-WS send queue drainer (~120 LOC)
├── pair.rs                           (new) WebSocketPair (workerd extension) — splits into two
│                                          coupled WebSockets, hooks into gateway 101-response path
│                                          (~120 LOC)
├── slots.rs                          (new) V8 private symbol helpers (peer, signal, conn, …) (~80 LOC)
├── budget.rs                         (new) D-23 concurrent-WS cap (~40 LOC)
└── constants.rs                      (new) RFC 6455 GUID, opcode/status enums,
                                            sub-protocol-token disallowed-char set (~50 LOC)

crates/runtime/src/lib.rs             (modified) +pub mod websocket; rename old websocket
                                                  module to websocket::legacy on landing 2.

crates/runtime/src/init.rs            (modified) install WebSocket / MessageEvent / CloseEvent
                                                  classes; route the existing __ws* callback
                                                  installers behind the feature flag.

crates/runtime/src/state.rs           (modified) add OpResult::WebSocketEvent variant (D-14).
                                                  WebSocketState fields evolve in place — additive
                                                  changes only, the existing gateway pump keeps
                                                  reading the same fields.

crates/runtime/src/embed/websocket.js (deleted in landing 3)
crates/runtime/src/websocket.rs       (deleted in landing 3 — replaced by crates/runtime/src/websocket/)

crates/runtime/src/runtime.rs         (modified) dispatch OpResult::WebSocketEvent variant.

crates/runtime/Cargo.toml             (modified) explicit `compio-ws = { version = "0.3" }` dep
                                                  (currently transitive via compio); add the
                                                  feature `tls` for compio_ws::client_async_tls.

crates/runtime/tests/
├── websocket_construct.rs            (new) hand-written Constructor / URL parse / protocol-validation tests
├── websocket_send.rs                 (new) Send dispatch — string / Blob / ArrayBuffer / typed-array
├── websocket_close.rs                (new) close-code validation, wasClean semantics
├── websocket_events.rs               (new) MessageEvent / CloseEvent / instanceof
├── websocket_pair.rs                 (new) WebSocketPair coupling tests (gateway path coverage)
├── websocket_e2e.rs                  (new) End-to-end against an in-process echo server
└── wpt_websockets.rs                 (new) WPT runner — constructor, close, send subdirs

crates/runtime/tests/wpt/websockets/  (new in sparse-checkout) — extends setup-wpt.sh.
```

### I.3. The connection lifecycle (algorithm-level)

Per the spec, `new WebSocket(url, protocols)` is §3.1; it parses the URL,
validates protocols, sets `[[readyState]] = CONNECTING`, and runs
"establish a WebSocket connection" (§4.1) **in parallel**. The
`feedback the establish a WebSocket connection algorithm` (§4.2) is
the callback invoked when the handshake either succeeds (`open`) or
fails (`error` + `close`).

The §4 "Feedback from the protocol" tasks queue MessageEvent (on
incoming text/binary frames), set `[[readyState]] = CLOSING` (when a
Close frame is received), or set `[[readyState]] = CLOSED` and dispatch
CloseEvent (when the connection is fully torn down).

The mutual recursion in the spec is preserved 1:1 in the Rust algorithm
names — `establish_a_websocket_connection`, `feedback_the_establish_algorithm`,
`fail_the_websocket_connection`, `close_the_websocket_connection`,
`validate_close_code_and_reason`. Every named function in §3 / §4 has a
Rust counterpart in `crates/runtime/src/websocket/algorithms.rs`.

### I.4. Boundary between Rust and JS during a WebSocket session

The native path crosses Rust↔V8 a small number of times per session:

1. **JS → Rust** at `new WebSocket(url, protocols)` constructor entry
   (1 V8 enter to allocate state, parse URL, validate protocols, kick
   off the connect task).
2. (No JS hops during the connect phase — the cyper request, the
   handshake validation, the SSRF check, the Sec-WebSocket-Accept
   verification all happen on the compio thread without re-entering V8.)
3. **Rust → V8 → JS** at "open": one enter to mint the (plain) Event,
   set `readyState`/`protocol`/`extensions` fields, dispatch `open`.
4. **Rust → V8 → JS** at each frame received: one enter per frame to
   build a fresh MessageEvent and dispatchEvent. The MessageEvent
   wrapper is built natively (single `new MessageEvent` allocation
   path; data is the Blob / ArrayBuffer / String constructed during
   the same enter).
5. **JS → Rust** at each `send(data)` call: one enter to copy the
   bytes into the send queue and bump bufferedAmount.
6. **Rust → V8 → JS** at "close": one enter to build CloseEvent,
   set `readyState=CLOSED`, dispatch `close`.

Compare to the polyfill's flow: 5 callbacks (`__wsCreatePair`,
`__wsLinkPair`, `__wsAccept`, `__wsSend`, `__wsClose`) plus a per-message
`_onMessage` lookup pass via `__wsRegistry[ws_id]` (resolved once-per-WS
in `resolve_ws_handles` and cached — that optimisation is preserved in
the native path via `WsCachedHandles`).

The dominant per-message cost in the polyfill profile is V8 entry/exit;
native eliminates the registry lookup and the `String(data)` coercion
entirely.

### I.5. AGENTS.md compliance audit

| Invariant | Status |
|-----------|--------|
| Zero tokio | OK — uses `compio_ws::WebSocketStream` (compio + tungstenite); cyper for the HTTP/1.1 GET. No tokio transitive dep introduced. |
| V8 per thread, one isolate per app | OK — every per-WebSocket compio task is `!Send`. `thread_local! CLIENT` from fetch is reused. |
| typed_id everywhere | N/A (a WebSocket isn't a typed entity in the platform sense; the per-isolate `ws_id: u32` is a runtime-internal identifier). |
| Wire formats are immutable contracts | OK — `WebSocket` IDL is the wire format and we match the WHATWG spec exactly. The internal `WebSocketImpl` struct is a private contract between the V8 callback path and the runtime pump; changes freely. |
| Native primitives are the kernel | OK — `WebSocket` / `MessageEvent` / `CloseEvent` are spec-mandated DOM/HTML primitives, not platform-specific zeroship globals. They live alongside Event / EventTarget / AbortSignal in the existing kernel surface. |
| The gateway is dumb | OK — the gateway-side WebSocketPair coupling continues to live in the worker, not the gateway. The gateway's existing 101-detection logic (`crates/runtime/src/http.rs:171-179`) is unchanged. |

## II. IDL surface — exhaustive

The complete spec inventory of interfaces (WebSockets §3.1 plus the
event types it dispatches):

| # | Interface | Section | Internal-field count | LOC est. (Rust) |
|---|-----------|---------|----------------------|------------------|
| 1 | `WebSocket : EventTarget` | §3.1 | 1 (Box<WebSocketImpl>) | ~600 |
| 2 | `MessageEvent : Event` | HTML §9.4.2 (referenced from spec §3.2) | 1 (Box<MessageEventState> with embedded Event) | ~150 |
| 3 | `CloseEvent : Event` | §3.2 | 1 (Box<CloseEventState> with embedded Event) | ~120 |

Items 2-3 are referenced from but not strictly part of the WebSockets
spec; v1 ships them as part of this design because the WebSocket cannot
be spec-compliant without them (`onmessage(e) → e instanceof MessageEvent`).

### II.1. `WebSocket` (§3.1)

**IDL (§3.1):**

```webidl
enum BinaryType { "blob", "arraybuffer" };

[Exposed=(Window,Worker)]
interface WebSocket : EventTarget {
  constructor(USVString url, optional (DOMString or sequence<DOMString>) protocols = []);
  readonly attribute USVString url;

  // ready state
  const unsigned short CONNECTING = 0;
  const unsigned short OPEN = 1;
  const unsigned short CLOSING = 2;
  const unsigned short CLOSED = 3;
  readonly attribute unsigned short readyState;
  readonly attribute unsigned long long bufferedAmount;

  // networking
  attribute EventHandler onopen;
  attribute EventHandler onerror;
  attribute EventHandler onclose;
  readonly attribute DOMString extensions;
  readonly attribute DOMString protocol;
  undefined close(optional [Clamp] unsigned short code, optional USVString reason);

  // messaging
  attribute EventHandler onmessage;
  attribute BinaryType binaryType;
  undefined send((BufferSource or Blob or USVString) data);
};
```

**Internal slots (§3.1.5):**
- `[[url]]` — URL record
- `[[binaryType]]` — BinaryType, initially "blob"
- `[[readyState]]` — number, initially CONNECTING (0)

**Plus implicit slots needed by the algorithms:**
- `[[bufferedAmount]]` — observable; tracked in Rust as `Cell<u64>`.
- `[[protocol]]` — initially empty string; set by feedback step 3.
- `[[extensions]]` — initially empty string; set by feedback step 2.
- `[[connection]]` — opaque RFC 6455 connection handle.
- `[[full]]` — RFC 6455 §6.1 "the WebSocket has been flagged as full"
  — internal flag for over-buffer-limit; never observable from JS.
- `[[serialised-url]]` — the value `url` getter returns. Not strictly
  a separate slot in spec (it just serialises `[[url]]` on access),
  but we cache it because URL serialisation isn't free.

**Rust state (boxed in V8 internal field 0):**

```rust
#[repr(C)]
pub struct WebSocketImpl {
    /// EventTarget base — required by #[v8_inherit(EventTarget)]; empty
    /// state per the existing convention, the listener Rc lives on the
    /// JS wrapper as a private symbol per `dom::event_target::attach_listeners`.
    /// #[repr(C)] + first-field is load-bearing for the
    /// `*mut WebSocketImpl as *mut EventTarget` cast in inherited methods.
    pub event_target: crate::dom::event_target::EventTarget,

    /// Spec [[readyState]]. Initially CONNECTING. State transitions are
    /// strictly forward (CONNECTING → OPEN → CLOSING → CLOSED) plus the
    /// CONNECTING → CLOSING → CLOSED early-fail path.
    pub ready_state: Cell<ReadyState>,

    /// Spec [[url]] — the parsed URL record. Stored as `url::Url` so we
    /// can serialise on demand (cheap for non-fragment URLs) and so the
    /// network code can read host/port/path directly.
    pub url: RefCell<url::Url>,

    /// Cached serialised URL — what `socket.url` getter returns.
    /// Computed once at construction; never mutated.
    pub url_serialized: String,

    /// Spec [[protocol]] — the negotiated subprotocol. Empty until OPEN.
    pub protocol: RefCell<String>,

    /// Spec [[extensions]] — the negotiated extensions header. Empty
    /// until OPEN.
    pub extensions: RefCell<String>,

    /// Spec [[bufferedAmount]] — bytes queued for send. Bumped by send(),
    /// decremented by the send-pump as each frame goes over the wire.
    pub buffered_amount: Cell<u64>,

    /// Spec [[binaryType]] — "blob" (default per spec §3.1) or "arraybuffer".
    pub binary_type: Cell<BinaryType>,

    /// Internal "full" flag — RFC 6455 §6.1 — set when the send queue
    /// exceeds an implementation cap (we choose 16 MB to match cyper's
    /// MAX_RESPONSE_SIZE). When set, the next send() short-circuits.
    pub full: Cell<bool>,

    /// Per-isolate WebSocket id. Used by:
    ///   - the runtime pump's `OpResult::WebSocketEvent { ws_id, ... }` dispatch;
    ///   - the WebSocketPair peer-link map;
    ///   - the gateway's 101-response-extraction path
    ///     (`http.rs:171-179` continues to read this field).
    pub ws_id: u32,

    /// Cached V8 handles — resolved once at first `dispatchEvent`,
    /// reused for every subsequent event. Same pattern the polyfill
    /// uses today (`crates/runtime/src/state.rs:127-141`,
    /// `crates/runtime/src/websocket.rs:90-118`). Native version caches
    /// the WebSocket wrapper Global (for the `dispatchEvent` `this`
    /// arg) plus optionally the `onmessage`/`onclose`/`onopen`/`onerror`
    /// handler functions (for the EventHandler IDL setter shortcuts).
    pub cached_handles: RefCell<Option<WsCachedHandles>>,

    /// Outgoing send queue. Each entry is a fully-encoded WS frame
    /// payload + opcode. The send-pump drains in order.
    pub send_queue: RefCell<VecDeque<WsFrame>>,

    /// Wake handle for the send-pump; set by send() / close() to
    /// notify the pump that new work is queued. Same plumbing as the
    /// existing `outgoing_ready: Rc<Cell<bool>>` +
    /// `pump_waker: Rc<RefCell<Option<Waker>>>` (state.rs:160-163).
    pub send_ready: Rc<Cell<bool>>,
    pub send_waker: Rc<RefCell<Option<Waker>>>,

    /// WebSocketPair peer (workerd extension). When `Some(other_id)`,
    /// every send() ALSO enqueues into the peer's incoming queue, and
    /// the pump knows this is a pair-coupled socket.
    pub peer_id: Cell<Option<u32>>,

    /// `accepted` flag (workerd extension) — true after `.accept()`.
    /// Required for WebSocketPair[1] before message delivery starts.
    /// Always true (pre-set) for client-side `new WebSocket(url)`
    /// sockets; the user never calls accept() on a client socket
    /// (in fact accept() throws TypeError on a client socket — D-21).
    pub accepted: Cell<bool>,

    /// Self-pointer back to the V8 wrapper object — used to mint clones
    /// (none in our case) and to pass to `dispatch_event(scope, target,
    /// event)`. Same WeakV8Ref pattern streams uses.
    pub self_weak: WeakV8Ref,
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
#[repr(u16)]
pub enum ReadyState {
    Connecting = 0,
    Open       = 1,
    Closing    = 2,
    Closed     = 3,
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum BinaryType {
    Blob,
    ArrayBuffer,
}

pub struct WsCachedHandles {
    pub ws_obj: v8::Global<v8::Object>,
    pub on_message_handler: Option<v8::Global<v8::Function>>,
    pub on_close_handler: Option<v8::Global<v8::Function>>,
    pub on_open_handler: Option<v8::Global<v8::Function>>,
    pub on_error_handler: Option<v8::Global<v8::Function>>,
}

pub enum WsFrame {
    Text(String),
    Binary(Vec<u8>),
    /// Spec §3.1 step 4 — Blob byte-extraction is async; bufferedAmount
    /// jumps by `size` synchronously and the bytes are resolved in the
    /// send pump. Holds a `Global<Object>` to the Blob and the cached
    /// size (so a `pop_front()`-induced decrement uses the right number
    /// even if Blob.size changes mid-transit, which it cannot per IDL).
    /// Mirrors undici's per-frame Blob deferral (`sender.js`).
    /// (addresses critic MAJOR #6 + #19 — covers the "WsFrame::Blob
    /// referenced by §V.4 prose but missing from the §II type list" gap.)
    Blob { handle: v8::Global<v8::Object>, size: u64 },
    /// Close frame. `code: None` means "send Close with empty payload"
    /// per RFC 6455 §5.5.1
    /// (https://datatracker.ietf.org/doc/html/rfc6455#section-5.5.1):
    /// the close frame is sent without a status code field. Code 1005
    /// is RESERVED as an internal sentinel and MUST NOT appear on the
    /// wire (RFC 6455 §7.4.1
    /// https://datatracker.ietf.org/doc/html/rfc6455#section-7.4.1).
    /// (addresses critic CRITICAL #6)
    Close { code: Option<u16>, reason: String },
}
```

The existing `WebSocketState` in `crates/runtime/src/state.rs:145-166`
is renamed to `WebSocketImpl` and grows the new fields above. The rename
is wire-format-internal (no on-disk format depends on it); the existing
`websockets: HashMap<u32, WebSocketState>` (state.rs:401) keeps its
shape — only the value type changes.

### II.2. `MessageEvent` (HTML §9.4.2 — referenced from WebSockets spec §3.2)

**IDL (HTML §9.4.2):**

```webidl
[Exposed=(Window,Worker,AudioWorklet)]
interface MessageEvent : Event {
  constructor(DOMString type, optional MessageEventInit eventInitDict = {});

  readonly attribute any data;
  readonly attribute USVString origin;
  readonly attribute DOMString lastEventId;
  readonly attribute MessageEventSource? source;
  readonly attribute FrozenArray<MessagePort> ports;

  undefined initMessageEvent(
    DOMString type,
    optional boolean bubbles = false,
    optional boolean cancelable = false,
    optional any data = null,
    optional USVString origin = "",
    optional DOMString lastEventId = "",
    optional MessageEventSource? source = null,
    optional sequence<MessagePort> ports = []
  );
};

dictionary MessageEventInit : EventInit {
  any data = null;
  USVString origin = "";
  DOMString lastEventId = "";
  MessageEventSource? source = null;
  sequence<MessagePort> ports = [];
};

typedef (WindowProxy or MessagePort or ServiceWorker) MessageEventSource;
```

**v1 simplifications:**
- `source` is always null (no Window/MessagePort/ServiceWorker support).
- `ports` is always an empty FrozenArray (no MessagePort transfer in v1).
- `lastEventId` is always the empty string for WebSocket-dispatched events
  (the field is meaningful for EventSource — when EventSource ships, the
  field becomes meaningful).

**Rust state (boxed in V8 internal field 0):**

```rust
#[repr(C)]
pub struct MessageEventState {
    /// Embedded Event — Event must be at offset 0 for the inherited
    /// Event getters to cast `*mut MessageEventState as *mut Event`.
    /// Same #[repr(C)] discipline as CustomEvent (`crates/runtime/src/dom/custom_event.rs`).
    pub event: crate::dom::event::Event,

    /// `data` — `any` per IDL, default null. Held as a Global so the
    /// SAME JS value (object identity) is returned on every `.data`
    /// access (per WebIDL the field is read-only and readback-stable).
    /// `Option` to avoid allocating a global for the null-default case.
    pub data: RefCell<Option<v8::Global<v8::Value>>>,

    /// `origin` — for WebSocket-dispatched events, the spec says "the
    /// serialization of the WebSocket object's url's origin". For our
    /// non-browser context, that's `serialize_origin(url)` per HTML §3.5
    /// (e.g. `wss://api.example.com:8443` → "wss://api.example.com:8443").
    /// For user-constructed MessageEvents via `new MessageEvent("...", { origin })`,
    /// this is the value passed in.
    pub origin: RefCell<String>,

    /// `lastEventId` — empty for WebSocket-dispatched events; meaningful
    /// for EventSource. Default "".
    pub last_event_id: RefCell<String>,
}
```

`MessageEventInit` parsing reads `bubbles`/`cancelable`/`composed`
(inherited from EventInit) plus `data` / `origin` / `lastEventId`
(MessageEvent-specific). `source` and `ports` are read but stored as
None/empty; the getters always return null/empty FrozenArray. (The IDL
must surface them — WPT `MessageEvent-constructor.any.js` tests the
existence of the getters even when the values are null.)

### II.3. `CloseEvent` (§3.2)

**IDL (§3.2):**

```webidl
[Exposed=(Window,Worker)]
interface CloseEvent : Event {
  constructor(DOMString type, optional CloseEventInit eventInitDict = {});
  readonly attribute boolean wasClean;
  readonly attribute unsigned short code;
  readonly attribute USVString reason;
};

dictionary CloseEventInit : EventInit {
  boolean wasClean = false;
  unsigned short code = 0;
  USVString reason = "";
};
```

**Rust state:**

```rust
#[repr(C)]
pub struct CloseEventState {
    pub event: crate::dom::event::Event,
    pub was_clean: Cell<bool>,
    pub code: Cell<u16>,
    pub reason: RefCell<String>,
}
```

The IDL is small and the parser reads four init members:
`bubbles`/`cancelable`/`composed` (inherited) plus `wasClean`/`code`/`reason`.
The `code` member uses WebIDL `unsigned short` conversion (mod 2^16, no
clamp — same as Event.eventPhase) per IDL §3.2.4.

### II.4. `BinaryType` enum

```webidl
enum BinaryType { "blob", "arraybuffer" };
```

A two-variant Rust enum. WebIDL `enum` setters reject any value not in
the set; the polyfill silently coerces unknown values to "blob" — undici
matches that behaviour at `undici/lib/web/websocket/websocket.js:453-461`.
The CURRENT spec (verified 2026-05-02) explicitly uses
`enum BinaryType { ... }` IDL syntax. Setting `socket.binaryType =
"unknown"` per WebIDL §3.10.4 enum coercion rules is a TypeError.
**Decision for v1:** match the CURRENT spec (TypeError) — undici and
the polyfill predate the IDL change to a strict enum. WPT
`binaryType-wrong-value.any.js` is the test that distinguishes; v1 should
pass it (matching strict enum). If a creator app breaks on the strictness,
add a graceful coercion behind a feature flag.

## III. Event classes — implementation detail

### III.1. MessageEvent — `#[v8_class]` impl skeleton

```rust
#[v8_class]
#[v8_inherit(crate::dom::event::Event)]
impl MessageEventState {
    #[v8_constructor]
    fn new(
        scope: &mut v8::PinScope,
        ty: v8::Local<v8::Value>,
        init: v8::Local<v8::Value>,
    ) -> Result<MessageEventState, OpError> {
        if ty.is_undefined() {
            return Err(OpError::type_error(
                "MessageEvent(): missing required 'type' argument",
            ));
        }
        let Some(type_str) = ty.to_string(scope) else {
            return Ok(MessageEventState::default());
        };
        let type_rust = type_str.to_rust_string_lossy(scope);
        let parsed = parse_message_event_init(scope, init)?;

        let me = MessageEventState::default();
        *me.event.event_type.borrow_mut() = type_rust;
        me.event.bubbles.set(parsed.bubbles);
        me.event.cancelable.set(parsed.cancelable);
        me.event.composed.set(parsed.composed);
        me.event.time_stamp.set(crate::dom::event::now_ms());
        *me.data.borrow_mut() = parsed.data;
        *me.origin.borrow_mut() = parsed.origin;
        *me.last_event_id.borrow_mut() = parsed.last_event_id;
        Ok(me)
    }

    #[v8_getter]
    fn data<'s>(&self, scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Value> {
        match self.data.borrow().as_ref() {
            Some(g) => v8::Local::new(scope, g.clone()),
            None => v8::null(scope).into(),
        }
    }

    #[v8_getter]
    fn origin(&self) -> String { self.origin.borrow().clone() }

    #[v8_getter]
    #[v8_name = "lastEventId"]
    fn last_event_id(&self) -> String { self.last_event_id.borrow().clone() }

    /// `source` — always null in v1 (no Window / MessagePort / ServiceWorker).
    /// IDL surface preserved for spec parity.
    #[v8_getter]
    fn source<'s>(&self, scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Value> {
        v8::null(scope).into()
    }

    /// `ports` — empty FrozenArray in v1.
    #[v8_getter]
    fn ports<'s>(&self, scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Value> {
        let arr = v8::Array::new(scope, 0);
        // FrozenArray semantics: freeze the array. WebIDL §3.10.34 says
        // "frozen array type values are exposed as immutable JavaScript
        // arrays" — Object.isFrozen(arr) === true.
        arr.set_integrity_level(scope, v8::IntegrityLevel::Frozen);
        arr.into()
    }

    #[v8_method]
    #[v8_name = "initMessageEvent"]
    fn init_message_event(
        &self,
        scope: &mut v8::PinScope,
        ty: v8::Local<v8::Value>,
        bubbles: v8::Local<v8::Value>,
        cancelable: v8::Local<v8::Value>,
        data: v8::Local<v8::Value>,
        origin: v8::Local<v8::Value>,
        last_event_id: v8::Local<v8::Value>,
        _source: v8::Local<v8::Value>,
        _ports: v8::Local<v8::Value>,
    ) -> Result<(), OpError> {
        // Per HTML §9.4.2: legacy method preserved for compat. No-op if
        // dispatch flag is set.
        if self.event.dispatch_flag.get() { return Ok(()); }
        // ... (same shape as Event::initEvent + CustomEvent::initCustomEvent)
        Ok(())
    }
}
```

### III.2. Native MessageEvent mint helper

For the runtime-internal path (frame received → MessageEvent dispatched),
we need a Rust-callable mint helper that bypasses the JS constructor —
identical pattern to `dom::event::build_abort_event` for AbortSignal's
"abort" event:

```rust
/// Mint a MessageEvent for a received WebSocket frame. The `data` is
/// already the JS-side representation (Blob or ArrayBuffer or String);
/// `origin` is the WebSocket's URL origin. Sets `is_trusted = true`
/// (platform-emitted), `bubbles = false`, `cancelable = false` per
/// HTML §9.4.2 + WebSockets §3.1.
pub(crate) fn build_message_event<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    data: v8::Local<v8::Value>,
    origin: &str,
) -> v8::Local<'s, v8::Object> {
    // Same allocation shape as build_abort_event in dom/abort_signal.rs.
    let tmpl = MessageEventState::install(scope);
    let inst_tmpl = tmpl.instance_template(scope);
    let obj = inst_tmpl
        .new_instance(scope)
        .expect("MessageEvent instance allocation failed");

    let me = MessageEventState::default();
    *me.event.event_type.borrow_mut() = "message".to_string();
    me.event.is_trusted.set(true);
    me.event.time_stamp.set(crate::dom::event::now_ms());
    *me.data.borrow_mut() = Some(v8::Global::new(scope, data));
    *me.origin.borrow_mut() = origin.to_string();

    let boxed: Box<MessageEventState> = Box::new(me);
    let raw = Box::into_raw(boxed);
    let raw_addr = raw as usize;
    let ext = v8::External::new(scope, raw as *mut std::ffi::c_void);
    obj.set_internal_field(0, ext.into());

    let class_fn = tmpl.get_function(scope).unwrap();
    let proto_key = v8::String::new(scope, "prototype").unwrap();
    let proto_v = class_fn.get(scope, proto_key.into()).unwrap();
    obj.set_prototype(scope, proto_v);

    let weak = v8::Weak::with_guaranteed_finalizer(
        scope, obj,
        Box::new(move || unsafe {
            drop(Box::from_raw(raw_addr as *mut MessageEventState));
        }),
    );
    std::mem::forget(weak);
    obj
}
```

### III.3. CloseEvent — `#[v8_class]` impl skeleton

Same shape as MessageEvent (~120 LOC). The `code` getter returns u16,
`reason` returns String, `wasClean` returns bool. Constructor reads
`{ bubbles, cancelable, composed, wasClean, code, reason }` from
init dict; `code` uses WebIDL `unsigned short` conversion (no Clamp —
spec doesn't mark `code` as `[Clamp]`).

A native mint helper `build_close_event(scope, code, reason, was_clean)`
mirrors `build_message_event`, used by the receive task on Close frame.

## IV. WebSocket constructor — step-by-step

Per spec §3.1 step 1-13 (the constructor):

> 1. Let baseURL be this's relevant settings object's API base URL.
> 2. Let urlRecord be the result of getting a URL record given url and baseURL.
> 3. If urlRecord is failure, then throw a "SyntaxError" DOMException.
> 4. If urlRecord's scheme is "http", then set urlRecord's scheme to "ws".
> 5. Otherwise, if urlRecord's scheme is "https", set urlRecord's scheme to "wss".
> 6. If urlRecord's scheme is not "ws" or "wss", then throw a "SyntaxError" DOMException.
> 7. If urlRecord's fragment is non-null, then throw a "SyntaxError" DOMException.
> 8. If protocols is a string, set protocols to a sequence consisting of just that string.
> 9. If any of the values in protocols occur more than once or otherwise fail to match
>    the requirements for elements that comprise the value of `Sec-WebSocket-Protocol`
>    fields as defined by The WebSocket protocol, then throw a "SyntaxError" DOMException.
> 10. Set this's url to urlRecord.
> 11. Let client be this's relevant settings object.
> 12. Run this step in parallel:
>     12.1. Establish a WebSocket connection given urlRecord, protocols, and client.
> 13. The constructor returns this. Each WebSocket object has an associated
>     ready state, which is a number representing the state of the connection.
>     Initially it must be CONNECTING (0).

### IV.1. Construction-mode dispatch

A single `#[v8_class]` covers both the v1 client-side `new WebSocket(url)`
and the workerd-extension server-side `new WebSocket()` (no-args, used
internally by `WebSocketPair`). The discriminator is "did the constructor
receive a URL?":

- **Client mode** (`new WebSocket("wss://...", protocols)`) — runs steps 1-13
  above. `accepted = false` is irrelevant (set true unconditionally
  because the JS user never calls accept() on a client socket).
- **Server mode** (`new WebSocket()` with no args, used by `WebSocketPair`) —
  skips the URL parse / handshake; `readyState = CONNECTING` until
  `accept()` is called. The polyfill's existing
  `WebSocket() { Reflect.construct(EventTarget, [], WebSocket); ... }`
  pattern (`embed/websocket.js:50-67`) maps to this mode.

Whether `new WebSocket()` (no args) is an IDL violation is a spec
question. The spec IDL has `constructor(USVString url, ...)` — `url` is
required, and a no-args call is a TypeError. But `WebSocketPair` needs
to construct WebSocket instances internally. Workerd's solution is to
have a private C++ constructor (`web-socket.h:222`) that JS user code
can't reach. v1 takes the same approach: the V8-visible JS constructor
(`new WebSocket(...)` from JS) requires `url`; a hidden mint helper
`mint_paired_websocket(scope) -> v8::Local<Object>` is used by
`WebSocketPair`.

```rust
#[v8_class]
#[v8_inherit(crate::dom::event_target::EventTarget)]
impl WebSocketImpl {
    #[v8_constructor]
    fn new(
        scope: &mut v8::PinScope,
        url_arg: v8::Local<v8::Value>,
        protocols_arg: v8::Local<v8::Value>,
    ) -> Result<WebSocketImpl, OpError> {
        // STEP per spec §3.1 step 1: URL is required (per IDL).
        if url_arg.is_undefined() {
            return Err(OpError::type_error(
                "WebSocket constructor: 'url' is required",
            ));
        }

        // STEP 1-2: parse URL. baseURL is null in our context
        // (no document; ada-url's parse-with-no-base is fine).
        let url_input = match url_arg.to_string(scope) {
            Some(s) => s.to_rust_string_lossy(scope),
            None => return Ok(WebSocketImpl::default()), // V8 has pending exception
        };
        let mut url_record = match url::Url::parse(&url_input) {
            Ok(u) => u,
            Err(e) => return Err(OpError::syntax_error(&format!(
                "WebSocket: invalid URL: {e}"
            ))),
        };

        // STEP 4-5: scheme normalisation.
        match url_record.scheme() {
            "http"  => { url_record.set_scheme("ws").unwrap();  }
            "https" => { url_record.set_scheme("wss").unwrap(); }
            "ws" | "wss" => {}
            scheme => return Err(OpError::syntax_error(&format!(
                "WebSocket: scheme must be 'ws' or 'wss', got '{scheme}'"
            ))),
        }

        // STEP 6: re-check scheme post-normalisation.
        match url_record.scheme() {
            "ws" | "wss" => {}
            _ => return Err(OpError::syntax_error("WebSocket: scheme must be 'ws' or 'wss'")),
        }

        // STEP 7: fragment must be null. ada-url's `Url.fragment()`
        // returns Option<&str>; Some("") means "URL had a `#` with
        // empty fragment", which the spec treats as non-null
        // (a fragment IS present, just empty).
        if url_record.fragment().is_some() || url_input.ends_with('#') {
            return Err(OpError::syntax_error(
                "WebSocket: URL must not contain a fragment",
            ));
        }

        // STEP 8-9: protocols.
        let protocols = parse_and_validate_protocols(scope, protocols_arg)?;

        // STEP 13: ready state CONNECTING.
        let mut impl_ = WebSocketImpl::default();
        impl_.url_serialized = url_record.as_str().to_string();
        *impl_.url.borrow_mut() = url_record.clone();
        impl_.ready_state.set(ReadyState::Connecting);
        impl_.binary_type.set(BinaryType::Blob); // D-7 spec default
        impl_.accepted.set(true); // client-mode: implicitly accepted
        impl_.ws_id = next_ws_id_from(scope);

        // STEP 12 (in parallel): kick off the connection. The compio
        // task is pushed onto the runtime's spawned_ops queue; it
        // runs as soon as we return to the event loop.
        let signal_obj = read_signal_member(scope, /* options arg */)?; // see §V.3
        spawn_establish_a_websocket_connection(
            scope,
            impl_.ws_id,
            url_record,
            protocols,
            signal_obj,
        );

        Ok(impl_)
    }
    // ... methods follow
}
```

### IV.2. Protocol validation (§3.1 step 9)

Per RFC 6455 §4.1, each subprotocol token must be a non-empty string of
codepoints from U+0021..U+007E excluding the RFC 7230 separator chars.
The undici implementation (`util.js:101-141`) is the model:

```rust
fn is_valid_subprotocol(s: &str) -> bool {
    if s.is_empty() { return false; }
    s.chars().all(|c| {
        let code = c as u32;
        if !(0x21..=0x7E).contains(&code) { return false; }
        // RFC 7230 separator chars (token disallowed):
        // " ( ) , / : ; < = > ? @ [ \ ] { }
        !matches!(c, '"'|'('|')'|','|'/'|':'|';'|'<'|'='|'>'|'?'|'@'
                   |'['|'\\'|']'|'{'|'}')
    })
}

fn parse_and_validate_protocols(
    scope: &mut v8::PinScope,
    arg: v8::Local<v8::Value>,
) -> Result<Vec<String>, OpError> {
    // String → single-element sequence; sequence-of-strings → as-is.
    let list: Vec<String> = if arg.is_undefined() {
        Vec::new()
    } else if arg.is_string() {
        vec![arg.to_rust_string_lossy(scope)]
    } else {
        // Iterate via [Symbol.iterator] per WebIDL sequence<DOMString>.
        // ... (same approach fetch's RequestInit headers parses an iterable)
        Vec::new()
    };

    // Uniqueness — case-insensitive per spec.
    let mut seen = std::collections::HashSet::new();
    for p in &list {
        let lowered = p.to_ascii_lowercase();
        if !seen.insert(lowered) {
            return Err(OpError::syntax_error(&format!(
                "WebSocket: duplicate protocol '{p}'"
            )));
        }
        if !is_valid_subprotocol(p) {
            return Err(OpError::syntax_error(&format!(
                "WebSocket: invalid protocol '{p}'"
            )));
        }
    }

    Ok(list)
}
```

### IV.3. WebSocket-init dict (the `signal` and `origin` extensions, D-24, D-28)

The IDL `constructor(USVString url, optional (DOMString or sequence<DOMString>) protocols = [])`
takes two args. v2 ships a third dictionary argument carrying
non-spec init members: `signal` (per workerd convention) and `origin`
(per RFC 6455 §10.2 opt-in design):

```webidl
// Extension — not in the WHATWG spec. ships in v1 (D-24, D-28).
dictionary WebSocketInit {
  AbortSignal? signal = null;
  /// Per RFC 6455 §10.2 + critic CRITICAL #3: server-side runtimes
  /// SHOULD NOT auto-emit Origin. If the creator app wants the server
  /// to see an Origin header, they pass it explicitly here. Empty /
  /// null / unset = no Origin header on the wire (the spec-correct
  /// default for non-browser clients).
  USVString? origin = null;
  /// Per RFC 6455 §7.4 + critic MAJOR #14: hard upper bound on
  /// in-bound message size. Defaults to 4 MiB (creator-app realistic);
  /// pinned in tungstenite via `WebSocketConfig::max_message_size`.
  /// Larger creator apps (file uploads over WS) may bump.
  unsigned long maxMessageSize = 4194304;
  /// Per critic MAJOR #12: hard upper bound on a single frame.
  /// Defaults to 1 MiB. Tungstenite default is 16 MiB; we tighten.
  unsigned long maxFrameSize = 1048576;
  /// Per critic MAJOR #21: client-side ping keepalive interval, in
  /// milliseconds. 0 = disabled. Default 30000 (matches undici and
  /// workerd defaults). The runtime sends an empty Ping every
  /// `pingIntervalMs` ms when the connection is idle; tungstenite
  /// auto-replies with Pong on incoming Pings.
  unsigned long pingIntervalMs = 30000;
};
```

The constructor signature becomes
`constructor(USVString url, optional (DOMString or sequence<DOMString>) protocols = [], optional WebSocketInit init = {})`.
`new WebSocket("wss://...", undefined, { signal, origin: "https://my-app" })`
works. The init parser:

- `signal` → AbortSignal Global stored as a private symbol; the connect
  task registers an `add_abort_algorithm` on it (§VII.5).
- `origin` → stored on the boxed `WebSocketImpl::origin: Option<String>`;
  `run_client_handshake` emits the header only when `Some`.
- `maxMessageSize` / `maxFrameSize` → passed into `tungstenite::WebSocketConfig`
  via `compio_ws::client_async_with_config`.
- `pingIntervalMs` → drives a per-WS interval timer that pushes
  `WsFrame::Ping(Vec::new())` onto the send queue.

(addresses critic CRITICAL #3 — origin opt-in; MAJORs #14, #12, #21 —
size and ping-interval pinning.)

## V. The WebSocket class — methods and getters

### V.1. URL handling

The `url` getter returns `url_serialized` directly — no recomputation.
The serialised form is computed once at construction.

### V.2. readyState / protocol / extensions / bufferedAmount / binaryType

```rust
#[v8_getter]
#[v8_name = "readyState"]
fn ready_state(&self) -> u16 { self.ready_state.get() as u16 }

#[v8_getter]
fn url(&self) -> String { self.url_serialized.clone() }

#[v8_getter]
fn protocol(&self) -> String { self.protocol.borrow().clone() }

#[v8_getter]
fn extensions(&self) -> String { self.extensions.borrow().clone() }

#[v8_getter]
#[v8_name = "bufferedAmount"]
fn buffered_amount(&self) -> u64 { self.buffered_amount.get() }

#[v8_getter]
#[v8_name = "binaryType"]
fn binary_type(&self) -> String {
    match self.binary_type.get() {
        BinaryType::Blob => "blob".into(),
        BinaryType::ArrayBuffer => "arraybuffer".into(),
    }
}

#[v8_setter]
#[v8_name = "binaryType"]
fn set_binary_type(&self, scope: &mut v8::PinScope, v: v8::Local<v8::Value>) -> Result<(), OpError> {
    // Per CURRENT spec §3.1: binaryType is `attribute BinaryType binaryType`,
    // and BinaryType is `enum BinaryType { "blob", "arraybuffer" }`. Per
    // WebIDL §3.10.4 enum coercion: any value not in the enum throws TypeError.
    let s = v.to_rust_string_lossy(scope);
    let new_type = match s.as_str() {
        "blob" => BinaryType::Blob,
        "arraybuffer" => BinaryType::ArrayBuffer,
        _ => return Err(OpError::type_error(&format!(
            "WebSocket.binaryType: invalid enum value '{s}'"
        ))),
    };
    self.binary_type.set(new_type);
    Ok(())
}
```

### V.3. The connect spawn — `establish_a_websocket_connection`

Per spec §4.1 — the algorithm runs "in parallel" (i.e. on a separate
"thread") relative to the JS that constructed the WebSocket. In our
single-threaded compio model, "in parallel" means "as a spawned compio
task that yields whenever it would block".

```rust
pub fn spawn_establish_a_websocket_connection(
    scope: &mut v8::PinScope,
    ws_id: u32,
    url: url::Url,
    protocols: Vec<String>,
    signal: Option<v8::Global<v8::Object>>,
) {
    // Per D-23 budget cap: enforce concurrent-WebSocket limit. WHATWG
    // §3.1 step 12 frames this entire spawn as "in parallel"; budget
    // overflow is a connection-failed signal, not a constructor throw.
    // Per WHATWG §4 "feedback from the protocol"
    // (https://websockets.spec.whatwg.org/#feedback-from-the-protocol):
    // every connection-failed path queues `error` then `close` with
    // wasClean=false. (addresses critic CRITICAL #10)
    if !budget::reserve_websocket_slot(scope) {
        push_event(scope, ws_id, WsEvent::Error {
            reason: "too many concurrent WebSocket connections".into(),
        });
        push_event(scope, ws_id, WsEvent::Close {
            code: 1006, reason: "".into(), was_clean: false,
        });
        return;
    }

    let url = url.clone();
    let request_id = current_request_id(scope);
    let cancel_flag = current_cancel_flag(scope);

    let task = Box::pin(async move {
        // Step 1 of §4.1: Convert `ws`/`wss` to `http`/`https` for the fetch.
        // Step 2-10: build the request (handled in handshake.rs).
        match handshake::run_client_handshake(url, protocols).await {
            Ok(handshake::Established { ws_stream, protocol, extensions }) => {
                // Push Open event first so JS sees state transition before
                // any messages arrive.
                push_event_blocking(ws_id, WsEvent::Open { protocol, extensions });

                // Begin the receive loop. This runs until Close / error.
                receive_loop::run(ws_id, ws_stream).await;
            }
            Err(handshake::HandshakeError::Aborted { signal_reason }) => {
                // Per WHATWG §4 "feedback from the protocol"
                // (https://websockets.spec.whatwg.org/#feedback-from-the-protocol,
                // "if the connection-failed state is reached"): the
                // connection-failed task fires:
                //   1. set readyState = CLOSED
                //   2. fire `error` event
                //   3. fire `close` event with wasClean=false
                // EVERY connection-failed path must produce both. v1
                // emitted only Close on the Aborted branch; v2 emits
                // Error then Close so AbortSignal users get the same
                // observable shape as any other handshake failure.
                // (addresses critic CRITICAL #5 + MAJOR #25)
                //
                // signal_reason is propagated into close.reason so that
                // app code reading `closeEvent.reason` knows WHY the
                // connection was aborted. v1 emitted reason="" always.
                let reason = signal_reason.unwrap_or_default();
                push_event_blocking(ws_id, WsEvent::Error {
                    reason: format!("aborted: {reason}"),
                });
                push_event_blocking(ws_id, WsEvent::Close {
                    code: 1006, reason, was_clean: false,
                });
            }
            Err(e) => {
                push_event_blocking(ws_id, WsEvent::Error {
                    reason: format!("{e}"),
                });
                push_event_blocking(ws_id, WsEvent::Close {
                    code: 1006, reason: "".into(), was_clean: false,
                });
            }
        }

        OpResult::WebSocketEvent { ws_id, kind: WsEvent::TaskFinished }
    });

    push_spawned_op(scope, task);
}
```

`push_event_blocking` writes into a per-WS Rc<RefCell<VecDeque<WsEvent>>>
plus signals via `Notify` — a small compio-native channel. The runtime
event loop drains the queue when it sees `OpResult::WebSocketEvent`.

If `signal` is provided:

```rust
if let Some(sig_global) = signal {
    let sig_obj = v8::Local::new(scope, sig_global.clone());
    let ws_id_clone = ws_id;
    crate::dom::abort_signal::add_abort_algorithm(
        scope, sig_obj,
        Box::new(move || {
            // Wake the connect task with an Aborted result — it will
            // emit Close{1006} and exit.
            cancel_flag_for_ws(ws_id_clone).cancel();
        }),
    );
}
```

### V.4. send(data) — type-dispatched

```rust
#[v8_method]
fn send(
    &self,
    scope: &mut v8::PinScope,
    data: v8::Local<v8::Value>,
) -> Result<(), OpError> {
    // Per spec §3.1: throw InvalidStateError if CONNECTING.
    if self.ready_state.get() == ReadyState::Connecting {
        return Err(OpError::invalid_state_error("WebSocket is still CONNECTING"));
    }

    // For CLOSING / CLOSED, send is a silent no-op per spec §3.1 step 2
    // ("If the connection is established and the WebSocket closing
    // handshake has not yet started"). The polyfill does the same.
    if self.ready_state.get() != ReadyState::Open {
        return Ok(());
    }

    // Dispatch order per WHATWG §3.1 send algorithm steps 3-6
    // (https://websockets.spec.whatwg.org/#dom-websocket-send):
    //   step 3: "If data is a string, ..."
    //   step 4: "If data is a Blob object, ..."
    //   step 5: "If data is an ArrayBuffer object, ..."
    //   step 6: "If data is an ArrayBufferView object, ..."
    // Type-test predicates are by branding (V8 internal slots / IsBlob),
    // NOT by `data.toString` — so the "Blob.toString hazard" that the
    // v1 rationale invented is not a real concern.
    // (addresses critic CRITICAL #1)
    if data.is_string() {
        let str_v8 = data.to_string(scope)
            .ok_or_else(|| OpError::error("send: string conversion failed"))?;
        // USVString conversion: V8 strings can contain unpaired
        // surrogates; per WebIDL USVString
        // (https://webidl.spec.whatwg.org/#es-USVString) the spec replaces
        // them with U+FFFD before transmission. `to_rust_string_lossy`
        // performs the same replacement.
        let s = str_v8.to_rust_string_lossy(scope);
        self.queue_text(s);
        return Ok(());
    }
    // Blob — duck-type via has-internal-field-0-and-Blob-tag.
    if crate::blob::is_blob(scope, data) {
        // Per spec step 4: enqueue a "send placeholder" with the Blob's
        // size so bufferedAmount reflects the byte count synchronously,
        // and resolve the actual bytes in the send pump. Matches undici
        // (`sender.js`).
        let blob_size = crate::blob::size_sync(scope, data);
        self.queue_blob(scope, data, blob_size);
        return Ok(());
    }
    if let Ok(ab) = v8::Local::<v8::ArrayBuffer>::try_from(data) {
        let bs = ab.get_backing_store(scope);
        let bytes = bs.iter().map(|c| c.get()).collect::<Vec<u8>>();
        self.queue_binary(bytes);
        return Ok(());
    }
    if let Ok(view) = v8::Local::<v8::ArrayBufferView>::try_from(data) {
        // ArrayBufferView (Uint8Array, Float32Array, DataView, …) —
        // per spec, send the underlying buffer's bytes from
        // view.byteOffset, view.byteLength.
        let mut buf = vec![0u8; view.byte_length()];
        let copied = view.copy_contents(&mut buf);
        debug_assert_eq!(copied, buf.len());
        self.queue_binary(buf);
        return Ok(());
    }
    // Per WHATWG §3.1: if data matches none of the above, the WebIDL
    // union conversion algorithm coerces to USVString.
    let str_v8 = data.to_string(scope)
        .ok_or_else(|| OpError::error("send: data could not be coerced"))?;
    let s = str_v8.to_rust_string_lossy(scope);
    self.queue_text(s);
    Ok(())
}

fn queue_text(&self, s: String) {
    let bytes_len = s.len() as u64;
    self.send_queue.borrow_mut().push_back(WsFrame::Text(s));
    self.buffered_amount.set(self.buffered_amount.get() + bytes_len);
    self.notify_send_pump();
}

fn queue_binary(&self, b: Vec<u8>) {
    let bytes_len = b.len() as u64;
    self.send_queue.borrow_mut().push_back(WsFrame::Binary(b));
    self.buffered_amount.set(self.buffered_amount.get() + bytes_len);
    self.notify_send_pump();
}

fn notify_send_pump(&self) {
    self.send_ready.set(true);
    if let Some(waker) = self.send_waker.borrow_mut().take() {
        waker.wake();
    }
}
```

The Blob path defers byte extraction: `Blob.bytes()` is async per IDL,
and the spec is silent on whether the WebSocket send is sync or async at
the byte level. v2 follows undici (`undici/lib/web/websocket/sender.js`):
`bufferedAmount` jumps by `blob.size` synchronously, and the send pump
extracts bytes when it pops the frame. The `WsFrame::Blob { handle, size }`
variant in §II.1 carries the Global<Object> + cached size; the pump
calls `crate::blob::extract_bytes(scope, handle).await` and then writes
a Binary message. (addresses critic MAJOR #6 timing-discussion gap and
#19 missing-WsFrame-variant gap.)

`queue_blob` increments `bufferedAmount` by `size` and pushes the
deferred frame onto the queue, so `socket.send(blob); socket.bufferedAmount`
read on the next line returns the right value. The synchronous-increment-
async-drain split matches undici's observable shape and passes WPT
`Send-binary-blob.any.js`'s bufferedAmount-after-send assertion.

### V.5. close(code, reason) — validation + state transition

```rust
#[v8_method]
fn close(
    &self,
    scope: &mut v8::PinScope,
    code: v8::Local<v8::Value>,
    reason: v8::Local<v8::Value>,
) -> Result<(), OpError> {
    // Step 1: validate code.
    // Per WHATWG IDL, the `code` argument is `optional [Clamp] unsigned
    // short code`. The `[Clamp]` extended attribute drives the WebIDL
    // ConvertToInt algorithm
    // (https://webidl.spec.whatwg.org/#abstract-opdef-converttoint, the
    // [Clamp] case):
    //   1. If V is NaN → return 0.
    //   2. Set V to min(max(V, 0), 65535).
    //   3. If V is finite and V−floor(V) === 0.5 and floor(V) is even,
    //      return floor(V) (round half to even — banker's rounding).
    //   4. Otherwise return round(V) (half-away-from-zero is wrong here;
    //      [Clamp] specifies banker's rounding only for the .5 tie case;
    //      otherwise nearest-integer).
    // v1 used `code.uint32_value(scope).unwrap_or(0).min(65535) as u16`
    // which:
    //   - reads V8's uint32_value (modulo 2^32 on negative — wrong vs
    //     step 2 which clamps to 0),
    //   - never applies banker's rounding (V8 truncates toward zero).
    // v2 implements ConvertToInt[Clamp] explicitly via `clamp_unsigned_short`.
    // (addresses critic CRITICAL #2)
    let code_opt: Option<u16> = if code.is_undefined() {
        None
    } else {
        Some(algorithms::clamp_unsigned_short(scope, code))
    };

    // Step 2: validate reason length.
    let reason_opt: Option<String> = if reason.is_undefined() {
        None
    } else {
        let s = reason.to_rust_string_lossy(scope);
        Some(s)
    };

    // Per WHATWG §3.1 close algorithm
    // (https://websockets.spec.whatwg.org/#dom-websocket-close):
    //   step 1: if code is present and not 1000 nor in 3000-4999, throw.
    //   step 2: if reason is present, UTF-8 encode; if > 123 bytes, throw.
    //   step 3: run the close-the-WebSocket-connection algorithm with
    //           code/reason and readyState dispatch.
    // Validation MUST run before any state transition.
    algorithms::validate_close_code_and_reason(code_opt, reason_opt.as_deref())?;

    // Per WHATWG §3.1 step 3 + RFC 6455 §7.1.7 (Fail the WebSocket
    // Connection https://datatracker.ietf.org/doc/html/rfc6455#section-7.1.7)
    // and §7.1.1 (Closing the Connection
    // https://datatracker.ietf.org/doc/html/rfc6455#section-7.1.1):
    //
    //   - CLOSING / CLOSED: do nothing.
    //   - CONNECTING: fail-the-WebSocket-connection. There may or may
    //     not be a TCP socket open at this point — the connect future
    //     might not have reached `compio_ws::client_async`, in which
    //     case there is no wire to send a Close frame on. In either
    //     sub-case the spec yields `Close{1006, was_clean: false}`
    //     (RFC 6455 §7.4.1: 1006 = Abnormal Closure).
    //   - OPEN: start the closing handshake — enqueue a Close frame,
    //     transition to CLOSING. The send pump emits the frame, then
    //     waits up to 5s (§IX.3) for the peer's Close ACK.
    //
    // v1 collapsed CONNECTING to "fail" without distinguishing the
    // two CONNECTING sub-cases; v2 distinguishes them via the
    // `connection_handle.is_some()` check inside
    // `fail_the_websocket_connection`. (addresses critic CRITICAL #4)
    use ReadyState::*;
    match self.ready_state.get() {
        Closing | Closed => return Ok(()),
        Connecting => {
            self.ready_state.set(Closing);
            // fail_the_websocket_connection handles the "wire exists"
            // vs "no wire yet" sub-cases internally — see §IX.2.
            // No Close frame is enqueued; the connect future will
            // observe the cancel flag and emit Close{1006} via the
            // connection-failed path.
            algorithms::fail_the_websocket_connection(
                scope,
                self,
                /* code */ code_opt,
                /* reason */ reason_opt.as_deref().unwrap_or(""),
            );
        }
        Open => {
            // Step 3.3-3.4: start closing handshake; readyState=CLOSING.
            // Per RFC 6455 §5.5.1
            // (https://datatracker.ietf.org/doc/html/rfc6455#section-5.5.1):
            // a Close frame's status code is OPTIONAL on the wire. When
            // the user calls `close()` with no code argument, we MUST
            // send a Close frame WITHOUT a status code field — we MUST
            // NOT serialise 1005, which RFC 6455 §7.4.1 reserves as an
            // internal sentinel. Storing `Option<u16>` in WsFrame::Close
            // preserves the "no code" case end-to-end.
            // (addresses critic CRITICAL #6)
            self.ready_state.set(Closing);
            let frame = WsFrame::Close {
                code: code_opt,  // Option<u16>, NOT Some(1005)
                reason: reason_opt.unwrap_or_default(),
            };
            self.send_queue.borrow_mut().push_back(frame);
            self.notify_send_pump();
        }
    }
    Ok(())
}
```

The `validate_close_code_and_reason` algorithm in `algorithms.rs`:

```rust
pub fn validate_close_code_and_reason(
    code: Option<u16>,
    reason: Option<&str>,
) -> Result<(), OpError> {
    // Per spec: "If code is not null, but is neither 1000 nor in 3000-4999,
    // throw InvalidAccessError DOMException."
    if let Some(c) = code {
        if c != 1000 && !(3000..=4999).contains(&c) {
            return Err(OpError::dom_exception(
                "InvalidAccessError",
                &format!("WebSocket.close: invalid code {c}"),
            ));
        }
    }
    // Per spec: "If reason is non-null, then UTF-8 encode it; if the
    // result is longer than 123 bytes, throw SyntaxError DOMException."
    if let Some(r) = reason {
        if r.as_bytes().len() > 123 {
            return Err(OpError::dom_exception(
                "SyntaxError",
                "WebSocket.close: reason must not be longer than 123 bytes",
            ));
        }
    }
    Ok(())
}

/// WebIDL `[Clamp] unsigned short` conversion per
/// https://webidl.spec.whatwg.org/#abstract-opdef-converttoint (Clamp
/// case). Applied to the close() `code` argument.
///
/// Algorithm:
///   1. Let V be the input value coerced to a Number (V8 ToNumber).
///   2. If V is NaN, return 0.
///   3. Set V to min(max(V, 0), 2^16 − 1).
///   4. If V is finite and V − floor(V) === 0.5 and floor(V) is even,
///      return floor(V) (round-half-to-even).
///   5. Otherwise return the integer nearest to V (rounding ties to the
///      higher integer for the non-half-even case is technically wrong;
///      WebIDL says "rounded to the nearest integer", which combined with
///      step 4 gives banker's rounding for ties only). For non-tie cases,
///      `(V).round()` (half-away-from-zero) gives the right answer because
///      ties are already covered by step 4.
///
/// (addresses critic CRITICAL #2)
pub fn clamp_unsigned_short(
    scope: &mut v8::PinScope,
    value: v8::Local<v8::Value>,
) -> u16 {
    // Step 1: ToNumber. V8's `number_value` runs ECMAScript ToNumber.
    let n = value.number_value(scope).unwrap_or(f64::NAN);

    // Step 2: NaN → 0.
    if n.is_nan() {
        return 0;
    }

    // Step 3: clamp to [0, 65535].
    let clamped = n.max(0.0).min(65535.0);

    // Step 4: round-half-to-even (banker's rounding) for the .5 tie case.
    if clamped.is_finite() {
        let floor_v = clamped.floor();
        let frac = clamped - floor_v;
        if frac == 0.5 {
            // Tie → round to the even integer.
            let floor_u = floor_v as u32;
            let rounded = if floor_u % 2 == 0 { floor_u } else { floor_u + 1 };
            return rounded as u16;
        }
    }

    // Step 5: nearest integer (half-away-from-zero is fine for non-ties).
    clamped.round() as u16
}
```

The same `clamp_unsigned_short` helper is reused if any future
WebSocket attribute or argument acquires `[Clamp]` semantics; the
function is callable from the dictionary parser (so CloseEventInit and
similar parsing paths share one implementation).

### V.6. EventHandler attributes (onopen / onmessage / onerror / onclose)

Per WebIDL `attribute EventHandler onopen` — these are
EventHandlerNonNull / EventHandlerNullable IDL attributes that, when
set, install (or replace) a single internal listener that the
EventTarget.dispatchEvent runs alongside any manually-added listeners.

The spec algorithm (HTML §8.1.5.1 "event handler IDL attributes"):
- Set: store the value in `[[event handler]]` slot. If the slot already
  had a listener registered, remove that listener; install a new one.
- Get: return the stored value (the original function, not a wrapper).
- The internal listener delegates to the stored value when fired.

undici's implementation (`websocket.js:355-445`) is the reference: it
stores the raw user function in `#events.{open,error,close,message}`,
removes the previous listener (if any) via removeEventListener, and adds
the new one via addEventListener. Same pattern in v1.

```rust
#[v8_setter]
fn onmessage(
    &self,
    scope: &mut v8::PinScope,
    fn_arg: v8::Local<v8::Value>,
) -> Result<(), OpError> {
    set_event_handler_attr(scope, /* this */ self, "message",
        &self.cached_handles.borrow_mut().as_mut().map(|h| &mut h.on_message_handler),
        fn_arg)
}

#[v8_getter]
fn onmessage<'s>(&self, scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Value> {
    match self.cached_handles.borrow().as_ref()
        .and_then(|h| h.on_message_handler.as_ref()) {
        Some(g) => v8::Local::new(scope, g.clone()).into(),
        None => v8::null(scope).into(),
    }
}
// ... same for onopen / onerror / onclose
```

### V.7. accept() — workerd extension preserved (D-21)

```rust
/// Cloudflare-Workers extension — required for WebSocketPair[1] to begin
/// receiving messages. Throws TypeError on a client-side WebSocket
/// (created via `new WebSocket(url)`); silent no-op if already accepted.
#[v8_method]
fn accept(&self, scope: &mut v8::PinScope) -> Result<(), OpError> {
    if self.peer_id.get().is_none() {
        // Client-side socket: accept is meaningless / forbidden per workerd.
        return Err(OpError::type_error(
            "WebSocket.accept: cannot accept() a client-side WebSocket",
        ));
    }
    if self.accepted.get() {
        return Ok(()); // idempotent
    }
    self.accepted.set(true);
    // The actual delivery start is handled by the gateway pump on the
    // next event-loop tick (the pump checks `accepted` before delivering).
    Ok(())
}
```

## VI. WebSocketPair (workerd extension) — coexistence with the gateway

### VI.1. Architecture

```
JS code:
    const [client, server] = Object.values(new WebSocketPair());
    server.accept();
    server.send("hi");
    return new Response(null, { status: 101, webSocket: client });

Native side:
    new WebSocketPair() — mints two paired WebSocket instances:
        ws_client (peer_id = ws_server.id)
        ws_server (peer_id = ws_client.id)
    Both start in CONNECTING / not-accepted.
    server.accept() — sets accepted=true on the server side; pump
                      starts delivering messages from the client-side
                      incoming queue.
    server.send("hi") — enqueues onto BOTH:
                       (a) server.outgoing (drained by the gateway pump
                           that owns the inbound TCP socket on the gateway side);
                       (b) client.incoming (delivered to the JS user
                           when the gateway pump connects the two halves).
    Response { status: 101, webSocket: client } — http.rs:171-179
                       extracts the client's ws_id and hands it to the
                       gateway, which:
                       - performs the inbound TCP WebSocket handshake
                         (compio_ws::accept_async) with the actual
                         client of the HTTP request,
                       - couples the inbound TCP stream's frame phase
                         to the WebSocketPair's "client" half via two
                         pumps (TCP→client.incoming and
                         client.outgoing→TCP).
```

### VI.2. The mint helper

```rust
/// Mint two coupled WebSocket wrappers. Used by the public
/// `new WebSocketPair()` constructor; not callable from JS directly.
/// Returns `[client_obj, server_obj]`.
pub(crate) fn mint_websocket_pair<'s>(
    scope: &mut v8::PinScope<'s, '_>,
) -> [v8::Local<'s, v8::Object>; 2] {
    // Allocate two ws_ids back-to-back.
    let id0 = next_ws_id(scope);
    let id1 = next_ws_id(scope);

    // Mint two WebSocketImpl instances. They ARE construction-mode-server
    // (no URL); the constructor path in §IV.1 isn't taken — we go through
    // the same `mint_*_internal` shape that `dom::abort_signal::mint_abort_signal`
    // uses to build a wrapper without invoking the JS constructor.
    let ws0_obj = mint_websocket_internal(scope, id0, /*peer*/ Some(id1));
    let ws1_obj = mint_websocket_internal(scope, id1, /*peer*/ Some(id0));

    [ws0_obj, ws1_obj]
}

fn mint_websocket_internal<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    ws_id: u32,
    peer: Option<u32>,
) -> v8::Local<'s, v8::Object> {
    let tmpl = WebSocketImpl::install(scope);
    let inst_tmpl = tmpl.instance_template(scope);
    let obj = inst_tmpl
        .new_instance(scope)
        .expect("WebSocket instance allocation failed");

    let mut impl_ = WebSocketImpl::default();
    impl_.ws_id = ws_id;
    impl_.peer_id.set(peer);
    impl_.ready_state.set(ReadyState::Connecting);
    impl_.url_serialized = String::new(); // pair sockets have no URL
    impl_.binary_type.set(BinaryType::Blob);

    let boxed: Box<WebSocketImpl> = Box::new(impl_);
    let raw = Box::into_raw(boxed);
    let raw_addr = raw as usize;
    let ext = v8::External::new(scope, raw as *mut std::ffi::c_void);
    obj.set_internal_field(0, ext.into());

    let class_fn = tmpl.get_function(scope).unwrap();
    let proto_key = v8::String::new(scope, "prototype").unwrap();
    let proto_v = class_fn.get(scope, proto_key.into()).unwrap();
    obj.set_prototype(scope, proto_v);

    crate::dom::event_target::attach_listeners(scope, obj);

    let weak = v8::Weak::with_guaranteed_finalizer(
        scope, obj,
        Box::new(move || unsafe {
            drop(Box::from_raw(raw_addr as *mut WebSocketImpl));
        }),
    );
    std::mem::forget(weak);

    obj
}
```

### VI.3. The `WebSocketPair` constructor

```rust
/// Mint a `WebSocketPair` — workerd extension, NOT a WHATWG class.
/// Returns an object with `0` and `1` properties pointing to the two
/// coupled WebSocket wrappers. Iterable via Object.values.
pub fn websocket_pair_constructor(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let [client, server] = mint_websocket_pair(scope);
    let pair_obj = v8::Object::new(scope);

    // Set integer-keyed properties 0 and 1.
    let zero = v8::Integer::new_from_unsigned(scope, 0);
    let one = v8::Integer::new_from_unsigned(scope, 1);
    pair_obj.set(scope, zero.into(), client.into());
    pair_obj.set(scope, one.into(), server.into());

    rv.set(pair_obj.into());
}
```

The native install registers `WebSocketPair` as a free function on
globalThis (NOT a `#[v8_class]` — workerd's
`JSG_TS_OVERRIDE(const WebSocketPair: { new(): { 0: WebSocket; 1: WebSocket } })`
is a JS-side type override, but the runtime impl is just a constructor
function). v1 matches that.

### VI.4. The gateway-side coupling

The existing flow in `crates/runtime/src/http.rs:171-179` reads
`Response.webSocket` (a Global<WebSocket>) and extracts its `ws_id`.
The runtime-up path (gateway) then accepts the inbound TCP socket via
`compio_ws::accept_async` and creates two pumps:

- **Pump A** (`tcp_to_pair`): reads frames from the inbound TCP
  WebSocket; for each frame, pushes a `WsMessage` onto the
  `client.incoming` queue. (The `client.incoming` is the one the JS
  user's `server.send(...)` writes to via the polyfill's existing
  peer-link logic — see `crates/runtime/src/websocket.rs:140-148`.)
- **Pump B** (`pair_to_tcp`): drains `client.outgoing` and writes
  frames to the inbound TCP WebSocket.

Both pumps run as compio tasks; they exit when either side closes.

The native cutover preserves both pumps verbatim — the only thing that
changes is the JS-visible class. The pump reads the same `WebSocketImpl`
fields the polyfill's `WebSocketState` exposed (renamed; see §I.2).

### VI.5. Relationship to the existing `docs/reference/websocket-design.md`

The existing reference doc covers the GATEWAY-SIDE WebSocket — the
inbound HTTP-Upgrade flow that turns an external client's `Upgrade:
websocket` request into a coupled `WebSocketPair` in user code. That
flow is unchanged by this proposal.

This proposal ADDS the CLIENT-SIDE WebSocket — the WHATWG-spec
`new WebSocket("wss://...")` flow that opens an OUTBOUND connection
from user code. Both flows land on the same `#[v8_class] WebSocketImpl`,
so the JS-visible class surface is unified, but the lifecycle differs:

| Aspect | Server-side (`WebSocketPair`) | Client-side (`new WebSocket(url)`) |
|--------|-------------------------------|------------------------------------|
| Construction | `new WebSocketPair()` mints 2 wrappers | `new WebSocket(url)` mints 1 |
| URL | None (empty `socket.url`) | The user-provided URL |
| Handshake | Performed by the gateway after the response is returned | Performed by the constructor's spawned task |
| `accept()` | Required (workerd extension) | Throws TypeError (D-21) |
| Lifecycle owner | Gateway pumps | Per-WS receive-loop / send-pump tasks |
| Peer | Other half of pair | None (`peer_id = None`) |

The existing reference doc at `docs/reference/websocket-design.md`
remains valid for the server-side flow. After this proposal lands and
becomes an ADR, the reference doc gets a follow-up update note pointing
to the ADR for the client-side spec compliance. (No deprecation; both
flows coexist.)

## VII. The receive loop and send pump

### VII.1. RFC 6455 framer — `compio_ws::WebSocketStream`

The crate `compio-ws 0.3.1` (already in `Cargo.lock:608`, transitively
via the `compio` umbrella) wraps `tungstenite 0.28.0` and exposes a
`Stream<Item = Result<Message, WebSocketError>> + Sink<Message>`
duplex over a `compio::TcpStream` or `compio_tls::TlsStream`. The
`Message` type's variants:

```rust
pub enum Message {
    Text(String),
    Binary(Vec<u8>),
    Ping(Vec<u8>),
    Pong(Vec<u8>),
    Close(Option<CloseFrame>),
    Frame(Frame),  // raw frame, rare
}
```

The framer handles:
- **Fragmentation reassembly** — text and binary frames split across
  multiple wire frames are reassembled before yielding.
- **Ping/Pong auto-response** — tungstenite auto-replies with Pong on
  Ping by default. We KEEP that default (matches undici, workerd).
- **Mask validation** — server-to-client frames must be unmasked
  (RFC 6455 §5.2 "MUST NOT mask"); the framer fails the connection on
  masked frames.
- **UTF-8 validation on text frames** — D-17. Fails with `WebSocketError::Utf8`.
- **Control-frame size limit** — RFC 6455 §5.5: control frames ≤ 125
  bytes. Framer enforces.

### VII.2. The receive loop

```rust
// crates/runtime/src/websocket/receive_loop.rs
use compio_ws::{Message, WebSocketStream};
use futures::stream::StreamExt;
use std::sync::Arc;

pub async fn run<S>(ws_id: u32, mut stream: WebSocketStream<S>)
where S: compio::buf::IoBuf + Unpin + 'static {
    // Per critic MAJOR #12: receive-side backpressure is required to
    // avoid an unbounded events_queue. tungstenite's stream-level
    // backpressure pauses on full kernel buffer; we add a per-WS event
    // queue cap above that. When the queue exceeds RECV_BACKPRESSURE_CAP
    // (default: 256 messages, configurable via WebSocketInit), the
    // receive loop awaits drain before reading the next frame.
    while let Some(item) = stream.next().await {
        // Backpressure-await before parsing the next frame so the
        // tungstenite-level receive buffer can apply TCP backpressure.
        wait_for_recv_drain(ws_id).await;
        let event = match item {
            Ok(Message::Text(s)) => WsEvent::Message(WsMessage::Text(s)),
            Ok(Message::Binary(b)) => WsEvent::Message(WsMessage::Binary(b)),
            Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => continue, // auto-handled
            Ok(Message::Close(frame)) => {
                // Per RFC 6455 §7.4.1: a Close control frame received
                // with NO status code is observed as the internal
                // sentinel 1005 ("No Status Rcvd"). 1005 IS the
                // internal-only API representation; the wire payload
                // for a "no code" Close is empty, NOT 1005-encoded.
                // The CloseEvent.code that JS sees is 1005 in this
                // case — that's the JS-observable value per spec §3.2
                // and the §XVII.6 CloseEvent.code policy.
                let (code, reason) = match frame {
                    Some(f) => (u16::from(f.code), f.reason.to_string()),
                    None => (1005, String::new()),
                };
                WsEvent::Close { code, reason, was_clean: true }
            }
            Err(e) => {
                // Protocol error / UTF-8 failure / TCP error.
                // Per WHATWG §4: connection-failed → fire `error` then
                // `close{1006, was_clean: false}`.
                push_event_blocking(ws_id, WsEvent::Error { reason: e.to_string() });
                push_event_blocking(ws_id, WsEvent::Close {
                    code: 1006, reason: String::new(), was_clean: false,
                });
                return;
            }
            Ok(Message::Frame(_)) => continue, // raw frame — not surfaced
        };
        // Per critic MAJOR #20: avoid the v1 use-after-move bug —
        // `event` is moved into push_event_blocking and then read by
        // matches!. v2 inspects the kind FIRST, then moves.
        let is_close = matches!(event, WsEvent::Close { .. });
        push_event_blocking(ws_id, event);
        if is_close { return; }
    }
    // Stream ended without a Close frame — abnormal closure.
    // Per WHATWG §4: emit error first, then close{1006, was_clean: false}.
    push_event_blocking(ws_id, WsEvent::Error {
        reason: "connection terminated without Close frame".into(),
    });
    push_event_blocking(ws_id, WsEvent::Close {
        code: 1006, reason: String::new(), was_clean: false,
    });
}
```

### VII.3. Event dispatch on the V8 thread

When the runtime pump pulls an `OpResult::WebSocketEvent` off the
`spawned_ops` queue, it enters V8 and dispatches:

```rust
// crates/runtime/src/runtime.rs — handle_op_result match arm
OpResult::WebSocketEvent { ws_id, kind } => {
    let state = state_clone.borrow();
    let Some(impl_) = state.websockets.get(&ws_id) else { return; };
    let Some(handles) = &impl_.cached_handles.borrow().as_ref() else {
        // First event for this WS: resolve and cache the handles.
        // Same lazy-resolve pattern the polyfill uses today.
        ws_resolve_handles(scope, ws_id);
        // ... retry
        return;
    };
    let ws_obj = v8::Local::new(scope, handles.ws_obj.clone());

    match kind {
        WsEvent::Open { protocol, extensions } => {
            // §4 step 4: set protocol, extensions; fire 'open' event.
            *impl_.protocol.borrow_mut() = protocol;
            *impl_.extensions.borrow_mut() = extensions;
            impl_.ready_state.set(ReadyState::Open);
            let event = build_plain_event(scope, "open");
            crate::dom::event_target::dispatch_event(scope, ws_obj, event);
        }
        WsEvent::Message(WsMessage::Text(s)) => {
            // §4 step 2.1: text frame → DOMString.
            let s_v8 = v8::String::new(scope, &s).unwrap();
            // MessageEvent.origin per WebSockets §3.1 + HTML §3.5 origin
            // serialisation: serialise the WebSocket URL's origin.
            // `url::Url::origin()` returns an `url::Origin`, whose
            // `ascii_serialization()` produces "wss://api.example.com:8443"
            // (omitting the default port). v1 used `.as_str()` on the
            // Origin which doesn't exist; v2 uses the correct method.
            // (addresses critic MAJOR #17)
            let origin = impl_.url.borrow().origin().ascii_serialization();
            let me = build_message_event(scope, s_v8.into(), &origin);
            crate::dom::event_target::dispatch_event(scope, ws_obj, me);
        }
        WsEvent::Message(WsMessage::Binary(b)) => {
            // §4 step 2.2 / 2.3: binary frame → Blob or ArrayBuffer per binaryType.
            let data_v8: v8::Local<v8::Value> = match impl_.binary_type.get() {
                BinaryType::Blob => {
                    let blob_obj = crate::blob::Blob::from_bytes(scope, b, None);
                    blob_obj.into()
                }
                BinaryType::ArrayBuffer => {
                    let bs = v8::ArrayBuffer::new_backing_store_from_vec(scope, b);
                    let ab = v8::ArrayBuffer::with_backing_store(scope, &bs.make_shared());
                    ab.into()
                }
            };
            // MessageEvent.origin per WebSockets §3.1 + HTML §3.5 origin
            // serialisation: serialise the WebSocket URL's origin.
            // `url::Url::origin()` returns an `url::Origin`, whose
            // `ascii_serialization()` produces "wss://api.example.com:8443"
            // (omitting the default port). v1 used `.as_str()` on the
            // Origin which doesn't exist; v2 uses the correct method.
            // (addresses critic MAJOR #17)
            let origin = impl_.url.borrow().origin().ascii_serialization();
            let me = build_message_event(scope, data_v8, &origin);
            crate::dom::event_target::dispatch_event(scope, ws_obj, me);
        }
        WsEvent::Close { code, reason, was_clean } => {
            // §4 step 3: set readyState=CLOSED; fire 'close' event.
            impl_.ready_state.set(ReadyState::Closed);
            let ce = build_close_event(scope, code, &reason, was_clean);
            crate::dom::event_target::dispatch_event(scope, ws_obj, ce);
        }
        WsEvent::Error { reason: _ } => {
            // §4 step 3.1: fire plain 'error' Event (NOT ErrorEvent — D-26).
            let event = build_plain_event(scope, "error");
            crate::dom::event_target::dispatch_event(scope, ws_obj, event);
        }
        WsEvent::TaskFinished => {
            // Cleanup hook — release the budget slot.
            budget::release_websocket_slot(scope);
        }
    }
}
```

### VII.4. The send pump

```rust
// crates/runtime/src/websocket/send_pump.rs
pub async fn run<S>(ws_id: u32, mut stream: WebSocketStream<S>)
where S: ... {
    loop {
        // Wait for send-ready signal.
        let queue_check = || {
            let state = current_state();
            let s = state.borrow();
            s.websockets.get(&ws_id)
                .map(|w| !w.send_queue.borrow().is_empty())
                .unwrap_or(false)
        };

        if !queue_check() {
            wait_for_send_ready(ws_id).await;
            continue;
        }

        let frame = {
            let state = current_state();
            let s = state.borrow();
            s.websockets.get(&ws_id).unwrap().send_queue.borrow_mut().pop_front()
        };

        let Some(frame) = frame else { continue; };

        match frame {
            WsFrame::Text(s) => {
                let bytes_len = s.len() as u64;
                if let Err(_) = stream.send(Message::Text(s)).await {
                    return; // connection broken
                }
                decrement_buffered(ws_id, bytes_len);
            }
            WsFrame::Binary(b) => {
                let bytes_len = b.len() as u64;
                if let Err(_) = stream.send(Message::Binary(b)).await {
                    return;
                }
                decrement_buffered(ws_id, bytes_len);
            }
            WsFrame::Text(_) | WsFrame::Binary(_) => {
                // (Handled above; rust analyser checks exhaustiveness here.)
                unreachable!()
            }
            WsFrame::Blob { handle, size: _ } => {
                // Per spec §3.1 step 4 — Blob byte extraction is async.
                // Decrement bufferedAmount by the cached size after
                // extraction succeeds. (addresses critic MAJOR #6, #19)
                let bytes = match crate::blob::extract_bytes_async(scope_handle, handle).await {
                    Ok(b) => b,
                    Err(_) => continue,
                };
                let bytes_len = bytes.len() as u64;
                if stream.send(Message::Binary(bytes)).await.is_err() {
                    notify_send_failure(ws_id);
                    return;
                }
                decrement_buffered(ws_id, bytes_len);
            }
            WsFrame::Close { code, reason } => {
                // Per RFC 6455 §5.5.1
                // (https://datatracker.ietf.org/doc/html/rfc6455#section-5.5.1):
                // Close frames may be sent with NO payload (no status
                // code, no reason) or with a 16-bit big-endian status
                // code optionally followed by a reason. Code 1005 is
                // reserved (RFC 6455 §7.4.1
                // https://datatracker.ietf.org/doc/html/rfc6455#section-7.4.1)
                // and MUST NOT appear in any sent Close frame.
                //
                // tungstenite's `Message::Close(None)` sends an empty
                // payload; `Message::Close(Some(CloseFrame { code, reason }))`
                // sends a code + reason payload. v2 maps Option<u16>
                // directly: `None` → empty payload; `Some(c)` → code
                // payload (with reason if non-empty).
                // (addresses critic CRITICAL #6)
                let payload = match code {
                    None => None,
                    Some(c) => Some(compio_ws::CloseFrame {
                        code: compio_ws::CloseCode::from(c),
                        reason: reason.into(),
                    }),
                };
                let _ = stream.send(Message::Close(payload)).await;

                // Per RFC 6455 §7.1.1
                // (https://datatracker.ietf.org/doc/html/rfc6455#section-7.1.1):
                // after sending Close, wait up to ~5 seconds for the
                // peer's Close frame ACK; if it doesn't arrive, drop
                // the TCP connection. tungstenite does NOT auto-timeout
                // on close; v2 wraps `stream.close(None).await` in a
                // 5s `compio::time::timeout` to avoid half-open hangs.
                // (addresses critic MAJOR #14)
                use std::time::Duration;
                let _ = compio::time::timeout(
                    Duration::from_secs(5),
                    stream.close(None),
                ).await;
                return; // close pump exits
            }
        }
    }
}
```

### VII.5. AbortSignal integration

Per D-24, `new WebSocket(url, undefined, { signal })` registers an abort
algorithm on the signal. When the signal aborts:

- During **CONNECTING**: cancel the connect future via the per-WS
  `CancelFlag`. The connect task observes the cancel, drops the
  in-flight cyper request, emits `Close{1006, was_clean: false}`.
- During **OPEN**: equivalent to `socket.close(1000)` — enqueue a close
  frame on the send pump.
- During **CLOSING / CLOSED**: no-op.

```rust
fn install_signal_abort_algorithm(
    scope: &mut v8::PinScope,
    signal: v8::Local<v8::Object>,
    ws_id: u32,
) {
    crate::dom::abort_signal::add_abort_algorithm(
        scope, signal,
        Box::new(move || {
            let state = current_state();
            let mut s = state.borrow_mut();
            let Some(impl_) = s.websockets.get_mut(&ws_id) else { return; };
            match impl_.ready_state.get() {
                ReadyState::Connecting => {
                    cancel_connect_future(ws_id);
                }
                ReadyState::Open => {
                    impl_.send_queue.borrow_mut().push_back(WsFrame::Close {
                        code: 1000, reason: String::new(),
                    });
                    impl_.ready_state.set(ReadyState::Closing);
                    impl_.notify_send_pump();
                }
                _ => {}
            }
        }),
    );
}
```

## VIII. Handshake — RFC 6455 §4.1 client-side

### VIII.1. Building the request

```rust
// crates/runtime/src/websocket/handshake.rs
use base64::Engine;
use sha1::Digest;

pub const RFC6455_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

pub struct Established<S> {
    pub ws_stream: compio_ws::WebSocketStream<S>,
    pub protocol: String,
    pub extensions: String,
}

pub async fn run_client_handshake(
    url: url::Url,
    protocols: Vec<String>,
    init_origin: Option<String>,
    config: tungstenite::protocol::WebSocketConfig,
) -> Result<Established<impl compio::buf::IoBuf>, HandshakeError> {
    // SSRF: TWO-PHASE check, mirroring fetch's discipline at
    // crates/runtime/src/fetch.rs:36-163.
    //
    // Phase 1 — URL string check: reject IP-literal hostnames in the
    //   blocklist (loopback, link-local, private, multicast, …).
    // Phase 2 — DNS resolution + revalidation: resolve the hostname,
    //   then check EACH resolved IP against the blocklist. This
    //   defends against DNS-rebinding attacks where `attacker.com`
    //   resolves to `127.0.0.1` between phase 1 and the actual
    //   `connect()`.
    // Phase 3 — Bind the resolved IP into the connect() call so
    //   the kernel can't re-resolve to a different address.
    //
    // v1 only ran phase 1 — TcpStream::connect((host, port)) did its
    // own internal DNS resolution that bypassed the SSRF check. v2
    // routes through `crate::fetch::resolve_and_check_ssrf` which
    // returns the validated `SocketAddr` to use directly.
    // (addresses critic CRITICAL #8)
    crate::fetch::validate_url(url.as_str())
        .map_err(HandshakeError::Ssrf)?;
    let host = url.host_str().ok_or(HandshakeError::MissingHost)?;
    let port = url.port_or_known_default().unwrap_or(if url.scheme() == "wss" { 443 } else { 80 });
    let addr = crate::fetch::resolve_and_check_ssrf(host, port)
        .await
        .map_err(HandshakeError::Ssrf)?;

    // Step 5: generate a 16-byte random key, base64 it.
    let mut key_bytes = [0u8; 16];
    aws_lc_rs::rand::fill(&mut key_bytes)
        .map_err(|_| HandshakeError::RandomFailure)?;
    let sec_websocket_key = base64::engine::general_purpose::STANDARD
        .encode(&key_bytes);

    // Build the GET request via http::Request. host/port were captured
    // above for SSRF resolution.
    let path_and_query = if let Some(q) = url.query() {
        format!("{}?{}", url.path(), q)
    } else {
        url.path().to_string()
    };

    let mut request = http::Request::builder()
        .method(http::Method::GET)
        .uri(&path_and_query)
        .header(http::header::HOST, format!("{host}:{port}"))
        .header(http::header::UPGRADE, "websocket")
        .header(http::header::CONNECTION, "Upgrade")
        .header("Sec-WebSocket-Key", &sec_websocket_key)
        .header("Sec-WebSocket-Version", "13");

    if !protocols.is_empty() {
        request = request.header(
            "Sec-WebSocket-Protocol",
            protocols.join(", "),
        );
    }

    // Origin: do NOT send unconditionally. Per RFC 6455 §10.2
    // (https://datatracker.ietf.org/doc/html/rfc6455#section-10.2):
    //   "Non-browser clients ... MAY send an Origin header field but
    //   SHOULD NOT (because they're not subject to same-origin policy)."
    // The runtime is server-side (no script-context origin); spoofing
    // Origin can also bypass server-side CSRF defences. v2 only emits
    // Origin when the user explicitly opts in via the WebSocketInit
    // dict (see §IV.3 — `origin` member). Mirrors undici and workerd
    // defaults; v1's "always send Origin matches Deno" claim was
    // incorrect (Deno sends Origin only when set on `WebSocketStream`
    // options, per `deno/ext/websocket/lib.rs`).
    // (addresses critic CRITICAL #3)
    if let Some(explicit_origin) = init_origin.as_deref() {
        request = request.header(http::header::ORIGIN, explicit_origin);
    }

    let request = request.body(()).map_err(HandshakeError::Http)?;

    // Open TCP/TLS, run handshake. We pass the SSRF-validated SocketAddr
    // directly so the kernel cannot re-resolve to a different host
    // between our DNS check and the connect call. compio_ws's
    // `client_async` and `client_async_tls_with_connector` return
    // (WebSocketStream, http::Response).
    let scheme = url.scheme();
    let (ws_stream, response) = match scheme {
        "ws" => {
            // Connect directly to the validated addr; SNI / Host header
            // still uses the original hostname.
            let stream = compio::net::TcpStream::connect(addr)
                .await.map_err(HandshakeError::Connect)?;
            compio_ws::client_async_with_config(request, stream, Some(config))
                .await.map_err(HandshakeError::WebSocket)?
        }
        "wss" => {
            // Per critic missing-concept #4: the rustls config used by
            // compio_ws::client_async_tls_with_connector controls:
            //   - the root cert store (we use the system trust store
            //     via rustls_native_certs, mirroring fetch);
            //   - SNI (set to the URL hostname so HTTPS-style cert
            //     validation works against virtual-hosted servers);
            //   - ALPN (we advertise `http/1.1` only; HTTP/2 over
            //     WebSocket per RFC 8441 is out of scope per Non-goals).
            // build_tls_connector() in network.rs returns the connector
            // wired to these settings.
            let connector = build_tls_connector(host)?;
            // Connect to the validated SocketAddr; the connector still
            // performs SNI/cert validation against `host`.
            let tcp = compio::net::TcpStream::connect(addr)
                .await.map_err(HandshakeError::Connect)?;
            compio_ws::client_async_tls_with_connector_and_config(
                request, host, tcp, connector, Some(config),
            ).await.map_err(HandshakeError::WebSocket)?
        }
        _ => unreachable!(),
    };

    // Verify Sec-WebSocket-Accept (RFC 6455 §4.1 step 6).
    // tungstenite already does this internally, BUT we double-check
    // for defence-in-depth — and so the design doc fully documents
    // the algorithm (the polyfill never did this).
    let accept_header = response.headers()
        .get("Sec-WebSocket-Accept")
        .ok_or(HandshakeError::MissingAccept)?;
    let expected = compute_sec_websocket_accept(&sec_websocket_key);
    if accept_header.as_bytes() != expected.as_bytes() {
        return Err(HandshakeError::AcceptMismatch);
    }

    // Extract negotiated subprotocol — per RFC 6455 §4.1 step 6 of
    // the response checks
    // (https://datatracker.ietf.org/doc/html/rfc6455#section-4.1):
    //   "If the response includes a |Sec-WebSocket-Protocol| header
    //    field and this header field indicates the use of a subprotocol
    //    that was not present in the client's handshake (the server
    //    has indicated a subprotocol not requested by the client),
    //    the client MUST Fail the WebSocket Connection."
    //
    // tungstenite enforces this when the client passes a subprotocols
    // list to its handshake builder, but the contract is fragile (the
    // header is set on `request.headers_mut()`; tungstenite parses it
    // back). v2 adds a defence-in-depth check below — same paranoia
    // as the Sec-WebSocket-Accept double-verify.
    // (addresses critic CRITICAL #9)
    let protocol = match response.headers().get("Sec-WebSocket-Protocol") {
        None => String::new(),
        Some(v) => {
            let server_pick = v.to_str()
                .map_err(|_| HandshakeError::InvalidSubprotocol)?
                .trim()
                .to_string();
            // RFC 6455 §4.1 says the server MUST echo a SINGLE value
            // (not a comma-separated list). If the server returns
            // anything other than one we offered, fail.
            if !protocols.iter().any(|p| p == &server_pick) {
                return Err(HandshakeError::UnrequestedSubprotocol(server_pick));
            }
            server_pick
        }
    };

    // Extract negotiated extensions — per RFC 6455 §9.1
    // (https://datatracker.ietf.org/doc/html/rfc6455#section-9.1):
    //   "if the Sec-WebSocket-Extensions header field includes any
    //    extension that the client did not request, the client MUST
    //    Fail the WebSocket Connection."
    //
    // v1 handshake advertises NO extensions (empty client offer set);
    // ANY value in the response header is a violation and MUST fail.
    // tungstenite has no validator that compares response extensions
    // against the (empty) client offer set by default — the v1 prose
    // claim "tungstenite enforces" was unverified. v2 enforces this
    // explicitly here. The Cargo features (§XI.1) lock down deflate
    // so RSV1=1 frames also fail at the framer level (defence in depth
    // — addresses critic MAJOR #18).
    // (addresses critic CRITICAL #7)
    let extensions = match response.headers().get("Sec-WebSocket-Extensions") {
        None => String::new(),
        Some(v) => {
            // Empty value (e.g. `Sec-WebSocket-Extensions:`) is permitted
            // but vacuous; treat as no extensions. Anything non-empty is
            // an unrequested extension and fails the connection.
            let raw = v.to_str()
                .map_err(|_| HandshakeError::InvalidExtensions)?
                .trim();
            if !raw.is_empty() {
                return Err(HandshakeError::UnrequestedExtensions(raw.to_string()));
            }
            String::new()
        }
    };

    Ok(Established { ws_stream, protocol, extensions })
}

pub fn compute_sec_websocket_accept(key: &str) -> String {
    let mut hasher = sha1::Sha1::new();
    hasher.update(key.as_bytes());
    hasher.update(RFC6455_GUID.as_bytes());
    let digest = hasher.finalize();
    base64::engine::general_purpose::STANDARD.encode(digest)
}
```

### VIII.2. The Origin header — opt-in only

Per RFC 6455 §10.2
(https://datatracker.ietf.org/doc/html/rfc6455#section-10.2): "Non-browser
clients ... MAY send an Origin header field but SHOULD NOT (because
they're not subject to same-origin policy)." The header was designed as
a browser-driven security signal (RFC 6454 §7); a server-side runtime
that emits it impersonates browser context and can bypass server-side
CSRF defences.

v2 default: do NOT emit Origin. The user may opt in by passing
`{ origin: "https://my-app.example" }` via the WebSocketInit dict
(see §IV.3); the constructor stores it on the boxed state and
`run_client_handshake` emits the header only when present. This
matches:

- **undici** — `undici/lib/web/websocket/connection.js` sets Origin
  only when an "environment settings object" provides one (browser
  context via embedder; absent for the standalone runtime).
- **workerd** — Origin sourced from the embedder's environment;
  not auto-emitted from the URL.
- **Deno** — `deno/ext/websocket/lib.rs` reads `headers` from the
  `WebSocketStream` options; no auto-emit. (v1's "matches Deno"
  citation was wrong.)

For `wss://api.example.com:8443/foo`, an explicit `origin` of
`"https://my-dashboard.example"` would emit `Origin:
https://my-dashboard.example` and HTML §3.5 origin serialisation
applies on the user-supplied string. (addresses critic CRITICAL #3)

Apps that previously relied on "the runtime auto-emitted Origin"
must now pass `origin` explicitly — flagged loudly in the runbook
and in the AGENTS.md surface for the cutover landing.

### VIII.3. Verifying the response

Per RFC 6455 §4.1 client-side response checks (step 1-6 of the response
"validation phase"). v2 verifies all six explicitly — defence in depth
even where tungstenite already checks. Treating tungstenite as a black
box for security-relevant checks is unsound; the design verifies each
check in our own code so the audit trail lives in the design, not in
the dependency.

1. Status MUST be 101. (tungstenite checks; v2 also returns
   `HandshakeError::BadStatus(actual)` if a non-101 reaches our handler.)
2. Upgrade header MUST be present and case-insensitively "websocket".
   (tungstenite checks; v2 re-asserts.)
3. Connection header MUST contain "Upgrade" (case-insensitive token list).
   (tungstenite checks; v2 re-asserts.)
4. Sec-WebSocket-Accept MUST equal `base64(SHA1(key + GUID))`. v2
   re-computes via `compute_sec_websocket_accept` and byte-compares.
5. **Sec-WebSocket-Extensions** — per RFC 6455 §9.1, any extension in
   the response that the client did not request MUST fail the connection.
   v2 advertises NONE (Non-goals). Any non-empty response value is a
   hard fail (`HandshakeError::UnrequestedExtensions`). The v1 claim
   "tungstenite enforces" was unverified and incorrect for the
   default-features build of `tungstenite 0.28`. (addresses critic
   CRITICAL #7)
6. **Sec-WebSocket-Protocol** — per RFC 6455 §4.1 step 6, the server's
   echoed subprotocol MUST be a member of the client's offered set.
   v2 checks this explicitly against the `protocols: Vec<String>` we
   sent; mismatch yields `HandshakeError::UnrequestedSubprotocol`.
   (addresses critic CRITICAL #9)

On any failure the connection is failed per RFC 6455 §7.1.7
(https://datatracker.ietf.org/doc/html/rfc6455#section-7.1.7), emitting
`Error` + `Close{1006, was_clean: false}` per WHATWG §4 (the spec calls
for 1006 on connection-failed paths, NOT 1002 — 1002 is a peer-sent
status code; the runtime that detects a protocol error during the
handshake never receives a peer Close, so the JS-observable code is
1006 "Abnormal Closure"). v1 used 1002 here; v2 corrects to 1006 to
match the §4 dispatch contract.

## IX. The spec algorithms — Rust map

Per D-22, every named spec algorithm has a Rust function with the same
name. Inventory:

| Spec | Spec section | Rust function | File |
|------|--------------|---------------|------|
| `establish a WebSocket connection` | §4.1 | `establish_a_websocket_connection` | `algorithms.rs` |
| `feedback the establish a WebSocket connection algorithm` | §4.2 | `feedback_the_establish_algorithm` | `algorithms.rs` |
| `make disappear` | §3.4 | `make_disappear` | `algorithms.rs` |
| `fail the WebSocket connection` | RFC 6455 §7.1.7 | `fail_the_websocket_connection` | `algorithms.rs` |
| `close the WebSocket connection` | RFC 6455 §7.1.1 | `close_the_websocket_connection` | `algorithms.rs` |
| `validate close code and reason` | §3.1 | `validate_close_code_and_reason` | `algorithms.rs` |
| `obtain a WebSocket connection` | §4.1 | (folded into `establish_a_websocket_connection`) | `handshake.rs` |
| `WebSocket message received` | §4.4 | (handled in receive_loop) | `receive_loop.rs` |
| `closing handshake started` | §4.5 | (handled in receive_loop on Close frame) | `receive_loop.rs` |
| `connection closed` | §4.6 | (handled in receive_loop on stream end) | `receive_loop.rs` |
| `WebSocket task source` | §4 | (implicit — every `push_event` queues onto the runtime's spawned_ops which is the only task source) | (none) |

Reviewer hint: if a future change diverges the Rust algorithm names
from the spec names, that's a regression. Same audit rule streams-native
D-20 and fetch-native D-20 use.

### IX.1. `make_disappear`

Per spec §3.4, called when a WebSocket is no longer reachable from JS
(GC). Behaviour:

- If not yet established: fail the connection with code 1001.
- If closing handshake not yet started: start the closing handshake with
  code 1001 ("Going Away").
- Otherwise: do nothing.

The native implementation hooks into the V8 weak finalizer for the
WebSocket wrapper (the same finalizer that drops `Box<WebSocketImpl>`).
Before the Box drop, we run `make_disappear` to send the Close frame.
This is best-effort; if the runtime is shutting down, the frame may not
make it onto the wire — same as every JS WebSocket impl.

### IX.2. `fail_the_websocket_connection`

Per RFC 6455 §7.1.7. Behaviour:

- If the connection is established (i.e. readyState was OPEN), send a
  Close frame with the given code (default 1002 "Protocol error" per
  spec) and reason; transition CLOSING → CLOSED.
- Drop the underlying TCP/TLS connection.
- Fire `error` event followed by `close` event with `wasClean: false`.

### IX.3. `close_the_websocket_connection`

Per RFC 6455 §7.1.1. The "clean close" path:
- Send a Close frame with the given code + reason.
- Wait up to a timeout (5s default) for the peer's Close frame ACK.
- Drop the TCP/TLS connection.

Tungstenite handles the timeout; we just drive the send pump's close
path.

## X. V8 internal-fields layout — per slot

### X.1. WebSocket

| Slot | Storage | Notes |
|------|---------|-------|
| `[[url]]` | `Box<WebSocketImpl>::url` (RefCell<url::Url>) | Spec slot — internal-fields[0] holds the box |
| `[[readyState]]` | `Box<WebSocketImpl>::ready_state` (Cell<ReadyState>) | Spec slot |
| `[[bufferedAmount]]` | `Box<WebSocketImpl>::buffered_amount` (Cell<u64>) | Spec slot |
| `[[binaryType]]` | `Box<WebSocketImpl>::binary_type` (Cell<BinaryType>) | Spec slot |
| `[[connection]]` | (drop the connection handle into the spawned compio task; not held in V8) | Spec slot — not directly observable |
| `[[full]]` flag | `Box<WebSocketImpl>::full` (Cell<bool>) | RFC 6455 §6.1 |
| `[[protocol]]` | `Box<WebSocketImpl>::protocol` (RefCell<String>) | Spec slot |
| `[[extensions]]` | `Box<WebSocketImpl>::extensions` (RefCell<String>) | Spec slot |
| `[[event handler]]` × 4 (onopen/onmessage/onerror/onclose) | `Box<WebSocketImpl>::cached_handles` (RefCell<Option<WsCachedHandles>>) | HTML EventHandler IDL |
| Listener Rc (EventTarget mixin) | private symbol `zs::dom::listeners` on the wrapper | Inherited from EventTarget |
| `signal` (D-24 extension) | private symbol `zs::ws::signal` on the wrapper | Holds `v8::Global<v8::Object>` (AbortSignal) |
| `peer_id` (WebSocketPair) | `Box<WebSocketImpl>::peer_id` (Cell<Option<u32>>) | Workerd extension |
| `accepted` (WebSocketPair) | `Box<WebSocketImpl>::accepted` (Cell<bool>) | Workerd extension |
| `ws_id` (per-isolate ID) | `Box<WebSocketImpl>::ws_id` (u32) | Internal ID for OpResult routing |

Rule: spec slots → Rust struct fields (since none need JS-identity
preservation). Cross-realm refs (the AbortSignal) → V8 private symbol.
Inherited mixins (EventTarget listeners) → same private symbol the
EventTarget mixin uses.

### X.2. MessageEvent

| Slot | Storage |
|------|---------|
| Inherited Event slots (`type`, `bubbles`, …) | `Box<MessageEventState>::event` (offset 0 — `#[repr(C)]`) |
| `[[data]]` | `Box<MessageEventState>::data` (RefCell<Option<v8::Global<v8::Value>>>) |
| `[[origin]]` | `Box<MessageEventState>::origin` (RefCell<String>) |
| `[[lastEventId]]` | `Box<MessageEventState>::last_event_id` (RefCell<String>) |
| `[[source]]` (always null) | (none) |
| `[[ports]]` (always empty) | (none) |

### X.3. CloseEvent

| Slot | Storage |
|------|---------|
| Inherited Event slots | `Box<CloseEventState>::event` (offset 0) |
| `[[wasClean]]` | `Box<CloseEventState>::was_clean` (Cell<bool>) |
| `[[code]]` | `Box<CloseEventState>::code` (Cell<u16>) |
| `[[reason]]` | `Box<CloseEventState>::reason` (RefCell<String>) |

## XI. Macro extensions required

Audit of `crates/runtime-macros/src/v8_class.rs` against this design.

| # | Need | Status |
|---|------|--------|
| 1 | `#[v8_inherit(EventTarget)]` for WebSocket | Already shipped (used by AbortSignal) |
| 2 | `#[v8_inherit(Event)]` for MessageEvent / CloseEvent | Already shipped (used by CustomEvent) |
| 3 | `#[v8_class]` constructor returning `Result<Self, OpError>` | Already shipped (used by Event, AbortSignal, Request, Response, …) |
| 4 | `#[v8_method]` returning `Result<(), OpError>` | Already shipped |
| 5 | `#[v8_getter] / #[v8_setter] / #[v8_name]` | Already shipped |
| 6 | EventHandler IDL attributes (onopen/onmessage/...) | NO new macro work — implemented as plain getter+setter pairs that delegate to the EventTarget listener list. Same pattern AbortSignal's `onabort` uses today. |
| 7 | `static` constants on the constructor (CONNECTING/OPEN/CLOSING/CLOSED) | NEW — small extension. Currently `Event` installs constants via a hand-rolled `install_event_constants` (`crates/runtime/src/dom/event.rs:409-434`). Either follow that pattern (hand-roll) OR add an `#[v8_constant]` attribute to the macro. Picking hand-roll for v1 (10 LOC; matches Event's pattern). |
| 8 | `Rc<RefCell<…>>` field access on boxed state | Pattern, not macro work — same as streams. |

**Summary: NO new macro extensions are required.** The hand-rolled
constants installer is a 10-LOC copy of `install_event_constants`. All
other surface is covered by existing macro features.

## XII. Test plan

### XII.1. WPT inventory (must-pass v1)

The WPT websockets directory inventory (verified against
https://github.com/web-platform-tests/wpt/tree/master/websockets,
2026-05-02):

**Top-level (~50 files):**
- `Close-*.any.js` (17 files) — close-code & reason validation, server-initiated close, etc.
- `Send-*.any.js` (~20 files) — string, binary, ArrayBuffer, ArrayBufferView (one per typed-array kind), Blob, 65K, 0-byte, before-open, null, paired/unpaired surrogates, unicode.
- `Create-*.any.js` (~15 files) — URL parse, scheme validation, fragment rejection, protocol token validation, blocked-port, http-urls, valid URL with binaryType.
- `bufferedAmount-unchanged-by-sync-xhr.any.js` — checks bufferedAmount is observable but not affected by sync XHR (irrelevant for our context — sync XHR is browser-only — but the bufferedAmount-observable check IS relevant). Adapt as a hand-written test.
- `binaryType-wrong-value.any.js` — `socket.binaryType = "unknown"` rejection (D-7 / D-18).
- `close-invalid.any.js` — close-code validation (D-8).
- `constructor.any.js` — constructor invariants.
- `eventhandlers.any.js` — onopen/onmessage/onerror/onclose IDL semantics.
- `idlharness.any.js` — `webidl-test-harness`-driven IDL surface check. Pass/fail per attribute / method / inheritance.
- `extended-payload-length.html` — sends 65K+ frames; tests the 64-bit length encoding. Adapt to .any.js if not already (the .html version requires a navigation context).
- `referrer.any.js` — OUT (Non-goal).
- `mixed-content.https.any.js` — OUT (Non-goal).
- `basic-auth.any.js` — basic-auth via URL credentials. Tentative scope; depends on cyper supporting URL credentials. Probably yes for v1.
- `back-forward-cache-*.window.js` (6 files) — OUT (Non-goal).
- `remove-own-iframe-during-onerror.window.js` — OUT (no iframes).
- `bufferedAmount-unchanged-by-sync-xhr.any.js` — OUT-AS-WRITTEN; adapt.
- `send-many-64K-messages-with-backpressure.any.js` — IN (D-15).

**Subdirectories:**
- `constructor/` (~18 .html files) — early constructor tests. Most are .html; v1 may skip the .html-only ones if they require document context, run only the .any.js variants.
- `closing-handshake/` — close protocol corner cases. IN.
- `binary/` — IN.
- `opening-handshake/` — IN.
- `interfaces/` — IDL test variants. IN.
- `keeping-connection-open/` — Test that `socket.send` while reading keeps socket open. IN.
- `security/` — port blocklist, scheme enforcement. IN.
- `cookies/` — OUT (Non-goal).
- `multi-globals/` — OUT (Non-goal).
- `unload-a-document/` — OUT (Non-goal).
- `stream/tentative/` — OUT (WebSocketStream — v2).
- `handlers/` — Server-side echo handlers. NOT tests — used by tests.
- `resources/` — Test utilities.

**Estimated WPT count:**
- Targetable v1: ~85 files (top-level minus OUT; constructor; closing-handshake; binary; opening-handshake; interfaces; keeping-connection-open; security)
- Out for v1: ~25 files (back-forward-cache, mixed-content, referrer, cookies, multi-globals, unload, WebSocketStream, navigable-context-only)

### XII.2. WPT extension to setup-wpt.sh

`crates/runtime/tests/setup-wpt.sh` does NOT currently include
`/websockets/` in its sparse-checkout list. The cutover Plan adds:

```bash
# Append to setup-wpt.sh sparse-checkout list:
    "/websockets/" \
```

This pulls ~3 MB of additional WPT files. Re-running `setup-wpt.sh`
post-merge populates the directory.

### XII.3. In-process echo server

Functional WPT tests need a real WebSocket server. The runtime already
ships `echo-server` (`crates/runtime/src/echo_server.rs`, declared in
`crates/runtime/Cargo.toml`). v1 extends it with WebSocket echo support
via `compio_ws::accept_async` — ~80 LOC addition. Test runner spawns
the echo server on `127.0.0.1:0`, captures the bound port, sets the
WPT `WSPORT` substitution variable, runs the test.

### XII.4. Hand-written tests

Per `crates/runtime/tests/` convention (one file per concern):

```
tests/websocket_construct.rs          — URL parse, protocol validation, fragment rejection
tests/websocket_send.rs               — Send dispatch order, types, bufferedAmount tracking
tests/websocket_close.rs              — close-code validation, wasClean, reason length cap
tests/websocket_events.rs             — MessageEvent / CloseEvent class semantics, instanceof
tests/websocket_pair.rs               — WebSocketPair coupling, accept(), peer-link routing
tests/websocket_e2e.rs                — End-to-end against in-process echo
tests/websocket_signal.rs             — D-24 AbortSignal extension
tests/wpt_websockets.rs               — WPT runner (constructor, close, send, binary subdirs)
```

Hand-written count target: ~80 unit-test functions across the 7 files,
matching the streams / fetch density.

### XII.5. WPT runner

Mirrors `wpt_streams_*.rs` / `wpt_fetch_*.rs` — `include_str!` the WPT
file (test files stay pristine), wrap in the WPT testharness shim, run
in V8. The runner targets each WPT subdirectory as a separate
`#[test]` to allow per-subdir pass/fail tracking in CI.

## XIII. Polyfill removal cadence (D-25)

Three landings:

### Landing 1 — feature-flagged native, polyfill default

- Add `crates/runtime/src/websocket/` module + `MessageEvent` /
  `CloseEvent` in `crates/runtime/src/dom/`.
- Behind `#[cfg(feature = "runtime_native_websocket")]` (default off):
  install native classes; route the gateway 101-handshake to the native
  WebSocketImpl.
- Polyfill (`embed/websocket.js` + `crates/runtime/src/websocket.rs`)
  remains the default — no behaviour change for existing apps.
- New WPT runner ships, gated on the feature flag; CI runs both code
  paths against WPT once cutover-1 lands.

### Landing 2 — flip default to native

- Default the feature on. Polyfill remains as fallback (turn off via
  `runtime_legacy_websocket` for emergency rollback).
- Rename `crates/runtime/src/websocket.rs` → `crates/runtime/src/websocket/legacy.rs`
  to mark it deprecated. Both modules coexist during this window.
- Refactor any callsite that depends on polyfill semantics (the gateway
  pump in particular — verify `crates/runtime/src/http.rs:171-179`
  still extracts `ws_id` correctly from the new native Response).
- Update `docs/reference/websocket-design.md` with a note pointing to
  this ADR.

### Landing 3 — delete polyfill

- Delete `crates/runtime/src/embed/websocket.js` (166 LOC).
- Delete `crates/runtime/src/websocket/legacy.rs` (the old `websocket.rs`,
  ~180 LOC).
- Delete the five `__ws*` callbacks from `crates/runtime/src/init.rs`
  install code.
- Remove the feature flag — native is the only implementation.
- File the ADR under `docs/decisions/`.

Total LOC removed in landing 3: ~350 LOC of polyfill (166 JS + ~180 Rust
callbacks).

## XIV. Implementation sequence

### XIV.1. Order

1. Land MessageEvent + CloseEvent native classes (parallel — no inter-deps).
2. Land WebSocketImpl `#[v8_class]` skeleton (constructor, all attributes,
   no network yet).
3. Land hand-rolled tests for construction / URL / protocols / close-code /
   binaryType.
4. Land WebSocket handshake.rs — RFC 6455 §4.1 client-side.
5. Land receive_loop.rs + send_pump.rs.
6. Land WebSocketPair mint helper + JS install.
7. Refactor http.rs::inspect_response to read native WebSocket via
   the native class layout (vs the polyfill's `_id` field).
8. WPT runner — start with constructor.any.js, expand to close, send, binary.
9. End-to-end with in-process echo server.
10. D-25 cutover landings 1, 2, 3.

### XIV.2. Hours (industry-inflated for ADR provenance)

| Step | Hours |
|------|------:|
| 1. MessageEvent + CloseEvent native classes (~270 LOC + tests) | 8 |
| 2. WebSocketImpl skeleton (no network) (~600 LOC) | 16 |
| 3. Hand-rolled construction/close/binaryType tests (~400 LOC) | 12 |
| 4. handshake.rs (RFC 6455 §4.1, ~200 LOC) + sha1/key helpers | 10 |
| 5. receive_loop.rs + send_pump.rs (~300 LOC) | 14 |
| 6. WebSocketPair (~120 LOC) + gateway-coupling refactor | 8 |
| 7. http.rs inspect_response refactor (~50 LOC change) | 4 |
| 8. WPT runner + sparse-checkout extension + 3-4 WPT subdirs | 24 |
| 9. End-to-end echo + 5 hand-rolled e2e tests | 10 |
| 10. D-25 cutover landings 1+2+3 (3 commits over 2 weeks) | 12 |
| **Total** | **118 industry-hours** |

User productivity is industry/40 per `feedback_estimates_hours_not_weeks.md`,
so ~3 user-hours of focused work. The 118-hour figure is the ADR-style
estimate for cross-team comparison; if a follow-up agent picks this up
on a freshly-spun cluster, plan for ~3-4 days at industry pace.

## XV. Comparison with reference implementations

| Impl | LOC | Approach |
|------|----:|----------|
| **undici WebSocket** | 2,715 (JS, 9 files) | Pure JS on top of Node net/tls. Hand-rolled RFC 6455 framer (`receiver.js` 490 + `frame.js` 127 + `sender.js` 109). `permessage-deflate` ships. |
| **workerd WebSocket** | 2,024 (C++, 2 files) | Pure C++ on top of KJ HTTP. Frame phase delegated to `kj::WebSocket` (KJ's own framer). Hibernation extensions add ~600 LOC on top. |
| **Deno WebSocket** | 2,186 (Rust+JS, 3 files) | Hybrid: 884 LOC Rust + 775 LOC client JS + 527 LOC WebSocketStream JS. Frame phase via `fastwebsockets` (separate crate, ~3K LOC). HTTP/2 RFC 8441 path included. |
| **Current `embed/websocket.js`** (zeroship polyfill) | 166 (JS only) | JS class surface; client-side not implemented; `WebSocketPair` only. |
| **Current `crates/runtime/src/websocket.rs`** (zeroship Rust callbacks) | 183 (5 V8 callbacks) | Backing for the polyfill; queues messages on per-WS VecDeques. |
| **This design (estimated)** | ~2,200 (Rust, 8 files in `websocket/`) + ~270 LOC (MessageEvent + CloseEvent in `dom/`) | Pure native via `compio-ws` framer (already a transitive dep — no new wire-protocol code in-tree). Same depth as workerd; smaller than undici because we delegate framing. |

Roughly the size of workerd's implementation, but with fewer
extension surfaces (no hibernation, no HTTP/2). Smaller-scope spec
than streams or fetch.

## XVI. Dependencies satisfied

- **streams native:** WebSocket doesn't expose a stream by default in v1
  (WebSocketStream is the streams-based API; deferred). But
  `Blob.stream()` is needed for `binaryType="blob"` interop with user
  code that wants to consume the blob as a ReadableStream. Already
  shipped in streams-native + blob-native.
- **EventTarget native:** shipped (`crates/runtime/src/dom/event_target.rs`).
- **Event native:** shipped (`crates/runtime/src/dom/event.rs`).
- **`#[v8_inherit(EventTarget)]` macro:** shipped (used by AbortSignal).
- **`#[v8_inherit(Event)]` macro:** shipped (used by CustomEvent).
- **AbortSignal native:** shipped — used for D-24 `signal` extension.
- **URL native (ada-url backed):** shipped — used by constructor URL parse.
- **Blob native:** shipped — used for `binaryType="blob"` MessageEvent
  data.
- **ArrayBuffer/Uint8Array native (V8 built-ins):** shipped.
- **DOMException** (in flight via fetch-js-delete agent) — used for
  close-code validation errors. Until DOMException ships natively, the
  existing JS DOMException shim in `embed/fetch.js` is reused (the
  v1 cutover landings can finalise once DOMException-native lands; in
  the interim, the close-code-validation throws via the existing shim
  with `.name = "InvalidAccessError"` — matches the spec observable).
- **compio-ws 0.3.1 (tungstenite):** transitive workspace dep
  (`Cargo.lock:608`); promoted to a direct `crates/runtime/Cargo.toml`
  dep in landing 1.
- **sha1:** workspace dep already (`crates/runtime/Cargo.toml`).
- **base64:** workspace dep already.
- **aws_lc_rs::rand::fill:** already in use for crypto.getRandomValues.

## XVII. Open questions

The list of items where the design has to make a policy call. None
require user input to PROCEED — the design has picked an answer for
each — but each is flagged for cross-checking.

### XVII.1. WebSocketStream API scope

**Picked:** OUT for v1, IN for v2 (per Non-goals).

**Why:** The newer streams-based WebSocket API (Chromium ships behind
`--enable-experimental-web-platform-features`) is not yet in any browser
release. WPT tests live in `stream/tentative/`. v1 ships the older
WebSocket interface (the one every existing library uses). v2 layers
WebSocketStream on top.

**Risk:** AI-builder apps generated via the agent may reference
WebSocketStream (LLM training data sees it as "the modern API"). v1
returns ReferenceError when a user references it. Mitigation: provide
a clear error message AND a hint in the AGENTS.md surface that
`new WebSocket(...)` is the v1 API.

### XVII.2. permessage-deflate extension

**Picked:** OUT for v1.

**Why:** Adds ~400 LOC + a flate2 dependency surface in the per-message
path. AI-builder apps rarely benefit (most messages are small JSON
diffs; compression adds CPU > savings). undici ships it; workerd ships
it; v2 will ship it (the extension surface is well-defined; tungstenite's
deflate variant is well-tested). v1 advertises NO extensions on the
client handshake.

### XVII.3. AbortSignal extension on the constructor

**Picked:** IN for v1 (D-24).

**Why:** Near-zero implementation cost (AbortSignal is wired up for
fetch); high creator-app demand (every undici-based library uses
AbortSignal); workerd ships it (verified `web-socket.h:303` accepts
`AcceptOptions` for server-side, but the client constructor pattern
for AbortSignal is a community extension undici endorses).

The IDL extension is non-spec; document loudly in the runbook so
creators know they're using a non-standard feature.

### XVII.4. Server-side WebSocket (gateway accept) coexistence

**Picked:** Coexist — preserve `WebSocketPair` workerd semantics. See §VI.

**Why:** Switching the gateway-side flow to a different paradigm
(e.g. `Deno.upgradeWebSocket(req)`) would break every existing creator
app. Both flows land on the same `#[v8_class] WebSocket`; the JS-visible
surface is unified. The `docs/reference/websocket-design.md` reference
doc remains the authority on the gateway flow.

### XVII.5. MessageEvent shape — full HTML §9.4.2 vs WebSocket-subset

**Picked:** Full HTML §9.4.2 IDL surface, but `source` always null and
`ports` always empty FrozenArray.

**Why:** The IDL surface MUST be there for `instanceof MessageEvent`
to mean what users expect. The polyfill's "plain Event with expandos"
fails this. Implementing `source` and `ports` requires MessagePort
which we don't ship; v1 returns null/empty, which is observable but not
a spec violation (the IDL types are nullable / sequence).

### XVII.6. CloseEvent code default — 0 vs 1005

**Picked:** Default 0 per IDL (`unsigned short code = 0`); CONNECTING-
to-CLOSING transitions where no Close frame was received yield code
1006 ("Abnormal Closure"); peer-sent Close with no code yields 1005
("No Status Rcvd"). Spec §3.2 is explicit on these two internal codes.

**Why:** Spec compliance verbatim.

### XVII.7. The "ErrorEvent vs plain Event" question

**Picked:** Plain Event (D-26).

**Why:** The spec §3.1 step "fire a connection-failed event" uses a plain
Event, NOT ErrorEvent (which is HTML §10.7.5 and has a different shape).
workerd ships ErrorEvent as an extension; we don't, on grounds of
spec-fidelity. Creator apps that read `e.message` on the error event get
`undefined` — same as today's polyfill.

### XVII.8. cyper Upgrade API gap

**Picked:** Bypass cyper for WebSocket; use compio-ws directly (D-12).

**Why:** cyper 0.8 doesn't expose `Upgraded`. Rewrapping cyper to add it
is out of scope for this design. compio-ws is already a transitive dep
and is the workhorse for the wire protocol.

**Future:** when cyper grows an Upgrade API (or when we move to HTTP/2
via cyper feature flip), revisit. The IDL surface and the V8 class shape
are unaffected.

### XVII.9. UTF-8 strictness on send vs receive

**Picked:** Send accepts any string (V8 strings can be lone surrogates;
WebIDL USVString conversion replaces them with U+FFFD). Receive enforces
strict UTF-8 (D-17 — RFC 6455 §8.1).

**Why:** Spec on each side. WPT
`Send-paired-surrogates.any.js` and `Send-unpaired-surrogates.any.js`
test the send-side conversion (USVString replaces lone surrogates with
U+FFFD); the receive side is in `interfaces/` and the autobahn fuzzer.

### XVII.10. The 1024-cap on concurrent WebSockets

**Picked:** 1024 per-isolate (D-23). Cap-overflow is an async
connection failure (Error + Close{1006, was_clean: false}), NOT a
constructor throw. (addresses critic CRITICAL #10)

**Why:** Memory-bound (16MB per app at full saturation). Matches the
existing `MAX_PENDING_OPS` order. WHATWG §3.1 frames the entire
establish-a-WebSocket-connection algorithm as "in parallel"; budget
failure is no different from any other connection-failed path —
including DNS failure, TCP refused, TLS handshake error, or 401 on
upgrade — all of which surface via the §4 "feedback from the protocol"
event pair (error → close). A constructor throw would force creator
apps to write try/catch around every `new WebSocket(...)`; the
async-fail pattern lets `socket.onerror` / `socket.onclose` handle it
uniformly.

**Per-account rate limiting:** the per-isolate cap protects within a
single tenant; the gateway's existing CHWBL routing layer holds the
cross-tenant budget (per `crates/gateway/src/dispatch.rs`). The cap
above is a runtime self-defence number; the gateway-side per-account
budget is a separate concern handled in §XVII.11.

## XVIII. Summary

A native WHATWG WebSocket implementation rooted in the existing
DOM (Event, EventTarget, AbortSignal) and Streams (Blob) primitives,
with the wire protocol delegated to `compio-ws` (already a transitive
dep). Replaces a 166-LOC JS polyfill that was server-side-only and
silently corrupted binary data. The native shape unifies the
client-side `new WebSocket(url)` path with the existing server-side
`WebSocketPair` (Cloudflare-Workers-compatible) path on a single
`#[v8_class] WebSocketImpl`. Three new V8 classes ship: `WebSocket`,
`MessageEvent`, `CloseEvent`. No new macro extensions are required.

Estimated implementation cost: 118 industry-hours (~3 user-hours
focused) across 10 ordered steps; three-landing polyfill cutover
mirrors the streams and fetch patterns. WPT target: ~85 of ~110
files in `websockets/`, with the genuine browser-only / WebSocketStream
files deferred. The design's storage shapes are forward-compatible
with the v2 WebSocketStream layer.

The single-source slot rule, EventTarget mixin pattern, and `#[v8_inherit]`
macro keep this design's complexity low: this is a smaller-scope
WHATWG primitive than streams or fetch, and the existing DOM
foundation does most of the heavy lifting.
