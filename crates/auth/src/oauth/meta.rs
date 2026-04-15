//! Meta (Facebook) OAuth2 provider.
//!
//! Flow:
//! 1. Redirect user to `facebook.com/v19.0/dialog/oauth`
//! 2. Exchange authorization code at `graph.facebook.com/v19.0/oauth/access_token` (GET)
//! 3. Fetch user profile from `graph.facebook.com/v19.0/me`
//!
//! Facebook quirk: token exchange uses GET with query parameters, not POST.

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
// Provider
// ---------------------------------------------------------------------------

/// Meta (Facebook) OAuth2 provider.
#[derive(Debug, Clone)]
pub struct MetaProvider {
    config: OAuthConfig,
}

impl MetaProvider {
    /// Create a new Meta (Facebook) OAuth provider with the given configuration.
    pub fn new(config: OAuthConfig) -> Self {
        Self { config }
    }
}

impl OAuthProvider for MetaProvider {
    fn name(&self) -> &str {
        "meta"
    }

    fn authorize_url(&self, state: &str) -> String {
        format!(
            "https://www.facebook.com/v19.0/dialog/oauth\
             ?client_id={client_id}\
             &redirect_uri={redirect_uri}\
             &scope=email,public_profile\
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

    // --- Step 1: Exchange code for access token (Facebook uses GET) ---
    let token_url = format!(
        "https://graph.facebook.com/v19.0/oauth/access_token\
         ?client_id={client_id}\
         &client_secret={client_secret}\
         &redirect_uri={redirect_uri}\
         &code={code}",
        client_id = urlencod(&config.client_id),
        client_secret = urlencod(&config.client_secret),
        redirect_uri = urlencod(&config.redirect_uri),
        code = urlencod(code),
    );

    let resp = client
        .get(&token_url)
        .map_err(|e| format!("meta: build token request: {e}"))?
        .send()
        .await
        .map_err(|e| format!("meta: token exchange: {e}"))?;

    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("meta: token exchange failed (non-2xx): {body}"));
    }

    let token: TokenResponse = resp
        .json()
        .await
        .map_err(|e| format!("meta: parse token response: {e}"))?;

    // --- Step 2: Fetch user profile ---
    let userinfo_url = format!(
        "https://graph.facebook.com/v19.0/me\
         ?fields=id,email,name,picture.type(large)\
         &access_token={access_token}",
        access_token = urlencod(&token.access_token),
    );

    let resp = client
        .get(&userinfo_url)
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

    let avatar_url = user
        .picture
        .and_then(|p| p.data)
        .and_then(|d| d.url);

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
