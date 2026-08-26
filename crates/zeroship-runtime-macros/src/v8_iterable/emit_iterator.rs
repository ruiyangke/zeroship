//! `next()` + `forEach` callbacks for the iterable surface.
//!
//! Split out from the old monolithic `v8_iterable.rs`. This file owns
//! the emit shape for the iterator-side callbacks: the
//! `<Class>Iterator.prototype.next()` callback and the parent class's
//! `forEach`. The companion class struct, factory callbacks, and
//! install bridge live in `emit_factory.rs`.

use proc_macro2::TokenStream as TokenStream2;
use quote::quote;

use super::EmitCtx;

/// Emit the per-mode `<Class>Iterator.prototype.next()` callback.
///
/// Snapshot mode walks the cached `__pairs` vector. Live mode
/// re-localises the parent and calls `value_pairs()` afresh on each
/// call, indexing at the current cursor.
pub(super) fn gen_next_callback(ctx: &EmitCtx<'_>) -> TokenStream2 {
    let iter_class_ty = &ctx.iter_class_ty;
    let key_ty = ctx.key_ty;
    let value_ty = ctx.value_ty;
    let next_ident = &ctx.next_ident;
    let next_brand_check = &ctx.next_brand_check;
    let next_external_recovery = &ctx.next_external_recovery;
    let key_src = &ctx.key_src;
    let val_src = &ctx.val_src;
    let key_out = &ctx.key_out;
    let val_out = &ctx.val_out;
    let key_to_v8 = &ctx.key_to_v8;
    let val_to_v8 = &ctx.val_to_v8;
    let value_key_init = &ctx.value_key_init;
    let done_key_init = &ctx.done_key_init;
    let iter_kind_keys = ctx.iter_kind_keys;
    let iter_kind_values = ctx.iter_kind_values;
    let self_ptr_ty = &ctx.self_ptr_ty;
    let self_borrow = &ctx.self_borrow;
    let self_borrow_ty = &ctx.self_borrow_ty;
    let value_pairs_args = &ctx.value_pairs_args;
    let reentry_guard = &ctx.reentry_guard;

    // Per-mode pair resolution. Snapshot reads `__it.__pairs[__it
    // .__index]` directly. Live re-localises the parent, recovers
    // `&Self`, calls `value_pairs()`, then indexes at the cursor.
    let next_pair_resolve = if ctx.live {
        quote! {
            // Read iterator fields under a short-lived borrow, then
            // drop the borrow before re-entering scope ops to read
            // the parent.
            let __idx = __it.__index;
            let __kind = __it.__kind;
            // Clone the Global (Rc-shaped, cheap) so we don't need to
            // hold the &mut __it borrow across scope.set_slot etc.
            let __parent_global = __it.__parent.clone();
            // Move out of __it (it's a &mut, non-Copy) to end the borrow.
            let _ = __it;

            // Re-localise the parent. Brand-check is implicit: the
            // factory only emits an iterator after a successful brand
            // check, and the Global pin keeps the SAME wrapper Object
            // alive — so the parent ref is still a #class_ty wrapper.
            let __parent_local: ::v8::Local<::v8::Object> =
                ::v8::Local::new(scope, &__parent_global);
            let __parent_ext = match __parent_local
                .get_internal_field(scope, 0)
                .and_then(|v| v8::Local::<v8::External>::try_from(v).ok())
            {
                Some(e) => e,
                None => {
                    // Defensive: the parent's internal field is gone.
                    // Yield `done` rather than UB on a null deref.
                    let __res = v8::Object::new(scope);
                    let __vk = #value_key_init;
                    let __dk = #done_key_init;
                    let __undef = v8::undefined(scope);
                    let __true = v8::Boolean::new(scope, true);
                    __res.set(scope, __vk.into(), __undef.into());
                    __res.set(scope, __dk.into(), __true.into());
                    rv.set(__res.into());
                    return;
                }
            };
            // SAFETY: the parent's internal field 0 was populated by
            // gen_box_and_install_finalizer with a Box<#class_ty>; the
            // Global pin keeps the wrapper alive for as long as this
            // iterator lives. value_pairs() may take &Self or &mut Self
            // per the user's signature — the macro promotes the recovery
            // accordingly.
            let __inflight_addr = __parent_ext.value() as usize;
            #reentry_guard
            let __instance_ptr: #self_ptr_ty = __parent_ext.value() as #self_ptr_ty;
            let __pairs: ::std::vec::Vec<(#key_ty, #value_ty)> = {
                let __instance: #self_borrow_ty =
                    unsafe { #self_borrow __instance_ptr };
                __instance.value_pairs #value_pairs_args
            };
            // End the parent borrow before re-entering scope.
            drop(__parent_ext);

            if __idx >= __pairs.len() {
                // Done: parent shrank below the cursor (or never had
                // enough entries).
                let __res = v8::Object::new(scope);
                let __vk = #value_key_init;
                let __dk = #done_key_init;
                let __undef = v8::undefined(scope);
                let __true = v8::Boolean::new(scope, true);
                __res.set(scope, __vk.into(), __undef.into());
                __res.set(scope, __dk.into(), __true.into());
                rv.set(__res.into());
                return;
            }

            let (#key_src, #val_src): (#key_ty, #value_ty) =
                __pairs[__idx].clone();

            // Re-acquire &mut __it briefly to advance the cursor. Safe
            // because the previous &mut went out of scope when we did
            // `let _ = __it;` above; this is a fresh, non-overlapping
            // borrow of the same allocation.
            {
                let __it_again: &mut #iter_class_ty =
                    unsafe { &mut *(__ext.value() as *mut #iter_class_ty) };
                __it_again.__index = __idx + 1;
            }
        }
    } else {
        quote! {
            if __it.__index >= __it.__pairs.len() {
                // Done: return { value: undefined, done: true }.
                let __res = v8::Object::new(scope);
                let __vk = #value_key_init;
                let __dk = #done_key_init;
                let __undef = v8::undefined(scope);
                let __true = v8::Boolean::new(scope, true);
                __res.set(scope, __vk.into(), __undef.into());
                __res.set(scope, __dk.into(), __true.into());
                rv.set(__res.into());
                return;
            }

            // Snapshot the current pair (clone — we own it, but we
            // also want to advance index without holding the borrow).
            let (#key_src, #val_src): (#key_ty, #value_ty) =
                __it.__pairs[__it.__index].clone();
            __it.__index += 1;
            // Drop the &mut borrow before any further v8 calls that
            // mutably borrow `scope`.
            let __kind = __it.__kind;
            let _ = __it; // explicit drop hint
        }
    };

    quote! {
        // -------------------------------------------------------------
        // next() — walks the iterator state
        // -------------------------------------------------------------

        /// `<Class>Iterator.prototype.next()`. Returns `{ value, done }`
        /// per the JS Iterator protocol.
        ///
        /// Snapshot mode walks the cached `__pairs` vector. Live mode
        /// re-localises the parent and calls `value_pairs()` afresh on
        /// each call, indexing at the current cursor.
        #[doc(hidden)]
        #[allow(non_snake_case, unused_variables, unused_mut)]
        pub(crate) fn #next_ident(
            scope: &mut v8::PinScope,
            args: v8::FunctionCallbackArguments,
            mut rv: v8::ReturnValue,
        ) {
            // Brand-check the receiver against `<Class>Iterator
            // .prototype`. Without this, a caller could
            // hand any `#[v8_class]` wrapper to
            // `<Class>Iterator.prototype.next.call(...)` — every
            // wrapper has `internal_field(0) = External(Box<X>)`, so
            // the bare External check below would pass and the
            // recovery `__ext.value() as *mut <Class>Iterator` would
            // reinterpret a `Box<Other>` as `*mut <Class>Iterator`,
            // which is UB.
            #next_brand_check
            #next_external_recovery
            let __it: &mut #iter_class_ty =
                unsafe { &mut *(__ext.value() as *mut #iter_class_ty) };

            // Per-mode pair resolution: snapshot indexes `__it.__pairs`
            // directly; live re-reads value_pairs() via the stashed
            // parent Global. After this block, the names in scope are:
            //   #key_src : #key_ty
            //   #val_src : #value_ty
            //   __kind   : i32
            // and the `done` early-return path has already been taken
            // if applicable.
            #next_pair_resolve

            #key_to_v8
            #val_to_v8

            let __value: v8::Local<v8::Value> = match __kind {
                #iter_kind_keys => #key_out,
                #iter_kind_values => #val_out,
                _ => {
                    // Entries: yield a 2-element [k, v] array.
                    let __arr = v8::Array::new(scope, 2);
                    __arr.set_index(scope, 0, #key_out);
                    __arr.set_index(scope, 1, #val_out);
                    __arr.into()
                }
            };

            let __res = v8::Object::new(scope);
            let __vk = #value_key_init;
            let __dk = #done_key_init;
            let __false = v8::Boolean::new(scope, false);
            __res.set(scope, __vk.into(), __value);
            __res.set(scope, __dk.into(), __false.into());
            rv.set(__res.into());
        }
    }
}

/// Emit the per-mode `forEach(callback, thisArg?)` callback.
///
/// In snapshot mode, `value_pairs()` is read once before the loop;
/// mutations made by the callback do NOT show up in the remaining
/// iterations. In live mode, `value_pairs()` is re-read between
/// callbacks per WebIDL §3.7.10.3.
pub(super) fn gen_for_each_callback(ctx: &EmitCtx<'_>) -> TokenStream2 {
    let key_ty = ctx.key_ty;
    let value_ty = ctx.value_ty;
    let for_each_ident = &ctx.for_each_ident;
    let for_each_brand_check = &ctx.for_each_brand_check;
    let for_each_external_recovery = &ctx.for_each_external_recovery;
    let key_src = &ctx.key_src;
    let val_src = &ctx.val_src;
    let key_out = &ctx.key_out;
    let val_out = &ctx.val_out;
    let key_to_v8 = &ctx.key_to_v8;
    let val_to_v8 = &ctx.val_to_v8;
    let foreach_not_callable_init = &ctx.foreach_not_callable_init;
    let self_ptr_ty = &ctx.self_ptr_ty;
    let self_borrow = &ctx.self_borrow;
    let self_borrow_ty = &ctx.self_borrow_ty;
    let value_pairs_args = &ctx.value_pairs_args;
    let reentry_guard = &ctx.reentry_guard;

    // Per-mode forEach loop. Snapshot reads `value_pairs()` once
    // before the loop. Live re-reads BETWEEN callbacks, so a callback
    // that mutates the parent sees its own mutations on the next
    // iteration.
    let for_each_loop = if ctx.live {
        quote! {
            // Live forEach: track a cursor, re-read value_pairs() each
            // iteration. We don't hold a `&Self`/`&mut Self` borrow
            // across callbacks (which would freeze the parent's
            // RefCell, etc.) — instead we drop it after each
            // pair-fetch.
            let mut __cursor: usize = 0;
            // Re-entry guard scope: `value_pairs` is reentry-guarded
            // for `&mut self` shapes; the address used for the guard
            // is the parent's External value.
            let __inflight_addr = __ext.value() as usize;
            #reentry_guard
            loop {
                let __pairs: ::std::vec::Vec<(#key_ty, #value_ty)> = {
                    // Fresh `&Self`/`&mut Self` per iteration. The brand
                    // check at the top of the callback already
                    // established the receiver is a #class_ty; we reload
                    // via the SAME __ext (it still points at the same
                    // allocation).
                    let __instance_ptr: #self_ptr_ty =
                        __ext.value() as #self_ptr_ty;
                    let __instance: #self_borrow_ty =
                        unsafe { #self_borrow __instance_ptr };
                    __instance.value_pairs #value_pairs_args
                };
                if __cursor >= __pairs.len() {
                    break;
                }
                let (#key_src, #val_src): (#key_ty, #value_ty) =
                    __pairs[__cursor].clone();
                __cursor += 1;
                // value_pairs result drops here — no borrow into
                // __ext when we re-enter the callback (which may
                // synchronously mutate the parent's RefCell).
                drop(__pairs);

                #key_to_v8
                #val_to_v8
                let __cb_args = [#val_out, #key_out, __this.into()];
                if __cb_fn.call(scope, __this_arg, &__cb_args).is_none() {
                    return; // exception thrown — propagate
                }
            }
        }
    } else {
        quote! {
            let __pairs: ::std::vec::Vec<(#key_ty, #value_ty)> = {
                let __instance: #self_borrow_ty =
                    unsafe { #self_borrow __instance_ptr };
                __instance.value_pairs #value_pairs_args
            };
            drop(__ext);

            for (#key_src, #val_src) in __pairs.into_iter() {
                #key_to_v8
                #val_to_v8
                let __cb_args = [#val_out, #key_out, __this.into()];
                if __cb_fn.call(scope, __this_arg, &__cb_args).is_none() {
                    return; // exception thrown — propagate
                }
            }
        }
    };

    // Snapshot pre-binds the instance pointer + reentry guard; live
    // re-binds inside the loop. The guard's RAII releases at end of
    // callback (snapshot) or end of forEach loop scope (live).
    let for_each_instance_pre = if ctx.live {
        quote! {}
    } else {
        quote! {
            let __inflight_addr = __ext.value() as usize;
            #reentry_guard
            let __instance_ptr: #self_ptr_ty =
                __ext.value() as #self_ptr_ty;
        }
    };

    quote! {
        // -------------------------------------------------------------
        // forEach callback — WebIDL §3.7.10.3
        // -------------------------------------------------------------

        /// `forEach(callback, thisArg?)`. Invokes `callback(value, key,
        /// this)` for each pair, where `this` is the parent collection
        /// instance.
        ///
        /// In snapshot mode, `value_pairs()` is read once before the
        /// loop; mutations made by the callback do NOT show up in the
        /// remaining iterations. In live mode, `value_pairs()` is re-
        /// read between callbacks per WebIDL §3.7.10.3.
        #[doc(hidden)]
        #[allow(non_snake_case, unused_variables, unused_mut)]
        pub(crate) fn #for_each_ident(
            scope: &mut v8::PinScope,
            args: v8::FunctionCallbackArguments,
            _rv: v8::ReturnValue,
        ) {
            #for_each_brand_check
            #for_each_external_recovery

            // Snapshot: bind __instance up-front (one read of
            // value_pairs() before the loop). Live: do not bind here;
            // the loop re-binds per iteration.
            #for_each_instance_pre

            // Validate the callback arg.
            let __cb_arg = args.get(0);
            let __cb_fn: v8::Local<v8::Function> = match __cb_arg.try_into() {
                Ok(f) => f,
                Err(_) => {
                    let __msg = #foreach_not_callable_init;
                    let __exc = v8::Exception::type_error(scope, __msg);
                    scope.throw_exception(__exc);
                    return;
                }
            };
            let __this_arg = args.get(1);

            #for_each_loop
        }
    }
}
