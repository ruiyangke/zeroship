//! Streaming + one-shot digest contexts.
//!
//! See `docs/proposals/node-crypto-native.md` §I.5. The kernel's
//! `DigestContext` wraps `aws_lc_rs::digest::Context`
//! incrementally; the one-shot helper is what WebCrypto's
//! `subtle.digest()` calls. Stage A: SHA-1/256/384/512 (the four
//! hashes in WebCrypto), plus the broader Node hash family
//! (SHA-224, SHA-512/256, SHA-3-256/384/512) for `createHash()`.
//! MD5 + SHA-512/224 ship via FFI under §III.2a in a follow-up.

#![allow(dead_code)]

use super::error::KernelError;
use aws_lc_rs::digest as lc_digest;

/// All hash algorithms node:crypto exposes via `createHash` that we
/// support natively in Stage B (no FFI required).
///
/// MD5 + SHA-512/224 are in the design for Stage B but require
/// aws-lc-sys raw FFI per §III.2a; not yet wired here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KernelHashAlgo {
    Sha1,
    Sha224,
    Sha256,
    Sha384,
    Sha512,
    Sha512_256,
    Sha3_256,
    Sha3_384,
    Sha3_512,
}

impl KernelHashAlgo {
    /// Map a Node-style or WebCrypto-style algorithm name to the
    /// kernel enum. Case-insensitive (Node lowercases names by
    /// convention; WebCrypto uses canonical "SHA-256").
    pub fn from_str(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "sha1" | "sha-1" | "rsa-sha1" => Some(Self::Sha1),
            "sha224" | "sha-224" | "rsa-sha224" => Some(Self::Sha224),
            "sha256" | "sha-256" | "rsa-sha256" => Some(Self::Sha256),
            "sha384" | "sha-384" | "rsa-sha384" => Some(Self::Sha384),
            "sha512" | "sha-512" | "rsa-sha512" => Some(Self::Sha512),
            "sha512-256" | "sha-512-256" | "sha512_256" => Some(Self::Sha512_256),
            "sha3-256" | "sha3_256" => Some(Self::Sha3_256),
            "sha3-384" | "sha3_384" => Some(Self::Sha3_384),
            "sha3-512" | "sha3_512" => Some(Self::Sha3_512),
            _ => None,
        }
    }

    /// Canonical Node-style lowercase name for `getHashes()` listing.
    pub fn node_name(self) -> &'static str {
        match self {
            Self::Sha1 => "sha1",
            Self::Sha224 => "sha224",
            Self::Sha256 => "sha256",
            Self::Sha384 => "sha384",
            Self::Sha512 => "sha512",
            Self::Sha512_256 => "sha512-256",
            Self::Sha3_256 => "sha3-256",
            Self::Sha3_384 => "sha3-384",
            Self::Sha3_512 => "sha3-512",
        }
    }

    /// Output size in bytes.
    pub fn digest_len(self) -> usize {
        match self {
            Self::Sha1 => 20,
            Self::Sha224 => 28,
            Self::Sha256 | Self::Sha512_256 | Self::Sha3_256 => 32,
            Self::Sha384 | Self::Sha3_384 => 48,
            Self::Sha512 | Self::Sha3_512 => 64,
        }
    }

    fn aws_alg(self) -> &'static lc_digest::Algorithm {
        match self {
            Self::Sha1 => &lc_digest::SHA1_FOR_LEGACY_USE_ONLY,
            Self::Sha224 => &lc_digest::SHA224,
            Self::Sha256 => &lc_digest::SHA256,
            Self::Sha384 => &lc_digest::SHA384,
            Self::Sha512 => &lc_digest::SHA512,
            Self::Sha512_256 => &lc_digest::SHA512_256,
            Self::Sha3_256 => &lc_digest::SHA3_256,
            Self::Sha3_384 => &lc_digest::SHA3_384,
            Self::Sha3_512 => &lc_digest::SHA3_512,
        }
    }
}

/// Static list of every algorithm name `getHashes()` returns. Includes
/// the canonical Node-style spellings that npm packages pass to
/// `createHash`. Per the design doc §IX.1 (algorithm registry). The
/// caller can layer aliases (e.g. "sha-256") on top by passing
/// through `KernelHashAlgo::from_str`.
pub const HASH_NAMES: &[&str] = &[
    "sha1",
    "sha224",
    "sha256",
    "sha384",
    "sha512",
    "sha512-256",
    "sha3-256",
    "sha3-384",
    "sha3-512",
];

/// Streaming digest. Wraps aws-lc-rs's `digest::Context`.
///
/// Once `finalize()` runs the context refuses further `update()` and
/// further `finalize()` per Node parity (`ERR_CRYPTO_HASH_FINALIZED`).
pub struct DigestContext {
    inner: lc_digest::Context,
    finalised: bool,
    algo: KernelHashAlgo,
}

impl DigestContext {
    pub fn new(algo: KernelHashAlgo) -> Self {
        Self {
            inner: lc_digest::Context::new(algo.aws_alg()),
            finalised: false,
            algo,
        }
    }

    pub fn algo(&self) -> KernelHashAlgo {
        self.algo
    }

    pub fn update(&mut self, data: &[u8]) -> Result<(), KernelError> {
        if self.finalised {
            return Err(KernelError::HashFinalised);
        }
        self.inner.update(data);
        Ok(())
    }

    /// Consume the context, return the digest bytes.
    pub fn finalize(&mut self) -> Result<Vec<u8>, KernelError> {
        if self.finalised {
            return Err(KernelError::HashFinalised);
        }
        self.finalised = true;
        // aws_lc_rs::digest::Context::finish consumes self. We hold a
        // mutable ref, so clone the in-progress state first (mirror's
        // workerd's pattern). The clone cost is negligible (~50 ns).
        let snapshot = self.inner.clone();
        Ok(snapshot.finish().as_ref().to_vec())
    }

    /// Clone the in-progress state for `Hash.copy()`.
    pub fn clone_state(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            finalised: self.finalised,
            algo: self.algo,
        }
    }
}

/// One-shot digest: WebCrypto's `subtle.digest()` calls this.
pub fn digest_one_shot(algo: KernelHashAlgo, data: &[u8]) -> Vec<u8> {
    lc_digest::digest(algo.aws_alg(), data).as_ref().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_streaming_matches_one_shot() {
        let mut ctx = DigestContext::new(KernelHashAlgo::Sha256);
        ctx.update(b"hello ").unwrap();
        ctx.update(b"world").unwrap();
        let streaming = ctx.finalize().unwrap();
        let one_shot = digest_one_shot(KernelHashAlgo::Sha256, b"hello world");
        assert_eq!(streaming, one_shot);
    }

    #[test]
    fn finalize_twice_errors() {
        let mut ctx = DigestContext::new(KernelHashAlgo::Sha256);
        ctx.update(b"data").unwrap();
        ctx.finalize().unwrap();
        let err = ctx.finalize().unwrap_err();
        assert!(matches!(err, KernelError::HashFinalised));
    }

    #[test]
    fn update_after_finalize_errors() {
        let mut ctx = DigestContext::new(KernelHashAlgo::Sha256);
        ctx.update(b"a").unwrap();
        ctx.finalize().unwrap();
        let err = ctx.update(b"b").unwrap_err();
        assert!(matches!(err, KernelError::HashFinalised));
    }

    #[test]
    fn copy_preserves_state() {
        let mut a = DigestContext::new(KernelHashAlgo::Sha256);
        a.update(b"prefix-").unwrap();
        let mut b = a.clone_state();
        a.update(b"suffix-A").unwrap();
        b.update(b"suffix-B").unwrap();
        let da = a.finalize().unwrap();
        let db = b.finalize().unwrap();
        assert_ne!(da, db);
        let one_a = digest_one_shot(KernelHashAlgo::Sha256, b"prefix-suffix-A");
        assert_eq!(da, one_a);
        let one_b = digest_one_shot(KernelHashAlgo::Sha256, b"prefix-suffix-B");
        assert_eq!(db, one_b);
    }

    fn hex(bytes: &[u8]) -> String {
        let mut s = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            s.push_str(&format!("{:02x}", b));
        }
        s
    }

    #[test]
    fn sha224_works() {
        let h = digest_one_shot(KernelHashAlgo::Sha224, b"abc");
        // RFC 3874 test vector for SHA-224("abc")
        assert_eq!(
            hex(&h),
            "23097d223405d8228642a477bda255b32aadbce4bda0b3f7e36c9da7"
        );
    }

    #[test]
    fn sha3_256_works() {
        let h = digest_one_shot(KernelHashAlgo::Sha3_256, b"abc");
        // FIPS 202 test vector for SHA3-256("abc")
        assert_eq!(
            hex(&h),
            "3a985da74fe225b2045c172d6bd390bd855f086e3e9d525b46bfe24511431532"
        );
    }
}
