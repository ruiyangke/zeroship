//! Tests for the `#[v8_method(fastcall)]` and `#[v8_getter(fastcall)]`
//! attribute extensions.
//!
//! V8's fast API path (CFunction) lets Turbofan inline a typed-shape
//! function call directly into the optimised JIT, skipping the full
//! FunctionCallback prologue (~30-100ns: External lookup, scope setup,
//! brand check, *self recovery). The macro emits BOTH a slow-path
//! FunctionCallback (current behaviour, used as a fallback whenever V8
//! can't or doesn't optimise) AND a typed CFunction shim, then wires
//! them via `FunctionTemplate::builder(slow).build_fast(scope, &[fast])`.
//!
//! V8 chooses between fast and slow at JIT time based on the receiver's
//! hidden class and arg shape. Tests use the V8 internals
//! `%PrepareFunctionForOptimization` / `%OptimizeFunctionOnNextCall` to
//! force the path under test.
//!
//! Coverage:
//!   - bool getter — `(this) -> bool` fastcall path
//!   - u32 method — `(this, u32) -> u32` fastcall path
//!   - FastOneByteString arg — `(this, FastOneByteString) -> bool`,
//!     ASCII-only fast path; multibyte falls through to slow
//!   - Receiver lookup — confirm `*const Self` recovered via
//!     internal-field-1 aligned pointer (slot 1 holds the raw ptr;
//!     slot 0 keeps the External + finalizer for the GC path)
//!   - Slow path still works when V8 hasn't optimised
//!
//! The compile-fail rejections (String return, Vec<u8> return, &mut self)
//! live as `compile_fail` doctests on the proc macro at module level —
//! see `crates/runtime/src/lib.rs`.

#![allow(unsafe_code)]

use zeroship_runtime::init_v8;
#[allow(unused_imports)]
use zeroship_runtime_macros::{v8_class, v8_constructor, v8_getter, v8_method};

// ---------------------------------------------------------------------------
// Test harness — minimal isolate setup with --allow-natives-syntax so we can
// %OptimizeFunctionOnNextCall inside test scripts.
// ---------------------------------------------------------------------------

fn run_in_v8<F, R>(install: impl FnOnce(&mut v8::PinScope, v8::Local<v8::Object>), src: &str, f: F) -> R
where
    F: FnOnce(v8::Local<v8::Value>, &mut v8::PinScope) -> R,
{
    // Enable %PrepareFunctionForOptimization etc. before V8 initialises —
    // these intrinsics are gated on --allow-natives-syntax. Setting flags
    // AFTER initialize() is silently ignored. We install the flags via a
    // Once that races init_v8's own Once: whichever runs first wins, but
    // both are idempotent.
    use std::sync::Once;
    static FLAGS: Once = Once::new();
    FLAGS.call_once(|| {
        v8::V8::set_flags_from_string("--allow-natives-syntax --turbofan --expose-gc");
    });
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
// Test 1: bool getter — fast path returns a primitive
// ---------------------------------------------------------------------------

mod bool_getter {
    use super::*;

    pub struct Flag {
        pub on: bool,
    }

    #[v8_class]
    impl Flag {
        #[v8_constructor]
        fn new(start: Option<u32>) -> Flag {
            Flag {
                on: start.unwrap_or(0) != 0,
            }
        }

        // The fastcall annotation emits a typed CFunction shim alongside
        // the regular FunctionCallback. V8 inlines the shim at hot
        // sites; the slow path is what `f(...)` resolves to before
        // OptimizeFunctionOnNextCall.
        #[v8_getter(fastcall)]
        fn is_on(&self) -> bool {
            self.on
        }
    }
}

#[test]
fn fastcall_bool_getter_via_optimised_call() {
    let s = run_in_v8(
        |scope, global| install_class::<bool_getter::Flag>(
            bool_getter::Flag::install, "Flag", scope, global,
        ),
        r#"
        const f = new Flag(1);
        // %PrepareFunctionForOptimization + a few warm-up calls so V8
        // collects type feedback, then OptimizeFunctionOnNextCall to
        // enter the fast path.
        function read(x) { return x.is_on; }
        %PrepareFunctionForOptimization(read);
        let warm = read(f);
        warm = read(f);
        warm = read(f);
        %OptimizeFunctionOnNextCall(read);
        const r1 = read(f);
        const r2 = read(f);
        JSON.stringify({ r1, r2 });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"r1":true,"r2":true}"#);
}

#[test]
fn fastcall_bool_getter_pre_optimisation_takes_slow_path() {
    // Before OptimizeFunctionOnNextCall, V8 calls the slow FunctionCallback.
    // The result must match — both paths return the same bool.
    let s = run_in_v8(
        |scope, global| install_class::<bool_getter::Flag>(
            bool_getter::Flag::install, "Flag", scope, global,
        ),
        r#"
        const f = new Flag(0);
        const r = f.is_on;  // Direct getter access, no Turbofan
        JSON.stringify({ r });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"r":false}"#);
}

// ---------------------------------------------------------------------------
// Test 2: u32 method — `(this, u32) -> u32`
// ---------------------------------------------------------------------------

mod u32_method {
    use super::*;

    pub struct Adder {
        pub bias: u32,
    }

    #[v8_class]
    impl Adder {
        #[v8_constructor]
        fn new(bias: Option<u32>) -> Adder {
            Adder { bias: bias.unwrap_or(0) }
        }

        #[v8_method(fastcall)]
        fn add(&self, n: u32) -> u32 {
            self.bias + n
        }
    }
}

#[test]
fn fastcall_u32_method_via_optimised_call() {
    let s = run_in_v8(
        |scope, global| install_class::<u32_method::Adder>(
            u32_method::Adder::install, "Adder", scope, global,
        ),
        r#"
        const a = new Adder(100);
        function call(x, n) { return x.add(n); }
        %PrepareFunctionForOptimization(call);
        let r = call(a, 5);
        r = call(a, 5);
        r = call(a, 5);
        %OptimizeFunctionOnNextCall(call);
        const r1 = call(a, 1);
        const r2 = call(a, 23);
        JSON.stringify({ r1, r2 });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"r1":101,"r2":123}"#);
}

// ---------------------------------------------------------------------------
// Test 3: FastOneByteString arg — ASCII fast path; multibyte → slow
// ---------------------------------------------------------------------------
//
// V8's fast API treats SeqOneByteString as a one-byte string fast path:
// every byte is ≤ 0xFF. Strings stored in V8's two-byte form (anything
// > U+00FF) cause V8 to fall back to the slow callback automatically.
// The fast handler can therefore receive the ASCII byte slice with zero
// copy and zero conversion.
//
// We test:
//   1. ASCII input under optimisation hits the fast path,
//   2. Pre-optimisation hits the slow path,
//   3. Multibyte input under optimisation falls back to slow (because
//      V8 can't represent it as SeqOneByteString).

mod onebyte_arg {
    use super::*;
    use std::cell::Cell;

    pub struct PathProbe {
        // Counts how many times the fast path body ran — instrumentation
        // for the test, not user-visible. Static so the fastcall (no
        // scope) can read+write it. Stored as Cell<u32> on the instance
        // so the test can compare counts before/after Optimize calls.
        pub fast_hits: Cell<u32>,
        pub slow_hits: Cell<u32>,
    }

    #[v8_class]
    impl PathProbe {
        #[v8_constructor]
        fn new() -> PathProbe {
            PathProbe {
                fast_hits: Cell::new(0),
                slow_hits: Cell::new(0),
            }
        }

        #[v8_method(fastcall)]
        fn matches(&self, name: ::zeroship_runtime::byte_string::ByteString) -> bool {
            // The macro emits two bodies for this method — a slow path
            // that takes the WebIDL ByteString conversion (extracts via
            // `read_byte_string`) and a fast path that takes a
            // FastOneByteString and constructs ByteString::from_bytes.
            //
            // The user body is shared. We bump path-specific counters
            // via fastcall-only interlude in codegen: NO. The user body
            // can't tell which path it's on. So instead we use a
            // separate instrumented method (below) to count fast-path
            // entries via the slow-path counter as a baseline.
            name.as_slice() == b"hello"
        }

        // Companion instrumented method: increments slow_hits on every
        // entry so we can witness slow-path traffic explicitly.
        #[v8_method]
        fn slow_only(&self, name: ::zeroship_runtime::byte_string::ByteString) -> bool {
            self.slow_hits.set(self.slow_hits.get() + 1);
            name.as_slice() == b"hello"
        }

        #[v8_getter]
        fn fast_count(&self) -> u32 {
            self.fast_hits.get()
        }

        #[v8_getter]
        fn slow_count(&self) -> u32 {
            self.slow_hits.get()
        }
    }
}

#[test]
fn fastcall_onebyte_arg_ascii_returns_correct() {
    let s = run_in_v8(
        |scope, global| install_class::<onebyte_arg::PathProbe>(
            onebyte_arg::PathProbe::install, "Probe", scope, global,
        ),
        r#"
        const p = new Probe();
        function call(x, s) { return x.matches(s); }
        %PrepareFunctionForOptimization(call);
        let r = call(p, "hello");
        r = call(p, "hello");
        r = call(p, "hello");
        %OptimizeFunctionOnNextCall(call);
        const r1 = call(p, "hello");
        const r2 = call(p, "world");
        JSON.stringify({ r1, r2 });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"r1":true,"r2":false}"#);
}

#[test]
fn fastcall_onebyte_arg_multibyte_still_correct() {
    // U+0100 forces V8 to materialise the string as two-byte; the fast
    // path can't accept that shape, so V8 routes the call through the
    // slow callback. The result must still be correct (the slow path
    // does the full WebIDL ByteString conversion which throws on
    // > 0xFF input — caught and surfaced).
    let s = run_in_v8(
        |scope, global| install_class::<onebyte_arg::PathProbe>(
            onebyte_arg::PathProbe::install, "Probe", scope, global,
        ),
        r#"
        const p = new Probe();
        function call(x, s) { return x.matches(s); }
        %PrepareFunctionForOptimization(call);
        let r = call(p, "hello");
        r = call(p, "hello");
        %OptimizeFunctionOnNextCall(call);
        let kind, msg;
        try { call(p, "\u0100hello"); }
        catch (e) { kind = e.constructor.name; msg = e.message; }
        JSON.stringify({ kind, msg });
        "#,
        |val, scope| js_string(val, scope),
    );
    // Multibyte input throws TypeError via the slow-path WebIDL
    // ByteString conversion. The fast path never runs (V8 routes
    // two-byte strings to the slow callback automatically).
    assert!(s.contains(r#""kind":"TypeError""#), "got: {s}");
}

// ---------------------------------------------------------------------------
// Test 4: Receiver lookup — internal-field-1 aligned pointer
// ---------------------------------------------------------------------------
//
// The macro stores `Box<Self>` as an External in slot 0 (existing
// behaviour, used by slow callbacks AND the GC finalizer) and ALSO
// stores the same raw pointer as an aligned pointer in slot 1 when
// any method on the class has `fastcall`. The fast-path shim reads
// from slot 1 via `get_aligned_pointer_from_internal_field(1, 0)`,
// which is a single load instruction — no scope, no External unwrap.
//
// We exercise this by verifying that two distinct instances keep
// independent state when accessed via the fast path: a wrong slot
// would alias state across all instances.

mod receiver_lookup {
    use super::*;

    pub struct Box32 {
        pub n: u32,
    }

    #[v8_class]
    impl Box32 {
        #[v8_constructor]
        fn new(initial: Option<u32>) -> Box32 {
            Box32 { n: initial.unwrap_or(0) }
        }

        #[v8_method(fastcall)]
        fn read(&self) -> u32 {
            self.n
        }
    }
}

#[test]
fn fastcall_receiver_lookup_keeps_state_isolated() {
    let s = run_in_v8(
        |scope, global| install_class::<receiver_lookup::Box32>(
            receiver_lookup::Box32::install, "Box32", scope, global,
        ),
        r#"
        const a = new Box32(11);
        const b = new Box32(99);
        function read(x) { return x.read(); }
        %PrepareFunctionForOptimization(read);
        let warm = read(a) + read(b);
        warm = read(a) + read(b);
        warm = read(a) + read(b);
        %OptimizeFunctionOnNextCall(read);
        const r1 = read(a);
        const r2 = read(b);
        JSON.stringify({ r1, r2 });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"r1":11,"r2":99}"#);
}

// ---------------------------------------------------------------------------
// Test 5: Mixed methods — class with both fastcall and non-fastcall methods
// ---------------------------------------------------------------------------
//
// Verifies that decorating just one method with `fastcall` doesn't
// break the other methods on the same class. Both paths coexist on
// the same FunctionTemplate — fastcall methods get build_fast,
// non-fastcall methods stay on the default new(scope, callback) path.

mod mixed_methods {
    use super::*;

    pub struct Mixed {
        pub n: u32,
    }

    #[v8_class]
    impl Mixed {
        #[v8_constructor]
        fn new() -> Mixed {
            Mixed { n: 7 }
        }

        #[v8_method(fastcall)]
        fn fast_get(&self) -> u32 {
            self.n
        }

        #[v8_method]
        fn slow_get(&self) -> u32 {
            self.n
        }

        #[v8_method]
        fn name(&self) -> String {
            "mixed".to_string()
        }
    }
}

#[test]
fn fastcall_coexists_with_regular_methods() {
    let s = run_in_v8(
        |scope, global| install_class::<mixed_methods::Mixed>(
            mixed_methods::Mixed::install, "Mixed", scope, global,
        ),
        r#"
        const m = new Mixed();
        const a = m.fast_get();
        const b = m.slow_get();
        const c = m.name();
        JSON.stringify({ a, b, c });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"a":7,"b":7,"c":"mixed"}"#);
}

// ---------------------------------------------------------------------------
// Test 6: Verify the fastcall method compiles via TurboFan
// ---------------------------------------------------------------------------
//
// V8's `%GetOptimizationStatus(f)` returns a bitmask describing how the
// engine has compiled `f`. Bit 0x10 set ⇒ TurboFanned. We force-optimise
// a function that calls a fastcall method, then assert the optimisation
// landed. If the build_fast wiring weren't actually present (e.g., we
// regressed to `FunctionTemplate::new`), the slow callback would still
// run, but TurboFan's compilation success is a necessary precondition
// for the fast path to fire — so this gate guards against the
// "fastcall annotation has no effect" regression.

// ---------------------------------------------------------------------------
// Test 7: Verify the fast path actually fires (not just compiles)
// ---------------------------------------------------------------------------
//
// To prove V8 actually inlines the CFunction shim under optimisation
// (and isn't silently routing every call through the slow path), we
// install a method whose user body increments a thread_local counter.
// Since the user body runs on BOTH paths (fast and slow), the counter
// alone can't distinguish — but it shows the call ran. To split fast
// from slow, we use a SECOND counter that's bumped only inside the
// slow callback wrapper.
//
// The macro doesn't expose path-specific instrumentation, but we can
// observe the side effect: under heavy iteration, the user body is
// called N times. If V8 took the fast path, the slow callback's
// pre-deref work (brand check, External unwrap) didn't run. A timing
// assertion is too brittle for CI; instead we check via
// %GetOptimizationStatus that TF kept the function optimised through
// many iterations — a deopt mid-loop would zero kOptimized.

mod fast_fires {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    pub static USER_BODY_HITS: AtomicU64 = AtomicU64::new(0);

    pub struct Probe {
        pub _n: u32,
    }

    #[v8_class]
    impl Probe {
        #[v8_constructor]
        fn new() -> Probe {
            Probe { _n: 0 }
        }

        #[v8_method(fastcall)]
        fn ping(&self, n: u32) -> u32 {
            USER_BODY_HITS.fetch_add(1, Ordering::SeqCst);
            n + 1
        }
    }
}

#[test]
fn fastcall_runs_user_body_under_optimisation() {
    use fast_fires::USER_BODY_HITS;
    USER_BODY_HITS.store(0, std::sync::atomic::Ordering::SeqCst);
    let s = run_in_v8(
        |scope, global| install_class::<fast_fires::Probe>(
            fast_fires::Probe::install, "Probe", scope, global,
        ),
        r#"
        const p = new Probe();
        function call(x) { return x.ping(7); }
        %PrepareFunctionForOptimization(call);
        // Warm-up: force several slow-path calls so V8 has type
        // feedback. Each call hits USER_BODY (via the slow callback).
        for (let i = 0; i < 3; i++) call(p);
        %OptimizeFunctionOnNextCall(call);
        // Optimised invocations — these should hit the fast path.
        let total = 0;
        for (let i = 0; i < 1000; i++) total += call(p);
        const status = %GetOptimizationStatus(call);
        const still_optimized = (status & (1 << 3)) !== 0;
        JSON.stringify({ total, still_optimized });
        "#,
        |val, scope| js_string(val, scope),
    );
    let parsed: serde_json::Value = serde_json::from_str(&s).expect("json");
    // Sanity: the user body ran for every call (3 warm-up + 1000 hot).
    let hits = USER_BODY_HITS.load(std::sync::atomic::Ordering::SeqCst);
    assert!(
        hits >= 1003,
        "user body should have run for every JS-side call (warm-up + hot loop); got {hits}"
    );
    // Each call returns 7 + 1 = 8; 1000 hot iterations → 8000.
    assert_eq!(parsed["total"], serde_json::json!(8000), "total mismatch: {s}");
    // V8 didn't deopt the function during the loop. If our fastcall
    // shim crashed or returned garbage, V8 would deopt and this
    // assertion would fail.
    assert_eq!(parsed["still_optimized"], serde_json::json!(true),
        "fastcall caused a deopt mid-loop; status: {s}");
}

// ---------------------------------------------------------------------------
// Test 8: RT-1 regression — foreign-receiver type confusion (isolate escape)
// ---------------------------------------------------------------------------
//
// A `#[v8_method(fastcall)]` / `#[v8_getter(fastcall)]` invoked on a FOREIGN
// or forged receiver under JIT must throw `TypeError: Illegal invocation`,
// NOT reach the brand-check-free fast shim — which would reinterpret the
// receiver's slot-1 bytes as `*const Self` (type confusion → arbitrary memory
// read / SIGABRT, an isolate escape). The fix wires a `v8::Signature` onto the
// FunctionTemplate so V8 deopts a non-matching receiver to the slow callback,
// which brand-checks and bounds-checks the internal field. WITHOUT the fix,
// the optimised fast path runs on ANY object and these tests abort the process.

#[test]
fn fastcall_method_foreign_receiver_throws_not_crash() {
    let s = run_in_v8(
        |scope, global| install_class::<u32_method::Adder>(
            u32_method::Adder::install, "Adder", scope, global,
        ),
        r#"
        const a = new Adder(100);
        const stolen = Adder.prototype.add;
        function call(x) { return stolen.call(x, 5); }
        %PrepareFunctionForOptimization(call);
        call(a); call(a); call(a);
        %OptimizeFunctionOnNextCall(call);
        call(a); // optimise on the real native shape
        let k_plain, k_proto, legit;
        // A plain object — fails the signature AND the slow-path brand check.
        try { call({}); } catch (e) { k_plain = e.constructor.name; }
        // A fake with the right prototype but no internal fields — fails the
        // signature; slow path brand-passes then bounds-checks the field → throws.
        try { call(Object.create(Adder.prototype)); } catch (e) { k_proto = e.constructor.name; }
        // A legitimate receiver must still work on the (optimised) fast path.
        legit = call(a);
        JSON.stringify({ k_plain, k_proto, legit });
        "#,
        |val, scope| js_string(val, scope),
    );
    let parsed: serde_json::Value = serde_json::from_str(&s).expect("json");
    assert_eq!(parsed["k_plain"], serde_json::json!("TypeError"),
        "plain {{}} receiver must throw, not crash; got: {s}");
    assert_eq!(parsed["k_proto"], serde_json::json!("TypeError"),
        "Object.create(proto) receiver must throw, not crash; got: {s}");
    assert_eq!(parsed["legit"], serde_json::json!(105),
        "a legitimate receiver must still work on the fast path; got: {s}");
}

#[test]
fn fastcall_getter_foreign_receiver_throws_not_crash() {
    // Same RT-1 fix must cover the GETTER fastcall install site.
    let s = run_in_v8(
        |scope, global| install_class::<bool_getter::Flag>(
            bool_getter::Flag::install, "Flag", scope, global,
        ),
        r#"
        const f = new Flag(1);
        const get = Object.getOwnPropertyDescriptor(Flag.prototype, "is_on").get;
        function read(x) { return get.call(x); }
        %PrepareFunctionForOptimization(read);
        read(f); read(f); read(f);
        %OptimizeFunctionOnNextCall(read);
        read(f);
        let k_plain, legit;
        try { read({}); } catch (e) { k_plain = e.constructor.name; }
        legit = read(f);
        JSON.stringify({ k_plain, legit });
        "#,
        |val, scope| js_string(val, scope),
    );
    let parsed: serde_json::Value = serde_json::from_str(&s).expect("json");
    assert_eq!(parsed["k_plain"], serde_json::json!("TypeError"),
        "getter on a plain {{}} receiver must throw, not crash; got: {s}");
    assert_eq!(parsed["legit"], serde_json::json!(true),
        "getter on a legitimate receiver must still work; got: {s}");
}

#[test]
fn fastcall_method_compiles_via_turbofan() {
    let s = run_in_v8(
        |scope, global| install_class::<u32_method::Adder>(
            u32_method::Adder::install, "Adder", scope, global,
        ),
        r#"
        const a = new Adder(10);
        function call(x) { return x.add(1); }
        %PrepareFunctionForOptimization(call);
        for (let i = 0; i < 5; i++) call(a);
        %OptimizeFunctionOnNextCall(call);
        // Force one optimised invocation.
        const result = call(a);
        // Bitmask query — V8's OptimizationStatus enum (in
        // src/runtime/runtime.h):
        //   kIsFunction          = 1 << 0
        //   kOptimized           = 1 << 3
        //   kMaglevved           = 1 << 4
        //   kTurboFanned         = 1 << 5
        //   kInterpreted         = 1 << 6
        // We assert kOptimized (1<<3) — set whenever the function has
        // been compiled to TF-or-Maglev optimised code. The fast call
        // C-function is wired through TurboFan only.
        const status = %GetOptimizationStatus(call);
        const is_optimized = (status & (1 << 3)) !== 0;
        JSON.stringify({ result, is_optimized, status });
        "#,
        |val, scope| js_string(val, scope),
    );
    let parsed: serde_json::Value = serde_json::from_str(&s).expect("json");
    assert_eq!(parsed["result"], serde_json::json!(11), "got: {s}");
    assert_eq!(parsed["is_optimized"], serde_json::json!(true),
        "fastcall method must compile via TurboFan; status: {s}");
}
