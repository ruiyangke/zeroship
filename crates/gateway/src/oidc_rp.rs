//! OIDC Relying Party module for the gateway.
//!
//! Each hosted creator app on `{app}.zeroship.ai` is served by the same
//! `gateway` OIDC client registered with hydra; the gateway runs the
//! authorize-redirect dance and code exchange on the creator app's behalf
//! (per proposal §2.2 and §10.2). After a successful exchange the
//! gateway mints its own per-origin app session cookie
//! (`__Host-zs_app_session`) — the ID token from hydra never reaches the
//! creator app or the browser.
//!
//! Wiring into the dispatch pipeline lives in U5; this module only
//! publishes the type + its methods. Dead-code warnings on the
//! callback-side surface (`OidcRpError`, `TokenResponse`, the
//! `finish_callback` method) are silenced at module scope until U5
//! lights them up.

#![allow(dead_code)]

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use zeroship_core::oidc_verify::{verify_id_token, JwksCache, OidcError, TokenClaims};
use zeroship_core::pkce::{generate_verifier, s256_challenge};

/// A configured OIDC relying party for the gateway, parameterised by the
/// hydra public URL, the shared gateway client credentials, and the
/// signing key used to MAC the per-request `__Host-zs_oidc_stash` cookie.
#[derive(Debug, Clone)]
pub struct OidcRp {
    /// hydra's public base URL (e.g. `https://auth.zeroship.ai`).
    pub auth_public: String,
    /// `OAuth2` `client_id` registered with hydra (currently `"gateway"`).
    pub client_id: String,
    /// `OAuth2` `client_secret` for confidential client auth at `/oauth2/token`.
    pub client_secret: String,
    /// JWKS cache — shared with other RPs in the same gateway process.
    pub jwks: Arc<JwksCache>,
    /// HMAC-SHA256 key used to sign the `__Host-zs_oidc_stash` cookie body.
    /// Must be at least 32 random bytes in prod.
    pub stash_signing_key: Vec<u8>,
}

impl OidcRp {
    /// Construct a new RP. Caller supplies hydra's public URL, the `OAuth2`
    /// client credentials registered for the gateway, and the stash
    /// signing key. The JWKS cache is derived from `auth_public` by
    /// appending `/.well-known/jwks.json`.
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
    /// body. `original_path` is the request path the user was trying to
    /// reach; `redirect_uri` is the per-app callback URL the worker
    /// registered with hydra (e.g.
    /// `https://myapp.zeroship.ai/__zs/auth/callback`).
    ///
    /// Returns `(authorize_url, stash_cookie_value)`. Caller wraps the
    /// cookie value with [`set_stash_cookie`] before setting it on the
    /// response.
    #[must_use]
    pub fn build_authorize_redirect(
        &self,
        original_path: &str,
        redirect_uri: &str,
    ) -> (String, String) {
        let verifier = generate_verifier();
        let challenge = s256_challenge(&verifier);
        // Reuse `generate_verifier` for state/nonce — each call yields
        // a fresh 256-bit base64url string which satisfies the `OAuth2`
        // `state` and OIDC `nonce` opaqueness requirements.
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

        // Build all query params via form_urlencoded so escaping is
        // consistent (no manual `format!` interpolation of unescaped
        // values). The result is `application/x-www-form-urlencoded`
        // which is the historical query-string convention.
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

    /// Process the callback. Verifies the stash cookie signature, matches
    /// the `state` parameter, exchanges the code for tokens, verifies the
    /// ID token, and returns `(claims, original_path)` on success.
    ///
    /// # Errors
    /// - [`OidcRpError::StashInvalid`] — stash cookie absent, malformed,
    ///   or tampered.
    /// - [`OidcRpError::StateMismatch`] — `state` query param does not
    ///   equal the value stashed when the dance started (CSRF guard).
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
        // 1. Decode + verify stash cookie.
        let stash = Stash::decode(stash_cookie, &self.stash_signing_key)
            .ok_or(OidcRpError::StashInvalid)?;

        // 2. State match (constant-time not strictly required since the
        //    server-side stash is already the source of truth, but compare
        //    by value — mismatch → reject).
        if state_param != stash.state {
            return Err(OidcRpError::StateMismatch);
        }

        // 3. POST /oauth2/token with the code + PKCE verifier + client creds.
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

        // 4. Verify ID token. Hydra's issuer is `auth_public` with a
        //    trailing slash (per ops/hydra.yaml).
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

/// `/oauth2/token` response body (subset). Hydra emits the OIDC standard
/// fields; we only deserialize what we use.
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

/// Server-side state stashed in the signed `__Host-zs_oidc_stash` cookie
/// between the initial 302 → hydra and the eventual callback. Sized to
/// fit comfortably under the 4 KiB cookie limit (well under, in
/// practice: ~250 bytes JSON + 64 byte signature).
///
/// Signed with HMAC-SHA256 using `OidcRp::stash_signing_key`; tampering
/// is rejected at decode time via constant-time compare.
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
        // Constant-time compare — guards against timing side-channels
        // when an attacker probes the signing key bit-by-bit.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn make_stash() -> Stash {
        Stash {
            state: "s".into(),
            verifier: "v".into(),
            nonce: "n".into(),
            original_path: "/p".into(),
            redirect_uri: "https://x/cb".into(),
        }
    }

    #[test]
    fn authorize_url_has_required_params() {
        let rp = OidcRp::new(
            "https://auth.zeroship.ai",
            "gateway",
            "secret",
            b"signing-key-1234".to_vec(),
        );
        let (url, stash) = rp.build_authorize_redirect(
            "/some/path",
            "https://myapp.zeroship.ai/__zs/auth/callback",
        );
        assert!(url.starts_with("https://auth.zeroship.ai/oauth2/auth?"));
        assert!(url.contains("client_id=gateway"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("code_challenge="));
        assert!(url.contains("state="));
        assert!(url.contains("nonce="));
        // `scope=openid offline_access email profile` URL-encoded
        // (form_urlencoded uses `+` for space).
        assert!(url.contains("scope=openid+offline_access+email+profile"));
        // redirect_uri URL-encoded.
        assert!(url.contains("redirect_uri=https%3A%2F%2Fmyapp.zeroship.ai%2F__zs%2Fauth%2Fcallback"));
        // Stash is non-empty and contains the dot-separator.
        assert!(stash.contains('.'));
    }

    #[test]
    fn authorize_url_trims_trailing_slash_on_auth_public() {
        let rp = OidcRp::new(
            "https://auth.zeroship.ai/",
            "gateway",
            "secret",
            b"k".repeat(32),
        );
        let (url, _) = rp.build_authorize_redirect("/", "https://app/cb");
        // No double slash before `/oauth2/auth`.
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
        assert_eq!(decoded.original_path, "/p");
        assert_eq!(decoded.redirect_uri, "https://x/cb");
    }

    #[test]
    fn stash_rejects_tampering() {
        let key = b"k".repeat(32);
        let stash = make_stash();
        // Flip the last character of the encoded cookie to corrupt the MAC.
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
        // No dot separator.
        assert!(Stash::decode("not-a-signed-cookie", &key).is_none());
        // Bad base64.
        assert!(Stash::decode("@@@.@@@", &key).is_none());
        // Empty.
        assert!(Stash::decode("", &key).is_none());
    }
}
