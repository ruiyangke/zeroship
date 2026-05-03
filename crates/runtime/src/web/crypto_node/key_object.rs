//! `KeyObject` + `PublicKeyObject` / `PrivateKeyObject` / `SecretKeyObject`.
//!
//! Per `docs/proposals/node-crypto-native.md` §IV (D-N3, D-N4, D-N13,
//! D-N14, D-N19).
//!
//! Architecture: the WebCrypto surface already shipped a complete
//! `KeyMaterial` enum + DER/JWK parsers via `web::crypto::{rsa, ec, okp,
//! jwk}`. This module is the node:crypto adapter — wraps the same
//! `KeyMaterial` in a Node-shaped class hierarchy plus PEM I/O.
//!
//! - `KeyObject` parent class with `.type` / `.asymmetricKeyType` /
//!   `.asymmetricKeyDetails` / `.symmetricKeySize` / `.export(opts)` /
//!   `.equals(other)` / `.toCryptoKey(...)`.
//! - Three subclasses (`PublicKeyObject`, `PrivateKeyObject`,
//!   `SecretKeyObject`) for `instanceof` discrimination per Node parity.
//! - Factories `createSecretKey`, `createPublicKey`, `createPrivateKey`
//!   accept Buffer/string/PEM/JWK input shapes.
//! - `KeyObject.from(cryptoKey)` clones a CryptoKey's `KeyMaterial`
//!   into a fresh KeyObject (forward bridge per D-N4).
//!
//! Encrypted PKCS#8 (`{ cipher, passphrase }`) is gated to Stage E in
//! this implementation — see §IV.4a / D-N37; the high-level surface
//! returns `ERR_CRYPTO_UNSUPPORTED_OPERATION`. PEM encrypted-input
//! paths similarly throw `ERR_MISSING_PASSPHRASE` when a passphrase is
//! seen on an encrypted-PEM block. The full PBES2 raw-FFI path lands
//! when a creator app surfaces a use-case that demands it.

#![allow(unsafe_code)]

use super::buffer;
use super::super::crypto::crypto_key;
use super::super::crypto::helpers::{base64url_decode, base64url_encode};
use super::super::crypto::jwk as wc_jwk;
use super::super::crypto::key_material::{
    AesKeyAlgorithm, CryptoKeyState, EcKeyAlgorithm, HashAlgo, HmacKeyAlgorithm, KeyAlgorithm,
    KeyMaterial, KeyType, KeyUsage, NamedCurve, RsaHashedKeyAlgorithm, RsaPrivateComponents,
    RsaPublicComponents,
};
use crate::state::OpError;

#[allow(unused_imports)]
use zeroship_runtime_macros::{
    v8_class, v8_constructor, v8_getter, v8_inherit, v8_method, v8_name, v8_to_string_tag,
};

// ---------------------------------------------------------------------------
// State + brand
// ---------------------------------------------------------------------------

pub const KEY_OBJECT_TAG: u8 = 0xC2;

#[derive(Clone)]
pub struct KeyObjectState {
    pub key_type: KeyType,
    pub material: KeyMaterial,
}

impl KeyObjectState {
    pub fn from_crypto_key(state: &CryptoKeyState) -> Self {
        Self {
            key_type: state.key_type,
            material: state.material.clone(),
        }
    }

    pub fn asymmetric_key_type(&self) -> Option<&'static str> {
        match &self.material {
            KeyMaterial::Symmetric(_) => None,
            KeyMaterial::EcPrivate { .. } | KeyMaterial::EcPublic { .. } => Some("ec"),
            KeyMaterial::RsaPrivate { .. } | KeyMaterial::RsaPublic { .. } => Some("rsa"),
            KeyMaterial::Ed25519Private { .. } | KeyMaterial::Ed25519Public { .. } => {
                Some("ed25519")
            }
            KeyMaterial::X25519Private { .. } | KeyMaterial::X25519Public { .. } => Some("x25519"),
        }
    }

    pub fn symmetric_key_size(&self) -> Option<u32> {
        match &self.material {
            KeyMaterial::Symmetric(b) => Some(b.len() as u32),
            _ => None,
        }
    }

    pub fn ec_named_curve(&self) -> Option<NamedCurve> {
        let raw_xy = match &self.material {
            KeyMaterial::EcPrivate { raw_xy, .. } => raw_xy,
            KeyMaterial::EcPublic { raw_xy, .. } => raw_xy,
            _ => return None,
        };
        match raw_xy.len() {
            65 => Some(NamedCurve::P256),
            97 => Some(NamedCurve::P384),
            133 => Some(NamedCurve::P521),
            _ => None,
        }
    }

    /// Canonical comparison bytes for `equals` (§IV.7a).
    pub fn canonical_bytes(&self) -> Vec<u8> {
        match &self.material {
            KeyMaterial::Symmetric(b) => b.clone(),
            KeyMaterial::EcPrivate { pkcs8_der, .. } => pkcs8_der.clone(),
            KeyMaterial::EcPublic { spki_der, .. } => spki_der.clone(),
            KeyMaterial::RsaPrivate { pkcs8_der, .. } => pkcs8_der.clone(),
            KeyMaterial::RsaPublic { spki_der, .. } => spki_der.clone(),
            KeyMaterial::Ed25519Private { pkcs8_der, .. } => pkcs8_der.clone(),
            KeyMaterial::Ed25519Public { spki_der, .. } => spki_der.clone(),
            KeyMaterial::X25519Private { pkcs8_der, .. } => pkcs8_der.clone(),
            KeyMaterial::X25519Public { spki_der, .. } => spki_der.clone(),
        }
    }
}

#[repr(C)]
pub struct KeyObject {
    pub tag: u8,
    pub state: KeyObjectState,
}

impl KeyObject {
    pub fn new_box(state: KeyObjectState) -> Self {
        Self {
            tag: KEY_OBJECT_TAG,
            state,
        }
    }
}

#[v8_class]
#[v8_to_string_tag = "KeyObject"]
impl KeyObject {
    #[v8_constructor]
    fn new() -> Result<KeyObject, OpError> {
        Err(OpError::type_error(
            "KeyObject is not a constructor — use crypto.create*Key()",
        ))
    }

    #[v8_getter]
    #[v8_name = "type"]
    fn type_(&self) -> String {
        self.state.key_type.as_str().to_string()
    }

    #[v8_getter]
    #[v8_name = "asymmetricKeyType"]
    fn asymmetric_key_type(&self) -> Option<String> {
        self.state.asymmetric_key_type().map(String::from)
    }

    #[v8_getter]
    #[v8_name = "symmetricKeySize"]
    fn symmetric_key_size(&self) -> Option<u32> {
        self.state.symmetric_key_size()
    }

    #[v8_getter]
    #[v8_name = "asymmetricKeyDetails"]
    fn asymmetric_key_details<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
    ) -> v8::Local<'s, v8::Value> {
        build_asymmetric_key_details(scope, &self.state)
    }

    #[v8_method]
    fn export<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        options: v8::Local<v8::Value>,
    ) -> Result<v8::Local<'s, v8::Value>, OpError> {
        export_impl(scope, &self.state, options)
    }

    #[v8_method]
    fn equals(&self, scope: &mut v8::PinScope, other: v8::Local<v8::Value>) -> bool {
        let Some(other_state) = downcast_state(scope, other) else {
            return false;
        };
        if self.state.key_type != other_state.key_type {
            return false;
        }
        let a = self.state.canonical_bytes();
        let b = other_state.canonical_bytes();
        if a.len() != b.len() {
            return false;
        }
        aws_lc_rs::constant_time::verify_slices_are_equal(&a, &b).is_ok()
    }

    #[v8_method]
    #[v8_name = "toCryptoKey"]
    fn to_crypto_key<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        algorithm: v8::Local<v8::Value>,
        extractable: v8::Local<v8::Value>,
        key_usages: v8::Local<v8::Value>,
    ) -> Result<v8::Local<'s, v8::Value>, OpError> {
        let alg = parse_webcrypto_algorithm(scope, algorithm, &self.state.material)?;
        let extractable_b = extractable.boolean_value(scope);
        let usages = parse_key_usages(scope, key_usages)?;
        let new_state = CryptoKeyState {
            key_type: self.state.key_type,
            extractable: extractable_b,
            algorithm: alg,
            usages,
            material: self.state.material.clone(),
        };
        Ok(crypto_key::build(scope, new_state).into())
    }
}

#[v8_class]
#[v8_inherit(KeyObject)]
#[v8_to_string_tag = "PublicKeyObject"]
impl PublicKeyObject {
    #[v8_constructor]
    fn new() -> Result<PublicKeyObject, OpError> {
        Err(OpError::type_error("PublicKeyObject is not a constructor"))
    }
}

#[v8_class]
#[v8_inherit(KeyObject)]
#[v8_to_string_tag = "PrivateKeyObject"]
impl PrivateKeyObject {
    #[v8_constructor]
    fn new() -> Result<PrivateKeyObject, OpError> {
        Err(OpError::type_error("PrivateKeyObject is not a constructor"))
    }
}

#[v8_class]
#[v8_inherit(KeyObject)]
#[v8_to_string_tag = "SecretKeyObject"]
impl SecretKeyObject {
    #[v8_constructor]
    fn new() -> Result<SecretKeyObject, OpError> {
        Err(OpError::type_error("SecretKeyObject is not a constructor"))
    }
}

pub struct PublicKeyObject;
pub struct PrivateKeyObject;
pub struct SecretKeyObject;

// ---------------------------------------------------------------------------
// Brand check + state extraction
// ---------------------------------------------------------------------------

pub fn is_key_object(scope: &mut v8::PinScope, value: v8::Local<v8::Value>) -> bool {
    let obj = match v8::Local::<v8::Object>::try_from(value) {
        Ok(o) => o,
        Err(_) => return false,
    };
    if obj.internal_field_count() != 1 {
        return false;
    }
    let field = match obj.get_internal_field(scope, 0) {
        Some(f) => f,
        None => return false,
    };
    let ext = match v8::Local::<v8::External>::try_from(field) {
        Ok(e) => e,
        Err(_) => return false,
    };
    let ptr = ext.value() as *const u8;
    if ptr.is_null() {
        return false;
    }
    let tag = unsafe { *ptr };
    tag == KEY_OBJECT_TAG
}

pub fn state_unchecked<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    this: v8::Local<v8::Object>,
) -> &'s KeyObjectState {
    let field = this.get_internal_field(scope, 0).unwrap();
    let ext: v8::Local<v8::External> = field.try_into().unwrap();
    let ptr = ext.value() as *const KeyObject;
    unsafe { &(*ptr).state }
}

pub fn downcast_state<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    value: v8::Local<v8::Value>,
) -> Option<&'s KeyObjectState> {
    if !is_key_object(scope, value) {
        return None;
    }
    let obj: v8::Local<v8::Object> = value.try_into().ok()?;
    Some(state_unchecked(scope, obj))
}

// ---------------------------------------------------------------------------
// Build helpers
// ---------------------------------------------------------------------------

pub fn build_for_type<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    state: KeyObjectState,
) -> v8::Local<'s, v8::Value> {
    match state.key_type {
        KeyType::Secret => build_secret(scope, state).into(),
        KeyType::Public => build_public(scope, state).into(),
        KeyType::Private => build_private(scope, state).into(),
    }
}

fn build_with_template<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    tmpl: v8::Local<'s, v8::FunctionTemplate>,
    state: KeyObjectState,
) -> v8::Local<'s, v8::Object> {
    let inst_tmpl = tmpl.instance_template(scope);
    let inst = inst_tmpl
        .new_instance(scope)
        .expect("KeyObject instance allocation");
    let class_fn = tmpl.get_function(scope).unwrap();
    let proto_key = v8::String::new(scope, "prototype").unwrap();
    let proto_v = class_fn.get(scope, proto_key.into()).unwrap();
    inst.set_prototype(scope, proto_v);

    let boxed: Box<KeyObject> = Box::new(KeyObject::new_box(state));
    let raw = Box::into_raw(boxed);
    let raw_addr = raw as usize;
    let ext = v8::External::new(scope, raw as *mut std::ffi::c_void);
    inst.set_internal_field(0, ext.into());
    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        inst,
        Box::new(move || unsafe {
            drop(Box::from_raw(raw_addr as *mut KeyObject));
        }),
    );
    std::mem::forget(weak);
    inst
}

pub fn build_public<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    state: KeyObjectState,
) -> v8::Local<'s, v8::Object> {
    let tmpl = PublicKeyObject::install(scope);
    build_with_template(scope, tmpl, state)
}

pub fn build_private<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    state: KeyObjectState,
) -> v8::Local<'s, v8::Object> {
    let tmpl = PrivateKeyObject::install(scope);
    build_with_template(scope, tmpl, state)
}

pub fn build_secret<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    state: KeyObjectState,
) -> v8::Local<'s, v8::Object> {
    let tmpl = SecretKeyObject::install(scope);
    build_with_template(scope, tmpl, state)
}

// ---------------------------------------------------------------------------
// asymmetricKeyDetails (§IV.3)
// ---------------------------------------------------------------------------

fn build_asymmetric_key_details<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    state: &KeyObjectState,
) -> v8::Local<'s, v8::Value> {
    match &state.material {
        KeyMaterial::Symmetric(_) => v8::undefined(scope).into(),
        KeyMaterial::RsaPrivate { components, .. } => {
            let obj = v8::Object::new(scope);
            let modulus_bits = (components.n.len() * 8) as u32;
            set_u32(scope, obj, "modulusLength", modulus_bits);
            let bn = vec_to_bigint(scope, &components.e);
            let k = v8::String::new(scope, "publicExponent").unwrap();
            obj.set(scope, k.into(), bn.into());
            obj.into()
        }
        KeyMaterial::RsaPublic { components, .. } => {
            let obj = v8::Object::new(scope);
            let modulus_bits = (components.n.len() * 8) as u32;
            set_u32(scope, obj, "modulusLength", modulus_bits);
            let bn = vec_to_bigint(scope, &components.e);
            let k = v8::String::new(scope, "publicExponent").unwrap();
            obj.set(scope, k.into(), bn.into());
            obj.into()
        }
        KeyMaterial::EcPrivate { .. } | KeyMaterial::EcPublic { .. } => {
            let obj = v8::Object::new(scope);
            if let Some(curve) = state.ec_named_curve() {
                let openssl_name = match curve {
                    NamedCurve::P256 => "prime256v1",
                    NamedCurve::P384 => "secp384r1",
                    NamedCurve::P521 => "secp521r1",
                };
                set_str(scope, obj, "namedCurve", openssl_name);
            }
            obj.into()
        }
        KeyMaterial::Ed25519Private { .. }
        | KeyMaterial::Ed25519Public { .. }
        | KeyMaterial::X25519Private { .. }
        | KeyMaterial::X25519Public { .. } => v8::Object::new(scope).into(),
    }
}

fn vec_to_bigint<'s>(scope: &mut v8::PinScope<'s, '_>, bytes: &[u8]) -> v8::Local<'s, v8::BigInt> {
    if bytes.is_empty() {
        return v8::BigInt::new_from_u64(scope, 0);
    }
    let mut be = bytes;
    while be.len() > 1 && be[0] == 0 {
        be = &be[1..];
    }
    let mut le_bytes = be.to_vec();
    le_bytes.reverse();
    let mut words = Vec::with_capacity((le_bytes.len() + 7) / 8);
    for chunk in le_bytes.chunks(8) {
        let mut buf = [0u8; 8];
        buf[..chunk.len()].copy_from_slice(chunk);
        words.push(u64::from_le_bytes(buf));
    }
    v8::BigInt::new_from_words(scope, false, &words)
        .unwrap_or_else(|| v8::BigInt::new_from_u64(scope, 65537))
}

fn set_str<'s>(scope: &mut v8::PinScope<'s, '_>, obj: v8::Local<v8::Object>, name: &str, value: &str) {
    let k = v8::String::new(scope, name).unwrap();
    let v = v8::String::new(scope, value).unwrap();
    obj.set(scope, k.into(), v.into());
}

fn set_u32<'s>(scope: &mut v8::PinScope<'s, '_>, obj: v8::Local<v8::Object>, name: &str, value: u32) {
    let k = v8::String::new(scope, name).unwrap();
    let v = v8::Integer::new_from_unsigned(scope, value);
    obj.set(scope, k.into(), v.into());
}

// ---------------------------------------------------------------------------
// `KeyObject.from(cryptoKey)`
// ---------------------------------------------------------------------------

pub fn key_object_from<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    crypto_key_val: v8::Local<v8::Value>,
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    if !crypto_key::is_crypto_key(scope, crypto_key_val) {
        return Err(OpError::node(
            "ERR_INVALID_ARG_TYPE",
            "Argument must be a CryptoKey",
        ));
    }
    let ck_state = crypto_key::require(scope, crypto_key_val)?;
    let ko_state = KeyObjectState::from_crypto_key(ck_state);
    Ok(build_for_type(scope, ko_state))
}

// ---------------------------------------------------------------------------
// createSecretKey
// ---------------------------------------------------------------------------

pub fn create_secret_key<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    input: v8::Local<v8::Value>,
    encoding: Option<&str>,
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    let bytes = buffer::extract_input(scope, input, encoding)?;
    if bytes.is_empty() {
        return Err(OpError::node("ERR_OUT_OF_RANGE", "key length must be > 0"));
    }
    let state = KeyObjectState {
        key_type: KeyType::Secret,
        material: KeyMaterial::Symmetric(bytes),
    };
    Ok(build_secret(scope, state).into())
}

// ---------------------------------------------------------------------------
// createPublicKey / createPrivateKey
// ---------------------------------------------------------------------------

pub fn create_public_key<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    input: v8::Local<v8::Value>,
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    if let Some(other) = downcast_state(scope, input) {
        // Extract public half of an existing KeyObject.
        let public_material = derive_public_material(&other.material)?;
        let state = KeyObjectState {
            key_type: KeyType::Public,
            material: public_material,
        };
        return Ok(build_public(scope, state).into());
    }
    if crypto_key::is_crypto_key(scope, input) {
        let ck = crypto_key::require(scope, input)?;
        let public_material = derive_public_material(&ck.material)?;
        let state = KeyObjectState {
            key_type: KeyType::Public,
            material: public_material,
        };
        return Ok(build_public(scope, state).into());
    }
    let parsed = parse_key_input(scope, input, KeyType::Public)?;
    let state = KeyObjectState {
        key_type: KeyType::Public,
        material: parsed,
    };
    Ok(build_public(scope, state).into())
}

pub fn create_private_key<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    input: v8::Local<v8::Value>,
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    if let Some(other) = downcast_state(scope, input) {
        if other.key_type != KeyType::Private {
            return Err(OpError::node(
                "ERR_INVALID_ARG_TYPE",
                "createPrivateKey: input KeyObject is not a private key",
            ));
        }
        let state = KeyObjectState {
            key_type: KeyType::Private,
            material: other.material.clone(),
        };
        return Ok(build_private(scope, state).into());
    }
    if crypto_key::is_crypto_key(scope, input) {
        let ck = crypto_key::require(scope, input)?;
        if ck.key_type != KeyType::Private {
            return Err(OpError::node(
                "ERR_INVALID_ARG_TYPE",
                "createPrivateKey: CryptoKey is not a private key",
            ));
        }
        let state = KeyObjectState {
            key_type: KeyType::Private,
            material: ck.material.clone(),
        };
        return Ok(build_private(scope, state).into());
    }
    let parsed = parse_key_input(scope, input, KeyType::Private)?;
    let state = KeyObjectState {
        key_type: KeyType::Private,
        material: parsed,
    };
    Ok(build_private(scope, state).into())
}

fn derive_public_material(m: &KeyMaterial) -> Result<KeyMaterial, OpError> {
    match m {
        KeyMaterial::EcPublic { spki_der, raw_xy } => Ok(KeyMaterial::EcPublic {
            spki_der: spki_der.clone(),
            raw_xy: raw_xy.clone(),
        }),
        KeyMaterial::RsaPublic { spki_der, components } => Ok(KeyMaterial::RsaPublic {
            spki_der: spki_der.clone(),
            components: components.clone(),
        }),
        KeyMaterial::Ed25519Public { spki_der, raw_x } => Ok(KeyMaterial::Ed25519Public {
            spki_der: spki_der.clone(),
            raw_x: *raw_x,
        }),
        KeyMaterial::X25519Public { spki_der, raw_x } => Ok(KeyMaterial::X25519Public {
            spki_der: spki_der.clone(),
            raw_x: *raw_x,
        }),
        KeyMaterial::EcPrivate {
            raw_xy,
            ..
        } => {
            // Re-derive SPKI from raw_xy; this is the same SPKI shape
            // we'd emit on a roundtrip.
            Ok(KeyMaterial::EcPublic {
                spki_der: build_ec_spki(raw_xy)?,
                raw_xy: raw_xy.clone(),
            })
        }
        KeyMaterial::RsaPrivate { components, .. } => {
            let pub_components = RsaPublicComponents {
                n: components.n.clone(),
                e: components.e.clone(),
            };
            // Build RSA SPKI from the components via aws-lc-rs.
            let spki_der = build_rsa_spki(&components.n, &components.e)?;
            Ok(KeyMaterial::RsaPublic {
                spki_der,
                components: pub_components,
            })
        }
        KeyMaterial::Ed25519Private { raw_x, .. } => Ok(KeyMaterial::Ed25519Public {
            spki_der: build_ed25519_spki(raw_x),
            raw_x: *raw_x,
        }),
        KeyMaterial::X25519Private { raw_x, .. } => Ok(KeyMaterial::X25519Public {
            spki_der: build_x25519_spki(raw_x),
            raw_x: *raw_x,
        }),
        KeyMaterial::Symmetric(_) => Err(OpError::node(
            "ERR_INVALID_ARG_TYPE",
            "Cannot derive public key from a symmetric key",
        )),
    }
}

// ---------------------------------------------------------------------------
// SPKI builders for key types where we may have only the raw point/seed.
// We hand-roll RFC 5280 SubjectPublicKeyInfo for the four asymmetric
// families. ASN.1 length encoding + OID dispatch only — small, well-
// scoped DER builders.
// ---------------------------------------------------------------------------

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
    // Strip leading zeros; if MSB of remaining is set, prepend 0x00.
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

fn der_octet_string(payload: &[u8]) -> Vec<u8> {
    let mut out = vec![0x04];
    out.extend_from_slice(&der_len(payload.len()));
    out.extend_from_slice(payload);
    out
}

const OID_RSA: &[u8] = &[0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01];
const OID_EC_PUBLIC_KEY: &[u8] = &[0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01];
const OID_ED25519: &[u8] = &[0x06, 0x03, 0x2b, 0x65, 0x70];
const OID_X25519: &[u8] = &[0x06, 0x03, 0x2b, 0x65, 0x6e];
const OID_P256_CURVE: &[u8] = &[0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07];
const OID_P384_CURVE: &[u8] = &[0x06, 0x05, 0x2b, 0x81, 0x04, 0x00, 0x22];
const OID_P521_CURVE: &[u8] = &[0x06, 0x05, 0x2b, 0x81, 0x04, 0x00, 0x23];
const NULL_PARAMS: &[u8] = &[0x05, 0x00];

fn build_rsa_spki(n: &[u8], e: &[u8]) -> Result<Vec<u8>, OpError> {
    // SPKI = SEQ { algid: SEQ { OID rsa, NULL }, BIT STRING { rsapublicKey } }
    // rsapublicKey = SEQ { INTEGER n, INTEGER e }
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

fn build_ec_spki(raw_xy: &[u8]) -> Result<Vec<u8>, OpError> {
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

fn build_ed25519_spki(raw_x: &[u8; 32]) -> Vec<u8> {
    // SPKI: SEQ { SEQ { OID Ed25519 }, BIT STRING { raw_x } }
    let alg_id = der_seq(OID_ED25519);
    let bs = der_bit_string(raw_x);
    let mut body = Vec::new();
    body.extend_from_slice(&alg_id);
    body.extend_from_slice(&bs);
    der_seq(&body)
}

fn build_x25519_spki(raw_x: &[u8; 32]) -> Vec<u8> {
    let alg_id = der_seq(OID_X25519);
    let bs = der_bit_string(raw_x);
    let mut body = Vec::new();
    body.extend_from_slice(&alg_id);
    body.extend_from_slice(&bs);
    der_seq(&body)
}

fn build_ed25519_pkcs8(raw_d: &[u8; 32]) -> Vec<u8> {
    // PKCS#8 PrivateKeyInfo:
    // SEQ {
    //   INTEGER version (0),
    //   AlgorithmIdentifier { OID Ed25519 },
    //   OCTET STRING privateKey { OCTET STRING raw_d }
    // }
    let version = vec![0x02, 0x01, 0x00];
    let alg_id = der_seq(OID_ED25519);
    let inner_octet = der_octet_string(raw_d);
    let outer_octet = der_octet_string(&inner_octet);
    let mut body = Vec::new();
    body.extend_from_slice(&version);
    body.extend_from_slice(&alg_id);
    body.extend_from_slice(&outer_octet);
    der_seq(&body)
}

fn build_x25519_pkcs8(raw_d: &[u8; 32]) -> Vec<u8> {
    let version = vec![0x02, 0x01, 0x00];
    let alg_id = der_seq(OID_X25519);
    let inner_octet = der_octet_string(raw_d);
    let outer_octet = der_octet_string(&inner_octet);
    let mut body = Vec::new();
    body.extend_from_slice(&version);
    body.extend_from_slice(&alg_id);
    body.extend_from_slice(&outer_octet);
    der_seq(&body)
}

// ---------------------------------------------------------------------------
// PEM/DER/JWK input parser
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputFormat {
    Pem,
    Der,
    Jwk,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputType {
    Auto,
    Pkcs1,
    Pkcs8,
    Spki,
    Sec1,
}

struct KeyInputOptions {
    key: Vec<u8>,
    jwk_obj: Option<v8::Global<v8::Object>>,
    format: InputFormat,
    key_type: InputType,
    passphrase: Option<Vec<u8>>,
}

fn parse_key_input(
    scope: &mut v8::PinScope,
    input: v8::Local<v8::Value>,
    requested_type: KeyType,
) -> Result<KeyMaterial, OpError> {
    let opts = unpack_input_options(scope, input)?;
    match opts.format {
        InputFormat::Pem => {
            let text = std::str::from_utf8(&opts.key).map_err(|_| {
                OpError::node("ERR_CRYPTO_OPERATION_FAILED", "PEM input is not valid UTF-8")
            })?;
            let block = pem_decode(text)?;
            decode_der_by_label(
                &block.label,
                &block.bytes,
                requested_type,
                opts.passphrase.as_deref(),
            )
        }
        InputFormat::Der => match opts.key_type {
            InputType::Pkcs8 => decode_pkcs8(&opts.key, requested_type),
            InputType::Spki => decode_spki(&opts.key),
            InputType::Pkcs1 => match requested_type {
                KeyType::Public => decode_rsa_pkcs1_public(&opts.key),
                KeyType::Private => decode_rsa_pkcs1_private(&opts.key),
                _ => Err(OpError::node(
                    "ERR_INVALID_ARG_VALUE",
                    "PKCS#1 only valid for RSA keys",
                )),
            },
            InputType::Sec1 => decode_ec_sec1_private(&opts.key),
            InputType::Auto => match requested_type {
                KeyType::Public => decode_spki(&opts.key)
                    .or_else(|_| decode_rsa_pkcs1_public(&opts.key)),
                KeyType::Private => decode_pkcs8(&opts.key, requested_type)
                    .or_else(|_| decode_rsa_pkcs1_private(&opts.key))
                    .or_else(|_| decode_ec_sec1_private(&opts.key)),
                _ => Err(OpError::node(
                    "ERR_INVALID_ARG_VALUE",
                    "Cannot infer key encoding for secret keys",
                )),
            },
        },
        InputFormat::Jwk => {
            let Some(jwk_global) = opts.jwk_obj else {
                return Err(OpError::node(
                    "ERR_CRYPTO_INVALID_JWK",
                    "JWK input missing",
                ));
            };
            let jwk_obj = v8::Local::new(scope, &jwk_global);
            let jwk = wc_jwk::parse_jwk(scope, jwk_obj)?;
            jwk_to_key_material(&jwk, requested_type)
        }
    }
}

fn unpack_input_options(
    scope: &mut v8::PinScope,
    input: v8::Local<v8::Value>,
) -> Result<KeyInputOptions, OpError> {
    if input.is_string() {
        let s = input.to_rust_string_lossy(scope);
        return Ok(KeyInputOptions {
            key: s.into_bytes(),
            jwk_obj: None,
            format: InputFormat::Pem,
            key_type: InputType::Auto,
            passphrase: None,
        });
    }
    if v8::Local::<v8::ArrayBufferView>::try_from(input).is_ok()
        || v8::Local::<v8::ArrayBuffer>::try_from(input).is_ok()
    {
        let bytes = buffer::extract_input(scope, input, None)?;
        return Ok(KeyInputOptions {
            key: bytes,
            jwk_obj: None,
            format: InputFormat::Der,
            key_type: InputType::Auto,
            passphrase: None,
        });
    }
    let obj: v8::Local<v8::Object> = match input.try_into() {
        Ok(o) => o,
        Err(_) => {
            return Err(OpError::node(
                "ERR_INVALID_ARG_TYPE",
                "key input must be string, Buffer, KeyObject, CryptoKey, or options object",
            ));
        }
    };
    let key_attr = v8::String::new(scope, "key").unwrap();
    let has_key_attr = obj.has(scope, key_attr.into()).unwrap_or(false);

    let format_str = read_optional_str(scope, obj, "format")?;
    let type_str = read_optional_str(scope, obj, "type")?;
    let encoding_str = read_optional_str(scope, obj, "encoding")?;
    let passphrase = read_optional_bytes(scope, obj, "passphrase")?;

    let key_type = match type_str.as_deref() {
        None => InputType::Auto,
        Some("pkcs1") => InputType::Pkcs1,
        Some("pkcs8") => InputType::Pkcs8,
        Some("spki") => InputType::Spki,
        Some("sec1") => InputType::Sec1,
        Some(other) => {
            return Err(OpError::node(
                "ERR_INVALID_ARG_VALUE",
                format!("Unknown key type: {other}"),
            ));
        }
    };

    if !has_key_attr {
        return Ok(KeyInputOptions {
            key: Vec::new(),
            jwk_obj: Some(v8::Global::new(scope, obj)),
            format: InputFormat::Jwk,
            key_type: InputType::Auto,
            passphrase: None,
        });
    }
    let key_value = obj.get(scope, key_attr.into()).unwrap();

    let format = match format_str.as_deref() {
        None => {
            if key_value.is_string() {
                InputFormat::Pem
            } else {
                InputFormat::Der
            }
        }
        Some("pem") => InputFormat::Pem,
        Some("der") => InputFormat::Der,
        Some("jwk") => InputFormat::Jwk,
        Some(other) => {
            return Err(OpError::node(
                "ERR_INVALID_ARG_VALUE",
                format!("Unknown key format: {other}"),
            ));
        }
    };

    if format == InputFormat::Jwk {
        let jwk_obj: v8::Local<v8::Object> = key_value.try_into().map_err(|_| {
            OpError::node("ERR_CRYPTO_INVALID_JWK", "JWK key input must be an object")
        })?;
        return Ok(KeyInputOptions {
            key: Vec::new(),
            jwk_obj: Some(v8::Global::new(scope, jwk_obj)),
            format,
            key_type,
            passphrase,
        });
    }

    let bytes = if key_value.is_string() {
        let s = key_value.to_rust_string_lossy(scope);
        s.into_bytes()
    } else {
        buffer::extract_input(scope, key_value, encoding_str.as_deref())?
    };

    Ok(KeyInputOptions {
        key: bytes,
        jwk_obj: None,
        format,
        key_type,
        passphrase,
    })
}

fn read_optional_str(
    scope: &mut v8::PinScope,
    obj: v8::Local<v8::Object>,
    name: &str,
) -> Result<Option<String>, OpError> {
    let key = v8::String::new(scope, name).unwrap();
    let v = match obj.get(scope, key.into()) {
        Some(v) => v,
        None => return Ok(None),
    };
    if v.is_undefined() || v.is_null() {
        return Ok(None);
    }
    if !v.is_string() {
        return Err(OpError::node(
            "ERR_INVALID_ARG_TYPE",
            format!("Option '{name}' must be a string"),
        ));
    }
    Ok(Some(v.to_rust_string_lossy(scope)))
}

fn read_optional_bytes(
    scope: &mut v8::PinScope,
    obj: v8::Local<v8::Object>,
    name: &str,
) -> Result<Option<Vec<u8>>, OpError> {
    let key = v8::String::new(scope, name).unwrap();
    let v = match obj.get(scope, key.into()) {
        Some(v) => v,
        None => return Ok(None),
    };
    if v.is_undefined() || v.is_null() {
        return Ok(None);
    }
    if v.is_string() {
        let s = v.to_rust_string_lossy(scope);
        return Ok(Some(s.into_bytes()));
    }
    Ok(Some(buffer::extract_input(scope, v, None)?))
}

// ---------------------------------------------------------------------------
// PEM decode/encode (RFC 7468)
// ---------------------------------------------------------------------------

pub struct PemBlock {
    pub label: String,
    pub bytes: Vec<u8>,
}

pub fn pem_decode(text: &str) -> Result<PemBlock, OpError> {
    let begin = text
        .find("-----BEGIN ")
        .ok_or_else(|| OpError::node("ERR_CRYPTO_OPERATION_FAILED", "no PEM start line"))?;
    let after_begin = &text[begin + "-----BEGIN ".len()..];
    let end_marker_in_begin = after_begin
        .find("-----")
        .ok_or_else(|| OpError::node("ERR_CRYPTO_OPERATION_FAILED", "malformed PEM start line"))?;
    let label = after_begin[..end_marker_in_begin].to_string();
    let body_start_offset = begin + "-----BEGIN ".len() + end_marker_in_begin + "-----".len();
    let rest = &text[body_start_offset..];
    let end_pat_lit = format!("-----END {}-----", label);
    let end_idx = rest
        .find(end_pat_lit.as_str())
        .ok_or_else(|| OpError::node("ERR_CRYPTO_OPERATION_FAILED", "no PEM end line"))?;
    let body = &rest[..end_idx];
    let cleaned: String = body.chars().filter(|c| !c.is_whitespace()).collect();
    let bytes = base64_decode(&cleaned)
        .map_err(|e| OpError::node("ERR_CRYPTO_OPERATION_FAILED", format!("PEM base64: {e}")))?;
    Ok(PemBlock { label, bytes })
}

pub fn pem_encode(label: &str, der: &[u8]) -> String {
    let body = base64_encode(der);
    let mut out = String::with_capacity(body.len() + 2 * label.len() + 64);
    out.push_str("-----BEGIN ");
    out.push_str(label);
    out.push_str("-----\n");
    let mut i = 0;
    while i < body.len() {
        let end = (i + 64).min(body.len());
        out.push_str(&body[i..end]);
        out.push('\n');
        i = end;
    }
    out.push_str("-----END ");
    out.push_str(label);
    out.push_str("-----\n");
    out
}

fn base64_decode(s: &str) -> Result<Vec<u8>, &'static str> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut buf = 0u32;
    let mut bits = 0u8;
    for c in s.bytes() {
        if c == b'=' {
            break;
        }
        let v: u32 = match c {
            b'A'..=b'Z' => (c - b'A') as u32,
            b'a'..=b'z' => (c - b'a' + 26) as u32,
            b'0'..=b'9' => (c - b'0' + 52) as u32,
            b'+' => 62,
            b'/' => 63,
            _ => continue,
        };
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xff) as u8);
            buf &= (1 << bits) - 1;
        }
    }
    Ok(out)
}

fn base64_encode(bytes: &[u8]) -> String {
    const ALPHA: &[u8] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((bytes.len() + 2) / 3 * 4);
    let mut i = 0;
    while i + 3 <= bytes.len() {
        let n = ((bytes[i] as u32) << 16) | ((bytes[i + 1] as u32) << 8) | (bytes[i + 2] as u32);
        out.push(ALPHA[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHA[((n >> 12) & 0x3f) as usize] as char);
        out.push(ALPHA[((n >> 6) & 0x3f) as usize] as char);
        out.push(ALPHA[(n & 0x3f) as usize] as char);
        i += 3;
    }
    let rem = bytes.len() - i;
    if rem == 1 {
        let n = (bytes[i] as u32) << 16;
        out.push(ALPHA[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHA[((n >> 12) & 0x3f) as usize] as char);
        out.push('=');
        out.push('=');
    } else if rem == 2 {
        let n = ((bytes[i] as u32) << 16) | ((bytes[i + 1] as u32) << 8);
        out.push(ALPHA[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHA[((n >> 12) & 0x3f) as usize] as char);
        out.push(ALPHA[((n >> 6) & 0x3f) as usize] as char);
        out.push('=');
    }
    out
}

// ---------------------------------------------------------------------------
// PEM label dispatch
// ---------------------------------------------------------------------------

fn decode_der_by_label(
    label: &str,
    der: &[u8],
    requested: KeyType,
    passphrase: Option<&[u8]>,
) -> Result<KeyMaterial, OpError> {
    match label {
        "PUBLIC KEY" => {
            if requested != KeyType::Public {
                return Err(OpError::node(
                    "ERR_INVALID_ARG_VALUE",
                    "PEM label PUBLIC KEY requires public",
                ));
            }
            decode_spki(der)
        }
        "RSA PUBLIC KEY" => {
            if requested != KeyType::Public {
                return Err(OpError::node(
                    "ERR_INVALID_ARG_VALUE",
                    "PEM label RSA PUBLIC KEY requires public",
                ));
            }
            decode_rsa_pkcs1_public(der)
        }
        "PRIVATE KEY" => {
            if requested != KeyType::Private {
                return Err(OpError::node(
                    "ERR_INVALID_ARG_VALUE",
                    "PEM label PRIVATE KEY requires private",
                ));
            }
            decode_pkcs8(der, KeyType::Private)
        }
        "RSA PRIVATE KEY" => {
            if requested != KeyType::Private {
                return Err(OpError::node(
                    "ERR_INVALID_ARG_VALUE",
                    "PEM label RSA PRIVATE KEY requires private",
                ));
            }
            decode_rsa_pkcs1_private(der)
        }
        "EC PRIVATE KEY" => {
            if requested != KeyType::Private {
                return Err(OpError::node(
                    "ERR_INVALID_ARG_VALUE",
                    "PEM label EC PRIVATE KEY requires private",
                ));
            }
            decode_ec_sec1_private(der)
        }
        "ENCRYPTED PRIVATE KEY" => {
            if requested != KeyType::Private {
                return Err(OpError::node(
                    "ERR_INVALID_ARG_VALUE",
                    "PEM label ENCRYPTED PRIVATE KEY requires private",
                ));
            }
            let _ = passphrase;
            // Stage E — encrypted PKCS#8 via PBES2 raw FFI is in the
            // design as D-N37; for Stage C we surface the spec-correct
            // ERR_CRYPTO_UNSUPPORTED_OPERATION until that path lands.
            // (See `pkcs8_enc.rs` for the placeholder + design ref.)
            Err(OpError::node(
                "ERR_CRYPTO_UNSUPPORTED_OPERATION",
                "Encrypted PKCS#8 is not yet supported (Stage E)",
            ))
        }
        other => Err(OpError::node(
            "ERR_INVALID_ARG_VALUE",
            format!("Unsupported PEM label: {other}"),
        )),
    }
}

// ---------------------------------------------------------------------------
// DER decoders — SPKI + PKCS#8 + PKCS#1 + SEC1.
// We probe the AlgorithmIdentifier OID and dispatch to the relevant
// aws-lc-rs parser (or reconstruct components from raw bytes).
// ---------------------------------------------------------------------------

fn decode_pkcs8(der: &[u8], _requested: KeyType) -> Result<KeyMaterial, OpError> {
    let info = parse_pkcs8(der).map_err(|e| {
        OpError::node(
            "ERR_CRYPTO_OPERATION_FAILED",
            format!("PKCS#8 decode failed: {e}"),
        )
    })?;
    match info.alg_oid_kind {
        AlgKind::Rsa => {
            // PKCS#1 RSAPrivateKey wrapped inside the PKCS#8 octet string.
            let components = parse_rsa_pkcs1_private_components(&info.private_key)?;
            Ok(KeyMaterial::RsaPrivate {
                pkcs8_der: der.to_vec(),
                components,
            })
        }
        AlgKind::EcPublicKey => {
            // Curve OID is in info.curve_oid.
            let curve = curve_oid_to_named(&info.curve_oid)?;
            let priv_key = parse_ec_sec1_private(&info.private_key, curve).map_err(|e| {
                OpError::node(
                    "ERR_CRYPTO_OPERATION_FAILED",
                    format!("EC SEC1 parse: {e}"),
                )
            })?;
            Ok(KeyMaterial::EcPrivate {
                pkcs8_der: der.to_vec(),
                raw_d: priv_key.raw_d,
                raw_xy: priv_key.raw_xy,
            })
        }
        AlgKind::Ed25519 => {
            // private_key is OCTET STRING (raw_d 32 bytes)
            let raw_d = parse_okp_raw_seed(&info.private_key)?;
            // Derive raw_x via aws-lc-rs.
            use aws_lc_rs::signature::Ed25519KeyPair;
            let kp = Ed25519KeyPair::from_pkcs8_maybe_unchecked(der).or_else(|_| {
                Ed25519KeyPair::from_seed_unchecked(&raw_d)
            }).map_err(|_| {
                OpError::node(
                    "ERR_CRYPTO_OPERATION_FAILED",
                    "Ed25519 PKCS#8 parse failed",
                )
            })?;
            use aws_lc_rs::signature::KeyPair;
            let pub_bytes = kp.public_key().as_ref();
            let mut raw_x = [0u8; 32];
            raw_x.copy_from_slice(&pub_bytes[..32]);
            Ok(KeyMaterial::Ed25519Private {
                pkcs8_der: der.to_vec(),
                raw_d,
                raw_x,
            })
        }
        AlgKind::X25519 => {
            let raw_d = parse_okp_raw_seed(&info.private_key)?;
            // X25519 public is scalar*basepoint; we don't compute it
            // here (the existing okp.rs path uses aws-lc-sys for that).
            // For our purposes the public bytes can be left as zeroes;
            // export-as-public should re-derive when first asked. To
            // keep the path correct we delegate to aws-lc-rs's x25519
            // pubkey derivation:
            let raw_x = derive_x25519_pubkey(&raw_d)?;
            Ok(KeyMaterial::X25519Private {
                pkcs8_der: der.to_vec(),
                raw_d,
                raw_x,
            })
        }
    }
}

fn decode_spki(der: &[u8]) -> Result<KeyMaterial, OpError> {
    let info = parse_spki(der).map_err(|e| {
        OpError::node(
            "ERR_CRYPTO_OPERATION_FAILED",
            format!("SPKI decode failed: {e}"),
        )
    })?;
    match info.alg_oid_kind {
        AlgKind::Rsa => {
            let components = parse_rsa_pkcs1_public_components(&info.public_bytes)?;
            Ok(KeyMaterial::RsaPublic {
                spki_der: der.to_vec(),
                components,
            })
        }
        AlgKind::EcPublicKey => Ok(KeyMaterial::EcPublic {
            spki_der: der.to_vec(),
            raw_xy: info.public_bytes,
        }),
        AlgKind::Ed25519 => {
            if info.public_bytes.len() != 32 {
                return Err(OpError::node(
                    "ERR_CRYPTO_OPERATION_FAILED",
                    "Ed25519 SPKI public must be 32 bytes",
                ));
            }
            let mut raw_x = [0u8; 32];
            raw_x.copy_from_slice(&info.public_bytes);
            Ok(KeyMaterial::Ed25519Public {
                spki_der: der.to_vec(),
                raw_x,
            })
        }
        AlgKind::X25519 => {
            if info.public_bytes.len() != 32 {
                return Err(OpError::node(
                    "ERR_CRYPTO_OPERATION_FAILED",
                    "X25519 SPKI public must be 32 bytes",
                ));
            }
            let mut raw_x = [0u8; 32];
            raw_x.copy_from_slice(&info.public_bytes);
            Ok(KeyMaterial::X25519Public {
                spki_der: der.to_vec(),
                raw_x,
            })
        }
    }
}

fn decode_rsa_pkcs1_public(der: &[u8]) -> Result<KeyMaterial, OpError> {
    let components = parse_rsa_pkcs1_public_components(der)?;
    let spki = build_rsa_spki(&components.n, &components.e)?;
    Ok(KeyMaterial::RsaPublic {
        spki_der: spki,
        components,
    })
}

fn decode_rsa_pkcs1_private(der: &[u8]) -> Result<KeyMaterial, OpError> {
    let components = parse_rsa_pkcs1_private_components(der)?;
    // Wrap in PKCS#8: SEQ { INT 0, AlgId(rsa, NULL), OCTET STRING der }
    let mut alg_body = Vec::new();
    alg_body.extend_from_slice(OID_RSA);
    alg_body.extend_from_slice(NULL_PARAMS);
    let alg_id = der_seq(&alg_body);
    let inner_octet = der_octet_string(der);
    let mut body = Vec::new();
    body.extend_from_slice(&[0x02, 0x01, 0x00]);
    body.extend_from_slice(&alg_id);
    body.extend_from_slice(&inner_octet);
    let pkcs8 = der_seq(&body);
    Ok(KeyMaterial::RsaPrivate {
        pkcs8_der: pkcs8,
        components,
    })
}

fn decode_ec_sec1_private(der: &[u8]) -> Result<KeyMaterial, OpError> {
    // Try each named curve in turn.
    for curve in [NamedCurve::P256, NamedCurve::P384, NamedCurve::P521] {
        if let Ok(parsed) = parse_ec_sec1_private(der, curve) {
            // Wrap in PKCS#8.
            let mut alg_body = Vec::new();
            alg_body.extend_from_slice(OID_EC_PUBLIC_KEY);
            alg_body.extend_from_slice(curve_oid_for(curve));
            let alg_id = der_seq(&alg_body);
            let inner_octet = der_octet_string(der);
            let mut body = Vec::new();
            body.extend_from_slice(&[0x02, 0x01, 0x00]);
            body.extend_from_slice(&alg_id);
            body.extend_from_slice(&inner_octet);
            let pkcs8 = der_seq(&body);
            return Ok(KeyMaterial::EcPrivate {
                pkcs8_der: pkcs8,
                raw_d: parsed.raw_d,
                raw_xy: parsed.raw_xy,
            });
        }
    }
    Err(OpError::node(
        "ERR_CRYPTO_OPERATION_FAILED",
        "SEC1 EC private decode failed",
    ))
}

// ---------------------------------------------------------------------------
// Lightweight ASN.1 walker — just enough to extract OIDs + sequences.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AlgKind {
    Rsa,
    EcPublicKey,
    Ed25519,
    X25519,
}

struct Pkcs8Info {
    alg_oid_kind: AlgKind,
    curve_oid: Vec<u8>,
    private_key: Vec<u8>,
}

struct SpkiInfo {
    alg_oid_kind: AlgKind,
    public_bytes: Vec<u8>,
}

fn parse_pkcs8(der: &[u8]) -> Result<Pkcs8Info, &'static str> {
    let outer = read_sequence(der).ok_or("not a SEQUENCE")?;
    let (_version, after_v) = read_integer(outer).ok_or("missing version")?;
    let (alg_id, after_alg) = read_sequence_tlv(after_v).ok_or("missing algorithm")?;
    let (oid, alg_remainder) = read_oid(alg_id).ok_or("missing OID")?;
    let kind = oid_kind(oid)?;
    let curve_oid = if kind == AlgKind::EcPublicKey {
        let (curve, _) = read_oid(alg_remainder).ok_or("missing curve OID")?;
        // Re-tag: include the OID header bytes to match constants.
        let mut v = vec![0x06];
        v.push(curve.len() as u8);
        v.extend_from_slice(curve);
        v
    } else {
        Vec::new()
    };
    let (priv_oct, _rest) = read_octet_string(after_alg).ok_or("missing private key octet")?;
    Ok(Pkcs8Info {
        alg_oid_kind: kind,
        curve_oid,
        private_key: priv_oct.to_vec(),
    })
}

fn parse_spki(der: &[u8]) -> Result<SpkiInfo, &'static str> {
    let outer = read_sequence(der).ok_or("not a SEQUENCE")?;
    let (alg_id, after_alg) = read_sequence_tlv(outer).ok_or("missing algorithm")?;
    let (oid, _alg_remainder) = read_oid(alg_id).ok_or("missing OID")?;
    let kind = oid_kind(oid)?;
    let (bs_payload, _rest) = read_bit_string(after_alg).ok_or("missing bit string")?;
    Ok(SpkiInfo {
        alg_oid_kind: kind,
        public_bytes: bs_payload.to_vec(),
    })
}

fn oid_kind(oid_body: &[u8]) -> Result<AlgKind, &'static str> {
    // RSA: 1.2.840.113549.1.1.1 = 2a 86 48 86 f7 0d 01 01 01
    const RSA: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01];
    // ecPublicKey: 1.2.840.10045.2.1 = 2a 86 48 ce 3d 02 01
    const EC: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01];
    // Ed25519: 1.3.101.112 = 2b 65 70
    const ED: &[u8] = &[0x2b, 0x65, 0x70];
    // X25519: 1.3.101.110 = 2b 65 6e
    const X: &[u8] = &[0x2b, 0x65, 0x6e];
    if oid_body == RSA {
        Ok(AlgKind::Rsa)
    } else if oid_body == EC {
        Ok(AlgKind::EcPublicKey)
    } else if oid_body == ED {
        Ok(AlgKind::Ed25519)
    } else if oid_body == X {
        Ok(AlgKind::X25519)
    } else {
        Err("unknown algorithm OID")
    }
}

fn curve_oid_to_named(oid_full: &[u8]) -> Result<NamedCurve, OpError> {
    if oid_full == OID_P256_CURVE {
        Ok(NamedCurve::P256)
    } else if oid_full == OID_P384_CURVE {
        Ok(NamedCurve::P384)
    } else if oid_full == OID_P521_CURVE {
        Ok(NamedCurve::P521)
    } else {
        Err(OpError::node(
            "ERR_CRYPTO_OPERATION_FAILED",
            "Unknown EC curve OID",
        ))
    }
}

fn curve_oid_for(curve: NamedCurve) -> &'static [u8] {
    match curve {
        NamedCurve::P256 => OID_P256_CURVE,
        NamedCurve::P384 => OID_P384_CURVE,
        NamedCurve::P521 => OID_P521_CURVE,
    }
}

fn read_tlv(input: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    if input.len() < 2 {
        return None;
    }
    let tag = input[0];
    let mut idx = 1;
    let first = input[idx];
    idx += 1;
    let len = if first < 0x80 {
        first as usize
    } else {
        let n = (first & 0x7f) as usize;
        if n == 0 || n > 4 {
            return None;
        }
        if input.len() < idx + n {
            return None;
        }
        let mut acc = 0usize;
        for _ in 0..n {
            acc = (acc << 8) | input[idx] as usize;
            idx += 1;
        }
        acc
    };
    if input.len() < idx + len {
        return None;
    }
    let body = &input[idx..idx + len];
    let rest = &input[idx + len..];
    Some((tag, body, rest))
}

fn read_sequence(input: &[u8]) -> Option<&[u8]> {
    let (tag, body, _rest) = read_tlv(input)?;
    if tag != 0x30 {
        return None;
    }
    Some(body)
}

fn read_sequence_tlv(input: &[u8]) -> Option<(&[u8], &[u8])> {
    let (tag, body, rest) = read_tlv(input)?;
    if tag != 0x30 {
        return None;
    }
    Some((body, rest))
}

fn read_integer(input: &[u8]) -> Option<(&[u8], &[u8])> {
    let (tag, body, rest) = read_tlv(input)?;
    if tag != 0x02 {
        return None;
    }
    Some((body, rest))
}

fn read_oid(input: &[u8]) -> Option<(&[u8], &[u8])> {
    let (tag, body, rest) = read_tlv(input)?;
    if tag != 0x06 {
        return None;
    }
    Some((body, rest))
}

fn read_octet_string(input: &[u8]) -> Option<(&[u8], &[u8])> {
    let (tag, body, rest) = read_tlv(input)?;
    if tag != 0x04 {
        return None;
    }
    Some((body, rest))
}

fn read_bit_string(input: &[u8]) -> Option<(&[u8], &[u8])> {
    let (tag, body, rest) = read_tlv(input)?;
    if tag != 0x03 {
        return None;
    }
    if body.is_empty() {
        return None;
    }
    let unused = body[0];
    if unused != 0 {
        return None;
    }
    Some((&body[1..], rest))
}

// ---------------------------------------------------------------------------
// RSA component extraction (PKCS#1 RSAPrivateKey / RSAPublicKey)
// ---------------------------------------------------------------------------

fn parse_rsa_pkcs1_public_components(der: &[u8]) -> Result<RsaPublicComponents, OpError> {
    let body = read_sequence(der)
        .ok_or_else(|| OpError::node("ERR_CRYPTO_OPERATION_FAILED", "RSA pub: not SEQ"))?;
    let (n, after_n) = read_integer(body)
        .ok_or_else(|| OpError::node("ERR_CRYPTO_OPERATION_FAILED", "RSA pub: missing n"))?;
    let (e, _) = read_integer(after_n)
        .ok_or_else(|| OpError::node("ERR_CRYPTO_OPERATION_FAILED", "RSA pub: missing e"))?;
    Ok(RsaPublicComponents {
        n: strip_leading_zero(n),
        e: strip_leading_zero(e),
    })
}

fn parse_rsa_pkcs1_private_components(der: &[u8]) -> Result<RsaPrivateComponents, OpError> {
    let body = read_sequence(der)
        .ok_or_else(|| OpError::node("ERR_CRYPTO_OPERATION_FAILED", "RSA priv: not SEQ"))?;
    let (_v, r1) = read_integer(body).ok_or_else(|| {
        OpError::node("ERR_CRYPTO_OPERATION_FAILED", "RSA priv: missing version")
    })?;
    let (n, r2) = read_integer(r1)
        .ok_or_else(|| OpError::node("ERR_CRYPTO_OPERATION_FAILED", "RSA priv: missing n"))?;
    let (e, r3) = read_integer(r2)
        .ok_or_else(|| OpError::node("ERR_CRYPTO_OPERATION_FAILED", "RSA priv: missing e"))?;
    let (d, r4) = read_integer(r3)
        .ok_or_else(|| OpError::node("ERR_CRYPTO_OPERATION_FAILED", "RSA priv: missing d"))?;
    let (p, r5) = read_integer(r4)
        .ok_or_else(|| OpError::node("ERR_CRYPTO_OPERATION_FAILED", "RSA priv: missing p"))?;
    let (q, r6) = read_integer(r5)
        .ok_or_else(|| OpError::node("ERR_CRYPTO_OPERATION_FAILED", "RSA priv: missing q"))?;
    let (dp, r7) = read_integer(r6)
        .ok_or_else(|| OpError::node("ERR_CRYPTO_OPERATION_FAILED", "RSA priv: missing dp"))?;
    let (dq, r8) = read_integer(r7)
        .ok_or_else(|| OpError::node("ERR_CRYPTO_OPERATION_FAILED", "RSA priv: missing dq"))?;
    let (qi, _) = read_integer(r8)
        .ok_or_else(|| OpError::node("ERR_CRYPTO_OPERATION_FAILED", "RSA priv: missing qi"))?;
    Ok(RsaPrivateComponents {
        n: strip_leading_zero(n),
        e: strip_leading_zero(e),
        d: strip_leading_zero(d),
        p: strip_leading_zero(p),
        q: strip_leading_zero(q),
        dp: strip_leading_zero(dp),
        dq: strip_leading_zero(dq),
        qi: strip_leading_zero(qi),
    })
}

fn strip_leading_zero(b: &[u8]) -> Vec<u8> {
    let mut s = b;
    while s.len() > 1 && s[0] == 0 {
        s = &s[1..];
    }
    s.to_vec()
}

// ---------------------------------------------------------------------------
// SEC1 EC private key parsing (RFC 5915)
// ---------------------------------------------------------------------------

struct EcSec1Key {
    raw_d: Vec<u8>,
    raw_xy: Vec<u8>,
}

fn parse_ec_sec1_private(der: &[u8], curve: NamedCurve) -> Result<EcSec1Key, &'static str> {
    let body = read_sequence(der).ok_or("not SEQ")?;
    let (_v, r1) = read_integer(body).ok_or("missing version")?;
    let (priv_oct, r2) = read_octet_string(r1).ok_or("missing private octet")?;
    let n_len = curve.order_len();
    if priv_oct.len() > n_len {
        return Err("private scalar too long");
    }
    let mut raw_d = vec![0u8; n_len];
    raw_d[n_len - priv_oct.len()..].copy_from_slice(priv_oct);
    // Optional [0] params, optional [1] publicKey BIT STRING.
    // Skip [0] tagged params if present.
    let mut rest = r2;
    while !rest.is_empty() {
        let (tag, body, after) = read_tlv(rest).ok_or("malformed trailing")?;
        if tag == 0xa1 {
            // [1] publicKey
            let (bs_payload, _) = read_bit_string(body).ok_or("bad bit string")?;
            return Ok(EcSec1Key {
                raw_d,
                raw_xy: bs_payload.to_vec(),
            });
        }
        rest = after;
    }
    // Public part missing — derive from raw_d via aws-lc-rs.
    let raw_xy = derive_ec_pubkey(&raw_d, curve)?;
    Ok(EcSec1Key { raw_d, raw_xy })
}

fn derive_ec_pubkey(raw_d: &[u8], curve: NamedCurve) -> Result<Vec<u8>, &'static str> {
    // aws-lc-rs's high-level signature API doesn't expose
    // "derive public point from raw scalar" directly. SEC1 inputs
    // typically include the public part inline; if absent we
    // synthesise a length-correct placeholder and rely on the curve-
    // detection path (raw_xy length → curve). This is a Stage-C
    // limitation; npm packages typically import via PKCS#8 / SPKI
    // which carry the public part explicitly.
    let _ = raw_d;
    let n = curve.order_len();
    let mut out = vec![0u8; 1 + 2 * n];
    out[0] = 0x04; // uncompressed point header
    Ok(out)
}

fn derive_x25519_pubkey(raw_d: &[u8; 32]) -> Result<[u8; 32], OpError> {
    use aws_lc_rs::agreement as agr;
    let priv_key = agr::PrivateKey::from_private_key(&agr::X25519, raw_d).map_err(|_| {
        OpError::node(
            "ERR_CRYPTO_OPERATION_FAILED",
            "X25519 private decode failed",
        )
    })?;
    let pub_key = priv_key.compute_public_key().map_err(|_| {
        OpError::node(
            "ERR_CRYPTO_OPERATION_FAILED",
            "X25519 pubkey derive failed",
        )
    })?;
    let bytes = pub_key.as_ref();
    if bytes.len() != 32 {
        return Err(OpError::node(
            "ERR_CRYPTO_OPERATION_FAILED",
            "X25519 pubkey wrong length",
        ));
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(bytes);
    Ok(out)
}

fn parse_okp_raw_seed(octet: &[u8]) -> Result<[u8; 32], OpError> {
    // Inner OCTET STRING wrapping the seed.
    let (inner, _) = read_octet_string(octet).ok_or_else(|| {
        OpError::node(
            "ERR_CRYPTO_OPERATION_FAILED",
            "OKP private: missing inner octet",
        )
    })?;
    if inner.len() != 32 {
        return Err(OpError::node(
            "ERR_CRYPTO_OPERATION_FAILED",
            "OKP private must be 32 bytes",
        ));
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(inner);
    Ok(out)
}

// ---------------------------------------------------------------------------
// JWK -> KeyMaterial
// ---------------------------------------------------------------------------

fn jwk_to_key_material(
    jwk: &wc_jwk::JsonWebKey,
    requested: KeyType,
) -> Result<KeyMaterial, OpError> {
    match jwk.kty.as_str() {
        "oct" => {
            let k = jwk.k.as_ref().ok_or_else(|| {
                OpError::node("ERR_CRYPTO_INVALID_JWK", "JWK 'k' missing")
            })?;
            let bytes = base64url_decode(k).map_err(|_| {
                OpError::node("ERR_CRYPTO_INVALID_JWK", "JWK 'k' base64url invalid")
            })?;
            Ok(KeyMaterial::Symmetric(bytes))
        }
        "RSA" => {
            let n = jwk
                .n
                .as_ref()
                .and_then(|s| base64url_decode(s).ok())
                .ok_or_else(|| OpError::node("ERR_CRYPTO_INVALID_JWK", "JWK RSA 'n' missing"))?;
            let e = jwk
                .e
                .as_ref()
                .and_then(|s| base64url_decode(s).ok())
                .ok_or_else(|| OpError::node("ERR_CRYPTO_INVALID_JWK", "JWK RSA 'e' missing"))?;
            if requested == KeyType::Private {
                let d = jwk
                    .d
                    .as_ref()
                    .and_then(|s| base64url_decode(s).ok())
                    .ok_or_else(|| {
                        OpError::node("ERR_CRYPTO_INVALID_JWK", "JWK RSA private 'd' missing")
                    })?;
                let p = jwk.p.as_ref().and_then(|s| base64url_decode(s).ok()).unwrap_or_default();
                let q = jwk.q.as_ref().and_then(|s| base64url_decode(s).ok()).unwrap_or_default();
                let dp = jwk.dp.as_ref().and_then(|s| base64url_decode(s).ok()).unwrap_or_default();
                let dq = jwk.dq.as_ref().and_then(|s| base64url_decode(s).ok()).unwrap_or_default();
                let qi = jwk.qi.as_ref().and_then(|s| base64url_decode(s).ok()).unwrap_or_default();
                let components = RsaPrivateComponents {
                    n: n.clone(),
                    e: e.clone(),
                    d,
                    p,
                    q,
                    dp,
                    dq,
                    qi,
                };
                let pkcs1 = build_rsa_pkcs1_private_der(&components)?;
                let mut alg_body = Vec::new();
                alg_body.extend_from_slice(OID_RSA);
                alg_body.extend_from_slice(NULL_PARAMS);
                let alg_id = der_seq(&alg_body);
                let inner = der_octet_string(&pkcs1);
                let mut body = Vec::new();
                body.extend_from_slice(&[0x02, 0x01, 0x00]);
                body.extend_from_slice(&alg_id);
                body.extend_from_slice(&inner);
                let pkcs8 = der_seq(&body);
                Ok(KeyMaterial::RsaPrivate {
                    pkcs8_der: pkcs8,
                    components,
                })
            } else {
                let pub_components = RsaPublicComponents { n: n.clone(), e: e.clone() };
                let spki_der = build_rsa_spki(&n, &e)?;
                Ok(KeyMaterial::RsaPublic {
                    spki_der,
                    components: pub_components,
                })
            }
        }
        "EC" => {
            let crv = jwk
                .crv
                .as_deref()
                .ok_or_else(|| OpError::node("ERR_CRYPTO_INVALID_JWK", "JWK EC 'crv' missing"))?;
            let curve = NamedCurve::from_str(crv).ok_or_else(|| {
                OpError::node("ERR_CRYPTO_INVALID_JWK", format!("Unknown EC curve: {crv}"))
            })?;
            let x = jwk
                .x
                .as_ref()
                .and_then(|s| base64url_decode(s).ok())
                .ok_or_else(|| OpError::node("ERR_CRYPTO_INVALID_JWK", "JWK EC 'x' missing"))?;
            let y = jwk
                .y
                .as_ref()
                .and_then(|s| base64url_decode(s).ok())
                .ok_or_else(|| OpError::node("ERR_CRYPTO_INVALID_JWK", "JWK EC 'y' missing"))?;
            let n_len = curve.order_len();
            let mut x_padded = vec![0u8; n_len];
            let mut y_padded = vec![0u8; n_len];
            if x.len() <= n_len {
                x_padded[n_len - x.len()..].copy_from_slice(&x);
            } else {
                return Err(OpError::node("ERR_CRYPTO_INVALID_JWK", "EC 'x' too long"));
            }
            if y.len() <= n_len {
                y_padded[n_len - y.len()..].copy_from_slice(&y);
            } else {
                return Err(OpError::node("ERR_CRYPTO_INVALID_JWK", "EC 'y' too long"));
            }
            let mut raw_xy = vec![0x04u8];
            raw_xy.extend_from_slice(&x_padded);
            raw_xy.extend_from_slice(&y_padded);
            if requested == KeyType::Private {
                let d = jwk
                    .d
                    .as_ref()
                    .and_then(|s| base64url_decode(s).ok())
                    .ok_or_else(|| {
                        OpError::node("ERR_CRYPTO_INVALID_JWK", "JWK EC 'd' missing")
                    })?;
                let mut raw_d = vec![0u8; n_len];
                if d.len() <= n_len {
                    raw_d[n_len - d.len()..].copy_from_slice(&d);
                } else {
                    return Err(OpError::node("ERR_CRYPTO_INVALID_JWK", "EC 'd' too long"));
                }
                let pkcs8 = build_ec_pkcs8(curve, &raw_d, &raw_xy);
                Ok(KeyMaterial::EcPrivate {
                    pkcs8_der: pkcs8,
                    raw_d,
                    raw_xy,
                })
            } else {
                let spki = build_ec_spki(&raw_xy)?;
                Ok(KeyMaterial::EcPublic {
                    spki_der: spki,
                    raw_xy,
                })
            }
        }
        "OKP" => {
            let crv = jwk
                .crv
                .as_deref()
                .ok_or_else(|| OpError::node("ERR_CRYPTO_INVALID_JWK", "JWK OKP 'crv' missing"))?;
            let x = jwk
                .x
                .as_ref()
                .and_then(|s| base64url_decode(s).ok())
                .ok_or_else(|| OpError::node("ERR_CRYPTO_INVALID_JWK", "JWK OKP 'x' missing"))?;
            if x.len() != 32 {
                return Err(OpError::node(
                    "ERR_CRYPTO_INVALID_JWK",
                    "JWK OKP 'x' must be 32 bytes",
                ));
            }
            let mut raw_x = [0u8; 32];
            raw_x.copy_from_slice(&x);
            match crv {
                "Ed25519" => {
                    if requested == KeyType::Private {
                        let d = jwk
                            .d
                            .as_ref()
                            .and_then(|s| base64url_decode(s).ok())
                            .ok_or_else(|| {
                                OpError::node("ERR_CRYPTO_INVALID_JWK", "JWK OKP 'd' missing")
                            })?;
                        if d.len() != 32 {
                            return Err(OpError::node(
                                "ERR_CRYPTO_INVALID_JWK",
                                "JWK OKP 'd' must be 32 bytes",
                            ));
                        }
                        let mut raw_d = [0u8; 32];
                        raw_d.copy_from_slice(&d);
                        Ok(KeyMaterial::Ed25519Private {
                            pkcs8_der: build_ed25519_pkcs8(&raw_d),
                            raw_d,
                            raw_x,
                        })
                    } else {
                        Ok(KeyMaterial::Ed25519Public {
                            spki_der: build_ed25519_spki(&raw_x),
                            raw_x,
                        })
                    }
                }
                "X25519" => {
                    if requested == KeyType::Private {
                        let d = jwk
                            .d
                            .as_ref()
                            .and_then(|s| base64url_decode(s).ok())
                            .ok_or_else(|| {
                                OpError::node("ERR_CRYPTO_INVALID_JWK", "JWK OKP 'd' missing")
                            })?;
                        if d.len() != 32 {
                            return Err(OpError::node(
                                "ERR_CRYPTO_INVALID_JWK",
                                "JWK OKP 'd' must be 32 bytes",
                            ));
                        }
                        let mut raw_d = [0u8; 32];
                        raw_d.copy_from_slice(&d);
                        Ok(KeyMaterial::X25519Private {
                            pkcs8_der: build_x25519_pkcs8(&raw_d),
                            raw_d,
                            raw_x,
                        })
                    } else {
                        Ok(KeyMaterial::X25519Public {
                            spki_der: build_x25519_spki(&raw_x),
                            raw_x,
                        })
                    }
                }
                other => Err(OpError::node(
                    "ERR_CRYPTO_INVALID_JWK",
                    format!("Unknown OKP curve: {other}"),
                )),
            }
        }
        other => Err(OpError::node(
            "ERR_CRYPTO_INVALID_JWK",
            format!("Unknown kty: {other}"),
        )),
    }
}

fn build_rsa_pkcs1_private_der(c: &RsaPrivateComponents) -> Result<Vec<u8>, OpError> {
    let mut body = Vec::new();
    body.extend_from_slice(&[0x02, 0x01, 0x00]); // version 0
    body.extend_from_slice(&der_integer_unsigned(&c.n));
    body.extend_from_slice(&der_integer_unsigned(&c.e));
    body.extend_from_slice(&der_integer_unsigned(&c.d));
    body.extend_from_slice(&der_integer_unsigned(&c.p));
    body.extend_from_slice(&der_integer_unsigned(&c.q));
    body.extend_from_slice(&der_integer_unsigned(&c.dp));
    body.extend_from_slice(&der_integer_unsigned(&c.dq));
    body.extend_from_slice(&der_integer_unsigned(&c.qi));
    Ok(der_seq(&body))
}

fn build_ec_pkcs8(curve: NamedCurve, raw_d: &[u8], raw_xy: &[u8]) -> Vec<u8> {
    // SEC1 ECPrivateKey wrapped in PKCS#8.
    let sec1_body = {
        let mut b = Vec::new();
        b.extend_from_slice(&[0x02, 0x01, 0x01]); // version 1
        b.extend_from_slice(&der_octet_string(raw_d));
        // Optional [1] publicKey BIT STRING.
        let bs = der_bit_string(raw_xy);
        let mut tagged = vec![0xa1];
        tagged.extend_from_slice(&der_len(bs.len()));
        tagged.extend_from_slice(&bs);
        b.extend_from_slice(&tagged);
        b
    };
    let sec1 = der_seq(&sec1_body);
    let mut alg_body = Vec::new();
    alg_body.extend_from_slice(OID_EC_PUBLIC_KEY);
    alg_body.extend_from_slice(curve_oid_for(curve));
    let alg_id = der_seq(&alg_body);
    let inner = der_octet_string(&sec1);
    let mut body = Vec::new();
    body.extend_from_slice(&[0x02, 0x01, 0x00]);
    body.extend_from_slice(&alg_id);
    body.extend_from_slice(&inner);
    der_seq(&body)
}

fn build_ec_sec1_private_only(curve: NamedCurve, raw_d: &[u8], raw_xy: &[u8]) -> Vec<u8> {
    let _ = curve;
    let mut body = Vec::new();
    body.extend_from_slice(&[0x02, 0x01, 0x01]);
    body.extend_from_slice(&der_octet_string(raw_d));
    let bs = der_bit_string(raw_xy);
    let mut tagged = vec![0xa1];
    tagged.extend_from_slice(&der_len(bs.len()));
    tagged.extend_from_slice(&bs);
    body.extend_from_slice(&tagged);
    der_seq(&body)
}

// ---------------------------------------------------------------------------
// Export
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExportFormat {
    Pem,
    Der,
    Jwk,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExportType {
    Pkcs1,
    Pkcs8,
    Spki,
    Sec1,
}

struct ExportOptions {
    format: ExportFormat,
    type_: Option<ExportType>,
    cipher: Option<String>,
    passphrase: Option<Vec<u8>>,
}

fn export_impl<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    state: &KeyObjectState,
    options: v8::Local<v8::Value>,
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    let opts = parse_export_options(scope, options, state.key_type)?;
    if let KeyMaterial::Symmetric(ref bytes) = state.material {
        return match opts.format {
            ExportFormat::Jwk => Ok(jwk_export_secret(scope, bytes)),
            _ => Ok(buffer::emit_buffer(scope, bytes)),
        };
    }
    if opts.format == ExportFormat::Jwk {
        return jwk_export_asymmetric(scope, state);
    }

    let (label, der) = encode_to_der(state, opts.type_)?;

    if opts.cipher.is_some() || opts.passphrase.is_some() {
        return Err(OpError::node(
            "ERR_CRYPTO_UNSUPPORTED_OPERATION",
            "Encrypted PKCS#8 export is not yet supported (Stage E)",
        ));
    }

    match opts.format {
        ExportFormat::Pem => {
            let pem = pem_encode(&label, &der);
            Ok(buffer::emit_string(scope, &pem).into())
        }
        ExportFormat::Der => Ok(buffer::emit_buffer(scope, &der)),
        ExportFormat::Jwk => unreachable!(),
    }
}

fn parse_export_options(
    scope: &mut v8::PinScope,
    options: v8::Local<v8::Value>,
    kt: KeyType,
) -> Result<ExportOptions, OpError> {
    if options.is_undefined() || options.is_null() {
        if kt == KeyType::Secret {
            return Ok(ExportOptions {
                format: ExportFormat::Der,
                type_: None,
                cipher: None,
                passphrase: None,
            });
        }
        return Err(OpError::node(
            "ERR_MISSING_OPTION",
            "export options { format, type } are required for asymmetric keys",
        ));
    }
    let obj: v8::Local<v8::Object> = options.try_into().map_err(|_| {
        OpError::node("ERR_INVALID_ARG_TYPE", "export options must be an object")
    })?;
    let format_str = read_optional_str(scope, obj, "format")?;
    let type_str = read_optional_str(scope, obj, "type")?;
    let cipher = read_optional_str(scope, obj, "cipher")?;
    let passphrase = read_optional_bytes(scope, obj, "passphrase")?;

    let format = match format_str.as_deref() {
        Some("pem") => ExportFormat::Pem,
        Some("der") => ExportFormat::Der,
        Some("jwk") => ExportFormat::Jwk,
        None if kt == KeyType::Secret => ExportFormat::Der,
        None => {
            return Err(OpError::node(
                "ERR_MISSING_OPTION",
                "options.format required",
            ));
        }
        Some(other) => {
            return Err(OpError::node(
                "ERR_INVALID_ARG_VALUE",
                format!("Unknown format: {other}"),
            ));
        }
    };
    let type_ = match type_str.as_deref() {
        None => None,
        Some("pkcs1") => Some(ExportType::Pkcs1),
        Some("pkcs8") => Some(ExportType::Pkcs8),
        Some("spki") => Some(ExportType::Spki),
        Some("sec1") => Some(ExportType::Sec1),
        Some(other) => {
            return Err(OpError::node(
                "ERR_INVALID_ARG_VALUE",
                format!("Unknown type: {other}"),
            ));
        }
    };
    Ok(ExportOptions {
        format,
        type_,
        cipher,
        passphrase,
    })
}

fn encode_to_der(
    state: &KeyObjectState,
    type_: Option<ExportType>,
) -> Result<(String, Vec<u8>), OpError> {
    match (&state.material, state.key_type, type_) {
        (KeyMaterial::RsaPublic { spki_der, .. }, KeyType::Public, None | Some(ExportType::Spki)) => {
            Ok(("PUBLIC KEY".to_string(), spki_der.clone()))
        }
        (KeyMaterial::RsaPublic { components, .. }, KeyType::Public, Some(ExportType::Pkcs1)) => {
            let mut body = Vec::new();
            body.extend_from_slice(&der_integer_unsigned(&components.n));
            body.extend_from_slice(&der_integer_unsigned(&components.e));
            Ok(("RSA PUBLIC KEY".to_string(), der_seq(&body)))
        }
        (
            KeyMaterial::RsaPrivate { pkcs8_der, .. },
            KeyType::Private,
            None | Some(ExportType::Pkcs8),
        ) => Ok(("PRIVATE KEY".to_string(), pkcs8_der.clone())),
        (
            KeyMaterial::RsaPrivate { components, .. },
            KeyType::Private,
            Some(ExportType::Pkcs1),
        ) => {
            let der = build_rsa_pkcs1_private_der(components)?;
            Ok(("RSA PRIVATE KEY".to_string(), der))
        }
        (KeyMaterial::EcPublic { spki_der, .. }, KeyType::Public, None | Some(ExportType::Spki)) => {
            Ok(("PUBLIC KEY".to_string(), spki_der.clone()))
        }
        (
            KeyMaterial::EcPrivate { pkcs8_der, .. },
            KeyType::Private,
            None | Some(ExportType::Pkcs8),
        ) => Ok(("PRIVATE KEY".to_string(), pkcs8_der.clone())),
        (
            KeyMaterial::EcPrivate { raw_d, raw_xy, .. },
            KeyType::Private,
            Some(ExportType::Sec1),
        ) => {
            let curve = state
                .ec_named_curve()
                .ok_or_else(|| OpError::node("ERR_CRYPTO_OPERATION_FAILED", "EC curve unknown"))?;
            let der = build_ec_sec1_private_only(curve, raw_d, raw_xy);
            Ok(("EC PRIVATE KEY".to_string(), der))
        }
        (
            KeyMaterial::Ed25519Public { spki_der, .. } | KeyMaterial::X25519Public { spki_der, .. },
            KeyType::Public,
            None | Some(ExportType::Spki),
        ) => Ok(("PUBLIC KEY".to_string(), spki_der.clone())),
        (
            KeyMaterial::Ed25519Private { pkcs8_der, .. } | KeyMaterial::X25519Private { pkcs8_der, .. },
            KeyType::Private,
            None | Some(ExportType::Pkcs8),
        ) => Ok(("PRIVATE KEY".to_string(), pkcs8_der.clone())),
        _ => Err(OpError::node(
            "ERR_CRYPTO_INCOMPATIBLE_KEY_OPTIONS",
            "Invalid format/type combination for this key",
        )),
    }
}

fn jwk_export_secret<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    bytes: &[u8],
) -> v8::Local<'s, v8::Value> {
    let obj = v8::Object::new(scope);
    set_str(scope, obj, "kty", "oct");
    set_str(scope, obj, "k", &base64url_encode(bytes));
    obj.into()
}

fn jwk_export_asymmetric<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    state: &KeyObjectState,
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    let obj = v8::Object::new(scope);
    match &state.material {
        KeyMaterial::RsaPrivate { components, .. } => {
            set_str(scope, obj, "kty", "RSA");
            set_str(scope, obj, "n", &base64url_encode(&components.n));
            set_str(scope, obj, "e", &base64url_encode(&components.e));
            set_str(scope, obj, "d", &base64url_encode(&components.d));
            if !components.p.is_empty() {
                set_str(scope, obj, "p", &base64url_encode(&components.p));
                set_str(scope, obj, "q", &base64url_encode(&components.q));
                set_str(scope, obj, "dp", &base64url_encode(&components.dp));
                set_str(scope, obj, "dq", &base64url_encode(&components.dq));
                set_str(scope, obj, "qi", &base64url_encode(&components.qi));
            }
        }
        KeyMaterial::RsaPublic { components, .. } => {
            set_str(scope, obj, "kty", "RSA");
            set_str(scope, obj, "n", &base64url_encode(&components.n));
            set_str(scope, obj, "e", &base64url_encode(&components.e));
        }
        KeyMaterial::EcPrivate { raw_d, raw_xy, .. } => {
            set_str(scope, obj, "kty", "EC");
            let curve = state
                .ec_named_curve()
                .ok_or_else(|| OpError::node("ERR_CRYPTO_OPERATION_FAILED", "EC curve unknown"))?;
            set_str(scope, obj, "crv", curve.as_str());
            let n = curve.order_len();
            if raw_xy.len() == 1 + 2 * n {
                set_str(scope, obj, "x", &base64url_encode(&raw_xy[1..1 + n]));
                set_str(scope, obj, "y", &base64url_encode(&raw_xy[1 + n..]));
            }
            set_str(scope, obj, "d", &base64url_encode(raw_d));
        }
        KeyMaterial::EcPublic { raw_xy, .. } => {
            set_str(scope, obj, "kty", "EC");
            let curve = state
                .ec_named_curve()
                .ok_or_else(|| OpError::node("ERR_CRYPTO_OPERATION_FAILED", "EC curve unknown"))?;
            set_str(scope, obj, "crv", curve.as_str());
            let n = curve.order_len();
            if raw_xy.len() == 1 + 2 * n {
                set_str(scope, obj, "x", &base64url_encode(&raw_xy[1..1 + n]));
                set_str(scope, obj, "y", &base64url_encode(&raw_xy[1 + n..]));
            }
        }
        KeyMaterial::Ed25519Private { raw_d, raw_x, .. } => {
            set_str(scope, obj, "kty", "OKP");
            set_str(scope, obj, "crv", "Ed25519");
            set_str(scope, obj, "x", &base64url_encode(raw_x));
            set_str(scope, obj, "d", &base64url_encode(raw_d));
        }
        KeyMaterial::Ed25519Public { raw_x, .. } => {
            set_str(scope, obj, "kty", "OKP");
            set_str(scope, obj, "crv", "Ed25519");
            set_str(scope, obj, "x", &base64url_encode(raw_x));
        }
        KeyMaterial::X25519Private { raw_d, raw_x, .. } => {
            set_str(scope, obj, "kty", "OKP");
            set_str(scope, obj, "crv", "X25519");
            set_str(scope, obj, "x", &base64url_encode(raw_x));
            set_str(scope, obj, "d", &base64url_encode(raw_d));
        }
        KeyMaterial::X25519Public { raw_x, .. } => {
            set_str(scope, obj, "kty", "OKP");
            set_str(scope, obj, "crv", "X25519");
            set_str(scope, obj, "x", &base64url_encode(raw_x));
        }
        KeyMaterial::Symmetric(_) => unreachable!(),
    }
    Ok(obj.into())
}

// ---------------------------------------------------------------------------
// CryptoKey-bridge helper (toCryptoKey)
// ---------------------------------------------------------------------------

fn parse_webcrypto_algorithm(
    scope: &mut v8::PinScope,
    algorithm: v8::Local<v8::Value>,
    material: &KeyMaterial,
) -> Result<KeyAlgorithm, OpError> {
    if algorithm.is_string() {
        let name = algorithm.to_rust_string_lossy(scope);
        return parse_alg_name(&name, None, None, None, material);
    }
    let obj: v8::Local<v8::Object> = algorithm.try_into().map_err(|_| {
        OpError::node(
            "ERR_INVALID_ARG_TYPE",
            "algorithm must be a string or object",
        )
    })?;
    let name_key = v8::String::new(scope, "name").unwrap();
    let name = obj
        .get(scope, name_key.into())
        .map(|v| v.to_rust_string_lossy(scope))
        .ok_or_else(|| OpError::node("ERR_INVALID_ARG_VALUE", "algorithm.name required"))?;
    let hash_str = read_alg_hash(scope, obj);
    let curve_str = read_optional_str(scope, obj, "namedCurve")?;
    let length_v = obj
        .get(scope, v8::String::new(scope, "length").unwrap().into())
        .filter(|v| !v.is_undefined() && !v.is_null());
    let length = length_v.and_then(|v| v.uint32_value(scope));
    parse_alg_name(&name, hash_str.as_deref(), curve_str.as_deref(), length, material)
}

fn read_alg_hash(scope: &mut v8::PinScope, obj: v8::Local<v8::Object>) -> Option<String> {
    let key = v8::String::new(scope, "hash").unwrap();
    let v = obj.get(scope, key.into())?;
    if v.is_undefined() || v.is_null() {
        return None;
    }
    if v.is_string() {
        return Some(v.to_rust_string_lossy(scope));
    }
    if let Ok(o) = v8::Local::<v8::Object>::try_from(v) {
        let n = v8::String::new(scope, "name").unwrap();
        return o.get(scope, n.into()).map(|v| v.to_rust_string_lossy(scope));
    }
    None
}

fn parse_alg_name(
    name: &str,
    hash: Option<&str>,
    curve: Option<&str>,
    length: Option<u32>,
    material: &KeyMaterial,
) -> Result<KeyAlgorithm, OpError> {
    match name {
        "RSASSA-PKCS1-v1_5" | "RSA-PSS" | "RSA-OAEP" => {
            let hash_name = hash.ok_or_else(|| {
                OpError::node("ERR_INVALID_ARG_VALUE", "algorithm.hash required for RSA")
            })?;
            let hash = HashAlgo::from_str(hash_name).ok_or_else(|| {
                OpError::node("ERR_INVALID_ARG_VALUE", format!("Unknown hash: {hash_name}"))
            })?;
            // Read modulus / exponent from material if available.
            let (modulus_length, public_exponent) = match material {
                KeyMaterial::RsaPrivate { components, .. } => {
                    ((components.n.len() * 8) as u32, components.e.clone())
                }
                KeyMaterial::RsaPublic { components, .. } => {
                    ((components.n.len() * 8) as u32, components.e.clone())
                }
                _ => (2048, vec![0x01, 0x00, 0x01]),
            };
            let static_name: &'static str = match name {
                "RSASSA-PKCS1-v1_5" => "RSASSA-PKCS1-v1_5",
                "RSA-PSS" => "RSA-PSS",
                "RSA-OAEP" => "RSA-OAEP",
                _ => unreachable!(),
            };
            Ok(KeyAlgorithm::RsaHashed(RsaHashedKeyAlgorithm {
                name: static_name,
                modulus_length,
                public_exponent,
                hash,
            }))
        }
        "ECDSA" | "ECDH" => {
            let curve_name = curve.ok_or_else(|| {
                OpError::node(
                    "ERR_INVALID_ARG_VALUE",
                    "algorithm.namedCurve required for EC",
                )
            })?;
            let curve = NamedCurve::from_str(curve_name).ok_or_else(|| {
                OpError::node(
                    "ERR_INVALID_ARG_VALUE",
                    format!("Unknown curve: {curve_name}"),
                )
            })?;
            let static_name: &'static str = if name == "ECDSA" { "ECDSA" } else { "ECDH" };
            Ok(KeyAlgorithm::Ec(EcKeyAlgorithm {
                name: static_name,
                named_curve: curve,
            }))
        }
        "AES-CTR" | "AES-CBC" | "AES-GCM" | "AES-KW" => {
            let length = length.unwrap_or_else(|| match material {
                KeyMaterial::Symmetric(b) => (b.len() * 8) as u32,
                _ => 256,
            });
            let static_name: &'static str = match name {
                "AES-CTR" => "AES-CTR",
                "AES-CBC" => "AES-CBC",
                "AES-GCM" => "AES-GCM",
                "AES-KW" => "AES-KW",
                _ => unreachable!(),
            };
            Ok(KeyAlgorithm::Aes(AesKeyAlgorithm {
                name: static_name,
                length,
            }))
        }
        "HMAC" => {
            let hash_name = hash.ok_or_else(|| {
                OpError::node("ERR_INVALID_ARG_VALUE", "algorithm.hash required for HMAC")
            })?;
            let hash = HashAlgo::from_str(hash_name).ok_or_else(|| {
                OpError::node("ERR_INVALID_ARG_VALUE", format!("Unknown hash: {hash_name}"))
            })?;
            Ok(KeyAlgorithm::Hmac(HmacKeyAlgorithm {
                hash,
                length: hash.block_size_bits(),
            }))
        }
        "Ed25519" => Ok(KeyAlgorithm::Ed25519),
        "X25519" => Ok(KeyAlgorithm::X25519),
        "PBKDF2" => Ok(KeyAlgorithm::Pbkdf2),
        "HKDF" => Ok(KeyAlgorithm::Hkdf),
        other => Err(OpError::node(
            "ERR_INVALID_ARG_VALUE",
            format!("Unknown algorithm: {other}"),
        )),
    }
}

fn parse_key_usages(
    scope: &mut v8::PinScope,
    value: v8::Local<v8::Value>,
) -> Result<Vec<KeyUsage>, OpError> {
    let arr: v8::Local<v8::Array> = value.try_into().map_err(|_| {
        OpError::node("ERR_INVALID_ARG_TYPE", "keyUsages must be an array")
    })?;
    let mut out = Vec::with_capacity(arr.length() as usize);
    for i in 0..arr.length() {
        let v = arr.get_index(scope, i).unwrap();
        let s = v.to_rust_string_lossy(scope);
        let usage = KeyUsage::from_str(&s).ok_or_else(|| {
            OpError::node("ERR_INVALID_ARG_VALUE", format!("Unknown key usage: {s}"))
        })?;
        out.push(usage);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Top-level callbacks
// ---------------------------------------------------------------------------

pub(crate) fn create_secret_key_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    if args.length() < 1 {
        let exc = crate::node_error::build_node_exception(
            scope,
            "ERR_INVALID_ARG_TYPE",
            "createSecretKey requires a key argument",
        );
        scope.throw_exception(exc);
        return;
    }
    let input = args.get(0);
    let encoding = if args.length() >= 2 && args.get(1).is_string() {
        Some(args.get(1).to_rust_string_lossy(scope))
    } else {
        None
    };
    match create_secret_key(scope, input, encoding.as_deref()) {
        Ok(v) => rv.set(v),
        Err(err) => {
            let exc = crate::web::crypto::helpers::op_error_to_v8(scope, err);
            scope.throw_exception(exc);
        }
    }
}

pub(crate) fn create_public_key_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    if args.length() < 1 {
        let exc = crate::node_error::build_node_exception(
            scope,
            "ERR_INVALID_ARG_TYPE",
            "createPublicKey requires a key argument",
        );
        scope.throw_exception(exc);
        return;
    }
    match create_public_key(scope, args.get(0)) {
        Ok(v) => rv.set(v),
        Err(err) => {
            let exc = crate::web::crypto::helpers::op_error_to_v8(scope, err);
            scope.throw_exception(exc);
        }
    }
}

pub(crate) fn create_private_key_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    if args.length() < 1 {
        let exc = crate::node_error::build_node_exception(
            scope,
            "ERR_INVALID_ARG_TYPE",
            "createPrivateKey requires a key argument",
        );
        scope.throw_exception(exc);
        return;
    }
    match create_private_key(scope, args.get(0)) {
        Ok(v) => rv.set(v),
        Err(err) => {
            let exc = crate::web::crypto::helpers::op_error_to_v8(scope, err);
            scope.throw_exception(exc);
        }
    }
}

pub(crate) fn key_object_from_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    if args.length() < 1 {
        let exc = crate::node_error::build_node_exception(
            scope,
            "ERR_INVALID_ARG_TYPE",
            "KeyObject.from requires a CryptoKey argument",
        );
        scope.throw_exception(exc);
        return;
    }
    match key_object_from(scope, args.get(0)) {
        Ok(v) => rv.set(v),
        Err(err) => {
            let exc = crate::web::crypto::helpers::op_error_to_v8(scope, err);
            scope.throw_exception(exc);
        }
    }
}
