//! GitHub OAuth2 provider — plain OAuth2 (not OIDC).
//!
//! Flow:
//! 1. Redirect user to `github.com/login/oauth/authorize`
//! 2. Exchange authorization code at `github.com/login/oauth/access_token`
//! 3. Fetch user profile from `api.github.com/user`
//! 4. If email is null, fetch primary verified email from `api.github.com/user/emails`
//!
//! GitHub does not support OIDC, so endpoints are configured manually via
//! a synthetic `CoreProviderMetadata` and the user profile is fetched via
//! GitHub's REST API using the access token.

use openidconnect::{ClientId, ClientSecret, RedirectUrl};
use serde::Deserialize;

use super::{
    OAuthConfig, OAuthCoreClient, OAuthProfile, Provider, ProviderKind, cyper_client,
    manual_provider_metadata,
};

/// GitHub's authorization endpoint.
const AUTH_URL: &str = "https://github.com/login/oauth/authorize";
/// GitHub's token endpoint.
const TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
/// GitHub's user API endpoint.
const USERINFO_URL: &str = "https://api.github.com/user";

// ---------------------------------------------------------------------------
// GitHub API response shapes
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct UserResponse {
    email: Option<String>,
    name: Option<String>,
    login: String,
    avatar_url: Option<String>,
}

#[derive(Deserialize)]
struct EmailEntry {
    email: String,
    primary: bool,
    verified: bool,
}

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

/// Create a GitHub OAuth2 provider with manually configured endpoints.
///
/// This is async for signature consistency with OIDC providers, but does not
/// perform any network calls.
pub async fn build(config: OAuthConfig) -> Result<Provider, String> {
    let metadata = manual_provider_metadata("https://github.com", AUTH_URL, TOKEN_URL)?;

    let client = OAuthCoreClient::from_provider_metadata(
        metadata,
        ClientId::new(config.client_id),
        Some(ClientSecret::new(config.client_secret)),
    )
    .set_redirect_uri(
        RedirectUrl::new(config.redirect_uri)
            .map_err(|e| format!("github: invalid redirect URI: {e}"))?,
    )
    .disable_openid_scope();

    Ok(Provider {
        name: "github".to_string(),
        client,
        kind: ProviderKind::OAuth2,
        scopes: vec!["user:email".to_string()],
        profile_fetcher: Some(fetch_profile),
    })
}

// ---------------------------------------------------------------------------
// Profile fetcher
// ---------------------------------------------------------------------------

/// Fetch user profile from GitHub's REST API using an access token.
fn fetch_profile(
    access_token: &str,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<OAuthProfile, String>> + Send + '_>>
{
    Box::pin(fetch_profile_impl(access_token))
}

async fn fetch_profile_impl(access_token: &str) -> Result<OAuthProfile, String> {
    let client = cyper_client();

    // --- Fetch user profile ---
    let resp = client
        .get(USERINFO_URL)
        .map_err(|e| format!("github: build user request: {e}"))?
        .bearer_auth(access_token)
        .map_err(|e| format!("github: set bearer auth: {e}"))?
        .header("User-Agent", "zeroship")
        .map_err(|e| format!("github: set user-agent: {e}"))?
        .header("Accept", "application/json")
        .map_err(|e| format!("github: set accept: {e}"))?
        .send()
        .await
        .map_err(|e| format!("github: user request: {e}"))?;

    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("github: user request failed: {body}"));
    }

    let user: UserResponse = resp
        .json()
        .await
        .map_err(|e| format!("github: parse user response: {e}"))?;

    // --- Resolve email ---
    // GitHub may not include email in /user if the user's email is private.
    // Fall back to /user/emails to find the primary verified email.
    let email = match user.email {
        Some(ref e) if !e.is_empty() => e.clone(),
        _ => fetch_primary_email(access_token).await?,
    };

    Ok(OAuthProfile {
        email,
        name: user.name.unwrap_or(user.login),
        avatar_url: user.avatar_url,
    })
}

/// Fetch the primary verified email from `GET /user/emails`.
async fn fetch_primary_email(access_token: &str) -> Result<String, String> {
    let client = cyper_client();

    let resp = client
        .get("https://api.github.com/user/emails")
        .map_err(|e| format!("github: build emails request: {e}"))?
        .bearer_auth(access_token)
        .map_err(|e| format!("github: set bearer auth (emails): {e}"))?
        .header("User-Agent", "zeroship")
        .map_err(|e| format!("github: set user-agent (emails): {e}"))?
        .header("Accept", "application/json")
        .map_err(|e| format!("github: set accept (emails): {e}"))?
        .send()
        .await
        .map_err(|e| format!("github: emails request: {e}"))?;

    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("github: emails request failed: {body}"));
    }

    let emails: Vec<EmailEntry> = resp
        .json()
        .await
        .map_err(|e| format!("github: parse emails response: {e}"))?;

    // Prefer the primary + verified email.
    emails
        .iter()
        .find(|e| e.primary && e.verified)
        .or_else(|| emails.iter().find(|e| e.verified))
        .map(|e| e.email.clone())
        .ok_or_else(|| "github: no verified email found".to_string())
}
