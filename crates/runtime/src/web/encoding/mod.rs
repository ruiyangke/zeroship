//! Native `TextEncoder` and `TextDecoder` per the WHATWG Encoding spec
//! (https://encoding.spec.whatwg.org). Replaces the buggy hand-rolled
//! JS polyfills that lived in `embed/fetch.js`.
//!
//! Implementation notes:
//!
//! - **All WHATWG encodings supported** via `encoding_rs` — Henri
//!   Sivonen's reference implementation, also Firefox's encoding
//!   library. Covers UTF-8, UTF-16LE, UTF-16BE, the eight ISO-8859-*
//!   variants, Windows-125x, the CJK multi-byte encodings (Big5,
//!   GB18030, Shift_JIS, EUC-JP, EUC-KR, ISO-2022-JP), Macintosh,
//!   IBM866, KOI8-{R,U}, and the `replacement` encoding. Matches WPT
//!   for U+FFFD substitution counts and stream-state behavior.
//!
//! - **TextEncoder is UTF-8 only** by spec — the legacy
//!   `new TextEncoder("utf-16")` constructor was removed years ago.
//!   `new TextEncoder()` is the only valid form.
//!
//! - **BOM handling** uses `encoding_rs::Encoding::new_decoder` (BOM
//!   sniffing on) by default; `ignoreBOM: true` switches to
//!   `new_decoder_without_bom_handling`. BOM detection is correct
//!   across streamed chunk boundaries (BOM split 1+2 or 2+1 bytes
//!   works) without manual partial-byte tracking.
//!
//! - **BOM-removal state resets per non-streaming session**, matching
//!   WPT's `textdecoder-byte-order-marks`: `dec.decode(bom)` strips on
//!   every call, not just the first. We achieve this by dropping the
//!   internal `encoding_rs::Decoder` after each non-streaming flush
//!   (also avoiding encoding_rs's "Must not use a decoder that has
//!   finished" panic on reuse).
//!
//! - **Argument validation** follows WebIDL coercion strictly:
//!   - `new TextDecoder(null)` → `null` coerces to the string
//!     `"null"`; rejected as an unknown label → `RangeError`.
//!   - `new TextDecoder("utf-8", "fatal")` → non-object as dictionary
//!     → `TypeError`.
//!   - `decode(42)` → not a `BufferSource` → `TypeError`.
//!   - `decode(bytes, "stream")` → non-object as options → `TypeError`.
//!
//! - **Symbol.toStringTag** is set on the prototype by the
//!   `#[v8_class]` macro's install codegen, so
//!   `Object.prototype.toString.call(new TextDecoder())` returns
//!   `"[object TextDecoder]"` (libraries like webidl-conversions
//!   check this).

use encoding_rs::{DecoderResult, Encoding};
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

pub mod streams;

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

    /// `encodeInto(source: USVString, destination: Uint8Array) ->
    /// TextEncoderEncodeIntoResult`
    ///
    /// Writes UTF-8 bytes for `source` into `destination`. Returns
    /// `{ read, written }` where:
    /// - `read` is the number of UTF-16 code units consumed from
    ///   `source` (an unpaired surrogate counts as 1 code unit;
    ///   a surrogate pair counts as 2).
    /// - `written` is the number of bytes written to `destination`.
    ///
    /// If a multi-byte UTF-8 sequence wouldn't fit in the remaining
    /// destination space, no bytes are written for that codepoint
    /// (the partial sequence is not split). `read` reflects only
    /// codepoints whose UTF-8 bytes fully fit.
    #[v8_method]
    #[allow(non_snake_case)]
    fn encodeInto<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        source: v8::Local<v8::Value>,
        destination: v8::Local<v8::Value>,
    ) -> Result<v8::Local<'s, v8::Value>, OpError> {
        // Source is `USVString`. Per WebIDL §3.2.10, USVString
        // conversion is: ToString(V), then replace unpaired
        // surrogates with U+FFFD. So `encodeInto(42, dest)` should
        // encode "42", not throw. V8's `Value::to_string` performs
        // ToString — returns None only if the conversion threw
        // (e.g., a Symbol). We propagate that rejection as a
        // generic OpError; the V8-side pending exception will
        // surface as the macro's error path runs.
        let Some(src_str) = source.to_string(scope) else {
            return Err(OpError::type_error(
                "TextEncoder.encodeInto: source could not be converted to a string",
            ));
        };
        let src_len = src_str.length();
        let mut src_utf16 = vec![0u16; src_len];
        src_str.write_v2(scope, 0, &mut src_utf16, v8::WriteFlags::empty());

        // Destination must be a Uint8Array (per the WebIDL signature
        // `[AllowShared] Uint8Array destination`). Other ArrayBufferViews
        // throw TypeError.
        let Ok(dest_view) = v8::Local::<v8::Uint8Array>::try_from(destination) else {
            return Err(OpError::type_error(
                "TextEncoder.encodeInto: destination must be a Uint8Array",
            ));
        };

        let dest_len = dest_view.byte_length();
        let mut tmp = vec![0u8; dest_len];

        let mut encoder = encoding_rs::UTF_8.new_encoder();
        let (_result, read_u16, written, _had_replacements) =
            encoder.encode_from_utf16(&src_utf16, &mut tmp, true);

        // Copy `tmp[..written]` into the destination Uint8Array's
        // backing store. We can't hand encoding_rs a `&mut [u8]` view
        // of the V8 typed array directly (V8 requires Cell<u8>
        // access via the SharedRef<BackingStore>), so a small
        // intermediate buffer is unavoidable here.
        if written > 0 {
            let ab = dest_view
                .buffer(scope)
                .ok_or_else(|| OpError::error("TextEncoder.encodeInto: destination has no backing buffer"))?;
            let offset = dest_view.byte_offset();
            let store = ab.get_backing_store();
            for i in 0..written {
                store[offset + i].set(tmp[i]);
            }
        }

        // Build the result object: `{ read: u32, written: u32 }`.
        let result = v8::Object::new(scope);
        let read_key = v8::String::new(scope, "read").unwrap();
        let read_val = v8::Number::new(scope, read_u16 as f64);
        result.set(scope, read_key.into(), read_val.into());
        let written_key = v8::String::new(scope, "written").unwrap();
        let written_val = v8::Number::new(scope, written as f64);
        result.set(scope, written_key.into(), written_val.into());
        Ok(result.into())
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
    /// The encoding this decoder was constructed with. We keep a
    /// `&'static Encoding` reference (encoding_rs returns these as
    /// statics) and use it to spin up fresh `Decoder` instances per
    /// session.
    encoding: &'static Encoding,
    /// Lazily-instantiated `encoding_rs::Decoder`. Recreated after
    /// every non-streaming flush because (a) encoding_rs panics on
    /// reuse-after-finalize, and (b) WPT byte-order-marks tests
    /// confirm WHATWG semantics: BOM is stripped on EVERY
    /// non-streaming call's first byte, not just the first call's
    /// for the lifetime of the TextDecoder. Recreating the decoder
    /// per session naturally resets BOM-removal state too.
    decoder: Option<encoding_rs::Decoder>,
    fatal_flag: bool,
    ignore_bom_flag: bool,
}

impl Default for TextDecoder {
    fn default() -> Self {
        TextDecoder {
            encoding: encoding_rs::UTF_8,
            decoder: None,
            fatal_flag: false,
            ignore_bom_flag: false,
        }
    }
}

#[v8_class]
impl TextDecoder {
    /// `new TextDecoder(label?: DOMString, options?: TextDecoderOptions)`
    ///
    /// `label` defaults to `"utf-8"` per spec. Resolved via WHATWG's
    /// "get an encoding" algorithm (`encoding_rs::Encoding::for_label`),
    /// which strips ASCII whitespace and applies ASCII-case-insensitive
    /// comparison against the encoding label table. Unknown labels and
    /// the `replacement` encoding throw `RangeError` per spec.
    /// `options` must be `undefined`, `null`, or a plain object;
    /// anything else throws `TypeError` per WebIDL §3.2.20.
    #[v8_constructor]
    fn new(
        scope: &mut v8::PinScope,
        label: v8::Local<v8::Value>,
        options: v8::Local<v8::Value>,
    ) -> Result<Self, OpError> {
        // Per spec: label defaults to "utf-8" only when undefined.
        // null coerces to the string "null" via WebIDL DOMString,
        // which `Encoding::for_label` will reject as unknown.
        let label_str = if label.is_undefined() {
            "utf-8".to_string()
        } else {
            label.to_rust_string_lossy(scope)
        };

        let encoding = match Encoding::for_label(label_str.as_bytes()) {
            Some(enc) => enc,
            None => {
                return Err(OpError::range_error(
                    "TextDecoder: unsupported encoding label",
                ));
            }
        };

        // Per WHATWG §4.2 step 4: if the encoding is the
        // `replacement` encoding, throw RangeError. encoding_rs
        // exposes it as `REPLACEMENT`; the spec disallows
        // constructing a TextDecoder for it.
        if encoding == encoding_rs::REPLACEMENT {
            return Err(OpError::range_error(
                "TextDecoder: replacement encoding is not a valid label for TextDecoder",
            ));
        }

        let (fatal_flag, ignore_bom_flag) = read_decoder_options(scope, options)?;

        Ok(TextDecoder {
            encoding,
            decoder: None,
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
        // Order matters per WebIDL: process `options` first because
        // its getters may have side effects (the WPT test
        // `textdecoder-arguments.any.js` detaches the input buffer
        // inside the `stream` getter). Only after options coerces
        // do we read the buffer's bytes; if it was detached during
        // options, we get an empty input.
        let stream = read_stream_option(scope, options)?;
        let bytes = read_buffer_source(input)?;

        // Lazy-init or re-init the decoder. We drop it after every
        // non-streaming `decode()` (see the bottom of this method),
        // so each new "decode session" starts with a fresh decoder
        // — and thus fresh BOM-removal state. WPT's
        // textdecoder-byte-order-marks asserts BOM stripping on
        // every non-streaming call's input, which only works if
        // BOM detection resets per session.
        if self.decoder.is_none() {
            let dec = if self.ignore_bom_flag {
                self.encoding.new_decoder_without_bom_handling()
            } else {
                self.encoding.new_decoder_with_bom_removal()
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
        // Per WHATWG §4.2 step 5, the canonical name is the
        // lowercase form. encoding_rs's `name()` returns the
        // canonical name in title case (e.g. `"UTF-8"`,
        // `"windows-1252"`); the spec wants lowercase
        // (`"utf-8"`, `"windows-1252"`). For ASCII-only names this
        // is just `to_ascii_lowercase`.
        self.encoding.name().to_ascii_lowercase()
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
    // KNOWN GAP: if `obj.get()` triggers a user-supplied Proxy or
    // accessor that throws, V8 sets a pending exception and
    // returns None. Per WebIDL §3.2.20 the original exception
    // should propagate to the caller; instead we surface a
    // generic OpError here, which the macro re-throws as our own
    // Error with a less helpful message. Properly preserving the
    // V8 exception requires either a sentinel OpError variant
    // that the macro recognizes as "exception already pending,
    // don't overwrite," or scope-aware Result type. Tracked as a
    // future macro feature; the encoding tests don't exercise
    // this path.
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
