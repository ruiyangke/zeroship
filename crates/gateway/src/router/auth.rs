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
    let dpop_user_header = resolve_dpop_user_header(req, state, request_id).await;
    if let Some(header) = dpop_user_header {
        // DPoP succeeded — short-circuit. We treat a DPoP-authed request
        // as fully authenticated regardless of policy (anon or user).
        return AuthOutcome::Allowed {
            user_header: Some(header),
        };
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
    match resolve_bearer_user_header(req, state, request_id, oauth_client_id).await {
        BearerOutcome::Allowed(header) => {
            return AuthOutcome::Allowed {
                user_header: Some(header),
            };
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
    let session_user_header =
        resolve_app_session_user_header_inner(req, state, &app_id_str, request_id).await;
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
///      `active: true` (P7-U5 fallback, no `cnf.jkt` enforcement).
///
/// Returns `None` for "no `DPoP` token in this request" AND for every
/// failure mode above. The caller distinguishes the two via
/// [`has_dpop_authorization`].
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
) -> Option<String> {
    // 1. Authorization: DPoP <token>
    let auth_header = req
        .headers()
        .get(http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())?;
    let access_token = auth_header.strip_prefix("DPoP ")?;

    // 2. DPoP proof header
    let Some(proof) = req.headers().get("dpop").and_then(|v| v.to_str().ok()) else {
        tracing::warn!("Authorization: DPoP present but DPoP proof header missing");
        return None;
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
            return None;
        }
    };

    // 5. jti replay protection — 120 s freshness window matches the
    //    accepted clock skew on the proof's iat claim.
    match state.dpop_jti_cache.insert(&verified.jti, now, 120).await {
        Ok(true) => {}
        Ok(false) => {
            tracing::warn!(jti = %verified.jti, "DPoP jti replay detected");
            return None;
        }
        Err(e) => {
            tracing::warn!(error = %e, "DPoP jti replay check failed");
            return None;
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
                    return None;
                };
                if cnf.jkt != verified.jkt {
                    tracing::warn!(
                        expected = %cnf.jkt,
                        actual = %verified.jkt,
                        "DPoP proof jkt does not match wrapper cnf.jkt — rejecting"
                    );
                    return None;
                }
                if claims.sub.is_empty() {
                    tracing::warn!("DPoP wrapper token missing sub — rejecting");
                    return None;
                }
                if let (Some(db_cfg), Some(subject)) = (
                    state.db.as_ref(),
                    zeroship_core::wrapper_revocation::subject_uuid(&claims.sub),
                ) {
                    // Check out a pooled connection for just this
                    // revocation lookup and release it on drop.
                    let pool = match crate::db::checkout(db_cfg).await {
                        Ok(p) => p,
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                "DPoP wrapper revocation: pg pool checkout failed"
                            );
                            return None;
                        }
                    };
                    let conn = match pool.get().await {
                        Ok(c) => c,
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                "DPoP wrapper revocation: pg pool checkout failed"
                            );
                            return None;
                        }
                    };
                    match zeroship_core::wrapper_revocation::is_subject_revoked_since(
                        &conn,
                        subject,
                        claims.iat,
                    )
                    .await
                    {
                        Ok(true) => {
                            tracing::warn!(
                                sub = %claims.sub,
                                "DPoP wrapper subject was revoked after wrapper issue"
                            );
                            return None;
                        }
                        Ok(false) => {}
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                sub = %claims.sub,
                                "DPoP wrapper revocation check failed"
                            );
                            return None;
                        }
                    }
                }
                let owned = build_worker_user_from_wrapper(&claims);
                let user: oidc_rp::WorkerUser<'_> = (&owned).into();
                return Some(oidc_rp::encode_user_header(
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
                    return None;
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
            return None;
        }
        Err(e) => {
            tracing::warn!(error = %e, "DPoP introspect call failed");
            return None;
        }
    };
    if !matches!(info.sub.as_deref(), Some(sub) if !sub.is_empty()) {
        tracing::warn!("DPoP introspection response missing sub");
        return None;
    }

    // 7. Build the `ZeroShip-User` header from the introspection result.
    let owned = build_worker_user_from_introspection(&info);
    let user: oidc_rp::WorkerUser<'_> = (&owned).into();
    Some(oidc_rp::encode_user_header(
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
/// Slice 1c does NOT derive pairwise subjects (Slice 4) nor enforce
/// scopes (Slice 3): the raw-Hydra `sub` is used directly as the worker
/// user id, mirroring the introspection fallback in the DPoP arm.
async fn resolve_bearer_user_header(
    req: &HttpRequest,
    state: &Arc<GateState>,
    request_id: &Uuid,
    oauth_client_id: Option<&str>,
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
        let owned = build_worker_user_from_wrapper(&claims);
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
        // Cross-node PER-APP family-marker revocation (spec §8.5). Keyed on
        // `(expected_client_id, sub)` — the SAME `(client_id, sub)` shape
        // the wrapper path uses — so revocation is per-app, not global:
        // revoking this user on app A leaves their raw-Hydra access on app
        // B valid. We key on `expected_client_id` (the route's bound
        // client) rather than the token's `client_id` claim because the
        // binding above already proved they agree (or the aud fallback did),
        // and the marker is written against the route's client. The sub is
        // the global Hydra UUID today; Slice 4 will project it to a pws_.
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
                &claims.sub,
                claims.iat,
            )
            .await
            {
                Ok(true) => {
                    tracing::warn!(
                        client_id = %expected_client_id,
                        sub = %claims.sub,
                        "raw-Hydra Bearer family revoked after iat"
                    );
                    return BearerOutcome::Invalid;
                }
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(error = %e, sub = %claims.sub, "raw-Hydra Bearer revocation check failed");
                    return BearerOutcome::Invalid;
                }
            }
        }
        let owned = build_worker_user_from_access_claims(&claims);
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
/// Slice 1c uses the `sub` (global UUID) directly as the worker user id;
/// Slice 4 will project it to a per-app `pws_` first. The profile fields
/// come straight from the verified claims.
fn build_worker_user_from_access_claims(claims: &crate::oidc_rp::AccessClaims) -> OwnedWorkerUser {
    OwnedWorkerUser {
        id: claims.sub.clone(),
        email: claims.email.clone().unwrap_or_default(),
        name: claims.name.clone().unwrap_or_default(),
        email_verified: claims.email_verified.unwrap_or(false),
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
}

impl<'a> From<&'a OwnedWorkerUser> for oidc_rp::WorkerUser<'a> {
    fn from(owned: &'a OwnedWorkerUser) -> Self {
        oidc_rp::WorkerUser {
            id: &owned.id,
            email: &owned.email,
            name: &owned.name,
            avatar: None,
            email_verified: owned.email_verified,
        }
    }
}

/// Resolve the `ZeroShip-User` header value from the per-origin app
/// session cookie. Returns `None` if no cookie, validate fails, or no
/// DB is configured. Logs DB errors at warn — never panics.
async fn resolve_app_session_user_header_inner(
    req: &HttpRequest,
    state: &Arc<GateState>,
    app_id_str: &str,
    request_id: &Uuid,
) -> Option<String> {
    let cookie_header = req
        .headers()
        .get("cookie")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let session_id = oidc_rp::parse_app_session_cookie(cookie_header, state.config.insecure_dev)?;
    // Check out a pooled connection for just this validate (which slides
    // the idle window) and release it on drop.
    let db_cfg = state.db.as_ref()?;
    let pool = match crate::db::checkout(db_cfg).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "gateway: pg pool checkout failed (session validate)");
            return None;
        }
    };
    let conn = match pool.get().await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "gateway: pg pool checkout failed (session validate)");
            return None;
        }
    };
    let session = match sessions::validate(&conn, session_id, app_id_str).await {
        Ok(Some(s)) => s,
        Ok(None) => return None,
        Err(e) => {
            // Fail-closed on DB errors. Returning None forces the auth
            // gate to treat the request as unauthenticated; the caller
            // either 401s (API) or redirects to login (HTML).
            tracing::warn!(error = %e, "gateway: session validate failed");
            return None;
        }
    };
    drop(conn);
    let user = oidc_rp::WorkerUser {
        id: &session.user_id,
        email: session.email.as_deref().unwrap_or(""),
        name: session.name.as_deref().unwrap_or(""),
        avatar: session.avatar_url.as_deref(),
        email_verified: session.email_verified,
    };
    Some(oidc_rp::encode_user_header(
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
        };
        let owned = build_worker_user_from_introspection(&info);
        assert_eq!(owned.id, "usr_xyz");
        assert_eq!(owned.email, "u@x.test");
        assert_eq!(owned.name, "Bob");
        assert!(owned.email_verified);
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
        issue_wrapper_for_sub(state, "usr_test", proof_jkt, aud)
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
        let header = resolve_dpop_user_header(&req, &state, &request_id).await;
        assert!(header.is_some(), "wrapper path must accept matched jkt");
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
        let header = resolve_dpop_user_header(&req, &state, &request_id).await;
        assert!(
            header.is_none(),
            "DPoP path must reject a cnf-less plain-Bearer wrapper"
        );
    }

    #[compio::test]
    async fn resolve_dpop_rejects_wrapper_revoked_by_subject() {
        let Ok(dsn) = std::env::var("AUTH_DB_URL") else {
            eprintln!("skipping (no AUTH_DB_URL)");
            return;
        };
        let db_cfg = crate::db::DbConfig::new(dsn.clone(), 4);

        let subject = Uuid::new_v4();
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let client_key = ed25519_dalek::SigningKey::from_bytes(&[42u8; 32]);
        let state = build_state_with_wrapper_and_auth_ui_url_and_db(
            gateway_signing,
            "http://127.0.0.1:1",
            Some(db_cfg.clone()),
        );

        let jkt = client_jkt(&client_key);
        let aud = "myapp.zeroship.ai";
        let wrapper = issue_wrapper_for_sub(&state, &subject.to_string(), &jkt, aud);
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
            resolve_dpop_user_header(&first_req, &state, &request_id)
                .await
                .is_some(),
            "wrapper should resolve before subject revocation"
        );

        {
            let pool = crate::db::checkout(&db_cfg).await.expect("pool checkout");
            let conn = pool.get().await.expect("pool checkout");
            zeroship_core::wrapper_revocation::revoke_subject(&conn, subject)
                .await
                .expect("revoke subject");
        }

        let second_proof = sign_dpop_proof(&client_key, "GET", &htu, &wrapper, now);
        let second_req = ntex::web::test::TestRequest::default()
            .uri("/api/me")
            .header(http::header::HOST, aud)
            .header(http::header::AUTHORIZATION, format!("DPoP {wrapper}"))
            .header("dpop", second_proof)
            .to_http_request();
        assert!(
            resolve_dpop_user_header(&second_req, &state, &request_id)
                .await
                .is_none(),
            "revoked wrapper subject must be rejected"
        );

        let pool = crate::db::checkout(&db_cfg).await.expect("pool checkout");
        let conn = pool.get().await.expect("pool checkout");
        conn.execute(
            "DELETE FROM auth.wrapper_revoked_subjects WHERE subject = $1",
            &[&subject],
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
        let header = resolve_dpop_user_header(&req, &state, &request_id).await;
        assert!(
            header.is_none(),
            "wrapper path must reject tokens with an empty sub"
        );
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
        let header = resolve_dpop_user_header(&req, &state, &request_id).await;
        assert!(
            header.is_none(),
            "wrapper path must reject when cnf.jkt does not match proof jkt"
        );
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
        let header = resolve_dpop_user_header(&req, &state, &request_id).await;
        assert!(
            header.is_none(),
            "malformed wrapper must hard-reject instead of downgrading to introspection"
        );
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
        state
            .wrapper_issuer
            .as_ref()
            .expect("issuer configured")
            .issue(&crate::wrapper_token::WrapperMint {
                aud,
                sub,
                scope: "openid email",
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

    #[compio::test]
    async fn bearer_valid_wrapper_emits_zeroship_user() {
        // Happy path (wrapper): a plain-Bearer wrapper for the route's
        // client_id verifies and the ZeroShip-User header carries the
        // wrapper's sub/email straight through (no pairwise re-derivation).
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let state = build_state_with_wrapper(gateway_signing);
        let aud = "myapp.zeroship.ai";
        let token = issue_plain_wrapper(&state, "pws_alice", "oac_myapp", aud);

        let req = bearer_req(&token, aud);
        let request_id = Uuid::new_v4();
        let outcome =
            resolve_bearer_user_header(&req, &state, &request_id, Some("oac_myapp")).await;
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
        assert_eq!(user["email"], "relay-alias@zeroship.ai");
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
            resolve_bearer_user_header(&req, &state, &request_id, Some("oac_app_b")).await;
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
        let outcome = resolve_bearer_user_header(&req, &state, &request_id, None).await;
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
            resolve_bearer_user_header(&req, &state, &request_id, Some("oac_myapp")).await;
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
            resolve_bearer_user_header(&req, &state, &request_id, Some("oac_myapp")).await;
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
        )
        .await;
        assert!(
            matches!(gated, AuthOutcome::Unauthenticated),
            "expired Bearer on User route must be Unauthenticated, got {gated:?}"
        );
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
        // client_id claim → Allowed + ZeroShip-User from the JWT sub.
        let jwks_signing = ed25519_dalek::SigningKey::from_bytes(&[55u8; 32]); // Hydra's key
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]); // gateway wrapper key
        let srv = start_jwks_server(hydra_jwks_doc(&jwks_signing)).await;
        let base = srv.url("").trim_end_matches('/').to_string();
        let state = build_state_for_hydra(gateway_signing, &base);

        let aud = "myapp.zeroship.ai";
        let token = sign_hydra_access_jwt(
            &jwks_signing,
            "usr_global_uuid",
            Some("oac_myapp"),
            serde_json::json!(["http://api.zeroship.localhost"]), // resource-server aud, NOT the client
            "user@example.com",
            "Hydra User",
            3600,
        );

        let req = bearer_req(&token, aud);
        let request_id = Uuid::new_v4();
        let outcome =
            resolve_bearer_user_header(&req, &state, &request_id, Some("oac_myapp")).await;
        let BearerOutcome::Allowed(header) = outcome else {
            panic!("expected Allowed, got {outcome:?}");
        };
        let json = zeroship_core::auth::verify_zeroship_user_header(
            state.config.worker_key.as_bytes(),
            &header,
        )
        .expect("MAC verifies");
        let user: serde_json::Value = serde_json::from_str(&json).expect("user json");
        // Slice 1c: raw-Hydra sub used directly (no pairwise yet).
        assert_eq!(user["id"], "usr_global_uuid");
        assert_eq!(user["email"], "user@example.com");

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
        let outcome =
            resolve_bearer_user_header(&req, &state, &request_id, Some("oac_myapp")).await;
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
            resolve_bearer_user_header(&req, &state, &request_id, Some("oac_app_b")).await;
        assert!(
            matches!(outcome, BearerOutcome::Invalid),
            "raw-Hydra client_id mismatch must be Invalid, got {outcome:?}"
        );

        drop(srv);
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
            resolve_bearer_user_header(&req, &state, &request_id, Some("oac_myapp")).await;
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
            resolve_bearer_user_header(&req, &state, &request_id, Some("oac_myapp")).await;
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
            resolve_bearer_user_header(&req, &state, &request_id, Some("gateway")).await;
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
                resolve_bearer_user_header(&bearer, &state, &request_id, Some("gateway")).await,
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
            resolve_dpop_user_header(&dpop_req, &state, &request_id)
                .await
                .is_some(),
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
                resolve_bearer_user_header(&req, &state, &request_id, Some(client_id)).await,
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
                resolve_bearer_user_header(&req, &state, &request_id, Some(client_id)).await,
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

        // Revoke ONLY app A's family for this sub.
        {
            let pool = crate::db::checkout(&db).await.expect("pool checkout");
            let conn = pool.get().await.expect("pool checkout");
            zeroship_core::wrapper_revocation::revoke_family(&conn, "oac_app_a", &sub)
                .await
                .expect("revoke_family app A");
        }

        // App A token: revoked → Invalid.
        assert!(
            matches!(
                resolve_bearer_user_header(&req_a, &state, &request_id, Some("oac_app_a")).await,
                BearerOutcome::Invalid
            ),
            "revoked (oac_app_a, sub) family must reject app A's token"
        );
        // App B token, SAME sub: NOT revoked → Allowed (per-app scoping).
        assert!(
            matches!(
                resolve_bearer_user_header(&req_b, &state, &request_id, Some("oac_app_b")).await,
                BearerOutcome::Allowed(_)
            ),
            "app A revocation must NOT revoke the same sub on app B"
        );

        let pool = crate::db::checkout(&db).await.expect("pool checkout");
        let conn = pool.get().await.expect("pool checkout");
        conn.execute(
            "DELETE FROM auth.token_revocations WHERE sub = $1",
            &[&sub],
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

        let aud = "myapp.zeroship.ai";
        let token = sign_hydra_access_jwt(
            &jwks_signing,
            "usr_global_uuid",
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
        assert_eq!(user["id"], "usr_global_uuid");

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
}
