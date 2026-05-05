//! Codegen for `#[v8_static_method]` and `#[v8_static_getter]` —
//! WebIDL §3.7.4 static operations / attributes.
//!
//! Wave 3 commit 4 — relocated from `v8_class/method.rs:560-600` into
//! the `emit/` cluster (design `docs/proposals/runtime-macros-refactor.md`
//! §4.1, F3).

use proc_macro2::TokenStream as TokenStream2;
use quote::quote;

use super::super::helpers::{
    gen_param_extractions, method_callback_ident, parse_params_skipping_self,
};
use super::super::shared::class_config::ClassConfig;
use super::super::ClassMethod;
use crate::gen_call_return;

/// Codegen for `#[v8_static_method]` / `#[v8_static_getter]` — WebIDL
/// §3.7.4 static operations / attributes. No receiver, no brand check,
/// no internal-field deref. The emitted callback parses JS args, calls
/// the user's free fn (`<Class>::method(args)` syntax), and routes the
/// return value through the standard `gen_call_return` marshaling.
///
/// Static getters re-use the same callback shape as static methods —
/// V8's accessor mechanism invokes the callback with no args, the
/// extraction loop emits no args (the parser skips the receiver, and
/// there's no receiver, so `params` is whatever args the user
/// declared — typically zero for getters).
///
/// **Parameter naming.** Both `class_ty` and `state_ty` are threaded
/// in through the [`ClassConfig`]. `class_ty` keys the callback
/// identifier (`__<class>_<method>_callback`); `state_ty` keys the
/// dispatch (`<state_ty>::method_name(...)`). Under
/// `#[v8_state_marker(MarkerTy)]` the user's `impl` block is `impl
/// StateTy`, not `impl MarkerTy` — so the dispatch must resolve to
/// StateTy even though the macro keys the install on MarkerTy.
/// Without `#[v8_state_marker]` the two are identical (state_ty ==
/// class_ty), so this is a no-op for the common case but mandatory for
/// state-marker support (Phase 1 commit `d4d65fd` + the static-method
/// extension in `1a924d9`). The H12 finding flagged the name `state_ty`
/// as misleading on the static path (no instance state), but renaming
/// would diverge from the instance/setter/async-method codegen paths
/// that share the same parameter; keeping the cross-emit-site
/// consistency is more valuable than the naming nit. Documented here
/// so the next reader sees the rationale rather than reflexively
/// renaming.
pub(crate) fn gen_static_callback(cfg: &ClassConfig, m: &ClassMethod) -> TokenStream2 {
    let class_ty = cfg.class_ty;
    let state_ty = cfg.state_ty;
    let method_name = &m.func.sig.ident;
    let callback_name = method_callback_ident(class_ty, method_name);

    // Static methods take no `self`, so `parse_params_skipping_self`
    // collects every param verbatim.
    let params = parse_params_skipping_self(m.func);
    // Wave 4: pre-parsed by analyse phase; emit just reads.
    let extractions = gen_param_extractions(&params, &m.reject_shared_names);

    let call_args: Vec<&syn::Ident> = params.iter().map(|p| &p.name).collect();
    // Static method bodies live on the impl target (state_ty), which
    // under `#[v8_state_marker(MarkerTy)]` is the StateTy struct, not
    // the unit MarkerTy — see this fn's doc-comment for the parameter
    // naming rationale.
    let call = quote! {
        <#state_ty>::#method_name(#(#call_args),*)
    };

    let call_return = gen_call_return(&call, &m.func.sig.output);

    quote! {
        #[allow(non_snake_case, unused_variables, unused_mut, clippy::needless_borrow)]
        pub(crate) fn #callback_name(
            scope: &mut v8::PinScope,
            args: v8::FunctionCallbackArguments,
            mut rv: v8::ReturnValue,
        ) {
            // No brand check: WebIDL §3.7.4 static operations are
            // invoked with no `this` (or `Class` itself as `this`).
            // No internal-field deref: there's no boxed `Self` to
            // recover from a wrapper instance.
            // No re-entrancy guard: there's no `&mut self` to alias.
            #(#extractions)*
            #call_return
        }
    }
}
