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
//! Wiring into the dispatch pipeline lives in U5 — the gateway's
//! dispatch handler calls `OidcRp::build_authorize_redirect` on
//! unauthenticated HTML requests and `OidcRp::finish_callback` from
//! the `/__zs/auth/callback` handler.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use zeroship_core::auth::hmac_sha256;
use zeroship_core::oidc_verify::{verify_id_token, JwksCache, OidcError, TokenClaims};
// `CachedKey` is referenced in `verify_access_jwt`'s closure type below.
use zeroship_core::pkce::{generate_verifier, s256_challenge};

/// A configured OIDC relying party for the gateway, parameterised by the
/// auth UI/OIDC upstream URL, the shared gateway client credentials, and
/// the signing key used to MAC the per-request `__Host-zs_oidc_stash`
/// cookie.
#[derive(Debug, Clone)]
pub struct OidcRp {
    /// Auth service UI base URL (e.g. `https://auth.zeroship.ai`).
    /// This is the URL the gateway actually dials for `/oauth2/auth`,
    /// `/oauth2/token`, and JWKS — it must be reachable from the gateway
    /// process. In tests it points at a loopback auth/Hydra surface; in
    /// prod it is the public DNS name.
    pub auth_ui_url: String,
    /// Expected `iss` claim in ID tokens issued by hydra. Per
    /// `ops/hydra.yaml::urls.self.issuer` hydra always emits its
    /// configured public URL with a trailing slash, regardless of which
    /// host the RP dialled to obtain the token. In tests this can be
    /// overridden separately from `auth_ui_url` (which may point at
    /// loopback) — see [`OidcRp::with_issuer`].
    pub issuer: String,
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
    /// Construct a new RP. Caller supplies the auth UI/OIDC upstream URL,
    /// the `OAuth2` client credentials registered for the gateway, and
    /// the stash signing key. The JWKS cache is derived from
    /// `auth_ui_url` by appending `/.well-known/jwks.json`. The expected
    /// ID-token `iss` defaults to `auth_ui_url` with a trailing slash;
    /// override via
    /// [`OidcRp::with_issuer`] when the dial-URL and issuer string
    /// differ (e.g. loopback hydra during tests).
    pub fn new(
        auth_ui_url: impl Into<String>,
        client_id: impl Into<String>,
        client_secret: impl Into<String>,
        stash_signing_key: impl Into<Vec<u8>>,
    ) -> Self {
        let auth_ui_url = auth_ui_url.into();
        let jwks_url = format!(
            "{}/.well-known/jwks.json",
            auth_ui_url.trim_end_matches('/')
        );
        let issuer = format!("{}/", auth_ui_url.trim_end_matches('/'));
        Self {
            auth_ui_url,
            issuer,
            client_id: client_id.into(),
            client_secret: client_secret.into(),
            jwks: Arc::new(JwksCache::new(jwks_url)),
            stash_signing_key: stash_signing_key.into(),
        }
    }

    /// Override the expected ID-token `iss` claim. Used when the
    /// network-reachable auth/Hydra URL (`auth_ui_url`) and the logical
    /// issuer hydra emits in ID tokens are not the same string — e.g.
    /// in integration tests against a loopback hydra that's configured
    /// with `urls.self.issuer: https://auth.zeroship.ai/`.
    #[must_use]
    pub fn with_issuer(mut self, issuer: impl Into<String>) -> Self {
        self.issuer = issuer.into();
        self
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
            self.auth_ui_url.trim_end_matches('/'),
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
            self.auth_ui_url.trim_end_matches('/')
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
            // SECURITY: a 2xx body from `/oauth2/token` contains the
            // access_token AND refresh_token in plaintext. NEVER embed it in
            // an error that surfaces at `tracing::warn!` — the refresh family
            // must never be logged (§8.1/§8.5). Redact to the parse error only.
            OidcRpError::TokenExchange(format!("parse: {e} (success body redacted)"))
        })?;

        // 4. Verify ID token. Hydra's issuer is whatever
        //    `urls.self.issuer` is set to in ops/hydra.yaml; by default
        //    we derive it from `auth_ui_url + "/"`, but the test suite
        //    (and any deployment where the dial URL differs from the
        //    logical issuer) overrides it via [`with_issuer`].
        let claims = verify_id_token(
            &self.jwks,
            &tr.id_token,
            &self.issuer,
            &self.client_id,
            Some(&stash.nonce),
            Some(&tr.access_token),
            Some(code),
        )
        .await?;

        Ok((claims, stash.original_path))
    }

    /// Introspect an access token at hydra's `/oauth2/introspect`
    /// endpoint (RFC 7662). Returns the parsed response; callers MUST
    /// check `.active` before trusting any other field — hydra emits a
    /// 200 with `{"active": false}` for revoked/expired/unknown tokens.
    ///
    /// Used by the gateway's DPoP-bound resource-server path: a worker
    /// request carrying `Authorization: DPoP <access_token>` triggers
    /// proof verification (`core::dpop::verify`) followed by this
    /// introspection call so the gateway can resolve `sub`/`email`
    /// from a hydra-issued opaque access token without a local JWT.
    ///
    /// # Errors
    ///
    /// [`OidcRpError::TokenExchange`] on transport error, non-2xx
    /// status, or JSON parse failure. (`active: false` is NOT an error
    /// — that's a valid response indicating the token isn't usable.)
    pub async fn introspect_token(
        &self,
        access_token: &str,
    ) -> Result<IntrospectionResponse, OidcRpError> {
        use base64::engine::general_purpose::STANDARD as B64;
        use base64::Engine as _;

        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("token", access_token)
            .finish();
        let url = format!(
            "{}/oauth2/introspect",
            self.auth_ui_url.trim_end_matches('/')
        );

        // Basic auth header — hydra requires confidential-client auth on
        // /oauth2/introspect regardless of the token's own client_id.
        let creds = format!("{}:{}", self.client_id, self.client_secret);
        let auth = format!("Basic {}", B64.encode(&creds));

        let client = cyper::Client::new();
        let resp = client
            .request(http::Method::POST, &url)
            .map_err(|e| OidcRpError::TokenExchange(format!("introspect build: {e}")))?
            .header("content-type", "application/x-www-form-urlencoded")
            .map_err(|e| OidcRpError::TokenExchange(format!("introspect ct: {e}")))?
            .header("authorization", &auth)
            .map_err(|e| OidcRpError::TokenExchange(format!("introspect auth: {e}")))?
            .body(body)
            .send()
            .await
            .map_err(|e| OidcRpError::TokenExchange(format!("introspect send: {e}")))?;

        let status = resp.status().as_u16();
        let resp_body = resp
            .text()
            .await
            .map_err(|e| OidcRpError::TokenExchange(format!("introspect read: {e}")))?;
        if !(200..300).contains(&status) {
            return Err(OidcRpError::TokenExchange(format!(
                "introspect HTTP {status}: {resp_body}"
            )));
        }
        serde_json::from_str(&resp_body).map_err(|e| {
            OidcRpError::TokenExchange(format!("introspect parse: {e}\nbody: {resp_body}"))
        })
    }

    /// Exchange an authorization code for tokens as a PUBLIC PKCE client
    /// (auth-sdk Slice 1b, `POST /__zs/auth/token`). Unlike
    /// [`OidcRp::finish_callback`] (the interactive cookie flow, which uses
    /// the gateway's confidential `client_secret`), the browser SDK is a
    /// public client: it sends `code` + `code_verifier`, and the gateway
    /// injects the per-app `client_id` (the browser never sends it). No
    /// `client_secret` — public clients authenticate by PKCE alone.
    ///
    /// Returns the full token set INCLUDING the `refresh_token` (the
    /// gateway keeps it server-side under the anchor in `server_anchor`
    /// mode; it is never returned to the browser).
    ///
    /// # Errors
    /// [`OidcRpError::TokenExchange`] on transport error, non-2xx status,
    /// or a JSON parse failure.
    pub async fn exchange_code_public(
        &self,
        client_id: &str,
        code: &str,
        code_verifier: &str,
        redirect_uri: &str,
    ) -> Result<TokenSet, OidcRpError> {
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("grant_type", "authorization_code")
            .append_pair("code", code)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("client_id", client_id)
            .append_pair("code_verifier", code_verifier)
            .finish();
        self.post_token(body).await
    }

    /// Rotate a refresh family as a PUBLIC PKCE client (the server-held
    /// `?mint=1` refresh, auth-sdk Slice 1b-anchors). Posts
    /// `grant_type=refresh_token` with the per-app `client_id` injected.
    /// Returns the rotated token set (new `access_token` + new
    /// `refresh_token`).
    ///
    /// # Errors
    /// [`OidcRpError::TokenExchange`] — its message contains the upstream
    /// status + body, so callers can detect Hydra `invalid_grant` (family
    /// revoked / expired / 720h ceiling) by substring and treat the anchor
    /// as dead.
    pub async fn refresh_token_public(
        &self,
        client_id: &str,
        refresh_token: &str,
    ) -> Result<TokenSet, OidcRpError> {
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("grant_type", "refresh_token")
            .append_pair("refresh_token", refresh_token)
            .append_pair("client_id", client_id)
            .finish();
        self.post_token(body).await
    }

    /// Shared `POST /oauth2/token` for the public-client grants above.
    async fn post_token(&self, body: String) -> Result<TokenSet, OidcRpError> {
        let client = cyper::Client::new();
        let token_url = format!("{}/oauth2/token", self.auth_ui_url.trim_end_matches('/'));
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
        serde_json::from_str(&resp_body).map_err(|e| {
            // SECURITY: redact — a 2xx `/oauth2/token` body carries the
            // access_token + refresh_token in plaintext and this error is
            // logged on the refresh path (§8.1/§8.5). Non-2xx bodies (Hydra
            // error JSON, no tokens) are surfaced above this branch.
            OidcRpError::TokenExchange(format!("parse: {e} (success body redacted)"))
        })
    }

    /// Verify a **raw Hydra access JWT** (RFC 9068) locally against the
    /// gateway's JWKS cache — no introspection round-trip. Used by the
    /// Bearer arm's raw-Hydra path (§1.3, slice 1c) for non-browser
    /// clients that hold a Hydra access token directly (CLI,
    /// server-to-server). The browser never takes this path — it holds a
    /// gateway *wrapper*, verified by `wrapper_token::Verifier` instead.
    ///
    /// Validation covers: signature (against any cached JWK matching the
    /// token's `kid`/`alg`), `iss == self.issuer`, and `exp` (with the
    /// jsonwebtoken default 60 s leeway). **`aud` is deliberately NOT
    /// validated here** — an access token's `aud` is the resource-server
    /// audience, not the OAuth client, so per-app binding is done by the
    /// caller on the `client_id` claim (RFC 9068 §3), with an `aud`
    /// fallback. We therefore disable jsonwebtoken's audience check and
    /// surface `aud` in the returned claims for the caller's fallback.
    ///
    /// **This function does NOT bind the token to any client.** It verifies
    /// only signature + `iss` + `exp` and returns the decoded claims. Per-app
    /// binding (`client_id` claim == route client, with an `aud`-contains
    /// fallback) is the CALLER's responsibility — it lives in the Bearer arm
    /// because it needs the `aud` fallback, which depends on the normalized
    /// `aud` list this function surfaces. A caller that skips the
    /// caller-side binding check silently opens cross-app replay; that is
    /// why no `expected_client_id` parameter is accepted here (it would
    /// falsely imply this function enforces binding).
    ///
    /// # Errors
    ///
    /// [`OidcRpError::VerifyIdToken`] wrapping an [`OidcError`] for any
    /// JWKS/signature/iss/exp failure. Callers translate this into a
    /// `401` (User/Admin route) or fall-through to anonymous (Anon
    /// route), per the Bearer-arm policy gate.
    pub async fn verify_access_token(&self, token: &str) -> Result<AccessClaims, OidcRpError> {
        let claims = verify_access_jwt(&self.jwks, token, &self.issuer).await?;
        Ok(claims)
    }
}

/// Decoded claims of a raw Hydra access JWT (RFC 9068). The Bearer arm
/// reads `client_id`/`aud` for per-app binding, `sub` for identity, and
/// the profile fields for the `ZeroShip-User` header.
///
/// `aud` per RFC 7519 may be a string OR an array of strings; the helper
/// normalizes both into a `Vec<String>` so the caller's `aud`-fallback
/// binding (`aud` contains the expected `client_id`) is uniform.
#[derive(Debug, Clone)]
pub struct AccessClaims {
    /// Subject — the **global** Hydra UUID (`usr_…`). On the raw-Hydra
    /// Bearer path Slice 4 projects this to a per-app `pws_`; Slice 1c
    /// uses it directly (no pairwise derivation yet).
    pub sub: String,
    /// The OAuth `client_id` claim, when present (RFC 9068 §3 mandates
    /// it; Hydra emits it). The Bearer arm's primary per-app binding.
    pub client_id: Option<String>,
    /// The token audience(s), normalized to a list. The Bearer arm's
    /// fallback binding (when `client_id` is absent) checks whether this
    /// list contains the expected `client_id`.
    pub aud: Vec<String>,
    /// Issued-at (UNIX seconds) — used by the revocation family marker.
    pub iat: i64,
    pub email: Option<String>,
    pub email_verified: Option<bool>,
    pub name: Option<String>,
    pub scope: Option<String>,
}

/// Wire shape of a Hydra access JWT we deserialize. `aud` is a raw
/// `serde_json::Value` so we accept both the string and array forms.
#[derive(Deserialize)]
struct RawAccessClaims {
    sub: String,
    iss: String,
    #[serde(default)]
    aud: serde_json::Value,
    #[serde(default)]
    exp: Option<i64>,
    #[serde(default)]
    iat: i64,
    #[serde(default)]
    client_id: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    email_verified: Option<bool>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    scope: Option<String>,
}

/// Verify a raw Hydra access JWT against `cache`, pinning `iss` and
/// `exp` but NOT `aud` (see [`OidcRp::verify_access_token`] rationale).
/// On a first verify failure (likely a rotated JWKS) the cache is
/// force-refreshed and verification retried once — mirroring
/// [`zeroship_core::oidc_verify::verify_id_token`].
async fn verify_access_jwt(
    cache: &JwksCache,
    token: &str,
    expected_iss: &str,
) -> Result<AccessClaims, OidcRpError> {
    use jsonwebtoken::{decode, decode_header, Validation};

    let header = decode_header(token).map_err(|e| {
        OidcRpError::VerifyIdToken(OidcError::DecodeHeader(e.to_string()))
    })?;
    let kid = header
        .kid
        .clone()
        .ok_or_else(|| OidcRpError::VerifyIdToken(OidcError::DecodeHeader("no kid".into())))?;
    let alg = header.alg;

    let try_verify = |keys: Vec<zeroship_core::oidc_verify::CachedKey>| -> Result<RawAccessClaims, OidcError> {
        let key = keys
            .iter()
            .find(|k| k.kid == kid && k.alg == alg)
            .ok_or_else(|| OidcError::NoMatchingKey(kid.clone()))?;
        let mut validation = Validation::new(alg);
        validation.set_issuer(&[expected_iss]);
        // Access-token `aud` is the resource-server audience, NOT the
        // OAuth client — so we do NOT pin it here. Per-app binding is on
        // `client_id` (with an `aud` fallback) in the Bearer arm.
        validation.validate_aud = false;
        // `validate_exp` is on by default (60 s leeway).
        let data: jsonwebtoken::TokenData<RawAccessClaims> =
            decode(token, &key.decoding, &validation)
                .map_err(|e| OidcError::Verify(e.to_string()))?;
        Ok(data.claims)
    };

    let raw = if let Ok(c) = try_verify(cache.keys().await.map_err(OidcRpError::VerifyIdToken)?) {
        c
    } else {
        // Likely cause: JWKS rotated. Force-refresh once and retry.
        cache.refresh().await.map_err(OidcRpError::VerifyIdToken)?;
        try_verify(cache.keys().await.map_err(OidcRpError::VerifyIdToken)?)
            .map_err(OidcRpError::VerifyIdToken)?
    };

    // Defense-in-depth iss re-check (jsonwebtoken checked it above).
    if raw.iss != expected_iss {
        return Err(OidcRpError::VerifyIdToken(OidcError::IssuerMismatch {
            expected: expected_iss.into(),
            got: raw.iss,
        }));
    }
    let _ = raw.exp; // exp enforced by jsonwebtoken; surfaced for clarity only.

    let aud = match raw.aud {
        serde_json::Value::String(s) => vec![s],
        serde_json::Value::Array(items) => items
            .into_iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    };

    Ok(AccessClaims {
        sub: raw.sub,
        client_id: raw.client_id,
        aud,
        iat: raw.iat,
        email: raw.email,
        email_verified: raw.email_verified,
        name: raw.name,
        scope: raw.scope,
    })
}

/// Subset of an RFC 7662 introspection response (hydra's
/// `/oauth2/introspect`). Only `active` is mandatory; everything else
/// is `Option` because hydra omits fields when the token is inactive
/// or when no value is bound. Callers MUST gate on `.active` before
/// reading any other field.
#[derive(Debug, Clone, Deserialize)]
pub struct IntrospectionResponse {
    /// `true` iff the access token is currently valid (not revoked, not
    /// expired, recognised by the AS).
    pub active: bool,
    /// Subject (user id) the access token represents.
    #[serde(default)]
    pub sub: Option<String>,
    /// `OAuth2` `client_id` the token was issued to.
    #[serde(default)]
    pub client_id: Option<String>,
    /// User's email address (when the `email` scope was granted).
    #[serde(default)]
    pub email: Option<String>,
    /// Whether the user's email is verified at the `IdP`.
    #[serde(default)]
    pub email_verified: Option<bool>,
    /// User's display name (when the `profile` scope was granted).
    #[serde(default)]
    pub name: Option<String>,
    /// Space-separated list of granted scopes.
    #[serde(default)]
    pub scope: Option<String>,
    /// Absolute expiry (UNIX seconds).
    #[serde(default)]
    pub exp: Option<i64>,
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

/// Public token set returned by [`OidcRp::exchange_code_public`] /
/// [`OidcRp::refresh_token_public`] (auth-sdk Slice 1b). `id_token` is
/// `Option` because a `refresh_token` grant does not always re-issue one.
#[derive(Debug, Clone, Deserialize)]
pub struct TokenSet {
    pub access_token: String,
    #[serde(default)]
    pub id_token: Option<String>,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub expires_in: Option<i64>,
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

// ─── Cookie helpers ──────────────────────────────────────────────────────
//
// Two cookies live on the per-app origin (`{app}.zeroship.ai`):
//
// - `__Host-zs_app_session` — opaque session id minted after a successful
//   OIDC dance. 12 h max-age, set on `/__zs/auth/callback`, cleared on
//   `/__zs/auth/logout`. The gateway looks this up server-side to resolve
//   `ZeroShip-User` on every request.
// - `__Host-zs_oidc_stash` — the signed PKCE+state stash. 10 min max-age,
//   set on the redirect to hydra, cleared on callback.
//
// Both use the `__Host-` prefix which RFC 6265bis (§4.1.3) requires
// `Path=/`, no `Domain=`, and `Secure`. RFC 6265bis §4.1.3.2 makes
// `Secure` non-optional for `__Host-`; compliant clients silently reject
// `__Host-` cookies missing `Secure`. Dev mode (HTTP localhost) therefore
// drops `Secure` AND the prefix together — without that the cookie never
// makes the round-trip and downstream double-submit / session lookup
// fails with "invalid request".

/// Production app session cookie name (`__Host-` prefix → Secure required).
pub const APP_SESSION_COOKIE_PROD: &str = "__Host-zs_app_session";
/// Dev app session cookie name (no `__Host-` prefix).
pub const APP_SESSION_COOKIE_DEV: &str = "zs_app_session";

/// 12-hour absolute lifetime for the app session cookie.
pub const APP_SESSION_MAX_AGE_SECS: i64 = 12 * 3600;

/// Resolve the app session cookie name for the current environment.
#[must_use]
pub fn app_session_cookie_name(insecure_dev: bool) -> &'static str {
    if insecure_dev { APP_SESSION_COOKIE_DEV } else { APP_SESSION_COOKIE_PROD }
}

/// Build the `Set-Cookie` header value for the per-app session.
///
/// `insecure_dev = true` drops the `Secure` flag AND the `__Host-`
/// prefix (RFC 6265bis §4.1.3.2 — `__Host-` requires Secure).
#[must_use]
pub fn set_app_session_cookie(session_id: &uuid::Uuid, insecure_dev: bool) -> String {
    let name = app_session_cookie_name(insecure_dev);
    let secure = if insecure_dev { "" } else { "; Secure" };
    format!(
        "{name}={session_id}; Path=/; HttpOnly; SameSite=Lax{secure}; Max-Age={APP_SESSION_MAX_AGE_SECS}"
    )
}

/// Clear the per-app session cookie on logout.
#[must_use]
pub fn clear_app_session_cookie(insecure_dev: bool) -> String {
    let name = app_session_cookie_name(insecure_dev);
    let secure = if insecure_dev { "" } else { "; Secure" };
    format!("{name}=; Path=/; HttpOnly; SameSite=Lax{secure}; Max-Age=0")
}

/// Parse the app session UUID from a `Cookie` header value.
#[must_use]
pub fn parse_app_session_cookie(cookie_header: &str, insecure_dev: bool) -> Option<uuid::Uuid> {
    let name = app_session_cookie_name(insecure_dev);
    let prefix = format!("{name}=");
    for part in cookie_header.split(';') {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix(&prefix) {
            return uuid::Uuid::parse_str(rest).ok();
        }
    }
    None
}

/// Production stash cookie name (`__Host-` prefix → Secure required).
pub const STASH_COOKIE_PROD: &str = "__Host-zs_oidc_stash";
/// Dev stash cookie name (no `__Host-` prefix).
pub const STASH_COOKIE_DEV: &str = "zs_oidc_stash";

/// 10-minute window for the OIDC dance to complete. After this the user
/// has to re-initiate.
pub const STASH_MAX_AGE_SECS: i64 = 600;

/// Resolve the stash cookie name for the current environment.
#[must_use]
pub fn stash_cookie_name(insecure_dev: bool) -> &'static str {
    if insecure_dev { STASH_COOKIE_DEV } else { STASH_COOKIE_PROD }
}

/// Build the `Set-Cookie` header value for the OIDC stash.
#[must_use]
pub fn set_stash_cookie(value: &str, insecure_dev: bool) -> String {
    let name = stash_cookie_name(insecure_dev);
    let secure = if insecure_dev { "" } else { "; Secure" };
    format!(
        "{name}={value}; Path=/; HttpOnly; SameSite=Lax{secure}; Max-Age={STASH_MAX_AGE_SECS}"
    )
}

/// Clear the OIDC stash cookie. Set on the callback response so the
/// short-lived stash doesn't linger after the dance completes.
#[must_use]
pub fn clear_stash_cookie(insecure_dev: bool) -> String {
    let name = stash_cookie_name(insecure_dev);
    let secure = if insecure_dev { "" } else { "; Secure" };
    format!("{name}=; Path=/; HttpOnly; SameSite=Lax{secure}; Max-Age=0")
}

/// Parse the raw stash value out of a `Cookie` header. Returns the
/// signed-blob string; pass it to `Stash::decode` (via
/// `OidcRp::finish_callback`) to verify and recover the payload.
#[must_use]
pub fn parse_stash_cookie(cookie_header: &str, insecure_dev: bool) -> Option<String> {
    let name = stash_cookie_name(insecure_dev);
    let prefix = format!("{name}=");
    for part in cookie_header.split(';') {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix(&prefix) {
            return Some(rest.to_string());
        }
    }
    None
}

// ─── Worker header encoding ──────────────────────────────────────────────
//
// Post-callback the gateway resolves the per-request `ZeroShip-User`
// header from the app-session row (not from the ID token directly).
// The payload shape and HMAC envelope are the canonical wire format;
// the worker MAC-verifies and deserializes `WorkerUser` from it.

/// Public user shape forwarded to the worker as the JSON body of the
/// `ZeroShip-User` header. JWT internals (`sub` rename, `app`/`exp`
/// stripping) live elsewhere; this struct is what the worker actually
/// deserializes after MAC verification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerUser<'a> {
    pub id: &'a str,
    pub email: &'a str,
    pub name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avatar: Option<&'a str>,
    pub email_verified: bool,
}

/// Serialize the authenticated user as
/// `base64(JSON).<request_id>.<iat>.<hex-hmac>` for the `ZeroShip-User`
/// header. The worker decodes the base64 portion and verifies the HMAC
/// against the same `worker_key` before trusting the identity.
///
/// Signing prevents a caller with direct network access to the worker
/// from forging a user identity, even if the worker's endpoint bearer-auth
/// were ever bypassed. The request id and timestamp binding limit replay if
/// a header leaks through logs or a proxy.
#[must_use]
pub fn encode_user_header(
    user: &WorkerUser<'_>,
    worker_key: &str,
    request_id: uuid::Uuid,
) -> String {
    let json = serde_json::to_string(user).unwrap_or_default();
    zeroship_core::auth::sign_zeroship_user_header(
        worker_key.as_bytes(),
        json.as_bytes(),
        request_id,
    )
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
    fn new_derives_issuer_from_auth_ui_url_with_trailing_slash() {
        // Default — issuer is auth_ui_url with one trailing slash, matching
        // hydra's `urls.self.issuer` shape.
        let rp = OidcRp::new(
            "https://auth.zeroship.ai",
            "gateway",
            "secret",
            b"k".repeat(32),
        );
        assert_eq!(rp.issuer, "https://auth.zeroship.ai/");

        // Trimming is idempotent — trailing slash on auth_ui_url must not
        // produce `//`.
        let rp = OidcRp::new(
            "https://auth.zeroship.ai/",
            "gateway",
            "secret",
            b"k".repeat(32),
        );
        assert_eq!(rp.issuer, "https://auth.zeroship.ai/");
    }

    #[test]
    fn with_issuer_overrides_default_iss() {
        // Tests dial loopback hydra but expect the canonical hydra
        // issuer string — `with_issuer` decouples the two.
        let rp = OidcRp::new(
            "http://127.0.0.1:4444",
            "gateway",
            "secret",
            b"k".repeat(32),
        )
        .with_issuer("https://auth.zeroship.ai/");
        // `auth_ui_url` still drives /oauth2/token + JWKS (loopback).
        assert_eq!(rp.auth_ui_url, "http://127.0.0.1:4444");
        // `issuer` is the logical hydra issuer that ID tokens carry.
        assert_eq!(rp.issuer, "https://auth.zeroship.ai/");
    }

    #[test]
    fn authorize_url_trims_trailing_slash_on_auth_ui_url() {
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

    // ─── App session cookie ─────────────────────────────────────────

    #[test]
    fn app_session_set_cookie_has_secure_in_prod() {
        let id = uuid::Uuid::new_v4();
        let c = set_app_session_cookie(&id, false);
        assert!(c.starts_with("__Host-zs_app_session="));
        assert!(c.contains(&id.to_string()));
        assert!(c.contains("Path=/"));
        assert!(c.contains("HttpOnly"));
        assert!(c.contains("SameSite=Lax"));
        assert!(c.contains("Secure"));
        assert!(c.contains("Max-Age=43200")); // 12h
    }

    #[test]
    fn app_session_set_cookie_drops_secure_and_host_prefix_in_dev() {
        // RFC 6265bis §4.1.3.2: __Host- cookies require Secure. Dev
        // runs over plain HTTP without Secure, so the prefix MUST be
        // dropped too — otherwise compliant clients silently reject
        // the cookie.
        let id = uuid::Uuid::new_v4();
        let c = set_app_session_cookie(&id, true);
        assert!(!c.starts_with("__Host-"), "dev cookie must NOT use __Host- prefix: {c}");
        assert!(c.starts_with("zs_app_session="), "dev cookie name: {c}");
        assert!(!c.contains("Secure"), "dev cookie must NOT have Secure: {c}");
        assert!(c.contains("HttpOnly"));
        assert!(c.contains("SameSite=Lax"));
    }

    #[test]
    fn app_session_clear_cookie_zero_max_age() {
        let c = clear_app_session_cookie(false);
        assert!(c.contains("Max-Age=0"));
        assert!(c.contains("Secure"));
        let dev = clear_app_session_cookie(true);
        assert!(!dev.contains("Secure"));
        assert!(!dev.starts_with("__Host-"));
    }

    #[test]
    fn app_session_parse_cookie_roundtrips() {
        let id = uuid::Uuid::new_v4();
        let header = format!("foo=bar; __Host-zs_app_session={id}; baz=qux");
        assert_eq!(parse_app_session_cookie(&header, false), Some(id));
        assert_eq!(parse_app_session_cookie("nothing-here", false), None);
        assert_eq!(parse_app_session_cookie("__Host-zs_app_session=not-a-uuid", false), None);

        // Dev mode reads the bare-name cookie.
        let dev_header = format!("zs_app_session={id}");
        assert_eq!(parse_app_session_cookie(&dev_header, true), Some(id));
        assert_eq!(parse_app_session_cookie(&header, true), None);
    }

    // ─── Stash cookie ───────────────────────────────────────────────

    #[test]
    fn stash_set_cookie_has_secure_in_prod() {
        let c = set_stash_cookie("payload.signed", false);
        assert!(c.starts_with("__Host-zs_oidc_stash=payload.signed"));
        assert!(c.contains("Path=/"));
        assert!(c.contains("HttpOnly"));
        assert!(c.contains("SameSite=Lax"));
        assert!(c.contains("Secure"));
        assert!(c.contains("Max-Age=600")); // 10 min
    }

    #[test]
    fn stash_set_cookie_drops_secure_and_host_prefix_in_dev() {
        let c = set_stash_cookie("v", true);
        assert!(!c.starts_with("__Host-"), "dev cookie must NOT use __Host- prefix: {c}");
        assert!(c.starts_with("zs_oidc_stash=v"));
        assert!(!c.contains("Secure"));
    }

    #[test]
    fn stash_clear_cookie_zero_max_age() {
        let c = clear_stash_cookie(false);
        assert!(c.contains("Max-Age=0"));
        assert!(c.contains("Secure"));
        let dev = clear_stash_cookie(true);
        assert!(!dev.contains("Secure"));
        assert!(!dev.starts_with("__Host-"));
    }

    #[test]
    fn stash_parse_cookie_roundtrips() {
        let header = "foo=bar; __Host-zs_oidc_stash=abc.def; baz=qux";
        assert_eq!(parse_stash_cookie(header, false), Some("abc.def".into()));
        assert_eq!(parse_stash_cookie("nothing", false), None);

        let dev_header = "zs_oidc_stash=abc.def";
        assert_eq!(parse_stash_cookie(dev_header, true), Some("abc.def".into()));
        // Prod-named cookie must not match in dev mode.
        assert_eq!(parse_stash_cookie(header, true), None);
    }
}
