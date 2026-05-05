//! Per-class isolate-slot marker types.
//!
//! Wave 3 commit 3 — extracted from `mod.rs`'s 195-line megaquote
//! (design `docs/proposals/runtime-macros-refactor.md` §4.1, F3).
//! Emits the two structs that hold the cached install template and
//! the cached `Foo.prototype` for brand checks.

use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};

use super::super::shared::class_config::ClassConfig;

/// Emit the two per-class isolate-slot marker structs:
///
///   pub struct __InstallSlot_<Class>(::v8::Global<::v8::FunctionTemplate>);
///   pub struct __BrandSlot_<Class>(::v8::Global<::v8::Object>);
///
/// These have to live at module scope (a `pub struct` can't live
/// inside an `impl` block) and are named after the class so two
/// classes never collide on a single TypeId. The structs are
/// `#[doc(hidden)]` because they're an implementation detail —
/// consumers query the cached template via `Foo::install(scope)`.
pub(super) fn gen_install_slot_types(cfg: &ClassConfig) -> TokenStream2 {
    let class_ty = cfg.class_ty;
    let install_slot_ty = format_ident!("__InstallSlot_{}", class_ty);
    let brand_slot_ty = format_ident!("__BrandSlot_{}", class_ty);

    quote! {
        /// Per-class isolate-slot marker holding the cached
        /// FunctionTemplate. Exists so `Foo::install` is idempotent
        /// per isolate — required for `#[v8_inherit]` to chain
        /// derived classes onto the SAME template the global was
        /// bound to (otherwise `instanceof` walks a different
        /// [[FunctionPrototype]] and returns false).
        #[doc(hidden)]
        #[allow(non_camel_case_types)]
        pub struct #install_slot_ty(::v8::Global<::v8::FunctionTemplate>);

        /// Per-class isolate-slot marker holding `Foo.prototype` for
        /// WebIDL §3.7 brand checks. Captured eagerly during `install`
        /// (after `get_function`) and consulted by every method,
        /// getter, and setter callback before the unsafe internal-field
        /// deref.
        ///
        /// Without this, the only "brand check" in the prologue is "is
        /// internal field 0 an External" — which any `#[v8_class]`
        /// instance with one internal field passes, allowing
        /// `Headers.prototype.append.call(blob)` to reinterpret the
        /// Blob's box as a Headers and write Vec<u8> internals into
        /// arbitrary memory (UB).
        #[doc(hidden)]
        #[allow(non_camel_case_types)]
        pub struct #brand_slot_ty(::v8::Global<::v8::Object>);
    }
}
