//! `#[v8_class]` proc macro — wraps a Rust `impl` block as a V8
//! ObjectTemplate-backed class.
//!
//! Walks the impl block, collects methods marked with `#[v8_method]`,
//! `#[v8_getter]`, `#[v8_setter]`, `#[v8_constructor]`, and emits a
//! `Self::install(scope) -> v8::Local<v8::FunctionTemplate>` function
//! plus per-method callbacks.
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
//! ## Known gaps (deferred until a real consumer needs them)
//!
//! - **Async methods.** `async fn foo(&self, ...) -> T` would need to
//!   spawn the future via SharedState and return a Promise. The
//!   existing `#[zeroship_op(async)]` does this for free functions;
//!   port that pattern when fetch grows methods that await.
//! - **Inheritance.** No `#[v8_inherit(BaseClass)]` yet — needed for
//!   the `EventTarget` chain (WebSocket / EventSource extend it).
//!   `FunctionTemplate::inherit` is the underlying primitive.
//! - **Same-name getter+setter pairing.** Defining `#[v8_getter]
//!   value(&self)` and `#[v8_setter] value(&mut self, v)` at once is
//!   illegal in Rust (duplicate method names) and the install code
//!   calls `set_accessor_property` separately for each, which V8
//!   rejects. Fix needs a `#[v8_name = "value"]` rename plus pairing
//!   in install codegen. Body's `body`/`bodyUsed` are read-only so
//!   not blocking fetch.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use std::collections::{HashMap, HashSet};
use syn::{
    parse_macro_input, Attribute, Expr, ExprLit, FnArg, ImplItem, ImplItemFn, ItemImpl, Lit, Meta,
    Receiver, ReturnType, Type,
};

use crate::{gen_call_return, gen_extract};

// ---------------------------------------------------------------------------
// Method classification
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MethodKind {
    Method,
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
}

fn classify(func: &ImplItemFn) -> Option<MethodKind> {
    for attr in &func.attrs {
        let path = attr.path();
        if path.is_ident("v8_method") {
            return Some(MethodKind::Method);
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
                methods.push(ClassMethod {
                    kind,
                    func,
                    mut_receiver: has_mut_self(func),
                    js_name,
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

    // Per-method callback fns
    let callbacks: Vec<TokenStream2> = regular
        .iter()
        .map(|m| gen_method_callback(class_ty, m))
        .collect();

    let constructor_callback = match constructor {
        Some(c) => gen_constructor_callback(class_ty, c),
        None => gen_default_constructor_callback(class_ty),
    };

    // Impl-block-level overrides for class-wide install behaviour.
    let to_string_tag_override = extract_to_string_tag(&input.attrs);
    let inherit_intrinsic = extract_inherit_intrinsic(&input.attrs);

    // `Self::install(scope) -> v8::Local<v8::FunctionTemplate>`
    let install = gen_install(
        class_ty,
        &regular,
        constructor.is_some(),
        to_string_tag_override.as_deref(),
        inherit_intrinsic.as_deref(),
    );

    // Strip our marker attributes from the impl items so rustc doesn't
    // see unknown attributes after expansion. Keep everything else.
    let stripped_impl = strip_marker_attrs(input.clone());

    let expanded = quote! {
        #stripped_impl

        #[allow(non_snake_case, dead_code)]
        impl #class_ty {
            #install
        }

        #constructor_callback
        #(#callbacks)*
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
        !(p.is_ident("v8_to_string_tag") || p.is_ident("v8_inherit_intrinsic"))
    });
    for item in &mut input.items {
        if let ImplItem::Fn(func) = item {
            func.attrs.retain(|attr| {
                let p = attr.path();
                !(p.is_ident("v8_method")
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
                MethodKind::Method => {
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

    quote! {
        /// Install this class on the given V8 scope, returning the
        /// FunctionTemplate. The runtime calls this from
        /// `setup_globals` and uses the returned template to attach
        /// the class as a global (e.g. `globalThis.Headers`).
        pub fn install<'s>(
            scope: &mut v8::PinScope<'s, '_>,
        ) -> v8::Local<'s, v8::FunctionTemplate> {
            let __ctor_tmpl = v8::FunctionTemplate::new(scope, #constructor_callback_ident);
            let __class_name = v8::String::new(scope, #class_name_str).unwrap();
            __ctor_tmpl.set_class_name(__class_name);

            // Reserve one internal field to hold the boxed Rust state.
            __ctor_tmpl
                .instance_template(scope)
                .set_internal_field_count(1);

            let __proto = __ctor_tmpl.prototype_template(scope);
            #(#proto_sets)*

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

            __ctor_tmpl
        }
    }
}

// ---------------------------------------------------------------------------
// Method callback codegen
// ---------------------------------------------------------------------------

fn method_callback_ident(class_ty: &syn::Ident, method: &syn::Ident) -> syn::Ident {
    format_ident!("__{}_{}_callback", class_ty, method)
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

    quote! {
        #[allow(non_snake_case, unused_variables, unused_mut, clippy::needless_borrow)]
        pub(crate) fn #callback_name(
            scope: &mut v8::PinScope,
            args: v8::FunctionCallbackArguments,
            mut rv: v8::ReturnValue,
        ) {
            // Extract the boxed instance from internal field 0 of `this`.
            let __this = args.this();
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
            let __instance = unsafe { &mut *(__ext.value() as *mut #class_ty) };

            #(#extractions)*
            #call_return
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

    quote! {
        #[allow(non_snake_case, unused_variables, unused_mut, clippy::needless_borrow)]
        pub(crate) fn #callback_name(
            scope: &mut v8::PinScope,
            args: v8::FunctionCallbackArguments,
            mut rv: v8::ReturnValue,
        ) {
            let __this = args.this();
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
                    let __msg = v8::String::new(scope, &__err.message).unwrap();
                    let __exc = match __err.kind {
                        ::zeroship_runtime::state::OpErrorKind::TypeError => v8::Exception::type_error(scope, __msg),
                        ::zeroship_runtime::state::OpErrorKind::RangeError => v8::Exception::range_error(scope, __msg),
                        _ => v8::Exception::error(scope, __msg),
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

    quote! {
        #[allow(non_snake_case, unused_variables, unused_mut, clippy::needless_borrow)]
        pub(crate) fn #callback_ident(
            scope: &mut v8::PinScope,
            args: v8::FunctionCallbackArguments,
            _rv: v8::ReturnValue,
        ) {
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

    quote! {
        #[allow(non_snake_case, unused_variables, unused_mut, clippy::needless_borrow)]
        pub(crate) fn #callback_ident(
            scope: &mut v8::PinScope,
            args: v8::FunctionCallbackArguments,
            _rv: v8::ReturnValue,
        ) {
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
        if is_pin_scope_ref(&p.ty) {
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
