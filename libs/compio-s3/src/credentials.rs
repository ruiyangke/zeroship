//! Resolved S3 credentials.
//!
//! V1 supports exactly one static identity per process: an access-key /
//! secret-key pair plus an optional session token (for temporary STS
//! credentials). There is **no provider chain** and **no metadata-service
//! lookup** — the higher layers resolve credentials from CLI / env / `[secrets]`
//! and hand a concrete [`S3Credentials`] to the client. See the proposal
//! "Credentials" section.

/// A resolved set of static S3 credentials.
///
/// The secret is held in memory for the process lifetime; `Debug` redacts it.
#[derive(Clone)]
pub struct S3Credentials {
    /// AWS access key id (e.g. `AKIA…`).
    pub access_key_id: String,
    /// AWS secret access key.
    pub secret_access_key: String,
    /// Optional session token for temporary (STS) credentials.
    pub session_token: Option<String>,
}

impl S3Credentials {
    /// Construct a credential set.
    pub fn new(
        access_key_id: impl Into<String>,
        secret_access_key: impl Into<String>,
        session_token: Option<String>,
    ) -> Self {
        Self {
            access_key_id: access_key_id.into(),
            secret_access_key: secret_access_key.into(),
            session_token,
        }
    }

    /// Whether temporary-credential session token signing applies.
    #[must_use]
    pub const fn has_session_token(&self) -> bool {
        self.session_token.is_some()
    }
}

impl std::fmt::Debug for S3Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Credentials")
            .field("access_key_id", &Redacted)
            .field("secret_access_key", &Redacted)
            .field("session_token", &self.session_token.as_ref().map(|_| Redacted))
            .finish()
    }
}

/// Debug placeholder that never prints secret material.
struct Redacted;
impl std::fmt::Debug for Redacted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_redacts_secrets() {
        let c = S3Credentials::new("AKIAEXAMPLE", "supersecret", Some("sessiontokval".into()));
        let s = format!("{c:?}");
        assert!(!s.contains("supersecret"));
        assert!(!s.contains("AKIAEXAMPLE"));
        // Note: the *field name* `session_token` is printed by debug_struct;
        // assert the secret token *value* is absent, not a generic substring.
        assert!(!s.contains("sessiontokval"));
        assert!(s.contains("<redacted>"));
    }

    #[test]
    fn session_token_flag() {
        assert!(!S3Credentials::new("a", "b", None).has_session_token());
        assert!(S3Credentials::new("a", "b", Some("t".into())).has_session_token());
    }
}
