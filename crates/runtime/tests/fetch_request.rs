//! Hand-written tests for the native `Request` class
//! (`fetch_request::install_global`).
//!
//! These exercise the Fetch §5.4 constructor + getters + `clone()` +
//! body consumers + signal propagation. WPT conformance lives in
//! `wpt_fetch_request.rs`; this file covers the design's CRITICAL list
//! and behavioural shape.
//!
//! Pattern matches `fetch_body.rs`: install fetch globals on a fresh
//! isolate, evaluate JS, assert via stringified result.

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
// Construction — basic URL parsing
// ---------------------------------------------------------------------------

#[test]
fn construct_with_valid_url_succeeds() {
    let s = run_in_v8(
        r#"
        const r = new Request("https://example.com/path");
        JSON.stringify({
            tag: r.constructor.name,
            symTag: r[Symbol.toStringTag],
            url: r.url,
            method: r.method,
        });
        "#,
        js_string,
    );
    assert_eq!(
        s,
        r#"{"tag":"Request","symTag":"Request","url":"https://example.com/path","method":"GET"}"#
    );
}

#[test]
fn construct_with_invalid_url_throws_type_error() {
    // Per Fetch §5.4 step 6 (b): if URL parsing fails, throw TypeError.
    // "not a url" actually parses successfully as a path-relative URL
    // against our synthetic base — to test the error path we use an
    // input that fails parsing in BOTH absolute and relative modes.
    // "http://" with no host is one such case.
    let s = run_in_v8(
        r#"
        let err = null;
        try {
            new Request("http://[invalid");
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

// ---------------------------------------------------------------------------
// Method normalization (D-18)
// ---------------------------------------------------------------------------

#[test]
fn method_defaults_to_get() {
    let s = run_in_v8(
        r#"
        const r = new Request("https://example.com");
        r.method;
        "#,
        js_string,
    );
    assert_eq!(s, "GET");
}

#[test]
fn method_lowercase_post_uppercased() {
    // Per Fetch §4.3: standard methods are uppercased.
    let s = run_in_v8(
        r#"
        const r = new Request("https://example.com", { method: "post" });
        r.method;
        "#,
        js_string,
    );
    assert_eq!(s, "POST");
}

#[test]
fn method_patch_case_preserved() {
    // Per Fetch §4.3 step 13: method names that are NOT in the spec's
    // "standard methods" list are case-preserved. PATCH is one of these
    // (workerd / node match this; deno explicitly tests PATCH preserves case).
    let s = run_in_v8(
        r#"
        const r = new Request("https://example.com", { method: "PATCH" });
        r.method;
        "#,
        js_string,
    );
    assert_eq!(s, "PATCH");
}

#[test]
fn method_connect_throws_type_error() {
    // Per Fetch §4.3 step 4: forbidden methods include CONNECT.
    let s = run_in_v8(
        r#"
        let err = null;
        try {
            new Request("https://example.com", { method: "CONNECT" });
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
fn method_trace_throws_type_error() {
    let s = run_in_v8(
        r#"
        let err = null;
        try {
            new Request("https://example.com", { method: "TRACE" });
        } catch (e) {
            err = e;
        }
        err && err.name;
        "#,
        js_string,
    );
    assert_eq!(s, "TypeError");
}

// ---------------------------------------------------------------------------
// Headers init
// ---------------------------------------------------------------------------

#[test]
fn init_headers_object_populates_headers() {
    let s = run_in_v8(
        r#"
        const r = new Request("https://example.com", {
            headers: { "x-custom": "hello" },
        });
        JSON.stringify({
            isHeaders: r.headers instanceof Headers,
            x: r.headers.get("x-custom"),
        });
        "#,
        js_string,
    );
    assert_eq!(s, r#"{"isHeaders":true,"x":"hello"}"#);
}

#[test]
fn headers_returns_same_object_per_same_object_idl() {
    // Per Fetch §5.4 [SameObject]: every read of request.headers
    // returns the SAME Headers instance.
    let s = run_in_v8(
        r#"
        const r = new Request("https://example.com");
        const a = r.headers;
        const b = r.headers;
        JSON.stringify({ same: a === b });
        "#,
        js_string,
    );
    assert_eq!(s, r#"{"same":true}"#);
}

// ---------------------------------------------------------------------------
// Body — extraction + GET/HEAD constraint
// ---------------------------------------------------------------------------

#[test]
fn post_with_body_extracts_text_plain_content_type() {
    let s = run_in_v8(
        r#"
        const r = new Request("https://example.com", {
            method: "POST",
            body: "hello",
        });
        JSON.stringify({
            method: r.method,
            ct: r.headers.get("Content-Type"),
        });
        "#,
        js_string,
    );
    assert_eq!(
        s,
        r#"{"method":"POST","ct":"text/plain;charset=UTF-8"}"#
    );
}

#[test]
fn get_with_body_throws_type_error() {
    // Per Fetch §5.4 step 35.5: a Request with GET/HEAD method cannot
    // have a body.
    let s = run_in_v8(
        r#"
        let err = null;
        try {
            new Request("https://example.com", { body: "hello" });  // default GET
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
fn head_with_body_throws_type_error() {
    let s = run_in_v8(
        r#"
        let err = null;
        try {
            new Request("https://example.com", {
                method: "HEAD",
                body: "hello",
            });
        } catch (e) {
            err = e;
        }
        err && err.name;
        "#,
        js_string,
    );
    assert_eq!(s, "TypeError");
}

// ---------------------------------------------------------------------------
// body / bodyUsed surface
// ---------------------------------------------------------------------------

#[test]
fn body_is_readable_stream_when_set_or_null_otherwise() {
    let s = run_in_v8(
        r#"
        const noBody = new Request("https://example.com");
        const withBody = new Request("https://example.com", {
            method: "POST",
            body: "hello",
        });
        JSON.stringify({
            noBody: noBody.body,
            withBodyIsStream: withBody.body instanceof ReadableStream,
        });
        "#,
        js_string,
    );
    assert_eq!(
        s,
        r#"{"noBody":null,"withBodyIsStream":true}"#
    );
}

#[test]
fn body_used_initially_false_then_true_after_text() {
    // Per Fetch §3.5 step 1: consumer flips bodyUsed to true.
    let s = run_async_in_v8(
        r#"
        const r = new Request("https://example.com", {
            method: "POST",
            body: "hello",
        });
        const before = r.bodyUsed;
        r.text().then(_ => {
            globalThis.__result = JSON.stringify({
                before,
                after: r.bodyUsed,
            });
        });
        "#,
    );
    assert_eq!(s, r#"{"before":false,"after":true}"#);
}

// ---------------------------------------------------------------------------
// clone()
// ---------------------------------------------------------------------------

#[test]
fn clone_yields_independent_consumable_request() {
    // Per Fetch §5.4 clone: both original and clone are independently
    // consumable.
    let s = run_async_in_v8(
        r#"
        const r1 = new Request("https://example.com", {
            method: "POST",
            body: "hello",
        });
        const r2 = r1.clone();
        // Consume both bodies in parallel and compare.
        Promise.all([r1.text(), r2.text()]).then(([a, b]) => {
            globalThis.__result = JSON.stringify({
                a, b,
                sameUrl: r1.url === r2.url,
                sameMethod: r1.method === r2.method,
            });
        }).catch(e => {
            globalThis.__result = "ERR:" + e.message;
        });
        "#,
    );
    assert_eq!(
        s,
        r#"{"a":"hello","b":"hello","sameUrl":true,"sameMethod":true}"#
    );
}

// ---------------------------------------------------------------------------
// Signal — propagation from init.signal
// ---------------------------------------------------------------------------

#[test]
fn signal_default_minted() {
    // Per Fetch §5.4: request.signal is non-null. We mint one if
    // init.signal is missing.
    let s = run_in_v8(
        r#"
        const r = new Request("https://example.com");
        JSON.stringify({
            isSignal: r.signal instanceof AbortSignal,
            aborted: r.signal.aborted,
        });
        "#,
        js_string,
    );
    assert_eq!(s, r#"{"isSignal":true,"aborted":false}"#);
}

#[test]
fn signal_propagates_init_abort() {
    // Per Fetch §5.4 step 30+: request.signal follows init.signal
    // (we use AbortSignal.any() to chain).
    let s = run_in_v8(
        r#"
        const c = new AbortController();
        const r = new Request("https://example.com", { signal: c.signal });
        const before = r.signal.aborted;
        c.abort("user cancelled");
        JSON.stringify({
            before,
            after: r.signal.aborted,
            reason: r.signal.reason,
        });
        "#,
        js_string,
    );
    assert_eq!(
        s,
        r#"{"before":false,"after":true,"reason":"user cancelled"}"#
    );
}
