# Native WHATWG WebSocket shipped

**Status:** Shipped 2026-05-02
**Long-form design:** [`docs/proposals/websocket-native.md`](../archive/websocket-native.md)
**Companion reference doc:** [`docs/reference/websocket-design.md`](../reference/websocket-design.md)
**Implementation:** [`crates/runtime/src/web/websocket/`](../../crates/runtime/src/web/websocket/) (handshake, frame_reader, frame_writer, dispatch, pair)

## Context

The previous implementation routed WebSocket through a JS polyfill
plus ad-hoc V8 callbacks. v1 of the proposal scored 56/100 with 10
CRITICAL spec violations: wrong `send()` type-dispatch order, fake
`[Clamp]` conversion, unconditional `Origin` header, no
CONNECTING/OPEN split on `close()`, wrong abort-during-CONNECTING
event order, 1005 encoded on the wire (RFC 6455 §7.4.1 forbids),
etc. v2 fixed every CRITICAL/MAJOR against the actual specs (WHATWG
WebSockets, RFC 6455, WebIDL).

## Decision

- Pure-native `WebSocket` class on RFC 6455 wire format, no tokio.
- WHATWG §3.1 `send()` type-dispatch: String → Blob → ArrayBuffer → ArrayBufferView, in that order.
- WebIDL `[Clamp]` conversion via explicit `clamp_unsigned_short` helper (NaN → 0, sign-aware clamp, round-half-to-even).
- `Origin` header sent only when the user explicitly sets it via `init` (RFC 6455 §10.2 — non-browser clients SHOULD NOT send Origin).
- CONNECTING-state `close()` "fails the connection" (no wire frame); OPEN-state enqueues a Close frame.
- AbortSignal-during-CONNECTING fires `error` then `close` (WHATWG §4 connection-failed).
- Close frame default does not encode 1005 (RFC 6455 §7.4.1).
- `WebSocketPair` is fully native (no JS bridge).

## Consequences

- The companion reference doc at `docs/reference/websocket-design.md` documents the user-facing API; the proposal stays as design history.
- Gateway upgrade plumbing routes to native pair (`70b856d`).
- Post-ship fix: `163b248` serve: fix WS echo data loss in gateway upgrade pump.

## See also

- Implementing commits: `0e76305` receive loop, send pump, event dispatch; `3d6928a` native WebSocketPair; `abe72a9` cutover landing 2 (native default); `9835173` cutover landing 3 (delete polyfill); `70b856d` gateway upgrade routes to native pair; `ea615a8` Merge feature/websocket-impl.
