//! `gen_install` — `pub fn install(scope) -> FunctionTemplate` codegen.
//!
//! Wave 3 commit 3 — extracted from `mod.rs`'s 195-line megaquote
//! (design `docs/proposals/runtime-macros-refactor.md` §4.1, F3, F4).
//! Closes the 9-arg parameter list by reading every input from
//! [`ClassConfig`].
//!
//! Wave 6 will further split the install body into per-fragment
//! helpers (`#[v8_inherit]`, `#[v8_const]`, `#[v8_async_iterable]`,
//! Symbol.toStringTag); for Wave 3 the install function stays
//! monolithic but emits byte-identical tokens to the pre-refactor
//! shape.

use std::collections::{HashMap, HashSet};

use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};

use super::super::fastcall::fastcall_cfn_ident;
use super::super::helpers::method_callback_ident;
use super::super::shared::class_config::ClassConfig;
use super::super::{ClassMethod, ConstKind, MethodKind};
use crate::must_str;

/// Emit the per-class `pub fn install(scope) -> v8::Local<v8::FunctionTemplate>`
/// body. Splices into the surrounding `impl <Class>` block emitted by
/// the top-level `assemble_tokens` orchestrator.
pub(super) fn gen_install(cfg: &ClassConfig) -> TokenStream2 {
    let class_ty = cfg.class_ty;
    let has_any_fastcall = cfg.has_any_fastcall;
    let to_string_tag_override = cfg.to_string_tag.as_deref();
    let inherit_intrinsic = cfg.inherit_intrinsic.as_deref();
    let inherit_base = cfg.inherit_base.as_ref();
    let install_iterable_call = cfg.install_iterable_call.as_ref();
    let async_iterable_method = cfg.async_iterable_method.as_deref();
    let const_decls = cfg.consts.as_slice();
    let methods: Vec<&ClassMethod> = cfg.regular();
    let methods = methods.as_slice();

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
    let fastcall_by_jsname: HashMap<String, &ClassMethod> = cfg.fastcall_by_jsname();

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
    // Wave 9 NS2: the unrecognised-value diagnostic moved to
    // `analyze.rs`'s pre-emit validation step. By the time we get
    // here, `inherit_intrinsic` is known to be either `None` or
    // `Some("IteratorPrototype")`. Any other value would have been
    // rejected as a clean `syn::Error` before this fn ran.
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
        // Unreachable per the analyse-phase validation above. Kept as
        // a defensive guard; the `unreachable!` here surfaces as a
        // proc-macro panic at expand time, NOT as a `compile_error!`
        // spliced into the user's fn body — so any future drift in
        // the validation gate fails loudly.
        Some(_) => unreachable!(
            "v8_inherit_intrinsic validation in analyze.rs accepts only `IteratorPrototype`"
        ),
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
