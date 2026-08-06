//! Gateway-wide error type.
//!
//! Used by the gateway's database helpers, signing-key loader, and session-token
//! issuer and verifier. Request handlers translate runtime failures into
//! endpoint-specific responses; signing and configuration failures abort startup.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum GatewayError {
    #[error("database: {0}")]
    Db(String),
    /// Boot-time signing-key configuration error — unreadable file, malformed
    /// key, wrong key type, etc. Surfaced by `signing::load_from_path` when the
    /// `--signing-key-file` argument does not name a usable PKCS#8 Ed25519 key.
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
    /// `session_token::Issuer::issue` and `Verifier::verify` for both
    /// signing-side and verification-side failures (signature mismatch,
    /// expired token, kid mismatch, …). The dispatcher surfaces these as
    /// `500 Internal Server Error` on the issue side and "no valid session"
    /// on the verify side.
    #[error("internal: {0}")]
    Internal(String),
}

pub type Result<T> = std::result::Result<T, GatewayError>;
