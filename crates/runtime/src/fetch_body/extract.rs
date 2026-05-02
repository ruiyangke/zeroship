//! Fetch §3.2 "extract a body" (https://fetch.spec.whatwg.org/#concept-bodyinit-extract).
//!
//! Maps a `BodyInit` JS value to a `(BodyImpl, Option<Content-Type>)`
//! pair. The Content-Type default is set on Request/Response headers
//! ONLY if the user didn't already provide one (per Fetch §5.4 step 36
//! / §5.5 step 12).
//!
//! ## v2 fixes from the design
//!
//! - **C-10 dispatch order**: Blob, byte sequence (BufferSource), then
//!   FormData, URLSearchParams, scalar value string, ReadableStream.
//!   Explicit type-test predicates BEFORE `to_rust_string_lossy` so a
//!   user-typed-array body doesn't accidentally turn into the literal
//!   string `"[object Uint8Array]"`. v1 has no native Blob class, so the
//!   "Blob" arm is a duck-type fallback (an object whose
//!   `Symbol.toStringTag` is "Blob") plus the `arrayBuffer()` /
//!   `bytes()` / `text()` methods — but for v1 we just skip the Blob
//!   arm and document it as deferred (the polyfill Blob lives in
//!   `embed/blob.js`; the native Blob ships in a later chunk).
//!
//! - **C-11 USVString conversion**: a string body must be UTF-8-encoded
//!   per the spec's USVString → bytes step. Lone surrogates are
//!   replaced with U+FFFD. NOT `to_rust_string_lossy` directly because
//!   that quietly mishandles strings with embedded U+0000 boundaries
//!   in some V8 versions. We use `String::write_v2` against a Vec and
//!   then re-encode replacing lone surrogates.
//!
//! - **ReadableStream + keepalive**: per Fetch §3.2 step 11.10, throws
//!   TypeError if the body is a stream and `keepalive: true`.
//!
//! - **Disturbed/locked stream**: per Fetch §3.2 step 11.11 and the
//!   Body model, a disturbed or locked stream throws TypeError on
//!   extract.

use std::rc::Rc;

use crate::state::OpError;

use super::body::{BodyImpl, BodySource};

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Result of extract_body: the body state plus the default Content-Type
/// (or None if the body is null OR was a ReadableStream / BufferSource
/// without a self-derived MIME).
pub struct Extracted {
    pub body: BodyImpl,
    pub content_type: Option<String>,
}

/// Per Fetch §3.2 "extract a body" (steps 1–11). Returns the extracted
/// `BodyImpl` and the default Content-Type if any.
///
/// `keepalive` matters only for ReadableStream bodies: per step 11.10,
/// a stream body with `keepalive: true` throws TypeError.
pub fn extract_body(
    scope: &mut v8::PinScope,
    value: v8::Local<v8::Value>,
    keepalive: bool,
) -> Result<Extracted, OpError> {
    // Step 2: null body.
    if value.is_null_or_undefined() {
        return Ok(Extracted {
            body: BodyImpl::null(),
            content_type: None,
        });
    }

    // C-10 dispatch order. We check explicit type predicates so a user
    // body doesn't accidentally fall into the string arm.

    // ReadableStream branch (step 11.11).
    //
    // We check via `instanceof globalThis.ReadableStream` here rather
    // than the streams crate's `is_readable_stream` helper. The helper
    // tests for "any V8 wrapper with an External in internal field 0",
    // which `#[v8_class]`-generated classes (FormData, Headers, etc.)
    // also match — leading to a body extraction that wrongly treats
    // FormData as a stream. The instanceof check is the spec-faithful
    // discriminator.
    if let Ok(obj) = v8::Local::<v8::Object>::try_from(value) {
        if is_readable_stream_instance(scope, obj) {
            return extract_from_stream(scope, obj, keepalive);
        }
    }

    // ArrayBuffer / ArrayBufferView (BufferSource).
    if value.is_array_buffer() || value.is_array_buffer_view() {
        return extract_from_buffer_source(scope, value);
    }

    // Blob — native class. Per Fetch §3.2 step 11.3:
    //   - body's stream is a stream that emits the Blob's bytes,
    //   - Content-Type defaults to the Blob's `type`.
    if let Ok(obj) = v8::Local::<v8::Object>::try_from(value) {
        if crate::blob_native::blob::is_blob_instance_public(scope, obj) {
            if let Some((bytes, type_)) =
                crate::blob_native::blob::read_blob_bytes_and_type(scope, obj)
            {
                let length = Some(bytes.len() as u64);
                let content_type = if type_.is_empty() { None } else { Some(type_.clone()) };
                let bytes_rc = Rc::new(bytes);
                // Defer stream materialization (Fix B): consumers can drain
                // the source bytes directly without ever constructing a
                // ReadableStream when they know they'll fully consume.
                return Ok(Extracted {
                    body: BodyImpl {
                        stream: std::cell::RefCell::new(None),
                        source: Some(BodySource::Blob(bytes_rc, Some(type_))),
                        length,
                    },
                    content_type,
                });
            }
        }
    }

    // FormData — duck-type via `Symbol.toStringTag === "FormData"` and
    // the `entries()` iterable. The native FormData is `formData.entries()`
    // → iterable of `[name, value]` pairs.
    if let Some(fd_bytes) = try_extract_form_data(scope, value)? {
        let (bytes, boundary, mime) = fd_bytes;
        let length = Some(bytes.len() as u64);
        let bytes_rc = Rc::new(bytes);
        // FIX B: defer stream construction — body getter materializes
        // it lazily on first access. Saves a JS ReadableStream alloc
        // when the body is consumed via text/json/arrayBuffer/bytes.
        return Ok(Extracted {
            body: BodyImpl {
                stream: std::cell::RefCell::new(None),
                source: Some(BodySource::FormData(bytes_rc, boundary)),
                length,
            },
            content_type: Some(mime),
        });
    }

    // URLSearchParams — duck-type via `Symbol.toStringTag === "URLSearchParams"`
    // and the `toString()` returning the urlencoded form.
    if let Some(usp_bytes) = try_extract_url_search_params(scope, value)? {
        let length = Some(usp_bytes.len() as u64);
        let bytes_rc = Rc::new(usp_bytes);
        return Ok(Extracted {
            body: BodyImpl {
                stream: std::cell::RefCell::new(None),
                source: Some(BodySource::UrlSearchParams(bytes_rc.clone())),
                length,
            },
            content_type: Some("application/x-www-form-urlencoded;charset=UTF-8".into()),
        });
    }

    // Fallback: scalar value string. Per spec USVString conversion.
    let bytes = string_to_usv_bytes(scope, value)?;
    let length = Some(bytes.len() as u64);
    let bytes_rc = Rc::new(bytes);
    Ok(Extracted {
        body: BodyImpl {
            stream: std::cell::RefCell::new(None),
            source: Some(BodySource::Bytes(bytes_rc)),
            length,
        },
        content_type: Some("text/plain;charset=UTF-8".into()),
    })
}

// ---------------------------------------------------------------------------
// Path: ReadableStream
// ---------------------------------------------------------------------------

/// True iff `obj instanceof globalThis.ReadableStream`. Used for body
/// extraction's stream-arm dispatch — must NOT collide with FormData /
/// Headers / Request / Response which also use V8 internal field 0
/// for native state. Falls back to false if the global is missing.
fn is_readable_stream_instance(scope: &mut v8::PinScope, obj: v8::Local<v8::Object>) -> bool {
    let global = scope.get_current_context().global(scope);
    let key = match v8::String::new(scope, "ReadableStream") {
        Some(k) => k,
        None => return false,
    };
    let Some(class_v) = global.get(scope, key.into()) else {
        return false;
    };
    let Ok(class_obj) = v8::Local::<v8::Object>::try_from(class_v) else {
        return false;
    };
    obj.instance_of(scope, class_obj).unwrap_or(false)
}

fn extract_from_stream(
    scope: &mut v8::PinScope,
    stream_obj: v8::Local<v8::Object>,
    keepalive: bool,
) -> Result<Extracted, OpError> {
    // Step 11.10: keepalive + stream → TypeError.
    if keepalive {
        return Err(OpError::type_error(
            "keepalive Request with a ReadableStream body is not allowed",
        ));
    }

    // Disturbed or locked → TypeError. The stream's `locked` getter is
    // on the prototype; check via JS to avoid reaching into RSState.
    if stream_is_locked_or_disturbed(scope, stream_obj)? {
        return Err(OpError::type_error(
            "ReadableStream body is locked or disturbed",
        ));
    }

    let stream_global = v8::Global::new(scope, stream_obj);
    Ok(Extracted {
        body: BodyImpl {
            stream: std::cell::RefCell::new(Some(stream_global)),
            source: Some(BodySource::Stream),
            length: None,
        },
        content_type: None,
    })
}

/// Probe `stream.locked === true` OR the streams crate's RSState
/// `disturbed` slot is set. Per Fetch §3.2 step 11.11.2 (extract a
/// body from a ReadableStream): "If body's stream is disturbed or
/// locked, then throw a TypeError." The locked getter is observable
/// from JS; the disturbed flag is private (per WHATWG Streams §4.1).
/// We inspect both to honour the spec.
fn stream_is_locked_or_disturbed(
    scope: &mut v8::PinScope,
    stream_obj: v8::Local<v8::Object>,
) -> Result<bool, OpError> {
    if let Some(disturbed) =
        crate::streams::readable::with_rs_state(scope, stream_obj, |s| s.disturbed.get())
    {
        if disturbed {
            return Ok(true);
        }
    }
    let key = v8::String::new(scope, "locked").unwrap();
    let v = stream_obj
        .get(scope, key.into())
        .ok_or_else(|| OpError::type_error("ReadableStream.locked access threw"))?;
    Ok(v.boolean_value(scope))
}

// ---------------------------------------------------------------------------
// Path: ArrayBuffer / ArrayBufferView
// ---------------------------------------------------------------------------

fn extract_from_buffer_source(
    scope: &mut v8::PinScope,
    value: v8::Local<v8::Value>,
) -> Result<Extracted, OpError> {
    let bytes = read_buffer_source_bytes(scope, value)?;
    let length = Some(bytes.len() as u64);
    let bytes_rc = Rc::new(bytes);
    Ok(Extracted {
        body: BodyImpl {
            stream: std::cell::RefCell::new(None),
            source: Some(BodySource::Bytes(bytes_rc)),
            length,
        },
        // BufferSource has no MIME default (Fetch §3.2 step 11.5).
        content_type: None,
    })
}

/// Copy bytes out of an ArrayBuffer or ArrayBufferView. SharedArrayBuffer
/// is rejected (Fetch §3.2 step 11.4 says BufferSource is `[AllowShared]`-
/// excluded for body extract; v2 fix lines up with WebIDL §3.2.21).
fn read_buffer_source_bytes(
    scope: &mut v8::PinScope,
    value: v8::Local<v8::Value>,
) -> Result<Vec<u8>, OpError> {
    if let Ok(view) = v8::Local::<v8::ArrayBufferView>::try_from(value) {
        let buf = view
            .buffer(scope)
            .ok_or_else(|| OpError::type_error("ArrayBufferView has no buffer"))?;
        if buf.is_shared_array_buffer() {
            return Err(OpError::type_error(
                "SharedArrayBuffer-backed body is not allowed",
            ));
        }
        let offset = view.byte_offset();
        let len = view.byte_length();
        let store = buf.get_backing_store();
        let mut out = vec![0u8; len];
        if len > 0 {
            unsafe {
                let src = store.data().expect("backing store data").as_ptr() as *const u8;
                std::ptr::copy_nonoverlapping(src.add(offset), out.as_mut_ptr(), len);
            }
        }
        Ok(out)
    } else if let Ok(buf) = v8::Local::<v8::ArrayBuffer>::try_from(value) {
        if buf.is_shared_array_buffer() {
            return Err(OpError::type_error(
                "SharedArrayBuffer-backed body is not allowed",
            ));
        }
        let len = buf.byte_length();
        let store = buf.get_backing_store();
        let mut out = vec![0u8; len];
        if len > 0 {
            unsafe {
                let src = store.data().expect("backing store data").as_ptr() as *const u8;
                std::ptr::copy_nonoverlapping(src, out.as_mut_ptr(), len);
            }
        }
        Ok(out)
    } else {
        Err(OpError::type_error("Not a BufferSource"))
    }
}

// ---------------------------------------------------------------------------
// Path: USVString (scalar string body)
// ---------------------------------------------------------------------------

/// Per WebIDL `USVString` and Fetch §3.2 step 11.7: convert to a string,
/// then UTF-8-encode replacing lone surrogates with U+FFFD.
fn string_to_usv_bytes(
    scope: &mut v8::PinScope,
    value: v8::Local<v8::Value>,
) -> Result<Vec<u8>, OpError> {
    let s = value
        .to_string(scope)
        .ok_or_else(|| OpError::type_error("Cannot convert body to string"))?;

    // Read out as UTF-16 code units, walk + emit UTF-8 bytes with
    // surrogate replacement.
    let len = s.length();
    let mut units: Vec<u16> = vec![0u16; len];
    s.write_v2(scope, 0, &mut units, v8::WriteFlags::empty());

    let mut out = Vec::with_capacity(len);
    let mut i = 0;
    while i < units.len() {
        let cu = units[i];
        if (0xD800..=0xDBFF).contains(&cu) {
            // High surrogate — needs a low surrogate next.
            if i + 1 < units.len() {
                let low = units[i + 1];
                if (0xDC00..=0xDFFF).contains(&low) {
                    let high = cu as u32;
                    let low = low as u32;
                    let cp = 0x10000 + (((high - 0xD800) << 10) | (low - 0xDC00));
                    encode_utf8(cp, &mut out);
                    i += 2;
                    continue;
                }
            }
            // Lone high surrogate.
            encode_utf8(0xFFFD, &mut out);
            i += 1;
        } else if (0xDC00..=0xDFFF).contains(&cu) {
            // Lone low surrogate.
            encode_utf8(0xFFFD, &mut out);
            i += 1;
        } else {
            encode_utf8(cu as u32, &mut out);
            i += 1;
        }
    }
    Ok(out)
}

fn encode_utf8(cp: u32, out: &mut Vec<u8>) {
    if cp <= 0x7F {
        out.push(cp as u8);
    } else if cp <= 0x7FF {
        out.push(0xC0 | ((cp >> 6) as u8));
        out.push(0x80 | ((cp & 0x3F) as u8));
    } else if cp <= 0xFFFF {
        out.push(0xE0 | ((cp >> 12) as u8));
        out.push(0x80 | (((cp >> 6) & 0x3F) as u8));
        out.push(0x80 | ((cp & 0x3F) as u8));
    } else {
        out.push(0xF0 | ((cp >> 18) as u8));
        out.push(0x80 | (((cp >> 12) & 0x3F) as u8));
        out.push(0x80 | (((cp >> 6) & 0x3F) as u8));
        out.push(0x80 | ((cp & 0x3F) as u8));
    }
}

// ---------------------------------------------------------------------------
// Path: FormData
// ---------------------------------------------------------------------------

/// One FormData entry as the multipart serializer sees it: the name
/// (USVString bytes) plus the value, which is either bytes (USVString
/// stringified UTF-8 OR file bytes), plus optional `(filename,
/// content_type)` for File-typed entries.
struct ExtractedEntry {
    name: Vec<u8>,
    value: ExtractedValue,
}

enum ExtractedValue {
    Text(Vec<u8>),
    File {
        filename: String,
        content_type: String,
        bytes: Vec<u8>,
    },
}

/// Try to extract a FormData. Returns `Ok(Some((bytes, boundary, mime)))`
/// if the value duck-types as FormData; `Ok(None)` otherwise; `Err` on
/// real failure.
fn try_extract_form_data(
    scope: &mut v8::PinScope,
    value: v8::Local<v8::Value>,
) -> Result<Option<(Vec<u8>, String, String)>, OpError> {
    let Ok(obj) = v8::Local::<v8::Object>::try_from(value) else {
        return Ok(None);
    };
    if !is_form_data(scope, obj) {
        return Ok(None);
    }

    let entries = read_form_data_entries(scope, obj)?;
    let boundary = generate_multipart_boundary();
    let bytes = serialize_form_data_multipart(&entries, &boundary);
    let mime = format!("multipart/form-data; boundary={boundary}");
    Ok(Some((bytes, boundary, mime)))
}

fn is_form_data(scope: &mut v8::PinScope, obj: v8::Local<v8::Object>) -> bool {
    let tag = v8::Symbol::get_to_string_tag(scope);
    if let Some(v) = obj.get(scope, tag.into()) {
        if v.is_string() {
            return v.to_rust_string_lossy(scope) == "FormData";
        }
    }
    false
}

fn read_form_data_entries(
    scope: &mut v8::PinScope,
    fd_obj: v8::Local<v8::Object>,
) -> Result<Vec<ExtractedEntry>, OpError> {
    // Call fd.entries() to get the iterator.
    let key = v8::String::new(scope, "entries").unwrap();
    let entries_fn_v = fd_obj
        .get(scope, key.into())
        .ok_or_else(|| OpError::type_error("FormData.entries access threw"))?;
    let entries_fn: v8::Local<v8::Function> = entries_fn_v
        .try_into()
        .map_err(|_| OpError::type_error("FormData.entries is not callable"))?;
    let iter_v = entries_fn
        .call(scope, fd_obj.into(), &[])
        .ok_or_else(|| OpError::type_error("FormData.entries() threw"))?;
    let iter: v8::Local<v8::Object> = iter_v
        .try_into()
        .map_err(|_| OpError::type_error("FormData.entries did not return an object"))?;
    let next_key = v8::String::new(scope, "next").unwrap();
    let next_fn_v = iter
        .get(scope, next_key.into())
        .ok_or_else(|| OpError::type_error("iter.next access threw"))?;
    let next_fn: v8::Local<v8::Function> = next_fn_v
        .try_into()
        .map_err(|_| OpError::type_error("iter.next is not callable"))?;
    let done_key = v8::String::new(scope, "done").unwrap();
    let value_key = v8::String::new(scope, "value").unwrap();

    let mut out: Vec<ExtractedEntry> = Vec::new();
    loop {
        let step_v = next_fn
            .call(scope, iter.into(), &[])
            .ok_or_else(|| OpError::type_error("iter.next() threw"))?;
        let step: v8::Local<v8::Object> = step_v
            .try_into()
            .map_err(|_| OpError::type_error("iter.next() did not return an object"))?;
        let done_v = step
            .get(scope, done_key.into())
            .ok_or_else(|| OpError::type_error("step.done access threw"))?;
        if done_v.boolean_value(scope) {
            break;
        }
        let pair_v = step
            .get(scope, value_key.into())
            .ok_or_else(|| OpError::type_error("step.value access threw"))?;
        let pair: v8::Local<v8::Object> = pair_v
            .try_into()
            .map_err(|_| OpError::type_error("step.value is not an object"))?;
        let name_v = pair
            .get_index(scope, 0)
            .ok_or_else(|| OpError::type_error("FormData entry [0] access threw"))?;
        let val_v = pair
            .get_index(scope, 1)
            .ok_or_else(|| OpError::type_error("FormData entry [1] access threw"))?;
        let name_b = string_to_usv_bytes(scope, name_v)?;

        // Distinguish Blob/File from string. For File entries we read
        // the raw bytes + filename + type. For non-Blob entries we
        // USVString-coerce.
        let value: ExtractedValue = if let Ok(obj) = v8::Local::<v8::Object>::try_from(val_v) {
            if crate::blob_native::blob::is_blob_instance_public(scope, obj) {
                let (bytes, content_type) = crate::blob_native::blob::read_blob_bytes_and_type(scope, obj)
                    .ok_or_else(|| OpError::type_error("FormData entry Blob has no bytes"))?;
                // For File: read the .name property; for plain Blob,
                // default filename is "blob".
                let filename = if crate::blob_native::blob::is_file_instance_public(scope, obj) {
                    let name_key = v8::String::new(scope, "name").unwrap();
                    obj.get(scope, name_key.into())
                        .map(|v| v.to_rust_string_lossy(scope))
                        .unwrap_or_else(|| "blob".to_string())
                } else {
                    "blob".to_string()
                };
                ExtractedValue::File {
                    filename,
                    content_type,
                    bytes,
                }
            } else {
                ExtractedValue::Text(string_to_usv_bytes(scope, val_v)?)
            }
        } else {
            ExtractedValue::Text(string_to_usv_bytes(scope, val_v)?)
        };

        out.push(ExtractedEntry { name: name_b, value });
    }
    Ok(out)
}

fn generate_multipart_boundary() -> String {
    // 16 random hex chars prefixed with "----zsboundary-" (matches the
    // common format other runtimes use). Source the bytes from
    // SystemRandom-style nanos; this is a multipart boundary, not a
    // crypto key — uniqueness is the only requirement.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    let mix = ((pid as u64) << 32) | (nanos as u64);
    format!("----zsboundary-{mix:016x}")
}

fn serialize_form_data_multipart(entries: &[ExtractedEntry], boundary: &str) -> Vec<u8> {
    let mut out = Vec::new();
    for entry in entries {
        out.extend_from_slice(b"--");
        out.extend_from_slice(boundary.as_bytes());
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(b"Content-Disposition: form-data; name=\"");
        // RFC 7578 §4.2: percent-encode the name's "%", CR, LF, and
        // double-quote. Keep other UTF-8 bytes.
        for &b in &entry.name {
            push_disp_escaped(&mut out, b);
        }
        out.extend_from_slice(b"\"");
        match &entry.value {
            ExtractedValue::Text(value) => {
                out.extend_from_slice(b"\r\n\r\n");
                out.extend_from_slice(value);
            }
            ExtractedValue::File { filename, content_type, bytes } => {
                out.extend_from_slice(b"; filename=\"");
                for &b in filename.as_bytes() {
                    push_disp_escaped(&mut out, b);
                }
                out.extend_from_slice(b"\"\r\n");
                // Per RFC 7578 §4.4: include Content-Type if known.
                // Default to application/octet-stream when missing —
                // matches every browser implementation.
                let ct = if content_type.is_empty() {
                    "application/octet-stream"
                } else {
                    content_type.as_str()
                };
                out.extend_from_slice(b"Content-Type: ");
                out.extend_from_slice(ct.as_bytes());
                out.extend_from_slice(b"\r\n\r\n");
                out.extend_from_slice(bytes);
            }
        }
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"--");
    out.extend_from_slice(boundary.as_bytes());
    out.extend_from_slice(b"--\r\n");
    out
}

#[inline]
fn push_disp_escaped(out: &mut Vec<u8>, b: u8) {
    match b {
        b'"' => out.extend_from_slice(b"%22"),
        b'\r' => out.extend_from_slice(b"%0D"),
        b'\n' => out.extend_from_slice(b"%0A"),
        _ => out.push(b),
    }
}

// ---------------------------------------------------------------------------
// Path: URLSearchParams
// ---------------------------------------------------------------------------

fn try_extract_url_search_params(
    scope: &mut v8::PinScope,
    value: v8::Local<v8::Value>,
) -> Result<Option<Vec<u8>>, OpError> {
    let Ok(obj) = v8::Local::<v8::Object>::try_from(value) else {
        return Ok(None);
    };
    if !is_url_search_params(scope, obj) {
        return Ok(None);
    }
    // The URLSearchParams.toString() returns the urlencoded form per
    // URL spec §6.2 — always ASCII. UTF-8-encode the JS string.
    let s = obj
        .to_string(scope)
        .ok_or_else(|| OpError::type_error("URLSearchParams.toString threw"))?;
    let bytes = s.to_rust_string_lossy(scope).into_bytes();
    Ok(Some(bytes))
}

fn is_url_search_params(scope: &mut v8::PinScope, obj: v8::Local<v8::Object>) -> bool {
    let tag = v8::Symbol::get_to_string_tag(scope);
    if let Some(v) = obj.get(scope, tag.into()) {
        if v.is_string() {
            return v.to_rust_string_lossy(scope) == "URLSearchParams";
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Build a ReadableStream from in-memory bytes
// ---------------------------------------------------------------------------

/// Build a JS-visible ReadableStream that emits the provided bytes as
/// a single Uint8Array chunk and then closes. We construct it via the
/// JS-visible `new ReadableStream(...)` so the resulting object behaves
/// exactly like a user-constructed stream (instanceof ReadableStream,
/// proper prototype chain, body consumers can disturb/lock).
///
/// The bytes Rc is cheaply cloned so the stream's start callback can
/// own a fresh reference; the parent BodyImpl still holds the original
/// Rc for clone()/redirect.
pub fn build_byte_stream(
    scope: &mut v8::PinScope,
    bytes: Rc<Vec<u8>>,
) -> v8::Global<v8::Object> {
    let stream_obj = build_byte_stream_via_constructor(scope, &bytes);
    v8::Global::new(scope, stream_obj)
}

/// Construct a fresh ReadableStream wrapping the given bytes. Uses the
/// JS-visible constructor.
fn build_byte_stream_via_constructor<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    bytes: &Rc<Vec<u8>>,
) -> v8::Local<'s, v8::Object> {
    let global = scope.get_current_context().global(scope);
    let key = v8::String::new(scope, "ReadableStream").unwrap();
    let class_v = global
        .get(scope, key.into())
        .expect("globalThis.ReadableStream missing");
    let class_fn: v8::Local<v8::Function> = class_v
        .try_into()
        .expect("globalThis.ReadableStream is not a function");

    // Build the underlyingSource object: { start(c) { c.enqueue(bytes); c.close(); } }
    // The bytes go in via an External captured by the start callback.
    let underlying = v8::Object::new(scope);

    // Box the Rc<Vec<u8>> so the External points at a stable address.
    // The closure attached to the FunctionTemplate frees the Box via a
    // weak finalizer on the Function wrapper.
    let boxed: Box<Rc<Vec<u8>>> = Box::new(bytes.clone());
    let raw = Box::into_raw(boxed);
    let raw_addr = raw as usize;
    let ext = v8::External::new(scope, raw as *mut std::ffi::c_void);

    let tmpl = v8::FunctionTemplate::builder(start_callback)
        .data(ext.into())
        .build(scope);
    let start_fn = tmpl.get_function(scope).unwrap();

    // Free the Box when the start function is GC'd. (It will be GC'd
    // shortly after the stream is fully drained — V8 keeps the object
    // tree alive until then.)
    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        start_fn,
        Box::new(move || unsafe {
            drop(Box::from_raw(raw_addr as *mut Rc<Vec<u8>>));
        }),
    );
    std::mem::forget(weak);

    let start_key = v8::String::new(scope, "start").unwrap();
    underlying.set(scope, start_key.into(), start_fn.into());

    // new ReadableStream(underlying)
    let args = [underlying.into()];
    let stream = class_fn
        .new_instance(scope, &args)
        .expect("new ReadableStream failed");
    stream
}

fn start_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    // controller is args[0]. Read bytes from External data. enqueue +
    // close.
    let data = args.data();
    let Ok(ext) = v8::Local::<v8::External>::try_from(data) else {
        return;
    };
    let raw = ext.value() as *const Rc<Vec<u8>>;
    if raw.is_null() {
        return;
    }
    // SAFETY: the External points at a Box<Rc<Vec<u8>>> created in
    // build_byte_stream_via_constructor. The finalizer reclaims it; we
    // borrow read-only here.
    let bytes_rc = unsafe { &*raw };

    let controller_v = args.get(0);
    let Ok(controller) = v8::Local::<v8::Object>::try_from(controller_v) else {
        return;
    };

    if !bytes_rc.is_empty() {
        // Build a Uint8Array wrapping a fresh ArrayBuffer with the bytes.
        let buf = v8::ArrayBuffer::new_backing_store_from_vec((**bytes_rc).clone()).make_shared();
        let ab = v8::ArrayBuffer::with_backing_store(scope, &buf);
        let len = ab.byte_length();
        let view = v8::Uint8Array::new(scope, ab, 0, len).unwrap();

        let enq_key = v8::String::new(scope, "enqueue").unwrap();
        let enq_v = controller.get(scope, enq_key.into()).unwrap();
        let enq_fn: v8::Local<v8::Function> = enq_v.try_into().unwrap();
        let enq_args = [view.into()];
        let _ = enq_fn.call(scope, controller.into(), &enq_args);
    }

    let close_key = v8::String::new(scope, "close").unwrap();
    let close_v = controller.get(scope, close_key.into()).unwrap();
    let close_fn: v8::Local<v8::Function> = close_v.try_into().unwrap();
    let _ = close_fn.call(scope, controller.into(), &[]);
}
