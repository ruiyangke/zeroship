# WebSocket Runtime

The runtime exposes a standards-based `WebSocket` global: the browser WebSocket
API shape (defined by the WHATWG standards body) speaking the RFC 6455 wire
protocol. Creator code uses two surfaces:

- **client sockets** opened with `new WebSocket(url, ...)`, and
- **server-initiated upgrades** through `WebSocketPair` plus
  `Response { status: 101, webSocket }`.

This page describes the calls a creator makes, their defaults and limits, the
shape of the errors they see, and where each surface is served.

Client sockets work everywhere your app runs. Server-initiated upgrades are
completed only by the single-tenant local dev server today; a deployed app's
gateway does not proxy an upgraded connection. See "Where sockets are served".

## Client sockets

### `new WebSocket(url, protocols?, init?)`

`url` is required. It must use a `ws:` or `wss:` scheme — `http:` and `https:`
are normalized to `ws:` and `wss:` respectively — and it must not contain a
fragment. A missing `url`, a `url` that does not parse, a scheme that is not
`ws:`/`wss:` after normalization, or a fragment all throw a `TypeError`.

`protocols` is an optional string or array of subprotocol strings. Each must be
a non-empty token with no duplicate entries. An invalid or duplicated
subprotocol, or a value that is neither a string nor a sequence of strings,
throws a `TypeError`.

The optional `init` dictionary accepts:

- `origin` — the `Origin` header sent on the connection. It is sent only when
  you set it here; otherwise no `Origin` header is sent. There is no allow-list
  on the creator's side: the value you set is sent as-is, and whether the server
  accepts it is the server's decision.
- `maxMessageSize` — the largest message the client will accept, in bytes.
  Default `4194304` (4 MiB).
- `maxFrameSize` — the largest single frame the client will accept, in bytes.
  Default `1048576` (1 MiB).
- `pingIntervalMs` — how often the client sends a keepalive ping, in
  milliseconds. Default `30000` (30 s).

### Properties and events

A socket exposes `url`, `readyState`, `bufferedAmount`, `protocol`, `extensions`
and `binaryType`. `binaryType` defaults to `"blob"` and also accepts
`"arraybuffer"`; assigning any other value is a silent no-op that keeps the
current value. `binaryType` controls the type of the `data` field on incoming
binary `message` events: `"blob"` delivers a `Blob`, `"arraybuffer"` an
`ArrayBuffer`. Text messages always deliver a string.

`readyState` is one of `WebSocket.CONNECTING` (0), `OPEN` (1), `CLOSING` (2) or
`CLOSED` (3).

Events arrive through the `onopen`, `onmessage`, `onerror` and `onclose`
handlers, or through `addEventListener`. Their payload shapes are:

- `open` — a plain `Event`, no data.
- `message` — a `MessageEvent`. `data` is a string for text frames, and a
  `Blob` or `ArrayBuffer` for binary frames depending on `binaryType`. `origin`
  is also present.
- `error` — a plain `Event` with **no** payload: it carries no message, code or
  reason. The only channel for detail is the `close` event that follows.
- `close` — a `CloseEvent` with `code`, `reason` and `wasClean`.

### `send(data)`

`send()` accepts strings, `Blob`, `ArrayBuffer` and `ArrayBufferView`. Calling
it while the socket is still `CONNECTING` throws; the caught error's message
begins with `InvalidStateError`. After the socket has entered `CLOSING` or
`CLOSED` it is a no-op. Queued bytes accumulate in `bufferedAmount`.

Once the queue reaches the 16 MiB cap, further sends are dropped silently: the
call returns normally, no event fires, and `bufferedAmount` stops rising and
holds. Sending resumes once the queue drains below half the cap.

### `close(code?, reason?)`

`close()` starts the closing handshake. When given, `code` must be `1000` or a
value from `3000` to `4999`; any other value throws, and the caught error's
message begins with `InvalidAccessError`. `reason` must be no more than 123
bytes when UTF-8 encoded; longer throws with a message beginning `SyntaxError`.

Calling `close()` on a still-`CONNECTING` socket fails the connection rather
than sending a close frame: the socket fires `error`, then `close` with code
`1006` and `wasClean` `false`.

## Server upgrades: `WebSocketPair`

`new WebSocketPair()` returns an object with a `0` and a `1` property, each a
WebSocket. The `0` socket is the one you hand back to the client; the `1` socket
is the one your code holds. Both start in `CONNECTING`. Call `accept()` on the
socket your code holds before it starts delivering messages to your handlers;
`accept()` takes no arguments and returns nothing. `accept()` on a
client-created socket throws a `TypeError`.

## Upgrade path

To upgrade an incoming request to a WebSocket, return a Response whose
`webSocket` carries the client socket from the pair:

```ts
const { 0: client, 1: server } = new WebSocketPair();
server.accept();
return new Response(null, { status: 101, webSocket: client });
```

This upgrade is completed to the connecting client only in single-tenant local
dev. Where this fits in a deployed app, see "Where sockets are served".

## Outbound sockets and egress

An outbound `new WebSocket(url)` is governed by the same egress rules as every
other raw byte stream your app can open. A rule is an accept or reject verdict
for a destination (a DNS name or an address range in CIDR form) plus a port; a
rule that accepts a host and port admits that destination, and an app with no
matching accept rule opens no outbound socket at all. `fetch()` is the only
outbound path with no egress rule at all.

A refusal fires an `error` event (which, as above, carries no detail) and then a
`close` event with code `1006`; the close `reason` is the refusal, written as a
code and message. `ERR_NET_SSRF` names the platform-wide server-side
request-forgery (SSRF) floor that blocks private and reserved addresses;
`ERR_NET_EGRESS_DENIED` names a refusal by your app's own rules.

## Client handshake

When `new WebSocket(url)` connects, the client speaks RFC 6455 directly:

- the server's `Sec-WebSocket-Accept` is recomputed and verified;
- a subprotocol the server echoes must have been offered by the client;
- the server must not return a non-empty `Sec-WebSocket-Extensions`;
- `permessage-deflate` (the compression extension) is not negotiated.

If any check fails, the connection fails the same way a network error does: the
socket fires `error`, then `close` with code `1006` and `wasClean` `false`.

## Where sockets are served

The two surfaces are served differently today, and the difference is worth
knowing before you build around a socket.

- **Client sockets** (`new WebSocket(url)`) are opened by your app's own code
  and work everywhere your app runs, subject only to the egress rules above.
- **Server-initiated upgrades** (`WebSocketPair` + `Response { status: 101,
  webSocket }`) are completed only by the single-tenant local dev server, which
  speaks WebSocket directly to the connecting client. A deployed app is fronted
  by a gateway that does not proxy an upgraded connection, so an upgrade your
  handler returns is not served there; the upgrade attempt is answered with an
  HTTP `500` instead of a live socket.

Plan server-driven sockets for local development, or open the connection from
the client with `new WebSocket(url)` where it must work in a deployed app.

## Authentication

A WebSocket upgrade request passes through the same authentication as any other
request into your app, so a route that requires a signed-in user is not upgraded
for a caller the platform cannot authenticate.

A browser socket opened from your app's own origin authenticates on the session
cookie, which the browser attaches automatically — the same `HttpOnly` cookie
that authorizes your RPC and API calls. A socket opened from a different origin
does not send that cookie, and the browser holds no token it could offer in the
subprotocol handshake, so it arrives without an identity. A route declared
`auth: "user"` (the fail-closed default for procedures) refuses such a caller
rather than serving it silently. Keep browser sockets same-origin; anything
cross-origin that must authenticate should go through a same-origin endpoint on
your app.