// =============================================================================
// TextEncoderStream / TextDecoderStream
// =============================================================================
//
// WHATWG transform streams wrapping TextEncoder / TextDecoder. The AI
// SDK's `toUIMessageStreamResponse()` ends with
// `.pipeThrough(new TextEncoderStream())`, so they need to be present
// as TransformStream-shaped globals. Spec:
//   https://encoding.spec.whatwg.org/#interface-textencoderstream
//
// Loaded AFTER native streams install so TransformStream / TextEncoder /
// TextDecoder are all on `globalThis` by the time we reference them.
// FETCH_JS runs earlier in the pipeline and would see TransformStream
// still undefined; this module loads after.

(function () {
  if (typeof globalThis.TextEncoderStream === "undefined" &&
      typeof globalThis.TransformStream !== "undefined") {
    globalThis.TextEncoderStream = function TextEncoderStream() {
      var enc = new TextEncoder();
      var ts = new TransformStream({
        transform: function (chunk, controller) {
          controller.enqueue(enc.encode(String(chunk)));
        },
      });
      this.readable = ts.readable;
      this.writable = ts.writable;
    };
    Object.defineProperty(globalThis.TextEncoderStream.prototype, "encoding", {
      get: function () { return "utf-8"; },
    });
  }

  if (typeof globalThis.TextDecoderStream === "undefined" &&
      typeof globalThis.TransformStream !== "undefined") {
    globalThis.TextDecoderStream = function TextDecoderStream(label, options) {
      // `{ stream: true }` keeps the decoder's partial-byte state
      // across chunk boundaries — critical for SSE parsers that pipe
      // network chunks straight through (e.g. AI SDK v6's
      // `parseJsonEventStream` does
      // `body.pipeThrough(new TextDecoderStream()).pipeThrough(new EventSourceParserStream())`).
      // Without streaming mode, a multi-byte UTF-8 char split across
      // packets gets replaced with U+FFFD and the SSE parser sees
      // garbage like `dat<U+FFFD>:...` — surfaces as
      // "Unexpected non-whitespace character at position 3".
      var dec;
      try {
        dec = new TextDecoder(label, options);
      } catch (_e) {
        dec = new TextDecoder();
      }
      var ts = new TransformStream({
        transform: function (chunk, controller) {
          var s = dec.decode(chunk, { stream: true });
          if (s) controller.enqueue(s);
        },
        flush: function (controller) {
          // Drain the decoder's tail buffer when the input stream ends.
          var s = dec.decode();
          if (s) controller.enqueue(s);
        },
      });
      this.readable = ts.readable;
      this.writable = ts.writable;
    };
    Object.defineProperty(globalThis.TextDecoderStream.prototype, "encoding", {
      get: function () { return "utf-8"; },
    });
  }
})();
