//! `MarkerAttr` trait + driver — closes F5 / H5-H7 (design
//! `docs/proposals/runtime-macros-refactor.md` §3.2).
//!
//! Pre-Wave-4, `parse.rs` had 12 `extract_*` helpers each with subtly
//! different return shapes (`bool` / `Option<T>` / `Result<Option<T>>` /
//! `HashSet<T>`) and inconsistent error policy (some silently fell back
//! to None on malformed shape — H5; some emitted `compile_error!`). This
//! module unifies all of them under a single trait + a single driver.
//!
//! ## Strict-by-default
//!
//! Per design §3.2 + critique H5, all marker attributes now emit
//! `compile_error!` on malformed shape. Previously-silent fallbacks
//! (`#[v8_name(foo)]` with no `=`, `#[v8_to_string_tag = 42]` with a
//! non-string literal) become hard errors. Compile-fail fixtures live in
//! `crates/runtime/tests/compile_fail_marker_attr/`.
//!
//! ## Repeatable vs. at-most-one
//!
//! Each impl encodes its own merge policy via `MarkerAttr::merge`. The
//! at-most-one shapes (V8Name, V8ToStringTag, V8InheritIntrinsic,
//! V8InheritBase, V8StateMarker, AsyncIterable, PostInit) error on
//! duplicate. The accumulating shapes (RejectShared HashSet,
//! ConstDecls Vec) extend the inner collection. Flag shapes (SameObject,
//! Fastcall, CallableNoNew) OR a bool.
//!
//! ## Two-name attrs
//!
//! `FastcallFlag` reads from BOTH `#[v8_method(...)]` AND
//! `#[v8_getter(...)]` (the fastcall flag is method-or-getter scoped),
//! so the trait carries `NAMES: &'static [&'static str]` rather than a
//! single name. Most impls have NAMES = `&["..."]` (1 element).

use std::collections::HashSet;

use syn::{Attribute, Expr, ExprLit, Lit, Meta};

use super::super::{ConstDecl, ConstKind};

/// A single marker attribute on an impl item or impl block.
///
/// Each `#[v8_*]` attribute the macro recognises implements this. The
/// `merge` method is called once per matching attribute by
/// [`extract_marker_attr`]; impls choose whether to error on duplicate
/// (at-most-one shapes) or accumulate (HashSet / Vec shapes).
///
/// ## Implementation contract
///
/// - `NAMES`: the attribute path identifiers this impl recognises.
///   Most impls match exactly one path; `FastcallFlag` matches two
///   (`v8_method` and `v8_getter`).
/// - `merge`: called per attribute whose `path().is_ident(name)` matches
///   any element of `NAMES`. The driver does NOT pre-filter beyond the
///   path match — `merge` is responsible for the full structure check
///   (NameValue vs. List form, expected literal types, etc.) and emits
///   `syn::Error::new_spanned(...)` on malformed input.
pub(crate) trait MarkerAttr: Sized + Default {
    /// Attribute path identifiers this impl recognises (e.g.
    /// `&["v8_name"]`, `&["v8_method", "v8_getter"]`).
    const NAMES: &'static [&'static str];

    /// Merge one matching attribute into `self`. Errors on malformed
    /// shape OR (for at-most-one impls) on duplicate occurrence.
    fn merge(&mut self, attr: &Attribute) -> syn::Result<()>;
}

/// Drive a [`MarkerAttr`] impl across an attribute slice. Walks once,
/// calls `merge` per matching attribute, returns the accumulated value.
///
/// Returns `Err` if any matching attribute is malformed OR if an
/// at-most-one impl saw a duplicate.
pub(crate) fn extract_marker_attr<T: MarkerAttr>(attrs: &[Attribute]) -> syn::Result<T> {
    let mut acc = T::default();
    for attr in attrs {
        let path = attr.path();
        let matched = T::NAMES.iter().any(|name| path.is_ident(name));
        if !matched {
            continue;
        }
        acc.merge(attr)?;
    }
    Ok(acc)
}

// ===========================================================================
// Shape helpers
// ===========================================================================

/// Read a `name = "string-literal"` shape's literal-string value.
/// Returns `Err` with a span pointing at the offending tokens on:
///   - non-NameValue meta (e.g. list form)
///   - non-string literal value
fn require_str_value(attr: &Attribute, attr_name: &str) -> syn::Result<String> {
    let nv = match &attr.meta {
        Meta::NameValue(nv) => nv,
        other => {
            return Err(syn::Error::new_spanned(
                other,
                format!("#[{attr_name} = \"...\"]: expected `name = literal` shape"),
            ));
        }
    };
    let s = match &nv.value {
        Expr::Lit(ExprLit {
            lit: Lit::Str(s), ..
        }) => s,
        other => {
            return Err(syn::Error::new_spanned(
                other,
                format!("#[{attr_name} = \"...\"]: expected a string literal"),
            ));
        }
    };
    Ok(s.value())
}

/// Read a list-form attribute as a comma-separated list of bare
/// identifiers (e.g. `#[v8_getter(same_object)]` → `["same_object"]`).
/// Returns `Err` on parse failure (e.g. `#[v8_getter(123)]`).
fn parse_ident_list(attr: &Attribute, attr_name: &str) -> syn::Result<Vec<syn::Ident>> {
    attr.parse_args_with(|input: syn::parse::ParseStream| {
        let mut acc: Vec<syn::Ident> = Vec::new();
        while !input.is_empty() {
            let id: syn::Ident = input.parse().map_err(|e| {
                syn::Error::new(
                    e.span(),
                    format!("#[{attr_name}(...)]: expected a bare identifier"),
                )
            })?;
            acc.push(id);
            if input.is_empty() {
                break;
            }
            let _: syn::Token![,] = input.parse()?;
        }
        Ok(acc)
    })
}

// ===========================================================================
// At-most-one string-valued attributes
// ===========================================================================

/// `#[v8_name = "literal"]` on a method.
///
/// Strict-by-default per H5. `#[v8_name(foo)]` (list form, no `=`) and
/// `#[v8_name = 42]` (non-string literal) now error rather than silently
/// fall back to None.
#[derive(Default)]
pub(crate) struct V8NameAttr(pub Option<String>);

impl MarkerAttr for V8NameAttr {
    const NAMES: &'static [&'static str] = &["v8_name"];
    fn merge(&mut self, attr: &Attribute) -> syn::Result<()> {
        if self.0.is_some() {
            return Err(syn::Error::new_spanned(
                attr,
                "#[v8_name]: duplicate attribute (only one #[v8_name = \"...\"] per method)",
            ));
        }
        self.0 = Some(require_str_value(attr, "v8_name")?);
        Ok(())
    }
}

/// `#[v8_to_string_tag = "literal"]` on the impl block.
#[derive(Default)]
pub(crate) struct V8ToStringTagAttr(pub Option<String>);

impl MarkerAttr for V8ToStringTagAttr {
    const NAMES: &'static [&'static str] = &["v8_to_string_tag"];
    fn merge(&mut self, attr: &Attribute) -> syn::Result<()> {
        if self.0.is_some() {
            return Err(syn::Error::new_spanned(
                attr,
                "#[v8_to_string_tag]: duplicate attribute",
            ));
        }
        self.0 = Some(require_str_value(attr, "v8_to_string_tag")?);
        Ok(())
    }
}

/// `#[v8_inherit_intrinsic = "literal"]` on the impl block.
#[derive(Default)]
pub(crate) struct V8InheritIntrinsicAttr(pub Option<String>);

impl MarkerAttr for V8InheritIntrinsicAttr {
    const NAMES: &'static [&'static str] = &["v8_inherit_intrinsic"];
    fn merge(&mut self, attr: &Attribute) -> syn::Result<()> {
        if self.0.is_some() {
            return Err(syn::Error::new_spanned(
                attr,
                "#[v8_inherit_intrinsic]: duplicate attribute",
            ));
        }
        self.0 = Some(require_str_value(attr, "v8_inherit_intrinsic")?);
        Ok(())
    }
}

// ===========================================================================
// At-most-one path-valued attributes
// ===========================================================================

/// `#[v8_inherit(BasePath)]` on the impl block.
#[derive(Default)]
pub(crate) struct V8InheritBaseAttr(pub Option<syn::Path>);

impl MarkerAttr for V8InheritBaseAttr {
    const NAMES: &'static [&'static str] = &["v8_inherit"];
    fn merge(&mut self, attr: &Attribute) -> syn::Result<()> {
        if self.0.is_some() {
            return Err(syn::Error::new_spanned(
                attr,
                "#[v8_inherit]: duplicate attribute (only one base class supported)",
            ));
        }
        let path: syn::Path = attr.parse_args().map_err(|e| {
            syn::Error::new(
                e.span(),
                "#[v8_inherit(BaseClass)]: expected a type path (e.g. `EventTarget` or \
                 `super::event_target::EventTarget`)",
            )
        })?;
        self.0 = Some(path);
        Ok(())
    }
}

/// `#[v8_state_marker(MarkerTy)]` on the impl block.
#[derive(Default)]
pub(crate) struct V8StateMarkerAttr(pub Option<syn::Path>);

impl MarkerAttr for V8StateMarkerAttr {
    const NAMES: &'static [&'static str] = &["v8_state_marker"];
    fn merge(&mut self, attr: &Attribute) -> syn::Result<()> {
        if self.0.is_some() {
            return Err(syn::Error::new_spanned(
                attr,
                "#[v8_state_marker]: duplicate attribute",
            ));
        }
        let path: syn::Path = attr.parse_args().map_err(|e| {
            syn::Error::new(
                e.span(),
                "#[v8_state_marker(M)]: expected a type identifier",
            )
        })?;
        self.0 = Some(path);
        Ok(())
    }
}

// ===========================================================================
// At-most-one nested-meta attributes (constructor flags)
// ===========================================================================

/// `#[v8_constructor(callable_no_new)]` flag.
///
/// The `#[v8_constructor]` attribute can carry both `callable_no_new`
/// AND `post_init = "..."` in the same list (`PostInitAttr` reads the
/// same attribute). This impl extracts only the `callable_no_new` half.
#[derive(Default)]
pub(crate) struct CallableNoNewFlag(pub bool);

impl MarkerAttr for CallableNoNewFlag {
    const NAMES: &'static [&'static str] = &["v8_constructor"];
    fn merge(&mut self, attr: &Attribute) -> syn::Result<()> {
        // Bare `#[v8_constructor]` (no list) — consistent with prior
        // behaviour, treat as the "no flags" path. parse_args returns
        // Err in that case; we treat that as "no flags set".
        let Ok(metas) = attr.parse_args_with(
            syn::punctuated::Punctuated::<Meta, syn::Token![,]>::parse_terminated,
        ) else {
            return Ok(());
        };
        for m in metas {
            if let Meta::Path(p) = m {
                if p.is_ident("callable_no_new") {
                    self.0 = true;
                }
            }
        }
        Ok(())
    }
}

/// `#[v8_constructor(post_init = "fn_name")]`. Strict by design — silent
/// no-op on malformed values would be a debugging nightmare given
/// post_init is semantically load-bearing (design §5.7).
#[derive(Default)]
pub(crate) struct PostInitAttr(pub Option<syn::Ident>);

impl MarkerAttr for PostInitAttr {
    const NAMES: &'static [&'static str] = &["v8_constructor"];
    fn merge(&mut self, attr: &Attribute) -> syn::Result<()> {
        let Ok(metas) = attr.parse_args_with(
            syn::punctuated::Punctuated::<Meta, syn::Token![,]>::parse_terminated,
        ) else {
            // Bare `#[v8_constructor]` — no post_init. Return Ok so other
            // markers parsing the same attr (CallableNoNewFlag) get to
            // run.
            return Ok(());
        };
        for meta in metas {
            let Meta::NameValue(nv) = meta else { continue };
            if !nv.path.is_ident("post_init") {
                continue;
            }
            if self.0.is_some() {
                return Err(syn::Error::new_spanned(
                    &nv.path,
                    "#[v8_constructor]: duplicate `post_init = \"...\"`",
                ));
            }
            let lit = match &nv.value {
                Expr::Lit(ExprLit {
                    lit: Lit::Str(s), ..
                }) => s,
                other => {
                    return Err(syn::Error::new_spanned(
                        other,
                        "#[v8_constructor]: post_init must be a string literal naming a function on this impl, e.g. post_init = \"after_install\"",
                    ));
                }
            };
            let raw = lit.value();
            let ident = syn::parse_str::<syn::Ident>(&raw).map_err(|_| {
                syn::Error::new_spanned(
                    lit,
                    format!("#[v8_constructor]: post_init = {raw:?} is not a valid Rust identifier"),
                )
            })?;
            self.0 = Some(ident);
        }
        Ok(())
    }
}

// ===========================================================================
// Method-list-form flags
// ===========================================================================

/// `#[v8_getter(same_object)]` flag.
#[derive(Default)]
pub(crate) struct SameObjectFlag(pub bool);

impl MarkerAttr for SameObjectFlag {
    const NAMES: &'static [&'static str] = &["v8_getter"];
    fn merge(&mut self, attr: &Attribute) -> syn::Result<()> {
        // Bare `#[v8_getter]` (no list) — `parse_args_with` returns Err.
        // Treat that as "flag absent" rather than propagate the error;
        // a bare getter is the default no-caching path.
        let Ok(idents) = parse_ident_list(attr, "v8_getter") else {
            return Ok(());
        };
        for id in idents {
            if id == "same_object" {
                self.0 = true;
            }
        }
        Ok(())
    }
}

/// `#[v8_method(fastcall)]` / `#[v8_getter(fastcall)]` flag. Reads
/// BOTH attribute names — the fastcall flag is method-or-getter scoped.
#[derive(Default)]
pub(crate) struct FastcallFlag(pub bool);

impl MarkerAttr for FastcallFlag {
    const NAMES: &'static [&'static str] = &["v8_method", "v8_getter"];
    fn merge(&mut self, attr: &Attribute) -> syn::Result<()> {
        // Bare `#[v8_method]` / `#[v8_getter]` (no list) — treat absent.
        let attr_name = if attr.path().is_ident("v8_method") {
            "v8_method"
        } else {
            "v8_getter"
        };
        let Ok(idents) = parse_ident_list(attr, attr_name) else {
            return Ok(());
        };
        for id in idents {
            if id == "fastcall" {
                self.0 = true;
            }
        }
        Ok(())
    }
}

// ===========================================================================
// Repeatable / accumulating attributes
// ===========================================================================

/// `#[reject_shared(arg1, arg2, ...)]` — accumulates parameter names
/// across multiple occurrences.
#[derive(Default)]
pub(crate) struct RejectSharedAttr(pub HashSet<String>);

impl MarkerAttr for RejectSharedAttr {
    const NAMES: &'static [&'static str] = &["reject_shared"];
    fn merge(&mut self, attr: &Attribute) -> syn::Result<()> {
        let idents = parse_ident_list(attr, "reject_shared")?;
        for id in idents {
            self.0.insert(id.to_string());
        }
        Ok(())
    }
}

/// `#[v8_async_iterable(method = "name")]`.
#[derive(Default)]
pub(crate) struct AsyncIterableAttr(pub Option<String>);

impl MarkerAttr for AsyncIterableAttr {
    const NAMES: &'static [&'static str] = &["v8_async_iterable"];
    fn merge(&mut self, attr: &Attribute) -> syn::Result<()> {
        if self.0.is_some() {
            return Err(syn::Error::new_spanned(
                attr,
                "#[v8_async_iterable]: duplicate attribute",
            ));
        }
        let mut method: Option<String> = None;
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("method") {
                let value = meta.value()?;
                if let Ok(s) = value.parse::<syn::LitStr>() {
                    method = Some(s.value());
                } else {
                    let id: syn::Ident = value.parse()?;
                    method = Some(id.to_string());
                }
                Ok(())
            } else {
                Err(meta.error("expected `method = \"name\"`"))
            }
        })?;
        let m = method.ok_or_else(|| {
            syn::Error::new_spanned(
                attr,
                "#[v8_async_iterable]: missing `method = \"name\"` (e.g. `method = \"values\"`)",
            )
        })?;
        self.0 = Some(m);
        Ok(())
    }
}

/// `#[v8_const(NAME = LIT)]` declarations — accumulates across
/// multiple occurrences, errors on duplicate name.
#[derive(Default)]
pub(crate) struct ConstDeclsAttr(pub Vec<ConstDecl>);

impl MarkerAttr for ConstDeclsAttr {
    const NAMES: &'static [&'static str] = &["v8_const"];
    fn merge(&mut self, attr: &Attribute) -> syn::Result<()> {
        let parsed = attr.parse_args_with(
            |input: syn::parse::ParseStream| -> syn::Result<(syn::Ident, syn::ExprLit)> {
                let name: syn::Ident = input.parse()?;
                let _: syn::Token![=] = input.parse()?;
                let expr: syn::Expr = input.parse()?;
                let lit = match expr {
                    syn::Expr::Lit(lit) => lit,
                    other => {
                        return Err(syn::Error::new_spanned(
                            other,
                            "#[v8_const]: expected an integer literal (e.g. `12u16`, `100i32`)",
                        ));
                    }
                };
                Ok((name, lit))
            },
        )?;
        let (name, lit) = parsed;
        let name_str = name.to_string();
        if self.0.iter().any(|d| d.name == name) {
            return Err(syn::Error::new_spanned(
                &name,
                format!("#[v8_const]: duplicate constant `{name_str}`"),
            ));
        }
        let int = match &lit.lit {
            syn::Lit::Int(i) => i,
            other => {
                return Err(syn::Error::new_spanned(
                    other,
                    "#[v8_const]: expected an integer literal with a type suffix \
                     (e.g. `12u16`, `100i32`, `1000u32`)",
                ));
            }
        };
        let kind = match int.suffix() {
            "u16" | "u32" => ConstKind::UInt,
            "i32" => ConstKind::SInt,
            "" => {
                return Err(syn::Error::new_spanned(
                    int,
                    "#[v8_const]: literal needs a type suffix \
                     (e.g. `12u16`, `100i32`, `1000u32`); unsuffixed literals \
                     are ambiguous and rejected",
                ));
            }
            other => {
                return Err(syn::Error::new_spanned(
                    int,
                    format!(
                        "#[v8_const]: unsupported literal suffix `{other}` \
                         (expected `u16`, `u32`, or `i32`)"
                    ),
                ));
            }
        };
        self.0.push(ConstDecl {
            name,
            value: lit,
            kind,
        });
        Ok(())
    }
}
