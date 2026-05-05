//! Per-call re-entrancy guard for `&mut self` `value_pairs` callbacks.
//!
//! Wave 9 split — extracted from `crates/runtime-macros/src/
//! v8_iterable.rs`'s 1,368-LOC god file. This is the iterable-side
//! analogue of `v8_class/emit/reentry_guard.rs`. Wave 8 (closes
//! design §13 / C5/H13) introduced the multi-slot Cell shape; this
//! helper emits the same pattern for `value_pairs(&mut self)`.
//!
//! Caller-site contract: the emitted block expects `__inflight_addr`
//! and `scope` to be in scope. On entry: insert the address into a
//! per-class thread-local; on hit (re-entry detected) throw a V8
//! TypeError. On scope exit: RAII removes the address.

use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::Ident;

use crate::must_str;

/// Emit the multi-slot Cell-based re-entrancy guard for `&mut self`
/// `value_pairs`. Returns an empty TokenStream for `&self` shapes
/// (no guard needed — multiple aliased `&Self` borrows are sound).
pub(super) fn gen_iter_reentry_guard(class_ty: &Ident, is_mut: bool) -> TokenStream2 {
    if !is_mut {
        return quote! {};
    }

    let scope_tok = quote! { scope };
    let err_msg = format!(
        "re-entered `value_pairs` on {} instance — concurrent &mut self callback",
        class_ty
    );
    let depth_msg = format!(
        "re-entry depth exceeded for `value_pairs` on {} (cap is 8 — a future raise requires a code change)",
        class_ty
    );
    let err_msg_init = must_str(&scope_tok, &quote! { #err_msg });
    let depth_msg_init = must_str(&scope_tok, &quote! { #depth_msg });

    quote! {
        ::std::thread_local! {
            static __ZS_VALUE_PAIRS_INFLIGHT: ::std::cell::Cell<
                [::std::option::Option<usize>; 8],
            > = ::std::cell::Cell::new([::std::option::Option::None; 8]);
        }
        let __slot_index: ::std::option::Option<usize> = __ZS_VALUE_PAIRS_INFLIGHT
            .with(|__s| {
                let mut __arr = __s.get();
                for __slot in __arr.iter() {
                    if *__slot == ::std::option::Option::Some(__inflight_addr) {
                        return ::std::option::Option::None;
                    }
                }
                for __i in 0..__arr.len() {
                    if __arr[__i].is_none() {
                        __arr[__i] = ::std::option::Option::Some(__inflight_addr);
                        __s.set(__arr);
                        return ::std::option::Option::Some(__i);
                    }
                }
                ::std::option::Option::Some(::std::usize::MAX)
            });
        match __slot_index {
            ::std::option::Option::None => {
                let __msg = #err_msg_init;
                let __exc = v8::Exception::type_error(scope, __msg);
                scope.throw_exception(__exc);
                return;
            }
            ::std::option::Option::Some(::std::usize::MAX) => {
                let __msg = #depth_msg_init;
                let __exc = v8::Exception::type_error(scope, __msg);
                scope.throw_exception(__exc);
                return;
            }
            ::std::option::Option::Some(_) => {}
        }
        struct __ReentryGuard(usize);
        impl ::std::ops::Drop for __ReentryGuard {
            fn drop(&mut self) {
                __ZS_VALUE_PAIRS_INFLIGHT.with(|__s| {
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
