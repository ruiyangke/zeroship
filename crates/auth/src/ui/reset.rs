//! `/reset` GET + POST handlers.
//!
//! GET renders the new-password form with the reset token in a hidden
//! field. POST validates CSRF + password length, atomically redeems the
//! reset token (single-use), Argon2-hashes the new password on a
//! `spawn_blocking` worker (the event loop stays free), updates
//! `zeroship.users.password_hash`, emits a `password_changed` audit event,
//! revokes every existing session, consumes outstanding email tokens,
//! clears cross-device magic completions, and redirects to `/login`.
//!
//! Note: GET does not "peek" at the token. Token validity is checked
//! only at POST time, at the moment of redemption. The form might
//! render against an already-expired token; the user will see "reset
//! link invalid or expired" on submit. The alternative — validating at
//! GET and again at POST — costs an extra DB round-trip per render and
//! the 1-hour TTL + 32-byte CSPRNG entropy makes pre-emptive feedback
//! unnecessary.

use askama::Template;
use ntex::http::header::{HeaderValue, COOKIE, LOCATION, SET_COOKIE};
use ntex::web::{
    types::{Form, Query, State},
    HttpRequest, HttpResponse,
};
use serde::Deserialize;
use std::sync::Arc;

use crate::audit::{self, AuditEvent};
use crate::config::AuthConfig;
use crate::csrf;
use crate::error::{AuthError, Result};
use crate::headers;
use crate::identity::{password, password_reset};
use crate::ui::ResetPage;

#[derive(Debug, Deserialize)]
pub struct ResetQuery {
    /// Raw reset token. base64url, no padding (32-byte CSPRNG).
    pub token: String,
}

#[derive(Debug, Deserialize)]
pub struct ResetForm {
    pub csrf: String,
    pub token: String,
    pub password: String,
}

/// `/reset?token=<token>` GET — render the new-password form.
//
// ntex's per-thread service futures are intentionally `!Send`.
#[allow(clippy::unused_async, clippy::future_not_send)]
pub async fn get(query: Query<ResetQuery>, cfg: State<Arc<AuthConfig>>) -> HttpResponse {
    render_form(&cfg, &query.token, None)
}

/// `/reset` POST — validate token + length, atomically redeem the
/// reset token, hash the new password, update the user, audit, revoke
/// existing sessions, consume pending email tokens, and redirect to `/login`.
#[allow(clippy::future_not_send)]
pub async fn post(
    req: HttpRequest,
    form: Form<ResetForm>,
    cfg: State<Arc<AuthConfig>>,
    db: State<Arc<compio_postgres::Client>>,
) -> HttpResponse {
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
        return render_form(&cfg, &form.token, Some("invalid request"));
    }

    // 2. NIST 800-63B Rev 4: 15-character minimum (same rule as /signup).
    //    Count chars, not bytes — multibyte passphrases aren't penalised.
    if form.password.chars().count() < 15 {
        return render_form(
            &cfg,
            &form.token,
            Some("password must be at least 15 characters"),
        );
    }

    // 3. Hash on spawn_blocking — Argon2id is CPU-bound and synchronous;
    //    parking the ntex event loop is a non-starter (same constraint as
    //    /login and /signup).
    let password_clone = form.password.clone();
    let phc = match compio::runtime::spawn_blocking(move || password::hash(&password_clone)).await
    {
        Ok(Ok(p)) => p,
        Ok(Err(e)) => {
            tracing::error!(error = %e, "password_reset hash failed");
            return render_form(&cfg, &form.token, Some("internal error"));
        }
        Err(_) => {
            tracing::error!("password_reset hash spawn_blocking panicked");
            return render_form(&cfg, &form.token, Some("internal error"));
        }
    };

    // 4. Atomically consume the reset token with the password update,
    //    then audit, revoke existing sessions, and consume outstanding
    //    email tokens in one transaction.
    let completed = match complete_password_reset(db.as_ref(), &form.token, &phc, &req).await {
        Ok(Some(completed)) => completed,
        Ok(None) => {
            audit::emit(
                db.as_ref(),
                &AuditEvent {
                    event_type: "password_changed",
                    outcome: "failure",
                    auth_method: Some("password_reset"),
                    detail: serde_json::json!({ "reason": "token_invalid_or_expired" }),
                    ..AuditEvent::from_request(&req)
                },
            )
            .await;
            return render_form(&cfg, &form.token, Some("reset link invalid or expired"));
        }
        Err(e) => {
            tracing::error!(error = %e, "password_reset completion failed");
            return render_form(&cfg, &form.token, Some("internal error"));
        }
    };
    let revoked = completed.counts;

    tracing::info!(
        user_id = %completed.user_id,
        idp_sessions = revoked.idp_sessions,
        gateway_sessions = revoked.gateway_sessions,
        magic_tokens = revoked.magic_tokens,
        magic_completions = revoked.magic_completions,
        "password_reset revoked sessions and stale tokens"
    );

    // 7. Redirect to /login. The user signs in fresh with the new
    //    credential — we intentionally don't auto-mint a session here
    //    (a reset link clicked from a different browser shouldn't
    //    silently log you in on that other browser).
    let mut resp = HttpResponse::Found();
    resp.header(LOCATION, HeaderValue::from_static("/login"));
    resp.finish()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ResetRevocationCounts {
    idp_sessions: u64,
    gateway_sessions: u64,
    magic_tokens: u64,
    magic_completions: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResetCompletion {
    user_id: uuid::Uuid,
    email: String,
    counts: ResetRevocationCounts,
}

async fn complete_password_reset(
    conn: &compio_postgres::Client,
    raw_token: &str,
    phc: &str,
    req: &HttpRequest,
) -> Result<Option<ResetCompletion>> {
    conn.execute("BEGIN", &[])
        .await
        .map_err(|e| AuthError::Db(format!("password_reset begin: {e}")))?;

    let result = complete_password_reset_tx(conn, raw_token, phc, req).await;
    match result {
        Ok(Some(completed)) => {
            conn.execute("COMMIT", &[])
                .await
                .map_err(|e| AuthError::Db(format!("password_reset commit: {e}")))?;
            Ok(Some(completed))
        }
        Ok(None) => {
            if let Err(rollback_err) = conn.execute("ROLLBACK", &[]).await {
                tracing::error!(error = %rollback_err, "password_reset rollback after invalid token failed");
            }
            Ok(None)
        }
        Err(e) => {
            if let Err(rollback_err) = conn.execute("ROLLBACK", &[]).await {
                tracing::error!(error = %rollback_err, "password_reset rollback failed");
            }
            Err(e)
        }
    }
}

async fn complete_password_reset_tx(
    conn: &compio_postgres::Client,
    raw_token: &str,
    phc: &str,
    req: &HttpRequest,
) -> Result<Option<ResetCompletion>> {
    let Some(completed) = password_reset::complete(conn, raw_token, phc).await? else {
        return Ok(None);
    };

    audit::emit_strict(
        conn,
        &AuditEvent {
            event_type: "password_changed",
            outcome: "success",
            user_id: Some(&completed.user_id),
            auth_method: Some("password_reset"),
            ..AuditEvent::from_request(&req)
        },
    )
    .await?;

    let idp_sessions = conn
        .execute(
            "DELETE FROM zeroship.idp_sessions WHERE user_id = $1",
            &[&completed.user_id],
        )
        .await
        .map_err(|e| AuthError::Db(format!("password_reset delete zeroship.idp_sessions: {e}")))?;

    let gateway_sessions = conn
        .execute(
            "DELETE FROM zeroship.gateway_sessions WHERE user_id = $1",
            &[&completed.user_id],
        )
        .await
        .map_err(|e| AuthError::Db(format!("password_reset delete gateway_sessions: {e}")))?;

    let magic_tokens = conn
        .execute(
            "UPDATE zeroship.magic_links \
             SET consumed_at = NOW() \
             WHERE email = $1::citext \
               AND consumed_at IS NULL",
            &[&completed.email],
        )
        .await
        .map_err(|e| AuthError::Db(format!("password_reset consume magic links: {e}")))?;

    let magic_completions = conn
        .execute(
            "DELETE FROM zeroship.magic_completions WHERE email = $1::citext",
            &[&completed.email],
        )
        .await
        .map_err(|e| AuthError::Db(format!("password_reset delete magic_completions: {e}")))?;

    let counts = ResetRevocationCounts {
        idp_sessions,
        gateway_sessions,
        magic_tokens,
        magic_completions,
    };

    audit::emit_strict(
        conn,
        &AuditEvent {
            event_type: "sessions_revoked_after_password_reset",
            outcome: "success",
            user_id: Some(&completed.user_id),
            auth_method: Some("password_reset"),
            detail: serde_json::json!({
                "idp_sessions": counts.idp_sessions,
                "gateway_sessions": counts.gateway_sessions,
                "magic_tokens": counts.magic_tokens,
                "magic_completions": counts.magic_completions,
            }),
            ..AuditEvent::from_request(&req)
        },
    )
    .await?;

    Ok(Some(ResetCompletion {
        user_id: completed.user_id,
        email: completed.email,
        counts,
    }))
}

fn render_form(cfg: &AuthConfig, token: &str, error: Option<&str>) -> HttpResponse {
    let csrf_token = csrf::generate_token();
    // Independent per-response CSP script nonce — must NOT be the CSRF token
    // (which is also a non-HttpOnly cookie + plaintext form field). See L3.
    let script_nonce = csrf::generate_token();
    let page = ResetPage {
        token,
        csrf: &csrf_token,
        script_nonce: &script_nonce,
        error,
    };
    let body = page
        .render()
        .unwrap_or_else(|_| "<h1>error</h1>".to_string());
    let mut resp = HttpResponse::Ok();
    resp.content_type("text/html; charset=utf-8");
    resp.header("Cache-Control", "no-store");
    resp.header("Pragma", "no-cache");
    resp.header(
        "Content-Security-Policy",
        headers::content_security_policy_with_script_nonce(&script_nonce),
    );
    resp.header(SET_COOKIE, csrf::set_cookie(&csrf_token, cfg.insecure_dev));
    resp.body(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression for the `/reset?t=…` → `/reset?token=…` rename
    /// (the auth handlers were inconsistent: `/link` used `?token=`,
    /// every other token-redeem handler used `?t=`). The query struct
    /// MUST reject the old name — otherwise we'd silently keep a
    /// back-compat alias in place.
    ///
    /// `ResetQuery` is a serde-derived struct deserialised from
    /// `application/x-www-form-urlencoded` query strings. We exercise it
    /// directly with `url::form_urlencoded` so the test stays a unit
    /// test (no live server, no DB).
    #[test]
    fn reset_query_accepts_token_param_and_rejects_legacy_t_param() {
        fn parse(q: &str) -> std::result::Result<ResetQuery, serde::de::value::Error> {
            use serde::Deserialize;
            // Mirror the way ntex's Query<T> extractor decodes the URL
            // query: pairs → MapDeserializer → T.
            let pairs: Vec<(String, String)> =
                url::form_urlencoded::parse(q.as_bytes())
                    .into_owned()
                    .collect();
            let de = serde::de::value::MapDeserializer::new(pairs.into_iter());
            ResetQuery::deserialize(de)
        }

        // `?token=…` parses.
        let q = parse("token=abc").expect("token= must parse");
        assert_eq!(q.token, "abc");

        // The legacy `?t=…` must NOT parse — the field is `token`, not `t`.
        let legacy = parse("t=abc");
        assert!(
            legacy.is_err(),
            "legacy ?t= must not deserialize into ResetQuery; got {legacy:?}"
        );
    }

    /// L3 regression: the `/reset` inline `<script nonce>` must carry an
    /// independent per-response CSP nonce, NOT the CSRF token. The render
    /// helper (`render_form`) generates a fresh nonce; the template must
    /// emit it (not `{{ csrf }}`).
    #[test]
    fn reset_page_script_nonce_is_independent_of_csrf() {
        use askama::Template;

        let csrf = "csrf-double-submit-token-value";
        let script_nonce = "independent-csp-script-nonce";
        let page = ResetPage {
            token: "tok_abc",
            csrf,
            script_nonce,
            error: None,
        };

        let html = page.render().expect("reset page renders");

        assert!(
            html.contains(&format!("nonce=\"{script_nonce}\"")),
            "inline <script> must carry the independent script nonce"
        );
        assert!(
            !html.contains(&format!("nonce=\"{csrf}\"")),
            "inline <script> must NOT reuse the CSRF token as its nonce"
        );
    }
}
