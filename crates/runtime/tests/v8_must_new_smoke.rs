//! Smoke test for `#[v8_class]` constructor must-new check.
//!
//! WebIDL §3.7.1: every interface constructor MUST be invoked with
//! `new`. Without the macro check, `Foo()` (without new) would call
//! the constructor callback with `args.this()` set to `globalThis` (or
//! to the function itself in strict mode); `set_internal_field` would
//! either no-op or panic depending on the wrapper's shape, and any
//! subsequent prototype method call would either UB-deref (if the
//! receiver had a slot) or throw the existing "Illegal invocation"
//! error. WPT fails across `event_target`, `blob_native`, `abort`
//! tests for this reason.
//!
//! Coverage:
//!   - `new Foo()` works (the existing path)
//!   - `Foo()` (without new) throws TypeError with the per-class message
//!   - The same shape applies to a default-constructed class
//!     (no `#[v8_constructor]` provided)
#![allow(unsafe_code)]

use zeroship_runtime::init_v8;
#[allow(unused_imports)]
use zeroship_runtime_macros::{v8_class, v8_constructor, v8_getter, v8_method};

// ---------------------------------------------------------------------------
// Test harness — copied from v8_class_smoke.rs to keep tests independent.
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

fn js_string(val: v8::Local<v8::Value>, scope: &mut v8::PinScope) -> String {
    val.to_rust_string_lossy(scope)
}

// ---------------------------------------------------------------------------
// Test class with explicit constructor.
// ---------------------------------------------------------------------------

mod explicit_ctor {
    use super::*;

    pub struct Widget {
        pub n: u32,
    }

    #[v8_class]
    impl Widget {
        #[v8_constructor]
        fn new(n: Option<u32>) -> Widget {
            Widget { n: n.unwrap_or(7) }
        }

        #[v8_getter]
        fn n(&self) -> u32 {
            self.n
        }
    }
}

#[test]
fn must_new_with_new_succeeds() {
    let r = run_in_v8(
        |scope, global| {
            install_class::<explicit_ctor::Widget>(
                explicit_ctor::Widget::install,
                "Widget",
                scope,
                global,
            )
        },
        r#"
        const w = new Widget(11);
        w.n;
        "#,
        |val, scope| val.uint32_value(scope).unwrap(),
    );
    assert_eq!(r, 11);
}

#[test]
fn must_new_without_new_throws_typeerror() {
    let s = run_in_v8(
        |scope, global| {
            install_class::<explicit_ctor::Widget>(
                explicit_ctor::Widget::install,
                "Widget",
                scope,
                global,
            )
        },
        r#"
        let kind, msg;
        try {
            // `Widget(7)` — no `new`, must throw TypeError per WebIDL §3.7.1.
            Widget(7);
        } catch (e) {
            kind = e.constructor.name;
            msg  = e.message;
        }
        JSON.stringify({ kind, msg });
        "#,
        |val, scope| js_string(val, scope),
    );
    // The class name is interpolated into the message so WPT can
    // diagnose mistakes per-class.
    assert_eq!(
        s,
        r#"{"kind":"TypeError","msg":"Failed to construct 'Widget': Please use the 'new' operator, this DOM object constructor cannot be called as a function."}"#
    );
}

#[test]
fn must_new_function_call_via_call_throws() {
    // `.call(this, ...)` is also not a construct call — V8's
    // `is_construct_call` returns false. Belt-and-suspenders coverage
    // since this is exactly how `Foo.prototype.method.call(otherFoo)`
    // would otherwise smuggle past the check.
    let s = run_in_v8(
        |scope, global| {
            install_class::<explicit_ctor::Widget>(
                explicit_ctor::Widget::install,
                "Widget",
                scope,
                global,
            )
        },
        r#"
        let kind;
        try {
            Widget.call({}, 5);
        } catch (e) {
            kind = e.constructor.name;
        }
        kind;
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "TypeError");
}

// ---------------------------------------------------------------------------
// Default-constructed class: no `#[v8_constructor]` on the impl block.
// The macro emits a default-call constructor; the must-new guard must
// fire there too.
// ---------------------------------------------------------------------------

mod default_ctor {
    use super::*;

    #[derive(Default)]
    pub struct Bare {
        pub _seen: bool,
    }

    #[v8_class]
    impl Bare {
        #[v8_method]
        fn touch(&mut self) -> bool {
            self._seen = true;
            self._seen
        }
    }
}

#[test]
fn must_new_default_ctor_with_new_succeeds() {
    let r = run_in_v8(
        |scope, global| {
            install_class::<default_ctor::Bare>(
                default_ctor::Bare::install,
                "Bare",
                scope,
                global,
            )
        },
        r#"
        const b = new Bare();
        b.touch();
        "#,
        |val, scope| val.boolean_value(scope),
    );
    assert!(r);
}

#[test]
fn must_new_default_ctor_without_new_throws() {
    let s = run_in_v8(
        |scope, global| {
            install_class::<default_ctor::Bare>(
                default_ctor::Bare::install,
                "Bare",
                scope,
                global,
            )
        },
        r#"
        let kind;
        try { Bare(); } catch (e) { kind = e.constructor.name; }
        kind;
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "TypeError");
}
