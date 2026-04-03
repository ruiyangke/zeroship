(function(globalThis) {
  "use strict";

  // =========================================================================
  // Base64 helpers (only used for small structured params: IV, AAD, salt, info, label)
  // Main data payloads use zero-copy ArrayBuffer bridge.
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
  // getRandomValues is registered directly as a native callback on crypto object
  // by globals.rs — it fills the TypedArray backing store directly (zero copies).
  // No JS override needed.
  var _nativeDigest = _crypto.__cryptoDigest;
  var _nativeImportKey = _crypto.__cryptoImportKey;
  var _nativeExportKey = _crypto.__cryptoExportKey;
  var _nativeGenerateKey = _crypto.__cryptoGenerateKey;
  var _nativeSign = _crypto.__cryptoSign;
  var _nativeVerify = _crypto.__cryptoVerify;
  var _nativeEncrypt = _crypto.__cryptoEncrypt;
  var _nativeDecrypt = _crypto.__cryptoDecrypt;
  var _nativeDeriveBits = _crypto.__cryptoDeriveBits;
  var _nativeDeriveKey = _crypto.__cryptoDeriveKey;

  // =========================================================================
  // CryptoKey — opaque handle to Rust key store
  // =========================================================================

  function CryptoKey(handle, algorithm, type, extractable, usages) {
    this._handle = handle;
    this.algorithm = Object.freeze(algorithm);
    this.type = type;
    this.extractable = extractable;
    this.usages = Object.freeze(usages.slice());
  }

  // =========================================================================
  // SubtleCrypto
  // =========================================================================

  function SubtleCrypto() {}

  SubtleCrypto.prototype.digest = function(algorithm, data) {
    try {
      var algo = normalizeAlgorithm(algorithm);
      var bytes = toBytes(data);
      // Zero-serialization: pass ArrayBuffer directly, receive ArrayBuffer back.
      // No base64 encode/decode — the macro reads from TypedArray backing store
      // and returns a new ArrayBuffer.
      var result = _nativeDigest(algo.name, bytes);
      return Promise.resolve(result);
    } catch (e) {
      return Promise.reject(e);
    }
  };

  SubtleCrypto.prototype.importKey = function(format, keyData, algorithm, extractable, keyUsages) {
    try {
      var algo = normalizeAlgorithm(algorithm);
      var bytes;
      if (format === "raw" || format === "pkcs8" || format === "spki") {
        bytes = toBytes(keyData);
      } else {
        throw new TypeError("Unsupported format: " + format);
      }
      // Zero-serialization: pass key material as ArrayBuffer directly.
      // Algorithm config stays as JSON string (small, structured).
      var algoJson = JSON.stringify(algo);
      var result = JSON.parse(_nativeImportKey(format, bytes, algoJson));
      return Promise.resolve(new CryptoKey(result.keyId, algo, result.type, !!extractable, keyUsages || []));
    } catch (e) {
      return Promise.reject(e);
    }
  };

  SubtleCrypto.prototype.exportKey = function(format, key) {
    try {
      if (!(key instanceof CryptoKey)) throw new TypeError("Expected CryptoKey");
      if (!key.extractable) throw new DOMException("Key is not extractable", "InvalidAccessError");
      // Zero-serialization: receive key material as ArrayBuffer directly.
      var result = _nativeExportKey(format, key._handle);
      return Promise.resolve(result);
    } catch (e) {
      return Promise.reject(e);
    }
  };

  SubtleCrypto.prototype.generateKey = function(algorithm, extractable, keyUsages) {
    try {
      var algo = normalizeAlgorithm(algorithm);
      var params = JSON.stringify({
        algorithm: algo,
        extractable: !!extractable,
        usages: keyUsages || []
      });
      var result = JSON.parse(_nativeGenerateKey(params));
      if (result.keyId !== undefined) {
        // Symmetric key
        return Promise.resolve(new CryptoKey(result.keyId, algo, "secret", !!extractable, keyUsages || []));
      }
      // Key pair
      var pubUsages = [];
      var privUsages = [];
      for (var i = 0; i < (keyUsages || []).length; i++) {
        var u = keyUsages[i];
        if (u === "verify" || u === "encrypt" || u === "wrapKey") pubUsages.push(u);
        else privUsages.push(u);
      }
      return Promise.resolve({
        publicKey: new CryptoKey(result.publicKeyId, algo, "public", true, pubUsages),
        privateKey: new CryptoKey(result.privateKeyId, algo, "private", !!extractable, privUsages)
      });
    } catch (e) {
      return Promise.reject(e);
    }
  };

  SubtleCrypto.prototype.sign = function(algorithm, key, data) {
    try {
      if (!(key instanceof CryptoKey)) throw new TypeError("Expected CryptoKey");
      var algo = normalizeAlgorithm(algorithm);
      var hash = algo.hash ? algo.hash.name : "SHA-256";
      var bytes = toBytes(data);
      // Zero-serialization: pass data as ArrayBuffer, receive signature as ArrayBuffer.
      var result = _nativeSign(algo.name, hash, key._handle, bytes);
      return Promise.resolve(result);
    } catch (e) {
      return Promise.reject(e);
    }
  };

  SubtleCrypto.prototype.verify = function(algorithm, key, signature, data) {
    try {
      if (!(key instanceof CryptoKey)) throw new TypeError("Expected CryptoKey");
      var algo = normalizeAlgorithm(algorithm);
      var hash = algo.hash ? algo.hash.name : "SHA-256";
      var dataBytes = toBytes(data);
      var sigBytes = toBytes(signature);
      // Zero-serialization: pass data and signature as ArrayBuffer directly.
      var result = _nativeVerify(algo.name, hash, key._handle, dataBytes, sigBytes);
      return Promise.resolve(result === "true");
    } catch (e) {
      return Promise.reject(e);
    }
  };

  SubtleCrypto.prototype.encrypt = function(algorithm, key, data) {
    try {
      if (!(key instanceof CryptoKey)) throw new TypeError("Expected CryptoKey");
      var algo = normalizeAlgorithm(algorithm);
      var bytes = toBytes(data);
      // Build algo config JSON — IV/AAD/label stay base64-encoded (small, structured).
      // Only the main data payload crosses as ArrayBuffer.
      var algoParams = { name: algo.name };
      if (algo.hash) algoParams.hash = algo.hash;
      if (algo.iv) algoParams.iv = _B64.encode(toBytes(algo.iv));
      if (algo.additionalData) algoParams.additionalData = _B64.encode(toBytes(algo.additionalData));
      if (algo.tagLength) algoParams.tagLength = algo.tagLength;
      if (algo.label) algoParams.label = _B64.encode(toBytes(algo.label));
      var algoJson = JSON.stringify(algoParams);
      // Zero-serialization: pass data as ArrayBuffer, receive ciphertext as ArrayBuffer.
      var result = _nativeEncrypt(algoJson, key._handle, bytes);
      return Promise.resolve(result);
    } catch (e) {
      return Promise.reject(e);
    }
  };

  SubtleCrypto.prototype.decrypt = function(algorithm, key, data) {
    try {
      if (!(key instanceof CryptoKey)) throw new TypeError("Expected CryptoKey");
      var algo = normalizeAlgorithm(algorithm);
      var bytes = toBytes(data);
      // Build algo config JSON — IV/AAD/label stay base64-encoded (small, structured).
      // Only the main data payload crosses as ArrayBuffer.
      var algoParams = { name: algo.name };
      if (algo.hash) algoParams.hash = algo.hash;
      if (algo.iv) algoParams.iv = _B64.encode(toBytes(algo.iv));
      if (algo.additionalData) algoParams.additionalData = _B64.encode(toBytes(algo.additionalData));
      if (algo.tagLength) algoParams.tagLength = algo.tagLength;
      if (algo.label) algoParams.label = _B64.encode(toBytes(algo.label));
      var algoJson = JSON.stringify(algoParams);
      // Zero-serialization: pass ciphertext as ArrayBuffer, receive plaintext as ArrayBuffer.
      var result = _nativeDecrypt(algoJson, key._handle, bytes);
      return Promise.resolve(result);
    } catch (e) {
      return Promise.reject(e);
    }
  };

  SubtleCrypto.prototype.deriveBits = function(algorithm, baseKey, length) {
    try {
      if (!(baseKey instanceof CryptoKey)) throw new TypeError("Expected CryptoKey");
      var algo = normalizeAlgorithm(algorithm);
      var algoParams = { name: algo.name };
      if (algo.hash) algoParams.hash = algo.hash;
      if (algo.salt) algoParams.salt = _B64.encode(toBytes(algo.salt));
      if (algo.info) algoParams.info = _B64.encode(toBytes(algo.info));
      if (algo.iterations) algoParams.iterations = algo.iterations;
      var params = JSON.stringify({
        algorithm: algoParams,
        keyId: baseKey._handle,
        length: length
      });
      // Zero-serialization: derived bits returned as ArrayBuffer directly.
      var result = _nativeDeriveBits(params);
      return Promise.resolve(result);
    } catch (e) { return Promise.reject(e); }
  };

  SubtleCrypto.prototype.deriveKey = function(algorithm, baseKey, derivedKeyAlgorithm, extractable, keyUsages) {
    try {
      if (!(baseKey instanceof CryptoKey)) throw new TypeError("Expected CryptoKey");
      var algo = normalizeAlgorithm(algorithm);
      var derivedAlgo = normalizeAlgorithm(derivedKeyAlgorithm);
      var algoParams = { name: algo.name };
      if (algo.hash) algoParams.hash = algo.hash;
      if (algo.salt) algoParams.salt = _B64.encode(toBytes(algo.salt));
      if (algo.info) algoParams.info = _B64.encode(toBytes(algo.info));
      if (algo.iterations) algoParams.iterations = algo.iterations;
      var params = JSON.stringify({
        algorithm: algoParams,
        keyId: baseKey._handle,
        derivedKeyAlgorithm: derivedAlgo,
        extractable: !!extractable,
        usages: keyUsages || []
      });
      var result = JSON.parse(_nativeDeriveKey(params));
      return Promise.resolve(new CryptoKey(result.keyId, derivedAlgo, "secret", !!extractable, keyUsages || []));
    } catch (e) { return Promise.reject(e); }
  };

  _crypto.subtle = new SubtleCrypto();

  // Ensure crypto and CryptoKey are on globalThis
  globalThis.crypto = _crypto;
  globalThis.CryptoKey = CryptoKey;

})(globalThis);
