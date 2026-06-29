//! Platform auth-provider token verification seam.
//!
//! This is enum dispatch rather than a boxed async trait. The cyper-based
//! Hydra client is thread-local/`!Send`, so the hot path keeps inherent
//! `async fn`s and matches on the selected concrete provider.

use thiserror::Error;

use crate::hydra::{HydraIntrospector, IntrospectError, IntrospectResult};

/// The selected platform auth provider.
#[derive(Debug)]
pub enum AuthProvider {
    Hydra(HydraProvider),
    // Supabase(...) — later slice.
}

impl AuthProvider {
    #[allow(clippy::future_not_send)]
    pub async fn verify_token(&self, token: &str) -> Result<VerifiedToken, VerifyTokenError> {
        match self {
            Self::Hydra(provider) => provider.verify_token(token).await,
        }
    }

    #[must_use]
    pub fn issuer(&self) -> &str {
        match self {
            Self::Hydra(provider) => provider.issuer(),
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

    #[error("inactive token")]
    InactiveToken,

    #[error("hydra introspection response missing subject")]
    MissingSubject,
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
}
