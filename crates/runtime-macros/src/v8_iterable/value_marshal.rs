//! Per-yield value marshalling for the iterable codegen.
//!
//! Split out from the old monolithic `v8_iterable.rs` to mirror the
//! `v8_class/` layout.
//!
//! Two responsibilities:
//!   1. [`SupportedTy`] + [`classify_ty`] — recognise the K/V types
//!      the macro can marshal back to V8.
//!   2. [`gen_to_v8`] — emit the K-or-V → `v8::Local<v8::Value>`
//!      conversion tokens, picking either the built-in classifier
//!      branch or a user-supplied `value_marshal = some_fn` free
//!      function.

use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::Ident;

use crate::must_str;

/// Recognised string / byte / integer types that we know how to
/// marshal back to V8 from the iterator's `next()` snapshot. Returns
/// the codegen branch token-stream for a single value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SupportedTy {
    /// `String` / `USVString` — emit as a UTF-8 v8::String.
    Utf8,
    /// `ByteString` — emit as a Latin-1 one-byte v8::String. The
    /// snapshot stores `Vec<u8>` so we can write_one_byte directly.
    ByteStr,
    /// `u32` — emit as an unsigned integer.
    U32,
    /// `Vec<u8>` — emit as a Uint8Array. Used for value side of pair
    /// iterators that yield raw bytes (e.g. FormData entries that
    /// carry Blob bytes inline).
    Bytes,
}

pub(super) fn classify_ty(ty: &syn::Type) -> Option<SupportedTy> {
    let ident = match crate::type_ident(ty) {
        Some(s) => s,
        None => return None,
    };
    match ident.as_str() {
        "ByteString" => Some(SupportedTy::ByteStr),
        "String" | "USVString" => Some(SupportedTy::Utf8),
        "u32" => Some(SupportedTy::U32),
        "Vec" => {
            // Only Vec<u8> is supported in this position.
            if crate::is_vec_u8(ty) {
                Some(SupportedTy::Bytes)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Emit code that converts a snapshot value (the user's K or V) to a
/// `v8::Local<v8::Value>` named `out_ident`. Caller bound the source
/// value to `src_ident` already.
///
/// When `marshal` is `Some(path)`, the macro emits a call to the
/// user-supplied free function instead of selecting a built-in
/// classifier. Used for the `value_marshal = ident` attribute so
/// consumers like `FormDataIterator` can yield a
/// `(USVString or File)` union without baking that into the macro.
pub(super) fn gen_to_v8(
    ty: &syn::Type,
    src_ident: &Ident,
    out_ident: &Ident,
    marshal: Option<&syn::Path>,
) -> Result<TokenStream2, syn::Error> {
    if let Some(path) = marshal {
        // Custom marshal — bypass type classification entirely. The
        // user's function is responsible for producing a valid
        // `v8::Local<v8::Value>` from `&V`.
        return Ok(quote! {
            let #out_ident: v8::Local<v8::Value> = #path(scope, &#src_ident);
        });
    }
    let kind = classify_ty(ty).ok_or_else(|| {
        syn::Error::new_spanned(
            ty,
            "#[v8_iterable]: unsupported key/value type. Expected one of: \
             ByteString, USVString, String, u32, Vec<u8>",
        )
    })?;
    let scope_tok = quote! { scope };
    Ok(match kind {
        SupportedTy::Utf8 => {
            let s_ref_init = must_str(&scope_tok, &quote! { __s_ref });
            quote! {
                let __s_ref: &str = ::std::convert::AsRef::as_ref(&#src_ident);
                let #out_ident: v8::Local<v8::Value> = #s_ref_init.into();
            }
        },
        SupportedTy::ByteStr => quote! {
            // ByteString → Latin-1 one-byte string. The snapshot stores
            // ByteString (which derefs to &[u8]); copy bytes verbatim.
            let __bytes_ref: &[u8] = ::std::convert::AsRef::as_ref(&#src_ident);
            let #out_ident: v8::Local<v8::Value> = v8::String::new_from_one_byte(
                scope,
                __bytes_ref,
                v8::NewStringType::Normal,
            )
            .unwrap()
            .into();
        },
        SupportedTy::U32 => quote! {
            let __n: u32 = #src_ident;
            let #out_ident: v8::Local<v8::Value> =
                v8::Integer::new_from_unsigned(scope, __n).into();
        },
        SupportedTy::Bytes => quote! {
            // Vec<u8> → Uint8Array. Allocate a fresh ArrayBuffer per
            // yield (snapshot owns the bytes; we can't transfer
            // ownership of the inner Vec).
            let __bytes: &Vec<u8> = &#src_ident;
            let __len = __bytes.len();
            let __ab = v8::ArrayBuffer::new(scope, __len);
            let __store = __ab.get_backing_store();
            for (__i, &__b) in __bytes.iter().enumerate() {
                __store[__i].set(__b);
            }
            let #out_ident: v8::Local<v8::Value> =
                v8::Uint8Array::new(scope, __ab, 0, __len).unwrap().into();
        },
    })
}

/// Helper for K-side classification — keys are always classified
/// (no `value_marshal` opt-out). Used by the orchestrator to surface
/// a clean compile error early when K is unrecognised.
pub(super) fn require_classified(
    ty: &syn::Type,
    role: &str,
) -> Result<(), syn::Error> {
    classify_ty(ty).ok_or_else(|| {
        let msg = format!(
            "#[v8_iterable]: unsupported {role} type. Expected one of: \
             ByteString, USVString, String, u32{}",
            if role == "value" {
                ", Vec<u8> — or supply `value_marshal = some_fn` for arbitrary V."
            } else {
                ""
            }
        );
        syn::Error::new_spanned(ty, msg)
    })?;
    Ok(())
}
