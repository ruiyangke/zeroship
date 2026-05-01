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

/// `encodeInto` source param is `USVString` per WebIDL § 3.2.10.
/// USVString coercion is: ToString(V), then replace unpaired
/// surrogates with U+FFFD. So `encodeInto(42, dest)` should encode
/// the string "42", not throw TypeError.
#[test]
fn encode_into_coerces_non_string_source_via_tostring() {
    let s = run_in_v8(
        r#"
        const dest = new Uint8Array(8);
        const r = new TextEncoder().encodeInto(42, dest);
        // "42" → 2 ASCII bytes → r.read=2, r.written=2, dest[0]='4'
        JSON.stringify({ read: r.read, written: r.written, b0: dest[0], b1: dest[1] });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"read":2,"written":2,"b0":52,"b1":50}"#);
}

/// Symbol can't be ToString-coerced; spec says throw.
#[test]
fn encode_into_throws_on_symbol_source() {
    let s = run_in_v8(
        r#"
        const dest = new Uint8Array(8);
        let kind;
        try { new TextEncoder().encodeInto(Symbol("x"), dest); }
        catch (e) { kind = e.constructor.name; }
        kind;
        "#,
        |val, scope| js_string(val, scope),
    );
    // V8's ToString(Symbol) throws TypeError natively; our
    // `Value::to_string` returns None and we surface a TypeError too.
    assert_eq!(s, "TypeError");
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
        let kind;
        // "fakeenc" is not a valid WHATWG encoding label.
        try { new TextDecoder("fakeenc"); }
        catch (e) { kind = e.constructor.name; }
        kind;
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "RangeError");
}

#[test]
fn decoder_accepts_legacy_encodings() {
    // We support the full WHATWG encoding set via encoding_rs.
    // ascii / latin1 / Windows-1252 / Big5 / Shift_JIS / GB18030 /
    // UTF-16LE etc. all work.
    let s = run_in_v8(
        r#"
        const labels = ["ascii", "latin1", "windows-1252", "Big5",
                        "shift_jis", "gb18030", "utf-16le", "utf-16be"];
        labels.map(l => new TextDecoder(l).encoding).join(",");
        "#,
        |val, scope| js_string(val, scope),
    );
    // ASCII-canonical names per WHATWG; encoding_rs returns
    // canonical-but-mixed-case for some, then we lowercase.
    assert_eq!(
        s,
        "windows-1252,windows-1252,windows-1252,big5,shift_jis,gb18030,utf-16le,utf-16be"
    );
}

#[test]
fn decoder_rejects_replacement_encoding_label() {
    // Per WHATWG §4.2 step 4, the `replacement` encoding can't be
    // constructed via new TextDecoder() — it's used only for
    // labels like ISO-2022-CN that don't have a real decoder.
    let s = run_in_v8(
        r#"
        let kind;
        try { new TextDecoder("iso-2022-cn"); }
        catch (e) { kind = e.constructor.name; }
        kind;
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "RangeError");
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
fn decoder_strips_bom_on_every_non_streaming_call() {
    // Per WPT textdecoder-byte-order-marks: BOM is stripped on every
    // non-streaming call's first byte, not just the first call's
    // for the lifetime of the TextDecoder. (My initial impl had a
    // sticky `bom_seen` flag — wrong. Correct behavior: each
    // non-streaming `decode()` is its own session; BOM-removal
    // state resets between sessions.)
    let s = run_in_v8(
        r#"
        const dec = new TextDecoder();
        const bom = new Uint8Array([0xEF, 0xBB, 0xBF]);
        const a = dec.decode(bom);  // first call → BOM stripped → ""
        const b = dec.decode(bom);  // second call → ALSO stripped
        JSON.stringify({ aLen: a.length, bLen: b.length });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"aLen":0,"bLen":0}"#);
}

// ---------------------------------------------------------------------------
// Round-trip
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Spec-compliance regression tests (from harsh review)
// ---------------------------------------------------------------------------

/// BOM split across two streaming chunks must still get stripped on
/// the very first call's behalf — the BOM is part of the prefix of
/// the byte stream, not the prefix of any one chunk. The previous
/// hand-rolled implementation set `bom_consumed = true` on the first
/// chunk regardless of whether it actually contained the BOM
/// sequence, so a BOM split as `[EF] | [BB BF, ..]` left the BOM in
/// the output.
#[test]
fn decoder_strips_bom_split_across_streaming_chunks() {
    let s = run_in_v8(
        r#"
        const dec = new TextDecoder();
        const a = dec.decode(new Uint8Array([0xEF]),       { stream: true });
        const b = dec.decode(new Uint8Array([0xBB, 0xBF, 0x68, 0x69]));
        const out = a + b;
        JSON.stringify({ length: out.length, value: out });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"length":2,"value":"hi"}"#);
}

/// Empty `decode()` calls must NOT flip the BOM-seen flag. The
/// previous impl set `bom_consumed = true` on every decode() entry,
/// so a `dec.decode(new Uint8Array([]))` followed by a real BOM
/// chunk left the BOM in output.
#[test]
fn decoder_empty_call_does_not_consume_bom_state() {
    let s = run_in_v8(
        r#"
        const dec = new TextDecoder();
        dec.decode(new Uint8Array([]));        // empty — must NOT mark BOM as seen
        const out = dec.decode(new Uint8Array([0xEF, 0xBB, 0xBF, 0x68]));
        JSON.stringify({ length: out.length, value: out });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"length":1,"value":"h"}"#);
}

/// Per WebIDL §3.2.20, passing a non-object non-undefined non-null
/// to a method that expects a dictionary throws TypeError. The
/// previous impl silently ignored such args and used defaults.
#[test]
fn decoder_constructor_rejects_non_object_options() {
    let s = run_in_v8(
        r#"
        let kind;
        try { new TextDecoder("utf-8", "fatal"); }
        catch (e) { kind = e.constructor.name; }
        kind;
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "TypeError");
}

#[test]
fn decode_rejects_non_object_options() {
    let s = run_in_v8(
        r#"
        const dec = new TextDecoder();
        let kind;
        try { dec.decode(new Uint8Array([0x68]), "stream"); }
        catch (e) { kind = e.constructor.name; }
        kind;
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "TypeError");
}

/// `decode()` only accepts `BufferSource` (ArrayBuffer or
/// ArrayBufferView). Numbers, strings, or other types must throw
/// TypeError per WebIDL union coercion. The previous impl silently
/// returned an empty string.
#[test]
fn decode_rejects_non_buffer_input() {
    let s = run_in_v8(
        r#"
        const dec = new TextDecoder();
        let kind;
        try { dec.decode(42); }
        catch (e) { kind = e.constructor.name; }
        kind;
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "TypeError");
}

/// `null` label coerces to the string `"null"` per WebIDL DOMString
/// rules — that's not a valid encoding alias, so spec requires
/// throwing RangeError.
#[test]
fn decoder_constructor_rejects_null_label() {
    let s = run_in_v8(
        r#"
        let kind;
        try { new TextDecoder(null); }
        catch (e) { kind = e.constructor.name; }
        kind;
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "RangeError");
}

/// Label normalization is ASCII-only per WHATWG §4.2. encoding_rs's
/// `Encoding::for_label` implements this directly (strips ASCII
/// whitespace HT/LF/FF/CR/SP, ASCII-case-insensitive comparison).
#[test]
fn decoder_label_uses_ascii_only_normalization() {
    // U+00A0 NBSP is not ASCII whitespace; not stripped → invalid
    // label.
    let s = run_in_v8(
        r#"
        let kind;
        try { new TextDecoder("\u00A0utf-8"); }
        catch (e) { kind = e.constructor.name; }
        kind;
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "RangeError");
}

#[test]
fn decoder_accepts_full_utf8_label_table() {
    // Per WHATWG encodings.json, the canonical UTF-8 aliases include
    // these. The previous impl was missing `unicode20utf8` and
    // `x-unicode20utf8`.
    let s = run_in_v8(
        r#"
        const labels = [
            "utf-8", "UTF-8", "utf8", "UTF8",
            "unicode-1-1-utf-8", "unicode11utf8",
            "unicode20utf8", "x-unicode20utf8",
            "  utf-8  ",  // ASCII whitespace is allowed
        ];
        labels.map(l => new TextDecoder(l).encoding).join(",");
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "utf-8,utf-8,utf-8,utf-8,utf-8,utf-8,utf-8,utf-8,utf-8");
}

/// Per WHATWG, `Object.prototype.toString.call(new TextDecoder())`
/// must be `"[object TextDecoder]"`. The class name on the
/// FunctionTemplate makes this work via V8's default
/// Symbol.toStringTag handling.
#[test]
fn decoder_has_correct_string_tag() {
    let s = run_in_v8(
        r#"
        Object.prototype.toString.call(new TextDecoder());
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "[object TextDecoder]");
}

#[test]
fn encoder_has_correct_string_tag() {
    let s = run_in_v8(
        r#"
        Object.prototype.toString.call(new TextEncoder());
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "[object TextEncoder]");
}

/// WHATWG decoder algorithm: invalid bytes get U+FFFD per "error
/// grouping" rules. Rust's `std::str::from_utf8` doesn't match these
/// rules; switching to encoding_rs (which is the spec reference
/// impl) does.
///
/// Test vector from web-platform-tests:
/// `[0xF1, 0x80, 0x80, 0xE1, 0x80, 0xC0, 0x10]` per WHATWG decode:
///   - `F1 80 80` is a 4-byte lead + 2 continuations; needs one more
///     continuation. Next byte `E1` is NOT a continuation (it's a
///     3-byte lead), so emit U+FFFD and back up to `E1`.
///   - `E1 80` is a 3-byte lead + 1 continuation; needs one more.
///     Next byte `C0` is NOT a continuation (lead bytes 0xC0/0xC1 are
///     forbidden as overlong), so emit U+FFFD and back up to `C0`.
///   - `C0` is invalid as a lead byte → emit U+FFFD.
///   - `10` is ASCII → passthrough.
/// Result: `"\uFFFD\uFFFD\uFFFD\u0010"` — 4 codepoints. Rust's
/// `std::str::from_utf8` produces fewer FFFDs because its error
/// grouping isn't the WHATWG state machine.
#[test]
fn decoder_ufffd_count_matches_whatwg_spec() {
    let s = run_in_v8(
        r#"
        const dec = new TextDecoder();
        const out = dec.decode(new Uint8Array([0xF1, 0x80, 0x80, 0xE1, 0x80, 0xC0, 0x10]));
        JSON.stringify({
            len: out.length,
            cps: Array.from(out, c => c.codePointAt(0)),
        });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"len":4,"cps":[65533,65533,65533,16]}"#);
}

/// Spec test: trailing 4-byte sequence prefixes of various lengths
/// should produce one U+FFFD per chunk in non-streaming mode.
#[test]
fn decoder_trailing_partial_4byte_sequence() {
    for (input, expected_len) in [
        // 1-of-4 lead → 1 replacement
        ("[0xF0]", 1u32),
        // 2-of-4 → 1 replacement (the partial 2 bytes form one error)
        ("[0xF0, 0x9F]", 1),
        // 3-of-4 → 1 replacement
        ("[0xF0, 0x9F, 0x98]", 1),
    ] {
        let src = format!(
            r#"
            const dec = new TextDecoder();
            const out = dec.decode(new Uint8Array({input}));
            JSON.stringify({{ len: out.length, cp0: out.codePointAt(0) }});
            "#
        );
        let s = run_in_v8(&src, |val, scope| js_string(val, scope));
        let expected = format!(r#"{{"len":{expected_len},"cp0":65533}}"#);
        assert_eq!(s, expected, "input {input}");
    }
}

/// Unpaired surrogate in TextEncoder input must encode as the UTF-8
/// of U+FFFD (`EF BF BD`). V8's WTF-16→UTF-8 conversion via
/// `to_rust_string_lossy` does this automatically — this test locks
/// in that V8-side behavior so a future change doesn't drift.
#[test]
fn encoder_replaces_unpaired_surrogates() {
    let s = run_in_v8(
        r#"
        const out = new TextEncoder().encode("\uD800");
        Array.from(out).map(b => b.toString(16)).join(",");
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "ef,bf,bd");
}

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
