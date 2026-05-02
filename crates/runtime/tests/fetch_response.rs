//! Hand-written tests for the native `Response` class
//! (`fetch_response::install_global`).
//!
//! These exercise the Fetch §5.5 constructor + getters + clone() +
//! body consumers + static methods (Response.error, Response.redirect,
//! Response.json). WPT conformance lives in `wpt_fetch_response.rs`;
//! this file covers the design's CRITICAL list and behavioural shape.

#![allow(unsafe_code)]

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
    fetch_request::install_global(scope, global);
    fetch_response::install_global(scope, global);
}

fn js_string(val: v8::Local<v8::Value>, scope: &mut v8::PinScope) -> String {
    val.to_rust_string_lossy(scope)
}

// ---------------------------------------------------------------------------
// Default construction
// ---------------------------------------------------------------------------

#[test]
fn default_construct_yields_200_default_type() {
    let s = run_in_v8(
        r#"
        const r = new Response();
        JSON.stringify({
            tag: r.constructor.name,
            symTag: r[Symbol.toStringTag],
            type: r.type,
            status: r.status,
            statusText: r.statusText,
            ok: r.ok,
            body: r.body,
        });
        "#,
        js_string,
    );
    assert_eq!(
        s,
        r#"{"tag":"Response","symTag":"Response","type":"default","status":200,"statusText":"","ok":true,"body":null}"#
    );
}

// ---------------------------------------------------------------------------
// status / ok
// ---------------------------------------------------------------------------

#[test]
fn ok_true_for_status_200_to_299() {
    let s = run_in_v8(
        r#"
        const r1 = new Response(null, { status: 200 });
        const r2 = new Response(null, { status: 250 });
        const r3 = new Response(null, { status: 299 });
        const r4 = new Response(null, { status: 300 });
        const r5 = new Response(null, { status: 599 });
        JSON.stringify([r1.ok, r2.ok, r3.ok, r4.ok, r5.ok]);
        "#,
        js_string,
    );
    assert_eq!(s, r#"[true,true,true,false,false]"#);
}

// ---------------------------------------------------------------------------
// Null-body status (Fetch §5.5 step 7)
// ---------------------------------------------------------------------------

#[test]
fn null_body_status_204_with_body_throws() {
    // Per Fetch §5.5: { 101, 103, 204, 205, 304 } are null-body
    // statuses — having a body throws TypeError.
    let s = run_in_v8(
        r#"
        let err = null;
        try {
            new Response("body", { status: 204 });
        } catch (e) {
            err = e;
        }
        JSON.stringify({
            isTE: err instanceof TypeError,
            name: err && err.name,
        });
        "#,
        js_string,
    );
    assert_eq!(s, r#"{"isTE":true,"name":"TypeError"}"#);
}

#[test]
fn null_body_status_set_in_range_throw_type_error_with_body() {
    // Per Fetch §5.5: null-body statuses are { 101, 103, 204, 205, 304 }.
    // The spec's "Initialize a response" steps run the range check
    // (200..=599) FIRST, but the native impl (and the polyfill before
    // it) allow 101 as a workerd-style WebSocket-upgrade extension.
    // Result: 101 reaches the null-body check and throws TypeError; 103
    // is still out-of-range and throws RangeError. 204/205/304 are in
    // the spec range and reach the null-body check (TypeError).
    let s = run_in_v8(
        r#"
        const reachesNullBodyCheck = [101, 204, 205, 304];
        const stillOutOfRange = [103];
        const reachesResults = reachesNullBodyCheck.map(st => {
            try {
                new Response("body", { status: st });
                return "no-throw";
            } catch (e) {
                return e.name;
            }
        });
        const outOfRangeResults = stillOutOfRange.map(st => {
            try {
                new Response("body", { status: st });
                return "no-throw";
            } catch (e) {
                return e.name;
            }
        });
        JSON.stringify({ inRange: reachesResults, outOfRange: outOfRangeResults });
        "#,
        js_string,
    );
    assert_eq!(
        s,
        r#"{"inRange":["TypeError","TypeError","TypeError","TypeError"],"outOfRange":["RangeError"]}"#
    );
}

#[test]
fn null_body_status_with_null_body_succeeds() {
    // 204/205/304/etc are fine when body is null.
    let s = run_in_v8(
        r#"
        const r = new Response(null, { status: 204 });
        JSON.stringify({ status: r.status, body: r.body });
        "#,
        js_string,
    );
    assert_eq!(s, r#"{"status":204,"body":null}"#);
}

// ---------------------------------------------------------------------------
// Status range (Fetch §5.5 step 1)
// ---------------------------------------------------------------------------

#[test]
fn invalid_status_below_200_throws_range_error() {
    // Per Fetch §5.5 step 1: status must be in [200, 599].
    let s = run_in_v8(
        r#"
        let err = null;
        try { new Response(null, { status: 100 }); } catch (e) { err = e; }
        JSON.stringify({
            isRE: err instanceof RangeError,
            name: err && err.name,
        });
        "#,
        js_string,
    );
    assert_eq!(s, r#"{"isRE":true,"name":"RangeError"}"#);
}

#[test]
fn invalid_status_above_599_throws_range_error() {
    let s = run_in_v8(
        r#"
        let err = null;
        try { new Response(null, { status: 600 }); } catch (e) { err = e; }
        err && err.name;
        "#,
        js_string,
    );
    assert_eq!(s, "RangeError");
}

// ---------------------------------------------------------------------------
// Response.error()
// ---------------------------------------------------------------------------

#[test]
fn response_error_returns_network_error_response() {
    // Per Fetch §5.5: Response.error() returns a "network error"
    // response: type="error", status=0, body=null, headers immutable.
    let s = run_in_v8(
        r#"
        const r = Response.error();
        JSON.stringify({
            isResp: r instanceof Response,
            type: r.type,
            status: r.status,
            statusText: r.statusText,
            body: r.body,
            ok: r.ok,
        });
        "#,
        js_string,
    );
    assert_eq!(
        s,
        r#"{"isResp":true,"type":"error","status":0,"statusText":"","body":null,"ok":false}"#
    );
}

// ---------------------------------------------------------------------------
// Response.redirect()
// ---------------------------------------------------------------------------

#[test]
fn response_redirect_default_302() {
    // Per Fetch §5.5: Response.redirect(url) defaults to status 302
    // and sets Location to url.
    let s = run_in_v8(
        r#"
        const r = Response.redirect("https://example.com/dest");
        JSON.stringify({
            status: r.status,
            location: r.headers.get("Location"),
        });
        "#,
        js_string,
    );
    assert_eq!(
        s,
        r#"{"status":302,"location":"https://example.com/dest"}"#
    );
}

#[test]
fn response_redirect_explicit_status() {
    let s = run_in_v8(
        r#"
        const r = Response.redirect("https://example.com/dest", 307);
        r.status;
        "#,
        |val, scope| val.int32_value(scope).unwrap_or(-1),
    );
    assert_eq!(s, 307);
}

#[test]
fn response_redirect_invalid_status_throws_range_error() {
    // Per Fetch §5.5: redirect status must be one of {301, 302, 303,
    // 307, 308}. Anything else is RangeError.
    let s = run_in_v8(
        r#"
        let err = null;
        try {
            Response.redirect("https://example.com", 200);
        } catch (e) {
            err = e;
        }
        JSON.stringify({
            isRE: err instanceof RangeError,
            name: err && err.name,
        });
        "#,
        js_string,
    );
    assert_eq!(s, r#"{"isRE":true,"name":"RangeError"}"#);
}

// ---------------------------------------------------------------------------
// Response.json()
// ---------------------------------------------------------------------------

#[test]
fn response_json_static_serializes_data_and_sets_content_type() {
    // Per Fetch §5.5: Response.json(data, init?) serializes data via
    // JSON.stringify and sets Content-Type to application/json.
    let s = run_async_in_v8(
        r#"
        const r = Response.json({ a: 1, b: "two" });
        const ct = r.headers.get("Content-Type");
        r.text().then(t => {
            globalThis.__result = JSON.stringify({
                ct,
                body: t,
            });
        });
        "#,
    );
    assert_eq!(
        s,
        r#"{"ct":"application/json","body":"{\"a\":1,\"b\":\"two\"}"}"#
    );
}

// ---------------------------------------------------------------------------
// clone()
// ---------------------------------------------------------------------------

#[test]
fn clone_yields_independent_consumable_response() {
    let s = run_async_in_v8(
        r#"
        const r1 = new Response("hello");
        const r2 = r1.clone();
        Promise.all([r1.text(), r2.text()]).then(([a, b]) => {
            globalThis.__result = JSON.stringify({ a, b });
        }).catch(e => {
            globalThis.__result = "ERR:" + e.message;
        });
        "#,
    );
    assert_eq!(s, r#"{"a":"hello","b":"hello"}"#);
}

#[test]
fn clone_preserves_status_status_text_headers() {
    let s = run_in_v8(
        r#"
        const r1 = new Response("body", {
            status: 201,
            statusText: "Created",
            headers: { "x-custom": "1" },
        });
        const r2 = r1.clone();
        JSON.stringify({
            status: r2.status,
            statusText: r2.statusText,
            x: r2.headers.get("x-custom"),
        });
        "#,
        js_string,
    );
    assert_eq!(
        s,
        r#"{"status":201,"statusText":"Created","x":"1"}"#
    );
}

// ---------------------------------------------------------------------------
// Body consumers
// ---------------------------------------------------------------------------

#[test]
fn text_returns_body_string() {
    let s = run_async_in_v8(
        r#"
        const r = new Response("hello world");
        r.text().then(t => { globalThis.__result = t; });
        "#,
    );
    assert_eq!(s, "hello world");
}

#[test]
fn array_buffer_returns_array_buffer_not_uint8array() {
    // Per Fetch §3.5 arrayBuffer(): the resolved value is an
    // ArrayBuffer, NOT a Uint8Array view.
    let s = run_async_in_v8(
        r#"
        const r = new Response("hello");
        r.arrayBuffer().then(ab => {
            globalThis.__result = JSON.stringify({
                isAB: ab instanceof ArrayBuffer,
                isU8: ab instanceof Uint8Array,
                len: ab.byteLength,
            });
        });
        "#,
    );
    assert_eq!(
        s,
        r#"{"isAB":true,"isU8":false,"len":5}"#
    );
}

#[test]
fn json_rejects_with_syntax_error_not_type_error() {
    // Per MAJOR-25: response.json() rejects with SyntaxError, not
    // TypeError, on parse failure.
    let s = run_async_in_v8(
        r#"
        const r = new Response("not json{{");
        r.json().catch(e => {
            globalThis.__result = JSON.stringify({
                isSE: e instanceof SyntaxError,
                isTE: e instanceof TypeError,
                name: e && e.name,
            });
        });
        "#,
    );
    assert_eq!(
        s,
        r#"{"isSE":true,"isTE":false,"name":"SyntaxError"}"#
    );
}
