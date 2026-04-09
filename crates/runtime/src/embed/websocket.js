(function(globalThis) {
"use strict";

// WebSocket states
var CONNECTING = 0, OPEN = 1, CLOSING = 2, CLOSED = 3;

// Global registry so native code can find WebSocket objects by ID
var __wsRegistry = Object.create(null);

function WebSocket(url) {
    EventTarget.call(this);
    this.url = url || "";
    this.readyState = CONNECTING;
    this.protocol = "";
    this.extensions = "";
    this.binaryType = "arraybuffer";
    this._id = 0;  // set by native after accept
    this._sendQueue = [];
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
    var event = new MessageEvent("message", { data: data });
    this.dispatchEvent(event);
    if (typeof this.onmessage === "function") this.onmessage(event);
};

// Internal: called by native when connection closes
WebSocket.prototype._onClose = function(code, reason) {
    this.readyState = CLOSED;
    var event = new CloseEvent("close", { code: code, reason: reason, wasClean: code === 1000 });
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

// MessageEvent
function MessageEvent(type, init) {
    Event.call(this, type, init);
    this.data = init && init.data !== undefined ? init.data : null;
    this.origin = init && init.origin || "";
    this.lastEventId = "";
}
MessageEvent.prototype = Object.create(Event.prototype);
MessageEvent.prototype.constructor = MessageEvent;

// CloseEvent
function CloseEvent(type, init) {
    Event.call(this, type, init);
    this.code = init && init.code !== undefined ? init.code : 0;
    this.reason = init && init.reason || "";
    this.wasClean = init && init.wasClean || false;
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
