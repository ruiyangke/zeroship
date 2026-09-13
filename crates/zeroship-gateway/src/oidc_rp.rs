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
//! The gateway's dispatch handler calls `OidcRp::build_authorize_redirect` on
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
    /// The OP protocol surface is derived by appending `/oauth2`; this base
    /// must be reachable from the gateway process.
    pub auth_ui_url: String,
    /// Expected `iss` claim in ID tokens issued by the platform OP. Defaults
    /// to `{auth_ui_url}/oauth2`, byte-identical to the self-contained OP's
    /// issuer stamp. In tests this can be overridden separately from
    /// `auth_ui_url` — see [`OidcRp::with_issuer`].
    pub issuer: String,
    /// Platform broker master secret used to derive per-app client secrets for
    /// brokered `oac_` clients.
    broker_secret: BrokerSecret,
    /// JWKS cache — shared with other RPs in the same gateway process.
    pub jwks: Arc<JwksCache>,
    /// HMAC-SHA256 key used to sign the `__Host-zs_oidc_stash` cookie body.
    /// Must be at least 32 random bytes in prod.
    pub stash_signing_key: Vec<u8>,
    /// Shared circuit breaker for ALL outbound OP token/revoke calls.
    /// `Arc`-shared so an OP brownout
    /// observed on one ntex worker thread trips the breaker for every thread —
    /// the reused `cyper::Client` itself is per-worker-thread (`!Send` in
    /// practice; see [`crate::op_client`]), but the breaker state is
    /// one-per-process. When open, every OP call fast-fails with
    /// `OpError::Open` (→ `503 upstream_unavailable`) instead of opening a
    /// fresh connection into the brownout.
    pub breaker: Arc<crate::op_client::CircuitBreaker>,
    /// Bounded per-call timeout for every outbound OP request. A hung OP
    /// returns a fast `OpError::Timeout` (counted as a breaker
    /// failure) rather than an unbounded await pinning a connection.
    pub op_timeout: std::time::Duration,
}

impl OidcRp {
    /// Construct a new RP. Caller supplies the auth UI/OIDC upstream URL, the
    /// validated platform broker secret, and the stash signing key. The JWKS
    /// cache is derived from `{auth_ui_url}/oauth2/.well-known/jwks.json`.
    /// The expected ID-token `iss` defaults to `{auth_ui_url}/oauth2`; override via
    /// [`OidcRp::with_issuer`] when the dial-URL and issuer string differ
    /// (e.g. loopback tests).
    pub fn new(
        auth_ui_url: impl Into<String>,
        broker_secret: BrokerSecret,
        stash_signing_key: impl Into<Vec<u8>>,
    ) -> Self {
        let auth_ui_url = auth_ui_url.into();
        let issuer = op_base_url(&auth_ui_url);
        let jwks_url = format!(
            "{}/.well-known/jwks.json",
            issuer
        );
        Self {
            auth_ui_url,
            issuer,
            broker_secret,
            jwks: Arc::new(JwksCache::new(jwks_url)),
            stash_signing_key: stash_signing_key.into(),
            breaker: Arc::new(crate::op_client::CircuitBreaker::default()),
            op_timeout: crate::op_client::DEFAULT_OP_TIMEOUT,
        }
    }

    /// Override the shared circuit breaker (tests inject a fast-tripping
    /// breaker; production uses the [`CircuitBreaker::default`] from
    /// [`OidcRp::new`]).
    ///
    /// [`CircuitBreaker::default`]: crate::op_client::CircuitBreaker::default
    #[must_use]
    pub fn with_breaker(mut self, breaker: Arc<crate::op_client::CircuitBreaker>) -> Self {
        self.breaker = breaker;
        self
    }

    /// Override the bounded per-call OP timeout (tests use a short value
    /// to exercise the timeout→breaker-failure path quickly).
    #[must_use]
    pub fn with_op_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.op_timeout = timeout;
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

    fn op_base_url(&self) -> String {
        op_base_url(&self.auth_ui_url)
    }

    /// Build the `/authorize` redirect URL + the signed stash cookie
    /// body. `original_path` is the request path the user was trying to
    /// reach; `redirect_uri` is the per-app callback URL the worker
    /// registered with op (e.g.
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

        let url = format!("{}/authorize?{}", self.op_base_url(), query);

        (url, stash_value)
    }

    /// Process the callback. Verifies the stash cookie signature, matches
    /// the `state` parameter, exchanges the code for tokens, verifies the
    /// ID token, and returns `(claims, original_path, granted_scopes)` on
    /// success. `granted_scopes` is the token endpoint's `scope` response
    /// (the scopes the user actually consented to), persisted onto the cookie
    /// session row and included in the signed cookie so the per-request cookie
    /// path can emit `WorkerUser.scopes` without reading that row.
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
        expected_client_id: &str,
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

        // 2b. Bind the exchange to THIS route's per-app client. The stash was
        //     minted with the route's `oauth_client_id`; the session is later
        //     projected under the caller's route client_id + sector. Enforce
        //     the two agree as an INVARIANT (not merely an emergent property of
        //     __Host- cookie origin-isolation): a stash whose client_id does not
        //     match the route it is being redeemed on is a per-app-isolation
        //     violation — fail closed (cross-tenant identity/PII bind otherwise).
        if stash.client_id != expected_client_id {
            return Err(OidcRpError::ClientMismatch);
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

        let token_url = format!("{}/token", self.op_base_url());
        // Reused, breaker-guarded, bounded-timeout client (§8.7). A failed
        // build is a programming error (bad URL), not a transport failure, so
        // it never reaches the breaker; the `send()` await is what the breaker
        // and timeout wrap.
        let resp = crate::op_client::call(&self.breaker, self.op_timeout, |client| async move {
            client
                .request(http::Method::POST, &token_url)?
                .header("content-type", "application/x-www-form-urlencoded")?
                .body(body)
                .send()
                .await
        })
        .await
        .map_err(|e| OidcRpError::from_op(e, "token"))?;

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
        // at_hash binds the id_token to the paired access token (the OP stamps
        // at_hash on the token-endpoint id_token). c_hash is NOT applicable here:
        // it is only defined for the AUTHORIZATION-endpoint id_token of the
        // implicit/hybrid flows (OIDC Core 3.1.3.6 — the token-endpoint id_token
        // of the code flow carries no c_hash), so we pass None. (Code injection
        // is already blocked by mandatory S256 PKCE.)
        let claims = verify_id_token(
            &self.jwks,
            &tr.id_token,
            &self.issuer,
            &stash.client_id,
            Some(&stash.nonce),
            Some(&tr.access_token),
            None,
        )
        .await?;
        let _ = code;

        // Granted scopes — what the user actually consented to.
        // Persisted onto the cookie session so the per-request path emits
        // WorkerUser.scopes with no token to decode in the browser.
        //
        // Primary source: the token endpoint's `scope` response. But RFC 6749
        // §5.1 makes that parameter OPTIONAL when the granted scope equals the
        // requested scope, so OP may omit it on a no-narrowing consent. When
        // it is absent/empty we MUST fall back to the always-present `scope`
        // claim of the access token — decoded through the SAME JWKS-verified
        // path the Bearer/raw-OP arms use, so the cookie arm records the same
        // authoritative, non-spoofable scope set as every other arm (instead of
        // silently persisting `[]`).
        let access_scope = match tr.scope.as_deref() {
            // Token-response `scope` present and non-blank — authoritative, no
            // need to decode the access token at all.
            Some(s) if !s.trim().is_empty() => None,
            _ => match verify_access_jwt(&self.jwks, &tr.access_token, &self.issuer).await {
                Ok(ac) => ac.scope,
                Err(e) => {
                    // The access token is a OP-issued RFC 9068 JWT here, so a
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

    /// Exchange an authorization code for tokens as a brokered PKCE client for
    /// `POST /__zeroship/auth/session`. Unlike
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

    /// Rotate a refresh family as a brokered PKCE client for the server-held
    /// `?mint=1` refresh. Posts
    /// `grant_type=refresh_token` with the per-app `client_id` and derived
    /// broker secret injected.
    /// Returns the rotated token set (new `access_token` + new
    /// `refresh_token`).
    ///
    /// # Errors
    /// [`OidcRpError::TokenExchange`] — its message contains the upstream
    /// status + body, so callers can detect OP `invalid_grant` (family
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

    /// Build the OP `/authorize` URL for the browser PKCE flow served by
    /// `GET /__zeroship/auth/authorize`.
    ///
    /// Unlike [`OidcRp::build_authorize_redirect`] (the interactive cookie
    /// flow, which generates the PKCE verifier/state/nonce server-side and
    /// stashes the verifier in a signed cookie), this is the SUPABASE-style
    /// public-client flow: **the browser holds the PKCE verifier**, so the
    /// gateway is given the already-computed `code_challenge` (S256), the
    /// browser-chosen `state` + `nonce`, the requested `scope`, and the
    /// per-app `redirect_uri`. The gateway holds NO server-side state — it
    /// just assembles the redirect. `prompt` is a PASSTHROUGH (omitted in
    /// the common interactive-popup case so OP's SSO skip fires; `login`
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
        // `prompt` is optional — omitted in the common case so OP's
        // SSO/`remember` skip path fires. Passthrough
        // when the browser explicitly asks for `login`/`consent` step-up.
        if let Some(prompt) = p.prompt {
            if !prompt.is_empty() {
                q.append_pair("prompt", prompt);
            }
        }
        // `idp_hint` is optional — passed through to OP so the login UI can
        // route to / pre-select the named upstream IdP. Omitted ⇒ default picker.
        if let Some(idp_hint) = p.idp_hint {
            if !idp_hint.is_empty() {
                q.append_pair("idp_hint", idp_hint);
            }
        }
        let query = q.finish();
        format!(
            "{}/authorize?{}",
            self.op_base_url(),
            query
        )
    }

    /// Best-effort revoke a token (refresh family) at the OP's RFC 7009
    /// `/revoke` endpoint as a brokered client during
    /// `POST /__zeroship/auth/signout`. The per-app `client_id`
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
        let url = format!("{}/revoke", self.op_base_url());
        // Reused, breaker-guarded, bounded-timeout client (§8.7).
        let resp = crate::op_client::call(&self.breaker, self.op_timeout, |client| async move {
            client
                .request(http::Method::POST, &url)?
                .header("content-type", "application/x-www-form-urlencoded")?
                .body(body)
                .send()
                .await
        })
        .await
        .map_err(|e| OidcRpError::from_op(e, "revoke"))?;
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
        let token_url = format!("{}/token", self.op_base_url());
        // Reused, breaker-guarded, bounded-timeout client (§8.7). This is the
        // mint hot path — `exchange_code_public` / `refresh_token_public` both
        // funnel through here, so the breaker here is what protects the
        // gateway from an OP `/token` brownout.
        let resp = crate::op_client::call(&self.breaker, self.op_timeout, |client| async move {
            client
                .request(http::Method::POST, &token_url)?
                .header("content-type", "application/x-www-form-urlencoded")?
                .body(body)
                .send()
                .await
        })
        .await
        .map_err(|e| OidcRpError::from_op(e, "token"))?;
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
            // logged on the refresh path (§8.1/§8.5). Non-2xx bodies (OP
            // error JSON, no tokens) are surfaced above this branch.
            OidcRpError::TokenExchange(format!("parse: {e} (success body redacted)"))
        })
    }

    /// Verify a **raw OP access JWT** (RFC 9068) locally against the
    /// gateway's JWKS cache — no remote validation round-trip. Used by the
    /// Bearer arm's raw-OP path for non-browser
    /// clients that hold an OP access token directly (CLI,
    /// server-to-server). The browser never takes this path — it holds a
    /// gateway-signed session cookie, verified by `session_token::Verifier`.
    ///
    /// Validation covers: `typ == at+jwt`, `alg == EdDSA`, signature
    /// (against a cached JWK matching the token's `kid`), `iss == self.issuer`,
    /// `exp`, and the RFC 9068 required claim set. **`aud` is deliberately NOT
    /// validated here** — an access token's `aud` is the resource-server
    /// audience, not the OAuth client. The caller must bind both the
    /// `client_id` claim and the route-specific resource audience.
    ///
    /// **This function does NOT bind the token to any client or resource.** It
    /// verifies only token shape/signature/issuer/lifetime and returns the
    /// decoded claims. Per-app binding (`client_id` claim == route client) and
    /// resource binding (`aud` contains the route resource audience) are the
    /// CALLER's responsibility. A caller that skips those caller-side checks
    /// silently opens cross-app or cross-resource replay; that is why no
    /// `expected_client_id` parameter is accepted here (it would falsely imply
    /// this function enforces binding).
    ///
    /// # Errors
    ///
    /// [`OidcRpError::VerifyAccessToken`] wrapping an [`OidcError`] for any
    /// JWKS/signature/iss/exp failure. Callers translate this into a
    /// `401` (a `RequiredPrincipal::User` route) or a fall-through to
    /// anonymous (a `RequiredPrincipal::Anonymous` route), per the Bearer-arm
    /// policy gate.
    pub async fn verify_access_token(&self, token: &str) -> Result<AccessClaims, OidcRpError> {
        let claims = verify_access_jwt(&self.jwks, token, &self.issuer).await?;
        Ok(claims)
    }
}

fn op_base_url(auth_ui_url: &str) -> String {
    format!("{}/oauth2", auth_ui_url.trim_end_matches('/'))
}

/// Decoded claims of a raw OP access JWT (RFC 9068). The Bearer arm
/// reads `client_id`/`aud` for per-app binding and `sub` for identity.
/// Optional profile claims are decoded but are not trusted as an app-facing
/// email projection.
///
/// `aud` per RFC 7519 may be a string OR an array of strings; the helper
/// normalizes both into a `Vec<String>` so the caller can enforce the route's
/// resource-server audience uniformly.
#[derive(Debug, Clone)]
pub struct AccessClaims {
    /// Issuer-projected per-app pairwise subject. The raw-OP Bearer path
    /// requires the `pws_` shape and forwards it unchanged.
    pub sub: String,
    /// The OAuth `client_id` claim (RFC 9068 §3 mandates it; OP emits it).
    /// The Bearer arm's per-app authorized-party binding.
    pub client_id: Option<String>,
    /// The token audience(s), normalized to a list. The Bearer arm requires this
    /// list to contain the route's resource-server audience.
    pub aud: Vec<String>,
    /// Issued-at (UNIX seconds) — used by the revocation family marker.
    pub iat: i64,
    pub email: Option<String>,
    pub email_verified: Option<bool>,
    pub name: Option<String>,
    pub scope: Option<String>,
    /// OIDC `auth_time` (UNIX seconds), when the token carries it. Forwarded
    /// onto the re-created gateway session on reload-recovery (BFF §2.2 step
    /// 5b). `None` when absent — OP access JWTs do not always include it.
    pub auth_time: Option<i64>,
    /// OIDC `amr` (authentication methods), when present. Forwarded onto the
    /// re-created gateway session on reload-recovery.
    pub amr: Option<Vec<String>>,
}

/// Wire shape of a OP access JWT we deserialize. `aud` is a raw
/// `serde_json::Value` so we accept both the string and array forms.
#[derive(Deserialize)]
struct RawAccessClaims {
    sub: String,
    iss: String,
    aud: serde_json::Value,
    exp: i64,
    iat: i64,
    // RFC 9068 §2.2 requires `jti`. jsonwebtoken's `required_spec_claims` only
    // enforces registered claims (exp/iss/aud/sub/nbf), so `jti` presence is
    // enforced here by making it a required (non-`default`) deserialized field —
    // a token missing `jti` fails to parse and is rejected.
    #[allow(dead_code)]
    jti: String,
    client_id: String,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    email_verified: Option<bool>,
    #[serde(default)]
    name: Option<String>,
    scope: String,
    #[serde(default)]
    auth_time: Option<i64>,
    #[serde(default)]
    amr: Option<Vec<String>>,
}

/// Resolve the cookie-arm `granted_scopes` from the two
/// authoritative sources, in precedence order:
///
/// 1. `token_response_scope` — the OAuth token-endpoint `scope` field. Per
///    RFC 6749 §5.1 it is REQUIRED only when the granted scope differs from
///    the requested scope, so a server (OP) MAY omit it on a no-narrowing
///    consent. When present and non-blank it wins.
/// 2. `access_token_scope` — the `scope` claim of the (already JWKS-verified)
///    access-token JWT (RFC 9068 §2.2.3 makes it mandatory for OP-issued
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

/// Verify a raw OP access JWT against `cache`, pinning token type, algorithm,
/// `iss`, `exp`, and RFC 9068 required claims but NOT `aud` (see
/// [`OidcRp::verify_access_token`] rationale).
/// On a first verify failure (likely a rotated JWKS) the cache is
/// force-refreshed and verification retried once — mirroring
/// [`zeroship_core::oidc_verify::verify_id_token`].
async fn verify_access_jwt(
    cache: &JwksCache,
    token: &str,
    expected_iss: &str,
) -> Result<AccessClaims, OidcRpError> {
    use jsonwebtoken::{decode, decode_header, Algorithm, Validation};

    let header = decode_header(token).map_err(|e| {
        OidcRpError::VerifyAccessToken(OidcError::DecodeHeader(e.to_string()))
    })?;
    if header.typ.as_deref() != Some("at+jwt") {
        return Err(OidcRpError::VerifyAccessToken(OidcError::DecodeHeader(
            "access-token typ mismatch: expected at+jwt".into(),
        )));
    }
    if header.alg != Algorithm::EdDSA {
        return Err(OidcRpError::VerifyAccessToken(OidcError::DecodeHeader(
            "access-token alg mismatch: expected EdDSA".into(),
        )));
    }
    let kid = header
        .kid
        .clone()
        .ok_or_else(|| {
            OidcRpError::VerifyAccessToken(OidcError::DecodeHeader("no kid".into()))
        })?;

    let try_verify =
        |keys: Vec<zeroship_core::oidc_verify::CachedKey>| -> Result<RawAccessClaims, OidcError> {
        let key = keys
            .iter()
            .find(|k| k.kid == kid && k.alg == Algorithm::EdDSA)
            .ok_or_else(|| OidcError::NoMatchingKey(kid.clone()))?;
        let mut validation = Validation::new(Algorithm::EdDSA);
        validation.algorithms = vec![Algorithm::EdDSA];
        validation.set_issuer(&[expected_iss]);
        // Access-token `aud` is the resource-server audience, NOT the
        // OAuth client, so we do NOT pin it here. The Bearer arm enforces
        // `client_id` and the route's resource audience after decoding.
        validation.validate_aud = false;
        validation.validate_nbf = true;
        validation.leeway = 0;
        validation.required_spec_claims = [
            "exp",
            "iss",
            "aud",
            "sub",
            "iat",
            "jti",
            "client_id",
            "scope",
        ]
        .into_iter()
        .map(str::to_string)
        .collect::<std::collections::HashSet<_>>();
        let data: jsonwebtoken::TokenData<RawAccessClaims> =
            decode(token, &key.decoding, &validation)
                .map_err(|e| OidcError::Verify(e.to_string()))?;
        Ok(data.claims)
    };

    let raw = if let Ok(c) =
        try_verify(cache.keys().await.map_err(OidcRpError::VerifyAccessToken)?)
    {
        c
    } else {
        // Likely cause: JWKS rotated. Force-refresh once and retry.
        cache.refresh().await.map_err(OidcRpError::VerifyAccessToken)?;
        try_verify(cache.keys().await.map_err(OidcRpError::VerifyAccessToken)?)
            .map_err(OidcRpError::VerifyAccessToken)?
    };

    // Defense-in-depth iss re-check (jsonwebtoken checked it above).
    if raw.iss != expected_iss {
        return Err(OidcRpError::VerifyAccessToken(OidcError::IssuerMismatch {
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
        client_id: Some(raw.client_id),
        aud,
        iat: raw.iat,
        email: raw.email,
        email_verified: raw.email_verified,
        name: raw.name,
        scope: Some(raw.scope),
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
    #[error("stash client_id does not match the route it is redeemed on")]
    ClientMismatch,
    #[error("token exchange: {0}")]
    TokenExchange(String),
    #[error("verify id_token: {0}")]
    VerifyIdToken(#[from] OidcError),
    #[error("verify access_token: {0}")]
    VerifyAccessToken(#[source] OidcError),
    /// The shared OP circuit breaker is open (or the bounded per-call
    /// timeout fired) — OP is browning out. Handlers surface this as
    /// `503 upstream_unavailable` rather than a generic token-exchange error,
    /// and the mint path short-circuits before holding any DB connection
    /// (auth-sdk §8.7).
    #[error("upstream unavailable: {0}")]
    UpstreamUnavailable(String),
}

impl OidcRpError {
    /// Map a [`OpError`](crate::op_client::OpError) from the shared
    /// breaker-guarded client into an `OidcRpError`, tagging it with the
    /// `context` of the call site (`"token"`, `"revoke"`) so
    /// the surfaced `Display` still identifies WHICH OP call failed — the
    /// per-site discriminator the pre-shared-client code carried in its
    /// inline `format!` prefixes. The brownout-vs-transport distinction is
    /// preserved: breaker-open / bounded-timeout → `UpstreamUnavailable`
    /// (→ 503), a transport error → `TokenExchange` (already counted as a
    /// breaker failure inside [`op_client::call`](crate::op_client::call)).
    fn from_op(e: crate::op_client::OpError, context: &str) -> Self {
        use crate::op_client::OpError;
        match e {
            OpError::Open | OpError::Timeout(_) => {
                OidcRpError::UpstreamUnavailable(format!("{context} {e}"))
            }
            OpError::Upstream(msg) => {
                OidcRpError::TokenExchange(format!("{context} send: {msg}"))
            }
        }
    }

    /// `true` when this error is a OP brownout (breaker open or bounded
    /// timeout) — the signal handlers use to emit `503 upstream_unavailable`.
    #[must_use]
    pub const fn is_upstream_unavailable(&self) -> bool {
        matches!(self, OidcRpError::UpstreamUnavailable(_))
    }
}

/// `/oauth2/token` response body (subset). OP emits the OIDC standard
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
/// [`OidcRp::refresh_token_public`]. `id_token` is
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

/// Browser-supplied parameters for [`OidcRp::build_browser_authorize_url`].
/// Every value originates in the SDK and is
/// passed through to OP verbatim — the gateway holds no PKCE verifier
/// (the browser does). `prompt` is optional (omitted ⇒ OP SSO skip).
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
    /// OP as `idp_hint` so the login UI can pre-select / route to the named
    /// upstream IdP selected by `SignInOptions.provider`.
    /// Omitted ⇒ OP/login-UI shows the default provider picker.
    pub idp_hint: Option<&'a str>,
}

/// Server-side state stashed in the signed `__Host-zs_oidc_stash` cookie
/// between the initial 302 → op and the eventual callback. Sized to
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
// - `__Host-zeroship_app_session` is a short-lived gateway-signed identity JWT.
//   The gateway verifies it locally and applies pushed lifecycle plus family
//   revocation before resolving `ZeroShip-User`.
// - `__Host-zs_oidc_stash` — the signed PKCE+state stash. 10 min max-age,
//   set on the redirect to op, cleared on callback.
//
// Both use the `__Host-` prefix which RFC 6265bis (§4.1.3) requires
// `Path=/`, no `Domain=`, and `Secure`. These properties are unconditional:
// local development must satisfy the same cookie contract as a deployment.

/// App session cookie name (`__Host-` prefix requires `Secure`).
pub const APP_SESSION_COOKIE: &str = "__Host-zeroship_app_session";

/// Cookie `Max-Age` for the SIGNED STATELESS session cookie (BFF redesign slice
/// R1b). The cookie is a gateway-signed `zeroship-sess+jwt` identity assertion with a
/// short ~15 min lifetime ([`crate::session_token::SESSION_TOKEN_TTL_SECS`]) —
/// NOT a 12h opaque session id. The browser holds it only as long as its `exp`;
/// the durable credential is the 30-day server-held anchor, which silently
/// re-signs a fresh cookie via `GET /__zeroship/auth/session` when this one lapses.
pub const APP_SESSION_MAX_AGE_SECS: i64 = crate::session_token::SESSION_TOKEN_TTL_SECS;

#[must_use]
pub const fn app_session_cookie_name() -> &'static str {
    APP_SESSION_COOKIE
}

/// Build the `Set-Cookie` header value for the per-app session.
///
/// The value is the gateway-SIGNED `zeroship-sess+jwt` token (BFF slice R1b), NOT an
/// opaque session id. The cookie is always `Secure`, `HttpOnly`, and
/// `SameSite=Lax` (XSS cannot read it; the signed token is an identity
/// assertion, never a power token).
#[must_use]
pub fn set_app_session_cookie(token: &str) -> String {
    let name = app_session_cookie_name();
    format!(
        "{name}={token}; Path=/; HttpOnly; SameSite=Lax; Secure; Max-Age={APP_SESSION_MAX_AGE_SECS}"
    )
}

/// Clear the per-app session cookie on logout.
#[must_use]
pub fn clear_app_session_cookie() -> String {
    let name = app_session_cookie_name();
    format!("{name}=; Path=/; HttpOnly; SameSite=Lax; Secure; Max-Age=0")
}

/// Parse the raw signed session token out of a `Cookie` header value (BFF slice
/// R1b — the cookie now carries a `zeroship-sess+jwt`, not a UUID). Returns the token
/// string for the cookie arm to verify LOCALLY via
/// [`crate::session_token::Verifier`] — no DB round-trip.
#[must_use]
pub fn parse_app_session_cookie(cookie_header: &str) -> Option<String> {
    let name = app_session_cookie_name();
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

/// Stash cookie name (`__Host-` prefix requires `Secure`).
pub const STASH_COOKIE: &str = "__Host-zs_oidc_stash";

/// 10-minute window for the OIDC dance to complete. After this the user
/// has to re-initiate.
pub const STASH_MAX_AGE_SECS: i64 = 600;

#[must_use]
pub const fn stash_cookie_name() -> &'static str {
    STASH_COOKIE
}

/// Build the `Set-Cookie` header value for the OIDC stash.
#[must_use]
pub fn set_stash_cookie(value: &str) -> String {
    let name = stash_cookie_name();
    format!("{name}={value}; Path=/; HttpOnly; SameSite=Lax; Secure; Max-Age={STASH_MAX_AGE_SECS}")
}

/// Clear the OIDC stash cookie. Set on the callback response so the
/// short-lived stash doesn't linger after the dance completes.
#[must_use]
pub fn clear_stash_cookie() -> String {
    let name = stash_cookie_name();
    format!("{name}=; Path=/; HttpOnly; SameSite=Lax; Secure; Max-Age=0")
}

/// Parse the raw stash value out of a `Cookie` header. Returns the
/// signed-blob string; pass it to `Stash::decode` (via
/// `OidcRp::finish_callback`) to verify and recover the payload.
#[must_use]
pub fn parse_stash_cookie(cookie_header: &str) -> Option<String> {
    let name = stash_cookie_name();
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
    /// Serialised even when absent, as `null`. The SDK's `User` declares
    /// `avatar: string | null`, so dropping the key made `user.avatar === null`
    /// and `"avatar" in user` answer differently under `pnpm dev` than deployed.
    pub avatar: Option<&'a str>,
    pub email_verified: bool,
    /// OAuth scopes granted to THIS app for this user. A PERMANENT
    /// kernel-contract field on the `ZeroShip-User`
    /// projection: app code reads it via `env.auth.getUser().scopes`. Sourced
    /// from the raw-OP token's `scope` claim or signed session/anchor
    /// `granted_scopes`. Always present
    /// (empty when the token/session carries no scopes), so the worker JSON
    /// shape is stable across every auth arm.
    #[serde(default)]
    pub scopes: Vec<&'a str>,
}

/// Serialize the authenticated user as
/// `base64(JSON).<request_id>.<iat>.<kid>.<base64url-ed25519-signature>` for
/// the `ZeroShip-User` header. The worker decodes the base64 portion and
/// verifies the signature under the GATEWAY's published public key before
/// trusting the identity.
///
/// Signing prevents a caller with direct network access to the worker
/// from forging a user identity, even if the worker's endpoint bearer-auth
/// were ever bypassed. The request id and timestamp binding limit replay if
/// a header leaks through logs or a proxy.
///
/// # That guarantee is now true, and was not
///
/// The paragraph above shipped for as long as this function has existed, and
/// under the previous envelope it was FALSE BY CONSTRUCTION: the signature was
/// an HMAC keyed by `worker_key`, the very secret that bearer-authenticated the
/// dispatch endpoint. A caller who had bypassed - or simply held - that bearer
/// held the minting key too, so "even if the bearer were bypassed" described a
/// defence that did not exist. Worse, an empty `worker_key` disabled the bearer
/// check while the worker went on verifying envelopes under the same empty key.
///
/// The identity envelope is asymmetric as of this change
/// ([`zeroship_core::user_envelope`]). The gateway holds the only private key
/// that can produce one; the worker holds the public half and can check but not
/// mint. The claim is a property of the key model now rather than a hope about
/// the transport.
///
/// Returns `None` when this gateway loaded no service key material and so
/// cannot sign. Callers MUST fail the request rather than forward an unsigned
/// identity - and such a gateway also mints no dispatch credential, so the
/// worker refuses the hop outright.
#[must_use]
pub fn encode_user_header(
    user: &WorkerUser<'_>,
    signer: Option<&zeroship_core::user_envelope::UserEnvelopeSigner>,
    request_id: uuid::Uuid,
) -> Option<String> {
    let signer = signer?;
    let json = serde_json::to_string(user).unwrap_or_default();
    Some(signer.sign(json.as_bytes(), request_id))
}

#[cfg(test)]
mod tests;
