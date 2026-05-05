//! Table-driven argument-type classifier for `gen_extract` (Wave 4b —
//! design `docs/proposals/runtime-macros-refactor.md` §3.7).
//!
//! Pre-Wave-4b `gen_extract` was a 277-LOC dispatcher with a stack of
//! `if is_X(ty)` predicates plus a 13-arm string-keyed `match` on the
//! type's last-segment ident. The shape was hostile to extension — every
//! new newtype added one more `if` block and one more silent fallback to
//! the catch-all String arm.
//!
//! This module folds all of that into a single classification step:
//!
//! 1. [`KnownType::from_ty`] inspects the `syn::Type` once and returns
//!    the matching enum variant. The `User(syn::Path)` variant is the
//!    escape hatch for any shape we don't recognise — the caller can
//!    choose to fall through to the legacy `String`/`from_v8`-style path.
//! 2. [`KnownType::extract_tokens`] emits the per-variant extraction
//!    quote once per variant. The two callers (slow-path
//!    `gen_extract` in `lib.rs`) both go through the same table.
//!
//! Closes F10 / H8 of `runtime-macros-architecture-critique-2026-05-05`
//! and the §3 stringly-typed-dispatch anti-pattern: the `clamp_kind` /
//! `wrap_kind` standalone helpers (which returned `&'static str` and
//! used `unreachable!` arms) collapse into the variant's own data.
//!
//! ### Layout
//!
//! - **Primitives** (Bool, U32, I32, F64) — bare V8 number / boolean
//!   coercion, never throws.
//! - **Sequences** (VecU8) — ArrayBuffer / ArrayBufferView bridge.
//! - **Newtypes** (ByteString, USVString, Clamp*, Wrap*, EnforceRange*)
//!   — WebIDL conversions. Throwing variants route through the runtime
//!   crate's reader fns; no-throw variants (Wrap, Clamp) call directly.
//! - **V8 passthrough** (LocalValue) — `v8::Local<v8::Value>`, used by
//!   union-typed args that dispatch on the value's V8 shape themselves.
//! - **Wrappers** (Option<T>) — lifts the inner extraction with
//!   undefined / null fall-through to None.
//! - **Catchall** (StringDefault, User) — the fallback path. Today
//!   gen_extract treats unrecognised types as String (lossy
//!   to_rust_string_lossy); the User variant carries the path so a
//!   future wave can route through `WebIdlConvertible::from_v8` for
//!   user-defined dictionaries / enums.

use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{Ident, Type};

use crate::{
    first_generic_arg, gen_extract_throw, is_byte_string, is_enforce_range_u32,
    is_enforce_range_u64, is_option_usv_string, is_usv_string, is_vec_u8, type_ident,
};

/// A `syn::Type` classified into one of the macro's known argument
/// shapes (or `User`/`StringDefault` for the catchall path).
///
/// The variants intentionally mirror the conditional cascade pre-Wave-4b
/// `gen_extract` performed; the value of the refactor is centralising
/// that cascade in ONE place ([`KnownType::from_ty`]) instead of
/// duplicating it across every emit caller.
pub(crate) enum KnownType {
    // -- Primitives -------------------------------------------------
    Bool,
    U32,
    I32,
    F64,

    // -- Sequences --------------------------------------------------
    /// `Vec<u8>` — ArrayBuffer / ArrayBufferView bridge.
    VecU8,

    // -- Newtypes (WebIDL conversions) ------------------------------
    ByteString,
    USVString,
    EnforceRangeU32,
    EnforceRangeU64,
    /// `Clamp{U16,U32,I32,U64,I64}` — WebIDL `[Clamp]` integer coercion.
    Clamp(ClampInt),
    /// `Wrap{U8,U16,U32,I8,I16,I32}` — WebIDL default integer coercion.
    Wrap(WrapInt),

    // -- V8 passthrough ---------------------------------------------
    /// `v8::Local<v8::Value>` (or any `v8::Local<v8::T>`) — passed to
    /// the user method unchanged so it can dispatch on the shape.
    LocalValue,

    // -- Wrappers ---------------------------------------------------
    /// `Option<T>` — undefined / null collapses to None; otherwise
    /// extract `T`. The macro recognises a few specific `Option<T>`
    /// shapes (Vec<u8>, USVString, u32, i32) for spec-correct
    /// extraction; everything else under `Option<_>` falls back to
    /// `Option<String>` via the catchall path.
    OptionVecU8,
    OptionUSVString,
    OptionU32,
    OptionI32,
    /// `Option<T>` for any T not specifically recognised above. We
    /// preserve today's behaviour: lossy `to_rust_string_lossy` into
    /// `Option<String>` (the catchall).
    OptionStringFallback,

    // -- Catchall ---------------------------------------------------
    /// Unrecognised type — falls back to `String` via
    /// `to_rust_string_lossy`. This is the legacy "default arm" of the
    /// pre-Wave-4b dispatcher.
    StringDefault,
}

/// Width / signedness suffix for a `Clamp*` newtype. Drives both the
/// reader-fn path and the constructor-fn path in
/// [`KnownType::extract_tokens`].
#[derive(Clone, Copy)]
pub(crate) enum ClampInt {
    U16,
    U32,
    I32,
    U64,
    I64,
}

/// Width / signedness suffix for a `Wrap*` newtype. Mirror of
/// [`ClampInt`] for the no-throw integer-coercion path.
#[derive(Clone, Copy)]
pub(crate) enum WrapInt {
    U8,
    U16,
    U32,
    I8,
    I16,
    I32,
}

impl KnownType {
    /// Classify a `syn::Type` into one of the variants above. Walks the
    /// type's last-segment ident plus generic args once; never returns
    /// None (the catchall `StringDefault` covers everything we don't
    /// recognise).
    pub(crate) fn from_ty(ty: &Type) -> Self {
        // V8 passthrough — first because it's a structural dispatch
        // (preserves the original Local for the user method) rather
        // than a value extraction.
        if type_ident(ty).as_deref() == Some("Local") {
            return KnownType::LocalValue;
        }

        // Newtypes (most-specific first).
        if is_byte_string(ty) {
            return KnownType::ByteString;
        }
        if is_usv_string(ty) {
            return KnownType::USVString;
        }
        if is_option_usv_string(ty) {
            return KnownType::OptionUSVString;
        }
        if let Some(c) = clamp_int_from_ty(ty) {
            return KnownType::Clamp(c);
        }
        if let Some(w) = wrap_int_from_ty(ty) {
            return KnownType::Wrap(w);
        }
        if is_enforce_range_u64(ty) {
            return KnownType::EnforceRangeU64;
        }
        if is_enforce_range_u32(ty) {
            return KnownType::EnforceRangeU32;
        }

        // Sequences.
        if is_vec_u8(ty) {
            return KnownType::VecU8;
        }

        // Wrappers / primitives — the last-segment ident dispatch.
        match type_ident(ty).as_deref() {
            Some("Option") => {
                let inner_ty = first_generic_arg(ty);
                if inner_ty.map(is_vec_u8).unwrap_or(false) {
                    return KnownType::OptionVecU8;
                }
                let inner = inner_ty.and_then(type_ident);
                match inner.as_deref() {
                    Some("u32") => KnownType::OptionU32,
                    Some("i32") => KnownType::OptionI32,
                    _ => KnownType::OptionStringFallback,
                }
            }
            Some("bool") => KnownType::Bool,
            Some("u32") => KnownType::U32,
            Some("i32") => KnownType::I32,
            Some("f64") => KnownType::F64,
            _ => KnownType::StringDefault,
        }
    }

    /// Emit the per-variant extraction tokens for the slow-path
    /// FunctionCallback. The bound name is `name` (the user's parameter
    /// ident); the JS arg index is `idx`. Throwing variants emit a
    /// shared throw prologue via [`gen_extract_throw`].
    pub(crate) fn extract_tokens(&self, name: &Ident, idx: i32) -> TokenStream2 {
        match self {
            // ----- V8 passthrough -----
            KnownType::LocalValue => quote! {
                let #name = args.get(#idx);
            },

            // ----- ByteString -----
            KnownType::ByteString => {
                let throw = gen_extract_throw();
                quote! {
                    let #name = match ::zeroship_runtime::byte_string::read_byte_string(
                        scope,
                        args.get(#idx),
                    ) {
                        Ok(__bytes) => ::zeroship_runtime::byte_string::ByteString::from_bytes(__bytes),
                        Err(__err) => {
                            #throw
                            return;
                        }
                    };
                }
            }

            // ----- USVString -----
            KnownType::USVString => {
                let throw = gen_extract_throw();
                quote! {
                    let #name = match ::zeroship_runtime::url_native::helpers::read_usv_string_or_throw(
                        scope,
                        args.get(#idx),
                    ) {
                        Ok(__s) => ::zeroship_runtime::url_native::helpers::USVString::from_string(__s),
                        Err(__err) => {
                            #throw
                            return;
                        }
                    };
                }
            }

            // ----- Option<USVString> -----
            KnownType::OptionUSVString => {
                let throw = gen_extract_throw();
                quote! {
                    let #name: Option<::zeroship_runtime::url_native::helpers::USVString> =
                        if args.length() > #idx && !args.get(#idx).is_undefined() {
                            match ::zeroship_runtime::url_native::helpers::read_usv_string_or_throw(
                                scope,
                                args.get(#idx),
                            ) {
                                Ok(__s) => Some(::zeroship_runtime::url_native::helpers::USVString::from_string(__s)),
                                Err(__err) => {
                                    #throw
                                    return;
                                }
                            }
                        } else {
                            None
                        };
                }
            }

            // ----- Clamp{U16,U32,I32,U64,I64} -----
            KnownType::Clamp(c) => {
                let (reader, ctor) = clamp_reader_ctor(*c);
                quote! {
                    let #name = #ctor(#reader(scope, args.get(#idx)));
                }
            }

            // ----- Wrap{U8,U16,U32,I8,I16,I32} -----
            KnownType::Wrap(w) => {
                let reader = wrap_reader(*w);
                quote! {
                    let #name = #reader(scope, args.get(#idx));
                }
            }

            // ----- EnforceRangeU64 -----
            KnownType::EnforceRangeU64 => {
                let throw = gen_extract_throw();
                quote! {
                    let #name = match ::zeroship_runtime::enforce_range::read_enforce_range_u64(
                        scope,
                        args.get(#idx),
                    ) {
                        Ok(__v) => __v,
                        Err(__err) => {
                            #throw
                            return;
                        }
                    };
                }
            }

            // ----- EnforceRangeU32 -----
            KnownType::EnforceRangeU32 => {
                let throw = gen_extract_throw();
                quote! {
                    let #name = match ::zeroship_runtime::enforce_range::read_enforce_range_u32(
                        scope,
                        args.get(#idx),
                    ) {
                        Ok(__v) => __v,
                        Err(__err) => {
                            #throw
                            return;
                        }
                    };
                }
            }

            // ----- Vec<u8> -----
            KnownType::VecU8 => quote! {
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
            },

            // ----- Option<Vec<u8>> -----
            KnownType::OptionVecU8 => quote! {
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
            },

            // ----- Option<u32> -----
            KnownType::OptionU32 => quote! {
                let #name: Option<u32> = if args.length() > #idx
                    && !args.get(#idx).is_null_or_undefined()
                {
                    args.get(#idx).uint32_value(scope)
                } else {
                    None
                };
            },

            // ----- Option<i32> -----
            KnownType::OptionI32 => quote! {
                let #name: Option<i32> = if args.length() > #idx
                    && !args.get(#idx).is_null_or_undefined()
                {
                    args.get(#idx).int32_value(scope)
                } else {
                    None
                };
            },

            // ----- Option<*> catchall -----
            KnownType::OptionStringFallback => quote! {
                let #name: Option<String> = if args.length() > #idx
                    && !args.get(#idx).is_null_or_undefined()
                {
                    Some(args.get(#idx).to_rust_string_lossy(scope))
                } else {
                    None
                };
            },

            // ----- Primitives -----
            KnownType::Bool => quote! {
                let #name: bool = args.get(#idx).boolean_value(scope);
            },
            KnownType::U32 => quote! {
                let #name: u32 = args.get(#idx).uint32_value(scope).unwrap_or(0);
            },
            KnownType::I32 => quote! {
                let #name: i32 = args.get(#idx).int32_value(scope).unwrap_or(0);
            },
            KnownType::F64 => quote! {
                let #name: f64 = args.get(#idx).number_value(scope).unwrap_or(0.0);
            },

            // ----- StringDefault catchall -----
            KnownType::StringDefault => quote! {
                let #name: String = args.get(#idx).to_rust_string_lossy(scope);
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Newtype classification helpers (used by `from_ty`)
// ---------------------------------------------------------------------------

/// Map a `Clamp{...}` newtype to its [`ClampInt`] variant. None if `ty`
/// isn't a recognised clamp newtype.
fn clamp_int_from_ty(ty: &Type) -> Option<ClampInt> {
    match type_ident(ty).as_deref()? {
        "ClampU16" => Some(ClampInt::U16),
        "ClampU32" => Some(ClampInt::U32),
        "ClampI32" => Some(ClampInt::I32),
        "ClampU64" => Some(ClampInt::U64),
        "ClampI64" => Some(ClampInt::I64),
        _ => None,
    }
}

/// Map a `Wrap{...}` newtype to its [`WrapInt`] variant. None if `ty`
/// isn't a recognised wrap newtype.
fn wrap_int_from_ty(ty: &Type) -> Option<WrapInt> {
    match type_ident(ty).as_deref()? {
        "WrapU8" => Some(WrapInt::U8),
        "WrapU16" => Some(WrapInt::U16),
        "WrapU32" => Some(WrapInt::U32),
        "WrapI8" => Some(WrapInt::I8),
        "WrapI16" => Some(WrapInt::I16),
        "WrapI32" => Some(WrapInt::I32),
        _ => None,
    }
}

/// `(reader_path, ctor_path)` for a clamp variant. The reader is the
/// runtime-crate fn that performs the WebIDL `[Clamp]` algorithm; the
/// constructor wraps the resulting integer in the newtype.
fn clamp_reader_ctor(c: ClampInt) -> (TokenStream2, TokenStream2) {
    match c {
        ClampInt::U16 => (
            quote! { ::zeroship_runtime::clamp::read_clamp_u16 },
            quote! { ::zeroship_runtime::clamp::ClampU16 },
        ),
        ClampInt::U32 => (
            quote! { ::zeroship_runtime::clamp::read_clamp_u32 },
            quote! { ::zeroship_runtime::clamp::ClampU32 },
        ),
        ClampInt::I32 => (
            quote! { ::zeroship_runtime::clamp::read_clamp_i32 },
            quote! { ::zeroship_runtime::clamp::ClampI32 },
        ),
        ClampInt::U64 => (
            quote! { ::zeroship_runtime::clamp::read_clamp_u64 },
            quote! { ::zeroship_runtime::clamp::ClampU64 },
        ),
        ClampInt::I64 => (
            quote! { ::zeroship_runtime::clamp::read_clamp_i64 },
            quote! { ::zeroship_runtime::clamp::ClampI64 },
        ),
    }
}

/// Reader-fn path for a wrap variant. There's no constructor — the
/// reader fn returns the wrap newtype directly.
fn wrap_reader(w: WrapInt) -> TokenStream2 {
    match w {
        WrapInt::U8 => quote! { ::zeroship_runtime::wrap::read_wrap_u8 },
        WrapInt::U16 => quote! { ::zeroship_runtime::wrap::read_wrap_u16 },
        WrapInt::U32 => quote! { ::zeroship_runtime::wrap::read_wrap_u32 },
        WrapInt::I8 => quote! { ::zeroship_runtime::wrap::read_wrap_i8 },
        WrapInt::I16 => quote! { ::zeroship_runtime::wrap::read_wrap_i16 },
        WrapInt::I32 => quote! { ::zeroship_runtime::wrap::read_wrap_i32 },
    }
}
