(function () {
  const Socket = globalThis.__zsNetSocket;
  const decoder = new TextDecoder();

  function normalizeCa(ca) {
    if (ca == null) return undefined;
    if (Array.isArray(ca)) {
      return ca.map(normalizeCa).filter((v) => v != null && v !== "").join("\n");
    }
    if (typeof ca === "string") return ca;
    if (ca instanceof ArrayBuffer) return decoder.decode(new Uint8Array(ca));
    if (ArrayBuffer.isView(ca)) {
      return decoder.decode(new Uint8Array(ca.buffer, ca.byteOffset, ca.byteLength));
    }
    return String(ca);
  }

  function normalizeOptions(options) {
    if (!options || typeof options !== "object") {
      throw new TypeError("tls.connect options must be an object");
    }
    const socket = options.socket || undefined;
    const host = String(options.host || options.hostname || "localhost");
    const port = Number(options.port);
    const servername = String(options.servername || options.host || options.hostname || "localhost");
    const rejectUnauthorized = options.rejectUnauthorized !== false;
    const ca = normalizeCa(options.ca);

    if (!socket) {
      if (!Number.isInteger(port) || port <= 0 || port > 65535) {
        throw new RangeError("port must be an integer between 1 and 65535");
      }
    }

    return { socket, host, port, servername, rejectUnauthorized, ca };
  }

  class TLSSocket extends Socket {
    constructor(socketOrOptions = undefined) {
      const sourceSocket =
        socketOrOptions && socketOrOptions.socket ? socketOrOptions.socket : undefined;
      if (sourceSocket) {
        if (!sourceSocket._native || typeof sourceSocket._native.startTls !== "function") {
          throw new TypeError("tls.connect socket must be a node:net Socket");
        }
        super({
          __zsNativeSocket: sourceSocket._native,
          __zsAdoptFrom: sourceSocket,
        });
      } else {
        super();
      }
      this.encrypted = true;
      this.authorized = false;
      this.authorizationError = null;
      this._secureConnecting = false;
      this.on("secureConnect", () => {
        this._secureConnecting = false;
        this.connecting = false;
        this.pending = false;
        this.readyState = "open";
        this._flushPending();
      });
    }

    _isOpening() {
      return this._secureConnecting || super._isOpening();
    }

    connect(options, cb) {
      const opts = normalizeOptions(options);
      if (opts.socket) {
        return startTls(opts, cb);
      }
      if (typeof cb === "function") this.once("secureConnect", cb);
      this._secureConnecting = true;
      this.connecting = true;
      this.pending = true;
      this.destroyed = false;
      this.readyState = "opening";
      this._native.validateTls(opts.rejectUnauthorized);
      deferNativeStart(() => {
        this._native.connectTls(
          opts.host,
          opts.port,
          opts.servername,
          opts.rejectUnauthorized,
          opts.ca,
        );
      });
      return this;
    }
  }

  function deferNativeStart(fn) {
    if (typeof queueMicrotask === "function") queueMicrotask(fn);
    else Promise.resolve().then(fn);
  }

  function startTls(opts, cb) {
    const socket = new TLSSocket({ socket: opts.socket });
    if (typeof cb === "function") socket.once("secureConnect", cb);
    socket._secureConnecting = true;
    socket.connecting = true;
    socket.pending = true;
    socket.readyState = "opening";
    socket._native.validateTls(opts.rejectUnauthorized);
    deferNativeStart(() => {
      socket._native.startTls(opts.servername, opts.rejectUnauthorized, opts.ca);
    });
    return socket;
  }

  function connect(options, cb) {
    if (typeof cb !== "function" && typeof arguments[1] === "function") {
      cb = arguments[1];
    }
    const opts = normalizeOptions(options);
    if (opts.socket) {
      return startTls(opts, cb);
    }
    return new TLSSocket().connect(opts, cb);
  }

  const ns = { TLSSocket, connect };
  ns.default = ns;
  return ns;
})();
