//! Authentication helpers used by the resource-tree dispatch path.
//!
//! `resolve_auth` is the gate; the other two helpers
//! (`jwt_subject_unverified`, `extract_session_cookie`) are pure
//! parsing utilities that other modules also reach for (subscription
//! affinity, per-rule rate-limit bucket derivation).

use std::sync::Arc;

use ntex::web::HttpRequest;
use uuid::Uuid;

use crate::oidc_rp;
use crate::GateState;

/// Outcome of the per-request auth gate. The dispatch handler consumes
/// this and either proceeds with the resolved `ZeroShip-User` header,
/// short-circuits with a 401 (API client) or kicks off the OIDC dance
/// (HTML navigation).
#[derive(Debug)]
pub(crate) enum AuthOutcome {
    /// Policy is `Anon` (request passes without identity) OR the
    /// policy required `User`/`Admin` and a user credential validated.
    /// `user_header` is `Some(...)` whenever identity was actually
    /// resolved — even on `Anon` resources, so the worker can still
    /// see the authenticated user when present.
    Allowed { user_header: Option<String> },
    /// Policy required `User`/`Admin` and no valid user credential was found.
    /// Caller decides between a 401 (API) and a 302 → op (HTML).
    Unauthenticated,
    /// A raw OP bearer resolved a real user, but the route has no
    /// `sector_identifier` and is not fully provisioned for OAuth. Fail closed
    /// with `503 client_not_provisioned` until control finishes provisioning.
    ClientNotProvisioned,
    /// The request authenticated successfully (any arm), but the matched
    /// route declares `required_scopes` the principal's granted `scopes`
    /// do NOT cover (RFC 6750 §3.1). Distinct
    /// from `Unauthenticated`: identity is fine, the *grant* is too
    /// narrow. The caller answers `403 scope_required` (JSON body) with a
    /// `WWW-Authenticate: Bearer error="insufficient_scope"` challenge and
    /// lists the required scopes. Reached ONLY by an AUTHENTICATED principal
    /// on a `User`/`Admin` route — an unauthenticated request is gated by the
    /// `Anon`/`User`/`Admin` policy first (401/redirect), and an `Anon`
    /// (public) route never scope-gates an authenticated visitor.
    InsufficientScope { required: Vec<String> },
}

/// The per-request family-marker revocation decision, routed through the
/// short-TTL [`RevocationCache`](zeroship_authz::wrapper_revocation::RevocationCache)
/// (BFF reshape R1d).
///
/// `NotRevoked` / `Revoked` are the two clean answers; `Unavailable` means a
/// cache MISS hit a DB error (or no pooled connection) — every arm maps it to
/// the SAME fail-closed rejection the un-cached direct read used. Keeping it a
/// distinct variant (rather than collapsing into `Revoked`) keeps the arms'
/// log lines accurate ("revocation check failed" vs "family revoked").
enum RevocationDecision {
    NotRevoked,
    Revoked,
    Unavailable,
}

/// Family-marker revocation gate for the per-request auth arms (cookie /
/// raw OP Bearer), routed through the short-TTL
/// read-through cache (R1d).
///
/// On a FRESH cache hit the decision is computed LOCALLY (`revoked_after >
/// iat`) with NO DB round-trip — this is the steady-state win that removes the
/// last per-request DB read from the cookie hot path. On a MISS we check out a
/// pooled connection, load the family's latest `revoked_after` via
/// [`revoked_after_for`](zeroship_authz::wrapper_revocation::revoked_after_for),
/// cache it (negative results included — that is the whole point), and decide
/// locally.
///
/// Fail-closed: a MISS that cannot reach the DB (pool checkout/get failure) or
/// whose query errors returns [`RevocationDecision::Unavailable`] WITHOUT
/// caching anything, so the next request retries the DB rather than serving a
/// guessed answer. A cached "not revoked" entry MAY serve through a brief DB
/// blip until its TTL expires (a small availability gain), after which a
/// refresh-miss + DB-down fails closed again.
///
/// The caller MUST have already confirmed `state.db.is_some()`; this is only
/// reached on the DB-configured path (the smoke/no-DB path skips revocation
/// entirely, exactly as before).
async fn family_revocation_decision(
    state: &Arc<GateState>,
    db_cfg: &crate::db::DbConfig,
    client_id: &str,
    sub: &str,
    iat: i64,
) -> RevocationDecision {
    let now = std::time::Instant::now();

    // 1. Fast path: a fresh cache entry answers locally, no DB.
    if let Some(revoked_after) = state.revocation_cache.get(client_id, sub, now) {
        return if zeroship_authz::wrapper_revocation::family_revoked_at(revoked_after, iat) {
            RevocationDecision::Revoked
        } else {
            RevocationDecision::NotRevoked
        };
    }

    // 2. Miss: load the marker from the DB and populate the cache. Hold the
    //    pooled connection only across this single lookup.
    let pool = match crate::db::checkout(db_cfg).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "revocation: pg pool checkout failed");
            return RevocationDecision::Unavailable;
        }
    };
    let conn = match pool.get().await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "revocation: pg pool get failed");
            return RevocationDecision::Unavailable;
        }
    };
    match zeroship_authz::wrapper_revocation::revoked_after_for(&conn, client_id, sub).await {
        Ok(revoked_after) => {
            // Cache the loaded marker (negative caching included) keyed on
            // `now`; re-read `Instant::now()` is unnecessary — the lookup is
            // fast and `now` is a tight upper bound on freshness.
            state.revocation_cache.store(client_id, sub, revoked_after, now);
            if zeroship_authz::wrapper_revocation::family_revoked_at(revoked_after, iat) {
                RevocationDecision::Revoked
            } else {
                RevocationDecision::NotRevoked
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, sub = %sub, "revocation check failed");
            RevocationDecision::Unavailable
        }
    }
}

/// Resolve the per-request auth gate and protected-route scope requirements.
/// A raw OP Bearer token is evaluated before the signed session cookie; either
/// credential can produce the HMAC-signed `ZeroShip-User` header forwarded to
/// the worker. Recognized but invalid user-session Bearers may continue without
/// identity on anonymous routes, while protected routes fail closed. Other
/// Bearer schemes are rejected on every route.
///
/// Without a Bearer credential, the gateway verifies the signed session cookie
/// locally and applies the same-origin anti-CSRF gate to state-changing requests.
/// A fresh pushed lifecycle and family-cutoff snapshot is required in every
/// mode. When a database is configured, the per-app family marker is also
/// checked through the read-through cache; only that secondary check is skipped
/// in no-database smoke mode.
///
/// Anonymous routes allow a request without resolved identity. `User` and `Admin`
/// routes require one, and any declared `required_scopes` must be covered by the
/// authenticated principal's scopes.
pub(crate) async fn resolve_auth(
    req: &HttpRequest,
    state: &Arc<GateState>,
    policy: &crate::compiled::EffectivePolicy,
    request_id: &Uuid,
    oauth_client_id: Option<&str>,
    sector_identifier: Option<&str>,
) -> AuthOutcome {
    // Resolve identity first (Bearer / cookie arms, untouched).
    let outcome = resolve_auth_inner(
        req,
        state,
        policy,
        request_id,
        oauth_client_id,
        sector_identifier,
    )
    .await;

    // Route-level scope enforcement follows RFC 6750 §3.1. The matched
    // route's `required_scopes` (compiled from the
    // manifest, unioned along the inheritance chain) gate an AUTHENTICATED
    // principal ONLY: once an arm resolved a `ZeroShip-User` header, the
    // principal's granted `scopes`, encoded in that header, MUST
    // be a superset, else `403 scope_required`. An UNAUTHENTICATED request
    // never reaches this gate — `Unauthenticated`/`ClientNotProvisioned`
    // pass through unchanged, gated by the Anon/User/Admin policy first
    // (401/redirect). Empty `required_scopes` ⇒ no gate (unchanged behavior).
    // The arms stay scope-agnostic; we read the just-resolved header's scopes
    // here (the same wire form the worker consumes) so there is ONE
    // enforcement point regardless of which arm authenticated.
    //
    // CRITICAL: the gate fires ONLY on `User`/`Admin` routes. An `Anon` route
    // is part of the app's PUBLIC surface (HTML/JS/CSS, SSR, public RPCs); it
    // can still resolve a `ZeroShip-User` when a session is present, but it
    // must NEVER scope-403 an authenticated visitor — otherwise a logged-in
    // browser whose session lacks a scope inherited from a broad `*`
    // parent would get 403 on public pages a logged-OUT user loads fine. That
    // is the "logged-in is worse than anonymous on public routes" footgun the
    // Invalid-Bearer fix removed; scope gating must not re-introduce
    // it. Scopes on `*` therefore constrain only the protected (`User`/`Admin`)
    // descendants, exactly like the auth level itself.
    if policy.required_scopes.is_empty()
        || matches!(policy.auth, zeroship_bundle::AuthLevel::Anon)
    {
        return outcome;
    }
    if let AuthOutcome::Allowed {
        user_header: Some(header),
    } = &outcome
    {
        let granted = decode_header_scopes(state, header);
        if !scopes_satisfied(&granted, &policy.required_scopes) {
            tracing::warn!(
                required = ?policy.required_scopes,
                granted = ?granted,
                "route-level required_scopes not satisfied — 403 scope_required"
            );
            return AuthOutcome::InsufficientScope {
                required: policy.required_scopes.clone(),
            };
        }
    }
    outcome
}

/// Whether `granted` is a superset of every scope in `required`. Empty
/// `required` ⇒ trivially satisfied (no scope gate). Exact string match
/// per scope (OAuth scopes are opaque tokens; no hierarchy / wildcards).
fn scopes_satisfied(granted: &[String], required: &[String]) -> bool {
    required
        .iter()
        .all(|need| granted.iter().any(|have| have == need))
}

/// Recover the `scopes` vector from a freshly-built `ZeroShip-User`
/// header, the scope source of truth set by whichever arm authenticated.
/// Verifies the MAC under the worker key and JSON-parses the
/// `scopes` array — the same path the worker uses. A verify/parse failure
/// yields `[]`, which fails the scope gate closed (a route demanding a
/// scope rejects an unreadable principal rather than waving it through).
fn decode_header_scopes(state: &Arc<GateState>, header: &str) -> Vec<String> {
    let Some(value) = decode_user_header(state, header) else {
        return Vec::new();
    };
    value
        .get("scopes")
        .and_then(|s| s.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Verify a freshly-built `ZeroShip-User` header under the worker key and
/// return its decoded claims object — the SAME bytes, verified the SAME
/// way, that the worker will read as the request's principal.
///
/// Reading identity back out of the header rather than threading it
/// separately keeps ONE source of truth: whatever arm authenticated, the
/// principal the gateway acts on is exactly the principal the worker
/// sees, so the two can never drift. A verify/parse failure yields `None`
/// and every caller MUST fail closed on it.
fn decode_user_header(state: &Arc<GateState>, header: &str) -> Option<serde_json::Value> {
    let key = state.config.worker_key.as_bytes();
    let json = zeroship_core::auth::verify_zeroship_user_header(key, header)?;
    serde_json::from_str::<serde_json::Value>(&json).ok()
}

/// The authenticated principal's stable per-app identity (the pairwise
/// `pws_…` subject) recovered from its `ZeroShip-User` header.
///
/// Used to partition the idempotency dedupe namespace. Returns `None`
/// when the header cannot be verified/parsed or carries no non-empty
/// `id`; callers must NOT substitute an unpartitioned key in that case.
pub(super) fn user_header_subject(state: &Arc<GateState>, header: &str) -> Option<String> {
    decode_user_header(state, header)?
        .get("id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

#[allow(clippy::too_many_arguments)]
async fn resolve_auth_inner(
    req: &HttpRequest,
    state: &Arc<GateState>,
    policy: &crate::compiled::EffectivePolicy,
    request_id: &Uuid,
    oauth_client_id: Option<&str>,
    sector_identifier: Option<&str>,
) -> AuthOutcome {
    use zeroship_bundle::AuthLevel;

    // 1. Bearer arm. Ordered BEFORE the cookie arm.
    //    Serves NON-BROWSER OAuth clients (CLI /
    //    server-to-server) presenting a raw OP access JWT; the SPA uses
    //    the signed session cookie, not Bearer. Discriminates the raw OP
    //    access JWT from the reserved API-key path:
    //
    //      - `Allowed(header)`     → short-circuit, fully authenticated.
    //      - `Invalid`            → a recognized raw OP user-session token
    //        that failed verify/binding/revocation. By policy: anonymous on
    //        an `Anon` route (a non-browser client may auto-attach Bearer,
    //        and an expired-but-present Bearer must not break public pages),
    //        401 on `User`/`Admin`.
    //      - `NotUserSession`     → a Bearer that is not a raw OP JWT
    //        (e.g. a future `zsk_…` API key). Reserved path → 401 on EVERY
    //        route, including `Anon` (it asserts a DIFFERENT scheme, not an
    //        expired user session).
    //      - `NotBearer`          → no `Authorization: Bearer`. Fall
    //        through to the cookie arm.
    match resolve_bearer_user_header(req, state, request_id, oauth_client_id, sector_identifier)
        .await
    {
        BearerOutcome::Allowed(header) => {
            return AuthOutcome::Allowed {
                user_header: Some(header),
            };
        }
        BearerOutcome::ClientNotProvisioned => {
            // A valid raw OP Bearer user reached an incompletely provisioned
            // OAuth route. Fail closed.
            return AuthOutcome::ClientNotProvisioned;
        }
        BearerOutcome::NotUserSession => {
            // Reserved scheme — 401 regardless of route policy.
            return AuthOutcome::Unauthenticated;
        }
        BearerOutcome::Invalid => match policy.auth {
            // Expired/invalid auto-attached user-session Bearer: serve the
            // public page anonymously; the SDK's next refresh re-auths.
            AuthLevel::Anon => {
                return AuthOutcome::Allowed { user_header: None };
            }
            // INTENTIONAL: on a `User`/`Admin` route an
            // Invalid Bearer 401s and
            // does NOT fall through to the cookie arm — even if the request
            // also carries a valid cookie session. A client that presented
            // (and failed) a user-session Bearer cannot silently re-assert a
            // DIFFERENT identity via a cookie; the SDK refreshes its Bearer
            // before firing, so a stale-Bearer + valid-cookie collision is a
            // bug to surface (401), not to paper over. See
            // `resolve_auth_invalid_bearer_on_user_route_does_not_use_cookie`.
            AuthLevel::User | AuthLevel::Admin => {
                return AuthOutcome::Unauthenticated;
            }
        },
        BearerOutcome::NotBearer => {}
    }

    let session_user_header = resolve_app_session_user_header_inner(
        req,
        state,
        request_id,
        oauth_client_id,
    )
    .await;
    let session_user_header = match session_user_header {
        CookieOutcome::Allowed(header) => {
            // BFF anti-CSRF gate (spec §1.2/§2.3, P3). The SameSite=Lax
            // `__Host-zeroship_app_session` cookie is the SPA's LIVE credential, so a
            // state-changing same-site `POST /api/*` ridden by an XSS / a
            // cross-site top-level form-POST is the residual CSRF risk the BFF
            // trade explicitly bounds. We require, for state-changing methods
            // authenticated PURELY by this cookie: an `Origin` present and an
            // exact match of the app's own origin or a configured trusted
            // origin, plus fetch metadata consistent with that match when the
            // browser sends it. GET/HEAD are exempt (non-state-changing). No
            // mandatory custom header - raw-JS deploys
            // that POST a plain form must still work, so the Origin match (which
            // the browser sets and script cannot forge cross-site) carries the
            // defense. A failure REJECTS the cookie credential for this request
            // (treated as if no session resolved) rather than 403, so a public
            // (`Anon`) route still serves anonymously and a `User` route 401s —
            // identical posture to a missing cookie.
            if cookie_csrf_rejected(req, &state.config) {
                None
            } else {
                Some(header)
            }
        }
        CookieOutcome::None => None,
    };
    match policy.auth {
        AuthLevel::Anon => AuthOutcome::Allowed {
            user_header: session_user_header,
        },
        AuthLevel::User | AuthLevel::Admin => {
            if session_user_header.is_some() {
                AuthOutcome::Allowed {
                    user_header: session_user_header,
                }
            } else {
                AuthOutcome::Unauthenticated
            }
        }
    }
}

/// Whether a state-changing request authenticated PURELY by the SameSite=Lax
/// `__Host-zeroship_app_session` cookie must be REJECTED on anti-CSRF grounds (BFF
/// spec §1.2/§2.3, P3).
///
/// Returns `true` (reject the cookie credential) when the method is
/// state-changing (`POST`/`PUT`/`PATCH`/`DELETE`) AND the same-origin posture
/// fails: a missing `Origin`, an `Origin: null`, a foreign `Origin` (no
/// substring/subdomain match, never reflected), or inconsistent fetch
/// metadata. `GET`/`HEAD`/`OPTIONS` are exempt
/// (non-state-changing). When `true`, the caller drops the resolved
/// `ZeroShip-User` so the request is treated as if no session was present
/// (anon on a public route, 401 on a protected route) — never a leaked
/// cross-site mutation.
///
/// No custom-header requirement here (unlike `auth_token::same_origin_guard`):
/// the dispatch path serves raw-JS deploys that legitimately POST a plain form,
/// so the browser-set `Origin` (which page script cannot forge cross-site)
/// carries the defense. Bearer-authenticated requests never reach this
/// gate — they are resolved on the earlier arms and carry an explicit,
/// non-auto-attached `Authorization` header that is itself CSRF-proof.
fn cookie_csrf_rejected(req: &HttpRequest, config: &crate::GateConfig) -> bool {
    let method = req.method();
    let state_changing = matches!(
        *method,
        ntex::http::Method::POST
            | ntex::http::Method::PUT
            | ntex::http::Method::PATCH
            | ntex::http::Method::DELETE
    );
    if !state_changing {
        return false;
    }

    let host = req
        .headers()
        .get(http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    // Origin: required + exact-match. Missing/null/foreign ⇒ reject.
    let origin_match = match req
        .headers()
        .get(http::header::ORIGIN)
        .and_then(|v| v.to_str().ok())
    {
        Some(origin) => match config.classify_origin(origin, host) {
            Some(origin_match) => origin_match,
            None => return true,
        },
        _ => return true,
    };

    // Sec-Fetch-Site: when present, require metadata consistent with the exact
    // Origin class. Its absence remains advisory for older clients.
    if let Some(value) = req.headers().get("sec-fetch-site") {
        let sfs = value.to_str().unwrap_or("");
        if !origin_match.accepts_sec_fetch_site(sfs) {
            return true;
        }
    }

    false
}

/// Outcome of the Bearer arm ([`resolve_bearer_user_header`]). The five
/// variants map directly onto the route-policy gate in [`resolve_auth`]:
/// see the comment at the Bearer-arm call site for the policy table.
#[derive(Debug)]
enum BearerOutcome {
    /// A valid raw OP user-session Bearer that verified, bound to this
    /// app, and passed revocation. Carries the signed `ZeroShip-User`
    /// header. Fully authenticated regardless of policy.
    Allowed(String),
    /// A recognized raw OP user-session token (`iss == oidc_rp.issuer`)
    /// that FAILED verification / per-app binding / revocation, OR was
    /// presented while the route is un-provisioned (`oauth_client_id ==
    /// None`). Treated as no-identity: anonymous on `Anon`, 401 on
    /// `User`/`Admin`.
    Invalid,
    /// A Bearer token whose `iss` is not OP — the reserved API-key path
    /// (a future `zsk_…` shape). 401 on every route.
    NotUserSession,
    /// A valid raw OP Bearer user, but the route has no `sector_identifier`
    /// and is not fully provisioned for OAuth. Fail closed (`503`).
    ClientNotProvisioned,
    /// No `Authorization: Bearer` header. Fall through to the cookie arm.
    NotBearer,
}

/// Peek the unverified `iss` claim out of a JWT payload. Mirrors
/// [`jwt_subject_unverified`] but for `iss` — used ONLY to discriminate
/// a raw OP access JWT (`iss == oidc_rp.issuer`) from the reserved
/// API-key path; the actual trust decision is the subsequent signature
/// check, so reading `iss` before verification is safe. Returns `None`
/// for any structural parse failure (e.g. an opaque/non-JWT API key).
fn jwt_issuer_unverified(jwt: &str) -> Option<String> {
    use base64::Engine as _;
    let mut parts = jwt.split('.');
    let _header = parts.next()?;
    let payload_b64 = parts.next()?;
    let _sig = parts.next()?;
    if parts.next().is_some() {
        return None; // more than 3 segments — not a compact JWS
    }
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload_b64)
        .ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    v.get("iss").and_then(|s| s.as_str()).map(str::to_string)
}

/// Resolve the `ZeroShip-User` header from an `Authorization: Bearer`
/// access token. This serves **non-browser** OAuth clients
/// (CLI / server-to-server) that present a RAW OP access JWT; the SPA
/// uses the signed session cookie, not Bearer.
///
/// Discriminates by the **unverified** `iss` peek:
///   - `iss == state.oidc_rp.issuer` (OP) → RAW-OP path: verify the
///     JWT signature locally via the gateway's JWKS cache
///     (`state.oidc_rp.verify_access_token`), then bind per-app on the
///     `client_id` claim (RFC 9068 §3) and require the route's resource
///     audience in `aud`.
///   - anything else → `NotUserSession` (reserved API-key path).
///
/// **Per-app binding** is the critical safety property: a token minted
/// for app A must be rejected at app B's host. `oauth_client_id` is the
/// matched route's expected client. When it is `None` (the app is not
/// yet provisioned), a raw OP token cannot be bound to a
/// missing client and yields `Invalid`.
///
/// **Revocation** uses the per-app family marker
/// (`zeroship.token_revocations`, keyed on `(client_id, sub)` with `sub` as
/// TEXT). The arm rejects a token when a row exists for its
/// `(client_id, pws_)` with `revoked_after > token.iat`; `sub` being TEXT
/// is what lets the per-app `pws_…` subject be matched (the UUID-only
/// subject denylist could not). Per-app scoping means a revocation on app A
/// leaves the same user's tokens on app B valid.
///
/// Pairwise binding: the OP issuer already stamps app access tokens with a
/// `pws_` subject. This path requires that shape and forwards it unchanged.
/// Treating it as a global UUID and deriving again would produce a different
/// identity that cannot match revocation or lifecycle state.
async fn resolve_bearer_user_header(
    req: &HttpRequest,
    state: &Arc<GateState>,
    request_id: &Uuid,
    oauth_client_id: Option<&str>,
    sector_identifier: Option<&str>,
) -> BearerOutcome {
    // a. Extract `Authorization: Bearer <token>`.
    let Some(auth_header) = req
        .headers()
        .get(http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    else {
        return BearerOutcome::NotBearer;
    };
    let Some(token) = auth_header.strip_prefix("Bearer ") else {
        return BearerOutcome::NotBearer;
    };

    // b. Peek the unverified `iss` to pick the verifier.
    let Some(iss) = jwt_issuer_unverified(token) else {
        // Not even a JWT (opaque API key) — reserved path.
        return BearerOutcome::NotUserSession;
    };

    if iss == state.oidc_rp.issuer {
        // ── RAW-OP path (RFC 9068, non-browser clients) ────────────
        let Some(expected_client_id) = oauth_client_id else {
            tracing::warn!(
                "raw OP Bearer presented but route has no oauth_client_id; rejecting"
            );
            return BearerOutcome::Invalid;
        };
        let claims = match state.oidc_rp.verify_access_token(token).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "raw OP Bearer verification failed");
                return BearerOutcome::Invalid;
            }
        };
        // Authorized-party binding: native OP access tokens always carry
        // `client_id` (RFC 9068 §3). Do not fall back to `aud`: `aud` is the
        // resource-server audience, and an ID token has `aud == client_id`.
        if claims.client_id.as_deref() != Some(expected_client_id) {
            tracing::warn!(
                client_id = ?claims.client_id,
                expected = %expected_client_id,
                "raw OP Bearer client_id binding failed — rejecting"
            );
            return BearerOutcome::Invalid;
        }

        // Resource-server audience binding: the native OP stamps access tokens
        // with `app:{app_id}` for per-app clients. `client_id` identifies the
        // authorized party; `aud` identifies the resource server.
        let Some(expected_app_id) =
            zeroship_core::typed_id::app_id_from_oauth_client_id(expected_client_id)
        else {
            tracing::warn!(
                expected = %expected_client_id,
                "raw OP Bearer route client_id is not a per-app OAuth client — rejecting"
            );
            return BearerOutcome::Invalid;
        };
        let expected_resource_audience = format!("app:{expected_app_id}");
        if !claims
            .aud
            .iter()
            .any(|aud| aud == &expected_resource_audience)
        {
            tracing::warn!(
                aud = ?claims.aud,
                expected = %expected_resource_audience,
                client_id = %expected_client_id,
                "raw OP Bearer resource audience binding failed — rejecting"
            );
            return BearerOutcome::Invalid;
        }
        if claims.sub.is_empty() {
            tracing::warn!("raw OP Bearer token missing sub — rejecting");
            return BearerOutcome::Invalid;
        }
        // Require the complete OAuth route shape even though this verified
        // token already carries its issuer-projected pairwise subject.
        let Some(_sector) = sector_identifier else {
            return BearerOutcome::ClientNotProvisioned;
        };
        if !zeroship_core::auth::is_pairwise_subject(&claims.sub) {
            tracing::warn!(sub = %claims.sub, "raw OP Bearer sub is not pairwise; rejecting");
            return BearerOutcome::Invalid;
        }
        let pws_sub = claims.sub.clone();
        if !credential_authentication_allows(
            state,
            expected_client_id,
            &pws_sub,
            claims.iat,
        ) {
            tracing::warn!(sub = %pws_sub, "raw OP Bearer principal lifecycle rejected");
            return BearerOutcome::Invalid;
        }
        // Cross-node per-app family-marker revocation. Keyed on
        // `(expected_client_id, pws_sub)` — the SAME `(client_id, pws_)` shape
        // the writers use. Per-app: revoking this user on app A leaves their
        // raw OP access on app B valid (app B's `pws_` differs). We key on
        // `expected_client_id` (the route's bound client) because the binding
        // above proved the token agrees and the marker is written against the
        // route's client.
        if let Some(db_cfg) = state.db.as_ref() {
            // Routed through the short-TTL read-through `revocation_cache`,
            // the same as the cookie arm. A fresh hit decides
            // locally with no DB round-trip; a miss loads + caches the marker.
            match family_revocation_decision(
                state,
                db_cfg,
                expected_client_id,
                &pws_sub,
                claims.iat,
            )
            .await
            {
                RevocationDecision::NotRevoked => {}
                RevocationDecision::Revoked => {
                    tracing::warn!(
                        client_id = %expected_client_id,
                        sub = %pws_sub,
                        "raw OP Bearer family revoked after iat"
                    );
                    return BearerOutcome::Invalid;
                }
                // Fail-closed: cache-miss + DB error rejects as before.
                RevocationDecision::Unavailable => return BearerOutcome::Invalid,
            }
        }
        if !credential_authentication_allows(
            state,
            expected_client_id,
            &pws_sub,
            claims.iat,
        ) {
            tracing::warn!(sub = %pws_sub, "raw OP Bearer lifecycle changed during verification");
            return BearerOutcome::Invalid;
        }
        // The issuer already projected the subject. Forward that verified id
        // unchanged, but never trust profile claims from a bearer as an app
        // identity projection: an unexpected email claim could expose the real
        // inbox. The access-token arm therefore fails closed on email.
        let mut owned = build_worker_user_from_access_claims(&claims);
        owned.id = pws_sub;
        let user: oidc_rp::WorkerUser<'_> = (&owned).into();
        return BearerOutcome::Allowed(oidc_rp::encode_user_header(
            &user,
            &state.config.worker_key,
            *request_id,
        ));
    }

    // Not a raw OP JWT — reserved API-key path (e.g. a future `zsk_…`
    // shape). Stays 401.
    BearerOutcome::NotUserSession
}

/// Materialise a `WorkerUser` from a verified raw OP access JWT.
///
/// `id` is the issuer-projected pairwise `sub`. Email is always empty because
/// this arm has no trusted relay-alias source and must never forward a real
/// inbox carried by an unexpected token claim.
fn build_worker_user_from_access_claims(claims: &crate::oidc_rp::AccessClaims) -> OwnedWorkerUser {
    OwnedWorkerUser {
        id: claims.sub.clone(),
        email: String::new(),
        name: claims.name.clone().unwrap_or_default(),
        email_verified: false,
        // Raw-OP arm: scopes come from the access token's `scope` claim.
        scopes: split_scope_claim(claims.scope.as_deref().unwrap_or_default()),
    }
}

/// Owned counterpart to [`oidc_rp::WorkerUser`] — the public type
/// borrows, but our build sites need to hold the strings somewhere on
/// the stack so the borrow stays valid through
/// [`oidc_rp::encode_user_header`]. `From` makes the borrow conversion
/// implicit at call sites.
struct OwnedWorkerUser {
    id: String,
    email: String,
    name: String,
    email_verified: bool,
    /// Granted scopes. Owned here so the borrowed
    /// `WorkerUser.scopes` can survive through `encode_user_header`.
    scopes: Vec<String>,
}

impl<'a> From<&'a OwnedWorkerUser> for oidc_rp::WorkerUser<'a> {
    fn from(owned: &'a OwnedWorkerUser) -> Self {
        oidc_rp::WorkerUser {
            id: &owned.id,
            email: &owned.email,
            name: &owned.name,
            avatar: None,
            email_verified: owned.email_verified,
            scopes: owned.scopes.iter().map(String::as_str).collect(),
        }
    }
}

/// Split an OAuth `scope` claim (space-delimited) into a scope vector. Empty
/// or whitespace-only ⇒ empty vec, so `WorkerUser.scopes` is `[]` (never a
/// `[""]`) when the token carries no scopes.
fn split_scope_claim(scope: &str) -> Vec<String> {
    scope.split_whitespace().map(str::to_string).collect()
}

/// Outcome of the cookie arm ([`resolve_app_session_user_header_inner`]).
#[derive(Debug)]
enum CookieOutcome {
    /// A valid SIGNED session cookie verified LOCALLY (signature, `kid`, `exp`,
    /// and `app` == route client), its principal is allowed by the fresh
    /// lifecycle snapshot, and its `(client_id, pws_)` family is not revoked.
    /// Carries the signed `ZeroShip-User` header built directly from the
    /// cookie claims (NO DB read for identity).
    Allowed(String),
    /// No cookie, no signing key configured, a verification failure
    /// (tampered/expired/wrong-app/wrong-kid), a revoked family marker, or a
    /// non-`pws_` subject. The caller treats this as no-identity
    /// (fail-closed-as-unauthenticated).
    None,
}

/// Resolve the `ZeroShip-User` header value from the SIGNED STATELESS session
/// cookie (BFF redesign **slice R1b** — the addendum). The `__Host-zeroship_app_session`
/// cookie is a gateway-signed `zeroship-sess+jwt` identity assertion, verified LOCALLY
/// here on every request — **no `sessions::validate`, no per-request DB read for
/// identity**.
///
/// Steps (identity verification and lifecycle enforcement are edge-local):
///  1. Parse the signed cookie token (opaque to the parser).
///  2. Verify it LOCALLY via [`crate::session_token::Verifier`]: signature
///     (current OR previous `kid`), `iss`, `exp`, and `app` == the resolved
///     route's `oauth_client_id` (audience binding). A tampered / expired /
///     wrong-app / wrong-/unknown-`kid` cookie fails here → [`CookieOutcome::None`].
///     A `zeroship-sess+jwt` typ is required (an `at+jwt` access token is rejected by
///     the session verifier's typ gate).
///  3. Defense-in-depth: the cookie `sub` MUST be a `pws_…` pairwise subject
///     (every minter projects it; a non-`pws_` cookie is a mint bug → reject).
///  4. Require a fresh pushed lifecycle snapshot that allows this `pws_`.
///  5. Check the per-app family marker used by the Bearer arm. A fresh cache hit
///     is local; a cache miss reads the database and fails closed on error.
///     Smoke mode skips this marker check when no database is configured.
///  6. Emit `ZeroShip-User` DIRECTLY from the cookie claims (`id = pws_`, relay
///     alias `email`, `scopes`). The relay-alias swap + `pws_` projection
///     already happened at ISSUE time (`/session` / interactive callback); the
///     hot path does not re-derive them.
///
/// `gateway_sessions` is NO LONGER read here (the cookie is self-contained; the
/// revocation truth is the family marker). The binding key is the cookie's `app`
/// claim — so this arm takes NO `app_id`/`sector_identifier`: the cookie carries
/// its own per-app `pws_` subject and `app` (client_id) binding, and neither the
/// route UUID nor the sector is consulted on the stateless path.
async fn resolve_app_session_user_header_inner(
    req: &HttpRequest,
    state: &Arc<GateState>,
    request_id: &Uuid,
    oauth_client_id: Option<&str>,
) -> CookieOutcome {
    let cookie_header = req
        .headers()
        .get("cookie")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let Some(token) = oidc_rp::parse_app_session_cookie(cookie_header) else {
        return CookieOutcome::None;
    };

    // A session cookie binds per-app on its `app` claim. Without an expected
    // client_id (un-provisioned app) we cannot bind, so refuse rather than
    // accept an unbound cookie — mirrors the raw OP Bearer arm.
    let Some(expected_client_id) = oauth_client_id else {
        tracing::warn!("signed session cookie presented but route has no oauth_client_id; rejecting");
        return CookieOutcome::None;
    };
    let Some(verifier) = state.session_verifier.as_ref() else {
        // No signing key ⇒ cannot verify a signed cookie. Fail closed.
        return CookieOutcome::None;
    };

    // LOCAL verify: signature (current/prev kid) + iss + exp + app == route.
    // NO DB, NO network. This is the hot-path stateless check.
    let claims = match verifier.verify(&token, expected_client_id) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "signed session cookie verification failed");
            return CookieOutcome::None;
        }
    };

    // Self-describing-subject invariant (matches the Bearer arm): the
    // cookie `sub` is ALWAYS a per-app `pws_…` (every minter projects it). A
    // non-`pws_` sub means a mint path failed to project — hard-reject so the
    // global identity can never leak through the session cookie.
    if !zeroship_core::auth::is_pairwise_subject(&claims.sub) {
        tracing::warn!(
            sub = %claims.sub,
            "signed session cookie sub is not a pws_ pairwise subject — rejecting"
        );
        return CookieOutcome::None;
    }
    if !credential_authentication_allows(state, &claims.app, &claims.sub, claims.iat) {
        tracing::warn!(sub = %claims.sub, "signed session cookie principal lifecycle rejected");
        return CookieOutcome::None;
    }
    // Revocation gate — the per-app family marker (spec §8.5), keyed on
    // `(client_id, pws_)` with `iat` as the binding instant. The SAME mechanism
    // the Bearer arm uses: a revoked family rejects a still-valid signed
    // cookie. Routed through the short-TTL read-through `revocation_cache`
    // (R1d): a fresh cache hit decides `> iat` LOCALLY with NO DB round-trip,
    // so the steady-state (no-revocation) cookie request is fully DB-free —
    // the last per-request DB read on the hot path is gone on a cache hit. A
    // cross-node revocation is honored within `<= REVOCATION_CACHE_TTL_SECS`;
    // same-node `/signout` busts the entry immediately. Skipped when no DB is
    // configured (smoke mode) — proving the verify path itself is DB-free for a
    // valid cookie.
    if let Some(db_cfg) = state.db.as_ref() {
        match family_revocation_decision(state, db_cfg, &claims.app, &claims.sub, claims.iat).await
        {
            RevocationDecision::NotRevoked => {}
            RevocationDecision::Revoked => {
                tracing::warn!(
                    client_id = %claims.app,
                    sub = %claims.sub,
                    "signed session cookie family was revoked after iat — rejecting"
                );
                return CookieOutcome::None;
            }
            // Fail-closed: a cache-miss that could not reach the DB rejects,
            // exactly as the un-cached direct read did.
            RevocationDecision::Unavailable => return CookieOutcome::None,
        }
    }
    if !credential_authentication_allows(state, &claims.app, &claims.sub, claims.iat) {
        tracing::warn!(sub = %claims.sub, "signed session cookie lifecycle changed during verification");
        return CookieOutcome::None;
    }
    // Emit ZeroShip-User DIRECTLY from the cookie claims — identity + scopes are
    // self-contained (the relay-alias swap + pws_ projection happened at issue
    // time). NO per-request projection, NO DB.
    let user = oidc_rp::WorkerUser {
        id: &claims.sub,
        email: &claims.email,
        name: &claims.name,
        avatar: claims.avatar.as_deref(),
        email_verified: claims.email_verified,
        scopes: claims.scopes.iter().map(String::as_str).collect(),
    };
    CookieOutcome::Allowed(oidc_rp::encode_user_header(
        &user,
        &state.config.worker_key,
        *request_id,
    ))
}

fn credential_authentication_allows(
    state: &crate::GateState,
    client_id: &str,
    subject: &str,
    issued_at: i64,
) -> bool {
    let freshness_budget = zeroship_core::readiness::staleness_budget(
        std::time::Duration::from_secs(state.config.poll_interval_secs),
    );
    state
        .routes
        .credential_authentication_allowed(client_id, subject, issued_at, freshness_budget)
}

/// Lift the `sub` claim out of a JWT *without* verification. This is
/// strictly for affinity hashing — a malicious client can pin a
/// different bucket for themselves but can't gain access to anyone
/// else's subscription state, because access control runs against
/// the verified token elsewhere (`resolve_auth` + the worker's own
/// auth context). Returns `None` for any structural parse failure.
pub(super) fn jwt_subject_unverified(jwt: &str) -> Option<String> {
    let mut parts = jwt.split('.');
    let _header = parts.next()?;
    let payload_b64 = parts.next()?;
    use base64::Engine;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload_b64)
        .ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    v.get("sub").and_then(|s| s.as_str()).map(|s| s.to_string())
}

/// Pull the app-session cookie value out of a Cookie header.
///
/// The cookie name is always `__Host-zeroship_app_session`. Returns `None`
/// when the cookie is missing or empty so callers can fall back to a different
/// discriminator (for example, session-keyed rate-limit buckets fall back to
/// the client IP).
pub(super) fn extract_session_cookie(cookie_header: Option<&str>) -> Option<String> {
    let s = cookie_header?;
    let prefix = format!("{}=", oidc_rp::app_session_cookie_name());
    let token = s
        .split(';')
        .map(|p| p.trim())
        .find(|p| p.starts_with(&prefix))?
        .strip_prefix(&prefix)?;
    if token.is_empty() {
        None
    } else {
        Some(token.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_session_cookie_handles_empty_value() {
        // `__Host-zeroship_app_session=` (empty value) → None, so the caller
        // falls back to IP. Treating empty as a real bucket key would
        // collapse every cookie-empty client into one shared bucket.
        assert_eq!(
            extract_session_cookie(Some("__Host-zeroship_app_session=")),
            None
        );
        assert_eq!(extract_session_cookie(None), None);
        assert_eq!(extract_session_cookie(Some("other=foo")), None);
    }

    #[test]
    fn extract_session_cookie_reads_app_session_value() {
        let id = uuid::Uuid::new_v4();
        let header = format!("foo=bar; __Host-zeroship_app_session={id}; baz=qux");
        let extracted = extract_session_cookie(Some(&header)).expect("present");
        assert_eq!(extracted, id.to_string());
    }

    #[test]
    fn extract_session_cookie_rejects_bare_name() {
        let id = uuid::Uuid::new_v4();
        let bare_header = format!("zeroship_app_session={id}");
        assert_eq!(extract_session_cookie(Some(&bare_header)), None);
    }

    // ─── Worker-user builders (raw OP Bearer) ─────────────────────
    //
    // The surviving arms map their verified claims straight onto a
    // `WorkerUser`. A regression in the field mapping (e.g. losing
    // `email_verified`, dropping `name`, mangling `scopes`) would silently
    // degrade the worker's view of the authenticated user — covered here.

    /// The raw OP Bearer arm carries the app's granted scopes from the
    /// access-token `scope` claim onto `WorkerUser.scopes`, and
    /// they survive the encode → verify → JSON-parse round-trip the worker
    /// performs.
    #[test]
    fn worker_user_scopes_round_trip_through_header() {
        let claims = crate::oidc_rp::AccessClaims {
            sub: "usr_global".into(),
            client_id: Some("oac_app".into()),
            aud: vec!["oac_app".into()],
            iat: 0,
            email: Some("a@b.test".into()),
            email_verified: Some(true),
            name: Some("Alice".into()),
            scope: Some("openid read:billing write:projects".into()),
            auth_time: None,
            amr: None,
        };
        let owned = build_worker_user_from_access_claims(&claims);
        assert_eq!(
            owned.scopes,
            vec!["openid", "read:billing", "write:projects"]
        );

        // Encode the WorkerUser as the worker would receive it, verify the MAC,
        // and JSON-parse it back — `scopes` must survive verbatim.
        let user: oidc_rp::WorkerUser<'_> = (&owned).into();
        let key = "worker-key-1234567890";
        let rid = Uuid::new_v4();
        let header = oidc_rp::encode_user_header(&user, key, rid);
        let json = zeroship_core::auth::verify_zeroship_user_header(key.as_bytes(), &header)
            .expect("MAC verifies");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("user json");
        assert_eq!(
            parsed["scopes"],
            serde_json::json!(["openid", "read:billing", "write:projects"]),
            "scopes must round-trip through the ZeroShip-User header"
        );
    }

    /// The raw OP Bearer arm carries `scope` from the access-token claims
    /// onto `WorkerUser.scopes`.
    #[test]
    fn build_worker_user_from_access_claims_carries_scopes() {
        let claims = crate::oidc_rp::AccessClaims {
            sub: "usr_global".into(),
            client_id: Some("oac_app".into()),
            aud: vec!["oac_app".into()],
            iat: 0,
            email: Some("u@x.test".into()),
            email_verified: Some(true),
            name: Some("Carol".into()),
            scope: Some("openid read:billing".into()),
            auth_time: None,
            amr: None,
        };
        let owned = build_worker_user_from_access_claims(&claims);
        assert_eq!(owned.id, "usr_global");
        assert_eq!(owned.scopes, vec!["openid".to_string(), "read:billing".to_string()]);
    }

    // ─── Bearer / cookie GateState fixtures ───────────────────────────
    //
    // The integration tests below build a real raw OP access token / signed
    // session cookie, and a real `GateState` (sans live PG / op) and drive
    // `resolve_bearer_user_header` / the cookie arm directly.

    fn test_broker_secret() -> crate::oidc_rp::BrokerSecret {
        crate::oidc_rp::BrokerSecret::from_bytes(
            b"gateway-router-auth-test-broker-master-32-bytes".to_vec(),
        )
        .expect("broker secret")
    }

    /// `BlobStore` stub for the test fixture — `GateState` requires
    /// one, but the auth path never reaches it.
    #[derive(Debug, Default)]
    struct StubBlobStore;

    #[async_trait::async_trait(?Send)]
    impl zeroship_bundle::BlobStore for StubBlobStore {
        async fn get_blob(
            &self,
            _h: &str,
        ) -> Result<bytes::Bytes, zeroship_bundle::BlobError> {
            Err(zeroship_bundle::BlobError::NotFound("unused".into()))
        }
        fn local_path(&self, _h: &str) -> Option<std::path::PathBuf> {
            None
        }
        async fn put_blob(
            &self,
            _h: &str,
            _d: &[u8],
        ) -> Result<zeroship_bundle::PutOutcome, zeroship_bundle::BlobError> {
            Ok(zeroship_bundle::PutOutcome::Wrote)
        }
        async fn put_blob_stream(
            &self,
            _h: &str,
            _s: u64,
            _r: &mut dyn std::io::Read,
        ) -> Result<zeroship_bundle::PutOutcome, zeroship_bundle::BlobError> {
            Ok(zeroship_bundle::PutOutcome::Wrote)
        }
        async fn has_blob(&self, _h: &str) -> Result<bool, zeroship_bundle::BlobError> {
            Ok(false)
        }

        async fn probe(&self) -> Result<(), zeroship_bundle::BlobError> {
            Ok(())
        }
        async fn get_blob_to_file(
            &self,
            _h: &str,
            _out: &compio::fs::File,
            _expected_size: Option<u64>,
            _max_bytes: u64,
        ) -> Result<u64, zeroship_bundle::BlobError> {
            Err(zeroship_bundle::BlobError::NotFound("unused".into()))
        }
        async fn put_manifest(
            &self,
            _a: &uuid::Uuid,
            _d: &str,
            _j: &[u8],
        ) -> Result<(), zeroship_bundle::BlobError> {
            Ok(())
        }
        async fn get_manifest(
            &self,
            _a: &uuid::Uuid,
            _d: &str,
        ) -> Result<bytes::Bytes, zeroship_bundle::BlobError> {
            Err(zeroship_bundle::BlobError::NotFound("unused".into()))
        }
        async fn delete_manifest(
            &self,
            _a: &uuid::Uuid,
            _d: &str,
        ) -> Result<bool, zeroship_bundle::BlobError> {
            Ok(false)
        }
        async fn delete_app_manifests(
            &self,
            _a: &uuid::Uuid,
        ) -> Result<(), zeroship_bundle::BlobError> {
            Ok(())
        }
    }

    /// Build a Gateway state with the signed-session-cookie issuer + verifier
    /// configured against the supplied signing key, and `OidcRp` pointed at a
    /// dead URL (the cookie-arm and policy tests never make a network call).
    fn build_state_with_session(
        signing: ed25519_dalek::SigningKey,
    ) -> std::sync::Arc<crate::GateState> {
        build_state_with_session_and_auth_ui_url(signing, "http://127.0.0.1:1")
    }

    fn build_state_with_session_and_auth_ui_url(
        signing: ed25519_dalek::SigningKey,
        auth_ui_url: &str,
    ) -> std::sync::Arc<crate::GateState> {
        build_state_with_session_and_auth_ui_url_and_db(signing, auth_ui_url, None)
    }

    fn build_state_with_session_and_auth_ui_url_and_db(
        signing: ed25519_dalek::SigningKey,
        auth_ui_url: &str,
        db: Option<crate::db::DbConfig>,
    ) -> std::sync::Arc<crate::GateState> {
        // Preserve the historical behavior: `auth_ui_url` drives the
        // OidcRp dial URL (and JWKS), with the issuer derived from it.
        let oidc_rp = crate::oidc_rp::OidcRp::new(
            auth_ui_url,
            test_broker_secret(),
            b"test-stash-key-32-bytes-long----".to_vec(),
        );
        build_state_with_session_and_oidc_and_db(signing, oidc_rp, db)
    }

    /// Most general state builder: inject a custom [`OidcRp`] so the
    /// raw OP Bearer tests can point its JWKS cache at a live test
    /// server and pin a known `issuer`. The signed-session-cookie
    /// issuer/verifier are built from `signing` (the gateway's own key);
    /// `public_url` stays `https://api.zeroship.ai` (the session `iss`).
    fn build_state_with_session_and_oidc_and_db(
        signing: ed25519_dalek::SigningKey,
        oidc_rp: crate::oidc_rp::OidcRp,
        db: Option<crate::db::DbConfig>,
    ) -> std::sync::Arc<crate::GateState> {
        use std::sync::Arc as StdArc;

        let mut tmp = std::env::temp_dir();
        tmp.push(format!(
            "zsgate-auth-u4-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let disk = crate::blob_cache::DiskBlobCache::new(tmp, 1024 * 1024)
            .expect("disk cache");

        // The signed session-cookie issuer/verifier comes from the configured key.
        let session_issuer =
            crate::session_token::Issuer::new(&signing, "https://api.zeroship.ai".into())
                .expect("session issuer");
        let session_verifier = crate::session_token::Verifier::new(
            &signing.verifying_key(),
            "https://api.zeroship.ai".into(),
        );

        let state = StdArc::new(crate::GateState {
            config: crate::GateConfig {
                control_url: String::new(),
                control_key: String::new(),
                worker_urls: vec![],
                poll_interval_secs: 5,
                worker_key: "wk".into(),
                auth_ui_url: oidc_rp.auth_ui_url.clone(),
                origin_scheme: zeroship_core::config::OriginScheme::Http,
                trusted_origins: vec![],
                trust_proxy: false,
                public_url: "https://api.zeroship.ai".into(),
            },
            routes: crate::sync::RouteCache::new(),
            hash_ring: crate::proxy::HashRing::new(vec!["http://0.0.0.0:0".into()], 1),
            rate_limiters: crate::enforce::RateLimitRegistry::new(1, 1),
            per_rule_rate_limits: crate::enforce::PerRuleRateLimitRegistry::new(),
            concurrency: crate::enforce::ConcurrencyRegistry::new(1),
            blob_store: StdArc::new(StubBlobStore),
            blob_cache: crate::blob_cache::BlobCache::new(8 * 1024 * 1024),
            disk_cache: disk,
            idempotency_store: StdArc::new(
                crate::idempotency::InMemoryIdempotencyStore::new(),
            ),
            oidc_rp: StdArc::new(oidc_rp),
            db,
            logout_jti_cache: StdArc::new(
                zeroship_core::logout_token::LogoutJtiCache::default(),
            ),
            revocation_cache: StdArc::new(
                zeroship_authz::wrapper_revocation::RevocationCache::new(),
            ),
            signing_key: Some(StdArc::new(signing)),
            prev_signing_key: None,
            session_issuer: Some(StdArc::new(session_issuer)),
            session_verifier: Some(StdArc::new(session_verifier)),
            anchor_enc_key: [0u8; 32],
            pairwise_salt: [0u8; 32],
            meter: StdArc::new(zeroship_metering::Meter::new()),
        });
        state.routes.update_snapshot(
            zeroship_core::types::GatewaySnapshot {
                routes: zeroship_core::types::RouteMap::new(),
                principal_lifecycle: Vec::new(),
                family_revocations: Vec::new(),
            },
            &state.rate_limiters,
            &state.concurrency,
        );
        state
    }

    // ─── Bearer arm ────────────────────────────────────────
    //
    // The Bearer arm sits before the cookie arm. It
    // recognizes a raw OP access JWT (`iss == oidc_rp.issuer`, verified
    // locally via the gateway JWKS) for non-browser clients — plus the
    // reserved API-key path (any other `iss`). These tests exercise
    // `resolve_bearer_user_header` directly with the REAL `JwksCache` (no
    // stubs), and the `Anon`/`User` policy gate through `resolve_auth`.

    /// The logical OP issuer the raw OP path pins. The test JWKS
    /// server dials loopback, but `OidcRp::with_issuer` decouples the
    /// dial URL from the `iss` the access JWT actually carries.
    const OP_ISS: &str = "https://auth.zeroship.ai/oauth2";

    /// Fixed app UUIDs the raw-OP fixtures derive their `oac_` client ids and
    /// resource audiences from. `OP_APP_UUID` is the single-app default;
    /// `OP_APP_A_UUID`/`OP_APP_B_UUID` are the two genuinely distinct apps the
    /// cross-app divergence test needs.
    const OP_APP_UUID: &str = "0192f1aa-0000-7000-8000-0000000000a1";
    const OP_APP_A_UUID: &str = "0192f1aa-0000-7000-8000-0000000000aa";
    const OP_APP_B_UUID: &str = "0192f1aa-0000-7000-8000-0000000000bb";

    /// Mint the consistent `(client_id, resource_audience)` pair the raw-OP arm
    /// expects for one app, from a fixed app UUID.
    ///
    /// The arm decodes the route's client id back to an app UUID via
    /// `typed_id::app_id_from_oauth_client_id` (strip the `oac_` prefix, then
    /// base62-decode the tail) and requires the token's `aud` to contain
    /// `app:{app_uuid}`. A hand-written label such as `oac_myapp` has a tail
    /// that does not base62-decode to a UUID, so the arm rejects it before any
    /// binding or sector check is reached - fixtures on this path must derive
    /// the client id from a real UUID rather than spell one out. The UUIDs are
    /// fixed rather than random so a failure reproduces exactly.
    fn op_app_binding(app_uuid: &str) -> (String, String) {
        let app_id = Uuid::parse_str(app_uuid).expect("fixture app uuid parses");
        (
            zeroship_core::typed_id::app_oauth_client_id(&app_id),
            format!("app:{app_id}"),
        )
    }

    /// Sign a raw OP-style access JWT (RFC 9068) with `signing`
    /// (EdDSA). `client_id`/`aud` are stamped so the Bearer arm's per-app
    /// bindings can be exercised: `client_id` is the authorized party and
    /// `aud` must carry the route's resource audience (`app:{app_id}`). Pass
    /// `client_id: None` to drop the claim entirely, which the arm rejects.
    /// `exp_delta` controls expiry relative to now (negative ⇒ expired).
    #[allow(clippy::too_many_arguments)]
    fn sign_op_access_jwt(
        signing: &ed25519_dalek::SigningKey,
        sub: &str,
        client_id: Option<&str>,
        aud: serde_json::Value,
        email: &str,
        name: &str,
        exp_delta: i64,
    ) -> String {
        use ed25519_dalek::pkcs8::EncodePrivateKey;
        use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};

        let now = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        )
        .unwrap();
        let mut body = serde_json::json!({
            "sub": sub,
            "iss": OP_ISS,
            "aud": aud,
            "exp": now + exp_delta,
            "iat": now - 5,
            "email": email,
            "email_verified": true,
            "name": name,
            "scope": "openid email",
            // RFC 9068 sec. 2.2 requires `jti` on a JWT access token, and the
            // verifier's claim struct makes it mandatory, so a token without
            // one is rejected before any binding check runs. The OP stamps a
            // fresh one per token (`crates/auth/src/oidc/issuer.rs`); mint one
            // here too, or this fixture signs a token no issuer would produce.
            "jti": uuid::Uuid::new_v4().to_string(),
        });
        if let Some(cid) = client_id {
            body["client_id"] = serde_json::Value::String(cid.to_string());
        }

        let mut header = Header::new(Algorithm::EdDSA);
        header.typ = Some("at+jwt".into());
        header.kid = Some(crate::signing::jwk_thumbprint(signing));
        let der = signing.to_pkcs8_der().expect("pkcs8");
        let key = EncodingKey::from_ed_der(der.as_bytes());
        encode(&header, &body, &key).expect("sign access jwt")
    }

    /// Build the JWKS document (one EdDSA/OKP key) that verifies tokens
    /// signed by `signing`. `kid` is the RFC 7638 thumbprint so it
    /// matches the JWT header the signer stamps.
    fn op_jwks_doc(signing: &ed25519_dalek::SigningKey) -> serde_json::Value {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine as _;
        let pk = signing.verifying_key();
        let x = URL_SAFE_NO_PAD.encode(pk.to_bytes());
        serde_json::json!({
            "keys": [{
                "kid": crate::signing::jwk_thumbprint(signing),
                "kty": "OKP",
                "alg": "EdDSA",
                "crv": "Ed25519",
                "x": x,
            }]
        })
    }

    /// Spin up a loopback JWKS server serving `doc` and return its base
    /// URL. The gateway `OidcRp` dials `{base}/oauth2/.well-known/jwks.json`.
    async fn start_jwks_server(doc: serde_json::Value) -> ntex::web::test::TestServer {
        let doc = std::sync::Arc::new(doc);
        let doc_for_server = doc.clone();
        ntex::web::test::server(move || {
            let doc = doc_for_server.clone();
            async move {
                ntex::web::App::new().state(doc).service(
                    ntex::web::resource("/oauth2/.well-known/jwks.json").route(
                        ntex::web::get().to(
                            |doc: ntex::web::types::State<
                                std::sync::Arc<serde_json::Value>,
                            >| async move {
                                ntex::web::HttpResponse::Ok().json(doc.get_ref().as_ref())
                            },
                        ),
                    ),
                )
            }
        })
        .await
    }

    /// Build a state whose `OidcRp` JWKS dials `jwks_base` and whose
    /// `issuer` is the canonical OP `iss`. `gateway_signing` is the
    /// gateway's own session-cookie signing key (distinct from OP's JWKS key).
    fn build_state_for_op(
        gateway_signing: ed25519_dalek::SigningKey,
        jwks_base: &str,
    ) -> std::sync::Arc<crate::GateState> {
        let oidc_rp = crate::oidc_rp::OidcRp::new(
            jwks_base,
            test_broker_secret(),
            b"test-stash-key-32-bytes-long----".to_vec(),
        )
        .with_issuer(OP_ISS);
        build_state_with_session_and_oidc_and_db(gateway_signing, oidc_rp, None)
    }

    fn bearer_req(token: &str, host: &str) -> ntex::web::HttpRequest {
        ntex::web::test::TestRequest::default()
            .uri("/api/me")
            .header(http::header::HOST, host)
            .header(http::header::AUTHORIZATION, format!("Bearer {token}"))
            .to_http_request()
    }

    /// `EffectivePolicy` has no `Default` (its `action` field has none),
    /// so build the minimal policy explicitly. Only `auth` is load-bearing
    /// for the Bearer-arm policy gate.
    fn policy_with_auth(auth: zeroship_bundle::AuthLevel) -> crate::compiled::EffectivePolicy {
        crate::compiled::EffectivePolicy {
            auth,
            rate_limit: None,
            cors: None,
            cache: None,
            csrf_origins: None,
            idempotent: false,
            idempotency_ttl_hours: None,
            timeout_ms: None,
            max_input_bytes: None,
            middleware: vec![],
            publicly_accessible: true,
            required_scopes: vec![],
            kind: None,
            action: crate::compiled::ResolvedAction::WorkerRpc,
            input_schema: None,
            output_schema: None,
        }
    }

    fn anon_policy() -> crate::compiled::EffectivePolicy {
        policy_with_auth(zeroship_bundle::AuthLevel::Anon)
    }

    fn user_policy() -> crate::compiled::EffectivePolicy {
        policy_with_auth(zeroship_bundle::AuthLevel::User)
    }

    /// A `User` policy that additionally demands `required` scopes, exercising
    /// the route-level scope gate.
    fn user_policy_requiring(
        required: &[&str],
    ) -> crate::compiled::EffectivePolicy {
        let mut p = user_policy();
        p.required_scopes = required.iter().map(|s| s.to_string()).collect();
        p
    }

    // ─── BFF P3 anti-CSRF gate on cookie-authenticated dispatch (DB-free) ──

    /// Build a request with an explicit method and optional origin metadata.
    fn csrf_req(
        method: ntex::http::Method,
        host: &str,
        origin: Option<&str>,
        sfs: Option<&str>,
    ) -> HttpRequest {
        let mut b = ntex::web::test::TestRequest::default()
            .method(method)
            .uri("/api/do")
            .header(http::header::HOST, host);
        if let Some(o) = origin {
            b = b.header(http::header::ORIGIN, o);
        }
        if let Some(s) = sfs {
            b = b.header("sec-fetch-site", s);
        }
        b.to_http_request()
    }

    fn csrf_config(
        origin_scheme: zeroship_core::config::OriginScheme,
        trusted_origins: &[&str],
    ) -> crate::GateConfig {
        crate::GateConfig {
            control_url: String::new(),
            control_key: String::new(),
            worker_urls: vec![],
            poll_interval_secs: 5,
            worker_key: String::new(),
            auth_ui_url: String::new(),
            origin_scheme,
            trusted_origins: trusted_origins
                .iter()
                .map(|origin| origin.parse().expect("validated trusted origin"))
                .collect(),
            trust_proxy: false,
            public_url: String::new(),
        }
    }

    #[test]
    fn csrf_gate_exempts_safe_methods() {
        // GET/HEAD are non-state-changing: never rejected, even with a foreign
        // Origin (the cookie still authenticates reads).
        let host = "myapp.zeroship.ai";
        let config = csrf_config(zeroship_core::config::OriginScheme::Http, &[]);
        assert!(!cookie_csrf_rejected(
            &csrf_req(ntex::http::Method::GET, host, Some("https://evil.example"), None),
            &config,
        ));
        assert!(!cookie_csrf_rejected(
            &csrf_req(ntex::http::Method::HEAD, host, None, None),
            &config,
        ));
    }

    #[test]
    fn csrf_gate_accepts_same_origin_state_change() {
        // POST with the app's own HTTP origin and same-origin Sec-Fetch.
        let host = "myapp.zeroship.ai";
        let config = csrf_config(zeroship_core::config::OriginScheme::Http, &[]);
        assert!(!cookie_csrf_rejected(
            &csrf_req(
                ntex::http::Method::POST,
                host,
                Some("http://myapp.zeroship.ai"),
                Some("same-origin"),
            ),
            &config,
        ));
        // Sec-Fetch-Site absent is tolerated (advisory) when Origin matches.
        assert!(!cookie_csrf_rejected(
            &csrf_req(ntex::http::Method::POST, host, Some("http://myapp.zeroship.ai"), None),
            &config,
        ));
    }

    #[test]
    fn csrf_gate_accepts_validated_configured_origins_with_realistic_metadata() {
        let host = "myapp.zeroship.ai";
        let config = csrf_config(
            zeroship_core::config::OriginScheme::Https,
            &[
                "https://console.zeroship.ai",
                "https://console.zeroship.example",
            ],
        );

        for (origin, fetch_site) in [
            ("https://console.zeroship.example", "same-origin"),
            ("https://console.zeroship.ai", "same-site"),
            ("https://console.zeroship.example", "cross-site"),
        ] {
            assert!(!cookie_csrf_rejected(
                &csrf_req(
                    ntex::http::Method::POST,
                    host,
                    Some(origin),
                    Some(fetch_site),
                ),
                &config,
            ));
        }

        assert!(!cookie_csrf_rejected(
            &csrf_req(
                ntex::http::Method::POST,
                host,
                Some("https://console.zeroship.example"),
                None,
            ),
            &config,
        ));
    }

    #[test]
    fn csrf_gate_rejects_inconsistent_metadata_for_configured_origin() {
        let host = "myapp.zeroship.ai";
        let config = csrf_config(
            zeroship_core::config::OriginScheme::Https,
            &["https://console.zeroship.example"],
        );
        for fetch_site in ["none", "unknown"] {
            assert!(cookie_csrf_rejected(
                &csrf_req(
                    ntex::http::Method::POST,
                    host,
                    Some("https://console.zeroship.example"),
                    Some(fetch_site),
                ),
                &config,
            ));
        }
    }

    #[test]
    fn csrf_gate_rejects_foreign_missing_and_cross_site_state_change() {
        let host = "myapp.zeroship.ai";
        let config = csrf_config(zeroship_core::config::OriginScheme::Http, &[]);
        // Foreign Origin on a state-changing POST rejects the cookie credential.
        assert!(cookie_csrf_rejected(
            &csrf_req(ntex::http::Method::POST, host, Some("https://evil.example"), None),
            &config,
        ));
        // Missing Origin on a state-changing POST has no origin to match.
        assert!(cookie_csrf_rejected(
            &csrf_req(ntex::http::Method::POST, host, None, None),
            &config,
        ));
        // Origin: null is never trusted.
        assert!(cookie_csrf_rejected(
            &csrf_req(ntex::http::Method::POST, host, Some("null"), None),
            &config,
        ));
        // Cross-site fetch metadata rejects even a matching origin.
        assert!(cookie_csrf_rejected(
            &csrf_req(
                ntex::http::Method::POST,
                host,
                Some("http://myapp.zeroship.ai"),
                Some("cross-site"),
            ),
            &config,
        ));
        // PUT/PATCH/DELETE are state-changing too.
        for m in [ntex::http::Method::PUT, ntex::http::Method::PATCH, ntex::http::Method::DELETE] {
            assert!(
                cookie_csrf_rejected(
                    &csrf_req(m.clone(), host, Some("https://evil.example"), None),
                    &config,
                ),
                "{m} with foreign Origin must be rejected"
            );
        }
    }

    #[test]
    fn csrf_gate_uses_public_origin_scheme() {
        let host = "myapp.zeroship.ai";
        let https_config = csrf_config(zeroship_core::config::OriginScheme::Https, &[]);
        assert!(!cookie_csrf_rejected(
            &csrf_req(ntex::http::Method::POST, host, Some("https://myapp.zeroship.ai"), Some("same-origin")),
            &https_config,
        ));
        assert!(cookie_csrf_rejected(
            &csrf_req(ntex::http::Method::POST, host, Some("http://myapp.zeroship.ai"), None),
            &https_config,
        ));

        let http_config = csrf_config(zeroship_core::config::OriginScheme::Http, &[]);
        assert!(!cookie_csrf_rejected(
            &csrf_req(ntex::http::Method::POST, host, Some("http://myapp.zeroship.ai"), None),
            &http_config,
        ));
        assert!(cookie_csrf_rejected(
            &csrf_req(ntex::http::Method::POST, host, Some("https://myapp.zeroship.ai"), None),
            &http_config,
        ));
    }

    #[compio::test]
    async fn bearer_non_jwt_token_is_not_user_session() {
        // An opaque, non-JWT Bearer (e.g. a future `zsk_…` API key) is the
        // reserved path → NotUserSession (401 on every route, including
        // Anon).
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let state = build_state_with_session(gateway_signing);
        let req = bearer_req("zsk_opaque_api_key_value", "myapp.zeroship.ai");
        let request_id = Uuid::new_v4();
        let outcome =
            resolve_bearer_user_header(&req, &state, &request_id, Some("oac_myapp"), None).await;
        assert!(
            matches!(outcome, BearerOutcome::NotUserSession),
            "opaque token must be NotUserSession, got {outcome:?}"
        );
    }

    #[compio::test]
    async fn bearer_unrecognized_iss_jwt_is_not_user_session() {
        // A well-formed JWT whose `iss` is neither the gateway nor OP
        // is still the reserved path → NotUserSession.
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let state = build_state_with_session(gateway_signing);
        // Craft an unsigned-but-structurally-valid JWT with a foreign iss.
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine as _;
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"EdDSA","typ":"at+jwt"}"#);
        let payload = URL_SAFE_NO_PAD.encode(br#"{"iss":"https://evil.example.com","sub":"x"}"#);
        let token = format!("{header}.{payload}.AAAA");
        let req = bearer_req(&token, "myapp.zeroship.ai");
        let request_id = Uuid::new_v4();
        let outcome =
            resolve_bearer_user_header(&req, &state, &request_id, Some("oac_myapp"), None).await;
        assert!(
            matches!(outcome, BearerOutcome::NotUserSession),
            "foreign-iss JWT must be NotUserSession, got {outcome:?}"
        );
    }

    #[ntex::test]
    async fn resolve_auth_invalid_raw_op_on_anon_route_serves_anonymously() {
        // A present-but-INVALID raw OP user-session Bearer on an `Anon`
        // route must NOT 401 — it falls through to anonymous (a client may
        // auto-attach a Bearer to every request; a stale/invalid one must not
        // break public pages). The same invalid Bearer on a `User`
        // route is Unauthenticated (401).
        let jwks_signing = ed25519_dalek::SigningKey::from_bytes(&[55u8; 32]);
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        // JWKS server serves a DIFFERENT key than the token is signed with, so
        // the raw OP Bearer fails signature verification → Invalid.
        let srv = start_jwks_server(op_jwks_doc(&jwks_signing)).await;
        let base = srv.url("").trim_end_matches('/').to_string();
        let oidc_rp = crate::oidc_rp::OidcRp::new(
            &base,
            test_broker_secret(),
            b"test-stash-key-32-bytes-long----".to_vec(),
        )
        .with_issuer(OP_ISS);
        let state = build_state_with_session_and_oidc_and_db(gateway_signing, oidc_rp, None);
        let aud = "myapp.zeroship.ai";

        let bad_key = ed25519_dalek::SigningKey::from_bytes(&[99u8; 32]);
        let token = sign_op_access_jwt(
            &bad_key,
            "0192f1aa-bbbb-7ccc-8ddd-eeeeffff00aa",
            Some("oac_myapp"),
            serde_json::json!(["oac_myapp"]),
            "user@example.com",
            "OP User",
            3600,
        );

        let req = bearer_req(&token, aud);
        let request_id = Uuid::new_v4();

        // Anon route: invalid Bearer → Allowed with NO user header.
        let anon = resolve_auth(
            &req,
            &state,
            &anon_policy(),
            &request_id,
            Some("oac_myapp"),
            Some("https://myapp.zeroship.ai"),
        )
        .await;
        assert!(
            matches!(anon, AuthOutcome::Allowed { user_header: None }),
            "invalid Bearer on Anon route must serve anonymously, got {anon:?}"
        );

        // User route: same invalid Bearer → Unauthenticated (401).
        let gated = resolve_auth(
            &req,
            &state,
            &user_policy(),
            &request_id,
            Some("oac_myapp"),
            Some("https://myapp.zeroship.ai"),
        )
        .await;
        assert!(
            matches!(gated, AuthOutcome::Unauthenticated),
            "invalid Bearer on User route must be Unauthenticated, got {gated:?}"
        );
        drop(srv);
    }

    // ─── Route-level required-scope enforcement ──
    //
    // After a principal authenticates (here via a real signed session
    // cookie), the matched route's `required_scopes` gate the GRANT:
    // a superset passes, a miss is `403 insufficient_scope` (NOT 401 —
    // identity is fine), and an empty `required_scopes` is unchanged.
    // These drive the REAL `resolve_auth` with a REAL signed cookie +
    // verifier (no shim), exercising the same path dispatch uses. (The scope
    // gate is arm-agnostic — it reads the just-resolved ZeroShip-User header's
    // scopes regardless of which arm authenticated.)

    /// Build a GET request carrying a signed session cookie for the cookie arm.
    fn scope_cookie_req(state: &crate::GateState, client_id: &str, scopes: &[&str], aud: &str) -> ntex::web::HttpRequest {
        let scopes: Vec<String> = scopes.iter().map(|s| (*s).to_string()).collect();
        let pws = format!("pws_{}", &Uuid::new_v4().simple().to_string()[..20]);
        let token =
            issue_signed_session_cookie(state, client_id, &pws, "relay-alias@zeroship.ai", &scopes);
        let cookie_name = oidc_rp::app_session_cookie_name();
        ntex::web::test::TestRequest::default()
            .uri("/api/me")
            .header(http::header::HOST, aud)
            .header("cookie", format!("{cookie_name}={token}"))
            .to_http_request()
    }

    #[ntex::test]
    async fn resolve_auth_authenticated_without_required_scope_403s() {
        // Authenticated principal whose granted scopes do NOT include the
        // route's `required_scopes` → InsufficientScope (the dispatch 403),
        // NOT Allowed and NOT Unauthenticated.
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let state = build_state_with_session(gateway_signing);
        let aud = "myapp.zeroship.ai";
        // Granted: openid email — does NOT include read:billing.
        let req = scope_cookie_req(&state, "oac_myapp", &["openid", "email"], aud);
        let request_id = Uuid::new_v4();

        let outcome = resolve_auth(
            &req,
            &state,
            &user_policy_requiring(&["read:billing"]),
            &request_id,
            Some("oac_myapp"),
            None,
        )
        .await;
        match outcome {
            AuthOutcome::InsufficientScope { required } => {
                assert_eq!(required, vec!["read:billing".to_string()]);
            }
            other => panic!("expected InsufficientScope, got {other:?}"),
        }
    }

    #[ntex::test]
    async fn resolve_auth_authenticated_with_required_scope_allowed() {
        // Same principal + route, but the cookie WAS granted read:billing
        // → Allowed (the scope gate is a superset check). The emitted
        // ZeroShip-User header carries the granted scopes through.
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let state = build_state_with_session(gateway_signing);
        let aud = "myapp.zeroship.ai";
        let req = scope_cookie_req(&state, "oac_myapp", &["openid", "email", "read:billing"], aud);
        let request_id = Uuid::new_v4();

        let outcome = resolve_auth(
            &req,
            &state,
            &user_policy_requiring(&["read:billing"]),
            &request_id,
            Some("oac_myapp"),
            None,
        )
        .await;
        let AuthOutcome::Allowed {
            user_header: Some(header),
        } = outcome
        else {
            panic!("expected Allowed with a user header, got {outcome:?}");
        };
        // Faithful: the granted scope rode through the header the worker reads.
        let json = zeroship_core::auth::verify_zeroship_user_header(
            state.config.worker_key.as_bytes(),
            &header,
        )
        .expect("ZeroShip-User MAC verifies");
        let user: serde_json::Value = serde_json::from_str(&json).expect("user json");
        assert!(
            user["scopes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|s| s == "read:billing"),
            "granted scope must survive into ZeroShip-User"
        );
    }

    #[ntex::test]
    async fn resolve_auth_empty_required_scopes_unchanged() {
        // A route with NO required_scopes is unchanged: the authenticated
        // principal is Allowed regardless of which scopes it carries.
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let state = build_state_with_session(gateway_signing);
        let aud = "myapp.zeroship.ai";
        let req = scope_cookie_req(&state, "oac_myapp", &["openid"], aud);
        let request_id = Uuid::new_v4();

        let outcome = resolve_auth(
            &req,
            &state,
            &user_policy(), // required_scopes == []
            &request_id,
            Some("oac_myapp"),
            None,
        )
        .await;
        assert!(
            matches!(outcome, AuthOutcome::Allowed { user_header: Some(_) }),
            "empty required_scopes ⇒ no scope gate, got {outcome:?}"
        );
    }

    #[ntex::test]
    async fn resolve_auth_required_scope_superset_passes() {
        // The gate is a SUPERSET check: a principal granted MORE than the
        // route demands still passes (it has everything required).
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let state = build_state_with_session(gateway_signing);
        let aud = "myapp.zeroship.ai";
        let req = scope_cookie_req(
            &state,
            "oac_myapp",
            &["openid", "email", "read:billing", "write:projects"],
            aud,
        );
        let request_id = Uuid::new_v4();

        let outcome = resolve_auth(
            &req,
            &state,
            &user_policy_requiring(&["read:billing"]),
            &request_id,
            Some("oac_myapp"),
            None,
        )
        .await;
        assert!(
            matches!(outcome, AuthOutcome::Allowed { user_header: Some(_) }),
            "granted superset must pass the scope gate, got {outcome:?}"
        );
    }

    #[compio::test]
    async fn resolve_auth_unauthenticated_scope_route_still_401s_not_403() {
        // An UNAUTHENTICATED request to a required-scope `User` route is
        // gated by the User policy FIRST (401), never by the scope gate
        // (403) — required_scopes only gates an authenticated principal.
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let state = build_state_with_session(gateway_signing);
        // No Authorization header, no cookie → unauthenticated.
        let req = ntex::web::test::TestRequest::default()
            .uri("/api/billing")
            .header(http::header::HOST, "myapp.zeroship.ai")
            .to_http_request();
        let request_id = Uuid::new_v4();

        let outcome = resolve_auth(
            &req,
            &state,
            &user_policy_requiring(&["read:billing"]),
            &request_id,
            Some("oac_myapp"),
            None,
        )
        .await;
        assert!(
            matches!(outcome, AuthOutcome::Unauthenticated),
            "unauthenticated request must 401 (policy gate), not 403 (scope gate), got {outcome:?}"
        );
    }

    #[ntex::test]
    async fn resolve_auth_underscoped_authenticated_on_anon_route_is_allowed() {
        // Regression: the scope gate must NOT fire
        // on an `Anon` (public) route. A logged-in browser whose session lacks
        // a scope that a broad `*` parent put into `required_scopes` would
        // otherwise get 403 on the app's own HTML/JS/CSS while a logged-OUT
        // visitor loads it fine — the "logged-in is worse than anonymous on
        // public routes" footgun. The authenticated principal must be Allowed
        // (with its user_header) on the public route regardless of scope.
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let state = build_state_with_session(gateway_signing);
        let aud = "myapp.zeroship.ai";
        // Granted only `openid` — does NOT include the route's required scope.
        let req = scope_cookie_req(&state, "oac_myapp", &["openid"], aud);
        let request_id = Uuid::new_v4();

        // `Anon` route that nonetheless carries `required_scopes` (e.g.
        // inherited from a scoped `*`). The Anon policy must win: Allowed.
        let mut anon_with_scope = anon_policy();
        anon_with_scope.required_scopes = vec!["read:billing".to_string()];

        let outcome = resolve_auth(
            &req,
            &state,
            &anon_with_scope,
            &request_id,
            Some("oac_myapp"),
            None,
        )
        .await;
        assert!(
            matches!(outcome, AuthOutcome::Allowed { user_header: Some(_) }),
            "underscoped authenticated principal on an Anon route must be Allowed, \
             never scope-403'd, got {outcome:?}"
        );
    }

    #[test]
    fn scopes_satisfied_superset_semantics() {
        // Unit-level guard on the gate predicate: every required scope must
        // be present; empty required is trivially satisfied; a missing one
        // fails.
        let granted = vec!["openid".to_string(), "read:billing".to_string()];
        assert!(scopes_satisfied(&granted, &[]));
        assert!(scopes_satisfied(&granted, &["read:billing".to_string()]));
        assert!(scopes_satisfied(
            &granted,
            &["openid".to_string(), "read:billing".to_string()]
        ));
        assert!(!scopes_satisfied(&granted, &["write:billing".to_string()]));
        // Fails closed when the principal carries no scopes at all.
        assert!(!scopes_satisfied(&[], &["read:billing".to_string()]));
    }

    #[compio::test]
    async fn resolve_auth_non_user_session_bearer_401s_even_on_anon() {
        // The reserved API-key path asserts a DIFFERENT scheme, not an
        // expired user session — so it 401s even on an `Anon` route
        // (unlike an invalid raw OP Bearer, which falls through to anonymous).
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let state = build_state_with_session(gateway_signing);
        let req = bearer_req("zsk_opaque_api_key", "myapp.zeroship.ai");
        let request_id = Uuid::new_v4();
        let outcome = resolve_auth(
            &req,
            &state,
            &anon_policy(),
            &request_id,
            Some("oac_myapp"),
            None,
        )
        .await;
        assert!(
            matches!(outcome, AuthOutcome::Unauthenticated),
            "reserved-scheme Bearer must 401 even on Anon, got {outcome:?}"
        );
    }

    #[ntex::test]
    async fn bearer_valid_raw_op_jwt_emits_zeroship_user() {
        // Happy path (raw OP): a real EdDSA-signed access JWT,
        // JWKS-verified against a live JWKS server, with a matching
        // client_id claim produces Allowed + ZeroShip-User whose id is the
        // issuer-projected per-app pws_; no global UUID reaches the worker.
        let jwks_signing = ed25519_dalek::SigningKey::from_bytes(&[55u8; 32]); // OP's key
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]); // gateway wrapper key
        let srv = start_jwks_server(op_jwks_doc(&jwks_signing)).await;
        let base = srv.url("").trim_end_matches('/').to_string();
        let state = build_state_for_op(gateway_signing, &base);

        let global_sub = "0192f1aa-bbbb-7ccc-8ddd-eeeeffff0001";
        let sector = "https://myapp.zeroship.ai";
        let host = "myapp.zeroship.ai";
        let (client_id, resource_aud) = op_app_binding(OP_APP_UUID);
        let expected_pws =
            zeroship_core::auth::derive_pairwise(&state.pairwise_salt, global_sub, sector);
        let token = sign_op_access_jwt(
            &jwks_signing,
            &expected_pws,
            Some(&client_id),
            // Resource-server audiences, NOT the client. The unrelated first
            // entry keeps this a contains-check, not an equality-check.
            serde_json::json!(["http://api.zeroship.localhost", resource_aud]),
            "user@example.com",
            "OP User",
            3600,
        );

        let req = bearer_req(&token, host);
        let request_id = Uuid::new_v4();
        let outcome = resolve_bearer_user_header(
            &req,
            &state,
            &request_id,
            Some(&client_id),
            Some(sector),
        )
        .await;
        let BearerOutcome::Allowed(header) = outcome else {
            panic!("expected Allowed, got {outcome:?}");
        };
        let json = zeroship_core::auth::verify_zeroship_user_header(
            state.config.worker_key.as_bytes(),
            &header,
        )
        .expect("MAC verifies");
        let user: serde_json::Value = serde_json::from_str(&json).expect("user json");
        assert_eq!(user["id"], expected_pws);
        assert!(
            expected_pws.starts_with("pws_"),
            "id must be a pws_, got {expected_pws}"
        );
        // The global UUID must NOT appear anywhere in the worker header JSON.
        assert!(
            !json.contains(global_sub),
            "global UUID leaked into ZeroShip-User: {json}"
        );
        // Email-claim swap: the app NEVER sees the real email.
        // With no DB/alias source here (`build_state_for_op` db=None) the
        // swap fails closed → empty email. The real `user@example.com` (what
        // OP stamped) must be ABSENT from the projected header.
        assert_eq!(user["email"], "", "no alias ⇒ empty email (fail closed)");
        assert!(
            !json.contains("user@example.com"),
            "real email leaked into ZeroShip-User: {json}"
        );

        drop(srv);
    }

    #[ntex::test]
    async fn bearer_raw_op_absent_client_id_rejected_with_no_aud_fallback() {
        // RFC 9068 sec. 3 mandates `client_id`, and the arm binds the authorized
        // party on that claim ALONE: there is deliberately no fallback to
        // binding on `aud`, because `aud` names the resource server and an ID
        // token carries `aud == client_id` (which would let an ID token stand
        // in for an access token). Here the JWT has NO client_id claim and
        // lists the route's client id in `aud` - the exact shape the removed
        // fallback would have accepted. It must be rejected.
        let jwks_signing = ed25519_dalek::SigningKey::from_bytes(&[55u8; 32]);
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let srv = start_jwks_server(op_jwks_doc(&jwks_signing)).await;
        let base = srv.url("").trim_end_matches('/').to_string();
        let state = build_state_for_op(gateway_signing, &base);

        let host = "myapp.zeroship.ai";
        let (client_id, resource_aud) = op_app_binding(OP_APP_UUID);
        let token = sign_op_access_jwt(
            &jwks_signing,
            "0192f1aa-bbbb-7ccc-8ddd-eeeeffff0002",
            None, // no client_id claim
            // Both the client id (what the old fallback keyed on) and the real
            // resource audience are present, so the ONLY thing missing is the
            // client_id claim.
            serde_json::json!([resource_aud, client_id]),
            "user@example.com",
            "OP User",
            3600,
        );

        let req = bearer_req(&token, host);
        let request_id = Uuid::new_v4();
        // Provisioned route (sector present) so a rejection here cannot be the
        // fail-closed no-sector path: the only reason to reject is the missing
        // client_id claim.
        let outcome = resolve_bearer_user_header(
            &req,
            &state,
            &request_id,
            Some(&client_id),
            Some("https://myapp.zeroship.ai"),
        )
        .await;
        assert!(
            matches!(outcome, BearerOutcome::Invalid),
            "a token with no client_id claim must be Invalid (no aud fallback), got {outcome:?}"
        );

        drop(srv);
    }

    #[ntex::test]
    async fn bearer_raw_op_client_id_mismatch_rejected() {
        // A raw OP JWT whose client_id claim is app A must be rejected
        // at app B's host (cross-app replay defense on the raw path too).
        let jwks_signing = ed25519_dalek::SigningKey::from_bytes(&[55u8; 32]);
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let srv = start_jwks_server(op_jwks_doc(&jwks_signing)).await;
        let base = srv.url("").trim_end_matches('/').to_string();
        let state = build_state_for_op(gateway_signing, &base);

        let aud = "myapp.zeroship.ai";
        let token = sign_op_access_jwt(
            &jwks_signing,
            "usr_global_uuid",
            Some("oac_app_a"),
            serde_json::json!(["http://api.zeroship.localhost"]),
            "user@example.com",
            "OP User",
            3600,
        );

        let req = bearer_req(&token, aud);
        let request_id = Uuid::new_v4();
        let outcome =
            resolve_bearer_user_header(&req, &state, &request_id, Some("oac_app_b"), None).await;
        assert!(
            matches!(outcome, BearerOutcome::Invalid),
            "raw OP client_id mismatch must be Invalid, got {outcome:?}"
        );

        drop(srv);
    }

    /// 48-bit pseudo-random suffix for a unique-per-run global sub: the
    /// unique sub avoids cross-run assertion taint (a stale row from an
    /// earlier run keys on a different sub). Stale rows are not cleaned up on
    /// an unwound assert! panic — they accumulate until the 24h
    /// `sweep_expired_families` reaps them. No `rand` dep needed — the nanos
    /// clock is plenty.
    fn rand_suffix() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64
            & 0xffff_ffff_ffff
    }

    #[ntex::test]
    async fn bearer_raw_op_expired_rejected() {
        // An expired raw OP JWT (beyond the 60s leeway) fails the JWKS
        // verify → Invalid (401 on User, anonymous on Anon).
        let jwks_signing = ed25519_dalek::SigningKey::from_bytes(&[55u8; 32]);
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let srv = start_jwks_server(op_jwks_doc(&jwks_signing)).await;
        let base = srv.url("").trim_end_matches('/').to_string();
        let state = build_state_for_op(gateway_signing, &base);

        let aud = "myapp.zeroship.ai";
        let token = sign_op_access_jwt(
            &jwks_signing,
            "usr_global_uuid",
            Some("oac_myapp"),
            serde_json::json!(["http://api.zeroship.localhost"]),
            "user@example.com",
            "OP User",
            -100, // expired
        );

        let req = bearer_req(&token, aud);
        let request_id = Uuid::new_v4();
        let outcome =
            resolve_bearer_user_header(&req, &state, &request_id, Some("oac_myapp"), None).await;
        assert!(
            matches!(outcome, BearerOutcome::Invalid),
            "expired raw OP JWT must be Invalid, got {outcome:?}"
        );

        drop(srv);
    }

    #[ntex::test]
    async fn bearer_raw_op_bad_signature_rejected() {
        // A raw OP JWT signed by a key NOT in the JWKS must fail
        // signature verification → Invalid.
        let jwks_signing = ed25519_dalek::SigningKey::from_bytes(&[55u8; 32]); // published
        let forged_signing = ed25519_dalek::SigningKey::from_bytes(&[66u8; 32]); // NOT published
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let srv = start_jwks_server(op_jwks_doc(&jwks_signing)).await;
        let base = srv.url("").trim_end_matches('/').to_string();
        let state = build_state_for_op(gateway_signing, &base);

        let aud = "myapp.zeroship.ai";
        // Signed by forged key → its kid won't be in the JWKS (the kid is
        // the thumbprint of the forged public half).
        let token = sign_op_access_jwt(
            &forged_signing,
            "usr_global_uuid",
            Some("oac_myapp"),
            serde_json::json!(["http://api.zeroship.localhost"]),
            "user@example.com",
            "OP User",
            3600,
        );

        let req = bearer_req(&token, aud);
        let request_id = Uuid::new_v4();
        let outcome =
            resolve_bearer_user_header(&req, &state, &request_id, Some("oac_myapp"), None).await;
        assert!(
            matches!(outcome, BearerOutcome::Invalid),
            "bad-signature raw OP JWT must be Invalid, got {outcome:?}"
        );

        drop(srv);
    }

    #[compio::test]
    async fn resolve_auth_no_bearer_falls_through_to_cookie_arm() {
        // No Authorization header at all → the Bearer arm yields
        // NotBearer and resolve_auth falls through to the cookie arm
        // (which, with no DB and no cookie, resolves to None). On an Anon
        // route that is Allowed{None}; on a User route, Unauthenticated.
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let state = build_state_with_session(gateway_signing);
        let req = ntex::web::test::TestRequest::default()
            .uri("/api/me")
            .header(http::header::HOST, "myapp.zeroship.ai")
            .to_http_request();
        let request_id = Uuid::new_v4();
        let anon = resolve_auth(
            &req,
            &state,
            &anon_policy(),
            &request_id,
            Some("oac_myapp"),
            None,
        )
        .await;
        assert!(matches!(anon, AuthOutcome::Allowed { user_header: None }));
        let gated = resolve_auth(
            &req,
            &state,
            &user_policy(),
            &request_id,
            Some("oac_myapp"),
            None,
        )
        .await;
        assert!(matches!(gated, AuthOutcome::Unauthenticated));
    }

    // ─── Per-app family-marker revocation (§8.5, major regressions) ───────
    //
    // PG-gated: these need a live `auth` schema with `zeroship.token_revocations`
    // (skip when there is no test database; set PG_TEST_URL).
    // They cover the MAJOR finding that revocation is PER-APP — revoking a
    // user on app A does NOT revoke the same sub on app B — keyed on the
    // TEXT `(client_id, pws_)` family marker (the old UUID-only denylist
    // could never match the `pws_…` subject).

    // Postgres is not optional for this workspace's tests (see
    // crates/test-support/src/lib.rs); `test_database_url()` panics with the
    // provisioning command rather than let these revocation tests report a
    // pass for a check they never ran.
    async fn connect_auth_db() -> crate::db::DbConfig {
        let dsn = zeroship_core::config::test_database_url();
        crate::db::DbConfig::new(dsn, 4)
    }

    #[ntex::test]
    async fn bearer_raw_op_revocation_is_per_app_not_global() {
        let db = connect_auth_db().await;
        let jwks_signing = ed25519_dalek::SigningKey::from_bytes(&[55u8; 32]);
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let srv = start_jwks_server(op_jwks_doc(&jwks_signing)).await;
        let base = srv.url("").trim_end_matches('/').to_string();

        // build_state_for_op with a DB so the revocation check runs.
        let oidc_rp = crate::oidc_rp::OidcRp::new(
            &base,
            test_broker_secret(),
            b"test-stash-key-32-bytes-long----".to_vec(),
        )
        .with_issuer(OP_ISS);
        let state =
            build_state_with_session_and_oidc_and_db(gateway_signing, oidc_rp, Some(db.clone()));

        // A fresh global sub per run: this test WRITES a revocation marker, and
        // a fixed sub would leave a row that makes the next run's "not revoked"
        // arm depend on the previous run.
        let sub = Uuid::new_v4().to_string();

        // Two genuinely distinct apps, built with the same `op_app_binding`
        // helper the sibling per-app tests use.
        //
        // This test used to hand the arm the placeholder strings "oac_app_a" /
        // "oac_app_b" and a resource audience of "http://api.zeroship.localhost".
        // Neither is what the arm accepts: a per-app client_id is
        // `oac_<base62(app uuid)>`, which the arm PARSES back to an app id
        // before it consults revocation at all, and `aud` must carry that app's
        // `app:{app_id}` resource audience. So both tokens were refused on the
        // shape check ("route client_id is not a per-app OAuth client"), which
        // failed the app B assertion outright AND made the app A assertion pass
        // for the wrong reason - the token was rejected as malformed, not as
        // revoked. The property this test claims to prove was never exercised.
        let (client_a, resource_aud_a) = op_app_binding(OP_APP_A_UUID);
        let (client_b, resource_aud_b) = op_app_binding(OP_APP_B_UUID);
        assert_ne!(client_a, client_b, "fixture must model two DIFFERENT apps");
        assert_ne!(resource_aud_a, resource_aud_b);
        let sector_a = "https://app-a.zeroship.ai";
        let sector_b = "https://app-b.zeroship.ai";
        let pws_a = zeroship_core::auth::derive_pairwise(&state.pairwise_salt, &sub, sector_a);
        let pws_b = zeroship_core::auth::derive_pairwise(&state.pairwise_salt, &sub, sector_b);

        // One user, two apps: the issuer signs each app's pairwise subject.
        let token_a = sign_op_access_jwt(
            &jwks_signing,
            &pws_a,
            Some(&client_a),
            serde_json::json!([resource_aud_a]),
            "user@example.com",
            "OP User",
            3600,
        );
        let token_b = sign_op_access_jwt(
            &jwks_signing,
            &pws_b,
            Some(&client_b),
            serde_json::json!([resource_aud_b]),
            "user@example.com",
            "OP User",
            3600,
        );
        let req_a = bearer_req(&token_a, "app-a.zeroship.ai");
        let req_b = bearer_req(&token_b, "app-b.zeroship.ai");
        let request_id = Uuid::new_v4();

        // Batch A fix 3: the marker is keyed on `(client_id, pws_)` — the SAME
        // key the arm now reads (it projects pws_ BEFORE the lookup). Derive
        // each app's pws_ under ITS sector (the state's salt is all-zero, the
        // same salt the arm uses), and revoke ONLY app A's family.
        {
            let pool = crate::db::checkout(&db).await.expect("pool checkout");
            let conn = pool.get().await.expect("pool checkout");
            zeroship_authz::wrapper_revocation::revoke_family(&conn, &client_a, &pws_a)
                .await
                .expect("revoke_family app A");
        }
        // R1d: bust the same-node cache entry the pre-revocation read warmed,
        // mirroring the real gateway writer; otherwise the negative entry would
        // serve through its TTL and mask the just-written marker.
        state.revocation_cache.invalidate(&client_a, &pws_a);

        // App A token: revoked → Invalid. (Sector present so the arm derives
        // the SAME pws_a the marker was written under.)
        //
        // This arm only means something because the UNREVOKED app B arm below
        // Allows on the same fixture. Without that partner, an Invalid here is
        // equally consistent with the token being rejected for any reason at
        // all - which is exactly how this test passed while proving nothing.
        assert!(
            matches!(
                resolve_bearer_user_header(
                    &req_a,
                    &state,
                    &request_id,
                    Some(&client_a),
                    Some(sector_a)
                )
                .await,
                BearerOutcome::Invalid
            ),
            "revoked (client_a, pws_a) family must reject app A's token"
        );
        // App B token, SAME global sub but DIFFERENT pws_ (different sector) and
        // client: NOT revoked → Allowed (per-app scoping).
        assert!(
            matches!(
                resolve_bearer_user_header(
                    &req_b,
                    &state,
                    &request_id,
                    Some(&client_b),
                    Some(sector_b)
                )
                .await,
                BearerOutcome::Allowed(_)
            ),
            "app A revocation must NOT revoke the same sub on app B"
        );

        let pool = crate::db::checkout(&db).await.expect("pool checkout");
        let conn = pool.get().await.expect("pool checkout");
        conn.execute(
            "DELETE FROM zeroship.token_revocations WHERE sub = ANY($1)",
            &[&vec![pws_a, pws_b]],
        )
        .await
        .ok();
        drop(conn);
        drop(srv);
    }

    // ─── End-to-end positive path through resolve_auth (minor) ────────────
    //
    // The valid-Bearer success was only exercised at the helper level. These
    // drive the FULL resolve_auth on a User route and assert the line-110
    // short-circuit (BearerOutcome::Allowed → AuthOutcome::Allowed{Some}).

    #[ntex::test]
    async fn resolve_auth_valid_raw_op_on_user_route_allows_with_header() {
        let jwks_signing = ed25519_dalek::SigningKey::from_bytes(&[55u8; 32]);
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let srv = start_jwks_server(op_jwks_doc(&jwks_signing)).await;
        let base = srv.url("").trim_end_matches('/').to_string();
        let state = build_state_for_op(gateway_signing, &base);

        let global_sub = "0192f1aa-bbbb-7ccc-8ddd-eeeeffff0001";
        let sector = "https://myapp.zeroship.ai";
        let host = "myapp.zeroship.ai";
        let (client_id, resource_aud) = op_app_binding(OP_APP_UUID);
        let pws_sub =
            zeroship_core::auth::derive_pairwise(&state.pairwise_salt, global_sub, sector);
        let token = sign_op_access_jwt(
            &jwks_signing,
            &pws_sub,
            Some(&client_id),
            serde_json::json!(["http://api.zeroship.localhost", resource_aud]),
            "user@example.com",
            "OP User",
            3600,
        );
        let req = bearer_req(&token, host);
        let request_id = Uuid::new_v4();

        let outcome = resolve_auth(
            &req,
            &state,
            &user_policy(),
            &request_id,
            Some(&client_id),
            Some(sector),
        )
        .await;
        let AuthOutcome::Allowed {
            user_header: Some(header),
        } = outcome
        else {
            panic!("expected Allowed{{Some}}, got {outcome:?}");
        };
        let json = zeroship_core::auth::verify_zeroship_user_header(
            state.config.worker_key.as_bytes(),
            &header,
        )
        .expect("MAC verifies");
        let user: serde_json::Value = serde_json::from_str(&json).expect("user json");
        // The OP-issued per-app pws_ survives resolve_auth unchanged; the
        // global UUID never reaches the worker header.
        let expected_pws =
            zeroship_core::auth::derive_pairwise(&state.pairwise_salt, global_sub, sector);
        assert_eq!(user["id"], expected_pws);
        assert!(!json.contains(global_sub), "global UUID leaked: {json}");

        drop(srv);
    }

    // ─── Invalid Bearer does NOT fall back to a valid cookie ──────
    //
    // Documents the behavior: on a User/Admin route an Invalid
    // raw OP Bearer 401s and is NOT silently
    // rescued by a valid cookie session. The cookie path is DB-free: it uses a
    // SIGNED `zeroship-sess+jwt` verified locally, so the test mints a real signed
    // cookie (genuinely valid) and proves the Bearer still wins the 401.
    #[ntex::test]
    async fn resolve_auth_invalid_bearer_on_user_route_does_not_use_cookie() {
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let jwks_signing = ed25519_dalek::SigningKey::from_bytes(&[55u8; 32]);
        // Stand up a JWKS server that serves a DIFFERENT key than the token is
        // signed with, so the raw OP Bearer fails signature verification →
        // BearerOutcome::Invalid (the "recognized user-session token that
        // failed verify" case). The OidcRp issuer is pinned to OP_ISS.
        let srv = start_jwks_server(op_jwks_doc(&jwks_signing)).await;
        let base = srv.url("").trim_end_matches('/').to_string();
        let oidc_rp = crate::oidc_rp::OidcRp::new(
            &base,
            test_broker_secret(),
            b"test-stash-key-32-bytes-long----".to_vec(),
        )
        .with_issuer(OP_ISS);
        let state = build_state_with_session_and_oidc_and_db(gateway_signing, oidc_rp, None);
        let aud = "myapp.zeroship.ai";
        let client_id = "oac_myapp";
        let pws = format!("pws_{}", &Uuid::new_v4().simple().to_string()[..20]);

        // Mint a REAL, currently-valid SIGNED cookie for a user.
        let signed = issue_signed_session_cookie(&state, client_id, &pws, "relay-alias@zeroship.ai", &[]);

        // Sanity: that cookie ALONE (no Bearer) authenticates the User route.
        let cookie_name = oidc_rp::app_session_cookie_name();
        let cookie_only_req = ntex::web::test::TestRequest::default()
            .uri("/api/me")
            .header(http::header::HOST, aud)
            .header("cookie", format!("{cookie_name}={signed}"))
            .to_http_request();
        let request_id = Uuid::new_v4();
        assert!(
            matches!(
                resolve_auth(
                    &cookie_only_req,
                    &state,
                    &user_policy(),
                    &request_id,
                    Some(client_id),
                    Some("https://myapp.zeroship.ai"),
                )
                .await,
                AuthOutcome::Allowed {
                    user_header: Some(_)
                }
            ),
            "the signed cookie alone must authenticate (test fixture sanity)"
        );

        // Now attach a raw OP Bearer signed with the WRONG key (so it fails
        // verification) alongside the SAME valid cookie. The Bearer arm yields
        // Invalid; on a User route that 401s and MUST NOT fall through to the
        // (valid) cookie.
        let bad_key = ed25519_dalek::SigningKey::from_bytes(&[99u8; 32]);
        let unverifiable_bearer = sign_op_access_jwt(
            &bad_key,
            "0192f1aa-bbbb-7ccc-8ddd-eeeeffff0099",
            Some(client_id),
            serde_json::json!([client_id]),
            "user@example.com",
            "OP User",
            3600,
        );

        let shadowed_req = ntex::web::test::TestRequest::default()
            .uri("/api/me")
            .header(http::header::HOST, aud)
            .header(http::header::AUTHORIZATION, format!("Bearer {unverifiable_bearer}"))
            .header("cookie", format!("{cookie_name}={signed}"))
            .to_http_request();
        let outcome = resolve_auth(
            &shadowed_req,
            &state,
            &user_policy(),
            &request_id,
            Some(client_id),
            Some("https://myapp.zeroship.ai"),
        )
        .await;
        assert!(
            matches!(outcome, AuthOutcome::Unauthenticated),
            "Invalid Bearer on User route must 401, NOT fall back to the valid cookie, got {outcome:?}"
        );
        drop(srv);
    }

    // ─── Cookie-arm required-scope enforcement ──
    //
    // The single enforcement point in `resolve_auth` is arm-agnostic, but the
    // plain-Bearer scope tests above only verify ONE arm. This pair drives the
    // COOKIE arm — whose scopes now come from the SIGNED cookie's `scopes`
    // claim (no DB row) — through the REAL `resolve_auth`. A signed
    // cookie whose scopes cover the route's `required_scopes` is Allowed; one
    // that does not is InsufficientScope (403), proving the gate is not
    // Bearer-only. DB-free (the cookie arm is stateless).

    #[ntex::test]
    async fn resolve_auth_cookie_arm_with_required_scope_allowed() {
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let state = build_state_with_session_and_auth_ui_url_and_db(
            gateway_signing,
            "http://127.0.0.1:1",
            None,
        );
        let aud = "myapp.zeroship.ai";
        let client_id = "oac_myapp";
        let pws = format!("pws_{}", &Uuid::new_v4().simple().to_string()[..20]);

        // Signed cookie whose scopes INCLUDE the route's required scope.
        let scopes = vec!["openid".to_string(), "read:billing".to_string()];
        let signed = issue_signed_session_cookie(&state, client_id, &pws, "", &scopes);

        let cookie_name = oidc_rp::app_session_cookie_name();
        let req = ntex::web::test::TestRequest::default()
            .uri("/api/billing")
            .header(http::header::HOST, aud)
            .header("cookie", format!("{cookie_name}={signed}"))
            .to_http_request();
        let request_id = Uuid::new_v4();
        let outcome = resolve_auth(
            &req,
            &state,
            &user_policy_requiring(&["read:billing"]),
            &request_id,
            Some(client_id),
            Some("https://myapp.zeroship.ai"),
        )
        .await;

        assert!(
            matches!(outcome, AuthOutcome::Allowed { user_header: Some(_) }),
            "signed cookie granting read:billing must pass the scope gate, got {outcome:?}"
        );
    }

    #[ntex::test]
    async fn resolve_auth_cookie_arm_without_required_scope_403s() {
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let state = build_state_with_session_and_auth_ui_url_and_db(
            gateway_signing,
            "http://127.0.0.1:1",
            None,
        );
        let aud = "myapp.zeroship.ai";
        let client_id = "oac_myapp";
        let pws = format!("pws_{}", &Uuid::new_v4().simple().to_string()[..20]);

        // Signed cookie whose scopes do NOT include the route's required scope —
        // identity is fine, the grant is too narrow → 403, not 401.
        let scopes = vec!["openid".to_string(), "email".to_string()];
        let signed = issue_signed_session_cookie(&state, client_id, &pws, "", &scopes);

        let cookie_name = oidc_rp::app_session_cookie_name();
        let req = ntex::web::test::TestRequest::default()
            .uri("/api/billing")
            .header(http::header::HOST, aud)
            .header("cookie", format!("{cookie_name}={signed}"))
            .to_http_request();
        let request_id = Uuid::new_v4();
        let outcome = resolve_auth(
            &req,
            &state,
            &user_policy_requiring(&["read:billing"]),
            &request_id,
            Some(client_id),
            Some("https://myapp.zeroship.ai"),
        )
        .await;

        match outcome {
            AuthOutcome::InsufficientScope { required } => {
                assert_eq!(required, vec!["read:billing".to_string()]);
            }
            other => panic!(
                "signed cookie lacking read:billing must be InsufficientScope (403), got {other:?}"
            ),
        }
    }

    // ─── Pairwise subject projection ─────────────────────
    //
    // These cover the four properties of the consistent `pws_` contract:
    // (1) cross-app divergence (same user, two apps → different pws_);
    // (2) cross-arm + re-login consistency (the issuer and cookie minter
    //     produce the SAME pws_ for the same (user, app));
    // (3) the global UUID is ABSENT from every outward `ZeroShip-User`;
    // (4) fail-closed 503 when the route has no sector yet.
    // The DB upsert + cookie arm are PG-gated (skip when there is no test
    // database, like the revocation tests); the in-memory arms run always.

    /// A fixed global UUID + two distinct app sectors. A `pws_` derived for
    /// the SAME user under DIFFERENT sectors MUST differ — no cross-app
    /// correlation. This is the cross-app divergence property at the
    /// gateway boundary, asserted against the raw OP arm's emitted header.
    #[ntex::test]
    async fn raw_op_same_user_two_apps_get_different_pws() {
        let jwks_signing = ed25519_dalek::SigningKey::from_bytes(&[55u8; 32]);
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let srv = start_jwks_server(op_jwks_doc(&jwks_signing)).await;
        let base = srv.url("").trim_end_matches('/').to_string();
        let state = build_state_for_op(gateway_signing, &base);

        let global_sub = "0192f1aa-bbbb-7ccc-8ddd-eeeeffff0042";

        // Two genuinely distinct apps: distinct UUIDs, hence distinct client
        // ids and distinct resource audiences. Guarded, because the whole
        // property under test evaporates if both sides are the same app.
        let (client_a, resource_aud_a) = op_app_binding(OP_APP_A_UUID);
        let (client_b, resource_aud_b) = op_app_binding(OP_APP_B_UUID);
        assert_ne!(client_a, client_b, "fixture must model two DIFFERENT apps");
        assert_ne!(resource_aud_a, resource_aud_b);
        let pws_a = zeroship_core::auth::derive_pairwise(
            &state.pairwise_salt,
            global_sub,
            "https://app-a.zeroship.ai",
        );
        let pws_b = zeroship_core::auth::derive_pairwise(
            &state.pairwise_salt,
            global_sub,
            "https://app-b.zeroship.ai",
        );

        // App A.
        let token_a = sign_op_access_jwt(
            &jwks_signing,
            &pws_a,
            Some(&client_a),
            serde_json::json!([resource_aud_a]),
            "user@example.com",
            "User",
            3600,
        );
        let req_a = bearer_req(&token_a, "app-a.zeroship.ai");
        let rid = Uuid::new_v4();
        let BearerOutcome::Allowed(header_a) = resolve_bearer_user_header(
            &req_a,
            &state,
            &rid,
            Some(&client_a),
            Some("https://app-a.zeroship.ai"),
        )
        .await
        else {
            panic!("app A must Allow");
        };
        let id_a = decode_header_id(&state, &header_a);

        // App B — SAME user, different sector/client.
        let token_b = sign_op_access_jwt(
            &jwks_signing,
            &pws_b,
            Some(&client_b),
            serde_json::json!([resource_aud_b]),
            "user@example.com",
            "User",
            3600,
        );
        let req_b = bearer_req(&token_b, "app-b.zeroship.ai");
        let BearerOutcome::Allowed(header_b) = resolve_bearer_user_header(
            &req_b,
            &state,
            &rid,
            Some(&client_b),
            Some("https://app-b.zeroship.ai"),
        )
        .await
        else {
            panic!("app B must Allow");
        };
        let id_b = decode_header_id(&state, &header_b);

        assert!(id_a.starts_with("pws_"), "app A id must be pws_: {id_a}");
        assert!(id_b.starts_with("pws_"), "app B id must be pws_: {id_b}");
        assert_ne!(
            id_a, id_b,
            "same user on two apps must get DIFFERENT pws_ (cross-app divergence)"
        );
        // The global UUID never appears in either outward header.
        assert!(!id_a.contains(global_sub) && !id_b.contains(global_sub));

        drop(srv);
    }

    /// The OP already mints app access tokens with a pairwise subject. The
    /// gateway must forward that verified `pws_` unchanged rather than hashing
    /// it a second time as though it were a global UUID.
    #[ntex::test]
    async fn raw_op_pws_is_consistent_across_arms_and_relogin() {
        let jwks_signing = ed25519_dalek::SigningKey::from_bytes(&[55u8; 32]);
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let srv = start_jwks_server(op_jwks_doc(&jwks_signing)).await;
        let base = srv.url("").trim_end_matches('/').to_string();
        let state = build_state_for_op(gateway_signing, &base);

        let global_sub = "0192f1aa-bbbb-7ccc-8ddd-eeeeffff0077";
        let sector = "https://myapp.zeroship.ai";
        let host = "myapp.zeroship.ai";
        let (client_id, resource_aud) = op_app_binding(OP_APP_UUID);
        let issued_pws =
            zeroship_core::auth::derive_pairwise(&state.pairwise_salt, global_sub, sector);

        // `Issuer::issue_access_token` performs this projection before signing.
        let token = sign_op_access_jwt(
            &jwks_signing,
            &issued_pws,
            Some(&client_id),
            serde_json::json!([resource_aud]),
            "user@example.com",
            "User",
            3600,
        );
        let req = bearer_req(&token, host);
        let rid = Uuid::new_v4();
        let BearerOutcome::Allowed(header) =
            resolve_bearer_user_header(&req, &state, &rid, Some(&client_id), Some(sector)).await
        else {
            panic!("must Allow");
        };
        let arm_id = decode_header_id(&state, &header);

        assert_eq!(
            arm_id, issued_pws,
            "raw OP arm must preserve the pairwise subject the issuer signed"
        );
        assert!(arm_id.starts_with("pws_"));

        drop(srv);
    }

    #[ntex::test]
    async fn raw_op_bearer_rejects_a_pushed_disabled_principal() {
        let jwks_signing = ed25519_dalek::SigningKey::from_bytes(&[55u8; 32]);
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let srv = start_jwks_server(op_jwks_doc(&jwks_signing)).await;
        let base = srv.url("").trim_end_matches('/').to_string();
        let state = build_state_for_op(gateway_signing, &base);

        let user_id = Uuid::parse_str("0192f1aa-bbbb-7ccc-8ddd-eeeeffff0088").unwrap();
        let sector = "https://myapp.zeroship.ai";
        let host = "myapp.zeroship.ai";
        let (client_id, resource_aud) = op_app_binding(OP_APP_UUID);
        let issued_pws = zeroship_core::auth::derive_pairwise(
            &state.pairwise_salt,
            &user_id.to_string(),
            sector,
        );
        let mut routes = zeroship_core::types::RouteMap::new();
        routes.insert(
            Uuid::parse_str(OP_APP_UUID).unwrap(),
            zeroship_core::types::RouteEntry {
                name: host.to_string(),
                plan_id: "free".to_string(),
                api_key_hash: "hash".to_string(),
                deploy_hash: None,
                manifest: zeroship_bundle::Manifest::passthrough(),
                oauth_client_id: Some(client_id.clone()),
                sector_identifier: Some(sector.to_string()),
                spend_state: zeroship_core::types::SpendState::Allow,
                account_state: zeroship_core::types::AccountState::Active,
            },
        );
        state.routes.update_snapshot(
            zeroship_core::types::GatewaySnapshot {
                routes,
                principal_lifecycle: vec![
                    zeroship_core::types::GatewayPrincipalLifecycle::disabled(
                        user_id,
                        vec![issued_pws.clone()],
                    ),
                ],
                family_revocations: Vec::new(),
            },
            &state.rate_limiters,
            &state.concurrency,
        );

        let token = sign_op_access_jwt(
            &jwks_signing,
            &issued_pws,
            Some(&client_id),
            serde_json::json!([resource_aud]),
            "",
            "User",
            3600,
        );
        let outcome = resolve_bearer_user_header(
            &bearer_req(&token, host),
            &state,
            &Uuid::new_v4(),
            Some(&client_id),
            Some(sector),
        )
        .await;
        assert!(
            matches!(outcome, BearerOutcome::Invalid),
            "a disabled principal in the pushed snapshot must be rejected, got {outcome:?}"
        );

        drop(srv);
    }

    /// Fail closed when a valid raw OP token reaches a route whose OAuth
    /// provisioning is incomplete because its sector is missing.
    #[ntex::test]
    async fn raw_op_unprovisioned_sector_fails_closed() {
        let jwks_signing = ed25519_dalek::SigningKey::from_bytes(&[55u8; 32]);
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let srv = start_jwks_server(op_jwks_doc(&jwks_signing)).await;
        let base = srv.url("").trim_end_matches('/').to_string();
        let state = build_state_for_op(gateway_signing, &base);

        // The token binds correctly on BOTH client_id and resource audience,
        // so the ONLY thing that can stop it is the missing sector - i.e. the
        // request really does reach the fail-closed check rather than being
        // turned away by an earlier binding rejection.
        let (client_id, resource_aud) = op_app_binding(OP_APP_UUID);
        let token = sign_op_access_jwt(
            &jwks_signing,
            "0192f1aa-bbbb-7ccc-8ddd-eeeeffff0099",
            Some(&client_id),
            serde_json::json!([resource_aud]),
            "user@example.com",
            "User",
            3600,
        );
        let req = bearer_req(&token, "myapp.zeroship.ai");
        let rid = Uuid::new_v4();
        // No sector → fail closed.
        let outcome =
            resolve_bearer_user_header(&req, &state, &rid, Some(&client_id), None).await;
        assert!(
            matches!(outcome, BearerOutcome::ClientNotProvisioned),
            "no sector_identifier must fail closed (ClientNotProvisioned), got {outcome:?}"
        );

        // And end-to-end through resolve_auth → AuthOutcome::ClientNotProvisioned.
        let outcome = resolve_auth(
            &req,
            &state,
            &user_policy(),
            &rid,
            Some(&client_id),
            None,
        )
        .await;
        assert!(
            matches!(outcome, AuthOutcome::ClientNotProvisioned),
            "resolve_auth must surface ClientNotProvisioned (→ 503), got {outcome:?}"
        );

        drop(srv);
    }

    /// Decode the signed `ZeroShip-User` header and pull `.id` out.
    fn decode_header_id(state: &crate::GateState, header: &str) -> String {
        let json = zeroship_core::auth::verify_zeroship_user_header(
            state.config.worker_key.as_bytes(),
            header,
        )
        .expect("ZeroShip-User MAC verifies");
        let v: serde_json::Value = serde_json::from_str(&json).expect("user json");
        v["id"].as_str().expect("id is a string").to_string()
    }

    // ─── R1b: signed stateless session cookie — local-verify cookie arm ────

    /// Mint a gateway-SIGNED `zeroship-sess+jwt` session cookie via the state's
    /// session issuer, for the given app (`client_id`) + per-app `pws_` subject.
    /// This is exactly the token `POST /__zeroship/auth/session` writes into
    /// `__Host-zeroship_app_session`.
    fn issue_signed_session_cookie(
        state: &crate::GateState,
        client_id: &str,
        pws_sub: &str,
        email: &str,
        scopes: &[String],
    ) -> String {
        state
            .session_issuer
            .as_ref()
            .expect("session issuer configured")
            .issue(&crate::session_token::SessionMint {
                app: client_id,
                sub: pws_sub,
                credential_iat: 1_700_000_000,
                auth_time: Some(1_700_000_000),
                amr: &["pwd".to_string()],
                email,
                email_verified: true,
                name: "Cookie User",
                avatar: None,
                scopes,
            })
            .expect("issue signed session cookie")
    }

    fn install_disabled_principal_snapshot(
        state: &crate::GateState,
        user_id: Uuid,
        client_id: &str,
        sector: &str,
        host: &str,
    ) {
        let mut routes = zeroship_core::types::RouteMap::new();
        routes.insert(
            Uuid::new_v4(),
            zeroship_core::types::RouteEntry {
                name: host.to_string(),
                plan_id: "free".to_string(),
                api_key_hash: "hash".to_string(),
                deploy_hash: None,
                manifest: zeroship_bundle::Manifest::passthrough(),
                oauth_client_id: Some(client_id.to_string()),
                sector_identifier: Some(sector.to_string()),
                spend_state: zeroship_core::types::SpendState::Allow,
                account_state: zeroship_core::types::AccountState::Active,
            },
        );
        let pairwise = zeroship_core::auth::derive_pairwise(
            &state.pairwise_salt,
            &user_id.to_string(),
            sector,
        );
        let lifecycle = zeroship_core::types::GatewayPrincipalLifecycle::disabled(
            user_id,
            vec![pairwise],
        );
        state.routes.update_snapshot(
            zeroship_core::types::GatewaySnapshot {
                routes,
                principal_lifecycle: vec![lifecycle],
                family_revocations: Vec::new(),
            },
            &state.rate_limiters,
            &state.concurrency,
        );
    }

    /// The cookie arm verifies a gateway-signed session cookie LOCALLY and emits
    /// the correct `ZeroShip-User` (pws_ id + scopes + relay-alias email) with
    /// **NO DB read** — proven by running it with `db = None` for a valid signed
    /// cookie. No PG needed: the verify path is entirely stateless.
    #[ntex::test]
    async fn cookie_arm_local_verifies_signed_cookie_with_no_db() {
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        // db = None ⇒ the family-marker (the only DB touch) is SKIPPED, so a
        // successful Allow proves the identity path made zero DB calls.
        let state = build_state_with_session_and_auth_ui_url_and_db(
            gateway_signing,
            "http://127.0.0.1:1",
            None,
        );
        assert!(state.db.is_none(), "fixture must have no DB for this proof");

        let aud = "myapp.zeroship.ai";
        let client_id = "oac_myapp";
        let pws = format!("pws_{}", &Uuid::new_v4().simple().to_string()[..20]);
        let scopes = vec!["openid".to_string(), "email".to_string()];
        let token =
            issue_signed_session_cookie(&state, client_id, &pws, "relay-alias@zeroship.ai", &scopes);

        let cookie_name = oidc_rp::app_session_cookie_name();
        let req = ntex::web::test::TestRequest::default()
            .uri("/api/me")
            .header(http::header::HOST, aud)
            .header("cookie", format!("{cookie_name}={token}"))
            .to_http_request();
        let rid = Uuid::new_v4();

        let outcome = resolve_app_session_user_header_inner(
            &req,
            &state,
            &rid,
            Some(client_id),
        )
        .await;
        let CookieOutcome::Allowed(header) = outcome else {
            panic!("a valid signed cookie must Allow with db=None, got {outcome:?}");
        };
        // Emits the pws_ id + relay alias + scopes, straight from the claims.
        let id = decode_header_id(&state, &header);
        assert_eq!(id, pws, "cookie arm emits the cookie's pws_ subject");
        let json = zeroship_core::auth::verify_zeroship_user_header(
            state.config.worker_key.as_bytes(),
            &header,
        )
        .expect("MAC verifies");
        let user: serde_json::Value = serde_json::from_str(&json).expect("user json");
        assert_eq!(user["email"], "relay-alias@zeroship.ai", "relay alias from claim");
        assert_eq!(
            user["scopes"],
            serde_json::json!(["openid", "email"]),
            "scopes from the signed cookie claim"
        );
    }

    #[ntex::test]
    async fn cookie_arm_rejects_a_pushed_disabled_principal() {
        let state = build_state_with_session_and_auth_ui_url_and_db(
            ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]),
            "http://127.0.0.1:1",
            None,
        );
        let user_id = Uuid::new_v4();
        let client_id = "oac_cookie_lifecycle";
        let sector = "https://cookie-lifecycle.zeroship.test";
        let host = "cookie-lifecycle.zeroship.test";
        install_disabled_principal_snapshot(&state, user_id, client_id, sector, host);
        let pws = zeroship_core::auth::derive_pairwise(
            &state.pairwise_salt,
            &user_id.to_string(),
            sector,
        );
        let token = issue_signed_session_cookie(&state, client_id, &pws, "", &[]);
        let req = ntex::web::test::TestRequest::default()
            .uri("/api/me")
            .header(http::header::HOST, host)
            .header(
                "cookie",
                format!("{}={token}", oidc_rp::app_session_cookie_name()),
            )
            .to_http_request();

        let outcome = resolve_app_session_user_header_inner(
            &req,
            &state,
            &Uuid::new_v4(),
            Some(client_id),
        )
        .await;
        assert!(
            matches!(outcome, CookieOutcome::None),
            "a disabled principal's signed cookie must be rejected, got {outcome:?}"
        );
    }

    #[test]
    fn session_cookie_signing_rechecks_lifecycle_at_emission() {
        let state = build_state_with_session_and_auth_ui_url_and_db(
            ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]),
            "http://127.0.0.1:1",
            None,
        );
        let user_id = Uuid::new_v4();
        let client_id = "oac_sign_lifecycle";
        let sector = "https://sign-lifecycle.zeroship.test";
        install_disabled_principal_snapshot(
            &state,
            user_id,
            client_id,
            sector,
            "sign-lifecycle.zeroship.test",
        );
        let pws = zeroship_core::auth::derive_pairwise(
            &state.pairwise_salt,
            &user_id.to_string(),
            sector,
        );

        let result = crate::auth_token::sign_session_cookie(
            &state,
            client_id,
            &pws,
            1_700_000_000,
            None,
            None,
            None,
            None,
            None,
            &[],
            &[],
        );
        assert!(
            result.is_err(),
            "a denial pulled during mint work must prevent final cookie signing"
        );
    }

    /// A tampered, expired, wrong-app, and wrong-kid signed cookie are each
    /// rejected by the local-verify cookie arm (→ no identity). DB-free.
    #[ntex::test]
    async fn cookie_arm_rejects_tampered_expired_wrong_app_wrong_kid() {
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let state =
            build_state_with_session_and_auth_ui_url_and_db(gateway_signing, "http://127.0.0.1:1", None);
        let aud = "myapp.zeroship.ai";
        let client_id = "oac_myapp";
        let pws = format!("pws_{}", &Uuid::new_v4().simple().to_string()[..20]);
        let rid = Uuid::new_v4();
        let cookie_name = oidc_rp::app_session_cookie_name();

        let arm = |token: String, client: &str| {
            let req = ntex::web::test::TestRequest::default()
                .uri("/api/me")
                .header(http::header::HOST, aud)
                .header("cookie", format!("{cookie_name}={token}"))
                .to_http_request();
            (req, client.to_string())
        };

        let valid = issue_signed_session_cookie(&state, client_id, &pws, "", &[]);

        // (a) tampered: flip the last signature char.
        let mut chars: Vec<char> = valid.chars().collect();
        let last = chars.len() - 1;
        chars[last] = if chars[last] == 'A' { 'B' } else { 'A' };
        let tampered: String = chars.into_iter().collect();
        let (req, c) = arm(tampered, client_id);
        assert!(
            matches!(
                resolve_app_session_user_header_inner(&req, &state, &rid, Some(&c)).await,
                CookieOutcome::None
            ),
            "tampered cookie must be rejected"
        );

        // (b) wrong app: cookie minted for client_id, presented on a different
        //     route client. The verifier's `app` binding rejects it.
        let (req, c) = arm(valid.clone(), "oac_other");
        assert!(
            matches!(
                resolve_app_session_user_header_inner(&req, &state, &rid, Some(&c)).await,
                CookieOutcome::None
            ),
            "wrong-app cookie must be rejected"
        );

        // (c) wrong kid: sign with a DIFFERENT key (unknown kid) — the verifier
        //     holds only the gateway key.
        let other_key = ed25519_dalek::SigningKey::from_bytes(&[9u8; 32]);
        let other_issuer =
            crate::session_token::Issuer::new(&other_key, "https://api.zeroship.ai".into())
                .expect("issuer");
        let wrong_kid = other_issuer
            .issue(&crate::session_token::SessionMint {
                app: client_id,
                sub: &pws,
                credential_iat: 1_700_000_000,
                auth_time: None,
                amr: &[],
                email: "",
                email_verified: false,
                name: "",
                avatar: None,
                scopes: &[],
            })
            .expect("issue");
        let (req, c) = arm(wrong_kid, client_id);
        assert!(
            matches!(
                resolve_app_session_user_header_inner(&req, &state, &rid, Some(&c)).await,
                CookieOutcome::None
            ),
            "wrong-kid cookie must be rejected"
        );

        // (d) expired: hand-sign with exp in the past.
        let expired = sign_expired_session_cookie(&gateway_signing_key_of(&state), client_id, &pws);
        let (req, c) = arm(expired, client_id);
        assert!(
            matches!(
                resolve_app_session_user_header_inner(&req, &state, &rid, Some(&c)).await,
                CookieOutcome::None
            ),
            "expired cookie must be rejected"
        );
    }

    /// The signing key behind the fixture (seed [7u8;32]) — for hand-crafting
    /// edge-case tokens (expired) the issuer won't mint.
    fn gateway_signing_key_of(_state: &crate::GateState) -> ed25519_dalek::SigningKey {
        ed25519_dalek::SigningKey::from_bytes(&[7u8; 32])
    }

    /// Hand-sign a `zeroship-sess+jwt` whose `exp` is 100s in the past (beyond the 60s
    /// verify leeway).
    fn sign_expired_session_cookie(
        signing: &ed25519_dalek::SigningKey,
        client_id: &str,
        pws_sub: &str,
    ) -> String {
        use ed25519_dalek::pkcs8::EncodePrivateKey;
        use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
        let now = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        )
        .unwrap();
        let claims = crate::session_token::SessionClaims {
            iss: "https://api.zeroship.ai".into(),
            app: client_id.into(),
            sub: pws_sub.into(),
            iat: now - 2000,
            exp: now - 100,
            auth_time: None,
            amr: vec![],
            email: String::new(),
            email_verified: false,
            name: String::new(),
            avatar: None,
            scopes: vec![],
        };
        let mut header = Header::new(Algorithm::EdDSA);
        header.typ = Some(crate::session_token::SESSION_TOKEN_TYP.into());
        header.kid = Some(crate::signing::jwk_thumbprint(signing));
        let der = signing.to_pkcs8_der().unwrap();
        let key = EncodingKey::from_ed_der(der.as_bytes());
        encode(&header, &claims, &key).unwrap()
    }

    /// Hand-sign an RFC-9068 `at+jwt`-typ token with the gateway key. Used to
    /// prove the session-cookie arm's `zeroship-sess+jwt` typ gate rejects a non-
    /// session token type, even when it is gateway-signed with a valid kid.
    fn sign_at_jwt_typ_token(signing: &ed25519_dalek::SigningKey, sub: &str, aud: &str) -> String {
        use ed25519_dalek::pkcs8::EncodePrivateKey;
        use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
        let now = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        )
        .unwrap();
        let body = serde_json::json!({
            "iss": "https://api.zeroship.ai",
            "aud": aud,
            "sub": sub,
            "iat": now,
            "exp": now + 600,
        });
        let mut header = Header::new(Algorithm::EdDSA);
        header.typ = Some("at+jwt".into());
        header.kid = Some(crate::signing::jwk_thumbprint(signing));
        let der = signing.to_pkcs8_der().unwrap();
        let key = EncodingKey::from_ed_der(der.as_bytes());
        encode(&header, &body, &key).unwrap()
    }

    /// Typ separation, both directions. A signed SESSION cookie (`zeroship-sess+jwt`)
    /// presented on the Bearer arm is REJECTED (the Bearer arm only recognizes a
    /// raw OP `iss`, never the gateway-signed session token) — and a gateway-
    /// signed `at+jwt` token presented to the session-cookie arm is rejected by
    /// the session verifier's `zeroship-sess+jwt` typ gate. DB-free.
    #[ntex::test]
    async fn typ_separation_session_cookie_vs_at_jwt_both_ways() {
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let state =
            build_state_with_session_and_auth_ui_url_and_db(gateway_signing.clone(), "http://127.0.0.1:1", None);
        let aud = "myapp.zeroship.ai";
        let client_id = "oac_myapp";
        let pws = format!("pws_{}", &Uuid::new_v4().simple().to_string()[..20]);
        let rid = Uuid::new_v4();

        // (1) Session cookie presented on the Bearer arm → not a user session
        //     (its `iss` is the gateway, not OP, so it is the reserved path).
        let session_token = issue_signed_session_cookie(&state, client_id, &pws, "", &[]);
        let bearer = bearer_req(&session_token, aud);
        let bearer_outcome =
            resolve_bearer_user_header(&bearer, &state, &rid, Some(client_id), None).await;
        assert!(
            matches!(bearer_outcome, BearerOutcome::NotUserSession),
            "a zeroship-sess+jwt session cookie must not authenticate on the Bearer arm, got {bearer_outcome:?}"
        );

        // (2) An at+jwt-typ token presented to the session-cookie arm → None
        //     (the session verifier hard-rejects a non-`zeroship-sess+jwt` typ).
        let at_jwt = sign_at_jwt_typ_token(&gateway_signing, &pws, aud);
        let cookie_name = oidc_rp::app_session_cookie_name();
        let req = ntex::web::test::TestRequest::default()
            .uri("/api/me")
            .header(http::header::HOST, aud)
            .header("cookie", format!("{cookie_name}={at_jwt}"))
            .to_http_request();
        let cookie_outcome = resolve_app_session_user_header_inner(
            &req,
            &state,
            &rid,
            Some(client_id),
        )
        .await;
        assert!(
            matches!(cookie_outcome, CookieOutcome::None),
            "an at+jwt typ token must be rejected by the session-cookie arm, got {cookie_outcome:?}"
        );
    }

    /// A previous-kid signed cookie still verifies during the rotation overlap
    /// (the verifier holds [current, previous]). DB-free.
    #[ntex::test]
    async fn cookie_arm_accepts_previous_kid_during_overlap() {
        // current = B (seed 7); previous = A (seed 9).
        let current = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let prev = ed25519_dalek::SigningKey::from_bytes(&[9u8; 32]);
        let mut state = build_state_with_session_and_auth_ui_url_and_db(
            current.clone(),
            "http://127.0.0.1:1",
            None,
        );
        // Rebuild the session verifier as [current, previous] (overlap window).
        {
            let s = std::sync::Arc::get_mut(&mut state).expect("unique");
            s.session_verifier = Some(std::sync::Arc::new(
                crate::session_token::Verifier::with_previous(
                    &current.verifying_key(),
                    &prev.verifying_key(),
                    "https://api.zeroship.ai".into(),
                ),
            ));
        }
        let aud = "myapp.zeroship.ai";
        let client_id = "oac_myapp";
        let pws = format!("pws_{}", &Uuid::new_v4().simple().to_string()[..20]);

        // Mint under the PREVIOUS key A.
        let prev_issuer =
            crate::session_token::Issuer::new(&prev, "https://api.zeroship.ai".into()).expect("issuer");
        let token = prev_issuer
            .issue(&crate::session_token::SessionMint {
                app: client_id,
                sub: &pws,
                credential_iat: 1_700_000_000,
                auth_time: None,
                amr: &[],
                email: "",
                email_verified: false,
                name: "",
                avatar: None,
                scopes: &[],
            })
            .expect("issue under prev key");

        let cookie_name = oidc_rp::app_session_cookie_name();
        let req = ntex::web::test::TestRequest::default()
            .uri("/api/me")
            .header(http::header::HOST, aud)
            .header("cookie", format!("{cookie_name}={token}"))
            .to_http_request();
        let outcome = resolve_app_session_user_header_inner(
            &req,
            &state,
            &Uuid::new_v4(),
            Some(client_id),
        )
        .await;
        assert!(
            matches!(outcome, CookieOutcome::Allowed(_)),
            "a previous-kid cookie must verify during the overlap, got {outcome:?}"
        );
    }

    /// A revoked `(client_id, pws_)` family marker makes the cookie arm reject a
    /// still-valid signed cookie — revocation works STATELESSLY (the same cached
    /// per-app marker the Bearer arm uses). PG-gated.
    #[compio::test]
    async fn cookie_arm_rejects_revoked_family_statelessly() {
        let db = connect_auth_db().await;
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let state = build_state_with_session_and_auth_ui_url_and_db(
            gateway_signing,
            "http://127.0.0.1:1",
            Some(db.clone()),
        );
        let aud = "myapp.zeroship.ai";
        let client_id = "oac_myapp";
        let pws = format!("pws_{}", &Uuid::new_v4().simple().to_string()[..20]);
        let token = issue_signed_session_cookie(&state, client_id, &pws, "", &[]);
        let cookie_name = oidc_rp::app_session_cookie_name();
        let req = ntex::web::test::TestRequest::default()
            .uri("/api/me")
            .header(http::header::HOST, aud)
            .header("cookie", format!("{cookie_name}={token}"))
            .to_http_request();
        let rid = Uuid::new_v4();

        // Before revocation: Allowed.
        assert!(
            matches!(
                resolve_app_session_user_header_inner(&req, &state, &rid, Some(client_id))
                    .await,
                CookieOutcome::Allowed(_)
            ),
            "valid signed cookie must Allow before revocation"
        );

        // Revoke the (client_id, pws_) family the way /signout does — and bust
        // the same-node cache the way the real `/signout` writer does (R1d), so
        // the next read reloads the just-written marker instead of serving the
        // pre-revocation "not revoked" entry through its TTL.
        {
            let pool = crate::db::checkout(&db).await.expect("pool");
            let conn = pool.get().await.expect("pool");
            zeroship_authz::wrapper_revocation::revoke_family(&conn, client_id, &pws)
                .await
                .expect("revoke_family");
        }
        state.revocation_cache.invalidate(client_id, &pws);

        // After revocation: the SAME valid cookie is rejected (stateless verify
        // succeeds, the family-marker gate fails it).
        let after = resolve_app_session_user_header_inner(
            &req,
            &state,
            &rid,
            Some(client_id),
        )
        .await;
        assert!(
            matches!(after, CookieOutcome::None),
            "a revoked family must reject the still-valid signed cookie, got {after:?}"
        );

        // Cleanup the marker.
        {
            let pool = crate::db::checkout(&db).await.expect("pool");
            let conn = pool.get().await.expect("pool");
            conn.execute(
                "DELETE FROM zeroship.token_revocations WHERE client_id = $1 AND sub = $2",
                &[&client_id, &pws],
            )
            .await
            .ok();
        }
    }

    // ─── R1d revocation-cache regression tests ──────────────────────────
    //
    // Each is constructed to FAIL on pre-R1d code (no cache) or on a naive
    // cache (a precomputed bool, a missing negative-cache, no TTL, no bust).

    /// Swap a fresh `RevocationCache` with the given TTL onto a (uniquely-owned)
    /// state `Arc`. Used by the staleness / fail-closed tests to drive expiry
    /// deterministically without sleeping.
    fn set_revocation_cache_ttl(state: &mut std::sync::Arc<crate::GateState>, ttl_secs: u64) {
        let s = std::sync::Arc::get_mut(state).expect("state Arc must be unique");
        s.revocation_cache = std::sync::Arc::new(
            zeroship_authz::wrapper_revocation::RevocationCache::with_ttl_and_capacity(
                ttl_secs, 1024,
            ),
        );
    }

    /// Replace `state.db` (point it at an unreachable DSN, or drop it) on a
    /// uniquely-owned state `Arc`.
    fn set_state_db(
        state: &mut std::sync::Arc<crate::GateState>,
        db: Option<crate::db::DbConfig>,
    ) {
        let s = std::sync::Arc::get_mut(state).expect("state Arc must be unique");
        s.db = db;
    }

    /// (a) STALENESS BOUND. Warm the cache with "not revoked", THEN write a
    /// revocation marker, THEN let the entry expire (TTL=0 ⇒ the next read is a
    /// miss) — the next check must REJECT, proving a cross-node revocation is
    /// honored within `<= TTL`.
    ///
    /// FAILS on a naive infinite-TTL cache (the stale "not revoked" would serve
    /// forever) and proves the entry is re-loaded from the DB after expiry.
    /// PG-gated.
    #[compio::test]
    async fn revocation_cache_honors_revocation_after_ttl_expiry() {
        let db = connect_auth_db().await;
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let mut state = build_state_with_session_and_auth_ui_url_and_db(
            gateway_signing,
            "http://127.0.0.1:1",
            Some(db.clone()),
        );
        // TTL=0 ⇒ every stored entry reads back as a MISS, so each request
        // reloads from the DB. This is the "entry expired" condition.
        set_revocation_cache_ttl(&mut state, 0);

        let client_id = "oac_myapp";
        let pws = format!("pws_{}", &Uuid::new_v4().simple().to_string()[..20]);
        let token = issue_signed_session_cookie(&state, client_id, &pws, "", &[]);
        let cookie_name = oidc_rp::app_session_cookie_name();
        let req = ntex::web::test::TestRequest::default()
            .uri("/api/me")
            .header(http::header::HOST, "myapp.zeroship.ai")
            .header("cookie", format!("{cookie_name}={token}"))
            .to_http_request();
        let rid = Uuid::new_v4();

        // Warm: pre-revocation read caches "not revoked" (which immediately
        // expires under TTL=0) → Allowed.
        assert!(
            matches!(
                resolve_app_session_user_header_inner(&req, &state, &rid, Some(client_id)).await,
                CookieOutcome::Allowed(_)
            ),
            "valid cookie must Allow before any revocation"
        );

        // Cross-node revoke: write the marker DIRECTLY (no same-node bust). The
        // ONLY thing that can make the next read honor it is the TTL expiry
        // forcing a DB reload.
        {
            let pool = crate::db::checkout(&db).await.expect("pool");
            let conn = pool.get().await.expect("pool");
            zeroship_authz::wrapper_revocation::revoke_family(&conn, client_id, &pws)
                .await
                .expect("revoke_family");
        }

        // After TTL expiry: the entry is a miss → DB reload → marker found →
        // REJECT.
        let after =
            resolve_app_session_user_header_inner(&req, &state, &rid, Some(client_id)).await;
        assert!(
            matches!(after, CookieOutcome::None),
            "after TTL expiry the cross-node revocation must be honored, got {after:?}"
        );

        // Cleanup.
        {
            let pool = crate::db::checkout(&db).await.expect("pool");
            let conn = pool.get().await.expect("pool");
            conn.execute(
                "DELETE FROM zeroship.token_revocations WHERE client_id = $1 AND sub = $2",
                &[&client_id, &pws],
            )
            .await
            .ok();
        }
    }

    /// (b) NEGATIVE-CACHE DB-AVOIDANCE. Warm an unrevoked family via a live DB
    /// (caches `Some(None)`), THEN point `state.db` at an UNREACHABLE DSN and
    /// assert the cached "not revoked" entry STILL serves Allowed within the
    /// TTL — proving the 2nd consecutive check did NOT touch the DB (a DB touch
    /// against the dead DSN would fail-closed to `None`). After the TTL expires,
    /// the no-DB miss correctly fails CLOSED.
    ///
    /// The no-DB-on-hit property is made observable two ways: (1) the second
    /// read Allows DESPITE a dead DB (only possible from the cache), and (2)
    /// `revocation_cache.get(...)` is asserted to hold `Some(None)`. FAILS on a
    /// cache without negative caching (an unrevoked family would store nothing
    /// → the second read would hit the dead DB → fail closed). PG-gated.
    #[compio::test]
    async fn revocation_cache_negative_entry_serves_without_db_within_ttl() {
        let db = connect_auth_db().await;
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        // Long TTL so the warmed negative entry is unquestionably fresh for the
        // second read.
        let mut state = build_state_with_session_and_auth_ui_url_and_db(
            gateway_signing,
            "http://127.0.0.1:1",
            Some(db.clone()),
        );
        set_revocation_cache_ttl(&mut state, 600);

        let client_id = "oac_myapp";
        let pws = format!("pws_{}", &Uuid::new_v4().simple().to_string()[..20]);
        // Ensure no stale marker exists for this fresh family.
        {
            let pool = crate::db::checkout(&db).await.expect("pool");
            let conn = pool.get().await.expect("pool");
            conn.execute(
                "DELETE FROM zeroship.token_revocations WHERE client_id = $1 AND sub = $2",
                &[&client_id, &pws],
            )
            .await
            .ok();
        }
        let token = issue_signed_session_cookie(&state, client_id, &pws, "", &[]);
        let cookie_name = oidc_rp::app_session_cookie_name();
        let req = ntex::web::test::TestRequest::default()
            .uri("/api/me")
            .header(http::header::HOST, "myapp.zeroship.ai")
            .header("cookie", format!("{cookie_name}={token}"))
            .to_http_request();
        let rid = Uuid::new_v4();

        // Warm (DB present): loads `None` (no marker) and caches it.
        assert!(
            matches!(
                resolve_app_session_user_header_inner(&req, &state, &rid, Some(client_id)).await,
                CookieOutcome::Allowed(_)
            ),
            "first read warms the negative cache entry"
        );
        // The negative entry is now present and fresh — observable directly.
        assert_eq!(
            state.revocation_cache.get(client_id, &pws, std::time::Instant::now()),
            Some(None),
            "negative caching is mandatory: an unrevoked family must be cached as None"
        );

        // Now point the state's DB at an UNREACHABLE DSN. If the 2nd read hit
        // the DB it would fail-closed (the miss + DB error path → None). It does
        // NOT, because the fresh negative cache entry serves the answer with
        // ZERO DB contact.
        set_state_db(
            &mut state,
            Some(crate::db::DbConfig::new(
                "postgres://nope:nope@127.0.0.1:1/none",
                1,
            )),
        );
        let second =
            resolve_app_session_user_header_inner(&req, &state, &rid, Some(client_id)).await;
        assert!(
            matches!(second, CookieOutcome::Allowed(_)),
            "a fresh negative cache entry must serve Allowed without touching the DB, got {second:?}"
        );

        // Replace the cache with an empty one (TTL=0): the next read is a cold
        // miss + unreachable DB → fails CLOSED.
        set_revocation_cache_ttl(&mut state, 0);
        // Re-warm under TTL=0 is impossible (the DB is unreachable), so the next
        // read is a miss against a dead DB.
        let after_expiry =
            resolve_app_session_user_header_inner(&req, &state, &rid, Some(client_id)).await;
        assert!(
            matches!(after_expiry, CookieOutcome::None),
            "after TTL expiry a cache miss with a dead DB must fail closed, got {after_expiry:?}"
        );

        // Cleanup (reconnect via the good DSN).
        {
            let pool = crate::db::checkout(&db).await.expect("pool");
            let conn = pool.get().await.expect("pool");
            conn.execute(
                "DELETE FROM zeroship.token_revocations WHERE client_id = $1 AND sub = $2",
                &[&client_id, &pws],
            )
            .await
            .ok();
        }
    }

    /// (c) SAME-NODE BUST. A gateway signout/revoke for `(client_id, sub)` makes
    /// the very next check REJECT WITHOUT waiting for the TTL. Uses a LONG TTL
    /// (600 s) so a passing result can ONLY come from the immediate bust, not
    /// from expiry.
    ///
    /// FAILS on a cache that lacks a write-side bust (the stale "not revoked"
    /// would serve for the full 600 s). PG-gated.
    #[compio::test]
    async fn revocation_cache_same_node_bust_takes_effect_immediately() {
        let db = connect_auth_db().await;
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let mut state = build_state_with_session_and_auth_ui_url_and_db(
            gateway_signing,
            "http://127.0.0.1:1",
            Some(db.clone()),
        );
        // Long TTL: only the bust (not expiry) can produce a rejection.
        set_revocation_cache_ttl(&mut state, 600);

        let client_id = "oac_myapp";
        let pws = format!("pws_{}", &Uuid::new_v4().simple().to_string()[..20]);
        let token = issue_signed_session_cookie(&state, client_id, &pws, "", &[]);
        let cookie_name = oidc_rp::app_session_cookie_name();
        let req = ntex::web::test::TestRequest::default()
            .uri("/api/me")
            .header(http::header::HOST, "myapp.zeroship.ai")
            .header("cookie", format!("{cookie_name}={token}"))
            .to_http_request();
        let rid = Uuid::new_v4();

        // Warm "not revoked" (cached for 600 s).
        assert!(
            matches!(
                resolve_app_session_user_header_inner(&req, &state, &rid, Some(client_id)).await,
                CookieOutcome::Allowed(_)
            ),
            "valid cookie must Allow before signout"
        );

        // Same-node signout: write the marker AND bust the cache (exactly what
        // the real `/signout` writer does, R1d).
        {
            let pool = crate::db::checkout(&db).await.expect("pool");
            let conn = pool.get().await.expect("pool");
            zeroship_authz::wrapper_revocation::revoke_family(&conn, client_id, &pws)
                .await
                .expect("revoke_family");
        }
        state.revocation_cache.invalidate(client_id, &pws);

        // The very next check rejects — no 600 s TTL wait.
        let after =
            resolve_app_session_user_header_inner(&req, &state, &rid, Some(client_id)).await;
        assert!(
            matches!(after, CookieOutcome::None),
            "a same-node bust must reject the very next request without a TTL wait, got {after:?}"
        );

        // Cleanup.
        {
            let pool = crate::db::checkout(&db).await.expect("pool");
            let conn = pool.get().await.expect("pool");
            conn.execute(
                "DELETE FROM zeroship.token_revocations WHERE client_id = $1 AND sub = $2",
                &[&client_id, &pws],
            )
            .await
            .ok();
        }
    }

    /// (d) FAIL-CLOSED on cache-miss + DB error. With the DB configured but
    /// UNREACHABLE and a cold cache, the revocation read errors → the cookie arm
    /// MUST reject (`CookieOutcome::None`), preserving the pre-R1d fail-closed
    /// posture. No PG needed (the point is the DB is unreachable).
    #[ntex::test]
    async fn revocation_cache_miss_plus_db_error_fails_closed() {
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        // DB configured (so the revocation block runs) but pointed at a dead
        // address — the pool checkout/query will error.
        let dead_db = crate::db::DbConfig::new("postgres://nope:nope@127.0.0.1:1/none", 1);
        let state = build_state_with_session_and_auth_ui_url_and_db(
            gateway_signing,
            "http://127.0.0.1:1",
            Some(dead_db),
        );

        let client_id = "oac_myapp";
        let pws = format!("pws_{}", &Uuid::new_v4().simple().to_string()[..20]);
        let token = issue_signed_session_cookie(&state, client_id, &pws, "", &[]);
        let cookie_name = oidc_rp::app_session_cookie_name();
        let req = ntex::web::test::TestRequest::default()
            .uri("/api/me")
            .header(http::header::HOST, "myapp.zeroship.ai")
            .header("cookie", format!("{cookie_name}={token}"))
            .to_http_request();
        let rid = Uuid::new_v4();

        // Cold cache + dead DB ⇒ miss + DB error ⇒ reject (fail-closed).
        let outcome =
            resolve_app_session_user_header_inner(&req, &state, &rid, Some(client_id)).await;
        assert!(
            matches!(outcome, CookieOutcome::None),
            "a cache miss followed by a DB error must fail closed, got {outcome:?}"
        );
        // And nothing was cached (the error path must not poison the cache).
        assert!(
            state
                .revocation_cache
                .get(client_id, &pws, std::time::Instant::now())
                .is_none(),
            "a failed DB read must NOT populate the cache"
        );
    }

    /// The LIVE per-request dispatch gate `resolve_auth` authenticates a signed
    /// session cookie and yields a `pws_` `ZeroShip-User`; the BFF P3 anti-CSRF
    /// gate still drops a cross-origin state-changing POST riding the Lax cookie
    /// (→ 401 on a `User` route), while a same-origin POST authenticates. DB-free
    /// (the signed cookie verify is stateless; no marker means no revocation).
    #[ntex::test]
    async fn resolve_auth_cookie_arm_authenticates_signed_cookie_and_enforces_csrf() {
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let state =
            build_state_with_session_and_auth_ui_url_and_db(gateway_signing, "http://127.0.0.1:1", None);
        let aud = "myapp.zeroship.ai";
        let client_id = "oac_myapp";
        let pws = format!("pws_{}", &Uuid::new_v4().simple().to_string()[..20]);
        let token =
            issue_signed_session_cookie(&state, client_id, &pws, "relay-alias@zeroship.ai", &[]);
        let cookie_name = oidc_rp::app_session_cookie_name();
        let request_id = Uuid::new_v4();

        // Safe GET with the signed cookie: Allow with the pws_ header.
        let get_req = ntex::web::test::TestRequest::default()
            .uri("/api/me")
            .header(http::header::HOST, aud)
            .header("cookie", format!("{cookie_name}={token}"))
            .to_http_request();
        let outcome = resolve_auth(
            &get_req,
            &state,
            &user_policy(),
            &request_id,
            Some(client_id),
            None,
        )
        .await;
        let AuthOutcome::Allowed { user_header: Some(header) } = outcome else {
            panic!("signed cookie must authenticate on the live dispatch arm, got {outcome:?}");
        };
        assert_eq!(decode_header_id(&state, &header), pws);

        // Cross-origin state-changing POST riding the Lax cookie → Unauthenticated.
        let csrf_post = ntex::web::test::TestRequest::default()
            .method(ntex::http::Method::POST)
            .uri("/api/transfer")
            .header(http::header::HOST, aud)
            .header(http::header::ORIGIN, "https://evil.example")
            .header("cookie", format!("{cookie_name}={token}"))
            .to_http_request();
        let csrf_outcome = resolve_auth(
            &csrf_post,
            &state,
            &user_policy(),
            &request_id,
            Some(client_id),
            None,
        )
        .await;
        assert!(
            matches!(csrf_outcome, AuthOutcome::Unauthenticated),
            "cross-origin POST riding the Lax session cookie MUST be rejected (P3), got {csrf_outcome:?}"
        );

        // Same-origin POST authenticates (the legit SPA mutation path).
        let same_origin_post = ntex::web::test::TestRequest::default()
            .method(ntex::http::Method::POST)
            .uri("/api/transfer")
            .header(http::header::HOST, aud)
            .header(http::header::ORIGIN, "http://myapp.zeroship.ai") // origin_scheme=Http
            .header("sec-fetch-site", "same-origin")
            .header("cookie", format!("{cookie_name}={token}"))
            .to_http_request();
        let ok_outcome = resolve_auth(
            &same_origin_post,
            &state,
            &user_policy(),
            &request_id,
            Some(client_id),
            None,
        )
        .await;
        assert!(
            matches!(ok_outcome, AuthOutcome::Allowed { user_header: Some(_) }),
            "a same-origin state-changing POST MUST authenticate, got {ok_outcome:?}"
        );
    }

    // ─── Batch A fix 3: Bearer revocation parity ──────────────────────────

    /// Write the family marker the EXACT way `/signout` does — keyed on
    /// `(client_id, pws_)` where `pws_ = derive_pairwise(salt, global_uuid,
    /// sector)` — then assert that a still-live raw OP Bearer token for the
    /// same `(client_id, user)` is rejected. Pre-fix the lookup used the GLOBAL
    /// UUID while the writer keyed on `pws_`, so a real signout never matched a
    /// live token. PG-gated.
    ///
    /// `#[ntex::test]` (not `#[compio::test]`) because it stands up an
    /// `ntex::web::test::server` JWKS mock, which requires the ntex runtime.
    #[ntex::test]
    async fn revocation_keyed_on_pws_rejects_raw_op_bearer() {
        let db = connect_auth_db().await;

        let jwks_signing = ed25519_dalek::SigningKey::from_bytes(&[55u8; 32]);
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);

        let client_id = "oac_revparity";
        let host = "myapp.zeroship.ai";
        let sector = "https://myapp.zeroship.ai";
        let global_sub = format!("0192f1aa-bbbb-7ccc-8ddd-{:012x}", rand_suffix());

        // Build a state whose oidc_rp JWKS serves the OP key. Read the SAME
        // salt the Bearer arm will use off the built state and derive the
        // WRITER's pws_ from it — no fragile Arc mutation.
        let srv = start_jwks_server(op_jwks_doc(&jwks_signing)).await;
        let base = srv.url("").trim_end_matches('/').to_string();
        let oidc_rp = crate::oidc_rp::OidcRp::new(
            &base,
            test_broker_secret(),
            b"test-stash-key-32-bytes-long----".to_vec(),
        )
        .with_issuer(OP_ISS);
        let bearer_state =
            build_state_with_session_and_oidc_and_db(gateway_signing, oidc_rp, Some(db.clone()));

        // The pws_ the WRITER (/signout) keys on — derive_pairwise under the
        // route's sector with the SAME salt the arm uses (read off the state).
        let pws_sub =
            zeroship_core::auth::derive_pairwise(&bearer_state.pairwise_salt, &global_sub, sector);

        // Write the family marker EXACTLY as browser_auth::signout does:
        // revoke_family(client_id, pws_sub).
        {
            let pool = crate::db::checkout(&db).await.expect("pool checkout");
            let conn = pool.get().await.expect("pool get");
            zeroship_authz::wrapper_revocation::revoke_family(&conn, client_id, &pws_sub)
                .await
                .expect("signout-style revoke_family on (client_id, pws_)");
        }

        let token = sign_op_access_jwt(
            &jwks_signing,
            &pws_sub,
            Some(client_id),
            serde_json::json!(["http://api.zeroship.localhost"]),
            "user@example.com",
            "OP User",
            3600,
        );
        let req = bearer_req(&token, host);
        let request_id = Uuid::new_v4();
        let outcome = resolve_bearer_user_header(
            &req,
            &bearer_state,
            &request_id,
            Some(client_id),
            Some(sector),
        )
        .await;
        assert!(
            matches!(outcome, BearerOutcome::Invalid),
            "a still-live raw OP Bearer must be rejected by the pws_-keyed marker, got {outcome:?}"
        );

        // Cleanup.
        let pool = crate::db::checkout(&db).await.expect("pool checkout");
        let conn = pool.get().await.expect("pool get");
        conn.execute(
            "DELETE FROM zeroship.token_revocations WHERE client_id = $1 AND sub = $2",
            &[&client_id, &pws_sub],
        )
        .await
        .ok();
        drop(srv);
    }
}
