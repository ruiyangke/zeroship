//! `ClassConfig` — the parameter object passed to every codegen helper
//! after parse + analyse phases (design
//! `docs/proposals/runtime-macros-refactor.md` §3.1, Appendix B).
//!
//! Closes F4 (the 10-arg `gen_install` signature) and the cascade of
//! `(class_ty, state_ty, ...)` repetitions across every codegen helper.
//!
//! Adding a new impl-block-level attribute now means one new field here
//! plus one new emit helper that reads it, never a parameter-list
//! migration through the call graph.

use std::collections::HashMap;

use quote::format_ident;

use super::super::{ClassMethod, ConstDecl, MethodKind};

/// Aggregate of everything the emit phase needs to render a single
/// `#[v8_class]`-annotated impl block.
///
/// Lifetime `'a` borrows from the parsed `syn::ItemImpl` whose attrs +
/// items the parse phase walked. Owned fields (`marker_ty`,
/// `to_string_tag`, …) hold values resolved during analysis from
/// attrs + defaults; borrowed fields (`class_ty`, `state_ty`, the
/// per-method records inside `methods`) point back into the input AST.
///
/// Construction goes through [`ClassConfig::new`]; helpers iterate via
/// the convenience accessors below.
pub(crate) struct ClassConfig<'a> {
    // ---- Identity ---------------------------------------------------
    /// The JS-class identity ident — drives install slot, brand check,
    /// callback names, the install fn's enclosing impl, and the
    /// `set_class_name` literal. Under `#[v8_state_marker(M)]` this is
    /// `M`; without the marker, this is the impl receiver type ident.
    pub class_ty: &'a syn::Ident,
    /// The boxed payload type — what V8 internal field 0 holds via
    /// External, what the constructor returns, and what per-method
    /// `&[mut] self` resolves to. Without `#[v8_state_marker]` this is
    /// equal to `class_ty`.
    pub state_ty: &'a syn::Ident,

    // ---- Methods ----------------------------------------------------
    /// All classified methods (constructors, instance methods, getters,
    /// setters, async methods, statics) in source order. `extract_*`
    /// guards have already filtered duplicate JS-visible names.
    pub methods: Vec<ClassMethod<'a>>,
    /// Whether at least one method is `#[v8_method(fastcall)]` /
    /// `#[v8_getter(fastcall)]`. Set on the install path's
    /// internal-field count (1 → 2 slots), threaded into
    /// `gen_box_and_install_finalizer`'s aligned-pointer install, and
    /// switches install's per-method emit between `FunctionTemplate::new`
    /// and `FunctionTemplate::builder().build_fast()`.
    pub has_any_fastcall: bool,

    // ---- Class-wide attributes -------------------------------------
    /// Override for `Symbol.toStringTag`'s value. Defaults to
    /// `class_ty.to_string()` when `None`.
    pub to_string_tag: Option<String>,
    /// `#[v8_inherit_intrinsic = "..."]` — name of the V8 built-in
    /// intrinsic to chain. Currently only `"IteratorPrototype"` is
    /// recognised; other values produce a `compile_error!` in the
    /// emit.
    pub inherit_intrinsic: Option<String>,
    /// `#[v8_inherit(BasePath)]` — parent class whose install template
    /// the derived class chains via `FunctionTemplate::inherit`.
    pub inherit_base: Option<syn::Path>,
    /// `#[v8_async_iterable(method = "name")]` — JS-visible method
    /// name to alias under `[Symbol.asyncIterator]`.
    pub async_iterable_method: Option<String>,
    /// `#[v8_const(NAME = LIT)]` declarations.
    pub consts: Vec<ConstDecl>,

    // ---- Iterable codegen (driven by v8_iterable.rs) ----------------
    /// If `#[v8_iterable]` is present, the pre-rendered iterable
    /// codegen tokens (companion class + factory + forEach + next +
    /// install hook). The emit phase splices them at the end of the
    /// expansion's top-level token assembly.
    pub iterable_codegen: proc_macro2::TokenStream,
    /// If `#[v8_iterable]` is present, the install-time call site
    /// `<Class>::__zs_install_iterable_methods(scope, __proto)` to
    /// splice into the install fn body. `None` when absent.
    pub install_iterable_call: Option<proc_macro2::TokenStream>,

    // ---- Cached idents (Wave 9 N1) ---------------------------------
    /// Pre-computed `__brand_check_<Class>` ident. The format-ident
    /// pattern is invoked at 6 emit sites (brand-check helper,
    /// public_is, gen_recover_box, gen_async_method_callback,
    /// gen_same_object_getter_callback, v8_iterable's parent brand
    /// pass-through). Caching here means the `format_ident!` runs
    /// once at config-build time; emit sites read the cached value.
    /// Cosmetic/performance refinement — same ident, fewer
    /// allocations. Closes N1 from
    /// docs/reviews/runtime-macros-architecture-critique-2026-05-05-v2.md.
    pub brand_check_ident: syn::Ident,
}

impl<'a> ClassConfig<'a> {
    /// Construct a ClassConfig from already-parsed phase outputs.
    /// Intentionally NO attribute parsing happens here — that lives in
    /// `expand_tokens`'s parse pass; this constructor is a pure shape
    /// transform.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        class_ty: &'a syn::Ident,
        state_ty: &'a syn::Ident,
        methods: Vec<ClassMethod<'a>>,
        has_any_fastcall: bool,
        to_string_tag: Option<String>,
        inherit_intrinsic: Option<String>,
        inherit_base: Option<syn::Path>,
        async_iterable_method: Option<String>,
        consts: Vec<ConstDecl>,
        iterable_codegen: proc_macro2::TokenStream,
        install_iterable_call: Option<proc_macro2::TokenStream>,
    ) -> Self {
        let brand_check_ident = format_ident!("__brand_check_{}", class_ty);
        Self {
            class_ty,
            state_ty,
            methods,
            has_any_fastcall,
            to_string_tag,
            inherit_intrinsic,
            inherit_base,
            async_iterable_method,
            consts,
            iterable_codegen,
            install_iterable_call,
            brand_check_ident,
        }
    }

    /// The class's user-defined constructor, if any. Used by the emit
    /// phase to switch between `gen_constructor_callback` (user
    /// constructor present) and `gen_default_constructor_callback`
    /// (fall back to `<State as Default>::default()`).
    pub(crate) fn constructor(&self) -> Option<&ClassMethod<'a>> {
        self.methods.iter().find(|m| m.kind == MethodKind::Constructor)
    }

    /// Methods that install on the prototype / constructor template
    /// (everything except the constructor itself). Returned as a Vec
    /// of references because callers iterate it multiple times in a
    /// single `gen_install` body.
    pub(crate) fn regular(&self) -> Vec<&ClassMethod<'a>> {
        self.methods
            .iter()
            .filter(|m| m.kind != MethodKind::Constructor)
            .collect()
    }

    /// Index `(JS-name → fastcall ClassMethod)` for the install
    /// codegen's pair lookup. Methods always pair under their own
    /// name; getters pair by JS-name with their setter sibling, but
    /// setters never have fastcall (rejected at extract for void
    /// returns). So per-method-name fastcall tracking is sufficient.
    pub(crate) fn fastcall_by_jsname(&self) -> HashMap<String, &ClassMethod<'a>> {
        let mut out: HashMap<String, &ClassMethod<'a>> = HashMap::new();
        for m in &self.methods {
            if m.kind != MethodKind::Constructor && m.fastcall {
                out.insert(m.js_name.clone(), m);
            }
        }
        out
    }
}
