//! `Crypto` (the global `crypto`) — `getRandomValues`, `randomUUID`,
//! `subtle` getter. Per `docs/proposals/webcrypto-native.md` §II.

#![allow(unsafe_code)]

use super::helpers::fill_random;
use crate::state::OpError;

#[allow(unused_imports)]
use zeroship_runtime_macros::{
    v8_class, v8_constructor, v8_getter, v8_method, v8_name, v8_to_string_tag,
};

const HEX: &[u8; 16] = b"0123456789abcdef";

pub struct Crypto;

impl Default for Crypto {
    fn default() -> Self {
        Crypto
    }
}

#[v8_class]
#[v8_to_string_tag = "Crypto"]
impl Crypto {
    /// `getRandomValues(arr)` — spec §10.1.1.
    #[v8_method]
    #[v8_name = "getRandomValues"]
    fn get_random_values<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        array: v8::Local<v8::Value>,
    ) -> Result<v8::Local<'s, v8::Value>, OpError> {
        // Step 1: type filter — reject Float32Array, Float64Array,
        // DataView, and any non-typed-array.
        if !is_allowed_typed_array(array) {
            return Err(OpError::dom(
                "TypeMismatchError",
                "getRandomValues argument must be one of Int8Array, Uint8Array, \
                 Uint8ClampedArray, Int16Array, Uint16Array, Int32Array, \
                 Uint32Array, BigInt64Array, BigUint64Array",
            ));
        }
        let view: v8::Local<v8::ArrayBufferView> = array.try_into().unwrap();
        let byte_len = view.byte_length();

        // Step 2: quota — DOMException QuotaExceededError.
        if byte_len > 65536 {
            return Err(OpError::dom(
                "QuotaExceededError",
                format!("getRandomValues quota exceeded ({byte_len} > 65536 bytes)"),
            ));
        }
        if byte_len == 0 {
            return Ok(v8::Local::new(scope, array));
        }
        let mut tmp = vec![0u8; byte_len];
        fill_random(&mut tmp);
        let ab = view.buffer(scope).ok_or_else(|| {
            OpError::error("getRandomValues: backing buffer unavailable")
        })?;
        let offset = view.byte_offset();
        let store = ab.get_backing_store();
        for (i, &byte) in tmp.iter().enumerate() {
            store[offset + i].set(byte);
        }
        Ok(v8::Local::new(scope, array))
    }

    /// `randomUUID()` — spec §10.1.2 + RFC 4122 §4.4.
    #[v8_method]
    #[v8_name = "randomUUID"]
    fn random_uuid(&self) -> String {
        let mut b = [0u8; 16];
        fill_random(&mut b);
        b[6] = (b[6] & 0x0f) | 0x40; // version 4
        b[8] = (b[8] & 0x3f) | 0x80; // variant 10xx

        let mut buf = [0u8; 36];
        let mut p = 0;
        for (i, &byte) in b.iter().enumerate() {
            if i == 4 || i == 6 || i == 8 || i == 10 {
                buf[p] = b'-';
                p += 1;
            }
            buf[p] = HEX[(byte >> 4) as usize];
            p += 1;
            buf[p] = HEX[(byte & 0x0f) as usize];
            p += 1;
        }
        // SAFETY: buf is ASCII (hex digits + hyphens). (Critic #34.)
        unsafe { String::from_utf8_unchecked(buf.to_vec()) }
    }

    /// `crypto.subtle` — `[SameObject]` getter per WebCrypto §10.
    /// Returns the per-realm SubtleCrypto singleton; the macro's
    /// `same_object` flag stashes the minted Object on a private symbol
    /// of the wrapper instance so subsequent reads return the SAME JS
    /// object (`crypto.subtle === crypto.subtle`).
    #[v8_getter(same_object)]
    fn subtle(&self, scope: &mut v8::PinScope) -> v8::Global<v8::Object> {
        let inst = super::subtle::build(scope);
        v8::Global::new(scope, inst)
    }
}

fn is_allowed_typed_array(value: v8::Local<v8::Value>) -> bool {
    // Spec §10.1.1 step 1: must be one of Int8/Uint8/Uint8Clamped/
    // Int16/Uint16/Int32/Uint32/BigInt64/BigUint64 only. Reject every
    // other ArrayBufferView (DataView, Float16/32/64Array). Listing
    // the allowed types explicitly catches both today's float types
    // and any future TypedArray (Float16Array landed in V8 ~v138).
    value.is_int8_array()
        || value.is_uint8_array()
        || value.is_uint8_clamped_array()
        || value.is_int16_array()
        || value.is_uint16_array()
        || value.is_int32_array()
        || value.is_uint32_array()
        || value.is_big_int64_array()
        || value.is_big_uint64_array()
}

/// Allocate a `Crypto` JS object (the per-realm `globalThis.crypto`
/// singleton). Bypasses the auto-default constructor.
pub fn build<'s>(scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Object> {
    let tmpl = Crypto::install(scope);
    let inst_tmpl = tmpl.instance_template(scope);
    let inst = inst_tmpl.new_instance(scope).expect("Crypto instance allocation");
    let class_fn = tmpl.get_function(scope).unwrap();
    let proto_key = v8::String::new(scope, "prototype").unwrap();
    let proto_v = class_fn.get(scope, proto_key.into()).unwrap();
    inst.set_prototype(scope, proto_v);

    let boxed: Box<Crypto> = Box::new(Crypto);
    let raw = Box::into_raw(boxed);
    let raw_addr = raw as usize;
    let ext = v8::External::new(scope, raw as *mut std::ffi::c_void);
    inst.set_internal_field(0, ext.into());
    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        inst,
        Box::new(move || unsafe {
            drop(Box::from_raw(raw_addr as *mut Crypto));
        }),
    );
    std::mem::forget(weak);
    inst
}

pub fn install_global<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    global: v8::Local<v8::Object>,
) {
    // The `subtle` getter is now wired via `#[v8_getter(same_object)]`
    // on the impl block — the macro emits a private-symbol-cached
    // accessor, so we no longer need a hand-rolled callback here.
    let tmpl = Crypto::install(scope);

    let class_fn = tmpl.get_function(scope).unwrap();
    let crypto_class_key = v8::String::new(scope, "Crypto").unwrap();
    global.set(scope, crypto_class_key.into(), class_fn.into());

    // Install the global `crypto` instance (a single Crypto wrapper
    // per realm). The `subtle` getter caches a per-realm SubtleCrypto
    // singleton on first access.
    let inst = build(scope);
    let crypto_key = v8::String::new(scope, "crypto").unwrap();
    global.set(scope, crypto_key.into(), inst.into());
}
