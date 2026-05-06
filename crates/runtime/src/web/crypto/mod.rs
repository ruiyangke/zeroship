//! Native W3C WebCryptoAPI Level 2.
//!
//! Replaces `crates/runtime/src/embed/crypto.js` (313 LOC JS shim) and
//! the algorithm dispatch ops in `crates/runtime/src/crypto.rs`. Lives
//! beside the existing `crypto.rs` (which keeps the
//! `crypto_random_uuid_callback` / `crypto_get_random_values_callback`
//! ops + the `__cryptoHashSync` / `__cryptoHmacSync` node-compat
//! helpers used by the node:crypto polyfill).
//!
//! See `docs/proposals/webcrypto-native.md` for the full design.
//!
//! # Module layout
//!
//! - `key_material` — `CryptoKeyState`, `KeyAlgorithm`, `KeyMaterial`,
//!   `KeyType`, `KeyUsage`, `HashAlgo`, `NamedCurve`, the `BrandedBox`
//!   wrapper that puts a tag byte at offset 0 for brand checks.
//! - `crypto_key` — `CryptoKey` `#[v8_class]` with `[SameObject]`
//!   getter caching for `algorithm` / `usages`, the brand check, and
//!   the `build` helper that allocates a JS wrapper around a state.
//! - `crypto_class` — `Crypto` (the global) with `getRandomValues`,
//!   `randomUUID`, and the `subtle` getter.
//! - `subtle` — `SubtleCrypto` `#[v8_class]` with the 11 spec methods.
//!   Each method runs synchronously on the V8 thread and
//!   wraps the result in a Promise via `helpers::resolve_now`.
//! - `digest` — SHA-1/256/384/512 digest. `helpers::resolve_digest_algorithm`
//!   handles the DOMString / object input shape per spec §32.
//! - `registry` — algorithm name → enum + `(op, name) → supports`
//!   table + recursive HashAlgorithmIdentifier normalization.
//! - `ops` — top-level dispatchers per WebCrypto operation (encrypt,
//!   decrypt, sign, verify, etc.). Looks up the algorithm, validates
//!   usage, and delegates to a per-algorithm module.
//! - `aes` — AES-{CTR,CBC,GCM,KW}, including CTR mode, key wrapping,
//!   variable IVs, and variable tag lengths.
//! - `rsa` — RSASSA-PKCS1-v1_5 / RSA-PSS / RSA-OAEP, including key
//!   generation and variable PSS salt handling.
//! - `ec` — ECDSA + ECDH, including fixed-length `r∥s` encoding,
//!   ECDH `deriveBits`, and P-521 support.
//! - `okp` — Ed25519 + X25519.
//! - `hmac` — HMAC sign/verify + keygen + import/export, including
//!   default block-size key lengths.
//! - `derive` — HKDF + PBKDF2, including `[EnforceRange]` iterations
//!   and SHA-1 variants.
//! - `jwk` — JsonWebKey import/export per algorithm.
//! - `wrap` — wrapKey/unwrapKey orchestration.
//! - `helpers` — shared utilities (BufferSource read, base64url codec,
//!   resolve_now/reject_now, etc.).
//!
//! # Activation
//!
//! Native is the only path. The legacy `embed/crypto.js` polyfill has
//! been deleted; `is_enabled()` is retained as a sentinel function but
//! always returns true.

#![allow(unsafe_code)]

pub mod aes;
pub mod crypto_class;
pub mod crypto_key;
pub mod der;
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
pub mod evp_ffi;
pub mod rsa;
pub mod subtle;
pub mod sync_helpers;
pub mod wrap;

// Back-compat shim: the crypto kernel lives at `crate::base::crypto`
// since it is the dual-surface backend shared by `web::crypto`
// (WebCrypto) and `node::crypto` (node:crypto). Re-export here so
// existing `crate::web::crypto::kernel::*` paths keep resolving until
// callers naturally migrate.
pub use crate::base::crypto as kernel;

/// Install Crypto / SubtleCrypto / CryptoKey on `globalThis`. Replaces
/// the legacy `crypto` ad-hoc op installs in `init.rs` when the
/// feature flag is set.
pub fn install_globals<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    global: v8::Local<v8::Object>,
) {
    // Install the classes first so SubtleCrypto::install / CryptoKey::install
    // are cached per-isolate by the time `crypto.subtle` getter runs.
    // #198 — CryptoKey is bare bind; SubtleCrypto's `install_global`
    // returns its Function for callers that wanted it (no current users
    // — the let _ binding swallows it) so it stays on the wrapper for
    // now; Crypto needs the global `crypto` instance build, also stays.
    crate::register_native_classes!(scope, global, [
        crypto_key::CryptoKey,
    ]);
    let _ = subtle::install_global(scope, global);
    crypto_class::install_global(scope, global);
}

/// Always true because the native implementation is the only path and
/// the polyfill is gone. Retained as a callable so out-of-tree consumers
/// that may still query the gate keep building.
pub fn is_enabled() -> bool {
    true
}
