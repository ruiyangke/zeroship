//! Smoke tests for `#[derive(WebIdlDict)]` — WebIDL §3.10 dictionary
//! parsing.
//!
//! Coverage:
//!   - `{}` → all fields default
//!   - `{a:"hi", b:42, c:true}` → fields populated
//!   - `{a:"x"}` → only `a` populated, others default
//!   - `null` → all defaults
//!   - `undefined` → all defaults
//!   - `42` (primitive) → TypeError
//!   - `{a:Symbol()}` → TypeError on member coercion (Symbol cannot
//!      ToString)
//!   - `#[webidl_name = "..."]` rename works
//!   - Nested dict (dict-as-member) reads recursively
//!   - Local<Value> field carries raw V8 handle through
#![allow(unsafe_code)]

use zeroship_runtime::init_v8;
use zeroship_runtime::usv_string::USVString;
use zeroship_runtime_macros::WebIdlDict;

// ---------------------------------------------------------------------------
// Test harness
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
// Test 1: simple dict with three primitive members
// ---------------------------------------------------------------------------

#[derive(WebIdlDict, Default, Debug)]
struct Foo {
    a: Option<USVString>,
    b: Option<u32>,
    c: Option<bool>,
}

#[test]
fn empty_object_yields_all_defaults() {
    let foo = run_with_value(r#"({})"#, |val, scope| Foo::from_v8(scope, val).unwrap());
    assert!(foo.a.is_none());
    assert!(foo.b.is_none());
    assert!(foo.c.is_none());
}

#[test]
fn full_object_populates_all_fields() {
    let foo = run_with_value(r#"({a: "hi", b: 42, c: true})"#, |val, scope| {
        Foo::from_v8(scope, val).unwrap()
    });
    assert_eq!(foo.a.as_ref().unwrap().as_str(), "hi");
    assert_eq!(foo.b, Some(42u32));
    assert_eq!(foo.c, Some(true));
}

#[test]
fn partial_object_leaves_missing_at_default() {
    let foo = run_with_value(r#"({a: "x"})"#, |val, scope| {
        Foo::from_v8(scope, val).unwrap()
    });
    assert_eq!(foo.a.as_ref().unwrap().as_str(), "x");
    assert!(foo.b.is_none());
    assert!(foo.c.is_none());
}

#[test]
fn null_yields_all_defaults() {
    let foo = run_with_value(r#"null"#, |val, scope| Foo::from_v8(scope, val).unwrap());
    assert!(foo.a.is_none());
    assert!(foo.b.is_none());
    assert!(foo.c.is_none());
}

#[test]
fn undefined_yields_all_defaults() {
    let foo = run_with_value(r#"undefined"#, |val, scope| Foo::from_v8(scope, val).unwrap());
    assert!(foo.a.is_none());
    assert!(foo.b.is_none());
    assert!(foo.c.is_none());
}

#[test]
fn primitive_throws_typeerror() {
    let err = run_with_value(r#"42"#, |val, scope| Foo::from_v8(scope, val).err());
    let err = err.expect("expected TypeError on primitive");
    assert!(
        err.message.contains("not an object") || err.message.contains("dictionary"),
        "unexpected message: {}",
        err.message
    );
}

#[test]
fn boolean_primitive_throws_typeerror() {
    let err = run_with_value(r#"true"#, |val, scope| Foo::from_v8(scope, val).err());
    assert!(err.is_some(), "expected TypeError on bool primitive");
}

#[test]
fn member_coercion_propagates_typeerror() {
    // Symbol value for `a` (USVString member) — Symbol cannot ToString,
    // so the inner converter throws and the dict-from_v8 propagates.
    let err = run_with_value(r#"({a: Symbol()})"#, |val, scope| {
        Foo::from_v8(scope, val).err()
    });
    assert!(err.is_some(), "expected TypeError on Symbol member");
}

// ---------------------------------------------------------------------------
// Test 2: #[webidl_name] rename — Rust field `r#type` → JS-side "type".
// ---------------------------------------------------------------------------

#[derive(WebIdlDict, Default, Debug)]
struct Renamed {
    #[webidl_name = "type"]
    type_field: Option<USVString>,
}

#[test]
fn webidl_name_rename_reads_from_js_name() {
    let r = run_with_value(r#"({type: "module"})"#, |val, scope| {
        Renamed::from_v8(scope, val).unwrap()
    });
    assert_eq!(r.type_field.as_ref().unwrap().as_str(), "module");
}

#[test]
fn webidl_name_rename_ignores_rust_name() {
    // Setting the Rust field name on the JS side does NOT match.
    let r = run_with_value(r#"({type_field: "module"})"#, |val, scope| {
        Renamed::from_v8(scope, val).unwrap()
    });
    assert!(r.type_field.is_none());
}

// ---------------------------------------------------------------------------
// Test 3: nested dict — Inner dict as Outer member, exercises
// recursive WebIdlConvertible (the derive emits both inherent
// `from_v8` AND a blanket `WebIdlConvertible` impl).
// ---------------------------------------------------------------------------

#[derive(WebIdlDict, Default, Debug)]
struct Inner {
    name: Option<USVString>,
    count: Option<u32>,
}

#[derive(WebIdlDict, Default, Debug)]
struct Outer {
    label: Option<USVString>,
    inner: Option<Inner>,
}

#[test]
fn nested_dict_reads_recursively() {
    let outer = run_with_value(
        r#"({label: "outer-1", inner: {name: "x", count: 7}})"#,
        |val, scope| Outer::from_v8(scope, val).unwrap(),
    );
    assert_eq!(outer.label.as_ref().unwrap().as_str(), "outer-1");
    let inner = outer.inner.as_ref().expect("inner should be Some");
    assert_eq!(inner.name.as_ref().unwrap().as_str(), "x");
    assert_eq!(inner.count, Some(7u32));
}

#[test]
fn nested_dict_missing_yields_none() {
    // Outer has no `inner` key → the `Option<Inner>` field defaults to
    // `None` (via Option's Default impl). Inner is NOT recursively
    // default-constructed because Option::default() = None.
    let outer = run_with_value(r#"({label: "alone"})"#, |val, scope| {
        Outer::from_v8(scope, val).unwrap()
    });
    assert_eq!(outer.label.as_ref().unwrap().as_str(), "alone");
    assert!(outer.inner.is_none());
}

#[test]
fn nested_dict_null_member_yields_default() {
    // `inner: null` — the Option<Inner>'s WebIdlConvertible blanket
    // impl returns None for null.
    let outer = run_with_value(r#"({inner: null})"#, |val, scope| {
        Outer::from_v8(scope, val).unwrap()
    });
    assert!(outer.inner.is_none());
}

// ---------------------------------------------------------------------------
// Test 4: bool / numeric defaults on bare (non-Option) members.
// ---------------------------------------------------------------------------

#[derive(WebIdlDict, Default, Debug)]
struct WithBool {
    keepalive: bool,
    count: u32,
}

#[test]
fn bool_default_is_false_and_u32_default_is_zero() {
    let w = run_with_value(r#"({})"#, |val, scope| {
        WithBool::from_v8(scope, val).unwrap()
    });
    assert!(!w.keepalive);
    assert_eq!(w.count, 0u32);
}

#[test]
fn bool_field_reads_explicit_value() {
    let w = run_with_value(r#"({keepalive: true, count: 100})"#, |val, scope| {
        WithBool::from_v8(scope, val).unwrap()
    });
    assert!(w.keepalive);
    assert_eq!(w.count, 100u32);
}

// ---------------------------------------------------------------------------
// Test 5: undefined-valued property is treated as missing (per spec).
// ---------------------------------------------------------------------------

#[test]
fn undefined_member_is_treated_as_missing() {
    let foo = run_with_value(r#"({a: undefined, b: 5})"#, |val, scope| {
        Foo::from_v8(scope, val).unwrap()
    });
    assert!(foo.a.is_none(), "undefined-valued member should default");
    assert_eq!(foo.b, Some(5u32));
}
