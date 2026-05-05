//! `[SameObject]` getter codegen — `#[v8_getter(same_object)]`.
//!
//! Wave 3 commit 4 — relocated from `v8_class/method.rs:258-373` into
//! the `emit/` cluster (design `docs/proposals/runtime-macros-refactor.md`
//! §4.1, F3). Plain getters route through `emit/method.rs::gen_method_callback`;
//! only the SameObject variant lives here because it interleaves the
//! private-symbol cache check between brand check and External
//! recovery.

use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};

use super::super::helpers::{
    gen_param_extractions, method_callback_ident, parse_params_skipping_self,
};
use super::super::shared::class_config::ClassConfig;
use super::super::shared::recover_box;
use super::super::ClassMethod;
use super::reentry_guard::gen_reentry_guard;

/// Codegen for `#[v8_getter(same_object)]` — WebIDL `[SameObject]`
/// semantics.
///
/// `Request.headers`, `Response.headers`, `URL.searchParams`, and
/// several other WebIDL accessors must return THE SAME JS object across
/// reads on the same instance:
///
/// ```js
/// const h = req.headers;
/// h === req.headers;   // true
/// h === req.headers;   // still true (no fresh object minted)
/// ```
///
/// Without caching, each access would mint a fresh wrapper, breaking
/// userland code that uses `===` identity (e.g. comparing iterators,
/// caching the headers reference, etc.).
///
/// Implementation strategy:
///
/// - Cache on a per-instance V8 Private symbol named
///   `__zs_same_object_<ClassTy>_<getter>`. The symbol is class-scoped
///   so two classes with `headers` getters don't collide on a single
///   shared name (interning of Privates by name across the isolate is
///   irrelevant since reads/writes are per-Object — but the explicit
///   class-prefix is self-documenting).
///
/// - On callback entry: brand check; recover the boxed instance; look
///   up the private symbol on `args.this()`. If present and not
///   `undefined`, return it as the rv and short-circuit (no user
///   method called).
///
/// - On cache miss: invoke the user's `&self`/`&mut self` method,
///   which returns a `v8::Global<v8::Object>`. Convert to Local,
///   stash on the wrapper instance via `set_private`, return the
///   Local as rv.
///
/// User method shape:
/// ```ignore
/// #[v8_getter(same_object)]
/// fn headers(&self, scope: &mut v8::PinScope) -> v8::Global<v8::Object> {
///     // mint and return — invoked at most ONCE per instance lifetime.
/// }
/// ```
///
/// The user method receives a synthetic `&mut PinScope` (so it can
/// build the Object) and returns `Global<Object>`. The macro doesn't
/// pass any positional JS args (getters take none) — the user method
/// can have only `&self` (or `&mut self`) and the optional `scope`
/// param.
///
/// We don't migrate existing classes to this attribute in this PR
/// (Request.headers, Response.headers, URL.searchParams continue to
/// hand-roll their own private-symbol stash for now). The smoke test
/// in `tests/v8_same_object_smoke.rs` proves the macro wiring works.
pub(crate) fn gen_same_object_getter_callback(
    cfg: &ClassConfig,
    m: &ClassMethod,
) -> TokenStream2 {
    let class_ty = cfg.class_ty;
    let state_ty = cfg.state_ty;
    let method_name = &m.func.sig.ident;
    let callback_name = method_callback_ident(class_ty, method_name);

    // Skip the receiver param when extracting JS args. Getters take
    // no positional args; the only param shape we expect is `&self`
    // (+ optional synthetic `scope`). Extractions are emitted but
    // typically empty.
    let params = parse_params_skipping_self(m.func);
    // Wave 4: pre-parsed by analyse phase; emit just reads.
    let extractions = gen_param_extractions(&params, &m.reject_shared_names);
    let call_args: Vec<&syn::Ident> = params.iter().map(|p| &p.name).collect();
    let receiver_ref = if m.mut_receiver {
        quote! { &mut *__instance }
    } else {
        quote! { &*__instance }
    };

    let brand_check_fn = format_ident!("__brand_check_{}", class_ty);
    // §2.7 / §4.1 row 16: the Private symbol is keyed by
    // `(module_path, marker, method)` so two classes with same-named
    // markers in different modules can't collide on a single Private.
    // The `module_path!()` is resolved at the *user crate's* expansion
    // site (we emit the call literally into the user's code), so the
    // qualifier reflects where the class lives. Net string format:
    //   __zs_same_object_<crate::path::to::module>::<MarkerTy>_<method>
    let private_marker_method = format!("{}_{}", class_ty, method_name);
    // Brand check + (cache-miss-path) External recovery + reentry guard
    // are decomposed into the building blocks of `shared::recover_box`
    // because the SameObject private-symbol cache check has to interleave
    // BETWEEN brand check and External recovery — `gen_recover_box`'s
    // all-in-one form would emit the wrong order for that.
    let brand_check = recover_box::gen_brand_check_throw(&brand_check_fn);
    let recover_external = recover_box::gen_recover_external();
    let reentry_guard = gen_reentry_guard(class_ty, method_name, m.mut_receiver);

    quote! {
        #[allow(non_snake_case, unused_variables, unused_mut, clippy::needless_borrow)]
        pub(crate) fn #callback_name(
            scope: &mut v8::PinScope,
            args: v8::FunctionCallbackArguments,
            mut rv: v8::ReturnValue,
        ) {
            // 1. Brand check before touching internal fields. Same
            //    contract as every other generated callback — see
            //    `__brand_check_<ClassTy>`'s doc-comment.
            #brand_check

            // 2. Resolve the per-instance Private symbol for this
            //    getter. `Private::for_api` is interned by name across
            //    the isolate, so the lookup is O(1) after the first
            //    call — V8 returns the same symbol object on repeat
            //    reads with the same name.
            //
            //    Symbol name is qualified by `module_path!()` at the
            //    user crate's expansion site so two classes with
            //    same-named markers in different modules cannot share
            //    a Private (design §2.7).
            let __private_name = ::std::concat!(
                "__zs_same_object_",
                ::std::module_path!(),
                "::",
                #private_marker_method,
            );
            let __key_str = v8::String::new(scope, __private_name).unwrap();
            let __priv = v8::Private::for_api(scope, Some(__key_str));

            // 3. Cache hit short-circuit: if the wrapper has already
            //    minted a Same-Object value, return it without calling
            //    user code. `get_private` returns Some(undefined) when
            //    the slot was never written, so we filter both None
            //    and undefined paths.
            if let Some(__cached) = __this.get_private(scope, __priv) {
                if !__cached.is_undefined() {
                    rv.set(__cached);
                    return;
                }
            }

            // 4. Cache miss: recover Box<Self>, mint the value, stash,
            //    return.
            #recover_external
            // Re-entry guard for `&mut self` SameObject getters. The
            // miss path runs the user method exactly once; if that body
            // re-enters the same instance (e.g. through a JS callback
            // it triggers), the second call would alias `&mut Self`.
            // No-op for the `&self` case (the common shape).
            #reentry_guard
            let __instance = unsafe { &mut *(__ext.value() as *mut #state_ty) };

            #(#extractions)*

            // The user method returns a `v8::Global<v8::Object>` — we
            // own it after the call returns, so we can both stash it
            // (by re-Localising) and use the same Local for the rv.
            let __value: ::v8::Global<::v8::Object> =
                <#state_ty>::#method_name(#receiver_ref, #(#call_args),*);
            let __local: ::v8::Local<::v8::Object> = ::v8::Local::new(scope, &__value);

            // Stash on the wrapper. `set_private` is fallible (returns
            // None on context teardown); we ignore the result — the
            // worst case is the cache stays empty and the user method
            // runs again, which is observable but not unsound. The
            // user method should be idempotent on its own state for
            // the same reason.
            let _ = __this.set_private(scope, __priv, __local.into());

            rv.set(__local.into());
        }
    }
}
