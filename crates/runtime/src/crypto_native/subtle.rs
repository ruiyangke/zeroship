//! `SubtleCrypto` — the dispatcher class. Per
//! `docs/proposals/webcrypto-native.md` §III.
//!
//! Each method synchronously executes the spec algorithm on the V8
//! thread (D-29 v1), wrapping the result in a Promise via
//! `helpers::resolve_now` / `reject_now`. Validation failures throw
//! synchronously at the promise-creation boundary (matches workerd /
//! Chrome behaviour for spec steps 1–3 of each op).

#![allow(unsafe_code)]
#![allow(clippy::too_many_arguments)]

use super::digest;
use super::helpers::{
    read_buffer_source, reject_now, resolve_now, vec_to_arraybuffer,
};
use super::key_material::KeyFormat;
use crate::state::OpError;

#[allow(unused_imports)]
use zeroship_runtime_macros::{v8_class, v8_constructor, v8_method, v8_name, v8_to_string_tag};

pub struct SubtleCrypto;

impl Default for SubtleCrypto {
    fn default() -> Self {
        SubtleCrypto
    }
}

#[v8_class]
#[v8_to_string_tag = "SubtleCrypto"]
impl SubtleCrypto {
    /// `subtle.digest(algorithm, data) -> Promise<ArrayBuffer>` —
    /// spec §32. Sync-on-V8-thread per D-29.
    #[v8_method]
    fn digest<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        alg: v8::Local<v8::Value>,
        data: v8::Local<v8::Value>,
    ) -> v8::Local<'s, v8::Value> {
        let hash = match digest::resolve_digest_algorithm(scope, alg) {
            Ok(h) => h,
            Err(e) => return reject_now(scope, e).into(),
        };
        let data_bytes = match read_buffer_source(scope, data) {
            Ok(b) => b,
            Err(e) => return reject_now(scope, e).into(),
        };
        let out = digest::digest_bytes(hash, &data_bytes);
        let ua = vec_to_arraybuffer(scope, &out);
        resolve_now(scope, ua).into()
    }

    /// `subtle.encrypt(algorithm, key, data) -> Promise<ArrayBuffer>`.
    /// AES-{CTR,CBC,GCM} + RSA-OAEP per spec §§22, 27, 28, 29.
    #[v8_method]
    fn encrypt<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        alg: v8::Local<v8::Value>,
        key: v8::Local<v8::Value>,
        data: v8::Local<v8::Value>,
    ) -> v8::Local<'s, v8::Value> {
        match super::ops::encrypt(scope, alg, key, data) {
            Ok(bytes) => {
                let ua = vec_to_arraybuffer(scope, &bytes);
                resolve_now(scope, ua).into()
            }
            Err(e) => reject_now(scope, e).into(),
        }
    }

    #[v8_method]
    fn decrypt<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        alg: v8::Local<v8::Value>,
        key: v8::Local<v8::Value>,
        data: v8::Local<v8::Value>,
    ) -> v8::Local<'s, v8::Value> {
        match super::ops::decrypt(scope, alg, key, data) {
            Ok(bytes) => {
                let ua = vec_to_arraybuffer(scope, &bytes);
                resolve_now(scope, ua).into()
            }
            Err(e) => reject_now(scope, e).into(),
        }
    }

    #[v8_method]
    fn sign<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        alg: v8::Local<v8::Value>,
        key: v8::Local<v8::Value>,
        data: v8::Local<v8::Value>,
    ) -> v8::Local<'s, v8::Value> {
        match super::ops::sign(scope, alg, key, data) {
            Ok(bytes) => {
                let ua = vec_to_arraybuffer(scope, &bytes);
                resolve_now(scope, ua).into()
            }
            Err(e) => reject_now(scope, e).into(),
        }
    }

    #[v8_method]
    fn verify<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        alg: v8::Local<v8::Value>,
        key: v8::Local<v8::Value>,
        signature: v8::Local<v8::Value>,
        data: v8::Local<v8::Value>,
    ) -> v8::Local<'s, v8::Value> {
        match super::ops::verify(scope, alg, key, signature, data) {
            Ok(b) => {
                let v: v8::Local<v8::Value> = v8::Boolean::new(scope, b).into();
                resolve_now(scope, v).into()
            }
            Err(e) => reject_now(scope, e).into(),
        }
    }

    #[v8_method]
    #[v8_name = "generateKey"]
    fn generate_key<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        alg: v8::Local<v8::Value>,
        extractable: v8::Local<v8::Value>,
        usages: v8::Local<v8::Value>,
    ) -> v8::Local<'s, v8::Value> {
        let extractable_b = extractable.boolean_value(scope);
        match super::ops::generate_key(scope, alg, extractable_b, usages) {
            Ok(value) => resolve_now(scope, value).into(),
            Err(e) => reject_now(scope, e).into(),
        }
    }

    #[v8_method]
    #[v8_name = "importKey"]
    fn import_key<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        format: v8::Local<v8::Value>,
        key_data: v8::Local<v8::Value>,
        alg: v8::Local<v8::Value>,
        extractable: v8::Local<v8::Value>,
        usages: v8::Local<v8::Value>,
    ) -> v8::Local<'s, v8::Value> {
        let format_str = format.to_rust_string_lossy(scope);
        let format_enum = match KeyFormat::from_str(&format_str) {
            Some(f) => f,
            None => {
                return reject_now(
                    scope,
                    OpError::dom(
                        "NotSupportedError",
                        format!("Unsupported key format '{}'", format_str),
                    ),
                )
                .into();
            }
        };
        let extractable_b = extractable.boolean_value(scope);
        match super::ops::import_key(
            scope,
            format_enum,
            key_data,
            alg,
            extractable_b,
            usages,
        ) {
            Ok(obj) => resolve_now(scope, obj.into()).into(),
            Err(e) => reject_now(scope, e).into(),
        }
    }

    #[v8_method]
    #[v8_name = "exportKey"]
    fn export_key<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        format: v8::Local<v8::Value>,
        key: v8::Local<v8::Value>,
    ) -> v8::Local<'s, v8::Value> {
        let format_str = format.to_rust_string_lossy(scope);
        let format_enum = match KeyFormat::from_str(&format_str) {
            Some(f) => f,
            None => {
                return reject_now(
                    scope,
                    OpError::dom(
                        "NotSupportedError",
                        format!("Unsupported key format '{}'", format_str),
                    ),
                )
                .into();
            }
        };
        match super::ops::export_key(scope, format_enum, key) {
            Ok(value) => resolve_now(scope, value).into(),
            Err(e) => reject_now(scope, e).into(),
        }
    }

    #[v8_method]
    #[v8_name = "deriveBits"]
    fn derive_bits<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        alg: v8::Local<v8::Value>,
        base_key: v8::Local<v8::Value>,
        length: v8::Local<v8::Value>,
    ) -> v8::Local<'s, v8::Value> {
        match super::ops::derive_bits(scope, alg, base_key, length) {
            Ok(bytes) => {
                let ua = vec_to_arraybuffer(scope, &bytes);
                resolve_now(scope, ua).into()
            }
            Err(e) => reject_now(scope, e).into(),
        }
    }

    #[v8_method]
    #[v8_name = "deriveKey"]
    fn derive_key<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        alg: v8::Local<v8::Value>,
        base_key: v8::Local<v8::Value>,
        derived_key_alg: v8::Local<v8::Value>,
        extractable: v8::Local<v8::Value>,
        usages: v8::Local<v8::Value>,
    ) -> v8::Local<'s, v8::Value> {
        let extractable_b = extractable.boolean_value(scope);
        match super::ops::derive_key(
            scope,
            alg,
            base_key,
            derived_key_alg,
            extractable_b,
            usages,
        ) {
            Ok(obj) => resolve_now(scope, obj.into()).into(),
            Err(e) => reject_now(scope, e).into(),
        }
    }

    #[v8_method]
    #[v8_name = "wrapKey"]
    fn wrap_key<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        format: v8::Local<v8::Value>,
        key: v8::Local<v8::Value>,
        wrapping_key: v8::Local<v8::Value>,
        wrap_alg: v8::Local<v8::Value>,
    ) -> v8::Local<'s, v8::Value> {
        let format_str = format.to_rust_string_lossy(scope);
        let format_enum = match KeyFormat::from_str(&format_str) {
            Some(f) => f,
            None => {
                return reject_now(
                    scope,
                    OpError::dom(
                        "NotSupportedError",
                        format!("Unsupported key format '{}'", format_str),
                    ),
                )
                .into();
            }
        };
        match super::ops::wrap_key(scope, format_enum, key, wrapping_key, wrap_alg) {
            Ok(bytes) => {
                let ua = vec_to_arraybuffer(scope, &bytes);
                resolve_now(scope, ua).into()
            }
            Err(e) => reject_now(scope, e).into(),
        }
    }

    #[v8_method]
    #[v8_name = "unwrapKey"]
    fn unwrap_key<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        format: v8::Local<v8::Value>,
        wrapped_key: v8::Local<v8::Value>,
        unwrapping_key: v8::Local<v8::Value>,
        unwrap_alg: v8::Local<v8::Value>,
        unwrapped_key_alg: v8::Local<v8::Value>,
        extractable: v8::Local<v8::Value>,
        usages: v8::Local<v8::Value>,
    ) -> v8::Local<'s, v8::Value> {
        let format_str = format.to_rust_string_lossy(scope);
        let format_enum = match KeyFormat::from_str(&format_str) {
            Some(f) => f,
            None => {
                return reject_now(
                    scope,
                    OpError::dom(
                        "NotSupportedError",
                        format!("Unsupported key format '{}'", format_str),
                    ),
                )
                .into();
            }
        };
        let extractable_b = extractable.boolean_value(scope);
        match super::ops::unwrap_key(
            scope,
            format_enum,
            wrapped_key,
            unwrapping_key,
            unwrap_alg,
            unwrapped_key_alg,
            extractable_b,
            usages,
        ) {
            Ok(obj) => resolve_now(scope, obj.into()).into(),
            Err(e) => reject_now(scope, e).into(),
        }
    }
}

pub fn install_global<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    global: v8::Local<v8::Object>,
) -> v8::Local<'s, v8::Function> {
    let tmpl = SubtleCrypto::install(scope);
    let class_fn = tmpl.get_function(scope).unwrap();
    let key = v8::String::new(scope, "SubtleCrypto").unwrap();
    global.set(scope, key.into(), class_fn.into());
    class_fn
}

/// Allocate a SubtleCrypto wrapper for the per-realm `crypto.subtle`
/// singleton. Same `instance_template().new_instance()` pattern as the
/// constructor codegen, but bypasses the auto-default constructor so
/// no JS-visible work happens.
pub fn build<'s>(scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Object> {
    let tmpl = SubtleCrypto::install(scope);
    let inst_tmpl = tmpl.instance_template(scope);
    let inst = inst_tmpl
        .new_instance(scope)
        .expect("SubtleCrypto instance allocation");
    let class_fn = tmpl.get_function(scope).unwrap();
    let proto_key = v8::String::new(scope, "prototype").unwrap();
    let proto_v = class_fn.get(scope, proto_key.into()).unwrap();
    inst.set_prototype(scope, proto_v);

    let boxed: Box<SubtleCrypto> = Box::new(SubtleCrypto);
    let raw = Box::into_raw(boxed);
    let raw_addr = raw as usize;
    let ext = v8::External::new(scope, raw as *mut std::ffi::c_void);
    inst.set_internal_field(0, ext.into());
    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        inst,
        Box::new(move || unsafe {
            drop(Box::from_raw(raw_addr as *mut SubtleCrypto));
        }),
    );
    std::mem::forget(weak);
    inst
}
