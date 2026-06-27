(function () {
  const EventEmitter = globalThis.__zsEventEmitter;
  const NativeSocket = globalThis.__zsNativeSocket;
  const HIGH_WATER_MARK = 16 * 1024;

  function normalizeArgs(args) {
    let cb;
    const last = args[args.length - 1];
    if (typeof last === "function") {
      cb = last;
      args = args.slice(0, -1);
    }

    let opts;
    if (typeof args[0] === "object" && args[0] !== null) {
      opts = { ...args[0] };
    } else {
      opts = { port: args[0], host: args[1] };
    }

    if (opts.path != null) {
      const err = new Error("Unix domain sockets are not implemented");
      err.code = "ERR_NOT_IMPLEMENTED";
      throw err;
    }
    const port = Number(opts.port);
    if (!Number.isInteger(port) || port <= 0 || port > 65535) {
      throw new RangeError("port must be an integer between 1 and 65535");
    }
    const host = String(opts.host || opts.hostname || "localhost");
    return { host, port, cb };
  }

  class Socket extends EventEmitter {
    constructor(_opts = undefined) {
      super();
      this._native = new NativeSocket();
      this._native.attach(this);
      this.__zsEmit = EventEmitter.prototype.emit;
      this.connecting = false;
      this.destroyed = false;
      this.pending = true;
      this.readyState = "closed";
      this._timeoutId = null;
      this._encoding = null;
      this._pendingWrites = [];
      this._pendingBytes = 0;
      this._pendingEnd = false;

      this.on("connect", () => {
        this.connecting = false;
        this.pending = false;
        this.readyState = "open";
        this._flushPending();
      });
      this.on("ready", () => {
        this.connecting = false;
        this.pending = false;
        this.readyState = "open";
        this._flushPending();
      });
      this.on("end", () => {
        if (!this.destroyed) this.readyState = "readOnly";
      });
      this.on("close", () => {
        this.connecting = false;
        this.pending = false;
        this.destroyed = true;
        this.readyState = "closed";
        if (this._timeoutId !== null) {
          clearTimeout(this._timeoutId);
          this._timeoutId = null;
        }
      });
    }

    connect(...args) {
      const { host, port, cb } = normalizeArgs(args);
      if (cb) this.once("connect", cb);
      this.connecting = true;
      this.pending = true;
      this.destroyed = false;
      this.readyState = "opening";
      this._native.connect(host, port);
      return this;
    }

    _estimateWriteSize(data) {
      if (typeof data === "string") return new TextEncoder().encode(data).byteLength;
      if (data && typeof data.byteLength === "number") return data.byteLength;
      if (data && typeof data.length === "number") return data.length;
      return String(data).length;
    }

    _isOpening() {
      return this.connecting || this.readyState === "opening";
    }

    _deferCallback(cb) {
      if (typeof cb !== "function") return;
      if (typeof setImmediate === "function") setImmediate(cb);
      else setTimeout(cb, 0);
    }

    _writeNow(data, encoding, cb) {
      const ok = this._native.write(data, encoding == null ? undefined : String(encoding));
      this._deferCallback(cb);
      return ok;
    }

    _flushPending() {
      if (this._flushingPending) return;
      this._flushingPending = true;
      try {
        const writes = this._pendingWrites;
        this._pendingWrites = [];
        this._pendingBytes = 0;
        for (const item of writes) {
          this._writeNow(item.data, item.encoding, item.cb);
        }
        if (this._pendingEnd) {
          this._pendingEnd = false;
          this._native.end();
          this.readyState = "writeOnly";
        }
      } finally {
        this._flushingPending = false;
      }
    }

    write(data, encoding, cb) {
      if (typeof encoding === "function") {
        cb = encoding;
        encoding = undefined;
      }
      if (this._isOpening()) {
        this._pendingBytes += this._estimateWriteSize(data);
        this._pendingWrites.push({ data, encoding, cb });
        return this._pendingBytes <= HIGH_WATER_MARK;
      }
      return this._writeNow(data, encoding, cb);
    }

    end(data, encoding, cb) {
      if (typeof data === "function") {
        cb = data;
        data = undefined;
        encoding = undefined;
      } else if (typeof encoding === "function") {
        cb = encoding;
        encoding = undefined;
      }
      if (data !== undefined && data !== null) this.write(data, encoding);
      if (typeof cb === "function") this.once("close", cb);
      if (this._isOpening()) {
        this._pendingEnd = true;
      } else {
        this._native.end();
        this.readyState = this.readyState === "readOnly" ? "closed" : "writeOnly";
      }
      return this;
    }

    destroy(err) {
      if (this.destroyed) return this;
      if (err) this.emit("error", err);
      this._native.destroy();
      return this;
    }

    pause() {
      this._native.pause();
      return this;
    }

    resume() {
      this._native.resume();
      return this;
    }

    setNoDelay(on = true) {
      this._native.setNoDelay(Boolean(on));
      return this;
    }

    setKeepAlive(on = true, initialDelay = 0) {
      this._native.setKeepAlive(Boolean(on), Number(initialDelay) || 0);
      return this;
    }

    setTimeout(ms, cb) {
      if (typeof cb === "function") this.once("timeout", cb);
      if (this._timeoutId !== null) clearTimeout(this._timeoutId);
      const delay = Number(ms) || 0;
      if (delay > 0) {
        this._timeoutId = setTimeout(() => {
          this._timeoutId = null;
          this.emit("timeout");
        }, delay);
      }
      return this;
    }

    cork() { return undefined; }
    uncork() { return undefined; }
    ref() { return this; }
    unref() { return this; }
    setEncoding(enc) {
      this._encoding = enc == null ? null : String(enc);
      return this;
    }

    get remoteAddress() { return this._native.remoteAddress; }
    get remotePort() { return this._native.remotePort; }
    get bytesRead() { return this._native.bytesRead; }
    get bytesWritten() { return this._native.bytesWritten; }
  }

  function createConnection(...args) {
    return new Socket().connect(...args);
  }

  function isIPv4(input) {
    if (typeof input !== "string") return false;
    const parts = input.split(".");
    if (parts.length !== 4) return false;
    return parts.every((part) => {
      if (!/^(0|[1-9]\d{0,2})$/.test(part)) return false;
      const n = Number(part);
      return n >= 0 && n <= 255;
    });
  }

  function isIPv6(input) {
    if (typeof input !== "string" || !input.includes(":")) return false;
    try {
      new URL("http://[" + input + "]/");
      return true;
    } catch (_) {
      return false;
    }
  }

  function isIP(input) {
    if (isIPv4(input)) return 4;
    if (isIPv6(input)) return 6;
    return 0;
  }

  const ns = {
    Socket,
    createConnection,
    connect: createConnection,
    isIP,
    isIPv4,
    isIPv6,
  };
  ns.default = ns;
  globalThis.__zsNetSocket = Socket;
  return ns;
})()
