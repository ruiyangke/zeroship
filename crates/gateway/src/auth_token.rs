//! `/__zeroship/auth/session` (POST + GET) — the ONE identity-session resource of the
//! `@zeroship/auth` SDK, **BFF redesign slice R1b**
//! (`2026-05-30-auth-bff-session-redesign` §2.2 + the signed-cookie addendum).
//!
//! **`POST /__zeroship/auth/token` is GONE — merged into `POST /__zeroship/auth/session`.**
//!
//! The browser receives **only an identity projection + HttpOnly cookies** —
//! NO power/wrapper access token, NO scopes, NO JWT in any response body. The
//! `__Host-zeroship_app_session` cookie is a gateway-SIGNED, short-lived (~15 min)
//! `zeroship-sess+jwt` identity assertion verified LOCALLY on every request (no
//! per-request DB read). The server-held refresh family (the anchor) stays as
//! the BFF custody store; the platform is the resource server.
//!
//! - **`POST /__zeroship/auth/session`** runs the PKCE code→token exchange on the
//!   browser's behalf, then: (a) writes a `zeroship.gateway_sessions` ROW — KEPT as
//!   the revocation/audit record + `auth_time`/`amr` source, NO LONGER read on
//!   the per-request path — AND keeps creating the encrypted server-held
//!   refresh-family anchor (`zeroship.app_session_anchors`); (b) ISSUES the signed
//!   `__Host-zeroship_app_session` cookie (`zeroship-sess+jwt`, Lax, ~15m) + the
//!   `__Host-zeroship_app_anchor` cookie (Strict, 30d, reload-recovery) + the
//!   `zs.<host>.is.authenticated` breadcrumb; (c) returns ONLY
//!   `{ user: { id: pws_, email: relay-alias, … }, expires_at }`. No
//!   `access_token`, no `scope`, no `id_token`, no `token_type`.
//! - **`GET /__zeroship/auth/session[?mint=1]`** DECODES the live signed cookie
//!   LOCALLY (no DB) and returns `{ user, expires_at }`. When the cookie has
//!   lapsed/`?mint=1` but the anchor is valid (reload-recovery), it rotates the
//!   server-held refresh family, re-writes the audit row, RE-SIGNS a fresh
//!   `__Host-zeroship_app_session` cookie, and returns the projection. **No JWT in any
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
    /// `zeroship.gateway_sessions` + `zeroship.app_session_anchors` rows, whose
    /// `app_id` columns are UUID and bound natively. Keying on the immutable
    /// UUID (the slug can be renamed) is what lets a `/token`-minted cookie
    /// validate on the real SPA→app dispatch path.
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
    let mut conn = match pool.get().await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "relay alias lookup: pool get failed (failing closed on email)");
            return None;
        }
    };
    match crate::identities::lookup_relay_email(&mut conn, client_id, global_user_id).await {
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

/// Form/JSON body the SDK posts to `POST /__zeroship/auth/session` (the merged
/// code→session exchange; the old `/token` route is gone).
#[derive(serde::Deserialize, Default)]
struct TokenRequest {
    grant_type: Option<String>,
    code: Option<String>,
    code_verifier: Option<String>,
    redirect_uri: Option<String>,
    refresh_token: Option<String>,
}

/// `POST /__zeroship/auth/session` — code→token exchange, then identity-only response
/// (BFF redesign §2.2 + slice R1b). This is the POST method of the MERGED
/// identity-session resource (the old `POST /__zeroship/auth/token` is gone).
///
/// The browser receives ONLY `{ user, expires_at }` (relay-swapped email,
/// `pws_` id) + two HttpOnly cookies: the SIGNED `__Host-zeroship_app_session`
/// (`zeroship-sess+jwt`, the live credential, verified locally on the hot path) and
/// `__Host-zeroship_app_anchor` (reload-recovery). No `access_token`, no `scope`, no
/// `id_token`, no `token_type` — and the real email never appears.
#[allow(clippy::future_not_send, clippy::too_many_lines)]
pub async fn session_post(
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

    // FAIL FAST on the missing session signing key. The end of this handler
    // MUST `sign_session_cookie`, which 503s (`session_signing_unavailable`)
    // when `state.session_issuer` is `None`. Without this early gate that 503
    // fires only AFTER the full Hydra code exchange + gateway-session + anchor
    // write — wasted work and a confusing late failure. `session_issuer` is
    // `Some` exactly when the gateway has a signing key (one-to-one in
    // `main.rs`), so checking it here is the same condition the late arm would
    // hit, surfaced before any outbound call or DB write.
    if state.session_issuer.is_none() {
        tracing::error!("/session: no session signing key configured — cannot mint session cookie");
        return error_response(
            HttpResponse::ServiceUnavailable(),
            "session_signing_unavailable",
            "gateway has no session signing key",
        );
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
            "only authorization_code is supported on POST /__zeroship/auth/session",
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
    let default_redirect = format!("{scheme}://{}/__zeroship/auth/popup-callback", route.host);
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

    // 6. Write the gateway_sessions ROW (the revocation/audit record + the
    //    auth_time/amr source — BFF R1b: NO LONGER read on the per-request path;
    //    the signed cookie is the live credential) AND the reload-recovery
    //    anchor, on one pooled connection. The row carries the GLOBAL UUID
    //    (internal), the consent scopes, and the id-token auth_time/amr; the
    //    anchor carries the encrypted refresh family. NO connection is held
    //    across any outbound call (the Hydra exchange already completed).
    let family_id = zeroship_core::typed_id::generate("rfam");
    // CANONICAL session/anchor key: the app UUID, NOT the subdomain slug.
    // Bound natively into the UUID `app_id` columns.
    let app_key = route.app_id;
    let anchor_id = {
        let pool = match crate::db::checkout(db_cfg).await {
            Ok(p) => p,
            Err(e) => return db_error(e),
        };
        let mut conn = match pool.get().await {
            Ok(c) => c,
            Err(e) => return db_error(e),
        };

        // 6a. The gateway session — KEPT as the revocation/audit record (+
        //     auth_time/amr source). NO LONGER read per request; the signed
        //     cookie is the live credential.
        let session = match crate::sessions::create(
            &mut conn,
            &crate::sessions::NewSession {
                user_id: &global_user_id.to_string(),
                app_id: app_key,
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
            &mut conn,
            &anchors::NewAnchor {
                app_id: app_key,
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
        // `session.id` (the gateway_sessions row) is KEPT as the
        // revocation/audit record + the auth_time/amr source — it is NO LONGER
        // read on the per-request path (the signed cookie is self-contained;
        // revocation is the per-app family marker). `_session_id` documents that.
        let _session_id = session.id;
        anchor.id
        // `conn`/`pool` drop here — released before we build the response.
    };

    // 7. Identity-only response: set the SIGNED session cookie (the SPA's live
    //    request credential — verified locally on the hot path, no DB) + the
    //    reload-recovery anchor + the breadcrumb, and return `{ user,
    //    expires_at }` with the relay-swapped email and the `pws_` id. NO
    //    `access_token`, NO `scope`, NO `id_token`, NO `token_type`.
    let session_cookie = match sign_session_cookie(
        &state,
        &route.client_id,
        &pws_sub,
        relay_email.as_deref(),
        claims.name.as_deref(),
        claims.picture.as_deref(),
        claims.email_verified,
        claims.auth_time,
        &amr,
        &scopes,
    ) {
        Ok(c) => c,
        Err(resp) => return resp,
    };
    let user = user_projection(&pws_sub, relay_email.as_deref(), claims.name.as_deref(), claims.email_verified);
    let expires_at = now_secs() + crate::oidc_rp::APP_SESSION_MAX_AGE_SECS;

    HttpResponse::Ok()
        .header("cache-control", CACHE_NO_STORE)
        .header("set-cookie", session_cookie)
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

/// `GET /__zeroship/auth/session[?mint=1]` — the identity projection (BFF redesign
/// §2.2). Returns ONLY `{ user, expires_at }` (relay-swapped email, `pws_` id);
/// NO JWT in any body; the real email never appears.
///
/// Read path: the live `gateway_sessions` row (via the `__Host-zeroship_app_session`
/// cookie) is the primary source. When that row is gone/expired but the
/// `__Host-zeroship_app_anchor` is valid, reload-recovery rotates the server-held
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

    let cookie_header = req
        .headers()
        .get(http::header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    // 1. Fast path (no `?mint=1`): DECODE the LIVE signed session cookie LOCALLY
    //    (signature + kid + iss + exp + app binding, no Hydra) and return the
    //    identity projection straight from its claims. This is the steady state —
    //    a present, non-expired signed cookie needs no anchor read and no network
    //    round-trip for IDENTITY. It MUST stay coherent with the per-request
    //    dispatch arm (`router/auth.rs::resolve_app_session_user_header_inner`):
    //    that arm runs a `pws_` sanity check + the per-app family-revocation gate
    //    before honoring the cookie, so this read does the SAME — otherwise
    //    `/session` would keep reporting a revoked user as logged-in for up to the
    //    cookie's ~15 min life (a stale logged-in signal). The revocation gate is
    //    one `SELECT EXISTS` when a DB is configured (skipped in smoke mode); a
    //    revoked family / non-`pws_` sub does NOT return the projection — it falls
    //    through to anchor reload-recovery below, which re-checks the family via
    //    Hydra and ends in `login_required` for a revoked family.
    if !want_mint {
        if let (Some(token), Some(verifier)) = (
            crate::oidc_rp::parse_app_session_cookie(cookie_header, state.config.insecure_dev),
            state.session_verifier.as_ref(),
        ) {
            if let Ok(claims) = verifier.verify(&token, &route.client_id) {
                // Defense in depth (same as the dispatch arm): the cookie `sub`
                // MUST be a `pws_…` pairwise subject. A non-`pws_` cookie is a
                // mint bug — never project it, fall through.
                if zeroship_core::auth::is_pairwise_subject(&claims.sub)
                    && !session_cookie_family_revoked(&state, &claims).await
                {
                    // The cookie already carries the relay-swapped email + pws_ id
                    // (set at issue time). Project them verbatim — no further DB.
                    let user = user_projection(
                        &claims.sub,
                        Some(&claims.email),
                        Some(&claims.name),
                        Some(claims.email_verified),
                    );
                    return identity_projection_ok(
                        &route,
                        state.config.insecure_dev,
                        user,
                        claims.exp,
                        None,
                    );
                }
                // Revoked family / non-`pws_` sub → fall through to anchor
                // reload-recovery (which will end in login_required for a revoked
                // family). Never return the stale logged-in projection.
            }
            // Cookie present but stale/invalid → fall through to anchor
            // reload-recovery below (re-sign a fresh cookie).
        }
    }

    // From here on (reload-recovery / `?mint=1`) we need the DB (the anchor
    // store + the relay-alias read).
    let Some(db_cfg) = state.db.as_ref() else {
        return error_response(
            HttpResponse::ServiceUnavailable(),
            "db_unavailable",
            "no database configured",
        );
    };

    // 2. Reload-recovery (signed cookie gone/expired, or `?mint=1`): read the
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
        let mut conn = match pool.get().await {
            Ok(c) => c,
            Err(e) => return db_error(e),
        };
        // RLS-scoped to `route.app_id` (changeset 0025): an anchor cookie
        // replayed against the wrong app resolves to `None`, so this READ both
        // loads the anchor AND enforces the former post-hoc
        // `anchor.app_id == route.app_id` bind check.
        match anchors::read_live(&mut conn, route.app_id, anchor_id).await {
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

            // Re-write the gateway_sessions ROW from the rotated facts — KEPT as
            // the revocation/audit record (+ auth_time/amr source), NOT read on
            // the per-request path. Carry auth_time/amr forward.
            {
                let pool = match crate::db::checkout(db_cfg).await {
                    Ok(p) => p,
                    Err(e) => return db_error(e),
                };
                let mut conn = match pool.get().await {
                    Ok(c) => c,
                    Err(e) => return db_error(e),
                };
                if let Err(e) = crate::sessions::create(
                    &mut conn,
                    &crate::sessions::NewSession {
                        user_id: &rotated.global_user_id.to_string(),
                        app_id: route.app_id,
                        // Real email stored on the audit row; never read back to
                        // the browser. The rotated raw access JWT does not always
                        // carry the email — leave it `None` when absent.
                        email: None,
                        name: rotated.name.as_deref(),
                        avatar_url: rotated.avatar_url.as_deref(),
                        email_verified: rotated.email_verified.unwrap_or(false),
                        granted_scopes: &rotated.granted_scopes,
                        auth_time: rotated.auth_time,
                        amr: &rotated.amr,
                    },
                )
                .await
                {
                    tracing::error!(error = %e, "/session: gateway_sessions audit re-write failed");
                    return error_response(
                        HttpResponse::InternalServerError(),
                        "internal",
                        "session re-write failed",
                    );
                }
            }

            // Re-SIGN a fresh short-lived signed cookie from the rotated facts —
            // the SPA's live credential, verified locally on the hot path.
            let new_session_cookie = match sign_session_cookie(
                &state,
                &route.client_id,
                &pws_sub,
                relay_email.as_deref(),
                rotated.name.as_deref(),
                rotated.avatar_url.as_deref(),
                rotated.email_verified,
                rotated.auth_time,
                &rotated.amr,
                &rotated.granted_scopes,
            ) {
                Ok(c) => c,
                Err(resp) => return resp,
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
                Some(new_session_cookie),
            )
        }
        Err(RotationError::LoginRequired) => {
            // Anchor-dead: delete the row + clear the breadcrumb.
            if let Ok(pool) = crate::db::checkout(db_cfg).await {
                if let Ok(mut conn) = pool.get().await {
                    let _ = anchors::delete(&mut conn, route.app_id, anchor_id).await;
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

/// Emit the identity-only `{ user, expires_at }` response, refreshing the
/// breadcrumb and (when reload-recovery re-signed a fresh cookie) re-setting the
/// `__Host-zeroship_app_session` cookie. NO JWT in the body — the SPA-facing body
/// carries only `{ user, expires_at }`; the signed cookie travels in the
/// `Set-Cookie` header (HttpOnly, never JS-readable).
fn identity_projection_ok(
    route: &RouteCtx,
    insecure_dev: bool,
    user: serde_json::Value,
    expires_at: i64,
    new_session_cookie: Option<String>,
) -> HttpResponse {
    let mut builder = HttpResponse::Ok();
    builder.header("cache-control", CACHE_NO_STORE);
    if let Some(cookie) = new_session_cookie {
        builder.header("set-cookie", cookie);
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
        let mut conn = match pool.get().await {
            Ok(c) => c,
            Err(e) => return Err(RotationError::Upstream(format!("pool get: {e}"))),
        };
        // RLS GUC keyed on the anchor's own app_id (loaded RLS-scoped to
        // route.app_id, so identical to it).
        if let Err(e) =
            anchors::update_rotated_family(&mut conn, anchor.app_id, anchor.id, &new_enc, &family_id)
                .await
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

/// Whether the signed session cookie's `(app, pws_)` family was revoked after
/// the cookie's `iat` — the SAME per-app family-marker gate the per-request
/// dispatch cookie arm runs ([`crate::router`]'s
/// `resolve_app_session_user_header_inner`). Keeps `GET /__zeroship/auth/session`'s
/// fast path coherent with dispatch so a revoked user is not reported
/// logged-in for the cookie's residual ~15 min life.
///
/// Returns `false` (NOT revoked) when no DB is configured (smoke mode — exactly
/// like the dispatch arm, which skips the gate and authenticates a valid cookie
/// DB-free). Returns `true` (revoked → reject) on a DB checkout/read failure
/// too: the identity read fails CLOSED rather than emit a stale logged-in
/// signal on a blip.
#[allow(clippy::future_not_send)]
async fn session_cookie_family_revoked(
    state: &GateState,
    claims: &crate::session_token::SessionClaims,
) -> bool {
    let Some(db_cfg) = state.db.as_ref() else {
        // No DB ⇒ no revocation store to consult; honor the locally-verified
        // cookie (matches the dispatch arm's smoke-mode behavior).
        return false;
    };
    let pool = match crate::db::checkout(db_cfg).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "/session: revocation pool checkout failed (failing closed)");
            return true;
        }
    };
    let conn = match pool.get().await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "/session: revocation pool get failed (failing closed)");
            return true;
        }
    };
    match zeroship_core::wrapper_revocation::is_family_revoked_since(
        &conn,
        &claims.app,
        &claims.sub,
        claims.iat,
    )
    .await
    {
        Ok(revoked) => revoked,
        Err(e) => {
            tracing::warn!(error = %e, sub = %claims.sub, "/session: revocation check failed (failing closed)");
            true
        }
    }
}

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

/// Mint the gateway-SIGNED `zeroship-sess+jwt` session cookie (BFF slice R1b) from the
/// resolved identity facts, and return the `Set-Cookie` value. The cookie is
/// self-contained: it carries the per-app `pws_` subject, the relay-alias email
/// (or empty — fail closed), `name`/`avatar`/`email_verified`, the granted
/// `scopes`, the route `app` binding (`client_id`), and `auth_time`/`amr`. The
/// per-request cookie arm verifies it LOCALLY and emits `ZeroShip-User` straight
/// from these claims — no DB.
///
/// Returns `Err(response)` (`503`) when the gateway has no signing key (so it
/// cannot sign a session cookie); the SDK treats `503` as retryable.
#[allow(clippy::too_many_arguments)]
fn sign_session_cookie(
    state: &GateState,
    client_id: &str,
    pws_sub: &str,
    relay_email: Option<&str>,
    name: Option<&str>,
    avatar: Option<&str>,
    email_verified: Option<bool>,
    auth_time: Option<i64>,
    amr: &[String],
    scopes: &[String],
) -> Result<String, HttpResponse> {
    let Some(issuer) = state.session_issuer.as_ref() else {
        tracing::error!("/session: no session signing key configured — cannot mint session cookie");
        return Err(error_response(
            HttpResponse::ServiceUnavailable(),
            "session_signing_unavailable",
            "gateway has no session signing key",
        ));
    };
    let token = issuer
        .issue(&crate::session_token::SessionMint {
            app: client_id,
            sub: pws_sub,
            auth_time,
            amr,
            // Relay alias only (fail closed to empty) — NEVER the real inbox.
            email: relay_email.unwrap_or(""),
            email_verified: email_verified.unwrap_or(false),
            name: name.unwrap_or(""),
            avatar,
            scopes,
        })
        .map_err(|e| {
            tracing::error!(error = %e, "/session: session cookie mint failed");
            error_response(
                HttpResponse::InternalServerError(),
                "internal",
                "session cookie mint failed",
            )
        })?;
    Ok(crate::oidc_rp::set_app_session_cookie(&token, state.config.insecure_dev))
}

/// Issue the signed `__Host-zeroship_app_session` cookie for the INTERACTIVE
/// server-rendered OIDC callback (`router::dispatch::handle_auth_callback`), so
/// BOTH the SDK popup flow and the interactive redirect flow produce the SAME
/// signed-cookie shape and the per-request cookie arm has exactly ONE
/// (local-verify) path (no opaque-UUID + `sessions::validate` arm to fork on).
///
/// Derives the per-app `pws_` from the validated id-token `sub` (the global
/// UUID) under the route's `sector`, reads the live relay alias (fail closed to
/// empty), and signs the `zeroship-sess+jwt`. Returns the `Set-Cookie` value, or
/// `Err(msg)` when the route has no `sector` yet / no signing key (the caller
/// renders the callback error page).
///
/// `pub(crate)` so the dispatch callback reuses the SAME mint path as the SDK
/// `POST /session` — one cookie shape, one verifier.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn issue_interactive_session_cookie(
    state: &GateState,
    db_cfg: &crate::db::DbConfig,
    client_id: &str,
    sector: Option<&str>,
    global_user_id: Uuid,
    name: Option<&str>,
    avatar: Option<&str>,
    email_verified: Option<bool>,
    auth_time: Option<i64>,
    amr: &[String],
    scopes: &[String],
) -> Result<String, String> {
    let Some(sector) = sector else {
        return Err("app has no sector_identifier yet".to_string());
    };
    let pws_sub = zeroship_core::auth::derive_pairwise(
        &state.pairwise_salt,
        &global_user_id.to_string(),
        sector,
    );
    let relay_email = relay_alias_for(db_cfg, client_id, global_user_id).await;
    sign_session_cookie(
        state,
        client_id,
        &pws_sub,
        relay_email.as_deref(),
        name,
        avatar,
        email_verified,
        auth_time,
        amr,
        scopes,
    )
    .map_err(|_| "session cookie mint failed".to_string())
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
