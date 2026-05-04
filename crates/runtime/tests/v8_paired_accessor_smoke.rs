//! Smoke tests for **same-name getter+setter pairing** via `#[v8_name]`.
//!
//! Background: defining `#[v8_getter] fn value(&self)` AND
//! `#[v8_setter] fn value(&mut self, v: ...)` in the same impl block is
//! illegal Rust (duplicate method names). The macro accepts the
//! workaround pattern: rename the Rust methods and apply
//! `#[v8_name = "value"]` to both.
//!
//! ```ignore
//! #[v8_getter]
//! #[v8_name = "value"]
//! fn get_value(&self) -> u32 { ... }
//!
//! #[v8_setter]
//! #[v8_name = "value"]
//! fn set_value(&mut self, v: u32) { ... }
//! ```
//!
//! The install codegen pairs them by their JS-visible name into a single
//! `set_accessor_property("value", getter_cb, setter_cb)` call. V8
//! installs both halves on the same property descriptor — without
//! pairing, two separate calls each overwrite the previous accessor and
//! one half is lost (or, worse, V8 rejects a setter-only / getter-only
//! re-install on a property that already has the other half installed).
//!
//! Coverage:
//!   - Paired getter+setter on a single JS name — get/set roundtrip.
//!   - Lone getter via `#[v8_name = "x"]` — read works, set is silently
//!     ignored (V8's no-setter semantics).
//!   - Lone setter via `#[v8_name = "y"]` — write works, read returns
//!     undefined.
//!   - A class with BOTH a paired pair AND an unrelated single getter —
//!     proves the keying is correct (no cross-talk between names).
//!   - Brand check still throws across the paired accessor: calling
//!     `Foo.prototype.value` getter on `{}` throws TypeError.
#![allow(unsafe_code)]

use std::cell::Cell;

use zeroship_runtime::init_v8;
#[allow(unused_imports)]
use zeroship_runtime_macros::{v8_class, v8_constructor, v8_getter, v8_method, v8_name, v8_setter};

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
// Class with a paired getter+setter on the JS name "value".
// ---------------------------------------------------------------------------

mod paired {
    use super::*;

    pub struct Box_ {
        pub value: Cell<u32>,
    }

    #[v8_class]
    impl Box_ {
        #[v8_constructor]
        fn new() -> Box_ {
            Box_ {
                value: Cell::new(0),
            }
        }

        // Paired pair: both apply to the JS-visible name "value".
        #[v8_getter]
        #[v8_name = "value"]
        fn get_value(&self) -> u32 {
            self.value.get()
        }

        #[v8_setter]
        #[v8_name = "value"]
        fn set_value(&self, v: u32) {
            self.value.set(v);
        }
    }
}

#[test]
fn paired_getter_setter_roundtrip() {
    let s = run_in_v8(
        |scope, global| {
            install_class::<paired::Box_>(paired::Box_::install, "Box", scope, global);
        },
        r#"
        const b = new Box();
        const r0 = b.value;
        b.value = 42;
        const r1 = b.value;
        b.value = 7;
        const r2 = b.value;
        `${r0},${r1},${r2}`;
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "0,42,7");
}

#[test]
fn paired_accessor_brand_check_throws() {
    // Reading the paired-accessor descriptor's getter on a non-Box
    // receiver must throw "Illegal invocation" (synchronously, before
    // any internal-field deref).
    let s = run_in_v8(
        |scope, global| {
            install_class::<paired::Box_>(paired::Box_::install, "Box", scope, global);
        },
        r#"
        const desc = Object.getOwnPropertyDescriptor(Box.prototype, "value");
        let kind = "no-throw";
        try { desc.get.call({}); }
        catch (e) { kind = "type-error:" + e.name; }
        kind;
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "type-error:TypeError");
}

#[test]
fn paired_accessor_setter_brand_check_throws() {
    // Symmetric: the setter half must also brand-check.
    let s = run_in_v8(
        |scope, global| {
            install_class::<paired::Box_>(paired::Box_::install, "Box", scope, global);
        },
        r#"
        const desc = Object.getOwnPropertyDescriptor(Box.prototype, "value");
        let kind = "no-throw";
        try { desc.set.call({}, 99); }
        catch (e) { kind = "type-error:" + e.name; }
        kind;
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "type-error:TypeError");
}

#[test]
fn paired_accessor_descriptor_has_both_halves() {
    // Confirms a single property descriptor was installed with both
    // get and set, rather than two separate accessor installs each
    // overwriting the other.
    let s = run_in_v8(
        |scope, global| {
            install_class::<paired::Box_>(paired::Box_::install, "Box", scope, global);
        },
        r#"
        const desc = Object.getOwnPropertyDescriptor(Box.prototype, "value");
        // Accessor descriptors expose `get` and `set` (Functions or
        // undefined). A paired install puts both in the same descriptor.
        const hasGet = typeof desc.get === "function";
        const hasSet = typeof desc.set === "function";
        `${hasGet},${hasSet}`;
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "true,true");
}

// ---------------------------------------------------------------------------
// Lone getter via `#[v8_name]` — read works, set is silently ignored.
// ---------------------------------------------------------------------------

mod lone_getter {
    use super::*;

    pub struct Reader;

    #[v8_class]
    impl Reader {
        #[v8_constructor]
        fn new() -> Reader {
            Reader
        }

        #[v8_getter]
        #[v8_name = "x"]
        fn read_x(&self) -> u32 {
            123
        }
    }
}

#[test]
fn lone_getter_with_v8_name_works() {
    let s = run_in_v8(
        |scope, global| {
            install_class::<lone_getter::Reader>(lone_getter::Reader::install, "Reader", scope, global);
        },
        r#"
        const r = new Reader();
        const v = r.x;
        // No setter installed — silently no-op in non-strict mode.
        r.x = 999;
        const after = r.x;
        `${v},${after}`;
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "123,123");
}

// ---------------------------------------------------------------------------
// Lone setter via `#[v8_name]` — write works, read returns undefined.
// ---------------------------------------------------------------------------

mod lone_setter {
    use super::*;

    use std::sync::atomic::{AtomicU32, Ordering};

    pub static LAST_SET: AtomicU32 = AtomicU32::new(0);

    pub struct Writer;

    #[v8_class]
    impl Writer {
        #[v8_constructor]
        fn new() -> Writer {
            Writer
        }

        #[v8_setter]
        #[v8_name = "y"]
        fn write_y(&self, v: u32) {
            LAST_SET.store(v, Ordering::SeqCst);
        }
    }
}

#[test]
fn lone_setter_with_v8_name_works() {
    let s = run_in_v8(
        |scope, global| {
            install_class::<lone_setter::Writer>(lone_setter::Writer::install, "Writer", scope, global);
        },
        r#"
        const w = new Writer();
        const before = w.y;     // no getter — undefined
        w.y = 77;               // setter records
        const after = w.y;      // still undefined
        `${typeof before},${typeof after}`;
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "undefined,undefined");
    assert_eq!(
        lone_setter::LAST_SET.load(std::sync::atomic::Ordering::SeqCst),
        77
    );
}

// ---------------------------------------------------------------------------
// Mixed: paired pair AND an unrelated single getter coexist.
// ---------------------------------------------------------------------------

mod mixed {
    use super::*;

    pub struct Mixed {
        pub value: Cell<u32>,
    }

    #[v8_class]
    impl Mixed {
        #[v8_constructor]
        fn new() -> Mixed {
            Mixed {
                value: Cell::new(0),
            }
        }

        // Paired half 1 — getter under JS name "value".
        #[v8_getter]
        #[v8_name = "value"]
        fn get_value(&self) -> u32 {
            self.value.get()
        }

        // Paired half 2 — setter under JS name "value".
        #[v8_setter]
        #[v8_name = "value"]
        fn set_value(&self, v: u32) {
            self.value.set(v);
        }

        // Unrelated single getter under "constant". MUST NOT clobber
        // the paired pair's keyed entry.
        #[v8_getter]
        fn constant(&self) -> u32 {
            42
        }
    }
}

#[test]
fn mixed_paired_and_lone_dont_collide() {
    let s = run_in_v8(
        |scope, global| {
            install_class::<mixed::Mixed>(mixed::Mixed::install, "Mixed", scope, global);
        },
        r#"
        const m = new Mixed();
        const c0 = m.constant;
        const v0 = m.value;
        m.value = 99;
        const v1 = m.value;
        const c1 = m.constant;     // still 42
        // Sanity-check both descriptors exist independently.
        const dValue = Object.getOwnPropertyDescriptor(Mixed.prototype, "value");
        const dConst = Object.getOwnPropertyDescriptor(Mixed.prototype, "constant");
        const valueHasBoth = typeof dValue.get === "function"
                             && typeof dValue.set === "function";
        const constHasOnlyGet = typeof dConst.get === "function"
                                && typeof dConst.set === "undefined";
        `${c0},${v0},${v1},${c1},${valueHasBoth},${constHasOnlyGet}`;
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "42,0,99,42,true,true");
}
