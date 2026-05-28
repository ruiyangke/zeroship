//! `/consent` GET + POST handlers.
//!
//! Two paths share this file:
//!
//! - **First-party fast path** (P2-U5): if the challenge's `client.skip_consent`
//!   is true, accept silently with the verbatim requested scopes/audience.
//! - **Third-party UI** (P4-U5): render an Allow/Deny form, then on POST
//!   either `accept_consent` (with optional remember + ID-token claims
//!   gated on granted scope) or `reject_consent` with `access_denied`.
//!
//! The form follows the same double-submit CSRF pattern as `/login`:
//! a freshly-minted `__Host-zsidp_csrf` cookie set on GET, and the form
//! field is constant-time-compared against it on POST.
//!
//! `prompt=` handling (`oidc_context.prompt`) is a noted TODO — see the
//! comment at the top of `get` — but the happy path doesn't need it.

use askama::Template;
use ntex::http::header::{HeaderValue, COOKIE, LOCATION, SET_COOKIE};
use ntex::web::{HttpRequest, HttpResponse};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

use crate::config::AuthConfig;
use crate::csrf;
use crate::hydra_client::types::{
    AcceptConsentRequest, ConsentRequest, ConsentSession, RejectRequest,
};
use crate::hydra_client::HydraAdmin;
use crate::store::users;
use crate::ui::ConsentPage;

/// Hydra's "remember this consent" window when the user ticks the checkbox.
/// Matches the proposal §10.3 spec (30 days).
const REMEMBER_FOR_SECS: i64 = 60 * 60 * 24 * 30;

#[derive(Debug, Deserialize)]
pub struct ConsentQuery {
    pub consent_challenge: String,
}

// ntex's per-thread service futures are intentionally `!Send`.
#[allow(clippy::future_not_send)]
pub async fn get(
    req: HttpRequest,
    query: ntex::web::types::Query<ConsentQuery>,
    admin: ntex::web::types::State<HydraAdmin>,
    cfg: ntex::web::types::State<Arc<AuthConfig>>,
    db: ntex::web::types::State<Arc<compio_postgres::Client>>,
) -> HttpResponse {
    let _ = req; // request-header extraction (UA/request-id) lands later.
    let challenge = &query.consent_challenge;

    let info = match admin.get_consent(challenge).await {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!(error = %e, challenge = %challenge, "consent challenge fetch failed");
            return render_error(&e.to_string());
        }
    };

    // Fast path 1 — first-party clients always skip the UI.
    //
    // Fast path 2 — hydra has already remembered this user's consent for
    // this client+scopes (`info.skip = true`).
    //
    // `prompt=consent` from the RP would normally force us to ignore
    // `info.skip` and re-prompt; that's a TODO and the happy path doesn't
    // need it. `prompt=none` similarly should reject with
    // `interaction_required` rather than render UI — also TODO. See
    // proposal §10.3.
    if info.client.skip_consent || info.skip {
        return silent_accept(&admin, challenge, &info, db.as_ref()).await;
    }

    // Third-party path — render the form.
    let csrf_token = csrf::generate_token();
    let scopes: Vec<&str> = info
        .requested_scope
        .iter()
        .map(|s| translate_scope(s))
        .collect();
    let client_name = info
        .client
        .client_name
        .as_deref()
        .unwrap_or(&info.client.client_id);

    let page = ConsentPage {
        challenge,
        csrf: &csrf_token,
        client_id: &info.client.client_id,
        client_name,
        scopes,
        error: None,
    };
    let body = match page.render() {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(error = %e, "render consent.html failed");
            return render_error("internal error");
        }
    };

    let mut resp = HttpResponse::Ok();
    resp.content_type("text/html; charset=utf-8");
    resp.header(SET_COOKIE, csrf::set_cookie(&csrf_token, cfg.insecure_dev));
    resp.body(body)
}

// ─── POST /consent ───────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ConsentForm {
    pub csrf: String,
    pub consent_challenge: String,
    pub decision: String,
    /// HTML form checkbox: `Some("on")` when ticked, `None` when not.
    #[serde(default)]
    pub remember: Option<String>,
}

/// `/consent` POST — Allow/Deny + remember.
///
/// 1. CSRF (form vs `__Host-zsidp_csrf` cookie).
/// 2. Re-fetch the consent challenge (don't trust the form's challenge alone).
/// 3. `decision=deny` → `reject_consent(error=access_denied)` → 302.
/// 4. `decision=allow` → build `AcceptConsentRequest` with:
///    - `grant_scope` = `info.requested_scope` (passed through verbatim;
///      hydra refuses scopes the client isn't allowed to request, so this
///      is safe by construction).
///    - `session.id_token` = user claims filtered by granted scope
///      (`email` and `profile` only included when the corresponding scope
///      was granted).
///    - `remember` honours the form checkbox; `remember_for=30d` when set.
#[allow(clippy::future_not_send, clippy::too_many_lines)]
pub async fn post(
    req: HttpRequest,
    form: ntex::web::types::Form<ConsentForm>,
    admin: ntex::web::types::State<HydraAdmin>,
    cfg: ntex::web::types::State<Arc<AuthConfig>>,
    db: ntex::web::types::State<Arc<compio_postgres::Client>>,
) -> HttpResponse {
    // 1. CSRF.
    let cookie_header = req
        .headers()
        .get(COOKIE)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let cookie_token = csrf::parse_cookie(cookie_header, cfg.insecure_dev);
    if cookie_token
        .as_deref()
        .is_none_or(|c| !csrf::matches(&form.csrf, c))
    {
        return render_error("invalid request");
    }

    // 2. Re-fetch the challenge from hydra. The form-carried challenge is
    // attacker-controlled — hydra is the source of truth for scopes, client
    // identity, and subject.
    let challenge = form.consent_challenge.as_str();
    let info = match admin.get_consent(challenge).await {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!(error = %e, challenge = %challenge, "POST /consent: get_consent failed");
            return render_error(&e.to_string());
        }
    };

    // 3. Deny path.
    if form.decision == "deny" {
        let reject = RejectRequest {
            error: "access_denied".into(),
            error_description: Some("user denied consent".into()),
            status_code: Some(403),
        };
        return match admin.reject_consent(challenge, &reject).await {
            Ok(resp) => redirect(&resp.redirect_to),
            Err(e) => {
                tracing::error!(error = %e, "reject_consent failed");
                render_error(&e.to_string())
            }
        };
    }

    // Anything other than "allow" at this point is a malformed form
    // (the template only renders Allow/Deny buttons). Treat it as a bad
    // request rather than silently accepting.
    if form.decision != "allow" {
        return render_error("invalid decision");
    }

    // 4. Allow path. Build session claims from the granted scopes.
    let id_token_claims =
        build_id_token_claims(db.as_ref(), &info.subject, &info.requested_scope).await;

    let remember = form.remember.is_some();
    let accept = AcceptConsentRequest {
        grant_scope: info.requested_scope.clone(),
        grant_access_token_audience: info.requested_access_token_audience.clone(),
        remember: Some(remember),
        remember_for: Some(if remember { REMEMBER_FOR_SECS } else { 0 }),
        session: Some(ConsentSession {
            id_token: Some(id_token_claims),
            // §13 "session.access_token leakage" — intentionally empty.
            access_token: None,
        }),
    };

    match admin.accept_consent(challenge, &accept).await {
        Ok(resp) => redirect(&resp.redirect_to),
        Err(e) => {
            tracing::error!(error = %e, "accept_consent failed");
            render_error(&e.to_string())
        }
    }
}

// ─── helpers ─────────────────────────────────────────────────────────────

/// Silently accept consent for the fast paths (`skip_consent` clients and
/// `info.skip` re-consent). Shared between the first-party shortcut and the
/// "user already consented" optimisation hydra signals via `info.skip`.
#[allow(clippy::future_not_send)]
async fn silent_accept(
    admin: &HydraAdmin,
    challenge: &str,
    info: &ConsentRequest,
    db: &compio_postgres::Client,
) -> HttpResponse {
    let id_token_claims = build_id_token_claims(db, &info.subject, &info.requested_scope).await;
    let accept = AcceptConsentRequest {
        grant_scope: info.requested_scope.clone(),
        grant_access_token_audience: info.requested_access_token_audience.clone(),
        remember: Some(true),
        remember_for: Some(3600),
        session: Some(ConsentSession {
            id_token: Some(id_token_claims),
            access_token: None,
        }),
    };

    match admin.accept_consent(challenge, &accept).await {
        Ok(resp) => redirect(&resp.redirect_to),
        Err(e) => {
            tracing::error!(error = %e, "accept_consent (silent) failed");
            render_error(&e.to_string())
        }
    }
}

/// Build the `id_token` claims object based on which scopes the user
/// granted. Claims appear only when their corresponding scope is in
/// `granted_scope`:
///
/// - `openid` always; the subject is set by hydra (`sub` in the ID token).
/// - `email` → `email` + `email_verified`
/// - `profile` → `name`, optional `picture`
///
/// If the user row can't be loaded (subject not parseable as UUID, row
/// deleted, or DB hiccup), returns an empty object — RPs can still hit
/// `/userinfo` for the rest, and hydra will populate `sub` regardless.
#[allow(clippy::future_not_send)]
async fn build_id_token_claims(
    db: &compio_postgres::Client,
    subject: &str,
    granted_scope: &[String],
) -> serde_json::Value {
    let user = match users::find_by_id(db, subject).await {
        Ok(u) => u,
        Err(e) => {
            tracing::warn!(error = %e, subject = %subject, "consent: users::find_by_id failed");
            None
        }
    };

    let mut claims = serde_json::Map::new();
    if let Some(u) = user.as_ref() {
        if granted_scope.iter().any(|s| s == "email") {
            claims.insert("email".into(), json!(u.email));
            claims.insert(
                "email_verified".into(),
                json!(u.email_verified_at.is_some()),
            );
        }
        if granted_scope.iter().any(|s| s == "profile") {
            claims.insert("name".into(), json!(u.name));
            if let Some(p) = u.avatar_url.as_ref() {
                claims.insert("picture".into(), json!(p));
            }
        }
    }
    serde_json::Value::Object(claims)
}

/// Translate an OIDC/OAuth2 scope string into a human-readable permission
/// description for the consent UI. Unknown scopes fall through to a
/// generic blurb so a misconfigured client doesn't leak as `<empty>`.
fn translate_scope(scope: &str) -> &'static str {
    match scope {
        "openid" => "Verify your identity",
        "email" => "See your email address",
        "profile" => "See your name and profile picture",
        "offline_access" => "Stay signed in to this app even when you're not using it",
        _ => "Access additional permissions",
    }
}

fn redirect(to: &str) -> HttpResponse {
    let mut r = HttpResponse::Found();
    r.header(
        LOCATION,
        HeaderValue::from_str(to).unwrap_or_else(|_| HeaderValue::from_static("/")),
    );
    r.finish()
}

fn render_error(msg: &str) -> HttpResponse {
    use crate::ui::ErrorPage;
    let page = ErrorPage {
        error: "Consent failed",
        error_description: Some(msg),
    };
    let body = page.render().unwrap_or_else(|_| format!("<h1>{msg}</h1>"));
    let mut r = HttpResponse::Ok();
    r.content_type("text/html; charset=utf-8");
    r.body(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translates_standard_scopes() {
        assert_eq!(translate_scope("openid"), "Verify your identity");
        assert_eq!(translate_scope("email"), "See your email address");
        assert_eq!(
            translate_scope("profile"),
            "See your name and profile picture"
        );
        assert_eq!(
            translate_scope("offline_access"),
            "Stay signed in to this app even when you're not using it"
        );
        assert_eq!(
            translate_scope("custom-scope"),
            "Access additional permissions"
        );
    }
}
