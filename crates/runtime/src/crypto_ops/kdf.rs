//! Key derivation: PBKDF2 + HKDF + scrypt.
//!
//! Per `docs/proposals/node-crypto-native.md` §VI.2 (D-N15) + §III.2a.
//! scrypt drops to aws-lc-sys raw FFI (`EVP_PBE_scrypt`) because the
//! high-level aws-lc-rs surface doesn't expose it.

#![allow(dead_code, unsafe_code)]

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

// ---------------------------------------------------------------------------
// scrypt (RFC 7914) — aws-lc-sys raw FFI per D-N15 / §III.2a.
// ---------------------------------------------------------------------------

/// scrypt parameter validation per RFC 7914 §6 + Node parity.
fn validate_scrypt_params(n: u64, r: u64, p: u64, max_mem: usize, dklen: usize) -> Result<(), KernelError> {
    if n < 2 {
        return Err(KernelError::InvalidKdfParams("N must be >= 2"));
    }
    if (n & (n - 1)) != 0 {
        return Err(KernelError::InvalidKdfParams("N must be a power of two"));
    }
    if r == 0 {
        return Err(KernelError::InvalidKdfParams("r must be > 0"));
    }
    if p == 0 {
        return Err(KernelError::InvalidKdfParams("p must be > 0"));
    }
    if dklen == 0 {
        return Err(KernelError::InvalidKdfParams("keylen must be > 0"));
    }
    // Memory check: 128 * N * r bytes (the inner array). aws-lc enforces
    // this internally via max_mem; we precheck for clean kernel error.
    let needed = 128u64
        .checked_mul(n)
        .and_then(|x| x.checked_mul(r))
        .ok_or(KernelError::InvalidKdfParams("scrypt memory overflow"))?;
    if needed > max_mem as u64 {
        return Err(KernelError::InvalidKdfParams(
            "scrypt memory exceeds maxmem",
        ));
    }
    Ok(())
}

/// scrypt — slice-in / Vec-out.
///
/// `n` is the CPU/memory cost; `r` is the block size; `p` is the
/// parallelisation parameter; `max_mem` caps the memory the
/// computation may use (bytes; default 32 MiB per Node).
pub fn scrypt(
    password: &[u8],
    salt: &[u8],
    n: u64,
    r: u64,
    p: u64,
    max_mem: usize,
    out_len: usize,
) -> Result<Vec<u8>, KernelError> {
    validate_scrypt_params(n, r, p, max_mem, out_len)?;
    let mut out = vec![0u8; out_len];
    // SAFETY: pointers are non-null + length-correct; aws-lc returns 1
    // on success, 0 on failure (parameter / OOM); the password is
    // borrowed for the duration of the call (no lifetime issues since
    // EVP_PBE_scrypt is sync).
    let rc = unsafe {
        aws_lc_sys::EVP_PBE_scrypt(
            password.as_ptr() as *const std::os::raw::c_char,
            password.len(),
            salt.as_ptr(),
            salt.len(),
            n,
            r,
            p,
            max_mem,
            out.as_mut_ptr(),
            out_len,
        )
    };
    if rc != 1 {
        return Err(KernelError::OperationFailed("scrypt computation failed"));
    }
    Ok(out)
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

    // RFC 7914 §11 vector #1.
    #[test]
    fn scrypt_rfc7914_v1() {
        // p="", salt="", N=16, r=1, p=1, dkLen=64
        let out = scrypt(b"", b"", 16, 1, 1, 32 * 1024 * 1024, 64).unwrap();
        let expected = hex_decode(
            "77d6576238657b203b19ca42c18a0497f16b4844e3074ae8dfdffa3fede21442\
             fcd0069ded0948f8326a753a0fc81f17e8d3e0fb2e0d3628cf35e20c38d18906",
        );
        assert_eq!(out, expected);
    }

    #[test]
    fn scrypt_rejects_non_power_of_two_n() {
        let err = scrypt(b"p", b"s", 15, 1, 1, 32 * 1024 * 1024, 32).unwrap_err();
        assert!(matches!(err, KernelError::InvalidKdfParams(_)));
    }

    #[test]
    fn scrypt_rejects_low_max_mem() {
        // 128 * 16 * 1 = 2048 bytes needed; pass max_mem=1024 → reject.
        let err = scrypt(b"p", b"s", 16, 1, 1, 1024, 32).unwrap_err();
        assert!(matches!(err, KernelError::InvalidKdfParams(_)));
    }
}
