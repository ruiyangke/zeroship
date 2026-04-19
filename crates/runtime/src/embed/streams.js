(function(globalThis) {
  "use strict";

  // =========================================================================
  // ReadableStream — value-preserving; falls back to the native __streams
  // slot only for byte chunks.
  //
  // Supports:
  //   - new ReadableStream({ start(controller), pull(controller), cancel() })
  //   - controller.enqueue(chunk), controller.close(), controller.error(e)
  //   - reader = stream.getReader(); reader.read() -> Promise<{value, done}>
  //   - reader.releaseLock(), reader.cancel()
  //   - for-await-of iteration (Symbol.asyncIterator)
  //
  // Two delivery paths coexist on a single stream:
  //
  //   1. BYTE PATH — `controller.enqueue(Uint8Array)` pushes the bytes
  //      through the native `__streams` slot identified by `_id`. That
  //      slot is what `inspect_response` hooks up to the HTTP response
  //      forwarder so byte streams flow straight to the wire with zero
  //      extra copies and no V8 re-entry per chunk.
  //
  //   2. VALUE PATH — `controller.enqueue(any)` for non-byte values (e.g.
  //      LangChain `AIMessageChunk`, OpenAI stream parts) goes into a
  //      JS-side FIFO and is delivered to the reader verbatim. The
  //      native slot is untouched; `inspect_response` will just see the
  //      stream as closed-empty for HTTP dispatch, which is the correct
  //      outcome because non-byte values can't be serialized to the
  //      wire without a caller-supplied encoder.
  //
  // The earlier implementation tried to unify both paths by doing
  // `__streams.enqueue(stream_id, new TextEncoder().encode(String(chunk)))`,
  // which turned every object into "[object Foo]" bytes — silently
  // breaking every SDK that ships object chunks (LangChain, OpenAI,
  // Vercel AI SDK, …).
  //
  // NOT supported (not needed yet):
  //   - BYOB readers, queuing strategies, backpressure, tee(), pipeTo()
  // =========================================================================

  var __streams = globalThis.__streams;

  // Strings are treated as byte chunks (UTF-8 encoded) because most
  // "streaming text" code does `controller.enqueue(someString)` and
  // expects the consumer to receive a Uint8Array. HTTP response bodies
  // also expect bytes on the wire, so promoting strings preserves the
  // previous streaming-text behavior while still keeping objects /
  // framework types (AIMessageChunk, OpenAI parts) on the value path.
  function isBytes(chunk) {
    return (
      typeof chunk === "string" ||
      chunk instanceof Uint8Array ||
      chunk instanceof ArrayBuffer ||
      (ArrayBuffer.isView && ArrayBuffer.isView(chunk))
    );
  }

  function toUint8Array(chunk) {
    if (typeof chunk === "string") return new TextEncoder().encode(chunk);
    if (chunk instanceof Uint8Array) return chunk;
    if (chunk instanceof ArrayBuffer) return new Uint8Array(chunk);
    if (ArrayBuffer.isView && ArrayBuffer.isView(chunk)) {
      return new Uint8Array(chunk.buffer, chunk.byteOffset, chunk.byteLength);
    }
    return null;
  }

  // -------------------------------------------------------------------------
  // ReadableStreamDefaultController
  // -------------------------------------------------------------------------

  function ReadableStreamDefaultController(stream) {
    this._stream = stream;
    this._streamId = stream._id;
    this._closed = false;
  }

  ReadableStreamDefaultController.prototype.enqueue = function(chunk) {
    if (this._closed) throw new TypeError("Cannot enqueue to a closed controller");
    var stream = this._stream;
    if (isBytes(chunk)) {
      __streams.enqueue(stream._id, toUint8Array(chunk));
      return;
    }
    // Non-byte value: deliver to a waiting reader if one is parked, else
    // buffer in the JS-side FIFO.
    if (stream._valueWaiters.length > 0) {
      var resolve = stream._valueWaiters.shift();
      resolve({ value: chunk, done: false });
      return;
    }
    stream._valueQueue.push(chunk);
  };

  ReadableStreamDefaultController.prototype.close = function() {
    if (this._closed) return;
    this._closed = true;
    var stream = this._stream;
    stream._ended = true;
    // Value-path waiters with nothing queued behind them resolve with
    // {done: true}. If values are still buffered, leave the waiters
    // parked — subsequent read() calls will drain them normally.
    while (stream._valueWaiters.length > 0 && stream._valueQueue.length === 0) {
      var resolve = stream._valueWaiters.shift();
      resolve({ value: undefined, done: true });
    }
    __streams.close(stream._id);
  };

  ReadableStreamDefaultController.prototype.error = function(e) {
    if (this._closed) return;
    this._closed = true;
    var stream = this._stream;
    stream._ended = true;
    stream._error = e || new Error("Stream error");
    while (stream._valueWaiters.length > 0) {
      var resolve = stream._valueWaiters.shift();
      // Per WHATWG spec the returned promise rejects when the stream
      // errors, but several popular libs just look at `{done:true}` —
      // we resolve with done to match the dominant real-world shape and
      // still propagate the native error below.
      resolve({ value: undefined, done: true });
    }
    __streams.error(stream._id, stream._error ? String(stream._error.message || stream._error) : "Stream error");
  };

  Object.defineProperty(ReadableStreamDefaultController.prototype, "desiredSize", {
    get: function() { return 1; },
    configurable: true
  });

  // -------------------------------------------------------------------------
  // ReadableStreamDefaultReader
  // -------------------------------------------------------------------------

  function ReadableStreamDefaultReader(stream) {
    this._stream = stream;
    this._closed = false;
  }

  ReadableStreamDefaultReader.prototype.read = function() {
    if (!this._stream) {
      return Promise.reject(new TypeError("Reader has been released"));
    }
    var stream = this._stream;

    // Kick `pull()` so JS-side async generators (LangChain, OpenAI SDK)
    // can lazily produce a chunk for this read. The `_pulling` guard
    // mirrors the spec's "pulling"/"pullAgain" flag — never re-enter
    // while one pull is still in flight. Pull may `controller.enqueue`
    // synchronously (value path → valueQueue / native slot) or return a
    // promise that will do so; either way, the read below will see the
    // result.
    var pullFn = stream._pullFn;
    if (pullFn && !stream._pulling && !stream._controller._closed) {
      stream._pulling = true;
      try {
        var pullResult = pullFn(stream._controller);
        var done = function() { stream._pulling = false; };
        if (pullResult && typeof pullResult.then === "function") {
          pullResult.then(done, function(e) {
            done();
            try { stream._controller.error(e); } catch (_e) {}
          });
        } else {
          done();
        }
      } catch (e) {
        stream._pulling = false;
        try { stream._controller.error(e); } catch (_e) {}
      }
    }

    // Value path takes priority when populated — it bypasses the native
    // slot entirely and avoids round-tripping arbitrary objects.
    if (stream._valueQueue.length > 0) {
      return Promise.resolve({ value: stream._valueQueue.shift(), done: false });
    }

    // Nothing in the JS queue. Ask native for bytes; if the controller
    // also has a non-byte value to deliver before bytes arrive, the
    // waiter we park below wins the race.
    return new Promise(function(resolve) {
      var settled = false;

      __streams.read(stream._id).then(function(result) {
        if (settled) return;
        // Remove our waiter if it's still parked (lost the race).
        var i = stream._valueWaiters.indexOf(valueResolver);
        if (i !== -1) stream._valueWaiters.splice(i, 1);
        settled = true;
        resolve(result);
      });

      var valueResolver = function(r) {
        if (settled) return;
        settled = true;
        resolve(r);
      };
      stream._valueWaiters.push(valueResolver);
    });
  };

  ReadableStreamDefaultReader.prototype.releaseLock = function() {
    if (this._stream) {
      this._stream._locked = false;
      this._stream = null;
    }
  };

  ReadableStreamDefaultReader.prototype.cancel = function(reason) {
    if (this._stream) {
      this._stream._locked = false;
      this._stream = null;
    }
    return Promise.resolve();
  };

  Object.defineProperty(ReadableStreamDefaultReader.prototype, "closed", {
    get: function() {
      return Promise.resolve(undefined);
    },
    configurable: true
  });

  // -------------------------------------------------------------------------
  // ReadableStream
  // -------------------------------------------------------------------------

  function ReadableStream(underlyingSource, options) {
    // Accept a pre-existing stream_id via options._streamId (used by streaming fetch).
    // When set, the Rust event loop owns the stream and pushes chunks directly.
    if (options && options._streamId !== undefined) {
      this._id = options._streamId;
    } else {
      this._id = __streams.create();
    }
    this._locked = false;
    this._disturbed = false;
    this._ended = false;
    this._error = null;
    // FIFO of non-byte values enqueued by user code. Byte chunks bypass
    // this queue entirely and land in the native slot identified by
    // `_id`, where `inspect_response` can pipe them to the HTTP wire.
    this._valueQueue = [];
    // Read waiters parked for the value path — drained by the controller
    // when a non-byte value is enqueued or the stream closes.
    this._valueWaiters = [];
    this._controller = new ReadableStreamDefaultController(this);

    if (underlyingSource && typeof underlyingSource.start === "function") {
      try {
        var startResult = underlyingSource.start(this._controller);
        // If start() is async, track the promise so the pump drives it
        // and errors propagate to the stream.
        if (startResult && typeof startResult.then === "function") {
          var ctrl = this._controller;
          startResult.then(undefined, function(e) { ctrl.error(e); });
        }
      } catch (e) {
        this._controller.error(e);
      }
    }

    this._pullFn = underlyingSource && typeof underlyingSource.pull === "function"
      ? underlyingSource.pull : null;
    this._cancelFn = underlyingSource && typeof underlyingSource.cancel === "function"
      ? underlyingSource.cancel : null;
  }

  ReadableStream.prototype.getReader = function() {
    if (this._locked) throw new TypeError("ReadableStream is already locked to a reader");
    this._locked = true;
    this._disturbed = true;
    return new ReadableStreamDefaultReader(this);
  };

  ReadableStream.prototype.cancel = function(reason) {
    if (this._locked) {
      return Promise.reject(new TypeError("Cannot cancel a locked ReadableStream"));
    }
    if (this._cancelFn) {
      try { this._cancelFn(reason); } catch (e) {}
    }
    return Promise.resolve();
  };

  Object.defineProperty(ReadableStream.prototype, "locked", {
    get: function() { return this._locked; },
    configurable: true
  });

  // for-await-of convenience. The spec exposes `[Symbol.asyncIterator]`
  // on ReadableStream; several SDK polyfills short-circuit when they
  // see it (OpenAI's `ReadableStreamToAsyncIterable`), so exposing it
  // ourselves keeps the fast path and saves an extra wrapper object.
  if (typeof Symbol !== "undefined" && Symbol.asyncIterator) {
    ReadableStream.prototype[Symbol.asyncIterator] = function() {
      var reader = this.getReader();
      return {
        next: function() {
          return reader.read().then(function(r) {
            if (r.done) { try { reader.releaseLock(); } catch (_e) {} }
            return r;
          });
        },
        "return": function(v) {
          try { reader.releaseLock(); } catch (_e) {}
          return Promise.resolve({ value: v, done: true });
        },
        "throw": function(e) {
          try { reader.releaseLock(); } catch (_e) {}
          return Promise.reject(e);
        },
        [Symbol.asyncIterator]: function() { return this; }
      };
    };
  }

  // -------------------------------------------------------------------------
  // Export
  // -------------------------------------------------------------------------

  globalThis.ReadableStream = ReadableStream;
  globalThis.ReadableStreamDefaultReader = ReadableStreamDefaultReader;
  globalThis.ReadableStreamDefaultController = ReadableStreamDefaultController;

  // =========================================================================
  // WritableStream — minimal implementation for TransformStream and piping.
  // =========================================================================

  function WritableStream(underlyingSink) {
    underlyingSink = underlyingSink || {};
    this._sink = underlyingSink;
    this._state = "writable";
    this._writer = null;
    this._writeQueue = [];
    this._closePromise = null;

    if (underlyingSink.start) {
      underlyingSink.start(this._getController());
    }
  }
  WritableStream.prototype._getController = function() {
    var self = this;
    return {
      error: function(e) { self._state = "errored"; self._error = e; }
    };
  };
  WritableStream.prototype.getWriter = function() {
    if (this._writer) throw new TypeError("WritableStream already locked");
    this._writer = new WritableStreamDefaultWriter(this);
    return this._writer;
  };
  WritableStream.prototype.close = function() {
    this._state = "closed";
    if (this._sink.close) return Promise.resolve(this._sink.close());
    return Promise.resolve();
  };
  Object.defineProperty(WritableStream.prototype, "locked", {
    get: function() { return this._writer !== null; }
  });

  function WritableStreamDefaultWriter(stream) {
    this._stream = stream;
    this.closed = new Promise(function() {}); // never resolves until close
    this.ready = Promise.resolve();
    this.desiredSize = 1;
  }
  WritableStreamDefaultWriter.prototype.write = function(chunk) {
    var sink = this._stream._sink;
    if (sink.write) return Promise.resolve(sink.write(chunk));
    return Promise.resolve();
  };
  WritableStreamDefaultWriter.prototype.close = function() {
    return this._stream.close();
  };
  WritableStreamDefaultWriter.prototype.abort = function(reason) {
    this._stream._state = "errored";
    if (this._stream._sink.abort) return Promise.resolve(this._stream._sink.abort(reason));
    return Promise.resolve();
  };
  WritableStreamDefaultWriter.prototype.releaseLock = function() {
    this._stream._writer = null;
  };

  // =========================================================================
  // TransformStream — connects a WritableStream to a ReadableStream via a
  // transformer object { transform(chunk, controller), flush(controller) }.
  // =========================================================================

  function TransformStream(transformer) {
    transformer = transformer || {};
    var readableController;

    var readable = new ReadableStream({
      start: function(c) { readableController = c; }
    });

    var transformFn = transformer.transform || function(chunk, controller) {
      controller.enqueue(chunk);
    };
    var flushFn = transformer.flush || null;

    var transformController = {
      enqueue: function(chunk) { readableController.enqueue(chunk); },
      error: function(e) { readableController.error(e); },
      terminate: function() { readableController.close(); }
    };

    var writable = new WritableStream({
      write: function(chunk) {
        return transformFn(chunk, transformController);
      },
      close: function() {
        if (flushFn) flushFn(transformController);
        readableController.close();
      }
    });

    this.readable = readable;
    this.writable = writable;
  }

  globalThis.WritableStream = WritableStream;
  globalThis.WritableStreamDefaultWriter = WritableStreamDefaultWriter;
  globalThis.TransformStream = TransformStream;

})(globalThis);
