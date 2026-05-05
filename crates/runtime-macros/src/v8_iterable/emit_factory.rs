//! Iterator companion class + factory callbacks + install bridge.
//!
//! Wave 9 split — extracted from `v8_iterable.rs`'s 1,368-LOC god
//! file (per design + v2 architecture-critic R2). This file owns the
//! emit shape for everything wired to the FACTORY side of the
//! iterable surface: the `<Class>Iterator` companion struct, its
//! install fn, the `__zs_iter_construct_throws` constructor stub, the
//! per-kind factory callbacks (keys / values / entries), and the
//! `__zs_install_iterable_methods` bridge that the parent's
//! `<Class>::install` calls.
//!
//! The next() + forEach callbacks live in `emit_iterator.rs`.

use proc_macro2::TokenStream as TokenStream2;
use quote::quote;

use super::EmitCtx;

/// Emit the companion `<Class>Iterator` struct + its
/// `__InstallSlot_<Class>Iterator` marker + the iterator class's
/// `install` method (FunctionTemplate cache, prototype walk to
/// %IteratorPrototype%) + the `__BrandSlot_<Class>Iterator` marker +
/// `__brand_check_<Class>Iterator` helper used by `next()`.
pub(super) fn gen_iterator_companion(ctx: &EmitCtx<'_>) -> TokenStream2 {
    let iter_class_ty = &ctx.iter_class_ty;
    let iter_install_slot_ty = &ctx.iter_install_slot_ty;
    let iter_brand_slot_ty = &ctx.iter_brand_slot_ty;
    let iter_brand_check_fn = &ctx.iter_brand_check_fn;
    let key_ty = ctx.key_ty;
    let value_ty = ctx.value_ty;
    let next_ident = &ctx.next_ident;
    let class_name_init = &ctx.class_name_init;
    let to_string_tag_init = &ctx.to_string_tag_init;
    let next_key_init = &ctx.next_key_init;
    let proto_key_init = &ctx.proto_key_init;
    let iter_proto_walk_js_init = &ctx.iter_proto_walk_js_init;

    // Per-mode iterator struct definition. Snapshot stashes
    // `__pairs: Vec<(K, V)>`; live stashes a `Global<Object>`
    // reference to the parent. Both share `__index` and `__kind`.
    let iter_struct_def_tokens = if ctx.live {
        quote! {
            /// Default-pair iterator companion for the parent class
            /// (live-mode). Holds a `Global<Object>` reference to the
            /// parent and re-reads `value_pairs()` on each `next()`
            /// call per WebIDL §3.7.10.2.
            #[doc(hidden)]
            #[allow(non_camel_case_types)]
            pub struct #iter_class_ty {
                /// Live-mode parent reference. The factory captures
                /// this as a Global so the parent stays reachable for
                /// the iterator's lifetime; `next()` re-localises and
                /// recovers `&parent` per call to call `value_pairs()`
                /// afresh.
                __parent: ::v8::Global<::v8::Object>,
                /// Current cursor into the live `value_pairs()` result.
                /// Increments on each successful `next()`. Does NOT
                /// reset when the parent shrinks below it; the
                /// iterator simply reports `done`.
                __index: usize,
                /// Iterator kind — selects which of the (K, V) tuple to
                /// surface as the `value` field of `IteratorResult`.
                ///   0 = keys, 1 = values, 2 = entries
                __kind: i32,
            }
        }
    } else {
        quote! {
            /// Default-pair iterator companion for the parent class
            /// (snapshot-mode). Holds a snapshot of `value_pairs()`
            /// taken at factory-call time; `next()` walks the snapshot.
            /// Snapshot is a deliberate simplification of WebIDL
            /// §3.7.10.2 live-iteration — see `v8_iterable.rs` doc-
            /// comment for the rationale.
            #[doc(hidden)]
            #[allow(non_camel_case_types)]
            pub struct #iter_class_ty {
                /// Snapshot of (K, V) pairs taken at factory-call time.
                /// Owned by this iterator; dropped with the JS wrapper's
                /// finalizer.
                __pairs: ::std::vec::Vec<(#key_ty, #value_ty)>,
                /// Current index into `__pairs`. Increments on each
                /// successful `next()` call.
                __index: usize,
                /// Iterator kind — selects which of the (K, V) tuple to
                /// surface as the `value` field of `IteratorResult`.
                ///   0 = keys, 1 = values, 2 = entries
                __kind: i32,
            }
        }
    };

    quote! {
        // -------------------------------------------------------------
        // <Class>Iterator — companion class
        // -------------------------------------------------------------

        #iter_struct_def_tokens

        // The iterator class's own install slot — see the parent
        // class's install slot for the rationale (idempotent install,
        // FunctionTemplate caching across calls in the same isolate).
        #[doc(hidden)]
        #[allow(non_camel_case_types)]
        pub struct #iter_install_slot_ty(::v8::Global<::v8::FunctionTemplate>);

        // Per-isolate slot for the cached `<Class>Iterator.prototype`
        // (used by `__brand_check_<Class>Iterator`). Lazily populated
        // on first brand-check call. Closes Wave 10 NS6.
        #[doc(hidden)]
        #[allow(non_camel_case_types)]
        pub struct #iter_brand_slot_ty(::v8::Global<::v8::Object>);

        /// Brand-check helper for the iterator class: walks the
        /// prototype chain of `obj` looking for the cached
        /// `<Class>Iterator.prototype`. Returns true on match (the
        /// receiver IS a `<Class>Iterator`), false otherwise.
        ///
        /// Closes Wave 10 NS6: the previous `next()` codegen relied
        /// solely on internal-field-0 being an `External`, which any
        /// `#[v8_class]` wrapper satisfies. A caller could pass a
        /// different wrapper as `this` and the recovery
        /// `__ext.value() as *mut <Class>Iterator` would reinterpret a
        /// `Box<Other>` as `*mut <Class>Iterator` — UB. The brand
        /// check pins the receiver to instances of THIS iterator class
        /// before the unsafe cast.
        ///
        /// Walks at most 1024 prototype links — matches V8's internal
        /// `Object::PrototypeChainLength` sanity bound. Cycle creation
        /// is already blocked by ECMAScript §10.4.7.2 step 8; the cap
        /// is belt-and-braces for proxy-driven prototype chains.
        ///
        /// The cached prototype is populated lazily on first call (NOT
        /// at install time) — eager `get_function(scope)` would freeze
        /// the FunctionTemplate's instance shape. Mirrors the parent-
        /// class `__brand_check_<Class>` helper in
        /// `v8_class/emit/brand.rs`.
        #[doc(hidden)]
        #[allow(non_snake_case, dead_code)]
        fn #iter_brand_check_fn(
            scope: &mut v8::PinScope,
            obj: v8::Local<v8::Object>,
        ) -> bool {
            let cached_global: v8::Global<v8::Object> =
                if let Some(slot) = scope.get_slot::<#iter_brand_slot_ty>() {
                    slot.0.clone()
                } else {
                    // Lazy fetch from the install slot. If that slot is
                    // missing too, the iterator class wasn't installed
                    // in this isolate — fall through to false.
                    let tmpl_global = match scope.get_slot::<#iter_install_slot_ty>() {
                        Some(s) => s.0.clone(),
                        None => return false,
                    };
                    let tmpl_local = v8::Local::new(scope, &tmpl_global);
                    let func = match tmpl_local.get_function(scope) {
                        Some(f) => f,
                        None => return false,
                    };
                    let proto_key = match v8::String::new(scope, "prototype") {
                        Some(s) => s,
                        None => return false,
                    };
                    let proto_v = match func.get(scope, proto_key.into()) {
                        Some(v) => v,
                        None => return false,
                    };
                    let proto: v8::Local<v8::Object> = match proto_v.try_into() {
                        Ok(o) => o,
                        Err(_) => return false,
                    };
                    let g = v8::Global::new(scope, proto);
                    let g_clone = g.clone();
                    scope.set_slot(#iter_brand_slot_ty(g));
                    g_clone
                };
            let expected_proto: v8::Local<v8::Object> = v8::Local::new(scope, &cached_global);
            let mut current: v8::Local<v8::Value> = match obj.get_prototype(scope) {
                Some(v) => v,
                None => return false,
            };
            for _ in 0..1024 {
                if current.is_null_or_undefined() {
                    return false;
                }
                let cur_obj: v8::Local<v8::Object> = match current.try_into() {
                    Ok(o) => o,
                    Err(_) => return false,
                };
                if cur_obj == expected_proto {
                    return true;
                }
                current = match cur_obj.get_prototype(scope) {
                    Some(v) => v,
                    None => return false,
                };
            }
            false
        }

        #[allow(non_snake_case, dead_code)]
        impl #iter_class_ty {
            /// Install the iterator class's FunctionTemplate. The
            /// emitted shape mirrors `#[v8_class]` (one internal field
            /// for the boxed instance, Symbol.toStringTag = "<Class>
            /// Iterator", prototype chained to %IteratorPrototype% per
            /// WebIDL §3.7.10.2). Idempotent per isolate.
            pub fn install<'s>(
                scope: &mut v8::PinScope<'s, '_>,
            ) -> v8::Local<'s, v8::FunctionTemplate> {
                if let Some(cached) = scope.get_slot::<#iter_install_slot_ty>() {
                    return v8::Local::new(scope, cached.0.clone());
                }
                let __ctor_tmpl = v8::FunctionTemplate::new(scope, __zs_iter_construct_throws);
                let __class_name = #class_name_init;
                __ctor_tmpl.set_class_name(__class_name);
                __ctor_tmpl
                    .instance_template(scope)
                    .set_internal_field_count(1);

                let __proto = __ctor_tmpl.prototype_template(scope);

                // next()
                {
                    let __key = #next_key_init;
                    let __fn_tmpl = v8::FunctionTemplate::new(scope, #next_ident);
                    __proto.set(__key.into(), __fn_tmpl.into());
                }

                // Symbol.toStringTag — "<Class> Iterator" per spec.
                {
                    let __tag_sym = v8::Symbol::get_to_string_tag(scope);
                    let __tag_value = #to_string_tag_init;
                    __proto.set_with_attr(
                        __tag_sym.into(),
                        __tag_value.into(),
                        v8::PropertyAttribute::READ_ONLY | v8::PropertyAttribute::DONT_ENUM,
                    );
                }

                // Chain prototype to %IteratorPrototype% per
                // WebIDL §3.7.10.2 default iterator [[Prototype]].
                {
                    let __ctor_fn = __ctor_tmpl.get_function(scope).unwrap();
                    let __proto_key = #proto_key_init;
                    let __ctor_proto_v = __ctor_fn.get(scope, __proto_key.into()).unwrap();
                    let __ctor_proto: v8::Local<v8::Object> = __ctor_proto_v.try_into().unwrap();
                    let __js = #iter_proto_walk_js_init;
                    let __script = v8::Script::compile(scope, __js, None).unwrap();
                    let __iter_proto = __script.run(scope).unwrap();
                    __ctor_proto.set_prototype(scope, __iter_proto);
                }

                let __global = ::v8::Global::new(scope, __ctor_tmpl);
                let __local = ::v8::Local::new(scope, __global.clone());
                scope.set_slot(#iter_install_slot_ty(__global));
                __local
            }
        }
    }
}

/// Emit the `__zs_iter_construct_throws` stub. WebIDL says iterator
/// objects are NOT user-callable as constructors — they're minted
/// only by the parent's keys() / values() / entries() factories. The
/// exposed constructor throws TypeError synchronously.
pub(super) fn gen_constructor_throws(ctx: &EmitCtx<'_>) -> TokenStream2 {
    let illegal_ctor_msg_init = &ctx.illegal_ctor_msg_init;
    quote! {
        /// `new <Class>Iterator()` is not user-callable per WebIDL —
        /// iterator objects are constructed by the parent's keys() /
        /// values() / entries() factories. The exposed constructor
        /// throws TypeError synchronously.
        #[doc(hidden)]
        #[allow(non_snake_case, unused_variables)]
        pub(crate) fn __zs_iter_construct_throws(
            scope: &mut v8::PinScope,
            _args: v8::FunctionCallbackArguments,
            _rv: v8::ReturnValue,
        ) {
            let __msg = #illegal_ctor_msg_init;
            let __exc = v8::Exception::type_error(scope, __msg);
            scope.throw_exception(__exc);
        }
    }
}

/// Emit the per-kind factory callbacks (keys / values / entries) +
/// the shared `__zs_iter_factory_impl` body. Brand-checks the
/// receiver against the parent class, allocates an iterator instance
/// with the requested `kind`, and binds the iterator state per the
/// snapshot/live mode.
pub(super) fn gen_factory_callbacks(ctx: &EmitCtx<'_>) -> TokenStream2 {
    let iter_class_ty = &ctx.iter_class_ty;
    let key_ty = ctx.key_ty;
    let value_ty = ctx.value_ty;
    let factory_keys_ident = &ctx.factory_keys_ident;
    let factory_values_ident = &ctx.factory_values_ident;
    let factory_entries_ident = &ctx.factory_entries_ident;
    let factory_brand_check = &ctx.factory_brand_check;
    let factory_external_recovery = &ctx.factory_external_recovery;
    let alloc_fail_msg_init = &ctx.alloc_fail_msg_init;
    let proto_key_init = &ctx.proto_key_init;
    let iter_kind_keys = ctx.iter_kind_keys;
    let iter_kind_values = ctx.iter_kind_values;
    let iter_kind_entries = ctx.iter_kind_entries;
    let self_ptr_ty = &ctx.self_ptr_ty;
    let self_borrow = &ctx.self_borrow;
    let self_borrow_ty = &ctx.self_borrow_ty;
    let value_pairs_args = &ctx.value_pairs_args;
    let reentry_guard = &ctx.reentry_guard;

    // Per-mode factory state-fetch + box construction. Snapshot:
    // clone value_pairs() at factory time, stash the Vec. Live:
    // capture a Global<Object> of the parent, no value_pairs() call
    // yet.
    let factory_state_pre = if ctx.live {
        quote! {
            // Capture the parent wrapper as a Global so it stays
            // reachable for the iterator's lifetime. No `value_pairs()`
            // call here — each `next()` re-reads.
            let __parent_global: ::v8::Global<::v8::Object> =
                ::v8::Global::new(scope, __this);
            // Drop the External handle before the iterator template
            // install (which mutably borrows scope).
            drop(__ext);
        }
    } else {
        quote! {
            // SAFETY: the brand check above passed, so internal field
            // 0 holds a Box<#class_ty> raw pointer placed there by
            // gen_box_and_install_finalizer. The borrow ends before we
            // touch `scope` again (the snapshot clone is the last use).
            // For `&mut self` value_pairs we promote to *mut + &mut *.
            let __inflight_addr = __ext.value() as usize;
            #reentry_guard
            let __instance_ptr: #self_ptr_ty = __ext.value() as #self_ptr_ty;
            let __pairs: ::std::vec::Vec<(#key_ty, #value_ty)> = {
                let __instance: #self_borrow_ty =
                    unsafe { #self_borrow __instance_ptr };
                __instance.value_pairs #value_pairs_args
            };
            // Drop the External BEFORE re-entering scope for the
            // iterator template install. The `&mut`/`&` borrow above
            // already ended at the closing brace of the snapshot block.
            drop(__ext);
        }
    };

    let iter_box_construct = if ctx.live {
        quote! {
            ::std::boxed::Box::new(#iter_class_ty {
                __parent: __parent_global,
                __index: 0,
                __kind,
            })
        }
    } else {
        quote! {
            ::std::boxed::Box::new(#iter_class_ty {
                __pairs,
                __index: 0,
                __kind,
            })
        }
    };

    quote! {
        // -------------------------------------------------------------
        // Iterator factory callbacks (one per kind)
        // -------------------------------------------------------------

        /// Allocate an iterator instance with the given kind. In
        /// snapshot mode this clones `value_pairs()` and stashes the
        /// vec; in live mode this captures a `Global<Object>` of the
        /// parent and defers the value_pairs() reads to each `next()`.
        /// Brand-checks the receiver against the parent class.
        #[doc(hidden)]
        #[allow(non_snake_case, unused_variables, unused_mut)]
        fn __zs_iter_factory_impl(
            scope: &mut v8::PinScope,
            args: v8::FunctionCallbackArguments,
            mut rv: v8::ReturnValue,
            __kind: i32,
        ) {
            // Brand check: the receiver MUST be a parent-class
            // instance. Reuses the parent's cached prototype chain
            // walk emitted by `#[v8_class]`. Recovery preamble
            // delegates to the shared helpers (design §3.8).
            #factory_brand_check
            // Recover Box<#class_ty> from internal field 0.
            #factory_external_recovery

            // Per-mode state-fetch: snapshot clones value_pairs() now;
            // live captures a Global<Object> of the parent.
            #factory_state_pre

            // Create the iterator instance + bind state.
            let __it_tmpl = #iter_class_ty::install(scope);
            let __it_inst_tmpl = __it_tmpl.instance_template(scope);
            let __it_obj = match __it_inst_tmpl.new_instance(scope) {
                Some(o) => o,
                None => {
                    let __msg = #alloc_fail_msg_init;
                    let __exc = v8::Exception::error(scope, __msg);
                    scope.throw_exception(__exc);
                    return;
                }
            };
            // Set its prototype explicitly — `new_instance` from an
            // instance_template doesn't auto-chain to the
            // FunctionTemplate's prototype. Match the hand-rolled
            // URLSearchParams iterator shape (see search_params.rs::
            // iter_factory_callback).
            let __it_class_fn = __it_tmpl.get_function(scope).unwrap();
            let __proto_key = #proto_key_init;
            let __it_proto_v = __it_class_fn.get(scope, __proto_key.into()).unwrap();
            __it_obj.set_prototype(scope, __it_proto_v);

            let __boxed = #iter_box_construct;
            let __raw = ::std::boxed::Box::into_raw(__boxed);
            let __raw_addr = __raw as usize;
            let __ext = v8::External::new(scope, __raw as *mut ::std::ffi::c_void);
            __it_obj.set_internal_field(0, __ext.into());

            // Guaranteed finalizer to reclaim the Box on GC. Same shape
            // as `gen_box_and_install_finalizer` in
            // `v8_class/emit/constructor.rs` — including the deliberate
            // `mem::forget(__weak)` that leaks ~32 bytes of WeakData
            // per iterator instance (closes design §13.5 / §13.7).
            // Measurement protocol + accepted-leak rationale documented
            // verbatim on the v8_class helper. Iterator instances are
            // typically transient (1-5 lifetime per parent), so the
            // resident overhead per app is dominated by the parent
            // class's leak, not this one.
            let __weak = v8::Weak::with_guaranteed_finalizer(
                scope,
                __it_obj,
                ::std::boxed::Box::new(move || unsafe {
                    drop(::std::boxed::Box::from_raw(__raw_addr as *mut #iter_class_ty));
                }),
            );
            ::std::mem::forget(__weak);

            rv.set(__it_obj.into());
        }

        #[doc(hidden)]
        #[allow(non_snake_case)]
        pub(crate) fn #factory_keys_ident(
            scope: &mut v8::PinScope,
            args: v8::FunctionCallbackArguments,
            rv: v8::ReturnValue,
        ) {
            __zs_iter_factory_impl(scope, args, rv, #iter_kind_keys);
        }

        #[doc(hidden)]
        #[allow(non_snake_case)]
        pub(crate) fn #factory_values_ident(
            scope: &mut v8::PinScope,
            args: v8::FunctionCallbackArguments,
            rv: v8::ReturnValue,
        ) {
            __zs_iter_factory_impl(scope, args, rv, #iter_kind_values);
        }

        #[doc(hidden)]
        #[allow(non_snake_case)]
        pub(crate) fn #factory_entries_ident(
            scope: &mut v8::PinScope,
            args: v8::FunctionCallbackArguments,
            rv: v8::ReturnValue,
        ) {
            __zs_iter_factory_impl(scope, args, rv, #iter_kind_entries);
        }
    }
}

/// Emit the `<Class>::__zs_install_iterable_methods(scope, proto)`
/// bridge that the parent's `<Class>::install` calls to layer the
/// iterable surface on the prototype. Hard-codes `entries()` as the
/// `@@iterator` alias per WebIDL §3.7.10 default-iterator semantics.
pub(super) fn gen_install_bridge(ctx: &EmitCtx<'_>) -> TokenStream2 {
    let class_ty = ctx.class_ty;
    let factory_keys_ident = &ctx.factory_keys_ident;
    let factory_values_ident = &ctx.factory_values_ident;
    let factory_entries_ident = &ctx.factory_entries_ident;
    let for_each_ident = &ctx.for_each_ident;
    let keys_key_init = &ctx.keys_key_init;
    let values_key_init = &ctx.values_key_init;
    let entries_key_init = &ctx.entries_key_init;
    let foreach_key_init = &ctx.foreach_key_init;

    quote! {
        // -------------------------------------------------------------
        // Bridge: install the iterable methods on the parent's prototype
        // -------------------------------------------------------------

        #[allow(non_snake_case, dead_code)]
        impl #class_ty {
            /// Called from `<Class>::install` to layer the iterable
            /// surface (keys / values / entries / forEach / @@iterator)
            /// onto the parent's prototype. Idempotent: safe to call
            /// multiple times within an isolate; the second call
            /// overwrites with identical templates.
            ///
            /// The macro hard-codes `entries()` as the @@iterator
            /// alias per WebIDL §3.7.10 default-iterator semantics.
            #[doc(hidden)]
            pub(crate) fn __zs_install_iterable_methods<'s>(
                scope: &mut v8::PinScope<'s, '_>,
                proto: v8::Local<'s, v8::ObjectTemplate>,
            ) {
                {
                    let __key = #keys_key_init;
                    let __tmpl = v8::FunctionTemplate::new(scope, #factory_keys_ident);
                    proto.set(__key.into(), __tmpl.into());
                }
                {
                    let __key = #values_key_init;
                    let __tmpl = v8::FunctionTemplate::new(scope, #factory_values_ident);
                    proto.set(__key.into(), __tmpl.into());
                }
                // `entries` and `[Symbol.iterator]` MUST resolve to the
                // SAME FunctionTemplate per WebIDL §3.7.10 default
                // iterator — JS code commonly compares
                // `fd.entries === fd[Symbol.iterator]` and the answer
                // has to be `true`. Build the template once and bind
                // it under both keys.
                let __entries_tmpl = v8::FunctionTemplate::new(scope, #factory_entries_ident);
                {
                    let __key = #entries_key_init;
                    proto.set(__key.into(), __entries_tmpl.into());
                }
                {
                    let __key = #foreach_key_init;
                    let __tmpl = v8::FunctionTemplate::new(scope, #for_each_ident);
                    proto.set(__key.into(), __tmpl.into());
                }
                {
                    let __sym = v8::Symbol::get_iterator(scope);
                    proto.set(__sym.into(), __entries_tmpl.into());
                }
            }
        }
    }
}
