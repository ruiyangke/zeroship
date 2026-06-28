(function () {
  const Socket = globalThis.__zsNetSocket;
  const pemDecoder = new TextDecoder("utf-8", { fatal: true });

  function tlsError(code, message) {
    const err = new Error(message);
    err.code = code;
    return err;
  }

  function assertPemCa(ca) {
    if (ca == null || ca === "") return undefined;
    if (
      !String(ca).includes("-----BEGIN CERTIFICATE-----") ||
      !String(ca).includes("-----END CERTIFICATE-----")
    ) {
      throw tlsError("ERR_TLS_CA_INVALID", "ca must contain PEM-encoded certificates");
    }
    return String(ca);
  }

  function decodeCaBytes(bytes) {
    try {
      return pemDecoder.decode(bytes);
    } catch (_) {
      throw tlsError("ERR_TLS_CA_INVALID", "ca must contain PEM-encoded certificates");
    }
  }

  function normalizeCa(ca) {
    if (ca == null) return undefined;
    if (Array.isArray(ca)) {
      return ca.map(normalizeCa).filter((v) => v != null && v !== "").join("\n");
    }
    if (typeof ca === "string") return assertPemCa(ca);
    if (ca instanceof ArrayBuffer) {
      return assertPemCa(decodeCaBytes(new Uint8Array(ca)));
    }
    if (ArrayBuffer.isView(ca)) {
      return assertPemCa(decodeCaBytes(new Uint8Array(ca.buffer, ca.byteOffset, ca.byteLength)));
    }
    return assertPemCa(String(ca));
  }

  function rejectClientAuthOptions(options) {
    for (const key of ["cert", "key", "passphrase", "pfx"]) {
      if (options[key] != null) {
        throw tlsError(
          "ERR_NOT_IMPLEMENTED",
          `tls.connect ${key} client-certificate option is not implemented`,
        );
      }
    }
  }

  function createSecureContext(options = {}) {
    if (!options || typeof options !== "object") {
      throw new TypeError("tls.createSecureContext options must be an object");
    }
    rejectClientAuthOptions(options);
    return { ca: normalizeCa(options.ca) };
  }

  function normalizeOptions(options) {
    if (!options || typeof options !== "object") {
      throw new TypeError("tls.connect options must be an object");
    }
    rejectClientAuthOptions(options);
    const secureContext = options.secureContext || undefined;
    const socket = options.socket || undefined;
    const host = String(options.host || options.hostname || "localhost");
    const port = Number(options.port);
    const servername = String(options.servername || options.host || options.hostname || "localhost");
    const rejectUnauthorized = options.rejectUnauthorized !== false;
    const ca = normalizeCa(options.ca) || normalizeCa(secureContext && secureContext.ca);
    const verifyIdentity = typeof options.checkServerIdentity !== "function";

    if (!socket) {
      if (!Number.isInteger(port) || port <= 0 || port > 65535) {
        throw new RangeError("port must be an integer between 1 and 65535");
      }
    }

    return { socket, host, port, servername, rejectUnauthorized, ca, verifyIdentity };
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
      this._sourceSocket = sourceSocket || null;
      this.authorized = false;
      this.authorizationError = null;
      this._secureConnecting = false;
      this.on("secureConnect", () => {
        this._secureConnecting = false;
        this.connecting = false;
        this.pending = false;
        this.readyState = "open";
        this._flushPendingWrites();
      });
      this.on("close", (hadError) => {
        if (this._sourceSocket && !this._sourceSocket.destroyed) {
          this._sourceSocket.__zsEmit.call(this._sourceSocket, "close", Boolean(hadError));
        }
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
      this._native.connectTls(
        opts.host,
        opts.port,
        opts.servername,
        opts.rejectUnauthorized,
        opts.ca,
        opts.verifyIdentity,
      );
      return this;
    }
  }

  function startTls(opts, cb) {
    const socket = new TLSSocket({ socket: opts.socket });
    if (typeof cb === "function") socket.once("secureConnect", cb);
    socket._secureConnecting = true;
    socket.connecting = true;
    socket.pending = true;
    socket.readyState = "opening";
    socket._native.validateTls(opts.rejectUnauthorized);
    socket._native.startTls(
      opts.servername,
      opts.rejectUnauthorized,
      opts.ca,
      opts.verifyIdentity,
    );
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

  const ns = { TLSSocket, connect, createSecureContext };
  ns.default = ns;
  return ns;
})();
