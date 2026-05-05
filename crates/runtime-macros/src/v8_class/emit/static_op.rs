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
use super::super::parse::extract_reject_shared;
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
pub(crate) fn gen_static_callback(cfg: &ClassConfig, m: &ClassMethod) -> TokenStream2 {
    let class_ty = cfg.class_ty;
    let state_ty = cfg.state_ty;
    let method_name = &m.func.sig.ident;
    let callback_name = method_callback_ident(class_ty, method_name);

    // Static methods take no `self`, so `parse_params_skipping_self`
    // collects every param verbatim.
    let params = parse_params_skipping_self(m.func);
    let reject_shared_names = extract_reject_shared(&m.func.attrs);
    let extractions = gen_param_extractions(&params, &reject_shared_names);

    let call_args: Vec<&syn::Ident> = params.iter().map(|p| &p.name).collect();
    // Static method bodies live on the impl receiver (state_ty), which
    // under `#[v8_state_marker(MarkerTy)]` is the StateTy struct, not the
    // unit MarkerTy. Mirror what gen_method_callback does for instance
    // methods — see the Phase 1 commit (`d4d65fd`) that introduced the
    // same threading for `&self` / `&mut self` callbacks.
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
