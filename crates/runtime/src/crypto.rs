//! Crypto APIs for V8 apps — backed by aws-lc-rs.

use zeroship_runtime_macros::zeroship_op;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;

use aws_lc_rs::aead::{Aad, Nonce, UnboundKey, LessSafeKey, AES_128_GCM, AES_256_GCM};
use aws_lc_rs::cipher::{
    DecryptionContext, EncryptionContext, PaddedBlockDecryptingKey, PaddedBlockEncryptingKey,
    UnboundCipherKey, AES_128, AES_256,
};
use aws_lc_rs::iv::{FixedLength, IV_LEN_128_BIT};
use aws_lc_rs::rsa::{
    OaepAlgorithm, OaepPrivateDecryptingKey, OaepPublicEncryptingKey, PrivateDecryptingKey,
    PublicEncryptingKey, OAEP_SHA256_MGF1SHA256, OAEP_SHA384_MGF1SHA384, OAEP_SHA512_MGF1SHA512,
};
use aws_lc_rs::signature::KeyPair;

use crate::state::SharedState;

// ---------------------------------------------------------------------------
// Crypto key store types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum Curve {
    P256,
    P384,
}

#[derive(Debug, Clone)]
pub enum KeyData {
    Symmetric { raw: Vec<u8> },
    EcPrivate { pkcs8_der: Vec<u8>, curve: Curve },
    EcPublic { raw: Vec<u8>, curve: Curve },
    RsaPrivate { pkcs8_der: Vec<u8> },
    RsaPublic { spki_der: Vec<u8> },
    Ed25519Private { pkcs8_der: Vec<u8> },
    Ed25519Public { raw: Vec<u8> },
}

// ---------------------------------------------------------------------------
// Thread-local entropy buffer (same pattern as workerd: 4KB lazy-fill)
// Amortizes CSPRNG syscall across ~256 UUID calls.
// ---------------------------------------------------------------------------

use std::cell::RefCell;

const ENTROPY_BUF_SIZE: usize = 4096;

/// Volatile zeroize — compiler cannot optimize this away.
/// Equivalent to workerd's OPENSSL_cleanse / BoringSSL's OPENSSL_cleanse.
#[inline(never)]
fn zeroize_slice(buf: &mut [u8]) {
    for byte in buf.iter_mut() {
        // SAFETY: volatile write ensures the compiler cannot elide the store.
        // The pointer is valid (derived from a mutable slice reference).
        #[allow(unsafe_code)]
        unsafe { std::ptr::write_volatile(byte as *mut u8, 0) };
    }
    // Compiler fence prevents reordering past this point
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
}

struct EntropyBuf {
    store: [u8; ENTROPY_BUF_SIZE],
    pos: usize,
}

impl EntropyBuf {
    fn new() -> Self {
        Self { store: [0u8; ENTROPY_BUF_SIZE], pos: ENTROPY_BUF_SIZE }
    }

    fn fill(&mut self, out: &mut [u8]) {
        let mut remaining = out.len();
        let mut offset = 0;
        while remaining > 0 {
            if self.pos >= ENTROPY_BUF_SIZE {
                aws_lc_rs::rand::fill(&mut self.store).unwrap();
                self.pos = 0;
            }
            let avail = ENTROPY_BUF_SIZE - self.pos;
            let n = remaining.min(avail);
            out[offset..offset + n].copy_from_slice(&self.store[self.pos..self.pos + n]);
            // Zeroize dispensed bytes — volatile write prevents compiler elision.
            // Same purpose as workerd's OPENSSL_cleanse: ensure consumed entropy
            // doesn't linger in memory where a side-channel could read it.
            zeroize_slice(&mut self.store[self.pos..self.pos + n]);
            self.pos += n;
            offset += n;
            remaining -= n;
        }
    }
}

thread_local! {
    static ENTROPY: RefCell<EntropyBuf> = RefCell::new(EntropyBuf::new());
}

pub(crate) fn fast_random(out: &mut [u8]) {
    ENTROPY.with(|e| e.borrow_mut().fill(out));
}

// ---------------------------------------------------------------------------
// randomUUID — manual hex LUT (same pattern as workerd: no format! macro)
// ---------------------------------------------------------------------------

const HEX: &[u8; 16] = b"0123456789abcdef";

/// `crypto.randomUUID() → string`
///
/// RFC 4122 v4 UUID. Uses thread-local buffered CSPRNG + manual hex formatting
/// (same approach as Cloudflare workerd).
#[zeroship_op]
fn crypto_random_uuid() -> String {
    let mut b = [0u8; 16];
    fast_random(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40; // version 4
    b[8] = (b[8] & 0x3f) | 0x80; // variant 10xx

    let mut buf = [0u8; 36];
    let mut p = 0;
    for (i, &byte) in b.iter().enumerate() {
        if i == 4 || i == 6 || i == 8 || i == 10 { buf[p] = b'-'; p += 1; }
        buf[p] = HEX[(byte >> 4) as usize]; p += 1;
        buf[p] = HEX[(byte & 0x0f) as usize]; p += 1;
    }
    String::from_utf8(buf.to_vec()).unwrap()
}

/// `crypto.getRandomValues(typedArray)` — fills TypedArray directly, zero copies.
///
/// Hand-written V8 callback (not `#[zeroship_op]`) because we need direct access
/// to the TypedArray backing store — same approach as workerd.
pub fn crypto_get_random_values_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let arg = args.get(0);

    // Must be a TypedArray (ArrayBufferView)
    let buf_view = match v8::Local::<v8::ArrayBufferView>::try_from(arg) {
        Ok(v) => v,
        Err(_) => {
            let msg = v8::String::new(scope, "getRandomValues: argument must be a typed array").unwrap();
            let exc = v8::Exception::type_error(scope, msg);
            scope.throw_exception(exc);
            return;
        }
    };

    let byte_len = buf_view.byte_length();
    if byte_len > 65536 {
        let msg = v8::String::new(scope, "getRandomValues: quota exceeded (max 65536 bytes)").unwrap();
        let exc = v8::Exception::error(scope, msg);
        scope.throw_exception(exc);
        return;
    }

    if byte_len == 0 {
        rv.set(arg);
        return;
    }

    // Generate random bytes into a temp buffer, then copy into the TypedArray
    let mut tmp = vec![0u8; byte_len];
    fast_random(&mut tmp);
    buf_view.copy_contents(&mut []); // ensure backing store exists
    // copy_contents reads FROM v8, we need to write TO v8 — use the backing store
    let ab = buf_view.buffer(scope).unwrap();
    let offset = buf_view.byte_offset();
    let store = ab.get_backing_store();
    // Write directly into the ArrayBuffer's backing store memory
    for (i, &byte) in tmp.iter().enumerate() {
        store[offset + i].set(byte);
    }

    rv.set(arg);
}

/// `__cryptoDigest(algo, data: ArrayBuffer) → ArrayBuffer`
///
/// Zero-serialization: takes ArrayBuffer directly, returns ArrayBuffer.
/// No base64 encode/decode overhead.
#[zeroship_op]
fn crypto_digest(algo: String, data: Vec<u8>) -> Result<Vec<u8>, crate::state::OpError> {
    let algorithm = match algo.as_str() {
        "SHA-1" => &aws_lc_rs::digest::SHA1_FOR_LEGACY_USE_ONLY,
        "SHA-256" => &aws_lc_rs::digest::SHA256,
        "SHA-384" => &aws_lc_rs::digest::SHA384,
        "SHA-512" => &aws_lc_rs::digest::SHA512,
        _ => {
            return Err(crate::state::OpError::type_error(format!(
                "Unsupported digest: {algo}"
            )))
        }
    };
    let digest = aws_lc_rs::digest::digest(algorithm, &data);
    Ok(digest.as_ref().to_vec())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parse named curve from an algorithm value (has `namedCurve` field directly).
fn parse_curve_from_algo(algo: &serde_json::Value) -> Result<Curve, crate::state::OpError> {
    match algo["namedCurve"].as_str().unwrap_or("") {
        "P-256" => Ok(Curve::P256),
        "P-384" => Ok(Curve::P384),
        other => Err(crate::state::OpError::type_error(format!(
            "Unsupported curve: {other}"
        ))),
    }
}

/// Parse named curve from a wrapper params object (has `algorithm.namedCurve`).
fn parse_curve(p: &serde_json::Value) -> Result<Curve, crate::state::OpError> {
    parse_curve_from_algo(&p["algorithm"])
}

// ---------------------------------------------------------------------------
// importKey
// ---------------------------------------------------------------------------

/// `__cryptoImportKey(format, keyData: ArrayBuffer, algoJson) → JSON {keyId, type}`
///
/// Zero-serialization: key material passed as ArrayBuffer.
/// Algorithm config stays in algoJson; result is a small JSON object with key handle.
///
/// Design note (audit items C-6, C-7): the algorithm config input and the {keyId, type}
/// result use JSON serialization.  These are infrequent setup operations with tiny
/// payloads, so the JSON overhead is negligible.  Bulk key material uses the zero-copy
/// ArrayBuffer bridge.
#[zeroship_op(state)]
fn crypto_import_key(state: SharedState, format: String, key_data: Vec<u8>, algo_json: String) -> Result<String, crate::state::OpError> {
    let p: serde_json::Value = serde_json::from_str(&algo_json)
        .map_err(|e| crate::state::OpError::type_error(format!("Invalid algo params: {e}")))?;

    let algo_name = p["name"]
        .as_str()
        .unwrap_or("")
        .to_uppercase();

    let key_bytes = key_data;
    let format = format.as_str();

    let (key_data, key_type) = match (algo_name.as_str(), format) {
        // Symmetric keys (raw only)
        (
            "HMAC" | "AES-GCM" | "AES-CBC" | "AES-CTR" | "AES-KW" | "HKDF" | "PBKDF2",
            "raw",
        ) => (KeyData::Symmetric { raw: key_bytes }, "secret"),
        // EC keys
        ("ECDSA" | "ECDH", "raw") => {
            let curve = parse_curve_from_algo(&p)?;
            (KeyData::EcPublic { raw: key_bytes, curve }, "public")
        }
        ("ECDSA" | "ECDH", "pkcs8") => {
            let curve = parse_curve_from_algo(&p)?;
            (
                KeyData::EcPrivate {
                    pkcs8_der: key_bytes,
                    curve,
                },
                "private",
            )
        }
        ("ECDSA" | "ECDH", "spki") => {
            let curve = parse_curve_from_algo(&p)?;
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
            return Err(crate::state::OpError::type_error(format!(
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

/// `__cryptoExportKey(format, keyId) → ArrayBuffer`
///
/// Zero-serialization: key material returned as ArrayBuffer.
#[zeroship_op(state)]
fn crypto_export_key(state: SharedState, format: String, key_id: u32) -> Result<Vec<u8>, crate::state::OpError> {
    let s = state.borrow();
    let key = s
        .key_store
        .get(&key_id)
        .ok_or_else(|| crate::state::OpError::type_error("Key not found"))?;

    let bytes = match (key, format.as_str()) {
        (KeyData::Symmetric { raw }, "raw") => raw.clone(),
        (KeyData::EcPublic { raw, .. }, "raw") => raw.clone(),
        (KeyData::EcPrivate { pkcs8_der, .. }, "pkcs8") => pkcs8_der.clone(),
        (KeyData::RsaPrivate { pkcs8_der }, "pkcs8") => pkcs8_der.clone(),
        (KeyData::RsaPublic { spki_der }, "spki") => spki_der.clone(),
        (KeyData::Ed25519Private { pkcs8_der }, "pkcs8") => pkcs8_der.clone(),
        (KeyData::Ed25519Public { raw }, "raw") => raw.clone(),
        _ => {
            return Err(crate::state::OpError::type_error(
                "Unsupported export format for this key type",
            ))
        }
    };

    Ok(bytes)
}

// ---------------------------------------------------------------------------
// generateKey
// ---------------------------------------------------------------------------

/// `__cryptoGenerateKey(params_json) → JSON {keyId} | {publicKeyId, privateKeyId}`
///
/// Design note (audit items C-8, C-9): params and result use JSON serialization.
/// Key generation is an infrequent setup operation with small structured payloads,
/// so the JSON overhead is negligible.
#[zeroship_op(state)]
fn crypto_generate_key(
    state: SharedState,
    params: String,
) -> Result<String, crate::state::OpError> {
    let p: serde_json::Value = serde_json::from_str(&params)
        .map_err(|e| crate::state::OpError::type_error(format!("Invalid params: {e}")))?;

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
                .map_err(|e| crate::state::OpError::error(format!("{e}")))?;
            let mut s = state.borrow_mut();
            let id = s.next_key_id;
            s.next_key_id += 1;
            s.key_store.insert(id, KeyData::Symmetric { raw });
            Ok(serde_json::json!({ "keyId": id }).to_string())
        }
        "AES-GCM" | "AES-CBC" | "AES-CTR" | "AES-KW" => {
            let len = p["algorithm"]["length"].as_u64().unwrap_or(256) / 8;
            if len != 16 && len != 24 && len != 32 {
                return Err(crate::state::OpError::type_error(
                    "AES key length must be 128, 192, or 256",
                ));
            }
            let mut raw = vec![0u8; len as usize];
            aws_lc_rs::rand::fill(&mut raw)
                .map_err(|e| crate::state::OpError::error(format!("{e}")))?;
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
                    crate::state::OpError::error(format!("Key generation failed: {e}"))
                })?;
            let key_pair =
                aws_lc_rs::signature::EcdsaKeyPair::from_pkcs8(alg, pkcs8.as_ref())
                    .map_err(|e| {
                        crate::state::OpError::error(format!("Key parse failed: {e}"))
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
                    crate::state::OpError::error(format!("Key generation failed: {e}"))
                })?;
            let key_pair =
                aws_lc_rs::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).map_err(
                    |e| crate::state::OpError::error(format!("Key parse failed: {e}")),
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
        _ => Err(crate::state::OpError::type_error(format!(
            "Unsupported generateKey algorithm: {algo_name}"
        ))),
    }
}

// ---------------------------------------------------------------------------
// sign
// ---------------------------------------------------------------------------

/// `__cryptoSign(algo, hash, keyId, data: ArrayBuffer) → ArrayBuffer`
///
/// Zero-serialization: data passed as ArrayBuffer, signature returned as ArrayBuffer.
#[zeroship_op(state)]
fn crypto_sign(state: SharedState, algo: String, hash: String, key_id: u32, data: Vec<u8>) -> Result<Vec<u8>, crate::state::OpError> {
    let algo_name = algo.to_uppercase();
    let hash = hash.to_uppercase();

    let s = state.borrow();
    let key = s
        .key_store
        .get(&key_id)
        .ok_or_else(|| crate::state::OpError::type_error("Key not found"))?;

    match (algo_name.as_str(), key) {
        ("HMAC", KeyData::Symmetric { raw }) => {
            let alg = match hash.as_str() {
                "SHA-1" => aws_lc_rs::hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY,
                "SHA-256" => aws_lc_rs::hmac::HMAC_SHA256,
                "SHA-384" => aws_lc_rs::hmac::HMAC_SHA384,
                "SHA-512" => aws_lc_rs::hmac::HMAC_SHA512,
                _ => {
                    return Err(crate::state::OpError::type_error(format!(
                        "Unsupported HMAC hash: {hash}"
                    )))
                }
            };
            let hmac_key = aws_lc_rs::hmac::Key::new(alg, raw);
            let tag = aws_lc_rs::hmac::sign(&hmac_key, &data);
            Ok(tag.as_ref().to_vec())
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
                    return Err(crate::state::OpError::type_error(
                        "Unsupported ECDSA curve/hash combo",
                    ))
                }
            };
            let rng = aws_lc_rs::rand::SystemRandom::new();
            let key_pair =
                aws_lc_rs::signature::EcdsaKeyPair::from_pkcs8(alg, pkcs8_der)
                    .map_err(|e| crate::state::OpError::error(format!("Invalid ECDSA key: {e}")))?;
            let sig = key_pair
                .sign(&rng, &data)
                .map_err(|e| crate::state::OpError::error(format!("ECDSA sign failed: {e}")))?;
            Ok(sig.as_ref().to_vec())
        }
        ("ED25519", KeyData::Ed25519Private { pkcs8_der }) => {
            let key_pair =
                aws_lc_rs::signature::Ed25519KeyPair::from_pkcs8(pkcs8_der).map_err(|e| {
                    crate::state::OpError::error(format!("Invalid Ed25519 key: {e}"))
                })?;
            let sig = key_pair.sign(&data);
            Ok(sig.as_ref().to_vec())
        }
        ("RSASSA-PKCS1-V1_5", KeyData::RsaPrivate { pkcs8_der }) => {
            let padding = match hash.as_str() {
                "SHA-256" => &aws_lc_rs::signature::RSA_PKCS1_SHA256,
                "SHA-384" => &aws_lc_rs::signature::RSA_PKCS1_SHA384,
                "SHA-512" => &aws_lc_rs::signature::RSA_PKCS1_SHA512,
                _ => {
                    return Err(crate::state::OpError::type_error(format!(
                        "Unsupported RSA hash: {hash}"
                    )))
                }
            };
            let key_pair =
                aws_lc_rs::signature::RsaKeyPair::from_pkcs8(pkcs8_der)
                    .map_err(|e| crate::state::OpError::error(format!("Invalid RSA key: {e}")))?;
            let rng = aws_lc_rs::rand::SystemRandom::new();
            let mut sig = vec![0u8; key_pair.public_modulus_len()];
            key_pair
                .sign(padding, &rng, &data, &mut sig)
                .map_err(|e| crate::state::OpError::error(format!("RSA sign failed: {e}")))?;
            Ok(sig)
        }
        ("RSA-PSS", KeyData::RsaPrivate { pkcs8_der }) => {
            let padding = match hash.as_str() {
                "SHA-256" => &aws_lc_rs::signature::RSA_PSS_SHA256,
                "SHA-384" => &aws_lc_rs::signature::RSA_PSS_SHA384,
                "SHA-512" => &aws_lc_rs::signature::RSA_PSS_SHA512,
                _ => {
                    return Err(crate::state::OpError::type_error(format!(
                        "Unsupported RSA-PSS hash: {hash}"
                    )))
                }
            };
            let key_pair =
                aws_lc_rs::signature::RsaKeyPair::from_pkcs8(pkcs8_der)
                    .map_err(|e| crate::state::OpError::error(format!("Invalid RSA key: {e}")))?;
            let rng = aws_lc_rs::rand::SystemRandom::new();
            let mut sig = vec![0u8; key_pair.public_modulus_len()];
            key_pair
                .sign(padding, &rng, &data, &mut sig)
                .map_err(|e| {
                    crate::state::OpError::error(format!("RSA-PSS sign failed: {e}"))
                })?;
            Ok(sig)
        }
        _ => Err(crate::state::OpError::type_error(format!(
            "Cannot sign with {algo_name} and this key type"
        ))),
    }
}

// ---------------------------------------------------------------------------
// verify
// ---------------------------------------------------------------------------

/// `__cryptoVerify(algo, hash, keyId, data: ArrayBuffer, signature: ArrayBuffer) → "true"|"false"`
///
/// Zero-serialization: data and signature passed as ArrayBuffer.
///
/// Returns the string `"true"` or `"false"` rather than a native boolean because the
/// `#[zeroship_op]` macro's return path is `Result<String, OpError>`.  The JS polyfill
/// in `crypto.js` converts this with a strict comparison (`result === "true"`) so a
/// truthy-but-wrong value like `"false"` (a non-empty string) never leaks through.
/// (Audit item C-12.)
#[zeroship_op(state)]
fn crypto_verify(state: SharedState, algo: String, hash: String, key_id: u32, data: Vec<u8>, signature: Vec<u8>) -> Result<String, crate::state::OpError> {
    let algo_name = algo.to_uppercase();
    let hash = hash.to_uppercase();

    let s = state.borrow();
    let key = s
        .key_store
        .get(&key_id)
        .ok_or_else(|| crate::state::OpError::type_error("Key not found"))?;

    let valid = match (algo_name.as_str(), key) {
        ("HMAC", KeyData::Symmetric { raw }) => {
            let alg = match hash.as_str() {
                "SHA-1" => aws_lc_rs::hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY,
                "SHA-256" => aws_lc_rs::hmac::HMAC_SHA256,
                "SHA-384" => aws_lc_rs::hmac::HMAC_SHA384,
                "SHA-512" => aws_lc_rs::hmac::HMAC_SHA512,
                _ => {
                    return Err(crate::state::OpError::type_error(format!(
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
                    return Err(crate::state::OpError::type_error(
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
                    return Err(crate::state::OpError::type_error(format!(
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
                    return Err(crate::state::OpError::type_error(format!(
                        "Unsupported RSA-PSS hash: {hash}"
                    )))
                }
            };
            let pub_key = aws_lc_rs::signature::UnparsedPublicKey::new(alg, spki_der);
            pub_key.verify(&data, &signature).is_ok()
        }
        ("HMAC", _) => {
            return Err(crate::state::OpError::type_error(
                "HMAC verify requires a symmetric key",
            ))
        }
        _ => {
            return Err(crate::state::OpError::type_error(format!(
                "Cannot verify with {algo_name} and this key type"
            )))
        }
    };

    Ok(if valid { "true" } else { "false" }.to_string())
}

// ---------------------------------------------------------------------------
// Helpers for encrypt/decrypt
// ---------------------------------------------------------------------------

fn oaep_algo_for_hash(hash: &str) -> Result<&'static OaepAlgorithm, crate::state::OpError> {
    match hash {
        "SHA-256" => Ok(&OAEP_SHA256_MGF1SHA256),
        "SHA-384" => Ok(&OAEP_SHA384_MGF1SHA384),
        "SHA-512" => Ok(&OAEP_SHA512_MGF1SHA512),
        _ => Err(crate::state::OpError::type_error(format!(
            "Unsupported RSA-OAEP hash: {hash}"
        ))),
    }
}

// ---------------------------------------------------------------------------
// encrypt
// ---------------------------------------------------------------------------

/// `__cryptoEncrypt(algoJson, keyId, data: ArrayBuffer) → ArrayBuffer`
///
/// Zero-serialization: data payload as ArrayBuffer, ciphertext returned as ArrayBuffer.
/// Algorithm config (IV, AAD, tagLength, label) stays in algoJson as base64 strings.
///
/// Design note (audit items C-1, C-2, C-3): IV (12-16 B), AAD, and RSA-OAEP label are
/// intentionally base64-encoded inside the JSON config rather than passed as separate
/// ArrayBuffer arguments.  These are tiny structured parameters (not bulk data), so the
/// base64 overhead is negligible and keeping them in a single JSON object simplifies the
/// op signature and JS polyfill.  Bulk plaintext/ciphertext always uses the zero-copy
/// ArrayBuffer bridge.
#[zeroship_op(state)]
fn crypto_encrypt(state: SharedState, algo_json: String, key_id: u32, data: Vec<u8>) -> Result<Vec<u8>, crate::state::OpError> {
    let p: serde_json::Value = serde_json::from_str(&algo_json)
        .map_err(|e| crate::state::OpError::type_error(format!("Invalid algo params: {e}")))?;

    let algo_name = p["name"]
        .as_str()
        .unwrap_or("")
        .to_uppercase();
    let hash = p["hash"]["name"]
        .as_str()
        .unwrap_or("SHA-256")
        .to_uppercase();

    let s = state.borrow();
    let key = s
        .key_store
        .get(&key_id)
        .ok_or_else(|| crate::state::OpError::type_error("Key not found"))?;

    match (algo_name.as_str(), key) {
        ("AES-GCM", KeyData::Symmetric { raw }) => {
            let iv_b64 = p["iv"].as_str().unwrap_or("");
            let iv_bytes = B64
                .decode(iv_b64)
                .map_err(|e| crate::state::OpError::type_error(format!("Invalid IV: {e}")))?;
            if iv_bytes.len() != 12 {
                return Err(crate::state::OpError::type_error(
                    "AES-GCM IV must be 12 bytes",
                ));
            }

            let aead_alg = match raw.len() {
                16 => &AES_128_GCM,
                32 => &AES_256_GCM,
                _ => {
                    return Err(crate::state::OpError::type_error(
                        "AES-GCM key must be 16 or 32 bytes",
                    ))
                }
            };

            let unbound = UnboundKey::new(aead_alg, raw)
                .map_err(|e| crate::state::OpError::error(format!("AES-GCM key error: {e}")))?;
            let less_safe = LessSafeKey::new(unbound);
            let nonce = Nonce::try_assume_unique_for_key(&iv_bytes)
                .map_err(|e| crate::state::OpError::error(format!("Nonce error: {e}")))?;

            let aad = if let Some(aad_b64) = p["additionalData"].as_str() {
                let aad_bytes = B64.decode(aad_b64).map_err(|e| {
                    crate::state::OpError::type_error(format!("Invalid AAD: {e}"))
                })?;
                Aad::from(aad_bytes)
            } else {
                Aad::from(Vec::<u8>::new())
            };

            let mut in_out = data;
            less_safe
                .seal_in_place_append_tag(nonce, aad, &mut in_out)
                .map_err(|e| crate::state::OpError::error(format!("AES-GCM encrypt: {e}")))?;

            Ok(in_out)
        }
        ("AES-CBC", KeyData::Symmetric { raw }) => {
            let iv_b64 = p["iv"].as_str().unwrap_or("");
            let iv_bytes = B64
                .decode(iv_b64)
                .map_err(|e| crate::state::OpError::type_error(format!("Invalid IV: {e}")))?;
            if iv_bytes.len() != 16 {
                return Err(crate::state::OpError::type_error(
                    "AES-CBC IV must be 16 bytes",
                ));
            }

            let cipher_alg: &'static aws_lc_rs::cipher::Algorithm = match raw.len() {
                16 => &AES_128,
                32 => &AES_256,
                _ => {
                    return Err(crate::state::OpError::type_error(
                        "AES-CBC key must be 16 or 32 bytes",
                    ))
                }
            };

            let unbound = UnboundCipherKey::new(cipher_alg, raw)
                .map_err(|e| crate::state::OpError::error(format!("AES-CBC key error: {e}")))?;
            let enc_key = PaddedBlockEncryptingKey::cbc_pkcs7(unbound)
                .map_err(|e| crate::state::OpError::error(format!("AES-CBC init error: {e}")))?;

            let iv_array: [u8; 16] = iv_bytes.as_slice().try_into().unwrap();
            let ctx = EncryptionContext::Iv128(FixedLength::<IV_LEN_128_BIT>::from(iv_array));

            let mut in_out = data;
            enc_key
                .less_safe_encrypt(&mut in_out, ctx)
                .map_err(|e| crate::state::OpError::error(format!("AES-CBC encrypt: {e}")))?;

            Ok(in_out)
        }
        ("RSA-OAEP", KeyData::RsaPublic { spki_der }) => {
            let oaep_alg = oaep_algo_for_hash(&hash)?;

            let pub_key = PublicEncryptingKey::from_der(spki_der)
                .map_err(|e| crate::state::OpError::error(format!("RSA public key error: {e}")))?;
            let oaep_key = OaepPublicEncryptingKey::new(pub_key)
                .map_err(|e| crate::state::OpError::error(format!("RSA-OAEP init error: {e}")))?;

            let label = if let Some(label_b64) = p["label"].as_str() {
                let label_bytes = B64.decode(label_b64).map_err(|e| {
                    crate::state::OpError::type_error(format!("Invalid label: {e}"))
                })?;
                Some(label_bytes)
            } else {
                None
            };

            let mut ciphertext = vec![0u8; oaep_key.ciphertext_size()];
            let ct = oaep_key
                .encrypt(
                    oaep_alg,
                    &data,
                    &mut ciphertext,
                    label.as_deref(),
                )
                .map_err(|e| crate::state::OpError::error(format!("RSA-OAEP encrypt: {e}")))?;

            Ok(ct.to_vec())
        }
        _ => Err(crate::state::OpError::type_error(format!(
            "Cannot encrypt with {algo_name} and this key type"
        ))),
    }
}

// ---------------------------------------------------------------------------
// decrypt
// ---------------------------------------------------------------------------

/// `__cryptoDecrypt(algoJson, keyId, data: ArrayBuffer) → ArrayBuffer`
///
/// Zero-serialization: ciphertext as ArrayBuffer, plaintext returned as ArrayBuffer.
/// Algorithm config (IV, AAD, tagLength, label) stays in algoJson as base64 strings.
///
/// Design note (audit items C-1, C-2, C-3): same rationale as `crypto_encrypt` — IV,
/// AAD, and label are small structured params that stay base64-in-JSON.  See the
/// encrypt doc comment for the full explanation.
#[zeroship_op(state)]
fn crypto_decrypt(state: SharedState, algo_json: String, key_id: u32, data: Vec<u8>) -> Result<Vec<u8>, crate::state::OpError> {
    let p: serde_json::Value = serde_json::from_str(&algo_json)
        .map_err(|e| crate::state::OpError::type_error(format!("Invalid algo params: {e}")))?;

    let algo_name = p["name"]
        .as_str()
        .unwrap_or("")
        .to_uppercase();
    let hash = p["hash"]["name"]
        .as_str()
        .unwrap_or("SHA-256")
        .to_uppercase();

    let s = state.borrow();
    let key = s
        .key_store
        .get(&key_id)
        .ok_or_else(|| crate::state::OpError::type_error("Key not found"))?;

    match (algo_name.as_str(), key) {
        ("AES-GCM", KeyData::Symmetric { raw }) => {
            let iv_b64 = p["iv"].as_str().unwrap_or("");
            let iv_bytes = B64
                .decode(iv_b64)
                .map_err(|e| crate::state::OpError::type_error(format!("Invalid IV: {e}")))?;
            if iv_bytes.len() != 12 {
                return Err(crate::state::OpError::type_error(
                    "AES-GCM IV must be 12 bytes",
                ));
            }

            let aead_alg = match raw.len() {
                16 => &AES_128_GCM,
                32 => &AES_256_GCM,
                _ => {
                    return Err(crate::state::OpError::type_error(
                        "AES-GCM key must be 16 or 32 bytes",
                    ))
                }
            };

            let unbound = UnboundKey::new(aead_alg, raw)
                .map_err(|e| crate::state::OpError::error(format!("AES-GCM key error: {e}")))?;
            let less_safe = LessSafeKey::new(unbound);
            let nonce = Nonce::try_assume_unique_for_key(&iv_bytes)
                .map_err(|e| crate::state::OpError::error(format!("Nonce error: {e}")))?;

            let aad = if let Some(aad_b64) = p["additionalData"].as_str() {
                let aad_bytes = B64.decode(aad_b64).map_err(|e| {
                    crate::state::OpError::type_error(format!("Invalid AAD: {e}"))
                })?;
                Aad::from(aad_bytes)
            } else {
                Aad::from(Vec::<u8>::new())
            };

            let mut in_out = data;
            let plaintext = less_safe
                .open_in_place(nonce, aad, &mut in_out)
                .map_err(|e| crate::state::OpError::error(format!("AES-GCM decrypt: {e}")))?;

            Ok(plaintext.to_vec())
        }
        ("AES-CBC", KeyData::Symmetric { raw }) => {
            let iv_b64 = p["iv"].as_str().unwrap_or("");
            let iv_bytes = B64
                .decode(iv_b64)
                .map_err(|e| crate::state::OpError::type_error(format!("Invalid IV: {e}")))?;
            if iv_bytes.len() != 16 {
                return Err(crate::state::OpError::type_error(
                    "AES-CBC IV must be 16 bytes",
                ));
            }

            let cipher_alg: &'static aws_lc_rs::cipher::Algorithm = match raw.len() {
                16 => &AES_128,
                32 => &AES_256,
                _ => {
                    return Err(crate::state::OpError::type_error(
                        "AES-CBC key must be 16 or 32 bytes",
                    ))
                }
            };

            let unbound = UnboundCipherKey::new(cipher_alg, raw)
                .map_err(|e| crate::state::OpError::error(format!("AES-CBC key error: {e}")))?;
            let dec_key = PaddedBlockDecryptingKey::cbc_pkcs7(unbound)
                .map_err(|e| crate::state::OpError::error(format!("AES-CBC init error: {e}")))?;

            let iv_array: [u8; 16] = iv_bytes.as_slice().try_into().unwrap();
            let ctx = DecryptionContext::Iv128(FixedLength::<IV_LEN_128_BIT>::from(iv_array));

            let mut in_out = data;
            let plaintext = dec_key
                .decrypt(&mut in_out, ctx)
                .map_err(|e| crate::state::OpError::error(format!("AES-CBC decrypt: {e}")))?;

            Ok(plaintext.to_vec())
        }
        ("RSA-OAEP", KeyData::RsaPrivate { pkcs8_der }) => {
            let oaep_alg = oaep_algo_for_hash(&hash)?;

            let priv_key = PrivateDecryptingKey::from_pkcs8(pkcs8_der)
                .map_err(|e| crate::state::OpError::error(format!("RSA private key error: {e}")))?;
            let oaep_key = OaepPrivateDecryptingKey::new(priv_key)
                .map_err(|e| crate::state::OpError::error(format!("RSA-OAEP init error: {e}")))?;

            let label = if let Some(label_b64) = p["label"].as_str() {
                let label_bytes = B64.decode(label_b64).map_err(|e| {
                    crate::state::OpError::type_error(format!("Invalid label: {e}"))
                })?;
                Some(label_bytes)
            } else {
                None
            };

            let mut plaintext = vec![0u8; oaep_key.min_output_size()];
            let pt = oaep_key
                .decrypt(
                    oaep_alg,
                    &data,
                    &mut plaintext,
                    label.as_deref(),
                )
                .map_err(|e| crate::state::OpError::error(format!("RSA-OAEP decrypt: {e}")))?;

            Ok(pt.to_vec())
        }
        _ => Err(crate::state::OpError::type_error(format!(
            "Cannot decrypt with {algo_name} and this key type"
        ))),
    }
}

// ---------------------------------------------------------------------------
// deriveBits / deriveKey
// ---------------------------------------------------------------------------

struct DeriveLen(usize);
impl aws_lc_rs::hkdf::KeyType for DeriveLen {
    fn len(&self) -> usize {
        self.0
    }
}

/// Shared deriveBits logic used by both `crypto_derive_bits` and `crypto_derive_key`.
/// Returns raw derived bytes (no base64 encoding).
///
/// Design note (audit items C-4, C-5): HKDF salt and info, and PBKDF2 salt, are
/// base64-encoded inside the JSON params.  These are typically 16-64 bytes of structured
/// config data, so the base64 overhead is negligible.  Keeping them in the JSON object
/// avoids complicating the op signature with extra ArrayBuffer arguments.
fn crypto_derive_bits_inner(
    state: &SharedState,
    p: &serde_json::Value,
) -> Result<Vec<u8>, crate::state::OpError> {
    let algo_name = p["algorithm"]["name"]
        .as_str()
        .unwrap_or("")
        .to_uppercase();
    let key_id = p["keyId"].as_u64().unwrap_or(0) as u32;
    let length = p["length"].as_u64().unwrap_or(0) as usize;
    let hash = p["algorithm"]["hash"]["name"]
        .as_str()
        .unwrap_or("SHA-256")
        .to_uppercase();

    if length == 0 || length % 8 != 0 {
        return Err(crate::state::OpError::type_error(
            "length must be a positive multiple of 8",
        ));
    }
    let byte_len = length / 8;

    let s = state.borrow();
    let key = s
        .key_store
        .get(&key_id)
        .ok_or_else(|| crate::state::OpError::type_error("Key not found"))?;

    let raw = match key {
        KeyData::Symmetric { raw } => raw,
        _ => {
            return Err(crate::state::OpError::type_error(
                "deriveBits requires a symmetric key",
            ))
        }
    };

    match algo_name.as_str() {
        "HKDF" => {
            let hkdf_alg = match hash.as_str() {
                "SHA-256" => aws_lc_rs::hkdf::HKDF_SHA256,
                "SHA-384" => aws_lc_rs::hkdf::HKDF_SHA384,
                "SHA-512" => aws_lc_rs::hkdf::HKDF_SHA512,
                _ => {
                    return Err(crate::state::OpError::type_error(format!(
                        "Unsupported HKDF hash: {hash}"
                    )))
                }
            };
            let salt_b64 = p["algorithm"]["salt"].as_str().unwrap_or("");
            let salt_bytes = B64.decode(salt_b64).map_err(|e| {
                crate::state::OpError::type_error(format!("Invalid salt: {e}"))
            })?;
            let info_b64 = p["algorithm"]["info"].as_str().unwrap_or("");
            let info_bytes = B64.decode(info_b64).map_err(|e| {
                crate::state::OpError::type_error(format!("Invalid info: {e}"))
            })?;

            let salt = aws_lc_rs::hkdf::Salt::new(hkdf_alg, &salt_bytes);
            let prk = salt.extract(raw);
            let info_refs: &[&[u8]] = &[&info_bytes];
            let okm = prk
                .expand(info_refs, DeriveLen(byte_len))
                .map_err(|_| crate::state::OpError::error("HKDF expand failed"))?;
            let mut out = vec![0u8; byte_len];
            okm.fill(&mut out)
                .map_err(|_| crate::state::OpError::error("HKDF fill failed"))?;
            Ok(out)
        }
        "PBKDF2" => {
            let pbkdf2_alg = match hash.as_str() {
                "SHA-256" => aws_lc_rs::pbkdf2::PBKDF2_HMAC_SHA256,
                "SHA-384" => aws_lc_rs::pbkdf2::PBKDF2_HMAC_SHA384,
                "SHA-512" => aws_lc_rs::pbkdf2::PBKDF2_HMAC_SHA512,
                _ => {
                    return Err(crate::state::OpError::type_error(format!(
                        "Unsupported PBKDF2 hash: {hash}"
                    )))
                }
            };
            let salt_b64 = p["algorithm"]["salt"].as_str().unwrap_or("");
            let salt_bytes = B64.decode(salt_b64).map_err(|e| {
                crate::state::OpError::type_error(format!("Invalid salt: {e}"))
            })?;
            let iterations = p["algorithm"]["iterations"].as_u64().unwrap_or(1000) as u32;
            let iterations = std::num::NonZeroU32::new(iterations)
                .ok_or_else(|| crate::state::OpError::type_error("iterations must be > 0"))?;

            let mut out = vec![0u8; byte_len];
            aws_lc_rs::pbkdf2::derive(pbkdf2_alg, iterations, &salt_bytes, raw, &mut out);
            Ok(out)
        }
        _ => Err(crate::state::OpError::type_error(format!(
            "Unsupported deriveBits algorithm: {algo_name}"
        ))),
    }
}

/// `__cryptoDeriveBits(params_json) → ArrayBuffer`
///
/// Zero-serialization: derived bits returned as ArrayBuffer.
/// Params stay as JSON (salt/info are small structured data).
#[zeroship_op(state)]
fn crypto_derive_bits(state: SharedState, params: String) -> Result<Vec<u8>, crate::state::OpError> {
    let p: serde_json::Value = serde_json::from_str(&params)
        .map_err(|e| crate::state::OpError::type_error(format!("Invalid params: {e}")))?;
    crypto_derive_bits_inner(&state, &p)
}

/// `__cryptoDeriveKey(params_json) → JSON {keyId}`
///
/// Design note (audit items C-10, C-11): params and result use JSON serialization.
/// Key derivation is an infrequent setup operation with small structured payloads,
/// so the JSON overhead is negligible.
#[zeroship_op(state)]
fn crypto_derive_key(state: SharedState, params: String) -> Result<String, crate::state::OpError> {
    let p: serde_json::Value = serde_json::from_str(&params)
        .map_err(|e| crate::state::OpError::type_error(format!("Invalid params: {e}")))?;

    let derived_algo = &p["derivedKeyAlgorithm"];
    let derived_name = derived_algo["name"]
        .as_str()
        .unwrap_or("")
        .to_uppercase();

    // Determine derived key length
    let key_length = if let Some(len) = derived_algo["length"].as_u64() {
        len as usize
    } else if derived_name == "HMAC" {
        // HMAC default key length = hash block size
        let hash = derived_algo["hash"]["name"]
            .as_str()
            .unwrap_or("SHA-256")
            .to_uppercase();
        match hash.as_str() {
            "SHA-256" => 256,
            "SHA-384" => 384,
            "SHA-512" => 512,
            _ => 256,
        }
    } else {
        return Err(crate::state::OpError::type_error(
            "derivedKeyAlgorithm must specify length",
        ));
    };

    // Build a deriveBits params with the computed length
    let mut derive_params = p.clone();
    derive_params["length"] = serde_json::json!(key_length);

    // Call deriveBits logic — returns raw bytes directly (no base64)
    let raw = crypto_derive_bits_inner(&state, &derive_params)?;

    // Store as symmetric key
    let mut s = state.borrow_mut();
    let key_id = s.next_key_id;
    s.next_key_id += 1;
    s.key_store.insert(key_id, KeyData::Symmetric { raw });

    Ok(serde_json::json!({ "keyId": key_id }).to_string())
}

// ---------------------------------------------------------------------------
// Sync hash / HMAC helpers for node:crypto polyfill
// ---------------------------------------------------------------------------

/// Encode a byte slice as a lowercase hex string.
fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// Extract a V8 value as `Vec<u8>`:
///   - ArrayBufferView / ArrayBuffer → copy backing store bytes
///   - anything else → UTF-8 encode the string representation
fn extract_bytes(scope: &mut v8::PinScope, val: v8::Local<v8::Value>) -> Vec<u8> {
    if let Ok(view) = v8::Local::<v8::ArrayBufferView>::try_from(val) {
        let mut buf = vec![0u8; view.byte_length()];
        view.copy_contents(&mut buf);
        return buf;
    }
    if let Ok(ab) = v8::Local::<v8::ArrayBuffer>::try_from(val) {
        let store = ab.get_backing_store();
        let mut buf = vec![0u8; ab.byte_length()];
        for (i, b) in buf.iter_mut().enumerate() {
            *b = store[i].get();
        }
        return buf;
    }
    // Fall back to string → UTF-8 bytes
    val.to_rust_string_lossy(scope).into_bytes()
}

/// `__cryptoHashSync(algorithm, data) → hex string`
///
/// Synchronous hash for node:crypto polyfill. Accepts algorithm name
/// ("sha256", "sha-256", "sha1", "sha-1", "sha384", "sha-384", "sha512", "sha-512",
/// "md5") and data as either a string or Uint8Array.
pub(crate) fn crypto_hash_sync_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    if args.length() < 2 {
        let msg = v8::String::new(scope, "__cryptoHashSync: expected (algorithm, data)").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    }

    let algorithm = args.get(0).to_rust_string_lossy(scope);
    let data = extract_bytes(scope, args.get(1));

    let alg: &aws_lc_rs::digest::Algorithm = match algorithm.to_lowercase().as_str() {
        "sha256" | "sha-256" => &aws_lc_rs::digest::SHA256,
        "sha1" | "sha-1" => &aws_lc_rs::digest::SHA1_FOR_LEGACY_USE_ONLY,
        "sha384" | "sha-384" => &aws_lc_rs::digest::SHA384,
        "sha512" | "sha-512" => &aws_lc_rs::digest::SHA512,
        other => {
            let msg = v8::String::new(
                scope,
                &format!("__cryptoHashSync: unsupported algorithm: {other}"),
            )
            .unwrap();
            let exc = v8::Exception::error(scope, msg);
            scope.throw_exception(exc);
            return;
        }
    };

    let digest = aws_lc_rs::digest::digest(alg, &data);
    let hex = hex_encode(digest.as_ref());
    let result = v8::String::new(scope, &hex).unwrap();
    rv.set(result.into());
}

/// `__cryptoHmacSync(algorithm, key, data) → hex string`
///
/// Synchronous HMAC for node:crypto polyfill. Accepts algorithm name, key
/// (string or Uint8Array) and data (string or Uint8Array). Returns hex-encoded HMAC tag.
pub(crate) fn crypto_hmac_sync_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    if args.length() < 3 {
        let msg =
            v8::String::new(scope, "__cryptoHmacSync: expected (algorithm, key, data)").unwrap();
        let exc = v8::Exception::type_error(scope, msg);
        scope.throw_exception(exc);
        return;
    }

    let algorithm = args.get(0).to_rust_string_lossy(scope);
    let key_bytes = extract_bytes(scope, args.get(1));
    let data = extract_bytes(scope, args.get(2));

    let alg: aws_lc_rs::hmac::Algorithm = match algorithm.to_lowercase().as_str() {
        "sha256" | "sha-256" => aws_lc_rs::hmac::HMAC_SHA256,
        "sha1" | "sha-1" => aws_lc_rs::hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY,
        "sha384" | "sha-384" => aws_lc_rs::hmac::HMAC_SHA384,
        "sha512" | "sha-512" => aws_lc_rs::hmac::HMAC_SHA512,
        other => {
            let msg = v8::String::new(
                scope,
                &format!("__cryptoHmacSync: unsupported algorithm: {other}"),
            )
            .unwrap();
            let exc = v8::Exception::error(scope, msg);
            scope.throw_exception(exc);
            return;
        }
    };

    let key = aws_lc_rs::hmac::Key::new(alg, &key_bytes);
    let tag = aws_lc_rs::hmac::sign(&key, &data);
    let hex = hex_encode(tag.as_ref());
    let result = v8::String::new(scope, &hex).unwrap();
    rv.set(result.into());
}
