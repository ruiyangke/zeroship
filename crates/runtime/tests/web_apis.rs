mod common;
use common::*;

#[test]
fn text_encoder_decoder() {
    let r = dispatch(m(r#"export function test() {
        var enc = new TextEncoder();
        var buf = enc.encode("Hello");
        var dec = new TextDecoder();
        return { encoded: Array.from(buf), decoded: dec.decode(buf) };
    }"#), r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#).unwrap();
    assert!(r.json.contains("Hello"), "got: {}", r.json);
    assert!(r.json.contains("[72,101,108,108,111]"), "got: {}", r.json);
}

// Regression: a previous TextDecoder polyfill did
// `new Uint8Array(buf.buffer || buf)` which silently widened every subarray
// view back to the underlying ArrayBuffer — breaking OpenAI SDK's LineDecoder
// (which splits the concatenation buffer via `subarray(0, patternIndex)` and
// then decodes each slice). The SSE stream produced exactly one empty line
// with the full buffer repeated, so `for await (const part of openai_stream)`
// yielded zero iterations.
#[test]
fn text_decoder_respects_subarray_bounds() {
    let r = dispatch(m(r#"export function test() {
        var enc = new TextEncoder();
        var full = enc.encode("ABCDE\nXYZ");
        var dec = new TextDecoder();
        var head = dec.decode(full.subarray(0, 5));     // "ABCDE"
        var mid  = dec.decode(full.subarray(6, 9));     // "XYZ"
        return { head: head, mid: mid };
    }"#), r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#).unwrap();
    assert!(r.json.contains("\"head\":\"ABCDE\""), "got: {}", r.json);
    assert!(r.json.contains("\"mid\":\"XYZ\""), "got: {}", r.json);
}

#[test]
fn structured_clone() {
    let r = dispatch(m(r#"export function test() {
        var obj = { a: 1, b: [2, 3], c: { d: "hello" } };
        var clone = structuredClone(obj);
        clone.a = 99;
        clone.b.push(4);
        return { original: obj.a, cloned: clone.a, origLen: obj.b.length, cloneLen: clone.b.length };
    }"#), r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#).unwrap();
    assert!(r.json.contains("\"original\":1"), "got: {}", r.json);
    assert!(r.json.contains("\"cloned\":99"), "got: {}", r.json);
    assert!(r.json.contains("\"origLen\":2"), "got: {}", r.json);
    assert!(r.json.contains("\"cloneLen\":3"), "got: {}", r.json);
}

#[test]
fn btoa_atob() {
    let r = dispatch(m(r#"export function test() {
        var encoded = btoa("Hello, World!");
        var decoded = atob(encoded);
        return { encoded, decoded };
    }"#), r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#).unwrap();
    assert!(r.json.contains("SGVsbG8sIFdvcmxkIQ=="), "got: {}", r.json);
    assert!(r.json.contains("Hello, World!"), "got: {}", r.json);
}

#[test]
fn headers_class_works() {
    let r = dispatch(m(r#"
        export function test() {
            var h = new Headers({ "Content-Type": "text/plain", "X-Custom": "hello" });
            h.append("X-Custom", "world");
            return {
                ct: h.get("content-type"),
                custom: h.get("x-custom"),
                has_ct: h.has("content-type"),
                missing: h.has("nonexistent"),
            };
        }
    "#), r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#).unwrap();
    assert!(r.json.contains("text/plain"), "got: {}", r.json);
    assert!(r.json.contains("hello, world"), "got: {}", r.json);
}

#[test]
fn request_class_works() {
    let r = dispatch(m(r#"
        export function test() {
            var req = new Request("https://example.com", {
                method: "POST",
                headers: { "X-Test": "1" },
                body: "hello",
            });
            return {
                url: req.url,
                method: req.method,
                header: req.headers.get("x-test"),
            };
        }
    "#), r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#).unwrap();
    assert!(r.json.contains("example.com"), "got: {}", r.json);
    assert!(r.json.contains("POST"), "got: {}", r.json);
}

#[test]
fn response_static_json() {
    let r = dispatch(m(r#"
        export async function test() {
            var resp = Response.json({ hello: "world" });
            var data = await resp.json();
            return { status: resp.status, hello: data.hello, ct: resp.headers.get("content-type") };
        }
    "#), r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#).unwrap();
    assert!(r.json.contains("world"), "got: {}", r.json);
    assert!(r.json.contains("application/json"), "got: {}", r.json);
}
