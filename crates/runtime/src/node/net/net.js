(function () {
  const EventEmitter = globalThis.__zsEventEmitter;
  const NativeSocket = globalThis.__zsNativeSocket;

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
      const opts = _opts && typeof _opts === "object" ? _opts : {};
      const adopted = opts.__zsNativeSocket;
      const adoptedFrom = opts.__zsAdoptFrom;
      this._native = adopted || new NativeSocket();
      this._native.attach(this);
      this.__zsEmit = EventEmitter.prototype.emit;
      this.connecting = adoptedFrom ? Boolean(adoptedFrom.connecting) : false;
      this.destroyed = adoptedFrom ? Boolean(adoptedFrom.destroyed) : false;
      this.pending = adoptedFrom ? Boolean(adoptedFrom.pending) : true;
      this.readyState = adoptedFrom ? String(adoptedFrom.readyState || "open") : "closed";
      this.readable = !this.destroyed;
      this.writable = !this.destroyed;
      this.writableEnded = false;
      this.writableDestroyed = this.destroyed;
      this.readableEnded = false;
      this.readableDestroyed = this.destroyed;
      this._timeoutId = null;
      this._encoding = null;
      this._encryptedOverride = undefined;
      this._pendingWriteQueue = [];
      this._pendingEnd = false;

      this.on("connect", () => {
        this.connecting = false;
        this.pending = false;
        this.readyState = "open";
        this._flushPendingWrites();
      });
      this.on("ready", () => {
        this.connecting = false;
        this.pending = false;
        this.readyState = "open";
        this._flushPendingWrites();
      });
      this.on("secureConnect", () => {
        this._flushPendingWrites();
      });
      this.on("end", () => {
        this.readable = false;
        this.readableEnded = true;
        if (!this.destroyed) this.readyState = "readOnly";
      });
      this.on("close", () => {
        this.connecting = false;
        this.pending = false;
        this.destroyed = true;
        this.readable = false;
        this.writable = false;
        this.readableDestroyed = true;
        this.writableDestroyed = true;
        this.readyState = "closed";
        if (this._timeoutId !== null) {
          clearTimeout(this._timeoutId);
          this._timeoutId = null;
        }
        this._pendingWriteQueue = [];
        this._pendingEnd = false;
      });
    }

    connect(...args) {
      const { host, port, cb } = normalizeArgs(args);
      if (cb) this.once("connect", cb);
      this.connecting = true;
      this.pending = true;
      this.destroyed = false;
      this.readable = true;
      this.writable = true;
      this.readableDestroyed = false;
      this.writableDestroyed = false;
      this.readyState = "opening";
      this._native.connect(host, port);
      return this;
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

    write(data, encoding, cb) {
      if (typeof encoding === "function") {
        cb = encoding;
        encoding = undefined;
      }
      if (this._isOpening()) {
        this._pendingWriteQueue.push([data, encoding, cb]);
        return true;
      }
      return this._writeNow(data, encoding, cb);
    }

    _flushPendingWrites() {
      if (this._isOpening()) return;
      const queue = this._pendingWriteQueue;
      this._pendingWriteQueue = [];
      for (const item of queue) {
        this._writeNow(item[0], item[1], item[2]);
      }
      if (this._pendingEnd) {
        this._pendingEnd = false;
        this._native.end();
      }
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
      if (typeof cb === "function") this.once("close", cb);
      if (this._isOpening()) {
        if (data !== undefined && data !== null) this._pendingWriteQueue.push([data, encoding, undefined]);
        this._pendingEnd = true;
        this.writable = false;
        this.writableEnded = true;
        this.readyState = this.readyState === "readOnly" ? "closed" : "writeOnly";
        return this;
      }
      if (data !== undefined && data !== null) this.write(data, encoding);
      this._native.end();
      this.writable = false;
      this.writableEnded = true;
      this.readyState = this.readyState === "readOnly" ? "closed" : "writeOnly";
      return this;
    }

    destroy(err) {
      if (this.destroyed) return this;
      if (err) this.emit("error", err);
      this.readable = false;
      this.writable = false;
      this.readableDestroyed = true;
      this.writableDestroyed = true;
      this._pendingWriteQueue = [];
      this._pendingEnd = false;
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
    get encrypted() {
      return this._encryptedOverride === undefined
        ? Boolean(this._native.encrypted)
        : Boolean(this._encryptedOverride);
    }
    set encrypted(value) { this._encryptedOverride = Boolean(value); }
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
