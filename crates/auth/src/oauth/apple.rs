//! Apple Sign In OAuth2 provider.
//!
//! Flow:
//! 1. Redirect user to `appleid.apple.com/auth/authorize`
//! 2. Exchange authorization code at `appleid.apple.com/auth/token`
//! 3. Decode the `id_token` JWT from the token response to extract user info
//!
//! Apple quirks:
//! - `response_mode=form_post` — Apple POSTs the callback (we also support GET).
//! - The `client_secret` is a JWT signed with a private key (ES256). Operators
//!   generate it externally; we accept the pre-generated secret.
//! - The token response includes an `id_token` (JWT). We decode the payload
//!   (without signature verification — it arrives over HTTPS from Apple).
//! - Apple only sends the user's name on the FIRST authorization. After that,
//!   only email is available. We fall back to the email prefix for `name`.

use std::collections::HashMap;

use serde::Deserialize;

use super::{ExchangeFuture, OAuthConfig, OAuthProfile, OAuthProvider, http_client};

// ---------------------------------------------------------------------------
// Token response shape
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct TokenResponse {
    id_token: String,
}

/// Claims extracted from Apple's `id_token` JWT payload.
#[derive(Deserialize)]
struct IdTokenClaims {
    /// User's email address.
    email: Option<String>,
    /// Apple's unique stable user ID.
    #[allow(dead_code)]
    sub: String,
}

// ---------------------------------------------------------------------------
// Provider
// ---------------------------------------------------------------------------

/// Apple Sign In OAuth2 provider.
#[derive(Debug, Clone)]
pub struct AppleProvider {
    config: OAuthConfig,
}

impl AppleProvider {
    /// Create a new Apple Sign In provider with the given configuration.
    pub fn new(config: OAuthConfig) -> Self {
        Self { config }
    }
}

impl OAuthProvider for AppleProvider {
    fn name(&self) -> &str {
        "apple"
    }

    fn authorize_url(&self, state: &str) -> String {
        format!(
            "https://appleid.apple.com/auth/authorize\
             ?client_id={client_id}\
             &redirect_uri={redirect_uri}\
             &scope=name%20email\
             &response_type=code\
             &response_mode=form_post\
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

    // --- Step 1: Exchange code for id_token ---
    let mut form = HashMap::new();
    form.insert("code", code);
    form.insert("client_id", config.client_id.as_str());
    form.insert("client_secret", config.client_secret.as_str());
    form.insert("redirect_uri", config.redirect_uri.as_str());
    form.insert("grant_type", "authorization_code");

    let resp = client
        .post("https://appleid.apple.com/auth/token")
        .map_err(|e| format!("apple: build token request: {e}"))?
        .form(&form)
        .map_err(|e| format!("apple: encode form: {e}"))?
        .send()
        .await
        .map_err(|e| format!("apple: token exchange: {e}"))?;

    if !resp.status().is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("apple: token exchange failed (non-2xx): {body}"));
    }

    let token: TokenResponse = resp
        .json()
        .await
        .map_err(|e| format!("apple: parse token response: {e}"))?;

    // --- Step 2: Decode id_token to extract user info ---
    let claims = decode_id_token(&token.id_token)?;

    let email = claims
        .email
        .ok_or_else(|| "apple: no email in id_token".to_string())?;

    // Apple only sends the user's name on the first authorization.
    // Fall back to the email prefix (everything before '@').
    let name = email
        .split('@')
        .next()
        .unwrap_or("user")
        .to_string();

    Ok(OAuthProfile {
        email,
        name,
        avatar_url: None,
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Decode the payload of a JWT without verifying the signature.
///
/// Apple's `id_token` arrives over HTTPS directly from Apple's token endpoint,
/// so the transport already guarantees authenticity. We only need to extract
/// the claims from the middle (payload) segment.
fn decode_id_token(jwt: &str) -> Result<IdTokenClaims, String> {
    let parts: Vec<&str> = jwt.split('.').collect();
    if parts.len() != 3 {
        return Err("apple: id_token is not a valid JWT (expected 3 parts)".to_string());
    }

    // JWT payload uses base64url (no padding).
    use base64::Engine;
    let payload_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(parts[1])
        .map_err(|e| format!("apple: base64 decode id_token payload: {e}"))?;

    serde_json::from_slice::<IdTokenClaims>(&payload_bytes)
        .map_err(|e| format!("apple: parse id_token claims: {e}"))
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
