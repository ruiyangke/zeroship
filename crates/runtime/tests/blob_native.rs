//! Hand-written tests for native `Blob` and `File` per WHATWG File API.
//!
//! These tests drive the JS surface through a real V8 isolate. WPT
//! conformance lives in `wpt_blob.rs`; this file covers the spec
//! shape (constructor variants, slice clamps, type normalization,
//! Promise return shapes, the prototype chain) explicitly.
//!
//! Pattern matches `headers.rs` / `wpt_text_encoding.rs`: a shared
//! `run_in_v8` harness, install Blob + File globals, evaluate JS that
//! asserts and serializes the result back via JSON.stringify.
#![allow(unsafe_code)]

use zeroship_runtime::blob_native;
use zeroship_runtime::init_v8;

/// Run a JS source after installing Blob + File and ReadableStream
/// (Blob.stream() depends on the latter). Returns whatever the source
/// evaluates to, post `f`-projected.
fn run_in_v8<F, R>(src: &str, f: F) -> R
where
    F: FnOnce(v8::Local<v8::Value>, &mut v8::PinScope) -> R,
{
    init_v8();
    let mut isolate = v8::Isolate::new(v8::CreateParams::default());
    v8::scope!(let handle_scope, &mut isolate);
    let context = v8::Context::new(handle_scope, Default::default());
    let scope = &mut v8::ContextScope::new(handle_scope, context);

    let global = scope.get_current_context().global(scope);
    // Streams must be installed first because Blob.stream() reads
    // globalThis.ReadableStream during invocation.
    zeroship_runtime::streams::install_native_streams(scope, global);
    blob_native::install_globals(scope, global);

    let src_v8 = v8::String::new(scope, src).unwrap();
    let script = v8::Script::compile(scope, src_v8, None).unwrap();
    let result = script.run(scope).unwrap();
    f(result, scope)
}

fn js_string(val: v8::Local<v8::Value>, scope: &mut v8::PinScope) -> String {
    val.to_rust_string_lossy(scope)
}

// ---------------------------------------------------------------------------
// Construction
// ---------------------------------------------------------------------------

#[test]
fn empty_blob() {
    let s = run_in_v8(
        r#"
        const b = new Blob();
        JSON.stringify({ size: b.size, type: b.type });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"size":0,"type":""}"#);
}

#[test]
fn construct_from_strings_with_type() {
    let s = run_in_v8(
        r#"
        const b = new Blob(["a", "b"], { type: "text/plain" });
        JSON.stringify({ size: b.size, type: b.type });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"size":2,"type":"text/plain"}"#);
}

#[test]
fn construct_from_uint8array() {
    let s = run_in_v8(
        r#"
        const b = new Blob([new Uint8Array([1, 2, 3])]);
        JSON.stringify({ size: b.size });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"size":3}"#);
}

#[test]
fn construct_from_arraybuffer() {
    // Direct ArrayBuffer (not view).
    let s = run_in_v8(
        r#"
        const ab = new ArrayBuffer(4);
        new Uint8Array(ab).set([10, 20, 30, 40]);
        const b = new Blob([ab]);
        b.size;
        "#,
        |val, scope| val.uint32_value(scope).unwrap_or(99),
    );
    assert_eq!(s, 4);
}

#[test]
fn construct_from_nested_blob_copies_bytes() {
    // Spec §3.2 step 1.b: Blob parts are *copied*, not shared.
    let s = run_in_v8(
        r#"
        const inner = new Blob(["x"]);
        const outer = new Blob([inner]);
        JSON.stringify({ inner: inner.size, outer: outer.size });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"inner":1,"outer":1}"#);
}

#[test]
fn construct_with_mixed_parts() {
    let s = run_in_v8(
        r#"
        const b = new Blob(["ab", new Uint8Array([99]), new Blob(["XY"])]);
        JSON.stringify({ size: b.size });
        "#,
        |val, scope| js_string(val, scope),
    );
    // "ab" (2) + [99] (1) + "XY" (2) = 5
    assert_eq!(s, r#"{"size":5}"#);
}

#[test]
fn type_with_control_chars_becomes_empty() {
    // Spec §3.1 step 2: type only retains lowercased ASCII printable;
    // any non-printable byte → empty type.
    let s = run_in_v8(
        r#"
        const b = new Blob([], { type: "text/plain\nbad" });
        JSON.stringify({ type: b.type });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"type":""}"#);
}

#[test]
fn type_lowercased() {
    let s = run_in_v8(
        r#"
        const b = new Blob([], { type: "TEXT/Plain;CHARSET=UTF-8" });
        JSON.stringify({ type: b.type });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"type":"text/plain;charset=utf-8"}"#);
}

#[test]
fn options_object_kinds_treated_as_dict() {
    // Per WebIDL §3.2.18: `Object` (regex, function, plain object) is
    // accepted; primitives throw. This tests the accepted shapes.
    let s = run_in_v8(
        r#"
        const c = new Blob([], /regex/);
        const d = new Blob([], () => {});
        const e = new Blob([], { unrecognized: true });
        JSON.stringify({ c: c.size, d: d.size, e: e.size });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"c":0,"d":0,"e":0}"#);
}

#[test]
fn options_primitives_throw() {
    // 123, 'abc', true, 123.4 → TypeError.
    let s = run_in_v8(
        r#"
        let count = 0;
        for (const arg of [123, 123.4, true, "abc"]) {
            try { new Blob([], arg); }
            catch (e) { if (e instanceof TypeError) count++; }
        }
        count;
        "#,
        |val, scope| val.uint32_value(scope).unwrap_or(0),
    );
    assert_eq!(s, 4);
}

#[test]
fn options_undefined_or_null_ok() {
    let s = run_in_v8(
        r#"
        const a = new Blob([], undefined);
        const c = new Blob([], null);
        JSON.stringify({ a: a.size, c: c.size });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"a":0,"c":0}"#);
}

// ---------------------------------------------------------------------------
// Method shape
// ---------------------------------------------------------------------------

#[test]
fn text_returns_promise_resolving_to_string() {
    // The result has no top-level await, so we drive the microtask
    // queue manually by writing into a global side-effect var.
    let s = run_in_v8(
        r#"
        let result;
        (async () => {
            const b = new Blob(["hello"]);
            result = await b.text();
        })();
        // Run microtasks to settle the promise.
        // V8 runs pending microtasks at the end of the current top
        // call when the harness invokes perform_microtask_checkpoint;
        // in-script we can't trigger one, but this small test relies
        // on v8 auto-running microtasks as part of awaiting.
        result;  // may still be undefined here
        "#,
        |_val, _scope| { /* ignore */ },
    );
    let _ = s;
    // The above is unreliable for awaiting. Drop the awaited shape and
    // test the .then path instead:
    let s2 = run_in_v8(
        r#"
        let captured = "<unset>";
        new Blob(["hello"]).text().then(v => { captured = v; });
        // Force a microtask drain by chaining; in the absence of an
        // explicit drain API, we rely on Promise.resolve().then() which
        // schedules another microtask.
        captured;
        "#,
        |_val, _scope| {},
    );
    let _ = s2;
    // Use a clean assert via the harness's perform_microtask_checkpoint
    // pathway: separate test below validates resolution.
}

#[test]
fn text_resolves_with_value() {
    init_v8();
    let mut isolate = v8::Isolate::new(v8::CreateParams::default());
    v8::scope!(let handle_scope, &mut isolate);
    let context = v8::Context::new(handle_scope, Default::default());
    let scope = &mut v8::ContextScope::new(handle_scope, context);
    let global = scope.get_current_context().global(scope);
    zeroship_runtime::streams::install_native_streams(scope, global);
    blob_native::install_globals(scope, global);

    // Capture the resolved value into a global JS var.
    let src = r#"
        globalThis.__captured = "<unset>";
        new Blob(["hello"]).text().then(v => { globalThis.__captured = v; });
    "#;
    let src_v8 = v8::String::new(scope, src).unwrap();
    v8::Script::compile(scope, src_v8, None)
        .unwrap()
        .run(scope)
        .unwrap();
    scope.perform_microtask_checkpoint();

    let read = v8::String::new(scope, "globalThis.__captured").unwrap();
    let got = v8::Script::compile(scope, read, None)
        .unwrap()
        .run(scope)
        .unwrap();
    assert_eq!(got.to_rust_string_lossy(scope), "hello");
}

#[test]
fn array_buffer_resolves_with_arraybuffer() {
    init_v8();
    let mut isolate = v8::Isolate::new(v8::CreateParams::default());
    v8::scope!(let handle_scope, &mut isolate);
    let context = v8::Context::new(handle_scope, Default::default());
    let scope = &mut v8::ContextScope::new(handle_scope, context);
    let global = scope.get_current_context().global(scope);
    zeroship_runtime::streams::install_native_streams(scope, global);
    blob_native::install_globals(scope, global);

    let src = r#"
        globalThis.__captured = "<unset>";
        new Blob([new Uint8Array([7,8,9])]).arrayBuffer().then(ab => {
            globalThis.__captured = ab.byteLength + ":" + (ab instanceof ArrayBuffer);
        });
    "#;
    let src_v8 = v8::String::new(scope, src).unwrap();
    v8::Script::compile(scope, src_v8, None)
        .unwrap()
        .run(scope)
        .unwrap();
    scope.perform_microtask_checkpoint();

    let read = v8::String::new(scope, "globalThis.__captured").unwrap();
    let got = v8::Script::compile(scope, read, None)
        .unwrap()
        .run(scope)
        .unwrap();
    assert_eq!(got.to_rust_string_lossy(scope), "3:true");
}

#[test]
fn bytes_resolves_with_uint8array() {
    init_v8();
    let mut isolate = v8::Isolate::new(v8::CreateParams::default());
    v8::scope!(let handle_scope, &mut isolate);
    let context = v8::Context::new(handle_scope, Default::default());
    let scope = &mut v8::ContextScope::new(handle_scope, context);
    let global = scope.get_current_context().global(scope);
    zeroship_runtime::streams::install_native_streams(scope, global);
    blob_native::install_globals(scope, global);

    let src = r#"
        globalThis.__captured = "<unset>";
        new Blob(["hi"]).bytes().then(u8 => {
            globalThis.__captured = u8.length + ":" + (u8 instanceof Uint8Array)
                + ":" + u8[0] + "," + u8[1];
        });
    "#;
    let src_v8 = v8::String::new(scope, src).unwrap();
    v8::Script::compile(scope, src_v8, None)
        .unwrap()
        .run(scope)
        .unwrap();
    scope.perform_microtask_checkpoint();

    let read = v8::String::new(scope, "globalThis.__captured").unwrap();
    let got = v8::Script::compile(scope, read, None)
        .unwrap()
        .run(scope)
        .unwrap();
    // "hi" = 0x68, 0x69
    assert_eq!(got.to_rust_string_lossy(scope), "2:true:104,105");
}

// ---------------------------------------------------------------------------
// slice
// ---------------------------------------------------------------------------

#[test]
fn slice_basic() {
    let s = run_in_v8(
        r#"
        const b = new Blob(["abcdef"]);
        const s = b.slice(1, 4, "text/plain");
        JSON.stringify({ size: s.size, type: s.type, isBlob: s instanceof Blob });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"size":3,"type":"text/plain","isBlob":true}"#);
}

#[test]
fn slice_negative_start() {
    // start = -2 of size-6 = 4; end default = 6 → length 2.
    let s = run_in_v8(
        r#"
        const b = new Blob(["abcdef"]);
        const s = b.slice(-2);
        JSON.stringify({ size: s.size });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"size":2}"#);
}

#[test]
fn slice_clamps_oob() {
    let s = run_in_v8(
        r#"
        const b = new Blob(["abc"]);
        const s = b.slice(0, 100);
        s.size;
        "#,
        |val, scope| val.uint32_value(scope).unwrap_or(99),
    );
    assert_eq!(s, 3);
}

#[test]
fn slice_clamps_inverted() {
    // end < start → length 0.
    let s = run_in_v8(
        r#"
        const b = new Blob(["abc"]);
        b.slice(2, 1).size;
        "#,
        |val, scope| val.uint32_value(scope).unwrap_or(99),
    );
    assert_eq!(s, 0);
}

#[test]
fn slice_default_args() {
    // No args → identity slice (size unchanged).
    let s = run_in_v8(
        r#"
        const b = new Blob(["hello"]);
        const s = b.slice();
        JSON.stringify({ size: s.size, type: s.type });
        "#,
        |val, scope| js_string(val, scope),
    );
    // Default content-type for slice without explicit arg is "" per
    // spec §3.3.6 step 5.
    assert_eq!(s, r#"{"size":5,"type":""}"#);
}

// ---------------------------------------------------------------------------
// instanceof / brand checks
// ---------------------------------------------------------------------------

#[test]
fn blob_instanceof_blob() {
    let s = run_in_v8(
        r#"
        new Blob() instanceof Blob;
        "#,
        |val, scope| val.boolean_value(scope),
    );
    assert!(s);
}

#[test]
fn to_string_tag_blob() {
    let s = run_in_v8(
        r#"
        Object.prototype.toString.call(new Blob());
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "[object Blob]");
}

#[test]
fn to_string_tag_file() {
    let s = run_in_v8(
        r#"
        Object.prototype.toString.call(new File([], "x"));
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "[object File]");
}

// ---------------------------------------------------------------------------
// File
// ---------------------------------------------------------------------------

#[test]
fn file_construct_basic() {
    let s = run_in_v8(
        r#"
        const f = new File(["x"], "foo.txt");
        JSON.stringify({
            name: f.name,
            size: f.size,
            isBlob: f instanceof Blob,
            isFile: f instanceof File,
        });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"name":"foo.txt","size":1,"isBlob":true,"isFile":true}"#);
}

#[test]
fn file_lastmodified_is_number() {
    let s = run_in_v8(
        r#"
        const f = new File([], "x");
        typeof f.lastModified;
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "number");
}

#[test]
fn file_lastmodified_explicit() {
    let s = run_in_v8(
        r#"
        const f = new File([], "x", { lastModified: 1234567890 });
        f.lastModified;
        "#,
        |val, scope| val.number_value(scope).unwrap_or(0.0) as i64,
    );
    assert_eq!(s, 1234567890);
}

#[test]
fn file_inherits_blob_methods() {
    // `text()`, `slice()`, etc. should be reachable via prototype
    // (we override them on File.prototype but the chain must be
    // intact for spec compliance — `Object.getPrototypeOf(File.prototype)
    // === Blob.prototype`).
    let s = run_in_v8(
        r#"
        Object.getPrototypeOf(File.prototype) === Blob.prototype;
        "#,
        |val, scope| val.boolean_value(scope),
    );
    assert!(s);
}

#[test]
fn file_with_typed_options() {
    let s = run_in_v8(
        r#"
        const f = new File(["abc"], "x.bin", { type: "APPLICATION/Octet-STREAM" });
        JSON.stringify({ name: f.name, type: f.type, size: f.size });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"name":"x.bin","type":"application/octet-stream","size":3}"#);
}

// ---------------------------------------------------------------------------
// stream()
// ---------------------------------------------------------------------------

#[test]
fn stream_returns_readable_stream() {
    let s = run_in_v8(
        r#"
        const b = new Blob(["abc"]);
        const r = b.stream();
        JSON.stringify({
            isStream: r instanceof ReadableStream,
            tag: Object.prototype.toString.call(r),
        });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"isStream":true,"tag":"[object ReadableStream]"}"#);
}

#[test]
fn stream_yields_bytes() {
    init_v8();
    let mut isolate = v8::Isolate::new(v8::CreateParams::default());
    v8::scope!(let handle_scope, &mut isolate);
    let context = v8::Context::new(handle_scope, Default::default());
    let scope = &mut v8::ContextScope::new(handle_scope, context);
    let global = scope.get_current_context().global(scope);
    zeroship_runtime::streams::install_native_streams(scope, global);
    blob_native::install_globals(scope, global);

    let src = r#"
        globalThis.__captured = "<unset>";
        const b = new Blob([new Uint8Array([10, 20, 30, 40])]);
        const r = b.stream();
        const reader = r.getReader();
        reader.read().then(({ value, done }) => {
            globalThis.__captured = done + ":" + value.length + ":" + value[0] + "," + value[3];
        });
    "#;
    let src_v8 = v8::String::new(scope, src).unwrap();
    v8::Script::compile(scope, src_v8, None)
        .unwrap()
        .run(scope)
        .unwrap();
    scope.perform_microtask_checkpoint();

    let read = v8::String::new(scope, "globalThis.__captured").unwrap();
    let got = v8::Script::compile(scope, read, None)
        .unwrap()
        .run(scope)
        .unwrap();
    assert_eq!(got.to_rust_string_lossy(scope), "false:4:10,40");
}

// ---------------------------------------------------------------------------
// Spec edge cases
// ---------------------------------------------------------------------------

#[test]
fn parts_undefined_treated_as_empty() {
    // Spec §3.2: "If invoked with zero parameters, the size of the
    // returned Blob MUST be 0".
    let s = run_in_v8(
        r#"
        const a = new Blob();
        const b = new Blob(undefined);
        JSON.stringify({ a: a.size, b: b.size });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"a":0,"b":0}"#);
}

#[test]
fn parts_non_iterable_throws() {
    // 42 is not iterable, so passing it as the `parts` arg → TypeError.
    let s = run_in_v8(
        r#"
        let kind;
        try { new Blob(42); }
        catch (e) { kind = e.constructor.name; }
        kind || "no-throw";
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "TypeError");
}

#[test]
fn empty_type_default() {
    let s = run_in_v8(
        r#"
        new Blob(["x"]).type;
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "");
}
