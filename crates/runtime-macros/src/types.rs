//! Type-classification helpers — predicate fns over `syn::Type` that
//! recognise the macro's well-known type shapes (Vec<u8>, ByteString,
//! USVString, EnforceRange*, Vec<Vec<u8>>, etc.).
//!
//! Wave 4 cleanup commit: split out from `lib.rs` to keep that file
//! near the ≤500 LOC budget (design `docs/proposals/runtime-macros-
//! refactor.md` post-Wave-4 target). The helpers are pub(crate)-only
//! and consumed by [`super::known_type`] (the slow-path arg-extract
//! classifier) and [`super::codegen`] (the return-side marshalling).
//!
//! Detection is by last-segment ident (e.g. `Vec`, `ByteString`) — we
//! don't enforce a full path since users typically import the type
//! into scope and refer to it bare (`use
//! ::zeroship_runtime::byte_string::ByteString;`). The macro relies on
//! ident hygiene: a user `struct Vec` shadowing the std type would
//! confuse the classifier, but that's a global Rust footgun and the
//! macro's contract documents the well-known names.

use syn::{GenericArgument, Ident, PathArguments, Type, TypePath};

/// A single user-method parameter post-decomposition: name + type. The
/// `parse_params_skipping_self` helper in `v8_class::helpers` builds
/// `Vec<Param>` from a `syn::ImplItemFn`, dropping the `self` receiver
/// so call sites can iterate over the JS-visible args uniformly.
pub(crate) struct Param {
    pub(crate) name: Ident,
    pub(crate) ty: Type,
}

/// True for the unit type `()`. Returned by mutator-style methods like
/// `Result<(), OpError>` that have nothing to set on `rv` — they want
/// the JS-visible call to evaluate to `undefined`.
pub(crate) fn is_unit_type(ty: &Type) -> bool {
    matches!(ty, Type::Tuple(t) if t.elems.is_empty())
}

/// Extract the last segment identifier from a type path (e.g. `String`,
/// `Option`, `Result`). Returns `None` for non-path types (references,
/// tuples, fn pointers, …).
pub(crate) fn type_ident(ty: &Type) -> Option<String> {
    if let Type::Path(TypePath { path, .. }) = ty {
        path.segments.last().map(|s| s.ident.to_string())
    } else {
        None
    }
}

/// Check if type is `Vec<u8>` — used for binary data args (reads from
/// ArrayBufferView).
pub(crate) fn is_vec_u8(ty: &Type) -> bool {
    type_ident(ty).as_deref() == Some("Vec")
        && first_generic_arg(ty).and_then(type_ident).as_deref() == Some("u8")
}

/// Check if type is `Vec<Vec<u8>>` — used by IDL methods like
/// `getSetCookie() -> sequence<ByteString>`. Marshalled as a JS Array
/// of ByteString (each element is a Latin-1 one-byte string).
pub(crate) fn is_vec_vec_u8(ty: &Type) -> bool {
    type_ident(ty).as_deref() == Some("Vec")
        && first_generic_arg(ty).map(is_vec_u8).unwrap_or(false)
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

/// Check if type is the `EnforceRangeU32` newtype — companion to
/// `EnforceRangeU64` for WebIDL `[EnforceRange] unsigned long`. Used
/// by the WebCrypto IDL surface (Pbkdf2Params.iterations,
/// RsaKeyGenParams.modulusLength, deriveBits.length, etc.). See
/// `docs/proposals/webcrypto-native.md` D-20.
pub(crate) fn is_enforce_range_u32(ty: &Type) -> bool {
    type_ident(ty).as_deref() == Some("EnforceRangeU32")
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

/// Extract the first generic type argument (e.g. `String` from
/// `Option<String>`). Used by the `Option<T>` / `Result<T, _>` /
/// `Vec<T>` cascades to recurse into the wrapper's inner type.
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
