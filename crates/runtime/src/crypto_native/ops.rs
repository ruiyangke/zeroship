//! Per-operation dispatchers — `encrypt`, `decrypt`, `sign`, `verify`,
//! `generateKey`, `importKey`, `exportKey`, `deriveBits`, `deriveKey`,
//! `wrapKey`, `unwrapKey`. Per `docs/proposals/webcrypto-native.md`
//! §IV.
//!
//! Each function:
//!  1. Normalizes the algorithm head (`registry::normalize_head`).
//!  2. Dispatches to the per-algorithm implementation in
//!     `aes.rs` / `rsa.rs` / `ec.rs` / `okp.rs` / `hmac.rs` /
//!     `derive.rs`.
//!  3. Returns the result (bytes, JS object, or boolean) for the
//!     `SubtleCrypto` method to wrap as a Promise.

use super::crypto_key;
use super::helpers::{read_buffer_source, read_optional_buffer_source};
use super::key_material::{
    AesKeyAlgorithm, CryptoKeyState, EcKeyAlgorithm, HashAlgo, HmacKeyAlgorithm, KeyAlgorithm,
    KeyFormat, KeyMaterial, KeyType, KeyUsage, NamedCurve, RsaHashedKeyAlgorithm,
};
use super::registry::{self, AlgorithmName, NormalizedHead, Operation};
use crate::enforce_range::read_enforce_range_u32;
use crate::state::OpError;

// ---------------------------------------------------------------------------
// Top-level dispatchers
// ---------------------------------------------------------------------------

pub fn encrypt<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    alg: v8::Local<v8::Value>,
    key: v8::Local<v8::Value>,
    data: v8::Local<v8::Value>,
) -> Result<Vec<u8>, OpError> {
    let (head, alg_obj) = registry::normalize_head(scope, Operation::Encrypt, alg)?;
    let key_state = crypto_key::require(scope, key)?;
    key_state.check_usage(KeyUsage::Encrypt)?;
    if !alg_matches_key(head.name, key_state) {
        return Err(OpError::dom(
            "InvalidAccessError",
            format!(
                "Algorithm '{}' does not match key algorithm '{}'",
                head.name.canonical(),
                key_state.algorithm.name()
            ),
        ));
    }
    let data_bytes = read_buffer_source(scope, data)?;
    match head.name {
        AlgorithmName::AesGcm => super::aes::encrypt_gcm(scope, alg_obj, key_state, &data_bytes),
        AlgorithmName::AesCbc => super::aes::encrypt_cbc(scope, alg_obj, key_state, &data_bytes),
        AlgorithmName::AesCtr => super::aes::encrypt_ctr(scope, alg_obj, key_state, &data_bytes),
        AlgorithmName::RsaOaep => super::rsa::encrypt_oaep(scope, alg_obj, key_state, &data_bytes),
        _ => Err(OpError::dom(
            "NotSupportedError",
            format!("encrypt does not support '{}'", head.name.canonical()),
        )),
    }
}

pub fn decrypt<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    alg: v8::Local<v8::Value>,
    key: v8::Local<v8::Value>,
    data: v8::Local<v8::Value>,
) -> Result<Vec<u8>, OpError> {
    let (head, alg_obj) = registry::normalize_head(scope, Operation::Decrypt, alg)?;
    let key_state = crypto_key::require(scope, key)?;
    key_state.check_usage(KeyUsage::Decrypt)?;
    if !alg_matches_key(head.name, key_state) {
        return Err(OpError::dom(
            "InvalidAccessError",
            format!(
                "Algorithm '{}' does not match key algorithm '{}'",
                head.name.canonical(),
                key_state.algorithm.name()
            ),
        ));
    }
    let data_bytes = read_buffer_source(scope, data)?;
    match head.name {
        AlgorithmName::AesGcm => super::aes::decrypt_gcm(scope, alg_obj, key_state, &data_bytes),
        AlgorithmName::AesCbc => super::aes::decrypt_cbc(scope, alg_obj, key_state, &data_bytes),
        AlgorithmName::AesCtr => super::aes::decrypt_ctr(scope, alg_obj, key_state, &data_bytes),
        AlgorithmName::RsaOaep => super::rsa::decrypt_oaep(scope, alg_obj, key_state, &data_bytes),
        _ => Err(OpError::dom(
            "NotSupportedError",
            format!("decrypt does not support '{}'", head.name.canonical()),
        )),
    }
}

pub fn sign<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    alg: v8::Local<v8::Value>,
    key: v8::Local<v8::Value>,
    data: v8::Local<v8::Value>,
) -> Result<Vec<u8>, OpError> {
    let (head, alg_obj) = registry::normalize_head(scope, Operation::Sign, alg)?;
    let key_state = crypto_key::require(scope, key)?;
    key_state.check_usage(KeyUsage::Sign)?;
    if !alg_matches_key(head.name, key_state) {
        return Err(OpError::dom(
            "InvalidAccessError",
            format!(
                "Algorithm '{}' does not match key algorithm '{}'",
                head.name.canonical(),
                key_state.algorithm.name()
            ),
        ));
    }
    let data_bytes = read_buffer_source(scope, data)?;
    match head.name {
        AlgorithmName::Hmac => super::hmac::sign(key_state, &data_bytes),
        AlgorithmName::Ecdsa => super::ec::sign_ecdsa(scope, alg_obj, key_state, &data_bytes),
        AlgorithmName::RsassaPkcs1v15 => super::rsa::sign_pkcs1(key_state, &data_bytes),
        AlgorithmName::RsaPss => super::rsa::sign_pss(scope, alg_obj, key_state, &data_bytes),
        AlgorithmName::Ed25519 => super::okp::sign_ed25519(key_state, &data_bytes),
        _ => Err(OpError::dom(
            "NotSupportedError",
            format!("sign does not support '{}'", head.name.canonical()),
        )),
    }
}

pub fn verify<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    alg: v8::Local<v8::Value>,
    key: v8::Local<v8::Value>,
    signature: v8::Local<v8::Value>,
    data: v8::Local<v8::Value>,
) -> Result<bool, OpError> {
    let (head, alg_obj) = registry::normalize_head(scope, Operation::Verify, alg)?;
    let key_state = crypto_key::require(scope, key)?;
    key_state.check_usage(KeyUsage::Verify)?;
    if !alg_matches_key(head.name, key_state) {
        return Err(OpError::dom(
            "InvalidAccessError",
            format!(
                "Algorithm '{}' does not match key algorithm '{}'",
                head.name.canonical(),
                key_state.algorithm.name()
            ),
        ));
    }
    let sig = read_buffer_source(scope, signature)?;
    let data_bytes = read_buffer_source(scope, data)?;
    match head.name {
        AlgorithmName::Hmac => super::hmac::verify(key_state, &data_bytes, &sig),
        AlgorithmName::Ecdsa => super::ec::verify_ecdsa(scope, alg_obj, key_state, &sig, &data_bytes),
        AlgorithmName::RsassaPkcs1v15 => {
            super::rsa::verify_pkcs1(key_state, &data_bytes, &sig)
        }
        AlgorithmName::RsaPss => {
            super::rsa::verify_pss(scope, alg_obj, key_state, &data_bytes, &sig)
        }
        AlgorithmName::Ed25519 => super::okp::verify_ed25519(key_state, &data_bytes, &sig),
        _ => Err(OpError::dom(
            "NotSupportedError",
            format!("verify does not support '{}'", head.name.canonical()),
        )),
    }
}

// ---------------------------------------------------------------------------
// generateKey — returns a single CryptoKey (symmetric) or
// { publicKey, privateKey } (asymmetric).
// ---------------------------------------------------------------------------

pub fn generate_key<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    alg: v8::Local<v8::Value>,
    extractable: bool,
    usages: v8::Local<v8::Value>,
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    let (head, alg_obj) = registry::normalize_head(scope, Operation::GenerateKey, alg)?;
    let usage_vec = registry::parse_usages(scope, usages)?;

    match head.name {
        AlgorithmName::AesCtr | AlgorithmName::AesCbc | AlgorithmName::AesGcm | AlgorithmName::AesKw => {
            super::aes::generate_key(scope, head.name, alg_obj, extractable, &usage_vec)
        }
        AlgorithmName::Hmac => super::hmac::generate_key(scope, alg_obj, extractable, &usage_vec, head.hash),
        AlgorithmName::RsassaPkcs1v15 | AlgorithmName::RsaPss | AlgorithmName::RsaOaep => {
            super::rsa::generate_key(scope, head.name, alg_obj, extractable, &usage_vec, head.hash)
        }
        AlgorithmName::Ecdsa | AlgorithmName::Ecdh => {
            super::ec::generate_key(scope, head.name, alg_obj, extractable, &usage_vec)
        }
        AlgorithmName::Ed25519 => super::okp::generate_ed25519(scope, extractable, &usage_vec),
        AlgorithmName::X25519 => super::okp::generate_x25519(scope, extractable, &usage_vec),
        _ => Err(OpError::dom(
            "NotSupportedError",
            format!(
                "generateKey does not support '{}'",
                head.name.canonical()
            ),
        )),
    }
}

// ---------------------------------------------------------------------------
// importKey
// ---------------------------------------------------------------------------

pub fn import_key<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    format: KeyFormat,
    key_data: v8::Local<v8::Value>,
    alg: v8::Local<v8::Value>,
    extractable: bool,
    usages: v8::Local<v8::Value>,
) -> Result<v8::Local<'s, v8::Object>, OpError> {
    let (head, alg_obj) = registry::normalize_head(scope, Operation::ImportKey, alg)?;
    let usage_vec = registry::parse_usages(scope, usages)?;

    match head.name {
        AlgorithmName::AesCtr | AlgorithmName::AesCbc | AlgorithmName::AesGcm | AlgorithmName::AesKw => {
            super::aes::import_key(scope, head.name, format, key_data, alg_obj, extractable, &usage_vec)
        }
        AlgorithmName::Hmac => {
            super::hmac::import_key(scope, format, key_data, alg_obj, extractable, &usage_vec, head.hash)
        }
        AlgorithmName::RsassaPkcs1v15 | AlgorithmName::RsaPss | AlgorithmName::RsaOaep => {
            super::rsa::import_key(scope, head.name, format, key_data, alg_obj, extractable, &usage_vec, head.hash)
        }
        AlgorithmName::Ecdsa | AlgorithmName::Ecdh => {
            super::ec::import_key(scope, head.name, format, key_data, alg_obj, extractable, &usage_vec)
        }
        AlgorithmName::Ed25519 => {
            super::okp::import_ed25519(scope, format, key_data, extractable, &usage_vec)
        }
        AlgorithmName::X25519 => {
            super::okp::import_x25519(scope, format, key_data, extractable, &usage_vec)
        }
        AlgorithmName::Hkdf => super::derive::import_hkdf(scope, format, key_data, extractable, &usage_vec),
        AlgorithmName::Pbkdf2 => super::derive::import_pbkdf2(scope, format, key_data, extractable, &usage_vec),
        _ => Err(OpError::dom(
            "NotSupportedError",
            format!("importKey does not support '{}'", head.name.canonical()),
        )),
    }
}

// ---------------------------------------------------------------------------
// exportKey
// ---------------------------------------------------------------------------

pub fn export_key<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    format: KeyFormat,
    key: v8::Local<v8::Value>,
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    let key_state = crypto_key::require(scope, key)?;
    if !key_state.extractable {
        return Err(OpError::dom(
            "InvalidAccessError",
            "Key is not extractable",
        ));
    }
    match &key_state.algorithm {
        KeyAlgorithm::Aes(_) => super::aes::export_key(scope, format, key_state),
        KeyAlgorithm::Hmac(_) => super::hmac::export_key(scope, format, key_state),
        KeyAlgorithm::RsaHashed(_) => super::rsa::export_key(scope, format, key_state),
        KeyAlgorithm::Ec(_) => super::ec::export_key(scope, format, key_state),
        KeyAlgorithm::Ed25519 => super::okp::export_ed25519(scope, format, key_state),
        KeyAlgorithm::X25519 => super::okp::export_x25519(scope, format, key_state),
        KeyAlgorithm::Hkdf | KeyAlgorithm::Pbkdf2 => Err(OpError::dom(
            "NotSupportedError",
            "HKDF / PBKDF2 keys are not exportable",
        )),
    }
}

// ---------------------------------------------------------------------------
// deriveBits / deriveKey
// ---------------------------------------------------------------------------

pub fn derive_bits<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    alg: v8::Local<v8::Value>,
    base_key: v8::Local<v8::Value>,
    length: v8::Local<v8::Value>,
) -> Result<Vec<u8>, OpError> {
    let (head, alg_obj) = registry::normalize_head(scope, Operation::DeriveBits, alg)?;
    let key_state = crypto_key::require(scope, base_key)?;
    key_state.check_usage(KeyUsage::DeriveBits)?;
    if !alg_matches_key(head.name, key_state) {
        return Err(OpError::dom(
            "InvalidAccessError",
            format!(
                "Algorithm '{}' does not match key algorithm '{}'",
                head.name.canonical(),
                key_state.algorithm.name()
            ),
        ));
    }
    let length_bits = if length.is_undefined() || length.is_null() {
        None
    } else {
        Some(read_enforce_range_u32(scope, length)?.0)
    };
    match head.name {
        AlgorithmName::Pbkdf2 => super::derive::pbkdf2_derive_bits(scope, alg_obj, key_state, length_bits),
        AlgorithmName::Hkdf => super::derive::hkdf_derive_bits(scope, alg_obj, key_state, length_bits),
        AlgorithmName::Ecdh => super::ec::ecdh_derive_bits(scope, alg_obj, key_state, length_bits),
        AlgorithmName::X25519 => super::okp::x25519_derive_bits(scope, alg_obj, key_state, length_bits),
        _ => Err(OpError::dom(
            "NotSupportedError",
            format!("deriveBits does not support '{}'", head.name.canonical()),
        )),
    }
}

pub fn derive_key<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    alg: v8::Local<v8::Value>,
    base_key: v8::Local<v8::Value>,
    derived_key_alg: v8::Local<v8::Value>,
    extractable: bool,
    usages: v8::Local<v8::Value>,
) -> Result<v8::Local<'s, v8::Object>, OpError> {
    // §14.3.7 deriveKey orchestration: get-key-length on the derived
    // algorithm to figure out how many bits to derive, then call
    // deriveBits, then importKey on the derived bytes.
    let (_, _) = registry::normalize_head(scope, Operation::DeriveBits, alg)?;
    let length_bits = compute_derived_key_length(scope, derived_key_alg)?;

    // Build a Number length argument and call deriveBits with it.
    let length_v: v8::Local<v8::Value> =
        v8::Integer::new_from_unsigned(scope, length_bits).into();
    let bits = derive_bits(scope, alg, base_key, length_v)?;

    // Wrap as a fake ArrayBuffer for importKey "raw".
    let ab = v8::ArrayBuffer::new(scope, bits.len());
    let store = ab.get_backing_store();
    for (i, &b) in bits.iter().enumerate() {
        store[i].set(b);
    }
    let raw_str_v: v8::Local<v8::Value> = v8::String::new(scope, "raw").unwrap().into();
    let _ = raw_str_v;
    import_key(
        scope,
        KeyFormat::Raw,
        ab.into(),
        derived_key_alg,
        extractable,
        usages,
    )
}

fn compute_derived_key_length<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    derived_key_alg: v8::Local<v8::Value>,
) -> Result<u32, OpError> {
    let (head, obj) = registry::normalize_head(scope, Operation::GetKeyLength, derived_key_alg)?;
    match head.name {
        AlgorithmName::AesCtr | AlgorithmName::AesCbc | AlgorithmName::AesGcm | AlgorithmName::AesKw => {
            // length must be one of {128, 192, 256}.
            let key = v8::String::new(scope, "length").unwrap();
            let v = obj.get(scope, key.into()).ok_or_else(|| {
                OpError::dom("OperationError", "AES derivedKey: missing 'length'")
            })?;
            let n = read_enforce_range_u32(scope, v)?.0;
            if !matches!(n, 128 | 192 | 256) {
                return Err(OpError::dom("OperationError", "AES length must be 128, 192, or 256"));
            }
            Ok(n)
        }
        AlgorithmName::Hmac => {
            let length_key = v8::String::new(scope, "length").unwrap();
            let v = obj.get(scope, length_key.into());
            if let Some(v) = v {
                if !v.is_undefined() && !v.is_null() {
                    let n = read_enforce_range_u32(scope, v)?.0;
                    if n == 0 {
                        return Err(OpError::type_error("HMAC length must be > 0"));
                    }
                    return Ok(n);
                }
            }
            // default: block size of hash (D-19).
            let hash = head.hash.ok_or_else(|| {
                OpError::dom("OperationError", "HMAC derivedKey requires 'hash'")
            })?;
            Ok(hash.block_size_bits())
        }
        _ => Err(OpError::dom(
            "NotSupportedError",
            format!(
                "deriveKey does not support deriving '{}'",
                head.name.canonical()
            ),
        )),
    }
}

// ---------------------------------------------------------------------------
// wrapKey / unwrapKey
// ---------------------------------------------------------------------------

pub fn wrap_key<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    format: KeyFormat,
    key: v8::Local<v8::Value>,
    wrapping_key: v8::Local<v8::Value>,
    wrap_alg: v8::Local<v8::Value>,
) -> Result<Vec<u8>, OpError> {
    super::wrap::wrap_key(scope, format, key, wrapping_key, wrap_alg)
}

pub fn unwrap_key<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    format: KeyFormat,
    wrapped_key: v8::Local<v8::Value>,
    unwrapping_key: v8::Local<v8::Value>,
    unwrap_alg: v8::Local<v8::Value>,
    unwrapped_key_alg: v8::Local<v8::Value>,
    extractable: bool,
    usages: v8::Local<v8::Value>,
) -> Result<v8::Local<'s, v8::Object>, OpError> {
    super::wrap::unwrap_key(
        scope,
        format,
        wrapped_key,
        unwrapping_key,
        unwrap_alg,
        unwrapped_key_alg,
        extractable,
        usages,
    )
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Compare a normalized algorithm name to a CryptoKey's stored
/// algorithm.name. Used as the spec's "If `key.algorithm.name` is not
/// equal to `normalizedAlgorithm.name`, throw `InvalidAccessError`."
fn alg_matches_key(name: AlgorithmName, key_state: &CryptoKeyState) -> bool {
    name.canonical() == key_state.algorithm.name()
}

/// Resolve a `_NormalizedHead` head's hash field, defaulting to the
/// key's stored hash if absent (some ops like `RSA-PSS sign` rely on
/// the key's hash slot).
pub(super) fn resolve_hash_or_key_hash(
    head: &NormalizedHead,
    key_state: &CryptoKeyState,
) -> Result<HashAlgo, OpError> {
    if let Some(h) = head.hash {
        return Ok(h);
    }
    match &key_state.algorithm {
        KeyAlgorithm::RsaHashed(r) => Ok(r.hash),
        KeyAlgorithm::Hmac(h) => Ok(h.hash),
        _ => Err(OpError::dom("NotSupportedError", "missing 'hash' field")),
    }
}

// Suppress dead warning for unused enum variants in this stub-heavy
// step. Real uses come online in steps 6-13.
#[allow(dead_code)]
fn _unused(
    _: AesKeyAlgorithm,
    _: HmacKeyAlgorithm,
    _: RsaHashedKeyAlgorithm,
    _: EcKeyAlgorithm,
    _: NamedCurve,
    _: KeyType,
    _: KeyMaterial,
    _: &dyn Fn(v8::Local<v8::Value>) -> Option<Vec<u8>>,
) {
    let _ = read_optional_buffer_source;
}
