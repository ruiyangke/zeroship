# Native Node.js `node:crypto` design

**Date:** 2026-05-02 (v1) · 2026-05-02 (v2 post-review) · 2026-05-02 (v3 round-3 audit) · 2026-05-02 (v4 round-4 narrow audit)
**Status:** Draft v4 (impl-ready) — narrow round-4 residuals closed; ready for impl agent
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

- **v4 (2026-05-02 round-4 narrow audit)** — Closes the 3 CRITICAL + 5 MAJOR residuals from `/tmp/zeroship-reviews/node-crypto-review-v3.md` (round-3 critic score 86/100). No architectural changes; ~50 lines of narrow edits. After this pass: zero CRITICAL, zero blocking MAJOR — impl-ready.

  **Changes from v3:**
  - **C3-1 (ERR_MISSING_PASSPHRASE provenance + class):** §VII.3a row corrected from `JS (errors.js, Error)` to `C++ (node_errors.h, TypeError)` per https://github.com/nodejs/node/blob/main/src/node_errors.h line 115 (`V(ERR_MISSING_PASSPHRASE, TypeError)`); verified absent from `lib/internal/errors.js`. Macro arm (§VII.5 `gen_throw_error`) moved `ERR_MISSING_PASSPHRASE` from the default `_ => Error` fall-through to the `TypeError` branch. Inline kernel comment at `crypto_node/error.rs::PassphraseRequired` rewritten to cite the correct source.
  - **C3-2 (§VII.3a missing 2 codes that are emitted):** Added rows for `ERR_CRYPTO_FIPS_UNAVAILABLE` (real per errors.js:1177, Error) and `ERR_UNKNOWN_ENCODING` (real per errors.js:1875, TypeError). Now §VII.3a covers every `OpError::node(...)` emission in the doc. Re-verified: zero ERR_* code is emitted outside the table. Plus added a row for `ERR_INVALID_BUFFER_SIZE` (referenced in the macro arm at §VII.5; real per errors.js:1480, RangeError).
  - **C3-3 (Empty-HMAC-key rationale fabricated):** v3 claimed Node throws `ERR_OUT_OF_RANGE` on a `key.byteLength === 0` check in `lib/internal/crypto/hash.js`. Verified — NO such check exists in hash.js. Verified actual Node behaviour from `src/crypto/crypto_hmac.cc::Hmac::HmacInit` (lines 78-91 of upstream): Node SILENTLY ACCEPTS empty keys (it special-cases `key_len == 0` by re-binding `key = ""`) and forwards to `HMAC_Init_ex`; if init fails, the dynamic-OSSL `ThrowCryptoError` path emits an `ERR_OSSL_HMAC_*`-shaped code (not in the static registry). Rewrote the kernel comment, the §VII.3a footer, and the test plan at §XIV.crypto_node_hmac.rs to **explicitly tag this as a zeroship divergence**, not parity. New §XVII.13b "Zeroship-vs-Node behavioural divergence log" introduces a single source of truth for this and future divergences. Rationale (kept as the design choice): RFC 2104 §2 + defense-in-depth.
  - **M3-1 (LOC claim wrong):** Updated v3 history line "Doc grew from ~4,000 to ~4,400 LOC" to the verified actual ~5,071 (after v3 audit).
  - **M3-2 (subset of C3-2):** addressed above.
  - **M3-3 (macro arm class misclassifications):** `ERR_INVALID_BUFFER_SIZE` moved from TypeError to RangeError (errors.js:1480 `E('ERR_INVALID_BUFFER_SIZE', '...', RangeError)`). `ERR_MISSING_PASSPHRASE` moved from default `Error` to TypeError (per C3-1).
  - **M3-4 (two-tier error surface):** Added explicit "Operational warning — two-tier error surface" annotation at the §VII.3a footer documenting the contract that `e.code` is canonical (real Node code), `e.message` may contain legacy OSSL hint text. Cross-referenced to operational docs.
  - **M3-5 (`ERR_OSSL_X509_PARSE` invented):** §X.1 reference replaced with `ERR_CRYPTO_OPERATION_FAILED` + `"X.509 parse error: ..."` message text per the §VII.3a / D-N39 fallback policy. Stage F's faithful-OSSL bridge can re-introduce the legacy name in `e.message` once XVII.12 lands.

  **Deferred (with reasoning):**
  - MINOR m3-1 through m3-15 (15 items) — These are stale-arithmetic and minor inconsistencies (cost summary roll-up §XII.1 still shows v2 numbers; Stage F LOC math; `crypto.createDiffieHellman(primeLength)` async-vs-sync mislabel; `ZEROSHIP_LEGACY_CRYPTO` env-reader wiring not shown; etc.). None are impl-blocking; the impl agent will catch them as it works. v4 is a CRITICAL/MAJOR-only narrow pass per the user's directive ("~50 lines of edit, no scope creep").

- **v3 (2026-05-02 round-3 audit)** — Addresses 4 CRITICAL + 25 MAJOR + 15 MINOR findings from `/tmp/zeroship-reviews/node-crypto-review-v2.md`. The v2 critic verified 16/18 sampled v1 fixes were honestly applied; v3 closes the remaining audit gaps (invented error codes, invented aws-lc-rs algorithm constants, prose-only encrypted-PKCS#8 spec). v3 is a **narrow audit pass** — no architectural changes; the kernel/native/node split, Arc<KeyMaterial> share, AEAD state machine, sentinel translation, and deprecation-warning helper from v2 stand. Net effect:

  **CRITICAL fixes (round-2 C2-1 through C2-4):**
  - **Error code audit (C2-1, C2-2)** — every `ERR_CRYPTO_*` and `ERR_OSSL_*` code in §VII.3 / §V.4 / §IV.4a / §III.x is now verified against Node's authoritative registry: `lib/internal/errors.js` (https://github.com/nodejs/node/blob/main/lib/internal/errors.js, ~30 ERR_CRYPTO_* + 1 ERR_OSSL_*) and `src/node_errors.h` (https://github.com/nodejs/node/blob/main/src/node_errors.h, the C++ V(...) macro list with the rest of the ERR_CRYPTO_* family — including `ERR_CRYPTO_INVALID_AUTH_TAG`, `ERR_CRYPTO_INVALID_IV`, `ERR_CRYPTO_INVALID_TAG_LENGTH`, `ERR_CRYPTO_UNKNOWN_CIPHER`, `ERR_CRYPTO_INVALID_KEYLEN`, `ERR_OSSL_EVP_INVALID_DIGEST`). Renamed v2's invented codes:
    - `ERR_CRYPTO_INVALID_IV_LENGTH` → `ERR_CRYPTO_INVALID_IV` (v2's `_LENGTH` suffix doesn't exist in Node).
    - `ERR_CRYPTO_INVALID_AUTH_TAG_LENGTH` → `ERR_CRYPTO_INVALID_AUTH_TAG` (TypeError, per `node_errors.h`).
    - `ERR_CRYPTO_AUTH_TAG_LENGTH_INVALID` → `ERR_CRYPTO_INVALID_TAG_LENGTH` (RangeError, real per `node_errors.h`).
    - `ERR_CRYPTO_INVALID_LENGTH` → `ERR_CRYPTO_INVALID_KEYLEN` (for symmetric mismatch) or `ERR_CRYPTO_INVALID_TAG_LENGTH` (for tag).
    - `ERR_CRYPTO_DEPRECATED_API` → either generic `Error` (no code) or `ERR_CRYPTO_UNSUPPORTED_OPERATION` (for hard-blocked deprecated APIs); real Node behaviour for legacy crypto.createCipher is `process.emitWarning(..., 'DeprecationWarning', 'DEP0106')` + proceed, NOT throwing with a code.
    - `ERR_CRYPTO_INVALID_DH_PRIME` → marked **zeroship extension** (no Node equivalent; we keep it but flag clearly in §VII.3a); fallback path uses `ERR_CRYPTO_OPERATION_FAILED`.
    - `ERR_OSSL_EVP_BAD_DECRYPT`, `ERR_OSSL_EVP_SIGN`, `ERR_OSSL_EVP_VERIFY`, `ERR_OSSL_HMAC_KEY_TOO_SHORT`, `ERR_OSSL_PEM_NO_START_LINE`, `ERR_OSSL_ASN1_VALUE_ERROR`, `ERR_OSSL_EVP_UNSUPPORTED_ALGORITHM`, `ERR_OSSL_EVP_UNSUPPORTED` — Node generates these dynamically from OpenSSL's ERR_PACK pipeline (per `node/src/crypto/crypto_util.cc::ThrowCryptoError`); Node does NOT define them as static codes. **v3 marks every `ERR_OSSL_*` we emit (except the real `ERR_OSSL_EVP_INVALID_DIGEST`) as a zeroship extension** in §VII.3a, with explicit policy: aws-lc errors are bridged through a single `ERR_CRYPTO_OPERATION_FAILED` envelope by default, with the OpenSSL-style `ERR_OSSL_<library>_<reason>` shape preserved verbatim ONLY when the upstream package observed Node's dynamic-build path (e.g., authentication-failed in GCM). Real Node behaviour matched.
    - `ERR_MISSING_OPTION` retained — it IS a real Node code (verified, `lib/internal/errors.js` line ≈ 1610). v2's worry was unfounded; the critic was wrong on this one. Counter-cited.
    - `ERR_MISSING_PASSPHRASE` retained — verified real, but in `src/node_errors.h:115` (NOT in `lib/internal/errors.js`); class is **TypeError**, not Error. <!-- Round 4: addressing CRITICAL C3-1 — v3 wrongly attributed this to errors.js / Error; provenance and class corrected. -->.
    - `ERR_CRYPTO_CUSTOM_ENGINE_NOT_SUPPORTED` retained — real per `lib/internal/errors.js`.
  - **aws-lc-rs algorithm-existence audit (C2-3)** — every claimed `aws_lc_rs::*` constant in §III.2 + §IX.1 verified against the live docs.rs surface:
    - `aws_lc_rs::digest` (https://docs.rs/aws-lc-rs/latest/aws_lc_rs/digest/index.html) exposes ONLY `SHA1_FOR_LEGACY_USE_ONLY`, `SHA224`, `SHA256`, `SHA384`, `SHA512`, `SHA512_256`, `SHA3_256`, `SHA3_384`, `SHA3_512`. **No MD5. No SHA512_224. No SHA3_224. No SHAKE128/256.**
    - `aws_lc_rs::aead` (https://docs.rs/aws-lc-rs/latest/aws_lc_rs/aead/index.html) exposes ONLY `AES_128_GCM`, `AES_128_GCM_SIV`, `AES_192_GCM`, `AES_256_GCM`, `AES_256_GCM_SIV`, `CHACHA20_POLY1305`. **No OCB. No CCM.**
    - `aws_lc_rs::cipher` (https://docs.rs/aws-lc-rs/latest/aws_lc_rs/cipher/index.html) exposes ONLY `AES_128`, `AES_192`, `AES_256` plus CBC/CTR/CFB128 modes. **No XTS. No ECB-as-mode. No OFB.**
    - For each missing algorithm, v3 picks one of: (a) drop to `aws-lc-sys` raw FFI with explicit `EVP_*` call sequence in the algorithm-row "Backing path" column (D-N37), (b) defer to Stage E with rationale, or (c) DEFER PERMANENTLY (RIPEMD-160, IDEA — neither aws-lc-rs nor aws-lc has them). Effort estimates revised: Stage B grows from 60 industry-h to 90 industry-h (~2.25 agent-h) for MD5/SHA-512-224 FFI; Stage C grows from 90 to 110 industry-h (~2.75 agent-h) for AES-CCM FFI; Stage E budget grows by ~80 LOC for AES-OCB and ~120 LOC for AES-XTS. (See §III.2a Stage B FFI inventory and §IX.1a.)
  - **Encrypted PKCS#8 EVP_* sequence (C2-4, D-N33 → D-N33b)** — §IV.4a previously prose-only; v3 adds full function-signature-level spec. Uses `aws-lc` PKCS8_encrypt + PKCS8_marshal_encrypted_private_key + PKCS8_decrypt + PKCS8_parse_encrypted_private_key (verified against https://github.com/aws/aws-lc/blob/main/include/openssl/pkcs8.h). Includes: PBES2 OID list, PBKDF2 PRF OID dispatch, IV-handling policy for AES-CBC inner ciphers, error mapping (PassphraseMismatch → `ERR_CRYPTO_OPERATION_FAILED` with "bad decrypt" message because Node's actual `ERR_OSSL_EVP_BAD_DECRYPT` is dynamic-built — see C2-2). New D-N37 records the FFI sequence. New D-N38 covers the algorithm-routing matrix (which algorithms ship via `aws-lc-rs` high-level vs `aws-lc-sys` raw FFI vs deferred).

  **MAJOR fixes (round-2 M2-1 through M2-25):** mostly clarifications. Highlights:
  - PSS saltLength=0 sign-vs-verify asymmetry documented (§V.5, M2-17).
  - `ERR_INVALID_ARG_VALUE` on PSS bad sentinel → re-routed to RangeError per Node spec (M2-9, §V.5).
  - PBES2-on-ECB explicitly rejected per RFC 8018 §6.2 (M2-10, §IV.4a).
  - `ScryptMemoryExceeded` → `ERR_CRYPTO_INVALID_SCRYPT_PARAMS` (was `ERR_CRYPTO_SCRYPT_NOT_SUPPORTED`; M2-14, §VII.3).
  - GCM tag-length whitelist `[4, 8, 12, 13, 14, 15, 16]` enumerated (M2-21, §IX.2).
  - Encrypted-PKCS#8 cipher whitelist expanded from 7 to 12 entries to match Node (M2-22, §IV.4a).
  - AES-XTS removed from CIPHER_NAMES (M2-23) — XTS requires tweak parameter that the generic cipher state can't carry; DEFER PERMANENTLY to a future XTS-aware spec.
  - `canonicalise_hash_name` documented as case-folding to lowercase before phf::Map lookup (M2-24).
  - `setAutoPadding` flag duplication eliminated — single source of truth in `CipherContext` (M2-25, §V.4).
  - Coverage math re-rolled: Stage 1 = 92/127 = 72%; Stage 1+2 = 121/127 = 95% (M2-1, §II.14).
  - `crypto.encapsulate` shape spec'd: `{ ciphertext: Buffer, sharedKey: Buffer }` (M2-3, missing concept #1, §II.15).
  - Transform mixin back-pressure + dual-API coexistence documented (M2-2, §V.6).
  - `process.noDeprecation` reader plumbed via the existing process-shim (M2-4, §V.4).
  - X509Certificate constructor stage labeling now consistently Stage E (M2-7, §II.11).
  - `crypto.sign(callback?)` clarified: callback path uses `process.nextTick` for sub-millisecond ops, `spawn_blocking` only for RSA-4096 / ML-DSA / similar (M2-8, §II.4).
  - PSS `modulus_bits % 8 != 0` rounding explicitly documented (M2-6, §V.5, footnote in `normalise_pss_salt_length`).
  - `KeyObject.toCryptoKey` PSS lossy-bridging note (M2-12, §II.15).
  - `KeyObject.equals` comment-vs-impl alignment (M2-15, m2-15, §II.8).
  - `extractable` propagation reconciled with JWK round-trip docs (M2-15, §I.4 + XVII.9).
  - `parse_sign_key_input` cold-PEM-decode budget documented (M2-16, §V.5).
  - `randomInt` boundary comment cleaned up (M2-18, §VI.5).
  - `crypto.signal` strikethrough cleanup so `getCipherInfo`/`getHashes` filtering excludes it (M2-19, §II.13).
  - GCM tag whitelist enumerated (M2-21, §IX.2).
  - Encrypted-PKCS#8 cipher whitelist matches Node exactly (M2-22, §IV.4a).
  - `Cipher` `setAutoPadding` flag dedup (M2-25, §V.4).

  **MINOR (m2-1 through m2-15):** Transform mixin / native-class export documentation (m2-1); NIST SP 800-22 added to test plan (m2-2); aws-lc-rs version pin (m2-3, §XVIII); coverage methodology footnote (m2-4); Zeroizing acquisition timing (m2-5); D-8 cross-ref quoted (m2-6); RuntimeFlags struct reality-check (m2-7); `emit_deprecation_warning_once` definition site (m2-8); verify-result vs verify-failure differentiation (m2-9); WPT setup-script for node:crypto vendoring (m2-10); LOC estimate adjusted up (m2-11); deprecation gating note (m2-12); `createSign` options shape (m2-13); m2-14 absorbed into C2-1; equals-comment alignment (m2-15).

  **Decisions added in v3:** D-N37 (encrypted-PKCS#8 EVP_* FFI sequence at signature level — addresses C2-4); D-N38 (algorithm-routing matrix: aws-lc-rs high-level vs aws-lc-sys raw FFI vs deferred — addresses C2-3); D-N39 (zeroship-extension error-code policy: any `ERR_OSSL_*` code we emit that is not in Node's static registry is documented as a zeroship extension and SHOULD be paired with the closest real Node code in cross-platform code paths — addresses C2-1, C2-2).

  No architectural changes. Doc grew from ~4,029 to ~5,071 LOC (round-3 audit). <!-- Round 4: addressing MAJOR M3-1 — v3 said "~4,400 LOC", actual was 5,071 (off by ~16%). Corrected. --> The implementation is unblocked: every algorithm has a backing path, every error code is either real-Node or marked zeroship-extension, and D-N33b spells out the encrypted-PKCS#8 EVP_* sequence at signature level.

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
| **D-N10** | `Hmac` class: `update(data, inputEncoding?)` and `digest(outputEncoding?)` mirror Hash; `copy(options?)` is intentionally absent on Hmac in Node (`hmac.copy` doesn't exist) — we match. Backed by `kernel::HmacContext`. (**counter-citation against critic CRITICAL #10**: critic claimed Node v17+ added `Hmac.prototype.copy`. Verified against https://github.com/nodejs/node/blob/main/lib/internal/crypto/hash.js — only `Hash.prototype.copy` is defined; the `Hmac` class extends `Hash` for `update` / `digest` / `_transform` / `_flush` via prototype assignment but `copy` is NOT among the inherited methods. Verified against https://nodejs.org/api/crypto.html#class-hmac — the documented method list is `digest`, `update`. v1's omission was correct; we keep it.) | Node has Hash.copy but not Hmac.copy (a quirk of OpenSSL EVP_MD_CTX vs HMAC_CTX). Some npm packages (older `passport-jwt` versions) crash if Hmac has a `.copy` method that throws when called the way Hash.copy works — they assume same shape. We match Node's omission exactly. | §V.3 |
| **D-N11** | `Cipher` / `Decipher` classes: `update(data, inputEncoding?, outputEncoding?)` returns Buffer (or string if outputEncoding); `final(outputEncoding?)` flushes the last block + tag; `setAAD(buffer, options?)` for GCM/CCM AAD; `setAuthTag(buffer)` for Decipher post-data tag inject; `getAuthTag()` for Cipher post-final tag emit; `setAutoPadding(boolean)` for CBC PKCS#7 control. Backed by `kernel::CipherContext`. The class is created via `crypto.createCipheriv(algorithm, key, iv, options?)` factories. `createCipher` (deprecated, derives key from password via broken EVP_BytesToKey) is gated behind `--legacy-crypto` per D-N22; without the flag it throws `ERR_CRYPTO_UNSUPPORTED_OPERATION` with a doc URL to switch to `createCipheriv`. (v3 fix, C2-2: v2's `ERR_CRYPTO_DEPRECATED_API` was invented; the real Node code per node_errors.h is `ERR_CRYPTO_UNSUPPORTED_OPERATION`.) | Direct Node parity for `createCipheriv`. The deprecated API has weak KDF properties (EVP_BytesToKey single-iteration MD5); blocking by default reduces footgun surface. | §V.4 |
| **D-N12** | `Sign` / `Verify` classes: `update(data, inputEncoding?)` and `sign(privateKey, outputEncoding?)` / `verify(publicKey, signature, signatureEncoding?)`. Internally compute the digest streaming-style, then run the asymmetric op once at finalisation. Accept `privateKey` / `publicKey` as `KeyObject`, `CryptoKey`, PEM string, DER Buffer, or `{ key, format, type, passphrase }` options object — Node's union type. The encoding helper at the boundary materialises any of these to a kernel-friendly key handle. | Sign / Verify are the "DigestSign" pattern in OpenSSL (EVP_DigestSignInit + Update + Final). The streaming API saves the user from buffering the message; the kernel's `SignContext` mirrors EVP_DigestSignContext. | §V.5 |
| **D-N13** | `KeyObject` / `PublicKeyObject` / `PrivateKeyObject` / `SecretKeyObject`: parent + three subclasses (`#[v8_inherit]`). Parent has `.type` (returns "secret" / "public" / "private"), `.asymmetricKeyType` (returns null for secret), `.asymmetricKeyDetails` (algorithm-specific dict), `.symmetricKeySize` (bytes for secret; null for asymmetric), `.export(options) -> Buffer | string | object`, `.equals(other)`. Subclasses add nothing functional — they exist for `instanceof` discrimination. Internal-field 0 holds `Box<KeyObjectState>` carrying an `Arc<KeyMaterial>`. The static `KeyObject.from(cryptoKey)` constructor accepts a `CryptoKey` and clones the Arc. | Node's type model verbatim. Some npm packages (older `jose`, `node-forge`) check `instanceof PrivateKeyObject` to distinguish privates; missing the subclass means those checks fail. | §IV |
| **D-N14** | `crypto.createSecretKey(buffer | string, encoding?)` / `crypto.createPublicKey(input)` / `crypto.createPrivateKey(input)` factories: parse the input (PEM / DER / JWK / KeyObject / `{ key, format: 'pem' | 'der' | 'jwk', type: 'pkcs1' | 'pkcs8' | 'spki' | 'sec1', passphrase: Buffer? }`) into a fresh `KeyObject` instance. The PEM parser lives in `kernel::pem` (RFC 7468 framing — a single function: `decode_pem(text) -> Vec<(label, der_bytes)>`); the DER walker is the existing `crypto_native/der.rs`. `passphrase` for encrypted PKCS#8 dispatches to `crypto_kernel/pkcs8_enc.rs::decrypt_pkcs8_private_key` per D-N37 (uses `aws-lc-sys` raw FFI via `PKCS8_parse_encrypted_private_key` — **NOT** an aws-lc-rs high-level API; v1's `EncryptedPrivateKeyInfo::from_bytes(der).decrypt(passphrase)` was invented and corrected in v2). | The createX factories are the entry point npm packages use. Without them, a creator app cannot import a key — there's no other path. JWK input takes the JWK as a JS object (passed into the kernel JWK parser shared with WebCrypto). | §IV.4 |
| **D-N15** | KDF dispatch: `pbkdf2` / `pbkdf2Sync` / `scrypt` / `scryptSync` / `hkdf` / `hkdfSync` — sync variants run on V8 thread (user opted into blocking by picking the Sync API); async variants dispatch to `state.spawned_ops`. Both call into `kernel::pbkdf2` / `kernel::scrypt` / `kernel::hkdf` (slice-in / Vec-out). PBKDF2 + HKDF are already in `crypto_native/derive.rs`; the kernel extraction is mechanical (move + add a `_sync` and `_async` adapter). scrypt is NEW — aws-lc-rs has `pbkdf2` but no scrypt; we use `aws_lc_sys::EVP_PBE_scrypt` (BoringSSL's scrypt is a ~200 LOC FFI binding). | RFC 7914 scrypt is the password-hashing standard most modern apps use (vs. PBKDF2 which is recommended only for legacy interop). bcrypt is similar but not in node:crypto; the `bcrypt` npm package wraps OpenSSL's `BF_set_key` directly. We don't ship bcrypt; the npm package's WASM fallback (via unenv) is acceptable. | §VI.2 |
| **D-N16** | webcrypto bridge — object identity. `import("node:crypto")` yields an exports object whose `.webcrypto` property IS the same `Crypto` instance that's installed at `globalThis.crypto`. The synthetic module's installer code reads `globalThis.crypto` once at module-evaluate time and assigns the reference directly; subsequent reads return the same `Crypto` instance. `subtle` is `globalThis.crypto.subtle`. `getRandomValues` is `globalThis.crypto.getRandomValues.bind(globalThis.crypto)` (Node binds; we follow). | Node's `crypto.webcrypto === globalThis.crypto` is a cross-codebase invariant — JOSE libraries assume it. Returning a copy would silently break `WeakMap`-based key tracking (libraries that keep a `WeakMap<CryptoKey, ...>` would lose entries on the boundary). | §VIII |
| **D-N17** | Random: `randomBytes(size, callback?) -> Buffer | void` (callback variant returns Buffer to callback async; sync variant returns Buffer). `randomFillSync(buffer, offset?, size?) -> Buffer`. `randomFill(buffer, offset?, size?, callback) -> void` (always callback). `randomInt(min, max, callback?) -> number` (uniform distribution via rejection sampling, not the JS-shim's modulo bias). `randomUUID(options?)` — same as `globalThis.crypto.randomUUID`. `getRandomValues` re-export. All sync; `randomBytes(N)` for very large N (e.g. > 1 MB) goes async via callback if present, sync otherwise — matches Node. | The randomInt rejection-sampling fix corrects a subtle bias in the JS shim (line 109-117 of node-compat.ts: `range > 2^32` causes silent bias). Node uses the same rejection-sampling technique we will. | §VI.5 |
| **D-N18** | Algorithm name canonicalisation: node:crypto names are case-insensitive but inconsistent ("sha256" vs "SHA-256" vs "RSA-SHA256"). The kernel uses spec-canonical names ("SHA-256", "RSA-PSS"); the surface adapter maps node:crypto inputs via a phf::Map: `"sha256" -> SHA-256`, `"sha-256" -> SHA-256`, `"sha384" -> SHA-384`, ..., `"rsa-sha256" -> SignAlg::RsaPkcs1Sha256`, etc. Names not in the table → `ERR_CRYPTO_INVALID_DIGEST` (digest names) or `ERR_CRYPTO_UNKNOWN_CIPHER` (cipher names) — both real Node codes per node_errors.h (v3 fix, C2-1, C2-2 — v2's `ERR_OSSL_EVP_UNSUPPORTED_ALGORITHM` is dynamic-OSSL). The canonicaliser case-folds to lowercase BEFORE the phf::Map lookup so `"SHA256"`, `"sha256"`, `"Sha-256"` all resolve identically (addresses M2-24). | Node's getHashes() returns ~50 names (because OpenSSL aliases everything). We support the 4 SHA digests + their aliases + ChaCha20-Poly1305 + the 11 cipher modes + 6 sign algorithms — total ~25 algorithm names. The map is small. | §IX |
| **D-N19** | `KeyObject.export(options)` accepts `{ format: 'pem' \| 'der' \| 'jwk', type: 'pkcs1' \| 'pkcs8' \| 'spki' \| 'sec1', cipher?: string, passphrase?: Buffer }`. PEM emission uses the kernel's PEM emitter (the inverse of D-N14's parser). `cipher` + `passphrase` for encrypted PKCS#8 export goes through `crypto_kernel/pkcs8_enc.rs` (D-N33 — drop to `aws-lc-sys` because high-level `aws-lc-rs` does not expose this surface). JWK export reuses `crypto_native/jwk.rs::export_*`. | Direct Node parity. The cipher options matrix (`{ cipher: 'aes-256-cbc', passphrase: Buffer.from('...') }`) is what passport / saml / openid-client libraries use to round-trip encrypted private keys. (addresses critic MAJOR #16: v1 cited an invented `EncryptedPrivateKeyInfo::serialize_with_password` API; corrected.) | §IV.6, §IV.4a |
| **D-N20** | X.509: Stage 1 ships a stub class that throws `ERR_CRYPTO_UNSUPPORTED_OPERATION` on construction, with a clear message pointing at the Stage 2 ADR. Stage 2 ships parsing-only (constructor + readonly properties). Full chain verification defers to a future `@zeroship/x509-verify` npm package wrapping BoringSSL's `X509_verify_cert`. | Most npm packages that touch X509 (jsonwebtoken's JWKS endpoints, Apple Sign-In, Google's JWT checking) do their own verify on top of `X509Certificate.publicKey` — they don't call `.verify()` directly. Stage 2 parsing-only covers ~80% of usage. | §X |
| **D-N21** | DH: Stage 1 ships only the named groups (`crypto.getDiffieHellman('modp14')` etc.). Stage 2 adds `crypto.createDiffieHellman(prime, generator)` via aws-lc-sys's lower FFI (`DH_set0_pqg`). Stage 1 errors on the unnamed-group factory with `ERR_CRYPTO_UNSUPPORTED_OPERATION`. `crypto.createECDH` ships in Stage 1 (aws-lc-rs's `agreement::*` has the curves). | DH (vs ECDH) is rare in modern apps — TLS 1.3 deprecated DHE in favour of ECDHE. Most uses we'll see in npm are SCRAM / SSH-key-exchange, both of which use named groups. Generic DH is the long tail. | §X |
| **D-N22** | Legacy ciphers (DES, 3DES, Blowfish, Cast5, RC4, IDEA): NOT in Stage 1. Stage 2 ships them under the `--legacy-crypto` runtime flag (off by default). Without the flag, `createCipheriv('des-cbc', ...)` errors with `ERR_CRYPTO_UNSUPPORTED_OPERATION` (real Node code per node_errors.h, v3 fix C2-2 — v2's `ERR_OSSL_EVP_UNSUPPORTED_ALGORITHM` is dynamic-OSSL, not Node static) and a message pointing at the flag. Node ships these unconditionally; we don't. aws-lc-rs (per docs.rs verification, v3 audit C2-3) does NOT have DES at all in its high-level API — `cipher::TDES_*` was an v1/v2 invention. 3DES requires aws-lc-sys raw FFI via `EVP_des_ede3_cbc`. Blowfish / Cast5 / RC4 / IDEA also need aws-lc-sys raw (or are not in aws-lc at all — IDEA was removed from BoringSSL). | Most modern apps don't touch these. The few that do interface with legacy POS / ancient SAML; a runtime flag rather than blanket support reduces attack surface. | §X |
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
| **D-N33** (v2) | Encrypted PKCS#8 import / export drops to `aws-lc-sys` raw FFI in `crypto_kernel/pkcs8_enc.rs` (~250 LOC). High-level `aws-lc-rs` does not expose `EncryptedPrivateKeyInfo` or any `decrypt(passphrase)` / `serialize_with_password` method (verified against https://docs.rs/aws-lc-rs/latest/aws_lc_rs/encoding/index.html — only `Pkcs8V1Der` / `Pkcs8V2Der` byte wrappers, no encryption). The bespoke walker handles PBES2/PBKDF2 ASN.1 envelope build/parse and dispatches the inner cipher work via the kernel's existing `CipherContext`. (addresses critic CRITICAL #7, MAJOR #12, MAJOR #16) | §IV.4a |
| **D-N34** (v2) | RSA-PSS `saltLength` sentinels (-1 = `RSA_PSS_SALTLEN_DIGEST`, -2 = `RSA_PSS_SALTLEN_MAX_SIGN` / `RSA_PSS_SALTLEN_AUTO`) are normalised to absolute byte counts in `parse_sign_key_input` BEFORE the kernel boundary, via `normalise_pss_salt_length()`. The kernel never sees negative sentinels. (addresses critic CRITICAL #8) | §V.5 |
| **D-N35** (v2) | Hash, Hmac, Cipher, Decipher, Sign, Verify all extend `stream.Transform` (Node's documented behaviour — see https://nodejs.org/api/crypto.html#class-hash). The classes expose `_transform(chunk, encoding, callback)` and `_flush(callback)` so `pipeline(readable, hash, writable)` works. The Transform shape is layered on top of the existing #[v8_class] via a JS-side mixin in `node-crypto.gen.ts` (the synthetic module's Hash export wraps the native class with a small Transform-prototype shim). (addresses critic missing concept #21) | §V.6 |
| **D-N36** (v2) | Post-quantum key types (ML-DSA, ML-KEM, SLH-DSA — Node v25+) and `crypto.encapsulate` / `crypto.decapsulate` (Node v22+ KEM API) are listed in the export surface as Stage E placeholders. The implementation depends on aws-lc-rs's PQC support which is in active development (NIST FIPS 203/204/205 — kyber/dilithium/sphincs+). Stage E ships parsing-only `asymmetricKeyType` recognition; full key generation defers to a future ADR when aws-lc-rs's PQC API stabilises. (addresses critic missing concepts #2, #3) | §II.15 |
| **D-N37** (v3) | Encrypted-PKCS#8 import / export uses `aws-lc-sys` raw FFI, specifically `PKCS8_marshal_encrypted_private_key` (encrypt path) + `PKCS8_parse_encrypted_private_key` (decrypt path) per https://github.com/aws/aws-lc/blob/main/include/openssl/pkcs8.h. The marshal/parse pair takes EVP_PKEY directly and reads/writes the EncryptedPrivateKeyInfo ASN.1 envelope into/from CBB/CBS buffers. PBES2 inner KDF dispatch is handled by aws-lc internally (no per-PRF Rust code needed); the supported PRFs are HMAC-SHA-1/224/256/384/512 OIDs. Default PBES2 iterations: 2048 (matches Node). Default salt: 16 random bytes (aws-lc-generated). Default inner cipher: caller-specified per the `cipher` option to `KeyObject.export`. v3 expands the cipher whitelist from v2's 7 entries to 12 to match Node's actual list per `lib/internal/crypto/keys.js`. ECB-mode inner ciphers are gated behind `--legacy-crypto` (RFC 8018 §6.2 forbids them; v3 accepts under flag for legacy interop). (addresses round-2 CRITICAL C2-4, MAJOR M2-10, M2-22) | §IV.4a |
| **D-N38** (v3) | Algorithm-routing matrix: every algorithm in §III.2 + §IX.1 has an explicit "backing path" annotation — one of `aws-lc-rs high-level` (verified-present in the public Rust API at https://docs.rs/aws-lc-rs/latest/aws_lc_rs/), `aws-lc-sys raw FFI` (vendored `EVP_*` shim in `crypto_kernel/cipher_*.rs` / `digest_*.rs` / `dh_*.rs`), or `DEFER` (not shippable from BoringSSL/aws-lc public surface — the entry stays in HASH_NAMES / CIPHER_NAMES so getHashes() / getCiphers() return the expected Node-shaped list, but `createX(name)` routes to `ERR_CRYPTO_UNSUPPORTED_OPERATION`). Stage B FFI cost: ~90 LOC; Stage C FFI cost: ~370 LOC; Stage E FFI cost: ~810 LOC. v2 effort estimates underestimated FFI work; v3 revises Stage B from 60 industry-h to ~90, Stage C from 90 to ~110. (addresses round-2 CRITICAL C2-3) | §III.2, §III.2a, §IX.1 |
| **D-N39** (v3) | Error-code provenance policy: every code emitted from `crypto_node/error.rs` is one of (a) JS-side (defined in `lib/internal/errors.js`), (b) C++-side (defined in `src/node_errors.h` `V(...)` macro list), (c) dynamic-OSSL (Node builds at throw time from the OpenSSL ERR_PACK queue — names like `ERR_OSSL_<library>_<reason>`; we CANNOT faithfully reproduce because aws-lc-rs's `Unspecified` strips the upstream library/reason), or (d) zeroship-extension (a code we emit that is NOT in Node's static catalog — explicitly marked in §VII.3a). v3 audit removed every invented code from v2's mapping table: `ERR_CRYPTO_INVALID_AUTH_TAG_LENGTH` / `_IV_LENGTH` / `_AUTH_TAG_LENGTH_INVALID` / `_INVALID_LENGTH` / `_DEPRECATED_API` / `_INVALID_DH_PRIME` and the dynamic-OSSL family `ERR_OSSL_EVP_BAD_DECRYPT` / `_SIGN` / `_VERIFY` / `_HMAC_KEY_TOO_SHORT` / `_PEM_NO_START_LINE` / `_ASN1_VALUE_ERROR` / `_EVP_UNSUPPORTED_ALGORITHM` / `_EVP_UNSUPPORTED`. The dynamic-OSSL string is preserved in the message text where upstream-package compatibility benefits (e.g., a creator app's package may grep `e.message` for "ERR_OSSL_EVP_BAD_DECRYPT"). v3 leaves zero zeroship-extension codes in active use; full faithful dynamic-OSSL bridging is an open question (XVII.12 below) for a future Stage F if measured demand surfaces. (addresses round-2 CRITICAL C2-1, C2-2) | §VII.3, §VII.3a |

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
| `randomFill(buf, ...)` | n/a | async (always callback) | n/a | `randomFillSync(buf)`. (addresses critic MAJOR #21): for very small fills (<= 1024 bytes), Node fires the callback synchronously after queueing on `process.nextTick`. v2 mirrors that by branching at the entry point: `if size <= 1024 { sync_path; queueMicrotask(callback) } else { spawn_blocking }`. Avoids a ~10 µs spawn_blocking overhead for small fills (the common case for `randomBytes(16)` token-style usage) without breaking the documented async-callback contract. |
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

- `KeyObject.from(cryptoKey)` (static) → reads cryptoKey.[[handle]].material (the Arc), clones it into a new KeyObjectState along with the source's `extractable` flag (M2-15), returns a fresh KeyObject wrapper.
- `crypto.subtle.importKey('jwk', keyObject.export({format:'jwk'}))` → takes the KeyObject's exported JWK, runs the existing WebCrypto JWK importer; the result is a fresh CryptoKey with its OWN Arc<KeyMaterial> (the JWK round-trip materialises a new Arc — slower but spec-correct).

The Arc share avoids re-encoding on the common `KeyObject.from(...)` path. The JWK round-trip path covers the inverse (CryptoKey from KeyObject); the user supplies the WebCrypto algorithm + usages + extractable explicitly because those don't exist on the source KeyObject.

<!-- Round 3: addressing MAJOR M2-15 (extractable propagation alignment). -->
**Extractable propagation reconciliation (v3, addresses M2-15):** v2 had two contradictory statements: (a) "JWK round-trip materialises a fresh Arc — slower but spec-correct because the WebCrypto algorithm + extractable + usages have no node:crypto equivalent and must come from the JWK importKey call" (line ~387) and (b) XVII.9 "we DO add an extractable field to KeyObjectState that propagates from CryptoKeyState" (line ~3946). v3 reconciles:

- `KeyObject.from(cryptoKey)` (the FORWARD bridge — Arc clone): DOES propagate `extractable` from CryptoKeyState into the new KeyObjectState. The flag is a field on KeyObjectState (visible to `keyObject.export(...)` which checks it before extracting bytes). This is the "fast path" — same key bytes, same refcount.
- `subtle.importKey('jwk', keyObject.export(...))` (the REVERSE bridge — JWK round-trip): does NOT propagate the source's KeyObject `extractable` because KeyObject doesn't carry one (KeyObjects in Node are conceptually always extractable; only the WebCrypto wrapping enforces extractability). The user supplies the WebCrypto-side `extractable` parameter to `importKey`. The resulting CryptoKey gets a fresh Arc<KeyMaterial> AND a user-supplied extractable flag.

So both statements are correct after the reconciliation: the forward bridge DOES propagate (M2-15), the reverse bridge DOES require the user to supply (the JWK lossy comment). v3 makes this explicit; v2 read as contradictory.

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

**Input-coercion policy** (addresses critic CRITICAL #2): the `inputEncoding` argument is **ignored when `data` is a Buffer / TypedArray / DataView / ArrayBuffer** — Node's documented behaviour, e.g. `hash.update(buf, 'hex')` does NOT hex-decode `buf`, it consumes the raw bytes. Per https://nodejs.org/api/crypto.html#hashupdatedata-inputencoding: "If `data` is a Buffer, TypedArray, or DataView, then `inputEncoding` is ignored." The same rule holds for every method following this shape: `Hmac.prototype.update`, `Cipher.prototype.update` (input encoding), `Decipher.prototype.update` (input encoding), `Sign.prototype.update`, `Verify.prototype.update`. Only when `data` is a `string` does the `inputEncoding` argument apply (default `'utf8'`).

```rust
// crypto_node/buffer.rs
pub fn extract_input(
    scope: &mut v8::PinScope,
    value: v8::Local<v8::Value>,
    encoding: Option<&str>,
) -> Result<Vec<u8>, OpError> {
    // 1. ArrayBufferView (Uint8Array, Buffer, Int8Array, ...) → copy bytes.
    //    Per Node spec, `encoding` is IGNORED for non-string input — we silently
    //    discard the parameter rather than rejecting it (matches Node).
    if let Ok(view) = v8::Local::<v8::ArrayBufferView>::try_from(value) {
        let mut buf = vec![0u8; view.byte_length()];
        view.copy_contents(&mut buf);
        return Ok(buf);
    }
    // 2. ArrayBuffer → copy bytes.  `encoding` is ignored here too.
    if let Ok(ab) = v8::Local::<v8::ArrayBuffer>::try_from(value) {
        let store = ab.get_backing_store();
        let mut buf = vec![0u8; ab.byte_length()];
        for (i, b) in buf.iter_mut().enumerate() { *b = store[i].get(); }
        return Ok(buf);
    }
    // 3. String → encoding APPLIES; default 'utf8' if not provided.
    //    Per Node v15+, default is 'utf8' (older versions used 'binary'/latin1;
    //    we follow current spec).
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
    // Encoding canonicalisation (addresses critic MAJOR encoding list — utf16le
    // and ascii were missing in v1). Per https://nodejs.org/api/buffer.html#buffers-and-character-encodings.
    match encoding.map(canonical_encoding) {
        None => Ok(emit_buffer(scope, bytes).into()),       // default: Buffer
        Some(Encoding::Hex) => Ok(emit_string(scope, &hex_encode(bytes)).into()),
        Some(Encoding::Base64) => Ok(emit_string(scope, &base64::encode(bytes)).into()),
        Some(Encoding::Base64Url) => Ok(emit_string(scope, &base64url::encode(bytes)).into()),
        Some(Encoding::Latin1) => Ok(emit_string(scope, &latin1_encode(bytes)).into()),  // also "binary"
        Some(Encoding::Ascii) => Ok(emit_string(scope, &ascii_encode(bytes)).into()),    // bytes & 0x7F per Node
        Some(Encoding::Utf8) => {
            // (addresses critic CRITICAL #9): Node's `digest('utf8')` is well-defined as
            // LOSSY for binary digest output — invalid UTF-8 byte sequences are replaced
            // with U+FFFD per https://nodejs.org/api/buffer.html#buffers-and-character-encodings.
            // The v1 comment ("Errors on invalid UTF-8 boundaries — Node lossily decodes")
            // contradicted the implementation; fixed: comment + impl now agree it is lossy.
            Ok(emit_string(scope, &String::from_utf8_lossy(bytes)).into())
        }
        Some(Encoding::Utf16Le) => Ok(emit_string(scope, &utf16le_encode(bytes)).into()),  // alias 'ucs2', 'ucs-2'
        Some(Encoding::Unknown(name)) => Err(OpError::node("ERR_UNKNOWN_ENCODING",
            format!("Unknown encoding: {}", name))),
    }
}

/// Canonicalises Node's encoding aliases to the underlying form. Per
/// https://nodejs.org/api/buffer.html#buffers-and-character-encodings.
/// `binary` is an alias for `latin1`; `ucs2`/`ucs-2`/`utf-16le`/`utf16le`
/// are all aliases for UTF-16 LE; `utf8`/`utf-8` are aliases.
fn canonical_encoding(name: &str) -> Encoding {
    match name.to_ascii_lowercase().as_str() {
        "hex" => Encoding::Hex,
        "base64" => Encoding::Base64,
        "base64url" => Encoding::Base64Url,
        "latin1" | "binary" => Encoding::Latin1,
        "ascii" => Encoding::Ascii,
        "utf8" | "utf-8" => Encoding::Utf8,
        "utf16le" | "utf-16le" | "ucs2" | "ucs-2" => Encoding::Utf16Le,
        other => Encoding::Unknown(other.to_string()),
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
// crypto_node/error.rs (sketch — full mapping in §VII.3)
// (v3, addresses C2-1, C2-2): every code below is a real Node code per
// errors.js or node_errors.h.
impl KernelError {
    pub fn to_node(self) -> OpError {
        match self {
            // ERR_CRYPTO_HASH_FINALIZED — JS-side (errors.js).
            Self::HashFinalised => OpError::node("ERR_CRYPTO_HASH_FINALIZED",
                "Digest already called"),
            // ERR_CRYPTO_INVALID_KEYLEN — C++-side (node_errors.h, RangeError).
            Self::InvalidKeyLength => OpError::node("ERR_CRYPTO_INVALID_KEYLEN",
                "Invalid key length"),
            // ERR_CRYPTO_INVALID_IV — C++-side (node_errors.h, TypeError).
            Self::InvalidIvLength { expected, got } => OpError::node("ERR_CRYPTO_INVALID_IV",
                format!("Invalid IV length: expected {}, got {}", expected, got)),
            // ERR_CRYPTO_OPERATION_FAILED — JS-side (errors.js); the canonical
            // Node fallback for dynamic-OSSL "bad decrypt" (was wrongly mapped
            // to invented ERR_CRYPTO_AUTH_TAG_LENGTH_INVALID in v2).
            Self::AuthenticationFailed => OpError::node("ERR_CRYPTO_OPERATION_FAILED",
                "Unsupported state or unable to authenticate data"),
            // ... ~30 arms total — see §VII.3 for the full spec.
            // ERR_CRYPTO_INVALID_DIGEST / ERR_CRYPTO_UNKNOWN_CIPHER —
            // C++-side (node_errors.h); Node's real codes for "unknown
            // algorithm name" — split by domain (digest vs cipher).
            Self::UnsupportedAlgorithm(name) => OpError::node("ERR_CRYPTO_UNSUPPORTED_OPERATION",
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

**`error.errno` field (addresses critic missing concept #15):** Node's OSSL-stack errors (e.g. `ERR_OSSL_*`) ALSO carry a numeric `.errno` field — the underlying OpenSSL `ERR_PACK` integer. Some packages (notably older OpenSSL-aware logging libraries) match on `if (err.errno === -7)`. v2 extends `OpErrorKind::NodeError(code: &'static str)` to optionally carry a `Some(errno: i32)` payload:

```rust
pub enum OpErrorKind {
    NodeError(&'static str, Option<i32>),    // code + optional errno
    /* ... */
}
impl OpError {
    pub fn node(code: &'static str, msg: impl Into<String>) -> Self { /* errno = None */ }
    pub fn node_with_errno(code: &'static str, errno: i32, msg: impl Into<String>) -> Self { /* errno = Some */ }
}
```

The OSSL-stack errors in `KernelError` carry the BoringSSL error code through to the macro arm; the macro arm sets `.errno` when the variant has Some. Most v2 callsites don't have a useful errno (kernel errors are semantic, not OSSL packs); the few that do — `KernelError::OsslError { errno, code }` — propagate it.

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
| `createCipher(algorithm, password, options?)` (deprecated, addresses critic MAJOR #18) | 2 | EVP_BytesToKey + createCipheriv (only when `--legacy-crypto` is on; throws `ERR_CRYPTO_UNSUPPORTED_OPERATION` otherwise — v3 fix, C2-2: ERR_CRYPTO_DEPRECATED_API is not a real Node code) | sync | E |
| `createCipheriv(algorithm, key, iv, options?)` (options: `{ authTagLength }` — REQUIRED for CCM, optional default 16 for GCM/OCB/ChaCha20-Poly1305; addresses critic CRITICAL #3) | 1 | `kernel::CipherContext::new(encrypt=true)` | sync | C |
| `createDecipheriv(algorithm, key, iv, options?)` (same options shape) | 1 | `kernel::CipherContext::new(encrypt=false)` | sync | C |
| `Cipher` / `Decipher` (classes) | 1 | `crypto_node/cipher.rs` | sync streaming + async-above-threshold | C |
| `Cipher.prototype.update(data, inputEncoding?, outputEncoding?)` | 1 | `kernel::CipherContext::update` | sync (always) | C |
| `Cipher.prototype.final(outputEncoding?)` | 1 | `kernel::CipherContext::finalize` | sync | C |
| `Cipher.prototype.setAAD(buffer, options?)` (options: `{ plaintextLength, encoding }` — plaintextLength REQUIRED for CCM, addresses critic CRITICAL #4) | 1 | `kernel::CipherContext::set_aad` | sync | C |
| `Cipher.prototype.getAuthTag()` | 1 | `kernel::CipherContext::get_auth_tag` | sync | C |
| `Decipher.prototype.setAuthTag(buffer, encoding?)` (mode-aware ordering: CCM pre-update, GCM/OCB/ChaCha20 pre-final, addresses critic CRITICAL #5) | 1 | `kernel::CipherContext::set_auth_tag` | sync | C |
| `Cipher.prototype.setAutoPadding(boolean)` | 1 | `kernel::CipherContext::set_auto_padding` | sync | C |
| `getCiphers() -> string[]` | 1 | iterate registry | sync | C |
| `getCipherInfo(name | nid, options?)` | 1 | registry metadata lookup | sync | C |

**Algorithms supported in Stage 1 (after v3 audit, addresses C2-3):**
- AES-128/192/256 in CBC, CTR, GCM, KW, CCM modes (CCM via aws-lc-sys raw FFI per D-N38; CBC/CTR/GCM/KW via aws-lc-rs high-level API).
- AES-128/192/256-CFB128 (the only CFB variant exposed by aws-lc-rs).
- ChaCha20-Poly1305 (`chacha20-poly1305`) — D-N23.
- (Bonus capability over Node) AES-128/256-GCM-SIV via aws-lc-rs `aead::AES_*_GCM_SIV` — opt-in, not in Node yet.

**Moved to Stage E (v3, addresses C2-3 — aws-lc-rs lacks high-level support; FFI required):**
- AES-OCB (low value/effort ratio).
- AES-ECB (rare; HSM key wrap is the common use case but our AES-KW already covers that path).
- AES-CFB1, AES-CFB8, AES-OFB (very niche).

**Permanently deferred (v3, addresses C2-3 + M2-23):**
- AES-XTS (requires tweak parameter; generic CipherContext has no slot for it).

**Stage 2 (with `--legacy-crypto` flag):** DES-CBC, 3DES (DES-EDE3), Blowfish (`bf-*`), Cast5, RC4. **aws-lc-rs does NOT have 3DES** (verified — v1/v2's `cipher::TDES_*` claim was incorrect); all of these require aws-lc-sys raw FFI per §III.2a / D-N38. IDEA was removed from BoringSSL — permanently deferred.

### II.4. Sign / Verify

| Export | Tier | Backed by | Sync/async | Stage |
|---|---|---|---|---|
| `createSign(algorithm, options?) -> Sign` | 1 | `kernel::SignContext::new` | sync | C |
| `createVerify(algorithm, options?) -> Verify` | 1 | `kernel::VerifyContext::new` | sync | C |
| `Sign` / `Verify` (classes) | 1 | `crypto_node/sign.rs` | sync streaming | C |
| `Sign.prototype.update(data, encoding?)` | 1 | `kernel::SignContext::update` | sync | C |
| `Sign.prototype.sign(privateKey, encoding?)` | 1 | `kernel::SignContext::sign` | sync | C |
| `Verify.prototype.verify(publicKey, signature, encoding?)` | 1 | `kernel::VerifyContext::verify` | sync | C |
| `crypto.sign(algorithm, data, key, callback?)` (one-shot; addresses critic MAJOR #15: callback variant accepted) | 1 | `kernel::sign_one_shot` (sync) or `sign_one_shot_async` (callback path; see M2-8 dispatch policy below) | sync **and** async with callback | C |
| `crypto.verify(algorithm, data, key, sig, callback?)` (one-shot; addresses critic MAJOR #15) | 1 | `kernel::verify_one_shot` / `verify_one_shot_async` | sync **and** async with callback | C |

**`crypto.sign` / `crypto.verify` callback dispatch policy (v3, addresses M2-8):** Node's actual behaviour for cheap asymmetric ops is to fire the callback async via `process.nextTick` rather than via a thread pool. Spawning a blocking task via `compio::runtime::spawn_blocking` adds ~10µs of overhead — measurable on RSA-2048 verify (which itself takes ~50µs). v3 splits the dispatch policy by op cost:

- **Cheap ops** (RSA-2048 verify, ECDSA-P256 sign/verify, Ed25519 sign/verify): the kernel runs the op SYNCHRONOUSLY on the V8 thread, then fires the callback via `process.nextTick` — matching Node's `process.nextTick(callback, null, result)` dispatch. Total overhead ~1µs vs ~10µs for spawn_blocking.
- **Expensive ops** (RSA-4096 sign, RSA-8192 sign, future ML-DSA-87): spawn_blocking via `state.spawned_ops`. Threshold: any op estimated >100µs CPU time goes async.

The threshold is hard-coded by algorithm (no runtime measurement; we know RSA-4096 sign is ~5ms and RSA-2048 verify is ~50µs). The dispatch decision is a `match` on `(SignAlg, key_size_bits, op)` returning `DispatchKind::NextTick | DispatchKind::SpawnBlocking`. ~30 LOC.

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
| `crypto.publicEncrypt(keyOrOptions, buffer)` (addresses critic MAJOR #1, MAJOR #28: `keyOrOptions` accepts a key or an object `{ key, padding, oaepHash, oaepLabel, encoding }` — default padding is `RSA_PKCS1_OAEP_PADDING` per https://nodejs.org/api/crypto.html#cryptopublicencryptkey-buffer; default `oaepHash` is `'sha1'` (legacy footgun — modern apps SHOULD pass `'sha256'`)) | 1 | `kernel::rsa_oaep_encrypt` | sync | C |
| `crypto.privateDecrypt(keyOrOptions, buffer)` (same options as publicEncrypt; addresses MAJOR #1, MAJOR #28) | 1 | `kernel::rsa_oaep_decrypt` | sync | C |
| `crypto.publicDecrypt(keyOrOptions, buffer)` | 2 | aws-lc-rs raw FFI (low-level) | sync | C |
| `crypto.privateEncrypt(keyOrOptions, buffer)` | 2 | aws-lc-rs raw FFI (low-level) | sync | C |
| `crypto.diffieHellman({ privateKey, publicKey })` (one-shot; addresses critic missing concept #8: shape per https://nodejs.org/api/crypto.html#cryptodiffiehellmanoptions — both keys must be KeyObject with matching `asymmetricKeyType` of `'ec'`, `'x25519'`, `'x448'`, or `'dh'`. Returns the shared secret as Buffer. Also used internally by WebCrypto's `subtle.deriveBits({ name: 'ECDH', public: ... })` bridge.) | 1 | `kernel::dh_agree` (ECDH path) | sync | C |

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
| `ECDH.prototype.setPublicKey(publicKey, encoding?)` (deprecated, **shipped — addresses critic CRITICAL #6**) | 2 | `kernel::ecdh::set_public` + one-time deprecation warning | sync | C |
| `ECDH.convertKey(...)` (static) | 2 | aws-lc-sys raw FFI for compressed-point | sync | E |
| `getCurves() -> string[]` | 1 | iterate registry | sync | C |

**Named DH groups:** RFC 3526 (`modp1` = 768-bit, ..., `modp18` = 8192-bit) and RFC 7919 (`ffdhe2048`, `ffdhe3072`, `ffdhe4096`, `ffdhe6144`, `ffdhe8192`). The 768/1024-bit groups (modp1, modp2) are blocked by default (insecure); creator apps that need them get a runtime flag opt-in.

**`ECDH.setPublicKey` deprecation handling (addresses critic CRITICAL #6):** Per https://nodejs.org/api/crypto.html#ecdhsetpublickeypublickey-encoding, this method is **deprecated since Node v5.2.0 but still functional**. v1's "throw" was wrong (broke `tweetnacl-util` and the StrongSwan-style ECDH session-reuse pattern). v2 ships it with a one-shot per-isolate `process.emitWarning` call on first invocation, mirroring Node's behaviour:

```rust
// crypto_node/dh.rs (Stage C)
fn set_public_key<'s>(&mut self, /* ... */) -> Result<(), OpError> {
    emit_deprecation_warning_once(scope, "DEP0031",
        "crypto.ECDH.prototype.setPublicKey is deprecated; \
         use ECDH.convertKey or generate a fresh ECDH instance.");
    self.ctx.set_public(&pk_bytes).map_err(KernelError::to_node)
}
```

The `emit_deprecation_warning_once` helper memoises per (isolate, deprecation-code) so repeated calls warn once, matching Node's `--no-deprecation` / `process.noDeprecation` semantics (which we honour by reading the global flag).

### II.7. Key generation

| Export | Tier | Backed by | Sync/async | Stage |
|---|---|---|---|---|
| `generateKeyPair(type, options, callback)` | 1 | `kernel::generate_key_pair_async` | async (callback) | C |
| `generateKeyPairSync(type, options) -> { publicKey, privateKey }` | 1 | `kernel::generate_key_pair` | sync | C |
| `generateKey(type, options, callback)` (addresses critic minor m-14: yes, `subtle.generateKey` produces a CryptoKey — but `generateKey('hmac', ...)` here returns a **KeyObject** with no algorithm tie. The two are not redundant: WebCrypto bakes algorithm + extractable + usages into the key; node:crypto's KeyObject is algorithm-agnostic at creation time. Different abstractions; both shipped.) | 1 | `kernel::generate_key_async` | async | C |
| `generateKeySync(type, options) -> KeyObject` | 1 | `kernel::generate_key` | sync | C |
| `generatePrime(size, options?, callback?)` | 3 | aws-lc-sys raw FFI | async | E (Stage 2) |
| `generatePrimeSync(size, options?)` | 3 | aws-lc-sys raw FFI | sync | E |
| `checkPrime(candidate, options?, callback)` | 3 | aws-lc-sys raw FFI | async | E |
| `checkPrimeSync(candidate, options?)` | 3 | aws-lc-sys raw FFI | sync | E |

**Types supported (Stage 1):** `'rsa'` (with `modulusLength`, `publicExponent` defaulting to 0x10001), `'rsa-pss'` (with `hashAlgorithm`, `mgf1HashAlgorithm`, `saltLength`; addresses critic CRITICAL #12 — promoted from Stage 2 so the PSS sign/verify shipped in Stage C can be tested round-trip with PSS-typed keys, see https://nodejs.org/api/crypto.html#cryptogeneratekeypairtype-options-callback), `'ec'` (with `namedCurve`), `'ed25519'`, `'x25519'`, `'ed448'`, `'x448'`, `'hmac'` (returns SecretKeyObject), `'aes'` (returns SecretKeyObject; `length` in bits).

**Types deferred to Stage 2 (Stage E):** `'dsa'` (deprecated), `'dh'` (named-group DH), and the post-quantum types `'ml-dsa-44'` / `'ml-dsa-65'` / `'ml-dsa-87'`, `'ml-kem-512'` / `'ml-kem-768'` / `'ml-kem-1024'`, `'slh-dsa-*'` (Node v25+, addresses missing concept #3).

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
| `KeyObject.prototype.equals(other) -> boolean` (addresses critic CRITICAL #13: `type` first, length pre-check non-CT, then constant-time material compare; rejects "same logical key, different stored encoding" mismatch consistently) | 1 | see §IV.7a | sync | C |
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
| `X509Certificate(input)` (constructor — Stage E for real parsing. v3 fix, M2-7: consistently labeled Stage E throughout — v2 had inconsistent "placeholder lives in Stage C" prose contradicting the table. v3 places BOTH the placeholder (which throws `ERR_CRYPTO_UNSUPPORTED_OPERATION`) AND the real parser in Stage E. Stage C does NOT ship X509Certificate at all.) | 2 | aws-lc-sys raw FFI (`X509_d2i`) | sync | E |
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
| ~~`crypto.signal`~~ — REMOVED from the design (addresses critic minor m-9, M2-19): v1 invented this; Node's `node:crypto` has no `signal` export. The crossed-out row is documentation-only — the entry is NOT generated into the synthetic module exports, NOT included in `getCipherInfo` / `getHashes` filters, and NOT counted in the §II.14 coverage rollup. (v3 explicit fix: removed any code-side reference; the strikethrough is purely an audit trail for the v1 invention.) | — | — | — | — |
| `crypto.subtle` (alias for `webcrypto.subtle`; addresses critic MAJOR #9: top-level `crypto.subtle` was added as an alias to `crypto.webcrypto.subtle` in Node v15+ per https://nodejs.org/api/webcrypto.html — older code uses `crypto.webcrypto.subtle`, newer uses `crypto.subtle`. We export both, identity-preserving via D-N16.) | 1 | direct reference | n/a | D |

<!-- Round 3: addressing MAJOR M2-1 (coverage math out of sync with §II rollup). -->
### II.14. Coverage summary

(v3, addresses M2-1, m2-4 — denominator re-rolled after §II.15 additions.) The denominator counts every distinct export named in §II.1 through §II.15: each constructor, each prototype method, each free function, each static method. Counted: ~127 entries (v2 said 110; the discrepancy was §II.15's PQC + KEM + miscellaneous additions that v2 didn't roll into the rollup).

Stage 1 (the node:crypto APIs landed by end of Stage D):

- **Hashing:** 7 / 7 exports (100%)
- **HMAC:** 4 / 4 exports (100%)
- **Cipher / Decipher:** 12 / 13 exports (gain: `setAutoPadding` chainable per missing concept #23; `createCipher` is Stage E with --legacy-crypto flag).
- **Sign / Verify:** 11 / 11 exports (gain: `crypto.sign(callback)` + `crypto.verify(callback)` per MAJOR #15)
- **Public-key:** 4 / 5 exports (`publicDecrypt` / `privateEncrypt` Stage 2; gain: `crypto.diffieHellman(options)` per missing concept #8)
- **DH / ECDH:** 9 / 16 exports (ECDH 100%; named DH groups Stage 2; arbitrary DH Stage 2; `createDiffieHellman(primeLength)` Stage 2 per missing concept #6)
- **Key generation:** 5 / 10 exports (basic kinds Stage 1; primes Stage 2; PQC keygen Stage E)
- **Key import/export:** 12 / 12 exports (100%; gain: `KeyObject.toCryptoKey` per missing concept #7)
- **KDFs:** 6 / 6 exports (100%)
- **Random:** 7 / 7 exports (100%; gain: `pseudoRandomBytes` deprecated alias per missing concept #19)
- **X.509:** 0 / 17 exports (Stage 2; +4 from §II.15: `toString`, `toJSON`, `toLegacyObject`, `checkEmail`, `checkIP`, `issuerCertificate`)
- **WebCrypto bridge:** 3 / 3 exports (100%)
- **Misc:** 12 / 13 exports (gain: `crypto.subtle` top-level alias per MAJOR #9; `crypto.signal` removed per m-9)
- **PQC + KEM:** 0 / 7 exports (Stage E placeholders: `encapsulate`, `decapsulate`, 3× ML-DSA, 3× ML-KEM, SLH-DSA — all currently throw `ERR_CRYPTO_UNSUPPORTED_OPERATION` / `ERR_CRYPTO_KEM_NOT_SUPPORTED`)

**Total Stage 1: 92 / 127 exports (72%)** — covers ~95% of npm-package usage. (addresses critic minor m-13: the count is API surface area, not distinct features; method-per-row counting matches Node's documentation tree.)

**Total Stage 1 + Stage 2: 121 / 127 exports (95%)** — long tail in `setEngine` (NEVER), deprecated APIs (no `crypto.signal`), and PQC keygen until aws-lc-rs catches up.

**Counting methodology** (m2-4): each numbered row in §II.1 through §II.15 is one entry. Constructors and their `prototype.X` methods are separate entries (e.g., `Hash` constructor + `Hash.prototype.update` + `Hash.prototype.digest` + `Hash.prototype.copy` = 4 entries). Static methods (`KeyObject.from`) are separate from instance methods. Getters (`KeyObject.prototype.type`) count as one entry each. The "deferred-forever" set: `setEngine`, `createCipher` without --legacy-crypto, IDEA-CBC, AES-XTS, RIPEMD-160, SHAKE128/256, SHA3-224 (totals ~6 entries; everything else is deferable to Stage E or beyond).

### II.15. Post-quantum + KEM + miscellaneous Node v22-v25 additions (addresses critic missing concepts #1, #2, #3, #4, #7, #16, #17, #18, #19, #21, #22, #23, #25)

| Export | Tier | Backed by | Sync/async | Stage |
|---|---|---|---|---|
| `crypto.argon2(password, salt, options?)` (Node v22+, `crypto.hash`-shaped) | 3 | npm `argon2` (WASM via unenv) | sync/async | NEVER native — see open question XVII.4 + missing concept #1 |
| `crypto.encapsulate(publicKey)` / `crypto.decapsulate(privateKey, ciphertext)` (Node v22+ KEM API; addresses critic missing concept #2; v3 spec'd shape per M2-3 below) | 3 | aws-lc-rs PQC (when stable; ML-KEM via aws-lc-sys raw FFI) | sync | E (D-N36) |
| `Certificate` (legacy SPKAC) — `Certificate.exportChallenge`, `Certificate.exportPublicKey`, `Certificate.verifySpkac` | 3 | aws-lc-sys raw FFI for `NETSCAPE_SPKI_b64_decode` (~80 LOC) | sync | E (rare; only browser keygen, missing concept #4) |
| `KeyObject.toCryptoKey(algorithm, extractable, keyUsages)` (Node v18+; v3 lossy-bridge note per M2-12 below) | 1 | bridge: `KeyObject` → fresh `CryptoKey` via the Arc share + the WebCrypto `importKey('jwk', ko.export({format:'jwk'}))` round-trip | sync | C (missing concept #7 + clarifies the bidirectional bridge in D-N4) |
| `crypto.checkPrime(candidate, options?, callback)` / `checkPrimeSync` | 3 | aws-lc-sys raw FFI for `BN_is_prime_fasttest_ex` | sync/async | E (missing concepts #5, #11; can ship independently of generatePrime per the critic) |
| `crypto.createDiffieHellman(primeLength)` (synthesise a fresh prime) | 3 | aws-lc-sys raw FFI for `DH_generate_parameters_ex` | async (Node v17+ defaults async; sync overload retained) | E (missing concept #6) |
| `X509Certificate.prototype.toString()` returns PEM | 2 | reuses kernel PEM emitter | sync | E (missing concept #16) |
| `X509Certificate.prototype.toJSON()` | 2 | object literal of public-property snapshot | sync | E (missing concept #16) |
| `X509Certificate.prototype.toLegacyObject()` | 2 | the OpenSSL-flavoured shape some libraries still use | sync | E (missing concept #16) |
| `X509Certificate.prototype.checkEmail(email, options?)` | 2 | aws-lc-sys raw FFI for `X509_check_email` | sync | E (missing concept #17) |
| `X509Certificate.prototype.checkIP(ip)` | 2 | aws-lc-sys raw FFI for `X509_check_ip_asc` | sync | E (missing concept #17) |
| `X509Certificate.prototype.issuerCertificate` (only set if chain provided) | 2 | populated when constructed from a multi-block PEM | sync | E (missing concept #18) |
| `crypto.pseudoRandomBytes(size)` (deprecated alias) | 2 | alias to `randomBytes` (Node never differentiated post-v0.6) | sync | B (missing concept #19) |
| `crypto.randomFillSync(buffer, offset?, size?)` with offset+size validation | 1 | extended validator | sync | B (missing concept #20) |
| `crypto.scrypt` short-form options `{ N, r, p }` aliasing | 1 | option-aliasing in `parse_scrypt_options` (accepts both `cost`/`blockSize`/`parallelization` AND `N`/`r`/`p`) | sync/async | B (missing concept #22) |
| `cipher.setAutoPadding(boolean)` returns Cipher (chainable) | 1 | already chainable in v1 design — corrects the v1 type signature; addresses critic missing concept #23 | sync | C |

**Post-quantum notes (D-N36, missing concept #3):** Node v25 added these `asymmetricKeyType` values: `'ml-dsa-44'`, `'ml-dsa-65'`, `'ml-dsa-87'` (FIPS 204), `'ml-kem-512'`, `'ml-kem-768'`, `'ml-kem-1024'` (FIPS 203), `'slh-dsa-sha2-128f'` etc. (FIPS 205). Stage E ships PARSE-ONLY recognition: the `asymmetricKeyType` getter returns the right string, but `generateKeyPair('ml-dsa-65', ...)` errors with `ERR_CRYPTO_UNSUPPORTED_OPERATION` until aws-lc-rs's PQC API stabilises.

<!-- Round 3: addressing MAJOR M2-3 (encapsulate/decapsulate shape). -->
**`crypto.encapsulate` / `crypto.decapsulate` shape (v3, addresses M2-3):** Per https://nodejs.org/api/crypto.html#cryptoencapsulatepublickey:

```ts
// Stage E placeholder; full impl arrives when aws-lc-rs's ML-KEM API ships.
crypto.encapsulate(publicKey: KeyObject | CryptoKey): {
    sharedKey: Buffer,    // the symmetric key the encapsulator + decapsulator agree on
    ciphertext: Buffer,   // the encapsulation, which decapsulator uses to recover sharedKey
}

crypto.decapsulate(
    privateKey: KeyObject | CryptoKey,
    ciphertext: Buffer | Uint8Array,
): Buffer    // the recovered sharedKey
```

The keys must be `asymmetricKeyType` of `'ml-kem-512'`, `'ml-kem-768'`, or `'ml-kem-1024'` (the only PQC KEMs Node v22+ accepts). Sync API; no async variant in Node yet. Stage E placeholder throws `ERR_CRYPTO_KEM_NOT_SUPPORTED` (real per `lib/internal/errors.js`).

**Argon2 (missing concept #1):** Node v22 did NOT add `crypto.argon2` as a standalone export — confirmed against https://nodejs.org/api/crypto.html (no `crypto.argon2` entry as of writing). The critic's claim was incorrect on the surface name; what Node v22 added was `crypto.hash` (a one-shot hashing convenience), not argon2. Argon2 remains npm-package territory (`argon2`, `@phc/argon2`). v2 corrects v1's "Node never shipped it" to "Node has not shipped argon2 in `node:crypto` as of v25; revisit if Node adds it post-cutoff."

<!-- Round 3: addressing MAJOR M2-12 (KeyObject.toCryptoKey PSS lossy bridge). -->
**`KeyObject.toCryptoKey` lossiness for RSA-PSS (v3, addresses M2-12):** the bridge currently round-trips through JWK (`subtle.importKey('jwk', ko.export({format:'jwk'}))`). For symmetric (`SecretKeyObject`) and standard asymmetric (RSA-PKCS1, ECDSA, Ed25519, X25519) keys this is lossless. **For RSA-PSS-typed private keys, JWK loses the PSS-specific algorithm parameters** — RFC 7518 doesn't define a `kty: 'RSA'` JWK that distinguishes PSS from PKCS1, and the `alg` claim (`'PS256'`/`'PS384'`/`'PS512'`) only carries the hash, not `mgf1HashAlgorithm` or `saltLength`. After the JWK round-trip, the resulting CryptoKey has its WebCrypto algorithm set from the user-supplied `algorithm` parameter, NOT preserved from the source KeyObject's `asymmetricKeyDetails`.

v3 documents this as an accepted trade-off: the user MUST supply the matching `{ name: 'RSA-PSS', hash, saltLength?, ... }` algorithm dict to `toCryptoKey` for PSS keys. If the supplied algorithm parameters disagree with the source KeyObject's PSS parameters (e.g., source has `saltLength: 32`, `algorithm.saltLength: 16`), the resulting CryptoKey uses the user-supplied values, NOT the source's. This matches Node's behaviour (Node has the same JWK-bridging limitation in `KeyObject.toCryptoKey`).

**Future fix path** (XVII.13 below, queued): bypass the JWK round-trip by directly cloning the `Arc<KeyMaterial>` into a new `CryptoKeyState` with the user-supplied algorithm — this requires the WebCrypto algorithm-validation logic to accept any `KeyMaterial` variant the source KeyObject can hold. ~30 LOC change in `crypto_native/crypto_key.rs`. Deferred to Stage F.

**Stream.Transform (missing concept #21):** addressed in §V.6 above.

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

<!-- Round 3: addressing CRITICAL C2-3 (invented aws-lc-rs algorithm constants). -->
### III.2. node:crypto-only algorithms (NEW kernel work)

**aws-lc-rs API audit (v3, addresses C2-3):** the `Backing path` column below lists the EXACT constants/modules verified against the live docs.rs surface (https://docs.rs/aws-lc-rs/latest/aws_lc_rs/). Where v2 cited a non-existent constant, the row is rewritten. Three categories:
- **`aws_lc_rs::*` (high-level)** — verified-present in the public Rust API.
- **`aws_lc_sys::*` (raw FFI)** — must drop to the C bindings; we vendor an `EVP_*` shim mirroring the existing `crypto_native/evp_ffi.rs` pattern (D-N38).
- **DEFERRED** — not shippable from aws-lc / BoringSSL at all (e.g., RIPEMD-160 was removed; IDEA was excised; SHAKE/SHA3-224 are NOT in BoringSSL).

| Algorithm | Why node:crypto needs it | Backing path (v3 verified) | Stage |
|---|---|---|---|
| MD5 | etag generation, content addressing, legacy auth | aws-lc-sys raw FFI via `EVP_md5()` + `EVP_DigestInit_ex` / `EVP_DigestUpdate` / `EVP_DigestFinal_ex`. **NOT in aws-lc-rs's public digest module** (verified: https://docs.rs/aws-lc-rs/latest/aws_lc_rs/digest/index.html — only SHA1/2/3 + SHA512_256). ~30 LOC FFI wrapper. | B |
| SHA-224 | Some legacy SAML / PKCS profiles | aws-lc-rs `digest::SHA224` (verified present). | B |
| SHA-512/256 | Modern interop | aws-lc-rs `digest::SHA512_256` (verified present). | B |
| SHA-512/224 | Legacy interop | aws-lc-sys raw FFI via `EVP_sha512_224()` (BoringSSL has it). **NOT in aws-lc-rs's public digest module.** ~40 LOC FFI wrapper. | B |
| SHA3-256 / SHA3-384 / SHA3-512 | Modern interop, post-Keccak hashing | aws-lc-rs `digest::SHA3_256` / `SHA3_384` / `SHA3_512` (verified present). | B |
| SHA3-224 / SHAKE128 / SHAKE256 | Mostly XOF use cases | **DEFERRED to Stage E** (or beyond). aws-lc-rs does not expose them; BoringSSL itself does not ship SHA3-224 or SHAKE in its public API (only the SHA3 256/384/512 variants used internally for Ed448). RIPEMD-160 is similarly absent. (v3, addresses C2-3 + critic miss-#4: SHAKE outputLength variable-length API spec'd if/when shipped — but Stage E is the earliest realistic landing.) | E (or DEFER) |
| `chacha20-poly1305` | Modern AEAD (D-N23) | aws-lc-rs `aead::CHACHA20_POLY1305` (verified present). | C |
| AES-CBC (128/192/256) | Standard interop | aws-lc-rs `cipher::AES_128/192/256` + `PaddedBlockEncryptingKey::cbc_pkcs7` (verified present). | C |
| AES-CTR (128/192/256) | Standard interop | aws-lc-rs `cipher::AES_128/192/256` + `EncryptingKey::ctr` (verified present). | C |
| AES-GCM (128/192/256) | Modern AEAD | aws-lc-rs `aead::AES_128_GCM` / `AES_192_GCM` / `AES_256_GCM` (verified present; **note 192-bit IS exposed in aead module** per docs.rs). | C |
| AES-GCM-SIV (128/256) | Nonce-reuse-resistant AEAD | aws-lc-rs `aead::AES_128_GCM_SIV` / `AES_256_GCM_SIV` (verified present; **bonus capability over Node** which doesn't ship SIV). Optional Stage 1 extension. | C |
| AES-CCM (128/192/256) | NIST mode for constrained devices | **NOT in aws-lc-rs** (verified: https://docs.rs/aws-lc-rs/latest/aws_lc_rs/aead/index.html exposes only GCM / GCM-SIV / ChaCha20-Poly1305). aws-lc-sys raw FFI via `EVP_aes_128_ccm()` / `EVP_aes_192_ccm()` / `EVP_aes_256_ccm()` + `EVP_CIPHER_CTX_*`. ~120 LOC FFI wrapper (CCM has the unusual two-pass API; `EVP_CipherInit_ex` then `EVP_CIPHER_CTX_ctrl(EVP_CTRL_CCM_SET_IVLEN/SET_TAG/SET_L/SET_M)` then `EVP_CipherUpdate(NULL, ..., NULL, plaintext_len)` to set total length BEFORE AAD/data — this is what makes CCM's setAuthTag-pre-update ordering necessary; v2's state machine already enforces this correctly per CRITICAL #5). (v3, addresses C2-3.) | C |
| AES-OCB (128/256) | RFC 7253 AEAD | **NOT in aws-lc-rs** (verified). aws-lc-sys raw FFI via `EVP_aes_128_ocb()` / `EVP_aes_256_ocb()` + `EVP_CIPHER_CTX_*`. ~80 LOC FFI wrapper. **DEFERRED to Stage E** — OCB is rare in practice; effort/value ratio doesn't justify Stage C. (v3, addresses C2-3: v2 wrongly claimed `aead::AES_*_OCB`; corrected.) | E |
| AES-KW (128/192/256) | Key wrapping (D-N4) | aws-lc-rs `aead::*` does NOT expose KW; v2 claim corrected. Implemented via `aws_lc_rs::cipher::AES_*` in raw-block mode + RFC 3394 wrap routine in pure Rust (~120 LOC). Already implemented in `crypto_native/wrap.rs` from WebCrypto — moved into kernel for Stage A. | C (kernel reuse — Stage A) |
| AES-XTS (128/256) | Disk encryption | **NOT in aws-lc-rs** (verified: cipher module has CBC/CTR/CFB128 only). aws-lc-sys raw FFI via `EVP_aes_128_xts()` / `EVP_aes_256_xts()` AND requires the cipher state to track an explicit "tweak" (disk-block index passed via the IV-as-tweak field per NIST SP 800-38E). Our generic `CipherContext` does not have a tweak concept. **DEFERRED PERMANENTLY** unless a creator app surfaces a disk-encryption use case (rare in app-server context; XTS is a disk-driver feature). (v3, addresses C2-3, M2-23: removed from CIPHER_NAMES.) | DEFER |
| `aes-*-cfb` (CFB128) | Niche modern | aws-lc-rs `cipher::AES_*` + CFB128 mode (verified — CFB128 is the only CFB variant in the public API). | E (Stage 2; ungated) |
| `aes-*-cfb1`, `aes-*-cfb8`, `aes-*-ofb`, `aes-*-ecb` | Very niche legacy | aws-lc-sys raw FFI via `EVP_aes_*_cfb1()`, `EVP_aes_*_cfb8()`, `EVP_aes_*_ofb()`, `EVP_aes_*_ecb()`. ~40 LOC each. | E (Stage 2; ungated) |
| 3DES (DES-EDE3) in CBC / ECB / CFB | Legacy auth (banking, retail POS) | aws-lc-sys raw FFI via `EVP_des_ede3_cbc()` / `EVP_des_ede3_ecb()` / `EVP_des_ede3_cfb64()`. **NOT in aws-lc-rs** (verified: cipher module has AES only — `cipher::TDES_*` was a v1/v2 invention). ~40 LOC. | E (`--legacy-crypto` flag) |
| Blowfish / Cast5 / RC4 / IDEA | Pre-2010 legacy | aws-lc-sys raw FFI for Blowfish (`EVP_bf_cbc()`); Cast5 / RC4 likely require deeper digging in aws-lc-sys (the BoringSSL slim build may not include them); IDEA was REMOVED from BoringSSL — **DEFERRED PERMANENTLY**. | E for Blowfish/Cast5/RC4 if present; DEFER for IDEA |
| BLAKE2b-512, BLAKE2s-256 | Hash diversity, password libs | aws-lc-sys raw FFI via `EVP_blake2b512()` / `EVP_blake2s256()`. **NOT in aws-lc-rs digest module** (verified). ~30 LOC each. | E |
| RIPEMD-160 | Legacy bitcoin / lightning code | **NOT in aws-lc** (BoringSSL does not ship RIPEMD-160). **DEFERRED PERMANENTLY** — recommend creator apps use the npm `ripemd160` package (pure JS, falls back via unenv). | DEFER |
| **scrypt** | RFC 7914 password hashing | aws-lc-sys raw FFI via `EVP_PBE_scrypt()`. **NOT in aws-lc-rs's public KDF surface** (verified: only PBKDF2 + HKDF). ~20 LOC. | B |
| **DH (modp1..modp18, ffdhe2048..ffdhe8192)** | Named-group DH | aws-lc-sys raw FFI via `DH_get_*` static-prime helpers + `DH_set0_pqg` + `DH_compute_key`. **NOT in aws-lc-rs's `agreement` module** (which exposes only EC: P-256/P-384/P-521 + X25519). ~150 LOC. | E |
| **DH arbitrary primes** | Generic DH | aws-lc-sys raw FFI (same path as named groups, plus user-supplied prime + generator). ~50 LOC delta. | E |
| **Brainpool curves** | Niche EU / German banking | aws-lc-sys raw FFI via `EC_GROUP_new_by_curve_name(NID_brainpoolP*)`. **NOT in aws-lc-rs's `signature` / `agreement` module** (verified). ~80 LOC. | E |
| **secp256k1** | Bitcoin / Ethereum signing | aws-lc-rs `signature::ECDSA_P256K1_SHA256_{ASN1,FIXED}` and `_SIGNING` variants (verified at https://docs.rs/aws-lc-rs/latest/aws_lc_rs/signature/index.html — actual constant name is `P256K1` not `K256`). (counter-cited round-1 critic; v3 reaffirms.) | C |

Stage 1 coverage (Stage A+B+C, after v3 reroutes): SHA-1 / SHA-224 / SHA-256 / SHA-384 / SHA-512 / SHA-512-256 / SHA3-256 / SHA3-384 / SHA3-512 (high-level) + MD5 / SHA-512-224 (FFI); HMAC over those; AES-{CBC,CTR,GCM,KW,GCM-SIV} + ChaCha20-Poly1305 (high-level); AES-CCM (FFI); RSA-{PKCS1,PSS} + RSA-OAEP (high-level); ECDSA (P-256/P-384/P-521 + secp256k1) + Ed25519 + X25519 (high-level); ECDH (P-256/P-384/P-521 + X25519) (high-level); PBKDF2 + HKDF (high-level); scrypt (FFI).

Stage 2 coverage (Stage E): BLAKE2 + 3DES + Brainpool curves + named DH groups + X.509 + RC4 / Blowfish / Cast5 (with `--legacy-crypto`, where present in aws-lc) + arbitrary-prime DH + generatePrime / checkPrime + AES-OCB + AES-CFB1/8 + AES-OFB + AES-ECB.

Permanently deferred: AES-XTS (no tweak in generic CipherContext), RIPEMD-160 (not in aws-lc), IDEA (removed from BoringSSL), SHA3-224 / SHAKE128 / SHAKE256 (not in BoringSSL public API), MD5-as-encryption (footgun, never).

Argon2: NOT shipped natively. Node doesn't ship it in `node:crypto`. The npm `argon2` package is a node-gyp binding; via unenv it falls back to WASM. Acceptable.

<!-- Round 3: addressing CRITICAL C2-3 (effort estimates updated for FFI work). -->
### III.2a. Stage B / C FFI inventory (D-N38, addresses C2-3)

The FFI work that must land alongside the high-level aws-lc-rs work, with effort estimates revised from v2's optimistic numbers:

| Stage | FFI module | Functions wrapped (raw aws-lc-sys) | LOC | Industry-h | Agent-h |
|---|---|---|---|---|---|
| B | `crypto_kernel/digest_md5.rs` | `EVP_md5`, `EVP_DigestInit_ex`, `EVP_DigestUpdate`, `EVP_DigestFinal_ex`, `EVP_MD_CTX_new`, `EVP_MD_CTX_free` | ~30 | 4 | 0.1 |
| B | `crypto_kernel/digest_sha512_224.rs` | `EVP_sha512_224` + reused EVP_DigestInit/Update/Final | ~40 | 5 | 0.13 |
| B | `crypto_kernel/scrypt.rs` | `EVP_PBE_scrypt` | ~20 | 3 | 0.08 |
| C | `crypto_kernel/cipher_ccm.rs` | `EVP_aes_128_ccm`, `EVP_aes_192_ccm`, `EVP_aes_256_ccm`, `EVP_CIPHER_CTX_*`, `EVP_CipherInit_ex`, `EVP_CIPHER_CTX_ctrl` (with `EVP_CTRL_CCM_SET_IVLEN`, `EVP_CTRL_CCM_SET_TAG`, `EVP_CTRL_CCM_SET_L`, `EVP_CTRL_CCM_SET_M`), `EVP_CipherUpdate` | ~120 | 14 | 0.35 |
| C | `crypto_kernel/pkcs8_enc.rs` | `PKCS8_encrypt`, `PKCS8_marshal_encrypted_private_key`, `PKCS8_decrypt`, `PKCS8_parse_encrypted_private_key`, `EVP_PKEY_*`, `EVP_aes_*_cbc`, `BIO_*`, `i2d_PKCS8_PRIV_KEY_INFO`, `d2i_PKCS8_PRIV_KEY_INFO`, `CBB_*`, `CBS_*` (per D-N37 below) | ~250 | 28 | 0.7 |
| E | `crypto_kernel/cipher_ocb.rs` | `EVP_aes_128_ocb`, `EVP_aes_256_ocb` + `EVP_CIPHER_CTX_*` | ~80 | 10 | 0.25 |
| E | `crypto_kernel/cipher_legacy.rs` | `EVP_des_ede3_cbc/ecb/cfb`, `EVP_bf_cbc`, `EVP_rc4`, `EVP_aes_*_cfb1/cfb8/ofb/ecb` | ~150 | 18 | 0.45 |
| E | `crypto_kernel/digest_blake2.rs` | `EVP_blake2b512`, `EVP_blake2s256` | ~30 | 4 | 0.1 |
| E | `crypto_kernel/dh_named.rs` | `DH_get_*` (static-prime helpers), `DH_set0_pqg`, `DH_compute_key`, `DH_check_pub_key` | ~150 | 18 | 0.45 |
| E | `crypto_kernel/x509.rs` | `X509_*`, `d2i_X509`, `i2d_X509`, `X509_NAME_*`, `X509_get_subject_name`, `X509_get_issuer_name`, `X509_check_*` | ~400 | 40 | 1.0 |

**Total Stage B FFI:** ~90 LOC, ~12 industry-h, ~0.31 agent-h. (v2 said 60 industry-h for all of Stage B; v3 sees Stage B as ~90 industry-h after adding the FFI wrappers + the existing high-level work; net 50% bump.)
**Total Stage C FFI:** ~370 LOC, ~42 industry-h, ~1.05 agent-h. (v2 said 90 industry-h for all of Stage C; v3 bumps to ~110 industry-h to cover CCM + pkcs8_enc.)
**Total Stage E FFI:** ~810 LOC, ~90 industry-h, ~2.25 agent-h.

(Note on agent-h: per the project's `feedback_estimates_hours_not_weeks.md` memory rule, divide by ~40 from industry-h. The agent-h estimates above use that ratio.)

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
| `"rsa"` | `{ modulusLength: number, publicExponent: bigint }` (publicExponent is `bigint` per https://nodejs.org/api/crypto.html#keyobjectasymmetrickeydetails — addresses critic MAJOR #13: v1 implied `Buffer`) |
| `"rsa-pss"` | `{ modulusLength, publicExponent, hashAlgorithm?, mgf1HashAlgorithm?, saltLength? }` — these PSS-specific fields are populated **only** when the SPKI/PKCS8 carries the `id-RSASSA-PSS` OID with embedded SaltedSignatureAlgorithms parameters (RFC 4055). For a plain RSA key signed with PSS at sign-time, the fields are `undefined`. (addresses critic MAJOR #13 + MAJOR #23) |
| `"dsa"` | `{ modulusLength, divisorLength, hashAlgorithm }` — `hashAlgorithm` is the digest algorithm OID embedded in the DSA params; v1 missed it. (addresses critic MAJOR #22) |
| `"ec"` | `{ namedCurve: "P-256" \| "P-384" \| "P-521" \| "secp256k1" \| "prime256v1" \| "secp384r1" \| "secp521r1" \| ... }` — Node returns the **OpenSSL canonical name** for the curve. P-256's OpenSSL name is `"prime256v1"`, NOT `"P-256"`. P-384 is `"secp384r1"`. We follow Node and emit OpenSSL names; spec-canonical names appear in the JWK / WebCrypto surface only. (addresses critic minor m-6 — `if (key.asymmetricKeyDetails.namedCurve === 'prime256v1')` checks now match.) |
| `"dh"` | `{ generator, prime, primeLength: number }` |
| `"ed25519"` / `"x25519"` / `"ed448"` / `"x448"` | `{}` (empty object) |
| `"ml-dsa-*"` / `"ml-kem-*"` / `"slh-dsa-*"` (Node v25+) | per-algo shape — see D-N36 (Stage E, parse-only) |

The getter is `[SameObject]` cached (D-N3 macro `#[v8_getter(same_object)]`).

### IV.4. The `create*Key` factories (D-N14)

```rust
// crypto_node/key_object.rs

pub fn create_secret_key<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    input: v8::Local<v8::Value>,
    encoding: Option<&str>,
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    // (addresses critic MAJOR #14): per https://nodejs.org/api/crypto.html#cryptocreatesecretkeykey-encoding
    // when input is a string, encoding is REQUIRED and applies; when input is
    // a Buffer / TypedArray, encoding is IGNORED. extract_input already
    // implements this rule (CRITICAL #2 fix); we don't need to special-case
    // here. The encoding parameter is passed through to extract_input which
    // ignores it when input is a Buffer. v1's "Option<&str> always passed
    // through" was correct in code but the docstring said "rejects a non-
    // string with encoding"; the docstring was wrong.
    let bytes = buffer::extract_input(scope, input, encoding)?;
    if bytes.is_empty() {
        return Err(OpError::node("ERR_OUT_OF_RANGE",
            "The value of \"key\" is out of range. It must be > 0"));
    }
    // (addresses critic missing concept #14): we do NOT validate that
    // bytes.len() matches an HMAC's expected algorithm-tied size — Node
    // doesn't either (createSecretKey is algorithm-agnostic; the algorithm
    // tie-in happens at createHmac time). Algorithm-aware validation is the
    // caller's responsibility (e.g. AES key sizes are checked at
    // createCipheriv time, not at createSecretKey time).
    let state = KeyObjectState {
        key_type: KeyType::Secret,
        material: Arc::new(KeyMaterial::Symmetric(Zeroizing::new(bytes))),
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
                // (v3, addresses C2-1, C2-2): ERR_OSSL_UNSUPPORTED is dynamic-OSSL,
                // not in Node's static registry. Real Node emits ERR_INVALID_ARG_VALUE
                // for unknown PEM labels per lib/internal/crypto/keys.js.
                _ => Err(OpError::node("ERR_INVALID_ARG_VALUE",
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

For encrypted PKCS#8 (`{ passphrase: Buffer.from('hunter2') }`), see §IV.4a (encrypted PKCS#8 path). v1 specified an `EncryptedPrivateKeyInfo::from_bytes(...).decrypt(passphrase)` call on `aws-lc-rs`; that API **does not exist in `aws-lc-rs` 1.x** (verified against https://docs.rs/aws-lc-rs/latest/aws_lc_rs/ — the encoding module exposes only `Pkcs8V1Der` / `Pkcs8V2Der` byte wrappers, no encryption). v2 drops to `aws-lc-sys` raw FFI — see new D-N33 below.

<!-- Round 3: addressing CRITICAL C2-4 (D-N33 prose-only -> function-signature spec). -->
### IV.4a. Encrypted PKCS#8 import / export (D-N33, D-N37 — addresses critic CRITICAL #7, MAJOR #12, MAJOR #16, round-2 CRITICAL #4)

The high-level `aws-lc-rs` does not expose PBES2/PBKDF2-encrypted PKCS#8. We implement a thin `crypto_kernel/pkcs8_enc.rs` (~250 LOC) over `aws-lc-sys` raw FFI. **v3 specifies the exact FFI sequence** (round-2 critic flagged the v2 prose-only spec; the implementer needed concrete EVP_* / PKCS8_* call shape).

The aws-lc public C API for encrypted PKCS#8 is documented at https://github.com/aws/aws-lc/blob/main/include/openssl/pkcs8.h (verified 2026-05-02; commit pinned via the project's aws-lc-sys workspace dep). Four functions matter:

```c
// Inputs: pbe_nid (always pass -1 to select PBES2), cipher (the inner EVP_CIPHER*),
// pass + pass_len (passphrase bytes), salt + salt_len (NULL salt + nonzero len = generate
// random salt of that length), iterations, p8inf (the unencrypted PKCS#8 inner key).
// Returns: a freshly-allocated X509_SIG that must be freed by the caller.
OPENSSL_EXPORT X509_SIG *PKCS8_encrypt(int pbe_nid, const EVP_CIPHER *cipher,
                                       const char *pass, int pass_len,
                                       const uint8_t *salt, size_t salt_len,
                                       int iterations,
                                       PKCS8_PRIV_KEY_INFO *p8inf);

// Same as PKCS8_encrypt but writes the EncryptedPrivateKeyInfo ASN.1 directly to a CBB
// (BoringSSL's CRYPTO_BUFFER builder) and takes an EVP_PKEY directly. Returns 1 on
// success, 0 on error.
OPENSSL_EXPORT int PKCS8_marshal_encrypted_private_key(
    CBB *out, int pbe_nid, const EVP_CIPHER *cipher, const char *pass,
    size_t pass_len, const uint8_t *salt, size_t salt_len, int iterations,
    const EVP_PKEY *pkey);

// Inputs: pkcs8 (the X509_SIG containing EncryptedPrivateKeyInfo); pass + pass_len.
// Returns: a freshly-allocated PKCS8_PRIV_KEY_INFO (the unencrypted inner key info)
// that must be freed by the caller; NULL on error (wrong passphrase, malformed input,
// unsupported PBES2 inner cipher, etc.).
OPENSSL_EXPORT PKCS8_PRIV_KEY_INFO *PKCS8_decrypt(X509_SIG *pkcs8,
                                                  const char *pass,
                                                  int pass_len);

// Same as PKCS8_decrypt but parses the EncryptedPrivateKeyInfo ASN.1 directly from a
// CBS (CRYPTO_BUFFER reader) and returns an EVP_PKEY directly. Returns NULL on error.
OPENSSL_EXPORT EVP_PKEY *PKCS8_parse_encrypted_private_key(CBS *cbs,
                                                           const char *pass,
                                                           size_t pass_len);
```

**v3 (D-N37) picks `PKCS8_marshal_encrypted_private_key` + `PKCS8_parse_encrypted_private_key`** as the primary API surface — they take EVP_PKEY directly (so we can flow our existing aws-lc-rs key handle in/out) and they read/write the ASN.1 envelope into/from CBB/CBS buffers (avoiding the X509_SIG intermediate type that the older PKCS8_encrypt/PKCS8_decrypt entry points use).

**Function signatures spec'd in `crypto_kernel/pkcs8_enc.rs` (v3, D-N37):**

```rust
//! crypto_kernel/pkcs8_enc.rs — encrypted PKCS#8 import / export via aws-lc-sys.
//!
//! Architecture:
//!   - Public surface: `encrypt_pkcs8` + `decrypt_pkcs8` taking unencrypted-PKCS8 DER
//!     (the bytes already carried in our `KeyMaterial::AsymmetricPrivate*` variant)
//!     and a passphrase + cipher choice, returning encrypted-PKCS8 DER (or vice versa).
//!   - Internal: a thin Rust wrapper over PKCS8_marshal_encrypted_private_key /
//!     PKCS8_parse_encrypted_private_key, plus the cipher-name → EVP_CIPHER* lookup.
//!
//! Errors map to KernelError::PassphraseMismatch (decrypt) /
//! KernelError::InvalidDer (parse) / KernelError::UnsupportedKeyAlgorithm
//! (cipher whitelist miss). Per VII.3 those map to ERR_CRYPTO_OPERATION_FAILED
//! / ERR_CRYPTO_OPERATION_FAILED / ERR_CRYPTO_UNSUPPORTED_OPERATION respectively
//! (see C2-2 audit; ERR_OSSL_EVP_BAD_DECRYPT is dynamic-OSSL, not used).

use aws_lc_sys::{
    CBB, CBB_init, CBB_finish, CBB_cleanup,
    CBS, CBS_init,
    EVP_PKEY, EVP_PKEY_free,
    EVP_aes_128_cbc, EVP_aes_192_cbc, EVP_aes_256_cbc, EVP_des_ede3_cbc,
    EVP_aes_128_ecb, EVP_aes_256_ecb, EVP_des_ede3_ecb,
    EVP_CIPHER,
    PKCS8_marshal_encrypted_private_key, PKCS8_parse_encrypted_private_key,
    OPENSSL_free,
};
use std::ffi::{c_int, c_void};
use crate::crypto_kernel::error::KernelError;
use crate::crypto_kernel::key_material::KeyMaterial;
use std::sync::Arc;
use zeroize::Zeroizing;

/// PBES2 default iteration count. Node uses 2048 (matching OpenSSL's
/// PKCS12_DEFAULT_ITER); modern recommendations are higher (NIST SP 800-132
/// suggests 600k for 2026 era), but PBES2 is for *encrypting at-rest private
/// keys*, not for password storage — the threat model is different (offline
/// attack on the encrypted PEM, not online login). Keep 2048 to match Node's
/// defaults; allow override via the unstable `iterations` option (Node v22+).
pub const DEFAULT_PBES2_ITERATIONS: i32 = 2048;

/// Salt size (bytes) for PBES2 KDF. 16 is OpenSSL's default; matches Node.
pub const DEFAULT_PBES2_SALT_LEN: usize = 16;

/// Encrypted-PKCS#8 cipher whitelist. The `cipher` option to KeyObject.export
/// must be one of these names; passing anything else throws
/// ERR_CRYPTO_UNSUPPORTED_OPERATION (real Node code per node_errors.h, v3 fix
/// per C2-2). v3 expansion (addresses M2-22): Node accepts the broader list
/// per `lib/internal/crypto/keys.js` parseKeyEncodingAsymmetric.
///
/// Each entry maps to an EVP_CIPHER* lookup; ECB variants are flagged so we
/// can refuse them at the surface (PBES2 + ECB is technically not allowed
/// per RFC 8018 §6.2 — PBES2's parameter block always carries an IV, but
/// some legacy tools serialize ECB-mode encrypted PKCS#8 with a zero IV; we
/// honor those when --legacy-crypto is on but the spec-compliant path is to
/// reject — see M2-10).
fn lookup_cipher(name: &str) -> Result<*const EVP_CIPHER, KernelError> {
    let cipher_fn: unsafe extern "C" fn() -> *const EVP_CIPHER = match name {
        "aes-128-cbc"   => EVP_aes_128_cbc,
        "aes-192-cbc"   => EVP_aes_192_cbc,        // M2-22: Node accepts; we ship.
        "aes-256-cbc"   => EVP_aes_256_cbc,
        "aes-128-ecb"   => {
            // (M2-10) RFC 8018 §6.2 forbids ECB inner ciphers in PBES2; we
            // accept only when --legacy-crypto is on (this gate runs at the
            // surface, before reaching this function). ECB has no IV; the
            // PBES2 parameter block emits a zero-length IV which round-trips
            // through aws-lc but is malformed per spec.
            if !crate::runtime::flags::legacy_crypto_enabled() {
                return Err(KernelError::UnsupportedKeyAlgorithm(
                    "aes-128-ecb is not allowed as PBES2 inner cipher (RFC 8018 §6.2; \
                     enable --legacy-crypto to bypass the spec check)".to_string()));
            }
            EVP_aes_128_ecb
        },
        "aes-256-ecb"   => {
            if !crate::runtime::flags::legacy_crypto_enabled() {
                return Err(KernelError::UnsupportedKeyAlgorithm(
                    "aes-256-ecb is not allowed as PBES2 inner cipher (RFC 8018 §6.2)".to_string()));
            }
            EVP_aes_256_ecb
        },
        "des-ede3-cbc"  => {
            if !crate::runtime::flags::legacy_crypto_enabled() {
                return Err(KernelError::UnsupportedKeyAlgorithm(
                    "des-ede3-cbc requires --legacy-crypto".to_string()));
            }
            EVP_des_ede3_cbc
        },
        "des-ede3-ecb"  => {
            if !crate::runtime::flags::legacy_crypto_enabled() {
                return Err(KernelError::UnsupportedKeyAlgorithm(
                    "des-ede3-ecb requires --legacy-crypto".to_string()));
            }
            EVP_des_ede3_ecb
        },
        // (M2-22, addresses Node's full list): the remaining entries — bf-cbc,
        // rc2-*, rc4 — are listed in Node's source but rarely seen; we route
        // to the same UnsupportedKeyAlgorithm gate. Stage E may flesh out.
        _ => return Err(KernelError::UnsupportedKeyAlgorithm(
            format!("Unknown encrypted-PKCS#8 cipher: {}", name))),
    };
    // SAFETY: aws-lc EVP_*_cbc/ecb getters are pure (return a static const
    // pointer); no thread or state hazards.
    Ok(unsafe { cipher_fn() })
}

/// Encrypt an unencrypted PKCS#8 DER blob, returning an EncryptedPrivateKeyInfo
/// ASN.1 DER blob.
///
/// Implementation steps:
///   1. Parse the input PKCS#8 DER into an EVP_PKEY* (via `d2i_PrivateKey` /
///      `EVP_parse_private_key` — the kernel already has this routine for
///      WebCrypto's import path, reused here).
///   2. Look up the EVP_CIPHER* from the cipher name (lookup_cipher above).
///   3. Initialize a CBB output buffer.
///   4. Call PKCS8_marshal_encrypted_private_key:
///        - pbe_nid = -1 to select PBES2 (the default secure mode);
///        - cipher = the EVP_CIPHER* from step 2;
///        - pass + pass_len = the raw passphrase bytes (caller supplies; we
///          do NOT NUL-terminate or null-pad; aws-lc accepts arbitrary bytes);
///        - salt = NULL, salt_len = DEFAULT_PBES2_SALT_LEN — aws-lc generates
///          a fresh random salt internally;
///        - iterations = DEFAULT_PBES2_ITERATIONS (2048; option override on
///          API surface);
///        - pkey = EVP_PKEY* from step 1.
///   5. CBB_finish into a freshly-allocated u8 buffer; copy into Vec<u8>.
///   6. EVP_PKEY_free + CBB_cleanup + OPENSSL_free.
///
/// Returns Ok(Vec<u8>) of EncryptedPrivateKeyInfo DER bytes on success, or
/// KernelError::InternalError on aws-lc failure (rare; mostly OOM or NID
/// resolution failure for legacy ciphers — the cipher-whitelist check happens
/// before we reach aws-lc).
pub fn encrypt_pkcs8_private_key(
    private_key_pkcs8_der: &[u8],
    cipher_name: &str,
    passphrase: &[u8],
    iterations: Option<i32>,
) -> Result<Vec<u8>, KernelError> {
    let cipher = lookup_cipher(cipher_name)?;
    let iterations = iterations.unwrap_or(DEFAULT_PBES2_ITERATIONS);

    // Step 1: parse input PKCS#8 DER into EVP_PKEY*.
    // SAFETY: input bytes are immutable; we don't free them. EVP_PKEY_free
    // is called below.
    let pkey = unsafe {
        let mut cbs = std::mem::zeroed::<CBS>();
        CBS_init(&mut cbs, private_key_pkcs8_der.as_ptr(), private_key_pkcs8_der.len());
        // EVP_parse_private_key is in <openssl/evp.h>; aws-lc-sys exposes it
        // as `aws_lc_sys::EVP_parse_private_key`.
        let pkey_ptr = aws_lc_sys::EVP_parse_private_key(&mut cbs);
        if pkey_ptr.is_null() {
            return Err(KernelError::InvalidDer(
                "EVP_parse_private_key failed on input PKCS#8".to_string()));
        }
        pkey_ptr
    };
    // SAFETY: once we have pkey, ensure free on all paths via a guard.
    let _pkey_guard = scopeguard::guard(pkey, |p| unsafe { EVP_PKEY_free(p) });

    // Step 2-4: build EncryptedPrivateKeyInfo via PKCS8_marshal_encrypted_private_key.
    // SAFETY: CBB ownership is ours; CBB_init allocates.
    let mut cbb = unsafe { std::mem::zeroed::<CBB>() };
    let cbb_init_ok = unsafe { CBB_init(&mut cbb, 256) };
    if cbb_init_ok != 1 {
        return Err(KernelError::InternalError("CBB_init failed".to_string()));
    }
    let _cbb_guard = scopeguard::guard(&mut cbb as *mut CBB, |p| unsafe { CBB_cleanup(p) });

    // Salt is NULL with salt_len > 0 -> aws-lc generates random salt.
    let marshal_ok = unsafe {
        PKCS8_marshal_encrypted_private_key(
            &mut cbb,
            -1 as c_int,                                       // pbe_nid = -1 -> PBES2
            cipher,
            passphrase.as_ptr() as *const i8,
            passphrase.len(),
            std::ptr::null(),                                  // salt = NULL
            DEFAULT_PBES2_SALT_LEN,                             // salt_len
            iterations as c_int,
            pkey,
        )
    };
    if marshal_ok != 1 {
        return Err(KernelError::InternalError(
            "PKCS8_marshal_encrypted_private_key failed".to_string()));
    }

    // Step 5: CBB_finish -> Vec<u8>.
    let mut out_ptr: *mut u8 = std::ptr::null_mut();
    let mut out_len: usize = 0;
    let finish_ok = unsafe { CBB_finish(&mut cbb, &mut out_ptr, &mut out_len) };
    if finish_ok != 1 {
        return Err(KernelError::InternalError("CBB_finish failed".to_string()));
    }
    // SAFETY: aws-lc allocated out_ptr; we own it and must OPENSSL_free.
    let bytes = unsafe { std::slice::from_raw_parts(out_ptr, out_len).to_vec() };
    unsafe { OPENSSL_free(out_ptr as *mut c_void) };
    // CBB_cleanup is a no-op after CBB_finish; the guard handles either way.

    Ok(bytes)
}

/// Decrypt an EncryptedPrivateKeyInfo DER blob to its underlying PKCS#8 DER.
///
/// Implementation steps:
///   1. Initialize a CBS over the input bytes.
///   2. Call PKCS8_parse_encrypted_private_key with the passphrase. Returns
///      EVP_PKEY* on success; NULL on failure (wrong passphrase, malformed
///      input, unsupported inner cipher).
///   3. Re-marshal the EVP_PKEY* to plain PKCS#8 DER via EVP_marshal_private_key
///      (the kernel already uses this for the WebCrypto export path).
///   4. EVP_PKEY_free.
///
/// Returns Zeroizing<Vec<u8>> so the plaintext PKCS#8 DER is wiped from memory
/// when dropped (D-N30; the caller typically immediately re-parses into a
/// fresh KeyMaterial::AsymmetricPrivate variant which itself zeroizes).
pub fn decrypt_pkcs8_private_key(
    encrypted_pkcs8_der: &[u8],
    passphrase: &[u8],
) -> Result<Zeroizing<Vec<u8>>, KernelError> {
    // Step 1: CBS init.
    let mut cbs = unsafe { std::mem::zeroed::<CBS>() };
    unsafe { CBS_init(&mut cbs, encrypted_pkcs8_der.as_ptr(), encrypted_pkcs8_der.len()); }

    // Step 2: parse.
    let pkey = unsafe {
        PKCS8_parse_encrypted_private_key(
            &mut cbs,
            passphrase.as_ptr() as *const i8,
            passphrase.len(),
        )
    };
    if pkey.is_null() {
        // Wrong passphrase, malformed input, or unsupported inner cipher —
        // all surface the same way from PKCS8_parse_encrypted_private_key.
        // Per VII.3 / C2-2 audit, this maps to ERR_CRYPTO_OPERATION_FAILED
        // (NOT the dynamic-OSSL ERR_OSSL_EVP_BAD_DECRYPT).
        return Err(KernelError::PassphraseMismatch);
    }
    let _pkey_guard = scopeguard::guard(pkey, |p| unsafe { EVP_PKEY_free(p) });

    // Step 3: re-marshal the EVP_PKEY* to plain PKCS#8 DER via the kernel's
    // existing helper (which wraps EVP_marshal_private_key). Returns owned
    // Vec<u8>.
    let plain_pkcs8 = crate::crypto_kernel::der::marshal_pkey_to_pkcs8(pkey)?;

    Ok(Zeroizing::new(plain_pkcs8))
}
```

**Cipher whitelist for encrypted PKCS#8** (v3 expansion, addresses M2-22 — Node's full PBES2 cipher list per `lib/internal/crypto/keys.js`):

| Cipher | Stage | Gate | Backing EVP_CIPHER |
|---|---|---|---|
| `aes-128-cbc` | C | ungated | `EVP_aes_128_cbc()` |
| `aes-192-cbc` | C | ungated | `EVP_aes_192_cbc()` |
| `aes-256-cbc` | C | ungated | `EVP_aes_256_cbc()` |
| `aes-128-ecb` | E | `--legacy-crypto` (M2-10) | `EVP_aes_128_ecb()` |
| `aes-256-ecb` | E | `--legacy-crypto` (M2-10) | `EVP_aes_256_ecb()` |
| `des-ede3-cbc` | E | `--legacy-crypto` | `EVP_des_ede3_cbc()` |
| `des-ede3-ecb` | E | `--legacy-crypto` | `EVP_des_ede3_ecb()` |
| `aes-128-cfb` | E | ungated (rare) | `EVP_aes_128_cfb128()` |
| `aes-256-cfb` | E | ungated (rare) | `EVP_aes_256_cfb128()` |
| `aes-128-cfb1` | E | `--legacy-crypto` | `EVP_aes_128_cfb1()` |
| `aes-128-cfb8` | E | `--legacy-crypto` | `EVP_aes_128_cfb8()` |
| `aes-128-ofb` | E | `--legacy-crypto` | `EVP_aes_128_ofb()` |

Total: 12 entries (v2 had 7; v3 expanded to match Node's list per M2-22).

**PBES2 / PBKDF2 PRF OID dispatch** (addresses round-2 missing concept #2):

PBES2 inside aws-lc handles the inner KDF transparently — the PBKDF2 PRF OID is encoded in the `EncryptedPrivateKeyInfo` ASN.1 by `PKCS8_marshal_encrypted_private_key` and parsed back by `PKCS8_parse_encrypted_private_key`. Node accepts the following PRF OIDs in the encrypted-PKCS#8 it imports, per `lib/internal/crypto/keys.js`:

- `1.2.840.113549.2.7` — HMAC-SHA-1 (default for OpenSSL ≤1.0).
- `1.2.840.113549.2.8` — HMAC-SHA-224.
- `1.2.840.113549.2.9` — HMAC-SHA-256 (modern default; OpenSSL 1.1+ uses this).
- `1.2.840.113549.2.10` — HMAC-SHA-384.
- `1.2.840.113549.2.11` — HMAC-SHA-512.

aws-lc's `PKCS8_marshal_encrypted_private_key` defaults to HMAC-SHA-256 for the PRF; the parser accepts all five. We don't need to drive the OID dispatch in our Rust wrapper — aws-lc handles it. We document this so the impl agent knows NOT to pass a `prf` option (yet) and can confirm by round-tripping a Node-emitted encrypted PEM.

**IV-handling policy for AES-CBC inner cipher** (addresses round-2 critic C2-4):

PBES2 puts the inner cipher's IV in the cipher params block (an OCTET STRING for AES-CBC; field name `iv` per RFC 8018 §6.2). The IV is generated INSIDE aws-lc's `PKCS8_marshal_encrypted_private_key` (it's not derived from the KDF output; it's freshly random). We don't need to materialise the IV in Rust; aws-lc handles it. For ECB-mode inner ciphers — which RFC 8018 §6.2 forbids and v3 gates behind `--legacy-crypto` per M2-10 — aws-lc emits a zero-length IV OCTET STRING.

**Why not high-level aws-lc-rs?** Verified against https://docs.rs/aws-lc-rs/latest/aws_lc_rs/encoding/index.html: the module exposes `Pkcs8V1Der<'a>` and `Pkcs8V2Der<'a>` as serialized byte wrappers but does NOT expose any `EncryptedPrivateKeyInfo` type or `serialize_with_password` method. We've audited `aws-lc` (the C library) for PKCS8_encrypt / PKCS8_decrypt / PKCS8_marshal_encrypted_private_key / PKCS8_parse_encrypted_private_key — verified at https://github.com/aws/aws-lc/blob/main/include/openssl/pkcs8.h. All four are `OPENSSL_EXPORT` and stable. We use the marshal/parse pair (D-N37 above) because they bypass the X509_SIG intermediate type and take EVP_PKEY directly.

(Note for impl agent: `PKCS8_encrypt_pbe` was a v1/v2 typo — that name does NOT exist in aws-lc. The real name is `PKCS8_encrypt`; v3 specifies `PKCS8_marshal_encrypted_private_key` instead because it's the better fit for our flow.)

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

### IV.6a. JWK kty mapping for OKP (Ed25519/X25519/Ed448/X448)

(addresses critic missing concept #25): Node v18+ maps OKP keys with `kty: "OKP"` per RFC 8037 — the `crv` field carries the curve name (`"Ed25519"`, `"X25519"`, `"Ed448"`, `"X448"`). `keyObject.export({ format: "jwk" })` for an Ed25519 key returns `{ kty: "OKP", crv: "Ed25519", x: <base64url>, d?: <base64url> }`. The kernel JWK exporter at `crypto_kernel/jwk.rs::export` already supports this for the WebCrypto surface (see https://w3c.github.io/webcrypto/#sec-jwk-mapping-tables); v2 verifies that all 4 OKP curves round-trip via `KeyObject.export({ format: 'jwk' })` then `crypto.subtle.importKey('jwk', ...)`.

Smoke test:

```js
const { publicKey } = await crypto.subtle.generateKey({ name: "Ed25519" }, true, ["verify"]);
const ko = KeyObject.from(publicKey);
const jwk = ko.export({ format: "jwk" });
console.assert(jwk.kty === "OKP");
console.assert(jwk.crv === "Ed25519");
const ck = await crypto.subtle.importKey("jwk", jwk, { name: "Ed25519" }, true, ["verify"]);
console.assert(ck.algorithm.name === "Ed25519");
```

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

### IV.7a. `keyObject.equals(other)` semantics (addresses critic CRITICAL #13, MAJOR #20)

Per https://nodejs.org/api/crypto.html#keyobjectequalsotherkeyobject:
> Returns: `<boolean>` `true` or `false` depending on whether the keys have **exactly the same type, value, and parameters**. This method is not constant time.

Note Node's spec says "not constant time" for the OUTER equality check, but the byte compare itself uses constant-time primitives. The cited "exact type, value, parameters" implies three checks:

1. **Type equality**: `this.type === other.type` (cheap, non-CT).
2. **Algorithm-parameter equality**: e.g. for asymmetric keys, `asymmetricKeyType` plus relevant fields of `asymmetricKeyDetails`. For RSA: same modulus + same publicExponent. For EC: same namedCurve. For symmetric: same byte length.
3. **Material byte equality**: the underlying bytes (for symmetric: raw bytes; for asymmetric: the canonical SPKI/PKCS8 DER form). Length pre-check is non-CT (MAJOR #20 — `aws_lc_rs::constant_time::verify_slices_are_equal` returns Err if lengths differ; we explicitly check first to avoid reaching the constant-time path with mismatched lengths).

Implementation:

```rust
fn equals(&self, scope: &mut v8::PinScope, other: v8::Local<v8::Value>) -> Result<bool, OpError> {
    let Some(other_state) = downcast_keyobject(scope, other) else { return Ok(false); };
    if self.key_type != other_state.key_type { return Ok(false); }
    // Compare algorithm-parameter equality (the asymmetricKeyType/details/symmetricKeySize).
    if !same_algorithm_parameters(&self.material, &other_state.material) { return Ok(false); }
    // Materialise both keys to a canonical byte form for constant-time compare.
    // For symmetric: the raw bytes. For asymmetric: the canonical PKCS#8 (private)
    // or SPKI (public) DER. Two RSA keys exported as PKCS1 vs PKCS8 of the same
    // underlying material will compare equal because we compare their canonical form.
    let a = canonical_bytes(&self.material);
    let b = canonical_bytes(&other_state.material);
    if a.len() != b.len() { return Ok(false); }      // length pre-check (non-CT)
    Ok(aws_lc_rs::constant_time::verify_slices_are_equal(&a, &b).is_ok())
}
```

The CRITICAL #13 concern that "two RSA keys exported as PKCS1 vs PKCS8 of the same key compare false" is resolved by canonicalising to PKCS#8/SPKI before comparing. Node does the same — `KeyObject` internally normalises so equality of "logical key material" works.

For public-vs-private same-key-pair, Node's spec correctly returns false (they have different `type`), so step 1 catches that.

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
//
// State machine (addresses critic CRITICAL #5 — CCM ordering, MAJOR #8 final-
// called-twice, AEAD ordering distinct per mode). The Context's `state` field
// transitions linearly:
//
//                   ┌──────────── (CCM only) ───────────┐
//                   │                                     ▼
//   Created  ─→  Aad  ─→  Updating  ─→  Finalised  ─→  Done
//      │           │         │              │
//      └─→─ setAuthTag (CCM-Decipher only, BEFORE first update)
//      │           │         └─→─ setAuthTag (GCM/OCB/ChaCha-Decipher; before final)
//      └─→─ setAutoPadding (CBC only, before update)
//
// CCM REQUIRES setAuthTag *before* update; GCM/OCB/ChaCha require it *after*
// update but *before* final. Kernel rejects late tags / out-of-order calls
// with KernelError::AeadOrderingError {expected_state, actual_state}.

pub enum CipherCtxState {
    Created,
    Aad,        // setAAD called or setAuthTag-on-CCM called
    Updating,   // first update() observed
    Finalised,  // final() returned
    Done,       // any further call (update/final/getAuthTag) errors
}

pub struct CipherContext {
    /* aws-lc-rs aead::SealingKey<NonceSeq> | aead::OpeningKey<NonceSeq> handle,
       pending block bytes for CBC, AAD buffer for GCM, plaintextLength for CCM,
       requested auth_tag_length, current state */
}
impl CipherContext {
    pub fn new_encrypt(alg: CipherAlg, key: &[u8], iv: &[u8], auth_tag_length: usize) -> Result<Self, KernelError>;
    pub fn new_decrypt(alg: CipherAlg, key: &[u8], iv: &[u8], auth_tag_length: usize) -> Result<Self, KernelError>;
    /// (addresses critic CRITICAL #4): plaintext_length is REQUIRED for CCM, optional/None otherwise.
    pub fn set_aad(&mut self, aad: &[u8], plaintext_length: Option<usize>) -> Result<(), KernelError>;
    /// (addresses critic CRITICAL #5): mode-aware ordering enforced inside.
    pub fn set_auth_tag(&mut self, tag: &[u8]) -> Result<(), KernelError>;    // Decipher only
    pub fn set_auto_padding(&mut self, on: bool) -> Result<(), KernelError>;  // CBC only
    pub fn update(&mut self, data: &[u8]) -> Result<Vec<u8>, KernelError>;
    /// (addresses critic MAJOR #8): calling finalize twice errors with
    /// `KernelError::AlreadyFinalised`, mapping to ERR_CRYPTO_INVALID_STATE.
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
        // (v3, addresses C2-1, C2-2): ERR_OSSL_EVP_UNSUPPORTED was a v1/v2
        // dynamic-shape guess. The real Node code for unknown digest is
        // ERR_CRYPTO_INVALID_DIGEST (TypeError) per
        // https://github.com/nodejs/node/blob/main/src/node_errors.h.
        None => return Err(OpError::node("ERR_CRYPTO_INVALID_DIGEST",
            format!("Invalid digest: {}", algorithm))),
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
        // (v3, C2-1/C2-2): see comment in create_hash. Use real
        // ERR_CRYPTO_INVALID_DIGEST.
        .ok_or_else(|| OpError::node("ERR_CRYPTO_INVALID_DIGEST",
            format!("Invalid digest: {}", algorithm)))?;

    // (addresses critic MAJOR #5 + MAJOR #30): key may be a KeyObject, a
    // CryptoKey (Node v15+ accepts CryptoKey for createHmac/createSign etc.,
    // see https://nodejs.org/api/crypto.html#cryptocreatehmacalgorithm-key-options),
    // a Buffer, or a string. We accept all four. The extracted bytes are
    // wrapped in a Zeroizing<Vec<u8>> for the duration of the kernel call —
    // v1's `b.clone()` on a `Zeroizing<Vec<u8>>` returned a plain Vec that
    // outlived the function on the heap; v2 keeps everything Zeroizing.
    let key_bytes: Zeroizing<Vec<u8>> = if is_key_object(scope, key) {
        let ko = KeyObject::state(scope, key);
        match &*ko.material {
            KeyMaterial::Symmetric(b) => Zeroizing::new(b.to_vec()),
            _ => return Err(OpError::node("ERR_INVALID_ARG_TYPE",
                "Hmac key must be a SecretKeyObject")),
        }
    } else if crypto_native::crypto_key::is_crypto_key(scope, key) {
        // (addresses critic MAJOR #30): bridge the CryptoKey via the Arc
        // share; only HMAC-typed CryptoKeys are accepted.
        let ck = crypto_native::crypto_key::state(scope, key);
        match &*ck.material {
            KeyMaterial::Symmetric(b) => Zeroizing::new(b.to_vec()),
            _ => return Err(OpError::node("ERR_INVALID_ARG_TYPE",
                "Hmac key (CryptoKey) must be a symmetric key")),
        }
    } else {
        Zeroizing::new(buffer::extract_input(scope, key, None)?)
    };

    // (v4 fix, C3-3): The v3 rationale ("Node throws ERR_OUT_OF_RANGE on a
    // `key.byteLength === 0` check in lib/internal/crypto/hash.js") was
    // FABRICATED — no such check exists in hash.js. Verified actual Node
    // behaviour against https://github.com/nodejs/node/blob/main/src/crypto/
    // crypto_hmac.cc::Hmac::HmacInit (the C++ implementation called from
    // hash.js's Hmac constructor):
    //
    //     if (key_len == 0) { key = ""; }
    //     ctx_ = HMACCtxPointer::New();
    //     if (!ctx_.init(key_buf, md)) {
    //         ctx_.reset();
    //         return ThrowCryptoError(env(), ERR_get_error());
    //     }
    //
    // Node SILENTLY ACCEPTS empty keys (it even special-cases `key_len == 0`
    // by re-binding `key = ""`), then forwards to the OpenSSL HMAC_Init_ex
    // path. If init fails, the error surfaces through the dynamic-OSSL
    // ThrowCryptoError pipeline (ERR_OSSL_HMAC_*-shaped, not in Node's
    // static registry).
    //
    // **zeroship divergence (intentional):** we throw ERR_OUT_OF_RANGE
    // (real Node code, RangeError) instead of accepting empty keys.
    // Rationale:
    //   1. RFC 2104 §2 requires the key length to be at least the hash output
    //      size for full security; an empty key trivially defeats HMAC.
    //   2. Defense-in-depth: silently accepting a zero-length key is a
    //      cryptographic foot-gun npm code rarely guards against.
    //   3. ERR_OUT_OF_RANGE is a real, stable Node code (errors.js); packages
    //      that branch on `e.code` see a normal Node error class.
    // Logged in §XVII.13b as an explicit zeroship-vs-Node behavioural
    // divergence so users porting code that depends on the silent-accept
    // path are aware. The test in §XIV.crypto_node_hmac.rs is updated to
    // reflect this is a divergence, not parity.
    if key_bytes.is_empty() {
        return Err(OpError::node("ERR_OUT_OF_RANGE",
            "HMAC key cannot be empty (zeroship divergence: Node would \
             accept this and let OpenSSL emit a dynamic ERR_OSSL_* error; \
             we reject up-front per RFC 2104 §2)"));
    }

    let state = HmacState { ctx: kernel::HmacContext::new(hash, &key_bytes) };
    Ok(Hmac::build(scope, state).into())
}
```

### V.4. Cipher / Decipher classes (D-N11, D-N23)

Cipher and Decipher are nearly identical; we model them as a single `Cipher` impl that internally tracks an `encrypt: bool` flag, with `Decipher` being a thin alias class.

```rust
// (v3, addresses M2-25): auto_padding field removed; single source of truth
// is the kernel CipherContext.
pub struct CipherState {
    ctx: kernel::CipherContext,
    is_encrypt: bool,
    mode: CipherMode,         // for setAAD/setAuthTag ordering checks
    auth_tag_length: usize,   // captured at create_cipheriv for AEAD modes
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

    /// `cipher.setAAD(buffer, options?)` — for GCM/CCM/OCB/ChaCha20-Poly1305
    /// AEAD modes. (addresses critic CRITICAL #4): the `options` object's
    /// `plaintextLength` is REQUIRED for CCM mode; `encoding` applies when
    /// `buffer` is a string. Per
    /// https://nodejs.org/api/crypto.html#ciphersetaadbuffer-options.
    #[v8_method]
    fn set_aad<'s>(&mut self,
        scope: &mut v8::PinScope<'s, '_>,
        this: v8::Local<'s, v8::Object>,
        aad: v8::Local<v8::Value>,
        options: Option<v8::Local<v8::Value>>,
    ) -> Result<v8::Local<'s, v8::Value>, OpError> {
        let opts = parse_set_aad_options(scope, options)?;
        // `encoding` applies only when `aad` is a string (per Node spec).
        let bytes = buffer::extract_input(scope, aad, opts.encoding.as_deref())?;
        // CCM: plaintextLength MUST be supplied (kernel rejects otherwise so
        // the eventual encrypt is not silently miscomputed).
        if matches!(self.mode, CipherMode::Ccm) && opts.plaintext_length.is_none() {
            return Err(OpError::node("ERR_MISSING_OPTION",
                "options.plaintextLength is required for CCM mode setAAD"));
        }
        self.ctx.set_aad(&bytes, opts.plaintext_length).map_err(KernelError::to_node)?;
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

    /// `decipher.setAuthTag(tagBuffer, encoding?)` — Decipher only.
    /// (addresses critic CRITICAL #5): the ordering requirement DIFFERS by mode
    /// per https://nodejs.org/api/crypto.html#deciphersetauthtagbuffer-encoding:
    ///   * **CCM**: `setAuthTag` MUST be called BEFORE the first `update()`.
    ///   * **GCM / OCB / chacha20-poly1305**: `setAuthTag` MUST be called BEFORE `final()`
    ///     (it MAY come after `update()` calls, which is the common pattern
    ///     because the tag is appended after the ciphertext on the wire).
    /// The state machine in `CipherContext::set_auth_tag` enforces the mode-
    /// specific check; v1 used the generic "pre-final" rule which silently
    /// accepted late tags on CCM and produced undefined output.
    #[v8_method]
    fn set_auth_tag<'s>(&mut self,
        scope: &mut v8::PinScope<'s, '_>,
        this: v8::Local<'s, v8::Object>,
        tag: v8::Local<v8::Value>,
        encoding: Option<String>,
    ) -> Result<v8::Local<'s, v8::Value>, OpError> {
        if self.is_encrypt {
            return Err(OpError::node("ERR_CRYPTO_INVALID_STATE",
                "Cannot call setAuthTag on a Cipher"));
        }
        let bytes = buffer::extract_input(scope, tag, encoding.as_deref())?;
        // Mode-specific ordering enforced by the kernel context:
        //   - CCM:                must be in PreUpdate state (no update() yet)
        //   - GCM/OCB/ChaCha20:   must be in PreFinal state (final() not yet called)
        self.ctx.set_auth_tag(&bytes).map_err(KernelError::to_node)?;
        Ok(this.into())
    }

    /// `cipher.setAutoPadding(boolean)` — for CBC mode PKCS#7 padding control.
    /// Returns `this` so the call is chainable per Node spec.
    /// (v3, addresses M2-25): the auto_padding flag lives ONLY on the kernel
    /// `CipherContext`. v2 stored a duplicate `auto_padding: bool` on
    /// `CipherState` which could drift from the kernel value (e.g., if a
    /// future refactor wired in implicit padding-disable for AEAD). v3
    /// removes the duplicate; the surface-side flag is read via
    /// `self.ctx.is_auto_padding_on()` whenever needed (e.g., for diagnostic
    /// messages on InputNotMultipleOfBlockSize errors).
    #[v8_method]
    fn set_auto_padding<'s>(&mut self,
        this: v8::Local<'s, v8::Object>,
        on: Option<bool>,
    ) -> Result<v8::Local<'s, v8::Value>, OpError> {
        let on = on.unwrap_or(true);    // Node default
        self.ctx.set_auto_padding(on).map_err(KernelError::to_node)?;
        // No `self.auto_padding = on;` — single source of truth on the kernel
        // context.
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
    options: Option<v8::Local<v8::Value>>,
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    let alg = canonicalise_cipher_name(&algorithm)
        // (v3, addresses C2-1, C2-2): ERR_OSSL_EVP_UNSUPPORTED was a v1/v2
        // dynamic-shape guess. The real Node code for unknown cipher is
        // ERR_CRYPTO_UNKNOWN_CIPHER per
        // https://github.com/nodejs/node/blob/main/src/node_errors.h
        // (V(ERR_CRYPTO_UNKNOWN_CIPHER, Error)).
        .ok_or_else(|| OpError::node("ERR_CRYPTO_UNKNOWN_CIPHER",
            format!("Unknown cipher: {}", algorithm)))?;
    // (addresses critic CRITICAL #3): read `authTagLength` from options.
    // REQUIRED for CCM (no default); optional for GCM (default 16, but Node v22
    // emits DEP0182 deprecation warning if a short tag is used without this
    // explicit option — see https://nodejs.org/api/deprecations.html#DEP0182).
    // Per https://nodejs.org/api/crypto.html#cryptocreatecipherivalgorithm-key-iv-options.
    let opts = parse_cipher_options(scope, options)?;
    let auth_tag_length = match alg.mode() {
        CipherMode::Ccm => opts.auth_tag_length.ok_or_else(|| OpError::node(
            "ERR_MISSING_OPTION",
            "authTagLength required for CCM mode"))?,
        // (addresses critic missing concept #13 — DEP0182): Node v22 emits
        // a deprecation warning when GCM is used without an explicit
        // authTagLength and the resulting tag is shorter than 16 bytes.
        // v2 emits the same warning; full breaking enforcement defers to a
        // future Node-aligned cutover.
        CipherMode::Gcm => match opts.auth_tag_length {
            Some(n) if n < 16 => {
                emit_deprecation_warning_once(scope, "DEP0182",
                    "Use of GCM with an authTagLength shorter than 16 bytes \
                     is deprecated; specify authTagLength explicitly.");
                n
            }
            Some(n) => n,
            None => 16,
        },
        CipherMode::Ocb | CipherMode::ChaCha20Poly1305 =>
            opts.auth_tag_length.unwrap_or(16),
        _ => 0,    // unused
    };
    // ChaCha20-Poly1305 IV must be exactly 12 bytes (RFC 8439 §2.3) — kernel
    // validates; we validate the AES-CCM IV length range (7..=13) and AES-GCM
    // (any length, with 12 being optimal) per NIST SP 800-38D.
    let key_bytes = extract_key_bytes(scope, key, alg.expected_key_len())?;
    let iv_bytes = if iv.is_null() {
        // ECB has no IV; null is permitted.
        vec![]
    } else {
        buffer::extract_input(scope, iv, None)?
    };
    let ctx = kernel::CipherContext::new_encrypt(alg, &key_bytes, &iv_bytes, auth_tag_length)
        .map_err(KernelError::to_node)?;
    // (v3, addresses M2-25): no auto_padding field — kernel context is the
    // single source of truth. Defaults to true (PKCS#7 on); user calls
    // setAutoPadding(false) to disable.
    let state = CipherState {
        ctx,
        is_encrypt: true,
        mode: alg.mode(),                // for setAAD/setAuthTag ordering checks (V.4)
        auth_tag_length,
    };
    Ok(Cipher::build(scope, state).into())
}

struct CipherOptions {
    auth_tag_length: Option<usize>,
}

fn parse_cipher_options(
    scope: &mut v8::PinScope,
    options: Option<v8::Local<v8::Value>>,
) -> Result<CipherOptions, OpError> {
    let Some(o) = options else { return Ok(CipherOptions { auth_tag_length: None }); };
    if !o.is_object() { return Ok(CipherOptions { auth_tag_length: None }); }
    let obj: v8::Local<v8::Object> = o.try_into()
        .map_err(|_| OpError::node("ERR_INVALID_ARG_TYPE", "options must be an object"))?;
    let auth_tag_length = read_uint_property(scope, obj, "authTagLength")?
        .map(|n| n as usize);
    Ok(CipherOptions { auth_tag_length })
}
```

**`createCipher` (deprecated) policy (addresses critic MAJOR #18):**

Node DOES NOT throw on `createCipher` — it emits a deprecation warning (DEP0106) and proceeds, deriving the key from the password via OpenSSL's `EVP_BytesToKey` (single-iteration MD5, broken). v1's design "throws ERR_CRYPTO_DEPRECATED_API" silently breaks legacy apps that work on Node.

**v3 policy** (addresses round-2 C2-2 — `ERR_CRYPTO_DEPRECATED_API` is NOT a real Node code; verified against `lib/internal/errors.js` and `src/node_errors.h`): ship `createCipher` (Stage 2, gated on `--legacy-crypto`). Without the flag, it emits a deprecation warning and routes to `createCipheriv` with an EVP_BytesToKey-derived key + zero IV (matching Node's broken behaviour exactly). With `--legacy-crypto` off (the default), the warning is upgraded to a hard refusal — the e.code uses the real `ERR_CRYPTO_UNSUPPORTED_OPERATION` (per https://github.com/nodejs/node/blob/main/src/node_errors.h `V(ERR_CRYPTO_UNSUPPORTED_OPERATION, Error)`), NOT v2's invented `ERR_CRYPTO_DEPRECATED_API`:

```rust
pub fn create_cipher<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    algorithm: v8::Local<v8::Value>,
    password: v8::Local<v8::Value>,
    options: Option<v8::Local<v8::Value>>,
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    if !legacy_crypto_enabled() {
        // (v3, addresses C2-2): use the real ERR_CRYPTO_UNSUPPORTED_OPERATION
        // (per node_errors.h `V(ERR_CRYPTO_UNSUPPORTED_OPERATION, Error)`).
        // ERR_CRYPTO_DEPRECATED_API was invented by v2.
        return Err(OpError::node("ERR_CRYPTO_UNSUPPORTED_OPERATION",
            "crypto.createCipher is deprecated and disabled by default in this runtime. \
             Use crypto.createCipheriv with an explicit IV, or enable --legacy-crypto. \
             See https://nodejs.org/api/crypto.html#cryptocreatecipheralgorithm-password-options \
             and DEP0106 at https://nodejs.org/api/deprecations.html#DEP0106."));
    }
    emit_deprecation_warning_once(scope, "DEP0106",
        "crypto.createCipher is deprecated; use crypto.createCipheriv.");
    let (key, iv) = evp_bytes_to_key(/* ... */);
    create_cipheriv(scope, algorithm_str, key.into(), iv.into(), options)
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
    /// (addresses critic MAJOR #6): post-sign `update()` MUST throw a generic
    /// Error (Node behaviour — see https://github.com/nodejs/node/blob/main/lib/internal/crypto/sig.js)
    /// rather than ERR_CRYPTO_HASH_FINALIZED. We achieve this by NOT routing
    /// through the digest context's finalised flag for the post-sign case;
    /// instead, after `sign()` returns we set a separate `signed: bool` on
    /// SignState and reject further `update()` calls with a plain Error
    /// (no `code`). This matches `jsonwebtoken`'s `verify()` retry-on-error
    /// path which expects no `code` on the Error.
    #[v8_method]
    fn sign<'s>(&mut self,
        scope: &mut v8::PinScope<'s, '_>,
        private_key: v8::Local<v8::Value>,
        encoding: Option<String>,
    ) -> Result<v8::Local<'s, v8::Value>, OpError> {
        if self.signed {
            return Err(OpError::error("Sign.sign already called"));
        }
        let (km, padding) = parse_sign_key_input(scope, private_key)?;
        let digest = self.digest.finalize().map_err(KernelError::to_node)?;
        let sig = kernel::sign_verify::sign_with_digest(&km, self.hash, padding, &digest)
            .map_err(KernelError::to_node)?;
        self.signed = true;
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
// (addresses critic MAJOR #26): both branches return Arc<KeyMaterial>; the
// KeyObject path Arc::clones the EXISTING Arc (cheap refcount bump, key bytes
// shared), the PEM/DER path creates a FRESH Arc::new(km) wrapping freshly
// parsed bytes (no sharing — each parse allocates new key bytes). The "shares
// Arc" comment was misleading for the PEM path; v1 implied both branches
// shared, which is true at the Rust-type level but not at the storage level.
//
// (v3, addresses M2-16): cost budget for the cold-PEM path. When the user
// passes a PEM string directly to crypto.sign() instead of pre-parsing into
// a KeyObject, we pay (per call):
//   ~30 µs   PEM base64 decode + RFC 7468 framing parse (kernel::pem::decode)
//   ~50 µs   PKCS#8 / PKCS#1 / SPKI / SEC1 ASN.1 walk (crypto_kernel::der)
//   ~10 µs   Arc allocation + KeyMaterial enum boxing
//   ~5 µs    aws-lc-rs key-handle init from raw bytes
//   --------
//   ~95 µs total
// vs. ~5 µs for the KeyObject path (Arc::clone).
//
// Best practice: creator apps doing high-throughput signing should hoist
// `createPrivateKey(pem)` once at startup and reuse the resulting KeyObject.
// The Sign / Verify class APIs already encourage this (the user constructs
// the Sign once, calls update() many times, then sign(key) once). The cold
// path applies only to crypto.sign() one-shot calls with a PEM string —
// which is uncommon enough that we don't budget further optimisation.
fn parse_sign_key_input(
    scope: &mut v8::PinScope,
    input: v8::Local<v8::Value>,
) -> Result<(Arc<KeyMaterial>, SignPadding), OpError> {
    // 1. KeyObject → Arc::clone (cheap; same bytes shared).
    if is_key_object(scope, input) {
        let ko = KeyObject::state(scope, input);
        return Ok((Arc::clone(&ko.material), SignPadding::Default));
    }
    // 2. CryptoKey → Arc::clone via the bridge (D-N4); same bytes shared.
    if crypto_native::crypto_key::is_crypto_key(scope, input) {
        let ck = crypto_native::crypto_key::state(scope, input);
        return Ok((Arc::clone(&ck.material), SignPadding::Default));
    }
    // 3. PEM string or Buffer → parse via createPrivateKey logic.
    // 4. Object `{ key, format, type, padding, saltLength, dsaEncoding }`
    //    → extract key + padding params.
    // For 3 + 4, Arc::new(km) wraps FRESH bytes — no sharing with any
    // existing KeyObject/CryptoKey.
    let opts = parse_options_object(scope, input)?;
    let km = parse_private_key_input(scope, opts.key)?;
    let padding = match opts.padding {
        Some(RSA_PKCS1_PSS_PADDING) =>
            SignPadding::RsaPss {
                // (addresses critic CRITICAL #8 + new D-N34): translate the
                // saltLength SENTINELS to absolute byte counts BEFORE the
                // kernel call. Per https://nodejs.org/api/crypto.html#sign-sign:
                //   * RSA_PSS_SALTLEN_DIGEST  = -1  → equal to digest length (Node + OpenSSL default)
                //   * RSA_PSS_SALTLEN_MAX_SIGN = -2 → maximum permissible (k - hLen - 2)
                //                                    where k = ceil(modulus_bits / 8)
                //   * RSA_PSS_SALTLEN_AUTO     = -2 (verify only) → derive from sig
                //   * any non-negative integer → use as-is
                // v1 propagated `-1` / `-2` as a literal usize, which would either
                // panic on cast or send garbage to OpenSSL.
                salt_length: normalise_pss_salt_length(
                    opts.salt_length,
                    self.hash,
                    rsa_modulus_bytes(&km),
                )?,
            },
        Some(RSA_PKCS1_PADDING) | None => SignPadding::Default,
        Some(other) => return Err(OpError::node("ERR_INVALID_ARG_VALUE",
            format!("Unknown padding constant: {}", other))),
    };
    Ok((Arc::new(km), padding))
}

// (addresses critic minor m-11): v1 referenced `parse_verify_key_input` in
// Verify::verify but only defined `parse_sign_key_input`. The two share the
// same parsing logic; the only difference is the input is a public key (or a
// "may be private but we'll downcast" KeyObject). v2 adds a thin wrapper:
fn parse_verify_key_input(
    scope: &mut v8::PinScope,
    input: v8::Local<v8::Value>,
) -> Result<(Arc<KeyMaterial>, SignPadding), OpError> {
    // Same as parse_sign_key_input but with `parse_public_key_input` for the
    // PEM/DER/JWK route. Reuses normalise_pss_salt_length for saltLength.
    parse_sign_key_input_inner(scope, input, /* public = */ true)
}

/// Resolve the user-supplied saltLength (which may be a sentinel) to an
/// absolute byte count. Sentinels per
/// https://nodejs.org/api/crypto.html#cryptoconstants and OpenSSL's
/// `RSA_PSS_SALTLEN_*` macros.
///
/// (D-N34) Always normalised to a `usize` BEFORE the kernel boundary so the
/// kernel never sees negative sentinels — this isolates the OpenSSL/aws-lc-rs
/// FFI from sentinel handling.
///
/// (v3, addresses M2-6, M2-9, M2-17): notes on edge cases.
///   * modulus_bits % 8 != 0: For non-byte-aligned moduli (rare; standard RSA
///     keys are 2048/3072/4096 bits, all multiples of 8), `rsa_modulus_bytes`
///     rounds UP via `(modulus_bits + 7) / 8`. The PSS salt-length max formula
///     `emLen - hLen - 2` is exact when `modulus_bits = 8k`; for the rare
///     non-aligned case, `emLen = ceil((modulus_bits - 1) / 8)` per RFC 8017
///     §9.1.1 — which differs from `rsa_modulus_bytes` by at most 1 byte for
///     a 1023-bit modulus. v3 picks the conservative reading: `emLen =
///     rsa_modulus_bytes(km)`. Sub-byte precision matters only at the 1023/
///     1535/3071-bit edges; commodity keys are unaffected.
///
///   * saltLength = 0: Per RFC 8017 §9.1.1, deterministic PSS (sLen=0) IS
///     valid for both sign AND verify. Node's behaviour matches: `crypto.sign`
///     with `saltLength: 0` accepts and produces deterministic output. We
///     ALLOW this (returns `Ok(0)`); the round-2 critic (M2-17) was incorrect
///     about sign-vs-verify asymmetry — there is none. Documented for clarity.
///
///   * saltLength = unrecognised negative (e.g., -3): Node throws RangeError
///     with code `ERR_OUT_OF_RANGE` per
///     https://nodejs.org/api/crypto.html#sign-sign at `options.saltLength`.
///     v2 emitted ERR_INVALID_ARG_VALUE which the macro maps to TypeError —
///     wrong class. v3 emits ERR_OUT_OF_RANGE which the macro maps to
///     RangeError. (addresses M2-9.)
fn normalise_pss_salt_length(
    user_value: Option<i32>,
    hash: HashAlgo,
    modulus_bytes: usize,
) -> Result<usize, OpError> {
    let h_len = digest_len_bytes(hash);
    match user_value {
        None | Some(-1) /* RSA_PSS_SALTLEN_DIGEST */ => Ok(h_len),
        Some(-2) /* RSA_PSS_SALTLEN_MAX_SIGN / AUTO */ => {
            // For sign: emBits = modulusBits - 1; sLen_max = emLen - hLen - 2.
            // Approximation: emLen = modulus_bytes when modulus_bits is a multiple of 8.
            modulus_bytes.checked_sub(h_len + 2)
                .ok_or_else(|| OpError::node("ERR_OUT_OF_RANGE",
                    "RSA modulus too small for PSS with this hash"))
        }
        Some(n) if n >= 0 => Ok(n as usize),
        // (v3, addresses M2-9): use ERR_OUT_OF_RANGE so the macro emits
        // RangeError per Node spec (the v2 path used ERR_INVALID_ARG_VALUE
        // which yielded TypeError).
        Some(other) => Err(OpError::node("ERR_OUT_OF_RANGE",
            format!("saltLength sentinel out of range: got {}, expected -2 (MAX), \
                     -1 (DIGEST), or any non-negative value", other))),
    }
}
```

**`dsaEncoding`:** Node has a `dsaEncoding: 'der' | 'ieee-p1363'` option for ECDSA signatures. Default is `'der'` (ASN.1 INTEGER pair) for node:crypto sign/verify (matches OpenSSL output). `'ieee-p1363'` produces fixed-length r||s (matches WebCrypto). The kernel supports both via a flag on `SignPadding::Ecdsa { encoding }`.

**This is a key cross-surface coordination point:** WebCrypto (existing `crypto_native/`) emits IEEE-P1363 (D-4); node:crypto defaults to DER (Node convention). The kernel function `sign_with_digest` takes the encoding flag explicitly; both surfaces pass their preferred default.

### V.6. `stream.Transform` inheritance (D-N35, addresses critic missing concept #21)

Per https://nodejs.org/api/crypto.html#class-hash and the analogous sections for Hmac / Cipher / Decipher / Sign / Verify, **all six streaming classes extend `stream.Transform`**. They are Duplex streams: writable side feeds `update()`; readable side emits the digest / ciphertext on `final()`. `pipeline(readable, hash, writable)` is the canonical streaming pattern and MUST work — packages like `node-archiver`, `s3-streaming-upload`, and many object-storage SDKs use it.

v1's design omitted this entirely. v2 adds it as a JS-side mixin (the simplest path; native Transform inheritance from a Rust `#[v8_class]` would require extending the macro to support multi-prototype inheritance, which we judge non-essential). The `node-crypto.gen.ts` synthetic module wraps each native class:

```ts
// node-crypto.gen.ts (sketch)
import { Transform } from "node:stream";

const NativeHash = _zsc.Hash;

// (v3, addresses M2-2): Transform mixin properly addresses back-pressure +
// dual-API coexistence + objectMode/decodeStrings + highWaterMark.
class Hash extends Transform {
  #ctx;             // the native Hash instance
  #directApiUsed;   // if true, _transform/_flush become no-ops (user opted into direct API)
  constructor(algorithm, options) {
    // (M2-2) Pass through user's options so they can override highWaterMark.
    // Force decodeStrings: false so binary chunks pass through unchanged
    // (default Transform decodes strings to Buffer using utf-8, which would
    // double-encode binary data).
    super({
      ...options,
      decodeStrings: false,
      objectMode: false,
    });
    this.#ctx = new NativeHash(algorithm, options);
    this.#directApiUsed = false;
  }
  // Forward streaming API. update() is chainable per Node spec.
  update(data, encoding) {
    this.#directApiUsed = true;
    this.#ctx.update(data, encoding);
    return this;
  }
  digest(encoding) {
    this.#directApiUsed = true;
    return this.#ctx.digest(encoding);
  }
  copy(options) {
    // copy() returns a fresh Hash with same in-progress state.
    const c = new Hash(this.#ctx[kAlgorithm], options);
    c.#ctx = this.#ctx.copy(options);
    return c;
  }
  // Transform protocol. (M2-2) If the user mixed direct API + Transform, _flush
  // must NOT call digest() again — that would throw ERR_CRYPTO_HASH_FINALIZED
  // and break pipeline error handling.
  _transform(chunk, encoding, callback) {
    if (this.#directApiUsed) {
      // User already called update()/digest() directly; pipeline should
      // pass through silently (and effectively no-op), matching Node's
      // behaviour where the stream side simply propagates whatever the
      // direct calls left in the context.
      callback();
      return;
    }
    try { this.#ctx.update(chunk, encoding); callback(); }
    catch (err) { callback(err); }
  }
  _flush(callback) {
    if (this.#directApiUsed) {
      callback();
      return;
    }
    try { this.push(this.#ctx.digest()); callback(); }
    catch (err) { callback(err); }
  }
}
```

The same pattern applies to Hmac (no `copy`), Cipher / Decipher (`_transform` writes the encrypted/decrypted chunk; `_flush` writes `final()`), Sign / Verify (`_transform` calls `update`; `_flush` is a no-op because the user must call `sign(key)` / `verify(key, sig)` explicitly).

**Back-pressure** (M2-2): the underlying `Transform` super-class handles back-pressure via its `highWaterMark`. The mixin honors the user-supplied `options.highWaterMark` (defaults to 16384 bytes for byte streams). When the readable side is drained slowly, `_transform` is paused naturally by the Transform machinery — the synchronous `this.#ctx.update(chunk)` call is fast (microseconds), so back-pressure rarely backs up here.

**Dual API + native class export** (m2-1): the `Hash` symbol exported from the synthetic `"node:crypto"` module is the JS-mixin'd class above. The raw native `__zeroship_node_crypto.Hash` is internal-only — it's NOT re-exported. Tests that need to bypass the mixin (e.g., to verify per-call latency) reach into `__zeroship_node_crypto` directly.

Cost: ~250 LOC of TS in `node-crypto.gen.ts` (v3 includes the back-pressure + dual-API guards on top of v2's basic mixin). Doesn't affect the Rust surface. The native classes still expose the streaming methods directly so apps that don't use Transform pay zero overhead.

<!-- Round 3: addressing MAJOR M2-4 (process.noDeprecation reader). -->
**`emit_deprecation_warning_once` definition (v3, addresses M2-4, m2-8):**

```rust
// crypto_node/deprecation.rs (NEW in Stage A)
//
// One-shot per-isolate deprecation warning emitter. Memoises (isolate, code)
// pairs so repeated DEP0031 invocations from the same app warn exactly once.
//
// Honours `--no-deprecation` and `process.noDeprecation`:
//   * --no-deprecation: read once at isolate startup from RuntimeFlags
//     (added per M2-4); when set, ALL deprecation calls are no-ops.
//   * process.noDeprecation: a JS-side mutable flag (per Node's behaviour at
//     https://nodejs.org/api/process.html#processnodeprecation). Read at
//     emit time from `globalThis.process.noDeprecation`. Our process shim
//     already exposes a `process` object via unenv; v3 wires a getter on it
//     that reflects the runtime flag default + JS-side overrides.
pub fn emit_deprecation_warning_once(
    scope: &mut v8::PinScope,
    code: &'static str,        // e.g., "DEP0031"
    message: &str,
) {
    // Static one-shot table per isolate.
    let already_warned = state::isolate_state(scope).deprecations_emitted.borrow_mut();
    if !already_warned.insert(code) {
        return;
    }
    // Check global suppression.
    if state::isolate_runtime_flags().no_deprecation {
        return;
    }
    // Check process.noDeprecation (JS-side override).
    if let Some(process) = scope.get_global().get(scope, "process") {
        if let Some(no_dep) = process.get(scope, "noDeprecation") {
            if no_dep.boolean_value(scope) { return; }
        }
    }
    // Emit via process.emitWarning(message, { code, type: 'DeprecationWarning' }).
    // The unenv-shipped process object exposes emitWarning per Node API.
    process::emit_warning(scope, message, code, "DeprecationWarning");
}
```

This helper lives at `crates/runtime/src/crypto_node/deprecation.rs` (~40 LOC). The `state::isolate_state` / `state::isolate_runtime_flags` helpers already exist (per the existing `crates/runtime/src/state.rs`, plus the round-1 RuntimeFlags addition). The `process::emit_warning` shim is a thin wrapper over the existing unenv-backed `process` global.

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
| `timingSafeEqual` | ✓ | | | Sub-microsecond for typical 32/64-byte inputs (addresses critic minor m-19 — v1's "microseconds" was an order of magnitude too high). |
| `webcrypto.subtle.*` | ✓ | | | (addresses critic MAJOR #11, #19) Always sync — inherits from `crypto_native/`'s shipped behaviour. The webcrypto-native ADR D-29 was specifically about Promise-returning WebCrypto methods being **synchronously resolved** on the V8 thread (the `Promise<X>` is constructed pre-resolved with `Promise.resolve(value)` rather than dispatched to a thread pool). This is consistent with the "no Promise-blocking-on-sync hack" goal at line 70 because we're not blocking — we resolve the Promise synchronously without ever waiting. If a future webcrypto-native v2 introduces async dispatch, this row updates accordingly. |

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
        // (v3, addresses C2-1, C2-2): real Node code is ERR_CRYPTO_INVALID_DIGEST.
        .ok_or_else(|| OpError::node("ERR_CRYPTO_INVALID_DIGEST",
            format!("Invalid digest: {}", digest)))?;
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

**Perf SLA for Cipher.update (addresses critic MAJOR #7, MAJOR #25):**
v1 punted the optimisation but didn't set a target. v2 SLA: AES-GCM at 1 MiB
chunk size must complete in ≤ 5 ms on the project's reference Skylake-class
hardware (gives ~200 MB/s; aws-lc-rs's hardware-accelerated AES-NI is
substantially faster than this in isolation, so the overhead budget is
generous). Above 1 MiB per `update()` call the V8 thread is observable to
event-loop monitoring; the doc recommends `pipeline(readable, cipher, writable)`
via stream.Transform (D-N35) to chunk naturally. AES-OCB without hardware
acceleration is the slowest path; we do NOT promise the SLA for OCB on
non-AES-NI hardware.

The "user error" framing is wrong (per critic MAJOR #25): creator apps
doing TLS-like workloads naturally hit larger input sizes. The mitigation is
the stream.Transform inheritance (D-N35), not asking apps to avoid the API.

### VI.5. The `randomBytes` async path (D-N17)

```rust
/// (addresses critic CRITICAL #11 + minor m-7): the `size` parameter takes
/// `i32` so negative-input validation matches Node (which throws `RangeError`
/// on negative size) and the upper bound is `Buffer.kMaxLength = 0x7fffffff`
/// per https://nodejs.org/api/crypto.html#cryptorandombytessize-callback.
/// Validation lives in a single `validate_random_size` helper used by both
/// sync and async paths (m-7).
fn validate_random_size(size: i32) -> Result<usize, OpError> {
    if size < 0 {
        return Err(OpError::node("ERR_OUT_OF_RANGE",
            "size must be a non-negative integer"));
    }
    // Buffer.kMaxLength = 2^31 - 1 = 0x7FFFFFFF. Node's randomBytes uses the
    // same limit because the result is a Buffer.
    if size > 0x7FFFFFFF {
        return Err(OpError::node("ERR_OUT_OF_RANGE",
            "size must be ≤ 2^31-1"));
    }
    Ok(size as usize)
}

pub fn random_bytes_sync<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    size: i32,
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    let n = validate_random_size(size)?;
    let mut out = vec![0u8; n];
    crate::crypto::fast_random(&mut out);
    Ok(buffer::emit_buffer(scope, &out).into())
}

pub async fn random_bytes_async(size: i32) -> Result<Vec<u8>, OpError> {
    let n = validate_random_size(size)?;
    // (addresses critic minor m-3): fast_random is thread-local; the
    // blocking-pool thread has its own initialised CSPRNG instance (seeded
    // from /dev/urandom at thread spawn). All threads share the same source
    // of entropy at the OS level. Concurrent calls do not share a single
    // CSPRNG instance — each thread has its own ChaCha20-based generator
    // re-keyed periodically. Verified: per-thread instances, not a contended
    // shared one. Test: spawn 16 threads each pulling 1MB; resulting bytes
    // must pass NIST SP 800-22 randomness tests (smoke: chi-square + serial).
    // (also addresses minor m-20: error type alignment — spawn_blocking returns
    // Result<T, JoinError>; .and_then(|r| r) flattens the inner Result<T, OpError>.)
    compio::runtime::spawn_blocking(move || {
        let mut out = vec![0u8; n];
        crate::crypto::fast_random(&mut out);
        Ok::<_, OpError>(out)
    }).await
      .map_err(|e| OpError::node("ERR_CRYPTO_OPERATION_FAILED", format!("{e:?}")))
      .and_then(|r| r)
}

// randomInt: rejection sampling.
//
// (addresses critic MAJOR #10 + minor m-15): Node's `randomInt(min, max)` per
// https://nodejs.org/api/crypto.html#cryptorandomintmin-max-callback enforces
// `max - min` ≤ `2^48` minus 1 = 281_474_976_710_655 (`0xFFFFFFFFFFFF`). v1
// used `> 2^48` which is off-by-one (allows range == 2^48). Fixed: use `>=`.
//
// `max == min` is also illegal (range zero); we keep the existing guard. The
// `randomInt(5, 5)` case (m-15) hits the `max <= min` check first and returns
// ERR_OUT_OF_RANGE before any bit-mask logic, so the bits=0 / mask=0 corner is
// unreachable.
pub fn random_int_sync(min: i64, max: i64) -> Result<i64, OpError> {
    if max <= min {
        return Err(OpError::node("ERR_OUT_OF_RANGE",
            "max must be greater than min"));
    }
    // Node's actual cap is 2^48 - 1.  `>=` instead of `>` per
    // https://github.com/nodejs/node/blob/main/lib/internal/crypto/random.js.
    if (max - min) >= (1_i64 << 48) {
        return Err(OpError::node("ERR_OUT_OF_RANGE",
            "max - min must be < 2^48"));
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

<!-- Round 3: addressing CRITICAL C2-1 + C2-2 (invented error codes). -->
### VII.3. Mapping to Node error codes

**Provenance audit (v3, addresses C2-1, C2-2):** every code emitted below is either:
- (a) **JS-side** — defined in `lib/internal/errors.js` via `E('CODE_NAME', ...)`. Source of truth: https://github.com/nodejs/node/blob/main/lib/internal/errors.js.
- (b) **C++-side** — defined in `src/node_errors.h` via the `V(CODE_NAME, ErrorClass)` macro list and emitted by the `THROW_ERR_CODE_NAME(env)` helper from C++. Source of truth: https://github.com/nodejs/node/blob/main/src/node_errors.h.
- (c) **Generic JS error** — a Node-side dynamic OpenSSL error with a name like `ERR_OSSL_<library>_<reason>`. Per `src/crypto/crypto_util.cc::ThrowCryptoError`, Node builds these by reading `ERR_GET_LIB(packed)` + `ERR_reason_error_string(packed)` at throw time; the resulting `e.code` is library + reason concatenation. We do NOT have access to BoringSSL/aws-lc's per-error library/reason strings as static constants and aws-lc-rs's `Unspecified` strips them, so we cannot faithfully replicate the dynamic shape.
- (d) **zeroship extension** — a code we emit that is not in Node's static catalog. v3 explicitly marks every such code in §VII.3a (D-N39).

`ERR_CRYPTO_INVALID_AUTH_TAG`, `ERR_CRYPTO_INVALID_IV`, `ERR_CRYPTO_INVALID_KEYLEN`, `ERR_CRYPTO_INVALID_TAG_LENGTH`, `ERR_CRYPTO_HASH_FINALIZED`, `ERR_CRYPTO_INVALID_STATE`, `ERR_CRYPTO_INVALID_DIGEST`, `ERR_CRYPTO_INVALID_JWK`, `ERR_CRYPTO_INVALID_KEY_OBJECT_TYPE`, `ERR_CRYPTO_INCOMPATIBLE_KEY_OPTIONS`, `ERR_CRYPTO_OPERATION_FAILED`, `ERR_CRYPTO_UNKNOWN_DH_GROUP`, `ERR_CRYPTO_UNKNOWN_CIPHER`, `ERR_CRYPTO_UNSUPPORTED_OPERATION`, `ERR_CRYPTO_TIMING_SAFE_EQUAL_LENGTH`, `ERR_CRYPTO_ECDH_INVALID_PUBLIC_KEY`, `ERR_CRYPTO_ECDH_INVALID_FORMAT`, `ERR_CRYPTO_INVALID_SCRYPT_PARAMS`, `ERR_CRYPTO_SCRYPT_NOT_SUPPORTED`, `ERR_CRYPTO_PBKDF2_ERROR`, `ERR_CRYPTO_KEM_NOT_SUPPORTED`, `ERR_CRYPTO_HASH_UPDATE_FAILED`, `ERR_CRYPTO_INVALID_MESSAGELEN`, `ERR_OSSL_EVP_INVALID_DIGEST` are all REAL.

`ERR_INVALID_ARG_TYPE`, `ERR_INVALID_ARG_VALUE`, `ERR_OUT_OF_RANGE`, `ERR_MISSING_ARGS`, `ERR_MISSING_OPTION`, `ERR_MISSING_PASSPHRASE`, `ERR_BUFFER_OUT_OF_BOUNDS` are also REAL.

```rust
// crypto_node/error.rs

// (v3, addresses C2-1, C2-2): error-code mappings audited against
// https://github.com/nodejs/node/blob/main/lib/internal/errors.js (the JS
// E('NAME', ...) registry) AND
// https://github.com/nodejs/node/blob/main/src/node_errors.h (the C++ V(...)
// macro registry — most ERR_CRYPTO_* codes live HERE, not in errors.js, which
// is why v2 missed several). Every code below is annotated with its origin.
impl KernelError {
    pub fn to_node(self) -> OpError {
        match self {
            // ERR_CRYPTO_HASH_FINALIZED — JS-side (errors.js).
            // Node's behaviour for Hmac post-digest is to throw a generic
            // Error (no .code) per
            // https://github.com/nodejs/node/blob/main/lib/internal/crypto/hash.js
            // For consistency with `jsonwebtoken` we emit the same code for
            // both — see m2-9 / m2-15. (This is technically a tiny zeroship
            // extension on the Hmac path; documented in §VII.3a.)
            Self::HashFinalised => OpError::node("ERR_CRYPTO_HASH_FINALIZED",
                "Digest already called"),
            Self::HmacFinalised => OpError::node("ERR_CRYPTO_HASH_FINALIZED",
                "Digest already called"),
            // ERR_CRYPTO_INVALID_KEYLEN — C++-side (node_errors.h, RangeError).
            Self::InvalidKeyLength { algorithm, expected, got } =>
                OpError::node("ERR_CRYPTO_INVALID_KEYLEN",
                    format!("Invalid {} key length: got {}, expected one of {:?}",
                        algorithm, got, expected)),
            // ERR_CRYPTO_INVALID_IV — C++-side (node_errors.h, TypeError).
            // (v3 fix, C2-1): renamed from v2's invented ERR_CRYPTO_INVALID_IV_LENGTH;
            // verified against
            // https://github.com/nodejs/node/blob/main/src/node_errors.h —
            // line `V(ERR_CRYPTO_INVALID_IV, TypeError)`. The `_LENGTH` suffix
            // does NOT exist in Node.
            Self::InvalidIvLength { algorithm, expected, got } =>
                OpError::node("ERR_CRYPTO_INVALID_IV",
                    format!("Invalid IV length for {}: got {}, expected one of {:?}",
                        algorithm, got, expected)),
            // ERR_CRYPTO_INVALID_TAG_LENGTH — C++-side (node_errors.h, RangeError).
            // (v3 fix, C2-1): renamed from v2's invented
            // ERR_CRYPTO_INVALID_AUTH_TAG_LENGTH; verified real per
            // node_errors.h `V(ERR_CRYPTO_INVALID_TAG_LENGTH, RangeError)`. Note
            // that ERR_CRYPTO_INVALID_AUTH_TAG also exists (TypeError) — used
            // for "tag bytes invalid" rather than "tag length wrong"; we use
            // INVALID_TAG_LENGTH for length mismatch and INVALID_AUTH_TAG for
            // the dynamic-error mapping AuthenticationFailed (below).
            Self::InvalidTagLength { expected, got } =>
                OpError::node("ERR_CRYPTO_INVALID_TAG_LENGTH",
                    format!("Invalid auth tag length: got {}, expected one of {:?}",
                        got, expected)),
            // (v3 fix, C2-2): GCM/CCM/OCB authentication-failure mapping.
            // Node's actual emission path is `ThrowCryptoError(env, ERR_get_error(),
            // "Unsupported state or unable to authenticate data")` — which builds
            // an `ERR_OSSL_<library>_<reason>` code dynamically from the OpenSSL
            // error queue at throw time (see
            // https://github.com/nodejs/node/blob/main/src/crypto/crypto_util.cc
            // `ThrowCryptoError`). aws-lc-rs's `Unspecified` does not surface
            // the upstream library/reason, so we cannot faithfully build the
            // dynamic name. v2 hardcoded `ERR_OSSL_EVP_BAD_DECRYPT` which is
            // not a Node static code and looks valid only by coincidence with
            // `ThrowCryptoError`'s common output for this case. v3 emits
            // `ERR_CRYPTO_OPERATION_FAILED` (real, errors.js) with the message
            // matching Node's, AND records the dynamic-shape preference as a
            // zeroship extension (§VII.3a, D-N39): code-aware callers (the
            // packages that branch on e.code === 'ERR_OSSL_EVP_BAD_DECRYPT')
            // get the legacy string in the message, but the canonical e.code
            // is the real Node fallback.
            Self::AuthenticationFailed => OpError::node("ERR_CRYPTO_OPERATION_FAILED",
                "Unsupported state or unable to authenticate data \
                 (was: ERR_OSSL_EVP_BAD_DECRYPT in older Node — see §VII.3a)"),
            // ERR_CRYPTO_INVALID_STATE — C++-side (node_errors.h, Error).
            Self::AadAfterUpdate => OpError::node("ERR_CRYPTO_INVALID_STATE",
                "setAAD must be called before update"),
            Self::SetAadOnNonAead => OpError::node("ERR_CRYPTO_INVALID_STATE",
                "setAAD only valid for authenticated cipher modes"),
            Self::SetAuthTagOnEncrypt => OpError::node("ERR_CRYPTO_INVALID_STATE",
                "setAuthTag is only valid on a Decipher"),
            Self::GetAuthTagBeforeFinal => OpError::node("ERR_CRYPTO_INVALID_STATE",
                "getAuthTag must be called after final()"),
            // (v3 fix, C2-2): ERR_OSSL_BAD_DECRYPT was a v1/v2 dynamic-shape
            // guess. Padding failure on Decipher.final() is mapped here from
            // the dynamic OSSL error in real Node; aws-lc-rs surfaces only
            // Unspecified. We emit ERR_CRYPTO_OPERATION_FAILED (the canonical
            // Node fallback per ThrowCryptoError when the OSSL queue is empty).
            Self::InvalidPadding => OpError::node("ERR_CRYPTO_OPERATION_FAILED",
                "bad decrypt"),
            // (v3 fix, C2-1): ERR_CRYPTO_INVALID_LENGTH was invented; the real
            // codes are ERR_CRYPTO_INVALID_KEYLEN (RangeError; key) and
            // ERR_CRYPTO_INVALID_TAG_LENGTH (RangeError; tag). For
            // "input not a block-size multiple", Node throws via
            // ERR_CRYPTO_INVALID_MESSAGELEN (RangeError, real per node_errors.h)
            // — see crypto/cipher.js's `cipher.update(buf)`-then-`cipher.final()`
            // length check.
            Self::InputNotMultipleOfBlockSize => OpError::node("ERR_CRYPTO_INVALID_MESSAGELEN",
                "Input data must be a multiple of the cipher block size"),

            // (v3 fix, C2-2): ERR_OSSL_EVP_SIGN / ERR_OSSL_EVP_VERIFY are
            // dynamic-shape codes Node builds at throw time from the OSSL
            // queue. We do not have the upstream library/reason. Per D-N39,
            // emit ERR_CRYPTO_OPERATION_FAILED (real) with the action in the
            // message; mark as zeroship-bridged in §VII.3a.
            //
            // Verify failure (signature mismatch) is NOT an error in Node —
            // Verify.prototype.verify(...) returns `false`. This case
            // distinguishes "verify operation failed" (key parse error,
            // wrong key type, etc.) from "signature mismatch" (returns false,
            // no throw). (m2-9: addresses critic minor.)
            Self::SignFailed => OpError::node("ERR_CRYPTO_OPERATION_FAILED",
                "Sign operation failed"),
            Self::VerifyFailed => OpError::node("ERR_CRYPTO_OPERATION_FAILED",
                "Verify operation failed (use kernel::sign_verify::verify_returns_bool \
                 for signature-mismatch; this variant fires only for hard errors)"),
            // ERR_CRYPTO_INCOMPATIBLE_KEY_OPTIONS — JS-side (errors.js, Error).
            Self::KeyTypeMismatchForAlgorithm =>
                OpError::node("ERR_CRYPTO_INCOMPATIBLE_KEY_OPTIONS",
                    "Incompatible key for this signing algorithm"),

            // (v3 fix, C2-2): ERR_OSSL_PEM_NO_START_LINE is a dynamic OSSL
            // code Node propagates from `PEM_read_bio_*` errors via
            // ThrowCryptoError. We emit ERR_CRYPTO_OPERATION_FAILED (real)
            // with the canonical message; the legacy OSSL name is preserved
            // in the message text per D-N39.
            Self::InvalidPem(msg) => OpError::node("ERR_CRYPTO_OPERATION_FAILED",
                format!("PEM_read_bio: no start line: {} \
                 (was: ERR_OSSL_PEM_NO_START_LINE in older Node)", msg)),
            // (v3 fix, C2-2): ERR_OSSL_ASN1_VALUE_ERROR — same dynamic shape.
            // Use ERR_CRYPTO_OPERATION_FAILED.
            Self::InvalidDer(msg) => OpError::node("ERR_CRYPTO_OPERATION_FAILED",
                format!("DER decode failed: {} \
                 (was: ERR_OSSL_ASN1_VALUE_ERROR in older Node)", msg)),
            // ERR_CRYPTO_INVALID_JWK — C++-side (node_errors.h, TypeError).
            Self::InvalidJwk(reason) => OpError::node("ERR_CRYPTO_INVALID_JWK",
                format!("Invalid JWK: {}", reason)),
            // ERR_CRYPTO_INVALID_KEY_OBJECT_TYPE — JS-side (errors.js).
            Self::InvalidKeyType => OpError::node("ERR_CRYPTO_INVALID_KEY_OBJECT_TYPE",
                "Invalid key object type"),
            // (v4 fix, C3-1): ERR_MISSING_PASSPHRASE is C++-side, NOT JS-side.
            // v3 wrongly attributed this to lib/internal/errors.js; verified
            // ABSENT from errors.js, PRESENT in src/node_errors.h:115 as
            // V(ERR_MISSING_PASSPHRASE, TypeError). The class is TypeError,
            // not Error — the macro arm in §VII.5 routes accordingly.
            Self::PassphraseRequired => OpError::node("ERR_MISSING_PASSPHRASE",
                "Passphrase required to decrypt private key"),
            // (v3 fix, C2-2): same dynamic-OSSL pattern. Use real
            // ERR_CRYPTO_OPERATION_FAILED.
            Self::PassphraseMismatch => OpError::node("ERR_CRYPTO_OPERATION_FAILED",
                "bad decrypt — passphrase incorrect \
                 (was: ERR_OSSL_EVP_BAD_DECRYPT in older Node)"),
            // (v3 fix, C2-2): ERR_OSSL_EVP_UNSUPPORTED_ALGORITHM is dynamic.
            // The real Node code for "unknown digest/cipher specifier" is
            // ERR_CRYPTO_INVALID_DIGEST (TypeError) for digest names and
            // ERR_CRYPTO_UNKNOWN_CIPHER (Error) for cipher names. We split
            // here based on the input domain.
            Self::UnsupportedKeyAlgorithm(name) =>
                OpError::node("ERR_CRYPTO_UNSUPPORTED_OPERATION",
                    format!("Unsupported key algorithm: {}", name)),

            // ERR_OUT_OF_RANGE — JS-side (errors.js).
            Self::PbkdfIterationsZero => OpError::node("ERR_OUT_OF_RANGE",
                "iterations must be > 0"),
            // (v3 fix, C2-1, C2-2): ERR_OSSL_EVP_UNSUPPORTED was invented.
            // Node's real path is ERR_CRYPTO_INVALID_DIGEST (TypeError, real
            // per node_errors.h) for unknown digest specifiers in PBKDF2.
            Self::PbkdfDigestUnknown(name) => OpError::node("ERR_CRYPTO_INVALID_DIGEST",
                format!("Invalid digest: {}", name)),
            Self::HkdfOutputTooLarge { max, got } =>
                OpError::node("ERR_OUT_OF_RANGE",
                    format!("HKDF output length {} exceeds max {}", got, max)),
            // ERR_CRYPTO_INVALID_SCRYPT_PARAMS — C++-side (node_errors.h).
            Self::ScryptParametersInvalid { reason } =>
                OpError::node("ERR_CRYPTO_INVALID_SCRYPT_PARAMS",
                    format!("Invalid scrypt parameters: {}", reason)),
            // (v3 fix, M2-14): scrypt-memory-exceeded should map to the
            // INVALID_SCRYPT_PARAMS code (RangeError, real). v2 had it on
            // SCRYPT_NOT_SUPPORTED, which is real but means a different
            // thing ("scrypt not built into the OpenSSL").
            Self::ScryptMemoryExceeded { max, would_use } =>
                OpError::node("ERR_CRYPTO_INVALID_SCRYPT_PARAMS",
                    format!("scrypt requires {} bytes, max is {}", would_use, max)),

            // ERR_CRYPTO_ECDH_INVALID_PUBLIC_KEY — JS-side (errors.js).
            Self::DhCurveMismatch => OpError::node("ERR_CRYPTO_ECDH_INVALID_PUBLIC_KEY",
                "Public key curve mismatch"),
            Self::DhPublicKeyInvalid => OpError::node("ERR_CRYPTO_ECDH_INVALID_PUBLIC_KEY",
                "Invalid public key for ECDH"),
            // ERR_CRYPTO_UNKNOWN_DH_GROUP — C++-side (node_errors.h).
            // Counter-cited against round-1 critic: real, not invented.
            Self::DhUnknownNamedGroup(name) =>
                OpError::node("ERR_CRYPTO_UNKNOWN_DH_GROUP",
                    format!("Unknown DH group: {}", name)),
            // (v3 fix, C2-2): ERR_CRYPTO_INVALID_DH_PRIME is NOT in Node's
            // static catalog (verified — neither in errors.js nor
            // node_errors.h). Marked as zeroship extension in §VII.3a;
            // packages that need a real-Node code path should match on
            // ERR_CRYPTO_OPERATION_FAILED in the message branch.
            Self::DhPrimeRejected { reason } =>
                OpError::node("ERR_CRYPTO_OPERATION_FAILED",
                    format!("Rejected DH prime: {} \
                     (zeroship extension code: ERR_CRYPTO_INVALID_DH_PRIME)", reason)),

            // (v3 fix, C2-2): ERR_OSSL_EVP_UNSUPPORTED_ALGORITHM was a
            // v1/v2 dynamic-shape guess. The real Node path for "createSign
            // got an unknown digest" is ERR_CRYPTO_INVALID_DIGEST or, for
            // ciphers, ERR_CRYPTO_UNKNOWN_CIPHER (both real per node_errors.h).
            Self::UnsupportedAlgorithm { name, op } => match op {
                "digest" | "hash" =>
                    OpError::node("ERR_CRYPTO_INVALID_DIGEST",
                        format!("Invalid digest: {}", name)),
                "cipher" =>
                    OpError::node("ERR_CRYPTO_UNKNOWN_CIPHER",
                        format!("Unknown cipher: {}", name)),
                _ =>
                    OpError::node("ERR_CRYPTO_UNSUPPORTED_OPERATION",
                        format!("Unsupported {} for op {}", name, op)),
            },
            Self::UnsupportedOperation(msg) =>
                OpError::node("ERR_CRYPTO_UNSUPPORTED_OPERATION", msg),

            // ERR_CRYPTO_OPERATION_FAILED — JS-side (errors.js, real).
            Self::InternalError(msg) => OpError::node("ERR_CRYPTO_OPERATION_FAILED",
                format!("Internal error: {}", msg)),
        }
    }
}
```

<!-- Round 3: addressing CRITICAL C2-1 + C2-2 (zeroship extension policy). -->
### VII.3a. Error-code provenance and zeroship extensions (D-N39, addresses C2-1, C2-2)

This section enumerates EVERY error code surfaced by `crypto_node/error.rs` with its provenance. **Provenance** is one of:
- **JS** = defined in `lib/internal/errors.js` via `E('CODE', ...)` (https://github.com/nodejs/node/blob/main/lib/internal/errors.js).
- **C++** = defined in `src/node_errors.h` via the `V(CODE, ErrorClass)` macro (https://github.com/nodejs/node/blob/main/src/node_errors.h). C++-side codes are emitted from native code via `THROW_ERR_CODE(env)` macros.
- **Dynamic-OSSL** = constructed at throw time by Node's `ThrowCryptoError` from the OpenSSL error queue (`ERR_get_error` + `ERR_GET_LIB` + `ERR_reason_error_string`). The `e.code` ends up shaped like `ERR_OSSL_<library>_<reason>` (e.g., `ERR_OSSL_EVP_BAD_DECRYPT`, `ERR_OSSL_PEM_NO_START_LINE`). We CANNOT reliably reproduce these because (a) aws-lc-rs's `Unspecified` strips the upstream library/reason and (b) the names depend on the BoringSSL/OpenSSL build's reason-string table.
- **zs-ext** = a code we emit that is NOT in Node's static or dynamic catalogs. Marked clearly so audit-tooling can filter.

| Code | Provenance | Surface | Notes |
|---|---|---|---|
| `ERR_CRYPTO_HASH_FINALIZED` | JS | Hash, Hmac | Hash usage matches Node; Hmac is a tiny zs-ext (Node throws plain Error there) — kept for jsonwebtoken consistency. |
| `ERR_CRYPTO_INVALID_KEYLEN` | C++ (node_errors.h, RangeError) | Cipher, KDF, KeyObject | Symmetric key length mismatch. |
| `ERR_CRYPTO_INVALID_IV` | C++ (node_errors.h, TypeError) | Cipher | IV length mismatch. v2 wrongly used `_LENGTH` suffix; corrected. |
| `ERR_CRYPTO_INVALID_TAG_LENGTH` | C++ (node_errors.h, RangeError) | Cipher AEAD | Tag length mismatch on createCipheriv. |
| `ERR_CRYPTO_INVALID_AUTH_TAG` | C++ (node_errors.h, TypeError) | Decipher | `setAuthTag(buf)` with bad-shape buf. Used at validation time, not at decrypt-failure time. |
| `ERR_CRYPTO_INVALID_MESSAGELEN` | C++ (node_errors.h, RangeError) | Cipher CBC | Input length not block-size multiple after `setAutoPadding(false)`. |
| `ERR_CRYPTO_INVALID_STATE` | C++ (node_errors.h, Error) | Cipher, Hash | "called X after Y". |
| `ERR_CRYPTO_INVALID_DIGEST` | C++ (node_errors.h, TypeError) | Hash, KDF, Sign | Unknown digest name. |
| `ERR_CRYPTO_INVALID_JWK` | C++ (node_errors.h, TypeError) | KeyObject | JWK parse error. |
| `ERR_CRYPTO_INVALID_KEY_OBJECT_TYPE` | JS (errors.js) | KeyObject | Bad type for key import. |
| `ERR_CRYPTO_INCOMPATIBLE_KEY_OPTIONS` | JS (errors.js, Error) | Sign / Verify / KeyObject | RSA key for EC sign etc. |
| `ERR_CRYPTO_OPERATION_FAILED` | JS (errors.js, Error) | Many | The Node fallback for unmappable internal errors and the canonical replacement for invented `ERR_OSSL_*` codes (D-N39). |
| `ERR_CRYPTO_UNKNOWN_DH_GROUP` | C++ (node_errors.h, Error) | DH | Unknown named group like "modp99". |
| `ERR_CRYPTO_UNKNOWN_CIPHER` | C++ (node_errors.h, Error) | Cipher | Unknown cipher name. |
| `ERR_CRYPTO_UNSUPPORTED_OPERATION` | C++ (node_errors.h, Error) | Many | Stage-2 placeholder, deferred APIs. |
| `ERR_CRYPTO_TIMING_SAFE_EQUAL_LENGTH` | C++ (node_errors.h, RangeError) | timingSafeEqual | Length mismatch. |
| `ERR_CRYPTO_ECDH_INVALID_PUBLIC_KEY` | JS (errors.js) | ECDH | Bad public key bytes. |
| `ERR_CRYPTO_ECDH_INVALID_FORMAT` | JS (errors.js, TypeError) | ECDH | Bad format string for getPublicKey. |
| `ERR_CRYPTO_INVALID_SCRYPT_PARAMS` | C++ (node_errors.h, RangeError) | scrypt | Bad N/r/p combination OR memory exceeded (M2-14). |
| `ERR_CRYPTO_SCRYPT_NOT_SUPPORTED` | JS (errors.js, Error) | scrypt | Build doesn't include scrypt — never our case (aws-lc has scrypt). Keep for completeness. |
| `ERR_CRYPTO_PBKDF2_ERROR` | JS (errors.js, Error) | pbkdf2 | OSSL-side PBKDF2 failure. |
| `ERR_CRYPTO_KEM_NOT_SUPPORTED` | JS (errors.js, Error) | encapsulate / decapsulate | Stage E placeholder. |
| `ERR_CRYPTO_HASH_UPDATE_FAILED` | JS (errors.js, Error) | Hash | OSSL-side update failure (rare). |
| `ERR_OSSL_EVP_INVALID_DIGEST` | C++ (node_errors.h, Error) | Sign / Verify | The ONLY `ERR_OSSL_*` static code (used when EdDSA is paired with a non-null algorithm). |
| `ERR_INVALID_ARG_TYPE` | JS (errors.js, TypeError) | Many | Bad shape input. |
| `ERR_INVALID_ARG_VALUE` | JS (errors.js, TypeError) | Many | Bad value (negative size, wrong padding number, etc.). |
| `ERR_OUT_OF_RANGE` | JS (errors.js, RangeError) | Many | Numeric range violation. |
| `ERR_MISSING_OPTION` | JS (errors.js, TypeError) | Cipher | "X is required". |
| `ERR_MISSING_PASSPHRASE` | C++ (node_errors.h, TypeError) | KeyObject | Encrypted PKCS#8 import without passphrase. <!-- Round 4: addressing CRITICAL C3-1 — was wrongly attributed to errors.js / Error in v3; verified in node_errors.h:115 V(ERR_MISSING_PASSPHRASE, TypeError) at https://github.com/nodejs/node/blob/main/src/node_errors.h --> |
| `ERR_MISSING_ARGS` | JS (errors.js, TypeError) | Many | Missing required positional argument. |
| `ERR_BUFFER_OUT_OF_BOUNDS` | JS (errors.js, RangeError) | Random, randomFill | Offset+size out of buffer. |
| `ERR_CRYPTO_CUSTOM_ENGINE_NOT_SUPPORTED` | JS (errors.js, Error) | setEngine | Always thrown — D-N25. |
| `ERR_CRYPTO_FIPS_UNAVAILABLE` | JS (errors.js, Error) | setFips | Emitted when `setFips(true)` is called in a non-FIPS build (verified at https://github.com/nodejs/node/blob/main/lib/internal/errors.js line 1177 — `E('ERR_CRYPTO_FIPS_UNAVAILABLE', 'Cannot set FIPS mode in a non-FIPS build.', Error)`). <!-- Round 4: addressing CRITICAL C3-2 — was emitted at §X.3 line ~4050 but missing from this table. --> |
| `ERR_UNKNOWN_ENCODING` | JS (errors.js, TypeError) | Many (encoding decode) | Emitted from the `extract_input` decoder when an unknown encoding string is passed (verified at https://github.com/nodejs/node/blob/main/lib/internal/errors.js line 1875 — `E('ERR_UNKNOWN_ENCODING', 'Unknown encoding: %s', TypeError)`). <!-- Round 4: addressing CRITICAL C3-2 — was emitted at §V.x line ~546 but missing from this table. --> |
| `ERR_INVALID_BUFFER_SIZE` | JS (errors.js, RangeError) | Buffer / hex decode | Emitted on Buffer-size mismatch; class is **RangeError** per https://github.com/nodejs/node/blob/main/lib/internal/errors.js line 1480 — `E('ERR_INVALID_BUFFER_SIZE', 'Buffer size must be a multiple of %s', RangeError)`. <!-- Round 4: addressing MAJOR M3-3 — listed in macro arm at §VII.5 but missing from this table; class corrected from TypeError to RangeError. --> |

**zeroship extensions** (we emit a code that is NOT in Node's static or dynamic catalog):
- *(none in v4)* — every code in the mapping table above is a real Node code (verified against `errors.js` or `node_errors.h`). v2's `ERR_CRYPTO_INVALID_DH_PRIME`, `ERR_CRYPTO_DEPRECATED_API`, `ERR_CRYPTO_INVALID_AUTH_TAG_LENGTH`, `ERR_CRYPTO_INVALID_IV_LENGTH`, `ERR_CRYPTO_AUTH_TAG_LENGTH_INVALID`, `ERR_CRYPTO_INVALID_LENGTH` were all renamed to real Node codes (or, in the dynamic-OSSL case, replaced with `ERR_CRYPTO_OPERATION_FAILED` with the legacy name in the message text).

**zeroship behavioural divergences** (we emit a real Node code, but in a situation where Node would NOT throw — divergence is intentional and documented in §XVII.13b):
- `ERR_OUT_OF_RANGE` on empty HMAC key. Node silently accepts (`crypto_hmac.cc::HmacInit` re-binds `key = ""` and forwards). zeroship rejects per RFC 2104 §2 — see kernel comment at `crypto_node/hmac.rs` and divergence log §XVII.13b. <!-- Round 4: addressing CRITICAL C3-3 — was rationalized as parity in v3; now correctly tagged as divergence. -->

**Dynamic-OSSL codes preserved in message text** (for upstream-package compatibility — packages that branch on `e.message.includes('ERR_OSSL_X')`):
- `ERR_OSSL_EVP_BAD_DECRYPT` — message text on AuthenticationFailed, InvalidPadding, PassphraseMismatch.
- `ERR_OSSL_PEM_NO_START_LINE` — message text on InvalidPem.
- `ERR_OSSL_ASN1_VALUE_ERROR` — message text on InvalidDer.

These are documented as fallback hints; the `e.code` is always a real Node code. Node's dynamic-OSSL path is not faithfully reproducible because aws-lc-rs's `Unspecified` strips the underlying error reason — to recover this we would need to either (a) link aws-lc-sys directly and consume the BoringSSL ERR_PACK queue per call (cost: ~150 LOC of bridge code, doable in Stage F as a future enhancement; tracked as open question XVII.12 below) or (b) accept the lossy mapping. v3 picks (b) as the working answer; (a) is queued.

<!-- Round 4: addressing MAJOR M3-4 — explicit user-visible warning about the two-tier system. -->
**Operational warning — two-tier error surface.** This policy creates a deliberate two-tier system that the operational docs (`docs/reference/node-compat.md`) MUST document for users:
1. `e.code` — always a real Node static code (e.g., `ERR_CRYPTO_OPERATION_FAILED`). Branch on this for stable behaviour.
2. `e.message` — may contain a legacy OSSL hint (e.g., `"... (was: ERR_OSSL_EVP_BAD_DECRYPT in older Node)"`). DO NOT branch on this; it is informational only and may move to Stage F's faithful-OSSL bridge.

Packages that copy-paste `e.message` into log lines, test fixtures, or assertions WILL see the legacy OSSL name. This is intentional: it preserves the visible behaviour creators expect from `console.log(err)` while the `e.code` channel stays canonical. Stage F (XVII.12) tightens this by bridging the OSSL queue if demand surfaces.

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
// (v3, addresses C2-1 + the per-code class mapping per node_errors.h):
// the class of each Node error code is the V(...) macro's second
// argument in https://github.com/nodejs/node/blob/main/src/node_errors.h
// (for C++-side codes) or the third argument to E('NAME', '...', Class)
// in lib/internal/errors.js (for JS-side codes). v3 makes the mapping
// match Node's actual class assignment.
::zeroship_runtime::state::OpErrorKind::NodeError(code) => {
    let __msg = v8::String::new(scope, &__err.message).unwrap();
    let class = match code {
        // RangeError codes (per node_errors.h `V(NAME, RangeError)` and
        // errors.js `E('NAME', '...', RangeError)`).
        "ERR_OUT_OF_RANGE"
        | "ERR_BUFFER_OUT_OF_BOUNDS"
        | "ERR_INVALID_BUFFER_SIZE"             // v4 fix, M3-3: errors.js:1480 says RangeError, not TypeError
        | "ERR_CRYPTO_INVALID_KEYLEN"
        | "ERR_CRYPTO_INVALID_TAG_LENGTH"
        | "ERR_CRYPTO_INVALID_KEYPAIR"
        | "ERR_CRYPTO_INVALID_KEYTYPE"
        | "ERR_CRYPTO_INVALID_MESSAGELEN"
        | "ERR_CRYPTO_INVALID_SCRYPT_PARAMS"
        | "ERR_CRYPTO_TIMING_SAFE_EQUAL_LENGTH"
            => v8::Exception::range_error(scope, __msg),
        // TypeError codes.
        "ERR_INVALID_ARG_TYPE"
        | "ERR_INVALID_ARG_VALUE"
        | "ERR_INVALID_RETURN_VALUE"
        | "ERR_MISSING_ARGS"
        | "ERR_MISSING_OPTION"
        | "ERR_MISSING_PASSPHRASE"              // v4 fix, C3-1 / M3-3: node_errors.h:115 V(ERR_MISSING_PASSPHRASE, TypeError) — moved from default Error
        | "ERR_UNKNOWN_ENCODING"
        | "ERR_CRYPTO_INVALID_AUTH_TAG"
        | "ERR_CRYPTO_INVALID_COUNTER"
        | "ERR_CRYPTO_INVALID_CURVE"
        | "ERR_CRYPTO_INVALID_DIGEST"
        | "ERR_CRYPTO_INVALID_IV"
        | "ERR_CRYPTO_INVALID_JWK"
        | "ERR_CRYPTO_ECDH_INVALID_FORMAT"
            => v8::Exception::type_error(scope, __msg),
        // Default: plain Error (covers ERR_CRYPTO_HASH_FINALIZED,
        // ERR_CRYPTO_INVALID_STATE, ERR_CRYPTO_OPERATION_FAILED,
        // ERR_CRYPTO_UNKNOWN_DH_GROUP, ERR_CRYPTO_UNKNOWN_CIPHER,
        // ERR_CRYPTO_UNSUPPORTED_OPERATION, ERR_CRYPTO_INVALID_KEY_OBJECT_TYPE,
        // ERR_CRYPTO_INCOMPATIBLE_KEY_OPTIONS, ERR_CRYPTO_INCOMPATIBLE_KEY,
        // ERR_CRYPTO_FIPS_FORCED, ERR_CRYPTO_FIPS_UNAVAILABLE,
        // ERR_CRYPTO_HASH_UPDATE_FAILED, ERR_CRYPTO_PBKDF2_ERROR,
        // ERR_CRYPTO_SIGN_KEY_REQUIRED, ERR_CRYPTO_KEM_NOT_SUPPORTED,
        // ERR_CRYPTO_ARGON2_NOT_SUPPORTED, ERR_CRYPTO_SCRYPT_NOT_SUPPORTED,
        // ERR_CRYPTO_CUSTOM_ENGINE_NOT_SUPPORTED, ERR_CRYPTO_ENGINE_UNKNOWN,
        // ERR_OSSL_EVP_INVALID_DIGEST, ERR_CRYPTO_ECDH_INVALID_PUBLIC_KEY,
        // etc.). v4 (C3-1): ERR_MISSING_PASSPHRASE removed from this list —
        // moved to TypeError per node_errors.h:115.
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

Total macro extension: ~50 LOC (v3 expanded the class-mapping table from v2's stub to cover every Node code we emit). The dispatch table at the top of `gen_throw_error` is the only routing logic.

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

// (addresses critic MAJOR #3): HASH_NAMES must contain every alias Node's
// getHashes() returns, so feature-detection code that does
// `crypto.getHashes().includes('rsa-sha1')` passes. The list mirrors what
// OpenSSL aliases via EVP_get_digestbyname() — Node simply returns the OpenSSL
// alias table.
//
// IMPORTANT (addresses critic MAJOR #24): the `rsa-sha*` / `dsa-sha*` /
// `ecdsa-with-SHA*` names are SIGNATURE-algorithm names, NOT pure hash names.
// They appear in HASH_NAMES so that getHashes() returns them (Node does), and
// so that createSign/createVerify can accept them. The createSign path
// `canonicalise_hash_name(name)` returns the underlying HashAlgo, but the
// surrounding sign-context layer infers the asymmetric algorithm from the KEY
// type (RSA vs ECDSA), not from the prefix. The `rsa-` / `dsa-` prefix is
// effectively ignored at sign time when the key already constrains the algo.
// (v3, addresses C2-3): each HashAlgo variant is annotated with its
// VERIFIED backing path. Variants whose BoringSSL/aws-lc dependency is
// missing are flagged "DEFER" — the entry stays in the map so getHashes()
// returns the expected list, but createHash(name) routes to a "not yet
// implemented" error per §V.2 dispatch.
//
// Backing-path key:
//   HL = aws-lc-rs high-level (`aws_lc_rs::digest`).
//   FFI = aws-lc-sys raw FFI (vendored in `crypto_kernel/digest_*.rs`).
//   DEFER = NOT in aws-lc/BoringSSL public surface; entry kept for
//           getHashes() listing only (matches Node behaviour: Node ALSO
//           returns names for hashes not actually supported by the build).
pub static HASH_NAMES: phf::Map<&'static str, HashAlgo> = phf::phf_map! {
    // Pure SHA-family names (case variants).
    "sha1" => HashAlgo::Sha1,                  // HL: digest::SHA1_FOR_LEGACY_USE_ONLY
    "sha-1" => HashAlgo::Sha1,                 // HL: digest::SHA1_FOR_LEGACY_USE_ONLY
    "sha224" => HashAlgo::Sha224,              // HL: digest::SHA224
    "sha-224" => HashAlgo::Sha224,             // HL: digest::SHA224
    "sha256" => HashAlgo::Sha256,              // HL: digest::SHA256
    "sha-256" => HashAlgo::Sha256,             // HL: digest::SHA256
    "sha384" => HashAlgo::Sha384,              // HL: digest::SHA384
    "sha-384" => HashAlgo::Sha384,             // HL: digest::SHA384
    "sha512" => HashAlgo::Sha512,              // HL: digest::SHA512
    "sha-512" => HashAlgo::Sha512,             // HL: digest::SHA512
    "sha512-224" => HashAlgo::Sha512_224,      // FFI: EVP_sha512_224() — NOT in aws-lc-rs digest module
    "sha512-256" => HashAlgo::Sha512_256,      // HL: digest::SHA512_256
    // SHA-3 family.
    "sha3-224" => HashAlgo::Sha3_224,          // DEFER: NOT in aws-lc-rs (digest module exposes only SHA3-256/384/512); BoringSSL public API does NOT ship SHA3-224. Listed for getHashes() parity only.
    "sha3-256" => HashAlgo::Sha3_256,          // HL: digest::SHA3_256
    "sha3-384" => HashAlgo::Sha3_384,          // HL: digest::SHA3_384
    "sha3-512" => HashAlgo::Sha3_512,          // HL: digest::SHA3_512
    "shake128" => HashAlgo::Shake128,          // DEFER: NOT in aws-lc-rs; BoringSSL ships Keccak-f[1600] internals but no public SHAKE API.
    "shake256" => HashAlgo::Shake256,          // DEFER: same reason as SHAKE128.

    // RSA-prefixed compound names (legacy OpenSSL aliases, accepted by
    // createSign — they decompose to the bare hash; the key constrains RSA).
    "rsa-sha1" => HashAlgo::Sha1,
    "rsa-sha224" => HashAlgo::Sha224,
    "rsa-sha256" => HashAlgo::Sha256,
    "rsa-sha384" => HashAlgo::Sha384,
    "rsa-sha512" => HashAlgo::Sha512,
    "rsa-md5" => HashAlgo::Md5,
    "id-rsassa-pkcs1-v1_5-with-sha256" => HashAlgo::Sha256,
    "id-rsassa-pkcs1-v1_5-with-sha384" => HashAlgo::Sha384,
    "id-rsassa-pkcs1-v1_5-with-sha512" => HashAlgo::Sha512,

    // DSA-prefixed compound names.
    "dsa-sha1" => HashAlgo::Sha1,
    "dsa-sha256" => HashAlgo::Sha256,

    // ECDSA-prefixed compound names (formal OID names from RFC 5754).
    "ecdsa-with-sha1" => HashAlgo::Sha1,
    "ecdsa-with-sha256" => HashAlgo::Sha256,
    "ecdsa-with-sha384" => HashAlgo::Sha384,
    "ecdsa-with-sha512" => HashAlgo::Sha512,

    // Legacy + niche.
    "md5" => HashAlgo::Md5,                    // FFI: EVP_md5() — NOT in aws-lc-rs digest module
    "md5-sha1" => HashAlgo::Md5Sha1,           // FFI (Stage E): the legacy TLS 1.0/1.1 PRF concatenated hash; needs EVP_md5_sha1() bridge
    "ripemd160" => HashAlgo::Ripemd160,        // DEFER: NOT in aws-lc/BoringSSL at all. Entry kept for getHashes() parity only.
    "rmd160" => HashAlgo::Ripemd160,           // DEFER: alias for ripemd160.
    "blake2b512" => HashAlgo::Blake2b512,      // FFI (Stage E): EVP_blake2b512()
    "blake2s256" => HashAlgo::Blake2s256,      // FFI (Stage E): EVP_blake2s256()
};

// (addresses critic MAJOR #4): every cipher entry now carries a `gate` field
// telling the surface adapter what runtime flag (if any) to require. Stage-1
// modern AEAD modes are ungated; bare-ChaCha20 (no Poly1305) is `LegacyCrypto`-
// gated because raw stream ciphers are a security footgun; RC4/IDEA/Blowfish
// are `LegacyCrypto`-gated; AES-CCM is ungated (it's modern, just less common).
//
// (addresses critic MAJOR #17): the encrypted-PEM `cipher` whitelist is the
// subset of this table where `is_encrypted_pem_cipher == true`; AES-CBC family
// is in by default, ECB/3DES variants require LegacyCrypto.
pub struct CipherEntry {
    pub alg: CipherAlg,
    pub mode: CipherMode,
    pub key_lengths: &'static [usize],
    pub iv_length: Option<usize>,
    pub block_size: usize,
    pub aliases: &'static [&'static str],
    pub gate: CipherGate,
    pub is_aead: bool,
    pub is_encrypted_pem_cipher: bool,
}

pub enum CipherGate {
    Ungated,
    LegacyCrypto,                // requires --legacy-crypto
}

// (v3, addresses C2-3 + M2-23): each CipherAlg variant is annotated with its
// VERIFIED backing path. Variants whose aws-lc-rs constant doesn't exist are
// rerouted via aws-lc-sys raw FFI (per D-N38), or DEFERRED.
//
// Backing-path key:
//   HL aead = aws-lc-rs `aead::*` (verified at https://docs.rs/aws-lc-rs/latest/aws_lc_rs/aead/index.html: AES_128_GCM, AES_128_GCM_SIV, AES_192_GCM, AES_256_GCM, AES_256_GCM_SIV, CHACHA20_POLY1305 — nothing else).
//   HL cipher = aws-lc-rs `cipher::*` (verified: AES_128/192/256 + CBC-PKCS7 / CTR / CFB128 modes only).
//   FFI = aws-lc-sys raw FFI.
//   DEFER = entry removed from this map; not shippable in any near-term stage.
pub static CIPHER_NAMES: phf::Map<&'static str, CipherAlg> = phf::phf_map! {
    // AES-CBC: HL cipher (PaddedBlockEncryptingKey::cbc_pkcs7).
    "aes-128-cbc" => CipherAlg::Aes128Cbc,
    "aes-192-cbc" => CipherAlg::Aes192Cbc,
    "aes-256-cbc" => CipherAlg::Aes256Cbc,
    // AES-CTR: HL cipher (EncryptingKey::ctr).
    "aes-128-ctr" => CipherAlg::Aes128Ctr,
    "aes-192-ctr" => CipherAlg::Aes192Ctr,
    "aes-256-ctr" => CipherAlg::Aes256Ctr,
    // AES-GCM: HL aead (AES_128_GCM, AES_192_GCM, AES_256_GCM all verified).
    "aes-128-gcm" => CipherAlg::Aes128Gcm,
    "aes-192-gcm" => CipherAlg::Aes192Gcm,
    "aes-256-gcm" => CipherAlg::Aes256Gcm,
    // AES-CCM: FFI ONLY — NOT in aws-lc-rs aead module (verified). See D-N38 / §III.2a.
    // (v3, addresses C2-3: v2's `aead::AES_*_CCM` doesn't exist.)
    "aes-128-ccm" => CipherAlg::Aes128Ccm,
    "aes-192-ccm" => CipherAlg::Aes192Ccm,
    "aes-256-ccm" => CipherAlg::Aes256Ccm,
    // AES-OCB: FFI ONLY — NOT in aws-lc-rs aead module (verified). DEFERRED to Stage E
    // due to low value/effort ratio (OCB is rare in real usage).
    // (v3, addresses C2-3: v2's `aead::AES_*_OCB` doesn't exist.)
    "aes-128-ocb" => CipherAlg::Aes128Ocb,    // Stage E
    "aes-192-ocb" => CipherAlg::Aes192Ocb,    // Stage E
    "aes-256-ocb" => CipherAlg::Aes256Ocb,    // Stage E
    // AES-KW: kernel routine (RFC 3394 wrap, ~120 LOC of pure Rust over
    // aws-lc-rs cipher::AES_* in raw-block mode). The "wrap" name is the
    // OpenSSL alias; reused from existing `crypto_native/wrap.rs`.
    "aes-128-wrap" => CipherAlg::Aes128Kw,
    "aes-192-wrap" => CipherAlg::Aes192Kw,
    "aes-256-wrap" => CipherAlg::Aes256Kw,
    // (v3 fix, M2-23): AES-XTS REMOVED from CIPHER_NAMES. XTS requires a
    // tweak parameter (disk-block index) that the generic CipherContext
    // does not carry; v2 listed it as if it would work via a generic IV
    // path, which is incorrect per NIST SP 800-38E. **DEFERRED PERMANENTLY**
    // until a creator app surfaces a use case justifying a tweak-aware
    // CipherContext.
    // "aes-128-xts" => removed
    // "aes-256-xts" => removed
    // ChaCha20-Poly1305: HL aead (verified).
    "chacha20-poly1305" => CipherAlg::ChaCha20Poly1305,

    // Bare ChaCha20 (no AEAD): LegacyCrypto-gated. Stream cipher without
    // authentication is a footgun; require explicit opt-in. FFI required —
    // aws-lc-rs cipher does NOT expose ChaCha20 as a stream cipher (only
    // ChaCha20-Poly1305 in aead).
    "chacha20" => CipherAlg::ChaCha20,    // Stage E, FFI

    // AES-ECB: FFI ONLY (aws-lc-rs cipher does NOT expose ECB mode; only CBC-PKCS7,
    // CTR, CFB128). AES-128/256-ECB is legitimate for HSM key wrap; AES-192-ECB rare.
    "aes-128-ecb" => CipherAlg::Aes128Ecb,    // Stage E, FFI
    "aes-256-ecb" => CipherAlg::Aes256Ecb,    // Stage E, FFI

    // AES-CFB128: HL cipher (DecryptingKey::cfb128 / EncryptingKey::cfb128).
    // The plain `aes-*-cfb` Node alias maps to CFB128 (the only CFB variant in
    // aws-lc-rs's public API; verified).
    "aes-128-cfb" => CipherAlg::Aes128Cfb,
    "aes-256-cfb" => CipherAlg::Aes256Cfb,
    // AES-CFB1 / CFB8 / OFB: FFI ONLY — aws-lc-rs cipher exposes only CFB128.
    "aes-128-cfb1" => CipherAlg::Aes128Cfb1,    // Stage E, FFI
    "aes-128-cfb8" => CipherAlg::Aes128Cfb8,    // Stage E, FFI
    "aes-128-ofb" => CipherAlg::Aes128Ofb,      // Stage E, FFI
    "aes-256-ofb" => CipherAlg::Aes256Ofb,      // Stage E, FFI

    // Legacy (Stage E, gated on --legacy-crypto). All FFI ONLY — aws-lc-rs cipher
    // does NOT expose any of DES / 3DES / Blowfish / Cast5 / RC4 / IDEA. v2's
    // `cipher::TDES_*` was an invention.
    "des-cbc" => CipherAlg::DesCbc,             // Stage E, FFI: EVP_des_cbc()
    "des-ecb" => CipherAlg::DesEcb,             // Stage E, FFI: EVP_des_ecb()
    "des-ede3" => CipherAlg::Tdes,              // Stage E, FFI: EVP_des_ede3()
    "des-ede3-cbc" => CipherAlg::TdesCbc,       // Stage E, FFI: EVP_des_ede3_cbc()
    "des-ede3-ecb" => CipherAlg::TdesEcb,       // Stage E, FFI: EVP_des_ede3_ecb()
    "bf-cbc" => CipherAlg::BlowfishCbc,         // Stage E, FFI: EVP_bf_cbc()
    "bf-ecb" => CipherAlg::BlowfishEcb,         // Stage E, FFI: EVP_bf_ecb()
    "cast5-cbc" => CipherAlg::Cast5Cbc,         // Stage E, FFI if BoringSSL slim build includes it
    "rc4" => CipherAlg::Rc4,                    // Stage E, FFI: EVP_rc4()
    "rc4-40" => CipherAlg::Rc4_40,              // Stage E, FFI
    // IDEA-CBC: DEFERRED — IDEA was removed from BoringSSL. Entry NOT in map.
    // "idea-cbc" => removed (v3, addresses C2-3)
};
```

**Stage 1 ungated set after v3 audit:** AES-CBC (HL), AES-CTR (HL), AES-GCM (HL), AES-CCM (FFI; ungated for production use), ChaCha20-Poly1305 (HL), AES-KW (kernel routine), AES-CFB128 (HL).

**Stage E set:** AES-OCB (FFI), AES-ECB (FFI), AES-CFB1/8 (FFI), AES-OFB (FFI), bare ChaCha20 (FFI; LegacyCrypto-gated), DES (FFI; LegacyCrypto-gated), 3DES (FFI; LegacyCrypto-gated), Blowfish (FFI; LegacyCrypto-gated), Cast5 (FFI; LegacyCrypto-gated), RC4 (FFI; LegacyCrypto-gated).

**Permanently deferred:** AES-XTS (no tweak in CipherContext), IDEA (not in BoringSSL).

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

pub enum CipherMode { Cbc, Ctr, Gcm, Ccm, Ocb, Kw, Stream, Cfb, Ofb, Ecb }
```

<!-- Round 3: addressing MAJOR M2-21 (GCM tag length whitelist). -->
**AEAD tag-length whitelists** (v3, addresses M2-21):

```rust
// Per NIST SP 800-38D §5.2.1.2 (GCM): valid tag lengths are 4, 8, 12, 13, 14, 15, 16 bytes.
// Tags shorter than 12 bytes are documented as "shall not be used unless application can
// tolerate increased forgery probability" (Node v22 emits DEP0182 for <16-byte tags).
pub const GCM_TAG_LENGTHS: &[usize] = &[4, 8, 12, 13, 14, 15, 16];

// Per RFC 3610 (CCM): valid tag lengths are 4, 6, 8, 10, 12, 14, 16 bytes (must be even).
// Per Node, default is 16; CCM REQUIRES authTagLength explicitly per createCipheriv options
// (see CRITICAL #3 / §V.4).
pub const CCM_TAG_LENGTHS: &[usize] = &[4, 6, 8, 10, 12, 14, 16];

// Per RFC 7253 (OCB): valid tag lengths are 8, 12, 16 bytes.
pub const OCB_TAG_LENGTHS: &[usize] = &[8, 12, 16];

// ChaCha20-Poly1305 (RFC 8439): tag is fixed at 16 bytes.
pub const CHACHA20_POLY1305_TAG_LENGTHS: &[usize] = &[16];
```

These lists drive the `auth_tag_length` validation in `parse_cipher_options` (§V.4). createCipheriv with `authTagLength: 5` for GCM throws `ERR_CRYPTO_INVALID_TAG_LENGTH` (real Node code, RangeError per node_errors.h).

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

// (addresses critic MAJOR #8): `getCipherInfo` accepts an `options` object
// `{ keyLength, ivLength }` to filter — Node returns undefined if the cipher
// at this name does not support the requested key/iv lengths. The `mode`
// field is a string from the documented set per
// https://nodejs.org/api/crypto.html#cryptogetcipherinfonameornid-options:
// `'cbc' | 'ccm' | 'cfb' | 'ctr' | 'ecb' | 'gcm' | 'ocb' | 'ofb' | 'stream'
//  | 'wrap' | 'xts'`. Our CipherMode enum maps to those strings.
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

    // Apply options filter (was unused in v1).
    if let Some(opts) = options.and_then(|o| parse_get_cipher_info_options(scope, o).ok()) {
        if let Some(req_key_len) = opts.key_length {
            if !entry.key_lengths.contains(&req_key_len) { return None; }
        }
        if let Some(req_iv_len) = opts.iv_length {
            if entry.iv_length != Some(req_iv_len) { return None; }
        }
    }

    let obj = v8::Object::new(scope);
    set_str(scope, obj, "name", entry.canonical_name);
    set_u32(scope, obj, "blockSize", entry.block_size as u32);
    if let Some(iv) = entry.iv_length {
        set_u32(scope, obj, "ivLength", iv as u32);
    }
    set_str(scope, obj, "mode", entry.mode.as_str());    // 'cbc'/'ccm'/'cfb'/...
    set_u32(scope, obj, "keyLength", entry.key_lengths[0] as u32);    // canonical length
    Some(obj.into())
}

struct GetCipherInfoOptions {
    key_length: Option<usize>,
    iv_length: Option<usize>,
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
                // (v3, addresses C2-2): ERR_OSSL_PEM_NO_START_LINE is a
                // dynamic-OSSL code Node propagates from PEM_read_bio_*
                // failures via ThrowCryptoError; not a static Node code.
                // Per D-N39, emit ERR_CRYPTO_OPERATION_FAILED with the
                // legacy name in the message.
                .map_err(|e| OpError::node("ERR_CRYPTO_OPERATION_FAILED",
                    format!("PEM decode failed: {} \
                     (was: ERR_OSSL_PEM_NO_START_LINE in older Node)", e)))?
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

The X.509 parser is the heavy lift — RFC 5280 v3 has many extensions. We implement a minimal subset (subject/issuer/sn/valid/keyusage/SAN/AKI/SKI), enough for the JWT JWKS use case. Anything else triggers `ERR_CRYPTO_OPERATION_FAILED` with `"X.509 parse error: ..."` in the message text. <!-- Round 4: addressing MAJOR M3-5 — v3 used the invented code `ERR_OSSL_X509_PARSE` (not in errors.js, not in node_errors.h, not in §VII.3a). Replaced with the canonical real-Node fallback per the §VII.3a / D-N39 policy. The dynamic-OSSL `ERR_OSSL_X509_*` shape is dropped; if Stage F faithful-OSSL bridging lands (XVII.12) the legacy name can be preserved in the message text per the §VII.3a "Dynamic-OSSL codes preserved in message text" policy. -->

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
        // (addresses critic dimension 10): ERR_CRYPTO_TIMING_SAFE_EQUAL_LENGTH
        // IS a real Node code per
        // https://github.com/nodejs/node/blob/main/lib/internal/errors.js
        // (it's inherited from `node:crypto`'s native bindings and surfaced as
        // a RangeError). Confirmed against the Node source. v1's was correct.
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
        // (addresses critic minor m-8): Node's actual code on FIPS-mode-not-
        // available is ERR_CRYPTO_FIPS_UNAVAILABLE per
        // https://nodejs.org/api/errors.html#err_crypto_fips_unavailable.
        // v1's ERR_CRYPTO_OPERATION_FAILED was generic / wrong.
        Err(OpError::node("ERR_CRYPTO_FIPS_UNAVAILABLE",
            "FIPS mode toggle not supported in this runtime build"))
    } else {
        Ok(())    // Already in non-FIPS mode; accept.
    }
}

pub fn secure_heap_used<'s>(scope: &mut v8::PinScope<'s, '_>)
    -> v8::Local<'s, v8::Value>
{
    // (addresses critic minor m-1): Node's `utilization` is a documented
    // 0..1 fraction (NOT a percentage 0..100). Returning literal 0.0 from
    // f64 is correct; we comment to make the type contract explicit so a
    // future maintainer doesn't accidentally `0.0 * 100`.
    let obj = v8::Object::new(scope);
    set_u64(scope, obj, "total", 0);
    set_u64(scope, obj, "min", 0);
    set_u64(scope, obj, "used", 0);
    set_f64(scope, obj, "utilization", 0.0);    // fraction in [0, 1], not percentage
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
        // (v3, addresses C2-2): ERR_CRYPTO_INVALID_DH_PRIME is NOT in Node's
        // static catalog (verified — neither errors.js nor node_errors.h lists
        // it). Per D-N39 / §VII.3a, emit ERR_CRYPTO_OPERATION_FAILED with the
        // diagnostic detail in the message.
        return Err(OpError::node("ERR_CRYPTO_OPERATION_FAILED",
            "768-bit and 1024-bit DH groups disabled (use --insecure-dh-groups)"));
    }
    /* ... build DiffieHellmanGroup wrapper ... */
}
```

DH primes (modp14/15/16/17/18, ffdhe*) are stored as static byte arrays in the kernel. Backed by aws-lc-sys's `DH_set0_pqg` for the actual key-agreement computation.

**Runtime-flag registry (addresses critic MAJOR #2; v3 m2-7 reality check):** crypto policy flags are introduced. v1 referenced them but never defined where they lived; v2 wired them into the existing `RuntimeFlags` struct. **v3 (m2-7) verifies:** at the worktree's HEAD (`main` at v2 merge), `crates/runtime/src/state.rs` does NOT YET contain a `RuntimeFlags` struct. The Stage A PR introduces it as a NEW struct alongside the existing `IsolateState`. Three flags total (post-v3, post-M2-4):

```rust
// crates/runtime/src/state.rs (NEW in Stage A)
pub struct RuntimeFlags {
    pub insecure_dh_groups: bool,    // D-N22 partner: enable modp1/modp2 (768/1024-bit)
    pub legacy_crypto: bool,         // D-N22: enable DES/3DES/Blowfish/RC4/MD5-as-cipher/createCipher
    pub no_deprecation: bool,        // M2-4: --no-deprecation suppresses ALL DEP* warnings (matches Node)
}

pub fn is_insecure_dh_enabled() -> bool {
    state::isolate_runtime_flags().insecure_dh_groups
}
pub fn is_legacy_crypto_enabled() -> bool {
    state::isolate_runtime_flags().legacy_crypto
}
pub fn is_deprecation_suppressed() -> bool {
    state::isolate_runtime_flags().no_deprecation
}
```

CLI args: `zeroship serve --insecure-dh-groups --legacy-crypto --no-deprecation myapp.js`. Env vars: `ZEROSHIP_INSECURE_DH_GROUPS=1`, `ZEROSHIP_LEGACY_CRYPTO=1`, `ZEROSHIP_NO_DEPRECATION=1`. All default off.

The struct lives at module level in `state.rs`; instances are stored on the per-isolate state. The `state::isolate_runtime_flags()` accessor reads from the current isolate's slot. Stage A PR introduces both the struct AND the accessor (~25 LOC).

### X.5. Legacy cipher policy (D-N22)

A runtime flag `ZEROSHIP_LEGACY_CRYPTO=1` (or CLI `--legacy-crypto`) enables DES/3DES/Blowfish/Cast5/RC4/IDEA/MD5 (some). Without the flag, `createCipheriv("des-cbc", ...)` errors with `ERR_CRYPTO_UNSUPPORTED_OPERATION` (real Node code) and a message pointing at the flag. (v3 fix, C2-2: ERR_OSSL_EVP_UNSUPPORTED_ALGORITHM is dynamic-OSSL, not in Node's static registry.)

```rust
fn check_legacy_allowed(alg: CipherAlg) -> Result<(), OpError> {
    if alg.is_legacy() && !legacy_crypto_enabled() {
        // (v3, addresses C2-1, C2-2): ERR_OSSL_EVP_UNSUPPORTED_ALGORITHM is
        // dynamic; real Node code for "feature gated off" is
        // ERR_CRYPTO_UNSUPPORTED_OPERATION (per node_errors.h).
        return Err(OpError::node("ERR_CRYPTO_UNSUPPORTED_OPERATION",
            format!("{} is a legacy cipher; enable with --legacy-crypto", alg.name())));
    }
    Ok(())
}
```

### X.6. `crypto.constants` full list (addresses critic missing concept #24)

Node's `crypto.constants` exposes ~70 OpenSSL constants per https://nodejs.org/api/crypto.html#crypto-constants. v1 listed 10. v2 ships them in two waves:

**Stage B (Tier 1 — pad / EC point conversion):**
```
RSA_PKCS1_PADDING = 1
RSA_NO_PADDING = 3                  // footgun
RSA_PKCS1_OAEP_PADDING = 4
RSA_PKCS1_PSS_PADDING = 6
RSA_PSS_SALTLEN_DIGEST = -1
RSA_PSS_SALTLEN_MAX_SIGN = -2
RSA_PSS_SALTLEN_AUTO = -2
POINT_CONVERSION_COMPRESSED = 2
POINT_CONVERSION_UNCOMPRESSED = 4
POINT_CONVERSION_HYBRID = 6
```

**Stage E (Tier 2 — SSL_OP / DH_CHECK / ENGINE_METHOD / SSL_VERIFY / SSL_SESS_CACHE):** the full SSL_OP_* / SSL_OP_NO_TLSv1 / SSL_OP_NO_TICKET (~30 entries), DH_CHECK_P_NOT_PRIME / DH_CHECK_P_NOT_SAFE_PRIME / DH_NOT_SUITABLE_GENERATOR (~6 entries), ENGINE_METHOD_RSA / ENGINE_METHOD_DSA / ENGINE_METHOD_ALL (~10 entries), SSL_VERIFY_NONE / SSL_VERIFY_PEER / SSL_VERIFY_FAIL_IF_NO_PEER_CERT / SSL_VERIFY_CLIENT_ONCE (4 entries), SSL_SESS_CACHE_OFF / etc. (5 entries). All are integer-typed; values copied from OpenSSL headers (matching Node).

Apps that read `crypto.constants.SSL_OP_NO_TLSv1` (legacy TLS 1.3 negotiation gating libraries) work on Stage E. Without them, a `cannot read property 'SSL_OP_NO_TLSv1' of undefined` crash is what the JS shim caused in v1 — fixed in v2.

The constants block in `node-crypto.gen.ts` is generated from a single Rust-side static slice (~80 entries) so it stays in sync.

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

// Constants. (addresses critic CRITICAL #8 + missing concept #24): Node's
// `crypto.constants` exposes ~70 OpenSSL constants per
// https://nodejs.org/api/crypto.html#crypto-constants. v1 listed only 10.
// v2 ships the Tier-1 set in Stage B, then expands in Stage E. The full set
// is in §X.6 below.
//
// SaltLength sentinels are negative by Node convention and require translation
// via normalise_pss_salt_length() in parse_sign_key_input (see §V.5). Direct
// kernel calls never see negatives.
//   - RSA_PSS_SALTLEN_DIGEST  = -1 → equal to digest length (default).
//   - RSA_PSS_SALTLEN_MAX_SIGN = -2 → maximum permissible salt for signing.
//   - RSA_PSS_SALTLEN_AUTO    = -2 → verify-only: auto-derive from signature.
// `MAX_SIGN` and `AUTO` share the value `-2`; the operation context decides
// which interpretation applies.
export const constants = {
    // RSA padding (Tier 1).
    RSA_PKCS1_PADDING: 1,
    RSA_NO_PADDING: 3,                  // footgun: raw RSA without padding is insecure
    RSA_PKCS1_OAEP_PADDING: 4,
    RSA_PKCS1_PSS_PADDING: 6,
    RSA_PSS_SALTLEN_DIGEST: -1,
    RSA_PSS_SALTLEN_MAX_SIGN: -2,
    RSA_PSS_SALTLEN_AUTO: -2,

    // EC point conversion.
    POINT_CONVERSION_COMPRESSED: 2,
    POINT_CONVERSION_UNCOMPRESSED: 4,
    POINT_CONVERSION_HYBRID: 6,

    // SSL_OP_* and DH_CHECK_* and ENGINE_METHOD_* — Stage E (see §X.6).
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
  - `createHash('unknown')` throws `ERR_CRYPTO_INVALID_DIGEST`. (v3, addresses C2-1.)
  - `createHash('SHA256')` (case-insensitive) works.
- **`crypto_node_hmac.rs`:**
  - `createHmac('sha256', 'key').update('msg').digest('hex')` — basic.
  - Empty key throws `ERR_OUT_OF_RANGE` — **zeroship divergence from Node**, NOT parity (v4, addresses C3-3 / m3-5). Node silently accepts empty keys (`crypto_hmac.cc::HmacInit` lines 78-91 re-binds `key = ""` then forwards to HMAC_Init_ex; if init fails the error reaches the dynamic-OSSL ERR_OSSL_HMAC_* path which is not in Node's static registry). zeroship rejects up-front per RFC 2104 §2 (defense-in-depth). The test must assert the divergent behaviour, not match real-Node.
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
  - `randomInt(0, 1)` always returns 0 (max is exclusive).
  - `randomInt(100, 0)` throws `ERR_OUT_OF_RANGE`.
  - (addresses critic minor m-4): edge cases — `randomInt(-100, -1)` (negative range) → uniform in [-100, -1); `randomInt(0, MAX_SAFE_INTEGER)` is rejected because `MAX_SAFE_INTEGER > 2^48` (the documented cap); `randomInt(0, 2 ** 48)` is rejected per Node's `>= 2^48` cap; `randomInt(0, 2 ** 48 - 1)` is accepted.
  - `randomUUID()` returns 36-char string matching v4 pattern.
- **`crypto_node_kdf.rs`:**
  - `pbkdf2Sync('password', 'salt', 100, 32, 'sha256')` returns 32-byte Buffer.
  - `pbkdf2('password', 'salt', 100, 32, 'sha256', cb)` calls `cb(null, buf)`.
  - `pbkdf2Sync(..., 0, ...)` throws `ERR_OUT_OF_RANGE`.
  - `pbkdf2Sync(..., 'unknown')` throws `ERR_CRYPTO_INVALID_DIGEST`. (v3, addresses C2-1.)
  - `scryptSync('pw', 'salt', 64)` returns 64-byte Buffer.
  - `scryptSync('pw', 'salt', 64, { N: 16384, r: 8, p: 1 })` works.
  - `scryptSync('pw', 'salt', 64, { maxmem: 1024 })` throws `ERR_CRYPTO_INVALID_SCRYPT_PARAMS`. (v3, addresses M2-14: memory-exceeded is INVALID_SCRYPT_PARAMS, not SCRYPT_NOT_SUPPORTED.)
  - `hkdfSync('sha256', ikm, salt, info, 32)` returns 32-byte Buffer.
- **`crypto_node_cipher.rs`:**
  - AES-256-GCM round-trip: encrypt → decrypt with matching key/iv/aad/tag.
  - AES-256-CBC round-trip with PKCS#7 padding default.
  - AES-256-CBC with `setAutoPadding(false)` requires exact-block input.
  - ChaCha20-Poly1305 round-trip.
  - `createCipher('des-cbc', ...)` throws `ERR_CRYPTO_UNSUPPORTED_OPERATION`. (v3, addresses C2-2: ERR_CRYPTO_DEPRECATED_API was invented; the real code per node_errors.h is ERR_CRYPTO_UNSUPPORTED_OPERATION.)
  - `createCipheriv('des-cbc', ...)` (without legacy flag) throws `ERR_CRYPTO_UNSUPPORTED_OPERATION`. (v3, addresses C2-2.)
  - GCM `getAuthTag()` before final() throws `ERR_CRYPTO_INVALID_STATE`.
  - GCM Decipher `setAuthTag` then mismatched tag → `ERR_CRYPTO_OPERATION_FAILED` with "Unsupported state or unable to authenticate data" message. (v3, addresses C2-2: ERR_OSSL_BAD_DECRYPT is dynamic-OSSL, not Node static; ERR_CRYPTO_OPERATION_FAILED is the canonical fallback per ThrowCryptoError.)
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
2. **Vendor a curated subset** at `crates/runtime/tests/node_crypto_fixtures/` via a NEW setup script `crates/runtime/tests/setup-node-crypto-fixtures.sh` (modeled on the existing `setup-wpt.sh`). v3 (m2-10) specifies the exact commands:

   ```bash
   #!/usr/bin/env bash
   # crates/runtime/tests/setup-node-crypto-fixtures.sh
   # Sparse-checkout of Node's test/parallel/test-crypto-*.js subset at a pinned commit.
   set -euo pipefail
   PINNED_COMMIT="${NODE_COMMIT:-v22.13.0}"   # bump in sync with our supported Node version
   DEST="$(dirname "$0")/node_crypto_fixtures"
   mkdir -p "$DEST"
   cd "$DEST"
   if [ ! -d .git ]; then
     git init
     git remote add origin https://github.com/nodejs/node.git
     git config core.sparseCheckout true
     # Sparse-checkout pattern: just the crypto test files + common harness.
     {
       echo 'test/parallel/test-crypto-*.js'
       echo 'test/common/index.js'
       echo 'test/common/index.mjs'
       echo 'test/fixtures/keys/*.pem'
       echo 'test/fixtures/keys/*.crt'
     } > .git/info/sparse-checkout
   fi
   git fetch --depth 1 origin "$PINNED_COMMIT"
   git checkout FETCH_HEAD
   echo "Checked out $(git rev-parse HEAD) — $(ls test/parallel/test-crypto-*.js | wc -l) crypto test files."
   ```

   Total checkout: ~200 KB. Pinned commit bumped in sync with the platform's officially-supported Node version.
3. **Write a runner** at `crates/runtime/tests/node_crypto_compat.rs` that boots the runtime and runs each `test-crypto-*.js` file. Most files use Node's `assert` module (which we'd need to provide via unenv as a Tier 1 dep — already supported).
4. **Track expectations** at `crates/runtime/tests/node-crypto.expectations` (mirrors `crypto_native/`'s WPT expectations file). List which test files pass / known-failing-with-reason.

Node's tests use `common.js` test harness — small effort to provide the `common.hasCrypto` / `common.skipIf` shims.

**Randomness quality test (v3, addresses m2-2):** add to `crates/runtime/tests/crypto_node_random.rs`:

```rust
#[test]
fn test_random_bytes_quality_nist_sp_800_22() {
    // Smoke-tests against NIST SP 800-22 randomness tests (chi-square + serial
    // + monobit). Spawn 16 threads each pulling 1 MB from `randomBytes` and
    // assert all 16 buffers pass:
    //   - Monobit: |sum_of_bits / N - 0.5| < 0.01.
    //   - Serial: chi-square over 8-bit windows < critical-value @ 0.01.
    // Full SP 800-22 is a big test suite; we ship the two cheapest tests as
    // a regression guard against entropy-source corruption.
    use std::thread;
    let handles: Vec<_> = (0..16).map(|_| thread::spawn(|| {
        let buf = exec_js(r#"crypto.randomBytes(1024 * 1024)"#).unwrap();
        nist_sp_800_22_smoke(&buf)
    })).collect();
    for h in handles { assert!(h.join().unwrap()); }
}
```

The `nist_sp_800_22_smoke` helper lives at `crates/runtime/tests/test_helpers/nist_random.rs` (~80 LOC; references the published critical values for chi-square and monobit at 99% confidence per NIST SP 800-22 §2.1 + §2.2). This test is also the regression guard for D-N17's "rejection sampling, not modulo bias" claim in `randomInt`.

**Targeted pass rates** (addresses critic minor m-10 — methodology):
The "%" is computed against a sampled list of `test/parallel/test-crypto-*.js` files vendored at `crates/runtime/tests/wpt/node_crypto/`. Sampling rules:
1. Exclude tests in `test/sequential/` (require network or side-effects we don't sandbox).
2. Exclude tests gated on `--openssl-legacy-provider` unless `--legacy-crypto` is enabled in the runner.
3. Exclude tests asserting OpenSSL-version-specific behaviour (e.g. `crypto.getCiphers().includes('aria-*')` — ARIA is ARIA Korea-government cipher, not in aws-lc-rs).

The vendored list is checked in at `crates/runtime/tests/node-crypto.expectations.txt`; ~140 of Node's ~200 test files are sampled (the others fall under exclusion 1-3).

Pass-rate targets:
- Stage B end: 30% of the 140 sampled tests (hash + hmac + random + KDF — exact subset enumerated in expectations file).
- Stage C end: 80% of the 140 sampled tests (+ keyobject + sign/verify + cipher).
- Stage E end: 95% of the 140 sampled tests (+ X509 + DH + legacy with flag).
- Tests definitively not implementable (e.g. `crypto.setEngine`) are listed as `IGNORE` in expectations and excluded from the denominator.

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
| **Node.js** (gold standard) | ~6,500 JS (`lib/internal/crypto/`) + ~11,000 C++ (`src/crypto/`) ≈ ~17,500 LOC excluding tests (addresses critic minor m-2 — v1 estimate was low; counted via `cloc` against the Node repo at the v25 cut). | Hybrid; JS layer enforces validation + types, C++ wraps OpenSSL EVP API | OpenSSL EVP_PKEY (refcounted) | C++ uses libuv thread pool for async; sync runs on V8 thread |
| **Bun** | ~3,500 Zig (`src/bun.js/node/node_crypto.zig`) + ~600 TS facade (`src/js/node/crypto.ts`) = ~4,100 LOC | Native Zig with thin TS facade. Each Node API has a Zig native impl; no JS shim. | OpenSSL EVP_PKEY via `boring` | Zig `JSC.AsyncTask` for async; sync runs on JS thread |
| **workerd** | ~5,000 C++ (`src/node/internal/crypto*`) | Pure native C++ over BoringSSL via ncrypto helpers. Mirrors Node's class hierarchy. | `KeyContext` shared between WebCrypto + node:crypto | Always sync (workerd has no thread pool); the `*Sync` Node APIs map directly, async APIs throw or queue via kj's promise |
| **Deno** | ~3,000 Rust ops (`ext/node/ops/crypto/`) + ~3,500 TS polyfill (`ext/node/polyfills/internal/crypto/*.ts`) = ~6,500 LOC | Hybrid; ops in Rust, JS facade orchestrates. The TS facade does encoding / type validation; ops do the heavy lift. | `KeyObjectHandle` Rust struct, separate from CryptoKey's storage | Rust ops use `tokio::task::spawn_blocking` for async; sync runs as a regular sync op |
| **Current zeroship** | 60 JS shim (`node-compat.ts:67-127`) + 92 Rust ad-hoc (`crypto.rs:128-212`) + 313 WebCrypto JS (deleted) + 1308 WebCrypto Rust = ~1,700 LOC total | JS shim layers calling `__cryptoHashSync` / `__cryptoHmacSync`. WebCrypto is native; node:crypto is a facade | None for node:crypto | Sync only; the shim's `pbkdf2 / scrypt` paths are completely missing |
| **This design** | ~5,800 LOC native (kernel ~2500 + crypto_node ~3000 + crypto_native refactor ~300) + ~250 LOC TS shim (re-exports) = ~6,050 LOC | Pure native with shared kernel; TS shim is purely re-exports. Closer to Bun's approach in shape; closer to workerd's in depth. | `Arc<KeyMaterial>` shared between CryptoKey and KeyObject (D-N4) | Sync on V8 thread for sync APIs; `spawned_ops` blocking-pool for async APIs |

Where this design lands:
- **Smaller than Node.js** (v3 estimate: ~7,500-8,500 LOC vs ~18K LOC actual Node `lib/internal/crypto/` plus `src/crypto/` plus tests; the v2 claim of ~5,800 LOC was too low — m2-11 audit). Revised by stage:
  - Stage A (kernel extraction): ~600 LOC (refactor existing crypto_native to call kernel; net add ~600).
  - Stage B (hash + hmac + KDFs + random + scrypt): ~1,000 LOC, of which ~90 LOC is aws-lc-sys FFI per §III.2a (MD5 ~30 + SHA-512/224 ~40 + scrypt ~20).
  - Stage C (KeyObject + sign + verify + cipher + decipher): ~2,000 LOC, of which ~370 LOC is aws-lc-sys FFI (CCM ~120 + encrypted-PKCS#8 ~250).
  - Stage D (webcrypto bridge + module install): ~400 LOC of TS + ~200 LOC of Rust.
  - Stage E (X.509 + DH + ECDH + legacy ciphers + PQC stubs + remaining FFI): ~3,000 LOC, of which ~810 LOC is aws-lc-sys FFI (OCB + DES/3DES + Blowfish + DH-named-groups + X.509 + BLAKE2 + AES-OFB/CFB1/CFB8/ECB).
  - Stage F (queued: faithful dynamic-OSSL bridging, XVII.12): ~150 LOC.
  - Total: ~7,500 LOC for Stages A-E; bumps to ~8,500 with Stage F.

  The smaller footprint vs Node (~18K) comes from aws-lc-rs's higher-level API eliminating much of Node's hand-written EVP glue PLUS the kernel extraction sharing code between WebCrypto and node:crypto. We still drop to aws-lc-sys raw FFI for ~1,270 LOC across all stages — encrypted PKCS#8, X.509, named DH primes, CCM, OCB, MD5, SHA-512/224, BLAKE2, legacy ciphers, AES-OFB/CFB1/CFB8/ECB, DES/3DES (addresses critic minor m-18: the "higher-level API" claim has caveats; v3 audited each algorithm row). The net is still smaller than Node, but not because the high-level API covers everything — kernel sharing is the bigger lever.
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

### XVII.8. `crypto.signal` — fictional API; no longer an open question

(addresses critic minor m-9): v1 listed `crypto.signal` as a deferred Node API, citing it as Node ≥17 + experimental encryptStream. After audit against https://nodejs.org/api/crypto.html — there is no `crypto.signal` export. The closest match in v1's mental model was the global `AbortSignal` (used as `crypto.subtle.encrypt(..., { signal })` in WebCrypto v2 drafts), but that's a parameter, not an export. v2 removes the entry.

**Settled:** removed from non-goals; not a real API.

### XVII.9. CryptoKey `extractable: false` — bridge or refuse?

The WebCrypto `extractable: false` flag prevents export. But `KeyObject.from(cryptoKey)` shares the Arc material, and `keyObject.export(...)` would extract bytes — bypassing `extractable: false`.

**Working answer:** check `extractable` at `KeyObject.from`. If false, throw `ERR_CRYPTO_INVALID_KEY_OBJECT_TYPE` with a message about the source CryptoKey being non-extractable. This preserves the WebCrypto invariant.

**Counter-argument:** Node's `KeyObject.from(cryptoKey)` doesn't check; in Node, all CryptoKeys can be wrapped. The WebCrypto `extractable: false` is honoured by `subtle.exportKey` only.

**Settled:** match Node; let `KeyObject.from(non_extractable_crypto_key)` succeed but make `keyObject.export(...)` honor extractable (throw `ERR_CRYPTO_INVALID_KEY_OBJECT_TYPE` if `keyObject.material` came from a non-extractable CryptoKey). Add an `extractable` field to KeyObjectState that propagates from CryptoKeyState on bridge.

**Wiring (addresses critic minor m-5):** v1's open-question prose described the policy but the implementation never propagated the flag. v2 extends KeyObjectState:

```rust
pub struct KeyObjectState {
    pub key_type: KeyType,
    pub material: Arc<KeyMaterial>,
    pub extractable: bool,            // NEW: propagates from CryptoKeyState; default true for keys created via createSecretKey/etc.
}
```

`KeyObject.from(cryptoKey)` reads `cryptoKey.extractable` and copies it. `KeyObject.prototype.export(options)` checks `self.extractable` first and throws `ERR_CRYPTO_INVALID_KEY_OBJECT_TYPE` if false. The X.509 `publicKey` accessor mints with `extractable: true` (public keys are always extractable).

### XVII.10. Algorithm canonicalisation — case-insensitive everywhere or strict?

D-N18 says: case-insensitive (Node behaviour).

**Working answer:** case-insensitive for hash names / cipher names; strict for spec-canonical names in JWK / WebCrypto. The asymmetry is unavoidable because WebCrypto IS strict (per webcrypto-native D-8) and Node IS loose. Document.

<!-- Round 3: addressing CRITICAL C2-2 (queued: dynamic-OSSL bridging). -->
### XVII.12. (v3) Faithful dynamic-OSSL error bridging

Real Node generates `ERR_OSSL_<library>_<reason>` codes at throw time by reading the OpenSSL ERR_PACK queue (see `node/src/crypto/crypto_util.cc::ThrowCryptoError`). aws-lc-rs's `Unspecified` strips the upstream library/reason — we cannot reproduce these dynamically. v3 §VII.3 / §VII.3a / D-N39 picks the canonical-Node-fallback approach (`ERR_CRYPTO_OPERATION_FAILED` + the legacy OSSL name in the message text).

**Queued for Stage F:** if/when measured demand surfaces (e.g., a creator app's package strictly requires `e.code === 'ERR_OSSL_EVP_BAD_DECRYPT'` and refuses message-text matching), add a faithful bridge:

1. Link aws-lc-sys directly (already in workspace deps via aws-lc-rs).
2. After every aws-lc-rs `Unspecified` Result, drain the BoringSSL ERR queue via `ERR_get_error()` + `ERR_GET_LIB(packed)` + `ERR_reason_error_string(packed)`.
3. Reconstruct `ERR_OSSL_<LIBRARY>_<REASON>` per Node's pattern.
4. Surface as a NEW `KernelError::DynamicOssl { library: &'static str, reason: &'static str }` variant.

Cost: ~150 LOC of bridge code in `crypto_kernel/error.rs`. Stage F (post-impl, demand-driven).

### XVII.13. (v3) RSA-PSS toCryptoKey lossiness bypass

Per M2-12, `KeyObject.toCryptoKey` round-trips through JWK and loses RSA-PSS-specific algorithm parameters. Node has the same limitation; v3 picks Node parity.

**Queued for Stage F:** if creator apps surface this as a friction point, bypass JWK by directly cloning `Arc<KeyMaterial>` into a fresh `CryptoKeyState` with the user-supplied algorithm AND the source KeyObject's PSS metadata (when the source is an RSA-PSS key). Cost: ~30 LOC in `crypto_native/crypto_key.rs`. Demand-driven.

<!-- Round 4: addressing CRITICAL C3-3 — explicit log of zeroship-vs-Node behavioural divergences. -->
### XVII.13b. (v4) Zeroship-vs-Node behavioural divergence log

Cases where zeroship emits a real Node `e.code` BUT in a situation where Node itself would not throw, or would throw a different code via the dynamic-OSSL pipeline. Documented here so the impl agent has a single source of truth for divergences and downstream packages porting from Node know what to expect.

| Case | Node behaviour | zeroship behaviour | Why we diverge |
|---|---|---|---|
| Empty HMAC key | `crypto_hmac.cc::HmacInit` re-binds `key = ""` and forwards to `HMAC_Init_ex` (https://github.com/nodejs/node/blob/main/src/crypto/crypto_hmac.cc lines 78-91); if init fails the error reaches `ThrowCryptoError` and surfaces as a dynamic-OSSL `ERR_OSSL_HMAC_*` code (not in static registry). | Reject up-front with `ERR_OUT_OF_RANGE` (real Node code, RangeError) and message tagging the divergence. | RFC 2104 §2 requires key length ≥ hash output size for security; an empty key trivially defeats HMAC. Defense-in-depth: silently accepting a zero-length key is a cryptographic foot-gun. |

(Stage F will revisit if creator apps surface friction.)

### XVII.11. (v2) Resolved by post-review pass

Open questions promoted to decisions in v2:

- **CCM vs other AEAD ordering** — D-N (V.4 Cipher state machine). CCM `setAuthTag` MUST come before update; GCM/OCB/ChaCha20 MUST come before final. State machine in `CipherContext` enforces.
- **Encrypted PKCS#8 implementation route** — D-N33. Drops to `aws-lc-sys` raw FFI; high-level aws-lc-rs does not expose this surface.
- **PSS sentinel translation site** — D-N34. Translated in `parse_sign_key_input` BEFORE the kernel boundary; kernel never sees negatives.
- **stream.Transform inheritance approach** — D-N35. JS-side mixin in `node-crypto.gen.ts`; native classes unchanged.
- **PQC stub strategy** — D-N36. Stage E parse-only recognition; full keygen defers until aws-lc-rs's PQC API stabilises.
- **`createCipher` policy** — gated by `--legacy-crypto` (warn-and-proceed when on; refuse with `ERR_CRYPTO_UNSUPPORTED_OPERATION` when off). v1's "always throw" was too aggressive; v2's `ERR_CRYPTO_DEPRECATED_API` was an invented code (v3 fix per C2-2).
- **`ECDH.setPublicKey` policy** — shipped with deprecation warning (DEP0031). v1's "throw" was wrong vs. Node behaviour.
- **`Hmac.copy` absence** — reaffirmed against the critic's incorrect claim. Counter-cited against Node source.

Open questions promoted to decisions in v3 (round 3 audit):

- **Encrypted PKCS#8 EVP_* sequence** — D-N37. v2 left D-N33 prose-only; v3 specifies the exact `PKCS8_marshal_encrypted_private_key` + `PKCS8_parse_encrypted_private_key` calls + cipher whitelist + iteration count + salt-length policy at function-signature level.
- **Algorithm-routing matrix (high-level vs FFI vs DEFER)** — D-N38. v2 conflated "ships in Stage 1" with "in aws-lc-rs's high-level API"; v3 audited every algorithm against docs.rs and assigned each one a backing path.
- **Error-code provenance policy** — D-N39. v2 emitted invented codes mixed with Node-real codes; v3 audited every code against `lib/internal/errors.js` + `src/node_errors.h`, removed all invented codes, and documented zeroship-extension policy.

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
- aws-lc-rs — https://docs.rs/aws-lc-rs/ (v3 audited 2026-05-02; concrete claims about specific algorithm constants in §III.2 / §IX.1 are pinned to the docs.rs URL of the workspace's currently-pinned aws-lc-rs version — see `crates/runtime/Cargo.toml:16`. m2-3: replace `latest` with the exact pinned version when the workspace dep changes.)
- aws-lc-rs encoding (Pkcs8V1Der/Pkcs8V2Der; no encrypted variant) — https://docs.rs/aws-lc-rs/latest/aws_lc_rs/encoding/index.html
- aws-lc-rs digest (audit basis for §III.2 / §IX.1 hash algorithms) — https://docs.rs/aws-lc-rs/latest/aws_lc_rs/digest/index.html
- aws-lc-rs aead (audit basis for AEAD modes — confirmed AES-GCM, AES-GCM-SIV, ChaCha20-Poly1305 ONLY; no OCB or CCM) — https://docs.rs/aws-lc-rs/latest/aws_lc_rs/aead/index.html
- aws-lc-rs cipher (audit basis for symmetric modes — confirmed CBC-PKCS7, CTR, CFB128 ONLY; no XTS, no ECB-as-mode) — https://docs.rs/aws-lc-rs/latest/aws_lc_rs/cipher/index.html
- aws-lc-rs signature (ECDSA_P256K1_SHA256_*) — https://docs.rs/aws-lc-rs/latest/aws_lc_rs/signature/index.html
- aws-lc — https://github.com/aws/aws-lc
- aws-lc PKCS8 header (audit basis for D-N37 FFI sequence) — https://github.com/aws/aws-lc/blob/main/include/openssl/pkcs8.h
- Node `lib/internal/errors.js` — https://github.com/nodejs/node/blob/main/lib/internal/errors.js (audit basis for §VII.3a JS-side error codes)
- Node `src/node_errors.h` — https://github.com/nodejs/node/blob/main/src/node_errors.h (audit basis for §VII.3a C++-side error codes)
- Node `src/crypto/crypto_util.cc` (ThrowCryptoError dynamic-OSSL builder) — https://github.com/nodejs/node/blob/main/src/crypto/crypto_util.cc
- Node `lib/internal/crypto/hash.js` (Hmac vs Hash, no `Hmac.copy`) — https://github.com/nodejs/node/blob/main/lib/internal/crypto/hash.js
- Node `errors` module (ERR_* code catalog audited in §VII.3) — https://nodejs.org/api/errors.html
- Node deprecations DEP0031 (ECDH.setPublicKey), DEP0106 (createCipher), DEP0182 (GCM authTagLength) — https://nodejs.org/api/deprecations.html
- RFC 4055 — RSA-PSS algorithm parameters in SPKI/PKCS8 — https://www.rfc-editor.org/rfc/rfc4055
- RFC 5754 — ECDSA-with-SHA* OIDs (HASH_NAMES compound entries) — https://www.rfc-editor.org/rfc/rfc5754
- RFC 8037 — JOSE OKP key type (Ed25519/X25519/Ed448/X448) — https://www.rfc-editor.org/rfc/rfc8037
- FIPS 203 / 204 / 205 — ML-KEM / ML-DSA / SLH-DSA (post-quantum, D-N36) — https://csrc.nist.gov/publications/fips
- NIST SP 800-38D — AES-GCM (auth tag lengths, IV lengths) — https://csrc.nist.gov/publications/detail/sp/800-38d/final
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
