//! Shared helpers for the WebCrypto native module.
//!
//! - `read_buffer_source` — read a `BufferSource` (`ArrayBuffer` or
//!   `ArrayBufferView`) into a `Vec<u8>`. Matches the existing macro
//!   pattern (`#[v8_class]` `Vec<u8>` extraction). Throws TypeError on
//!   mismatch.
//! - `read_optional_buffer_source` — same but returns `None` on
//!   undefined / null.
//! - `read_optional_string` / `read_optional_bool` — small JS-object
//!   field walkers used by JWK and the algorithm registry.
//! - `vec_to_uint8array` — wrap Rust bytes as a fresh `Uint8Array`
//!   (the spec return shape for `digest`, `encrypt`, `sign`, etc.).
//! - `resolve_now` / `reject_now` — synchronous resolution helpers for
//!   the v1 "sync-on-V8-thread" execution model.

use crate::state::OpError;

/// Read a `BufferSource` (ArrayBuffer or ArrayBufferView) into an
/// owned `Vec<u8>`. Returns `Err(TypeError)` if the value is neither.
/// Matches the macro's `Vec<u8>` extraction shape.
pub fn read_buffer_source(
    scope: &mut v8::PinScope,
    value: v8::Local<v8::Value>,
) -> Result<Vec<u8>, OpError> {
    if let Ok(view) = v8::Local::<v8::ArrayBufferView>::try_from(value) {
        let mut buf = vec![0u8; view.byte_length()];
        view.copy_contents(&mut buf);
        return Ok(buf);
    }
    if let Ok(ab) = v8::Local::<v8::ArrayBuffer>::try_from(value) {
        let store = ab.get_backing_store();
        let len = ab.byte_length();
        let mut buf = vec![0u8; len];
        for i in 0..len {
            buf[i] = store[i].get();
        }
        return Ok(buf);
    }
    let _ = scope;
    Err(OpError::type_error("Expected a BufferSource (ArrayBuffer or ArrayBufferView)"))
}

pub fn read_optional_buffer_source(
    scope: &mut v8::PinScope,
    value: v8::Local<v8::Value>,
) -> Result<Option<Vec<u8>>, OpError> {
    if value.is_undefined() || value.is_null() {
        return Ok(None);
    }
    Ok(Some(read_buffer_source(scope, value)?))
}

/// Read an optional string field from a JS object. Returns `Err(TypeError)`
/// if the property exists and is non-undefined but isn't a string.
pub fn read_optional_string(
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
        return Err(OpError::type_error(format!(
            "JWK field '{}' must be a string",
            name
        )));
    }
    Ok(Some(v.to_rust_string_lossy(scope)))
}

/// Read a required string field from a JS object. Returns
/// `Err(TypeError)` if the field is missing, undefined, or non-string.
pub fn read_required_string(
    scope: &mut v8::PinScope,
    obj: v8::Local<v8::Object>,
    name: &str,
) -> Result<String, OpError> {
    let key = v8::String::new(scope, name).unwrap();
    let v = obj.get(scope, key.into()).ok_or_else(|| {
        OpError::type_error(format!("Missing required field '{}'", name))
    })?;
    if v.is_undefined() || v.is_null() {
        return Err(OpError::type_error(format!(
            "Missing required field '{}'",
            name
        )));
    }
    if !v.is_string() {
        return Err(OpError::type_error(format!(
            "Field '{}' must be a string",
            name
        )));
    }
    Ok(v.to_rust_string_lossy(scope))
}

/// Read a required object field. Returns `Err(TypeError)` on missing
/// / non-object value.
pub fn read_required_object<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    obj: v8::Local<v8::Object>,
    name: &str,
) -> Result<v8::Local<'s, v8::Object>, OpError> {
    let key = v8::String::new(scope, name).unwrap();
    let v = obj.get(scope, key.into()).ok_or_else(|| {
        OpError::type_error(format!("Missing required field '{}'", name))
    })?;
    v8::Local::<v8::Object>::try_from(v).map_err(|_| {
        OpError::type_error(format!("Field '{}' must be an object", name))
    })
}

pub fn read_optional_bool(
    scope: &mut v8::PinScope,
    obj: v8::Local<v8::Object>,
    name: &str,
) -> Option<bool> {
    let key = v8::String::new(scope, name).unwrap();
    let v = obj.get(scope, key.into())?;
    if v.is_undefined() || v.is_null() {
        return None;
    }
    Some(v.boolean_value(scope))
}

pub fn read_optional_string_array(
    scope: &mut v8::PinScope,
    obj: v8::Local<v8::Object>,
    name: &str,
) -> Result<Option<Vec<String>>, OpError> {
    let key = v8::String::new(scope, name).unwrap();
    let v = match obj.get(scope, key.into()) {
        Some(v) => v,
        None => return Ok(None),
    };
    if v.is_undefined() || v.is_null() {
        return Ok(None);
    }
    let arr = v8::Local::<v8::Array>::try_from(v).map_err(|_| {
        OpError::type_error(format!("Field '{}' must be an array", name))
    })?;
    let len = arr.length();
    let mut out = Vec::with_capacity(len as usize);
    for i in 0..len {
        let elem = arr.get_index(scope, i).ok_or_else(|| {
            OpError::type_error(format!("Field '{}' element {} unreadable", name, i))
        })?;
        if !elem.is_string() {
            return Err(OpError::type_error(format!(
                "Field '{}' must be an array of strings",
                name
            )));
        }
        out.push(elem.to_rust_string_lossy(scope));
    }
    Ok(Some(out))
}

/// Build a fresh `Uint8Array` from owned bytes. Matches the macro's
/// `Vec<u8>` return shape.
pub fn vec_to_uint8array<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    bytes: &[u8],
) -> v8::Local<'s, v8::Value> {
    let len = bytes.len();
    let ab = v8::ArrayBuffer::new(scope, len);
    let store = ab.get_backing_store();
    for (i, &b) in bytes.iter().enumerate() {
        store[i].set(b);
    }
    let arr = v8::Uint8Array::new(scope, ab, 0, len).unwrap();
    arr.into()
}

/// Build a fresh `ArrayBuffer` (not a Uint8Array view) from a byte
/// slice. Per W3C WebCrypto §17.4 (exportKey) and similar, the output
/// type is always an ArrayBuffer for raw/spki/pkcs8 formats.
pub fn vec_to_arraybuffer<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    bytes: &[u8],
) -> v8::Local<'s, v8::Value> {
    let len = bytes.len();
    let ab = v8::ArrayBuffer::new(scope, len);
    let store = ab.get_backing_store();
    for (i, &b) in bytes.iter().enumerate() {
        store[i].set(b);
    }
    ab.into()
}

/// Build a fresh `Uint8Array` from owned bytes, returning the array
/// type rather than a generic Value. Used where the call-site needs
/// the typed Local for further set_index etc.
pub fn vec_to_uint8array_typed<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    bytes: &[u8],
) -> v8::Local<'s, v8::Uint8Array> {
    let len = bytes.len();
    let ab = v8::ArrayBuffer::new(scope, len);
    let store = ab.get_backing_store();
    for (i, &b) in bytes.iter().enumerate() {
        store[i].set(b);
    }
    v8::Uint8Array::new(scope, ab, 0, len).unwrap()
}

/// Synchronously resolve a fresh Promise with the given JS value.
/// Every WebCrypto op runs on the V8 thread; the microtask hop happens
/// via V8's promise-resolution semantics.
pub fn resolve_now<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    value: v8::Local<'s, v8::Value>,
) -> v8::Local<'s, v8::Promise> {
    let resolver = v8::PromiseResolver::new(scope).unwrap();
    let p = resolver.get_promise(scope);
    resolver.resolve(scope, value);
    p
}

/// Synchronously reject a fresh Promise with the given OpError.
pub fn reject_now<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    err: OpError,
) -> v8::Local<'s, v8::Promise> {
    let resolver = v8::PromiseResolver::new(scope).unwrap();
    let p = resolver.get_promise(scope);
    let exc = op_error_to_v8(scope, err);
    resolver.reject(scope, exc);
    p
}

/// Materialise an OpError as a V8 exception value (TypeError /
/// RangeError / DOMException / NodeError / Error per kind).
pub fn op_error_to_v8<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    err: OpError,
) -> v8::Local<'s, v8::Value> {
    // JsValue passthrough — surface the captured user exception
    // verbatim. Skip the message translation; the captured value IS
    // the exception (with its own message, prototype, props).
    if let crate::state::OpErrorKind::JsValue(global) = &err.kind {
        return v8::Local::new(scope, global);
    }
    let msg = v8::String::new(scope, &err.message).unwrap();
    match &err.kind {
        crate::state::OpErrorKind::TypeError => v8::Exception::type_error(scope, msg),
        crate::state::OpErrorKind::RangeError => v8::Exception::range_error(scope, msg),
        crate::state::OpErrorKind::Error => v8::Exception::error(scope, msg),
        crate::state::OpErrorKind::DomException(name) => {
            crate::dom::exception::build(scope, &err.message, name).into()
        }
        crate::state::OpErrorKind::NodeError(code) => {
            crate::node_error::build_node_exception(scope, code, &err.message)
        }
        crate::state::OpErrorKind::CodedError { code, hint } => {
            let exc = v8::Exception::error(scope, msg);
            if let Ok(obj) = v8::Local::<v8::Object>::try_from(exc) {
                let code_key = v8::String::new(scope, "code").unwrap();
                let code_val = v8::String::new(scope, code).unwrap();
                obj.set(scope, code_key.into(), code_val.into());
                if let Some(h) = hint {
                    let hint_key = v8::String::new(scope, "hint").unwrap();
                    let hint_val = v8::String::new(scope, h).unwrap();
                    obj.set(scope, hint_key.into(), hint_val.into());
                }
            }
            exc
        }
        // Unreachable due to early-return above; keeps the match
        // exhaustive for the compiler.
        crate::state::OpErrorKind::JsValue(_) => unreachable!(),
    }
}

// ---------------------------------------------------------------------------
// base64url (RFC 4648 §5) — small fast path for JWK fields
// ---------------------------------------------------------------------------

pub fn base64url_decode(s: &str) -> Result<Vec<u8>, OpError> {
    use base64::Engine;
    // Convert URL-safe to standard alphabet by character mapping; pad
    // to multiple of 4 with '=' so the decoder accepts.
    let mut converted = String::with_capacity(s.len() + 3);
    for c in s.chars() {
        match c {
            '-' => converted.push('+'),
            '_' => converted.push('/'),
            // Ignore whitespace and CR/LF (RFC 7515 sec. 2 says JWK
            // values are unpadded base64url; but some producers pad).
            ' ' | '\t' | '\n' | '\r' => {}
            _ => converted.push(c),
        }
    }
    while converted.len() % 4 != 0 {
        converted.push('=');
    }
    base64::engine::general_purpose::STANDARD
        .decode(&converted)
        .map_err(|_| OpError::dom("DataError", "JWK base64url decode failed"))
}

pub fn base64url_encode(bytes: &[u8]) -> String {
    use base64::Engine;
    let s = base64::engine::general_purpose::STANDARD.encode(bytes);
    // Strip trailing '=' padding, swap +/ for -_.
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '=' => break,
            '+' => out.push('-'),
            '/' => out.push('_'),
            other => out.push(other),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// CSPRNG fill (shared with crypto.rs's thread-local 4 KB buffer)
// ---------------------------------------------------------------------------

pub fn fill_random(out: &mut [u8]) {
    crate::crypto::fast_random(out);
}
