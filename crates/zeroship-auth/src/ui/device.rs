//! `/device` GET + POST handlers for OAuth 2.0 Device Authorization Grant
//! user-code entry (RFC 8628).
//!
//! This page is the human end of the OP's own device grant: the browser types
//! the code the CLI printed, the signed-in user is bound to the pending row,
//! and the CLI redeems it at `/oauth2/token`.
//!
//! It used to serve a second audience. Control ran a parallel device flow
//! whose rows carried `provider = 'platform'`, and under
//! `AuthProviderKind::Supabase` this page rendered a GoTrue sign-in that
//! posted the resulting bearer to control's `/api/device/approve` instead of
//! writing the row itself. Control's flow is gone - `zeroship login` drives
//! the OP grant - so that page had nothing left to post to and was removed
//! with it. Supabase survives as an upstream social login
//! (`docs/decisions/2026-06-30-self-contained-auth-replace-hydra.md`, line 13);
//! what retired is its DEPLOY path, which line 32 of the same ADR supersedes.

use std::sync::Arc;

use askama::Template;
use ntex::http::header::{HeaderValue, COOKIE, LOCATION, SET_COOKIE};
use ntex::http::StatusCode;
use ntex::web::{HttpRequest, HttpResponse};
use serde::Deserialize;
use serde_json::json;

use crate::audit::{self, AuditEvent};
use crate::config::AuthConfig;
use crate::csrf;
use crate::headers;
use crate::identity::eligibility;
use crate::oidc::device_token::{self, DeviceApproval};
use zeroship_authn::rate_limit::{self, Quota, RateLimitDecision};
use crate::sessions::login as session_cookie;
use crate::store::sessions;
use crate::ui::{DevicePage, DeviceScopeView};
use zeroship_authz::Scope;

#[derive(Debug, Deserialize)]
pub struct DeviceForm {
    pub user_code: String,
    #[serde(default)]
    pub confirm: Option<String>,
    /// Double-submit CSRF token mirrored from the `__Host-zsidp_csrf` cookie.
    /// The device-confirmation POST binds an OAuth device challenge to the
    /// signed-in user — an identity-conferring state change — so it MUST carry
    /// the same CSRF token the ten sibling form handlers enforce (RFC 8628
    /// §5.4 names device confirmation as a CSRF target).
    pub csrf: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct DeviceQuery {
    #[serde(default)]
    pub user_code: Option<String>,
}

#[allow(clippy::future_not_send)]
pub async fn get(
    req: HttpRequest,
    query: ntex::web::types::Query<DeviceQuery>,
    cfg: ntex::web::types::State<Arc<AuthConfig>>,
    db: ntex::web::types::State<Arc<compio_postgres::Client>>,
) -> HttpResponse {
    let user_code = query.user_code.as_deref().unwrap_or("").trim();
    if user_code.is_empty() {
        return render_form("", None, StatusCode::OK, None);
    }
    if !device_token::valid_user_code(user_code) {
        return render_form(
            user_code,
            Some("invalid or expired code"),
            StatusCode::BAD_REQUEST,
            None,
        );
    }
    match device_token::pending_user_code_details(db.as_ref(), user_code).await {
        Ok(Some(details)) => {
            render_form(user_code, None, StatusCode::OK, Some(&details))
        }
        Ok(None) => {
            if let Some(resp) =
                rate_limit_failed_get_user_code_attempt(db.as_ref(), &req, cfg.as_ref()).await
            {
                return resp;
            }
            render_form(
                user_code,
                Some("invalid or expired code"),
                StatusCode::BAD_REQUEST,
                None,
            )
        }
        Err(e) => {
            tracing::error!(error = %e, "pending device user-code detail lookup failed");
            if let Some(resp) =
                rate_limit_failed_get_user_code_attempt(db.as_ref(), &req, cfg.as_ref()).await
            {
                return resp;
            }
            render_form(
                user_code,
                Some("invalid or expired code"),
                StatusCode::BAD_REQUEST,
                None,
            )
        }
    }
}

async fn rate_limit_failed_get_user_code_attempt(
    db: &compio_postgres::Client,
    req: &HttpRequest,
    cfg: &AuthConfig,
) -> Option<HttpResponse> {
    let session = current_session(req, cfg, db).await;
    rate_limit_failed_user_code_attempt(db, req, session.as_ref()).await
}

/// `/device` POST - approve a pending device grant. Anonymous
/// browsers are sent into the normal sign-in route; already-signed-in browsers
/// complete the matching grant.
#[allow(clippy::future_not_send)]
pub async fn post(
    req: HttpRequest,
    form: ntex::web::types::Form<DeviceForm>,
    cfg: ntex::web::types::State<Arc<AuthConfig>>,
    db: ntex::web::types::State<Arc<compio_postgres::Client>>,
) -> HttpResponse {
    // CSRF double-submit — enforced FIRST, before any state change, exactly
    // like the login/signup/consent/reset siblings.
    if !csrf_valid(&req, &form) {
        return render_form(
            "",
            Some("invalid request"),
            StatusCode::FORBIDDEN,
            None,
        );
    }

    let user_code = form.user_code.trim();
    if user_code.is_empty() {
        return render_form(
            "",
            Some("enter the code shown on your device"),
            StatusCode::BAD_REQUEST,
            None,
        );
    }
    if !device_token::valid_user_code(user_code) {
        return render_form(
            "",
            Some("invalid or expired code"),
            StatusCode::BAD_REQUEST,
            None,
        );
    }

    let session = current_session(&req, cfg.as_ref(), db.as_ref()).await;

    let pending = match device_token::pending_user_code_details(db.as_ref(), user_code).await {
        Ok(details) => details,
        Err(e) => {
            tracing::error!(error = %e, "native device user-code detail lookup failed");
            return render_form(
                user_code,
                Some("invalid or expired code"),
                StatusCode::BAD_REQUEST,
                None,
            );
        }
    };
    let Some(pending) = pending else {
        if let Some(resp) =
            rate_limit_failed_user_code_attempt(db.as_ref(), &req, session.as_ref())
                .await
        {
            return resp;
        }
        return render_form(
            user_code,
            Some("invalid or expired code"),
            StatusCode::BAD_REQUEST,
            None,
        );
    };

    let Some(session) = session else {
        return redirect("/login");
    };

    if let Err(e) = eligibility::check_user_eligible(db.as_ref(), &session.user_id).await {
        if !e.is_account_state() {
            tracing::error!(error = %e, user_id = session.user_id.as_str(), "device grant eligibility check failed");
            return render_form(
                user_code,
                Some("invalid or expired code"),
                StatusCode::BAD_REQUEST,
                Some(&pending),
            );
        }
        return render_form(
            user_code,
            Some("account temporarily locked"),
            StatusCode::FORBIDDEN,
            Some(&pending),
        );
    }

    if form.confirm.as_deref() != Some("authorize") {
        return render_form(
            user_code,
            None,
            StatusCode::OK,
            Some(&pending),
        );
    }

    match device_token::approve_user_code(
        db.as_ref(),
        user_code,
        &pending.provider,
        &session.user_id,
        session.id,
        session.credential_version,
    )
    .await
    {
        Ok(DeviceApproval::Approved) => {
            emit_device_grant_audit(db.as_ref(), &req, &session).await;
            render_device_approved()
        }
        Ok(DeviceApproval::NotFound) => {
            if let Some(resp) =
                rate_limit_failed_user_code_attempt(db.as_ref(), &req, Some(&session))
                    .await
            {
                return resp;
            }
            render_form(
                user_code,
                Some("invalid or expired code"),
                StatusCode::BAD_REQUEST,
                None,
            )
        }
        Err(e) => {
            tracing::error!(error = %e, user_id = session.user_id.as_str(), "native device grant approval failed");
            render_form(
                user_code,
                Some("invalid or expired code"),
                StatusCode::BAD_REQUEST,
                None,
            )
        }
    }
}

async fn emit_device_grant_audit(
    db: &compio_postgres::Client,
    req: &HttpRequest,
    session: &sessions::Session,
) {
    audit::emit(
        db,
        &AuditEvent {
            event_type: "device_grant",
            outcome: "success",
            user_id: Some(&session.user_id),
            auth_method: Some("device"),
            detail: json!({
                "session_id": session.id.to_string(),
            }),
            ..AuditEvent::from_request(req)
        },
    )
    .await;
}

async fn rate_limit_failed_user_code_attempt(
    db: &compio_postgres::Client,
    req: &HttpRequest,
    session: Option<&sessions::Session>,
) -> Option<HttpResponse> {
    let ip = headers::client_ip(req);
    if let Some(session) = session {
        let key = format!("device:user_ip:{}:{ip}", session.user_id.as_str());
        if let Some(resp) = consume_failed_attempt_bucket(db, &key, Quota::LOGIN_EIP).await
        {
            return Some(resp);
        }
    }

    let key = format!("device:ip:{ip}");
    consume_failed_attempt_bucket(db, &key, Quota::LOGIN_IP).await
}

async fn consume_failed_attempt_bucket(
    db: &compio_postgres::Client,
    key: &str,
    bucket: Quota,
) -> Option<HttpResponse> {
    match rate_limit::consume(db, key, bucket).await {
        Ok(RateLimitDecision::Allowed) => None,
        Ok(RateLimitDecision::Throttled(_)) => Some(render_form(
            "",
            Some("too many attempts, try again later"),
            StatusCode::TOO_MANY_REQUESTS,
            None,
        )),
        Err(e) => {
            tracing::error!(error = %e, bucket = %key, "device user-code rate-limit consume failed");
            Some(render_form(
                "",
                Some("try again later"),
                StatusCode::SERVICE_UNAVAILABLE,
                None,
            ))
        }
    }
}

/// Double-submit CSRF check for the device-confirmation POST. Mirrors the
/// `consent.rs` / `reset.rs` helpers: the form-field token must be present and
/// byte-equal (constant-time) to the `__Host-zsidp_csrf` cookie token.
fn csrf_valid(req: &HttpRequest, form: &DeviceForm) -> bool {
    let cookie_header = req
        .headers()
        .get(COOKIE)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let cookie_token = csrf::parse_cookie(cookie_header);
    let Some(form_token) = form.csrf.as_deref() else {
        return false;
    };
    cookie_token
        .as_deref()
        .is_some_and(|cookie| csrf::matches(form_token, cookie))
}

async fn current_session(
    req: &HttpRequest,
    _cfg: &AuthConfig,
    db: &compio_postgres::Client,
) -> Option<sessions::Session> {
    let cookie_header = req
        .headers()
        .get(COOKIE)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let session_id = session_cookie::parse_cookie(cookie_header)?;
    sessions::validate(db, session_id).await.ok().flatten()
}

fn redirect(to: &str) -> HttpResponse {
    let mut resp = HttpResponse::Found();
    resp.header(
        LOCATION,
        HeaderValue::from_str(to).unwrap_or_else(|_| HeaderValue::from_static("/")),
    );
    resp.finish()
}

/// Name shown on the confirmation page for a grant that names no OAuth client.
///
/// Control's parallel-flow rows carried a NULL `client_id` and did not bind to
/// auth's first-party CLI registration, so the page still had to name what the
/// human was authorizing. That flow is deleted and nothing writes such a row
/// now, so this arm is unreachable in practice; it is kept only because the
/// `provider` discriminator it reads is still schema.
const PLATFORM_DEVICE_CLIENT_NAME: &str = "the zeroship CLI";

/// What the confirmation page calls the thing asking for authorization.
fn confirmation_client_name(details: &device_token::PendingDeviceGrant) -> &str {
    match details.client_name.as_str() {
        "" if details.is_platform() => PLATFORM_DEVICE_CLIENT_NAME,
        name => name,
    }
}

fn render_form(
    user_code: &str,
    error: Option<&str>,
    status: StatusCode,
    details: Option<&device_token::PendingDeviceGrant>,
) -> HttpResponse {
    let csrf_token = csrf::generate_token();
    let scopes = details
        .map(|details| device_scope_views(&details.scopes))
        .unwrap_or_default();
    let page = DevicePage {
        user_code,
        error,
        csrf: &csrf_token,
        confirm: details.is_some(),
        client_id: details.map(|details| details.client_id.as_str()).unwrap_or(""),
        client_name: details.map(confirmation_client_name).unwrap_or(""),
        scopes: &scopes,
    };
    let body = page
        .render()
        .unwrap_or_else(|_| "<h1>device authorization</h1>".to_string());
    let mut resp = HttpResponse::build(status);
    resp.content_type("text/html; charset=utf-8");
    resp.header(SET_COOKIE, csrf::set_cookie(&csrf_token));
    resp.body(body)
}

fn device_scope_views(scopes: &[String]) -> Vec<DeviceScopeView> {
    scopes
        .iter()
        .map(|scope| DeviceScopeView {
            scope: scope.clone(),
            label: standard_scope_label(scope)
                .map(str::to_string)
                .or_else(|| Scope::parse(scope).ok().map(|scope| scope.human_label().to_string()))
                .unwrap_or_else(|| scope.clone()),
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

fn render_device_approved() -> HttpResponse {
    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(
            "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
             <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
             <title>Device approved · zeroship</title></head><body>\
             <main><h1>zeroship</h1><h2>Device approved</h2>\
             <p>You can return to the device that requested access.</p></main>\
             </body></html>",
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The device screen is the consent screen's peer and must hold no copy of
    /// its own either: a platform scope renders exactly `Scope::human_label()`.
    ///
    /// The two renderers are separate functions in separate files, so "the
    /// consent screen cannot name a deleted authority" says nothing about this
    /// one. `zeroship deploy` from a headless machine goes through HERE, which
    /// makes it the screen a creator is most likely to be reading when they
    /// approve a CLI - and it was the screen that would have said "Roll back
    /// deployments" for an authority no handler ever checked.
    #[test]
    fn every_platform_scope_renders_its_own_human_label_and_nothing_else() {
        let requested: Vec<String> = Scope::ALL
            .iter()
            .map(|scope| scope.as_str().to_owned())
            .collect();
        assert!(
            requested.len() >= 10,
            "ruled on {} scope(s) - the vocabulary extraction collapsed",
            requested.len()
        );

        let views = device_scope_views(&requested);
        assert_eq!(views.len(), Scope::ALL.len());
        for (scope, view) in Scope::ALL.iter().zip(views) {
            assert_eq!(view.scope, scope.as_str());
            assert_eq!(
                view.label,
                scope.human_label(),
                "{} renders copy the enum does not own",
                scope.as_str()
            );
        }
    }

    #[test]
    fn device_user_code_format_is_bounded() {
        assert!(device_token::valid_user_code("BCDF-GHJK-LMNP"));
        assert!(device_token::valid_user_code("bcdf ghjk lmnp"));
        assert!(!device_token::valid_user_code("ABCD-EFGH"));
        assert!(!device_token::valid_user_code(&"A".repeat(32)));
        assert!(!device_token::valid_user_code(""));
        assert!(!device_token::valid_user_code(&"A".repeat(33)));
    }

    #[test]
    fn native_device_entry_template_keeps_code_form_shape() {
        let scopes = Vec::new();
        let body = DevicePage {
            user_code: "",
            error: None,
            csrf: "csrf-token",
            confirm: false,
            client_id: "",
            client_name: "",
            scopes: &scopes,
        }
        .render()
        .expect("render native device entry template");
        assert!(
            body.contains("Enter the code shown on your device"),
            "{body}"
        );
        assert!(body.contains(r#"<form method="POST" action="/device">"#), "{body}");
        assert!(
            body.contains(r#"name="user_code" value="""#),
            "native GET should continue ignoring verification_uri_complete user_code: {body}"
        );
        assert!(!body.contains("supabaseAuthUrl"), "{body}");
        assert!(!body.contains("/api/device/approve"), "{body}");
        assert!(!body.contains(r#"name="email""#), "{body}");
    }

    /// A control-plane grant reaches the confirmation page with no OAuth client
    /// to name, and the human still has to be told what they are approving.
    ///
    /// What this does NOT catch: whether the page ever RECEIVES such a grant.
    /// That is `pending_user_code_details` dropping its `provider = 'op'` filter
    /// and its inner join, which needs a database, and end to end it is
    /// `tests/e2e_device_login.sh`. This pins only the rendering.
    #[test]
    fn a_platform_grant_names_the_cli_and_discloses_its_deploy_scopes() {
        let pending = device_token::PendingDeviceGrant {
            client_id: String::new(),
            client_name: String::new(),
            scopes: vec![
                "apps:archive".to_string(),
                "apps:deploy".to_string(),
                "apps:read".to_string(),
                "apps:write".to_string(),
            ],
            provider: "platform".to_string(),
        };
        assert!(pending.is_platform());

        let body = render_body(&pending);
        assert!(body.contains("Authorize the zeroship CLI"), "{body}");
        for scope in ["apps:archive", "apps:deploy", "apps:read", "apps:write"] {
            assert!(body.contains(scope), "scope {scope} missing from {body}");
        }
        assert!(body.contains(r#"name="confirm" value="authorize""#), "{body}");
        assert!(body.contains(r#"name="csrf""#), "{body}");
    }

    /// The one-variable control for the case above: same page, same render
    /// path, one thing changed - the grant names an OAuth client. The client id
    /// line appears only when there is one, so the platform case is not simply
    /// rendering an empty paragraph.
    #[test]
    fn an_op_grant_still_names_its_registered_client() {
        let pending = device_token::PendingDeviceGrant {
            client_id: "oac_test".to_string(),
            client_name: "Test Device App".to_string(),
            scopes: vec!["openid".to_string()],
            provider: "op".to_string(),
        };
        assert!(!pending.is_platform());

        let body = render_body(&pending);
        assert!(body.contains("Authorize Test Device App"), "{body}");
        assert!(body.contains("oac_test"), "{body}");
        assert!(!body.contains("the zeroship CLI"), "{body}");
    }

    /// Render the confirmation page the way `render_form` does, through the
    /// same two decisions the handler delegates to it: the displayed client
    /// name and the scope list.
    fn render_body(pending: &device_token::PendingDeviceGrant) -> String {
        let scopes = device_scope_views(&pending.scopes);
        DevicePage {
            user_code: "BCDF-GHJK-LMNP",
            error: None,
            csrf: "csrf-token",
            confirm: true,
            client_id: &pending.client_id,
            client_name: confirmation_client_name(pending),
            scopes: &scopes,
        }
        .render()
        .expect("render device confirmation")
    }

    #[test]
    fn native_device_confirmation_template_shows_client_and_scopes() {
        let scopes = vec![DeviceScopeView {
            scope: "apps:read".to_string(),
            label: "Read app metadata".to_string(),
        }];
        let body = DevicePage {
            user_code: "BCDF-GHJK-LMNP",
            error: None,
            csrf: "csrf-token",
            confirm: true,
            client_id: "oac_test",
            client_name: "Test Device App",
            scopes: &scopes,
        }
        .render()
        .expect("render native device confirmation template");
        assert!(body.contains("Authorize Test Device App"), "{body}");
        assert!(body.contains("oac_test"), "{body}");
        assert!(body.contains("Read app metadata"), "{body}");
        assert!(body.contains("apps:read"), "{body}");
        assert!(body.contains(r#"name="confirm" value="authorize""#), "{body}");
        assert!(body.contains(r#"name="user_code" value="BCDF-GHJK-LMNP""#), "{body}");
    }
}
