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

  var VALID_TOKEN = /^[!#$%&'*+\-.^_`|~0-9A-Za-z]+$/;

  function validateName(name) {
    name = String(name);
    if (!VALID_TOKEN.test(name)) {
      throw new TypeError("Invalid header name: " + name);
    }
    return name;
  }

  function validateValue(value) {
    value = String(value);
    for (var i = 0; i < value.length; i++) {
      var c = value.charCodeAt(i);
      if (c > 0xFF || c === 0x00 || c === 0x0A || c === 0x0D) {
        throw new TypeError("Invalid header value");
      }
    }
    return value.replace(/^[\t ]+|[\t ]+$/g, "");
  }

  function Headers(init) {
    this._map = Object.create(null); // lowercase key -> [value, ...]

    if (init === null || (init !== undefined && typeof init !== "object")) {
      throw new TypeError("Failed to construct 'Headers': The provided value is not of type '(sequence<sequence<ByteString>> or record<ByteString, ByteString>)'");
    }

    if (init) {
      if (init instanceof Headers) {
        init.forEach(function(value, name) {
          this.append(name, value);
        }, this);
      } else if (init !== null && typeof init === "object" && typeof init[Symbol.iterator] === "function") {
        // Iterable (Array, custom iterators, etc.)
        var iter = init[Symbol.iterator]();
        var item;
        while (!(item = iter.next()).done) {
          var pair = item.value;
          if (!pair || typeof pair !== "object" || typeof pair[Symbol.iterator] !== "function") {
            throw new TypeError("Each header pair must be iterable");
          }
          var pairArr = Array.from ? Array.from(pair) : [].slice.call(pair);
          if (pairArr.length !== 2) {
            throw new TypeError("Each header pair must have exactly two elements");
          }
          this.append(pairArr[0], pairArr[1]);
        }
      } else if (typeof init === "object") {
        // Record<string, string> — per spec, sort keys and skip Symbols
        var names = Object.keys(init).sort();
        for (var j = 0; j < names.length; j++) {
          var name = validateName(names[j]);
          var val = validateValue(String(init[names[j]]));
          this.append(name, val);
        }
      }
    }
  }

  Headers.prototype.append = function(name, value) {
    var key = validateName(name).toLowerCase();
    value = validateValue(value);
    if (this._map[key]) {
      this._map[key].push(value);
    } else {
      this._map[key] = [value];
    }
  };

  // Trusted fast-path: skip validation for headers from the HTTP stack.
  // Like workerd's appendUnguarded — inbound headers are already validated by Hyper.
  Headers._fromTrusted = function(map) {
    var h = new Headers();
    h._map = map;
    return h;
  };

  Headers.prototype.delete = function(name) {
    var key = validateName(name).toLowerCase();
    delete this._map[key];
  };

  Headers.prototype.get = function(name) {
    var key = validateName(name).toLowerCase();
    var values = this._map[key];
    return values ? values.join(", ") : null;
  };

  Headers.prototype.has = function(name) {
    var key = validateName(name).toLowerCase();
    return key in this._map;
  };

  Headers.prototype.set = function(name, value) {
    var key = validateName(name).toLowerCase();
    value = validateValue(value);
    this._map[key] = [value];
  };

  Headers.prototype.forEach = function(callback, thisArg) {
    var keys = Object.keys(this._map).sort();
    for (var i = 0; i < keys.length; i++) {
      var key = keys[i];
      callback.call(thisArg, this._map[key].join(", "), key, this);
    }
  };

  Headers.prototype.entries = function() {
    var self = this;
    var keys = Object.keys(this._map).sort(); // snapshot once
    var index = 0;
    var iter = {
      next: function() {
        if (index >= keys.length) return { done: true, value: undefined };
        var key = keys[index++];
        return { done: false, value: [key, self._map[key].join(", ")] };
      },
      [Symbol.iterator]: function() { return this; }
    };
    Object.defineProperty(iter, Symbol.toStringTag, { value: "Iterator" });
    return iter;
  };

  Headers.prototype.keys = function() {
    var keys = Object.keys(this._map).sort(); // snapshot once
    var index = 0;
    var iter = {
      next: function() {
        if (index >= keys.length) return { done: true, value: undefined };
        return { done: false, value: keys[index++] };
      },
      [Symbol.iterator]: function() { return this; }
    };
    Object.defineProperty(iter, Symbol.toStringTag, { value: "Iterator" });
    return iter;
  };

  Headers.prototype.values = function() {
    var self = this;
    var keys = Object.keys(this._map).sort(); // snapshot once
    var index = 0;
    var iter = {
      next: function() {
        if (index >= keys.length) return { done: true, value: undefined };
        return { done: false, value: self._map[keys[index++]].join(", ") };
      },
      [Symbol.iterator]: function() { return this; }
    };
    Object.defineProperty(iter, Symbol.toStringTag, { value: "Iterator" });
    return iter;
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
    obj._bodyBytes = null;  // ArrayBuffer for binary bodies

    if (body === undefined || body === null) {
      obj._bodyText = "";
    } else if (typeof body === "string") {
      obj._bodyText = body;
    } else if (body instanceof ArrayBuffer) {
      obj._bodyBytes = body;
      // Also set _bodyText for backwards compat (lossy for binary)
      var bytes = new Uint8Array(body);
      var text = "";
      for (var i = 0; i < bytes.length; i++) text += String.fromCharCode(bytes[i]);
      obj._bodyText = text;
    } else if (ArrayBuffer.isView && ArrayBuffer.isView(body)) {
      obj._bodyBytes = body.buffer.slice(body.byteOffset, body.byteOffset + body.byteLength);
      var bytes = new Uint8Array(obj._bodyBytes);
      var text = "";
      for (var i = 0; i < bytes.length; i++) text += String.fromCharCode(bytes[i]);
      obj._bodyText = text;
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
      if (this._bodyUsed) return Promise.reject(new TypeError("Body has already been consumed."));
      this._bodyUsed = true;
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

  applyBodyMixin(Request.prototype);

  // =========================================================================
  // Response
  // =========================================================================

  function Response(body, init) {
    init = init || {};
    var status = init.status !== undefined ? init.status : 200;

    // Validate status range
    if (status < 200 || status > 599) {
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

    initBody(this, body);
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
        var response = Object.create(Response.prototype);
        response.status = parsed.status;
        response.statusText = parsed.statusText || "";
        response.headers = responseHeaders;
        response.type = "basic";
        response.url = parsed.url || url;
        response.redirected = !!parsed.redirected;
        response.ok = parsed.status >= 200 && parsed.status < 300;
        initBody(response, parsed.body || "");

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

  // =========================================================================
  // TextEncoder / TextDecoder polyfill
  // =========================================================================

  if (typeof TextEncoder === "undefined") {
    globalThis.TextEncoder = function TextEncoder() {};
    TextEncoder.prototype.encode = function(str) {
      str = String(str);
      var buf = new Uint8Array(str.length * 3); // UTF-8 worst case
      var pos = 0;
      for (var i = 0; i < str.length; i++) {
        var c = str.charCodeAt(i);
        if (c < 0x80) {
          buf[pos++] = c;
        } else if (c < 0x800) {
          buf[pos++] = 0xc0 | (c >> 6);
          buf[pos++] = 0x80 | (c & 0x3f);
        } else if (c >= 0xd800 && c <= 0xdbff) {
          var next = str.charCodeAt(++i);
          var cp = ((c - 0xd800) << 10) + (next - 0xdc00) + 0x10000;
          buf[pos++] = 0xf0 | (cp >> 18);
          buf[pos++] = 0x80 | ((cp >> 12) & 0x3f);
          buf[pos++] = 0x80 | ((cp >> 6) & 0x3f);
          buf[pos++] = 0x80 | (cp & 0x3f);
        } else {
          buf[pos++] = 0xe0 | (c >> 12);
          buf[pos++] = 0x80 | ((c >> 6) & 0x3f);
          buf[pos++] = 0x80 | (c & 0x3f);
        }
      }
      return buf.subarray(0, pos);
    };
  }

  if (typeof TextDecoder === "undefined") {
    globalThis.TextDecoder = function TextDecoder() {};
    TextDecoder.prototype.decode = function(buf) {
      if (!buf) return "";
      var bytes = new Uint8Array(buf.buffer || buf);
      var result = "";
      for (var i = 0; i < bytes.length;) {
        var b = bytes[i];
        if (b < 0x80) { result += String.fromCharCode(b); i++; }
        else if ((b & 0xe0) === 0xc0) {
          result += String.fromCharCode(((b & 0x1f) << 6) | (bytes[i+1] & 0x3f));
          i += 2;
        } else if ((b & 0xf0) === 0xe0) {
          result += String.fromCharCode(((b & 0x0f) << 12) | ((bytes[i+1] & 0x3f) << 6) | (bytes[i+2] & 0x3f));
          i += 3;
        } else {
          var cp = ((b & 0x07) << 18) | ((bytes[i+1] & 0x3f) << 12) | ((bytes[i+2] & 0x3f) << 6) | (bytes[i+3] & 0x3f);
          cp -= 0x10000;
          result += String.fromCharCode(0xd800 + (cp >> 10), 0xdc00 + (cp & 0x3ff));
          i += 4;
        }
      }
      return result;
    };
  }

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
