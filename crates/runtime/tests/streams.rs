#![allow(unsafe_code)]

mod common;
use common::*;

#[test]
fn readable_stream_sync_enqueue() {
    let r = dispatch(m(r#"
        export async function test() {
            var stream = new ReadableStream({
                start(controller) {
                    controller.enqueue("hello ");
                    controller.enqueue("world");
                    controller.close();
                }
            });
            var reader = stream.getReader();
            var text = "";
            while (true) {
                var r = await reader.read();
                if (r.done) break;
                text += new TextDecoder().decode(r.value);
            }
            return text;
        }
    "#), "test", "[]").unwrap();
    assert!(r.json.contains("hello world"), "got: {}", r.json);
}

#[test]
fn readable_stream_response_text_method() {
    let r = dispatch(m(r#"
        export async function test() {
            var stream = new ReadableStream({
                start(controller) {
                    controller.enqueue("abc");
                    controller.enqueue("def");
                    controller.close();
                }
            });
            var resp = new Response(stream);
            var text = await resp.text();
            return text;
        }
    "#), "test", "[]").unwrap();
    assert!(r.json.contains("abcdef"), "got: {}", r.json);
}

// ===========================================================================
// Native QueuingStrategy classes — ByteLengthQueuingStrategy / CountQueuingStrategy
// ---------------------------------------------------------------------------
// These tests bypass the dispatch wrapper and install the strategy classes
// directly on a fresh isolate. They verify the §6.2 / §6.3 IDL surface
// without needing the rest of the streams infrastructure (ReadableStream
// constructor, etc.) to be wired up.
//
// WPT coverage: queuing-strategies.any.js subset; the per-realm shared-size
// invariant from queuing-strategies-size-function-per-global.window.js.
// ===========================================================================

use zeroship_runtime::init_v8;
use zeroship_runtime::streams::strategies::{
    install_byte_length_queuing_strategy, install_count_queuing_strategy,
};

fn run_with_strategies<R>(
    src: &str,
    f: impl FnOnce(v8::Local<v8::Value>, &mut v8::PinScope) -> R,
) -> R {
    init_v8();
    let mut isolate = v8::Isolate::new(v8::CreateParams::default());
    v8::scope!(let handle_scope, &mut isolate);
    let context = v8::Context::new(handle_scope, Default::default());
    let scope = &mut v8::ContextScope::new(handle_scope, context);
    let global = scope.get_current_context().global(scope);

    install_byte_length_queuing_strategy(scope, global);
    install_count_queuing_strategy(scope, global);

    let src_v8 = v8::String::new(scope, src).unwrap();
    let script = v8::Script::compile(scope, src_v8, None).unwrap();
    let result = script.run(scope).unwrap();
    f(result, scope)
}

#[test]
fn byte_length_strategy_constructs_and_exposes_hwm() {
    let r = run_with_strategies(
        r#"
        const s = new ByteLengthQueuingStrategy({ highWaterMark: 1024 });
        s.highWaterMark;
        "#,
        |val, scope| val.number_value(scope).unwrap(),
    );
    assert_eq!(r, 1024.0);
}

#[test]
fn byte_length_strategy_size_returns_byte_length_for_uint8array() {
    let r = run_with_strategies(
        r#"
        const s = new ByteLengthQueuingStrategy({ highWaterMark: 100 });
        const buf = new Uint8Array(7);
        s.size(buf);
        "#,
        |val, scope| val.number_value(scope).unwrap(),
    );
    assert_eq!(r, 7.0);
}

#[test]
fn byte_length_strategy_size_returns_byte_length_for_arraybuffer() {
    let r = run_with_strategies(
        r#"
        const s = new ByteLengthQueuingStrategy({ highWaterMark: 100 });
        const buf = new ArrayBuffer(13);
        s.size(buf);
        "#,
        |val, scope| val.number_value(scope).unwrap(),
    );
    assert_eq!(r, 13.0);
}

#[test]
fn byte_length_strategy_size_returns_undefined_for_plain_object() {
    // Spec ByteLengthQueuingStrategy.size is literally `return chunk.byteLength`.
    // For a plain object with no `byteLength` property, this is `undefined`.
    let r = run_with_strategies(
        r#"
        const s = new ByteLengthQueuingStrategy({ highWaterMark: 100 });
        const v = s.size({});
        v === undefined ? "undefined" : (Number.isNaN(v) ? "NaN" : String(v));
        "#,
        |val, scope| val.to_rust_string_lossy(scope),
    );
    assert_eq!(r, "undefined");
}

#[test]
fn byte_length_strategy_throws_when_init_missing() {
    let r = run_with_strategies(
        r#"
        let kind;
        try { new ByteLengthQueuingStrategy(); }
        catch (e) { kind = e.constructor.name; }
        kind || "no-throw";
        "#,
        |val, scope| val.to_rust_string_lossy(scope),
    );
    assert_eq!(r, "TypeError");
}

#[test]
fn byte_length_strategy_throws_when_hwm_missing() {
    let r = run_with_strategies(
        r#"
        let kind;
        try { new ByteLengthQueuingStrategy({}); }
        catch (e) { kind = e.constructor.name; }
        kind || "no-throw";
        "#,
        |val, scope| val.to_rust_string_lossy(scope),
    );
    assert_eq!(r, "TypeError");
}

#[test]
fn byte_length_strategy_size_is_shared_per_realm() {
    // WPT queuing-strategies-size-function-per-global.window.js:
    // Object.is(a.size, b.size) === true within a realm.
    let r = run_with_strategies(
        r#"
        const a = new ByteLengthQueuingStrategy({ highWaterMark: 1 });
        const b = new ByteLengthQueuingStrategy({ highWaterMark: 2 });
        Object.is(a.size, b.size) ? "shared" : "different";
        "#,
        |val, scope| val.to_rust_string_lossy(scope),
    );
    assert_eq!(r, "shared");
}

#[test]
fn count_strategy_size_always_returns_one() {
    let r = run_with_strategies(
        r#"
        const s = new CountQueuingStrategy({ highWaterMark: 4 });
        const a = s.size("hello");
        const b = s.size({ x: 1 });
        const c = s.size(undefined);
        a === 1 && b === 1 && c === 1 ? "all-1" : `a=${a} b=${b} c=${c}`;
        "#,
        |val, scope| val.to_rust_string_lossy(scope),
    );
    assert_eq!(r, "all-1");
}

#[test]
fn count_strategy_exposes_hwm() {
    let r = run_with_strategies(
        r#"
        new CountQueuingStrategy({ highWaterMark: 4 }).highWaterMark;
        "#,
        |val, scope| val.number_value(scope).unwrap(),
    );
    assert_eq!(r, 4.0);
}

#[test]
fn count_strategy_size_is_shared_per_realm() {
    let r = run_with_strategies(
        r#"
        const a = new CountQueuingStrategy({ highWaterMark: 1 });
        const b = new CountQueuingStrategy({ highWaterMark: 2 });
        Object.is(a.size, b.size) ? "shared" : "different";
        "#,
        |val, scope| val.to_rust_string_lossy(scope),
    );
    assert_eq!(r, "shared");
}

#[test]
fn count_strategy_throws_when_init_missing() {
    let r = run_with_strategies(
        r#"
        let kind;
        try { new CountQueuingStrategy(); }
        catch (e) { kind = e.constructor.name; }
        kind || "no-throw";
        "#,
        |val, scope| val.to_rust_string_lossy(scope),
    );
    assert_eq!(r, "TypeError");
}

#[test]
fn byte_length_size_and_count_size_are_distinct_functions() {
    let r = run_with_strategies(
        r#"
        const bl = new ByteLengthQueuingStrategy({ highWaterMark: 1 });
        const cs = new CountQueuingStrategy({ highWaterMark: 1 });
        Object.is(bl.size, cs.size) ? "same" : "distinct";
        "#,
        |val, scope| val.to_rust_string_lossy(scope),
    );
    assert_eq!(r, "distinct");
}

#[test]
fn byte_length_strategy_has_to_string_tag() {
    let r = run_with_strategies(
        r#"
        const s = new ByteLengthQueuingStrategy({ highWaterMark: 1 });
        Object.prototype.toString.call(s);
        "#,
        |val, scope| val.to_rust_string_lossy(scope),
    );
    assert_eq!(r, "[object ByteLengthQueuingStrategy]");
}

#[test]
fn count_strategy_has_to_string_tag() {
    let r = run_with_strategies(
        r#"
        const s = new CountQueuingStrategy({ highWaterMark: 1 });
        Object.prototype.toString.call(s);
        "#,
        |val, scope| val.to_rust_string_lossy(scope),
    );
    assert_eq!(r, "[object CountQueuingStrategy]");
}
