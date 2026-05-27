//! OIDC Relying Party module for the control plane. Single OIDC client
//! `console.zeroship.ai`. Per proposal §2.3.
//!
//! The shape mirrors `crates/gateway/src/oidc_rp.rs` — same authorize
//! redirect / stash / callback flow, with the cookies + session table
//! scoped to the console. The control plane is the OIDC RP that owns
//! the `/auth/callback` route for the creator dashboard; the ID token
//! issued by hydra never reaches the browser. Post-callback the
//! control plane mints its own opaque per-origin session id
//! (`__Host-zs_console_session`) and stores the user profile in
//! `auth.console_sessions`.
//!
//! Wiring into HTTP handlers lives in `api.rs`; U7 only adds the new
//! flow alongside the legacy `auth_handlers` / `auth_service` chain.
//! U8 retires the legacy path.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use zeroship_core::oidc_verify::{verify_id_token, JwksCache, OidcError, TokenClaims};
use zeroship_core::pkce::{generate_verifier, s256_challenge};

/// A configured OIDC relying party for the control plane, parameterised
/// by the hydra public URL, the `console.zeroship.ai` client credentials,
/// and the signing key used to MAC the per-request
/// `__Host-zs_console_stash` cookie.
#[derive(Debug, Clone)]
pub struct ConsoleOidcRp {
    /// hydra's public base URL (e.g. `https://auth.zeroship.ai`).
    pub auth_public: String,
    /// `OAuth2` `client_id` registered with hydra. Always
    /// `"console.zeroship.ai"` in this binary; kept as a field so tests
    /// can drive a different value.
    pub client_id: String,
    /// `OAuth2` `client_secret` for confidential client auth at
    /// `/oauth2/token`.
    pub client_secret: String,
    /// JWKS cache — one per RP instance.
    pub jwks: Arc<JwksCache>,
    /// HMAC-SHA256 key used to sign the `__Host-zs_console_stash`
    /// cookie body. Must be at least 32 random bytes in prod.
    pub stash_signing_key: Vec<u8>,
}

impl ConsoleOidcRp {
    /// Construct a new RP. The JWKS cache URL is derived from
    /// `auth_public` by appending `/.well-known/jwks.json`.
    pub fn new(
        auth_public: impl Into<String>,
        client_id: impl Into<String>,
        client_secret: impl Into<String>,
        stash_signing_key: impl Into<Vec<u8>>,
    ) -> Self {
        let auth_public = auth_public.into();
        let jwks_url = format!(
            "{}/.well-known/jwks.json",
            auth_public.trim_end_matches('/')
        );
        Self {
            auth_public,
            client_id: client_id.into(),
            client_secret: client_secret.into(),
            jwks: Arc::new(JwksCache::new(jwks_url)),
            stash_signing_key: stash_signing_key.into(),
        }
    }

    /// Build the `/oauth2/auth` redirect URL + the signed stash cookie
    /// body. `original_path` is the dashboard path the user was trying
    /// to reach; `redirect_uri` is the fixed
    /// `https://console.zeroship.ai/auth/callback` (dev: `http://...`).
    ///
    /// Returns `(authorize_url, stash_cookie_value)`. Caller wraps the
    /// cookie value with [`set_console_stash_cookie`] before setting it
    /// on the response.
    #[must_use]
    pub fn build_authorize_redirect(
        &self,
        original_path: &str,
        redirect_uri: &str,
    ) -> (String, String) {
        let verifier = generate_verifier();
        let challenge = s256_challenge(&verifier);
        let state = generate_verifier();
        let nonce = generate_verifier();

        let stash = Stash {
            state: state.clone(),
            verifier,
            nonce: nonce.clone(),
            original_path: original_path.to_string(),
            redirect_uri: redirect_uri.to_string(),
        };
        let stash_value = stash.encode(&self.stash_signing_key);

        let mut q = url::form_urlencoded::Serializer::new(String::new());
        q.append_pair("client_id", &self.client_id);
        q.append_pair("response_type", "code");
        q.append_pair("scope", "openid offline_access email profile");
        q.append_pair("redirect_uri", redirect_uri);
        q.append_pair("state", &state);
        q.append_pair("nonce", &nonce);
        q.append_pair("code_challenge", &challenge);
        q.append_pair("code_challenge_method", "S256");
        let query = q.finish();

        let url = format!(
            "{}/oauth2/auth?{}",
            self.auth_public.trim_end_matches('/'),
            query
        );

        (url, stash_value)
    }

    /// Process the callback. Verifies the stash cookie signature,
    /// matches the `state` parameter, exchanges the code for tokens,
    /// verifies the ID token, and returns `(claims, original_path)` on
    /// success.
    ///
    /// # Errors
    /// - [`OidcRpError::StashInvalid`] — stash cookie absent, malformed,
    ///   or tampered.
    /// - [`OidcRpError::StateMismatch`] — `state` query param does not
    ///   equal the value stashed at authorize time (CSRF guard).
    /// - [`OidcRpError::TokenExchange`] — hydra rejected the code or
    ///   network failure on `/oauth2/token`.
    /// - [`OidcRpError::VerifyIdToken`] — ID token signature, issuer,
    ///   audience, `exp`, or `nonce` check failed.
    pub async fn finish_callback(
        &self,
        code: &str,
        state_param: &str,
        stash_cookie: &str,
    ) -> Result<(TokenClaims, String), OidcRpError> {
        let stash = Stash::decode(stash_cookie, &self.stash_signing_key)
            .ok_or(OidcRpError::StashInvalid)?;

        if state_param != stash.state {
            return Err(OidcRpError::StateMismatch);
        }

        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("grant_type", "authorization_code")
            .append_pair("code", code)
            .append_pair("redirect_uri", &stash.redirect_uri)
            .append_pair("client_id", &self.client_id)
            .append_pair("client_secret", &self.client_secret)
            .append_pair("code_verifier", &stash.verifier)
            .finish();

        let client = cyper::Client::new();
        let token_url = format!(
            "{}/oauth2/token",
            self.auth_public.trim_end_matches('/')
        );
        let resp = client
            .request(http::Method::POST, &token_url)
            .map_err(|e| OidcRpError::TokenExchange(format!("build: {e}")))?
            .header("content-type", "application/x-www-form-urlencoded")
            .map_err(|e| OidcRpError::TokenExchange(format!("header: {e}")))?
            .body(body)
            .send()
            .await
            .map_err(|e| OidcRpError::TokenExchange(format!("send: {e}")))?;

        let status = resp.status().as_u16();
        let resp_body = resp
            .text()
            .await
            .map_err(|e| OidcRpError::TokenExchange(format!("read: {e}")))?;
        if !(200..300).contains(&status) {
            return Err(OidcRpError::TokenExchange(format!(
                "HTTP {status}: {resp_body}"
            )));
        }

        let tr: TokenResponse = serde_json::from_str(&resp_body).map_err(|e| {
            OidcRpError::TokenExchange(format!("parse: {e}\nbody: {resp_body}"))
        })?;

        let expected_iss = format!("{}/", self.auth_public.trim_end_matches('/'));
        let claims = verify_id_token(
            &self.jwks,
            &tr.id_token,
            &expected_iss,
            &self.client_id,
            Some(&stash.nonce),
        )
        .await?;

        Ok((claims, stash.original_path))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum OidcRpError {
    #[error("stash cookie invalid or tampered")]
    StashInvalid,
    #[error("state parameter mismatch")]
    StateMismatch,
    #[error("token exchange: {0}")]
    TokenExchange(String),
    #[error("verify id_token: {0}")]
    VerifyIdToken(#[from] OidcError),
}

/// `/oauth2/token` response body (subset).
#[derive(Debug, Serialize, Deserialize)]
struct TokenResponse {
    access_token: String,
    id_token: String,
    expires_in: u64,
    #[serde(default)]
    refresh_token: Option<String>,
    token_type: String,
    #[serde(default)]
    scope: Option<String>,
}

/// Server-side state stashed in the signed `__Host-zs_console_stash`
/// cookie between the initial 302 → hydra and the eventual callback.
/// Signed with HMAC-SHA256 using `ConsoleOidcRp::stash_signing_key`.
#[derive(Debug, Serialize, Deserialize)]
struct Stash {
    state: String,
    verifier: String,
    nonce: String,
    original_path: String,
    redirect_uri: String,
}

impl Stash {
    /// Encode + HMAC-sign with `key`. Returns `base64url(json).base64url(hmac)`.
    fn encode(&self, key: &[u8]) -> String {
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
        let json = serde_json::to_vec(self).expect("stash serialize");
        let b64 = URL_SAFE_NO_PAD.encode(&json);
        let mac = hmac_sha256(key, b64.as_bytes());
        let mac_b64 = URL_SAFE_NO_PAD.encode(mac);
        format!("{b64}.{mac_b64}")
    }

    /// Decode + verify the HMAC. Returns `Some` if signature checks out
    /// and the JSON deserializes, `None` otherwise. Uses a constant-time
    /// MAC comparison.
    fn decode(value: &str, key: &[u8]) -> Option<Self> {
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
        let (b64, mac_b64) = value.split_once('.')?;
        let expected_mac = hmac_sha256(key, b64.as_bytes());
        let provided_mac = URL_SAFE_NO_PAD.decode(mac_b64).ok()?;
        if expected_mac.len() != provided_mac.len() {
            return None;
        }
        let mut diff = 0u8;
        for (a, b) in expected_mac.iter().zip(provided_mac.iter()) {
            diff |= a ^ b;
        }
        if diff != 0 {
            return None;
        }
        let json = URL_SAFE_NO_PAD.decode(b64).ok()?;
        serde_json::from_slice(&json).ok()
    }
}

fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    use hmac::{Hmac, Mac};
    let mut mac = <Hmac<sha2::Sha256>>::new_from_slice(key).expect("hmac key");
    mac.update(msg);
    mac.finalize().into_bytes().into()
}

// ─── Cookie helpers ──────────────────────────────────────────────────────
//
// Two cookies live on `console.zeroship.ai`:
//
// - `__Host-zs_console_session` — opaque session id minted after a
//   successful OIDC dance. 12 h max-age, set on `/auth/callback`,
//   cleared on `/auth/logout`. The control plane looks this up
//   server-side via `console_sessions::validate` on every request.
// - `__Host-zs_console_stash` — the signed PKCE+state stash. 10 min
//   max-age, set on the redirect to hydra, cleared on callback.
//
// The `__Host-` prefix (RFC 6265bis §4.1.3) requires `Path=/`, no
// `Domain=`, and `Secure`. The dev override drops `Secure` only for
// HTTP localhost; the rest of the attributes never move.

/// Per-origin console session cookie name. Set after a successful code
/// exchange; cleared on logout.
pub const CONSOLE_SESSION_COOKIE: &str = "__Host-zs_console_session";

/// 12-hour absolute lifetime for the console session cookie.
pub const CONSOLE_SESSION_MAX_AGE_SECS: i64 = 12 * 3600;

/// Build the `Set-Cookie` header value for the console session.
///
/// `insecure_dev = true` drops `Secure` so localhost HTTP works. In
/// production this MUST be false (the `__Host-` prefix requires Secure).
#[must_use]
pub fn set_console_session_cookie(session_id: &uuid::Uuid, insecure_dev: bool) -> String {
    let secure = if insecure_dev { "" } else { "; Secure" };
    format!(
        "{CONSOLE_SESSION_COOKIE}={session_id}; Path=/; HttpOnly; SameSite=Lax{secure}; Max-Age={CONSOLE_SESSION_MAX_AGE_SECS}"
    )
}

/// Clear the console session cookie on logout.
#[must_use]
pub fn clear_console_session_cookie(insecure_dev: bool) -> String {
    let secure = if insecure_dev { "" } else { "; Secure" };
    format!("{CONSOLE_SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Lax{secure}; Max-Age=0")
}

/// Parse the console session UUID from a `Cookie` header value.
#[must_use]
pub fn parse_console_session_cookie(cookie_header: &str) -> Option<uuid::Uuid> {
    let prefix = format!("{CONSOLE_SESSION_COOKIE}=");
    for part in cookie_header.split(';') {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix(&prefix) {
            return uuid::Uuid::parse_str(rest).ok();
        }
    }
    None
}

/// PKCE/state stash cookie name. Lives only between the initial redirect
/// to hydra and the eventual `/auth/callback`.
pub const CONSOLE_STASH_COOKIE: &str = "__Host-zs_console_stash";

/// 10-minute window for the OIDC dance to complete.
pub const CONSOLE_STASH_MAX_AGE_SECS: i64 = 600;

/// Build the `Set-Cookie` header value for the OIDC stash.
#[must_use]
pub fn set_console_stash_cookie(value: &str, insecure_dev: bool) -> String {
    let secure = if insecure_dev { "" } else { "; Secure" };
    format!(
        "{CONSOLE_STASH_COOKIE}={value}; Path=/; HttpOnly; SameSite=Lax{secure}; Max-Age={CONSOLE_STASH_MAX_AGE_SECS}"
    )
}

/// Clear the OIDC stash cookie. Set on the callback response so the
/// short-lived stash doesn't linger after the dance completes.
#[must_use]
pub fn clear_console_stash_cookie(insecure_dev: bool) -> String {
    let secure = if insecure_dev { "" } else { "; Secure" };
    format!("{CONSOLE_STASH_COOKIE}=; Path=/; HttpOnly; SameSite=Lax{secure}; Max-Age=0")
}

/// Parse the raw stash value out of a `Cookie` header. Returns the
/// signed-blob string; pass it to `ConsoleOidcRp::finish_callback` to
/// verify and recover the payload.
#[must_use]
pub fn parse_console_stash_cookie(cookie_header: &str) -> Option<String> {
    let prefix = format!("{CONSOLE_STASH_COOKIE}=");
    for part in cookie_header.split(';') {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix(&prefix) {
            return Some(rest.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_stash() -> Stash {
        Stash {
            state: "s".into(),
            verifier: "v".into(),
            nonce: "n".into(),
            original_path: "/dash".into(),
            redirect_uri: "https://console.zeroship.ai/auth/callback".into(),
        }
    }

    #[test]
    fn authorize_url_has_required_params() {
        let rp = ConsoleOidcRp::new(
            "https://auth.zeroship.ai",
            "console.zeroship.ai",
            "secret",
            b"signing-key-1234".to_vec(),
        );
        let (url, stash) = rp.build_authorize_redirect(
            "/dash/apps",
            "https://console.zeroship.ai/auth/callback",
        );
        assert!(url.starts_with("https://auth.zeroship.ai/oauth2/auth?"));
        assert!(url.contains("client_id=console.zeroship.ai"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("code_challenge="));
        assert!(url.contains("state="));
        assert!(url.contains("nonce="));
        assert!(url.contains("scope=openid+offline_access+email+profile"));
        assert!(url.contains(
            "redirect_uri=https%3A%2F%2Fconsole.zeroship.ai%2Fauth%2Fcallback"
        ));
        assert!(stash.contains('.'));
    }

    #[test]
    fn authorize_url_trims_trailing_slash_on_auth_public() {
        let rp = ConsoleOidcRp::new(
            "https://auth.zeroship.ai/",
            "console.zeroship.ai",
            "secret",
            b"k".repeat(32),
        );
        let (url, _) =
            rp.build_authorize_redirect("/", "https://console.zeroship.ai/auth/callback");
        assert!(url.starts_with("https://auth.zeroship.ai/oauth2/auth?"));
    }

    #[test]
    fn stash_roundtrips_through_sign_verify() {
        let key = b"k".repeat(32);
        let stash = make_stash();
        let encoded = stash.encode(&key);
        let decoded = Stash::decode(&encoded, &key).expect("decode");
        assert_eq!(decoded.state, "s");
        assert_eq!(decoded.verifier, "v");
        assert_eq!(decoded.nonce, "n");
        assert_eq!(decoded.original_path, "/dash");
        assert_eq!(
            decoded.redirect_uri,
            "https://console.zeroship.ai/auth/callback"
        );
    }

    #[test]
    fn stash_rejects_tampering() {
        let key = b"k".repeat(32);
        let stash = make_stash();
        let mut tampered = stash.encode(&key);
        let last = tampered.pop().unwrap();
        tampered.push(if last == 'A' { 'B' } else { 'A' });
        assert!(Stash::decode(&tampered, &key).is_none());
    }

    #[test]
    fn stash_rejects_wrong_key() {
        let stash = make_stash();
        let encoded = stash.encode(b"k1234567890abcdef");
        assert!(Stash::decode(&encoded, b"different-key-here").is_none());
    }

    #[test]
    fn stash_rejects_malformed_input() {
        let key = b"k".repeat(32);
        assert!(Stash::decode("not-a-signed-cookie", &key).is_none());
        assert!(Stash::decode("@@@.@@@", &key).is_none());
        assert!(Stash::decode("", &key).is_none());
    }

    // ─── Console session cookie ─────────────────────────────────────

    #[test]
    fn console_session_set_cookie_has_secure_in_prod() {
        let id = uuid::Uuid::new_v4();
        let c = set_console_session_cookie(&id, false);
        assert!(c.starts_with("__Host-zs_console_session="));
        assert!(c.contains(&id.to_string()));
        assert!(c.contains("Path=/"));
        assert!(c.contains("HttpOnly"));
        assert!(c.contains("SameSite=Lax"));
        assert!(c.contains("Secure"));
        assert!(c.contains("Max-Age=43200")); // 12h
    }

    #[test]
    fn console_session_set_cookie_drops_secure_in_dev() {
        let id = uuid::Uuid::new_v4();
        let c = set_console_session_cookie(&id, true);
        assert!(
            !c.contains("Secure"),
            "dev cookie must NOT have Secure: {c}"
        );
        assert!(c.contains("HttpOnly"));
        assert!(c.contains("SameSite=Lax"));
    }

    #[test]
    fn console_session_clear_cookie_zero_max_age() {
        let c = clear_console_session_cookie(false);
        assert!(c.contains("Max-Age=0"));
        assert!(c.contains("Secure"));
        let dev = clear_console_session_cookie(true);
        assert!(!dev.contains("Secure"));
    }

    #[test]
    fn console_session_parse_cookie_roundtrips() {
        let id = uuid::Uuid::new_v4();
        let header = format!("foo=bar; __Host-zs_console_session={id}; baz=qux");
        assert_eq!(parse_console_session_cookie(&header), Some(id));
        assert_eq!(parse_console_session_cookie("nothing-here"), None);
        assert_eq!(
            parse_console_session_cookie("__Host-zs_console_session=not-a-uuid"),
            None
        );
    }

    // ─── Stash cookie ───────────────────────────────────────────────

    #[test]
    fn console_stash_set_cookie_has_secure_in_prod() {
        let c = set_console_stash_cookie("payload.signed", false);
        assert!(c.starts_with("__Host-zs_console_stash=payload.signed"));
        assert!(c.contains("Path=/"));
        assert!(c.contains("HttpOnly"));
        assert!(c.contains("SameSite=Lax"));
        assert!(c.contains("Secure"));
        assert!(c.contains("Max-Age=600")); // 10 min
    }

    #[test]
    fn console_stash_set_cookie_drops_secure_in_dev() {
        let c = set_console_stash_cookie("v", true);
        assert!(!c.contains("Secure"));
    }

    #[test]
    fn console_stash_clear_cookie_zero_max_age() {
        let c = clear_console_stash_cookie(false);
        assert!(c.contains("Max-Age=0"));
        assert!(c.contains("Secure"));
        let dev = clear_console_stash_cookie(true);
        assert!(!dev.contains("Secure"));
    }

    #[test]
    fn console_stash_parse_cookie_roundtrips() {
        let header = "foo=bar; __Host-zs_console_stash=abc.def; baz=qux";
        assert_eq!(parse_console_stash_cookie(header), Some("abc.def".into()));
        assert_eq!(parse_console_stash_cookie("nothing"), None);
    }
}
