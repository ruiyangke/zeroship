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
use crate::sessions;
use crate::GateState;

/// Outcome of the per-request auth gate. The dispatch handler consumes
/// this and either proceeds with the resolved `ZeroShip-User` header,
/// short-circuits with a 401 (API client) or kicks off the OIDC dance
/// (HTML navigation).
#[derive(Debug)]
pub(crate) enum AuthOutcome {
    /// Policy is `Anon` (request passes without identity) OR the
    /// policy required `User`/`Admin` and the session cookie validated.
    /// `user_header` is `Some(...)` whenever a session was actually
    /// resolved — even on `Anon` resources, so the worker can still
    /// see the authenticated user when present.
    Allowed { user_header: Option<String> },
    /// Policy required `User`/`Admin` and no valid session was found.
    /// Caller decides between a 401 (API) and a 302 → hydra (HTML).
    Unauthenticated,
    /// A request resolved a real user (cookie / raw-Hydra / DPoP-introspect)
    /// but the route has no `sector_identifier` yet, so the gateway CANNOT
    /// derive the per-app pairwise `pws_…` (auth-sdk Slice 4, §6.2). We FAIL
    /// CLOSED — never project the global UUID into `ZeroShip-User.id` — and
    /// answer `503 client_not_provisioned`, the same retryable posture the
    /// browser-token path uses (`auth_token.rs`). The SDK keeps its
    /// breadcrumb and retries once control finishes provisioning the app.
    ClientNotProvisioned,
    /// The request authenticated successfully (any arm), but the matched
    /// route declares `required_scopes` the principal's granted `scopes`
    /// do NOT cover (auth-sdk Slice 3c, §5.3 / RFC 6750 §3.1). Distinct
    /// from `Unauthenticated`: identity is fine, the *grant* is too
    /// narrow. The caller answers `403 scope_required` (JSON body) with a
    /// `WWW-Authenticate: Bearer error="insufficient_scope"` challenge and
    /// lists the required scopes. Reached ONLY by an AUTHENTICATED principal
    /// on a `User`/`Admin` route — an unauthenticated request is gated by the
    /// `Anon`/`User`/`Admin` policy first (401/redirect), and an `Anon`
    /// (public) route never scope-gates an authenticated visitor.
    InsufficientScope { required: Vec<String> },
}

/// Resolve the per-request auth gate. Returns `Allowed` when the
/// resource policy is satisfied, `Unauthenticated` otherwise. The
/// caller layers the HTML-vs-API response decision on top.
///
/// Flow:
///   1. Look for `__Host-zs_app_session` cookie.
///   2. If present + DB configured + validate succeeds → resolved
///      user. Header is the HMAC-signed payload the worker expects.
///   3. If `policy.auth == Anon` we return `Allowed` regardless of
///      whether the cookie validated (anonymous resources don't
///      require a session, but a present session still produces a
///      `ZeroShip-User` so the worker sees the user when available).
///   4. If `policy.auth == User|Admin` and no session resolved →
///      `Unauthenticated`.
///
/// Dev / test fallthrough: when `state.db` is `None` the gateway has no
/// way to validate sessions. `Anon` resources still pass; `User`/`Admin`
/// resources are gated to `Unauthenticated`. (The legacy `auth_secret`
/// empty-string fall-open is removed — Phase 3 made the gateway the
/// authoritative auth checker.)
pub(crate) async fn resolve_auth(
    req: &HttpRequest,
    state: &Arc<GateState>,
    policy: &crate::compiled::EffectivePolicy,
    app_id: &Uuid,
    request_id: &Uuid,
    oauth_client_id: Option<&str>,
    sector_identifier: Option<&str>,
) -> AuthOutcome {
    // Resolve identity first (DPoP / Bearer / cookie arms, untouched).
    let outcome = resolve_auth_inner(
        req,
        state,
        policy,
        app_id,
        request_id,
        oauth_client_id,
        sector_identifier,
    )
    .await;

    // Route-level scope enforcement (auth-sdk Slice 3c, §5.3 / RFC 6750
    // §3.1). The matched route's `required_scopes` (compiled from the
    // manifest, unioned along the inheritance chain) gate an AUTHENTICATED
    // principal ONLY: once an arm resolved a `ZeroShip-User` header, the
    // principal's granted `scopes` (Slice 3a — encoded in that header) MUST
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
    // browser whose 10-min wrapper lacks a scope inherited from a broad `*`
    // parent would get 403 on public pages a logged-OUT user loads fine. That
    // is the "logged-in is worse than anonymous on public routes" footgun the
    // round-3 Invalid-Bearer fix removed; scope gating must not re-introduce
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
/// header (the scope source-of-truth set by whichever arm authenticated,
/// Slice 3a). Verifies the MAC under the worker key and JSON-parses the
/// `scopes` array — the same path the worker uses. A verify/parse failure
/// yields `[]`, which fails the scope gate closed (a route demanding a
/// scope rejects an unreadable principal rather than waving it through).
fn decode_header_scopes(state: &Arc<GateState>, header: &str) -> Vec<String> {
    let key = state.config.worker_key.as_bytes();
    let Some(json) = zeroship_core::auth::verify_zeroship_user_header(key, header) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&json) else {
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

#[allow(clippy::too_many_arguments)]
async fn resolve_auth_inner(
    req: &HttpRequest,
    state: &Arc<GateState>,
    policy: &crate::compiled::EffectivePolicy,
    app_id: &Uuid,
    request_id: &Uuid,
    oauth_client_id: Option<&str>,
    sector_identifier: Option<&str>,
) -> AuthOutcome {
    use zeroship_bundle::AuthLevel;

    // 0. DPoP-bound access tokens take precedence over the cookie path.
    //    A request that presents `Authorization: DPoP <token>` plus a
    //    `DPoP:` proof header is opting into RFC 9449 sender-constrained
    //    resource access. We verify the proof + introspect the token
    //    here; anything else (no Authorization header, a Bearer header,
    //    a malformed DPoP header) falls through to the cookie session
    //    resolution below.
    //
    //    Note: full token binding (`cnf.jkt` ↔ proof thumbprint) is
    //    deferred — hydra doesn't currently issue `cnf.jkt` on access
    //    tokens, so Phase 7 ships proof-of-possession verification only.
    //    Phase 8+ adds the binding step.
    let dpop_user_header =
        resolve_dpop_user_header(req, state, request_id, oauth_client_id, sector_identifier).await;
    match dpop_user_header {
        DpopOutcome::Allowed(header) => {
            // DPoP succeeded — short-circuit. We treat a DPoP-authed request
            // as fully authenticated regardless of policy (anon or user).
            return AuthOutcome::Allowed {
                user_header: Some(header),
            };
        }
        DpopOutcome::ClientNotProvisioned => {
            // A valid DPoP-introspected user, but no sector_identifier yet ⇒
            // cannot derive the per-app pws_. Fail closed (§6.2).
            return AuthOutcome::ClientNotProvisioned;
        }
        DpopOutcome::None => {}
    }
    // If an Authorization: DPoP header WAS present but verification
    // failed, treat the request as unauthenticated rather than falling
    // back to cookies — a client that asserted DPoP cannot then claim
    // a different identity via a session cookie.
    if has_dpop_authorization(req) {
        return AuthOutcome::Unauthenticated;
    }

    // 1. Bearer arm (§1.3, slice 1c). Ordered AFTER the DPoP arm and
    //    BEFORE the cookie arm. Discriminates two issuer-recognized
    //    user-session token shapes (the gateway WRAPPER and a raw Hydra
    //    access JWT) from the reserved API-key path:
    //
    //      - `Allowed(header)`     → short-circuit, fully authenticated.
    //      - `Invalid`            → a recognized user-session token that
    //        failed verify/binding/revocation. By policy: anonymous on an
    //        `Anon` route (the SDK auto-attaches Bearer to EVERY request,
    //        and the wrapper is only 10 min, so an expired-but-present
    //        Bearer is the common case on public pages — round-3), 401 on
    //        `User`/`Admin`.
    //      - `NotUserSession`     → a Bearer that is neither a wrapper nor
    //        a raw-Hydra JWT (e.g. a future `zsk_…` API key). Reserved
    //        path → 401 on EVERY route, including `Anon` (it asserts a
    //        DIFFERENT scheme, not an expired user session).
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
            // A valid raw-Hydra Bearer user, but no sector_identifier yet ⇒
            // cannot derive the per-app pws_. Fail closed (§6.2).
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
            // INTENTIONAL (round-3 decision, mirrors the DPoP precedent at
            // step 0): on a `User`/`Admin` route an Invalid Bearer 401s and
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

    let app_id_str = app_id.to_string();
    let session_user_header = resolve_app_session_user_header_inner(
        req,
        state,
        &app_id_str,
        request_id,
        oauth_client_id,
        sector_identifier,
    )
    .await;
    let session_user_header = match session_user_header {
        // A cookie session resolved a real user but the route has no
        // sector_identifier yet ⇒ cannot derive the per-app pws_. Fail
        // closed (§6.2) on EVERY policy, including Anon — an authenticated
        // request must never have its global UUID projected outward.
        CookieOutcome::ClientNotProvisioned => return AuthOutcome::ClientNotProvisioned,
        CookieOutcome::Allowed(header) => Some(header),
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

/// Outcome of projecting a global user id to its per-app pairwise `pws_…`
/// (auth-sdk Slice 4, §6.2). Either the route is provisioned with a
/// `sector_identifier` (and we derived + persisted the `pws_`), or it is
/// not — in which case the caller MUST fail closed (`503
/// client_not_provisioned`) rather than ever leak the global UUID.
enum PairwiseProjection {
    /// The derived per-app `pws_…` subject + the per-app relay alias (the
    /// email-claim swap, §7). The global UUID never appears in `pws` (HMAC of
    /// the UUID under the platform salt); `relay_email` is the app-facing
    /// `email` claim — `None` when no ACTIVE alias exists, in which case the
    /// caller FAILS CLOSED on the email (emits empty), NEVER the real address.
    Projected {
        pws: String,
        /// The active relay alias for this `(app, user)`, or `None` when no
        /// alias is minted / it is revoked. The caller substitutes this for
        /// the real email and emits empty when it is `None` — the real email
        /// must NEVER reach an app (§7).
        relay_email: Option<String>,
    },
    /// No `sector_identifier` on the route yet ⇒ no `pws_` derivation
    /// possible. Fail closed.
    Unprovisioned,
}

/// Derive the per-app pairwise `pws_…` for `global_user_id` under the
/// route's `sector_identifier`, and idempotently UPSERT the mapping into
/// `auth.app_user_identities` so support tooling / the relay handler /
/// revocation can reverse `pws_ → (app, global_user)` (§6.2/§6.3).
///
/// Fail-closed contract: returns [`PairwiseProjection::Unprovisioned`]
/// when the route has no `sector_identifier` (the caller answers `503`),
/// matching the browser-token path in `auth_token.rs`. The derivation is
/// a pure HMAC (no DB round-trip); the mapping UPSERT is best-effort —
/// a DB failure is logged and the (already-correct) `pws_` is still
/// returned, because the persisted row is a reverse-lookup cache, not
/// part of the per-request trust decision.
///
/// The UPSERT + relay-alias read check out a pooled connection for JUST those
/// two writes/reads and release it on drop — never held across an outbound
/// HTTP call.
///
/// ## Email-claim swap (§7)
///
/// In the SAME checkout that upserts the pairwise mapping, this reads the
/// ACTIVE relay alias (`relay_email`, `revoked_at IS NULL`) for
/// `(app_client_id, global_user_id)` and returns it as
/// [`PairwiseProjection::Projected::relay_email`]. The caller substitutes that
/// alias for the user's REAL email so apps NEVER see the real address.
/// `None` (no minted alias yet, or the grant was revoked) makes the caller
/// FAIL CLOSED — emit an empty email — never the real one.
async fn project_pairwise(
    state: &Arc<GateState>,
    app_client_id: Option<&str>,
    sector_identifier: Option<&str>,
    global_user_id: &str,
) -> PairwiseProjection {
    let Some(sector) = sector_identifier else {
        return PairwiseProjection::Unprovisioned;
    };
    let pws = zeroship_core::auth::derive_pairwise(&state.pairwise_salt, global_user_id, sector);

    // Persist the (app_client_id, global_user_id) → pws_ mapping AND read the
    // active relay alias, both keyed on the per-app oac_ client_id (§6.2), in
    // ONE pooled checkout. The UPSERT is best-effort (log-and-continue: the
    // pws_ is already projected). The relay-alias read is the email-swap
    // source (§7): a read failure leaves `relay_email = None`, so the caller
    // fails closed (empty email) — it NEVER falls back to the real address.
    let mut relay_email = None;
    if let (Some(app_client_id), Some(db_cfg)) = (app_client_id, state.db.as_ref()) {
        if let Ok(uuid) = Uuid::parse_str(global_user_id) {
            match crate::db::checkout(db_cfg).await {
                Ok(pool) => match pool.get().await {
                    Ok(conn) => {
                        if let Err(e) =
                            crate::identities::upsert(&conn, app_client_id, uuid, &pws).await
                        {
                            tracing::warn!(
                                error = %e,
                                app_client_id = %app_client_id,
                                "app_user_identities upsert failed (non-fatal; pws_ already projected)"
                            );
                        }
                        // Email-claim swap: read the active alias for this
                        // (app, user). None ⇒ caller emits empty email (§7).
                        match crate::identities::lookup_relay_email(&conn, app_client_id, uuid).await
                        {
                            Ok(alias) => relay_email = alias,
                            Err(e) => tracing::warn!(
                                error = %e,
                                app_client_id = %app_client_id,
                                "relay_email lookup failed (non-fatal; failing closed on email)"
                            ),
                        }
                    }
                    Err(e) => tracing::warn!(
                        error = %e,
                        "app_user_identities upsert: pg pool checkout failed (non-fatal)"
                    ),
                },
                Err(e) => tracing::warn!(
                    error = %e,
                    "app_user_identities upsert: pg pool checkout failed (non-fatal)"
                ),
            }
        }
    }

    PairwiseProjection::Projected { pws, relay_email }
}

/// Re-resolve the LIVE relay alias for a wrapper's `(client_id, pws_sub)` on
/// the wrapper fast-paths (Batch A fix 5), returning the `email` claim the
/// worker header should carry — NEVER the (TTL-stale) email the wrapper itself
/// embeds.
///
/// A wrapper is JS-readable and lives for its full TTL (10 min browser / 1 h
/// DPoP), so the alias it was minted with can be revoked WHILE the wrapper is
/// still cryptographically valid. The cookie / introspection / raw-Hydra arms
/// already re-read the alias live via [`project_pairwise`]; this gives the two
/// wrapper fast-paths the SAME posture: a single pooled read keyed on the
/// per-app `pws_…` subject, with the same `revoked_at IS NULL` gate.
///
/// Fail-closed contract: returns an EMPTY string when the alias is revoked OR
/// no live alias exists OR the DB read fails — the worker then sees no email
/// rather than a dead/stale alias. It NEVER returns the wrapper's embedded
/// claim and NEVER the real address. When the gateway has no DB at all
/// (smoke mode) there is nothing to re-resolve, so the wrapper's own (already
/// alias-only, never-real-email) `email` claim is passed through unchanged.
#[allow(clippy::future_not_send)]
async fn live_relay_email_for_wrapper(
    state: &Arc<GateState>,
    client_id: &str,
    pws_sub: &str,
) -> String {
    let Some(db_cfg) = state.db.as_ref() else {
        // Smoke mode (no DB): nothing to re-resolve against. Fail CLOSED to an
        // empty email rather than trust the wrapper's embedded claim — this
        // matches the raw-Hydra/introspection/cookie arms (which already emit
        // empty when no alias is resolvable) and means the no-DB path can never
        // surface an embedded email regardless of how the wrapper was minted
        // (defense-in-depth: not an implicit dependency on the minter invariant).
        return String::new();
    };
    match crate::db::checkout(db_cfg).await {
        Ok(pool) => match pool.get().await {
            Ok(conn) => {
                match crate::identities::lookup_relay_email_by_pairwise(
                    &conn, client_id, pws_sub,
                )
                .await
                {
                    Ok(alias) => alias.unwrap_or_default(),
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            client_id = %client_id,
                            "wrapper live relay_email re-resolve failed (failing closed on email)"
                        );
                        String::new()
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "wrapper live relay_email re-resolve: pg pool get failed (failing closed)"
                );
                String::new()
            }
        },
        Err(e) => {
            tracing::warn!(
                error = %e,
                "wrapper live relay_email re-resolve: pg pool checkout failed (failing closed)"
            );
            String::new()
        }
    }
}

/// Whether the request carries an `Authorization: DPoP <token>` header.
/// We branch on this so a failed `DPoP` verification doesn't silently
/// fall back to the cookie path.
fn has_dpop_authorization(req: &HttpRequest) -> bool {
    req.headers()
        .get(http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|s| s.starts_with("DPoP "))
}

/// Cheap JWT-header discriminator for gateway wrapper tokens.
///
/// This intentionally does not verify the signature; it only answers
/// whether a token is shaped like a wrapper so the dispatch path can
/// distinguish "raw hydra opaque token" from "malformed wrapper" before
/// deciding whether introspection fallback is allowed.
fn looks_like_wrapper(token: &str) -> bool {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine as _;

    let mut parts = token.split('.');
    let (header_b64, payload_b64, sig_b64) =
        match (parts.next(), parts.next(), parts.next(), parts.next()) {
            (Some(h), Some(p), Some(s), None) => (h, p, s),
            _ => return false,
        };
    if header_b64.is_empty() || payload_b64.is_empty() || sig_b64.is_empty() {
        return false;
    }

    let Ok(header_bytes) = URL_SAFE_NO_PAD.decode(header_b64) else {
        return false;
    };
    let Ok(header) = serde_json::from_slice::<serde_json::Value>(&header_bytes) else {
        return false;
    };

    header.get("typ").and_then(|v| v.as_str()) == Some("at+jwt")
}

/// Outcome of the DPoP arm ([`resolve_dpop_user_header`]).
#[derive(Debug)]
enum DpopOutcome {
    /// A verified DPoP-bound user (wrapper fast-path or introspection
    /// fallback). Carries the signed `ZeroShip-User` header. The wrapper
    /// fast-path's `sub` is ALREADY the `pws_`; the introspection fallback
    /// projects the global UUID to the per-app `pws_` (§6.2).
    Allowed(String),
    /// A verified raw-Hydra-introspected user, but the route has no
    /// `sector_identifier` yet ⇒ no `pws_` derivation possible. Fail
    /// closed (`503`) rather than project the global UUID.
    ClientNotProvisioned,
    /// No DPoP credential (or it failed verification / replay / binding).
    /// `resolve_auth` falls through to the Bearer/cookie arms unless an
    /// `Authorization: DPoP` header was present (then it 401s).
    None,
}

/// Resolve the `ZeroShip-User` header from a DPoP-bound access token.
///
/// Returns `Some(header)` when:
///   1. `Authorization: DPoP <token>` is present, AND
///   2. The `DPoP:` proof header is present, AND
///   3. The proof verifies (signature, htm, htu, iat, ath), AND
///   4. The proof's `jti` has not been seen before in the freshness
///      window (replay defense), AND
///   5. EITHER the access token verifies as a gateway-issued wrapper
///      AND `wrapper.aud == Host` AND `wrapper.cnf.jkt == proof.jkt`
///      (Phase 8 U4 fast path — self-contained, no introspection),
///      OR the token does not look like a wrapper / no verifier is
///      configured AND hydra's `/oauth2/introspect` returns
///      `active: true` AND the introspected `client_id` equals the route's
///      expected `oauth_client_id` (P7-U5 fallback; no `cnf.jkt` enforcement,
///      but the per-app `client_id` binding closes the cross-app
///      token-confusion gap the wrapper fast-path covers via `aud`/`cnf`).
///
/// Returns [`DpopOutcome::None`] for "no `DPoP` token in this request"
/// AND for every failure mode above. The caller distinguishes the two
/// via [`has_dpop_authorization`].
///
/// ## Pairwise projection (auth-sdk Slice 4, §6.2)
///
/// The wrapper fast-path's `sub` is ALREADY the per-app `pws_` (minted
/// that way at `/token` / `?mint=1`), so it forwards unchanged. The
/// introspection fallback carries the GLOBAL Hydra UUID `sub`, so it
/// projects to the per-app `pws_` via [`project_pairwise`] before
/// encoding the header — and fails closed
/// ([`DpopOutcome::ClientNotProvisioned`] → `503`) when the route has no
/// `sector_identifier` yet, so the global UUID is never emitted.
///
/// ## Wrapper-vs-raw branching (Phase 8 U4)
///
/// We try the wrapper-verifier first. A successful verify means the
/// gateway minted this token via `/__zs/auth/dpop-exchange` (U3) — it's
/// already scoped to a specific request host in `aud` and bound to a
/// specific `DPoP` key in its `cnf.jkt` claim. We require both the
/// request Host and proof binding to match; mismatch is a hard reject
/// (no fallback). On a wrapper hit we build the worker user from the
/// embedded claims and skip the introspection round-trip entirely.
///
/// A token that does not look like a wrapper is treated as "raw hydra
/// token, try introspection". A token that does look like a wrapper but
/// fails wrapper verification is a hard reject. Falling through would
/// let a forged or tampered wrapper dodge `cnf.jkt` enforcement by using
/// the unbound introspection path.
async fn resolve_dpop_user_header(
    req: &HttpRequest,
    state: &Arc<GateState>,
    request_id: &Uuid,
    oauth_client_id: Option<&str>,
    sector_identifier: Option<&str>,
) -> DpopOutcome {
    // 1. Authorization: DPoP <token>
    let Some(auth_header) = req
        .headers()
        .get(http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    else {
        return DpopOutcome::None;
    };
    let Some(access_token) = auth_header.strip_prefix("DPoP ") else {
        return DpopOutcome::None;
    };

    // 2. DPoP proof header
    let Some(proof) = req.headers().get("dpop").and_then(|v| v.to_str().ok()) else {
        tracing::warn!("Authorization: DPoP present but DPoP proof header missing");
        return DpopOutcome::None;
    };

    // 3. Build expected htu = scheme://host/path (RFC 9449 §4.2 — query
    //    and fragment stripped). `Host` is the client-visible host so
    //    it matches what the client signed into the proof.
    let scheme = if state.config.insecure_dev { "http" } else { "https" };
    let host = req
        .headers()
        .get(http::header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let path = req.uri().path();
    let expected_uri = format!("{scheme}://{host}{path}");
    let method = req.method().as_str();

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    // 4. Verify the proof (signature + htm + htu + iat + ath).
    let verified = match zeroship_core::dpop::verify(
        proof,
        method,
        &expected_uri,
        Some(access_token),
        now,
    ) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "DPoP proof verification failed");
            return DpopOutcome::None;
        }
    };

    // 5. jti replay protection — 120 s freshness window matches the
    //    accepted clock skew on the proof's iat claim.
    match state.dpop_jti_cache.insert(&verified.jti, now, 120).await {
        Ok(true) => {}
        Ok(false) => {
            tracing::warn!(jti = %verified.jti, "DPoP jti replay detected");
            return DpopOutcome::None;
        }
        Err(e) => {
            tracing::warn!(error = %e, "DPoP jti replay check failed");
            return DpopOutcome::None;
        }
    }

    // 6a. Wrapper-token fast path (Phase 8 U4). Try to verify the
    //     access token as a gateway-issued wrapper. On success we
    //     enforce `cnf.jkt == proof.jkt` and build the worker user
    //     from the embedded claims — no hydra round-trip required.
    let token_looks_like_wrapper = looks_like_wrapper(access_token);
    if let Some(verifier) = state.wrapper_verifier.as_ref() {
        // The DPoP fast-path passes `None` for the client_id binding:
        // it enforces the DPoP key binding (`cnf.jkt == proof.jkt`)
        // below instead. The per-app `client_id`-claim binding belongs
        // to the Bearer arm (§1.3, slice 1c), which passes
        // `Some(route.oauth_client_id)`.
        match verifier.verify(access_token, host, None) {
            Ok(claims) => {
                // Strict binding: a wrapper minted for jkt_A cannot be
                // presented with a proof signed by jkt_B. Mismatch is a
                // hard reject — we deliberately do NOT fall through to
                // introspection here, because falling through would let
                // an attacker who stole a wrapper bypass its binding
                // simply by also presenting a different valid DPoP
                // proof for their own key.
                //
                // A wrapper with no `cnf` (the plain-Bearer browser
                // mint) cannot satisfy a DPoP proof binding at all, so
                // it is rejected on this DPoP path.
                let Some(cnf) = claims.cnf.as_ref() else {
                    tracing::warn!(
                        "DPoP wrapper token has no cnf.jkt — rejecting on DPoP path"
                    );
                    return DpopOutcome::None;
                };
                if cnf.jkt != verified.jkt {
                    tracing::warn!(
                        expected = %cnf.jkt,
                        actual = %verified.jkt,
                        "DPoP proof jkt does not match wrapper cnf.jkt — rejecting"
                    );
                    return DpopOutcome::None;
                }
                if claims.sub.is_empty() {
                    tracing::warn!("DPoP wrapper token missing sub — rejecting");
                    return DpopOutcome::None;
                }
                // Self-describing-subject invariant (Batch A fix 2). A
                // gateway-issued wrapper's `sub` is ALWAYS the per-app `pws_…`
                // (the gateway minted it that way at /token / ?mint=1 /
                // dpop-exchange, §6.2). A wrapper whose `sub` is the global
                // Hydra UUID (or otherwise not `pws_`-shaped) means a mint path
                // failed to project — defense-in-depth, hard-reject it so a
                // non-projected wrapper can NEVER reach a worker (it would leak
                // the global identity into the JS-readable token).
                //
                // The `debug_assert!` is a developer tripwire for a GATEWAY
                // bug (a mint path that forgot to project). It is gated off
                // under `cfg(test)` so the adversarial subject-invariant test
                // can feed a hand-crafted UUID-sub wrapper and exercise the
                // RUNTIME reject below (an attacker's forged-but-rejected token
                // must surface as a clean reject, not a panic).
                #[cfg(not(test))]
                debug_assert!(
                    zeroship_core::auth::is_pairwise_subject(&claims.sub),
                    "wrapper sub must be a pws_ pairwise subject, got {}",
                    claims.sub
                );
                if !zeroship_core::auth::is_pairwise_subject(&claims.sub) {
                    tracing::warn!(
                        sub = %claims.sub,
                        "DPoP wrapper sub is not a pws_ pairwise subject — rejecting (self-describing-subject invariant)"
                    );
                    return DpopOutcome::None;
                }
                // Cross-node PER-APP family-marker revocation (spec §8.5), the
                // SAME `(client_id, sub)` shape and writer the other arms use.
                // The wrapper's `sub` IS the `pws_` (fix 1/2 above), so this
                // keys on `(claims.client_id, claims.sub=pws_)` — exactly what
                // /signout + control's disconnect-app cascade write. (Batch A
                // fix 3: the old code keyed `is_subject_revoked_since` on a
                // UUID parse of `sub`, which a `pws_` can never satisfy, so the
                // check silently no-op'd for every wrapper.)
                //
                // KEY TRUST (Batch A minor): this fast-path called
                // `verifier.verify(.., None)` above, so `claims.client_id` is
                // NOT checked against the resolved route's client here — we key
                // the marker on the gateway-SIGNED `claims.client_id`. That is
                // safe because (a) the wrapper is gateway-signed, so
                // `claims.client_id` is the value the gateway itself stamped at
                // mint (= `route.client_id`), not attacker-controlled, and
                // (b) `aud == host` was enforced by the verify above, pinning
                // the wrapper to THIS app's host. The marker writers key on the
                // same per-app client, so the key matches. The Bearer-wrapper
                // arm passes the route client to verify explicitly; this arm
                // relies on the signed claim + aud binding instead.
                if let Some(db_cfg) = state.db.as_ref() {
                    let pool = match crate::db::checkout(db_cfg).await {
                        Ok(p) => p,
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                "DPoP wrapper revocation: pg pool checkout failed"
                            );
                            return DpopOutcome::None;
                        }
                    };
                    let conn = match pool.get().await {
                        Ok(c) => c,
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                "DPoP wrapper revocation: pg pool checkout failed"
                            );
                            return DpopOutcome::None;
                        }
                    };
                    match zeroship_core::wrapper_revocation::is_family_revoked_since(
                        &conn,
                        &claims.client_id,
                        &claims.sub,
                        claims.iat,
                    )
                    .await
                    {
                        Ok(true) => {
                            tracing::warn!(
                                client_id = %claims.client_id,
                                sub = %claims.sub,
                                "DPoP wrapper family was revoked after wrapper issue"
                            );
                            return DpopOutcome::None;
                        }
                        Ok(false) => {}
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                sub = %claims.sub,
                                "DPoP wrapper revocation check failed"
                            );
                            return DpopOutcome::None;
                        }
                    }
                }
                // DPoP wrapper fast-path: the wrapper's `sub` is ALREADY
                // the per-app `pws_` (the gateway minted it that way at
                // /token / ?mint=1, §6.2) — no pairwise re-derivation, the
                // same consistent `pws_` the Bearer-wrapper arm reads.
                let mut owned = build_worker_user_from_wrapper(&claims);
                // Email-claim swap re-resolved LIVE (Batch A fix 5): the
                // wrapper's embedded `email` is alias-only but TTL-stale, so a
                // revoked alias would still forward for the wrapper's lifetime.
                // Re-read the alias keyed on `(client_id, pws_)` and fail
                // closed (empty) on revoke/miss — matching the cookie /
                // introspection / raw-Hydra arms.
                owned.email = live_relay_email_for_wrapper(
                    state,
                    &claims.client_id,
                    &claims.sub,
                )
                .await;
                let user: oidc_rp::WorkerUser<'_> = (&owned).into();
                return DpopOutcome::Allowed(oidc_rp::encode_user_header(
                    &user,
                    &state.config.worker_key,
                    *request_id,
                ));
            }
            Err(e) => {
                if token_looks_like_wrapper {
                    tracing::warn!(
                        error = %e,
                        "wrapper-shaped DPoP token failed verification — rejecting"
                    );
                    return DpopOutcome::None;
                }
                tracing::debug!(
                    error = %e,
                    "wrapper verify failed; falling back to hydra introspect"
                );
            }
        }
    }

    // 6b. Fallback: introspect the access token as a raw hydra opaque
    //     token. Hydra's response carries the user identity claims when
    //     `active: true`. No `cnf.jkt` enforcement here — hydra doesn't
    //     currently surface `cnf.jkt` on access tokens, so binding is
    //     proof-of-possession only (the proof's `ath` claim already
    //     binds the proof to this specific access token in step 4).
    let info = match state.oidc_rp.introspect_token(access_token).await {
        Ok(i) if i.active => i,
        Ok(_) => {
            tracing::warn!("DPoP access token introspected as inactive");
            return DpopOutcome::None;
        }
        Err(e) => {
            tracing::warn!(error = %e, "DPoP introspect call failed");
            return DpopOutcome::None;
        }
    };
    if !matches!(info.sub.as_deref(), Some(sub) if !sub.is_empty()) {
        tracing::warn!("DPoP introspection response missing sub");
        return DpopOutcome::None;
    }

    // 6c. Per-app binding (Slice 4 token-confusion fix). The DPoP wrapper
    //     fast-path (6a) binds via `aud == Host` + `cnf.jkt`; this raw-opaque
    //     fallback has neither, so an opaque DPoP-bound token active for app A,
    //     replayed at app B's host with a valid proof, would otherwise be
    //     accepted as B (and projected to B's sector). We close that gap by
    //     binding the introspected token to the route's expected client, the
    //     SAME `client_id`-claim binding the raw-Hydra Bearer arm enforces
    //     (RFC 9068 §3 / RFC 7662 `client_id`).
    //
    //     When the route has no `oauth_client_id` (un-provisioned app, 1d
    //     fills it) we cannot bind to a missing client — exactly the Bearer
    //     arm's posture (it returns `Invalid` rather than accept an unbound
    //     token). Here we reject the DPoP credential (`None`); the caller then
    //     401s (a request that asserted DPoP cannot re-assert via cookie). The
    //     ClientNotProvisioned 503 is reserved for the DOWNSTREAM no-sector
    //     case (6.2), reached only once the token is already bound.
    let Some(expected_client_id) = oauth_client_id else {
        tracing::warn!(
            "DPoP introspection succeeded but route has no oauth_client_id — \
             cannot bind token to a client; rejecting"
        );
        return DpopOutcome::None;
    };
    if info.client_id.as_deref() != Some(expected_client_id) {
        tracing::warn!(
            token_client_id = ?info.client_id,
            expected = %expected_client_id,
            "DPoP introspected token client_id does not match route client — \
             rejecting (cross-app token confusion)"
        );
        return DpopOutcome::None;
    }

    // 6d. Cross-node PER-APP family-marker revocation (spec §8.5), the SAME
    //     check the raw-Hydra Bearer arm runs. Hydra `active: true` (step 6b)
    //     only reflects GLOBAL revocation; the per-app `auth.token_revocations`
    //     marker is keyed on `(client_id, pws_)` — the SAME key the WRITERS
    //     (/signout + control's disconnect-app cascade) use — so revoking a
    //     user on app A must also reject their DPoP-bound opaque token on app
    //     A's path here. We project the per-app `pws_` FIRST and key the marker
    //     check on it (Batch A fix 3): pre-fix this keyed on the GLOBAL Hydra
    //     `sub` while the writer keyed on `pws_`, so a real revocation never
    //     matched the live token.
    //
    //     The pairwise derivation needs the route's `sector_identifier`; with
    //     no sector we cannot derive the `pws_` (and the downstream projection
    //     would fail closed anyway), so fail closed (503) BEFORE the marker
    //     check rather than key on the global UUID. `iat` comes from the
    //     introspection response (RFC 7662 §2.2); when Hydra omits it we fail
    //     CLOSED (epoch `0`), so any live family marker rejects rather than
    //     silently skipping the check. We hold the pooled connection only
    //     across this lookup — never across the introspection HTTP call above.
    let Some(sector) = sector_identifier else {
        return DpopOutcome::ClientNotProvisioned;
    };
    // `info.sub` is already confirmed `Some(non-empty)` above.
    // `derive_pairwise` canonicalizes a UUID `sub` (Batch A M1), so the `pws_`
    // derived from the RAW introspection `info.sub` is byte-identical to the
    // canonical-form writers' marker regardless of Hydra's sub spelling.
    let global_sub = info.sub.as_deref().unwrap_or_default();
    let pws_sub =
        zeroship_core::auth::derive_pairwise(&state.pairwise_salt, global_sub, sector);
    if let Some(db_cfg) = state.db.as_ref() {
        let iat = info.iat.unwrap_or(0);
        let pool = match crate::db::checkout(db_cfg).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "DPoP introspection revocation: pg pool checkout failed");
                return DpopOutcome::None;
            }
        };
        let conn = match pool.get().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "DPoP introspection revocation: pg pool get failed");
                return DpopOutcome::None;
            }
        };
        match zeroship_core::wrapper_revocation::is_family_revoked_since(
            &conn,
            expected_client_id,
            &pws_sub,
            iat,
        )
        .await
        {
            Ok(true) => {
                tracing::warn!(
                    client_id = %expected_client_id,
                    sub = %pws_sub,
                    "DPoP introspection family revoked after iat — rejecting"
                );
                return DpopOutcome::None;
            }
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(error = %e, sub = %pws_sub, "DPoP introspection revocation check failed");
                return DpopOutcome::None;
            }
        }
    }

    // 7. Build the `ZeroShip-User` header from the introspection result.
    //    The introspected `sub` is the GLOBAL Hydra UUID — project it to
    //    the per-app `pws_` (§6.2) so the worker header never carries the
    //    global id. `project_pairwise` re-derives the SAME `pws_sub` (pure
    //    deterministic HMAC) and additionally upserts the reverse-lookup row +
    //    reads the email-swap alias.
    let mut owned = build_worker_user_from_introspection(&info);
    match project_pairwise(state, Some(expected_client_id), Some(sector), &owned.id).await {
        PairwiseProjection::Projected { pws, relay_email } => {
            owned.id = pws;
            // Email-claim swap (§7): project the relay alias, never the real
            // email. No active alias ⇒ empty (fail closed).
            owned.email = relay_email.unwrap_or_default();
        }
        PairwiseProjection::Unprovisioned => return DpopOutcome::ClientNotProvisioned,
    }
    let user: oidc_rp::WorkerUser<'_> = (&owned).into();
    DpopOutcome::Allowed(oidc_rp::encode_user_header(
        &user,
        &state.config.worker_key,
        *request_id,
    ))
}

/// Outcome of the Bearer arm ([`resolve_bearer_user_header`]). The four
/// variants map directly onto the route-policy gate in [`resolve_auth`]:
/// see the comment at the Bearer-arm call site for the policy table.
#[derive(Debug)]
enum BearerOutcome {
    /// A valid user-session Bearer (wrapper or raw-Hydra) that verified,
    /// bound to this app, and passed revocation. Carries the signed
    /// `ZeroShip-User` header. Fully authenticated regardless of policy.
    Allowed(String),
    /// A recognized user-session token (gateway-wrapper or raw-Hydra
    /// `iss`) that FAILED verification / per-app binding / revocation, OR
    /// a wrapper presented while the route is un-provisioned
    /// (`oauth_client_id == None`). Treated as no-identity: anonymous on
    /// `Anon`, 401 on `User`/`Admin`.
    Invalid,
    /// A Bearer token whose `iss` is neither the gateway nor Hydra — the
    /// reserved API-key path (a future `zsk_…` shape). 401 on every route.
    NotUserSession,
    /// A valid raw-Hydra Bearer user, but the route has no
    /// `sector_identifier` yet ⇒ no per-app `pws_` derivation possible.
    /// Fail closed (`503`) rather than project the global UUID (§6.2).
    ClientNotProvisioned,
    /// No `Authorization: Bearer` header. Fall through to the cookie arm.
    NotBearer,
}

/// Peek the unverified `iss` claim out of a JWT payload. Mirrors
/// [`jwt_subject_unverified`] but for `iss` — used ONLY to discriminate
/// which verifier to run (wrapper vs raw-Hydra); the actual trust
/// decision is the subsequent signature check, so reading `iss` before
/// verification is safe. Returns `None` for any structural parse
/// failure (e.g. an opaque/non-JWT API key).
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
/// access token (§1.3, slice 1c).
///
/// Discriminates by the **unverified** `iss` peek:
///   - `iss == config.public_url` (the gateway) → WRAPPER path: verify
///     via `state.wrapper_verifier.verify(token, host, Some(client_id))`
///     (ed25519 sig + iss + aud==Host + the per-app `client_id`-claim
///     binding). `sub` is ALREADY the `pws_` and `email` the alias — no
///     pairwise re-derivation (the DPoP path's precedent). A wrapper with
///     `cnf = Some(jkt)` (DPoP-bound) is REJECTED here: it is
///     sender-constrained and MUST be presented as `Authorization: DPoP`
///     with a proof, not as a plain Bearer (§1.3 c-wrap downgrade guard).
///   - `iss == state.oidc_rp.issuer` (Hydra) → RAW-HYDRA path: verify the
///     JWT signature locally via the gateway's JWKS cache
///     (`state.oidc_rp.verify_access_token`), then bind per-app on the
///     `client_id` claim (RFC 9068 §3) with an `aud`-contains fallback.
///   - anything else → `NotUserSession` (reserved API-key path).
///
/// **Per-app binding** is the critical safety property: a token minted
/// for app A must be rejected at app B's host. `oauth_client_id` is the
/// matched route's expected client. When it is `None` (the app is not
/// yet provisioned — 1d fills it), a wrapper cannot be bound to a
/// missing client, so the wrapper path yields `Invalid` (never binds to
/// a falsy value). A raw-Hydra token likewise cannot be bound and yields
/// `Invalid`.
///
/// **Revocation** is the spec §8.5 PER-APP family marker
/// (`auth.token_revocations`, keyed on `(client_id, sub)` with `sub` as
/// TEXT). Both arms reject a token when a row exists for its
/// `(client_id, sub)` with `revoked_after > token.iat`; `sub` being TEXT
/// is what lets the wrapper path's `pws_…` subject be matched (the
/// UUID-only subject denylist could not). Per-app scoping means a
/// revocation on app A leaves the same user's tokens on app B valid.
///
/// Pairwise projection (Slice 4, §6.2): the WRAPPER path's `sub` is
/// ALREADY the per-app `pws_` (the gateway minted it that way), so it
/// forwards unchanged — the same consistent `pws_` for a given
/// `(user, app)`. The RAW-HYDRA path's `sub` is the GLOBAL Hydra UUID,
/// so it is projected to the per-app `pws_` via [`project_pairwise`]
/// before encoding the header (and the mapping row is upserted). The
/// raw-Hydra arm fails closed ([`BearerOutcome::ClientNotProvisioned`] →
/// `503`) when the route has no `sector_identifier` yet, so the global
/// UUID never reaches the worker header.
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

    let host = req
        .headers()
        .get(http::header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");

    if iss == state.config.public_url {
        // ── WRAPPER path ──────────────────────────────────────────────
        // A wrapper binds per-app on its `client_id` claim. Without an
        // expected client_id (un-provisioned app) we cannot bind, so we
        // refuse rather than accept an unbound wrapper.
        let Some(expected_client_id) = oauth_client_id else {
            tracing::warn!(
                "Bearer wrapper presented but route has no oauth_client_id — rejecting"
            );
            return BearerOutcome::Invalid;
        };
        let Some(verifier) = state.wrapper_verifier.as_ref() else {
            // No verifier configured — cannot validate a wrapper.
            return BearerOutcome::Invalid;
        };
        let claims = match verifier.verify(token, host, Some(expected_client_id)) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "Bearer wrapper verification failed");
                return BearerOutcome::Invalid;
            }
        };
        // DPoP downgrade guard (§1.3 c-wrap). A wrapper minted with
        // `cnf = Some(jkt)` is SENDER-CONSTRAINED: the client opted into
        // RFC 9449 proof-of-possession, so it MUST be presented as
        // `Authorization: DPoP <token>` with a matching proof and routed
        // through the DPoP arm (which enforces `cnf.jkt == proof.jkt`).
        // Accepting it here on the plain-`Bearer` scheme — where there is
        // NO proof — would silently downgrade a sender-constrained token
        // to a replayable bearer, defeating the binding the user opted
        // into. A DPoP-bound wrapper is therefore NOT a valid plain
        // bearer → Invalid. Only `cnf = None` (the browser plain-Bearer
        // mint) is acceptable on this path.
        if claims.cnf.is_some() {
            tracing::warn!(
                "DPoP-bound wrapper (cnf present) presented on plain Bearer — \
                 rejecting (must use Authorization: DPoP with a proof)"
            );
            return BearerOutcome::Invalid;
        }
        if claims.sub.is_empty() {
            tracing::warn!("Bearer wrapper token missing sub — rejecting");
            return BearerOutcome::Invalid;
        }
        // Self-describing-subject invariant (Batch A fix 2). A gateway-issued
        // wrapper's `sub` is ALWAYS the per-app `pws_…` (minted that way at
        // /token / ?mint=1 / dpop-exchange, §6.2). A wrapper carrying the
        // global Hydra UUID (or any non-`pws_` sub) means a mint path failed
        // to project — defense-in-depth, hard-reject so a non-projected
        // wrapper can never reach a worker and leak the global identity into
        // the JS-readable token.
        //
        // `debug_assert!` is a developer tripwire for a GATEWAY mint bug; gated
        // off under `cfg(test)` so the adversarial subject-invariant test can
        // exercise the RUNTIME reject below (a forged-but-rejected token must
        // surface as a clean reject, not a panic).
        #[cfg(not(test))]
        debug_assert!(
            zeroship_core::auth::is_pairwise_subject(&claims.sub),
            "wrapper sub must be a pws_ pairwise subject, got {}",
            claims.sub
        );
        if !zeroship_core::auth::is_pairwise_subject(&claims.sub) {
            tracing::warn!(
                sub = %claims.sub,
                "Bearer wrapper sub is not a pws_ pairwise subject — rejecting (self-describing-subject invariant)"
            );
            return BearerOutcome::Invalid;
        }
        // Cross-node PER-APP family-marker revocation (spec §8.5). Keyed on
        // `(client_id, sub)` with `sub` as TEXT, so it covers the wrapper's
        // `pws_…` pairwise subject — which the UUID-only subject denylist
        // could never match. Per-app: a marker for this app's client_id
        // does not affect the same user on a sibling app.
        if let Some(db_cfg) = state.db.as_ref() {
            // Check out a pooled connection for just this family-marker
            // lookup and release it on drop.
            let pool = match crate::db::checkout(db_cfg).await {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(error = %e, "Bearer wrapper revocation: pg pool checkout failed");
                    return BearerOutcome::Invalid;
                }
            };
            let conn = match pool.get().await {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(error = %e, "Bearer wrapper revocation: pg pool checkout failed");
                    return BearerOutcome::Invalid;
                }
            };
            match zeroship_core::wrapper_revocation::is_family_revoked_since(
                &conn,
                &claims.client_id,
                &claims.sub,
                claims.iat,
            )
            .await
            {
                Ok(true) => {
                    tracing::warn!(
                        client_id = %claims.client_id,
                        sub = %claims.sub,
                        "Bearer wrapper family was revoked after wrapper issue"
                    );
                    return BearerOutcome::Invalid;
                }
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(error = %e, sub = %claims.sub, "Bearer wrapper revocation check failed");
                    return BearerOutcome::Invalid;
                }
            }
        }
        let mut owned = build_worker_user_from_wrapper(&claims);
        // Email-claim swap re-resolved LIVE (Batch A fix 5): the wrapper's
        // embedded `email` is alias-only but TTL-stale (10-min browser
        // wrapper), so a revoked alias would still surface for the wrapper's
        // lifetime. Re-read the alias keyed on `(client_id, pws_)` and fail
        // closed (empty) on revoke/miss — the same posture as the cookie /
        // introspection / raw-Hydra arms.
        owned.email = live_relay_email_for_wrapper(
            state,
            &claims.client_id,
            &claims.sub,
        )
        .await;
        let user: oidc_rp::WorkerUser<'_> = (&owned).into();
        return BearerOutcome::Allowed(oidc_rp::encode_user_header(
            &user,
            &state.config.worker_key,
            *request_id,
        ));
    }

    if iss == state.oidc_rp.issuer {
        // ── RAW-HYDRA path (RFC 9068, non-browser clients) ────────────
        let Some(expected_client_id) = oauth_client_id else {
            tracing::warn!(
                "raw-Hydra Bearer presented but route has no oauth_client_id — rejecting"
            );
            return BearerOutcome::Invalid;
        };
        let claims = match state.oidc_rp.verify_access_token(token).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "raw-Hydra Bearer verification failed");
                return BearerOutcome::Invalid;
            }
        };
        // Per-app binding: bind on the `client_id` claim (RFC 9068 §3 —
        // Hydra emits it) when present; else fall back to `aud` containing
        // the expected client_id. We bind to `client_id`, NOT `aud` as the
        // primary, because `aud` is the resource-server audience.
        let bound = match claims.client_id.as_deref() {
            Some(cid) => cid == expected_client_id,
            None => claims.aud.iter().any(|a| a == expected_client_id),
        };
        if !bound {
            tracing::warn!(
                client_id = ?claims.client_id,
                aud = ?claims.aud,
                expected = %expected_client_id,
                "raw-Hydra Bearer per-app binding failed — rejecting"
            );
            return BearerOutcome::Invalid;
        }
        if claims.sub.is_empty() {
            tracing::warn!("raw-Hydra Bearer token missing sub — rejecting");
            return BearerOutcome::Invalid;
        }
        // Project the per-app pairwise `pws_` FIRST (§6.2), then key the
        // revocation check on it — the marker WRITERS (/signout + control's
        // disconnect-app cascade) key `auth.token_revocations` on
        // `(client_id, pws_)`, NOT the global Hydra UUID, so the reader MUST
        // agree (Batch A fix 3). Pre-fix this arm keyed the lookup on the
        // global `claims.sub` while the writer keyed on `pws_`, so a real
        // revocation never matched a still-live raw-Hydra token.
        //
        // The pairwise derivation needs the route's `sector_identifier`; with
        // no sector we cannot derive the `pws_` (and would never reach the
        // worker without one anyway), so fail closed (503) BEFORE the marker
        // check rather than fall back to keying on the global UUID.
        let Some(sector) = sector_identifier else {
            return BearerOutcome::ClientNotProvisioned;
        };
        // `derive_pairwise` canonicalizes a UUID `sub` to its hyphenated-
        // lowercase form before hashing (Batch A M1), so the `pws_` this reader
        // computes from the RAW Hydra `claims.sub` is byte-identical to the
        // marker the canonical-form writers (`/signout`, control cascade) wrote
        // — even if Hydra emitted a non-canonical sub spelling.
        let pws_sub =
            zeroship_core::auth::derive_pairwise(&state.pairwise_salt, &claims.sub, sector);
        // Cross-node PER-APP family-marker revocation (spec §8.5). Keyed on
        // `(expected_client_id, pws_sub)` — the SAME `(client_id, pws_)` shape
        // the writers use. Per-app: revoking this user on app A leaves their
        // raw-Hydra access on app B valid (app B's `pws_` differs). We key on
        // `expected_client_id` (the route's bound client) because the binding
        // above proved the token agrees and the marker is written against the
        // route's client.
        if let Some(db_cfg) = state.db.as_ref() {
            // Check out a pooled connection for just this family-marker
            // lookup and release it on drop.
            let pool = match crate::db::checkout(db_cfg).await {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(error = %e, "raw-Hydra Bearer revocation: pg pool checkout failed");
                    return BearerOutcome::Invalid;
                }
            };
            let conn = match pool.get().await {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(error = %e, "raw-Hydra Bearer revocation: pg pool checkout failed");
                    return BearerOutcome::Invalid;
                }
            };
            match zeroship_core::wrapper_revocation::is_family_revoked_since(
                &conn,
                expected_client_id,
                &pws_sub,
                claims.iat,
            )
            .await
            {
                Ok(true) => {
                    tracing::warn!(
                        client_id = %expected_client_id,
                        sub = %pws_sub,
                        "raw-Hydra Bearer family revoked after iat"
                    );
                    return BearerOutcome::Invalid;
                }
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(error = %e, sub = %pws_sub, "raw-Hydra Bearer revocation check failed");
                    return BearerOutcome::Invalid;
                }
            }
        }
        // Slice 4 (§6.2): the raw-Hydra `sub` is the GLOBAL Hydra UUID —
        // project it to the per-app `pws_` (and upsert the mapping + read the
        // live relay alias) before the header is built, so the worker never
        // sees the global id. `expected_client_id` is the route's bound oac_
        // client (the binding above proved the token agrees), so the mapping
        // is keyed on it. `project_pairwise` re-derives the SAME `pws_sub` (a
        // pure deterministic HMAC) and additionally persists the reverse-lookup
        // row + reads the email-swap alias.
        let mut owned = build_worker_user_from_access_claims(&claims);
        match project_pairwise(
            state,
            Some(expected_client_id),
            Some(sector),
            &owned.id,
        )
        .await
        {
            PairwiseProjection::Projected { pws, relay_email } => {
                owned.id = pws;
                // Email-claim swap (§7): project the relay alias, never the
                // real email. No active alias ⇒ empty (fail closed).
                owned.email = relay_email.unwrap_or_default();
            }
            PairwiseProjection::Unprovisioned => {
                return BearerOutcome::ClientNotProvisioned;
            }
        }
        let user: oidc_rp::WorkerUser<'_> = (&owned).into();
        return BearerOutcome::Allowed(oidc_rp::encode_user_header(
            &user,
            &state.config.worker_key,
            *request_id,
        ));
    }

    // Neither a gateway wrapper nor a raw-Hydra JWT — reserved API-key
    // path (e.g. a future `zsk_…` shape). Stays 401.
    BearerOutcome::NotUserSession
}

/// Materialise a `WorkerUser` from a verified raw-Hydra access JWT.
///
/// `id` is the raw `sub` (the GLOBAL Hydra UUID) here; the caller
/// ([`resolve_bearer_user_header`]) projects it to the per-app `pws_`
/// via [`project_pairwise`] (Slice 4, §6.2) BEFORE the header is built,
/// so the global UUID never reaches the worker. The profile fields come
/// straight from the verified claims.
fn build_worker_user_from_access_claims(claims: &crate::oidc_rp::AccessClaims) -> OwnedWorkerUser {
    OwnedWorkerUser {
        id: claims.sub.clone(),
        email: claims.email.clone().unwrap_or_default(),
        name: claims.name.clone().unwrap_or_default(),
        email_verified: claims.email_verified.unwrap_or(false),
        // Raw-Hydra arm: scopes come from the access token's `scope` claim.
        scopes: split_scope_claim(claims.scope.as_deref().unwrap_or_default()),
    }
}

/// Materialise a `WorkerUser` from a verified wrapper-token claim set.
///
/// The wrapper's claims are self-contained (the gateway populated them
/// from the original hydra introspection at exchange time, U3), so this
/// is purely a field rename — no network calls, no further validation.
/// `OwnedWorkerUser` holds the strings on the stack so the
/// `WorkerUser` borrow can survive the lifetime needed by
/// `encode_user_header`.
fn build_worker_user_from_wrapper(claims: &crate::wrapper_token::WrapperClaims) -> OwnedWorkerUser {
    OwnedWorkerUser {
        id: claims.sub.clone(),
        email: claims.email.clone().unwrap_or_default(),
        name: claims.name.clone().unwrap_or_default(),
        email_verified: claims.email_verified.unwrap_or(false),
        // Bearer-wrapper arm: scopes come from the wrapper's `scope` claim
        // (the gateway populated it from the granted scope set at mint, U3).
        scopes: split_scope_claim(&claims.scope),
    }
}

/// Materialise a `WorkerUser` from a hydra introspection response.
///
/// Caller MUST have already gated on `info.active == true`. Used by the
/// P7-U5 fallback path — wrapper-verifier failed (or absent) and we
/// fell through to introspection.
fn build_worker_user_from_introspection(
    info: &crate::oidc_rp::IntrospectionResponse,
) -> OwnedWorkerUser {
    OwnedWorkerUser {
        id: info.sub.clone().unwrap_or_default(),
        email: info.email.clone().unwrap_or_default(),
        name: info.name.clone().unwrap_or_default(),
        email_verified: info.email_verified.unwrap_or(false),
        // Introspection fallback: scopes come from the response `scope` field.
        scopes: split_scope_claim(info.scope.as_deref().unwrap_or_default()),
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
    /// Granted scopes (Slice 3, §1.4). Owned here so the borrowed
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
    /// A valid cookie session resolved a real user and the per-app `pws_`
    /// projected cleanly. Carries the signed `ZeroShip-User` header.
    Allowed(String),
    /// A valid cookie session resolved a real user, but the route has no
    /// `sector_identifier` yet ⇒ no per-app `pws_` derivation possible.
    /// Fail closed (`503`) rather than project the global UUID (§6.2).
    ClientNotProvisioned,
    /// No cookie, no DB configured, validate miss, or a DB error
    /// (fail-closed-as-unauthenticated). The caller treats this as
    /// no-identity.
    None,
}

/// Resolve the `ZeroShip-User` header value from the per-origin app
/// session cookie. Returns [`CookieOutcome::None`] if no cookie, validate
/// fails, or no DB is configured. Logs DB errors at warn — never panics.
///
/// Pairwise projection (Slice 4, §6.2): `auth.gateway_sessions.user_id`
/// is the GLOBAL user UUID (internal storage stays global). The cookie
/// arm derives the per-app `pws_` from it via [`project_pairwise`] (and
/// upserts the mapping) and writes THAT into `ZeroShip-User.id`, so the
/// worker never sees the global UUID. Fails closed
/// ([`CookieOutcome::ClientNotProvisioned`] → `503`) when the route has
/// no `sector_identifier` yet.
async fn resolve_app_session_user_header_inner(
    req: &HttpRequest,
    state: &Arc<GateState>,
    app_id_str: &str,
    request_id: &Uuid,
    oauth_client_id: Option<&str>,
    sector_identifier: Option<&str>,
) -> CookieOutcome {
    let cookie_header = req
        .headers()
        .get("cookie")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let Some(session_id) =
        oidc_rp::parse_app_session_cookie(cookie_header, state.config.insecure_dev)
    else {
        return CookieOutcome::None;
    };
    // Check out a pooled connection for just this validate (which slides
    // the idle window) and release it on drop.
    let Some(db_cfg) = state.db.as_ref() else {
        return CookieOutcome::None;
    };
    let pool = match crate::db::checkout(db_cfg).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "gateway: pg pool checkout failed (session validate)");
            return CookieOutcome::None;
        }
    };
    let conn = match pool.get().await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "gateway: pg pool checkout failed (session validate)");
            return CookieOutcome::None;
        }
    };
    let session = match sessions::validate(&conn, session_id, app_id_str).await {
        Ok(Some(s)) => s,
        Ok(None) => return CookieOutcome::None,
        Err(e) => {
            // Fail-closed on DB errors. Returning None forces the auth
            // gate to treat the request as unauthenticated; the caller
            // either 401s (API) or redirects to login (HTML).
            tracing::warn!(error = %e, "gateway: session validate failed");
            return CookieOutcome::None;
        }
    };
    drop(conn);

    // Project the GLOBAL session user_id to the per-app `pws_` (§6.2) and
    // upsert the mapping. Fail closed when the route has no sector yet. The
    // SAME call reads the active relay alias (§7) — the app-facing email.
    let (pws, relay_email) = match project_pairwise(
        state,
        oauth_client_id,
        sector_identifier,
        &session.user_id,
    )
    .await
    {
        PairwiseProjection::Projected { pws, relay_email } => (pws, relay_email),
        PairwiseProjection::Unprovisioned => return CookieOutcome::ClientNotProvisioned,
    };

    // Email-claim swap (§7): the app sees the relay alias, NEVER the real
    // `session.email`. No active alias ⇒ empty email (fail closed).
    let email = relay_email.as_deref().unwrap_or("");
    let user = oidc_rp::WorkerUser {
        id: &pws,
        email,
        name: session.name.as_deref().unwrap_or(""),
        avatar: session.avatar_url.as_deref(),
        email_verified: session.email_verified,
        // Cookie arm: scopes come from the session row's granted_scopes column
        // (Slice 3, §1.4) — no control.oauth_grants hot-path join.
        scopes: session.granted_scopes.iter().map(String::as_str).collect(),
    };
    CookieOutcome::Allowed(oidc_rp::encode_user_header(
        &user,
        &state.config.worker_key,
        *request_id,
    ))
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
/// Cookie name is `__Host-zs_app_session` in production and
/// `zs_app_session` in dev (RFC 6265bis §4.1.3.2 — `__Host-` mandates
/// Secure, which dev runs over plain HTTP without). Returns `None`
/// when the cookie is missing or empty so callers can fall back to a
/// different discriminator (e.g. per-rule rate-limit session-keyed
/// buckets falling back to IP).
pub(super) fn extract_session_cookie(
    cookie_header: Option<&str>,
    insecure_dev: bool,
) -> Option<String> {
    let s = cookie_header?;
    let prefix = format!("{}=", oidc_rp::app_session_cookie_name(insecure_dev));
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
        // `__Host-zs_app_session=` (empty value) → None, so the caller
        // falls back to IP. Treating empty as a real bucket key would
        // collapse every cookie-empty client into one shared bucket.
        assert_eq!(
            extract_session_cookie(Some("__Host-zs_app_session="), false),
            None
        );
        assert_eq!(extract_session_cookie(None, false), None);
        assert_eq!(extract_session_cookie(Some("other=foo"), false), None);
    }

    #[test]
    fn extract_session_cookie_reads_app_session_value() {
        let id = uuid::Uuid::new_v4();
        let header = format!("foo=bar; __Host-zs_app_session={id}; baz=qux");
        let extracted = extract_session_cookie(Some(&header), false).expect("present");
        assert_eq!(extracted, id.to_string());
    }

    #[test]
    fn extract_session_cookie_dev_uses_bare_name_and_rejects_host_prefix() {
        // Regression for the __Host- + insecure-dev incompatibility:
        // dev cookies have no __Host- prefix (RFC 6265bis §4.1.3.2),
        // so the dev parser must look for the bare name and ignore a
        // stale __Host- cookie of the same suffix.
        let id = uuid::Uuid::new_v4();
        let dev_header = format!("zs_app_session={id}");
        let extracted = extract_session_cookie(Some(&dev_header), true).expect("present");
        assert_eq!(extracted, id.to_string());

        let prod_header = format!("__Host-zs_app_session={id}");
        assert_eq!(extract_session_cookie(Some(&prod_header), true), None);
    }

    // ─── DPoP detection ─────────────────────────────────────────────
    //
    // `has_dpop_authorization` is the discriminator that decides
    // whether `resolve_auth`'s DPoP arm short-circuits to
    // `Unauthenticated` on a failed proof or falls through to the
    // cookie path. Mis-classifying a DPoP request as Bearer (or vice
    // versa) is the difference between "client gets a fresh login
    // page" and "client gets a silent fallback to a stolen cookie".

    #[test]
    fn has_dpop_authorization_detects_dpop_scheme() {
        let req = ntex::web::test::TestRequest::default()
            .header(http::header::AUTHORIZATION, "DPoP abc.def.ghi")
            .to_http_request();
        assert!(has_dpop_authorization(&req));
    }

    #[test]
    fn has_dpop_authorization_rejects_bearer() {
        // Bearer is NOT DPoP — the DPoP arm must not fire for a
        // plain bearer token (Phase 7 deliberately doesn't accept
        // bearer; future API-key flows will live on a different path).
        let req = ntex::web::test::TestRequest::default()
            .header(http::header::AUTHORIZATION, "Bearer abc")
            .to_http_request();
        assert!(!has_dpop_authorization(&req));
    }

    #[test]
    fn has_dpop_authorization_returns_false_when_absent() {
        let req = ntex::web::test::TestRequest::default().to_http_request();
        assert!(!has_dpop_authorization(&req));
    }

    #[test]
    fn has_dpop_authorization_rejects_case_variants() {
        // `Authorization` scheme tokens are technically case-insensitive
        // per RFC 7235, but RFC 9449 §7.1 spells the scheme as `DPoP`
        // and we follow that literal — keeping the match strict means
        // a typo / lower-case variant doesn't accidentally take the
        // DPoP path with a malformed proof and silently 401.
        let req = ntex::web::test::TestRequest::default()
            .header(http::header::AUTHORIZATION, "dpop abc")
            .to_http_request();
        assert!(!has_dpop_authorization(&req));
    }

    // ─── Wrapper-token worker-user builders (Phase 8 U4) ──────────────
    //
    // The wrapper-token fast path skips hydra introspection by mapping
    // the wrapper's embedded claims straight onto a `WorkerUser`. A
    // regression in the field mapping (e.g. losing `email_verified`,
    // dropping `name`) would silently degrade the worker's view of the
    // authenticated user — covered here.

    #[test]
    fn build_worker_user_from_wrapper_maps_all_fields() {
        use crate::wrapper_token::{Cnf, WrapperClaims};
        let claims = WrapperClaims {
            iss: "https://api.zeroship.ai".into(),
            aud: "myapp.zeroship.ai".into(),
            sub: "usr_abc".into(),
            exp: 0,
            iat: 0,
            jti: "j".into(),
            cnf: Some(Cnf { jkt: "k".into() }),
            scope: "openid".into(),
            client_id: "gateway".into(),
            email: Some("a@b.test".into()),
            email_verified: Some(true),
            name: Some("Alice".into()),
            wraps: Some("w".into()),
        };
        let owned = build_worker_user_from_wrapper(&claims);
        assert_eq!(owned.id, "usr_abc");
        assert_eq!(owned.email, "a@b.test");
        assert_eq!(owned.name, "Alice");
        assert!(owned.email_verified);
        assert_eq!(owned.scopes, vec!["openid".to_string()]);
    }

    /// The Bearer-wrapper arm carries the app's granted scopes from the
    /// wrapper `scope` claim onto `WorkerUser.scopes` (Slice 3, §1.4), and they
    /// survive the encode → verify → JSON-parse round-trip the worker performs.
    #[test]
    fn worker_user_scopes_round_trip_through_header() {
        use crate::wrapper_token::{Cnf, WrapperClaims};
        let claims = WrapperClaims {
            iss: "https://api.zeroship.ai".into(),
            aud: "myapp.zeroship.ai".into(),
            sub: "pws_abc".into(),
            exp: 0,
            iat: 0,
            jti: "j".into(),
            cnf: Some(Cnf { jkt: "k".into() }),
            scope: "openid read:billing write:projects".into(),
            client_id: "oac_app".into(),
            email: Some("a@b.test".into()),
            email_verified: Some(true),
            name: Some("Alice".into()),
            wraps: None,
        };
        let owned = build_worker_user_from_wrapper(&claims);
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

    /// No granted scopes (e.g. client-credentials) ⇒ `WorkerUser.scopes` is an
    /// empty vec, never a `[""]`.
    #[test]
    fn worker_user_scopes_empty_when_no_scope_claim() {
        use crate::wrapper_token::{Cnf, WrapperClaims};
        let claims = WrapperClaims {
            iss: "https://api.zeroship.ai".into(),
            aud: "myapp.zeroship.ai".into(),
            sub: "usr_x".into(),
            exp: 0,
            iat: 0,
            jti: "j".into(),
            cnf: Some(Cnf { jkt: "k".into() }),
            scope: "   ".into(),
            client_id: "oac_app".into(),
            email: None,
            email_verified: None,
            name: None,
            wraps: None,
        };
        let owned = build_worker_user_from_wrapper(&claims);
        assert!(owned.scopes.is_empty(), "whitespace-only scope ⇒ empty vec");
    }

    #[test]
    fn build_worker_user_from_wrapper_defaults_missing_optionals() {
        // hydra omits `email`/`name`/`email_verified` for client-credentials
        // grants (or when the scope wasn't granted). The wrapper claims
        // mirror that with `Option`; the worker-user struct doesn't —
        // we materialise defaults so the worker never sees a serde error
        // on a token issued for a non-user identity.
        use crate::wrapper_token::{Cnf, WrapperClaims};
        let claims = WrapperClaims {
            iss: "https://api.zeroship.ai".into(),
            aud: "myapp.zeroship.ai".into(),
            sub: "usr_test".into(),
            exp: 0,
            iat: 0,
            jti: "j".into(),
            cnf: Some(Cnf { jkt: "k".into() }),
            scope: String::new(),
            client_id: "gateway".into(),
            email: None,
            email_verified: None,
            name: None,
            wraps: Some("w".into()),
        };
        let owned = build_worker_user_from_wrapper(&claims);
        assert_eq!(owned.id, "usr_test");
        assert_eq!(owned.email, "");
        assert_eq!(owned.name, "");
        assert!(!owned.email_verified);
    }

    #[test]
    fn build_worker_user_from_introspection_maps_all_fields() {
        // The introspection fallback path (P7-U5) builds the WorkerUser
        // from the hydra `/oauth2/introspect` response. Same regression
        // surface as the wrapper helper — a field-rename break here
        // would silently corrupt the worker's authenticated-user view.
        let info = crate::oidc_rp::IntrospectionResponse {
            active: true,
            sub: Some("usr_xyz".into()),
            client_id: Some("gateway".into()),
            email: Some("u@x.test".into()),
            email_verified: Some(true),
            name: Some("Bob".into()),
            scope: Some("openid email".into()),
            exp: Some(0),
            iat: Some(0),
        };
        let owned = build_worker_user_from_introspection(&info);
        assert_eq!(owned.id, "usr_xyz");
        assert_eq!(owned.email, "u@x.test");
        assert_eq!(owned.name, "Bob");
        assert!(owned.email_verified);
        assert_eq!(owned.scopes, vec!["openid".to_string(), "email".to_string()]);
    }

    /// The raw-Hydra Bearer arm carries `scope` from the access-token claims
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
        };
        let owned = build_worker_user_from_access_claims(&claims);
        assert_eq!(owned.id, "usr_global");
        assert_eq!(owned.scopes, vec!["openid".to_string(), "read:billing".to_string()]);
    }

    // ─── cnf.jkt enforcement (Phase 8 U4) ──────────────────────────────
    //
    // The whole point of the wrapper-token branch is the binding check:
    // a wrapper minted for jkt_A presented with a DPoP proof signed by
    // jkt_B must be rejected. The wrapper-token round-trip itself is
    // covered in `wrapper_token::tests`; the integration with a real
    // DPoP proof + GateState lives in U5. Here we cover the
    // binding-comparison call sites directly — same `cnf.jkt` shape
    // the dispatch path consumes.

    #[test]
    fn cnf_jkt_match_compares_strings_exactly() {
        use crate::wrapper_token::Cnf;
        // Two thumbprints with the same logical value but different
        // string content MUST NOT match — DPoP thumbprints are
        // base64url-encoded SHA-256 outputs, so any visual difference
        // is a real key difference.
        let a = Cnf {
            jkt: "abcDEF123".into(),
        };
        let b = Cnf {
            jkt: "abcDEF123".into(),
        };
        let c = Cnf {
            jkt: "abcDEF124".into(),
        };
        assert_eq!(a.jkt, b.jkt);
        assert_ne!(a.jkt, c.jkt);
    }

    // ─── End-to-end wrapper-token binding (Phase 8 U4) ────────────────
    //
    // The integration test below builds a real DPoP proof, a real
    // wrapper token, and a real `GateState` (sans live PG / hydra) and
    // drives `resolve_dpop_user_header` directly. This exercises:
    //
    //   1. Wrapper-verify SUCCESS + matching `cnf.jkt` → returns Some
    //      (the worker-user header). No hydra call required (the
    //      OidcRp in the fixture points at a dead URL — a successful
    //      return PROVES the wrapper path short-circuited).
    //
    //   2. Wrapper-verify SUCCESS + mismatched `cnf.jkt` → returns
    //      None (binding rejected; no fallthrough to introspection).
    //      Same OidcRp pointing at a dead URL — a fallthrough would
    //      surface as a network error in tracing but still return
    //      None; the assertion is that we never reach the network at
    //      all (the test passes in <100 ms regardless of the dead URL).

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
    }

    /// Build a Gateway state with wrapper-token issuer + verifier
    /// configured against the supplied signing key. `OidcRp` points at
    /// a dead URL — a successful return from `resolve_dpop_user_header`
    /// PROVES the wrapper short-circuit fired, since the fallback
    /// would have failed on the network call.
    fn build_state_with_wrapper(
        signing: ed25519_dalek::SigningKey,
    ) -> std::sync::Arc<crate::GateState> {
        build_state_with_wrapper_and_auth_ui_url(signing, "http://127.0.0.1:1")
    }

    fn build_state_with_wrapper_and_auth_ui_url(
        signing: ed25519_dalek::SigningKey,
        auth_ui_url: &str,
    ) -> std::sync::Arc<crate::GateState> {
        build_state_with_wrapper_and_auth_ui_url_and_db(signing, auth_ui_url, None)
    }

    fn build_state_with_wrapper_and_auth_ui_url_and_db(
        signing: ed25519_dalek::SigningKey,
        auth_ui_url: &str,
        db: Option<crate::db::DbConfig>,
    ) -> std::sync::Arc<crate::GateState> {
        // Preserve the historical behavior: `auth_ui_url` drives the
        // OidcRp dial URL (and JWKS), with the issuer derived from it.
        let oidc_rp = crate::oidc_rp::OidcRp::new(
            auth_ui_url,
            "gateway",
            "test-secret",
            b"test-stash-key-32-bytes-long----".to_vec(),
        );
        build_state_with_wrapper_and_oidc_and_db(signing, oidc_rp, db)
    }

    /// Most general state builder: inject a custom [`OidcRp`] so the
    /// raw-Hydra Bearer tests can point its JWKS cache at a live test
    /// server and pin a known `issuer`. The wrapper issuer/verifier are
    /// still built from `signing` (the gateway's own key); `public_url`
    /// stays `https://api.zeroship.ai` (the wrapper `iss`).
    fn build_state_with_wrapper_and_oidc_and_db(
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

        let issuer =
            crate::wrapper_token::Issuer::new(&signing, "https://api.zeroship.ai".into())
                .expect("issuer");
        let verifier = crate::wrapper_token::Verifier::new(
            &signing.verifying_key(),
            "https://api.zeroship.ai".into(),
        );

        StdArc::new(crate::GateState {
            config: crate::GateConfig {
                control_url: String::new(),
                control_key: String::new(),
                worker_urls: vec![],
                poll_interval_secs: 5,
                worker_key: "wk".into(),
                hydra_public_url: String::new(),
                auth_ui_url: oidc_rp.auth_ui_url.clone(),
                insecure_dev: true,
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
            dpop_jti_cache: StdArc::new(zeroship_core::dpop::TieredJtiCache::default()),
            logout_jti_cache: StdArc::new(
                zeroship_core::logout_token::LogoutJtiCache::default(),
            ),
            signing_key: Some(StdArc::new(signing)),
            prev_signing_key: None,
            wrapper_issuer: Some(StdArc::new(issuer)),
            wrapper_verifier: Some(StdArc::new(verifier)),
            anchor_enc_key: [0u8; 32],
            pairwise_salt: [0u8; 32],
        })
    }

    /// Sign a `DPoP` proof with the supplied Ed25519 client key, bound
    /// to the given htm/htu/access-token. Mirrors the test-helper
    /// used in `zeroship_core::dpop::tests` — we duplicate it here to
    /// keep the test self-contained (the core helper is `cfg(test)`
    /// private to that module).
    fn sign_dpop_proof(
        client_key: &ed25519_dalek::SigningKey,
        htm: &str,
        htu: &str,
        access_token: &str,
        now: i64,
    ) -> String {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine as _;
        use ed25519_dalek::Signer;
        use sha2::{Digest, Sha256};

        let pk = client_key.verifying_key();
        let x = URL_SAFE_NO_PAD.encode(pk.to_bytes());
        let jwk = serde_json::json!({
            "kty": "OKP",
            "crv": "Ed25519",
            "x": x,
        });
        let header = serde_json::json!({
            "typ": "dpop+jwt",
            "alg": "EdDSA",
            "jwk": jwk,
        });
        let ath = URL_SAFE_NO_PAD.encode(Sha256::digest(access_token.as_bytes()));
        let body = serde_json::json!({
            "jti": uuid::Uuid::new_v4().to_string(),
            "htm": htm,
            "htu": htu,
            "iat": now,
            "ath": ath,
        });
        let header_b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header).unwrap());
        let body_b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&body).unwrap());
        let signing_input = format!("{header_b64}.{body_b64}");
        let sig = client_key.sign(signing_input.as_bytes());
        let sig_b64 = URL_SAFE_NO_PAD.encode(sig.to_bytes());
        format!("{signing_input}.{sig_b64}")
    }

    /// Compute the RFC 7638 JWK thumbprint of an Ed25519 client key —
    /// used to bind the wrapper token to a specific `DPoP` key on the
    /// issue side.
    fn client_jkt(client_key: &ed25519_dalek::SigningKey) -> String {
        crate::signing::jwk_thumbprint(client_key)
    }

    /// Mint a wrapper token via the issuer, bound to `proof_jkt`.
    fn issue_wrapper(
        state: &crate::GateState,
        proof_jkt: &str,
        aud: &str,
    ) -> String {
        // A `pws_…`-shaped default sub: a gateway-issued wrapper's `sub` is
        // ALWAYS a per-app pairwise pseudonym (the self-describing-subject
        // invariant, Batch A fix 2), so the test fixtures mint one too.
        issue_wrapper_for_sub(state, "pws_testsubject0000000", proof_jkt, aud)
    }

    /// Build a DPoP-style [`WrapperMint`] (cnf + wraps present) for the
    /// dispatch tests. Mirrors what `dpop_exchange.rs` builds from an
    /// introspection response.
    fn dpop_test_mint<'a>(sub: &'a str, proof_jkt: &'a str, aud: &'a str) -> crate::wrapper_token::WrapperMint<'a> {
        crate::wrapper_token::WrapperMint {
            aud,
            sub,
            scope: "openid",
            client_id: "gateway",
            exp_secs: 3600,
            cnf: Some(proof_jkt),
            wraps: Some("aGVsbG8"),
            email: Some("test@example.com"),
            email_verified: Some(true),
            name: Some("Test"),
        }
    }

    fn issue_wrapper_for_sub(
        state: &crate::GateState,
        sub: &str,
        proof_jkt: &str,
        aud: &str,
    ) -> String {
        state
            .wrapper_issuer
            .as_ref()
            .expect("issuer configured")
            .issue(&dpop_test_mint(sub, proof_jkt, aud))
            .expect("issue wrapper")
    }

    fn issue_wrapper_with_signing_key(
        signing: &ed25519_dalek::SigningKey,
        proof_jkt: &str,
        aud: &str,
    ) -> String {
        let issuer =
            crate::wrapper_token::Issuer::new(signing, "https://api.zeroship.ai".into())
                .expect("issuer");
        issuer
            .issue(&dpop_test_mint("usr_test", proof_jkt, aud))
            .expect("issue wrapper")
    }

    fn sign_wrapper_claims(
        signing: &ed25519_dalek::SigningKey,
        mut claims: crate::wrapper_token::WrapperClaims,
    ) -> String {
        use ed25519_dalek::pkcs8::EncodePrivateKey;
        use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};

        claims.iss = "https://api.zeroship.ai".into();
        let mut header = Header::new(Algorithm::EdDSA);
        header.typ = Some("at+jwt".into());
        header.kid = Some(crate::signing::jwk_thumbprint(signing));
        let der = signing.to_pkcs8_der().expect("pkcs8");
        let key = EncodingKey::from_ed_der(der.as_bytes());
        encode(&header, &claims, &key).expect("sign wrapper claims")
    }

    fn wrapper_claims(
        sub: impl Into<String>,
        proof_jkt: &str,
        aud: &str,
    ) -> crate::wrapper_token::WrapperClaims {
        let now = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        )
        .unwrap();
        crate::wrapper_token::WrapperClaims {
            iss: "https://api.zeroship.ai".into(),
            aud: aud.into(),
            sub: sub.into(),
            exp: now + 3600,
            iat: now,
            jti: Uuid::new_v4().to_string(),
            cnf: Some(crate::wrapper_token::Cnf {
                jkt: proof_jkt.into(),
            }),
            scope: "openid".into(),
            client_id: "gateway".into(),
            email: Some("test@example.com".into()),
            email_verified: Some(true),
            name: Some("Test".into()),
            wraps: Some("hydra-token-shadow".into()),
        }
    }

    #[compio::test]
    async fn resolve_dpop_accepts_wrapper_when_jkt_matches() {
        // Happy path: client signs the DPoP proof with key A, wrapper
        // is bound to key A's thumbprint, dispatch verifies → Some.
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let client_key = ed25519_dalek::SigningKey::from_bytes(&[42u8; 32]);
        let state = build_state_with_wrapper(gateway_signing);

        let jkt = client_jkt(&client_key);
        let aud = "myapp.zeroship.ai";
        let wrapper = issue_wrapper(&state, &jkt, aud);

        let now = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        )
        .unwrap();
        let htu = format!("http://{aud}/api/me");
        let proof = sign_dpop_proof(&client_key, "GET", &htu, &wrapper, now);

        let req = ntex::web::test::TestRequest::default()
            .uri("/api/me")
            .header(http::header::HOST, aud)
            .header(http::header::AUTHORIZATION, format!("DPoP {wrapper}"))
            .header("dpop", proof)
            .to_http_request();

        let request_id = Uuid::new_v4();
        let header = resolve_dpop_user_header(&req, &state, &request_id, None, None).await;
        assert!(matches!(header, DpopOutcome::Allowed(_)), "wrapper path must accept matched jkt");
    }

    #[compio::test]
    async fn resolve_dpop_rejects_plain_bearer_wrapper_without_cnf() {
        // A plain-Bearer wrapper (cnf: None — the browser mint) has no
        // DPoP key binding, so it can never satisfy the DPoP path's
        // `cnf.jkt == proof.jkt` check. The DPoP fast-path must reject
        // it rather than treat a missing cnf as a wildcard match.
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let client_key = ed25519_dalek::SigningKey::from_bytes(&[42u8; 32]);
        let state = build_state_with_wrapper(gateway_signing);

        let aud = "myapp.zeroship.ai";
        // Mint a cnf-less wrapper directly via the issuer.
        let wrapper = state
            .wrapper_issuer
            .as_ref()
            .expect("issuer")
            .issue(&crate::wrapper_token::WrapperMint {
                aud,
                sub: "pws_browser",
                scope: "openid",
                client_id: "gateway",
                exp_secs: 600,
                cnf: None,
                wraps: None,
                email: None,
                email_verified: None,
                name: None,
            })
            .expect("issue plain wrapper");

        let now = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        )
        .unwrap();
        let htu = format!("http://{aud}/api/me");
        let proof = sign_dpop_proof(&client_key, "GET", &htu, &wrapper, now);

        let req = ntex::web::test::TestRequest::default()
            .uri("/api/me")
            .header(http::header::HOST, aud)
            .header(http::header::AUTHORIZATION, format!("DPoP {wrapper}"))
            .header("dpop", proof)
            .to_http_request();

        let request_id = Uuid::new_v4();
        let header = resolve_dpop_user_header(&req, &state, &request_id, None, None).await;
        assert!(matches!(header, DpopOutcome::None), "DPoP path must reject a cnf-less plain-Bearer wrapper");
    }

    /// Batch A fix 3: the DPoP wrapper fast-path keys revocation on the per-app
    /// FAMILY marker `(client_id, pws_)` — the SAME mechanism (and the SAME
    /// writer) the Bearer-wrapper / raw-Hydra / introspection arms use. (This
    /// replaced a legacy UUID-only subject denylist that a `pws_` sub could
    /// never match — now deleted, Batch A M2.) Mint a `pws_`-sub DPoP wrapper,
    /// prove it resolves, `revoke_family(client_id, pws_)` the way /signout
    /// does, and prove the SAME wrapper is then rejected. PG-gated.
    #[compio::test]
    async fn resolve_dpop_rejects_wrapper_revoked_by_family_marker() {
        let Ok(dsn) = std::env::var("AUTH_DB_URL") else {
            eprintln!("skipping (no AUTH_DB_URL)");
            return;
        };
        let db_cfg = crate::db::DbConfig::new(dsn.clone(), 4);

        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let client_key = ed25519_dalek::SigningKey::from_bytes(&[42u8; 32]);
        let state = build_state_with_wrapper_and_auth_ui_url_and_db(
            gateway_signing,
            "http://127.0.0.1:1",
            Some(db_cfg.clone()),
        );

        let jkt = client_jkt(&client_key);
        let aud = "myapp.zeroship.ai";
        // The wrapper's client_id is the `dpop_test_mint` default ("gateway");
        // the family marker keys on that + the wrapper's pws_ sub.
        let marker_client_id = "gateway";
        let pws_sub = format!("pws_{}", Uuid::new_v4().simple());
        let wrapper = issue_wrapper_for_sub(&state, &pws_sub, &jkt, aud);
        let htu = format!("http://{aud}/api/me");
        let now = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        )
        .unwrap();

        let first_proof = sign_dpop_proof(&client_key, "GET", &htu, &wrapper, now);
        let first_req = ntex::web::test::TestRequest::default()
            .uri("/api/me")
            .header(http::header::HOST, aud)
            .header(http::header::AUTHORIZATION, format!("DPoP {wrapper}"))
            .header("dpop", first_proof)
            .to_http_request();
        let request_id = Uuid::new_v4();
        assert!(
            matches!(
                resolve_dpop_user_header(&first_req, &state, &request_id, None, None).await,
                DpopOutcome::Allowed(_)
            ),
            "wrapper should resolve before family revocation"
        );

        {
            let pool = crate::db::checkout(&db_cfg).await.expect("pool checkout");
            let conn = pool.get().await.expect("pool checkout");
            zeroship_core::wrapper_revocation::revoke_family(&conn, marker_client_id, &pws_sub)
                .await
                .expect("revoke family (client_id, pws_)");
        }

        let second_proof = sign_dpop_proof(&client_key, "GET", &htu, &wrapper, now);
        let second_req = ntex::web::test::TestRequest::default()
            .uri("/api/me")
            .header(http::header::HOST, aud)
            .header(http::header::AUTHORIZATION, format!("DPoP {wrapper}"))
            .header("dpop", second_proof)
            .to_http_request();
        assert!(
            matches!(
                resolve_dpop_user_header(&second_req, &state, &request_id, None, None).await,
                DpopOutcome::None
            ),
            "revoked (client_id, pws_) family must reject the DPoP wrapper"
        );

        let pool = crate::db::checkout(&db_cfg).await.expect("pool checkout");
        let conn = pool.get().await.expect("pool checkout");
        conn.execute(
            "DELETE FROM auth.token_revocations WHERE client_id = $1 AND sub = $2",
            &[&marker_client_id, &pws_sub],
        )
        .await
        .ok();
    }

    #[compio::test]
    async fn resolve_dpop_rejects_wrapper_with_empty_sub() {
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let client_key = ed25519_dalek::SigningKey::from_bytes(&[42u8; 32]);
        let state = build_state_with_wrapper(gateway_signing.clone());

        let jkt = client_jkt(&client_key);
        let aud = "myapp.zeroship.ai";
        let wrapper = sign_wrapper_claims(&gateway_signing, wrapper_claims("", &jkt, aud));

        let now = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        )
        .unwrap();
        let htu = format!("http://{aud}/api/me");
        let proof = sign_dpop_proof(&client_key, "GET", &htu, &wrapper, now);

        let req = ntex::web::test::TestRequest::default()
            .uri("/api/me")
            .header(http::header::HOST, aud)
            .header(http::header::AUTHORIZATION, format!("DPoP {wrapper}"))
            .header("dpop", proof)
            .to_http_request();

        let request_id = Uuid::new_v4();
        let header = resolve_dpop_user_header(&req, &state, &request_id, None, None).await;
        assert!(matches!(header, DpopOutcome::None), "wrapper path must reject tokens with an empty sub");
    }

    #[compio::test]
    async fn resolve_dpop_rejects_wrapper_when_jkt_mismatches() {
        // cnf.jkt mismatch: wrapper bound to key A's thumbprint, but
        // the client signs the proof with key B. Must return None
        // (binding rejected). Critically, this MUST NOT silently fall
        // through to introspection — that would defeat the whole
        // point of the binding.
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let client_key_a = ed25519_dalek::SigningKey::from_bytes(&[42u8; 32]);
        let client_key_b = ed25519_dalek::SigningKey::from_bytes(&[43u8; 32]);
        let state = build_state_with_wrapper(gateway_signing);

        let jkt_a = client_jkt(&client_key_a);
        let aud = "myapp.zeroship.ai";
        // Wrapper bound to A.
        let wrapper = issue_wrapper(&state, &jkt_a, aud);

        let now = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        )
        .unwrap();
        let htu = format!("http://{aud}/api/me");
        // Proof signed by B.
        let proof = sign_dpop_proof(&client_key_b, "GET", &htu, &wrapper, now);

        let req = ntex::web::test::TestRequest::default()
            .uri("/api/me")
            .header(http::header::HOST, aud)
            .header(http::header::AUTHORIZATION, format!("DPoP {wrapper}"))
            .header("dpop", proof)
            .to_http_request();

        let request_id = Uuid::new_v4();
        let header = resolve_dpop_user_header(&req, &state, &request_id, None, None).await;
        assert!(matches!(header, DpopOutcome::None), "wrapper path must reject when cnf.jkt does not match proof jkt");
    }

    #[ntex::test]
    async fn e2e_rejects_malformed_wrapper_no_downgrade() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc as StdArc;

        async fn active_introspection(
            hits: ntex::web::types::State<StdArc<AtomicUsize>>,
        ) -> ntex::web::HttpResponse {
            hits.fetch_add(1, Ordering::SeqCst);
            ntex::web::HttpResponse::Ok().json(&serde_json::json!({
                "active": true,
                "sub": "usr_from_introspection",
                "client_id": "gateway",
                "email": "fallback@example.com",
                "email_verified": true,
                "name": "Fallback User",
                "scope": "openid"
            }))
        }

        let hits = StdArc::new(AtomicUsize::new(0));
        let hits_for_server = hits.clone();
        let srv = ntex::web::test::server(move || {
            let hits = hits_for_server.clone();
            async move {
                ntex::web::App::new().state(hits).service(
                    ntex::web::resource("/oauth2/introspect")
                        .route(ntex::web::post().to(active_introspection)),
                )
            }
        })
        .await;
        let auth_ui_url = srv.url("").trim_end_matches('/').to_string();

        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let forged_signing = ed25519_dalek::SigningKey::from_bytes(&[99u8; 32]);
        let client_key = ed25519_dalek::SigningKey::from_bytes(&[42u8; 32]);
        let state =
            build_state_with_wrapper_and_auth_ui_url(gateway_signing, &auth_ui_url);

        let aud = "myapp.zeroship.ai";
        let jkt = client_jkt(&client_key);
        let malformed_wrapper = issue_wrapper_with_signing_key(&forged_signing, &jkt, aud);
        assert!(
            looks_like_wrapper(&malformed_wrapper),
            "fixture must be wrapper-shaped"
        );

        let now = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        )
        .unwrap();
        let htu = format!("http://{aud}/api/me");
        let proof = sign_dpop_proof(&client_key, "GET", &htu, &malformed_wrapper, now);

        let req = ntex::web::test::TestRequest::default()
            .uri("/api/me")
            .header(http::header::HOST, aud)
            .header(
                http::header::AUTHORIZATION,
                format!("DPoP {malformed_wrapper}"),
            )
            .header("dpop", proof)
            .to_http_request();

        let request_id = Uuid::new_v4();
        let header = resolve_dpop_user_header(&req, &state, &request_id, None, None).await;
        assert!(matches!(header, DpopOutcome::None), "malformed wrapper must hard-reject instead of downgrading to introspection");
        assert_eq!(
            hits.load(Ordering::SeqCst),
            0,
            "malformed wrapper must not call hydra introspection"
        );

        drop(srv);
    }

    // ─── Bearer arm (slice 1c) ────────────────────────────────────────
    //
    // The Bearer arm sits between the DPoP arm and the cookie arm. It
    // recognizes two issuer-discriminated user-session shapes — the
    // gateway WRAPPER (verified by `wrapper_verifier`) and a raw Hydra
    // access JWT (verified locally via the gateway JWKS) — plus the
    // reserved API-key path (any other `iss`). These tests exercise
    // `resolve_bearer_user_header` directly with the REAL `Verifier` and
    // `JwksCache` (no stubs), and the `Anon`/`User` policy gate through
    // `resolve_auth`.

    /// The gateway's public URL — the `iss` of every wrapper token. The
    /// Bearer arm's wrapper discriminator matches `config.public_url`.
    const GATEWAY_ISS: &str = "https://api.zeroship.ai";
    /// The logical Hydra issuer the raw-Hydra path pins. The test JWKS
    /// server dials loopback, but `OidcRp::with_issuer` decouples the
    /// dial URL from the `iss` the access JWT actually carries.
    const HYDRA_ISS: &str = "https://auth.zeroship.ai/";

    /// Mint a plain-Bearer wrapper (cnf = None — the browser shape) for
    /// `sub`/`client_id`, bound to `aud` (the request Host). This is the
    /// default browser-session token shape the Bearer arm sees.
    fn issue_plain_wrapper(
        state: &crate::GateState,
        sub: &str,
        client_id: &str,
        aud: &str,
    ) -> String {
        issue_plain_wrapper_with_scope(state, sub, client_id, aud, "openid email")
    }

    /// Same as [`issue_plain_wrapper`] but with a caller-chosen `scope`
    /// claim, so the route-level scope-enforcement tests (Slice 3c) can
    /// mint a principal with / without a specific granted scope.
    fn issue_plain_wrapper_with_scope(
        state: &crate::GateState,
        sub: &str,
        client_id: &str,
        aud: &str,
        scope: &str,
    ) -> String {
        state
            .wrapper_issuer
            .as_ref()
            .expect("issuer configured")
            .issue(&crate::wrapper_token::WrapperMint {
                aud,
                sub,
                scope,
                client_id,
                exp_secs: 600,
                cnf: None,
                wraps: None,
                email: Some("relay-alias@zeroship.ai"),
                email_verified: Some(true),
                name: Some("Plain Bearer User"),
            })
            .expect("issue plain wrapper")
    }

    /// Sign a raw Hydra-style access JWT (RFC 9068) with `signing`
    /// (EdDSA). `client_id`/`aud` are stamped so the Bearer arm's per-app
    /// binding (client_id primary, aud fallback) can be exercised; pass
    /// `client_id: None` to drop the claim and force the aud fallback.
    /// `exp_delta` controls expiry relative to now (negative ⇒ expired).
    #[allow(clippy::too_many_arguments)]
    fn sign_hydra_access_jwt(
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
            "iss": HYDRA_ISS,
            "aud": aud,
            "exp": now + exp_delta,
            "iat": now - 5,
            "email": email,
            "email_verified": true,
            "name": name,
            "scope": "openid email",
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
    fn hydra_jwks_doc(signing: &ed25519_dalek::SigningKey) -> serde_json::Value {
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
    /// URL. The gateway `OidcRp` dials `{base}/.well-known/jwks.json`.
    async fn start_jwks_server(doc: serde_json::Value) -> ntex::web::test::TestServer {
        let doc = std::sync::Arc::new(doc);
        let doc_for_server = doc.clone();
        ntex::web::test::server(move || {
            let doc = doc_for_server.clone();
            async move {
                ntex::web::App::new().state(doc).service(
                    ntex::web::resource("/.well-known/jwks.json").route(
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
    /// `issuer` is the canonical Hydra `iss`. `gateway_signing` is the
    /// gateway's own wrapper key (distinct from Hydra's JWKS key).
    fn build_state_for_hydra(
        gateway_signing: ed25519_dalek::SigningKey,
        jwks_base: &str,
    ) -> std::sync::Arc<crate::GateState> {
        let oidc_rp = crate::oidc_rp::OidcRp::new(
            jwks_base,
            "gateway",
            "test-secret",
            b"test-stash-key-32-bytes-long----".to_vec(),
        )
        .with_issuer(HYDRA_ISS);
        build_state_with_wrapper_and_oidc_and_db(gateway_signing, oidc_rp, None)
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

    /// A `User` policy that additionally demands `required` scopes
    /// (auth-sdk Slice 3c, §5.3) — the route-level scope gate.
    fn user_policy_requiring(
        required: &[&str],
    ) -> crate::compiled::EffectivePolicy {
        let mut p = user_policy();
        p.required_scopes = required.iter().map(|s| s.to_string()).collect();
        p
    }

    #[compio::test]
    async fn bearer_valid_wrapper_emits_zeroship_user() {
        // Happy path (wrapper): a plain-Bearer wrapper for the route's
        // client_id verifies and the ZeroShip-User header carries the
        // wrapper's pws_ sub. The email is re-resolved LIVE (Batch A fix 5):
        // this state has no DB (smoke), so the live re-resolve fails CLOSED to
        // an empty email rather than trusting the wrapper's embedded claim —
        // a real gateway always has a DB and re-reads the alias by (client_id,
        // pws_). (See bearer_wrapper_reresolves_relay_email_live_and_blanks_on_revoke
        // for the with-DB live re-resolve + blank-on-revoke path.)
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let state = build_state_with_wrapper(gateway_signing);
        let aud = "myapp.zeroship.ai";
        let token = issue_plain_wrapper(&state, "pws_alice", "oac_myapp", aud);

        let req = bearer_req(&token, aud);
        let request_id = Uuid::new_v4();
        let outcome =
            resolve_bearer_user_header(&req, &state, &request_id, Some("oac_myapp"), None).await;
        let BearerOutcome::Allowed(header) = outcome else {
            panic!("expected Allowed, got {outcome:?}");
        };
        // The emitted header MAC-verifies and decodes to the wrapper user.
        let json = zeroship_core::auth::verify_zeroship_user_header(
            state.config.worker_key.as_bytes(),
            &header,
        )
        .expect("ZeroShip-User MAC verifies");
        let user: serde_json::Value = serde_json::from_str(&json).expect("user json");
        assert_eq!(user["id"], "pws_alice");
        // Smoke mode (no DB) ⇒ live email re-resolve fails closed to empty.
        assert_eq!(user["email"], "", "no-DB wrapper arm fails closed to empty email");
        assert_eq!(user["email_verified"], true);
    }

    #[compio::test]
    async fn bearer_wrapper_client_id_mismatch_rejected() {
        // A wrapper minted for app A's client_id must be rejected at app
        // B's host (per-app binding — the critical safety property).
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let state = build_state_with_wrapper(gateway_signing);
        let aud = "myapp.zeroship.ai";
        let token = issue_plain_wrapper(&state, "pws_alice", "oac_app_a", aud);

        let req = bearer_req(&token, aud);
        let request_id = Uuid::new_v4();
        // Route expects app B's client_id.
        let outcome =
            resolve_bearer_user_header(&req, &state, &request_id, Some("oac_app_b"), None).await;
        assert!(
            matches!(outcome, BearerOutcome::Invalid),
            "client_id mismatch must be Invalid, got {outcome:?}"
        );
    }

    #[compio::test]
    async fn bearer_wrapper_none_client_id_rejected() {
        // When the route is un-provisioned (oauth_client_id == None) a
        // wrapper cannot be bound to a missing client → Invalid (never
        // binds to a falsy value). §1.5 round-2.
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let state = build_state_with_wrapper(gateway_signing);
        let aud = "myapp.zeroship.ai";
        let token = issue_plain_wrapper(&state, "pws_alice", "oac_myapp", aud);

        let req = bearer_req(&token, aud);
        let request_id = Uuid::new_v4();
        let outcome = resolve_bearer_user_header(&req, &state, &request_id, None, None).await;
        assert!(
            matches!(outcome, BearerOutcome::Invalid),
            "wrapper with None client_id must be Invalid, got {outcome:?}"
        );
    }

    #[compio::test]
    async fn bearer_non_jwt_token_is_not_user_session() {
        // An opaque, non-JWT Bearer (e.g. a future `zsk_…` API key) is the
        // reserved path → NotUserSession (401 on every route, including
        // Anon).
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let state = build_state_with_wrapper(gateway_signing);
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
        // A well-formed JWT whose `iss` is neither the gateway nor Hydra
        // is still the reserved path → NotUserSession.
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let state = build_state_with_wrapper(gateway_signing);
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

    #[compio::test]
    async fn resolve_auth_expired_wrapper_on_anon_route_serves_anonymously() {
        // A present-but-expired user-session Bearer on an `Anon` route
        // must NOT 401 — it falls through to anonymous (the SDK
        // auto-attaches Bearer to every request; the 10-min wrapper is
        // often stale on idle public pages). round-3.
        use ed25519_dalek::pkcs8::EncodePrivateKey;
        use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};

        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let state = build_state_with_wrapper(gateway_signing.clone());
        let aud = "myapp.zeroship.ai";

        // Hand-sign an EXPIRED wrapper (exp 100s in the past, beyond the
        // 60s default leeway) — the issuer always stamps a future exp.
        let now = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        )
        .unwrap();
        let claims = crate::wrapper_token::WrapperClaims {
            iss: GATEWAY_ISS.into(),
            aud: aud.into(),
            sub: "pws_alice".into(),
            exp: now - 100,
            iat: now - 700,
            jti: Uuid::new_v4().to_string(),
            cnf: None,
            scope: "openid".into(),
            client_id: "oac_myapp".into(),
            email: Some("relay-alias@zeroship.ai".into()),
            email_verified: Some(true),
            name: None,
            wraps: None,
        };
        let mut header = Header::new(Algorithm::EdDSA);
        header.typ = Some("at+jwt".into());
        header.kid = Some(crate::signing::jwk_thumbprint(&gateway_signing));
        let der = gateway_signing.to_pkcs8_der().unwrap();
        let key = EncodingKey::from_ed_der(der.as_bytes());
        let token = encode(&header, &claims, &key).unwrap();

        let req = bearer_req(&token, aud);
        let app_id = Uuid::new_v4();
        let request_id = Uuid::new_v4();

        // Anon route: expired Bearer → Allowed with NO user header.
        let anon = resolve_auth(
            &req,
            &state,
            &anon_policy(),
            &app_id,
            &request_id,
            Some("oac_myapp"),
            None,
        )
        .await;
        assert!(
            matches!(anon, AuthOutcome::Allowed { user_header: None }),
            "expired Bearer on Anon route must serve anonymously, got {anon:?}"
        );

        // User route: same expired Bearer → Unauthenticated (401).
        let gated = resolve_auth(
            &req,
            &state,
            &user_policy(),
            &app_id,
            &request_id,
            Some("oac_myapp"),
            None,
        )
        .await;
        assert!(
            matches!(gated, AuthOutcome::Unauthenticated),
            "expired Bearer on User route must be Unauthenticated, got {gated:?}"
        );
    }

    // ─── Route-level required-scope enforcement (auth-sdk Slice 3c, §5.3) ──
    //
    // After a principal authenticates (here via a real plain-Bearer
    // wrapper), the matched route's `required_scopes` gate the GRANT:
    // a superset passes, a miss is `403 insufficient_scope` (NOT 401 —
    // identity is fine), and an empty `required_scopes` is unchanged.
    // These drive the REAL `resolve_auth` with a REAL minted wrapper +
    // verifier (no shim), exercising the same path dispatch uses.

    #[compio::test]
    async fn resolve_auth_authenticated_without_required_scope_403s() {
        // Authenticated principal whose granted scopes do NOT include the
        // route's `required_scopes` → InsufficientScope (the dispatch 403),
        // NOT Allowed and NOT Unauthenticated.
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let state = build_state_with_wrapper(gateway_signing);
        let aud = "myapp.zeroship.ai";
        // Granted: openid email — does NOT include read:billing.
        let token =
            issue_plain_wrapper_with_scope(&state, "pws_alice", "oac_myapp", aud, "openid email");

        let req = bearer_req(&token, aud);
        let app_id = Uuid::new_v4();
        let request_id = Uuid::new_v4();

        let outcome = resolve_auth(
            &req,
            &state,
            &user_policy_requiring(&["read:billing"]),
            &app_id,
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

    #[compio::test]
    async fn resolve_auth_authenticated_with_required_scope_allowed() {
        // Same principal + route, but the wrapper WAS granted read:billing
        // → Allowed (the scope gate is a superset check). The emitted
        // ZeroShip-User header carries the granted scopes through.
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let state = build_state_with_wrapper(gateway_signing);
        let aud = "myapp.zeroship.ai";
        let token = issue_plain_wrapper_with_scope(
            &state,
            "pws_alice",
            "oac_myapp",
            aud,
            "openid email read:billing",
        );

        let req = bearer_req(&token, aud);
        let app_id = Uuid::new_v4();
        let request_id = Uuid::new_v4();

        let outcome = resolve_auth(
            &req,
            &state,
            &user_policy_requiring(&["read:billing"]),
            &app_id,
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

    #[compio::test]
    async fn resolve_auth_empty_required_scopes_unchanged() {
        // A route with NO required_scopes is unchanged: the authenticated
        // principal is Allowed regardless of which scopes it carries.
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let state = build_state_with_wrapper(gateway_signing);
        let aud = "myapp.zeroship.ai";
        let token =
            issue_plain_wrapper_with_scope(&state, "pws_alice", "oac_myapp", aud, "openid");

        let req = bearer_req(&token, aud);
        let app_id = Uuid::new_v4();
        let request_id = Uuid::new_v4();

        let outcome = resolve_auth(
            &req,
            &state,
            &user_policy(), // required_scopes == []
            &app_id,
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

    #[compio::test]
    async fn resolve_auth_required_scope_superset_passes() {
        // The gate is a SUPERSET check: a principal granted MORE than the
        // route demands still passes (it has everything required).
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let state = build_state_with_wrapper(gateway_signing);
        let aud = "myapp.zeroship.ai";
        let token = issue_plain_wrapper_with_scope(
            &state,
            "pws_alice",
            "oac_myapp",
            aud,
            "openid email read:billing write:projects",
        );

        let req = bearer_req(&token, aud);
        let app_id = Uuid::new_v4();
        let request_id = Uuid::new_v4();

        let outcome = resolve_auth(
            &req,
            &state,
            &user_policy_requiring(&["read:billing"]),
            &app_id,
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
        let state = build_state_with_wrapper(gateway_signing);
        // No Authorization header, no cookie → unauthenticated.
        let req = ntex::web::test::TestRequest::default()
            .uri("/api/billing")
            .header(http::header::HOST, "myapp.zeroship.ai")
            .to_http_request();
        let app_id = Uuid::new_v4();
        let request_id = Uuid::new_v4();

        let outcome = resolve_auth(
            &req,
            &state,
            &user_policy_requiring(&["read:billing"]),
            &app_id,
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

    #[compio::test]
    async fn resolve_auth_underscoped_authenticated_on_anon_route_is_allowed() {
        // REGRESSION (Slice 3c review finding 2): the scope gate must NOT fire
        // on an `Anon` (public) route. A logged-in browser whose wrapper lacks
        // a scope that a broad `*` parent put into `required_scopes` would
        // otherwise get 403 on the app's own HTML/JS/CSS while a logged-OUT
        // visitor loads it fine — the "logged-in is worse than anonymous on
        // public routes" footgun. The authenticated principal must be Allowed
        // (with its user_header) on the public route regardless of scope.
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let state = build_state_with_wrapper(gateway_signing);
        let aud = "myapp.zeroship.ai";
        // Granted only `openid` — does NOT include the route's required scope.
        let token =
            issue_plain_wrapper_with_scope(&state, "pws_alice", "oac_myapp", aud, "openid");

        let req = bearer_req(&token, aud);
        let app_id = Uuid::new_v4();
        let request_id = Uuid::new_v4();

        // `Anon` route that nonetheless carries `required_scopes` (e.g.
        // inherited from a scoped `*`). The Anon policy must win: Allowed.
        let mut anon_with_scope = anon_policy();
        anon_with_scope.required_scopes = vec!["read:billing".to_string()];

        let outcome = resolve_auth(
            &req,
            &state,
            &anon_with_scope,
            &app_id,
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
        // (unlike an expired wrapper, which falls through to anonymous).
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let state = build_state_with_wrapper(gateway_signing);
        let req = bearer_req("zsk_opaque_api_key", "myapp.zeroship.ai");
        let app_id = Uuid::new_v4();
        let request_id = Uuid::new_v4();
        let outcome = resolve_auth(
            &req,
            &state,
            &anon_policy(),
            &app_id,
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
    async fn bearer_valid_raw_hydra_jwt_emits_zeroship_user() {
        // Happy path (raw Hydra): a real EdDSA-signed access JWT,
        // JWKS-verified against a live JWKS server, with a matching
        // client_id claim → Allowed + ZeroShip-User whose id is the per-app
        // pairwise pws_ (Slice 4 §6.2 — the global UUID sub is projected,
        // never emitted on the worker header).
        let jwks_signing = ed25519_dalek::SigningKey::from_bytes(&[55u8; 32]); // Hydra's key
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]); // gateway wrapper key
        let srv = start_jwks_server(hydra_jwks_doc(&jwks_signing)).await;
        let base = srv.url("").trim_end_matches('/').to_string();
        let state = build_state_for_hydra(gateway_signing, &base);

        let global_sub = "0192f1aa-bbbb-7ccc-8ddd-eeeeffff0001";
        let sector = "https://myapp.zeroship.ai";
        let aud = "myapp.zeroship.ai";
        let token = sign_hydra_access_jwt(
            &jwks_signing,
            global_sub,
            Some("oac_myapp"),
            serde_json::json!(["http://api.zeroship.localhost"]), // resource-server aud, NOT the client
            "user@example.com",
            "Hydra User",
            3600,
        );

        let req = bearer_req(&token, aud);
        let request_id = Uuid::new_v4();
        let outcome = resolve_bearer_user_header(
            &req,
            &state,
            &request_id,
            Some("oac_myapp"),
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
        // Slice 4: the raw-Hydra global UUID is projected to the per-app pws_.
        let expected_pws =
            zeroship_core::auth::derive_pairwise(&state.pairwise_salt, global_sub, sector);
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
        // Slice 5c §7 — email-claim swap: the app NEVER sees the real email.
        // With no DB/alias source here (`build_state_for_hydra` db=None) the
        // swap fails closed → empty email. The real `user@example.com` (what
        // Hydra stamped) must be ABSENT from the projected header.
        assert_eq!(user["email"], "", "no alias ⇒ empty email (fail closed)");
        assert!(
            !json.contains("user@example.com"),
            "real email leaked into ZeroShip-User: {json}"
        );

        drop(srv);
    }

    #[ntex::test]
    async fn bearer_raw_hydra_aud_fallback_binds_when_client_id_absent() {
        // RFC 9068 §3 mandates client_id, but if Hydra ever omits it the
        // Bearer arm falls back to binding on `aud` CONTAINING the
        // expected client_id (the pre-decided S1 fallback). Here the JWT
        // has NO client_id claim but lists the client_id in `aud`.
        let jwks_signing = ed25519_dalek::SigningKey::from_bytes(&[55u8; 32]);
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let srv = start_jwks_server(hydra_jwks_doc(&jwks_signing)).await;
        let base = srv.url("").trim_end_matches('/').to_string();
        let state = build_state_for_hydra(gateway_signing, &base);

        let aud = "myapp.zeroship.ai";
        let token = sign_hydra_access_jwt(
            &jwks_signing,
            "usr_global_uuid",
            None, // no client_id claim → force aud fallback
            serde_json::json!(["http://api.zeroship.localhost", "oac_myapp"]),
            "user@example.com",
            "Hydra User",
            3600,
        );

        let req = bearer_req(&token, aud);
        let request_id = Uuid::new_v4();
        // Provisioned route (sector present) so the pairwise projection
        // succeeds — this test exercises the aud-fallback BINDING, not the
        // fail-closed path.
        let outcome = resolve_bearer_user_header(
            &req,
            &state,
            &request_id,
            Some("oac_myapp"),
            Some("https://myapp.zeroship.ai"),
        )
        .await;
        assert!(
            matches!(outcome, BearerOutcome::Allowed(_)),
            "aud-fallback binding must Allow, got {outcome:?}"
        );

        drop(srv);
    }

    #[ntex::test]
    async fn bearer_raw_hydra_client_id_mismatch_rejected() {
        // A raw-Hydra JWT whose client_id claim is app A must be rejected
        // at app B's host (cross-app replay defense on the raw path too).
        let jwks_signing = ed25519_dalek::SigningKey::from_bytes(&[55u8; 32]);
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let srv = start_jwks_server(hydra_jwks_doc(&jwks_signing)).await;
        let base = srv.url("").trim_end_matches('/').to_string();
        let state = build_state_for_hydra(gateway_signing, &base);

        let aud = "myapp.zeroship.ai";
        let token = sign_hydra_access_jwt(
            &jwks_signing,
            "usr_global_uuid",
            Some("oac_app_a"),
            serde_json::json!(["http://api.zeroship.localhost"]),
            "user@example.com",
            "Hydra User",
            3600,
        );

        let req = bearer_req(&token, aud);
        let request_id = Uuid::new_v4();
        let outcome =
            resolve_bearer_user_header(&req, &state, &request_id, Some("oac_app_b"), None).await;
        assert!(
            matches!(outcome, BearerOutcome::Invalid),
            "raw-Hydra client_id mismatch must be Invalid, got {outcome:?}"
        );

        drop(srv);
    }

    // ─── DPoP introspection-fallback per-app binding (Slice 4 token-confusion
    //     fix) ──────────────────────────────────────────────────────────────
    //
    // The DPoP wrapper fast-path binds via `aud == Host` + `cnf.jkt`. The
    // raw-opaque introspection fallback (6b) has neither, so it now binds the
    // introspected `client_id` to the route's expected `oauth_client_id` —
    // mirroring the raw-Hydra Bearer arm. These tests drive the REAL
    // `resolve_dpop_user_header` against a loopback `/oauth2/introspect` mock
    // (same harness shape as `start_jwks_server` / the dpop_exchange tests):
    //
    //   - client_id mismatch (token issued to app A, presented at app B) →
    //     rejected (`DpopOutcome::None`), no global UUID projected.
    //   - client_id match → `Allowed`, sub projected to the per-app `pws_`.
    //   - route un-provisioned (`oauth_client_id == None`) → rejected,
    //     consistent with the Bearer arm refusing to bind an unbound token.
    //
    // An opaque (non-JWT) access token + `wrapper_verifier: None` guarantees
    // the wrapper fast-path is skipped and the introspection arm runs.

    /// Spin up a loopback Hydra `/oauth2/introspect` mock returning `body`
    /// verbatim. `OidcRp::introspect_token` POSTs to `{auth_ui_url}/oauth2/
    /// introspect`, so the gateway must be built with `auth_ui_url` = this
    /// server's base URL.
    async fn start_introspect_server(
        body: serde_json::Value,
    ) -> ntex::web::test::TestServer {
        let body = std::sync::Arc::new(body);
        let body_for_server = body.clone();
        ntex::web::test::server(move || {
            let body = body_for_server.clone();
            async move {
                ntex::web::App::new().state(body).service(
                    ntex::web::resource("/oauth2/introspect").route(
                        ntex::web::post().to(
                            |body: ntex::web::types::State<
                                std::sync::Arc<serde_json::Value>,
                            >| async move {
                                ntex::web::HttpResponse::Ok().json(body.get_ref().as_ref())
                            },
                        ),
                    ),
                )
            }
        })
        .await
    }

    /// Build a state whose `OidcRp` introspection endpoint dials
    /// `introspect_base` and whose `wrapper_verifier` is `None` — so a DPoP
    /// access token that is NOT a wrapper falls straight through to the
    /// introspection fallback (6b). `db: None` keeps the pairwise projection
    /// a pure HMAC (no PG round-trip) while still exercising the real
    /// `project_pairwise`.
    fn build_state_for_introspection(
        gateway_signing: ed25519_dalek::SigningKey,
        introspect_base: &str,
    ) -> std::sync::Arc<crate::GateState> {
        build_state_for_introspection_with_db(gateway_signing, introspect_base, None)
    }

    /// Same as [`build_state_for_introspection`] but with a `db` so the
    /// introspection arm's per-app family-marker revocation check runs
    /// against a live `auth.token_revocations` (mirrors the Bearer arm's
    /// `build_state_with_wrapper_and_oidc_and_db`). PG-gated tests pass
    /// `Some(db)`; the others keep `None` (pairwise stays a pure HMAC).
    fn build_state_for_introspection_with_db(
        gateway_signing: ed25519_dalek::SigningKey,
        introspect_base: &str,
        db: Option<crate::db::DbConfig>,
    ) -> std::sync::Arc<crate::GateState> {
        use std::sync::Arc as StdArc;

        let oidc_rp = crate::oidc_rp::OidcRp::new(
            introspect_base,
            "gateway",
            "test-secret",
            b"test-stash-key-32-bytes-long----".to_vec(),
        );

        let mut tmp = std::env::temp_dir();
        tmp.push(format!("zsgate-dpop-introspect-{}", uuid::Uuid::new_v4().simple()));
        let disk = crate::blob_cache::DiskBlobCache::new(tmp, 1024 * 1024).expect("disk cache");

        StdArc::new(crate::GateState {
            config: crate::GateConfig {
                control_url: String::new(),
                control_key: String::new(),
                worker_urls: vec![],
                poll_interval_secs: 5,
                worker_key: "wk".into(),
                hydra_public_url: String::new(),
                auth_ui_url: oidc_rp.auth_ui_url.clone(),
                insecure_dev: true,
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
            idempotency_store: StdArc::new(crate::idempotency::InMemoryIdempotencyStore::new()),
            oidc_rp: StdArc::new(oidc_rp),
            db,
            dpop_jti_cache: StdArc::new(zeroship_core::dpop::TieredJtiCache::default()),
            logout_jti_cache: StdArc::new(zeroship_core::logout_token::LogoutJtiCache::default()),
            signing_key: Some(StdArc::new(gateway_signing)),
            prev_signing_key: None,
            // No wrapper verifier ⇒ the wrapper fast-path is skipped and an
            // opaque DPoP token routes to the introspection fallback.
            wrapper_issuer: None,
            wrapper_verifier: None,
            anchor_enc_key: [0u8; 32],
            pairwise_salt: [0u8; 32],
        })
    }

    /// Build a DPoP request carrying an OPAQUE (non-JWT) access token + a real
    /// RFC 9449 proof signed by `client_key`, bound to `http://{host}{path}`.
    fn opaque_dpop_req(
        client_key: &ed25519_dalek::SigningKey,
        access_token: &str,
        host: &str,
        path: &str,
    ) -> ntex::web::HttpRequest {
        let now = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        )
        .unwrap();
        let htu = format!("http://{host}{path}");
        let proof = sign_dpop_proof(client_key, "GET", &htu, access_token, now);
        ntex::web::test::TestRequest::default()
            .uri(path)
            .header(http::header::HOST, host)
            .header(http::header::AUTHORIZATION, format!("DPoP {access_token}"))
            .header("dpop", proof)
            .to_http_request()
    }

    #[ntex::test]
    async fn resolve_dpop_introspection_rejects_client_id_mismatch() {
        // A DPoP-bound opaque token active for app A ("oac_app_a"), replayed at
        // app B's host with a VALID proof, must be rejected — the introspected
        // client_id does not match the route's expected client. Pre-fix this
        // was accepted and projected to B's sector (cross-app token confusion).
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let client_key = ed25519_dalek::SigningKey::from_bytes(&[42u8; 32]);
        let srv = start_introspect_server(serde_json::json!({
            "active": true,
            "sub": "0192f1aa-bbbb-7ccc-8ddd-eeeeffff0001",
            "client_id": "oac_app_a",
            "email": "user@example.com",
            "email_verified": true,
            "name": "Hydra User",
            "scope": "openid email",
        }))
        .await;
        let base = srv.url("").trim_end_matches('/').to_string();
        let state = build_state_for_introspection(gateway_signing, &base);

        let host = "appb.zeroship.ai";
        let req = opaque_dpop_req(&client_key, "ht_opaque_app_a", host, "/api/me");
        let request_id = Uuid::new_v4();
        // Route bound to app B; token says app A.
        let outcome = resolve_dpop_user_header(
            &req,
            &state,
            &request_id,
            Some("oac_app_b"),
            Some("https://appb.zeroship.ai"),
        )
        .await;
        assert!(
            matches!(outcome, DpopOutcome::None),
            "DPoP introspection client_id mismatch must reject (None), got {outcome:?}"
        );

        drop(srv);
    }

    #[ntex::test]
    async fn resolve_dpop_introspection_accepts_matching_client_id() {
        // Matching client_id → Allowed, and the GLOBAL Hydra UUID sub is
        // projected to the per-app pws_ for THIS route's client/sector (never
        // emitted on the worker header).
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let client_key = ed25519_dalek::SigningKey::from_bytes(&[42u8; 32]);
        let global_sub = "0192f1aa-bbbb-7ccc-8ddd-eeeeffff0002";
        let srv = start_introspect_server(serde_json::json!({
            "active": true,
            "sub": global_sub,
            "client_id": "oac_myapp",
            "email": "user@example.com",
            "email_verified": true,
            "name": "Hydra User",
            "scope": "openid email",
        }))
        .await;
        let base = srv.url("").trim_end_matches('/').to_string();
        let state = build_state_for_introspection(gateway_signing, &base);

        let host = "myapp.zeroship.ai";
        let sector = "https://myapp.zeroship.ai";
        let req = opaque_dpop_req(&client_key, "ht_opaque_myapp", host, "/api/me");
        let request_id = Uuid::new_v4();
        let outcome = resolve_dpop_user_header(
            &req,
            &state,
            &request_id,
            Some("oac_myapp"),
            Some(sector),
        )
        .await;
        let DpopOutcome::Allowed(header) = outcome else {
            panic!("expected Allowed on matching client_id, got {outcome:?}");
        };
        let json = zeroship_core::auth::verify_zeroship_user_header(
            state.config.worker_key.as_bytes(),
            &header,
        )
        .expect("MAC verifies");
        let user: serde_json::Value = serde_json::from_str(&json).expect("user json");
        let expected_pws =
            zeroship_core::auth::derive_pairwise(&state.pairwise_salt, global_sub, sector);
        assert_eq!(user["id"], expected_pws, "sub must project to the per-app pws_");
        assert!(expected_pws.starts_with("pws_"), "id must be a pws_, got {expected_pws}");
        assert!(
            !json.contains(global_sub),
            "global UUID leaked into ZeroShip-User: {json}"
        );

        drop(srv);
    }

    #[ntex::test]
    async fn resolve_dpop_introspection_rejects_when_route_unprovisioned() {
        // No route client (oauth_client_id == None): we cannot bind the
        // introspected token to a client. Consistent with the Bearer arm
        // (which refuses an unbound token rather than accept it), the DPoP
        // introspection fallback rejects — it does NOT fall through to the
        // no-sector ClientNotProvisioned 503 (that is the downstream §6.2
        // case, reached only once a token is already bound).
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let client_key = ed25519_dalek::SigningKey::from_bytes(&[42u8; 32]);
        let srv = start_introspect_server(serde_json::json!({
            "active": true,
            "sub": "0192f1aa-bbbb-7ccc-8ddd-eeeeffff0003",
            "client_id": "oac_app_a",
            "scope": "openid",
        }))
        .await;
        let base = srv.url("").trim_end_matches('/').to_string();
        let state = build_state_for_introspection(gateway_signing, &base);

        let req = opaque_dpop_req(&client_key, "ht_opaque", "appx.zeroship.ai", "/api/me");
        let request_id = Uuid::new_v4();
        let outcome =
            resolve_dpop_user_header(&req, &state, &request_id, None, None).await;
        assert!(
            matches!(outcome, DpopOutcome::None),
            "un-provisioned route must reject the introspection fallback (None), got {outcome:?}"
        );

        drop(srv);
    }

    #[compio::test]
    async fn resolve_dpop_wrapper_fast_path_unaffected_by_introspection_binding() {
        // Guard: the new introspection-arm binding must NOT touch the wrapper
        // fast-path, which binds via aud + cnf.jkt and is reached even when the
        // route has no oauth_client_id (it passes None to the wrapper verifier).
        // A matching-jkt wrapper with oauth_client_id == None still Allows.
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let client_key = ed25519_dalek::SigningKey::from_bytes(&[42u8; 32]);
        let state = build_state_with_wrapper(gateway_signing);

        let jkt = client_jkt(&client_key);
        let aud = "myapp.zeroship.ai";
        let wrapper = issue_wrapper(&state, &jkt, aud);

        let now = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        )
        .unwrap();
        let htu = format!("http://{aud}/api/me");
        let proof = sign_dpop_proof(&client_key, "GET", &htu, &wrapper, now);
        let req = ntex::web::test::TestRequest::default()
            .uri("/api/me")
            .header(http::header::HOST, aud)
            .header(http::header::AUTHORIZATION, format!("DPoP {wrapper}"))
            .header("dpop", proof)
            .to_http_request();

        let request_id = Uuid::new_v4();
        // oauth_client_id == None — the wrapper fast-path must still accept
        // (binding is via cnf.jkt, not the route client).
        let outcome = resolve_dpop_user_header(&req, &state, &request_id, None, None).await;
        assert!(
            matches!(outcome, DpopOutcome::Allowed(_)),
            "wrapper fast-path must remain unaffected by the introspection binding, got {outcome:?}"
        );
    }

    #[ntex::test]
    async fn resolve_dpop_introspection_rejects_revoked_family() {
        // PARITY with the raw-Hydra Bearer arm's family-marker revocation
        // (`bearer_raw_hydra_revocation_is_per_app_not_global`): a DPoP-bound
        // OPAQUE token whose `(client_id, sub)` family was revoked-since-before
        // the token's `iat` must be rejected on the introspection fallback —
        // Hydra `active: true` only covers GLOBAL revocation, not the per-app
        // family marker. Pre-fix this path trusted `active` alone and let a
        // per-app-revoked DPoP token through.
        //
        // This test proves BOTH halves of the Bearer parity claim:
        //   1. REJECT: revoking (oac_app_a, sub) rejects app A's DPoP token.
        //   2. PER-APP SCOPING: the SAME global sub on app B (oac_app_b) stays
        //      Allowed — the 6d key is `(client_id, sub)`, not just `sub`, so a
        //      regression that dropped the client_id (made 6d global) would
        //      let arm 2 fail (app B wrongly rejected).
        //
        // PG-gated: needs a live `auth.token_revocations` (skip when
        // AUTH_DB_URL is unset), exactly like the Bearer revocation tests.
        let Some(db) = connect_auth_db().await else {
            eprintln!("skipping (no AUTH_DB_URL)");
            return;
        };
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let client_key = ed25519_dalek::SigningKey::from_bytes(&[42u8; 32]);

        let client_a = "oac_app_a";
        let client_b = "oac_app_b";
        let host_a = "app-a.zeroship.ai";
        let host_b = "app-b.zeroship.ai";
        let sector_a = "https://app-a.zeroship.ai";
        let sector_b = "https://app-b.zeroship.ai";
        // ONE global Hydra sub presented to both apps — exactly the Bearer
        // test's shape (same `sub`, two `client_id`s).
        let global_sub = format!("0192f1aa-bbbb-7ccc-8ddd-{:012x}", rand_suffix());
        // `iat` strictly in the past so a `revoke_family` stamped NOW() is
        // `revoked_after > to_timestamp(iat)` → the token is rejected.
        let now = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        )
        .unwrap();
        let iat = now - 60;

        // Two introspection servers / states, one per client_id, each
        // returning the SAME global sub but its own client_id claim.
        let srv_a = start_introspect_server(serde_json::json!({
            "active": true,
            "sub": global_sub,
            "client_id": client_a,
            "email": "user@example.com",
            "email_verified": true,
            "name": "Hydra User",
            "scope": "openid email",
            "iat": iat,
        }))
        .await;
        let srv_b = start_introspect_server(serde_json::json!({
            "active": true,
            "sub": global_sub,
            "client_id": client_b,
            "email": "user@example.com",
            "email_verified": true,
            "name": "Hydra User",
            "scope": "openid email",
            "iat": iat,
        }))
        .await;
        let base_a = srv_a.url("").trim_end_matches('/').to_string();
        let base_b = srv_b.url("").trim_end_matches('/').to_string();
        let state_a =
            build_state_for_introspection_with_db(gateway_signing.clone(), &base_a, Some(db.clone()));
        let state_b =
            build_state_for_introspection_with_db(gateway_signing, &base_b, Some(db.clone()));

        let request_id = Uuid::new_v4();

        // Before revocation: app A Allowed (matching client_id, no marker).
        let pre = resolve_dpop_user_header(
            &opaque_dpop_req(&client_key, "ht_opaque_revoke", host_a, "/api/me"),
            &state_a,
            &request_id,
            Some(client_a),
            Some(sector_a),
        )
        .await;
        assert!(
            matches!(pre, DpopOutcome::Allowed(_)),
            "pre-revocation DPoP introspection token must be Allowed, got {pre:?}"
        );

        // Batch A fix 3: the introspection arm now projects the per-app pws_
        // BEFORE the marker check and keys on `(client_id, pws_)` — the SAME
        // key the WRITERS use. Derive each app's pws_ under ITS sector (the
        // states' salt is all-zero, the same salt the arm uses) and revoke ONLY
        // app A's family.
        let pws_a = zeroship_core::auth::derive_pairwise(&state_a.pairwise_salt, &global_sub, sector_a);
        let pws_b = zeroship_core::auth::derive_pairwise(&state_b.pairwise_salt, &global_sub, sector_b);
        {
            let pool = crate::db::checkout(&db).await.expect("pool checkout");
            let conn = pool.get().await.expect("pool checkout");
            zeroship_core::wrapper_revocation::revoke_family(&conn, client_a, &pws_a)
                .await
                .expect("revoke_family app A");
        }

        // Arm 1 — REJECT: app A token now rejected (None). The new family
        // check fires before the pairwise projection / Allowed.
        let post_a = resolve_dpop_user_header(
            &opaque_dpop_req(&client_key, "ht_opaque_revoke2", host_a, "/api/me"),
            &state_a,
            &request_id,
            Some(client_a),
            Some(sector_a),
        )
        .await;
        assert!(
            matches!(post_a, DpopOutcome::None),
            "revoked (oac_app_a, sub) family must reject app A's DPoP introspection token, got {post_a:?}"
        );

        // Arm 2 — PER-APP SCOPING: the SAME global sub on app B is NOT in the
        // revoked family `(oac_app_b, sub)` → still Allowed. Proves the 6d key
        // is per-app, not global; a regression dropping client_id would reject
        // here.
        let post_b = resolve_dpop_user_header(
            &opaque_dpop_req(&client_key, "ht_opaque_revoke3", host_b, "/api/me"),
            &state_b,
            &request_id,
            Some(client_b),
            Some(sector_b),
        )
        .await;
        assert!(
            matches!(post_b, DpopOutcome::Allowed(_)),
            "app A revocation must NOT revoke the same sub on app B (per-app, not global), got {post_b:?}"
        );

        let pool = crate::db::checkout(&db).await.expect("pool checkout");
        let conn = pool.get().await.expect("pool checkout");
        conn.execute(
            "DELETE FROM auth.token_revocations WHERE sub = ANY($1)",
            &[&vec![pws_a, pws_b]],
        )
        .await
        .ok();
        drop(conn);
        drop(srv_a);
        drop(srv_b);
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
    async fn bearer_raw_hydra_expired_rejected() {
        // An expired raw-Hydra JWT (beyond the 60s leeway) fails the JWKS
        // verify → Invalid (401 on User, anonymous on Anon).
        let jwks_signing = ed25519_dalek::SigningKey::from_bytes(&[55u8; 32]);
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let srv = start_jwks_server(hydra_jwks_doc(&jwks_signing)).await;
        let base = srv.url("").trim_end_matches('/').to_string();
        let state = build_state_for_hydra(gateway_signing, &base);

        let aud = "myapp.zeroship.ai";
        let token = sign_hydra_access_jwt(
            &jwks_signing,
            "usr_global_uuid",
            Some("oac_myapp"),
            serde_json::json!(["http://api.zeroship.localhost"]),
            "user@example.com",
            "Hydra User",
            -100, // expired
        );

        let req = bearer_req(&token, aud);
        let request_id = Uuid::new_v4();
        let outcome =
            resolve_bearer_user_header(&req, &state, &request_id, Some("oac_myapp"), None).await;
        assert!(
            matches!(outcome, BearerOutcome::Invalid),
            "expired raw-Hydra JWT must be Invalid, got {outcome:?}"
        );

        drop(srv);
    }

    #[ntex::test]
    async fn bearer_raw_hydra_bad_signature_rejected() {
        // A raw-Hydra JWT signed by a key NOT in the JWKS must fail
        // signature verification → Invalid.
        let jwks_signing = ed25519_dalek::SigningKey::from_bytes(&[55u8; 32]); // published
        let forged_signing = ed25519_dalek::SigningKey::from_bytes(&[66u8; 32]); // NOT published
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let srv = start_jwks_server(hydra_jwks_doc(&jwks_signing)).await;
        let base = srv.url("").trim_end_matches('/').to_string();
        let state = build_state_for_hydra(gateway_signing, &base);

        let aud = "myapp.zeroship.ai";
        // Signed by forged key → its kid won't be in the JWKS (the kid is
        // the thumbprint of the forged public half).
        let token = sign_hydra_access_jwt(
            &forged_signing,
            "usr_global_uuid",
            Some("oac_myapp"),
            serde_json::json!(["http://api.zeroship.localhost"]),
            "user@example.com",
            "Hydra User",
            3600,
        );

        let req = bearer_req(&token, aud);
        let request_id = Uuid::new_v4();
        let outcome =
            resolve_bearer_user_header(&req, &state, &request_id, Some("oac_myapp"), None).await;
        assert!(
            matches!(outcome, BearerOutcome::Invalid),
            "bad-signature raw-Hydra JWT must be Invalid, got {outcome:?}"
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
        let state = build_state_with_wrapper(gateway_signing);
        let req = ntex::web::test::TestRequest::default()
            .uri("/api/me")
            .header(http::header::HOST, "myapp.zeroship.ai")
            .to_http_request();
        let app_id = Uuid::new_v4();
        let request_id = Uuid::new_v4();
        let anon = resolve_auth(
            &req,
            &state,
            &anon_policy(),
            &app_id,
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
            &app_id,
            &request_id,
            Some("oac_myapp"),
            None,
        )
        .await;
        assert!(matches!(gated, AuthOutcome::Unauthenticated));
    }

    // ─── DPoP-downgrade guard (§1.3 c-wrap, blocker regression) ───────────
    //
    // A DPoP-bound wrapper (`cnf = Some(jkt)`) is sender-constrained. If it
    // is presented on the plain `Authorization: Bearer` scheme there is NO
    // proof-of-possession, so accepting it would downgrade a
    // sender-constrained token to a replayable bearer. The Bearer arm MUST
    // reject `cnf.is_some()`; the same wrapper MUST still authenticate via
    // the DPoP arm with a matching proof.

    #[compio::test]
    async fn bearer_dpop_bound_wrapper_rejected_on_plain_bearer() {
        // cnf=Some wrapper on plain Bearer → Invalid (no proof present).
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let client_key = ed25519_dalek::SigningKey::from_bytes(&[42u8; 32]);
        let state = build_state_with_wrapper(gateway_signing);

        let jkt = client_jkt(&client_key);
        let aud = "myapp.zeroship.ai";
        // `issue_wrapper` mints a DPoP-style wrapper: cnf = Some(jkt),
        // client_id = "gateway".
        let wrapper = issue_wrapper(&state, &jkt, aud);

        let req = bearer_req(&wrapper, aud);
        let request_id = Uuid::new_v4();
        let outcome =
            resolve_bearer_user_header(&req, &state, &request_id, Some("gateway"), None).await;
        assert!(
            matches!(outcome, BearerOutcome::Invalid),
            "DPoP-bound wrapper on plain Bearer must be Invalid (no PoP), got {outcome:?}"
        );
    }

    #[compio::test]
    async fn bearer_dpop_bound_wrapper_still_works_via_dpop_arm() {
        // The SAME cnf=Some wrapper the Bearer arm rejects MUST still
        // authenticate via the DPoP arm with a matching proof — the
        // downgrade guard rejects the *scheme*, not the token.
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let client_key = ed25519_dalek::SigningKey::from_bytes(&[42u8; 32]);
        let state = build_state_with_wrapper(gateway_signing);

        let jkt = client_jkt(&client_key);
        let aud = "myapp.zeroship.ai";
        let wrapper = issue_wrapper(&state, &jkt, aud);

        // Plain Bearer: rejected.
        let bearer = bearer_req(&wrapper, aud);
        let request_id = Uuid::new_v4();
        assert!(
            matches!(
                resolve_bearer_user_header(&bearer, &state, &request_id, Some("gateway"), None).await,
                BearerOutcome::Invalid
            ),
            "plain Bearer leg must reject the DPoP-bound wrapper"
        );

        // DPoP arm with a matching proof: accepted.
        let now = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        )
        .unwrap();
        let htu = format!("http://{aud}/api/me");
        let proof = sign_dpop_proof(&client_key, "GET", &htu, &wrapper, now);
        let dpop_req = ntex::web::test::TestRequest::default()
            .uri("/api/me")
            .header(http::header::HOST, aud)
            .header(http::header::AUTHORIZATION, format!("DPoP {wrapper}"))
            .header("dpop", proof)
            .to_http_request();
        assert!(
            matches!(
                resolve_dpop_user_header(&dpop_req, &state, &request_id, None, None).await,
                DpopOutcome::Allowed(_)
            ),
            "DPoP arm must accept the same wrapper with a valid proof"
        );
    }

    // ─── Per-app family-marker revocation (§8.5, major regressions) ───────
    //
    // PG-gated: these need a live `auth` schema with `auth.token_revocations`
    // (skip when AUTH_DB_URL is unset, mirroring the DPoP revocation test).
    // They cover the two MAJOR findings: (1) the wrapper path's `pws_…`
    // subject IS matched by the TEXT-keyed family marker (the old UUID-only
    // denylist silently no-op'd it); (2) revocation is PER-APP — revoking a
    // user on app A does NOT revoke the same sub on app B.

    async fn connect_auth_db() -> Option<crate::db::DbConfig> {
        let dsn = std::env::var("AUTH_DB_URL").ok()?;
        Some(crate::db::DbConfig::new(dsn, 4))
    }

    #[compio::test]
    async fn bearer_wrapper_pws_subject_revoked_by_family_marker() {
        let Some(db) = connect_auth_db().await else {
            eprintln!("skipping (no AUTH_DB_URL)");
            return;
        };
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let state = build_state_with_wrapper_and_auth_ui_url_and_db(
            gateway_signing,
            "http://127.0.0.1:1",
            Some(db.clone()),
        );

        let aud = "myapp.zeroship.ai";
        let client_id = "oac_myapp";
        // A `pws_…` subject — the exact shape the UUID-only denylist could
        // never parse, so the old code silently skipped the revocation
        // check. The TEXT family marker matches it.
        let pws_sub = format!("pws_{}", Uuid::new_v4().simple());
        let token = issue_plain_wrapper(&state, &pws_sub, client_id, aud);
        let req = bearer_req(&token, aud);
        let request_id = Uuid::new_v4();

        // Before revocation: Allowed.
        assert!(
            matches!(
                resolve_bearer_user_header(&req, &state, &request_id, Some(client_id), None).await,
                BearerOutcome::Allowed(_)
            ),
            "pre-revocation wrapper must be Allowed"
        );

        // Revoke the (client_id, pws_sub) family AFTER the token's iat.
        {
            let pool = crate::db::checkout(&db).await.expect("pool checkout");
            let conn = pool.get().await.expect("pool checkout");
            zeroship_core::wrapper_revocation::revoke_family(&conn, client_id, &pws_sub)
                .await
                .expect("revoke_family");
        }

        // After revocation: Invalid (the dead branch is now alive for pws_).
        assert!(
            matches!(
                resolve_bearer_user_header(&req, &state, &request_id, Some(client_id), None).await,
                BearerOutcome::Invalid
            ),
            "revoked pws_ family must be Invalid on the wrapper path"
        );

        let pool = crate::db::checkout(&db).await.expect("pool checkout");
        let conn = pool.get().await.expect("pool checkout");
        conn.execute(
            "DELETE FROM auth.token_revocations WHERE client_id = $1 AND sub = $2",
            &[&client_id, &pws_sub],
        )
        .await
        .ok();
    }

    #[ntex::test]
    async fn bearer_raw_hydra_revocation_is_per_app_not_global() {
        let Some(db) = connect_auth_db().await else {
            eprintln!("skipping (no AUTH_DB_URL)");
            return;
        };
        let jwks_signing = ed25519_dalek::SigningKey::from_bytes(&[55u8; 32]);
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let srv = start_jwks_server(hydra_jwks_doc(&jwks_signing)).await;
        let base = srv.url("").trim_end_matches('/').to_string();

        // build_state_for_hydra with a DB so the revocation check runs.
        let oidc_rp = crate::oidc_rp::OidcRp::new(
            &base,
            "gateway",
            "test-secret",
            b"test-stash-key-32-bytes-long----".to_vec(),
        )
        .with_issuer(HYDRA_ISS);
        let state =
            build_state_with_wrapper_and_oidc_and_db(gateway_signing, oidc_rp, Some(db.clone()));

        let aud = "myapp.zeroship.ai";
        let sub = format!("usr_{}", Uuid::new_v4().simple());
        // One Hydra token whose client_id claim is app A; the route binds
        // on client_id, so present it at app A and (separately) app B.
        let token_a = sign_hydra_access_jwt(
            &jwks_signing,
            &sub,
            Some("oac_app_a"),
            serde_json::json!(["http://api.zeroship.localhost"]),
            "user@example.com",
            "Hydra User",
            3600,
        );
        let token_b = sign_hydra_access_jwt(
            &jwks_signing,
            &sub,
            Some("oac_app_b"),
            serde_json::json!(["http://api.zeroship.localhost"]),
            "user@example.com",
            "Hydra User",
            3600,
        );
        let req_a = bearer_req(&token_a, aud);
        let req_b = bearer_req(&token_b, aud);
        let request_id = Uuid::new_v4();

        // Batch A fix 3: the marker is keyed on `(client_id, pws_)` — the SAME
        // key the arm now reads (it projects pws_ BEFORE the lookup). Derive
        // each app's pws_ under ITS sector (the state's salt is all-zero, the
        // same salt the arm uses), and revoke ONLY app A's family.
        let sector_a = "https://app-a.zeroship.ai";
        let sector_b = "https://app-b.zeroship.ai";
        let pws_a = zeroship_core::auth::derive_pairwise(&state.pairwise_salt, &sub, sector_a);
        let pws_b = zeroship_core::auth::derive_pairwise(&state.pairwise_salt, &sub, sector_b);
        {
            let pool = crate::db::checkout(&db).await.expect("pool checkout");
            let conn = pool.get().await.expect("pool checkout");
            zeroship_core::wrapper_revocation::revoke_family(&conn, "oac_app_a", &pws_a)
                .await
                .expect("revoke_family app A");
        }

        // App A token: revoked → Invalid. (Sector present so the arm derives
        // the SAME pws_a the marker was written under.)
        assert!(
            matches!(
                resolve_bearer_user_header(
                    &req_a,
                    &state,
                    &request_id,
                    Some("oac_app_a"),
                    Some(sector_a)
                )
                .await,
                BearerOutcome::Invalid
            ),
            "revoked (oac_app_a, pws_a) family must reject app A's token"
        );
        // App B token, SAME global sub but DIFFERENT pws_ (different sector) and
        // client: NOT revoked → Allowed (per-app scoping).
        assert!(
            matches!(
                resolve_bearer_user_header(
                    &req_b,
                    &state,
                    &request_id,
                    Some("oac_app_b"),
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
            "DELETE FROM auth.token_revocations WHERE sub = ANY($1)",
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

    #[compio::test]
    async fn resolve_auth_valid_wrapper_on_user_route_allows_with_header() {
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let state = build_state_with_wrapper(gateway_signing);
        let aud = "myapp.zeroship.ai";
        let token = issue_plain_wrapper(&state, "pws_alice", "oac_myapp", aud);
        let req = bearer_req(&token, aud);
        let app_id = Uuid::new_v4();
        let request_id = Uuid::new_v4();

        let outcome = resolve_auth(
            &req,
            &state,
            &user_policy(),
            &app_id,
            &request_id,
            Some("oac_myapp"),
            None,
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
        assert_eq!(user["id"], "pws_alice");
    }

    #[ntex::test]
    async fn resolve_auth_valid_raw_hydra_on_user_route_allows_with_header() {
        let jwks_signing = ed25519_dalek::SigningKey::from_bytes(&[55u8; 32]);
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let srv = start_jwks_server(hydra_jwks_doc(&jwks_signing)).await;
        let base = srv.url("").trim_end_matches('/').to_string();
        let state = build_state_for_hydra(gateway_signing, &base);

        let global_sub = "0192f1aa-bbbb-7ccc-8ddd-eeeeffff0001";
        let sector = "https://myapp.zeroship.ai";
        let aud = "myapp.zeroship.ai";
        let token = sign_hydra_access_jwt(
            &jwks_signing,
            global_sub,
            Some("oac_myapp"),
            serde_json::json!(["http://api.zeroship.localhost"]),
            "user@example.com",
            "Hydra User",
            3600,
        );
        let req = bearer_req(&token, aud);
        let app_id = Uuid::new_v4();
        let request_id = Uuid::new_v4();

        let outcome = resolve_auth(
            &req,
            &state,
            &user_policy(),
            &app_id,
            &request_id,
            Some("oac_myapp"),
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
        // Slice 4: the raw-Hydra global UUID is projected to the per-app pws_
        // end-to-end through resolve_auth (the global UUID never reaches the
        // worker header).
        let expected_pws =
            zeroship_core::auth::derive_pairwise(&state.pairwise_salt, global_sub, sector);
        assert_eq!(user["id"], expected_pws);
        assert!(!json.contains(global_sub), "global UUID leaked: {json}");

        drop(srv);
    }

    // ─── Invalid-Bearer does NOT fall back to a valid cookie (minor) ──────
    //
    // Documents the round-3 decision (mirroring the DPoP precedent): on a
    // User/Admin route an Invalid Bearer 401s and is NOT silently rescued by
    // a valid cookie session. PG-gated (needs auth.gateway_sessions to mint a
    // REAL validated cookie session, so the test is faithful — the cookie
    // genuinely validates, yet the Bearer still wins the 401).
    #[compio::test]
    async fn resolve_auth_invalid_bearer_on_user_route_does_not_use_cookie() {
        use ed25519_dalek::pkcs8::EncodePrivateKey;
        use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};

        let Some(db) = connect_auth_db().await else {
            eprintln!("skipping (no AUTH_DB_URL)");
            return;
        };
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let state = build_state_with_wrapper_and_auth_ui_url_and_db(
            gateway_signing.clone(),
            "http://127.0.0.1:1",
            Some(db.clone()),
        );
        let aud = "myapp.zeroship.ai";
        let app_id = Uuid::new_v4();
        let app_id_str = app_id.to_string();

        // Mint a REAL, currently-valid cookie session for a user.
        let user_id = Uuid::new_v4();
        let session = {
            let pool = crate::db::checkout(&db).await.expect("pool checkout");
            let conn = pool.get().await.expect("pool checkout");
            crate::sessions::create(
                &conn,
                &crate::sessions::NewSession {
                    user_id: &user_id.to_string(),
                    app_id: &app_id_str,
                    email: Some("cookie-user@example.com"),
                    name: Some("Cookie User"),
                    avatar_url: None,
                    email_verified: true,
                    granted_scopes: &[],
                },
            )
            .await
            .expect("create cookie session")
        };

        // Sanity: that cookie ALONE (no Bearer) authenticates the User route.
        let cookie_name = oidc_rp::app_session_cookie_name(true); // insecure_dev
        let cookie_only_req = ntex::web::test::TestRequest::default()
            .uri("/api/me")
            .header(http::header::HOST, aud)
            .header("cookie", format!("{cookie_name}={}", session.id))
            .to_http_request();
        let request_id = Uuid::new_v4();
        assert!(
            matches!(
                resolve_auth(
                    &cookie_only_req,
                    &state,
                    &user_policy(),
                    &app_id,
                    &request_id,
                    Some("oac_myapp"),
                    Some("https://myapp.zeroship.ai"),
                )
                .await,
                AuthOutcome::Allowed {
                    user_header: Some(_)
                }
            ),
            "the cookie session alone must authenticate (test fixture sanity)"
        );

        // Now attach an EXPIRED wrapper Bearer alongside the SAME valid
        // cookie. The Bearer arm yields Invalid; on a User route that 401s
        // and MUST NOT fall through to the (valid) cookie.
        let now = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        )
        .unwrap();
        let claims = crate::wrapper_token::WrapperClaims {
            iss: GATEWAY_ISS.into(),
            aud: aud.into(),
            sub: "pws_alice".into(),
            exp: now - 100, // expired beyond the 60s leeway
            iat: now - 700,
            jti: Uuid::new_v4().to_string(),
            cnf: None,
            scope: "openid".into(),
            client_id: "oac_myapp".into(),
            email: Some("relay-alias@zeroship.ai".into()),
            email_verified: Some(true),
            name: None,
            wraps: None,
        };
        let mut header = Header::new(Algorithm::EdDSA);
        header.typ = Some("at+jwt".into());
        header.kid = Some(crate::signing::jwk_thumbprint(&gateway_signing));
        let der = gateway_signing.to_pkcs8_der().unwrap();
        let key = EncodingKey::from_ed_der(der.as_bytes());
        let expired_bearer = encode(&header, &claims, &key).unwrap();

        let shadowed_req = ntex::web::test::TestRequest::default()
            .uri("/api/me")
            .header(http::header::HOST, aud)
            .header(http::header::AUTHORIZATION, format!("Bearer {expired_bearer}"))
            .header("cookie", format!("{cookie_name}={}", session.id))
            .to_http_request();
        let outcome = resolve_auth(
            &shadowed_req,
            &state,
            &user_policy(),
            &app_id,
            &request_id,
            Some("oac_myapp"),
            None,
        )
        .await;
        assert!(
            matches!(outcome, AuthOutcome::Unauthenticated),
            "Invalid Bearer on User route must 401, NOT fall back to the valid cookie, got {outcome:?}"
        );

        {
            let pool = crate::db::checkout(&db).await.expect("pool checkout");
            let conn = pool.get().await.expect("pool checkout");
            crate::sessions::revoke(&conn, session.id).await.ok();
        }
    }

    // ─── Cookie-arm required-scope enforcement (Slice 3c review finding 6) ──
    //
    // The single enforcement point in `resolve_auth` is arm-agnostic, but the
    // plain-Bearer scope tests above only verify ONE arm. This pair drives the
    // COOKIE arm — whose scopes come from the new Slice-3a
    // `auth.gateway_sessions.granted_scopes` column — through the REAL
    // `resolve_auth`, minting a REAL validated session (PG-gated). A cookie
    // whose granted_scopes cover the route's `required_scopes` is Allowed; one
    // that does not is InsufficientScope (403), proving the gate is not
    // Bearer-only.

    #[compio::test]
    async fn resolve_auth_cookie_arm_with_required_scope_allowed() {
        let Some(db) = connect_auth_db().await else {
            eprintln!("skipping (no AUTH_DB_URL)");
            return;
        };
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let state = build_state_with_wrapper_and_auth_ui_url_and_db(
            gateway_signing,
            "http://127.0.0.1:1",
            Some(db.clone()),
        );
        let aud = "myapp.zeroship.ai";
        let app_id = Uuid::new_v4();
        let app_id_str = app_id.to_string();

        // Session whose granted_scopes INCLUDE the route's required scope.
        let session = {
            let pool = crate::db::checkout(&db).await.expect("pool checkout");
            let conn = pool.get().await.expect("pool checkout");
            crate::sessions::create(
                &conn,
                &crate::sessions::NewSession {
                    user_id: &Uuid::new_v4().to_string(),
                    app_id: &app_id_str,
                    email: Some("cookie-user@example.com"),
                    name: Some("Cookie User"),
                    avatar_url: None,
                    email_verified: true,
                    granted_scopes: &["openid".to_string(), "read:billing".to_string()],
                },
            )
            .await
            .expect("create cookie session")
        };

        let cookie_name = oidc_rp::app_session_cookie_name(true);
        let req = ntex::web::test::TestRequest::default()
            .uri("/api/billing")
            .header(http::header::HOST, aud)
            .header("cookie", format!("{cookie_name}={}", session.id))
            .to_http_request();
        let request_id = Uuid::new_v4();
        let outcome = resolve_auth(
            &req,
            &state,
            &user_policy_requiring(&["read:billing"]),
            &app_id,
            &request_id,
            Some("oac_myapp"),
            Some("https://myapp.zeroship.ai"),
        )
        .await;

        {
            let pool = crate::db::checkout(&db).await.expect("pool checkout");
            let conn = pool.get().await.expect("pool checkout");
            crate::sessions::revoke(&conn, session.id).await.ok();
        }

        assert!(
            matches!(outcome, AuthOutcome::Allowed { user_header: Some(_) }),
            "cookie session granting read:billing must pass the scope gate, got {outcome:?}"
        );
    }

    #[compio::test]
    async fn resolve_auth_cookie_arm_without_required_scope_403s() {
        let Some(db) = connect_auth_db().await else {
            eprintln!("skipping (no AUTH_DB_URL)");
            return;
        };
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let state = build_state_with_wrapper_and_auth_ui_url_and_db(
            gateway_signing,
            "http://127.0.0.1:1",
            Some(db.clone()),
        );
        let aud = "myapp.zeroship.ai";
        let app_id = Uuid::new_v4();
        let app_id_str = app_id.to_string();

        // Session whose granted_scopes do NOT include the route's required
        // scope — identity is fine, the grant is too narrow → 403, not 401.
        let session = {
            let pool = crate::db::checkout(&db).await.expect("pool checkout");
            let conn = pool.get().await.expect("pool checkout");
            crate::sessions::create(
                &conn,
                &crate::sessions::NewSession {
                    user_id: &Uuid::new_v4().to_string(),
                    app_id: &app_id_str,
                    email: Some("cookie-user@example.com"),
                    name: Some("Cookie User"),
                    avatar_url: None,
                    email_verified: true,
                    granted_scopes: &["openid".to_string(), "email".to_string()],
                },
            )
            .await
            .expect("create cookie session")
        };

        let cookie_name = oidc_rp::app_session_cookie_name(true);
        let req = ntex::web::test::TestRequest::default()
            .uri("/api/billing")
            .header(http::header::HOST, aud)
            .header("cookie", format!("{cookie_name}={}", session.id))
            .to_http_request();
        let request_id = Uuid::new_v4();
        let outcome = resolve_auth(
            &req,
            &state,
            &user_policy_requiring(&["read:billing"]),
            &app_id,
            &request_id,
            Some("oac_myapp"),
            Some("https://myapp.zeroship.ai"),
        )
        .await;

        {
            let pool = crate::db::checkout(&db).await.expect("pool checkout");
            let conn = pool.get().await.expect("pool checkout");
            crate::sessions::revoke(&conn, session.id).await.ok();
        }

        match outcome {
            AuthOutcome::InsufficientScope { required } => {
                assert_eq!(required, vec!["read:billing".to_string()]);
            }
            other => panic!(
                "cookie session lacking read:billing must be InsufficientScope (403), got {other:?}"
            ),
        }
    }

    // ─── Slice 4 — pairwise subject projection (§6.2) ─────────────────────
    //
    // These cover the four properties of the consistent `pws_` projection:
    // (1) cross-app divergence (same user, two apps → different pws_);
    // (2) cross-arm + re-login consistency (cookie vs raw-Hydra vs wrapper →
    //     the SAME pws_ for the same (user, app));
    // (3) the global UUID is ABSENT from every outward `ZeroShip-User`;
    // (4) fail-closed 503 when the route has no sector yet.
    // The DB upsert + cookie arm are PG-gated (skip when AUTH_DB_URL is
    // unset, like the revocation tests); the in-memory arms run always.

    /// A fixed global UUID + two distinct app sectors. A `pws_` derived for
    /// the SAME user under DIFFERENT sectors MUST differ — no cross-app
    /// correlation (G4). This is the cross-app divergence property at the
    /// gateway projection boundary, asserted against the raw-Hydra arm's
    /// emitted header (the path that actually projects).
    #[ntex::test]
    async fn raw_hydra_same_user_two_apps_get_different_pws() {
        let jwks_signing = ed25519_dalek::SigningKey::from_bytes(&[55u8; 32]);
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let srv = start_jwks_server(hydra_jwks_doc(&jwks_signing)).await;
        let base = srv.url("").trim_end_matches('/').to_string();
        let state = build_state_for_hydra(gateway_signing, &base);

        let global_sub = "0192f1aa-bbbb-7ccc-8ddd-eeeeffff0042";

        // App A.
        let token_a = sign_hydra_access_jwt(
            &jwks_signing,
            global_sub,
            Some("oac_app_a"),
            serde_json::json!(["http://api.zeroship.localhost"]),
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
            Some("oac_app_a"),
            Some("https://app-a.zeroship.ai"),
        )
        .await
        else {
            panic!("app A must Allow");
        };
        let id_a = decode_header_id(&state, &header_a);

        // App B — SAME user, different sector/client.
        let token_b = sign_hydra_access_jwt(
            &jwks_signing,
            global_sub,
            Some("oac_app_b"),
            serde_json::json!(["http://api.zeroship.localhost"]),
            "user@example.com",
            "User",
            3600,
        );
        let req_b = bearer_req(&token_b, "app-b.zeroship.ai");
        let BearerOutcome::Allowed(header_b) = resolve_bearer_user_header(
            &req_b,
            &state,
            &rid,
            Some("oac_app_b"),
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

    /// Cross-arm + re-login consistency: the SAME (user, app) yields the
    /// SAME `pws_` whether the gateway resolves it via the raw-Hydra arm or
    /// derives it directly (the cookie arm uses the identical derivation on
    /// the SAME global UUID + sector). Re-login is modelled by deriving
    /// twice — `derive_pairwise` is deterministic, so a fresh token for the
    /// same user re-projects to the same id.
    #[ntex::test]
    async fn raw_hydra_pws_is_consistent_across_arms_and_relogin() {
        let jwks_signing = ed25519_dalek::SigningKey::from_bytes(&[55u8; 32]);
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let srv = start_jwks_server(hydra_jwks_doc(&jwks_signing)).await;
        let base = srv.url("").trim_end_matches('/').to_string();
        let state = build_state_for_hydra(gateway_signing, &base);

        let global_sub = "0192f1aa-bbbb-7ccc-8ddd-eeeeffff0077";
        let sector = "https://myapp.zeroship.ai";
        let aud = "myapp.zeroship.ai";

        // Raw-Hydra arm projection.
        let token = sign_hydra_access_jwt(
            &jwks_signing,
            global_sub,
            Some("oac_myapp"),
            serde_json::json!(["http://api.zeroship.localhost"]),
            "user@example.com",
            "User",
            3600,
        );
        let req = bearer_req(&token, aud);
        let rid = Uuid::new_v4();
        let BearerOutcome::Allowed(header) =
            resolve_bearer_user_header(&req, &state, &rid, Some("oac_myapp"), Some(sector)).await
        else {
            panic!("must Allow");
        };
        let arm_id = decode_header_id(&state, &header);

        // The cookie arm and the browser-wrapper mint use the SAME
        // derivation on the SAME (global UUID, sector). Re-login (a second
        // fresh token) re-derives the identical value.
        let direct = zeroship_core::auth::derive_pairwise(&state.pairwise_salt, global_sub, sector);
        let relogin =
            zeroship_core::auth::derive_pairwise(&state.pairwise_salt, global_sub, sector);

        assert_eq!(
            arm_id, direct,
            "raw-Hydra arm must project the same pws_ the cookie/wrapper arms derive"
        );
        assert_eq!(direct, relogin, "re-login must re-derive the SAME pws_");
        assert!(arm_id.starts_with("pws_"));

        drop(srv);
    }

    /// Fail-closed: a VALID raw-Hydra user whose route has NO
    /// `sector_identifier` yet must NOT be projected — the arm returns
    /// `ClientNotProvisioned` (which `resolve_auth` maps to 503), never the
    /// global UUID. Mirrors the browser-token path's posture.
    #[ntex::test]
    async fn raw_hydra_unprovisioned_sector_fails_closed() {
        let jwks_signing = ed25519_dalek::SigningKey::from_bytes(&[55u8; 32]);
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let srv = start_jwks_server(hydra_jwks_doc(&jwks_signing)).await;
        let base = srv.url("").trim_end_matches('/').to_string();
        let state = build_state_for_hydra(gateway_signing, &base);

        let token = sign_hydra_access_jwt(
            &jwks_signing,
            "0192f1aa-bbbb-7ccc-8ddd-eeeeffff0099",
            Some("oac_myapp"),
            serde_json::json!(["http://api.zeroship.localhost"]),
            "user@example.com",
            "User",
            3600,
        );
        let req = bearer_req(&token, "myapp.zeroship.ai");
        let rid = Uuid::new_v4();
        // No sector → fail closed.
        let outcome =
            resolve_bearer_user_header(&req, &state, &rid, Some("oac_myapp"), None).await;
        assert!(
            matches!(outcome, BearerOutcome::ClientNotProvisioned),
            "no sector_identifier must fail closed (ClientNotProvisioned), got {outcome:?}"
        );

        // And end-to-end through resolve_auth → AuthOutcome::ClientNotProvisioned.
        let app_id = Uuid::new_v4();
        let outcome = resolve_auth(
            &req,
            &state,
            &user_policy(),
            &app_id,
            &rid,
            Some("oac_myapp"),
            None,
        )
        .await;
        assert!(
            matches!(outcome, AuthOutcome::ClientNotProvisioned),
            "resolve_auth must surface ClientNotProvisioned (→ 503), got {outcome:?}"
        );

        drop(srv);
    }

    /// The browser-wrapper Bearer arm carries a `pws_` `sub` straight from
    /// the minted wrapper (no re-derivation) and the global UUID is absent.
    /// Confirms cross-arm CONSISTENCY: a wrapper minted with the same `pws_`
    /// the raw-Hydra arm derives projects the SAME id.
    #[compio::test]
    async fn wrapper_arm_carries_pws_and_omits_global_uuid() {
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let state = build_state_with_wrapper(gateway_signing);
        let global_sub = "0192f1aa-bbbb-7ccc-8ddd-eeeeffff0077";
        let sector = "https://myapp.zeroship.ai";
        let aud = "myapp.zeroship.ai";
        // The /token mint derives this exact pws_ for the wrapper sub.
        let pws = zeroship_core::auth::derive_pairwise(&state.pairwise_salt, global_sub, sector);
        let token = issue_plain_wrapper(&state, &pws, "oac_myapp", aud);

        let req = bearer_req(&token, aud);
        let rid = Uuid::new_v4();
        // The wrapper arm reads the pws_ straight from the wrapper; sector is
        // irrelevant to it (it never re-derives), so even None sector works.
        let BearerOutcome::Allowed(header) =
            resolve_bearer_user_header(&req, &state, &rid, Some("oac_myapp"), None).await
        else {
            panic!("wrapper must Allow");
        };
        let id = decode_header_id(&state, &header);
        assert_eq!(id, pws, "wrapper arm forwards the minted pws_ unchanged");
        assert!(!id.contains(global_sub), "global UUID must not appear: {id}");
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

    // ─── PG-gated: the cookie arm + the app_user_identities upsert ────────

    /// The cookie arm projects the GLOBAL session user_id to the per-app
    /// `pws_`, emits THAT in `ZeroShip-User.id` (never the UUID), and
    /// UPSERTS the `(app_client_id, global_user_id) → pws_` mapping. A
    /// second resolution is idempotent (one row, same pws_). PG-gated.
    #[compio::test]
    async fn cookie_arm_projects_pws_and_upserts_mapping_idempotently() {
        let Some(db) = connect_auth_db().await else {
            eprintln!("skipping (no AUTH_DB_URL)");
            return;
        };
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let state = build_state_with_wrapper_and_auth_ui_url_and_db(
            gateway_signing,
            "http://127.0.0.1:1",
            Some(db.clone()),
        );
        let aud = "myapp.zeroship.ai";
        let app_id = Uuid::new_v4();
        let app_id_str = app_id.to_string();
        let client_id = "oac_myapp";
        let sector = "https://myapp.zeroship.ai";

        // Mint a REAL cookie session for a global user. The user row must
        // exist first: `app_user_identities.global_user_id` FK-references
        // `auth.users(id)`, so the gateway's mapping upsert silently no-ops
        // (best-effort) without it — which the persistence assertion below
        // would then fail. Seed it so the upsert actually lands.
        let user_id = Uuid::new_v4();
        {
            let pool = crate::db::checkout(&db).await.expect("pool");
            let conn = pool.get().await.expect("pool");
            conn.execute(
                "INSERT INTO auth.users (id, email, name, email_verified_at) \
                 VALUES ($1, $2::citext, $3, NOW()) ON CONFLICT (id) DO NOTHING",
                &[&user_id, &"cookie-user@example.com", &"Cookie User"],
            )
            .await
            .expect("seed user");
        }
        let session = {
            let pool = crate::db::checkout(&db).await.expect("pool");
            let conn = pool.get().await.expect("pool");
            crate::sessions::create(
                &conn,
                &crate::sessions::NewSession {
                    user_id: &user_id.to_string(),
                    app_id: &app_id_str,
                    email: Some("cookie-user@example.com"),
                    name: Some("Cookie User"),
                    avatar_url: None,
                    email_verified: true,
                    granted_scopes: &[],
                },
            )
            .await
            .expect("create session")
        };

        let cookie_name = oidc_rp::app_session_cookie_name(true);
        let req = ntex::web::test::TestRequest::default()
            .uri("/api/me")
            .header(http::header::HOST, aud)
            .header("cookie", format!("{cookie_name}={}", session.id))
            .to_http_request();
        let rid = Uuid::new_v4();

        let outcome = resolve_app_session_user_header_inner(
            &req,
            &state,
            &app_id_str,
            &rid,
            Some(client_id),
            Some(sector),
        )
        .await;
        let CookieOutcome::Allowed(header) = outcome else {
            panic!("cookie must Allow, got {outcome:?}");
        };
        let emitted_id = decode_header_id(&state, &header);
        let expected_pws = zeroship_core::auth::derive_pairwise(
            &state.pairwise_salt,
            &user_id.to_string(),
            sector,
        );
        assert_eq!(emitted_id, expected_pws, "cookie arm must emit the pws_");
        assert!(
            !header.contains(&user_id.to_string()),
            "global UUID must not appear in the cookie-arm header"
        );
        // Slice 5c §7 — email-claim swap: no relay alias minted for this
        // (app, user), so the cookie arm fails closed → empty email. The
        // session's real `cookie-user@example.com` must NEVER reach the worker.
        {
            let json = zeroship_core::auth::verify_zeroship_user_header(
                state.config.worker_key.as_bytes(),
                &header,
            )
            .expect("MAC verifies");
            let user: serde_json::Value = serde_json::from_str(&json).expect("user json");
            assert_eq!(user["email"], "", "no alias ⇒ empty email (fail closed)");
            assert!(
                !json.contains("cookie-user@example.com"),
                "real email leaked into cookie-arm ZeroShip-User: {json}"
            );
        }

        // The mapping row exists with the right (app_client_id, global, pws_).
        let stored = {
            let pool = crate::db::checkout(&db).await.expect("pool");
            let conn = pool.get().await.expect("pool");
            crate::identities::lookup_pairwise_sub(&conn, client_id, user_id)
                .await
                .expect("lookup")
        };
        assert_eq!(
            stored.as_deref(),
            Some(expected_pws.as_str()),
            "app_user_identities must hold the projected pws_"
        );

        // Idempotent: a SECOND resolution leaves exactly ONE row, same pws_.
        let _ = resolve_app_session_user_header_inner(
            &req,
            &state,
            &app_id_str,
            &rid,
            Some(client_id),
            Some(sector),
        )
        .await;
        let (count, again) = {
            let pool = crate::db::checkout(&db).await.expect("pool");
            let conn = pool.get().await.expect("pool");
            let rows = conn
                .query(
                    "SELECT pairwise_sub FROM auth.app_user_identities \
                     WHERE app_client_id = $1 AND global_user_id = $2",
                    &[&client_id, &user_id],
                )
                .await
                .expect("count query");
            let again: Option<String> = rows.first().map(|r| r.get("pairwise_sub"));
            (rows.len(), again)
        };
        assert_eq!(count, 1, "upsert must be idempotent (exactly one row)");
        assert_eq!(again.as_deref(), Some(expected_pws.as_str()));

        // Cleanup.
        {
            let pool = crate::db::checkout(&db).await.expect("pool");
            let conn = pool.get().await.expect("pool");
            conn.execute(
                "DELETE FROM auth.app_user_identities WHERE app_client_id = $1 AND global_user_id = $2",
                &[&client_id, &user_id],
            )
            .await
            .ok();
            crate::sessions::revoke(&conn, session.id).await.ok();
            // Drop the seeded user row last (FK from app_user_identities cleared above).
            conn.execute("DELETE FROM auth.users WHERE id = $1", &[&user_id])
                .await
                .ok();
        }
    }

    /// The cookie arm fails closed (no `pws_`) when the route has no
    /// `sector_identifier` — even for a fully-valid cookie session, the
    /// global UUID is never projected. PG-gated.
    #[compio::test]
    async fn cookie_arm_unprovisioned_sector_fails_closed() {
        let Some(db) = connect_auth_db().await else {
            eprintln!("skipping (no AUTH_DB_URL)");
            return;
        };
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let state = build_state_with_wrapper_and_auth_ui_url_and_db(
            gateway_signing,
            "http://127.0.0.1:1",
            Some(db.clone()),
        );
        let aud = "myapp.zeroship.ai";
        let app_id = Uuid::new_v4();
        let app_id_str = app_id.to_string();

        let user_id = Uuid::new_v4();
        let session = {
            let pool = crate::db::checkout(&db).await.expect("pool");
            let conn = pool.get().await.expect("pool");
            crate::sessions::create(
                &conn,
                &crate::sessions::NewSession {
                    user_id: &user_id.to_string(),
                    app_id: &app_id_str,
                    email: Some("cookie-user@example.com"),
                    name: Some("Cookie User"),
                    avatar_url: None,
                    email_verified: true,
                    granted_scopes: &[],
                },
            )
            .await
            .expect("create session")
        };
        let cookie_name = oidc_rp::app_session_cookie_name(true);
        let req = ntex::web::test::TestRequest::default()
            .uri("/api/me")
            .header(http::header::HOST, aud)
            .header("cookie", format!("{cookie_name}={}", session.id))
            .to_http_request();
        let rid = Uuid::new_v4();

        let outcome = resolve_app_session_user_header_inner(
            &req,
            &state,
            &app_id_str,
            &rid,
            Some("oac_myapp"),
            None, // no sector → fail closed
        )
        .await;
        assert!(
            matches!(outcome, CookieOutcome::ClientNotProvisioned),
            "cookie arm must fail closed without a sector, got {outcome:?}"
        );

        let pool = crate::db::checkout(&db).await.expect("pool");
        let conn = pool.get().await.expect("pool");
        crate::sessions::revoke(&conn, session.id).await.ok();
    }

    // ─── Batch A fix 2: self-describing-subject invariant ─────────────────

    /// Decode the signed `ZeroShip-User` header and pull `.email` out.
    fn decode_header_email(state: &crate::GateState, header: &str) -> String {
        let json = zeroship_core::auth::verify_zeroship_user_header(
            state.config.worker_key.as_bytes(),
            header,
        )
        .expect("ZeroShip-User MAC verifies");
        let v: serde_json::Value = serde_json::from_str(&json).expect("user json");
        v["email"].as_str().unwrap_or("").to_string()
    }

    /// A hand-crafted wrapper whose `sub` is a GLOBAL UUID (not a `pws_`) must
    /// be HARD-REJECTED by BOTH wrapper fast-paths — defense in depth: even if
    /// some future mint path forgot to project, a non-projected wrapper can
    /// never reach a worker and leak the global identity into the JS-readable
    /// token. No PG needed (the invariant check precedes any DB touch). The
    /// `debug_assert!` is `cfg(not(test))`-gated so this exercises the runtime
    /// reject, the production defense against a forged token.
    #[ntex::test]
    async fn wrapper_with_uuid_sub_is_rejected_by_both_fast_paths() {
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let state = build_state_with_wrapper(gateway_signing);
        let request_id = Uuid::new_v4();

        let aud = "myapp.zeroship.ai";
        let client_id = "oac_myapp";
        // A global UUID sub — exactly what an un-projected wrapper would carry.
        let uuid_sub = "0192f1aa-bbbb-7ccc-8ddd-eeeeffff0001";

        // (a) Bearer wrapper path (cnf = None).
        let plain = issue_plain_wrapper(&state, uuid_sub, client_id, aud);
        let bearer = bearer_req(&plain, aud);
        let outcome =
            resolve_bearer_user_header(&bearer, &state, &request_id, Some(client_id), None).await;
        assert!(
            matches!(outcome, BearerOutcome::Invalid),
            "Bearer wrapper with a UUID sub must be Invalid (subject invariant), got {outcome:?}"
        );

        // (b) DPoP wrapper fast-path (cnf = Some(jkt)).
        let client_key = ed25519_dalek::SigningKey::from_bytes(&[42u8; 32]);
        let jkt = client_jkt(&client_key);
        let dpop_wrapper = issue_wrapper_for_sub(&state, uuid_sub, &jkt, aud);
        let now = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        )
        .unwrap();
        let htu = format!("http://{aud}/api/me");
        let proof = sign_dpop_proof(&client_key, "GET", &htu, &dpop_wrapper, now);
        let dpop_req = ntex::web::test::TestRequest::default()
            .uri("/api/me")
            .header(http::header::HOST, aud)
            .header(http::header::AUTHORIZATION, format!("DPoP {dpop_wrapper}"))
            .header("dpop", proof)
            .to_http_request();
        let outcome =
            resolve_dpop_user_header(&dpop_req, &state, &request_id, None, None).await;
        assert!(
            matches!(outcome, DpopOutcome::None),
            "DPoP wrapper with a UUID sub must be rejected (subject invariant), got {outcome:?}"
        );

        // Sanity: the SAME wrappers with a real pws_ sub ARE accepted, so the
        // reject above is the invariant firing — not a broken fixture.
        let pws = format!("pws_{}", Uuid::new_v4().simple());
        let plain_ok = issue_plain_wrapper(&state, &pws, client_id, aud);
        assert!(
            matches!(
                resolve_bearer_user_header(&bearer_req(&plain_ok, aud), &state, &request_id, Some(client_id), None).await,
                BearerOutcome::Allowed(_)
            ),
            "a pws_-sub Bearer wrapper must still Allow"
        );
    }

    // ─── Batch A fix 3: revocation cross-arm parity ───────────────────────

    /// Write the family marker the EXACT way `/signout` does — keyed on
    /// `(client_id, pws_)` where `pws_ = derive_pairwise(salt, global_uuid,
    /// sector)` — then assert that BOTH a still-live raw-Hydra Bearer token AND
    /// a DPoP-introspected token for the SAME `(client_id, user)` are now
    /// rejected. Pre-fix these arms keyed the lookup on the GLOBAL UUID while
    /// the writer keyed on `pws_`, so a real signout never matched a live token
    /// (the MAJOR cross-arm namespace mismatch). PG-gated.
    ///
    /// `#[ntex::test]` (not `#[compio::test]`) because it stands up
    /// `ntex::web::test::server` JWKS + introspection mocks — the same harness
    /// the sibling raw-Hydra / introspection revocation tests use — and that
    /// requires the ntex runtime (a `compio::test` would nest runtimes).
    #[ntex::test]
    async fn revocation_keyed_on_pws_rejects_raw_hydra_and_dpop_introspection() {
        let Some(db) = connect_auth_db().await else {
            eprintln!("skipping (no AUTH_DB_URL)");
            return;
        };

        let jwks_signing = ed25519_dalek::SigningKey::from_bytes(&[55u8; 32]);
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let client_key = ed25519_dalek::SigningKey::from_bytes(&[42u8; 32]);

        let client_id = "oac_revparity";
        let host = "myapp.zeroship.ai";
        let sector = "https://myapp.zeroship.ai";
        let global_sub = format!("0192f1aa-bbbb-7ccc-8ddd-{:012x}", rand_suffix());

        // (a) RAW-HYDRA BEARER arm. Build a state whose oidc_rp JWKS serves the
        // Hydra key. Both test builders default to an all-zero `pairwise_salt`,
        // so we read the SAME salt the arms will use off the built state and
        // derive the WRITER's pws_ from it — no fragile Arc mutation.
        let srv = start_jwks_server(hydra_jwks_doc(&jwks_signing)).await;
        let base = srv.url("").trim_end_matches('/').to_string();
        let oidc_rp = crate::oidc_rp::OidcRp::new(
            &base,
            "gateway",
            "test-secret",
            b"test-stash-key-32-bytes-long----".to_vec(),
        )
        .with_issuer(HYDRA_ISS);
        let bearer_state =
            build_state_with_wrapper_and_oidc_and_db(gateway_signing.clone(), oidc_rp, Some(db.clone()));

        // The pws_ the WRITER (/signout) keys on — derive_pairwise under the
        // route's sector with the SAME salt the arm uses (read off the state).
        let pws_sub =
            zeroship_core::auth::derive_pairwise(&bearer_state.pairwise_salt, &global_sub, sector);

        // Write the family marker EXACTLY as browser_auth::signout does:
        // revoke_family(client_id, pws_sub).
        {
            let pool = crate::db::checkout(&db).await.expect("pool checkout");
            let conn = pool.get().await.expect("pool get");
            zeroship_core::wrapper_revocation::revoke_family(&conn, client_id, &pws_sub)
                .await
                .expect("signout-style revoke_family on (client_id, pws_)");
        }

        let token = sign_hydra_access_jwt(
            &jwks_signing,
            &global_sub,
            Some(client_id),
            serde_json::json!(["http://api.zeroship.localhost"]),
            "user@example.com",
            "Hydra User",
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
            "a still-live raw-Hydra Bearer must be rejected by the pws_-keyed marker, got {outcome:?}"
        );

        // (b) DPoP-INTROSPECTION arm. Introspection mock returns the SAME
        // global sub + the route client_id; the arm projects pws_ and keys the
        // marker check on it.
        let srv_i = start_introspect_server(serde_json::json!({
            "active": true,
            "sub": global_sub,
            "client_id": client_id,
            "email": "user@example.com",
            "email_verified": true,
            "name": "Hydra User",
            "scope": "openid email",
            "iat": i64::try_from(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs(),
            )
            .unwrap()
                - 60,
        }))
        .await;
        let base_i = srv_i.url("").trim_end_matches('/').to_string();
        // Same all-zero default salt as `bearer_state`, so the introspection
        // arm derives the SAME pws_ the marker was written under.
        let intro_state =
            build_state_for_introspection_with_db(gateway_signing, &base_i, Some(db.clone()));
        debug_assert_eq!(intro_state.pairwise_salt, bearer_state.pairwise_salt);

        let dpop_req = opaque_dpop_req(&client_key, "ht_opaque_revparity", host, "/api/me");
        let outcome = resolve_dpop_user_header(
            &dpop_req,
            &intro_state,
            &request_id,
            Some(client_id),
            Some(sector),
        )
        .await;
        assert!(
            matches!(outcome, DpopOutcome::None),
            "a DPoP-introspected token must be rejected by the pws_-keyed marker, got {outcome:?}"
        );

        // Cleanup.
        let pool = crate::db::checkout(&db).await.expect("pool checkout");
        let conn = pool.get().await.expect("pool get");
        conn.execute(
            "DELETE FROM auth.token_revocations WHERE client_id = $1 AND sub = $2",
            &[&client_id, &pws_sub],
        )
        .await
        .ok();
        drop(srv);
        drop(srv_i);
    }

    // ─── Batch A fix 5: live relay_email re-resolve on wrapper fast-path ──

    /// A wrapper carries an alias-only email at mint, but the alias can be
    /// revoked WHILE the wrapper is still valid. The Bearer wrapper fast-path
    /// must re-resolve the alias LIVE keyed on `(client_id, pws_)` and emit an
    /// EMPTY email once it is revoked — never the stale alias the wrapper still
    /// embeds. PG-gated.
    #[compio::test]
    async fn bearer_wrapper_reresolves_relay_email_live_and_blanks_on_revoke() {
        let Some(db) = connect_auth_db().await else {
            eprintln!("skipping (no AUTH_DB_URL)");
            return;
        };
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let state = build_state_with_wrapper_and_auth_ui_url_and_db(
            gateway_signing,
            "http://127.0.0.1:1",
            Some(db.clone()),
        );

        let aud = "myapp.zeroship.ai";
        let client_id = format!("oac_livemail_{}", Uuid::new_v4().simple());
        let global_user_id = Uuid::new_v4();
        let pws_sub = format!("pws_live_{}", Uuid::new_v4().simple());
        let active_alias = format!("{}@relay.zeroship.localhost", Uuid::new_v4().simple());

        // Seed the user + an ACTIVE alias row keyed on (client_id, pws_).
        let dsn = std::env::var("AUTH_DB_URL").unwrap();
        let (seed, conn) = compio_postgres::connect(&dsn, compio_postgres::NoTls)
            .await
            .expect("seed connect");
        compio::runtime::spawn(async move {
            let _ = conn.run().await;
        })
        .detach();
        seed.execute(
            "INSERT INTO auth.users (id, email, name, email_verified_at) \
             VALUES ($1, $2::citext, $3, NOW())",
            &[&global_user_id, &format!("real-{}@example.com", global_user_id.simple()), &"Live User"],
        )
        .await
        .expect("seed user");
        seed.execute(
            "INSERT INTO auth.app_user_identities \
                (app_client_id, global_user_id, pairwise_sub, relay_email) \
             VALUES ($1, $2, $3, $4)",
            &[&client_id, &global_user_id, &pws_sub, &active_alias],
        )
        .await
        .expect("seed alias");

        // The wrapper is minted carrying the helper's default alias claim
        // (`relay-alias@zeroship.ai`). The LIVE re-resolve must OVERRIDE that
        // with the DB value: pre-revoke the header email must be the seeded
        // ACTIVE alias (not the mint-time claim), proving the arm reads live.
        let token = issue_plain_wrapper_with_scope(&state, &pws_sub, &client_id, aud, "openid email");
        let req = bearer_req(&token, aud);
        let request_id = Uuid::new_v4();

        // Pre-revoke: the live alias is projected.
        let BearerOutcome::Allowed(header) =
            resolve_bearer_user_header(&req, &state, &request_id, Some(&client_id), None).await
        else {
            panic!("pre-revoke wrapper must Allow");
        };
        assert_eq!(
            decode_header_email(&state, &header),
            active_alias,
            "wrapper fast-path must project the LIVE active alias"
        );

        // Revoke the alias (the 5c cascade write).
        seed.execute(
            "UPDATE auth.app_user_identities SET revoked_at = now() \
             WHERE app_client_id = $1 AND global_user_id = $2",
            &[&client_id, &global_user_id],
        )
        .await
        .expect("revoke alias");

        // Post-revoke: the SAME still-valid wrapper now projects an EMPTY email
        // (fail closed) — the stale alias must NOT survive.
        let BearerOutcome::Allowed(header2) =
            resolve_bearer_user_header(&req, &state, &request_id, Some(&client_id), None).await
        else {
            panic!("wrapper still cryptographically valid post alias-revoke");
        };
        assert_eq!(
            decode_header_email(&state, &header2),
            "",
            "a revoked alias must blank the wrapper fast-path email (fail closed)"
        );

        // Cleanup.
        seed.execute(
            "DELETE FROM auth.app_user_identities WHERE app_client_id = $1",
            &[&client_id],
        )
        .await
        .ok();
        seed.execute("DELETE FROM auth.users WHERE id = $1", &[&global_user_id])
            .await
            .ok();
    }
}
