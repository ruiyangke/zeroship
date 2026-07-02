//! Platform auth-provider token verification.
//!
//! This is enum dispatch rather than a boxed async trait, keeping the hot path
//! on inherent `async fn`s with matches on the selected concrete provider.

use thiserror::Error;

mod platform;
mod supabase;

pub use platform::{
    PlatformConfig, PlatformConfigError, PlatformProvider, PlatformVerifyError,
};
pub use supabase::{
    SupabaseConfig, SupabaseConfigError, SupabaseProvider, SupabaseVerification,
    SupabaseVerifyError,
};

/// The selected platform auth provider.
#[derive(Debug)]
pub enum AuthProvider {
    Supabase(SupabaseProvider),
    Platform(PlatformProvider),
    DualIssuer(DualIssuerProvider),
}

impl AuthProvider {
    #[allow(clippy::future_not_send)]
    pub async fn verify_token(&self, token: &str) -> Result<VerifiedToken, VerifyTokenError> {
        match self {
            Self::Supabase(provider) => provider
                .verify_token(token)
                .await
                .map_err(|_err| VerifyTokenError::InactiveToken),
            Self::Platform(provider) => provider
                .verify_token(token)
                .await
                .map_err(VerifyTokenError::PlatformVerification),
            Self::DualIssuer(provider) => provider.verify_token(token).await,
        }
    }

    #[must_use]
    pub fn issuer(&self) -> &str {
        match self {
            Self::Supabase(provider) => provider.issuer(),
            Self::Platform(provider) => provider.issuer(),
            Self::DualIssuer(provider) => provider.legacy_issuer(),
        }
    }

    #[must_use]
    pub fn platform_issuer(&self) -> Option<&str> {
        match self {
            Self::Platform(provider) => Some(provider.issuer()),
            Self::DualIssuer(provider) => Some(provider.platform_issuer()),
            Self::Supabase(_) => None,
        }
    }

    #[must_use]
    pub fn supabase_url(&self) -> Option<&str> {
        match self {
            Self::Supabase(provider) => Some(provider.url()),
            Self::Platform(_) => None,
            Self::DualIssuer(provider) => provider.supabase_url(),
        }
    }

    #[must_use]
    pub fn supabase_anon_key(&self) -> Option<&str> {
        match self {
            Self::Supabase(provider) => Some(provider.anon_key()),
            Self::Platform(_) => None,
            Self::DualIssuer(provider) => provider.supabase_anon_key(),
        }
    }

    #[must_use]
    pub fn supabase_service_role_key(&self) -> Option<&str> {
        match self {
            Self::Supabase(provider) => provider.service_role_key(),
            Self::Platform(_) => None,
            Self::DualIssuer(provider) => provider.supabase_service_role_key(),
        }
    }
}

/// A migration verifier that accepts platform-issued access tokens and one
/// configured legacy provider. It dispatches only on the unverified `iss`; the
/// selected arm still performs the signature, algorithm, issuer, expiry, and
/// token-type checks.
#[derive(Debug)]
pub struct DualIssuerProvider {
    platform: PlatformProvider,
    legacy: LegacyAuthProvider,
}

impl DualIssuerProvider {
    #[must_use]
    pub fn new(platform: PlatformProvider, legacy: LegacyAuthProvider) -> Self {
        Self { platform, legacy }
    }

    #[allow(clippy::future_not_send)]
    async fn verify_token(&self, token: &str) -> Result<VerifiedToken, VerifyTokenError> {
        let iss = unverified_issuer(token).ok_or(VerifyTokenError::MissingIssuer)?;
        if iss == self.platform.issuer() {
            return self
                .platform
                .verify_token(token)
                .await
                .map_err(VerifyTokenError::PlatformVerification);
        }
        if iss == self.legacy.issuer() {
            return self.legacy.verify_token(token).await;
        }
        Err(VerifyTokenError::UnknownIssuer(iss))
    }

    #[must_use]
    fn legacy_issuer(&self) -> &str {
        self.legacy.issuer()
    }

    #[must_use]
    fn platform_issuer(&self) -> &str {
        self.platform.issuer()
    }

    #[must_use]
    fn supabase_url(&self) -> Option<&str> {
        self.legacy.supabase_url()
    }

    #[must_use]
    fn supabase_anon_key(&self) -> Option<&str> {
        self.legacy.supabase_anon_key()
    }

    #[must_use]
    fn supabase_service_role_key(&self) -> Option<&str> {
        self.legacy.supabase_service_role_key()
    }
}

/// The legacy side of a dual-issuer migration. Kept separate from
/// `AuthProvider` so the dual verifier's async dispatch is not recursive.
#[derive(Debug)]
pub enum LegacyAuthProvider {
    Supabase(SupabaseProvider),
}

impl LegacyAuthProvider {
    #[allow(clippy::future_not_send)]
    async fn verify_token(&self, token: &str) -> Result<VerifiedToken, VerifyTokenError> {
        match self {
            Self::Supabase(provider) => provider
                .verify_token(token)
                .await
                .map_err(|_err| VerifyTokenError::InactiveToken),
        }
    }

    #[must_use]
    pub fn issuer(&self) -> &str {
        match self {
            Self::Supabase(provider) => provider.issuer(),
        }
    }

    #[must_use]
    fn supabase_url(&self) -> Option<&str> {
        match self {
            Self::Supabase(provider) => Some(provider.url()),
        }
    }

    #[must_use]
    fn supabase_anon_key(&self) -> Option<&str> {
        match self {
            Self::Supabase(provider) => Some(provider.anon_key()),
        }
    }

    #[must_use]
    fn supabase_service_role_key(&self) -> Option<&str> {
        match self {
            Self::Supabase(provider) => provider.service_role_key(),
        }
    }
}

impl From<LegacyAuthProvider> for AuthProvider {
    fn from(provider: LegacyAuthProvider) -> Self {
        match provider {
            LegacyAuthProvider::Supabase(provider) => Self::Supabase(provider),
        }
    }
}

/// Provider-neutral verified token claims. This is not an authorization
/// decision; control maps it into platform policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedToken {
    pub provider_subject: String,
    pub email: Option<String>,
    pub email_verified: bool,
    pub session_id: Option<String>,
    pub provider_authz: ProviderAuthz,
    pub exp: u64,
    /// OAuth client that minted the token, when the verifier can attest it
    /// from the signed/introspected token. Platform OP access tokens always
    /// carry this; legacy providers may not.
    pub client_id: Option<String>,
    /// Issued-at timestamp, epoch seconds, when present and trusted.
    pub iat: Option<u64>,
    /// Provider token audiences. Resource servers gate this against their
    /// expected OAuth audience.
    pub aud: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderAuthz {
    OAuthScope(String),
    GoTrueRole(String),
}

#[derive(Debug, Error)]
pub enum VerifyTokenError {
    #[error("platform token verification failed: {0}")]
    PlatformVerification(PlatformVerifyError),

    #[error("inactive token")]
    InactiveToken,

    #[error("provider token missing subject")]
    MissingSubject,

    #[error("JWT bearer missing issuer")]
    MissingIssuer,

    #[error("unknown token issuer: {0}")]
    UnknownIssuer(String),
}

/// Peek the unverified `iss` claim from a compact JWS payload.
///
/// This is only for verifier routing. The selected provider must still verify
/// the signature, issuer, algorithm, lifetime, and token type.
#[must_use]
pub fn unverified_issuer(token: &str) -> Option<String> {
    use base64::Engine as _;

    let mut parts = token.split('.');
    let _header = parts.next()?;
    let payload_b64 = parts.next()?;
    let _sig = parts.next()?;
    if parts.next().is_some() {
        return None;
    }

    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload_b64)
        .ok()?;
    let body: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    body.get("iss")
        .and_then(|iss| iss.as_str())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unverified_issuer_peeks_only_compact_jws_payloads() {
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
        use serde_json::json;

        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
        let payload = URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&json!({"iss":"https://auth.zeroship.test"}))
                .expect("payload json"),
        );
        assert_eq!(
            unverified_issuer(&format!("{header}.{payload}.")),
            Some("https://auth.zeroship.test".to_string())
        );
        assert_eq!(unverified_issuer("opaque-token"), None);
        assert_eq!(unverified_issuer("a.b.c.d"), None);
    }
}
