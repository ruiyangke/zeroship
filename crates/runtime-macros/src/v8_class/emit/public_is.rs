//! Public `__zs_is_<Class>` brand-check entry point.
//!
//! Wave 3 commit 3 — extracted from `mod.rs`'s 195-line megaquote
//! (design `docs/proposals/runtime-macros-refactor.md` §4.1, F3).
//! Wave 5 will add a stable `<Class>::is_instance` trait impl
//! alongside this underscored symbol (design §3.5, F2).

use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};

use super::super::shared::class_config::ClassConfig;

/// Emit the `pub fn __zs_is_<Class>(scope, v) -> bool` wrapper that
/// gates `__brand_check_<Class>` behind a `Local<Value>::try_into`
/// to `Local<Object>`. Cross-class type queries (e.g. is_blob_instance
/// in a Request body coercion) call this without hand-rolling a
/// prototype chain walk.
///
/// Returns `false` for non-Object values (primitives, null, undefined)
/// and `false` if the class hasn't been installed in the current
/// isolate (the install slot is empty), matching
/// `__brand_check_<Class>`'s behaviour.
pub(super) fn gen_public_is_fn(cfg: &ClassConfig) -> TokenStream2 {
    let class_ty = cfg.class_ty;
    let brand_check_fn = format_ident!("__brand_check_{}", class_ty);
    let public_is_fn = format_ident!("__zs_is_{}", class_ty);

    quote! {
        /// Public brand check: is `v` an instance of this class (or a
        /// subclass via `#[v8_inherit]`) in the current isolate?
        ///
        /// Re-exports the macro's per-class brand-check via a stable
        /// `__zs_is_<Class>(scope, v: Local<Value>) -> bool` symbol so
        /// cross-class type queries (e.g. `is_blob_instance` /
        /// `is_form_data_instance` checks in a Request body coercion)
        /// don't have to hand-roll prototype-chain walks.
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
    }
}
