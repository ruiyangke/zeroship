//! Browser-facing auth HTTP surface (auth-sdk Slice 1b-browser, spec §1.2).
//!
//! Three same-origin gateway endpoints the `@zeroship/auth` SDK drives:
//!
//! - **`GET /__zeroship/auth/authorize`** — the ONE cross-site hop. Resolves the
//!   app's per-app PUBLIC PKCE client from `Host`, then 302s to Hydra's
//!   `/oauth2/auth` carrying the BROWSER-supplied PKCE `code_challenge`
//!   (S256), `state`, `nonce`, requested `scope`, and an optional `prompt`
//!   passthrough, with `redirect_uri = {scheme}://{host}/__zeroship/auth/popup-callback`
//!   (a registered URI from 1d). The gateway holds NO PKCE verifier — the
//!   browser does (Supabase-style). 503 `client_not_provisioned` when the
//!   route has no `oauth_client_id` yet.
//!
//! - **`GET /__zeroship/auth/popup-callback`** — a tiny SAME-ORIGIN HTML relay
//!   page. Inline (CSP-nonce-tagged) JS reads `code`+`state` (or
//!   `error`+`error_description`+`state`) FROM `location.search` (never
//!   reflected into the DOM by the gateway — no XSS sink) and
//!   `postMessage`s `{type:'zs:authorization_response', response:{…}}` to the
//!   launcher — `window.opener` (the popup leg, federated google/github) OR
//!   `window.parent` (the immersive iframe leg, our first-party password UI) —
//!   with `targetOrigin = location.origin` (its OWN origin, never `'*'`); see
//!   the dual-target block on [`popup_callback_html`]. A same-origin
//!   `BroadcastChannel` + one-shot
//!   `localStorage` relay cover the COOP-severed-opener case (§4.4). Strict
//!   CSP (`default-src 'none'; script-src 'nonce-…'; frame-ancestors 'self'`),
//!   `Referrer-Policy: no-referrer`, `COOP: same-origin`.
//!
//! - **`POST /__zeroship/auth/signout`** — FIXES the live "no handler" bug. Same-
//!   origin guard (X-ZS-Auth + exact Origin). Reads the `__Host-zeroship_app_anchor`
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

// ─── GET /__zeroship/auth/authorize ────────────────────────────────────────────

/// `GET /__zeroship/auth/authorize` — build the Hydra `/oauth2/auth` URL for the
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
    // URI from 1d). When the SDK supplies one, it MUST EXACTLY match one of
    // this app's registered callback URIs — a foreign OR same-origin-but-
    // unregistered redirect_uri is rejected (open-redirect guard, RFC 6749
    // §3.1.2.3 / RFC 9700 §4.1.3). The ultimate allowlist is Hydra's
    // registered redirect_uris; we mirror that exact-match set up front so a
    // misconfigured SDK fails fast AND so a gateway-layer deviation can never
    // widen Hydra's allowlist if Hydra registration ever drifts.
    let scheme = if state.config.insecure_dev { "http" } else { "https" };
    let redirect_uri = match q.redirect_uri.as_deref().filter(|s| !s.is_empty()) {
        None => default_redirect_uri(scheme, &route.host),
        Some(supplied) => {
            if is_registered_redirect_uri(scheme, &route.host, supplied) {
                supplied.to_string()
            } else {
                return error_response(
                    HttpResponse::BadRequest(),
                    "invalid_request",
                    "redirect_uri must exactly match a registered callback URI",
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

/// The same-origin OAuth callback paths registered per host by the control
/// plane (`control::app_oauth_client::CALLBACK_PATHS`). The gateway MUST keep
/// this list byte-identical to that registration set: a `redirect_uri` override
/// is accepted only when it exactly equals one of these paths on this app's own
/// origin (RFC 6749 §3.1.2.3 exact-match). The first entry is the default.
const REGISTERED_CALLBACK_PATHS: [&str; 2] =
    ["/__zeroship/auth/popup-callback", "/__zeroship/auth/callback"];

/// This app's default redirect_uri — the first registered callback path.
fn default_redirect_uri(scheme: &str, host: &str) -> String {
    format!("{scheme}://{host}{}", REGISTERED_CALLBACK_PATHS[0])
}

/// Exact-match a supplied `redirect_uri` against this app's registered callback
/// set. NOT a prefix/origin check: a same-origin-but-unregistered path (or any
/// extra query/fragment) is rejected, mirroring Hydra's registered allowlist so
/// the gateway can never widen it.
fn is_registered_redirect_uri(scheme: &str, host: &str, supplied: &str) -> bool {
    REGISTERED_CALLBACK_PATHS
        .iter()
        .any(|path| supplied == format!("{scheme}://{host}{path}"))
}

/// `GET /__zeroship/auth/authorize` query params (all browser-supplied).
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

// ─── GET /__zeroship/auth/popup-callback ───────────────────────────────────────

/// `GET /__zeroship/auth/popup-callback` — the same-origin HTML relay page. See
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
/// relays them via THREE same-origin channels (postMessage to the launcher,
/// BroadcastChannel, one-shot localStorage), targetOrigin = own origin. No
/// DOM writes of any query value (the reflected `error_description` is never
/// an XSS sink). The ONLY value the gateway interpolates is the CSP `nonce`,
/// which is a server-generated base64url token (not attacker-controlled).
///
/// **Dual-target postMessage (immersive iframe login, design §4.2).** The same
/// relay backs BOTH launchers: the popup WINDOW (federated `google`/`github`),
/// where the launcher is `window.opener`; and the immersive `<iframe>` (our
/// first-party password UI on the same-site console), where the launcher is
/// `window.parent`. We post to EXACTLY ONE window — `window.opener` if
/// present-and-distinct (popup leg), else `window.parent` if present-and-distinct
/// (iframe leg), else NONE (full-page redirect: `parent === self`, opener null →
/// the BroadcastChannel + localStorage fallbacks carry the code). `targetOrigin`
/// stays pinned to `location.origin` (the app/console origin) — NEVER `'*'` — so
/// even a wrong-context window of a foreign origin silently drops the message.
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
  // Primary: postMessage to the launcher — opener (popup) || parent (iframe).\n\
  // Same-origin target pinned to location.origin; NEVER '*'.\n\
  var tgt = (window.opener && window.opener !== window) ? window.opener\n\
          : (window.parent  && window.parent  !== window) ? window.parent : null;\n\
  try {{ if (tgt) tgt.postMessage(msg, location.origin); }} catch (e) {{}}\n\
  // Fallback (opener/parent severed by COOP across app->auth->app, or the\n\
  // full-page redirect leg, §4.4): both channels are SAME-ORIGIN, so no\n\
  // cross-origin exposure.\n\
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

// ─── POST /__zeroship/auth/signout ─────────────────────────────────────────────

/// Body the SDK posts to `/__zeroship/auth/signout`.
#[derive(serde::Deserialize, Default)]
struct SignOutRequest {
    /// `"local"` (default — this device) or `"global"` (this app, every
    /// device). Anything else is treated as `local`.
    #[serde(default)]
    scope: Option<String>,
}

/// `POST /__zeroship/auth/signout` — FIX the live missing-handler bug. See module
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
        let mut conn = match pool.get().await {
            Ok(c) => c,
            Err(e) => return db_error(e),
        };
        // RLS-scoped to `route.app_id` (changeset 0025): a cookie replayed
        // against the wrong app resolves to `None` here, so this READ both
        // loads the anchor AND enforces the former post-hoc
        // `anchor.app_id == route.app_id` check.
        match anchors::read_live(&mut conn, route.app_id, anchor_id).await {
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
        let mut conn = match pool.get().await {
            Ok(c) => c,
            Err(e) => return db_error(e),
        };

        // (a) Per-app family marker — the PRIMARY cross-node revocation
        //     (§8.5): an already-issued session cookie / access token for
        //     `(client_id, pws_sub)` is rejected from now on. Best-effort.
        //     `token_revocations` is NOT an RLS table, so this runs directly on
        //     `&conn` (no tenant GUC) — sequentially before the RLS-scoped
        //     anchor delete below, so the two `&conn`/`&mut conn` borrows never
        //     overlap.
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
            match anchors::delete_all_for_user(&mut conn, route.app_id, anchor.global_user_id)
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
            if let Err(e) = anchors::delete(&mut conn, route.app_id, anchor_id).await {
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

/// Build the 204 signout response: clear the `__Host-zeroship_app_anchor` anchor
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
        // Dual-target (immersive iframe login, §4.2): the launcher is resolved
        // as `window.opener` (popup) || `window.parent` (iframe), then posted to
        // with `targetOrigin = location.origin` — NEVER '*'.
        assert!(
            html.contains("(window.opener && window.opener !== window) ? window.opener")
                && html.contains("(window.parent  && window.parent  !== window) ? window.parent"),
            "callback must resolve the launcher as opener||parent: {html}"
        );
        assert!(
            html.contains("tgt.postMessage(msg, location.origin)"),
            "callback must post to the resolved launcher at location.origin: {html}"
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
    fn redirect_uri_override_is_exact_match_not_prefix_match() {
        // L2 regression: a `redirect_uri` override must EXACTLY match one of
        // this app's registered callback URIs (RFC 6749 §3.1.2.3), not merely
        // be prefixed by the app's own origin. The prior `starts_with(origin)`
        // check accepted any same-origin path; this exercises the cases it let
        // through.
        let scheme = "https";
        let host = "app.zeroship.ai";

        // (1) The two registered callbacks are accepted (legitimate behavior
        //     preserved — popup-callback + callback, matching the control
        //     plane's CALLBACK_PATHS registration set).
        assert!(is_registered_redirect_uri(
            scheme,
            host,
            "https://app.zeroship.ai/__zeroship/auth/popup-callback"
        ));
        assert!(is_registered_redirect_uri(
            scheme,
            host,
            "https://app.zeroship.ai/__zeroship/auth/callback"
        ));
        assert_eq!(
            default_redirect_uri(scheme, host),
            "https://app.zeroship.ai/__zeroship/auth/popup-callback"
        );

        // (2) THE BUG: a same-origin-but-UNREGISTERED path. This passes the old
        //     prefix check (`starts_with("https://app.zeroship.ai/")`) yet is
        //     NOT a registered callback — it must be rejected.
        assert!(
            "https://app.zeroship.ai/evil"
                .starts_with("https://app.zeroship.ai/"),
            "precondition: the malicious URI DID pass the old prefix check",
        );
        assert!(
            !is_registered_redirect_uri(scheme, host, "https://app.zeroship.ai/evil"),
            "same-origin-but-unregistered redirect_uri must be rejected (exact-match)",
        );

        // (3) A registered path with an extra query string / fragment is a
        //     distinct URI under exact-match — rejected.
        assert!(!is_registered_redirect_uri(
            scheme,
            host,
            "https://app.zeroship.ai/__zeroship/auth/popup-callback?next=//evil.com"
        ));
        assert!(!is_registered_redirect_uri(
            scheme,
            host,
            "https://app.zeroship.ai/__zeroship/auth/popup-callback/../evil"
        ));

        // (4) A foreign origin (and a sibling-domain prefix trick) is rejected.
        assert!(!is_registered_redirect_uri(
            scheme,
            host,
            "https://evil.com/__zeroship/auth/popup-callback"
        ));
        assert!(!is_registered_redirect_uri(
            scheme,
            host,
            "https://app.zeroship.ai.evil.com/__zeroship/auth/popup-callback"
        ));
    }

    #[test]
    fn registered_callback_paths_match_control_plane_registration() {
        // The gateway's accept-set MUST stay byte-identical to the control
        // plane's CALLBACK_PATHS (control::app_oauth_client) that registers the
        // URIs with Hydra. If these drift, the gateway would either reject a
        // legitimately-registered callback or (worse) accept one Hydra never
        // registered. Pin the exact strings.
        assert_eq!(
            REGISTERED_CALLBACK_PATHS,
            ["/__zeroship/auth/popup-callback", "/__zeroship/auth/callback"],
        );
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
