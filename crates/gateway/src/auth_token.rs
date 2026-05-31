//! `POST /__zs/auth/token` + `GET /__zs/auth/session` — the browser-login
//! CORE of the `@zeroship/auth` SDK, **BFF redesign**
//! (`2026-05-30-auth-bff-session-redesign` §2.2).
//!
//! The browser receives **only an identity projection + HttpOnly cookies** —
//! NO power/wrapper access token, NO scopes, NO JWT in any response body. The
//! server-held refresh family (the anchor) stays as the BFF custody store; the
//! platform is the resource server.
//!
//! - **`POST /__zs/auth/token`** runs the PKCE code→token exchange on the
//!   browser's behalf, then: (a) creates a `auth.gateway_sessions` row (the
//!   store the live cookie arm reads) carrying the global user UUID, the
//!   consent scopes, and the id-token `auth_time`/`amr`, AND keeps creating the
//!   encrypted server-held refresh-family anchor (`auth.app_session_anchors`);
//!   (b) sets the HttpOnly `__Host-zs_app_session` cookie (Lax, 12h) + the
//!   `__Host-zs_app_anchor` cookie (Strict, 30d, reload-recovery) + the
//!   `zs.<host>.is.authenticated` breadcrumb; (c) returns ONLY
//!   `{ user: { id: pws_, email: relay-alias, … }, expires_at }`. No
//!   `access_token`, no `scope`, no `id_token`, no `token_type`.
//! - **`GET /__zs/auth/session[?mint=1]`** returns the identity projection
//!   `{ user, expires_at }` (relay-swapped email, `pws_` id) read from the live
//!   `gateway_sessions` row. When the gateway session has lapsed but the anchor
//!   is valid (reload-recovery), `?mint=1` rotates the server-held refresh
//!   family, re-creates the `gateway_sessions` row + re-sets
//!   `__Host-zs_app_session`, and returns the projection. **No JWT in any
//!   body; the real email never appears.**
//!
//! ## Same-origin-only (CORS is NOT the boundary)
//!
//! Both endpoints emit NO `Access-Control-Allow-Origin` /
//! `allow-credentials`. The boundary is the conjunction of: a custom
//! `X-ZS-Auth` header, an exact `Origin == app origin` match (foreign /
//! `null` rejected; no reflection), and `Sec-Fetch-Site: same-origin`
//! (enforced when present). `?mint=1` additionally REQUIRES `X-ZS-Auth` so
//! a top-level navigation cannot trigger a family rotation.
//!
//! ## Reload-recovery single-flight (round-6 BLOCKER)
//!
//! Concurrent `?mint=1` callers for the same anchor on one worker thread
//! coalesce into ONE Hydra refresh via a per-thread in-process single-flight
//! keyed on `anchor_id` ([`crate::anchors::with_single_flight`]). With the
//! browser wrapper gone (BFF §3.1) there is no cached-wrapper short-circuit;
//! reload-storm coalescing is the family-rotation single-flight alone. **NO db
//! connection or lock is held across the Hydra HTTP call**: the rotation future
//! checks a pooled connection out, reads the anchor, RELEASES it, does the
//! Hydra refresh, then checks another out to persist.

use std::sync::Arc;

use futures::FutureExt as _;
use ntex::util::Bytes;
use ntex::web::{types::State, HttpRequest, HttpResponse};
use serde_json::json;
use uuid::Uuid;

use crate::anchors::{self, RotationError, RotationOk, RotationResult};
use crate::oidc_rp::TokenSet;
use crate::GateState;

pub(crate) const CACHE_NO_STORE: &str = "no-store";
const X_ZS_AUTH: &str = "x-zs-auth";

/// Resolved per-request route facts the auth endpoints need.
pub(crate) struct RouteCtx {
    pub(crate) app_name: String,
    /// The app's stable UUID (the `RouteMap` key from `lookup_by_name`). This —
    /// NOT the subdomain slug `app_name` — is the CANONICAL key for the
    /// `auth.gateway_sessions` + `auth.app_session_anchors` rows, matching the
    /// live per-request dispatch arm (`router/auth.rs` keys sessions by
    /// `app_id.to_string()`). Keying on the immutable UUID (the slug can be
    /// renamed) is what lets a `/token`-minted cookie validate on the real
    /// SPA→app dispatch path.
    pub(crate) app_id: Uuid,
    pub(crate) host: String,
    pub(crate) client_id: String,
    /// The app's stable apex origin used to scope the per-app pairwise
    /// `pws_…` subject (§6.2). `None` until the control plane provisions
    /// it; the identity projection then hard-fails closed (no `pws_`, 503)
    /// rather than fall back to the global UUID.
    pub(crate) sector_identifier: Option<String>,
}

/// Resolve the app name (subdomain), Host, and per-app `oauth_client_id`
/// from the request. Returns `Err(response)` when the host is not a
/// provisioned app or the app has no OAuth client yet (`503`).
pub(crate) fn resolve_route(req: &HttpRequest, state: &GateState) -> Result<RouteCtx, HttpResponse> {
    let host = req
        .headers()
        .get(http::header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("")
        .to_string();

    let Some(app_name) = crate::router::extract_app_name(req, None) else {
        return Err(error_response(
            HttpResponse::BadRequest(),
            "invalid_request",
            "could not resolve app from Host",
        ));
    };
    let Some((app_id, route)) = state.routes.lookup_by_name(&app_name) else {
        // Unknown app host — same shape the app would 404 with, but the
        // auth endpoint answers JSON.
        return Err(error_response(
            HttpResponse::ServiceUnavailable(),
            "client_not_provisioned",
            "no route for this host",
        ));
    };
    let Some(client_id) = route.entry.oauth_client_id.clone() else {
        // App exists but its OAuth client isn't provisioned yet (1d). The
        // SDK treats 503 as retryable/recovering and KEEPS the breadcrumb.
        return Err(error_response(
            HttpResponse::ServiceUnavailable(),
            "client_not_provisioned",
            "app has no oauth_client_id yet",
        ));
    };

    let sector_identifier = route.entry.sector_identifier.clone();

    Ok(RouteCtx { app_name, app_id, host, client_id, sector_identifier })
}

/// Derive the per-app pairwise subject (`pws_…`) the identity projection
/// carries, so the SPA reads a per-app pseudonym, never the global user UUID
/// (§6.2/G4). Hard-fails (`None`) when the route has no `sector_identifier`
/// yet — the caller answers `503 client_not_provisioned` rather than ever
/// project the global UUID to the browser.
fn pairwise_sub(state: &GateState, route: &RouteCtx, global_user_id: &str) -> Option<String> {
    let sector = route.sector_identifier.as_deref()?;
    Some(zeroship_core::auth::derive_pairwise(
        &state.pairwise_salt,
        global_user_id,
        sector,
    ))
}

/// Resolve the ACTIVE relay alias for `(route.client_id, global_user_id)` —
/// the email-claim swap source for the `/token`/`/session` identity projection
/// (relay sub-spec §7). The SPA-facing `{ user }.email` MUST be the alias,
/// never the user's real address.
///
/// Returns `None` when no alias is minted yet OR the grant was revoked
/// (`revoked_at IS NULL` gate) — the caller then projects an EMPTY email (fail
/// closed), NEVER the real one. A DB checkout/read failure also yields `None`
/// (fail closed): the projection must never leak the real email on a blip.
///
/// `pub(crate)` so the DPoP-exchange handler reuses the SAME fail-closed
/// email-swap source (Batch A fix 1) rather than duplicate the pooled-read
/// logic — keeping every mint path's email projection byte-for-byte identical.
#[allow(clippy::future_not_send)]
pub(crate) async fn relay_alias_for(
    db_cfg: &crate::db::DbConfig,
    client_id: &str,
    global_user_id: Uuid,
) -> Option<String> {
    let pool = match crate::db::checkout(db_cfg).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "relay alias lookup: pool checkout failed (failing closed on email)");
            return None;
        }
    };
    let conn = match pool.get().await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "relay alias lookup: pool get failed (failing closed on email)");
            return None;
        }
    };
    match crate::identities::lookup_relay_email(&conn, client_id, global_user_id).await {
        Ok(alias) => alias,
        Err(e) => {
            tracing::warn!(error = %e, "relay alias lookup failed (failing closed on email)");
            None
        }
    }
}

/// Same-origin guard for state-changing POST `/token` (§1.2). Rejects a
/// present-but-foreign `Origin`, `Origin: null`, and (for POST) a missing
/// `Origin` on browsers that send it. `Sec-Fetch-Site` is enforced WHEN
/// present, advisory when absent. The SDK's custom `X-ZS-Auth` header is
/// the primary, browser-version-independent defense.
///
/// `require_custom_header` is `true` for `?mint=1` (a top-level navigation
/// cannot set it) and for POST `/token`.
pub(crate) fn same_origin_guard(
    req: &HttpRequest,
    host: &str,
    insecure_dev: bool,
    require_custom_header: bool,
    require_origin: bool,
) -> Result<(), HttpResponse> {
    // (1) Custom non-simple header.
    if require_custom_header && req.headers().get(X_ZS_AUTH).is_none() {
        return Err(error_response(
            HttpResponse::BadRequest(),
            "invalid_request",
            "missing X-ZS-Auth header",
        ));
    }

    let scheme = if insecure_dev { "http" } else { "https" };
    let expected_origin = format!("{scheme}://{host}");

    // (2) Origin exact-match the app's own origin. Never reflect, never
    // substring/subdomain-match.
    match req
        .headers()
        .get(http::header::ORIGIN)
        .and_then(|v| v.to_str().ok())
    {
        Some("null") => {
            return Err(error_response(
                HttpResponse::Forbidden(),
                "forbidden",
                "Origin: null rejected",
            ));
        }
        Some(origin) if origin == expected_origin => {}
        Some(_) => {
            return Err(error_response(
                HttpResponse::Forbidden(),
                "forbidden",
                "foreign Origin rejected",
            ));
        }
        None => {
            // Modern browsers always send Origin on POST fetch; a missing
            // Origin on a state-changing POST is rejected. GET /session is
            // more lenient (older browsers, top-level), so the X-ZS-Auth
            // requirement carries the defense there.
            if require_origin {
                return Err(error_response(
                    HttpResponse::Forbidden(),
                    "forbidden",
                    "missing Origin on state-changing request",
                ));
            }
        }
    }

    // (3) Sec-Fetch-Site: enforced WHEN present, advisory when absent.
    if let Some(sfs) = req
        .headers()
        .get("sec-fetch-site")
        .and_then(|v| v.to_str().ok())
    {
        if sfs != "same-origin" {
            return Err(error_response(
                HttpResponse::Forbidden(),
                "forbidden",
                "Sec-Fetch-Site is not same-origin",
            ));
        }
    }

    Ok(())
}

/// Form/JSON body the SDK posts to `/__zs/auth/token`.
#[derive(serde::Deserialize, Default)]
struct TokenRequest {
    grant_type: Option<String>,
    code: Option<String>,
    code_verifier: Option<String>,
    redirect_uri: Option<String>,
    refresh_token: Option<String>,
}

/// `POST /__zs/auth/token` — code→token exchange, then identity-only response
/// (BFF redesign §2.2). See module docs.
///
/// The browser receives ONLY `{ user, expires_at }` (relay-swapped email,
/// `pws_` id) + two HttpOnly cookies (`__Host-zs_app_session` live credential,
/// `__Host-zs_app_anchor` reload-recovery). No `access_token`, no `scope`, no
/// `id_token`, no `token_type` — and the real email never appears.
#[allow(clippy::future_not_send, clippy::too_many_lines)]
pub async fn token(
    req: HttpRequest,
    body: Bytes,
    state: State<Arc<GateState>>,
) -> HttpResponse {
    let route = match resolve_route(&req, &state) {
        Ok(r) => r,
        Err(resp) => return resp,
    };

    // Same-origin guard: POST /token requires the custom header AND an
    // Origin (state-changing).
    if let Err(resp) = same_origin_guard(&req, &route.host, state.config.insecure_dev, true, true) {
        return resp;
    }

    let Some(db_cfg) = state.db.as_ref() else {
        return error_response(
            HttpResponse::ServiceUnavailable(),
            "db_unavailable",
            "no database configured",
        );
    };

    let parsed = parse_token_request(&req, &body);
    let grant = parsed.grant_type.as_deref().unwrap_or("authorization_code");

    // Only the authorization_code grant is in scope here. The browser
    // refresh-grant is gone — the SPA rides the cookie session, and
    // reload-recovery rotates the server-held family via /session?mint=1.
    if grant != "authorization_code" {
        return error_response(
            HttpResponse::BadRequest(),
            "unsupported_grant_type",
            "only authorization_code is supported on /token in this slice",
        );
    }
    let (Some(code), Some(verifier)) = (parsed.code.as_deref(), parsed.code_verifier.as_deref())
    else {
        return error_response(
            HttpResponse::BadRequest(),
            "invalid_request",
            "code + code_verifier required",
        );
    };
    // Default redirect_uri to the popup-callback on the app origin (what the
    // SDK uses) when the body omits it.
    let scheme = if state.config.insecure_dev { "http" } else { "https" };
    let default_redirect = format!("{scheme}://{}/__zs/auth/popup-callback", route.host);
    let redirect_uri = parsed.redirect_uri.as_deref().unwrap_or(&default_redirect);

    // 1. Code→token exchange (public PKCE client; gateway injects client_id).
    let tokens = match state
        .oidc_rp
        .exchange_code_public(&route.client_id, code, verifier, redirect_uri)
        .await
    {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(error = %e, app = %route.app_name, "/token: code exchange failed");
            return error_response(
                HttpResponse::BadRequest(),
                "invalid_grant",
                "code exchange failed",
            );
        }
    };

    // 2. The id_token is LOAD-BEARING: validate sig/iss/aud(=client_id)/exp
    //    via the gateway's Hydra JWKS. `expected_nonce=None` — the nonce is
    //    the SDK's own sessionStorage cross-flow guard, never echoed to the
    //    gateway. at_hash/c_hash bind the id_token to the access token + code.
    let Some(id_token) = tokens.id_token.as_deref() else {
        return error_response(
            HttpResponse::BadRequest(),
            "invalid_token",
            "no id_token in token response",
        );
    };
    let claims = match zeroship_core::oidc_verify::verify_id_token(
        &state.oidc_rp.jwks,
        id_token,
        &state.oidc_rp.issuer,
        &route.client_id,
        None,
        Some(&tokens.access_token),
        Some(code),
    )
    .await
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, app = %route.app_name, "/token: id_token verify failed");
            return error_response(
                HttpResponse::BadRequest(),
                "invalid_token",
                "id_token verification failed",
            );
        }
    };

    // 3. The global user UUID is the Hydra sub. It is stored INTERNALLY (the
    //    anchor + the gateway session `user_id`) and projected to the per-app
    //    `pws_` for the browser — the global UUID never reaches the browser.
    let Ok(global_user_id) = Uuid::parse_str(&claims.sub) else {
        tracing::warn!(sub = %claims.sub, "/token: id_token sub is not a UUID");
        return error_response(
            HttpResponse::BadRequest(),
            "invalid_token",
            "id_token sub is not a global user id",
        );
    };

    // 4. server_anchor mode: keep the refresh family server-side, encrypted.
    let Some(refresh_token) = tokens.refresh_token.as_deref() else {
        return error_response(
            HttpResponse::BadRequest(),
            "invalid_grant",
            "no refresh_token in token response (offline_access not granted?)",
        );
    };
    let aad = anchor_aad(&route.client_id, &claims.sub);
    let refresh_enc =
        match zeroship_core::crypto::encrypt(&state.anchor_enc_key, &aad, refresh_token.as_bytes()) {
            Ok(ct) => ct,
            Err(e) => {
                tracing::error!(error = %e, "/token: refresh encrypt failed");
                return error_response(
                    HttpResponse::InternalServerError(),
                    "internal",
                    "refresh encryption failed",
                );
            }
        };

    // 5. Project the per-app pairwise `pws_` subject (§6.2/G4) for the browser
    //    identity. Derive on the CANONICAL UUID string. Fail closed (503) when
    //    the route has no `sector_identifier` yet rather than ever project the
    //    global UUID to the browser.
    let Some(pws_sub) = pairwise_sub(&state, &route, &global_user_id.to_string()) else {
        return error_response(
            HttpResponse::ServiceUnavailable(),
            "client_not_provisioned",
            "app has no sector_identifier yet",
        );
    };
    // Email-claim swap (§7): the `{ user }` projection carries the relay ALIAS,
    // never the real `claims.email`. No active alias (not yet minted at
    // consent, or revoked) ⇒ empty email (fail closed) — the real address
    // NEVER reaches the browser.
    let relay_email = relay_alias_for(db_cfg, &route.client_id, global_user_id).await;
    let scope = tokens.scope.clone().unwrap_or_default();
    let scopes: Vec<String> = scope.split_whitespace().map(str::to_string).collect();
    let amr = claims.amr.clone().unwrap_or_default();

    // 6. Create the gateway session (the SPA's live request credential — the
    //    store the cookie arm already reads) AND the reload-recovery anchor,
    //    on one pooled connection. The gateway session carries the GLOBAL UUID
    //    (internal), the consent scopes, and the id-token auth_time/amr; the
    //    anchor carries the encrypted refresh family. NO connection is held
    //    across any outbound call (the Hydra exchange already completed).
    let family_id = zeroship_core::typed_id::generate("rfam");
    // CANONICAL session/anchor key: the app UUID, NOT the subdomain slug. The
    // live per-request dispatch arm (`router/auth.rs`) validates the cookie
    // session with `app_id.to_string()`; keying these rows by the same UUID is
    // what lets the /token-minted cookie authenticate the SPA's real
    // fetch('/api/...') requests.
    let app_key = route.app_id.to_string();
    let (session_id, anchor_id) = {
        let pool = match crate::db::checkout(db_cfg).await {
            Ok(p) => p,
            Err(e) => return db_error(e),
        };
        let conn = match pool.get().await {
            Ok(c) => c,
            Err(e) => return db_error(e),
        };

        // 6a. The gateway session — the SPA's live cookie credential.
        let session = match crate::sessions::create(
            &conn,
            &crate::sessions::NewSession {
                user_id: &global_user_id.to_string(),
                app_id: &app_key,
                // The REAL email is stored on the row (CITEXT); it is
                // relay-swapped only on the READ path (the `{ user }` body
                // below + /session), never emitted to the browser.
                email: claims.email.as_deref(),
                name: claims.name.as_deref(),
                avatar_url: claims.picture.as_deref(),
                email_verified: claims.email_verified.unwrap_or(false),
                granted_scopes: &scopes,
                auth_time: claims.auth_time,
                amr: &amr,
            },
        )
        .await
        {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "/token: gateway_sessions create failed");
                return error_response(
                    HttpResponse::InternalServerError(),
                    "internal",
                    "session create failed",
                );
            }
        };

        // 6b. The reload-recovery anchor (server-held refresh family). Keyed by
        //     the SAME canonical app UUID as the gateway session so the
        //     `anchor.app_id == route.app_id` self-consistency check on
        //     reload-recovery is meaningful and never slug-vs-UUID skewed.
        let anchor = match anchors::create(
            &conn,
            &anchors::NewAnchor {
                app_id: &app_key,
                client_id: &route.client_id,
                global_user_id,
                refresh_token_enc: &refresh_enc,
                refresh_family_id: &family_id,
                granted_scopes: &scopes,
            },
        )
        .await
        {
            Ok(a) => a,
            Err(e) => {
                tracing::error!(error = %e, "/token: anchor create failed");
                return error_response(
                    HttpResponse::InternalServerError(),
                    "internal",
                    "anchor create failed",
                );
            }
        };
        (session.id, anchor.id)
        // `conn`/`pool` drop here — released before we build the response.
    };

    // 7. Identity-only response: set BOTH HttpOnly cookies (live session +
    //    reload-recovery anchor) + the breadcrumb, and return `{ user,
    //    expires_at }` with the relay-swapped email and the `pws_` id. NO
    //    `access_token`, NO `scope`, NO `id_token`, NO `token_type`.
    let user = user_projection(&pws_sub, relay_email.as_deref(), claims.name.as_deref(), claims.email_verified);
    let expires_at = now_secs() + crate::oidc_rp::APP_SESSION_MAX_AGE_SECS;

    HttpResponse::Ok()
        .header("cache-control", CACHE_NO_STORE)
        .header(
            "set-cookie",
            crate::oidc_rp::set_app_session_cookie(&session_id, state.config.insecure_dev),
        )
        .header(
            "set-cookie",
            anchors::set_anchor_cookie(&anchor_id, state.config.insecure_dev),
        )
        .header(
            "set-cookie",
            anchors::set_breadcrumb_cookie(&route.host, state.config.insecure_dev),
        )
        .json(&json!({
            "user": user,
            "expires_at": expires_at,
        }))
}

/// `GET /__zs/auth/session[?mint=1]` — the identity projection (BFF redesign
/// §2.2). Returns ONLY `{ user, expires_at }` (relay-swapped email, `pws_` id);
/// NO JWT in any body; the real email never appears.
///
/// Read path: the live `gateway_sessions` row (via the `__Host-zs_app_session`
/// cookie) is the primary source. When that row is gone/expired but the
/// `__Host-zs_app_anchor` is valid, reload-recovery rotates the server-held
/// refresh family, re-creates the gateway session + re-sets the session cookie,
/// and returns the projection. `?mint=1` forces that rotation.
#[allow(clippy::future_not_send, clippy::too_many_lines)]
pub async fn session(req: HttpRequest, state: State<Arc<GateState>>) -> HttpResponse {
    let route = match resolve_route(&req, &state) {
        Ok(r) => r,
        Err(resp) => return resp,
    };

    let want_mint = req.query_string().split('&').any(|kv| {
        matches!(kv.split_once('='), Some(("mint", "1")))
    });

    // Same-origin guard. `?mint=1` REQUIRES X-ZS-Auth (a top-level
    // navigation cannot set it ⇒ cannot trigger a rotation). A non-mint GET is
    // harmless, so the custom header is not required there. GET is not strictly
    // state-changing, so a missing Origin is tolerated (the X-ZS-Auth
    // requirement carries the rotation defense).
    if let Err(resp) = same_origin_guard(
        &req,
        &route.host,
        state.config.insecure_dev,
        want_mint,
        false,
    ) {
        return resp;
    }

    let Some(db_cfg) = state.db.as_ref() else {
        return error_response(
            HttpResponse::ServiceUnavailable(),
            "db_unavailable",
            "no database configured",
        );
    };

    let cookie_header = req
        .headers()
        .get(http::header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    // 1. Fast path (no `?mint=1`): the live gateway_sessions row IS the
    //    identity. Read it and project. This is the steady state — a present,
    //    non-expired session needs no anchor read and no Hydra round-trip.
    if !want_mint {
        if let Some(session_id) =
            crate::oidc_rp::parse_app_session_cookie(cookie_header, state.config.insecure_dev)
        {
            let validated = {
                let pool = match crate::db::checkout(db_cfg).await {
                    Ok(p) => p,
                    Err(e) => return db_error(e),
                };
                let conn = match pool.get().await {
                    Ok(c) => c,
                    Err(e) => return db_error(e),
                };
                match crate::sessions::validate(&conn, session_id, &route.app_id.to_string())
                    .await
                {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(error = %e, "/session: gateway_sessions validate failed");
                        None
                    }
                }
            };
            if let Some(s) = validated {
                return match build_identity_projection(&state, &route, db_cfg, &s).await {
                    Ok(user) => identity_projection_ok(
                        &route,
                        state.config.insecure_dev,
                        user,
                        s.abs_expires_at.timestamp(),
                        None,
                    ),
                    Err(resp) => resp,
                };
            }
            // Session cookie present but stale/invalid → fall through to
            // anchor reload-recovery below.
        }
    }

    // 2. Reload-recovery (gateway session gone/expired, or `?mint=1`): read the
    //    anchor, rotate the server-held family, re-create the gateway session,
    //    re-set the session cookie, return the projection. Released
    //    immediately (NO conn held across the Hydra refresh).
    let Some(anchor_id) = anchors::parse_anchor_cookie(cookie_header, state.config.insecure_dev)
    else {
        return login_required(&route.host, state.config.insecure_dev);
    };

    let anchor = {
        let pool = match crate::db::checkout(db_cfg).await {
            Ok(p) => p,
            Err(e) => return db_error(e),
        };
        let conn = match pool.get().await {
            Ok(c) => c,
            Err(e) => return db_error(e),
        };
        match anchors::read_live(&conn, anchor_id).await {
            Ok(Some(a)) => a,
            Ok(None) => return login_required(&route.host, state.config.insecure_dev),
            Err(e) => {
                tracing::error!(error = %e, "/session: anchor read failed");
                return error_response(
                    HttpResponse::InternalServerError(),
                    "internal",
                    "anchor read failed",
                );
            }
        }
    };

    // Bind the anchor to this host's app (defense in depth — the cookie is
    // __Host- so it cannot have come from another host, but the app_id must
    // still match the resolved route). Both are now the canonical app UUID.
    if anchor.app_id != route.app_id.to_string() {
        return login_required(&route.host, state.config.insecure_dev);
    }

    // Rotate the server-held family via the per-node single-flight (one Hydra
    // refresh for N concurrent reloaders), then re-create the gateway session
    // from the rotated id-token facts.
    match rotate_family(&state, &route, &anchor).await {
        Ok(rotated) => {
            // Project the per-app `pws_` (§6.3) + relay-swap the email. Fail
            // closed if the sector is missing (same posture as /token).
            let Some(pws_sub) =
                pairwise_sub(&state, &route, &rotated.global_user_id.to_string())
            else {
                return error_response(
                    HttpResponse::ServiceUnavailable(),
                    "client_not_provisioned",
                    "app has no sector_identifier yet",
                );
            };
            let relay_email =
                relay_alias_for(db_cfg, &route.client_id, rotated.global_user_id).await;

            // Re-create the gateway session from the rotated facts so the SPA
            // regains a live cookie credential. Carry auth_time/amr forward.
            let new_session_id = {
                let pool = match crate::db::checkout(db_cfg).await {
                    Ok(p) => p,
                    Err(e) => return db_error(e),
                };
                let conn = match pool.get().await {
                    Ok(c) => c,
                    Err(e) => return db_error(e),
                };
                match crate::sessions::create(
                    &conn,
                    &crate::sessions::NewSession {
                        user_id: &rotated.global_user_id.to_string(),
                        // Canonical key: the app UUID (matches the live dispatch
                        // arm + the /token create above), NOT the subdomain slug.
                        app_id: &route.app_id.to_string(),
                        // Real email stored on the row; relay-swapped on read.
                        // The rotated raw access JWT does not always carry the
                        // email — leave it `None` when absent (the session row
                        // is identity-by-pws_; the projection reads the relay
                        // alias regardless).
                        email: None,
                        name: rotated.name.as_deref(),
                        // name/avatar are sourced from the rotated id_token in
                        // do_refresh (BFF minor fix) so they no longer degrade
                        // across a reload-recovery vs the original /token row.
                        avatar_url: rotated.avatar_url.as_deref(),
                        email_verified: rotated.email_verified.unwrap_or(false),
                        granted_scopes: &rotated.granted_scopes,
                        auth_time: rotated.auth_time,
                        amr: &rotated.amr,
                    },
                )
                .await
                {
                    Ok(s) => s.id,
                    Err(e) => {
                        tracing::error!(error = %e, "/session: gateway_sessions re-create failed");
                        return error_response(
                            HttpResponse::InternalServerError(),
                            "internal",
                            "session re-create failed",
                        );
                    }
                }
            };

            let user = user_projection(
                &pws_sub,
                relay_email.as_deref(),
                rotated.name.as_deref(),
                rotated.email_verified,
            );
            let expires_at = now_secs() + crate::oidc_rp::APP_SESSION_MAX_AGE_SECS;
            identity_projection_ok(
                &route,
                state.config.insecure_dev,
                user,
                expires_at,
                Some(new_session_id),
            )
        }
        Err(RotationError::LoginRequired) => {
            // Anchor-dead: delete the row + clear the breadcrumb.
            if let Ok(pool) = crate::db::checkout(db_cfg).await {
                if let Ok(conn) = pool.get().await {
                    let _ = anchors::delete(&conn, anchor_id).await;
                }
            }
            login_required(&route.host, state.config.insecure_dev)
        }
        Err(RotationError::Upstream(msg)) => {
            tracing::warn!(error = %msg, "/session: reload-recovery upstream failure");
            // Retryable/recovering — do NOT clear the breadcrumb (§4.3).
            error_response(
                HttpResponse::ServiceUnavailable(),
                "temporarily_unavailable",
                "reload-recovery upstream failure",
            )
        }
    }
}

/// Build the `{ user }` projection from a live gateway session row, with the
/// MANDATORY relay-email swap (§2.3): the `gateway_sessions.email` column holds
/// the REAL inbox (CITEXT) — it must NEVER reach the browser. The relay alias
/// (or empty string, fail-closed) is emitted instead. The `pws_` id is derived
/// from the session's GLOBAL `user_id`; a missing sector fails closed (503).
#[allow(clippy::future_not_send)]
async fn build_identity_projection(
    state: &Arc<GateState>,
    route: &RouteCtx,
    db_cfg: &crate::db::DbConfig,
    s: &crate::sessions::AppSession,
) -> Result<serde_json::Value, HttpResponse> {
    let Some(pws_sub) = pairwise_sub(state, route, &s.user_id) else {
        return Err(error_response(
            HttpResponse::ServiceUnavailable(),
            "client_not_provisioned",
            "app has no sector_identifier yet",
        ));
    };
    let global_user_id = match Uuid::parse_str(&s.user_id) {
        Ok(u) => u,
        Err(e) => {
            tracing::warn!(error = %e, "/session: gateway session user_id is not a UUID");
            return Err(error_response(
                HttpResponse::InternalServerError(),
                "internal",
                "invalid session user id",
            ));
        }
    };
    // Relay-swap: emit the ALIAS, never `s.email` (the real inbox). None ⇒ "".
    let relay_email = relay_alias_for(db_cfg, &route.client_id, global_user_id).await;
    Ok(user_projection(
        &pws_sub,
        relay_email.as_deref(),
        s.name.as_deref(),
        Some(s.email_verified),
    ))
}

/// Emit the identity-only `{ user, expires_at }` response, refreshing the
/// breadcrumb and (when reload-recovery re-created the session) re-setting the
/// `__Host-zs_app_session` cookie. NO JWT in the body.
fn identity_projection_ok(
    route: &RouteCtx,
    insecure_dev: bool,
    user: serde_json::Value,
    expires_at: i64,
    new_session_id: Option<Uuid>,
) -> HttpResponse {
    let mut builder = HttpResponse::Ok();
    builder.header("cache-control", CACHE_NO_STORE);
    if let Some(sid) = new_session_id {
        builder.header(
            "set-cookie",
            crate::oidc_rp::set_app_session_cookie(&sid, insecure_dev),
        );
    }
    builder.header(
        "set-cookie",
        anchors::set_breadcrumb_cookie(&route.host, insecure_dev),
    );
    builder.json(&json!({ "user": user, "expires_at": expires_at }))
}

/// Rotate the server-held refresh family for `anchor` (the `?mint=1` /
/// reload-recovery core). Coalesces concurrent reloaders for this anchor on
/// THIS worker thread into ONE Hydra refresh via the per-thread single-flight.
/// NO db connection is held across the Hydra call. The browser receives no
/// wrapper — the result carries only the identity facts the handler needs to
/// re-create the gateway session.
#[allow(clippy::future_not_send)]
async fn rotate_family(
    state: &Arc<GateState>,
    route: &RouteCtx,
    anchor: &anchors::Anchor,
) -> RotationResult {
    // PER-NODE SINGLE-FLIGHT keyed on anchor.id. If another task on THIS thread
    // already started the refresh, await its shared future.
    let anchor_id = anchor.id;
    if let Some(existing) = anchors::with_single_flight(|sf| sf.get(anchor_id)) {
        return existing.await;
    }

    // We are the leader: build the refresh future and register it. Removal of
    // the single-flight entry is tied to the SHARED future's resolution, NOT to
    // this leader task's survival: an `EntryGuard` owned by the future body
    // removes the entry when the future is dropped (the round-6 BLOCKER
    // invariant `remove single_flight.entry once fut resolves`). So if this
    // leader's request future is cancelled mid-flight (client disconnect / ntex
    // timeout) after `insert`, a surviving follower still drives the shared
    // future to completion, and the guard fires on drop — the entry is cleared
    // and never leaks a resolved-but-stuck result to later callers.
    let st = Arc::clone(state);
    let client_id = route.client_id.clone();
    let anchor = anchor.clone();
    let fut: anchors::SharedRotationFuture = (Box::pin(async move {
        let _guard = anchors::EntryGuard::new(anchor_id);
        do_refresh(&st, &client_id, &anchor).await
    }) as std::pin::Pin<Box<dyn std::future::Future<Output = RotationResult>>>)
        .shared();

    let shared = anchors::with_single_flight(|sf| sf.insert(anchor_id, fut));
    // Drive the shared future. Removal is the guard's job (above), so we do NOT
    // call `remove` here — that would only fire on the leader's survival and is
    // exactly the leak the guard fixes.
    shared.await
}

/// The coalesced refresh body: one Hydra `/oauth2/token` refresh, then persist
/// the rotated family (NO wrapper — the browser holds none, BFF §3.1) and
/// return the rotated id-token facts. Holds NO db connection across the Hydra
/// call.
#[allow(clippy::future_not_send)]
async fn do_refresh(
    state: &Arc<GateState>,
    client_id: &str,
    anchor: &anchors::Anchor,
) -> RotationResult {
    let Some(db_cfg) = state.db.as_ref() else {
        return Err(RotationError::Upstream("no database".into()));
    };

    // Decrypt the server-held refresh family. AAD binds (client_id, sub).
    let sub = anchor.global_user_id.to_string();
    let aad = anchor_aad(client_id, &sub);
    let refresh = match zeroship_core::crypto::decrypt(
        &state.anchor_enc_key,
        &aad,
        &anchor.refresh_token_enc,
    ) {
        Ok(pt) => String::from_utf8_lossy(&pt).into_owned(),
        Err(e) => return Err(RotationError::Upstream(format!("refresh decrypt: {e}"))),
    };

    // Hydra refresh — NO db connection held here.
    let tokens: TokenSet = match state.oidc_rp.refresh_token_public(client_id, &refresh).await {
        Ok(t) => t,
        Err(e) => {
            // Distinguish anchor-dead (invalid_grant → 720h ceiling / family
            // revoked) from a transient upstream failure.
            let msg = e.to_string();
            if msg.contains("invalid_grant") {
                return Err(RotationError::LoginRequired);
            }
            return Err(RotationError::Upstream(msg));
        }
    };

    // Verify the rotated RAW access JWT LOCALLY via the gateway JWKS (no
    // per-mint introspection). The raw JWT stays server-side — it never leaves
    // the gateway and is never handed to the browser.
    let raw = match state.oidc_rp.verify_access_token(&tokens.access_token).await {
        Ok(c) => c,
        Err(e) => return Err(RotationError::Upstream(format!("rotated access verify: {e}"))),
    };
    // Bind the rotated raw JWT to THIS app's client (RFC 9068 §3): a token
    // issued to another client must never re-establish this app's session.
    if let Some(tok_client) = raw.client_id.as_deref() {
        if tok_client != client_id {
            return Err(RotationError::Upstream(format!(
                "rotated access client_id mismatch: {tok_client} != {client_id}"
            )));
        }
    }

    // Identity facts (name / email_verified / auth_time / amr / avatar) are
    // sourced from the rotated ID TOKEN first — the OIDC profile carrier — and
    // fall back to the raw ACCESS JWT only when the refresh grant returns no
    // id_token (BFF minor fix). The access JWT "does not always" carry the
    // profile claims, so reading them off it alone silently degrades
    // name/avatar/email_verified across a reload-recovery vs the original
    // /token row. We verify the rotated id_token sig/iss/aud against the same
    // gateway JWKS; on a refresh grant there is no `code`, so the
    // at_hash/c_hash/nonce bindings are not applicable (None). A present but
    // INVALID id_token is a hard upstream failure (a rotated token must verify);
    // an ABSENT id_token transparently falls back to the access JWT.
    let id_claims: Option<zeroship_core::oidc_verify::TokenClaims> =
        match tokens.id_token.as_deref() {
            Some(id_token) => match zeroship_core::oidc_verify::verify_id_token(
                &state.oidc_rp.jwks,
                id_token,
                &state.oidc_rp.issuer,
                client_id,
                None,
                None,
                None,
            )
            .await
            {
                Ok(c) => Some(c),
                Err(e) => {
                    return Err(RotationError::Upstream(format!("rotated id_token verify: {e}")))
                }
            },
            None => None,
        };

    // Project each identity fact: id_token claim when present, else the raw
    // access JWT claim (the prior behavior).
    let name = id_claims
        .as_ref()
        .and_then(|c| c.name.clone())
        .or_else(|| raw.name.clone());
    let avatar_url = id_claims
        .as_ref()
        .and_then(|c| c.picture.clone());
    let email_verified = id_claims
        .as_ref()
        .and_then(|c| c.email_verified)
        .or(raw.email_verified);
    let auth_time = id_claims
        .as_ref()
        .and_then(|c| c.auth_time)
        .or(raw.auth_time);
    let amr = id_claims
        .as_ref()
        .and_then(|c| c.amr.clone())
        .or_else(|| raw.amr.clone())
        .unwrap_or_default();

    let scope = tokens
        .scope
        .clone()
        .or(raw.scope.clone())
        .unwrap_or_default();
    let granted_scopes: Vec<String> = scope.split_whitespace().map(str::to_string).collect();

    // Persist the rotated family (checkout, write, release). No wrapper to
    // cache — only the rotated encrypted refresh family + its lineage id.
    let new_refresh = tokens.refresh_token.as_deref().unwrap_or(&refresh);
    let new_enc =
        match zeroship_core::crypto::encrypt(&state.anchor_enc_key, &aad, new_refresh.as_bytes()) {
            Ok(ct) => ct,
            Err(e) => return Err(RotationError::Upstream(format!("refresh encrypt: {e}"))),
        };
    // Carry the anchor's OWN gateway-generated lineage id verbatim across the
    // rotation — Hydra's TokenSet exposes no usable family-lineage field.
    let family_id = anchor.refresh_family_id.clone();
    {
        let pool = match crate::db::checkout(db_cfg).await {
            Ok(p) => p,
            Err(e) => return Err(RotationError::Upstream(format!("pool checkout: {e}"))),
        };
        let conn = match pool.get().await {
            Ok(c) => c,
            Err(e) => return Err(RotationError::Upstream(format!("pool get: {e}"))),
        };
        if let Err(e) =
            anchors::update_rotated_family(&conn, anchor.id, &new_enc, &family_id).await
        {
            return Err(RotationError::Upstream(format!("anchor update: {e}")));
        }
    }

    Ok(RotationOk {
        global_user_id: anchor.global_user_id,
        granted_scopes,
        email_verified,
        name,
        avatar_url,
        auth_time,
        amr,
    })
}

// ─── helpers ─────────────────────────────────────────────────────────────

/// AAD binding the encrypted refresh family to its `(client_id, sub)` row
/// context, so a ciphertext copied to another row/app cannot be decrypted.
fn anchor_aad(client_id: &str, sub: &str) -> Vec<u8> {
    format!("zs-anchor-refresh:{client_id}:{sub}").into_bytes()
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(0))
        .unwrap_or(0)
}

/// The browser-facing identity projection (BFF redesign §2.2). Carries ONLY
/// `{ id: pws_, email: relay-alias, name, avatar, email_verified }`. **No
/// scopes** — the SPA holds no capability and makes no authz decision. `email`
/// is the relay alias or **empty string** (fail closed — `None` ⇒ `""`), never
/// the real inbox (§2.3).
fn user_projection(
    id: &str,
    email: Option<&str>,
    name: Option<&str>,
    email_verified: Option<bool>,
) -> serde_json::Value {
    json!({
        "id": id,
        "email": email.unwrap_or(""),
        "name": name,
        "avatar": serde_json::Value::Null,
        "email_verified": email_verified.unwrap_or(false),
    })
}

/// Parse the `/token` body as form-urlencoded OR JSON (Content-Type-driven,
/// defaulting to form).
fn parse_token_request(req: &HttpRequest, body: &[u8]) -> TokenRequest {
    let ct = req
        .headers()
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if ct.contains("application/json") {
        serde_json::from_slice(body).unwrap_or_default()
    } else {
        let mut out = TokenRequest::default();
        for (k, v) in url::form_urlencoded::parse(body) {
            match k.as_ref() {
                "grant_type" => out.grant_type = Some(v.into_owned()),
                "code" => out.code = Some(v.into_owned()),
                "code_verifier" => out.code_verifier = Some(v.into_owned()),
                "redirect_uri" => out.redirect_uri = Some(v.into_owned()),
                "refresh_token" => out.refresh_token = Some(v.into_owned()),
                _ => {}
            }
        }
        out
    }
}

pub(crate) fn error_response(
    mut builder: ntex::web::HttpResponseBuilder,
    code: &str,
    detail: &str,
) -> HttpResponse {
    builder
        .header("cache-control", CACHE_NO_STORE)
        .json(&json!({ "error": code, "error_description": detail }))
}

fn login_required(host: &str, insecure_dev: bool) -> HttpResponse {
    HttpResponse::Unauthorized()
        .header("cache-control", CACHE_NO_STORE)
        .header("set-cookie", anchors::clear_anchor_cookie(insecure_dev))
        .header("set-cookie", anchors::clear_breadcrumb_cookie(host, insecure_dev))
        .json(&json!({ "error": "login_required" }))
}

pub(crate) fn db_error(e: compio_postgres::Error) -> HttpResponse {
    tracing::warn!(error = %e, "auth endpoint: pg pool checkout failed");
    error_response(
        HttpResponse::ServiceUnavailable(),
        "db_unavailable",
        "database checkout failed",
    )
}
