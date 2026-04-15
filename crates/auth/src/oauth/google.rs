//! Google OAuth2 provider.
//!
//! Flow:
//! 1. Redirect user to `accounts.google.com/o/oauth2/v2/auth`
//! 2. Exchange authorization code at `oauth2.googleapis.com/token`
//! 3. Fetch user profile from `googleapis.com/oauth2/v3/userinfo`

use std::collections::HashMap;

use serde::Deserialize;

use super::{ExchangeFuture, OAuthConfig, OAuthProfile, OAuthProvider, http_client};

// ---------------------------------------------------------------------------
// Token + userinfo response shapes
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
}

#[derive(Deserialize)]
struct UserInfoResponse {
    email: String,
    name: Option<String>,
    picture: Option<String>,
}

// ---------------------------------------------------------------------------
// Provider
// ---------------------------------------------------------------------------

/// Google OAuth2 provider.
#[derive(Debug, Clone)]
pub struct GoogleProvider {
    config: OAuthConfig,
}

impl GoogleProvider {
    /// Create a new Google OAuth provider with the given configuration.
    pub fn new(config: OAuthConfig) -> Self {
        Self { config }
    }
}

impl OAuthProvider for GoogleProvider {
    fn name(&self) -> &str {
        "google"
    }

    fn authorize_url(&self, state: &str) -> String {
        format!(
            "https://accounts.google.com/o/oauth2/v2/auth\
             ?client_id={client_id}\
             &redirect_uri={redirect_uri}\
             &scope=openid%20email%20profile\
             &response_type=code\
             &state={state}",
            client_id = urlencod(&self.config.client_id),
            redirect_uri = urlencod(&self.config.redirect_uri),
            state = urlencod(state),
        )
    }

    fn exchange(&self, code: &str) -> ExchangeFuture<'_> {
        let code = code.to_string();
        Box::pin(async move { exchange_impl(&self.config, &code).await })
    }
}

/// The actual async exchange logic, extracted so the trait method can return
/// a boxed future.
async fn exchange_impl(config: &OAuthConfig, code: &str) -> Result<OAuthProfile, String> {
    let client = http_client();

    // --- Step 1: Exchange code for access token ---
    let mut form = HashMap::new();
    form.insert("code", code);
    form.insert("client_id", config.client_id.as_str());
    form.insert("client_secret", config.client_secret.as_str());
    form.insert("redirect_uri", config.redirect_uri.as_str());
    form.insert("grant_type", "authorization_code");

    let resp = client
        .post("https://oauth2.googleapis.com/token")
        .map_err(|e| format!("google: build token request: {e}"))?
        .form(&form)
        .map_err(|e| format!("google: encode form: {e}"))?
        .send()
        .await
        .map_err(|e| format!("google: token exchange: {e}"))?;

    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("google: token exchange failed (non-2xx): {body}"));
    }

    let token: TokenResponse = resp
        .json()
        .await
        .map_err(|e| format!("google: parse token response: {e}"))?;

    // --- Step 2: Fetch user profile ---
    let resp = client
        .get("https://www.googleapis.com/oauth2/v3/userinfo")
        .map_err(|e| format!("google: build userinfo request: {e}"))?
        .bearer_auth(&token.access_token)
        .map_err(|e| format!("google: set bearer auth: {e}"))?
        .send()
        .await
        .map_err(|e| format!("google: userinfo request: {e}"))?;

    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("google: userinfo failed: {body}"));
    }

    let info: UserInfoResponse = resp
        .json()
        .await
        .map_err(|e| format!("google: parse userinfo: {e}"))?;

    Ok(OAuthProfile {
        email: info.email,
        name: info.name.unwrap_or_default(),
        avatar_url: info.picture,
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
