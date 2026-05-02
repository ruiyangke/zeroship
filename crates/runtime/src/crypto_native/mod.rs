//! Native W3C WebCryptoAPI Level 2.
//!
//! Replaces `crates/runtime/src/embed/crypto.js` (313 LOC JS shim) and
//! the algorithm dispatch ops in `crates/runtime/src/crypto.rs`. Lives
//! beside the existing `crypto.rs` (which keeps the
//! `crypto_random_uuid_callback` / `crypto_get_random_values_callback`
//! ops + the `__cryptoHashSync` / `__cryptoHmacSync` node-compat
//! helpers used by the node:crypto polyfill).
//!
//! Per `docs/proposals/webcrypto-native.md` (D-1 through D-30).
//!
//! # Module layout
//!
//! - `key_material` — `CryptoKeyState`, `KeyAlgorithm`, `KeyMaterial`,
//!   `KeyType`, `KeyUsage`, `HashAlgo`, `NamedCurve`, the `BrandedBox`
//!   wrapper that puts a tag byte at offset 0 (D-10 brand check).
//! - `crypto_key` — `CryptoKey` `#[v8_class]` with `[SameObject]`
//!   getter caching for `algorithm` / `usages`, the brand check, and
//!   the `build` helper that allocates a JS wrapper around a state.
//! - `crypto_class` — `Crypto` (the global) with `getRandomValues`,
//!   `randomUUID`, and the `subtle` getter.
//! - `subtle` — `SubtleCrypto` `#[v8_class]` with the 11 spec methods.
//!   Each method runs synchronously on the V8 thread (D-29 v1) and
//!   wraps the result in a Promise via `helpers::resolve_now`.
//! - `digest` — SHA-1/256/384/512 digest. `helpers::resolve_digest_algorithm`
//!   handles the DOMString / object input shape per spec §32.
//! - `registry` — algorithm name → enum + `(op, name) → supports`
//!   table + recursive HashAlgorithmIdentifier normalization (D-8).
//! - `ops` — top-level dispatchers per WebCrypto operation (encrypt,
//!   decrypt, sign, verify, etc.). Looks up the algorithm, validates
//!   usage, and delegates to a per-algorithm module.
//! - `aes` — AES-{CTR,CBC,GCM,KW}. D-11 CTR, D-12 KW, D-17 variable
//!   IV, D-18 variable tag.
//! - `rsa` — RSASSA-PKCS1-v1_5 / RSA-PSS / RSA-OAEP. D-15 keygen,
//!   D-16 PSS variable salt (rejected with NotSupportedError for v1).
//! - `ec` — ECDSA + ECDH. D-4 fixed-length r∥s wire format, D-13
//!   ECDH deriveBits, D-14 P-521.
//! - `okp` — Ed25519 + X25519. D-26.
//! - `hmac` — HMAC sign/verify + keygen + import/export. D-19 default
//!   block-size key length.
//! - `derive` — HKDF + PBKDF2. D-20 [EnforceRange] iterations + SHA-1
//!   variants.
//! - `jwk` — JsonWebKey import/export per algorithm. D-5.
//! - `wrap` — wrapKey/unwrapKey orchestration. D-12.
//! - `helpers` — shared utilities (BufferSource read, base64url codec,
//!   resolve_now/reject_now, etc.).
//!
//! # Activation
//!
//! Set the env var `ZEROSHIP_NATIVE_CRYPTO=1` to swap the JS polyfill
//! for native (D-23 cadence step 1). Default: polyfill.

#![allow(unsafe_code)]

pub mod aes;
pub mod crypto_class;
pub mod crypto_key;
pub mod derive;
pub mod digest;
pub mod ec;
pub mod helpers;
pub mod hmac;
pub mod jwk;
pub mod key_material;
pub mod okp;
pub mod ops;
pub mod registry;
pub mod rsa;
pub mod subtle;
pub mod wrap;

/// Install Crypto / SubtleCrypto / CryptoKey on `globalThis`. Replaces
/// the legacy `crypto` ad-hoc op installs in `init.rs` when the
/// feature flag is set.
pub fn install_globals<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    global: v8::Local<v8::Object>,
) {
    // Install the classes first so SubtleCrypto::install / CryptoKey::install
    // are cached per-isolate by the time `crypto.subtle` getter runs.
    crypto_key::install_global(scope, global);
    let _ = subtle::install_global(scope, global);
    crypto_class::install_global(scope, global);
}

/// Returns true when the runtime should use the native WebCrypto path
/// instead of the JS polyfill. Currently keyed off the
/// `ZEROSHIP_NATIVE_CRYPTO` env var (D-23 landing 1). When the flag
/// flips to default-on (landing 2), this returns true unconditionally.
pub fn is_enabled() -> bool {
    std::env::var("ZEROSHIP_NATIVE_CRYPTO")
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false)
}
