//! Codegen for `#[v8_method(fastcall)]` / `#[v8_getter(fastcall)]`.
//!
//! V8's fast API path lets Turbofan inline a typed CFunction call shim
//! at hot sites, skipping the full FunctionCallback prologue
//! (~10–30 ns per call). The macro emits the fast shim alongside the
//! slow-path callback; V8 chooses fast vs slow at JIT time based on
//! receiver shape and arg types, falling back to the slow path when
//! the inline cache hasn't seen the receiver's hidden class yet or
//! when arg shapes don't match the typed signature (e.g. multibyte
//! strings for SeqOneByteString).
//!
//! Hosts:
//! - `validate_fastcall_signature` — parse-time signature checker.
//! - `fastcall_arg_mapping` / `fastcall_return_mapping` — Rust ↔ fast-API
//!   type translators.
//! - `fastcall_fn_ident` / `fastcall_cfn_ident` / `fastcall_cinfo_ident`
//!   — mangled identifier helpers.
//! - `gen_fastcall_callback` — emits the extern "C" shim, the static
//!   `CFunctionInfo`, and the static `CFunction`.

use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::{FnArg, ImplItemFn, ReturnType, Type};

use super::helpers::parse_params_skipping_self;
use super::ClassMethod;

/// Validate that the user's method signature is compatible with the
/// V8 fast API path. Called at expand-time when `#[v8_method(fastcall)]`
/// or `#[v8_getter(fastcall)]` is set; emits a `compile_error!`-shaped
/// `syn::Error` on rejection so the user sees the diagnostic at the
/// right span.
///
/// Allowed shapes (per V8 fast API + macro design):
///   - Receiver: `&self` (no `&mut self`; that's already rejected before
///     we get here).
///   - Args: `bool`, `i32`, `u32`, `i64`, `u64`, `f32`, `f64`,
///     `ByteString` (the macro maps to V8's SeqOneByteString fast type).
///   - Return: `bool`, `i32`, `u32`, `i64`, `u64`, `f32`, `f64`, `()`,
///     OR `Result<<primitive>, OpError>` (the fast path catches the
///     Err and re-routes through CallbackScope::new + throw_exception).
///
/// Rejected explicitly with helpful messages:
///   - `String` / `&str` return — fast path forbids allocation
///   - `Vec<u8>` / `Vec<T>` return — fast path forbids allocation
///   - `Option<T>` return — fast path can't represent `None`
///   - `v8::Local<...>` return / arg — requires a scope, which the fast
///     path doesn't have (would need to allocate a CallbackScope ⇒ slow)
pub(super) fn validate_fastcall_signature(func: &ImplItemFn) -> syn::Result<()> {
    // Validate args (skipping the receiver).
    for input in func.sig.inputs.iter() {
        let typed = match input {
            FnArg::Receiver(_) => continue,
            FnArg::Typed(t) => t,
        };
        let ty = &*typed.ty;
        // Skip synthetic `&mut PinScope` / `Local<Object>` params —
        // these would never appear in a fastcall-validated function
        // because we don't have a scope, but we won't insert them
        // either. Reject explicitly to be clear.
        if let Type::Reference(_) = ty {
            return Err(syn::Error::new_spanned(
                ty,
                "#[v8_method(fastcall)]: reference params (e.g. &mut PinScope) \
                 are not supported in the fast path — fast callbacks have no scope",
            ));
        }
        if let Some(name) = crate::type_ident(ty) {
            // The full set of allowed arg type names. Keep this list in
            // sync with `gen_fastcall_arg_extract` and the CFunction
            // CTypeInfo array.
            let ok = matches!(
                name.as_str(),
                "bool"
                    | "i32"
                    | "u32"
                    | "i64"
                    | "u64"
                    | "f32"
                    | "f64"
                    | "ByteString"
            );
            if !ok {
                return Err(syn::Error::new_spanned(
                    ty,
                    format!(
                        "#[v8_method(fastcall)]: unsupported arg type `{name}` \
                         — fast path only accepts: bool, i32, u32, i64, u64, f32, f64, ByteString. \
                         Allocating types (String, Vec<u8>) and union types (Option, Result, Local<Value>) \
                         are forbidden in the fast path",
                    ),
                ));
            }
        } else {
            return Err(syn::Error::new_spanned(
                ty,
                "#[v8_method(fastcall)]: arg type not recognised — fast path \
                 only accepts named primitive types",
            ));
        }
    }

    // Validate return type.
    match &func.sig.output {
        ReturnType::Default => Ok(()),
        ReturnType::Type(_, ret_ty) => {
            let outer = crate::type_ident(ret_ty);
            match outer.as_deref() {
                Some("Result") => {
                    // Result<T, OpError>: T must be a fastcall-allowed
                    // primitive (or unit). The Err arm will be re-routed
                    // through a slow-path CallbackScope throw.
                    let inner = crate::first_generic_arg(ret_ty);
                    if let Some(t) = inner {
                        if crate::is_unit_type(t) {
                            return Ok(());
                        }
                        if let Some(name) = crate::type_ident(t) {
                            if matches!(
                                name.as_str(),
                                "bool" | "i32" | "u32" | "i64" | "u64" | "f32" | "f64"
                            ) {
                                return Ok(());
                            }
                            return Err(syn::Error::new_spanned(
                                t,
                                format!(
                                    "#[v8_method(fastcall)]: Result inner type `{name}` \
                                     is not a fastcall primitive — only bool, i32, u32, \
                                     i64, u64, f32, f64, () are allowed"
                                ),
                            ));
                        }
                    }
                    Err(syn::Error::new_spanned(
                        ret_ty,
                        "#[v8_method(fastcall)]: Result return must have a primitive Ok type",
                    ))
                }
                Some(name) => {
                    if matches!(
                        name,
                        "bool" | "i32" | "u32" | "i64" | "u64" | "f32" | "f64"
                    ) {
                        Ok(())
                    } else if crate::is_unit_type(ret_ty) {
                        Ok(())
                    } else {
                        Err(syn::Error::new_spanned(
                            ret_ty,
                            format!(
                                "#[v8_method(fastcall)]: return type `{name}` is not a \
                                 fastcall primitive — String, Vec<u8>, Option<T>, and \
                                 Local<...> are forbidden (the fast path can't allocate \
                                 or represent null). Allowed: bool, i32, u32, i64, u64, \
                                 f32, f64, (), or Result<primitive, OpError>"
                            ),
                        ))
                    }
                }
                None => {
                    if crate::is_unit_type(ret_ty) {
                        Ok(())
                    } else {
                        Err(syn::Error::new_spanned(
                            ret_ty,
                            "#[v8_method(fastcall)]: unrecognised return type",
                        ))
                    }
                }
            }
        }
    }
}

/// Map a Rust arg type to (CTypeInfo, fast-shim arg type tokens,
/// extraction tokens). Used by `gen_fastcall_callback` to build the
/// CFunctionInfo array, the extern "C" fn signature, and the per-arg
/// adaption that converts the fast-API value to the user method's
/// expected param type.
///
/// The returned tuple:
///   - `cinfo` — token to place inside the CTypeInfo array literal
///     (e.g. `v8::fast_api::Type::Uint32.as_info()`).
///   - `arg_ty` — the extern "C" fn parameter type
///     (e.g. `u32` or `*const v8::fast_api::FastApiOneByteString`).
///   - `bind` — token that adapts the raw fast-API value (in scope as
///     a binding named `<original_name>_raw`) to the user method's
///     expected type (the original Rust type), under the name
///     `<original_name>` ready for the user-method call.
fn fastcall_arg_mapping(name: &syn::Ident, ty: &Type) -> Option<(TokenStream2, TokenStream2, TokenStream2)> {
    let raw_name = format_ident!("{}_raw", name);
    let ident = crate::type_ident(ty);
    match ident.as_deref() {
        Some("bool") => Some((
            quote! { ::v8::fast_api::Type::Bool.as_info() },
            quote! { bool },
            quote! { let #name: bool = #raw_name; },
        )),
        Some("i32") => Some((
            quote! { ::v8::fast_api::Type::Int32.as_info() },
            quote! { i32 },
            quote! { let #name: i32 = #raw_name; },
        )),
        Some("u32") => Some((
            quote! { ::v8::fast_api::Type::Uint32.as_info() },
            quote! { u32 },
            quote! { let #name: u32 = #raw_name; },
        )),
        Some("i64") => Some((
            quote! { ::v8::fast_api::Type::Int64.as_info() },
            quote! { i64 },
            quote! { let #name: i64 = #raw_name; },
        )),
        Some("u64") => Some((
            quote! { ::v8::fast_api::Type::Uint64.as_info() },
            quote! { u64 },
            quote! { let #name: u64 = #raw_name; },
        )),
        Some("f32") => Some((
            quote! { ::v8::fast_api::Type::Float32.as_info() },
            quote! { f32 },
            quote! { let #name: f32 = #raw_name; },
        )),
        Some("f64") => Some((
            quote! { ::v8::fast_api::Type::Float64.as_info() },
            quote! { f64 },
            quote! { let #name: f64 = #raw_name; },
        )),
        Some("ByteString") => Some((
            quote! { ::v8::fast_api::Type::SeqOneByteString.as_info() },
            quote! { *const ::v8::fast_api::FastApiOneByteString },
            quote! {
                // SAFETY: V8 guarantees the FastApiOneByteString lives
                // for the duration of the fast call. as_bytes() returns
                // a borrowed slice; we copy into a fresh ByteString to
                // satisfy the user method's owned-bytes signature. This
                // ALLOCATES a Vec, which technically violates "no alloc
                // in the fast path" — but ByteString construction is
                // the cheapest path the user method can accept, and
                // the Vec is small (header names are typically <64
                // bytes) so the allocation is dwarfed by the saved
                // prologue. Tradeoff documented in the macro design.
                let #name = {
                    let __bytes_slice = unsafe { (&*#raw_name).as_bytes() };
                    ::zeroship_runtime::byte_string::ByteString::from_bytes(__bytes_slice.to_vec())
                };
            },
        )),
        _ => None,
    }
}

/// Map the user method's return type to:
///   - `cinfo` — return-side CTypeInfo (e.g. `Type::Bool.as_info()`).
///   - `ret_ty` — extern "C" fn return type tokens.
///   - `marshal` — token that takes the user method's call expression
///     bound to `__r` and produces the extern "C" return value. For
///     `Result<T, OpError>` the Err arm uses CallbackScope::new(options)
///     to throw, then returns a sentinel zero-value (V8 ignores the
///     return when an exception is pending).
fn fastcall_return_mapping(output: &ReturnType) -> Option<(TokenStream2, TokenStream2, TokenStream2)> {
    let unit_response = (
        quote! { ::v8::fast_api::Type::Void.as_info() },
        quote! { () },
        quote! { let _ = __r; },
    );
    match output {
        ReturnType::Default => Some(unit_response),
        ReturnType::Type(_, ret_ty) => {
            let outer = crate::type_ident(ret_ty);
            match outer.as_deref() {
                Some("Result") => {
                    // Result<T, OpError>: for the fast path, on Err we
                    // construct a CallbackScope from options and throw
                    // the exception there (which deopts and routes the
                    // call to the slow path next iteration). The fn
                    // returns a sentinel default(zero) — V8 ignores the
                    // return value when an exception is pending.
                    let inner = crate::first_generic_arg(ret_ty);
                    if let Some(t) = inner {
                        if crate::is_unit_type(t) {
                            return Some((
                                quote! { ::v8::fast_api::Type::Void.as_info() },
                                quote! { () },
                                quote! {
                                    match __r {
                                        Ok(_) => {}
                                        Err(__err) => {
                                            let __opts: &::v8::fast_api::FastApiCallbackOptions = unsafe { &*__options };
                                            ::v8::callback_scope!(unsafe let __cb_scope, __opts);
                                            let __msg = ::v8::String::new(__cb_scope, &__err.message)
                                                .unwrap();
                                            let __exc = ::v8::Exception::type_error(__cb_scope, __msg);
                                            __cb_scope.throw_exception(__exc);
                                        }
                                    }
                                },
                            ));
                        }
                        if let Some(name) = crate::type_ident(t) {
                            let (cinfo, ret_ty_tok, sentinel) = match name.as_str() {
                                "bool" => (
                                    quote! { ::v8::fast_api::Type::Bool.as_info() },
                                    quote! { bool },
                                    quote! { false },
                                ),
                                "i32" => (
                                    quote! { ::v8::fast_api::Type::Int32.as_info() },
                                    quote! { i32 },
                                    quote! { 0i32 },
                                ),
                                "u32" => (
                                    quote! { ::v8::fast_api::Type::Uint32.as_info() },
                                    quote! { u32 },
                                    quote! { 0u32 },
                                ),
                                "i64" => (
                                    quote! { ::v8::fast_api::Type::Int64.as_info() },
                                    quote! { i64 },
                                    quote! { 0i64 },
                                ),
                                "u64" => (
                                    quote! { ::v8::fast_api::Type::Uint64.as_info() },
                                    quote! { u64 },
                                    quote! { 0u64 },
                                ),
                                "f32" => (
                                    quote! { ::v8::fast_api::Type::Float32.as_info() },
                                    quote! { f32 },
                                    quote! { 0.0f32 },
                                ),
                                "f64" => (
                                    quote! { ::v8::fast_api::Type::Float64.as_info() },
                                    quote! { f64 },
                                    quote! { 0.0f64 },
                                ),
                                _ => return None,
                            };
                            return Some((
                                cinfo,
                                ret_ty_tok,
                                quote! {
                                    match __r {
                                        Ok(__v) => __v,
                                        Err(__err) => {
                                            let __opts: &::v8::fast_api::FastApiCallbackOptions = unsafe { &*__options };
                                            ::v8::callback_scope!(unsafe let __cb_scope, __opts);
                                            let __msg = ::v8::String::new(__cb_scope, &__err.message)
                                                .unwrap();
                                            let __exc = ::v8::Exception::type_error(__cb_scope, __msg);
                                            __cb_scope.throw_exception(__exc);
                                            #sentinel
                                        }
                                    }
                                },
                            ));
                        }
                    }
                    None
                }
                Some("bool") => Some((
                    quote! { ::v8::fast_api::Type::Bool.as_info() },
                    quote! { bool },
                    quote! { __r },
                )),
                Some("i32") => Some((
                    quote! { ::v8::fast_api::Type::Int32.as_info() },
                    quote! { i32 },
                    quote! { __r },
                )),
                Some("u32") => Some((
                    quote! { ::v8::fast_api::Type::Uint32.as_info() },
                    quote! { u32 },
                    quote! { __r },
                )),
                Some("i64") => Some((
                    quote! { ::v8::fast_api::Type::Int64.as_info() },
                    quote! { i64 },
                    quote! { __r },
                )),
                Some("u64") => Some((
                    quote! { ::v8::fast_api::Type::Uint64.as_info() },
                    quote! { u64 },
                    quote! { __r },
                )),
                Some("f32") => Some((
                    quote! { ::v8::fast_api::Type::Float32.as_info() },
                    quote! { f32 },
                    quote! { __r },
                )),
                Some("f64") => Some((
                    quote! { ::v8::fast_api::Type::Float64.as_info() },
                    quote! { f64 },
                    quote! { __r },
                )),
                _ => {
                    if crate::is_unit_type(ret_ty) {
                        Some(unit_response)
                    } else {
                        None
                    }
                }
            }
        }
    }
}

/// Mangled identifier of the fastcall extern "C" fn.
fn fastcall_fn_ident(class_ty: &syn::Ident, method: &syn::Ident) -> syn::Ident {
    format_ident!("__{}_{}_fastcall_fn", class_ty, method)
}

/// Mangled identifier of the per-method static CFunction descriptor.
pub(super) fn fastcall_cfn_ident(class_ty: &syn::Ident, method: &syn::Ident) -> syn::Ident {
    format_ident!("__{}_{}_FASTCALL_CFN", class_ty, method)
}

/// Mangled identifier of the per-method static CFunctionInfo descriptor.
/// We need a separate static for the CFunctionInfo because its address
/// must live as long as the CFunction it's referenced from — V8 reads
/// from `*const CFunctionInfo` at JIT time.
fn fastcall_cinfo_ident(class_ty: &syn::Ident, method: &syn::Ident) -> syn::Ident {
    format_ident!("__{}_{}_FASTCALL_CINFO", class_ty, method)
}

/// Emit the fastcall shim for a method or getter:
///   - `extern "C" fn __<Class>_<method>_fastcall_fn(recv, args..., options) -> ret`
///   - `static __<Class>_<method>_FASTCALL_CINFO: CFunctionInfo = ...`
///   - `static __<Class>_<method>_FASTCALL_CFN: CFunction = ...`
///
/// The shim:
///   1. Recovers `*const Self` from internal-field-1 via
///      `get_aligned_pointer_from_internal_field(1, 0)` — a single load
///      instruction. No scope, no External unwrap.
///   2. Adapts each fast-API typed arg to the user method's Rust type
///      via `fastcall_arg_mapping`'s `bind` snippet (a no-op for
///      primitives; a Vec copy for ByteString from FastApiOneByteString).
///   3. Calls the user method as `<Class>::method(&*self, args...)`.
///   4. For Result returns, splits Ok/Err: Ok unwraps into the return
///      slot; Err allocates a CallbackScope, throws via TypeError, and
///      returns a zero sentinel (V8 ignores the slot when an exception
///      is pending).
///
/// Brand check: not emitted in the fast path. V8's CFunction signature
/// (typed `Local<Object>` receiver) is enforced at JIT time — Turbofan
/// inserts an inline-cache shape check before dispatch, so only objects
/// whose hidden class matches the cached one ever reach the fast path.
/// Cross-class deception (e.g. `Headers.prototype.has.call(blob)`) hits
/// a shape-mismatch deopt and falls through to the slow callback, which
/// runs the prototype-walk brand check and throws "Illegal invocation".
/// See the design doc for the chain-of-trust analysis.
pub(super) fn gen_fastcall_callback(class_ty: &syn::Ident, m: &ClassMethod) -> Option<TokenStream2> {
    let method_name = &m.func.sig.ident;
    let fn_name = fastcall_fn_ident(class_ty, method_name);
    let cinfo_name = fastcall_cinfo_ident(class_ty, method_name);
    let cfn_name = fastcall_cfn_ident(class_ty, method_name);

    let params = parse_params_skipping_self(m.func);

    // Build per-arg pieces.
    let mut arg_cinfos: Vec<TokenStream2> = Vec::new();
    // Fast path: receiver is the first CFunction arg (V8Value).
    arg_cinfos.push(quote! { ::v8::fast_api::Type::V8Value.as_info() });

    let mut arg_decls: Vec<TokenStream2> = Vec::new();
    let mut arg_binds: Vec<TokenStream2> = Vec::new();
    let mut arg_call_idents: Vec<&syn::Ident> = Vec::new();

    for p in &params {
        let mapping = fastcall_arg_mapping(&p.name, &p.ty)?;
        let (cinfo, raw_ty, bind) = mapping;
        arg_cinfos.push(cinfo);
        let raw_name = format_ident!("{}_raw", p.name);
        arg_decls.push(quote! { #raw_name: #raw_ty });
        arg_binds.push(bind);
        arg_call_idents.push(&p.name);
    }

    // Trailing CallbackOptions arg — we always emit it so the Result
    // path (which needs to throw via CallbackScope::new(options)) has
    // access. For the no-throw case the cost is one extra ABI slot,
    // negligible.
    arg_cinfos.push(quote! { ::v8::fast_api::Type::CallbackOptions.as_info() });

    let (ret_cinfo, ret_ty_tok, ret_marshal) = fastcall_return_mapping(&m.func.sig.output)?;

    // CFunction and CFunctionInfo hold raw pointers and are therefore
    // !Sync. They're safe to share across threads in practice — V8
    // reads them at JIT compile time on whichever thread compiled the
    // function, and the underlying data is immutable. We wrap each in
    // a tuple-struct that asserts Sync via unsafe impl, then deref the
    // underlying value at the call site.
    let cinfo_wrapper = format_ident!("{}_Wrapper", cinfo_name);
    let cfn_wrapper = format_ident!("{}_Wrapper", cfn_name);

    Some(quote! {
        /// !Sync wrapper around CFunctionInfo. The wrapped value holds
        /// raw pointers (`*const v8_CTypeInfo`) which Rust auto-derives
        /// !Sync for — but the data is immutable after construction and
        /// V8 reads it on the JIT thread, so cross-thread sharing is
        /// sound. The wrapper is the standard "transparent !Sync escape
        /// hatch" pattern (same shape as `lazy_static` users).
        #[doc(hidden)]
        #[allow(non_camel_case_types)]
        struct #cinfo_wrapper(::v8::fast_api::CFunctionInfo);
        unsafe impl Sync for #cinfo_wrapper {}

        /// V8 fast API CFunctionInfo descriptor for this method.
        /// Generated by `#[v8_method(fastcall)]` / `#[v8_getter(fastcall)]`.
        /// Static so its address is stable for V8 to read at JIT time
        /// (CFunction holds `*const CFunctionInfo`).
        #[doc(hidden)]
        #[allow(non_upper_case_globals)]
        static #cinfo_name: #cinfo_wrapper = #cinfo_wrapper(
            ::v8::fast_api::CFunctionInfo::new(
                #ret_cinfo,
                &[#(#arg_cinfos),*],
                ::v8::fast_api::Int64Representation::Number,
            )
        );

        /// !Sync wrapper around CFunction — same rationale as the
        /// CFunctionInfo wrapper above.
        #[doc(hidden)]
        #[allow(non_camel_case_types)]
        struct #cfn_wrapper(::v8::fast_api::CFunction);
        unsafe impl Sync for #cfn_wrapper {}

        /// V8 fast API CFunction descriptor for this method. Holds
        /// `(address, *const CFunctionInfo)`. Wired into the
        /// FunctionTemplate via `builder(slow).build_fast(scope, &[#cfn_name.0])`
        /// so Turbofan can inline the typed-shape call at hot sites.
        #[doc(hidden)]
        #[allow(non_upper_case_globals)]
        static #cfn_name: #cfn_wrapper = #cfn_wrapper(
            ::v8::fast_api::CFunction::new(
                #fn_name as *const ::std::ffi::c_void,
                &#cinfo_name.0,
            )
        );

        /// Fast-path shim for `<#class_ty>::<#method_name>`. Called by
        /// V8 Turbofan when the optimised JIT inlines this method at a
        /// hot site. The signature matches the CFunctionInfo above
        /// exactly — V8 enforces the receiver type at JIT time, so
        /// `recv` is guaranteed to be a `<#class_ty>` instance (any
        /// shape mismatch deopts to the slow callback).
        ///
        /// Receiver recovery uses internal-field-1 aligned pointer
        /// (set by `gen_box_and_install_finalizer` when any method on
        /// the class is fastcall). Slot 0 retains the External + GC
        /// finalizer for the standard wrapper teardown.
        ///
        /// SAFETY:
        ///   - Slot 1 holds the `Box<#class_ty>` raw pointer set at
        ///     construction time. As long as the wrapper is reachable
        ///     by V8, the Box stays alive (slot 0's finalizer fires
        ///     only on GC of the wrapper).
        ///   - The receiver-type check is enforced by V8 at JIT time
        ///     via the CFunction's typed signature. Cross-class call
        ///     attempts deopt to the slow path before reaching this
        ///     shim.
        ///   - We take `&Self` only — fastcall is rejected at macro
        ///     time for `&mut self`, so no aliasing risk.
        #[doc(hidden)]
        #[allow(non_snake_case, unused_variables, unused_unsafe)]
        extern "C" fn #fn_name(
            __recv: ::v8::Local<::v8::Object>,
            #(#arg_decls,)*
            __options: *mut ::v8::fast_api::FastApiCallbackOptions,
        ) -> #ret_ty_tok {
            // Recover the boxed instance via aligned pointer in slot 1.
            // tag=0 matches the value passed to set_aligned_pointer_in_internal_field.
            let __raw: *const ::std::ffi::c_void = unsafe {
                __recv.get_aligned_pointer_from_internal_field(1, 0)
            };
            let __instance: &#class_ty = unsafe { &*(__raw as *const #class_ty) };

            // Per-arg adaptation from fast-API raw type to user method
            // expected type (no-op for primitives; Vec copy for
            // ByteString from FastApiOneByteString).
            #(#arg_binds)*

            // Call the user method. The receiver is `&Self`; user
            // method's signature MUST match (validated at macro time
            // by `validate_fastcall_signature`).
            let __r = <#class_ty>::#method_name(__instance, #(#arg_call_idents),*);

            // Marshal the return value. For `Result`, this branches
            // Ok/Err; the Err arm allocates a CallbackScope and throws.
            #ret_marshal
        }
    })
}
