//! Magic-link UI handlers.
//!
//! Routes (registered in `server::configure`):
//!
//! - **POST `/magic/start`** — accept an email + `login_challenge`, issue a
//!   magic-link token, send the email, set `__Host-zsidp_magic_csrf` at
//!   the requesting device, render the "check your email" page (with a
//!   hidden code-entry form for cross-device completion). Always returns
//!   200, regardless of whether the address exists or is rate-limited
//!   (enumeration defense).
//!
//! - **GET `/magic/await`** — alternate landing for the cross-device
//!   code-entry form. The check-email page already embeds the same form
//!   inside a `<details>` toggle; this route exists for direct entry.
//!
//! `verify` (GET) + `complete` (POST) land in P5-U4.3.

use askama::Template;
use ntex::http::header::{HeaderValue, COOKIE, SET_COOKIE, USER_AGENT};
use ntex::web::{HttpRequest, HttpResponse};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use url::form_urlencoded;

use crate::audit::{self, AuditEvent};
use crate::config::AuthConfig;
use crate::csrf;
use crate::identity::magic_link;
use crate::mailer::templates::{build_email, MagicLinkHtml, MagicLinkText};
use crate::mailer::{Address, Mailer};
use crate::ratelimit::{self, Bucket};
use crate::ui::{ErrorPage, MagicAwaitCodePage, MagicCheckEmailPage};

// ─── Cookie helpers ──────────────────────────────────────────────────

/// Name of the cookie set at the requesting device when a magic-link is
/// issued. Match against the magic-link row's `csrf_nonce` to decide
/// same-device vs cross-device on redeem (P5-U4.3).
pub const MAGIC_CSRF_COOKIE: &str = "__Host-zsidp_magic_csrf";

/// Build the `Set-Cookie` header value for the magic-link CSRF cookie.
///
/// 15-minute Max-Age matches the magic-link token's TTL. `SameSite=Lax`
/// so the email-link click (which is a cross-site GET back to
/// `auth.zeroship.ai`) still presents the cookie. `HttpOnly` because no
/// JS needs to read it — only the server consults it on
/// `/magic/verify`. `Path=/` because the cookie must be present on the
/// `/magic/verify` and `/magic/complete` paths alike.
pub(crate) fn magic_csrf_set_cookie(nonce: &str, insecure_dev: bool) -> String {
    let secure = if insecure_dev { "" } else { "; Secure" };
    format!(
        "{MAGIC_CSRF_COOKIE}={nonce}; Path=/; HttpOnly; SameSite=Lax{secure}; Max-Age=900"
    )
}

/// Parse the magic-link CSRF nonce from a request's `Cookie` header
/// value.
///
/// Used by `/magic/verify` (P5-U4.3) to decide same-device vs
/// cross-device on redeem.
#[allow(dead_code)] // used by U4.3 `/magic/verify`
pub(crate) fn parse_magic_csrf_cookie(cookie_header: &str) -> Option<String> {
    for part in cookie_header.split(';') {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix(&format!("{MAGIC_CSRF_COOKIE}=")) {
            return Some(rest.to_string());
        }
    }
    None
}

// ─── POST /magic/start ───────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct MagicStartForm {
    pub csrf: String,
    pub email: String,
    pub login_challenge: String,
}

/// Issue a magic-link token + send the email + render "check your email".
///
/// Enumeration-resistant: rate-limit failures and missing-user paths
/// still return the same 200 + page so an attacker can't probe which
/// addresses have accounts.
///
/// CSRF: form `csrf` value must match the `__Host-zsidp_csrf` cookie
/// (the existing double-submit cookie shared with `/login`/`/signup`).
#[allow(clippy::future_not_send, clippy::too_many_lines)]
pub async fn start(
    req: HttpRequest,
    form: ntex::web::types::Form<MagicStartForm>,
    cfg: ntex::web::types::State<Arc<AuthConfig>>,
    db: ntex::web::types::State<Arc<compio_postgres::Client>>,
    mailer: ntex::web::types::State<Arc<dyn Mailer>>,
) -> HttpResponse {
    // 1. CSRF — cheapest check first.
    let cookie_header = req
        .headers()
        .get(COOKIE)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let cookie_token = csrf::parse_cookie(cookie_header);
    if cookie_token
        .as_deref()
        .is_none_or(|c| !csrf::matches(&form.csrf, c))
    {
        return render_error_page("invalid request", Some("csrf"));
    }

    let email_norm = form.email.trim().to_ascii_lowercase();
    let login_challenge = form.login_challenge.clone();

    // 2. Rate-limit per-email + per-IP. On throttle, render the
    //    check-email page anyway so the attacker can't distinguish.
    let ip = req
        .connection_info()
        .remote()
        .unwrap_or("0.0.0.0")
        .to_string();
    let buckets = [
        (format!("magic:email:{email_norm}"), Bucket::LOGIN_EMAIL),
        (format!("magic:ip:{ip}"), Bucket::LOGIN_IP),
    ];
    let mut throttled = false;
    for (key, bucket) in &buckets {
        match ratelimit::consume(db.as_ref(), key, *bucket).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => {
                throttled = true;
                audit::emit(
                    db.as_ref(),
                    &AuditEvent {
                        event_type: "magic_issued",
                        outcome: "failure",
                        auth_method: Some("magic"),
                        detail: json!({ "reason": "rate_limited", "bucket": key }),
                        ..Default::default()
                    },
                )
                .await;
                break;
            }
            Err(e) => {
                tracing::error!(error = %e, bucket = %key, "magic rate-limit consume failed");
                return render_error_page("internal error", Some("rate limit"));
            }
        }
    }

    // 3. Issue the token (skipped on throttle — but still render the
    //    same page so the attacker can't tell).
    let issued = if throttled {
        None
    } else {
        match magic_link::issue(db.as_ref(), &email_norm, "login").await {
            Ok(i) => Some(i),
            Err(e) => {
                tracing::error!(error = %e, "magic_link::issue failed");
                return render_error_page("internal error", Some("issue"));
            }
        }
    };

    // 4. Send the email. Errors are logged but never surfaced (enumeration
    //    defense — a suppressed/bouncing address must look identical to a
    //    deliverable one).
    if let Some(ref issued) = issued {
        let lc_escaped: String =
            form_urlencoded::byte_serialize(login_challenge.as_bytes()).collect();
        let link = format!(
            "{}/magic/verify?t={}&login_challenge={}",
            cfg.public_url(),
            issued.raw,
            lc_escaped,
        );
        let name_hint = email_norm.split('@').next().unwrap_or("there").to_string();
        let device_str = user_agent_str(req.headers().get(USER_AGENT));

        let html = MagicLinkHtml {
            name: &name_hint,
            link: &link,
            expires_in: "15 minutes",
            requesting_device: &device_str,
            requesting_location: "Unknown",
        }
        .render()
        .unwrap_or_default();
        let text = MagicLinkText {
            name: &name_hint,
            link: &link,
            expires_in: "15 minutes",
            requesting_device: &device_str,
            requesting_location: "Unknown",
        }
        .render()
        .unwrap_or_default();

        let email_msg = build_email(
            Address {
                email: email_norm.clone(),
                name: None,
            },
            Address {
                email: cfg.mail_from_email.clone(),
                name: Some(cfg.mail_from_name.clone()),
            },
            "Sign in to zeroship".into(),
            text,
            html,
            vec!["magic-link".into(), "login".into()],
        );
        if let Err(e) = mailer.send(db.as_ref(), email_msg).await {
            tracing::warn!(error = %e, email = %email_norm, "magic_link email send failed");
        }

        audit::emit(
            db.as_ref(),
            &AuditEvent {
                event_type: "magic_issued",
                outcome: "success",
                auth_method: Some("magic"),
                detail: json!({
                    "email_domain": email_norm.split('@').nth(1).unwrap_or("")
                }),
                ..Default::default()
            },
        )
        .await;
    }

    // 5. Render the check-email page. Fresh CSRF cookie for the
    //    cross-device `/magic/complete` POST.
    let new_csrf = csrf::generate_token();
    // `csrf_nonce` rendered into the hidden form field MUST be the
    // magic-link's nonce, because `/magic/complete` will look the row up
    // by it (U4.3). When throttled we use a random placeholder — the
    // form will not match anything, which is the intended behaviour (we
    // silently dropped the request).
    let nonce = issued
        .as_ref()
        .map_or_else(|| "throttled-no-token".into(), |i| i.csrf_nonce.clone());

    let page = MagicCheckEmailPage {
        csrf: &new_csrf,
        login_challenge: &login_challenge,
        csrf_nonce: &nonce,
        email: &email_norm,
    };
    let body = page
        .render()
        .unwrap_or_else(|_| "<h1>Check your email</h1>".to_string());

    let mut resp = HttpResponse::Ok();
    resp.content_type("text/html; charset=utf-8");
    resp.header(SET_COOKIE, csrf::set_cookie(&new_csrf, cfg.insecure_dev));
    // The requesting-device cookie: only set if we actually issued.
    if let Some(ref i) = issued {
        resp.header(
            SET_COOKIE,
            magic_csrf_set_cookie(&i.csrf_nonce, cfg.insecure_dev),
        );
    }
    resp.body(body)
}

// ─── GET /magic/await ────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct MagicAwaitQuery {
    pub login_challenge: String,
    pub csrf_nonce: String,
    pub email: String,
}

/// `/magic/await` GET — surfaces just the code-entry form.
///
/// (The "I opened the link on another device" variant of the
/// check-email page.) The check-email page already embeds the same
/// form inside a `<details>`; this route is for explicit deep-link
/// entry.
#[allow(clippy::future_not_send, clippy::unused_async)]
pub async fn await_code(
    query: ntex::web::types::Query<MagicAwaitQuery>,
    cfg: ntex::web::types::State<Arc<AuthConfig>>,
) -> HttpResponse {
    let new_csrf = csrf::generate_token();
    let page = MagicAwaitCodePage {
        csrf: &new_csrf,
        login_challenge: &query.login_challenge,
        csrf_nonce: &query.csrf_nonce,
        email: &query.email,
    };
    let body = page
        .render()
        .unwrap_or_else(|_| "<h1>Enter your code</h1>".to_string());
    let mut resp = HttpResponse::Ok();
    resp.content_type("text/html; charset=utf-8");
    resp.header(SET_COOKIE, csrf::set_cookie(&new_csrf, cfg.insecure_dev));
    resp.body(body)
}

// ─── Helpers ─────────────────────────────────────────────────────────

fn render_error_page(error: &str, error_description: Option<&str>) -> HttpResponse {
    let page = ErrorPage {
        error,
        error_description,
    };
    let body = page
        .render()
        .unwrap_or_else(|_| format!("<h1>{error}</h1>"));
    let mut resp = HttpResponse::Ok();
    resp.content_type("text/html; charset=utf-8");
    resp.body(body)
}

/// Best-effort device label extracted from a `User-Agent` header. We
/// don't ship a full UA parser; the value is purely for the email body
/// ("Chrome on macOS just requested a sign-in link …"). Falls back to
/// "Unknown device" when the header is missing or malformed.
fn user_agent_str(h: Option<&HeaderValue>) -> String {
    h.and_then(|v| v.to_str().ok())
        .map_or_else(|| "Unknown device".to_string(), str::to_string)
}
