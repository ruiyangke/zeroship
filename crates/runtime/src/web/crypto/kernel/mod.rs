//! Crypto kernel — pure-Rust slice-in/Vec-out primitives.
//!
//! Per `docs/proposals/node-crypto-native.md` §I.1 (D-N1, D-N2). The
//! kernel owns digest / HMAC / cipher / sign-verify / KDF state
//! machines. Both surfaces (`web::crypto` for WebCrypto and the new
//! `web::crypto_node` for node:crypto) are thin V8 adapters over the
//! kernel — they meet here at slice-shaped boundaries, never call each
//! other.
//!
//! The kernel exposes:
//!
//! - `Context`-shaped streaming primitives (`DigestContext`,
//!   `HmacContext`, etc.) — used by node:crypto's `Hash` / `Hmac` /
//!   `Cipher` / `Sign` classes which carry incremental state across
//!   multiple JS-side `update()` calls (D-N2).
//! - One-shot helpers (`digest_one_shot`, `hmac_one_shot`, ...) — used
//!   by WebCrypto's `subtle.digest()` / `subtle.sign()` etc., which
//!   are non-streaming.
//!
//! The two paths share the same underlying state machine. Bug fixes
//! land in one place.

pub mod digest;
pub mod error;
pub mod hmac;
pub mod kdf;

pub use digest::{DigestContext, KernelHashAlgo, digest_one_shot};
pub use error::KernelError;
pub use hmac::{HmacContext, hmac_one_shot};
