//! GitHub OAuth 2.0 federation client.
//!
//! GitHub is NOT OIDC — there's no ID token. We exchange the code for an
//! access token, then fetch `/user` and `/user/emails` to assemble the
//! user profile.
//!
//! Flow:
//!   1. [`start_authorize_url`] — random `state` + PKCE pair. (No `nonce`:
//!      OIDC-only construct, and there is no ID-token to bind it to.)
//!   2. [`complete_callback`] — exchange code for access token, then GET
//!      `/user` + `/user/emails` to assemble [`GitHubIdentity`].
//!
//! Email-picker policy: the chosen email must be `primary && verified` AND
//! not end with `@users.noreply.github.com`. The latter is GitHub's
//! pseudonymous "hide my real email" address — accepting it as a
//! canonical identity would (a) prevent reset/notification mail ever
//! being deliverable and (b) make the eventual "merge accounts by email"
//! flow unable to converge with the user's real address.
//!
//! Cookies are NOT this module's job — the HTTP handler
//! (`ui::oauth_github`) stashes state/verifier in a signed cookie before
//! redirecting to GitHub and re-reads it on the callback.

use serde::{Deserialize, Serialize};
use zeroship_core::pkce::{generate_verifier, s256_challenge};

use crate::config::AuthConfig;
use crate::error::{AuthError, Result};

/// `@users.noreply.github.com` addresses are GitHub's pseudonymous
/// "keep my real email private" mailbox — they don't deliver mail. We
/// refuse to use them as the canonical identity for a zeroship account.
const NOREPLY_DOMAIN: &str = "@users.noreply.github.com";

/// The PKCE+state material the authorize step generated.
///
/// The HTTP handler stashes [`state`](Self::state) and
/// [`verifier`](Self::verifier) in a signed cookie before redirecting the
/// browser to [`url`](Self::url). `nonce` is unused (GitHub is OAuth 2.0,
/// not OIDC), but the shared [`crate::ui::oauth_stash::OAuthStash`]
/// payload carries the field — handlers pass an empty string.
#[derive(Debug)]
pub struct AuthorizeStart {
    pub url: String,
    pub state: String,
    pub verifier: String,
}

/// Normalised GitHub profile, ready for the linker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitHubIdentity {
    /// GitHub's numeric `user.id` rendered as a base-10 string. Stable
    /// for the lifetime of the GitHub account (survives username changes).
    pub subject: String,
    pub login: String,
    /// Verified primary email, NOT `@users.noreply.github.com`. The
    /// picker in [`complete_callback`] guarantees this; if no such email
    /// exists the callback fails rather than fall back to noreply.
    pub email: String,
    pub name: Option<String>,
    pub avatar_url: Option<String>,
}

/// Build the GitHub authorize URL + the values to stash.
///
/// # Errors
///
/// [`AuthError::Config`] if `github_client_id` is not configured.
pub fn start_authorize_url(cfg: &AuthConfig) -> Result<AuthorizeStart> {
    let client_id = cfg
        .github_client_id
        .as_deref()
        .ok_or_else(|| AuthError::Config("github_client_id missing".into()))?;

    let verifier = generate_verifier();
    let challenge = s256_challenge(&verifier);
    let state = generate_verifier();

    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("response_type", "code")
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", &cfg.github_redirect_uri)
        .append_pair("scope", "read:user user:email")
        .append_pair("state", &state)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .finish();
    let url = format!("{}?{query}", cfg.github_authorize_url);

    Ok(AuthorizeStart {
        url,
        state,
        verifier,
    })
}

/// Exchange the authorization code, fetch `/user` + `/user/emails`,
/// assemble the [`GitHubIdentity`].
///
/// # Errors
///
/// - [`AuthError::Config`] if GitHub credentials are missing.
/// - [`AuthError::Internal`] on HTTP / JSON failure, missing
///   `user:email` scope, or no verified primary non-noreply email.
pub async fn complete_callback(
    cfg: &AuthConfig,
    code: &str,
    verifier: &str,
) -> Result<GitHubIdentity> {
    let client_id = cfg
        .github_client_id
        .as_deref()
        .ok_or_else(|| AuthError::Config("github_client_id missing".into()))?;
    let client_secret = cfg
        .github_client_secret
        .as_deref()
        .ok_or_else(|| AuthError::Config("github_client_secret missing".into()))?;

    let client = cyper::Client::new();

    // 1. POST /login/oauth/access_token (form-encoded). We set
    //    `Accept: application/json` so GitHub returns JSON instead of
    //    `application/x-www-form-urlencoded` — see "Things to be
    //    careful about" in P4-U3 brief; the JSON path is well-supported
    //    and stable.
    let token_body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("grant_type", "authorization_code")
        .append_pair("code", code)
        .append_pair("client_id", client_id)
        .append_pair("client_secret", client_secret)
        .append_pair("redirect_uri", &cfg.github_redirect_uri)
        .append_pair("code_verifier", verifier)
        .finish();

    let resp = client
        .request(http::Method::POST, cfg.github_token_url.as_str())
        .map_err(|e| AuthError::Internal(format!("github token build: {e}")))?
        .header("content-type", "application/x-www-form-urlencoded")
        .map_err(|e| AuthError::Internal(format!("github token header: {e}")))?
        .header("accept", "application/json")
        .map_err(|e| AuthError::Internal(format!("github token accept: {e}")))?
        .header("user-agent", "zeroship-auth/1")
        .map_err(|e| AuthError::Internal(format!("github token ua: {e}")))?
        .body(token_body.into_bytes())
        .send()
        .await
        .map_err(|e| AuthError::Internal(format!("github token send: {e}")))?;

    let status = resp.status().as_u16();
    let body = resp
        .text()
        .await
        .map_err(|e| AuthError::Internal(format!("github token read: {e}")))?;
    if !(200..300).contains(&status) {
        return Err(AuthError::Internal(format!(
            "github token → {status}: {body}"
        )));
    }

    let tr: TokenResponse = serde_json::from_str(&body)
        .map_err(|e| AuthError::Internal(format!("github token parse: {e}\nbody: {body}")))?;

    if !tr.scope.contains("user:email") {
        return Err(AuthError::Internal(format!(
            "github scope missing user:email (got: {})",
            tr.scope
        )));
    }

    // 2. GET /user
    let user = get_with_token::<UserResponse>(
        &client,
        cfg.github_user_url.as_str(),
        &tr.access_token,
    )
    .await?;

    // 3. GET /user/emails
    let emails: Vec<EmailEntry> = get_with_token(
        &client,
        cfg.github_emails_url.as_str(),
        &tr.access_token,
    )
    .await?;

    // 4. Pick primary + verified + not @users.noreply.github.com
    let chosen = emails
        .iter()
        .find(|e| e.primary && e.verified && !e.email.ends_with(NOREPLY_DOMAIN))
        .ok_or_else(|| {
            AuthError::Internal(
                "GitHub: no primary + verified email (excluding @users.noreply.github.com); \
                 user must mark a primary, verified, public/private email on GitHub"
                    .into(),
            )
        })?;

    Ok(GitHubIdentity {
        subject: user.id.to_string(),
        login: user.login,
        email: chosen.email.clone(),
        name: user.name,
        avatar_url: user.avatar_url,
    })
}

/// Subset of GitHub's `/login/oauth/access_token` response. We only
/// consume `access_token` (for the subsequent `/user` + `/user/emails`
/// requests) and `scope` (to enforce the `user:email` requirement).
#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    scope: String,
    #[serde(default)]
    #[allow(dead_code)] // captured for completeness; not consumed
    token_type: String,
}

#[derive(Deserialize)]
struct UserResponse {
    id: i64,
    login: String,
    name: Option<String>,
    avatar_url: Option<String>,
}

#[derive(Deserialize)]
struct EmailEntry {
    email: String,
    primary: bool,
    verified: bool,
}

/// Authenticated GET against the GitHub REST API, decoded as JSON.
///
/// Per the GitHub API docs (and observed behaviour), three headers are
/// non-negotiable:
///   - `Authorization: Bearer <token>` — the user-context credential.
///   - `Accept: application/vnd.github+json` — pins the response shape
///     to the current REST API (without this you can get the legacy V3
///     shape on some endpoints).
///   - `User-Agent: <something>` — GitHub returns 403 without one. We
///     send a stable identifier so abuse reports name us correctly.
///   - `X-GitHub-Api-Version: 2022-11-28` — explicit API version pin.
async fn get_with_token<T: serde::de::DeserializeOwned>(
    client: &cyper::Client,
    url: &str,
    access_token: &str,
) -> Result<T> {
    let resp = client
        .request(http::Method::GET, url)
        .map_err(|e| AuthError::Internal(format!("github GET build {url}: {e}")))?
        .header("authorization", &format!("Bearer {access_token}"))
        .map_err(|e| AuthError::Internal(format!("github auth header: {e}")))?
        .header("accept", "application/vnd.github+json")
        .map_err(|e| AuthError::Internal(format!("github accept: {e}")))?
        .header("user-agent", "zeroship-auth/1")
        .map_err(|e| AuthError::Internal(format!("github ua: {e}")))?
        .header("x-github-api-version", "2022-11-28")
        .map_err(|e| AuthError::Internal(format!("github api version: {e}")))?
        .send()
        .await
        .map_err(|e| AuthError::Internal(format!("github GET send {url}: {e}")))?;
    let status = resp.status().as_u16();
    let body = resp
        .text()
        .await
        .map_err(|e| AuthError::Internal(format!("github GET read {url}: {e}")))?;
    if !(200..300).contains(&status) {
        return Err(AuthError::Internal(format!(
            "github GET {url} → {status}: {body}"
        )));
    }
    serde_json::from_str(&body)
        .map_err(|e| AuthError::Internal(format!("github GET decode {url}: {e}\nbody: {body}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn cfg_with_github() -> AuthConfig {
        let mut cfg = AuthConfig::parse_from(["zeroship-auth", "--db-url", "postgres://x/y"]);
        cfg.github_client_id = Some("test-client".into());
        cfg.github_client_secret = Some("test-secret".into());
        cfg.github_redirect_uri = "https://auth.zeroship.ai/oauth/github/callback".into();
        cfg
    }

    fn make_email(email: &str, primary: bool, verified: bool) -> EmailEntry {
        EmailEntry {
            email: email.into(),
            primary,
            verified,
        }
    }

    #[test]
    fn start_authorize_url_well_formed() {
        let cfg = cfg_with_github();
        let start = start_authorize_url(&cfg).expect("start");
        assert!(
            start.url.starts_with(cfg.github_authorize_url.as_str()),
            "url base: {}",
            start.url
        );
        assert!(start.url.contains("client_id=test-client"));
        assert!(start.url.contains("response_type=code"));
        assert!(start.url.contains("code_challenge_method=S256"));
        assert!(start.url.contains("code_challenge="));
        // `form_urlencoded` emits `+` for the space between scopes; `:`
        // in `read:user` is URL-encoded as `%3A`.
        assert!(start.url.contains("scope=read%3Auser+user%3Aemail"));
        assert!(start.url.contains(&format!("state={}", start.state)));
        // PKCE verifier is the raw 43-char base64url; the URL only
        // carries the challenge, never the verifier.
        assert!(!start.url.contains(&start.verifier));
        // redirect_uri is URL-encoded (`/` → `%2F`, `:` → `%3A`).
        assert!(start
            .url
            .contains("redirect_uri=https%3A%2F%2Fauth.zeroship.ai%2Foauth%2Fgithub%2Fcallback"));
        // 32-byte CSPRNG → 43-char base64url-no-pad.
        assert_eq!(start.verifier.len(), 43);
        assert_eq!(start.state.len(), 43);
        // state and verifier must be distinct random values.
        assert_ne!(start.state, start.verifier);
    }

    #[test]
    fn start_authorize_url_errors_when_client_id_missing() {
        let mut cfg = AuthConfig::parse_from(["zeroship-auth", "--db-url", "postgres://x/y"]);
        cfg.github_client_id = None;
        let err = start_authorize_url(&cfg).expect_err("must fail without client_id");
        assert!(matches!(err, AuthError::Config(_)), "got: {err:?}");
    }

    #[test]
    fn picks_primary_verified_non_noreply() {
        let emails = [
            // Even when noreply is listed first AND is primary+verified,
            // we must skip it. GitHub lists it because the user opted
            // into "Keep my email addresses private", and the noreply
            // address gets the `primary=true` flag in that mode.
            make_email("12345+alice@users.noreply.github.com", true, true),
            make_email("alice@example.com", true, true),
            make_email("alice.other@example.com", false, true),
        ];
        let chosen = emails
            .iter()
            .find(|e| e.primary && e.verified && !e.email.ends_with(NOREPLY_DOMAIN));
        assert_eq!(chosen.map(|e| e.email.as_str()), Some("alice@example.com"));
    }

    #[test]
    fn rejects_when_only_noreply_or_unverified() {
        let emails = [
            make_email("12345+alice@users.noreply.github.com", true, true),
            // not primary
            make_email("alice@example.com", false, true),
            // not verified
            make_email("alice.other@example.com", false, false),
        ];
        let chosen = emails
            .iter()
            .find(|e| e.primary && e.verified && !e.email.ends_with(NOREPLY_DOMAIN));
        assert!(chosen.is_none());
    }
}
