//! Streaming HMAC.
//!
//! See `docs/proposals/node-crypto-native.md` §V.3. Wraps
//! `aws_lc_rs::hmac::Context` for incremental update; `hmac_one_shot`
//! is the WebCrypto path.
//!
//! Note: Node silently accepts empty
//! HMAC keys (special-cases `key_len == 0` by re-binding `key = ""`
//! and forwards to HMAC_Init_ex per `crypto_hmac.cc:78-91`). We match
//! Node — empty key is permitted here. The defense-in-depth check
//! at the WebCrypto surface (`hmac::validate_hmac_usages`) is
//! intentionally NOT applied to node:crypto's surface (XVII.13b — a
//! zeroship-vs-Node divergence we deliberately avoid in the
//! node:crypto path for npm parity).

#![allow(dead_code)]

use super::digest::KernelHashAlgo;
use super::error::KernelError;
use aws_lc_rs::hmac as lc_hmac;

fn aws_alg(algo: KernelHashAlgo) -> Result<lc_hmac::Algorithm, KernelError> {
    match algo {
        KernelHashAlgo::Sha1 => Ok(lc_hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY),
        KernelHashAlgo::Sha224 => Ok(lc_hmac::HMAC_SHA224),
        KernelHashAlgo::Sha256 => Ok(lc_hmac::HMAC_SHA256),
        KernelHashAlgo::Sha384 => Ok(lc_hmac::HMAC_SHA384),
        KernelHashAlgo::Sha512 => Ok(lc_hmac::HMAC_SHA512),
        // SHA-512/256 / SHA3-* are NOT in aws-lc-rs's hmac surface
        // (verified per docs.rs); npm packages don't use them with
        // HMAC in practice, but we surface a clean error.
        _ => Err(KernelError::UnsupportedAlgorithm(format!(
            "HMAC over {} is not supported (aws-lc-rs limitation)",
            algo.node_name()
        ))),
    }
}

/// Streaming HMAC. Node has Hmac.update + digest, but no
/// `Hmac.copy()` (Hash has copy; Hmac does not — quirk of OpenSSL
/// EVP_MD_CTX vs HMAC_CTX). We match.
pub struct HmacContext {
    inner: lc_hmac::Context,
    finalised: bool,
    algo: KernelHashAlgo,
}

impl HmacContext {
    pub fn new(algo: KernelHashAlgo, key: &[u8]) -> Result<Self, KernelError> {
        let alg = aws_alg(algo)?;
        // aws-lc-rs's hmac::Key::new accepts empty keys; matches Node
        // Match Node's empty-key behavior.
        let key = lc_hmac::Key::new(alg, key);
        let inner = lc_hmac::Context::with_key(&key);
        Ok(Self {
            inner,
            finalised: false,
            algo,
        })
    }

    pub fn algo(&self) -> KernelHashAlgo {
        self.algo
    }

    pub fn update(&mut self, data: &[u8]) -> Result<(), KernelError> {
        if self.finalised {
            return Err(KernelError::HmacFinalised);
        }
        self.inner.update(data);
        Ok(())
    }

    pub fn finalize(&mut self) -> Result<Vec<u8>, KernelError> {
        if self.finalised {
            return Err(KernelError::HmacFinalised);
        }
        self.finalised = true;
        // Cloning is cheap and gives us exclusive ownership for sign().
        let snapshot = self.inner.clone();
        Ok(snapshot.sign().as_ref().to_vec())
    }
}

/// One-shot HMAC compute. Used by WebCrypto's `subtle.sign({HMAC},...)`.
pub fn hmac_one_shot(
    algo: KernelHashAlgo,
    key: &[u8],
    data: &[u8],
) -> Result<Vec<u8>, KernelError> {
    let alg = aws_alg(algo)?;
    let key = lc_hmac::Key::new(alg, key);
    let tag = lc_hmac::sign(&key, data);
    Ok(tag.as_ref().to_vec())
}

/// One-shot HMAC verify. Constant-time.
pub fn hmac_verify_one_shot(
    algo: KernelHashAlgo,
    key: &[u8],
    data: &[u8],
    tag: &[u8],
) -> Result<bool, KernelError> {
    let alg = aws_alg(algo)?;
    let key = lc_hmac::Key::new(alg, key);
    Ok(lc_hmac::verify(&key, data, tag).is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_streaming_matches_one_shot() {
        let key = b"secret-key";
        let mut ctx = HmacContext::new(KernelHashAlgo::Sha256, key).unwrap();
        ctx.update(b"hello ").unwrap();
        ctx.update(b"world").unwrap();
        let streaming = ctx.finalize().unwrap();
        let one_shot = hmac_one_shot(KernelHashAlgo::Sha256, key, b"hello world").unwrap();
        assert_eq!(streaming, one_shot);
    }

    #[test]
    fn empty_key_works() {
        let result = HmacContext::new(KernelHashAlgo::Sha256, &[]);
        assert!(result.is_ok());
    }

    #[test]
    fn finalize_twice_errors() {
        let mut ctx = HmacContext::new(KernelHashAlgo::Sha256, b"k").unwrap();
        ctx.update(b"d").unwrap();
        ctx.finalize().unwrap();
        let err = ctx.finalize().unwrap_err();
        assert!(matches!(err, KernelError::HmacFinalised));
    }
}
