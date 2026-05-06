//! HMAC — sign/verify/generateKey/importKey/exportKey. See
//! `docs/proposals/webcrypto-native.md` §IV.8.

#![allow(dead_code)]

use super::crypto_key;
use super::digest;
use super::helpers::{read_buffer_source, vec_to_uint8array};
use super::key_material::{
    CryptoKeyState, HashAlgo, HmacKeyAlgorithm, KeyAlgorithm, KeyFormat, KeyMaterial, KeyType,
    KeyUsage,
};
use crate::enforce_range::read_enforce_range_u32;
use crate::state::OpError;

pub fn sign(key: &CryptoKeyState, data: &[u8]) -> Result<Vec<u8>, OpError> {
    let (raw, hash) = require_hmac(key)?;
    let alg = match hash {
        HashAlgo::Sha1 => &aws_lc_rs::hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY,
        HashAlgo::Sha256 => &aws_lc_rs::hmac::HMAC_SHA256,
        HashAlgo::Sha384 => &aws_lc_rs::hmac::HMAC_SHA384,
        HashAlgo::Sha512 => &aws_lc_rs::hmac::HMAC_SHA512,
    };
    let key = aws_lc_rs::hmac::Key::new(*alg, raw);
    let tag = aws_lc_rs::hmac::sign(&key, data);
    Ok(tag.as_ref().to_vec())
}

pub fn verify(
    key: &CryptoKeyState,
    data: &[u8],
    signature: &[u8],
) -> Result<bool, OpError> {
    let (raw, hash) = require_hmac(key)?;
    let alg = match hash {
        HashAlgo::Sha1 => &aws_lc_rs::hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY,
        HashAlgo::Sha256 => &aws_lc_rs::hmac::HMAC_SHA256,
        HashAlgo::Sha384 => &aws_lc_rs::hmac::HMAC_SHA384,
        HashAlgo::Sha512 => &aws_lc_rs::hmac::HMAC_SHA512,
    };
    let key = aws_lc_rs::hmac::Key::new(*alg, raw);
    Ok(aws_lc_rs::hmac::verify(&key, data, signature).is_ok())
}

pub fn generate_key<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    alg_obj: v8::Local<v8::Object>,
    extractable: bool,
    usages: &[KeyUsage],
    hash: Option<HashAlgo>,
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    validate_hmac_usages(usages)?;
    let hash = hash.ok_or_else(|| {
        OpError::dom("NotSupportedError", "HMAC requires 'hash' field")
    })?;
    // length: optional. If absent, default to the hash block size in
    // bits (spec §31.4.3 step 2).
    let length_key = v8::String::new(scope, "length").unwrap();
    let length_bits = match alg_obj.get(scope, length_key.into()) {
        Some(v) if !v.is_undefined() && !v.is_null() => {
            let n = read_enforce_range_u32(scope, v)?.0;
            if n == 0 {
                return Err(OpError::dom(
                    "OperationError",
                    "HMAC length must be > 0",
                ));
            }
            n
        }
        _ => hash.block_size_bits(),
    };
    let bytes_len = ((length_bits as usize) + 7) / 8;
    let mut bytes = vec![0u8; bytes_len];
    super::helpers::fill_random(&mut bytes);
    let state = CryptoKeyState {
        key_type: KeyType::Secret,
        extractable,
        algorithm: KeyAlgorithm::Hmac(HmacKeyAlgorithm {
            hash,
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
    format: KeyFormat,
    key_data: v8::Local<v8::Value>,
    alg_obj: v8::Local<v8::Object>,
    extractable: bool,
    usages: &[KeyUsage],
    hash: Option<HashAlgo>,
) -> Result<v8::Local<'s, v8::Object>, OpError> {
    validate_hmac_usages(usages)?;
    let hash = hash.ok_or_else(|| {
        OpError::dom("NotSupportedError", "HMAC requires 'hash'")
    })?;
    match format {
        KeyFormat::Raw => {
            let bytes = read_buffer_source(scope, key_data)?;
            if bytes.is_empty() {
                return Err(OpError::dom("DataError", "HMAC key must be non-empty"));
            }
            let length_key = v8::String::new(scope, "length").unwrap();
            let length_bits = match alg_obj.get(scope, length_key.into()) {
                Some(v) if !v.is_undefined() && !v.is_null() => {
                    let n = read_enforce_range_u32(scope, v)?.0;
                    if n == 0 {
                        return Err(OpError::type_error("HMAC length must be > 0"));
                    }
                    n
                }
                _ => (bytes.len() as u32) * 8,
            };
            let state = CryptoKeyState {
                key_type: KeyType::Secret,
                extractable,
                algorithm: KeyAlgorithm::Hmac(HmacKeyAlgorithm { hash, length: length_bits }),
                usages: usages.to_vec(),
                material: KeyMaterial::Symmetric(bytes),
            };
            Ok(crypto_key::build(scope, state))
        }
        KeyFormat::Jwk => super::jwk::import_hmac(scope, key_data, hash, extractable, usages),
        _ => Err(OpError::dom(
            "NotSupportedError",
            "HMAC import format must be 'raw' or 'jwk'",
        )),
    }
}

pub fn export_key<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    format: KeyFormat,
    key: &CryptoKeyState,
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    let (raw, _) = require_hmac(key)?;
    match format {
        KeyFormat::Raw => Ok(vec_to_uint8array(scope, raw)),
        KeyFormat::Jwk => super::jwk::export_hmac(scope, key, raw),
        _ => Err(OpError::dom(
            "NotSupportedError",
            "HMAC export format must be 'raw' or 'jwk'",
        )),
    }
}

fn validate_hmac_usages(usages: &[KeyUsage]) -> Result<(), OpError> {
    // Per W3C WebCrypto §31 (HMAC importKey) step 6: empty usages →
    // SyntaxError.
    if usages.is_empty() {
        return Err(OpError::dom(
            "SyntaxError",
            "HMAC importKey: usages must be non-empty",
        ));
    }
    for u in usages {
        if !matches!(u, KeyUsage::Sign | KeyUsage::Verify) {
            return Err(OpError::dom(
                "SyntaxError",
                format!("Usage '{}' not allowed for HMAC", u.as_str()),
            ));
        }
    }
    Ok(())
}

fn require_hmac(key: &CryptoKeyState) -> Result<(&[u8], HashAlgo), OpError> {
    match (&key.algorithm, &key.material) {
        (KeyAlgorithm::Hmac(h), KeyMaterial::Symmetric(b)) => Ok((b.as_slice(), h.hash)),
        _ => Err(OpError::dom("InvalidAccessError", "Not an HMAC key")),
    }
}
