//! Miscellaneous node:crypto exports — `timingSafeEqual`, `getHashes`,
//! `getCiphers`, `getCurves`, `getFips`, `setFips`, `secureHeapUsed`.
//!
//! Per `docs/proposals/node-crypto-native.md` §X.2, §X.3, §II.13.

use super::super::crypto::kernel::digest::HASH_NAMES;

/// `timingSafeEqual(a, b) -> boolean`.
/// Per D-N31, the lengths must match (non-CT pre-check); the
/// equal-length compare uses aws-lc-rs's constant-time primitive.
/// Throws `ERR_CRYPTO_TIMING_SAFE_EQUAL_LENGTH` if lengths differ.
pub(crate) fn timing_safe_equal_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    if args.length() < 2 {
        let exc = crate::node_error::build_node_exception(
            scope,
            "ERR_INVALID_ARG_TYPE",
            "timingSafeEqual: requires (a, b)",
        );
        scope.throw_exception(exc);
        return;
    }
    let a_view: v8::Local<v8::ArrayBufferView> = match args.get(0).try_into() {
        Ok(v) => v,
        Err(_) => {
            let exc = crate::node_error::build_node_exception(
                scope,
                "ERR_INVALID_ARG_TYPE",
                "timingSafeEqual: a must be a Buffer/TypedArray",
            );
            scope.throw_exception(exc);
            return;
        }
    };
    let b_view: v8::Local<v8::ArrayBufferView> = match args.get(1).try_into() {
        Ok(v) => v,
        Err(_) => {
            let exc = crate::node_error::build_node_exception(
                scope,
                "ERR_INVALID_ARG_TYPE",
                "timingSafeEqual: b must be a Buffer/TypedArray",
            );
            scope.throw_exception(exc);
            return;
        }
    };
    if a_view.byte_length() != b_view.byte_length() {
        let exc = crate::node_error::build_node_exception(
            scope,
            "ERR_CRYPTO_TIMING_SAFE_EQUAL_LENGTH",
            "Input buffers must have the same byte length",
        );
        scope.throw_exception(exc);
        return;
    }
    let mut a_bytes = vec![0u8; a_view.byte_length()];
    a_view.copy_contents(&mut a_bytes);
    let mut b_bytes = vec![0u8; b_view.byte_length()];
    b_view.copy_contents(&mut b_bytes);

    let equal =
        aws_lc_rs::constant_time::verify_slices_are_equal(&a_bytes, &b_bytes).is_ok();
    rv.set(v8::Boolean::new(scope, equal).into());
}

/// `getHashes() -> string[]`. Per D-N28; iterates the kernel registry.
pub(crate) fn get_hashes_callback(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let arr = v8::Array::new(scope, HASH_NAMES.len() as i32);
    for (i, name) in HASH_NAMES.iter().enumerate() {
        let s = v8::String::new(scope, name).unwrap();
        arr.set_index(scope, i as u32, s.into());
    }
    rv.set(arr.into());
}

/// `getCiphers() -> string[]` — Stage C ships the cipher classes;
/// for now return an empty array so feature-detection code that does
/// `if (getCiphers().includes('aes-256-gcm'))` doesn't crash. The
/// list will be populated when Cipher / Decipher land (Stage C).
pub(crate) fn get_ciphers_callback(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let arr = v8::Array::new(scope, 0);
    rv.set(arr.into());
}

/// `getCurves() -> string[]` — the named curves we support.
pub(crate) fn get_curves_callback(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    // The names match WebCrypto-canonical (P-256, P-384, P-521) plus
    // Ed25519/X25519 since those are EC-flavour even if not "curves"
    // strictly. Node returns OpenSSL-canonical (prime256v1 etc.) but
    // npm packages mostly check `getCurves().includes('P-256')`.
    let names = ["P-256", "P-384", "P-521", "secp256k1", "Ed25519", "X25519"];
    let arr = v8::Array::new(scope, names.len() as i32);
    for (i, name) in names.iter().enumerate() {
        let s = v8::String::new(scope, name).unwrap();
        arr.set_index(scope, i as u32, s.into());
    }
    rv.set(arr.into());
}

/// `getFips() -> 0`. Per D-N25 — FIPS toggle is build-time.
pub(crate) fn get_fips_callback(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    rv.set(v8::Integer::new(scope, 0).into());
}

/// `setFips(boolean)` — throws if `true`. Per D-N25.
pub(crate) fn set_fips_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let val = args.get(0);
    if val.is_true() {
        let exc = crate::node_error::build_node_exception(
            scope,
            "ERR_CRYPTO_OPERATION_FAILED",
            "FIPS mode toggle not supported",
        );
        scope.throw_exception(exc);
    }
    // setFips(false) is a no-op (already non-FIPS).
}

/// `secureHeapUsed()` — stub returning zero values per D-N25.
pub(crate) fn secure_heap_used_callback(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let obj = v8::Object::new(scope);
    let zero = v8::Integer::new(scope, 0);
    let zero_f = v8::Number::new(scope, 0.0);
    let total_k = v8::String::new(scope, "total").unwrap();
    obj.set(scope, total_k.into(), zero.into());
    let min_k = v8::String::new(scope, "min").unwrap();
    obj.set(scope, min_k.into(), zero.into());
    let used_k = v8::String::new(scope, "used").unwrap();
    obj.set(scope, used_k.into(), zero.into());
    let util_k = v8::String::new(scope, "utilization").unwrap();
    obj.set(scope, util_k.into(), zero_f.into());
    rv.set(obj.into());
}
