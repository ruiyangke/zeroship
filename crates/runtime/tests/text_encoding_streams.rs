//! End-to-end tests for native `TextEncoderStream` and `TextDecoderStream`.
//!
//! Targets the spec gaps that the previous JS shim left open:
//!
//!   - `Object.prototype.toString.call(new TextEncoderStream())` must
//!     produce `"[object TextEncoderStream]"` (the shim returned
//!     `"[object Object]"`).
//!   - `instanceof TextEncoderStream` works.
//!   - The `encoding` getter is brand-checked (the shim leaked across
//!     spoofed receivers).
//!   - `readable` / `writable` are spec-shaped getters (per WHATWG
//!     GenericTransformStream §6.1) reading the underlying
//!     TransformStream's `[[readable]]` / `[[writable]]` slots, not own
//!     data props copied at construction.
//!   - The constructor without `new` throws TypeError.
//!   - Decode-side streaming preserves multi-byte UTF-8 state across
//!     chunk boundaries — the AI-SDK SSE-parser bug the JS shim was
//!     written to address.

#![allow(unsafe_code)]

use zeroship_runtime::init_v8;
use zeroship_runtime::streams::install_native_streams;
use zeroship_runtime::text_encoding::{TextDecoder, TextEncoder};
use zeroship_runtime::text_encoding::streams::{TextDecoderStream, TextEncoderStream};

// ---------------------------------------------------------------------------
// Test harness — wires up a fresh isolate with TextEncoder/Decoder,
// native streams (TransformStream, ReadableStream, WritableStream), and
// the new TextEncoderStream / TextDecoderStream classes. Order matters:
// the streams classes must come up before the *Stream classes (which
// construct a TransformStream internally during `new`).
// ---------------------------------------------------------------------------

fn run_in_v8<R>(src: &str, f: impl FnOnce(v8::Local<v8::Value>, &mut v8::PinScope) -> R) -> R {
    init_v8();
    let mut isolate = v8::Isolate::new(v8::CreateParams::default());
    v8::scope!(let handle_scope, &mut isolate);
    let context = v8::Context::new(handle_scope, Default::default());
    let scope = &mut v8::ContextScope::new(handle_scope, context);

    let global = scope.get_current_context().global(scope);

    // 1. TextEncoder / TextDecoder — base classes the streams wrap.
    for (name, tmpl) in [
        ("TextEncoder", TextEncoder::install(scope)),
        ("TextDecoder", TextDecoder::install(scope)),
    ] {
        let class_fn = tmpl.get_function(scope).unwrap();
        let key = v8::String::new(scope, name).unwrap();
        global.set(scope, key.into(), class_fn.into());
    }

    // 2. Native streams (TransformStream, ReadableStream, WritableStream, …).
    install_native_streams(scope, global);

    // 3. The new stream classes.
    for (name, tmpl) in [
        ("TextEncoderStream", TextEncoderStream::install(scope)),
        ("TextDecoderStream", TextDecoderStream::install(scope)),
    ] {
        let class_fn = tmpl.get_function(scope).unwrap();
        let key = v8::String::new(scope, name).unwrap();
        global.set(scope, key.into(), class_fn.into());
    }

    let src_v8 = v8::String::new(scope, src).unwrap();
    let script = v8::Script::compile(scope, src_v8, None).unwrap();
    let result = script.run(scope).unwrap();
    scope.perform_microtask_checkpoint();
    f(result, scope)
}

/// Run a JS expression, drain microtasks, and return the awaited Promise's
/// JSON-stringified value as a Rust String. Tests that pipe through a
/// stream must `await` the consumer side — pipeTo / reader.read.
fn run_async_to_string(src: &str) -> String {
    init_v8();
    let mut isolate = v8::Isolate::new(v8::CreateParams::default());
    v8::scope!(let handle_scope, &mut isolate);
    let context = v8::Context::new(handle_scope, Default::default());
    let scope = &mut v8::ContextScope::new(handle_scope, context);

    let global = scope.get_current_context().global(scope);
    for (name, tmpl) in [
        ("TextEncoder", TextEncoder::install(scope)),
        ("TextDecoder", TextDecoder::install(scope)),
    ] {
        let class_fn = tmpl.get_function(scope).unwrap();
        let key = v8::String::new(scope, name).unwrap();
        global.set(scope, key.into(), class_fn.into());
    }
    install_native_streams(scope, global);
    for (name, tmpl) in [
        ("TextEncoderStream", TextEncoderStream::install(scope)),
        ("TextDecoderStream", TextDecoderStream::install(scope)),
    ] {
        let class_fn = tmpl.get_function(scope).unwrap();
        let key = v8::String::new(scope, name).unwrap();
        global.set(scope, key.into(), class_fn.into());
    }

    // Stash a slot that the JS sets after the Promise resolves; we
    // pump microtasks until it's filled (or 1024 iterations cap).
    let prelude = r#"
        globalThis.__result = null;
        globalThis.__error = null;
    "#;
    let s = v8::String::new(scope, prelude).unwrap();
    v8::Script::compile(scope, s, None).unwrap().run(scope).unwrap();

    // The caller passes a top-level expression that evaluates to a
    // Promise (typically a `(async () => …)()` IIFE — chain the
    // settle handlers directly, no extra IIFE wrapping. Extra layers
    // add microtask ticks the harness has no way to skip past, and
    // bury rejection errors a level deeper than `String(e.message)`
    // can describe.
    let wrapped = format!(
        r#"
        (function () {{
            const __p = ({src});
            __p.then(
                v => {{ globalThis.__result = JSON.stringify(v); }},
                e => {{ globalThis.__error = String((e && e.message) || e); }},
            );
        }})();
        "#
    );
    let s = v8::String::new(scope, &wrapped).unwrap();
    v8::Script::compile(scope, s, None).unwrap().run(scope).unwrap();

    for i in 0..2048 {
        scope.perform_microtask_checkpoint();
        let probe = v8::String::new(
            scope,
            "JSON.stringify([globalThis.__result, globalThis.__error])",
        )
        .unwrap();
        let v = v8::Script::compile(scope, probe, None)
            .unwrap()
            .run(scope)
            .unwrap();
        let s = v.to_rust_string_lossy(scope);
        if s.contains("null,null") {
            continue;
        }
        // Decode: ["resolved-string-or-null", "error-msg-or-null"].
        let (res, err) = parse_pair(&s);
        if let Some(e) = err {
            panic!("async test rejected (after {i} pumps): {e}");
        }
        return res.unwrap_or_else(|| "<no-result>".into());
    }
    panic!("async test never settled");
}

// ["a","b"] → (a, b) where each is None on JSON null.
fn parse_pair(s: &str) -> (Option<String>, Option<String>) {
    let v: serde_json::Value = serde_json::from_str(s).expect("pair JSON parse");
    let arr = v.as_array().expect("array");
    let res = arr[0].as_str().map(|s| s.to_string());
    let err = arr[1].as_str().map(|s| s.to_string());
    (res, err)
}

fn js_string(val: v8::Local<v8::Value>, scope: &mut v8::PinScope) -> String {
    val.to_rust_string_lossy(scope)
}

// ===========================================================================
// TextEncoderStream
// ===========================================================================

#[test]
fn encoder_stream_constructible() {
    let s = run_in_v8("typeof new TextEncoderStream()", |v, s| js_string(v, s));
    assert_eq!(s, "object");
}

#[test]
fn encoder_stream_encoding_is_utf8() {
    let s = run_in_v8("new TextEncoderStream().encoding", |v, s| js_string(v, s));
    assert_eq!(s, "utf-8");
}

#[test]
fn encoder_stream_to_string_tag() {
    let s = run_in_v8(
        "Object.prototype.toString.call(new TextEncoderStream())",
        |v, s| js_string(v, s),
    );
    assert_eq!(s, "[object TextEncoderStream]");
}

#[test]
fn encoder_stream_instanceof() {
    let s = run_in_v8(
        "new TextEncoderStream() instanceof TextEncoderStream",
        |v, s| js_string(v, s),
    );
    assert_eq!(s, "true");
}

#[test]
fn encoder_stream_readable_writable_are_streams() {
    let s = run_in_v8(
        r#"
        const tes = new TextEncoderStream();
        JSON.stringify({
            r: tes.readable instanceof ReadableStream,
            w: tes.writable instanceof WritableStream,
        });
        "#,
        |v, s| js_string(v, s),
    );
    assert_eq!(s, r#"{"r":true,"w":true}"#);
}

#[test]
fn encoder_stream_readable_writable_are_getters_not_own_props() {
    // Per WHATWG GenericTransformStream §6.1, `readable` and `writable`
    // are accessor properties on the prototype — not own data props.
    // The previous shim assigned `this.readable = ts.readable` in the
    // constructor body which made them own data props.
    let s = run_in_v8(
        r#"
        const tes = new TextEncoderStream();
        JSON.stringify({
            ownReadable: Object.prototype.hasOwnProperty.call(tes, "readable"),
            ownWritable: Object.prototype.hasOwnProperty.call(tes, "writable"),
            protoHasReadable:
                Object.getOwnPropertyDescriptor(
                    Object.getPrototypeOf(tes), "readable"
                ) !== undefined,
            protoHasWritable:
                Object.getOwnPropertyDescriptor(
                    Object.getPrototypeOf(tes), "writable"
                ) !== undefined,
        });
        "#,
        |v, s| js_string(v, s),
    );
    assert_eq!(
        s,
        r#"{"ownReadable":false,"ownWritable":false,"protoHasReadable":true,"protoHasWritable":true}"#,
    );
}

#[test]
fn encoder_stream_pipe_single_string() {
    // Concurrent producer / consumer: backpressure on the writable
    // side blocks `writer.write` (HWM defaults to 1) until the reader
    // drains. The polyfill behaved the same way; sequential
    // `await writer.write(); await reader.read()` deadlocks per
    // WHATWG Streams §3.2 (`WritableStreamDefaultWriterWrite` resolves
    // when chunkPromise settles).
    let s = run_async_to_string(
        r#"
        (async () => {
            const tes = new TextEncoderStream();
            const writer = tes.writable.getWriter();
            const reader = tes.readable.getReader();
            const writes = (async () => {
                await writer.write("hello");
                await writer.close();
            })();
            const out = [];
            const reads = (async () => {
                while (true) {
                    const { value, done } = await reader.read();
                    if (done) break;
                    for (const b of value) out.push(b);
                }
            })();
            await Promise.all([writes, reads]);
            return out.join(",");
        })()
        "#,
    );
    // "hello" → bytes 104,101,108,108,111
    assert_eq!(s, r#""104,101,108,108,111""#);
}

#[test]
fn encoder_stream_pipe_multiple_chunks() {
    let s = run_async_to_string(
        r#"
        (async () => {
            const tes = new TextEncoderStream();
            const writer = tes.writable.getWriter();
            const reader = tes.readable.getReader();
            const writes = (async () => {
                await writer.write("hi ");
                await writer.write("there");
                await writer.close();
            })();
            const chunks = [];
            const reads = (async () => {
                while (true) {
                    const { value, done } = await reader.read();
                    if (done) break;
                    chunks.push(Array.from(value).join(","));
                }
            })();
            await Promise.all([writes, reads]);
            return chunks.join("|");
        })()
        "#,
    );
    // "hi " → 104,105,32 ; "there" → 116,104,101,114,101
    assert_eq!(s, r#""104,105,32|116,104,101,114,101""#);
}

#[test]
fn encoder_stream_brand_check_on_encoding() {
    // The shim's `encoding` getter had no brand check — applied to a
    // spoofed object it returned "utf-8". The native getter throws
    // TypeError on illegal-receiver per WebIDL.
    let s = run_in_v8(
        r#"
        const desc = Object.getOwnPropertyDescriptor(
            TextEncoderStream.prototype, "encoding"
        );
        let threw = false;
        try { desc.get.call({}); } catch (_e) { threw = true; }
        threw;
        "#,
        |v, s| js_string(v, s),
    );
    assert_eq!(s, "true");
}

// ===========================================================================
// TextDecoderStream
// ===========================================================================

#[test]
fn decoder_stream_default_label() {
    let s = run_in_v8("new TextDecoderStream().encoding", |v, s| js_string(v, s));
    assert_eq!(s, "utf-8");
}

#[test]
fn decoder_stream_label_and_options() {
    let s = run_in_v8(
        r#"
        const tds = new TextDecoderStream("utf-8", { fatal: true, ignoreBOM: true });
        JSON.stringify({
            encoding: tds.encoding,
            fatal: tds.fatal,
            ignoreBOM: tds.ignoreBOM,
        });
        "#,
        |v, s| js_string(v, s),
    );
    assert_eq!(
        s,
        r#"{"encoding":"utf-8","fatal":true,"ignoreBOM":true}"#,
    );
}

#[test]
fn decoder_stream_invalid_label_throws_range_error() {
    let s = run_in_v8(
        r#"
        let kind = "<no-throw>";
        try {
            new TextDecoderStream("not-a-real-encoding");
        } catch (e) {
            kind = e.constructor.name;
        }
        kind;
        "#,
        |v, s| js_string(v, s),
    );
    assert_eq!(s, "RangeError");
}

#[test]
fn decoder_stream_to_string_tag() {
    let s = run_in_v8(
        "Object.prototype.toString.call(new TextDecoderStream())",
        |v, s| js_string(v, s),
    );
    assert_eq!(s, "[object TextDecoderStream]");
}

#[test]
fn decoder_stream_pipe_round_trip() {
    let s = run_async_to_string(
        r#"
        (async () => {
            const tds = new TextDecoderStream();
            const writer = tds.writable.getWriter();
            const reader = tds.readable.getReader();
            const bytes = new Uint8Array([104,105,32,116,104,101,114,101]); // "hi there"
            const writes = (async () => {
                await writer.write(bytes);
                await writer.close();
            })();
            const parts = [];
            const reads = (async () => {
                while (true) {
                    const { value, done } = await reader.read();
                    if (done) break;
                    parts.push(value);
                }
            })();
            await Promise.all([writes, reads]);
            return parts.join("");
        })()
        "#,
    );
    assert_eq!(s, r#""hi there""#);
}

#[test]
fn decoder_stream_split_multi_byte_utf8() {
    // The headline AI-SDK / SSE bug: a 3-byte CJK codepoint split
    // across chunks. Without `{ stream: true }`, the first chunk's
    // partial bytes get replaced with U+FFFD and the SSE parser sees
    // garbage. The TDS must preserve decoder state.
    //
    // U+4E2D ("中") = E4 B8 AD in UTF-8.
    let s = run_async_to_string(
        r#"
        (async () => {
            const tds = new TextDecoderStream();
            const writer = tds.writable.getWriter();
            const reader = tds.readable.getReader();
            const writes = (async () => {
                await writer.write(new Uint8Array([0xE4]));         // first byte
                await writer.write(new Uint8Array([0xB8, 0xAD]));   // remaining two
                await writer.close();
            })();
            let acc = "";
            const reads = (async () => {
                while (true) {
                    const { value, done } = await reader.read();
                    if (done) break;
                    acc += value;
                }
            })();
            await Promise.all([writes, reads]);
            // Spec-compliant streaming: result is exactly "中".
            return acc;
        })()
        "#,
    );
    assert_eq!(s, r#""中""#);
}

#[test]
fn decoder_stream_tail_replacement_on_close() {
    // Default (non-fatal) mode: an incomplete byte sequence at end-
    // of-stream surfaces as U+FFFD on flush. WHATWG §4.2 the
    // decoder's [[handler]] picks "replacement" when an "error" with
    // pending bytes hits flush-mode.
    let s = run_async_to_string(
        r#"
        (async () => {
            const tds = new TextDecoderStream();
            const writer = tds.writable.getWriter();
            const reader = tds.readable.getReader();
            const writes = (async () => {
                await writer.write(new Uint8Array([0xE4])); // dangling first byte
                await writer.close();
            })();
            let acc = "";
            const reads = (async () => {
                while (true) {
                    const { value, done } = await reader.read();
                    if (done) break;
                    acc += value;
                }
            })();
            await Promise.all([writes, reads]);
            // Codepoints: U+FFFD (\uFFFD) → JSON-encoded "\\ufffd".
            return acc;
        })()
        "#,
    );
    assert_eq!(s, r#""\ufffd""#);
}

#[test]
fn decoder_stream_brand_check_on_encoding() {
    let s = run_in_v8(
        r#"
        const desc = Object.getOwnPropertyDescriptor(
            TextDecoderStream.prototype, "encoding"
        );
        let threw = false;
        try { desc.get.call({}); } catch (_e) { threw = true; }
        threw;
        "#,
        |v, s| js_string(v, s),
    );
    assert_eq!(s, "true");
}

#[test]
fn decoder_stream_readable_writable_are_streams() {
    let s = run_in_v8(
        r#"
        const tds = new TextDecoderStream();
        JSON.stringify({
            r: tds.readable instanceof ReadableStream,
            w: tds.writable instanceof WritableStream,
        });
        "#,
        |v, s| js_string(v, s),
    );
    assert_eq!(s, r#"{"r":true,"w":true}"#);
}

#[test]
fn decoder_stream_readable_writable_are_getters_not_own_props() {
    let s = run_in_v8(
        r#"
        const tds = new TextDecoderStream();
        JSON.stringify({
            ownReadable: Object.prototype.hasOwnProperty.call(tds, "readable"),
            ownWritable: Object.prototype.hasOwnProperty.call(tds, "writable"),
            protoHasReadable:
                Object.getOwnPropertyDescriptor(
                    Object.getPrototypeOf(tds), "readable"
                ) !== undefined,
            protoHasWritable:
                Object.getOwnPropertyDescriptor(
                    Object.getPrototypeOf(tds), "writable"
                ) !== undefined,
        });
        "#,
        |v, s| js_string(v, s),
    );
    assert_eq!(
        s,
        r#"{"ownReadable":false,"ownWritable":false,"protoHasReadable":true,"protoHasWritable":true}"#,
    );
}

#[test]
fn decoder_stream_instanceof() {
    let s = run_in_v8(
        "new TextDecoderStream() instanceof TextDecoderStream",
        |v, s| js_string(v, s),
    );
    assert_eq!(s, "true");
}
