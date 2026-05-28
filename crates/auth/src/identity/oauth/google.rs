//! Google OIDC federation client.
//!
//! Flow:
//!   1. `start_authorize_url(cfg)` — random state, nonce, PKCE pair.
//!   2. `complete_callback(cfg, code, verifier, expected_nonce, jwks)`
//!      — token exchange, ID-token verify (via `core::oidc_verify`
//!      against Google's JWKS), return [`GoogleIdentity`].
//!
//! Cookies are NOT this module's job — the HTTP handler
//! (`ui::oauth_google`) stashes state/verifier/nonce in a signed cookie
//! before redirecting to Google, and re-reads it on the callback.

use serde::{Deserialize, Serialize};
use zeroship_core::oidc_verify::{verify_id_token, JwksCache, TokenClaims};
use zeroship_core::pkce::{generate_verifier, s256_challenge};

use crate::config::AuthConfig;
use crate::error::{AuthError, Result};

/// Subset of Google's `/token` response. The access token is consumed
/// only for OIDC `at_hash` verification when Google includes that claim.
#[derive(Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    id_token: String,
}

/// The PKCE+state material the authorize step generated.
///
/// The HTTP handler stashes [`state`](Self::state),
/// [`verifier`](Self::verifier), and [`nonce`](Self::nonce) in a signed
/// cookie before redirecting the browser to [`url`](Self::url).
#[derive(Debug)]
pub struct AuthorizeStart {
    pub url: String,
    pub state: String,
    pub verifier: String,
    pub nonce: String,
}

/// Normalised Google profile, ready for the linker.
///
/// Captures the OIDC standard claims plus Google's Workspace `hd`
/// (hosted-domain) extension — the latter is what marks an account as
/// Workspace-managed (vs. consumer `@gmail.com`), which the
/// trust-for-email policy uses to decide whether auto-create on a
/// brand-new email is allowed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoogleIdentity {
    /// Google's `sub` claim — the stable opaque user id per Google account.
    pub subject: String,
    pub email: String,
    pub email_verified: bool,
    pub name: Option<String>,
    pub picture: Option<String>,
    /// Workspace hosted-domain claim. `None` for consumer accounts.
    pub hd: Option<String>,
}

/// Build the `accounts.google.com` authorize URL plus the random material
/// the callback needs to verify the response.
///
/// # Errors
///
/// [`AuthError::Config`] if `google_client_id` is not configured.
pub fn start_authorize_url(cfg: &AuthConfig) -> Result<AuthorizeStart> {
    let client_id = cfg
        .google_client_id
        .as_deref()
        .ok_or_else(|| AuthError::Config("google_client_id missing".into()))?;

    let verifier = generate_verifier();
    let challenge = s256_challenge(&verifier);
    // `generate_verifier` is just "32 random bytes → base64url"; reusing it
    // for state + nonce gives 256 bits of entropy each.
    let state = generate_verifier();
    let nonce = generate_verifier();

    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("response_type", "code")
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", &cfg.google_redirect_uri)
        .append_pair("scope", "openid email profile")
        .append_pair("access_type", "online")
        .append_pair("include_granted_scopes", "true")
        .append_pair("state", &state)
        .append_pair("nonce", &nonce)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .finish();
    let url = format!("{}?{query}", cfg.google_auth_url);

    Ok(AuthorizeStart {
        url,
        state,
        verifier,
        nonce,
    })
}

/// Exchange the authorization code, then verify the ID token via Google's JWKS.
///
/// # Errors
///
/// [`AuthError::Config`] if Google credentials are missing,
/// [`AuthError::Internal`] on HTTP / JSON / verify failure.
pub async fn complete_callback(
    cfg: &AuthConfig,
    code: &str,
    verifier: &str,
    expected_nonce: &str,
    jwks: &JwksCache,
) -> Result<GoogleIdentity> {
    let client_id = cfg
        .google_client_id
        .as_deref()
        .ok_or_else(|| AuthError::Config("google_client_id missing".into()))?;
    let client_secret = cfg
        .google_client_secret
        .as_deref()
        .ok_or_else(|| AuthError::Config("google_client_secret missing".into()))?;

    // 1. POST /token (form-encoded).
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("grant_type", "authorization_code")
        .append_pair("code", code)
        .append_pair("client_id", client_id)
        .append_pair("client_secret", client_secret)
        .append_pair("redirect_uri", &cfg.google_redirect_uri)
        .append_pair("code_verifier", verifier)
        .finish();

    let client = cyper::Client::new();
    let resp = client
        .request(http::Method::POST, cfg.google_token_url.as_str())
        .map_err(|e| AuthError::Internal(format!("google token build: {e}")))?
        .header("content-type", "application/x-www-form-urlencoded")
        .map_err(|e| AuthError::Internal(format!("google token header: {e}")))?
        .body(body.into_bytes())
        .send()
        .await
        .map_err(|e| AuthError::Internal(format!("google token send: {e}")))?;

    let status = resp.status().as_u16();
    let body_text = resp
        .text()
        .await
        .map_err(|e| AuthError::Internal(format!("google token read: {e}")))?;
    if !(200..300).contains(&status) {
        return Err(AuthError::Internal(format!(
            "google token → {status}: {body_text}"
        )));
    }

    let tr: TokenResponse = serde_json::from_str(&body_text)
        .map_err(|e| AuthError::Internal(format!("google token parse: {e}")))?;

    // 2. Verify the ID token using core::oidc_verify against Google's JWKS.
    //    The `audience` we expect is our client_id; the issuer is Google's
    //    canonical value; the nonce is whatever the start step stashed.
    let claims: TokenClaims = verify_id_token(
        jwks,
        &tr.id_token,
        cfg.google_issuer.as_str(),
        client_id,
        Some(expected_nonce),
        tr.access_token.as_deref(),
        Some(code),
    )
    .await
    .map_err(|e| AuthError::Internal(format!("google id_token verify: {e}")))?;

    // 3. Extract Google-specific fields. `hd` (Workspace hosted-domain)
    //    is not one of the named TokenClaims fields, so it lands in `other`.
    let hd = claims
        .other
        .get("hd")
        .and_then(|v| v.as_str())
        .map(String::from);

    Ok(GoogleIdentity {
        subject: claims.sub,
        email: claims
            .email
            .ok_or_else(|| AuthError::Internal("google id_token missing email".into()))?,
        email_verified: claims.email_verified.unwrap_or(false),
        name: claims.name,
        picture: claims.picture,
        hd,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// Build a baseline `AuthConfig` with no env / file deps, then layer
    /// Google credentials on top. `clap::Parser::parse_from(&["bin"])` runs
    /// the same path as production startup, filling defaults for every
    /// optional field — so this test breaks loudly if a new required arg
    /// gets added without env defaults.
    fn cfg_with_google() -> AuthConfig {
        let mut cfg = AuthConfig::parse_from(["zeroship-auth", "--db-url", "postgres://x/y"]);
        cfg.google_client_id = Some("test-client".into());
        cfg.google_client_secret = Some("test-secret".into());
        cfg.google_redirect_uri = "https://auth.zeroship.ai/oauth/google/callback".into();
        cfg
    }

    #[test]
    fn start_authorize_url_well_formed() {
        let cfg = cfg_with_google();
        let start = start_authorize_url(&cfg).expect("start");
        assert!(
            start.url.starts_with(cfg.google_auth_url.as_str()),
            "url base: {}",
            start.url
        );
        assert!(start.url.contains("client_id=test-client"));
        assert!(start.url.contains("response_type=code"));
        assert!(start.url.contains("code_challenge_method=S256"));
        assert!(start.url.contains("code_challenge="));
        // `form_urlencoded` emits `+` for spaces.
        assert!(start.url.contains("scope=openid+email+profile"));
        assert!(start.url.contains(&format!("state={}", start.state)));
        assert!(start.url.contains(&format!("nonce={}", start.nonce)));
        // PKCE verifier is the raw 43-char base64url; the URL only carries
        // the challenge, never the verifier.
        assert!(!start.url.contains(&start.verifier));
        // redirect_uri is URL-encoded (`/` → `%2F`, `:` → `%3A`).
        assert!(start
            .url
            .contains("redirect_uri=https%3A%2F%2Fauth.zeroship.ai%2Foauth%2Fgoogle%2Fcallback"));
        // 32-byte CSPRNG → 43-char base64url-no-pad.
        assert_eq!(start.verifier.len(), 43);
        assert_eq!(start.state.len(), 43);
        assert_eq!(start.nonce.len(), 43);
        // state and nonce must be distinct random values.
        assert_ne!(start.state, start.nonce);
        assert_ne!(start.state, start.verifier);
    }

    #[test]
    fn start_authorize_url_errors_when_client_id_missing() {
        let mut cfg = AuthConfig::parse_from(["zeroship-auth", "--db-url", "postgres://x/y"]);
        cfg.google_client_id = None;
        let err = start_authorize_url(&cfg).expect_err("must fail without client_id");
        assert!(matches!(err, AuthError::Config(_)), "got: {err:?}");
    }
}
