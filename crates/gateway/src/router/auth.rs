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
    let dpop_user_header = resolve_dpop_user_header(req, state).await;
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
        resolve_app_session_user_header_inner(req, state, &app_id_str).await;
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

/// Resolve the `ZeroShip-User` header from a DPoP-bound access token.
///
/// Returns `Some(header)` when:
///   1. `Authorization: DPoP <token>` is present, AND
///   2. The `DPoP:` proof header is present, AND
///   3. The proof verifies (signature, htm, htu, iat, ath), AND
///   4. The proof's `jti` has not been seen before in the freshness
///      window (replay defense), AND
///   5. Hydra's `/oauth2/introspect` returns `active: true` for the
///      access token.
///
/// Returns `None` for "no `DPoP` token in this request" AND for every
/// failure mode above. The caller distinguishes the two via
/// [`has_dpop_authorization`].
async fn resolve_dpop_user_header(
    req: &HttpRequest,
    state: &Arc<GateState>,
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
    if !state.dpop_jti_cache.insert(&verified.jti, now, 120) {
        tracing::warn!(jti = %verified.jti, "DPoP jti replay detected");
        return None;
    }

    // 6. Introspect the access token. Hydra's response carries the
    //    user identity claims when `active: true`.
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

    // 7. Build the `ZeroShip-User` header from the introspection result.
    //    `WorkerUser` borrows — keep the owned strings on the stack.
    let id = info.sub.unwrap_or_default();
    let email = info.email.unwrap_or_default();
    let name = info.name.unwrap_or_default();
    let user = oidc_rp::WorkerUser {
        id: &id,
        email: &email,
        name: &name,
        avatar: None,
        email_verified: info.email_verified.unwrap_or(false),
    };
    Some(oidc_rp::encode_user_header(
        &user,
        &state.config.worker_key,
    ))
}

/// Resolve the `ZeroShip-User` header value from the per-origin app
/// session cookie. Returns `None` if no cookie, validate fails, or no
/// DB is configured. Logs DB errors at warn — never panics.
async fn resolve_app_session_user_header_inner(
    req: &HttpRequest,
    state: &Arc<GateState>,
    app_id_str: &str,
) -> Option<String> {
    let cookie_header = req
        .headers()
        .get("cookie")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let session_id = oidc_rp::parse_app_session_cookie(cookie_header)?;
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

/// Pull the `__Host-zs_app_session` value out of a Cookie header.
/// Returns `None` when the cookie is missing or empty so callers can
/// fall back to a different discriminator (e.g. per-rule rate-limit
/// session-keyed buckets falling back to IP).
pub(super) fn extract_session_cookie(cookie_header: Option<&str>) -> Option<String> {
    let s = cookie_header?;
    let prefix = format!("{}=", oidc_rp::APP_SESSION_COOKIE);
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
            extract_session_cookie(Some("__Host-zs_app_session=")),
            None
        );
        assert_eq!(extract_session_cookie(None), None);
        assert_eq!(extract_session_cookie(Some("other=foo")), None);
    }

    #[test]
    fn extract_session_cookie_reads_app_session_value() {
        let id = uuid::Uuid::new_v4();
        let header = format!("foo=bar; __Host-zs_app_session={id}; baz=qux");
        let extracted = extract_session_cookie(Some(&header)).expect("present");
        assert_eq!(extracted, id.to_string());
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
}
