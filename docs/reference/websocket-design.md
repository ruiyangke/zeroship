# WebSocket Runtime

Current zeroship WebSocket behavior, as implemented in [`crates/runtime/src/web/websocket/`](../../crates/runtime/src/web/websocket/).

## Scope

The runtime now ships a native WebSocket implementation. The old JS polyfill is gone.

Two surfaces matter:

- client sockets: `new WebSocket(url, protocols?, init?)`
- server-side upgrades: `WebSocketPair` plus `Response { status: 101, webSocket }`

## Public API

### `WebSocket`

Implemented in [`mod.rs`](../../crates/runtime/src/web/websocket/mod.rs).

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

Implemented in [`pair.rs`](../../crates/runtime/src/web/websocket/pair.rs).

- `new WebSocketPair()` returns an object with `0` and `1` properties.
- pair sockets start in `CONNECTING`.
- `accept()` is required before a pair socket begins local message delivery.
- calling `accept()` on a client-created socket throws.

## Upgrade path

The HTTP upgrade handoff is:

1. JS returns `new Response(null, { status: 101, webSocket: client })`.
2. [`crates/runtime/src/transport/handler.rs`](../../crates/runtime/src/transport/handler.rs) extracts the native `ws_id`.
3. The runtime converts that to `FetchOutcome::WebSocketUpgrade`.
4. The kernel-side server path completes the wire handshake and pumps frames.

This is also the mechanism used by the bootstrap subscription fallback in [`crates/runtime/src/core/init.rs`](../../crates/runtime/src/core/init.rs).

## Transport architecture

### Client handshake

[`handshake.rs`](../../crates/runtime/src/web/websocket/handshake.rs) performs the RFC 6455 client handshake directly:

- URL validation mirrors fetch's SSRF checks
- `Sec-WebSocket-Accept` is recomputed and verified
- echoed subprotocols must have been offered
- non-empty `Sec-WebSocket-Extensions` is rejected
- permessage-deflate is not negotiated

The handshake also returns any bytes pipelined after the `101` response so the frame reader does not lose the first frame.

### Runtime event flow

[`network.rs`](../../crates/runtime/src/web/websocket/network.rs) owns the per-socket state and queues `WsEvent`s. Each queued event triggers `OpResult::WebSocketEvent { ws_id }`; [`dispatch.rs`](../../crates/runtime/src/web/websocket/dispatch.rs) drains the queue on the V8 thread and dispatches native `open`, `message`, `error`, and `close` events.

For `ws://`, reads and writes run as separate compio tasks on the same `TcpStream`. For `wss://`, the runtime uses a single-owner loop because `TlsStream` is not aliasable.

### Pair sockets

`WebSocketPair` does not use the network stack. [`pair.rs`](../../crates/runtime/src/web/websocket/pair.rs) moves queued frames directly onto the peer's event queue and reuses the same native dispatch path.

## Subscription fallback

The bootstrap's fallback subscription transport currently uses `WebSocketPair` and the `zs.v1` subprotocol:

- client must send a `hello` frame within 5 seconds
- server sends `ping` every 30 seconds
- missing `pong` for 60 seconds closes the socket
- streamed frames are JSON envelopes carrying `data`, `error`, or `end`

See [`crates/runtime/src/core/init.rs`](../../crates/runtime/src/core/init.rs) for the exact wire behavior.
