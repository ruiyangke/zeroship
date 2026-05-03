//! KDF ops — `pbkdf2Sync`, `pbkdf2`, `hkdfSync`, `hkdf`.
//!
//! Per `docs/proposals/node-crypto-native.md` §VI.2 (D-N15).
//!
//! Sync variants run on the V8 thread (user opted in by picking the
//! `*Sync` API). Async variants (`pbkdf2(...callback)` and the
//! Promise-shaped form) currently run synchronously on the V8 thread
//! and resolve a Promise / fire the callback synchronously via the
//! microtask queue. A future commit can wire the spawn_blocking
//! threadpool variant per D-N5; npm packages that block the event
//! loop with PBKDF2 1M iterations from request handlers are doing
//! something wrong (XVII.6).
//!
//! scrypt is in the design (Stage B FFI inventory) but requires
//! aws-lc-sys raw FFI; not yet wired here.

use super::buffer;
use super::super::crypto::kernel::digest::KernelHashAlgo;
use super::super::crypto::kernel::error::KernelError;
use super::super::crypto::kernel::kdf;
use crate::state::OpError;

fn map_kdf_err(err: KernelError) -> OpError {
    match err {
        KernelError::UnsupportedAlgorithm(name) => OpError::node(
            "ERR_OSSL_EVP_INVALID_DIGEST",
            format!("Unsupported algorithm: {name}"),
        ),
        KernelError::InvalidKdfParams(s) => OpError::node(
            "ERR_OUT_OF_RANGE",
            format!("Invalid KDF parameters: {s}"),
        ),
        _ => OpError::node(
            "ERR_CRYPTO_OPERATION_FAILED",
            format!("KDF operation failed: {err}"),
        ),
    }
}

/// `pbkdf2Sync(password, salt, iterations, keylen, digest) -> Buffer`.
pub(crate) fn pbkdf2_sync_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    if args.length() < 5 {
        let exc = crate::node_error::build_node_exception(
            scope,
            "ERR_INVALID_ARG_TYPE",
            "pbkdf2Sync: requires (password, salt, iterations, keylen, digest)",
        );
        scope.throw_exception(exc);
        return;
    }
    let result = run_pbkdf2(scope, args.get(0), args.get(1), args.get(2), args.get(3), args.get(4));
    match result {
        Ok(out) => {
            rv.set(buffer::emit_buffer(scope, &out));
        }
        Err(err) => {
            let exc = crate::web::crypto::helpers::op_error_to_v8(scope, err);
            scope.throw_exception(exc);
        }
    }
}

/// `pbkdf2(password, salt, iterations, keylen, digest, callback) -> void`.
/// Currently runs sync; fires callback async via microtask. A future
/// commit can spawn_blocking for high iteration counts (D-N5).
pub(crate) fn pbkdf2_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    if args.length() < 6 {
        let exc = crate::node_error::build_node_exception(
            scope,
            "ERR_INVALID_ARG_TYPE",
            "pbkdf2: requires (password, salt, iterations, keylen, digest, callback)",
        );
        scope.throw_exception(exc);
        return;
    }
    let cb_val = args.get(5);
    if !cb_val.is_function() {
        let exc = crate::node_error::build_node_exception(
            scope,
            "ERR_INVALID_ARG_TYPE",
            "callback must be a function",
        );
        scope.throw_exception(exc);
        return;
    }
    let result = run_pbkdf2(scope, args.get(0), args.get(1), args.get(2), args.get(3), args.get(4));
    let cb: v8::Local<v8::Function> = cb_val.try_into().unwrap();
    match result {
        Ok(out) => {
            let buf = buffer::emit_buffer(scope, &out);
            super::random_callback_helpers::schedule_node_cb(scope, cb, None, Some(buf));
        }
        Err(err) => {
            let exc = crate::web::crypto::helpers::op_error_to_v8(scope, err);
            super::random_callback_helpers::schedule_node_cb(scope, cb, Some(exc), None);
        }
    }
}

fn run_pbkdf2(
    scope: &mut v8::PinScope,
    password_v: v8::Local<v8::Value>,
    salt_v: v8::Local<v8::Value>,
    iterations_v: v8::Local<v8::Value>,
    keylen_v: v8::Local<v8::Value>,
    digest_v: v8::Local<v8::Value>,
) -> Result<Vec<u8>, OpError> {
    // Default encoding for string password / salt is utf8 per Node.
    let password = buffer::extract_input(scope, password_v, Some("utf8"))?;
    let salt = buffer::extract_input(scope, salt_v, Some("utf8"))?;
    let iterations = iterations_v.uint32_value(scope).ok_or_else(|| {
        OpError::node("ERR_INVALID_ARG_TYPE", "iterations must be a number")
    })?;
    let keylen = keylen_v.uint32_value(scope).ok_or_else(|| {
        OpError::node("ERR_INVALID_ARG_TYPE", "keylen must be a number")
    })? as usize;
    let digest = digest_v.to_rust_string_lossy(scope);
    let algo = KernelHashAlgo::from_str(&digest).ok_or_else(|| {
        OpError::node(
            "ERR_OSSL_EVP_INVALID_DIGEST",
            format!("Digest method not supported: {digest}"),
        )
    })?;
    kdf::pbkdf2(algo, &password, &salt, iterations, keylen).map_err(map_kdf_err)
}

/// `hkdfSync(digest, ikm, salt, info, keylen) -> ArrayBuffer`.
/// Per Node spec, hkdf returns ArrayBuffer (not Buffer).
pub(crate) fn hkdf_sync_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    if args.length() < 5 {
        let exc = crate::node_error::build_node_exception(
            scope,
            "ERR_INVALID_ARG_TYPE",
            "hkdfSync: requires (digest, ikm, salt, info, keylen)",
        );
        scope.throw_exception(exc);
        return;
    }
    let result = run_hkdf(scope, args.get(0), args.get(1), args.get(2), args.get(3), args.get(4));
    match result {
        Ok(out) => {
            // hkdf returns ArrayBuffer per Node spec.
            let len = out.len();
            let ab = v8::ArrayBuffer::new(scope, len);
            if len > 0 {
                let store = ab.get_backing_store();
                for (i, &b) in out.iter().enumerate() {
                    store[i].set(b);
                }
            }
            rv.set(ab.into());
        }
        Err(err) => {
            let exc = crate::web::crypto::helpers::op_error_to_v8(scope, err);
            scope.throw_exception(exc);
        }
    }
}

/// `hkdf(digest, ikm, salt, info, keylen, callback) -> void`.
pub(crate) fn hkdf_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    if args.length() < 6 {
        let exc = crate::node_error::build_node_exception(
            scope,
            "ERR_INVALID_ARG_TYPE",
            "hkdf: requires (digest, ikm, salt, info, keylen, callback)",
        );
        scope.throw_exception(exc);
        return;
    }
    let cb_val = args.get(5);
    if !cb_val.is_function() {
        let exc = crate::node_error::build_node_exception(
            scope,
            "ERR_INVALID_ARG_TYPE",
            "callback must be a function",
        );
        scope.throw_exception(exc);
        return;
    }
    let result = run_hkdf(scope, args.get(0), args.get(1), args.get(2), args.get(3), args.get(4));
    let cb: v8::Local<v8::Function> = cb_val.try_into().unwrap();
    match result {
        Ok(out) => {
            let len = out.len();
            let ab = v8::ArrayBuffer::new(scope, len);
            if len > 0 {
                let store = ab.get_backing_store();
                for (i, &b) in out.iter().enumerate() {
                    store[i].set(b);
                }
            }
            super::random_callback_helpers::schedule_node_cb(scope, cb, None, Some(ab.into()));
        }
        Err(err) => {
            let exc = crate::web::crypto::helpers::op_error_to_v8(scope, err);
            super::random_callback_helpers::schedule_node_cb(scope, cb, Some(exc), None);
        }
    }
}

fn run_hkdf(
    scope: &mut v8::PinScope,
    digest_v: v8::Local<v8::Value>,
    ikm_v: v8::Local<v8::Value>,
    salt_v: v8::Local<v8::Value>,
    info_v: v8::Local<v8::Value>,
    keylen_v: v8::Local<v8::Value>,
) -> Result<Vec<u8>, OpError> {
    let digest = digest_v.to_rust_string_lossy(scope);
    let algo = KernelHashAlgo::from_str(&digest).ok_or_else(|| {
        OpError::node(
            "ERR_OSSL_EVP_INVALID_DIGEST",
            format!("Digest method not supported: {digest}"),
        )
    })?;
    let ikm = buffer::extract_input(scope, ikm_v, Some("utf8"))?;
    let salt = if salt_v.is_undefined() || salt_v.is_null() {
        Vec::new()
    } else {
        buffer::extract_input(scope, salt_v, Some("utf8"))?
    };
    let info = if info_v.is_undefined() || info_v.is_null() {
        Vec::new()
    } else {
        buffer::extract_input(scope, info_v, Some("utf8"))?
    };
    let keylen = keylen_v.uint32_value(scope).ok_or_else(|| {
        OpError::node("ERR_INVALID_ARG_TYPE", "keylen must be a number")
    })? as usize;
    kdf::hkdf(algo, &ikm, &salt, &info, keylen).map_err(map_kdf_err)
}

/// `scryptSync(password, salt, keylen, options?) -> Buffer`.
/// Per Node's signature (https://nodejs.org/api/crypto.html#cryptoscryptsyncpassword-salt-keylen-options).
pub(crate) fn scrypt_sync_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    if args.length() < 3 {
        let exc = crate::node_error::build_node_exception(
            scope,
            "ERR_INVALID_ARG_TYPE",
            "scryptSync: requires (password, salt, keylen, options?)",
        );
        scope.throw_exception(exc);
        return;
    }
    let options = if args.length() >= 4 {
        Some(args.get(3))
    } else {
        None
    };
    match run_scrypt(scope, args.get(0), args.get(1), args.get(2), options) {
        Ok(out) => rv.set(buffer::emit_buffer(scope, &out)),
        Err(err) => {
            let exc = crate::web::crypto::helpers::op_error_to_v8(scope, err);
            scope.throw_exception(exc);
        }
    }
}

/// `scrypt(password, salt, keylen, options?, callback) -> void`.
pub(crate) fn scrypt_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let n = args.length();
    if n < 4 {
        let exc = crate::node_error::build_node_exception(
            scope,
            "ERR_INVALID_ARG_TYPE",
            "scrypt: requires (password, salt, keylen, [options], callback)",
        );
        scope.throw_exception(exc);
        return;
    }
    let cb_val = args.get(n - 1);
    if !cb_val.is_function() {
        let exc = crate::node_error::build_node_exception(
            scope,
            "ERR_INVALID_ARG_TYPE",
            "callback must be a function",
        );
        scope.throw_exception(exc);
        return;
    }
    // (password, salt, keylen, callback) — 4 args.
    // (password, salt, keylen, options, callback) — 5 args.
    let options = if n >= 5 { Some(args.get(3)) } else { None };
    let result = run_scrypt(scope, args.get(0), args.get(1), args.get(2), options);
    let cb: v8::Local<v8::Function> = cb_val.try_into().unwrap();
    match result {
        Ok(out) => {
            let buf = buffer::emit_buffer(scope, &out);
            super::random_callback_helpers::schedule_node_cb(scope, cb, None, Some(buf));
        }
        Err(err) => {
            let exc = crate::web::crypto::helpers::op_error_to_v8(scope, err);
            super::random_callback_helpers::schedule_node_cb(scope, cb, Some(exc), None);
        }
    }
}

fn run_scrypt(
    scope: &mut v8::PinScope,
    password_v: v8::Local<v8::Value>,
    salt_v: v8::Local<v8::Value>,
    keylen_v: v8::Local<v8::Value>,
    options_v: Option<v8::Local<v8::Value>>,
) -> Result<Vec<u8>, OpError> {
    let password = buffer::extract_input(scope, password_v, Some("utf8"))?;
    let salt = buffer::extract_input(scope, salt_v, Some("utf8"))?;
    let keylen = keylen_v.uint32_value(scope).ok_or_else(|| {
        OpError::node("ERR_INVALID_ARG_TYPE", "keylen must be a number")
    })? as usize;
    // Defaults per Node: N=16384, r=8, p=1, maxmem=32 MiB.
    let mut n: u64 = 16384;
    let mut r: u64 = 8;
    let mut p: u64 = 1;
    let mut max_mem: usize = 32 * 1024 * 1024;
    if let Some(opt) = options_v {
        if !opt.is_undefined() && !opt.is_null() {
            if let Ok(obj) = v8::Local::<v8::Object>::try_from(opt) {
                let read_u64 = |scope: &mut v8::PinScope, k: &str| -> Option<u64> {
                    let key = v8::String::new(scope, k).unwrap();
                    obj.get(scope, key.into())
                        .and_then(|v| if v.is_undefined() { None } else { Some(v) })
                        .and_then(|v| v.number_value(scope))
                        .map(|f| f as u64)
                };
                if let Some(v) = read_u64(scope, "N").or_else(|| read_u64(scope, "cost")) {
                    n = v;
                }
                if let Some(v) = read_u64(scope, "r").or_else(|| read_u64(scope, "blockSize")) {
                    r = v;
                }
                if let Some(v) = read_u64(scope, "p").or_else(|| read_u64(scope, "parallelization")) {
                    p = v;
                }
                if let Some(v) = read_u64(scope, "maxmem") {
                    max_mem = v as usize;
                }
            }
        }
    }
    kdf::scrypt(&password, &salt, n, r, p, max_mem, keylen).map_err(map_kdf_err)
}
