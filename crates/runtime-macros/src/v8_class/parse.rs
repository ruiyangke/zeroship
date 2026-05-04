//! Attribute parsing and method classification for `#[v8_class]`.
//!
//! Hosts:
//! - `MethodKind` classifier (`classify`) and `ClassMethod` (in `mod.rs`).
//! - The full set of `extract_*` helpers that read the WebIDL/v8-class
//!   marker attributes off impl-items and the impl-block itself.
//! - `has_mut_self` / `has_any_receiver` receiver-shape predicates.

use std::collections::HashSet;
use syn::{Attribute, Expr, ExprLit, FnArg, ImplItemFn, Lit, Meta, Receiver};

use super::{ConstDecl, ConstKind, MethodKind};

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

/// Read `#[v8_getter(same_object)]` from a method's attributes.
/// Returns true if the bare-identifier `same_object` appears in the
/// list form. Used to opt the getter into WebIDL `[SameObject]`
/// caching semantics — see `gen_same_object_getter_callback`.
///
/// The list form is `#[v8_getter(same_object)]`. `#[v8_getter]`
/// (no list) is the default, no caching.
pub(super) fn extract_same_object(attrs: &[Attribute]) -> bool {
    for attr in attrs {
        if !attr.path().is_ident("v8_getter") {
            continue;
        }
        if let Ok(idents) = attr.parse_args_with(|input: syn::parse::ParseStream| {
            let mut acc: Vec<syn::Ident> = Vec::new();
            while !input.is_empty() {
                let id: syn::Ident = input.parse()?;
                acc.push(id);
                if input.is_empty() {
                    break;
                }
                let _: syn::Token![,] = input.parse()?;
            }
            Ok(acc)
        }) {
            for id in idents {
                if id == "same_object" {
                    return true;
                }
            }
        }
    }
    false
}

/// Read `#[v8_method(fastcall)]` or `#[v8_getter(fastcall)]` from a
/// method's attributes. Returns true if the bare-identifier `fastcall`
/// appears in the list form on a `v8_method` or `v8_getter` attribute.
///
/// V8's fast API path lets Turbofan inline a typed CFunction call shim
/// at hot sites, skipping the full FunctionCallback prologue
/// (~10–30 ns per call). The macro's contract is "opt-in per-method,
/// preserves the slow path verbatim, falls back automatically when V8
/// can't take the fast path" (e.g., when the receiver's hidden class
/// hasn't been seen by the inline cache yet, or when arg shapes don't
/// match the typed signature like multibyte strings for SeqOneByteString).
///
/// The fast path imposes hard restrictions on the method's signature
/// (see `validate_fastcall_signature` in `fastcall.rs`): primitives only,
/// no allocation, no `&mut self`. We enforce these at compile time so
/// users get a clear error rather than runtime UB.
pub(super) fn extract_fastcall(attrs: &[Attribute]) -> bool {
    for attr in attrs {
        let p = attr.path();
        if !(p.is_ident("v8_method") || p.is_ident("v8_getter")) {
            continue;
        }
        if let Ok(idents) = attr.parse_args_with(|input: syn::parse::ParseStream| {
            let mut acc: Vec<syn::Ident> = Vec::new();
            while !input.is_empty() {
                let id: syn::Ident = input.parse()?;
                acc.push(id);
                if input.is_empty() {
                    break;
                }
                let _: syn::Token![,] = input.parse()?;
            }
            Ok(acc)
        }) {
            for id in idents {
                if id == "fastcall" {
                    return true;
                }
            }
        }
    }
    false
}

/// Read `#[v8_name = "literal"]` from a method's attributes. Returns
/// `Some(name)` if present, `None` otherwise. Invalid shapes (non-string
/// literal, list form, etc.) silently fall back to None — the macro
/// then uses the Rust identifier as the JS name.
pub(super) fn extract_v8_name(attrs: &[Attribute]) -> Option<String> {
    for attr in attrs {
        if !attr.path().is_ident("v8_name") {
            continue;
        }
        if let Meta::NameValue(nv) = &attr.meta {
            if let Expr::Lit(ExprLit {
                lit: Lit::Str(s), ..
            }) = &nv.value
            {
                return Some(s.value());
            }
        }
    }
    None
}

/// Read `#[v8_to_string_tag = "literal"]` from impl-block attributes
/// (the `#[…]` placed directly above the `impl` block).
pub(super) fn extract_to_string_tag(attrs: &[Attribute]) -> Option<String> {
    for attr in attrs {
        if !attr.path().is_ident("v8_to_string_tag") {
            continue;
        }
        if let Meta::NameValue(nv) = &attr.meta {
            if let Expr::Lit(ExprLit {
                lit: Lit::Str(s), ..
            }) = &nv.value
            {
                return Some(s.value());
            }
        }
    }
    None
}

/// Read `#[reject_shared(arg1, arg2, ...)]` from a method's attributes.
/// Returns the set of parameter names that should reject SharedArrayBuffer-
/// backed views. Empty set if the attribute is absent or malformed.
///
/// The list-form `#[reject_shared(name)]` is a method-level attribute
/// rather than an attribute *on* the parameter itself, because Rust
/// proc-macro attributes can't apply to function parameters. The
/// outer `#[v8_class]` macro reads the list and emits a SAB check
/// before extracting each named parameter's bytes.
///
/// Per WebIDL §3.2.21: BufferSource without `[AllowShared]` rejects
/// SharedArrayBuffer-backed views with TypeError. The CompressionStream
/// IDL omits `[AllowShared]`, so chunks must reject SAB. See the
/// design `compression-streams-native.md` BLOCKER-5 / D-5.
pub(super) fn extract_reject_shared(attrs: &[Attribute]) -> HashSet<String> {
    let mut names = HashSet::new();
    for attr in attrs {
        if !attr.path().is_ident("reject_shared") {
            continue;
        }
        // List form: `#[reject_shared(a, b, c)]`. Parse via
        // `Attribute::parse_args_with` + a simple comma-separated
        // identifier list.
        if let Ok(list) = attr.parse_args_with(|input: syn::parse::ParseStream| {
            let mut acc: Vec<syn::Ident> = Vec::new();
            while !input.is_empty() {
                let id: syn::Ident = input.parse()?;
                acc.push(id);
                if input.is_empty() {
                    break;
                }
                let _: syn::Token![,] = input.parse()?;
            }
            Ok(acc)
        }) {
            for id in list {
                names.insert(id.to_string());
            }
        }
    }
    names
}

/// Parse all `#[v8_const(NAME = LIT)]` attributes off an impl block.
///
/// Returns the parsed list (possibly empty) or a `syn::Error` on:
///   - malformed shape (missing `=`, non-ident name, non-int literal)
///   - duplicate name
///   - unsupported literal type suffix
pub(super) fn extract_consts(attrs: &[Attribute]) -> Result<Vec<ConstDecl>, syn::Error> {
    let mut decls: Vec<ConstDecl> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for attr in attrs {
        if !attr.path().is_ident("v8_const") {
            continue;
        }
        // Parse `NAME = LIT` inside the parens. Use parse_args with a
        // closure that reads an ident, an `=`, and a literal-expr.
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
        if !seen.insert(name_str.clone()) {
            return Err(syn::Error::new_spanned(
                &name,
                format!("#[v8_const]: duplicate constant `{name_str}`"),
            ));
        }
        // Inspect the literal's type suffix to pick the V8 materialiser.
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
        decls.push(ConstDecl {
            name,
            value: lit,
            kind,
        });
    }
    Ok(decls)
}

/// Read `#[v8_async_iterable(method = "name")]` from impl-block
/// attributes. Returns the method name to alias `[Symbol.asyncIterator]`
/// to. Per WebIDL §3.7.10.5, the spec calls for a separate
/// FunctionTemplate that wraps the named method's callback and has its
/// `name` property set to the method's name; the install codegen
/// emits exactly that pattern.
///
/// Accepts both:
///   - `#[v8_async_iterable(method = "values")]` — the canonical form
///   - `#[v8_async_iterable(method = values)]` — bare ident form, for
///     consistency with `#[v8_iterable(key = TY)]` shape.
pub(super) fn extract_async_iterable(attrs: &[Attribute]) -> Result<Option<String>, syn::Error> {
    for attr in attrs {
        if !attr.path().is_ident("v8_async_iterable") {
            continue;
        }
        let mut method: Option<String> = None;
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("method") {
                let value = meta.value()?;
                // Accept "values" or values.
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
        return Ok(Some(m));
    }
    Ok(None)
}

/// Read `#[v8_inherit_intrinsic = "IteratorPrototype"]` from impl-block
/// attributes. Currently only `"IteratorPrototype"` is recognised.
pub(super) fn extract_inherit_intrinsic(attrs: &[Attribute]) -> Option<String> {
    for attr in attrs {
        if !attr.path().is_ident("v8_inherit_intrinsic") {
            continue;
        }
        if let Meta::NameValue(nv) = &attr.meta {
            if let Expr::Lit(ExprLit {
                lit: Lit::Str(s), ..
            }) = &nv.value
            {
                return Some(s.value());
            }
        }
    }
    None
}

/// Read `#[v8_inherit(BaseClass)]` from impl-block attributes. Returns
/// the base class path — e.g. for AbortSignal inheriting EventTarget,
/// this is the parsed path `super::event_target::EventTarget`. Used
/// to plumb spec-mandated DOM inheritance (DOM §3.3 AbortSignal :
/// EventTarget) through the FunctionTemplate's `inherit` API.
///
/// Accepts both bare identifiers (`#[v8_inherit(EventTarget)]`) and
/// fully-qualified paths (`#[v8_inherit(super::event_target::EventTarget)]`)
/// — the latter is what real cross-module usage emits.
///
/// The codegen emits `__ctor_tmpl.inherit(<BaseClass>::install(scope))`.
/// The base class must itself be a `#[v8_class]`-decorated struct (or
/// expose an equivalent `install` fn — EventTarget hand-rolls one) that
/// returns a cached FunctionTemplate.
pub(super) fn extract_inherit_base(attrs: &[Attribute]) -> Option<syn::Path> {
    for attr in attrs {
        if !attr.path().is_ident("v8_inherit") {
            continue;
        }
        // List form: `#[v8_inherit(Path::To::Base)]`. Parse as a
        // path so module-qualified bases work.
        if let Ok(path) = attr.parse_args::<syn::Path>() {
            return Some(path);
        }
    }
    None
}

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

/// `#[v8_constructor(...)]` opt-out for the must-new check — currently
/// unused (no class today wants `Foo()` without `new` to succeed), but
/// retained as a hook for future legacy-callable shapes (a few WebIDL
/// interfaces are spec'd with `[LegacyFactoryFunction]`, e.g.
/// `Image()`). When `callable_no_new` is present the macro skips the
/// `is_construct_call` guard.
///
/// Parses via `Punctuated<Meta, Comma>` to coexist with the
/// `post_init = "fn_name"` shape introduced by MAC-02 (design
/// `docs/proposals/macro-constructor-post-init.md` §4.2). Bare `Path`
/// metas with the `callable_no_new` ident return true; everything else
/// (including parse failure) returns false — consistent with the prior
/// `parse_args_with`-based shape that silently ignored unparseable
/// attribute lists.
pub(super) fn extract_callable_no_new(attrs: &[Attribute]) -> bool {
    for attr in attrs {
        if !attr.path().is_ident("v8_constructor") {
            continue;
        }
        let Ok(metas) = attr.parse_args_with(
            syn::punctuated::Punctuated::<Meta, syn::Token![,]>::parse_terminated,
        ) else {
            continue;
        };
        for m in metas {
            if let Meta::Path(p) = m {
                if p.is_ident("callable_no_new") {
                    return true;
                }
            }
        }
    }
    false
}

/// `#[v8_constructor(post_init = "fn_name")]` — MAC-02. Returns the
/// named hook fn (as `syn::Ident`) or `None` if absent. Returns `Err`
/// on malformed shapes — the macro propagates those as `compile_error!`
/// at the precise span of the offending value.
///
/// Accepted shape:
///   `#[v8_constructor(post_init = "after_install")]`
///   `#[v8_constructor(callable_no_new, post_init = "after_install")]`
///
/// Rejected shapes (each emits a tailored diagnostic):
///   - non-string-literal value (`post_init = ident`)
///   - non-identifier string (`post_init = "1bad"`)
///
/// Parser is strict by design — silent no-op on malformed values would
/// be a debugging nightmare given post_init is semantically load-bearing
/// (see design §5.7).
pub(super) fn extract_post_init(attrs: &[Attribute]) -> Result<Option<syn::Ident>, syn::Error> {
    for attr in attrs {
        if !attr.path().is_ident("v8_constructor") {
            continue;
        }
        let Ok(metas) = attr.parse_args_with(
            syn::punctuated::Punctuated::<Meta, syn::Token![,]>::parse_terminated,
        ) else {
            // If this attribute can't be parsed as a punctuated Meta
            // list, we let the rest of the macro pipeline surface the
            // error (extract_callable_no_new also tolerates this; the
            // user will see a syntax error from one of the parsers).
            continue;
        };
        for meta in metas {
            let Meta::NameValue(nv) = meta else { continue };
            if !nv.path.is_ident("post_init") {
                continue;
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
            return Ok(Some(ident));
        }
    }
    Ok(None)
}
