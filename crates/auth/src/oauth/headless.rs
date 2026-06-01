//! Headless cookie-jar OAuth authorization-code dance.
//!
//! Drives Hydra's authorization-code + PKCE flow *server-side*, without a
//! browser, after the caller has already verified the subject's password
//! credentials out-of-band (see [`crate::identity::credentials`]). This is the
//! engine behind the in-page (`POST /password`) login endpoint: the gateway
//! collects the user's credentials inside the app's own page, the auth service
//! verifies them, and then mints an authorization `code` here that the browser
//! exchanges for tokens at Hydra's `/oauth2/token` (it holds the PKCE verifier).
//!
//! ## Why a cookie jar
//!
//! Hydra threads the login → consent → code handoff through a server-set
//! cookie (the login session). The flow is: `GET /oauth2/auth` → 302 to our
//! `/login?login_challenge=…`; we `accept_login` via the admin API and follow
//! Hydra's `redirect_to` (which loops back through `/oauth2/auth?login_verifier=…`)
//! carrying the cookie; Hydra then 302s to `/consent?consent_challenge=…`; we
//! `accept_consent` (silent self-grant) and follow the final `redirect_to` to
//! the registered `redirect_uri?code=…`. Each hop must replay Hydra's cookies,
//! so we keep a minimal jar (mirrors `tests/common/mod.rs::CookieJar`).
//!
//! ## Security
//!
//! - **First-party only.** `accept_consent` here is a *silent self-grant* for
//!   the authenticated subject's OWN identity scopes (`openid`/`profile`/
//!   `email`/`offline_access`). It is gated on `client.skip_consent` — a
//!   first-party platform client. A third-party (`skip_consent = false`) client
//!   reaching this path is rejected ([`HeadlessError::ConsentRequired`]); it
//!   must go through the interactive `/consent` UI. This is never a third-party
//!   consent auto-grant.
//! - This module performs NO credential check of its own. Callers MUST have a
//!   successful [`crate::identity::credentials::verify_password_credentials`]
//!   before invoking [`mint_code_for_subject`] (invariant II).
//!
//! The redirect-following logic re-points every Hydra `redirect_to` at the
//! configured `hydra_public_url` base (preserving path + query) so the
//! `login_verifier` / `consent_verifier` / `code` survive even when Hydra
//! stamps an externally-facing host into its redirects.

use std::collections::HashMap;

use serde_json::json;

use crate::error::AuthError;
use crate::hydra_client::types::{
    AcceptConsentRequest, AcceptLoginRequest, ConsentSession,
};
use crate::hydra_client::HydraAdmin;
use crate::store::users;

/// Reserved OIDC identity scopes — always self-grantable by the authenticated
/// end user (sharing one's own name/email needs no platform privilege). Mirrors
/// `ui::consent::IDENTITY_SCOPES`.
const IDENTITY_SCOPES: &[&str] = &["openid", "profile", "email", "offline_access"];

/// Inputs to [`mint_code_for_subject`]. All browser-supplied except `subject`,
/// which the caller fixes from the verified credentials.
#[derive(Debug)]
pub struct MintCodeParams<'a> {
    /// OAuth client id (first-party, `skip_consent`) the code is minted for.
    pub client_id: &'a str,
    /// The authenticated subject — the verified user's UUID, hyphenated. This
    /// becomes Hydra's session `subject` and the `sub` claim base.
    pub subject: &'a str,
    /// Registered redirect URI the code is bound to (the app's popup-callback).
    pub redirect_uri: &'a str,
    /// Requested scope (space-delimited). Defaults to `"openid"` when empty.
    pub scope: &'a str,
    /// Browser-supplied `state` (round-tripped to the RP).
    pub state: &'a str,
    /// Browser-supplied OIDC `nonce`.
    pub nonce: &'a str,
    /// Browser-supplied PKCE `code_challenge` (S256). The gateway/browser holds
    /// the verifier; the auth service never sees it.
    pub code_challenge: &'a str,
    /// PKCE challenge method. Only `S256` is accepted; empty defaults to `S256`.
    pub code_challenge_method: &'a str,
}

/// Failure classes for the headless dance.
#[derive(Debug)]
pub enum HeadlessError {
    /// Hydra/admin transport or protocol failure (non-2xx, decode, timeout).
    Hydra(AuthError),
    /// A redirect we needed to parse carried no expected query param
    /// (`login_challenge` / `consent_challenge` / `code`), or a hop returned a
    /// non-redirect status. Signals a Hydra-side mismatch, not a user error.
    Protocol(String),
    /// The client is NOT a first-party `skip_consent` client, or it requested a
    /// non-identity scope. The headless silent-grant path refuses; the flow
    /// must use the interactive consent UI.
    ConsentRequired,
}

impl From<AuthError> for HeadlessError {
    fn from(e: AuthError) -> Self {
        Self::Hydra(e)
    }
}

impl std::fmt::Display for HeadlessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Hydra(e) => write!(f, "hydra: {e}"),
            Self::Protocol(m) => write!(f, "protocol: {m}"),
            Self::ConsentRequired => write!(f, "consent required (not a first-party client)"),
        }
    }
}

impl std::error::Error for HeadlessError {}

/// Minimal cookie jar: `name → value`. Ignores Domain/Path/Expires (the flow
/// hits one host — Hydra public — and never reuses a name across hops with
/// conflicting meaning). Mirrors `tests/common/mod.rs::CookieJar`.
#[derive(Default)]
struct CookieJar {
    inner: HashMap<String, String>,
}

impl CookieJar {
    fn absorb(&mut self, resp: &cyper::Response) {
        for hv in resp.headers().get_all(http::header::SET_COOKIE) {
            let Ok(s) = hv.to_str() else { continue };
            let first = s.split(';').next().unwrap_or("");
            if let Some((name, value)) = first.split_once('=') {
                let name = name.trim();
                let value = value.trim();
                if name.is_empty() {
                    continue;
                }
                if value.is_empty() {
                    self.inner.remove(name);
                } else {
                    self.inner.insert(name.to_string(), value.to_string());
                }
            }
        }
    }

    fn header(&self) -> String {
        let mut parts: Vec<String> =
            self.inner.iter().map(|(k, v)| format!("{k}={v}")).collect();
        parts.sort();
        parts.join("; ")
    }
}

fn location(resp: &cyper::Response) -> String {
    resp.headers()
        .get(http::header::LOCATION)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("")
        .to_string()
}

fn extract_query_param(raw_url: &str, key: &str) -> Option<String> {
    let parsed = url::Url::parse(raw_url).ok()?;
    parsed
        .query_pairs()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
}

/// Re-point a Hydra `redirect_to` at the reachable `hydra_public_url` base,
/// preserving path + query so the embedded verifier/code survive. When the
/// redirect already targets a different host (e.g. the app's `redirect_uri`
/// carrying `?code=`), it is returned verbatim — we only rewrite Hydra's own
/// `/oauth2/*` hops back to the configured public base.
fn point_at_hydra(redirect_to: &str, hydra_public: &str) -> String {
    let hydra_public = hydra_public.trim_end_matches('/');
    // Parse both; if the redirect path is a Hydra OAuth path, force the host to
    // the configured public base. Otherwise leave it (it's the final RP
    // redirect_uri).
    let Ok(parsed) = url::Url::parse(redirect_to) else {
        return redirect_to.to_string();
    };
    let path = parsed.path();
    if path.starts_with("/oauth2/") || path.starts_with("/login") || path.starts_with("/consent") {
        let query = parsed.query().map(|q| format!("?{q}")).unwrap_or_default();
        format!("{hydra_public}{path}{query}")
    } else {
        redirect_to.to_string()
    }
}

/// Mint an authorization `code` for an already-verified subject, headlessly.
///
/// Drives Hydra's authorization-code + PKCE dance using the admin API to
/// `accept_login` and `accept_consent` (silent self-grant), following Hydra's
/// redirects with a server-held cookie jar, and returns the authorization
/// `code` from the final `redirect_uri?code=…`.
///
/// SECURITY: callers MUST have a successful credential verify first (invariant
/// II). The consent self-grant is first-party `skip_consent` + identity scopes
/// only (invariant III) — a third-party client yields
/// [`HeadlessError::ConsentRequired`].
///
/// # Errors
///
/// [`HeadlessError`] on any Hydra transport/protocol error, a missing redirect
/// param, or a non-first-party / non-identity-scope consent request.
// The dance is a linear sequence of named hops (auth → accept_login → follow →
// consent → accept_consent → follow → code); splitting it would obscure the
// strict ordering the security invariants depend on.
#[allow(clippy::future_not_send, clippy::too_many_lines)]
pub async fn mint_code_for_subject(
    admin: &HydraAdmin,
    db: &compio_postgres::Client,
    hydra_public_url: &str,
    http: &cyper::Client,
    params: &MintCodeParams<'_>,
) -> std::result::Result<String, HeadlessError> {
    // Validate PKCE method up front (only S256). Empty ⇒ S256.
    let method = if params.code_challenge_method.is_empty() {
        "S256"
    } else {
        params.code_challenge_method
    };
    if method != "S256" {
        return Err(HeadlessError::Protocol(
            "code_challenge_method must be S256".into(),
        ));
    }
    let scope = if params.scope.is_empty() {
        "openid"
    } else {
        params.scope
    };
    let hydra_public = hydra_public_url.trim_end_matches('/');

    let mut jar = CookieJar::default();

    // 1. GET /oauth2/auth → 302 to /login?login_challenge=… (mirrors
    //    e2e_password.rs:188-213).
    let auth_url = {
        let q = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("client_id", params.client_id)
            .append_pair("response_type", "code")
            .append_pair("scope", scope)
            .append_pair("redirect_uri", params.redirect_uri)
            .append_pair("state", params.state)
            .append_pair("nonce", params.nonce)
            .append_pair("code_challenge", params.code_challenge)
            .append_pair("code_challenge_method", method)
            .finish();
        format!("{hydra_public}/oauth2/auth?{q}")
    };
    let resp = http
        .request(http::Method::GET, &auth_url)
        .map_err(|e| HeadlessError::Protocol(format!("build GET /oauth2/auth: {e}")))?
        .header("cookie", jar.header())
        .map_err(|e| HeadlessError::Protocol(format!("cookie header: {e}")))?
        .send()
        .await
        .map_err(|e| HeadlessError::Protocol(format!("send GET /oauth2/auth: {e}")))?;
    jar.absorb(&resp);
    let login_loc = location(&resp);
    let login_challenge = extract_query_param(&login_loc, "login_challenge").ok_or_else(|| {
        HeadlessError::Protocol(format!(
            "hydra /oauth2/auth redirect carries no login_challenge: {login_loc}"
        ))
    })?;

    // 2. accept_login via admin (acr/amr stamp the pwd authentication).
    let accept = AcceptLoginRequest {
        subject: params.subject.to_string(),
        remember: Some(true),
        remember_for: Some(3600),
        acr: Some("urn:zeroship:pwd".into()),
        amr: Some(vec!["pwd".into()]),
        ..Default::default()
    };
    let after_login = admin.accept_login(&login_challenge, &accept).await?;

    // 3. Follow Hydra's post-login redirect (carrying the cookie) → /consent.
    let after_login = point_at_hydra(&after_login.redirect_to, hydra_public);
    let resp = http
        .request(http::Method::GET, &after_login)
        .map_err(|e| HeadlessError::Protocol(format!("build GET post-login: {e}")))?
        .header("cookie", jar.header())
        .map_err(|e| HeadlessError::Protocol(format!("cookie header: {e}")))?
        .send()
        .await
        .map_err(|e| HeadlessError::Protocol(format!("send GET post-login: {e}")))?;
    jar.absorb(&resp);
    let consent_loc = location(&resp);

    // If Hydra skipped consent entirely (a remembered grant), this hop may go
    // straight to the redirect_uri with ?code=. Handle that fast path.
    if let Some(code) = extract_query_param(&consent_loc, "code") {
        return Ok(code);
    }

    let consent_challenge =
        extract_query_param(&consent_loc, "consent_challenge").ok_or_else(|| {
            HeadlessError::Protocol(format!(
                "hydra post-login redirect carries no consent_challenge: {consent_loc}"
            ))
        })?;

    // 4. Fetch the consent challenge and enforce first-party + identity-scope.
    let info = admin.get_consent(&consent_challenge).await?;

    // INVARIANT III: silent self-grant only for first-party skip_consent
    // clients. A third-party client must use the interactive consent UI.
    if !info.client.skip_consent {
        return Err(HeadlessError::ConsentRequired);
    }
    // Only identity scopes may be silently self-granted here. Any non-identity
    // scope (an app/platform scope) requires the interactive authorization gate.
    if info
        .requested_scope
        .iter()
        .any(|s| !IDENTITY_SCOPES.contains(&s.as_str()))
    {
        return Err(HeadlessError::ConsentRequired);
    }

    // 5. accept_consent (silent self-grant) — id_token claims from the user row.
    let id_token_claims = build_id_token_claims(db, &info.subject, &info.requested_scope).await;
    let accept = AcceptConsentRequest {
        grant_scope: info.requested_scope.clone(),
        grant_access_token_audience: info.requested_access_token_audience.clone(),
        remember: Some(true),
        remember_for: Some(3600),
        session: Some(ConsentSession {
            id_token: Some(id_token_claims),
            access_token: None,
        }),
    };
    let after_consent = admin.accept_consent(&consent_challenge, &accept).await?;

    // 6. Follow the final redirect → redirect_uri?code=…
    let after_consent = point_at_hydra(&after_consent.redirect_to, hydra_public);
    let resp = http
        .request(http::Method::GET, &after_consent)
        .map_err(|e| HeadlessError::Protocol(format!("build GET post-consent: {e}")))?
        .header("cookie", jar.header())
        .map_err(|e| HeadlessError::Protocol(format!("cookie header: {e}")))?
        .send()
        .await
        .map_err(|e| HeadlessError::Protocol(format!("send GET post-consent: {e}")))?;
    let cb_url = location(&resp);
    extract_query_param(&cb_url, "code").ok_or_else(|| {
        HeadlessError::Protocol(format!(
            "hydra post-consent redirect carries no code: {cb_url}"
        ))
    })
}

/// Build the `id_token` claims object from the granted identity scopes. Mirrors
/// `ui::consent::build_id_token_claims`; Hydra supplies `sub` itself.
#[allow(clippy::future_not_send)]
async fn build_id_token_claims(
    db: &compio_postgres::Client,
    subject: &str,
    granted_scope: &[String],
) -> serde_json::Value {
    let user = match users::find_by_id(db, subject).await {
        Ok(u) => u,
        Err(e) => {
            tracing::warn!(error = %e, subject = %subject, "headless: users::find_by_id failed");
            None
        }
    };

    let mut claims = serde_json::Map::new();
    if let Some(u) = user.as_ref() {
        if granted_scope.iter().any(|s| s == "email") {
            claims.insert("email".into(), json!(u.email));
            claims.insert(
                "email_verified".into(),
                json!(u.email_verified_at.is_some()),
            );
        }
        if granted_scope.iter().any(|s| s == "profile") {
            claims.insert("name".into(), json!(u.name));
            if let Some(p) = u.avatar_url.as_ref() {
                claims.insert("picture".into(), json!(p));
            }
        }
    }
    serde_json::Value::Object(claims)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn point_at_hydra_rewrites_oauth_paths_to_public_base() {
        let out = point_at_hydra(
            "https://auth.zeroship.ai/oauth2/auth?login_verifier=abc",
            "http://127.0.0.1:4444",
        );
        assert_eq!(out, "http://127.0.0.1:4444/oauth2/auth?login_verifier=abc");
    }

    #[test]
    fn point_at_hydra_rewrites_login_and_consent_paths() {
        assert_eq!(
            point_at_hydra("https://ext.example/login?login_challenge=x", "http://h:4444"),
            "http://h:4444/login?login_challenge=x"
        );
        assert_eq!(
            point_at_hydra(
                "https://ext.example/consent?consent_challenge=y",
                "http://h:4444/"
            ),
            "http://h:4444/consent?consent_challenge=y"
        );
    }

    #[test]
    fn point_at_hydra_leaves_rp_redirect_uri_untouched() {
        // The final hop to the app's own redirect_uri must NOT be rewritten.
        let cb = "https://myapp.zeroship.ai/__zeroship/auth/popup-callback?code=zzz&state=s";
        assert_eq!(point_at_hydra(cb, "http://127.0.0.1:4444"), cb);
    }

    #[test]
    fn extract_query_param_reads_code() {
        assert_eq!(
            extract_query_param("https://x/cb?code=abc&state=s", "code").as_deref(),
            Some("abc")
        );
        assert_eq!(extract_query_param("https://x/cb?state=s", "code"), None);
    }

    #[test]
    fn cookie_jar_absorbs_and_serializes() {
        // We can't easily synth a cyper::Response here; exercise the jar via the
        // serialize path with manual inserts (absorb is covered by the live e2e).
        let mut jar = CookieJar::default();
        jar.inner.insert("b".into(), "2".into());
        jar.inner.insert("a".into(), "1".into());
        assert_eq!(jar.header(), "a=1; b=2");
    }
}
