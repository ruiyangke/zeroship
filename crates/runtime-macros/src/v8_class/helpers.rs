//! Shared helpers used across the `v8_class` codegen submodules.
//!
//! - `method_callback_ident` — mangle `__<Class>_<method>_callback`.
//! - `gen_param_extractions` — emit per-arg extraction code, treating
//!   `&mut PinScope` and `Local<Object>` as synthetic params.
//! - `parse_params_skipping_self` — collect typed args, dropping the
//!   receiver.
//! - `is_pin_scope_ref`, `is_wrapper_local`, `type_path_contains_segment`,
//!   `outer_ident` — type classification helpers.

use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use std::collections::HashSet;
use syn::{FnArg, ImplItemFn, ReturnType, Type};

use crate::gen_extract;

pub(super) fn method_callback_ident(class_ty: &syn::Ident, method: &syn::Ident) -> syn::Ident {
    format_ident!("__{}_{}_callback", class_ty, method)
}

/// Build the per-arg extraction code, treating `&mut v8::PinScope` (or
/// any `PinScope`-typed reference) as a "synthetic" arg that consumes
/// no JS index. The synthetic arg is reborrowed from the callback's
/// own `scope` AFTER all JS-arg extractions complete, so user methods
/// can pass it on to v8 ops without fighting the borrow checker —
/// crucially, the reborrow happens after the extractions release any
/// implicit borrows that `args.get(idx)` keeps alive (the returned
/// `Local<'s, Value>` borrows from `args`, whose lifetime can unify
/// with `scope`'s in inference; reborrowing `scope` mutably while a
/// `Local<'s>` is alive triggers E0502 — see commit 4c41d... for
/// the regression test case).
///
/// Concrete output for `fn decode(&mut self, scope: &mut PinScope, n:
/// u32)` is:
///   let n: u32 = args.get(0).uint32_value(scope).unwrap_or(0);
///   let scope = &mut *scope;        // synthetic reborrow, shadows param
///
/// `reject_shared_names` is the set of parameter names whose JS-side
/// argument must reject SharedArrayBuffer-backed views with
/// `TypeError` — emitted before the regular extraction so the SAB
/// check fails before any byte copy. Per WebIDL §3.2.21 and the
/// Compression spec's omission of `[AllowShared]`.
pub(super) fn gen_param_extractions(
    params: &[crate::Param],
    reject_shared_names: &HashSet<String>,
) -> Vec<TokenStream2> {
    let mut out = Vec::with_capacity(params.len());
    let mut js_idx: usize = 0;

    // Emit JS-arg extractions FIRST in declared order (skipping
    // synthetic PinScope refs). These use the original `scope` param,
    // so no shadow reborrow is alive yet — `args.get(idx)` is free to
    // produce Locals whose lifetime unifies with the param `scope`.
    for p in params.iter() {
        if is_pin_scope_ref(&p.ty) || is_wrapper_local(&p.ty) {
            continue;
        }
        // Optional SAB-rejection guard, emitted *before* the regular
        // extraction so the TypeError fires before any byte copy. The
        // guard checks both ArrayBufferView (Uint8Array etc.) and bare
        // SharedArrayBuffer arguments, matching the IDL `BufferSource`
        // union surface.
        if reject_shared_names.contains(&p.name.to_string()) {
            let idx_lit = js_idx as i32;
            out.push(quote! {
                {
                    let __reject_arg = args.get(#idx_lit);
                    let mut __is_shared = false;
                    if let Ok(__view) =
                        v8::Local::<v8::ArrayBufferView>::try_from(__reject_arg)
                    {
                        if let Some(__buf) = __view.buffer(scope) {
                            if __buf.is_shared_array_buffer() {
                                __is_shared = true;
                            }
                        }
                    } else if v8::Local::<v8::SharedArrayBuffer>::try_from(__reject_arg).is_ok() {
                        __is_shared = true;
                    }
                    if __is_shared {
                        let __msg = v8::String::new(
                            scope,
                            "SharedArrayBuffer-backed buffer source is not allowed",
                        )
                        .unwrap();
                        let __exc = v8::Exception::type_error(scope, __msg);
                        scope.throw_exception(__exc);
                        return;
                    }
                }
            });
        }
        out.push(gen_extract(js_idx, &p.name, &p.ty));
        js_idx += 1;
    }

    // The user method takes the synthetic param BY NAME — usually
    // literally `scope`, but could be any identifier. If the user
    // chose a name OTHER than `scope`, we need to rebind it so the
    // call-site can pass it through. For the common case (`scope`),
    // shadowing is unnecessary because the callback parameter is
    // already named `scope` and the user method body refers to it
    // verbatim. Skip the shadowing in that case to avoid borrow-check
    // conflicts when JS args' Locals are still alive (their lifetime
    // unifies with `scope`'s, and a mutable reborrow while a Local is
    // alive is E0502).
    for p in params.iter() {
        if is_pin_scope_ref(&p.ty) {
            let name = &p.name;
            if name != "scope" {
                out.push(quote! { let #name = &mut *scope; });
            }
        }
    }

    // Bind synthetic `wrapper: v8::Local<v8::Object>` (or any
    // `v8::Local<v8::Object>` typed param) to `args.this()`. Methods
    // that need to register themselves with the runtime (e.g.
    // WebSocket.send registering the wrapper Global for event
    // dispatch) take this synthetic. Idempotent: `args.this()` is
    // cheap to call repeatedly.
    for p in params.iter() {
        if is_wrapper_local(&p.ty) {
            let name = &p.name;
            out.push(quote! { let #name: v8::Local<v8::Object> = args.this(); });
        }
    }

    out
}

/// True for `&mut v8::PinScope<'_, '_>` and similar reference forms.
/// We don't bother distinguishing `&` vs `&mut` — V8 ops universally
/// require `&mut`, and the type alias system means PinScope appears
/// in many shapes (with/without lifetime params, with/without the v8::
/// prefix).
fn is_pin_scope_ref(ty: &Type) -> bool {
    if let Type::Reference(r) = ty {
        return type_path_contains_segment(&r.elem, "PinScope");
    }
    false
}

/// True for `v8::Local<v8::Object>` typed params — synthetic that
/// gets bound to `args.this()`. Used by methods that need access
/// to the JS wrapper itself (e.g. to register a Global for use by
/// async event-dispatch paths). Distinct from is_pin_scope_ref:
/// no reference form, just the bare Local<Object>.
fn is_wrapper_local(ty: &Type) -> bool {
    let Type::Path(tp) = ty else {
        return false;
    };
    let last = match tp.path.segments.last() {
        Some(s) => s,
        None => return false,
    };
    if last.ident != "Local" {
        return false;
    }
    let syn::PathArguments::AngleBracketed(args) = &last.arguments else {
        return false;
    };
    // Look for v8::Object as the last generic arg.
    args.args.iter().any(|arg| {
        if let syn::GenericArgument::Type(inner) = arg {
            return type_path_contains_segment(inner, "Object");
        }
        false
    })
}

fn type_path_contains_segment(ty: &Type, target: &str) -> bool {
    if let Type::Path(tp) = ty {
        return tp
            .path
            .segments
            .iter()
            .any(|s| s.ident == target);
    }
    false
}

pub(super) fn parse_params_skipping_self(f: &ImplItemFn) -> Vec<crate::Param> {
    f.sig
        .inputs
        .iter()
        .filter_map(|arg| {
            if let FnArg::Typed(pt) = arg {
                if let syn::Pat::Ident(pi) = &*pt.pat {
                    return Some(crate::Param {
                        name: pi.ident.clone(),
                        ty: (*pt.ty).clone(),
                    });
                }
            }
            None
        })
        .collect()
}

pub(super) fn outer_ident(output: &ReturnType) -> Option<String> {
    match output {
        ReturnType::Default => None,
        ReturnType::Type(_, ty) => crate::type_ident(ty),
    }
}
