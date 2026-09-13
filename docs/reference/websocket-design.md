# WebSocket Runtime

Current zeroship WebSocket behavior, as implemented in [`crates/zeroship-runtime/src/web/websocket/`](../../crates/zeroship-runtime/src/web/websocket/).

## Scope

The runtime now ships a native WebSocket implementation. The old JS polyfill is gone.

Two surfaces matter:

- client sockets: `new WebSocket(url, protocols?, init?)`
- server-side upgrades: `WebSocketPair` plus `Response { status: 101, webSocket }`

## Public API

### `WebSocket`

Implemented in [`mod.rs`](../../crates/zeroship-runtime/src/web/websocket/mod.rs).

- constructor: `new WebSocket(url, protocols?, init?)`
- getters: `url`, `readyState`, `bufferedAmount`, `protocol`, `extensions`, `binaryType`
- event handlers: `onopen`, `onmessage`, `onerror`, `onclose`
- methods: `send(data)`, `close(code?, reason?)`

Current behavior:

- `binaryType` defaults to `"blob"` and also accepts `"arraybuffer"`.
- `send()` accepts strings, `Blob`, `ArrayBuffer`, and `ArrayBufferView`.
- user-facing `close()` only accepts code `1000` or `3000-4999`, with a UTF-8 reason capped at 123 bytes.
- `WebSocketInit` currently supports `origin`, `maxMessageSize`, `maxFrameSize`, and `pingIntervalMs`.

### `WebSocketPair`

Implemented in [`pair.rs`](../../crates/zeroship-runtime/src/web/websocket/pair.rs).

- `new WebSocketPair()` returns an object with `0` and `1` properties.
- pair sockets start in `CONNECTING`.
- `accept()` is required before a pair socket begins local message delivery.
- calling `accept()` on a client-created socket throws.

## Upgrade path

The HTTP upgrade handoff is:

1. JS returns `new Response(null, { status: 101, webSocket: client })`.
2. [`crates/zeroship-runtime/src/transport/handler.rs`](../../crates/zeroship-runtime/src/transport/handler.rs) extracts the native `ws_id`.
3. The runtime converts that to `FetchOutcome::WebSocketUpgrade`.
4. The kernel-side server path completes the wire handshake and pumps frames.

RPC subscriptions use a native upgrade path and do not construct a creator
`Response`; see
[`rpc/subscription.rs`](../../crates/zeroship-runtime/src/rpc/subscription.rs).

## Transport architecture

### Client handshake

[`handshake.rs`](../../crates/zeroship-runtime/src/web/websocket/handshake.rs) performs the RFC 6455 client handshake directly:

- `Sec-WebSocket-Accept` is recomputed and verified
- echoed subprotocols must have been offered
- non-empty `Sec-WebSocket-Extensions` is rejected
- permessage-deflate is not negotiated

The handshake also returns any bytes pipelined after the `101` response so the frame reader does not lose the first frame.

### Outbound WebSocket is gated by the app's egress rules

An outbound `new WebSocket(url)` is a raw bidirectional byte stream the moment the upgrade completes, so it is subject to the app's egress rule set exactly as `node:net` is, and through the same evaluator ([`transport/egress.rs`](../../crates/zeroship-runtime/src/transport/egress.rs)). One rule set covers both: a rule accepting `api.example.com:443` admits that destination over either transport, and an app that holds no accept rule opens no WebSocket at all.

The connect task resolves the verdict before the handshake runs and hands `run_handshake` an address that is already authorized; the handshake makes no policy decision of its own. A refusal fires `error` and then `close` with code `1006`; because the `error` event carries no payload by spec, the reason on the close event names which check refused — `ERR_NET_SSRF` for the platform SSRF floor, `ERR_NET_EGRESS_DENIED` for the app's own rules. These are the same codes `node:net` reports, from the same classifier.

`fetch` remains ungated. It is the only outbound path an app can use with no rule at all. Writing egress rules: [`control.md`](control.md).

### Runtime event flow

[`network.rs`](../../crates/zeroship-runtime/src/web/websocket/network.rs) owns the per-socket state and queues `WsEvent`s. Each queued event triggers `OpResult::WebSocketEvent { ws_id }`; [`dispatch.rs`](../../crates/zeroship-runtime/src/web/websocket/dispatch.rs) drains the queue on the V8 thread and dispatches native `open`, `message`, `error`, and `close` events.

For `ws://`, reads and writes run as separate compio tasks on the same `TcpStream`. For `wss://`, the runtime uses a single-owner loop because `TlsStream` is not aliasable.

### Pair sockets

`WebSocketPair` does not use the network stack. [`pair.rs`](../../crates/zeroship-runtime/src/web/websocket/pair.rs) moves queued frames directly onto the peer's event queue and reuses the same native dispatch path.

## Subscription transport

The native RPC subscription transport uses the `zs.v1` subprotocol:

- client must send a `hello` frame before `HELLO_TIMEOUT`
- server sends `ping` on `PING_INTERVAL`
- missing `pong` at `PONG_TIMEOUT` closes the socket
- streamed frames are JSON envelopes carrying `data`, `error`, or `end`

Rust resolves the string-keyed procedure, retains the returned iterator and
pulls it under the captured request context. Disconnect closes the session and
calls the iterator's `return()` method. See
[`crates/zeroship-runtime/src/rpc/subscription.rs`](../../crates/zeroship-runtime/src/rpc/subscription.rs)
for the exact wire behavior.

## Where sockets are served

Single-tenant `zeroship serve` speaks WebSocket directly.

Behind the multi-node gateway, **subscription routes return `501 Not
Implemented`** - the gateway does not proxy an upgraded connection today. This
is why [`rpc.md`](rpc.md) lists `subscription` as not part of the public client
surface and points you at `stream(...)` for live feeds that ship.

## Authentication

A WebSocket upgrade goes through the same gateway auth gate as any other
request: the upgrade check only rejects non-upgrade traffic on a socket route,
and the request then reaches `resolve_auth` exactly as an HTTP call would. So a
socket opened from your own app's origin authenticates on the session cookie,
which the browser attaches automatically.

**A cross-origin socket cannot authenticate today.** The cookie is not sent
cross-origin, and there is no alternative credential: under the BFF model the
browser never holds a token it could offer in a subprotocol. This is a
consequence of that design rather than a gap in the socket implementation, and
nothing in the platform will report it as an auth failure - the socket simply
arrives unauthenticated, and an `auth: "user"` procedure behind it refuses.

If you need a socket from another origin, put it behind your own same-origin
endpoint rather than pointing a browser at the app directly.
