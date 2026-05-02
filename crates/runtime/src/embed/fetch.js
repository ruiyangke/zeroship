(function(globalThis) {
  "use strict";

  // After D-23 step 3, the JS Body / Request / Response / fetch /
  // AbortController / AbortSignal polyfill bodies are deleted — those
  // surfaces are entirely native. What remains is the small set of
  // things that aren't (yet) native and the kernel-bridge helper
  // `__zsBeginStreamForward`.

  // =========================================================================
  // DOMException polyfill (minimal, for AbortError)
  // =========================================================================
  //
  // Stays as JS until a native DOMException class lands. Native fetch's
  // synchronous-abort path constructs DOMException via this constructor
  // when the user didn't supply a reason; the WHATWG-spec form is
  // `new DOMException("...", "AbortError")` which inherits from Error.

  var DOMException = globalThis.DOMException;
  if (!DOMException) {
    DOMException = function DOMException(message, name) {
      this.message = message || "";
      this.name = name || "Error";
      this.code = 0;
      if (name === "AbortError") this.code = 20;
    };
    DOMException.prototype = Object.create(Error.prototype);
    DOMException.prototype.constructor = DOMException;
    globalThis.DOMException = DOMException;
  }

  // =========================================================================
  // atob / btoa polyfill (Base64)
  // =========================================================================
  //
  // No native Base64 class shipped yet. These are tiny and rarely a
  // perf hot path; the JS implementations stay until we have a native
  // crypto/text-processing class that absorbs them.

  if (typeof btoa === "undefined") {
    var _chars = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    globalThis.btoa = function(str) {
      str = String(str);
      var out = "";
      for (var i = 0; i < str.length; i += 3) {
        var a = str.charCodeAt(i);
        var b = i + 1 < str.length ? str.charCodeAt(i + 1) : 0;
        var c = i + 2 < str.length ? str.charCodeAt(i + 2) : 0;
        out += _chars[(a >> 2)];
        out += _chars[((a & 3) << 4) | (b >> 4)];
        out += i + 1 < str.length ? _chars[((b & 15) << 2) | (c >> 6)] : "=";
        out += i + 2 < str.length ? _chars[c & 63] : "=";
      }
      return out;
    };
    globalThis.atob = function(str) {
      str = String(str).replace(/=+$/, "");
      var out = "";
      for (var i = 0; i < str.length; i += 4) {
        var a = _chars.indexOf(str[i]);
        var b = _chars.indexOf(str[i + 1]);
        var c = _chars.indexOf(str[i + 2]);
        var d = _chars.indexOf(str[i + 3]);
        out += String.fromCharCode((a << 2) | (b >> 4));
        if (c >= 0) out += String.fromCharCode(((b & 15) << 4) | (c >> 2));
        if (d >= 0) out += String.fromCharCode(((c & 3) << 6) | d);
      }
      return out;
    };
  }

  // =========================================================================
  // structuredClone polyfill (JSON-based, covers common cases)
  // =========================================================================
  //
  // No native structuredClone — JSON-roundtrip is the practical-enough
  // approximation for the values user code typically clones (handler
  // input, RPC envelopes). Doesn't preserve prototype chain, dates,
  // typed arrays, etc. — caveats apply, but matches what the polyfill
  // shipped before.

  if (typeof structuredClone === "undefined") {
    globalThis.structuredClone = function(obj) {
      return JSON.parse(JSON.stringify(obj));
    };
  }

})(globalThis);
