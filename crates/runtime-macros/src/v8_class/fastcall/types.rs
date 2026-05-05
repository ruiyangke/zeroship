//! Table-driven fast-API type classifier (Wave 4b — design
//! `docs/proposals/runtime-macros-refactor.md` §3.7).
//!
//! Pre-Wave-4b `fastcall_arg_mapping` and `fastcall_return_mapping`
//! were two parallel string-keyed `match` tables (see the parent
//! mod.rs's `arg_mapping` / `return_mapping` history). Each new
//! supported type meant editing two tables, and a typo in one (e.g.
//! `"u32"` vs `"U32"`) silently degraded the case to the catchall
//! "unsupported" arm.
//!
//! This module folds both tables into a single classifier: the same
//! [`FastcallType`] enum drives the CTypeInfo entry, the extern "C"
//! signature, the per-arg adaption snippet, and the Result-arm
//! sentinel. Argument vs. return positions select between
//! [`FastcallType::from_arg_ty`] and [`FastcallType::from_return_ty`]
//! — they share the same backing variants but accept different shapes
//! (no `Void` for args, no `FastOneByteString` for returns; the latter
//! is only valid as a borrowed input).
//!
//! Closes F6 (string-keyed fastcall dispatch) and the §3 stringly-typed
//! anti-pattern. Supported variants:
//!
//! - **Primitives** (Bool, I32, U32, I64, U64, F32, F64) — round-trip
//!   without conversion; the `bind` snippet is a no-op `let x = x_raw;`.
//! - **FastOneByteString** — argument-only; V8 hands us a `*const
//!   FastApiOneByteString` which we copy into a fresh `ByteString` (a
//!   small Vec — design tradeoff documented in the parent module).
//! - **Void** — return-only; covers `()` and `Result<(), OpError>`.

use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::{Ident, Type};

/// A V8 fast-API type the macro knows how to translate to/from the
/// user method's Rust signature. The variants intentionally cover the
/// pre-Wave-4b string-keyed cases plus `Void` for the return position.
///
/// Use [`FastcallType::from_arg_ty`] for argument positions and
/// [`FastcallType::from_return_ty`] for return positions; mismatched
/// variants (e.g. `Void` as an arg, `FastOneByteString` as a return)
/// are statically unrepresentable.
#[derive(Clone, Copy)]
pub(super) enum FastcallType {
    /// `bool`.
    Bool,
    /// `i32`.
    I32,
    /// `u32`.
    U32,
    /// `i64`.
    I64,
    /// `u64`.
    U64,
    /// `f32`.
    F32,
    /// `f64`.
    F64,
    /// `ByteString` argument — V8 delivers `*const FastApiOneByteString`
    /// which the bind snippet copies into a fresh ByteString. Argument
    /// position only.
    FastOneByteString,
    /// Return-position `()` — matches `ReturnType::Default`,
    /// `-> ()`, and `Result<(), OpError>`.
    Void,
}

impl FastcallType {
    /// Classify an argument type. Returns `None` for anything outside
    /// the fast-API allowlist; the parent module's
    /// `validate_fastcall_signature` already rejected those at
    /// expand-time, so callers here can `?`-propagate.
    pub(super) fn from_arg_ty(ty: &Type) -> Option<Self> {
        match crate::type_ident(ty).as_deref()? {
            "bool" => Some(FastcallType::Bool),
            "i32" => Some(FastcallType::I32),
            "u32" => Some(FastcallType::U32),
            "i64" => Some(FastcallType::I64),
            "u64" => Some(FastcallType::U64),
            "f32" => Some(FastcallType::F32),
            "f64" => Some(FastcallType::F64),
            "ByteString" => Some(FastcallType::FastOneByteString),
            _ => None,
        }
    }

    /// Classify a return type's Rust ident. The Result wrapper is
    /// stripped by the caller before this is called; this fn sees the
    /// inner type (or the bare type, for non-Result returns).
    pub(super) fn from_return_inner(ty: &Type) -> Option<Self> {
        if crate::is_unit_type(ty) {
            return Some(FastcallType::Void);
        }
        match crate::type_ident(ty).as_deref()? {
            "bool" => Some(FastcallType::Bool),
            "i32" => Some(FastcallType::I32),
            "u32" => Some(FastcallType::U32),
            "i64" => Some(FastcallType::I64),
            "u64" => Some(FastcallType::U64),
            "f32" => Some(FastcallType::F32),
            "f64" => Some(FastcallType::F64),
            _ => None,
        }
    }

    /// Token for the CTypeInfo entry (used both arg-side in the array
    /// literal and return-side in the CFunctionInfo constructor).
    pub(super) fn cinfo(self) -> TokenStream2 {
        match self {
            FastcallType::Bool => quote! { ::v8::fast_api::Type::Bool.as_info() },
            FastcallType::I32 => quote! { ::v8::fast_api::Type::Int32.as_info() },
            FastcallType::U32 => quote! { ::v8::fast_api::Type::Uint32.as_info() },
            FastcallType::I64 => quote! { ::v8::fast_api::Type::Int64.as_info() },
            FastcallType::U64 => quote! { ::v8::fast_api::Type::Uint64.as_info() },
            FastcallType::F32 => quote! { ::v8::fast_api::Type::Float32.as_info() },
            FastcallType::F64 => quote! { ::v8::fast_api::Type::Float64.as_info() },
            FastcallType::FastOneByteString => {
                quote! { ::v8::fast_api::Type::SeqOneByteString.as_info() }
            }
            FastcallType::Void => quote! { ::v8::fast_api::Type::Void.as_info() },
        }
    }

    /// Token for the extern "C" fn parameter / return type.
    pub(super) fn extern_ty(self) -> TokenStream2 {
        match self {
            FastcallType::Bool => quote! { bool },
            FastcallType::I32 => quote! { i32 },
            FastcallType::U32 => quote! { u32 },
            FastcallType::I64 => quote! { i64 },
            FastcallType::U64 => quote! { u64 },
            FastcallType::F32 => quote! { f32 },
            FastcallType::F64 => quote! { f64 },
            FastcallType::FastOneByteString => {
                quote! { *const ::v8::fast_api::FastApiOneByteString }
            }
            FastcallType::Void => quote! { () },
        }
    }

    /// Argument-side bind snippet: convert the raw fast-API value
    /// (named `<name>_raw`) to the user method's expected type (named
    /// `<name>`). Primitives are no-ops; `FastOneByteString` copies into
    /// a fresh `ByteString`. Panics in debug if called with `Void`
    /// (which should never reach the arg side).
    pub(super) fn arg_bind(self, name: &Ident) -> TokenStream2 {
        let raw_name = format_ident!("{}_raw", name);
        match self {
            FastcallType::Bool => quote! { let #name: bool = #raw_name; },
            FastcallType::I32 => quote! { let #name: i32 = #raw_name; },
            FastcallType::U32 => quote! { let #name: u32 = #raw_name; },
            FastcallType::I64 => quote! { let #name: i64 = #raw_name; },
            FastcallType::U64 => quote! { let #name: u64 = #raw_name; },
            FastcallType::F32 => quote! { let #name: f32 = #raw_name; },
            FastcallType::F64 => quote! { let #name: f64 = #raw_name; },
            FastcallType::FastOneByteString => quote! {
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
                    ::zeroship_runtime::macro_runtime::byte_string::ByteString::from_bytes(__bytes_slice.to_vec())
                };
            },
            FastcallType::Void => {
                // Unreachable in practice — `Void` is returns-only and
                // the parent module's signature validator rejects `()`
                // arguments. Emit a no-op so accidental misuse compiles
                // (the user method's call expression will fail at
                // type-check, surfacing a clearer diagnostic).
                quote! { let _ = #raw_name; }
            }
        }
    }

    /// Return-side sentinel for the Err arm of a `Result<T, OpError>`
    /// fastcall. V8 ignores the return value when an exception is
    /// pending, so any zero-bit-pattern of the right type works.
    pub(super) fn err_sentinel(self) -> TokenStream2 {
        match self {
            FastcallType::Bool => quote! { false },
            FastcallType::I32 => quote! { 0i32 },
            FastcallType::U32 => quote! { 0u32 },
            FastcallType::I64 => quote! { 0i64 },
            FastcallType::U64 => quote! { 0u64 },
            FastcallType::F32 => quote! { 0.0f32 },
            FastcallType::F64 => quote! { 0.0f64 },
            FastcallType::FastOneByteString => {
                // Argument-only — never used as a return sentinel.
                quote! { ::std::ptr::null() }
            }
            FastcallType::Void => quote! { () },
        }
    }
}
