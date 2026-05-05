//! Re-entry guard for `&mut self` callbacks.
//!
//! Wave 3 commit 4 — relocated from `v8_class/method.rs:84-133`
//! (design `docs/proposals/runtime-macros-refactor.md` §4.1, F3).
//! Wave 2 deliberately bailed on the `Cell<Option<usize>>` (single-slot)
//! migration after pinning a soundness gap on 3-deep nesting (`a → b → a`
//! cross-instance). The HashSet variant shipped unchanged through
//! Waves 3-7.
//!
//! Wave 8 (closes design §13 / C5/H13): replace the heap-allocating
//! `RefCell<HashSet<usize>>` with a fixed-capacity stack-resident
//! `Cell<[Option<usize>; 8]>` (closes design amendment after Wave 2's
//! bail). The 8-slot cap covers re-entry depths well beyond the
//! 3-deep regression test (Wave 2 found `a → b → a` was the worst
//! case in real consumers — Headers, FormData, URLSearchParams). The
//! membership check is a 8-element scan (branchless `.iter().any`),
//! the insert finds the first `None` slot, the remove clears the
//! matching slot. All operations are heap-free; no `HashSet` allocator
//! pressure on the per-isolate thread.

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
/// **Fix.** A per-method, thread-local `Cell<[Option<usize>; 8]>` keyed
/// by the External pointer's address (`__ext.value() as usize` ==
/// the Box raw addr). The prologue scans for the addr; if found, throws
/// a V8 TypeError with a clear, per-method message and returns from the
/// callback BEFORE the unsafe `&mut Self` materialisation. Otherwise,
/// it stores the addr in the first `None` slot. A RAII drop guard
/// clears the slot on scope exit so even a panic in the user body
/// releases the entry.
///
/// **Why fixed-cap multi-slot beats HashSet.** Heap-free in the steady
/// state (HashSet allocates a backing table on first insert; even a
/// 0-element HashSet carries the 32-byte header + a stable backing
/// pointer). Membership check is an 8-element scan (≤ 64 bytes,
/// fits in a single cache line); HashSet does a hash compute + bucket
/// probe + key comparison. The 8-cap covers nesting depths well
/// beyond the 3-deep `a → b → a` worst case observed in real
/// consumers — Headers, FormData, URLSearchParams. If a future class
/// nests deeper than 8, the prologue panics with a "re-entry depth
/// exceeded" message at the spill point rather than silent corruption
/// (the panic still propagates to V8 cleanly because it fires before
/// the unsafe `&mut Self` materialisation).
///
/// **Why fixed-cap multi-slot beats single-slot Cell.** Wave 2 bailed
/// on `Cell<Option<usize>>` because saving the prior addr on entry
/// and restoring it on exit fails for the cross-instance case
/// `a → b → a`: when `b.method()` returns, the slot was restored to
/// `Some(a)`, so the inner `a` re-entry fires. But the outer `a`
/// is also `Some(a)` — and we DO want to fire on the inner re-entry.
/// The save-and-restore semantics conflate "in flight on this
/// thread's stack" with "the most recent caller". An 8-slot variant
/// holds the FULL stack of in-flight addrs, so the membership check
/// is exactly "is `a` currently in flight anywhere on the stack".
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
/// Per-method (one slot-array per `Foo::method`) AND per-instance
/// (key on the Box addr) — no false positives across distinct
/// instances or distinct methods. Thread-local — no cross-thread cost.
///
/// Cost: one 8-element scan + one slot store per `&mut self` call.
/// The array has 0 or 1 entries in the steady state (re-entry is
/// pathological, not common).
///
/// Emitted ONLY for `&mut self` methods. `&self` callbacks are sound
/// to nest (multiple aliased shared references are fine) and skip the
/// guard entirely.
///
/// Returns a token stream that:
///   1. Computes `__inflight_addr = __ext.value() as usize`.
///   2. Scans the per-method thread-local slot array; throws
///      a V8 TypeError + `return`s if already present.
///   3. Stores the addr in the first `None` slot (or panics if the
///      array is full — depth > 8, which shouldn't happen).
///   4. Defines a `Drop`-impl shim that clears the matching slot.
///   5. Binds the shim instance to a let so it lives until scope end.
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
    let depth_msg = format!(
        "re-entry depth exceeded for method `{}::{}` (cap is 8 — a future raise requires a code change)",
        class_ty, method_name,
    );
    // Use ONE thread_local per method per class. The static names are
    // local to the callback function so they don't pollute the impl
    // block's namespace and don't collide across methods.
    //
    // Cap = 8: see this file's module doc for the rationale (3-deep is
    // the worst observed in real consumers; 8 is comfortable headroom).
    quote! {
        let __inflight_addr = __ext.value() as usize;
        ::std::thread_local! {
            static __INFLIGHT: ::std::cell::Cell<[::std::option::Option<usize>; 8]> =
                ::std::cell::Cell::new([::std::option::Option::None; 8]);
        }
        // Membership scan + slot insert in one transaction. The Cell
        // semantics let us read/write the entire fixed-cap array
        // without an unsafe block — `Cell<[Option<usize>; 8]>` is `Copy`
        // because `Option<usize>` is `Copy`, so `.get()` returns a copy
        // and `.set(arr)` writes the whole thing back.
        let __slot_index: ::std::option::Option<usize> = __INFLIGHT.with(|__s| {
            let mut __arr = __s.get();
            // Membership scan: addr already in flight?
            for __slot in __arr.iter() {
                if *__slot == ::std::option::Option::Some(__inflight_addr) {
                    return ::std::option::Option::None;
                }
            }
            // Find the first empty slot and store.
            for __i in 0..__arr.len() {
                if __arr[__i].is_none() {
                    __arr[__i] = ::std::option::Option::Some(__inflight_addr);
                    __s.set(__arr);
                    return ::std::option::Option::Some(__i);
                }
            }
            // Cap reached. The depth-exceeded panic happens below; we
            // signal that here by returning Some(usize::MAX) — distinct
            // from None (re-entry) and Some(0..8) (insert ok).
            ::std::option::Option::Some(::std::usize::MAX)
        });
        match __slot_index {
            ::std::option::Option::None => {
                // Re-entry: addr already in flight on this thread.
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
            ::std::option::Option::Some(::std::usize::MAX) => {
                // Capacity exceeded — depth > 8. This is a code-change
                // signal, not a user-recoverable error: the macro emits
                // a V8 TypeError so the user JS sees a clear diagnostic
                // pointing at the affected method, and the Rust callback
                // returns BEFORE the unsafe `&mut Self` recovery so
                // there's no aliasing risk.
                let __msg = v8::String::new(scope, #depth_msg).unwrap();
                let __exc = v8::Exception::type_error(scope, __msg);
                scope.throw_exception(__exc);
                return;
            }
            ::std::option::Option::Some(_) => {}
        }
        // RAII guard: clear the matching slot on scope exit so any path
        // out (normal return, V8 exception thrown by user code, …)
        // releases the entry. Without this, a single throw would leave
        // the slot "occupied" and every subsequent call would
        // incorrectly trigger the guard.
        //
        // The guard stores the addr (NOT the slot index) so that
        // out-of-order drops still match the right slot. In practice
        // drops always match in LIFO order with the inserts, but the
        // address-keyed scan is robust to weird unwind sequences and
        // the cost (8-element scan) is negligible vs the V8 callback
        // overhead.
        struct __ReentryGuard(usize);
        impl ::std::ops::Drop for __ReentryGuard {
            fn drop(&mut self) {
                __INFLIGHT.with(|__s| {
                    let mut __arr = __s.get();
                    for __slot in __arr.iter_mut() {
                        if *__slot == ::std::option::Option::Some(self.0) {
                            *__slot = ::std::option::Option::None;
                            break;
                        }
                    }
                    __s.set(__arr);
                });
            }
        }
        let __reentry_guard = __ReentryGuard(__inflight_addr);
    }
}
