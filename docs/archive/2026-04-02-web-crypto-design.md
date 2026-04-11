# Web Crypto API Design

**Date:** 2026-04-02
**Backend:** aws-lc-rs 1.16.2 (BoringSSL-backed, single crate covers all algorithms)

## Architecture

Rust native ops + JS polyfill wrapper. Same pattern as URL (ada-url native + JS class).

```
JS polyfill (embed/crypto.js)              Rust native ops (crypto.rs)
┌──────────────────────────────┐    ┌────────────────────────────────────┐
│ crypto.getRandomValues(buf)──┼────┼→ crypto_get_random_values(len)     │
│ crypto.randomUUID()       ───┼────┼→ crypto_random_uuid() [exists]     │
│                              │    │                                    │
│ class SubtleCrypto            │    │  Backend: aws-lc-rs 1.16.2        │
│   digest(algo, data)      ───┼────┼→ crypto_digest(algo, data_b64)     │
│   sign(algo, key, data)   ───┼────┼→ crypto_sign(params_json)          │
│   verify(algo, key, data) ───┼────┼→ crypto_verify(params_json)        │
│   encrypt(algo, key, data)───┼────┼→ crypto_encrypt(params_json)       │
│   decrypt(algo, key, data)───┼────┼→ crypto_decrypt(params_json)       │
│   importKey(...)          ───┼────┼→ crypto_import_key(params_json)     │
│   exportKey(...)          ───┼────┼→ crypto_export_key(format, key_id)  │
│   generateKey(...)        ───┼────┼→ crypto_generate_key(params_json)   │
│   deriveBits(...)         ───┼────┼→ crypto_derive_bits(params_json)    │
│                              │    │                                    │
│ class CryptoKey               │    │  Key store (in EventLoopState):    │
│   _handle: u32 (opaque ID)   │    │  HashMap<u32, KeyData>             │
│   algorithm, type, usages    │    │  Key material stays in Rust        │
│   extractable                │    │  Never exposed to JS               │
└──────────────────────────────┘    └────────────────────────────────────┘
```

## Key Design Decisions

1. **Key material stays in Rust.** JS CryptoKey holds only a u32 handle. Raw key bytes live in `EventLoopState.key_store`. Prevents JS from leaking secrets.

2. **All ops are synchronous.** Same as workerd. Single-threaded V8 — spawn_blocking adds complexity for no benefit. PBKDF2 100K iterations ~50ms is acceptable.

3. **Base64 for binary data.** Data crosses JS↔Rust boundary as base64-encoded strings via `#[appbase_op]`. Optimize to ArrayBuffer later if needed.

4. **JS validates, Rust executes.** JS SubtleCrypto normalizes algorithm names, validates key usages, checks extractable flags. Rust ops do the actual crypto.

5. **aws-lc-rs chosen over ring.** ring lacks AES-CBC, AES-CTR, RSA-OAEP, RSA keygen, AES-KW — would need 9 gap-fill crates. aws-lc-rs covers everything in one crate. Already coexists with ring (pulled by reqwest/rustls) — no conflict.

## Rust Data Types

```rust
// Added to EventLoopState
key_store: HashMap<u32, KeyData>,
next_key_id: u32,

enum KeyData {
    Symmetric { raw: Vec<u8> },
    EcPrivate { pkcs8_der: Vec<u8>, curve: Curve },
    EcPublic { raw: Vec<u8>, curve: Curve },
    RsaPrivate { pkcs8_der: Vec<u8> },
    RsaPublic { spki_der: Vec<u8> },
    Ed25519Private { pkcs8_der: Vec<u8> },
    Ed25519Public { raw: Vec<u8> },
}

enum Curve { P256, P384 }
```

## Native Ops (13 total, all via `#[appbase_op]`)

| Op | Signature | Returns |
|---|---|---|
| `crypto_get_random_values` | `(len: u32) → String` | base64 random bytes |
| `crypto_digest` | `(algo: String, data: String) → String` | base64 hash |
| `crypto_import_key` | `(state, params: String) → String` | JSON {keyId, algorithm, type} |
| `crypto_export_key` | `(state, format: String, key_id: u32) → String` | JSON key data |
| `crypto_generate_key` | `(state, params: String) → String` | JSON {keyId} or {publicKeyId, privateKeyId} |
| `crypto_sign` | `(state, params: String) → String` | base64 signature |
| `crypto_verify` | `(state, params: String) → bool` | true/false |
| `crypto_encrypt` | `(state, params: String) → String` | base64 ciphertext |
| `crypto_decrypt` | `(state, params: String) → String` | base64 plaintext |
| `crypto_derive_bits` | `(state, params: String) → String` | base64 derived bits |
| `crypto_derive_key` | `(state, params: String) → String` | JSON {keyId} |
| `crypto_wrap_key` | `(state, params: String) → String` | base64 wrapped key |
| `crypto_unwrap_key` | `(state, params: String) → String` | JSON {keyId} |

## Algorithm Support Matrix

| Method | HMAC | ECDSA | Ed25519 | RSA-PKCS1 | RSA-PSS | RSA-OAEP | AES-GCM | AES-CBC | AES-CTR | AES-KW | HKDF | PBKDF2 |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| sign | x | x | x | x | x | | | | | | | |
| verify | x | x | x | x | x | | | | | | | |
| encrypt | | | | | | x | x | x | x | | | |
| decrypt | | | | | | x | x | x | x | | | |
| generateKey | x | x | x | | | x | x | x | x | x | | |
| importKey | x | x | x | x | x | x | x | x | x | x | x | x |
| exportKey | x | x | x | x | x | x | x | x | x | x | | |
| deriveBits | | | | | | | | | | | x | x |
| wrapKey | | | | | | x | x | x | x | x | | |
| unwrapKey | | | | | | x | x | x | x | x | | |

## JS Polyfill (embed/crypto.js)

```js
// CryptoKey — opaque handle to Rust key store
class CryptoKey {
    constructor(handle, algorithm, type, extractable, usages) {
        this._handle = handle;
        this.algorithm = Object.freeze(algorithm);
        this.type = type;
        this.extractable = extractable;
        this.usages = Object.freeze(usages);
    }
}

// SubtleCrypto — validates args, dispatches to native ops
class SubtleCrypto {
    async digest(algorithm, data) { ... }
    async sign(algorithm, key, data) { ... }
    async verify(algorithm, key, data, signature) { ... }
    async encrypt(algorithm, key, data) { ... }
    async decrypt(algorithm, key, data) { ... }
    async importKey(format, keyData, algorithm, extractable, usages) { ... }
    async exportKey(format, key) { ... }
    async generateKey(algorithm, extractable, usages) { ... }
    async deriveBits(algorithm, baseKey, length) { ... }
    async deriveKey(algorithm, baseKey, derivedKeyAlgo, extractable, usages) { ... }
    async wrapKey(format, key, wrappingKey, wrapAlgo) { ... }
    async unwrapKey(format, wrappedKey, unwrappingKey, ...) { ... }
}

globalThis.crypto.subtle = new SubtleCrypto();
globalThis.crypto.getRandomValues = function(buf) { ... };
```

## Key Format Support

| Format | Import | Export | Notes |
|---|---|---|---|
| `raw` | Symmetric keys, EC public keys | Same | Bare bytes |
| `pkcs8` | EC/RSA/Ed25519 private keys | Same | DER encoded |
| `spki` | EC/RSA/Ed25519 public keys | Same | DER encoded |
| `jwk` | All key types | All key types | Manual base64url + serde |

JWK requires a manual conversion layer (base64url decode → DER construction for import, reverse for export). This is ~200 LOC of serde + base64url encoding.

## Known Limitations

1. **X25519 static key import** — aws-lc-rs only supports ephemeral X25519. generateKey works, importKey("pkcs8") does not. Rare in practice.
2. **HKDF Prk is opaque** — cannot extract raw PRK bytes from aws-lc-rs. deriveBits works for the common expand-only case.
3. **Binary data as base64** — adds ~33% overhead on large payloads. Can optimize to ArrayBuffer transfer later.
4. **P-521 curve** — not prioritized (P-256 and P-384 cover 99% of use cases).

## Implementation Order

1. getRandomValues + digest (no key store, simplest)
2. Key store + importKey/exportKey/generateKey (foundation)
3. sign/verify (HMAC, ECDSA, RSA, Ed25519)
4. encrypt/decrypt (AES-GCM, AES-CBC, RSA-OAEP)
5. deriveBits/deriveKey (HKDF, PBKDF2)
6. wrapKey/unwrapKey (AES-KW, AES-GCM)
7. JWK import/export

## Testing

Per-algorithm roundtrip tests:
- digest: SHA-256 known-answer test
- HMAC: sign → verify roundtrip
- ECDSA P-256: generateKey → sign → verify
- RSA: importKey(pkcs8) → sign → importKey(spki) → verify
- AES-GCM: generateKey → encrypt → decrypt roundtrip
- AES-CBC: same roundtrip
- PBKDF2: derive with known salt → compare to known output
- HKDF: derive with known info → compare
- JWT HS256: full importKey → sign → verify flow
- exportKey/importKey roundtrip for each format (raw, pkcs8, spki, jwk)
