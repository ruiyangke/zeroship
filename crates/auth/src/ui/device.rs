//! `/device` GET + POST handlers for OAuth 2.0 Device Authorization Grant
//! user-code entry (RFC 8628).

use std::sync::Arc;

use askama::Template;
use ntex::http::header::{HeaderValue, COOKIE, LOCATION, SET_COOKIE};
use ntex::http::StatusCode;
use ntex::web::{HttpRequest, HttpResponse};
use serde::Deserialize;
use serde_json::json;

use crate::audit::{self, AuditEvent};
use crate::config::{AuthConfig, AuthProviderKind};
use crate::csrf;
use crate::headers;
use crate::identity::eligibility;
use crate::oidc::device_token::{self, DeviceApproval};
use crate::ratelimit::{self, Bucket, RateLimitDecision};
use crate::sessions::login as session_cookie;
use crate::store::sessions;
use crate::ui::{DevicePage, DeviceScopeView, SupabaseDevicePage};
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
        return render_form("", None, StatusCode::OK, cfg.insecure_dev, None);
    }
    if !device_token::valid_user_code(user_code) {
        return render_form(
            user_code,
            Some("invalid or expired code"),
            StatusCode::BAD_REQUEST,
            cfg.insecure_dev,
            None,
        );
    }
    match device_token::native_user_code_details(db.as_ref(), user_code).await {
        Ok(Some(details)) => {
            render_form(user_code, None, StatusCode::OK, cfg.insecure_dev, Some(&details))
        }
        Ok(None) => render_form(
            user_code,
            Some("invalid or expired code"),
            StatusCode::BAD_REQUEST,
            cfg.insecure_dev,
            None,
        ),
        Err(e) => {
            tracing::error!(error = %e, "native device user-code detail lookup failed");
            render_form(
                user_code,
                Some("invalid or expired code"),
                StatusCode::BAD_REQUEST,
                cfg.insecure_dev,
                None,
            )
        }
    }
}

/// `/device` POST — approve a native OP device grant. Anonymous browsers are
/// sent into the normal sign-in route; already-signed-in browsers complete the
/// matching grant.
#[allow(clippy::future_not_send)]
pub async fn post(
    req: HttpRequest,
    form: ntex::web::types::Form<DeviceForm>,
    cfg: ntex::web::types::State<Arc<AuthConfig>>,
    db: ntex::web::types::State<Arc<compio_postgres::Client>>,
) -> HttpResponse {
    let insecure_dev = cfg.insecure_dev;

    if cfg.auth_provider() == AuthProviderKind::Supabase {
        if !csrf_valid(&req, &form, insecure_dev) {
            return render_supabase_form(
                cfg.as_ref(),
                form.user_code.trim(),
                Some("invalid request"),
                StatusCode::FORBIDDEN,
            );
        }
        return render_supabase_form(
            cfg.as_ref(),
            form.user_code.trim(),
            Some("device approval is completed in the browser"),
            StatusCode::METHOD_NOT_ALLOWED,
        );
    }

    // CSRF double-submit — enforced FIRST, before any state change, exactly
    // like the login/signup/consent/reset siblings.
    if !csrf_valid(&req, &form, insecure_dev) {
        return render_form(
            "",
            Some("invalid request"),
            StatusCode::FORBIDDEN,
            insecure_dev,
            None,
        );
    }

    let user_code = form.user_code.trim();
    if user_code.is_empty() {
        return render_form(
            "",
            Some("enter the code shown on your device"),
            StatusCode::BAD_REQUEST,
            insecure_dev,
            None,
        );
    }
    if !device_token::valid_user_code(user_code) {
        return render_form(
            "",
            Some("invalid or expired code"),
            StatusCode::BAD_REQUEST,
            insecure_dev,
            None,
        );
    }

    let session = current_session(&req, cfg.as_ref(), db.as_ref()).await;

    let pending = match device_token::native_user_code_details(db.as_ref(), user_code).await {
        Ok(details) => details,
        Err(e) => {
            tracing::error!(error = %e, "native device user-code detail lookup failed");
            return render_form(
                user_code,
                Some("invalid or expired code"),
                StatusCode::BAD_REQUEST,
                insecure_dev,
                None,
            );
        }
    };
    let Some(pending) = pending else {
        if let Some(resp) =
            rate_limit_failed_user_code_attempt(db.as_ref(), &req, session.as_ref(), insecure_dev)
                .await
        {
            return resp;
        }
        return render_form(
            user_code,
            Some("invalid or expired code"),
            StatusCode::BAD_REQUEST,
            insecure_dev,
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
                insecure_dev,
                Some(&pending),
            );
        }
        return render_form(
            user_code,
            Some("account temporarily locked"),
            StatusCode::FORBIDDEN,
            insecure_dev,
            Some(&pending),
        );
    }

    if form.confirm.as_deref() != Some("authorize") {
        return render_form(
            user_code,
            None,
            StatusCode::OK,
            insecure_dev,
            Some(&pending),
        );
    }

    match device_token::approve_user_code(
        db.as_ref(),
        user_code,
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
                rate_limit_failed_user_code_attempt(db.as_ref(), &req, Some(&session), insecure_dev)
                    .await
            {
                return resp;
            }
            render_form(
                user_code,
                Some("invalid or expired code"),
                StatusCode::BAD_REQUEST,
                insecure_dev,
                None,
            )
        }
        Err(e) => {
            tracing::error!(error = %e, user_id = %session.user_id, "native device grant approval failed");
            render_form(
                user_code,
                Some("invalid or expired code"),
                StatusCode::BAD_REQUEST,
                insecure_dev,
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
    insecure_dev: bool,
) -> Option<HttpResponse> {
    let ip = headers::client_ip(req);
    if let Some(session) = session {
        let key = format!("device:user_ip:{}:{ip}", session.user_id);
        if let Some(resp) = consume_failed_attempt_bucket(
            db,
            &key,
            Bucket::LOGIN_EIP,
            insecure_dev,
        )
        .await
        {
            return Some(resp);
        }
    }

    let key = format!("device:ip:{ip}");
    consume_failed_attempt_bucket(db, &key, Bucket::LOGIN_IP, insecure_dev).await
}

async fn consume_failed_attempt_bucket(
    db: &compio_postgres::Client,
    key: &str,
    bucket: Bucket,
    insecure_dev: bool,
) -> Option<HttpResponse> {
    match ratelimit::consume(db, key, bucket).await {
        Ok(RateLimitDecision::Allowed) => None,
        Ok(RateLimitDecision::Throttled(_)) => Some(render_form(
            "",
            Some("too many attempts, try again later"),
            StatusCode::TOO_MANY_REQUESTS,
            insecure_dev,
            None,
        )),
        Err(e) => {
            tracing::error!(error = %e, bucket = %key, "device user-code rate-limit consume failed");
            Some(render_form(
                "",
                Some("try again later"),
                StatusCode::SERVICE_UNAVAILABLE,
                insecure_dev,
                None,
            ))
        }
    }
}

/// Double-submit CSRF check for the device-confirmation POST. Mirrors the
/// `consent.rs` / `reset.rs` helpers: the form-field token must be present and
/// byte-equal (constant-time) to the `__Host-zsidp_csrf` cookie token.
fn csrf_valid(req: &HttpRequest, form: &DeviceForm, insecure_dev: bool) -> bool {
    let cookie_header = req
        .headers()
        .get(COOKIE)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let cookie_token = csrf::parse_cookie(cookie_header, insecure_dev);
    let Some(form_token) = form.csrf.as_deref() else {
        return false;
    };
    cookie_token
        .as_deref()
        .is_some_and(|cookie| csrf::matches(form_token, cookie))
}

async fn current_session(
    req: &HttpRequest,
    cfg: &AuthConfig,
    db: &compio_postgres::Client,
) -> Option<sessions::Session> {
    let cookie_header = req
        .headers()
        .get(COOKIE)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let session_id = session_cookie::parse_cookie(cookie_header, cfg.insecure_dev)?;
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

fn render_form(
    user_code: &str,
    error: Option<&str>,
    status: StatusCode,
    insecure_dev: bool,
    details: Option<&device_token::NativeDeviceGrantDetails>,
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
        client_name: details
            .map(|details| details.client_name.as_str())
            .unwrap_or(""),
        scopes: &scopes,
    };
    let body = page
        .render()
        .unwrap_or_else(|_| "<h1>device authorization</h1>".to_string());
    let mut resp = HttpResponse::build(status);
    resp.content_type("text/html; charset=utf-8");
    resp.header(SET_COOKIE, csrf::set_cookie(&csrf_token, insecure_dev));
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
    let csrf_token = csrf::generate_token();
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
        csrf: &csrf_token,
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
    resp.header(SET_COOKIE, csrf::set_cookie(&csrf_token, cfg.insecure_dev));
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
            csrf: "csrf-token",
            script_nonce: "script-nonce",
            supabase_auth_url_json: &json_for_script(supabase_auth_url),
            supabase_anon_key_json: &json_for_script("anon-test-key"),
            control_approve_url_json: &json_for_script(control_approve_url),
        }
        .render()
        .expect("render supabase device template");
        assert!(body.contains("Authorize device"), "{body}");
        assert!(body.contains(r#"name="user_code" value="BCDF-GHJK-LMNP""#), "{body}");
        assert!(body.contains(r#"name="csrf""#), "{body}");
        assert!(body.contains(r#"value="csrf-token""#), "{body}");
        assert!(body.contains("https://project.supabase.test/auth/v1"), "{body}");
        assert!(
            body.contains("https://control.zeroship.test/api/device/approve"),
            "{body}"
        );
        assert!(body.contains("x-zeroship-csrf"), "{body}");
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
