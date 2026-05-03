//! Key derivation: PBKDF2 + HKDF.
//!
//! Per `docs/proposals/node-crypto-native.md` §VI.2 (D-N15). scrypt
//! requires aws-lc-sys raw FFI per §III.2a — not yet wired here.

#![allow(dead_code)]

use super::digest::KernelHashAlgo;
use super::error::KernelError;
use std::num::NonZeroU32;

fn pbkdf2_alg(algo: KernelHashAlgo) -> Result<aws_lc_rs::pbkdf2::Algorithm, KernelError> {
    match algo {
        KernelHashAlgo::Sha1 => Ok(aws_lc_rs::pbkdf2::PBKDF2_HMAC_SHA1),
        KernelHashAlgo::Sha256 => Ok(aws_lc_rs::pbkdf2::PBKDF2_HMAC_SHA256),
        KernelHashAlgo::Sha384 => Ok(aws_lc_rs::pbkdf2::PBKDF2_HMAC_SHA384),
        KernelHashAlgo::Sha512 => Ok(aws_lc_rs::pbkdf2::PBKDF2_HMAC_SHA512),
        _ => Err(KernelError::UnsupportedAlgorithm(format!(
            "PBKDF2 over {} is not supported",
            algo.node_name()
        ))),
    }
}

fn hkdf_alg(algo: KernelHashAlgo) -> Result<aws_lc_rs::hkdf::Algorithm, KernelError> {
    match algo {
        KernelHashAlgo::Sha1 => Ok(aws_lc_rs::hkdf::HKDF_SHA1_FOR_LEGACY_USE_ONLY),
        KernelHashAlgo::Sha256 => Ok(aws_lc_rs::hkdf::HKDF_SHA256),
        KernelHashAlgo::Sha384 => Ok(aws_lc_rs::hkdf::HKDF_SHA384),
        KernelHashAlgo::Sha512 => Ok(aws_lc_rs::hkdf::HKDF_SHA512),
        _ => Err(KernelError::UnsupportedAlgorithm(format!(
            "HKDF over {} is not supported",
            algo.node_name()
        ))),
    }
}

/// PBKDF2 — slice-in / Vec-out.
pub fn pbkdf2(
    algo: KernelHashAlgo,
    password: &[u8],
    salt: &[u8],
    iterations: u32,
    out_len: usize,
) -> Result<Vec<u8>, KernelError> {
    let alg = pbkdf2_alg(algo)?;
    let iters = NonZeroU32::new(iterations)
        .ok_or(KernelError::InvalidKdfParams("iterations must be > 0"))?;
    if out_len == 0 {
        return Ok(Vec::new());
    }
    let mut out = vec![0u8; out_len];
    aws_lc_rs::pbkdf2::derive(alg, iters, salt, password, &mut out);
    Ok(out)
}

/// HKDF — Extract + Expand in one call. `salt` may be empty (HKDF
/// uses a zero-length string per RFC 5869 §2.2).
pub fn hkdf(
    algo: KernelHashAlgo,
    ikm: &[u8],
    salt: &[u8],
    info: &[u8],
    out_len: usize,
) -> Result<Vec<u8>, KernelError> {
    let alg = hkdf_alg(algo)?;
    if out_len == 0 {
        return Ok(Vec::new());
    }
    // RFC 5869 limit: L ≤ 255 * HashLen. aws-lc-rs enforces this in
    // `Okm::fill` and panics if exceeded — we pre-check to return a
    // clean kernel error.
    let max_len = 255 * algo.digest_len();
    if out_len > max_len {
        return Err(KernelError::InvalidKdfParams(
            "HKDF output length exceeds 255 * HashLen",
        ));
    }
    let salt_obj = aws_lc_rs::hkdf::Salt::new(alg, salt);
    let prk = salt_obj.extract(ikm);
    let info_array = [info];
    let okm = prk
        .expand(&info_array, OkmLen(out_len))
        .map_err(|_| KernelError::InvalidKdfParams("HKDF expand failed"))?;
    let mut out = vec![0u8; out_len];
    okm.fill(&mut out)
        .map_err(|_| KernelError::InvalidKdfParams("HKDF fill failed"))?;
    Ok(out)
}

/// Helper — `aws_lc_rs::hkdf::KeyType` requires a typed length.
struct OkmLen(usize);
impl aws_lc_rs::hkdf::KeyType for OkmLen {
    fn len(&self) -> usize {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex_decode(s: &str) -> Vec<u8> {
        let bytes = s.as_bytes();
        assert!(bytes.len() % 2 == 0, "hex: odd length");
        let mut out = Vec::with_capacity(bytes.len() / 2);
        for chunk in bytes.chunks(2) {
            let hi = (chunk[0] as char).to_digit(16).unwrap() as u8;
            let lo = (chunk[1] as char).to_digit(16).unwrap() as u8;
            out.push((hi << 4) | lo);
        }
        out
    }

    // RFC 6070 PBKDF2-HMAC-SHA-1 test vector #1.
    #[test]
    fn pbkdf2_sha1_rfc6070_v1() {
        let out = pbkdf2(KernelHashAlgo::Sha1, b"password", b"salt", 1, 20).unwrap();
        let expected = hex_decode("0c60c80f961f0e71f3a9b524af6012062fe037a6");
        assert_eq!(out, expected);
    }

    #[test]
    fn pbkdf2_zero_iterations_rejected() {
        let err =
            pbkdf2(KernelHashAlgo::Sha256, b"p", b"s", 0, 32).unwrap_err();
        assert!(matches!(err, KernelError::InvalidKdfParams(_)));
    }

    // RFC 5869 HKDF-SHA-256 test vector #1.
    #[test]
    fn hkdf_sha256_rfc5869_v1() {
        let ikm = hex_decode("0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b");
        let salt = hex_decode("000102030405060708090a0b0c");
        let info = hex_decode("f0f1f2f3f4f5f6f7f8f9");
        let out = hkdf(KernelHashAlgo::Sha256, &ikm, &salt, &info, 42).unwrap();
        let expected = hex_decode(
            "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865",
        );
        assert_eq!(out, expected);
    }

    #[test]
    fn hkdf_too_long_rejected() {
        let ikm = vec![0u8; 32];
        // SHA-256 max = 255*32 = 8160; ask for 9000.
        let err = hkdf(KernelHashAlgo::Sha256, &ikm, &[], &[], 9000).unwrap_err();
        assert!(matches!(err, KernelError::InvalidKdfParams(_)));
    }
}
