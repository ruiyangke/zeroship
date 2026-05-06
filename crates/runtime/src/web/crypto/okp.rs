//! Ed25519 + X25519. Per spec §§25, 26.

#![allow(dead_code)]

use super::crypto_key;
use super::helpers::{read_buffer_source, vec_to_arraybuffer};
use super::key_material::{
    CryptoKeyState, KeyAlgorithm, KeyFormat, KeyMaterial, KeyType, KeyUsage,
};
use crate::state::OpError;

// -----------------------------------------------------------------------------
// Ed25519
// -----------------------------------------------------------------------------

pub fn sign_ed25519(key: &CryptoKeyState, data: &[u8]) -> Result<Vec<u8>, OpError> {
    if key.key_type != KeyType::Private {
        return Err(OpError::dom(
            "InvalidAccessError",
            "Ed25519 sign requires a private key",
        ));
    }
    let pkcs8 = match &key.material {
        KeyMaterial::Ed25519Private { pkcs8_der, .. } => pkcs8_der,
        _ => {
            return Err(OpError::dom(
                "InvalidAccessError",
                "Ed25519: missing private material",
            ));
        }
    };
    let key_pair = aws_lc_rs::signature::Ed25519KeyPair::from_pkcs8(pkcs8)
        .map_err(|_| OpError::dom("DataError", "Ed25519 key load failed"))?;
    let sig = key_pair.sign(data);
    Ok(sig.as_ref().to_vec())
}

pub fn verify_ed25519(
    key: &CryptoKeyState,
    data: &[u8],
    sig: &[u8],
) -> Result<bool, OpError> {
    if key.key_type != KeyType::Public {
        return Err(OpError::dom(
            "InvalidAccessError",
            "Ed25519 verify requires a public key",
        ));
    }
    let raw_x = match &key.material {
        KeyMaterial::Ed25519Public { raw_x, .. } => raw_x.as_slice(),
        KeyMaterial::Ed25519Private { raw_x, .. } => raw_x.as_slice(),
        _ => {
            return Err(OpError::dom(
                "InvalidAccessError",
                "Ed25519: missing public material",
            ));
        }
    };
    let unparsed =
        aws_lc_rs::signature::UnparsedPublicKey::new(&aws_lc_rs::signature::ED25519, raw_x);
    Ok(unparsed.verify(data, sig).is_ok())
}

pub fn generate_ed25519<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    extractable: bool,
    usages: &[KeyUsage],
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    validate_sig_usages(usages)?;
    let rng = aws_lc_rs::rand::SystemRandom::new();
    let pkcs8 = aws_lc_rs::signature::Ed25519KeyPair::generate_pkcs8(&rng)
        .map_err(|_| OpError::dom("OperationError", "Ed25519 keygen failed"))?;
    let pkcs8_bytes = pkcs8.as_ref().to_vec();
    let key_pair = aws_lc_rs::signature::Ed25519KeyPair::from_pkcs8(&pkcs8_bytes)
        .map_err(|_| OpError::dom("OperationError", "Ed25519 keygen reload"))?;
    use aws_lc_rs::signature::KeyPair as _;
    let pub_bytes: [u8; 32] = key_pair
        .public_key()
        .as_ref()
        .try_into()
        .map_err(|_| OpError::dom("OperationError", "Ed25519 public not 32 bytes"))?;
    // aws-lc-rs hides the seed; recover it by walking the PKCS#8 we
    // just got back (RFC 8410 OneAsymmetricKey wrapping a 32-byte
    // CurvePrivateKey OCTET STRING).
    let raw_d: [u8; 32] = super::der::extract_cfrg_raw_seed(&pkcs8_bytes)
        .ok_or_else(|| OpError::dom("OperationError", "Ed25519 PKCS#8 walk failed"))?;

    let (priv_usages, pub_usages) = split_sig_usages(usages);
    let priv_state = CryptoKeyState {
        key_type: KeyType::Private,
        extractable,
        algorithm: KeyAlgorithm::Ed25519,
        usages: priv_usages,
        material: KeyMaterial::Ed25519Private {
            pkcs8_der: pkcs8_bytes,
            raw_d,
            raw_x: pub_bytes,
        },
    };
    let pub_state = CryptoKeyState {
        key_type: KeyType::Public,
        extractable: true,
        algorithm: KeyAlgorithm::Ed25519,
        usages: pub_usages,
        material: KeyMaterial::Ed25519Public {
            spki_der: Vec::new(),
            raw_x: pub_bytes,
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

pub fn import_ed25519<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    format: KeyFormat,
    key_data: v8::Local<v8::Value>,
    extractable: bool,
    usages: &[KeyUsage],
) -> Result<v8::Local<'s, v8::Object>, OpError> {
    validate_sig_usages(usages)?;
    if usages.is_empty() {
        let is_private = match format {
            KeyFormat::Pkcs8 => true,
            KeyFormat::Jwk => jwk_has_private_d(scope, key_data),
            _ => false,
        };
        if is_private {
            return Err(OpError::dom(
                "SyntaxError",
                "Ed25519 private-key import: usages must be non-empty",
            ));
        }
    }
    match format {
        KeyFormat::Raw => {
            let bytes = read_buffer_source(scope, key_data)?;
            if bytes.len() != 32 {
                return Err(OpError::dom(
                    "DataError",
                    "Ed25519 raw public key must be 32 bytes",
                ));
            }
            let mut raw_x = [0u8; 32];
            raw_x.copy_from_slice(&bytes);
            let state = CryptoKeyState {
                key_type: KeyType::Public,
                extractable,
                algorithm: KeyAlgorithm::Ed25519,
                usages: usages.to_vec(),
                material: KeyMaterial::Ed25519Public {
                    spki_der: Vec::new(),
                    raw_x,
                },
            };
            Ok(crypto_key::build(scope, state))
        }
        KeyFormat::Jwk => {
            super::jwk::import_ed25519(scope, key_data, extractable, usages)
        }
        KeyFormat::Spki => {
            let bytes = read_buffer_source(scope, key_data)?;
            let raw_x = parse_cfrg_spki(&bytes, ED25519_OID).ok_or_else(|| {
                OpError::dom("DataError", "Ed25519 SPKI parse failed")
            })?;
            let state = CryptoKeyState {
                key_type: KeyType::Public,
                extractable,
                algorithm: KeyAlgorithm::Ed25519,
                usages: usages.to_vec(),
                material: KeyMaterial::Ed25519Public {
                    spki_der: bytes,
                    raw_x,
                },
            };
            Ok(crypto_key::build(scope, state))
        }
        KeyFormat::Pkcs8 => {
            let bytes = read_buffer_source(scope, key_data)?;
            // Validate via aws-lc-rs (also rejects wrong-curve keys).
            let kp = aws_lc_rs::signature::Ed25519KeyPair::from_pkcs8(&bytes)
                .map_err(|_| OpError::dom("DataError", "Ed25519 PKCS#8 parse failed"))?;
            use aws_lc_rs::signature::KeyPair as _;
            let mut raw_x = [0u8; 32];
            raw_x.copy_from_slice(kp.public_key().as_ref());
            let raw_d = super::der::extract_cfrg_raw_seed(&bytes).ok_or_else(|| {
                OpError::dom("DataError", "Ed25519 PKCS#8 seed extract failed")
            })?;
            let state = CryptoKeyState {
                key_type: KeyType::Private,
                extractable,
                algorithm: KeyAlgorithm::Ed25519,
                usages: usages.to_vec(),
                material: KeyMaterial::Ed25519Private {
                    pkcs8_der: bytes,
                    raw_d,
                    raw_x,
                },
            };
            Ok(crypto_key::build(scope, state))
        }
    }
}

pub fn export_ed25519<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    format: KeyFormat,
    key: &CryptoKeyState,
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    match format {
        KeyFormat::Raw => {
            if key.key_type == KeyType::Private {
                return Err(OpError::dom(
                    "NotSupportedError",
                    "Ed25519 private keys cannot export 'raw'",
                ));
            }
            let raw_x = match &key.material {
                KeyMaterial::Ed25519Public { raw_x, .. } => raw_x,
                KeyMaterial::Ed25519Private { raw_x, .. } => raw_x,
                _ => return Err(OpError::dom("OperationError", "Not Ed25519")),
            };
            Ok(vec_to_arraybuffer(scope, raw_x))
        }
        KeyFormat::Jwk => super::jwk::export_ed25519(scope, key),
        KeyFormat::Spki => {
            let raw_x = match &key.material {
                KeyMaterial::Ed25519Public { spki_der, raw_x } => {
                    if !spki_der.is_empty() {
                        return Ok(vec_to_arraybuffer(scope, spki_der));
                    }
                    raw_x
                }
                _ => {
                    return Err(OpError::dom(
                        "InvalidAccessError",
                        "Ed25519 SPKI export requires a public key",
                    ));
                }
            };
            let der = build_cfrg_spki(ED25519_OID, raw_x);
            Ok(vec_to_arraybuffer(scope, &der))
        }
        KeyFormat::Pkcs8 => match &key.material {
            KeyMaterial::Ed25519Private { pkcs8_der, .. } => {
                Ok(vec_to_arraybuffer(scope, pkcs8_der))
            }
            _ => Err(OpError::dom(
                "InvalidAccessError",
                "Ed25519 PKCS#8 export requires a private key",
            )),
        },
    }
}

const ED25519_OID: &[u8] = &[0x2b, 0x65, 0x70]; // 1.3.101.112
const X25519_OID: &[u8] = &[0x2b, 0x65, 0x6e]; // 1.3.101.110

/// Parse a CFRG SubjectPublicKeyInfo (RFC 8410 §4) and return the raw
/// 32-byte public key. `expected_oid` is the curve OID octets (Ed25519
/// or X25519).
fn parse_cfrg_spki(spki: &[u8], expected_oid: &[u8]) -> Option<[u8; 32]> {
    use super::der::read_tlv_pub;
    let (top, rest) = read_tlv_pub(spki)?;
    if !rest.is_empty() || top.tag != 0x30 {
        return None;
    }
    let body = top.value;
    let (alg_id, body) = read_tlv_pub(body)?;
    if alg_id.tag != 0x30 {
        return None;
    }
    let (oid, _) = read_tlv_pub(alg_id.value)?;
    if oid.tag != 0x06 || oid.value != expected_oid {
        return None;
    }
    let (bit_string, _) = read_tlv_pub(body)?;
    if bit_string.tag != 0x03 || bit_string.value.len() != 33 || bit_string.value[0] != 0 {
        return None;
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&bit_string.value[1..]);
    Some(out)
}

fn build_cfrg_spki(curve_oid: &[u8], raw_x: &[u8; 32]) -> Vec<u8> {
    // SubjectPublicKeyInfo:
    //   SEQUENCE {
    //     SEQUENCE { OID curve_oid }
    //     BIT STRING (33 bytes: 0x00 unused-bits + 32 raw_x)
    //   }
    let mut alg_id = vec![0x06];
    alg_id.push(curve_oid.len() as u8);
    alg_id.extend_from_slice(curve_oid);
    let mut alg_id_seq = vec![0x30, alg_id.len() as u8];
    alg_id_seq.extend_from_slice(&alg_id);
    let mut bit_string = vec![0x03, 33u8, 0u8];
    bit_string.extend_from_slice(raw_x);
    let mut body = Vec::new();
    body.extend_from_slice(&alg_id_seq);
    body.extend_from_slice(&bit_string);
    let mut out = vec![0x30, body.len() as u8];
    out.extend_from_slice(&body);
    out
}

// -----------------------------------------------------------------------------
// X25519
// -----------------------------------------------------------------------------

pub fn x25519_derive_bits<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    alg_obj: v8::Local<v8::Object>,
    key: &CryptoKeyState,
    length_bits: Option<u32>,
) -> Result<Vec<u8>, OpError> {
    if key.key_type != KeyType::Private {
        return Err(OpError::dom(
            "InvalidAccessError",
            "X25519 deriveBits requires a private key",
        ));
    }
    if !matches!(key.algorithm, KeyAlgorithm::X25519) {
        return Err(OpError::dom(
            "InvalidAccessError",
            "X25519 deriveBits: key is not X25519",
        ));
    }
    let public_v = alg_obj
        .get(scope, v8::String::new(scope, "public").unwrap().into())
        .ok_or_else(|| OpError::type_error("X25519: missing 'public'"))?;
    let pub_state = crypto_key::require(scope, public_v)?;
    if !matches!(pub_state.algorithm, KeyAlgorithm::X25519) {
        return Err(OpError::dom(
            "InvalidAccessError",
            "X25519 'public' key must be X25519",
        ));
    }
    // The "public" key parameter must actually be a public key per
    // spec §35.6.1 step 4.
    if pub_state.key_type != KeyType::Public {
        return Err(OpError::dom(
            "InvalidAccessError",
            "X25519 'public' key parameter must be a public key",
        ));
    }
    let priv_d = match &key.material {
        KeyMaterial::X25519Private { raw_d, .. } => raw_d,
        _ => {
            return Err(OpError::dom(
                "InvalidAccessError",
                "X25519: missing private material",
            ));
        }
    };
    let pub_x = match &pub_state.material {
        KeyMaterial::X25519Public { raw_x, .. } => raw_x,
        _ => {
            return Err(OpError::dom(
                "InvalidAccessError",
                "X25519: missing public material",
            ));
        }
    };

    // aws-lc-rs's `from_private_key_der` rejects X25519 outright;
    // `from_private_key` accepts the raw 32-byte seed directly.
    let priv_key = aws_lc_rs::agreement::PrivateKey::from_private_key(
        &aws_lc_rs::agreement::X25519,
        priv_d,
    )
    .map_err(|_| OpError::dom("OperationError", "X25519 private key load"))?;
    let peer = aws_lc_rs::agreement::UnparsedPublicKey::new(
        &aws_lc_rs::agreement::X25519,
        pub_x.as_slice(),
    );
    let dom_err = OpError::dom("OperationError", "X25519 agreement");
    let shared = aws_lc_rs::agreement::agree(&priv_key, &peer, dom_err, |z: &[u8]| {
        Ok::<Vec<u8>, OpError>(z.to_vec())
    })?;
    truncate_bits(&shared, length_bits)
}

fn truncate_bits(data: &[u8], length_bits: Option<u32>) -> Result<Vec<u8>, OpError> {
    super::ec::truncate_to_bits_pub(data, length_bits)
}

fn build_x25519_pkcs8_from_seed(seed: &[u8; 32]) -> Vec<u8> {
    // Minimal X25519 PKCS#8 (RFC 8410). The structure is:
    //   PrivateKeyInfo ::= SEQUENCE {
    //     version INTEGER (0),
    //     privateKeyAlgorithm AlgorithmIdentifier {{ X25519 }},
    //     privateKey OCTET STRING (encoding of CurvePrivateKey)
    //   }
    //   CurvePrivateKey ::= OCTET STRING
    //
    // The full DER for a 32-byte X25519 seed is 48 bytes total.
    // Hand-rolled — header bytes are constant.
    let mut out = Vec::with_capacity(48);
    out.extend_from_slice(&[0x30, 0x2e]); // SEQUENCE, length 0x2e
    out.extend_from_slice(&[0x02, 0x01, 0x00]); // INTEGER 0
    out.extend_from_slice(&[
        0x30, 0x05, // SEQUENCE
        0x06, 0x03, 0x2b, 0x65, 0x6e, // OID 1.3.101.110 (X25519)
    ]);
    out.extend_from_slice(&[0x04, 0x22]); // OCTET STRING, length 0x22
    out.extend_from_slice(&[0x04, 0x20]); // inner OCTET STRING, length 0x20
    out.extend_from_slice(seed);
    out
}

pub fn generate_x25519<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    extractable: bool,
    usages: &[KeyUsage],
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    validate_kdf_usages(usages)?;
    // aws-lc-rs's `from_private_key_der` rejects X25519 outright (the
    // PKCS#8 path is gated to ECDH-* curves). We instead use
    // `from_private_key` which takes a raw 32-byte seed.
    let mut seed = [0u8; 32];
    super::helpers::fill_random(&mut seed);
    let pkcs8 = build_x25519_pkcs8_from_seed(&seed);
    let priv_key = aws_lc_rs::agreement::PrivateKey::from_private_key(
        &aws_lc_rs::agreement::X25519,
        &seed,
    )
    .map_err(|_| OpError::dom("OperationError", "X25519 key load"))?;
    let public = priv_key
        .compute_public_key()
        .map_err(|_| OpError::dom("OperationError", "X25519 pub derive"))?;
    let mut pub_arr = [0u8; 32];
    let pb = public.as_ref();
    if pb.len() != 32 {
        return Err(OpError::dom(
            "OperationError",
            "X25519 public key not 32 bytes",
        ));
    }
    pub_arr.copy_from_slice(pb);

    let (priv_usages, pub_usages) = split_kdf_usages(usages);
    let priv_state = CryptoKeyState {
        key_type: KeyType::Private,
        extractable,
        algorithm: KeyAlgorithm::X25519,
        usages: priv_usages,
        material: KeyMaterial::X25519Private {
            pkcs8_der: pkcs8,
            raw_d: seed,
            raw_x: pub_arr,
        },
    };
    let pub_state = CryptoKeyState {
        key_type: KeyType::Public,
        extractable: true,
        algorithm: KeyAlgorithm::X25519,
        usages: pub_usages,
        material: KeyMaterial::X25519Public {
            spki_der: Vec::new(),
            raw_x: pub_arr,
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

pub fn import_x25519<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    format: KeyFormat,
    key_data: v8::Local<v8::Value>,
    extractable: bool,
    usages: &[KeyUsage],
) -> Result<v8::Local<'s, v8::Object>, OpError> {
    validate_kdf_usages(usages)?;
    if usages.is_empty() {
        let is_private = match format {
            KeyFormat::Pkcs8 => true,
            KeyFormat::Jwk => jwk_has_private_d(scope, key_data),
            _ => false,
        };
        if is_private {
            return Err(OpError::dom(
                "SyntaxError",
                "X25519 private-key import: usages must be non-empty",
            ));
        }
    }
    match format {
        KeyFormat::Raw => {
            let bytes = read_buffer_source(scope, key_data)?;
            if bytes.len() != 32 {
                return Err(OpError::dom(
                    "DataError",
                    "X25519 raw public key must be 32 bytes",
                ));
            }
            let mut raw_x = [0u8; 32];
            raw_x.copy_from_slice(&bytes);
            let state = CryptoKeyState {
                key_type: KeyType::Public,
                extractable,
                algorithm: KeyAlgorithm::X25519,
                usages: usages.to_vec(),
                material: KeyMaterial::X25519Public {
                    spki_der: Vec::new(),
                    raw_x,
                },
            };
            Ok(crypto_key::build(scope, state))
        }
        KeyFormat::Jwk => super::jwk::import_x25519(scope, key_data, extractable, usages),
        KeyFormat::Spki => {
            let bytes = read_buffer_source(scope, key_data)?;
            let raw_x = parse_cfrg_spki(&bytes, X25519_OID).ok_or_else(|| {
                OpError::dom("DataError", "X25519 SPKI parse failed")
            })?;
            let state = CryptoKeyState {
                key_type: KeyType::Public,
                extractable,
                algorithm: KeyAlgorithm::X25519,
                usages: usages.to_vec(),
                material: KeyMaterial::X25519Public {
                    spki_der: bytes,
                    raw_x,
                },
            };
            Ok(crypto_key::build(scope, state))
        }
        KeyFormat::Pkcs8 => {
            let bytes = read_buffer_source(scope, key_data)?;
            let raw_d = super::der::extract_cfrg_raw_seed(&bytes).ok_or_else(|| {
                OpError::dom("DataError", "X25519 PKCS#8 seed extract failed")
            })?;
            // Derive the public from the raw seed via aws-lc-rs.
            let priv_key = aws_lc_rs::agreement::PrivateKey::from_private_key(
                &aws_lc_rs::agreement::X25519,
                &raw_d,
            )
            .map_err(|_| OpError::dom("DataError", "X25519 reload via aws-lc-rs failed"))?;
            let public = priv_key
                .compute_public_key()
                .map_err(|_| OpError::dom("DataError", "X25519 public derive failed"))?;
            let pb = public.as_ref();
            if pb.len() != 32 {
                return Err(OpError::dom("DataError", "X25519 public not 32 bytes"));
            }
            let mut raw_x = [0u8; 32];
            raw_x.copy_from_slice(pb);
            let state = CryptoKeyState {
                key_type: KeyType::Private,
                extractable,
                algorithm: KeyAlgorithm::X25519,
                usages: usages.to_vec(),
                material: KeyMaterial::X25519Private {
                    pkcs8_der: bytes,
                    raw_d,
                    raw_x,
                },
            };
            Ok(crypto_key::build(scope, state))
        }
    }
}

pub fn export_x25519<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    format: KeyFormat,
    key: &CryptoKeyState,
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    match format {
        KeyFormat::Raw => {
            if key.key_type == KeyType::Private {
                return Err(OpError::dom(
                    "NotSupportedError",
                    "X25519 private keys cannot export 'raw'",
                ));
            }
            let raw_x = match &key.material {
                KeyMaterial::X25519Public { raw_x, .. } => raw_x,
                KeyMaterial::X25519Private { raw_x, .. } => raw_x,
                _ => return Err(OpError::dom("OperationError", "Not X25519")),
            };
            Ok(vec_to_arraybuffer(scope, raw_x))
        }
        KeyFormat::Jwk => super::jwk::export_x25519(scope, key),
        KeyFormat::Spki => {
            let raw_x = match &key.material {
                KeyMaterial::X25519Public { spki_der, raw_x } => {
                    if !spki_der.is_empty() {
                        return Ok(vec_to_arraybuffer(scope, spki_der));
                    }
                    raw_x
                }
                _ => {
                    return Err(OpError::dom(
                        "InvalidAccessError",
                        "X25519 SPKI export requires a public key",
                    ));
                }
            };
            let der = build_cfrg_spki(X25519_OID, raw_x);
            Ok(vec_to_arraybuffer(scope, &der))
        }
        KeyFormat::Pkcs8 => match &key.material {
            KeyMaterial::X25519Private { pkcs8_der, .. } => {
                Ok(vec_to_arraybuffer(scope, pkcs8_der))
            }
            _ => Err(OpError::dom(
                "InvalidAccessError",
                "X25519 PKCS#8 export requires a private key",
            )),
        },
    }
}

fn jwk_has_private_d<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    key_data: v8::Local<v8::Value>,
) -> bool {
    let obj: v8::Local<v8::Object> = match key_data.try_into() {
        Ok(o) => o,
        Err(_) => return false,
    };
    let key = v8::String::new(scope, "d").unwrap();
    match obj.get(scope, key.into()) {
        Some(v) if v.is_string() => !v.to_rust_string_lossy(scope).is_empty(),
        _ => false,
    }
}

fn validate_sig_usages(usages: &[KeyUsage]) -> Result<(), OpError> {
    for u in usages {
        if !matches!(u, KeyUsage::Sign | KeyUsage::Verify) {
            return Err(OpError::dom(
                "SyntaxError",
                format!("Usage '{}' not allowed for Ed25519", u.as_str()),
            ));
        }
    }
    Ok(())
}

fn split_sig_usages(usages: &[KeyUsage]) -> (Vec<KeyUsage>, Vec<KeyUsage>) {
    let mut p = Vec::new();
    let mut q = Vec::new();
    for &u in usages {
        match u {
            KeyUsage::Sign => p.push(u),
            KeyUsage::Verify => q.push(u),
            _ => {}
        }
    }
    (p, q)
}

fn validate_kdf_usages(usages: &[KeyUsage]) -> Result<(), OpError> {
    for u in usages {
        if !matches!(u, KeyUsage::DeriveBits | KeyUsage::DeriveKey) {
            return Err(OpError::dom(
                "SyntaxError",
                format!("Usage '{}' not allowed for X25519", u.as_str()),
            ));
        }
    }
    Ok(())
}

fn split_kdf_usages(usages: &[KeyUsage]) -> (Vec<KeyUsage>, Vec<KeyUsage>) {
    let mut p = Vec::new();
    for &u in usages {
        match u {
            KeyUsage::DeriveBits | KeyUsage::DeriveKey => p.push(u),
            _ => {}
        }
    }
    (p, Vec::new())
}
