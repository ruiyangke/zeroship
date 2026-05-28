//! `/oauth/github/start` + `/oauth/github/callback` — GitHub OAuth 2.0
//! federation.
//!
//! Mirrors `oauth_google` except for the OIDC-specific bits:
//!
//!   - **No `nonce`** — GitHub is OAuth 2.0, not OIDC. There's no
//!     ID token to bind a nonce against, so the stash field carries an
//!     empty string.
//!   - **No `JwksCache`** — `github::complete_callback` doesn't verify
//!     any JWT signature; it fetches `/user` + `/user/emails` against
//!     GitHub's REST API using the access token.
//!   - **Provider trust** — `provider_trusted_for_email` is always
//!     `true` because the email-picker in
//!     [`crate::identity::oauth::github`] only returns a `primary &&
//!     verified` address. There is no Workspace-domain analogue;
//!     GitHub's signal IS the verified flag on the email row.
//!   - **Cookie name** — `__Host-zsidp_github_stash` so a concurrent
//!     Google dance in another tab doesn't clobber it.
//!
//! Route gating: registered only when `cfg.github_client_id.is_some()`
//! (see [`crate::server::configure`]).

use std::sync::Arc;

use askama::Template;
use ntex::http::header::{HeaderValue, COOKIE, LOCATION, SET_COOKIE};
use ntex::web::{HttpRequest, HttpResponse};
use serde::Deserialize;
use serde_json::json;

use crate::audit::{self, AuditEvent};
use crate::config::AuthConfig;
use crate::hydra_client::types::AcceptLoginRequest;
use crate::hydra_client::HydraAdmin;
use crate::identity::eligibility;
use crate::identity::linker::{self, LinkOutcome, ResolvedProfile};
use crate::identity::oauth::github::{self, GitHubIdentity};
use crate::sessions::login as session_cookie;
use crate::store::{sessions, users};
use crate::ui::oauth_stash::{
    clear_stash_cookie, github_stash_cookie_name, parse_stash_cookie, set_stash_cookie, OAuthStash,
};
use crate::ui::{ErrorPage, PublicErrorMessage};

const PROVIDER: &str = "github";
const ACR_GITHUB: &str = "urn:zeroship:github";

#[derive(Debug, Deserialize)]
pub struct StartQuery {
    pub login_challenge: String,
}

#[derive(Debug, Deserialize)]
pub struct CallbackQuery {
    pub code: Option<String>,
    pub state: Option<String>,
    /// `error`/`error_description` are set when GitHub rejects the
    /// request (e.g. user-cancelled the OAuth app authorisation). We
    /// surface a friendly page instead of trying to exchange a
    /// non-existent code.
    pub error: Option<String>,
    pub error_description: Option<String>,
}

// ─── /oauth/github/start ─────────────────────────────────────────────────

/// Begin the GitHub OAuth dance.
///
/// `!Send` for the same structural reason `oauth_google::start` is:
/// ntex's per-thread service futures hold `Rc`-backed state.
#[allow(clippy::future_not_send)]
pub async fn start(
    query: ntex::web::types::Query<StartQuery>,
    cfg: ntex::web::types::State<Arc<AuthConfig>>,
) -> HttpResponse {
    let auth_start = match github::start_authorize_url(&cfg) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "github start_authorize_url failed");
            return render_error(PublicErrorMessage::PleaseTryAgain);
        }
    };

    let stash = OAuthStash::new(
        auth_start.state.clone(),
        auth_start.verifier,
        // GitHub is OAuth 2.0 — no nonce. We carry an empty string
        // through the stash so the shared payload shape is unchanged.
        String::new(),
        query.login_challenge.clone(),
    );
    let cookie_value = stash.encode(cfg.stash_signing_key.as_bytes());

    let mut resp = HttpResponse::Found();
    resp.header(
        LOCATION,
        HeaderValue::from_str(&auth_start.url).unwrap_or_else(|_| HeaderValue::from_static("/")),
    );
    resp.header(
        SET_COOKIE,
        set_stash_cookie(github_stash_cookie_name(cfg.insecure_dev), &cookie_value, cfg.insecure_dev),
    );
    resp.finish()
}

// ─── /oauth/github/callback ──────────────────────────────────────────────

/// Finish the GitHub OAuth dance — verify state, exchange code, fetch
/// profile + emails, link, then hand off to hydra.
#[allow(clippy::too_many_lines)]
#[allow(clippy::future_not_send)]
pub async fn callback(
    req: HttpRequest,
    query: ntex::web::types::Query<CallbackQuery>,
    admin: ntex::web::types::State<HydraAdmin>,
    cfg: ntex::web::types::State<Arc<AuthConfig>>,
    db: ntex::web::types::State<Arc<compio_postgres::Client>>,
) -> HttpResponse {
    let cookie_header = req
        .headers()
        .get(COOKIE)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let Some(stash_blob) = parse_stash_cookie(cookie_header, github_stash_cookie_name(cfg.insecure_dev)) else {
        audit::emit(
            db.as_ref(),
            &AuditEvent {
                event_type: "oauth_callback_failure",
                outcome: "failure",
                auth_method: Some(PROVIDER),
                detail: json!({ "reason": "stash_cookie_missing" }),
                ..AuditEvent::from_request(&req)
            },
        )
        .await;
        return render_error_clearing(PublicErrorMessage::InvalidRequest, &cfg);
    };
    let Some(stash) = OAuthStash::decode(&stash_blob, cfg.stash_signing_key.as_bytes()) else {
        audit::emit(
            db.as_ref(),
            &AuditEvent {
                event_type: "oauth_callback_failure",
                outcome: "failure",
                auth_method: Some(PROVIDER),
                detail: json!({ "reason": "stash_invalid_signature" }),
                ..AuditEvent::from_request(&req)
            },
        )
        .await;
        return render_error_clearing(PublicErrorMessage::InvalidRequest, &cfg);
    };

    // Upstream rejection path — GitHub returned `?error=...`.
    if let Some(err) = query.error.as_deref() {
        audit::emit(
            db.as_ref(),
            &AuditEvent {
                event_type: "oauth_callback_failure",
                outcome: "failure",
                auth_method: Some(PROVIDER),
                detail: json!({
                    "reason": "upstream_error",
                    "error": err,
                    "description": query.error_description,
                }),
                ..AuditEvent::from_request(&req)
            },
        )
        .await;
        return render_error_clearing(PublicErrorMessage::PleaseTryAgain, &cfg);
    }

    let Some(code) = query.code.as_deref() else {
        return render_error_clearing(PublicErrorMessage::InvalidRequest, &cfg);
    };
    let Some(state_param) = query.state.as_deref() else {
        return render_error_clearing(PublicErrorMessage::InvalidRequest, &cfg);
    };

    // CSRF guard.
    if state_param != stash.state {
        audit::emit(
            db.as_ref(),
            &AuditEvent {
                event_type: "oauth_callback_failure",
                outcome: "failure",
                auth_method: Some(PROVIDER),
                detail: json!({ "reason": "state_mismatch" }),
                ..AuditEvent::from_request(&req)
            },
        )
        .await;
        return render_error_clearing(PublicErrorMessage::InvalidRequest, &cfg);
    }

    if let Err(e) = admin.get_login(&stash.login_challenge).await {
        tracing::warn!(error = %e, challenge = %stash.login_challenge, "github callback hydra challenge validation failed");
        audit::emit(
            db.as_ref(),
            &AuditEvent {
                event_type: "oauth_callback_failure",
                outcome: "failure",
                auth_method: Some(PROVIDER),
                detail: json!({ "reason": "login_challenge_invalid" }),
                ..AuditEvent::from_request(&req)
            },
        )
        .await;
        return render_error_clearing(PublicErrorMessage::InvalidRequest, &cfg);
    }

    // Token exchange + /user + /user/emails.
    let id = match github::complete_callback(&cfg, code, &stash.verifier).await {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!(error = %e, "github complete_callback failed");
            // The picker-failure error string is distinctive; surface a
            // more specific audit reason so dashboards can split
            // "verified-email" issues from generic upstream failures.
            let reason = if e.to_string().contains("primary + verified email") {
                "email_picker_failed"
            } else {
                "upstream_error"
            };
            audit::emit(
                db.as_ref(),
                &AuditEvent {
                    event_type: "oauth_callback_failure",
                    outcome: "failure",
                    auth_method: Some(PROVIDER),
                    detail: json!({ "reason": reason, "error": e.to_string() }),
                    ..AuditEvent::from_request(&req)
                },
            )
            .await;
            return render_error_clearing(PublicErrorMessage::PleaseTryAgain, &cfg);
        }
    };

    // Build the ResolvedProfile. The github picker guarantees
    // `chosen.verified == true`, so both `email_verified` and
    // `provider_trusted_for_email` are unconditionally true here.
    let raw_profile = serde_json::to_value(&id).ok();
    let profile = build_resolved_profile(&id, raw_profile.as_ref());

    let outcome = match linker::resolve_or_link(
        db.as_ref(),
        &profile,
        &stash.login_challenge,
        cfg.stash_signing_key.as_bytes(),
    )
    .await
    {
        Ok(o) => o,
        Err(e) => {
            tracing::error!(error = %e, "linker::resolve_or_link failed");
            audit::emit(
                db.as_ref(),
                &AuditEvent {
                    event_type: "oauth_callback_failure",
                    outcome: "failure",
                    auth_method: Some(PROVIDER),
                    detail: json!({ "reason": "linker_failed", "error": e.to_string() }),
                    ..AuditEvent::from_request(&req)
                },
            )
            .await;
            return render_error_clearing(PublicErrorMessage::ContactSupport, &cfg);
        }
    };

    let user_id = match outcome {
        LinkOutcome::Existing { user_id } | LinkOutcome::Created { user_id } => user_id,
        LinkOutcome::NeedsConfirmation { pending_token, .. } => {
            // Email collided with a locally-credentialed user. Bounce to
            // /link?token=… so the user can confirm with their existing
            // zeroship password — hydra stays pending until /link POST
            // resolves the challenge.
            audit::emit(
                db.as_ref(),
                &AuditEvent {
                    event_type: "oauth_link_needs_confirmation",
                    outcome: "success",
                    auth_method: Some(PROVIDER),
                    detail: json!({ "subject": id.subject, "email": id.email }),
                    ..AuditEvent::from_request(&req)
                },
            )
            .await;
            let location = format!("/link?token={pending_token}");
            let mut resp = HttpResponse::Found();
            resp.header(
                LOCATION,
                HeaderValue::from_str(&location)
                    .unwrap_or_else(|_| HeaderValue::from_static("/link")),
            );
            resp.header(
                SET_COOKIE,
                clear_stash_cookie(github_stash_cookie_name(cfg.insecure_dev), cfg.insecure_dev),
            );
            return resp.finish();
        }
    };

    if let Err(e) = eligibility::check_user_eligible(db.as_ref(), user_id).await {
        if !e.is_account_state() {
            tracing::error!(error = %e, user_id = %user_id, "github callback eligibility check failed");
            return render_error_clearing(PublicErrorMessage::ContactSupport, &cfg);
        }
        audit::emit(
            db.as_ref(),
            &AuditEvent {
                event_type: "oauth_callback_failure",
                outcome: "failure",
                user_id: Some(&user_id),
                auth_method: Some(PROVIDER),
                detail: json!({ "reason": "account_ineligible" }),
                ..AuditEvent::from_request(&req)
            },
        )
        .await;
        return render_error_clearing(PublicErrorMessage::AccountTemporarilyLocked, &cfg);
    }

    // Best-effort: bump last_login_at on the user row.
    if let Err(e) = users::touch_last_login(db.as_ref(), user_id).await {
        tracing::warn!(error = %e, user_id = %user_id, "touch_last_login failed");
    }

    // IdP session row at auth.zeroship.ai.
    let session = match sessions::create(
        db.as_ref(),
        &sessions::CreateSession {
            user_id,
            auth_method: PROVIDER,
            amr: vec!["oauth".into()],
            acr: Some(ACR_GITHUB),
            expected_credential_version: None,
            idle_minutes: session_cookie::IDLE_MINUTES,
            absolute_hours: session_cookie::ABSOLUTE_HOURS,
        },
    )
    .await
    {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "sessions::create failed");
            return render_error_clearing(PublicErrorMessage::ContactSupport, &cfg);
        }
    };

    // Accept the hydra login challenge.
    let accept = AcceptLoginRequest {
        subject: user_id.to_string(),
        remember: Some(true),
        remember_for: Some(3600),
        acr: Some(ACR_GITHUB.into()),
        amr: Some(vec!["oauth".into()]),
        ..Default::default()
    };
    let redirect_to = match admin.accept_login(&stash.login_challenge, &accept).await {
        Ok(r) => r.redirect_to,
        Err(e) => {
            tracing::error!(error = %e, "accept_login failed");
            return render_error_clearing(PublicErrorMessage::ContactSupport, &cfg);
        }
    };

    audit::emit(
        db.as_ref(),
        &AuditEvent {
            event_type: "oauth_callback_success",
            outcome: "success",
            user_id: Some(&user_id),
            auth_method: Some(PROVIDER),
            detail: json!({
                "subject": id.subject,
                "login": id.login,
                "created": matches!(outcome, LinkOutcome::Created { .. }),
            }),
            ..AuditEvent::from_request(&req)
        },
    )
    .await;

    let mut resp = HttpResponse::Found();
    resp.header(
        LOCATION,
        HeaderValue::from_str(&redirect_to).unwrap_or_else(|_| HeaderValue::from_static("/")),
    );
    resp.header(
        SET_COOKIE,
        session_cookie::set_cookie(&session.id, cfg.insecure_dev),
    );
    resp.header(
        SET_COOKIE,
        clear_stash_cookie(github_stash_cookie_name(cfg.insecure_dev), cfg.insecure_dev),
    );
    resp.finish()
}

/// Build a [`ResolvedProfile`] from the GitHub identity.
///
/// Because the [`github::complete_callback`] picker only ever returns a
/// `primary && verified && !noreply` address, the email is always
/// considered verified, and the provider is always trusted for the
/// email (the picker already enforced GitHub's verified flag — there
/// is no additional Workspace-domain analogue).
fn build_resolved_profile<'a>(
    id: &'a GitHubIdentity,
    raw_profile: Option<&serde_json::Value>,
) -> ResolvedProfile<'a> {
    ResolvedProfile {
        provider: PROVIDER,
        subject: &id.subject,
        email: &id.email,
        email_verified: true,
        name: id.name.as_deref().or(Some(&id.login)),
        avatar_url: id.avatar_url.as_deref(),
        provider_trusted_for_email: true,
        raw_profile: raw_profile.cloned(),
    }
}

fn render_error(message: PublicErrorMessage) -> HttpResponse {
    let page = ErrorPage {
        message,
        error_code: message.error_code(),
    };
    let body = page
        .render()
        .unwrap_or_else(|_| format!("<h1>{}</h1>", message.as_str()));
    let mut resp = HttpResponse::Ok();
    resp.content_type("text/html; charset=utf-8");
    resp.body(body)
}

/// Same as [`render_error`] but also clears the stash cookie. Use on
/// the callback path so an aborted dance doesn't leave a stale stash
/// on the browser.
fn render_error_clearing(
    message: PublicErrorMessage,
    cfg: &AuthConfig,
) -> HttpResponse {
    let page = ErrorPage {
        message,
        error_code: message.error_code(),
    };
    let body = page
        .render()
        .unwrap_or_else(|_| format!("<h1>{}</h1>", message.as_str()));
    let mut resp = HttpResponse::Ok();
    resp.content_type("text/html; charset=utf-8");
    resp.header(
        SET_COOKIE,
        clear_stash_cookie(github_stash_cookie_name(cfg.insecure_dev), cfg.insecure_dev),
    );
    resp.body(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(email: &str, name: Option<&str>) -> GitHubIdentity {
        GitHubIdentity {
            subject: "12345".into(),
            login: "alice".into(),
            email: email.into(),
            name: name.map(String::from),
            avatar_url: Some("https://avatars/x".into()),
        }
    }

    #[test]
    fn always_trusted_because_picker_enforces_verified() {
        let g = id("alice@example.com", Some("Alice"));
        let p = build_resolved_profile(&g, None);
        assert!(p.provider_trusted_for_email);
        assert!(p.email_verified);
        assert_eq!(p.provider, "github");
        assert_eq!(p.subject, "12345");
        assert_eq!(p.email, "alice@example.com");
        assert_eq!(p.name, Some("Alice"));
    }

    #[test]
    fn name_falls_back_to_login_when_absent() {
        // GitHub `/user` returns `name: null` when the user hasn't set
        // a display name. We carry the login as a sensible default so
        // `auth.users.name` never lands as an empty string.
        let g = id("alice@example.com", None);
        let p = build_resolved_profile(&g, None);
        assert_eq!(p.name, Some("alice"));
    }
}
