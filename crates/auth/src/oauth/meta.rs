//! Meta (Facebook) OAuth2 provider — plain OAuth2 (not OIDC).
//!
//! Flow:
//! 1. Redirect user to `facebook.com/v19.0/dialog/oauth`
//! 2. Exchange authorization code at `graph.facebook.com/v19.0/oauth/access_token`
//! 3. Fetch user profile from `graph.facebook.com/v19.0/me`
//!
//! Meta does not support standard OIDC, so endpoints are configured manually
//! via a synthetic `CoreProviderMetadata` and the user profile is fetched via
//! the Graph API using the access token.

use openidconnect::{ClientId, ClientSecret, RedirectUrl};
use serde::Deserialize;

use super::{
    OAuthConfig, OAuthCoreClient, OAuthProfile, Provider, ProviderKind, cyper_client,
    manual_provider_metadata,
};

/// Meta's authorization endpoint.
const AUTH_URL: &str = "https://www.facebook.com/v19.0/dialog/oauth";
/// Meta's token endpoint.
const TOKEN_URL: &str = "https://graph.facebook.com/v19.0/oauth/access_token";
/// Meta's Graph API user endpoint.
const USERINFO_URL: &str =
    "https://graph.facebook.com/v19.0/me?fields=id,email,name,picture.type(large)";

// ---------------------------------------------------------------------------
// Graph API response shapes
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct UserResponse {
    email: Option<String>,
    name: Option<String>,
    #[allow(dead_code)]
    id: String,
    picture: Option<PictureWrapper>,
}

#[derive(Deserialize)]
struct PictureWrapper {
    data: Option<PictureData>,
}

#[derive(Deserialize)]
struct PictureData {
    url: Option<String>,
}

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

/// Create a Meta (Facebook) OAuth2 provider with manually configured endpoints.
///
/// This is async for signature consistency with OIDC providers, but does not
/// perform any network calls.
pub async fn build(config: OAuthConfig) -> Result<Provider, String> {
    let metadata = manual_provider_metadata("https://facebook.com", AUTH_URL, TOKEN_URL)?;

    let client = OAuthCoreClient::from_provider_metadata(
        metadata,
        ClientId::new(config.client_id),
        Some(ClientSecret::new(config.client_secret)),
    )
    .set_redirect_uri(
        RedirectUrl::new(config.redirect_uri)
            .map_err(|e| format!("meta: invalid redirect URI: {e}"))?,
    )
    .disable_openid_scope();

    Ok(Provider {
        name: "meta".to_string(),
        client,
        kind: ProviderKind::OAuth2,
        scopes: vec!["email".to_string(), "public_profile".to_string()],
        profile_fetcher: Some(fetch_profile),
    })
}

// ---------------------------------------------------------------------------
// Profile fetcher
// ---------------------------------------------------------------------------

/// Fetch user profile from Meta's Graph API using an access token.
fn fetch_profile(
    access_token: &str,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<OAuthProfile, String>> + Send + '_>>
{
    Box::pin(fetch_profile_impl(access_token))
}

async fn fetch_profile_impl(access_token: &str) -> Result<OAuthProfile, String> {
    let client = cyper_client();

    // Graph API — pass access_token as a query parameter.
    let url = format!("{USERINFO_URL}&access_token={}", urlencod(access_token));

    let resp = client
        .get(&url)
        .map_err(|e| format!("meta: build userinfo request: {e}"))?
        .send()
        .await
        .map_err(|e| format!("meta: userinfo request: {e}"))?;

    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("meta: userinfo failed: {body}"));
    }

    let user: UserResponse = resp
        .json()
        .await
        .map_err(|e| format!("meta: parse userinfo: {e}"))?;

    let email = user
        .email
        .ok_or_else(|| "meta: no email in user profile".to_string())?;

    let avatar_url = user.picture.and_then(|p| p.data).and_then(|d| d.url);

    Ok(OAuthProfile {
        email,
        name: user.name.unwrap_or_default(),
        avatar_url,
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Minimal percent-encoding for URL query parameter values.
fn urlencod(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                out.push('%');
                out.push(char::from(HEX[(b >> 4) as usize]));
                out.push(char::from(HEX[(b & 0x0f) as usize]));
            }
        }
    }
    out
}

const HEX: &[u8; 16] = b"0123456789ABCDEF";
