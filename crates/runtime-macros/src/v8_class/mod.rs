//! `#[v8_class]` proc macro — wraps a Rust `impl` block as a V8
//! ObjectTemplate-backed class.
//!
//! Walks the impl block, collects methods marked with `#[v8_method]`,
//! `#[v8_async_method]`, `#[v8_getter]`, `#[v8_setter]`,
//! `#[v8_constructor]`, and emits a `Self::install(scope) ->
//! v8::Local<v8::FunctionTemplate>` function plus per-method
//! callbacks.
//!
//! Instance state lives in the V8 object's internal field (slot 0): a
//! `Box<Self>` is stored as `External` and reclaimed via a guaranteed
//! V8 weak-finalizer when the wrapper is GC'd.
//!
//! Argument and return marshaling lives in the parent crate's
//! `gen_extract` + `gen_call_return` helpers. Supported types: String,
//! bool, u32, i32, f64, Vec<u8>, Option<T>, Result<T, OpError>, plus
//! `v8::Local<v8::Value>` passthrough for union-typed args.
//!
//! ### Async methods (`#[v8_async_method]`)
//!
//! Async-marked methods compile to a sync V8 callback that allocates a
//! `v8::PromiseResolver`, spawns the user's `async fn` body via
//! `state.spawned_ops`, and returns the Promise immediately. The pump
//! resolves (or rejects) the bound resolver from `OpResult::JsValue`
//! when the future settles. `&mut self` async methods are rejected at
//! compile time — borrow across `.await` is unsound under V8 re-entry.
//! Use `&self` with `Cell` / `RefCell` for state that needs to mutate
//! inside the body. See `gen_async_method_callback`'s doc comment for
//! the borrow-safety contract.
//!
//! ## Same-name getter+setter pairing
//!
//! Defining `#[v8_getter] value(&self)` AND `#[v8_setter] value(&mut
//! self, v)` in the same impl block is illegal Rust (duplicate method
//! names). The supported pattern: rename the Rust fns and apply
//! `#[v8_name = "value"]` to both halves. The install codegen pairs
//! by JS-visible name into a single `set_accessor_property("value",
//! getter, setter, attrs)` call rather than two installs that would
//! each overwrite the previous. See `tests/v8_paired_accessor_smoke.rs`
//! for the supported shapes.
//!
//! ## Submodule layout
//!
//! - `parse` — attribute parsing (`extract_*` helpers), `MethodKind`
//!   classifier, receiver-shape predicates.
//! - `method` — slow-path FunctionCallback codegen for methods, getters,
//!   setters, async methods, static methods/getters, and constructors.
//!   Hosts `gen_box_and_install_finalizer` and the re-entrancy guard.
//! - `fastcall` — `#[v8_method(fastcall)]` / `#[v8_getter(fastcall)]`
//!   shim emission and signature validation.
//! - `helpers` — cross-submodule utilities: `method_callback_ident`,
//!   `gen_param_extractions`, `parse_params_skipping_self`, type
//!   classification predicates.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use std::collections::HashMap;
use syn::{ImplItem, ItemImpl, Type};

use crate::v8_iterable;

mod emit;
mod fastcall;
mod helpers;
mod method;
mod parse;
pub(crate) mod shared;

use fastcall::validate_fastcall_signature;
use parse::{
    classify, extract_async_iterable, extract_consts, extract_fastcall, extract_inherit_base,
    extract_inherit_intrinsic, extract_same_object, extract_state_marker, extract_to_string_tag,
    extract_v8_name, has_any_receiver, has_mut_self, resolve_state_and_marker,
};

// ---------------------------------------------------------------------------
// Method classification
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MethodKind {
    Method,
    /// An async method — emits a callback that spawns a future via
    /// `state.spawned_ops` and returns a Promise. The user writes
    /// `async fn foo(&self, ...) -> T` (or `Result<T, OpError>`) and
    /// the macro hides the resolver/spawn dance. Rejected at compile
    /// time if the receiver is `&mut self` (borrow across .await is
    /// unsound under V8 re-entry).
    AsyncMethod,
    Getter,
    Setter,
    Constructor,
    /// WebIDL §3.7.4 static operation — `#[v8_static_method]`. No
    /// receiver, no brand check, no internal-field deref. Installed
    /// on the constructor FunctionTemplate, not the prototype.
    StaticMethod,
    /// WebIDL §3.7.4 static attribute (read-only) — `#[v8_static_getter]`.
    /// No receiver. Installed via `set_accessor_property` on the
    /// constructor template.
    StaticGetter,
}

pub(crate) struct ClassMethod<'a> {
    pub(crate) kind: MethodKind,
    pub(crate) func: &'a syn::ImplItemFn,
    /// Whether the receiver is `&mut self` (vs `&self`). Constructors
    /// have no receiver — we set this to false; it's unused for them.
    pub(crate) mut_receiver: bool,
    /// JS-visible name. Defaults to the Rust identifier; overridden by
    /// `#[v8_name = "..."]` on the method. Lets us install
    /// `delete_(&mut self)` under the JS name `delete`, etc.
    pub(crate) js_name: String,
    /// `#[v8_getter(same_object)]` — WebIDL `[SameObject]` semantics:
    /// the getter must return THE SAME JS object across reads on the
    /// same wrapper instance. The macro caches via a V8 private symbol
    /// keyed by `__zs_same_object_<ClassTy>_<getter>`. User method
    /// returns `v8::Global<v8::Object>` (minted on first call); macro
    /// stashes it on the wrapper instance and returns the cached Local
    /// thereafter. Only meaningful for `MethodKind::Getter`.
    pub(crate) same_object: bool,
    /// `#[v8_method(fastcall)]` / `#[v8_getter(fastcall)]` — emit a
    /// CFunction shim alongside the slow-path FunctionCallback so V8
    /// Turbofan can inline the typed-shape call at hot sites. See
    /// `extract_fastcall` for the rationale and `gen_fastcall_*` for
    /// the codegen detail. Mutually compatible with `same_object` only
    /// in the negative — fastcall paths can't allocate and SameObject
    /// returns a Global<Object>, so the two flags are not co-applicable.
    pub(crate) fastcall: bool,
}

/// A single `#[v8_const(NAME = LIT)]` declaration.
pub(crate) struct ConstDecl {
    /// The JS-visible property name (Rust ident verbatim).
    pub(crate) name: syn::Ident,
    /// The literal expression — quoted as-is so the literal's type
    /// suffix is preserved through expansion.
    pub(crate) value: syn::ExprLit,
    /// Selected V8-side materialiser, derived from the literal suffix.
    pub(crate) kind: ConstKind,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum ConstKind {
    /// Suffix `u16` or `u32` → `v8::Integer::new_from_unsigned`.
    UInt,
    /// Suffix `i32` → `v8::Integer::new`.
    SInt,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub fn expand(attr: TokenStream, item: TokenStream) -> TokenStream {
    expand_tokens(attr.into(), item.into()).into()
}

/// proc-macro2 entry — same logic as [`expand`] but operates on
/// `TokenStream2` so unit tests in this crate can call it without going
/// through the proc-macro driver. Insta snapshots in
/// `tests/v8_class_codegen_snapshot.rs` consume this entry.
pub fn expand_tokens(_attr: TokenStream2, item: TokenStream2) -> TokenStream2 {
    let input: ItemImpl = match syn::parse2(item) {
        Ok(parsed) => parsed,
        Err(e) => return e.to_compile_error(),
    };

    let receiver_ty = match extract_class_ident(&input.self_ty) {
        Some(t) => t,
        None => {
            return syn::Error::new_spanned(
                &input.self_ty,
                "#[v8_class] requires a plain type, e.g. `impl Headers`",
            )
            .to_compile_error();
        }
    };

    // MAC-01 Phase 1 (design `docs/proposals/macro-v8-state.md` §4.2):
    // resolve `(state_ty, marker_ty)`.
    //  - `state_ty` is what the box stored in V8 internal field 0
    //    contains (`Box<StateTy>`). The macro emits casts as
    //    `*mut StateTy` / `*const StateTy`, the constructor returns
    //    `StateTy`, and per-method `&self` desugars against the impl
    //    receiver — which IS `StateTy` under Option B.
    //  - `marker_ty` drives JS-class identity: install/brand slots,
    //    callback names, the install fn's enclosing impl, the
    //    `set_class_name` literal, must-new/Symbol.toStringTag, and
    //    iterable companion install.
    //
    // Without `#[v8_state_marker]`: state == marker == receiver
    // (byte-identical to today's emission, locked by insta snapshots).
    // With `#[v8_state_marker(M)] impl S`: state = S, marker = M.
    let state_marker_path = extract_state_marker(&input.attrs);
    let (state_ty, marker_ty) =
        match resolve_state_and_marker(receiver_ty, state_marker_path.as_ref()) {
            Ok(pair) => pair,
            Err(ts) => return ts,
        };
    // Most existing call sites read `class_ty` as the JS-identity ident
    // (install slot / brand check / callback names) — that's now the
    // marker. Keep the local name to minimise diff churn; the only
    // sites that switched to `state_ty` are the constructor's
    // `let __instance` ascription, the box / finalizer drop type, and
    // the per-method receiver cast + dispatch (rows 6-15, 17-18 in
    // §4.1 of the design).
    let class_ty = &marker_ty;

    let mut methods: Vec<ClassMethod> = Vec::new();
    for item in &input.items {
        if let ImplItem::Fn(func) = item {
            if let Some(kind) = classify(func) {
                let js_name = extract_v8_name(&func.attrs)
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
                    return syn::Error::new_spanned(
                        &func.sig.ident,
                        "#[v8_async_method] does not support &mut self — use \
                         &self with Cell/RefCell on state that needs to mutate \
                         (borrow across .await is unsound under V8 re-entry)",
                    )
                    .to_compile_error();
                }

                // Compile-time guard: `#[v8_async_method]` requires the
                // function to be declared `async`. Without `async`, the
                // user's body would need to return a Future explicitly
                // (an unergonomic shape we don't support) — and the
                // macro's call-site emits `.await`, which would fail
                // type-check on a non-Future return.
                if matches!(kind, MethodKind::AsyncMethod) && func.sig.asyncness.is_none() {
                    return syn::Error::new_spanned(
                        &func.sig.ident,
                        "#[v8_async_method] requires the method to be declared `async`",
                    )
                    .to_compile_error();
                }

                let same_object_flag =
                    matches!(kind, MethodKind::Getter) && extract_same_object(&func.attrs);

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
                    return syn::Error::new_spanned(
                        &func.sig.ident,
                        "#[v8_static_method] / #[v8_static_getter] cannot have a \
                         `self` receiver — static operations are invoked via \
                         `Class.method()` with no `this`",
                    )
                    .to_compile_error();
                }

                // `#[v8_method(fastcall)]` / `#[v8_getter(fastcall)]`.
                // Only valid on plain Method / Getter — not async, not
                // setter, not constructor, not same_object.
                let fastcall_flag = matches!(kind, MethodKind::Method | MethodKind::Getter)
                    && extract_fastcall(&func.attrs);

                if fastcall_flag {
                    // Compile-time guard 1: fastcall path can't take
                    // `&mut self`. The macro emits the fast shim as a
                    // bare `extern "C"` fn that recovers `*const Self`
                    // from internal-field 1; there's no slot for the
                    // re-entrancy guard the slow path emits for
                    // `&mut self` callbacks. The user must use
                    // `&self` + `Cell`/`RefCell` for state that mutates.
                    if mut_recv {
                        return syn::Error::new_spanned(
                            &func.sig.ident,
                            "#[v8_method(fastcall)] / #[v8_getter(fastcall)] does not \
                             support &mut self — use &self with Cell/RefCell on state \
                             that needs to mutate (V8 fast-path callbacks have no \
                             scope, so the slow path's per-method re-entrancy guard \
                             cannot be emitted)",
                        )
                        .to_compile_error();
                    }
                    if same_object_flag {
                        return syn::Error::new_spanned(
                            &func.sig.ident,
                            "#[v8_getter(same_object, fastcall)] is not supported — \
                             SameObject getters return a v8::Global<v8::Object> \
                             (allocates), and the fast path forbids allocation",
                        )
                        .to_compile_error();
                    }
                    if let Err(err) = validate_fastcall_signature(func) {
                        return err.to_compile_error();
                    }
                }

                methods.push(ClassMethod {
                    kind,
                    func,
                    mut_receiver: mut_recv,
                    js_name,
                    same_object: same_object_flag,
                    fastcall: fastcall_flag,
                });
            }
        }
    }

    // Conflict-detect duplicate JS-visible names. The macro's self-doc
    // (lines 28–34) calls this out: a `#[v8_name = "x"]` rename
    // colliding with another method literally named `x` would silently
    // double-install on the prototype. Catch it at compile time.
    //
    // Exception: a (Getter, Setter) pair under the same JS name is
    // legal — that's how WebIDL `attribute` accessors work (e.g.
    // `URL.href`'s getter+setter pair). The install codegen detects
    // this and emits a single `set_accessor_property` with both
    // templates rather than two separate calls.
    let mut seen: HashMap<String, &ClassMethod> = HashMap::new();
    for m in &methods {
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
                return syn::Error::new_spanned(
                    &m.func.sig.ident,
                    format!(
                        "#[v8_class]: duplicate JS-visible method name `{}` (rename one with #[v8_name])",
                        m.js_name,
                    ),
                )
                .to_compile_error();
            }
        }
    }

    // Compute the global `has_any_fastcall` flag; the install fn
    // needs it to decide on internal-field count and the per-method
    // emit between `FunctionTemplate::new` and the fast-shim builder.
    let has_any_fastcall = methods
        .iter()
        .filter(|m| m.kind != MethodKind::Constructor)
        .any(|m| m.fastcall);

    // Impl-block-level overrides for class-wide install behaviour.
    // These were 6 separate args to `gen_install` before Wave 3; now
    // they're fields on the single `ClassConfig` parameter object.
    let to_string_tag_override = extract_to_string_tag(&input.attrs);
    let inherit_intrinsic = extract_inherit_intrinsic(&input.attrs);
    let inherit_base = extract_inherit_base(&input.attrs);
    let async_iterable_method = match extract_async_iterable(&input.attrs) {
        Ok(opt) => opt,
        Err(err) => return err.to_compile_error(),
    };
    let const_decls = match extract_consts(&input.attrs) {
        Ok(d) => d,
        Err(err) => return err.to_compile_error(),
    };

    // Validate that the named method actually exists in the impl block
    // — better error than waiting for the method-callback ident lookup
    // to fail at quote-expansion time. Match against the JS-visible
    // name (post-`#[v8_name = ...]` rename) since that's what users
    // think of.
    if let Some(ref name) = async_iterable_method {
        let exists = methods.iter().any(|m| {
            matches!(
                m.kind,
                MethodKind::Method | MethodKind::AsyncMethod
            ) && &m.js_name == name
        });
        if !exists {
            return syn::Error::new_spanned(
                &input.self_ty,
                format!(
                    "#[v8_async_iterable(method = \"{name}\")]: no method named `{name}` (must be \
                     `#[v8_method]` or `#[v8_async_method]` on this impl block)"
                ),
            )
            .to_compile_error();
        }
    }

    // `#[v8_iterable(key = K, value = V)]` — emit the pair-iterator
    // surface (keys / values / entries / forEach / @@iterator) plus a
    // companion `<Class>Iterator` class. The user supplies a
    // `value_pairs(&[mut] self [, scope]) -> Vec<(K, V)>` method on the
    // impl block; we sniff its receiver/arg shape so the codegen can
    // pick the right pointer recovery (`*const`/`*mut`) and pass the
    // outer scope through when requested.
    let iterable_attr = match v8_iterable::extract_iterable(&input.attrs) {
        Ok(opt) => opt,
        Err(err) => return err.to_compile_error(),
    };
    let value_pairs_sig = v8_iterable::inspect_value_pairs(&input.items);
    let iterable_codegen = match iterable_attr.as_ref() {
        Some(attr) => match v8_iterable::generate(class_ty, attr, value_pairs_sig) {
            Ok(ts) => ts,
            Err(err) => return err.to_compile_error(),
        },
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

    // ----------------------------------------------------------------
    // Build the ClassConfig parameter object (Wave 3 / design §3.1).
    // Every emit helper from here on takes `&ClassConfig` as its first
    // arg — closes F4's 10-arg `gen_install` signature and the
    // `(class_ty, state_ty, ...)` repetition across every helper.
    //
    // Once built, the bare `methods` Vec is moved into the cfg; the
    // emit phase iterates `cfg.methods` / `cfg.regular()` exclusively
    // from this point.
    // ----------------------------------------------------------------
    let cfg = shared::class_config::ClassConfig::new(
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

    // Strip our marker attributes from the impl items so rustc doesn't
    // see unknown attributes after expansion. Keep everything else.
    let stripped_impl = strip_marker_attrs(input.clone());

    // Hand off to the emit orchestrator. `assemble_tokens` composes
    // per-fragment helpers (slot types, brand check, public_is,
    // install fn, per-method callbacks, fastcall shims, iterable
    // codegen) into the final token stream. The 195-line megaquote
    // that lived in this file before Wave 3 commit 3 is now
    // distributed across `emit/{slot_types,brand,public_is,install,
    // mod}.rs` (design §4.1, F3).
    emit::assemble_tokens(&cfg, &stripped_impl)
}

fn extract_class_ident(ty: &Type) -> Option<&syn::Ident> {
    match ty {
        Type::Path(p) => p.path.get_ident(),
        _ => None,
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


// ---------------------------------------------------------------------------
// Codegen snapshot tests (MAC-01 Phase 1)
// ---------------------------------------------------------------------------
//
// Lock the macro's emission against unintended drift. Per design §5.1
// / §6.4: the no-attribute path is required to be byte-identical to
// pre-Phase-1 emission (modulo the qualified Private-symbol name in row
// 16). The new `#[v8_state_marker]` path is also snapshotted so a
// future change can detect drift in either direction.
//
// We snapshot the prettyplease-formatted output of `expand_tokens` so
// the snapshot stays human-readable across rustc / quote tweaks. Bumps
// require `cargo insta accept` with reviewer audit (design §8 settled-
// question 9).
#[cfg(test)]
mod snapshots {
    use super::expand_tokens;
    use proc_macro2::TokenStream as TokenStream2;
    use quote::quote;

    /// Format the macro output through prettyplease so the snapshot
    /// stays diff-friendly across whitespace tweaks in `quote!`.
    fn format_expansion(out: TokenStream2) -> String {
        // Parse the emitted tokens back as a `syn::File` so prettyplease
        // can format them. The macro emits items at module scope.
        let parsed: syn::File = syn::parse2(out).expect("macro output parses as items");
        prettyplease::unparse(&parsed)
    }

    /// Insta inline snapshot for the no-attribute (control) shape — a
    /// `#[v8_class] impl Foo { ... }` with one constructor + one method
    /// + one getter + one setter. Locks the byte-identical-emission
    /// invariant that the no-attribute path must satisfy
    /// (design §5.1 over CloseEventState / AbortSignal / Blob).
    #[test]
    fn snapshot_class_basic() {
        let item = quote! {
            impl Foo {
                #[v8_constructor]
                fn new(start: u32) -> Foo {
                    Foo { value: start }
                }

                #[v8_method]
                fn touch(&mut self) -> u32 {
                    self.value += 1;
                    self.value
                }

                #[v8_getter]
                fn value(&self) -> u32 {
                    self.value
                }

                #[v8_setter]
                #[v8_name = "value"]
                fn set_value(&mut self, n: u32) {
                    self.value = n;
                }
            }
        };
        let out = expand_tokens(quote! {}, item);
        insta::assert_snapshot!("class_basic", format_expansion(out));
    }

    /// Insta inline snapshot for the new `#[v8_state_marker(Marker)]
    /// impl State` shape. The marker (`Marker`) drives JS-class
    /// identity; the receiver (`State`) drives the `Box<State>` payload
    /// and per-method receiver type.
    #[test]
    fn snapshot_class_with_state_marker() {
        let item = quote! {
            #[v8_state_marker(Marker)]
            impl State {
                #[v8_constructor]
                fn new(start: u32) -> Result<State, OpError> {
                    Ok(State { value: start })
                }

                #[v8_method]
                fn touch(&mut self) -> u32 {
                    self.value += 1;
                    self.value
                }

                #[v8_getter]
                fn value(&self) -> u32 {
                    self.value
                }
            }
        };
        let out = expand_tokens(quote! {}, item);
        insta::assert_snapshot!("class_with_state_marker", format_expansion(out));
    }

    /// Hard-error snapshot: marker == receiver. Per design §4.7 the
    /// macro emits a clear compile_error rather than silently treating
    /// it as a no-op (which would mask a typo'd marker name).
    #[test]
    fn snapshot_class_marker_equals_receiver_errors() {
        let item = quote! {
            #[v8_state_marker(Foo)]
            impl Foo {
                #[v8_constructor]
                fn new() -> Foo { Foo }
            }
        };
        let out = expand_tokens(quote! {}, item);
        // Compile-error tokens still parse as a valid syn::File (each
        // `compile_error!(...)` is an item-level macro invocation), so
        // prettyplease can format them.
        insta::assert_snapshot!(
            "class_marker_equals_receiver_errors",
            format_expansion(out)
        );
    }
}
