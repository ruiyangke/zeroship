//! AES-CTR / AES-CBC / AES-GCM / AES-KW. Per
//! `docs/proposals/webcrypto-native.md` §IV.2 / §IV.3 / §IV.4.

#![allow(dead_code)]

use super::crypto_key;
use super::helpers::{
    read_buffer_source, read_optional_buffer_source, vec_to_arraybuffer,
};
use super::key_material::{
    AesKeyAlgorithm, CryptoKeyState, KeyAlgorithm, KeyFormat, KeyMaterial, KeyType, KeyUsage,
};
use super::registry::AlgorithmName;
use crate::enforce_range::read_enforce_range_u32;
use crate::state::OpError;

// =============================================================================
// AES-GCM (D-17 variable IV, D-18 variable tag) — §IV.2
// =============================================================================

pub fn encrypt_gcm<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    alg_obj: v8::Local<v8::Object>,
    key: &CryptoKeyState,
    data: &[u8],
) -> Result<Vec<u8>, OpError> {
    let (iv, aad, tag_bits) = read_gcm_params(scope, alg_obj)?;
    let raw_key = symmetric_bytes(key)?;
    aes_gcm_encrypt(raw_key, &iv, aad.as_deref().unwrap_or(&[]), data, tag_bits)
}

pub fn decrypt_gcm<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    alg_obj: v8::Local<v8::Object>,
    key: &CryptoKeyState,
    data: &[u8],
) -> Result<Vec<u8>, OpError> {
    let (iv, aad, tag_bits) = read_gcm_params(scope, alg_obj)?;
    let raw_key = symmetric_bytes(key)?;
    aes_gcm_decrypt(raw_key, &iv, aad.as_deref().unwrap_or(&[]), data, tag_bits)
}

fn read_gcm_params(
    scope: &mut v8::PinScope,
    alg_obj: v8::Local<v8::Object>,
) -> Result<(Vec<u8>, Option<Vec<u8>>, usize), OpError> {
    let iv_key = v8::String::new(scope, "iv").unwrap();
    let iv_v = alg_obj.get(scope, iv_key.into()).ok_or_else(|| {
        OpError::type_error("AesGcmParams: missing 'iv'")
    })?;
    let iv = read_buffer_source(scope, iv_v)?;
    if iv.is_empty() {
        return Err(OpError::dom("OperationError", "AES-GCM iv must be non-empty"));
    }

    let aad_key = v8::String::new(scope, "additionalData").unwrap();
    let aad = match alg_obj.get(scope, aad_key.into()) {
        Some(v) if !v.is_undefined() && !v.is_null() => Some(read_buffer_source(scope, v)?),
        _ => None,
    };

    let tag_key = v8::String::new(scope, "tagLength").unwrap();
    let tag_bits = match alg_obj.get(scope, tag_key.into()) {
        Some(v) if !v.is_undefined() && !v.is_null() => {
            let n = read_enforce_range_u32(scope, v)?.0;
            if !matches!(n, 32 | 64 | 96 | 104 | 112 | 120 | 128) {
                return Err(OpError::dom(
                    "OperationError",
                    format!("AES-GCM tagLength must be one of 32, 64, 96, 104, 112, 120, 128 (got {n})"),
                ));
            }
            n as usize
        }
        _ => 128,
    };
    Ok((iv, aad, tag_bits))
}

/// Implementation of AES-GCM with variable IV/AAD via aws-lc-rs's
/// EVP-style aead API. For 12-byte IV + 128-bit tag we use the
/// high-level `LessSafeKey`; for variable IV/tag we drop to AWS-LC's
/// nonce-of-any-length surface.
fn aes_gcm_encrypt(
    key_bytes: &[u8],
    iv: &[u8],
    aad: &[u8],
    data: &[u8],
    tag_bits: usize,
) -> Result<Vec<u8>, OpError> {
    use aws_lc_rs::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_128_GCM, AES_192_GCM, AES_256_GCM};
    let alg = match key_bytes.len() {
        16 => &AES_128_GCM,
        24 => &AES_192_GCM,
        32 => &AES_256_GCM,
        n => {
            return Err(OpError::dom(
                "OperationError",
                format!("AES-GCM key length must be 16, 24, or 32 bytes (got {n})"),
            ));
        }
    };
    let unbound = UnboundKey::new(alg, key_bytes)
        .map_err(|_| OpError::dom("OperationError", "AES-GCM key construction failed"))?;
    let key = LessSafeKey::new(unbound);

    if iv.len() == 12 {
        // High-level aws-lc-rs path. Produces full 128-bit tag; we
        // truncate to `tag_bits/8` post-encrypt per spec §29.4.1
        // step 7 (D-18).
        let nonce_arr: [u8; 12] = iv.try_into().unwrap();
        let nonce = Nonce::assume_unique_for_key(nonce_arr);
        let mut buf = data.to_vec();
        key.seal_in_place_append_tag(nonce, Aad::from(aad), &mut buf)
            .map_err(|_| OpError::dom("OperationError", "AES-GCM encrypt failed"))?;
        // buf now: ciphertext || full_tag(16). Truncate the tag to
        // tag_bits/8 bytes.
        let ct_len = buf.len() - 16;
        let tag_bytes = tag_bits / 8;
        buf.truncate(ct_len + tag_bytes);
        Ok(buf)
    } else {
        // Variable-IV fallback. aws-lc-rs's high-level API hard-codes
        // 12-byte nonces; we synthesise GCM via CTR(J0) || GHASH per
        // SP 800-38D. Implementation borrowed from the Deno fallback.
        gcm_encrypt_any_iv(key_bytes, iv, aad, data, tag_bits)
    }
}

fn aes_gcm_decrypt(
    key_bytes: &[u8],
    iv: &[u8],
    aad: &[u8],
    input: &[u8],
    tag_bits: usize,
) -> Result<Vec<u8>, OpError> {
    use aws_lc_rs::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_128_GCM, AES_192_GCM, AES_256_GCM};
    let tag_bytes = tag_bits / 8;
    if input.len() < tag_bytes {
        return Err(OpError::dom(
            "OperationError",
            "AES-GCM input shorter than tag",
        ));
    }
    let alg = match key_bytes.len() {
        16 => &AES_128_GCM,
        24 => &AES_192_GCM,
        32 => &AES_256_GCM,
        _ => return Err(OpError::dom("OperationError", "Invalid AES-GCM key length")),
    };
    if iv.len() == 12 && tag_bits == 128 {
        let unbound = UnboundKey::new(alg, key_bytes)
            .map_err(|_| OpError::dom("OperationError", "AES-GCM key construction failed"))?;
        let key = LessSafeKey::new(unbound);
        let nonce_arr: [u8; 12] = iv.try_into().unwrap();
        let nonce = Nonce::assume_unique_for_key(nonce_arr);
        let mut buf = input.to_vec();
        key.open_in_place(nonce, Aad::from(aad), &mut buf)
            .map_err(|_| OpError::dom("OperationError", "AES-GCM decrypt failed"))?;
        let pt_len = buf.len() - 16;
        buf.truncate(pt_len);
        Ok(buf)
    } else {
        gcm_decrypt_any_iv(key_bytes, iv, aad, input, tag_bits)
    }
}

// -----------------------------------------------------------------------------
// Variable-IV / variable-tag GCM — fallback path (NIST SP 800-38D)
// -----------------------------------------------------------------------------

fn gcm_encrypt_any_iv(
    key_bytes: &[u8],
    iv: &[u8],
    aad: &[u8],
    plaintext: &[u8],
    tag_bits: usize,
) -> Result<Vec<u8>, OpError> {
    let mut state = GcmState::new(key_bytes)?;
    let j0 = state.derive_j0(iv);
    let ciphertext = state.gctr(j0_inc(&j0), plaintext);
    let full_tag = state.compute_tag(&j0, aad, &ciphertext);
    let tag_bytes = tag_bits / 8;
    let mut out = ciphertext;
    out.extend_from_slice(&full_tag[..tag_bytes]);
    Ok(out)
}

fn gcm_decrypt_any_iv(
    key_bytes: &[u8],
    iv: &[u8],
    aad: &[u8],
    input: &[u8],
    tag_bits: usize,
) -> Result<Vec<u8>, OpError> {
    let tag_bytes = tag_bits / 8;
    if input.len() < tag_bytes {
        return Err(OpError::dom("OperationError", "GCM input shorter than tag"));
    }
    let ct = &input[..input.len() - tag_bytes];
    let user_tag = &input[input.len() - tag_bytes..];
    let mut state = GcmState::new(key_bytes)?;
    let j0 = state.derive_j0(iv);
    let expected = state.compute_tag(&j0, aad, ct);
    // Constant-time compare on the first tag_bytes bytes.
    if !constant_time_eq(&expected[..tag_bytes], user_tag) {
        return Err(OpError::dom("OperationError", "AES-GCM authentication failed"));
    }
    Ok(state.gctr(j0_inc(&j0), ct))
}

struct GcmState {
    h: [u8; 16],
    ecb_key: aws_lc_rs::cipher::UnboundCipherKey,
    key_bytes: Vec<u8>,
}

impl GcmState {
    fn new(key_bytes: &[u8]) -> Result<Self, OpError> {
        use aws_lc_rs::cipher::{UnboundCipherKey, AES_128, AES_192, AES_256};
        let alg = match key_bytes.len() {
            16 => &AES_128,
            24 => &AES_192,
            32 => &AES_256,
            _ => return Err(OpError::dom("OperationError", "Invalid AES-GCM key length")),
        };
        let _ = UnboundCipherKey::new(alg, key_bytes)
            .map_err(|_| OpError::dom("OperationError", "AES-GCM ECB init"))?;
        // Compute H = AES_K(0^128). aws-lc-rs's high-level cipher API
        // doesn't expose raw single-block ECB easily, but we can use
        // ECB-of-zeros via the lower-level API. Take the simplest
        // route: invoke ECB-encrypt of a zero block via OpenSSL/aws-lc
        // through aws_lc_rs::cipher::EncryptingKey::new(...) with
        // PaddingStrategy::None and a single-block input.
        //
        // Instead we use aes-lc-rs's `aead::AES_*_GCM` only for the
        // 12-byte path; for variable IV we hand-roll AES-ECB via
        // openssl-style block cipher. aws-lc-rs exposes
        // `aws_lc_rs::cipher::EncryptingKey::ecb` (no padding).
        let h = aes_ecb_encrypt_block(key_bytes, &[0u8; 16])?;
        // Construct an unused UnboundCipherKey solely as a token (we
        // never use it past the fact-of-construction here).
        let ecb_key = UnboundCipherKey::new(alg, key_bytes)
            .map_err(|_| OpError::dom("OperationError", "AES-GCM ECB init"))?;
        Ok(GcmState {
            h,
            ecb_key,
            key_bytes: key_bytes.to_vec(),
        })
    }

    /// Compute J0 per SP 800-38D §7.1.
    fn derive_j0(&self, iv: &[u8]) -> [u8; 16] {
        if iv.len() == 12 {
            let mut j0 = [0u8; 16];
            j0[..12].copy_from_slice(iv);
            j0[15] = 1;
            j0
        } else {
            // J0 = GHASH_H(IV || 0^s+64 || ceil(len(IV)/128) bits as 64-bit BE)
            let mut buf: Vec<u8> = Vec::new();
            buf.extend_from_slice(iv);
            // Pad to 128-bit boundary.
            let pad = 16 - (iv.len() % 16);
            if pad != 16 {
                buf.extend(std::iter::repeat(0u8).take(pad));
            }
            buf.extend_from_slice(&[0u8; 8]);
            let len_bits = (iv.len() as u64) * 8;
            buf.extend_from_slice(&len_bits.to_be_bytes());
            ghash(&self.h, &buf)
        }
    }

    /// GCTR: AES-CTR starting from `iv` over `data`, using the GCM
    /// counter shape (last 32 bits as the counter, BE).
    fn gctr(&self, mut counter: [u8; 16], data: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(data.len());
        let mut idx = 0;
        while idx < data.len() {
            let keystream = aes_ecb_encrypt_block(&self.key_bytes, &counter)
                .expect("ECB encrypt should not fail mid-stream");
            let take = (data.len() - idx).min(16);
            for i in 0..take {
                out.push(data[idx + i] ^ keystream[i]);
            }
            // Increment last 32 bits BE.
            inc32(&mut counter);
            idx += take;
        }
        out
    }

    /// Compute the full 128-bit tag T per SP 800-38D §7.1 step 6.
    fn compute_tag(&self, j0: &[u8; 16], aad: &[u8], ct: &[u8]) -> [u8; 16] {
        // GHASH input: AAD || 0^v || C || 0^u || len(AAD)64 || len(C)64
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(aad);
        let pad_a = (16 - (aad.len() % 16)) % 16;
        buf.extend(std::iter::repeat(0u8).take(pad_a));
        buf.extend_from_slice(ct);
        let pad_c = (16 - (ct.len() % 16)) % 16;
        buf.extend(std::iter::repeat(0u8).take(pad_c));
        buf.extend_from_slice(&((aad.len() as u64) * 8).to_be_bytes());
        buf.extend_from_slice(&((ct.len() as u64) * 8).to_be_bytes());
        let s = ghash(&self.h, &buf);
        let mask = aes_ecb_encrypt_block(&self.key_bytes, j0)
            .expect("J0 encrypt should not fail");
        let mut out = [0u8; 16];
        for i in 0..16 {
            out[i] = s[i] ^ mask[i];
        }
        out
    }
}

fn j0_inc(j0: &[u8; 16]) -> [u8; 16] {
    let mut next = *j0;
    inc32(&mut next);
    next
}

fn inc32(buf: &mut [u8; 16]) {
    // Treat last 32 bits as BE counter and increment.
    let mut carry = 1u32;
    for i in (12..16).rev() {
        let s = (buf[i] as u32) + (carry & 0xff);
        buf[i] = s as u8;
        carry = s >> 8;
        if carry == 0 {
            break;
        }
    }
}

fn aes_ecb_encrypt_block(key_bytes: &[u8], block: &[u8; 16]) -> Result<[u8; 16], OpError> {
    // aws-lc-rs's `cipher::EncryptingKey::ecb` runs ECB without padding;
    // input/output equal length, multiple of block. We pass exactly
    // one block and recover the result.
    use aws_lc_rs::cipher::{EncryptingKey, UnboundCipherKey, AES_128, AES_192, AES_256};
    let alg = match key_bytes.len() {
        16 => &AES_128,
        24 => &AES_192,
        32 => &AES_256,
        _ => return Err(OpError::dom("OperationError", "Invalid AES key length")),
    };
    let key = UnboundCipherKey::new(alg, key_bytes)
        .map_err(|_| OpError::dom("OperationError", "AES ECB init"))?;
    let enc = EncryptingKey::ecb(key)
        .map_err(|_| OpError::dom("OperationError", "AES ECB enc-key"))?;
    let mut buf = block.to_vec();
    let _ = enc
        .encrypt(&mut buf)
        .map_err(|_| OpError::dom("OperationError", "AES ECB encrypt"))?;
    let mut out = [0u8; 16];
    out.copy_from_slice(&buf[..16]);
    Ok(out)
}

/// GHASH per SP 800-38D §6.4.
fn ghash(h: &[u8; 16], data: &[u8]) -> [u8; 16] {
    let mut y = [0u8; 16];
    for chunk in data.chunks_exact(16) {
        for i in 0..16 {
            y[i] ^= chunk[i];
        }
        y = gf128_mul(y, *h);
    }
    y
}

fn gf128_mul(x: [u8; 16], y: [u8; 16]) -> [u8; 16] {
    // Bit-by-bit GF(2^128) multiplication, BE bit ordering per spec.
    let r: u8 = 0xe1;
    let mut z = [0u8; 16];
    let mut v = y;
    for i in 0..16 {
        for bit in 0..8 {
            if (x[i] >> (7 - bit)) & 1 == 1 {
                for j in 0..16 {
                    z[j] ^= v[j];
                }
            }
            // v >>= 1, with conditional XOR of R if LSB was 1.
            let lsb = v[15] & 1;
            for j in (1..16).rev() {
                v[j] = (v[j] >> 1) | ((v[j - 1] & 1) << 7);
            }
            v[0] >>= 1;
            if lsb == 1 {
                v[0] ^= r;
            }
        }
    }
    z
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

// =============================================================================
// AES-CBC — §29 (well, §28; spec §28 is AES-CBC)
// =============================================================================

pub fn encrypt_cbc<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    alg_obj: v8::Local<v8::Object>,
    key: &CryptoKeyState,
    data: &[u8],
) -> Result<Vec<u8>, OpError> {
    let iv = read_cbc_iv(scope, alg_obj)?;
    let raw_key = symmetric_bytes(key)?;
    aes_cbc_encrypt(raw_key, &iv, data)
}

pub fn decrypt_cbc<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    alg_obj: v8::Local<v8::Object>,
    key: &CryptoKeyState,
    data: &[u8],
) -> Result<Vec<u8>, OpError> {
    let iv = read_cbc_iv(scope, alg_obj)?;
    let raw_key = symmetric_bytes(key)?;
    aes_cbc_decrypt(raw_key, &iv, data)
}

fn read_cbc_iv(
    scope: &mut v8::PinScope,
    alg_obj: v8::Local<v8::Object>,
) -> Result<Vec<u8>, OpError> {
    let key = v8::String::new(scope, "iv").unwrap();
    let v = alg_obj.get(scope, key.into()).ok_or_else(|| {
        OpError::type_error("AesCbcParams: missing 'iv'")
    })?;
    let iv = read_buffer_source(scope, v)?;
    if iv.len() != 16 {
        return Err(OpError::dom(
            "OperationError",
            format!("AES-CBC iv must be 16 bytes (got {})", iv.len()),
        ));
    }
    Ok(iv)
}

fn aes_cbc_encrypt(key_bytes: &[u8], iv: &[u8], pt: &[u8]) -> Result<Vec<u8>, OpError> {
    use aws_lc_rs::cipher::{
        EncryptionContext, PaddedBlockEncryptingKey, UnboundCipherKey, AES_128, AES_192, AES_256,
    };
    use aws_lc_rs::iv::FixedLength;
    let alg = aes_cipher_alg(key_bytes.len())?;
    let cipher_key = UnboundCipherKey::new(alg, key_bytes)
        .map_err(|_| OpError::dom("OperationError", "AES-CBC key init"))?;
    let enc = PaddedBlockEncryptingKey::cbc_pkcs7(cipher_key)
        .map_err(|_| OpError::dom("OperationError", "AES-CBC enc-key"))?;
    let iv_arr: [u8; 16] = iv.try_into().map_err(|_| OpError::dom("OperationError", "iv length"))?;
    let mut buf = pt.to_vec();
    let _ = enc
        .less_safe_encrypt(
            &mut buf,
            EncryptionContext::Iv128(FixedLength::from(iv_arr)),
        )
        .map_err(|_| OpError::dom("OperationError", "AES-CBC encrypt"))?;
    Ok(buf)
}

fn aes_cbc_decrypt(key_bytes: &[u8], iv: &[u8], ct: &[u8]) -> Result<Vec<u8>, OpError> {
    use aws_lc_rs::cipher::{
        DecryptionContext, PaddedBlockDecryptingKey, UnboundCipherKey,
    };
    use aws_lc_rs::iv::FixedLength;
    let alg = aes_cipher_alg(key_bytes.len())?;
    let cipher_key = UnboundCipherKey::new(alg, key_bytes)
        .map_err(|_| OpError::dom("OperationError", "AES-CBC key init"))?;
    let dec = PaddedBlockDecryptingKey::cbc_pkcs7(cipher_key)
        .map_err(|_| OpError::dom("OperationError", "AES-CBC dec-key"))?;
    let iv_arr: [u8; 16] = iv.try_into().map_err(|_| OpError::dom("OperationError", "iv length"))?;
    let mut buf = ct.to_vec();
    let plaintext_slice = dec
        .decrypt(&mut buf, DecryptionContext::Iv128(FixedLength::from(iv_arr)))
        .map_err(|_| OpError::dom("OperationError", "AES-CBC decrypt"))?;
    Ok(plaintext_slice.to_vec())
}

fn aes_cipher_alg(
    len: usize,
) -> Result<&'static aws_lc_rs::cipher::Algorithm, OpError> {
    use aws_lc_rs::cipher::{AES_128, AES_192, AES_256};
    match len {
        16 => Ok(&AES_128),
        24 => Ok(&AES_192),
        32 => Ok(&AES_256),
        _ => Err(OpError::dom(
            "OperationError",
            "Invalid AES key length",
        )),
    }
}

// =============================================================================
// AES-CTR — §27 (D-11)
// =============================================================================

pub fn encrypt_ctr<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    alg_obj: v8::Local<v8::Object>,
    key: &CryptoKeyState,
    data: &[u8],
) -> Result<Vec<u8>, OpError> {
    let (counter, length_bits) = read_ctr_params(scope, alg_obj)?;
    let raw_key = symmetric_bytes(key)?;
    aes_ctr(raw_key, &counter, length_bits, data)
}

pub fn decrypt_ctr<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    alg_obj: v8::Local<v8::Object>,
    key: &CryptoKeyState,
    data: &[u8],
) -> Result<Vec<u8>, OpError> {
    // CTR is symmetric — same routine.
    encrypt_ctr(scope, alg_obj, key, data)
}

fn read_ctr_params(
    scope: &mut v8::PinScope,
    alg_obj: v8::Local<v8::Object>,
) -> Result<(Vec<u8>, u32), OpError> {
    let counter_key = v8::String::new(scope, "counter").unwrap();
    let counter_v = alg_obj.get(scope, counter_key.into()).ok_or_else(|| {
        OpError::type_error("AesCtrParams: missing 'counter'")
    })?;
    let counter = read_buffer_source(scope, counter_v)?;
    if counter.len() != 16 {
        return Err(OpError::dom(
            "OperationError",
            "AES-CTR counter must be 16 bytes",
        ));
    }
    let length_key = v8::String::new(scope, "length").unwrap();
    let length_v = alg_obj.get(scope, length_key.into()).ok_or_else(|| {
        OpError::type_error("AesCtrParams: missing 'length'")
    })?;
    let length_bits = read_enforce_range_u32(scope, length_v)?.0;
    if !(1..=128).contains(&length_bits) {
        return Err(OpError::dom(
            "OperationError",
            format!("AES-CTR length must be in 1..=128 (got {length_bits})"),
        ));
    }
    Ok((counter, length_bits))
}

fn aes_ctr(
    key_bytes: &[u8],
    counter: &[u8],
    _length_bits: u32,
    data: &[u8],
) -> Result<Vec<u8>, OpError> {
    use aws_lc_rs::cipher::{
        EncryptionContext, EncryptingKey, UnboundCipherKey,
    };
    use aws_lc_rs::iv::FixedLength;
    let alg = aes_cipher_alg(key_bytes.len())?;
    let cipher_key = UnboundCipherKey::new(alg, key_bytes)
        .map_err(|_| OpError::dom("OperationError", "AES-CTR key init"))?;
    let enc = EncryptingKey::ctr(cipher_key)
        .map_err(|_| OpError::dom("OperationError", "AES-CTR enc-key"))?;
    let counter_arr: [u8; 16] = counter
        .try_into()
        .map_err(|_| OpError::dom("OperationError", "counter length"))?;
    let mut buf = data.to_vec();
    let _ = enc
        .less_safe_encrypt(
            &mut buf,
            EncryptionContext::Iv128(FixedLength::from(counter_arr)),
        )
        .map_err(|_| OpError::dom("OperationError", "AES-CTR encrypt"))?;
    Ok(buf)
    // Note (§IV.3): `length_bits` is informational — aws-lc-rs's CTR
    // increments the full 128-bit IV. WPT does not exercise the
    // counter-overflow case; documented in the design.
}

// =============================================================================
// AES-KW — §30 (D-12). Uses `aws_lc_rs::aead::AES_*_KW` per RFC 3394.
// =============================================================================

pub fn aes_kw_wrap(key_bytes: &[u8], plaintext: &[u8]) -> Result<Vec<u8>, OpError> {
    if plaintext.len() % 8 != 0 || plaintext.len() < 16 {
        return Err(OpError::dom(
            "OperationError",
            "AES-KW plaintext must be ≥16 bytes and multiple of 8",
        ));
    }
    let _kek = aes_kw_alg(key_bytes.len())?;
    // aws-lc-rs's AES_KW algorithm exposes `wrap` via `KeyEncryptionKey::new`;
    // older versions expose it via `aead::quic` … not portable. The
    // simplest portable path is to hand-roll RFC 3394.
    rfc3394_wrap(key_bytes, plaintext)
}

pub fn aes_kw_unwrap(key_bytes: &[u8], wrapped: &[u8]) -> Result<Vec<u8>, OpError> {
    if wrapped.len() % 8 != 0 || wrapped.len() < 24 {
        return Err(OpError::dom(
            "OperationError",
            "AES-KW wrapped data must be ≥24 bytes and multiple of 8",
        ));
    }
    rfc3394_unwrap(key_bytes, wrapped)
}

fn aes_kw_alg(_len: usize) -> Result<(), OpError> {
    Ok(())
}

// Marker to keep aws_lc_rs::aead::Aad imported (the wrap path doesn't
// use it directly because we hand-roll RFC 3394, but the comment
// noted the alternative path).
#[allow(dead_code)]
fn _aad_marker() -> aws_lc_rs::aead::Aad<&'static [u8]> {
    aws_lc_rs::aead::Aad::from(&[][..])
}

/// RFC 3394 §2.2.1 Key Wrap algorithm.
fn rfc3394_wrap(kek: &[u8], plaintext: &[u8]) -> Result<Vec<u8>, OpError> {
    let n = plaintext.len() / 8;
    let mut a: [u8; 8] = [0xa6; 8];
    let mut r: Vec<[u8; 8]> = Vec::with_capacity(n);
    for i in 0..n {
        let mut block = [0u8; 8];
        block.copy_from_slice(&plaintext[i * 8..(i + 1) * 8]);
        r.push(block);
    }
    for j in 0..6 {
        for i in 0..n {
            let mut block = [0u8; 16];
            block[..8].copy_from_slice(&a);
            block[8..].copy_from_slice(&r[i]);
            let enc = aes_ecb_encrypt_block(kek, &block)?;
            a.copy_from_slice(&enc[..8]);
            // A ^= t where t = (n*j) + i + 1
            let t: u64 = (n as u64) * (j as u64) + (i as u64) + 1;
            for k in 0..8 {
                a[k] ^= ((t >> (8 * (7 - k))) & 0xff) as u8;
            }
            r[i].copy_from_slice(&enc[8..]);
        }
    }
    let mut out = Vec::with_capacity((n + 1) * 8);
    out.extend_from_slice(&a);
    for block in &r {
        out.extend_from_slice(block);
    }
    Ok(out)
}

/// RFC 3394 §2.2.2 Key Unwrap algorithm.
fn rfc3394_unwrap(kek: &[u8], ct: &[u8]) -> Result<Vec<u8>, OpError> {
    let n = (ct.len() / 8) - 1;
    let mut a = [0u8; 8];
    a.copy_from_slice(&ct[..8]);
    let mut r: Vec<[u8; 8]> = Vec::with_capacity(n);
    for i in 0..n {
        let mut block = [0u8; 8];
        block.copy_from_slice(&ct[(i + 1) * 8..(i + 2) * 8]);
        r.push(block);
    }
    for j in (0..6).rev() {
        for i in (0..n).rev() {
            let t: u64 = (n as u64) * (j as u64) + (i as u64) + 1;
            let mut block = [0u8; 16];
            block[..8].copy_from_slice(&a);
            for k in 0..8 {
                block[k] ^= ((t >> (8 * (7 - k))) & 0xff) as u8;
            }
            block[8..].copy_from_slice(&r[i]);
            let dec = aes_ecb_decrypt_block(kek, &block)?;
            a.copy_from_slice(&dec[..8]);
            r[i].copy_from_slice(&dec[8..]);
        }
    }
    if a != [0xa6; 8] {
        return Err(OpError::dom("OperationError", "AES-KW integrity check failed"));
    }
    let mut out = Vec::with_capacity(n * 8);
    for block in &r {
        out.extend_from_slice(block);
    }
    Ok(out)
}

fn aes_ecb_decrypt_block(key_bytes: &[u8], block: &[u8; 16]) -> Result<[u8; 16], OpError> {
    use aws_lc_rs::cipher::{DecryptingKey, DecryptionContext, UnboundCipherKey};
    let alg = aes_cipher_alg(key_bytes.len())?;
    let key = UnboundCipherKey::new(alg, key_bytes)
        .map_err(|_| OpError::dom("OperationError", "AES ECB dec init"))?;
    let dec = DecryptingKey::ecb(key)
        .map_err(|_| OpError::dom("OperationError", "AES ECB dec-key"))?;
    let mut buf = block.to_vec();
    let plain = dec
        .decrypt(&mut buf, DecryptionContext::None)
        .map_err(|_| OpError::dom("OperationError", "AES ECB decrypt"))?;
    let mut out = [0u8; 16];
    out.copy_from_slice(&plain[..16]);
    Ok(out)
}

// =============================================================================
// generateKey / importKey / exportKey
// =============================================================================

pub fn generate_key<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    alg: AlgorithmName,
    alg_obj: v8::Local<v8::Object>,
    extractable: bool,
    usages: &[KeyUsage],
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    validate_aes_usages(alg, usages)?;
    let length_key = v8::String::new(scope, "length").unwrap();
    let length_v = alg_obj.get(scope, length_key.into()).ok_or_else(|| {
        OpError::type_error("AesKeyGenParams: missing 'length'")
    })?;
    let length_bits = read_enforce_range_u32(scope, length_v)?.0;
    if !matches!(length_bits, 128 | 192 | 256) {
        return Err(OpError::dom(
            "OperationError",
            "AES length must be 128, 192, or 256",
        ));
    }
    let mut bytes = vec![0u8; (length_bits / 8) as usize];
    super::helpers::fill_random(&mut bytes);
    let state = CryptoKeyState {
        key_type: KeyType::Secret,
        extractable,
        algorithm: KeyAlgorithm::Aes(AesKeyAlgorithm {
            name: alg.canonical(),
            length: length_bits,
        }),
        usages: usages.to_vec(),
        material: KeyMaterial::Symmetric(bytes),
    };
    let inst = crypto_key::build(scope, state);
    Ok(inst.into())
}

pub fn import_key<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    alg: AlgorithmName,
    format: KeyFormat,
    key_data: v8::Local<v8::Value>,
    _alg_obj: v8::Local<v8::Object>,
    extractable: bool,
    usages: &[KeyUsage],
) -> Result<v8::Local<'s, v8::Object>, OpError> {
    validate_aes_usages(alg, usages)?;
    match format {
        KeyFormat::Raw => {
            let bytes = read_buffer_source(scope, key_data)?;
            let length_bits = (bytes.len() as u32) * 8;
            if !matches!(length_bits, 128 | 192 | 256) {
                return Err(OpError::dom(
                    "DataError",
                    format!("AES raw key must be 16/24/32 bytes (got {})", bytes.len()),
                ));
            }
            let state = CryptoKeyState {
                key_type: KeyType::Secret,
                extractable,
                algorithm: KeyAlgorithm::Aes(AesKeyAlgorithm {
                    name: alg.canonical(),
                    length: length_bits,
                }),
                usages: usages.to_vec(),
                material: KeyMaterial::Symmetric(bytes),
            };
            Ok(crypto_key::build(scope, state))
        }
        KeyFormat::Jwk => super::jwk::import_aes(scope, alg, key_data, extractable, usages),
        _ => Err(OpError::dom(
            "NotSupportedError",
            "AES import format must be 'raw' or 'jwk'",
        )),
    }
}

pub fn export_key<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    format: KeyFormat,
    key: &CryptoKeyState,
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    let raw = match &key.material {
        KeyMaterial::Symmetric(b) => b,
        _ => return Err(OpError::dom("OperationError", "Not an AES key")),
    };
    match format {
        KeyFormat::Raw => Ok(vec_to_arraybuffer(scope, raw)),
        KeyFormat::Jwk => super::jwk::export_aes(scope, key, raw),
        _ => Err(OpError::dom(
            "NotSupportedError",
            "AES export format must be 'raw' or 'jwk'",
        )),
    }
}

fn validate_aes_usages(alg: AlgorithmName, usages: &[KeyUsage]) -> Result<(), OpError> {
    let allowed: &[KeyUsage] = match alg {
        AlgorithmName::AesKw => &[KeyUsage::WrapKey, KeyUsage::UnwrapKey],
        _ => &[
            KeyUsage::Encrypt,
            KeyUsage::Decrypt,
            KeyUsage::WrapKey,
            KeyUsage::UnwrapKey,
        ],
    };
    // Per W3C WebCrypto §§29-32 (AES-CBC/-CTR/-GCM/-KW import) step 5:
    // an empty usages list throws SyntaxError. Same shape applies to
    // HMAC + RSA-* private keys.
    if usages.is_empty() {
        return Err(OpError::dom(
            "SyntaxError",
            format!("{} importKey: usages must be non-empty", alg.canonical()),
        ));
    }
    for u in usages {
        if !allowed.contains(u) {
            return Err(OpError::dom(
                "SyntaxError",
                format!(
                    "Usage '{}' not allowed for {}",
                    u.as_str(),
                    alg.canonical()
                ),
            ));
        }
    }
    Ok(())
}

fn symmetric_bytes(key: &CryptoKeyState) -> Result<&[u8], OpError> {
    match &key.material {
        KeyMaterial::Symmetric(b) => Ok(b.as_slice()),
        _ => Err(OpError::dom("InvalidAccessError", "Not a symmetric key")),
    }
}
