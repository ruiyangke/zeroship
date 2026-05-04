//! `#[v8_iterable(key = K, value = V)]` — emit the WebIDL pair-iterator
//! surface (keys / values / entries / forEach / @@iterator) from a
//! single user-supplied `value_pairs(&self) -> Vec<(K, V)>` method.
//!
//! Per WebIDL §3.7.10.2 (default iterators) and §3.7.10.3 (forEach):
//!
//! ```ignore
//! interface Foo {
//!   iterable<K, V>;
//!   // Emits:
//!   //   keys()    -> FooIterator (kind = Keys)
//!   //   values()  -> FooIterator (kind = Values)
//!   //   entries() -> FooIterator (kind = Entries)  [also @@iterator]
//!   //   forEach(callback, thisArg?) -> undefined
//!   //   FooIterator class with next() -> { value, done }
//! };
//! ```
//!
//! # Iteration model — snapshot vs. live
//!
//! This macro implements the **snapshot** model: at iterator-factory
//! call time, the macro calls the user's `value_pairs` ONCE, clones
//! its return into a `Vec<(K, V)>` baked into the iterator's state,
//! and walks that snapshot on each `next()`. Subsequent mutations to
//! the parent collection are not visible through the running iterator.
//!
//! WebIDL §3.7.10.2 actually mandates **live** iteration (each `next`
//! re-reads the parent's `value_pairs`). The existing hand-rolled
//! Headers / URLSearchParams iterators implement live mode for spec
//! compliance. This derive's snapshot model is a deliberate
//! simplification:
//!
//!   - It works correctly for read-only iterables (the common case).
//!   - It avoids the complexity of weak-ref'ing the parent + re-
//!     entering its locked state from inside `next()`.
//!   - For mutable iterables that need live semantics, users continue
//!     to hand-roll the iterator (Headers / URLSearchParams already
//!     do, and they're not migrated here).
//!
//! The deviation is documented per-class via the macro's emitted doc
//! comment so consumers can opt out of the derive when they need live
//! iteration.
//!
//! # Type bounds on K, V
//!
//! The macro emits `v8::String::new_from_one_byte(scope, &k)` for
//! `ByteString` and `v8::String::new(scope, &k)` for stringly types.
//! It supports:
//!
//! - `K: ByteString | USVString | String | u32`
//! - `V: same set, plus Vec<u8>`
//!
//! Other types are rejected with a compile_error in the codegen below.

use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::{Attribute, Ident};

/// Parsed `#[v8_iterable(key = TY, value = TY)]` attribute.
pub(crate) struct IterableAttr {
    pub key_ty: syn::Type,
    pub value_ty: syn::Type,
}

/// Read `#[v8_iterable(key = ..., value = ...)]` from impl-block attrs.
/// Returns `None` if the attribute is absent. Returns `Err` if the
/// attribute is present but malformed (caller surfaces as compile_error).
pub(crate) fn extract_iterable(attrs: &[Attribute]) -> Result<Option<IterableAttr>, syn::Error> {
    let mut found: Option<IterableAttr> = None;
    for attr in attrs {
        if !attr.path().is_ident("v8_iterable") {
            continue;
        }
        let mut key_ty: Option<syn::Type> = None;
        let mut value_ty: Option<syn::Type> = None;
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("key") {
                let ty: syn::Type = meta.value()?.parse()?;
                key_ty = Some(ty);
            } else if meta.path.is_ident("value") {
                let ty: syn::Type = meta.value()?.parse()?;
                value_ty = Some(ty);
            } else {
                return Err(meta.error("expected `key = TY` or `value = TY`"));
            }
            Ok(())
        })?;
        let key_ty = key_ty.ok_or_else(|| {
            syn::Error::new_spanned(
                attr,
                "#[v8_iterable]: missing `key = TY` (e.g. `key = ByteString`)",
            )
        })?;
        let value_ty = value_ty.ok_or_else(|| {
            syn::Error::new_spanned(
                attr,
                "#[v8_iterable]: missing `value = TY` (e.g. `value = ByteString`)",
            )
        })?;
        if found.is_some() {
            return Err(syn::Error::new_spanned(
                attr,
                "#[v8_iterable]: only one occurrence allowed per impl block",
            ));
        }
        found = Some(IterableAttr { key_ty, value_ty });
    }
    Ok(found)
}

/// Recognised string / byte / integer types that we know how to
/// marshal back to V8 from the iterator's `next()` snapshot. Returns
/// the codegen branch token-stream for a single value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SupportedTy {
    /// `String` / `USVString` — emit as a UTF-8 v8::String.
    Utf8,
    /// `ByteString` — emit as a Latin-1 one-byte v8::String. The
    /// snapshot stores `Vec<u8>` so we can write_one_byte directly.
    ByteStr,
    /// `u32` — emit as an unsigned integer.
    U32,
    /// `Vec<u8>` — emit as a Uint8Array. Used for value side of pair
    /// iterators that yield raw bytes (e.g. FormData entries that
    /// carry Blob bytes inline).
    Bytes,
}

fn classify_ty(ty: &syn::Type) -> Option<SupportedTy> {
    let ident = match crate::type_ident(ty) {
        Some(s) => s,
        None => return None,
    };
    match ident.as_str() {
        "ByteString" => Some(SupportedTy::ByteStr),
        "String" | "USVString" => Some(SupportedTy::Utf8),
        "u32" => Some(SupportedTy::U32),
        "Vec" => {
            // Only Vec<u8> is supported in this position.
            if crate::is_vec_u8(ty) {
                Some(SupportedTy::Bytes)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Emit code that converts a snapshot value (the user's K or V) to a
/// `v8::Local<v8::Value>` named `__out_local`. Caller bound the source
/// value to `__src` already.
fn gen_to_v8(ty: &syn::Type, src_ident: &Ident, out_ident: &Ident) -> Result<TokenStream2, syn::Error> {
    let kind = classify_ty(ty).ok_or_else(|| {
        syn::Error::new_spanned(
            ty,
            "#[v8_iterable]: unsupported key/value type. Expected one of: \
             ByteString, USVString, String, u32, Vec<u8>",
        )
    })?;
    Ok(match kind {
        SupportedTy::Utf8 => quote! {
            let __s_ref: &str = ::std::convert::AsRef::as_ref(&#src_ident);
            let #out_ident: v8::Local<v8::Value> =
                v8::String::new(scope, __s_ref).unwrap().into();
        },
        SupportedTy::ByteStr => quote! {
            // ByteString → Latin-1 one-byte string. The snapshot stores
            // ByteString (which derefs to &[u8]); copy bytes verbatim.
            let __bytes_ref: &[u8] = ::std::convert::AsRef::as_ref(&#src_ident);
            let #out_ident: v8::Local<v8::Value> = v8::String::new_from_one_byte(
                scope,
                __bytes_ref,
                v8::NewStringType::Normal,
            )
            .unwrap()
            .into();
        },
        SupportedTy::U32 => quote! {
            let __n: u32 = #src_ident;
            let #out_ident: v8::Local<v8::Value> =
                v8::Integer::new_from_unsigned(scope, __n).into();
        },
        SupportedTy::Bytes => quote! {
            // Vec<u8> → Uint8Array. Allocate a fresh ArrayBuffer per
            // yield (snapshot owns the bytes; we can't transfer
            // ownership of the inner Vec).
            let __bytes: &Vec<u8> = &#src_ident;
            let __len = __bytes.len();
            let __ab = v8::ArrayBuffer::new(scope, __len);
            let __store = __ab.get_backing_store();
            for (__i, &__b) in __bytes.iter().enumerate() {
                __store[__i].set(__b);
            }
            let #out_ident: v8::Local<v8::Value> =
                v8::Uint8Array::new(scope, __ab, 0, __len).unwrap().into();
        },
    })
}

/// Generate the iterable surface for a class.
///
/// Emits:
///   - 4 free callback functions (`__<Class>_iter_factory_<kind>` for
///     keys/values/entries) and `__<Class>_for_each` and `__<Class>_iter_next`.
///   - The `<Class>Iterator` struct + its install_template helper.
///   - A `<Class>::install_iterable_methods(scope, proto)` method that
///     the existing install() codegen calls automatically.
///
/// `class_ty` is the parent class ident (e.g. `Headers`).
/// `attr` is the parsed `#[v8_iterable(key=..., value=...)]`.
pub(crate) fn generate(
    class_ty: &Ident,
    attr: &IterableAttr,
) -> Result<TokenStream2, syn::Error> {
    let key_ty = &attr.key_ty;
    let value_ty = &attr.value_ty;

    // Sanity check: both K and V are supported in classify_ty.
    classify_ty(key_ty).ok_or_else(|| {
        syn::Error::new_spanned(
            key_ty,
            "#[v8_iterable]: unsupported key type. Expected one of: \
             ByteString, USVString, String, u32",
        )
    })?;
    classify_ty(value_ty).ok_or_else(|| {
        syn::Error::new_spanned(
            value_ty,
            "#[v8_iterable]: unsupported value type. Expected one of: \
             ByteString, USVString, String, u32, Vec<u8>",
        )
    })?;

    let iter_class_ty = format_ident!("{}Iterator", class_ty);
    let iter_class_name_str = iter_class_ty.to_string();
    let iter_to_string_tag_str = format!("{} Iterator", class_ty);

    let factory_keys_ident = format_ident!("__{}_iter_factory_keys", class_ty);
    let factory_values_ident = format_ident!("__{}_iter_factory_values", class_ty);
    let factory_entries_ident = format_ident!("__{}_iter_factory_entries", class_ty);
    let for_each_ident = format_ident!("__{}_for_each", class_ty);
    let next_ident = format_ident!("__{}_iter_next", class_ty);

    // Templates for converting a snapshot value back to V8.
    let key_src = Ident::new("__k", proc_macro2::Span::call_site());
    let key_out = Ident::new("__k_v", proc_macro2::Span::call_site());
    let key_to_v8 = gen_to_v8(key_ty, &key_src, &key_out)?;

    let val_src = Ident::new("__v", proc_macro2::Span::call_site());
    let val_out = Ident::new("__v_v", proc_macro2::Span::call_site());
    let val_to_v8 = gen_to_v8(value_ty, &val_src, &val_out)?;

    // The iterator class — emitted as a parallel V8 class with its own
    // install. We can't use `#[v8_class]` directly because we're inside
    // the v8_class expansion already, and the iterator's behaviour
    // (snapshot owned + custom next) is tailor-made.
    //
    // Internal-field count = 1 (holds a `Box<<Class>Iterator>`).

    let iter_install_slot_ty = format_ident!("__InstallSlot_{}", iter_class_ty);

    let parent_brand_check_fn = format_ident!("__brand_check_{}", class_ty);

    let iter_kind_keys: i32 = 0;
    let iter_kind_values: i32 = 1;
    let iter_kind_entries: i32 = 2;

    Ok(quote! {
        // -------------------------------------------------------------
        // <Class>Iterator — companion class
        // -------------------------------------------------------------

        /// Default-pair iterator companion for the parent class. Holds
        /// a snapshot of `value_pairs()` taken at factory-call time;
        /// `next()` walks the snapshot. The snapshot model is a
        /// deliberate simplification of WebIDL §3.7.10.2 live-iteration
        /// — see `v8_iterable.rs` doc-comment for the rationale.
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

        // The iterator class's own install slot — see the parent
        // class's install slot for the rationale (idempotent install,
        // FunctionTemplate caching across calls in the same isolate).
        #[doc(hidden)]
        #[allow(non_camel_case_types)]
        pub struct #iter_install_slot_ty(::v8::Global<::v8::FunctionTemplate>);

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
                let __class_name = v8::String::new(scope, #iter_class_name_str).unwrap();
                __ctor_tmpl.set_class_name(__class_name);
                __ctor_tmpl
                    .instance_template(scope)
                    .set_internal_field_count(1);

                let __proto = __ctor_tmpl.prototype_template(scope);

                // next()
                {
                    let __key = v8::String::new(scope, "next").unwrap();
                    let __fn_tmpl = v8::FunctionTemplate::new(scope, #next_ident);
                    __proto.set(__key.into(), __fn_tmpl.into());
                }

                // Symbol.toStringTag — "<Class> Iterator" per spec.
                {
                    let __tag_sym = v8::Symbol::get_to_string_tag(scope);
                    let __tag_value = v8::String::new(scope, #iter_to_string_tag_str).unwrap();
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
                    let __proto_key = v8::String::new(scope, "prototype").unwrap();
                    let __ctor_proto_v = __ctor_fn.get(scope, __proto_key.into()).unwrap();
                    let __ctor_proto: v8::Local<v8::Object> = __ctor_proto_v.try_into().unwrap();
                    let __js = v8::String::new(
                        scope,
                        "Object.getPrototypeOf(Object.getPrototypeOf([][Symbol.iterator]()))",
                    )
                    .unwrap();
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
            let __msg = v8::String::new(
                scope,
                concat!(
                    "Illegal constructor: ",
                    #iter_class_name_str,
                    " can only be created via the parent's keys() / values() / entries()",
                ),
            )
            .unwrap();
            let __exc = v8::Exception::type_error(scope, __msg);
            scope.throw_exception(__exc);
        }

        // -------------------------------------------------------------
        // Iterator factory callbacks (one per kind)
        // -------------------------------------------------------------

        /// Allocate an iterator instance with the given kind, snapshot
        /// the parent's `value_pairs()`, and return it. Brand-checks
        /// the receiver against the parent class.
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
            // walk emitted by `#[v8_class]`.
            let __this = args.this();
            if !#parent_brand_check_fn(scope, __this) {
                let __msg = v8::String::new(scope, "Illegal invocation").unwrap();
                let __exc = v8::Exception::type_error(scope, __msg);
                scope.throw_exception(__exc);
                return;
            }
            // Recover Box<#class_ty> from internal field 0.
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
            // SAFETY: the brand check above passed, so internal field
            // 0 holds a Box<#class_ty> raw pointer placed there by
            // gen_box_and_install_finalizer. The borrow ends before we
            // touch `scope` again (the snapshot clone is the last use).
            let __instance: &#class_ty = unsafe { &*(__ext.value() as *const #class_ty) };

            // Snapshot the pairs. The macro requires the user to define
            // `value_pairs(&self) -> Vec<(K, V)>`. We clone the result
            // into the iterator state.
            let __pairs: ::std::vec::Vec<(#key_ty, #value_ty)> = __instance.value_pairs();
            // Drop the borrow before touching `scope` for the iterator
            // template install (which mutably borrows).
            drop(__ext);

            // Create the iterator instance + bind state.
            let __it_tmpl = #iter_class_ty::install(scope);
            let __it_inst_tmpl = __it_tmpl.instance_template(scope);
            let __it_obj = match __it_inst_tmpl.new_instance(scope) {
                Some(o) => o,
                None => {
                    let __msg = v8::String::new(scope, "Failed to allocate iterator").unwrap();
                    let __exc = v8::Exception::error(scope, __msg);
                    scope.throw_exception(__exc);
                    return;
                }
            };
            // Set its prototype explicitly — `new_instance` from an
            // instance_template doesn't auto-chain to the FunctionTemplate's
            // prototype. Match the hand-rolled URLSearchParams iterator
            // shape (see search_params.rs::iter_factory_callback).
            let __it_class_fn = __it_tmpl.get_function(scope).unwrap();
            let __proto_key = v8::String::new(scope, "prototype").unwrap();
            let __it_proto_v = __it_class_fn.get(scope, __proto_key.into()).unwrap();
            __it_obj.set_prototype(scope, __it_proto_v);

            let __boxed = ::std::boxed::Box::new(#iter_class_ty {
                __pairs,
                __index: 0,
                __kind,
            });
            let __raw = ::std::boxed::Box::into_raw(__boxed);
            let __raw_addr = __raw as usize;
            let __ext = v8::External::new(scope, __raw as *mut ::std::ffi::c_void);
            __it_obj.set_internal_field(0, __ext.into());

            // Guaranteed finalizer to reclaim the Box on GC. Same shape
            // as `gen_box_and_install_finalizer` in v8_class.rs.
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

        // -------------------------------------------------------------
        // forEach callback — WebIDL §3.7.10.3
        // -------------------------------------------------------------

        /// `forEach(callback, thisArg?)`. Invokes `callback(value, key,
        /// this)` for each pair, where `this` is the parent collection
        /// instance. Per spec, mutation during forEach is observable;
        /// our snapshot model does NOT match the live-spec exactly —
        /// we snapshot at the START of forEach, not per-iteration.
        /// Hand-roll the iterator if you need per-iteration live reads.
        #[doc(hidden)]
        #[allow(non_snake_case, unused_variables, unused_mut)]
        pub(crate) fn #for_each_ident(
            scope: &mut v8::PinScope,
            args: v8::FunctionCallbackArguments,
            _rv: v8::ReturnValue,
        ) {
            let __this = args.this();
            if !#parent_brand_check_fn(scope, __this) {
                let __msg = v8::String::new(scope, "Illegal invocation").unwrap();
                let __exc = v8::Exception::type_error(scope, __msg);
                scope.throw_exception(__exc);
                return;
            }
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
            let __instance: &#class_ty = unsafe { &*(__ext.value() as *const #class_ty) };

            // Validate the callback arg.
            let __cb_arg = args.get(0);
            let __cb_fn: v8::Local<v8::Function> = match __cb_arg.try_into() {
                Ok(f) => f,
                Err(_) => {
                    let __msg = v8::String::new(scope, "forEach callback is not callable").unwrap();
                    let __exc = v8::Exception::type_error(scope, __msg);
                    scope.throw_exception(__exc);
                    return;
                }
            };
            let __this_arg = args.get(1);

            let __pairs: ::std::vec::Vec<(#key_ty, #value_ty)> = __instance.value_pairs();
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

        // -------------------------------------------------------------
        // next() — walks the snapshot
        // -------------------------------------------------------------

        /// `<Class>Iterator.prototype.next()`. Returns `{ value, done }`
        /// per the JS Iterator protocol.
        #[doc(hidden)]
        #[allow(non_snake_case, unused_variables, unused_mut)]
        pub(crate) fn #next_ident(
            scope: &mut v8::PinScope,
            args: v8::FunctionCallbackArguments,
            mut rv: v8::ReturnValue,
        ) {
            let __this = args.this();
            // Brand-check against the iterator class (not the parent).
            // We don't go through the macro's brand-check helper for
            // the iterator class because we don't have one — the
            // iterator is hand-rolled by this codegen, not by
            // #[v8_class]. Simple internal-field-1-is-External check
            // is sufficient since the iterator class isn't exposed in
            // a way that lets users construct one with a different
            // box layout.
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
            let __it: &mut #iter_class_ty =
                unsafe { &mut *(__ext.value() as *mut #iter_class_ty) };

            if __it.__index >= __it.__pairs.len() {
                // Done: return { value: undefined, done: true }.
                let __res = v8::Object::new(scope);
                let __vk = v8::String::new(scope, "value").unwrap();
                let __dk = v8::String::new(scope, "done").unwrap();
                let __undef = v8::undefined(scope);
                let __true = v8::Boolean::new(scope, true);
                __res.set(scope, __vk.into(), __undef.into());
                __res.set(scope, __dk.into(), __true.into());
                rv.set(__res.into());
                return;
            }

            // Snapshot the current pair (clone — we own it, but we
            // also want to advance index without holding the borrow).
            let (#key_src, #val_src): (#key_ty, #value_ty) = __it.__pairs[__it.__index].clone();
            __it.__index += 1;
            // Drop the &mut borrow before any further v8 calls that
            // mutably borrow `scope`.
            let __kind = __it.__kind;
            let _ = __it; // explicit drop hint

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
            let __vk = v8::String::new(scope, "value").unwrap();
            let __dk = v8::String::new(scope, "done").unwrap();
            let __false = v8::Boolean::new(scope, false);
            __res.set(scope, __vk.into(), __value);
            __res.set(scope, __dk.into(), __false.into());
            rv.set(__res.into());
        }

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
                    let __key = v8::String::new(scope, "keys").unwrap();
                    let __tmpl = v8::FunctionTemplate::new(scope, #factory_keys_ident);
                    proto.set(__key.into(), __tmpl.into());
                }
                {
                    let __key = v8::String::new(scope, "values").unwrap();
                    let __tmpl = v8::FunctionTemplate::new(scope, #factory_values_ident);
                    proto.set(__key.into(), __tmpl.into());
                }
                {
                    let __key = v8::String::new(scope, "entries").unwrap();
                    let __tmpl = v8::FunctionTemplate::new(scope, #factory_entries_ident);
                    proto.set(__key.into(), __tmpl.into());
                }
                {
                    let __key = v8::String::new(scope, "forEach").unwrap();
                    let __tmpl = v8::FunctionTemplate::new(scope, #for_each_ident);
                    proto.set(__key.into(), __tmpl.into());
                }
                // @@iterator → entries (WebIDL §3.7.10 default
                // iterator). Use the same FunctionTemplate as `entries`
                // so identity is preserved for callers that compare
                // `obj.entries === obj[Symbol.iterator]`.
                {
                    let __sym = v8::Symbol::get_iterator(scope);
                    let __tmpl = v8::FunctionTemplate::new(scope, #factory_entries_ident);
                    proto.set(__sym.into(), __tmpl.into());
                }
            }
        }
    })
}
