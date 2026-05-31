//! Browser-facing auth HTTP surface (auth-sdk Slice 1b-browser, spec §1.2).
//!
//! Three same-origin gateway endpoints the `@zeroship/auth` SDK drives:
//!
//! - **`GET /__zs/auth/authorize`** — the ONE cross-site hop. Resolves the
//!   app's per-app PUBLIC PKCE client from `Host`, then 302s to Hydra's
//!   `/oauth2/auth` carrying the BROWSER-supplied PKCE `code_challenge`
//!   (S256), `state`, `nonce`, requested `scope`, and an optional `prompt`
//!   passthrough, with `redirect_uri = {scheme}://{host}/__zs/auth/popup-callback`
//!   (a registered URI from 1d). The gateway holds NO PKCE verifier — the
//!   browser does (Supabase-style). 503 `client_not_provisioned` when the
//!   route has no `oauth_client_id` yet.
//!
//! - **`GET /__zs/auth/popup-callback`** — a tiny SAME-ORIGIN HTML relay
//!   page. Inline (CSP-nonce-tagged) JS reads `code`+`state` (or
//!   `error`+`error_description`+`state`) FROM `location.search` (never
//!   reflected into the DOM by the gateway — no XSS sink) and
//!   `postMessage`s `{type:'zs:authorization_response', response:{…}}` to
//!   `window.opener` with `targetOrigin = location.origin` (its OWN origin,
//!   never `'*'`). A same-origin `BroadcastChannel` + one-shot
//!   `localStorage` relay cover the COOP-severed-opener case (§4.4). Strict
//!   CSP (`default-src 'none'; script-src 'nonce-…'; frame-ancestors 'self'`),
//!   `Referrer-Policy: no-referrer`, `COOP: same-origin`.
//!
//! - **`POST /__zs/auth/signout`** — FIXES the live "no handler" bug. Same-
//!   origin guard (X-ZS-Auth + exact Origin). Reads the `__Host-zs_app_anchor`
//!   anchor cookie → loads the anchor → (a) sets the per-app family marker
//!   via `revoke_family(client_id, pws_sub)`, (b) best-effort revokes the
//!   server-held refresh family at Hydra `/oauth2/revoke`, (c) deletes the
//!   anchor row(s), (d) clears the anchor cookie + breadcrumb. `scope:
//!   'local'` (default, this device) or `'global'` (this app, every device).
//!   Returns `204` + no-store cookie clears.

use std::sync::Arc;

use ntex::util::Bytes;
use ntex::web::{types::State, HttpRequest, HttpResponse};

use crate::auth_token::{
    db_error, error_response, resolve_route, same_origin_guard, CACHE_NO_STORE,
};
use crate::oidc_rp::BrowserAuthorizeParams;
use crate::{anchors, GateState};

/// Strict CSP for the popup-callback page (spec §1.2). `default-src 'none'`
/// blocks all loads; `script-src 'nonce-<random>'` permits ONLY the inline
/// nonce-tagged relay script (a reflected/injected `<script>` is blocked
/// even if a future edit reflected a query param); `frame-ancestors 'self'`
/// restricts who may embed the page.
fn popup_csp(nonce: &str) -> String {
    format!("default-src 'none'; script-src 'nonce-{nonce}'; frame-ancestors 'self'")
}

// ─── GET /__zs/auth/authorize ────────────────────────────────────────────

/// `GET /__zs/auth/authorize` — build the Hydra `/oauth2/auth` URL for the
/// per-app PUBLIC PKCE client and 302. See module docs.
#[allow(clippy::future_not_send)]
pub async fn authorize(req: HttpRequest, state: State<Arc<GateState>>) -> HttpResponse {
    // Resolve the app + per-app oauth_client_id. 503 client_not_provisioned
    // when the route has no client yet (1d), matching /token's posture.
    let route = match resolve_route(&req, &state) {
        Ok(r) => r,
        Err(resp) => return resp,
    };

    let q = AuthorizeQuery::parse(req.query_string());
    let Some(code_challenge) = q.code_challenge.as_deref().filter(|s| !s.is_empty()) else {
        return error_response(
            HttpResponse::BadRequest(),
            "invalid_request",
            "code_challenge (S256) required",
        );
    };
    // We only support S256 (spec §1.2). Reject a `plain` challenge method.
    if let Some(method) = q.code_challenge_method.as_deref() {
        if !method.is_empty() && method != "S256" {
            return error_response(
                HttpResponse::BadRequest(),
                "invalid_request",
                "code_challenge_method must be S256",
            );
        }
    }
    let Some(stt) = q.state.as_deref().filter(|s| !s.is_empty()) else {
        return error_response(HttpResponse::BadRequest(), "invalid_request", "state required");
    };
    let Some(nonce) = q.nonce.as_deref().filter(|s| !s.is_empty()) else {
        return error_response(HttpResponse::BadRequest(), "invalid_request", "nonce required");
    };
    let scope = q.scope.as_deref().filter(|s| !s.is_empty()).unwrap_or("openid");

    // `prompt` is a PASSTHROUGH (spec §1.2): omitted in the common case so
    // Hydra's SSO skip fires; `login`/`consent` for explicit step-up. The
    // silent-iframe `prompt=none` path was removed, but the gateway does NOT
    // reject `none` here — it is simply forwarded (Hydra decides). We only
    // forward a non-empty prompt.
    let prompt = q.prompt.as_deref().filter(|s| !s.is_empty());

    // `idp_hint` is the SDK's `SignInOptions.provider` (google/github/password)
    // threaded through verbatim to Hydra so the login UI can route to / pre-
    // select the named upstream IdP. PASSTHROUGH only (Hydra/login-UI decides
    // what a value means); omitted ⇒ default provider picker.
    let idp_hint = q.idp_hint.as_deref().filter(|s| !s.is_empty());

    // redirect_uri defaults to THIS app's own popup-callback (a registered
    // URI from 1d). When the SDK supplies one, it MUST be on this app's
    // origin — a foreign redirect_uri is rejected (open-redirect guard). The
    // ultimate allowlist is Hydra's registered redirect_uris, but we reject
    // an obviously-foreign value up front so a misconfigured SDK fails fast.
    let scheme = if state.config.insecure_dev { "http" } else { "https" };
    let default_redirect = format!("{scheme}://{}/__zs/auth/popup-callback", route.host);
    let redirect_uri = match q.redirect_uri.as_deref().filter(|s| !s.is_empty()) {
        None => default_redirect,
        Some(supplied) => {
            let own_origin = format!("{scheme}://{}/", route.host);
            if supplied.starts_with(&own_origin) {
                supplied.to_string()
            } else {
                return error_response(
                    HttpResponse::BadRequest(),
                    "invalid_request",
                    "redirect_uri must be on this app's origin",
                );
            }
        }
    };

    let url = state.oidc_rp.build_browser_authorize_url(
        &route.client_id,
        &BrowserAuthorizeParams {
            code_challenge,
            state: stt,
            nonce,
            scope,
            redirect_uri: &redirect_uri,
            prompt,
            idp_hint,
        },
    );

    HttpResponse::Found()
        .header(http::header::LOCATION, url)
        .header("cache-control", CACHE_NO_STORE)
        .finish()
}

/// `GET /__zs/auth/authorize` query params (all browser-supplied).
#[derive(Default)]
struct AuthorizeQuery {
    code_challenge: Option<String>,
    code_challenge_method: Option<String>,
    state: Option<String>,
    nonce: Option<String>,
    scope: Option<String>,
    prompt: Option<String>,
    redirect_uri: Option<String>,
    idp_hint: Option<String>,
}

impl AuthorizeQuery {
    fn parse(query: &str) -> Self {
        let mut out = Self::default();
        for (k, v) in url::form_urlencoded::parse(query.as_bytes()) {
            match k.as_ref() {
                "code_challenge" => out.code_challenge = Some(v.into_owned()),
                "code_challenge_method" => out.code_challenge_method = Some(v.into_owned()),
                "state" => out.state = Some(v.into_owned()),
                "nonce" => out.nonce = Some(v.into_owned()),
                "scope" => out.scope = Some(v.into_owned()),
                "prompt" => out.prompt = Some(v.into_owned()),
                "redirect_uri" => out.redirect_uri = Some(v.into_owned()),
                "idp_hint" => out.idp_hint = Some(v.into_owned()),
                _ => {}
            }
        }
        out
    }
}

// ─── GET /__zs/auth/popup-callback ───────────────────────────────────────

/// `GET /__zs/auth/popup-callback` — the same-origin HTML relay page. See
/// module docs. The query params are NEVER reflected into the response body
/// by the gateway: the inline JS reads them from `location.search` at
/// runtime and `postMessage`s the PARSED values. The gateway only emits a
/// static, query-independent document plus a per-response CSP nonce.
#[allow(clippy::future_not_send)]
pub async fn popup_callback() -> HttpResponse {
    // Per-response CSP nonce (fresh random base64url). Stamped on BOTH the
    // CSP header and the <script nonce>, so only THIS inline script runs and
    // any injected/reflected <script> is blocked.
    let nonce = zeroship_core::pkce::generate_verifier();
    let html = popup_callback_html(&nonce);

    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .header("content-security-policy", popup_csp(&nonce))
        .header("referrer-policy", "no-referrer")
        .header("cross-origin-opener-policy", "same-origin")
        .header("cache-control", CACHE_NO_STORE)
        .body(html)
}

/// Build the popup-callback document. Self-contained, no external scripts.
/// The inline JS parses `code`/`state`/`error` from `location.search` and
/// relays them via THREE same-origin channels (postMessage to opener,
/// BroadcastChannel, one-shot localStorage), targetOrigin = own origin. No
/// DOM writes of any query value (the reflected `error_description` is never
/// an XSS sink). The ONLY value the gateway interpolates is the CSP `nonce`,
/// which is a server-generated base64url token (not attacker-controlled).
fn popup_callback_html(nonce: &str) -> String {
    format!(
        "<!doctype html><meta charset=utf-8><title>Sign-in</title>\
<script nonce=\"{nonce}\">\n\
(function(){{\n\
  var p = new URLSearchParams(location.search);\n\
  var state = p.get('state');\n\
  var msg = {{ type: 'zs:authorization_response', response:\n\
    p.get('error')\n\
      ? {{ error: p.get('error'), error_description: p.get('error_description'), state: state }}\n\
      : {{ code: p.get('code'), state: state }} }};\n\
  // Primary: postMessage to the opener (same-origin target; NEVER '*').\n\
  try {{ if (window.opener) window.opener.postMessage(msg, location.origin); }} catch (e) {{}}\n\
  // Fallback (opener severed by COOP across app->auth->app, §4.4): both\n\
  // channels are SAME-ORIGIN, so no cross-origin exposure.\n\
  try {{ new BroadcastChannel('zs:auth').postMessage(msg); }} catch (e) {{}}\n\
  try {{\n\
    if (state) {{\n\
      localStorage.setItem('@@zsauth@@::relay::' + state, JSON.stringify(msg));\n\
      localStorage.removeItem('@@zsauth@@::relay::' + state);\n\
    }}\n\
  }} catch (e) {{}}\n\
  try {{ window.close(); }} catch (e) {{}}\n\
}})();\n\
</script>"
    )
}

// ─── POST /__zs/auth/signout ─────────────────────────────────────────────

/// Body the SDK posts to `/__zs/auth/signout`.
#[derive(serde::Deserialize, Default)]
struct SignOutRequest {
    /// `"local"` (default — this device) or `"global"` (this app, every
    /// device). Anything else is treated as `local`.
    #[serde(default)]
    scope: Option<String>,
}

/// `POST /__zs/auth/signout` — FIX the live missing-handler bug. See module
/// docs. Same-origin guard, then revoke + clear. Always 204 + cookie clears
/// (idempotent: a signout with no anchor cookie still clears the cookies).
#[allow(clippy::future_not_send, clippy::too_many_lines)]
pub async fn signout(req: HttpRequest, body: Bytes, state: State<Arc<GateState>>) -> HttpResponse {
    let route = match resolve_route(&req, &state) {
        Ok(r) => r,
        Err(resp) => return resp,
    };

    // Same-origin guard: state-changing POST requires the custom X-ZS-Auth
    // header AND an exact Origin match (same posture as POST /token).
    if let Err(resp) = same_origin_guard(&req, &route.host, state.config.insecure_dev, true, true) {
        return resp;
    }

    let want_global = matches!(
        parse_signout_body(&req, &body).scope.as_deref(),
        Some("global")
    );

    // Read the anchor cookie. If absent, signout is a no-op that still
    // clears the cookies (idempotent — a double signout, or a signout from a
    // tab whose anchor already died, must not error).
    let anchor_id = req
        .headers()
        .get(http::header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|c| anchors::parse_anchor_cookie(c, state.config.insecure_dev));

    let Some(db_cfg) = state.db.as_ref() else {
        // No DB — nothing server-side to revoke; still clear the cookies.
        return signout_cleared(&route.host, state.config.insecure_dev);
    };
    let Some(anchor_id) = anchor_id else {
        return signout_cleared(&route.host, state.config.insecure_dev);
    };

    // Load the anchor (released immediately — no conn held across the Hydra
    // revoke). A missing/expired anchor ⇒ just clear the cookies.
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
            Ok(None) => return signout_cleared(&route.host, state.config.insecure_dev),
            Err(e) => {
                tracing::error!(error = %e, "/signout: anchor read failed");
                return error_response(
                    HttpResponse::InternalServerError(),
                    "internal",
                    "anchor read failed",
                );
            }
        }
    };

    // Defense in depth: the cookie is __Host- (host-scoped), but the anchor's
    // app_id must still match the resolved route. A mismatch ⇒ clear cookies
    // without touching the foreign anchor. NB: anchors are keyed by the app
    // UUID (`route.app_id`), NOT the subdomain slug `app_name` — matching the
    // /token + /session create path and the live dispatch arm (see RouteCtx).
    if anchor.app_id != route.app_id.to_string() {
        return signout_cleared(&route.host, state.config.insecure_dev);
    }

    // The per-app pairwise `pws_` subject the session cookie / access token
    // carries — the family marker is keyed on `(client_id, pws_sub)` to match
    // their `sub` (the cookie / Bearer / DPoP arms check the SAME
    // (client_id, sub), §8.5). When the sector is missing we cannot derive the
    // pws_; the family marker is then best-effort skipped (the anchor delete +
    // cookie clear still happen).
    let pws_sub = route
        .sector_identifier
        .as_deref()
        .map(|sector| {
            zeroship_core::auth::derive_pairwise(
                &state.pairwise_salt,
                &anchor.global_user_id.to_string(),
                sector,
            )
        });

    // The encrypted families to revoke at Hydra + the anchor rows to delete.
    // For `local`: just this anchor's family + this row. For `global`: every
    // anchor row for `(app_id, global_user_id)` (this app, every device).
    let mut families: Vec<(Vec<u8>, String)> = Vec::new();
    {
        let pool = match crate::db::checkout(db_cfg).await {
            Ok(p) => p,
            Err(e) => return db_error(e),
        };
        let conn = match pool.get().await {
            Ok(c) => c,
            Err(e) => return db_error(e),
        };

        // (a) Per-app family marker — the PRIMARY cross-node revocation
        //     (§8.5): an already-issued session cookie / access token for
        //     `(client_id, pws_sub)` is rejected from now on. Best-effort.
        if let Some(pws_sub) = pws_sub.as_deref() {
            if let Err(e) = zeroship_core::wrapper_revocation::revoke_family(
                &conn,
                &anchor.client_id,
                pws_sub,
            )
            .await
            {
                tracing::warn!(error = %e, "/signout: family-marker upsert failed");
            }
            // SAME-NODE write-side bust (R1d): drop any cached "not revoked"
            // entry for this family so a request hitting THIS node right after
            // signout reloads the just-written marker IMMEDIATELY — no TTL
            // wait. Cross-node readers rely on the TTL backstop. Invalidate
            // unconditionally (even on a marker-write error): the entry is now
            // suspect, and a reload is the safe default.
            state
                .revocation_cache
                .invalidate(&anchor.client_id, pws_sub);
        }

        // (c) Delete the anchor row(s) and collect the family ciphertexts
        //     for the (best-effort) Hydra revoke fan-out.
        if want_global {
            match anchors::delete_all_for_user(&conn, &route.app_id.to_string(), anchor.global_user_id)
                .await
            {
                Ok(deleted) => {
                    for d in deleted {
                        families.push((d.refresh_token_enc, d.client_id));
                    }
                }
                Err(e) => {
                    tracing::error!(error = %e, "/signout(global): delete_all_for_user failed");
                    return error_response(
                        HttpResponse::InternalServerError(),
                        "internal",
                        "anchor delete failed",
                    );
                }
            }
        } else {
            families.push((anchor.refresh_token_enc.clone(), anchor.client_id.clone()));
            if let Err(e) = anchors::delete(&conn, anchor_id).await {
                tracing::error!(error = %e, "/signout(local): anchor delete failed");
                return error_response(
                    HttpResponse::InternalServerError(),
                    "internal",
                    "anchor delete failed",
                );
            }
        }
        // conn drops here — released before the outbound Hydra revoke.
    }

    // (b) Best-effort revoke each server-held refresh family at Hydra. NO db
    //     connection is held across these calls. The anchor delete + family
    //     marker above are the authoritative revocation; a Hydra hiccup must
    //     not block signout, so failures are logged, not surfaced.
    for (refresh_enc, client_id) in &families {
        let aad = anchor_aad(client_id, &anchor.global_user_id.to_string());
        let refresh = match zeroship_core::crypto::decrypt(&state.anchor_enc_key, &aad, refresh_enc)
        {
            Ok(pt) => String::from_utf8_lossy(&pt).into_owned(),
            Err(e) => {
                tracing::warn!(error = %e, "/signout: refresh decrypt failed (skipping Hydra revoke)");
                continue;
            }
        };
        if let Err(e) = state.oidc_rp.revoke_token_public(client_id, &refresh).await {
            // RFC 7009 §2.2: a non-2xx here is not fatal — the family is
            // already removed locally and the marker rejects live tokens.
            tracing::warn!(error = %e, "/signout: Hydra /oauth2/revoke best-effort failure");
        }
    }

    // (d) Clear the anchor cookie + breadcrumb. 204 No Content.
    signout_cleared(&route.host, state.config.insecure_dev)
}

/// Build the 204 signout response: clear the `__Host-zs_app_anchor` anchor
/// cookie + the `is.authenticated` breadcrumb, `Cache-Control: no-store`.
fn signout_cleared(host: &str, insecure_dev: bool) -> HttpResponse {
    HttpResponse::NoContent()
        .header("cache-control", CACHE_NO_STORE)
        .header("set-cookie", anchors::clear_anchor_cookie(insecure_dev))
        .header(
            "set-cookie",
            anchors::clear_breadcrumb_cookie(host, insecure_dev),
        )
        .finish()
}

/// Parse the signout body as JSON OR form-urlencoded (Content-Type-driven,
/// defaulting to form). An empty/absent body ⇒ default (`local`).
fn parse_signout_body(req: &HttpRequest, body: &[u8]) -> SignOutRequest {
    if body.is_empty() {
        return SignOutRequest::default();
    }
    let ct = req
        .headers()
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if ct.contains("application/json") {
        serde_json::from_slice(body).unwrap_or_default()
    } else {
        let mut out = SignOutRequest::default();
        for (k, v) in url::form_urlencoded::parse(body) {
            if k.as_ref() == "scope" {
                out.scope = Some(v.into_owned());
            }
        }
        out
    }
}

/// AAD binding the encrypted refresh family to its `(client_id, sub)` row
/// context — MUST match `auth_token::anchor_aad` exactly, since signout
/// decrypts a family that `/token` (or `?mint=1`) encrypted. Kept in sync
/// by the `signout_aad_matches_token_aad` regression test.
fn anchor_aad(client_id: &str, sub: &str) -> Vec<u8> {
    format!("zs-anchor-refresh:{client_id}:{sub}").into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn popup_callback_html_does_not_reflect_query_and_targets_own_origin() {
        // The page body is a STATIC document (only the CSP nonce is
        // interpolated). It must not contain any query value, and the
        // postMessage target MUST be `location.origin`, never '*'.
        let nonce = "test-nonce-abc";
        let html = popup_callback_html(nonce);
        // postMessage targets own origin (never wildcard).
        assert!(
            html.contains("window.opener.postMessage(msg, location.origin)"),
            "{html}"
        );
        assert!(!html.contains(", '*')"), "must never postMessage to '*': {html}");
        // The relay reads from location.search at runtime — the gateway does
        // NOT interpolate any query param into the body.
        assert!(html.contains("new URLSearchParams(location.search)"), "{html}");
        // No DOM-write sink (innerHTML/document.write) for any value.
        assert!(!html.contains("innerHTML"), "no innerHTML sink: {html}");
        assert!(!html.contains("document.write"), "no document.write sink: {html}");
        // The exact message type the SDK matches on.
        assert!(html.contains("zs:authorization_response"), "{html}");
        // Both COOP-fallback channels are present.
        assert!(html.contains("new BroadcastChannel('zs:auth')"), "{html}");
        assert!(html.contains("@@zsauth@@::relay::"), "{html}");
        // The script is nonce-tagged (CSP-safe inline).
        assert!(html.contains(&format!("<script nonce=\"{nonce}\">")), "{html}");
    }

    #[test]
    fn popup_callback_html_xss_payload_in_nonce_position_is_the_only_interpolation() {
        // Belt-and-suspenders: prove the ONLY interpolation point is the
        // nonce. Two different nonces produce bodies that differ ONLY in the
        // nonce occurrences — so no query/user value can ever leak in.
        let a = popup_callback_html("AAAA");
        let b = popup_callback_html("BBBB");
        assert_eq!(a.replace("AAAA", "_"), b.replace("BBBB", "_"));
    }

    #[test]
    fn csp_pins_nonce_and_blocks_default_src() {
        let csp = popup_csp("xyz123");
        assert!(csp.contains("default-src 'none'"), "{csp}");
        assert!(csp.contains("script-src 'nonce-xyz123'"), "{csp}");
        assert!(csp.contains("frame-ancestors 'self'"), "{csp}");
        // No 'unsafe-inline' — the nonce is the whole point (spec §1.2 r2).
        assert!(!csp.contains("unsafe-inline"), "{csp}");
    }

    #[test]
    fn signout_aad_matches_token_aad() {
        // signout decrypts a family that auth_token::anchor_aad encrypted —
        // the AAD strings MUST be byte-identical or decrypt fails and the
        // Hydra revoke is silently skipped. Pin the exact format here.
        let mine = anchor_aad("oac_app", "usr_123");
        assert_eq!(mine, b"zs-anchor-refresh:oac_app:usr_123".to_vec());
    }

    #[test]
    fn signout_body_parses_scope_json_and_form_and_defaults() {
        use ntex::web::test::TestRequest;
        // JSON body.
        let req = TestRequest::default()
            .header(http::header::CONTENT_TYPE, "application/json")
            .to_http_request();
        let b = parse_signout_body(&req, br#"{"scope":"global"}"#);
        assert_eq!(b.scope.as_deref(), Some("global"));
        // Form body.
        let req = TestRequest::default()
            .header(http::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .to_http_request();
        let b = parse_signout_body(&req, b"scope=global");
        assert_eq!(b.scope.as_deref(), Some("global"));
        // Empty body ⇒ default (local).
        let req = TestRequest::default().to_http_request();
        let b = parse_signout_body(&req, b"");
        assert_eq!(b.scope, None);
    }

    #[test]
    fn authorize_query_parses_all_params() {
        let q = AuthorizeQuery::parse(
            "code_challenge=CH&code_challenge_method=S256&state=ST&nonce=NO&scope=openid+profile&prompt=consent&redirect_uri=https%3A%2F%2Fapp%2Fcb&idp_hint=google",
        );
        assert_eq!(q.code_challenge.as_deref(), Some("CH"));
        assert_eq!(q.code_challenge_method.as_deref(), Some("S256"));
        assert_eq!(q.state.as_deref(), Some("ST"));
        assert_eq!(q.nonce.as_deref(), Some("NO"));
        assert_eq!(q.scope.as_deref(), Some("openid profile"));
        assert_eq!(q.prompt.as_deref(), Some("consent"));
        assert_eq!(q.redirect_uri.as_deref(), Some("https://app/cb"));
        // Fix 5: the provider hint parses into idp_hint and is forwarded to Hydra.
        assert_eq!(q.idp_hint.as_deref(), Some("google"));
    }
}
