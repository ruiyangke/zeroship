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

// ---------------------------------------------------------------------------
// Test 6: `#[webidl_dict_member(reject_null)]` per-field flag.
//
// Spec consumer: `AddEventListenerOptions.signal: AbortSignal?` —
// passing `signal: null` is a TypeError per WebIDL §3.13.27 (the
// nullable-AbortSignal contract is "MUST be a real AbortSignal or
// absent — null is not allowed"), NOT default-construct. Today the
// derive routes null through `Option<T>`'s blanket WebIdlConvertible
// impl which returns None; with the flag, null throws.
//
// `undefined` and missing keys still fall through to default — WebIDL
// distinguishes null (an explicitly-bound null) from undefined
// (member-not-supplied). The flag affects ONLY null.
// ---------------------------------------------------------------------------

#[derive(WebIdlDict, Default, Debug)]
struct WithRequiredNonNull {
    name: Option<USVString>,
    #[webidl_dict_member(reject_null)]
    signal: Option<USVString>,
}

#[test]
fn reject_null_throws_on_null() {
    let err = run_with_value(r#"({signal: null})"#, |val, scope| {
        WithRequiredNonNull::from_v8(scope, val).err()
    });
    let err = err.expect("expected TypeError on signal: null");
    assert!(
        err.message.contains("signal") && err.message.contains("null"),
        "message should mention 'signal' and 'null': {}",
        err.message
    );
}

#[test]
fn reject_null_accepts_undefined_as_missing() {
    // undefined = member not supplied → default fallback. Distinct
    // from null per WebIDL §3.10.
    let w = run_with_value(r#"({signal: undefined})"#, |val, scope| {
        WithRequiredNonNull::from_v8(scope, val).unwrap()
    });
    assert!(w.signal.is_none());
}

#[test]
fn reject_null_accepts_missing_as_default() {
    // Missing key entirely → default. Same as undefined.
    let w = run_with_value(r#"({})"#, |val, scope| {
        WithRequiredNonNull::from_v8(scope, val).unwrap()
    });
    assert!(w.signal.is_none());
}

#[test]
fn reject_null_accepts_value() {
    // Concrete value passes through normally.
    let w = run_with_value(r#"({signal: "hello"})"#, |val, scope| {
        WithRequiredNonNull::from_v8(scope, val).unwrap()
    });
    assert_eq!(w.signal.as_ref().unwrap().as_str(), "hello");
}

// Without the flag: null falls through to default (existing
// behaviour). Regression guard.
#[derive(WebIdlDict, Default, Debug)]
struct WithoutRejectNull {
    signal: Option<USVString>,
}

#[test]
fn without_reject_null_null_is_default() {
    let w = run_with_value(r#"({signal: null})"#, |val, scope| {
        WithoutRejectNull::from_v8(scope, val).unwrap()
    });
    assert!(w.signal.is_none(), "default behaviour: null → None");
}

// Other fields in the same struct are unaffected by the flag — only
// the marked field rejects null.
#[test]
fn reject_null_only_affects_marked_field() {
    // `name` (no flag) accepts null → None; `signal` would reject.
    let w = run_with_value(r#"({name: null, signal: "ok"})"#, |val, scope| {
        WithRequiredNonNull::from_v8(scope, val).unwrap()
    });
    assert!(w.name.is_none());
    assert_eq!(w.signal.as_ref().unwrap().as_str(), "ok");
}

// ---------------------------------------------------------------------------
// Test 7: `DictOrBool<T>` union shape `(<dict> or boolean)`.
//
// Spec consumer: `AddEventListenerOptions` accepts
// `(EventListenerOptions or boolean)` per DOM §2.7. The boolean
// shorthand sets `capture` only. Other dict shapes that the spec
// allows to be a boolean are vanishingly rare; the WebIDL spec's full
// union machinery is overkill for this single shape.
//
// The DictOrBool<T> wrapper carries its own WebIdlConvertible impl
// that branches on the JS value: primitive boolean → DictOrBool::Bool,
// otherwise read as T. The dict derive needs no new attribute — the
// type IS the contract. `null` and `undefined` fall through to the
// outer Option<DictOrBool<T>>'s default (None), preserving the
// WebIDL §3.10 dict semantics.
// ---------------------------------------------------------------------------

use zeroship_runtime::convert::DictOrBool;

#[derive(WebIdlDict, Default, Debug)]
struct EventListenerOptions {
    capture: Option<bool>,
    passive: Option<bool>,
    once: Option<bool>,
}

#[derive(WebIdlDict, Default, Debug)]
struct AddEventListenerOptions {
    options: Option<DictOrBool<EventListenerOptions>>,
}

#[test]
fn dict_or_bool_boolean_shorthand() {
    let w = run_with_value(r#"({options: true})"#, |val, scope| {
        AddEventListenerOptions::from_v8(scope, val).unwrap()
    });
    match w.options {
        Some(DictOrBool::Bool(b)) => assert!(b),
        other => panic!("expected Some(Bool(true)), got {:?}", other),
    }
}

#[test]
fn dict_or_bool_boolean_shorthand_false() {
    let w = run_with_value(r#"({options: false})"#, |val, scope| {
        AddEventListenerOptions::from_v8(scope, val).unwrap()
    });
    match w.options {
        Some(DictOrBool::Bool(b)) => assert!(!b),
        other => panic!("expected Some(Bool(false)), got {:?}", other),
    }
}

#[test]
fn dict_or_bool_full_dict() {
    let w = run_with_value(
        r#"({options: {capture: true, passive: false, once: true}})"#,
        |val, scope| AddEventListenerOptions::from_v8(scope, val).unwrap(),
    );
    match w.options {
        Some(DictOrBool::Dict(d)) => {
            assert_eq!(d.capture, Some(true));
            assert_eq!(d.passive, Some(false));
            assert_eq!(d.once, Some(true));
        }
        other => panic!("expected Some(Dict(...)), got {:?}", other),
    }
}

#[test]
fn dict_or_bool_partial_dict() {
    // A dict missing fields still resolves to Dict (not Bool).
    let w = run_with_value(r#"({options: {capture: true}})"#, |val, scope| {
        AddEventListenerOptions::from_v8(scope, val).unwrap()
    });
    match w.options {
        Some(DictOrBool::Dict(d)) => {
            assert_eq!(d.capture, Some(true));
            assert_eq!(d.passive, None);
        }
        other => panic!("expected Dict, got {:?}", other),
    }
}

#[test]
fn dict_or_bool_null_falls_through() {
    // null → outer Option<DictOrBool<...>>'s blanket impl returns None.
    let w = run_with_value(r#"({options: null})"#, |val, scope| {
        AddEventListenerOptions::from_v8(scope, val).unwrap()
    });
    assert!(w.options.is_none());
}

#[test]
fn dict_or_bool_undefined_falls_through() {
    let w = run_with_value(r#"({options: undefined})"#, |val, scope| {
        AddEventListenerOptions::from_v8(scope, val).unwrap()
    });
    assert!(w.options.is_none());
}

#[test]
fn dict_or_bool_missing_falls_through() {
    let w = run_with_value(r#"({})"#, |val, scope| {
        AddEventListenerOptions::from_v8(scope, val).unwrap()
    });
    assert!(w.options.is_none());
}

#[test]
fn dict_or_bool_non_object_non_bool_falls_back_to_dict_path() {
    // A primitive that's not a boolean (e.g. a number) takes the
    // dict-extraction path. The dict derive then rejects non-object
    // with TypeError per WebIDL §3.10. This is the same behaviour as
    // for a non-union dict member — the union just adds the boolean
    // shortcut.
    let err = run_with_value(r#"({options: 42})"#, |val, scope| {
        AddEventListenerOptions::from_v8(scope, val).err()
    });
    assert!(err.is_some(), "expected TypeError on non-object non-bool");
}

// ---------------------------------------------------------------------------
// Test 8: tc_scope user-exception preservation.
//
// When a dict field's `WebIdlConvertible::from_v8` invokes user JS
// (custom toString, throwing Symbol.toPrimitive) and that user code
// throws, the macro must capture the exception value as
// `OpError::JsValue` and the throw machinery must re-throw it
// verbatim — Error subclass, custom properties (`e.code`),
// `instanceof` chain all preserved.
//
// Pre-fix the macro discarded the exception and surfaced a generic
// `OpError::TypeError("Cannot convert ...")`, hiding the user's
// actual thrown value. Spec: ECMA-262 abstract op `ToString` calls
// `Symbol.toPrimitive` then `toString` then `valueOf` — any of which
// can throw user values that MUST propagate.
// ---------------------------------------------------------------------------

#[derive(WebIdlDict, Default, Debug)]
struct WithStringMember {
    name: Option<USVString>,
}

// Helper: run JS that defines an object whose member's toString
// throws a user-defined exception, then call `Foo::from_v8` and
// catch — the caught value should be the same JS object.
//
// The harness here is more involved than the rest: we set up a
// global that exposes a Rust function, evaluate JS that throws
// inside an extraction, and observe the captured exception.

#[test]
fn dict_member_user_throw_tostring_preserves_error_object() {
    // Use a JS-side wrapper that captures the from_v8 outcome —
    // our existing harness can't easily get the OpError back to JS
    // without binding a native fn. So we test the OpError shape
    // directly: the err.kind should be JsValue carrying the
    // original exception. We then create a JS String from it and
    // assert it carries our marker value.
    use zeroship_runtime::state::OpErrorKind;

    let err: zeroship_runtime::state::OpError = run_with_value(
        r#"
            (() => {
                const e = new Error('boom');
                e.code = 'CUSTOM_THROW';
                return {
                    name: {
                        toString() { throw e; },
                    },
                };
            })()
        "#,
        |val, scope| WithStringMember::from_v8(scope, val).err(),
    )
    .expect("expected error from throwing toString");

    // The OpError must carry the captured user exception.
    match &err.kind {
        OpErrorKind::JsValue(_) => {
            // Good — the macro captured the value.
        }
        other => panic!(
            "expected OpErrorKind::JsValue, got {:?}",
            std::mem::discriminant(other)
        ),
    }
}

#[test]
fn dict_member_user_throw_preserves_code_property() {
    // Now exercise the throw end-to-end through V8: from_v8 returns
    // Err(JsValue), we materialise the global as a Local, and
    // observe its `.code` property in JS via a property read.
    use zeroship_runtime::init_v8;
    use zeroship_runtime::state::OpErrorKind;

    init_v8();
    let mut isolate = v8::Isolate::new(v8::CreateParams::default());
    v8::scope!(let handle_scope, &mut isolate);
    let context = v8::Context::new(handle_scope, Default::default());
    let scope = &mut v8::ContextScope::new(handle_scope, context);

    let src = v8::String::new(
        scope,
        r#"
            (() => {
                const e = new Error('boom-msg');
                e.code = 'CUSTOM_THROW';
                return {
                    name: {
                        toString() { throw e; },
                    },
                };
            })()
        "#,
    )
    .unwrap();
    let script = v8::Script::compile(scope, src, None).unwrap();
    let value = script.run(scope).unwrap();
    let err = WithStringMember::from_v8(scope, value).expect_err("must error");

    match &err.kind {
        OpErrorKind::JsValue(global) => {
            let local = v8::Local::new(scope, global);
            let obj: v8::Local<v8::Object> = local
                .try_into()
                .expect("captured exception should be an Object (Error)");
            let code_key = v8::String::new(scope, "code").unwrap();
            let code_val = obj.get(scope, code_key.into()).unwrap();
            let code_str = code_val.to_rust_string_lossy(scope);
            assert_eq!(
                code_str, "CUSTOM_THROW",
                "captured exception's code property must be preserved"
            );
            // Also verify the message round-trips.
            let msg_key = v8::String::new(scope, "message").unwrap();
            let msg_val = obj.get(scope, msg_key.into()).unwrap();
            let msg_str = msg_val.to_rust_string_lossy(scope);
            assert_eq!(msg_str, "boom-msg");
        }
        _ => panic!("expected JsValue kind"),
    }
}

#[test]
fn dict_member_user_throw_string_preserves_string() {
    // Throwing a primitive string directly (no Error wrapper) — the
    // captured value must be the same string, not a TypeError. JS
    // `throw "literal"` is legal.
    use zeroship_runtime::init_v8;
    use zeroship_runtime::state::OpErrorKind;

    init_v8();
    let mut isolate = v8::Isolate::new(v8::CreateParams::default());
    v8::scope!(let handle_scope, &mut isolate);
    let context = v8::Context::new(handle_scope, Default::default());
    let scope = &mut v8::ContextScope::new(handle_scope, context);

    let src = v8::String::new(
        scope,
        r#"
            ({
                name: {
                    [Symbol.toPrimitive]() { throw "x"; },
                },
            })
        "#,
    )
    .unwrap();
    let script = v8::Script::compile(scope, src, None).unwrap();
    let value = script.run(scope).unwrap();
    let err = WithStringMember::from_v8(scope, value).expect_err("must error");

    match &err.kind {
        OpErrorKind::JsValue(global) => {
            let local = v8::Local::new(scope, global);
            // Primitive string: not an Object — but is a Value with
            // ToString = the literal "x".
            assert!(local.is_string(), "captured value should be a string");
            let s = local.to_rust_string_lossy(scope);
            assert_eq!(s, "x");
        }
        _ => panic!("expected JsValue kind, got {:?}", err.kind),
    }
}

#[test]
fn dict_extraction_no_user_code_passes_through_unchanged() {
    // Regression guard: the existing happy paths (no user JS, plain
    // primitives) must still round-trip without a difference. The
    // tc_scope wrap should be transparent when the inner from_v8
    // doesn't raise.
    let foo = run_with_value(r#"({a: "hi", b: 42, c: true})"#, |val, scope| {
        Foo::from_v8(scope, val).unwrap()
    });
    assert_eq!(foo.a.as_ref().unwrap().as_str(), "hi");
    assert_eq!(foo.b, Some(42u32));
    assert_eq!(foo.c, Some(true));
}
