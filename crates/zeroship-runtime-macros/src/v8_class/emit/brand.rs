//! Brand and state-installer codegen.
//!
//! Emits the per-class `__brand_check_<Class>` fn used by every
//! method/getter/setter callback prologue (via
//! `shared::recover_box`) before the unsafe internal-field deref,
//! `<Class>::__zs_brand`, which marks a wrapper with the brand the check
//! looks for, and `<Class>::__zs_install`, the one way a wrapper of the
//! class gets its native state.

use proc_macro2::TokenStream as TokenStream2;
use quote::quote;

use super::super::shared::class_config::ClassConfig;

/// The class's internal field count, as a const expression: 1, or 2 for slot
/// 1 when the class has fastcall methods. A derived class takes the larger of
/// its own count and its base's, so a base's fastcall shim always finds slot 1
/// on it.
fn field_count(cfg: &ClassConfig) -> TokenStream2 {
    let own: usize = if cfg.has_any_fastcall { 2 } else { 1 };
    cfg.inherit_base.as_ref().map_or_else(
        || quote! { #own },
        |base| quote! {{
            let __own: usize = #own;
            let __base: usize =
                <#base as ::zeroship_runtime::macro_runtime::brand::ClassState>::FIELD_COUNT;
            if __own > __base { __own } else { __base }
        }},
    )
}

/// Emit `__brand_check_<Class>`, `<Class>::__zs_brand` and
/// `<Class>::__zs_install`.
///
/// The check asks `zeroship_runtime::brand` whether the receiver is a
/// wrapper this runtime branded as holding a `Box<State>`. The brand is
/// a per-isolate private symbol carrying the same `External` as
/// internal field 0, so script can neither forge it (`Object.create`,
/// `Object.setPrototypeOf`) nor move it onto another class's wrapper,
/// which a prototype-chain walk cannot rule out.
///
/// `__zs_brand` marks a wrapper for the class's own state type and,
/// under `#[v8_inherit(Base)]`, for every base state type as well, so
/// the base class's callbacks read the derived box as their own. That is
/// sound only when the base state sits at offset zero of the derived
/// state, and the emitted constants prove it at compile time: with
/// `state_field = f`, field `f` has exactly the base class's state type
/// (`ClassState::State`, compared through a raw pointer so no deref
/// coercion can stand in for it) and offset zero; without it, the base state is
/// zero-sized. Either way the derived state is at least as aligned.
///
/// `__zs_install` writes everything a wrapper's native state consists
/// of: the box in internal field 0, the same pointer in slot 1 when the
/// class has fastcall methods (the fast shim reads it there with no
/// brand check), the brand, and the finalizer that drops the box. The
/// constructor callback and every wrapper the runtime builds by hand go
/// through it, so no path can leave the fast shim's slot unset.
pub(super) fn gen_brand_check_helpers(cfg: &ClassConfig) -> TokenStream2 {
    let class_ty = cfg.class_ty;
    let state_ty = cfg.state_ty;
    let brand_check_fn = &cfg.brand_check_ident;
    let base_brand = cfg.inherit_base.as_ref().map(|base| {
        quote! { <#base>::__zs_brand(scope, obj, state); }
    });
    let base_layout = cfg.inherit_base.as_ref().map(|base| {
        let base_state = quote! {
            <#base as ::zeroship_runtime::macro_runtime::brand::ClassState>::State
        };
        let placement = cfg.inherit_state_field.as_ref().map_or_else(
            || quote! {
                const _: () = ::core::assert!(
                    ::core::mem::size_of::<#base_state>() == 0,
                    "#[v8_inherit(Base)]: the base state is not zero-sized; name the derived state's field holding it with state_field = f",
                );
            },
            |field| quote! {
                // The base state is field `#field` itself, at offset zero. A raw
                // pointer, unlike a reference, never deref-coerces, so a field
                // that only points at a base state (a `Box`, an `Rc`, a
                // reference) does not pass for one.
                const _: fn(&#state_ty) -> *const #base_state =
                    |__state| ::core::ptr::addr_of!(__state.#field);
                const _: () = ::core::assert!(
                    ::core::mem::offset_of!(#state_ty, #field) == 0,
                    "#[v8_inherit(Base, state_field = f)]: f must be the derived state's field at offset zero",
                );
            },
        );
        quote! {
            #placement
            const _: () = ::core::assert!(
                ::core::mem::align_of::<#base_state>() <= ::core::mem::align_of::<#state_ty>(),
                "#[v8_inherit]: the derived state must be at least as aligned as the base state",
            );
        }
    });
    let field_count = field_count(cfg);

    quote! {
        /// Brand-check helper: true iff `obj` is a wrapper this runtime
        /// branded as holding this class's state (or a derived class's,
        /// via `#[v8_inherit]`).
        #[doc(hidden)]
        #[allow(non_snake_case, dead_code)]
        fn #brand_check_fn(
            scope: &mut v8::PinScope,
            obj: v8::Local<v8::Object>,
        ) -> bool {
            ::zeroship_runtime::macro_runtime::brand::is::<#state_ty>(scope, obj)
        }

        impl ::zeroship_runtime::macro_runtime::brand::ClassState for #class_ty {
            type State = #state_ty;
            const FIELD_COUNT: usize = #field_count;
        }

        #base_layout

        impl #class_ty {
            /// Brand `obj`, whose internal field 0 holds `state` (a
            /// `Box` of this class's state), as a wrapper of this class
            /// and of every class it inherits from. Only
            /// `__zs_install` and a derived class's `__zs_brand` call
            /// it.
            #[doc(hidden)]
            pub fn __zs_brand(
                scope: &mut v8::PinScope,
                obj: v8::Local<v8::Object>,
                state: v8::Local<v8::External>,
            ) {
                ::zeroship_runtime::macro_runtime::brand::mark::<#state_ty>(scope, obj, state);
                #base_brand
            }

            /// Install `state` as the native state of `obj`, a wrapper
            /// made from this class's template: box it into internal
            /// field 0 (and slot 1, for a class with fastcall methods),
            /// brand the wrapper, and drop the box when V8 collects it.
            ///
            /// `None`, with nothing installed, when `obj` lacks this
            /// class's internal fields or already holds a state.
            #[doc(hidden)]
            pub fn __zs_install<'__zs>(
                scope: &mut v8::PinScope<'__zs, '_>,
                obj: v8::Local<v8::Object>,
                state: #state_ty,
            ) -> ::core::option::Option<v8::Local<'__zs, v8::External>> {
                const __FIELD_COUNT: usize =
                    <#class_ty as ::zeroship_runtime::macro_runtime::brand::ClassState>::FIELD_COUNT;
                if obj.internal_field_count() < __FIELD_COUNT {
                    return ::core::option::Option::None;
                }
                if obj
                    .get_internal_field(scope, 0)
                    .is_some_and(|__field| v8::Local::<v8::External>::try_from(__field).is_ok())
                {
                    return ::core::option::Option::None;
                }
                let __raw = ::std::boxed::Box::into_raw(::std::boxed::Box::new(state));
                let __raw_addr = __raw as usize;
                let __ext = v8::External::new(scope, __raw.cast());
                obj.set_internal_field(0, __ext.into());
                if __FIELD_COUNT > 1 {
                    // The fastcall shim of this class, or of a class it
                    // inherits from, reads the state here. tag = 0: must
                    // match the tag the shim passes to
                    // get_aligned_pointer_from_internal_field.
                    obj.set_aligned_pointer_in_internal_field(1, __raw.cast(), 0);
                }
                Self::__zs_brand(scope, obj, __ext);
                // SAFETY: __raw_addr was Box::into_raw'd from a Box of
                // this class's state; the finalizer is the only code that
                // frees it, once, when V8 reclaims `obj`.
                let __weak = v8::Weak::with_guaranteed_finalizer(
                    scope,
                    obj,
                    ::std::boxed::Box::new(move || unsafe {
                        drop(::std::boxed::Box::from_raw(__raw_addr as *mut #state_ty));
                    }),
                );
                // Dropping the handle would cancel the finalizer; the
                // guaranteed variant fires on collection or isolate
                // disposal either way.
                ::std::mem::forget(__weak);
                ::core::option::Option::Some(__ext)
            }
        }
    }
}
