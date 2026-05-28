//! `/consent` GET plus consent decision POST handlers.
//!
//! Two paths share this file:
//!
//! - **First-party fast path**: `client.skip_consent` clients accept silently
//!   only for first use or scopes already recorded in `control.oauth_grants`.
//! - **Third-party UI**: render Allow/Deny forms with human-readable Phase 10
//!   scope labels and CSRF protection, then PUT the decision to hydra-admin.

use askama::Template;
use ntex::http::header::{HeaderValue, COOKIE, LOCATION, SET_COOKIE};
use ntex::web::{HttpRequest, HttpResponse};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use uuid::Uuid;
use zeroship_authz::{self as authz, AuthzContext, Resource, Scope};

use crate::config::AuthConfig;
use crate::csrf;
use crate::hydra_client::HydraAdmin;
use crate::hydra_client::types::{
    AcceptConsentRequest, ConsentRequest, ConsentSession, RejectRequest,
};
use crate::store::users;
use crate::ui::{ConsentPage, ConsentScopeView, PublicErrorMessage};

/// Hydra's "remember this consent" window when the user ticks the checkbox.
/// Matches the proposal §10.3 spec (30 days).
const REMEMBER_FOR_SECS: i64 = 60 * 60 * 24 * 30;
const CANNOT_GRANT: &str = "you cannot grant this permission";

#[derive(Debug, Deserialize)]
pub struct ConsentQuery {
    pub consent_challenge: String,
}

// ntex's per-thread service futures are intentionally `!Send`.
#[allow(clippy::future_not_send)]
pub async fn get_consent(
    query: ntex::web::types::Query<ConsentQuery>,
    admin: ntex::web::types::State<HydraAdmin>,
    cfg: ntex::web::types::State<Arc<AuthConfig>>,
    db: ntex::web::types::State<Arc<compio_postgres::Client>>,
) -> HttpResponse {
    let challenge = &query.consent_challenge;

    let info = match admin.get_consent(challenge).await {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!(error = %e, challenge = %challenge, "consent challenge fetch failed");
            return render_error(PublicErrorMessage::InvalidRequest);
        }
    };

    let subject = match consent_subject_uuid(&info) {
        Ok(subject) => subject,
        Err(e) => {
            tracing::error!(error = %e, challenge = %challenge, "consent subject parse failed");
            return render_error(PublicErrorMessage::ContactSupport);
        }
    };
    let requested_scopes = sort_dedup_scopes(&info.requested_scope);
    let prior_grant = match load_oauth_grant(db.as_ref(), subject, &info.client.client_id).await {
        Ok(grant) => grant,
        Err(e) => {
            tracing::error!(error = %e, challenge = %challenge, "oauth grant lookup failed");
            return render_error(PublicErrorMessage::ContactSupport);
        }
    };

    if info.client.skip_consent {
        match prior_grant {
            None => {
                if let Err(e) = upsert_oauth_grant(
                    db.as_ref(),
                    subject,
                    &info.client.client_id,
                    &requested_scopes,
                )
                .await
                {
                    tracing::error!(error = %e, challenge = %challenge, "oauth grant insert failed");
                    return render_error(PublicErrorMessage::ContactSupport);
                }
                return silent_accept(&admin, challenge, &info, db.as_ref()).await;
            }
            Some(previously_granted)
                if scopes_are_subset(&requested_scopes, &previously_granted) =>
            {
                if let Err(e) =
                    touch_oauth_grant(db.as_ref(), subject, &info.client.client_id).await
                {
                    tracing::error!(error = %e, challenge = %challenge, "oauth grant last_used update failed");
                    return render_error(PublicErrorMessage::ContactSupport);
                }
                return silent_accept(&admin, challenge, &info, db.as_ref()).await;
            }
            Some(_) => {}
        }
    }

    let can_grant = match grantor_can_grant_requested_scopes(db.as_ref(), &info).await {
        Ok(can_grant) => can_grant,
        Err(e) => {
            tracing::error!(error = %e, challenge = %challenge, "consent grant authorization failed");
            return render_error(PublicErrorMessage::ContactSupport);
        }
    };

    render_consent_page(challenge, &info, db.as_ref(), &cfg, can_grant).await
}

// ─── POST /consent/accept and /consent/deny ──────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ConsentDecisionForm {
    pub csrf: Option<String>,
    pub consent_challenge: String,
    /// HTML form checkbox: `Some("on")` when ticked, `None` when not.
    #[serde(default)]
    pub remember: Option<String>,
}

/// `/consent/accept` POST — validates CSRF, re-fetches the hydra challenge,
/// verifies the grantor can delegate every recognized Phase 10 scope, and PUTs
/// hydra-admin `/consent/accept`.
#[allow(clippy::future_not_send)]
pub async fn post_consent_accept(
    req: HttpRequest,
    form: ntex::web::types::Form<ConsentDecisionForm>,
    admin: ntex::web::types::State<HydraAdmin>,
    cfg: ntex::web::types::State<Arc<AuthConfig>>,
    db: ntex::web::types::State<Arc<compio_postgres::Client>>,
) -> HttpResponse {
    if !csrf_valid(&req, &form, &cfg) {
        return render_error_forbidden(PublicErrorMessage::InvalidRequest);
    }

    let challenge = form.consent_challenge.as_str();
    let info = match admin.get_consent(challenge).await {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!(error = %e, challenge = %challenge, "POST /consent/accept: get_consent failed");
            return render_error(PublicErrorMessage::InvalidRequest);
        }
    };

    match grantor_can_grant_requested_scopes(db.as_ref(), &info).await {
        Ok(true) => {}
        Ok(false) => {
            return render_consent_page(challenge, &info, db.as_ref(), &cfg, false).await;
        }
        Err(e) => {
            tracing::error!(error = %e, challenge = %challenge, "POST /consent/accept authorization failed");
            return render_error(PublicErrorMessage::ContactSupport);
        }
    }

    let subject = match consent_subject_uuid(&info) {
        Ok(subject) => subject,
        Err(e) => {
            tracing::error!(error = %e, challenge = %challenge, "POST /consent/accept subject parse failed");
            return render_error(PublicErrorMessage::ContactSupport);
        }
    };
    let requested_scopes = sort_dedup_scopes(&info.requested_scope);
    if let Err(e) =
        upsert_oauth_grant(db.as_ref(), subject, &info.client.client_id, &requested_scopes).await
    {
        tracing::error!(error = %e, challenge = %challenge, "POST /consent/accept oauth grant upsert failed");
        return render_error(PublicErrorMessage::ContactSupport);
    }

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
            render_error(PublicErrorMessage::ContactSupport)
        }
    }
}

/// `/consent/deny` POST — validates CSRF, re-fetches the hydra challenge, and
/// PUTs hydra-admin `/consent/reject`.
#[allow(clippy::future_not_send)]
pub async fn post_consent_deny(
    req: HttpRequest,
    form: ntex::web::types::Form<ConsentDecisionForm>,
    admin: ntex::web::types::State<HydraAdmin>,
    cfg: ntex::web::types::State<Arc<AuthConfig>>,
) -> HttpResponse {
    if !csrf_valid(&req, &form, &cfg) {
        return render_error_forbidden(PublicErrorMessage::InvalidRequest);
    }

    let challenge = form.consent_challenge.as_str();
    if let Err(e) = admin.get_consent(challenge).await {
        tracing::warn!(error = %e, challenge = %challenge, "POST /consent/deny: get_consent failed");
        return render_error(PublicErrorMessage::InvalidRequest);
    }

    let reject = RejectRequest {
        error: "access_denied".into(),
        error_description: Some("user denied consent".into()),
        status_code: Some(403),
    };
    match admin.reject_consent(challenge, &reject).await {
        Ok(resp) => redirect(&resp.redirect_to),
        Err(e) => {
            tracing::error!(error = %e, "reject_consent failed");
            render_error(PublicErrorMessage::ContactSupport)
        }
    }
}

// ─── helpers ─────────────────────────────────────────────────────────────

fn consent_subject_uuid(info: &ConsentRequest) -> Result<Uuid, String> {
    Uuid::parse_str(&info.subject).map_err(|e| format!("consent subject is not a UUID: {e}"))
}

fn sort_dedup_scopes(scopes: &[String]) -> Vec<String> {
    let mut sorted = scopes.to_vec();
    sorted.sort();
    sorted.dedup();
    sorted
}

fn scopes_are_subset(requested: &[String], previously_granted: &[String]) -> bool {
    requested
        .iter()
        .all(|scope| previously_granted.binary_search(scope).is_ok())
}

async fn load_oauth_grant(
    db: &compio_postgres::Client,
    user_id: Uuid,
    client_id: &str,
) -> Result<Option<Vec<String>>, String> {
    let rows = db
        .query(
            "SELECT granted_scopes \
             FROM control.oauth_grants \
             WHERE user_id = $1 AND client_id = $2",
            &[&user_id, &client_id],
        )
        .await
        .map_err(|e| format!("select control.oauth_grants: {e}"))?;

    Ok(rows
        .first()
        .map(|row| sort_dedup_scopes(&row.get::<_, Vec<String>>("granted_scopes"))))
}

async fn upsert_oauth_grant(
    db: &compio_postgres::Client,
    user_id: Uuid,
    client_id: &str,
    granted_scopes: &[String],
) -> Result<(), String> {
    let granted_scopes = sort_dedup_scopes(granted_scopes);
    db.execute(
        "INSERT INTO control.oauth_grants \
             (user_id, client_id, granted_scopes, granted_at, updated_at) \
         VALUES ($1, $2, $3, NOW(), NOW()) \
         ON CONFLICT (user_id, client_id) DO UPDATE \
         SET granted_scopes = EXCLUDED.granted_scopes, \
             updated_at = NOW()",
        &[&user_id, &client_id, &granted_scopes],
    )
    .await
    .map_err(|e| format!("upsert control.oauth_grants: {e}"))?;
    Ok(())
}

async fn touch_oauth_grant(
    db: &compio_postgres::Client,
    user_id: Uuid,
    client_id: &str,
) -> Result<(), String> {
    db.execute(
        "UPDATE control.oauth_grants SET last_used_at = NOW() \
         WHERE user_id = $1 AND client_id = $2",
        &[&user_id, &client_id],
    )
    .await
    .map_err(|e| format!("touch control.oauth_grants: {e}"))?;
    Ok(())
}

/// Silently accept consent after the caller has already decided the request is
/// eligible for the first-party fast path.
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
            render_error(PublicErrorMessage::ContactSupport)
        }
    }
}

#[allow(clippy::future_not_send)]
async fn render_consent_page(
    challenge: &str,
    info: &ConsentRequest,
    db: &compio_postgres::Client,
    cfg: &AuthConfig,
    can_grant: bool,
) -> HttpResponse {
    let csrf_token = csrf::generate_token();
    let client = load_client_display(db, info).await;
    let page = ConsentPage {
        challenge,
        csrf: &csrf_token,
        client_id: &info.client.client_id,
        client_name: &client.name,
        client_logo_uri: client.logo_uri.as_deref(),
        scopes: scope_views(&info.requested_scope),
        can_grant,
        grant_error: (!can_grant).then_some(CANNOT_GRANT),
    };
    let body = match page.render() {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(error = %e, "render consent.html failed");
            return render_error(PublicErrorMessage::ContactSupport);
        }
    };

    let mut resp = HttpResponse::Ok();
    resp.content_type("text/html; charset=utf-8");
    resp.header(SET_COOKIE, csrf::set_cookie(&csrf_token, cfg.insecure_dev));
    resp.body(body)
}

async fn grantor_can_grant_requested_scopes(
    db: &compio_postgres::Client,
    info: &ConsentRequest,
) -> Result<bool, String> {
    let scopes = info
        .requested_scope
        .iter()
        .filter_map(|raw| Scope::parse(raw).ok())
        .collect::<Vec<_>>();
    if scopes.is_empty() {
        return Ok(true);
    }

    let principal_id = Uuid::parse_str(&info.subject)
        .map_err(|e| format!("consent subject is not a UUID: {e}"))?;
    let policies = authz::load_platform_policies()
        .map_err(|e| format!("load platform policies: {e}"))?;

    for scope in scopes {
        let ctx = AuthzContext {
            principal_id,
            token_id: None,
            token_policy: None,
            action: scope.action(),
            resource: Resource::Any,
            request_ip: None,
            mfa_verified: false,
            mfa_age_seconds: None,
            request_id: None,
        };
        match authz::is_authorized_anywhere(db, &policies, &ctx).await {
            Ok(true) => {}
            Ok(false) => return Ok(false),
            Err(e) => return Err(format!("authorize {}: {e}", scope.as_str())),
        }
    }

    Ok(true)
}

struct ClientDisplay {
    name: String,
    logo_uri: Option<String>,
}

async fn load_client_display(
    db: &compio_postgres::Client,
    info: &ConsentRequest,
) -> ClientDisplay {
    let fallback_name = info
        .client
        .client_name
        .as_deref()
        .unwrap_or(&info.client.client_id);
    let mut display = ClientDisplay {
        name: fallback_name.to_owned(),
        logo_uri: None,
    };

    let rows = match db
        .query(
            "SELECT client_name, logo_uri \
             FROM control.oauth_clients \
             WHERE client_id = $1",
            &[&info.client.client_id],
        )
        .await
    {
        Ok(rows) => rows,
        Err(err) if missing_relation_or_column(&err) => return display,
        Err(err) => {
            tracing::warn!(error = %err, client_id = %info.client.client_id, "consent client metadata lookup failed");
            return display;
        }
    };

    if let Some(row) = rows.first() {
        if let Ok(Some(name)) = row.try_get::<_, Option<String>>("client_name") {
            display.name = name;
        }
        if let Ok(logo_uri) = row.try_get::<_, Option<String>>("logo_uri") {
            display.logo_uri = logo_uri;
        }
    }

    display
}

fn missing_relation_or_column(err: &compio_postgres::Error) -> bool {
    let text = err.to_string();
    text.contains("does not exist")
        || text.contains("undefined_column")
        || text.contains("42P01")
        || text.contains("42703")
}

fn scope_views(scopes: &[String]) -> Vec<ConsentScopeView> {
    scopes
        .iter()
        .map(|scope| {
            if let Ok(parsed) = Scope::parse(scope) {
                ConsentScopeView {
                    label: parsed.human_label().to_owned(),
                    unrecognized: false,
                }
            } else if let Some(label) = standard_scope_label(scope) {
                ConsentScopeView {
                    label: label.to_owned(),
                    unrecognized: false,
                }
            } else {
                ConsentScopeView {
                    label: scope.clone(),
                    unrecognized: true,
                }
            }
        })
        .collect()
}

fn standard_scope_label(scope: &str) -> Option<&'static str> {
    Some(match scope {
        "openid" => "Verify your identity",
        "email" => "See your email address",
        "profile" => "See your name and profile picture",
        "offline_access" => "Stay signed in to this app even when you're not using it",
        _ => return None,
    })
}

fn csrf_valid(
    req: &HttpRequest,
    form: &ConsentDecisionForm,
    cfg: &AuthConfig,
) -> bool {
    let cookie_header = req
        .headers()
        .get(COOKIE)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let cookie_token = csrf::parse_cookie(cookie_header, cfg.insecure_dev);
    let Some(form_token) = form.csrf.as_deref() else {
        return false;
    };
    cookie_token
        .as_deref()
        .is_some_and(|cookie| csrf::matches(form_token, cookie))
}

/// Build the `id_token` claims object based on which scopes the user granted.
/// Claims appear only when their corresponding OIDC scope is in
/// `granted_scope`; hydra supplies `sub` itself.
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

fn redirect(to: &str) -> HttpResponse {
    let mut r = HttpResponse::Found();
    r.header(
        LOCATION,
        HeaderValue::from_str(to).unwrap_or_else(|_| HeaderValue::from_static("/")),
    );
    r.finish()
}

fn render_error(message: PublicErrorMessage) -> HttpResponse {
    use crate::ui::ErrorPage;
    let page = ErrorPage {
        message,
        error_code: message.error_code(),
    };
    let body = page
        .render()
        .unwrap_or_else(|_| format!("<h1>{}</h1>", message.as_str()));
    let mut response = HttpResponse::Ok();
    response.content_type("text/html; charset=utf-8");
    response.body(body)
}

fn render_error_forbidden(message: PublicErrorMessage) -> HttpResponse {
    use crate::ui::ErrorPage;
    let page = ErrorPage {
        message,
        error_code: message.error_code(),
    };
    let body = page
        .render()
        .unwrap_or_else(|_| format!("<h1>{}</h1>", message.as_str()));
    let mut response = HttpResponse::Forbidden();
    response.content_type("text/html; charset=utf-8");
    response.body(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_scope_labels() {
        let scopes = scope_views(&[
            "openid".to_owned(),
            "apps:deploy".to_owned(),
            "custom-scope".to_owned(),
        ]);

        assert_eq!(scopes[0].label, "Verify your identity");
        assert!(!scopes[0].unrecognized);
        assert_eq!(scopes[1].label, "Deploy code to your apps");
        assert!(!scopes[1].unrecognized);
        assert_eq!(scopes[2].label, "custom-scope");
        assert!(scopes[2].unrecognized);
    }
}
