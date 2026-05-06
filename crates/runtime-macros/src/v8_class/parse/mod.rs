//! Attribute parsing and method classification for `#[v8_class]`.
//!
//! Current layout:
//! - [`marker_attr`] — `MarkerAttr` trait + `extract_marker_attr<T>`
//!   driver + per-attribute impls (closes F5 / H5-H7).
//! - This module — `MethodKind` classifier (`classify`),
//!   receiver/return-shape predicates, `resolve_state_and_marker`, and
//!   the [`extract_*`] wrappers preserved for the per-method emit
//!   helpers (`emit::method`, `emit::constructor`, `emit::getter`,
//!   `emit::static_op`) that walk method-attrs in isolation.
//!
//! The class-level single-scan parser ([`parse_attrs`]) lives below.
//! Earlier versions walked the impl-block attribute slice once per
//! extractor; now one walk builds [`ParsedAttrs`].

pub(crate) mod marker_attr;

use std::collections::HashSet;

use syn::{Attribute, FnArg, ImplItemFn, Receiver};

use super::{ConstDecl, MethodKind};

pub(super) use marker_attr::{
    extract_marker_attr, AsyncIterableAttr, CallableNoNewFlag, ConstDeclsAttr, FastcallFlag,
    PostInitAttr, RejectSharedAttr, SameObjectFlag, V8InheritBaseAttr, V8InheritIntrinsicAttr,
    V8NameAttr, V8StateMarkerAttr, V8ToStringTagAttr,
};

// ---------------------------------------------------------------------------
// Method classification
// ---------------------------------------------------------------------------

pub(super) fn classify(func: &ImplItemFn) -> Option<MethodKind> {
    for attr in &func.attrs {
        let path = attr.path();
        if path.is_ident("v8_method") {
            return Some(MethodKind::Method);
        }
        if path.is_ident("v8_async_method") {
            return Some(MethodKind::AsyncMethod);
        }
        if path.is_ident("v8_getter") {
            return Some(MethodKind::Getter);
        }
        if path.is_ident("v8_setter") {
            return Some(MethodKind::Setter);
        }
        if path.is_ident("v8_constructor") {
            return Some(MethodKind::Constructor);
        }
        if path.is_ident("v8_static_method") {
            return Some(MethodKind::StaticMethod);
        }
        if path.is_ident("v8_static_getter") {
            return Some(MethodKind::StaticGetter);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Per-method extracts (used by emit/{method,constructor,getter,static_op}.rs)
//
// These are thin wrappers over `extract_marker_attr<T>` that preserve
// the earlier surface for emit-side callers that walk method attrs
// in isolation. The class-level single-scan walk lives in
// [`parse_attrs`].
// ---------------------------------------------------------------------------

/// Read `#[v8_getter(same_object)]` from a method's attributes. Used
/// at analyse-time for the SameObject getter shape.
pub(super) fn extract_same_object(attrs: &[Attribute]) -> syn::Result<bool> {
    Ok(extract_marker_attr::<SameObjectFlag>(attrs)?.0)
}

/// Read `#[v8_method(fastcall)]` / `#[v8_getter(fastcall)]` from a
/// method's attributes.
pub(super) fn extract_fastcall(attrs: &[Attribute]) -> syn::Result<bool> {
    Ok(extract_marker_attr::<FastcallFlag>(attrs)?.0)
}

/// Read `#[v8_name = "literal"]` from a method's attributes. Strict —
/// malformed shape (`#[v8_name(foo)]`, `#[v8_name = 42]`) errors.
pub(super) fn extract_v8_name(attrs: &[Attribute]) -> syn::Result<Option<String>> {
    Ok(extract_marker_attr::<V8NameAttr>(attrs)?.0)
}

/// Read `#[reject_shared(a, b, c)]` from a method's attributes. Returns
/// the accumulated set of parameter names. Multiple occurrences are
/// merged.
pub(super) fn extract_reject_shared(attrs: &[Attribute]) -> syn::Result<HashSet<String>> {
    Ok(extract_marker_attr::<RejectSharedAttr>(attrs)?.0)
}

/// Read `#[v8_constructor(callable_no_new)]` from a method's attributes.
pub(super) fn extract_callable_no_new(attrs: &[Attribute]) -> syn::Result<bool> {
    Ok(extract_marker_attr::<CallableNoNewFlag>(attrs)?.0)
}

/// Read `#[v8_constructor(post_init = "fn_name")]` — strict on malformed
/// shape (e.g. `post_init = ident` or `post_init = "1bad"`).
pub(super) fn extract_post_init(attrs: &[Attribute]) -> syn::Result<Option<syn::Ident>> {
    Ok(extract_marker_attr::<PostInitAttr>(attrs)?.0)
}

// (extract_state_marker is unused at the wrapper layer — `parse_attrs`
// reads it via the single-scan walk. Keep the trait impl in
// `marker_attr.rs` available if a future wave needs the standalone
// extractor.)

// ---------------------------------------------------------------------------
// Single-scan class-level attribute parser
// ---------------------------------------------------------------------------

/// All impl-block-level attribute values for a `#[v8_class]` impl block,
/// extracted in a single walk.
///
/// Earlier versions walked the attribute slice once per extractor
/// (6+ walks). This struct carries the same data with one
/// walk.
#[derive(Default)]
pub(super) struct ParsedAttrs {
    pub to_string_tag: Option<String>,
    pub inherit_intrinsic: Option<String>,
    pub inherit_base: Option<syn::Path>,
    pub state_marker: Option<syn::Path>,
    pub async_iterable: Option<String>,
    pub consts: Vec<ConstDecl>,
}

/// Walk an impl block's attribute slice ONCE, dispatching each attr to
/// the matching MarkerAttr's merge fn. Closes F8.
///
/// Per-method attributes (`v8_method`, `v8_getter`, `v8_setter`, etc.)
/// are NOT consumed here — they live on impl items, not the impl block,
/// and are walked by the per-method emit helpers via the legacy
/// extract wrappers above.
///
/// `v8_iterable` is also NOT consumed here — it's owned by the
/// `v8_iterable` module which has its own `extract_iterable`
/// (different attribute shape with `mode = ...` flag).
pub(super) fn parse_attrs(attrs: &[Attribute]) -> syn::Result<ParsedAttrs> {
    let mut out = ParsedAttrs::default();
    // Single accumulator per attribute kind — fed by ONE pass through
    // `attrs`. The driver's `extract_marker_attr<T>` would walk per
    // type; we inline the dispatch here so the walk is genuinely O(N).
    let mut to_string_tag = V8ToStringTagAttr::default();
    let mut inherit_intrinsic = V8InheritIntrinsicAttr::default();
    let mut inherit_base = V8InheritBaseAttr::default();
    let mut state_marker = V8StateMarkerAttr::default();
    let mut async_iterable = AsyncIterableAttr::default();
    let mut consts = ConstDeclsAttr::default();

    for attr in attrs {
        let path = attr.path();
        // Each `if` covers one attribute kind. The `?` propagates the
        // first malformed-shape error with the attr's span attached.
        if path.is_ident(<V8ToStringTagAttr as marker_attr::MarkerAttr>::NAMES[0]) {
            <V8ToStringTagAttr as marker_attr::MarkerAttr>::merge(&mut to_string_tag, attr)?;
            continue;
        }
        if path.is_ident(<V8InheritIntrinsicAttr as marker_attr::MarkerAttr>::NAMES[0]) {
            <V8InheritIntrinsicAttr as marker_attr::MarkerAttr>::merge(
                &mut inherit_intrinsic,
                attr,
            )?;
            continue;
        }
        if path.is_ident(<V8InheritBaseAttr as marker_attr::MarkerAttr>::NAMES[0]) {
            <V8InheritBaseAttr as marker_attr::MarkerAttr>::merge(&mut inherit_base, attr)?;
            continue;
        }
        if path.is_ident(<V8StateMarkerAttr as marker_attr::MarkerAttr>::NAMES[0]) {
            <V8StateMarkerAttr as marker_attr::MarkerAttr>::merge(&mut state_marker, attr)?;
            continue;
        }
        if path.is_ident(<AsyncIterableAttr as marker_attr::MarkerAttr>::NAMES[0]) {
            <AsyncIterableAttr as marker_attr::MarkerAttr>::merge(&mut async_iterable, attr)?;
            continue;
        }
        if path.is_ident(<ConstDeclsAttr as marker_attr::MarkerAttr>::NAMES[0]) {
            <ConstDeclsAttr as marker_attr::MarkerAttr>::merge(&mut consts, attr)?;
            continue;
        }
        // Unknown impl-block-level attributes pass through to rustc.
    }

    out.to_string_tag = to_string_tag.0;
    out.inherit_intrinsic = inherit_intrinsic.0;
    out.inherit_base = inherit_base.0;
    out.state_marker = state_marker.0;
    out.async_iterable = async_iterable.0;
    out.consts = consts.0;

    Ok(out)
}

// ---------------------------------------------------------------------------
// State / marker resolution
// ---------------------------------------------------------------------------

/// Resolve the `(state_ty, marker_ty)` pair for codegen.
///
/// - With no `#[v8_state_marker]` attribute: both are the impl
///   receiver's bare ident — byte-identical emission to today.
/// - With `#[v8_state_marker(M)] impl S`: `state_ty = S`,
///   `marker_ty = M`.
///
/// Errors (returned as a compile-error TokenStream the caller forwards
/// directly into the proc-macro output):
///  - The marker isn't a bare identifier — generics or path-qualified
///    forms are forbidden in v1 (design §4.2).
///  - The marker matches the impl receiver — that's the no-attribute
///    path, and the v1 strict policy rejects it (design §4.7).
pub(super) fn resolve_state_and_marker<'a>(
    receiver_ty: &'a syn::Ident,
    marker: Option<&syn::Path>,
) -> Result<(&'a syn::Ident, syn::Ident), proc_macro2::TokenStream> {
    let Some(path) = marker else {
        return Ok((receiver_ty, receiver_ty.clone()));
    };
    let m_ident = match path.get_ident().cloned() {
        Some(id) => id,
        None => {
            return Err(syn::Error::new_spanned(
                path,
                "#[v8_state_marker]: marker must be a bare type identifier \
                 (no generics, no paths) — bring the type into scope with \
                 a `use` statement above the impl block if it lives elsewhere",
            )
            .to_compile_error());
        }
    };
    if &m_ident == receiver_ty {
        return Err(syn::Error::new_spanned(
            path,
            "#[v8_state_marker]: marker type matches impl receiver — \
             remove the attribute (use #[v8_class] alone for the no-op path)",
        )
        .to_compile_error());
    }
    Ok((receiver_ty, m_ident))
}

// ---------------------------------------------------------------------------
// Receiver-shape and return-shape predicates
// ---------------------------------------------------------------------------

pub(super) fn has_mut_self(func: &ImplItemFn) -> bool {
    func.sig.inputs.iter().any(|arg| {
        matches!(
            arg,
            FnArg::Receiver(Receiver {
                mutability: Some(_),
                reference: Some(_),
                ..
            })
        )
    })
}

/// True if the function has ANY receiver (`self`, `&self`, `&mut self`).
/// Used to reject static methods that accidentally took a `self` arg.
pub(super) fn has_any_receiver(func: &ImplItemFn) -> bool {
    func.sig
        .inputs
        .iter()
        .any(|arg| matches!(arg, FnArg::Receiver(_)))
}

/// True if the function's return type is the unit `()` — either the
/// implicit `ReturnType::Default` (no `->` clause) or an explicit
/// `-> ()` written by the user.
pub(super) fn is_unit_return(output: &syn::ReturnType) -> bool {
    match output {
        syn::ReturnType::Default => true,
        syn::ReturnType::Type(_, ty) => matches!(
            ty.as_ref(),
            syn::Type::Tuple(t) if t.elems.is_empty()
        ),
    }
}

/// True if the function's return type is `Result<(), _>`.
pub(super) fn is_result_unit_return(output: &syn::ReturnType) -> bool {
    let syn::ReturnType::Type(_, ty) = output else {
        return false;
    };
    let syn::Type::Path(p) = ty.as_ref() else {
        return false;
    };
    let Some(last) = p.path.segments.last() else {
        return false;
    };
    if last.ident != "Result" {
        return false;
    }
    let syn::PathArguments::AngleBracketed(args) = &last.arguments else {
        return false;
    };
    let Some(syn::GenericArgument::Type(ok_ty)) = args.args.first() else {
        return false;
    };
    matches!(ok_ty, syn::Type::Tuple(t) if t.elems.is_empty())
}
