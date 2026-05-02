//! Native atob/btoa tests. WHATWG HTML §8.6
//! (https://html.spec.whatwg.org/multipage/webappapis.html#atob-and-btoa).

mod common;
use common::{dispatch, m};

#[test]
fn btoa_encodes_ascii() {
    let r = dispatch(
        m(r#"export function test() {
            return { encoded: btoa("hello") };
        }"#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains("\"encoded\":\"aGVsbG8=\""), "got: {}", r.json);
}

#[test]
fn atob_decodes_base64() {
    let r = dispatch(
        m(r#"export function test() {
            return { decoded: atob("aGVsbG8=") };
        }"#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains("\"decoded\":\"hello\""), "got: {}", r.json);
}

#[test]
fn btoa_round_trip() {
    let r = dispatch(
        m(r#"export function test() {
            const original = "Hello, World!";
            const encoded = btoa(original);
            const decoded = atob(encoded);
            return { round_trip: decoded === original, encoded };
        }"#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains("\"round_trip\":true"), "got: {}", r.json);
}

#[test]
fn btoa_throws_on_high_code_units() {
    // Per spec: btoa with code units > 0xFF must throw
    // DOMException("InvalidCharacterError"). The polyfill silently
    // dropped the high byte.
    let r = dispatch(
        m(r#"export function test() {
            try {
                btoa("\u0100");
                return { threw: false };
            } catch (e) {
                return {
                    threw: true,
                    name: e.name,
                    isDom: e instanceof DOMException,
                };
            }
        }"#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains("\"threw\":true"), "got: {}", r.json);
    assert!(r.json.contains("\"name\":\"InvalidCharacterError\""), "got: {}", r.json);
    assert!(r.json.contains("\"isDom\":true"), "got: {}", r.json);
}

#[test]
fn atob_throws_on_invalid_chars() {
    // Per spec: atob with characters outside the base64 alphabet must
    // throw DOMException("InvalidCharacterError").
    let r = dispatch(
        m(r#"export function test() {
            try {
                atob("not!base64!");
                return { threw: false };
            } catch (e) {
                return {
                    threw: true,
                    name: e.name,
                    isDom: e instanceof DOMException,
                };
            }
        }"#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains("\"threw\":true"), "got: {}", r.json);
    assert!(r.json.contains("\"name\":\"InvalidCharacterError\""), "got: {}", r.json);
    assert!(r.json.contains("\"isDom\":true"), "got: {}", r.json);
}

#[test]
fn atob_strips_ascii_whitespace() {
    // Per spec: atob strips ASCII whitespace before validation/decoding.
    // The polyfill didn't, so this used to fail.
    let r = dispatch(
        m(r#"export function test() {
            return { decoded: atob("aGVs\nbG8=") };
        }"#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains("\"decoded\":\"hello\""), "got: {}", r.json);
}

#[test]
fn atob_handles_unpadded_input() {
    // "any" → "YW55" — already aligned to 4. "any!" → "YW55IQ==", and
    // "YW55IQ" without padding should also decode (forgiving base64).
    let r = dispatch(
        m(r#"export function test() {
            return {
                with_pad: atob("YW55IQ=="),
                without_pad: atob("YW55IQ"),
            };
        }"#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains("\"with_pad\":\"any!\""), "got: {}", r.json);
    assert!(r.json.contains("\"without_pad\":\"any!\""), "got: {}", r.json);
}

#[test]
fn btoa_handles_empty_string() {
    let r = dispatch(
        m(r#"export function test() {
            return { encoded: btoa(""), decoded: atob("") };
        }"#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains("\"encoded\":\"\""), "got: {}", r.json);
    assert!(r.json.contains("\"decoded\":\"\""), "got: {}", r.json);
}

#[test]
fn btoa_handles_latin1_range() {
    // 0xFF is the upper limit per spec. Each byte becomes one output byte.
    let r = dispatch(
        m(r#"export function test() {
            const s = String.fromCharCode(0xFF) + String.fromCharCode(0x80) + "A";
            const enc = btoa(s);
            const dec = atob(enc);
            return {
                encLen: enc.length,
                decLen: dec.length,
                roundTrip: dec === s,
            };
        }"#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains("\"roundTrip\":true"), "got: {}", r.json);
}

#[test]
fn atob_invalid_length_throws() {
    // Length % 4 == 1 after stripping padding/whitespace is illegal.
    let r = dispatch(
        m(r#"export function test() {
            try {
                atob("A");
                return { threw: false };
            } catch (e) {
                return { threw: true, name: e.name };
            }
        }"#),
        "test",
        "[]",
    )
    .unwrap();
    assert!(r.json.contains("\"threw\":true"), "got: {}", r.json);
    assert!(r.json.contains("\"name\":\"InvalidCharacterError\""), "got: {}", r.json);
}
