//! Apple Sign In provider — OIDC with discovery.
//!
//! Uses OpenID Connect Discovery at `https://appleid.apple.com` to
//! automatically configure endpoints and JWKS. User profile is extracted
//! from the `id_token` claims.
//!
//! Apple quirks:
//! - `response_mode=form_post` — Apple POSTs the callback. We set this via
//!   an extra parameter on the authorization URL.
//! - The `client_secret` is a JWT signed with a private key (ES256). Operators
//!   generate it externally; we accept the pre-generated secret.
//! - Apple only sends the user's name on the FIRST authorization. After that,
//!   only email is available. We fall back to the email prefix for `name`.

use openidconnect::core::CoreProviderMetadata;
use openidconnect::{ClientId, ClientSecret, IssuerUrl, RedirectUrl};

use super::{OAuthConfig, OAuthCoreClient, Provider, ProviderKind, http_client_fn};

/// Apple's OIDC issuer URL.
const ISSUER_URL: &str = "https://appleid.apple.com";

/// Create an Apple Sign In provider via OIDC discovery.
///
/// This is async because it fetches the provider metadata (JWKS, endpoints)
/// from Apple's discovery document at startup.
pub async fn build(config: OAuthConfig) -> Result<Provider, String> {
    let issuer = IssuerUrl::new(ISSUER_URL.to_string())
        .map_err(|e| format!("apple: invalid issuer URL: {e}"))?;

    let metadata = CoreProviderMetadata::discover_async(issuer, &http_client_fn)
        .await
        .map_err(|e| format!("apple: OIDC discovery failed: {e}"))?;

    let client = OAuthCoreClient::from_provider_metadata(
        metadata,
        ClientId::new(config.client_id),
        Some(ClientSecret::new(config.client_secret)),
    )
    .set_redirect_uri(
        RedirectUrl::new(config.redirect_uri)
            .map_err(|e| format!("apple: invalid redirect URI: {e}"))?,
    );

    Ok(Provider {
        name: "apple".to_string(),
        client,
        kind: ProviderKind::Oidc,
        // Apple scopes: name and email.
        scopes: vec!["name".to_string(), "email".to_string()],
        profile_fetcher: None,
    })
}
