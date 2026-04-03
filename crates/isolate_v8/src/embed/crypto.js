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
  var _nativeImportKey = _crypto.__cryptoImportKey;
  var _nativeExportKey = _crypto.__cryptoExportKey;
  var _nativeGenerateKey = _crypto.__cryptoGenerateKey;
  var _nativeSign = _crypto.__cryptoSign;
  var _nativeVerify = _crypto.__cryptoVerify;
  var _nativeEncrypt = _crypto.__cryptoEncrypt;
  var _nativeDecrypt = _crypto.__cryptoDecrypt;

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
      var b64 = _nativeDigest(algo.name, _B64.encode(bytes));
      return Promise.resolve(_B64.decode(b64).buffer);
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
      var params = JSON.stringify({
        format: format,
        keyData: _B64.encode(bytes),
        algorithm: algo,
        extractable: !!extractable,
        usages: keyUsages || []
      });
      var result = JSON.parse(_nativeImportKey(params));
      return Promise.resolve(new CryptoKey(result.keyId, algo, result.type, !!extractable, keyUsages || []));
    } catch (e) {
      return Promise.reject(e);
    }
  };

  SubtleCrypto.prototype.exportKey = function(format, key) {
    try {
      if (!(key instanceof CryptoKey)) throw new TypeError("Expected CryptoKey");
      if (!key.extractable) throw new DOMException("Key is not extractable", "InvalidAccessError");
      var params = JSON.stringify({ format: format, keyId: key._handle });
      var result = JSON.parse(_nativeExportKey(params));
      var bytes = _B64.decode(result.keyData);
      return Promise.resolve(bytes.buffer);
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
      var bytes = toBytes(data);
      var params = JSON.stringify({
        algorithm: algo,
        keyId: key._handle,
        data: _B64.encode(bytes)
      });
      var result = _nativeSign(params);
      return Promise.resolve(_B64.decode(result).buffer);
    } catch (e) {
      return Promise.reject(e);
    }
  };

  SubtleCrypto.prototype.verify = function(algorithm, key, signature, data) {
    try {
      if (!(key instanceof CryptoKey)) throw new TypeError("Expected CryptoKey");
      var algo = normalizeAlgorithm(algorithm);
      var dataBytes = toBytes(data);
      var sigBytes = toBytes(signature);
      var params = JSON.stringify({
        algorithm: algo,
        keyId: key._handle,
        data: _B64.encode(dataBytes),
        signature: _B64.encode(sigBytes)
      });
      var result = _nativeVerify(params);
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
      var algoParams = { name: algo.name };
      if (algo.hash) algoParams.hash = algo.hash;
      if (algo.iv) algoParams.iv = _B64.encode(toBytes(algo.iv));
      if (algo.additionalData) algoParams.additionalData = _B64.encode(toBytes(algo.additionalData));
      if (algo.tagLength) algoParams.tagLength = algo.tagLength;
      if (algo.label) algoParams.label = _B64.encode(toBytes(algo.label));
      var params = JSON.stringify({
        algorithm: algoParams,
        keyId: key._handle,
        data: _B64.encode(bytes)
      });
      var result = _nativeEncrypt(params);
      return Promise.resolve(_B64.decode(result).buffer);
    } catch (e) {
      return Promise.reject(e);
    }
  };

  SubtleCrypto.prototype.decrypt = function(algorithm, key, data) {
    try {
      if (!(key instanceof CryptoKey)) throw new TypeError("Expected CryptoKey");
      var algo = normalizeAlgorithm(algorithm);
      var bytes = toBytes(data);
      var algoParams = { name: algo.name };
      if (algo.hash) algoParams.hash = algo.hash;
      if (algo.iv) algoParams.iv = _B64.encode(toBytes(algo.iv));
      if (algo.additionalData) algoParams.additionalData = _B64.encode(toBytes(algo.additionalData));
      if (algo.tagLength) algoParams.tagLength = algo.tagLength;
      if (algo.label) algoParams.label = _B64.encode(toBytes(algo.label));
      var params = JSON.stringify({
        algorithm: algoParams,
        keyId: key._handle,
        data: _B64.encode(bytes)
      });
      var result = _nativeDecrypt(params);
      return Promise.resolve(_B64.decode(result).buffer);
    } catch (e) {
      return Promise.reject(e);
    }
  };

  _crypto.subtle = new SubtleCrypto();

  // Ensure crypto and CryptoKey are on globalThis
  globalThis.crypto = _crypto;
  globalThis.CryptoKey = CryptoKey;

})(globalThis);
