//! OIDC Relying Party module for the gateway.
//!
//! Each hosted creator app on `{app}.zeroship.ai` has a per-app `oac_` OAuth
//! client. The gateway brokers the authorize-redirect dance and code exchange
//! on the creator app's behalf, deriving the per-app broker secret from the
//! platform broker master for token endpoint authentication. After a successful
//! exchange the gateway mints its own per-origin app session cookie
//! (`__Host-zeroship_app_session`) — the OP ID token never reaches the creator
//! app or the browser.
//!
//! Wiring into the dispatch pipeline lives in U5 — the gateway's
//! dispatch handler calls `OidcRp::build_authorize_redirect` on
//! unauthenticated HTML requests and `OidcRp::finish_callback` from
//! the `/__zeroship/auth/callback` handler.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use zeroship_core::auth::{derive_broker_secret, hmac_sha256, validate_broker_master};
use zeroship_core::oidc_verify::{verify_id_token, JwksCache, OidcError, TokenClaims};
// `CachedKey` is referenced in `verify_access_jwt`'s closure type below.
use zeroship_core::pkce::{generate_verifier, s256_challenge};

/// Validated platform broker master secret. `Debug` deliberately redacts the
/// raw bytes because `OidcRp` is frequently held inside wider gateway state.
#[derive(Clone)]
pub struct BrokerSecret(Vec<u8>);

impl std::fmt::Debug for BrokerSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("BrokerSecret").field(&"<redacted>").finish()
    }
}

impl BrokerSecret {
    /// Validate and store raw broker master-secret bytes.
    ///
    /// The bytes must be read exactly the same way by auth and gateway; do not
    /// trim or normalize the file contents here.
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self, String> {
        validate_broker_master(&bytes)?;
        Ok(Self(bytes))
    }

    fn derive_client_secret(&self, client_id: &str) -> String {
        derive_broker_secret(&self.0, client_id)
    }
}

/// A configured OIDC relying party for the gateway, parameterised by the
/// auth UI/OIDC upstream URL, the broker master secret used to derive per-app
/// client secrets, and the signing key used to MAC the per-request
/// `__Host-zs_oidc_stash` cookie.
#[derive(Debug, Clone)]
pub struct OidcRp {
    /// Auth service UI base URL (e.g. `https://auth.zeroship.ai`).
    /// This is the URL the gateway actually dials for `/authorize`,
    /// `/token`, and JWKS — it must be reachable from the gateway
    /// process. In tests it points at a loopback auth/Hydra surface; in
    /// prod it is the public DNS name.
    pub auth_ui_url: String,
    /// Expected `iss` claim in ID tokens issued by the platform OP. Defaults
    /// to `auth_ui_url` without a trailing slash, matching the OP's issuer
    /// stamp. In tests this can be overridden separately from `auth_ui_url`
    /// (which may point at loopback) — see [`OidcRp::with_issuer`].
    pub issuer: String,
    /// Platform broker master secret used to derive per-app client secrets for
    /// brokered `oac_` clients.
    broker_secret: BrokerSecret,
    /// JWKS cache — shared with other RPs in the same gateway process.
    pub jwks: Arc<JwksCache>,
    /// HMAC-SHA256 key used to sign the `__Host-zs_oidc_stash` cookie body.
    /// Must be at least 32 random bytes in prod.
    pub stash_signing_key: Vec<u8>,
    /// Shared circuit breaker for ALL outbound OP token/revoke calls
    /// (auth-sdk §8.7, round-6 MAJOR #3). `Arc`-shared so an OP brownout
    /// observed on one ntex worker thread trips the breaker for every thread —
    /// the reused `cyper::Client` itself is per-worker-thread (`!Send` in
    /// practice; see [`crate::hydra_client`]), but the breaker state is
    /// one-per-process. When open, every OP call fast-fails with
    /// `HydraError::Open` (→ `503 upstream_unavailable`) instead of opening a
    /// fresh connection into the brownout.
    pub breaker: Arc<crate::hydra_client::CircuitBreaker>,
    /// Bounded per-call timeout for every outbound OP request. A hung OP
    /// returns a fast `HydraError::Timeout` (counted as a breaker
    /// failure) rather than an unbounded await pinning a connection.
    pub hydra_timeout: std::time::Duration,
}

impl OidcRp {
    /// Construct a new RP. Caller supplies the auth UI/OIDC upstream URL, the
    /// validated platform broker secret, and the stash signing key. The JWKS
    /// cache is derived from `auth_ui_url` by appending
    /// `/.well-known/jwks.json`. The expected ID-token `iss` defaults to
    /// `auth_ui_url` without a trailing slash; override via
    /// [`OidcRp::with_issuer`] when the dial-URL and issuer string differ
    /// (e.g. loopback tests).
    pub fn new(
        auth_ui_url: impl Into<String>,
        broker_secret: BrokerSecret,
        stash_signing_key: impl Into<Vec<u8>>,
    ) -> Self {
        let auth_ui_url = auth_ui_url.into();
        let jwks_url = format!(
            "{}/.well-known/jwks.json",
            auth_ui_url.trim_end_matches('/')
        );
        let issuer = auth_ui_url.trim_end_matches('/').to_string();
        Self {
            auth_ui_url,
            issuer,
            broker_secret,
            jwks: Arc::new(JwksCache::new(jwks_url)),
            stash_signing_key: stash_signing_key.into(),
            breaker: Arc::new(crate::hydra_client::CircuitBreaker::default()),
            hydra_timeout: crate::hydra_client::DEFAULT_HYDRA_TIMEOUT,
        }
    }

    /// Override the shared circuit breaker (tests inject a fast-tripping
    /// breaker; production uses the [`CircuitBreaker::default`] from
    /// [`OidcRp::new`]).
    ///
    /// [`CircuitBreaker::default`]: crate::hydra_client::CircuitBreaker::default
    #[must_use]
    pub fn with_breaker(mut self, breaker: Arc<crate::hydra_client::CircuitBreaker>) -> Self {
        self.breaker = breaker;
        self
    }

    /// Override the bounded per-call Hydra timeout (tests use a short value
    /// to exercise the timeout→breaker-failure path quickly).
    #[must_use]
    pub fn with_hydra_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.hydra_timeout = timeout;
        self
    }

    /// Override the expected ID-token `iss` claim. Used when the
    /// network-reachable auth/OP URL (`auth_ui_url`) and the logical issuer the
    /// OP emits in ID tokens are not the same string.
    #[must_use]
    pub fn with_issuer(mut self, issuer: impl Into<String>) -> Self {
        self.issuer = issuer.into();
        self
    }

    /// Build the `/authorize` redirect URL + the signed stash cookie
    /// body. `original_path` is the request path the user was trying to
    /// reach; `redirect_uri` is the per-app callback URL the worker
    /// registered with hydra (e.g.
    /// `https://myapp.zeroship.ai/__zeroship/auth/callback`).
    ///
    /// Returns `(authorize_url, stash_cookie_value)`. Caller wraps the
    /// cookie value with [`set_stash_cookie`] before setting it on the
    /// response.
    #[must_use]
    pub fn build_authorize_redirect(
        &self,
        client_id: &str,
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
            client_id: client_id.to_string(),
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
        q.append_pair("client_id", client_id);
        q.append_pair("response_type", "code");
        q.append_pair("scope", "openid offline_access email profile");
        q.append_pair("redirect_uri", redirect_uri);
        q.append_pair("state", &state);
        q.append_pair("nonce", &nonce);
        q.append_pair("code_challenge", &challenge);
        q.append_pair("code_challenge_method", "S256");
        let query = q.finish();

        let url = format!(
            "{}/authorize?{}",
            self.auth_ui_url.trim_end_matches('/'),
            query
        );

        (url, stash_value)
    }

    /// Process the callback. Verifies the stash cookie signature, matches
    /// the `state` parameter, exchanges the code for tokens, verifies the
    /// ID token, and returns `(claims, original_path, granted_scopes)` on
    /// success. `granted_scopes` is the token endpoint's `scope` response
    /// (the scopes the user actually consented to), persisted onto the cookie
    /// session row so the per-request cookie path can emit `WorkerUser.scopes`
    /// (Slice 3, §1.4).
    ///
    /// # Errors
    /// - [`OidcRpError::StashInvalid`] — stash cookie absent, malformed,
    ///   or tampered.
    /// - [`OidcRpError::StateMismatch`] — `state` query param does not
    ///   equal the value stashed when the dance started (CSRF guard).
    /// - [`OidcRpError::TokenExchange`] — the OP rejected the code or
    ///   network failure on `/token`.
    /// - [`OidcRpError::VerifyIdToken`] — ID token signature, issuer,
    ///   audience, `exp`, or `nonce` check failed.
    pub async fn finish_callback(
        &self,
        code: &str,
        state_param: &str,
        stash_cookie: &str,
    ) -> Result<(TokenClaims, String, Vec<String>), OidcRpError> {
        // 1. Decode + verify stash cookie.
        let stash = Stash::decode(stash_cookie, &self.stash_signing_key)
            .ok_or(OidcRpError::StashInvalid)?;

        // 2. State match (constant-time not strictly required since the
        //    server-side stash is already the source of truth, but compare
        //    by value — mismatch → reject).
        if state_param != stash.state {
            return Err(OidcRpError::StateMismatch);
        }

        // 3. POST /token with the code + PKCE verifier + brokered client auth.
        let client_secret = self.broker_secret.derive_client_secret(&stash.client_id);
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("grant_type", "authorization_code")
            .append_pair("code", code)
            .append_pair("redirect_uri", &stash.redirect_uri)
            .append_pair("client_id", &stash.client_id)
            .append_pair("client_secret", &client_secret)
            .append_pair("code_verifier", &stash.verifier)
            .finish();

        let token_url = format!(
            "{}/token",
            self.auth_ui_url.trim_end_matches('/')
        );
        // Reused, breaker-guarded, bounded-timeout client (§8.7). A failed
        // build is a programming error (bad URL), not a transport failure, so
        // it never reaches the breaker; the `send()` await is what the breaker
        // and timeout wrap.
        let resp = crate::hydra_client::call(&self.breaker, self.hydra_timeout, |client| async move {
            client
                .request(http::Method::POST, &token_url)?
                .header("content-type", "application/x-www-form-urlencoded")?
                .body(body)
                .send()
                .await
        })
        .await
        .map_err(|e| OidcRpError::from_hydra(e, "token"))?;

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
            // SECURITY: a 2xx body from `/token` contains the
            // access_token AND refresh_token in plaintext. NEVER embed it in
            // an error that surfaces at `tracing::warn!` — the refresh family
            // must never be logged (§8.1/§8.5). Redact to the parse error only.
            OidcRpError::TokenExchange(format!("parse: {e} (success body redacted)"))
        })?;

        // 4. Verify ID token. The OP stamps `aud` with the per-app `oac_`
        //    client id stashed at authorize time.
        let claims = verify_id_token(
            &self.jwks,
            &tr.id_token,
            &self.issuer,
            &stash.client_id,
            Some(&stash.nonce),
            Some(&tr.access_token),
            Some(code),
        )
        .await?;

        // Granted scopes — what the user actually consented to (Slice 3, §1.4).
        // Persisted onto the cookie session so the per-request path emits
        // WorkerUser.scopes with no token to decode in the browser.
        //
        // Primary source: the token endpoint's `scope` response. But RFC 6749
        // §5.1 makes that parameter OPTIONAL when the granted scope equals the
        // requested scope, so Hydra may omit it on a no-narrowing consent. When
        // it is absent/empty we MUST fall back to the always-present `scope`
        // claim of the access token — decoded through the SAME JWKS-verified
        // path the Bearer/raw-Hydra arms use, so the cookie arm records the same
        // authoritative, non-spoofable scope set as every other arm (instead of
        // silently persisting `[]`).
        let access_scope = match tr.scope.as_deref() {
            // Token-response `scope` present and non-blank — authoritative, no
            // need to decode the access token at all.
            Some(s) if !s.trim().is_empty() => None,
            _ => match verify_access_jwt(&self.jwks, &tr.access_token, &self.issuer).await {
                Ok(ac) => ac.scope,
                Err(e) => {
                    // The access token is a Hydra-issued RFC 9068 JWT here, so a
                    // verify failure is unexpected; log and fall through to an
                    // empty scope set rather than failing the whole login.
                    tracing::warn!(
                        error = %e,
                        "gateway oidc_rp: could not decode access-token scope claim for cookie session; recording no granted scopes"
                    );
                    None
                }
            },
        };
        let granted_scopes =
            resolve_granted_scopes(tr.scope.as_deref(), access_scope.as_deref());

        Ok((claims, stash.original_path, granted_scopes))
    }

    /// Exchange an authorization code for tokens as a brokered PKCE client
    /// (auth-sdk Slice 1b, `POST /__zeroship/auth/token`). Unlike
    /// [`OidcRp::finish_callback`] (the interactive cookie flow), the browser
    /// SDK holds the PKCE verifier, but the gateway still authenticates the
    /// per-app `oac_` client by deriving its broker secret. The browser never
    /// sees or sends the `client_secret`.
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
        let client_secret = self.broker_secret.derive_client_secret(client_id);
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("grant_type", "authorization_code")
            .append_pair("code", code)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("client_id", client_id)
            .append_pair("client_secret", &client_secret)
            .append_pair("code_verifier", code_verifier)
            .finish();
        self.post_token(body).await
    }

    /// Rotate a refresh family as a brokered PKCE client (the server-held
    /// `?mint=1` refresh, auth-sdk Slice 1b-anchors). Posts
    /// `grant_type=refresh_token` with the per-app `client_id` and derived
    /// broker secret injected.
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
        let client_secret = self.broker_secret.derive_client_secret(client_id);
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("grant_type", "refresh_token")
            .append_pair("refresh_token", refresh_token)
            .append_pair("client_id", client_id)
            .append_pair("client_secret", &client_secret)
            .finish();
        self.post_token(body).await
    }

    /// Build the OP `/authorize` URL for the browser PKCE flow
    /// (auth-sdk Slice 1b-browser, `GET /__zeroship/auth/authorize`, spec §1.2).
    ///
    /// Unlike [`OidcRp::build_authorize_redirect`] (the interactive cookie
    /// flow, which generates the PKCE verifier/state/nonce server-side and
    /// stashes the verifier in a signed cookie), this is the SUPABASE-style
    /// public-client flow: **the browser holds the PKCE verifier**, so the
    /// gateway is given the already-computed `code_challenge` (S256), the
    /// browser-chosen `state` + `nonce`, the requested `scope`, and the
    /// per-app `redirect_uri`. The gateway holds NO server-side state — it
    /// just assembles the redirect. `prompt` is a PASSTHROUGH (omitted in
    /// the common interactive-popup case so Hydra's SSO skip fires; `login`
    /// / `consent` only on explicit step-up — `none` is not supported here,
    /// the silent-iframe path having been removed, but the gateway does not
    /// reject it: the caller validates `prompt` before calling).
    ///
    /// `client_id` is the per-app PUBLIC client (`route.oauth_client_id`),
    /// injected by the gateway (the browser never supplies it).
    #[must_use]
    pub fn build_browser_authorize_url(&self, client_id: &str, p: &BrowserAuthorizeParams<'_>) -> String {
        let mut q = url::form_urlencoded::Serializer::new(String::new());
        q.append_pair("client_id", client_id);
        q.append_pair("response_type", "code");
        q.append_pair("scope", p.scope);
        q.append_pair("redirect_uri", p.redirect_uri);
        q.append_pair("state", p.state);
        q.append_pair("nonce", p.nonce);
        q.append_pair("code_challenge", p.code_challenge);
        q.append_pair("code_challenge_method", "S256");
        // `prompt` is optional — omitted in the common case so Hydra's
        // SSO/`remember` skip path fires (spec §1.2 round-2). Passthrough
        // when the browser explicitly asks for `login`/`consent` step-up.
        if let Some(prompt) = p.prompt {
            if !prompt.is_empty() {
                q.append_pair("prompt", prompt);
            }
        }
        // `idp_hint` is optional — passed through to Hydra so the login UI can
        // route to / pre-select the named upstream IdP. Omitted ⇒ default picker.
        if let Some(idp_hint) = p.idp_hint {
            if !idp_hint.is_empty() {
                q.append_pair("idp_hint", idp_hint);
            }
        }
        let query = q.finish();
        format!(
            "{}/authorize?{}",
            self.auth_ui_url.trim_end_matches('/'),
            query
        )
    }

    /// Best-effort revoke a token (refresh family) at the OP's RFC 7009
    /// `/revoke` endpoint as a brokered client (auth-sdk Slice 1b-browser,
    /// `POST /__zeroship/auth/signout`, spec §1.2). The per-app `client_id`
    /// and derived broker secret are sent so the OP scopes the revoke to this
    /// client's family. `token_type_hint=refresh_token`
    /// because signout revokes the server-held refresh family.
    ///
    /// RFC 7009 §2.2: the AS returns `200` even for an unknown/already-dead
    /// token, so callers treat any non-2xx as a transient failure to log,
    /// NOT a signout blocker — the anchor delete + family marker are the
    /// authoritative revocation; this revoke is defense-in-depth.
    ///
    /// # Errors
    /// [`OidcRpError::TokenExchange`] on transport error or non-2xx status.
    pub async fn revoke_token_public(
        &self,
        client_id: &str,
        token: &str,
    ) -> Result<(), OidcRpError> {
        let client_secret = self.broker_secret.derive_client_secret(client_id);
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("token", token)
            .append_pair("token_type_hint", "refresh_token")
            .append_pair("client_id", client_id)
            .append_pair("client_secret", &client_secret)
            .finish();
        let url = format!("{}/revoke", self.auth_ui_url.trim_end_matches('/'));
        // Reused, breaker-guarded, bounded-timeout client (§8.7).
        let resp = crate::hydra_client::call(&self.breaker, self.hydra_timeout, |client| async move {
            client
                .request(http::Method::POST, &url)?
                .header("content-type", "application/x-www-form-urlencoded")?
                .body(body)
                .send()
                .await
        })
        .await
        .map_err(|e| OidcRpError::from_hydra(e, "revoke"))?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            // Body MAY echo an error code, but NEVER the token (we sent it,
            // it is not in the response). Surface status only.
            return Err(OidcRpError::TokenExchange(format!("revoke HTTP {status}")));
        }
        Ok(())
    }

    /// Shared `POST /token` for the brokered-client grants above.
    async fn post_token(&self, body: String) -> Result<TokenSet, OidcRpError> {
        let token_url = format!("{}/token", self.auth_ui_url.trim_end_matches('/'));
        // Reused, breaker-guarded, bounded-timeout client (§8.7). This is the
        // mint hot path — `exchange_code_public` / `refresh_token_public` both
        // funnel through here, so the breaker here is what protects the
        // gateway from an OP `/token` brownout.
        let resp = crate::hydra_client::call(&self.breaker, self.hydra_timeout, |client| async move {
            client
                .request(http::Method::POST, &token_url)?
                .header("content-type", "application/x-www-form-urlencoded")?
                .body(body)
                .send()
                .await
        })
        .await
        .map_err(|e| OidcRpError::from_hydra(e, "token"))?;
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
            // SECURITY: redact — a 2xx `/token` body carries the
            // access_token + refresh_token in plaintext and this error is
            // logged on the refresh path (§8.1/§8.5). Non-2xx bodies (Hydra
            // error JSON, no tokens) are surfaced above this branch.
            OidcRpError::TokenExchange(format!("parse: {e} (success body redacted)"))
        })
    }

    /// Verify a **raw Hydra access JWT** (RFC 9068) locally against the
    /// gateway's JWKS cache — no remote validation round-trip. Used by the
    /// Bearer arm's raw-Hydra path (§1.3, slice 1c) for non-browser
    /// clients that hold a Hydra access token directly (CLI,
    /// server-to-server). The browser never takes this path — it holds a
    /// gateway-signed session cookie, verified by `session_token::Verifier`.
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
    /// OIDC `auth_time` (UNIX seconds), when the token carries it. Forwarded
    /// onto the re-created gateway session on reload-recovery (BFF §2.2 step
    /// 5b). `None` when absent — Hydra access JWTs do not always include it.
    pub auth_time: Option<i64>,
    /// OIDC `amr` (authentication methods), when present. Forwarded onto the
    /// re-created gateway session on reload-recovery.
    pub amr: Option<Vec<String>>,
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
    #[serde(default)]
    auth_time: Option<i64>,
    #[serde(default)]
    amr: Option<Vec<String>>,
}

/// Resolve the cookie-arm `granted_scopes` (Slice 3, §1.4) from the two
/// authoritative sources, in precedence order:
///
/// 1. `token_response_scope` — the OAuth token-endpoint `scope` field. Per
///    RFC 6749 §5.1 it is REQUIRED only when the granted scope differs from
///    the requested scope, so a server (Hydra) MAY omit it on a no-narrowing
///    consent. When present and non-blank it wins.
/// 2. `access_token_scope` — the `scope` claim of the (already JWKS-verified)
///    access-token JWT (RFC 9068 §2.2.3 makes it mandatory for Hydra-issued
///    access tokens). Used as the fallback when (1) is absent/blank, so the
///    cookie arm never silently records `[]` for a consented session.
///
/// Each source is whitespace-split into individual scope ids. An entirely
/// absent/blank pair yields an empty vec.
fn resolve_granted_scopes(
    token_response_scope: Option<&str>,
    access_token_scope: Option<&str>,
) -> Vec<String> {
    let chosen = match token_response_scope {
        Some(s) if !s.trim().is_empty() => Some(s),
        _ => access_token_scope,
    };
    chosen
        .map(|s| s.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default()
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
        auth_time: raw.auth_time,
        amr: raw.amr,
    })
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
    /// The shared Hydra circuit breaker is open (or the bounded per-call
    /// timeout fired) — Hydra is browning out. Handlers surface this as
    /// `503 upstream_unavailable` rather than a generic token-exchange error,
    /// and the mint path short-circuits before holding any DB connection
    /// (auth-sdk §8.7).
    #[error("upstream unavailable: {0}")]
    UpstreamUnavailable(String),
}

impl OidcRpError {
    /// Map a [`HydraError`](crate::hydra_client::HydraError) from the shared
    /// breaker-guarded client into an `OidcRpError`, tagging it with the
    /// `context` of the call site (`"token"`, `"revoke"`) so
    /// the surfaced `Display` still identifies WHICH Hydra call failed — the
    /// per-site discriminator the pre-shared-client code carried in its
    /// inline `format!` prefixes. The brownout-vs-transport distinction is
    /// preserved: breaker-open / bounded-timeout → `UpstreamUnavailable`
    /// (→ 503), a transport error → `TokenExchange` (already counted as a
    /// breaker failure inside [`hydra_client::call`](crate::hydra_client::call)).
    fn from_hydra(e: crate::hydra_client::HydraError, context: &str) -> Self {
        use crate::hydra_client::HydraError;
        match e {
            HydraError::Open | HydraError::Timeout(_) => {
                OidcRpError::UpstreamUnavailable(format!("{context} {e}"))
            }
            HydraError::Upstream(msg) => {
                OidcRpError::TokenExchange(format!("{context} send: {msg}"))
            }
        }
    }

    /// `true` when this error is a Hydra brownout (breaker open or bounded
    /// timeout) — the signal handlers use to emit `503 upstream_unavailable`.
    #[must_use]
    pub const fn is_upstream_unavailable(&self) -> bool {
        matches!(self, OidcRpError::UpstreamUnavailable(_))
    }
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

/// Browser-supplied parameters for [`OidcRp::build_browser_authorize_url`]
/// (auth-sdk Slice 1b-browser). Every value originates in the SDK and is
/// passed through to Hydra verbatim — the gateway holds no PKCE verifier
/// (the browser does). `prompt` is optional (omitted ⇒ Hydra SSO skip).
#[derive(Debug, Clone)]
pub struct BrowserAuthorizeParams<'a> {
    /// PKCE S256 challenge the browser derived from its own verifier.
    pub code_challenge: &'a str,
    /// `OAuth2` `state` (the SDK's CSRF/relay-match token).
    pub state: &'a str,
    /// OIDC `nonce` (the SDK's cross-flow guard; not echoed back to gateway).
    pub nonce: &'a str,
    /// Space-delimited requested scopes.
    pub scope: &'a str,
    /// The app's own registered callback (defaults to `.../popup-callback`).
    pub redirect_uri: &'a str,
    /// Optional `prompt` passthrough (`login`/`consent` for step-up;
    /// omitted in the common interactive case).
    pub prompt: Option<&'a str>,
    /// Optional provider hint (`google`/`github`/`password`) passed through to
    /// Hydra as `idp_hint` so the login UI can pre-select / route to the named
    /// upstream IdP (auth-sdk Slice 1b-browser, Phase-1 `SignInOptions.provider`).
    /// Omitted ⇒ Hydra/login-UI shows the default provider picker.
    pub idp_hint: Option<&'a str>,
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
    client_id: String,
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
// - `__Host-zeroship_app_session` — opaque session id minted after a successful
//   OIDC dance. 12 h max-age, set on `/__zeroship/auth/callback`, cleared on
//   `/__zeroship/auth/logout`. The gateway looks this up server-side to resolve
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
pub const APP_SESSION_COOKIE_PROD: &str = "__Host-zeroship_app_session";
/// Dev app session cookie name (no `__Host-` prefix).
pub const APP_SESSION_COOKIE_DEV: &str = "zeroship_app_session";

/// Cookie `Max-Age` for the SIGNED STATELESS session cookie (BFF redesign slice
/// R1b). The cookie is a gateway-signed `zeroship-sess+jwt` identity assertion with a
/// short ~15 min lifetime ([`crate::session_token::SESSION_TOKEN_TTL_SECS`]) —
/// NOT a 12h opaque session id. The browser holds it only as long as its `exp`;
/// the durable credential is the 30-day server-held anchor, which silently
/// re-signs a fresh cookie via `GET /__zeroship/auth/session` when this one lapses.
pub const APP_SESSION_MAX_AGE_SECS: i64 = crate::session_token::SESSION_TOKEN_TTL_SECS;

/// Resolve the app session cookie name for the current environment.
#[must_use]
pub fn app_session_cookie_name(insecure_dev: bool) -> &'static str {
    if insecure_dev { APP_SESSION_COOKIE_DEV } else { APP_SESSION_COOKIE_PROD }
}

/// Build the `Set-Cookie` header value for the per-app session.
///
/// The value is the gateway-SIGNED `zeroship-sess+jwt` token (BFF slice R1b), NOT an
/// opaque session id. `insecure_dev = true` drops the `Secure` flag AND the
/// `__Host-` prefix (RFC 6265bis §4.1.3.2 — `__Host-` requires Secure). The
/// cookie stays HttpOnly + SameSite=Lax (XSS cannot read it; the signed token
/// is an identity assertion, never a power token).
#[must_use]
pub fn set_app_session_cookie(token: &str, insecure_dev: bool) -> String {
    let name = app_session_cookie_name(insecure_dev);
    let secure = if insecure_dev { "" } else { "; Secure" };
    format!(
        "{name}={token}; Path=/; HttpOnly; SameSite=Lax{secure}; Max-Age={APP_SESSION_MAX_AGE_SECS}"
    )
}

/// Clear the per-app session cookie on logout.
#[must_use]
pub fn clear_app_session_cookie(insecure_dev: bool) -> String {
    let name = app_session_cookie_name(insecure_dev);
    let secure = if insecure_dev { "" } else { "; Secure" };
    format!("{name}=; Path=/; HttpOnly; SameSite=Lax{secure}; Max-Age=0")
}

/// Parse the raw signed session token out of a `Cookie` header value (BFF slice
/// R1b — the cookie now carries a `zeroship-sess+jwt`, not a UUID). Returns the token
/// string for the cookie arm to verify LOCALLY via
/// [`crate::session_token::Verifier`] — no DB round-trip.
#[must_use]
pub fn parse_app_session_cookie(cookie_header: &str, insecure_dev: bool) -> Option<String> {
    let name = app_session_cookie_name(insecure_dev);
    let prefix = format!("{name}=");
    for part in cookie_header.split(';') {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix(&prefix) {
            if rest.is_empty() {
                return None;
            }
            return Some(rest.to_string());
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
    /// OAuth scopes granted to THIS app for this user (auth-sdk Slice 3,
    /// spec §1.4). A PERMANENT kernel-contract field on the `ZeroShip-User`
    /// projection: app code reads it via `env.auth.getUser().scopes`. Sourced
    /// from the token `scope` claim (Bearer wrapper + raw-Hydra arms) or the
    /// session/anchor `granted_scopes` (cookie/anchor arms). Always present
    /// (empty when the token/session carries no scopes), so the worker JSON
    /// shape is stable across every auth arm.
    #[serde(default)]
    pub scopes: Vec<&'a str>,
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

    const TEST_BROKER_MASTER: &[u8] = b"gateway-test-broker-master-secret-32-bytes";

    fn test_broker_secret() -> BrokerSecret {
        BrokerSecret::from_bytes(TEST_BROKER_MASTER.to_vec()).expect("valid test broker secret")
    }

    fn make_stash() -> Stash {
        Stash {
            state: "s".into(),
            client_id: "oac_myapp".into(),
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
            test_broker_secret(),
            b"signing-key-1234".to_vec(),
        );
        let (url, stash) = rp.build_authorize_redirect(
            "oac_myapp",
            "/some/path",
            "https://myapp.zeroship.ai/__zeroship/auth/callback",
        );
        assert!(url.starts_with("https://auth.zeroship.ai/authorize?"));
        assert!(url.contains("client_id=oac_myapp"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("code_challenge="));
        assert!(url.contains("state="));
        assert!(url.contains("nonce="));
        // `scope=openid offline_access email profile` URL-encoded
        // (form_urlencoded uses `+` for space).
        assert!(url.contains("scope=openid+offline_access+email+profile"));
        // redirect_uri URL-encoded.
        assert!(url.contains("redirect_uri=https%3A%2F%2Fmyapp.zeroship.ai%2F__zeroship%2Fauth%2Fcallback"));
        // Stash is non-empty and contains the dot-separator.
        assert!(stash.contains('.'));
    }

    #[test]
    fn browser_authorize_url_carries_browser_pkce_and_no_stash() {
        // The browser flow (Slice 1b-browser): the gateway is HANDED the
        // already-computed code_challenge + browser-chosen state/nonce, and
        // injects the per-app PUBLIC client_id. No stash cookie is minted
        // (the verifier lives in the browser). Assert every passthrough
        // param lands and the per-app client_id (not "gateway") is used.
        let rp = OidcRp::new(
            "https://auth.zeroship.ai",
            test_broker_secret(),
            b"k".repeat(32),
        );
        let params = BrowserAuthorizeParams {
            code_challenge: "BROWSER_CHALLENGE_abc",
            state: "STATE_xyz",
            nonce: "NONCE_123",
            scope: "openid profile read:billing",
            redirect_uri: "https://myapp.zeroship.ai/__zeroship/auth/popup-callback",
            prompt: None,
            idp_hint: None,
        };
        let url = rp.build_browser_authorize_url("oac_myapp", &params);
        assert!(url.starts_with("https://auth.zeroship.ai/authorize?"), "{url}");
        // PER-APP client_id, never the gateway confidential client.
        assert!(url.contains("client_id=oac_myapp"), "{url}");
        assert!(!url.contains("client_id=gateway"), "{url}");
        assert!(url.contains("response_type=code"));
        assert!(url.contains("code_challenge=BROWSER_CHALLENGE_abc"), "{url}");
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state=STATE_xyz"), "{url}");
        assert!(url.contains("nonce=NONCE_123"), "{url}");
        // scope URL-encoded (form_urlencoded uses `+` for space).
        assert!(url.contains("scope=openid+profile+read%3Abilling"), "{url}");
        assert!(
            url.contains("redirect_uri=https%3A%2F%2Fmyapp.zeroship.ai%2F__zeroship%2Fauth%2Fpopup-callback"),
            "{url}"
        );
        // No prompt in the common case (so Hydra's SSO skip fires).
        assert!(!url.contains("prompt="), "default omits prompt: {url}");
        // No idp_hint unless the SDK supplied a provider.
        assert!(!url.contains("idp_hint="), "default omits idp_hint: {url}");
    }

    #[test]
    fn browser_authorize_url_passes_prompt_through_when_present() {
        // `prompt=consent` (incremental-scope step-up) and `prompt=login`
        // (re-auth) are passed straight through; an empty prompt is dropped.
        let rp = OidcRp::new("https://auth.zeroship.ai", test_broker_secret(), b"k".repeat(32));
        let base = BrowserAuthorizeParams {
            code_challenge: "c",
            state: "s",
            nonce: "n",
            scope: "openid",
            redirect_uri: "https://app/cb",
            prompt: Some("consent"),
            idp_hint: None,
        };
        let url = rp.build_browser_authorize_url("oac_app", &base);
        assert!(url.contains("prompt=consent"), "{url}");

        let login = BrowserAuthorizeParams { prompt: Some("login"), ..base.clone() };
        assert!(rp.build_browser_authorize_url("oac_app", &login).contains("prompt=login"));

        let empty = BrowserAuthorizeParams { prompt: Some(""), ..base };
        assert!(!rp.build_browser_authorize_url("oac_app", &empty).contains("prompt="));
    }

    /// Fix 5 (MAJOR): `SignInOptions.provider` is threaded through as the
    /// `idp_hint` authorize-URL param so the login UI can route to the named
    /// upstream IdP. A present hint lands in the Hydra URL; an empty one is
    /// dropped (mirrors the `prompt` passthrough discipline).
    #[test]
    fn browser_authorize_url_passes_idp_hint_through_when_present() {
        let rp = OidcRp::new("https://auth.zeroship.ai", test_broker_secret(), b"k".repeat(32));
        let base = BrowserAuthorizeParams {
            code_challenge: "c",
            state: "s",
            nonce: "n",
            scope: "openid",
            redirect_uri: "https://app/cb",
            prompt: None,
            idp_hint: Some("google"),
        };
        let url = rp.build_browser_authorize_url("oac_app", &base);
        assert!(url.contains("idp_hint=google"), "provider must reach the authorize URL: {url}");

        let github = BrowserAuthorizeParams { idp_hint: Some("github"), ..base.clone() };
        assert!(rp.build_browser_authorize_url("oac_app", &github).contains("idp_hint=github"));

        let password = BrowserAuthorizeParams { idp_hint: Some("password"), ..base.clone() };
        assert!(rp.build_browser_authorize_url("oac_app", &password).contains("idp_hint=password"));

        // An empty idp_hint is dropped (no `idp_hint=` in the URL).
        let empty = BrowserAuthorizeParams { idp_hint: Some(""), ..base };
        assert!(!rp.build_browser_authorize_url("oac_app", &empty).contains("idp_hint="));
    }

    #[test]
    fn browser_authorize_url_trims_trailing_slash() {
        let rp = OidcRp::new("https://auth.zeroship.ai/", test_broker_secret(), b"k".repeat(32));
        let params = BrowserAuthorizeParams {
            code_challenge: "c",
            state: "s",
            nonce: "n",
            scope: "openid",
            redirect_uri: "https://app/cb",
            prompt: None,
            idp_hint: None,
        };
        let url = rp.build_browser_authorize_url("oac_app", &params);
        assert!(url.starts_with("https://auth.zeroship.ai/authorize?"), "no double slash: {url}");
    }

    #[test]
    fn new_derives_issuer_from_auth_ui_url_with_trailing_slash() {
        let rp = OidcRp::new(
            "https://auth.zeroship.ai",
            test_broker_secret(),
            b"k".repeat(32),
        );
        assert_eq!(rp.issuer, "https://auth.zeroship.ai");

        // Trimming is idempotent — trailing slash on auth_ui_url must not
        // produce `//`.
        let rp = OidcRp::new(
            "https://auth.zeroship.ai/",
            test_broker_secret(),
            b"k".repeat(32),
        );
        assert_eq!(rp.issuer, "https://auth.zeroship.ai");
    }

    #[test]
    fn with_issuer_overrides_default_iss() {
        // Tests dial loopback auth but expect the canonical OP
        // issuer string — `with_issuer` decouples the two.
        let rp = OidcRp::new(
            "http://127.0.0.1:4444",
            test_broker_secret(),
            b"k".repeat(32),
        )
        .with_issuer("https://auth.zeroship.ai");
        // `auth_ui_url` still drives /token + JWKS (loopback).
        assert_eq!(rp.auth_ui_url, "http://127.0.0.1:4444");
        // `issuer` is the logical OP issuer that ID tokens carry.
        assert_eq!(rp.issuer, "https://auth.zeroship.ai");
    }

    #[test]
    fn authorize_url_trims_trailing_slash_on_auth_ui_url() {
        let rp = OidcRp::new(
            "https://auth.zeroship.ai/",
            test_broker_secret(),
            b"k".repeat(32),
        );
        let (url, _) = rp.build_authorize_redirect("oac_app", "/", "https://app/cb");
        // No double slash before `/authorize`.
        assert!(url.starts_with("https://auth.zeroship.ai/authorize?"));
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
        // The cookie now carries a signed zeroship-sess+jwt token, not a UUID.
        let token = "eyJ.signed.token";
        let c = set_app_session_cookie(token, false);
        assert!(c.starts_with("__Host-zeroship_app_session="));
        assert!(c.contains(token));
        assert!(c.contains("Path=/"));
        assert!(c.contains("HttpOnly"));
        assert!(c.contains("SameSite=Lax"));
        assert!(c.contains("Secure"));
        // Short-lived signed cookie (~15 min), NOT the old 12h opaque id.
        assert!(c.contains("Max-Age=900"));
    }

    #[test]
    fn app_session_set_cookie_drops_secure_and_host_prefix_in_dev() {
        // RFC 6265bis §4.1.3.2: __Host- cookies require Secure. Dev
        // runs over plain HTTP without Secure, so the prefix MUST be
        // dropped too — otherwise compliant clients silently reject
        // the cookie.
        let c = set_app_session_cookie("eyJ.signed.token", true);
        assert!(!c.starts_with("__Host-"), "dev cookie must NOT use __Host- prefix: {c}");
        assert!(c.starts_with("zeroship_app_session="), "dev cookie name: {c}");
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
        // The value is now a signed token string (opaque to the parser).
        let token = "eyJhbGc.eyJzdWI.sig";
        let header = format!("foo=bar; __Host-zeroship_app_session={token}; baz=qux");
        assert_eq!(parse_app_session_cookie(&header, false).as_deref(), Some(token));
        assert_eq!(parse_app_session_cookie("nothing-here", false), None);
        // Empty value ⇒ None (no token to verify).
        assert_eq!(parse_app_session_cookie("__Host-zeroship_app_session=", false), None);

        // Dev mode reads the bare-name cookie.
        let dev_header = format!("zeroship_app_session={token}");
        assert_eq!(parse_app_session_cookie(&dev_header, true).as_deref(), Some(token));
        // Prod-prefixed cookie is ignored in dev mode (looks for bare name).
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

    // ----- Slice 3 §1.4: cookie-arm granted-scope resolution -----------------

    #[test]
    fn granted_scopes_prefers_token_response_scope() {
        // When the token endpoint returns a non-blank `scope`, it is
        // authoritative and the access-token claim is ignored.
        let out = resolve_granted_scopes(
            Some("openid read:billing"),
            Some("openid offline_access SHOULD_NOT_APPEAR"),
        );
        assert_eq!(out, vec!["openid".to_string(), "read:billing".to_string()]);
    }

    #[test]
    fn granted_scopes_falls_back_to_access_token_claim_when_response_scope_absent() {
        // RFC 6749 §5.1: the token-response `scope` MAY be omitted when the
        // granted scope equals the requested scope. The cookie arm must then
        // recover the consented scopes from the access-token `scope` claim —
        // NOT silently record `[]` (the bug this fix addresses).
        let out = resolve_granted_scopes(None, Some("openid offline_access read:billing"));
        assert_eq!(
            out,
            vec![
                "openid".to_string(),
                "offline_access".to_string(),
                "read:billing".to_string(),
            ],
            "absent token-response scope must fall back to the access-token claim"
        );
    }

    #[test]
    fn granted_scopes_falls_back_when_response_scope_is_blank() {
        // An empty / whitespace-only `scope` string is treated the same as
        // absent — fall back to the access-token claim.
        let out = resolve_granted_scopes(Some("   "), Some("openid email"));
        assert_eq!(out, vec!["openid".to_string(), "email".to_string()]);
    }

    #[test]
    fn granted_scopes_empty_when_both_sources_missing() {
        assert!(resolve_granted_scopes(None, None).is_empty());
        assert!(resolve_granted_scopes(Some(""), None).is_empty());
    }
}
