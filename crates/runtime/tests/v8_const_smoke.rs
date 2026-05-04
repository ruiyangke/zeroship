//! Smoke tests for `#[v8_const(NAME = LIT)]` — WebIDL §3.7.5
//! interface constants.
//!
//! Per WebIDL §3.7.5, a `const NAME = VALUE;` declaration on an
//! interface installs the value at BOTH the constructor function
//! (`Class.NAME`) AND the prototype (`Class.prototype.NAME`), with
//! the property descriptor `{ writable: false, enumerable: true,
//! configurable: false }` (read-only / non-configurable; the only
//! visibility flag is enumerable, which is true).
//!
//! Pre-extension, classes like DOMException had a hand-rolled
//! `&[(&str, u16)]` table + a manual install loop. The macro
//! attribute lifts that into a repeatable impl-block-level shape:
//!
//! ```ignore
//! #[v8_class]
//! #[v8_const(SYNTAX_ERR = 12u16)]
//! #[v8_const(NETWORK_ERR = 19u16)]
//! impl DOMException { ... }
//! ```
//!
//! Type tag is inferred from the literal suffix (`u16` / `u32` /
//! `i32`). Other types are rejected with a syn error.
//!
//! Coverage:
//!   - constants visible on the constructor function (`Class.NAME`)
//!   - constants visible on the prototype (`Class.prototype.NAME`)
//!   - constants visible on instances (via prototype lookup)
//!   - read-only: assignment in strict mode throws TypeError
//!   - inherited classes (via `#[v8_inherit]`) see parent constants
//!   - mixed types in one class (u16 + i32 + u32)
#![allow(unsafe_code)]

use zeroship_runtime::init_v8;
#[allow(unused_imports)]
use zeroship_runtime_macros::{v8_class, v8_const, v8_constructor, v8_method};

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
// Three constants, mixed types.
// ---------------------------------------------------------------------------

mod three_consts {
    use super::*;

    pub struct Codes;

    #[v8_class]
    #[v8_const(LO = 1u16)]
    #[v8_const(MID = 100i32)]
    #[v8_const(HI = 1000u32)]
    impl Codes {
        #[v8_constructor]
        fn new() -> Codes {
            Codes
        }
    }
}

#[test]
fn constants_on_constructor() {
    let r = run_in_v8(
        |scope, global| {
            install_class::<three_consts::Codes>(three_consts::Codes::install, "Codes", scope, global);
        },
        r#"
        JSON.stringify({
            lo: Codes.LO,
            mid: Codes.MID,
            hi: Codes.HI,
        });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(r, r#"{"lo":1,"mid":100,"hi":1000}"#);
}

#[test]
fn constants_on_prototype() {
    let r = run_in_v8(
        |scope, global| {
            install_class::<three_consts::Codes>(three_consts::Codes::install, "Codes", scope, global);
        },
        r#"
        JSON.stringify({
            lo: Codes.prototype.LO,
            mid: Codes.prototype.MID,
            hi: Codes.prototype.HI,
        });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(r, r#"{"lo":1,"mid":100,"hi":1000}"#);
}

#[test]
fn constants_on_instance() {
    // Per spec, constants on the prototype are visible on instances
    // via the prototype chain — `(new Codes).LO === 1`.
    let r = run_in_v8(
        |scope, global| {
            install_class::<three_consts::Codes>(three_consts::Codes::install, "Codes", scope, global);
        },
        r#"
        const c = new Codes();
        JSON.stringify({ lo: c.LO, mid: c.MID, hi: c.HI });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(r, r#"{"lo":1,"mid":100,"hi":1000}"#);
}

#[test]
fn constant_is_read_only_strict_mode() {
    // Per WebIDL §3.7.5, constants are non-writable. In strict mode
    // an assignment should throw TypeError.
    let r = run_in_v8(
        |scope, global| {
            install_class::<three_consts::Codes>(three_consts::Codes::install, "Codes", scope, global);
        },
        r#"
        "use strict";
        let kind;
        try {
            Codes.LO = 999;
        } catch (e) {
            kind = e.constructor.name;
        }
        kind ?? "no-throw";
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(r, "TypeError");
}

#[test]
fn constant_is_read_only_on_prototype_strict_mode() {
    // Same check but writing through the prototype slot.
    let r = run_in_v8(
        |scope, global| {
            install_class::<three_consts::Codes>(three_consts::Codes::install, "Codes", scope, global);
        },
        r#"
        "use strict";
        let kind;
        try {
            Codes.prototype.LO = 999;
        } catch (e) {
            kind = e.constructor.name;
        }
        kind ?? "no-throw";
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(r, "TypeError");
}

// ---------------------------------------------------------------------------
// Inherited constants: a derived class via #[v8_inherit] sees the
// parent's constants on its instance via the prototype chain.
// ---------------------------------------------------------------------------

mod inherit_consts {
    use super::*;

    pub struct Base;

    #[v8_class]
    #[v8_const(LIMIT = 42u16)]
    impl Base {
        #[v8_constructor]
        fn new() -> Base {
            Base
        }
    }

    pub struct Derived;

    #[v8_class]
    #[v8_inherit(Base)]
    impl Derived {
        #[v8_constructor]
        fn new() -> Derived {
            Derived
        }
    }
}

#[test]
fn inherited_class_sees_parent_constants() {
    let r = run_in_v8(
        |scope, global| {
            install_class::<inherit_consts::Base>(inherit_consts::Base::install, "Base", scope, global);
            install_class::<inherit_consts::Derived>(
                inherit_consts::Derived::install,
                "Derived",
                scope,
                global,
            );
        },
        r#"
        const d = new Derived();
        // Constants live on Base.prototype, which is on Derived's chain.
        JSON.stringify({
            inst: d.LIMIT,
            on_derived_proto: Derived.prototype.LIMIT,
            on_derived_ctor: Derived.LIMIT,
        });
        "#,
        |val, scope| js_string(val, scope),
    );
    // The constructor function `Derived.LIMIT` does NOT inherit from
    // the parent's constructor function (constructor functions don't
    // share a prototype chain by default). The instance and prototype
    // do — that's the standard JS prototype-chain behaviour.
    let parsed: serde_json::Value = serde_json::from_str(&r).unwrap();
    assert_eq!(parsed["inst"], 42);
    assert_eq!(parsed["on_derived_proto"], 42);
    // on_derived_ctor may be undefined (constructor inheritance is not
    // by-prototype-of-prototype; it's a separate chain). Either undefined
    // or absent is fine for this assertion — we only exercise the
    // instance and prototype paths above.
}
