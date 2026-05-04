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
//! Reuses `gen_extract` + `gen_call_return` from the parent crate for
//! argument and return marshaling, so the supported types match
//! `#[zeroship_op]` (String, bool, u32, i32, f64, Vec<u8>, Option<T>,
//! Result<T, OpError>, plus `v8::Local<v8::Value>` passthrough for
//! union-typed args).
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

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use std::collections::{HashMap, HashSet};
use syn::{
    parse_macro_input, Attribute, Expr, ExprLit, FnArg, ImplItem, ImplItemFn, ItemImpl, Lit, Meta,
    Receiver, ReturnType, Type,
};

use crate::{gen_call_return, gen_extract, v8_iterable};

// ---------------------------------------------------------------------------
// Method classification
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MethodKind {
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
}

struct ClassMethod<'a> {
    kind: MethodKind,
    func: &'a ImplItemFn,
    /// Whether the receiver is `&mut self` (vs `&self`). Constructors
    /// have no receiver — we set this to false; it's unused for them.
    mut_receiver: bool,
    /// JS-visible name. Defaults to the Rust identifier; overridden by
    /// `#[v8_name = "..."]` on the method. Lets us install
    /// `delete_(&mut self)` under the JS name `delete`, etc.
    js_name: String,
    /// `#[v8_getter(same_object)]` — WebIDL `[SameObject]` semantics:
    /// the getter must return THE SAME JS object across reads on the
    /// same wrapper instance. The macro caches via a V8 private symbol
    /// keyed by `__zs_same_object_<ClassTy>_<getter>`. User method
    /// returns `v8::Global<v8::Object>` (minted on first call); macro
    /// stashes it on the wrapper instance and returns the cached Local
    /// thereafter. Only meaningful for `MethodKind::Getter`.
    same_object: bool,
}

fn classify(func: &ImplItemFn) -> Option<MethodKind> {
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
fn extract_same_object(attrs: &[Attribute]) -> bool {
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

/// Read `#[v8_name = "literal"]` from a method's attributes. Returns
/// `Some(name)` if present, `None` otherwise. Invalid shapes (non-string
/// literal, list form, etc.) silently fall back to None — the macro
/// then uses the Rust identifier as the JS name.
fn extract_v8_name(attrs: &[Attribute]) -> Option<String> {
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
fn extract_to_string_tag(attrs: &[Attribute]) -> Option<String> {
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
fn extract_reject_shared(attrs: &[Attribute]) -> HashSet<String> {
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

/// Read `#[v8_inherit_intrinsic = "IteratorPrototype"]` from impl-block
/// attributes. Currently only `"IteratorPrototype"` is recognised.
fn extract_inherit_intrinsic(attrs: &[Attribute]) -> Option<String> {
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
fn extract_inherit_base(attrs: &[Attribute]) -> Option<syn::Path> {
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

fn has_mut_self(func: &ImplItemFn) -> bool {
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

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub fn expand(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let input = parse_macro_input!(item as ItemImpl);

    let class_ty = match extract_class_ident(&input.self_ty) {
        Some(t) => t,
        None => {
            return syn::Error::new_spanned(
                &input.self_ty,
                "#[v8_class] requires a plain type, e.g. `impl Headers`",
            )
            .to_compile_error()
            .into();
        }
    };

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
                    .to_compile_error()
                    .into();
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
                    .to_compile_error()
                    .into();
                }

                let same_object_flag =
                    matches!(kind, MethodKind::Getter) && extract_same_object(&func.attrs);

                methods.push(ClassMethod {
                    kind,
                    func,
                    mut_receiver: mut_recv,
                    js_name,
                    same_object: same_object_flag,
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
                .to_compile_error()
                .into();
            }
        }
    }

    let constructor = methods.iter().find(|m| m.kind == MethodKind::Constructor);
    let regular: Vec<&ClassMethod> = methods
        .iter()
        .filter(|m| m.kind != MethodKind::Constructor)
        .collect();

    // Per-method callback fns. Async methods take a different codegen
    // path (spawn a future via `state.spawned_ops` and return a Promise
    // immediately) but install on the prototype identically — async vs
    // sync is opaque to V8. SameObject getters have their own codegen
    // path that wraps the user method with private-symbol caching.
    let callbacks: Vec<TokenStream2> = regular
        .iter()
        .map(|m| match m.kind {
            MethodKind::AsyncMethod => gen_async_method_callback(class_ty, m),
            MethodKind::Getter if m.same_object => gen_same_object_getter_callback(class_ty, m),
            _ => gen_method_callback(class_ty, m),
        })
        .collect();

    let constructor_callback = match constructor {
        Some(c) => gen_constructor_callback(class_ty, c),
        None => gen_default_constructor_callback(class_ty),
    };

    // Impl-block-level overrides for class-wide install behaviour.
    let to_string_tag_override = extract_to_string_tag(&input.attrs);
    let inherit_intrinsic = extract_inherit_intrinsic(&input.attrs);
    let inherit_base = extract_inherit_base(&input.attrs);

    // `#[v8_iterable(key = K, value = V)]` — emit the pair-iterator
    // surface (keys / values / entries / forEach / @@iterator) plus a
    // companion `<Class>Iterator` class.
    let iterable_attr = match v8_iterable::extract_iterable(&input.attrs) {
        Ok(opt) => opt,
        Err(err) => return err.to_compile_error().into(),
    };
    let iterable_codegen = match iterable_attr.as_ref() {
        Some(attr) => match v8_iterable::generate(class_ty, attr) {
            Ok(ts) => ts,
            Err(err) => return err.to_compile_error().into(),
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

    // `Self::install(scope) -> v8::Local<v8::FunctionTemplate>`
    let install = gen_install(
        class_ty,
        &regular,
        constructor.is_some(),
        to_string_tag_override.as_deref(),
        inherit_intrinsic.as_deref(),
        inherit_base.as_ref(),
        install_iterable_call.as_ref(),
    );

    // Strip our marker attributes from the impl items so rustc doesn't
    // see unknown attributes after expansion. Keep everything else.
    let stripped_impl = strip_marker_attrs(input.clone());

    // Per-class isolate-slot marker type. Emitted at module scope (a
    // `pub struct` can't live inside an `impl` block). The macro's
    // generated `Foo::install` reads/writes the slot keyed by this
    // type so repeated calls return the same FunctionTemplate (see
    // `gen_install`'s comment).
    let install_slot_ty = format_ident!("__InstallSlot_{}", class_ty);
    let brand_slot_ty = format_ident!("__BrandSlot_{}", class_ty);
    let brand_check_fn = format_ident!("__brand_check_{}", class_ty);
    let public_is_fn = format_ident!("__zs_is_{}", class_ty);

    let expanded = quote! {
        #stripped_impl

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

        /// Brand-check helper: walks the prototype chain of `this`
        /// looking for the cached `Foo.prototype`. Returns true on
        /// match (the receiver IS a Foo, or a subclass via
        /// `#[v8_inherit]`), false otherwise.
        ///
        /// Walks at most 32 prototype links (deep chains are typically
        /// 1–3 hops; the cap protects against pathologically deep
        /// chains a malicious caller could craft with
        /// `Object.setPrototypeOf` loops). The cost is dwarfed by the
        /// ~100ns V8 callback overhead — the brand check itself is
        /// O(depth) Local pointer comparisons.
        ///
        /// The cached prototype is populated lazily on first call —
        /// NOT in `install` — because eager `get_function(scope)` at
        /// install time would freeze the FunctionTemplate's instance
        /// shape and silently no-op any subsequent
        /// `prototype_template().set_accessor_property(...)` calls.
        /// Several classes (URL.searchParams, etc.) install accessors
        /// on the prototype_template AFTER `Self::install` returns; we
        /// must not break those.
        ///
        /// First-call cost (one-time per isolate): one `get_function`
        /// + one `.get(prototype)`. Steady state: an isolate-slot read
        /// (Rc-clone-shaped) plus the chain walk.
        ///
        /// Lifetimes are elided here on purpose. An explicit `<'s>`
        /// would tie the `Local<Object>` argument's lifetime to the
        /// `&mut PinScope` lifetime in an invariant way (mutable
        /// references are invariant over their type param), which
        /// then conflicts with `args.this()`'s callsite-derived
        /// lifetime. Elision lets each Local pick its own appropriate
        /// (and shorter) lifetime — the helper body never returns a
        /// `Local` so there's no need to relate them outside the
        /// function.
        #[doc(hidden)]
        #[allow(non_snake_case, dead_code)]
        fn #brand_check_fn(
            scope: &mut v8::PinScope,
            obj: v8::Local<v8::Object>,
        ) -> bool {
            // Resolve the cached prototype, lazily populating the
            // brand slot on first call. We can't hold the slot's
            // borrow across `set_slot` (mutable borrow) so we drop
            // it (via `.cloned()` of the Global) before any `set_slot`
            // call.
            let cached_global: v8::Global<v8::Object> =
                if let Some(slot) = scope.get_slot::<#brand_slot_ty>() {
                    slot.0.clone()
                } else {
                    // Lazy fetch from the install slot. If that slot
                    // is missing too, the class wasn't installed in
                    // this isolate — fall through to false.
                    let tmpl_global = match scope.get_slot::<#install_slot_ty>() {
                        Some(s) => s.0.clone(),
                        None => return false,
                    };
                    let tmpl_local = v8::Local::new(scope, &tmpl_global);
                    let func = match tmpl_local.get_function(scope) {
                        Some(f) => f,
                        None => return false,
                    };
                    let proto_key = match v8::String::new(scope, "prototype") {
                        Some(s) => s,
                        None => return false,
                    };
                    let proto_v = match func.get(scope, proto_key.into()) {
                        Some(v) => v,
                        None => return false,
                    };
                    let proto: v8::Local<v8::Object> = match proto_v.try_into() {
                        Ok(o) => o,
                        Err(_) => return false,
                    };
                    let g = v8::Global::new(scope, proto);
                    let g_clone = g.clone();
                    scope.set_slot(#brand_slot_ty(g));
                    g_clone
                };
            let expected_proto: v8::Local<v8::Object> = v8::Local::new(scope, &cached_global);
            // Walk the [[Prototype]] chain. Each `get_prototype` call
            // can return null (chain root) or a Value (potentially an
            // Object). Bail at depth 32 to bound worst-case cost.
            let mut current: v8::Local<v8::Value> = match obj.get_prototype(scope) {
                Some(v) => v,
                None => return false,
            };
            for _ in 0..32 {
                if current.is_null_or_undefined() {
                    return false;
                }
                let cur_obj: v8::Local<v8::Object> = match current.try_into() {
                    Ok(o) => o,
                    Err(_) => return false,
                };
                // V8 Locals compare by handle equality, which matches
                // pointer identity for Persistent-derived Locals. The
                // cached prototype is the exact Object the install
                // captured at first-install time; any genuine `new
                // Foo()` (or instance of a class inheriting Foo) has
                // that Object on its chain.
                if cur_obj == expected_proto {
                    return true;
                }
                current = match cur_obj.get_prototype(scope) {
                    Some(v) => v,
                    None => return false,
                };
            }
            false
        }

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

        #[allow(non_snake_case, dead_code)]
        impl #class_ty {
            #install
        }

        #constructor_callback
        #(#callbacks)*

        // Iterable codegen (when `#[v8_iterable(...)]` is set on the
        // impl block). Emits the companion `<Class>Iterator` struct +
        // its install fn, the four factory callbacks (keys, values,
        // entries, forEach), the iterator's `next()` callback, and a
        // `<Class>::__zs_install_iterable_methods` helper called from
        // `<Class>::install`. No-op when the attribute is absent.
        #iterable_codegen
    };

    expanded.into()
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
            || p.is_ident("v8_iterable"))
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
                    || p.is_ident("v8_name")
                    || p.is_ident("reject_shared"))
            });
        }
    }
    input
}

// ---------------------------------------------------------------------------
// install() codegen
// ---------------------------------------------------------------------------

fn gen_install(
    class_ty: &syn::Ident,
    methods: &[&ClassMethod],
    has_user_constructor: bool,
    to_string_tag_override: Option<&str>,
    inherit_intrinsic: Option<&str>,
    inherit_base: Option<&syn::Path>,
    install_iterable_call: Option<&TokenStream2>,
) -> TokenStream2 {
    let class_name_str = class_ty.to_string();
    let constructor_callback_ident = format_ident!("__{}_constructor_callback", class_ty);

    // Per-method prototype installation lines.
    //
    // Accessors with matching JS names (one getter + one setter) are
    // combined into a single `set_accessor_property` call with both
    // templates. V8 rejects two separate calls for the same key —
    // each call replaces the previous one's slot, so the second call
    // wipes out the first. The pair-detection runs over the full
    // method list to find getter↔setter siblings.
    //
    // Build a name → (getter_cb?, setter_cb?, sample_method) map so we
    // can short-circuit duplicate emit cycles.
    let mut accessor_pairs: HashMap<String, (Option<TokenStream2>, Option<TokenStream2>)> =
        HashMap::new();
    for m in methods {
        if matches!(m.kind, MethodKind::Getter | MethodKind::Setter) {
            let name = &m.func.sig.ident;
            let cb = method_callback_ident(class_ty, name);
            let entry = accessor_pairs.entry(m.js_name.clone()).or_default();
            match m.kind {
                MethodKind::Getter => entry.0 = Some(quote! { #cb }),
                MethodKind::Setter => entry.1 = Some(quote! { #cb }),
                _ => {}
            }
        }
    }

    // Track which accessor names have been emitted so we don't emit
    // them twice (once per ClassMethod entry).
    let mut emitted_accessors: HashSet<String> = HashSet::new();

    let proto_sets: Vec<TokenStream2> = methods
        .iter()
        .filter_map(|m| {
            let js_name = m.js_name.clone();
            match m.kind {
                MethodKind::Method | MethodKind::AsyncMethod => {
                    // Async vs sync is opaque to V8 — async methods
                    // return a Promise from a sync callback, so they
                    // install on the prototype identically.
                    let name = &m.func.sig.ident;
                    let cb = method_callback_ident(class_ty, name);
                    Some(quote! {
                        {
                            let __key = v8::String::new(scope, #js_name).unwrap();
                            let __fn_tmpl = v8::FunctionTemplate::new(scope, #cb);
                            __proto.set(__key.into(), __fn_tmpl.into());
                        }
                    })
                }
                MethodKind::Getter | MethodKind::Setter => {
                    if !emitted_accessors.insert(js_name.clone()) {
                        return None;
                    }
                    let pair = accessor_pairs.get(&js_name);
                    let (getter_opt, setter_opt) = pair.cloned().unwrap_or_default();
                    let getter_tokens = match getter_opt {
                        Some(cb) => quote! {
                            let __getter_tmpl = v8::FunctionTemplate::new(scope, #cb);
                            let __getter_arg: Option<v8::Local<v8::FunctionTemplate>> = Some(__getter_tmpl);
                        },
                        None => quote! {
                            let __getter_arg: Option<v8::Local<v8::FunctionTemplate>> = None;
                        },
                    };
                    let setter_tokens = match setter_opt {
                        Some(cb) => quote! {
                            let __setter_tmpl = v8::FunctionTemplate::new(scope, #cb);
                            let __setter_arg: Option<v8::Local<v8::FunctionTemplate>> = Some(__setter_tmpl);
                        },
                        None => quote! {
                            let __setter_arg: Option<v8::Local<v8::FunctionTemplate>> = None;
                        },
                    };
                    Some(quote! {
                        {
                            let __key = v8::String::new(scope, #js_name).unwrap();
                            #getter_tokens
                            #setter_tokens
                            __proto.set_accessor_property(
                                __key.into(),
                                __getter_arg,
                                __setter_arg,
                                v8::PropertyAttribute::NONE,
                            );
                        }
                    })
                }
                MethodKind::Constructor => None,
            }
        })
        .collect();

    // If there's no user-defined constructor, still emit one that
    // default-constructs `Self`. Requires `Self: Default`.
    let _user_ctor_marker = has_user_constructor;

    // The literal that goes into Symbol.toStringTag. Defaults to the
    // Rust struct name; overridden by `#[v8_to_string_tag = "..."]`.
    let to_string_tag_str = to_string_tag_override
        .map(str::to_string)
        .unwrap_or_else(|| class_name_str.clone());

    // Optional prototype-chain link to a V8 built-in intrinsic.
    // Currently only `"IteratorPrototype"` is wired. Implementation
    // strategy: after build-time, the install call has access to an
    // active context (downstream test/setup_globals already operate
    // inside one). We compile and run a tiny JS snippet that grabs
    // `%Iterator.prototype%` (the prototype-of-prototype of any
    // built-in iterator like `[][Symbol.iterator]()`) and applies it
    // to our prototype via `Object.setPrototypeOf`.
    //
    // This is the minimal correct implementation per WebIDL §3.7.10.2
    // (default iterator [[Prototype]] = %Iterator.prototype%). V8
    // exposes `Intrinsic::IteratorPrototype` only through
    // `Template::set_intrinsic_data_property`, which would install
    // it AS a named property — wrong shape. Direct prototype-set via
    // JS is the documented Deno/Cloudflare workaround.
    let inherit_block = match inherit_intrinsic {
        None => quote! {},
        Some("IteratorPrototype") => quote! {
            // After get_function() the prototype object exists in the
            // current context. Walk to %Iterator.prototype% and chain.
            {
                let __ctor_fn = __ctor_tmpl.get_function(scope).unwrap();
                let __proto_key = v8::String::new(scope, "prototype").unwrap();
                let __ctor_proto_v = __ctor_fn.get(scope, __proto_key.into()).unwrap();
                let __ctor_proto: v8::Local<v8::Object> = __ctor_proto_v.try_into().unwrap();
                // %IteratorPrototype% via getPrototypeOf(getPrototypeOf([][Symbol.iterator]())).
                let __js = v8::String::new(
                    scope,
                    "Object.getPrototypeOf(Object.getPrototypeOf([][Symbol.iterator]()))",
                ).unwrap();
                let __script = v8::Script::compile(scope, __js, None).unwrap();
                let __iter_proto = __script.run(scope).unwrap();
                __ctor_proto.set_prototype(scope, __iter_proto);
            }
        },
        Some(other) => {
            let msg = format!(
                "#[v8_inherit_intrinsic]: unrecognised value `{other}` (expected \"IteratorPrototype\")"
            );
            quote! { compile_error!(#msg); }
        }
    };

    // `#[v8_inherit(BaseClass)]` plumbs prototype-chain inheritance —
    // e.g. AbortSignal : EventTarget per DOM §3.3 — by calling
    // `FunctionTemplate::inherit(parent_tmpl)` BEFORE the prototype
    // template is touched. The parent's template MUST be the same
    // FunctionTemplate object the parent was registered with
    // globally; otherwise `signal instanceof EventTarget === false`
    // because the [[FunctionPrototype]] chain points at the parent's
    // first template while the global `EventTarget` is bound to the
    // first one. We therefore call the parent's `install` (which
    // caches its own template per isolate via `__cached_install`)
    // and trust it to return the same Local on every call within a
    // single isolate.
    //
    // We call `inherit` AFTER `set_class_name` and BEFORE adding our
    // own prototype methods so the chain is established before any
    // method/getter/setter sets are layered on. Internal-field count
    // is set on the instance template separately and is independent
    // of inheritance.
    let inherit_base_block = match inherit_base {
        None => quote! {},
        Some(base) => quote! {
            {
                let __base_tmpl = <#base>::install(scope);
                __ctor_tmpl.inherit(__base_tmpl);
            }
        },
    };

    // Per-class isolate-slot marker. Holds the cached FunctionTemplate
    // as a `v8::Global<v8::FunctionTemplate>` so repeated `install`
    // calls (e.g. from a derived class's `#[v8_inherit]` codegen) see
    // the exact same template object — required for V8's instanceof
    // check (which compares the underlying [[FunctionPrototype]] by
    // identity) and for the global `globalThis.Foo` to match
    // `Foo.prototype` of `new Foo()` via the prototype chain.
    //
    // The slot type is private to this class's expansion (named after
    // the class to avoid TypeId collisions across classes). The first
    // call writes the slot; subsequent calls in the same isolate
    // return the cached `Local` reborrow.
    let install_slot_ty = format_ident!("__InstallSlot_{}", class_ty);

    quote! {
        /// Install this class on the given V8 scope, returning the
        /// FunctionTemplate. The runtime calls this from
        /// `setup_globals` and uses the returned template to attach
        /// the class as a global (e.g. `globalThis.Headers`).
        ///
        /// Idempotent per isolate: subsequent calls return the same
        /// FunctionTemplate (looked up via an isolate slot keyed by
        /// the per-class `__InstallSlot_<Class>` marker type emitted
        /// by `#[v8_class]` at module scope). This is what makes
        /// `#[v8_inherit]` work — derived classes resolve the parent's
        /// template by calling the parent's install, which returns
        /// the cached template on the second call (the first call
        /// typically being the runtime's own register-as-global step).
        pub fn install<'s>(
            scope: &mut v8::PinScope<'s, '_>,
        ) -> v8::Local<'s, v8::FunctionTemplate> {
            // Hot path: template already cached for this isolate.
            if let Some(cached) = scope.get_slot::<#install_slot_ty>() {
                return v8::Local::new(scope, cached.0.clone());
            }

            let __ctor_tmpl = v8::FunctionTemplate::new(scope, #constructor_callback_ident);
            let __class_name = v8::String::new(scope, #class_name_str).unwrap();
            __ctor_tmpl.set_class_name(__class_name);

            // `#[v8_inherit(BaseClass)]` — establish the prototype chain
            // BEFORE we layer our own prototype properties on top.
            #inherit_base_block

            // Reserve one internal field to hold the boxed Rust state.
            __ctor_tmpl
                .instance_template(scope)
                .set_internal_field_count(1);

            let __proto = __ctor_tmpl.prototype_template(scope);
            #(#proto_sets)*

            // `#[v8_iterable(...)]` — install keys / values / entries /
            // forEach / @@iterator on the prototype template. The
            // companion `<Class>Iterator` class is emitted at module
            // scope (see `iterable_codegen`) and the install call here
            // wires its factories onto the parent's proto.
            #install_iterable_call

            // Install Symbol.toStringTag so
            // `Object.prototype.toString.call(new Foo())` → "[object Foo]".
            // Per WebIDL §3.7.4 the descriptor must be
            //   { writable: false, enumerable: false, configurable: true }.
            // PropertyAttribute flags: READ_ONLY = !writable, DONT_ENUM
            // = !enumerable. Configurable is the absence of DONT_DELETE.
            // Default is the Rust struct name; `#[v8_to_string_tag = "…"]`
            // overrides it (e.g. "Headers Iterator" for default iterators).
            {
                let __tag_sym = v8::Symbol::get_to_string_tag(scope);
                let __tag_value = v8::String::new(scope, #to_string_tag_str).unwrap();
                __proto.set_with_attr(
                    __tag_sym.into(),
                    __tag_value.into(),
                    v8::PropertyAttribute::READ_ONLY | v8::PropertyAttribute::DONT_ENUM,
                );
            }

            #inherit_block

            // Cache the FunctionTemplate for this isolate. Future
            // `install` calls return the same Local — required for
            // `#[v8_inherit]` to chain derived classes onto the same
            // prototype.
            //
            // The brand-check prototype is captured LAZILY on first
            // brand check (see `__brand_check_<ClassTy>`) rather than
            // here, because eagerly calling `get_function(scope)` at
            // install time freezes the FunctionTemplate's instance
            // shape — any subsequent `prototype_template()
            // .set_accessor_property(...)` from outside `install`
            // would silently no-op. URL hand-installs `searchParams`
            // on the prototype_template right after `URL::install`
            // returns; we must not break that.
            let __global = ::v8::Global::new(scope, __ctor_tmpl);
            let __local = ::v8::Local::new(scope, __global.clone());
            scope.set_slot(#install_slot_ty(__global));
            __local
        }
    }
}

// ---------------------------------------------------------------------------
// Method callback codegen
// ---------------------------------------------------------------------------

fn method_callback_ident(class_ty: &syn::Ident, method: &syn::Ident) -> syn::Ident {
    format_ident!("__{}_{}_callback", class_ty, method)
}

/// Re-entry guard for `&mut self` methods.
///
/// **Problem.** A `&mut self` method recovers `&mut Self` from the
/// External pointer in internal field 0. If the user body calls back
/// into JS (e.g. `Local<Function>::call`, fired-event handler) and the
/// callback synchronously re-enters the SAME instance via the prototype,
/// the macro materialises ANOTHER `&mut Self` pointing at the same Box.
/// That's aliased mutable references — UB. Pre-fix, the symptom was a
/// cryptic `RefCell already mutably borrowed` panic from deep inside V8
/// when the user's body wrapped state in an inner `RefCell`; classes
/// without an inner cell silently corrupted memory.
///
/// **Fix.** A per-method, thread-local `RefCell<HashSet<usize>>` keyed
/// by the External pointer's address (`__ext.value() as usize` ==
/// the Box raw addr). The prologue inserts the addr on entry; if it
/// was already present, throws a V8 TypeError with a clear, per-method
/// message and returns from the callback BEFORE the unsafe `&mut Self`
/// materialisation. A RAII drop guard removes the addr on scope exit so
/// even a panic in the user body releases the entry.
///
/// We throw a V8 TypeError (not a Rust panic) because Rust's panic
/// runtime can't unwind through V8's C++ frames cleanly — the
/// experimental result on Linux is "fatal runtime error: failed to
/// initiate panic, error 5" + SIGABRT. A V8 exception propagates the
/// way every other macro-emitted error already does (see brand check
/// "Illegal invocation"), so the user code observes a JS-side
/// `TypeError` with the diagnostic message. That's still WAY clearer
/// than a cryptic RefCell-borrow panic from inside V8.
///
/// Per-method (one set per `Foo::method`) AND per-instance (key on the
/// Box addr) — no false positives across distinct instances or
/// distinct methods. Thread-local — no cross-thread cost.
///
/// Cost: one HashSet `insert` + one `remove` per `&mut self` call.
/// The set has 0 or 1 entries in the steady state (re-entry is
/// pathological, not common).
///
/// Emitted ONLY for `&mut self` methods. `&self` callbacks are sound
/// to nest (multiple aliased shared references are fine) and skip the
/// guard entirely.
///
/// Returns a token stream that:
///   1. Computes `__inflight_addr = __ext.value() as usize`.
///   2. Tries to insert into the per-method thread-local set; throws
///      a V8 TypeError + `return`s if already present.
///   3. Defines a `Drop`-impl shim that removes the addr.
///   4. Binds the shim instance to a let so it lives until scope end.
///
/// The caller must run this AFTER the External recovery and BEFORE
/// the unsafe `&mut Self` materialisation.
fn gen_reentry_guard(
    class_ty: &syn::Ident,
    method_name: &syn::Ident,
    is_mut_self: bool,
) -> TokenStream2 {
    if !is_mut_self {
        return quote! {};
    }
    let err_msg = format!(
        "re-entered method `{}::{}` on instance — concurrent &mut self callback",
        class_ty, method_name,
    );
    // Use ONE thread_local per method per class. The static names are
    // local to the callback function so they don't pollute the impl
    // block's namespace and don't collide across methods.
    quote! {
        let __inflight_addr = __ext.value() as usize;
        ::std::thread_local! {
            static __INFLIGHT: ::std::cell::RefCell<::std::collections::HashSet<usize>> =
                ::std::cell::RefCell::new(::std::collections::HashSet::new());
        }
        let __already_inflight = __INFLIGHT.with(|__s| !__s.borrow_mut().insert(__inflight_addr));
        if __already_inflight {
            // Throw a V8 TypeError with the diagnostic message. We
            // can't `panic!` here because Rust panic can't unwind
            // through V8's C++ frames (SIGABRT on Linux). A V8
            // exception propagates correctly and surfaces in user JS
            // as a TypeError, which is way clearer than the pre-fix
            // cryptic RefCell-already-mutably-borrowed panic.
            let __msg = v8::String::new(scope, #err_msg).unwrap();
            let __exc = v8::Exception::type_error(scope, __msg);
            scope.throw_exception(__exc);
            return;
        }
        // RAII guard: remove the addr on scope exit so any path out
        // (normal return, V8 exception thrown by user code, …)
        // releases the entry. Without this, a single throw would
        // leave the set "occupied" and every subsequent call would
        // incorrectly trigger the guard.
        struct __ReentryGuard(usize);
        impl ::std::ops::Drop for __ReentryGuard {
            fn drop(&mut self) {
                __INFLIGHT.with(|__s| {
                    __s.borrow_mut().remove(&self.0);
                });
            }
        }
        let __reentry_guard = __ReentryGuard(__inflight_addr);
    }
}

fn gen_method_callback(class_ty: &syn::Ident, m: &ClassMethod) -> TokenStream2 {
    let method_name = &m.func.sig.ident;
    let callback_name = method_callback_ident(class_ty, method_name);

    // Skip the receiver param when extracting JS args.
    let params = parse_params_skipping_self(m.func);
    let reject_shared_names = extract_reject_shared(&m.func.attrs);
    let extractions = gen_param_extractions(&params, &reject_shared_names);

    let call_args: Vec<&syn::Ident> = params.iter().map(|p| &p.name).collect();
    let receiver_ref = if m.mut_receiver {
        quote! { &mut *__instance }
    } else {
        quote! { &*__instance }
    };

    let call = match m.kind {
        MethodKind::Setter => {
            // Setters in V8 are called with one positional arg (the value).
            // We don't emit return marshaling — accessor setters discard.
            return gen_setter_callback(class_ty, m);
        }
        _ => quote! {
            <#class_ty>::#method_name(#receiver_ref, #(#call_args),*)
        },
    };

    let call_return = gen_call_return(&call, &m.func.sig.output);

    let getter_args = if m.kind == MethodKind::Getter {
        // V8 getters use AccessorCallback signature; we use FunctionTemplate
        // for parity with methods, so the args object is still passed.
        quote! {}
    } else {
        quote! {}
    };

    let _ = getter_args;

    let brand_check_fn = format_ident!("__brand_check_{}", class_ty);
    let reentry_guard = gen_reentry_guard(class_ty, method_name, m.mut_receiver);

    quote! {
        #[allow(non_snake_case, unused_variables, unused_mut, clippy::needless_borrow)]
        pub(crate) fn #callback_name(
            scope: &mut v8::PinScope,
            args: v8::FunctionCallbackArguments,
            mut rv: v8::ReturnValue,
        ) {
            // WebIDL §3.7 brand check: walk the prototype chain
            // looking for the cached `Foo.prototype`. If absent, the
            // receiver isn't a Foo (or a Foo subclass) — throwing
            // "Illegal invocation" is mandatory before the unsafe
            // internal-field deref. See `__brand_check_<ClassTy>`'s
            // doc-comment for the soundness rationale.
            let __this = args.this();
            if !#brand_check_fn(scope, __this) {
                let __msg = v8::String::new(scope, "Illegal invocation").unwrap();
                let __exc = v8::Exception::type_error(scope, __msg);
                scope.throw_exception(__exc);
                return;
            }
            // Brand check passed: internal field 0 is guaranteed to
            // hold a `Box<#class_ty>` raw pointer (set in
            // `gen_box_and_install_finalizer`). Recover the External.
            let __ext = match __this.get_internal_field(scope, 0)
                .and_then(|v| v8::Local::<v8::External>::try_from(v).ok())
            {
                Some(e) => e,
                None => {
                    let __msg = v8::String::new(scope, "Illegal invocation").unwrap();
                    let __exc = v8::Exception::type_error(scope, __msg);
                    scope.throw_exception(__exc);
                    return;
                }
            };
            // Re-entry guard for `&mut self` (no-op for `&self`). MUST
            // run AFTER External recovery (we need the addr) and BEFORE
            // the unsafe `&mut Self` materialisation (or we'd UB through
            // an aliased pointer before the guard could fire).
            #reentry_guard
            let __instance = unsafe { &mut *(__ext.value() as *mut #class_ty) };

            #(#extractions)*
            #call_return
        }
    }
}

/// Codegen for `#[v8_getter(same_object)]` — WebIDL `[SameObject]`
/// semantics.
///
/// `Request.headers`, `Response.headers`, `URL.searchParams`, and
/// several other WebIDL accessors must return THE SAME JS object across
/// reads on the same instance:
///
/// ```js
/// const h = req.headers;
/// h === req.headers;   // true
/// h === req.headers;   // still true (no fresh object minted)
/// ```
///
/// Without caching, each access would mint a fresh wrapper, breaking
/// userland code that uses `===` identity (e.g. comparing iterators,
/// caching the headers reference, etc.).
///
/// Implementation strategy:
///
/// - Cache on a per-instance V8 Private symbol named
///   `__zs_same_object_<ClassTy>_<getter>`. The symbol is class-scoped
///   so two classes with `headers` getters don't collide on a single
///   shared name (interning of Privates by name across the isolate is
///   irrelevant since reads/writes are per-Object — but the explicit
///   class-prefix is self-documenting).
///
/// - On callback entry: brand check; recover the boxed instance; look
///   up the private symbol on `args.this()`. If present and not
///   `undefined`, return it as the rv and short-circuit (no user
///   method called).
///
/// - On cache miss: invoke the user's `&self`/`&mut self` method,
///   which returns a `v8::Global<v8::Object>`. Convert to Local,
///   stash on the wrapper instance via `set_private`, return the
///   Local as rv.
///
/// User method shape:
/// ```ignore
/// #[v8_getter(same_object)]
/// fn headers(&self, scope: &mut v8::PinScope) -> v8::Global<v8::Object> {
///     // mint and return — invoked at most ONCE per instance lifetime.
/// }
/// ```
///
/// The user method receives a synthetic `&mut PinScope` (so it can
/// build the Object) and returns `Global<Object>`. The macro doesn't
/// pass any positional JS args (getters take none) — the user method
/// can have only `&self` (or `&mut self`) and the optional `scope`
/// param.
///
/// We don't migrate existing classes to this attribute in this PR
/// (Request.headers, Response.headers, URL.searchParams continue to
/// hand-roll their own private-symbol stash for now). The smoke test
/// in `tests/v8_same_object_smoke.rs` proves the macro wiring works.
fn gen_same_object_getter_callback(class_ty: &syn::Ident, m: &ClassMethod) -> TokenStream2 {
    let method_name = &m.func.sig.ident;
    let callback_name = method_callback_ident(class_ty, method_name);

    // Skip the receiver param when extracting JS args. Getters take
    // no positional args; the only param shape we expect is `&self`
    // (+ optional synthetic `scope`). Extractions are emitted but
    // typically empty.
    let params = parse_params_skipping_self(m.func);
    let reject_shared_names = extract_reject_shared(&m.func.attrs);
    let extractions = gen_param_extractions(&params, &reject_shared_names);
    let call_args: Vec<&syn::Ident> = params.iter().map(|p| &p.name).collect();
    let receiver_ref = if m.mut_receiver {
        quote! { &mut *__instance }
    } else {
        quote! { &*__instance }
    };

    let brand_check_fn = format_ident!("__brand_check_{}", class_ty);
    let private_name = format!("__zs_same_object_{}_{}", class_ty, method_name);
    let reentry_guard = gen_reentry_guard(class_ty, method_name, m.mut_receiver);

    quote! {
        #[allow(non_snake_case, unused_variables, unused_mut, clippy::needless_borrow)]
        pub(crate) fn #callback_name(
            scope: &mut v8::PinScope,
            args: v8::FunctionCallbackArguments,
            mut rv: v8::ReturnValue,
        ) {
            // 1. Brand check before touching internal fields. Same
            //    contract as every other generated callback — see
            //    `__brand_check_<ClassTy>`'s doc-comment.
            let __this = args.this();
            if !#brand_check_fn(scope, __this) {
                let __msg = v8::String::new(scope, "Illegal invocation").unwrap();
                let __exc = v8::Exception::type_error(scope, __msg);
                scope.throw_exception(__exc);
                return;
            }

            // 2. Resolve the per-instance Private symbol for this
            //    getter. `Private::for_api` is interned by name across
            //    the isolate, so the lookup is O(1) after the first
            //    call — V8 returns the same symbol object on repeat
            //    reads with the same name.
            let __key_str = v8::String::new(scope, #private_name).unwrap();
            let __priv = v8::Private::for_api(scope, Some(__key_str));

            // 3. Cache hit short-circuit: if the wrapper has already
            //    minted a Same-Object value, return it without calling
            //    user code. `get_private` returns Some(undefined) when
            //    the slot was never written, so we filter both None
            //    and undefined paths.
            if let Some(__cached) = __this.get_private(scope, __priv) {
                if !__cached.is_undefined() {
                    rv.set(__cached);
                    return;
                }
            }

            // 4. Cache miss: recover Box<Self>, mint the value, stash,
            //    return.
            let __ext = match __this.get_internal_field(scope, 0)
                .and_then(|v| v8::Local::<v8::External>::try_from(v).ok())
            {
                Some(e) => e,
                None => {
                    let __msg = v8::String::new(scope, "Illegal invocation").unwrap();
                    let __exc = v8::Exception::type_error(scope, __msg);
                    scope.throw_exception(__exc);
                    return;
                }
            };
            // Re-entry guard for `&mut self` SameObject getters. The
            // miss path runs the user method exactly once; if that body
            // re-enters the same instance (e.g. through a JS callback
            // it triggers), the second call would alias `&mut Self`.
            // No-op for the `&self` case (the common shape).
            #reentry_guard
            let __instance = unsafe { &mut *(__ext.value() as *mut #class_ty) };

            #(#extractions)*

            // The user method returns a `v8::Global<v8::Object>` — we
            // own it after the call returns, so we can both stash it
            // (by re-Localising) and use the same Local for the rv.
            let __value: ::v8::Global<::v8::Object> =
                <#class_ty>::#method_name(#receiver_ref, #(#call_args),*);
            let __local: ::v8::Local<::v8::Object> = ::v8::Local::new(scope, &__value);

            // Stash on the wrapper. `set_private` is fallible (returns
            // None on context teardown); we ignore the result — the
            // worst case is the cache stays empty and the user method
            // runs again, which is observable but not unsound. The
            // user method should be idempotent on its own state for
            // the same reason.
            let _ = __this.set_private(scope, __priv, __local.into());

            rv.set(__local.into());
        }
    }
}

/// Codegen for `#[v8_async_method]` — emits a sync V8 callback that
/// allocates a Promise, spawns the user's async body via
/// `state.spawned_ops`, and returns the Promise immediately. The pump
/// resolves (or rejects) the promise when the future settles.
///
/// Shape of the emitted callback:
/// ```ignore
/// fn __Foo_method_callback(scope, args, rv) {
///     // 1. Resolve `this` → Box<Foo> via internal field 0.
///     let raw_self_addr = ...;
///     // 2. Extract JS args (using existing gen_param_extractions).
///     let arg_0 = ...;
///     // 3. Allocate resolver + capture Globals.
///     let resolver = v8::PromiseResolver::new(scope).unwrap();
///     let promise = resolver.get_promise(scope);
///     let resolver_global = v8::Global::new(scope, resolver);
///     let wrapper_global = v8::Global::new(scope, args.this());
///     // 4. Pull SharedState off the isolate slot.
///     let state = scope.get_slot::<SharedState>().unwrap().clone();
///     let request_id = state.borrow().executing_request_id;
///     // 5. Build the future.
///     let fut = async move {
///         let _keepalive = wrapper_global; // pin Box<Foo> across .await
///         // SAFETY: see emitted comment.
///         let this: &Foo = unsafe { &*(raw_self_addr as *mut Foo) };
///         let result = this.method(arg_0).await;
///         OpResult::JsValue {
///             resolver: resolver_global,
///             value: result.into_resolve_value(),
///             request_id,
///         }
///     };
///     // 6. Push to spawned_ops + wake pump.
///     state.borrow_mut().spawned_ops.push(Box::pin(fut));
///     if let Some(mut tx) = state.borrow().pump_notify_tx.clone() {
///         let _ = tx.try_send(());
///     }
///     // 7. Return promise.
///     rv.set(promise.into());
/// }
/// ```
///
/// Borrow-safety contract for the emitted code:
///   - `wrapper_global` is captured by value into the future. As long as
///     the future has not dropped, the V8 wrapper Object is reachable;
///     therefore the GC-finalizer that drops the boxed instance cannot
///     fire. The `*mut Self` recovered each poll is valid for the
///     future's lifetime.
///   - The macro REJECTS `&mut self` async methods (see `expand`); the
///     re-acquired pointer is always taken as `&Self`, so two
///     simultaneous polls (or re-entry from a microtask) cannot
///     materialise an aliased `&mut Self`. State that needs to mutate
///     must use `Cell` / `RefCell` — the user's responsibility, not
///     the macro's.
///   - All future captures are owned (`Vec<u8>`, `String`, `Global<…>`,
///     scalar), never borrowed. The future is `'static + !Send`, which
///     matches the single-thread compio invariant.
fn gen_async_method_callback(class_ty: &syn::Ident, m: &ClassMethod) -> TokenStream2 {
    let method_name = &m.func.sig.ident;
    let callback_name = method_callback_ident(class_ty, method_name);

    // Skip the receiver param when extracting JS args.
    let params = parse_params_skipping_self(m.func);
    let reject_shared_names = extract_reject_shared(&m.func.attrs);
    let extractions = gen_param_extractions(&params, &reject_shared_names);

    let call_args: Vec<&syn::Ident> = params.iter().map(|p| &p.name).collect();
    let brand_check_fn = format_ident!("__brand_check_{}", class_ty);

    quote! {
        #[allow(non_snake_case, unused_variables, unused_mut, clippy::needless_borrow)]
        pub(crate) fn #callback_name(
            scope: &mut v8::PinScope,
            args: v8::FunctionCallbackArguments,
            mut rv: v8::ReturnValue,
        ) {
            // 1. Recover the `Box<Self>` pointer from internal field 0.
            //    On illegal invocation (receiver is not a wrapper), fail
            //    *synchronously* with a TypeError — same contract as the
            //    sync method path. The user code never runs. The brand
            //    check (WebIDL §3.7) walks the prototype chain rather
            //    than just verifying internal-field 0 is an External,
            //    so cross-class calls (`Foo.prototype.method.call(bar)`)
            //    fail before the unsafe deref.
            let __this = args.this();
            if !#brand_check_fn(scope, __this) {
                let __msg = v8::String::new(scope, "Illegal invocation").unwrap();
                let __exc = v8::Exception::type_error(scope, __msg);
                scope.throw_exception(__exc);
                return;
            }
            let __ext = match __this.get_internal_field(scope, 0)
                .and_then(|v| v8::Local::<v8::External>::try_from(v).ok())
            {
                Some(e) => e,
                None => {
                    let __msg = v8::String::new(scope, "Illegal invocation").unwrap();
                    let __exc = v8::Exception::type_error(scope, __msg);
                    scope.throw_exception(__exc);
                    return;
                }
            };
            // Cast to usize so the future capture doesn't carry a raw
            // pointer (Rust treats `*mut T` as !Send/!Sync; the future
            // is single-thread either way, but cleaner to launder).
            let __raw_addr: usize = __ext.value() as usize;

            // 2. Extract JS args. Uses the same shared logic as sync
            //    methods so type extraction (Vec<u8>, ByteString,
            //    Option<String>, …) is identical across sync/async.
            //    These run BEFORE we move out of `scope` for the
            //    resolver allocation, matching the sync convention.
            #(#extractions)*

            // 3. Allocate the Promise + capture Globals to bridge into
            //    the future. `wrapper_global` keeps the Box<Self>
            //    alive: as long as `wrapper_global` lives in the
            //    future capture, V8 cannot finalise the wrapper, so
            //    the Box behind `__raw_addr` stays valid across every
            //    poll of the future.
            let __resolver = v8::PromiseResolver::new(scope).unwrap();
            let __promise = __resolver.get_promise(scope);
            let __resolver_global = v8::Global::new(scope, __resolver);
            let __wrapper_global = v8::Global::new(scope, __this);

            // 4. Pull SharedState off the isolate slot. Cloned `Rc`,
            //    cheap. The future captures another clone; the
            //    callback can drop its handle freely.
            let __state: ::zeroship_runtime::state::SharedState = scope
                .get_slot::<::zeroship_runtime::state::SharedState>()
                .expect("RuntimeState not in isolate slot")
                .clone();
            let __request_id = __state.borrow().executing_request_id;

            // 5. Build the future. The block keeps `wrapper_global`
            //    alive for the future's full lifetime (as the
            //    `_keepalive` binding) so the JS wrapper stays
            //    reachable even if no user JS holds a reference.
            //    Re-acquiring `&Self` per poll is safe because:
            //      a) the macro rejects `&mut self` async (see
            //         `expand`), so no aliased `&mut` can exist;
            //      b) the wrapper Global pins the Box.
            let __fut = async move {
                let _keepalive = __wrapper_global;
                // SAFETY: __raw_addr was Box::into_raw'd from
                // Box<#class_ty> at construction time; the keepalive
                // Global pins that allocation for as long as this
                // future hasn't dropped. The macro's `expand` rejects
                // `&mut self` async, so a `&Self` borrow is the only
                // shape the user method takes — no aliasing risk
                // even under V8 re-entry from microtasks.
                let __instance: &#class_ty = unsafe { &*(__raw_addr as *mut #class_ty) };
                let __result = <#class_ty>::#method_name(__instance, #(#call_args),*).await;
                let __value = ::zeroship_runtime::state::IntoResolveValue::into_resolve_value(__result);
                ::zeroship_runtime::state::OpResult::JsValue {
                    resolver: __resolver_global,
                    value: __value,
                    request_id: __request_id,
                }
            };

            // 6. Push to the runtime's spawned_ops queue. The pump
            //    polls these futures on every event-loop tick;
            //    settling produces the OpResult::JsValue that the
            //    pump matches into `r.resolve(scope, …)`.
            __state.borrow_mut().spawned_ops.push(::std::boxed::Box::pin(__fut));
            // Wake the pump so streaming / cross-task spawns settle
            // promptly. Mirrors `fetch_native::fetch_callback`'s
            // notify shape.
            let __notify = __state.borrow().pump_notify_tx.clone();
            if let Some(mut __tx) = __notify {
                let _ = __tx.try_send(());
            }

            // 7. Return the unsettled Promise. JS sees this as the
            //    method's return value and `await`s on it.
            rv.set(__promise.into());
        }
    }
}

fn gen_setter_callback(class_ty: &syn::Ident, m: &ClassMethod) -> TokenStream2 {
    let method_name = &m.func.sig.ident;
    let callback_name = method_callback_ident(class_ty, method_name);

    // Setters take exactly one logical param: the new value.
    let params = parse_params_skipping_self(m.func);
    let reject_shared_names = extract_reject_shared(&m.func.attrs);
    let extractions = gen_param_extractions(&params, &reject_shared_names);
    let call_args: Vec<&syn::Ident> = params.iter().map(|p| &p.name).collect();
    let receiver_ref = if m.mut_receiver {
        quote! { &mut *__instance }
    } else {
        quote! { &*__instance }
    };
    let brand_check_fn = format_ident!("__brand_check_{}", class_ty);
    let reentry_guard = gen_reentry_guard(class_ty, method_name, m.mut_receiver);

    quote! {
        #[allow(non_snake_case, unused_variables, unused_mut, clippy::needless_borrow)]
        pub(crate) fn #callback_name(
            scope: &mut v8::PinScope,
            args: v8::FunctionCallbackArguments,
            mut rv: v8::ReturnValue,
        ) {
            // WebIDL §3.7 brand check — see method-callback prologue
            // for the soundness rationale.
            let __this = args.this();
            if !#brand_check_fn(scope, __this) {
                let __msg = v8::String::new(scope, "Illegal invocation").unwrap();
                let __exc = v8::Exception::type_error(scope, __msg);
                scope.throw_exception(__exc);
                return;
            }
            let __ext = match __this.get_internal_field(scope, 0)
                .and_then(|v| v8::Local::<v8::External>::try_from(v).ok())
            {
                Some(e) => e,
                None => {
                    let __msg = v8::String::new(scope, "Illegal invocation").unwrap();
                    let __exc = v8::Exception::type_error(scope, __msg);
                    scope.throw_exception(__exc);
                    return;
                }
            };
            // Re-entry guard for `&mut self` setters. See
            // `gen_reentry_guard` doc-comment for the contract.
            #reentry_guard
            let __instance = unsafe { &mut *(__ext.value() as *mut #class_ty) };

            #(#extractions)*

            // Discard return — setters don't propagate values.
            let _ = <#class_ty>::#method_name(#receiver_ref, #(#call_args),*);
        }
    }
}

// ---------------------------------------------------------------------------
// Constructor callback codegen
// ---------------------------------------------------------------------------

/// `#[v8_constructor(...)]` opt-out for the must-new check — currently
/// unused (no class today wants `Foo()` without `new` to succeed), but
/// retained as a hook for future legacy-callable shapes (a few WebIDL
/// interfaces are spec'd with `[LegacyFactoryFunction]`, e.g.
/// `Image()`). When `callable_no_new` is present the macro skips the
/// `is_construct_call` guard.
fn extract_callable_no_new(attrs: &[Attribute]) -> bool {
    for attr in attrs {
        if !attr.path().is_ident("v8_constructor") {
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
                if id == "callable_no_new" {
                    return true;
                }
            }
        }
    }
    false
}

/// WebIDL §3.7.1: every interface constructor MUST be called with `new`.
/// Returns the `if !args.is_construct_call() { throw TypeError; return; }`
/// prologue unless the class opts out via `#[v8_constructor(callable_no_new)]`.
///
/// Class-name interpolation in the message (e.g. `"Constructor Headers
/// requires 'new'"`) lets WPT diagnose mistakes per-class. The
/// `is_construct_call` flag is V8-native — it differentiates `new Foo()`
/// (true) from `Foo()` and `Foo.call(...)` (false) without a runtime
/// thunk in the user code.
fn gen_must_new_prologue(class_ty: &syn::Ident, opt_out: bool) -> TokenStream2 {
    if opt_out {
        return quote! {};
    }
    let class_name_str = class_ty.to_string();
    let msg = format!(
        "Failed to construct '{class_name_str}': Please use the 'new' operator, this DOM object constructor cannot be called as a function."
    );
    quote! {
        if !args.is_construct_call() {
            let __msg = v8::String::new(scope, #msg).unwrap();
            let __exc = v8::Exception::type_error(scope, __msg);
            scope.throw_exception(__exc);
            return;
        }
    }
}

fn gen_constructor_callback(class_ty: &syn::Ident, c: &ClassMethod) -> TokenStream2 {
    let ctor_name = &c.func.sig.ident;
    let callback_ident = format_ident!("__{}_constructor_callback", class_ty);

    // Constructors have no `self` receiver; the skipping-self helper
    // works uniformly here since it just collects typed args.
    let params = parse_params_skipping_self(c.func);
    let reject_shared_names = extract_reject_shared(&c.func.attrs);
    let extractions = gen_param_extractions(&params, &reject_shared_names);
    let call_args: Vec<&syn::Ident> = params.iter().map(|p| &p.name).collect();

    let is_result = matches!(
        outer_ident(&c.func.sig.output).as_deref(),
        Some("Result")
    );

    let make_instance = if is_result {
        quote! {
            let __instance: #class_ty = match <#class_ty>::#ctor_name(#(#call_args),*) {
                Ok(__v) => __v,
                Err(__err) => {
                    // JsValue passthrough — preserves user-thrown
                    // exception verbatim (Error subclass, .code, etc.).
                    if let ::zeroship_runtime::state::OpErrorKind::JsValue(__global) = &__err.kind {
                        let __local = v8::Local::new(scope, __global);
                        scope.throw_exception(__local);
                        return;
                    }
                    let __msg = v8::String::new(scope, &__err.message).unwrap();
                    let __exc: v8::Local<v8::Value> = match &__err.kind {
                        ::zeroship_runtime::state::OpErrorKind::TypeError => v8::Exception::type_error(scope, __msg),
                        ::zeroship_runtime::state::OpErrorKind::RangeError => v8::Exception::range_error(scope, __msg),
                        ::zeroship_runtime::state::OpErrorKind::DomException(__name) => {
                            ::zeroship_runtime::dom::exception::build(scope, &__err.message, __name).into()
                        }
                        ::zeroship_runtime::state::OpErrorKind::NodeError(__code) => {
                            ::zeroship_runtime::node_error::build_node_exception(scope, __code, &__err.message)
                        }
                        ::zeroship_runtime::state::OpErrorKind::Error => v8::Exception::error(scope, __msg),
                        ::zeroship_runtime::state::OpErrorKind::JsValue(_) => unreachable!(),
                    };
                    scope.throw_exception(__exc);
                    return;
                }
            };
        }
    } else {
        quote! {
            let __instance: #class_ty = <#class_ty>::#ctor_name(#(#call_args),*);
        }
    };

    let store = gen_box_and_install_finalizer(class_ty);
    let must_new = gen_must_new_prologue(class_ty, extract_callable_no_new(&c.func.attrs));

    quote! {
        #[allow(non_snake_case, unused_variables, unused_mut, clippy::needless_borrow)]
        pub(crate) fn #callback_ident(
            scope: &mut v8::PinScope,
            args: v8::FunctionCallbackArguments,
            _rv: v8::ReturnValue,
        ) {
            #must_new
            let __this = args.this();

            #(#extractions)*
            #make_instance

            #store
        }
    }
}

fn gen_default_constructor_callback(class_ty: &syn::Ident) -> TokenStream2 {
    let callback_ident = format_ident!("__{}_constructor_callback", class_ty);
    let store = gen_box_and_install_finalizer(class_ty);
    // No method-level attrs to read — the Default-derived constructor
    // is always must-new. The opt-out attribute requires a user-written
    // `#[v8_constructor]`, by definition.
    let must_new = gen_must_new_prologue(class_ty, false);

    quote! {
        #[allow(non_snake_case, unused_variables, unused_mut, clippy::needless_borrow)]
        pub(crate) fn #callback_ident(
            scope: &mut v8::PinScope,
            args: v8::FunctionCallbackArguments,
            _rv: v8::ReturnValue,
        ) {
            #must_new
            let __this = args.this();
            let __instance: #class_ty = <#class_ty as ::core::default::Default>::default();

            #store
        }
    }
}

/// Box the instance, store the raw pointer in internal field 0, and
/// register a guaranteed finalizer on the JS wrapper to reclaim the
/// Box when V8 GCs the object.
///
/// The pointer is captured as `usize` in the closure so we don't have
/// to assert `Send` on a `*mut Self`; we cast back inside the closure
/// where the type is statically known. The Weak handle is forgotten
/// (via `mem::forget`) because dropping it would deregister the
/// finalizer — `with_guaranteed_finalizer` ensures the closure runs
/// on GC or isolate teardown regardless.
fn gen_box_and_install_finalizer(class_ty: &syn::Ident) -> TokenStream2 {
    quote! {
        let __boxed = Box::new(__instance);
        let __raw_ptr = Box::into_raw(__boxed);
        let __raw_addr = __raw_ptr as usize;

        let __ext = v8::External::new(scope, __raw_ptr as *mut ::std::ffi::c_void);
        __this.set_internal_field(0, __ext.into());

        // SAFETY: __raw_addr was Box::into_raw'd from Box<#class_ty>;
        // the finalizer closure casts back to the same type and drops
        // the Box exactly once when V8 reclaims the JS wrapper.
        let __weak = v8::Weak::with_guaranteed_finalizer(
            scope,
            __this,
            Box::new(move || {
                unsafe {
                    drop(Box::from_raw(__raw_addr as *mut #class_ty));
                }
            }),
        );
        // Dropping the Weak removes the finalizer. The "guaranteed"
        // variant fires on GC or isolate teardown anyway, so we leak
        // the per-instance WeakData (~32 bytes) to keep the registration.
        ::std::mem::forget(__weak);
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build the per-arg extraction code, treating `&mut v8::PinScope` (or
/// any `PinScope`-typed reference) as a "synthetic" arg that consumes
/// no JS index. The synthetic arg is reborrowed from the callback's
/// own `scope` AFTER all JS-arg extractions complete, so user methods
/// can pass it on to v8 ops without fighting the borrow checker —
/// crucially, the reborrow happens after the extractions release any
/// implicit borrows that `args.get(idx)` keeps alive (the returned
/// `Local<'s, Value>` borrows from `args`, whose lifetime can unify
/// with `scope`'s in inference; reborrowing `scope` mutably while a
/// `Local<'s>` is alive triggers E0502 — see commit 4c41d... for
/// the regression test case).
///
/// Concrete output for `fn decode(&mut self, scope: &mut PinScope, n:
/// u32)` is:
///   let n: u32 = args.get(0).uint32_value(scope).unwrap_or(0);
///   let scope = &mut *scope;        // synthetic reborrow, shadows param
///
/// `reject_shared_names` is the set of parameter names whose JS-side
/// argument must reject SharedArrayBuffer-backed views with
/// `TypeError` — emitted before the regular extraction so the SAB
/// check fails before any byte copy. Per WebIDL §3.2.21 and the
/// Compression spec's omission of `[AllowShared]`.
fn gen_param_extractions(
    params: &[crate::Param],
    reject_shared_names: &HashSet<String>,
) -> Vec<TokenStream2> {
    let mut out = Vec::with_capacity(params.len());
    let mut js_idx: usize = 0;

    // Emit JS-arg extractions FIRST in declared order (skipping
    // synthetic PinScope refs). These use the original `scope` param,
    // so no shadow reborrow is alive yet — `args.get(idx)` is free to
    // produce Locals whose lifetime unifies with the param `scope`.
    for p in params.iter() {
        if is_pin_scope_ref(&p.ty) || is_wrapper_local(&p.ty) {
            continue;
        }
        // Optional SAB-rejection guard, emitted *before* the regular
        // extraction so the TypeError fires before any byte copy. The
        // guard checks both ArrayBufferView (Uint8Array etc.) and bare
        // SharedArrayBuffer arguments, matching the IDL `BufferSource`
        // union surface.
        if reject_shared_names.contains(&p.name.to_string()) {
            let idx_lit = js_idx as i32;
            out.push(quote! {
                {
                    let __reject_arg = args.get(#idx_lit);
                    let mut __is_shared = false;
                    if let Ok(__view) =
                        v8::Local::<v8::ArrayBufferView>::try_from(__reject_arg)
                    {
                        if let Some(__buf) = __view.buffer(scope) {
                            if __buf.is_shared_array_buffer() {
                                __is_shared = true;
                            }
                        }
                    } else if v8::Local::<v8::SharedArrayBuffer>::try_from(__reject_arg).is_ok() {
                        __is_shared = true;
                    }
                    if __is_shared {
                        let __msg = v8::String::new(
                            scope,
                            "SharedArrayBuffer-backed buffer source is not allowed",
                        )
                        .unwrap();
                        let __exc = v8::Exception::type_error(scope, __msg);
                        scope.throw_exception(__exc);
                        return;
                    }
                }
            });
        }
        out.push(gen_extract(js_idx, &p.name, &p.ty));
        js_idx += 1;
    }

    // The user method takes the synthetic param BY NAME — usually
    // literally `scope`, but could be any identifier. If the user
    // chose a name OTHER than `scope`, we need to rebind it so the
    // call-site can pass it through. For the common case (`scope`),
    // shadowing is unnecessary because the callback parameter is
    // already named `scope` and the user method body refers to it
    // verbatim. Skip the shadowing in that case to avoid borrow-check
    // conflicts when JS args' Locals are still alive (their lifetime
    // unifies with `scope`'s, and a mutable reborrow while a Local is
    // alive is E0502).
    for p in params.iter() {
        if is_pin_scope_ref(&p.ty) {
            let name = &p.name;
            if name != "scope" {
                out.push(quote! { let #name = &mut *scope; });
            }
        }
    }

    // Bind synthetic `wrapper: v8::Local<v8::Object>` (or any
    // `v8::Local<v8::Object>` typed param) to `args.this()`. Methods
    // that need to register themselves with the runtime (e.g.
    // WebSocket.send registering the wrapper Global for event
    // dispatch) take this synthetic. Idempotent: `args.this()` is
    // cheap to call repeatedly.
    for p in params.iter() {
        if is_wrapper_local(&p.ty) {
            let name = &p.name;
            out.push(quote! { let #name: v8::Local<v8::Object> = args.this(); });
        }
    }

    out
}

/// True for `&mut v8::PinScope<'_, '_>` and similar reference forms.
/// We don't bother distinguishing `&` vs `&mut` — V8 ops universally
/// require `&mut`, and the type alias system means PinScope appears
/// in many shapes (with/without lifetime params, with/without the v8::
/// prefix).
fn is_pin_scope_ref(ty: &Type) -> bool {
    if let Type::Reference(r) = ty {
        return type_path_contains_segment(&r.elem, "PinScope");
    }
    false
}

/// True for `v8::Local<v8::Object>` typed params — synthetic that
/// gets bound to `args.this()`. Used by methods that need access
/// to the JS wrapper itself (e.g. to register a Global for use by
/// async event-dispatch paths). Distinct from is_pin_scope_ref:
/// no reference form, just the bare Local<Object>.
fn is_wrapper_local(ty: &Type) -> bool {
    let Type::Path(tp) = ty else {
        return false;
    };
    let last = match tp.path.segments.last() {
        Some(s) => s,
        None => return false,
    };
    if last.ident != "Local" {
        return false;
    }
    let syn::PathArguments::AngleBracketed(args) = &last.arguments else {
        return false;
    };
    // Look for v8::Object as the last generic arg.
    args.args.iter().any(|arg| {
        if let syn::GenericArgument::Type(inner) = arg {
            return type_path_contains_segment(inner, "Object");
        }
        false
    })
}

fn type_path_contains_segment(ty: &Type, target: &str) -> bool {
    if let Type::Path(tp) = ty {
        return tp
            .path
            .segments
            .iter()
            .any(|s| s.ident == target);
    }
    false
}

fn parse_params_skipping_self(f: &ImplItemFn) -> Vec<crate::Param> {
    f.sig
        .inputs
        .iter()
        .filter_map(|arg| {
            if let FnArg::Typed(pt) = arg {
                if let syn::Pat::Ident(pi) = &*pt.pat {
                    return Some(crate::Param {
                        name: pi.ident.clone(),
                        ty: (*pt.ty).clone(),
                    });
                }
            }
            None
        })
        .collect()
}

fn outer_ident(output: &ReturnType) -> Option<String> {
    match output {
        ReturnType::Default => None,
        ReturnType::Type(_, ty) => crate::type_ident(ty),
    }
}
