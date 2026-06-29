//! `Sign` / `Verify` classes + one-shot `crypto.sign` / `crypto.verify`
//! + `crypto.publicEncrypt` / `crypto.privateDecrypt` (RSA-OAEP).
//!
//! See `docs/proposals/node-crypto-native.md` §V.5 and §II.5.
//!
//! Architecture: `update()` buffers the message bytes (the existing
//! `evp_ffi` helpers expect raw data and run the digest internally via
//! `EVP_DigestSignUpdate`/`EVP_DigestVerifyUpdate`). On `sign(privateKey)`
//! / `verify(publicKey, ...)` we feed the buffered bytes into the
//! Rust-side EVP wrappers shipped for the WebCrypto surface.
//!
//! ECDSA signature wire-format note: Node returns DER-encoded ECDSA
//! sigs (`SEQUENCE { INTEGER r, INTEGER s }`) by default. The existing
//! `evp_ffi::ecdsa_sign` post-processes the EVP DER output into raw
//! r||s for WebCrypto — for Node we use the kernel-level helper that
//! preserves DER (defined below).

#![allow(unsafe_code)]

use crate::node::buffer;
use super::key_object::{self};
use crate::web::crypto::crypto_key;
use crate::web::crypto::key_material::{
    HashAlgo, KeyMaterial, KeyType,
};
use crate::state::OpError;

#[allow(unused_imports)]
use zeroship_runtime_macros::{v8_class, v8_constructor, v8_method, v8_name, v8_to_string_tag};

// ---------------------------------------------------------------------------
// Sign class
// ---------------------------------------------------------------------------

pub struct Sign {
    /// Buffered message bytes — fed to EVP_DigestSignUpdate at sign() time.
    buf: Vec<u8>,
    hash_name: String,
    finalised: bool,
}

#[v8_class]
#[v8_to_string_tag = "Sign"]
impl Sign {
    #[v8_constructor]
    fn new() -> Result<Sign, OpError> {
        Err(OpError::type_error(
            "Sign is not a constructor — use crypto.createSign(name)",
        ))
    }

    #[v8_method]
    fn update(
        &mut self,
        scope: &mut v8::PinScope,
        data: v8::Local<v8::Value>,
        encoding: Option<String>,
    ) -> Result<(), OpError> {
        if self.finalised {
            return Err(OpError::error("Sign.update called after sign()"));
        }
        let bytes = buffer::extract_input(scope, data, encoding.as_deref())?;
        self.buf.extend_from_slice(&bytes);
        Ok(())
    }

    #[v8_method]
    fn sign<'s>(
        &mut self,
        scope: &mut v8::PinScope<'s, '_>,
        private_key: v8::Local<v8::Value>,
        encoding: Option<String>,
    ) -> Result<v8::Local<'s, v8::Value>, OpError> {
        if self.finalised {
            return Err(OpError::error("Sign.sign already called"));
        }
        self.finalised = true;
        let SignKeyInput {
            material,
            key_type,
            padding,
            salt_length,
        } = parse_sign_key_input(scope, private_key)?;
        if key_type != KeyType::Private {
            return Err(OpError::node(
                "ERR_CRYPTO_INCOMPATIBLE_KEY_OPTIONS",
                "Sign requires a private key",
            ));
        }
        let hash = parse_hash_name(&self.hash_name)?;
        let sig = sign_with_material(&material, hash, &self.buf, padding, salt_length)?;
        buffer::emit_output(scope, &sig, encoding.as_deref())
    }
}

// ---------------------------------------------------------------------------
// Verify class
// ---------------------------------------------------------------------------

pub struct Verify {
    buf: Vec<u8>,
    hash_name: String,
    finalised: bool,
}

#[v8_class]
#[v8_to_string_tag = "Verify"]
impl Verify {
    #[v8_constructor]
    fn new() -> Result<Verify, OpError> {
        Err(OpError::type_error(
            "Verify is not a constructor — use crypto.createVerify(name)",
        ))
    }

    #[v8_method]
    fn update(
        &mut self,
        scope: &mut v8::PinScope,
        data: v8::Local<v8::Value>,
        encoding: Option<String>,
    ) -> Result<(), OpError> {
        if self.finalised {
            return Err(OpError::error("Verify.update called after verify()"));
        }
        let bytes = buffer::extract_input(scope, data, encoding.as_deref())?;
        self.buf.extend_from_slice(&bytes);
        Ok(())
    }

    #[v8_method]
    fn verify(
        &mut self,
        scope: &mut v8::PinScope,
        public_key: v8::Local<v8::Value>,
        signature: v8::Local<v8::Value>,
        encoding: Option<String>,
    ) -> Result<bool, OpError> {
        if self.finalised {
            return Err(OpError::error("Verify.verify already called"));
        }
        self.finalised = true;
        let sig_bytes = buffer::extract_input(scope, signature, encoding.as_deref())?;
        let SignKeyInput {
            material,
            padding,
            salt_length,
            ..
        } = parse_sign_key_input(scope, public_key)?;
        let hash = parse_hash_name(&self.hash_name)?;
        verify_with_material(&material, hash, &self.buf, &sig_bytes, padding, salt_length)
    }
}

// ---------------------------------------------------------------------------
// Factories
// ---------------------------------------------------------------------------

pub fn create_sign<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    name: &str,
    _options: Option<v8::Local<v8::Value>>,
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    let _ = parse_hash_name(name)?;
    let s = Sign {
        buf: Vec::new(),
        hash_name: name.to_string(),
        finalised: false,
    };
    Ok(build_sign(scope, s).into())
}

pub fn create_verify<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    name: &str,
    _options: Option<v8::Local<v8::Value>>,
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    let _ = parse_hash_name(name)?;
    let v = Verify {
        buf: Vec::new(),
        hash_name: name.to_string(),
        finalised: false,
    };
    Ok(build_verify(scope, v).into())
}

fn parse_hash_name(name: &str) -> Result<HashAlgo, OpError> {
    let lower = name.to_ascii_lowercase();
    let canonical = match lower.as_str() {
        "sha1" | "sha-1" | "rsa-sha1" => "SHA-1",
        "sha256" | "sha-256" | "rsa-sha256" => "SHA-256",
        "sha384" | "sha-384" | "rsa-sha384" => "SHA-384",
        "sha512" | "sha-512" | "rsa-sha512" => "SHA-512",
        _ => {
            return Err(OpError::node(
                "ERR_OSSL_EVP_INVALID_DIGEST",
                format!("Unsupported digest: {name}"),
            ));
        }
    };
    HashAlgo::from_str(canonical).ok_or_else(|| {
        OpError::node(
            "ERR_OSSL_EVP_INVALID_DIGEST",
            format!("Unsupported digest: {name}"),
        )
    })
}

fn build_sign<'s>(scope: &mut v8::PinScope<'s, '_>, sign: Sign) -> v8::Local<'s, v8::Object> {
    let tmpl = Sign::install(scope);
    let inst_tmpl = tmpl.instance_template(scope);
    let inst = inst_tmpl.new_instance(scope).expect("Sign instance");
    let class_fn = tmpl.get_function(scope).unwrap();
    let proto_key = v8::String::new(scope, "prototype").unwrap();
    let proto_v = class_fn.get(scope, proto_key.into()).unwrap();
    inst.set_prototype(scope, proto_v);
    let boxed: Box<Sign> = Box::new(sign);
    let raw = Box::into_raw(boxed);
    let raw_addr = raw as usize;
    let ext = v8::External::new(scope, raw as *mut std::ffi::c_void);
    inst.set_internal_field(0, ext.into());
    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        inst,
        Box::new(move || unsafe {
            drop(Box::from_raw(raw_addr as *mut Sign));
        }),
    );
    std::mem::forget(weak);
    inst
}

fn build_verify<'s>(scope: &mut v8::PinScope<'s, '_>, verify: Verify) -> v8::Local<'s, v8::Object> {
    let tmpl = Verify::install(scope);
    let inst_tmpl = tmpl.instance_template(scope);
    let inst = inst_tmpl.new_instance(scope).expect("Verify instance");
    let class_fn = tmpl.get_function(scope).unwrap();
    let proto_key = v8::String::new(scope, "prototype").unwrap();
    let proto_v = class_fn.get(scope, proto_key.into()).unwrap();
    inst.set_prototype(scope, proto_v);
    let boxed: Box<Verify> = Box::new(verify);
    let raw = Box::into_raw(boxed);
    let raw_addr = raw as usize;
    let ext = v8::External::new(scope, raw as *mut std::ffi::c_void);
    inst.set_internal_field(0, ext.into());
    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        inst,
        Box::new(move || unsafe {
            drop(Box::from_raw(raw_addr as *mut Verify));
        }),
    );
    std::mem::forget(weak);
    inst
}

// ---------------------------------------------------------------------------
// Sign/Verify key input parser
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignPadding {
    Default,
    RsaPkcs1v15,
    RsaPss,
}

pub struct SignKeyInput {
    pub material: KeyMaterial,
    pub key_type: KeyType,
    pub padding: SignPadding,
    pub salt_length: i32,
}

pub fn parse_sign_key_input(
    scope: &mut v8::PinScope,
    input: v8::Local<v8::Value>,
) -> Result<SignKeyInput, OpError> {
    if let Some(state) = key_object::downcast_state(scope, input) {
        return Ok(SignKeyInput {
            material: state.material.clone(),
            key_type: state.key_type,
            padding: SignPadding::Default,
            salt_length: -1,
        });
    }
    if crypto_key::is_crypto_key(scope, input) {
        let ck = crypto_key::require(scope, input)?;
        return Ok(SignKeyInput {
            material: ck.material.clone(),
            key_type: ck.key_type,
            padding: SignPadding::Default,
            salt_length: -1,
        });
    }
    if let Ok(obj) = v8::Local::<v8::Object>::try_from(input) {
        let key_attr = v8::String::new(scope, "key").unwrap();
        if obj.has(scope, key_attr.into()).unwrap_or(false) {
            let padding = read_optional_u32(scope, obj, "padding")?;
            let salt_length = read_optional_i32(scope, obj, "saltLength")?;
            let key_v = obj.get(scope, key_attr.into()).unwrap();
            let (material, key_type) = if let Some(state) = key_object::downcast_state(scope, key_v)
            {
                (state.material.clone(), state.key_type)
            } else if crypto_key::is_crypto_key(scope, key_v) {
                let ck = crypto_key::require(scope, key_v)?;
                (ck.material.clone(), ck.key_type)
            } else {
                let parsed_v = key_object::create_private_key(scope, key_v)
                    .or_else(|_| key_object::create_public_key(scope, key_v))?;
                let parsed_state =
                    key_object::downcast_state(scope, parsed_v).ok_or_else(|| {
                        OpError::node(
                            "ERR_INVALID_ARG_VALUE",
                            "Could not parse key from options object",
                        )
                    })?;
                (parsed_state.material.clone(), parsed_state.key_type)
            };
            let padding_kind = match padding {
                Some(1) => SignPadding::RsaPkcs1v15,
                Some(6) => SignPadding::RsaPss,
                None => SignPadding::Default,
                Some(other) => {
                    return Err(OpError::node(
                        "ERR_INVALID_ARG_VALUE",
                        format!("Unknown padding: {other}"),
                    ));
                }
            };
            return Ok(SignKeyInput {
                material,
                key_type,
                padding: padding_kind,
                salt_length: salt_length.unwrap_or(-1),
            });
        }
    }
    let parsed_v = key_object::create_private_key(scope, input)
        .or_else(|_| key_object::create_public_key(scope, input))?;
    let parsed_state = key_object::downcast_state(scope, parsed_v).ok_or_else(|| {
        OpError::node("ERR_INVALID_ARG_VALUE", "Could not parse key input")
    })?;
    Ok(SignKeyInput {
        material: parsed_state.material.clone(),
        key_type: parsed_state.key_type,
        padding: SignPadding::Default,
        salt_length: -1,
    })
}

fn read_optional_u32(
    scope: &mut v8::PinScope,
    obj: v8::Local<v8::Object>,
    name: &str,
) -> Result<Option<u32>, OpError> {
    let k = v8::String::new(scope, name).unwrap();
    let v = match obj.get(scope, k.into()) {
        Some(v) => v,
        None => return Ok(None),
    };
    if v.is_undefined() || v.is_null() {
        return Ok(None);
    }
    Ok(v.uint32_value(scope))
}

fn read_optional_i32(
    scope: &mut v8::PinScope,
    obj: v8::Local<v8::Object>,
    name: &str,
) -> Result<Option<i32>, OpError> {
    let k = v8::String::new(scope, name).unwrap();
    let v = match obj.get(scope, k.into()) {
        Some(v) => v,
        None => return Ok(None),
    };
    if v.is_undefined() || v.is_null() {
        return Ok(None);
    }
    Ok(v.int32_value(scope))
}

// ---------------------------------------------------------------------------
// Sign / verify dispatch
// ---------------------------------------------------------------------------

fn sign_with_material(
    material: &KeyMaterial,
    hash: HashAlgo,
    data: &[u8],
    padding: SignPadding,
    salt_length: i32,
) -> Result<Vec<u8>, OpError> {
    match material {
        KeyMaterial::RsaPrivate { pkcs8_der, .. } => match padding {
            SignPadding::RsaPss => {
                crate::web::crypto::evp_ffi::sign_with_salt(pkcs8_der, hash, data, salt_length)
            }
            _ => crate::web::crypto::evp_ffi::pkcs1_sign(pkcs8_der, hash, data),
        },
        KeyMaterial::EcPrivate { pkcs8_der, raw_xy, .. } => {
            // Node returns DER-encoded ECDSA signatures — call the
            // helper that PRESERVES the EVP DER output rather than
            // converting to r||s.
            ecdsa_sign_der(pkcs8_der, hash, data, raw_xy.len())
        }
        KeyMaterial::Ed25519Private { pkcs8_der, .. } => {
            // Ed25519 signs the message directly via EVP — no separate
            // hash. The existing okp::sign_ed25519 expects a CryptoKeyState;
            // we route via aws-lc-rs Ed25519KeyPair directly.
            use aws_lc_rs::signature::Ed25519KeyPair;
            let kp = Ed25519KeyPair::from_pkcs8_maybe_unchecked(pkcs8_der).map_err(|_| {
                OpError::node("ERR_CRYPTO_OPERATION_FAILED", "Ed25519 key load")
            })?;
            Ok(kp.sign(data).as_ref().to_vec())
        }
        _ => {
            let _ = salt_length;
            Err(OpError::node(
                "ERR_CRYPTO_INCOMPATIBLE_KEY_OPTIONS",
                "key material does not support signing",
            ))
        }
    }
}

fn verify_with_material(
    material: &KeyMaterial,
    hash: HashAlgo,
    data: &[u8],
    sig: &[u8],
    padding: SignPadding,
    salt_length: i32,
) -> Result<bool, OpError> {
    match material {
        KeyMaterial::RsaPublic { spki_der, .. } => match padding {
            SignPadding::RsaPss => crate::web::crypto::evp_ffi::verify_with_salt(
                spki_der,
                hash,
                data,
                sig,
                salt_length,
            ),
            _ => crate::web::crypto::evp_ffi::pkcs1_verify(spki_der, hash, data, sig),
        },
        KeyMaterial::EcPublic { spki_der, raw_xy } => {
            ecdsa_verify_der(spki_der, hash, data, sig, raw_xy.len())
        }
        KeyMaterial::EcPrivate { raw_xy, .. } => {
            // Verify using a derived SPKI. Build it from raw_xy.
            let spki = build_ec_spki_for_verify(raw_xy)?;
            ecdsa_verify_der(&spki, hash, data, sig, raw_xy.len())
        }
        KeyMaterial::Ed25519Public { raw_x, .. } => {
            use aws_lc_rs::signature;
            let unparsed = signature::UnparsedPublicKey::new(&signature::ED25519, raw_x.as_slice());
            Ok(unparsed.verify(data, sig).is_ok())
        }
        KeyMaterial::Ed25519Private { raw_x, .. } => {
            use aws_lc_rs::signature;
            let unparsed = signature::UnparsedPublicKey::new(&signature::ED25519, raw_x.as_slice());
            Ok(unparsed.verify(data, sig).is_ok())
        }
        KeyMaterial::RsaPrivate { components, .. } => {
            // Build SPKI from components and verify against it.
            let spki = build_rsa_spki(&components.n, &components.e)?;
            match padding {
                SignPadding::RsaPss => crate::web::crypto::evp_ffi::verify_with_salt(
                    &spki, hash, data, sig, salt_length,
                ),
                _ => crate::web::crypto::evp_ffi::pkcs1_verify(&spki, hash, data, sig),
            }
        }
        _ => {
            let _ = padding;
            let _ = salt_length;
            Err(OpError::node(
                "ERR_CRYPTO_INCOMPATIBLE_KEY_OPTIONS",
                "key material does not support verifying",
            ))
        }
    }
}

/// ECDSA sign returning DER-encoded signature (Node default wire
/// format). Implementation: same as evp_ffi::ecdsa_sign but skip the
/// p1363 conversion at the end. We replicate the EVP loop here to
/// avoid changing the WebCrypto helper's return contract.
fn ecdsa_sign_der(pkcs8: &[u8], hash: HashAlgo, data: &[u8], _xy_len: usize) -> Result<Vec<u8>, OpError> {
    // Get raw DER from EVP_DigestSignFinal — same approach as
    // evp_ffi::ecdsa_sign, minus the final ecdsa_der_to_p1363 step.
    crate::web::crypto::evp_ffi::ecdsa_sign_der(pkcs8, hash, data)
}

fn ecdsa_verify_der(
    spki: &[u8],
    hash: HashAlgo,
    data: &[u8],
    sig: &[u8],
    _xy_len: usize,
) -> Result<bool, OpError> {
    crate::web::crypto::evp_ffi::ecdsa_verify_der(spki, hash, data, sig)
}

// SPKI builders (small DER builders also live in key_object.rs; we
// duplicate the minimum needed here to avoid pub-exporting all the
// asn.1 helpers from key_object).
fn der_len(n: usize) -> Vec<u8> {
    if n < 128 {
        vec![n as u8]
    } else if n < 256 {
        vec![0x81, n as u8]
    } else if n < 65536 {
        vec![0x82, ((n >> 8) & 0xff) as u8, (n & 0xff) as u8]
    } else {
        vec![
            0x83,
            ((n >> 16) & 0xff) as u8,
            ((n >> 8) & 0xff) as u8,
            (n & 0xff) as u8,
        ]
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

fn der_integer_unsigned(bytes: &[u8]) -> Vec<u8> {
    let mut s = bytes;
    while s.len() > 1 && s[0] == 0 {
        s = &s[1..];
    }
    let mut content = Vec::with_capacity(s.len() + 1);
    if !s.is_empty() && s[0] & 0x80 != 0 {
        content.push(0);
    }
    content.extend_from_slice(s);
    let mut out = vec![0x02];
    out.extend_from_slice(&der_len(content.len()));
    out.extend_from_slice(&content);
    out
}

const OID_RSA: &[u8] = &[0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01];
const OID_EC_PUBLIC_KEY: &[u8] = &[0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01];
const OID_P256_CURVE: &[u8] = &[0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07];
const OID_P384_CURVE: &[u8] = &[0x06, 0x05, 0x2b, 0x81, 0x04, 0x00, 0x22];
const OID_P521_CURVE: &[u8] = &[0x06, 0x05, 0x2b, 0x81, 0x04, 0x00, 0x23];
const NULL_PARAMS: &[u8] = &[0x05, 0x00];

fn build_rsa_spki(n: &[u8], e: &[u8]) -> Result<Vec<u8>, OpError> {
    let rsa_pub = {
        let mut body = Vec::new();
        body.extend_from_slice(&der_integer_unsigned(n));
        body.extend_from_slice(&der_integer_unsigned(e));
        der_seq(&body)
    };
    let mut alg_id_body = Vec::new();
    alg_id_body.extend_from_slice(OID_RSA);
    alg_id_body.extend_from_slice(NULL_PARAMS);
    let alg_id = der_seq(&alg_id_body);
    let bs = der_bit_string(&rsa_pub);
    let mut body = Vec::new();
    body.extend_from_slice(&alg_id);
    body.extend_from_slice(&bs);
    Ok(der_seq(&body))
}

fn build_ec_spki_for_verify(raw_xy: &[u8]) -> Result<Vec<u8>, OpError> {
    let curve_oid: &[u8] = match raw_xy.len() {
        65 => OID_P256_CURVE,
        97 => OID_P384_CURVE,
        133 => OID_P521_CURVE,
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

// ---------------------------------------------------------------------------
// One-shot
// ---------------------------------------------------------------------------

pub fn one_shot_sign<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    algorithm: v8::Local<v8::Value>,
    data: v8::Local<v8::Value>,
    key: v8::Local<v8::Value>,
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    let SignKeyInput {
        material,
        key_type,
        padding,
        salt_length,
    } = parse_sign_key_input(scope, key)?;
    if key_type != KeyType::Private {
        return Err(OpError::node(
            "ERR_CRYPTO_INCOMPATIBLE_KEY_OPTIONS",
            "crypto.sign requires a private key",
        ));
    }
    let data_bytes = buffer::extract_input(scope, data, None)?;
    let sig = if matches!(material, KeyMaterial::Ed25519Private { .. }) {
        // Ed25519 ignores the `algorithm` (must be null per Node).
        let pkcs8 = match &material {
            KeyMaterial::Ed25519Private { pkcs8_der, .. } => pkcs8_der,
            _ => unreachable!(),
        };
        use aws_lc_rs::signature::Ed25519KeyPair;
        let kp = Ed25519KeyPair::from_pkcs8_maybe_unchecked(pkcs8).map_err(|_| {
            OpError::node("ERR_CRYPTO_OPERATION_FAILED", "Ed25519 key load")
        })?;
        kp.sign(&data_bytes).as_ref().to_vec()
    } else {
        let hash_name = if algorithm.is_string() {
            algorithm.to_rust_string_lossy(scope)
        } else {
            return Err(OpError::node(
                "ERR_OSSL_EVP_INVALID_DIGEST",
                "crypto.sign: algorithm required for non-Ed25519 keys",
            ));
        };
        let hash = parse_hash_name(&hash_name)?;
        sign_with_material(&material, hash, &data_bytes, padding, salt_length)?
    };
    Ok(buffer::emit_buffer(scope, &sig))
}

pub fn one_shot_verify<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    algorithm: v8::Local<v8::Value>,
    data: v8::Local<v8::Value>,
    key: v8::Local<v8::Value>,
    signature: v8::Local<v8::Value>,
) -> Result<bool, OpError> {
    let SignKeyInput {
        material,
        padding,
        salt_length,
        ..
    } = parse_sign_key_input(scope, key)?;
    let data_bytes = buffer::extract_input(scope, data, None)?;
    let sig_bytes = buffer::extract_input(scope, signature, None)?;
    if matches!(
        material,
        KeyMaterial::Ed25519Private { .. } | KeyMaterial::Ed25519Public { .. }
    ) {
        let raw_x = match &material {
            KeyMaterial::Ed25519Private { raw_x, .. } | KeyMaterial::Ed25519Public { raw_x, .. } => {
                raw_x
            }
            _ => unreachable!(),
        };
        use aws_lc_rs::signature;
        let unparsed = signature::UnparsedPublicKey::new(&signature::ED25519, raw_x.as_slice());
        return Ok(unparsed.verify(&data_bytes, &sig_bytes).is_ok());
    }
    let hash_name = if algorithm.is_string() {
        algorithm.to_rust_string_lossy(scope)
    } else {
        return Err(OpError::node(
            "ERR_OSSL_EVP_INVALID_DIGEST",
            "crypto.verify: algorithm required for non-Ed25519 keys",
        ));
    };
    let hash = parse_hash_name(&hash_name)?;
    verify_with_material(&material, hash, &data_bytes, &sig_bytes, padding, salt_length)
}

// ---------------------------------------------------------------------------
// publicEncrypt / privateDecrypt — RSA-OAEP via aws-lc-rs
// ---------------------------------------------------------------------------

pub fn public_encrypt<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    key_or_options: v8::Local<v8::Value>,
    data: v8::Local<v8::Value>,
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    let (material, oaep_hash) = parse_oaep_key(scope, key_or_options, true)?;
    let data_bytes = buffer::extract_input(scope, data, None)?;
    let spki = match &material {
        KeyMaterial::RsaPublic { spki_der, .. } => spki_der.clone(),
        KeyMaterial::RsaPrivate { components, .. } => build_rsa_spki(&components.n, &components.e)?,
        _ => {
            return Err(OpError::node(
                "ERR_CRYPTO_INCOMPATIBLE_KEY_OPTIONS",
                "publicEncrypt requires an RSA key",
            ));
        }
    };
    let oaep = match oaep_hash {
        HashAlgo::Sha1 => &aws_lc_rs::rsa::OAEP_SHA1_MGF1SHA1,
        HashAlgo::Sha256 => &aws_lc_rs::rsa::OAEP_SHA256_MGF1SHA256,
        HashAlgo::Sha384 => &aws_lc_rs::rsa::OAEP_SHA384_MGF1SHA384,
        HashAlgo::Sha512 => &aws_lc_rs::rsa::OAEP_SHA512_MGF1SHA512,
    };
    let pub_key = aws_lc_rs::rsa::OaepPublicEncryptingKey::new(
        aws_lc_rs::rsa::PublicEncryptingKey::from_der(&spki).map_err(|_| {
            OpError::node("ERR_CRYPTO_OPERATION_FAILED", "RSA public key load failed")
        })?,
    )
    .map_err(|_| OpError::node("ERR_CRYPTO_OPERATION_FAILED", "OAEP key wrap failed"))?;
    let mut out = vec![0u8; pub_key.ciphertext_size()];
    let written = pub_key
        .encrypt(oaep, &data_bytes, &mut out, None)
        .map_err(|_| OpError::node("ERR_CRYPTO_OPERATION_FAILED", "OAEP encrypt failed"))?;
    Ok(buffer::emit_buffer(scope, written))
}

pub fn private_decrypt<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    key_or_options: v8::Local<v8::Value>,
    data: v8::Local<v8::Value>,
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    let (material, oaep_hash) = parse_oaep_key(scope, key_or_options, false)?;
    let data_bytes = buffer::extract_input(scope, data, None)?;
    let pkcs8 = match &material {
        KeyMaterial::RsaPrivate { pkcs8_der, .. } => pkcs8_der.clone(),
        _ => {
            return Err(OpError::node(
                "ERR_CRYPTO_INCOMPATIBLE_KEY_OPTIONS",
                "privateDecrypt requires an RSA private key",
            ));
        }
    };
    let oaep = match oaep_hash {
        HashAlgo::Sha1 => &aws_lc_rs::rsa::OAEP_SHA1_MGF1SHA1,
        HashAlgo::Sha256 => &aws_lc_rs::rsa::OAEP_SHA256_MGF1SHA256,
        HashAlgo::Sha384 => &aws_lc_rs::rsa::OAEP_SHA384_MGF1SHA384,
        HashAlgo::Sha512 => &aws_lc_rs::rsa::OAEP_SHA512_MGF1SHA512,
    };
    let priv_key = aws_lc_rs::rsa::OaepPrivateDecryptingKey::new(
        aws_lc_rs::rsa::PrivateDecryptingKey::from_pkcs8(&pkcs8).map_err(|_| {
            OpError::node("ERR_CRYPTO_OPERATION_FAILED", "RSA private key load failed")
        })?,
    )
    .map_err(|_| OpError::node("ERR_CRYPTO_OPERATION_FAILED", "OAEP key wrap failed"))?;
    let mut out = vec![0u8; priv_key.min_output_size()];
    let pt = priv_key
        .decrypt(oaep, &data_bytes, &mut out, None)
        .map_err(|_| OpError::node("ERR_CRYPTO_OPERATION_FAILED", "OAEP decrypt failed"))?;
    Ok(buffer::emit_buffer(scope, pt))
}

fn parse_oaep_key(
    scope: &mut v8::PinScope,
    input: v8::Local<v8::Value>,
    public: bool,
) -> Result<(KeyMaterial, HashAlgo), OpError> {
    let mut hash = HashAlgo::Sha1;
    if let Ok(obj) = v8::Local::<v8::Object>::try_from(input) {
        let key_attr = v8::String::new(scope, "key").unwrap();
        if obj.has(scope, key_attr.into()).unwrap_or(false) {
            let key_v = obj.get(scope, key_attr.into()).unwrap();
            let oaep_hash_attr = v8::String::new(scope, "oaepHash").unwrap();
            if let Some(h) = obj.get(scope, oaep_hash_attr.into()) {
                if h.is_string() {
                    let s = h.to_rust_string_lossy(scope);
                    hash = HashAlgo::from_str(&s).unwrap_or(HashAlgo::Sha1);
                }
            }
            return Ok((extract_key_material(scope, key_v, public)?, hash));
        }
    }
    Ok((extract_key_material(scope, input, public)?, hash))
}

fn extract_key_material(
    scope: &mut v8::PinScope,
    input: v8::Local<v8::Value>,
    public: bool,
) -> Result<KeyMaterial, OpError> {
    if let Some(state) = key_object::downcast_state(scope, input) {
        return Ok(state.material.clone());
    }
    if crypto_key::is_crypto_key(scope, input) {
        let ck = crypto_key::require(scope, input)?;
        return Ok(ck.material.clone());
    }
    let parsed_v = if public {
        key_object::create_public_key(scope, input)?
    } else {
        key_object::create_private_key(scope, input)?
    };
    let parsed_state = key_object::downcast_state(scope, parsed_v).ok_or_else(|| {
        OpError::node("ERR_CRYPTO_OPERATION_FAILED", "Could not parse key input")
    })?;
    Ok(parsed_state.material.clone())
}

// ---------------------------------------------------------------------------
// Top-level callbacks
// ---------------------------------------------------------------------------

pub(crate) fn create_sign_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    if args.length() < 1 {
        let exc = crate::node_error::build_node_exception(
            scope,
            "ERR_INVALID_ARG_TYPE",
            "createSign requires an algorithm",
        );
        scope.throw_exception(exc);
        return;
    }
    let name = args.get(0).to_rust_string_lossy(scope);
    let options = if args.length() >= 2 {
        Some(args.get(1))
    } else {
        None
    };
    match create_sign(scope, &name, options) {
        Ok(v) => rv.set(v),
        Err(err) => {
            let exc = crate::web::crypto::helpers::op_error_to_v8(scope, err);
            scope.throw_exception(exc);
        }
    }
}

pub(crate) fn create_verify_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    if args.length() < 1 {
        let exc = crate::node_error::build_node_exception(
            scope,
            "ERR_INVALID_ARG_TYPE",
            "createVerify requires an algorithm",
        );
        scope.throw_exception(exc);
        return;
    }
    let name = args.get(0).to_rust_string_lossy(scope);
    let options = if args.length() >= 2 {
        Some(args.get(1))
    } else {
        None
    };
    match create_verify(scope, &name, options) {
        Ok(v) => rv.set(v),
        Err(err) => {
            let exc = crate::web::crypto::helpers::op_error_to_v8(scope, err);
            scope.throw_exception(exc);
        }
    }
}

pub(crate) fn sign_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    if args.length() < 3 {
        let exc = crate::node_error::build_node_exception(
            scope,
            "ERR_INVALID_ARG_TYPE",
            "sign(algorithm, data, key) requires three arguments",
        );
        scope.throw_exception(exc);
        return;
    }
    match one_shot_sign(scope, args.get(0), args.get(1), args.get(2)) {
        Ok(v) => rv.set(v),
        Err(err) => {
            let exc = crate::web::crypto::helpers::op_error_to_v8(scope, err);
            scope.throw_exception(exc);
        }
    }
}

pub(crate) fn verify_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    if args.length() < 4 {
        let exc = crate::node_error::build_node_exception(
            scope,
            "ERR_INVALID_ARG_TYPE",
            "verify(algorithm, data, key, signature) requires four arguments",
        );
        scope.throw_exception(exc);
        return;
    }
    match one_shot_verify(scope, args.get(0), args.get(1), args.get(2), args.get(3)) {
        Ok(b) => {
            let v = v8::Boolean::new(scope, b);
            rv.set(v.into());
        }
        Err(err) => {
            let exc = crate::web::crypto::helpers::op_error_to_v8(scope, err);
            scope.throw_exception(exc);
        }
    }
}

pub(crate) fn public_encrypt_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    if args.length() < 2 {
        let exc = crate::node_error::build_node_exception(
            scope,
            "ERR_INVALID_ARG_TYPE",
            "publicEncrypt(key, buffer) requires two arguments",
        );
        scope.throw_exception(exc);
        return;
    }
    match public_encrypt(scope, args.get(0), args.get(1)) {
        Ok(v) => rv.set(v),
        Err(err) => {
            let exc = crate::web::crypto::helpers::op_error_to_v8(scope, err);
            scope.throw_exception(exc);
        }
    }
}

pub(crate) fn private_decrypt_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    if args.length() < 2 {
        let exc = crate::node_error::build_node_exception(
            scope,
            "ERR_INVALID_ARG_TYPE",
            "privateDecrypt(key, buffer) requires two arguments",
        );
        scope.throw_exception(exc);
        return;
    }
    match private_decrypt(scope, args.get(0), args.get(1)) {
        Ok(v) => rv.set(v),
        Err(err) => {
            let exc = crate::web::crypto::helpers::op_error_to_v8(scope, err);
            scope.throw_exception(exc);
        }
    }
}
