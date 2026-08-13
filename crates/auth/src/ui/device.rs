//! `/device` GET + POST handlers for OAuth 2.0 Device Authorization Grant
//! user-code entry (RFC 8628).
//!
//! This page is the human end of BOTH device flows that share
//! `zeroship.device_grants`, and it dispatches on the pending row's `provider`
//! rather than on how the process is configured:
//!
//! * an `op` row is the auth service's own grant, redeemed at `/oauth2/token`;
//! * a `platform` row is control's deploy grant, the one `zeroship login`
//!   drives, redeemed at control's `/api/device/token`.
//!
//! Approval is the same write for both - bind the signed-in user to the row -
//! because `principal_id` is a `zeroship.users` id in both. That is what lets
//! this page approve a control-plane grant while holding no control credential.
//!
//! It did not used to. The page rendered the control-approving form only under
//! `AuthProviderKind::Supabase` and otherwise drove the OP grant, which filtered
//! `provider = 'op'`; control writes `provider = 'platform'`. On the shipped
//! platform-only default the code `zeroship login` printed was invisible to the
//! page `zeroship login` told the human to open, and every attempt read
//! "invalid or expired code".

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
use crate::ratelimit::{self, Bucket, RateLimitDecision};
use crate::sessions::login as session_cookie;
use crate::store::sessions;
use crate::ui::{DevicePage, DeviceScopeView, SupabaseDevicePage};
use zeroship_authz::Scope;
use zeroship_core::config::AuthProviderKind;

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
    if cfg.auth_provider() == AuthProviderKind::Supabase {
        let user_code = query.user_code.as_deref().unwrap_or("").trim();
        return render_supabase_form(cfg.as_ref(), user_code, None, StatusCode::OK);
    }
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

/// `/device` POST — approve a pending device grant, of either flow. Anonymous
/// browsers are sent into the normal sign-in route; already-signed-in browsers
/// complete the matching grant.
#[allow(clippy::future_not_send)]
pub async fn post(
    req: HttpRequest,
    form: ntex::web::types::Form<DeviceForm>,
    cfg: ntex::web::types::State<Arc<AuthConfig>>,
    db: ntex::web::types::State<Arc<compio_postgres::Client>>,
) -> HttpResponse {
    if cfg.auth_provider() == AuthProviderKind::Supabase {
        return render_supabase_form(
            cfg.as_ref(),
            form.user_code.trim(),
            Some("device approval is completed in the browser"),
            StatusCode::METHOD_NOT_ALLOWED,
        );
    }

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

    if let Err(e) = eligibility::check_user_eligible(db.as_ref(), session.user_id).await {
        if !e.is_account_state() {
            tracing::error!(error = %e, user_id = %session.user_id, "device grant eligibility check failed");
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
        session.user_id,
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
            tracing::error!(error = %e, user_id = %session.user_id, "native device grant approval failed");
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
        let key = format!("device:user_ip:{}:{ip}", session.user_id);
        if let Some(resp) = consume_failed_attempt_bucket(db, &key, Bucket::LOGIN_EIP).await
        {
            return Some(resp);
        }
    }

    let key = format!("device:ip:{ip}");
    consume_failed_attempt_bucket(db, &key, Bucket::LOGIN_IP).await
}

async fn consume_failed_attempt_bucket(
    db: &compio_postgres::Client,
    key: &str,
    bucket: Bucket,
) -> Option<HttpResponse> {
    match ratelimit::consume(db, key, bucket).await {
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
/// Control's rows carry a NULL `client_id`: `zeroship.device_grants.client_id`
/// has a foreign key into `zeroship.oauth_clients`, and the CLI is not a
/// registered OP client, so there is no id to store and none to display. The
/// page still has to tell the human what they are authorizing.
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

fn render_supabase_form(
    cfg: &AuthConfig,
    user_code: &str,
    error: Option<&str>,
    status: StatusCode,
) -> HttpResponse {
    let script_nonce = csrf::generate_token();
    let supabase_auth_url = format!(
        "{}/auth/v1",
        cfg.supabase_url().unwrap_or("").trim_end_matches('/')
    );
    let control_approve_url = format!(
        "{}/api/device/approve",
        cfg.control_url().trim_end_matches('/')
    );
    let supabase_auth_url_json = json_for_script(&supabase_auth_url);
    let supabase_anon_key_json = json_for_script(cfg.supabase_anon_key().unwrap_or(""));
    let control_approve_url_json = json_for_script(&control_approve_url);
    let page = SupabaseDevicePage {
        user_code,
        error,
        script_nonce: &script_nonce,
        supabase_auth_url_json: &supabase_auth_url_json,
        supabase_anon_key_json: &supabase_anon_key_json,
        control_approve_url_json: &control_approve_url_json,
    };
    let body = page
        .render()
        .unwrap_or_else(|_| "<h1>device authorization</h1>".to_string());
    let mut resp = HttpResponse::build(status);
    resp.content_type("text/html; charset=utf-8");
    let csp = supabase_device_csp(&script_nonce, &supabase_auth_url, &control_approve_url);
    if let Ok(value) = HeaderValue::from_str(&csp) {
        resp.header("Content-Security-Policy", value);
    } else {
        resp.header(
            "Content-Security-Policy",
            headers::content_security_policy_with_script_nonce(&script_nonce),
        );
    }
    resp.body(body)
}

fn json_for_script(value: &str) -> String {
    serde_json::to_string(value)
        .expect("serialize script string")
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

fn supabase_device_csp(nonce: &str, supabase_auth_url: &str, control_approve_url: &str) -> String {
    let mut connect = vec!["'self'".to_string()];
    for source in [
        csp_source_origin(supabase_auth_url),
        csp_source_origin(control_approve_url),
    ]
    .into_iter()
    .flatten()
    {
        if !connect.iter().any(|existing| existing == &source) {
            connect.push(source);
        }
    }
    format!(
        "default-src 'self'; \
         script-src 'self' 'nonce-{nonce}'; \
         style-src 'self'; \
         img-src 'self' data: https://*.zeroship.ai \
                       https://lh3.googleusercontent.com \
                       https://avatars.githubusercontent.com; \
         connect-src {}; \
         form-action 'self'; \
         frame-ancestors 'none'; \
         base-uri 'none'; \
         object-src 'none'; \
         upgrade-insecure-requests",
        connect.join(" ")
    )
}

fn csp_source_origin(raw_url: &str) -> Option<String> {
    let parsed = url::Url::parse(raw_url).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }
    let host = parsed.host_str()?;
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    let port = parsed.port().map_or_else(String::new, |port| format!(":{port}"));
    Some(format!("{}://{}{}", parsed.scheme(), host, port))
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn supabase_device_template_renders_gotrue_approval_page() {
        let supabase_auth_url = "https://project.supabase.test/auth/v1";
        let control_approve_url = "https://control.zeroship.test/api/device/approve";
        let body = SupabaseDevicePage {
            user_code: "BCDF-GHJK-LMNP",
            error: None,
            script_nonce: "script-nonce",
            supabase_auth_url_json: &json_for_script(supabase_auth_url),
            supabase_anon_key_json: &json_for_script("anon-test-key"),
            control_approve_url_json: &json_for_script(control_approve_url),
        }
        .render()
        .expect("render supabase device template");
        assert!(body.contains("Authorize device"), "{body}");
        assert!(body.contains(r#"name="user_code" value="BCDF-GHJK-LMNP""#), "{body}");
        assert!(!body.contains(r#"name="csrf""#), "{body}");
        assert!(body.contains("https://project.supabase.test/auth/v1"), "{body}");
        assert!(
            body.contains("https://control.zeroship.test/api/device/approve"),
            "{body}"
        );
        assert!(!body.contains("x-zeroship-csrf"), "{body}");
        assert!(
            !body.contains("/oauth2/device/verify"),
            "Supabase render must not reference native device verification: {body}"
        );

        let csp = supabase_device_csp("script-nonce", supabase_auth_url, control_approve_url);
        assert!(csp.contains("script-src 'self' 'nonce-script-nonce'"), "{csp}");
        assert!(
            csp.contains(
                "connect-src 'self' https://project.supabase.test https://control.zeroship.test"
            ),
            "{csp}"
        );
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
                "apps:deploy".to_string(),
                "apps:read".to_string(),
                "apps:write".to_string(),
            ],
            provider: "platform".to_string(),
        };
        assert!(pending.is_platform());

        let body = render_body(&pending);
        assert!(body.contains("Authorize the zeroship CLI"), "{body}");
        for scope in ["apps:deploy", "apps:read", "apps:write"] {
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
