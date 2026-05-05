//! Slow-path FunctionCallback codegen for plain methods, setters, and
//! async methods.
//!
//! Wave 3 commit 4 — relocated from `v8_class/method.rs` into the
//! `emit/` cluster (design `docs/proposals/runtime-macros-refactor.md`
//! §4.1, F3). Each callback shares the same brand-check + External
//! recovery preamble (via `shared::recover_box`) and diverges only in
//! the post-recovery body.

use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};

use super::super::helpers::{
    gen_param_extractions, method_callback_ident, outer_ident, parse_params_skipping_self,
};
use super::super::shared::class_config::ClassConfig;
use super::super::shared::recover_box;
use super::super::{ClassMethod, MethodKind};
use crate::{gen_call_return, must_str};

/// Slow-path FunctionCallback for `#[v8_method]` and plain
/// `#[v8_getter]` (without `same_object`). Brand-check + Box<Self>
/// recovery + (optional) re-entry guard + unsafe materialisation +
/// arg extraction + user-method dispatch + return marshaling.
///
/// Setters dispatch through here too because they're classified as
/// `MethodKind::Setter` and the early-return below routes to
/// [`gen_setter_callback`] which discards the return value.
pub(crate) fn gen_method_callback(cfg: &ClassConfig, m: &ClassMethod) -> TokenStream2 {
    let class_ty = cfg.class_ty;
    let state_ty = cfg.state_ty;
    let method_name = &m.func.sig.ident;
    let callback_name = method_callback_ident(class_ty, method_name);

    // Skip the receiver param when extracting JS args.
    let params = parse_params_skipping_self(m.func);
    // Wave 4: pre-parsed by analyse phase; emit just reads. Closes F8
    // for per-method walks (reject_shared used to be re-extracted at
    // every emit site).
    let extractions = gen_param_extractions(&params, &m.reject_shared_names);

    let call_args: Vec<&syn::Ident> = params.iter().map(|p| &p.name).collect();
    let receiver_ref = if m.mut_receiver {
        quote! { &mut *__instance }
    } else {
        quote! { &*__instance }
    };

    let call = match m.kind {
        MethodKind::Setter => {
            // Setters in V8 are called with one positional arg (the value).
            // We don't emit return marshaling — accessor setters discard.
            return gen_setter_callback(cfg, m);
        }
        _ => quote! {
            <#state_ty>::#method_name(#receiver_ref, #(#call_args),*)
        },
    };

    let call_return = gen_call_return(&call, &m.func.sig.output);

    // WebIDL §3.7 brand check + Box<Self> recovery + (optional)
    // re-entry guard + unsafe `&mut Self` materialisation. See
    // `shared::recover_box::gen_recover_box` for the soundness
    // rationale and the byte-identity contract with the hand-rolled
    // prologue this replaces.
    let recover = recover_box::gen_recover_box(class_ty, state_ty, method_name, m.mut_receiver);

    quote! {
        #[allow(non_snake_case, unused_variables, unused_mut, clippy::needless_borrow)]
        pub(crate) fn #callback_name(
            scope: &mut v8::PinScope,
            args: v8::FunctionCallbackArguments,
            mut rv: v8::ReturnValue,
        ) {
            #recover

            #(#extractions)*
            #call_return
        }
    }
}

/// Slow-path FunctionCallback for `#[v8_setter]` (one positional arg,
/// no return marshaling). Discards the user method's return value at
/// the end; the V8 accessor protocol ignores anything a setter
/// returns.
///
/// Setter return-shape contract (§13.1, enforced at the analyse phase
/// by `is_unit_return` / `is_result_unit_return`):
///   - `()` — call-and-discard.
///   - `Result<(), OpError>` — Ok-discard, Err routed through the
///     standard 6-variant `gen_throw_op_error_arms` dispatch. Pre-fix
///     this site emitted `let _ = setter(...)` which silently swallowed
///     the OpError — the §13.1 finding masked a real bug in
///     `URL::set_href` and `WebSocketImpl::set_binary_type` whose Err
///     arms were unobservable to JS.
///   - Anything else — rejected at compile time in `analyze.rs`'s
///     setter-shape validator.
pub(crate) fn gen_setter_callback(cfg: &ClassConfig, m: &ClassMethod) -> TokenStream2 {
    let class_ty = cfg.class_ty;
    let state_ty = cfg.state_ty;
    let method_name = &m.func.sig.ident;
    let callback_name = method_callback_ident(class_ty, method_name);

    // Setters take exactly one logical param: the new value.
    let params = parse_params_skipping_self(m.func);
    // Wave 4: pre-parsed by analyse phase; emit just reads. Closes F8
    // for per-method walks (reject_shared used to be re-extracted at
    // every emit site).
    let extractions = gen_param_extractions(&params, &m.reject_shared_names);
    let call_args: Vec<&syn::Ident> = params.iter().map(|p| &p.name).collect();
    let receiver_ref = if m.mut_receiver {
        quote! { &mut *__instance }
    } else {
        quote! { &*__instance }
    };
    // WebIDL §3.7 brand check + Box<Self> recovery — same contract as
    // `gen_method_callback`. The setter discards the return value at
    // the end; the prologue itself is byte-identical.
    let recover = recover_box::gen_recover_box(class_ty, state_ty, method_name, m.mut_receiver);

    // §13.1 fix: setters declared as `Result<(), OpError>` route an
    // `Err` arm through the standard 6-variant OpError dispatch so the
    // user's error surfaces as a JS exception instead of being silently
    // swallowed. Unit-returning setters fall through to a plain call
    // (no Result match needed). The shape parser at the analyse site
    // (analyze.rs) already rejects setters whose return type is neither
    // `()` nor `Result<(), _>`, so this branch is exhaustive.
    let is_result = matches!(
        outer_ident(&m.func.sig.output).as_deref(),
        Some("Result")
    );
    let invoke = if is_result {
        let throw = crate::gen_throw_op_error_arms(&quote! { scope }, &quote! { __err });
        quote! {
            match <#state_ty>::#method_name(#receiver_ref, #(#call_args),*) {
                ::std::result::Result::Ok(()) => {}
                ::std::result::Result::Err(__err) => {
                    #throw
                    return;
                }
            }
        }
    } else {
        // Setter returns `()` — call and ignore.
        quote! {
            <#state_ty>::#method_name(#receiver_ref, #(#call_args),*);
        }
    };

    quote! {
        #[allow(non_snake_case, unused_variables, unused_mut, clippy::needless_borrow)]
        pub(crate) fn #callback_name(
            scope: &mut v8::PinScope,
            args: v8::FunctionCallbackArguments,
            mut rv: v8::ReturnValue,
        ) {
            #recover

            #(#extractions)*

            // Setter dispatch — Ok-discard, Err-throw for Result-returning
            // setters; plain call-and-discard for `()` setters. WebIDL
            // §3.7.6 says the setter return value is unobservable to JS,
            // so we never write to `rv`.
            #invoke
        }
    }
}

/// Codegen for `#[v8_async_method]` — emits a sync V8 callback that
/// allocates a Promise, spawns the user's async body via
/// `state.spawned_ops`, and returns the Promise immediately. The pump
/// resolves (or rejects) the promise when the future settles.
///
/// Shape of the emitted callback:
/// ```ignore
/// fn __Foo_method_callback(scope, args, rv) {
///     // 1. Resolve `this` → Box<Foo> via internal field 0.
///     let raw_self_addr = ...;
///     // 2. Extract JS args (using existing gen_param_extractions).
///     let arg_0 = ...;
///     // 3. Allocate resolver + capture Globals.
///     let resolver = v8::PromiseResolver::new(scope).unwrap();
///     let promise = resolver.get_promise(scope);
///     let resolver_global = v8::Global::new(scope, resolver);
///     let wrapper_global = v8::Global::new(scope, args.this());
///     // 4. Pull SharedState off the isolate slot.
///     let state = scope.get_slot::<SharedState>().unwrap().clone();
///     let request_id = state.borrow().executing_request_id;
///     // 5. Build the future.
///     let fut = async move {
///         let _keepalive = wrapper_global; // pin Box<Foo> across .await
///         // SAFETY: see emitted comment.
///         let this: &Foo = unsafe { &*(raw_self_addr as *mut Foo) };
///         let result = this.method(arg_0).await;
///         OpResult::JsValue {
///             resolver: resolver_global,
///             value: result.into_resolve_value(),
///             request_id,
///         }
///     };
///     // 6. Push to spawned_ops + wake pump.
///     state.borrow_mut().spawned_ops.push(Box::pin(fut));
///     if let Some(mut tx) = state.borrow().pump_notify_tx.clone() {
///         let _ = tx.try_send(());
///     }
///     // 7. Return promise.
///     rv.set(promise.into());
/// }
/// ```
///
/// Borrow-safety contract for the emitted code:
///   - `wrapper_global` is captured by value into the future. As long as
///     the future has not dropped, the V8 wrapper Object is reachable;
///     therefore the GC-finalizer that drops the boxed instance cannot
///     fire. The `*mut Self` recovered each poll is valid for the
///     future's lifetime.
///   - The macro REJECTS `&mut self` async methods (see `expand`); the
///     re-acquired pointer is always taken as `&Self`, so two
///     simultaneous polls (or re-entry from a microtask) cannot
///     materialise an aliased `&mut Self`. State that needs to mutate
///     must use `Cell` / `RefCell` — the user's responsibility, not
///     the macro's.
///   - All future captures are owned (`Vec<u8>`, `String`, `Global<…>`,
///     scalar), never borrowed. The future is `'static + !Send`, which
///     matches the single-thread compio invariant.
pub(crate) fn gen_async_method_callback(cfg: &ClassConfig, m: &ClassMethod) -> TokenStream2 {
    let class_ty = cfg.class_ty;
    let state_ty = cfg.state_ty;
    let method_name = &m.func.sig.ident;
    let callback_name = method_callback_ident(class_ty, method_name);

    // Skip the receiver param when extracting JS args.
    let params = parse_params_skipping_self(m.func);
    // Wave 4: pre-parsed by analyse phase; emit just reads. Closes F8
    // for per-method walks (reject_shared used to be re-extracted at
    // every emit site).
    let extractions = gen_param_extractions(&params, &m.reject_shared_names);

    let call_args: Vec<&syn::Ident> = params.iter().map(|p| &p.name).collect();
    let brand_check_fn = format_ident!("__brand_check_{}", class_ty);
    // Async paths can't take `&mut self` (rejected at `expand`) so no
    // re-entry guard is emitted. The brand-check + External-recovery
    // halves are byte-identical to the sync method's prologue; the
    // recovered `__ext.value()` is laundered through `usize` for the
    // async future capture.
    let brand_check = recover_box::gen_brand_check_throw(&brand_check_fn);
    let recover_external = recover_box::gen_recover_external();
    // §4.1 Wave 2 + critique C8: pre-fix this site `.expect`'d on the
    // SharedState slot lookup. A misconfigured runtime (slot not
    // installed) would Rust-panic THROUGH V8's C++ frames, which on
    // Linux is a SIGABRT (Rust's panic runtime can't unwind through an
    // `extern "C"` boundary cleanly — same reasoning as the re-entry
    // guard's V8-TypeError-not-panic doc-comment). Surface as a JS-side
    // RangeError instead — exceptional but recoverable.
    let scope_tok = quote! { scope };
    let state_missing_msg_init = must_str(
        &scope_tok,
        &quote! { "internal error: SharedState not installed on isolate" },
    );

    quote! {
        #[allow(non_snake_case, unused_variables, unused_mut, clippy::needless_borrow)]
        pub(crate) fn #callback_name(
            scope: &mut v8::PinScope,
            args: v8::FunctionCallbackArguments,
            mut rv: v8::ReturnValue,
        ) {
            // 1. Recover the `Box<Self>` pointer from internal field 0.
            //    On illegal invocation (receiver is not a wrapper), fail
            //    *synchronously* with a TypeError — same contract as the
            //    sync method path. The user code never runs. The brand
            //    check (WebIDL §3.7) walks the prototype chain rather
            //    than just verifying internal-field 0 is an External,
            //    so cross-class calls (`Foo.prototype.method.call(bar)`)
            //    fail before the unsafe deref.
            #brand_check
            #recover_external
            // Cast to usize so the future capture doesn't carry a raw
            // pointer (Rust treats `*mut T` as !Send/!Sync; the future
            // is single-thread either way, but cleaner to launder).
            let __raw_addr: usize = __ext.value() as usize;

            // 2. Extract JS args. Uses the same shared logic as sync
            //    methods so type extraction (Vec<u8>, ByteString,
            //    Option<String>, …) is identical across sync/async.
            //    These run BEFORE we move out of `scope` for the
            //    resolver allocation, matching the sync convention.
            #(#extractions)*

            // 3. Allocate the Promise + capture Globals to bridge into
            //    the future. `wrapper_global` keeps the Box<Self>
            //    alive: as long as `wrapper_global` lives in the
            //    future capture, V8 cannot finalise the wrapper, so
            //    the Box behind `__raw_addr` stays valid across every
            //    poll of the future.
            let __resolver = v8::PromiseResolver::new(scope).unwrap();
            let __promise = __resolver.get_promise(scope);
            let __resolver_global = v8::Global::new(scope, __resolver);
            let __wrapper_global = v8::Global::new(scope, __this);

            // 4. Pull SharedState off the isolate slot. Cloned `Rc`,
            //    cheap. The future captures another clone; the
            //    callback can drop its handle freely.
            //
            //    Pre-fix: `.expect("RuntimeState not in isolate slot")`
            //    Rust-panicked here on a misconfigured runtime. Since
            //    V8 callbacks are invoked through an `extern "C"`
            //    boundary, a Rust panic abort is the default — SIGABRT
            //    on Linux (same reason gen_reentry_guard throws a
            //    V8 TypeError instead of panicking). Surface as a
            //    JS-side RangeError so the user observes a recoverable
            //    JS exception, NOT a crashed worker.
            let __state: ::zeroship_runtime::macro_runtime::state::SharedState =
                match scope.get_slot::<::zeroship_runtime::macro_runtime::state::SharedState>() {
                    Some(__s) => __s.clone(),
                    None => {
                        let __msg = #state_missing_msg_init;
                        let __exc = v8::Exception::range_error(scope, __msg);
                        scope.throw_exception(__exc);
                        return;
                    }
                };
            let __request_id = __state.borrow().executing_request_id;

            // 5. Build the future. The block keeps `wrapper_global`
            //    alive for the future's full lifetime (as the
            //    `_keepalive` binding) so the JS wrapper stays
            //    reachable even if no user JS holds a reference.
            //    Re-acquiring `&Self` per poll is safe because:
            //      a) the macro rejects `&mut self` async (see
            //         `expand`), so no aliased `&mut` can exist;
            //      b) the wrapper Global pins the Box.
            let __fut = async move {
                let _keepalive = __wrapper_global;
                // SAFETY: __raw_addr was Box::into_raw'd from
                // Box<#state_ty> at construction time; the keepalive
                // Global pins that allocation for as long as this
                // future hasn't dropped. The macro's `expand` rejects
                // `&mut self` async, so a `&Self` borrow is the only
                // shape the user method takes — no aliasing risk
                // even under V8 re-entry from microtasks.
                let __instance: &#state_ty = unsafe { &*(__raw_addr as *mut #state_ty) };
                let __result = <#state_ty>::#method_name(__instance, #(#call_args),*).await;
                let __value = ::zeroship_runtime::macro_runtime::state::IntoResolveValue::into_resolve_value(__result);
                ::zeroship_runtime::macro_runtime::state::OpResult::JsValue {
                    resolver: __resolver_global,
                    value: __value,
                    request_id: __request_id,
                }
            };

            // 6. Push to the runtime's spawned_ops queue. The pump
            //    polls these futures on every event-loop tick;
            //    settling produces the OpResult::JsValue that the
            //    pump matches into `r.resolve(scope, …)`.
            __state.borrow_mut().spawned_ops.push(::std::boxed::Box::pin(__fut));
            // Wake the pump so streaming / cross-task spawns settle
            // promptly. Mirrors `fetch_native::fetch_callback`'s
            // notify shape.
            let __notify = __state.borrow().pump_notify_tx.clone();
            if let Some(mut __tx) = __notify {
                let _ = __tx.try_send(());
            }

            // 7. Return the unsettled Promise. JS sees this as the
            //    method's return value and `await`s on it.
            rv.set(__promise.into());
        }
    }
}
