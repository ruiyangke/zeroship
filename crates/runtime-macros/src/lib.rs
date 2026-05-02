//! Proc macros for the zeroship runtime.
//!
//! Two macros live here:
//!
//! - [`zeroship_op`] — wraps a plain Rust function as a V8 free-function
//!   callback. Handles argument extraction, state access, return value
//!   marshaling, error throwing, and async-Promise plumbing.
//! - [`v8_class`] — wraps an `impl` block as a V8 ObjectTemplate-backed
//!   class. Methods, getters, setters, and constructors get auto-generated
//!   callbacks; instance state lives in V8 internal fields.
//!
//! ```ignore
//! // Free-function op:
//! #[zeroship_op]
//! fn url_can_parse(input: String, base: Option<String>) -> bool { ... }
//!
//! // Class:
//! struct Headers { /* ... */ }
//!
//! #[v8_class]
//! impl Headers {
//!     #[v8_constructor]
//!     fn new() -> Result<Self, OpError> { ... }
//!
//!     #[v8_method]
//!     fn get(&self, name: String) -> Option<String> { ... }
//!
//!     #[v8_method]
//!     fn set(&mut self, name: String, value: String) -> Result<(), OpError> { ... }
//! }
//! ```
//!
//! The class macro generates `Headers::install(scope) -> v8::Local<v8::FunctionTemplate>`
//! that the runtime calls during `setup_globals` to wire the class onto
//! `globalThis`.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::{
    parse_macro_input, FnArg, GenericArgument, Ident, ItemFn, Pat, PathArguments, ReturnType,
    Type, TypePath,
};

mod v8_class;

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Wrap an `impl` block as a V8 ObjectTemplate-backed class.
///
/// See module docs for usage. Methods marked with `#[v8_method]`,
/// `#[v8_getter]`, `#[v8_setter]`, and `#[v8_constructor]` get
/// auto-generated callbacks; the macro emits `Self::install(scope) ->
/// v8::Local<v8::FunctionTemplate>` for the runtime to register on the
/// global object.
#[proc_macro_attribute]
pub fn v8_class(attr: TokenStream, item: TokenStream) -> TokenStream {
    v8_class::expand(attr, item)
}

#[proc_macro_attribute]
pub fn v8_method(_attr: TokenStream, item: TokenStream) -> TokenStream {
    // Marker attribute consumed by `#[v8_class]`. When applied to a method
    // outside a `#[v8_class]` impl block this is a no-op (the method stays
    // as written) — the macro doesn't error so editor tooling that
    // pre-expands attribute macros doesn't surface a false positive.
    item
}

#[proc_macro_attribute]
pub fn v8_getter(_attr: TokenStream, item: TokenStream) -> TokenStream {
    item
}

#[proc_macro_attribute]
pub fn v8_setter(_attr: TokenStream, item: TokenStream) -> TokenStream {
    item
}

#[proc_macro_attribute]
pub fn v8_constructor(_attr: TokenStream, item: TokenStream) -> TokenStream {
    item
}

/// Marker attribute consumed by `#[v8_class]`: rename a method on the
/// JS-visible surface. `#[v8_name = "delete"]` lets a Rust `fn delete_`
/// be installed as `Foo.prototype.delete`. Outside of a `#[v8_class]`
/// impl block this is a no-op.
#[proc_macro_attribute]
pub fn v8_name(_attr: TokenStream, item: TokenStream) -> TokenStream {
    item
}

/// Impl-block-level marker attribute consumed by `#[v8_class]`:
/// override the default `Symbol.toStringTag` value. Without this, the
/// install codegen uses the Rust struct name (e.g.
/// `"HeadersIterator"`); with `#[v8_to_string_tag = "Headers Iterator"]`
/// it installs that literal instead. Used for WebIDL default iterator
/// objects whose spec tag is "<InterfaceName> Iterator".
#[proc_macro_attribute]
pub fn v8_to_string_tag(_attr: TokenStream, item: TokenStream) -> TokenStream {
    item
}

/// Impl-block-level marker attribute: chain the class's prototype to a
/// V8 built-in intrinsic. Currently the only recognised value is
/// `"IteratorPrototype"`, which sets the prototype's `[[Prototype]]`
/// to `%Iterator.prototype%` per WebIDL §3.7.10.2.
#[proc_macro_attribute]
pub fn v8_inherit_intrinsic(_attr: TokenStream, item: TokenStream) -> TokenStream {
    item
}

/// Argument-level marker attribute consumed by `#[v8_class]`: when
/// applied to a `Vec<u8>` parameter, the macro emits a SAB-rejection
/// guard *before* extracting the bytes. A SharedArrayBuffer-backed
/// view (or a bare `SharedArrayBuffer`) throws `TypeError` and the
/// method body never runs.
///
/// Per WebIDL §3.2.21: BufferSource arguments default to rejecting
/// shared backing stores; only the `[AllowShared]` extended attribute
/// opts in. We invert that for ergonomics — `Vec<u8>` extraction is
/// permissive by default (matches how legacy ops use it for blobs and
/// uploads), and the `#[reject_shared]` attribute pins down the spec-
/// strict cases (CompressionStream chunks, etc.).
///
/// Outside a `#[v8_class]` method param this attribute is a no-op.
#[proc_macro_attribute]
pub fn reject_shared(_attr: TokenStream, item: TokenStream) -> TokenStream {
    item
}

#[proc_macro_attribute]
pub fn zeroship_op(attr: TokenStream, item: TokenStream) -> TokenStream {
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

/// True for the unit type `()`. Returned by mutator-style methods
/// like `Result<(), OpError>` that have nothing to set on `rv` —
/// they want the JS-visible call to evaluate to `undefined`.
pub(crate) fn is_unit_type(ty: &Type) -> bool {
    matches!(ty, Type::Tuple(t) if t.elems.is_empty())
}

/// Extract the last segment identifier from a type path (e.g. `String`, `Option`, `Result`).
pub(crate) fn type_ident(ty: &Type) -> Option<String> {
    if let Type::Path(TypePath { path, .. }) = ty {
        path.segments.last().map(|s| s.ident.to_string())
    } else {
        None
    }
}

/// Check if type is `Vec<u8>` — used for binary data args (reads from ArrayBufferView).
pub(crate) fn is_vec_u8(ty: &Type) -> bool {
    type_ident(ty).as_deref() == Some("Vec")
        && first_generic_arg(ty)
            .and_then(type_ident)
            .as_deref()
            == Some("u8")
}

/// Check if type is `Vec<Vec<u8>>` — used by IDL methods like
/// `getSetCookie() -> sequence<ByteString>`. Marshalled as a JS Array
/// of ByteString (each element is a Latin-1 one-byte string).
pub(crate) fn is_vec_vec_u8(ty: &Type) -> bool {
    type_ident(ty).as_deref() == Some("Vec")
        && first_generic_arg(ty)
            .map(is_vec_u8)
            .unwrap_or(false)
}

/// Check if type is the `ByteString` newtype from
/// `zeroship_runtime::byte_string`. Used for WebIDL ByteString args
/// (Headers names/values etc.). Detection is by last segment ident; we
/// don't enforce the full path since users typically `use
/// ::zeroship_runtime::byte_string::ByteString` or alias the type.
pub(crate) fn is_byte_string(ty: &Type) -> bool {
    type_ident(ty).as_deref() == Some("ByteString")
}

/// Check if type is the `EnforceRangeU64` newtype from
/// `zeroship_runtime::enforce_range`. Used for WebIDL `[EnforceRange]
/// unsigned long long` args (BYOBReader.read min, BYOBRequest.respond
/// bytesWritten). Detection by last segment ident, like ByteString.
pub(crate) fn is_enforce_range_u64(ty: &Type) -> bool {
    type_ident(ty).as_deref() == Some("EnforceRangeU64")
}

/// Check if type is the `USVString` newtype from
/// `zeroship_runtime::url_native::helpers`. Used for WebIDL USVString
/// args (URL.* setters, URLSearchParams names/values). Conversion
/// replaces unmatched surrogate code units with U+FFFD.
pub(crate) fn is_usv_string(ty: &Type) -> bool {
    type_ident(ty).as_deref() == Some("USVString")
}

/// Check if type is `Option<USVString>`.
pub(crate) fn is_option_usv_string(ty: &Type) -> bool {
    type_ident(ty).as_deref() == Some("Option")
        && first_generic_arg(ty).map(is_usv_string).unwrap_or(false)
}

/// Extract the first generic type argument (e.g. `String` from `Option<String>`).
pub(crate) fn first_generic_arg(ty: &Type) -> Option<&Type> {
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

pub(crate) struct Param {
    pub(crate) name: Ident,
    pub(crate) ty: Type,
}

pub(crate) fn parse_params(f: &ItemFn) -> Vec<Param> {
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

pub(crate) fn gen_extract(index: usize, name: &Ident, ty: &Type) -> TokenStream2 {
    let idx = index as i32;
    let ident = type_ident(ty);

    // v8::Local<v8::Value> (or any v8::Local<v8::T>) — pass the raw
    // arg through unchanged. Lets handlers accept union types
    // (Request body, Headers init, etc.) and dispatch on the V8
    // value's actual shape themselves.
    if ident.as_deref() == Some("Local") {
        return quote! {
            let #name = args.get(#idx);
        };
    }

    // ByteString → WebIDL ByteString conversion. On any code unit
    // > 0xFF, sets a pending TypeError and returns from the callback
    // (so the JS caller observes the throw). The match-and-return
    // shape works in callbacks that return `()` (the V8 ABI shape) —
    // we don't need the user method to return Result. After the throw
    // is set, JS execution unwinds normally.
    if is_byte_string(ty) {
        return quote! {
            let #name = match ::zeroship_runtime::byte_string::read_byte_string(
                scope,
                args.get(#idx),
            ) {
                Ok(__bytes) => ::zeroship_runtime::byte_string::ByteString::from_bytes(__bytes),
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
        };
    }

    // USVString → WebIDL USVString conversion. Replaces lone surrogate
    // code units with U+FFFD per https://webidl.spec.whatwg.org/#es-USVString.
    // The result is owned `String` so callers don't keep a `Local<Value>`
    // borrow alive across subsequent V8 ops.
    if is_usv_string(ty) {
        return quote! {
            let #name = match ::zeroship_runtime::url_native::helpers::read_usv_string_or_throw(
                scope,
                args.get(#idx),
            ) {
                Ok(__s) => ::zeroship_runtime::url_native::helpers::USVString::from_string(__s),
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
        };
    }

    // Option<USVString> — undefined / null produces None; otherwise
    // run USVString conversion and wrap in Some.
    if is_option_usv_string(ty) {
        return quote! {
            let #name: Option<::zeroship_runtime::url_native::helpers::USVString> =
                if args.length() > #idx && !args.get(#idx).is_undefined() {
                    match ::zeroship_runtime::url_native::helpers::read_usv_string_or_throw(
                        scope,
                        args.get(#idx),
                    ) {
                        Ok(__s) => Some(::zeroship_runtime::url_native::helpers::USVString::from_string(__s)),
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
                    }
                } else {
                    None
                };
        };
    }

    // EnforceRangeU64 → WebIDL [EnforceRange] unsigned long long. Throws
    // TypeError for NaN, ±∞, negative, and values > 2^53-1 (Number
    // precision limit) — see streams design §XIV.8.
    if is_enforce_range_u64(ty) {
        return quote! {
            let #name = match ::zeroship_runtime::enforce_range::read_enforce_range_u64(
                scope,
                args.get(#idx),
            ) {
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
        };
    }

    // Vec<u8> → read from ArrayBufferView backing store (zero-serialization binary transfer)
    if is_vec_u8(ty) {
        return quote! {
            let #name: Vec<u8> = {
                let __arg = args.get(#idx);
                if let Ok(__view) = v8::Local::<v8::ArrayBufferView>::try_from(__arg) {
                    let mut __buf = vec![0u8; __view.byte_length()];
                    __view.copy_contents(&mut __buf);
                    __buf
                } else if let Ok(__ab) = v8::Local::<v8::ArrayBuffer>::try_from(__arg) {
                    let __store = __ab.get_backing_store();
                    let mut __buf = vec![0u8; __ab.byte_length()];
                    for __i in 0..__buf.len() {
                        __buf[__i] = __store[__i].get();
                    }
                    __buf
                } else {
                    Vec::new()
                }
            };
        };
    }

    match ident.as_deref() {
        Some("Option") => {
            let inner_ty = first_generic_arg(ty);
            let inner = inner_ty.and_then(type_ident);
            // Option<Vec<u8>> needs the same ArrayBuffer/View
            // extraction the bare Vec<u8> path uses, just lifted
            // through Option to handle missing/null/undefined args.
            if inner_ty.map(is_vec_u8).unwrap_or(false) {
                return quote! {
                    let #name: Option<Vec<u8>> = if args.length() > #idx
                        && !args.get(#idx).is_null_or_undefined()
                    {
                        let __arg = args.get(#idx);
                        if let Ok(__view) = v8::Local::<v8::ArrayBufferView>::try_from(__arg) {
                            let mut __buf = vec![0u8; __view.byte_length()];
                            __view.copy_contents(&mut __buf);
                            Some(__buf)
                        } else if let Ok(__ab) = v8::Local::<v8::ArrayBuffer>::try_from(__arg) {
                            let __store = __ab.get_backing_store();
                            let mut __buf = vec![0u8; __ab.byte_length()];
                            for __i in 0..__buf.len() {
                                __buf[__i] = __store[__i].get();
                            }
                            Some(__buf)
                        } else {
                            None
                        }
                    } else {
                        None
                    };
                };
            }
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

/// Generate code to write Vec<u8> as a V8 Uint8Array.
///
/// Returning a plain ArrayBuffer was easier but spec-wrong for every
/// real consumer: WHATWG TextEncoder.encode and WebCrypto digest both
/// return Uint8Array, and downstream JS code (streams, fetch body
/// coercion) typically branches on `instanceof Uint8Array` to decide
/// whether to wrap. Returning Uint8Array matches the spec contract
/// without forcing every caller to do `new Uint8Array(arrayBuffer)`.
fn gen_vec_u8_set(val: &TokenStream2) -> TokenStream2 {
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

/// Generate code to convert a scalar value (referenced by `val` tokens) to a V8 return value.
fn gen_scalar_set(ty: &Type, val: &TokenStream2) -> TokenStream2 {
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
        _ => quote! {
            let __v = v8::String::new(scope, &#val).unwrap();
            rv.set(__v.into());
        },
    }
}

/// Generate code to convert an `Option<T>` inner value to V8. The
/// emitted code expects `__inner` to be the unwrapped Some value.
fn gen_option_some_set(ty: &Type) -> TokenStream2 {
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

/// Generate code to build a `v8::Array` from a `Vec<Vec<u8>>`. Each
/// element is materialised as a Latin-1 one-byte string (a WebIDL
/// ByteString round-trips faithfully — bytes 0x80–0xFF survive). Used
/// by methods like `Headers.getSetCookie() -> sequence<ByteString>`.
fn gen_vec_vec_u8_set() -> TokenStream2 {
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

/// Generate error throw from `OpError`.
fn gen_throw_error() -> TokenStream2 {
    quote! {
        let __msg = v8::String::new(scope, &__err.message).unwrap();
        let __exc = match __err.kind {
            ::zeroship_runtime::state::OpErrorKind::TypeError => v8::Exception::type_error(scope, __msg),
            ::zeroship_runtime::state::OpErrorKind::RangeError => v8::Exception::range_error(scope, __msg),
            _ => v8::Exception::error(scope, __msg),
        };
        scope.throw_exception(__exc);
    }
}

/// Generate the function call + return value handling.
///
/// `call` is the pre-built call expression (e.g. `my_fn(a, b)` or
/// `__instance.method(a, b)`). Splitting this out lets both the
/// `#[zeroship_op]` and `#[v8_class]` macros reuse the return-value
/// marshaling logic with their respective call shapes.
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
                                inner
                                    .map(|t| gen_scalar_set(t, &val))
                                    .unwrap_or_else(|| gen_scalar_set(&syn::parse_quote!(String), &val))
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
                Some("String") => quote! {
                    let __r = #call;
                    let __v = v8::String::new(scope, &__r).unwrap();
                    rv.set(__v.into());
                },

                // --- Direct V8 value (Local<Value>, Local<Object>, etc.) ---
                // Used by methods that build a custom JS shape (e.g.
                // TextEncoder.encodeInto returning `{ read, written }`).
                Some("Local") => quote! { rv.set(#call.into()); },

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
            let state: crate::state::SharedState = scope
                .get_slot::<crate::state::SharedState>()
                .expect("RuntimeState not in isolate slot")
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
    let call = quote! { #fn_name(#(#call_args),*) };

    let call_return = gen_call_return(&call, &input_fn.sig.output);

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
            let __state: crate::state::SharedState = scope
                .get_slot::<crate::state::SharedState>()
                .expect("RuntimeState not in isolate slot")
                .clone();

            #(#extractions)*

            // Create promise
            let __resolver = v8::PromiseResolver::new(scope).unwrap();
            let __promise = __resolver.get_promise(scope);
            let __global_resolver = v8::Global::new(scope, __resolver);

            let (__op_id, __request_id) = {
                let mut __s = __state.borrow_mut();
                let __id = __s.next_op_id;
                __s.next_op_id += 1;
                __s.pending_resolvers.insert(__id, __global_resolver);
                (__id, __s.executing_request_id)
            };

            let __fut = Box::pin(async move {
                #send_result
                crate::state::OpResult::Completed {
                    op_id: __op_id,
                    value: __value,
                    request_id: __request_id,
                }
            });

            __state.borrow_mut().spawned_ops.push(__fut);

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
