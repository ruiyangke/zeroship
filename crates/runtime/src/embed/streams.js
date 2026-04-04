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

  function ReadableStream(underlyingSource) {
    this._id = __streams.create();
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

})(globalThis);
