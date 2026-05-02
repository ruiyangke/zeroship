//! RSA-OAEP / RSASSA-PKCS1-v1_5 / RSA-PSS — sign/verify/encrypt/decrypt
//! + key gen + import/export. D-15 keygen, D-16 variable PSS salt.
//! Per `docs/proposals/webcrypto-native.md` §IV.7.

#![allow(dead_code)]

use super::crypto_key;
use super::helpers::{read_buffer_source, vec_to_uint8array};
use super::key_material::{
    CryptoKeyState, HashAlgo, KeyAlgorithm, KeyFormat, KeyMaterial, KeyType, KeyUsage,
    RsaHashedKeyAlgorithm, RsaPrivateComponents, RsaPublicComponents,
};
use super::registry::AlgorithmName;
use crate::enforce_range::read_enforce_range_u32;
use crate::state::OpError;

use aws_lc_rs::encoding::AsDer;

// =============================================================================
// RSASSA-PKCS1-v1_5 sign / verify
// =============================================================================

pub fn sign_pkcs1(key: &CryptoKeyState, data: &[u8]) -> Result<Vec<u8>, OpError> {
    if key.key_type != KeyType::Private {
        return Err(OpError::dom(
            "InvalidAccessError",
            "RSA sign requires a private key",
        ));
    }
    let pkcs8 = match &key.material {
        KeyMaterial::RsaPrivate { pkcs8_der, .. } => pkcs8_der,
        _ => {
            return Err(OpError::dom(
                "InvalidAccessError",
                "RSA: missing private material",
            ));
        }
    };
    let hash = require_rsa_hash(key)?;
    let alg: &'static dyn aws_lc_rs::signature::RsaEncoding = match hash {
        HashAlgo::Sha1 => {
            return Err(OpError::dom(
                "NotSupportedError",
                "RSA-PKCS1 sign with SHA-1 not supported",
            ));
        }
        HashAlgo::Sha256 => &aws_lc_rs::signature::RSA_PKCS1_SHA256,
        HashAlgo::Sha384 => &aws_lc_rs::signature::RSA_PKCS1_SHA384,
        HashAlgo::Sha512 => &aws_lc_rs::signature::RSA_PKCS1_SHA512,
    };
    let key_pair = aws_lc_rs::signature::RsaKeyPair::from_pkcs8(pkcs8)
        .map_err(|_| OpError::dom("DataError", "RSA private key load"))?;
    let mut sig = vec![0u8; key_pair.public_modulus_len()];
    let rng = aws_lc_rs::rand::SystemRandom::new();
    key_pair
        .sign(alg, &rng, data, &mut sig)
        .map_err(|_| OpError::dom("OperationError", "RSA-PKCS1 sign failed"))?;
    Ok(sig)
}

pub fn verify_pkcs1(
    key: &CryptoKeyState,
    data: &[u8],
    sig: &[u8],
) -> Result<bool, OpError> {
    if key.key_type != KeyType::Public {
        return Err(OpError::dom(
            "InvalidAccessError",
            "RSA verify requires a public key",
        ));
    }
    let spki = match &key.material {
        KeyMaterial::RsaPublic { spki_der, .. } => spki_der,
        _ => {
            return Err(OpError::dom(
                "InvalidAccessError",
                "RSA: missing public material",
            ));
        }
    };
    let hash = require_rsa_hash(key)?;
    let alg: &dyn aws_lc_rs::signature::VerificationAlgorithm = match hash {
        HashAlgo::Sha1 => &aws_lc_rs::signature::RSA_PKCS1_2048_8192_SHA1_FOR_LEGACY_USE_ONLY,
        HashAlgo::Sha256 => &aws_lc_rs::signature::RSA_PKCS1_2048_8192_SHA256,
        HashAlgo::Sha384 => &aws_lc_rs::signature::RSA_PKCS1_2048_8192_SHA384,
        HashAlgo::Sha512 => &aws_lc_rs::signature::RSA_PKCS1_2048_8192_SHA512,
    };
    let unparsed = aws_lc_rs::signature::UnparsedPublicKey::new(alg, spki.as_slice());
    Ok(unparsed.verify(data, sig).is_ok())
}

// =============================================================================
// RSA-PSS sign / verify (D-16 variable salt)
// =============================================================================

pub fn sign_pss<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    alg_obj: v8::Local<v8::Object>,
    key: &CryptoKeyState,
    data: &[u8],
) -> Result<Vec<u8>, OpError> {
    if key.key_type != KeyType::Private {
        return Err(OpError::dom(
            "InvalidAccessError",
            "RSA-PSS sign requires a private key",
        ));
    }
    let pkcs8 = match &key.material {
        KeyMaterial::RsaPrivate { pkcs8_der, .. } => pkcs8_der,
        _ => return Err(OpError::dom("InvalidAccessError", "RSA: missing private")),
    };
    let hash = require_rsa_hash(key)?;
    let salt_len = read_salt_length(scope, alg_obj)?;
    if matches!(hash, HashAlgo::Sha1) {
        return Err(OpError::dom(
            "NotSupportedError",
            "RSA-PSS with SHA-1 not supported",
        ));
    }
    // Drop down to aws-lc-sys for variable salt length. The high-level
    // aws-lc-rs `signature::RSA_PSS_*` algorithms hard-code salt =
    // digest length, so any caller-specified saltLength other than
    // hLen would otherwise round-trip incorrectly.
    super::rsa_pss_variable_salt::sign_with_salt(pkcs8, hash, data, salt_len as i32)
}

pub fn verify_pss<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    alg_obj: v8::Local<v8::Object>,
    key: &CryptoKeyState,
    data: &[u8],
    sig: &[u8],
) -> Result<bool, OpError> {
    if key.key_type != KeyType::Public {
        return Err(OpError::dom(
            "InvalidAccessError",
            "RSA-PSS verify requires a public key",
        ));
    }
    let spki = match &key.material {
        KeyMaterial::RsaPublic { spki_der, .. } => spki_der,
        _ => return Err(OpError::dom("InvalidAccessError", "RSA: missing public")),
    };
    let hash = require_rsa_hash(key)?;
    if matches!(hash, HashAlgo::Sha1) {
        return Err(OpError::dom(
            "NotSupportedError",
            "RSA-PSS with SHA-1 not supported",
        ));
    }
    let salt_len = read_salt_length(scope, alg_obj)?;
    super::rsa_pss_variable_salt::verify_with_salt(spki, hash, data, sig, salt_len as i32)
}

fn read_salt_length(
    scope: &mut v8::PinScope,
    alg_obj: v8::Local<v8::Object>,
) -> Result<u32, OpError> {
    let key = v8::String::new(scope, "saltLength").unwrap();
    let v = alg_obj.get(scope, key.into()).ok_or_else(|| {
        OpError::type_error("RsaPssParams: missing 'saltLength'")
    })?;
    Ok(read_enforce_range_u32(scope, v)?.0)
}

// =============================================================================
// RSA-OAEP encrypt / decrypt
// =============================================================================

pub fn encrypt_oaep<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    alg_obj: v8::Local<v8::Object>,
    key: &CryptoKeyState,
    data: &[u8],
) -> Result<Vec<u8>, OpError> {
    if key.key_type != KeyType::Public {
        return Err(OpError::dom(
            "InvalidAccessError",
            "RSA-OAEP encrypt requires a public key",
        ));
    }
    let spki = match &key.material {
        KeyMaterial::RsaPublic { spki_der, .. } => spki_der,
        _ => return Err(OpError::dom("InvalidAccessError", "RSA: missing public")),
    };
    let hash = require_rsa_hash(key)?;
    let label = read_optional_label(scope, alg_obj)?;
    let oaep_alg = oaep_algorithm(hash)?;
    let pub_key = aws_lc_rs::rsa::OaepPublicEncryptingKey::new(
        aws_lc_rs::rsa::PublicEncryptingKey::from_der(spki)
            .map_err(|_| OpError::dom("DataError", "RSA public key load"))?,
    )
    .map_err(|_| OpError::dom("OperationError", "RSA-OAEP key wrap"))?;
    let mut out = vec![0u8; pub_key.ciphertext_size()];
    let label_opt: Option<&[u8]> = label.as_deref();
    let written = pub_key
        .encrypt(oaep_alg, data, &mut out, label_opt)
        .map_err(|_| OpError::dom("OperationError", "RSA-OAEP encrypt failed"))?;
    Ok(written.to_vec())
}

pub fn decrypt_oaep<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    alg_obj: v8::Local<v8::Object>,
    key: &CryptoKeyState,
    data: &[u8],
) -> Result<Vec<u8>, OpError> {
    if key.key_type != KeyType::Private {
        return Err(OpError::dom(
            "InvalidAccessError",
            "RSA-OAEP decrypt requires a private key",
        ));
    }
    let pkcs8 = match &key.material {
        KeyMaterial::RsaPrivate { pkcs8_der, .. } => pkcs8_der,
        _ => return Err(OpError::dom("InvalidAccessError", "RSA: missing private")),
    };
    let hash = require_rsa_hash(key)?;
    let label = read_optional_label(scope, alg_obj)?;
    let oaep_alg = oaep_algorithm(hash)?;
    let priv_key = aws_lc_rs::rsa::OaepPrivateDecryptingKey::new(
        aws_lc_rs::rsa::PrivateDecryptingKey::from_pkcs8(pkcs8)
            .map_err(|_| OpError::dom("DataError", "RSA private key load"))?,
    )
    .map_err(|_| OpError::dom("OperationError", "RSA-OAEP key wrap"))?;
    let mut out = vec![0u8; priv_key.min_output_size()];
    let label_opt: Option<&[u8]> = label.as_deref();
    let pt = priv_key
        .decrypt(oaep_alg, data, &mut out, label_opt)
        .map_err(|_| OpError::dom("OperationError", "RSA-OAEP decrypt failed"))?;
    Ok(pt.to_vec())
}

fn oaep_algorithm(hash: HashAlgo) -> Result<&'static aws_lc_rs::rsa::OaepAlgorithm, OpError> {
    match hash {
        HashAlgo::Sha256 => Ok(&aws_lc_rs::rsa::OAEP_SHA256_MGF1SHA256),
        HashAlgo::Sha384 => Ok(&aws_lc_rs::rsa::OAEP_SHA384_MGF1SHA384),
        HashAlgo::Sha512 => Ok(&aws_lc_rs::rsa::OAEP_SHA512_MGF1SHA512),
        HashAlgo::Sha1 => Err(OpError::dom(
            "NotSupportedError",
            "RSA-OAEP with SHA-1 not supported",
        )),
    }
}

fn read_optional_label(
    scope: &mut v8::PinScope,
    alg_obj: v8::Local<v8::Object>,
) -> Result<Option<Vec<u8>>, OpError> {
    let key = v8::String::new(scope, "label").unwrap();
    let v = match alg_obj.get(scope, key.into()) {
        Some(v) => v,
        None => return Ok(None),
    };
    if v.is_undefined() || v.is_null() {
        return Ok(None);
    }
    Ok(Some(read_buffer_source(scope, v)?))
}

// =============================================================================
// generateKey (D-15)
// =============================================================================

pub fn generate_key<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    alg: AlgorithmName,
    alg_obj: v8::Local<v8::Object>,
    extractable: bool,
    usages: &[KeyUsage],
    hash: Option<HashAlgo>,
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    validate_rsa_usages(alg, usages)?;
    let modulus_v = alg_obj
        .get(scope, v8::String::new(scope, "modulusLength").unwrap().into())
        .ok_or_else(|| OpError::type_error("RsaKeyGenParams: missing 'modulusLength'"))?;
    let modulus_length = read_enforce_range_u32(scope, modulus_v)?.0;
    if !(1024..=16384).contains(&modulus_length) || modulus_length % 8 != 0 {
        return Err(OpError::dom(
            "OperationError",
            format!("RSA modulusLength {modulus_length} out of supported range"),
        ));
    }
    let pub_exp_v = alg_obj
        .get(scope, v8::String::new(scope, "publicExponent").unwrap().into())
        .ok_or_else(|| OpError::type_error("RsaKeyGenParams: missing 'publicExponent'"))?;
    let pub_exp = read_buffer_source(scope, pub_exp_v)?;
    // Validate publicExponent: 3 or 65537 only (FIPS 186-5).
    let exp_val = bigint_to_u64(&pub_exp);
    if !(exp_val == Some(3) || exp_val == Some(65537)) {
        return Err(OpError::dom(
            "OperationError",
            "RSA publicExponent must be 3 or 65537",
        ));
    }
    let hash = hash.ok_or_else(|| {
        OpError::dom("NotSupportedError", "RSA generateKey requires 'hash'")
    })?;
    let bits = modulus_length;
    let key_size = match bits {
        2048 => aws_lc_rs::rsa::KeySize::Rsa2048,
        3072 => aws_lc_rs::rsa::KeySize::Rsa3072,
        4096 => aws_lc_rs::rsa::KeySize::Rsa4096,
        8192 => aws_lc_rs::rsa::KeySize::Rsa8192,
        n => {
            return Err(OpError::dom(
                "OperationError",
                format!("RSA modulusLength {n} not supported (use 2048/3072/4096/8192)"),
            ));
        }
    };
    let priv_key = aws_lc_rs::rsa::PrivateDecryptingKey::generate(key_size)
        .map_err(|_| OpError::dom("OperationError", "RSA keygen failed"))?;
    let pkcs8_doc: aws_lc_rs::encoding::Pkcs8V1Der<'static> = AsDer::as_der(&priv_key)
        .map_err(|_| OpError::dom("OperationError", "RSA private DER"))?;
    let pkcs8 = pkcs8_doc.as_ref().to_vec();
    let pub_part = priv_key.public_key();
    let spki_doc: aws_lc_rs::encoding::PublicKeyX509Der<'static> = AsDer::as_der(&pub_part)
        .map_err(|_| OpError::dom("OperationError", "RSA public DER"))?;
    let spki = spki_doc.as_ref().to_vec();

    let pub_components = RsaPublicComponents {
        n: Vec::new(),
        e: pub_exp.clone(),
    };
    let priv_components = RsaPrivateComponents {
        n: Vec::new(),
        e: pub_exp.clone(),
        d: Vec::new(),
        p: Vec::new(),
        q: Vec::new(),
        dp: Vec::new(),
        dq: Vec::new(),
        qi: Vec::new(),
    };

    let (priv_usages, pub_usages) = split_rsa_usages(alg, usages);
    let priv_state = CryptoKeyState {
        key_type: KeyType::Private,
        extractable,
        algorithm: KeyAlgorithm::RsaHashed(RsaHashedKeyAlgorithm {
            name: alg.canonical(),
            modulus_length,
            public_exponent: pub_exp.clone(),
            hash,
        }),
        usages: priv_usages,
        material: KeyMaterial::RsaPrivate {
            pkcs8_der: pkcs8.clone(),
            components: priv_components,
        },
    };
    let pub_state = CryptoKeyState {
        key_type: KeyType::Public,
        extractable: true,
        algorithm: KeyAlgorithm::RsaHashed(RsaHashedKeyAlgorithm {
            name: alg.canonical(),
            modulus_length,
            public_exponent: pub_exp,
            hash,
        }),
        usages: pub_usages,
        material: KeyMaterial::RsaPublic {
            spki_der: spki,
            components: pub_components,
        },
    };
    let priv_obj = crypto_key::build(scope, priv_state);
    let pub_obj = crypto_key::build(scope, pub_state);
    let pair = v8::Object::new(scope);
    let priv_k = v8::String::new(scope, "privateKey").unwrap();
    let pub_k = v8::String::new(scope, "publicKey").unwrap();
    pair.set(scope, priv_k.into(), priv_obj.into());
    pair.set(scope, pub_k.into(), pub_obj.into());
    Ok(pair.into())
}

fn bigint_to_u64(bytes: &[u8]) -> Option<u64> {
    if bytes.len() > 8 {
        return None;
    }
    let mut x: u64 = 0;
    for &b in bytes {
        x = (x << 8) | (b as u64);
    }
    Some(x)
}

// =============================================================================
// importKey / exportKey
// =============================================================================

pub fn import_key<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    alg: AlgorithmName,
    format: KeyFormat,
    key_data: v8::Local<v8::Value>,
    _alg_obj: v8::Local<v8::Object>,
    extractable: bool,
    usages: &[KeyUsage],
    hash: Option<HashAlgo>,
) -> Result<v8::Local<'s, v8::Object>, OpError> {
    validate_rsa_usages(alg, usages)?;
    let hash = hash.ok_or_else(|| {
        OpError::dom("NotSupportedError", "RSA importKey requires 'hash'")
    })?;
    match format {
        KeyFormat::Spki => {
            let bytes = read_buffer_source(scope, key_data)?;
            let pub_key = aws_lc_rs::rsa::PublicEncryptingKey::from_der(&bytes)
                .map_err(|_| OpError::dom("DataError", "RSA SPKI parse failed"))?;
            let modulus_bits = (pub_key.key_size_bytes() as u32) * 8;
            let state = CryptoKeyState {
                key_type: KeyType::Public,
                extractable,
                algorithm: KeyAlgorithm::RsaHashed(RsaHashedKeyAlgorithm {
                    name: alg.canonical(),
                    modulus_length: modulus_bits,
                    public_exponent: vec![0x01, 0x00, 0x01],
                    hash,
                }),
                usages: usages.to_vec(),
                material: KeyMaterial::RsaPublic {
                    spki_der: bytes,
                    components: RsaPublicComponents {
                        n: Vec::new(),
                        e: vec![0x01, 0x00, 0x01],
                    },
                },
            };
            Ok(crypto_key::build(scope, state))
        }
        KeyFormat::Pkcs8 => {
            let bytes = read_buffer_source(scope, key_data)?;
            let priv_key = aws_lc_rs::rsa::PrivateDecryptingKey::from_pkcs8(&bytes)
                .map_err(|_| OpError::dom("DataError", "RSA PKCS#8 parse failed"))?;
            let modulus_bits = (priv_key.key_size_bytes() as u32) * 8;
            let state = CryptoKeyState {
                key_type: KeyType::Private,
                extractable,
                algorithm: KeyAlgorithm::RsaHashed(RsaHashedKeyAlgorithm {
                    name: alg.canonical(),
                    modulus_length: modulus_bits,
                    public_exponent: vec![0x01, 0x00, 0x01],
                    hash,
                }),
                usages: usages.to_vec(),
                material: KeyMaterial::RsaPrivate {
                    pkcs8_der: bytes,
                    components: RsaPrivateComponents {
                        n: Vec::new(),
                        e: vec![0x01, 0x00, 0x01],
                        d: Vec::new(),
                        p: Vec::new(),
                        q: Vec::new(),
                        dp: Vec::new(),
                        dq: Vec::new(),
                        qi: Vec::new(),
                    },
                },
            };
            Ok(crypto_key::build(scope, state))
        }
        KeyFormat::Jwk => super::jwk::import_rsa(scope, alg, hash, key_data, extractable, usages),
        _ => Err(OpError::dom(
            "NotSupportedError",
            "RSA import format must be spki/pkcs8/jwk",
        )),
    }
}

pub fn export_key<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    format: KeyFormat,
    key: &CryptoKeyState,
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    match format {
        KeyFormat::Spki => match &key.material {
            KeyMaterial::RsaPublic { spki_der, .. } => Ok(vec_to_uint8array(scope, spki_der)),
            _ => Err(OpError::dom("InvalidAccessError", "RSA: not a public key")),
        },
        KeyFormat::Pkcs8 => match &key.material {
            KeyMaterial::RsaPrivate { pkcs8_der, .. } => Ok(vec_to_uint8array(scope, pkcs8_der)),
            _ => Err(OpError::dom("InvalidAccessError", "RSA: not a private key")),
        },
        KeyFormat::Jwk => super::jwk::export_rsa(scope, key),
        _ => Err(OpError::dom(
            "NotSupportedError",
            "RSA export format must be spki/pkcs8/jwk",
        )),
    }
}

// -----------------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------------

fn validate_rsa_usages(alg: AlgorithmName, usages: &[KeyUsage]) -> Result<(), OpError> {
    let allowed: &[KeyUsage] = match alg {
        AlgorithmName::RsaOaep => &[
            KeyUsage::Encrypt,
            KeyUsage::Decrypt,
            KeyUsage::WrapKey,
            KeyUsage::UnwrapKey,
        ],
        AlgorithmName::RsassaPkcs1v15 | AlgorithmName::RsaPss => {
            &[KeyUsage::Sign, KeyUsage::Verify]
        }
        _ => &[],
    };
    for u in usages {
        if !allowed.contains(u) {
            return Err(OpError::dom(
                "SyntaxError",
                format!("Usage '{}' not allowed for {}", u.as_str(), alg.canonical()),
            ));
        }
    }
    Ok(())
}

fn split_rsa_usages(
    alg: AlgorithmName,
    usages: &[KeyUsage],
) -> (Vec<KeyUsage>, Vec<KeyUsage>) {
    let mut p = Vec::new();
    let mut q = Vec::new();
    for &u in usages {
        match (alg, u) {
            (AlgorithmName::RsassaPkcs1v15 | AlgorithmName::RsaPss, KeyUsage::Sign) => p.push(u),
            (AlgorithmName::RsassaPkcs1v15 | AlgorithmName::RsaPss, KeyUsage::Verify) => q.push(u),
            (AlgorithmName::RsaOaep, KeyUsage::Decrypt | KeyUsage::UnwrapKey) => p.push(u),
            (AlgorithmName::RsaOaep, KeyUsage::Encrypt | KeyUsage::WrapKey) => q.push(u),
            _ => {}
        }
    }
    (p, q)
}

fn require_rsa_hash(key: &CryptoKeyState) -> Result<HashAlgo, OpError> {
    match &key.algorithm {
        KeyAlgorithm::RsaHashed(r) => Ok(r.hash),
        _ => Err(OpError::dom("InvalidAccessError", "Not an RSA-hashed key")),
    }
}
