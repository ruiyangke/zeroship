# Web Crypto API Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Implement the Web Crypto API (SubtleCrypto) for the appbase V8 runtime using aws-lc-rs.

**Architecture:** Rust native ops via `#[appbase_op]` for all crypto operations. JS polyfill (`embed/crypto.js`) provides the SubtleCrypto/CryptoKey classes that validate arguments and dispatch to native ops. Key material stored in Rust `EventLoopState.key_store`, JS holds opaque u32 handles.

**Tech Stack:** aws-lc-rs 1.16.2, base64 crate, serde_json, `#[appbase_op]` proc macro.

**Design doc:** `docs/plans/2026-04-02-web-crypto-design.md`

---

### Task 1: Add aws-lc-rs dependency + key store in EventLoopState

**Files:**
- Modify: `crates/isolate_v8/Cargo.toml`
- Modify: `crates/isolate_v8/src/event_loop.rs:23-72`
- Modify: `crates/isolate_v8/src/crypto.rs`

**Step 1: Add dependencies to Cargo.toml**

Add after `ada-url = "3.4.4"`:
```toml
aws-lc-rs = "1"
base64 = "0.22"
```

**Step 2: Add KeyData enum and key store to event_loop.rs**

Add after the `use` block in `event_loop.rs` (after line 15):
```rust
// ---------------------------------------------------------------------------
// Crypto key store
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub(crate) enum Curve {
    P256,
    P384,
}

#[derive(Debug, Clone)]
pub(crate) enum KeyData {
    Symmetric { raw: Vec<u8> },
    EcPrivate { pkcs8_der: Vec<u8>, curve: Curve },
    EcPublic { raw: Vec<u8>, curve: Curve },
    RsaPrivate { pkcs8_der: Vec<u8> },
    RsaPublic { spki_der: Vec<u8> },
    Ed25519Private { pkcs8_der: Vec<u8> },
    Ed25519Public { raw: Vec<u8> },
}
```

Add two fields to `EventLoopState` struct (after `env_vars`):
```rust
    /// Crypto key store — key material stays in Rust, JS holds opaque u32 handles.
    pub(crate) key_store: HashMap<u32, KeyData>,
    pub(crate) next_key_id: u32,
```

Initialize in `new()` (after `env_vars: HashMap::new()`):
```rust
            key_store: HashMap::new(),
            next_key_id: 1,
```

**Step 3: Verify it compiles**

Run: `cargo check -p appbase-isolate-v8`

**Step 4: Commit**

```bash
git add crates/isolate_v8/Cargo.toml crates/isolate_v8/src/event_loop.rs
git commit -m "feat(crypto): add aws-lc-rs dep + key store in EventLoopState"
```

---

### Task 2: getRandomValues + digest native ops

**Files:**
- Modify: `crates/isolate_v8/src/crypto.rs`
- Modify: `crates/isolate_v8/src/globals.rs:260-268`
- Modify: `crates/isolate_v8/src/lib.rs` (add test)

**Step 1: Write failing tests in lib.rs**

Add to the `#[cfg(test)] mod tests` block:
```rust
    #[test]
    fn crypto_get_random_values() {
        init_v8();
        let mut isolate = Isolate::new(m(r#"
            export function test() {
                var buf = new Uint8Array(16);
                crypto.getRandomValues(buf);
                // Check that at least some bytes are non-zero
                var nonzero = 0;
                for (var i = 0; i < buf.length; i++) { if (buf[i] !== 0) nonzero++; }
                return nonzero > 0 ? "ok" : "all_zeros";
            }
        "#), no_env());
        let r = isolate.execute_request(r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#).unwrap();
        assert!(r.json.contains("ok"), "got: {}", r.json);
    }

    #[test]
    fn crypto_subtle_digest() {
        init_v8();
        let mut isolate = Isolate::new(m(r#"
            export async function test() {
                var data = new TextEncoder().encode("hello");
                var hash = await crypto.subtle.digest("SHA-256", data);
                // SHA-256 of "hello" = 2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824
                var bytes = new Uint8Array(hash);
                var hex = "";
                for (var i = 0; i < bytes.length; i++) hex += ("0" + bytes[i].toString(16)).slice(-2);
                return hex;
            }
        "#), no_env());
        let r = isolate.execute_request(r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#).unwrap();
        assert!(r.json.contains("2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"), "got: {}", r.json);
    }
```

**Step 2: Implement crypto ops in crypto.rs**

Rewrite `crates/isolate_v8/src/crypto.rs`:
```rust
//! Crypto APIs for V8 apps — backed by aws-lc-rs.

use appbase_ops::appbase_op;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;

/// `crypto.randomUUID() → string`
#[appbase_op]
fn crypto_random_uuid() -> String {
    let mut bytes = [0u8; 16];
    aws_lc_rs::rand::fill(&mut bytes).unwrap();
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15]
    )
}

/// `__cryptoGetRandomValues(len) → base64 string`
#[appbase_op]
fn crypto_get_random_values(len: u32) -> Result<String, crate::ops::OpError> {
    if len > 65536 {
        return Err(crate::ops::OpError::type_error("getRandomValues: length exceeds 65536"));
    }
    let mut buf = vec![0u8; len as usize];
    aws_lc_rs::rand::fill(&mut buf).map_err(|e| crate::ops::OpError::error(format!("RNG failed: {e}")))?;
    Ok(B64.encode(&buf))
}

/// `__cryptoDigest(algo, data_b64) → base64 hash`
#[appbase_op]
fn crypto_digest(algo: String, data_b64: String) -> Result<String, crate::ops::OpError> {
    let algorithm = match algo.as_str() {
        "SHA-1" => &aws_lc_rs::digest::SHA1_FOR_LEGACY_USE_ONLY,
        "SHA-256" => &aws_lc_rs::digest::SHA256,
        "SHA-384" => &aws_lc_rs::digest::SHA384,
        "SHA-512" => &aws_lc_rs::digest::SHA512,
        _ => return Err(crate::ops::OpError::type_error(format!("Unsupported digest algorithm: {algo}"))),
    };
    let data = B64.decode(&data_b64).map_err(|e| crate::ops::OpError::type_error(format!("Invalid base64: {e}")))?;
    let digest = aws_lc_rs::digest::digest(algorithm, &data);
    Ok(B64.encode(digest.as_ref()))
}
```

**Step 3: Register native callbacks in globals.rs**

Replace the `crypto.randomUUID()` block at line 260-268 with:
```rust
    // crypto namespace
    {
        let crypto = v8::Object::new(scope);

        let uuid_fn = v8::Function::new(scope, crate::crypto::crypto_random_uuid_callback).unwrap();
        let uuid_key = v8::String::new(scope, "randomUUID").unwrap();
        crypto.set(scope, uuid_key.into(), uuid_fn.into());

        let grv_fn = v8::Function::new(scope, crate::crypto::crypto_get_random_values_callback).unwrap();
        let grv_key = v8::String::new(scope, "__cryptoGetRandomValues").unwrap();
        crypto.set(scope, grv_key.into(), grv_fn.into());

        let digest_fn = v8::Function::new(scope, crate::crypto::crypto_digest_callback).unwrap();
        let digest_key = v8::String::new(scope, "__cryptoDigest").unwrap();
        crypto.set(scope, digest_key.into(), digest_fn.into());

        let crypto_key = v8::String::new(scope, "crypto").unwrap();
        global.set(scope, crypto_key.into(), crypto.into());
    }
```

**Step 4: Write the JS polyfill**

Create `crates/isolate_v8/src/embed/crypto.js`:
```js
(function(globalThis) {
  "use strict";

  var _B64 = {
    _chars: "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/",
    encode: function(buf) {
      var bytes = new Uint8Array(buf.buffer || buf);
      var out = "";
      for (var i = 0; i < bytes.length; i += 3) {
        var a = bytes[i], b = bytes[i+1] || 0, c = bytes[i+2] || 0;
        out += this._chars[a >> 2];
        out += this._chars[((a & 3) << 4) | (b >> 4)];
        out += (i+1 < bytes.length) ? this._chars[((b & 15) << 2) | (c >> 6)] : "=";
        out += (i+2 < bytes.length) ? this._chars[c & 63] : "=";
      }
      return out;
    },
    decode: function(str) {
      str = str.replace(/=+$/, "");
      var out = new Uint8Array(Math.floor(str.length * 3 / 4));
      var pos = 0;
      for (var i = 0; i < str.length; i += 4) {
        var a = this._chars.indexOf(str[i]);
        var b = this._chars.indexOf(str[i+1]);
        var c = this._chars.indexOf(str[i+2]);
        var d = this._chars.indexOf(str[i+3]);
        out[pos++] = (a << 2) | (b >> 4);
        if (c >= 0) out[pos++] = ((b & 15) << 4) | (c >> 2);
        if (d >= 0) out[pos++] = ((c & 3) << 6) | d;
      }
      return out.subarray(0, pos);
    }
  };

  function normalizeAlgorithm(algo) {
    if (typeof algo === "string") return { name: algo.toUpperCase() };
    if (algo && typeof algo === "object") {
      var result = {};
      for (var k in algo) result[k] = algo[k];
      if (result.name) result.name = result.name.toUpperCase();
      if (result.hash) {
        if (typeof result.hash === "string") result.hash = { name: result.hash.toUpperCase() };
        else if (result.hash.name) result.hash = { name: result.hash.name.toUpperCase() };
      }
      return result;
    }
    throw new TypeError("Algorithm must be a string or object");
  }

  function toBytes(data) {
    if (data instanceof ArrayBuffer) return new Uint8Array(data);
    if (ArrayBuffer.isView(data)) return new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
    throw new TypeError("Expected BufferSource");
  }

  // getRandomValues — fills typed array with random bytes
  var _origCrypto = globalThis.crypto || {};
  var _nativeGRV = _origCrypto.__cryptoGetRandomValues;
  var _nativeDigest = _origCrypto.__cryptoDigest;

  globalThis.crypto = _origCrypto;

  globalThis.crypto.getRandomValues = function(typedArray) {
    if (!(typedArray instanceof Int8Array || typedArray instanceof Uint8Array ||
          typedArray instanceof Int16Array || typedArray instanceof Uint16Array ||
          typedArray instanceof Int32Array || typedArray instanceof Uint32Array ||
          typedArray instanceof Uint8ClampedArray ||
          typedArray instanceof BigInt64Array || typedArray instanceof BigUint64Array)) {
      throw new TypeError("Argument must be an integer typed array");
    }
    if (typedArray.byteLength > 65536) {
      throw new DOMException("getRandomValues: quota exceeded", "QuotaExceededError");
    }
    var b64 = _nativeGRV(typedArray.byteLength);
    var bytes = _B64.decode(b64);
    new Uint8Array(typedArray.buffer, typedArray.byteOffset, typedArray.byteLength).set(bytes);
    return typedArray;
  };

  // SubtleCrypto — digest only for now, more methods added in subsequent tasks
  function SubtleCrypto() {}

  SubtleCrypto.prototype.digest = function(algorithm, data) {
    var algo = normalizeAlgorithm(algorithm);
    var bytes = toBytes(data);
    var b64 = _nativeDigest(algo.name, _B64.encode(bytes));
    return Promise.resolve(_B64.decode(b64).buffer);
  };

  globalThis.crypto.subtle = new SubtleCrypto();

})(globalThis);
```

**Step 5: Load the crypto polyfill**

Add to `crates/isolate_v8/src/runtime.rs` after the `URL_JS` line:
```rust
pub(crate) const CRYPTO_JS: &str = include_str!("embed/crypto.js");
```

Add `CRYPTO_JS` to the import in `crates/isolate_v8/src/isolate.rs:16`:
```rust
use crate::runtime::{thread_cpu_time, HttpResult, RequestResult, DISPATCH_JS, FETCH_JS, URL_JS, CRYPTO_JS};
```

Change the polyfill loop at `isolate.rs:95`:
```rust
        for polyfill in [FETCH_JS, URL_JS, CRYPTO_JS] {
```

Do the same for `concurrent.rs` — add `CRYPTO_JS` to the import and polyfill loop.

**Step 6: Remove getrandom dependency**

Remove `getrandom = "0.2"` from `crates/isolate_v8/Cargo.toml` (replaced by `aws_lc_rs::rand`).

**Step 7: Run tests**

Run: `cargo test -p appbase-isolate-v8 --lib -- --nocapture`
Expected: All tests pass including the two new crypto tests.

Run: `cargo test -p appbase-isolate-v8 --test examples -- --nocapture`
Expected: All 11 example tests pass.

**Step 8: Commit**

```bash
git add -A
git commit -m "feat(crypto): getRandomValues + subtle.digest (SHA-256/384/512)"
```

---

### Task 3: Key store + importKey + exportKey + generateKey

**Files:**
- Modify: `crates/isolate_v8/src/crypto.rs` — add import/export/generate ops
- Modify: `crates/isolate_v8/src/globals.rs` — register new callbacks
- Modify: `crates/isolate_v8/src/embed/crypto.js` — add CryptoKey class, importKey/exportKey/generateKey methods
- Add tests in `crates/isolate_v8/src/lib.rs`

This is the foundation task. Implement:
- `crypto_import_key(state, params_json)` — parses format + key data + algorithm, stores in key_store, returns key_id
- `crypto_export_key(state, format, key_id)` — reads from key_store, serializes to requested format
- `crypto_generate_key(state, params_json)` — generates key via aws-lc-rs, stores in key_store

Supported algorithms for importKey: HMAC (raw), AES-GCM/CBC/CTR (raw), ECDSA P-256/P-384 (raw/pkcs8/spki), RSA (pkcs8/spki), Ed25519 (raw/pkcs8), HKDF (raw), PBKDF2 (raw).

Supported algorithms for generateKey: HMAC, AES-GCM/CBC/CTR, ECDSA P-256/P-384, Ed25519, RSA-OAEP.

JS CryptoKey class holds `{_handle, algorithm, type, extractable, usages}`.

Test: `importKey("raw", key, "HMAC", ...) → exportKey("raw", key) → compare bytes`.

**Step 1–8:** Write tests, implement Rust ops, add JS methods, register callbacks, run tests, commit.

---

### Task 4: sign + verify (HMAC, ECDSA, RSA, Ed25519)

**Files:**
- Modify: `crates/isolate_v8/src/crypto.rs` — add sign/verify ops
- Modify: `crates/isolate_v8/src/globals.rs` — register callbacks
- Modify: `crates/isolate_v8/src/embed/crypto.js` — add sign/verify methods
- Add tests in `crates/isolate_v8/src/lib.rs`

Implement:
- `crypto_sign(state, params_json)` — reads key from key_store, signs data, returns base64 signature
- `crypto_verify(state, params_json)` — reads key, verifies signature, returns bool

params_json contains: `{algorithm: {name, hash?}, keyId, data_b64, signature_b64?}`

Test: HMAC sign → verify roundtrip. ECDSA P-256 generateKey → sign → verify. JWT HS256 flow.

---

### Task 5: encrypt + decrypt (AES-GCM, AES-CBC, RSA-OAEP)

**Files:**
- Modify: `crates/isolate_v8/src/crypto.rs`
- Modify: `crates/isolate_v8/src/globals.rs`
- Modify: `crates/isolate_v8/src/embed/crypto.js`
- Add tests

Implement:
- `crypto_encrypt(state, params_json)` — AES-GCM (with iv + aad), AES-CBC (with iv), RSA-OAEP
- `crypto_decrypt(state, params_json)` — reverse

params_json for AES-GCM: `{algorithm: {name: "AES-GCM", iv_b64, additionalData_b64?}, keyId, data_b64}`
params_json for AES-CBC: `{algorithm: {name: "AES-CBC", iv_b64}, keyId, data_b64}`
params_json for RSA-OAEP: `{algorithm: {name: "RSA-OAEP", label_b64?}, keyId, data_b64}`

Test: AES-GCM generateKey → encrypt → decrypt roundtrip. AES-CBC same. RSA-OAEP same.

---

### Task 6: deriveBits + deriveKey (HKDF, PBKDF2)

**Files:**
- Modify: `crates/isolate_v8/src/crypto.rs`
- Modify: `crates/isolate_v8/src/globals.rs`
- Modify: `crates/isolate_v8/src/embed/crypto.js`
- Add tests

Implement:
- `crypto_derive_bits(state, params_json)` — HKDF extract+expand or PBKDF2
- `crypto_derive_key(state, params_json)` — derive_bits + import into a new key

params_json for HKDF: `{algorithm: {name: "HKDF", hash, salt_b64, info_b64}, keyId, length}`
params_json for PBKDF2: `{algorithm: {name: "PBKDF2", hash, salt_b64, iterations}, keyId, length}`

Test: PBKDF2 with known salt → compare output. HKDF with known info → compare.

---

### Task 7: wrapKey + unwrapKey (AES-KW, AES-GCM)

**Files:**
- Modify: `crates/isolate_v8/src/crypto.rs`
- Modify: `crates/isolate_v8/src/globals.rs`
- Modify: `crates/isolate_v8/src/embed/crypto.js`
- Add tests

Implement:
- `crypto_wrap_key(state, params_json)` — export key, encrypt with wrapping key
- `crypto_unwrap_key(state, params_json)` — decrypt, import

Supported wrapping algorithms: AES-KW (via `aws_lc_rs::key_wrap`), AES-GCM.

Test: generateKey(AES) → wrapKey → unwrapKey → compare exported bytes.

---

### Task 8: JWK import + export

**Files:**
- Modify: `crates/isolate_v8/src/crypto.rs`
- Add tests

Implement JWK format support for importKey/exportKey:
- Symmetric (HMAC, AES): `{kty: "oct", k: base64url}`
- EC (ECDSA, ECDH): `{kty: "EC", crv, x, y, d?}` — base64url big-endian coords
- RSA: `{kty: "RSA", n, e, d?, p?, q?, dp?, dq?, qi?}` — base64url components
- Ed25519: `{kty: "OKP", crv: "Ed25519", x, d?}`

This requires base64url encoding (not standard base64) and manual DER construction for EC/RSA.

Add `base64` crate's URL_SAFE_NO_PAD engine for base64url.

Test: importKey("jwk", {...}, "HMAC") → exportKey("jwk") → compare fields.

---

### Task 9: Integration test — full JWT HS256 flow

**Files:**
- Create: `examples/jwt-validator.js`
- Modify: `crates/isolate_v8/tests/examples.rs`

Write an example app that:
1. Imports an HMAC key from a raw secret
2. Signs a JWT payload (header.payload → HMAC-SHA256)
3. Verifies the JWT signature
4. Returns the decoded payload

This validates the complete crypto pipeline end-to-end.

---

## Summary

| Task | What | Key files |
|---|---|---|
| 1 | aws-lc-rs dep + key store | Cargo.toml, event_loop.rs |
| 2 | getRandomValues + digest | crypto.rs, crypto.js, globals.rs |
| 3 | importKey/exportKey/generateKey | crypto.rs, crypto.js |
| 4 | sign/verify | crypto.rs, crypto.js |
| 5 | encrypt/decrypt | crypto.rs, crypto.js |
| 6 | deriveBits/deriveKey | crypto.rs, crypto.js |
| 7 | wrapKey/unwrapKey | crypto.rs, crypto.js |
| 8 | JWK format | crypto.rs |
| 9 | JWT integration test | examples/, tests/ |
