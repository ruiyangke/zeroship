(function(globalThis) {
"use strict";

// CustomEvent polyfill.
//
// Event and EventTarget are native (see crates/runtime/src/dom/{event,event_target}.rs);
// install_dom runs BEFORE this file is evaluated, so `globalThis.Event`
// at this point is the native class. CustomEvent is a real WHATWG DOM
// type but doesn't yet have a native impl; this thin shim extends the
// native Event class so `customEvent instanceof Event === true`.
//
// Once a native CustomEvent class lands (crates/runtime/src/dom/), this
// file can be deleted entirely.

if (typeof globalThis.CustomEvent !== "function") {
    function CustomEvent(type, options) {
        var ev = new globalThis.Event(type, options);
        ev.detail = options && options.detail !== undefined ? options.detail : null;
        Object.setPrototypeOf(ev, CustomEvent.prototype);
        return ev;
    }
    CustomEvent.prototype = Object.create(globalThis.Event.prototype);
    CustomEvent.prototype.constructor = CustomEvent;
    globalThis.CustomEvent = CustomEvent;
}

})(globalThis);
