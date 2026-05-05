//! Smoke tests for the public `<Class>::is_instance(scope, value) ->
//! bool` brand-check entry point emitted alongside `<Class>::install`.
//!
//! Pre-extension, the macro emitted a private `__brand_check_<Class>`
//! that took `Local<Object>` and was usable only inside the
//! macro-generated method callbacks. Cross-class type queries (e.g.
//! "is this Local<Value> a Headers / Blob / FormData?" inside the
//! Request body coercion) had to hand-roll their own
//! `instance_of(globalThis.Foo)` walks — which break if the user
//! shadows the global, miss subclasses created via prototype-chaining,
//! and don't share the cache the in-class brand check uses.
//!
//! The MAC-12 extension (Wave 5c) exposes the typed
//! `<Class>::is_instance(scope, v) -> bool` method. Body delegates to
//! `__brand_check_<Class>` after a `Local::<Object>::try_from` gate so
//! non-Object values (primitives, null, undefined) return `false`
//! instead of UB.
//!
//! Wave 8: the legacy underscored `__zs_is_<Class>` shim was removed
//! per `crates/runtime-macros/STABILITY.md`. This test now exercises
//! `<Class>::is_instance` directly — same observable behaviour, the
//! only change is the call-site spelling.
//!
//! Coverage:
//!   - matches a real instance of the class
//!   - rejects a plain `{}` object (no class prototype on chain)
//!   - rejects an instance of a *different* `#[v8_class]`
//!   - rejects null / undefined / a primitive number / a string
#![allow(unsafe_code)]

use zeroship_runtime::init_v8;
#[allow(unused_imports)]
use zeroship_runtime_macros::{v8_class, v8_constructor, v8_method};

// ---------------------------------------------------------------------------
// Test harness — local copy.
// ---------------------------------------------------------------------------

fn run_in_v8<F, R>(
    install: impl FnOnce(&mut v8::PinScope, v8::Local<v8::Object>),
    src: &str,
    f: F,
) -> R
where
    F: FnOnce(v8::Local<v8::Value>, &mut v8::PinScope) -> R,
{
    init_v8();
    let mut isolate = v8::Isolate::new(v8::CreateParams::default());
    v8::scope!(let handle_scope, &mut isolate);
    let context = v8::Context::new(handle_scope, Default::default());
    let scope = &mut v8::ContextScope::new(handle_scope, context);
    let global = scope.get_current_context().global(scope);
    install(scope, global);

    let src_v8 = v8::String::new(scope, src).unwrap();
    let script = v8::Script::compile(scope, src_v8, None).unwrap();
    let result = script.run(scope).unwrap();
    f(result, scope)
}

fn install_class<'s, T>(
    install_fn: fn(&mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::FunctionTemplate>,
    name: &str,
    scope: &mut v8::PinScope<'s, '_>,
    global: v8::Local<v8::Object>,
) {
    let _ = std::marker::PhantomData::<T>;
    let tmpl = install_fn(scope);
    let class_fn = tmpl.get_function(scope).unwrap();
    let key = v8::String::new(scope, name).unwrap();
    global.set(scope, key.into(), class_fn.into());
}

// ---------------------------------------------------------------------------
// Two unrelated classes.
// ---------------------------------------------------------------------------

mod cls {
    use super::*;

    pub struct Alpha {
        pub n: u32,
    }

    #[v8_class]
    impl Alpha {
        #[v8_constructor]
        fn new() -> Alpha {
            Alpha { n: 7 }
        }

        #[v8_method]
        fn val(&self) -> u32 {
            self.n
        }
    }

    pub struct Beta;

    #[v8_class]
    impl Beta {
        #[v8_constructor]
        fn new() -> Beta {
            Beta
        }
    }
}

// ---------------------------------------------------------------------------
// Drive the public `<Class>::is_instance` from a dedicated registered op:
// the test installs an op `_test_is_alpha(value)` that calls the
// typed entry point and returns the boolean. JS code then exercises
// every shape we want to cover.
// ---------------------------------------------------------------------------

fn install_test_is_alpha(scope: &mut v8::PinScope, global: v8::Local<v8::Object>) {
    fn callback(
        scope: &mut v8::PinScope,
        args: v8::FunctionCallbackArguments,
        mut rv: v8::ReturnValue,
    ) {
        let v = args.get(0);
        let r = cls::Alpha::is_instance(scope, v);
        rv.set(v8::Boolean::new(scope, r).into());
    }
    let tmpl = v8::FunctionTemplate::new(scope, callback);
    let f = tmpl.get_function(scope).unwrap();
    let key = v8::String::new(scope, "_test_is_alpha").unwrap();
    global.set(scope, key.into(), f.into());
}

fn install_test_is_beta(scope: &mut v8::PinScope, global: v8::Local<v8::Object>) {
    fn callback(
        scope: &mut v8::PinScope,
        args: v8::FunctionCallbackArguments,
        mut rv: v8::ReturnValue,
    ) {
        let v = args.get(0);
        let r = cls::Beta::is_instance(scope, v);
        rv.set(v8::Boolean::new(scope, r).into());
    }
    let tmpl = v8::FunctionTemplate::new(scope, callback);
    let f = tmpl.get_function(scope).unwrap();
    let key = v8::String::new(scope, "_test_is_beta").unwrap();
    global.set(scope, key.into(), f.into());
}

fn install_all(scope: &mut v8::PinScope, global: v8::Local<v8::Object>) {
    install_class::<cls::Alpha>(cls::Alpha::install, "Alpha", scope, global);
    install_class::<cls::Beta>(cls::Beta::install, "Beta", scope, global);
    install_test_is_alpha(scope, global);
    install_test_is_beta(scope, global);
}

#[test]
fn matches_real_instance() {
    let r = run_in_v8(
        install_all,
        r#"
        const a = new Alpha();
        _test_is_alpha(a);
        "#,
        |val, scope| val.boolean_value(scope),
    );
    assert!(r);
}

#[test]
fn rejects_plain_object() {
    let r = run_in_v8(
        install_all,
        r#"
        _test_is_alpha({});
        "#,
        |val, scope| val.boolean_value(scope),
    );
    assert!(!r);
}

#[test]
fn rejects_other_class_instance() {
    let r = run_in_v8(
        install_all,
        r#"
        const b = new Beta();
        _test_is_alpha(b);
        "#,
        |val, scope| val.boolean_value(scope),
    );
    assert!(!r);
}

#[test]
fn matches_other_class_only_for_other_class() {
    let r = run_in_v8(
        install_all,
        r#"
        const a = new Alpha();
        const b = new Beta();
        JSON.stringify({
            a_is_a: _test_is_alpha(a),
            a_is_b: _test_is_beta(a),
            b_is_a: _test_is_alpha(b),
            b_is_b: _test_is_beta(b),
        });
        "#,
        |val, scope| val.to_rust_string_lossy(scope),
    );
    assert_eq!(
        r,
        r#"{"a_is_a":true,"a_is_b":false,"b_is_a":false,"b_is_b":true}"#
    );
}

#[test]
fn rejects_primitives() {
    let r = run_in_v8(
        install_all,
        r#"
        JSON.stringify({
            null_:  _test_is_alpha(null),
            undef:  _test_is_alpha(undefined),
            num:    _test_is_alpha(42),
            str:    _test_is_alpha("Alpha"),
            bool_:  _test_is_alpha(true),
        });
        "#,
        |val, scope| val.to_rust_string_lossy(scope),
    );
    assert_eq!(
        r,
        r#"{"null_":false,"undef":false,"num":false,"str":false,"bool_":false}"#
    );
}
