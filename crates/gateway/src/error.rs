//! Gateway-wide error type.
//!
//! Today this is consumed only by `sessions` (P3-U4); other modules
//! continue to surface their own local error enums or `ntex` responses
//! until the gateway's error story is consolidated.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum GatewayError {
    #[error("database: {0}")]
    Db(String),
    /// Boot-time configuration error — bad CLI flag, unreadable file,
    /// malformed key, etc. Surfaced by `signing::load_from_path` when
    /// the `--signing-key-file` argument points at something we can't
    /// parse as a PKCS#8 Ed25519 private key (Phase 8 U1).
    #[error("config: {0}")]
    Config(String),
    /// Refuse to boot with a credential file that can be read or
    /// modified by group/world users.
    #[error("insecure permissions on {path}: mode {mode:o}")]
    InsecurePermissions {
        path: std::path::PathBuf,
        mode: u32,
    },
    /// Runtime failure that shouldn't happen in a healthy gateway —
    /// JWT encode/decode error, system clock failure, etc. Used by
    /// `wrapper_token::Issuer::issue` and `Verifier::verify` (Phase 8
    /// U2) for both signing-side and verification-side failures
    /// (signature mismatch, expired token, kid mismatch, …). The
    /// dispatcher surfaces these as `500 Internal Server Error` on
    /// the issue side and `401 invalid_token` on the verify side.
    #[error("internal: {0}")]
    Internal(String),
}

pub type Result<T> = std::result::Result<T, GatewayError>;
