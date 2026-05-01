//! Native `TextEncoder` and `TextDecoder` per the WHATWG Encoding spec
//! (https://encoding.spec.whatwg.org). Replaces the buggy hand-rolled
//! JS polyfills that lived in `embed/fetch.js`.
//!
//! Why native:
//! - The JS polyfill ignored the `{ stream: true }` option, so a
//!   multi-byte UTF-8 sequence split across two `decode()` calls got
//!   replaced with U+FFFD. AI SDK / SSE parsers hit this on non-ASCII
//!   responses.
//! - The JS polyfill is ~100 lines of hand-rolled UTF-8 codec — slow
//!   on the hot fetch-body path.
//! - WHATWG spec corner cases (BOM stripping, fatal mode, the four
//!   accepted utf-8 labels) are easy to get wrong in JS and trivial
//!   in Rust where `std::str::from_utf8` does the work.
//!
//! Scope: UTF-8 only. Other encodings (utf-16le/be, latin1, the
//! single-byte legacy set) are real WHATWG surface but not used by
//! AI/SSE/JSON workloads. A future expansion can plug in
//! `encoding_rs` for the full set without touching the JS layer.
//!
//! NB: `TextEncoderStream` / `TextDecoderStream` (the stream-API
//! adapters that wrap these) still live in `streams-polyfill.js`.
//! Once the streams layer also goes native we'll port them; the
//! native classes here already support `{ stream: true }` so the
//! adapter layer becomes trivial.

#![allow(non_snake_case)]

use zeroship_runtime_macros::{v8_class, v8_constructor, v8_getter, v8_method};

use crate::state::OpError;

// ---------------------------------------------------------------------------
// TextEncoder
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct TextEncoder;

#[v8_class]
impl TextEncoder {
    #[v8_constructor]
    fn new() -> Self {
        TextEncoder
    }

    /// `encode(input?: USVString) -> Uint8Array`
    ///
    /// Returns a `Vec<u8>` which the macro marshals as ArrayBuffer.
    /// Spec calls for a Uint8Array view; the JS shim that exposes
    /// the class wraps with `new Uint8Array(arrayBuffer)` so callers
    /// see the spec'd shape. The conversion is zero-copy because the
    /// macro's Vec<u8> codegen builds the ArrayBuffer's backing
    /// store directly from the bytes.
    #[v8_method]
    fn encode(&self, input: Option<String>) -> Vec<u8> {
        input.unwrap_or_default().into_bytes()
    }

    #[v8_getter]
    fn encoding(&self) -> String {
        "utf-8".into()
    }
}

// ---------------------------------------------------------------------------
// TextDecoder
// ---------------------------------------------------------------------------

pub struct TextDecoder {
    encoding: String,
    fatal_flag: bool,
    ignore_bom_flag: bool,
    /// Trailing partial-UTF-8-sequence bytes from a previous
    /// `decode(chunk, { stream: true })` call. Concatenated with the
    /// next input before decoding.
    pending: Vec<u8>,
    /// Set true once the first `decode()` call has run, so we only
    /// look for a leading BOM on the very first chunk.
    bom_consumed: bool,
}

impl Default for TextDecoder {
    fn default() -> Self {
        TextDecoder {
            encoding: "utf-8".into(),
            fatal_flag: false,
            ignore_bom_flag: false,
            pending: Vec::new(),
            bom_consumed: false,
        }
    }
}

#[v8_class]
impl TextDecoder {
    /// `new TextDecoder(label?: DOMString, options?: { fatal?: bool, ignoreBOM?: bool })`
    ///
    /// Per spec, the label is normalized via the encoding lookup
    /// table. We accept the four labels that the spec maps to UTF-8
    /// and reject everything else with `RangeError`.
    #[v8_constructor]
    fn new(
        scope: &mut v8::PinScope,
        label: Option<String>,
        options: v8::Local<v8::Value>,
    ) -> Result<Self, OpError> {
        let normalized = match label
            .as_deref()
            .unwrap_or("utf-8")
            .trim()
            .to_lowercase()
            .as_str()
        {
            "utf-8" | "utf8" | "unicode-1-1-utf-8" | "unicode11utf8" => "utf-8".to_string(),
            other => {
                return Err(OpError::range_error(format!(
                    "TextDecoder: unsupported encoding label \"{other}\"; only utf-8 is implemented"
                )));
            }
        };

        let (fatal_flag, ignore_bom_flag) = read_decoder_options(scope, options);

        Ok(TextDecoder {
            encoding: normalized,
            fatal_flag,
            ignore_bom_flag,
            pending: Vec::new(),
            bom_consumed: false,
        })
    }

    /// `decode(input?: BufferSource, options?: { stream?: bool }) -> USVString`
    ///
    /// On the first call, strips a leading BOM (`EF BB BF`) unless
    /// `ignoreBOM` was set in the constructor. With `{ stream: true }`,
    /// trailing partial UTF-8 sequences are buffered for the next
    /// call instead of producing replacement characters.
    #[v8_method]
    fn decode(
        &mut self,
        scope: &mut v8::PinScope,
        input: Option<Vec<u8>>,
        options: v8::Local<v8::Value>,
    ) -> Result<String, OpError> {
        let stream = read_stream_option(scope, options);
        let bytes = input.unwrap_or_default();
        decode_utf8(self, &bytes, stream)
    }

    #[v8_getter]
    fn encoding(&self) -> String {
        self.encoding.clone()
    }

    #[v8_getter]
    fn fatal(&self) -> bool {
        self.fatal_flag
    }

    /// Spec name is `ignoreBOM`. Suppressed lint at the file level
    /// — when we add `#[v8_name = "..."]` to the macro this can move
    /// back to snake_case.
    #[v8_getter]
    fn ignoreBOM(&self) -> bool {
        self.ignore_bom_flag
    }
}

// ---------------------------------------------------------------------------
// Decode logic
// ---------------------------------------------------------------------------

/// Decode `buf` as UTF-8 with streaming-aware partial-sequence
/// handling. Mutates `dec.pending` to hold trailing partial bytes
/// when `stream` is true; flushes them as U+FFFD (or returns an
/// error in fatal mode) when `stream` is false.
fn decode_utf8(dec: &mut TextDecoder, buf: &[u8], stream: bool) -> Result<String, OpError> {
    // Concat the pending tail from the previous call with the new
    // chunk. `mem::take` zeroes the field so we don't double-prepend.
    let mut input = std::mem::take(&mut dec.pending);
    input.extend_from_slice(buf);

    // BOM stripping only on the first decode call where ignoreBOM is
    // not set. Per spec, we look at the byte stream after concatenation,
    // not the buffer prefix only — but in practice the BOM is always
    // at byte 0 of the first chunk if present.
    let start = if !dec.bom_consumed {
        dec.bom_consumed = true;
        if !dec.ignore_bom_flag && input.starts_with(b"\xEF\xBB\xBF") {
            3
        } else {
            0
        }
    } else {
        0
    };

    let mut out = String::with_capacity(input.len() - start);
    let mut idx = start;

    while idx < input.len() {
        match std::str::from_utf8(&input[idx..]) {
            Ok(s) => {
                out.push_str(s);
                break;
            }
            Err(e) => {
                let valid = e.valid_up_to();
                // SAFETY: bytes 0..valid_up_to are valid UTF-8 by
                // construction (that's the contract of the API).
                out.push_str(unsafe { std::str::from_utf8_unchecked(&input[idx..idx + valid]) });
                idx += valid;

                match e.error_len() {
                    None => {
                        // Trailing incomplete sequence. In streaming
                        // mode, save for the next call. In flush mode
                        // (or no chunk follows), substitute or error.
                        if stream {
                            dec.pending = input[idx..].to_vec();
                            return Ok(out);
                        }
                        if dec.fatal_flag {
                            return Err(OpError::type_error(
                                "TextDecoder: incomplete UTF-8 sequence at end of input",
                            ));
                        }
                        out.push('\u{FFFD}');
                        break;
                    }
                    Some(n) => {
                        // Actual invalid sequence in the middle of
                        // input. Skip the offending bytes per spec
                        // (one U+FFFD per error_len group) or fail.
                        if dec.fatal_flag {
                            return Err(OpError::type_error(
                                "TextDecoder: invalid UTF-8 sequence",
                            ));
                        }
                        out.push('\u{FFFD}');
                        idx += n;
                    }
                }
            }
        }
    }

    Ok(out)
}

// ---------------------------------------------------------------------------
// Options-object parsing
// ---------------------------------------------------------------------------

/// Read `(fatal, ignoreBOM)` flags from a decoder constructor's
/// options object. Missing properties default to false; non-object
/// values silently default the same way (matches WebIDL coercion).
fn read_decoder_options(scope: &mut v8::PinScope, val: v8::Local<v8::Value>) -> (bool, bool) {
    let Ok(obj) = v8::Local::<v8::Object>::try_from(val) else {
        return (false, false);
    };
    let fatal = read_bool_prop(scope, obj, "fatal");
    let ignore_bom = read_bool_prop(scope, obj, "ignoreBOM");
    (fatal, ignore_bom)
}

/// Read `decode()`'s `{ stream: true }` flag. Same robustness as
/// above — missing/non-object → false.
fn read_stream_option(scope: &mut v8::PinScope, val: v8::Local<v8::Value>) -> bool {
    let Ok(obj) = v8::Local::<v8::Object>::try_from(val) else {
        return false;
    };
    read_bool_prop(scope, obj, "stream")
}

fn read_bool_prop(scope: &mut v8::PinScope, obj: v8::Local<v8::Object>, key: &str) -> bool {
    let key_v8 = v8::String::new(scope, key).unwrap();
    let Some(val) = obj.get(scope, key_v8.into()) else {
        return false;
    };
    val.boolean_value(scope)
}
