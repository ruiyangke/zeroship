//! `wrapKey` / `unwrapKey` orchestration. Per
//! `docs/proposals/webcrypto-native.md` §IV.4 / §14.3.10 / §14.3.11.

#![allow(dead_code)]

use super::crypto_key;
use super::helpers::read_buffer_source;
use super::key_material::{
    CryptoKeyState, KeyAlgorithm, KeyFormat, KeyMaterial, KeyUsage,
};
use super::registry::{self, AlgorithmName, Operation};
use crate::state::OpError;

/// Spec §14.3.10: wrap a key by serializing it (per `format`) then
/// running the wrap algorithm's encrypt over the serialized bytes.
pub fn wrap_key<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    format: KeyFormat,
    key: v8::Local<v8::Value>,
    wrapping_key: v8::Local<v8::Value>,
    wrap_alg: v8::Local<v8::Value>,
) -> Result<Vec<u8>, OpError> {
    let (head, alg_obj) = registry::normalize_head(scope, Operation::WrapKey, wrap_alg)?;
    let wrapping_state = crypto_key::require(scope, wrapping_key)?;
    wrapping_state.check_usage(KeyUsage::WrapKey)?;
    if head.name.canonical() != wrapping_state.algorithm.name() {
        return Err(OpError::dom(
            "InvalidAccessError",
            "wrapAlgorithm does not match wrappingKey.algorithm",
        ));
    }
    let inner_state = crypto_key::require(scope, key)?;
    if !inner_state.extractable {
        return Err(OpError::dom(
            "InvalidAccessError",
            "Key to wrap is not extractable",
        ));
    }

    // Serialize the inner key per format. We hand-roll the export
    // (rather than re-entering `subtle.exportKey`) so the format is a
    // local concern. The format must be valid for the inner key's
    // algorithm — defer to the per-algorithm exporters.
    let serialized = serialize_for_wrap(scope, format, inner_state)?;

    // Run the wrapping algorithm's encrypt-equivalent over the
    // serialized bytes.
    match head.name {
        AlgorithmName::AesKw => {
            let kek = match &wrapping_state.material {
                KeyMaterial::Symmetric(b) => b,
                _ => return Err(OpError::dom("InvalidAccessError", "AES-KW: not symmetric")),
            };
            super::aes::aes_kw_wrap(kek, &serialized)
        }
        AlgorithmName::AesGcm => super::aes::encrypt_gcm(scope, alg_obj, wrapping_state, &serialized),
        AlgorithmName::AesCbc => super::aes::encrypt_cbc(scope, alg_obj, wrapping_state, &serialized),
        AlgorithmName::AesCtr => super::aes::encrypt_ctr(scope, alg_obj, wrapping_state, &serialized),
        AlgorithmName::RsaOaep => super::rsa::encrypt_oaep(scope, alg_obj, wrapping_state, &serialized),
        _ => Err(OpError::dom(
            "NotSupportedError",
            format!("wrapKey does not support '{}'", head.name.canonical()),
        )),
    }
}

pub fn unwrap_key<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    format: KeyFormat,
    wrapped: v8::Local<v8::Value>,
    unwrapping_key: v8::Local<v8::Value>,
    unwrap_alg: v8::Local<v8::Value>,
    unwrapped_alg: v8::Local<v8::Value>,
    extractable: bool,
    usages: v8::Local<v8::Value>,
) -> Result<v8::Local<'s, v8::Object>, OpError> {
    let wrapped_bytes = read_buffer_source(scope, wrapped)?;
    let (head, unwrap_obj) = registry::normalize_head(scope, Operation::UnwrapKey, unwrap_alg)?;
    let unwrapping_state = crypto_key::require(scope, unwrapping_key)?;
    unwrapping_state.check_usage(KeyUsage::UnwrapKey)?;
    if head.name.canonical() != unwrapping_state.algorithm.name() {
        return Err(OpError::dom(
            "InvalidAccessError",
            "unwrapAlgorithm does not match unwrappingKey.algorithm",
        ));
    }

    let serialized = match head.name {
        AlgorithmName::AesKw => {
            let kek = match &unwrapping_state.material {
                KeyMaterial::Symmetric(b) => b,
                _ => return Err(OpError::dom("InvalidAccessError", "AES-KW: not symmetric")),
            };
            super::aes::aes_kw_unwrap(kek, &wrapped_bytes)?
        }
        AlgorithmName::AesGcm => {
            super::aes::decrypt_gcm(scope, unwrap_obj, unwrapping_state, &wrapped_bytes)?
        }
        AlgorithmName::AesCbc => {
            super::aes::decrypt_cbc(scope, unwrap_obj, unwrapping_state, &wrapped_bytes)?
        }
        AlgorithmName::AesCtr => {
            super::aes::decrypt_ctr(scope, unwrap_obj, unwrapping_state, &wrapped_bytes)?
        }
        AlgorithmName::RsaOaep => {
            super::rsa::decrypt_oaep(scope, unwrap_obj, unwrapping_state, &wrapped_bytes)?
        }
        _ => {
            return Err(OpError::dom(
                "NotSupportedError",
                format!("unwrapKey does not support '{}'", head.name.canonical()),
            ));
        }
    };

    // Build a fresh ArrayBuffer for the importKey call.
    let ab = v8::ArrayBuffer::new(scope, serialized.len());
    let store = ab.get_backing_store();
    for (i, &b) in serialized.iter().enumerate() {
        store[i].set(b);
    }
    super::ops::import_key(scope, format, ab.into(), unwrapped_alg, extractable, usages)
}

/// Serialize a CryptoKey to bytes per `format`. Mirrors the per-algorithm
/// exporters but returns raw bytes (not JS values) since we feed them
/// through encrypt → ArrayBuffer downstream.
fn serialize_for_wrap(
    _scope: &mut v8::PinScope,
    format: KeyFormat,
    key: &CryptoKeyState,
) -> Result<Vec<u8>, OpError> {
    match (format, &key.algorithm) {
        (KeyFormat::Raw, KeyAlgorithm::Aes(_) | KeyAlgorithm::Hmac(_)) => match &key.material {
            KeyMaterial::Symmetric(b) => Ok(b.clone()),
            _ => Err(OpError::dom("OperationError", "Wrap raw: not symmetric")),
        },
        (KeyFormat::Raw, KeyAlgorithm::Ec(_)) => match &key.material {
            KeyMaterial::EcPublic { raw_xy, .. } => Ok(raw_xy.clone()),
            _ => Err(OpError::dom(
                "NotSupportedError",
                "Wrap raw: EC private not allowed",
            )),
        },
        (KeyFormat::Raw, KeyAlgorithm::Ed25519) => match &key.material {
            KeyMaterial::Ed25519Public { raw_x, .. } => Ok(raw_x.to_vec()),
            _ => Err(OpError::dom(
                "NotSupportedError",
                "Wrap raw: Ed25519 private not allowed",
            )),
        },
        (KeyFormat::Spki, _) => match &key.material {
            KeyMaterial::RsaPublic { spki_der, .. } => Ok(spki_der.clone()),
            _ => Err(OpError::dom(
                "NotSupportedError",
                "Wrap spki: only RSA public supported",
            )),
        },
        (KeyFormat::Pkcs8, _) => match &key.material {
            KeyMaterial::RsaPrivate { pkcs8_der, .. } => Ok(pkcs8_der.clone()),
            KeyMaterial::EcPrivate { pkcs8_der, .. } => Ok(pkcs8_der.clone()),
            _ => Err(OpError::dom(
                "NotSupportedError",
                "Wrap pkcs8: only RSA/EC private supported",
            )),
        },
        (KeyFormat::Jwk, _) => Err(OpError::dom(
            "NotSupportedError",
            "Wrap 'jwk' format requires JSON serialization (deferred)",
        )),
        _ => Err(OpError::dom(
            "NotSupportedError",
            "Unsupported wrap format/algorithm pair",
        )),
    }
}
