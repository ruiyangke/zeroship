//! Native `TextEncoder` and `TextDecoder` per the WHATWG Encoding spec
//! (https://encoding.spec.whatwg.org). Replaces the buggy hand-rolled
//! JS polyfills that lived in `embed/fetch.js`.
//!
//! Implementation notes:
//!
//! - **UTF-8 decoder is `encoding_rs`'s** — Henri Sivonen's reference
//!   implementation of the WHATWG decoder algorithm, also used by
//!   Firefox. Matches Web Platform Tests for U+FFFD substitution
//!   counts on adversarial / malformed inputs (Rust's
//!   `std::str::from_utf8` does NOT match the WHATWG state-machine
//!   grouping rules — that was a real divergence in the previous
//!   draft of this file).
//!
//! - **BOM handling** uses `encoding_rs`'s built-in BOM-stripping
//!   variant (`new_decoder` vs `new_decoder_without_bom_handling`),
//!   so BOM detection is correct across streamed chunk boundaries
//!   (BOM split into 1+2 or 2+1 bytes works) without us tracking
//!   `bom_consumed` state by hand.
//!
//! - **Argument validation** follows WebIDL coercion strictly:
//!   - `new TextDecoder(null)` → throws `RangeError` (`null` coerces
//!     to the string `"null"`, which isn't a UTF-8 alias).
//!   - `new TextDecoder("utf-8", "fatal")` → throws `TypeError`
//!     (non-object non-null non-undefined as dictionary).
//!   - `decode(42)` → throws `TypeError` (not a `BufferSource`).
//!   - `decode(bytes, "stream")` → throws `TypeError`.
//!
//! - **Symbol.toStringTag** is set so
//!   `Object.prototype.toString.call(new TextDecoder()) === "[object
//!   TextDecoder]"` (libraries like webidl-conversions check this).
//!
//! - Scope is intentionally UTF-8 only at the moment. The full
//!   WHATWG encoding set (utf-16le/be, latin1, the legacy single-
//!   byte encodings) is a one-line change to use
//!   `encoding_rs::Encoding::for_label` instead of the hardcoded
//!   UTF-8 path. Deferred until a real consumer needs it.

use encoding_rs::{DecoderResult, UTF_8};
// `v8_class` is the only attribute consumed at the impl-block level.
// The marker attributes (`v8_method`/`v8_getter`/`v8_constructor`) are
// no-op procedural macros that the user puts on individual methods —
// importing them lets them parse, but rustc sees no usage in this
// file's symbol table because the attributes get stripped during the
// outer expansion. The `v8_class` proc macro removes them before
// quoting the impl block back out.
use zeroship_runtime_macros::v8_class;
#[allow(unused_imports)]
use zeroship_runtime_macros::{v8_constructor, v8_getter, v8_method};

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
    /// Returns a `Vec<u8>`, which the macro marshals as Uint8Array
    /// (matching the spec — see macro's `gen_vec_u8_set`). V8's
    /// WTF-16 → UTF-8 conversion (via `to_rust_string_lossy`) already
    /// substitutes U+FFFD for unpaired surrogates per spec, so we
    /// don't need extra handling here.
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
    /// Lazily-instantiated `encoding_rs::Decoder`. Recreated after
    /// every non-streaming flush because encoding_rs panics with
    /// `"Must not use a decoder that has finished"` if you pass
    /// `last: true` and then try to reuse it. WHATWG spec allows
    /// multiple `decode(bytes)` calls on the same TextDecoder, so
    /// we drop-and-recreate to bridge the gap.
    decoder: Option<encoding_rs::Decoder>,
    /// Set true after the first decode call that consumed any
    /// bytes. Subsequent calls won't request BOM-removal — matches
    /// the WHATWG "BOM seen flag" which persists across calls for
    /// the lifetime of the TextDecoder.
    bom_seen: bool,
    fatal_flag: bool,
    ignore_bom_flag: bool,
}

impl Default for TextDecoder {
    fn default() -> Self {
        TextDecoder {
            decoder: None,
            bom_seen: false,
            fatal_flag: false,
            ignore_bom_flag: false,
        }
    }
}

#[v8_class]
impl TextDecoder {
    /// `new TextDecoder(label?: DOMString, options?: TextDecoderOptions)`
    ///
    /// `label` defaults to `"utf-8"` per spec. We currently only
    /// support UTF-8 — other labels throw `RangeError`. `options`
    /// must be `undefined`, `null`, or a plain object; anything else
    /// throws `TypeError` per WebIDL §3.2.20.
    #[v8_constructor]
    fn new(
        scope: &mut v8::PinScope,
        label: v8::Local<v8::Value>,
        options: v8::Local<v8::Value>,
    ) -> Result<Self, OpError> {
        // Per spec: label defaults to "utf-8" only when undefined.
        // null coerces to the string "null" — which is not a valid
        // encoding alias and must throw RangeError.
        let label_str = if label.is_undefined() {
            "utf-8".to_string()
        } else {
            label.to_rust_string_lossy(scope)
        };

        if !is_utf8_label(&label_str) {
            return Err(OpError::range_error(
                "TextDecoder: unsupported encoding label; only utf-8 is implemented",
            ));
        }

        let (fatal_flag, ignore_bom_flag) = read_decoder_options(scope, options)?;

        Ok(TextDecoder {
            decoder: None,
            bom_seen: false,
            fatal_flag,
            ignore_bom_flag,
        })
    }

    /// `decode(input?: BufferSource, options?: { stream?: bool }) -> USVString`
    ///
    /// `input` must be `undefined`, `null`, or a `BufferSource`
    /// (ArrayBuffer or ArrayBufferView). Anything else throws
    /// `TypeError`. `options` follows the same rules as the
    /// constructor's options arg.
    #[v8_method]
    fn decode(
        &mut self,
        scope: &mut v8::PinScope,
        input: v8::Local<v8::Value>,
        options: v8::Local<v8::Value>,
    ) -> Result<String, OpError> {
        let bytes = read_buffer_source(input)?;
        let stream = read_stream_option(scope, options)?;

        // Lazy-init or re-init the decoder. After the previous
        // non-streaming call we set `decoder = None`, so this branch
        // also handles "post-flush, fresh state" reconstruction.
        if self.decoder.is_none() {
            // BOM-removal kicks in only on the very first chunk that
            // could contain a BOM. After we've seen any input bytes,
            // future decoders skip BOM detection.
            let dec = if self.ignore_bom_flag || self.bom_seen {
                UTF_8.new_decoder_without_bom_handling()
            } else {
                UTF_8.new_decoder_with_bom_removal()
            };
            self.decoder = Some(dec);
        }

        let decoder = self.decoder.as_mut().expect("decoder just installed");

        // Worst-case expansion: each input byte plus any pending
        // partial-sequence state may produce up to one U+FFFD
        // (3 bytes UTF-8). encoding_rs's `max_utf8_buffer_length`
        // gives us the safe upper bound — we pre-allocate so the
        // CoderResult::OutputFull branch never fires.
        let max_out = decoder
            .max_utf8_buffer_length(bytes.len())
            .unwrap_or_else(|| bytes.len().saturating_mul(3) + 8);

        let mut out = String::with_capacity(max_out);

        let result_is_err = if self.fatal_flag {
            let (result, _read) =
                decoder.decode_to_string_without_replacement(&bytes, &mut out, !stream);
            match result {
                DecoderResult::InputEmpty => false,
                DecoderResult::OutputFull => {
                    return Err(OpError::error(
                        "TextDecoder: internal error — output buffer underflow",
                    ));
                }
                DecoderResult::Malformed(_, _) => true,
            }
        } else {
            let (result, _read, _replaced) =
                decoder.decode_to_string(&bytes, &mut out, !stream);
            match result {
                encoding_rs::CoderResult::InputEmpty => false,
                encoding_rs::CoderResult::OutputFull => {
                    return Err(OpError::error(
                        "TextDecoder: internal error — output buffer underflow",
                    ));
                }
            }
        };

        // Any input that reached the decoder's internal state means
        // BOM detection has had its chance. encoding_rs commits the
        // BOM/no-BOM decision after the first byte that disambiguates
        // (i.e., as soon as the second-byte mismatch with the BOM
        // sequence is observed) — but for the spec, we conservatively
        // mark BOM as seen once any non-empty chunk has been
        // processed, even if it doesn't contain bytes that confirm
        // the absence of a BOM. This matches what Chrome and Firefox
        // do on partial chunks.
        if !bytes.is_empty() {
            self.bom_seen = true;
        }

        // Non-streaming call → discard the decoder so the next
        // `decode()` call starts fresh, per WHATWG semantics.
        // encoding_rs would otherwise panic on reuse-after-finalize.
        if !stream {
            self.decoder = None;
        }

        if result_is_err {
            Err(OpError::type_error(
                "TextDecoder: invalid byte sequence (fatal mode)",
            ))
        } else {
            Ok(out)
        }
    }

    #[v8_getter]
    fn encoding(&self) -> String {
        // We're hardcoded to UTF-8 currently. Once we accept other
        // labels via `Encoding::for_label`, this should read from the
        // (still-allocated) decoder, or from a stored Encoding ref
        // on the struct.
        "utf-8".into()
    }

    #[v8_getter]
    fn fatal(&self) -> bool {
        self.fatal_flag
    }

    /// Spec name is `ignoreBOM`. Suppress the lint locally — when we
    /// add `#[v8_name = "..."]` to the macro this can move back to
    /// snake_case and read more naturally.
    #[v8_getter]
    #[allow(non_snake_case)]
    fn ignoreBOM(&self) -> bool {
        self.ignore_bom_flag
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// True iff the given label normalizes to UTF-8 per WHATWG §4.2
/// "get an encoding." Strips ASCII whitespace (HT, LF, FF, CR, SP)
/// only — NOT Unicode whitespace, which `str::trim` would do —
/// and uses ASCII-case-insensitive comparison.
fn is_utf8_label(label: &str) -> bool {
    let trimmed = trim_ascii_whitespace(label);
    matches!(
        trimmed.to_ascii_lowercase().as_str(),
        "utf-8"
            | "utf8"
            | "unicode-1-1-utf-8"
            | "unicode11utf8"
            | "unicode20utf8"
            | "x-unicode20utf8"
    )
}

/// ASCII-whitespace trim per WHATWG infra spec: strip leading/trailing
/// HT (U+0009), LF (U+000A), FF (U+000C), CR (U+000D), SPACE (U+0020).
/// Unlike `str::trim`, does NOT strip non-ASCII whitespace like
/// U+00A0 NBSP — those characters are part of the label and should
/// trigger a label-mismatch.
fn trim_ascii_whitespace(s: &str) -> &str {
    let bytes = s.as_bytes();
    let start = bytes.iter().position(|&b| !is_ascii_ws(b)).unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|&b| !is_ascii_ws(b))
        .map_or(start, |i| i + 1);
    // SAFETY: ASCII whitespace bytes are all < 0x80, so trimming on
    // byte boundaries can't split a multi-byte char.
    &s[start..end]
}

fn is_ascii_ws(b: u8) -> bool {
    matches!(b, 0x09 | 0x0A | 0x0C | 0x0D | 0x20)
}

/// Read `(fatal, ignoreBOM)` from a decoder constructor's options
/// arg. Per WebIDL §3.2.20 dictionary coercion:
///   - `undefined` or `null` → use defaults.
///   - any other non-object → throw `TypeError`.
///   - object → read `fatal` and `ignoreBOM` properties; missing
///     properties → defaults.
fn read_decoder_options(
    scope: &mut v8::PinScope,
    val: v8::Local<v8::Value>,
) -> Result<(bool, bool), OpError> {
    if val.is_undefined() || val.is_null() {
        return Ok((false, false));
    }
    let Ok(obj) = v8::Local::<v8::Object>::try_from(val) else {
        return Err(OpError::type_error(
            "TextDecoder: options must be an object",
        ));
    };
    let fatal = read_bool_prop(scope, obj, "fatal")?;
    let ignore_bom = read_bool_prop(scope, obj, "ignoreBOM")?;
    Ok((fatal, ignore_bom))
}

/// Read `decode()`'s options arg with the same WebIDL rules as
/// above. Returns the `stream` flag.
fn read_stream_option(
    scope: &mut v8::PinScope,
    val: v8::Local<v8::Value>,
) -> Result<bool, OpError> {
    if val.is_undefined() || val.is_null() {
        return Ok(false);
    }
    let Ok(obj) = v8::Local::<v8::Object>::try_from(val) else {
        return Err(OpError::type_error(
            "TextDecoder.decode: options must be an object",
        ));
    };
    read_bool_prop(scope, obj, "stream")
}

fn read_bool_prop(
    scope: &mut v8::PinScope,
    obj: v8::Local<v8::Object>,
    key: &str,
) -> Result<bool, OpError> {
    let key_v8 = v8::String::new(scope, key)
        .ok_or_else(|| OpError::error("TextDecoder: out of memory allocating property key"))?;
    let val = obj
        .get(scope, key_v8.into())
        .ok_or_else(|| OpError::error("TextDecoder: property access threw"))?;
    Ok(val.boolean_value(scope))
}

/// Coerce a JS value to a byte slice per WebIDL `BufferSource`.
///   - `undefined` or no input → empty.
///   - `ArrayBufferView` or `ArrayBuffer` → bytes.
///   - anything else → `TypeError`.
fn read_buffer_source(val: v8::Local<v8::Value>) -> Result<Vec<u8>, OpError> {
    if val.is_undefined() || val.is_null() {
        return Ok(Vec::new());
    }
    if let Ok(view) = v8::Local::<v8::ArrayBufferView>::try_from(val) {
        let mut buf = vec![0u8; view.byte_length()];
        view.copy_contents(&mut buf);
        return Ok(buf);
    }
    if let Ok(ab) = v8::Local::<v8::ArrayBuffer>::try_from(val) {
        let store = ab.get_backing_store();
        let mut buf = vec![0u8; ab.byte_length()];
        for (i, b) in buf.iter_mut().enumerate() {
            *b = store[i].get();
        }
        return Ok(buf);
    }
    Err(OpError::type_error(
        "TextDecoder.decode: input must be ArrayBuffer or ArrayBufferView",
    ))
}
