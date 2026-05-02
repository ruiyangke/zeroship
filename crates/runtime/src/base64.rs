//! Native `atob` and `btoa` per WHATWG HTML §8.6
//! (https://html.spec.whatwg.org/multipage/webappapis.html#atob-and-btoa).
//!
//! Replaces the JS polyfill that lived in `embed/fetch.js`. The polyfill
//! had a tight loop using indexOf into an alphabet string and silently
//! garbled invalid input — observable bugs:
//!
//!   - `btoa("\u0100")` (a U+0100 code point, 0x100 > 0xFF) silently
//!     dropped the high byte. Spec says throw
//!     `DOMException("InvalidCharacterError")`.
//!   - `atob("not!valid")` silently returned a corrupted string instead
//!     of throwing.
//!   - Whitespace handling: per spec, ASCII whitespace (tab/LF/CR/SP/FF)
//!     in the atob input is stripped before decoding. The polyfill
//!     didn't strip, so `atob("aGVsbG8=\n")` failed.
//!
//! The native implementation uses the `base64` crate's STANDARD engine
//! (`+/=`), with explicit pre-validation of input bytes for `btoa` and
//! whitespace stripping for `atob`. Both throw native `DOMException`
//! ("InvalidCharacterError") on failure.

use base64::Engine;

use super::dom::exception;

/// `btoa(data: USVString) -> DOMString`
///
/// Per spec:
///   1. If any code unit of `data` is > 0xFF, throw
///      `InvalidCharacterError`.
///   2. Otherwise, treat each code unit as a byte and encode as
///      base64 with `+`/`/` and `=` padding.
///
/// USVString conversion (lone surrogate → U+FFFD) is implicit — V8's
/// `to_string(scope)` for the input `Local<Value>` performs `ToString`
/// per ECMA-262, then we decode as UTF-16 code units. Any code unit
/// over 0xFF triggers the spec throw.
pub fn btoa_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let arg = args.get(0);
    // Per spec: missing arg coerces to "undefined" string per ToString.
    // (https://html.spec.whatwg.org/#dom-btoa step 1: "Let bytes be
    // the result of UTF-8 encoding data" — but data is DOMString, so
    // ToString first.) The "undefined" string contains code units all
    // ≤ 0xFF so it's encodeable; matches browser behavior.
    let Some(s) = arg.to_string(scope) else {
        // V8 has a pending exception (e.g. throwing toString) — yield.
        return;
    };
    // Read the JS string as UTF-16 code units; any code unit > 0xFF
    // is the spec's "out-of-range" trigger.
    let len = s.length();
    let mut buf16 = vec![0u16; len];
    s.write_v2(scope, 0, &mut buf16, v8::WriteFlags::empty());
    let mut bytes = Vec::with_capacity(len);
    for cu in &buf16 {
        if *cu > 0xFF {
            exception::throw(
                scope,
                "btoa: input contains code units outside Latin-1 range",
                "InvalidCharacterError",
            );
            return;
        }
        bytes.push(*cu as u8);
    }

    let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
    let out = v8::String::new(scope, &encoded).unwrap();
    rv.set(out.into());
}

/// `atob(data: DOMString) -> ByteString`
///
/// Per spec:
///   1. Strip ASCII whitespace from `data`.
///   2. If the stripped length is divisible by 4 only after removing
///      one or two `=` from the end, do that.
///   3. If any character outside `[A-Za-z0-9+/=]` remains, throw
///      `InvalidCharacterError`.
///   4. Decode as base64; result is a sequence of bytes.
///   5. Return as a ByteString (each byte → one code unit).
///
/// The `base64` crate's STANDARD engine handles `+/=`, but rejects
/// some patterns the spec accepts (and accepts some it doesn't).
/// `STANDARD_NO_PAD` is closer; even closer is `STANDARD` with
/// `decode_allow_trailing_bits = true` and `decode_padding_mode =
/// Indifferent`. We build a custom config to match the spec's
/// "forgiving base64 decode" algorithm.
pub fn atob_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let arg = args.get(0);
    let Some(s) = arg.to_string(scope) else {
        return;
    };
    let input = s.to_rust_string_lossy(scope);

    // Per WHATWG infra "forgiving base64 decode":
    // (https://infra.spec.whatwg.org/#forgiving-base64-decode)
    //   1. Remove all ASCII whitespace.
    let mut filtered: Vec<u8> = Vec::with_capacity(input.len());
    for &b in input.as_bytes() {
        // ASCII whitespace per Infra §"ASCII whitespace": TAB, LF, FF,
        // CR, SP. (https://infra.spec.whatwg.org/#ascii-whitespace)
        if matches!(b, b'\t' | b'\n' | 0x0C | b'\r' | b' ') {
            continue;
        }
        filtered.push(b);
    }

    //   2. If data ends with one or two U+003D (=) code points, remove
    //      them from data. (Per Infra "forgiving base64 decode" §3 —
    //      this is a one-shot strip of up to 2 `=`, NOT alignment-driven.)
    if filtered.ends_with(b"=") {
        filtered.pop();
        if filtered.ends_with(b"=") {
            filtered.pop();
        }
    }

    //   3. If length % 4 == 1, throw.
    if filtered.len() % 4 == 1 {
        exception::throw(
            scope,
            "atob: invalid base64 (length not aligned to 4 after stripping)",
            "InvalidCharacterError",
        );
        return;
    }

    //   4. Validate each character is base64. (After step 2 there must
    //      be no `=` left; if there is, it's an error per spec — Infra
    //      step 4 says throw if any char is not in the alphabet.)
    for &b in &filtered {
        let ok = matches!(b, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'+' | b'/');
        if !ok {
            exception::throw(
                scope,
                "atob: input contains characters outside the base64 alphabet",
                "InvalidCharacterError",
            );
            return;
        }
    }

    //   5. Decode (no padding required by this point).
    let cfg = base64::engine::GeneralPurposeConfig::new()
        .with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent)
        .with_decode_allow_trailing_bits(true);
    let engine = base64::engine::GeneralPurpose::new(&base64::alphabet::STANDARD, cfg);
    let bytes = match engine.decode(&filtered) {
        Ok(b) => b,
        Err(_) => {
            exception::throw(
                scope,
                "atob: base64 decode failed",
                "InvalidCharacterError",
            );
            return;
        }
    };

    // Each byte → one UTF-16 code unit. Use one-byte string for the
    // fast path (V8 represents Latin-1 strings inline).
    let out = v8::String::new_from_one_byte(scope, &bytes, v8::NewStringType::Normal)
        .expect("atob: V8 string allocation failed");
    rv.set(out.into());
}

// ---------------------------------------------------------------------------
// install_global
// ---------------------------------------------------------------------------

/// Install `btoa` / `atob` on `globalThis`. Called from
/// `init::setup_globals`.
pub fn install_global<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    global: v8::Local<v8::Object>,
) {
    let btoa_fn = v8::Function::new(scope, btoa_callback).unwrap();
    let btoa_key = v8::String::new(scope, "btoa").unwrap();
    global.set(scope, btoa_key.into(), btoa_fn.into());

    let atob_fn = v8::Function::new(scope, atob_callback).unwrap();
    let atob_key = v8::String::new(scope, "atob").unwrap();
    global.set(scope, atob_key.into(), atob_fn.into());
}
