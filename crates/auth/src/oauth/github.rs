//! GitHub OAuth2 provider.
//!
//! Flow:
//! 1. Redirect user to `github.com/login/oauth/authorize`
//! 2. Exchange authorization code at `github.com/login/oauth/access_token`
//! 3. Fetch user profile from `api.github.com/user`
//! 4. If email is null, fetch primary verified email from `api.github.com/user/emails`

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
// Provider
// ---------------------------------------------------------------------------

/// GitHub OAuth2 provider.
#[derive(Debug, Clone)]
pub struct GitHubProvider {
    config: OAuthConfig,
}

impl GitHubProvider {
    /// Create a new GitHub OAuth provider with the given configuration.
    pub fn new(config: OAuthConfig) -> Self {
        Self { config }
    }
}

impl OAuthProvider for GitHubProvider {
    fn name(&self) -> &str {
        "github"
    }

    fn authorize_url(&self, state: &str) -> String {
        format!(
            "https://github.com/login/oauth/authorize\
             ?client_id={client_id}\
             &redirect_uri={redirect_uri}\
             &scope=user:email\
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

    let resp = client
        .post("https://github.com/login/oauth/access_token")
        .map_err(|e| format!("github: build token request: {e}"))?
        .header("Accept", "application/json")
        .map_err(|e| format!("github: set accept header: {e}"))?
        .form(&form)
        .map_err(|e| format!("github: encode form: {e}"))?
        .send()
        .await
        .map_err(|e| format!("github: token exchange: {e}"))?;

    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("github: token exchange failed: {body}"));
    }

    let token: TokenResponse = resp
        .json()
        .await
        .map_err(|e| format!("github: parse token response: {e}"))?;

    // --- Step 2: Fetch user profile ---
    let resp = client
        .get("https://api.github.com/user")
        .map_err(|e| format!("github: build user request: {e}"))?
        .bearer_auth(&token.access_token)
        .map_err(|e| format!("github: set bearer auth: {e}"))?
        .header("User-Agent", "zeroship")
        .map_err(|e| format!("github: set user-agent: {e}"))?
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

    // --- Step 3: Resolve email ---
    // GitHub may not include email in /user if the user's email is private.
    // Fall back to /user/emails to find the primary verified email.
    let email = match user.email {
        Some(ref e) if !e.is_empty() => e.clone(),
        _ => fetch_primary_email(&token.access_token).await?,
    };

    Ok(OAuthProfile {
        email,
        name: user.name.unwrap_or(user.login),
        avatar_url: user.avatar_url,
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Fetch the primary verified email from `GET /user/emails`.
async fn fetch_primary_email(access_token: &str) -> Result<String, String> {
    let client = http_client();

    let resp = client
        .get("https://api.github.com/user/emails")
        .map_err(|e| format!("github: build emails request: {e}"))?
        .bearer_auth(access_token)
        .map_err(|e| format!("github: set bearer auth (emails): {e}"))?
        .header("User-Agent", "zeroship")
        .map_err(|e| format!("github: set user-agent (emails): {e}"))?
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
