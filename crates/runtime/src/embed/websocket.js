(function(globalThis) {
"use strict";

// WebSocket states
var CONNECTING = 0, OPEN = 1, CLOSING = 2, CLOSED = 3;

// Global registry so native code can find WebSocket objects by ID
var __wsRegistry = Object.create(null);

// Build a MessageEvent-shaped Event by constructing a real (native or
// polyfill) Event and then attaching the MessageEvent-specific fields
// as expandos. Works against both the JS polyfill EventTarget (where
// Event is the polyfill's plain JS function and `dispatchEvent` accepts
// any object) and the native EventTarget (where Event is `#[v8_class]`
// with brand-checked dispatch — only real Event instances pass).
//
// The previous `function MessageEvent(type, init) { Event.call(this, …) }`
// pattern broke under the native cutover because:
//
//   - Native Event has internal field 0 holding `Box<EventState>`.
//   - `Event.call(this, …)` on a polyfill MessageEvent instance does
//     a `set_internal_field(0, …)` on a wrapper that has no field
//     slot — silent no-op or panic depending on the V8 build.
//   - Native dispatchEvent's brand check rejects the resulting object
//     ("event is not an Event instance").
//
// Building via the constructor produces a real Event whose internal
// state is correctly initialised; expandos give us the MessageEvent
// surface (`data`, `origin`, `lastEventId`) without inheritance.
function makeMessageEvent(type, init) {
    var ev = new Event(type, init);
    ev.data = init && init.data !== undefined ? init.data : null;
    ev.origin = init && init.origin || "";
    ev.lastEventId = "";
    return ev;
}

function makeCloseEvent(type, init) {
    var ev = new Event(type, init);
    ev.code = init && init.code !== undefined ? init.code : 0;
    ev.reason = init && init.reason || "";
    ev.wasClean = init && init.wasClean || false;
    return ev;
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
    // with internal-field 0 backed by a fresh `Box<EventTarget>` (or
    // a no-op for the polyfill); `Reflect.construct` then re-points
    // the prototype to WebSocket.prototype so `instanceof WebSocket`
    // still works. Equivalent to ES6 `super()` without needing class
    // syntax (this file is ES5 for legacy V8 compat).
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

// MessageEvent class — construct via `new MessageEvent(type, init)`.
// Re-exported for instanceof / type-checking. Since `makeMessageEvent`
// returns a plain Event with expandos (not an Event subclass), the
// returned event is `instanceof Event === true` but
// `instanceof MessageEvent === false`. Acceptable for a polyfill;
// Cloudflare's MessageEvent shape (`data` / `origin` / `lastEventId`)
// is the contract we promise.
function MessageEvent(type, init) {
    return makeMessageEvent(type, init);
}
MessageEvent.prototype = Object.create(Event.prototype);
MessageEvent.prototype.constructor = MessageEvent;

// CloseEvent — same pattern as MessageEvent.
function CloseEvent(type, init) {
    return makeCloseEvent(type, init);
}
CloseEvent.prototype = Object.create(Event.prototype);
CloseEvent.prototype.constructor = CloseEvent;

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

globalThis.WebSocket = WebSocket;
globalThis.WebSocketPair = WebSocketPair;
globalThis.MessageEvent = MessageEvent;
globalThis.CloseEvent = CloseEvent;
globalThis.__wsRegistry = __wsRegistry;

})(globalThis);
