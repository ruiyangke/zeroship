//! Smoke tests for WebIDL `[NewObject]` semantics on default getters.
//!
//! WebIDL `[NewObject]` marks a getter that MUST return a freshly-allocated
//! Object on every access. The default behaviour for a `#[v8_getter]` (no
//! list-form attribute) is already `[NewObject]`: `gen_method_callback`
//! invokes the user method on every read and never stashes a result.
//!
//! The opposite — `[SameObject]` caching — is opt-in via
//! `#[v8_getter(same_object)]` (commit `3cb0fe11`). This test pair
//! demonstrates that the default and the opt-in produce the expected
//! identity behaviour:
//!
//!   - default `#[v8_getter]` returning a v8::Local Object: each read
//!     mints a FRESH JS Object (`a !== b`) — `[NewObject]`.
//!   - `#[v8_getter(same_object)]` returning a v8::Global Object: each
//!     read returns the SAME JS Object (`a === b`) — `[SameObject]`.
//!
//! Combined with the existing `tests/v8_same_object_smoke.rs` coverage,
//! this closes the audit captured in `runtime-macros/TODO.md` "[NewObject]
//! semantic": the default IS `[NewObject]`; no opt-out attribute is
//! needed because there's no implicit caching to opt out of.
#![allow(unsafe_code)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

use zeroship_runtime::init_v8;
#[allow(unused_imports)]
use zeroship_runtime_macros::{v8_class, v8_constructor, v8_getter, v8_method};

/// Process-wide mutex to serialise tests in this file. The MINTS counter
/// is shared, so cargo's parallel test runner would interleave reads
/// against mints from a sibling test and break the delta assertion.
/// Same pattern as `tests/v8_same_object_smoke.rs::test_lock`.
fn test_lock() -> MutexGuard<'static, ()> {
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

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
// Class with a default getter (no `same_object`) returning a v8::Local
// Object. Each read must mint a fresh Object — `[NewObject]` semantics.
// ---------------------------------------------------------------------------

mod new_object_class {
    use super::*;

    pub static MINTS: AtomicUsize = AtomicUsize::new(0);

    pub struct Crate;

    #[v8_class]
    impl Crate {
        #[v8_constructor]
        fn new() -> Crate {
            Crate
        }

        /// Default getter — no `(same_object)`. Returns a v8::Local
        /// Object containing a unique-per-call `mintedAt` counter so the
        /// test can confirm the user method actually ran on every read.
        #[v8_getter]
        fn fresh<'s>(&self, scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Value> {
            let n = MINTS.fetch_add(1, Ordering::SeqCst);
            let obj = v8::Object::new(scope);
            let k = v8::String::new(scope, "mintedAt").unwrap();
            let v = v8::Number::new(scope, n as f64);
            obj.set(scope, k.into(), v.into());
            obj.into()
        }
    }
}

// ---------------------------------------------------------------------------
// Two reads of the default getter must yield distinct JS Objects, AND
// the user method must have run twice (delta on the mint counter == 2).
// This is the WebIDL `[NewObject]` behaviour.
// ---------------------------------------------------------------------------

#[test]
fn default_getter_mints_fresh_object_each_read() {
    let _g = test_lock();
    let before = new_object_class::MINTS.load(Ordering::SeqCst);
    let s = run_in_v8(
        |scope, global| {
            install_class::<new_object_class::Crate>(
                new_object_class::Crate::install,
                "Crate",
                scope,
                global,
            );
        },
        r#"
        const b = new Crate();
        const a = b.fresh;
        const c = b.fresh;
        // Three reads, three distinct Object identities, three distinct
        // `mintedAt` counters — confirming the user method ran on every
        // read (no caching by the macro).
        const d = b.fresh;
        JSON.stringify({
            distinctRefs: a !== c && c !== d && a !== d,
            distinctMintedAt: a.mintedAt !== c.mintedAt
                              && c.mintedAt !== d.mintedAt
                              && a.mintedAt !== d.mintedAt,
        });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"distinctRefs":true,"distinctMintedAt":true}"#);
    let after = new_object_class::MINTS.load(Ordering::SeqCst);
    assert_eq!(
        after - before,
        3,
        "default getter must run user method on every read \
         (got delta {after} - {before} = {})",
        after - before,
    );
}

// ---------------------------------------------------------------------------
// Cross-instance isolation: two instances reading the same getter still
// produce distinct Objects. This is true with or without caching, but
// asserting it here documents the contract.
// ---------------------------------------------------------------------------

#[test]
fn default_getter_two_instances_get_distinct_objects() {
    let _g = test_lock();
    let before = new_object_class::MINTS.load(Ordering::SeqCst);
    let s = run_in_v8(
        |scope, global| {
            install_class::<new_object_class::Crate>(
                new_object_class::Crate::install,
                "Crate",
                scope,
                global,
            );
        },
        r#"
        const x = new Crate();
        const y = new Crate();
        const a = x.fresh;
        const b = y.fresh;
        JSON.stringify({
            crossInstanceDistinct: a !== b,
        });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"crossInstanceDistinct":true}"#);
    let after = new_object_class::MINTS.load(Ordering::SeqCst);
    assert_eq!(after - before, 2, "two reads = two mints");
}
