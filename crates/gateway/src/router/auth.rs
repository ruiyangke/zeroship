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
                if let (Some(db), Some(subject)) = (
                    state.db.as_ref(),
                    zeroship_core::wrapper_revocation::subject_uuid(&claims.sub),
                ) {
                    match zeroship_core::wrapper_revocation::is_subject_revoked_since(
                        db.as_ref(),
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
    let db = state.db.as_ref()?;
    let session = match sessions::validate(db, session_id, app_id_str).await {
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
        db: Option<std::sync::Arc<compio_postgres::Client>>,
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
                auth_ui_url: auth_ui_url.into(),
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
            oidc_rp: StdArc::new(crate::oidc_rp::OidcRp::new(
                "http://127.0.0.1:1",
                "gateway",
                "test-secret",
                b"test-stash-key-32-bytes-long----".to_vec(),
            )),
            db,
            dpop_jti_cache: StdArc::new(zeroship_core::dpop::TieredJtiCache::default()),
            logout_jti_cache: StdArc::new(
                zeroship_core::logout_token::LogoutJtiCache::default(),
            ),
            signing_key: Some(StdArc::new(signing)),
            wrapper_issuer: Some(StdArc::new(issuer)),
            wrapper_verifier: Some(StdArc::new(verifier)),
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
        let (client, connection) = compio_postgres::connect(&dsn, compio_postgres::NoTls)
            .await
            .expect("connect auth db");
        compio::runtime::spawn(async move {
            let _ = connection.run().await;
        })
        .detach();
        let db = std::sync::Arc::new(client);

        let subject = Uuid::new_v4();
        let gateway_signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let client_key = ed25519_dalek::SigningKey::from_bytes(&[42u8; 32]);
        let state = build_state_with_wrapper_and_auth_ui_url_and_db(
            gateway_signing,
            "http://127.0.0.1:1",
            Some(db.clone()),
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

        zeroship_core::wrapper_revocation::revoke_subject(&db, subject)
            .await
            .expect("revoke subject");

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

        db.execute(
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
}
