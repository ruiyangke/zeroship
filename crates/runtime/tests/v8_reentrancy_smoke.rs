//! Smoke tests for the `&mut self` re-entry guard.
//!
//! When a `&mut self` method's body invokes a user-supplied JS callback
//! (e.g. `Local<Function>::call`, fired event handler, `dispatchEvent`),
//! the callback can synchronously re-enter the SAME instance via the
//! prototype: `this.method(...)` from JS. Pre-fix, the macro
//! materialised a fresh `&mut Self` from the External pointer in
//! internal field 0 on every call — re-entry produced two aliased
//! `&mut Self` references, which is UB. The cryptic symptom (when the
//! user wrapped state in an inner `RefCell`) was a `RefCell already
//! mutably borrowed` panic from deep inside V8.
//!
//! The fix in `gen_reentry_guard` emits a per-method, per-instance
//! thread-local `RefCell<HashSet<usize>>` keyed by the External addr.
//! On entry: insert; if already present, throw a V8 TypeError with a
//! clear message and return BEFORE the unsafe `&mut Self`
//! materialisation. On scope exit (RAII drop guard): remove.
//!
//! Wave 2 explored a single-slot `Cell<Option<usize>>` as a memory
//! optimisation but reverted the migration after discovering a
//! soundness gap for the 3-deep nesting case `a → b → a` (see
//! `nested_cross_instance_then_same_instance_throws` below — the
//! regression test that pinned the gap). HashSet ships unchanged.
//!
//! Implementation note: we throw a V8 TypeError rather than `panic!`
//! because Rust's panic runtime can't unwind through V8's C++ frames
//! (SIGABRT on Linux). The user-facing message is still way clearer
//! than the pre-fix cryptic RefCell-already-mutably-borrowed panic.
//!
//! Coverage:
//!   - Synchronous re-entry from a JS callback throws a TypeError
//!     with the macro-emitted message naming the class and method.
//!   - The guard is per-instance: re-entering a DIFFERENT instance from
//!     a callback does NOT throw.
//!   - `&self` methods are unaffected (no guard emitted, no throw on
//!     re-entry).
//!   - The guard releases on RAII drop: a re-entry throw on instance A
//!     doesn't leave instance A "occupied" — a fresh outer call on A
//!     after the throw works again.
#![allow(unsafe_code)]

use std::cell::Cell;

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

fn js_string(val: v8::Local<v8::Value>, scope: &mut v8::PinScope) -> String {
    val.to_rust_string_lossy(scope)
}

// ---------------------------------------------------------------------------
// Test class.
//
// `Reenterable` exposes:
//   - `tickle(&mut self)` — calls back into a JS callback stored in
//     a thread-local stash. The callback may re-enter the same instance.
//   - `set_callback(&mut self, fn)` — stash a JS function as the
//     "callback" tickle invokes.
//   - `peek(&self)` — `&self`, NOT guarded; can re-enter freely.
// ---------------------------------------------------------------------------

thread_local! {
    /// Stash for the JS callback `tickle()` invokes. Keyed by the
    /// instance address so the per-test setup can install distinct
    /// callbacks for distinct instances. Empty between tests (each
    /// test runs its own isolate).
    static CALLBACKS: std::cell::RefCell<
        std::collections::HashMap<usize, v8::Global<v8::Function>>,
    > = std::cell::RefCell::new(std::collections::HashMap::new());
}

mod reentry_class {
    use super::*;

    pub struct Reenterable {
        pub tag: u32,
        // A simple counter to demonstrate state mutation per call.
        pub seen: Cell<u32>,
    }

    #[v8_class]
    impl Reenterable {
        #[v8_constructor]
        fn new(tag: Option<u32>) -> Reenterable {
            Reenterable {
                tag: tag.unwrap_or(0),
                seen: Cell::new(0),
            }
        }

        /// Stash a JS function under this instance's address. Read by
        /// `tickle` when it wants to invoke the callback.
        #[v8_method]
        fn set_callback<'s>(
            &mut self,
            scope: &mut v8::PinScope<'s, '_>,
            f: v8::Local<v8::Value>,
        ) -> bool {
            let addr = self as *mut Reenterable as usize;
            let func: v8::Local<v8::Function> = match f.try_into() {
                Ok(fun) => fun,
                Err(_) => return false,
            };
            let g = v8::Global::new(scope, func);
            CALLBACKS.with(|m| m.borrow_mut().insert(addr, g));
            true
        }

        /// `&mut self` method that calls back into JS. The callback may
        /// re-enter THIS instance via the prototype (`this.tickle()`)
        /// — the macro guard catches that and throws TypeError. Or
        /// another instance's tickle (cross-instance, allowed).
        #[v8_method]
        fn tickle<'s>(&mut self, scope: &mut v8::PinScope<'s, '_>) -> u32 {
            self.seen.set(self.seen.get() + 1);
            let addr = self as *mut Reenterable as usize;
            let cb = CALLBACKS.with(|m| m.borrow().get(&addr).cloned());
            if let Some(g) = cb {
                let f = v8::Local::new(scope, &g);
                let undef: v8::Local<v8::Value> = v8::undefined(scope).into();
                // Call the JS callback. If it re-enters the same
                // instance synchronously, the macro's re-entry guard
                // throws a TypeError. The exception propagates back
                // up through `f.call`'s pending-exception machinery.
                let _ = f.call(scope, undef, &[]);
            }
            self.tag
        }

        /// `&self` — no guard emitted. Re-entry on `&self` is sound:
        /// multiple aliased shared references are fine.
        #[v8_method]
        fn peek(&self) -> u32 {
            self.seen.get()
        }
    }
}

// ---------------------------------------------------------------------------
// Re-entry from a JS callback into the SAME method on the SAME instance
// must throw a TypeError with the macro's message naming the class and
// method (NOT a cryptic RefCell error, NOT a Rust panic).
// ---------------------------------------------------------------------------

#[test]
fn reentry_same_method_same_instance_throws_typeerror() {
    // Catch the re-entry exception in JS so we can read the message and
    // assert on its shape. The OUTER tickle returns its tag (7);
    // confirming the body kept running after the inner throw shows the
    // RAII drop guard correctly released the addr after the inner call.
    let s = run_in_v8(
        |scope, global| {
            install_class::<reentry_class::Reenterable>(
                reentry_class::Reenterable::install,
                "Reenterable",
                scope,
                global,
            );
        },
        r#"
        const r = new Reenterable(7);
        let innerKind, innerMsg;
        // Install a callback that re-enters tickle synchronously.
        r.set_callback(() => {
            try { r.tickle(); }
            catch (e) {
                innerKind = e.constructor.name;
                innerMsg  = e.message;
            }
        });
        const outerTag = r.tickle();   // outer tickle still returns its tag.
        JSON.stringify({
            outerTag,
            innerKind,
            innerMsgHasMethod: innerMsg.indexOf("Reenterable::tickle") !== -1,
            innerMsgHasMutSelf: innerMsg.indexOf("&mut self") !== -1,
        });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(
        s,
        r#"{"outerTag":7,"innerKind":"TypeError","innerMsgHasMethod":true,"innerMsgHasMutSelf":true}"#
    );
}

// ---------------------------------------------------------------------------
// Cross-instance re-entry: instance A's callback calls instance B's
// `tickle`. The guard keys on the instance address, so distinct
// instances do NOT trigger the throw.
// ---------------------------------------------------------------------------

#[test]
fn cross_instance_reentry_does_not_throw() {
    let result = run_in_v8(
        |scope, global| {
            install_class::<reentry_class::Reenterable>(
                reentry_class::Reenterable::install,
                "Reenterable",
                scope,
                global,
            );
        },
        r#"
        const a = new Reenterable(11);
        const b = new Reenterable(22);
        // a's callback calls b.tickle() — cross-instance, sound.
        a.set_callback(() => { b.tickle(); });
        // b's callback calls nothing.
        b.set_callback(() => { });
        const aTag = a.tickle();
        // If the guard misfired, the call would throw and aTag would
        // be undefined; assert the well-known tag instead.
        aTag;
        "#,
        |val, scope| val.uint32_value(scope).unwrap(),
    );
    assert_eq!(result, 11);
}

// ---------------------------------------------------------------------------
// `&self` re-entry: peek doesn't take `&mut self`, so no guard is
// emitted. Recursive nested reads are sound.
// ---------------------------------------------------------------------------

#[test]
fn shared_self_reentry_is_unaffected() {
    let result = run_in_v8(
        |scope, global| {
            install_class::<reentry_class::Reenterable>(
                reentry_class::Reenterable::install,
                "Reenterable",
                scope,
                global,
            );
        },
        r#"
        // peek is &self — multiple "in-flight" calls are fine because
        // there's no &mut Self to alias. The guard is not emitted.
        const r = new Reenterable(99);
        function recur(n) {
            if (n === 0) return r.peek();
            // Re-enter peek transitively; result must equal r.peek()
            // (which is 0 since peek doesn't mutate).
            return recur(n - 1) + r.peek();
        }
        recur(3);
        "#,
        |val, scope| val.uint32_value(scope).unwrap(),
    );
    // peek returns 0 every time (the seen counter is bumped only by
    // tickle, which we never call here). 4 nested invocations × 0 = 0.
    assert_eq!(result, 0);
}

// ---------------------------------------------------------------------------
// RAII drop releases the addr after the throw: a SECOND outer call on
// the same instance succeeds (the inner re-entry threw and unwound,
// the drop guard removed the addr from the in-flight set).
// ---------------------------------------------------------------------------

#[test]
fn guard_releases_after_throw() {
    let s = run_in_v8(
        |scope, global| {
            install_class::<reentry_class::Reenterable>(
                reentry_class::Reenterable::install,
                "Reenterable",
                scope,
                global,
            );
        },
        r#"
        const r = new Reenterable(42);
        // First tickle: callback re-enters, inner throws, outer continues.
        r.set_callback(() => { try { r.tickle(); } catch (_) {} });
        const first  = r.tickle();
        // Reset callback so the second outer tickle doesn't recurse.
        r.set_callback(() => {});
        // Second tickle: must NOT throw (the in-flight set is empty
        // again because the first call's drop guard ran).
        const second = r.tickle();
        JSON.stringify({ first, second });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"first":42,"second":42}"#);
}

// ---------------------------------------------------------------------------
// Restore-prior nesting: A → B → A. With the post-Wave-2 single-slot
// `Cell<Option<usize>>`, the guard's correctness depends on the drop
// guard restoring the PRIOR value (not just None) so that, after the
// inner B-call's drop guard fires, the slot is `Some(a_addr)` again —
// which means a synchronous attempt to re-enter A from inside B's body
// MUST still fire the guard (A is still in-flight on the call stack).
// This pins the cross-instance + same-method nesting semantic that the
// HashSet variant got "for free" (`HashSet.contains(a_addr)` after B's
// `remove(b_addr)` is still true). With the Cell variant, the
// correctness comes from the restore-prior-on-drop pattern.
// ---------------------------------------------------------------------------

#[test]
fn nested_cross_instance_then_same_instance_throws() {
    let s = run_in_v8(
        |scope, global| {
            install_class::<reentry_class::Reenterable>(
                reentry_class::Reenterable::install,
                "Reenterable",
                scope,
                global,
            );
        },
        r#"
        const a = new Reenterable(1);
        const b = new Reenterable(2);
        let innerKind = null, innerMsg = null;
        // a's callback calls b.tickle (cross-instance, allowed). b's
        // callback then tries to re-enter a.tickle synchronously
        // — that MUST throw the per-method TypeError because a is
        // still in flight on this thread's call stack.
        a.set_callback(() => { b.tickle(); });
        b.set_callback(() => {
            try { a.tickle(); }
            catch (e) {
                innerKind = e.constructor.name;
                innerMsg  = e.message;
            }
        });
        const outer = a.tickle();
        JSON.stringify({
            outer,
            innerKind,
            innerHasMethod: innerMsg && innerMsg.indexOf("Reenterable::tickle") !== -1,
        });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(
        s,
        r#"{"outer":1,"innerKind":"TypeError","innerHasMethod":true}"#
    );
}

// ---------------------------------------------------------------------------
// Slot fully restores after nested return. After the A→B→A throw test,
// the drop chain MUST leave __INFLIGHT[tickle] = None. A fresh top-level
// call on either A or B must NOT throw.
// ---------------------------------------------------------------------------

#[test]
fn slot_clears_after_nested_unwind() {
    let s = run_in_v8(
        |scope, global| {
            install_class::<reentry_class::Reenterable>(
                reentry_class::Reenterable::install,
                "Reenterable",
                scope,
                global,
            );
        },
        r#"
        const a = new Reenterable(11);
        const b = new Reenterable(22);
        a.set_callback(() => { b.tickle(); });
        b.set_callback(() => {});
        // Outer A → inner B → done. After the outer unwinds, the
        // thread's __INFLIGHT[tickle] slot MUST be None again.
        const first = a.tickle();
        // Same instances, fresh top-level call. The Cell-restore-prior
        // semantics are correct iff this returns 11 (no false-positive
        // re-entry throw).
        a.set_callback(() => {});  // make a's callback a no-op
        const second = a.tickle();
        // A *different* instance also works (no shared bookkeeping
        // across instances).
        b.set_callback(() => {});
        const third = b.tickle();
        JSON.stringify({ first, second, third });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"first":11,"second":11,"third":22}"#);
}
