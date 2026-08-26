//! Platform auth-provider token verification.
//!
//! A process trusts a SET of providers and picks one per token by the token's
//! unverified `iss`. That is the whole dispatch rule, and it is why the set is
//! a `Vec` of concrete backends rather than a type per combination: one
//! configured provider and two configured providers are the same operation on
//! different-length sets, so they must not be different types. The predecessor
//! of this module had `enum { Supabase, Platform, DualIssuer }`, which needed a
//! new variant for every PAIR of backends and would have needed a `Triple` for
//! every trio.
//!
//! Dispatch stays enum-based rather than a boxed async trait. The providers are
//! `!Send` by construction (their `cyper` clients live in a `thread_local!`),
//! so `dyn` dispatch would mean hand-rolled `Pin<Box<dyn Future>>` plus an
//! allocation on a path that today is an inherent `async fn`.
//!
//! Selecting by `iss` is ROUTING ONLY. The selected backend still verifies the
//! signature, algorithm, issuer, lifetime and token type; a token that names an
//! issuer is not thereby trusted by it.

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

/// One configured backend.
///
/// Adding a provider adds ONE variant here plus its config type. Nothing about
/// the set, the dispatch, or the number of simultaneously trusted providers
/// changes with it.
#[derive(Debug)]
pub enum ConfiguredProvider {
    /// The platform's own OP, verified against its JWKS.
    Platform(PlatformProvider),
    /// Supabase Auth / GoTrue.
    Supabase(SupabaseProvider),
}

impl ConfiguredProvider {
    /// The `iss` this backend claims. Never empty: both config constructors
    /// reject an empty issuer, which is what makes the set indexable by it.
    #[must_use]
    pub fn issuer(&self) -> &str {
        match self {
            Self::Platform(provider) => provider.issuer(),
            Self::Supabase(provider) => provider.issuer(),
        }
    }

    #[allow(clippy::future_not_send)]
    async fn verify_token(&self, token: &str) -> Result<VerifiedToken, VerifyTokenError> {
        match self {
            Self::Platform(provider) => provider
                .verify_token(token)
                .await
                .map_err(VerifyTokenError::PlatformVerification),
            Self::Supabase(provider) => provider
                .verify_token(token)
                .await
                .map_err(|_err| VerifyTokenError::InactiveToken),
        }
    }
}

/// Every auth provider this process trusts.
///
/// Order is not significant: [`AuthProvider::new`] refuses two providers that
/// claim one issuer, so at most one element can ever match a token.
#[derive(Debug)]
pub struct AuthProvider {
    providers: Vec<ConfiguredProvider>,
}

impl AuthProvider {
    /// Build a trusted set.
    ///
    /// # Errors
    ///
    /// [`AuthProviderSetError::Empty`] when nothing is configured - a process
    /// that trusts no issuer rejects every request, and finding that out at
    /// boot is the point. [`AuthProviderSetError::DuplicateIssuer`] when two
    /// providers claim one `iss`: the dispatch would silently route every
    /// token for that issuer to whichever happened to be first.
    pub fn new(providers: Vec<ConfiguredProvider>) -> Result<Self, AuthProviderSetError> {
        if providers.is_empty() {
            return Err(AuthProviderSetError::Empty);
        }
        for (index, provider) in providers.iter().enumerate() {
            if providers[..index]
                .iter()
                .any(|earlier| earlier.issuer() == provider.issuer())
            {
                return Err(AuthProviderSetError::DuplicateIssuer(
                    provider.issuer().to_owned(),
                ));
            }
        }
        Ok(Self { providers })
    }

    /// The one-element set holding the platform OP.
    #[must_use]
    pub fn platform(provider: PlatformProvider) -> Self {
        Self {
            providers: vec![ConfiguredProvider::Platform(provider)],
        }
    }

    /// The one-element set holding Supabase.
    #[must_use]
    pub fn supabase(provider: SupabaseProvider) -> Self {
        Self {
            providers: vec![ConfiguredProvider::Supabase(provider)],
        }
    }

    /// Verify `token` against whichever configured provider claims its `iss`.
    ///
    /// # Errors
    ///
    /// [`VerifyTokenError::MissingIssuer`] when the bearer is not a compact JWS
    /// carrying `iss`, [`VerifyTokenError::UnknownIssuer`] when no configured
    /// provider claims it, and the selected provider's own failure otherwise.
    #[allow(clippy::future_not_send)]
    pub async fn verify_token(&self, token: &str) -> Result<VerifiedToken, VerifyTokenError> {
        let iss = unverified_issuer(token).ok_or(VerifyTokenError::MissingIssuer)?;
        let provider = self
            .providers
            .iter()
            .find(|provider| provider.issuer() == iss)
            .ok_or(VerifyTokenError::UnknownIssuer(iss))?;
        provider.verify_token(token).await
    }

    /// Every trusted issuer, in configuration order.
    pub fn issuers(&self) -> impl Iterator<Item = &str> {
        self.providers.iter().map(ConfiguredProvider::issuer)
    }

    /// The platform OP's issuer, when the platform OP is trusted.
    #[must_use]
    pub fn platform_issuer(&self) -> Option<&str> {
        self.providers.iter().find_map(|provider| match provider {
            ConfiguredProvider::Platform(platform) => Some(platform.issuer()),
            ConfiguredProvider::Supabase(_) => None,
        })
    }

    /// The GoTrue issuer, when Supabase is trusted.
    #[must_use]
    pub fn supabase_issuer(&self) -> Option<&str> {
        self.supabase_provider().map(SupabaseProvider::issuer)
    }

    #[must_use]
    pub fn supabase_url(&self) -> Option<&str> {
        self.supabase_provider().map(SupabaseProvider::url)
    }

    #[must_use]
    pub fn supabase_anon_key(&self) -> Option<&str> {
        self.supabase_provider().map(SupabaseProvider::anon_key)
    }

    #[must_use]
    pub fn supabase_service_role_key(&self) -> Option<&str> {
        self.supabase_provider()
            .and_then(SupabaseProvider::service_role_key)
    }

    /// The Supabase backend, when it is in the set.
    ///
    /// Provider-SPECIFIC lookups like this exist only where a handler genuinely
    /// needs one backend's configuration - the GoTrue device flow needs the
    /// GoTrue base URL, and no generic accessor could supply it. They are not
    /// part of dispatch, so a new backend adds one only if some handler asks
    /// for its config.
    fn supabase_provider(&self) -> Option<&SupabaseProvider> {
        self.providers.iter().find_map(|provider| match provider {
            ConfiguredProvider::Supabase(supabase) => Some(supabase),
            ConfiguredProvider::Platform(_) => None,
        })
    }
}

/// Why a trusted set was refused at construction.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum AuthProviderSetError {
    #[error("at least one auth provider must be configured")]
    Empty,

    #[error("two auth providers claim the same issuer: {0}")]
    DuplicateIssuer(String),
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

    const PLATFORM_ISSUER: &str = "https://auth.zeroship.test/oauth2";
    const GOTRUE_ISSUER: &str = "https://project.supabase.co/auth/v1";

    fn platform_provider(issuer: &str) -> PlatformProvider {
        PlatformProvider::new(
            PlatformConfig::new(issuer.to_owned(), None).expect("platform config"),
        )
    }

    fn supabase_provider(issuer: &str) -> SupabaseProvider {
        SupabaseProvider::new(
            SupabaseConfig::new(
                "https://project.supabase.co",
                "anon",
                None,
                Some("test-supabase-jwt-secret-at-least-32-bytes".to_owned()),
                None,
                issuer.to_owned(),
            )
            .expect("supabase config"),
        )
    }

    fn token_with_issuer(issuer: &str) -> String {
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
        use serde_json::json;

        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
        let payload = URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&json!({ "iss": issuer })).expect("payload json"),
        );
        format!("{header}.{payload}.")
    }

    #[test]
    fn one_provider_and_two_providers_are_the_same_type() {
        // The property that replaced `DualIssuer`. If these two ever needed
        // different types again, this line would not compile.
        let sets: Vec<AuthProvider> = vec![
            AuthProvider::platform(platform_provider(PLATFORM_ISSUER)),
            AuthProvider::new(vec![
                ConfiguredProvider::Platform(platform_provider(PLATFORM_ISSUER)),
                ConfiguredProvider::Supabase(supabase_provider(GOTRUE_ISSUER)),
            ])
            .expect("two distinct issuers"),
        ];
        assert_eq!(sets[0].issuers().count(), 1);
        assert_eq!(sets[1].issuers().count(), 2);
    }

    #[test]
    fn the_two_provider_set_exposes_both_sides_the_way_dual_issuer_did() {
        let set = AuthProvider::new(vec![
            ConfiguredProvider::Platform(platform_provider(PLATFORM_ISSUER)),
            ConfiguredProvider::Supabase(supabase_provider(GOTRUE_ISSUER)),
        ])
        .expect("two distinct issuers");

        assert_eq!(set.platform_issuer(), Some(PLATFORM_ISSUER));
        assert_eq!(set.supabase_issuer(), Some(GOTRUE_ISSUER));
        assert_eq!(set.supabase_url(), Some("https://project.supabase.co"));
        assert_eq!(set.supabase_anon_key(), Some("anon"));
        assert_eq!(set.supabase_service_role_key(), None);
        assert_eq!(
            set.issuers().collect::<Vec<_>>(),
            vec![PLATFORM_ISSUER, GOTRUE_ISSUER]
        );
    }

    #[test]
    fn a_one_provider_set_answers_none_for_the_backend_it_lacks() {
        // The one-variable control for the case above: same accessors, same
        // set type, one thing changed - the Supabase element is absent.
        let set = AuthProvider::platform(platform_provider(PLATFORM_ISSUER));
        assert_eq!(set.platform_issuer(), Some(PLATFORM_ISSUER));
        assert_eq!(set.supabase_issuer(), None);
        assert_eq!(set.supabase_url(), None);
        assert_eq!(set.supabase_anon_key(), None);
        assert_eq!(set.supabase_service_role_key(), None);
    }

    #[test]
    fn an_empty_set_is_refused_at_construction() {
        // A process trusting no issuer rejects every request. Boot is when an
        // operator can still act on that.
        assert_eq!(
            AuthProvider::new(Vec::new()).expect_err("empty set must be refused"),
            AuthProviderSetError::Empty
        );
    }

    #[test]
    fn two_providers_claiming_one_issuer_are_refused_at_construction() {
        // Without this the linear scan would route every token for the shared
        // issuer to whichever element came first - a silent, order-dependent
        // choice of verifier.
        let error = AuthProvider::new(vec![
            ConfiguredProvider::Platform(platform_provider(PLATFORM_ISSUER)),
            ConfiguredProvider::Supabase(supabase_provider(PLATFORM_ISSUER)),
        ])
        .expect_err("a shared issuer must be refused");
        assert_eq!(
            error,
            AuthProviderSetError::DuplicateIssuer(PLATFORM_ISSUER.to_owned())
        );

        // The one-variable control: the SAME two backends, one issuer changed.
        AuthProvider::new(vec![
            ConfiguredProvider::Platform(platform_provider(PLATFORM_ISSUER)),
            ConfiguredProvider::Supabase(supabase_provider(GOTRUE_ISSUER)),
        ])
        .expect("distinct issuers are accepted");
    }

    #[compio::test]
    async fn dispatch_refuses_a_token_no_configured_provider_claims() {
        let set = AuthProvider::new(vec![
            ConfiguredProvider::Platform(platform_provider(PLATFORM_ISSUER)),
            ConfiguredProvider::Supabase(supabase_provider(GOTRUE_ISSUER)),
        ])
        .expect("two distinct issuers");

        let error = set
            .verify_token(&token_with_issuer("https://attacker.test"))
            .await
            .expect_err("an unclaimed issuer must not reach any provider");
        assert!(
            matches!(error, VerifyTokenError::UnknownIssuer(ref iss) if iss == "https://attacker.test"),
            "unexpected error: {error:?}"
        );

        let error = set
            .verify_token("opaque-not-a-jws")
            .await
            .expect_err("a non-JWS bearer has no issuer to route on");
        assert!(
            matches!(error, VerifyTokenError::MissingIssuer),
            "unexpected error: {error:?}"
        );
    }

    #[compio::test]
    async fn a_one_provider_set_also_routes_by_issuer() {
        // BEHAVIOUR CHANGE, stated deliberately. The old single-provider arms
        // handed every bearer straight to their one provider, so a foreign
        // `iss` came back as that provider's own failure. Routing is now
        // uniform, so it comes back as UnknownIssuer and never reaches the
        // provider - one fewer JWKS fetch, and an accurate diagnostic.
        let set = AuthProvider::supabase(supabase_provider(GOTRUE_ISSUER));
        let error = set
            .verify_token(&token_with_issuer(PLATFORM_ISSUER))
            .await
            .expect_err("a foreign issuer must not reach the one provider");
        assert!(
            matches!(error, VerifyTokenError::UnknownIssuer(ref iss) if iss == PLATFORM_ISSUER),
            "unexpected error: {error:?}"
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
