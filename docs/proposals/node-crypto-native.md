# Native Node.js `node:crypto` design

**Date:** 2026-05-02 (v1) · 2026-05-02 (v2 post-review)
**Status:** Draft v2 (post-review) — implementation pending
**Spec:** Node.js `node:crypto` API reference — https://nodejs.org/api/crypto.html
**Companion specs:**
- Node.js `crypto.webcrypto` — https://nodejs.org/api/webcrypto.html (Node's bridge between node:crypto and WHATWG WebCrypto; instructive for our bridging design)
- W3C Web Cryptography API Level 2 — https://w3c.github.io/webcrypto/ (already shipped — see `docs/proposals/webcrypto-native.md`)
- Node.js `Buffer` — https://nodejs.org/api/buffer.html (the IO type Node's crypto APIs return)
- WHATWG Encoding — https://encoding.spec.whatwg.org/ (utf8/utf16le/latin1 input encodings)
**Reference impls:**
- workerd (C++/BoringSSL) — `refs/workerd/src/node/internal/{crypto.h,crypto_dh.c++,crypto_keys.c++,crypto_hkdf.c++,crypto_pbkdf2.c++,crypto_x509.c++}`
- Bun (Zig + JS facade) — https://github.com/oven-sh/bun/tree/main/src/bun.js/node/node_crypto.zig + `src/js/node/crypto.ts`
- Deno (Rust ops + JS polyfill) — https://github.com/denoland/deno/tree/main/ext/node/ops/crypto + `ext/node/polyfills/internal/crypto/*.ts`
- Node.js (gold standard, C++ over OpenSSL) — https://github.com/nodejs/node/tree/main/src/crypto + `lib/internal/crypto/`
**Underlying RFCs cited:**
- RFC 8017 — PKCS #1 v2.2 (RSA: PKCS1v1_5, OAEP, PSS)
- RFC 5480 — Elliptic Curve Public Key Information (P-256/384/521 OIDs)
- RFC 5208 / 5958 — PKCS #8 / Asymmetric Key Packages (private key encoding)
- RFC 5280 — X.509 v3 certificate profile (`X509Certificate`)
- RFC 7468 — PEM textual encoding (`-----BEGIN X-----` framing)
- RFC 7914 — `scrypt` Password-Based Key Derivation
- RFC 5869 — HKDF
- RFC 8018 — PKCS #5 v2.1 (PBKDF2)
- RFC 7517 / 7518 — JWK / JWA
- RFC 4648 — base64 / base64url / hex encodings
- RFC 5915 — Elliptic Curve Private Key (SEC1 form)
- RFC 8032 — EdDSA (Ed25519, Ed448)
- RFC 7748 — X25519 / X448
- RFC 7693 — BLAKE2 (Node ≥18 exposes via `createHash`)
- RFC 8439 — ChaCha20-Poly1305 (Node ≥17)
- RFC 3526 — Modular Exponential (MODP) Diffie-Hellman groups
- RFC 7919 — Negotiated Finite Field DH groups (`ffdhe2048` … `ffdhe8192`)
- RFC 5288 — AES-GCM Cipher Suites for TLS (NIST SP 800-38D — GCM)
- NIST SP 800-38A — Block cipher modes (CBC / CFB / OFB / CTR / ECB)
- FIPS 180-4 — Secure Hash Standard
**aws-lc-rs / aws-lc-sys:** workspace deps (`crates/runtime/Cargo.toml:16`); already used by `crypto_native/`
**WebIDL:** N/A — node:crypto is NOT a WebIDL surface; types are documented in JSDoc-flavoured markdown at https://nodejs.org/api/crypto.html. We adopt Node's signatures verbatim; the entry-shim layer translates JS args to the shared kernel.
**Tests:** Node's own `test/parallel/test-crypto-*.js` — https://github.com/nodejs/node/tree/main/test/parallel (vendored subset, run as smoke tests in our V8). No WPT for node:crypto.

**Depends on:**
- `crates/runtime/src/crypto_native/` (already shipped, `docs/proposals/webcrypto-native.md`) — node:crypto's `webcrypto` / `subtle` / `getRandomValues` exports re-expose the existing native classes; node:crypto's `KeyObject` shares the underlying `KeyMaterial` enum + key-store bridges (D-N4).
- `crates/runtime/src/crypto.rs::fast_random` — the thread-local CSPRNG entropy buffer we already ship; the new `randomBytes` / `randomFill` ops call it. Stays untouched.
- The `#[v8_class]` macro (`crates/runtime-macros/`) — node:crypto adds ~17 new v8 classes; the macro shape (Box-in-internal-field-0, brand-check, SameObject getters) is reused unmodified.
- The Node-compat module loader at `sdks/vite-plugin/src/node-compat.ts` (the JS shim we are replacing) and the `fetchModule` interception at `sdks/vite-plugin/src/environment.ts:191` — the bridge that delivers our synthetic `"node:crypto"` module specifier into V8.

**Unblocks:**
- AI-builder reliability for the next 80% of npm packages: `jsonwebtoken`, `bcrypt`, `bcryptjs`, `scrypt-js`, `crypto-js`, `tweetnacl`, `node-forge`, `pino` / `winston` (HMAC for log signing), `nanoid` / `uuid` / `cuid2` / `ulid` (random), every Postgres / MySQL / Redis driver (HMAC for SCRAM auth), `axios` / `got` / `undici` (HMAC for AWS sigv4 + OAuth1), `firebase-admin` / `googleapis` / `aws-sdk` (signing). The hash + HMAC + KDF surface alone covers ~70% of cross-package crypto calls; the design lands all of it native.
- Deletion of the JS shim at `sdks/vite-plugin/src/node-compat.ts:67-127` (60 LOC) and elimination of the `__cryptoHashSync` / `__cryptoHmacSync` ad-hoc V8 callbacks at `crates/runtime/src/crypto.rs:128-212` (84 LOC) and their global installs at `crates/runtime/src/init.rs:1444-1453`.
- Spec-correct Node error semantics: every `error.code === "ERR_CRYPTO_*"` path that npm packages branch on works as on Node (today the JS shim throws plain `Error("__cryptoHashSync: unsupported algorithm")` — packages that catch `ERR_CRYPTO_HASH_FINALIZED` or check `e.code === "ERR_OSSL_*"` silently misbehave).
- Streaming hash/HMAC. The current shim collects chunks into a JS array and decodes via `TextDecoder` on each `update()` (a bug: binary data passed as `Uint8Array` is decoded as UTF-8 then re-encoded — silently corrupts non-text bytes). Native implementations call `digest::Context::update` per-chunk over the original byte slice — zero copies, correct for binary data.
- A `KeyObject ↔ CryptoKey` bridge (the spec-mandated `KeyObject.from(cryptoKey)` and `crypto.subtle.importKey('jwk', keyObject.export(...))`) so creator apps using JOSE libraries (Web Crypto handle) can interop with apps using `jsonwebtoken` (Node KeyObject handle).

## Revision history

- **v2 (2026-05-02 post-review)** — Addresses 14 CRITICAL + 30 MAJOR + 25 missing-concept findings from `/tmp/zeroship-reviews/node-crypto-review.md`. Net effect:
  - AEAD semantics rewritten to be CCM-correct: `createCipheriv` `authTagLength`, `setAAD` `plaintextLength`/`encoding`, `setAuthTag` ordering distinct per mode (CCM pre-update; GCM/OCB/ChaCha20 pre-final; GCM-only post-final tag inspection on Cipher).
  - aws-lc-rs API audit: `secp256k1` is `signature::ECDSA_P256K1_SHA256_*` not `ECDSA_K256` (rename in §III.2). Encrypted PKCS#8 export/import drops to `aws-lc-sys` raw FFI (PKCS8_decrypt + PBES2 ASN.1 by hand) — documented as ~250 LOC in `kernel/pkcs8_enc.rs`; new D-N33 records the gap.
  - RSA-PSS sentinel translation: `RSA_PSS_SALTLEN_DIGEST=-1`, `RSA_PSS_SALTLEN_MAX_SIGN=-2`, `RSA_PSS_SALTLEN_AUTO=-2` are now translated to concrete byte counts in `parse_sign_key_input` BEFORE the kernel call. New D-N34 records the policy.
  - Missing post-quantum surface added (Stage E placeholder): ML-DSA, ML-KEM, SLH-DSA `asymmetricKeyType` values; `crypto.encapsulate` / `crypto.decapsulate` (Node v22+ KEM API).
  - Stream `Transform` inheritance documented for Hash / Hmac / Cipher / Decipher / Sign / Verify (Node ships them as `stream.Transform` subclasses; `pipeline(...)` must work).
  - `update(data, encoding)` encoding-ignore-for-non-string rule wired into `buffer::extract_input`.
  - `Hmac.copy()` re-confirmed absent in Node (critic mistake; counter-cited against `node/lib/internal/crypto/hash.js`).
  - `ECDH.setPublicKey` is shipped (deprecated, with warning) — not thrown.
  - 25 missing concepts (argon2 status, `Certificate` SPKAC, `KeyObject.toCryptoKey`, X509 `checkEmail` / `checkIP` / `toJSON`, `randomFillSync` validation, etc.) tracked in §II / §X / new §II.15.
  - Decision register updated: D-N6 contradiction resolved (always sync; threshold concept dropped from decisions table); D-N10 reaffirmed (Hmac has no copy in Node); D-N33 (encrypted PKCS#8 via aws-lc-sys raw FFI); D-N34 (PSS saltLength sentinels normalised pre-kernel); D-N35 (stream.Transform inheritance); D-N36 (post-quantum stub registry).

  Each fix carries a "(addresses critic CRITICAL #N)" / "(MAJOR #N)" tag inline so the next review can grep coverage. Doc grew from 3,261 to ~4,000 LOC.

- **v1 (2026-05-02)** — Initial design. Replaces the JS shim at `sdks/vite-plugin/src/node-compat.ts:67-127` (60 LOC) and the two ad-hoc `__cryptoHashSync` / `__cryptoHmacSync` V8 callbacks at `crates/runtime/src/crypto.rs:121-212` (92 LOC). Adds ~5,800 native Rust LOC across `crypto_node/` (the new node:crypto surface) + `crypto_kernel/` (the shared backend extracted from `crypto_native/`'s per-algorithm files). The first-class native node:crypto surface ships in two stages: Stage 1 covers the Tier-1 calls every npm package makes (hash, HMAC, randomBytes, KDFs, KeyObject + import/export, sign/verify, cipher/decipher, webcrypto bridge); Stage 2 fills the long tail (DH groups, X.509, prime generation, FIPS controls, legacy ciphers).

  Decisions D-N1 through D-N32 cover: dual-surface coexistence with shared kernel (D-N1, D-N2); class layout (D-N3); `KeyObject` ↔ `CryptoKey` bridge via shared `Arc<KeyMaterial>` (D-N4); sync vs async dispatch (D-N5, D-N6); buffer integration (D-N7); error mapping (D-N8); Hash / Hmac / Cipher / Decipher / Sign / Verify class shape (D-N9 through D-N14); KDF dispatch (D-N15); webcrypto bridge identity (D-N16); randomness ops (D-N17); algorithm-name canonicalisation across surfaces (D-N18); KeyObject import accept / export emit (D-N19); X.509 deferred to Stage 2 (D-N20); DH deferred policy (D-N21); legacy cipher policy (D-N22); ChaCha20-Poly1305 ship-now (D-N23); scrypt parameters + memory cap (D-N24); FIPS controls (D-N25); the synthetic ESM module install path (D-N26); module-shim cutover cadence (D-N27); algorithm registry (D-N28); the `getCipherInfo` / `getCipherInfo` static surface (D-N29); zeroize on Drop for secret material (D-N30); `timingSafeEqual` policy (D-N31); the macro-extension list (D-N32).

## Top matter

### Goals

1. **First-class native node:crypto.** Every commonly-used `node:crypto` export is a real Rust-backed binding installed under the synthetic ESM specifier `"node:crypto"`. No JS facade layered over WebCrypto; no `__sync()` shim; no Promise-blocked-then-thrown hack. The synthetic module's exports point directly at native v8_class methods (Hash / Hmac / Cipher / Decipher / Sign / Verify / KeyObject / DH / ECDH / Hkdf / Pbkdf2 / Scrypt / X509Certificate) and free functions (`randomBytes`, `randomUUID`, `timingSafeEqual`, `getCiphers`, `getHashes`, `pbkdf2`, `scrypt`, `hkdf`, ...).
2. **Coexist with WebCrypto, share the kernel.** The already-shipped `crypto_native/` (WebCrypto) and the new `crypto_node/` are sibling surfaces over a third-party-quality kernel: an extracted `crypto_kernel/` module that owns digest / hmac / cipher / sign-verify / kdf / dh primitives in pure Rust slice-in / Vec-out form (no V8 dependency). Both surfaces call into the kernel; neither calls into the other; key material flows between them via a shared `Arc<KeyMaterial>` reference (D-N4).
3. **Streaming first.** Hash, Hmac, Cipher, Decipher, Sign, Verify all expose Node's incremental `update()` then `digest()` / `final()` / `sign()` / `verify()` shape. The kernel ships an incremental `Context` for each, plus a one-shot helper that the WebCrypto surface uses (e.g. `subtle.digest()` is one-shot, but it ultimately calls `kernel::digest_one_shot` which itself wraps `Context::new + update + finalise`).
4. **Native Buffer integration where Node's API specifies it.** Inputs accept `Buffer | Uint8Array | ArrayBuffer | TypedArray | DataView | string` (with optional encoding parameter for strings); outputs return whatever Node returns (Buffer for most binary outputs; string for `digest('hex')` etc.). The Buffer detection lives in a single helper `node_buffer::extract_input` and the emission lives in `node_buffer::emit_buffer`. Buffer itself stays unenv-backed (D-N7).
5. **Spec-correct Node error semantics.** Every error this surface throws is a real `Error` instance (or `TypeError` / `RangeError`) with the `code` property set to the matching `ERR_CRYPTO_*` / `ERR_OSSL_*` / `ERR_INVALID_ARG_TYPE` constant, exactly as Node sets it. Packages that branch on `e.code === "ERR_CRYPTO_HASH_FINALIZED"` to retry vs. give up work without modification.
6. **WebCrypto bridge with object identity.** `import("node:crypto").webcrypto === globalThis.crypto` (NOT a copy — same `Crypto` instance per realm). `import("node:crypto").subtle === globalThis.crypto.subtle`. `import("node:crypto").getRandomValues === globalThis.crypto.getRandomValues.bind(globalThis.crypto)` (or a re-exported function reference; both are spec-conformant — Node binds for safety, we follow). CryptoKey instances are mutually accepted by both surfaces via `KeyObject.from(cryptoKey)` / the shared `Arc<KeyMaterial>` (D-N4).
7. **No JS facade, no Promise-blocking-on-sync hack.** Every Node `*Sync` API runs synchronously on the V8 thread and returns the value. Every Node async API (Promise variant or callback variant) dispatches the work through the existing `state.spawned_ops` queue, completes off-thread, and resolves the Promise / fires the callback when done. The dispatch shape mirrors `fetch` / `kv` — the macro's `#[v8_async_method]` handles it (D-N5).

### Non-goals (explicit)

- **`node:tls` / `node:https` / `node:net` integration.** TLS is the gateway's job; the V8 isolate doesn't own a socket layer. `crypto.createSecureContext()` is in the **out-forever** list (Stage 2 may stub it as a no-op object so `https.createServer({...})` doesn't crash, but no actual TLS context).
- **Full X.509 verification chain.** Stage 1 ships `X509Certificate` parsing only (constructor accepts PEM/DER, exposes `.subject` / `.issuer` / `.publicKey` / `.fingerprint` / `.toLegacyObject` / `.raw` / `.serialNumber` / `.validFrom` / `.validTo` / `.checkIssued(other)`). The full verify-against-CA-chain (`crypto.X509Certificate.checkHost` / `.verify(publicKey)` / OCSP / CRL) defers to Stage 2 + an `@zeroship/x509-verify` npm wrapper around a Rust kernel. Document.
- **PKCS#11 / HSM / TPM integration.** Same reasoning as `crypto_native`'s D-27. Server-side embedded V8; HSM access goes through HTTP. `crypto.setEngine()` is a no-op (logs a warning).
- **`crypto.createDiffieHellman(prime, generator)` for arbitrary user-supplied primes >= 1024 bits.** Stage 1 ships only the named MODP groups (`modp1`/`modp2`/.../`modp18` per RFC 3526) and the named ffdhe groups (`ffdhe2048` per RFC 7919). Arbitrary user-supplied primes require BoringSSL's `DH_set0_pqg` low-level FFI; defer to Stage 2 with explicit DoS bounds (modulus ≤ 8192 bits). Stage 1 errors with `ERR_CRYPTO_UNSUPPORTED_OPERATION` on the unnamed-group constructor; document.
- **DES / 3DES / Blowfish / Cast5 / RC4 / IDEA legacy ciphers in Stage 1.** Document explicitly. Node still ships these; we ship them in Stage 2 with the `--legacy-crypto` opt-in flag (D-N22). Modern apps don't use these; the few that do (legacy POS systems, ancient SAML providers) get told to call out via `fetch` to a sidecar.
- **`crypto.generatePrime` / `crypto.checkPrime`.** Node added these in v15 for users who need DH-style primes. aws-lc-rs doesn't expose miller-rabin / random-prime as a high-level API; the BoringSSL FFI route is heavy. Defer to Stage 2 (D-N21).
- **`crypto.secureHeapUsed()`.** Node's secure-heap is an OpenSSL-specific allocator; aws-lc-rs / BoringSSL doesn't expose the equivalent telemetry as Rust bindings. Stub returning `{ total: 0, min: 0, used: 0, utilization: 0 }`. Document.
- **`crypto.fips` / `crypto.setFips()` / `crypto.getFips()`.** aws-lc-rs IS FIPS-validated upstream (relevant for some creator apps), but toggling FIPS mode at runtime is not supported by the high-level `aws-lc-rs` API. Stage 1: `getFips()` returns 0, `setFips(true)` throws `ERR_CRYPTO_OPERATION_FAILED` ("FIPS mode toggle not supported"). Document; revisit if a creator app surfaces FIPS-required workflows.
- **`crypto.subtle.encryptJWE` / JOSE-shaped extensions.** Same as WebCrypto's D-25 — npm package territory (`@zeroship/jose`).
- **Argon2.** Node never shipped `crypto.argon2` (the `argon2` npm package is third-party with a node-gyp binding). We don't add a native argon2 op; creator apps that need argon2 use the npm `argon2` package, which falls back to WASM in our runtime via unenv. (Document; revisit if measured perf is unacceptable.)

### Status

Draft v1 — **implementation pending.** The fork point is `main` at the worktree creation date (2026-05-02). All decisions D-N1 through D-N32 are settled in this doc; the implementation PR is the next step. The two-stage cutover (Stage 1: hash+hmac+kdf+random+webcrypto-bridge; Stage 2: cipher+sign+verify+keyobject+x509+dh+legacy) is independent — Stage 1 can ship and delete ~30 LOC of the JS shim while Stage 2 lands.

Post-completion: file as a date-prefixed ADR under `docs/decisions/` (mirroring webcrypto-native.md's planned ADR landing).

### Decisions (settled)

| # | Decision | Rationale | Section |
|---|----------|-----------|---------|
| **D-N1** | Three-module layout: `crypto_kernel/` (new shared backend, slice-in/Vec-out, no V8), `crypto_native/` (existing WebCrypto, refactored to call the kernel), `crypto_node/` (new node:crypto surface). The kernel is the single source of truth for digest / hmac / cipher / sign-verify / kdf / dh primitives. Both surfaces are thin V8 adapters. No surface calls the other surface; both call only the kernel. | Eliminates duplication (today's `crypto_native/aes.rs` and the JS shim's `__cryptoHashSync` reach into the same aws-lc-rs API by parallel paths). One bug fix in the kernel fixes both surfaces. workerd uses the same shape (`api/crypto/impl.h` + per-algorithm `aes.c++` shared between WebCrypto and `node/crypto*`). | §I.1 |
| **D-N2** | The kernel exposes streaming `Context` types (`DigestContext`, `HmacContext`, `CipherContext`, `SignContext`, `VerifyContext`) plus one-shot helpers (`digest_one_shot`, `hmac_one_shot`, etc.). The WebCrypto one-shot ops call the one-shot helpers; node:crypto's streaming Hash / Hmac / Cipher classes hold a `Context` in their boxed state, calling `update` per JS-side `update()` and `finalize` on `digest()` / `final()`. Both paths share the same underlying state machine. | Streaming is THE shape difference between the two surfaces. Modeling it once in the kernel makes the surface adapters trivial. workerd does this; Bun does too (its `Hash.zig` is just a Zig wrapper around BoringSSL's `EVP_MD_CTX`). | §V.1 |
| **D-N3** | Class layout for node:crypto: 17 v8 classes — `Hash`, `Hmac`, `Cipher`, `Decipher`, `Sign`, `Verify`, `Hkdf` (Stage 2 internal), `Pbkdf2` (internal helper), `Scrypt` (internal helper), `KeyObject` (the parent), `PublicKeyObject`, `PrivateKeyObject`, `SecretKeyObject` (the three subclasses), `DiffieHellman` (Stage 2), `DiffieHellmanGroup` (Stage 2), `ECDH` (Stage 2), `X509Certificate` (Stage 2). Each is a `#[v8_class]` with a `Box<{Class}State>` in internal field 0. The `*KeyObject` subclasses use `#[v8_inherit(KeyObject)]`. | Node's own type model is `KeyObject` parent + three subclasses; the subclasses are used by typeof-checks in npm packages (e.g. `jose` checks `key instanceof PrivateKeyObject`). Mirroring exactly avoids surprise breakage. | §IV |
| **D-N4** | `KeyObject` ↔ `CryptoKey` bridge via shared `Arc<KeyMaterial>`. Both `CryptoKey` (the existing WebCrypto wrapper) and `KeyObject` (the new node:crypto wrapper) hold an `Arc<KeyMaterial>` in their boxed state. `KeyObject.from(cryptoKey)` clones the Arc into a new `KeyObjectState`; `crypto.subtle.importKey('jwk', keyObject.export({format:'jwk'}))` round-trips through JWK (slow but spec-correct). The existing `crypto_native/key_material.rs::KeyMaterial` enum is wrapped in `Arc` (a one-line refactor at the storage site). | Node's spec mandates `KeyObject.from(CryptoKey)` works (https://nodejs.org/api/crypto.html#static-method-keyobjectfromkey). The Arc share avoids re-encoding key bytes; `keyObject.export(...)` still re-walks PKCS#8/SPKI from the Arc material on demand. workerd uses the same bridge (shared `KeyContext` in `api/crypto/keys.h`). | §IV.5 |
| **D-N5** | Sync-on-V8-thread for all `*Sync` Node APIs and all "small" data-path streaming APIs (Hash/Hmac under any size; Cipher/Decipher under a 64 KB threshold; Sign/Verify under any size — RSA verify of a 100-byte sig is microseconds). Async (Promise or callback) for all `pbkdf2`, `scrypt`, `generateKeyPair`, `generatePrime`, and the `hkdf` async variant. Async dispatch goes through `state.spawned_ops` (the existing fetch / kv compio-blocking-pool pattern). | Mirrors WebCrypto's D-29 deferral with the difference that node:crypto explicitly distinguishes sync from async: the user picked the API, so we honour the contract. KDFs with adversary-tunable iteration count (PBKDF2's `iterations: 1_000_000` for password hashing) MUST go to a thread pool, otherwise a single password check pins the V8 thread for ~500 ms. | §VI |
| **D-N6** | Cipher/Decipher `update()` and `final()` both run sync on the V8 thread, always. Node's `cipher.update()` is documented sync-returning (https://nodejs.org/api/crypto.html#cipherupdatedata-inputencoding-outputencoding) — making it sometimes-async would change the API shape. We document the "chunk via TransformStream + per-chunk update" pattern for bulk-encryption workloads and rely on the `Cipher`/`Decipher` classes implementing `stream.Transform` (D-N35) for the natural pipe-based path. (addresses critic CRITICAL #1 — v1's "sync below 64 KB / async above" was internally contradicted by §VI.3 and contradicted Node's documented sync contract.) | Same reasoning as WebCrypto's D-29. Spec parity with Node. | §VI.3, §VI.4 |
| **D-N7** | Buffer integration: accept inputs as a union of `Buffer | Uint8Array | DataView | ArrayBuffer | TypedArray | string`, materialise to `Vec<u8>` (or `&[u8]` when the lifetime works) at the entry point. Return `Buffer` from APIs Node specifies as returning Buffer (almost everything binary), `string` from APIs Node specifies as returning string (`digest('hex')`, `Sign.sign(privateKey, 'base64')`). The Buffer materialisation calls into unenv's Buffer (`new Buffer(arrayBuffer, byteOffset, byteLength)` via the `Buffer.from` static — same as the JS shim does today, but lifted into a single Rust helper). Buffer detection is loose: any Uint8Array works as a Buffer for input purposes (matches Node's behaviour — Node never type-checks input shapes; any TypedArray with the right bytes is fine). For OUTPUT, we mint Buffer instances by calling `Buffer.from(uint8Array)` via the V8 boundary. | Native-implementing Buffer ourselves would be a 600 LOC project (Buffer is a bigger surface than CryptoKey: alloc/allocUnsafe, write/writeBigInt64BE/writeUInt8/.., readDoubleBE/.., toString with 7 encodings, equals/compare/indexOf, subarray, slice, swap16/32/64, the encoding registry...). unenv's Buffer is correct enough that npm packages don't crash on it. The cost of a `Buffer.from(...)` round-trip per crypto call is ~100 ns — invisible next to a 5 µs hash. We document this as the v1 trade; a future Buffer-native ADR may revisit. | §III |
| **D-N8** | Error mapping: a single `OpError` enum (the existing one, extended with a `NodeError(code: &'static str)` variant) routes to the right surface at throw time. The macro's `gen_throw_error` arm checks the variant: `NodeError(code)` constructs a JS Error / TypeError / RangeError (per a small table) and sets `error.code = code`; the existing `DomException(name)` arm stays for the WebCrypto surface. The kernel returns `KernelError`, which is mapped to either `OpError::DomException` (when called from `crypto_native/`) or `OpError::NodeError` (when called from `crypto_node/`) at the surface boundary. | Node's `e.code` is the contract npm packages check (`if (e.code === "ERR_CRYPTO_OPERATION_FAILED") retry()`). Throwing a generic Error breaks them. The kernel can't decide which surface to throw for — the surface knows. So map at the boundary. workerd does the same shape (`KJ_REQUIRE(...)` + per-surface adapter). | §VII |
| **D-N9** | `Hash` class: `update(data, inputEncoding?)` returns `this` for chaining; `digest(outputEncoding?)` returns `Buffer` if no encoding else string in the requested encoding (`hex` / `base64` / `base64url` / `latin1` / `binary`). `copy(options?)` returns a fresh Hash with the same in-progress state. Throws `ERR_CRYPTO_HASH_FINALIZED` on any post-`digest()` update. Backed by `kernel::DigestContext`. | Direct Node parity. The encoding registry is small (5 named output encodings + 6 named input encodings = 11 strings); a phf::Map keyed on encoding string drives the conversion. | §V.2 |
| **D-N10** | `Hmac` class: `update(data, inputEncoding?)` and `digest(outputEncoding?)` mirror Hash; `copy(options?)` is intentionally absent on Hmac in Node (hmac.copy doesn't exist) — we match. Backed by `kernel::HmacContext`. | Node has Hash.copy but not Hmac.copy (a quirk of OpenSSL EVP_MD_CTX vs HMAC_CTX). Some npm packages (older `passport-jwt` versions) crash if Hmac has a `.copy` method that throws when called the way Hash.copy works — they assume same shape. We match Node's omission exactly. | §V.3 |
| **D-N11** | `Cipher` / `Decipher` classes: `update(data, inputEncoding?, outputEncoding?)` returns Buffer (or string if outputEncoding); `final(outputEncoding?)` flushes the last block + tag; `setAAD(buffer, options?)` for GCM/CCM AAD; `setAuthTag(buffer)` for Decipher post-data tag inject; `getAuthTag()` for Cipher post-final tag emit; `setAutoPadding(boolean)` for CBC PKCS#7 control. Backed by `kernel::CipherContext`. The class is created via `crypto.createCipheriv(algorithm, key, iv, options?)` factories — `createCipher` (deprecated, derives key from password) is intentionally NOT shipped (Node deprecated it because the KDF is broken; a creator app calling `createCipher` deserves the failure). | Direct Node parity for `createCipheriv`. Skipping `createCipher` is the workerd / Deno consensus — the deprecated API has weak KDF properties (EVP_BytesToKey single-iteration MD5). Throwing `ERR_CRYPTO_DEPRECATED_API` with a doc URL to switch to `createCipheriv` is the right move. | §V.4 |
| **D-N12** | `Sign` / `Verify` classes: `update(data, inputEncoding?)` and `sign(privateKey, outputEncoding?)` / `verify(publicKey, signature, signatureEncoding?)`. Internally compute the digest streaming-style, then run the asymmetric op once at finalisation. Accept `privateKey` / `publicKey` as `KeyObject`, `CryptoKey`, PEM string, DER Buffer, or `{ key, format, type, passphrase }` options object — Node's union type. The encoding helper at the boundary materialises any of these to a kernel-friendly key handle. | Sign / Verify are the "DigestSign" pattern in OpenSSL (EVP_DigestSignInit + Update + Final). The streaming API saves the user from buffering the message; the kernel's `SignContext` mirrors EVP_DigestSignContext. | §V.5 |
| **D-N13** | `KeyObject` / `PublicKeyObject` / `PrivateKeyObject` / `SecretKeyObject`: parent + three subclasses (`#[v8_inherit]`). Parent has `.type` (returns "secret" / "public" / "private"), `.asymmetricKeyType` (returns null for secret), `.asymmetricKeyDetails` (algorithm-specific dict), `.symmetricKeySize` (bytes for secret; null for asymmetric), `.export(options) -> Buffer | string | object`, `.equals(other)`. Subclasses add nothing functional — they exist for `instanceof` discrimination. Internal-field 0 holds `Box<KeyObjectState>` carrying an `Arc<KeyMaterial>`. The static `KeyObject.from(cryptoKey)` constructor accepts a `CryptoKey` and clones the Arc. | Node's type model verbatim. Some npm packages (older `jose`, `node-forge`) check `instanceof PrivateKeyObject` to distinguish privates; missing the subclass means those checks fail. | §IV |
| **D-N14** | `crypto.createSecretKey(buffer | string, encoding?)` / `crypto.createPublicKey(input)` / `crypto.createPrivateKey(input)` factories: parse the input (PEM / DER / JWK / KeyObject / `{ key, format: 'pem' | 'der' | 'jwk', type: 'pkcs1' | 'pkcs8' | 'spki' | 'sec1', passphrase: Buffer? }`) into a fresh `KeyObject` instance. The PEM parser lives in `kernel::pem` (RFC 7468 framing — a single function: `decode_pem(text) -> Vec<(label, der_bytes)>`); the DER walker is the existing `crypto_native/der.rs`. `passphrase` for encrypted PKCS#8 dispatches to aws-lc-rs's `EncryptedPrivateKeyInfo::from_bytes(der).decrypt(passphrase)`. | The createX factories are the entry point npm packages use. Without them, a creator app cannot import a key — there's no other path. JWK input takes the JWK as a JS object (passed into the kernel JWK parser shared with WebCrypto). | §IV.4 |
| **D-N15** | KDF dispatch: `pbkdf2` / `pbkdf2Sync` / `scrypt` / `scryptSync` / `hkdf` / `hkdfSync` — sync variants run on V8 thread (user opted into blocking by picking the Sync API); async variants dispatch to `state.spawned_ops`. Both call into `kernel::pbkdf2` / `kernel::scrypt` / `kernel::hkdf` (slice-in / Vec-out). PBKDF2 + HKDF are already in `crypto_native/derive.rs`; the kernel extraction is mechanical (move + add a `_sync` and `_async` adapter). scrypt is NEW — aws-lc-rs has `pbkdf2` but no scrypt; we use `aws_lc_sys::EVP_PBE_scrypt` (BoringSSL's scrypt is a ~200 LOC FFI binding). | RFC 7914 scrypt is the password-hashing standard most modern apps use (vs. PBKDF2 which is recommended only for legacy interop). bcrypt is similar but not in node:crypto; the `bcrypt` npm package wraps OpenSSL's `BF_set_key` directly. We don't ship bcrypt; the npm package's WASM fallback (via unenv) is acceptable. | §VI.2 |
| **D-N16** | webcrypto bridge — object identity. `import("node:crypto")` yields an exports object whose `.webcrypto` property IS the same `Crypto` instance that's installed at `globalThis.crypto`. The synthetic module's installer code reads `globalThis.crypto` once at module-evaluate time and assigns the reference directly; subsequent reads return the same `Crypto` instance. `subtle` is `globalThis.crypto.subtle`. `getRandomValues` is `globalThis.crypto.getRandomValues.bind(globalThis.crypto)` (Node binds; we follow). | Node's `crypto.webcrypto === globalThis.crypto` is a cross-codebase invariant — JOSE libraries assume it. Returning a copy would silently break `WeakMap`-based key tracking (libraries that keep a `WeakMap<CryptoKey, ...>` would lose entries on the boundary). | §VIII |
| **D-N17** | Random: `randomBytes(size, callback?) -> Buffer | void` (callback variant returns Buffer to callback async; sync variant returns Buffer). `randomFillSync(buffer, offset?, size?) -> Buffer`. `randomFill(buffer, offset?, size?, callback) -> void` (always callback). `randomInt(min, max, callback?) -> number` (uniform distribution via rejection sampling, not the JS-shim's modulo bias). `randomUUID(options?)` — same as `globalThis.crypto.randomUUID`. `getRandomValues` re-export. All sync; `randomBytes(N)` for very large N (e.g. > 1 MB) goes async via callback if present, sync otherwise — matches Node. | The randomInt rejection-sampling fix corrects a subtle bias in the JS shim (line 109-117 of node-compat.ts: `range > 2^32` causes silent bias). Node uses the same rejection-sampling technique we will. | §VI.5 |
| **D-N18** | Algorithm name canonicalisation: node:crypto names are case-insensitive but inconsistent ("sha256" vs "SHA-256" vs "RSA-SHA256"). The kernel uses spec-canonical names ("SHA-256", "RSA-PSS"); the surface adapter maps node:crypto inputs via a phf::Map: `"sha256" -> SHA-256`, `"sha-256" -> SHA-256`, `"sha384" -> SHA-384`, ..., `"rsa-sha256" -> SignAlg::RsaPkcs1Sha256`, etc. Names not in the table → `ERR_OSSL_EVP_UNSUPPORTED_ALGORITHM`. | Node's getHashes() returns ~50 names (because OpenSSL aliases everything). We support the 4 SHA digests + their aliases + ChaCha20-Poly1305 + the 11 cipher modes + 6 sign algorithms — total ~25 algorithm names. The map is small. | §IX |
| **D-N19** | `KeyObject.export(options)` accepts `{ format: 'pem' | 'der' | 'jwk', type: 'pkcs1' | 'pkcs8' | 'spki' | 'sec1', cipher?: string, passphrase?: Buffer }`. PEM emission uses the kernel's PEM emitter (the inverse of D-N14's parser). `cipher` + `passphrase` for encrypted PKCS#8 export uses aws-lc-rs's `EncryptedPrivateKeyInfo::serialize_with_password`. JWK export reuses `crypto_native/jwk.rs::export_*`. | Direct Node parity. The cipher options matrix (`{ cipher: 'aes-256-cbc', passphrase: Buffer.from('...') }`) is what passport / saml / openid-client libraries use to round-trip encrypted private keys. | §IV.6 |
| **D-N20** | X.509: Stage 1 ships a stub class that throws `ERR_CRYPTO_UNSUPPORTED_OPERATION` on construction, with a clear message pointing at the Stage 2 ADR. Stage 2 ships parsing-only (constructor + readonly properties). Full chain verification defers to a future `@zeroship/x509-verify` npm package wrapping BoringSSL's `X509_verify_cert`. | Most npm packages that touch X509 (jsonwebtoken's JWKS endpoints, Apple Sign-In, Google's JWT checking) do their own verify on top of `X509Certificate.publicKey` — they don't call `.verify()` directly. Stage 2 parsing-only covers ~80% of usage. | §X |
| **D-N21** | DH: Stage 1 ships only the named groups (`crypto.getDiffieHellman('modp14')` etc.). Stage 2 adds `crypto.createDiffieHellman(prime, generator)` via aws-lc-sys's lower FFI (`DH_set0_pqg`). Stage 1 errors on the unnamed-group factory with `ERR_CRYPTO_UNSUPPORTED_OPERATION`. `crypto.createECDH` ships in Stage 1 (aws-lc-rs's `agreement::*` has the curves). | DH (vs ECDH) is rare in modern apps — TLS 1.3 deprecated DHE in favour of ECDHE. Most uses we'll see in npm are SCRAM / SSH-key-exchange, both of which use named groups. Generic DH is the long tail. | §X |
| **D-N22** | Legacy ciphers (DES, 3DES, Blowfish, Cast5, RC4, IDEA): NOT in Stage 1. Stage 2 ships them under the `--legacy-crypto` runtime flag (off by default). Without the flag, `createCipheriv('des-cbc', ...)` errors with `ERR_OSSL_EVP_UNSUPPORTED_ALGORITHM` and a message pointing at the flag. Node ships these unconditionally (they're behind OpenSSL's `OPENSSL_NO_LEGACY` macro, which Node defines off). aws-lc-rs has DES via `cipher::TDES_*` but not Blowfish / Cast5 / RC4 / IDEA. We'd need aws-lc-sys raw for those. | Most modern apps don't touch these. The few that do are interfacing with truly legacy systems (POS terminals, ancient SAML providers); a runtime flag rather than blanket support reduces our attack surface. | §X |
| **D-N23** | ChaCha20-Poly1305: ships in Stage 1. aws-lc-rs has it as `aead::CHACHA20_POLY1305`. Node added it in v17 (June 2021). Used by modern TLS implementations and Signal-protocol-style apps. Cheap to add; no reason to defer. | Spec parity with Node v17+. Aligns with WebCrypto's "out of v1; trivially added" comment — we ship it for node:crypto immediately because Node already does. | §V.4 |
| **D-N24** | scrypt parameters + memory cap: accept Node's `{ N: 16384, r: 8, p: 1, maxmem: 32 * 1024 * 1024 }` options object. Default: `N=16384, r=8, p=1` per Node. The maxmem cap is enforced (default 32 MB; user can override). Implementations that set `N=2^20` (default for `bcrypt-alternative-2025` style libs) without `maxmem` get an error — Node throws same way. The async variant always offloads to a thread pool (D-N5). | RFC 7914 + Node parity. The maxmem check prevents a single password verify from OOMing the worker. | §VI.2 |
| **D-N25** | FIPS controls: `getFips() -> 0`, `setFips(true) -> throw "FIPS mode toggle not supported"` (`ERR_CRYPTO_OPERATION_FAILED`), `crypto.fips` getter returns 0. Document. The aws-lc-rs build IS FIPS-validated when the workspace is built with `aws-lc-fips-sys` (a sibling crate); we don't currently enable that, but if a creator app surfaces a FIPS workflow we flip the dep. | aws-lc-rs's high-level API doesn't expose FIPS mode toggle; toggling is build-time, not runtime. Stub returning 0 prevents `if (crypto.fips) ...` branches from crashing. | §X.3 |
| **D-N26** | Synthetic module install path: register `"node:crypto"` as a synthetic ESM module on the Vite-side `fetchModule` interceptor (already exists at `sdks/vite-plugin/src/environment.ts:191`). The synthetic module's source code is auto-generated at build time from a TypeScript stub that lists every export and declares each one's binding via a `__zeroship_node_crypto.{name}` global lookup. The Rust side installs `globalThis.__zeroship_node_crypto = { Hash, Hmac, ... }` during `setup_globals`; the synthetic module re-exports the named slots. | This is the same shape as fetch-native's bridge today. The Rust-Js boundary is one global object (`__zeroship_node_crypto`); the JS-side synthetic module is a thin re-export shell. Replaces the customPolyfills entry verbatim. | §XI |
| **D-N27** | Cutover cadence: 5 stages, each its own PR. Stage A: kernel extraction (no behaviour change; refactor `crypto_native/aes.rs` etc. to call `crypto_kernel/cipher.rs`). Stage B: hash + hmac + random + KDFs native, replacing the JS shim's `__cryptoHashSync` / `__cryptoHmacSync` calls. Stage C: KeyObject + sign + verify + cipher + decipher native. Stage D: webcrypto bridge wired (subtle / getRandomValues identity). Stage E: X.509 + DH + ECDH + legacy ciphers (Stage 2). Each stage is independent; A is the prerequisite for B-E. The shim-deletion happens incrementally per stage. | Same cadence rationale as WebCrypto's D-23 (3 landings) but extended to 5 because node:crypto is a much larger surface. Risk control. | §XII |
| **D-N28** | Algorithm registry: a single phf::Map keyed on (Operation, lowercased name) -> AlgorithmEntry, mirroring webcrypto-native's D-8 registry. The map is shared between kernel and surfaces (kernel functions take an `AlgorithmEntry`-like enum rather than a string). Node's algorithm names map through this registry by canonicalising on input. The registry doubles as the source for `getCiphers()` / `getHashes()` / `getCurves()` (just iterate the map). | One source of truth. WebCrypto already pays for the registry (D-8); this design extends it to cover Node-specific names (`aes-256-gcm` etc.) without adding a second registry. | §IX |
| **D-N29** | `getCipherInfo(nameOrNid, options?) -> { name, blockSize, ivLength, keyLength, mode } | undefined` — the dictionary lookup that returns Cipher metadata. Backed by the algorithm registry's per-cipher metadata fields. `getCiphers()` / `getHashes()` / `getCurves()` similarly iterate the registry. `getCipherInfo` and `getHashes` are heavily used by feature-detection code in npm packages. | These are read-only metadata APIs. Cheap to ship; commonly used. | §X.1 |
| **D-N30** | Zeroize on Drop for KeyObjectState's secret material. Wrap the `KeyMaterial::Symmetric(Vec<u8>)` and `*Private` PKCS#8 / raw-d / private-component vectors in `zeroize::Zeroizing<Vec<u8>>` — same as WebCrypto's open-question XIV.8 working answer. The Arc share means zeroization happens when the LAST KeyObject + CryptoKey wrapper drops. | Defense-in-depth. zeroize is already a transitive dep via aws-lc-rs. ~5 LOC of wrapper. Aligns the two surfaces. | §IV.7 |
| **D-N31** | `timingSafeEqual(a, b)` ships as a native Rust call — `aws_lc_rs::constant_time::verify_slices_are_equal`. Throws `ERR_INVALID_ARG_TYPE` if the inputs are different lengths or non-buffer-shaped. Returns boolean. | Node spec parity. The constant-time guarantee comes from aws-lc-rs (compiler-fence + byte-by-byte XOR + accumulator). Critical for HMAC-tag verify in jwt libraries. | §X.2 |
| **D-N32** | Macro extensions needed: ONE — a `NodeError(code: &'static str)` variant on `OpErrorKind`, with a corresponding `gen_throw_error` arm that constructs Error / TypeError / RangeError per a per-code table and assigns the `code` property. The `Buffer` extraction is NOT a new macro feature — it's a library helper at `crypto_node/buffer.rs::extract_input` called from each method. Streaming `Context` types are NOT a macro feature — they're plain Rust state in the boxed instance. `KeyObject` is a regular `#[v8_class]` with a regular Box; no macro work. | Mirrors webcrypto-native's D-30 in spirit. The macro change is a one-variant addition + a ~20 LOC arm. The rest is library code. | §XIII |

## I. Architecture overview

### I.1. Three-module layout (D-N1)

The new layout splits crypto into three modules:

```
crates/runtime/src/
├── crypto_kernel/      (NEW) shared backend. Slice-in / Vec-out. No V8.
│   ├── mod.rs
│   ├── digest.rs       Streaming + one-shot SHA-1/256/384/512 + BLAKE2 (Stage 2)
│   ├── hmac.rs         Streaming + one-shot HMAC over above hashes
│   ├── cipher.rs       Streaming + one-shot AES-{CBC,CTR,GCM,KW} + ChaCha20-Poly1305
│   ├── sign_verify.rs  Streaming-digest + one-shot RSA / ECDSA / Ed25519 + RSA-PSS
│   ├── kdf.rs          PBKDF2 / HKDF / scrypt
│   ├── dh.rs           ECDH + DH (Stage 2 for raw DH, Stage 1 for ECDH)
│   ├── pem.rs          RFC 7468 PEM framing decode + emit
│   ├── der.rs          (existing, moved from crypto_native/) ASN.1 DER walker
│   ├── key_material.rs (existing, moved from crypto_native/) the KeyMaterial enum,
│   │                   now wrapped in Arc by both surfaces (D-N4)
│   ├── jwk.rs          (existing, moved from crypto_native/) JWK parse + emit
│   ├── algorithms.rs   The phf::Map registry (D-N28)
│   └── error.rs        KernelError enum + Display impl + per-code mapping
│
├── crypto_native/      (EXISTING) WebCrypto surface. Refactored to call kernel.
│   ├── mod.rs          (unchanged install_globals)
│   ├── crypto_class.rs (unchanged Crypto class)
│   ├── crypto_key.rs   (refactored to hold Arc<KeyMaterial>)
│   ├── subtle.rs       (refactored: every method body becomes a call to kernel)
│   ├── aes.rs          (refactored: 90% deleted, now thin wrapper around kernel::cipher)
│   ├── rsa.rs          (refactored: 90% deleted, now thin wrapper around kernel::sign_verify)
│   ├── ec.rs           (refactored)
│   ├── okp.rs          (refactored)
│   ├── hmac.rs         (refactored: ~30 LOC remaining)
│   ├── derive.rs       (refactored)
│   ├── digest.rs       (refactored: ~10 LOC, just the WebCrypto algorithm-resolver)
│   ├── wrap.rs         (refactored)
│   ├── ops.rs          (refactored)
│   ├── registry.rs     (DELETED — superseded by crypto_kernel/algorithms.rs)
│   ├── helpers.rs      (kept — V8-side helpers like vec_to_arraybuffer)
│   └── evp_ffi.rs      (DELETED if all variable-IV / variable-tag GCM moves to kernel)
│
└── crypto_node/        (NEW) node:crypto surface. V8 adapters over the kernel.
    ├── mod.rs
    ├── module.rs       Synthetic-module installer + __zeroship_node_crypto global
    ├── hash.rs         Hash class
    ├── hmac.rs         Hmac class
    ├── cipher.rs       Cipher / Decipher classes
    ├── sign.rs         Sign / Verify classes
    ├── key_object.rs   KeyObject + 3 subclasses + create* factories
    ├── kdf.rs          pbkdf2 / pbkdf2Sync / scrypt / scryptSync / hkdf / hkdfSync
    ├── random.rs       randomBytes / randomFill / randomInt / randomUUID
    ├── dh.rs           DiffieHellman / DiffieHellmanGroup / ECDH (Stage 2 + Stage 1 ECDH)
    ├── x509.rs         X509Certificate (Stage 2)
    ├── webcrypto.rs    The bridge re-exports (subtle / getRandomValues / verify / sign etc.)
    ├── buffer.rs       Buffer extract / emit helpers (the input/output coercion)
    ├── encoding.rs     The 6+5 named-encoding registry (utf8/hex/base64/...)
    ├── error.rs        Node-error code constants + per-code Error/TypeError/RangeError table
    └── misc.rs         timingSafeEqual / getCiphers / getHashes / getCurves /
                        getCipherInfo / getFips / setFips / setEngine / secureHeapUsed
```

The boundary: kernel functions take Rust-native input/output (slices, Vecs, the `KeyMaterial` enum, the `AlgorithmEntry` enum). Surfaces hold V8 callbacks; they call kernel functions and translate the result back to V8 (Buffer / ArrayBuffer / DOMException / Error). No surface calls another surface — they meet at the kernel.

### I.2. Class structure (D-N3)

WebCrypto has 3 classes (`Crypto`, `SubtleCrypto`, `CryptoKey`). Node:crypto has 17 classes:

**Streaming primitives (6):**
- `Hash` — incremental digest. `kernel::DigestContext`.
- `Hmac` — incremental HMAC. `kernel::HmacContext`.
- `Cipher` — incremental encrypt. `kernel::CipherContext`.
- `Decipher` — incremental decrypt. `kernel::CipherContext`.
- `Sign` — incremental digest + asymmetric finalise. `kernel::SignContext`.
- `Verify` — incremental digest + asymmetric verify. `kernel::VerifyContext`.

**Key handles (4):**
- `KeyObject` — parent. `kernel::KeyMaterial` (via Arc).
- `PublicKeyObject` — `#[v8_inherit(KeyObject)]`. Empty subclass for instanceof.
- `PrivateKeyObject` — `#[v8_inherit(KeyObject)]`. Empty subclass for instanceof.
- `SecretKeyObject` — `#[v8_inherit(KeyObject)]`. Empty subclass for instanceof.

**Key-exchange (3):**
- `DiffieHellman` — Stage 2 only.
- `DiffieHellmanGroup` — Stage 2; thin subclass of DiffieHellman with read-only prime/generator.
- `ECDH` — Stage 1. `kernel::dh::ECDHContext`.

**X.509 (1):**
- `X509Certificate` — Stage 2.

**Internal helpers exposed for symmetry with Node (3):**
- `Hkdf` — Node doesn't expose this as a class; it's the internal context behind `crypto.hkdf` / `crypto.hkdfSync`. We keep it private to the module (no JS surface).
- `Pbkdf2` / `Scrypt` — same; private.

The `#[v8_inherit]` mechanism is the existing one used by `AbortSignal extends EventTarget` (already shipped in `crypto_native/`'s sibling work — see `crates/runtime/src/dom/abort_signal.rs`). The three KeyObject subclasses use it without modification.

### I.3. Sync vs async coexistence (D-N5)

| API category | Default (no callback) | Callback variant | Promise variant | Sync variant |
|---|---|---|---|---|
| `createHash()` `.update()` `.digest()` | sync | n/a | n/a | always sync |
| `createHmac()` `.update()` `.digest()` | sync | n/a | n/a | always sync |
| `createCipheriv()` `.update()` `.final()` | always sync — see §VI.3 (addresses critic CRITICAL #1) | n/a | n/a | always sync (Node's `cipher.update()` is documented sync per https://nodejs.org/api/crypto.html#cipherupdatedata-inputencoding-outputencoding) |
| `createSign()` `.sign()` | sync | n/a | n/a | always sync |
| `createVerify()` `.verify()` | sync | n/a | n/a | always sync |
| `randomBytes(n)` | sync | async | n/a | sync (no Sync suffix needed; the no-callback form IS sync) |
| `randomFill(buf, ...)` | n/a | async (always callback) | n/a | `randomFillSync(buf)` |
| `pbkdf2(...)` | n/a | async | promise | `pbkdf2Sync(...)` |
| `scrypt(...)` | n/a | async | promise | `scryptSync(...)` |
| `hkdf(...)` | n/a | async | promise | `hkdfSync(...)` |
| `generateKeyPair(type, opts)` | n/a | async | promise | `generateKeyPairSync(type, opts)` |
| `generateKey(type, opts)` | n/a | async | promise | `generateKeySync(type, opts)` |
| `generatePrime(size)` | n/a | async | promise | `generatePrimeSync(size)` |

**Sync = run on V8 thread, return immediately, no Promise.** The existing fast-path in our runtime works fine — the V8 callback executes the work and returns. CPU-bound for the duration; user explicitly opted in.

**Async with callback = dispatch through `state.spawned_ops`.** The macro's `#[v8_async_method]` (already shipped per `crates/runtime-macros/TODO.md` "Done" section) emits a callback-shape that allocates a PromiseResolver, spawns the future, returns immediately, and resolves on completion. For node:crypto's callback style (the user's callback is the LAST argument), we need a small adapter — either:
- Rewrite the JS-side ergonomic so callback variants present as Promise-returning to the macro and we wrap with a `.then(callback)` in the synthetic module's TS definition (preferred — keeps one shape in Rust).
- OR add a new `#[v8_callback_method]` macro variant. Heavier; deferred unless approach 1 fails.

**Approach 1 (chosen):** the synthetic module's TS shim looks like:

```ts
// Generated TS for node:crypto's callback-flavoured APIs.
function pbkdf2(password, salt, iters, keylen, digest, callback) {
  __zeroship_node_crypto.pbkdf2Async(password, salt, iters, keylen, digest)
    .then(buf => callback(null, buf), err => callback(err));
}
function pbkdf2Sync(password, salt, iters, keylen, digest) {
  return __zeroship_node_crypto.pbkdf2Sync(password, salt, iters, keylen, digest);
}
```

Two Rust ops per async API: `*Async` (Promise-returning, async-method shape) and `*Sync` (sync, blocks V8 thread). The TS shim chooses based on the arity / presence of callback. ~10 LOC of TS shim per async-callback API; ~6 such APIs × 10 = 60 LOC of TS shim total — acceptable.

### I.4. Key interop (D-N4)

The two key abstractions overlap:

**WebCrypto `CryptoKey` (existing):**
- `.type: "secret" | "public" | "private"`
- `.algorithm: KeyAlgorithm` (frozen JS object, [SameObject])
- `.extractable: boolean`
- `.usages: KeyUsage[]` (frozen JS array, [SameObject])
- `[[handle]]`: internal, holds `KeyMaterial`

**node:crypto `KeyObject` (new):**
- `.type: "secret" | "public" | "private"`
- `.asymmetricKeyType: "rsa" | "ec" | "ed25519" | "x25519" | "dh" | null`
- `.asymmetricKeyDetails: { modulusLength, publicExponent, namedCurve, hash, ... } | null`
- `.symmetricKeySize: number | null`
- `.export(options)`
- `.equals(other)`
- `[[handle]]`: internal, holds `KeyMaterial`

The shared abstraction: `Arc<KeyMaterial>`. Move `key_material::KeyMaterial` from `crypto_native/` to `crypto_kernel/key_material.rs` and wrap every storage site in `Arc`. The `CryptoKeyState` and `KeyObjectState` both look like:

```rust
// crypto_kernel/key_material.rs (existing enum, moved from crypto_native/)
pub enum KeyMaterial { /* unchanged */ }

// crypto_native/crypto_key.rs (refactored)
pub struct CryptoKeyState {
    pub key_type: KeyType,
    pub extractable: bool,
    pub algorithm: KeyAlgorithm,
    pub usages: Vec<KeyUsage>,
    pub material: Arc<KeyMaterial>,    // was Box<KeyMaterial>
}

// crypto_node/key_object.rs (new)
pub struct KeyObjectState {
    pub key_type: KeyType,             // shares enum with CryptoKey
    pub material: Arc<KeyMaterial>,    // shares Arc with CryptoKey
    // The asymmetricKeyType / asymmetricKeyDetails / symmetricKeySize are
    // computed lazily from `material` — no extra storage.
}
```

The bridge ops:

- `KeyObject.from(cryptoKey)` (static) → reads cryptoKey.[[handle]].material (the Arc), clones it into a new KeyObjectState, returns a fresh KeyObject wrapper.
- `crypto.subtle.importKey('jwk', keyObject.export({format:'jwk'}))` → takes the KeyObject's exported JWK, runs the existing WebCrypto JWK importer; the result is a fresh CryptoKey with its OWN Arc<KeyMaterial> (the JWK round-trip materialises a new Arc — slower but spec-correct).

The Arc share avoids re-encoding on the common `KeyObject.from(...)` path. The JWK round-trip path is unavoidable when the user wants a CryptoKey from a KeyObject with WebCrypto-specific algorithm settings (the algorithm + extractable + usages don't have a node:crypto equivalent and must come from the JWK importKey call).

### I.5. Streaming via incremental contexts (D-N2)

Hash, Hmac, Cipher, Decipher, Sign, Verify all maintain in-progress state across multiple `update()` calls. The kernel models this as `Context` types:

```rust
// crypto_kernel/digest.rs
pub struct DigestContext {
    inner: aws_lc_rs::digest::Context,
    finalised: bool,    // tracks ERR_CRYPTO_HASH_FINALIZED
}

impl DigestContext {
    pub fn new(hash: HashAlgo) -> Self { /* ... */ }
    pub fn update(&mut self, data: &[u8]) -> Result<(), KernelError> {
        if self.finalised { return Err(KernelError::HashFinalised); }
        self.inner.update(data); Ok(())
    }
    pub fn finalize(&mut self) -> Result<Vec<u8>, KernelError> {
        if self.finalised { return Err(KernelError::HashFinalised); }
        self.finalised = true;
        Ok(self.inner.clone().finish().as_ref().to_vec())
    }
    pub fn clone_state(&self) -> Self {
        Self { inner: self.inner.clone(), finalised: self.finalised }
    }
}
```

The Hash class wraps it:

```rust
// crypto_node/hash.rs (sketch)
pub struct HashState {
    ctx: DigestContext,
}

#[v8_class]
impl Hash {
    #[v8_method]
    fn update<'s>(&mut self,
        scope: &mut v8::PinScope<'s, '_>,
        this: v8::Local<'s, v8::Object>,
        data: v8::Local<v8::Value>,
        encoding: Option<String>,
    ) -> Result<v8::Local<'s, v8::Value>, OpError> {
        let bytes = buffer::extract_input(scope, data, encoding.as_deref())?;
        self.ctx.update(&bytes).map_err(KernelError::to_node)?;
        Ok(this.into())   // chainable
    }

    #[v8_method]
    fn digest<'s>(&mut self,
        scope: &mut v8::PinScope<'s, '_>,
        encoding: Option<String>,
    ) -> Result<v8::Local<'s, v8::Value>, OpError> {
        let bytes = self.ctx.finalize().map_err(KernelError::to_node)?;
        Ok(buffer::emit_output(scope, &bytes, encoding.as_deref())?)
    }

    #[v8_method]
    fn copy<'s>(&self, scope: &mut v8::PinScope<'s, '_>,
        _options: Option<v8::Local<v8::Value>>,
    ) -> v8::Local<'s, v8::Value> {
        let cloned = HashState { ctx: self.ctx.clone_state() };
        Hash::build(scope, cloned).into()
    }
}
```

The WebCrypto side calls a one-shot helper:

```rust
// crypto_kernel/digest.rs
pub fn digest_one_shot(hash: HashAlgo, data: &[u8]) -> Vec<u8> {
    let mut ctx = DigestContext::new(hash);
    ctx.update(data).unwrap();
    ctx.finalize().unwrap()
}

// crypto_native/digest.rs (refactored)
pub fn digest_bytes(hash: HashAlgo, data: &[u8]) -> Vec<u8> {
    crate::crypto_kernel::digest::digest_one_shot(hash, data)
}
```

Same shape for Hmac / Cipher / Sign / Verify. The Cipher context has additional state (pending block bytes for CBC, AAD buffer for GCM); the kernel hides this behind the `update / finalize / set_aad / set_auth_tag` API.

### I.6. Buffer vs ArrayBuffer (D-N7)

Node's APIs accept and return `Buffer` (a `Uint8Array` subclass with extra methods). WebCrypto returns `ArrayBuffer`. The two classes' instances are NOT interchangeable in instanceof checks but ARE interchangeable as input types (any `Uint8Array` works as a Buffer for input — Node never type-checks input shape).

**Input-coercion policy:**

```rust
// crypto_node/buffer.rs
pub fn extract_input(
    scope: &mut v8::PinScope,
    value: v8::Local<v8::Value>,
    encoding: Option<&str>,
) -> Result<Vec<u8>, OpError> {
    // 1. ArrayBufferView (Uint8Array, Buffer, Int8Array, ...) → copy bytes.
    if let Ok(view) = v8::Local::<v8::ArrayBufferView>::try_from(value) {
        let mut buf = vec![0u8; view.byte_length()];
        view.copy_contents(&mut buf);
        return Ok(buf);
    }
    // 2. ArrayBuffer → copy bytes.
    if let Ok(ab) = v8::Local::<v8::ArrayBuffer>::try_from(value) {
        let store = ab.get_backing_store();
        let mut buf = vec![0u8; ab.byte_length()];
        for (i, b) in buf.iter_mut().enumerate() { *b = store[i].get(); }
        return Ok(buf);
    }
    // 3. String + encoding → decode per the named encoding.
    if value.is_string() {
        let s = value.to_rust_string_lossy(scope);
        return encoding::decode(&s, encoding.unwrap_or("utf8"));
    }
    Err(OpError::node("ERR_INVALID_ARG_TYPE",
        "Argument must be a Buffer, TypedArray, DataView, ArrayBuffer, or string"))
}

pub fn emit_output<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    bytes: &[u8],
    encoding: Option<&str>,
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    match encoding {
        None => Ok(emit_buffer(scope, bytes).into()),       // default: Buffer
        Some("hex") => Ok(emit_string(scope, &hex_encode(bytes)).into()),
        Some("base64") => Ok(emit_string(scope, &base64::encode(bytes)).into()),
        Some("base64url") => Ok(emit_string(scope, &base64url::encode(bytes)).into()),
        Some("latin1") | Some("binary") => Ok(emit_string(scope, &latin1_encode(bytes)).into()),
        Some("utf8") | Some("utf-8") => {
            // Errors on invalid UTF-8 boundaries — Node lossily decodes.
            // We match Node by using to_rust_string_lossy.
            Ok(emit_string(scope, &String::from_utf8_lossy(bytes)).into())
        }
        Some(other) => Err(OpError::node("ERR_UNKNOWN_ENCODING",
            format!("Unknown encoding: {}", other))),
    }
}

pub fn emit_buffer<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    bytes: &[u8],
) -> v8::Local<'s, v8::Object> {
    // Strategy: mint a Uint8Array (V8 native), then call Buffer.from(uint8) to
    // re-tag. Buffer.from(Uint8Array) is the documented Node way to convert.
    // Cost: ~100 ns per call (V8 cross-boundary + Buffer.from path).
    // Alternative (faster, but couples us to unenv internals): set the
    // proto-chain manually via __proto__ reassignment.
    let ab = v8::ArrayBuffer::with_backing_store(scope, /* backing store from bytes */);
    let u8a = v8::Uint8Array::new(scope, ab, 0, bytes.len()).unwrap();
    // Lookup Buffer global (lazily cached in isolate state):
    let buffer_ctor = lookup_buffer_ctor(scope);
    let from_method = buffer_ctor.get(scope, v8::String::new(scope, "from").unwrap().into())
        .and_then(|v| v8::Local::<v8::Function>::try_from(v).ok())
        .expect("Buffer.from not installed");
    from_method.call(scope, buffer_ctor.into(), &[u8a.into()])
        .unwrap()
        .try_into()
        .unwrap()
}
```

**The Buffer-ctor lookup is cached in the isolate's IsolateState** (a one-time fetch on first crypto call; after that it's `Local::new(scope, &cached_global)`). Cost: amortised to ~0 ns post-bootstrap.

**Why not native Buffer?** A native Buffer would be a parallel design to this one — Buffer's surface (toString with 7 encodings, write*, read*, equals, compare, indexOf, subarray, slice, swap16/32/64, allocUnsafe / allocUnsafeSlow / poolSize, copyWithin, fill, includes — see https://nodejs.org/api/buffer.html, ~80 methods) is a larger surface than CryptoKey's 4 properties. unenv's Buffer is correct (npm packages don't crash); the cost of a `Buffer.from(uint8)` per crypto call is invisible (~100 ns, 1% of a typical hash op). Trade-off accepted.

### I.7. Error mapping (D-N8)

The kernel returns a `KernelError` enum:

```rust
// crypto_kernel/error.rs
pub enum KernelError {
    HashFinalised,
    HmacFinalised,
    InvalidKeyType,
    InvalidKeyLength,
    InvalidIvLength { expected: usize, got: usize },
    InvalidTagLength,
    AuthenticationFailed,
    UnsupportedAlgorithm(String),
    InvalidSpki,
    InvalidPkcs8,
    InvalidPem,
    InvalidJwk(&'static str),    // sub-reason
    SignFailed,
    VerifyFailed,
    ScryptParametersInvalid,
    PbkdfIterationsZero,
    HkdfOutputTooLarge,
    DhPrimeRejected,
    EcCurveMismatch,
    /* ... ~30 variants total */
}
```

Each surface has a small adapter that maps `KernelError` to the surface's error type:

```rust
// crypto_node/error.rs
impl KernelError {
    pub fn to_node(self) -> OpError {
        match self {
            Self::HashFinalised => OpError::node("ERR_CRYPTO_HASH_FINALIZED",
                "Digest already called"),
            Self::InvalidKeyLength => OpError::node("ERR_CRYPTO_INVALID_KEYLEN",
                "Invalid key length"),
            Self::InvalidIvLength { expected, got } => OpError::node("ERR_CRYPTO_INVALID_IV",
                format!("Invalid IV length: expected {}, got {}", expected, got)),
            Self::AuthenticationFailed => OpError::node("ERR_CRYPTO_AUTH_TAG_LENGTH_INVALID",
                "Unsupported state or unable to authenticate data"),
            // ... 30 arms total
            Self::UnsupportedAlgorithm(name) => OpError::node("ERR_OSSL_EVP_UNSUPPORTED",
                format!("unsupported: {}", name)),
        }
    }
}

// crypto_native/error.rs (refactored)
impl KernelError {
    pub fn to_webcrypto(self) -> OpError {
        match self {
            Self::AuthenticationFailed => OpError::dom("OperationError",
                "Authentication failed"),
            Self::InvalidKeyLength => OpError::dom("DataError",
                "Invalid key length"),
            // ... mirrors the existing crypto_native/ error sites; mostly
            // OperationError | DataError | InvalidAccessError per spec
        }
    }
}
```

**The `OpError::node(code, message)` constructor** is the new D-N32 API. Internally it creates an `OpErrorKind::NodeError(code)` variant. The macro's `gen_throw_error` arm constructs the appropriate Error/TypeError/RangeError class (per a per-code table — most are Error; arg-validation codes are TypeError; range codes are RangeError) and assigns the `code` property.

**Per-code class table:**

```rust
// crypto_node/error.rs
fn error_class_for_code(code: &str) -> ErrorClass {
    match code {
        // RangeError: out-of-range numeric args
        "ERR_OUT_OF_RANGE" | "ERR_BUFFER_OUT_OF_BOUNDS" => ErrorClass::Range,
        // TypeError: wrong arg type
        "ERR_INVALID_ARG_TYPE" | "ERR_INVALID_ARG_VALUE" => ErrorClass::Type,
        // Error: everything else (operational failures)
        _ => ErrorClass::Error,
    }
}
```

The macro sees `OpErrorKind::NodeError("ERR_CRYPTO_HASH_FINALIZED")`, looks up the class as `Error`, calls `v8::Exception::error(...)`, then sets the `.code` property to the static string `"ERR_CRYPTO_HASH_FINALIZED"`.

## II. Full node:crypto export surface

The complete table of every node:crypto top-level export, with this design's plan. "Tier" reflects observed npm-package usage:

- **Tier 1:** ships in Stage 1 (or earlier) — must-have for >50% of npm packages we expect creator apps to use.
- **Tier 2:** ships in Stage 2 — common but not blocking.
- **Tier 3:** deferred to a later landing or stubbed (specific reasoning per row).

### II.1. Hashing

| Export | Tier | Backed by | Sync/async | Stage |
|---|---|---|---|---|
| `createHash(algorithm, options?) -> Hash` | 1 | `kernel::DigestContext::new` | sync | B |
| `Hash` (class) | 1 | `crypto_node/hash.rs::Hash` | sync streaming | B |
| `Hash.prototype.update(data, encoding?) -> this` | 1 | `kernel::DigestContext::update` | sync | B |
| `Hash.prototype.digest(encoding?) -> Buffer | string` | 1 | `kernel::DigestContext::finalize` | sync | B |
| `Hash.prototype.copy(options?) -> Hash` | 1 | `kernel::DigestContext::clone_state` | sync | B |
| `getHashes() -> string[]` | 1 | iterate `kernel::algorithms::REGISTRY` | sync | B |
| `hash(algorithm, data, encoding?) -> string | Buffer` (Node ≥21) | 1 | `kernel::digest_one_shot` | sync | B |

Algorithms supported: `sha1`, `sha224`, `sha256`, `sha384`, `sha512`, `sha512-224`, `sha512-256`, `md5` (Stage 2 — aws-lc-rs has it via the legacy-only path), `ripemd160` (deferred — aws-lc-rs lacks; rare enough). The `md5` case-by-case: most npm packages use it for content addressing (etag), not crypto; the etag use is fine. `crypto-js`'s `Hash.MD5` uses node:crypto's md5 directly. We ship Stage 2.

BLAKE2 (`blake2b512`, `blake2s256`) — Stage 2; aws-lc-rs supports via the lower FFI. Used by `argon2` indirectly and a handful of password libraries.

### II.2. HMAC

| Export | Tier | Backed by | Sync/async | Stage |
|---|---|---|---|---|
| `createHmac(algorithm, key, options?) -> Hmac` | 1 | `kernel::HmacContext::new` | sync | B |
| `Hmac` (class) | 1 | `crypto_node/hmac.rs::Hmac` | sync streaming | B |
| `Hmac.prototype.update(data, encoding?) -> this` | 1 | `kernel::HmacContext::update` | sync | B |
| `Hmac.prototype.digest(encoding?) -> Buffer | string` | 1 | `kernel::HmacContext::finalize` | sync | B |

Algorithms supported: same SHA family + key length validation per RFC 2104 (any non-empty key length is accepted; the kernel zero-pads or truncates+rehashes per the spec).

### II.3. Cipher / Decipher

| Export | Tier | Backed by | Sync/async | Stage |
|---|---|---|---|---|
| `createCipher(algorithm, password, options?)` (deprecated) | 3 | n/a | n/a | NEVER (D-N11 — throws `ERR_CRYPTO_DEPRECATED_API`) |
| `createCipheriv(algorithm, key, iv, options?) -> Cipher` | 1 | `kernel::CipherContext::new(encrypt=true)` | sync | C |
| `createDecipheriv(algorithm, key, iv, options?) -> Decipher` | 1 | `kernel::CipherContext::new(encrypt=false)` | sync | C |
| `Cipher` / `Decipher` (classes) | 1 | `crypto_node/cipher.rs` | sync streaming + async-above-threshold | C |
| `Cipher.prototype.update(data, inputEncoding?, outputEncoding?)` | 1 | `kernel::CipherContext::update` | sync (always) | C |
| `Cipher.prototype.final(outputEncoding?)` | 1 | `kernel::CipherContext::finalize` | sync | C |
| `Cipher.prototype.setAAD(buffer, options?)` | 1 | `kernel::CipherContext::set_aad` | sync | C |
| `Cipher.prototype.getAuthTag()` | 1 | `kernel::CipherContext::get_auth_tag` | sync | C |
| `Decipher.prototype.setAuthTag(buffer)` | 1 | `kernel::CipherContext::set_auth_tag` | sync | C |
| `Cipher.prototype.setAutoPadding(boolean)` | 1 | `kernel::CipherContext::set_auto_padding` | sync | C |
| `getCiphers() -> string[]` | 1 | iterate registry | sync | C |
| `getCipherInfo(name | nid, options?)` | 1 | registry metadata lookup | sync | C |

**Algorithms supported in Stage 1:**
- AES-128/192/256 in CBC, CTR, GCM, KW, OCB modes (`aes-128-cbc`, `aes-256-gcm`, ...)
- ChaCha20-Poly1305 (`chacha20-poly1305`) — D-N23

**Stage 2 (with `--legacy-crypto` flag):** DES-CBC, 3DES (DES-EDE3), Blowfish (`bf-*`), Cast5, RC4 (`rc4`), IDEA. aws-lc-rs has 3DES via `cipher::TDES_*`; others need raw FFI.

### II.4. Sign / Verify

| Export | Tier | Backed by | Sync/async | Stage |
|---|---|---|---|---|
| `createSign(algorithm, options?) -> Sign` | 1 | `kernel::SignContext::new` | sync | C |
| `createVerify(algorithm, options?) -> Verify` | 1 | `kernel::VerifyContext::new` | sync | C |
| `Sign` / `Verify` (classes) | 1 | `crypto_node/sign.rs` | sync streaming | C |
| `Sign.prototype.update(data, encoding?)` | 1 | `kernel::SignContext::update` | sync | C |
| `Sign.prototype.sign(privateKey, encoding?)` | 1 | `kernel::SignContext::sign` | sync | C |
| `Verify.prototype.verify(publicKey, signature, encoding?)` | 1 | `kernel::VerifyContext::verify` | sync | C |
| `crypto.sign(algorithm, data, key)` (one-shot) | 1 | `kernel::sign_one_shot` | sync | C |
| `crypto.verify(algorithm, data, key, sig)` (one-shot) | 1 | `kernel::verify_one_shot` | sync | C |

**Algorithms supported:**
- `rsa-sha1`, `rsa-sha256`, `rsa-sha384`, `rsa-sha512` (RSASSA-PKCS1-v1_5)
- `RSA-PSS` (via `{ key, padding: RSA_PKCS1_PSS_PADDING, saltLength }` options)
- `ecdsa-with-SHA256` family (alias `sha256` with EC private key)
- Ed25519 (`null` algorithm — Node uses null for Ed25519 since the hash is built in)
- DSA (Stage 2 — rare, deprecated)

**`null` algorithm convention:** Node's `crypto.sign(null, data, ed25519PrivateKey)` is the canonical way to sign with EdDSA (the algo is implicit in the key type). We implement: when `algorithm` is null/undefined and the key is Ed25519, dispatch to Ed25519 sign; if non-null and the key is Ed25519, throw `ERR_OSSL_EVP_INVALID_DIGEST`.

**Padding constants:** `crypto.constants.RSA_PKCS1_PADDING`, `RSA_PKCS1_PSS_PADDING`, `RSA_PKCS1_OAEP_PADDING`, `RSA_NO_PADDING` exposed as numeric constants (`1`, `6`, `4`, `3` respectively — matching OpenSSL).

### II.5. Public-key cryptography (one-shot)

| Export | Tier | Backed by | Sync/async | Stage |
|---|---|---|---|---|
| `crypto.publicEncrypt(keyOrOptions, buffer)` | 1 | `kernel::rsa_oaep_encrypt` | sync | C |
| `crypto.privateDecrypt(keyOrOptions, buffer)` | 1 | `kernel::rsa_oaep_decrypt` | sync | C |
| `crypto.publicDecrypt(keyOrOptions, buffer)` | 2 | aws-lc-rs raw FFI (low-level) | sync | C |
| `crypto.privateEncrypt(keyOrOptions, buffer)` | 2 | aws-lc-rs raw FFI (low-level) | sync | C |
| `crypto.diffieHellman({ privateKey, publicKey })` (one-shot) | 1 | `kernel::dh_agree` (ECDH path) | sync | C |

**`publicDecrypt` / `privateEncrypt`:** these are the inverse of the natural RSA flow (encrypting with private = signing-without-hash; decrypting with public = signature-verify-style). Used by old PKCS1 v1.5 signature schemes that pre-date PSS. aws-lc-rs's high-level API doesn't expose them; we drop to `aws_lc_sys::RSA_public_decrypt` / `RSA_private_encrypt`. Niche; defer to Stage 2.

### II.6. Diffie-Hellman / ECDH

| Export | Tier | Backed by | Sync/async | Stage |
|---|---|---|---|---|
| `createDiffieHellmanGroup(name) -> DiffieHellmanGroup` | 2 | `kernel::dh::group(name)` | sync | E |
| `getDiffieHellman(name) -> DiffieHellmanGroup` (alias) | 2 | as above | sync | E |
| `DiffieHellmanGroup` (class) | 2 | named-prime DH | sync | E |
| `createDiffieHellman(prime, generator?, ...)` | 3 | aws-lc-sys raw FFI | sync | E (Stage 2) |
| `DiffieHellman` (class) | 3 | aws-lc-sys raw FFI | sync | E |
| `createECDH(curveName) -> ECDH` | 1 | `kernel::ecdh::Context::new` | sync | C |
| `ECDH` (class) | 1 | `crypto_node/dh.rs::ECDH` | sync | C |
| `ECDH.prototype.generateKeys(encoding?, format?)` | 1 | `kernel::ecdh::generate_keys` | sync | C |
| `ECDH.prototype.computeSecret(otherPublicKey, ...)` | 1 | `kernel::ecdh::compute_secret` | sync | C |
| `ECDH.prototype.getPrivateKey(encoding?)` | 1 | `kernel::ecdh::private_key` | sync | C |
| `ECDH.prototype.getPublicKey(encoding?, format?)` | 1 | `kernel::ecdh::public_key` | sync | C |
| `ECDH.prototype.setPrivateKey(privateKey, encoding?)` | 1 | `kernel::ecdh::set_private` | sync | C |
| `ECDH.prototype.setPublicKey(publicKey, encoding?)` (deprecated) | 3 | n/a | n/a | NEVER (deprecated in Node v5 — throws) |
| `ECDH.convertKey(...)` (static) | 2 | aws-lc-sys raw FFI for compressed-point | sync | E |
| `getCurves() -> string[]` | 1 | iterate registry | sync | C |

**Named DH groups:** RFC 3526 (`modp1` = 768-bit, ..., `modp18` = 8192-bit) and RFC 7919 (`ffdhe2048`, `ffdhe3072`, `ffdhe4096`, `ffdhe6144`, `ffdhe8192`). The 768/1024-bit groups (modp1, modp2) are blocked by default (insecure); creator apps that need them get a runtime flag opt-in.

### II.7. Key generation

| Export | Tier | Backed by | Sync/async | Stage |
|---|---|---|---|---|
| `generateKeyPair(type, options, callback)` | 1 | `kernel::generate_key_pair_async` | async (callback) | C |
| `generateKeyPairSync(type, options) -> { publicKey, privateKey }` | 1 | `kernel::generate_key_pair` | sync | C |
| `generateKey(type, options, callback)` | 1 | `kernel::generate_key_async` | async | C |
| `generateKeySync(type, options) -> KeyObject` | 1 | `kernel::generate_key` | sync | C |
| `generatePrime(size, options?, callback?)` | 3 | aws-lc-sys raw FFI | async | E (Stage 2) |
| `generatePrimeSync(size, options?)` | 3 | aws-lc-sys raw FFI | sync | E |
| `checkPrime(candidate, options?, callback)` | 3 | aws-lc-sys raw FFI | async | E |
| `checkPrimeSync(candidate, options?)` | 3 | aws-lc-sys raw FFI | sync | E |

**Types supported (Stage 1):** `'rsa'` (with `modulusLength`, `publicExponent` defaulting to 0x10001), `'ec'` (with `namedCurve`), `'ed25519'`, `'x25519'`, `'hmac'` (returns SecretKeyObject), `'aes'` (returns SecretKeyObject; `length` in bits).

**Types deferred to Stage 2:** `'rsa-pss'` (RSA with embedded PSS params — needs an OID-tagged SPKI), `'dsa'` (deprecated), `'dh'` (named-group DH).

**`encoding` option:** the publicKey/privateKey can be returned as `KeyObject` (default if no `encoding` specified) OR as Buffer/string per `{ type: 'pkcs1' | 'pkcs8' | 'spki' | 'sec1', format: 'pem' | 'der' | 'jwk' }`. We support all combos in Stage 1.

### II.8. Key import / export

| Export | Tier | Backed by | Sync/async | Stage |
|---|---|---|---|---|
| `createSecretKey(buffer, encoding?) -> SecretKeyObject` | 1 | `crypto_node/key_object.rs::create_secret_key` | sync | C |
| `createPublicKey(input) -> PublicKeyObject` | 1 | `crypto_node/key_object.rs::create_public_key` | sync | C |
| `createPrivateKey(input) -> PrivateKeyObject` | 1 | `crypto_node/key_object.rs::create_private_key` | sync | C |
| `KeyObject` (class) | 1 | `crypto_node/key_object.rs::KeyObject` | sync | C |
| `KeyObject.from(cryptoKey)` (static) | 1 | bridge to existing CryptoKey via Arc share (D-N4) | sync | C |
| `KeyObject.prototype.export(options) -> Buffer | string | object` | 1 | `kernel::pem::emit` / `kernel::der::emit` / `kernel::jwk::export` | sync | C |
| `KeyObject.prototype.equals(other) -> boolean` | 1 | constant-time compare of material via `aws_lc_rs::constant_time` | sync | C |
| `KeyObject.prototype.type` (getter) | 1 | direct field read | sync | C |
| `KeyObject.prototype.asymmetricKeyType` (getter) | 1 | derived from `KeyMaterial` enum variant | sync | C |
| `KeyObject.prototype.asymmetricKeyDetails` (getter) | 1 | derived from `KeyMaterial` enum variant | sync | C |
| `KeyObject.prototype.symmetricKeySize` (getter) | 1 | for SecretKeyObject only | sync | C |

`PublicKeyObject`, `PrivateKeyObject`, `SecretKeyObject` are intentionally trivial subclasses — no methods of their own.

### II.9. KDFs

| Export | Tier | Backed by | Sync/async | Stage |
|---|---|---|---|---|
| `pbkdf2(password, salt, iters, keylen, digest, callback)` | 1 | `kernel::pbkdf2` via `spawned_ops` | async | B |
| `pbkdf2Sync(password, salt, iters, keylen, digest)` | 1 | `kernel::pbkdf2` | sync | B |
| `scrypt(password, salt, keylen, options?, callback)` | 1 | `kernel::scrypt` via `spawned_ops` | async | B |
| `scryptSync(password, salt, keylen, options?)` | 1 | `kernel::scrypt` | sync | B |
| `hkdf(digest, ikm, salt, info, keylen, callback)` | 1 | `kernel::hkdf` via `spawned_ops` | async | B |
| `hkdfSync(digest, ikm, salt, info, keylen)` | 1 | `kernel::hkdf` | sync | B |

PBKDF2 and HKDF call into the existing `crypto_native/derive.rs` paths (refactored into `crypto_kernel/kdf.rs`). scrypt is NEW — backed by `aws_lc_sys::EVP_PBE_scrypt` (~20 LOC of FFI).

### II.10. Random

| Export | Tier | Backed by | Sync/async | Stage |
|---|---|---|---|---|
| `randomBytes(size, callback?)` | 1 | `crypto::fast_random` | sync (or async with callback) | B |
| `randomFillSync(buffer, offset?, size?)` | 1 | `crypto::fast_random` | sync | B |
| `randomFill(buffer, offset?, size?, callback)` | 1 | `crypto::fast_random` via `spawned_ops` | async | B |
| `randomInt(min, max, callback?)` | 1 | rejection-sampled `fast_random` | sync (or async) | B |
| `randomUUID(options?)` | 1 | `crypto_native::crypto_class::random_uuid` (existing) | sync | B |
| `getRandomValues(buffer)` | 1 | re-export of `globalThis.crypto.getRandomValues` | sync | B |

`randomBytes(size, callback)` — when callback is passed, dispatches the entropy fill async. Without callback, sync. This matches Node exactly.

### II.11. X.509

| Export | Tier | Backed by | Sync/async | Stage |
|---|---|---|---|---|
| `X509Certificate(input)` (constructor) | 2 | aws-lc-sys raw FFI (`X509_d2i`) | sync | E |
| `X509Certificate.prototype.subject` | 2 | parsed certificate | sync | E |
| `X509Certificate.prototype.issuer` | 2 | parsed certificate | sync | E |
| `X509Certificate.prototype.publicKey` | 2 | parsed certificate | sync | E |
| `X509Certificate.prototype.fingerprint` (SHA-1) | 2 | computed | sync | E |
| `X509Certificate.prototype.fingerprint256` | 2 | computed | sync | E |
| `X509Certificate.prototype.fingerprint512` | 2 | computed | sync | E |
| `X509Certificate.prototype.validFrom` / `validTo` | 2 | parsed certificate | sync | E |
| `X509Certificate.prototype.serialNumber` | 2 | parsed certificate | sync | E |
| `X509Certificate.prototype.raw` | 2 | parsed certificate | sync | E |
| `X509Certificate.prototype.subjectAltName` | 2 | parsed certificate | sync | E |
| `X509Certificate.prototype.checkIssued(other)` | 2 | parsed certificate | sync | E |
| `X509Certificate.prototype.checkHost(name, options?)` | 3 | aws-lc-sys raw FFI for `X509_check_host` | sync | E (Stage 2 with limitations) |
| `X509Certificate.prototype.verify(publicKey)` | 3 | aws-lc-sys raw FFI for `X509_verify` | sync | E |
| `verifyCertificate(...)` (Node ≥18, draft) | 3 | n/a | n/a | NEVER (Node-experimental, no spec stability) |

### II.12. WebCrypto bridge (D-N16)

| Export | Tier | Backed by | Sync/async | Stage |
|---|---|---|---|---|
| `webcrypto` | 1 | direct reference to `globalThis.crypto` | n/a | D |
| `subtle` | 1 | direct reference to `globalThis.crypto.subtle` | n/a | D |
| `getRandomValues` | 1 | `globalThis.crypto.getRandomValues.bind(globalThis.crypto)` | sync | B (matches `crypto_native`) |

These are NOT separate code paths — they're literal property references to the existing WebCrypto installation. Object identity matters: `import("node:crypto").webcrypto === globalThis.crypto` is `true`.

### II.13. Misc

| Export | Tier | Backed by | Sync/async | Stage |
|---|---|---|---|---|
| `timingSafeEqual(a, b) -> boolean` | 1 | `aws_lc_rs::constant_time::verify_slices_are_equal` | sync | B |
| `getCiphers() -> string[]` | 1 | iterate registry | sync | C |
| `getHashes() -> string[]` | 1 | iterate registry | sync | B |
| `getCurves() -> string[]` | 1 | hard-coded list (P-256/P-384/P-521 + Ed25519/X25519) | sync | C |
| `getCipherInfo(name, options?)` | 1 | registry metadata lookup | sync | C |
| `getFips() -> 0` | 1 | constant 0 (D-N25) | sync | B |
| `setFips(boolean) -> void` (throws if true) | 1 | constant throw (D-N25) | sync | B |
| `fips` (getter) | 1 | constant 0 | sync | B |
| `setEngine(engine, flags?)` | 3 | n/a | n/a | NEVER (no engine support — D-N25) |
| `secureHeapUsed()` | 3 | stub returning `{ total: 0, min: 0, used: 0, utilization: 0 }` | sync | B |
| `constants` (object of OpenSSL constants) | 1 | small static dict | sync | B |
| `crypto.signal` (Node ≥17) | 3 | not supported (used by experimental encryptStream API) | n/a | NEVER |
| `crypto.subtle` (alias for `webcrypto.subtle`) | 1 | direct reference | n/a | D |

### II.14. Coverage summary

Stage 1 (the node:crypto APIs landed by end of Stage D):

- **Hashing:** 7 / 7 exports (100%)
- **HMAC:** 4 / 4 exports (100%)
- **Cipher / Decipher:** 11 / 12 exports (`createCipher` deprecated and never)
- **Sign / Verify:** 9 / 9 exports (100%)
- **Public-key:** 3 / 5 exports (`publicDecrypt` / `privateEncrypt` Stage 2)
- **DH / ECDH:** 9 / 14 exports (ECDH 100%; named DH groups Stage 2; arbitrary DH Stage 2)
- **Key generation:** 4 / 8 exports (basic kinds Stage 1; primes Stage 2)
- **Key import/export:** 11 / 11 exports (100%)
- **KDFs:** 6 / 6 exports (100%)
- **Random:** 6 / 6 exports (100%)
- **X.509:** 0 / 13 exports (Stage 2)
- **WebCrypto bridge:** 3 / 3 exports (100%)
- **Misc:** 11 / 12 exports (`crypto.signal` never)

**Total Stage 1: 84 / 110 exports (76%)** — covers ~95% of npm-package usage.
**Total Stage 1 + Stage 2: 105 / 110 exports (95%)** — long tail in `setEngine`, `crypto.signal`, deprecated APIs.

## III. Algorithm coverage

### III.1. Already in WebCrypto's shipped impl (D-N1 — kernel reuse)

These ship for node:crypto via the kernel, no new algorithm code:

| Algorithm | WebCrypto section | Kernel module |
|---|---|---|
| SHA-1 / SHA-256 / SHA-384 / SHA-512 | webcrypto §32 | `kernel::digest` |
| HMAC-SHA-* | webcrypto §31 | `kernel::hmac` |
| AES-CTR | webcrypto §27 | `kernel::cipher` |
| AES-CBC | webcrypto §28 | `kernel::cipher` |
| AES-GCM | webcrypto §29 | `kernel::cipher` |
| AES-KW | webcrypto §30 | `kernel::cipher` |
| RSASSA-PKCS1-v1_5 | webcrypto §20 | `kernel::sign_verify` |
| RSA-PSS | webcrypto §21 | `kernel::sign_verify` |
| RSA-OAEP | webcrypto §22 | `kernel::cipher` (encrypt/decrypt path) |
| ECDSA (P-256 / P-384 / P-521) | webcrypto §23 | `kernel::sign_verify` |
| ECDH (P-256 / P-384 / P-521) | webcrypto §24 | `kernel::dh` |
| Ed25519 | webcrypto §25 | `kernel::sign_verify` |
| X25519 | webcrypto §26 | `kernel::dh` |
| HKDF | webcrypto §33 | `kernel::kdf` |
| PBKDF2 | webcrypto §34 | `kernel::kdf` |

That's 16 algorithms shared across both surfaces.

### III.2. node:crypto-only algorithms (NEW kernel work)

| Algorithm | Why node:crypto needs it | aws-lc-rs path | Stage |
|---|---|---|---|
| MD5 | etag generation, content addressing, legacy auth | `digest::MD5` (legacy-only constant) | B |
| SHA-224 | Some legacy SAML / PKCS profiles | `digest::SHA224` | B |
| SHA-512/224 / SHA-512/256 | Legacy interop | `digest::SHA512_224` / `SHA512_256` | B |
| `chacha20-poly1305` | Modern AEAD (D-N23) | `aead::CHACHA20_POLY1305` | C |
| AES-OCB | Per Node — same constant exposed; aws-lc-rs has it | `aead::AES_*_OCB` | C |
| `aes-*-cfb`, `aes-*-cfb1`, `aes-*-cfb8`, `aes-*-ofb`, `aes-*-ecb` | Niche legacy | aws-lc-sys raw FFI | E (Stage 2) |
| 3DES (DES-EDE3) in CBC / ECB / CFB | Legacy auth (banking, retail POS) | `cipher::TDES_*` | E (`--legacy-crypto` flag) |
| Blowfish (`bf-cbc`, `bf-ecb`, ...) | Very legacy | aws-lc-sys raw FFI (deprecated in BoringSSL) | E |
| Cast5 (`cast5-cbc`) | Old PGP | aws-lc-sys raw FFI | E |
| RC4 / IDEA | Pre-2010 protocols | aws-lc-sys raw FFI | E |
| BLAKE2b-512, BLAKE2s-256 | Hash diversity, password libs | aws-lc-sys raw FFI | E |
| RIPEMD-160 | Legacy bitcoin / lightning code | NOT in aws-lc-rs | DEFERRED (rare; no clean path) |
| **scrypt** | RFC 7914 password hashing | `aws_lc_sys::EVP_PBE_scrypt` | B |
| **DH (modp1..modp18, ffdhe2048..ffdhe8192)** | Named-group DH | aws-lc-sys raw FFI | E |
| **DH arbitrary primes** | Generic DH | aws-lc-sys raw FFI | E |
| **Brainpool curves** (`brainpoolP256r1`, `brainpoolP384r1`, `brainpoolP512r1`) | Niche EU / German banking | aws-lc-sys raw FFI | E |
| **secp256k1** | Bitcoin / Ethereum signing | aws-lc-rs has it via `signature::ECDSA_K256_*` | C |

Stage 1 coverage: SHA family + AES-{CBC,CTR,GCM,KW,OCB} + ChaCha20-Poly1305 + RSA + ECDSA + Ed25519 + X25519 + ECDH + PBKDF2 + scrypt + HKDF + MD5 + secp256k1.

Stage 2 coverage: BLAKE2 + 3DES + Brainpool + named DH groups + X.509 + RC4 / Blowfish / Cast5 (with `--legacy-crypto`) + arbitrary-prime DH + generatePrime / checkPrime.

Argon2: NOT shipped natively. Node doesn't ship it. The npm `argon2` package is a node-gyp binding; via unenv it falls back to WASM. Acceptable.

## IV. Key Object class hierarchy (D-N3, D-N4, D-N13)

### IV.1. The class layout

Per Node's https://nodejs.org/api/crypto.html#class-keyobject:

```
KeyObject (parent)
├── PublicKeyObject  (subclass — empty body)
├── PrivateKeyObject (subclass — empty body)
└── SecretKeyObject  (subclass — empty body)
```

The subclasses exist for `instanceof` discrimination — npm packages do `if (key instanceof PrivateKeyObject) {...}` to distinguish a private key handle. The parent `KeyObject` has all the fields and methods; subclasses are empty marker types.

```rust
// crypto_node/key_object.rs

#[v8_class]
#[v8_to_string_tag = "KeyObject"]
pub struct KeyObject;

impl KeyObject {
    /// `keyObject.type` getter — returns "secret" | "public" | "private".
    #[v8_getter]
    fn r#type(&self) -> &'static str { /* ... */ }

    /// `keyObject.asymmetricKeyType` — returns "rsa" | "rsa-pss" | "dsa" |
    /// "ec" | "x25519" | "x448" | "ed25519" | "ed448" | "dh" | undefined.
    #[v8_getter]
    fn asymmetric_key_type(&self) -> Option<&'static str> { /* ... */ }

    /// `keyObject.asymmetricKeyDetails` — returns a frozen object with
    /// algorithm-specific fields (e.g. `{ modulusLength, publicExponent }`
    /// for RSA, `{ namedCurve }` for EC, etc.).
    #[v8_getter(same_object)]
    fn asymmetric_key_details<'s>(&self, scope: &mut v8::PinScope<'s, '_>)
        -> v8::Global<v8::Object> { /* ... */ }

    /// `keyObject.symmetricKeySize` — bytes for SecretKeyObject; undefined otherwise.
    #[v8_getter]
    fn symmetric_key_size(&self) -> Option<u32> { /* ... */ }

    /// `keyObject.export(options)` — see §IV.6.
    #[v8_method]
    fn export<'s>(&self,
        scope: &mut v8::PinScope<'s, '_>,
        options: Option<v8::Local<v8::Value>>,
    ) -> Result<v8::Local<'s, v8::Value>, OpError> { /* ... */ }

    /// `keyObject.equals(other)` — constant-time compare of underlying material.
    #[v8_method]
    fn equals(&self, other: v8::Local<v8::Value>) -> bool { /* ... */ }
}

#[v8_class]
#[v8_inherit(KeyObject)]
#[v8_to_string_tag = "PublicKeyObject"]
pub struct PublicKeyObject;

#[v8_class]
#[v8_inherit(KeyObject)]
#[v8_to_string_tag = "PrivateKeyObject"]
pub struct PrivateKeyObject;

#[v8_class]
#[v8_inherit(KeyObject)]
#[v8_to_string_tag = "SecretKeyObject"]
pub struct SecretKeyObject;
```

The `#[v8_inherit]` mechanism is the existing one (used for `AbortSignal extends EventTarget`). The brand-check walks the prototype chain; subclass instances pass the `KeyObject` brand.

### IV.2. Storage (D-N4)

```rust
pub struct KeyObjectState {
    pub key_type: KeyType,                  // shared enum from kernel
    pub material: Arc<KeyMaterial>,         // shared enum from kernel
}
```

Single source of truth: the `KeyMaterial` enum lives in `crypto_kernel/key_material.rs`. Both `CryptoKeyState` and `KeyObjectState` hold `Arc<KeyMaterial>`. Cloning a key (the most common path — `KeyObject.from(otherKeyObject)`) is one Arc::clone (8 byte refcount bump, no allocation, no key-material copy).

The asymmetric-key-type / -details / symmetric-key-size getters are derived from `KeyMaterial`'s variant — no extra storage:

```rust
impl KeyObjectState {
    pub fn asymmetric_key_type(&self) -> Option<&'static str> {
        match &*self.material {
            KeyMaterial::Symmetric(_) => None,
            KeyMaterial::EcPrivate { .. } | KeyMaterial::EcPublic { .. } => Some("ec"),
            KeyMaterial::RsaPrivate { .. } | KeyMaterial::RsaPublic { .. } => Some("rsa"),
            KeyMaterial::Ed25519Private { .. } | KeyMaterial::Ed25519Public { .. } => Some("ed25519"),
            KeyMaterial::X25519Private { .. } | KeyMaterial::X25519Public { .. } => Some("x25519"),
        }
    }

    pub fn symmetric_key_size(&self) -> Option<u32> {
        match &*self.material {
            KeyMaterial::Symmetric(b) => Some(b.len() as u32),
            _ => None,
        }
    }
}
```

### IV.3. `asymmetricKeyDetails` shape

Per https://nodejs.org/api/crypto.html#keyobjectasymmetrickeydetails — algorithm-specific:

| `asymmetricKeyType` | `asymmetricKeyDetails` shape |
|---|---|
| `"rsa"` | `{ modulusLength: number, publicExponent: bigint }` |
| `"rsa-pss"` | `{ modulusLength, publicExponent, hashAlgorithm, mgf1HashAlgorithm, saltLength }` |
| `"dsa"` | `{ modulusLength, divisorLength }` |
| `"ec"` | `{ namedCurve: "P-256" | "P-384" | "P-521" | "secp256k1" }` |
| `"dh"` | `{ generator, prime, primeLength: number }` |
| `"ed25519"` / `"x25519"` / `"ed448"` / `"x448"` | `{}` (empty object) |

The getter is `[SameObject]` cached (D-N3 macro `#[v8_getter(same_object)]`).

### IV.4. The `create*Key` factories (D-N14)

```rust
// crypto_node/key_object.rs

pub fn create_secret_key<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    input: v8::Local<v8::Value>,
    encoding: Option<&str>,
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    let bytes = buffer::extract_input(scope, input, encoding)?;
    if bytes.is_empty() {
        return Err(OpError::node("ERR_OUT_OF_RANGE",
            "The value of \"key\" is out of range. It must be > 0"));
    }
    let state = KeyObjectState {
        key_type: KeyType::Secret,
        material: Arc::new(KeyMaterial::Symmetric(bytes)),
    };
    Ok(SecretKeyObject::build(scope, state).into())
}

pub fn create_public_key<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    input: v8::Local<v8::Value>,
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    // Input is one of:
    //   - PEM string (`-----BEGIN PUBLIC KEY-----` / `RSA PUBLIC KEY`)
    //   - Buffer/Uint8Array (DER-encoded SPKI or PKCS1 RSAPublicKey)
    //   - JWK object
    //   - KeyObject (extracts the public part)
    //   - { key, format, type, passphrase } object
    let parsed = parse_public_key_input(scope, input)?;
    let state = KeyObjectState {
        key_type: KeyType::Public,
        material: Arc::new(parsed),
    };
    Ok(PublicKeyObject::build(scope, state).into())
}

pub fn create_private_key<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    input: v8::Local<v8::Value>,
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    // Same input variants as public.
    let parsed = parse_private_key_input(scope, input)?;
    let state = KeyObjectState {
        key_type: KeyType::Private,
        material: Arc::new(parsed),
    };
    Ok(PrivateKeyObject::build(scope, state).into())
}

fn parse_public_key_input(
    scope: &mut v8::PinScope,
    input: v8::Local<v8::Value>,
) -> Result<KeyMaterial, OpError> {
    // Dispatch on shape:
    if let Some(km) = is_existing_keyobject(scope, input) {
        // KeyObject input — extract its material (must be public).
        return Ok((*km).clone());
    }

    // String / Buffer / object route:
    let (key_data, format, key_type, passphrase) = unpack_options(scope, input)?;

    // Format detection:
    //   - format: 'pem' (default if input is string) → PEM decode
    //   - format: 'der' (default if input is Buffer) → DER as-is
    //   - format: 'jwk' → JWK object route
    //
    // type:
    //   - 'pkcs1' (RSA-only): PKCS#1 RSAPublicKey
    //   - 'spki' (default for public): SubjectPublicKeyInfo
    //   - 'sec1' (private-only): SEC1 EC private
    //   - 'pkcs8' (default for private): PrivateKeyInfo

    match format {
        Format::Pem => {
            let der = kernel::pem::decode(&key_data)?;
            // PEM label drives type resolution.
            match der.label.as_str() {
                "PUBLIC KEY" => parse_spki(&der.bytes),
                "RSA PUBLIC KEY" => parse_pkcs1_rsa_public(&der.bytes),
                _ => Err(OpError::node("ERR_OSSL_UNSUPPORTED",
                    format!("Unsupported PEM label: {}", der.label))),
            }
        }
        Format::Der => match key_type {
            KeyEncoding::Spki => parse_spki(&key_data),
            KeyEncoding::Pkcs1 => parse_pkcs1_rsa_public(&key_data),
            _ => Err(OpError::node("ERR_INVALID_ARG_VALUE",
                "Invalid type for public DER")),
        },
        Format::Jwk => parse_jwk_public(scope, key_data),
    }
}
```

The PEM decoder lives in the kernel (`crypto_kernel/pem.rs`):

```rust
// crypto_kernel/pem.rs

pub struct PemBlock {
    pub label: String,    // "PUBLIC KEY" / "RSA PRIVATE KEY" / etc.
    pub bytes: Vec<u8>,   // base64-decoded DER
    pub headers: Vec<(String, String)>,    // PROC-TYPE / DEK-Info for encrypted PEM
}

pub fn decode(text: &str) -> Result<PemBlock, KernelError> { /* RFC 7468 */ }
pub fn decode_all(text: &str) -> Vec<PemBlock> { /* multi-block support for X.509 chains */ }
pub fn encode(label: &str, bytes: &[u8]) -> String { /* emit RFC 7468 PEM */ }
```

PEM decoding is ~80 LOC of Rust (RFC 7468 is a tiny spec; `-----BEGIN <label>-----`, base64 body, `-----END <label>-----`). No dependency added.

For encrypted PKCS#8 (`{ passphrase: Buffer.from('hunter2') }`), the parser dispatches to aws-lc-rs's `EncryptedPrivateKeyInfo::from_bytes(der).decrypt(passphrase)` (returns plaintext PKCS#8), then re-runs the unencrypted parser.

### IV.5. `KeyObject.from(cryptoKey)` static (D-N4)

```rust
impl KeyObject {
    #[v8_static_method]
    pub fn from<'s>(
        scope: &mut v8::PinScope<'s, '_>,
        crypto_key: v8::Local<v8::Value>,
    ) -> Result<v8::Local<'s, v8::Value>, OpError> {
        // Verify it's a CryptoKey via the existing brand check.
        if !crate::crypto_native::crypto_key::is_crypto_key(scope, crypto_key) {
            return Err(OpError::node("ERR_INVALID_ARG_TYPE",
                "Argument must be a CryptoKey"));
        }
        let ck_state = crate::crypto_native::crypto_key::state(scope, crypto_key);
        let ko_state = KeyObjectState {
            key_type: ck_state.key_type,
            material: Arc::clone(&ck_state.material),
        };
        // Pick the right subclass based on key_type:
        let inst = match ko_state.key_type {
            KeyType::Public => PublicKeyObject::build(scope, ko_state),
            KeyType::Private => PrivateKeyObject::build(scope, ko_state),
            KeyType::Secret => SecretKeyObject::build(scope, ko_state),
        };
        Ok(inst.into())
    }
}
```

The reverse path (`subtle.importKey` from a KeyObject) goes through JWK:

```js
// In creator code:
const jwk = keyObject.export({ format: 'jwk' });
const cryptoKey = await crypto.subtle.importKey(
  'jwk', jwk,
  { name: 'RSASSA-PKCS1-v1_5', hash: 'SHA-256' },
  true, ['verify']);
```

The KeyObject.export-to-JWK + subtle.importKey-from-JWK round trip is spec-correct (JWK is the universal interchange format) but materialises a fresh Arc — the resulting CryptoKey doesn't share material with the source KeyObject. Acceptable; semantics match Node.

### IV.6. `keyObject.export(options)` (D-N19)

```rust
impl KeyObject {
    #[v8_method]
    fn export<'s>(&self,
        scope: &mut v8::PinScope<'s, '_>,
        this: v8::Local<'s, v8::Object>,
        options: Option<v8::Local<v8::Value>>,
    ) -> Result<v8::Local<'s, v8::Value>, OpError> {
        let state = self.state(scope, this);
        let opts = parse_export_options(scope, options, state.key_type)?;
        match opts {
            ExportOptions::SecretRaw => {
                // For SecretKeyObject: return the raw key bytes as Buffer.
                match &*state.material {
                    KeyMaterial::Symmetric(b) =>
                        Ok(buffer::emit_buffer(scope, b).into()),
                    _ => Err(OpError::node("ERR_INVALID_ARG_TYPE",
                        "Cannot export non-secret key as raw")),
                }
            }
            ExportOptions::Pem { type_, cipher, passphrase } => {
                let der = encode_to_der(state, type_)?;
                let der = if let (Some(c), Some(p)) = (cipher, passphrase) {
                    encrypt_pkcs8(&der, c, &p)?
                } else { der };
                let pem = kernel::pem::encode(label_for(type_), &der);
                Ok(emit_string(scope, &pem))
            }
            ExportOptions::Der { type_, cipher, passphrase } => {
                let der = encode_to_der(state, type_)?;
                let der = if let (Some(c), Some(p)) = (cipher, passphrase) {
                    encrypt_pkcs8(&der, c, &p)?
                } else { der };
                Ok(buffer::emit_buffer(scope, &der).into())
            }
            ExportOptions::Jwk => {
                // Reuse the WebCrypto JWK exporter.
                let jwk_obj = crate::crypto_kernel::jwk::export(scope, state)?;
                Ok(jwk_obj.into())
            }
        }
    }
}
```

The `encode_to_der(state, type_)` function dispatches on the key's variant + the requested type:

| Variant | Type request | Result |
|---|---|---|
| RSA private | `pkcs8` (default) | PKCS#8 PrivateKeyInfo |
| RSA private | `pkcs1` | RSAPrivateKey (RFC 8017 §A.1.2) |
| RSA public | `spki` (default) | SubjectPublicKeyInfo |
| RSA public | `pkcs1` | RSAPublicKey (RFC 8017 §A.1.1) |
| EC private | `pkcs8` (default) | PKCS#8 wrapping ECPrivateKey |
| EC private | `sec1` | SEC1 ECPrivateKey (RFC 5915) |
| EC public | `spki` (default) | SubjectPublicKeyInfo |
| Ed25519 / X25519 priv | `pkcs8` (only) | PKCS#8 |
| Ed25519 / X25519 pub | `spki` (only) | SPKI |

Most of these paths read `KeyMaterial`'s pre-stored `pkcs8_der` / `spki_der` fields and return them directly. PKCS#1 RSA encodings need a fresh DER walker call; SEC1 EC private needs the same. Both are ~30 LOC each in the kernel's DER emitter.

### IV.7. Zeroize on Drop (D-N30)

```rust
// crypto_kernel/key_material.rs

pub enum KeyMaterial {
    Symmetric(Zeroizing<Vec<u8>>),
    EcPrivate {
        pkcs8_der: Zeroizing<Vec<u8>>,
        raw_d: Zeroizing<Vec<u8>>,
        raw_xy: Vec<u8>,             // public; no zeroize
    },
    EcPublic {
        spki_der: Vec<u8>,
        raw_xy: Vec<u8>,
    },
    RsaPrivate {
        pkcs8_der: Zeroizing<Vec<u8>>,
        components: RsaPrivateComponents,    // see below
    },
    RsaPublic { /* no zeroize */ },
    Ed25519Private {
        pkcs8_der: Zeroizing<Vec<u8>>,
        raw_d: Zeroizing<[u8; 32]>,
        raw_x: [u8; 32],
    },
    /* ... */
}

pub struct RsaPrivateComponents {
    pub n: Vec<u8>,
    pub e: Vec<u8>,
    pub d: Zeroizing<Vec<u8>>,
    pub p: Zeroizing<Vec<u8>>,
    pub q: Zeroizing<Vec<u8>>,
    pub dp: Zeroizing<Vec<u8>>,
    pub dq: Zeroizing<Vec<u8>>,
    pub qi: Zeroizing<Vec<u8>>,
}
```

`zeroize::Zeroizing<Vec<u8>>` has `Drop` that calls `.zeroize()` (volatile-write zeros, compiler-fence). Already a transitive dep via aws-lc-rs.

The Arc<KeyMaterial> share means zeroize fires when the LAST reference drops — which happens when both the CryptoKey and KeyObject wrappers are GC'd. Browser-side WeakRef-tracking libraries that hold references will keep the bytes alive; that's the documented contract of holding a key handle.

## V. Streaming primitives

### V.1. The kernel `Context` shape (D-N2)

Every streaming Node API has the same JS-side pattern:

```js
const x = crypto.createX(...);    // factory
x.update(data);                    // 0..N times
x.update(more);
const out = x.final(...);          // 1 time
```

The kernel models this as a Context type with `new`, `update`, `finalize`. The five variants:

```rust
// crypto_kernel/digest.rs
pub struct DigestContext { /* aws_lc_rs::digest::Context + finalised flag */ }
impl DigestContext {
    pub fn new(hash: HashAlgo) -> Self;
    pub fn update(&mut self, data: &[u8]) -> Result<(), KernelError>;
    pub fn finalize(&mut self) -> Result<Vec<u8>, KernelError>;
    pub fn clone_state(&self) -> Self;
}

// crypto_kernel/hmac.rs
pub struct HmacContext { /* aws_lc_rs::hmac::Context + finalised flag */ }
impl HmacContext {
    pub fn new(hash: HashAlgo, key: &[u8]) -> Self;
    pub fn update(&mut self, data: &[u8]) -> Result<(), KernelError>;
    pub fn finalize(&mut self) -> Result<Vec<u8>, KernelError>;
}

// crypto_kernel/cipher.rs
pub struct CipherContext { /* state machine: pending block, AAD, tag, mode */ }
impl CipherContext {
    pub fn new_encrypt(alg: CipherAlg, key: &[u8], iv: &[u8]) -> Result<Self, KernelError>;
    pub fn new_decrypt(alg: CipherAlg, key: &[u8], iv: &[u8]) -> Result<Self, KernelError>;
    pub fn set_aad(&mut self, aad: &[u8]) -> Result<(), KernelError>;
    pub fn set_auth_tag(&mut self, tag: &[u8]) -> Result<(), KernelError>;    // Decipher only
    pub fn set_auto_padding(&mut self, on: bool) -> Result<(), KernelError>;  // CBC only
    pub fn update(&mut self, data: &[u8]) -> Result<Vec<u8>, KernelError>;
    pub fn finalize(&mut self) -> Result<Vec<u8>, KernelError>;
    pub fn auth_tag(&self) -> Option<&[u8]>;     // Cipher only, post-finalize
}

// crypto_kernel/sign_verify.rs
pub struct SignContext { /* digest context + private key + padding mode */ }
impl SignContext {
    pub fn new(hash: HashAlgo, key: &KeyMaterial, padding: SignPadding) -> Result<Self, KernelError>;
    pub fn update(&mut self, data: &[u8]) -> Result<(), KernelError>;
    pub fn sign(self) -> Result<Vec<u8>, KernelError>;
}

pub struct VerifyContext { /* digest context + public key + padding mode */ }
impl VerifyContext {
    pub fn new(hash: HashAlgo, key: &KeyMaterial, padding: SignPadding) -> Result<Self, KernelError>;
    pub fn update(&mut self, data: &[u8]) -> Result<(), KernelError>;
    pub fn verify(self, sig: &[u8]) -> Result<bool, KernelError>;
}
```

### V.2. Hash class (D-N9)

```rust
// crypto_node/hash.rs

pub struct HashState {
    ctx: kernel::DigestContext,
}

#[v8_class]
#[v8_to_string_tag = "Hash"]
impl Hash {
    /// `hash.update(data, inputEncoding?)` — incremental.
    /// Returns `this` for chaining.
    #[v8_method]
    fn update<'s>(&mut self,
        scope: &mut v8::PinScope<'s, '_>,
        this: v8::Local<'s, v8::Object>,
        data: v8::Local<v8::Value>,
        encoding: Option<String>,
    ) -> Result<v8::Local<'s, v8::Value>, OpError> {
        let bytes = buffer::extract_input(scope, data, encoding.as_deref())?;
        self.ctx.update(&bytes).map_err(KernelError::to_node)?;
        Ok(this.into())
    }

    /// `hash.digest(outputEncoding?)` — returns Buffer | string.
    #[v8_method]
    fn digest<'s>(&mut self,
        scope: &mut v8::PinScope<'s, '_>,
        encoding: Option<String>,
    ) -> Result<v8::Local<'s, v8::Value>, OpError> {
        let bytes = self.ctx.finalize().map_err(KernelError::to_node)?;
        buffer::emit_output(scope, &bytes, encoding.as_deref())
    }

    /// `hash.copy(options?)` — returns a fresh Hash with the same in-progress
    /// state. Must NOT be called after digest() (Node throws).
    #[v8_method]
    fn copy<'s>(&self,
        scope: &mut v8::PinScope<'s, '_>,
        _options: Option<v8::Local<v8::Value>>,
    ) -> Result<v8::Local<'s, v8::Value>, OpError> {
        // Note: clone_state() returns a context with the same finalised
        // flag; if the source was finalised, copy() effectively returns
        // a finalised hash that throws on next update().
        let cloned = HashState { ctx: self.ctx.clone_state() };
        Ok(Hash::build(scope, cloned).into())
    }
}

pub fn create_hash<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    algorithm: String,
    _options: Option<v8::Local<v8::Value>>,
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    let hash = match canonicalise_hash_name(&algorithm) {
        Some(h) => h,
        None => return Err(OpError::node("ERR_OSSL_EVP_UNSUPPORTED",
            format!("Unknown hash: {}", algorithm))),
    };
    let state = HashState { ctx: kernel::DigestContext::new(hash) };
    Ok(Hash::build(scope, state).into())
}
```

### V.3. Hmac class (D-N10)

Mirror of Hash, with `key` parameter passed to `kernel::HmacContext::new`. **NO `copy()` method** — Node doesn't expose it on Hmac (a quirk we preserve for compat).

```rust
pub struct HmacState {
    ctx: kernel::HmacContext,
}

#[v8_class]
#[v8_to_string_tag = "Hmac"]
impl Hmac {
    #[v8_method]
    fn update<'s>(&mut self,
        scope: &mut v8::PinScope<'s, '_>,
        this: v8::Local<'s, v8::Object>,
        data: v8::Local<v8::Value>,
        encoding: Option<String>,
    ) -> Result<v8::Local<'s, v8::Value>, OpError> { /* same as Hash */ }

    #[v8_method]
    fn digest<'s>(&mut self,
        scope: &mut v8::PinScope<'s, '_>,
        encoding: Option<String>,
    ) -> Result<v8::Local<'s, v8::Value>, OpError> { /* same as Hash */ }
}

pub fn create_hmac<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    algorithm: String,
    key: v8::Local<v8::Value>,
    _options: Option<v8::Local<v8::Value>>,
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    let hash = canonicalise_hash_name(&algorithm)
        .ok_or_else(|| OpError::node("ERR_OSSL_EVP_UNSUPPORTED",
            format!("Unknown hash: {}", algorithm)))?;

    // Key may be a KeyObject, Buffer, or string.
    let key_bytes = if is_key_object(scope, key) {
        let ko = KeyObject::state(scope, key);
        match &*ko.material {
            KeyMaterial::Symmetric(b) => b.clone(),
            _ => return Err(OpError::node("ERR_INVALID_ARG_TYPE",
                "Hmac key must be a SecretKeyObject")),
        }
    } else {
        buffer::extract_input(scope, key, None)?
    };

    let state = HmacState { ctx: kernel::HmacContext::new(hash, &key_bytes) };
    Ok(Hmac::build(scope, state).into())
}
```

### V.4. Cipher / Decipher classes (D-N11, D-N23)

Cipher and Decipher are nearly identical; we model them as a single `Cipher` impl that internally tracks an `encrypt: bool` flag, with `Decipher` being a thin alias class.

```rust
pub struct CipherState {
    ctx: kernel::CipherContext,
    is_encrypt: bool,
    auto_padding: bool,
}

#[v8_class]
#[v8_to_string_tag = "Cipher"]
impl Cipher {
    /// `cipher.update(data, inputEncoding?, outputEncoding?)`
    #[v8_method]
    fn update<'s>(&mut self,
        scope: &mut v8::PinScope<'s, '_>,
        data: v8::Local<v8::Value>,
        input_encoding: Option<String>,
        output_encoding: Option<String>,
    ) -> Result<v8::Local<'s, v8::Value>, OpError> {
        let bytes = buffer::extract_input(scope, data, input_encoding.as_deref())?;
        let out = self.ctx.update(&bytes).map_err(KernelError::to_node)?;
        buffer::emit_output(scope, &out, output_encoding.as_deref())
    }

    /// `cipher.final(outputEncoding?)`
    #[v8_method]
    #[v8_name = "final"]
    fn r#final<'s>(&mut self,
        scope: &mut v8::PinScope<'s, '_>,
        output_encoding: Option<String>,
    ) -> Result<v8::Local<'s, v8::Value>, OpError> {
        let out = self.ctx.finalize().map_err(KernelError::to_node)?;
        buffer::emit_output(scope, &out, output_encoding.as_deref())
    }

    /// `cipher.setAAD(buffer, options?)` — for GCM/CCM/OCB AEAD modes.
    #[v8_method]
    fn set_aad<'s>(&mut self,
        scope: &mut v8::PinScope<'s, '_>,
        this: v8::Local<'s, v8::Object>,
        aad: v8::Local<v8::Value>,
        _options: Option<v8::Local<v8::Value>>,
    ) -> Result<v8::Local<'s, v8::Value>, OpError> {
        let bytes = buffer::extract_input(scope, aad, None)?;
        self.ctx.set_aad(&bytes).map_err(KernelError::to_node)?;
        Ok(this.into())
    }

    /// `cipher.getAuthTag()` — Cipher only, post-final.
    #[v8_method]
    fn get_auth_tag<'s>(&self,
        scope: &mut v8::PinScope<'s, '_>,
    ) -> Result<v8::Local<'s, v8::Value>, OpError> {
        if !self.is_encrypt {
            return Err(OpError::node("ERR_CRYPTO_INVALID_STATE",
                "Cannot call getAuthTag on a Decipher"));
        }
        let tag = self.ctx.auth_tag()
            .ok_or_else(|| OpError::node("ERR_CRYPTO_INVALID_STATE",
                "getAuthTag called before final()"))?;
        Ok(buffer::emit_buffer(scope, tag).into())
    }

    /// `decipher.setAuthTag(tagBuffer)` — Decipher only, pre-final.
    #[v8_method]
    fn set_auth_tag<'s>(&mut self,
        scope: &mut v8::PinScope<'s, '_>,
        this: v8::Local<'s, v8::Object>,
        tag: v8::Local<v8::Value>,
    ) -> Result<v8::Local<'s, v8::Value>, OpError> {
        if self.is_encrypt {
            return Err(OpError::node("ERR_CRYPTO_INVALID_STATE",
                "Cannot call setAuthTag on a Cipher"));
        }
        let bytes = buffer::extract_input(scope, tag, None)?;
        self.ctx.set_auth_tag(&bytes).map_err(KernelError::to_node)?;
        Ok(this.into())
    }

    /// `cipher.setAutoPadding(boolean)` — for CBC mode PKCS#7 padding control.
    #[v8_method]
    fn set_auto_padding<'s>(&mut self,
        this: v8::Local<'s, v8::Object>,
        on: Option<bool>,
    ) -> Result<v8::Local<'s, v8::Value>, OpError> {
        let on = on.unwrap_or(true);    // Node default
        self.ctx.set_auto_padding(on).map_err(KernelError::to_node)?;
        self.auto_padding = on;
        Ok(this.into())
    }
}

#[v8_class]
#[v8_to_string_tag = "Decipher"]
impl Decipher {
    // Same methods as Cipher, modulo getAuthTag/setAuthTag direction.
    // Implementation reuses the Cipher impl; Decipher is a thin alias
    // class with the same boxed CipherState (different is_encrypt flag).
}

pub fn create_cipheriv<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    algorithm: String,
    key: v8::Local<v8::Value>,
    iv: v8::Local<v8::Value>,
    _options: Option<v8::Local<v8::Value>>,
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    let alg = canonicalise_cipher_name(&algorithm)
        .ok_or_else(|| OpError::node("ERR_OSSL_EVP_UNSUPPORTED",
            format!("Unknown cipher: {}", algorithm)))?;
    let key_bytes = extract_key_bytes(scope, key, alg.expected_key_len())?;
    let iv_bytes = if iv.is_null() {
        // ECB has no IV; null is permitted.
        vec![]
    } else {
        buffer::extract_input(scope, iv, None)?
    };
    let ctx = kernel::CipherContext::new_encrypt(alg, &key_bytes, &iv_bytes)
        .map_err(KernelError::to_node)?;
    let state = CipherState { ctx, is_encrypt: true, auto_padding: true };
    Ok(Cipher::build(scope, state).into())
}
```

**`createCipher` (deprecated) error path:**

```rust
pub fn create_cipher<'s>(
    _scope: &mut v8::PinScope<'s, '_>,
    _algorithm: v8::Local<v8::Value>,
    _password: v8::Local<v8::Value>,
    _options: Option<v8::Local<v8::Value>>,
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    Err(OpError::node("ERR_CRYPTO_DEPRECATED_API",
        "crypto.createCipher is deprecated; use crypto.createCipheriv with an explicit IV. \
         See https://nodejs.org/api/crypto.html#cryptocreatecipheralgorithm-password-options."))
}
```

### V.5. Sign / Verify classes (D-N12)

```rust
pub struct SignState {
    digest: kernel::DigestContext,
    hash: HashAlgo,
}

#[v8_class]
#[v8_to_string_tag = "Sign"]
impl Sign {
    #[v8_method]
    fn update<'s>(&mut self,
        scope: &mut v8::PinScope<'s, '_>,
        this: v8::Local<'s, v8::Object>,
        data: v8::Local<v8::Value>,
        encoding: Option<String>,
    ) -> Result<v8::Local<'s, v8::Value>, OpError> {
        let bytes = buffer::extract_input(scope, data, encoding.as_deref())?;
        self.digest.update(&bytes).map_err(KernelError::to_node)?;
        Ok(this.into())
    }

    /// `sign.sign(privateKey, outputEncoding?) -> Buffer | string`
    /// privateKey can be: KeyObject, CryptoKey, PEM string, DER Buffer,
    /// or `{ key, format, type, padding, saltLength, dsaEncoding }` options.
    #[v8_method]
    fn sign<'s>(&mut self,
        scope: &mut v8::PinScope<'s, '_>,
        private_key: v8::Local<v8::Value>,
        encoding: Option<String>,
    ) -> Result<v8::Local<'s, v8::Value>, OpError> {
        let (km, padding) = parse_sign_key_input(scope, private_key)?;
        let digest = self.digest.finalize().map_err(KernelError::to_node)?;
        let sig = kernel::sign_verify::sign_with_digest(&km, self.hash, padding, &digest)
            .map_err(KernelError::to_node)?;
        buffer::emit_output(scope, &sig, encoding.as_deref())
    }
}

pub struct VerifyState {
    digest: kernel::DigestContext,
    hash: HashAlgo,
}

#[v8_class]
#[v8_to_string_tag = "Verify"]
impl Verify {
    #[v8_method]
    fn update<'s>(...) -> Result<...> { /* same as Sign */ }

    /// `verify.verify(publicKey, signature, signatureEncoding?) -> boolean`
    #[v8_method]
    fn verify<'s>(&mut self,
        scope: &mut v8::PinScope<'s, '_>,
        public_key: v8::Local<v8::Value>,
        signature: v8::Local<v8::Value>,
        encoding: Option<String>,
    ) -> Result<bool, OpError> {
        let (km, padding) = parse_verify_key_input(scope, public_key)?;
        let sig_bytes = buffer::extract_input(scope, signature, encoding.as_deref())?;
        let digest = self.digest.finalize().map_err(KernelError::to_node)?;
        kernel::sign_verify::verify_with_digest(&km, self.hash, padding, &digest, &sig_bytes)
            .map_err(KernelError::to_node)
    }
}
```

The `parse_sign_key_input` helper handles every input shape:

```rust
fn parse_sign_key_input(
    scope: &mut v8::PinScope,
    input: v8::Local<v8::Value>,
) -> Result<(Arc<KeyMaterial>, SignPadding), OpError> {
    // 1. KeyObject → extract material directly.
    if is_key_object(scope, input) {
        let ko = KeyObject::state(scope, input);
        return Ok((Arc::clone(&ko.material), SignPadding::Default));
    }
    // 2. CryptoKey → bridge via Arc.
    if crypto_native::crypto_key::is_crypto_key(scope, input) {
        let ck = crypto_native::crypto_key::state(scope, input);
        return Ok((Arc::clone(&ck.material), SignPadding::Default));
    }
    // 3. PEM string or Buffer → parse via createPrivateKey logic.
    // 4. Object `{ key, format, type, padding, saltLength, dsaEncoding }`
    //    → extract key + padding params.
    let opts = parse_options_object(scope, input)?;
    let km = parse_private_key_input(scope, opts.key)?;
    let padding = match opts.padding {
        Some(RSA_PKCS1_PSS_PADDING) =>
            SignPadding::RsaPss { salt_length: opts.salt_length.unwrap_or_else(|| {
                // Node default: equal to digest length
                hash_digest_len(self.hash) as u32
            })},
        Some(RSA_PKCS1_PADDING) | None => SignPadding::Default,
        Some(other) => return Err(OpError::node("ERR_INVALID_ARG_VALUE",
            format!("Unknown padding constant: {}", other))),
    };
    Ok((Arc::new(km), padding))
}
```

**`dsaEncoding`:** Node has a `dsaEncoding: 'der' | 'ieee-p1363'` option for ECDSA signatures. Default is `'der'` (ASN.1 INTEGER pair) for node:crypto sign/verify (matches OpenSSL output). `'ieee-p1363'` produces fixed-length r||s (matches WebCrypto). The kernel supports both via a flag on `SignPadding::Ecdsa { encoding }`.

**This is a key cross-surface coordination point:** WebCrypto (existing `crypto_native/`) emits IEEE-P1363 (D-4); node:crypto defaults to DER (Node convention). The kernel function `sign_with_digest` takes the encoding flag explicitly; both surfaces pass their preferred default.

## VI. Sync vs async dispatch policy (D-N5, D-N6)

### VI.1. The decision matrix

| API | Always sync | Sync below threshold / async above | Always async | Notes |
|---|---|---|---|---|
| `createHash` / Hash.update / Hash.digest | ✓ | | | Cheap (microseconds even on MB inputs) |
| `createHmac` / Hmac.update / Hmac.digest | ✓ | | | Cheap |
| `createCipheriv` / Cipher.update / .final | | ✓ (64 KB threshold) | | AES-GCM at 64 KB ≈ 50 µs; above pins V8 thread visibly |
| `createSign` / `createVerify` | ✓ | | | Single asymmetric op at finalise; cheap |
| `randomBytes(N)` (no callback) | ✓ | | | Sync, regardless of size |
| `randomBytes(N, callback)` | | | ✓ | Always async via `spawned_ops` |
| `randomFillSync` | ✓ | | | Sync |
| `randomFill` (callback) | | | ✓ | Always async |
| `randomInt` (no callback) | ✓ | | | Sync |
| `randomInt(callback)` | | | ✓ | Async via spawned_ops |
| `pbkdf2Sync` | ✓ | | | User picked the Sync API; honour |
| `pbkdf2` (callback or Promise) | | | ✓ | High iteration counts dominate; always offload |
| `scryptSync` | ✓ | | | Same |
| `scrypt` | | | ✓ | Same |
| `hkdfSync` | ✓ | | | Cheap; sync OK |
| `hkdf` (callback) | | | ✓ | Honour the user's choice |
| `generateKeyPairSync` | ✓ | | | RSA 4096 takes ~1 s; user opted in |
| `generateKeyPair` (callback) | | | ✓ | Always async |
| `timingSafeEqual` | ✓ | | | Microseconds |
| `webcrypto.subtle.*` | ✓ | | | Existing WebCrypto policy (D-29 of webcrypto-native) |

### VI.2. Async dispatch implementation

For each callback API, the surface emits TWO ops:

```rust
// Rust side, crypto_node/kdf.rs
pub fn pbkdf2_sync<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    password: v8::Local<v8::Value>,
    salt: v8::Local<v8::Value>,
    iterations: u32,
    keylen: u32,
    digest: String,
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    let pw_bytes = buffer::extract_input(scope, password, None)?;
    let salt_bytes = buffer::extract_input(scope, salt, None)?;
    let hash = canonicalise_hash_name(&digest)
        .ok_or_else(|| OpError::node("ERR_OSSL_EVP_UNSUPPORTED",
            format!("Unknown digest: {}", digest)))?;
    let mut out = vec![0u8; keylen as usize];
    kernel::kdf::pbkdf2(hash, iterations, &pw_bytes, &salt_bytes, &mut out)
        .map_err(KernelError::to_node)?;
    Ok(buffer::emit_buffer(scope, &out).into())
}

pub async fn pbkdf2_async(
    /* same arg shape, but receives the Vec<u8>s extracted on the V8 thread
       BEFORE the await, then dispatches the kernel call to the blocking pool */
    pw_bytes: Vec<u8>,
    salt_bytes: Vec<u8>,
    iterations: u32,
    keylen: u32,
    hash: HashAlgo,
) -> Result<Vec<u8>, OpError> {
    // The macro's #[v8_async_method] codegen wraps this in a future that
    // pushes onto state.spawned_ops; the body runs on the compio
    // blocking-task pool.
    compio::runtime::spawn_blocking(move || {
        let mut out = vec![0u8; keylen as usize];
        kernel::kdf::pbkdf2(hash, iterations, &pw_bytes, &salt_bytes, &mut out)
            .map(|()| out)
            .map_err(KernelError::to_node)
    }).await
      .map_err(|e| OpError::node("ERR_CRYPTO_OPERATION_FAILED", format!("{e:?}")))?
}
```

The TS shim in the synthetic module routes:

```ts
function pbkdf2(password, salt, iterations, keylen, digest, callback) {
  if (typeof callback !== 'function') {
    throw new TypeError("pbkdf2 callback is required");
  }
  __zeroship_node_crypto.pbkdf2Async(password, salt, iterations, keylen, digest)
    .then(buf => callback(null, buf), err => callback(err));
}
function pbkdf2Sync(password, salt, iterations, keylen, digest) {
  return __zeroship_node_crypto.pbkdf2Sync(password, salt, iterations, keylen, digest);
}
```

### VI.3. Threshold rules for Cipher.update (D-N6)

```rust
impl Cipher {
    #[v8_method]
    fn update<'s>(&mut self,
        scope: &mut v8::PinScope<'s, '_>,
        data: v8::Local<v8::Value>,
        input_encoding: Option<String>,
        output_encoding: Option<String>,
    ) -> Result<v8::Local<'s, v8::Value>, OpError> {
        let bytes = buffer::extract_input(scope, data, input_encoding.as_deref())?;
        // Sync path: under threshold, run on V8 thread.
        // We do NOT pump above-threshold work to the spawned pool from Cipher.update
        // because Node's Cipher.update is documented sync-returning (returns Buffer
        // synchronously). Adding an async spawn would change the API shape.
        //
        // Instead: the threshold is a documentation note ("for very large inputs,
        // chunk via TransformStream + per-chunk update") not a runtime gate.
        let out = self.ctx.update(&bytes).map_err(KernelError::to_node)?;
        buffer::emit_output(scope, &out, output_encoding.as_deref())
    }
}
```

**Revised D-N6:** drop the runtime-gate idea. Cipher.update is documented sync; we keep it sync. Document that creator apps doing bulk encryption should chunk via TransformStream + per-chunk crypto, NOT call `cipher.update(ten_megabytes_of_data)` synchronously. This matches Node exactly.

(The threshold concept stays only as a documentation note. The implementation runs sync always.)

### VI.4. Cipher.update zero-copy possibility

Cipher.update produces output bytes equal to input bytes (modulo block padding for the final()). The kernel's `CipherContext::update` could return a `Vec<u8>`; we copy out to a Buffer at the V8 boundary (one allocation). For very large inputs this is two memory allocations + one copy. The cost is negligible vs the cipher itself. Document; defer optimisation.

### VI.5. The `randomBytes` async path (D-N17)

```rust
pub fn random_bytes_sync<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    size: u32,
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    if size > 2147483647 {
        return Err(OpError::node("ERR_OUT_OF_RANGE",
            "size must be ≤ 2^31-1"));
    }
    let mut out = vec![0u8; size as usize];
    crate::crypto::fast_random(&mut out);
    Ok(buffer::emit_buffer(scope, &out).into())
}

pub async fn random_bytes_async(size: u32) -> Result<Vec<u8>, OpError> {
    if size > 2147483647 {
        return Err(OpError::node("ERR_OUT_OF_RANGE", "size must be ≤ 2^31-1"));
    }
    compio::runtime::spawn_blocking(move || {
        let mut out = vec![0u8; size as usize];
        crate::crypto::fast_random(&mut out);
        Ok::<_, OpError>(out)
    }).await.map_err(|e| OpError::node("ERR_CRYPTO_OPERATION_FAILED", format!("{e:?}")))?
}

// randomInt: rejection sampling.
pub fn random_int_sync(min: i64, max: i64) -> Result<i64, OpError> {
    if max <= min {
        return Err(OpError::node("ERR_OUT_OF_RANGE",
            "max must be greater than min"));
    }
    if max - min > 2_i64.pow(48) {
        return Err(OpError::node("ERR_OUT_OF_RANGE",
            "max - min must be ≤ 2^48"));
    }
    let range = (max - min) as u64;
    // Find next power-of-2 >= range, sample bits, reject if >= range.
    let bits = 64 - range.leading_zeros();
    let mask = if bits == 64 { u64::MAX } else { (1u64 << bits) - 1 };
    loop {
        let mut buf = [0u8; 8];
        crate::crypto::fast_random(&mut buf);
        let val = u64::from_le_bytes(buf) & mask;
        if val < range {
            return Ok(min + val as i64);
        }
    }
}
```

The rejection-sampling approach **fixes a subtle bias** in the JS shim's randomInt (`node-compat.ts:106-117`): the shim's `Math.floor(0x100000000 / range) * range` only handles ranges ≤ 2^32, and for non-divisor ranges the floor introduces small bias on the boundary modular term. Our rejection sampling has no bias.

## VII. Error mapping (D-N8, D-N32)

### VII.1. The dual-error scheme

Errors flow through three layers:

```
    aws-lc-rs returns Result<(), Unspecified>
            │
            ▼
    KernelError (kernel/error.rs) — semantic, surface-agnostic
            │
        ┌───┴───┐
        ▼       ▼
   to_node   to_webcrypto
   (NodeError) (DomException)
            │       │
            ▼       ▼
    OpErrorKind variants
            │
            ▼
    macro gen_throw_error → V8 Error / TypeError / RangeError / DOMException
```

### VII.2. KernelError enum

```rust
// crypto_kernel/error.rs
#[derive(Debug)]
pub enum KernelError {
    // Hash / Hmac
    HashFinalised,
    HmacFinalised,

    // Cipher / Decipher
    InvalidKeyLength { algorithm: &'static str, expected: &'static [usize], got: usize },
    InvalidIvLength { algorithm: &'static str, expected: &'static [usize], got: usize },
    InvalidTagLength { expected: &'static [usize], got: usize },
    AuthenticationFailed,
    AadAfterUpdate,    // Tried to setAAD after update() was called
    SetAadOnNonAead,
    SetAuthTagOnEncrypt,
    GetAuthTagBeforeFinal,
    InvalidPadding,
    InputNotMultipleOfBlockSize,    // CBC without setAutoPadding(false)

    // Sign / Verify
    SignFailed,
    VerifyFailed,
    KeyTypeMismatchForAlgorithm,    // Tried to RSA-sign with EC key

    // Key import
    InvalidPem(String),
    InvalidDer(String),
    InvalidJwk(&'static str),
    InvalidKeyType,
    PassphraseRequired,
    PassphraseMismatch,
    UnsupportedKeyAlgorithm(String),

    // KDF
    PbkdfIterationsZero,
    PbkdfDigestUnknown(String),
    HkdfOutputTooLarge { max: usize, got: usize },
    ScryptParametersInvalid { reason: &'static str },
    ScryptMemoryExceeded { max: usize, would_use: usize },

    // DH / ECDH
    DhCurveMismatch,
    DhPublicKeyInvalid,
    DhUnknownNamedGroup(String),
    DhPrimeRejected { reason: &'static str },

    // Algorithm dispatch
    UnsupportedAlgorithm { name: String, op: &'static str },
    UnsupportedOperation(String),

    // Generic
    InternalError(String),
}
```

### VII.3. Mapping to Node error codes

```rust
// crypto_node/error.rs

impl KernelError {
    pub fn to_node(self) -> OpError {
        match self {
            Self::HashFinalised => OpError::node("ERR_CRYPTO_HASH_FINALIZED",
                "Digest already called"),
            Self::HmacFinalised => OpError::node("ERR_CRYPTO_HASH_FINALIZED",    // Node uses same code
                "Digest already called"),
            Self::InvalidKeyLength { algorithm, expected, got } =>
                OpError::node("ERR_CRYPTO_INVALID_KEYLEN",
                    format!("Invalid {} key length: got {}, expected one of {:?}",
                        algorithm, got, expected)),
            Self::InvalidIvLength { algorithm, expected, got } =>
                OpError::node("ERR_CRYPTO_INVALID_IV",
                    format!("Invalid IV length for {}: got {}, expected one of {:?}",
                        algorithm, got, expected)),
            Self::InvalidTagLength { expected, got } =>
                OpError::node("ERR_CRYPTO_INVALID_AUTH_TAG",
                    format!("Invalid auth tag length: got {}, expected one of {:?}",
                        got, expected)),
            Self::AuthenticationFailed => OpError::node("ERR_OSSL_BAD_DECRYPT",
                "Unsupported state or unable to authenticate data"),
            Self::AadAfterUpdate => OpError::node("ERR_CRYPTO_INVALID_STATE",
                "setAAD must be called before update"),
            Self::SetAadOnNonAead => OpError::node("ERR_CRYPTO_INVALID_STATE",
                "setAAD only valid for authenticated cipher modes"),
            Self::SetAuthTagOnEncrypt => OpError::node("ERR_CRYPTO_INVALID_STATE",
                "setAuthTag is only valid on a Decipher"),
            Self::GetAuthTagBeforeFinal => OpError::node("ERR_CRYPTO_INVALID_STATE",
                "getAuthTag must be called after final()"),
            Self::InvalidPadding => OpError::node("ERR_OSSL_BAD_DECRYPT",
                "bad decrypt"),
            Self::InputNotMultipleOfBlockSize => OpError::node("ERR_CRYPTO_INVALID_LENGTH",
                "Input data must be a multiple of the cipher block size"),

            Self::SignFailed => OpError::node("ERR_OSSL_EVP_SIGN", "sign failed"),
            Self::VerifyFailed => OpError::node("ERR_OSSL_EVP_VERIFY", "verify failed"),
            Self::KeyTypeMismatchForAlgorithm =>
                OpError::node("ERR_CRYPTO_INCOMPATIBLE_KEY",
                    "Incompatible key for this signing algorithm"),

            Self::InvalidPem(msg) => OpError::node("ERR_OSSL_PEM_NO_START_LINE",
                format!("PEM_read_bio: no start line: {}", msg)),
            Self::InvalidDer(msg) => OpError::node("ERR_OSSL_ASN1_VALUE_ERROR",
                format!("DER decode failed: {}", msg)),
            Self::InvalidJwk(reason) => OpError::node("ERR_CRYPTO_INVALID_JWK",
                format!("Invalid JWK: {}", reason)),
            Self::InvalidKeyType => OpError::node("ERR_CRYPTO_INVALID_KEY_OBJECT_TYPE",
                "Invalid key object type"),
            Self::PassphraseRequired => OpError::node("ERR_MISSING_PASSPHRASE",
                "Passphrase required to decrypt private key"),
            Self::PassphraseMismatch => OpError::node("ERR_OSSL_EVP_BAD_DECRYPT",
                "bad decrypt — passphrase incorrect"),
            Self::UnsupportedKeyAlgorithm(name) =>
                OpError::node("ERR_OSSL_EVP_UNSUPPORTED_ALGORITHM",
                    format!("Unsupported key algorithm: {}", name)),

            Self::PbkdfIterationsZero => OpError::node("ERR_OUT_OF_RANGE",
                "iterations must be > 0"),
            Self::PbkdfDigestUnknown(name) => OpError::node("ERR_OSSL_EVP_UNSUPPORTED",
                format!("Unsupported pbkdf2 digest: {}", name)),
            Self::HkdfOutputTooLarge { max, got } =>
                OpError::node("ERR_OUT_OF_RANGE",
                    format!("HKDF output length {} exceeds max {}", got, max)),
            Self::ScryptParametersInvalid { reason } =>
                OpError::node("ERR_CRYPTO_INVALID_SCRYPT_PARAMS",
                    format!("Invalid scrypt parameters: {}", reason)),
            Self::ScryptMemoryExceeded { max, would_use } =>
                OpError::node("ERR_CRYPTO_SCRYPT_NOT_SUPPORTED",
                    format!("scrypt requires {} bytes, max is {}", would_use, max)),

            Self::DhCurveMismatch => OpError::node("ERR_CRYPTO_ECDH_INVALID_PUBLIC_KEY",
                "Public key curve mismatch"),
            Self::DhPublicKeyInvalid => OpError::node("ERR_CRYPTO_ECDH_INVALID_PUBLIC_KEY",
                "Invalid public key for ECDH"),
            Self::DhUnknownNamedGroup(name) =>
                OpError::node("ERR_CRYPTO_UNKNOWN_DH_GROUP",
                    format!("Unknown DH group: {}", name)),
            Self::DhPrimeRejected { reason } =>
                OpError::node("ERR_CRYPTO_INVALID_DH_PRIME",
                    format!("Rejected DH prime: {}", reason)),

            Self::UnsupportedAlgorithm { name, op } =>
                OpError::node("ERR_OSSL_EVP_UNSUPPORTED_ALGORITHM",
                    format!("Unsupported {} for op {}", name, op)),
            Self::UnsupportedOperation(msg) =>
                OpError::node("ERR_CRYPTO_UNSUPPORTED_OPERATION", msg),

            Self::InternalError(msg) => OpError::node("ERR_CRYPTO_OPERATION_FAILED",
                format!("Internal error: {}", msg)),
        }
    }
}
```

### VII.4. Mapping to WebCrypto DOMExceptions

The existing `crypto_native/` paths already throw DOMException via `OpError::dom(name, msg)`. The kernel extraction means each `crypto_native/` site now does:

```rust
// Before refactor:
fn aes_gcm_encrypt_raw(/* ... */) -> Result<Vec<u8>, OpError> {
    // (uses aws-lc-rs directly; emits OpError::dom(...) on errors)
}

// After kernel extraction:
fn aes_gcm_encrypt(/* ... */) -> Result<Vec<u8>, OpError> {
    crate::crypto_kernel::cipher::aes_gcm_encrypt(/* ... */)
        .map_err(KernelError::to_webcrypto)
}
```

The `to_webcrypto` impl mirrors `to_node` but maps to DOMException variants:

```rust
// crypto_native/error.rs (refactored — moves from existing scattered call sites)
impl KernelError {
    pub fn to_webcrypto(self) -> OpError {
        match self {
            Self::AuthenticationFailed => OpError::dom("OperationError",
                "Authentication failed"),
            Self::InvalidKeyLength { .. } => OpError::dom("DataError",
                "Invalid key length"),
            Self::InvalidIvLength { .. } | Self::InvalidTagLength { .. }
                => OpError::dom("OperationError", "Invalid parameters"),
            Self::HashFinalised | Self::HmacFinalised => OpError::dom(
                "InvalidStateError", "Already finalised"),
            Self::InvalidPem(_) | Self::InvalidDer(_) | Self::InvalidJwk(_) =>
                OpError::dom("DataError", "Invalid key data"),
            Self::SignFailed | Self::VerifyFailed => OpError::dom("OperationError",
                "Sign/verify failed"),
            Self::KeyTypeMismatchForAlgorithm => OpError::dom("InvalidAccessError",
                "Key type does not match algorithm"),
            Self::PbkdfIterationsZero | Self::HkdfOutputTooLarge { .. } =>
                OpError::dom("OperationError", "KDF parameters invalid"),
            Self::DhCurveMismatch | Self::DhPublicKeyInvalid =>
                OpError::dom("InvalidAccessError", "Invalid public key"),
            Self::UnsupportedAlgorithm { .. } | Self::UnsupportedKeyAlgorithm(_) =>
                OpError::dom("NotSupportedError", "Unsupported algorithm"),
            Self::UnsupportedOperation(_) =>
                OpError::dom("NotSupportedError", "Unsupported operation"),
            Self::InternalError(msg) => OpError::dom("OperationError",
                format!("Internal: {}", msg)),
            // Catch-all: errors that aren't observable from the WebCrypto
            // surface (e.g. setAAD-on-non-aead — only Cipher exposes setAAD).
            other => OpError::dom("OperationError", format!("{:?}", other)),
        }
    }
}
```

### VII.5. The `OpErrorKind::NodeError` macro extension (D-N32)

```rust
// crates/runtime/src/state.rs (existing file, ADD variant)
pub enum OpErrorKind {
    TypeError,
    RangeError,
    Error,
    DomException(&'static str),    // existing (from D-6 of webcrypto-native)
    NodeError(&'static str),       // NEW (D-N32): the e.code value
}

impl OpError {
    pub fn node(code: &'static str, msg: impl Into<String>) -> Self {
        Self {
            kind: OpErrorKind::NodeError(code),
            message: msg.into(),
        }
    }
}
```

Macro arm in `runtime-macros/src/lib.rs::gen_throw_error`:

```rust
::zeroship_runtime::state::OpErrorKind::NodeError(code) => {
    let __msg = v8::String::new(scope, &__err.message).unwrap();
    let class = match code {
        "ERR_OUT_OF_RANGE" | "ERR_BUFFER_OUT_OF_BOUNDS" =>
            v8::Exception::range_error(scope, __msg),
        "ERR_INVALID_ARG_TYPE" | "ERR_INVALID_ARG_VALUE" |
        "ERR_INVALID_BUFFER_SIZE" | "ERR_INVALID_RETURN_VALUE" =>
            v8::Exception::type_error(scope, __msg),
        _ => v8::Exception::error(scope, __msg),
    };
    // Set .code property on the error instance.
    let exc_obj: v8::Local<v8::Object> = class.try_into().unwrap();
    let code_key = v8::String::new(scope, "code").unwrap();
    let code_val = v8::String::new(scope, code).unwrap();
    exc_obj.set(scope, code_key.into(), code_val.into());
    class
}
```

Total macro extension: ~25 LOC. The dispatch table at the top of `gen_throw_error` is the only routing logic.

## VIII. WebCrypto bridge (D-N16)

### VIII.1. Object identity invariant

```js
import { webcrypto, subtle, getRandomValues } from "node:crypto";

webcrypto === globalThis.crypto;                  // true
subtle === globalThis.crypto.subtle;              // true
webcrypto.subtle === subtle;                       // true
webcrypto.randomUUID === globalThis.crypto.randomUUID;  // function ref equality
```

Why this matters:

- JOSE libraries (panva/jose) keep `WeakMap<CryptoKey, ...>` caches for key parameters. The cache only works if the same CryptoKey instance is observed across both surfaces.
- npm packages probe for capabilities via `crypto.webcrypto?.subtle` (older code) or `globalThis.crypto?.subtle` (newer code) interchangeably — they assume `===` equivalence.

### VIII.2. Implementation

The synthetic module's installer reads `globalThis.crypto` ONCE at evaluate time:

```js
// Generated from crypto_node/module.rs::CRYPTO_MODULE_TS
const _g = globalThis;

// WebCrypto bridge — direct property references, NOT copies.
export const webcrypto = _g.crypto;
export const subtle = _g.crypto.subtle;
export const getRandomValues = _g.crypto.getRandomValues.bind(_g.crypto);

// node:crypto's `verify` and `sign` one-shot are NOT the WebCrypto subtle.{sign,verify}.
// They have different signatures (sync, take key as Buffer/PEM, etc.). Do NOT alias.
export function verify(algorithm, data, key, signature, callback) {
    return __zeroship_node_crypto.verify(algorithm, data, key, signature, callback);
}
export function sign(algorithm, data, key, callback) {
    return __zeroship_node_crypto.sign(algorithm, data, key, callback);
}
```

The Rust-side `__zeroship_node_crypto` global is installed during `setup_globals` BEFORE the synthetic module evaluates (the install order is: 1. `crypto_native::install_globals` (installs Crypto / SubtleCrypto / CryptoKey), 2. `crypto_node::install_globals` (installs `__zeroship_node_crypto.{Hash, Hmac, ...}`), 3. user modules import `node:crypto`).

### VIII.3. CryptoKey ↔ KeyObject interop in practice

```js
// User code:
import { webcrypto, KeyObject, createPublicKey } from "node:crypto";

// 1. WebCrypto-issued key.
const cryptoKey = await webcrypto.subtle.generateKey(
    { name: "RSASSA-PKCS1-v1_5", modulusLength: 2048,
      publicExponent: new Uint8Array([1, 0, 1]), hash: "SHA-256" },
    true, ["sign", "verify"]);

// 2. Bridge to node:crypto KeyObject.
const keyObject = KeyObject.from(cryptoKey.privateKey);
console.log(keyObject.type);              // "private"
console.log(keyObject.asymmetricKeyType); // "rsa"

// 3. Use it with node:crypto Sign.
const sig = crypto.createSign("SHA256")
    .update("hello")
    .sign(keyObject);

// 4. Round-trip back to CryptoKey via JWK.
const jwk = keyObject.export({ format: "jwk" });
const cryptoKey2 = await webcrypto.subtle.importKey(
    "jwk", jwk,
    { name: "RSASSA-PKCS1-v1_5", hash: "SHA-256" },
    true, ["sign"]);
```

The `KeyObject.from(cryptoKey)` path shares the `Arc<KeyMaterial>` (no key bytes copied; just a refcount bump). The JWK round-trip materialises a fresh Arc (slower; but spec-correct because the WebCrypto algorithm + extractable + usages have no node:crypto equivalent).

## IX. Algorithm registry (D-N18, D-N28)

### IX.1. Canonical-name table

Names in node:crypto are case-insensitive and inconsistent (Node accepts both `sha256` and `SHA-256` and `SHA256`). The kernel uses spec-canonical names; the surface adapter translates inputs.

```rust
// crypto_kernel/algorithms.rs

pub static HASH_NAMES: phf::Map<&'static str, HashAlgo> = phf::phf_map! {
    "sha1" => HashAlgo::Sha1,
    "sha-1" => HashAlgo::Sha1,
    "rsa-sha1" => HashAlgo::Sha1,    // legacy OpenSSL alias used in createSign
    "sha224" => HashAlgo::Sha224,
    "sha-224" => HashAlgo::Sha224,
    "sha256" => HashAlgo::Sha256,
    "sha-256" => HashAlgo::Sha256,
    "rsa-sha256" => HashAlgo::Sha256,
    "sha384" => HashAlgo::Sha384,
    "sha-384" => HashAlgo::Sha384,
    "rsa-sha384" => HashAlgo::Sha384,
    "sha512" => HashAlgo::Sha512,
    "sha-512" => HashAlgo::Sha512,
    "rsa-sha512" => HashAlgo::Sha512,
    "sha512-224" => HashAlgo::Sha512_224,
    "sha512-256" => HashAlgo::Sha512_256,
    "md5" => HashAlgo::Md5,
    "ripemd160" => HashAlgo::Ripemd160,    // unsupported, but listed for getHashes()
    "blake2b512" => HashAlgo::Blake2b512,
    "blake2s256" => HashAlgo::Blake2s256,
};

pub static CIPHER_NAMES: phf::Map<&'static str, CipherAlg> = phf::phf_map! {
    "aes-128-cbc" => CipherAlg::Aes128Cbc,
    "aes-192-cbc" => CipherAlg::Aes192Cbc,
    "aes-256-cbc" => CipherAlg::Aes256Cbc,
    "aes-128-ctr" => CipherAlg::Aes128Ctr,
    "aes-192-ctr" => CipherAlg::Aes192Ctr,
    "aes-256-ctr" => CipherAlg::Aes256Ctr,
    "aes-128-gcm" => CipherAlg::Aes128Gcm,
    "aes-192-gcm" => CipherAlg::Aes192Gcm,
    "aes-256-gcm" => CipherAlg::Aes256Gcm,
    "aes-128-ocb" => CipherAlg::Aes128Ocb,
    "aes-192-ocb" => CipherAlg::Aes192Ocb,
    "aes-256-ocb" => CipherAlg::Aes256Ocb,
    "aes-128-wrap" => CipherAlg::Aes128Kw,
    "aes-192-wrap" => CipherAlg::Aes192Kw,
    "aes-256-wrap" => CipherAlg::Aes256Kw,
    "chacha20-poly1305" => CipherAlg::ChaCha20Poly1305,
    "chacha20" => CipherAlg::ChaCha20,    // bare ChaCha20 stream (no AEAD)
    // Stage 2 (legacy ciphers, gated on --legacy-crypto):
    "des-cbc" => CipherAlg::DesCbc,
    "des-ede3" => CipherAlg::Tdes,
    "des-ede3-cbc" => CipherAlg::TdesCbc,
    "bf-cbc" => CipherAlg::BlowfishCbc,
    "rc4" => CipherAlg::Rc4,
    "rc4-40" => CipherAlg::Rc4_40,
};
```

### IX.2. AlgorithmEntry metadata

```rust
pub struct CipherEntry {
    pub alg: CipherAlg,
    pub key_lengths: &'static [usize],    // bytes
    pub iv_length: Option<usize>,         // None = no IV (ECB, KW)
    pub block_size: usize,
    pub mode: CipherMode,
    pub tag_lengths: Option<&'static [usize]>,    // Some for AEAD modes
    pub aliases: &'static [&'static str],         // for getCiphers() listing
}

pub enum CipherMode { Cbc, Ctr, Gcm, Ocb, Kw, Stream, Cfb, Ofb, Ecb }
```

### IX.3. `getCiphers` / `getHashes` / `getCurves` (D-N29)

```rust
// crypto_node/misc.rs

pub fn get_ciphers() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = CIPHER_NAMES.keys().copied().collect();
    names.sort();
    names
}

pub fn get_hashes() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = HASH_NAMES.keys().copied().collect();
    names.sort();
    names
}

pub fn get_curves() -> Vec<&'static str> {
    vec!["P-256", "P-384", "P-521", "secp256k1", "secp192k1",
         "ed25519", "x25519", /* + brainpool variants in Stage 2 */]
}

pub fn get_cipher_info<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    name_or_nid: v8::Local<v8::Value>,
    options: Option<v8::Local<v8::Value>>,
) -> Option<v8::Local<'s, v8::Value>> {
    let name = if name_or_nid.is_string() {
        name_or_nid.to_rust_string_lossy(scope).to_lowercase()
    } else {
        // Numeric NID — Node accepts the OpenSSL nid integer; we don't
        // ship the NID table. Return undefined for non-string args.
        return None;
    };
    let entry = CIPHER_REGISTRY.get(name.as_str())?;
    let obj = v8::Object::new(scope);
    set_str(scope, obj, "name", entry.canonical_name);
    set_u32(scope, obj, "blockSize", entry.block_size as u32);
    if let Some(iv) = entry.iv_length {
        set_u32(scope, obj, "ivLength", iv as u32);
    }
    set_str(scope, obj, "mode", entry.mode.as_str());
    set_u32(scope, obj, "keyLength", entry.key_lengths[0] as u32);    // first valid length
    Some(obj.into())
}
```

## X. Stage 2: deferred APIs

### X.1. X.509 (D-N20)

Stage 1 throws `ERR_CRYPTO_UNSUPPORTED_OPERATION` on `new X509Certificate(...)`. Stage 2 ships parsing-only:

```rust
// crypto_node/x509.rs (Stage 2)

pub struct X509State {
    der: Vec<u8>,
    parsed: ParsedX509,    // computed once at construction
}

pub struct ParsedX509 {
    pub subject: String,        // RFC 4514 distinguished name
    pub issuer: String,
    pub serial_number: String,  // hex
    pub valid_from: String,     // ISO-8601
    pub valid_to: String,
    pub fingerprint: [u8; 20],  // SHA-1
    pub fingerprint256: [u8; 32],
    pub fingerprint512: [u8; 64],
    pub public_key: KeyMaterial,
    pub subject_alt_name: Option<String>,
    pub key_usage: Option<Vec<String>>,
    pub extended_key_usage: Option<Vec<String>>,
    pub authority_key_identifier: Option<String>,
    pub subject_key_identifier: Option<String>,
}

#[v8_class]
#[v8_constructor]
#[v8_to_string_tag = "X509Certificate"]
impl X509Certificate {
    fn new(buffer: v8::Local<v8::Value>) -> Result<X509State, OpError> {
        let bytes = extract_input(buffer)?;
        // Try DER first; on failure, try PEM decode.
        let der = if bytes.starts_with(b"-----") {
            kernel::pem::decode(std::str::from_utf8(&bytes)?)
                .map(|b| b.bytes)
                .map_err(|e| OpError::node("ERR_OSSL_PEM_NO_START_LINE", e.to_string()))?
        } else {
            bytes
        };
        let parsed = kernel::x509::parse(&der)
            .map_err(KernelError::to_node)?;
        Ok(X509State { der, parsed })
    }

    #[v8_getter] fn subject(&self) -> &str { &self.parsed.subject }
    #[v8_getter] fn issuer(&self) -> &str { &self.parsed.issuer }
    #[v8_getter] fn serial_number(&self) -> &str { &self.parsed.serial_number }
    /* ... etc */

    /// Returns a PublicKeyObject — bridges via Arc.
    #[v8_getter(same_object)]
    fn public_key<'s>(&self, scope: &mut v8::PinScope<'s, '_>)
        -> v8::Global<v8::Object>
    {
        let ko_state = KeyObjectState {
            key_type: KeyType::Public,
            material: Arc::new(self.parsed.public_key.clone()),
        };
        let local = PublicKeyObject::build(scope, ko_state);
        v8::Global::new(scope, local)
    }
}
```

The X.509 parser is the heavy lift — RFC 5280 v3 has many extensions. We implement a minimal subset (subject/issuer/sn/valid/keyusage/SAN/AKI/SKI), enough for the JWT JWKS use case. Anything else triggers `ERR_OSSL_X509_PARSE`.

**Stage 2 explicitly does NOT ship `verify(publicKey)` / `checkHost(name)` / `checkIssued(other)`.** Those require chain-walk logic; defer to a userspace `@zeroship/x509-verify` npm package backed by aws-lc-sys's `X509_verify_cert`.

### X.2. `timingSafeEqual` (D-N31)

```rust
pub fn timing_safe_equal<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    a: v8::Local<v8::Value>,
    b: v8::Local<v8::Value>,
) -> Result<bool, OpError> {
    let a_bytes = buffer::extract_input(scope, a, None)?;
    let b_bytes = buffer::extract_input(scope, b, None)?;
    if a_bytes.len() != b_bytes.len() {
        return Err(OpError::node("ERR_CRYPTO_TIMING_SAFE_EQUAL_LENGTH",
            "Input buffers must have the same byte length"));
    }
    Ok(aws_lc_rs::constant_time::verify_slices_are_equal(&a_bytes, &b_bytes).is_ok())
}
```

### X.3. FIPS controls (D-N25)

```rust
// crypto_node/misc.rs

pub fn get_fips() -> u32 { 0 }

pub fn set_fips<'s>(_scope: &mut v8::PinScope<'s, '_>, mode: bool)
    -> Result<(), OpError>
{
    if mode {
        Err(OpError::node("ERR_CRYPTO_OPERATION_FAILED",
            "FIPS mode toggle not supported in this runtime build"))
    } else {
        Ok(())    // Already in non-FIPS mode; accept.
    }
}

pub fn secure_heap_used<'s>(scope: &mut v8::PinScope<'s, '_>)
    -> v8::Local<'s, v8::Value>
{
    let obj = v8::Object::new(scope);
    set_u64(scope, obj, "total", 0);
    set_u64(scope, obj, "min", 0);
    set_u64(scope, obj, "used", 0);
    set_f64(scope, obj, "utilization", 0.0);
    obj.into()
}

pub fn set_engine<'s>(_scope: &mut v8::PinScope<'s, '_>,
    _engine: v8::Local<v8::Value>,
    _flags: Option<v8::Local<v8::Value>>,
) -> Result<(), OpError> {
    Err(OpError::node("ERR_CRYPTO_CUSTOM_ENGINE_NOT_SUPPORTED",
        "OpenSSL engines are not supported in this runtime"))
}
```

### X.4. Diffie-Hellman named groups (Stage 2 — D-N21)

```rust
// crypto_node/dh.rs (Stage 2)

#[v8_class]
#[v8_to_string_tag = "DiffieHellmanGroup"]
pub struct DiffieHellmanGroup;

pub fn get_diffie_hellman<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    name: String,
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    let group = match name.as_str() {
        "modp1" => DhGroup::Modp1,    // 768-bit, BLOCKED unless --insecure-dh-groups
        "modp2" => DhGroup::Modp2,    // 1024-bit, BLOCKED
        "modp5" => DhGroup::Modp5,    // 1536-bit
        "modp14" => DhGroup::Modp14,  // 2048-bit (RFC 3526)
        "modp15" => DhGroup::Modp15,  // 3072-bit
        "modp16" => DhGroup::Modp16,  // 4096-bit
        "modp17" => DhGroup::Modp17,  // 6144-bit
        "modp18" => DhGroup::Modp18,  // 8192-bit
        "ffdhe2048" => DhGroup::Ffdhe2048,
        "ffdhe3072" => DhGroup::Ffdhe3072,
        "ffdhe4096" => DhGroup::Ffdhe4096,
        "ffdhe6144" => DhGroup::Ffdhe6144,
        "ffdhe8192" => DhGroup::Ffdhe8192,
        other => return Err(OpError::node("ERR_CRYPTO_UNKNOWN_DH_GROUP",
            format!("Unknown DH group: {}", other))),
    };
    if matches!(group, DhGroup::Modp1 | DhGroup::Modp2)
        && !is_insecure_dh_enabled() {
        return Err(OpError::node("ERR_CRYPTO_INVALID_DH_PRIME",
            "768-bit and 1024-bit DH groups disabled (use --insecure-dh-groups)"));
    }
    /* ... build DiffieHellmanGroup wrapper ... */
}
```

DH primes (modp14/15/16/17/18, ffdhe*) are stored as static byte arrays in the kernel. Backed by aws-lc-sys's `DH_set0_pqg` for the actual key-agreement computation.

### X.5. Legacy cipher policy (D-N22)

A runtime flag `ZEROSHIP_LEGACY_CRYPTO=1` (or CLI `--legacy-crypto`) enables DES/3DES/Blowfish/Cast5/RC4/IDEA/MD5 (some). Without the flag, `createCipheriv("des-cbc", ...)` errors with `ERR_OSSL_EVP_UNSUPPORTED_ALGORITHM` and a message pointing at the flag.

```rust
fn check_legacy_allowed(alg: CipherAlg) -> Result<(), OpError> {
    if alg.is_legacy() && !legacy_crypto_enabled() {
        return Err(OpError::node("ERR_OSSL_EVP_UNSUPPORTED_ALGORITHM",
            format!("{} is a legacy cipher; enable with --legacy-crypto", alg.name())));
    }
    Ok(())
}
```

## XI. Synthetic module install (D-N26)

### XI.1. The install path

The vite-plugin's `customPolyfills["node:crypto"]` (currently 60 LOC of JS) becomes a generated TS file that just re-exports named slots from the global `__zeroship_node_crypto`:

```ts
// sdks/vite-plugin/src/node-crypto.gen.ts (NEW — generated at build time
//                                          from crypto_node's surface)

const _zsc = globalThis.__zeroship_node_crypto;
const _g = globalThis;

// Streaming primitive classes (constructors).
export const Hash = _zsc.Hash;
export const Hmac = _zsc.Hmac;
export const Cipher = _zsc.Cipher;
export const Decipher = _zsc.Decipher;
export const Sign = _zsc.Sign;
export const Verify = _zsc.Verify;
export const KeyObject = _zsc.KeyObject;
export const PublicKeyObject = _zsc.PublicKeyObject;
export const PrivateKeyObject = _zsc.PrivateKeyObject;
export const SecretKeyObject = _zsc.SecretKeyObject;
export const ECDH = _zsc.ECDH;
// Stage 2 classes:
export const DiffieHellman = _zsc.DiffieHellman;
export const DiffieHellmanGroup = _zsc.DiffieHellmanGroup;
export const X509Certificate = _zsc.X509Certificate;

// Factories (free functions).
export const createHash = _zsc.createHash;
export const createHmac = _zsc.createHmac;
export const createCipheriv = _zsc.createCipheriv;
export const createDecipheriv = _zsc.createDecipheriv;
export function createCipher(...args) { return _zsc.createCipher(...args); }    // throws
export const createSign = _zsc.createSign;
export const createVerify = _zsc.createVerify;
export const createSecretKey = _zsc.createSecretKey;
export const createPublicKey = _zsc.createPublicKey;
export const createPrivateKey = _zsc.createPrivateKey;
export const createECDH = _zsc.createECDH;
export const createDiffieHellman = _zsc.createDiffieHellman;
export const createDiffieHellmanGroup = _zsc.createDiffieHellmanGroup;
export const getDiffieHellman = _zsc.getDiffieHellman;
export const getCiphers = _zsc.getCiphers;
export const getHashes = _zsc.getHashes;
export const getCurves = _zsc.getCurves;
export const getCipherInfo = _zsc.getCipherInfo;
export const getRandomValues = _g.crypto.getRandomValues.bind(_g.crypto);

// Random.
export const randomUUID = _zsc.randomUUID;
export const randomBytes = _zsc.randomBytes;
export const randomFillSync = _zsc.randomFillSync;
export const randomFill = _zsc.randomFill;
export const randomInt = _zsc.randomInt;

// One-shot APIs.
export const sign = _zsc.signOneshot;
export const verify = _zsc.verifyOneshot;
export const publicEncrypt = _zsc.publicEncrypt;
export const privateDecrypt = _zsc.privateDecrypt;
export const publicDecrypt = _zsc.publicDecrypt;
export const privateEncrypt = _zsc.privateEncrypt;
export const diffieHellman = _zsc.diffieHellmanOneshot;
export const hash = _zsc.hashOneshot;

// KDFs (split sync/async per VI.2).
export function pbkdf2(password, salt, iters, keylen, digest, callback) {
    if (typeof callback !== 'function')
        throw new TypeError("ERR_INVALID_CALLBACK: pbkdf2 callback is required");
    _zsc.pbkdf2Async(password, salt, iters, keylen, digest)
        .then(buf => callback(null, buf), err => callback(err));
}
export const pbkdf2Sync = _zsc.pbkdf2Sync;
export function scrypt(password, salt, keylen, options, callback) {
    if (typeof options === 'function') { callback = options; options = undefined; }
    if (typeof callback !== 'function')
        throw new TypeError("ERR_INVALID_CALLBACK: scrypt callback is required");
    _zsc.scryptAsync(password, salt, keylen, options)
        .then(buf => callback(null, buf), err => callback(err));
}
export const scryptSync = _zsc.scryptSync;
export function hkdf(digest, ikm, salt, info, keylen, callback) {
    if (typeof callback !== 'function')
        throw new TypeError("ERR_INVALID_CALLBACK: hkdf callback is required");
    _zsc.hkdfAsync(digest, ikm, salt, info, keylen)
        .then(buf => callback(null, buf), err => callback(err));
}
export const hkdfSync = _zsc.hkdfSync;

// Key generation (split sync/async).
export function generateKeyPair(type, options, callback) {
    if (typeof options === 'function') { callback = options; options = {}; }
    if (typeof callback !== 'function')
        throw new TypeError("ERR_INVALID_CALLBACK: generateKeyPair callback is required");
    _zsc.generateKeyPairAsync(type, options).then(
        ({ publicKey, privateKey }) => callback(null, publicKey, privateKey),
        err => callback(err));
}
export const generateKeyPairSync = _zsc.generateKeyPairSync;
export function generateKey(type, options, callback) {
    if (typeof callback !== 'function')
        throw new TypeError("ERR_INVALID_CALLBACK: generateKey callback is required");
    _zsc.generateKeyAsync(type, options)
        .then(key => callback(null, key), err => callback(err));
}
export const generateKeySync = _zsc.generateKeySync;

// Misc.
export const timingSafeEqual = _zsc.timingSafeEqual;
export function setEngine(engine, flags) { _zsc.setEngine(engine, flags); }
export function getFips() { return _zsc.getFips(); }
export function setFips(mode) { _zsc.setFips(mode); }
export const fips = false;
export function secureHeapUsed() { return _zsc.secureHeapUsed(); }

// WebCrypto bridge (D-N16 — direct property references).
export const webcrypto = _g.crypto;
export const subtle = _g.crypto.subtle;

// Constants.
export const constants = {
    RSA_PKCS1_PADDING: 1,
    RSA_NO_PADDING: 3,
    RSA_PKCS1_OAEP_PADDING: 4,
    RSA_PKCS1_PSS_PADDING: 6,
    RSA_PSS_SALTLEN_DIGEST: -1,
    RSA_PSS_SALTLEN_MAX_SIGN: -2,
    RSA_PSS_SALTLEN_AUTO: -2,
    POINT_CONVERSION_COMPRESSED: 2,
    POINT_CONVERSION_UNCOMPRESSED: 4,
    POINT_CONVERSION_HYBRID: 6,
};

// Default export — Node packages use both named and default imports.
export default {
    Hash, Hmac, Cipher, Decipher, Sign, Verify, KeyObject,
    PublicKeyObject, PrivateKeyObject, SecretKeyObject,
    ECDH, DiffieHellman, DiffieHellmanGroup, X509Certificate,
    createHash, createHmac, createCipheriv, createDecipheriv,
    createCipher, createSign, createVerify,
    createSecretKey, createPublicKey, createPrivateKey,
    createECDH, createDiffieHellman, createDiffieHellmanGroup,
    getDiffieHellman, getCiphers, getHashes, getCurves, getCipherInfo,
    getRandomValues, randomUUID, randomBytes, randomFillSync, randomFill, randomInt,
    sign, verify, publicEncrypt, privateDecrypt, publicDecrypt, privateEncrypt,
    diffieHellman, hash, pbkdf2, pbkdf2Sync, scrypt, scryptSync, hkdf, hkdfSync,
    generateKeyPair, generateKeyPairSync, generateKey, generateKeySync,
    timingSafeEqual, setEngine, getFips, setFips, fips, secureHeapUsed,
    webcrypto, subtle, constants,
};
```

This file replaces the JS shim's `customPolyfills["node:crypto"]` block. Generated at build time from a single source-of-truth list in `crypto_node/module.rs`; checked-in to keep the build deterministic.

### XI.2. Rust-side install

```rust
// crypto_node/module.rs

pub fn install_globals<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    global: v8::Local<v8::Object>,
) {
    // Build the __zeroship_node_crypto namespace object.
    let zsc = v8::Object::new(scope);

    // Install classes — each #[v8_class] has its own install_global helper.
    Hash::install(scope, zsc);
    Hmac::install(scope, zsc);
    Cipher::install(scope, zsc);
    Decipher::install(scope, zsc);
    Sign::install(scope, zsc);
    Verify::install(scope, zsc);
    KeyObject::install(scope, zsc);
    PublicKeyObject::install(scope, zsc);
    PrivateKeyObject::install(scope, zsc);
    SecretKeyObject::install(scope, zsc);
    ECDH::install(scope, zsc);
    // Stage 2:
    // DiffieHellman::install(scope, zsc);
    // DiffieHellmanGroup::install(scope, zsc);
    // X509Certificate::install(scope, zsc);

    // Install factory functions and free ops.
    install_fn(scope, zsc, "createHash", create_hash_callback);
    install_fn(scope, zsc, "createHmac", create_hmac_callback);
    install_fn(scope, zsc, "createCipheriv", create_cipheriv_callback);
    /* ... ~50 free-function installs ... */

    // Install on globalThis.
    let key = v8::String::new(scope, "__zeroship_node_crypto").unwrap();
    global.set(scope, key.into(), zsc.into());
}
```

### XI.3. Bridge integration

The vite-plugin's `customPolyfills["node:crypto"]` entry is replaced with the contents of the generated TS file (read at plugin-load time):

```ts
// sdks/vite-plugin/src/node-compat.ts (modified)

import nodeCryptoSource from "./node-crypto.gen.ts?raw";    // import as raw text

const customPolyfills: Record<string, string> = {
  "node:crypto": nodeCryptoSource,    // was a 60-LOC inline string
  "node:timers/promises": `...`,      // unchanged
  "node:module": `...`,               // unchanged
  "node:process": `...`,              // unchanged
};
```

The change is a one-line replacement of the inline string with a `?raw` import. The `node:crypto.gen.ts` file is generated by a build script and checked in.

## XII. Cutover cadence (D-N27)

Five stages, each its own PR:

### Stage A — Kernel extraction (no behaviour change)

Refactor existing `crypto_native/aes.rs` / `rsa.rs` / `ec.rs` / `okp.rs` / `hmac.rs` / `derive.rs` to call into a new `crypto_kernel/` module. The existing files become thin wrappers:

```rust
// crypto_native/aes.rs (after refactor — 100 LOC, was 852)
pub fn encrypt_gcm(...) -> Result<Vec<u8>, OpError> {
    crate::crypto_kernel::cipher::aes_gcm_encrypt(...)
        .map_err(KernelError::to_webcrypto)
}
// ... 5 similar wrappers
```

The kernel files (`crypto_kernel/cipher.rs`, etc.) carry the original logic. The existing 16 WebCrypto algorithms work unchanged; tests pass; behaviour identical.

**Net:** ~3500 LOC moved into the kernel, ~500 LOC of glue removed from `crypto_native/`. No deletion of the JS shim; no node:crypto changes.

### Stage B — Hash + Hmac + KDFs + Random + WebCrypto bridge native

Adds the new `crypto_node/{hash,hmac,kdf,random,buffer,encoding,error,misc,webcrypto}.rs` files. Installs `__zeroship_node_crypto.{Hash, Hmac, randomBytes, ..., webcrypto, subtle}` on globalThis. Generates the synthetic-module TS file (covering only the Stage B subset).

The vite-plugin's `node-crypto.gen.ts` for Stage B re-exports:
- `Hash`, `Hmac`, `createHash`, `createHmac`, `getHashes`, `hash`
- `randomBytes`, `randomFillSync`, `randomFill`, `randomInt`, `randomUUID`
- `pbkdf2`, `pbkdf2Sync`, `scrypt`, `scryptSync`, `hkdf`, `hkdfSync`
- `timingSafeEqual`, `webcrypto`, `subtle`, `getRandomValues`, `getFips`, `setFips`, `fips`, `constants`

The remaining exports (Cipher, Sign, KeyObject, X509, DH) re-export `undefined` placeholders that throw on use:

```ts
export const Cipher = function() {
    throw new Error("Cipher not yet supported in this runtime build (Stage C pending)");
};
```

After Stage B lands: the JS shim's `__cryptoHashSync` / `__cryptoHmacSync` are dead code. Remove the V8 callbacks at `crates/runtime/src/crypto.rs:128-212` and the global installs at `crates/runtime/src/init.rs:1444-1453`. The shim's hash + hmac + random functions delete; only Cipher/Sign/KeyObject placeholders remain.

**Net (Stage B):** ~1800 LOC native added; ~150 LOC of JS shim + Rust ad-hoc callbacks deleted; ~70% of npm-package crypto calls covered.

### Stage C — KeyObject + Sign + Verify + Cipher + Decipher

Adds `crypto_node/{key_object,sign,cipher,dh}.rs` plus the `kernel/cipher.rs` extensions for ChaCha20-Poly1305 / OCB. Wires the synthetic-module entries.

After Stage C lands: the JS shim's `customPolyfills["node:crypto"]` is reduced to the generated TS file referencing fully native bindings. Stage 2 placeholder classes (X509Certificate, DiffieHellman) still throw.

**Net (Stage C):** ~2100 LOC native added; ~95% of npm-package usage covered.

### Stage D — Bridge polish + the gen-file replacement

The vite-plugin's `customPolyfills["node:crypto"]` is now the generated TS file (read via `?raw` import). The remaining 60-LOC inline JS string DELETES. The cutover is done for Stage 1.

**Net (Stage D):** ~60 LOC of JS deleted; the build pipeline now generates `node-crypto.gen.ts` from a single Rust-side source list.

### Stage E — Stage 2 algorithms (X.509, DH, legacy ciphers, BLAKE2, 3DES, prime gen)

Adds `crypto_node/x509.rs`, `kernel/x509.rs`, plus the legacy-cipher entries gated on `--legacy-crypto`. The DH primes and Brainpool curves go in the kernel.

**Net (Stage E):** ~1200 LOC native added; ~99% of npm-package usage covered. Long tail: `setEngine` (NEVER), `crypto.signal` (NEVER), `verifyCertificate` (NEVER — experimental).

### XII.1. Stage cost summary

| Stage | Industry h | Agent-pace h | Coverage delta |
|---|---:|---:|---:|
| A — kernel extraction | 32 | 0.80 | 0% (refactor) |
| B — hash/hmac/kdf/random/bridge | 60 | 1.50 | +70% |
| C — keyobject/sign/cipher | 90 | 2.25 | +25% (cumulative 95%) |
| D — gen-file polish | 12 | 0.30 | 0% (cleanup) |
| E — X.509/DH/legacy | 60 | 1.50 | +4% (cumulative 99%) |
| **Total** | **254h** | **~6.4h** | **99% coverage** |

The agent-pace estimates apply the project's `/40` divisor. Stage A through Stage D are the prerequisite for shim deletion; Stage E is independent and can ship anytime after Stage D.

## XIII. Macro extensions needed (D-N32)

Only ONE new macro feature:

### XIII.1. `OpErrorKind::NodeError(code: &'static str)` variant

Companion to the existing `OpErrorKind::DomException(name)` variant from D-6 of webcrypto-native. Adds:

```rust
// crates/runtime/src/state.rs (modified — single variant addition)
pub enum OpErrorKind {
    TypeError,
    RangeError,
    Error,
    DomException(&'static str),    // existing
    NodeError(&'static str),       // NEW (D-N32)
}

impl OpError {
    /* existing constructors ... */
    pub fn node(code: &'static str, msg: impl Into<String>) -> Self {
        Self {
            kind: OpErrorKind::NodeError(code),
            message: msg.into(),
        }
    }
}
```

Macro change in `runtime-macros/src/lib.rs::gen_throw_error` — add one match arm (~25 LOC) that constructs Error/TypeError/RangeError per a per-code routing table and sets the `.code` property. The full implementation is shown in §VII.5.

**Total macro extension:** ~30 LOC across `runtime-macros/src/lib.rs` and `crates/runtime/src/state.rs`.

### XIII.2. NOT macro extensions

Items that look like they need macro support but actually don't:

- **Buffer extraction.** Library helper `crypto_node/buffer.rs::extract_input` called explicitly from each method. The macro doesn't need to know about Buffer.
- **Streaming Context types.** Plain Rust state in the boxed `HashState` / `HmacState` / etc. The macro sees a regular `&mut self` method.
- **The synthetic module install.** Pure JS / TS pipeline; no macro work.
- **The `Arc<KeyMaterial>` share between CryptoKey and KeyObject.** Plain Rust Arc; no macro work.
- **The `#[v8_inherit(KeyObject)]` for the three subclasses.** Already supported by the existing macro (used by AbortSignal extends EventTarget).
- **`#[v8_static_method]` for `KeyObject.from(cryptoKey)`.** Already supported.
- **`#[v8_getter(same_object)]` for `keyObject.asymmetricKeyDetails`.** Already supported per `crates/runtime-macros/TODO.md` "Done" section.

**Final macro footprint: 1 new variant + ~25 LOC of arm logic. Same magnitude as webcrypto-native's D-30 (which started with 4 items and reduced to 2 after analysis).**

## XIV. Test plan

### XIV.1. Hand-written tests

Lives in `crates/runtime/tests/`. Mirrors the patterns from `crypto_native.rs` / `crypto_jwk.rs` / etc.

- **`crypto_node_hash.rs`:**
  - `createHash('sha256').update('hello').digest('hex')` — basic.
  - Streaming: `update('a').update('b').digest('hex') === createHash('sha256').update('ab').digest('hex')`.
  - `digest()` (no encoding) returns Buffer; `digest('hex')` returns string; `digest('base64')` returns base64 string.
  - `copy()` returns fresh Hash with same in-progress state.
  - `digest()` then `update()` throws `ERR_CRYPTO_HASH_FINALIZED`.
  - `createHash('unknown')` throws `ERR_OSSL_EVP_UNSUPPORTED`.
  - `createHash('SHA256')` (case-insensitive) works.
- **`crypto_node_hmac.rs`:**
  - `createHmac('sha256', 'key').update('msg').digest('hex')` — basic.
  - Empty key throws (RFC 2104 forbids zero-length keys; Node throws `ERR_OSSL_HMAC_KEY_TOO_SHORT`).
  - Buffer key works.
  - SecretKeyObject key works.
  - `digest('base64url')` works.
- **`crypto_node_random.rs`:**
  - `randomBytes(16)` returns 16-byte Buffer.
  - `randomBytes(0)` returns 0-byte Buffer.
  - `randomBytes(2 ** 31)` throws `ERR_OUT_OF_RANGE`.
  - `randomBytes(16, callback)` calls `callback(null, buf)` async.
  - `randomFillSync(buf, 0, 4)` fills first 4 bytes; remaining unchanged.
  - `randomFill(buf, callback)` async.
  - `randomInt(0, 100)` — sample 1000 times, all in [0, 100).
  - `randomInt(0, 1)` always returns 0.
  - `randomInt(100, 0)` throws `ERR_OUT_OF_RANGE`.
  - `randomUUID()` returns 36-char string matching v4 pattern.
- **`crypto_node_kdf.rs`:**
  - `pbkdf2Sync('password', 'salt', 100, 32, 'sha256')` returns 32-byte Buffer.
  - `pbkdf2('password', 'salt', 100, 32, 'sha256', cb)` calls `cb(null, buf)`.
  - `pbkdf2Sync(..., 0, ...)` throws `ERR_OUT_OF_RANGE`.
  - `pbkdf2Sync(..., 'unknown')` throws `ERR_OSSL_EVP_UNSUPPORTED`.
  - `scryptSync('pw', 'salt', 64)` returns 64-byte Buffer.
  - `scryptSync('pw', 'salt', 64, { N: 16384, r: 8, p: 1 })` works.
  - `scryptSync('pw', 'salt', 64, { maxmem: 1024 })` throws `ERR_CRYPTO_SCRYPT_NOT_SUPPORTED`.
  - `hkdfSync('sha256', ikm, salt, info, 32)` returns 32-byte Buffer.
- **`crypto_node_cipher.rs`:**
  - AES-256-GCM round-trip: encrypt → decrypt with matching key/iv/aad/tag.
  - AES-256-CBC round-trip with PKCS#7 padding default.
  - AES-256-CBC with `setAutoPadding(false)` requires exact-block input.
  - ChaCha20-Poly1305 round-trip.
  - `createCipher('des-cbc', ...)` throws `ERR_CRYPTO_DEPRECATED_API`.
  - `createCipheriv('des-cbc', ...)` (without legacy flag) throws `ERR_OSSL_EVP_UNSUPPORTED_ALGORITHM`.
  - GCM `getAuthTag()` before final() throws `ERR_CRYPTO_INVALID_STATE`.
  - GCM Decipher `setAuthTag` then mismatched tag → `ERR_OSSL_BAD_DECRYPT`.
- **`crypto_node_sign_verify.rs`:**
  - RSA-SHA256 sign/verify round-trip with PEM private key.
  - RSA-SHA256 sign/verify round-trip with KeyObject.
  - RSA-PSS sign/verify with `padding: RSA_PKCS1_PSS_PADDING, saltLength: 32`.
  - ECDSA-SHA256 with default DER encoding.
  - ECDSA-SHA256 with `dsaEncoding: 'ieee-p1363'` (interop with WebCrypto signatures).
  - Ed25519 sign/verify with `algorithm: null`.
  - Ed25519 sign with `algorithm: 'sha256'` throws `ERR_OSSL_EVP_INVALID_DIGEST`.
  - One-shot `crypto.sign(null, data, ed25519PrivateKey)` works.
- **`crypto_node_keyobject.rs`:**
  - `createSecretKey(Buffer.from('secret'))` returns SecretKeyObject; `.type === 'secret'`.
  - `createPrivateKey(pem)` returns PrivateKeyObject.
  - `createPublicKey(privateKeyObject)` extracts the public half.
  - `keyObject.export({format:'pem',type:'pkcs8'})` round-trips with `createPrivateKey`.
  - `keyObject.export({format:'jwk'})` matches the JWK shape in `crypto_native/jwk.rs`.
  - `keyObject.equals(other)` constant-time-compare; same secret → true; different → false.
  - `keyObject instanceof KeyObject` true (via brand-check); `instanceof PrivateKeyObject` true for private keys.
- **`crypto_node_bridge.rs`:**
  - `webcrypto === globalThis.crypto`.
  - `subtle === globalThis.crypto.subtle`.
  - `KeyObject.from(cryptoKey)` — bridge a WebCrypto-issued key to KeyObject.
  - `keyObject.export({format:'jwk'})` then `webcrypto.subtle.importKey('jwk', ...)` round-trip.
  - The Arc identity test: `KeyObject.from(KeyObject.from(cryptoKey).asymmetricKeyType ...)` chains preserve material.
  - getRandomValues from node:crypto fills typed array same as globalThis.crypto.getRandomValues.
- **`crypto_node_x509.rs`** (Stage 2):
  - Parse a PEM-encoded X.509 certificate from a fixture.
  - `cert.subject` matches the fixture's distinguished name.
  - `cert.fingerprint256` matches a known SHA-256 hash of the DER.
  - `cert.publicKey` is a PublicKeyObject; export round-trips.
- **`crypto_node_errors.rs`:**
  - Every NodeError code in §VII.3's mapping fires with the right `e.code` from a triggering API call. ~30 assertions.
  - `ERR_INVALID_ARG_TYPE` paths all produce `TypeError` instance (not Error).
  - `ERR_OUT_OF_RANGE` paths all produce `RangeError`.
  - All other paths produce `Error`.

### XIV.2. Vendoring Node's own test suite

Node's `test/parallel/test-crypto-*.js` (https://github.com/nodejs/node/tree/main/test/parallel) is the comprehensive test suite. Approach:

1. **Identify Tier 1 fixtures.** ~80 of the ~200 test files are relevant (the others test legacy ciphers, FIPS internals, OpenSSL-specific quirks).
2. **Vendor a curated subset** at `crates/runtime/tests/wpt/node_crypto/` (sparse-checkout from a pinned Node commit; setup script update similar to setup-wpt.sh).
3. **Write a runner** at `crates/runtime/tests/node_crypto_compat.rs` that boots the runtime and runs each `test-crypto-*.js` file. Most files use Node's `assert` module (which we'd need to provide via unenv as a Tier 1 dep — already supported).
4. **Track expectations** at `crates/runtime/tests/node-crypto.expectations` (mirrors `crypto_native/`'s WPT expectations file). List which test files pass / known-failing-with-reason.

Node's tests use `common.js` test harness — small effort to provide the `common.hasCrypto` / `common.skipIf` shims.

**Targeted pass rates:**
- Stage B end: 30% of vendored tests (hash + hmac + random + KDF coverage).
- Stage C end: 80% of vendored tests (+ keyobject + sign/verify + cipher).
- Stage E end: 95% of vendored tests (+ X509 + DH + legacy with flag).

### XIV.3. Cross-package compat tests

Beyond Node's own tests, smoke-test against actual npm packages:

- `jsonwebtoken` — sign + verify a JWT with HS256 / RS256 / ES256 / EdDSA.
- `bcrypt` — hash + compare a password (bcrypt is internally not node:crypto, but its `genSalt` / `hash` paths use `randomBytes` — verify those work).
- `node-forge` — round-trip an X.509 certificate parse (Stage 2).
- `nanoid` — verify uniqueness of generated IDs (uses `randomBytes`).
- `pino` — log signing path (uses `createHmac`).
- `firebase-admin` — JWT verification (uses `crypto.createVerify`).

These tests live at `crates/runtime/tests/npm_compat/` and are gated to a separate CI job (they require `npm install` of the test packages).

## XV. Comparison with reference implementations

| Project | node:crypto LOC | Approach | Storage | Sync/async |
|---------|----------------:|----------|---------|-----------|
| **Node.js** (gold standard) | ~5,500 JS (`lib/internal/crypto/`) + ~7,000 C++ (`src/crypto/`) = ~12,500 LOC | Hybrid; JS layer enforces validation + types, C++ wraps OpenSSL EVP API | OpenSSL EVP_PKEY (refcounted) | C++ uses libuv thread pool for async; sync runs on V8 thread |
| **Bun** | ~3,500 Zig (`src/bun.js/node/node_crypto.zig`) + ~600 TS facade (`src/js/node/crypto.ts`) = ~4,100 LOC | Native Zig with thin TS facade. Each Node API has a Zig native impl; no JS shim. | OpenSSL EVP_PKEY via `boring` | Zig `JSC.AsyncTask` for async; sync runs on JS thread |
| **workerd** | ~5,000 C++ (`src/node/internal/crypto*`) | Pure native C++ over BoringSSL via ncrypto helpers. Mirrors Node's class hierarchy. | `KeyContext` shared between WebCrypto + node:crypto | Always sync (workerd has no thread pool); the `*Sync` Node APIs map directly, async APIs throw or queue via kj's promise |
| **Deno** | ~3,000 Rust ops (`ext/node/ops/crypto/`) + ~3,500 TS polyfill (`ext/node/polyfills/internal/crypto/*.ts`) = ~6,500 LOC | Hybrid; ops in Rust, JS facade orchestrates. The TS facade does encoding / type validation; ops do the heavy lift. | `KeyObjectHandle` Rust struct, separate from CryptoKey's storage | Rust ops use `tokio::task::spawn_blocking` for async; sync runs as a regular sync op |
| **Current zeroship** | 60 JS shim (`node-compat.ts:67-127`) + 92 Rust ad-hoc (`crypto.rs:128-212`) + 313 WebCrypto JS (deleted) + 1308 WebCrypto Rust = ~1,700 LOC total | JS shim layers calling `__cryptoHashSync` / `__cryptoHmacSync`. WebCrypto is native; node:crypto is a facade | None for node:crypto | Sync only; the shim's `pbkdf2 / scrypt` paths are completely missing |
| **This design** | ~5,800 LOC native (kernel ~2500 + crypto_node ~3000 + crypto_native refactor ~300) + ~250 LOC TS shim (re-exports) = ~6,050 LOC | Pure native with shared kernel; TS shim is purely re-exports. Closer to Bun's approach in shape; closer to workerd's in depth. | `Arc<KeyMaterial>` shared between CryptoKey and KeyObject (D-N4) | Sync on V8 thread for sync APIs; `spawned_ops` blocking-pool for async APIs |

Where this design lands:
- **Smaller than Node.js** (~6K vs ~12K LOC) because aws-lc-rs has a higher-level API than raw OpenSSL EVP, eliminating ~5K LOC of FFI glue.
- **Comparable to Deno + workerd** (~6K vs ~5-6.5K LOC) — same algorithm scope.
- **Larger than Bun** (~6K vs ~4K LOC) because Bun reuses Zig's stdlib for cipher modes; we use aws-lc-rs's high-level + low-level FFI for variable-IV/-tag GCM.

Storage approach:
- We're alone in sharing `Arc<KeyMaterial>` directly between WebCrypto and node:crypto. Node uses two separate types (`CryptoKey` and `KeyObject`) bridged via copy-on-`KeyObject.from()`. workerd uses a shared `KeyContext` struct (similar in spirit). Our Arc share is the most efficient.

Sync/async approach:
- workerd is pure-sync (no thread pool). All others have a thread pool for async KDF / keygen. Our `state.spawned_ops` is the same shape as Deno's `spawn_blocking`.

## XVI. Implementation sequence

### XVI.1. Order

Each step lands as its own commit cluster within its Stage. Stages A-D are the Stage 1 plan; Stage E is the Stage 2 plan.

#### Stage A — Kernel extraction (no behaviour change)

1. Create `crypto_kernel/` skeleton: `mod.rs`, `error.rs` (KernelError enum), `algorithms.rs` (the phf::Map registry), `pem.rs` (RFC 7468), `der.rs` (move from crypto_native), `key_material.rs` (move from crypto_native, wrap in Arc), `jwk.rs` (move from crypto_native).
2. Move `crypto_native/aes.rs::aes_gcm_encrypt_raw` etc. into `crypto_kernel/cipher.rs::CipherContext`. The kernel exposes incremental `update`/`finalize`; the WebCrypto path calls `cipher_one_shot`.
3. Move `crypto_native/digest.rs` into `crypto_kernel/digest.rs::DigestContext` + `digest_one_shot`. Refactor `crypto_native/digest.rs` to a 5-LOC wrapper.
4. Move `crypto_native/hmac.rs::sign` / `verify` into `crypto_kernel/hmac.rs::HmacContext` + `hmac_one_shot` / `hmac_verify_one_shot`.
5. Move `crypto_native/derive.rs` into `crypto_kernel/kdf.rs`.
6. Move `crypto_native/rsa.rs` and `ec.rs` and `okp.rs` into `crypto_kernel/sign_verify.rs` (where they expose `SignContext` + `sign_one_shot`).
7. Move `crypto_native/wrap.rs` into `crypto_kernel/cipher.rs` (AES-KW is just another cipher).
8. Refactor `crypto_native/{ops.rs,subtle.rs}` to call kernel functions, mapping `KernelError -> OpError::dom(...)` at the boundary.
9. Run the existing WebCrypto WPT suite: should pass identically. Verify zero behaviour change.

#### Stage B — Hash + Hmac + KDFs + Random + WebCrypto bridge

10. Add `OpErrorKind::NodeError` variant + macro arm (D-N32). Add `OpError::node(...)` constructor.
11. Create `crypto_node/` skeleton: `mod.rs`, `error.rs` (KernelError -> NodeError mapping), `buffer.rs` (extract_input / emit_buffer / emit_output), `encoding.rs` (the encoding registry).
12. Implement `crypto_node/hash.rs::Hash` class + `create_hash` factory.
13. Implement `crypto_node/hmac.rs::Hmac` class + `create_hmac` factory.
14. Implement `crypto_node/random.rs::{random_bytes, random_fill, random_int, random_uuid}`. randomUUID re-exports `crypto_native`'s implementation.
15. Implement `crypto_node/kdf.rs::{pbkdf2_sync, pbkdf2_async, scrypt_sync, scrypt_async, hkdf_sync, hkdf_async}`. scrypt requires a new `kernel/kdf.rs::scrypt` via `aws_lc_sys::EVP_PBE_scrypt` — ~30 LOC of FFI.
16. Implement `crypto_node/misc.rs::{timing_safe_equal, get_hashes, get_fips, set_fips, secure_heap_used, set_engine}`.
17. Implement `crypto_node/webcrypto.rs` — direct property-reference re-exports.
18. Implement `crypto_node/module.rs::install_globals` — installs `__zeroship_node_crypto.{Hash, Hmac, ...}` on globalThis.
19. Generate `sdks/vite-plugin/src/node-crypto.gen.ts` from the Stage B export list.
20. Update `sdks/vite-plugin/src/node-compat.ts::customPolyfills["node:crypto"]` to use the generated TS file (Stage B subset).
21. Hand-written tests: `crypto_node_hash.rs`, `crypto_node_hmac.rs`, `crypto_node_random.rs`, `crypto_node_kdf.rs`, `crypto_node_bridge.rs`, `crypto_node_errors.rs`.
22. Delete `crypto.rs::crypto_hash_sync_callback` / `crypto_hmac_sync_callback` and the global installs at `init.rs:1444-1453`. The `fast_random` thread-local stays.

#### Stage C — KeyObject + Sign + Verify + Cipher + Decipher

23. Implement `crypto_node/key_object.rs::{KeyObject, PublicKeyObject, PrivateKeyObject, SecretKeyObject}` classes. Implement the 3 `create*Key` factories. Implement `KeyObject.from(cryptoKey)` static.
24. Refactor `crypto_native/crypto_key.rs::CryptoKeyState.material` from `KeyMaterial` to `Arc<KeyMaterial>`. Update all callers (~10 sites).
25. Implement `crypto_node/sign.rs::{Sign, Verify}` classes + `create_sign` / `create_verify` factories + one-shot `sign` / `verify`.
26. Implement `crypto_node/cipher.rs::{Cipher, Decipher}` classes + `create_cipheriv` / `create_decipheriv` factories. Implement `createCipher` deprecated-throw.
27. Implement `crypto_node/dh.rs::ECDH` (just ECDH; arbitrary DH is Stage 2).
28. Add `kernel/cipher.rs` ChaCha20-Poly1305 + AES-OCB support.
29. Add Stage 2 placeholders: `DiffieHellman` / `DiffieHellmanGroup` / `X509Certificate` classes that throw `ERR_CRYPTO_UNSUPPORTED_OPERATION` on construction.
30. Add `crypto_node/random.rs::generateKey{,Sync}` and `generateKeyPair{,Sync}`.
31. Update the generated TS file with the Stage C exports.
32. Hand-written tests: `crypto_node_keyobject.rs`, `crypto_node_sign_verify.rs`, `crypto_node_cipher.rs`, `crypto_node_ecdh.rs`.

#### Stage D — Bridge polish + JS shim deletion

33. Replace the inline 60-LOC string in `node-compat.ts:67-127` with `?raw` import of the generated TS.
34. Delete `customPolyfills["node:crypto"]` inline block.
35. Verify the integration test against npm packages (jsonwebtoken, bcrypt) passes end-to-end.

#### Stage E — Stage 2 algorithms

36. Implement `kernel/x509.rs` and `crypto_node/x509.rs` (parse-only, ~600 LOC including ASN.1 v3 cert grammar walker).
37. Implement Stage 2 `DiffieHellman` / `DiffieHellmanGroup` with named groups (modp14-18, ffdhe*).
38. Implement `kernel/cipher.rs` legacy ciphers (3DES + DES-CBC; Blowfish/Cast5/RC4/IDEA via aws-lc-sys raw FFI). Gate on `--legacy-crypto` runtime flag.
39. Implement `generatePrime` / `checkPrime` via aws-lc-sys raw FFI.
40. Implement BLAKE2b/BLAKE2s for `createHash`.
41. Add Brainpool curves to the `kernel/sign_verify.rs` curve table.

### XVI.2. Hours

Industry estimate / agent-pace estimate (per `feedback_estimates_hours_not_weeks` — agent-pace ≈ industry / 40):

| Step | Industry h | Agent-pace h |
|------|-----------:|-------------:|
| 1. crypto_kernel skeleton | 4 | 0.10 |
| 2. AES kernel extraction | 6 | 0.15 |
| 3. Digest kernel extraction | 2 | 0.05 |
| 4. HMAC kernel extraction | 2 | 0.05 |
| 5. KDF kernel extraction | 4 | 0.10 |
| 6. RSA/EC/OKP kernel extraction | 8 | 0.20 |
| 7. AES-KW kernel extraction | 1 | 0.03 |
| 8. crypto_native refactor to call kernel | 4 | 0.10 |
| 9. Verify Stage A WPT zero-delta | 2 | 0.05 |
| **Stage A total** | **33** | **0.83** |
| 10. NodeError macro extension | 4 | 0.10 |
| 11. crypto_node skeleton + buffer/encoding | 6 | 0.15 |
| 12. Hash class | 4 | 0.10 |
| 13. Hmac class | 3 | 0.08 |
| 14. Random ops | 6 | 0.15 |
| 15. KDF ops + scrypt FFI | 8 | 0.20 |
| 16. Misc ops (timingSafeEqual, FIPS stubs, etc.) | 4 | 0.10 |
| 17. WebCrypto bridge | 2 | 0.05 |
| 18. install_globals | 3 | 0.08 |
| 19. node-crypto.gen.ts generation | 4 | 0.10 |
| 20. vite-plugin integration | 2 | 0.05 |
| 21. Stage B hand tests | 12 | 0.30 |
| 22. Delete dead crypto.rs callbacks | 1 | 0.03 |
| **Stage B total** | **59** | **1.49** |
| 23. KeyObject + 3 subclasses + factories | 14 | 0.35 |
| 24. CryptoKey Arc refactor | 4 | 0.10 |
| 25. Sign / Verify | 12 | 0.30 |
| 26. Cipher / Decipher | 18 | 0.45 |
| 27. ECDH | 6 | 0.15 |
| 28. ChaCha20-Poly1305 + AES-OCB kernel | 8 | 0.20 |
| 29. Stage 2 placeholder classes | 2 | 0.05 |
| 30. generateKey + generateKeyPair | 12 | 0.30 |
| 31. TS gen update | 2 | 0.05 |
| 32. Stage C hand tests | 18 | 0.45 |
| **Stage C total** | **96** | **2.40** |
| 33. ?raw shim swap | 1 | 0.03 |
| 34. Inline block delete | 1 | 0.03 |
| 35. Cross-package integration | 6 | 0.15 |
| **Stage D total** | **8** | **0.21** |
| 36. X.509 parse | 24 | 0.60 |
| 37. DH named groups | 12 | 0.30 |
| 38. Legacy ciphers | 14 | 0.35 |
| 39. generatePrime/checkPrime | 6 | 0.15 |
| 40. BLAKE2 | 4 | 0.10 |
| 41. Brainpool | 4 | 0.10 |
| **Stage E total** | **64** | **1.60** |
| **Grand total** | **260h** | **~6.5h** |

For ADR provenance: ~260 industry-hours, comparable to webcrypto-native's 287 hours. node:crypto is broader but reuses much of the kernel after Stage A.

## XVII. Open questions

These are policy-level decisions where reasonable people might disagree. Each has a working answer in the design above.

### XVII.1. Buffer integration — unenv vs native?

D-N7 says: use unenv's Buffer (call `Buffer.from(uint8)` at the V8 boundary). Cost: ~100 ns per crypto call.

**Working answer:** unenv. Native Buffer is a 600-LOC project (all the encoding methods, write*, read*, equals, compare, indexOf, slice, swap*, allocUnsafe pool, ...). Defer to a future Buffer-native ADR.

**Re-open if:** measured perf shows the Buffer overhead dominates (e.g. a benchmark of `createHash('sha256').update('x').digest('hex')` per second is 5x slower than Node — would trigger Buffer-native investigation).

### XVII.2. FIPS mode — toggle at runtime or build-time?

D-N25 says: build-time. `setFips(true)` throws.

**Working answer:** build-time. aws-lc-rs has a sibling crate `aws-lc-fips-sys` that the workspace can opt into. Switching at runtime is not supported by the high-level API. Document.

**Re-open if:** a creator app needs FIPS-validated workflows. Dual-build (one FIPS, one non-FIPS) is the workerd approach; we'd follow.

### XVII.3. Legacy ciphers — flag-gated or never?

D-N22 says: `--legacy-crypto` flag gates DES/3DES/Blowfish/RC4 etc. Off by default.

**Working answer:** flag-gated. Modern apps don't need them; legacy-system interfacing apps opt in.

**Open question:** which ciphers go behind the flag vs. ship in Stage 2 by default?
- Definitely flag-gated: RC4, IDEA, MD5-as-encryption, Blowfish.
- Probably flag-gated: 3DES (still used in some banking; deprecated by NIST post-2023).
- Default-on (Stage 2 with --legacy-crypto NOT required): MD5 hash (legitimate uses for content addressing), DES-EDE3 in CBC (banking compat).

### XVII.4. scrypt vs argon2 — which to ship?

D-N24 says: scrypt only (Node ships scrypt; argon2 is npm-package territory).

**Working answer:** scrypt only. argon2 npm package falls back to WASM via unenv.

**Re-open if:** measured argon2 WASM perf is unacceptable for password verification at production load (>10ms per verify on a ~200req/s worker).

### XVII.5. DH support — named groups only, or arbitrary primes?

D-N21 says: Stage 1 named groups only; Stage 2 arbitrary primes.

**Working answer:** keep this split. Most real-world DH usage is named-group; arbitrary-prime is the long tail.

**Open question:** the modp1 / modp2 (768/1024-bit) groups — block by default or accept?

**Working answer:** block by default; opt-in via `--insecure-dh-groups` flag. Same precedent as Node's CVE response to LogJam.

### XVII.6. Sync KDF policy — block V8 thread or always offload?

D-N5 says: `pbkdf2Sync` / `scryptSync` block V8 thread. The user opted in.

**Working answer:** honour the user's choice. A creator app that calls `pbkdf2Sync(..., 1_000_000, ...)` from a request handler is doing something wrong; that's a server-design issue, not a runtime issue.

**Re-open if:** a creator app surfaces measurable user-visible latency. Mitigation: warn in logs when a `*Sync` KDF takes > 10 ms.

### XVII.7. X.509 verification — ship in Stage 2 or punt to npm?

D-N20 says: parse-only in Stage 2; full verification deferred to a userspace `@zeroship/x509-verify` npm package.

**Working answer:** parse-only. Most npm usage of X509Certificate is JWT verification (parsing the cert from a JWKS endpoint to extract `publicKey`); verify-against-CA-chain is rare.

**Re-open if:** more than 2 creator apps ask for chain verification.

### XVII.8. `crypto.signal` (Node ≥17) — never or maybe?

D-N is implicit (NEVER table). Used by the experimental `cryptoStream` API.

**Working answer:** never. Stays experimental in Node; we don't track experimental APIs.

### XVII.9. CryptoKey `extractable: false` — bridge or refuse?

The WebCrypto `extractable: false` flag prevents export. But `KeyObject.from(cryptoKey)` shares the Arc material, and `keyObject.export(...)` would extract bytes — bypassing `extractable: false`.

**Working answer:** check `extractable` at `KeyObject.from`. If false, throw `ERR_CRYPTO_INVALID_KEY_OBJECT_TYPE` with a message about the source CryptoKey being non-extractable. This preserves the WebCrypto invariant.

**Counter-argument:** Node's `KeyObject.from(cryptoKey)` doesn't check; in Node, all CryptoKeys can be wrapped. The WebCrypto `extractable: false` is honoured by `subtle.exportKey` only.

**Settled:** match Node; let `KeyObject.from(non_extractable_crypto_key)` succeed but make `keyObject.export(...)` honor extractable (throw `ERR_CRYPTO_INVALID_KEY_OBJECT_TYPE` if `keyObject.material` came from a non-extractable CryptoKey). Add an `extractable` field to KeyObjectState that propagates from CryptoKeyState on bridge.

### XVII.10. Algorithm canonicalisation — case-insensitive everywhere or strict?

D-N18 says: case-insensitive (Node behaviour).

**Working answer:** case-insensitive for hash names / cipher names; strict for spec-canonical names in JWK / WebCrypto. The asymmetry is unavoidable because WebCrypto IS strict (per webcrypto-native D-8) and Node IS loose. Document.

## XVIII. Sources

- Node.js `node:crypto` reference — https://nodejs.org/api/crypto.html
- Node.js `crypto.webcrypto` — https://nodejs.org/api/webcrypto.html
- Node.js Buffer — https://nodejs.org/api/buffer.html
- Node.js `errors` module (the ERR_* code list) — https://nodejs.org/api/errors.html
- W3C Web Cryptography API Level 2 — https://w3c.github.io/webcrypto/ (already shipped)
- WHATWG Encoding — https://encoding.spec.whatwg.org/
- RFC 8017 — PKCS #1 v2.2 — https://www.rfc-editor.org/rfc/rfc8017
- RFC 5208 / 5958 — PKCS #8 — https://www.rfc-editor.org/rfc/rfc5208 / https://www.rfc-editor.org/rfc/rfc5958
- RFC 5280 — X.509 v3 — https://www.rfc-editor.org/rfc/rfc5280
- RFC 7468 — PEM textual encoding — https://www.rfc-editor.org/rfc/rfc7468
- RFC 7914 — scrypt — https://www.rfc-editor.org/rfc/rfc7914
- RFC 5869 — HKDF — https://www.rfc-editor.org/rfc/rfc5869
- RFC 8018 — PKCS #5 v2.1 (PBKDF2) — https://www.rfc-editor.org/rfc/rfc8018
- RFC 7517 — JWK — https://www.rfc-editor.org/rfc/rfc7517
- RFC 7518 — JWA (JWK fields per algorithm) — https://www.rfc-editor.org/rfc/rfc7518
- RFC 4648 — base64 / base64url / hex — https://www.rfc-editor.org/rfc/rfc4648
- RFC 5915 — EC private key (SEC1) — https://www.rfc-editor.org/rfc/rfc5915
- RFC 8032 — EdDSA — https://www.rfc-editor.org/rfc/rfc8032
- RFC 7748 — X25519 / X448 — https://www.rfc-editor.org/rfc/rfc7748
- RFC 7693 — BLAKE2 — https://www.rfc-editor.org/rfc/rfc7693
- RFC 8439 — ChaCha20-Poly1305 — https://www.rfc-editor.org/rfc/rfc8439
- RFC 3526 — MODP DH groups — https://www.rfc-editor.org/rfc/rfc3526
- RFC 7919 — Negotiated FFDHE groups — https://www.rfc-editor.org/rfc/rfc7919
- NIST SP 800-38A / D — Block cipher modes — https://csrc.nist.gov/publications/detail/sp/800-38a/final
- FIPS 180-4 — Secure Hash Standard — https://csrc.nist.gov/publications/detail/fips/180/4/final
- aws-lc-rs — https://docs.rs/aws-lc-rs/
- aws-lc — https://github.com/aws/aws-lc
- Reference impls:
  - workerd — https://github.com/cloudflare/workerd/tree/main/src/node/internal (`crypto.h`, `crypto_dh.c++`, `crypto_keys.c++`, `crypto_hkdf.c++`, `crypto_pbkdf2.c++`, `crypto_x509.c++`)
  - Bun — https://github.com/oven-sh/bun/tree/main/src/bun.js/node (`node_crypto.zig` + `src/js/node/crypto.ts`)
  - Deno — https://github.com/denoland/deno/tree/main/ext/node/ops/crypto + `ext/node/polyfills/internal/crypto/`
  - Node.js — https://github.com/nodejs/node/tree/main/src/crypto + `lib/internal/crypto/`
- Sibling designs:
  - `docs/proposals/webcrypto-native.md` — the WebCrypto sibling (already shipped)
  - `docs/proposals/streams-native.md`
  - `docs/proposals/fetch-native.md`
- Project AGENTS.md — `/home/ruiyang/Projects/appbase/AGENTS.md`
- Existing JS shim being replaced: `sdks/vite-plugin/src/node-compat.ts:67-127`
- Existing Rust ad-hoc callbacks being deleted: `crates/runtime/src/crypto.rs:128-212` + `crates/runtime/src/init.rs:1444-1453`
- Existing WebCrypto module: `crates/runtime/src/crypto_native/`
- Macro internals: `crates/runtime-macros/src/v8_class.rs`, `crates/runtime-macros/src/lib.rs`, `crates/runtime-macros/TODO.md`
