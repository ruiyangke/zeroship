//! Google OAuth — minimal authorization-code-with-PKCE client.
//!
//! Hand-rolled in ~150 lines because the `openidconnect` crate's
//! async http client wants tokio reqwest and the rest of our stack
//! is compio-only. We talk to two Google endpoints with `cyper`
//! and verify identity via the userinfo endpoint (Google's TLS +
//! the access-token-secret protect us; we trust Google's response).
//!
//! Flow:
//!   1. start_authorize_url(cfg)         — random state, PKCE pair
//!   2. complete_callback(cfg, code, …)  — token exchange, userinfo
//!
//! Cookies hold the state and PKCE verifier across the redirect.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use rand::RngCore;
use serde::Deserialize;
use sha2::{Digest, Sha256};

const GOOGLE_AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const GOOGLE_USERINFO_URL: &str = "https://www.googleapis.com/oauth2/v2/userinfo";

#[derive(Clone, Debug)]
pub struct GoogleConfig {
    pub client_id: String,
    pub client_secret: String,
    /// Must EXACTLY match one of the redirect URIs registered in the
    /// Google Cloud OAuth consent screen / credentials page.
    pub redirect_uri: String,
}

#[derive(Debug)]
pub struct AuthorizeUrlResult {
    pub url: String,
    /// CSRF guard — emit as a signed cookie + `state` URL param.
    pub state: String,
    /// PKCE verifier — emit as a (separate) signed cookie. The
    /// authorize URL only contains the SHA-256 challenge derived
    /// from this.
    pub pkce_verifier: String,
}

#[derive(Debug, Clone)]
pub struct OAuthIdentity {
    pub subject: String,
    pub email: String,
    pub name: String,
    pub avatar_url: Option<String>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
}

#[derive(Deserialize)]
struct UserinfoResponse {
    id: String,
    email: String,
    name: Option<String>,
    given_name: Option<String>,
    picture: Option<String>,
}

/// Build the authorize URL + the values to remember across the redirect.
pub fn start_authorize_url(cfg: &GoogleConfig) -> AuthorizeUrlResult {
    let state = random_token(32);
    let verifier = random_token(64);
    let challenge = pkce_challenge(&verifier);

    // url::form_urlencoded for safety; google is strict about encoding.
    let url = format!(
        "{base}?{q}",
        base = GOOGLE_AUTH_URL,
        q = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("response_type", "code")
            .append_pair("client_id", &cfg.client_id)
            .append_pair("redirect_uri", &cfg.redirect_uri)
            .append_pair("scope", "openid email profile")
            .append_pair("access_type", "online")
            .append_pair("include_granted_scopes", "true")
            .append_pair("state", &state)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256")
            .finish(),
    );

    AuthorizeUrlResult { url, state, pkce_verifier: verifier }
}

/// Exchange the authorization code, then fetch userinfo. Caller MUST
/// have already verified `state == stored_state`.
pub async fn complete_callback(
    cfg: &GoogleConfig,
    code: &str,
    pkce_verifier: &str,
) -> Result<OAuthIdentity, String> {
    let client = cyper::Client::new();

    // 1. Token exchange — POST application/x-www-form-urlencoded.
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("code", code)
        .append_pair("client_id", &cfg.client_id)
        .append_pair("client_secret", &cfg.client_secret)
        .append_pair("redirect_uri", &cfg.redirect_uri)
        .append_pair("grant_type", "authorization_code")
        .append_pair("code_verifier", pkce_verifier)
        .finish();

    let token_req = client
        .request(http::Method::POST, GOOGLE_TOKEN_URL)
        .map_err(|e| format!("oauth: build token request: {e}"))?
        .header("content-type", "application/x-www-form-urlencoded")
        .map_err(|e| format!("oauth: token header: {e}"))?
        .body(body);

    let token_res = token_req.send().await
        .map_err(|e| format!("oauth: token POST: {e}"))?;

    if token_res.status().as_u16() != 200 {
        let s = token_res.status();
        let body_text = token_res.text().await.unwrap_or_else(|_| "<no body>".into());
        return Err(format!("oauth token exchange → {s}: {body_text}"));
    }

    let token_text = token_res.text().await
        .map_err(|e| format!("oauth: read token body: {e}"))?;
    let token: TokenResponse = serde_json::from_str(&token_text)
        .map_err(|e| format!("oauth: token parse: {e}"))?;

    // 2. Userinfo — GET with bearer access token.
    let info_req = client
        .request(http::Method::GET, GOOGLE_USERINFO_URL)
        .map_err(|e| format!("oauth: build userinfo: {e}"))?
        .header("authorization", &format!("Bearer {}", token.access_token))
        .map_err(|e| format!("oauth: userinfo header: {e}"))?;

    let info_res = info_req.send().await
        .map_err(|e| format!("oauth: userinfo GET: {e}"))?;

    if info_res.status().as_u16() != 200 {
        let s = info_res.status();
        let body_text = info_res.text().await.unwrap_or_else(|_| "<no body>".into());
        return Err(format!("oauth userinfo → {s}: {body_text}"));
    }

    let info_text = info_res.text().await
        .map_err(|e| format!("oauth: read userinfo: {e}"))?;
    let info: UserinfoResponse = serde_json::from_str(&info_text)
        .map_err(|e| format!("oauth: userinfo parse: {e}"))?;

    let name = info.name
        .or(info.given_name)
        .or_else(|| info.email.split('@').next().map(|s| s.to_string()))
        .unwrap_or_else(|| "creator".into());

    Ok(OAuthIdentity {
        subject: info.id,
        email: info.email,
        name,
        avatar_url: info.picture,
    })
}

// ─── helpers ────────────────────────────────────────────────────────

fn random_token(n_bytes: usize) -> String {
    let mut buf = vec![0u8; n_bytes];
    rand::thread_rng().fill_bytes(&mut buf);
    URL_SAFE_NO_PAD.encode(&buf)
}

fn pkce_challenge(verifier: &str) -> String {
    let mut h = Sha256::new();
    h.update(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(h.finalize())
}
