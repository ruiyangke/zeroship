//! Analyse phase — turn a parsed `syn::ItemImpl` into a fully-validated
//! [`ClassConfig`] that the emit phase can consume directly.
//!
//! Extracted from `mod.rs` when the macro was split into dedicated
//! parse, analyse, and emit stages.
//!
//! Returns `Result<(ClassConfig, ItemImpl), TokenStream2>`. The `Err`
//! variant carries pre-rendered `compile_error!` tokens (so callers
//! pass them straight to the proc-macro driver). The `Ok` variant
//! carries the analysed cfg plus the marker-stripped impl block to
//! splice back into the emission.

use std::collections::HashMap;

use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{ImplItem, ItemImpl};

use super::fastcall::validate_fastcall_signature;
use super::helpers::{is_varargs_vec, parse_params_skipping_self};
use super::parse::{
    classify, extract_callable_no_new, extract_fastcall, extract_post_init, extract_reject_shared,
    extract_same_object, extract_v8_name, has_any_receiver, has_mut_self, is_result_unit_return,
    is_unit_return, ParsedAttrs,
};
use super::shared::class_config::ClassConfig;
use super::{ClassMethod, MethodKind};
use crate::v8_iterable;

/// Analyse a parsed `#[v8_class] impl Foo { ... }` block. Returns the
/// rendered `ClassConfig` ready for `emit::assemble_tokens` plus the
/// impl block with macro-only attributes stripped (so rustc doesn't
/// see unknown attributes after expansion).
///
/// The `'a` lifetime is the input's: `ClassConfig<'a>` and the
/// returned ItemImpl share the same backing AST. Callers that need to
/// own the AST should call `input.clone()` before invoking this fn.
pub(super) fn analyze<'a>(
    input: &'a ItemImpl,
    state_ty: &'a syn::Ident,
    marker_ty: &'a syn::Ident,
    parsed_class_attrs: ParsedAttrs,
) -> Result<(ClassConfig<'a>, ItemImpl), TokenStream2> {
    // Most existing call sites read `class_ty` as the JS-identity ident
    // (install slot / brand check / callback names) — that's the
    // marker. Keep the local name to minimise diff churn; the only
    // sites that switched to `state_ty` are the constructor's
    // `let __instance` ascription, the box / finalizer drop type, and
    // the per-method receiver cast + dispatch.
    let class_ty = marker_ty;

    let methods = collect_methods(input)?;

    // Conflict-detect duplicate JS-visible names. The macro's self-doc
    // calls this out: a `#[v8_name = "x"]` rename colliding with
    // another method literally named `x` would silently double-install
    // on the prototype. Catch it at compile time.
    //
    // Exception: a (Getter, Setter) pair under the same JS name is
    // legal — that's how WebIDL `attribute` accessors work (e.g.
    // `URL.href`'s getter+setter pair). The install codegen detects
    // this and emits a single `set_accessor_property` with both
    // templates rather than two separate calls.
    detect_duplicate_js_names(&methods)?;

    // Compute the global `has_any_fastcall` flag; the install fn
    // needs it to decide on internal-field count and the per-method
    // emit between `FunctionTemplate::new` and the fast-shim builder.
    let has_any_fastcall = methods
        .iter()
        .filter(|m| m.kind != MethodKind::Constructor)
        .any(|m| m.fastcall);

    // Impl-block-level overrides for class-wide install behaviour.
    // These used to be six separate `&[Attribute]` walks. Now
    // they're delivered pre-parsed by the single-scan
    // `parse::parse_attrs` (closes F8). The fields below are taken from
    // the already-built `ParsedAttrs`.
    let to_string_tag_override = parsed_class_attrs.to_string_tag;
    let inherit_intrinsic = parsed_class_attrs.inherit_intrinsic;
    let inherit_base = parsed_class_attrs.inherit_base;
    let async_iterable_method = parsed_class_attrs.async_iterable;
    let const_decls = parsed_class_attrs.consts;

    // Validate `#[v8_inherit_intrinsic = "..."]` at
    // analyse time, NOT during emit. Previously the install fn body
    // emitted `quote! { compile_error!(#msg); }` for unrecognised
    // values — which DOES surface the right diagnostic but spliced
    // INSIDE a fn body. Rustc's parser then trips on "expected
    // expression" / "unused variable" follow-on errors that drown out
    // the real one. Surfacing as `syn::Error::to_compile_error()` from
    // `analyze` lets the proc-macro driver emit a single clean
    // diagnostic with a span on the attribute, before any fn body is
    // built. Closes NS2 from the v2 code-critic.
    if let Some(ref value) = inherit_intrinsic {
        if value != "IteratorPrototype" && value != "Error" {
            return Err(syn::Error::new_spanned(
                &input.self_ty,
                format!(
                    "#[v8_inherit_intrinsic]: unrecognised value `{value}` (expected \"IteratorPrototype\" or \"Error\")"
                ),
            )
            .to_compile_error());
        }
    }

    // Validate that the named method actually exists in the impl block
    // — better error than waiting for the method-callback ident lookup
    // to fail at quote-expansion time. Match against the JS-visible
    // name (post-`#[v8_name = ...]` rename) since that's what users
    // think of.
    if let Some(ref name) = async_iterable_method {
        let exists = methods.iter().any(|m| {
            matches!(m.kind, MethodKind::Method | MethodKind::AsyncMethod) && &m.js_name == name
        });
        if !exists {
            return Err(syn::Error::new_spanned(
                &input.self_ty,
                format!(
                    "#[v8_async_iterable(method = \"{name}\")]: no method named `{name}` (must be \
                     `#[v8_method]` or `#[v8_async_method]` on this impl block)"
                ),
            )
            .to_compile_error());
        }
    }

    // `#[v8_iterable(key = K, value = V)]` — emit the pair-iterator
    // surface (keys / values / entries / forEach / @@iterator) plus a
    // companion `<Class>Iterator` class. The user supplies a
    // `value_pairs(&[mut] self [, scope]) -> Vec<(K, V)>` method on the
    // impl block; we sniff its receiver/arg shape so the codegen can
    // pick the right pointer recovery (`*const`/`*mut`) and pass the
    // outer scope through when requested.
    let iterable_attr =
        v8_iterable::extract_iterable(&input.attrs).map_err(|e| e.to_compile_error())?;
    let value_pairs_sig = v8_iterable::inspect_value_pairs(&input.items);
    let iterable_codegen = match iterable_attr.as_ref() {
        Some(attr) => v8_iterable::generate(class_ty, state_ty, attr, value_pairs_sig)
            .map_err(|e| e.to_compile_error())?,
        None => quote! {},
    };
    let install_iterable_call = if iterable_attr.is_some() {
        // The `gen()` codegen above emitted
        // `<Class>::__zs_install_iterable_methods(scope, __proto)`. We
        // insert the call here so it fires at the end of `install`'s
        // prototype-template setup.
        Some(quote! {
            <#class_ty>::__zs_install_iterable_methods(scope, __proto);
        })
    } else {
        None
    };

    let cfg = ClassConfig::new(
        class_ty,
        state_ty,
        methods,
        has_any_fastcall,
        to_string_tag_override,
        inherit_intrinsic,
        inherit_base,
        async_iterable_method,
        const_decls,
        iterable_codegen,
        install_iterable_call,
    );

    let stripped_impl = strip_marker_attrs(input.clone());
    Ok((cfg, stripped_impl))
}

/// Walk impl-block fns, classify each via `parse::classify`, and run
/// the per-kind compile-time guards (e.g. `#[v8_async_method] does
/// not support &mut self`). Returns the validated `Vec<ClassMethod>`
/// or rendered `compile_error!` tokens for the first guard violation.
fn collect_methods(input: &ItemImpl) -> Result<Vec<ClassMethod<'_>>, TokenStream2> {
    let mut methods: Vec<ClassMethod> = Vec::new();
    for item in &input.items {
        if let ImplItem::Fn(func) = item {
            if let Some(kind) = classify(func) {
                // Per-method extracts now share the strict
                // MarkerAttr error path. Surface malformed-shape errors
                // via the proc-macro's compile-error stream.
                let js_name = extract_v8_name(&func.attrs)
                    .map_err(|e| e.to_compile_error())?
                    .unwrap_or_else(|| func.sig.ident.to_string());
                let mut_recv = has_mut_self(func);

                // Compile-time guard: `#[v8_async_method]` + `&mut self`
                // is unsound under V8 re-entry. The future captures a
                // `*mut Self` that's re-acquired on every poll; if a
                // user `.await` runs JS that re-enters the same method
                // (e.g. `await something(); this.foo()` triggered by a
                // microtask), we'd alias `&mut self` with another
                // borrow inside the same instance. Cell/RefCell on a
                // `&self` method makes the runtime borrow check
                // explicit; we require that pattern here.
                if matches!(kind, MethodKind::AsyncMethod) && mut_recv {
                    return Err(syn::Error::new_spanned(
                        &func.sig.ident,
                        "#[v8_async_method] does not support &mut self — use \
                         &self with Cell/RefCell on state that needs to mutate \
                         (borrow across .await is unsound under V8 re-entry)",
                    )
                    .to_compile_error());
                }

                // Compile-time guard: `#[v8_async_method]` requires the
                // function to be declared `async`. Without `async`, the
                // user's body would need to return a Future explicitly
                // (an unergonomic shape we don't support) — and the
                // macro's call-site emits `.await`, which would fail
                // type-check on a non-Future return.
                if matches!(kind, MethodKind::AsyncMethod) && func.sig.asyncness.is_none() {
                    return Err(syn::Error::new_spanned(
                        &func.sig.ident,
                        "#[v8_async_method] requires the method to be declared `async`",
                    )
                    .to_compile_error());
                }

                let same_object_flag = matches!(kind, MethodKind::Getter)
                    && extract_same_object(&func.attrs).map_err(|e| e.to_compile_error())?;

                // Compile-time guard: static methods / getters cannot
                // have a receiver. WebIDL §3.7.4 static operations are
                // invoked via `Class.method()` with no `this`; the
                // emitted callback has no internal-field 0 to recover
                // a `Box<Self>` from, so a `&self` / `&mut self` arg
                // would never be bound. Reject at compile time with a
                // clear pointer rather than emit broken codegen.
                if matches!(kind, MethodKind::StaticMethod | MethodKind::StaticGetter)
                    && has_any_receiver(func)
                {
                    return Err(syn::Error::new_spanned(
                        &func.sig.ident,
                        "#[v8_static_method] / #[v8_static_getter] cannot have a \
                         `self` receiver — static operations are invoked via \
                         `Class.method()` with no `this`",
                    )
                    .to_compile_error());
                }

                // Compile-time guard: setters whose return type is
                // neither `()` nor `Result<(), OpError>` are rejected.
                // WebIDL §3.7.6 attribute-setter semantics specify that
                // V8's accessor setter ABI discards whatever the
                // callback writes to `rv` — so a non-unit, non-Result
                // return would have its value silently swallowed (the
                // §13.1 finding from
                // runtime-macros-architecture-critique-2026-05-05).
                // `Result<(), OpError>` IS supported because
                // `gen_setter_callback` honours it: an `Err` arm
                // routes through `gen_throw_op_error_arms` and surfaces
                // as a JS exception, matching the
                // `#[v8_method]` Result-return contract.
                if matches!(kind, MethodKind::Setter)
                    && !is_unit_return(&func.sig.output)
                    && !is_result_unit_return(&func.sig.output)
                {
                    return Err(syn::Error::new_spanned(
                        &func.sig.output,
                        "#[v8_setter] must return `()` or `Result<(), OpError>` \
                         — V8 accessor setters discard the return value, so a \
                         non-unit, non-Result return would have its value \
                         silently swallowed. Use `Result<(), OpError>` if you \
                         need to surface an error from the setter logic.",
                    )
                    .to_compile_error());
                }

                // `#[v8_method(fastcall)]` / `#[v8_getter(fastcall)]`.
                // Only valid on plain Method / Getter — not async, not
                // setter, not constructor, not same_object.
                let fastcall_flag = matches!(kind, MethodKind::Method | MethodKind::Getter)
                    && extract_fastcall(&func.attrs).map_err(|e| e.to_compile_error())?;

                if fastcall_flag {
                    // Compile-time guard 1: fastcall path can't take
                    // `&mut self`. The macro emits the fast shim as a
                    // bare `extern "C"` fn that recovers `*const Self`
                    // from internal-field 1; there's no slot for the
                    // re-entrancy guard the slow path emits for
                    // `&mut self` callbacks. The user must use
                    // `&self` + `Cell`/`RefCell` for state that mutates.
                    if mut_recv {
                        return Err(syn::Error::new_spanned(
                            &func.sig.ident,
                            "#[v8_method(fastcall)] / #[v8_getter(fastcall)] does not \
                             support &mut self — use &self with Cell/RefCell on state \
                             that needs to mutate (V8 fast-path callbacks have no \
                             scope, so the slow path's per-method re-entrancy guard \
                             cannot be emitted)",
                        )
                        .to_compile_error());
                    }
                    if same_object_flag {
                        return Err(syn::Error::new_spanned(
                            &func.sig.ident,
                            "#[v8_getter(same_object, fastcall)] is not supported — \
                             SameObject getters return a v8::Global<v8::Object> \
                             (allocates), and the fast path forbids allocation",
                        )
                        .to_compile_error());
                    }
                    if let Err(err) = validate_fastcall_signature(func) {
                        return Err(err.to_compile_error());
                    }
                }

                // Variadic param validation. The macro recognises a
                // `Vec<v8::Local<v8::Value>>` trailing parameter as
                // "give me args[N..] as a Vec" — see
                // `helpers::is_varargs_param` for the shape contract.
                // Reject the disallowed combinations here so the user
                // sees a span'd compile-error rather than a confused
                // codegen failure downstream.
                validate_variadic_param(func, kind, fastcall_flag)
                    .map_err(|e| e.to_compile_error())?;

                // Fold per-method attribute extracts into the
                // ClassMethod record so emit-side helpers don't walk
                // attrs again. Each extract uses the strict MarkerAttr
                // error path — malformed shapes surface as compile-
                // errors with the offending span.
                let reject_shared_names =
                    extract_reject_shared(&func.attrs).map_err(|e| e.to_compile_error())?;
                let callable_no_new = matches!(kind, MethodKind::Constructor)
                    && extract_callable_no_new(&func.attrs)
                        .map_err(|e| e.to_compile_error())?;
                let post_init = if matches!(kind, MethodKind::Constructor) {
                    extract_post_init(&func.attrs).map_err(|e| e.to_compile_error())?
                } else {
                    None
                };

                methods.push(ClassMethod {
                    kind,
                    func,
                    mut_receiver: mut_recv,
                    js_name,
                    same_object: same_object_flag,
                    fastcall: fastcall_flag,
                    reject_shared_names,
                    callable_no_new,
                    post_init,
                });
            }
        }
    }
    Ok(methods)
}

/// Reject duplicate JS-visible names (post-`#[v8_name]` rename). The
/// (Getter, Setter) pair exception lets WebIDL `attribute` accessors
/// register both halves under the same JS-name — the install codegen
/// detects this and emits one `set_accessor_property` call.
fn detect_duplicate_js_names(methods: &[ClassMethod<'_>]) -> Result<(), TokenStream2> {
    let mut seen: HashMap<String, &ClassMethod> = HashMap::new();
    for m in methods {
        if m.kind == MethodKind::Constructor {
            continue;
        }
        if let Some(prev) = seen.insert(m.js_name.clone(), m) {
            let pair_ok = matches!(
                (prev.kind, m.kind),
                (MethodKind::Getter, MethodKind::Setter)
                    | (MethodKind::Setter, MethodKind::Getter)
            );
            if !pair_ok {
                return Err(syn::Error::new_spanned(
                    &m.func.sig.ident,
                    format!(
                        "#[v8_class]: duplicate JS-visible method name `{}` (rename one with #[v8_name])",
                        m.js_name,
                    ),
                )
                .to_compile_error());
            }
        }
    }
    Ok(())
}

/// Validate a method's variadic-param shape. The macro recognises a
/// trailing `Vec<v8::Local<v8::Value>>` parameter as "give me args
/// from index N onward as a Vec." Enforce:
///   - at most ONE varargs param (the codegen would only fill the
///     last one anyway, but two would silently confuse the user);
///   - varargs MUST be the LAST positional param (everything after
///     it would always extract `undefined` because the variadic
///     swallows the remaining JS arg range);
///   - varargs is REJECTED on fastcall (fixed-arity by V8 ABI),
///     getters / setters (single-value spec ABI), and constructors
///     (constructor codegen has no place to bind a Vec — and the
///     usefulness is marginal; use a regular `Vec<...>` arg instead).
///   - varargs IS allowed on `#[v8_method]` and `#[v8_async_method]`.
///
/// Errors are returned as `syn::Error` so the proc-macro driver can
/// span the diagnostic on the offending parameter (or method ident
/// when no specific parameter exists).
fn validate_variadic_param(
    func: &syn::ImplItemFn,
    kind: MethodKind,
    fastcall_flag: bool,
) -> syn::Result<()> {
    let params = parse_params_skipping_self(func);
    let varargs_idxs: Vec<usize> = params
        .iter()
        .enumerate()
        .filter_map(|(i, p)| if is_varargs_vec(&p.ty) { Some(i) } else { None })
        .collect();

    if varargs_idxs.is_empty() {
        return Ok(());
    }

    if varargs_idxs.len() > 1 {
        // Span on the second occurrence — the first one is fine
        // structurally; it's the duplicate that's wrong.
        let dup_param = &params[varargs_idxs[1]];
        return Err(syn::Error::new_spanned(
            &dup_param.ty,
            "#[v8_method]: at most one variadic `Vec<v8::Local<v8::Value>>` parameter is allowed",
        ));
    }

    let varargs_idx = varargs_idxs[0];
    if varargs_idx != params.len() - 1 {
        let p = &params[varargs_idx];
        return Err(syn::Error::new_spanned(
            &p.ty,
            "#[v8_method]: variadic `Vec<v8::Local<v8::Value>>` parameter must be the \
             LAST parameter (it captures all JS args from this position to args.length())",
        ));
    }

    if fastcall_flag {
        return Err(syn::Error::new_spanned(
            &func.sig.ident,
            "#[v8_method(fastcall)] does not support variadic args; remove `fastcall` \
             or drop the trailing `Vec<v8::Local<v8::Value>>` parameter \
             (V8 fast API is fixed-arity by design)",
        ));
    }

    match kind {
        MethodKind::Getter | MethodKind::Setter => Err(syn::Error::new_spanned(
            &func.sig.ident,
            "getters and setters take fixed arity per WebIDL §3.7.6 — \
             variadic `Vec<v8::Local<v8::Value>>` is not allowed",
        )),
        MethodKind::StaticGetter => Err(syn::Error::new_spanned(
            &func.sig.ident,
            "static getters take fixed arity — variadic \
             `Vec<v8::Local<v8::Value>>` is not allowed",
        )),
        MethodKind::Constructor => Err(syn::Error::new_spanned(
            &func.sig.ident,
            "#[v8_constructor] does not support variadic \
             `Vec<v8::Local<v8::Value>>` parameters; use a regular \
             `Vec<...>` arg if you need a sequence",
        )),
        MethodKind::Method | MethodKind::AsyncMethod | MethodKind::StaticMethod => Ok(()),
    }
}

fn strip_marker_attrs(mut input: ItemImpl) -> ItemImpl {
    // Strip impl-block-level marker attributes (consumed by the macro,
    // not a real Rust feature).
    input.attrs.retain(|attr| {
        let p = attr.path();
        !(p.is_ident("v8_to_string_tag")
            || p.is_ident("v8_inherit_intrinsic")
            || p.is_ident("v8_inherit")
            || p.is_ident("v8_iterable")
            || p.is_ident("v8_async_iterable")
            || p.is_ident("v8_const")
            || p.is_ident("v8_state_marker"))
    });
    for item in &mut input.items {
        if let ImplItem::Fn(func) = item {
            func.attrs.retain(|attr| {
                let p = attr.path();
                !(p.is_ident("v8_method")
                    || p.is_ident("v8_async_method")
                    || p.is_ident("v8_getter")
                    || p.is_ident("v8_setter")
                    || p.is_ident("v8_constructor")
                    || p.is_ident("v8_static_method")
                    || p.is_ident("v8_static_getter")
                    || p.is_ident("v8_name")
                    || p.is_ident("reject_shared"))
            });
        }
    }
    input
}
