//! Crypto APIs for V8 apps — backed by aws-lc-rs.

use appbase_ops::appbase_op;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;

use aws_lc_rs::signature::KeyPair;

use crate::event_loop::{Curve, KeyData, SharedState};

/// `crypto.randomUUID() → string`
///
/// Generates a RFC 4122 v4 UUID.
#[appbase_op]
fn crypto_random_uuid() -> String {
    let mut bytes = [0u8; 16];
    aws_lc_rs::rand::fill(&mut bytes).unwrap();
    bytes[6] = (bytes[6] & 0x0f) | 0x40; // version 4
    bytes[8] = (bytes[8] & 0x3f) | 0x80; // variant 10xx

    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3],
        bytes[4], bytes[5], bytes[6], bytes[7],
        bytes[8], bytes[9], bytes[10], bytes[11],
        bytes[12], bytes[13], bytes[14], bytes[15]
    )
}

/// `__cryptoGetRandomValues(len) → base64 string of random bytes`
#[appbase_op]
fn crypto_get_random_values(len: u32) -> Result<String, crate::ops::OpError> {
    if len > 65536 {
        return Err(crate::ops::OpError::type_error(
            "getRandomValues: quota exceeded (max 65536 bytes)",
        ));
    }
    let mut buf = vec![0u8; len as usize];
    aws_lc_rs::rand::fill(&mut buf)
        .map_err(|e| crate::ops::OpError::error(format!("RNG failed: {e}")))?;
    Ok(B64.encode(&buf))
}

/// `__cryptoDigest(algo, data_b64) → base64 hash`
#[appbase_op]
fn crypto_digest(algo: String, data_b64: String) -> Result<String, crate::ops::OpError> {
    let algorithm = match algo.as_str() {
        "SHA-1" => &aws_lc_rs::digest::SHA1_FOR_LEGACY_USE_ONLY,
        "SHA-256" => &aws_lc_rs::digest::SHA256,
        "SHA-384" => &aws_lc_rs::digest::SHA384,
        "SHA-512" => &aws_lc_rs::digest::SHA512,
        _ => {
            return Err(crate::ops::OpError::type_error(format!(
                "Unsupported digest: {algo}"
            )))
        }
    };
    let data = B64
        .decode(&data_b64)
        .map_err(|e| crate::ops::OpError::type_error(format!("Invalid base64: {e}")))?;
    let digest = aws_lc_rs::digest::digest(algorithm, &data);
    Ok(B64.encode(digest.as_ref()))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn parse_curve(p: &serde_json::Value) -> Result<Curve, crate::ops::OpError> {
    match p["algorithm"]["namedCurve"].as_str().unwrap_or("") {
        "P-256" => Ok(Curve::P256),
        "P-384" => Ok(Curve::P384),
        other => Err(crate::ops::OpError::type_error(format!(
            "Unsupported curve: {other}"
        ))),
    }
}

// ---------------------------------------------------------------------------
// importKey
// ---------------------------------------------------------------------------

/// `__cryptoImportKey(params_json) → JSON {keyId, type}`
#[appbase_op(state)]
fn crypto_import_key(state: SharedState, params: String) -> Result<String, crate::ops::OpError> {
    let p: serde_json::Value = serde_json::from_str(&params)
        .map_err(|e| crate::ops::OpError::type_error(format!("Invalid params: {e}")))?;

    let format = p["format"].as_str().unwrap_or("");
    let key_data_b64 = p["keyData"].as_str().unwrap_or("");
    let algo_name = p["algorithm"]["name"]
        .as_str()
        .unwrap_or("")
        .to_uppercase();

    let key_bytes = B64
        .decode(key_data_b64)
        .map_err(|e| crate::ops::OpError::type_error(format!("Invalid key data: {e}")))?;

    let (key_data, key_type) = match (algo_name.as_str(), format) {
        // Symmetric keys (raw only)
        (
            "HMAC" | "AES-GCM" | "AES-CBC" | "AES-CTR" | "AES-KW" | "HKDF" | "PBKDF2",
            "raw",
        ) => (KeyData::Symmetric { raw: key_bytes }, "secret"),
        // EC keys
        ("ECDSA" | "ECDH", "raw") => {
            let curve = parse_curve(&p)?;
            (KeyData::EcPublic { raw: key_bytes, curve }, "public")
        }
        ("ECDSA" | "ECDH", "pkcs8") => {
            let curve = parse_curve(&p)?;
            (
                KeyData::EcPrivate {
                    pkcs8_der: key_bytes,
                    curve,
                },
                "private",
            )
        }
        ("ECDSA" | "ECDH", "spki") => {
            let curve = parse_curve(&p)?;
            (KeyData::EcPublic { raw: key_bytes, curve }, "public")
        }
        // RSA keys
        ("RSA-OAEP" | "RSASSA-PKCS1-V1_5" | "RSA-PSS", "pkcs8") => (
            KeyData::RsaPrivate {
                pkcs8_der: key_bytes,
            },
            "private",
        ),
        ("RSA-OAEP" | "RSASSA-PKCS1-V1_5" | "RSA-PSS", "spki") => (
            KeyData::RsaPublic {
                spki_der: key_bytes,
            },
            "public",
        ),
        // Ed25519
        ("ED25519", "raw") => (KeyData::Ed25519Public { raw: key_bytes }, "public"),
        ("ED25519", "pkcs8") => (
            KeyData::Ed25519Private {
                pkcs8_der: key_bytes,
            },
            "private",
        ),
        _ => {
            return Err(crate::ops::OpError::type_error(format!(
                "Unsupported import: algorithm={algo_name}, format={format}"
            )))
        }
    };

    let mut s = state.borrow_mut();
    let key_id = s.next_key_id;
    s.next_key_id += 1;
    s.key_store.insert(key_id, key_data);

    Ok(serde_json::json!({ "keyId": key_id, "type": key_type }).to_string())
}

// ---------------------------------------------------------------------------
// exportKey
// ---------------------------------------------------------------------------

/// `__cryptoExportKey(params_json) → JSON {keyData: base64}`
#[appbase_op(state)]
fn crypto_export_key(state: SharedState, params: String) -> Result<String, crate::ops::OpError> {
    let p: serde_json::Value = serde_json::from_str(&params)
        .map_err(|e| crate::ops::OpError::type_error(format!("Invalid params: {e}")))?;

    let format = p["format"].as_str().unwrap_or("");
    let key_id = p["keyId"].as_u64().unwrap_or(0) as u32;

    let s = state.borrow();
    let key = s
        .key_store
        .get(&key_id)
        .ok_or_else(|| crate::ops::OpError::type_error("Key not found"))?;

    let bytes = match (key, format) {
        (KeyData::Symmetric { raw }, "raw") => raw.clone(),
        (KeyData::EcPublic { raw, .. }, "raw") => raw.clone(),
        (KeyData::EcPrivate { pkcs8_der, .. }, "pkcs8") => pkcs8_der.clone(),
        (KeyData::RsaPrivate { pkcs8_der }, "pkcs8") => pkcs8_der.clone(),
        (KeyData::RsaPublic { spki_der }, "spki") => spki_der.clone(),
        (KeyData::Ed25519Private { pkcs8_der }, "pkcs8") => pkcs8_der.clone(),
        (KeyData::Ed25519Public { raw }, "raw") => raw.clone(),
        _ => {
            return Err(crate::ops::OpError::type_error(
                "Unsupported export format for this key type",
            ))
        }
    };

    Ok(serde_json::json!({ "keyData": B64.encode(&bytes) }).to_string())
}

// ---------------------------------------------------------------------------
// generateKey
// ---------------------------------------------------------------------------

/// `__cryptoGenerateKey(params_json) → JSON {keyId} | {publicKeyId, privateKeyId}`
#[appbase_op(state)]
fn crypto_generate_key(
    state: SharedState,
    params: String,
) -> Result<String, crate::ops::OpError> {
    let p: serde_json::Value = serde_json::from_str(&params)
        .map_err(|e| crate::ops::OpError::type_error(format!("Invalid params: {e}")))?;

    let algo_name = p["algorithm"]["name"]
        .as_str()
        .unwrap_or("")
        .to_uppercase();
    let rng = aws_lc_rs::rand::SystemRandom::new();

    match algo_name.as_str() {
        "HMAC" => {
            let hash = p["algorithm"]["hash"]["name"]
                .as_str()
                .unwrap_or("SHA-256")
                .to_uppercase();
            let len = p["algorithm"]["length"].as_u64().unwrap_or_else(|| {
                match hash.as_str() {
                    "SHA-384" => 384,
                    "SHA-512" => 512,
                    _ => 256, // SHA-256 default
                }
            }) / 8;
            let mut raw = vec![0u8; len as usize];
            aws_lc_rs::rand::fill(&mut raw)
                .map_err(|e| crate::ops::OpError::error(format!("{e}")))?;
            let mut s = state.borrow_mut();
            let id = s.next_key_id;
            s.next_key_id += 1;
            s.key_store.insert(id, KeyData::Symmetric { raw });
            Ok(serde_json::json!({ "keyId": id }).to_string())
        }
        "AES-GCM" | "AES-CBC" | "AES-CTR" | "AES-KW" => {
            let len = p["algorithm"]["length"].as_u64().unwrap_or(256) / 8;
            if len != 16 && len != 24 && len != 32 {
                return Err(crate::ops::OpError::type_error(
                    "AES key length must be 128, 192, or 256",
                ));
            }
            let mut raw = vec![0u8; len as usize];
            aws_lc_rs::rand::fill(&mut raw)
                .map_err(|e| crate::ops::OpError::error(format!("{e}")))?;
            let mut s = state.borrow_mut();
            let id = s.next_key_id;
            s.next_key_id += 1;
            s.key_store.insert(id, KeyData::Symmetric { raw });
            Ok(serde_json::json!({ "keyId": id }).to_string())
        }
        "ECDSA" | "ECDH" => {
            let curve = parse_curve(&p)?;
            let alg = match curve {
                Curve::P256 => &aws_lc_rs::signature::ECDSA_P256_SHA256_ASN1_SIGNING,
                Curve::P384 => &aws_lc_rs::signature::ECDSA_P384_SHA384_ASN1_SIGNING,
            };
            let pkcs8 = aws_lc_rs::signature::EcdsaKeyPair::generate_pkcs8(alg, &rng)
                .map_err(|e| {
                    crate::ops::OpError::error(format!("Key generation failed: {e}"))
                })?;
            let key_pair =
                aws_lc_rs::signature::EcdsaKeyPair::from_pkcs8(alg, pkcs8.as_ref())
                    .map_err(|e| {
                        crate::ops::OpError::error(format!("Key parse failed: {e}"))
                    })?;
            let pub_key = key_pair.public_key().as_ref().to_vec();

            let mut s = state.borrow_mut();
            let priv_id = s.next_key_id;
            s.next_key_id += 1;
            let pub_id = s.next_key_id;
            s.next_key_id += 1;
            s.key_store.insert(
                priv_id,
                KeyData::EcPrivate {
                    pkcs8_der: pkcs8.as_ref().to_vec(),
                    curve: curve.clone(),
                },
            );
            s.key_store
                .insert(pub_id, KeyData::EcPublic { raw: pub_key, curve });
            Ok(
                serde_json::json!({ "privateKeyId": priv_id, "publicKeyId": pub_id })
                    .to_string(),
            )
        }
        "ED25519" => {
            let pkcs8 = aws_lc_rs::signature::Ed25519KeyPair::generate_pkcs8(&rng)
                .map_err(|e| {
                    crate::ops::OpError::error(format!("Key generation failed: {e}"))
                })?;
            let key_pair =
                aws_lc_rs::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).map_err(
                    |e| crate::ops::OpError::error(format!("Key parse failed: {e}")),
                )?;
            let pub_key = key_pair.public_key().as_ref().to_vec();

            let mut s = state.borrow_mut();
            let priv_id = s.next_key_id;
            s.next_key_id += 1;
            let pub_id = s.next_key_id;
            s.next_key_id += 1;
            s.key_store.insert(
                priv_id,
                KeyData::Ed25519Private {
                    pkcs8_der: pkcs8.as_ref().to_vec(),
                },
            );
            s.key_store
                .insert(pub_id, KeyData::Ed25519Public { raw: pub_key });
            Ok(
                serde_json::json!({ "privateKeyId": priv_id, "publicKeyId": pub_id })
                    .to_string(),
            )
        }
        _ => Err(crate::ops::OpError::type_error(format!(
            "Unsupported generateKey algorithm: {algo_name}"
        ))),
    }
}

// ---------------------------------------------------------------------------
// sign
// ---------------------------------------------------------------------------

/// `__cryptoSign(params_json) → base64 signature`
#[appbase_op(state)]
fn crypto_sign(state: SharedState, params: String) -> Result<String, crate::ops::OpError> {
    let p: serde_json::Value = serde_json::from_str(&params)
        .map_err(|e| crate::ops::OpError::type_error(format!("Invalid params: {e}")))?;

    let algo_name = p["algorithm"]["name"]
        .as_str()
        .unwrap_or("")
        .to_uppercase();
    let key_id = p["keyId"].as_u64().unwrap_or(0) as u32;
    let data = B64
        .decode(p["data"].as_str().unwrap_or(""))
        .map_err(|e| crate::ops::OpError::type_error(format!("Invalid data: {e}")))?;
    let hash = p["algorithm"]["hash"]["name"]
        .as_str()
        .unwrap_or("SHA-256")
        .to_uppercase();

    let s = state.borrow();
    let key = s
        .key_store
        .get(&key_id)
        .ok_or_else(|| crate::ops::OpError::type_error("Key not found"))?;

    match (algo_name.as_str(), key) {
        ("HMAC", KeyData::Symmetric { raw }) => {
            let alg = match hash.as_str() {
                "SHA-1" => aws_lc_rs::hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY,
                "SHA-256" => aws_lc_rs::hmac::HMAC_SHA256,
                "SHA-384" => aws_lc_rs::hmac::HMAC_SHA384,
                "SHA-512" => aws_lc_rs::hmac::HMAC_SHA512,
                _ => {
                    return Err(crate::ops::OpError::type_error(format!(
                        "Unsupported HMAC hash: {hash}"
                    )))
                }
            };
            let hmac_key = aws_lc_rs::hmac::Key::new(alg, raw);
            let tag = aws_lc_rs::hmac::sign(&hmac_key, &data);
            Ok(B64.encode(tag.as_ref()))
        }
        ("ECDSA", KeyData::EcPrivate { pkcs8_der, curve }) => {
            let alg = match (curve, hash.as_str()) {
                (Curve::P256, "SHA-256") => {
                    &aws_lc_rs::signature::ECDSA_P256_SHA256_ASN1_SIGNING
                }
                (Curve::P384, "SHA-384") => {
                    &aws_lc_rs::signature::ECDSA_P384_SHA384_ASN1_SIGNING
                }
                _ => {
                    return Err(crate::ops::OpError::type_error(
                        "Unsupported ECDSA curve/hash combo",
                    ))
                }
            };
            let rng = aws_lc_rs::rand::SystemRandom::new();
            let key_pair =
                aws_lc_rs::signature::EcdsaKeyPair::from_pkcs8(alg, pkcs8_der)
                    .map_err(|e| crate::ops::OpError::error(format!("Invalid ECDSA key: {e}")))?;
            let sig = key_pair
                .sign(&rng, &data)
                .map_err(|e| crate::ops::OpError::error(format!("ECDSA sign failed: {e}")))?;
            Ok(B64.encode(sig.as_ref()))
        }
        ("ED25519", KeyData::Ed25519Private { pkcs8_der }) => {
            let key_pair =
                aws_lc_rs::signature::Ed25519KeyPair::from_pkcs8(pkcs8_der).map_err(|e| {
                    crate::ops::OpError::error(format!("Invalid Ed25519 key: {e}"))
                })?;
            let sig = key_pair.sign(&data);
            Ok(B64.encode(sig.as_ref()))
        }
        ("RSASSA-PKCS1-V1_5", KeyData::RsaPrivate { pkcs8_der }) => {
            let padding = match hash.as_str() {
                "SHA-256" => &aws_lc_rs::signature::RSA_PKCS1_SHA256,
                "SHA-384" => &aws_lc_rs::signature::RSA_PKCS1_SHA384,
                "SHA-512" => &aws_lc_rs::signature::RSA_PKCS1_SHA512,
                _ => {
                    return Err(crate::ops::OpError::type_error(format!(
                        "Unsupported RSA hash: {hash}"
                    )))
                }
            };
            let key_pair =
                aws_lc_rs::signature::RsaKeyPair::from_pkcs8(pkcs8_der)
                    .map_err(|e| crate::ops::OpError::error(format!("Invalid RSA key: {e}")))?;
            let rng = aws_lc_rs::rand::SystemRandom::new();
            let mut sig = vec![0u8; key_pair.public_modulus_len()];
            key_pair
                .sign(padding, &rng, &data, &mut sig)
                .map_err(|e| crate::ops::OpError::error(format!("RSA sign failed: {e}")))?;
            Ok(B64.encode(&sig))
        }
        ("RSA-PSS", KeyData::RsaPrivate { pkcs8_der }) => {
            let padding = match hash.as_str() {
                "SHA-256" => &aws_lc_rs::signature::RSA_PSS_SHA256,
                "SHA-384" => &aws_lc_rs::signature::RSA_PSS_SHA384,
                "SHA-512" => &aws_lc_rs::signature::RSA_PSS_SHA512,
                _ => {
                    return Err(crate::ops::OpError::type_error(format!(
                        "Unsupported RSA-PSS hash: {hash}"
                    )))
                }
            };
            let key_pair =
                aws_lc_rs::signature::RsaKeyPair::from_pkcs8(pkcs8_der)
                    .map_err(|e| crate::ops::OpError::error(format!("Invalid RSA key: {e}")))?;
            let rng = aws_lc_rs::rand::SystemRandom::new();
            let mut sig = vec![0u8; key_pair.public_modulus_len()];
            key_pair
                .sign(padding, &rng, &data, &mut sig)
                .map_err(|e| {
                    crate::ops::OpError::error(format!("RSA-PSS sign failed: {e}"))
                })?;
            Ok(B64.encode(&sig))
        }
        _ => Err(crate::ops::OpError::type_error(format!(
            "Cannot sign with {algo_name} and this key type"
        ))),
    }
}

// ---------------------------------------------------------------------------
// verify
// ---------------------------------------------------------------------------

/// `__cryptoVerify(params_json) → "true" | "false"`
#[appbase_op(state)]
fn crypto_verify(state: SharedState, params: String) -> Result<String, crate::ops::OpError> {
    let p: serde_json::Value = serde_json::from_str(&params)
        .map_err(|e| crate::ops::OpError::type_error(format!("Invalid params: {e}")))?;

    let algo_name = p["algorithm"]["name"]
        .as_str()
        .unwrap_or("")
        .to_uppercase();
    let key_id = p["keyId"].as_u64().unwrap_or(0) as u32;
    let data = B64
        .decode(p["data"].as_str().unwrap_or(""))
        .map_err(|e| crate::ops::OpError::type_error(format!("Invalid data: {e}")))?;
    let signature = B64
        .decode(p["signature"].as_str().unwrap_or(""))
        .map_err(|e| crate::ops::OpError::type_error(format!("Invalid signature: {e}")))?;
    let hash = p["algorithm"]["hash"]["name"]
        .as_str()
        .unwrap_or("SHA-256")
        .to_uppercase();

    let s = state.borrow();
    let key = s
        .key_store
        .get(&key_id)
        .ok_or_else(|| crate::ops::OpError::type_error("Key not found"))?;

    let valid = match (algo_name.as_str(), key) {
        ("HMAC", KeyData::Symmetric { raw }) => {
            let alg = match hash.as_str() {
                "SHA-1" => aws_lc_rs::hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY,
                "SHA-256" => aws_lc_rs::hmac::HMAC_SHA256,
                "SHA-384" => aws_lc_rs::hmac::HMAC_SHA384,
                "SHA-512" => aws_lc_rs::hmac::HMAC_SHA512,
                _ => {
                    return Err(crate::ops::OpError::type_error(format!(
                        "Unsupported HMAC hash: {hash}"
                    )))
                }
            };
            let hmac_key = aws_lc_rs::hmac::Key::new(alg, raw);
            aws_lc_rs::hmac::verify(&hmac_key, &data, &signature).is_ok()
        }
        ("ECDSA", KeyData::EcPublic { raw, curve }) => {
            let alg = match (curve, hash.as_str()) {
                (Curve::P256, "SHA-256") => &aws_lc_rs::signature::ECDSA_P256_SHA256_ASN1,
                (Curve::P384, "SHA-384") => &aws_lc_rs::signature::ECDSA_P384_SHA384_ASN1,
                _ => {
                    return Err(crate::ops::OpError::type_error(
                        "Unsupported ECDSA curve/hash combo",
                    ))
                }
            };
            let pub_key = aws_lc_rs::signature::UnparsedPublicKey::new(alg, raw);
            pub_key.verify(&data, &signature).is_ok()
        }
        ("ED25519", KeyData::Ed25519Public { raw }) => {
            let pub_key = aws_lc_rs::signature::UnparsedPublicKey::new(
                &aws_lc_rs::signature::ED25519,
                raw,
            );
            pub_key.verify(&data, &signature).is_ok()
        }
        ("RSASSA-PKCS1-V1_5", KeyData::RsaPublic { spki_der }) => {
            let alg = match hash.as_str() {
                "SHA-256" => &aws_lc_rs::signature::RSA_PKCS1_2048_8192_SHA256,
                "SHA-384" => &aws_lc_rs::signature::RSA_PKCS1_2048_8192_SHA384,
                "SHA-512" => &aws_lc_rs::signature::RSA_PKCS1_2048_8192_SHA512,
                _ => {
                    return Err(crate::ops::OpError::type_error(format!(
                        "Unsupported RSA hash: {hash}"
                    )))
                }
            };
            let pub_key = aws_lc_rs::signature::UnparsedPublicKey::new(alg, spki_der);
            pub_key.verify(&data, &signature).is_ok()
        }
        ("RSA-PSS", KeyData::RsaPublic { spki_der }) => {
            let alg = match hash.as_str() {
                "SHA-256" => &aws_lc_rs::signature::RSA_PSS_2048_8192_SHA256,
                "SHA-384" => &aws_lc_rs::signature::RSA_PSS_2048_8192_SHA384,
                "SHA-512" => &aws_lc_rs::signature::RSA_PSS_2048_8192_SHA512,
                _ => {
                    return Err(crate::ops::OpError::type_error(format!(
                        "Unsupported RSA-PSS hash: {hash}"
                    )))
                }
            };
            let pub_key = aws_lc_rs::signature::UnparsedPublicKey::new(alg, spki_der);
            pub_key.verify(&data, &signature).is_ok()
        }
        ("HMAC", _) => {
            return Err(crate::ops::OpError::type_error(
                "HMAC verify requires a symmetric key",
            ))
        }
        _ => {
            return Err(crate::ops::OpError::type_error(format!(
                "Cannot verify with {algo_name} and this key type"
            )))
        }
    };

    Ok(if valid { "true" } else { "false" }.to_string())
}
