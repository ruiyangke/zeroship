//! `/device` GET + POST handlers for OAuth 2.0 Device Authorization Grant
//! user-code entry (RFC 8628).

use std::sync::Arc;
use std::time::Duration;

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
use crate::hydra_client::types::AcceptDeviceUserCodeRequest;
use crate::hydra_client::HydraAdmin;
use crate::identity::eligibility;
use crate::sessions::login as session_cookie;
use crate::store::sessions;
use crate::ui::{DevicePage, SupabaseDevicePage};

const MAX_USER_CODE_BYTES: usize = 32;

#[derive(Debug, Deserialize)]
pub struct DeviceForm {
    pub user_code: String,
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
) -> HttpResponse {
    if cfg.auth_provider() == AuthProviderKind::Supabase {
        let user_code = query.user_code.as_deref().unwrap_or("").trim();
        return render_supabase_form(cfg.as_ref(), user_code, None, StatusCode::OK);
    }
    render_form("", None, StatusCode::OK, cfg.insecure_dev)
}

/// `/device` POST — validate the typed user code through Hydra's public
/// device-verification endpoint. Anonymous browsers are sent into the normal
/// sign-in route; already-signed-in browsers complete the device flow by
/// accepting the device challenge with Hydra's admin API.
#[allow(clippy::future_not_send)]
pub async fn post(
    req: HttpRequest,
    form: ntex::web::types::Form<DeviceForm>,
    admin: ntex::web::types::State<HydraAdmin>,
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

    // CSRF double-submit — enforced FIRST, before any Hydra round-trip or
    // state change, exactly like the login/signup/consent/reset siblings.
    if !csrf_valid(&req, &form, insecure_dev) {
        return render_form(
            "",
            Some("invalid request"),
            StatusCode::FORBIDDEN,
            insecure_dev,
        );
    }

    let user_code = form.user_code.trim();
    if user_code.is_empty() {
        return render_form(
            "",
            Some("enter the code shown on your device"),
            StatusCode::BAD_REQUEST,
            insecure_dev,
        );
    }
    if !valid_user_code(user_code) {
        return render_form(
            "",
            Some("invalid or expired code"),
            StatusCode::BAD_REQUEST,
            insecure_dev,
        );
    }

    let verified = match verify_user_code(cfg.hydra_public_url(), user_code).await {
        Ok(v) => v,
        Err(DeviceVerifyError::Rejected) => {
            return render_form(
                user_code,
                Some("invalid or expired code"),
                StatusCode::BAD_REQUEST,
                insecure_dev,
            );
        }
        Err(DeviceVerifyError::Hydra(e)) => {
            tracing::warn!(error = %e, "device user-code verification failed");
            return render_form(
                user_code,
                Some("invalid or expired code"),
                StatusCode::BAD_REQUEST,
                insecure_dev,
            );
        }
    };

    let Some(device_challenge) = verified.device_challenge else {
        tracing::warn!(
            location = ?verified.location,
            "device verification response had no device_challenge"
        );
        return render_form(
            user_code,
            Some("invalid or expired code"),
            StatusCode::BAD_REQUEST,
            insecure_dev,
        );
    };

    let Some(session) = current_session(&req, cfg.as_ref(), db.as_ref()).await else {
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
            );
        }
        return render_form(
            user_code,
            Some("account temporarily locked"),
            StatusCode::FORBIDDEN,
            insecure_dev,
        );
    }

    let accept = AcceptDeviceUserCodeRequest {
        user_code: Some(user_code.to_string()),
    };
    match admin.accept_device_user_code(&device_challenge, &accept).await {
        Ok(resp) => {
            audit::emit(
                db.as_ref(),
                &AuditEvent {
                    event_type: "device_grant",
                    outcome: "success",
                    user_id: Some(&session.user_id),
                    auth_method: Some("device"),
                    detail: json!({
                        "session_id": session.id.to_string(),
                    }),
                    ..AuditEvent::from_request(&req)
                },
            )
            .await;
            redirect(&resp.redirect_to)
        }
        Err(e) => {
            tracing::warn!(error = %e, "accept device user code failed");
            render_form(
                user_code,
                Some("invalid or expired code"),
                StatusCode::BAD_REQUEST,
                insecure_dev,
            )
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

#[derive(Debug)]
struct VerifiedDeviceCode {
    location: String,
    device_challenge: Option<String>,
}

#[derive(Debug)]
enum DeviceVerifyError {
    Rejected,
    Hydra(String),
}

#[allow(clippy::future_not_send)]
async fn verify_user_code(
    hydra_public: &str,
    user_code: &str,
) -> Result<VerifiedDeviceCode, DeviceVerifyError> {
    let q = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("user_code", user_code)
        .finish();
    let url = format!(
        "{}/oauth2/device/verify?{}",
        hydra_public.trim_end_matches('/'),
        q,
    );
    let res = cyper::Client::new()
        .request(http::Method::GET, url)
        .map_err(|e| DeviceVerifyError::Hydra(format!("build GET /oauth2/device/verify: {e}")))?
        .send_with_timeout(Duration::from_secs(10))
        .await
        .map_err(|_| DeviceVerifyError::Hydra("GET /oauth2/device/verify: timeout".into()))?
        .map_err(|e| DeviceVerifyError::Hydra(format!("GET /oauth2/device/verify: {e}")))?;

    let status = res.status().as_u16();
    if status == 400 || status == 404 {
        return Err(DeviceVerifyError::Rejected);
    }
    if !(300..400).contains(&status) {
        let body = res.text().await.unwrap_or_else(|_| "<no body>".into());
        return Err(DeviceVerifyError::Hydra(format!(
            "GET /oauth2/device/verify -> {status}: {body}"
        )));
    }

    let location = res
        .headers()
        .get(LOCATION)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("")
        .to_string();
    let device_challenge = query_param(&location, "device_challenge");
    Ok(VerifiedDeviceCode {
        location,
        device_challenge,
    })
}

trait SendWithTimeout {
    async fn send_with_timeout(
        self,
        timeout: Duration,
    ) -> std::result::Result<cyper::Result<cyper::Response>, compio::time::Elapsed>;
}

impl SendWithTimeout for cyper::RequestBuilder {
    async fn send_with_timeout(
        self,
        timeout: Duration,
    ) -> std::result::Result<cyper::Result<cyper::Response>, compio::time::Elapsed> {
        compio::time::timeout(timeout, self.send()).await
    }
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

fn query_param(raw_url: &str, key: &str) -> Option<String> {
    let parsed = url::Url::parse(raw_url).ok()?;
    parsed
        .query_pairs()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
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
) -> HttpResponse {
    let csrf_token = csrf::generate_token();
    let page = DevicePage {
        user_code,
        error,
        csrf: &csrf_token,
    };
    let body = page
        .render()
        .unwrap_or_else(|_| "<h1>device authorization</h1>".to_string());
    let mut resp = HttpResponse::build(status);
    resp.content_type("text/html; charset=utf-8");
    resp.header(SET_COOKIE, csrf::set_cookie(&csrf_token, insecure_dev));
    resp.body(body)
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

fn valid_user_code(user_code: &str) -> bool {
    !user_code.is_empty() && user_code.len() <= MAX_USER_CODE_BYTES
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use ntex::http::HeaderMap;
    use ntex::web;
    use ntex::web::test;
    use zeroship_core::config::AuthSection;

    fn hydra_cfg() -> Arc<AuthConfig> {
        let mut cfg = AuthConfig::parse_from(["zeroship-auth", "--db-url", "postgres://test"]);
        cfg.try_resolve(AuthSection::default())
            .expect("resolve hydra test config");
        Arc::new(cfg)
    }

    fn supabase_cfg() -> Arc<AuthConfig> {
        let mut cfg = AuthConfig::parse_from([
            "zeroship-auth",
            "--db-url",
            "postgres://test",
            "--auth-provider",
            "supabase",
            "--supabase-url",
            "https://project.supabase.test",
            "--supabase-anon-key",
            "anon-test-key",
            "--control-url",
            "https://control.zeroship.test",
        ]);
        cfg.try_resolve(AuthSection::default())
            .expect("resolve supabase test config");
        Arc::new(cfg)
    }

    async fn get_device_body(cfg: Arc<AuthConfig>, uri: &str) -> (StatusCode, HeaderMap, String) {
        let app = test::init_service(
            web::App::new()
                .state(cfg)
                .service(web::resource("/device").route(web::get().to(get))),
        )
        .await;
        let resp = test::call_service(&app, test::TestRequest::get().uri(uri).to_request()).await;
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = String::from_utf8(test::read_body(resp).await.to_vec())
            .expect("device response body is utf8");
        (status, headers, body)
    }

    #[test]
    fn device_user_code_is_bounded() {
        assert!(valid_user_code("ABCD-EFGH"));
        assert!(valid_user_code(&"A".repeat(32)));
        assert!(!valid_user_code(""));
        assert!(!valid_user_code(&"A".repeat(33)));
    }

    #[ntex::test]
    async fn supabase_device_get_renders_gotrue_approval_page_with_csrf() {
        let (status, headers, body) =
            get_device_body(supabase_cfg(), "/device?user_code=BCDF-GHJK").await;

        assert_eq!(status, StatusCode::OK);
        let set_cookie = headers
            .get(SET_COOKIE)
            .and_then(|value| value.to_str().ok())
            .expect("supabase /device sets csrf cookie");
        assert!(
            set_cookie.starts_with("__Host-zsidp_csrf="),
            "prod csrf cookie must use __Host- prefix: {set_cookie}"
        );
        let csrf_token = set_cookie
            .split(';')
            .next()
            .and_then(|kv| kv.strip_prefix("__Host-zsidp_csrf="))
            .expect("csrf cookie token");

        assert!(body.contains("Authorize device"), "{body}");
        assert!(body.contains(r#"name="user_code" value="BCDF-GHJK""#), "{body}");
        assert!(body.contains(r#"name="csrf""#), "{body}");
        assert!(
            body.contains(&format!(r#"value="{csrf_token}""#)),
            "hidden csrf field must echo the csrf cookie token: {body}"
        );
        assert!(body.contains("https://project.supabase.test/auth/v1"), "{body}");
        assert!(
            body.contains("https://control.zeroship.test/api/device/approve"),
            "{body}"
        );
        assert!(body.contains("x-zeroship-csrf"), "{body}");
        assert!(
            !body.contains("/oauth2/device/verify"),
            "Supabase render must not reference Hydra verification: {body}"
        );

        let csp = headers
            .get("content-security-policy")
            .and_then(|value| value.to_str().ok())
            .expect("supabase /device sets nonce CSP");
        assert!(csp.contains("script-src 'self' 'nonce-"), "{csp}");
        assert!(
            csp.contains(
                "connect-src 'self' https://project.supabase.test https://control.zeroship.test"
            ),
            "{csp}"
        );
    }

    #[ntex::test]
    async fn hydra_device_get_keeps_existing_page_shape() {
        let (status, _headers, body) =
            get_device_body(hydra_cfg(), "/device?user_code=BCDF-GHJK").await;

        assert_eq!(status, StatusCode::OK);
        assert!(
            body.contains("Enter the code shown on your device"),
            "{body}"
        );
        assert!(body.contains(r#"<form method="POST" action="/device">"#), "{body}");
        assert!(
            body.contains(r#"name="user_code" value="""#),
            "Hydra GET should continue ignoring verification_uri_complete user_code: {body}"
        );
        assert!(!body.contains("supabaseAuthUrl"), "{body}");
        assert!(!body.contains("/api/device/approve"), "{body}");
        assert!(!body.contains(r#"name="email""#), "{body}");
    }
}
