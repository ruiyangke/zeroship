(function(globalThis) {
  "use strict";

  // =========================================================================
  // DOMException polyfill (minimal, for AbortError)
  // =========================================================================

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
  // Headers — provided natively by `crate::headers::install_global`. The
  // native install runs immediately after this polyfill (see
  // `init::load_polyfills_and_modules`), so `globalThis.Headers` is the
  // WHATWG Fetch §2.2 implementation in Rust by the time any
  // Request/Response constructor body runs (constructor bodies are
  // defined here but only invoked once user code or fetch() runs).
  //
  // The hand-rolled JS shim that used to live here had several spec
  // divergences (no ByteString validation, no Set-Cookie special cases,
  // snapshot-style iteration); the native class fixes all of those.
  // WPT pass: 98/0/1.
  // =========================================================================

  // =========================================================================
  // Body mixin helpers
  // =========================================================================

  function initBody(obj, body) {
    obj._bodyUsed = false;
    obj._bodyText = "";
    obj._bodyBytes = null;  // ArrayBuffer for binary bodies

    if (body === undefined || body === null) {
      obj._bodyText = "";
    } else if (typeof body === "string") {
      obj._bodyText = body;
    } else if (body instanceof ArrayBuffer) {
      obj._bodyBytes = body;
      // TextDecoder is native C++; the old JS char-code loop was O(n²)
      // on string concat and wrong for UTF-8 (Latin-1 mapping). Native
      // decode handles both issues and is cheaper for bodies above ~20 bytes.
      obj._bodyText = new TextDecoder().decode(body);
    } else if (ArrayBuffer.isView && ArrayBuffer.isView(body)) {
      obj._bodyBytes = body.buffer.slice(body.byteOffset, body.byteOffset + body.byteLength);
      obj._bodyText = new TextDecoder().decode(body);
    } else {
      obj._bodyText = String(body);
    }
  }

  function consumeBody(obj) {
    if (obj._bodyUsed) {
      return Promise.reject(new TypeError("Body has already been consumed."));
    }
    obj._bodyUsed = true;
    return obj._bodyText;
  }

  // Read a ReadableStream to completion, returning a Promise<string>.
  // Drain a ReadableStream into a single Uint8Array. Used by every
  // non-streaming Body mixin consumer (text, json, arrayBuffer, blob) when
  // the response arrived via streaming fetch (`stream_id` set). Each chunk
  // is either already a Uint8Array (Rust push path) or a string (pure-JS
  // ReadableStreams from user code), and we normalize to bytes before
  // concatenation so TextDecoder can run on the combined buffer.
  function __readStreamToBytes(stream) {
    var reader = stream.getReader();
    var chunks = [];
    function pump() {
      return reader.read().then(function(result) {
        if (result.done) {
          var total = 0;
          for (var i = 0; i < chunks.length; i++) total += chunks[i].length;
          var buf = new Uint8Array(total);
          var off = 0;
          for (var i = 0; i < chunks.length; i++) {
            buf.set(chunks[i], off);
            off += chunks[i].length;
          }
          return buf;
        }
        if (result.value) {
          chunks.push(result.value instanceof Uint8Array ? result.value : new TextEncoder().encode(String(result.value)));
        }
        return pump();
      });
    }
    return pump();
  }

  function __readStreamToString(stream) {
    return __readStreamToBytes(stream).then(function(bytes) {
      return new TextDecoder().decode(bytes);
    });
  }

  function applyBodyMixin(proto) {
    Object.defineProperty(proto, "bodyUsed", {
      get: function() { return this._bodyUsed; },
      configurable: true
    });

    proto.text = function() {
      if (this._isStreamBody && this.body) {
        if (this._bodyUsed) return Promise.reject(new TypeError("Body has already been consumed."));
        this._bodyUsed = true;
        return __readStreamToString(this.body);
      }
      var text = consumeBody(this);
      if (text instanceof Promise) return text; // rejection
      return Promise.resolve(text);
    };

    proto.json = function() {
      return this.text().then(function(text) {
        return JSON.parse(text);
      });
    };

    proto.arrayBuffer = function() {
      if (this._bodyUsed) return Promise.reject(new TypeError("Body has already been consumed."));
      this._bodyUsed = true;
      // Streaming fetch response — drain the ReadableStream into a Uint8Array
      // and hand the backing ArrayBuffer to the caller. Without this path,
      // `fetch(url).then(r => r.arrayBuffer())` silently returned an empty
      // buffer for anything delivered via stream_id.
      if (this._isStreamBody && this.body) {
        return __readStreamToBytes(this.body).then(function(u8) {
          return u8.buffer.slice(u8.byteOffset, u8.byteOffset + u8.byteLength);
        });
      }
      if (this._bodyBytes) {
        return Promise.resolve(this._bodyBytes);
      }
      // Fallback for string bodies
      var text = this._bodyText;
      var buf = new ArrayBuffer(text.length);
      var view = new Uint8Array(buf);
      for (var i = 0; i < text.length; i++) {
        view[i] = text.charCodeAt(i) & 0xff;
      }
      return Promise.resolve(buf);
    };

    proto.blob = function() {
      return this.arrayBuffer().then(function(buf) {
        return new Blob([new Uint8Array(buf)], { type: "" });
      });
    };

    proto.formData = function() {
      // Basic application/x-www-form-urlencoded parsing
      var ct = "";
      if (this.headers) ct = this.headers.get("content-type") || "";

      if (ct.indexOf("application/x-www-form-urlencoded") !== -1) {
        return this.text().then(function(text) {
          var fd = new FormData();
          var pairs = text.split("&");
          for (var i = 0; i < pairs.length; i++) {
            var eq = pairs[i].indexOf("=");
            if (eq === -1) continue;
            fd.append(decodeURIComponent(pairs[i].slice(0, eq).replace(/\+/g, " ")),
                      decodeURIComponent(pairs[i].slice(eq + 1).replace(/\+/g, " ")));
          }
          return fd;
        });
      }
      return Promise.reject(new TypeError("Could not parse body as FormData"));
    };
  }

  // =========================================================================
  // Request
  // =========================================================================

  function Request(input, init) {
    init = init || {};
    var isReq = input instanceof Request;

    this.url = isReq ? input.url : String(input);
    this.method = isReq ? input.method : "GET";
    this.redirect = isReq ? input.redirect : "follow";
    this.signal = isReq ? input.signal : null;
    this.cache = isReq ? input.cache : "default";
    this.credentials = isReq ? input.credentials : "same-origin";
    this.mode = isReq ? input.mode : "cors";
    this.referrer = isReq ? input.referrer : "about:client";
    initBody(this, isReq ? input._bodyText : null);

    if (init.method !== undefined) this.method = init.method.toUpperCase();
    if (init.redirect !== undefined) this.redirect = init.redirect;
    if (init.signal !== undefined) this.signal = init.signal;
    if (init.cache !== undefined) this.cache = init.cache;
    if (init.credentials !== undefined) this.credentials = init.credentials;
    if (init.mode !== undefined) this.mode = init.mode;
    if (init.referrer !== undefined) this.referrer = init.referrer;
    if (init.body !== undefined) initBody(this, init.body);
    // Headers: construct ONCE. init.headers overrides, else copy from input Request.
    this.headers = new Headers(init.headers !== undefined ? init.headers : (isReq ? input.headers : undefined));
  }

  Request.prototype.clone = function() {
    if (this._bodyUsed) {
      throw new TypeError("Cannot clone a disturbed Request");
    }
    return new Request(this);
  };

  // Spec-default values on the prototype — the Rust-side fast-path
  // constructor (http::build_request_native) skips these per-instance
  // sets and relies on prototype lookup, saving ~8 hidden-class transitions
  // per hot-path Request. User-space `new Request()` still sets them
  // explicitly (the constructor is unchanged).
  Request.prototype.redirect = "follow";
  Request.prototype.signal = null;
  Request.prototype.cache = "default";
  Request.prototype.credentials = "same-origin";
  Request.prototype.mode = "cors";
  Request.prototype.referrer = "about:client";
  Request.prototype._bodyUsed = false;
  Request.prototype._bodyBytes = null;
  Request.prototype._bodyText = "";

  applyBodyMixin(Request.prototype);

  // =========================================================================
  // Response
  // =========================================================================

  function Response(body, init) {
    init = init || {};
    var status = init.status !== undefined ? init.status : 200;

    // Validate status range (101 allowed for WebSocket upgrade)
    if ((status < 200 || status > 599) && status !== 101) {
      throw new RangeError("Invalid status code: " + status);
    }

    var statusText = init.statusText !== undefined ? init.statusText : "";

    // Validate statusText (no non-ASCII, no CR/LF)
    for (var i = 0; i < statusText.length; i++) {
      var c = statusText.charCodeAt(i);
      if (c > 0x7E || (c < 0x20 && c !== 0x09)) {
        throw new TypeError("Invalid statusText");
      }
    }

    // Null-body status: 204, 205, 304 — body must be null
    if (body !== null && body !== undefined && body !== "" &&
        (status === 204 || status === 205 || status === 304)) {
      throw new TypeError("Response with null body status cannot have body");
    }

    this.status = status;
    this.statusText = statusText;
    this.headers = new Headers(init.headers);
    this.type = "default";
    this.url = "";
    this.redirected = false;
    this.ok = this.status >= 200 && this.status < 300;

    // WebSocket upgrade: store the client WebSocket reference
    if (init.webSocket !== undefined) {
      this.webSocket = init.webSocket;
    }

    // ReadableStream body — store the stream as-is and mark for streaming.
    // The lock-and-pump happens lazily in `__zsBeginStreamForward(this)`,
    // called from Rust's `inspect_response` after the kernel has decided
    // this Response will be sent to the wire. Locking eagerly here would
    // block `resp.text()` / `resp.json()` on Responses the user constructs
    // and then consumes themselves before returning a different value.
    //
    // Detection is duck-typed: anything with a `getReader()` method —
    // native ReadableStream, the polyfill class returned from
    // `pipeThrough` chains, or user-defined classes that implement the
    // surface — flows through unchanged. The kernel reads
    // `response._streamId` (allocated by the lazy forward helper);
    // legacy paths that still publish `body._id` are honoured via the
    // `__zsBeginStreamForward` fast-path so the polyfill skeleton's
    // direct-buffer write keeps working until D-19 step 2 deletes it.
    if (
      body !== null && body !== undefined &&
      typeof body === "object" &&
      typeof body.getReader === "function"
    ) {
      this.body = body;
      this._streamId = -1;  // sentinel: not yet allocated; set on first forward
      this._bodyText = null;
      this._bodyBytes = null;
      this._bodyUsed = false;
      this._isStreamBody = true;
    } else {
      this._isStreamBody = false;
      initBody(this, body);
    }
  }

  Response.prototype.clone = function() {
    if (this._bodyUsed) {
      throw new TypeError("Cannot clone a disturbed Response");
    }
    // Use _bodyBytes if available to preserve binary data losslessly
    var cloneBody = this._bodyBytes ? this._bodyBytes.slice(0) : this._bodyText;
    var resp = new Response(cloneBody, {
      status: this.status,
      statusText: this.statusText,
      headers: new Headers(this.headers)
    });
    resp.type = this.type;
    resp.url = this.url;
    resp.redirected = this.redirected;
    resp.ok = this.ok;
    return resp;
  };

  applyBodyMixin(Response.prototype);

  // Tag the prototype so Rust can classify a value as "shaped like a Response"
  // via one property read instead of probing `status` + `headers` (two reads,
  // two string interns) on every async RPC settlement. Any object that derives
  // from Response.prototype (polyfill, `new Response`, async-generator wrap)
  // inherits `__zsResponse === 1`; plain handler returns (objects, primitives)
  // don't. See `http::looks_like_response`.
  Response.prototype.__zsResponse = 1;

  // Shared, immutable headers-array for the `Response.json(x)` fast-path.
  // Rust `extract_response_headers` reads `_zsHeadersArr` first and skips
  // the `_map` walk when present. Every JSON response points at the same
  // frozen array — no per-request allocation. The object is frozen so user
  // code can't mutate the shared list (they'd need a real Headers instance).
  var JSON_HEADERS_ARR = Object.freeze([Object.freeze(["content-type", "application/json"])]);

  Response.error = function() {
    var resp = Object.create(Response.prototype);
    resp.status = 0;
    resp.statusText = "";
    resp.headers = new Headers();
    resp.type = "error";
    resp.url = "";
    resp.redirected = false;
    resp.ok = false;
    initBody(resp, null);
    return resp;
  };

  Response.redirect = function(url, status) {
    if (status === undefined) status = 302;
    var validStatuses = [301, 302, 303, 307, 308];
    if (validStatuses.indexOf(status) === -1) {
      throw new RangeError("Invalid status code for redirect: " + status);
    }
    var resp = new Response(null, { status: status, statusText: "" });
    resp.headers.set("Location", url);
    return resp;
  };

  Response.json = function(data, init) {
    var body = JSON.stringify(data);
    // Fast path: plain `Response.json(x)` — no init, default status/headers.
    // Skips validateName/validateValue regex and the Response constructor's
    // init-branching. All subsequent property access works identically
    // because we inherit from Response.prototype (bodyUsed getter,
    // text/json/arrayBuffer methods).
    if (!init) {
      var resp = Object.create(Response.prototype);
      resp.status = 200;
      resp.statusText = "";
      resp.type = "default";
      resp.url = "";
      resp.redirected = false;
      resp.ok = true;
      resp._bodyText = body;
      resp._bodyBytes = null;
      resp._bodyUsed = false;
      resp._isStreamBody = false;
      // Use the constructor — works identically against the JS polyfill
      // and the native `Headers` (the latter rejects the
      // `Object.create(prototype) + _map = …` shortcut because its
      // internal field 0 holds a Box<HeaderList> only the constructor
      // populates).
      resp.headers = new Headers([["content-type", "application/json"]]);
      // Rust-side `extract_response_headers` reads this array directly
      // and skips the iterable walk (~5-10 V8 property ops per header).
      resp._zsHeadersArr = JSON_HEADERS_ARR;
      return resp;
    }
    // Slow path — preserve full spec behaviour when init is present.
    var headers = new Headers(init.headers);
    if (!headers.has("content-type")) {
      headers.set("content-type", "application/json");
    }
    return new Response(body, {
      status: init.status !== undefined ? init.status : 200,
      statusText: init.statusText || "",
      headers: headers
    });
  };

  // =========================================================================
  // AbortSignal
  // =========================================================================

  function AbortSignal() {
    this.aborted = false;
    this.reason = undefined;
    this._listeners = [];
  }

  AbortSignal.prototype.addEventListener = function(type, listener) {
    if (type === "abort") {
      this._listeners.push(listener);
    }
  };

  AbortSignal.prototype.removeEventListener = function(type, listener) {
    if (type === "abort") {
      var idx = this._listeners.indexOf(listener);
      if (idx !== -1) this._listeners.splice(idx, 1);
    }
  };

  AbortSignal.prototype.dispatchEvent = function(event) {
    if (event.type === "abort") {
      event.target = this;
      event.currentTarget = this;
      for (var i = 0; i < this._listeners.length; i++) {
        try { this._listeners[i].call(this, event); } catch (e) {}
      }
      if (typeof this.onabort === "function") {
        try { this.onabort.call(this, event); } catch (e) {}
      }
    }
  };

  AbortSignal.prototype.throwIfAborted = function() {
    if (this.aborted) {
      throw this.reason;
    }
  };

  AbortSignal.abort = function(reason) {
    var signal = new AbortSignal();
    signal.aborted = true;
    signal.reason = reason !== undefined ? reason : new DOMException("The operation was aborted.", "AbortError");
    return signal;
  };

  AbortSignal.timeout = function(ms) {
    var signal = new AbortSignal();
    setTimeout(function() {
      if (!signal.aborted) {
        signal.aborted = true;
        signal.reason = new DOMException("The operation timed out.", "TimeoutError");
        signal.dispatchEvent({ type: "abort", target: signal, currentTarget: signal });
      }
    }, ms);
    return signal;
  };

  // AbortSignal.any(signals) — returns a signal that aborts when any of
  // the input signals abort. Used by langgraph and other Web-native libs
  // for combining cancellation sources. Without it, langgraph's state
  // machine deadlocks on the first invoke().
  AbortSignal.any = function(signals) {
    var combined = new AbortSignal();
    for (var i = 0; i < signals.length; i++) {
      var s = signals[i];
      if (s && s.aborted) {
        combined.aborted = true;
        combined.reason = s.reason;
        return combined;
      }
    }
    function onAbort(ev) {
      if (combined.aborted) return;
      combined.aborted = true;
      combined.reason = ev && ev.target ? ev.target.reason : undefined;
      combined.dispatchEvent({ type: "abort", target: combined, currentTarget: combined });
    }
    for (var j = 0; j < signals.length; j++) {
      var sj = signals[j];
      if (sj && typeof sj.addEventListener === "function") {
        sj.addEventListener("abort", onAbort);
      }
    }
    return combined;
  };

  // =========================================================================
  // AbortController
  // =========================================================================

  function AbortController() {
    this.signal = new AbortSignal();
  }

  AbortController.prototype.abort = function(reason) {
    if (this.signal.aborted) return;
    this.signal.aborted = true;
    this.signal.reason = reason !== undefined ? reason : new DOMException("The operation was aborted.", "AbortError");
    var event = { type: "abort", target: this.signal, currentTarget: this.signal };
    this.signal.dispatchEvent(event);
  };

  // =========================================================================
  // fetch()
  // =========================================================================

  function fetch(input, init) {
    return new Promise(function(resolve, reject) {
      var request;

      if (input instanceof Request) {
        request = input;
        if (init) {
          // Merge init overrides into a clone
          request = new Request(input, init);
        }
      } else {
        request = new Request(input, init);
      }

      var signal = request.signal;

      if (signal && signal.aborted) {
        return reject(signal.reason || new DOMException("The operation was aborted.", "AbortError"));
      }

      var method = request.method;
      var url = request.url;
      // Spread the headers iterable into [[name, value], ...]. Works
      // against the polyfill (which exposes `[Symbol.iterator]` via
      // `entries`) and the native `Headers` (per WHATWG Fetch §2.2
      // `iterable<>` mixin).
      var headersJson = JSON.stringify([...request.headers]);
      var body = request._bodyText || null;

      var abortHandler;
      if (signal) {
        abortHandler = function() {
          reject(signal.reason || new DOMException("The operation was aborted.", "AbortError"));
        };
        signal.addEventListener("abort", abortHandler);
      }

      var rawResult;
      try {
        rawResult = globalThis.__rawFetch(method, url, headersJson, body);
      } catch (e) {
        if (signal && abortHandler) signal.removeEventListener("abort", abortHandler);
        return reject(new TypeError("Network request failed: " + e.message));
      }

      // __rawFetch returns a Promise that resolves to a JSON string
      Promise.resolve(rawResult).then(function(jsonStr) {
        if (signal && abortHandler) signal.removeEventListener("abort", abortHandler);

        if (signal && signal.aborted) {
          return reject(signal.reason || new DOMException("The operation was aborted.", "AbortError"));
        }

        var parsed;
        try {
          parsed = JSON.parse(jsonStr);
        } catch (e) {
          return reject(new TypeError("Failed to parse fetch response: " + e.message));
        }

        if (parsed.error) {
          return reject(new TypeError("Network request failed: " + parsed.error));
        }

        var responseHeaders = new Headers(parsed.headers || []);
        var response = Object.create(Response.prototype);
        response.status = parsed.status;
        response.statusText = parsed.statusText || "";
        response.headers = responseHeaders;
        response.type = "basic";
        response.url = parsed.url || url;
        response.redirected = !!parsed.redirected;
        response.ok = parsed.status >= 200 && parsed.status < 300;

        if (parsed.stream_id !== undefined) {
          // Streaming body — wrap the pre-allocated stream_id in a
          // spec-shaped ReadableStream. The Rust event loop pushes chunks
          // via LoopEvent::StreamChunk into the StreamState behind
          // stream_id; `__streams.read(stream_id)` returns a Promise<{value,done}>
          // that the underlying-source's pull() drains lazily.
          //
          // Users see only the spec API (`response.body.getReader()`,
          // `for-await`, `pipeThrough`) — no reach into private fields.
          var rsId = parsed.stream_id;
          response.body = new ReadableStream({
            pull: function (controller) {
              return __streams.read(rsId).then(function (r) {
                if (r.done) { controller.close(); return; }
                controller.enqueue(r.value);
              });
            },
            cancel: function () {
              __streams.close(rsId);
            },
          });
          response._streamId = rsId;
          response._bodyText = null;
          response._bodyBytes = null;
          response._bodyUsed = false;
          response._isStreamBody = true;
        } else {
          // Full body (concurrent model or legacy)
          response._isStreamBody = false;
          initBody(response, parsed.body || "");
        }

        resolve(response);
      }, function(err) {
        if (signal && abortHandler) signal.removeEventListener("abort", abortHandler);
        reject(new TypeError("Network request failed: " + (err && err.message ? err.message : String(err))));
      });
    });
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
  // takes the lock and the user can no longer read the body — same race
  // semantics as the pre-cutover skeleton, but explicit instead of
  // implicit.
  //
  // No reach into stream-class private fields. Works against any class
  // that exposes the spec `getReader()` API: native ReadableStream, the
  // polyfill class returned from `pipeThrough` chains, or user-defined
  // classes implementing the surface.
  function __zsBeginStreamForward(response) {
    if (response._streamId !== undefined && response._streamId >= 0) {
      // Already forwarding (idempotent): inspect_response was called twice
      // on the same Response (rare but possible during cancel/replay paths).
      return response._streamId;
    }
    var stream = response.body;

    // Polyfill-skeleton fast path: if the stream exposes `_id`, it is
    // backed by a Rust-side StreamState that the user's controller writes
    // to directly via `__streams.enqueue(_id, ...)`. The wire forwarder
    // can attach to that StreamState immediately — no pump, no second
    // copy, no race against the skeleton's internal _valueWaiter dispatch
    // (which clobbers a final `[DONE]` chunk if a recursive read parks
    // between enqueue+close). This keeps the pre-cutover semantics until
    // D-19 step 2 removes the skeleton entirely.
    if (typeof stream._id === "number") {
      response._streamId = stream._id;
      return stream._id;
    }

    // Native / spec-conformant fallback: lock the stream and pump via
    // `getReader().read()` into a freshly-allocated StreamState. Works
    // against native ReadableStream, polyfill `__zsPolyfillReadableStream`,
    // and any user-defined class implementing the surface.
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
  // Export to globalThis
  // =========================================================================

  globalThis.fetch = fetch;
  // Headers comes from the native install — see top of file.
  globalThis.Request = Request;
  globalThis.Response = Response;
  globalThis.AbortController = AbortController;
  globalThis.AbortSignal = AbortSignal;

  // TextEncoder / TextDecoder are now installed natively from
  // crates/runtime/src/text_encoding.rs (via #[v8_class]). The
  // hand-rolled JS polyfill that lived here ignored decode()'s
  // { stream: true } option, corrupting multi-byte UTF-8 split
  // across chunks (the AI SDK / SSE bug). Removed entirely — if
  // setup_globals didn't run, the class is missing rather than
  // silently wrong.

  // =========================================================================
  // atob / btoa polyfill (Base64)
  // =========================================================================

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

  if (typeof structuredClone === "undefined") {
    globalThis.structuredClone = function(obj) {
      return JSON.parse(JSON.stringify(obj));
    };
  }

})(globalThis);
