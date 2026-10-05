//! Per-class isolate-slot marker type.
//!
//! Emits the struct that holds the cached install template.

use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};

use super::super::shared::class_config::ClassConfig;

/// Emit the per-class isolate-slot marker struct:
///
///   pub struct __InstallSlot_<Class>(::v8::Global<::v8::FunctionTemplate>);
///
/// It has to live at module scope (a `pub struct` can't live inside an
/// `impl` block) and is named after the class so two classes never
/// collide on a single `TypeId`. The struct is `#[doc(hidden)]` because
/// it's an implementation detail: consumers query the cached template
/// via `Foo::install(scope)`.
pub(super) fn gen_install_slot_types(cfg: &ClassConfig) -> TokenStream2 {
    let class_ty = cfg.class_ty;
    let install_slot_ty = format_ident!("__InstallSlot_{}", class_ty);

    quote! {
        /// Per-class isolate-slot marker holding the cached
        /// FunctionTemplate. Exists so `Foo::install` is idempotent
        /// per isolate — required for `#[v8_inherit]` to chain
        /// derived classes onto the SAME template the global was
        /// bound to (otherwise `instanceof` walks a different
        /// `[[FunctionPrototype]]` and returns false).
        #[doc(hidden)]
        #[allow(non_camel_case_types)]
        pub struct #install_slot_ty(::v8::Global<::v8::FunctionTemplate>);
    }
}
