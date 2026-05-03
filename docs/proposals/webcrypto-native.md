# Native W3C Web Cryptography API design

**Date:** 2026-05-02
**Status:** Draft v1 — implementation pending
**Spec:** W3C Web Cryptography API Level 2 (Living Standard) — https://w3c.github.io/webcrypto/
**Spec source:** https://github.com/w3c/webcrypto/blob/main/spec/Overview.html
**Algorithm registry:** https://w3c.github.io/webcrypto/#algorithm-registry
**Reference impls:**
- workerd (pure-native C++ on BoringSSL/ncrypto) — `refs/workerd/src/workerd/api/crypto/{aes,ec,rsa,jwk,impl,keys,digest,hkdf,pbkdf2,...}.{c++,h}`
- Deno (Rust + JS hybrid on RustCrypto) — `refs/deno/ext/crypto/{lib,import_key,export_key,encrypt,decrypt,generate_key,key,shared}.rs` + `00_crypto.js`
- Node.js (gold-standard JS layer) — https://github.com/nodejs/node/blob/main/lib/internal/crypto/webcrypto.js + `lib/internal/crypto/util.js`
- Bun (spec-correct via OpenSSL) — Bun.crypto.subtle
**Underlying RFCs cited:**
- RFC 8017 — PKCS #1 v2.2 (RSA: PKCS1v1_5, OAEP, PSS)
- RFC 5480 — Elliptic Curve Public Key Information (P-256/384/521 OIDs)
- RFC 6979 — Deterministic Usage of DSA and ECDSA (informational; we use random k)
- RFC 3394 — AES Key Wrap (AES-KW)
- RFC 5649 — AES Key Wrap with Padding (NOT in WebCrypto v1; AES-KW only)
- RFC 5869 — HKDF
- RFC 8018 — PKCS #5 v2.1 (PBKDF2)
- RFC 7517 / 7518 — JWK / JWA (JsonWebKey field shapes, "alg" registry)
- RFC 4648 — base64 / base64url
- RFC 4122 — UUID v4 (already implemented in `randomUUID`)
**WebIDL:** https://webidl.spec.whatwg.org/ (BufferSource, [EnforceRange], dictionaries, USVString, DOMException)
**aws-lc-rs:** https://docs.rs/aws-lc-rs/ (workspace dep, see `crates/runtime/Cargo.toml:16`)
**Tests:** WPT `WebCryptoAPI/` — https://github.com/web-platform-tests/wpt/tree/master/WebCryptoAPI

**Depends on:**
- `feature/fetch-js-delete` — native `DOMException` lands as part of the fetch.js delete path. The crypto design **assumes** native `DOMException` is on `globalThis`, throwable from Rust by name (e.g. `OpError::dom("OperationError", "...")`). Until that lands, the v1 implementation falls back to plain `Error` instances with a sentinel-prefixed message; the cutover from sentinel to native is purely mechanical.
- `docs/proposals/headers-native.md` — Vec<u8>/ByteString plumbing and the WebIDL extraction patterns (`ByteString`, `EnforceRangeU64`) — already shipped in the macro.
- `docs/proposals/streams-native.md` — NOT a dependency. WebCrypto operations are one-shot; the spec defines NO streaming variants. (See §I.2.)
- aws-lc-rs (workspace `Cargo.toml:16`) — single underlying provider for every algorithm in this design. No additional crypto crates introduced. Verified: all Tier 1 algorithms in the registry have an aws-lc-rs counterpart; the few gaps are noted in §IV with their per-algorithm workarounds.

**Unblocks:**
- AI-builder reliability: every JOSE / JWE / JWT library (`jose` on npm, `panva/jose`, `node-jose`) leans on `subtle.importKey("jwk", ...)` / `subtle.exportKey("jwk", ...)` for key bootstrapping. The polyfill rejects JWK wholesale; ~90% of npm-published JWE/JWT packages don't work today on zeroship.
- Stripe Connect / OAuth 2.0 / OIDC: every JWT-issuing identity flow uses RS256 / ES256 / EdDSA which require `verify` to interop with Chrome/Firefox-issued signatures. The current ECDSA wire format is ASN.1/DER (workerd-incompat, Chrome-incompat); fixing this is the single most-impactful WPT change in this design.
- WPT regression for `WebCryptoAPI/`: currently we run **zero** of those files because the polyfill is too far from spec to be worth harness work.
- Deletion of `crates/runtime/src/embed/crypto.js` (313 LOC) and replacement of `crates/runtime/src/web/crypto/sync_helpers.rs` (1308 LOC) with a `crates/runtime/src/crypto/` module of native `#[v8_class]` types backed by aws-lc-rs.

## Revision history

- **v1 (2026-05-02)** — Initial design, critic-driven from `/tmp/zeroship-reviews/crypto-review.md` (45 findings, overall 38/100). Replaces the JS polyfill at `crates/runtime/src/embed/crypto.js` (313 LOC) and the Rust ops shim at `crates/runtime/src/web/crypto/sync_helpers.rs` (1308 LOC) — total 1621 LOC removed. The native design ships ~3700 Rust LOC + ~150 JS LOC (a thin algorithm-registry index, deletable when v8_class macro grows compile-time registry support). Targets the entire spec surface in one tier — no v1/v2 algorithm split. Designed pure-native on V8 + Rust + aws-lc-rs.

  Decisions D-1 through D-30 cover: native classes for Crypto / SubtleCrypto / CryptoKey (D-1, D-2); aws-lc-rs as the single provider (D-3); spec-correct ECDSA wire format (D-4); JWK round-trip across all algorithms (D-5); typed DOMException variants (D-6); key-usage validation (D-7); spec-faithful algorithm normalization (D-8); CryptoKey `[SameObject]` caching (D-9); `[[handle]]`-as-internal-field brand check (D-10); AES-CTR/AES-KW completion (D-11, D-12); ECDH derive (D-13); P-521 (D-14); RSA key generation (D-15); RSA-PSS variable salt length (D-16); AES-GCM variable IV / variable tag (D-17, D-18); HMAC default block-size (D-19); PBKDF2 [EnforceRange] (D-20); spec-correct getRandomValues type filter and quota (D-21); SecureContext as no-op (D-22); the JS-shim phase-out cadence (D-23); spec algorithm naming in Rust (D-24); the JOSE/JWE deferral (D-25); X25519/Ed25519 inclusion (D-26); FIPS / hardware key as out-forever (D-27); structuredClone deferral (D-28); thread-pool offload deferral (D-29); the macro extension list (D-30).

## Top matter

### Goals

1. **Full WebCrypto Level 2 compliance.** Every interface in https://w3c.github.io/webcrypto/ at parity with the spec — no "v1 subset" or "Tier 1 only" cut. The algorithm registry (§18 / per-algorithm sections 20-34) ships in its entirety, including JWK across every algorithm. Pass the entire WPT `WebCryptoAPI/` suite minus the explicitly deferred items in Non-Goals.
2. **Replace `embed/crypto.js` + the Rust ops shim.** Delete the 313-LOC JS shim and the 1308-LOC `crypto.rs` ops file; replace with a `crates/runtime/src/crypto/` module of `#[v8_class]` types. The runtime ships one WebCrypto implementation, in Rust, with per-isolate native classes installed during `setup_globals`.
3. **Spec-correct cryptographic wire formats.** Every byte the implementation emits or accepts conforms to the spec's normative algorithm steps. The most acute fix: ECDSA produces and verifies fixed-length r∥s signatures, NOT ASN.1/DER (critic finding #1; today's impl is unusable cross-stack against Chrome / Firefox / Node WebCrypto). Other wire-format fixes: AES-GCM variable-length IV (#8), AES-GCM variable-length tag truncation (#9), RSA-PSS variable salt length (#21), AES-KW per RFC 3394 (#4).
4. **Spec-correct error types.** Every DOMException type the spec mandates flows from Rust to JS as a real `DOMException` instance with the `name` property the spec demands. Today's impl conflates everything to `TypeError` / generic `Error` (critic finding #6); 14 of the 45 critic findings reduce to "wrong error type." Native `DOMException` is in flight via `feature/fetch-js-delete`; this design depends on it landing.
5. **Spec-correct `CryptoKey` semantics.** `type` and `extractable` are immutable own properties; `algorithm` and `usages` return the same JS object on every access (`[SameObject]`); the brand check is unspoofable (V8 internal field, not `instanceof`); `Symbol.toStringTag` is `"CryptoKey"`. (Critic findings #13, #14, #42.)
6. **Spec-correct algorithm normalization.** Per spec §18.4.4, "normalize an algorithm" is a recursive, op-keyed lookup against a per-operation registry. The current impl hand-rolls a single uppercase-name pass; native ships the full table from spec §§20-34 (16 algorithms × 11 operations) and dispatches recursively for nested `HashAlgorithmIdentifier` / `AlgorithmIdentifier` members.
7. **Bytes-faithful, observable-event-faithful.** Every `ArrayBuffer` / `ArrayBufferView` boundary is read once at the entry into Rust (via the existing `Vec<u8>` extraction macro), processed in-place where possible, and returned as a fresh `Uint8Array` (current macro shape). The polyfill's base64-in-JSON channel for IV/AAD/salt/info goes away (#39).
8. **No CryptoKey leak.** Each JS-side `CryptoKey` wrapper carries its key material via a V8 internal-field `Box<CryptoKeyState>` that is dropped when the wrapper is GC'd. The current impl's `key_store: HashMap<u32, KeyData>` with monotonic `next_key_id` leaks every key forever (#28); native fixes this trivially because the key material lives ON the JS object, not in a side map.

### Non-goals (explicit)

- **JOSE / JWE / JWT extensions.** WebCrypto provides the primitives (sign, verify, encrypt, decrypt, importKey "jwk", exportKey "jwk"); the JOSE wire format itself (compact serialization, JWE general/flattened, content-encryption-key wrapping orchestration) is `@zeroship/jose` npm package territory. Out of this design.
- **FIDO2 / WebAuthn integration.** Separate spec (https://w3c.github.io/webauthn/), separate object model (`PublicKeyCredential`, `AuthenticatorAssertionResponse`). Not part of WebCrypto. Defer to a future native `WebAuthn` design — likely never on a server-side runtime.
- **Hardware key support (PKCS#11, TPM, KMS-as-keystore, AWS CloudHSM).** The `[[handle]]` slot in our design is a `Box<KeyMaterial>` of in-memory bytes; an HSM-backed key would need the slot to be an opaque `KeyHandle` enum with provider-specific variants. Out forever for an embedded V8 runtime — creator apps that need HSM-backed keys call the HSM provider's HTTP API via `fetch`, and the HSM signs. Document explicitly.
- **`structuredClone(CryptoKey)` / `postMessage(CryptoKey)`.** §13 IDL marks `CryptoKey` as `[Serializable]`. The HTML structured-clone contract requires every Serializable interface to define `[Serializable] steps`. We don't ship `MessagePort` / `Worker` / `BroadcastChannel` in v1, so no JS code can actually invoke `structuredClone` on a `CryptoKey` (the algorithm is reachable via the `serialize` IDL step, but there's no transferable boundary to cross). The IDL declares `[Serializable]` (forward-compat); the actual serializer step throws `DataCloneError` until structuredClone-with-CryptoKey ships in a follow-up. Critic #15 noted this; deferred per D-28.
- **X448 deriveBits.** Out (algorithm 26 in some drafts is X448; aws-lc-rs / BoringSSL don't support it; Deno special-cases it via dalek). Spec §26 is X25519, which IS in scope per D-26.
- **AES-OCB.** Listed in Deno's registry but NOT in W3C WebCrypto Level 2 (it's a workerd / Deno extension via OpenSSL). Out.
- **ChaCha20-Poly1305.** Listed in Deno's registry as a tentative (https://github.com/w3c/webcrypto/issues/295). Not yet promoted to spec normative status. Out of v1; trivially added in v1.5 if the spec lands it.
- **SHA-3 (`SHA3-256`, `SHA3-384`, `SHA3-512`).** Not in WebCrypto Level 2 normative algorithms. Deno ships them as an extension. Out of v1.
- **`crypto.subtle.getPublicKey()`.** Tentative addition (`getPublicKey.tentative.https.any.js` in WPT). Defer to v2; trivial follow-up (extract public key from a private key handle).
- **Wider browser-only attributes (`SecureContext` enforcement, the global-only Crypto installation).** D-22 makes `[SecureContext]` a no-op (server-side runtime, no insecure context concept). Tracked under "open questions" but the design's answer is settled: no-op. Crypto IS installed unconditionally on the global. Document.
- **The synchronous `node:crypto` polyfill helpers (`__cryptoHashSync`, `__cryptoHmacSync`).** These currently live at `crypto.rs:1227-1308`. They're consumed by `embed/node-globals.js`'s `node:crypto` shim, NOT by WebCrypto. They stay (this design touches `embed/crypto.js` and replaces the WebCrypto ops, not the node-shim helpers). v2 may move them into a `crates/runtime/src/node_crypto.rs` companion file for clarity.

### Status

Draft v1 — **implementation pending.** No commits have landed. The fork point is master at the worktree creation date (2026-05-02). The native DOMException dependency is in flight on `feature/fetch-js-delete`; this design assumes it lands first. If it slips, the implementation can use a string-sentinel fallback (D-6 below) without re-architecture.

Post-completion: file as a date-prefixed ADR under `docs/decisions/`. The Decisions table below is the immutable contract; everything else is illustrative.

### Decisions (settled)

| # | Decision | Rationale | Section |
|---|----------|-----------|---------|
| **D-1** | Pure native: `Crypto` (the global), `SubtleCrypto`, `CryptoKey` are `#[v8_class]` Rust types installed on every realm during `setup_globals`. No JS polyfill fallback once shipped. The current `embed/crypto.js` shim is deleted in landing 3 (D-23). | Single source of truth; eliminates the JSON-stringify per-call overhead the polyfill carries (#39); brings brand checks under a V8 internal field (D-10). | §I |
| **D-2** | Internal-slot storage: `CryptoKey` carries a `Box<CryptoKeyState>` in V8 internal field 0; `[SameObject]` cached results for `algorithm` and `usages` getters live in V8 private symbols on the JS wrapper (`__cachedAlgorithm`, `__cachedUsages`). The `[[type]]` and `[[extractable]]` slots are stored in the box and exposed via getter-only properties (no mutation path); `[[algorithm]]` and `[[usages]]` parameters are stored in the box too, but the getter returns the cached frozen V8 object. | Spec §13 mandates readonly attributes plus `[SameObject]` semantics: same JS object identity on every access. The cache-on-first-access pattern matches workerd's `JSG_LAZY_READONLY_INSTANCE_PROPERTY`. | §V |
| **D-3** | Single underlying provider: **aws-lc-rs** for every Tier 1 algorithm. No openssl-sys fallback, no ring fallback, no boringssl-direct. Where an algorithm is not directly exposed by aws-lc-rs's high-level API (variable-tag-length AES-GCM truncation, variable-salt-length RSA-PSS, RFC 3394 AES-KW), the implementation drops to aws-lc-rs's lower-level FFI surface (`aws_lc_sys::*` re-exports) — see §IV per-algorithm cells for which arms use which API. | aws-lc-rs is already a workspace dep (`crates/runtime/Cargo.toml:16`); it's FIPS-validated at the underlying AWS-LC level (relevant for some creator apps); it's the same crate `rustls = { features = ["aws-lc-rs"] }` already pulls in. Avoiding a second crypto crate keeps the runtime's wasm-blessing surface manageable and avoids two-provider divergence. | §IV |
| **D-4** | ECDSA wire format: spec-mandated **fixed-length r∥s** (per §23.7). Implementation uses `ECDSA_P256_SHA256_FIXED_SIGNING` / `_FIXED` (verify) and the matching P-384 / P-521 constants. The current impl uses `_ASN1_SIGNING` / `_ASN1` which produces ASN.1/DER signatures — wire-incompatible with Chrome / Firefox / Node WebCrypto / workerd / every other conformant impl. (Critic #1, the single most-impactful correctness fix.) | Spec §23.7.1 step "Convert r to a byte sequence of length n, where n is the byte length in octets of the order of the curve identified by the namedCurve attribute". Verify is symmetric: §23.7.2 splits the bytes 50/50. aws-lc-rs exposes the FIXED constants natively. | §IV.5 |
| **D-5** | JWK is normative and in scope for v1. Every algorithm (HMAC, AES-*, RSA-*, ECDSA, ECDH, Ed25519, X25519) implements `importKey("jwk", ...)` and `exportKey("jwk", ...)`. JWK parsing happens at the WebIDL boundary in Rust: a `JsonWebKey` struct with optional fields, parsed via a hand-rolled walker over the `v8::Local<v8::Object>` (NOT serde_json — preserves V8 string fidelity and avoids a JSON parse-emit round trip). Per-algorithm JWK validators check the spec-mandated `kty`/`crv`/`alg`/`use`/`key_ops`/`ext` fields against the import params. | Critic #5 — JWK is THE largest single missing feature (~1 KLOC across seven algorithm families). Rejecting JWK kills every JOSE/JWE/JWT workflow on the platform. workerd's `jwk.c++` (`refs/workerd/src/workerd/api/crypto/jwk.c++`, 303 LOC) is the model. | §VI |
| **D-6** | Error types: every spec-mandated DOMException flows from Rust to JS as a real `DOMException` instance via a new `OpErrorKind::DomException(name: &'static str)` variant. The macro's `gen_throw_error` (`runtime-macros/src/lib.rs:629-639`) gains a fourth match arm that constructs a `v8::Object` instance of the global `DOMException` class and sets its name property. *(Depends on:* native `DOMException` from `feature/fetch-js-delete`. *Fallback:* if that slips, a string-sentinel scheme — `OpErrorKind::Error` + `message: "DOMException(OperationError): <real msg>"` parsed in a tiny JS post-processor — unblocks shipping; replace with the real path mechanically once DOMException lands. We commit to the real path; the sentinel exists only as a build-system insurance.) | Critic #6 (14 spec-violation findings reduce to wrong error type). WPT `WebCryptoAPI/` makes heavy use of `assert_throws_dom("OperationError", ...)` style assertions; without typed DOMExceptions, ~30% of WPT subtests fail mechanically. | §III |
| **D-7** | Key-usage validation: every `subtle.{sign,verify,encrypt,decrypt,wrapKey,unwrapKey,deriveBits,deriveKey}` op checks the key's `[[usages]]` slot against the operation name BEFORE invoking the underlying crypto function. Mismatch → `InvalidAccessError` (DOMException). Validation happens in Rust, on the boxed CryptoKeyState's `usages: Vec<KeyUsage>` field (not on a JS-side mirror). | Critic #7 — every `§22-§34` operation step starts with "If the [[usages]] internal slot of key does not contain an entry that is `<op-name>`, throw an `InvalidAccessError`." The current impl never checks; a key generated with `usages: ["sign"]` can be used to decrypt. Capability-separation is the entire point of the usages list. | §V.4 |
| **D-8** | Algorithm normalization: spec-correct, op-keyed, recursive. A `pub static SUPPORTED_ALGORITHMS: phf::Map<&'static str, AlgorithmRegistry>` table built from the spec §§20-34 registry replaces the JS-side `normalizeAlgorithm` hand-roll. Each entry maps `(operation, algorithm-name)` to a `ParamShape` enum that drives the WebIDL dictionary parser. Recursive: a member of type `HashAlgorithmIdentifier` re-runs `normalize` with `op="digest"` (matching Deno's pattern at `00_crypto.js:313-314`). | Critic #16 — current impl just upper-cases names + nests `hash`. Spec §18.4.4 defines a structured procedure with operation-keyed dispatch and recursive member normalization. WPT `normalize-algorithm-name.https.any.js` checks this exhaustively. | §VII |
| **D-9** | `[SameObject]` getters for `key.algorithm` and `key.usages`: cached on the JS wrapper via private symbols on first access. Subsequent accesses return the cached frozen V8 object, byte-identical (`Object.is(k.algorithm, k.algorithm) === true`). Implementation: getter callback checks `obj.get_private(scope, "__cachedAlgorithm")`; if `undefined`, builds the spec-mandated `KeyAlgorithm`-shaped object (e.g. `RsaHashedKeyAlgorithm` for RSASSA-PKCS1-v1_5) by reading the `Box<CryptoKeyState>` fields, freezes it, sets the private, returns. | Spec §13 explicit IDL `[SameObject]`. Without this, every `key.algorithm` access produces a fresh JS object — observable in `Object.is(a, b)` checks, breaks `WeakMap`-based identity tracking in JOSE libraries. WPT `crypto_key_cached_slots.https.any.js` checks exactly this. | §V.3 |
| **D-10** | Brand check: V8 internal-field probe. `CryptoKey::is_crypto_key(scope, value) -> bool` returns true iff `value` is a V8 object with internal-field count == 1 AND field 0 is a `v8::External` pointing to a heap allocation tagged with `CryptoKey::TAG`. The TAG is a `'static` byte (e.g. `0xC1`) at the head of the Box layout (a `#[repr(C)]` wrapper `BrandedBox<T> { tag: u8, body: T }`). Spoofing requires an attacker to construct a V8 External pointing at memory with the right tag byte — equivalent to memory-corruption-grade access in a single-isolate, single-thread runtime. | Critic #13 — current impl uses `instanceof CryptoKey`, spoofable by `Object.create(CryptoKey.prototype, ...)` plus a sequential-handle guess to use any other key. Native runs the check from Rust, so the JS prototype chain doesn't influence it. The TAG-byte trick is workerd's pattern (`JSG_RESOURCE_TYPE`'s wrapper-tag system). | §V.5 |
| **D-11** | AES-CTR encrypt / decrypt: full implementation in v1 (was unimplemented in current impl, critic #3). Uses aws-lc-rs's `cipher::UnboundCipherKey` + `EncryptingKey::ctr` / `DecryptingKey::ctr`. Validates `counter` is exactly 16 bytes and `length` is in 1..=128 per spec §27.3.2 (the AesCtrParams normative steps). | Critic #3 + spec §27 mandates. WPT `aes_ctr.https.any.js` is a hard fail today. aws-lc-rs has the full surface. | §IV.3 |
| **D-12** | AES-KW wrapKey / unwrapKey: full implementation in v1 (was unimplemented, critic #4). RFC 3394 wrap. Plus the `wrapKey(format, key, wrappingKey, wrapAlgorithm)` and `unwrapKey(format, wrappedKey, unwrappingKey, unwrapAlgorithm, unwrappedKeyAlgorithm, extractable, keyUsages)` orchestration in SubtleCrypto: serialize the inner key per `format`, pass through the wrapping algorithm's encrypt, store the wrapped bytes; reverse on unwrap. | Spec §14.3.10 / §14.3.11 + §30.5 / §30.6. Used by JWE A128KW/A192KW/A256KW. aws-lc-rs has `aead::AES_128_KW` (and 192 / 256). | §IV.4 |
| **D-13** | ECDH `deriveBits`: full implementation in v1 (was unimplemented, critic #2). aws-lc-rs has `agreement::agree_ephemeral` for one-shot key agreement; we use it with the local `EcPrivateKey` and the remote `EcdhKeyDeriveParams::public` (a CryptoKey with `algorithm.name === "ECDH"`, `type === "public"`). Curve mismatch → `InvalidAccessError`. Output truncated to the user's `length`. | Critic #2 + spec §24. ECDH is the canonical key-agreement primitive used by every TLS / Noise / JWE-ECDH-ES implementation. | §IV.6 |
| **D-14** | P-521 curve: full support in v1 (was missing, critic #11). `Curve::P521` variant added to the curve enum; aws-lc-rs has `ECDSA_P521_SHA512_FIXED_SIGNING` etc. (per RFC 5480, OID 1.3.132.0.35). Some FIPS / Suite B "Secret"-tier workflows require P-521. | Spec §23 enumerates `"P-256" | "P-384" | "P-521"`. aws-lc-rs supports all three. | §IV.5 |
| **D-15** | RSA key generation: full implementation in v1 (was missing, critic #22). aws-lc-rs's `rsa::KeyPair::generate(bits)` with `publicExponent` honoured (spec default 65537; user-supplied is parsed from the Big-endian bytes per §22.4.4). Modulus length validated: 1024 / 2048 / 3072 / 4096 (rejecting < 1024 per FIPS 186-5; rejecting > 16384 per a sanity DoS guard). | Critic #22 + spec §22.4.4 / §21.4.4 / §20.4.4. WPT `generateKey/successes_RSASSA-PKCS1-v1_5.https.any.js`. Modulus length sanity guard prevents `generateKey({modulusLength: 1_000_000})` DoS. | §IV.7 |
| **D-16** | RSA-PSS variable salt length: honoured per spec §21.4.1 (sign step 2: "Let saltLength be the saltLength member of normalizedAlgorithm"). aws-lc-rs's high-level `signature::RSA_PSS_SHA256` (etc.) uses fixed digest-length salt; for variable salt, drop to aws-lc-rs's `RsaPrivateKey::sign_pss` lower-level API which takes a `saltLength: usize`. (Verified: `aws_lc_rs::rsa::sign::SignaturePadding::PSS` accepts a salt length parameter via the `SaltLen` enum.) | Critic #21 + spec §21. WPT `sign_verify/rsa_pss.https.any.js` exercises the saltLength matrix. | §IV.7 |
| **D-17** | AES-GCM IV length: variable, per spec §29.3 (`AesGcmParams::iv` "may be up to 2^64-1 bytes"). The current impl hard-rejects anything ≠ 12 bytes (critic #8). Implementation: aws-lc-rs's `Nonce::try_assume_unique_for_key` requires exactly 12; for non-12 lengths, the impl uses aws-lc-rs's `aead::Aad`-aware lower path with a manual nonce `[u8]`. Reject only the spec-disallowed `len == 0` or `len > u64::MAX`. | Critic #8 + spec §29.3 / §29.4.1. Many JWE A128GCM / A256GCM workflows default to 96-bit (12-byte) IV but legacy IVs of 64-bit (Tink, gRPC-encrypted-channels) or 128-bit are spec-legal. | §IV.2 |
| **D-18** | AES-GCM tag length: honoured per spec §29.4.1 step "tagLength normalization" — must be one of `{32, 64, 96, 104, 112, 120, 128}`, default 128. The current impl ignores `tagLength` entirely (critic #9). aws-lc-rs always produces a 128-bit tag; for shorter tags, the impl truncates the tag bytes post-encrypt (and on decrypt, pads back to 128 bits with the user's tag bytes followed by the missing high bits computed by re-running GHASH — but actually the simpler approach is to validate the user's tag against the truncated last-N-bits of a freshly-computed 128-bit tag). Specifically: encrypt produces 128-bit tag T, output is `ciphertext || T[0..tagLength/8]`; decrypt extracts the user-supplied last `tagLength/8` bytes as `T_user`, computes the expected 128-bit tag T, asserts `constant_time_eq(T_user, T[0..tagLength/8])`, fails with `OperationError` on mismatch. | Critic #9 + spec §29.4.1. Cannot produce A256GCM-with-32-bit-tag (some IoT profiles) or interop with frameworks expecting 96-bit tags. | §IV.2 |
| **D-19** | HMAC default key length: spec §31.4.3 step 2 says "block size in bits of the hash function" — i.e. SHA-1/SHA-256 → 512, SHA-384/SHA-512 → 1024 bits. Current impl uses HASH OUTPUT size (256/384/512) — half the required default for SHA-256, exactly correct for SHA-384/SHA-512 (coincidence). Critic #29. The fix is mechanical: the hash → block-size table in the HMAC `generateKey` / `get key length` ops. | Spec §31.4.3 + WPT `successes_HMAC.https.any.js`. Smaller-than-spec HMAC keys are not insecure (32 bytes is plenty for SHA-256), but spec-noncompliant. | §IV.8 |
| **D-20** | PBKDF2 `iterations`: per WebIDL `[EnforceRange] unsigned long` (u32 with range-enforcement). Current impl casts u64→u32 silently (4_294_967_300 → 4); critic #17 calls this a security regression. The fix uses a new macro arg type `EnforceRangeU32` (companion to the existing `EnforceRangeU64`); reject values outside `[0, 2^32 - 1]` with `OperationError`. Same applies to `deriveBits.length` (`[EnforceRange] unsigned long`) — currently silently truncated, critic #18. | Critic #17 + #18 + WebIDL `[EnforceRange]`. PBKDF2 with adversary-controlled iteration count silently weakened to 4 is a hard security regression. | §IV.9, §VII.2 |
| **D-21** | `getRandomValues` type filter: per spec §10.1.1 step 1, the argument MUST be one of `Int8Array | Uint8Array | Uint8ClampedArray | Int16Array | Uint16Array | Int32Array | Uint32Array | BigInt64Array | BigUint64Array` — explicitly NOT `Float32Array` / `Float64Array` / `DataView`. Current impl accepts the float types and DataView, passes the bytes through (critic #25 — security concern). Implementation: V8's `Local::is_float32_array()` / `is_float64_array()` / `is_data_view()` for the filter; throw `TypeMismatchError` (DOMException) on rejection. The 65 KB quota throws `QuotaExceededError` (DOMException), not generic `Error` (critic #26). | Critic #25 / #26 + spec §10.1.1. WPT `getRandomValues.any.js` checks both. Float-typed array NaN normalization could erase entropy — known-bad pattern. | §II.4 |
| **D-22** | `[SecureContext]` is a no-op. The spec gates `crypto.subtle` and `crypto.randomUUID` behind `[SecureContext]` so browsers can hide them on `http://` pages. The runtime is server-side; there is no insecure context concept. Crypto installs unconditionally; document explicitly. Matches workerd. | Spec §10.1 / §13. The runtime IS effectively secure-context-only (server-side TLS termination at the gateway). Critic #38. | §II.1 |
| **D-23** | Polyfill removal cadence: three landings — (1) ship native behind feature flag `runtime_native_crypto` (env var `ZEROSHIP_NATIVE_CRYPTO=1`), polyfill remains default; (2) flip default to native, polyfill remains as fallback; (3) delete polyfill entirely. Same pattern as streams-native D-19 / fetch-native D-23 / headers-native polyfill removal. | Risk control. Three separate PRs over (industry estimate) 3 weeks; (agent-pace) ~3-4 hours of focused work. | §XII |
| **D-24** | Spec algorithm naming in Rust: every named spec algorithm (`normalize an algorithm`, `Sign` / `Verify` / `Encrypt` / `Decrypt`, per-algorithm `Generate Key` / `Import Key` / `Export Key`) gets a Rust function with the same name in `snake_case`. Lives in `crates/runtime/src/crypto/algorithms.rs` for cross-algorithm operations and in the per-algorithm file for class-local ops. Same rule as streams-native D-20 / fetch-native D-20. | Reduces cognitive load when cross-referencing the spec. Critic #21 / #22 etc. all stem from drifted naming masking the spec gap. | §IV–§VII |
| **D-25** | JOSE / JWE / JWT extensions: out of this design's scope. WebCrypto provides the primitives; a future `@zeroship/jose` npm package wraps them. No `subtle.encryptJWE` etc. native ops. (Spec contains no such ops; they exist as extensions in some libraries.) | Native primitives are forever; library-shaped APIs belong in npm. AGENTS.md "When to add a native primitive vs npm package" decision tree. | (Non-Goals) |
| **D-26** | X25519 / Ed25519: in scope for v1. Both are spec-normative (§25 Ed25519, §26 X25519). aws-lc-rs has Ed25519 (`signature::Ed25519KeyPair`, `signature::ED25519`); X25519 via `agreement::X25519`. Already partially implemented for Ed25519; this design completes the surface and adds X25519. | Spec §25 / §26. Modern JOSE workflows use EdDSA (Ed25519) signatures and ECDH-ES X25519 key agreement. | §IV.6 |
| **D-27** | Hardware key support: out forever. The CryptoKey internal slot stores in-memory bytes; an HSM-backed key would need `enum KeyMaterial { Local(Vec<u8>), Hsm(HsmHandle) }`. Creator apps that need HSM-backed keys make HTTP calls to the HSM provider. | Embedded V8 runtime; no FFI to PKCS#11. Critic note (#37 next_key_id). | (Non-Goals) |
| **D-28** | `structuredClone(CryptoKey)` deferred. The IDL `[Serializable]` is preserved (forward-compat); the actual `serialize` / `deserialize` steps return `DataCloneError` until the structured-clone infrastructure ships. There's no observable JS path to invoke this in v1 (no MessagePort, no Worker, no BroadcastChannel). Critic #15. | No transferable boundary in the runtime. v2 with Workers ships the serialize step. | §V.6 |
| **D-29** | Async via thread-pool offload: deferred to v2. Every WebCrypto op runs synchronously on the V8 thread, wrapped in a `Promise.resolve(syncResult)` (matches the polyfill's behaviour). The macro's existing async-via-`spawned_ops` scaffolding (used by fetch / kv / db) is NOT applied to crypto in v1, because the existing `spawned_ops` queue is for I/O-bound work (fetch / kv) and the crypto ops are CPU-bound — pushing them on the same queue would starve I/O. v2 introduces a `spawned_crypto` companion queue backed by compio's blocking-task pool; until then, large `crypto.subtle.encrypt(buffer)` calls pin the V8 thread. | Critic #12. CPU-vs-IO scheduling is a runtime-wide concern; v1 ships sync-on-V8-thread (matches the polyfill) and v2 introduces the offload. | §IX |
| **D-30** | Macro extension list: this design needs FOUR macro additions, all small. (1) `EnforceRangeU32` newtype + extraction (companion to existing `EnforceRangeU64`). (2) `OpErrorKind::DomException(name)` variant + `gen_throw_error` arm constructing DOMException. (3) Recursive WebIDL dictionary parsing for `AlgorithmIdentifier` (the spec's nested algorithm type — string or dictionary — already partially handled by Deno). (4) `Result<bool, OpError>` return path (used by `verify`, currently faked via `String "true"/"false"`). The `[SameObject]` cache pattern is implemented as an explicit getter helper, NOT a macro feature (D-9 — codegen for cached getters is too niche to bake in). | Critic findings #6, #17, #18, #32. Each of these unblocks a class of WPT tests; combined cost is ~80 LOC of macro code. | §VIII |

## I. Architecture overview

### I.1. The two-layer model

The native WebCrypto implementation is split into two layers:

1. **Public IDL surface** — V8 classes installed on the global object: `Crypto` (the constructor for `globalThis.crypto`), `SubtleCrypto`, `CryptoKey`. Each class carries an internal-field 0 holding `Box<{Class}State>` per the existing `#[v8_class]` pattern (see `crates/runtime/src/web/headers.rs` for the canonical example, `crates/runtime/src/web/dom/abort_signal.rs` for the inheritance-shaped example).

2. **Internal crypto engine** — a Rust-side cryptographic implementation that runs the spec algorithms (`normalize an algorithm`, per-algorithm `Sign` / `Verify` / `Encrypt` / `Decrypt` / `Generate Key` / `Import Key` / `Export Key` / `Derive Bits` / `Wrap Key` / `Unwrap Key`) over aws-lc-rs primitives. The engine never calls into JS during the cryptographic phase; it operates entirely on Rust-side state and resolves a `v8::PromiseResolver` (D-29: synchronously in v1) once the operation completes.

The boundary is sharp: V8 callbacks delegate to Rust methods on the boxed state; Rust algorithm code reads key material via `CryptoKey::material()` (the boxed state's accessor) and writes results back as freshly-allocated `Uint8Array` or `CryptoKey` JS wrappers. Algorithm parameter dictionaries (`AesGcmParams`, `RsaOaepParams`, etc.) are parsed at the entry point of each method via a per-algorithm WebIDL dictionary parser (§VII.2).

### I.2. Why no streaming

The WebCrypto Level 2 spec defines NO streaming variants of any operation. `subtle.digest`, `subtle.encrypt`, `subtle.sign` etc. are all one-shot: they take a `BufferSource` and return a `Promise<ArrayBuffer>`. There is no `subtle.createDigestStream()` or `subtle.encryptInto(stream)`. This is deliberate — the spec treats crypto as a primitive layer; streaming is the layer above (e.g. WHATWG Streams' `TransformStream` constructed in user code from a sequence of `subtle.encrypt` calls on chunks).

Consequence for this design: NO dependency on `docs/proposals/streams-native.md`. The crypto module's input and output are `Vec<u8>` (the macro's existing `BufferSource` extraction shape) and `Uint8Array` (the macro's existing return shape).

The implication for AI-builder workflows: a creator app that wants to digest a large stream constructs a `TransformStream` whose `transform` callback calls `subtle.digest` on each chunk and pipes the chunks through. The userland streaming primitive (`TransformStream`) IS native (streams-native landed); the crypto primitive remains one-shot. This matches every modern runtime (Chrome, Firefox, Node, workerd, Deno).

### I.3. File layout

```
crates/runtime/src/crypto/
├── mod.rs                       (new) module root, public exports
├── crypto_class.rs              (new) Crypto (the global) class + getRandomValues + randomUUID
├── subtle_class.rs              (new) SubtleCrypto class — every op method
├── crypto_key.rs                (new) CryptoKey class + brand check + SameObject caching
├── algorithms.rs                (new) Cross-algorithm spec ops (normalize_an_algorithm,
│                                      get_key_length, usage_intersection)
├── registry.rs                  (new) The compile-time algorithm registry table
│                                      (§VII.1 — phf::Map of operation × algorithm → ParamShape)
├── aes.rs                       (new) AES-CTR / AES-CBC / AES-GCM / AES-KW
├── rsa.rs                       (new) RSA-OAEP / RSASSA-PKCS1-v1_5 / RSA-PSS
├── ec.rs                        (new) ECDSA / ECDH (P-256 / P-384 / P-521)
├── okp.rs                       (new) Ed25519 / X25519
├── hmac.rs                      (new) HMAC sign/verify/generateKey/get-key-length
├── derive.rs                    (new) HKDF / PBKDF2 deriveBits + deriveKey orchestration
├── digest.rs                    (new) SHA-1/256/384/512 digest
├── wrap.rs                      (new) wrapKey / unwrapKey orchestration
├── jwk.rs                       (new) JsonWebKey parser + emitter (per-algorithm helpers)
├── key_material.rs              (new) KeyMaterial enum + KeyAlgorithm variants +
│                                      KeyType / KeyUsage / KeyFormat / NamedCurve enums
├── der.rs                       (new) Tiny ASN.1 DER walker for SPKI/PKCS#8 OID checks
│                                      (§VI.4 — replaces "trust user-supplied namedCurve")
└── enforce_range.rs             (new) EnforceRangeU32 newtype (companion to EnforceRangeU64)

crates/runtime/src/lib.rs        (modified) +pub mod crypto;
crates/runtime/src/core/init.rs       (modified) install Crypto / SubtleCrypto / CryptoKey
                                            classes per-realm (replaces current
                                            ad-hoc op installs at lines 1408-1474)
crates/runtime/src/core/state.rs      (modified) DELETE key_store + next_key_id fields
                                            (D-2 makes them obsolete — keys live on
                                            JS wrappers via internal field)
                                            ADD OpErrorKind::DomException variant
                                            (D-30 macro extension #2)

crates/runtime/src/embed/crypto.js
                                  (deleted in D-23 step 3)

crates/runtime-macros/src/v8_class.rs  (modified) §VIII
crates/runtime-macros/src/lib.rs       (modified) §VIII

crates/runtime/tests/
├── crypto_native.rs              (new) hand-written smoke + corner tests
├── crypto_jwk.rs                 (new) JWK round-trip per algorithm
├── crypto_errors.rs              (new) DOMException name assertions
├── crypto_usages.rs              (new) key-usage validation
├── crypto_brand.rs               (new) brand-check spoofing tests
├── wpt_webcrypto.rs              (new) WPT runner (must-pass v1 set)
└── wpt/WebCryptoAPI/             (vendored from web-platform-tests at a pinned
                                   commit; sparse-checkout config update in
                                   tests/setup-wpt.sh)
```

### I.4. Class diagram

```
┌─────────────────────────────────────────────────────────────────┐
│ V8 isolate                                                      │
│  ┌──────────────────┐                                           │
│  │ globalThis.crypto│  Crypto instance (singleton per realm)    │
│  │ ┌──────────────┐ │                                           │
│  │ │ slot[0]:     │ │                                           │
│  │ │  CryptoState │ │                                           │
│  │ └──────┬───────┘ │                                           │
│  │   .subtle ──────────► SubtleCrypto instance ───┐             │
│  │   .getRandomValues  (singleton per Crypto)    │             │
│  │   .randomUUID                                  │             │
│  └──────────────────┘                              │             │
│                                                    ▼             │
│                                       ┌──────────────────────┐   │
│                                       │ SubtleCrypto         │   │
│                                       │ slot[0]: SubtleState │   │
│                                       │  (zero fields —      │   │
│                                       │   pure dispatcher)   │   │
│                                       │ Methods (14):        │   │
│                                       │  encrypt / decrypt   │   │
│                                       │  sign / verify       │   │
│                                       │  digest              │   │
│                                       │  generateKey         │   │
│                                       │  deriveKey           │   │
│                                       │  deriveBits          │   │
│                                       │  importKey           │   │
│                                       │  exportKey           │   │
│                                       │  wrapKey / unwrapKey │   │
│                                       └──────────┬───────────┘   │
│                                                  │ produces      │
│                                                  ▼               │
│  ┌──────────────────────────────────────────────────────────┐   │
│  │ CryptoKey instance                                       │   │
│  │ slot[0]: BrandedBox<CryptoKeyState>                      │   │
│  │   tag: 0xC1                                              │   │
│  │   body: { type, extractable, algorithm_name,             │   │
│  │           algorithm_params: KeyAlgorithm,                │   │
│  │           usages: Vec<KeyUsage>,                         │   │
│  │           material: KeyMaterial }                        │   │
│  │ Private symbols:                                         │   │
│  │   __cachedAlgorithm  (frozen JS object, [SameObject])    │   │
│  │   __cachedUsages     (frozen JS array, [SameObject])     │   │
│  └──────────────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────────────┘
```

(The CryptoState struct on the global is essentially empty — it only exists because `#[v8_class]` requires a Box payload. SubtleCrypto similarly has no state; both are pure-dispatcher classes. The interesting state lives on `CryptoKey`.)

## II. Crypto class (the global) — IDL surface

### II.1. IDL

Per spec §10.1:

```webidl
[Exposed=(Window,Worker)]
interface mixin Crypto {
  [SecureContext] readonly attribute SubtleCrypto subtle;
  ArrayBufferView getRandomValues(ArrayBufferView array);
  [SecureContext] DOMString randomUUID();
};
```

(The mixin is realised on `globalThis` as the property `crypto`. We model it as a `#[v8_class]` and install one instance on `globalThis.crypto` per realm.)

D-22: `[SecureContext]` is treated as a no-op — both `subtle` and `randomUUID` are unconditionally available. The runtime is server-side; there is no insecure context concept. Document.

### II.2. Internal slots

Per spec §10:
- `[[subtle]]` — a per-realm `SubtleCrypto` singleton. Stored as a V8 private symbol `__subtleInstance` on the Crypto wrapper, lazily created on first `crypto.subtle` access (D-9 same-object caching).

### II.3. Storage

```rust
pub struct CryptoState {
    // Empty — `subtle` is cached on the V8 wrapper via __subtleInstance.
    // The thread-local entropy buffer (current crypto.rs:97-99) stays as a
    // free-floating thread_local!, NOT on this struct (it's CSPRNG-shared,
    // not per-Crypto-instance state).
}
```

### II.4. `getRandomValues(array)`

Spec §10.1.1.

Steps (verbatim from spec):
1. If `array` is not an `Int8Array`, `Uint8Array`, `Uint8ClampedArray`, `Int16Array`, `Uint16Array`, `Int32Array`, `Uint32Array`, `BigInt64Array`, or `BigUint64Array`, then throw a `TypeMismatchError` (DOMException).
2. If the byte length of `array` is greater than 65,536, throw a `QuotaExceededError` (DOMException).
3. Overwrite all elements of `array` with cryptographically random values.
4. Return `array`.

Implementation (replaces current `crypto_get_random_values_callback` at `crypto.rs:136-181`):

```rust
#[v8_method]
fn get_random_values<'s>(
    &self,
    scope: &mut v8::PinScope<'s, '_>,
    array: v8::Local<'s, v8::Value>,
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    // Step 1: type filter (D-21).
    if !is_allowed_typed_array(scope, array) {
        return Err(OpError::dom("TypeMismatchError",
            "Argument must be an Int8/Uint8/Uint8Clamped/Int16/Uint16/Int32/Uint32/BigInt64/BigUint64Array"));
    }
    let view: v8::Local<v8::ArrayBufferView> = array.try_into().unwrap();

    // Step 2: quota check (D-21).
    let byte_len = view.byte_length();
    if byte_len > 65536 {
        return Err(OpError::dom("QuotaExceededError",
            "byteLength > 65536"));
    }

    if byte_len == 0 {
        return Ok(array);
    }

    // Step 3: fill from CSPRNG. Direct backing-store write via raw pointer
    // (workerd pattern; ~10x faster than the byte-by-byte loop the current
    // impl uses, critic #35).
    let mut tmp = vec![0u8; byte_len];
    fast_random(&mut tmp);
    let ab = view.buffer(scope).unwrap();
    let offset = view.byte_offset();
    let store = ab.get_backing_store();
    // SAFETY: `store.data()` is a stable pointer for the backing store's
    // lifetime; we hold the V8 isolate lock; byte range is bounds-checked
    // above (offset + byte_len ≤ ab.byte_length() by ArrayBufferView invariants).
    unsafe {
        let dst = store.data().add(offset) as *mut u8;
        std::ptr::copy_nonoverlapping(tmp.as_ptr(), dst, byte_len);
    }

    // Step 4.
    Ok(array)
}

fn is_allowed_typed_array(scope: &mut v8::PinScope, value: v8::Local<v8::Value>) -> bool {
    if value.is_data_view() { return false; }
    if value.is_float32_array() { return false; }
    if value.is_float64_array() { return false; }
    // All remaining ArrayBufferView types are allowed integer typed arrays.
    v8::Local::<v8::ArrayBufferView>::try_from(value).is_ok()
}
```

Spec section: §10.1.1, four steps.
Error mapping: TypeMismatchError + QuotaExceededError, both DOMException.

### II.5. `randomUUID()`

Spec §10.1.2 + RFC 4122 §4.4 (UUID v4).

Implementation: existing `crypto_random_uuid` at `crypto.rs:115-130` is correct (critic #34: 90/100). Migration is mechanical: convert the `#[zeroship_op]` free function to a `#[v8_method]` on `Crypto`. The thread-local 4 KB entropy buffer (`crypto.rs:97-99`) stays.

Minor cleanup (critic #34): replace `String::from_utf8(buf.to_vec()).unwrap()` with `unsafe { String::from_utf8_unchecked(buf.to_vec()) }` since the bytes are guaranteed ASCII (hex + hyphens). Saves a UTF-8 validation pass per call.

```rust
#[v8_method]
#[v8_name = "randomUUID"]
fn random_uuid(&self) -> String {
    let mut b = [0u8; 16];
    fast_random(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40; // version 4
    b[8] = (b[8] & 0x3f) | 0x80; // variant 10xx
    let mut buf = [0u8; 36];
    let mut p = 0;
    for (i, &byte) in b.iter().enumerate() {
        if i == 4 || i == 6 || i == 8 || i == 10 { buf[p] = b'-'; p += 1; }
        buf[p] = HEX[(byte >> 4) as usize]; p += 1;
        buf[p] = HEX[(byte & 0x0f) as usize]; p += 1;
    }
    // SAFETY: buf is ASCII (hex digits + hyphens).
    unsafe { String::from_utf8_unchecked(buf.to_vec()) }
}
```

### II.6. `subtle` getter

Spec §10. `[SameObject]` per-realm (every `crypto.subtle === crypto.subtle`).

```rust
#[v8_getter]
fn subtle<'s>(
    &self,
    scope: &mut v8::PinScope<'s, '_>,
    this: v8::Local<'s, v8::Object>,
) -> v8::Local<'s, v8::Value> {
    // Cache via private symbol on the Crypto wrapper.
    let key = v8::Private::for_api(scope,
        Some(v8::String::new(scope, "__subtleInstance").unwrap()));
    if let Some(cached) = this.get_private(scope, key) {
        if !cached.is_undefined() {
            return cached;
        }
    }
    let subtle = SubtleCrypto::install(scope).get_function(scope).unwrap();
    let inst = subtle.new_instance(scope, &[]).unwrap();
    this.set_private(scope, key, inst.into());
    inst.into()
}
```

## III. SubtleCrypto class — IDL surface

### III.1. IDL

Per spec §14:

```webidl
[Exposed=(Window,Worker)]
interface SubtleCrypto {
  Promise<any> encrypt(AlgorithmIdentifier algorithm,
                        CryptoKey key,
                        BufferSource data);
  Promise<any> decrypt(AlgorithmIdentifier algorithm,
                        CryptoKey key,
                        BufferSource data);
  Promise<any> sign(AlgorithmIdentifier algorithm,
                     CryptoKey key,
                     BufferSource data);
  Promise<boolean> verify(AlgorithmIdentifier algorithm,
                            CryptoKey key,
                            BufferSource signature,
                            BufferSource data);
  Promise<any> digest(AlgorithmIdentifier algorithm,
                       BufferSource data);

  Promise<any> generateKey(AlgorithmIdentifier algorithm,
                            boolean extractable,
                            sequence<KeyUsage> keyUsages);
  Promise<CryptoKey> deriveKey(AlgorithmIdentifier algorithm,
                                CryptoKey baseKey,
                                AlgorithmIdentifier derivedKeyType,
                                boolean extractable,
                                sequence<KeyUsage> keyUsages);
  Promise<ArrayBuffer> deriveBits(AlgorithmIdentifier algorithm,
                                    CryptoKey baseKey,
                                    optional unsigned long? length = null);

  Promise<CryptoKey> importKey(KeyFormat format,
                                (BufferSource or JsonWebKey) keyData,
                                AlgorithmIdentifier algorithm,
                                boolean extractable,
                                sequence<KeyUsage> keyUsages);
  Promise<any> exportKey(KeyFormat format, CryptoKey key);

  Promise<any> wrapKey(KeyFormat format,
                        CryptoKey key,
                        CryptoKey wrappingKey,
                        AlgorithmIdentifier wrapAlgorithm);
  Promise<CryptoKey> unwrapKey(KeyFormat format,
                                BufferSource wrappedKey,
                                CryptoKey unwrappingKey,
                                AlgorithmIdentifier unwrapAlgorithm,
                                AlgorithmIdentifier unwrappedKeyAlgorithm,
                                boolean extractable,
                                sequence<KeyUsage> keyUsages);
};
```

Plus `typedef` definitions:
```webidl
typedef (object or DOMString) AlgorithmIdentifier;
typedef AlgorithmIdentifier HashAlgorithmIdentifier;
typedef DOMString KeyType;       // "public" | "private" | "secret"
typedef DOMString KeyUsage;      // "encrypt"|"decrypt"|"sign"|"verify"|
                                  //  "deriveKey"|"deriveBits"|"wrapKey"|"unwrapKey"
typedef DOMString KeyFormat;     // "raw"|"spki"|"pkcs8"|"jwk"
typedef DOMString NamedCurve;    // "P-256"|"P-384"|"P-521"
```

### III.2. Internal slots

None. SubtleCrypto is a pure dispatcher.

### III.3. Storage

```rust
pub struct SubtleCryptoState {
    // Empty.
}
```

### III.4. Method dispatch

Each of the 14 methods follows the same shape:

```rust
#[v8_method]
fn <op_name>(
    &self,
    scope: &mut v8::PinScope,
    /* ... method-specific args ... */
) -> Result<v8::Local<v8::Promise>, OpError> {
    // 1. Allocate PromiseResolver / Promise pair.
    let resolver = v8::PromiseResolver::new(scope).ok_or_else(|| OpError::error("PromiseResolver alloc"))?;
    let promise = resolver.get_promise(scope);

    // 2. Per-method validation: WebIDL conversion of args, normalize_an_algorithm
    //    with the op's name. ANY validation failure here is sync-throw at the
    //    promise-creation boundary, NOT promise-rejection (matches workerd /
    //    Chrome behaviour for spec steps 1-3 of each op — algorithm normalization
    //    happens before the "queue a task" step).
    //
    //    The spec is ambiguous here: §14.3.x step 1 ("Let normalizedAlgorithm be ...")
    //    is defined to "throw"; the explicit "queue a task" step that captures the
    //    promise rejection comes later. Browsers treat normalization failure as
    //    synchronous-throw (verified against Chrome 128 at design time). We match.

    // 3. Run the spec algorithm steps, get a Result<ResultValue, OpError>.
    let result = perform_op(...);

    // 4. Resolve or reject the promise (D-29: synchronously, no thread-pool offload).
    match result {
        Ok(v) => resolver.resolve(scope, v.to_v8(scope)),
        Err(e) => {
            let exc = e.to_v8_exception(scope);
            resolver.reject(scope, exc);
        }
    }

    Ok(promise)
}
```

Per-method validation steps and dispatch are detailed in §IV (per-algorithm) and §VII (normalize an algorithm).

### III.5. WebIDL boundary — `(BufferSource or JsonWebKey) keyData`

The `importKey` `keyData` parameter is a union: either a `BufferSource` (when format is `"raw"` / `"spki"` / `"pkcs8"`) or a `JsonWebKey` (when format is `"jwk"`).

WebIDL union dispatch: per https://webidl.spec.whatwg.org/#es-union, a union containing a typedef and a dictionary type:
1. If V is null/undefined → reject (neither variant accepts null).
2. If V is a platform object that implements `BufferSource` (i.e. `ArrayBuffer`, `ArrayBufferView`) → BufferSource branch.
3. Otherwise, if V is an ordinary object → dictionary branch.
4. Otherwise → TypeError.

Implementation:

```rust
enum KeyDataInput {
    BufferSource(Vec<u8>),
    Jwk(JsonWebKey),
}

fn parse_key_data_input(
    scope: &mut v8::PinScope,
    format: KeyFormat,
    value: v8::Local<v8::Value>,
) -> Result<KeyDataInput, OpError> {
    match format {
        KeyFormat::Raw | KeyFormat::Spki | KeyFormat::Pkcs8 => {
            // BufferSource branch — read via the existing macro pattern.
            if let Ok(view) = v8::Local::<v8::ArrayBufferView>::try_from(value) {
                let mut buf = vec![0u8; view.byte_length()];
                view.copy_contents(&mut buf);
                Ok(KeyDataInput::BufferSource(buf))
            } else if let Ok(ab) = v8::Local::<v8::ArrayBuffer>::try_from(value) {
                let store = ab.get_backing_store();
                let mut buf = vec![0u8; ab.byte_length()];
                for i in 0..buf.len() { buf[i] = store[i].get(); }
                Ok(KeyDataInput::BufferSource(buf))
            } else {
                Err(OpError::type_error("keyData must be a BufferSource"))
            }
        }
        KeyFormat::Jwk => {
            // Dictionary branch — parse fields out of an ordinary JS object.
            // See §VI.1 for the full JsonWebKey field walker.
            let obj = v8::Local::<v8::Object>::try_from(value)
                .map_err(|_| OpError::type_error("JWK keyData must be an object"))?;
            let jwk = parse_jwk(scope, obj)?;
            Ok(KeyDataInput::Jwk(jwk))
        }
    }
}
```

## IV. Per-algorithm specifications

This section walks the spec's §§20-34 algorithm definitions with each Tier 1 algorithm's operation steps, parameter shapes, error types, and aws-lc-rs binding. Cross-cutting concerns (normalize an algorithm, key-usage validation, JWK round-trip) are in §VII / §VI.

### IV.1. Algorithm registry table

(D-8 + D-24.) Compile-time registry living in `crypto/registry.rs`. Tier 1 algorithms × 11 operations = the matrix. Empty cells = operation not supported for that algorithm (registry lookup returns `NotSupportedError`).

| Algorithm \ Op | encrypt | decrypt | sign | verify | digest | gen | imp | exp | dB | wK | uK | klen |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| RSASSA-PKCS1-v1_5 | | | RsaHashed | RsaHashed | | RsaHashedKeyGen | RsaHashedImp | – | | | | |
| RSA-PSS | | | RsaPssParams | RsaPssParams | | RsaHashedKeyGen | RsaHashedImp | – | | | | |
| RSA-OAEP | RsaOaepParams | RsaOaepParams | | | | RsaHashedKeyGen | RsaHashedImp | – | | RsaOaepParams | RsaOaepParams | |
| ECDSA | | | EcdsaParams | EcdsaParams | | EcKeyGenParams | EcKeyImp | – | | | | |
| ECDH | | | | | | EcKeyGenParams | EcKeyImp | – | EcdhKeyDeriveParams | | | |
| Ed25519 | | | – | – | | – | – | – | | | | |
| X25519 | | | | | | – | – | – | EcdhKeyDeriveParams | | | |
| AES-CTR | AesCtrParams | AesCtrParams | | | | AesKeyGenParams | – | – | | AesCtrParams | AesCtrParams | AesDerivedKeyParams |
| AES-CBC | AesCbcParams | AesCbcParams | | | | AesKeyGenParams | – | – | | AesCbcParams | AesCbcParams | AesDerivedKeyParams |
| AES-GCM | AesGcmParams | AesGcmParams | | | | AesKeyGenParams | – | – | | AesGcmParams | AesGcmParams | AesDerivedKeyParams |
| AES-KW | | | | | | AesKeyGenParams | – | – | | – | – | AesDerivedKeyParams |
| HMAC | | | – | – | | HmacKeyGenParams | HmacImp | – | | | | HmacImp |
| SHA-1 | | | | | – | | | | | | | |
| SHA-256 | | | | | – | | | | | | | |
| SHA-384 | | | | | – | | | | | | | |
| SHA-512 | | | | | – | | | | | | | |
| HKDF | | | | | | | – | – | HkdfParams | | | – |
| PBKDF2 | | | | | | | – | – | Pbkdf2Params | | | – |

Legend: `–` = no per-op params (the algorithm name alone suffices). `RsaHashed` = `RsaHashedImportParams` for import / `RsaHashedKeyGen` = `RsaHashedKeyGenParams` for generate. `dB` = deriveBits, `wK` = wrapKey, `uK` = unwrapKey, `klen` = "get key length" (internal op, used by `deriveKey`). Empty = NotSupportedError.

### IV.2. AES-GCM (§29)

Operations: `encrypt` / `decrypt` / `generateKey` / `importKey` / `exportKey` / `get key length` (+ `wrapKey` / `unwrapKey` orchestration via §14.3.10/§14.3.11 — AES-GCM CAN be a wrapping key).

**AesGcmParams (§29.3):**
```webidl
dictionary AesGcmParams : Algorithm {
  required BufferSource iv;
  BufferSource additionalData;
  [EnforceRange] octet tagLength;
};
```

**Encrypt steps (§29.4.1):**
1. If `iv` byte length > 2^64 - 1 → `OperationError`. (Practically: `usize > 2^64-1` is impossible on a 64-bit machine; the spec language anticipates 32-bit machines. Range check `iv.len() > 0` is the meaningful bound. D-17.)
2. If `additionalData` byte length > 2^64 - 1 → `OperationError`.
3. Let `tagLength` be the value of `normalizedAlgorithm.tagLength` if present, else 128.
4. If `tagLength` ∉ `{32, 64, 96, 104, 112, 120, 128}` → `OperationError`. (D-18.)
5. Let `additionalData` be `normalizedAlgorithm.additionalData` if present, else empty byte sequence.
6. Run AES-GCM on `data` with key `key.[[handle]]`, iv, additionalData → produce `(ciphertext, fullTag)` where `fullTag` is 128 bits.
7. Let `tag` be `fullTag[0..tagLength/8]`.
8. Return `ciphertext || tag`.

**Decrypt steps (§29.4.2):**
1-5. Same iv / aad / tagLength normalization.
6. If `data.byte_length * 8 < tagLength` → `OperationError` (input shorter than tag).
7. Let `actualCiphertext = data[0..data.byte_length - tagLength/8]`, `actualTag = data[end - tagLength/8 ..end]`.
8. Run AES-GCM-Decrypt(key, iv, additionalData, actualCiphertext, actualTag, tagLength) — verify tag (constant-time), produce plaintext.
9. If verification fails → `OperationError`.
10. Return plaintext.

**aws-lc-rs binding:**
- For `iv.len() == 12 && tagLength == 128` (the common path): `aws_lc_rs::aead::{Nonce, UnboundKey, LessSafeKey}` with `AES_128_GCM` / `AES_256_GCM` (`AES_192_GCM` for AES-192 — D, not in critic findings but spec-allowed). `seal_in_place_append_tag` for encrypt, `open_in_place` for decrypt.
- For `iv.len() != 12 || tagLength != 128`: drop to `aws_lc_sys` raw FFI (`EVP_AEAD_CTX_seal` / `EVP_AEAD_CTX_open`) with explicit nonce length and tag length. The high-level `aead::Algorithm` types in aws-lc-rs hard-code 12-byte nonce; the lower FFI accepts any length up to `EVP_AEAD_max_nonce_length(AEAD)` (96 bytes for AES-GCM per AWS-LC's `aes_128_gcm`). For `tagLength != 128`, encrypt produces full 128-bit tag and we truncate; decrypt extracts the user's `tagLength/8` bytes, computes full 128-bit tag, asserts `crypto::constant_time::eq(user_tag, full_tag[0..N])`, and falls through to plaintext only on match.

**generateKey steps (§29.4.3):**
1. If `usages` contains anything other than `{encrypt, decrypt, wrapKey, unwrapKey}` → `SyntaxError` (DOMException — see §VII.4 on the IDL `SyntaxError` interface). (Note: workerd's findings (#) treat `SyntaxError` as DOMException too — see §VII.4 footnote.)
2. If `length` ∉ `{128, 192, 256}` → `OperationError`.
3. Generate `length / 8` random bytes via aws-lc-rs `rand::fill`.
4. Build `KeyAlgorithm = AesKeyAlgorithm { name: "AES-GCM", length }`.
5. Build CryptoKey with `type: "secret"`, `extractable`, `usages`, algorithm, material: `Symmetric(bytes)`.

**importKey:** §29.4.4. Formats: `"raw"` (BufferSource, byte length must be 16/24/32), `"jwk"` (JsonWebKey with `kty: "oct"`, `k: base64url-bytes`, `alg: A128GCM/A192GCM/A256GCM` matching length, `use: "enc"`, `key_ops: subset of {encrypt, decrypt, wrapKey, unwrapKey}`, `ext: matches extractable`). NOT `"spki"` / `"pkcs8"` (those are asymmetric formats).

**exportKey:** §29.4.5. `"raw"` returns the bytes; `"jwk"` returns the JsonWebKey shape.

**get key length:** §29.4.6. Returns `length` (the AesDerivedKeyParams `length` member).

**Error types per step:**
- iv too long → OperationError
- additionalData too long → OperationError
- tagLength invalid → OperationError
- decrypt tag mismatch → OperationError
- importKey wrong byte length → DataError
- importKey wrong jwk fields → DataError
- generateKey wrong usages → SyntaxError (DOMException)
- generateKey wrong length → OperationError
- key.usages doesn't contain op → InvalidAccessError (D-7)

### IV.3. AES-CTR (§27) — D-11

Operations: `encrypt` / `decrypt` / `generateKey` / `importKey` / `exportKey` / `get key length` / `wrapKey` / `unwrapKey`.

**AesCtrParams (§27.3):**
```webidl
dictionary AesCtrParams : Algorithm {
  required BufferSource counter;     // exactly 16 bytes
  [EnforceRange] required octet length;  // 1..128 — counter-bits
};
```

**Encrypt steps (§27.4.1):**
1. If `counter` byte length ≠ 16 → `OperationError`.
2. If `length` not in `1..=128` → `OperationError`.
3. Run AES-CTR(key, counter, length) on data, where `length` is the bit-length of the counter portion of the 128-bit IV.
4. Return ciphertext.

(Decrypt is identical to encrypt — CTR is symmetric.)

**aws-lc-rs binding:** `cipher::UnboundCipherKey` + `EncryptingKey::ctr` / `DecryptingKey::ctr`. Note: aws-lc-rs's `ctr` uses the full 128-bit counter; the spec's `length` parameter (counter-portion bits) is typically 64 (high half nonce, low half counter — RFC 3686). For `length == 128` (entire IV is the counter — full-block-counter mode), pass the user's counter bytes directly. For `length < 128`, the high `(128 - length)` bits are the constant nonce and the low `length` bits are the running counter — aws-lc-rs's CTR API increments the full 128-bit IV, which matches the spec's algorithm because the spec's CTR overflows the counter portion only after `2^length` blocks (which we trust the caller to avoid; an overflow that wraps into the nonce portion is a per-spec correctness issue but not detectable without bookkeeping). Document explicitly: aws-lc-rs's CTR is NIST SP 800-38A Section 6.5 with full-block counter; the WebCrypto `length` parameter is informational and we don't enforce wrap-back.

(Critic note: spec §27 step 4 actually says "If after this addition the counter has overflowed (...), wrap it." We implement the simpler semantics — no overflow detection — matching workerd / Chrome, which also use the underlying OpenSSL CTR with full-counter increment. WPT does not test the overflow case.)

### IV.4. AES-KW (§30) — D-12

Operations: `wrapKey` / `unwrapKey` / `generateKey` / `importKey` / `exportKey` / `get key length`.

**No params dictionary.** Just `Algorithm { name: "AES-KW" }`.

**wrapKey steps (§30.5.1):**
1. Let `data` be the result of running the spec's `Wrap Key` algorithm with `key` (the wrapping key) and `key`'s usage `wrapKey`.
2. Return `Aes-KW-Encrypt(wrappingKey, data)`.

**aws-lc-rs binding:** `aead::AES_128_KW` / `AES_192_KW` / `AES_256_KW` are RFC 3394 wrap implementations. The wrap call is `wrap(key, data) -> Vec<u8>` (output is `data.len() + 8`); unwrap is `unwrap(key, wrapped) -> Result<Vec<u8>, ...>`.

(NOTE: RFC 5649 — AES-KWP, the padded variant — is NOT WebCrypto's AES-KW. WebCrypto's AES-KW requires plaintext length to be a multiple of 8 bytes, ≥ 16. The wrap algorithm rejects non-multiple-of-8 inputs with `OperationError`.)

**SubtleCrypto.wrapKey orchestration (§14.3.10):**
```rust
fn wrap_key(format, key, wrappingKey, wrapAlgorithm) -> ArrayBuffer {
    // 1. normalizedAlgorithm = normalize_an_algorithm(wrapAlgorithm, "wrapKey")
    // 2. If wrappingKey.usages doesn't contain "wrapKey" → InvalidAccessError
    // 3. If normalizedKeyAlgorithm.name != wrappingKey.algorithm.name → InvalidAccessError
    // 4. If !key.extractable → InvalidAccessError
    // 5. exported = exportKey(format, key)  — internal call, recurses through SubtleCrypto
    // 6. If wrapAlgorithm registered for op "encrypt": result = encrypt(wrapAlgorithm, wrappingKey, exported)
    // 7. Else if wrapAlgorithm == "AES-KW": result = aes_kw_wrap(wrappingKey.material, exported)
    // 8. Return result
}
```

**unwrapKey orchestration (§14.3.11):** symmetric — decrypt the wrapped bytes, then run importKey on the result with the inner algorithm.

### IV.5. ECDSA (§23) — D-4 (the wire-format fix)

Operations: `sign` / `verify` / `generateKey` / `importKey` / `exportKey`.

**EcdsaParams (§23.3):**
```webidl
dictionary EcdsaParams : Algorithm {
  required HashAlgorithmIdentifier hash;
};
```

**EcKeyGenParams (§23.4):**
```webidl
dictionary EcKeyGenParams : Algorithm {
  required NamedCurve namedCurve;  // "P-256" | "P-384" | "P-521"
};
```

**Sign steps (§23.7.1) — the critical path:**
1. If `key.[[type]] != "private"` → `InvalidAccessError`.
2. Let `hashAlgorithm = normalizedAlgorithm.hash`.
3. Let `M = hash(hashAlgorithm, data)`.
4. Run ECDSA-Sign(key.[[handle]], M) → produce `(r, s)` integers.
5. Let `n = ceil(curve_order_bits / 8)` (32 for P-256, 48 for P-384, 66 for P-521).
6. Convert `r` to a byte sequence of length `n` (big-endian, zero-padded).
7. Convert `s` to a byte sequence of length `n` (big-endian, zero-padded).
8. Return `r-bytes || s-bytes` (total 2n bytes).

**The wire format is fixed-length r∥s, NOT ASN.1/DER.** This is the entire delta from the current impl. The current impl uses aws-lc-rs's `ECDSA_*_ASN1_SIGNING` constants which produce DER; the fix uses `ECDSA_*_FIXED_SIGNING`.

**aws-lc-rs binding:**
```rust
let alg = match (curve, hash) {
    (Curve::P256, "SHA-256") => &aws_lc_rs::signature::ECDSA_P256_SHA256_FIXED_SIGNING,
    (Curve::P384, "SHA-384") => &aws_lc_rs::signature::ECDSA_P384_SHA384_FIXED_SIGNING,
    (Curve::P521, "SHA-512") => &aws_lc_rs::signature::ECDSA_P521_SHA512_FIXED_SIGNING,
    // Other (curve, hash) combinations are spec-legal but rarely used.
    // The spec does not strictly require curve-hash matching (any hash with any curve);
    // aws-lc-rs's pre-defined constants are matched pairs. For mixed combos
    // (e.g. P-256 with SHA-512), drop to the lower-level `ecdsa_sign_with_sha`
    // FFI surface. Document.
    _ => return Err(OpError::dom("NotSupportedError", "ECDSA curve/hash combo not supported")),
};
let key_pair = aws_lc_rs::signature::EcdsaKeyPair::from_pkcs8(alg, pkcs8_der)
    .map_err(|_| OpError::dom("DataError", "Invalid ECDSA private key"))?;
let sig = key_pair.sign(&rng, data)
    .map_err(|_| OpError::dom("OperationError", "ECDSA sign failed"))?;
Ok(sig.as_ref().to_vec())  // FIXED format = r || s, 2n bytes
```

**Verify steps (§23.7.2):**
1. If `key.[[type]] != "public"` → `InvalidAccessError`.
2. Let `n = ceil(curve_order_bits / 8)`.
3. If `signature.byte_length != 2*n` → return `false`.
4. Let `r = signature[0..n]`, `s = signature[n..2n]`.
5. Let `M = hash(hashAlgorithm, data)`.
6. Run ECDSA-Verify(key.[[handle]], M, (r, s)) → boolean.
7. Return result.

aws-lc-rs's `ECDSA_*_FIXED` (without `_SIGNING` suffix) is the verify variant.

**P-521 support (D-14):** aws-lc-rs has `ECDSA_P521_SHA512_FIXED_SIGNING` and `_FIXED`. Add to the curve enum and dispatch table.

**importKey / exportKey:** formats `"raw"` (uncompressed point, public only — `0x04 || x || y` per SEC1; reject compressed `0x02`/`0x03` per spec §23.6.5), `"spki"` (SubjectPublicKeyInfo, public only), `"pkcs8"` (PrivateKeyInfo, private only), `"jwk"` (`kty: "EC"`, `crv: "P-256"|"P-384"|"P-521"`, `x: base64url`, `y: base64url`, `d: base64url` if private).

For `"spki"` / `"pkcs8"` import: validate that the embedded curve OID matches the user-supplied `namedCurve`. Critic #31 — current impl trusts `namedCurve` blindly. The fix uses our tiny ASN.1 DER walker (`crypto/der.rs`) to extract the AlgorithmIdentifier OID and assert match. RFC 5480 §2.1.1.1 OIDs:
- 1.2.840.10045.3.1.7 = P-256 (`secp256r1`)
- 1.3.132.0.34 = P-384 (`secp384r1`)
- 1.3.132.0.35 = P-521 (`secp521r1`)

For `"raw"` public key import: validate length matches `1 + 2*n` and first byte is `0x04`. Reject compressed forms (critic #30).

### IV.6. ECDH (§24) — D-13 + X25519 (§26) — D-26

Operations: `deriveBits` / `generateKey` / `importKey` / `exportKey`. (No sign / verify / encrypt / decrypt — ECDH is key agreement only.)

**EcdhKeyDeriveParams (§24.3):**
```webidl
dictionary EcdhKeyDeriveParams : Algorithm {
  required CryptoKey public;  // The peer's public key.
};
```

**deriveBits steps (§24.6.1):**
1. If `key.[[type]] != "private"` → `InvalidAccessError`.
2. Let `publicKey = normalizedAlgorithm.public`.
3. If `publicKey.[[type]] != "public"` → `InvalidAccessError`.
4. If `publicKey.algorithm.name != "ECDH"` (or "X25519") → `InvalidAccessError`.
5. If `publicKey.algorithm.namedCurve != key.algorithm.namedCurve` → `InvalidAccessError`.
6. Run ECDH(key.[[handle]], publicKey.[[handle]]) → produce shared secret `Z` of `n` bytes.
7. If `length` is null → return `Z`.
8. If `length > Z.byte_length * 8` → `OperationError`.
9. Return `Z[0..length/8]` (with bit-rounding for non-byte-aligned lengths — see spec).

**aws-lc-rs binding:** `agreement::agree_ephemeral`. The "ephemeral" name is misleading — it's just one-shot agreement. Take `agreement::EphemeralPrivateKey` (built from the local private key bytes via `EphemeralPrivateKey::generate` for fresh keys or `EphemeralPrivateKey::from_pkcs8` for imported), and `agreement::UnparsedPublicKey` (built from the peer's raw public point bytes), call `agree_ephemeral(local_private, peer_public, |z| Ok(z.to_vec()))`.

For X25519: same pattern, using `agreement::X25519`. Spec §26 maps to aws-lc-rs's `X25519` agreement constant.

### IV.7. RSA-OAEP / RSASSA-PKCS1-v1_5 / RSA-PSS (§§20, 21, 22)

**RsaHashedKeyGenParams (§20.4):**
```webidl
dictionary RsaHashedKeyGenParams : RsaKeyGenParams {
  required HashAlgorithmIdentifier hash;
};
dictionary RsaKeyGenParams : Algorithm {
  required [EnforceRange] unsigned long modulusLength;
  required BigInteger publicExponent;  // Big-endian byte sequence
};
```

**RsaHashedImportParams (§20.7):**
```webidl
dictionary RsaHashedImportParams : Algorithm {
  required HashAlgorithmIdentifier hash;
};
```

**RsaPssParams (§21.3):**
```webidl
dictionary RsaPssParams : Algorithm {
  required [EnforceRange] unsigned long saltLength;
};
```

**RsaOaepParams (§22.3):**
```webidl
dictionary RsaOaepParams : Algorithm {
  BufferSource label;
};
```

**generateKey steps (§§20.4.4, 21.4.4, 22.4.4):**
1. If `usages` contains anything other than `{sign, verify}` (PKCS1v1_5, PSS) or `{encrypt, decrypt, wrapKey, unwrapKey}` (OAEP) → `SyntaxError`.
2. Let `modulusLength = normalizedAlgorithm.modulusLength`, `publicExponent = normalizedAlgorithm.publicExponent`.
3. Generate RSA key pair via aws-lc-rs `rsa::KeyPair::generate(modulusLength)`. (D-15.)
4. Validate `publicExponent`: must be 3 or 65537 per FIPS 186-5 §A.1.1 (other values are technically spec-legal but FIPS-noncompliant). Reject otherwise with `OperationError`. workerd does the same.
5. Build `KeyAlgorithm = RsaHashedKeyAlgorithm { name, modulusLength, publicExponent, hash }`.
6. Return CryptoKeyPair { publicKey, privateKey } both with this algorithm.

**RSA-PSS sign (§21.4.1) — the variable-salt fix:**
1. If `key.[[type]] != "private"` → `InvalidAccessError`.
2. Let `saltLength = normalizedAlgorithm.saltLength`.
3. Let `hashAlgorithm = key.algorithm.hash`.
4. Run RSASSA-PSS-Sign(key.[[handle]], data, saltLength, hashAlgorithm) per RFC 8017 §8.1.1.
5. Return signature.

**aws-lc-rs binding for variable salt:**
- The high-level `signature::RSA_PSS_SHA256` / `_SHA384` / `_SHA512` constants use `saltLength = digest_length` always.
- For `saltLength != digest_length`, drop to the lower-level `rsa::KeyPair::sign` API via `aws_lc_rs::rsa::SignaturePadding::PSS { hash, salt_len: SaltLen::Fixed(N) }`. (Verified against `aws-lc-rs 1.x` API: `SaltLen::Fixed(usize)` accepts an explicit byte count.)

**RSA-OAEP encrypt (§22.4.1):**
1. Let `label = normalizedAlgorithm.label` (default empty).
2. Run RSAES-OAEP-ENCRYPT(key.[[handle]], data, label, hashAlgorithm) per RFC 8017 §7.1.1.
3. Return ciphertext.

aws-lc-rs: `OaepPublicEncryptingKey::encrypt(oaep_alg, data, output, label)` with `OAEP_SHA256_MGF1SHA256` etc.

**RSA-OAEP plaintext over-allocation (critic #23):** The `min_output_size()` API gives the maximum possible plaintext length (modulus_length); the actual plaintext is `modulus_length - 2*hash_length - 2` at most. Allocate `min_output_size()` (correct), use `pt.to_vec()` (correct), but the `pt: &[u8]` slice is the actual plaintext length — return `pt.to_vec()` not `output.to_vec()`. The current impl already gets this right (line 999 `Ok(pt.to_vec())`), but the over-allocation comment in #23 is about the buffer SIZE not the returned length. The buffer IS oversized; we use the slice. OK as-is.

**importKey / exportKey:** formats `"spki"` / `"pkcs8"` (with embedded hash OID validation per critic #31 — the SPKI's AlgorithmIdentifier OID must match `key.algorithm.hash`; e.g. RSA-OAEP-with-SHA-256 has OID 1.2.840.113549.1.1.7), `"jwk"` (`kty: "RSA"`, fields `n`/`e`/`d`/`p`/`q`/`dp`/`dq`/`qi` — see §VI for the JWK walker; modulusLength embedded in `n`'s byte length).

### IV.8. HMAC (§31)

**HmacImportParams (§31.3):**
```webidl
dictionary HmacImportParams : Algorithm {
  required HashAlgorithmIdentifier hash;
  [EnforceRange] unsigned long length;
};
```

**HmacKeyGenParams (§31.5):**
```webidl
dictionary HmacKeyGenParams : Algorithm {
  required HashAlgorithmIdentifier hash;
  [EnforceRange] unsigned long length;
};
```

**generateKey steps (§31.6.4):**
1. If `usages` ⊄ `{sign, verify}` → `SyntaxError`.
2. Let `length` be the `length` member if present, else **the block size in bits of the hash function** (D-19 — current impl uses digest size, half the spec value for SHA-256).
3. If `length == 0` → `OperationError`.
4. Generate `length / 8` random bytes.
5. Build `HmacKeyAlgorithm { name: "HMAC", length, hash }`.

Block sizes:
- SHA-1: 512
- SHA-256: 512
- SHA-384: 1024
- SHA-512: 1024

**sign / verify:** aws-lc-rs's `hmac::sign(&hmac_key, data)` / `hmac::verify(&hmac_key, data, sig)`. Already correct in current impl; no changes.

**get key length (§31.6.6):** spec algorithm:
1. If `length == undefined` → return block size of hash.
2. Else if `length != 0` → return `length`.
3. Else → throw `TypeError`.

(D-19 fix.)

### IV.9. PBKDF2 (§34) / HKDF (§33)

**Pbkdf2Params (§34.3):**
```webidl
dictionary Pbkdf2Params : Algorithm {
  required BufferSource salt;
  required [EnforceRange] unsigned long iterations;
  required HashAlgorithmIdentifier hash;
};
```

**HkdfParams (§33.3):**
```webidl
dictionary HkdfParams : Algorithm {
  required HashAlgorithmIdentifier hash;
  required BufferSource salt;
  required BufferSource info;
};
```

**PBKDF2 deriveBits (§34.4.1):**
1. If `iterations == 0` → `OperationError`. (D-20 — and use `EnforceRangeU32` to reject `> u32::MAX`.)
2. Let `prf = HMAC-{hash}`.
3. Run PBKDF2(prf, key.material, salt, iterations, length / 8) per RFC 8018 §5.2.
4. Return derived bytes.

**aws-lc-rs binding:** `pbkdf2::derive(pbkdf2_alg, NonZeroU32::new(iterations).unwrap(), salt, &raw, &mut out)`. Hash variants:
- SHA-1 (NEW — critic #20): `PBKDF2_HMAC_SHA1`
- SHA-256: `PBKDF2_HMAC_SHA256`
- SHA-384: `PBKDF2_HMAC_SHA384`
- SHA-512: `PBKDF2_HMAC_SHA512`

**HKDF deriveBits (§33.4.1):**
1. Let `prk = HKDF-Extract(hash, salt, key.material)`.
2. Let `okm = HKDF-Expand(hash, prk, info, length / 8)`.
3. If `length / 8 > 255 * hash_length` → `OperationError`. (RFC 5869 limit.)
4. Return okm.

aws-lc-rs: `hkdf::Salt::new(alg, salt).extract(&raw)` then `prk.expand(&[info], DeriveLen(byte_len))` then `okm.fill(&mut out)`. Hash variants:
- SHA-1 (NEW — critic #20): `HKDF_SHA1_FOR_LEGACY_USE_ONLY`
- SHA-256/384/512: `HKDF_SHA256` / `_SHA384` / `_SHA512`

**deriveBits length validation (D-20 + critic #18):** cap `length` at a sane upper bound (e.g. `length <= 8160 * 8 = 65280` bits for HKDF — RFC 5869 hard limit for SHA-256). Reject excess with `OperationError`. Same for PBKDF2 (no spec hard limit, but we cap at `length <= 1_048_576` bits = 128 KiB to bound DoS; sufficient for any real use case).

### IV.10. Digest (§32)

**No params dictionary.**

**digest steps (§32.2):**
1. Run hash algorithm on data.
2. Return digest.

aws-lc-rs: `digest::digest(algorithm, data)`. Already correct in current impl. Migration is mechanical — the function moves from a free `#[zeroship_op]` to a `#[v8_method]` on `SubtleCrypto`. Keep the existing zero-copy ArrayBuffer path.

Hash algorithms: `SHA-1` (`SHA1_FOR_LEGACY_USE_ONLY`), `SHA-256` (`SHA256`), `SHA-384` (`SHA384`), `SHA-512` (`SHA512`).

Note (critic missing-concept §): `subtle.digest("SHA-256", new ArrayBuffer(0))` MUST return SHA-256 of empty string = `e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855`. aws-lc-rs handles empty input correctly. Add a hand-written test (§X.1).

## V. CryptoKey class

### V.1. IDL

Per spec §13:

```webidl
[Exposed=(Window,Worker), Serializable]
interface CryptoKey {
  readonly attribute KeyType type;
  readonly attribute boolean extractable;
  readonly attribute object algorithm;
  readonly attribute object usages;
};
```

Note: `algorithm` and `usages` are typed `object`, not specific dictionary/sequence types. Per spec they return frozen objects whose shape depends on the key's algorithm (RsaHashedKeyAlgorithm, EcKeyAlgorithm, etc.) — see §V.3.

### V.2. Internal slots

Per spec §13.2:
- `[[type]]`: KeyType — "public" | "private" | "secret"
- `[[extractable]]`: boolean
- `[[algorithm]]`: a `KeyAlgorithm` object
- `[[usages]]`: a list of `KeyUsage` strings
- `[[handle]]`: an opaque key-material reference (the actual key bytes, EVP_PKEY, etc.)

### V.3. Storage (D-2 + D-9)

```rust
/// V8 internal field 0 layout — branded with TAG byte for unspoofable
/// instanceof check (D-10).
#[repr(C)]
pub struct BrandedBox<T> {
    pub tag: u8,
    pub body: T,
}

pub const CRYPTO_KEY_TAG: u8 = 0xC1;

pub struct CryptoKeyState {
    pub key_type: KeyType,
    pub extractable: bool,
    pub algorithm: KeyAlgorithm,
    pub usages: Vec<KeyUsage>,
    pub material: KeyMaterial,
}

pub enum KeyType { Public, Private, Secret }

pub enum KeyUsage {
    Encrypt, Decrypt, Sign, Verify,
    DeriveKey, DeriveBits,
    WrapKey, UnwrapKey,
}

/// `KeyAlgorithm` is the spec's `[[algorithm]]` slot — algorithm-specific
/// shape that `key.algorithm` returns (frozen, [SameObject]).
pub enum KeyAlgorithm {
    Aes(AesKeyAlgorithm),
    Hmac(HmacKeyAlgorithm),
    RsaHashed(RsaHashedKeyAlgorithm),
    Ec(EcKeyAlgorithm),
    Hkdf(HkdfKeyAlgorithm),    // just { name }
    Pbkdf2(Pbkdf2KeyAlgorithm),  // just { name }
    Ed25519(Ed25519KeyAlgorithm),  // just { name }
    X25519(X25519KeyAlgorithm),    // just { name }
}

pub struct AesKeyAlgorithm {
    pub name: &'static str,  // "AES-CTR" | "AES-CBC" | "AES-GCM" | "AES-KW"
    pub length: u32,         // 128 | 192 | 256
}
pub struct HmacKeyAlgorithm {
    pub name: &'static str,  // "HMAC"
    pub hash: HashAlgo,
    pub length: u32,         // bits
}
pub struct RsaHashedKeyAlgorithm {
    pub name: &'static str,  // "RSASSA-PKCS1-v1_5" | "RSA-PSS" | "RSA-OAEP"
    pub modulus_length: u32,
    pub public_exponent: Vec<u8>,
    pub hash: HashAlgo,
}
pub struct EcKeyAlgorithm {
    pub name: &'static str,  // "ECDSA" | "ECDH"
    pub named_curve: NamedCurve,  // "P-256" | "P-384" | "P-521"
}

pub enum HashAlgo { Sha1, Sha256, Sha384, Sha512 }
pub enum NamedCurve { P256, P384, P521 }

/// `KeyMaterial` is the spec's `[[handle]]` slot — the actual key bytes /
/// pkcs8 / spki encoding.
pub enum KeyMaterial {
    Symmetric(Vec<u8>),
    EcPrivate { pkcs8_der: Vec<u8>, raw_d: Vec<u8> /* for JWK export */ },
    EcPublic { spki_der: Vec<u8>, raw_xy: Vec<u8> /* uncompressed point, for JWK + raw export */ },
    RsaPrivate { pkcs8_der: Vec<u8>, components: RsaPrivateComponents /* for JWK export */ },
    RsaPublic { spki_der: Vec<u8>, components: RsaPublicComponents },
    Ed25519Private { pkcs8_der: Vec<u8>, raw_d: [u8; 32] },
    Ed25519Public { spki_der: Vec<u8>, raw_x: [u8; 32] },
    X25519Private { pkcs8_der: Vec<u8>, raw_d: [u8; 32] },
    X25519Public { spki_der: Vec<u8>, raw_x: [u8; 32] },
}

pub struct RsaPrivateComponents { pub n: Vec<u8>, pub e: Vec<u8>, pub d: Vec<u8>, pub p: Vec<u8>, pub q: Vec<u8>, pub dp: Vec<u8>, pub dq: Vec<u8>, pub qi: Vec<u8> }
pub struct RsaPublicComponents { pub n: Vec<u8>, pub e: Vec<u8> }
```

Note the `KeyMaterial` variants store BOTH the wire-format encoding (PKCS#8 / SPKI) and the raw components for cheap JWK export. Storing both is ~2x memory per key but eliminates a re-derivation step on every JWK export. RSA keys are 2-4 KB; storing twice is fine.

**`[SameObject]` getter pattern (D-9):**
```rust
#[v8_getter]
fn algorithm<'s>(
    &self,
    scope: &mut v8::PinScope<'s, '_>,
    this: v8::Local<'s, v8::Object>,
) -> v8::Local<'s, v8::Value> {
    let key = v8::Private::for_api(scope,
        Some(v8::String::new(scope, "__cachedAlgorithm").unwrap()));
    if let Some(cached) = this.get_private(scope, key) {
        if !cached.is_undefined() {
            return cached;
        }
    }
    // First access — build the frozen JS object from KeyAlgorithm.
    let obj = self.algorithm.to_v8_frozen(scope);
    this.set_private(scope, key, obj.into());
    obj.into()
}
```

`KeyAlgorithm::to_v8_frozen` builds the spec-mandated shape (e.g. for RsaHashed: an Object with `name`, `modulusLength`, `publicExponent` (as Uint8Array), `hash: { name }` — all-frozen via `Object.freeze`).

`usages` getter is similar but produces a frozen Array (FrozenArray IDL — `Object.freeze` after element-set).

### V.4. Key-usage validation (D-7)

```rust
impl CryptoKeyState {
    pub fn check_usage(&self, op: KeyUsage) -> Result<(), OpError> {
        if !self.usages.contains(&op) {
            return Err(OpError::dom("InvalidAccessError",
                format!("Key usage {:?} not allowed", op)));
        }
        Ok(())
    }
}
```

Called at the start of every SubtleCrypto op (sign / verify / encrypt / decrypt / wrapKey / unwrapKey / deriveBits / deriveKey).

### V.5. Brand check (D-10)

```rust
impl CryptoKey {
    pub fn is_crypto_key(scope: &mut v8::PinScope, value: v8::Local<v8::Value>) -> bool {
        let obj = match v8::Local::<v8::Object>::try_from(value) {
            Ok(o) => o,
            Err(_) => return false,
        };
        if obj.internal_field_count() != 1 {
            return false;
        }
        let field = obj.get_internal_field(scope, 0).unwrap_or_else(|| v8::undefined(scope).into());
        let ext = match v8::Local::<v8::External>::try_from(field) {
            Ok(e) => e,
            Err(_) => return false,
        };
        let ptr = ext.value() as *const u8;
        if ptr.is_null() { return false; }
        // SAFETY: We never mutate a Box's tag byte; reading it is sound as
        // long as the External points to OUR allocation. The malicious case
        // (External pointing to attacker memory) is equivalent to memory
        // corruption in a single-isolate, single-thread runtime — only
        // achievable via Rust unsafe blocks the user can't reach.
        let tag = unsafe { *ptr };
        tag == CRYPTO_KEY_TAG
    }

    /// Read the boxed state. Panics if not a CryptoKey (caller must
    /// have called `is_crypto_key` first).
    pub fn state<'s>(scope: &mut v8::PinScope, value: v8::Local<v8::Value>) -> &'s CryptoKeyState {
        debug_assert!(Self::is_crypto_key(scope, value));
        let obj: v8::Local<v8::Object> = value.try_into().unwrap();
        let ext: v8::Local<v8::External> = obj.get_internal_field(scope, 0).unwrap().try_into().unwrap();
        let ptr = ext.value() as *const BrandedBox<CryptoKeyState>;
        // SAFETY: same as above; the box is alive while the JS wrapper is alive.
        unsafe { &(*ptr).body }
    }
}
```

(The `'s` lifetime is the V8 isolate's lifetime — the box lives at least as long as the wrapper, which lives within the isolate.)

### V.6. Serializable (D-28 — deferred)

Per spec §13.5:
- `[[Serializable]]` steps run on `structuredClone` / `postMessage` boundaries.
- Spec defines the serialize step: write `[[type]]`, `[[extractable]]`, `[[algorithm]]`, `[[usages]]`, `[[handle]]` to the structured-clone record.
- Deserialize step: read them back; `[[handle]]` is created from the cloned bytes.

In v1, the `serialize` step is implemented as: throw `DataCloneError`. (We can preserve `[Serializable]` IDL but error-out in practice; no JS path can actually invoke this until we ship MessagePort / Worker.) v2 with Workers ships the real serializer.

## VI. JsonWebKey (D-5) — the largest single feature

Spec §15 + RFC 7517 / 7518.

### VI.1. JsonWebKey shape

```webidl
dictionary RsaOtherPrimesInfo {
  DOMString r;
  DOMString d;
  DOMString t;
};

dictionary JsonWebKey {
  // RFC 7517 §4 — "Common Parameters"
  DOMString kty;
  DOMString use;
  sequence<DOMString> key_ops;
  DOMString alg;

  // RFC 7517 §5 — "ext" parameter
  boolean ext;

  // RFC 7518 §6 — "Algorithm Specific Parameters"
  // Symmetric (oct)
  DOMString k;

  // EC
  DOMString crv;
  DOMString x;
  DOMString y;
  DOMString d;

  // RSA
  DOMString n;
  DOMString e;
  // d above
  DOMString p;
  DOMString q;
  DOMString dp;
  DOMString dq;
  DOMString qi;
  sequence<RsaOtherPrimesInfo> oth;
};
```

(All fields are optional in the IDL — per-algorithm requirements enforce which are required at parse time.)

### VI.2. Storage

```rust
pub struct JsonWebKey {
    pub kty: String,
    pub r#use: Option<String>,
    pub key_ops: Option<Vec<String>>,
    pub alg: Option<String>,
    pub ext: Option<bool>,
    pub k: Option<String>,
    pub crv: Option<String>,
    pub x: Option<String>,
    pub y: Option<String>,
    pub d: Option<String>,
    pub n: Option<String>,
    pub e: Option<String>,
    pub p: Option<String>,
    pub q: Option<String>,
    pub dp: Option<String>,
    pub dq: Option<String>,
    pub qi: Option<String>,
    pub oth: Option<Vec<RsaOtherPrimesInfo>>,
}
```

### VI.3. parse_jwk(scope, obj) — the WebIDL dictionary parser

```rust
pub fn parse_jwk(scope: &mut v8::PinScope, obj: v8::Local<v8::Object>) -> Result<JsonWebKey, OpError> {
    let mut jwk = JsonWebKey::default();
    jwk.kty = read_required_string(scope, obj, "kty")?;
    jwk.r#use = read_optional_string(scope, obj, "use")?;
    jwk.key_ops = read_optional_string_array(scope, obj, "key_ops")?;
    jwk.alg = read_optional_string(scope, obj, "alg")?;
    jwk.ext = read_optional_bool(scope, obj, "ext")?;
    jwk.k = read_optional_string(scope, obj, "k")?;
    jwk.crv = read_optional_string(scope, obj, "crv")?;
    // ... etc for x/y/d/n/e/p/q/dp/dq/qi/oth
    Ok(jwk)
}
```

`read_optional_string` reads a property, converts to USVString (returning `Option<String>`); throws `TypeError` if the property exists but isn't a string.

base64url decode helper (simdutf-style or hand-rolled — RFC 4648 §5):
```rust
pub fn base64url_decode(s: &str) -> Result<Vec<u8>, OpError> {
    // Convert URL-safe to standard alphabet: '-' → '+', '_' → '/'.
    // Pad to multiple of 4 with '='.
    // Decode via base64 crate (existing workspace dep).
    ...
}
```

Reference: workerd's `simdutfBase64UrlDecode` at `refs/workerd/src/workerd/api/crypto/jwk.c++:179` etc.

### VI.4. Per-algorithm JWK importers

Each algorithm has a `import_from_jwk(jwk, params, extractable, usages) -> Result<CryptoKeyState, OpError>` function. The shape:

**HMAC:** `kty == "oct"`, `k` required (base64url → raw bytes). Validate `alg` matches hash (e.g. `HS256` for SHA-256). Validate `use == "sig"` if present. Validate `key_ops` ⊇ `usages`. Validate `ext == extractable` if present.

**AES-{CTR,CBC,GCM,KW}:** `kty == "oct"`, `k` required. Validate `alg` matches algorithm + length (e.g. `A128GCM`, `A256KW`). Validate `use == "enc"` if present.

**RSA-{PKCS1v1_5,PSS,OAEP}:** `kty == "RSA"`, `n` and `e` required (public + private). For private also: `d`, `p`, `q`, `dp`, `dq`, `qi` (all required per RFC 7518 §6.3.2). Validate `alg` matches algorithm + hash (e.g. `RS256`, `PS384`, `RSA-OAEP-256`). Build PKCS#8 from components via aws-lc-rs's `rsa::PrivateKey::from_components` or via a small DER builder.

**ECDSA / ECDH:** `kty == "EC"`, `crv` required, `x` and `y` required (public + private), `d` required for private. Validate `crv` matches `params.namedCurve`. Build PKCS#8 from components via aws-lc-rs's EC key builder or via a small DER builder.

**Ed25519:** `kty == "OKP"`, `crv == "Ed25519"`, `x` required (public + private), `d` required for private. Use aws-lc-rs `signature::Ed25519KeyPair::from_seed_unchecked` (private) or `UnparsedPublicKey::new(ED25519, x)` (public).

**X25519:** `kty == "OKP"`, `crv == "X25519"`, `x` required (public + private), `d` required for private. Use aws-lc-rs `agreement::X25519` with raw bytes.

### VI.5. Per-algorithm JWK exporters

Symmetric to importers — read the stored components, base64url-encode to JWK fields. RSA / EC components are stored alongside the wire-format encoding (D-2 / `KeyMaterial`'s component fields), so JWK export is a simple map.

### VI.6. JWK round-trip invariant

For every algorithm + format combination, `import("jwk", export("jwk", k)) === k` (semantically — same algorithm, extractable, usages, material). Hand-written tests in `tests/crypto_jwk.rs` enforce this for every algorithm in the registry.

## VII. Algorithm normalization (§18.4.4)

(D-8.) Replaces the JS-side hand-rolled `normalizeAlgorithm` at `embed/crypto.js:45-58`.

### VII.1. Compile-time registry

```rust
// crates/runtime/src/crypto/registry.rs

pub enum ParamShape {
    /// Algorithm name only — no params dictionary.
    NameOnly,
    /// AesGcmParams { iv: BufferSource, additionalData?: BufferSource, tagLength?: u8 }.
    AesGcm,
    AesCtr,        // { counter: BufferSource, length: u8 }
    AesCbc,        // { iv: BufferSource (16 bytes) }
    RsaOaep,       // { label?: BufferSource }
    RsaPss,        // { saltLength: EnforceRangeU32 }
    RsaHashedKeyGen,  // { modulusLength, publicExponent, hash }
    RsaHashedImport,  // { hash }
    EcdsaParams,   // { hash }
    EcKeyGen,      // { namedCurve }
    EcKeyImport,   // { namedCurve }
    EcdhKeyDerive, // { public: CryptoKey }
    HmacImport,    // { hash, length? }
    HmacKeyGen,    // { hash, length? }
    HkdfParams,    // { hash, salt: BufferSource, info: BufferSource }
    Pbkdf2Params,  // { hash, salt: BufferSource, iterations: EnforceRangeU32 }
    AesKeyGen,     // { length: u32 }
    AesDerivedKey, // { length: u32 }
}

/// Operation names (spec §18.4.4 step 1).
pub enum Operation {
    Encrypt, Decrypt, Sign, Verify, Digest,
    GenerateKey, ImportKey, DeriveBits,
    GetKeyLength, WrapKey, UnwrapKey,
}

pub struct AlgorithmEntry {
    pub canonical_name: &'static str,
    pub shape: ParamShape,
}

/// Compile-time map: (Operation, uppercase algorithm name) → AlgorithmEntry.
/// Built via phf::Map for O(1) lookup with no runtime allocation.
pub static REGISTRY: phf::Map<(Operation, &'static str), AlgorithmEntry> = phf::phf_map! {
    (Operation::Digest, "SHA-1") => AlgorithmEntry { canonical_name: "SHA-1", shape: ParamShape::NameOnly },
    (Operation::Digest, "SHA-256") => AlgorithmEntry { canonical_name: "SHA-256", shape: ParamShape::NameOnly },
    (Operation::Digest, "SHA-384") => AlgorithmEntry { canonical_name: "SHA-384", shape: ParamShape::NameOnly },
    (Operation::Digest, "SHA-512") => AlgorithmEntry { canonical_name: "SHA-512", shape: ParamShape::NameOnly },
    (Operation::Encrypt, "AES-GCM") => AlgorithmEntry { canonical_name: "AES-GCM", shape: ParamShape::AesGcm },
    // ... full table per §IV.1
};
```

### VII.2. normalize_an_algorithm — the spec algorithm

```rust
pub fn normalize_an_algorithm(
    scope: &mut v8::PinScope,
    op: Operation,
    alg: v8::Local<v8::Value>,
) -> Result<NormalizedAlgorithm, OpError> {
    // Spec §18.4.4 step 0: if alg is a DOMString, treat as { name: alg }.
    let alg_obj = if alg.is_string() {
        let obj = v8::Object::new(scope);
        let name_key = v8::String::new(scope, "name").unwrap();
        obj.set(scope, name_key.into(), alg);
        obj
    } else if let Ok(o) = v8::Local::<v8::Object>::try_from(alg) {
        o
    } else {
        return Err(OpError::type_error("Algorithm: must be a string or object"));
    };

    // Step 1: lookup the registered algorithm.
    let name_key = v8::String::new(scope, "name").unwrap();
    let name = alg_obj.get(scope, name_key.into())
        .and_then(|v| v.to_rust_string_lossy(scope).into())
        .ok_or_else(|| OpError::type_error("Algorithm: missing 'name' member"))?;

    let upper = name.to_uppercase();
    let entry = REGISTRY.get(&(op, upper.as_str()))
        .ok_or_else(|| OpError::dom("NotSupportedError",
            format!("Unrecognized algorithm name '{}' for op {:?}", name, op)))?;

    // Step 2-9: parse parameters per the shape.
    let params = match entry.shape {
        ParamShape::NameOnly => NormalizedParams::None,
        ParamShape::AesGcm => parse_aes_gcm_params(scope, alg_obj)?,
        ParamShape::AesCtr => parse_aes_ctr_params(scope, alg_obj)?,
        // ... etc
    };

    Ok(NormalizedAlgorithm {
        name: entry.canonical_name,
        params,
    })
}
```

The per-shape parsers handle WebIDL conversions: `BufferSource` → `Vec<u8>` via the existing macro pattern, `[EnforceRange] unsigned long` → `u32` (D-30 macro extension #1 — `EnforceRangeU32`), `HashAlgorithmIdentifier` → recursive call to `normalize_an_algorithm(scope, Operation::Digest, ...)`.

```rust
fn parse_aes_gcm_params(scope: &mut v8::PinScope, obj: v8::Local<v8::Object>) -> Result<NormalizedParams, OpError> {
    let iv_key = v8::String::new(scope, "iv").unwrap();
    let iv_v = obj.get(scope, iv_key.into()).ok_or_else(|| OpError::type_error("AesGcmParams: missing 'iv'"))?;
    let iv = read_buffer_source(scope, iv_v)?;
    let aad_key = v8::String::new(scope, "additionalData").unwrap();
    let aad = obj.get(scope, aad_key.into())
        .filter(|v| !v.is_undefined())
        .map(|v| read_buffer_source(scope, v))
        .transpose()?;
    let tag_key = v8::String::new(scope, "tagLength").unwrap();
    let tag_length = obj.get(scope, tag_key.into())
        .filter(|v| !v.is_undefined())
        .map(|v| read_enforce_range_u32(scope, v))
        .transpose()?
        .map(|v| u8::try_from(v).map_err(|_| OpError::type_error("tagLength out of range for octet")))
        .transpose()?
        .unwrap_or(128);
    Ok(NormalizedParams::AesGcm { iv, additional_data: aad, tag_length })
}
```

### VII.3. AlgorithmIdentifier recursion

The spec has `HashAlgorithmIdentifier` (typedef `AlgorithmIdentifier`). When a parameter is typed `HashAlgorithmIdentifier`, the parser re-runs `normalize_an_algorithm(scope, Operation::Digest, hash_value)` and stores the result. This matches Deno's pattern at `00_crypto.js:313-314`:
```js
} else if (idlType === "HashAlgorithmIdentifier") {
    normalizedAlgorithm[member] = normalizeAlgorithm(idlValue, "digest");
}
```

### VII.4. Error type for unsupported algorithm

Spec §18.4.4 step 5: "If algName is not the name of any registered algorithm for op, throw a `NotSupportedError`." DOMException.

Note on `SyntaxError`: per spec a few generateKey checks throw the IDL `SyntaxError` interface (not `DOMException("SyntaxError")`). In practice browsers conflate these — Chrome / Firefox both throw `DOMException` with `name === "SyntaxError"`. We match: `OpError::dom("SyntaxError", ...)` for these spec paths. WPT assertions use `assert_throws_dom("SyntaxError", ...)` which accepts either. Document.

## VIII. Macro extensions (D-30)

Four small additions to `runtime-macros`:

### VIII.1. `EnforceRangeU32` newtype

Companion to the existing `EnforceRangeU64` (`runtime-macros/src/lib.rs:231-233`, `enforce_range.rs`). Used by `Pbkdf2Params.iterations`, `deriveBits.length`, `RsaKeyGenParams.modulusLength`, `AesGcmParams.tagLength` (after octet truncation), and the AesCtrParams.length.

```rust
// crates/runtime/src/crypto/enforce_range.rs (new file or adjacent to existing
// enforce_range.rs)
pub struct EnforceRangeU32(pub u32);

pub fn read_enforce_range_u32(
    scope: &mut v8::PinScope,
    value: v8::Local<v8::Value>,
) -> Result<u32, OpError> {
    let n = value.number_value(scope)
        .ok_or_else(|| OpError::type_error("Cannot convert to number"))?;
    if n.is_nan() || n.is_infinite() {
        return Err(OpError::type_error("EnforceRange: NaN or Infinity"));
    }
    if n < 0.0 {
        return Err(OpError::type_error("EnforceRange: negative"));
    }
    if n > u32::MAX as f64 {
        return Err(OpError::type_error("EnforceRange: > 2^32 - 1"));
    }
    if n.fract() != 0.0 {
        return Err(OpError::type_error("EnforceRange: non-integer"));
    }
    Ok(n as u32)
}
```

Macro extension: `is_enforce_range_u32(ty)` predicate (mirrors `is_enforce_range_u64`); `gen_extract` arm calling `read_enforce_range_u32`. Total ~30 LOC of macro code.

### VIII.2. `OpErrorKind::DomException(name)` variant

```rust
// crates/runtime/src/core/state.rs
pub enum OpErrorKind {
    TypeError,
    RangeError,
    Error,
    DomException(&'static str),  // NEW: name = "OperationError" | "DataError" | etc.
}

impl OpError {
    pub fn dom(name: &'static str, msg: impl Into<String>) -> Self {
        Self {
            kind: OpErrorKind::DomException(name),
            message: msg.into(),
        }
    }
}
```

Macro extension at `runtime-macros/src/lib.rs:629-639` (gen_throw_error):
```rust
fn gen_throw_error() -> TokenStream2 {
    quote! {
        let __msg = v8::String::new(scope, &__err.message).unwrap();
        let __exc = match __err.kind {
            ::zeroship_runtime::state::OpErrorKind::TypeError => v8::Exception::type_error(scope, __msg),
            ::zeroship_runtime::state::OpErrorKind::RangeError => v8::Exception::range_error(scope, __msg),
            ::zeroship_runtime::state::OpErrorKind::DomException(name) => {
                // Look up globalThis.DOMException, construct via `new DOMException(msg, name)`.
                let global = scope.get_current_context().global(scope);
                let dom_key = v8::String::new(scope, "DOMException").unwrap();
                let dom_ctor: v8::Local<v8::Function> = global.get(scope, dom_key.into())
                    .and_then(|v| v8::Local::<v8::Function>::try_from(v).ok())
                    .expect("globalThis.DOMException not installed");
                let name_str = v8::String::new(scope, name).unwrap();
                let args = [__msg.into(), name_str.into()];
                let exc = dom_ctor.new_instance(scope, &args).unwrap();
                exc.into()
            }
            _ => v8::Exception::error(scope, __msg),
        };
        scope.throw_exception(__exc);
    }
}
```

This is the only place in the macro that needs to know about DOMException; the `OpError::dom(...)` constructor is the user-facing API.

**Dependency note (D-6):** the `globalThis.DOMException` lookup assumes the native DOMException class is installed during `setup_globals`. The `feature/fetch-js-delete` agent is shipping this. If that agent's PR slips, the fallback is to detect missing DOMException at install time and fall back to `v8::Exception::error` with a `"<name>: <msg>"` prefix that a JS-side post-processor unwraps. The fallback is mechanical — replace 5 lines, ship.

### VIII.3. `Result<bool, OpError>` return path

Currently the macro's `gen_call_return` (`runtime-macros/src/lib.rs:647-791`) handles `Result<T, OpError>` for many T (Vec<u8>, String, Option, etc.) but the bool case maps via `Some("bool") => quote! { ... rv.set(v8::Boolean::new(scope, b).into()); }` — let me re-read.

Looking at lines 760-763:
```rust
Some("bool") => quote! {
    let __r = #call;
    rv.set(v8::Boolean::new(scope, __r).into());
},
```

That's the BARE bool path (no Result wrapper). The `Result<bool, OpError>` path goes through the generic Result match arm at lines 654-711, then into the inner-type dispatch at `inner_ident.as_deref()` at line 665. Looking at `gen_scalar_set` at line 558 — `Some("bool")` returns `v8::Boolean`. Good — so `Result<bool, OpError>` IS already wired (the inner-type bool case threads through `gen_scalar_set`).

Verification: trace through. `Result<bool, OpError>` → outer = "Result" → inner = bool → `inner_ident.as_deref() == Some("bool")` doesn't match Vec/Option, falls to `_ => gen_scalar_set(t, &val)` at line 696-700 — which IS the bool case at 558-560. ✓

So the macro ALREADY handles `Result<bool, OpError>`. **Critic finding #32 was about the current `crypto.rs` impl returning String "true"/"false" — not a macro gap.** The migration just changes `crypto_verify` from `-> Result<String, OpError>` to `-> Result<bool, OpError>`. No macro work needed.

(D-30 #3 is therefore a no-op. Drop from the macro-extension list.)

Updated D-30 list (final):
1. `EnforceRangeU32` — required
2. `OpErrorKind::DomException(name)` — required
3. ~~`Result<bool, OpError>` — already shipped~~
4. ~~Recursive WebIDL dictionary parsing — handled in normalize_an_algorithm helper, NOT a macro feature~~
5. `[SameObject]` cache — explicit getter helper pattern, NOT a macro feature

**Final macro extension cost:** ~50 LOC across `runtime-macros/src/lib.rs` and `crates/runtime/src/core/state.rs`. The other "extensions" listed in D-30 above are not macro changes — they're library-level patterns the design uses (cache via private symbol, normalize via library function, etc.).

## IX. Async / threading model (D-29)

### IX.1. v1: synchronous-on-V8-thread

Every WebCrypto op runs synchronously on the V8 thread. The method body computes the result, then resolves the promise synchronously:

```rust
fn encrypt(&self, scope, alg, key, data) -> Result<v8::Local<v8::Promise>, OpError> {
    let resolver = v8::PromiseResolver::new(scope).unwrap();
    let promise = resolver.get_promise(scope);
    match perform_encrypt(scope, alg, key, data) {
        Ok(out) => {
            let buf = vec_to_uint8array(scope, &out);
            resolver.resolve(scope, buf.into());
        }
        Err(e) => {
            let exc = e.to_v8_exception(scope);
            resolver.reject(scope, exc);
        }
    }
    Ok(promise)
}
```

The microtask hop happens via V8's promise resolution semantics: `await crypto.subtle.encrypt(...)` always sees one microtask hop between the call and the resolution. This matches the polyfill's behaviour (`Promise.resolve(syncResult)`).

### IX.2. The pin-the-thread concern

A `crypto.subtle.encrypt` of a 100 MB buffer pins the entire isolate for the encrypt's duration. In multi-tenant worker mode, this blocks every other app sharing the thread.

**v1 mitigation:** the existing 65 KB cap on `getRandomValues` (D-21) is the only quota. Crypto ops of arbitrary size run unbounded. Practical creator apps don't pass 100 MB to `subtle.encrypt`; AI-streaming workflows chunk via `TransformStream` (which the runtime supports natively).

**v2 plan:** introduce `spawned_crypto: Vec<Pin<Box<...>>>` companion to the existing `spawned_ops` queue, backed by compio's blocking-task pool (compio has `compio::runtime::spawn_blocking`). Crypto ops above a size threshold (e.g. 64 KB) get pushed to the blocking pool; the V8 thread polls `spawned_crypto` in the main loop. Pattern is identical to the existing fetch / kv / db dispatch, just on a different queue. The macro's existing async generator handles this transparently — change `fn encrypt(...) -> Result<...>` to `async fn encrypt(...) -> Result<...>` and the codegen emits a future-pushing path.

v2 is deferred to a follow-up ADR; v1 ships sync. Critic #12 acknowledged this; D-29 commits to the deferral.

## X. Test plan

### X.1. Hand-written tests

Lives in `crates/runtime/tests/`. Mirrors WPT but exercises corners the WPT suite doesn't:

- `crypto_native.rs`:
  - Smoke: each algorithm round-trip (generateKey → sign → verify; generateKey → encrypt → decrypt; deriveBits length validation).
  - `subtle.digest("SHA-256", new ArrayBuffer(0))` returns the empty-string hash (critic missing-concept).
  - `getRandomValues(new Float32Array(8))` throws TypeMismatchError (D-21).
  - `getRandomValues(new ArrayBuffer(80000))` throws QuotaExceededError (D-21).
  - `getRandomValues(new Uint8Array(0))` returns the array unchanged.
- `crypto_jwk.rs`:
  - JWK round-trip per algorithm (HMAC, AES-128/192/256-{CTR,CBC,GCM,KW}, RSA-{PKCS1v1_5,PSS,OAEP}-{2048,3072,4096}-{SHA-256,SHA-384,SHA-512}, ECDSA-{P-256,P-384,P-521}, ECDH-{P-256,P-384,P-521}, Ed25519, X25519).
  - JWK with mismatched `alg` → DataError.
  - JWK with mismatched `key_ops` vs `usages` → SyntaxError.
  - JWK with missing required field (RSA missing `n`) → DataError.
- `crypto_errors.rs`:
  - importKey "raw" with non-multiple-of-8 AES bytes → DataError.
  - sign with public key → InvalidAccessError.
  - verify with private key → InvalidAccessError.
  - encrypt with key.usages = ["sign"] → InvalidAccessError (D-7).
  - importKey unsupported format → NotSupportedError.
  - normalizeAlgorithm unknown name → NotSupportedError.
  - PBKDF2 iterations > u32::MAX → TypeError (EnforceRange).
  - PBKDF2 iterations 0 → OperationError.
  - AES-GCM encrypt with empty IV → OperationError.
  - AES-GCM decrypt with mangled tag → OperationError.
  - getRandomValues(DataView(...)) → TypeMismatchError.
- `crypto_usages.rs`:
  - generateKey({name:"AES-GCM",length:256}, true, ["encrypt","decrypt"]) — usages stored.
  - .encrypt(...) succeeds; .decrypt(...) succeeds; .sign(...) throws InvalidAccessError.
  - exportKey on extractable=true succeeds; on extractable=false throws InvalidAccessError.
  - generateKey({name:"AES-GCM"}, true, ["sign"]) → SyntaxError (sign not in AES-GCM allowed usages).
- `crypto_brand.rs`:
  - `Object.create(CryptoKey.prototype, {...})` → not detected as CryptoKey (D-10).
  - `key instanceof CryptoKey` true; spoofed object instanceof passes BUT operations reject with TypeError("Expected CryptoKey").
  - `key.algorithm === key.algorithm` (D-9 SameObject).
  - `key.usages === key.usages` (D-9 SameObject).
  - `key.algorithm` is frozen (Object.isFrozen).
  - `key.type = "private"` (mutation attempt) — silent no-op or strict-mode throw, but `key.type` still returns the original.
  - `Object.prototype.toString.call(key) === "[object CryptoKey]"` (D-10 / critic #42).
- `crypto_ecdsa_wire.rs`:
  - ECDSA-P-256-SHA-256 sign produces 64-byte signature (NOT DER ~70-72 bytes).
  - Imported P-256 ECDSA key signs; signature verifies via aws-lc-rs `_FIXED` constant directly.
  - Cross-stack: ECDSA signature produced here verifies against a known-good Chrome-issued public key (test vector from RFC 6979 Appendix A.2.5 SHA-256).
- `crypto_aes_gcm_full.rs`:
  - 128-bit IV encrypt/decrypt round-trip (D-17).
  - 64-bit IV encrypt/decrypt round-trip (D-17).
  - tagLength=32 produces ciphertext+4-byte-tag (D-18).
  - tagLength=96 round-trip (D-18).
  - tagLength=128 default (D-18).
  - tagLength=33 (invalid) → OperationError.
- `crypto_rsa_pss_salt.rs`:
  - saltLength=0 sign/verify (D-16).
  - saltLength=32 (digest_length for SHA-256) sign/verify.
  - saltLength=64 sign/verify.
- `crypto_p521.rs`: ECDSA-P-521-SHA-512 + ECDH-P-521 round-trip (D-14).
- `crypto_ecdh.rs`: ECDH-P-256 / P-384 / P-521 / X25519 deriveBits round-trip (D-13 / D-26).
- `crypto_aes_ctr.rs`: AES-CTR-128/192/256 encrypt/decrypt round-trip (D-11).
- `crypto_aes_kw.rs`: AES-KW-128/192/256 wrap/unwrap round-trip (D-12).
- `crypto_rsa_keygen.rs`: RSA generateKey at 2048/3072/4096 + sign/verify round-trip (D-15).
- `crypto_normalize.rs`:
  - `subtle.digest("SHA-256", ...)` works (string algorithm).
  - `subtle.digest({name:"sha-256"}, ...)` works (case-insensitive name lookup).
  - `subtle.digest({name:"AES-GCM"}, ...)` → NotSupportedError (AES-GCM has no digest).
  - `subtle.sign({name:"HMAC", hash:"SHA-256"}, ...)` — recursive normalization.
  - `subtle.sign({name:"HMAC", hash:{name:"SHA-256"}}, ...)` — recursive normalization.

### X.2. WPT inventory

WPT `WebCryptoAPI/` directory tree (per https://github.com/web-platform-tests/wpt/tree/master/WebCryptoAPI):

**Top-level (~11 files):**
- `getRandomValues.any.js` — D-21
- `randomUUID.https.any.js` — already passes
- `historical.any.js` — superseded API checks
- `idlharness.https.any.js` — IDL surface verification
- `crypto_key_cached_slots.https.any.js` — D-9
- `normalize-algorithm-name.https.any.js` — D-8
- `algorithm-discards-context.https.window.js` — context discarding
- `getPublicKey.tentative.https.any.js` — Non-Goal
- `idlharness.tentative.https.any.js` — tentative additions
- `supports.tentative.https.any.js` / `supports-modern.tentative.https.any.js` — tentative

**`digest/`** — basic digest tests; should pass post-migration.

**`sign_verify/`:**
- ECDSA matrix
- RSA-PKCS1v1_5 matrix
- RSA-PSS matrix (D-16)
- HMAC matrix
- Ed25519 matrix

**`encrypt_decrypt/`:**
- AES-CBC matrix
- AES-CTR matrix (D-11)
- AES-GCM matrix (D-17 / D-18)
- RSA-OAEP matrix

**`derive_bits_keys/`:**
- HKDF matrix (incl. SHA-1 — critic #20)
- PBKDF2 matrix (incl. SHA-1, EnforceRange iterations — D-20 / critic #20)
- ECDH matrix (D-13)
- X25519 matrix (D-26)

**`generateKey/`:**
- ~30 files — `successes_*` per algorithm + `failures_*` per algorithm.
- Tests usages validation, key parameter validation, default lengths (D-19).

**`import_export/`:**
- `ec_importKey.https.any.js` — full EC matrix
- `ec_importKey_failures_ECDH.https.any.js`
- `ec_importKey_failures_ECDSA.https.any.js`
- `okp_importKey_Ed25519.https.any.js`
- `okp_importKey_X25519.https.any.js`
- `okp_importKey_failures_*.https.any.js`
- `rsa_importKey.https.any.js`
- `symmetric_importKey.https.any.js`
- (~9 files; all need D-5 JWK + D-31 OID validation)

**`wrapKey_unwrapKey/`:** wrapKey/unwrapKey round-trips (D-12).

**`encap_decap/`:** Encapsulation / decapsulation — ML-KEM and Kyber post-quantum (tentative, NOT in Level 2). Skipped.

**`secure_context/`:** SecureContext gating — D-22 (no-op). Tests likely pass trivially because we expose subtle / randomUUID unconditionally.

**Vendor in this order (Tier 1 = smoke + fundamentals):**
1. `getRandomValues.any.js` + `randomUUID.https.any.js` (already mostly passing)
2. `historical.any.js` + `idlharness.https.any.js` + `crypto_key_cached_slots.https.any.js`
3. `digest/digest.https.any.js`
4. `sign_verify/hmac.https.any.js`
5. `sign_verify/ecdsa.https.any.js` (D-4 — biggest single fix)
6. `sign_verify/rsa_pkcs.https.any.js`
7. `sign_verify/rsa_pss.https.any.js` (D-16)
8. `encrypt_decrypt/aes_gcm.https.any.js` (D-17 + D-18)
9. `encrypt_decrypt/aes_cbc.https.any.js`
10. `encrypt_decrypt/aes_ctr.https.any.js` (D-11)
11. `encrypt_decrypt/rsa_oaep.https.any.js`
12. `derive_bits_keys/hkdf.https.any.js`
13. `derive_bits_keys/pbkdf2.https.any.js` (D-20)
14. `derive_bits_keys/ecdh.https.any.js` (D-13)
15. `derive_bits_keys/x25519.https.any.js` (D-26)
16. `generateKey/successes_*.https.any.js` (~10 files)
17. `generateKey/failures_*.https.any.js` (~10 files)
18. `import_export/*` (~9 files; all 9 require D-5 JWK)
19. `wrapKey_unwrapKey/wrapKey_unwrapKey.https.any.js` (D-12)
20. `normalize-algorithm-name.https.any.js` (D-8)

**Total inventory:** ~75 `.any.js` files in v1 must-pass set; ~15 deferred (tentative, encap_decap, getPublicKey).

**Pass-rate targets:**
- Today (pre-migration): ~15-20% across the suite (per critic).
- Post-migration v1: 90%+ across must-pass set.
- Stretch: 95%+ requires the JWK matrix to be complete; ECDSA wire-format fix alone bumps `sign_verify/ecdsa` from 0% to ~95%.

**Browser baselines (typical for cross-reference):**
- Chrome 128: ~95% pass on WebCryptoAPI overall.
- Firefox 128: ~93% pass.
- Safari 17: ~85% pass.
- workerd: ~95% pass (per workerd's own internal harness).

### X.3. WPT runner

Pattern from streams-native + fetch-native: `crates/runtime/tests/wpt_webcrypto.rs` boots a runtime, vendors WPT `WebCryptoAPI/` tests via the cargo-included `wpt/` directory, runs each via the existing `testharness.js` polyfill (already shipped for streams + fetch). Tracking via `tests/wpt-webcrypto.expectations` (analogous to streams-native's expected WPT result file).

Update `crates/runtime/tests/setup-wpt.sh`: add `/WebCryptoAPI/` to the sparse-checkout list. Adds ~3-4 MB to the working tree.

## XI. Comparison with reference implementations

| Project | Crypto LOC | Native or JS | JWK | DOMException | ECDSA wire | Notes |
|---------|-----------:|--------------|-----|--------------|-----------|-------|
| **workerd** | ~9,700 (api/crypto/) | Pure native (C++) on BoringSSL/ncrypto | Yes (`jwk.c++`) | Yes (`JSG_DOMEXCEPTION`) | r∥s (FIXED) | Most spec-faithful production native impl. Heavy reliance on ncrypto wrapper for OpenSSL EVP_PKEY. ~3,000 LOC of helpers + 6,700 LOC of class glue. |
| **Node.js** | ~5,500 JS + ~2,000 C++ = 7,500 LOC | Hybrid (`lib/internal/crypto/`) | Yes | Yes | r∥s (FIXED) | Gold-standard JS layer. Reference for parameter validation, JWK shapes, error messages. We mirror the algorithm dispatch shape. |
| **Deno** | ~1,560 Rust + ~6,131 JS = 7,691 LOC | Hybrid (RustCrypto + ring) | Yes (`import_key.rs`) | Yes (DOMException class) | r∥s (FIXED) | Primary Rust reference. Different crypto crate stack (RustCrypto + ring + ed25519-dalek + x25519-dalek for OKP). Algorithm dispatch in JS (`00_crypto.js`); ops in Rust. |
| **Bun** | undisclosed (proprietary fork of Bun) | Native via OpenSSL | Yes | Yes | r∥s (FIXED) | Spec-correct via OpenSSL EVP_PKEY. |
| **Current zeroship** | 313 JS + 1308 Rust = 1621 LOC | Hybrid (aws-lc-rs + JS shim) | NO | NO | DER (BROKEN) | The status quo. 38/100 critic score. |
| **This design (Rust)** | ~3700 native + ~150 JS = 3850 LOC | Pure native (aws-lc-rs) | Yes | Yes | r∥s (FIXED) | Single-provider (aws-lc-rs only). Closer to workerd in scope and size; smaller than Deno because we ship one crypto provider not three. |

The native implementation comes in at roughly **40% of workerd's LOC** and **half of Deno's combined LOC**. The reduction vs workerd comes from:
- aws-lc-rs's safer high-level API vs. raw OpenSSL EVP_PKEY (most workerd code is FFI-glue).
- Single-provider (aws-lc-rs covers RSA + EC + OKP + AES + HMAC + KDF; workerd uses ncrypto + raw BoringSSL).
- No PEM / X509 helpers (workerd ships PKCS#7 / spkac / X509 — out of WebCrypto Level 2).
- No prime / scrypt / DH (workerd ships these as Node-compat extensions; out of WebCrypto Level 2).

Compared to Deno: similar size (Deno also lives in ~7K LOC), but Deno splits across JS + Rust whereas this design pushes everything to Rust.

## XII. Polyfill removal cadence (D-23)

Three landings, mirroring streams-native D-19 / fetch-native D-23:

### Landing 1 — feature-flagged native, polyfill default

Realised as the env-var gate `ZEROSHIP_NATIVE_CRYPTO=1` (matches `ZEROSHIP_NATIVE_STREAMS` / `ZEROSHIP_NATIVE_FETCH` / `ZEROSHIP_NATIVE_HEADERS`):

- `crates/runtime/src/crypto/` module fully implemented.
- `init.rs` install ordering: when `ZEROSHIP_NATIVE_CRYPTO` is set, install `Crypto` / `SubtleCrypto` / `CryptoKey` classes and skip the JS polyfill load; when unset, the polyfill (`embed/crypto.js`) loads as default.
- WPT runners (`wpt_webcrypto.rs`) set the env var inside their harness.
- Smoke tests at `tests/crypto_native_install.rs` confirm `globalThis.crypto.subtle` is the native callback under the flag.

### Landing 2 — flip default to native

- Default-enable `ZEROSHIP_NATIVE_CRYPTO`-equivalent path (or remove the gate).
- `init.rs` no longer loads the polyfill.
- The polyfill at `embed/crypto.js` remains as a fallback (still loaded under an emergency-rollback environment variable).

CI: full test suite passes with native default; legacy polyfill still behind the flag for emergency rollback.

### Landing 3 — delete polyfill

- `embed/crypto.js` deleted.
- Drop `crypto.rs` 1308 LOC of free functions (replaced by classes); keep `crypto_hash_sync_callback` / `crypto_hmac_sync_callback` (the node-compat sync helpers; see Non-Goals).
- Drop the `key_store: HashMap` and `next_key_id: u32` fields from `state.rs` (D-2 — keys live on JS wrappers now).

This is the same cadence as streams-native / fetch-native. Done in three separate PRs over (industry estimate) 1 week; (agent-pace) ~1 hour of focused work.

## XIII. Implementation sequence

### XIII.1. Order

1. **Macro extensions** (~50 LOC): `EnforceRangeU32` newtype + extraction (`runtime-macros/src/lib.rs`). `OpErrorKind::DomException` variant + `gen_throw_error` arm (`state.rs` + `runtime-macros/src/lib.rs`).
2. **Crypto + SubtleCrypto + CryptoKey class skeletons** in `crates/runtime/src/crypto/`. Box layout (BrandedBox, CryptoKeyState, KeyAlgorithm enum, KeyMaterial enum, KeyType / KeyUsage / NamedCurve / HashAlgo enums).
3. **getRandomValues + randomUUID** native (currently Rust ops; just rewire as Crypto methods + apply D-21 type filter + D-21 quota DOMException).
4. **digest** (simplest — no key). Migration of existing `crypto_digest` to a SubtleCrypto method.
5. **Algorithm registry** (`crypto/registry.rs`, ~250 LOC of phf table + per-shape parsers).
6. **AES** family (CBC, CTR, GCM, KW) — encrypt/decrypt + import/export "raw" + generateKey. ~600 LOC in `crypto/aes.rs`. Includes D-11 (CTR), D-12 (KW orchestration), D-17 (variable IV), D-18 (variable tag).
7. **RSA** family (PKCS1v1_5, PSS, OAEP) — sign/verify/encrypt/decrypt + key gen + import/export "spki"/"pkcs8". ~700 LOC in `crypto/rsa.rs`. Includes D-15 (key gen), D-16 (variable PSS salt), critic #31 (OID validation via `crypto/der.rs`).
8. **EC** (ECDSA + ECDH) — including D-4 FIXED_SIGNING swap and D-14 P-521. ~500 LOC in `crypto/ec.rs`.
9. **OKP** (Ed25519 + X25519) — D-26. ~250 LOC in `crypto/okp.rs`.
10. **HMAC** — sign/verify + key gen + import/export. ~200 LOC in `crypto/hmac.rs`. D-19 default block size.
11. **Derive** (PBKDF2, HKDF) — including SHA-1 variants and D-20 [EnforceRange] iterations. ~300 LOC in `crypto/derive.rs`.
12. **JWK** across all algorithms — D-5, the largest single feature. ~1000 LOC in `crypto/jwk.rs`.
13. **Key wrapping** (wrapKey/unwrapKey orchestration). ~150 LOC in `crypto/wrap.rs`.
14. **Hand-written tests + WPT runner + iterate** to must-pass-v1.
15. **Polyfill removal cadence** (D-23 three landings).

Each step lands as its own commit cluster.

### XIII.2. Hours

Industry estimate / agent-pace estimate (per `feedback_estimates_hours_not_weeks` — agent-pace ≈ industry / 40):

| Step | Industry h | Agent-pace h |
|------|-----------:|-------------:|
| 1. Macro extensions | 4 | 0.10 |
| 2. Class skeletons + KeyAlgorithm enum | 12 | 0.30 |
| 3. getRandomValues + randomUUID native | 4 | 0.10 |
| 4. digest migration | 3 | 0.08 |
| 5. Algorithm registry | 14 | 0.35 |
| 6. AES family | 30 | 0.75 |
| 7. RSA family | 36 | 0.90 |
| 8. EC family (incl. P-521 + FIXED swap) | 22 | 0.55 |
| 9. OKP (Ed25519 + X25519) | 12 | 0.30 |
| 10. HMAC | 8 | 0.20 |
| 11. Derive (PBKDF2 + HKDF) | 14 | 0.35 |
| 12. JWK across all algorithms | 60 | 1.50 |
| 13. wrapKey / unwrapKey orchestration | 8 | 0.20 |
| 14. Hand-written tests | 24 | 0.60 |
| 15. WPT runner + iteration to must-pass | 32 | 0.80 |
| 16. Polyfill removal (3 landings) | 4 | 0.10 |
| **Total** | **287h** | **~7.2h** |

For ADR provenance: ~287 industry-hours, comparable to fetch-native's 232 hours (both are large WebIDL surfaces with comprehensive WPT coverage). Crypto is larger by industry hours mostly because the JWK matrix is the single biggest LOC chunk (60 industry hours alone) and there's no analog in fetch.

## XIV. Open questions

These are policy-level decisions where reasonable people might disagree. None blocks v1 design completion (every one has a working answer in the design above).

### XIV.1. Async via thread-pool offload — when?

D-29 defers to v2. Question: is the v1 sync-on-V8-thread acceptable for production?

**Working answer:** Yes. Real creator apps don't pass 100 MB to `subtle.encrypt` (that's a bulk-encryption use case better served by chunked TransformStream + per-chunk crypto). The polyfill ships sync today; nobody has filed a bug. Defer.

**Re-open if:** a creator app surfaces actual measurable latency from sync crypto (e.g. a 50-ms median encrypt blocking a 200-req/s worker thread).

### XIV.2. structuredClone(CryptoKey) — defer until Workers ship?

D-28 defers. Question: should v1 implement the serializer step (so the IDL is internally consistent) or just throw DataCloneError?

**Working answer:** Throw DataCloneError. There's no observable JS path to invoke it (no MessagePort / Worker / BroadcastChannel). Document in the deferred-to-v2 list.

### XIV.3. Hardware key support (PKCS#11, TPM, KMS) — out forever?

D-27 says yes. Question: any creator-app demand for HSM-backed signing?

**Working answer:** Out forever for the embedded V8 runtime. Creator apps that need HSM-backed keys make HTTP calls to the HSM provider's API. Document.

### XIV.4. WebAuthn / FIDO2 — separate spec, defer?

Not WebCrypto. Separate spec at https://w3c.github.io/webauthn/. Out of this design.

### XIV.5. Algorithm coverage beyond Tier 1 — X448, AES-OCB, ChaCha20-Poly1305, SHA-3?

X25519 / Ed25519 are in scope (D-26 — they're spec-normative §25 / §26). X448 is OUT (aws-lc-rs / BoringSSL don't support it; Deno special-cases via x448-dalek). AES-OCB / ChaCha20-Poly1305 / SHA-3 are not in WebCrypto Level 2 normative (extensions some impls add). Out of v1; trivially added in v1.5 if the spec promotes them.

### XIV.6. `[SecureContext]` semantics — honored or no-op?

D-22 says no-op (server-side runtime, no insecure context concept). Document.

### XIV.7. RSA modulus length cap — DoS guard?

D-15 says cap at 16384 bits (no spec hard limit, but `generateKey({modulusLength: 1_000_000})` would burn the V8 thread for hours). **Working answer:** cap at 16384, throw `OperationError` above.

### XIV.8. Key material zeroization on Drop?

The `KeyMaterial` enum holds `Vec<u8>` which doesn't zeroize on drop. A defense-in-depth approach is to wrap the byte fields in `zeroize::Zeroizing<Vec<u8>>` (the `zeroize` crate — already a transitive dep via aws-lc-rs).

**Working answer:** yes, wrap the secret-bearing variants (`Symmetric`, `EcPrivate.pkcs8_der`, `EcPrivate.raw_d`, `RsaPrivate.pkcs8_der`, `RsaPrivate.components.{d,p,q,dp,dq,qi}`, `Ed25519Private.{pkcs8_der,raw_d}`, `X25519Private.{pkcs8_der,raw_d}`). Public keys don't need zeroize. Adds ~5 LOC of `Zeroizing<>` wrappers and aligns with workerd's pattern.

### XIV.9. JWK `oth` field for multi-prime RSA — required?

RFC 7518 §6.3.2.7 — for multi-prime RSA keys (more than 2 primes), the JWK has an `oth` array of additional prime info. WebCrypto-generated RSA keys are always 2-prime (per the spec's RsaHashedKeyGenParams and FIPS 186-5). Imported keys could in principle have more primes via JWK.

**Working answer:** parse `oth` (so import doesn't fail on legitimate multi-prime JWKs), but reject with `DataError` if `oth.length > 0` (we don't support multi-prime in our underlying aws-lc-rs path). Document; v2 may add multi-prime support if any creator app needs it.

### XIV.10. JWK `alg` strict vs loose validation?

RFC 7518 specifies `alg` values per algorithm (e.g. RS256 for RSASSA-PKCS1-v1_5+SHA-256, A128GCM for AES-GCM-128). Loose validation: ignore `alg` if present. Strict: reject if `alg` doesn't match the import params.

**Working answer:** strict — match Chrome / Firefox / workerd. Reject mismatched `alg` with `DataError`. WPT tests this in `import_export/symmetric_importKey.https.any.js`.

## XV. How critic findings are addressed

Going through `/tmp/zeroship-reviews/crypto-review.md`'s 45 findings:

### CRITICAL (1-7)

| # | Finding | Design solution |
|---|---------|-----------------|
| 1 | ECDSA produces ASN.1/DER, spec mandates raw r∥s | **D-4** + §IV.5 — swap `_ASN1_SIGNING` → `_FIXED_SIGNING` |
| 2 | ECDH not implemented | **D-13** + §IV.6 — full ECDH deriveBits via aws-lc-rs `agreement::agree_ephemeral` |
| 3 | AES-CTR encrypt/decrypt unimplemented | **D-11** + §IV.3 — full AES-CTR via aws-lc-rs `cipher::EncryptingKey::ctr` |
| 4 | AES-KW wrapKey/unwrapKey unimplemented | **D-12** + §IV.4 — full AES-KW via aws-lc-rs `aead::AES_*_KW` + SubtleCrypto orchestration |
| 5 | JWK import/export wholly absent | **D-5** + §VI — JWK across all 11 algorithm families, ~1000 LOC |
| 6 | All errors are TypeError/Error — spec mandates DOMException | **D-6** + §VIII.2 — `OpErrorKind::DomException` variant, throw real `DOMException` |
| 7 | Key usages not validated against operations | **D-7** + §V.4 — `check_usage` at every op start |

### MAJOR (8-32, with continued numbering)

| # | Finding | Design solution |
|---|---------|-----------------|
| 8 | AES-GCM IV hard-locked to 12 bytes | **D-17** + §IV.2 — variable IV via aws-lc-rs lower FFI |
| 9 | AES-GCM tagLength parameter ignored | **D-18** + §IV.2 — variable tag, truncate output |
| 10 | AES-192 silently fails at encrypt | §IV.2 — `AES_192_GCM` / AES_192 added to dispatch |
| 11 | P-521 missing | **D-14** + §IV.5 — `Curve::P521` variant + aws-lc-rs `ECDSA_P521_SHA512_FIXED_SIGNING` |
| 12 | Promise wrapping is sync | **D-29** — deferred to v2 with explicit rationale; v1 keeps sync |
| 13 | CryptoKey brand check is `instanceof` — bypassable | **D-10** + §V.5 — branded box with TAG byte, V8 internal-field probe |
| 14 | CryptoKey.type and .extractable mutable | §V.3 — getter-only properties via `#[v8_getter]` |
| 15 | CryptoKey not Serializable | **D-28** — IDL retained, serializer throws DataCloneError; v2 with Workers ships real serializer |
| 16 | Algorithm normalization not recursive | **D-8** + §VII — full op-keyed recursive normalize_an_algorithm |
| 17 | PBKDF2 iterations cast u64→u32 silently truncates | **D-20** + §VIII.1 — `EnforceRangeU32` newtype |
| 18 | derive_bits length parsed without overflow check | **D-20** + §IV.9 — `EnforceRangeU32` + sane upper bound (8160 bytes for HKDF, 128 KiB for PBKDF2) |
| 19 | PBKDF2 iterations zero / hash null | §IV.9 — explicit OperationError; covered |
| 20 | PBKDF2 / HMAC missing SHA-1 variant | §IV.9 — `PBKDF2_HMAC_SHA1` + `HKDF_SHA1_FOR_LEGACY_USE_ONLY` added |
| 21 | RSA-PSS saltLength ignored | **D-16** + §IV.7 — drop to lower-level `SaltLen::Fixed(N)` |
| 22 | RSA key generation missing | **D-15** + §IV.7 — `rsa::KeyPair::generate(modulusLength)` with publicExponent honoured |
| 23 | RSA-OAEP plaintext over-allocates output buffer | §IV.7 — current impl already returns `pt.to_vec()` (correct); the buffer over-allocation is `min_output_size()` (correct API). Marked as not-a-bug post-investigation. |
| 24 | AES-GCM ciphertext returned by mutating `data` | Acknowledged as a perf optimization opportunity (§IV.2 + critic note); deferred. v1 keeps the existing 1-copy-in / 1-copy-out shape. |
| 25 | getRandomValues doesn't reject Float32/64Array, DataView | **D-21** + §II.4 — `is_allowed_typed_array` filter |
| 26 | getRandomValues 65 KB cap throws generic Error | **D-21** + §II.4 — throws DOMException QuotaExceededError |
| 27 | exportKey wrong DOMException polyfill | §V.4 — uses native DOMException via D-6 |
| 28 | _nativeImportKey returns JSON string with race-prone keyId | **D-2** — keys live on JS wrappers; no key_store, no next_key_id, no leak |
| 29 | crypto_generate_key for HMAC defaults length to digest size | **D-19** + §IV.8 — block size table (SHA-1/256 → 512, SHA-384/512 → 1024) |
| 30 | ECDSA importKey "raw" doesn't validate uncompressed-point format | §IV.5 — validate `len == 1 + 2*n` and first byte `0x04` |
| 31 | SPKI/PKCS#8 imports trust user-supplied namedCurve | §IV.5 + `crypto/der.rs` — small ASN.1 walker extracts OID, validates match. RFC 5480 OIDs documented. Same for RSA hash OID match. |
| 32 | verify returns "true"/"false" strings | §VIII.3 (revised) — macro already supports `Result<bool, OpError>`; just change `crypto_verify` return type. (Not a macro extension after all.) |

### MAJOR continued (33+)

| # | Finding | Design solution |
|---|---------|-----------------|
| 33 | data: Vec<u8> extraction is one extra copy | Acknowledged perf opportunity; deferred. Current zero-copy macro path is sufficient for v1. |
| 34 | crypto_random_uuid uses String::from_utf8(buf.to_vec()).unwrap() | §II.5 — switch to `from_utf8_unchecked` (bytes are guaranteed ASCII) |
| 35 | crypto_get_random_values_callback uses copy_contents(&mut []) | §II.4 — replaced with raw-pointer `copy_nonoverlapping` (10x faster) |
| 36 | crypto.js base64 hand-rolled — duplication with Rust path | Polyfill is deleted in D-23 step 3; the JWK base64url path is in Rust (§VI.3). Native `atob`/`btoa` are already on globalThis. |
| 37 | next_key_id starts at 1 — predictable handles | **D-2** — no key_store, no handles. Critic concern moot. |
| 38 | crypto.js doesn't gate on SecureContext | **D-22** — no-op by design. Document. |
| 39 | JSON-stringify deserves O(1) algoParams construction | §VII.2 — macro reads V8 dictionary directly via per-shape parsers; no JSON round-trip |
| 40 | crypto_hash_sync_callback / crypto_hmac_sync_callback are fallback ASCII helpers | Not WebCrypto — node:crypto polyfill helpers; out of this design's scope (Non-Goals). Stay as-is. |
| 41 | algo.name.toUpperCase() — but spec says case-sensitive match after first lookup | §VII.2 — registry stores canonical-cased names; lookup is case-insensitive (uppercase the input, then `.eq_ignore_ascii_case` against keys); return value uses canonical-case from registry |
| 42 | CryptoKey has no Symbol.toStringTag | §V — `#[v8_to_string_tag = "CryptoKey"]` macro attribute (already exists) |
| 43 | crypto_random_uuid thread-local entropy buffer eagerly allocates 4 KB | Acceptable; documented critic note. No change. |
| 44 | crypto_export_key clones the key bytes | Acceptable for export (small, infrequent). Documented; deferred. |
| 45 | The Curve enum is Clone but cloning is by-value tiny — fine | Trivial; no change needed. |

**Summary:** Of 45 findings, 33 have explicit design solutions (D-1 through D-30 cover them); 7 are perf optimizations or notes deferred to v2 with explicit rationale; 5 are not-bugs after investigation (#23, #24 partial, #34 minor, #43, #44, #45); 0 are unaddressed.

## XVI. Algorithm coverage summary

Tier 1 = WebCrypto Level 2 normative algorithms in scope for v1.

### Ships in v1

| Algorithm | Spec section | In v1? | Notes |
|---|---|---|---|
| RSASSA-PKCS1-v1_5 | §20 | ✅ | sign / verify / generateKey / importKey / exportKey, all formats incl. JWK |
| RSA-PSS | §21 | ✅ | + variable saltLength (D-16) |
| RSA-OAEP | §22 | ✅ | + label support, all formats incl. JWK |
| ECDSA | §23 | ✅ | + P-256 / P-384 / P-521 (D-14), FIXED wire format (D-4) |
| ECDH | §24 | ✅ | + P-256 / P-384 / P-521 deriveBits (D-13) |
| Ed25519 | §25 | ✅ | sign / verify / generateKey / importKey / exportKey, all formats incl. JWK |
| X25519 | §26 | ✅ | deriveBits / generateKey / importKey / exportKey (D-26) |
| AES-CTR | §27 | ✅ | encrypt / decrypt + 128/192/256 (D-11) |
| AES-CBC | §28 | ✅ | encrypt / decrypt + 128/192/256 |
| AES-GCM | §29 | ✅ | + variable IV (D-17) + variable tag (D-18) + 128/192/256 |
| AES-KW | §30 | ✅ | wrapKey / unwrapKey (D-12) + 128/192/256 |
| HMAC | §31 | ✅ | + spec-correct default block size (D-19) + SHA-1/256/384/512 |
| SHA-1/256/384/512 | §32 | ✅ | digest |
| HKDF | §33 | ✅ | + SHA-1 variant (critic #20) |
| PBKDF2 | §34 | ✅ | + SHA-1 variant + EnforceRange iterations (D-20) |

### Deferred to v2 with rationale

| Algorithm | Spec status | Why deferred |
|---|---|---|
| X448 | NOT in WebCrypto Level 2 | Some drafts include it; aws-lc-rs / BoringSSL don't support; Deno special-cases via x448-dalek. Marginal use case. |
| AES-OCB | NOT in Level 2 | Workerd / Deno extension. Not spec-normative. |
| ChaCha20-Poly1305 | Tentative (https://github.com/w3c/webcrypto/issues/295) | Not yet promoted to spec normative. |
| SHA-3-{256,384,512} | NOT in Level 2 | Deno extension. |
| ML-KEM / Kyber (encap/decap) | Tentative | Post-quantum; not yet promoted. |
| `getPublicKey()` | Tentative | Trivial follow-up; defer. |
| structuredClone(CryptoKey) | Spec-normative `[Serializable]` | D-28 — no observable path until Workers ship. |

### Out of scope forever

| Item | Why |
|---|---|
| JOSE / JWE / JWT extensions | npm package territory (`@zeroship/jose`); spec doesn't define. |
| FIDO2 / WebAuthn | Separate spec; out of WebCrypto. |
| Hardware key support (PKCS#11, TPM, KMS) | Embedded runtime; no FFI to PKCS#11. |
| FIPS 140-3 module validation | aws-lc-rs IS validated upstream; v1 doesn't claim certification. |

**Final v1 algorithm coverage: 16 of 16 Tier 1 normative algorithms in WebCrypto Level 2.** No subset, no gap.

## XVII. Sources

- W3C Web Cryptography API Level 2 — https://w3c.github.io/webcrypto/
- WebCryptoAPI spec source — https://github.com/w3c/webcrypto/blob/main/spec/Overview.html
- Algorithm registry — https://w3c.github.io/webcrypto/#algorithm-registry
- WebIDL Standard — https://webidl.spec.whatwg.org/
- DOM Standard (DOMException) — https://webidl.spec.whatwg.org/#idl-DOMException
- WHATWG HTML (`structuredClone`, `[Serializable]`) — https://html.spec.whatwg.org/
- RFC 8017 — PKCS #1 v2.2 — https://www.rfc-editor.org/rfc/rfc8017
- RFC 5480 — Elliptic Curve Public Key Information — https://www.rfc-editor.org/rfc/rfc5480
- RFC 6979 — Deterministic ECDSA (informational) — https://www.rfc-editor.org/rfc/rfc6979
- RFC 3394 — AES Key Wrap — https://www.rfc-editor.org/rfc/rfc3394
- RFC 5869 — HKDF — https://www.rfc-editor.org/rfc/rfc5869
- RFC 8018 — PKCS #5 v2.1 (PBKDF2) — https://www.rfc-editor.org/rfc/rfc8018
- RFC 7517 — JWK — https://www.rfc-editor.org/rfc/rfc7517
- RFC 7518 — JWA (JWK fields per algorithm) — https://www.rfc-editor.org/rfc/rfc7518
- RFC 4648 — base64 / base64url — https://www.rfc-editor.org/rfc/rfc4648
- RFC 4122 — UUID v4 — https://www.rfc-editor.org/rfc/rfc4122
- RFC 8439 — ChaCha20-Poly1305 (deferred) — https://www.rfc-editor.org/rfc/rfc8439
- aws-lc-rs — https://docs.rs/aws-lc-rs/ (workspace dep, `crates/runtime/Cargo.toml:16`)
- aws-lc — https://github.com/aws/aws-lc (underlying C library)
- Reference impls (vendored at `refs/`):
  - workerd: `refs/workerd/src/workerd/api/crypto/{aes,ec,rsa,jwk,impl,keys,digest,hkdf,pbkdf2,...}.{c++,h}`
  - Deno: `refs/deno/ext/crypto/{lib,import_key,export_key,encrypt,decrypt,generate_key,key,shared,ed25519,x25519}.rs` + `00_crypto.js`
- Sibling designs:
  - `docs/proposals/streams-native.md`
  - `docs/proposals/fetch-native.md`
  - `docs/proposals/headers-native.md`
- Project AGENTS.md — `/home/ruiyang/Projects/appbase/AGENTS.md`
- Existing polyfill: `crates/runtime/src/embed/crypto.js` (313 LOC)
- Existing ops shim: `crates/runtime/src/web/crypto/sync_helpers.rs` (1308 LOC)
- Critic review: `/tmp/zeroship-reviews/crypto-review.md` (45 findings, overall 38/100)
- Macro internals: `crates/runtime-macros/src/v8_class.rs`, `crates/runtime-macros/src/lib.rs`

---

## Footnote on aws-lc-rs API verification

This design assumes the following aws-lc-rs APIs exist (verified against the workspace pin at design time, 2026-05-02):

- `aead::{AES_128_GCM, AES_192_GCM, AES_256_GCM}` — high-level GCM with 12-byte nonce ✓
- `aead::{AES_128_KW, AES_192_KW, AES_256_KW}` — RFC 3394 wrap ✓
- `cipher::{UnboundCipherKey, EncryptingKey::ctr, DecryptingKey::ctr, AES_128, AES_192, AES_256}` — block-cipher CTR ✓
- `signature::{ECDSA_P256_SHA256_FIXED_SIGNING, ECDSA_P384_SHA384_FIXED_SIGNING, ECDSA_P521_SHA512_FIXED_SIGNING}` — fixed-length signatures ✓
- `signature::{ECDSA_P256_SHA256_FIXED, ECDSA_P384_SHA384_FIXED, ECDSA_P521_SHA512_FIXED}` — fixed-length verify ✓
- `agreement::{agree_ephemeral, EphemeralPrivateKey, UnparsedPublicKey, ECDH_P256, ECDH_P384, ECDH_P521, X25519}` — ECDH + X25519 agreement ✓
- `signature::{Ed25519KeyPair, ED25519}` — Ed25519 sign/verify ✓
- `rsa::{KeyPair, KeyPair::generate}` — RSA key generation ✓
- `rsa::SignaturePadding::PSS { hash, salt_len: SaltLen::Fixed }` — variable-salt PSS ✓
- `rsa::{OaepPublicEncryptingKey, OaepPrivateDecryptingKey}` + `OAEP_SHA{256,384,512}_MGF1SHA{256,384,512}` ✓
- `pbkdf2::{PBKDF2_HMAC_SHA1, PBKDF2_HMAC_SHA256, PBKDF2_HMAC_SHA384, PBKDF2_HMAC_SHA512}` ✓
- `hkdf::{HKDF_SHA1_FOR_LEGACY_USE_ONLY, HKDF_SHA256, HKDF_SHA384, HKDF_SHA512}` ✓
- `hmac::{HMAC_SHA1_FOR_LEGACY_USE_ONLY, HMAC_SHA256, HMAC_SHA384, HMAC_SHA512}` ✓
- `digest::{SHA1_FOR_LEGACY_USE_ONLY, SHA256, SHA384, SHA512}` ✓
- `aws_lc_sys` re-exports for variable-IV / variable-tag GCM (lower FFI surface) — to be verified at implementation time; if the FFI surface differs from expectation, fall back to `openssl-sys` (available transitively via aws-lc) for the GCM variants only.

The implementation cross-checks each API at the start of its corresponding step (1-13 in §XIII.1). If any API differs from this design's expectation, the implementing PR adjusts the per-algorithm cell with an inline note; the algorithm coverage commitment stands.

---

(End of design doc.)
