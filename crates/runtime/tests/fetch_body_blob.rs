//! Hand-written tests for native cross-class wiring between
//! `body.blob()` / `body.formData()` and the native Blob/File classes.
//!
//! These tests exercise the full path:
//!   - `new Response(string).blob()` → native Blob with default
//!     "text/plain;charset=UTF-8" type.
//!   - `new Response(bytes, { headers: { "content-type": "image/png" } })
//!     .blob()` → native Blob with "image/png" type.
//!   - FormData with Blob/File values via append/set/get/getAll.
//!   - `body.formData()` parsing of multipart/form-data into FormData
//!     with File entries for parts that have a filename.
//!   - `Blob.stream()` → ReadableStream end-to-end through fetch body.
//!
//! Pattern matches `fetch_body.rs`: install all native globals, run JS,
//! capture result via `globalThis.__result`.

#![allow(unsafe_code)]

use zeroship_runtime::blob_native;
use zeroship_runtime::dom;
use zeroship_runtime::fetch_request;
use zeroship_runtime::fetch_response;
use zeroship_runtime::headers;
use zeroship_runtime::init_v8;
use zeroship_runtime::streams;

fn run_in_v8<F, R>(src: &str, f: F) -> R
where
    F: FnOnce(v8::Local<v8::Value>, &mut v8::PinScope) -> R,
{
    init_v8();
    let mut isolate = v8::Isolate::new(v8::CreateParams::default());
    v8::scope!(let handle_scope, &mut isolate);
    let context = v8::Context::new(handle_scope, Default::default());
    let scope = &mut v8::ContextScope::new(handle_scope, context);

    install_globals(scope);

    let src_v8 = v8::String::new(scope, src).unwrap();
    let script = v8::Script::compile(scope, src_v8, None).unwrap();
    let result = script.run(scope).unwrap();
    scope.perform_microtask_checkpoint();
    f(result, scope)
}

fn run_async_in_v8(setup_src: &str) -> String {
    init_v8();
    let mut isolate = v8::Isolate::new(v8::CreateParams::default());
    v8::scope!(let handle_scope, &mut isolate);
    let context = v8::Context::new(handle_scope, Default::default());
    let scope = &mut v8::ContextScope::new(handle_scope, context);

    install_globals(scope);

    let src_v8 = v8::String::new(scope, setup_src).unwrap();
    let script = v8::Script::compile(scope, src_v8, None).unwrap();
    script.run(scope).unwrap();

    for _ in 0..32 {
        scope.perform_microtask_checkpoint();
    }

    let read_src = v8::String::new(
        scope,
        "typeof globalThis.__result === 'undefined' ? '<undefined>' : String(globalThis.__result)",
    )
    .unwrap();
    let read_script = v8::Script::compile(scope, read_src, None).unwrap();
    let result = read_script.run(scope).unwrap();
    result.to_rust_string_lossy(scope)
}

fn install_globals(scope: &mut v8::PinScope) {
    let global = scope.get_current_context().global(scope);
    streams::install_native_streams(scope, global);
    streams::strategies::install_byte_length_queuing_strategy(scope, global);
    streams::strategies::install_count_queuing_strategy(scope, global);
    headers::install_global(scope, global);
    dom::install_globals(scope, global);
    blob_native::install_globals(scope, global);
    fetch_request::install_global(scope, global);
    fetch_response::install_global(scope, global);
}

fn js_string(val: v8::Local<v8::Value>, scope: &mut v8::PinScope) -> String {
    val.to_rust_string_lossy(scope)
}

// ---------------------------------------------------------------------------
// body.blob() — basic shapes
// ---------------------------------------------------------------------------

#[test]
fn response_text_blob_returns_native_blob_with_default_type() {
    // Per Fetch §3.5: blob() returns a Blob whose type is the
    // Content-Type from headers, or empty string if absent. A string
    // body sets Content-Type to "text/plain;charset=UTF-8" (Fetch §3.2
    // step 11.7).
    let s = run_async_in_v8(
        r#"
        const r = new Response("hello");
        r.blob().then(b => {
            globalThis.__result = JSON.stringify({
                isBlob: b instanceof Blob,
                size: b.size,
                type: b.type,
            });
        }).catch(e => {
            globalThis.__result = "ERR:" + e.message;
        });
        "#,
    );
    assert_eq!(
        s,
        r#"{"isBlob":true,"size":5,"type":"text/plain;charset=utf-8"}"#
    );
}

#[test]
fn response_with_image_content_type_blob_has_that_type() {
    // Explicit Content-Type → Blob.type matches.
    let s = run_async_in_v8(
        r#"
        const r = new Response("xxx", { headers: { "content-type": "image/png" } });
        r.blob().then(b => {
            globalThis.__result = JSON.stringify({
                size: b.size,
                type: b.type,
            });
        }).catch(e => { globalThis.__result = "ERR:" + e.message; });
        "#,
    );
    assert_eq!(s, r#"{"size":3,"type":"image/png"}"#);
}

#[test]
fn response_blob_marks_body_used() {
    // Per Fetch §3.5 step 1: blob() (like every consumer) must disturb
    // the body. After the call, bodyUsed === true.
    let s = run_async_in_v8(
        r#"
        const r = new Response("hello");
        r.blob().then(b => {
            globalThis.__result = JSON.stringify({
                bodyUsed: r.bodyUsed,
                size: b.size,
            });
        }).catch(e => { globalThis.__result = "ERR:" + e.message; });
        "#,
    );
    assert_eq!(s, r#"{"bodyUsed":true,"size":5}"#);
}

#[test]
fn response_blob_round_trip_text() {
    // Blob round-trip: blob().text() should give back the original.
    let s = run_async_in_v8(
        r#"
        const r = new Response("Hello, world!");
        r.blob().then(b => b.text()).then(t => {
            globalThis.__result = t;
        }).catch(e => { globalThis.__result = "ERR:" + e.message; });
        "#,
    );
    assert_eq!(s, "Hello, world!");
}

#[test]
fn response_blob_empty_body() {
    // Empty body → empty Blob.
    let s = run_async_in_v8(
        r#"
        const r = new Response();
        r.blob().then(b => {
            globalThis.__result = JSON.stringify({
                isBlob: b instanceof Blob,
                size: b.size,
                type: b.type,
            });
        }).catch(e => { globalThis.__result = "ERR:" + e.message; });
        "#,
    );
    assert_eq!(s, r#"{"isBlob":true,"size":0,"type":""}"#);
}

#[test]
fn response_blob_preserves_bytes() {
    // Round-trip a binary body through blob(). Bytes should be exact.
    let s = run_async_in_v8(
        r#"
        const u8 = new Uint8Array([0, 1, 2, 254, 255]);
        const r = new Response(u8);
        r.blob().then(b => b.arrayBuffer()).then(ab => {
            const out = new Uint8Array(ab);
            globalThis.__result = JSON.stringify({
                len: out.length,
                bytes: Array.from(out),
            });
        }).catch(e => { globalThis.__result = "ERR:" + e.message; });
        "#,
    );
    assert_eq!(s, r#"{"len":5,"bytes":[0,1,2,254,255]}"#);
}

#[test]
fn second_blob_call_rejects_with_type_error() {
    // Body can be consumed only once; a second blob() must reject with
    // TypeError per Fetch §3.5 (bodyUsed check).
    let s = run_async_in_v8(
        r#"
        const r = new Response("hi");
        r.blob().then(_ => r.blob()).then(
          () => { globalThis.__result = "UNEXPECTED_OK"; },
          e => { globalThis.__result = JSON.stringify({
            isTE: e instanceof TypeError,
            name: e && e.name,
          }); },
        );
        "#,
    );
    assert_eq!(s, r#"{"isTE":true,"name":"TypeError"}"#);
}

// ---------------------------------------------------------------------------
// FormData with Blob/File values
// ---------------------------------------------------------------------------

#[test]
fn form_data_append_blob_returns_file() {
    // Per WHATWG XHR §5: append(name, Blob) wraps the Blob as a File
    // with name="blob" and lastModified=0 (or current time per the
    // spec — Chrome uses 0; we use current time for consistency with
    // File constructor default).
    let s = run_in_v8(
        r#"
        const fd = new FormData();
        fd.append("k", new Blob(["x"]));
        const v = fd.get("k");
        JSON.stringify({
            isFile: v instanceof File,
            isBlob: v instanceof Blob,
            name: v.name,
            size: v.size,
            type: v.type,
        });
        "#,
        js_string,
    );
    assert_eq!(
        s,
        r#"{"isFile":true,"isBlob":true,"name":"blob","size":1,"type":""}"#
    );
}

#[test]
fn form_data_append_blob_with_filename() {
    // append(name, Blob, filename) → File with the supplied name.
    let s = run_in_v8(
        r#"
        const fd = new FormData();
        fd.append("k", new Blob(["x"]), "f.txt");
        const v = fd.get("k");
        JSON.stringify({
            isFile: v instanceof File,
            name: v.name,
            size: v.size,
        });
        "#,
        js_string,
    );
    assert_eq!(s, r#"{"isFile":true,"name":"f.txt","size":1}"#);
}

#[test]
fn form_data_append_file_preserves_identity() {
    // append(name, File) keeps the File as-is (not re-wrapped). Per
    // spec: if value is a File, the entry is the File itself.
    // We can't compare object identity through toString, but `name`
    // and `lastModified` should match the File's, not be reset.
    let s = run_in_v8(
        r#"
        const f = new File(["abc"], "y.txt", { type: "text/x", lastModified: 12345 });
        const fd = new FormData();
        fd.append("k", f);
        const v = fd.get("k");
        JSON.stringify({
            isFile: v instanceof File,
            name: v.name,
            type: v.type,
            lastModified: v.lastModified,
            sameRef: v === f,
        });
        "#,
        js_string,
    );
    assert_eq!(
        s,
        r#"{"isFile":true,"name":"y.txt","type":"text/x","lastModified":12345,"sameRef":true}"#
    );
}

#[test]
fn form_data_set_blob_works_like_append_then_replace() {
    // set(name, blob) acts like spec list-set: replaces existing entry.
    let s = run_in_v8(
        r#"
        const fd = new FormData();
        fd.append("k", new Blob(["a"]));
        fd.set("k", new Blob(["bb"]), "f");
        const v = fd.get("k");
        const all = fd.getAll("k");
        JSON.stringify({
            count: all.length,
            isFile: v instanceof File,
            name: v.name,
            size: v.size,
        });
        "#,
        js_string,
    );
    assert_eq!(s, r#"{"count":1,"isFile":true,"name":"f","size":2}"#);
}

#[test]
fn form_data_get_all_mixed_string_and_blob() {
    // getAll returns mixed string + File entries in insertion order.
    let s = run_in_v8(
        r#"
        const fd = new FormData();
        fd.append("k", "s1");
        fd.append("k", new Blob(["b1"]), "fb.txt");
        fd.append("k", "s2");
        const all = fd.getAll("k");
        JSON.stringify({
            count: all.length,
            kinds: all.map(v => typeof v === "string" ? "str" : (v instanceof File ? "file" : "?")),
            v0: all[0],
            v1Name: all[1].name,
            v1Size: all[1].size,
            v2: all[2],
        });
        "#,
        js_string,
    );
    assert_eq!(
        s,
        r#"{"count":3,"kinds":["str","file","str"],"v0":"s1","v1Name":"fb.txt","v1Size":2,"v2":"s2"}"#
    );
}

#[test]
fn form_data_iter_yields_correct_value_type() {
    // Iteration: string entries yield strings, blob entries yield File.
    let s = run_in_v8(
        r#"
        const fd = new FormData();
        fd.append("a", "1");
        fd.append("b", new Blob(["x"]), "bb");
        const out = [];
        for (const [k, v] of fd) {
            out.push({ k, kind: typeof v === "string" ? "str" : (v instanceof File ? "file" : "?"), val: typeof v === "string" ? v : v.name });
        }
        JSON.stringify(out);
        "#,
        js_string,
    );
    assert_eq!(
        s,
        r#"[{"k":"a","kind":"str","val":"1"},{"k":"b","kind":"file","val":"bb"}]"#
    );
}

#[test]
fn form_data_blob_string_coercion_for_third_arg() {
    // The third arg to append/set is the filename — it's USVString,
    // which means a number, etc. ToString-coerces.
    let s = run_in_v8(
        r#"
        const fd = new FormData();
        fd.append("k", new Blob(["x"]), 42);
        const v = fd.get("k");
        JSON.stringify({ name: v.name });
        "#,
        js_string,
    );
    assert_eq!(s, r#"{"name":"42"}"#);
}

// ---------------------------------------------------------------------------
// body.formData() multipart parsing
// ---------------------------------------------------------------------------

#[test]
fn body_form_data_multipart_round_trip_text_only() {
    // Round-trip: build a FormData with text entries, send through a
    // Response, parse back via body.formData() → entries match.
    let s = run_async_in_v8(
        r#"
        const fd = new FormData();
        fd.append("a", "1");
        fd.append("b", "hello world");
        const r = new Response(fd);
        r.formData().then(parsed => {
            globalThis.__result = JSON.stringify({
                a: parsed.get("a"),
                b: parsed.get("b"),
                count: Array.from(parsed.entries()).length,
            });
        }).catch(e => { globalThis.__result = "ERR:" + e.message; });
        "#,
    );
    assert_eq!(s, r#"{"a":"1","b":"hello world","count":2}"#);
}

#[test]
fn body_form_data_multipart_round_trip_with_blob_part() {
    // FormData with a Blob entry — round-trips through multipart.
    // The parsed entry comes back as a File (filename defaults to
    // "blob" or whatever was supplied at append-time).
    let s = run_async_in_v8(
        r#"
        const fd = new FormData();
        fd.append("k", "s");
        fd.append("file", new Blob(["binary"]), "data.bin");
        const r = new Response(fd);
        r.formData().then(parsed => {
            const f = parsed.get("file");
            globalThis.__result = JSON.stringify({
                k: parsed.get("k"),
                isFile: f instanceof File,
                name: f.name,
                size: f.size,
            });
        }).catch(e => { globalThis.__result = "ERR:" + e.message; });
        "#,
    );
    assert_eq!(
        s,
        r#"{"k":"s","isFile":true,"name":"data.bin","size":6}"#
    );
}

#[test]
fn body_form_data_multipart_file_preserves_bytes() {
    // The raw bytes of a File part must round-trip exactly through
    // multipart serialize → parse.
    let s = run_async_in_v8(
        r#"
        const u8 = new Uint8Array([0, 1, 2, 200, 254, 255]);
        const fd = new FormData();
        fd.append("file", new Blob([u8]), "binary.bin");
        const r = new Response(fd);
        r.formData().then(parsed => parsed.get("file").arrayBuffer()).then(ab => {
            const out = new Uint8Array(ab);
            globalThis.__result = JSON.stringify({
                len: out.length,
                bytes: Array.from(out),
            });
        }).catch(e => { globalThis.__result = "ERR:" + e.message; });
        "#,
    );
    assert_eq!(s, r#"{"len":6,"bytes":[0,1,2,200,254,255]}"#);
}

#[test]
fn body_form_data_multiple_text_parts() {
    // Multiple text entries, including duplicates with same name.
    let s = run_async_in_v8(
        r#"
        const fd = new FormData();
        fd.append("name", "Alice");
        fd.append("age", "30");
        fd.append("hobby", "reading");
        fd.append("hobby", "biking");
        const r = new Response(fd);
        r.formData().then(parsed => {
            globalThis.__result = JSON.stringify({
                name: parsed.get("name"),
                age: parsed.get("age"),
                hobbies: parsed.getAll("hobby"),
            });
        }).catch(e => { globalThis.__result = "ERR:" + e.message; });
        "#,
    );
    assert_eq!(
        s,
        r#"{"name":"Alice","age":"30","hobbies":["reading","biking"]}"#
    );
}

#[test]
fn body_form_data_url_encoded_still_works() {
    // Verify the urlencoded path didn't regress.
    let s = run_async_in_v8(
        r#"
        const r = new Response("a=1&b=hello%20world", { headers: { "content-type": "application/x-www-form-urlencoded" } });
        r.formData().then(parsed => {
            globalThis.__result = JSON.stringify({
                a: parsed.get("a"),
                b: parsed.get("b"),
            });
        }).catch(e => { globalThis.__result = "ERR:" + e.message; });
        "#,
    );
    assert_eq!(s, r#"{"a":"1","b":"hello world"}"#);
}

// ---------------------------------------------------------------------------
// Cross-class instanceof verification
// ---------------------------------------------------------------------------

#[test]
fn file_is_instance_of_blob() {
    // Per File API §4: File inherits Blob, so instanceof both.
    let s = run_in_v8(
        r#"
        const f = new File(["x"], "y");
        JSON.stringify({
            isFile: f instanceof File,
            isBlob: f instanceof Blob,
        });
        "#,
        js_string,
    );
    assert_eq!(s, r#"{"isFile":true,"isBlob":true}"#);
}

#[test]
fn headers_is_instance_of_headers() {
    // Sanity check.
    let s = run_in_v8(
        r#"
        const h = new Headers();
        JSON.stringify({
            isHeaders: h instanceof Headers,
        });
        "#,
        js_string,
    );
    assert_eq!(s, r#"{"isHeaders":true}"#);
}

#[test]
fn form_data_is_instance_of_form_data() {
    let s = run_in_v8(
        r#"
        const fd = new FormData();
        JSON.stringify({
            isFD: fd instanceof FormData,
        });
        "#,
        js_string,
    );
    assert_eq!(s, r#"{"isFD":true}"#);
}

// ---------------------------------------------------------------------------
// Blob.stream() end-to-end
// ---------------------------------------------------------------------------

#[test]
fn blob_stream_async_iter_yields_bytes() {
    // Blob.stream() returns a native ReadableStream. Async iteration
    // over it yields Uint8Array chunks.
    let s = run_async_in_v8(
        r#"
        const b = new Blob(["hello world"]);
        (async () => {
            const stream = b.stream();
            const chunks = [];
            for await (const c of stream) {
                chunks.push(Array.from(c));
            }
            globalThis.__result = JSON.stringify(chunks);
        })();
        "#,
    );
    // "hello world" → bytes [104,101,108,108,111,32,119,111,114,108,100]
    assert_eq!(
        s,
        r#"[[104,101,108,108,111,32,119,111,114,108,100]]"#
    );
}

#[test]
fn response_from_blob_text_path() {
    // Body extracts a Blob, exposes its bytes through native streams,
    // decodes via native TextDecoder. Tests Blob → streams → fetch end
    // to end.
    let s = run_async_in_v8(
        r#"
        const b = new Blob(["greetings"], { type: "text/plain" });
        const r = new Response(b);
        r.text().then(t => {
            globalThis.__result = t;
        }).catch(e => { globalThis.__result = "ERR:" + e.message; });
        "#,
    );
    assert_eq!(s, "greetings");
}

#[test]
fn response_blob_inherits_blob_type_when_no_other_ct() {
    // Per Fetch §3.2 step 11.3: a Blob body's Content-Type defaults
    // to the Blob's `type`. So Response from Blob inherits that and
    // .blob() reads it back.
    let s = run_async_in_v8(
        r#"
        const b = new Blob(["x"], { type: "image/png" });
        const r = new Response(b);
        r.blob().then(b2 => {
            globalThis.__result = JSON.stringify({ type: b2.type, size: b2.size });
        }).catch(e => { globalThis.__result = "ERR:" + e.message; });
        "#,
    );
    assert_eq!(s, r#"{"type":"image/png","size":1}"#);
}
