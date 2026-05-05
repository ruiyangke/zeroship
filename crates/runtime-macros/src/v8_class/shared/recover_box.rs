//! Shared brand-check + External-recovery preamble.
//!
//! Closes design `docs/proposals/runtime-macros-refactor.md` §3.8's
//! 7-site duplication of the same ~10-LOC prologue across
//! `gen_method_callback`, `gen_setter_callback`,
//! `gen_same_object_getter_callback`, `gen_async_method_callback`,
//! and three sites in `v8_iterable.rs` (factory, forEach, next).
//!
//! The 4 sites in `v8_class/method.rs` collapse to a single
//! [`gen_recover_box`] call that emits the whole prologue (brand check
//! → External recovery → re-entry guard → unsafe `&mut Self` /
//! `&Self` materialisation). The 3 sites in `v8_iterable.rs` and
//! `gen_async_method_callback` interleave bespoke logic between the
//! recovery steps, so they delegate to the smaller building blocks
//! [`gen_brand_check_throw`] and [`gen_recover_external`].
//!
//! Byte-identity contract: every helper here emits tokens that are
//! lexically identical to the hand-rolled prologues they replace. The
//! insta snapshot suite locks the contract; any drift is treated as a
//! regression per design §5.1.2's "structural change" classifier.

use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};

use super::super::method::gen_reentry_guard;

/// Emit the standalone brand-check prelude:
///
/// ```ignore
/// let __this = args.this();
/// if !#brand_check_fn(scope, __this) {
///     let __msg = v8::String::new(scope, "Illegal invocation").unwrap();
///     let __exc = v8::Exception::type_error(scope, __msg);
///     scope.throw_exception(__exc);
///     return;
/// }
/// ```
///
/// The caller passes the brand-check function ident — usually
/// `__brand_check_<Class>` for the class's own callbacks, but
/// `v8_iterable`'s factory/forEach codegen passes the PARENT class's
/// ident (the iterator companion uses its parent's brand for the
/// receiver-shape check on `args.this()`).
pub(crate) fn gen_brand_check_throw(brand_check_fn: &syn::Ident) -> TokenStream2 {
    quote! {
        let __this = args.this();
        if !#brand_check_fn(scope, __this) {
            let __msg = v8::String::new(scope, "Illegal invocation").unwrap();
            let __exc = v8::Exception::type_error(scope, __msg);
            scope.throw_exception(__exc);
            return;
        }
    }
}

/// Emit the External-recovery block that binds `__ext` from internal
/// field 0 of `__this`, throwing "Illegal invocation" on miss:
///
/// ```ignore
/// let __ext = match __this.get_internal_field(scope, 0)
///     .and_then(|v| v8::Local::<v8::External>::try_from(v).ok())
/// {
///     Some(e) => e,
///     None => {
///         let __msg = v8::String::new(scope, "Illegal invocation").unwrap();
///         let __exc = v8::Exception::type_error(scope, __msg);
///         scope.throw_exception(__exc);
///         return;
///     }
/// };
/// ```
///
/// The block is emit-time invariant — same tokens at every callsite.
/// Caller-provided bindings: `__this`, `scope`. Caller is responsible
/// for whatever they materialise from `__ext.value()` afterwards (a
/// `&mut #state_ty`, a `*const #state_ty`, a `usize`).
pub(crate) fn gen_recover_external() -> TokenStream2 {
    quote! {
        let __ext = match __this.get_internal_field(scope, 0)
            .and_then(|v| v8::Local::<v8::External>::try_from(v).ok())
        {
            Some(e) => e,
            None => {
                let __msg = v8::String::new(scope, "Illegal invocation").unwrap();
                let __exc = v8::Exception::type_error(scope, __msg);
                scope.throw_exception(__exc);
                return;
            }
        };
    }
}

/// Emit the full preamble — brand check + External recovery + re-entry
/// guard + unsafe `&mut Self` (or `&Self`) materialisation — for the
/// common case of a `#[v8_class]` instance callback with simple
/// receiver shape.
///
/// Caller-provided bindings: `scope`, `args`. Emits `__this`, `__ext`,
/// `__instance` as locals.
///
/// Used by the four method.rs sites whose preamble is byte-identical:
/// `gen_method_callback`, `gen_setter_callback`, the cache-miss branch
/// of `gen_same_object_getter_callback`, and any future plain-getter
/// emit. Sites that interleave bespoke logic between the steps
/// (private-symbol cache-check, async resolver allocation, iterator
/// state-fetch) compose the lower-level helpers above instead.
pub(crate) fn gen_recover_box(
    class_ty: &syn::Ident,
    state_ty: &syn::Ident,
    method_name: &syn::Ident,
    mut_receiver: bool,
) -> TokenStream2 {
    let brand_check_fn = format_ident!("__brand_check_{}", class_ty);
    let brand = gen_brand_check_throw(&brand_check_fn);
    let external = gen_recover_external();
    let reentry_guard = gen_reentry_guard(class_ty, method_name, mut_receiver);
    quote! {
        #brand
        #external
        #reentry_guard
        let __instance = unsafe { &mut *(__ext.value() as *mut #state_ty) };
    }
}
