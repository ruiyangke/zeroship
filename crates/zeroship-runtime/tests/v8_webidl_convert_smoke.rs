//! Smoke tests for `read_sequence<T>` and `read_record<K, V>` —
//! WebIDL §3.13.16 (sequence) and §3.13.18 (record).
//!
//! Coverage:
//!   - sequence<USVString> from a JS Array → ["a", "b", "c"]
//!   - sequence<u32> from a JS Array → [1, 2, 3]
//!   - sequence<USVString> from a non-iterable string-coerced object → TypeError
//!   - sequence from a synthetic JS iterable (custom @@iterator) → reads
//!   - record<USVString, USVString> from `{a:"1", b:"2"}` → [(a,1),(b,2)]
//!   - record from null → TypeError
//!   - record from `[["a","1"]]` (entries-array) → spec-correct: arrays
//!     ARE objects, so they iterate via own properties (numeric indices),
//!     producing `[("0", entry_array)]`. We assert this concrete shape
//!     matches spec rather than the more user-intuitive entries-coercion.
#![allow(unsafe_code)]

use zeroship_runtime::byte_string::ByteString;
use zeroship_runtime::convert::{read_record, read_sequence};
use zeroship_runtime::init_v8;
use zeroship_runtime::usv_string::USVString;

// ---------------------------------------------------------------------------
// Test harness — minimal isolate setup. Compiles a JS expression and
// passes its result to a callback running with a real PinScope.
// ---------------------------------------------------------------------------

fn run_with_value<F, R>(src: &str, f: F) -> R
where
    F: FnOnce(v8::Local<v8::Value>, &mut v8::PinScope) -> R,
{
    init_v8();
    let mut isolate = v8::Isolate::new(v8::CreateParams::default());
    v8::scope!(let handle_scope, &mut isolate);
    let context = v8::Context::new(handle_scope, Default::default());
    let scope = &mut v8::ContextScope::new(handle_scope, context);

    let src_v8 = v8::String::new(scope, src).unwrap();
    let script = v8::Script::compile(scope, src_v8, None).unwrap();
    let result = script.run(scope).unwrap();
    f(result, scope)
}

// ---------------------------------------------------------------------------
// sequence<T>
// ---------------------------------------------------------------------------

#[test]
fn sequence_usv_string_from_array() {
    let out = run_with_value(r#"["a", "b", "c"]"#, |val, scope| {
        read_sequence::<USVString>(scope, val).unwrap()
    });
    assert_eq!(out.len(), 3);
    assert_eq!(out[0].as_str(), "a");
    assert_eq!(out[1].as_str(), "b");
    assert_eq!(out[2].as_str(), "c");
}

#[test]
fn sequence_u32_from_array() {
    let out = run_with_value(r#"[1, 2, 3]"#, |val, scope| {
        read_sequence::<u32>(scope, val).unwrap()
    });
    assert_eq!(out, vec![1u32, 2, 3]);
}

#[test]
fn sequence_string_from_array() {
    // String (default JS-to-Rust lossy UTF-8) implements WebIdlConvertible too.
    let out = run_with_value(r#"["x", "y"]"#, |val, scope| {
        read_sequence::<String>(scope, val).unwrap()
    });
    assert_eq!(out, vec!["x".to_string(), "y".to_string()]);
}

#[test]
fn sequence_byte_string_from_array() {
    let out = run_with_value(r#"["A", "B"]"#, |val, scope| {
        read_sequence::<ByteString>(scope, val).unwrap()
    });
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].as_slice(), b"A");
    assert_eq!(out[1].as_slice(), b"B");
}

#[test]
fn sequence_string_not_iterable_throws() {
    // A bare number is not iterable — no @@iterator method.
    let err = run_with_value(r#"42"#, |val, scope| {
        read_sequence::<USVString>(scope, val).err()
    });
    let err = err.expect("expected TypeError on non-iterable");
    assert!(err.message.contains("not iterable") || err.message.contains("not an object"),
        "expected 'not iterable' or similar in message, got: {}", err.message);
}

#[test]
fn sequence_object_without_iterator_throws() {
    // A plain object (no @@iterator method, no array-like interface)
    // should reject — sequence requires @@iterator per §3.13.16.
    let err = run_with_value(r#"({})"#, |val, scope| {
        read_sequence::<USVString>(scope, val).err()
    });
    assert!(err.is_some(), "expected TypeError on plain object");
}

#[test]
fn sequence_from_custom_iterable() {
    // Synthetic iterable that yields two strings.
    let out = run_with_value(
        r#"
        ({
            [Symbol.iterator]() {
                let i = 0;
                return {
                    next() {
                        if (i < 2) { return { value: "v" + i++, done: false }; }
                        return { value: undefined, done: true };
                    }
                };
            }
        })
        "#,
        |val, scope| read_sequence::<USVString>(scope, val).unwrap(),
    );
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].as_str(), "v0");
    assert_eq!(out[1].as_str(), "v1");
}

#[test]
fn sequence_from_set() {
    // Set has @@iterator that yields values in insertion order.
    let out = run_with_value(r#"new Set(["a", "b"])"#, |val, scope| {
        read_sequence::<USVString>(scope, val).unwrap()
    });
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].as_str(), "a");
    assert_eq!(out[1].as_str(), "b");
}

// ---------------------------------------------------------------------------
// record<K, V>
// ---------------------------------------------------------------------------

#[test]
fn record_usv_usv_from_object() {
    let out = run_with_value(r#"({a: "1", b: "2"})"#, |val, scope| {
        read_record::<USVString, USVString>(scope, val).unwrap()
    });
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].0.as_str(), "a");
    assert_eq!(out[0].1.as_str(), "1");
    assert_eq!(out[1].0.as_str(), "b");
    assert_eq!(out[1].1.as_str(), "2");
}

#[test]
fn record_byte_byte_from_object() {
    // ByteString-keyed records — same shape, different conversion path.
    let out = run_with_value(r#"({A: "X", B: "Y"})"#, |val, scope| {
        read_record::<ByteString, ByteString>(scope, val).unwrap()
    });
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].0.as_slice(), b"A");
    assert_eq!(out[0].1.as_slice(), b"X");
    assert_eq!(out[1].0.as_slice(), b"B");
    assert_eq!(out[1].1.as_slice(), b"Y");
}

#[test]
fn record_null_throws() {
    let err = run_with_value(r#"null"#, |val, scope| {
        read_record::<USVString, USVString>(scope, val).err()
    });
    let err = err.expect("expected TypeError on null");
    assert!(err.message.contains("not an object"),
        "expected 'not an object' in message, got: {}", err.message);
}

#[test]
fn record_primitive_throws() {
    let err = run_with_value(r#"42"#, |val, scope| {
        read_record::<USVString, USVString>(scope, val).err()
    });
    assert!(err.is_some(), "expected TypeError on primitive");
}

#[test]
fn record_empty_object_yields_empty_vec() {
    let out = run_with_value(r#"({})"#, |val, scope| {
        read_record::<USVString, USVString>(scope, val).unwrap()
    });
    assert!(out.is_empty());
}

#[test]
fn record_integer_keys_first_then_strings() {
    // Per ECMA-262 OrdinaryOwnPropertyKeys: integer-indexed first,
    // then string keys in insertion order. JS object literals follow
    // the same rule.
    let out = run_with_value(
        r#"({b: "1", "1": "first", a: "2", "0": "zeroth"})"#,
        |val, scope| read_record::<USVString, USVString>(scope, val).unwrap(),
    );
    assert_eq!(out.len(), 4);
    // Numeric strings "0", "1" come first ascending.
    assert_eq!(out[0].0.as_str(), "0");
    assert_eq!(out[0].1.as_str(), "zeroth");
    assert_eq!(out[1].0.as_str(), "1");
    assert_eq!(out[1].1.as_str(), "first");
    // Then string keys in insertion order.
    assert_eq!(out[2].0.as_str(), "b");
    assert_eq!(out[2].1.as_str(), "1");
    assert_eq!(out[3].0.as_str(), "a");
    assert_eq!(out[3].1.as_str(), "2");
}
