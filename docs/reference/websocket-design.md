# WebSocket Design

**Date:** 2026-04-09
**Status:** Draft

## Scope

Server-side WebSocket: JS handler receives upgrade request, returns `Response { status: 101, webSocket }`. Client-side WebSocket (`new WebSocket(url)`) is out of scope for v1.

## JS API (Cloudflare Workers compatible)

```js
export function onRequest(req) {
    if (req.headers.get('upgrade') !== 'websocket') {
        return new Response('Expected WebSocket', { status: 426 });
    }
    
    const [client, server] = Object.values(new WebSocketPair());
    server.accept();
    server.addEventListener('message', event => {
        server.send('Echo: ' + event.data);
    });
    server.addEventListener('close', event => {
        console.log('WebSocket closed:', event.code);
    });
    
    return new Response(null, { status: 101, webSocket: client });
}
```

## Types

- `WebSocketPair` — constructor returns object with `0` and `1` properties (two connected WebSockets)
- `WebSocket` — has `send(data)`, `close(code, reason)`, `accept()`, `addEventListener(type, handler)`
- `MessageEvent` — has `data` property (string or ArrayBuffer)
- `CloseEvent` — has `code` and `reason` properties

## Architecture

```
Client TCP → compio TcpStream
  → httparse detects Upgrade: websocket
  → route to onRequest handler (JS)
  → JS creates WebSocketPair → two WebSockets connected via shared buffer
  → JS calls server.accept() → starts read loop
  → JS returns Response { status: 101, webSocket: client }
  → Runtime detects status 101:
    1. Perform WebSocket handshake (compio-ws accept_async)
    2. Couple "client" WebSocket to the real TCP WebSocket stream
    3. Bidirectional pump: TCP ↔ shared buffer ↔ JS callbacks
```

## Implementation layers

### 1. JS polyfill (`embed/websocket.js`)

WebSocketPair, WebSocket (extends EventTarget), MessageEvent, CloseEvent — pure JS using shared buffers backed by native callbacks.

### 2. Native callbacks

- `__wsAccept(ws_id)` — start the read loop for this WebSocket
- `__wsSend(ws_id, data)` — queue a message for sending
- `__wsClose(ws_id, code, reason)` — initiate close handshake

### 3. Runtime integration

- When `dispatch_http` gets a Response with status 101 + webSocket property, return `DispatchOutcome::WebSocketUpgrade { ws_id }`
- The connection handler performs the compio-ws handshake on the raw TcpStream
- A pump future reads from the TCP WebSocket and delivers messages to JS via the shared buffer
- Another pump future drains the JS send queue to the TCP WebSocket

### 4. Shared buffer (per WebSocket pair)

Each WebSocketPair creates a bidirectional channel in RuntimeState:

```
RuntimeState:
  websockets: HashMap<u32, WebSocketState>

WebSocketState:
  incoming: VecDeque<WsMessage>     ← TCP → JS (pump writes, JS reads via onmessage)
  outgoing: VecDeque<WsMessage>     ← JS → TCP (JS send() writes, pump reads)
  accepted: bool
  closed: bool
  pending_message_resolver: Option<Global<PromiseResolver>>  ← wakes JS when message arrives
```
