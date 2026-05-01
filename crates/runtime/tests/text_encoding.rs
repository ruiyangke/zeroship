//! End-to-end tests for native `TextEncoder` and `TextDecoder`.
//!
//! Covers the spec corners that the previous JS polyfill got wrong:
//!
//!   - `TextDecoder.decode(chunk, { stream: true })` preserves
//!     partial-UTF-8 state across calls. Splitting a 3-byte CJK
//!     codepoint across chunks must NOT produce U+FFFD — this was
//!     the AI SDK / SSE bug.
//!   - BOM stripping on first call only, with `ignoreBOM` opt-out.
//!   - `fatal: true` throws TypeError on invalid sequences instead
//!     of substituting U+FFFD.
//!   - Encoding label normalization (utf-8 / utf8 / case variants).
//!   - Round-trip identity through encode → decode for non-ASCII.

#![allow(unsafe_code)]

use zeroship_runtime::init_v8;
use zeroship_runtime::text_encoding::{TextDecoder, TextEncoder};

// ---------------------------------------------------------------------------
// Test harness
// ---------------------------------------------------------------------------

fn run_in_v8<R>(src: &str, f: impl FnOnce(v8::Local<v8::Value>, &mut v8::PinScope) -> R) -> R {
    init_v8();
    let mut isolate = v8::Isolate::new(v8::CreateParams::default());
    v8::scope!(let handle_scope, &mut isolate);
    let context = v8::Context::new(handle_scope, Default::default());
    let scope = &mut v8::ContextScope::new(handle_scope, context);

    // Install both classes on globalThis.
    let global = scope.get_current_context().global(scope);
    for (name, tmpl) in [
        ("TextEncoder", TextEncoder::install(scope)),
        ("TextDecoder", TextDecoder::install(scope)),
    ] {
        let class_fn = tmpl.get_function(scope).unwrap();
        let key = v8::String::new(scope, name).unwrap();
        global.set(scope, key.into(), class_fn.into());
    }

    let src_v8 = v8::String::new(scope, src).unwrap();
    let script = v8::Script::compile(scope, src_v8, None).unwrap();
    let result = script.run(scope).unwrap();
    f(result, scope)
}

fn js_string(val: v8::Local<v8::Value>, scope: &mut v8::PinScope) -> String {
    val.to_rust_string_lossy(scope)
}

// ---------------------------------------------------------------------------
// TextEncoder
// ---------------------------------------------------------------------------

#[test]
fn encoder_encoding_property_is_utf8() {
    let s = run_in_v8(
        "new TextEncoder().encoding",
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "utf-8");
}

#[test]
fn encoder_returns_uint8array() {
    // Per WHATWG spec, encode() returns Uint8Array, not ArrayBuffer.
    let s = run_in_v8(
        r#"
        const out = new TextEncoder().encode("hi");
        JSON.stringify({
            kind: out.constructor.name,
            isView: ArrayBuffer.isView(out),
            len: out.byteLength,
        });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"kind":"Uint8Array","isView":true,"len":2}"#);
}

#[test]
fn encoder_encodes_ascii() {
    let s = run_in_v8(
        r#"
        Array.from(new TextEncoder().encode("hello")).join(",");
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "104,101,108,108,111");
}

#[test]
fn encoder_encodes_multibyte() {
    // U+4F60 U+597D ("你好") = E4 BD A0 E5 A5 BD
    let s = run_in_v8(
        r#"
        Array.from(new TextEncoder().encode("你好")).map(b => b.toString(16)).join(",");
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "e4,bd,a0,e5,a5,bd");
}

#[test]
fn encoder_encodes_emoji() {
    // 😀 = U+1F600 = surrogate pair in JS, 4-byte UTF-8 (F0 9F 98 80)
    let s = run_in_v8(
        r#"
        Array.from(new TextEncoder().encode("😀")).map(b => b.toString(16)).join(",");
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "f0,9f,98,80");
}

#[test]
fn encoder_with_no_arg_returns_empty() {
    let s = run_in_v8(
        r#"
        new TextEncoder().encode().byteLength;
        "#,
        |val, scope| val.uint32_value(scope).unwrap(),
    );
    assert_eq!(s, 0);
}

// ---------------------------------------------------------------------------
// TextDecoder — basic + spec labels
// ---------------------------------------------------------------------------

#[test]
fn decoder_default_encoding_is_utf8() {
    let s = run_in_v8(
        "new TextDecoder().encoding",
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "utf-8");
}

#[test]
fn decoder_accepts_utf8_label_aliases() {
    // Spec normalizes any of these to "utf-8".
    let s = run_in_v8(
        r#"
        const labels = ["utf-8", "UTF-8", "utf8", "Utf8", "  utf-8  ", "unicode-1-1-utf-8", "unicode11utf8"];
        labels.map(l => new TextDecoder(l).encoding).join(",");
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "utf-8,utf-8,utf-8,utf-8,utf-8,utf-8,utf-8");
}

#[test]
fn decoder_rejects_unknown_encoding() {
    let s = run_in_v8(
        r#"
        let kind, msg;
        try { new TextDecoder("ascii"); }
        catch (e) { kind = e.constructor.name; msg = e.message; }
        JSON.stringify({ kind, msg });
        "#,
        |val, scope| js_string(val, scope),
    );
    // We accept only utf-8 for now; ascii is a real WHATWG encoding
    // we don't implement. Spec says throw RangeError.
    assert!(s.contains(r#""kind":"RangeError""#), "got: {s}");
    assert!(s.contains("ascii"), "got: {s}");
}

#[test]
fn decoder_decodes_ascii() {
    let s = run_in_v8(
        r#"
        const dec = new TextDecoder();
        const bytes = new Uint8Array([104, 101, 108, 108, 111]);
        dec.decode(bytes);
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "hello");
}

#[test]
fn decoder_decodes_multibyte() {
    let s = run_in_v8(
        r#"
        const dec = new TextDecoder();
        const bytes = new Uint8Array([0xE4, 0xBD, 0xA0, 0xE5, 0xA5, 0xBD]);
        dec.decode(bytes);
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "你好");
}

// ---------------------------------------------------------------------------
// TextDecoder — streaming UTF-8 (the bug we're fixing)
// ---------------------------------------------------------------------------

#[test]
fn decoder_streams_multibyte_split_across_chunks() {
    // "你" is 0xE4 0xBD 0xA0 — split between bytes 1 and 2, the
    // decoder must hold the partial sequence and emit it once chunk
    // 2 arrives. The OLD JS polyfill produced "U+FFFDU+FFFDU+FFFD"
    // here because it had no streaming state.
    let s = run_in_v8(
        r#"
        const dec = new TextDecoder("utf-8");
        const chunk1 = new Uint8Array([0xE4]);          // 1st byte of 你
        const chunk2 = new Uint8Array([0xBD, 0xA0]);    // 2nd+3rd bytes
        const a = dec.decode(chunk1, { stream: true });
        const b = dec.decode(chunk2, { stream: true });
        // Final flush call with no input — drains any tail.
        const c = dec.decode();
        JSON.stringify({ a, b, c, joined: a + b + c });
        "#,
        |val, scope| js_string(val, scope),
    );
    // First chunk has incomplete sequence — produces empty string,
    // bytes are buffered. Second chunk completes the codepoint.
    assert!(s.contains(r#""a":"""#), "got: {s}");
    assert!(s.contains(r#""joined":"你""#), "got: {s}");
}

#[test]
fn decoder_streams_emoji_split_across_chunks() {
    // 😀 = F0 9F 98 80 — split right in the middle (2 bytes + 2 bytes)
    let s = run_in_v8(
        r#"
        const dec = new TextDecoder();
        const chunk1 = new Uint8Array([0xF0, 0x9F]);
        const chunk2 = new Uint8Array([0x98, 0x80]);
        const a = dec.decode(chunk1, { stream: true });
        const b = dec.decode(chunk2, { stream: true });
        a + b;
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "😀");
}

#[test]
fn decoder_non_streaming_replaces_incomplete_tail() {
    // Without stream:true, an incomplete trailing sequence at the
    // end is replaced with U+FFFD (or throws in fatal mode).
    let s = run_in_v8(
        r#"
        const dec = new TextDecoder();
        const bytes = new Uint8Array([0xE4]);  // only 1st byte of 3-byte seq
        dec.decode(bytes);
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "\u{FFFD}");
}

#[test]
fn decoder_streaming_then_flush_emits_replacement_for_unfinished_tail() {
    // If we never complete the sequence, a final non-stream call
    // should flush as U+FFFD.
    let s = run_in_v8(
        r#"
        const dec = new TextDecoder();
        const chunk1 = new Uint8Array([0xE4]);  // 1st of 3
        const a = dec.decode(chunk1, { stream: true });
        const b = dec.decode();  // flush, no more bytes
        a + "|" + b;
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "|\u{FFFD}");
}

// ---------------------------------------------------------------------------
// TextDecoder — fatal mode
// ---------------------------------------------------------------------------

#[test]
fn decoder_fatal_throws_on_invalid_sequence() {
    let s = run_in_v8(
        r#"
        const dec = new TextDecoder("utf-8", { fatal: true });
        const bytes = new Uint8Array([0xC0, 0xC0]);  // overlong / invalid
        let kind, msg;
        try { dec.decode(bytes); }
        catch (e) { kind = e.constructor.name; msg = e.message; }
        JSON.stringify({ kind, msg, fatal: dec.fatal });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert!(s.contains(r#""kind":"TypeError""#), "got: {s}");
    assert!(s.contains(r#""fatal":true"#), "got: {s}");
}

#[test]
fn decoder_non_fatal_substitutes_invalid_sequence() {
    let s = run_in_v8(
        r#"
        const dec = new TextDecoder();
        const bytes = new Uint8Array([0x68, 0xC0, 0x69]);  // 'h' + bad + 'i'
        dec.decode(bytes);
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "h\u{FFFD}i");
}

// ---------------------------------------------------------------------------
// TextDecoder — BOM handling
// ---------------------------------------------------------------------------

#[test]
fn decoder_strips_bom_by_default() {
    let s = run_in_v8(
        r#"
        const dec = new TextDecoder();
        const bytes = new Uint8Array([0xEF, 0xBB, 0xBF, 0x68, 0x69]);  // BOM + "hi"
        JSON.stringify({ ignoreBOM: dec.ignoreBOM, decoded: dec.decode(bytes) });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert!(s.contains(r#""ignoreBOM":false"#), "got: {s}");
    assert!(s.contains(r#""decoded":"hi""#), "got: {s}");
}

#[test]
fn decoder_keeps_bom_when_ignoreBOM_set() {
    // Probe via codePointAt rather than serializing — JSON.stringify
    // doesn't escape U+FEFF (it's a valid JSON string char), so the
    // raw BOM ends up in the assertion target as itself, which is
    // hard to spot in test output.
    let s = run_in_v8(
        r#"
        const dec = new TextDecoder("utf-8", { ignoreBOM: true });
        const bytes = new Uint8Array([0xEF, 0xBB, 0xBF, 0x68, 0x69]);
        const out = dec.decode(bytes);
        JSON.stringify({
            ignoreBOM: dec.ignoreBOM,
            length: out.length,
            cp0: out.codePointAt(0),
            cp1: out.codePointAt(1),
            cp2: out.codePointAt(2),
        });
        "#,
        |val, scope| js_string(val, scope),
    );
    // Length 3: BOM + 'h' + 'i'. cp0 = 0xFEFF, cp1 = 0x68, cp2 = 0x69.
    assert_eq!(
        s,
        r#"{"ignoreBOM":true,"length":3,"cp0":65279,"cp1":104,"cp2":105}"#
    );
}

#[test]
fn decoder_only_strips_bom_on_first_call() {
    let s = run_in_v8(
        r#"
        const dec = new TextDecoder();
        const bom = new Uint8Array([0xEF, 0xBB, 0xBF]);
        const a = dec.decode(bom);            // first call → BOM stripped → ""
        const b = dec.decode(bom);            // second call → BOM kept
        JSON.stringify({
            aLen: a.length,
            bLen: b.length,
            bCp: b.length > 0 ? b.codePointAt(0) : null,
        });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"aLen":0,"bLen":1,"bCp":65279}"#);
}

// ---------------------------------------------------------------------------
// Round-trip
// ---------------------------------------------------------------------------

#[test]
fn encode_decode_round_trip_preserves_strings() {
    let s = run_in_v8(
        r#"
        const enc = new TextEncoder();
        const dec = new TextDecoder();
        const inputs = [
            "hello",
            "你好,世界",
            "🚀✨🎉",
            "mixed: ascii 你 emoji 😀 done",
            "",
        ];
        const round = inputs.map(s => dec.decode(enc.encode(s)));
        JSON.stringify(inputs.map((s, i) => s === round[i]));
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "[true,true,true,true,true]");
}
