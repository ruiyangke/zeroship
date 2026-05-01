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
use syn::{
    parse_macro_input, FnArg, ImplItem, ImplItemFn, ItemImpl, Receiver, ReturnType, Type,
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
                methods.push(ClassMethod {
                    kind,
                    func,
                    mut_receiver: has_mut_self(func),
                });
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

    // `Self::install(scope) -> v8::Local<v8::FunctionTemplate>`
    let install = gen_install(class_ty, &regular, constructor.is_some());

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
    for item in &mut input.items {
        if let ImplItem::Fn(func) = item {
            func.attrs.retain(|attr| {
                let p = attr.path();
                !(p.is_ident("v8_method")
                    || p.is_ident("v8_getter")
                    || p.is_ident("v8_setter")
                    || p.is_ident("v8_constructor"))
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
) -> TokenStream2 {
    let class_name_str = class_ty.to_string();
    let constructor_callback_ident = format_ident!("__{}_constructor_callback", class_ty);

    // Per-method prototype installation lines.
    let proto_sets: Vec<TokenStream2> = methods
        .iter()
        .map(|m| {
            let name = &m.func.sig.ident;
            let cb = method_callback_ident(class_ty, name);
            let js_name = name.to_string();
            match m.kind {
                MethodKind::Method => quote! {
                    {
                        let __key = v8::String::new(scope, #js_name).unwrap();
                        let __fn_tmpl = v8::FunctionTemplate::new(scope, #cb);
                        __proto.set(__key.into(), __fn_tmpl.into());
                    }
                },
                MethodKind::Getter => quote! {
                    {
                        let __key = v8::String::new(scope, #js_name).unwrap();
                        let __getter_tmpl = v8::FunctionTemplate::new(scope, #cb);
                        __proto.set_accessor_property(
                            __key.into(),
                            Some(__getter_tmpl.into()),
                            None,
                            v8::PropertyAttribute::NONE,
                        );
                    }
                },
                MethodKind::Setter => quote! {
                    {
                        let __key = v8::String::new(scope, #js_name).unwrap();
                        let __setter_tmpl = v8::FunctionTemplate::new(scope, #cb);
                        __proto.set_accessor_property(
                            __key.into(),
                            None,
                            Some(__setter_tmpl.into()),
                            v8::PropertyAttribute::NONE,
                        );
                    }
                },
                MethodKind::Constructor => quote! {},
            }
        })
        .collect();

    // If there's no user-defined constructor, still emit one that
    // default-constructs `Self`. Requires `Self: Default`.
    let _user_ctor_marker = has_user_constructor;

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
            // V8's `set_class_name` only affects the constructor's own
            // `name`; the @@toStringTag default is overridden by the
            // user's prototype unless we set it explicitly. Libraries
            // (webidl-conversions, etc.) check this for type guards.
            {
                let __tag_sym = v8::Symbol::get_to_string_tag(scope);
                let __tag_value = v8::String::new(scope, #class_name_str).unwrap();
                __proto.set(__tag_sym.into(), __tag_value.into());
            }

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
    let extractions = gen_param_extractions(&params);

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
    let extractions = gen_param_extractions(&params);
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
    let extractions = gen_param_extractions(&params);
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
/// own `scope` so user methods can pass it on to v8 ops without
/// fighting the borrow checker.
///
/// Concrete output for `fn decode(&mut self, scope: &mut PinScope, n:
/// u32)` is:
///   let scope = &mut *scope;        // reborrow, shadows callback param
///   let n: u32 = args.get(0).uint32_value(scope).unwrap_or(0);
///
/// Synthetic args are emitted FIRST so the reborrowed `scope` is
/// available to subsequent JS-arg extractions.
fn gen_param_extractions(params: &[crate::Param]) -> Vec<TokenStream2> {
    let mut out = Vec::with_capacity(params.len());
    let mut js_idx: usize = 0;

    // Emit reborrows for synthetic params first (they don't consume
    // JS indices and they need to be in scope before extractions).
    for p in params.iter() {
        if is_pin_scope_ref(&p.ty) {
            let name = &p.name;
            out.push(quote! { let #name = &mut *scope; });
        }
    }
    // Then emit JS-arg extractions in declared order, skipping
    // synthetics.
    for p in params.iter() {
        if !is_pin_scope_ref(&p.ty) {
            out.push(gen_extract(js_idx, &p.name, &p.ty));
            js_idx += 1;
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
