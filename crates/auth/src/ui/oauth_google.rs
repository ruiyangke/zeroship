//! `/oauth/google/start` + `/oauth/google/callback` — Google OIDC federation.
//!
//! Flow:
//!
//!   1. **start** — `?return_to=…` arrives on the auth server. We
//!      generate PKCE+state+nonce via [`identity::oauth::google`], stash
//!      them (plus the `return_to`) in a signed cookie, and 302 to
//!      `accounts.google.com/o/oauth2/v2/auth`.
//!   2. **callback** — `?code=…&state=…` arrives back. We re-read the
//!      stash, verify state, exchange the code for an ID token, verify
//!      that against Google's JWKS, resolve / create the local user via
//!      [`identity::linker`], create an `zeroship.idp_sessions` row, and
//!      redirect back to the native OIDC pipeline. The `IdP` session cookie is dropped on the same
//!      response so subsequent SSO requests skip the login form.
//!
//! Route gating: the auth server only registers these routes when
//! `cfg.google_client_id.is_some()` (see [`crate::server::configure`]).
//! The `Arc<JwksCache>` state is registered under the same predicate, so
//! handlers always find it present at request time.

use std::sync::Arc;

use askama::Template;
use ntex::http::header::{HeaderValue, COOKIE, LOCATION, SET_COOKIE};
use ntex::web::{HttpRequest, HttpResponse};
use serde::Deserialize;
use serde_json::json;
use zeroship_core::oidc_verify::JwksCache;

use crate::audit::{self, AuditEvent};
use crate::config::AuthConfig;
use crate::identity::eligibility;
use crate::identity::linker::{self, LinkOutcome, LinkResume, ResolvedProfile};
use crate::identity::oauth::{
    google::{self, GoogleIdentity},
    UpstreamPrompt,
};
use crate::oidc::auth_request::AuthRequest;
use crate::oidc::authorization_code::return_to_after_prompt_interaction;
use crate::return_to;
use crate::sessions::login as session_cookie;
use crate::store::{sessions, users};
use crate::ui::oauth_stash::{
    clear_stash_cookie, google_stash_cookie_name, parse_stash_cookie, set_stash_cookie, OAuthStash,
};
use crate::ui::{ErrorPage, PublicErrorMessage};

const PROVIDER: &str = "google";
const ACR_GOOGLE: &str = "urn:zeroship:google";

#[derive(Debug, Deserialize)]
pub struct StartQuery {
    pub return_to: Option<String>,
    pub prompt: Option<String>,
    pub max_age: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CallbackQuery {
    pub code: Option<String>,
    pub state: Option<String>,
    /// `error`/`error_description` are set when Google rejects the
    /// request (e.g. user-cancelled consent). We surface a friendly
    /// page instead of trying to exchange a non-existent code.
    pub error: Option<String>,
    pub error_description: Option<String>,
}

// ─── /oauth/google/start ─────────────────────────────────────────────────

/// Begin the Google OAuth dance.
///
/// ntex's per-thread service futures are intentionally `!Send` (their
/// internal state uses `Rc`s), so handler functions can't be `Send` — the
/// `#[allow(clippy::future_not_send)]` here is structural, matching the
/// rest of the auth `ui::*` handlers.
#[allow(clippy::future_not_send)]
pub async fn start(
    query: ntex::web::types::Query<StartQuery>,
    cfg: ntex::web::types::State<Arc<AuthConfig>>,
) -> HttpResponse {
    let query = query.into_inner();
    let Some(raw_return_to) = query.return_to.as_deref() else {
        return render_error(PublicErrorMessage::InvalidRequest);
    };
    let native_return_to = return_to::sanitize(Some(raw_return_to), return_to::SAFE_DEFAULT);
    let Ok(auth_request) = AuthRequest::parse_return_to(&native_return_to) else {
        return render_error(PublicErrorMessage::InvalidRequest);
    };
    let upstream_prompt = UpstreamPrompt::combine(
        auth_request.upstream_prompt(),
        UpstreamPrompt::from_oidc_prompt(query.prompt.as_deref(), query.max_age.as_deref()),
    );

    let auth_start = match google::start_authorize_url(&cfg, upstream_prompt) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "google start_authorize_url failed");
            return render_error(PublicErrorMessage::PleaseTryAgain);
        }
    };

    let stash = OAuthStash::with_return_to(
        auth_start.state.clone(),
        auth_start.verifier,
        auth_start.nonce.clone(),
        native_return_to,
    );
    let cookie_value = stash.encode(cfg.stash_signing_key.as_bytes());

    let mut resp = HttpResponse::Found();
    resp.header(
        LOCATION,
        HeaderValue::from_str(&auth_start.url)
            .unwrap_or_else(|_| HeaderValue::from_static("/")),
    );
    resp.header(
        SET_COOKIE,
        set_stash_cookie(google_stash_cookie_name(cfg.insecure_dev), &cookie_value, cfg.insecure_dev),
    );
    resp.finish()
}

// ─── /oauth/google/callback ──────────────────────────────────────────────

/// Finish the Google OAuth dance — verify state, exchange code, link, then
/// resume the native authorize request.
#[allow(clippy::too_many_lines)]
#[allow(clippy::future_not_send)]
pub async fn callback(
    req: HttpRequest,
    query: ntex::web::types::Query<CallbackQuery>,
    cfg: ntex::web::types::State<Arc<AuthConfig>>,
    db: ntex::web::types::State<Arc<compio_postgres::Client>>,
    jwks: ntex::web::types::State<Arc<JwksCache>>,
) -> HttpResponse {
    // Read + verify stash cookie. We need this even on the upstream-error
    // path so we can clear the cookie.
    let cookie_header = req
        .headers()
        .get(COOKIE)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let Some(stash_blob) = parse_stash_cookie(cookie_header, google_stash_cookie_name(cfg.insecure_dev)) else {
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

    // Upstream rejection path — Google returned `?error=...` (e.g. user
    // declined). Surface the upstream description verbatim; no need to
    // try the code exchange.
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

    // Token exchange + ID-token verify.
    let id = match google::complete_callback(
        &cfg,
        code,
        &stash.verifier,
        &stash.nonce,
        jwks.as_ref(),
    )
    .await
    {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!(error = %e, "google complete_callback failed");
            audit::emit(
                db.as_ref(),
                &AuditEvent {
                    event_type: "oauth_callback_failure",
                    outcome: "failure",
                    auth_method: Some(PROVIDER),
                    detail: json!({ "reason": "verify_failed", "error": e.to_string() }),
                    ..AuditEvent::from_request(&req)
                },
            )
            .await;
            return render_error_clearing(PublicErrorMessage::PleaseTryAgain, &cfg);
        }
    };

    // Build the ResolvedProfile. We trust Google for email verification
    // when the email_verified claim is true AND either the address is a
    // consumer `@gmail.com` (Google operates that domain end-to-end) or
    // the `hd` claim is set (Workspace-controlled domain). For any other
    // domain we still accept the email but require the unverified-auto-
    // create gate in the linker to refuse fresh-account creation.
    let raw_profile = serde_json::to_value(&id).ok();
    let profile = build_resolved_profile(&id, raw_profile.as_ref());

    let Some(native_return_to) = stash.return_to.as_deref() else {
        return render_error_clearing(PublicErrorMessage::InvalidRequest, &cfg);
    };
    let resume = LinkResume::ReturnTo(native_return_to);
    let outcome = match linker::resolve_or_link(
        db.as_ref(),
        &profile,
        resume,
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

    let created = matches!(&outcome, LinkOutcome::Created { .. });
    let user_id = match outcome {
        LinkOutcome::Existing { user_id } | LinkOutcome::Created { user_id } => user_id,
        LinkOutcome::NeedsConfirmation { pending_token, .. } => {
            // Email collided with a locally-credentialed user. Bounce to
            // /link?token=… so the user can confirm with their existing
            // zeroship password before the native authorize request resumes.
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
            // Clear the stash on the way out — the dance is over from the
            // federation handler's perspective.
            resp.header(
                SET_COOKIE,
                clear_stash_cookie(google_stash_cookie_name(cfg.insecure_dev), cfg.insecure_dev),
            );
            return resp.finish();
        }
    };

    // F2 lockout recovery: a verified federated Google login is strong
    // owner-present evidence, so clear any soft password-guessing lockout before
    // the eligibility gate — otherwise a victim locked by password-guessing
    // could never recover via OAuth. Best-effort; the gate still enforces hard
    // `disabled_at`.
    if let Err(e) = users::reset_login_failures(db.as_ref(), user_id).await {
        tracing::warn!(error = %e, user_id = %user_id, "google clear lockout failed");
    }
    if let Err(e) = eligibility::check_user_eligible(db.as_ref(), user_id).await {
        if !e.is_account_state() {
            tracing::error!(error = %e, user_id = %user_id, "google callback eligibility check failed");
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

    // IdP session row at auth.zeroship.ai. Same lifetime + amr/acr shape
    // the password flow uses, just with the OAuth method tag.
    let session = match sessions::create(
        db.as_ref(),
        &sessions::CreateSession {
            user_id,
            auth_method: PROVIDER,
            amr: vec!["oauth".into()],
            acr: Some(ACR_GOOGLE),
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

    audit::emit(
        db.as_ref(),
        &AuditEvent {
            event_type: "oauth_callback_success",
            outcome: "success",
            user_id: Some(&user_id),
            auth_method: Some(PROVIDER),
            detail: json!({
                "subject": id.subject,
                "created": created,
                "hd": id.hd,
            }),
            ..AuditEvent::from_request(&req)
        },
    )
    .await;

    let native_return_to =
        return_to_after_prompt_interaction(native_return_to, &["login", "select_account"]);
    let mut resp = return_to::see_other(&native_return_to);
    resp.header(
        SET_COOKIE,
        session_cookie::set_cookie(&session.id, cfg.insecure_dev),
    );
    resp.header(
        SET_COOKIE,
        clear_stash_cookie(google_stash_cookie_name(cfg.insecure_dev), cfg.insecure_dev),
    );
    resp.header("cache-control", "no-store");
    resp.finish()
}

/// Build a [`ResolvedProfile`] from the Google identity. Borrows from `id`
/// for `&str` fields so we don't have to clone strings into the profile.
fn build_resolved_profile<'a>(
    id: &'a GoogleIdentity,
    raw_profile: Option<&serde_json::Value>,
) -> ResolvedProfile<'a> {
    let provider_trusted_for_email = id.email_verified
        && (id.email.ends_with("@gmail.com") || id.hd.is_some());
    ResolvedProfile {
        provider: PROVIDER,
        subject: &id.subject,
        email: &id.email,
        email_verified: id.email_verified,
        name: id.name.as_deref(),
        avatar_url: id.picture.as_deref(),
        provider_trusted_for_email,
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

/// Same as [`render_error`] but also clears the stash cookie. Use on the
/// callback path so an aborted dance doesn't leave a stale stash on the
/// browser.
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
        clear_stash_cookie(google_stash_cookie_name(cfg.insecure_dev), cfg.insecure_dev),
    );
    resp.body(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(email: &str, hd: Option<&str>, verified: bool) -> GoogleIdentity {
        GoogleIdentity {
            subject: "sub-1".into(),
            email: email.into(),
            email_verified: verified,
            name: Some("Ada".into()),
            picture: Some("https://lh3/x".into()),
            hd: hd.map(String::from),
        }
    }

    #[test]
    fn provider_trusted_for_consumer_gmail() {
        let g = id("a@gmail.com", None, true);
        let p = build_resolved_profile(&g, None);
        assert!(p.provider_trusted_for_email);
    }

    #[test]
    fn provider_trusted_for_workspace_hd() {
        let g = id("a@example.com", Some("example.com"), true);
        let p = build_resolved_profile(&g, None);
        assert!(p.provider_trusted_for_email);
    }

    #[test]
    fn provider_not_trusted_for_unverified_email() {
        // Even on @gmail.com we don't trust unverified email — Google
        // shouldn't issue email_verified=false on its own domain, but
        // the policy is "verified == precondition".
        let g = id("a@gmail.com", None, false);
        let p = build_resolved_profile(&g, None);
        assert!(!p.provider_trusted_for_email);
    }

    #[test]
    fn provider_not_trusted_for_arbitrary_domain_without_hd() {
        let g = id("a@example.com", None, true);
        let p = build_resolved_profile(&g, None);
        assert!(!p.provider_trusted_for_email);
    }
}
