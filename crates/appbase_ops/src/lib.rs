//! Proc macro for generating V8 callback wrappers from plain Rust functions.
//!
//! Eliminates the per-op boilerplate of argument extraction, state access,
//! return value conversion, and error handling.
//!
//! # Usage
//!
//! ```ignore
//! // Sync, no state:
//! #[appbase_op]
//! fn url_can_parse(input: String, base: Option<String>) -> bool { ... }
//!
//! // Sync, with shared state:
//! #[appbase_op(state)]
//! fn kv_get(state: SharedState, key: String) -> Option<String> { ... }
//!
//! // Async (returns Promise, state plumbing is auto-generated):
//! #[appbase_op(async)]
//! async fn op_fetch(method: String, url: String) -> String { ... }
//! ```
//!
//! Each macro invocation keeps the original function and generates a
//! `{name}_callback` function with the V8 callback signature.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::{
    parse_macro_input, FnArg, GenericArgument, Ident, ItemFn, Pat, PathArguments, ReturnType,
    Type, TypePath,
};

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[proc_macro_attribute]
pub fn appbase_op(attr: TokenStream, item: TokenStream) -> TokenStream {
    let attr_str = attr.to_string();
    let is_async = attr_str.contains("async");
    let needs_state = attr_str.contains("state");

    let input_fn = parse_macro_input!(item as ItemFn);

    let result = if is_async {
        generate_async(&input_fn)
    } else {
        generate_sync(needs_state, &input_fn)
    };

    match result {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

// ---------------------------------------------------------------------------
// Type helpers
// ---------------------------------------------------------------------------

/// Extract the last segment identifier from a type path (e.g. `String`, `Option`, `Result`).
fn type_ident(ty: &Type) -> Option<String> {
    if let Type::Path(TypePath { path, .. }) = ty {
        path.segments.last().map(|s| s.ident.to_string())
    } else {
        None
    }
}

/// Extract the first generic type argument (e.g. `String` from `Option<String>`).
fn first_generic_arg(ty: &Type) -> Option<&Type> {
    if let Type::Path(TypePath { path, .. }) = ty {
        if let Some(seg) = path.segments.last() {
            if let PathArguments::AngleBracketed(ref ab) = seg.arguments {
                for arg in &ab.args {
                    if let GenericArgument::Type(t) = arg {
                        return Some(t);
                    }
                }
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Parameter parsing
// ---------------------------------------------------------------------------

struct Param {
    name: Ident,
    ty: Type,
}

fn parse_params(f: &ItemFn) -> Vec<Param> {
    f.sig
        .inputs
        .iter()
        .filter_map(|arg| {
            if let FnArg::Typed(pt) = arg {
                if let Pat::Ident(pi) = &*pt.pat {
                    return Some(Param {
                        name: pi.ident.clone(),
                        ty: (*pt.ty).clone(),
                    });
                }
            }
            None
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Argument extraction codegen (JS value → Rust type)
// ---------------------------------------------------------------------------

fn gen_extract(index: usize, name: &Ident, ty: &Type) -> TokenStream2 {
    let idx = index as i32;
    let ident = type_ident(ty);

    match ident.as_deref() {
        Some("Option") => {
            let inner = first_generic_arg(ty).and_then(type_ident);
            match inner.as_deref() {
                Some("u32") => quote! {
                    let #name: Option<u32> = if args.length() > #idx
                        && !args.get(#idx).is_null_or_undefined()
                    {
                        args.get(#idx).uint32_value(scope)
                    } else {
                        None
                    };
                },
                Some("i32") => quote! {
                    let #name: Option<i32> = if args.length() > #idx
                        && !args.get(#idx).is_null_or_undefined()
                    {
                        args.get(#idx).int32_value(scope)
                    } else {
                        None
                    };
                },
                // Default to Option<String>
                _ => quote! {
                    let #name: Option<String> = if args.length() > #idx
                        && !args.get(#idx).is_null_or_undefined()
                    {
                        Some(args.get(#idx).to_rust_string_lossy(scope))
                    } else {
                        None
                    };
                },
            }
        }
        Some("bool") => quote! {
            let #name: bool = args.get(#idx).boolean_value(scope);
        },
        Some("u32") => quote! {
            let #name: u32 = args.get(#idx).uint32_value(scope).unwrap_or(0);
        },
        Some("i32") => quote! {
            let #name: i32 = args.get(#idx).int32_value(scope).unwrap_or(0);
        },
        Some("f64") => quote! {
            let #name: f64 = args.get(#idx).number_value(scope).unwrap_or(0.0);
        },
        // Default: String (covers named types like String, &str aliases, etc.)
        _ => quote! {
            let #name: String = args.get(#idx).to_rust_string_lossy(scope);
        },
    }
}

// ---------------------------------------------------------------------------
// Return value codegen (Rust value → V8 value)
// ---------------------------------------------------------------------------

/// Generate code to convert a scalar value (referenced by `val` tokens) to a V8 return value.
fn gen_scalar_set(ty: &Type, val: &TokenStream2) -> TokenStream2 {
    match type_ident(ty).as_deref() {
        Some("bool") => quote! { rv.set(v8::Boolean::new(scope, #val).into()); },
        Some("u32") => quote! { rv.set(v8::Integer::new_from_unsigned(scope, #val).into()); },
        Some("i32") => quote! { rv.set(v8::Integer::new(scope, #val).into()); },
        Some("f64") => quote! { rv.set(v8::Number::new(scope, #val).into()); },
        // Default: String
        _ => quote! {
            let __v = v8::String::new(scope, &#val).unwrap();
            rv.set(__v.into());
        },
    }
}

/// Generate code to convert an `Option<T>` inner value to V8.
fn gen_option_some_set(ty: &Type) -> TokenStream2 {
    match type_ident(ty).as_deref() {
        Some("bool") => quote! { rv.set(v8::Boolean::new(scope, __inner).into()); },
        Some("u32") => quote! { rv.set(v8::Integer::new_from_unsigned(scope, __inner).into()); },
        _ => quote! {
            let __v = v8::String::new(scope, &__inner).unwrap();
            rv.set(__v.into());
        },
    }
}

/// Generate code to build a `v8::Array` from a `Vec<String>`.
fn gen_vec_set() -> TokenStream2 {
    quote! {
        let __arr = v8::Array::new(scope, __vec.len() as i32);
        for (__i, __s) in __vec.iter().enumerate() {
            let __v = v8::String::new(scope, __s).unwrap();
            __arr.set_index(scope, __i as u32, __v.into());
        }
        rv.set(__arr.into());
    }
}

/// Generate error throw from `OpError`.
fn gen_throw_error() -> TokenStream2 {
    quote! {
        let __msg = v8::String::new(scope, &__err.message).unwrap();
        let __exc = match __err.kind {
            crate::ops::OpErrorKind::TypeError => v8::Exception::type_error(scope, __msg),
            crate::ops::OpErrorKind::RangeError => v8::Exception::range_error(scope, __msg),
            _ => v8::Exception::error(scope, __msg),
        };
        scope.throw_exception(__exc);
    }
}

/// Generate the function call + return value handling.
fn gen_call_return(fn_name: &Ident, call_args: &[&Ident], output: &ReturnType) -> TokenStream2 {
    let call = quote! { #fn_name(#(#call_args),*) };

    match output {
        ReturnType::Default => quote! { #call; },
        ReturnType::Type(_, ty) => {
            let outer = type_ident(ty);
            match outer.as_deref() {
                // --- Result<T, OpError> ---
                Some("Result") => {
                    let inner = first_generic_arg(ty);
                    let inner_ident = inner.and_then(type_ident);
                    let ok_handling = match inner_ident.as_deref() {
                        Some("Option") => {
                            let inner2 = inner.and_then(first_generic_arg);
                            let some_set = inner2
                                .map(gen_option_some_set)
                                .unwrap_or_else(|| gen_option_some_set(&syn::parse_quote!(String)));
                            quote! {
                                match __ok {
                                    Some(__inner) => { #some_set }
                                    None => rv.set(v8::null(scope).into()),
                                }
                            }
                        }
                        Some("Vec") => {
                            quote! {
                                let __vec = __ok;
                                #(gen_vec_set())
                            }
                        }
                        _ => {
                            let val = quote! { __ok };
                            inner
                                .map(|t| gen_scalar_set(t, &val))
                                .unwrap_or_else(|| gen_scalar_set(&syn::parse_quote!(String), &val))
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

                // --- Vec<String> ---
                Some("Vec") => {
                    let vec_set = gen_vec_set();
                    quote! {
                        let __vec = #call;
                        #vec_set
                    }
                }

                // --- Scalars ---
                Some("bool") => quote! { rv.set(v8::Boolean::new(scope, #call).into()); },
                Some("u32") => {
                    quote! { rv.set(v8::Integer::new_from_unsigned(scope, #call).into()); }
                }
                Some("i32") => quote! { rv.set(v8::Integer::new(scope, #call).into()); },
                Some("f64") => quote! { rv.set(v8::Number::new(scope, #call).into()); },
                Some("String") => quote! {
                    let __r = #call;
                    let __v = v8::String::new(scope, &__r).unwrap();
                    rv.set(__v.into());
                },

                _ => quote! { #call; },
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Sync callback generator
// ---------------------------------------------------------------------------

fn generate_sync(needs_state: bool, input_fn: &ItemFn) -> syn::Result<TokenStream2> {
    let fn_name = &input_fn.sig.ident;
    let callback_name = format_ident!("{}_callback", fn_name);

    let params = parse_params(input_fn);
    let js_start = usize::from(needs_state);

    // State extraction
    let state_code = if needs_state {
        quote! {
            let state: crate::event_loop::SharedState = scope
                .get_slot::<crate::event_loop::SharedState>()
                .expect("EventLoopState not in isolate slot")
                .clone();
        }
    } else {
        quote! {}
    };

    // JS arg extractions (skip state param)
    let extractions: Vec<TokenStream2> = params[js_start..]
        .iter()
        .enumerate()
        .map(|(i, p)| gen_extract(i, &p.name, &p.ty))
        .collect();

    // Call args (all params, including state)
    let call_args: Vec<&Ident> = params.iter().map(|p| &p.name).collect();

    let call_return = gen_call_return(fn_name, &call_args, &input_fn.sig.output);

    Ok(quote! {
        #input_fn

        #[allow(unused_variables, unused_mut, clippy::needless_borrow)]
        pub(crate) fn #callback_name(
            scope: &mut v8::PinScope,
            args: v8::FunctionCallbackArguments,
            mut rv: v8::ReturnValue,
        ) {
            #state_code
            #(#extractions)*
            #call_return
        }
    })
}

// ---------------------------------------------------------------------------
// Async callback generator
// ---------------------------------------------------------------------------

fn generate_async(input_fn: &ItemFn) -> syn::Result<TokenStream2> {
    let fn_name = &input_fn.sig.ident;
    let callback_name = format_ident!("{}_callback", fn_name);

    let params = parse_params(input_fn);

    // All params are JS args for async (state plumbing is auto-generated)
    let extractions: Vec<TokenStream2> = params
        .iter()
        .enumerate()
        .map(|(i, p)| gen_extract(i, &p.name, &p.ty))
        .collect();

    let call_args: Vec<&Ident> = params.iter().map(|p| &p.name).collect();

    // Check return type: String or Result<String, OpError>
    let is_result = matches!(
        output_outer_ident(&input_fn.sig.output),
        Some(ref s) if s == "Result"
    );

    let send_result = if is_result {
        quote! {
            let __value = match #fn_name(#(#call_args),*).await {
                Ok(__v) => __v,
                Err(__e) => serde_json::json!({ "error": __e.message }).to_string(),
            };
        }
    } else {
        quote! {
            let __value = #fn_name(#(#call_args),*).await;
        }
    };

    Ok(quote! {
        #input_fn

        #[allow(unused_variables, unused_mut, clippy::needless_borrow)]
        pub(crate) fn #callback_name(
            scope: &mut v8::PinScope,
            args: v8::FunctionCallbackArguments,
            mut rv: v8::ReturnValue,
        ) {
            let __state: crate::event_loop::SharedState = scope
                .get_slot::<crate::event_loop::SharedState>()
                .expect("EventLoopState not in isolate slot")
                .clone();

            #(#extractions)*

            // Create promise
            let __resolver = v8::PromiseResolver::new(scope).unwrap();
            let __promise = __resolver.get_promise(scope);
            let __global_resolver = v8::Global::new(scope, __resolver);

            let (__op_id, __op_tx, __concurrent_tx, __tokio_handle) = {
                let mut __s = __state.borrow_mut();
                let __id = __s.next_op_id;
                __s.next_op_id += 1;
                __s.pending_resolvers.insert(__id, __global_resolver);
                (
                    __id,
                    __s.op_tx.clone(),
                    __s.concurrent_event_tx.clone(),
                    __s.tokio_handle.clone(),
                )
            };

            let __task = async move {
                #send_result
                match __concurrent_tx {
                    Some(ref __ctx) => {
                        let _ = __ctx.send(crate::concurrent::Event::OpCompleted {
                            op_id: __op_id,
                            value: __value,
                        });
                    }
                    None => {
                        let _ = __op_tx.send(crate::event_loop::OpResult {
                            id: __op_id,
                            value: __value,
                        });
                    }
                }
            };

            match __tokio_handle {
                Some(__handle) => {
                    __handle.spawn(__task);
                }
                None => {
                    std::thread::spawn(move || {
                        let __rt = tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                            .expect("Failed to create tokio runtime for async op");
                        __rt.block_on(__task);
                    });
                }
            }

            rv.set(__promise.into());
        }
    })
}

fn output_outer_ident(output: &ReturnType) -> Option<String> {
    match output {
        ReturnType::Default => None,
        ReturnType::Type(_, ty) => type_ident(ty),
    }
}
