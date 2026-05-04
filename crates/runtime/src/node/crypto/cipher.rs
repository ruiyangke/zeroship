//! `Cipher` / `Decipher` classes + `createCipheriv` / `createDecipheriv`
//! factories.
//!
//! Per `docs/proposals/node-crypto-native.md` §V.4 / §III.
//!
//! Stage C ships AES-{CBC,CTR,GCM} and ChaCha20-Poly1305 — the four
//! modes that cover ~95% of npm-package usage. CCM is in the design as
//! Stage C / D-N38 but defers to Stage E (the raw aws-lc-sys FFI is
//! ~120 LOC and rarely used in app-server code; covered when a creator
//! app surfaces the need).
//!
//! Architecture:
//! - `update()` accumulates the input bytes (or for stream-cipher
//!   modes, processes them incrementally — Stage C buffers and
//!   processes in `final()` for simplicity; npm packages overwhelmingly
//!   call update once + final once on small payloads where buffering
//!   is invisible).
//! - GCM/ChaCha20-Poly1305: `setAAD` captures AAD bytes; `final()`
//!   runs aws-lc-rs `aead::seal_in_place_append_tag` /
//!   `open_in_place`. `getAuthTag` returns the appended tag for
//!   Cipher; `setAuthTag` provides it for Decipher.
//! - CBC/CTR: aws-lc-rs `cipher::*` paths.

#![allow(unsafe_code)]

use super::buffer;
use super::key_object;
use crate::web::crypto::crypto_key;
use crate::web::crypto::key_material::KeyMaterial;
use crate::state::OpError;

#[allow(unused_imports)]
use zeroship_runtime_macros::{v8_class, v8_constructor, v8_method, v8_name, v8_to_string_tag};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CipherAlg {
    Aes128Cbc,
    Aes192Cbc,
    Aes256Cbc,
    Aes128Ctr,
    Aes192Ctr,
    Aes256Ctr,
    Aes128Gcm,
    Aes192Gcm,
    Aes256Gcm,
    ChaCha20Poly1305,
}

impl CipherAlg {
    fn from_name(name: &str) -> Option<Self> {
        Some(match name.to_ascii_lowercase().as_str() {
            "aes-128-cbc" => Self::Aes128Cbc,
            "aes-192-cbc" => Self::Aes192Cbc,
            "aes-256-cbc" => Self::Aes256Cbc,
            "aes-128-ctr" => Self::Aes128Ctr,
            "aes-192-ctr" => Self::Aes192Ctr,
            "aes-256-ctr" => Self::Aes256Ctr,
            "aes-128-gcm" => Self::Aes128Gcm,
            "aes-192-gcm" => Self::Aes192Gcm,
            "aes-256-gcm" => Self::Aes256Gcm,
            "chacha20-poly1305" => Self::ChaCha20Poly1305,
            _ => return None,
        })
    }

    fn key_len(self) -> usize {
        match self {
            Self::Aes128Cbc | Self::Aes128Ctr | Self::Aes128Gcm => 16,
            Self::Aes192Cbc | Self::Aes192Ctr | Self::Aes192Gcm => 24,
            Self::Aes256Cbc | Self::Aes256Ctr | Self::Aes256Gcm => 32,
            Self::ChaCha20Poly1305 => 32,
        }
    }

    fn is_aead(self) -> bool {
        matches!(
            self,
            Self::Aes128Gcm | Self::Aes192Gcm | Self::Aes256Gcm | Self::ChaCha20Poly1305
        )
    }

    fn block_size(self) -> usize {
        match self {
            Self::Aes128Cbc | Self::Aes192Cbc | Self::Aes256Cbc => 16,
            _ => 1,
        }
    }
}

pub struct Cipher {
    alg: CipherAlg,
    encrypt: bool,
    key: Vec<u8>,
    iv: Vec<u8>,
    aad: Vec<u8>,
    in_buf: Vec<u8>,
    out_buf: Vec<u8>,
    auth_tag: Option<Vec<u8>>,
    auth_tag_length: usize,
    finalised: bool,
    auto_padding: bool,
}

#[v8_class]
#[v8_to_string_tag = "Cipher"]
impl Cipher {
    #[v8_constructor]
    fn new() -> Result<Cipher, OpError> {
        Err(OpError::type_error(
            "Cipher is not a constructor — use crypto.createCipheriv(...)",
        ))
    }

    #[v8_method]
    fn update<'s>(
        &mut self,
        scope: &mut v8::PinScope<'s, '_>,
        data: v8::Local<v8::Value>,
        input_encoding: Option<String>,
        output_encoding: Option<String>,
    ) -> Result<v8::Local<'s, v8::Value>, OpError> {
        if self.finalised {
            return Err(OpError::node(
                "ERR_CRYPTO_INVALID_STATE",
                "Cipher.update called after final()",
            ));
        }
        let bytes = buffer::extract_input(scope, data, input_encoding.as_deref())?;
        self.in_buf.extend_from_slice(&bytes);
        // Return empty output until final(); npm consumers handle this fine.
        let empty: &[u8] = &[];
        buffer::emit_output(scope, empty, output_encoding.as_deref())
    }

    #[v8_method]
    #[v8_name = "final"]
    fn final_<'s>(
        &mut self,
        scope: &mut v8::PinScope<'s, '_>,
        output_encoding: Option<String>,
    ) -> Result<v8::Local<'s, v8::Value>, OpError> {
        if self.finalised {
            return Err(OpError::node(
                "ERR_CRYPTO_INVALID_STATE",
                "Cipher.final already called",
            ));
        }
        self.finalised = true;
        let result = run_cipher(self)?;
        buffer::emit_output(scope, &result, output_encoding.as_deref())
    }

    #[v8_method]
    #[v8_name = "setAAD"]
    fn set_aad(
        &mut self,
        scope: &mut v8::PinScope,
        aad: v8::Local<v8::Value>,
        _options: v8::Local<v8::Value>,
    ) -> Result<(), OpError> {
        if !self.alg.is_aead() {
            return Err(OpError::node(
                "ERR_CRYPTO_INVALID_STATE",
                "setAAD only valid for authenticated cipher modes",
            ));
        }
        let bytes = buffer::extract_input(scope, aad, None)?;
        self.aad = bytes;
        Ok(())
    }

    #[v8_method]
    #[v8_name = "getAuthTag"]
    fn get_auth_tag<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
    ) -> Result<v8::Local<'s, v8::Value>, OpError> {
        if !self.encrypt {
            return Err(OpError::node(
                "ERR_CRYPTO_INVALID_STATE",
                "Cannot call getAuthTag on a Decipher",
            ));
        }
        let tag = self.auth_tag.as_ref().ok_or_else(|| {
            OpError::node(
                "ERR_CRYPTO_INVALID_STATE",
                "getAuthTag called before final()",
            )
        })?;
        Ok(buffer::emit_buffer(scope, tag))
    }

    #[v8_method]
    #[v8_name = "setAuthTag"]
    fn set_auth_tag(
        &mut self,
        scope: &mut v8::PinScope,
        tag: v8::Local<v8::Value>,
        encoding: Option<String>,
    ) -> Result<(), OpError> {
        if self.encrypt {
            return Err(OpError::node(
                "ERR_CRYPTO_INVALID_STATE",
                "Cannot call setAuthTag on a Cipher",
            ));
        }
        let bytes = buffer::extract_input(scope, tag, encoding.as_deref())?;
        self.auth_tag = Some(bytes);
        Ok(())
    }

    #[v8_method]
    #[v8_name = "setAutoPadding"]
    fn set_auto_padding(
        &mut self,
        scope: &mut v8::PinScope,
        on: v8::Local<v8::Value>,
    ) -> Result<(), OpError> {
        let on_b = if on.is_undefined() || on.is_null() {
            true
        } else {
            on.boolean_value(scope)
        };
        self.auto_padding = on_b;
        Ok(())
    }
}

fn run_cipher(c: &mut Cipher) -> Result<Vec<u8>, OpError> {
    use aws_lc_rs::aead::{
        Aad, Nonce, UnboundKey, AES_128_GCM, AES_192_GCM, AES_256_GCM, CHACHA20_POLY1305, LessSafeKey,
    };
    match c.alg {
        CipherAlg::Aes128Gcm | CipherAlg::Aes192Gcm | CipherAlg::Aes256Gcm => {
            let alg = match c.alg {
                CipherAlg::Aes128Gcm => &AES_128_GCM,
                CipherAlg::Aes192Gcm => &AES_192_GCM,
                CipherAlg::Aes256Gcm => &AES_256_GCM,
                _ => unreachable!(),
            };
            let unbound = UnboundKey::new(alg, &c.key)
                .map_err(|_| OpError::node("ERR_CRYPTO_OPERATION_FAILED", "GCM key construction"))?;
            let key = LessSafeKey::new(unbound);
            if c.iv.len() != 12 {
                return Err(OpError::node(
                    "ERR_CRYPTO_INVALID_IV",
                    "AES-GCM IV must be 12 bytes",
                ));
            }
            let nonce_arr: [u8; 12] = c.iv.as_slice().try_into().unwrap();
            let nonce = Nonce::assume_unique_for_key(nonce_arr);
            if c.encrypt {
                let mut buf = c.in_buf.clone();
                key.seal_in_place_append_tag(nonce, Aad::from(&c.aad), &mut buf)
                    .map_err(|_| OpError::node("ERR_CRYPTO_OPERATION_FAILED", "GCM encrypt"))?;
                let ct_len = buf.len() - 16;
                let tag = buf[ct_len..].to_vec();
                buf.truncate(ct_len);
                c.auth_tag = Some(tag);
                Ok(buf)
            } else {
                let tag = c.auth_tag.as_ref().ok_or_else(|| {
                    OpError::node(
                        "ERR_CRYPTO_INVALID_STATE",
                        "Decipher.final called before setAuthTag",
                    )
                })?;
                let mut buf = Vec::with_capacity(c.in_buf.len() + tag.len());
                buf.extend_from_slice(&c.in_buf);
                buf.extend_from_slice(tag);
                key.open_in_place(nonce, Aad::from(&c.aad), &mut buf).map_err(|_| {
                    OpError::node(
                        "ERR_CRYPTO_OPERATION_FAILED",
                        "Unsupported state or unable to authenticate data",
                    )
                })?;
                let pt_len = buf.len() - 16;
                buf.truncate(pt_len);
                Ok(buf)
            }
        }
        CipherAlg::ChaCha20Poly1305 => {
            let unbound = UnboundKey::new(&CHACHA20_POLY1305, &c.key).map_err(|_| {
                OpError::node("ERR_CRYPTO_OPERATION_FAILED", "ChaCha20 key construction")
            })?;
            let key = LessSafeKey::new(unbound);
            if c.iv.len() != 12 {
                return Err(OpError::node(
                    "ERR_CRYPTO_INVALID_IV",
                    "ChaCha20-Poly1305 IV must be 12 bytes",
                ));
            }
            let nonce_arr: [u8; 12] = c.iv.as_slice().try_into().unwrap();
            let nonce = Nonce::assume_unique_for_key(nonce_arr);
            if c.encrypt {
                let mut buf = c.in_buf.clone();
                key.seal_in_place_append_tag(nonce, Aad::from(&c.aad), &mut buf)
                    .map_err(|_| {
                        OpError::node("ERR_CRYPTO_OPERATION_FAILED", "ChaCha20 encrypt")
                    })?;
                let ct_len = buf.len() - 16;
                let tag = buf[ct_len..].to_vec();
                buf.truncate(ct_len);
                c.auth_tag = Some(tag);
                Ok(buf)
            } else {
                let tag = c.auth_tag.as_ref().ok_or_else(|| {
                    OpError::node(
                        "ERR_CRYPTO_INVALID_STATE",
                        "Decipher.final called before setAuthTag",
                    )
                })?;
                let mut buf = Vec::with_capacity(c.in_buf.len() + tag.len());
                buf.extend_from_slice(&c.in_buf);
                buf.extend_from_slice(tag);
                key.open_in_place(nonce, Aad::from(&c.aad), &mut buf).map_err(|_| {
                    OpError::node(
                        "ERR_CRYPTO_OPERATION_FAILED",
                        "Unsupported state or unable to authenticate data",
                    )
                })?;
                let pt_len = buf.len() - 16;
                buf.truncate(pt_len);
                Ok(buf)
            }
        }
        CipherAlg::Aes128Cbc | CipherAlg::Aes192Cbc | CipherAlg::Aes256Cbc => {
            run_aes_cbc(c)
        }
        CipherAlg::Aes128Ctr | CipherAlg::Aes192Ctr | CipherAlg::Aes256Ctr => {
            run_aes_ctr(c)
        }
    }
}

fn run_aes_cbc(c: &mut Cipher) -> Result<Vec<u8>, OpError> {
    use aws_lc_rs::cipher::{
        DecryptionContext, EncryptionContext, PaddedBlockDecryptingKey, PaddedBlockEncryptingKey,
        UnboundCipherKey, AES_128, AES_192, AES_256,
    };
    use aws_lc_rs::iv::FixedLength;
    let alg = match c.alg {
        CipherAlg::Aes128Cbc => &AES_128,
        CipherAlg::Aes192Cbc => &AES_192,
        CipherAlg::Aes256Cbc => &AES_256,
        _ => unreachable!(),
    };
    if c.iv.len() != 16 {
        return Err(OpError::node(
            "ERR_CRYPTO_INVALID_IV",
            "AES-CBC IV must be 16 bytes",
        ));
    }
    let iv_arr: [u8; 16] = c.iv.as_slice().try_into().unwrap();
    if c.encrypt {
        let key = UnboundCipherKey::new(alg, &c.key).map_err(|_| {
            OpError::node("ERR_CRYPTO_OPERATION_FAILED", "AES-CBC key construction")
        })?;
        let enc = PaddedBlockEncryptingKey::cbc_pkcs7(key).map_err(|_| {
            OpError::node("ERR_CRYPTO_OPERATION_FAILED", "AES-CBC encrypt init")
        })?;
        let mut data = c.in_buf.clone();
        enc.less_safe_encrypt(&mut data, EncryptionContext::Iv128(FixedLength::from(iv_arr)))
            .map_err(|_| OpError::node("ERR_CRYPTO_OPERATION_FAILED", "AES-CBC encrypt"))?;
        Ok(data)
    } else {
        let key = UnboundCipherKey::new(alg, &c.key).map_err(|_| {
            OpError::node("ERR_CRYPTO_OPERATION_FAILED", "AES-CBC key construction")
        })?;
        let dec = PaddedBlockDecryptingKey::cbc_pkcs7(key).map_err(|_| {
            OpError::node("ERR_CRYPTO_OPERATION_FAILED", "AES-CBC decrypt init")
        })?;
        let mut data = c.in_buf.clone();
        let pt = dec
            .decrypt(&mut data, DecryptionContext::Iv128(FixedLength::from(iv_arr)))
            .map_err(|_| OpError::node("ERR_CRYPTO_OPERATION_FAILED", "AES-CBC decrypt"))?;
        Ok(pt.to_vec())
    }
}

fn run_aes_ctr(c: &mut Cipher) -> Result<Vec<u8>, OpError> {
    use aws_lc_rs::cipher::{
        DecryptingKey, DecryptionContext, EncryptingKey, EncryptionContext, UnboundCipherKey,
        AES_128, AES_192, AES_256,
    };
    use aws_lc_rs::iv::FixedLength;
    let alg = match c.alg {
        CipherAlg::Aes128Ctr => &AES_128,
        CipherAlg::Aes192Ctr => &AES_192,
        CipherAlg::Aes256Ctr => &AES_256,
        _ => unreachable!(),
    };
    if c.iv.len() != 16 {
        return Err(OpError::node(
            "ERR_CRYPTO_INVALID_IV",
            "AES-CTR IV must be 16 bytes",
        ));
    }
    let iv_arr: [u8; 16] = c.iv.as_slice().try_into().unwrap();
    if c.encrypt {
        let key = UnboundCipherKey::new(alg, &c.key).map_err(|_| {
            OpError::node("ERR_CRYPTO_OPERATION_FAILED", "AES-CTR key construction")
        })?;
        let enc = EncryptingKey::ctr(key).map_err(|_| {
            OpError::node("ERR_CRYPTO_OPERATION_FAILED", "AES-CTR encrypt init")
        })?;
        let mut data = c.in_buf.clone();
        enc.less_safe_encrypt(&mut data, EncryptionContext::Iv128(FixedLength::from(iv_arr)))
            .map_err(|_| OpError::node("ERR_CRYPTO_OPERATION_FAILED", "AES-CTR encrypt"))?;
        Ok(data)
    } else {
        let key = UnboundCipherKey::new(alg, &c.key).map_err(|_| {
            OpError::node("ERR_CRYPTO_OPERATION_FAILED", "AES-CTR key construction")
        })?;
        let dec = DecryptingKey::ctr(key).map_err(|_| {
            OpError::node("ERR_CRYPTO_OPERATION_FAILED", "AES-CTR decrypt init")
        })?;
        let mut data = c.in_buf.clone();
        let pt = dec
            .decrypt(&mut data, DecryptionContext::Iv128(FixedLength::from(iv_arr)))
            .map_err(|_| OpError::node("ERR_CRYPTO_OPERATION_FAILED", "AES-CTR decrypt"))?;
        Ok(pt.to_vec())
    }
}

// ---------------------------------------------------------------------------
// Factories
// ---------------------------------------------------------------------------

pub fn create_cipheriv<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    algorithm: &str,
    key: v8::Local<v8::Value>,
    iv: v8::Local<v8::Value>,
    _options: Option<v8::Local<v8::Value>>,
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    create_cipher_inner(scope, algorithm, key, iv, true)
}

pub fn create_decipheriv<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    algorithm: &str,
    key: v8::Local<v8::Value>,
    iv: v8::Local<v8::Value>,
    _options: Option<v8::Local<v8::Value>>,
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    create_cipher_inner(scope, algorithm, key, iv, false)
}

fn create_cipher_inner<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    algorithm: &str,
    key: v8::Local<v8::Value>,
    iv: v8::Local<v8::Value>,
    encrypt: bool,
) -> Result<v8::Local<'s, v8::Value>, OpError> {
    let alg = CipherAlg::from_name(algorithm).ok_or_else(|| {
        OpError::node(
            "ERR_CRYPTO_UNKNOWN_CIPHER",
            format!("Unknown cipher: {algorithm}"),
        )
    })?;
    let key_bytes = extract_key_bytes(scope, key)?;
    if key_bytes.len() != alg.key_len() {
        return Err(OpError::node(
            "ERR_CRYPTO_INVALID_KEYLEN",
            format!(
                "Invalid key length for {algorithm}: got {}, expected {}",
                key_bytes.len(),
                alg.key_len()
            ),
        ));
    }
    let iv_bytes = if iv.is_null() {
        Vec::new()
    } else {
        buffer::extract_input(scope, iv, None)?
    };
    let cipher = Cipher {
        alg,
        encrypt,
        key: key_bytes,
        iv: iv_bytes,
        aad: Vec::new(),
        in_buf: Vec::new(),
        out_buf: Vec::new(),
        auth_tag: None,
        auth_tag_length: 16,
        finalised: false,
        auto_padding: true,
    };
    Ok(build_cipher(scope, cipher).into())
}

fn extract_key_bytes(
    scope: &mut v8::PinScope,
    key: v8::Local<v8::Value>,
) -> Result<Vec<u8>, OpError> {
    if let Some(state) = key_object::downcast_state(scope, key) {
        if let KeyMaterial::Symmetric(b) = &state.material {
            return Ok(b.clone());
        }
        return Err(OpError::node(
            "ERR_INVALID_ARG_TYPE",
            "Cipher key must be a SecretKeyObject",
        ));
    }
    if crypto_key::is_crypto_key(scope, key) {
        let ck = crypto_key::require(scope, key)?;
        if let KeyMaterial::Symmetric(b) = &ck.material {
            return Ok(b.clone());
        }
        return Err(OpError::node(
            "ERR_INVALID_ARG_TYPE",
            "Cipher key (CryptoKey) must be a symmetric key",
        ));
    }
    buffer::extract_input(scope, key, None)
}

fn build_cipher<'s>(scope: &mut v8::PinScope<'s, '_>, c: Cipher) -> v8::Local<'s, v8::Object> {
    let tmpl = Cipher::install(scope);
    let inst_tmpl = tmpl.instance_template(scope);
    let inst = inst_tmpl.new_instance(scope).expect("Cipher instance");
    let class_fn = tmpl.get_function(scope).unwrap();
    let proto_key = v8::String::new(scope, "prototype").unwrap();
    let proto_v = class_fn.get(scope, proto_key.into()).unwrap();
    inst.set_prototype(scope, proto_v);
    let boxed: Box<Cipher> = Box::new(c);
    let raw = Box::into_raw(boxed);
    let raw_addr = raw as usize;
    let ext = v8::External::new(scope, raw as *mut std::ffi::c_void);
    inst.set_internal_field(0, ext.into());
    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        inst,
        Box::new(move || unsafe {
            drop(Box::from_raw(raw_addr as *mut Cipher));
        }),
    );
    std::mem::forget(weak);
    inst
}

// ---------------------------------------------------------------------------
// Top-level callbacks
// ---------------------------------------------------------------------------

pub(crate) fn create_cipheriv_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    if args.length() < 3 {
        let exc = crate::node_error::build_node_exception(
            scope,
            "ERR_INVALID_ARG_TYPE",
            "createCipheriv requires algorithm, key, iv",
        );
        scope.throw_exception(exc);
        return;
    }
    let algorithm = args.get(0).to_rust_string_lossy(scope);
    let options = if args.length() >= 4 {
        Some(args.get(3))
    } else {
        None
    };
    match create_cipheriv(scope, &algorithm, args.get(1), args.get(2), options) {
        Ok(v) => rv.set(v),
        Err(err) => {
            let exc = crate::web::crypto::helpers::op_error_to_v8(scope, err);
            scope.throw_exception(exc);
        }
    }
}

pub(crate) fn create_decipheriv_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    if args.length() < 3 {
        let exc = crate::node_error::build_node_exception(
            scope,
            "ERR_INVALID_ARG_TYPE",
            "createDecipheriv requires algorithm, key, iv",
        );
        scope.throw_exception(exc);
        return;
    }
    let algorithm = args.get(0).to_rust_string_lossy(scope);
    let options = if args.length() >= 4 {
        Some(args.get(3))
    } else {
        None
    };
    match create_decipheriv(scope, &algorithm, args.get(1), args.get(2), options) {
        Ok(v) => rv.set(v),
        Err(err) => {
            let exc = crate::web::crypto::helpers::op_error_to_v8(scope, err);
            scope.throw_exception(exc);
        }
    }
}

pub(crate) fn create_cipher_callback(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    // Deprecated path — D-N22 + design §V.4.
    let exc = crate::node_error::build_node_exception(
        scope,
        "ERR_CRYPTO_UNSUPPORTED_OPERATION",
        "crypto.createCipher is deprecated and disabled by default in this runtime. Use crypto.createCipheriv with an explicit IV.",
    );
    scope.throw_exception(exc);
}

pub(crate) fn create_decipher_callback(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let exc = crate::node_error::build_node_exception(
        scope,
        "ERR_CRYPTO_UNSUPPORTED_OPERATION",
        "crypto.createDecipher is deprecated and disabled by default in this runtime. Use crypto.createDecipheriv with an explicit IV.",
    );
    scope.throw_exception(exc);
}

pub(crate) fn get_cipher_info_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    if args.length() < 1 {
        rv.set(v8::undefined(scope).into());
        return;
    }
    let name = args.get(0).to_rust_string_lossy(scope);
    let alg = match CipherAlg::from_name(&name) {
        Some(a) => a,
        None => {
            rv.set(v8::undefined(scope).into());
            return;
        }
    };
    let obj = v8::Object::new(scope);
    let k = v8::String::new(scope, "name").unwrap();
    let v = v8::String::new(scope, &name).unwrap();
    obj.set(scope, k.into(), v.into());
    let k = v8::String::new(scope, "blockSize").unwrap();
    let v = v8::Integer::new_from_unsigned(scope, alg.block_size() as u32);
    obj.set(scope, k.into(), v.into());
    let k = v8::String::new(scope, "keyLength").unwrap();
    let v = v8::Integer::new_from_unsigned(scope, alg.key_len() as u32);
    obj.set(scope, k.into(), v.into());
    let iv_len = if alg.is_aead() { 12 } else { 16 };
    let k = v8::String::new(scope, "ivLength").unwrap();
    let v = v8::Integer::new_from_unsigned(scope, iv_len as u32);
    obj.set(scope, k.into(), v.into());
    let mode = match alg {
        CipherAlg::Aes128Cbc | CipherAlg::Aes192Cbc | CipherAlg::Aes256Cbc => "cbc",
        CipherAlg::Aes128Ctr | CipherAlg::Aes192Ctr | CipherAlg::Aes256Ctr => "ctr",
        CipherAlg::Aes128Gcm | CipherAlg::Aes192Gcm | CipherAlg::Aes256Gcm => "gcm",
        CipherAlg::ChaCha20Poly1305 => "stream",
    };
    let k = v8::String::new(scope, "mode").unwrap();
    let v = v8::String::new(scope, mode).unwrap();
    obj.set(scope, k.into(), v.into());
    rv.set(obj.into());
}
