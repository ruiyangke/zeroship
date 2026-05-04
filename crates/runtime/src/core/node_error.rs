//! Node.js error code → JS exception class mapping.
//!
//! Per `docs/proposals/node-crypto-native.md` §VII.3a — every
//! `ERR_CRYPTO_*`, `ERR_INVALID_*`, `ERR_OUT_OF_RANGE`, etc., that we
//! emit from `crate::node::crypto` is in this table with the right
//! exception class (Error vs TypeError vs RangeError).
//!
//! The map is a `match` rather than a phf::Map because the call site
//! is in the macro-emitted throw arm — phf adds a runtime hashmap and
//! a build-time crate dep we don't need for ~30 string keys.

/// Three JS exception classes Node attaches to an `e.code` payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeErrorClass {
    /// Plain `Error` instance.
    Error,
    /// `TypeError` instance (wrong arg type).
    TypeError,
    /// `RangeError` instance (arg out of range).
    RangeError,
}

/// Look up the exception class for a Node error code. Default is
/// `Error`. Per §VII.3a — verified against `lib/internal/errors.js`
/// and `src/node_errors.h` from upstream Node (see provenance column
/// in the design doc).
pub const fn class_for(code: &str) -> NodeErrorClass {
    match code.as_bytes() {
        // -- TypeError class (per upstream errors.js / node_errors.h) --
        b"ERR_INVALID_ARG_TYPE"
        | b"ERR_INVALID_ARG_VALUE"
        | b"ERR_CRYPTO_INVALID_DIGEST"
        | b"ERR_CRYPTO_INVALID_IV"
        | b"ERR_CRYPTO_INVALID_AUTH_TAG"
        | b"ERR_CRYPTO_UNKNOWN_CIPHER"
        | b"ERR_CRYPTO_INCOMPATIBLE_KEY"
        | b"ERR_CRYPTO_INVALID_KEY_OBJECT_TYPE"
        | b"ERR_OSSL_EVP_INVALID_DIGEST"
        | b"ERR_MISSING_PASSPHRASE"
        | b"ERR_UNKNOWN_ENCODING"
        | b"ERR_MISSING_OPTION" => NodeErrorClass::TypeError,
        // -- RangeError class --
        b"ERR_OUT_OF_RANGE"
        | b"ERR_BUFFER_OUT_OF_BOUNDS"
        | b"ERR_INVALID_BUFFER_SIZE"
        | b"ERR_CRYPTO_INVALID_KEYLEN"
        | b"ERR_CRYPTO_INVALID_TAG_LENGTH"
        | b"ERR_CRYPTO_INVALID_SCRYPT_PARAMS" => NodeErrorClass::RangeError,
        // -- everything else: plain Error
        _ => NodeErrorClass::Error,
    }
}

/// Build a JS exception value for a Node error: pick class, set
/// `e.code`, return the resulting `v8::Local<v8::Value>` ready to be
/// passed to `scope.throw_exception(...)`.
pub fn build_node_exception<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    code: &'static str,
    message: &str,
) -> v8::Local<'s, v8::Value> {
    let msg = v8::String::new(scope, message).unwrap();
    let exc: v8::Local<v8::Value> = match class_for(code) {
        NodeErrorClass::TypeError => v8::Exception::type_error(scope, msg),
        NodeErrorClass::RangeError => v8::Exception::range_error(scope, msg),
        NodeErrorClass::Error => v8::Exception::error(scope, msg),
    };
    // Attach `e.code = code`. Errors are objects in V8, so try_into
    // succeeds; if it doesn't (shouldn't happen) we silently skip the
    // assignment so the throw still proceeds.
    if let Ok(obj) = v8::Local::<v8::Object>::try_from(exc) {
        let code_key = v8::String::new(scope, "code").unwrap();
        let code_val = v8::String::new(scope, code).unwrap();
        obj.set(scope, code_key.into(), code_val.into());
    }
    exc
}
