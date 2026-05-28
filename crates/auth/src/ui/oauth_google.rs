//! `/oauth/google/start` + `/oauth/google/callback` — Google OIDC federation.
//!
//! Flow:
//!
//!   1. **start** — `?login_challenge=…` arrives on the auth server. We
//!      generate PKCE+state+nonce via [`identity::oauth::google`], stash
//!      them (plus the `login_challenge`) in a signed cookie, and 302 to
//!      `accounts.google.com/o/oauth2/v2/auth`.
//!   2. **callback** — `?code=…&state=…` arrives back. We re-read the
//!      stash, verify state, exchange the code for an ID token, verify
//!      that against Google's JWKS, resolve / create the local user via
//!      [`identity::linker`], create an `auth.sessions` row, and finally
//!      call hydra's `accept_login` to hand control back to the OIDC
//!      pipeline. The `IdP` session cookie is dropped on the same
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
use crate::hydra_client::types::AcceptLoginRequest;
use crate::hydra_client::HydraAdmin;
use crate::identity::eligibility;
use crate::identity::linker::{self, LinkOutcome, ResolvedProfile};
use crate::identity::oauth::google::{self, GoogleIdentity};
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
    pub login_challenge: String,
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
    let auth_start = match google::start_authorize_url(&cfg) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "google start_authorize_url failed");
            return render_error(PublicErrorMessage::PleaseTryAgain);
        }
    };

    let stash = OAuthStash::new(
        auth_start.state.clone(),
        auth_start.verifier,
        auth_start.nonce.clone(),
        query.login_challenge.clone(),
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
/// hand off to hydra.
#[allow(clippy::too_many_lines)]
#[allow(clippy::future_not_send)]
pub async fn callback(
    req: HttpRequest,
    query: ntex::web::types::Query<CallbackQuery>,
    admin: ntex::web::types::State<HydraAdmin>,
    cfg: ntex::web::types::State<Arc<AuthConfig>>,
    db: ntex::web::types::State<Arc<compio_postgres::Client>>,
    jwks: ntex::web::types::State<Arc<JwksCache>>,
) -> HttpResponse {
    // Read + verify stash cookie. We need this even on the upstream-error
    // path so we can report which login_challenge failed (and clear the
    // cookie).
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
                ..Default::default()
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
                ..Default::default()
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
                ..Default::default()
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
                ..Default::default()
            },
        )
        .await;
        return render_error_clearing(PublicErrorMessage::InvalidRequest, &cfg);
    }

    if let Err(e) = admin.get_login(&stash.login_challenge).await {
        tracing::warn!(error = %e, challenge = %stash.login_challenge, "google callback hydra challenge validation failed");
        audit::emit(
            db.as_ref(),
            &AuditEvent {
                event_type: "oauth_callback_failure",
                outcome: "failure",
                auth_method: Some(PROVIDER),
                detail: json!({ "reason": "login_challenge_invalid" }),
                ..Default::default()
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
                    ..Default::default()
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
                    ..Default::default()
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
            // zeroship password — do NOT accept_login here; hydra stays
            // pending until /link POST resolves the challenge.
            audit::emit(
                db.as_ref(),
                &AuditEvent {
                    event_type: "oauth_link_needs_confirmation",
                    outcome: "success",
                    auth_method: Some(PROVIDER),
                    detail: json!({ "subject": id.subject, "email": id.email }),
                    ..Default::default()
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
                ..Default::default()
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
        acr: Some(ACR_GOOGLE.into()),
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
                "created": matches!(outcome, LinkOutcome::Created { .. }),
                "hd": id.hd,
            }),
            ..Default::default()
        },
    )
    .await;

    let mut resp = HttpResponse::Found();
    resp.header(
        LOCATION,
        HeaderValue::from_str(&redirect_to)
            .unwrap_or_else(|_| HeaderValue::from_static("/")),
    );
    // Drop the IdP session cookie + clear the now-spent stash on the same
    // response. ntex's `header()` appends, so two SET_COOKIE values both
    // make it onto the wire.
    resp.header(
        SET_COOKIE,
        session_cookie::set_cookie(&session.id, cfg.insecure_dev),
    );
    resp.header(
        SET_COOKIE,
        clear_stash_cookie(google_stash_cookie_name(cfg.insecure_dev), cfg.insecure_dev),
    );
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
