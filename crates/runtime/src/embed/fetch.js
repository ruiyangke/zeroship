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
  // __zsBeginStreamForward — kernel hook for Response stream-body forwarding
  // =========================================================================
  //
  // Called by Rust's `http::inspect_response` AFTER it determines a
  // Response with a ReadableStream body should be sent to the wire.
  // Allocates a Rust-side StreamState (via `__streams.create()`), locks
  // the body via `getReader()`, and launches a self-driving pump that
  // pushes each chunk onto that StreamState. Returns the allocated
  // streamId so the kernel can forward chunks to the TCP writer.
  //
  // Lazy locking: `new Response(stream)` does NOT lock the stream — only
  // this function does. That keeps `resp.text()` / `resp.json()` working
  // on user-constructed Responses that the user reads themselves before
  // returning a different value to the handler. Once the kernel decides
  // to wire-forward (i.e. handler returned this Response), this helper
  // takes the lock and the user can no longer read the body.
  //
  // No reach into stream-class private fields. Works against any class
  // that exposes the spec `getReader()` API: native ReadableStream and
  // user-defined classes that implement the surface.
  //
  // Native Response objects are extensible (no Object.freeze on the
  // wrapper), so the `response._streamId = streamId` expando assignment
  // succeeds. The kernel later reads this field to decide whether to
  // re-pump or treat the stream as already-pumping.
  function __zsBeginStreamForward(response) {
    if (response._streamId !== undefined && response._streamId >= 0) {
      // Already forwarding (idempotent): inspect_response was called twice
      // on the same Response (rare but possible during cancel/replay paths).
      return response._streamId;
    }
    var stream = response.body;

    // Lock the stream via `getReader()` and pump chunks through
    // `__streams.{enqueue,close,error}` into a freshly-allocated
    // StreamState. Works against any class implementing the spec
    // ReadableStream surface.
    var streamId = __streams.create();
    response._streamId = streamId;
    var reader = stream.getReader();
    (function pump() {
      reader.read().then(function (r) {
        if (r.done) { __streams.close(streamId); return; }
        var v = r.value;
        if (v instanceof Uint8Array) {
          __streams.enqueue(streamId, v);
        } else if (typeof v === "string") {
          __streams.enqueue(streamId, new TextEncoder().encode(v));
        } else if (v instanceof ArrayBuffer) {
          __streams.enqueue(streamId, new Uint8Array(v));
        } else if (ArrayBuffer.isView && ArrayBuffer.isView(v)) {
          __streams.enqueue(streamId, new Uint8Array(v.buffer, v.byteOffset, v.byteLength));
        } else {
          // Non-byte chunk on the response wire is unsupported — coerce.
          __streams.enqueue(streamId, new TextEncoder().encode(String(v)));
        }
        pump();
      }, function (e) {
        __streams.error(streamId, e && e.message ? String(e.message) : String(e));
      });
    })();
    return streamId;
  }
  globalThis.__zsBeginStreamForward = __zsBeginStreamForward;

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
