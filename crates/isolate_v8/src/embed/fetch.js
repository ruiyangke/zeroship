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
  // Headers
  // =========================================================================

  function Headers(init) {
    this._map = Object.create(null); // lowercase key -> [name, ...values]

    if (init) {
      if (init instanceof Headers) {
        init.forEach(function(value, name) {
          this.append(name, value);
        }, this);
      } else if (Array.isArray(init)) {
        for (var i = 0; i < init.length; i++) {
          if (!Array.isArray(init[i]) || init[i].length < 2) {
            throw new TypeError("Each header pair must be an iterable [name, value]");
          }
          this.append(init[i][0], init[i][1]);
        }
      } else if (typeof init === "object") {
        var keys = Object.keys(init);
        for (var j = 0; j < keys.length; j++) {
          this.append(keys[j], init[keys[j]]);
        }
      }
    }
  }

  Headers.prototype.append = function(name, value) {
    var key = name.toLowerCase();
    if (this._map[key]) {
      this._map[key].push(String(value));
    } else {
      this._map[key] = [String(value)];
    }
  };

  Headers.prototype.delete = function(name) {
    delete this._map[name.toLowerCase()];
  };

  Headers.prototype.get = function(name) {
    var values = this._map[name.toLowerCase()];
    return values ? values.join(", ") : null;
  };

  Headers.prototype.has = function(name) {
    return name.toLowerCase() in this._map;
  };

  Headers.prototype.set = function(name, value) {
    this._map[name.toLowerCase()] = [String(value)];
  };

  Headers.prototype.forEach = function(callback, thisArg) {
    var keys = Object.keys(this._map).sort();
    for (var i = 0; i < keys.length; i++) {
      var key = keys[i];
      callback.call(thisArg, this._map[key].join(", "), key, this);
    }
  };

  Headers.prototype.entries = function() {
    var keys = Object.keys(this._map).sort();
    var index = 0;
    var self = this;
    return {
      next: function() {
        if (index >= keys.length) return { done: true, value: undefined };
        var key = keys[index++];
        return { done: false, value: [key, self._map[key].join(", ")] };
      },
      [Symbol.iterator]: function() { return this; }
    };
  };

  Headers.prototype.keys = function() {
    var keys = Object.keys(this._map).sort();
    var index = 0;
    return {
      next: function() {
        if (index >= keys.length) return { done: true, value: undefined };
        return { done: false, value: keys[index++] };
      },
      [Symbol.iterator]: function() { return this; }
    };
  };

  Headers.prototype.values = function() {
    var keys = Object.keys(this._map).sort();
    var index = 0;
    var self = this;
    return {
      next: function() {
        if (index >= keys.length) return { done: true, value: undefined };
        return { done: false, value: self._map[keys[index++]].join(", ") };
      },
      [Symbol.iterator]: function() { return this; }
    };
  };

  Headers.prototype[Symbol.iterator] = function() {
    return this.entries();
  };

  Headers.prototype._toArray = function() {
    var result = [];
    var keys = Object.keys(this._map).sort();
    for (var i = 0; i < keys.length; i++) {
      var key = keys[i];
      var values = this._map[key];
      for (var j = 0; j < values.length; j++) {
        result.push([key, values[j]]);
      }
    }
    return result;
  };

  // =========================================================================
  // Body mixin helpers
  // =========================================================================

  function initBody(obj, body) {
    obj._bodyUsed = false;
    obj._bodyText = "";

    if (body === undefined || body === null) {
      obj._bodyText = "";
    } else if (typeof body === "string") {
      obj._bodyText = body;
    } else if (body instanceof ArrayBuffer) {
      // Store as string for simplicity in this polyfill
      obj._bodyText = String.fromCharCode.apply(null, new Uint8Array(body));
    } else if (ArrayBuffer.isView && ArrayBuffer.isView(body)) {
      obj._bodyText = String.fromCharCode.apply(null, new Uint8Array(body.buffer, body.byteOffset, body.byteLength));
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

  function applyBodyMixin(proto) {
    Object.defineProperty(proto, "bodyUsed", {
      get: function() { return this._bodyUsed; },
      configurable: true
    });

    proto.text = function() {
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
      return this.text().then(function(text) {
        var buf = new ArrayBuffer(text.length);
        var view = new Uint8Array(buf);
        for (var i = 0; i < text.length; i++) {
          view[i] = text.charCodeAt(i) & 0xff;
        }
        return buf;
      });
    };

    proto.blob = function() {
      return this.text().then(function() {
        return Promise.reject(new Error("Blob is not supported in this environment"));
      });
    };

    proto.formData = function() {
      return Promise.reject(new Error("FormData is not supported in this environment"));
    };
  }

  // =========================================================================
  // Request
  // =========================================================================

  function Request(input, init) {
    init = init || {};

    if (input instanceof Request) {
      this.url = input.url;
      this.method = input.method;
      this.headers = new Headers(input.headers);
      this.redirect = input.redirect;
      this.signal = input.signal;
      this.cache = input.cache;
      this.credentials = input.credentials;
      this.mode = input.mode;
      this.referrer = input.referrer;
      initBody(this, input._bodyText);
    } else {
      this.url = String(input);
      this.method = "GET";
      this.headers = new Headers();
      this.redirect = "follow";
      this.signal = null;
      this.cache = "default";
      this.credentials = "same-origin";
      this.mode = "cors";
      this.referrer = "about:client";
      initBody(this, null);
    }

    if (init.method !== undefined) this.method = init.method.toUpperCase();
    if (init.headers !== undefined) this.headers = new Headers(init.headers);
    if (init.redirect !== undefined) this.redirect = init.redirect;
    if (init.signal !== undefined) this.signal = init.signal;
    if (init.cache !== undefined) this.cache = init.cache;
    if (init.credentials !== undefined) this.credentials = init.credentials;
    if (init.mode !== undefined) this.mode = init.mode;
    if (init.referrer !== undefined) this.referrer = init.referrer;
    if (init.body !== undefined) initBody(this, init.body);
  }

  Request.prototype.clone = function() {
    return new Request(this);
  };

  applyBodyMixin(Request.prototype);

  // =========================================================================
  // Response
  // =========================================================================

  function Response(body, init) {
    init = init || {};

    this.status = init.status !== undefined ? init.status : 200;
    this.statusText = init.statusText !== undefined ? init.statusText : "";
    this.headers = new Headers(init.headers);
    this.type = "default";
    this.url = "";
    this.redirected = false;
    this.ok = this.status >= 200 && this.status < 300;

    initBody(this, body);
  }

  Response.prototype.clone = function() {
    var resp = new Response(this._bodyText, {
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

  Response.error = function() {
    var resp = new Response(null, { status: 0, statusText: "" });
    resp.type = "error";
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
    init = init || {};
    var body = JSON.stringify(data);
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
        signal.dispatchEvent({ type: "abort" });
      }
    }, ms);
    return signal;
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
    this.signal.dispatchEvent({ type: "abort" });
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
      var headersJson = JSON.stringify(request.headers._toArray());
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
        var response = new Response(parsed.body || "", {
          status: parsed.status,
          statusText: parsed.statusText || "",
          headers: responseHeaders
        });
        response.url = parsed.url || url;
        response.redirected = !!parsed.redirected;
        response.ok = parsed.status >= 200 && parsed.status < 300;
        response.type = "basic";

        resolve(response);
      }, function(err) {
        if (signal && abortHandler) signal.removeEventListener("abort", abortHandler);
        reject(new TypeError("Network request failed: " + (err && err.message ? err.message : String(err))));
      });
    });
  }

  // =========================================================================
  // Export to globalThis
  // =========================================================================

  globalThis.fetch = fetch;
  globalThis.Headers = Headers;
  globalThis.Request = Request;
  globalThis.Response = Response;
  globalThis.AbortController = AbortController;
  globalThis.AbortSignal = AbortSignal;

})(globalThis);
