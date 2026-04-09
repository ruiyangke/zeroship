(function(globalThis) {
  "use strict";

  // =========================================================================
  // ReadableStream — minimal implementation for SSE and streaming responses.
  //
  // Supports:
  //   - new ReadableStream({ start(controller), pull(controller), cancel() })
  //   - controller.enqueue(chunk), controller.close(), controller.error(e)
  //   - reader = stream.getReader(); reader.read() -> Promise<{value, done}>
  //   - reader.releaseLock(), reader.cancel()
  //
  // NOT supported (not needed for SSE/streaming):
  //   - BYOB readers, queuing strategies, backpressure, tee(), pipeTo()
  // =========================================================================

  var __streams = globalThis.__streams;

  // -------------------------------------------------------------------------
  // ReadableStreamDefaultController
  // -------------------------------------------------------------------------

  function ReadableStreamDefaultController(streamId) {
    this._streamId = streamId;
    this._closed = false;
  }

  ReadableStreamDefaultController.prototype.enqueue = function(chunk) {
    if (this._closed) throw new TypeError("Cannot enqueue to a closed controller");
    var data;
    if (chunk instanceof Uint8Array) {
      data = chunk;
    } else if (chunk instanceof ArrayBuffer) {
      data = new Uint8Array(chunk);
    } else if (ArrayBuffer.isView && ArrayBuffer.isView(chunk)) {
      data = new Uint8Array(chunk.buffer, chunk.byteOffset, chunk.byteLength);
    } else {
      // String or other — encode as UTF-8
      data = new TextEncoder().encode(String(chunk));
    }
    __streams.enqueue(this._streamId, data);
  };

  ReadableStreamDefaultController.prototype.close = function() {
    if (this._closed) return;
    this._closed = true;
    __streams.close(this._streamId);
  };

  ReadableStreamDefaultController.prototype.error = function(e) {
    if (this._closed) return;
    this._closed = true;
    __streams.error(this._streamId, e ? String(e.message || e) : "Stream error");
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
    return __streams.read(this._stream._id);
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
    this._controller = new ReadableStreamDefaultController(this._id);

    if (underlyingSource && typeof underlyingSource.start === "function") {
      try {
        underlyingSource.start(this._controller);
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
