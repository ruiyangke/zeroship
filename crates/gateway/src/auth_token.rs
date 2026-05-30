//! `POST /__zs/auth/token` + `GET /__zs/auth/session` — the browser-token
//! CORE of the `@zeroship/auth` SDK (auth-sdk Slice 1b-anchors, spec §1.2).
//!
//! - **`POST /__zs/auth/token`** runs the PKCE code→token exchange on the
//!   browser's behalf, stores the rotating refresh family server-side
//!   (encrypted) under a NEW `auth.app_session_anchors` row, mints a 10-min
//!   per-app WRAPPER access token whose `sub` is the per-app pairwise `pws_`
//!   (§6.2/G4 — the global user UUID never reaches the browser), sets the
//!   `__Host-zs_app_session` anchor cookie + the `zs.<host>.is.authenticated`
//!   breadcrumb, and returns `{access_token, token_type, expires_in:600, user}`.
//! - **`GET /__zs/auth/session?mint=1`** is the SOLE reload-recovery path:
//!   it reads the anchor cookie, loads the anchor, and mints a fresh
//!   wrapper from the server-held refresh family — refreshing at Hydra ONLY
//!   when the short cached wrapper is expired. Without `mint=1` it returns
//!   just the user (no fresh token).
//!
//! ## Same-origin-only (CORS is NOT the boundary, §1.2 round-3)
//!
//! Both endpoints emit NO `Access-Control-Allow-Origin` /
//! `allow-credentials`. The boundary is the conjunction of: a custom
//! `X-ZS-Auth` header, an exact `Origin == app origin` match (foreign /
//! `null` rejected; no reflection), and `Sec-Fetch-Site: same-origin`
//! (enforced when present). `?mint=1` additionally REQUIRES `X-ZS-Auth` so
//! a top-level navigation cannot mint.
//!
//! ## Mint single-flight (round-6 BLOCKER, §1.2)
//!
//! Concurrent `?mint=1` minters for the same anchor on one worker thread
//! coalesce into ONE Hydra refresh via a per-thread in-process
//! single-flight keyed on `anchor_id` ([`crate::anchors::with_single_flight`]),
//! plus a seconds-scale cached wrapper under the anchor so a reload-storm
//! skips Hydra entirely. **NO db connection or lock is held across the
//! Hydra HTTP call**: the mint future checks a pooled connection out, reads
//! the anchor, RELEASES it, does the Hydra refresh, then checks another out
//! to persist.

use std::sync::Arc;

use futures::FutureExt as _;
use ntex::util::Bytes;
use ntex::web::{types::State, HttpRequest, HttpResponse};
use serde_json::json;
use uuid::Uuid;

use crate::anchors::{self, MintError, MintOk, MintResult};
use crate::oidc_rp::TokenSet;
use crate::GateState;

pub(crate) const CACHE_NO_STORE: &str = "no-store";
const X_ZS_AUTH: &str = "x-zs-auth";

/// Headroom (seconds) the cached-wrapper short-circuit requires before
/// serving an in-window wrapper, so a wrapper about to fall out of the server
/// cache window forces a fresh rotation instead (§1.2 line 1092:
/// `cached_access_exp > now()+skew`).
const CACHE_SKEW_SECS: i64 = 2;

/// Resolved per-request route facts the auth endpoints need.
pub(crate) struct RouteCtx {
    pub(crate) app_name: String,
    pub(crate) host: String,
    pub(crate) client_id: String,
    /// The app's stable apex origin used to scope the per-app pairwise
    /// `pws_…` subject (§6.2). `None` until the control plane provisions
    /// it; the browser wrapper then hard-fails closed (no `pws_`, 503)
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
    let Some((_, route)) = state.routes.lookup_by_name(&app_name) else {
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

    Ok(RouteCtx { app_name, host, client_id, sector_identifier })
}

/// Derive the per-app pairwise subject (`pws_…`) the browser wrapper carries,
/// so app JS decoding its own access token reads a per-app pseudonym, never
/// the global user UUID (§6.2/G4). Hard-fails (`None`) when the route has no
/// `sector_identifier` yet — the caller answers `503 client_not_provisioned`
/// rather than ship a wrapper containing the global UUID.
fn pairwise_sub(state: &GateState, route: &RouteCtx, global_user_id: &str) -> Option<String> {
    let sector = route.sector_identifier.as_deref()?;
    Some(zeroship_core::auth::derive_pairwise(
        &state.pairwise_salt,
        global_user_id,
        sector,
    ))
}

/// Resolve the ACTIVE relay alias for `(route.client_id, global_user_id)` —
/// the email-claim swap source for the browser wrapper + `/token`/`/session`
/// responses (relay sub-spec §7). The browser holds this wrapper and the SDK
/// decodes it, so the `email` it carries MUST be the alias, never the user's
/// real address.
///
/// Returns `None` when no alias is minted yet OR the grant was revoked
/// (`revoked_at IS NULL` gate) — the caller then projects an EMPTY email (fail
/// closed), NEVER the real one. A DB checkout/read failure also yields `None`
/// (fail closed): a browser token must never leak the real email on a blip.
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

/// `POST /__zs/auth/token` — code→token exchange (+ optional browser
/// refresh-grant). See module docs.
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

    // Issuer/verifier must be configured — without the signing key the
    // gateway cannot mint a wrapper. 503 exactly like dpop-exchange.
    let Some(issuer) = state.wrapper_issuer.as_ref() else {
        return error_response(
            HttpResponse::ServiceUnavailable(),
            "signing_not_configured",
            "wrapper signing key absent",
        );
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

    // Only the authorization_code grant is in scope for 1b-anchors. The
    // browser refresh-grant (browser_refresh mode) is a sibling slice.
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
    //    via the gateway's Hydra JWKS. `expected_nonce=None` (§1.2 round-5 —
    //    the nonce is the SDK's own sessionStorage cross-flow guard, never
    //    echoed to the gateway). at_hash/c_hash bind the id_token to the
    //    access token + code.
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

    // 3. The global user UUID is the Hydra sub. It is stored on the anchor
    //    (server-side only) and projected to the per-app `pws_` for the
    //    browser-held wrapper (step 5) — the global UUID never leaves the
    //    gateway in a browser token.
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

    // 5. Mint the browser wrapper with the per-app pairwise `pws_` subject
    //    (§6.2/G4) — NOT the global Hydra UUID, so app JS decoding its own
    //    access_token cannot read the global user id or correlate across
    //    apps. cnf=None, wraps=None; 600 s. This is the access_token the
    //    browser holds.
    // Derive on the CANONICAL UUID string (Batch A M1) — `derive_pairwise`
    // canonicalizes internally, but feeding it the already-parsed
    // `global_user_id` here keeps the mint convention identical to the
    // `/signout` / control-cascade writers (which derive on
    // `Uuid::to_string()`) at the call site too, not just in the helper.
    let Some(pws_sub) = pairwise_sub(&state, &route, &global_user_id.to_string()) else {
        // No sector_identifier yet ⇒ we cannot derive the pairwise sub.
        // Fail closed rather than ship a wrapper with the global UUID. The
        // SDK treats 503 as retryable and keeps the breadcrumb.
        return error_response(
            HttpResponse::ServiceUnavailable(),
            "client_not_provisioned",
            "app has no sector_identifier yet",
        );
    };
    // Email-claim swap (§7): the browser-held wrapper + the user projection
    // carry the relay ALIAS, never the real `claims.email`. No active alias
    // (not yet minted at consent, or revoked) ⇒ empty email (fail closed) —
    // the real address NEVER reaches the browser.
    let relay_email = relay_alias_for(db_cfg, &route.client_id, global_user_id).await;
    let scope = tokens.scope.clone().unwrap_or_default();
    let wrapper = match issuer.issue(&crate::wrapper_token::WrapperMint {
        aud: &route.host,
        sub: &pws_sub,
        scope: &scope,
        client_id: &route.client_id,
        exp_secs: anchors::WRAPPER_TTL_SECS,
        cnf: None,
        wraps: None,
        email: relay_email.as_deref(),
        email_verified: claims.email_verified,
        name: claims.name.as_deref(),
    }) {
        Ok(t) => t,
        Err(e) => {
            tracing::error!(error = %e, "/token: wrapper issue failed");
            return error_response(
                HttpResponse::InternalServerError(),
                "internal",
                "wrapper issue failed",
            );
        }
    };

    // 6. Create the anchor row (abs_expires_at = created_at + 30d, set once).
    //    `refresh_family_id` is a gateway-generated lineage id set ONCE here
    //    and carried verbatim across every rotation — Hydra's TokenSet has no
    //    usable family-lineage field, so we mint our own stable id rather than
    //    persist the (meaningless) scope string in a column named *family_id*.
    let scopes: Vec<String> = scope.split_whitespace().map(str::to_string).collect();
    let family_id = zeroship_core::typed_id::generate("rfam");
    let anchor = {
        let pool = match crate::db::checkout(db_cfg).await {
            Ok(p) => p,
            Err(e) => return db_error(e),
        };
        let conn = match pool.get().await {
            Ok(c) => c,
            Err(e) => return db_error(e),
        };
        match anchors::create(
            &conn,
            &anchors::NewAnchor {
                app_id: &route.app_name,
                client_id: &route.client_id,
                global_user_id,
                refresh_token_enc: &refresh_enc,
                refresh_family_id: &family_id,
                granted_scopes: &scopes,
                cached_access_token: Some(&wrapper),
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
        }
        // `conn` (and `pool`) drop here — released before we build the
        // response. No connection is held across any outbound call.
    };

    // User.id is the per-app `pws_` (§6.3), matching the wrapper `sub` — the
    // global UUID never reaches the browser. `email` is the relay alias (§7),
    // never the real `claims.email`.
    let user = user_projection(&pws_sub, relay_email.as_deref(), claims.name.as_deref(), claims.email_verified, &scopes);

    HttpResponse::Ok()
        .header("cache-control", CACHE_NO_STORE)
        .header(
            "set-cookie",
            anchors::set_anchor_cookie(&anchor.id, state.config.insecure_dev),
        )
        .header(
            "set-cookie",
            anchors::set_breadcrumb_cookie(&route.host, state.config.insecure_dev),
        )
        .json(&json!({
            "access_token": wrapper,
            "token_type": "Bearer",
            "expires_in": anchors::WRAPPER_TTL_SECS,
            "scope": scope,
            "user": user,
        }))
}

/// `GET /__zs/auth/session` — reload-recovery. With `?mint=1` mints a fresh
/// wrapper from the server-held family (single-flight + short cached
/// wrapper). Without `?mint=1` returns just the user projection.
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
    // navigation cannot set it ⇒ cannot mint). A non-mint GET is harmless,
    // so the custom header is not required there. GET is not strictly
    // state-changing, so a missing Origin is tolerated (the X-ZS-Auth
    // requirement carries the mint defense).
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

    // Read the anchor by cookie. Released immediately (NO conn held across
    // the Hydra refresh).
    let Some(anchor_id) = req
        .headers()
        .get(http::header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|c| anchors::parse_anchor_cookie(c, state.config.insecure_dev))
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
    // still match the resolved route).
    if anchor.app_id != route.app_name {
        return login_required(&route.host, state.config.insecure_dev);
    }

    // Project the user id as the per-app `pws_` (§6.3) — the bare /session
    // response must NOT leak the global UUID to the browser either. Fail
    // closed if the sector is missing (same posture as /token).
    let scopes = anchor.granted_scopes.clone();
    let Some(pws_sub) = pairwise_sub(&state, &route, &anchor.global_user_id.to_string()) else {
        return error_response(
            HttpResponse::ServiceUnavailable(),
            "client_not_provisioned",
            "app has no sector_identifier yet",
        );
    };
    // Email-claim swap (§7): the /session user projection carries the relay
    // ALIAS, never the real email. None ⇒ empty (fail closed).
    let relay_email = relay_alias_for(db_cfg, &route.client_id, anchor.global_user_id).await;
    let user = user_projection(&pws_sub, relay_email.as_deref(), None, None, &scopes);

    if !want_mint {
        // No fresh token — just the user. Refresh the breadcrumb so it
        // tracks the live anchor.
        return HttpResponse::Ok()
            .header("cache-control", CACHE_NO_STORE)
            .header(
                "set-cookie",
                anchors::set_breadcrumb_cookie(&route.host, state.config.insecure_dev),
            )
            .json(&json!({ "user": user }));
    }

    // ?mint=1 — mint a fresh wrapper from the server-held family via the
    // per-node single-flight (one Hydra refresh for N concurrent minters).
    match mint(&state, &route, &anchor).await {
        Ok(MintOk { access_token, expires_in }) => {
            let exp_at = now_secs() + expires_in;
            HttpResponse::Ok()
                .header("cache-control", CACHE_NO_STORE)
                .header(
                    "set-cookie",
                    anchors::set_breadcrumb_cookie(&route.host, state.config.insecure_dev),
                )
                .json(&json!({
                    "user": user,
                    "access_token": access_token,
                    "token_type": "Bearer",
                    "expires_in": expires_in,
                    "expires_at": exp_at,
                }))
        }
        Err(MintError::LoginRequired) => {
            // Anchor-dead: delete the row + clear the breadcrumb.
            if let Ok(pool) = crate::db::checkout(db_cfg).await {
                if let Ok(conn) = pool.get().await {
                    let _ = anchors::delete(&conn, anchor_id).await;
                }
            }
            login_required(&route.host, state.config.insecure_dev)
        }
        Err(MintError::Upstream(msg)) => {
            tracing::warn!(error = %msg, "/session?mint=1: upstream mint failure");
            // Retryable/recovering — do NOT clear the breadcrumb (§4.3).
            error_response(
                HttpResponse::ServiceUnavailable(),
                "temporarily_unavailable",
                "mint upstream failure",
            )
        }
    }
}

/// Mint a fresh wrapper for `anchor` (the `?mint=1` core, §1.2 round-6).
///
/// (1) SHORT-CIRCUIT on the per-anchor cached wrapper (in-window ⇒ no Hydra,
///     no single-flight). (2) Otherwise coalesce concurrent minters for this
///     anchor on THIS worker thread into ONE Hydra refresh via the
///     per-thread single-flight. NO db connection is held across the Hydra
///     call.
#[allow(clippy::future_not_send)]
async fn mint(state: &Arc<GateState>, route: &RouteCtx, anchor: &anchors::Anchor) -> MintResult {
    // (1) Cached-wrapper short-circuit (no lock, no single-flight, no Hydra).
    if let (Some(cached), Some(exp)) = (&anchor.cached_access_token, anchor.cached_access_exp) {
        // Serve the in-window cached WRAPPER, but require a little headroom
        // (CACHE_SKEW_SECS) so we don't hand out a wrapper that is about to
        // fall out of the server cache window (§1.2 line 1092:
        // `cached_access_exp > now()+skew`).
        if exp.timestamp() > now_secs() + CACHE_SKEW_SECS {
            return Ok(MintOk {
                access_token: cached.clone(),
                expires_in: anchors::WRAPPER_TTL_SECS,
            });
        }
    }

    // (2) PER-NODE SINGLE-FLIGHT keyed on anchor.id. If another task on THIS
    //     thread already started the refresh, await its shared future.
    let anchor_id = anchor.id;
    if let Some(existing) = anchors::with_single_flight(|sf| sf.get(anchor_id)) {
        return existing.await;
    }

    // We are the leader: build the refresh future and register it. Removal of
    // the single-flight entry is tied to the SHARED future's resolution, NOT
    // to this leader task's survival: an `EntryGuard` owned by the future body
    // removes the entry when the future is dropped (the round-6 BLOCKER
    // invariant `remove single_flight.entry once fut resolves`). So if this
    // leader's request future is cancelled mid-flight (client disconnect /
    // ntex timeout) after `insert`, a surviving follower still drives the
    // shared future to completion, and the guard fires on drop — the entry is
    // cleared and never leaks a resolved-but-stuck wrapper to later callers.
    let st = Arc::clone(state);
    let client_id = route.client_id.clone();
    let host = route.host.clone();
    let sector = route.sector_identifier.clone();
    let anchor = anchor.clone();
    let fut: anchors::SharedMintFuture = (Box::pin(async move {
        // The guard's Drop removes `anchor_id` from this worker thread's
        // single-flight map when the future body is dropped (after it
        // resolves, or when the last awaiter drops it).
        let _guard = anchors::EntryGuard::new(anchor_id);
        do_refresh(&st, &client_id, &host, sector.as_deref(), &anchor).await
    }) as std::pin::Pin<Box<dyn std::future::Future<Output = MintResult>>>)
        .shared();

    let shared = anchors::with_single_flight(|sf| sf.insert(anchor_id, fut));
    // Drive the shared future. Removal is the guard's job (above), so we do
    // NOT call `remove` here — that would only fire on the leader's survival
    // and is exactly the leak the guard fixes.
    shared.await
}

/// The coalesced refresh body: one Hydra `/oauth2/token` refresh, then
/// rebuild the wrapper locally (NO introspection — §1.2 round-5) and
/// persist the rotated family + cached wrapper. Holds NO db connection
/// across the Hydra call.
#[allow(clippy::future_not_send)]
async fn do_refresh(
    state: &Arc<GateState>,
    client_id: &str,
    host: &str,
    sector: Option<&str>,
    anchor: &anchors::Anchor,
) -> MintResult {
    let Some(issuer) = state.wrapper_issuer.as_ref() else {
        return Err(MintError::Upstream("wrapper signing key absent".into()));
    };
    let Some(db_cfg) = state.db.as_ref() else {
        return Err(MintError::Upstream("no database".into()));
    };
    // Without a sector we cannot derive the per-app `pws_` subject; fail
    // closed rather than ever fall back to the global UUID in the wrapper.
    let Some(sector) = sector else {
        return Err(MintError::Upstream("no sector_identifier for pairwise sub".into()));
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
        Err(e) => return Err(MintError::Upstream(format!("refresh decrypt: {e}"))),
    };

    // Hydra refresh — NO db connection held here.
    let tokens: TokenSet = match state.oidc_rp.refresh_token_public(client_id, &refresh).await {
        Ok(t) => t,
        Err(e) => {
            // Distinguish anchor-dead (invalid_grant → 720h ceiling / family
            // revoked) from a transient upstream failure.
            let msg = e.to_string();
            if msg.contains("invalid_grant") {
                return Err(MintError::LoginRequired);
            }
            return Err(MintError::Upstream(msg));
        }
    };

    // Rebuild the wrapper from the rotated RAW access JWT, verified LOCALLY
    // via the gateway JWKS (no per-mint introspection, §1.2 round-5).
    let raw = match state.oidc_rp.verify_access_token(&tokens.access_token).await {
        Ok(c) => c,
        Err(e) => return Err(MintError::Upstream(format!("rotated access verify: {e}"))),
    };
    // Bind the rotated raw JWT to THIS app's client (§1.2 line 1105:
    // `verify_access_token(resp.access_token, route.oauth_client_id)`).
    // `verify_access_token` checks only sig+iss+exp, so confirm the
    // `client_id` claim (RFC 9068 §3) matches the resolved app before we
    // project its claims into the wrapper — a token issued to another client
    // must never become this app's wrapper.
    if let Some(tok_client) = raw.client_id.as_deref() {
        if tok_client != client_id {
            return Err(MintError::Upstream(format!(
                "rotated access client_id mismatch: {tok_client} != {client_id}"
            )));
        }
    }
    // Per-app pairwise `pws_` subject (§6.2/G4) — the global UUID never
    // reaches the browser, on the refresh path just as on /token. Derive on
    // the CANONICAL UUID (`sub` = `anchor.global_user_id.to_string()`), NOT the
    // rotated `raw.sub` (Batch A M1): the anchor's UUID is the stable identity
    // and matches the `/signout` / control-cascade writers' convention, so a
    // refreshed wrapper's `pws_` is byte-identical to the revocation marker's.
    let pws_sub = zeroship_core::auth::derive_pairwise(&state.pairwise_salt, &sub, sector);
    // Email-claim swap (§7): the rotated wrapper carries the relay ALIAS, never
    // the real `raw.email`. None ⇒ empty (fail closed) — the real address never
    // reaches the browser on the refresh path either.
    let relay_email = relay_alias_for(db_cfg, client_id, anchor.global_user_id).await;
    let scope = tokens
        .scope
        .clone()
        .or(raw.scope.clone())
        .unwrap_or_default();
    let wrapper = match issuer.issue(&crate::wrapper_token::WrapperMint {
        aud: host,
        sub: &pws_sub,
        scope: &scope,
        client_id,
        exp_secs: anchors::WRAPPER_TTL_SECS,
        cnf: None,
        wraps: None,
        email: relay_email.as_deref(),
        email_verified: raw.email_verified,
        name: raw.name.as_deref(),
    }) {
        Ok(t) => t,
        Err(e) => return Err(MintError::Upstream(format!("wrapper issue: {e}"))),
    };

    // Persist the rotated family + cached wrapper (checkout, write, release).
    let new_refresh = tokens.refresh_token.as_deref().unwrap_or(&refresh);
    let new_enc =
        match zeroship_core::crypto::encrypt(&state.anchor_enc_key, &aad, new_refresh.as_bytes()) {
            Ok(ct) => ct,
            Err(e) => return Err(MintError::Upstream(format!("refresh encrypt: {e}"))),
        };
    // Carry the anchor's OWN gateway-generated lineage id verbatim across the
    // rotation — never overwrite it with the OAuth scope string (which is not
    // a family id). Hydra's TokenSet exposes no usable family-lineage field.
    let family_id = anchor.refresh_family_id.clone();
    {
        let pool = match crate::db::checkout(db_cfg).await {
            Ok(p) => p,
            Err(e) => return Err(MintError::Upstream(format!("pool checkout: {e}"))),
        };
        let conn = match pool.get().await {
            Ok(c) => c,
            Err(e) => return Err(MintError::Upstream(format!("pool get: {e}"))),
        };
        if let Err(e) =
            anchors::update_minted(&conn, anchor.id, &new_enc, &family_id, &wrapper).await
        {
            return Err(MintError::Upstream(format!("anchor update: {e}")));
        }
    }

    Ok(MintOk {
        access_token: wrapper,
        expires_in: anchors::WRAPPER_TTL_SECS,
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

fn user_projection(
    id: &str,
    email: Option<&str>,
    name: Option<&str>,
    email_verified: Option<bool>,
    scopes: &[String],
) -> serde_json::Value {
    json!({
        "id": id,
        "email": email,
        "name": name,
        "avatar": serde_json::Value::Null,
        "email_verified": email_verified.unwrap_or(false),
        "scopes": scopes,
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
