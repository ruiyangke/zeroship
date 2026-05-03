//! Encrypted PKCS#8 import/export (PBES2 / PBKDF2).
//!
//! Per `docs/proposals/node-crypto-native.md` §IV.4a / D-N37.
//!
//! **Stage C status:** placeholder. The full PBES2 raw-FFI path is
//! Stage E in the design — it requires `aws-lc-sys` raw FFI to
//! `PKCS8_marshal_encrypted_private_key` / `PKCS8_parse_encrypted_private_key`
//! plus a 12-entry cipher whitelist (RFC 8018). Surfacing it as a
//! Stage-C deliverable would have shipped a 250 LOC FFI block with
//! limited testing. We surface the spec-correct
//! `ERR_CRYPTO_UNSUPPORTED_OPERATION` here and leave the real impl as
//! a Stage E follow-up. Creator apps that need encrypted-PKCS#8
//! import/export get a clean error rather than a silent failure.

use crate::state::OpError;

/// Encrypt an unencrypted PKCS#8 DER blob using PBES2 / PBKDF2 with
/// the named cipher.
pub fn encrypt_pkcs8_private_key(
    _private_key_pkcs8_der: &[u8],
    _cipher_name: &str,
    _passphrase: &[u8],
    _iterations: Option<i32>,
) -> Result<Vec<u8>, OpError> {
    Err(OpError::node(
        "ERR_CRYPTO_UNSUPPORTED_OPERATION",
        "Encrypted PKCS#8 (PBES2) is not yet supported (Stage E)",
    ))
}

/// Decrypt an encrypted PKCS#8 DER blob back to plain PKCS#8 bytes.
pub fn decrypt_pkcs8_private_key(
    _encrypted_pkcs8_der: &[u8],
    _passphrase: &[u8],
) -> Result<Vec<u8>, OpError> {
    Err(OpError::node(
        "ERR_CRYPTO_UNSUPPORTED_OPERATION",
        "Encrypted PKCS#8 (PBES2) is not yet supported (Stage E)",
    ))
}
