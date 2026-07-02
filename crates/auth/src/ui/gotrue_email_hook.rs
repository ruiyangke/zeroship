//! GoTrue Send Email hook receiver.
//!
//! GoTrue's `GOTRUE_HOOK_SEND_EMAIL_*` path signs the raw JSON body with
//! Standard Webhooks and bypasses its built-in SMTP sender when this hook
//! succeeds. This handler verifies that signature before parsing, maps the
//! GoTrue action to a zeroship branded email template, and sends through the
//! shared suppression-aware `zeroship_mailer` trait.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use askama::Template;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use ntex::http::header::HeaderName;
use ntex::http::HeaderMap;
use ntex::http::StatusCode;
use ntex::util::Bytes;
use ntex::web::{
    types::State,
    HttpRequest, HttpResponse,
};
use serde::Deserialize;
use serde_json::json;
use url::Url;
use zeroship_core::auth::hmac_sha256;
use zeroship_mailer::templates::{
    build_email, EmailChangeHtml, EmailChangeText, InviteHtml, InviteText, MagicLinkHtml,
    MagicLinkText, PasswordResetHtml, PasswordResetText, ReauthenticationHtml,
    ReauthenticationText, VerifyEmailHtml, VerifyEmailText,
};
use zeroship_mailer::{Address, Email, Mailer, MailerError};

use crate::config::AuthConfig;
use crate::csrf;

const WEBHOOK_ID: HeaderName = HeaderName::from_static("webhook-id");
const WEBHOOK_TIMESTAMP: HeaderName = HeaderName::from_static("webhook-timestamp");
const WEBHOOK_SIGNATURE: HeaderName = HeaderName::from_static("webhook-signature");
const SIGNATURE_TOLERANCE_SECS: i64 = 5 * 60;

/// `POST /hooks/gotrue/send-email`.
//
// ntex's per-thread service futures are intentionally `!Send`.
#[allow(clippy::future_not_send)]
pub async fn send_email(
    req: HttpRequest,
    body: Bytes,
    cfg: State<Arc<AuthConfig>>,
    db: State<Arc<compio_postgres::Client>>,
    mailer: State<Arc<dyn Mailer>>,
) -> HttpResponse {
    let Some(secret) = cfg
        .gotrue_email_hook_secret
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        tracing::warn!("gotrue send-email hook hit but AUTH_GOTRUE_EMAIL_HOOK_SECRET is unset");
        return error_response(StatusCode::UNAUTHORIZED, "send-email hook is disabled");
    };

    if let Err(e) = verify_standard_webhook(
        req.headers(),
        &body,
        secret,
        now_unix_secs(),
        SIGNATURE_TOLERANCE_SECS,
    ) {
        tracing::warn!(error = %e, "gotrue send-email hook signature rejected");
        return error_response(StatusCode::UNAUTHORIZED, "invalid webhook signature");
    }

    let payload: GoTrueSendEmailPayload = match serde_json::from_slice(&body) {
        Ok(payload) => payload,
        Err(e) => {
            tracing::warn!(error = %e, "gotrue send-email hook payload parse failed");
            return error_response(StatusCode::BAD_REQUEST, "invalid send-email payload");
        }
    };

    let msg = match build_gotrue_email(&payload, cfg.as_ref()) {
        Ok(msg) => msg,
        Err(e) => {
            tracing::warn!(error = %e, "gotrue send-email hook payload could not be mapped");
            return error_response(StatusCode::BAD_REQUEST, e.public_message());
        }
    };

    match mailer.send(db.as_ref(), msg).await {
        Ok(_) => HttpResponse::Ok().json(&json!({})),
        Err(MailerError::Suppressed(email)) => {
            tracing::info!(
                email_domain = %email_domain(&email),
                "gotrue send-email suppressed recipient accepted"
            );
            HttpResponse::Ok().json(&json!({}))
        }
        Err(MailerError::Transport(e)) => {
            tracing::error!(error = %e, "gotrue send-email transport failed");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "could not send email")
        }
        Err(MailerError::Config(e)) => {
            tracing::error!(error = %e, "gotrue send-email config failed");
            error_response(StatusCode::INTERNAL_SERVER_ERROR, "could not send email")
        }
    }
}

#[derive(Debug, Deserialize)]
struct GoTrueSendEmailPayload {
    user: GoTrueUser,
    email_data: GoTrueEmailData,
}

#[derive(Debug, Deserialize)]
struct GoTrueUser {
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    new_email: Option<String>,
    #[serde(default)]
    user_metadata: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct GoTrueEmailData {
    #[serde(default)]
    token: String,
    #[serde(default)]
    token_hash: String,
    #[serde(default)]
    redirect_to: String,
    #[serde(default)]
    email_action_type: String,
    #[serde(default)]
    site_url: String,
    #[serde(default)]
    token_hash_new: String,
}

#[derive(Debug)]
enum SignatureError {
    MissingHeader(&'static str),
    InvalidTimestamp,
    StaleTimestamp,
    InvalidSecret,
    InvalidSignature,
}

impl std::fmt::Display for SignatureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingHeader(name) => write!(f, "missing {name}"),
            Self::InvalidTimestamp => write!(f, "invalid webhook timestamp"),
            Self::StaleTimestamp => write!(f, "stale webhook timestamp"),
            Self::InvalidSecret => write!(f, "invalid webhook secret"),
            Self::InvalidSignature => write!(f, "signature mismatch"),
        }
    }
}

#[derive(Debug)]
enum BuildEmailError {
    MissingRecipient(&'static str),
    MissingTokenHash(&'static str),
    MissingToken(&'static str),
    MissingVerifyBase,
    InvalidVerifyBase(String),
    Render(String),
    UnknownAction(String),
}

impl BuildEmailError {
    fn public_message(&self) -> &'static str {
        match self {
            Self::UnknownAction(_) => "unsupported email_action_type",
            _ => "invalid send-email payload",
        }
    }
}

impl std::fmt::Display for BuildEmailError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingRecipient(field) => write!(f, "missing recipient field {field}"),
            Self::MissingTokenHash(field) => write!(f, "missing token hash field {field}"),
            Self::MissingToken(field) => write!(f, "missing token field {field}"),
            Self::MissingVerifyBase => write!(f, "missing site_url and SUPABASE_URL"),
            Self::InvalidVerifyBase(e) => write!(f, "invalid verify base: {e}"),
            Self::Render(e) => write!(f, "template render failed: {e}"),
            Self::UnknownAction(action) => write!(f, "unknown email_action_type {action:?}"),
        }
    }
}

fn verify_standard_webhook(
    headers: &HeaderMap,
    raw_body: &[u8],
    secret: &str,
    now_secs: i64,
    tolerance_secs: i64,
) -> Result<(), SignatureError> {
    let id = required_header(headers, &WEBHOOK_ID, "webhook-id")?;
    let timestamp = required_header(headers, &WEBHOOK_TIMESTAMP, "webhook-timestamp")?;
    let signature = required_header(headers, &WEBHOOK_SIGNATURE, "webhook-signature")?;
    let ts: i64 = timestamp
        .parse()
        .map_err(|_| SignatureError::InvalidTimestamp)?;
    if now_secs.abs_diff(ts) > tolerance_secs.unsigned_abs() {
        return Err(SignatureError::StaleTimestamp);
    }

    let key = decode_standard_webhook_secret(secret)?;
    let mut signed = Vec::with_capacity(id.len() + timestamp.len() + raw_body.len() + 2);
    signed.extend_from_slice(id.as_bytes());
    signed.push(b'.');
    signed.extend_from_slice(timestamp.as_bytes());
    signed.push(b'.');
    signed.extend_from_slice(raw_body);
    let expected = STANDARD.encode(hmac_sha256(&key, &signed));

    for candidate in signature.split_whitespace() {
        let candidate = candidate.trim().trim_end_matches(',');
        let Some(provided) = candidate.strip_prefix("v1,") else {
            continue;
        };
        if csrf::matches(provided, &expected) {
            return Ok(());
        }
    }
    Err(SignatureError::InvalidSignature)
}

fn required_header<'a>(
    headers: &'a HeaderMap,
    name: &HeaderName,
    display: &'static str,
) -> Result<&'a str, SignatureError> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .ok_or(SignatureError::MissingHeader(display))
}

fn decode_standard_webhook_secret(secret: &str) -> Result<Vec<u8>, SignatureError> {
    let secret = secret.trim();
    let Some(rest) = secret.strip_prefix("v1,") else {
        return Err(SignatureError::InvalidSecret);
    };
    let Some(b64) = rest.strip_prefix("whsec_") else {
        return Err(SignatureError::InvalidSecret);
    };
    let key = STANDARD
        .decode(b64)
        .map_err(|_| SignatureError::InvalidSecret)?;
    if key.len() < 32 {
        return Err(SignatureError::InvalidSecret);
    }
    Ok(key)
}

fn build_gotrue_email(
    payload: &GoTrueSendEmailPayload,
    cfg: &AuthConfig,
) -> Result<Email, BuildEmailError> {
    let action = payload.email_data.email_action_type.trim();
    let name = name_hint(&payload.user);
    let from = Address {
        email: cfg.mail_from_email.clone(),
        name: Some(cfg.mail_from_name.clone()),
    };

    match action {
        "signup" | "email" => {
            let to = recipient(&payload.user, false)?;
            let link = verify_link(&payload.email_data, cfg, &payload.email_data.token_hash)?;
            let html = VerifyEmailHtml {
                name: &name,
                link: &link,
                expires_in: "24 hours",
            }
            .render()
            .map_err(|e| BuildEmailError::Render(e.to_string()))?;
            let text = VerifyEmailText {
                name: &name,
                link: &link,
                expires_in: "24 hours",
            }
            .render()
            .map_err(|e| BuildEmailError::Render(e.to_string()))?;
            Ok(build_email(
                to,
                from,
                "Verify your zeroship email".into(),
                text,
                html,
                vec!["gotrue".into(), action.to_string()],
            ))
        }
        "magiclink" => {
            let to = recipient(&payload.user, false)?;
            let link = verify_link(&payload.email_data, cfg, &payload.email_data.token_hash)?;
            let html = MagicLinkHtml {
                name: &name,
                link: &link,
                expires_in: "15 minutes",
                requesting_device: "Unknown device",
                requesting_location: "Unknown location",
            }
            .render()
            .map_err(|e| BuildEmailError::Render(e.to_string()))?;
            let text = MagicLinkText {
                name: &name,
                link: &link,
                expires_in: "15 minutes",
                requesting_device: "Unknown device",
                requesting_location: "Unknown location",
            }
            .render()
            .map_err(|e| BuildEmailError::Render(e.to_string()))?;
            Ok(build_email(
                to,
                from,
                "Sign in to zeroship".into(),
                text,
                html,
                vec!["gotrue".into(), "magiclink".into()],
            ))
        }
        "recovery" => {
            let to = recipient(&payload.user, false)?;
            let link = verify_link(&payload.email_data, cfg, &payload.email_data.token_hash)?;
            let html = PasswordResetHtml {
                name: &name,
                link: &link,
                expires_in: "1 hour",
            }
            .render()
            .map_err(|e| BuildEmailError::Render(e.to_string()))?;
            let text = PasswordResetText {
                name: &name,
                link: &link,
                expires_in: "1 hour",
            }
            .render()
            .map_err(|e| BuildEmailError::Render(e.to_string()))?;
            Ok(build_email(
                to,
                from,
                "Reset your zeroship password".into(),
                text,
                html,
                vec!["gotrue".into(), "recovery".into()],
            ))
        }
        "invite" => {
            let to = recipient(&payload.user, false)?;
            let link = verify_link(&payload.email_data, cfg, &payload.email_data.token_hash)?;
            let html = InviteHtml {
                name: &name,
                link: &link,
                expires_in: "24 hours",
            }
            .render()
            .map_err(|e| BuildEmailError::Render(e.to_string()))?;
            let text = InviteText {
                name: &name,
                link: &link,
                expires_in: "24 hours",
            }
            .render()
            .map_err(|e| BuildEmailError::Render(e.to_string()))?;
            Ok(build_email(
                to,
                from,
                "Accept your zeroship invitation".into(),
                text,
                html,
                vec!["gotrue".into(), "invite".into()],
            ))
        }
        "email_change" => {
            let to = recipient(&payload.user, true)?;
            let link = verify_link(
                &payload.email_data,
                cfg,
                &payload.email_data.token_hash_new,
            )?;
            let html = EmailChangeHtml {
                name: &name,
                new_email: &to.email,
                link: &link,
                expires_in: "24 hours",
            }
            .render()
            .map_err(|e| BuildEmailError::Render(e.to_string()))?;
            let text = EmailChangeText {
                name: &name,
                new_email: &to.email,
                link: &link,
                expires_in: "24 hours",
            }
            .render()
            .map_err(|e| BuildEmailError::Render(e.to_string()))?;
            Ok(build_email(
                to,
                from,
                "Confirm your zeroship email change".into(),
                text,
                html,
                vec!["gotrue".into(), "email-change".into()],
            ))
        }
        "reauthentication" => {
            let to = recipient(&payload.user, false)?;
            let token = non_empty(&payload.email_data.token)
                .ok_or(BuildEmailError::MissingToken("email_data.token"))?;
            let html = ReauthenticationHtml {
                name: &name,
                token,
                expires_in: "10 minutes",
            }
            .render()
            .map_err(|e| BuildEmailError::Render(e.to_string()))?;
            let text = ReauthenticationText {
                name: &name,
                token,
                expires_in: "10 minutes",
            }
            .render()
            .map_err(|e| BuildEmailError::Render(e.to_string()))?;
            Ok(build_email(
                to,
                from,
                "Confirm your zeroship sign-in".into(),
                text,
                html,
                vec!["gotrue".into(), "reauthentication".into()],
            ))
        }
        other => Err(BuildEmailError::UnknownAction(other.to_string())),
    }
}

fn recipient(user: &GoTrueUser, email_change: bool) -> Result<Address, BuildEmailError> {
    let field = if email_change {
        "user.new_email"
    } else {
        "user.email"
    };
    let email = if email_change {
        user.new_email.as_deref()
    } else {
        user.email.as_deref()
    };
    let email = email
        .and_then(non_empty)
        .ok_or(BuildEmailError::MissingRecipient(field))?;
    Ok(Address {
        email: email.to_ascii_lowercase(),
        name: Some(name_hint(user)),
    })
}

fn verify_link(
    data: &GoTrueEmailData,
    cfg: &AuthConfig,
    token_hash: &str,
) -> Result<String, BuildEmailError> {
    let token_hash = non_empty(token_hash)
        .ok_or(BuildEmailError::MissingTokenHash("email_data.token_hash"))?;
    let base = verify_base(&data.site_url, cfg.supabase_url())?;
    let raw = format!("{}/verify", base.trim_end_matches('/'));
    let mut url = Url::parse(&raw)
        .map_err(|e| BuildEmailError::InvalidVerifyBase(e.to_string()))?;
    url.query_pairs_mut()
        .append_pair("token", token_hash)
        .append_pair("type", data.email_action_type.trim())
        .append_pair("redirect_to", &data.redirect_to);
    Ok(url.to_string())
}

fn verify_base(site_url: &str, supabase_url: Option<&str>) -> Result<String, BuildEmailError> {
    let site_url = site_url.trim().trim_end_matches('/');
    if !site_url.is_empty() && path_contains_auth_v1(site_url) {
        return Ok(site_url.to_string());
    }
    if let Some(supabase_url) = supabase_url.and_then(non_empty) {
        return Ok(ensure_auth_v1_base(supabase_url));
    }
    if !site_url.is_empty() {
        return Ok(site_url.to_string());
    }
    Err(BuildEmailError::MissingVerifyBase)
}

fn path_contains_auth_v1(raw: &str) -> bool {
    Url::parse(raw).ok().is_some_and(|u| {
        let path = u.path().trim_end_matches('/');
        path == "/auth/v1" || path.contains("/auth/v1/")
    })
}

fn ensure_auth_v1_base(raw: &str) -> String {
    let raw = raw.trim().trim_end_matches('/');
    if path_contains_auth_v1(raw) {
        raw.to_string()
    } else {
        format!("{raw}/auth/v1")
    }
}

fn name_hint(user: &GoTrueUser) -> String {
    for key in ["name", "full_name"] {
        if let Some(value) = user
            .user_metadata
            .get(key)
            .and_then(serde_json::Value::as_str)
            .and_then(non_empty)
        {
            return value
                .split_whitespace()
                .next()
                .unwrap_or("there")
                .to_string();
        }
    }
    "there".to_string()
}

fn non_empty(value: &str) -> Option<&str> {
    let value = value.trim();
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

fn error_response(status: StatusCode, message: &str) -> HttpResponse {
    HttpResponse::build(status).json(&json!({
        "error": {
            "http_code": status.as_u16(),
            "message": message,
        }
    }))
}

fn now_unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

fn email_domain(email: &str) -> &str {
    email.split('@').nth(1).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    use clap::Parser;
    use compio_postgres::{connect, NoTls};
    use ntex::http::header::HeaderValue;
    use ntex::web::{self, test};
    use serde_json::json;
    use uuid::Uuid;
    use zeroship_mailer::{check_suppression, suppressions, MessageId};

    use super::*;

    const TEST_NOW: i64 = 1_735_689_600;

    #[derive(Debug, Default)]
    struct SuppressionAwareCountingMailer {
        transports: AtomicUsize,
    }

    impl SuppressionAwareCountingMailer {
        fn transports(&self) -> usize {
            self.transports.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl Mailer for SuppressionAwareCountingMailer {
        async fn send(
            &self,
            db: &compio_postgres::Client,
            msg: Email,
        ) -> Result<MessageId, MailerError> {
            check_suppression(db, &msg.to.email).await?;
            let n = self.transports.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(MessageId(format!("test-message-{n}")))
        }
    }

    fn test_secret() -> String {
        format!("v1,whsec_{}", STANDARD.encode([7u8; 32]))
    }

    fn sign(secret: &str, id: &str, timestamp: i64, raw_body: &[u8]) -> String {
        let key = decode_standard_webhook_secret(secret).expect("decode secret");
        let mut signed = Vec::new();
        signed.extend_from_slice(id.as_bytes());
        signed.push(b'.');
        signed.extend_from_slice(timestamp.to_string().as_bytes());
        signed.push(b'.');
        signed.extend_from_slice(raw_body);
        format!("v1,{}", STANDARD.encode(hmac_sha256(&key, &signed)))
    }

    fn signed_headers(secret: &str, raw_body: &[u8], timestamp: i64) -> HeaderMap {
        let id = "msg_test_123";
        let sig = sign(secret, id, timestamp, raw_body);
        let mut headers = HeaderMap::new();
        headers.insert(WEBHOOK_ID, HeaderValue::from_static(id));
        headers.insert(
            WEBHOOK_TIMESTAMP,
            HeaderValue::from_str(&timestamp.to_string()).expect("timestamp header"),
        );
        headers.insert(
            WEBHOOK_SIGNATURE,
            HeaderValue::from_str(&sig).expect("signature header"),
        );
        headers
    }

    fn cfg() -> AuthConfig {
        let mut cfg = AuthConfig::parse_from([
            "zeroship-auth",
            "--db-url",
            "postgres://test",
            "--dev-insecure",
            "--stash-signing-key",
            "test-stash-key-not-for-prod-32bytes!",
            "--auth-provider",
            "supabase",
            "--supabase-url",
            "http://localhost:54321",
            "--supabase-anon-key",
            "anon-test",
            "--mail-from-email",
            "auth@zeroship.test",
            "--mail-from-name",
            "zeroship test",
            "--gotrue-email-hook-secret",
            &test_secret(),
        ]);
        cfg.resolve(zeroship_core::config::AuthSection::default());
        cfg
    }

    fn payload(action: &str) -> GoTrueSendEmailPayload {
        serde_json::from_value(json!({
            "user": {
                "email": "User@Example.COM",
                "new_email": "New@Example.COM",
                "user_metadata": { "name": "Alice Example" }
            },
            "email_data": {
                "token": "305805",
                "token_hash": format!("hash_{action}"),
                "redirect_to": "https://app.example.com/welcome",
                "email_action_type": action,
                "site_url": "https://app.example.com",
                "token_hash_new": "hash_new_email_change"
            }
        }))
        .expect("payload")
    }

    #[test]
    fn signature_verify_accepts_correct_standard_webhook_signature() {
        let body = br#"{"ok":true}"#;
        let secret = test_secret();
        let headers = signed_headers(&secret, body, TEST_NOW);

        verify_standard_webhook(&headers, body, &secret, TEST_NOW, SIGNATURE_TOLERANCE_SECS)
            .expect("valid signature");
    }

    #[test]
    fn signature_verify_rejects_wrong_secret() {
        let body = br#"{"ok":true}"#;
        let secret = test_secret();
        let other = format!("v1,whsec_{}", STANDARD.encode([8u8; 32]));
        let headers = signed_headers(&secret, body, TEST_NOW);

        assert!(matches!(
            verify_standard_webhook(&headers, body, &other, TEST_NOW, SIGNATURE_TOLERANCE_SECS),
            Err(SignatureError::InvalidSignature)
        ));
    }

    #[test]
    fn signature_verify_rejects_tampered_body() {
        let body = br#"{"ok":true}"#;
        let secret = test_secret();
        let headers = signed_headers(&secret, body, TEST_NOW);

        assert!(matches!(
            verify_standard_webhook(
                &headers,
                br#"{"ok":false}"#,
                &secret,
                TEST_NOW,
                SIGNATURE_TOLERANCE_SECS
            ),
            Err(SignatureError::InvalidSignature)
        ));
    }

    #[test]
    fn signature_verify_rejects_stale_timestamp() {
        let body = br#"{"ok":true}"#;
        let secret = test_secret();
        let headers = signed_headers(&secret, body, TEST_NOW - SIGNATURE_TOLERANCE_SECS - 1);

        assert!(matches!(
            verify_standard_webhook(&headers, body, &secret, TEST_NOW, SIGNATURE_TOLERANCE_SECS),
            Err(SignatureError::StaleTimestamp)
        ));
    }

    #[test]
    fn signature_verify_rejects_missing_headers() {
        let headers = HeaderMap::new();

        assert!(matches!(
            verify_standard_webhook(&headers, b"{}", &test_secret(), TEST_NOW, SIGNATURE_TOLERANCE_SECS),
            Err(SignatureError::MissingHeader("webhook-id"))
        ));
    }

    #[test]
    fn payload_mapping_builds_expected_emails_for_each_action_type() {
        let cfg = cfg();
        for action in ["signup", "email", "magiclink", "recovery", "invite"] {
            let p = payload(action);
            let email = build_gotrue_email(&p, &cfg).expect("build email");
            assert_eq!(email.to.email, "user@example.com", "{action}");
            assert!(!email.subject.is_empty(), "{action}");
            let html = email.html.as_deref().expect("html body");
            let token = format!("hash_{action}");
            assert!(html.contains(&format!("token={token}")), "{action}: {html}");
            assert!(email.text.contains(&format!("token={token}")), "{action}: {}", email.text);
            assert!(html.contains(&format!("type={action}")), "{action}: {html}");
            assert!(email.text.contains(&format!("type={action}")), "{action}: {}", email.text);
            assert!(html.contains("redirect_to=https%3A%2F%2Fapp.example.com%2Fwelcome"), "{action}: {html}");
            assert!(!html.contains("305805"), "{action}: verify link must use token_hash, not OTP");
        }
    }

    #[test]
    fn payload_mapping_email_change_uses_new_hash_and_new_email() {
        let cfg = cfg();
        let email = build_gotrue_email(&payload("email_change"), &cfg).expect("build email");
        assert_eq!(email.to.email, "new@example.com");
        assert!(!email.subject.is_empty());
        let html = email.html.as_deref().expect("html body");
        assert!(html.contains("token=hash_new_email_change"), "{html}");
        assert!(email.text.contains("token=hash_new_email_change"), "{}", email.text);
        assert!(html.contains("new@example.com"), "{html}");
        assert!(!html.contains("hash_email_change"), "{html}");
    }

    #[test]
    fn payload_mapping_reauthentication_carries_otp_not_link() {
        let cfg = cfg();
        let email = build_gotrue_email(&payload("reauthentication"), &cfg).expect("build email");
        assert_eq!(email.to.email, "user@example.com");
        assert!(!email.subject.is_empty());
        let html = email.html.as_deref().expect("html body");
        assert!(html.contains("305805"), "{html}");
        assert!(email.text.contains("305805"), "{}", email.text);
        assert!(!html.contains("/verify?"), "{html}");
        assert!(!email.text.contains("/verify?"), "{}", email.text);
    }

    #[test]
    fn verify_link_prefers_configured_supabase_auth_v1_base() {
        let cfg = cfg();
        let email = build_gotrue_email(&payload("signup"), &cfg).expect("build email");
        let html = email.html.as_deref().expect("html body");
        assert!(html.contains("http://localhost:54321/auth/v1/verify"), "{html}");
    }

    #[ntex::test]
    async fn suppressed_recipient_returns_200_and_does_not_transport() {
        let Ok(dsn) = std::env::var("AUTH_DB_URL") else {
            eprintln!("skip suppressed_recipient_returns_200_and_does_not_transport (no AUTH_DB_URL)");
            return;
        };
        let (client, connection) = connect(&dsn, NoTls).await.expect("connect");
        compio::runtime::spawn(async move {
            if let Err(e) = connection.run().await {
                eprintln!("gotrue_email_hook_test pg connection error: {e}");
            }
        })
        .detach();
        let pg = Arc::new(client);
        let email = format!("suppressed-{}@zeroship.test", Uuid::new_v4().simple());
        suppressions::add(pg.as_ref(), &email, "test_suppression", None)
            .await
            .expect("add suppression");

        let mut cfg = cfg();
        cfg.gotrue_email_hook_secret = Some(test_secret());
        let cfg = Arc::new(cfg);
        let mailer = Arc::new(SuppressionAwareCountingMailer::default());
        let mailer_state: Arc<dyn Mailer> = mailer.clone();
        let app = test::init_service(
            web::App::new()
                .state(cfg.clone())
                .state(pg.clone())
                .state(mailer_state)
                .service(
                    web::resource("/hooks/gotrue/send-email")
                        .route(web::post().to(send_email)),
                ),
        )
        .await;

        let body = serde_json::to_vec(&json!({
            "user": {
                "email": email,
                "user_metadata": { "name": "Suppressed User" }
            },
            "email_data": {
                "token": "111111",
                "token_hash": "hash_suppressed",
                "redirect_to": "https://app.example.com",
                "email_action_type": "signup",
                "site_url": "https://app.example.com",
                "token_new": "",
                "token_hash_new": ""
            }
        }))
        .expect("body");
        let ts = now_unix_secs();
        let sig = sign(
            cfg.gotrue_email_hook_secret.as_deref().expect("secret"),
            "msg_suppressed",
            ts,
            &body,
        );
        let resp = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/hooks/gotrue/send-email")
                .header("content-type", "application/json")
                .header("webhook-id", "msg_suppressed")
                .header("webhook-timestamp", ts.to_string())
                .header("webhook-signature", sig)
                .set_payload(body)
                .to_request(),
        )
        .await;
        assert_eq!(resp.status().as_u16(), 200);
        assert_eq!(mailer.transports(), 0, "suppression must stop transport");

        pg.execute(
            "DELETE FROM zeroship.email_suppressions WHERE email = $1::citext",
            &[&email],
        )
        .await
        .ok();
    }
}
