//! `generateKeyPairSync` / `generateKeyPair` / `generateKeySync` /
//! `generateKey`.
//!
//! Per `docs/proposals/node-crypto-native.md` §II.7 (D-N6 sync,
//! Stage C scope).
//!
//! Strategy: for every supported asymmetric type (`rsa`, `ec`,
//! `ed25519`, `x25519`) we delegate to aws-lc-rs's keygen helpers
//! (the same ones the WebCrypto surface uses). For symmetric
//! (`hmac`, `aes`) we generate random bytes via aws-lc-rs RNG and
//! wrap in a `SecretKeyObject`.

#![allow(unsafe_code)]

use super::buffer;
use super::key_object::{self, KeyObjectState};
use super::super::crypto::key_material::{
    KeyMaterial, KeyType, NamedCurve, RsaPrivateComponents, RsaPublicComponents,
};
use crate::state::OpError;

// ---------------------------------------------------------------------------
// generateKeySync(type, options) -> KeyObject
// ---------------------------------------------------------------------------

pub fn generate_key_sync<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    key_type: &str,
    options: Option<v8::Local<v8::Value>>,
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    match key_type {
        "hmac" | "aes" => {
            let length_bits = read_length(scope, options)?;
            let bytes = (length_bits as usize + 7) / 8;
            let mut buf = vec![0u8; bytes];
            use aws_lc_rs::rand::SecureRandom;
            let rng = aws_lc_rs::rand::SystemRandom::new();
            rng.fill(&mut buf).map_err(|_| {
                OpError::node("ERR_CRYPTO_OPERATION_FAILED", "RNG fill failed")
            })?;
            let state = KeyObjectState {
                key_type: KeyType::Secret,
                material: KeyMaterial::Symmetric(buf),
            };
            Ok(key_object::build_secret(scope, state).into())
        }
        other => Err(OpError::node(
            "ERR_INVALID_ARG_VALUE",
            format!("Unknown generateKey type: {other}"),
        )),
    }
}

fn read_length(
    scope: &mut v8::PinScope,
    options: Option<v8::Local<v8::Value>>,
) -> Result<u32, OpError> {
    let Some(opts) = options else {
        return Ok(256);
    };
    if opts.is_undefined() || opts.is_null() {
        return Ok(256);
    }
    let obj: v8::Local<v8::Object> = opts.try_into().map_err(|_| {
        OpError::node("ERR_INVALID_ARG_TYPE", "options must be an object")
    })?;
    let k = v8::String::new(scope, "length").unwrap();
    let v = match obj.get(scope, k.into()) {
        Some(v) if !v.is_undefined() && !v.is_null() => v,
        _ => return Ok(256),
    };
    Ok(v.uint32_value(scope).unwrap_or(256))
}

// ---------------------------------------------------------------------------
// generateKeyPairSync(type, options) -> { publicKey, privateKey }
// ---------------------------------------------------------------------------

pub fn generate_key_pair_sync<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    key_type: &str,
    options: Option<v8::Local<v8::Value>>,
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    let (priv_state, pub_state) = match key_type {
        "rsa" => generate_rsa(scope, options)?,
        "ec" => generate_ec(scope, options)?,
        "ed25519" => generate_ed25519()?,
        "x25519" => generate_x25519()?,
        other => {
            return Err(OpError::node(
                "ERR_INVALID_ARG_VALUE",
                format!("Unknown generateKeyPair type: {other}"),
            ));
        }
    };
    let private_key = key_object::build_private(scope, priv_state);
    let public_key = key_object::build_public(scope, pub_state);
    let result = v8::Object::new(scope);
    let k = v8::String::new(scope, "publicKey").unwrap();
    result.set(scope, k.into(), public_key.into());
    let k = v8::String::new(scope, "privateKey").unwrap();
    result.set(scope, k.into(), private_key.into());
    Ok(result.into())
}

fn generate_rsa(
    scope: &mut v8::PinScope,
    options: Option<v8::Local<v8::Value>>,
) -> Result<(KeyObjectState, KeyObjectState), OpError> {
    let modulus_bits = read_modulus_length(scope, options)?;
    use aws_lc_rs::rsa::KeySize;
    let size = match modulus_bits {
        2048 => KeySize::Rsa2048,
        3072 => KeySize::Rsa3072,
        4096 => KeySize::Rsa4096,
        n => {
            return Err(OpError::node(
                "ERR_INVALID_ARG_VALUE",
                format!("Unsupported RSA modulus length: {n}"),
            ));
        }
    };
    let priv_key = aws_lc_rs::rsa::PrivateDecryptingKey::generate(size).map_err(|_| {
        OpError::node("ERR_CRYPTO_OPERATION_FAILED", "RSA keygen failed")
    })?;
    let pub_part = priv_key.public_key();
    use aws_lc_rs::encoding::AsDer;
    let pkcs8: aws_lc_rs::encoding::Pkcs8V1Der<'static> = AsDer::as_der(&priv_key)
        .map_err(|_| OpError::node("ERR_CRYPTO_OPERATION_FAILED", "RSA serialize PKCS#8"))?;
    let spki: aws_lc_rs::encoding::PublicKeyX509Der<'static> = AsDer::as_der(&pub_part)
        .map_err(|_| OpError::node("ERR_CRYPTO_OPERATION_FAILED", "RSA serialize SPKI"))?;
    let pkcs8_bytes = pkcs8.as_ref().to_vec();
    let spki_bytes = spki.as_ref().to_vec();
    let pub_components = RsaPublicComponents {
        n: Vec::new(),
        e: vec![0x01, 0x00, 0x01],
    };
    let priv_components = RsaPrivateComponents {
        n: Vec::new(),
        e: vec![0x01, 0x00, 0x01],
        d: Vec::new(),
        p: Vec::new(),
        q: Vec::new(),
        dp: Vec::new(),
        dq: Vec::new(),
        qi: Vec::new(),
    };
    Ok((
        KeyObjectState {
            key_type: KeyType::Private,
            material: KeyMaterial::RsaPrivate {
                pkcs8_der: pkcs8_bytes,
                components: priv_components,
            },
        },
        KeyObjectState {
            key_type: KeyType::Public,
            material: KeyMaterial::RsaPublic {
                spki_der: spki_bytes,
                components: pub_components,
            },
        },
    ))
}

fn read_modulus_length(
    scope: &mut v8::PinScope,
    options: Option<v8::Local<v8::Value>>,
) -> Result<u32, OpError> {
    let Some(opts) = options else { return Ok(2048) };
    if opts.is_undefined() || opts.is_null() {
        return Ok(2048);
    }
    let obj: v8::Local<v8::Object> = opts.try_into().map_err(|_| {
        OpError::node("ERR_INVALID_ARG_TYPE", "options must be an object")
    })?;
    let k = v8::String::new(scope, "modulusLength").unwrap();
    match obj.get(scope, k.into()) {
        Some(v) if !v.is_undefined() && !v.is_null() => Ok(v.uint32_value(scope).unwrap_or(2048)),
        _ => Ok(2048),
    }
}

fn generate_ec(
    scope: &mut v8::PinScope,
    options: Option<v8::Local<v8::Value>>,
) -> Result<(KeyObjectState, KeyObjectState), OpError> {
    let curve = read_named_curve(scope, options)?;
    use aws_lc_rs::signature as sig;
    let alg = match curve {
        NamedCurve::P256 => &sig::ECDSA_P256_SHA256_FIXED_SIGNING,
        NamedCurve::P384 => &sig::ECDSA_P384_SHA384_FIXED_SIGNING,
        NamedCurve::P521 => &sig::ECDSA_P521_SHA512_FIXED_SIGNING,
    };
    let rng = aws_lc_rs::rand::SystemRandom::new();
    let pkcs8_doc = sig::EcdsaKeyPair::generate_pkcs8(alg, &rng).map_err(|_| {
        OpError::node("ERR_CRYPTO_OPERATION_FAILED", "EC keygen failed")
    })?;
    let pkcs8_bytes = pkcs8_doc.as_ref().to_vec();
    let key_pair = sig::EcdsaKeyPair::from_pkcs8(alg, &pkcs8_bytes).map_err(|_| {
        OpError::node("ERR_CRYPTO_OPERATION_FAILED", "EC keypair load")
    })?;
    use aws_lc_rs::signature::KeyPair;
    let raw_xy = key_pair.public_key().as_ref().to_vec();
    // Build SPKI from raw_xy.
    let spki_der = build_ec_spki(&raw_xy)?;
    Ok((
        KeyObjectState {
            key_type: KeyType::Private,
            material: KeyMaterial::EcPrivate {
                pkcs8_der: pkcs8_bytes,
                raw_d: Vec::new(),
                raw_xy: raw_xy.clone(),
            },
        },
        KeyObjectState {
            key_type: KeyType::Public,
            material: KeyMaterial::EcPublic {
                spki_der,
                raw_xy,
            },
        },
    ))
}

fn read_named_curve(
    scope: &mut v8::PinScope,
    options: Option<v8::Local<v8::Value>>,
) -> Result<NamedCurve, OpError> {
    let Some(opts) = options else {
        return Err(OpError::node(
            "ERR_MISSING_OPTION",
            "options.namedCurve required",
        ));
    };
    let obj: v8::Local<v8::Object> = opts.try_into().map_err(|_| {
        OpError::node("ERR_INVALID_ARG_TYPE", "options must be an object")
    })?;
    let k = v8::String::new(scope, "namedCurve").unwrap();
    let v = obj.get(scope, k.into()).ok_or_else(|| {
        OpError::node("ERR_MISSING_OPTION", "options.namedCurve required")
    })?;
    let name = v.to_rust_string_lossy(scope);
    // Accept both spec-canonical (`P-256`) and Node's OpenSSL canonical
    // (`prime256v1`) names.
    let curve = match name.as_str() {
        "P-256" | "prime256v1" => NamedCurve::P256,
        "P-384" | "secp384r1" => NamedCurve::P384,
        "P-521" | "secp521r1" => NamedCurve::P521,
        _ => {
            return Err(OpError::node(
                "ERR_INVALID_ARG_VALUE",
                format!("Unknown curve: {name}"),
            ));
        }
    };
    Ok(curve)
}

fn build_ec_spki(raw_xy: &[u8]) -> Result<Vec<u8>, OpError> {
    fn der_len(n: usize) -> Vec<u8> {
        if n < 128 {
            vec![n as u8]
        } else if n < 256 {
            vec![0x81, n as u8]
        } else {
            vec![0x82, ((n >> 8) & 0xff) as u8, (n & 0xff) as u8]
        }
    }
    fn der_seq(body: &[u8]) -> Vec<u8> {
        let mut out = vec![0x30];
        out.extend_from_slice(&der_len(body.len()));
        out.extend_from_slice(body);
        out
    }
    fn der_bit_string(payload: &[u8]) -> Vec<u8> {
        let mut content = vec![0x00u8];
        content.extend_from_slice(payload);
        let mut out = vec![0x03];
        out.extend_from_slice(&der_len(content.len()));
        out.extend_from_slice(&content);
        out
    }
    const OID_EC_PUBLIC_KEY: &[u8] =
        &[0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01];
    const OID_P256: &[u8] = &[0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07];
    const OID_P384: &[u8] = &[0x06, 0x05, 0x2b, 0x81, 0x04, 0x00, 0x22];
    const OID_P521: &[u8] = &[0x06, 0x05, 0x2b, 0x81, 0x04, 0x00, 0x23];
    let curve_oid: &[u8] = match raw_xy.len() {
        65 => OID_P256,
        97 => OID_P384,
        133 => OID_P521,
        _ => {
            return Err(OpError::node(
                "ERR_CRYPTO_OPERATION_FAILED",
                "Unknown EC point length",
            ));
        }
    };
    let mut alg_id_body = Vec::new();
    alg_id_body.extend_from_slice(OID_EC_PUBLIC_KEY);
    alg_id_body.extend_from_slice(curve_oid);
    let alg_id = der_seq(&alg_id_body);
    let bs = der_bit_string(raw_xy);
    let mut body = Vec::new();
    body.extend_from_slice(&alg_id);
    body.extend_from_slice(&bs);
    Ok(der_seq(&body))
}

fn generate_ed25519() -> Result<(KeyObjectState, KeyObjectState), OpError> {
    use aws_lc_rs::signature::{Ed25519KeyPair, KeyPair};
    let rng = aws_lc_rs::rand::SystemRandom::new();
    let doc = Ed25519KeyPair::generate_pkcs8(&rng).map_err(|_| {
        OpError::node("ERR_CRYPTO_OPERATION_FAILED", "Ed25519 keygen failed")
    })?;
    let pkcs8 = doc.as_ref().to_vec();
    let kp = Ed25519KeyPair::from_pkcs8_maybe_unchecked(&pkcs8).map_err(|_| {
        OpError::node("ERR_CRYPTO_OPERATION_FAILED", "Ed25519 PKCS8 reload")
    })?;
    let pub_bytes = kp.public_key().as_ref();
    let mut raw_x = [0u8; 32];
    raw_x.copy_from_slice(&pub_bytes[..32]);
    let raw_d = [0u8; 32]; // we don't expose the raw scalar; pkcs8 carries it
    let spki = build_ed25519_spki(&raw_x);
    Ok((
        KeyObjectState {
            key_type: KeyType::Private,
            material: KeyMaterial::Ed25519Private {
                pkcs8_der: pkcs8,
                raw_d,
                raw_x,
            },
        },
        KeyObjectState {
            key_type: KeyType::Public,
            material: KeyMaterial::Ed25519Public {
                spki_der: spki,
                raw_x,
            },
        },
    ))
}

fn generate_x25519() -> Result<(KeyObjectState, KeyObjectState), OpError> {
    use aws_lc_rs::agreement;
    use aws_lc_rs::rand::SecureRandom;
    let mut seed = [0u8; 32];
    let rng = aws_lc_rs::rand::SystemRandom::new();
    rng.fill(&mut seed).map_err(|_| {
        OpError::node("ERR_CRYPTO_OPERATION_FAILED", "X25519 RNG fill failed")
    })?;
    let priv_key = agreement::PrivateKey::from_private_key(&agreement::X25519, &seed).map_err(|_| {
        OpError::node("ERR_CRYPTO_OPERATION_FAILED", "X25519 key load")
    })?;
    let pub_key = priv_key.compute_public_key().map_err(|_| {
        OpError::node("ERR_CRYPTO_OPERATION_FAILED", "X25519 pubkey derive")
    })?;
    let pub_bytes = pub_key.as_ref();
    let mut raw_x = [0u8; 32];
    raw_x.copy_from_slice(pub_bytes);
    let raw_d = seed;
    let pkcs8 = build_x25519_pkcs8(&raw_d);
    let spki = build_x25519_spki(&raw_x);
    Ok((
        KeyObjectState {
            key_type: KeyType::Private,
            material: KeyMaterial::X25519Private {
                pkcs8_der: pkcs8,
                raw_d,
                raw_x,
            },
        },
        KeyObjectState {
            key_type: KeyType::Public,
            material: KeyMaterial::X25519Public {
                spki_der: spki,
                raw_x,
            },
        },
    ))
}

fn build_ed25519_spki(raw_x: &[u8; 32]) -> Vec<u8> {
    fn der_len(n: usize) -> Vec<u8> {
        if n < 128 {
            vec![n as u8]
        } else if n < 256 {
            vec![0x81, n as u8]
        } else {
            vec![0x82, ((n >> 8) & 0xff) as u8, (n & 0xff) as u8]
        }
    }
    fn der_seq(body: &[u8]) -> Vec<u8> {
        let mut out = vec![0x30];
        out.extend_from_slice(&der_len(body.len()));
        out.extend_from_slice(body);
        out
    }
    fn der_bit_string(payload: &[u8]) -> Vec<u8> {
        let mut content = vec![0x00u8];
        content.extend_from_slice(payload);
        let mut out = vec![0x03];
        out.extend_from_slice(&der_len(content.len()));
        out.extend_from_slice(&content);
        out
    }
    const OID: &[u8] = &[0x06, 0x03, 0x2b, 0x65, 0x70];
    let alg_id = der_seq(OID);
    let bs = der_bit_string(raw_x);
    let mut body = Vec::new();
    body.extend_from_slice(&alg_id);
    body.extend_from_slice(&bs);
    der_seq(&body)
}

fn build_x25519_spki(raw_x: &[u8; 32]) -> Vec<u8> {
    fn der_len(n: usize) -> Vec<u8> {
        if n < 128 {
            vec![n as u8]
        } else {
            vec![0x81, n as u8]
        }
    }
    fn der_seq(body: &[u8]) -> Vec<u8> {
        let mut out = vec![0x30];
        out.extend_from_slice(&der_len(body.len()));
        out.extend_from_slice(body);
        out
    }
    fn der_bit_string(payload: &[u8]) -> Vec<u8> {
        let mut content = vec![0x00u8];
        content.extend_from_slice(payload);
        let mut out = vec![0x03];
        out.extend_from_slice(&der_len(content.len()));
        out.extend_from_slice(&content);
        out
    }
    const OID: &[u8] = &[0x06, 0x03, 0x2b, 0x65, 0x6e];
    let alg_id = der_seq(OID);
    let bs = der_bit_string(raw_x);
    let mut body = Vec::new();
    body.extend_from_slice(&alg_id);
    body.extend_from_slice(&bs);
    der_seq(&body)
}

fn build_x25519_pkcs8(raw_d: &[u8; 32]) -> Vec<u8> {
    fn der_len(n: usize) -> Vec<u8> {
        if n < 128 {
            vec![n as u8]
        } else {
            vec![0x81, n as u8]
        }
    }
    fn der_seq(body: &[u8]) -> Vec<u8> {
        let mut out = vec![0x30];
        out.extend_from_slice(&der_len(body.len()));
        out.extend_from_slice(body);
        out
    }
    fn der_octet(payload: &[u8]) -> Vec<u8> {
        let mut out = vec![0x04];
        out.extend_from_slice(&der_len(payload.len()));
        out.extend_from_slice(payload);
        out
    }
    const OID: &[u8] = &[0x06, 0x03, 0x2b, 0x65, 0x6e];
    let version = vec![0x02, 0x01, 0x00];
    let alg_id = der_seq(OID);
    let inner = der_octet(raw_d);
    let outer = der_octet(&inner);
    let mut body = Vec::new();
    body.extend_from_slice(&version);
    body.extend_from_slice(&alg_id);
    body.extend_from_slice(&outer);
    der_seq(&body)
}

// ---------------------------------------------------------------------------
// Top-level callbacks
// ---------------------------------------------------------------------------

pub(crate) fn generate_key_sync_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    if args.length() < 1 {
        let exc = crate::node_error::build_node_exception(
            scope,
            "ERR_INVALID_ARG_TYPE",
            "generateKeySync requires a type",
        );
        scope.throw_exception(exc);
        return;
    }
    let kt = args.get(0).to_rust_string_lossy(scope);
    let options = if args.length() >= 2 {
        Some(args.get(1))
    } else {
        None
    };
    match generate_key_sync(scope, &kt, options) {
        Ok(v) => rv.set(v),
        Err(err) => {
            let exc = crate::web::crypto::helpers::op_error_to_v8(scope, err);
            scope.throw_exception(exc);
        }
    }
}

pub(crate) fn generate_key_pair_sync_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    if args.length() < 1 {
        let exc = crate::node_error::build_node_exception(
            scope,
            "ERR_INVALID_ARG_TYPE",
            "generateKeyPairSync requires a type",
        );
        scope.throw_exception(exc);
        return;
    }
    let kt = args.get(0).to_rust_string_lossy(scope);
    let options = if args.length() >= 2 {
        Some(args.get(1))
    } else {
        None
    };
    match generate_key_pair_sync(scope, &kt, options) {
        Ok(v) => rv.set(v),
        Err(err) => {
            let exc = crate::web::crypto::helpers::op_error_to_v8(scope, err);
            scope.throw_exception(exc);
        }
    }
}
