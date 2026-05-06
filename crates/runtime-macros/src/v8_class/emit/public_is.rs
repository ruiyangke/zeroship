//! Public brand-check entry point for `#[v8_class]` types.
//!
//! This file emits the typed `<Class>::is_instance` API and the sealed
//! `V8ClassInstance` trait impl. The older underscored
//! `__zs_is_<Class>` shim has been removed.
//!
//! Symbol matrix:
//!
//! | Symbol | Visibility | Purpose | Stability |
//! |---|---|---|---|
//! | `__brand_check_<Class>` | `pub(crate)` | inner walker | private |
//! | `<Class>::is_instance` | `pub` (inherent) | typed entry point | stable |
//! | `<Class> as V8ClassInstance` | trait impl | generic bound | stable |
//!
//! See `crates/runtime-macros/STABILITY.md` for the formal contract.

use proc_macro2::TokenStream as TokenStream2;
use quote::quote;

use super::super::shared::class_config::ClassConfig;

/// Emit the public brand-check entry points for a class:
///
/// 1. `impl <Class> { pub fn is_instance(scope, v) -> bool { ... } }`
///    — the stable inherent method. Calls
///    `__brand_check_<Class>` directly after the
///    `Local::<Object>::try_from` fast-fail for non-Object values.
/// 2. `impl ::zeroship_runtime::macro_runtime::__private::Sealed for <Class>`
///    + `impl ::zeroship_runtime::macro_runtime::V8ClassInstance for <Class>`
///    — the sealed-trait pattern that lets generic code bound on
///    `T: V8ClassInstance` while preventing third-party impls
///    (Sealed lives in a private module).
///
/// The legacy `pub fn __zs_is_<Class>` shim is no longer emitted.
/// `<Class>::is_instance` is the public entry point and now
/// owns the `try_from` gate and the brand-check call directly.
pub(super) fn gen_public_is_fn(cfg: &ClassConfig) -> TokenStream2 {
    let class_ty = cfg.class_ty;
    // Brand-check identifier cached on ClassConfig.
    let brand_check_fn = &cfg.brand_check_ident;

    quote! {
        // ----- Typed brand-check API -----

        #[allow(non_snake_case, dead_code)]
        impl #class_ty {
            /// Brand-check: returns `true` iff `value` is an instance
            /// of this class (or a subclass via `#[v8_inherit]`) in
            /// the current isolate.
            ///
            /// Non-Object values (primitives, null, undefined) return
            /// `false`, as does an isolate where the class hasn't been
            /// installed. Spec alignment: WebIDL §3.7 brand identity.
            ///
            /// This is the stable public entry point. The old
            /// `__zs_is_<Class>` shim has been removed, so this method
            /// now owns the `Local<Value>::try_into` gate directly.
            pub fn is_instance(
                scope: &mut v8::PinScope,
                value: v8::Local<v8::Value>,
            ) -> bool {
                let obj: v8::Local<v8::Object> = match value.try_into() {
                    Ok(o) => o,
                    Err(_) => return false,
                };
                #brand_check_fn(scope, obj)
            }
        }

        // Sealed-trait impls. The `Sealed` super-trait lives in
        // `zeroship_runtime::macro_runtime::__private::Sealed` — only
        // the macro emits `impl Sealed for X`, so user code can't
        // satisfy the bound and can't write its own brand-check.
        #[allow(non_snake_case)]
        impl ::zeroship_runtime::macro_runtime::__private::Sealed for #class_ty {}

        #[allow(non_snake_case)]
        impl ::zeroship_runtime::macro_runtime::V8ClassInstance for #class_ty {
            fn is_instance(
                scope: &mut ::v8::PinScope,
                value: ::v8::Local<::v8::Value>,
            ) -> bool {
                <Self>::is_instance(scope, value)
            }
        }
    }
}
