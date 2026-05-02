(function(globalThis) {
"use strict";

// Per docs/proposals/websocket-native.md D-25 cutover: when the native
// WebSocket impl is feature-flagged ON, `globalThis.WebSocket` was
// already installed by `crate::websocket_native::install_global` BEFORE
// this polyfill ran. Detect via the constructor's prototype having a
// Symbol.toStringTag of "WebSocket" — the polyfill sets a function
// prototype with no toStringTag. If the native is present, skip the
// polyfill's class definitions but still install the per-instance
// __wsRegistry hooks for the existing WebSocketPair gateway path.
var __nativeWebSocket = (
    typeof globalThis.WebSocket === "function"
    && globalThis.WebSocket.prototype
    && globalThis.WebSocket.prototype[Symbol.toStringTag] === "WebSocket"
);

// WebSocket states
var CONNECTING = 0, OPEN = 1, CLOSING = 2, CLOSED = 3;

// Global registry so native code can find WebSocket objects by ID
var __wsRegistry = Object.create(null);

// MessageEvent / CloseEvent are now NATIVE classes installed by
// `dom::install_globals` (see `dom/message_event.rs`,
// `dom/close_event.rs`). The polyfill below uses the native
// constructors so dispatched events pass `instanceof MessageEvent`
// and `instanceof CloseEvent` correctly. The previous expando-based
// builders broke `instanceof` and silently corrupted `data` identity.
function makeMessageEvent(type, init) {
    return new MessageEvent(type, init);
}

function makeCloseEvent(type, init) {
    return new CloseEvent(type, init);
}

// WebSocket subclasses EventTarget via direct prototype delegation —
// the prototype chain reads the LIVE `EventTarget.prototype` at this
// load time, so as long as `websocket.js` runs AFTER `install_dom`,
// it picks up the native EventTarget prototype. The constructor uses
// `Reflect.construct(EventTarget, [], WebSocket)` to obtain a real
// EventTarget instance with internal field 0 wired up — exactly what
// the native `addEventListener` / `dispatchEvent` callbacks require.
function WebSocket(url) {
    // Acquire a real EventTarget-shaped instance whose prototype is
    // WebSocket.prototype. `new EventTarget()` builds the wrapper
    // with internal-field 0 backed by a fresh `Box<EventTarget>`;
    // `Reflect.construct` then re-points the prototype to
    // WebSocket.prototype so `instanceof WebSocket` still works.
    // Equivalent to ES6 `super()` without needing class syntax
    // (this file is ES5 for legacy V8 compat).
    var self = Reflect.construct(EventTarget, [], WebSocket);
    self.url = url || "";
    self.readyState = CONNECTING;
    self.protocol = "";
    self.extensions = "";
    self.binaryType = "arraybuffer";
    self._id = 0;  // set by native after accept
    self._sendQueue = [];
    return self;
}
WebSocket.prototype = Object.create(EventTarget.prototype);
WebSocket.prototype.constructor = WebSocket;
WebSocket.CONNECTING = CONNECTING;
WebSocket.OPEN = OPEN;
WebSocket.CLOSING = CLOSING;
WebSocket.CLOSED = CLOSED;

WebSocket.prototype.send = function(data) {
    if (this.readyState !== OPEN) throw new DOMException("WebSocket is not open", "InvalidStateError");
    __wsSend(this._id, typeof data === "string" ? data : String(data));
};

WebSocket.prototype.close = function(code, reason) {
    if (this.readyState === CLOSING || this.readyState === CLOSED) return;
    this.readyState = CLOSING;
    __wsClose(this._id, code || 1000, reason || "");
};

WebSocket.prototype.accept = function() {
    // Cloudflare Workers extension: server-side accept
    if (this.readyState !== CONNECTING) return;
    this.readyState = OPEN;
    __wsAccept(this._id);
};

// Internal: called by native when a message arrives
WebSocket.prototype._onMessage = function(data) {
    var event = makeMessageEvent("message", { data: data });
    this.dispatchEvent(event);
    if (typeof this.onmessage === "function") this.onmessage(event);
};

// Internal: called by native when connection closes
WebSocket.prototype._onClose = function(code, reason) {
    this.readyState = CLOSED;
    var event = makeCloseEvent("close", { code: code, reason: reason, wasClean: code === 1000 });
    this.dispatchEvent(event);
    if (typeof this.onclose === "function") this.onclose(event);
    // Remove from registry on close
    delete __wsRegistry[this._id];
};

// Internal: called by native on error
WebSocket.prototype._onError = function(message) {
    var event = new Event("error");
    this.dispatchEvent(event);
    if (typeof this.onerror === "function") this.onerror({ type: "error", message: message });
};

// MessageEvent / CloseEvent are installed as native #[v8_class] types
// by `dom::install_globals` — no polyfill class definitions here.

// WebSocketPair
function WebSocketPair() {
    var id0 = __wsCreatePair();
    var id1 = id0 + 1;

    var ws0 = new WebSocket();
    ws0._id = id0;
    ws0.readyState = CONNECTING;

    var ws1 = new WebSocket();
    ws1._id = id1;
    ws1.readyState = CONNECTING;

    // Link the pair in native
    __wsLinkPair(id0, id1);

    // Register in the global registry
    __wsRegistry[id0] = ws0;
    __wsRegistry[id1] = ws1;

    this[0] = ws0;
    this[1] = ws1;
}

// When native WebSocket is feature-flagged ON, do NOT overwrite
// globalThis.WebSocket — keep the native class as the single
// implementation. The polyfill's WebSocketPair constructor still
// creates pair-coupled sockets via the same __ws* callbacks, but it
// uses `new WebSocket()` which will route to the native class once
// step 6 lands the no-arg server-mode constructor support. For step 2
// (this commit), the polyfill's WebSocketPair path remains the only
// way to mint pair-coupled sockets — and it uses the polyfill's
// internal WebSocket class scoped to the IIFE.
if (!__nativeWebSocket) {
    globalThis.WebSocket = WebSocket;
    globalThis.WebSocketPair = WebSocketPair;
}
// Native WebSocket and WebSocketPair are both installed by
// `init.rs` BEFORE this polyfill runs when the feature is on; we
// detect that and leave them in place. The polyfill's own
// WebSocketPair wraps polyfill WebSockets which reference the
// internal __wsRegistry — incompatible with native flow.
// MessageEvent / CloseEvent are installed natively by dom::install_globals.
globalThis.__wsRegistry = __wsRegistry;

})(globalThis);
