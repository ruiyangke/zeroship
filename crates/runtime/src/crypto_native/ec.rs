//! ECDSA + ECDH per spec §§23-24. D-4 fixed-length r∥s wire format.
//! D-13 ECDH deriveBits. D-14 P-521 support.

#![allow(dead_code)]

use super::crypto_key;
use super::helpers::{read_buffer_source, vec_to_uint8array};
use super::key_material::{
    CryptoKeyState, EcKeyAlgorithm, HashAlgo, KeyAlgorithm, KeyFormat, KeyMaterial, KeyType,
    KeyUsage, NamedCurve,
};
use super::registry::AlgorithmName;
use crate::state::OpError;

use aws_lc_rs::encoding::AsDer;

// -----------------------------------------------------------------------------
// ECDSA sign/verify — D-4 FIXED_SIGNING wire format
// -----------------------------------------------------------------------------

pub fn sign_ecdsa<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    alg_obj: v8::Local<v8::Object>,
    key: &CryptoKeyState,
    data: &[u8],
) -> Result<Vec<u8>, OpError> {
    if key.key_type != KeyType::Private {
        return Err(OpError::dom(
            "InvalidAccessError",
            "ECDSA sign requires a private key",
        ));
    }
    let curve = require_ec_curve(key)?;
    let hash = read_hash_or_err(scope, alg_obj)?;
    let pkcs8 = match &key.material {
        KeyMaterial::EcPrivate { pkcs8_der, .. } => pkcs8_der,
        _ => {
            return Err(OpError::dom(
                "InvalidAccessError",
                "ECDSA: missing private key material",
            ));
        }
    };
    let alg = ecdsa_signing_alg(curve, hash)?;
    let rng = aws_lc_rs::rand::SystemRandom::new();
    let key_pair = aws_lc_rs::signature::EcdsaKeyPair::from_pkcs8(alg, pkcs8)
        .map_err(|_| OpError::dom("DataError", "ECDSA key construction failed"))?;
    let sig = key_pair
        .sign(&rng, data)
        .map_err(|_| OpError::dom("OperationError", "ECDSA sign failed"))?;
    Ok(sig.as_ref().to_vec())
}

pub fn verify_ecdsa<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    alg_obj: v8::Local<v8::Object>,
    key: &CryptoKeyState,
    sig: &[u8],
    data: &[u8],
) -> Result<bool, OpError> {
    if key.key_type != KeyType::Public {
        return Err(OpError::dom(
            "InvalidAccessError",
            "ECDSA verify requires a public key",
        ));
    }
    let curve = require_ec_curve(key)?;
    let hash = read_hash_or_err(scope, alg_obj)?;
    // Spec step 3: signature must be 2n bytes.
    let n = curve.order_len();
    if sig.len() != 2 * n {
        return Ok(false);
    }
    let raw_xy = match &key.material {
        KeyMaterial::EcPublic { raw_xy, .. } => raw_xy,
        KeyMaterial::EcPrivate { raw_xy, .. } => raw_xy,
        _ => {
            return Err(OpError::dom(
                "InvalidAccessError",
                "ECDSA: missing public key material",
            ));
        }
    };
    let alg = ecdsa_verify_alg(curve, hash)?;
    let unparsed = aws_lc_rs::signature::UnparsedPublicKey::new(alg, raw_xy.as_slice());
    Ok(unparsed.verify(data, sig).is_ok())
}

fn ecdsa_signing_alg(
    curve: NamedCurve,
    hash: HashAlgo,
) -> Result<&'static aws_lc_rs::signature::EcdsaSigningAlgorithm, OpError> {
    use aws_lc_rs::signature as s;
    match (curve, hash) {
        (NamedCurve::P256, HashAlgo::Sha256) => Ok(&s::ECDSA_P256_SHA256_FIXED_SIGNING),
        (NamedCurve::P384, HashAlgo::Sha384) => Ok(&s::ECDSA_P384_SHA384_FIXED_SIGNING),
        (NamedCurve::P521, HashAlgo::Sha512) => Ok(&s::ECDSA_P521_SHA512_FIXED_SIGNING),
        _ => Err(OpError::dom(
            "NotSupportedError",
            format!(
                "ECDSA: unsupported curve/hash pair ({}/{})",
                curve.as_str(),
                hash.as_str()
            ),
        )),
    }
}

fn ecdsa_verify_alg(
    curve: NamedCurve,
    hash: HashAlgo,
) -> Result<&'static aws_lc_rs::signature::EcdsaVerificationAlgorithm, OpError> {
    use aws_lc_rs::signature as s;
    match (curve, hash) {
        (NamedCurve::P256, HashAlgo::Sha256) => Ok(&s::ECDSA_P256_SHA256_FIXED),
        (NamedCurve::P384, HashAlgo::Sha384) => Ok(&s::ECDSA_P384_SHA384_FIXED),
        (NamedCurve::P521, HashAlgo::Sha512) => Ok(&s::ECDSA_P521_SHA512_FIXED),
        _ => Err(OpError::dom(
            "NotSupportedError",
            format!(
                "ECDSA: unsupported curve/hash pair ({}/{})",
                curve.as_str(),
                hash.as_str()
            ),
        )),
    }
}

// -----------------------------------------------------------------------------
// ECDH deriveBits (D-13)
// -----------------------------------------------------------------------------

pub fn ecdh_derive_bits<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    alg_obj: v8::Local<v8::Object>,
    key: &CryptoKeyState,
    length_bits: Option<u32>,
) -> Result<Vec<u8>, OpError> {
    if key.key_type != KeyType::Private {
        return Err(OpError::dom(
            "InvalidAccessError",
            "ECDH deriveBits requires a private key",
        ));
    }
    let priv_curve = require_ec_curve(key)?;

    // Spec §24.6.1 step 2-5: read normalizedAlgorithm.public.
    let public_v = alg_obj
        .get(scope, v8::String::new(scope, "public").unwrap().into())
        .ok_or_else(|| OpError::type_error("EcdhKeyDeriveParams: missing 'public'"))?;
    let pub_state = crypto_key::require(scope, public_v)?;
    if pub_state.key_type != KeyType::Public {
        return Err(OpError::dom(
            "InvalidAccessError",
            "ECDH 'public' key must be a public key",
        ));
    }
    let pub_curve = require_ec_curve(pub_state)?;
    if pub_curve != priv_curve {
        return Err(OpError::dom(
            "InvalidAccessError",
            "ECDH curve mismatch between private and public keys",
        ));
    }

    let priv_pkcs8 = match &key.material {
        KeyMaterial::EcPrivate { pkcs8_der, .. } => pkcs8_der,
        _ => {
            return Err(OpError::dom(
                "InvalidAccessError",
                "ECDH: missing private material",
            ));
        }
    };
    let pub_xy = match &pub_state.material {
        KeyMaterial::EcPublic { raw_xy, .. } => raw_xy,
        KeyMaterial::EcPrivate { raw_xy, .. } => raw_xy,
        _ => {
            return Err(OpError::dom(
                "InvalidAccessError",
                "ECDH: missing public material",
            ));
        }
    };

    let alg = match priv_curve {
        NamedCurve::P256 => &aws_lc_rs::agreement::ECDH_P256,
        NamedCurve::P384 => &aws_lc_rs::agreement::ECDH_P384,
        NamedCurve::P521 => &aws_lc_rs::agreement::ECDH_P521,
    };
    let priv_key = aws_lc_rs::agreement::PrivateKey::from_private_key_der(alg, priv_pkcs8)
        .map_err(|_| OpError::dom("OperationError", "ECDH private key load"))?;
    let peer = aws_lc_rs::agreement::UnparsedPublicKey::new(alg, pub_xy.as_slice());
    let dom_err = OpError::dom("OperationError", "ECDH agreement failed");
    let shared = aws_lc_rs::agreement::agree(&priv_key, &peer, dom_err, |z: &[u8]| {
        Ok::<Vec<u8>, OpError>(z.to_vec())
    })?;

    truncate_to_bits(&shared, length_bits)
}

pub fn truncate_to_bits_pub(data: &[u8], length_bits: Option<u32>) -> Result<Vec<u8>, OpError> {
    truncate_to_bits(data, length_bits)
}

fn truncate_to_bits(data: &[u8], length_bits: Option<u32>) -> Result<Vec<u8>, OpError> {
    let length = match length_bits {
        Some(n) => n as usize,
        None => return Ok(data.to_vec()),
    };
    let avail = data.len() * 8;
    if length > avail {
        return Err(OpError::dom(
            "OperationError",
            format!("derived length {length} exceeds available {avail} bits"),
        ));
    }
    let bytes = (length + 7) / 8;
    let mut out = data[..bytes].to_vec();
    let extra_bits = (bytes * 8) - length;
    if extra_bits > 0 && !out.is_empty() {
        let mask = (0xff_u8 << extra_bits) & 0xff;
        let last = out.len() - 1;
        out[last] &= mask;
    }
    Ok(out)
}

// -----------------------------------------------------------------------------
// generateKey
// -----------------------------------------------------------------------------

pub fn generate_key<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    alg: AlgorithmName,
    alg_obj: v8::Local<v8::Object>,
    extractable: bool,
    usages: &[KeyUsage],
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    validate_ec_usages(alg, usages)?;
    let curve = read_named_curve(scope, alg_obj)?;
    // For ECDSA: usages split — privKey gets sign, pubKey gets verify.
    // For ECDH: privKey gets deriveBits/deriveKey; pubKey gets [].
    let (priv_usages, pub_usages) = split_ec_usages(alg, usages);
    let (pkcs8, raw_xy): (Vec<u8>, Vec<u8>) = match alg {
        AlgorithmName::Ecdsa => {
            // We use the Ecdsa keygen with the right SIGNING alg; the
            // shape is identical for verifies.
            let signing = match curve {
                NamedCurve::P256 => &aws_lc_rs::signature::ECDSA_P256_SHA256_FIXED_SIGNING,
                NamedCurve::P384 => &aws_lc_rs::signature::ECDSA_P384_SHA384_FIXED_SIGNING,
                NamedCurve::P521 => &aws_lc_rs::signature::ECDSA_P521_SHA512_FIXED_SIGNING,
            };
            let rng = aws_lc_rs::rand::SystemRandom::new();
            let pkcs8 = aws_lc_rs::signature::EcdsaKeyPair::generate_pkcs8(signing, &rng)
                .map_err(|_| OpError::dom("OperationError", "ECDSA keygen failed"))?;
            let pkcs8_bytes = pkcs8.as_ref().to_vec();
            let key_pair = aws_lc_rs::signature::EcdsaKeyPair::from_pkcs8(signing, &pkcs8_bytes)
                .map_err(|_| OpError::dom("OperationError", "ECDSA keygen reload"))?;
            use aws_lc_rs::signature::KeyPair as _;
            let raw_xy = key_pair.public_key().as_ref().to_vec();
            (pkcs8_bytes, raw_xy)
        }
        AlgorithmName::Ecdh => {
            let agreement = match curve {
                NamedCurve::P256 => &aws_lc_rs::agreement::ECDH_P256,
                NamedCurve::P384 => &aws_lc_rs::agreement::ECDH_P384,
                NamedCurve::P521 => &aws_lc_rs::agreement::ECDH_P521,
            };
            let priv_key = aws_lc_rs::agreement::PrivateKey::generate(agreement)
                .map_err(|_| OpError::dom("OperationError", "ECDH keygen failed"))?;
            let public = priv_key
                .compute_public_key()
                .map_err(|_| OpError::dom("OperationError", "ECDH pub derive"))?;
            let pkcs8_doc: aws_lc_rs::encoding::Pkcs8V1Der<'static> =
                AsDer::as_der(&priv_key).map_err(|_| {
                    OpError::dom("OperationError", "ECDH private DER")
                })?;
            let pkcs8 = pkcs8_doc.as_ref().to_vec();
            (pkcs8, public.as_ref().to_vec())
        }
        _ => {
            return Err(OpError::dom(
                "NotSupportedError",
                "EC generateKey: not ECDSA or ECDH",
            ));
        }
    };

    let priv_state = CryptoKeyState {
        key_type: KeyType::Private,
        extractable,
        algorithm: KeyAlgorithm::Ec(EcKeyAlgorithm {
            name: alg.canonical(),
            named_curve: curve,
        }),
        usages: priv_usages,
        material: KeyMaterial::EcPrivate {
            pkcs8_der: pkcs8.clone(),
            raw_d: Vec::new(), // populated lazily via DER parse for JWK
            raw_xy: raw_xy.clone(),
        },
    };
    let pub_state = CryptoKeyState {
        key_type: KeyType::Public,
        extractable: true,
        algorithm: KeyAlgorithm::Ec(EcKeyAlgorithm {
            name: alg.canonical(),
            named_curve: curve,
        }),
        usages: pub_usages,
        material: KeyMaterial::EcPublic {
            spki_der: Vec::new(), // populated lazily
            raw_xy,
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

// -----------------------------------------------------------------------------
// importKey / exportKey — raw + jwk (spki/pkcs8 deferred — full DER walker
// required; the JWK path covers the key WPT cases).
// -----------------------------------------------------------------------------

pub fn import_key<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    alg: AlgorithmName,
    format: KeyFormat,
    key_data: v8::Local<v8::Value>,
    alg_obj: v8::Local<v8::Object>,
    extractable: bool,
    usages: &[KeyUsage],
) -> Result<v8::Local<'s, v8::Object>, OpError> {
    validate_ec_usages(alg, usages)?;
    let curve = read_named_curve(scope, alg_obj)?;
    match format {
        KeyFormat::Raw => {
            let bytes = read_buffer_source(scope, key_data)?;
            let n = curve.order_len();
            let expected = 1 + 2 * n;
            if bytes.len() != expected || bytes[0] != 0x04 {
                return Err(OpError::dom(
                    "DataError",
                    format!(
                        "Invalid uncompressed EC point ({} bytes, expected {})",
                        bytes.len(),
                        expected
                    ),
                ));
            }
            // Per spec public EC keys imported via "raw" only.
            let state = CryptoKeyState {
                key_type: KeyType::Public,
                extractable,
                algorithm: KeyAlgorithm::Ec(EcKeyAlgorithm {
                    name: alg.canonical(),
                    named_curve: curve,
                }),
                usages: usages.to_vec(),
                material: KeyMaterial::EcPublic {
                    spki_der: Vec::new(),
                    raw_xy: bytes,
                },
            };
            Ok(crypto_key::build(scope, state))
        }
        KeyFormat::Jwk => {
            super::jwk::import_ec(scope, alg, curve, key_data, extractable, usages)
        }
        KeyFormat::Spki => import_spki(scope, alg, curve, key_data, extractable, usages),
        KeyFormat::Pkcs8 => import_pkcs8(scope, alg, curve, key_data, extractable, usages),
    }
}

fn import_spki<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    alg: AlgorithmName,
    curve: NamedCurve,
    key_data: v8::Local<v8::Value>,
    extractable: bool,
    usages: &[KeyUsage],
) -> Result<v8::Local<'s, v8::Object>, OpError> {
    let bytes = read_buffer_source(scope, key_data)?;
    // SPKI is `SubjectPublicKeyInfo ::= SEQUENCE { algorithm
    // AlgorithmIdentifier, subjectPublicKey BIT STRING }`. Extract the
    // BIT STRING contents (the uncompressed point) by walking the DER.
    let raw_xy = parse_ec_spki(&bytes, curve).ok_or_else(|| {
        OpError::dom("DataError", "EC SPKI parse failed")
    })?;
    let state = CryptoKeyState {
        key_type: KeyType::Public,
        extractable,
        algorithm: KeyAlgorithm::Ec(EcKeyAlgorithm {
            name: alg.canonical(),
            named_curve: curve,
        }),
        usages: usages.to_vec(),
        material: KeyMaterial::EcPublic {
            spki_der: bytes,
            raw_xy,
        },
    };
    Ok(crypto_key::build(scope, state))
}

fn import_pkcs8<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    alg: AlgorithmName,
    curve: NamedCurve,
    key_data: v8::Local<v8::Value>,
    extractable: bool,
    usages: &[KeyUsage],
) -> Result<v8::Local<'s, v8::Object>, OpError> {
    let bytes = read_buffer_source(scope, key_data)?;
    // Validate by reloading via aws-lc-rs (also catches algorithm/
    // curve-OID mismatches). Then walk the PKCS#8 to extract raw d.
    let signing_alg = match curve {
        NamedCurve::P256 => &aws_lc_rs::signature::ECDSA_P256_SHA256_FIXED_SIGNING,
        NamedCurve::P384 => &aws_lc_rs::signature::ECDSA_P384_SHA384_FIXED_SIGNING,
        NamedCurve::P521 => &aws_lc_rs::signature::ECDSA_P521_SHA512_FIXED_SIGNING,
    };
    let key_pair = aws_lc_rs::signature::EcdsaKeyPair::from_pkcs8(signing_alg, &bytes)
        .map_err(|_| OpError::dom("DataError", "EC PKCS#8 parse failed"))?;
    use aws_lc_rs::signature::KeyPair as _;
    let raw_xy = key_pair.public_key().as_ref().to_vec();
    let n = curve.order_len();
    let raw_d = super::der::extract_ec_raw_d(&bytes, n)
        .ok_or_else(|| OpError::dom("DataError", "EC PKCS#8 scalar extract failed"))?;
    let state = CryptoKeyState {
        key_type: KeyType::Private,
        extractable,
        algorithm: KeyAlgorithm::Ec(EcKeyAlgorithm {
            name: alg.canonical(),
            named_curve: curve,
        }),
        usages: usages.to_vec(),
        material: KeyMaterial::EcPrivate {
            pkcs8_der: bytes,
            raw_d,
            raw_xy,
        },
    };
    Ok(crypto_key::build(scope, state))
}

/// Walk SubjectPublicKeyInfo to extract the EC public point. Strict
/// shape: SEQUENCE { SEQUENCE { ecPublicKey-OID, curve-OID }, BIT
/// STRING uncompressed-point }.
fn parse_ec_spki(spki: &[u8], curve: NamedCurve) -> Option<Vec<u8>> {
    use super::der::*;
    // For brevity reuse the public TLV walker via repeat parses.
    let (top, rest) = read_tlv_pub(spki)?;
    if !rest.is_empty() || top.tag != 0x30 {
        return None;
    }
    let body = top.value;
    let (alg_id, body) = read_tlv_pub(body)?;
    if alg_id.tag != 0x30 {
        return None;
    }
    // alg-id contents: ecPublicKey OID + curve OID. We don't strictly
    // validate the curve OID against the named-curve here (caller
    // already specified it), but we do require ecPublicKey first.
    let (oid_alg, params) = read_tlv_pub(alg_id.value)?;
    if oid_alg.tag != 0x06 {
        return None;
    }
    // ecPublicKey OID = 1.2.840.10045.2.1
    if oid_alg.value != [0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01] {
        return None;
    }
    // Validate the curve OID matches.
    if !params.is_empty() {
        let (oid_curve, _) = read_tlv_pub(params)?;
        if oid_curve.tag != 0x06 {
            return None;
        }
        let expected = match curve {
            NamedCurve::P256 => &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07][..],
            NamedCurve::P384 => &[0x2b, 0x81, 0x04, 0x00, 0x22][..],
            NamedCurve::P521 => &[0x2b, 0x81, 0x04, 0x00, 0x23][..],
        };
        if oid_curve.value != expected {
            return None;
        }
    }
    let (bit_string, _) = read_tlv_pub(body)?;
    if bit_string.tag != 0x03 || bit_string.value.is_empty() {
        return None;
    }
    // First byte is unused-bits count (0 for X.509 keys).
    if bit_string.value[0] != 0 {
        return None;
    }
    let point = &bit_string.value[1..];
    let n = curve.order_len();
    if point.len() != 1 + 2 * n || point[0] != 0x04 {
        return None;
    }
    Some(point.to_vec())
}

pub fn export_key<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    format: KeyFormat,
    key: &CryptoKeyState,
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    match format {
        KeyFormat::Raw => {
            let raw_xy = match &key.material {
                KeyMaterial::EcPublic { raw_xy, .. } => raw_xy,
                KeyMaterial::EcPrivate { raw_xy, .. } => raw_xy,
                _ => return Err(OpError::dom("OperationError", "Not an EC key")),
            };
            if key.key_type == KeyType::Private {
                return Err(OpError::dom(
                    "NotSupportedError",
                    "EC private keys cannot export 'raw'",
                ));
            }
            Ok(vec_to_uint8array(scope, raw_xy))
        }
        KeyFormat::Jwk => super::jwk::export_ec(scope, key),
        KeyFormat::Spki => {
            let curve = require_ec_curve(key)?;
            match &key.material {
                KeyMaterial::EcPublic { spki_der, raw_xy } => {
                    if !spki_der.is_empty() {
                        Ok(vec_to_uint8array(scope, spki_der))
                    } else {
                        let der = build_ec_spki(curve, raw_xy)?;
                        Ok(vec_to_uint8array(scope, &der))
                    }
                }
                _ => Err(OpError::dom(
                    "InvalidAccessError",
                    "EC SPKI export requires a public key",
                )),
            }
        }
        KeyFormat::Pkcs8 => match &key.material {
            KeyMaterial::EcPrivate { pkcs8_der, .. } => Ok(vec_to_uint8array(scope, pkcs8_der)),
            _ => Err(OpError::dom(
                "InvalidAccessError",
                "EC PKCS#8 export requires a private key",
            )),
        },
    }
}

/// Hand-build an X.509 SubjectPublicKeyInfo for an EC public point.
fn build_ec_spki(curve: NamedCurve, raw_xy: &[u8]) -> Result<Vec<u8>, OpError> {
    let oid_curve: &[u8] = match curve {
        NamedCurve::P256 => &[0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07],
        NamedCurve::P384 => &[0x06, 0x05, 0x2b, 0x81, 0x04, 0x00, 0x22],
        NamedCurve::P521 => &[0x06, 0x05, 0x2b, 0x81, 0x04, 0x00, 0x23],
    };
    let oid_ec_public: &[u8] = &[0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01];
    let mut alg_id = Vec::new();
    alg_id.extend_from_slice(oid_ec_public);
    alg_id.extend_from_slice(oid_curve);
    let mut alg_id_seq = vec![0x30];
    alg_id_seq.extend_from_slice(&der_len(alg_id.len()));
    alg_id_seq.extend_from_slice(&alg_id);

    let mut bit_string_payload = vec![0u8]; // unused-bits=0
    bit_string_payload.extend_from_slice(raw_xy);
    let mut bit_string = vec![0x03];
    bit_string.extend_from_slice(&der_len(bit_string_payload.len()));
    bit_string.extend_from_slice(&bit_string_payload);

    let mut body = Vec::new();
    body.extend_from_slice(&alg_id_seq);
    body.extend_from_slice(&bit_string);
    let mut out = vec![0x30];
    out.extend_from_slice(&der_len(body.len()));
    out.extend_from_slice(&body);
    Ok(out)
}

fn der_len(n: usize) -> Vec<u8> {
    if n < 0x80 {
        vec![n as u8]
    } else if n < 0x100 {
        vec![0x81, n as u8]
    } else if n < 0x10000 {
        vec![0x82, (n >> 8) as u8, n as u8]
    } else {
        vec![0x83, (n >> 16) as u8, (n >> 8) as u8, n as u8]
    }
}

// -----------------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------------

fn read_named_curve(
    scope: &mut v8::PinScope,
    alg_obj: v8::Local<v8::Object>,
) -> Result<NamedCurve, OpError> {
    let key = v8::String::new(scope, "namedCurve").unwrap();
    let v = alg_obj.get(scope, key.into()).ok_or_else(|| {
        OpError::type_error("EC: missing 'namedCurve'")
    })?;
    if !v.is_string() {
        return Err(OpError::type_error("EC: 'namedCurve' must be a string"));
    }
    let s = v.to_rust_string_lossy(scope);
    NamedCurve::from_str(&s).ok_or_else(|| {
        OpError::dom(
            "NotSupportedError",
            format!("Unsupported curve '{s}' (expected P-256, P-384, P-521)"),
        )
    })
}

fn read_hash_or_err(
    scope: &mut v8::PinScope,
    alg_obj: v8::Local<v8::Object>,
) -> Result<HashAlgo, OpError> {
    let key = v8::String::new(scope, "hash").unwrap();
    let v = alg_obj.get(scope, key.into()).ok_or_else(|| {
        OpError::type_error("EcdsaParams: missing 'hash'")
    })?;
    let name = if v.is_string() {
        v.to_rust_string_lossy(scope)
    } else if let Ok(o) = v8::Local::<v8::Object>::try_from(v) {
        let inner = v8::String::new(scope, "name").unwrap();
        let inner_v = o.get(scope, inner.into()).ok_or_else(|| {
            OpError::type_error("EcdsaParams.hash.name missing")
        })?;
        inner_v.to_rust_string_lossy(scope)
    } else {
        return Err(OpError::type_error("EcdsaParams.hash invalid"));
    };
    HashAlgo::from_str(&name)
        .ok_or_else(|| OpError::dom("NotSupportedError", format!("Unrecognised hash '{name}'")))
}

fn require_ec_curve(key: &CryptoKeyState) -> Result<NamedCurve, OpError> {
    match &key.algorithm {
        KeyAlgorithm::Ec(e) => Ok(e.named_curve),
        _ => Err(OpError::dom("InvalidAccessError", "Not an EC key")),
    }
}

fn validate_ec_usages(alg: AlgorithmName, usages: &[KeyUsage]) -> Result<(), OpError> {
    let allowed: &[KeyUsage] = match alg {
        AlgorithmName::Ecdsa => &[KeyUsage::Sign, KeyUsage::Verify],
        AlgorithmName::Ecdh => &[KeyUsage::DeriveBits, KeyUsage::DeriveKey],
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

fn split_ec_usages(
    alg: AlgorithmName,
    usages: &[KeyUsage],
) -> (Vec<KeyUsage>, Vec<KeyUsage>) {
    let mut priv_u = Vec::new();
    let mut pub_u = Vec::new();
    for &u in usages {
        match (alg, u) {
            (AlgorithmName::Ecdsa, KeyUsage::Sign) => priv_u.push(u),
            (AlgorithmName::Ecdsa, KeyUsage::Verify) => pub_u.push(u),
            (AlgorithmName::Ecdh, KeyUsage::DeriveBits | KeyUsage::DeriveKey) => priv_u.push(u),
            _ => {}
        }
    }
    (priv_u, pub_u)
}
