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
use quote::{format_ident, quote};
use std::collections::{HashMap, HashSet};
use syn::{ImplItem, ItemImpl, Type};

use crate::{must_str, v8_iterable};

mod fastcall;
mod helpers;
mod method;
mod parse;

use fastcall::{fastcall_cfn_ident, gen_fastcall_callback, validate_fastcall_signature};
use helpers::method_callback_ident;
use method::{
    gen_async_method_callback, gen_constructor_callback, gen_default_constructor_callback,
    gen_method_callback, gen_same_object_getter_callback, gen_static_callback,
};
use parse::{
    classify, extract_async_iterable, extract_consts, extract_fastcall, extract_inherit_base,
    extract_inherit_intrinsic, extract_same_object, extract_state_marker, extract_to_string_tag,
    extract_v8_name, has_any_receiver, has_mut_self, resolve_state_and_marker,
};

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
    /// WebIDL §3.7.4 static operation — `#[v8_static_method]`. No
    /// receiver, no brand check, no internal-field deref. Installed
    /// on the constructor FunctionTemplate, not the prototype.
    StaticMethod,
    /// WebIDL §3.7.4 static attribute (read-only) — `#[v8_static_getter]`.
    /// No receiver. Installed via `set_accessor_property` on the
    /// constructor template.
    StaticGetter,
}

struct ClassMethod<'a> {
    kind: MethodKind,
    func: &'a syn::ImplItemFn,
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
    /// `#[v8_method(fastcall)]` / `#[v8_getter(fastcall)]` — emit a
    /// CFunction shim alongside the slow-path FunctionCallback so V8
    /// Turbofan can inline the typed-shape call at hot sites. See
    /// `extract_fastcall` for the rationale and `gen_fastcall_*` for
    /// the codegen detail. Mutually compatible with `same_object` only
    /// in the negative — fastcall paths can't allocate and SameObject
    /// returns a Global<Object>, so the two flags are not co-applicable.
    fastcall: bool,
}

/// A single `#[v8_const(NAME = LIT)]` declaration.
struct ConstDecl {
    /// The JS-visible property name (Rust ident verbatim).
    name: syn::Ident,
    /// The literal expression — quoted as-is so the literal's type
    /// suffix is preserved through expansion.
    value: syn::ExprLit,
    /// Selected V8-side materialiser, derived from the literal suffix.
    kind: ConstKind,
}

#[derive(Debug, Clone, Copy)]
enum ConstKind {
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
    // Static methods / getters skip the brand check and internal-field
    // deref entirely (no receiver) and install on the constructor
    // template via `set_with_attr` / `set_accessor_property`.
    let callbacks: Vec<TokenStream2> = regular
        .iter()
        .map(|m| match m.kind {
            MethodKind::AsyncMethod => gen_async_method_callback(class_ty, state_ty, m),
            MethodKind::Getter if m.same_object => {
                gen_same_object_getter_callback(class_ty, state_ty, m)
            }
            MethodKind::StaticMethod | MethodKind::StaticGetter => {
                gen_static_callback(class_ty, state_ty, m)
            }
            _ => gen_method_callback(class_ty, state_ty, m),
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
        .filter_map(|m| gen_fastcall_callback(class_ty, state_ty, m))
        .collect();
    let has_any_fastcall = regular.iter().any(|m| m.fastcall);

    let constructor_callback = match constructor {
        Some(c) => gen_constructor_callback(class_ty, state_ty, c, has_any_fastcall),
        None => gen_default_constructor_callback(class_ty, state_ty, has_any_fastcall),
    };

    // Impl-block-level overrides for class-wide install behaviour.
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
        // Pass both `class_ty` (marker — drives JS naming) and
        // `state_ty` (impl-block receiver — what's in the Box).
        // Under `#[v8_state_marker]` they diverge; without it they're
        // equal so the emission is byte-identical to before.
        Some(attr) => match v8_iterable::generate(class_ty, state_ty, attr, value_pairs_sig) {
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

    // `Self::install(scope) -> v8::Local<v8::FunctionTemplate>`
    //
    // `constructor.is_some()` was wired into `gen_install` as
    // `has_user_constructor` for a never-implemented default-Self
    // codegen branch. Removed in this commit; if a future PR adds a
    // generated default constructor, plumb it back through (or, per
    // F4, fold it into a `ClassConfig` struct).
    let install = gen_install(
        class_ty,
        &regular,
        to_string_tag_override.as_deref(),
        inherit_intrinsic.as_deref(),
        inherit_base.as_ref(),
        install_iterable_call.as_ref(),
        async_iterable_method.as_deref(),
        &const_decls,
        has_any_fastcall,
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
        /// Walks at most 1024 prototype links — matching V8's own
        /// internal `Object::PrototypeChainLength` sanity bound. The
        /// cap is NOT a cycle defence: ECMAScript §10.4.7.2 step 8
        /// already requires `Object.setPrototypeOf` to reject any
        /// assignment that would create a cycle, so user JS cannot
        /// construct one. The cap exists purely as a defence-in-depth
        /// belt-and-braces against an underlying V8 bug or future
        /// proxy-driven prototype chain that fakes infinite linear
        /// depth. Real WebIDL inheritance chains are 1-3 hops; pure
        /// prototypal chains rarely exceed 5; reaching the cap is a
        /// pathological case for which "false" is the conservative
        /// answer.
        ///
        /// The cost is dwarfed by the ~100ns V8 callback overhead —
        /// the brand check itself is O(depth) Local pointer
        /// comparisons.
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
            // Object). The 1024 cap matches V8's internal sanity
            // bound; cycle creation is already blocked by V8 (see
            // doc-comment).
            let mut current: v8::Local<v8::Value> = match obj.get_prototype(scope) {
                Some(v) => v,
                None => return false,
            };
            for _ in 0..1024 {
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
    };

    expanded
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
// install() codegen
// ---------------------------------------------------------------------------

fn gen_install(
    class_ty: &syn::Ident,
    methods: &[&ClassMethod],
    to_string_tag_override: Option<&str>,
    inherit_intrinsic: Option<&str>,
    inherit_base: Option<&syn::Path>,
    install_iterable_call: Option<&TokenStream2>,
    async_iterable_method: Option<&str>,
    const_decls: &[ConstDecl],
    has_any_fastcall: bool,
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

    // Pair lookup for fastcall: which JS-name has a fastcall variant
    // (and therefore needs `builder(slow).build_fast(scope, &[fast])`
    // wiring). Methods always pair under their own name; getters pair
    // by JS-name with their setter sibling, but the setter never has
    // fastcall (rejected at extract time — setters return ()). So we
    // only need to track per-method fastcall.
    let mut fastcall_by_jsname: HashMap<String, &ClassMethod> = HashMap::new();
    for m in methods {
        if m.fastcall {
            fastcall_by_jsname.insert(m.js_name.clone(), m);
        }
    }

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
                    let scope_tok = quote! { scope };
                    let key_init = must_str(&scope_tok, &quote! { #js_name });
                    if m.fastcall {
                        let cfn = fastcall_cfn_ident(class_ty, name);
                        Some(quote! {
                            {
                                let __key = #key_init;
                                // Wire the slow callback as the
                                // FunctionCallback fallback AND the
                                // CFunction shim as the fast-path
                                // overload. V8 chooses fast vs slow at
                                // JIT time per the receiver/arg shape.
                                let __fn_tmpl = v8::FunctionTemplate::builder(#cb)
                                    .build_fast(scope, &[#cfn.0]);
                                __proto.set(__key.into(), __fn_tmpl.into());
                            }
                        })
                    } else {
                        Some(quote! {
                            {
                                let __key = #key_init;
                                let __fn_tmpl = v8::FunctionTemplate::new(scope, #cb);
                                __proto.set(__key.into(), __fn_tmpl.into());
                            }
                        })
                    }
                }
                MethodKind::Getter | MethodKind::Setter => {
                    if !emitted_accessors.insert(js_name.clone()) {
                        return None;
                    }
                    let pair = accessor_pairs.get(&js_name);
                    let (getter_opt, setter_opt) = pair.cloned().unwrap_or_default();
                    // Look up whether the getter under this JS-name is
                    // fastcall-annotated. The setter never is — fastcall
                    // is rejected at extract for setters since they
                    // return `()` and V8 setters discard the return.
                    let getter_fastcall_cfn = fastcall_by_jsname
                        .get(&js_name)
                        .filter(|cm| matches!(cm.kind, MethodKind::Getter))
                        .map(|cm| fastcall_cfn_ident(class_ty, &cm.func.sig.ident));
                    let getter_tokens = match (getter_opt, getter_fastcall_cfn) {
                        (Some(cb), Some(cfn)) => quote! {
                            let __getter_tmpl = v8::FunctionTemplate::builder(#cb)
                                .build_fast(scope, &[#cfn.0]);
                            let __getter_arg: Option<v8::Local<v8::FunctionTemplate>> = Some(__getter_tmpl);
                        },
                        (Some(cb), None) => quote! {
                            let __getter_tmpl = v8::FunctionTemplate::new(scope, #cb);
                            let __getter_arg: Option<v8::Local<v8::FunctionTemplate>> = Some(__getter_tmpl);
                        },
                        (None, _) => quote! {
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
                    let scope_tok = quote! { scope };
                    let key_init = must_str(&scope_tok, &quote! { #js_name });
                    Some(quote! {
                        {
                            let __key = #key_init;
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
                MethodKind::Constructor
                | MethodKind::StaticMethod
                | MethodKind::StaticGetter => None,
            }
        })
        .collect();

    // Static-method / static-getter installs go on the constructor
    // FunctionTemplate, NOT the prototype. WebIDL §3.7.4: static
    // operations and attributes live as own-properties of the
    // interface object (the constructor function). Implementation:
    // FunctionTemplate inherits Template, so we can call `set_with_attr`
    // / `set_accessor_property` directly on `__ctor_tmpl` — V8 promotes
    // the property onto the resolved Function once `get_function`
    // materialises it.
    let static_sets: Vec<TokenStream2> = methods
        .iter()
        .filter_map(|m| {
            let js_name = m.js_name.clone();
            match m.kind {
                MethodKind::StaticMethod => {
                    let name = &m.func.sig.ident;
                    let cb = method_callback_ident(class_ty, name);
                    let scope_tok = quote! { scope };
                    let key_init = must_str(&scope_tok, &quote! { #js_name });
                    Some(quote! {
                        {
                            let __key = #key_init;
                            let __fn_tmpl = v8::FunctionTemplate::new(scope, #cb);
                            // Attributes default to NONE — same as
                            // the prototype-method install above.
                            // Browsers expose static methods as
                            // configurable + writable + non-enumerable
                            // (matching standard JS class semantics);
                            // we follow that with an explicit DONT_ENUM.
                            __ctor_tmpl.set_with_attr(
                                __key.into(),
                                __fn_tmpl.into(),
                                v8::PropertyAttribute::DONT_ENUM,
                            );
                        }
                    })
                }
                MethodKind::StaticGetter => {
                    let name = &m.func.sig.ident;
                    let cb = method_callback_ident(class_ty, name);
                    let scope_tok = quote! { scope };
                    let key_init = must_str(&scope_tok, &quote! { #js_name });
                    Some(quote! {
                        {
                            let __key = #key_init;
                            let __getter_tmpl = v8::FunctionTemplate::new(scope, #cb);
                            // FunctionTemplate exposes
                            // `set_accessor_property`; static getters
                            // live on the constructor function as
                            // accessor descriptors per WebIDL §3.7.4.
                            __ctor_tmpl.set_accessor_property(
                                __key.into(),
                                Some(__getter_tmpl),
                                None,
                                v8::PropertyAttribute::DONT_ENUM,
                            );
                        }
                    })
                }
                _ => None,
            }
        })
        .collect();

    // The literal that goes into Symbol.toStringTag. Defaults to the
    // Rust struct name; overridden by `#[v8_to_string_tag = "..."]`.
    let to_string_tag_str = to_string_tag_override
        .map(str::to_string)
        .unwrap_or_else(|| class_name_str.clone());

    // `#[v8_const(NAME = LIT)]` — per WebIDL §3.7.5, install each
    // declared constant on BOTH the constructor's FunctionTemplate
    // (which materialises as `Class.NAME` once `get_function` is
    // called) and the prototype template (so `Class.prototype.NAME`
    // and instance lookups via the prototype chain see the value).
    //
    // Property attributes per spec: `{ writable: false, enumerable:
    // true, configurable: false }`. V8 flags: READ_ONLY (= !writable)
    // and DONT_DELETE (= !configurable). `enumerable: true` is the
    // template default.
    let const_block = if const_decls.is_empty() {
        quote! {}
    } else {
        let mut sets: Vec<TokenStream2> = Vec::with_capacity(const_decls.len());
        for decl in const_decls {
            let name_str = decl.name.to_string();
            let value = &decl.value;
            let materialise = match decl.kind {
                ConstKind::UInt => quote! {
                    let __v = v8::Integer::new_from_unsigned(scope, (#value) as u32);
                },
                ConstKind::SInt => quote! {
                    let __v = v8::Integer::new(scope, (#value) as i32);
                },
            };
            let scope_tok = quote! { scope };
            let key_init = must_str(&scope_tok, &quote! { #name_str });
            sets.push(quote! {
                {
                    let __key = #key_init;
                    #materialise
                    // Constructor side: `Class.NAME`.
                    __ctor_tmpl.set_with_attr(
                        __key.into(),
                        __v.into(),
                        v8::PropertyAttribute::READ_ONLY | v8::PropertyAttribute::DONT_DELETE,
                    );
                    // Prototype side: `Class.prototype.NAME` and
                    // `(new Class()).NAME` via the prototype chain.
                    __proto.set_with_attr(
                        __key.into(),
                        __v.into(),
                        v8::PropertyAttribute::READ_ONLY | v8::PropertyAttribute::DONT_DELETE,
                    );
                }
            });
        }
        quote! { #(#sets)* }
    };

    // `#[v8_async_iterable(method = "name")]` — alias
    // `[Symbol.asyncIterator]` to the named method per WebIDL §3.7.10.5.
    // The user-defined method retains its original installation on the
    // prototype; this block adds a SECOND FunctionTemplate that wraps
    // the same callback and is installed under `Symbol.asyncIterator`,
    // with `set_class_name(method)` so the alias's `name` property
    // matches the spec.
    let async_iterable_block = match async_iterable_method {
        None => quote! {},
        Some(method_name) => {
            // Look up the method's callback ident. We've already
            // validated in `expand` that the method exists, so the
            // first matching JS-name entry is guaranteed to be present.
            let method_ident = methods
                .iter()
                .find(|m| {
                    matches!(m.kind, MethodKind::Method | MethodKind::AsyncMethod)
                        && m.js_name == method_name
                })
                .map(|m| &m.func.sig.ident)
                .expect("async_iterable_method validated in expand()");
            let cb = method_callback_ident(class_ty, method_ident);
            let scope_tok = quote! { scope };
            let name_init = must_str(&scope_tok, &quote! { #method_name });
            quote! {
                {
                    let __async_iter_sym = v8::Symbol::get_async_iterator(scope);
                    let __alias_tmpl = v8::FunctionTemplate::new(scope, #cb);
                    let __name_v = #name_init;
                    __alias_tmpl.set_class_name(__name_v);
                    __proto.set(__async_iter_sym.into(), __alias_tmpl.into());
                }
            }
        }
    };

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
        Some("IteratorPrototype") => {
            let scope_tok = quote! { scope };
            let proto_key_init = must_str(&scope_tok, &quote! { "prototype" });
            let js_init = must_str(
                &scope_tok,
                &quote! { "Object.getPrototypeOf(Object.getPrototypeOf([][Symbol.iterator]()))" },
            );
            quote! {
                // After get_function() the prototype object exists in the
                // current context. Walk to %Iterator.prototype% and chain.
                {
                    let __ctor_fn = __ctor_tmpl.get_function(scope).unwrap();
                    let __proto_key = #proto_key_init;
                    let __ctor_proto_v = __ctor_fn.get(scope, __proto_key.into()).unwrap();
                    let __ctor_proto: v8::Local<v8::Object> = __ctor_proto_v.try_into().unwrap();
                    // %IteratorPrototype% via getPrototypeOf(getPrototypeOf([][Symbol.iterator]())).
                    let __js = #js_init;
                    let __script = v8::Script::compile(scope, __js, None).unwrap();
                    let __iter_proto = __script.run(scope).unwrap();
                    __ctor_proto.set_prototype(scope, __iter_proto);
                }
            }
        }
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

    // Internal field count: 2 when at least one method on the class
    // uses fastcall (slot 0 = External, slot 1 = aligned ptr); else 1
    // (the existing single-slot External shape).
    let internal_field_count_lit: usize = if has_any_fastcall { 2 } else { 1 };

    let scope_tok = quote! { scope };
    let class_name_init = must_str(&scope_tok, &quote! { #class_name_str });
    let tag_value_init = must_str(&scope_tok, &quote! { #to_string_tag_str });

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
            let __class_name = #class_name_init;
            __ctor_tmpl.set_class_name(__class_name);

            // `#[v8_inherit(BaseClass)]` — establish the prototype chain
            // BEFORE we layer our own prototype properties on top.
            #inherit_base_block

            // Reserve internal fields for the boxed Rust state.
            //
            //   Slot 0 — Box<Self> wrapped in an External, with a
            //            guaranteed-finalizer Weak that drops the Box
            //            on V8 GC of the wrapper. Used by every slow-
            //            path callback (the standard wrapper teardown
            //            path).
            //   Slot 1 — same Box<Self> raw pointer, set via
            //            set_aligned_pointer_in_internal_field, ONLY
            //            when at least one method/getter on the class
            //            is fastcall-annotated. The fast-path shim
            //            recovers `*const Self` from this slot via
            //            get_aligned_pointer_from_internal_field — a
            //            single load instruction with no scope.
            //
            // The two slots hold the same address, so memory cost is
            // one extra pointer per wrapper instance. Slot 1 is unused
            // for classes without fastcall, so the field count stays
            // at 1 in that case.
            __ctor_tmpl
                .instance_template(scope)
                .set_internal_field_count(#internal_field_count_lit);

            let __proto = __ctor_tmpl.prototype_template(scope);
            #(#proto_sets)*

            // Static operations / attributes per WebIDL §3.7.4 — own
            // properties of the constructor function, not the prototype.
            // No-op when no `#[v8_static_method]` / `#[v8_static_getter]`
            // attributes are present on the impl block.
            #(#static_sets)*

            // `#[v8_iterable(...)]` — install keys / values / entries /
            // forEach / @@iterator on the prototype template. The
            // companion `<Class>Iterator` class is emitted at module
            // scope (see `iterable_codegen`) and the install call here
            // wires its factories onto the parent's proto.
            #install_iterable_call

            // `#[v8_async_iterable(method = "name")]` — install
            // `[Symbol.asyncIterator]` aliasing the named method per
            // WebIDL §3.7.10.5. Empty when the attribute is absent.
            #async_iterable_block

            // `#[v8_const(NAME = LIT)]` — WebIDL §3.7.5 interface
            // constants. Installed on BOTH the constructor template
            // and the prototype template with read-only / non-
            // configurable / enumerable attributes. Empty when no
            // constants are declared.
            #const_block

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
                let __tag_value = #tag_value_init;
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
