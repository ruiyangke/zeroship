//! `/device` GET + POST handlers for OAuth 2.0 Device Authorization Grant
//! user-code entry (RFC 8628).

use std::sync::Arc;
use std::time::Duration;

use askama::Template;
use ntex::http::header::{HeaderValue, COOKIE, LOCATION};
use ntex::http::StatusCode;
use ntex::web::{HttpRequest, HttpResponse};
use serde::Deserialize;

use crate::config::AuthConfig;
use crate::hydra_client::types::AcceptDeviceUserCodeRequest;
use crate::hydra_client::HydraAdmin;
use crate::identity::eligibility;
use crate::sessions::login as session_cookie;
use crate::store::sessions;
use crate::ui::DevicePage;

const MAX_USER_CODE_BYTES: usize = 32;

#[derive(Debug, Deserialize)]
pub struct DeviceForm {
    pub user_code: String,
}

#[allow(clippy::future_not_send)]
pub async fn get() -> HttpResponse {
    render_form("", None, StatusCode::OK)
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
    let user_code = form.user_code.trim();
    if user_code.is_empty() {
        return render_form(
            "",
            Some("enter the code shown on your device"),
            StatusCode::BAD_REQUEST,
        );
    }
    if !valid_user_code(user_code) {
        return render_form("", Some("invalid or expired code"), StatusCode::BAD_REQUEST);
    }

    let verified = match verify_user_code(&cfg.hydra_public, user_code).await {
        Ok(v) => v,
        Err(DeviceVerifyError::Rejected) => {
            return render_form(
                user_code,
                Some("invalid or expired code"),
                StatusCode::BAD_REQUEST,
            );
        }
        Err(DeviceVerifyError::Hydra(e)) => {
            tracing::warn!(error = %e, "device user-code verification failed");
            return render_form(
                user_code,
                Some("invalid or expired code"),
                StatusCode::BAD_REQUEST,
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
            );
        }
        return render_form(
            user_code,
            Some("account temporarily locked"),
            StatusCode::FORBIDDEN,
        );
    }

    let accept = AcceptDeviceUserCodeRequest {
        user_code: Some(user_code.to_string()),
    };
    match admin.accept_device_user_code(&device_challenge, &accept).await {
        Ok(resp) => redirect(&resp.redirect_to),
        Err(e) => {
            tracing::warn!(error = %e, "accept device user code failed");
            render_form(user_code, Some("invalid or expired code"), StatusCode::BAD_REQUEST)
        }
    }
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

fn render_form(user_code: &str, error: Option<&str>, status: StatusCode) -> HttpResponse {
    let page = DevicePage { user_code, error };
    let body = page
        .render()
        .unwrap_or_else(|_| "<h1>device authorization</h1>".to_string());
    let mut resp = HttpResponse::build(status);
    resp.content_type("text/html; charset=utf-8");
    resp.body(body)
}

fn valid_user_code(user_code: &str) -> bool {
    !user_code.is_empty() && user_code.len() <= MAX_USER_CODE_BYTES
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_user_code_is_bounded() {
        assert!(valid_user_code("ABCD-EFGH"));
        assert!(valid_user_code(&"A".repeat(32)));
        assert!(!valid_user_code(""));
        assert!(!valid_user_code(&"A".repeat(33)));
    }
}
