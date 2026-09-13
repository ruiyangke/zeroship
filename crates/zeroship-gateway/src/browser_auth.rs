//! Browser-facing auth HTTP surface.
//!
//! Three same-origin gateway endpoints the `@zeroship/auth` SDK drives:
//!
//! - **`GET /__zeroship/auth/authorize`** — the ONE cross-site hop. Resolves the
//!   app's per-app brokered PKCE client from `Host`, then 302s to the OP's
//!   `/authorize` carrying the BROWSER-supplied PKCE `code_challenge`
//!   (S256), `state`, `nonce`, requested `scope`, and an optional `prompt`
//!   passthrough, with `redirect_uri = {scheme}://{host}/__zeroship/auth/popup-callback`
//!   (a registered per-app redirect URI). The gateway holds NO PKCE verifier — the
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
//!   `localStorage` relay cover the COOP-severed-opener case. Strict
//!   CSP (`default-src 'none'; script-src 'nonce-…'; frame-ancestors 'self'`),
//!   `Referrer-Policy: no-referrer`, `COOP: same-origin`.
//!
//! - **`POST /__zeroship/auth/signout`** — Enforces a same-origin guard
//!   (X-ZS-Auth + exact Origin). Reads the `__Host-zeroship_app_anchor`
//!   anchor cookie → loads the anchor → (a) sets the per-app family marker
//!   via `revoke_family(client_id, pws_sub)`, (b) best-effort revokes the
//!   server-held refresh family at the OP `/revoke`, (c) deletes the
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

/// Strict CSP for the popup-callback page. `default-src 'none'`
/// blocks all loads; `script-src 'nonce-<random>'` permits ONLY the inline
/// nonce-tagged relay script (a reflected/injected `<script>` is blocked
/// even if a future edit reflected a query param); `frame-ancestors 'self'`
/// restricts who may embed the page.
fn popup_csp(nonce: &str) -> String {
    format!("default-src 'none'; script-src 'nonce-{nonce}'; frame-ancestors 'self'")
}

// ─── GET /__zeroship/auth/authorize ────────────────────────────────────────────

/// `GET /__zeroship/auth/authorize` — build the OP `/authorize` URL for the
/// per-app brokered PKCE client and 302. See module docs.
#[allow(clippy::future_not_send)]
pub async fn authorize(req: HttpRequest, state: State<Arc<GateState>>) -> HttpResponse {
    // Resolve the app + per-app oauth_client_id. 503 client_not_provisioned
    // when the route has no client yet.
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
    let requested_scope = q.scope.as_deref().filter(|s| !s.is_empty()).unwrap_or("openid");
    let scope = scope_with_offline_access(requested_scope);

    // `prompt` is a PASSTHROUGH (spec §1.2): omitted in the common case so
    // the OP's SSO skip fires; `login`/`consent` for explicit step-up. The
    // silent-iframe `prompt=none` path was removed, but the gateway does NOT
    // reject `none` here — it is simply forwarded (the OP decides). We only
    // forward a non-empty prompt.
    let prompt = q.prompt.as_deref().filter(|s| !s.is_empty());

    // `idp_hint` is the SDK's `SignInOptions.provider` (google/github/password)
    // threaded through verbatim to the OP so the login UI can route to / pre-
    // select the named upstream IdP. PASSTHROUGH only (OP/login-UI decides
    // what a value means); omitted ⇒ default provider picker.
    let idp_hint = q.idp_hint.as_deref().filter(|s| !s.is_empty());

    // redirect_uri defaults to THIS app's own popup-callback (a registered
    // URI from 1d). When the SDK supplies one, it MUST EXACTLY match one of
    // this app's registered callback URIs — a foreign OR same-origin-but-
    // unregistered redirect_uri is rejected (open-redirect guard, RFC 6749
    // §3.1.2.3 / RFC 9700 §4.1.3). The ultimate allowlist is the OP's
    // registered redirect_uris; we mirror that exact-match set up front so a
    // misconfigured SDK fails fast AND so a gateway-layer deviation can never
    // widen the OP's allowlist if registration ever drifts.
    let scheme = state.config.origin_scheme.as_str();
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
            scope: scope.as_str(),
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
/// extra query/fragment) is rejected, mirroring the OP's registered allowlist so
/// the gateway can never widen it.
fn is_registered_redirect_uri(scheme: &str, host: &str, supplied: &str) -> bool {
    REGISTERED_CALLBACK_PATHS
        .iter()
        .any(|path| supplied == format!("{scheme}://{host}{path}"))
}

fn scope_with_offline_access(scope: &str) -> String {
    let trimmed = scope.trim();
    let mut out = if trimmed.is_empty() { "openid".to_string() } else { trimmed.to_string() };
    if !out.split_whitespace().any(|s| s == "offline_access") {
        out.push_str(" offline_access");
    }
    out
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
/// runtime and `postMessage`s the PARSED values. A present RFC 9207 `iss`
/// mismatch is rejected before the relay page is emitted; absence is tolerated
/// for mixed-version OP/dev flows.
#[allow(clippy::future_not_send)]
pub async fn popup_callback(req: HttpRequest, state: State<Arc<GateState>>) -> HttpResponse {
    for (k, v) in url::form_urlencoded::parse(req.query_string().as_bytes()) {
        if k == "iss" && !v.is_empty() && v.as_ref() != state.oidc_rp.issuer {
            return error_response(
                HttpResponse::BadRequest(),
                "invalid_request",
                "issuer mismatch",
            );
        }
    }

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
  var iss = p.get('iss');\n\
  var msg = {{ type: 'zs:authorization_response', response:\n\
    p.get('error')\n\
      ? {{ error: p.get('error'), error_description: p.get('error_description'), state: state, iss: iss }}\n\
      : {{ code: p.get('code'), state: state, iss: iss }} }};\n\
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
    if let Err(resp) = same_origin_guard(&req, &route.host, &state.config, true, true) {
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
        .and_then(anchors::parse_anchor_cookie);

    let Some(db_cfg) = state.db.as_ref() else {
        // No DB — nothing server-side to revoke; still clear the cookies.
        return signout_cleared(&route.host);
    };
    let Some(anchor_id) = anchor_id else {
        return signout_cleared(&route.host);
    };

    // Load the anchor (released immediately — no conn held across the OP
    // revoke). A missing/expired anchor ⇒ just clear the cookies.
    let anchor = {
        let pool = match crate::db::checkout(db_cfg).await {
            Ok(p) => p,
            Err(e) => return db_error(e),
        };
        let mut conn = match pool.acquire().await {
            Ok(c) => c,
            Err(e) => return db_error(e),
        };
        // RLS-scoped to `route.app_id` (changeset 0025): a cookie replayed
        // against the wrong app resolves to `None` here, so this READ both
        // loads the anchor AND enforces the former post-hoc
        // `anchor.app_id == route.app_id` check.
        match anchors::read_live(&mut conn, &route.app_id, anchor_id).await {
            Ok(Some(a)) => a,
            Ok(None) => return signout_cleared(&route.host),
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
    // their `sub` (the cookie / Bearer arms check the SAME
    // (client_id, sub), §8.5). When the sector is missing we cannot derive the
    // pws_; the family marker is then best-effort skipped (the anchor delete +
    // cookie clear still happen).
    let pws_sub = route
        .sector_identifier
        .as_deref()
        .map(|sector| {
            zeroship_core::auth::derive_pairwise(
                &state.pairwise_salt,
                &anchor.global_user_id,
                sector,
            )
        });

    // The encrypted families to revoke at OP + the anchor rows to delete.
    // For `local`: just this anchor's family + this row. For `global`: every
    // anchor row for `(app_id, global_user_id)` (this app, every device).
    let mut families: Vec<(Vec<u8>, String)> = Vec::new();
    {
        let pool = match crate::db::checkout(db_cfg).await {
            Ok(p) => p,
            Err(e) => return db_error(e),
        };
        let mut conn = match pool.acquire().await {
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
            if let Err(e) = zeroship_authz::wrapper_revocation::revoke_family(
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
        //     for the (best-effort) OP revoke fan-out.
        if want_global {
            match anchors::delete_all_for_user(&mut conn, &route.app_id, &anchor.global_user_id)
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
            if let Err(e) = anchors::delete(&mut conn, &route.app_id, anchor_id).await {
                tracing::error!(error = %e, "/signout(local): anchor delete failed");
                return error_response(
                    HttpResponse::InternalServerError(),
                    "internal",
                    "anchor delete failed",
                );
            }
        }
        // conn drops here — released before the outbound OP revoke.
    }

    // (b) Best-effort revoke each server-held refresh family at the OP. NO db
    //     connection is held across these calls. The anchor delete + family
    //     marker above are the authoritative revocation; an OP hiccup must
    //     not block signout, so failures are logged, not surfaced.
    for (refresh_enc, client_id) in &families {
        let aad = anchor_aad(client_id, anchor.global_user_id.as_str());
        let refresh = match zeroship_core::crypto::decrypt(&state.anchor_enc_key, &aad, refresh_enc)
        {
            Ok(pt) => String::from_utf8_lossy(&pt).into_owned(),
            Err(e) => {
                tracing::warn!(error = %e, "/signout: refresh decrypt failed (skipping OP revoke)");
                continue;
            }
        };
        if let Err(e) = state.oidc_rp.revoke_token_public(client_id, &refresh).await {
            // RFC 7009 §2.2: a non-2xx here is not fatal — the family is
            // already removed locally and the marker rejects live tokens.
            tracing::warn!(error = %e, "/signout: OP /revoke best-effort failure");
        }
    }

    // (d) Clear the anchor cookie + breadcrumb. 204 No Content.
    signout_cleared(&route.host)
}

/// Build the 204 signout response: clear the `__Host-zeroship_app_anchor` anchor
/// cookie + the `is.authenticated` breadcrumb, `Cache-Control: no-store`.
fn signout_cleared(host: &str) -> HttpResponse {
    HttpResponse::NoContent()
        .header("cache-control", CACHE_NO_STORE)
        .header("set-cookie", anchors::clear_anchor_cookie())
        .header("set-cookie", anchors::clear_breadcrumb_cookie(host))
        // Clear the live session credential together with the recovery cookies.
        .header("set-cookie", crate::oidc_rp::clear_app_session_cookie())
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
/// context. Signout must decrypt the family produced by session issuance.
/// Source-owned signout tests exercise that producer-to-consumer round trip
/// and observe the refresh token delivered to the provider.
fn anchor_aad(client_id: &str, sub: &str) -> Vec<u8> {
    format!("zs-anchor-refresh:{client_id}:{sub}").into_bytes()
}

#[cfg(test)]
mod tests;
