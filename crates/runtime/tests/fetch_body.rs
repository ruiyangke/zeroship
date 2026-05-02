//! Hand-written tests for the native body model
//! (`fetch_body::extract_body` + Body trait + consumer methods).
//!
//! These tests drive the JS surface through a real V8 isolate. WPT
//! conformance lives in `wpt_fetch_body.rs`; this file covers the
//! design's CRITICAL list (C-10 dispatch order, C-11 USVString
//! conversion, MAJOR-25 consumer error shapes) plus the behavioural
//! shape we want for v1 (length, content-type defaults, stream
//! disturbed/locked rejection).
//!
//! We exercise extract_body through `new Request(..., { body })` and
//! `new Response(body)` since extract_body itself is a private path —
//! the JS surface is what matters.
//!
//! Pattern matches `abort.rs`: install fetch globals on a fresh
//! isolate, evaluate JS, assert via stringified result.
//!
//! ## Async-result pattern
//!
//! Body consumers settle through multiple microtask hops (read →
//! release lock → resolve outer). The harness drains microtasks until
//! a quiescent state is reached, then evaluates a result-reader
//! script. Tests that don't need to wait for a Promise just use the
//! single-shot `run_in_v8`.

#![allow(unsafe_code)]

use zeroship_runtime::dom;
use zeroship_runtime::fetch_request;
use zeroship_runtime::fetch_response;
use zeroship_runtime::headers;
use zeroship_runtime::init_v8;
use zeroship_runtime::streams;

/// Set up a fresh V8 isolate with all native globals (streams,
/// Headers, DOM, Request, Response). Returns a closure-callable
/// scope. The harness drains microtasks once after the script run —
/// callers that need promise settlements should use the dedicated
/// `run_async_in_v8` helper.
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

/// Run JS that captures a Promise-derived value into a `globalThis.__result`
/// variable. The harness drains microtasks repeatedly until the result is
/// observed (or the budget is exhausted), then returns the result string.
fn run_async_in_v8(setup_src: &str) -> String {
    init_v8();
    let mut isolate = v8::Isolate::new(v8::CreateParams::default());
    v8::scope!(let handle_scope, &mut isolate);
    let context = v8::Context::new(handle_scope, Default::default());
    let scope = &mut v8::ContextScope::new(handle_scope, context);

    install_globals(scope);

    // Run the setup which should set globalThis.__result = ...; eventually.
    let src_v8 = v8::String::new(scope, setup_src).unwrap();
    let script = v8::Script::compile(scope, src_v8, None).unwrap();
    script.run(scope).unwrap();

    // Drain microtasks several times to settle promise chains.
    for _ in 0..32 {
        scope.perform_microtask_checkpoint();
    }

    // Read globalThis.__result.
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

    // Tests need URLSearchParams. We install a minimal pure-JS shim with
    // the spec-required Symbol.toStringTag so extract_body's duck-type
    // check fires. (The real polyfill at embed/url.js is loaded by the
    // full runtime stack; for unit tests we keep the harness self-contained.)
    let usp_shim = v8::String::new(scope, URL_SEARCH_PARAMS_SHIM).unwrap();
    let script = v8::Script::compile(scope, usp_shim, None).unwrap();
    script.run(scope).unwrap();
}

const URL_SEARCH_PARAMS_SHIM: &str = r#"
(function () {
  function URLSearchParams(init) {
    this._params = [];
    if (typeof init === "string") {
      if (init.charAt(0) === "?") init = init.slice(1);
      var pairs = init.split("&");
      for (var i = 0; i < pairs.length; i++) {
        if (!pairs[i]) continue;
        var eq = pairs[i].indexOf("=");
        if (eq === -1) {
          this._params.push([decodeURIComponent(pairs[i]), ""]);
        } else {
          this._params.push([
            decodeURIComponent(pairs[i].slice(0, eq)),
            decodeURIComponent(pairs[i].slice(eq + 1).replace(/\+/g, " ")),
          ]);
        }
      }
    } else if (Array.isArray(init)) {
      for (var j = 0; j < init.length; j++) {
        this._params.push([String(init[j][0]), String(init[j][1])]);
      }
    }
  }
  URLSearchParams.prototype.append = function (n, v) {
    this._params.push([String(n), String(v)]);
  };
  URLSearchParams.prototype.toString = function () {
    return this._params.map(function (p) {
      return encodeURIComponent(p[0]).replace(/%20/g, "+") +
             "=" +
             encodeURIComponent(p[1]).replace(/%20/g, "+");
    }).join("&");
  };
  // Spec: URLSearchParams.prototype[Symbol.toStringTag] === "URLSearchParams".
  URLSearchParams.prototype[Symbol.toStringTag] = "URLSearchParams";
  globalThis.URLSearchParams = URLSearchParams;
})();
"#;

fn js_string(val: v8::Local<v8::Value>, scope: &mut v8::PinScope) -> String {
    val.to_rust_string_lossy(scope)
}

// ---------------------------------------------------------------------------
// extract_body — string body
// ---------------------------------------------------------------------------

#[test]
fn extract_string_body_sets_length_and_text_plain_mime() {
    // Per Fetch §3.2 step 11.7: a USVString body has Content-Type
    // "text/plain;charset=UTF-8" and a known length equal to the UTF-8
    // byte count.
    let s = run_in_v8(
        r#"
        const r = new Response("hello");
        JSON.stringify({
            ct: r.headers.get("Content-Type"),
        });
        "#,
        js_string,
    );
    assert_eq!(s, r#"{"ct":"text/plain;charset=UTF-8"}"#);
}

#[test]
fn extract_string_body_arraybuffer_returns_5_bytes() {
    // String body of "hello" has 5 UTF-8 bytes; arrayBuffer().byteLength
    // exposes the length.
    let s = run_async_in_v8(
        r#"
        const r = new Response("hello");
        r.arrayBuffer().then(ab => {
            globalThis.__result = JSON.stringify({
                len: ab.byteLength,
                isAB: ab instanceof ArrayBuffer,
            });
        });
        "#,
    );
    assert_eq!(s, r#"{"len":5,"isAB":true}"#);
}

#[test]
fn extract_empty_string_body_has_length_zero() {
    // Empty body — length 0, Content-Type still set.
    let s = run_async_in_v8(
        r#"
        const r = new Response("");
        r.arrayBuffer().then(ab => {
            globalThis.__result = JSON.stringify({
                ct: r.headers.get("Content-Type"),
                len: ab.byteLength,
            });
        });
        "#,
    );
    assert_eq!(s, r#"{"ct":"text/plain;charset=UTF-8","len":0}"#);
}

// ---------------------------------------------------------------------------
// extract_body — BufferSource (Uint8Array / ArrayBuffer)
// ---------------------------------------------------------------------------

#[test]
fn extract_uint8array_body_no_default_content_type() {
    // Per Fetch §3.2 step 11.5: BufferSource bodies have NO default
    // Content-Type. The header must be null/absent.
    let s = run_in_v8(
        r#"
        const r = new Response(new Uint8Array([1,2,3]));
        JSON.stringify({
            ct: r.headers.get("Content-Type"),
            has: r.headers.has("Content-Type"),
        });
        "#,
        js_string,
    );
    assert_eq!(s, r#"{"ct":null,"has":false}"#);
}

#[test]
fn extract_uint8array_body_length_equals_byte_length() {
    // Length 3 for [1,2,3]; verify via arrayBuffer.
    let s = run_async_in_v8(
        r#"
        const r = new Response(new Uint8Array([1,2,3]));
        r.arrayBuffer().then(ab => {
            const u8 = new Uint8Array(ab);
            globalThis.__result = JSON.stringify({
                len: ab.byteLength,
                bytes: Array.from(u8),
            });
        });
        "#,
    );
    assert_eq!(s, r#"{"len":3,"bytes":[1,2,3]}"#);
}

// ---------------------------------------------------------------------------
// extract_body — URLSearchParams
// ---------------------------------------------------------------------------

#[test]
fn extract_url_search_params_body_sets_urlencoded_mime() {
    // Per Fetch §3.2 step 11.6: URLSearchParams body has Content-Type
    // "application/x-www-form-urlencoded;charset=UTF-8".
    let s = run_async_in_v8(
        r#"
        const usp = new URLSearchParams();
        usp.append("a", "1");
        usp.append("b", "2");
        const r = new Response(usp);
        r.text().then(t => {
            globalThis.__result = JSON.stringify({
                ct: r.headers.get("Content-Type"),
                body: t,
            });
        });
        "#,
    );
    assert_eq!(
        s,
        r#"{"ct":"application/x-www-form-urlencoded;charset=UTF-8","body":"a=1&b=2"}"#
    );
}

// ---------------------------------------------------------------------------
// extract_body — FormData (multipart)
// ---------------------------------------------------------------------------

#[test]
fn extract_form_data_body_multipart_with_boundary() {
    // Per Fetch §3.2 step 11.4: FormData body has Content-Type
    // "multipart/form-data; boundary=...".
    let s = run_in_v8(
        r#"
        const fd = new FormData();
        fd.append("a", "1");
        const r = new Response(fd);
        const ct = r.headers.get("Content-Type");
        JSON.stringify({
            startsWith: typeof ct === "string" && ct.startsWith("multipart/form-data; boundary="),
        });
        "#,
        js_string,
    );
    assert_eq!(s, r#"{"startsWith":true}"#);
}

// ---------------------------------------------------------------------------
// extract_body — ReadableStream
// ---------------------------------------------------------------------------

#[test]
fn extract_readable_stream_body_no_default_content_type() {
    // Per Fetch §3.2 step 11.11: a stream body has no MIME default.
    // The body must be a ReadableStream.
    let s = run_in_v8(
        r#"
        const stream = new ReadableStream({
            start(controller) {
                controller.enqueue(new Uint8Array([1,2,3]));
                controller.close();
            }
        });
        const r = new Response(stream);
        JSON.stringify({
            ct: r.headers.get("Content-Type"),
            has: r.headers.has("Content-Type"),
            isStream: r.body instanceof ReadableStream,
        });
        "#,
        js_string,
    );
    assert_eq!(s, r#"{"ct":null,"has":false,"isStream":true}"#);
}

#[test]
fn extract_disturbed_stream_body_throws_type_error() {
    // Per Fetch §3.2 step 11.11.2: a locked or disturbed stream body
    // throws TypeError on extraction.
    let s = run_in_v8(
        r#"
        const stream = new ReadableStream({
            start(controller) {
                controller.enqueue(new Uint8Array([1,2,3]));
                controller.close();
            }
        });
        // Lock the stream.
        const reader = stream.getReader();
        let err = null;
        try {
            new Response(stream);
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
// extract_body — keepalive constraint
// ---------------------------------------------------------------------------

#[test]
fn keepalive_with_string_body_works() {
    // Per Fetch §3.2 step 11.10: keepalive only conflicts with a
    // ReadableStream body. String bodies are fine.
    let s = run_in_v8(
        r#"
        let err = null;
        try {
            new Request("https://example.com", {
                method: "POST",
                body: "hello",
                keepalive: true,
            });
        } catch (e) {
            err = e;
        }
        JSON.stringify({
            ok: err === null,
        });
        "#,
        js_string,
    );
    assert_eq!(s, r#"{"ok":true}"#);
}

#[test]
fn keepalive_with_stream_body_throws_type_error() {
    // Per Fetch §3.2 step 11.10: keepalive + ReadableStream → TypeError.
    let s = run_in_v8(
        r#"
        const stream = new ReadableStream({
            start(c) { c.close(); }
        });
        let err = null;
        try {
            new Request("https://example.com", {
                method: "POST",
                body: stream,
                keepalive: true,
            });
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
// USVString conversion (C-11)
// ---------------------------------------------------------------------------

#[test]
fn usv_string_replaces_lone_surrogates() {
    // Per WebIDL USVString: lone surrogates → U+FFFD. Verify on a
    // string body containing a lone high surrogate. We assert via
    // numeric codepoints to avoid Rust-side escape handling.
    let s = run_async_in_v8(
        r#"
        // High surrogate alone — should become U+FFFD on UTF-8 encode +
        // re-decode round-trip.
        const r = new Response("a\uD800b");
        r.text().then(t => {
            globalThis.__result = JSON.stringify({
                len: t.length,
                first: t.charCodeAt(0),
                replCode: t.charCodeAt(1),
                last: t.charCodeAt(2),
                ufffd: 0xFFFD,
            });
        });
        "#,
    );
    // 'a' (0x61=97), U+FFFD (65533), 'b' (0x62=98)
    assert_eq!(
        s,
        r#"{"len":3,"first":97,"replCode":65533,"last":98,"ufffd":65533}"#
    );
}

// ---------------------------------------------------------------------------
// Consumer error shapes (MAJOR-25)
// ---------------------------------------------------------------------------

#[test]
fn json_consumer_rejects_with_syntax_error_on_bad_json() {
    // Per MAJOR-25 / WHATWG Fetch: response.json() rejects with
    // SyntaxError (JSON.parse semantics), NOT TypeError.
    let s = run_async_in_v8(
        r#"
        const r = new Response("not json {{");
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

#[test]
fn empty_body_json_rejects_with_syntax_error() {
    // Per JSON.parse: empty input → SyntaxError. Same for body.json()
    // on an empty body (no body case).
    let s = run_async_in_v8(
        r#"
        const r = new Response();
        r.json().catch(e => {
            globalThis.__result = JSON.stringify({
                isSE: e instanceof SyntaxError,
                name: e && e.name,
            });
        });
        "#,
    );
    assert_eq!(s, r#"{"isSE":true,"name":"SyntaxError"}"#);
}

// ---------------------------------------------------------------------------
// Stream-as-body with consumer
// ---------------------------------------------------------------------------

#[test]
fn consumer_on_stream_body_reads_chunks() {
    // Stream body — consumer reads through getReader/release_lock.
    let s = run_async_in_v8(
        r#"
        const stream = new ReadableStream({
            start(c) {
                c.enqueue(new Uint8Array([72, 101, 108, 108, 111]));  // "Hello"
                c.close();
            }
        });
        const r = new Response(stream);
        r.text().then(t => { globalThis.__result = t; });
        "#,
    );
    assert_eq!(s, "Hello");
}
