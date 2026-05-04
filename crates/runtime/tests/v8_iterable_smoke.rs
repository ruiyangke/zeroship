//! Smoke tests for `#[v8_iterable(key = K, value = V)]` — emits the
//! WebIDL pair-iterator surface (keys / values / entries / forEach /
//! @@iterator) plus a companion `<Class>Iterator` class.
//!
//! Iteration is **snapshot** mode: the iterator clones `value_pairs()`
//! once at factory call time and walks the snapshot. WebIDL §3.7.10.2
//! actually requires live iteration; this is a deliberate
//! simplification (see v8_iterable.rs doc-comment for rationale). The
//! tests assume snapshot semantics — mutation during iteration is NOT
//! observable through the running iterator.
//!
//! Coverage:
//!   - `for (const [k, v] of foo)` works (uses @@iterator → entries)
//!   - `foo.keys()`, `foo.values()`, `foo.entries()` each yield the
//!     correct projection
//!   - `foo.forEach((v, k, this_) => ...)` invokes per pair with this_
//!     bound to thisArg (or undefined)
//!   - `Symbol.toStringTag === "Foo Iterator"` on the iterator
//!   - The iterator's `next()` returns `{ value, done }` per protocol
//!   - Keys / values / entries each share the entries factory's
//!     identity for `[Symbol.iterator]` per spec
//!   - `foo.entries() === foo[Symbol.iterator]` is FALSE (different
//!     FunctionTemplates per the macro's emit; documented behaviour)
//!     — TODO confirm spec preference. We test the NEW iterator
//!     produced by each call instead.
//!   - Brand check: calling `MyMap.prototype.keys.call(otherObject)`
//!     throws "Illegal invocation".
//!   - Iterator constructor (`new MyMapIterator()`) throws — iterators
//!     can only come from the parent's factories.
#![allow(unsafe_code)]

use zeroship_runtime::byte_string::ByteString;
use zeroship_runtime::init_v8;
#[allow(unused_imports)]
use zeroship_runtime_macros::{v8_class, v8_constructor, v8_iterable, v8_method};

// ---------------------------------------------------------------------------
// Test harness
// ---------------------------------------------------------------------------

fn run_in_v8<F, R>(install: impl FnOnce(&mut v8::PinScope, v8::Local<v8::Object>), src: &str, f: F) -> R
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
// Test class — yields three pairs of (u32 key, ByteString value).
// ---------------------------------------------------------------------------

mod simple {
    use super::*;

    pub struct Counter;

    #[v8_class]
    #[v8_iterable(key = u32, value = ByteString)]
    impl Counter {
        #[v8_constructor]
        fn new() -> Counter {
            Counter
        }

        // Required by `#[v8_iterable]`. Must be public to the macro
        // (the emitted factory callbacks call `__instance.value_pairs()`)
        // but does NOT need `#[v8_method]` — it's not exposed to JS
        // directly.
        fn value_pairs(&self) -> Vec<(u32, ByteString)> {
            vec![
                (1u32, ByteString::from_bytes(b"a".to_vec())),
                (2u32, ByteString::from_bytes(b"b".to_vec())),
                (3u32, ByteString::from_bytes(b"c".to_vec())),
            ]
        }
    }
}

fn install(scope: &mut v8::PinScope, global: v8::Local<v8::Object>) {
    install_class::<simple::Counter>(simple::Counter::install, "Counter", scope, global);
}

// ---------------------------------------------------------------------------
// for-of works (uses @@iterator → entries)
// ---------------------------------------------------------------------------

#[test]
fn for_of_yields_pairs() {
    let s = run_in_v8(
        install,
        r#"
        const c = new Counter();
        const out = [];
        for (const [k, v] of c) {
            out.push(`${k}=${v}`);
        }
        out.join(",");
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "1=a,2=b,3=c");
}

// ---------------------------------------------------------------------------
// keys() / values() / entries() each yield the correct projection
// ---------------------------------------------------------------------------

#[test]
fn keys_yields_keys_only() {
    let s = run_in_v8(
        install,
        r#"
        const c = new Counter();
        const out = [];
        for (const k of c.keys()) { out.push(k); }
        out.join(",");
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "1,2,3");
}

#[test]
fn values_yields_values_only() {
    let s = run_in_v8(
        install,
        r#"
        const c = new Counter();
        const out = [];
        for (const v of c.values()) { out.push(v); }
        out.join(",");
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "a,b,c");
}

#[test]
fn entries_yields_pairs() {
    let s = run_in_v8(
        install,
        r#"
        const c = new Counter();
        const out = [];
        for (const [k, v] of c.entries()) { out.push(`${k}=${v}`); }
        out.join(",");
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "1=a,2=b,3=c");
}

// ---------------------------------------------------------------------------
// forEach
// ---------------------------------------------------------------------------

#[test]
fn for_each_invokes_callback_per_pair() {
    let s = run_in_v8(
        install,
        r#"
        const c = new Counter();
        const out = [];
        c.forEach((v, k) => { out.push(`${k}=${v}`); });
        out.join(",");
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "1=a,2=b,3=c");
}

#[test]
fn for_each_third_arg_is_collection() {
    let s = run_in_v8(
        install,
        r#"
        const c = new Counter();
        let third = null;
        c.forEach((v, k, t) => { if (third === null) third = t; });
        // The third arg is the collection itself (per WebIDL §3.7.10.3).
        third === c ? "yes" : "no";
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "yes");
}

#[test]
fn for_each_this_arg_is_bound() {
    let s = run_in_v8(
        install,
        r#"
        const c = new Counter();
        const myThis = { tag: "expected" };
        let captured = null;
        c.forEach(function (v, k) {
            if (captured === null) captured = this;
        }, myThis);
        captured === myThis ? "yes" : "no";
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "yes");
}

#[test]
fn for_each_callback_not_callable_throws() {
    let s = run_in_v8(
        install,
        r#"
        const c = new Counter();
        let msg = "no-throw";
        try { c.forEach(42); } catch (e) { msg = "type-error:" + e.name; }
        msg;
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "type-error:TypeError");
}

// ---------------------------------------------------------------------------
// Symbol.toStringTag on the iterator
// ---------------------------------------------------------------------------

#[test]
fn iterator_has_to_string_tag() {
    let s = run_in_v8(
        install,
        r#"
        const c = new Counter();
        const it = c.entries();
        Object.prototype.toString.call(it);
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "[object Counter Iterator]");
}

// ---------------------------------------------------------------------------
// next() returns { value, done } per JS Iterator protocol
// ---------------------------------------------------------------------------

#[test]
fn next_returns_value_done_object() {
    let s = run_in_v8(
        install,
        r#"
        const c = new Counter();
        const it = c.keys();
        const r1 = it.next();
        const r2 = it.next();
        const r3 = it.next();
        const r4 = it.next();
        `${r1.value},${r1.done},${r2.value},${r2.done},${r3.value},${r3.done},${r4.value},${r4.done}`;
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "1,false,2,false,3,false,undefined,true");
}

// ---------------------------------------------------------------------------
// Iterator prototype chain — should chain through %IteratorPrototype%
// per WebIDL §3.7.10.2
// ---------------------------------------------------------------------------

#[test]
fn iterator_prototype_chains_to_iterator_prototype() {
    let s = run_in_v8(
        install,
        r#"
        const c = new Counter();
        const it = c.entries();
        // %IteratorPrototype% is the prototype-of-prototype of an array
        // iterator. it.__proto__.__proto__ should equal that for a
        // proper WebIDL default iterator.
        const expected = Object.getPrototypeOf(Object.getPrototypeOf([][Symbol.iterator]()));
        Object.getPrototypeOf(Object.getPrototypeOf(it)) === expected ? "yes" : "no";
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "yes");
}

// ---------------------------------------------------------------------------
// Brand check: calling iterable methods on a non-Counter throws
// ---------------------------------------------------------------------------

#[test]
fn keys_on_plain_object_throws() {
    let s = run_in_v8(
        install,
        r#"
        let msg = "no-throw";
        try {
            Counter.prototype.keys.call({});
        } catch (e) {
            msg = "type-error:" + e.name;
        }
        msg;
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "type-error:TypeError");
}

// ---------------------------------------------------------------------------
// Iterator constructor is not user-callable
// ---------------------------------------------------------------------------

#[test]
fn iterator_constructor_throws() {
    // We don't expose the iterator class on globalThis directly, but
    // the user can grab it via getPrototypeOf(it).constructor. Calling
    // it as a constructor must throw.
    let s = run_in_v8(
        install,
        r#"
        const c = new Counter();
        const it = c.entries();
        const Ctor = Object.getPrototypeOf(it).constructor;
        let msg = "no-throw";
        try { new Ctor(); } catch (e) { msg = "type-error:" + e.name; }
        msg;
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "type-error:TypeError");
}

// ---------------------------------------------------------------------------
// Multiple iterators iterate independently (snapshot model)
// ---------------------------------------------------------------------------

#[test]
fn two_iterators_advance_independently() {
    let s = run_in_v8(
        install,
        r#"
        const c = new Counter();
        const a = c.keys();
        const b = c.keys();
        a.next(); a.next();         // a is at index 2
        const ar = a.next();         // a → 3
        const br = b.next();         // b → 1
        `${ar.value},${br.value}`;
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "3,1");
}
