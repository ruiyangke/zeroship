//! Cross-class brand-check coverage for `<Class>Iterator.prototype
//! .next()`.
//!
//! Every `#[v8_class]` wrapper stores `internal_field(0) = External(Box<X>)`,
//! so a caller can lift `<Class>Iterator.prototype.next` and `.call` it on a
//! different wrapper. Without a brand check, the macro's External-recovery
//! prologue at the top of the iterator's `next()` callback reads that foreign
//! wrapper's box:
//!
//! ```ignore
//! let __this = args.this();
//! // (lengthy comment claiming "internal-field-1-is-External check is
//! //  sufficient since the iterator class isn't exposed in a way that
//! //  lets users construct one with a different box layout.")
//! let __ext = match __this.get_internal_field(scope, 0)... ;
//! let __it: &mut <Class>Iterator =
//!     unsafe { &mut *(__ext.value() as *mut <Class>Iterator) };
//! ```
//!
//! The recovery `__ext.value() as *mut <Class>Iterator` then reinterprets a
//! `Box<Other>` as `*mut <Class>Iterator` and `&mut *`s it — UB: in release
//! mode it corrupts whichever box's state aliases the cast, and under Miri it
//! is an instant abort.
//!
//! The macro emits a per-iterator-class brand check (private prototype-walk
//! helper, mirrors the parent-class brand check in `v8_class/emit/brand.rs`)
//! and calls it at the top of `next()` before the External recovery. Mismatches
//! throw TypeError with the shape
//! `"<Class>Iterator.prototype.next called on incompatible receiver"`.
//!
//! What this file pins:
//!   1. **Cross-class**: `fakeMap.entries().__proto__.next.call(other)`
//!      where `other` is a different `#[v8_class]` instance: throws
//!      TypeError, no UB.
//!   2. **Cross-iterator**: `iterA.__proto__.next.call(iterBInstance)`
//!      where `iterA` and `iterB` are iterators of two DISTINCT iterable
//!      classes: throws TypeError, no UB.
//!   3. **Self-iterator passes**: `iter.next()` on its own instance still
//!      works.
#![allow(unsafe_code)]

use zeroship_runtime::byte_string::ByteString;
use zeroship_runtime::init_v8;
#[allow(unused_imports)]
use zeroship_runtime_macros::{v8_class, v8_constructor, v8_iterable, v8_method};

// ---------------------------------------------------------------------------
// Test harness — local copy (mirrors the one used by other v8_*_smoke
// tests in this directory).
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
// Test classes:
//   - `FakeMapA`: `#[v8_class] + #[v8_iterable]` — yields one pair.
//   - `FakeMapB`: same shape, different fields, different pair count
//     and different ByteString sizes (so a misinterpreted recovery
//     would dereference at the WRONG byte offsets, surfacing UB even
//     in release mode if the brand check were absent).
//   - `Box`: a non-iterable `#[v8_class]` — internal field 0 holds a
//     `Box<Box>` raw pointer, *NOT* a `Box<FakeMapAIterator>`.
// ---------------------------------------------------------------------------

// Each iterable class lives in its own submodule. The macro's per-
// iterable codegen emits non-class-prefixed `__zs_iter_construct_throws`
// and `__zs_iter_factory_impl` helpers, which would collide if two
// iterables coexisted in the same Rust module.

mod fixture_a {
    use super::*;

    /// Iterable class A. Box<FakeMapA> in internal field 0.
    pub struct FakeMapA;

    #[v8_class]
    #[v8_iterable(key = u32, value = ByteString)]
    impl FakeMapA {
        #[v8_constructor]
        fn new() -> FakeMapA {
            FakeMapA
        }

        fn value_pairs(&self) -> Vec<(u32, ByteString)> {
            vec![(7u32, ByteString::from_bytes(b"alpha".to_vec()))]
        }
    }
}

mod fixture_b {
    use super::*;

    /// Iterable class B. Different storage byte-shape (multiple
    /// entries, different lengths) so a mis-recovery would land on
    /// garbage offsets.
    pub struct FakeMapB;

    #[v8_class]
    #[v8_iterable(key = u32, value = ByteString)]
    impl FakeMapB {
        #[v8_constructor]
        fn new() -> FakeMapB {
            FakeMapB
        }

        fn value_pairs(&self) -> Vec<(u32, ByteString)> {
            vec![
                (1u32, ByteString::from_bytes(b"x".to_vec())),
                (2u32, ByteString::from_bytes(b"yy".to_vec())),
                (3u32, ByteString::from_bytes(b"zzz".to_vec())),
            ]
        }
    }
}

mod fixture_box {
    use super::*;

    /// A non-iterable wrapper. `Box<BoxClass>` is a different layout
    /// from `Box<FakeMapAIterator>` (no `__pairs`/`__index`/`__kind`
    /// fields), so reinterpreting `__ext.value()` would land on
    /// completely unrelated memory.
    pub struct BoxClass {
        pub _payload: [u8; 16],
    }

    #[v8_class]
    impl BoxClass {
        #[v8_constructor]
        fn new() -> BoxClass {
            BoxClass {
                _payload: [0xAA; 16],
            }
        }

        // A trivial method so JS can confirm we have a working wrapper.
        #[v8_method]
        fn marker(&self) -> u32 {
            42
        }
    }
}

fn install_all(scope: &mut v8::PinScope, global: v8::Local<v8::Object>) {
    install_class::<fixture_a::FakeMapA>(
        fixture_a::FakeMapA::install,
        "FakeMapA",
        scope,
        global,
    );
    install_class::<fixture_b::FakeMapB>(
        fixture_b::FakeMapB::install,
        "FakeMapB",
        scope,
        global,
    );
    install_class::<fixture_box::BoxClass>(
        fixture_box::BoxClass::install,
        "BoxClass",
        scope,
        global,
    );
}

// ---------------------------------------------------------------------------
// Sanity: own-iterator next() works on its own receiver. Pinning the
// happy path so a bug in the brand-check helper that rejected ALL
// receivers would surface here.
// ---------------------------------------------------------------------------

#[test]
fn own_iterator_next_works() {
    let s = run_in_v8(
        install_all,
        r#"
        const a = new FakeMapA();
        const it = a.entries();
        const r = it.next();
        `${r.value[0]},${r.value[1]},${r.done}`;
        "#,
        js_string,
    );
    assert_eq!(s, "7,alpha,false");
}

// ---------------------------------------------------------------------------
// Cross-class: lift `FakeMapA`'s iterator's `next` and call it with a
// non-iterable `BoxClass` instance as `this`. The brand check rejects the
// foreign receiver and throws TypeError with a clear message.
// ---------------------------------------------------------------------------

#[test]
fn next_on_foreign_class_wrapper_throws_typeerror() {
    let s = run_in_v8(
        install_all,
        r#"
        const a = new FakeMapA();
        const iter = a.entries();
        const Next = Object.getPrototypeOf(iter).next;
        const box_ = new BoxClass();
        // Sanity: BoxClass really is a working wrapper (so the receiver
        // we're passing isn't a stand-in object).
        const sanity = box_.marker();
        let kind = "no-throw";
        let msg = "";
        try {
            Next.call(box_);
        } catch (e) {
            kind = "type-error:" + e.name;
            msg = e.message;
        }
        JSON.stringify({ sanity, kind, msg });
        "#,
        js_string,
    );
    // Sanity == 42 confirms BoxClass wrapper is functioning. The brand
    // check then rejects it as a FakeMapAIterator receiver.
    assert_eq!(
        s,
        r#"{"sanity":42,"kind":"type-error:TypeError","msg":"FakeMapAIterator.prototype.next called on incompatible receiver"}"#
    );
}

// ---------------------------------------------------------------------------
// Cross-iterator: two distinct iterable classes (`FakeMapA`, `FakeMapB`)
// each emit their own `<Class>Iterator`. Calling A's `next` with B's
// iterator instance as `this` must be rejected even when the two iterator
// layouts match (snapshot mode: `__pairs`, `__index`, `__kind`), because
// they are DIFFERENT generic instantiations. A's brand check rejects the B
// iterator and throws TypeError.
// ---------------------------------------------------------------------------

#[test]
fn next_on_other_iterable_class_iterator_throws_typeerror() {
    let s = run_in_v8(
        install_all,
        r#"
        const a = new FakeMapA();
        const b = new FakeMapB();
        const iterA = a.entries();
        const iterB = b.entries();
        const NextA = Object.getPrototypeOf(iterA).next;
        // Sanity: A's iterator and B's iterator have DIFFERENT next
        // functions (separate FunctionTemplates per class).
        const NextB = Object.getPrototypeOf(iterB).next;
        const distinct = NextA !== NextB;
        let kind = "no-throw";
        let msg = "";
        try {
            // Calling A's next on B's iterator instance.
            NextA.call(iterB);
        } catch (e) {
            kind = "type-error:" + e.name;
            msg = e.message;
        }
        JSON.stringify({ distinct, kind, msg });
        "#,
        js_string,
    );
    assert_eq!(
        s,
        r#"{"distinct":true,"kind":"type-error:TypeError","msg":"FakeMapAIterator.prototype.next called on incompatible receiver"}"#
    );
}

// ---------------------------------------------------------------------------
// Symmetric cross-iterator: the OTHER direction (B's next on A's iterator).
// Pins that the brand check is per-iterator-class (not just per-parent-
// class), so each iterator class's prototype is uniquely identifying.
// ---------------------------------------------------------------------------

#[test]
fn next_on_other_iterable_class_iterator_symmetric_throws_typeerror() {
    let s = run_in_v8(
        install_all,
        r#"
        const a = new FakeMapA();
        const b = new FakeMapB();
        const iterA = a.entries();
        const iterB = b.entries();
        const NextB = Object.getPrototypeOf(iterB).next;
        let kind = "no-throw";
        let msg = "";
        try {
            // Calling B's next on A's iterator instance.
            NextB.call(iterA);
        } catch (e) {
            kind = "type-error:" + e.name;
            msg = e.message;
        }
        JSON.stringify({ kind, msg });
        "#,
        js_string,
    );
    assert_eq!(
        s,
        r#"{"kind":"type-error:TypeError","msg":"FakeMapBIterator.prototype.next called on incompatible receiver"}"#
    );
}

// ---------------------------------------------------------------------------
// Plain {} receiver still throws a TypeError. The V8 built-in iterators all
// throw TypeError on `next.call({})`, and JS frameworks often try/catch
// around iterator protocol probes, so the user-visible outcome must stay a
// TypeError regardless of the brand-check implementation.
// ---------------------------------------------------------------------------

#[test]
fn next_on_plain_object_throws_typeerror() {
    let s = run_in_v8(
        install_all,
        r#"
        const a = new FakeMapA();
        const iter = a.entries();
        const Next = Object.getPrototypeOf(iter).next;
        let kind = "no-throw";
        try { Next.call({}); }
        catch (e) { kind = "type-error:" + e.name; }
        kind;
        "#,
        js_string,
    );
    assert_eq!(s, "type-error:TypeError");
}
