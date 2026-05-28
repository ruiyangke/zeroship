//! `/me` GET + `/me/unlink/<provider>` POST handlers — the logged-in
//! user's profile page (P4-U6).
//!
//! Authentication source of truth is the `__Host-zsidp_session` cookie —
//! the `IdP` session at `auth.zeroship.ai`, not any per-app
//! `gateway_sessions` row. `store::sessions::validate` slides the idle
//! window on each hit, so the page itself counts as activity.
//!
//! ## Unlink policy
//!
//! Refuse to unlink the last credential. The user MUST retain at least one
//! way to sign in after the unlink, defined as:
//!
//!   - a local password (`auth.users.password_hash IS NOT NULL`), OR
//!   - at least one OTHER linked identity (different `provider`).
//!
//! Without this guard a single-provider OAuth user could lock themselves
//! out by clicking Unlink, and there is no self-service path back (no
//! password to reset). Mirroring Google/GitHub/Apple's own policies.
//!
//! ## Link-from-/me deferred
//!
//! Linking a new provider FROM /me requires the federation start/callback
//! handlers (`oauth_google`, `oauth_github`) to know they're being invoked
//! for "add another link to the signed-in user" vs "sign in / find-or-link
//! by email". That's a non-trivial extension to `linker::resolve_or_link`
//! and the start-route signature — out of scope for U6. The template
//! renders the section with a "coming soon" note. Tracked for Phase 4.5+.

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
use crate::sessions::login as session_cookie;
use crate::store::identities::{GuardedUnlink, Identity};
use crate::store::{identities, sessions, users};
use crate::store::users::UserRow;
use crate::ui::{ErrorPage, LinkedIdentity, MePage, PublicErrorMessage};

const MAX_PROVIDER_PATH_BYTES: usize = 64;

// ─── /me GET ─────────────────────────────────────────────────────────────

/// Render the profile page for the signed-in user.
///
/// Algorithm:
///
///   1. Parse `__Host-zsidp_session` from the `Cookie` header.
///   2. `store::sessions::validate` — atomic check-and-slide. None →
///      302 `/login`.
///   3. Look up the user by `session.user_id`. None → error page (the
///      session row pointed at a user that no longer exists; this is a
///      data-integrity warning, not a normal flow).
///   4. List `auth.identities` rows for the user.
///   5. Render `MePage` with a fresh CSRF cookie (used by the unlink form).
///
/// `!Send` for the same structural reason every other ntex handler in
/// this crate is.
#[allow(clippy::future_not_send)]
pub async fn get(
    req: HttpRequest,
    cfg: ntex::web::types::State<Arc<AuthConfig>>,
    db: ntex::web::types::State<Arc<compio_postgres::Client>>,
) -> HttpResponse {
    let Some(user) = resolve_user(&req, db.as_ref(), cfg.insecure_dev).await else {
        return redirect_to_login();
    };

    let idents = identities::list_for_user(db.as_ref(), user.id)
        .await
        .unwrap_or_default();

    let csrf_token = csrf::generate_token();
    render_me(&user, &idents, &csrf_token, &cfg, None, None)
}

// ─── /me/unlink/<provider> POST ──────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct UnlinkForm {
    pub csrf: String,
}

/// Unlink one external identity from the signed-in user. Refuses to
/// orphan the account (see module-level "Unlink policy").
///
/// On success the page is re-rendered (200) with a green success banner;
/// on policy refusal or "not linked" the page is re-rendered with a red
/// error banner. Always returns 200 + the same template so the user has
/// somewhere to land — no naked redirects.
#[allow(clippy::future_not_send, clippy::too_many_lines)]
pub async fn unlink(
    req: HttpRequest,
    path: ntex::web::types::Path<(String,)>,
    form: ntex::web::types::Form<UnlinkForm>,
    cfg: ntex::web::types::State<Arc<AuthConfig>>,
    db: ntex::web::types::State<Arc<compio_postgres::Client>>,
) -> HttpResponse {
    let provider = path.into_inner().0;
    if !valid_provider_path_segment(&provider) {
        return render_error_page_with_status(
            PublicErrorMessage::InvalidRequest,
            StatusCode::BAD_REQUEST,
        );
    }

    // 1. CSRF.
    let cookie_header = req
        .headers()
        .get(COOKIE)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let cookie_token = csrf::parse_cookie(cookie_header, cfg.insecure_dev);
    if cookie_token
        .as_deref()
        .is_none_or(|c| !csrf::matches(&form.csrf, c))
    {
        return render_error_page(PublicErrorMessage::InvalidRequest);
    }

    // 2. Session.
    let Some(user) = resolve_user(&req, db.as_ref(), cfg.insecure_dev).await else {
        return redirect_to_login();
    };

    // 3. Unlink with the orphan-guard enforced atomically in SQL.
    let result = match identities::unlink_preserving_credential(db.as_ref(), user.id, &provider)
        .await
    {
        Ok(result) => result,
        Err(e) => {
            tracing::error!(error = %e, "identities::unlink_preserving_credential failed");
            return render_error_page(PublicErrorMessage::ContactSupport);
        }
    };

    let idents = match identities::list_for_user(db.as_ref(), user.id).await {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(error = %e, "identities::list_for_user failed");
            return render_error_page(PublicErrorMessage::ContactSupport);
        }
    };

    if result == GuardedUnlink::NotLinked {
        let token = csrf::generate_token();
        return render_me(
            &user,
            &idents,
            &token,
            &cfg,
            Some("Identity not found"),
            None,
        );
    }

    if result == GuardedUnlink::WouldOrphan {
        audit::emit(
            db.as_ref(),
            &AuditEvent {
                event_type: "oauth_unlink_refused",
                outcome: "failure",
                user_id: Some(&user.id),
                auth_method: Some(&provider),
                detail: json!({ "reason": "would_orphan_account" }),
                ..AuditEvent::from_request(&req)
            },
        )
        .await;
        let token = csrf::generate_token();
        return render_me(
            &user,
            &idents,
            &token,
            &cfg,
            Some("Cannot unlink — this is your only sign-in method"),
            None,
        );
    }

    // 4. Audit.
    audit::emit(
        db.as_ref(),
        &AuditEvent {
            event_type: "oauth_unlink_success",
            outcome: "success",
            user_id: Some(&user.id),
            auth_method: Some(&provider),
            detail: json!({ "provider": provider }),
            ..AuditEvent::from_request(&req)
        },
    )
    .await;

    // 5. Re-render with the fresh identities list.
    let token = csrf::generate_token();
    render_me(
        &user,
        &idents,
        &token,
        &cfg,
        None,
        Some("Identity unlinked"),
    )
}

// ─── helpers ─────────────────────────────────────────────────────────────

/// Resolve the signed-in user from the request, or `None` if the cookie
/// is missing/invalid/expired or the user row is gone.
#[allow(clippy::future_not_send)]
async fn resolve_user(
    req: &HttpRequest,
    db: &compio_postgres::Client,
    insecure_dev: bool,
) -> Option<UserRow> {
    let cookie_header = req
        .headers()
        .get(COOKIE)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let session_id = session_cookie::parse_cookie(cookie_header, insecure_dev)?;

    let session = sessions::validate(db, session_id).await.ok().flatten()?;
    users::find_by_id(db, &session.user_id.to_string())
        .await
        .ok()
        .flatten()
}

/// Pure predicate — extracted so the unit tests below can assert the
/// "would not leave the account credential-less" rule directly.
///
/// Returns `true` when at least one sign-in method remains after the
/// unlink (a password OR one+ other linked identity).
#[must_use]
#[cfg(test)]
pub(crate) const fn would_leave_credential(has_password: bool, other_identities: usize) -> bool {
    has_password || other_identities > 0
}

fn valid_provider_path_segment(provider: &str) -> bool {
    !provider.is_empty() && provider.len() <= MAX_PROVIDER_PATH_BYTES
}

#[allow(clippy::too_many_arguments)]
fn render_me(
    user: &UserRow,
    idents: &[Identity],
    csrf_token: &str,
    cfg: &AuthConfig,
    error: Option<&str>,
    success: Option<&str>,
) -> HttpResponse {
    let identities_view: Vec<LinkedIdentity<'_>> = idents
        .iter()
        .map(|i| LinkedIdentity {
            provider: &i.provider,
            email_at_link: i.email_at_link.as_deref().unwrap_or(""),
        })
        .collect();

    let page = MePage {
        email: &user.email,
        name: &user.name,
        avatar_url: user.avatar_url.as_deref(),
        has_password: user.password_hash.is_some(),
        identities: identities_view,
        csrf: csrf_token,
        error,
        success,
    };
    let body = match page.render() {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(error = %e, "render me.html failed");
            return render_error_page(PublicErrorMessage::ContactSupport);
        }
    };

    let mut resp = HttpResponse::Ok();
    resp.content_type("text/html; charset=utf-8");
    resp.header(SET_COOKIE, csrf::set_cookie(csrf_token, cfg.insecure_dev));
    resp.body(body)
}

fn redirect_to_login() -> HttpResponse {
    let mut r = HttpResponse::Found();
    r.header(LOCATION, HeaderValue::from_static("/login"));
    r.finish()
}

fn render_error_page(message: PublicErrorMessage) -> HttpResponse {
    render_error_page_with_status(message, StatusCode::OK)
}

fn render_error_page_with_status(message: PublicErrorMessage, status: StatusCode) -> HttpResponse {
    let page = ErrorPage {
        message,
        error_code: message.error_code(),
    };
    let body = page
        .render()
        .unwrap_or_else(|_| format!("<h1>{}</h1>", message.as_str()));
    let mut r = HttpResponse::build(status);
    r.content_type("text/html; charset=utf-8");
    r.body(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Account with only one OAuth identity (e.g. Google), no password:
    /// unlinking that identity would leave the user with NO way to sign
    /// in. Must be refused.
    #[test]
    fn refuses_unlink_when_orphans_account() {
        let has_password = false;
        let other_identities = 0;
        assert!(
            !would_leave_credential(has_password, other_identities),
            "no password + no other identity → orphan",
        );
    }

    /// Account with a password and one OAuth identity: unlinking the
    /// OAuth identity is fine because the password remains.
    #[test]
    fn allows_unlink_when_password_exists() {
        let has_password = true;
        let other_identities = 0;
        assert!(would_leave_credential(has_password, other_identities));
    }

    /// Account with two OAuth identities, no password: unlinking one
    /// leaves the other as a working sign-in path.
    #[test]
    fn allows_unlink_when_other_identity_exists() {
        let has_password = false;
        let other_identities = 1;
        assert!(would_leave_credential(has_password, other_identities));
    }

    #[test]
    fn provider_path_segment_is_bounded() {
        assert!(valid_provider_path_segment("github"));
        assert!(valid_provider_path_segment(&"a".repeat(64)));
        assert!(!valid_provider_path_segment(""));
        assert!(!valid_provider_path_segment(&"a".repeat(65)));
    }
}
