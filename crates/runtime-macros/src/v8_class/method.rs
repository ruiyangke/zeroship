//! Per-method callback codegen.
//!
//! Every `#[v8_class]` impl emits one V8 FunctionCallback per method,
//! getter, setter, async method, static method/getter, and constructor.
//! This module hosts those codegen routines plus their shared helpers
//! (re-entrancy guard, must-new prologue, Box install + finalizer).
//!
//! Hosts:
//! - `gen_reentry_guard` — `&mut self` re-entry detection.
//! - `gen_method_callback` — slow-path FunctionCallback for plain methods.
//! - `gen_setter_callback` — accessor setter (one positional arg, no
//!   return marshaling).
//! - `gen_same_object_getter_callback` — WebIDL `[SameObject]` cache
//!   on a private symbol.
//! - `gen_async_method_callback` — sync callback that allocates a
//!   Promise + spawns the user's async body.
//! - `gen_static_callback` — WebIDL §3.7.4 static op/attr (no receiver).
//! - `gen_must_new_prologue` — WebIDL §3.7.1 `new`-required guard.
//! - `gen_constructor_callback` / `gen_default_constructor_callback`
//!   — user-defined and Default-derived constructor codegen.
//! - `gen_box_and_install_finalizer` — Box-install + GC finalizer
//!   plus optional fastcall slot-1 aligned pointer.

use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};

use super::helpers::{
    gen_param_extractions, method_callback_ident, outer_ident, parse_params_skipping_self,
};
use super::parse::{extract_callable_no_new, extract_post_init, extract_reject_shared};
use super::shared::class_config::ClassConfig;
use super::{ClassMethod, MethodKind};
use crate::gen_call_return;

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
pub(super) fn gen_reentry_guard(
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

pub(super) fn gen_method_callback(cfg: &ClassConfig, m: &ClassMethod) -> TokenStream2 {
    let class_ty = cfg.class_ty;
    let state_ty = cfg.state_ty;
    let method_name = &m.func.sig.ident;
    let callback_name = method_callback_ident(class_ty, method_name);

    // Skip the receiver param when extracting JS args.
    let params = parse_params_skipping_self(m.func);
    let reject_shared_names = extract_reject_shared(&m.func.attrs);
    let extractions = gen_param_extractions(&params, &reject_shared_names);

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

    let getter_args = if m.kind == MethodKind::Getter {
        // V8 getters use AccessorCallback signature; we use FunctionTemplate
        // for parity with methods, so the args object is still passed.
        quote! {}
    } else {
        quote! {}
    };

    let _ = getter_args;

    // WebIDL §3.7 brand check + Box<Self> recovery + (optional)
    // re-entry guard + unsafe `&mut Self` materialisation. See
    // `shared::recover_box::gen_recover_box` for the soundness
    // rationale and the byte-identity contract with the hand-rolled
    // prologue this replaces.
    let recover = super::shared::recover_box::gen_recover_box(
        class_ty,
        state_ty,
        method_name,
        m.mut_receiver,
    );

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
pub(super) fn gen_same_object_getter_callback(
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
    let reject_shared_names = extract_reject_shared(&m.func.attrs);
    let extractions = gen_param_extractions(&params, &reject_shared_names);
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
    let brand_check = super::shared::recover_box::gen_brand_check_throw(&brand_check_fn);
    let recover_external = super::shared::recover_box::gen_recover_external();
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
pub(super) fn gen_async_method_callback(cfg: &ClassConfig, m: &ClassMethod) -> TokenStream2 {
    let class_ty = cfg.class_ty;
    let state_ty = cfg.state_ty;
    let method_name = &m.func.sig.ident;
    let callback_name = method_callback_ident(class_ty, method_name);

    // Skip the receiver param when extracting JS args.
    let params = parse_params_skipping_self(m.func);
    let reject_shared_names = extract_reject_shared(&m.func.attrs);
    let extractions = gen_param_extractions(&params, &reject_shared_names);

    let call_args: Vec<&syn::Ident> = params.iter().map(|p| &p.name).collect();
    let brand_check_fn = format_ident!("__brand_check_{}", class_ty);
    // Async paths can't take `&mut self` (rejected at `expand`) so no
    // re-entry guard is emitted. The brand-check + External-recovery
    // halves are byte-identical to the sync method's prologue; the
    // recovered `__ext.value()` is laundered through `usize` for the
    // async future capture.
    let brand_check = super::shared::recover_box::gen_brand_check_throw(&brand_check_fn);
    let recover_external = super::shared::recover_box::gen_recover_external();

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
            let __state: ::zeroship_runtime::state::SharedState = scope
                .get_slot::<::zeroship_runtime::state::SharedState>()
                .expect("RuntimeState not in isolate slot")
                .clone();
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
                let __value = ::zeroship_runtime::state::IntoResolveValue::into_resolve_value(__result);
                ::zeroship_runtime::state::OpResult::JsValue {
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
pub(super) fn gen_static_callback(cfg: &ClassConfig, m: &ClassMethod) -> TokenStream2 {
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

fn gen_setter_callback(cfg: &ClassConfig, m: &ClassMethod) -> TokenStream2 {
    let class_ty = cfg.class_ty;
    let state_ty = cfg.state_ty;
    let method_name = &m.func.sig.ident;
    let callback_name = method_callback_ident(class_ty, method_name);

    // Setters take exactly one logical param: the new value.
    let params = parse_params_skipping_self(m.func);
    let reject_shared_names = extract_reject_shared(&m.func.attrs);
    let extractions = gen_param_extractions(&params, &reject_shared_names);
    let call_args: Vec<&syn::Ident> = params.iter().map(|p| &p.name).collect();
    let receiver_ref = if m.mut_receiver {
        quote! { &mut *__instance }
    } else {
        quote! { &*__instance }
    };
    // WebIDL §3.7 brand check + Box<Self> recovery — same contract as
    // `gen_method_callback`. The setter discards the return value at
    // the end; the prologue itself is byte-identical.
    let recover = super::shared::recover_box::gen_recover_box(
        class_ty,
        state_ty,
        method_name,
        m.mut_receiver,
    );

    quote! {
        #[allow(non_snake_case, unused_variables, unused_mut, clippy::needless_borrow)]
        pub(crate) fn #callback_name(
            scope: &mut v8::PinScope,
            args: v8::FunctionCallbackArguments,
            mut rv: v8::ReturnValue,
        ) {
            #recover

            #(#extractions)*

            // Discard return — setters don't propagate values.
            let _ = <#state_ty>::#method_name(#receiver_ref, #(#call_args),*);
        }
    }
}

// ---------------------------------------------------------------------------
// Constructor callback codegen
// ---------------------------------------------------------------------------

/// WebIDL §3.7.1: every interface constructor MUST be called with `new`.
/// Returns the `if !args.is_construct_call() { throw TypeError; return; }`
/// prologue unless the class opts out via `#[v8_constructor(callable_no_new)]`.
///
/// Class-name interpolation in the message (e.g. `"Constructor Headers
/// requires 'new'"`) lets WPT diagnose mistakes per-class. The
/// `is_construct_call` flag is V8-native — it differentiates `new Foo()`
/// (true) from `Foo()` and `Foo.call(...)` (false) without a runtime
/// thunk in the user code.
fn gen_must_new_prologue(class_ty: &syn::Ident, opt_out: bool) -> TokenStream2 {
    if opt_out {
        return quote! {};
    }
    let class_name_str = class_ty.to_string();
    let msg = format!(
        "Failed to construct '{class_name_str}': Please use the 'new' operator, this DOM object constructor cannot be called as a function."
    );
    quote! {
        if !args.is_construct_call() {
            let __msg = v8::String::new(scope, #msg).unwrap();
            let __exc = v8::Exception::type_error(scope, __msg);
            scope.throw_exception(__exc);
            return;
        }
    }
}

pub(super) fn gen_constructor_callback(cfg: &ClassConfig, c: &ClassMethod) -> TokenStream2 {
    let class_ty = cfg.class_ty;
    let state_ty = cfg.state_ty;
    let has_any_fastcall = cfg.has_any_fastcall;
    let ctor_name = &c.func.sig.ident;
    let callback_ident = format_ident!("__{}_constructor_callback", class_ty);

    // Constructors have no `self` receiver; the skipping-self helper
    // works uniformly here since it just collects typed args.
    let params = parse_params_skipping_self(c.func);
    let reject_shared_names = extract_reject_shared(&c.func.attrs);
    let extractions = gen_param_extractions(&params, &reject_shared_names);
    let call_args: Vec<&syn::Ident> = params.iter().map(|p| &p.name).collect();

    let is_result = matches!(
        outer_ident(&c.func.sig.output).as_deref(),
        Some("Result")
    );

    let make_instance = if is_result {
        quote! {
            let __instance: #state_ty = match <#state_ty>::#ctor_name(#(#call_args),*) {
                Ok(__v) => __v,
                Err(__err) => {
                    // JsValue passthrough — preserves user-thrown
                    // exception verbatim (Error subclass, .code, etc.).
                    if let ::zeroship_runtime::state::OpErrorKind::JsValue(__global) = &__err.kind {
                        let __local = v8::Local::new(scope, __global);
                        scope.throw_exception(__local);
                        return;
                    }
                    let __msg = v8::String::new(scope, &__err.message).unwrap();
                    let __exc: v8::Local<v8::Value> = match &__err.kind {
                        ::zeroship_runtime::state::OpErrorKind::TypeError => v8::Exception::type_error(scope, __msg),
                        ::zeroship_runtime::state::OpErrorKind::RangeError => v8::Exception::range_error(scope, __msg),
                        ::zeroship_runtime::state::OpErrorKind::DomException(__name) => {
                            ::zeroship_runtime::dom::exception::build(scope, &__err.message, __name).into()
                        }
                        ::zeroship_runtime::state::OpErrorKind::NodeError(__code) => {
                            ::zeroship_runtime::node_error::build_node_exception(scope, __code, &__err.message)
                        }
                        ::zeroship_runtime::state::OpErrorKind::Error => v8::Exception::error(scope, __msg),
                        ::zeroship_runtime::state::OpErrorKind::JsValue(_) => unreachable!(),
                    };
                    scope.throw_exception(__exc);
                    return;
                }
            };
        }
    } else {
        quote! {
            let __instance: #state_ty = <#state_ty>::#ctor_name(#(#call_args),*);
        }
    };

    let store = gen_box_and_install_finalizer(state_ty, has_any_fastcall);
    let must_new = gen_must_new_prologue(class_ty, extract_callable_no_new(&c.func.attrs));

    // MAC-02: post_init dispatch — runs AFTER box install, BEFORE the
    // callback returns to V8. Hook signature is
    // `fn(&mut PinScope, Local<Object>) -> Result<(), OpError>`.
    //
    // Behaviour matrix (design §5.2 / §5.3):
    //   - must_new + post_init: must-new throws early, post_init never
    //     runs. No special case in this code — must-new returns first.
    //   - callable_no_new + post_init: hook only fires for `new Foo()`
    //     (is_construct_call() == true). Bare `Foo()` skips the hook
    //     to avoid writing private symbols on globalThis (the design
    //     reverses the v1 "always run" decision; see §5.3).
    //   - #[v8_inherit]: derived's hook runs; base's does NOT auto-chain
    //     (V8's existing constructor semantics — derived is responsible
    //     for invoking base setup explicitly; see §5.4 worked example).
    //
    // Box reclamation under failure (§5.9 / §4.4): when the hook returns
    // Err, the macro throws a JS exception and returns. The box stays
    // installed in field 0 until the V8 weak finalizer reclaims the
    // wrapper on the next GC sweep. v1 ships with lazy drop; eager drop
    // is deferred per the design's cost-benefit analysis. The
    // user-visible contract: `Self::Drop` side-effects from a failed
    // post_init may be delayed by up to one GC cycle.
    let post_init = match extract_post_init(&c.func.attrs) {
        Ok(None) => quote! {},
        Err(e) => return e.to_compile_error(),
        Ok(Some(hook_ident)) => {
            // Mirror the make_instance Result arm verbatim — same five
            // OpErrorKind variants from crates/runtime/src/core/state.rs
            // (TypeError, RangeError, Error, DomException, NodeError, JsValue).
            // Any addition there must be mirrored here.
            quote! {
                if args.is_construct_call() {
                    match <#class_ty>::#hook_ident(scope, __this) {
                        Ok(()) => {},
                        Err(__err) => {
                            if let ::zeroship_runtime::state::OpErrorKind::JsValue(__global) = &__err.kind {
                                let __exc = v8::Local::new(scope, __global);
                                scope.throw_exception(__exc);
                                return;
                            }
                            let __msg = v8::String::new(scope, &__err.message).unwrap();
                            let __exc: v8::Local<v8::Value> = match __err.kind {
                                ::zeroship_runtime::state::OpErrorKind::TypeError => v8::Exception::type_error(scope, __msg),
                                ::zeroship_runtime::state::OpErrorKind::RangeError => v8::Exception::range_error(scope, __msg),
                                ::zeroship_runtime::state::OpErrorKind::DomException(__name) => {
                                    ::zeroship_runtime::dom::exception::build(scope, &__err.message, __name).into()
                                }
                                ::zeroship_runtime::state::OpErrorKind::NodeError(__code) => {
                                    ::zeroship_runtime::node_error::build_node_exception(scope, __code, &__err.message)
                                }
                                ::zeroship_runtime::state::OpErrorKind::Error => v8::Exception::error(scope, __msg),
                                ::zeroship_runtime::state::OpErrorKind::JsValue(_) => unreachable!(),
                            };
                            scope.throw_exception(__exc);
                            return;
                        }
                    }
                }
            }
        }
    };

    quote! {
        #[allow(non_snake_case, unused_variables, unused_mut, clippy::needless_borrow)]
        pub(crate) fn #callback_ident(
            scope: &mut v8::PinScope,
            args: v8::FunctionCallbackArguments,
            _rv: v8::ReturnValue,
        ) {
            #must_new
            let __this = args.this();

            #(#extractions)*
            #make_instance

            #store
            #post_init
        }
    }
}

pub(super) fn gen_default_constructor_callback(cfg: &ClassConfig) -> TokenStream2 {
    let class_ty = cfg.class_ty;
    let state_ty = cfg.state_ty;
    let has_any_fastcall = cfg.has_any_fastcall;
    let callback_ident = format_ident!("__{}_constructor_callback", class_ty);
    let store = gen_box_and_install_finalizer(state_ty, has_any_fastcall);
    // No method-level attrs to read — the Default-derived constructor
    // is always must-new. The opt-out attribute requires a user-written
    // `#[v8_constructor]`, by definition.
    let must_new = gen_must_new_prologue(class_ty, false);

    quote! {
        #[allow(non_snake_case, unused_variables, unused_mut, clippy::needless_borrow)]
        pub(crate) fn #callback_ident(
            scope: &mut v8::PinScope,
            args: v8::FunctionCallbackArguments,
            _rv: v8::ReturnValue,
        ) {
            #must_new
            let __this = args.this();
            let __instance: #state_ty = <#state_ty as ::core::default::Default>::default();

            #store
        }
    }
}

/// Box the instance, store the raw pointer in internal field 0, and
/// register a guaranteed finalizer on the JS wrapper to reclaim the
/// Box when V8 GCs the object.
///
/// When `has_any_fastcall` is true, the same raw pointer is also stored
/// in slot 1 via `set_aligned_pointer_in_internal_field` so fastcall
/// shims can recover `*const Self` without a scope (a single load,
/// `get_aligned_pointer_from_internal_field(1, 0)`). Slot 0 keeps the
/// External + finalizer for the standard wrapper teardown; slot 1 is
/// scope-free and read-only from the fast path.
///
/// The pointer is captured as `usize` in the closure so we don't have
/// to assert `Send` on a `*mut Self`; we cast back inside the closure
/// where the type is statically known. The Weak handle is forgotten
/// (via `mem::forget`) because dropping it would deregister the
/// finalizer — `with_guaranteed_finalizer` ensures the closure runs
/// on GC or isolate teardown regardless.
fn gen_box_and_install_finalizer(state_ty: &syn::Ident, has_any_fastcall: bool) -> TokenStream2 {
    let fastcall_slot1 = if has_any_fastcall {
        quote! {
            // tag = 0: must match the tag passed to
            // get_aligned_pointer_from_internal_field in the fastcall
            // shim. V8 uses the tag to distinguish embedder pointer
            // categories — a mismatch returns null.
            __this.set_aligned_pointer_in_internal_field(
                1,
                __raw_ptr as *const ::std::ffi::c_void,
                0,
            );
        }
    } else {
        quote! {}
    };
    quote! {
        let __boxed = Box::new(__instance);
        let __raw_ptr = Box::into_raw(__boxed);
        let __raw_addr = __raw_ptr as usize;

        let __ext = v8::External::new(scope, __raw_ptr as *mut ::std::ffi::c_void);
        __this.set_internal_field(0, __ext.into());

        // Optional fastcall slot — set only when at least one method
        // on the class is annotated with `#[v8_method(fastcall)]` /
        // `#[v8_getter(fastcall)]`. Slot 1 holds the same Box raw
        // pointer as slot 0's External, but stored as an aligned
        // pointer so the fast-path shim can recover `*const Self`
        // without a scope.
        #fastcall_slot1

        // SAFETY: __raw_addr was Box::into_raw'd from Box<#state_ty>;
        // the finalizer closure casts back to the same type and drops
        // the Box exactly once when V8 reclaims the JS wrapper.
        let __weak = v8::Weak::with_guaranteed_finalizer(
            scope,
            __this,
            Box::new(move || {
                unsafe {
                    drop(Box::from_raw(__raw_addr as *mut #state_ty));
                }
            }),
        );
        // Dropping the Weak removes the finalizer. The "guaranteed"
        // variant fires on GC or isolate teardown anyway, so we leak
        // the per-instance WeakData (~32 bytes) to keep the registration.
        ::std::mem::forget(__weak);
    }
}

