//! HKDF + PBKDF2 — deriveBits + importKey. Per
//! `docs/proposals/webcrypto-native.md` §IV.9 (D-20 [EnforceRange]
//! iterations + SHA-1 variants).

#![allow(dead_code)]

use super::crypto_key;
use super::helpers::read_buffer_source;
use super::key_material::{
    CryptoKeyState, HashAlgo, KeyAlgorithm, KeyFormat, KeyMaterial, KeyType, KeyUsage,
};
use crate::enforce_range::read_enforce_range_u32;
use crate::state::OpError;

// =============================================================================
// PBKDF2
// =============================================================================

pub fn pbkdf2_derive_bits<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    alg_obj: v8::Local<v8::Object>,
    key: &CryptoKeyState,
    length_bits: Option<u32>,
) -> Result<Vec<u8>, OpError> {
    let raw = match &key.material {
        KeyMaterial::Symmetric(b) => b.as_slice(),
        _ => return Err(OpError::dom("InvalidAccessError", "Not a PBKDF2 key")),
    };
    let length = length_bits.ok_or_else(|| {
        OpError::dom("OperationError", "PBKDF2 requires non-null length")
    })?;
    if length == 0 || length % 8 != 0 {
        return Err(OpError::dom(
            "OperationError",
            "PBKDF2 length must be > 0 and multiple of 8",
        ));
    }
    if length > 1_048_576 {
        return Err(OpError::dom(
            "OperationError",
            "PBKDF2 length too large (cap 128 KiB)",
        ));
    }
    let salt_v = alg_obj
        .get(scope, v8::String::new(scope, "salt").unwrap().into())
        .ok_or_else(|| OpError::type_error("Pbkdf2Params: missing 'salt'"))?;
    let salt = read_buffer_source(scope, salt_v)?;
    let iter_v = alg_obj
        .get(scope, v8::String::new(scope, "iterations").unwrap().into())
        .ok_or_else(|| OpError::type_error("Pbkdf2Params: missing 'iterations'"))?;
    let iterations = read_enforce_range_u32(scope, iter_v)?.0;
    if iterations == 0 {
        return Err(OpError::dom(
            "OperationError",
            "PBKDF2 iterations must be > 0",
        ));
    }
    let hash = read_hash(scope, alg_obj)?;
    let alg = match hash {
        HashAlgo::Sha1 => aws_lc_rs::pbkdf2::PBKDF2_HMAC_SHA1,
        HashAlgo::Sha256 => aws_lc_rs::pbkdf2::PBKDF2_HMAC_SHA256,
        HashAlgo::Sha384 => aws_lc_rs::pbkdf2::PBKDF2_HMAC_SHA384,
        HashAlgo::Sha512 => aws_lc_rs::pbkdf2::PBKDF2_HMAC_SHA512,
    };
    let out_len = (length / 8) as usize;
    let mut out = vec![0u8; out_len];
    let nz = std::num::NonZeroU32::new(iterations).unwrap();
    aws_lc_rs::pbkdf2::derive(alg, nz, &salt, raw, &mut out);
    Ok(out)
}

pub fn import_pbkdf2<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    format: KeyFormat,
    key_data: v8::Local<v8::Value>,
    extractable: bool,
    usages: &[KeyUsage],
) -> Result<v8::Local<'s, v8::Object>, OpError> {
    validate_kdf_usages(usages)?;
    if extractable {
        return Err(OpError::dom(
            "SyntaxError",
            "PBKDF2 keys must not be extractable",
        ));
    }
    let bytes = match format {
        KeyFormat::Raw => read_buffer_source(scope, key_data)?,
        _ => {
            return Err(OpError::dom(
                "NotSupportedError",
                "PBKDF2 import format must be 'raw'",
            ));
        }
    };
    let state = CryptoKeyState {
        key_type: KeyType::Secret,
        extractable: false,
        algorithm: KeyAlgorithm::Pbkdf2,
        usages: usages.to_vec(),
        material: KeyMaterial::Symmetric(bytes),
    };
    Ok(crypto_key::build(scope, state))
}

// =============================================================================
// HKDF
// =============================================================================

pub fn hkdf_derive_bits<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    alg_obj: v8::Local<v8::Object>,
    key: &CryptoKeyState,
    length_bits: Option<u32>,
) -> Result<Vec<u8>, OpError> {
    let raw = match &key.material {
        KeyMaterial::Symmetric(b) => b.as_slice(),
        _ => return Err(OpError::dom("InvalidAccessError", "Not an HKDF key")),
    };
    let length = length_bits.ok_or_else(|| {
        OpError::dom("OperationError", "HKDF requires non-null length")
    })?;
    if length == 0 || length % 8 != 0 {
        return Err(OpError::dom(
            "OperationError",
            "HKDF length must be > 0 and multiple of 8",
        ));
    }
    let hash = read_hash(scope, alg_obj)?;
    let salt_v = alg_obj
        .get(scope, v8::String::new(scope, "salt").unwrap().into())
        .ok_or_else(|| OpError::type_error("HkdfParams: missing 'salt'"))?;
    let salt = read_buffer_source(scope, salt_v)?;
    let info_v = alg_obj
        .get(scope, v8::String::new(scope, "info").unwrap().into())
        .ok_or_else(|| OpError::type_error("HkdfParams: missing 'info'"))?;
    let info = read_buffer_source(scope, info_v)?;
    // RFC 5869 §2.3 max length: 255 * digest_len.
    let max_bytes = 255 * hash.digest_len();
    let out_len = (length / 8) as usize;
    if out_len > max_bytes {
        return Err(OpError::dom(
            "OperationError",
            format!("HKDF length exceeds 255 * digest_len ({max_bytes} bytes)"),
        ));
    }
    let alg = match hash {
        HashAlgo::Sha1 => aws_lc_rs::hkdf::HKDF_SHA1_FOR_LEGACY_USE_ONLY,
        HashAlgo::Sha256 => aws_lc_rs::hkdf::HKDF_SHA256,
        HashAlgo::Sha384 => aws_lc_rs::hkdf::HKDF_SHA384,
        HashAlgo::Sha512 => aws_lc_rs::hkdf::HKDF_SHA512,
    };
    let salt_obj = aws_lc_rs::hkdf::Salt::new(alg, &salt);
    let prk = salt_obj.extract(raw);
    let info_slice: &[&[u8]] = &[&info[..]];
    let okm = prk
        .expand(info_slice, HkdfLen(out_len))
        .map_err(|_| OpError::dom("OperationError", "HKDF expand failed"))?;
    let mut out = vec![0u8; out_len];
    okm.fill(&mut out)
        .map_err(|_| OpError::dom("OperationError", "HKDF fill failed"))?;
    Ok(out)
}

#[derive(Debug, Clone, Copy)]
struct HkdfLen(usize);
impl aws_lc_rs::hkdf::KeyType for HkdfLen {
    fn len(&self) -> usize {
        self.0
    }
}

pub fn import_hkdf<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    format: KeyFormat,
    key_data: v8::Local<v8::Value>,
    extractable: bool,
    usages: &[KeyUsage],
) -> Result<v8::Local<'s, v8::Object>, OpError> {
    validate_kdf_usages(usages)?;
    if extractable {
        return Err(OpError::dom(
            "SyntaxError",
            "HKDF keys must not be extractable",
        ));
    }
    let bytes = match format {
        KeyFormat::Raw => read_buffer_source(scope, key_data)?,
        _ => {
            return Err(OpError::dom(
                "NotSupportedError",
                "HKDF import format must be 'raw'",
            ));
        }
    };
    let state = CryptoKeyState {
        key_type: KeyType::Secret,
        extractable: false,
        algorithm: KeyAlgorithm::Hkdf,
        usages: usages.to_vec(),
        material: KeyMaterial::Symmetric(bytes),
    };
    Ok(crypto_key::build(scope, state))
}

fn validate_kdf_usages(usages: &[KeyUsage]) -> Result<(), OpError> {
    for u in usages {
        if !matches!(u, KeyUsage::DeriveBits | KeyUsage::DeriveKey) {
            return Err(OpError::dom(
                "SyntaxError",
                format!(
                    "Usage '{}' not allowed for HKDF/PBKDF2",
                    u.as_str()
                ),
            ));
        }
    }
    Ok(())
}

fn read_hash(
    scope: &mut v8::PinScope,
    alg_obj: v8::Local<v8::Object>,
) -> Result<HashAlgo, OpError> {
    let key = v8::String::new(scope, "hash").unwrap();
    let v = alg_obj.get(scope, key.into()).ok_or_else(|| {
        OpError::type_error("missing 'hash' field")
    })?;
    let name = if v.is_string() {
        v.to_rust_string_lossy(scope)
    } else if let Ok(o) = v8::Local::<v8::Object>::try_from(v) {
        let inner = v8::String::new(scope, "name").unwrap();
        let inner_v = o.get(scope, inner.into()).ok_or_else(|| {
            OpError::type_error("hash.name missing")
        })?;
        inner_v.to_rust_string_lossy(scope)
    } else {
        return Err(OpError::type_error("hash invalid"));
    };
    HashAlgo::from_str(&name).ok_or_else(|| {
        OpError::dom("NotSupportedError", format!("Unrecognised hash '{name}'"))
    })
}
