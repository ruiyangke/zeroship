//! Smoke tests for `#[v8_getter(same_object)]` — WebIDL `[SameObject]`
//! cache attribute.
//!
//! Several WebIDL accessors must return the SAME JS object across
//! reads on the same instance (`Request.headers`, `Response.headers`,
//! `URL.searchParams`, …). Without caching, each read would mint a
//! fresh object, breaking userland code that uses `===` identity
//! against a stashed reference (e.g. comparing iterators, holding
//! the reference across an async boundary).
//!
//! Pre-fix, every interface that needed [SameObject] hand-rolled a
//! V8 private symbol stash. The macro now automates this:
//! `#[v8_getter(same_object)]` wraps the user method with a
//! cache-on-first-access lookup keyed by a per-class-and-getter
//! Private symbol on the wrapper instance.
//!
//! Coverage:
//!   - `instance.foo === instance.foo` (cache hit)
//!   - User method is invoked at most once per instance
//!   - Different instances get different cached objects
//!   - The cached object survives multiple property reads + a method
//!     call that reads the cache
//!   - The cache key is per-getter, not class-wide (two SameObject
//!     getters on the same class produce different cached objects)
#![allow(unsafe_code)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

use zeroship_runtime::init_v8;
#[allow(unused_imports)]
use zeroship_runtime_macros::{v8_class, v8_constructor, v8_getter, v8_method, v8_name};

/// Process-wide mutex to serialise tests in this file. Each test
/// observes/asserts a delta on the shared mint counters; cargo runs
/// tests on multiple threads by default, so without serialisation
/// two tests' reads/writes interleave and the delta assertions fail
/// non-deterministically. Holding this guard for the duration of
/// each test makes the counter reads transactional w.r.t. the JS
/// reads inside the test.
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
// Class with two SameObject getters. Each mints a fresh JS Object
// containing a unique-id (incremented per call) so we can check the
// cache by reading the id and seeing it stays stable.
// ---------------------------------------------------------------------------

mod same_object_class {
    use super::*;

    /// Counts how many times the `headers()` impl method actually ran
    /// (as opposed to returning a cached value). The smoke test
    /// reads this to verify the user method runs at most once per
    /// instance per getter.
    pub static HEADERS_MINTS: AtomicUsize = AtomicUsize::new(0);
    pub static SEARCH_PARAMS_MINTS: AtomicUsize = AtomicUsize::new(0);

    pub struct Container;

    #[v8_class]
    impl Container {
        #[v8_constructor]
        fn new() -> Container {
            Container
        }

        /// First SameObject getter: mints a fresh `{ kind: "headers",
        /// mintedAt: <call#> }` Object. The macro caches the Local on
        /// first access and returns it thereafter — `mintedAt` should
        /// stay constant for a given instance.
        #[v8_getter(same_object)]
        fn headers(&self, scope: &mut v8::PinScope) -> v8::Global<v8::Object> {
            let call_idx = HEADERS_MINTS.fetch_add(1, Ordering::SeqCst);
            let obj = v8::Object::new(scope);
            let k_kind = v8::String::new(scope, "kind").unwrap();
            let v_kind = v8::String::new(scope, "headers").unwrap();
            obj.set(scope, k_kind.into(), v_kind.into());
            let k_mint = v8::String::new(scope, "mintedAt").unwrap();
            let v_mint = v8::Number::new(scope, call_idx as f64);
            obj.set(scope, k_mint.into(), v_mint.into());
            v8::Global::new(scope, obj)
        }

        /// Second SameObject getter on the same class. Proves the
        /// per-getter cache key isolates them — reading
        /// `c.searchParams` doesn't return the cached `headers`.
        #[v8_getter(same_object)]
        #[v8_name = "searchParams"]
        fn search_params(&self, scope: &mut v8::PinScope) -> v8::Global<v8::Object> {
            let call_idx = SEARCH_PARAMS_MINTS.fetch_add(1, Ordering::SeqCst);
            let obj = v8::Object::new(scope);
            let k_kind = v8::String::new(scope, "kind").unwrap();
            let v_kind = v8::String::new(scope, "searchParams").unwrap();
            obj.set(scope, k_kind.into(), v_kind.into());
            let k_mint = v8::String::new(scope, "mintedAt").unwrap();
            let v_mint = v8::Number::new(scope, call_idx as f64);
            obj.set(scope, k_mint.into(), v_mint.into());
            v8::Global::new(scope, obj)
        }
    }
}

// ---------------------------------------------------------------------------
// `instance.foo === instance.foo` — cache hit.
// ---------------------------------------------------------------------------

#[test]
fn same_object_returns_same_reference() {
    // Process-global counters are shared with parallel tests; serialise.
    let _g = test_lock();
    // The HEADERS_MINTS counter is process-global and other tests in
    // this file also increment it. Snapshot the BEFORE value and
    // assert the delta of 1 — combined with the test_lock above, this
    // is transactional regardless of cargo's parallel execution.
    let before = same_object_class::HEADERS_MINTS.load(Ordering::SeqCst);
    let s = run_in_v8(
        |scope, global| {
            install_class::<same_object_class::Container>(
                same_object_class::Container::install,
                "Container",
                scope,
                global,
            );
        },
        r#"
        const c = new Container();
        const a = c.headers;
        const b = c.headers;
        const d = c.headers;
        // Three reads, but identity must match — cache hit on reads 2/3.
        // The cached object's `mintedAt` is whatever it was the moment
        // the user method ran, which is process-global; so we check the
        // mintedAt is *consistent across the three reads*, not its value.
        JSON.stringify({
            sameRef: a === b && b === d,
            kind: a.kind,
            mintedAtConsistent: a.mintedAt === b.mintedAt
                                 && b.mintedAt === d.mintedAt,
        });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(
        s,
        r#"{"sameRef":true,"kind":"headers","mintedAtConsistent":true}"#
    );
    let after = same_object_class::HEADERS_MINTS.load(Ordering::SeqCst);
    assert_eq!(
        after - before,
        1,
        "user method should run exactly once per instance per getter"
    );
}

// ---------------------------------------------------------------------------
// Two getters on the same class — independent caches.
// ---------------------------------------------------------------------------

#[test]
fn distinct_getters_cache_independently() {
    let _g = test_lock();
    let h_before = same_object_class::HEADERS_MINTS.load(Ordering::SeqCst);
    let sp_before = same_object_class::SEARCH_PARAMS_MINTS.load(Ordering::SeqCst);
    let s = run_in_v8(
        |scope, global| {
            install_class::<same_object_class::Container>(
                same_object_class::Container::install,
                "Container",
                scope,
                global,
            );
        },
        r#"
        const c = new Container();
        const h1 = c.headers;
        const sp1 = c.searchParams;
        const h2 = c.headers;
        const sp2 = c.searchParams;
        JSON.stringify({
            headersStable:      h1 === h2,
            searchParamsStable: sp1 === sp2,
            // The two getters mint distinct Objects (they're not aliased).
            distinctRefs:       h1 !== sp1,
            kinds:              [h1.kind, sp1.kind],
        });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(
        s,
        r#"{"headersStable":true,"searchParamsStable":true,"distinctRefs":true,"kinds":["headers","searchParams"]}"#
    );
    assert_eq!(
        same_object_class::HEADERS_MINTS.load(Ordering::SeqCst) - h_before,
        1
    );
    assert_eq!(
        same_object_class::SEARCH_PARAMS_MINTS.load(Ordering::SeqCst) - sp_before,
        1
    );
}

// ---------------------------------------------------------------------------
// Two instances of the same class — independent caches.
// ---------------------------------------------------------------------------

#[test]
fn different_instances_have_different_cached_objects() {
    let _g = test_lock();
    let before = same_object_class::HEADERS_MINTS.load(Ordering::SeqCst);
    let s = run_in_v8(
        |scope, global| {
            install_class::<same_object_class::Container>(
                same_object_class::Container::install,
                "Container",
                scope,
                global,
            );
        },
        r#"
        const a = new Container();
        const b = new Container();
        const ha = a.headers;
        const hb = b.headers;
        const ha2 = a.headers;
        const hb2 = b.headers;
        JSON.stringify({
            instanceAStable: ha === ha2,
            instanceBStable: hb === hb2,
            // Same class, different instance — caches are independent.
            crossInstanceDistinct: ha !== hb,
        });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(
        s,
        r#"{"instanceAStable":true,"instanceBStable":true,"crossInstanceDistinct":true}"#
    );
    // One mint per instance × two instances = 2 — the delta of the
    // process-global counter is 2 regardless of cross-test ordering.
    assert_eq!(
        same_object_class::HEADERS_MINTS.load(Ordering::SeqCst) - before,
        2
    );
}

// ---------------------------------------------------------------------------
// Brand check still applies on the cached path. Reading `headers` on
// a non-instance must throw before any private-symbol lookup.
// ---------------------------------------------------------------------------

#[test]
fn same_object_getter_brand_checks() {
    let s = run_in_v8(
        |scope, global| {
            install_class::<same_object_class::Container>(
                same_object_class::Container::install,
                "Container",
                scope,
                global,
            );
        },
        r#"
        const desc = Object.getOwnPropertyDescriptor(Container.prototype, "headers");
        let kind;
        try { desc.get.call({}); }
        catch (e) { kind = e.constructor.name; }
        kind;
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "TypeError");
}
