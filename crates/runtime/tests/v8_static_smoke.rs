//! Smoke tests for `#[v8_static_method]` / `#[v8_static_getter]` —
//! WebIDL §3.7.4 static operations and attributes.
//!
//! Static methods install on the CONSTRUCTOR FUNCTION (`Class.method`),
//! not the prototype. They have no receiver — no `&self` / `&mut self`
//! arg, no internal-field deref, no brand check, no re-entrancy guard.
//! The macro skips all that and emits a plain V8 callback that's
//! installed on the FunctionTemplate via `set_with_attr` (so it shows
//! up as a static property of the constructor).
//!
//! Pre-extension, 9 hand-rolled `install_static(scope, class_fn,
//! "name", cb)` sites peppered Response (`error/json/redirect`), URL
//! (`canParse/parse`), AbortSignal (`abort/timeout/any`), and
//! ReadableStream.from. Each emits the same boilerplate: build a
//! FunctionTemplate, get the function, set it as a property of the
//! constructor with `set` or `set_with_attr`. -200 LOC after migration.
//!
//! Coverage:
//!   - `Class.foo(args)` resolves to the static method
//!   - `(new Class()).foo` is undefined (not on the prototype)
//!   - Static getter resolves on the constructor; not on the instance
//!   - Mix of regular methods + statics in one class — both work
#![allow(unsafe_code)]

use zeroship_runtime::init_v8;
#[allow(unused_imports)]
use zeroship_runtime_macros::{
    v8_class, v8_constructor, v8_getter, v8_method, v8_static_getter, v8_static_method,
};

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

fn js_string(val: v8::Local<v8::Value>, scope: &mut v8::PinScope) -> String {
    val.to_rust_string_lossy(scope)
}

// ---------------------------------------------------------------------------
// Test class — a regular method, a static method, and a static getter.
// ---------------------------------------------------------------------------

mod cls {
    use super::*;

    pub struct Crate {
        pub n: u32,
    }

    #[v8_class]
    impl Crate {
        #[v8_constructor]
        fn new(n: Option<u32>) -> Crate {
            Crate { n: n.unwrap_or(7) }
        }

        // Regular instance method — appears on the prototype.
        #[v8_method]
        fn read(&self) -> u32 {
            self.n
        }

        // Static method — appears on Crate, NOT on the prototype.
        // Takes no `self`. Returns a string built from the args.
        #[v8_static_method]
        fn from(prefix: String, n: u32) -> String {
            format!("{prefix}-{n}")
        }

        // Static method with an early-return error path. Demonstrates
        // that Result<T, OpError> is handled the same as for instance
        // methods (no special divergence for statics).
        #[v8_static_method]
        fn divide(a: u32, b: u32) -> Result<u32, zeroship_runtime::state::OpError> {
            if b == 0 {
                return Err(zeroship_runtime::state::OpError::range_error(
                    "divide by zero",
                ));
            }
            Ok(a / b)
        }

        // Static getter — appears on Crate.DEFAULT_TIMEOUT, not on
        // (new Crate()).DEFAULT_TIMEOUT.
        #[v8_static_getter]
        #[allow(non_snake_case)]
        fn DEFAULT_TIMEOUT() -> u32 {
            5000
        }
    }
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[test]
fn static_method_callable_on_class() {
    let r = run_in_v8(
        |scope, global| {
            install_class::<cls::Crate>(cls::Crate::install, "Crate", scope, global);
        },
        r#"
        Crate.from("hi", 5);
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(r, "hi-5");
}

#[test]
fn static_method_not_on_instance() {
    // Per WebIDL §3.7.4, static operations are NOT installed on the
    // prototype. `(new Crate()).from` should resolve to undefined.
    let r = run_in_v8(
        |scope, global| {
            install_class::<cls::Crate>(cls::Crate::install, "Crate", scope, global);
        },
        r#"
        const b = new Crate();
        typeof b.from;
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(r, "undefined");
}

#[test]
fn static_method_not_on_prototype() {
    let r = run_in_v8(
        |scope, global| {
            install_class::<cls::Crate>(cls::Crate::install, "Crate", scope, global);
        },
        r#"
        typeof Crate.prototype.from;
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(r, "undefined");
}

#[test]
fn static_method_result_err_throws() {
    // `divide` returns Result<u32, OpError>; Err with RangeError kind
    // should surface as a thrown JS RangeError. Same shape as the
    // instance-method path — confirms statics share the standard
    // return-marshaling.
    let r = run_in_v8(
        |scope, global| {
            install_class::<cls::Crate>(cls::Crate::install, "Crate", scope, global);
        },
        r#"
        let kind, msg;
        try { Crate.divide(10, 0); }
        catch (e) { kind = e.constructor.name; msg = e.message; }
        JSON.stringify({ kind, msg });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(
        r,
        r#"{"kind":"RangeError","msg":"divide by zero"}"#
    );
}

#[test]
fn static_method_result_ok_returns() {
    let r = run_in_v8(
        |scope, global| {
            install_class::<cls::Crate>(cls::Crate::install, "Crate", scope, global);
        },
        r#"
        Crate.divide(10, 2);
        "#,
        |val, scope| val.uint32_value(scope).unwrap_or(0),
    );
    assert_eq!(r, 5);
}

#[test]
fn static_getter_visible_on_class() {
    let r = run_in_v8(
        |scope, global| {
            install_class::<cls::Crate>(cls::Crate::install, "Crate", scope, global);
        },
        r#"
        Crate.DEFAULT_TIMEOUT;
        "#,
        |val, scope| val.uint32_value(scope).unwrap_or(0),
    );
    assert_eq!(r, 5000);
}

#[test]
fn static_getter_not_on_instance() {
    let r = run_in_v8(
        |scope, global| {
            install_class::<cls::Crate>(cls::Crate::install, "Crate", scope, global);
        },
        r#"
        const b = new Crate();
        typeof b.DEFAULT_TIMEOUT;
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(r, "undefined");
}

#[test]
fn instance_method_still_works_with_statics() {
    // Sanity: a class with both regular and static surfaces both work
    // as expected.
    let r = run_in_v8(
        |scope, global| {
            install_class::<cls::Crate>(cls::Crate::install, "Crate", scope, global);
        },
        r#"
        const b = new Crate(42);
        JSON.stringify({
            inst_read: b.read(),
            cls_from: Crate.from("p", 1),
            cls_default: Crate.DEFAULT_TIMEOUT,
        });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(
        r,
        r#"{"inst_read":42,"cls_from":"p-1","cls_default":5000}"#
    );
}
