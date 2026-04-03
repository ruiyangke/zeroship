(function(globalThis) {
  "use strict";

  // =========================================================================
  // Base64 helpers (standard encoding, used for all crypto data transfer)
  // =========================================================================

  var _B64_CHARS = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

  var _B64 = {
    encode: function(buf) {
      var bytes = (buf instanceof Uint8Array) ? buf : new Uint8Array(buf);
      var out = "";
      for (var i = 0; i < bytes.length; i += 3) {
        var a = bytes[i], b = (i + 1 < bytes.length) ? bytes[i + 1] : 0, c = (i + 2 < bytes.length) ? bytes[i + 2] : 0;
        out += _B64_CHARS[a >> 2];
        out += _B64_CHARS[((a & 3) << 4) | (b >> 4)];
        out += (i + 1 < bytes.length) ? _B64_CHARS[((b & 15) << 2) | (c >> 6)] : "=";
        out += (i + 2 < bytes.length) ? _B64_CHARS[c & 63] : "=";
      }
      return out;
    },
    decode: function(str) {
      str = String(str).replace(/=+$/, "");
      var out = new Uint8Array(Math.floor(str.length * 3 / 4));
      var pos = 0;
      for (var i = 0; i < str.length; i += 4) {
        var a = _B64_CHARS.indexOf(str[i]);
        var b = _B64_CHARS.indexOf(str[i + 1]);
        var c = _B64_CHARS.indexOf(str[i + 2]);
        var d = _B64_CHARS.indexOf(str[i + 3]);
        out[pos++] = (a << 2) | (b >> 4);
        if (c >= 0) out[pos++] = ((b & 15) << 4) | (c >> 2);
        if (d >= 0) out[pos++] = ((c & 3) << 6) | d;
      }
      return out.subarray(0, pos);
    }
  };

  // =========================================================================
  // Helpers
  // =========================================================================

  function normalizeAlgorithm(algo) {
    if (typeof algo === "string") return { name: algo.toUpperCase() };
    if (algo && typeof algo === "object") {
      var result = {};
      for (var k in algo) result[k] = algo[k];
      if (result.name) result.name = result.name.toUpperCase();
      if (result.hash) {
        if (typeof result.hash === "string") result.hash = { name: result.hash.toUpperCase() };
        else if (result.hash && result.hash.name) result.hash = { name: result.hash.name.toUpperCase() };
      }
      return result;
    }
    throw new TypeError("Algorithm: must be a string or object");
  }

  function toBytes(data) {
    if (data instanceof ArrayBuffer) return new Uint8Array(data);
    if (ArrayBuffer.isView(data)) return new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
    throw new TypeError("Expected BufferSource");
  }

  // =========================================================================
  // Get native ops from the crypto object (registered by globals.rs)
  // =========================================================================

  var _crypto = globalThis.crypto || {};
  var _nativeGRV = _crypto.__cryptoGetRandomValues;
  var _nativeDigest = _crypto.__cryptoDigest;

  // =========================================================================
  // crypto.getRandomValues
  // =========================================================================

  _crypto.getRandomValues = function(typedArray) {
    if (!ArrayBuffer.isView(typedArray)) {
      throw new TypeError("Argument must be a typed array");
    }
    if (typedArray.byteLength === 0) return typedArray;
    if (typedArray.byteLength > 65536) {
      throw new DOMException("getRandomValues: quota exceeded", "QuotaExceededError");
    }
    var b64 = _nativeGRV(typedArray.byteLength);
    var bytes = _B64.decode(b64);
    new Uint8Array(typedArray.buffer, typedArray.byteOffset, typedArray.byteLength).set(bytes);
    return typedArray;
  };

  // =========================================================================
  // SubtleCrypto (digest only for now — more methods added in Tasks 3-7)
  // =========================================================================

  function SubtleCrypto() {}

  SubtleCrypto.prototype.digest = function(algorithm, data) {
    try {
      var algo = normalizeAlgorithm(algorithm);
      var bytes = toBytes(data);
      var b64 = _nativeDigest(algo.name, _B64.encode(bytes));
      return Promise.resolve(_B64.decode(b64).buffer);
    } catch (e) {
      return Promise.reject(e);
    }
  };

  _crypto.subtle = new SubtleCrypto();

  // Ensure crypto is on globalThis
  globalThis.crypto = _crypto;

})(globalThis);
