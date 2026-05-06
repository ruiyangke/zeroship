//! JsonWebKey import/export — RFC 7517 / RFC 7518.
//! See `docs/proposals/webcrypto-native.md` §VI.
//!
//! Each algorithm has `import_xxx` / `export_xxx` helpers that walk a
//! V8 object's JWK fields, validate, and produce / consume a
//! `CryptoKeyState`. base64url decode/encode lives in `helpers`.

#![allow(dead_code)]
#![allow(clippy::too_many_arguments)]

use super::crypto_key;
use super::helpers::{
    base64url_decode, base64url_encode, read_optional_bool, read_optional_string,
    read_optional_string_array, read_required_object,
};
use super::key_material::{
    AesKeyAlgorithm, CryptoKeyState, EcKeyAlgorithm, HashAlgo, HmacKeyAlgorithm, KeyAlgorithm,
    KeyMaterial, KeyType, KeyUsage, NamedCurve, RsaHashedKeyAlgorithm, RsaPrivateComponents,
    RsaPublicComponents,
};
use super::registry::AlgorithmName;
use crate::state::OpError;

// =============================================================================
// JsonWebKey struct + parser
// =============================================================================

#[derive(Debug, Clone, Default)]
pub struct JsonWebKey {
    pub kty: String,
    pub r#use: Option<String>,
    pub key_ops: Option<Vec<String>>,
    pub alg: Option<String>,
    pub ext: Option<bool>,
    // Symmetric
    pub k: Option<String>,
    // EC / OKP
    pub crv: Option<String>,
    pub x: Option<String>,
    pub y: Option<String>,
    pub d: Option<String>,
    // RSA
    pub n: Option<String>,
    pub e: Option<String>,
    pub p: Option<String>,
    pub q: Option<String>,
    pub dp: Option<String>,
    pub dq: Option<String>,
    pub qi: Option<String>,
    pub oth_count: usize,
}

pub fn parse_jwk(
    scope: &mut v8::PinScope,
    obj: v8::Local<v8::Object>,
) -> Result<JsonWebKey, OpError> {
    let mut jwk = JsonWebKey::default();
    jwk.kty = read_optional_string(scope, obj, "kty")?
        .ok_or_else(|| OpError::dom("DataError", "JWK 'kty' missing"))?;
    jwk.r#use = read_optional_string(scope, obj, "use")?;
    jwk.key_ops = read_optional_string_array(scope, obj, "key_ops")?;
    jwk.alg = read_optional_string(scope, obj, "alg")?;
    jwk.ext = read_optional_bool(scope, obj, "ext");
    jwk.k = read_optional_string(scope, obj, "k")?;
    jwk.crv = read_optional_string(scope, obj, "crv")?;
    jwk.x = read_optional_string(scope, obj, "x")?;
    jwk.y = read_optional_string(scope, obj, "y")?;
    jwk.d = read_optional_string(scope, obj, "d")?;
    jwk.n = read_optional_string(scope, obj, "n")?;
    jwk.e = read_optional_string(scope, obj, "e")?;
    jwk.p = read_optional_string(scope, obj, "p")?;
    jwk.q = read_optional_string(scope, obj, "q")?;
    jwk.dp = read_optional_string(scope, obj, "dp")?;
    jwk.dq = read_optional_string(scope, obj, "dq")?;
    jwk.qi = read_optional_string(scope, obj, "qi")?;
    // oth — sequence of {r,d,t}; we don't support multi-prime keys
    // (aws-lc-rs path doesn't expose the API). Reject if non-empty.
    let oth_key = v8::String::new(scope, "oth").unwrap();
    if let Some(v) = obj.get(scope, oth_key.into()) {
        if !v.is_undefined() && !v.is_null() {
            if let Ok(arr) = v8::Local::<v8::Array>::try_from(v) {
                jwk.oth_count = arr.length() as usize;
                if arr.length() > 0 {
                    return Err(OpError::dom(
                        "DataError",
                        "Multi-prime RSA (oth) not supported",
                    ));
                }
            }
        }
    }
    Ok(jwk)
}

fn read_jwk(
    scope: &mut v8::PinScope,
    value: v8::Local<v8::Value>,
) -> Result<JsonWebKey, OpError> {
    let obj = v8::Local::<v8::Object>::try_from(value)
        .map_err(|_| OpError::type_error("JWK keyData must be an object"))?;
    parse_jwk(scope, obj)
}

fn validate_key_ops(
    jwk: &JsonWebKey,
    usages: &[KeyUsage],
    extractable: bool,
) -> Result<(), OpError> {
    if let Some(ext) = jwk.ext {
        if extractable && !ext {
            return Err(OpError::dom(
                "DataError",
                "JWK 'ext' is false but extractable=true was requested",
            ));
        }
    }
    if let Some(ops) = &jwk.key_ops {
        for u in usages {
            if !ops.iter().any(|s| s == u.as_str()) {
                return Err(OpError::dom(
                    "DataError",
                    format!(
                        "JWK 'key_ops' missing requested usage '{}'",
                        u.as_str()
                    ),
                ));
            }
        }
    }
    Ok(())
}

// =============================================================================
// AES JWK
// =============================================================================

pub fn import_aes<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    alg: AlgorithmName,
    key_data: v8::Local<v8::Value>,
    extractable: bool,
    usages: &[KeyUsage],
) -> Result<v8::Local<'s, v8::Object>, OpError> {
    let jwk = read_jwk(scope, key_data)?;
    if jwk.kty != "oct" {
        return Err(OpError::dom("DataError", "AES JWK 'kty' must be 'oct'"));
    }
    let k = jwk
        .k
        .as_deref()
        .ok_or_else(|| OpError::dom("DataError", "AES JWK 'k' missing"))?;
    let bytes = base64url_decode(k)?;
    let length_bits = (bytes.len() as u32) * 8;
    if !matches!(length_bits, 128 | 192 | 256) {
        return Err(OpError::dom(
            "DataError",
            format!("AES JWK 'k' length {length_bits} not 128/192/256 bits"),
        ));
    }
    if let Some(jwk_alg) = &jwk.alg {
        let expected = expected_aes_alg(alg, length_bits);
        if jwk_alg != &expected {
            return Err(OpError::dom(
                "DataError",
                format!("AES JWK 'alg' {jwk_alg} does not match {expected}"),
            ));
        }
    }
    validate_key_ops(&jwk, usages, extractable)?;
    let state = CryptoKeyState {
        key_type: KeyType::Secret,
        extractable,
        algorithm: KeyAlgorithm::Aes(AesKeyAlgorithm {
            name: alg.canonical(),
            length: length_bits,
        }),
        usages: usages.to_vec(),
        material: KeyMaterial::Symmetric(bytes),
    };
    Ok(crypto_key::build(scope, state))
}

fn expected_aes_alg(alg: AlgorithmName, length_bits: u32) -> String {
    match (alg, length_bits) {
        (AlgorithmName::AesGcm, 128) => "A128GCM",
        (AlgorithmName::AesGcm, 192) => "A192GCM",
        (AlgorithmName::AesGcm, 256) => "A256GCM",
        (AlgorithmName::AesCbc, 128) => "A128CBC",
        (AlgorithmName::AesCbc, 192) => "A192CBC",
        (AlgorithmName::AesCbc, 256) => "A256CBC",
        (AlgorithmName::AesCtr, 128) => "A128CTR",
        (AlgorithmName::AesCtr, 192) => "A192CTR",
        (AlgorithmName::AesCtr, 256) => "A256CTR",
        (AlgorithmName::AesKw, 128) => "A128KW",
        (AlgorithmName::AesKw, 192) => "A192KW",
        (AlgorithmName::AesKw, 256) => "A256KW",
        _ => "",
    }
    .to_string()
}

pub fn export_aes<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    key: &CryptoKeyState,
    raw: &[u8],
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    let alg = match &key.algorithm {
        KeyAlgorithm::Aes(a) => a,
        _ => return Err(OpError::dom("OperationError", "Not an AES key")),
    };
    let alg_name = AlgorithmName::from_spec_name(alg.name).unwrap_or(AlgorithmName::AesGcm);
    let jwk_alg = expected_aes_alg(alg_name, alg.length);
    let obj = v8::Object::new(scope);
    set_str(scope, obj, "kty", "oct");
    set_str(scope, obj, "k", &base64url_encode(raw));
    if !jwk_alg.is_empty() {
        set_str(scope, obj, "alg", &jwk_alg);
    }
    write_key_ops(scope, obj, &key.usages);
    set_bool(scope, obj, "ext", key.extractable);
    Ok(obj.into())
}

// =============================================================================
// HMAC JWK
// =============================================================================

pub fn import_hmac<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    key_data: v8::Local<v8::Value>,
    hash: HashAlgo,
    extractable: bool,
    usages: &[KeyUsage],
) -> Result<v8::Local<'s, v8::Object>, OpError> {
    let jwk = read_jwk(scope, key_data)?;
    if jwk.kty != "oct" {
        return Err(OpError::dom("DataError", "HMAC JWK 'kty' must be 'oct'"));
    }
    let k = jwk
        .k
        .as_deref()
        .ok_or_else(|| OpError::dom("DataError", "HMAC JWK 'k' missing"))?;
    let bytes = base64url_decode(k)?;
    if let Some(jwk_alg) = &jwk.alg {
        let expected = match hash {
            HashAlgo::Sha1 => "HS1",
            HashAlgo::Sha256 => "HS256",
            HashAlgo::Sha384 => "HS384",
            HashAlgo::Sha512 => "HS512",
        };
        if jwk_alg != expected {
            return Err(OpError::dom(
                "DataError",
                format!("HMAC JWK 'alg' {jwk_alg} does not match {expected}"),
            ));
        }
    }
    validate_key_ops(&jwk, usages, extractable)?;
    let length_bits = (bytes.len() as u32) * 8;
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
    Ok(crypto_key::build(scope, state))
}

pub fn export_hmac<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    key: &CryptoKeyState,
    raw: &[u8],
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    let hash = match &key.algorithm {
        KeyAlgorithm::Hmac(h) => h.hash,
        _ => return Err(OpError::dom("OperationError", "Not an HMAC key")),
    };
    let alg = match hash {
        HashAlgo::Sha1 => "HS1",
        HashAlgo::Sha256 => "HS256",
        HashAlgo::Sha384 => "HS384",
        HashAlgo::Sha512 => "HS512",
    };
    let obj = v8::Object::new(scope);
    set_str(scope, obj, "kty", "oct");
    set_str(scope, obj, "k", &base64url_encode(raw));
    set_str(scope, obj, "alg", alg);
    write_key_ops(scope, obj, &key.usages);
    set_bool(scope, obj, "ext", key.extractable);
    Ok(obj.into())
}

// =============================================================================
// EC JWK (P-256 / P-384 / P-521 — ECDSA + ECDH)
// =============================================================================

pub fn import_ec<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    alg: AlgorithmName,
    curve: NamedCurve,
    key_data: v8::Local<v8::Value>,
    extractable: bool,
    usages: &[KeyUsage],
) -> Result<v8::Local<'s, v8::Object>, OpError> {
    let jwk = read_jwk(scope, key_data)?;
    if jwk.kty != "EC" {
        return Err(OpError::dom("DataError", "EC JWK 'kty' must be 'EC'"));
    }
    let crv = jwk
        .crv
        .as_deref()
        .ok_or_else(|| OpError::dom("DataError", "EC JWK 'crv' missing"))?;
    if crv != curve.as_str() {
        return Err(OpError::dom(
            "DataError",
            format!("EC JWK 'crv' {crv} does not match {}", curve.as_str()),
        ));
    }
    let x = jwk
        .x
        .as_deref()
        .ok_or_else(|| OpError::dom("DataError", "EC JWK 'x' missing"))?;
    let y = jwk
        .y
        .as_deref()
        .ok_or_else(|| OpError::dom("DataError", "EC JWK 'y' missing"))?;
    let x_bytes = base64url_decode(x)?;
    let y_bytes = base64url_decode(y)?;
    let n = curve.order_len();
    let mut raw_xy = Vec::with_capacity(1 + 2 * n);
    raw_xy.push(0x04);
    raw_xy.extend(left_pad(&x_bytes, n));
    raw_xy.extend(left_pad(&y_bytes, n));

    validate_key_ops(&jwk, usages, extractable)?;

    let state = if let Some(d) = jwk.d.as_deref() {
        let d_bytes = base64url_decode(d)?;
        let raw_d = left_pad(&d_bytes, n);
        // Build PKCS#8 from raw d. For aws-lc-rs ECDSA path we can
        // assemble a minimal SEC1 ECPrivateKey + PKCS8 wrapper; for v1
        // we lean on aws-lc-rs's `EcdsaKeyPair::from_private_key_and_public_key`
        // which would require extra plumbing. We document the gap and
        // store the raw_d so JWK round-trip works; sign/verify will
        // fail until the SPKI/PKCS8 builder is added — call out
        // explicitly.
        // Build a minimal PKCS#8 wrapper:
        let pkcs8 = build_ec_pkcs8(curve, &raw_d, &raw_xy)?;
        CryptoKeyState {
            key_type: KeyType::Private,
            extractable,
            algorithm: KeyAlgorithm::Ec(EcKeyAlgorithm {
                name: alg.canonical(),
                named_curve: curve,
            }),
            usages: usages.to_vec(),
            material: KeyMaterial::EcPrivate {
                pkcs8_der: pkcs8,
                raw_d,
                raw_xy,
            },
        }
    } else {
        CryptoKeyState {
            key_type: KeyType::Public,
            extractable,
            algorithm: KeyAlgorithm::Ec(EcKeyAlgorithm {
                name: alg.canonical(),
                named_curve: curve,
            }),
            usages: usages.to_vec(),
            material: KeyMaterial::EcPublic {
                spki_der: Vec::new(),
                raw_xy,
            },
        }
    };
    Ok(crypto_key::build(scope, state))
}

pub fn export_ec<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    key: &CryptoKeyState,
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    let ec = match &key.algorithm {
        KeyAlgorithm::Ec(e) => e,
        _ => return Err(OpError::dom("OperationError", "Not an EC key")),
    };
    let raw_xy = match &key.material {
        KeyMaterial::EcPublic { raw_xy, .. } => raw_xy,
        KeyMaterial::EcPrivate { raw_xy, .. } => raw_xy,
        _ => return Err(OpError::dom("OperationError", "EC missing material")),
    };
    let n = ec.named_curve.order_len();
    if raw_xy.len() != 1 + 2 * n || raw_xy[0] != 0x04 {
        return Err(OpError::dom(
            "OperationError",
            "EC public point malformed",
        ));
    }
    let x_part = &raw_xy[1..1 + n];
    let y_part = &raw_xy[1 + n..];
    let obj = v8::Object::new(scope);
    set_str(scope, obj, "kty", "EC");
    set_str(scope, obj, "crv", ec.named_curve.as_str());
    set_str(scope, obj, "x", &base64url_encode(x_part));
    set_str(scope, obj, "y", &base64url_encode(y_part));
    if let KeyMaterial::EcPrivate { raw_d, pkcs8_der, .. } = &key.material {
        // Generated keys store an empty raw_d (aws-lc-rs hides the
        // scalar). Walk the PKCS#8 lazily to recover it. JWK-imported
        // keys already have raw_d populated.
        let scalar = if !raw_d.is_empty() {
            raw_d.clone()
        } else {
            super::der::extract_ec_raw_d(pkcs8_der, n).ok_or_else(|| {
                OpError::dom(
                    "OperationError",
                    "EC private-key DER walk failed (scalar unrecoverable)",
                )
            })?
        };
        set_str(scope, obj, "d", &base64url_encode(&scalar));
    }
    write_key_ops(scope, obj, &key.usages);
    set_bool(scope, obj, "ext", key.extractable);
    Ok(obj.into())
}

fn build_ec_pkcs8(
    curve: NamedCurve,
    raw_d: &[u8],
    raw_xy: &[u8],
) -> Result<Vec<u8>, OpError> {
    // Hand-rolled DER. SEC1 ECPrivateKey:
    //   SEQUENCE {
    //     version INTEGER (1),
    //     privateKey OCTET STRING,
    //     parameters [0] OBJECT IDENTIFIER (curve),
    //     publicKey  [1] BIT STRING (uncompressed point)
    //   }
    // Wrapped in PKCS8 PrivateKeyInfo:
    //   SEQUENCE {
    //     version INTEGER (0),
    //     privateKeyAlgorithm AlgorithmIdentifier,
    //     privateKey OCTET STRING (encoding of SEC1 ECPrivateKey)
    //   }
    let oid_curve: &[u8] = match curve {
        NamedCurve::P256 => &[0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07], // 1.2.840.10045.3.1.7
        NamedCurve::P384 => &[0x06, 0x05, 0x2b, 0x81, 0x04, 0x00, 0x22], // 1.3.132.0.34
        NamedCurve::P521 => &[0x06, 0x05, 0x2b, 0x81, 0x04, 0x00, 0x23], // 1.3.132.0.35
    };
    // ecPublicKey OID 1.2.840.10045.2.1
    let oid_ec_public: &[u8] = &[0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01];
    // Build SEC1 ECPrivateKey inner.
    let mut sec1 = Vec::new();
    sec1.extend_from_slice(&[0x02, 0x01, 0x01]); // version INTEGER 1
    sec1.push(0x04); // OCTET STRING
    sec1.extend_from_slice(&der_len(raw_d.len()));
    sec1.extend_from_slice(raw_d);
    // [0] parameters (explicit context tag) wrapping curve OID
    let mut params = Vec::new();
    params.extend_from_slice(oid_curve);
    sec1.push(0xa0);
    sec1.extend_from_slice(&der_len(params.len()));
    sec1.extend_from_slice(&params);
    // [1] publicKey BIT STRING (unused-bits=0 prefix, then point)
    let mut bit_string = vec![0u8]; // unused-bits
    bit_string.extend_from_slice(raw_xy);
    sec1.push(0xa1);
    let mut bs_inner = Vec::new();
    bs_inner.push(0x03); // BIT STRING tag
    bs_inner.extend_from_slice(&der_len(bit_string.len()));
    bs_inner.extend_from_slice(&bit_string);
    sec1.extend_from_slice(&der_len(bs_inner.len()));
    sec1.extend_from_slice(&bs_inner);
    let mut sec1_seq = Vec::new();
    sec1_seq.push(0x30);
    sec1_seq.extend_from_slice(&der_len(sec1.len()));
    sec1_seq.extend_from_slice(&sec1);

    // PrivateKeyInfo wrapping sec1.
    let mut alg_id = Vec::new();
    alg_id.extend_from_slice(oid_ec_public);
    alg_id.extend_from_slice(oid_curve);
    let mut alg_id_seq = Vec::new();
    alg_id_seq.push(0x30);
    alg_id_seq.extend_from_slice(&der_len(alg_id.len()));
    alg_id_seq.extend_from_slice(&alg_id);

    let mut priv_key_oct = Vec::new();
    priv_key_oct.push(0x04);
    priv_key_oct.extend_from_slice(&der_len(sec1_seq.len()));
    priv_key_oct.extend_from_slice(&sec1_seq);

    let mut body = Vec::new();
    body.extend_from_slice(&[0x02, 0x01, 0x00]); // version 0
    body.extend_from_slice(&alg_id_seq);
    body.extend_from_slice(&priv_key_oct);

    let mut out = Vec::with_capacity(body.len() + 6);
    out.push(0x30);
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

fn left_pad(src: &[u8], n: usize) -> Vec<u8> {
    if src.len() >= n {
        src[src.len() - n..].to_vec()
    } else {
        let mut out = vec![0u8; n];
        out[n - src.len()..].copy_from_slice(src);
        out
    }
}

// =============================================================================
// RSA JWK
// =============================================================================

pub fn import_rsa<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    alg: AlgorithmName,
    hash: HashAlgo,
    key_data: v8::Local<v8::Value>,
    extractable: bool,
    usages: &[KeyUsage],
) -> Result<v8::Local<'s, v8::Object>, OpError> {
    let jwk = read_jwk(scope, key_data)?;
    if jwk.kty != "RSA" {
        return Err(OpError::dom("DataError", "RSA JWK 'kty' must be 'RSA'"));
    }
    let n = jwk
        .n
        .as_deref()
        .ok_or_else(|| OpError::dom("DataError", "RSA JWK 'n' missing"))?;
    let e = jwk
        .e
        .as_deref()
        .ok_or_else(|| OpError::dom("DataError", "RSA JWK 'e' missing"))?;
    let n_bytes = base64url_decode(n)?;
    let e_bytes = base64url_decode(e)?;
    if let Some(jwk_alg) = &jwk.alg {
        let expected = expected_rsa_alg(alg, hash);
        if jwk_alg != &expected {
            return Err(OpError::dom(
                "DataError",
                format!("RSA JWK 'alg' {jwk_alg} does not match {expected}"),
            ));
        }
    }
    validate_key_ops(&jwk, usages, extractable)?;

    let modulus_length = (n_bytes.len() as u32) * 8;

    if jwk.d.is_some() {
        // Private key — need d/p/q/dp/dq/qi all present.
        let d = jwk
            .d
            .as_deref()
            .ok_or_else(|| OpError::dom("DataError", "RSA private JWK missing 'd'"))?;
        let p = jwk
            .p
            .as_deref()
            .ok_or_else(|| OpError::dom("DataError", "RSA private JWK missing 'p'"))?;
        let q = jwk
            .q
            .as_deref()
            .ok_or_else(|| OpError::dom("DataError", "RSA private JWK missing 'q'"))?;
        let dp = jwk
            .dp
            .as_deref()
            .ok_or_else(|| OpError::dom("DataError", "RSA private JWK missing 'dp'"))?;
        let dq = jwk
            .dq
            .as_deref()
            .ok_or_else(|| OpError::dom("DataError", "RSA private JWK missing 'dq'"))?;
        let qi = jwk
            .qi
            .as_deref()
            .ok_or_else(|| OpError::dom("DataError", "RSA private JWK missing 'qi'"))?;
        let comps = RsaPrivateComponents {
            n: n_bytes.clone(),
            e: e_bytes.clone(),
            d: base64url_decode(d)?,
            p: base64url_decode(p)?,
            q: base64url_decode(q)?,
            dp: base64url_decode(dp)?,
            dq: base64url_decode(dq)?,
            qi: base64url_decode(qi)?,
        };
        let pkcs8 = build_rsa_pkcs8(&comps)?;
        let state = CryptoKeyState {
            key_type: KeyType::Private,
            extractable,
            algorithm: KeyAlgorithm::RsaHashed(RsaHashedKeyAlgorithm {
                name: alg.canonical(),
                modulus_length,
                public_exponent: e_bytes,
                hash,
            }),
            usages: usages.to_vec(),
            material: KeyMaterial::RsaPrivate {
                pkcs8_der: pkcs8,
                components: comps,
            },
        };
        Ok(crypto_key::build(scope, state))
    } else {
        let comps = RsaPublicComponents {
            n: n_bytes.clone(),
            e: e_bytes.clone(),
        };
        let spki = build_rsa_spki(&comps)?;
        let state = CryptoKeyState {
            key_type: KeyType::Public,
            extractable,
            algorithm: KeyAlgorithm::RsaHashed(RsaHashedKeyAlgorithm {
                name: alg.canonical(),
                modulus_length,
                public_exponent: e_bytes,
                hash,
            }),
            usages: usages.to_vec(),
            material: KeyMaterial::RsaPublic {
                spki_der: spki,
                components: comps,
            },
        };
        Ok(crypto_key::build(scope, state))
    }
}

fn expected_rsa_alg(alg: AlgorithmName, hash: HashAlgo) -> String {
    match (alg, hash) {
        (AlgorithmName::RsassaPkcs1v15, HashAlgo::Sha1) => "RS1",
        (AlgorithmName::RsassaPkcs1v15, HashAlgo::Sha256) => "RS256",
        (AlgorithmName::RsassaPkcs1v15, HashAlgo::Sha384) => "RS384",
        (AlgorithmName::RsassaPkcs1v15, HashAlgo::Sha512) => "RS512",
        (AlgorithmName::RsaPss, HashAlgo::Sha256) => "PS256",
        (AlgorithmName::RsaPss, HashAlgo::Sha384) => "PS384",
        (AlgorithmName::RsaPss, HashAlgo::Sha512) => "PS512",
        (AlgorithmName::RsaOaep, HashAlgo::Sha1) => "RSA-OAEP",
        (AlgorithmName::RsaOaep, HashAlgo::Sha256) => "RSA-OAEP-256",
        (AlgorithmName::RsaOaep, HashAlgo::Sha384) => "RSA-OAEP-384",
        (AlgorithmName::RsaOaep, HashAlgo::Sha512) => "RSA-OAEP-512",
        _ => "",
    }
    .to_string()
}

pub fn export_rsa<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    key: &CryptoKeyState,
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    let r = match &key.algorithm {
        KeyAlgorithm::RsaHashed(r) => r,
        _ => return Err(OpError::dom("OperationError", "Not an RSA key")),
    };
    let alg = AlgorithmName::from_spec_name(r.name).unwrap_or(AlgorithmName::RsaOaep);
    let jwk_alg = expected_rsa_alg(alg, r.hash);
    let obj = v8::Object::new(scope);
    set_str(scope, obj, "kty", "RSA");
    if !jwk_alg.is_empty() {
        set_str(scope, obj, "alg", &jwk_alg);
    }
    match &key.material {
        KeyMaterial::RsaPublic { components, .. } => {
            if components.n.is_empty() {
                return Err(OpError::dom(
                    "OperationError",
                    "RSA JWK export requires component data (only stored on JWK-imported keys)",
                ));
            }
            set_str(scope, obj, "n", &base64url_encode(&components.n));
            set_str(scope, obj, "e", &base64url_encode(&components.e));
        }
        KeyMaterial::RsaPrivate { components, .. } => {
            if components.n.is_empty() {
                return Err(OpError::dom(
                    "OperationError",
                    "RSA JWK export requires component data (only stored on JWK-imported keys)",
                ));
            }
            set_str(scope, obj, "n", &base64url_encode(&components.n));
            set_str(scope, obj, "e", &base64url_encode(&components.e));
            set_str(scope, obj, "d", &base64url_encode(&components.d));
            set_str(scope, obj, "p", &base64url_encode(&components.p));
            set_str(scope, obj, "q", &base64url_encode(&components.q));
            set_str(scope, obj, "dp", &base64url_encode(&components.dp));
            set_str(scope, obj, "dq", &base64url_encode(&components.dq));
            set_str(scope, obj, "qi", &base64url_encode(&components.qi));
        }
        _ => {
            return Err(OpError::dom(
                "OperationError",
                "RSA: missing key material",
            ));
        }
    }
    write_key_ops(scope, obj, &key.usages);
    set_bool(scope, obj, "ext", key.extractable);
    Ok(obj.into())
}

/// Build a minimal SPKI for an RSA public key.
fn build_rsa_spki(c: &RsaPublicComponents) -> Result<Vec<u8>, OpError> {
    // RSA public key DER: SEQUENCE { n INTEGER, e INTEGER }
    let mut rsa_pub = Vec::new();
    rsa_pub.extend_from_slice(&der_integer(&c.n));
    rsa_pub.extend_from_slice(&der_integer(&c.e));
    let mut rsa_pub_seq = Vec::new();
    rsa_pub_seq.push(0x30);
    rsa_pub_seq.extend_from_slice(&der_len(rsa_pub.len()));
    rsa_pub_seq.extend_from_slice(&rsa_pub);

    // BIT STRING wrapper (unused-bits=0 prefix + content).
    let mut bs = vec![0u8];
    bs.extend_from_slice(&rsa_pub_seq);
    let mut bit_string = vec![0x03];
    bit_string.extend_from_slice(&der_len(bs.len()));
    bit_string.extend_from_slice(&bs);

    // AlgorithmIdentifier { rsaEncryption (1.2.840.113549.1.1.1), NULL }.
    // OID DER: 06 09 2a 86 48 86 f7 0d 01 01 01.
    let alg_id: &[u8] = &[
        0x30, 0x0d, 0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01, 0x05, 0x00,
    ];
    let mut spki_inner = Vec::new();
    spki_inner.extend_from_slice(alg_id);
    spki_inner.extend_from_slice(&bit_string);
    let mut out = vec![0x30];
    out.extend_from_slice(&der_len(spki_inner.len()));
    out.extend_from_slice(&spki_inner);
    Ok(out)
}

/// Build a minimal PKCS#8 wrapper around an RSAPrivateKey DER.
fn build_rsa_pkcs8(c: &RsaPrivateComponents) -> Result<Vec<u8>, OpError> {
    // RSAPrivateKey DER per PKCS#1:
    //   SEQUENCE { version=0, n, e, d, p, q, dp, dq, qi }
    let mut inner = Vec::new();
    inner.extend_from_slice(&[0x02, 0x01, 0x00]); // version 0
    inner.extend_from_slice(&der_integer(&c.n));
    inner.extend_from_slice(&der_integer(&c.e));
    inner.extend_from_slice(&der_integer(&c.d));
    inner.extend_from_slice(&der_integer(&c.p));
    inner.extend_from_slice(&der_integer(&c.q));
    inner.extend_from_slice(&der_integer(&c.dp));
    inner.extend_from_slice(&der_integer(&c.dq));
    inner.extend_from_slice(&der_integer(&c.qi));
    let mut rsa_priv = vec![0x30];
    rsa_priv.extend_from_slice(&der_len(inner.len()));
    rsa_priv.extend_from_slice(&inner);

    // PrivateKeyInfo: SEQUENCE { version=0, AlgId(rsaEncryption,NULL), OCTET STRING(rsa_priv) }
    let alg_id: &[u8] = &[
        0x30, 0x0d, 0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01, 0x05, 0x00,
    ];
    let mut priv_oct = vec![0x04];
    priv_oct.extend_from_slice(&der_len(rsa_priv.len()));
    priv_oct.extend_from_slice(&rsa_priv);

    let mut body = Vec::new();
    body.extend_from_slice(&[0x02, 0x01, 0x00]);
    body.extend_from_slice(alg_id);
    body.extend_from_slice(&priv_oct);
    let mut out = vec![0x30];
    out.extend_from_slice(&der_len(body.len()));
    out.extend_from_slice(&body);
    Ok(out)
}

fn der_integer(bytes: &[u8]) -> Vec<u8> {
    // INTEGER tag, length prefix; insert leading 0x00 byte if high
    // bit is set (unsigned BigInt convention).
    let needs_pad = !bytes.is_empty() && bytes[0] & 0x80 != 0;
    let len = bytes.len() + if needs_pad { 1 } else { 0 };
    let mut out = Vec::with_capacity(2 + len);
    out.push(0x02);
    out.extend_from_slice(&der_len(len));
    if needs_pad {
        out.push(0x00);
    }
    out.extend_from_slice(bytes);
    out
}

// =============================================================================
// Ed25519 / X25519 JWK (RFC 8037)
// =============================================================================

pub fn import_ed25519<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    key_data: v8::Local<v8::Value>,
    extractable: bool,
    usages: &[KeyUsage],
) -> Result<v8::Local<'s, v8::Object>, OpError> {
    let jwk = read_jwk(scope, key_data)?;
    if jwk.kty != "OKP" {
        return Err(OpError::dom("DataError", "Ed25519 JWK 'kty' must be 'OKP'"));
    }
    if jwk.crv.as_deref() != Some("Ed25519") {
        return Err(OpError::dom(
            "DataError",
            "Ed25519 JWK 'crv' must be 'Ed25519'",
        ));
    }
    let x = jwk
        .x
        .as_deref()
        .ok_or_else(|| OpError::dom("DataError", "Ed25519 JWK 'x' missing"))?;
    let x_bytes = base64url_decode(x)?;
    if x_bytes.len() != 32 {
        return Err(OpError::dom("DataError", "Ed25519 'x' must be 32 bytes"));
    }
    let mut raw_x = [0u8; 32];
    raw_x.copy_from_slice(&x_bytes);
    validate_key_ops(&jwk, usages, extractable)?;
    let state = if let Some(d) = jwk.d.as_deref() {
        let d_bytes = base64url_decode(d)?;
        if d_bytes.len() != 32 {
            return Err(OpError::dom(
                "DataError",
                "Ed25519 'd' must be 32 bytes",
            ));
        }
        let mut raw_d = [0u8; 32];
        raw_d.copy_from_slice(&d_bytes);
        let pkcs8 = build_ed25519_pkcs8(&raw_d);
        CryptoKeyState {
            key_type: KeyType::Private,
            extractable,
            algorithm: KeyAlgorithm::Ed25519,
            usages: usages.to_vec(),
            material: KeyMaterial::Ed25519Private {
                pkcs8_der: pkcs8,
                raw_d,
                raw_x,
            },
        }
    } else {
        CryptoKeyState {
            key_type: KeyType::Public,
            extractable,
            algorithm: KeyAlgorithm::Ed25519,
            usages: usages.to_vec(),
            material: KeyMaterial::Ed25519Public {
                spki_der: Vec::new(),
                raw_x,
            },
        }
    };
    Ok(crypto_key::build(scope, state))
}

fn build_ed25519_pkcs8(seed: &[u8; 32]) -> Vec<u8> {
    // Same shape as X25519 PKCS#8 but with OID 1.3.101.112 (Ed25519).
    let mut out = Vec::with_capacity(48);
    out.extend_from_slice(&[0x30, 0x2e]);
    out.extend_from_slice(&[0x02, 0x01, 0x00]);
    out.extend_from_slice(&[
        0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, // OID Ed25519
    ]);
    out.extend_from_slice(&[0x04, 0x22]);
    out.extend_from_slice(&[0x04, 0x20]);
    out.extend_from_slice(seed);
    out
}

pub fn export_ed25519<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    key: &CryptoKeyState,
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    let obj = v8::Object::new(scope);
    set_str(scope, obj, "kty", "OKP");
    set_str(scope, obj, "crv", "Ed25519");
    // RFC 8037 §2 + WebCrypto §35: include `alg: "Ed25519"`. (X25519
    // does NOT have an alg per RFC 8037 §5.)
    set_str(scope, obj, "alg", "Ed25519");
    match &key.material {
        KeyMaterial::Ed25519Public { raw_x, .. } => {
            set_str(scope, obj, "x", &base64url_encode(raw_x));
        }
        KeyMaterial::Ed25519Private { raw_x, raw_d, pkcs8_der } => {
            set_str(scope, obj, "x", &base64url_encode(raw_x));
            // Generated keys: aws-lc-rs hides the seed and we stored
            // [0; 32] as a placeholder. Walk the PKCS#8 (RFC 8410) to
            // recover the actual seed for JWK export.
            let seed = if raw_d.iter().any(|&b| b != 0) {
                *raw_d
            } else {
                super::der::extract_cfrg_raw_seed(pkcs8_der).ok_or_else(|| {
                    OpError::dom(
                        "OperationError",
                        "Ed25519 PKCS#8 walk failed (seed unrecoverable)",
                    )
                })?
            };
            set_str(scope, obj, "d", &base64url_encode(&seed));
        }
        _ => return Err(OpError::dom("OperationError", "Not Ed25519")),
    }
    write_key_ops(scope, obj, &key.usages);
    set_bool(scope, obj, "ext", key.extractable);
    Ok(obj.into())
}

pub fn import_x25519<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    key_data: v8::Local<v8::Value>,
    extractable: bool,
    usages: &[KeyUsage],
) -> Result<v8::Local<'s, v8::Object>, OpError> {
    let jwk = read_jwk(scope, key_data)?;
    if jwk.kty != "OKP" {
        return Err(OpError::dom("DataError", "X25519 JWK 'kty' must be 'OKP'"));
    }
    if jwk.crv.as_deref() != Some("X25519") {
        return Err(OpError::dom("DataError", "X25519 JWK 'crv' must be 'X25519'"));
    }
    let x = jwk
        .x
        .as_deref()
        .ok_or_else(|| OpError::dom("DataError", "X25519 JWK 'x' missing"))?;
    let x_bytes = base64url_decode(x)?;
    if x_bytes.len() != 32 {
        return Err(OpError::dom("DataError", "X25519 'x' must be 32 bytes"));
    }
    let mut raw_x = [0u8; 32];
    raw_x.copy_from_slice(&x_bytes);
    validate_key_ops(&jwk, usages, extractable)?;
    let state = if let Some(d) = jwk.d.as_deref() {
        let d_bytes = base64url_decode(d)?;
        if d_bytes.len() != 32 {
            return Err(OpError::dom("DataError", "X25519 'd' must be 32 bytes"));
        }
        let mut raw_d = [0u8; 32];
        raw_d.copy_from_slice(&d_bytes);
        // Build PKCS#8 for X25519 from seed.
        let pkcs8 = {
            let mut out = Vec::with_capacity(48);
            out.extend_from_slice(&[0x30, 0x2e]);
            out.extend_from_slice(&[0x02, 0x01, 0x00]);
            out.extend_from_slice(&[0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x6e]);
            out.extend_from_slice(&[0x04, 0x22, 0x04, 0x20]);
            out.extend_from_slice(&raw_d);
            out
        };
        CryptoKeyState {
            key_type: KeyType::Private,
            extractable,
            algorithm: KeyAlgorithm::X25519,
            usages: usages.to_vec(),
            material: KeyMaterial::X25519Private {
                pkcs8_der: pkcs8,
                raw_d,
                raw_x,
            },
        }
    } else {
        CryptoKeyState {
            key_type: KeyType::Public,
            extractable,
            algorithm: KeyAlgorithm::X25519,
            usages: usages.to_vec(),
            material: KeyMaterial::X25519Public {
                spki_der: Vec::new(),
                raw_x,
            },
        }
    };
    Ok(crypto_key::build(scope, state))
}

pub fn export_x25519<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    key: &CryptoKeyState,
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    let obj = v8::Object::new(scope);
    set_str(scope, obj, "kty", "OKP");
    set_str(scope, obj, "crv", "X25519");
    match &key.material {
        KeyMaterial::X25519Public { raw_x, .. } => {
            set_str(scope, obj, "x", &base64url_encode(raw_x));
        }
        KeyMaterial::X25519Private { raw_x, raw_d, .. } => {
            set_str(scope, obj, "x", &base64url_encode(raw_x));
            set_str(scope, obj, "d", &base64url_encode(raw_d));
        }
        _ => return Err(OpError::dom("OperationError", "Not X25519")),
    }
    write_key_ops(scope, obj, &key.usages);
    set_bool(scope, obj, "ext", key.extractable);
    Ok(obj.into())
}

// =============================================================================
// Common JS helpers
// =============================================================================

fn set_str<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    obj: v8::Local<v8::Object>,
    name: &str,
    value: &str,
) {
    let k = v8::String::new(scope, name).unwrap();
    let v = v8::String::new(scope, value).unwrap();
    obj.set(scope, k.into(), v.into());
}

fn set_bool<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    obj: v8::Local<v8::Object>,
    name: &str,
    value: bool,
) {
    let k = v8::String::new(scope, name).unwrap();
    let v = v8::Boolean::new(scope, value);
    obj.set(scope, k.into(), v.into());
}

fn write_key_ops<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    obj: v8::Local<v8::Object>,
    usages: &[KeyUsage],
) {
    let arr = v8::Array::new(scope, usages.len() as i32);
    for (i, u) in usages.iter().enumerate() {
        let s = v8::String::new(scope, u.as_str()).unwrap();
        arr.set_index(scope, i as u32, s.into());
    }
    let k = v8::String::new(scope, "key_ops").unwrap();
    obj.set(scope, k.into(), arr.into());
}
