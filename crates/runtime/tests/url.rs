mod common;
use common::*;

#[test]
fn url_parse_basic() {
    let r = dispatch(m(r#"
        export function test() {
            const url = new URL("https://example.com:8080/path?q=1#frag");
            return {
                protocol: url.protocol,
                hostname: url.hostname,
                port: url.port,
                pathname: url.pathname,
                search: url.search,
                hash: url.hash,
                origin: url.origin,
                host: url.host,
            };
        }
    "#), r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#).unwrap();
    assert!(r.json.contains("\"protocol\":\"https:\""), "got: {}", r.json);
    assert!(r.json.contains("\"hostname\":\"example.com\""), "got: {}", r.json);
    assert!(r.json.contains("\"port\":\"8080\""), "got: {}", r.json);
    assert!(r.json.contains("\"pathname\":\"/path\""), "got: {}", r.json);
    assert!(r.json.contains("\"hash\":\"#frag\""), "got: {}", r.json);
}

#[test]
fn url_with_base() {
    let r = dispatch(m(r#"
        export function test() {
            const url = new URL("/api/users", "https://example.com");
            return url.href;
        }
    "#), r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#).unwrap();
    assert!(r.json.contains("https://example.com/api/users"), "got: {}", r.json);
}

#[test]
fn url_invalid_throws() {
    let r = dispatch(m(r#"
        export function test() {
            try { new URL("not a url"); return "should have thrown"; }
            catch (e) { return "caught: " + e.message; }
        }
    "#), r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#).unwrap();
    assert!(r.json.contains("caught:"), "got: {}", r.json);
}

#[test]
fn url_can_parse() {
    let r = dispatch(m(r#"
        export function test() {
            return {
                valid: URL.canParse("https://example.com"),
                invalid: URL.canParse("not a url"),
                relative: URL.canParse("/path", "https://example.com"),
            };
        }
    "#), r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#).unwrap();
    assert!(r.json.contains("\"valid\":true"), "got: {}", r.json);
    assert!(r.json.contains("\"invalid\":false"), "got: {}", r.json);
    assert!(r.json.contains("\"relative\":true"), "got: {}", r.json);
}

#[test]
fn url_search_params() {
    let r = dispatch(m(r#"
        export function test() {
            const url = new URL("https://example.com/search?q=hello&lang=en");
            const p = url.searchParams;
            return {
                q: p.get("q"),
                lang: p.get("lang"),
                missing: p.get("x"),
                has_q: p.has("q"),
            };
        }
    "#), r#"{"jsonrpc":"2.0","method":"test","params":[],"id":1}"#).unwrap();
    assert!(r.json.contains("\"q\":\"hello\""), "got: {}", r.json);
    assert!(r.json.contains("\"lang\":\"en\""), "got: {}", r.json);
    assert!(r.json.contains("\"missing\":null"), "got: {}", r.json);
    assert!(r.json.contains("\"has_q\":true"), "got: {}", r.json);
}
