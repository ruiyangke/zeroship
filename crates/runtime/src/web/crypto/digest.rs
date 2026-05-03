//! `subtle.digest(algorithm, data)` — spec §32.
//!
//! Per `docs/proposals/webcrypto-native.md` §IV.10. aws-lc-rs's
//! `digest::digest` covers the four normative hashes (SHA-1 via the
//! `_FOR_LEGACY_USE_ONLY` constant, SHA-256/384/512 normally).

use super::key_material::HashAlgo;
use crate::state::OpError;

pub fn digest_bytes(hash: HashAlgo, data: &[u8]) -> Vec<u8> {
    let alg = aws_lc_alg(hash);
    let d = aws_lc_rs::digest::digest(alg, data);
    d.as_ref().to_vec()
}

pub(crate) fn aws_lc_alg(hash: HashAlgo) -> &'static aws_lc_rs::digest::Algorithm {
    match hash {
        HashAlgo::Sha1 => &aws_lc_rs::digest::SHA1_FOR_LEGACY_USE_ONLY,
        HashAlgo::Sha256 => &aws_lc_rs::digest::SHA256,
        HashAlgo::Sha384 => &aws_lc_rs::digest::SHA384,
        HashAlgo::Sha512 => &aws_lc_rs::digest::SHA512,
    }
}

/// Resolve a JS algorithm value (`"SHA-256"` or `{ name: "SHA-256" }`)
/// to a `HashAlgo`. Spec §32 + §18.4.4.
pub fn resolve_digest_algorithm(
    scope: &mut v8::PinScope,
    alg: v8::Local<v8::Value>,
) -> Result<HashAlgo, OpError> {
    let name = if alg.is_string() {
        alg.to_rust_string_lossy(scope)
    } else if let Ok(obj) = v8::Local::<v8::Object>::try_from(alg) {
        let k = v8::String::new(scope, "name").unwrap();
        let v = obj.get(scope, k.into()).ok_or_else(|| {
            OpError::dom("NotSupportedError", "digest: missing 'name'")
        })?;
        if !v.is_string() {
            return Err(OpError::type_error("digest: 'name' must be a string"));
        }
        v.to_rust_string_lossy(scope)
    } else {
        return Err(OpError::type_error(
            "digest: algorithm must be a string or object",
        ));
    };
    HashAlgo::from_str(&name).ok_or_else(|| {
        OpError::dom(
            "NotSupportedError",
            format!("Unrecognised digest algorithm '{}'", name),
        )
    })
}
