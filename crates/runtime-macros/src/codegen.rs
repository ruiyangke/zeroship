//! Return-value codegen helpers — emit the Rust-value → V8 marshalling
//! tokens consumed by the slow-path `FunctionCallback` codegen in
//! `v8_class::emit::method` / `static_op`.
//!
//! Wave 4 cleanup commit (design `docs/proposals/runtime-macros-
//! refactor.md` §3.7 follow-up): split out from `lib.rs` so the latter
//! can shrink toward its ≤500 LOC target. The helpers live here as a
//! cohesive unit because they share the same `rv: ReturnValue` /
//! `scope: &mut PinScope` ABI assumption — they're emit-time-only and
//! never evaluated at macro time.
//!
//! All quoted snippets bind in scope:
//!   - `scope`  (the V8 scope, `&mut PinScope`)
//!   - `rv`     (the V8 ReturnValue)
//! Some additionally bind `__r`, `__ok`, `__inner`, `__vec`, `__bytes`
//! depending on the wrapper shape — those names are documented per-helper.
//!
//! See [`gen_call_return`] for the dispatcher; the per-shape emitters
//! ([`gen_scalar_set`], [`gen_option_some_set`], [`gen_vec_set`],
//! [`gen_vec_vec_u8_set`], [`gen_vec_u8_set`]) are exposed pub(crate)
//! so the WebIDL derive macros can reuse them where the marshalling
//! shape lines up.

use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{Ident, ReturnType, Type};

use crate::{first_generic_arg, is_unit_type, is_vec_u8, is_vec_vec_u8, type_ident};

// ---------------------------------------------------------------------------
// V8 string + error throw helpers
// ---------------------------------------------------------------------------

/// Emit `v8::String::new(<scope>, <lit>).unwrap()` for a string literal
/// or interpolated str token.
///
/// The 50+ call sites of this pattern across the crate's emit code are
/// noisy — `v8::String::new` returns `Option<Local<String>>` and is
/// `None` only on V8 string-pool exhaustion (a near-zero probability
/// event in practice; V8 itself aborts on isolate OOM well before
/// this), so every site does an `.unwrap()`. Centralising the pattern:
///   - Reduces visual noise in the generated code's templates.
///   - Gives one place to switch to a panic-free fallback if we ever
///     decide to surface OOM as a V8 RangeError instead of aborting.
///   - Makes drift easier to spot — if a future change wants the
///     `new_from_onebyte_const` ASCII fast path, it's one helper edit
///     instead of 50 grep-and-replace sites.
///
/// `scope_expr` is interpolated as the V8 scope binding (almost always
/// `scope` in callbacks; the parameter form lets factory codegen pass
/// a different binding without rebinding). `lit` is interpolated
/// directly — pass either a string literal (`"prototype"`) or a
/// pre-built token stream that names a `&str` binding (a `String` /
/// `&str` ident, etc.).
///
/// Use [`must_str_abs`] when the surrounding emit code uses the
/// absolute `::v8::` path (e.g. derive macros' emit, where the user's
/// crate may not have `use v8;` imported).
pub(crate) fn must_str(scope_expr: &TokenStream2, lit: &TokenStream2) -> TokenStream2 {
    quote! { v8::String::new(#scope_expr, #lit).unwrap() }
}

/// Absolute-path variant of [`must_str`]. Emits
/// `::v8::String::new(<scope>, <lit>).unwrap()`. Used by the WebIDL
/// derive macros, whose emit lives in user crates that may not have
/// imported `v8` directly.
pub(crate) fn must_str_abs(scope_expr: &TokenStream2, lit: &TokenStream2) -> TokenStream2 {
    quote! { ::v8::String::new(#scope_expr, #lit).unwrap() }
}

/// Single source of truth for the OpError → V8 exception dispatch.
/// Emits the full 6-variant match (TypeError, RangeError, DomException,
/// NodeError, Error, JsValue passthrough) used wherever the macro
/// translates a `Result<T, OpError>` boundary into a JS `throw`.
///
/// `scope_expr` and `err_expr` are inserted as the V8 scope and the
/// `OpError` reference, respectively — typically `quote!(scope)` and
/// `quote!(__err)` in slow-path callbacks. They're parametric so the
/// helper can be re-used from sites that bind these under different
/// names (e.g. async-method post-resolution).
///
/// Callers MUST emit `return;` (or whatever control-flow primitive
/// suits the surrounding callback shape) AFTER this block — the helper
/// only produces the exception-throw, never the unwind.
pub(crate) fn gen_throw_op_error_arms(
    scope_expr: &TokenStream2,
    err_expr: &TokenStream2,
) -> TokenStream2 {
    let msg_init = must_str(scope_expr, &quote! { &(#err_expr).message });
    quote! {
        if let ::zeroship_runtime::macro_runtime::state::OpErrorKind::JsValue(__global) = &(#err_expr).kind {
            let __local = v8::Local::new(#scope_expr, __global);
            (#scope_expr).throw_exception(__local);
        } else {
            let __msg = #msg_init;
            let __exc: v8::Local<v8::Value> = match &(#err_expr).kind {
                ::zeroship_runtime::macro_runtime::state::OpErrorKind::TypeError => v8::Exception::type_error(#scope_expr, __msg),
                ::zeroship_runtime::macro_runtime::state::OpErrorKind::RangeError => v8::Exception::range_error(#scope_expr, __msg),
                ::zeroship_runtime::macro_runtime::state::OpErrorKind::DomException(__name) => {
                    ::zeroship_runtime::macro_runtime::dom::exception::build(#scope_expr, &(#err_expr).message, __name).into()
                }
                ::zeroship_runtime::macro_runtime::state::OpErrorKind::NodeError(__code) => {
                    ::zeroship_runtime::macro_runtime::node_error::build_node_exception(#scope_expr, __code, &(#err_expr).message)
                }
                ::zeroship_runtime::macro_runtime::state::OpErrorKind::Error => v8::Exception::error(#scope_expr, __msg),
                // Already handled by the early-return above.
                ::zeroship_runtime::macro_runtime::state::OpErrorKind::JsValue(_) => unreachable!(),
            };
            (#scope_expr).throw_exception(__exc);
        }
    }
}

/// Emit the throw machinery for an `OpError` named `__err` in scope.
/// Used by the per-arg-extraction codegen (ByteString / USVString /
/// EnforceRange / etc.) where a conversion failure must surface as a
/// V8 exception and `return` from the V8 callback. The macro emits
/// `return` after this snippet — that's the caller's responsibility.
pub(crate) fn gen_extract_throw() -> TokenStream2 {
    let scope = quote! { scope };
    let err = quote! { __err };
    gen_throw_op_error_arms(&scope, &err)
}

/// Emit the per-arg extraction tokens for the slow-path
/// FunctionCallback. Wave 4b — delegates to the table-driven
/// [`super::known_type::KnownType`] classifier (design §3.7, closes
/// F10 / H8). The body here is a thin shim: classify the type once,
/// ask the variant for its emission. The 13-arm string-keyed dispatch
/// and the `clamp_kind` / `wrap_kind` standalone helpers (with their
/// `unreachable!` arms) used to live inline; they fold into
/// `KnownType::extract_tokens` and the variant data, respectively.
pub(crate) fn gen_extract(index: usize, name: &Ident, ty: &Type) -> TokenStream2 {
    let idx = index as i32;
    crate::known_type::KnownType::from_ty(ty).extract_tokens(name, idx)
}

// ---------------------------------------------------------------------------
// Return value codegen (Rust value → V8 value)
// ---------------------------------------------------------------------------

/// Emit the V8 setter for `Vec<u8>` — materialised as a `Uint8Array`.
///
/// Returning a plain `ArrayBuffer` was easier but spec-wrong for every
/// real consumer: WHATWG TextEncoder.encode and WebCrypto digest both
/// return `Uint8Array`, and downstream JS code (streams, fetch body
/// coercion) typically branches on `instanceof Uint8Array` to decide
/// whether to wrap. Returning `Uint8Array` matches the spec contract
/// without forcing every caller to do `new Uint8Array(arrayBuffer)`.
pub(crate) fn gen_vec_u8_set(val: &TokenStream2) -> TokenStream2 {
    quote! {
        let __bytes = #val;
        let __len = __bytes.len();
        let __ab = v8::ArrayBuffer::new(scope, __len);
        let __store = __ab.get_backing_store();
        for (__i, &__b) in __bytes.iter().enumerate() {
            __store[__i].set(__b);
        }
        let __u8 = v8::Uint8Array::new(scope, __ab, 0, __len).unwrap();
        rv.set(__u8.into());
    }
}

/// Emit the V8 setter for a scalar value referenced by `val` tokens.
/// Dispatches on the type's last-segment ident — bool / u32 / i32 / f64
/// have direct V8 constructors; everything else falls through to a
/// `to_string`-shaped `must_str` path.
pub(crate) fn gen_scalar_set(ty: &Type, val: &TokenStream2) -> TokenStream2 {
    if is_vec_u8(ty) {
        return gen_vec_u8_set(val);
    }
    // `v8::Local<v8::Value>` (or any v8::Local<v8::T>): pass straight
    // to `rv.set`. Used by methods that build their own JS object,
    // typed array, etc. — e.g. TextEncoder.encodeInto returning
    // `{ read, written }`.
    if type_ident(ty).as_deref() == Some("Local") {
        return quote! { rv.set(#val.into()); };
    }
    match type_ident(ty).as_deref() {
        Some("bool") => quote! { rv.set(v8::Boolean::new(scope, #val).into()); },
        Some("u32") => quote! { rv.set(v8::Integer::new_from_unsigned(scope, #val).into()); },
        Some("i32") => quote! { rv.set(v8::Integer::new(scope, #val).into()); },
        Some("f64") => quote! { rv.set(v8::Number::new(scope, #val).into()); },
        // Default: String
        _ => {
            let scope = quote! { scope };
            let v_init = must_str(&scope, &quote! { &#val });
            quote! {
                let __v = #v_init;
                rv.set(__v.into());
            }
        }
    }
}

/// Emit the V8 setter for the unwrapped `Some` branch of an
/// `Option<T>` return. The user method's value lives in `__inner`
/// (already unwrapped by the caller's `match`).
pub(crate) fn gen_option_some_set(ty: &Type) -> TokenStream2 {
    if is_vec_u8(ty) {
        // Option<Vec<u8>> for getters like `Headers.get(name) ->
        // ByteString?`. Emit a Latin-1 one-byte string so byte fidelity
        // is preserved (encoded session tokens, e.g. high-bit Set-Cookie
        // values, must round-trip).
        return quote! {
            let __v = v8::String::new_from_one_byte(
                scope,
                __inner.as_slice(),
                v8::NewStringType::Normal,
            ).unwrap();
            rv.set(__v.into());
        };
    }
    match type_ident(ty).as_deref() {
        Some("bool") => quote! { rv.set(v8::Boolean::new(scope, __inner).into()); },
        Some("u32") => quote! { rv.set(v8::Integer::new_from_unsigned(scope, __inner).into()); },
        _ => {
            let scope = quote! { scope };
            let v_init = must_str(&scope, &quote! { &__inner });
            quote! {
                let __v = #v_init;
                rv.set(__v.into());
            }
        }
    }
}

/// Emit the V8 setter for `Vec<String>` — a `v8::Array` of strings.
/// The user method's value is bound to `__vec` by the caller.
pub(crate) fn gen_vec_set() -> TokenStream2 {
    let scope = quote! { scope };
    let v_init = must_str(&scope, &quote! { __s });
    quote! {
        let __arr = v8::Array::new(scope, __vec.len() as i32);
        for (__i, __s) in __vec.iter().enumerate() {
            let __v = #v_init;
            __arr.set_index(scope, __i as u32, __v.into());
        }
        rv.set(__arr.into());
    }
}

/// Emit the V8 setter for `Vec<Vec<u8>>` — a `v8::Array` of Latin-1
/// one-byte strings (a WebIDL ByteString round-trips faithfully —
/// bytes 0x80–0xFF survive). Used by methods like
/// `Headers.getSetCookie() -> sequence<ByteString>`.
pub(crate) fn gen_vec_vec_u8_set() -> TokenStream2 {
    quote! {
        let __arr = v8::Array::new(scope, __vec.len() as i32);
        for (__i, __bytes) in __vec.iter().enumerate() {
            let __s = v8::String::new_from_one_byte(
                scope,
                __bytes.as_slice(),
                v8::NewStringType::Normal,
            ).unwrap();
            __arr.set_index(scope, __i as u32, __s.into());
        }
        rv.set(__arr.into());
    }
}

/// Emit the throw machinery for an `OpError` named `__err` in scope.
/// Routed through the shared [`gen_throw_op_error_arms`] helper —
/// single source of truth for the 6-variant OpErrorKind dispatch.
/// Adding a 7th variant means editing one match in one helper.
fn gen_throw_error() -> TokenStream2 {
    let scope = quote! { scope };
    let err = quote! { __err };
    gen_throw_op_error_arms(&scope, &err)
}

/// Emit the function-call expression + return-value handling.
///
/// `call` is the pre-built call expression (e.g.
/// `__instance.method(a, b)`). Used by `#[v8_class]` codegen to
/// marshal whatever the user's method returned into the V8
/// `ReturnValue`.
///
/// The dispatcher branches on the return type's outer shape
/// (`ReturnType::Default`, `Result`, `Option`, `Vec`, primitive,
/// `Local`, fallthrough) and delegates the actual setter quote to
/// the per-shape helper.
pub(crate) fn gen_call_return(call: &TokenStream2, output: &ReturnType) -> TokenStream2 {
    match output {
        ReturnType::Default => quote! { #call; },
        ReturnType::Type(_, ty) => {
            let outer = type_ident(ty);
            match outer.as_deref() {
                // --- Result<T, OpError> ---
                Some("Result") => {
                    let inner = first_generic_arg(ty);
                    let inner_ident = inner.and_then(type_ident);
                    let ok_handling = if inner.map(is_unit_type).unwrap_or(false) {
                        // `Result<(), OpError>` — Ok variant has no value
                        // to surface. Bind it to `_` so the unused-let
                        // lint doesn't fire, and leave `rv` untouched
                        // (defaults to `undefined`). Used by mutator
                        // methods like Headers.append.
                        quote! { let _ = __ok; }
                    } else {
                        match inner_ident.as_deref() {
                            Some("Option") => {
                                let inner2 = inner.and_then(first_generic_arg);
                                let some_set = inner2.map(gen_option_some_set).unwrap_or_else(
                                    || gen_option_some_set(&syn::parse_quote!(String)),
                                );
                                quote! {
                                    match __ok {
                                        Some(__inner) => { #some_set }
                                        None => rv.set(v8::null(scope).into()),
                                    }
                                }
                            }
                            Some("Vec") => {
                                if inner.map(is_vec_u8).unwrap_or(false) {
                                    let val = quote! { __ok };
                                    gen_vec_u8_set(&val)
                                } else if inner.map(is_vec_vec_u8).unwrap_or(false) {
                                    let vv_set = gen_vec_vec_u8_set();
                                    quote! {
                                        let __vec = __ok;
                                        #vv_set
                                    }
                                } else {
                                    let vec_set = gen_vec_set();
                                    quote! {
                                        let __vec = __ok;
                                        #vec_set
                                    }
                                }
                            }
                            _ => {
                                let val = quote! { __ok };
                                inner.map(|t| gen_scalar_set(t, &val)).unwrap_or_else(|| {
                                    gen_scalar_set(&syn::parse_quote!(String), &val)
                                })
                            }
                        }
                    };
                    let throw = gen_throw_error();
                    quote! {
                        match #call {
                            Ok(__ok) => { #ok_handling }
                            Err(__err) => { #throw }
                        }
                    }
                }

                // --- Option<T> ---
                Some("Option") => {
                    let inner = first_generic_arg(ty);
                    let some_set = inner
                        .map(gen_option_some_set)
                        .unwrap_or_else(|| gen_option_some_set(&syn::parse_quote!(String)));
                    quote! {
                        match #call {
                            Some(__inner) => { #some_set }
                            None => rv.set(v8::null(scope).into()),
                        }
                    }
                }

                // --- Vec<T> ---
                Some("Vec") => {
                    if is_vec_u8(ty) {
                        let val = quote! { __vec };
                        let ab_set = gen_vec_u8_set(&val);
                        quote! {
                            let __vec = #call;
                            #ab_set
                        }
                    } else if is_vec_vec_u8(ty) {
                        let vv_set = gen_vec_vec_u8_set();
                        quote! {
                            let __vec = #call;
                            #vv_set
                        }
                    } else {
                        let vec_set = gen_vec_set();
                        quote! {
                            let __vec = #call;
                            #vec_set
                        }
                    }
                }

                // --- Scalars ---
                //
                // Bind the user-method call's result to a local FIRST,
                // then construct the V8 value. Inlining `#call` into
                // `v8::Integer::new_from_unsigned(scope, #call)` would
                // borrow `scope` twice in the same expression — once
                // immutably for the first arg, once mutably inside
                // `#call` (when the user method itself takes
                // `scope: &mut PinScope`). E0502.
                Some("bool") => quote! {
                    let __r = #call;
                    rv.set(v8::Boolean::new(scope, __r).into());
                },
                Some("u32") => quote! {
                    let __r = #call;
                    rv.set(v8::Integer::new_from_unsigned(scope, __r).into());
                },
                Some("i32") => quote! {
                    let __r = #call;
                    rv.set(v8::Integer::new(scope, __r).into());
                },
                Some("f64") => quote! {
                    let __r = #call;
                    rv.set(v8::Number::new(scope, __r).into());
                },
                Some("String") => {
                    let scope = quote! { scope };
                    let v_init = must_str(&scope, &quote! { &__r });
                    quote! {
                        let __r = #call;
                        let __v = #v_init;
                        rv.set(__v.into());
                    }
                }

                // --- Direct V8 value (Local<Value>, Local<Object>, etc.) ---
                // Used by methods that build a custom JS shape (e.g.
                // TextEncoder.encodeInto returning `{ read, written }`).
                Some("Local") => quote! { rv.set(#call.into()); },

                _ => quote! { #call; },
            }
        }
    }
}
