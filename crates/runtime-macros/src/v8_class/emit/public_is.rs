//! Public brand-check entry point for `#[v8_class]` types.
//!
//! Wave 3 commit 3 — extracted from `mod.rs`'s 195-line megaquote
//! (design `docs/proposals/runtime-macros-refactor.md` §4.1, F3).
//! Wave 5c (this wave) — added the typed `<Class>::is_instance` and
//! the sealed `V8ClassInstance` trait impl alongside the legacy
//! underscored `__zs_is_<Class>` (design §3.5, closes F2).
//!
//! Symbol matrix:
//!
//! | Symbol | Visibility | Purpose | Stability |
//! |---|---|---|---|
//! | `__brand_check_<Class>` | `pub(crate)` | inner walker | private |
//! | `__zs_is_<Class>` | `pub` (`#[doc(hidden)]`) | legacy grep target | deprecated, removed Wave 8 |
//! | `<Class>::is_instance` | `pub` (inherent) | typed entry point | stable |
//! | `<Class> as V8ClassInstance` | trait impl | generic bound | stable |
//!
//! See `crates/runtime-macros/STABILITY.md` for the formal contract.

use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};

use super::super::shared::class_config::ClassConfig;

/// Emit the public brand-check entry points for a class:
///
/// 1. `pub fn __zs_is_<Class>(scope, v) -> bool` — the legacy
///    underscored symbol. Kept `#[doc(hidden)]` for back-compat with
///    pre-Wave-5c call sites; deprecated in favour of `<Class>::is_instance`.
///    Wave 8 will remove this per the design's deprecation policy
///    (STABILITY.md, F2).
/// 2. `impl <Class> { pub fn is_instance(scope, v) -> bool { ... } }`
///    — the new stable inherent method (Wave 5c, design §3.5).
/// 3. `impl ::zeroship_runtime::macro_runtime::__private::Sealed for <Class>`
///    + `impl ::zeroship_runtime::macro_runtime::V8ClassInstance for <Class>`
///    — the sealed-trait pattern that lets generic code bound on
///    `T: V8ClassInstance` while preventing third-party impls
///    (Sealed lives in a private module).
///
/// All three forms gate the `__brand_check_<Class>` walker (which
/// requires `Local<Object>`) behind a `Local<Value>::try_into`
/// fast-fail for non-Object values.
pub(super) fn gen_public_is_fn(cfg: &ClassConfig) -> TokenStream2 {
    let class_ty = cfg.class_ty;
    let brand_check_fn = format_ident!("__brand_check_{}", class_ty);
    let public_is_fn = format_ident!("__zs_is_{}", class_ty);

    quote! {
        /// Public brand check: is `v` an instance of this class (or a
        /// subclass via `#[v8_inherit]`) in the current isolate?
        ///
        /// **Deprecated**: prefer the typed `<Class>::is_instance`
        /// method emitted alongside this fn (Wave 5c, design §3.5).
        /// This symbol stays for back-compat with pre-Wave-5c callers;
        /// scheduled for removal in Wave 8 per the macro's
        /// `STABILITY.md`.
        ///
        /// Non-Object values (primitives, null, undefined) return
        /// `false` — the underlying `__brand_check_<Class>` requires
        /// `Local<Object>`, so this wrapper does the
        /// `Local::<Object>::try_from` gate for the caller. Spec
        /// alignment: WebIDL §3.7 brand identity treats only objects
        /// as candidates.
        ///
        /// Returns `false` if the class hasn't been installed in the
        /// current isolate (the install slot is empty), matching
        /// `__brand_check_<Class>`'s behaviour.
        #[doc(hidden)]
        #[allow(non_snake_case, dead_code)]
        pub fn #public_is_fn(
            scope: &mut v8::PinScope,
            v: v8::Local<v8::Value>,
        ) -> bool {
            let obj: v8::Local<v8::Object> = match v.try_into() {
                Ok(o) => o,
                Err(_) => return false,
            };
            #brand_check_fn(scope, obj)
        }

        // ----- Wave 5c, design §3.5 — typed brand-check API -----

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
            /// This is the stable entry point introduced in Wave 5c
            /// (design `docs/proposals/runtime-macros-refactor.md` §3.5).
            /// Prefer this over the legacy `__zs_is_<Class>` symbol —
            /// the latter is deprecated and scheduled for removal in
            /// Wave 8.
            pub fn is_instance(
                scope: &mut v8::PinScope,
                value: v8::Local<v8::Value>,
            ) -> bool {
                #public_is_fn(scope, value)
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
