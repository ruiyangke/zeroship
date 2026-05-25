# Native W3C Web Cryptography API shipped

**Status:** Shipped 2026-05-02
**Long-form design:** [`docs/proposals/webcrypto-native.md`](../archive/webcrypto-native.md)
**Implementation:** [`crates/runtime/src/web/crypto/`](../../crates/runtime/src/web/crypto/) (~8,200 LOC: aes, ec, rsa, okp, hmac, digest, derive, jwk, subtle, …)

## Context

The runtime carried a 313-LOC JS polyfill at `embed/crypto.js` plus
a 1308-LOC Rust ops shim. The implementation emitted
**ASN.1/DER ECDSA signatures** (cross-stack incompatible with
Chrome/Firefox/Node WebCrypto), rejected JWK wholesale (~90% of
npm-published JWE/JWT packages broken), conflated all errors to
`TypeError`/generic `Error` (14 of 45 critic findings), leaked
keys forever via a `HashMap<u32, KeyData>` keystore, and hand-rolled
algorithm normalization as a single uppercase pass. We ran zero of
WPT `WebCryptoAPI/`.

## Decision

- Full WebCrypto Level 2 compliance in one tier (no v1/v2 algorithm split).
- Pure-native `Crypto` / `SubtleCrypto` / `CryptoKey` `#[v8_class]` types backed by **aws-lc-rs** (workspace dep, single underlying provider).
- ECDSA produces and verifies fixed-length r∥s signatures (not ASN.1/DER).
- JWK round-trip for every algorithm.
- Spec-faithful algorithm normalization: full table from spec §§20-34 (16 algorithms × 11 operations), recursive dispatch for nested `HashAlgorithmIdentifier` / `AlgorithmIdentifier`.
- Spec-correct AES-KW (RFC 3394), AES-GCM variable IV + variable tag, RSA-PSS variable salt length, RSA-OAEP.
- Typed `DOMException` variants (OperationError, DataError, InvalidAccessError, NotSupportedError, …).
- `CryptoKey` carries key material via V8 internal-field `Box<CryptoKeyState>` — dropped on GC. No more leak.
- Brand check via V8 internal field (unspoofable; not `instanceof`).
- X25519/Ed25519 included; FIPS/hardware-key out forever; structuredClone deferred.
- Cutover via three D-23 landings: native module → flip default → delete `embed/crypto.js`.

## Consequences

- Every JOSE/JWE/JWT npm library now works (`jose`, `panva/jose`, `node-jose`, etc.).
- Stripe Connect / OAuth 2.0 / OIDC RS256/ES256/EdDSA verifies interop with Chrome/Firefox-issued signatures.
- WPT `WebCryptoAPI/` runnable for the first time.
- Deletion of `embed/crypto.js` (313 LOC) and the 1308-LOC ops shim.
- Post-ship perf: `Crypto.subtle` → `#[v8_getter(same_object)]` (`b32066c`); crypto enums → `#[derive(WebIdlEnum)]` (`36fa683`).
- node-crypto-native builds on this kernel (D-N4): `KeyObject` shares the underlying `KeyMaterial` enum.

## See also

- Implementing commits: `cdac86f` native WebCrypto module — Crypto/SubtleCrypto/CryptoKey; `cfec7b8` flip default to native WebCrypto (D-23 landing 2); `2d19711` delete embed/crypto.js (D-23 landing 3); `49735ff` drop 10 dead `__cryptoXxx` ops + key_store.
- Related ADRs: [node-crypto-native](./2026-05-05-node-crypto-native.md).
