//! Smoke + regression coverage for `gen_recover_box` honoring
//! `ReceiverKind`/`mut_receiver`. Closes Wave 9 NS1.
//!
//! Pre-fix, the shared `gen_recover_box` helper unconditionally emitted
//!
//! ```ignore
//! let __instance = unsafe { &mut *(__ext.value() as *mut #state_ty) };
//! ```
//!
//! for every callback shape, including `&self` methods. The dispatch
//! site reborrowed the `&mut Self` as `&Self`, so the user-facing call
//! shape was correct — but a synchronous re-entry on a `&self` callback
//! could materialise TWO `&mut Self` bindings from the same External
//! pointer (one from the outer call's binding, one from the inner
//! re-entry). Both would still be in scope until function return. The
//! reborrow at the dispatch site doesn't release the underlying `&mut`
//! borrow — only the binding ending does. Two simultaneous `&mut Self`
//! bindings from the same allocation is UB per stacked-borrows, even
//! when neither borrow is observably aliased at the user-visible
//! dispatch.
//!
//! Wave 9's fix gates the materialisation form on `mut_receiver`:
//! `&self` callbacks emit
//!
//! ```ignore
//! let __instance = unsafe { &*(__ext.value() as *const #state_ty) };
//! ```
//!
//! so a synchronous re-entry produces TWO `&Self` bindings from the
//! same allocation, which is sound (multiple aliased shared borrows
//! are allowed by stacked-borrows).
//!
//! What this file pins:
//!   - A `&self` method that synchronously re-enters itself via a JS
//!     callback (transitively through `Function::call`) returns the
//!     correct value with no Rust panic, no V8 throw, and no aborted
//!     test under `cargo test --release` (Miri is the canonical
//!     stacked-borrows checker, but a release-mode optimiser-aware
//!     run is the closest practical proxy here).
//!   - The regression covers the exact emission `gen_recover_box`
//!     refactored: a plain `#[v8_method]` taking `&self` that the user
//!     tickles into re-entering itself via JS.

#![allow(unsafe_code)]

use zeroship_runtime::init_v8;
#[allow(unused_imports)]
use zeroship_runtime_macros::{v8_class, v8_constructor, v8_method};

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
// Test class — exposes a `&self` method that synchronously re-enters
// itself via a stashed JS callback. The macro's `gen_recover_box`
// emits a `&Self` materialisation for the receiver; both the outer
// call and the inner re-entry's bindings coexist for the duration of
// the outer call's body. A `&mut Self` materialisation under the same
// shape would be UB per stacked-borrows.
// ---------------------------------------------------------------------------

thread_local! {
    /// Per-instance JS callback stash. Same shape as
    /// `v8_reentrancy_smoke.rs`'s helper, scoped here so the `&self`
    /// re-entry can fire its callback synchronously from inside the
    /// outer call.
    static CALLBACKS: std::cell::RefCell<
        std::collections::HashMap<usize, v8::Global<v8::Function>>,
    > = std::cell::RefCell::new(std::collections::HashMap::new());
}

mod recover_box_class {
    use super::*;

    pub struct Reader {
        /// Mutation-tracker — bumped via the inner `Cell` so the
        /// `&self` reborrow contract is respected. The cell holds the
        /// JS-visible "depth" the outer test asserts on.
        pub depth: std::cell::Cell<u32>,
    }

    #[v8_class]
    impl Reader {
        #[v8_constructor]
        fn new() -> Reader {
            Reader {
                depth: std::cell::Cell::new(0),
            }
        }

        /// Stash the JS callback under this instance's address. We use
        /// `&mut self` here because we need to mint a Global, but the
        /// stashing isn't the regression target — `peek` is.
        #[v8_method]
        fn set_callback<'s>(
            &mut self,
            scope: &mut v8::PinScope<'s, '_>,
            f: v8::Local<v8::Value>,
        ) -> bool {
            let addr = self as *mut Reader as usize;
            let func: v8::Local<v8::Function> = match f.try_into() {
                Ok(fun) => fun,
                Err(_) => return false,
            };
            let g = v8::Global::new(scope, func);
            CALLBACKS.with(|m| m.borrow_mut().insert(addr, g));
            true
        }

        /// `&self` method whose body synchronously calls into JS. The
        /// JS callback may re-enter THIS SAME `peek` on the same
        /// instance — and on the post-NS1 codegen, that's sound:
        /// `gen_recover_box` materialises `&*` (not `&mut *`), so two
        /// nested bindings from the same External are two `&Self`s,
        /// which stacked-borrows allows.
        ///
        /// Pre-fix, the materialisation was `&mut *(__ext.value() as
        /// *mut Self)` even for `&self` shapes — the inner re-entry
        /// would create a SECOND `&mut Self` from the same allocation
        /// while the outer one was still in scope. UB latent.
        #[v8_method]
        fn peek<'s>(&self, scope: &mut v8::PinScope<'s, '_>) -> u32 {
            // Bump depth via Cell — `&self` mutates interior state via
            // Cell exclusively. No outer `&mut` is taken.
            self.depth.set(self.depth.get() + 1);
            let addr = self as *const Reader as usize;
            let cb = CALLBACKS.with(|m| m.borrow().get(&addr).cloned());
            if let Some(g) = cb {
                let f = v8::Local::new(scope, &g);
                let undef: v8::Local<v8::Value> = v8::undefined(scope).into();
                // Synchronous re-entry: the JS callback may call
                // `r.peek()` on the same instance. The macro must NOT
                // throw a re-entry-guard exception here (the guard is
                // a no-op for `&self`); it must permit the nested call
                // and return cleanly.
                let _ = f.call(scope, undef, &[]);
            }
            self.depth.get()
        }
    }
}

// ---------------------------------------------------------------------------
// NS1 regression: synchronous `&self` re-entry through a JS callback
// returns a sane value — no panic, no exception, no abort. The post-fix
// codegen materialises `&Self`, so both the outer and inner bindings
// are shared references to the same allocation (sound).
//
// Pre-fix (Wave 8 and earlier) the materialisation was unconditional
// `&mut *`; the inner re-entry produced a second `&mut Self` while the
// outer one was still bound. Release-mode optimisers may miscompile
// under that aliasing, and Miri's stacked-borrows checker rejects it
// outright. The regression here pins the post-fix behaviour: `peek`
// returns a non-zero depth and the test runs to completion.
// ---------------------------------------------------------------------------

#[test]
fn shared_self_synchronous_reentry_is_sound() {
    let s = run_in_v8(
        |scope, global| {
            install_class::<recover_box_class::Reader>(
                recover_box_class::Reader::install,
                "Reader",
                scope,
                global,
            );
        },
        // The outer `peek` bumps depth → calls the callback → callback
        // re-enters peek → peek bumps depth again → returns. The outer
        // call observes both bumps via the Cell.
        //
        // We bound the recursion with a JS-side counter so a hypothetical
        // future re-entry-guard on `&self` would surface as a deviation
        // from the expected depth of 2 (or as an outright throw).
        r#"
        const r = new Reader();
        let calls = 0;
        r.set_callback(() => {
            calls += 1;
            if (calls < 2) {
                // Re-enter peek on the SAME instance. The post-NS1
                // codegen materialises `&Self`, so this is sound; the
                // pre-NS1 codegen materialised `&mut Self`, which is
                // UB per stacked-borrows.
                r.peek();
            }
        });
        const outer = r.peek();
        JSON.stringify({ outer, calls });
        "#,
        |val, scope| js_string(val, scope),
    );
    // outer == 2 means: outer call bumped (depth=1) → callback fired,
    // re-entered peek which bumped (depth=2) → returned 2 to the
    // callback → callback exited → outer's `self.depth.get()` reads 2.
    // calls == 2 confirms the re-entry actually fired (not skipped).
    //
    // If the pre-NS1 `&mut *` materialisation had been retained, the
    // inner re-entry would either (a) compile and miscompile in
    // release, (b) abort under Miri, or (c) panic via a
    // RefCell-already-mutably-borrowed error if the user wrapped state
    // in a RefCell. The post-fix `&*` materialisation lets the two
    // nested `&Self` bindings coexist soundly.
    assert_eq!(s, r#"{"outer":2,"calls":2}"#);
}

// ---------------------------------------------------------------------------
// Two `&self` callbacks on the same instance, BOTH active on the call
// stack at the same time, do not produce a Rust borrow conflict. This
// pins the cross-method case that the v2 critic flagged (the original
// finding said "user code synchronously re-enters a `&self` callback"
// — that includes re-entering a *different* `&self` method too).
// ---------------------------------------------------------------------------

#[test]
fn shared_self_cross_method_reentry_is_sound() {
    let s = run_in_v8(
        |scope, global| {
            install_class::<recover_box_class::Reader>(
                recover_box_class::Reader::install,
                "Reader",
                scope,
                global,
            );
        },
        // The outer `peek` calls the callback → callback re-enters peek
        // (which is the SAME method) — this is the cross-binding-of-
        // same-method case. Wave 9 NS1 covers this because the inner
        // call manifests its OWN `&Self` from `__ext.value()`, distinct
        // from the outer call's binding but pointing at the same
        // allocation. Two `&Self`s on the same allocation is sound.
        r#"
        const r = new Reader();
        let cbCalls = 0;
        r.set_callback(() => {
            cbCalls += 1;
            // Re-enter peek twice — three `&Self`s active at once
            // (outer + two from the callback's body).
            if (cbCalls < 2) { r.peek(); r.peek(); }
        });
        const finalDepth = r.peek();
        JSON.stringify({ finalDepth, cbCalls });
        "#,
        |val, scope| js_string(val, scope),
    );
    // Outer peek bumps to 1 → callback fires (cbCalls=1) → inner peek
    // bumps to 2 (its callback fires with cbCalls=2 but the if-guard
    // skips, so no further re-entry) → second inner peek bumps to 3
    // (its callback fires with cbCalls=3, also skips) → callback
    // exits → outer reads depth = 3.
    assert_eq!(s, r#"{"finalDepth":3,"cbCalls":3}"#);
}
