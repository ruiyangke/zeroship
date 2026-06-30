//! Platform auth-provider token verification seam.
//!
//! This is enum dispatch rather than a boxed async trait. The cyper-based
//! Hydra client is thread-local/`!Send`, so the hot path keeps inherent
//! `async fn`s and matches on the selected concrete provider.

use thiserror::Error;

use crate::hydra::{HydraIntrospector, IntrospectError, IntrospectResult};

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
    Hydra(HydraProvider),
    Supabase(SupabaseProvider),
    Platform(PlatformProvider),
    DualIssuer(DualIssuerProvider),
}

impl AuthProvider {
    #[allow(clippy::future_not_send)]
    pub async fn verify_token(&self, token: &str) -> Result<VerifiedToken, VerifyTokenError> {
        match self {
            Self::Hydra(provider) => provider.verify_token(token).await,
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
            Self::Hydra(provider) => provider.issuer(),
            Self::Supabase(provider) => provider.issuer(),
            Self::Platform(provider) => provider.issuer(),
            Self::DualIssuer(provider) => provider.legacy_issuer(),
        }
    }

    #[must_use]
    pub fn supabase_url(&self) -> Option<&str> {
        match self {
            Self::Hydra(_) => None,
            Self::Supabase(provider) => Some(provider.url()),
            Self::Platform(_) => None,
            Self::DualIssuer(provider) => provider.supabase_url(),
        }
    }

    #[must_use]
    pub fn supabase_anon_key(&self) -> Option<&str> {
        match self {
            Self::Hydra(_) => None,
            Self::Supabase(provider) => Some(provider.anon_key()),
            Self::Platform(_) => None,
            Self::DualIssuer(provider) => provider.supabase_anon_key(),
        }
    }

    #[must_use]
    pub fn supabase_service_role_key(&self) -> Option<&str> {
        match self {
            Self::Hydra(_) => None,
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
    Hydra(HydraProvider),
    Supabase(SupabaseProvider),
}

impl LegacyAuthProvider {
    #[allow(clippy::future_not_send)]
    async fn verify_token(&self, token: &str) -> Result<VerifiedToken, VerifyTokenError> {
        match self {
            Self::Hydra(provider) => provider.verify_token(token).await,
            Self::Supabase(provider) => provider
                .verify_token(token)
                .await
                .map_err(|_err| VerifyTokenError::InactiveToken),
        }
    }

    #[must_use]
    pub fn issuer(&self) -> &str {
        match self {
            Self::Hydra(provider) => provider.issuer(),
            Self::Supabase(provider) => provider.issuer(),
        }
    }

    #[must_use]
    fn supabase_url(&self) -> Option<&str> {
        match self {
            Self::Hydra(_) => None,
            Self::Supabase(provider) => Some(provider.url()),
        }
    }

    #[must_use]
    fn supabase_anon_key(&self) -> Option<&str> {
        match self {
            Self::Hydra(_) => None,
            Self::Supabase(provider) => Some(provider.anon_key()),
        }
    }

    #[must_use]
    fn supabase_service_role_key(&self) -> Option<&str> {
        match self {
            Self::Hydra(_) => None,
            Self::Supabase(provider) => provider.service_role_key(),
        }
    }
}

impl From<LegacyAuthProvider> for AuthProvider {
    fn from(provider: LegacyAuthProvider) -> Self {
        match provider {
            LegacyAuthProvider::Hydra(provider) => Self::Hydra(provider),
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
    /// Provider token audiences. The Hydra control-plane deploy-token path
    /// gates this against the expected platform OAuth audience.
    pub aud: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderAuthz {
    OAuthScope(String),
    GoTrueRole(String),
}

/// Hydra-backed token verifier.
#[derive(Debug)]
pub struct HydraProvider {
    introspector: HydraIntrospector,
    issuer: String,
}

impl HydraProvider {
    #[must_use]
    pub fn new(introspector: HydraIntrospector) -> Self {
        let issuer = introspector.admin_url().to_owned();
        Self {
            introspector,
            issuer,
        }
    }

    #[must_use]
    pub fn new_with_issuer(
        introspector: HydraIntrospector,
        issuer: impl Into<String>,
    ) -> Self {
        Self {
            introspector,
            issuer: issuer.into(),
        }
    }

    #[allow(clippy::future_not_send)]
    pub async fn verify_token(&self, token: &str) -> Result<VerifiedToken, VerifyTokenError> {
        let result = self.introspector.introspect(token).await?;
        hydra_verified_token(result)
    }

    #[must_use]
    pub fn issuer(&self) -> &str {
        &self.issuer
    }
}

#[derive(Debug, Error)]
pub enum VerifyTokenError {
    #[error("hydra introspection failed: {0}")]
    HydraIntrospection(#[from] IntrospectError),

    #[error("platform token verification failed: {0}")]
    PlatformVerification(PlatformVerifyError),

    #[error("inactive token")]
    InactiveToken,

    #[error("hydra introspection response missing subject")]
    MissingSubject,

    #[error("JWT bearer missing issuer")]
    MissingIssuer,

    #[error("unknown token issuer: {0}")]
    UnknownIssuer(String),
}

fn hydra_verified_token(result: IntrospectResult) -> Result<VerifiedToken, VerifyTokenError> {
    if !result.active {
        return Err(VerifyTokenError::InactiveToken);
    }

    let provider_subject = result.sub.ok_or(VerifyTokenError::MissingSubject)?;
    Ok(VerifiedToken {
        provider_subject,
        email: result.email,
        email_verified: result.email_verified.unwrap_or(false),
        session_id: result.session_id,
        provider_authz: ProviderAuthz::OAuthScope(result.scope.unwrap_or_default()),
        exp: result.exp.unwrap_or_default(),
        aud: result.aud,
    })
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
    fn hydra_introspection_maps_to_verified_token() {
        let verified = hydra_verified_token(IntrospectResult {
            active: true,
            sub: Some("usr_01HABC".to_string()),
            scope: Some("apps:read apps:deploy".to_string()),
            aud: Some(vec!["control.zeroship.ai".to_string()]),
            client_id: Some("oauth-test-client".to_string()),
            email: Some("creator@example.test".to_string()),
            email_verified: None,
            session_id: Some("sid_123".to_string()),
            exp: Some(1_800_000_000),
        })
        .expect("active token maps");

        assert_eq!(verified.provider_subject, "usr_01HABC");
        assert_eq!(verified.email.as_deref(), Some("creator@example.test"));
        assert!(!verified.email_verified);
        assert_eq!(verified.session_id.as_deref(), Some("sid_123"));
        assert_eq!(
            verified.provider_authz,
            ProviderAuthz::OAuthScope("apps:read apps:deploy".to_string())
        );
        assert_eq!(verified.exp, 1_800_000_000);
        assert_eq!(
            verified.aud.as_deref(),
            Some(&["control.zeroship.ai".to_string()][..])
        );
    }

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
