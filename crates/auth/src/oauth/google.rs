//! Google OAuth2 provider — OIDC with discovery.
//!
//! Uses OpenID Connect Discovery at `https://accounts.google.com` to
//! automatically configure endpoints and JWKS. User profile is extracted
//! from the `id_token` claims (email, name, picture).

use openidconnect::core::CoreProviderMetadata;
use openidconnect::{ClientId, ClientSecret, IssuerUrl, RedirectUrl};

use super::{OAuthConfig, OAuthCoreClient, Provider, ProviderKind, http_client_fn};

/// Google's OIDC issuer URL.
const ISSUER_URL: &str = "https://accounts.google.com";

/// Create a Google OIDC provider via discovery.
///
/// This is async because it fetches the provider metadata (JWKS, endpoints)
/// from Google's discovery document at startup.
pub async fn build(config: OAuthConfig) -> Result<Provider, String> {
    let issuer = IssuerUrl::new(ISSUER_URL.to_string())
        .map_err(|e| format!("google: invalid issuer URL: {e}"))?;

    let metadata = CoreProviderMetadata::discover_async(issuer, &http_client_fn)
        .await
        .map_err(|e| format!("google: OIDC discovery failed: {e}"))?;

    let client = OAuthCoreClient::from_provider_metadata(
        metadata,
        ClientId::new(config.client_id),
        Some(ClientSecret::new(config.client_secret)),
    )
    .set_redirect_uri(
        RedirectUrl::new(config.redirect_uri)
            .map_err(|e| format!("google: invalid redirect URI: {e}"))?,
    );

    Ok(Provider {
        name: "google".to_string(),
        client,
        kind: ProviderKind::Oidc,
        scopes: vec!["email".to_string(), "profile".to_string()],
        profile_fetcher: None,
    })
}
