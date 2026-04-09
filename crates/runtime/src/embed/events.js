(function(globalThis) {
"use strict";

// Event
function Event(type, options) {
    options = options || {};
    this.type = type;
    this.bubbles = !!options.bubbles;
    this.cancelable = !!options.cancelable;
    this.composed = !!options.composed;
    this.defaultPrevented = false;
    this.target = null;
    this.currentTarget = null;
    this.eventPhase = 0;
    this.timeStamp = performance.now();
    this._stopped = false;
    this._immediateStopped = false;
}
Event.prototype.preventDefault = function() {
    if (this.cancelable) this.defaultPrevented = true;
};
Event.prototype.stopPropagation = function() { this._stopped = true; };
Event.prototype.stopImmediatePropagation = function() { this._stopped = true; this._immediateStopped = true; };
Event.prototype.composedPath = function() { return this.target ? [this.target] : []; };

// CustomEvent
function CustomEvent(type, options) {
    Event.call(this, type, options);
    this.detail = options && options.detail !== undefined ? options.detail : null;
}
CustomEvent.prototype = Object.create(Event.prototype);
CustomEvent.prototype.constructor = CustomEvent;

// EventTarget
function EventTarget() {
    this._listeners = Object.create(null);
}
EventTarget.prototype.addEventListener = function(type, callback, options) {
    if (typeof callback !== "function") return;
    if (!this._listeners[type]) this._listeners[type] = [];
    var capture = typeof options === "boolean" ? options : (options && options.capture) || false;
    var once = options && options.once || false;
    this._listeners[type].push({ callback: callback, capture: capture, once: once });
};
EventTarget.prototype.removeEventListener = function(type, callback, options) {
    if (!this._listeners[type]) return;
    var capture = typeof options === "boolean" ? options : (options && options.capture) || false;
    this._listeners[type] = this._listeners[type].filter(function(l) {
        return l.callback !== callback || l.capture !== capture;
    });
};
EventTarget.prototype.dispatchEvent = function(event) {
    event.target = this;
    event.currentTarget = this;
    var listeners = this._listeners[event.type];
    if (!listeners) return true;
    var copy = listeners.slice();
    for (var i = 0; i < copy.length; i++) {
        if (event._immediateStopped) break;
        copy[i].callback.call(this, event);
        if (copy[i].once) {
            this.removeEventListener(event.type, copy[i].callback, copy[i].capture);
        }
    }
    return !event.defaultPrevented;
};

globalThis.Event = Event;
globalThis.CustomEvent = CustomEvent;
globalThis.EventTarget = EventTarget;

})(globalThis);
