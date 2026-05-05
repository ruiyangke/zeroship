//! Re-entry guard for `&mut self` callbacks.
//!
//! Wave 3 commit 4 — relocated from `v8_class/method.rs:84-133`
//! (design `docs/proposals/runtime-macros-refactor.md` §4.1, F3).
//! Wave 2 deliberately bailed on the `Cell<Option<usize>>` migration
//! (design §3.4 / §9.3 multi-method keying) — the HashSet variant
//! ships unchanged here.

use proc_macro2::TokenStream as TokenStream2;
use quote::quote;

/// Re-entry guard for `&mut self` methods.
///
/// **Problem.** A `&mut self` method recovers `&mut Self` from the
/// External pointer in internal field 0. If the user body calls back
/// into JS (e.g. `Local<Function>::call`, fired-event handler) and the
/// callback synchronously re-enters the SAME instance via the prototype,
/// the macro materialises ANOTHER `&mut Self` pointing at the same Box.
/// That's aliased mutable references — UB. Pre-fix, the symptom was a
/// cryptic `RefCell already mutably borrowed` panic from deep inside V8
/// when the user's body wrapped state in an inner `RefCell`; classes
/// without an inner cell silently corrupted memory.
///
/// **Fix.** A per-method, thread-local `RefCell<HashSet<usize>>` keyed
/// by the External pointer's address (`__ext.value() as usize` ==
/// the Box raw addr). The prologue inserts the addr on entry; if it
/// was already present, throws a V8 TypeError with a clear, per-method
/// message and returns from the callback BEFORE the unsafe `&mut Self`
/// materialisation. A RAII drop guard removes the addr on scope exit so
/// even a panic in the user body releases the entry.
///
/// We throw a V8 TypeError (not a Rust panic) because Rust's panic
/// runtime can't unwind through V8's C++ frames cleanly — the
/// experimental result on Linux is "fatal runtime error: failed to
/// initiate panic, error 5" + SIGABRT. A V8 exception propagates the
/// way every other macro-emitted error already does (see brand check
/// "Illegal invocation"), so the user code observes a JS-side
/// `TypeError` with the diagnostic message. That's still WAY clearer
/// than a cryptic RefCell-borrow panic from inside V8.
///
/// Per-method (one set per `Foo::method`) AND per-instance (key on the
/// Box addr) — no false positives across distinct instances or
/// distinct methods. Thread-local — no cross-thread cost.
///
/// Cost: one HashSet `insert` + one `remove` per `&mut self` call.
/// The set has 0 or 1 entries in the steady state (re-entry is
/// pathological, not common).
///
/// Emitted ONLY for `&mut self` methods. `&self` callbacks are sound
/// to nest (multiple aliased shared references are fine) and skip the
/// guard entirely.
///
/// Returns a token stream that:
///   1. Computes `__inflight_addr = __ext.value() as usize`.
///   2. Tries to insert into the per-method thread-local set; throws
///      a V8 TypeError + `return`s if already present.
///   3. Defines a `Drop`-impl shim that removes the addr.
///   4. Binds the shim instance to a let so it lives until scope end.
///
/// The caller must run this AFTER the External recovery and BEFORE
/// the unsafe `&mut Self` materialisation.
pub(crate) fn gen_reentry_guard(
    class_ty: &syn::Ident,
    method_name: &syn::Ident,
    is_mut_self: bool,
) -> TokenStream2 {
    if !is_mut_self {
        return quote! {};
    }
    let err_msg = format!(
        "re-entered method `{}::{}` on instance — concurrent &mut self callback",
        class_ty, method_name,
    );
    // Use ONE thread_local per method per class. The static names are
    // local to the callback function so they don't pollute the impl
    // block's namespace and don't collide across methods.
    quote! {
        let __inflight_addr = __ext.value() as usize;
        ::std::thread_local! {
            static __INFLIGHT: ::std::cell::RefCell<::std::collections::HashSet<usize>> =
                ::std::cell::RefCell::new(::std::collections::HashSet::new());
        }
        let __already_inflight = __INFLIGHT.with(|__s| !__s.borrow_mut().insert(__inflight_addr));
        if __already_inflight {
            // Throw a V8 TypeError with the diagnostic message. We
            // can't `panic!` here because Rust panic can't unwind
            // through V8's C++ frames (SIGABRT on Linux). A V8
            // exception propagates correctly and surfaces in user JS
            // as a TypeError, which is way clearer than the pre-fix
            // cryptic RefCell-already-mutably-borrowed panic.
            let __msg = v8::String::new(scope, #err_msg).unwrap();
            let __exc = v8::Exception::type_error(scope, __msg);
            scope.throw_exception(__exc);
            return;
        }
        // RAII guard: remove the addr on scope exit so any path out
        // (normal return, V8 exception thrown by user code, …)
        // releases the entry. Without this, a single throw would
        // leave the set "occupied" and every subsequent call would
        // incorrectly trigger the guard.
        struct __ReentryGuard(usize);
        impl ::std::ops::Drop for __ReentryGuard {
            fn drop(&mut self) {
                __INFLIGHT.with(|__s| {
                    __s.borrow_mut().remove(&self.0);
                });
            }
        }
        let __reentry_guard = __ReentryGuard(__inflight_addr);
    }
}
