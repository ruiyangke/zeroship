//! Top-level assembly of `#[v8_class]` macro emission.
//!
//! Split out from the old monolithic emit assembly.
//! The orchestrator [`assemble_tokens`] composes per-fragment helpers
//! (slot types, brand check, public_is, install fn, per-method
//! callbacks, fastcall shims, iterable codegen) into the final token
//! stream returned to the proc-macro driver.
//!
//! Per-fragment helpers each emit ≤80 LOC of tokens and live in their
//! own files for testability. The install fn still lives as a single
//! generated block in `emit/install.rs`, but the old megaquote wrapper
//! is gone.
//!
//! Byte-identity contract: the output of `assemble_tokens` is
//! lexically identical to the pre-refactor `expand_tokens` output for
//! every test class (3 insta snapshots + 246 v8_*_smoke tests).
//! Structural extraction only; no emit-shape changes.

use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::ItemImpl;

use super::fastcall::gen_fastcall_callback;
use super::shared::class_config::ClassConfig;
use super::{ClassMethod, MethodKind};
use constructor::{gen_constructor_callback, gen_default_constructor_callback};
use getter::gen_same_object_getter_callback;
use method::{gen_async_method_callback, gen_method_callback};
use static_op::gen_static_callback;

pub(super) mod brand;
pub(super) mod constructor;
pub(super) mod getter;
pub(super) mod install;
pub(super) mod method;
pub(super) mod public_is;
pub(super) mod reentry_guard;
pub(super) mod slot_types;
pub(super) mod static_op;

/// Compose the full `#[v8_class]` expansion from a parsed-and-analysed
/// [`ClassConfig`] plus the marker-stripped impl block. Caller passes
/// `stripped_impl` because the impl-block tokens still need to be
/// emitted verbatim (sans macro-only attributes like `#[v8_method]`).
pub(super) fn assemble_tokens(cfg: &ClassConfig, stripped_impl: &ItemImpl) -> TokenStream2 {
    let class_ty = cfg.class_ty;

    // Per-class isolate-slot marker types + brand-check helpers +
    // public is-instance entry. Each lives in its own per-fragment
    // helper, ≤80 LOC, tested in isolation via the existing insta
    // snapshots.
    let slot_types = slot_types::gen_install_slot_types(cfg);
    let brand_helpers = brand::gen_brand_check_helpers(cfg);
    let public_is = public_is::gen_public_is_fn(cfg);

    // The install fn body — `pub fn install(scope) -> FunctionTemplate`.
    // It stays monolithic for now, but is parameterized by ClassConfig.
    let install = install::gen_install(cfg);

    // The `register(scope, global)` companion (#198). Bare template +
    // globalThis-bind under `class_ty.to_string()`. Lives in the same
    // impl block as `install` so a single macro list in `setup_globals`
    // can enumerate `<Class>::register` for every simple class.
    let register = install::gen_register(cfg);

    // Per-method callback fns. Async methods take a different codegen
    // path (spawn a future via `state.spawned_ops` and return a Promise
    // immediately) but install on the prototype identically — async vs
    // sync is opaque to V8. SameObject getters have their own codegen
    // path that wraps the user method with private-symbol caching.
    // Static methods / getters skip the brand check and internal-field
    // deref entirely (no receiver) and install on the constructor
    // template via `set_with_attr` / `set_accessor_property`.
    let regular = cfg.regular();
    let callbacks: Vec<TokenStream2> = regular
        .iter()
        .map(|m| {
            let callback = match m.kind {
                MethodKind::AsyncMethod => gen_async_method_callback(cfg, m),
                MethodKind::Getter if m.same_object => gen_same_object_getter_callback(cfg, m),
                MethodKind::StaticMethod | MethodKind::StaticGetter => gen_static_callback(cfg, m),
                _ => gen_method_callback(cfg, m),
            };
            with_method_cfg(m, callback)
        })
        .collect();

    // Fastcall shims — emitted alongside the slow-path FunctionCallback
    // for methods/getters annotated with `#[v8_method(fastcall)]` or
    // `#[v8_getter(fastcall)]`. The slow callback above is unchanged;
    // V8 chooses fast vs slow at JIT time based on receiver shape and
    // arg types.
    let fastcall_callbacks: Vec<TokenStream2> = regular
        .iter()
        .filter(|m| m.fastcall)
        .filter_map(|m| {
            gen_fastcall_callback(class_ty, cfg.state_ty, m).map(|cb| with_method_cfg(m, cb))
        })
        .collect();

    let constructor_callback = match cfg.constructor() {
        Some(c) => gen_constructor_callback(cfg, c),
        None => gen_default_constructor_callback(cfg),
    };

    let iterable_codegen = &cfg.iterable_codegen;

    quote! {
        #stripped_impl

        #slot_types

        #brand_helpers

        #public_is

        #[allow(non_snake_case, dead_code)]
        impl #class_ty {
            #install

            #register
        }

        #constructor_callback
        #(#callbacks)*

        // Fastcall shims emitted alongside the slow-path callbacks
        // for methods/getters annotated with `#[v8_method(fastcall)]` /
        // `#[v8_getter(fastcall)]`. Each entry is the `extern "C" fn`
        // shim + a `static CFunctionInfo` + a `static CFunction`. No-op
        // when no method on the class is fastcall.
        #(#fastcall_callbacks)*

        // Iterable codegen (when `#[v8_iterable(...)]` is set on the
        // impl block). Emits the companion `<Class>Iterator` struct +
        // its install fn, the four factory callbacks (keys, values,
        // entries, forEach), the iterator's `next()` callback, and a
        // `<Class>::__zs_install_iterable_methods` helper called from
        // `<Class>::install`. No-op when the attribute is absent.
        #iterable_codegen
    }
}

fn with_method_cfg(m: &ClassMethod<'_>, tokens: TokenStream2) -> TokenStream2 {
    let cfg_attrs = &m.cfg_attrs;
    quote! {
        #(#cfg_attrs)*
        #tokens
    }
}
