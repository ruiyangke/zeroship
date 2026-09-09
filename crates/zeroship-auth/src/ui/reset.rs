//! `/reset` GET + POST handlers.
//!
//! GET renders the new-password form with the reset token in a hidden
//! field. POST validates CSRF + password length, rate-limits per IP,
//! declines a token that has no live row, Argon2-hashes the new password on
//! a `spawn_blocking` worker (the event loop stays free), atomically redeems
//! the reset token (single-use) together with the password update, emits a
//! `password_changed` audit event, revokes every existing session, consumes
//! outstanding email tokens, clears cross-device magic completions, and
//! redirects to `/login`.
//!
//! The limiter and the token pre-check both sit ahead of the hash on
//! purpose. Argon2id at 19 MiB plus a slot in the blocking pool shared with
//! `/login` and `/link` is far too much to spend on a request that a garbage
//! token was always going to lose, and the CSRF pair a GET hands out is
//! reusable (double-submit), so nothing else bounds the flood.
//!
//! Note: GET does not "peek" at the token. Token validity is checked
//! only at POST time. The form might render against an already-expired
//! token; the user will see "reset link invalid or expired" on submit.
//! The alternative — validating at GET and again at POST — costs an extra
//! DB round-trip per render and the 1-hour TTL + 32-byte CSPRNG entropy
//! makes pre-emptive feedback unnecessary.

use askama::Template;
use ntex::http::header::{HeaderValue, COOKIE, LOCATION, SET_COOKIE};
use ntex::http::StatusCode;
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
use zeroship_authn::rate_limit::{self, Quota, RateLimitDecision};
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

/// `/reset` POST: validate CSRF + length, rate-limit, decline a dead token,
/// hash the new password, atomically redeem the reset token together with the
/// password update, audit, revoke existing sessions, consume pending email
/// tokens, and redirect to `/login`.
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
    let cookie_token = csrf::parse_cookie(cookie_header);
    if cookie_token
        .as_deref()
        .is_none_or(|c| !csrf::matches(&form.csrf, c))
    {
        return render_form(&cfg, &form.token, Some("invalid request"));
    }

    // 2. NIST 800-63B Rev 4: 15-character minimum (same rule as /signup).
    //    Count chars, not bytes — multibyte passphrases aren't penalised.
    if form.password.chars().count() < crate::identity::password::MIN_PASSWORD_CHARS {
        return render_form(
            &cfg,
            &form.token,
            Some("password must be at least 15 characters"),
        );
    }

    // 3. Rate-limit per IP before the CPU-bound hash, the same placement
    //    /signup and /link use. Keyed on the forwarded client IP (auth runs
    //    behind the gateway, so the socket peer is the gateway and keying on
    //    it would make this one global bucket).
    let ip = headers::client_ip(&req);
    let reset_ip_key = format!("reset_ip:{ip}");
    match rate_limit::consume(db.as_ref(), &reset_ip_key, Quota::RESET_IP).await {
        Ok(RateLimitDecision::Allowed) => {}
        Ok(RateLimitDecision::Throttled(_)) => {
            audit::emit(
                db.as_ref(),
                &AuditEvent {
                    event_type: "password_reset_throttled",
                    outcome: "failure",
                    auth_method: Some("password_reset"),
                    detail: serde_json::json!({ "bucket": "reset_per_ip" }),
                    ..AuditEvent::from_request(&req)
                },
            )
            .await;
            return render_form_with_status(
                &cfg,
                &form.token,
                Some("too many attempts, try again later"),
                StatusCode::TOO_MANY_REQUESTS,
            );
        }
        Err(e) => {
            tracing::error!(error = %e, bucket = %reset_ip_key, "password_reset rate-limit consume failed");
            return render_form(&cfg, &form.token, Some("internal error"));
        }
    }

    // 4. Decline a token that already has no live row, before paying for the
    //    hash. Step 6 is still the authority: it consumes the token and sets
    //    the password in one statement, and a token that passes here but
    //    loses that race is rejected there. This only means a flood of
    //    never-issued tokens costs a primary-key lookup instead of 19 MiB and
    //    a blocking-pool slot shared with /login and /link.
    match password_reset::is_live(db.as_ref(), &form.token).await {
        Ok(true) => {}
        Ok(false) => return reject_dead_token(db.as_ref(), &cfg, &form.token, &req).await,
        Err(e) => {
            tracing::error!(error = %e, "password_reset token pre-check failed");
            return render_form(&cfg, &form.token, Some("internal error"));
        }
    }

    // 5. Hash on spawn_blocking — Argon2id is CPU-bound and synchronous;
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

    // 6. Atomically consume the reset token with the password update,
    //    then audit, revoke existing sessions, and consume outstanding
    //    email tokens in one transaction.
    let completed = match complete_password_reset(db.as_ref(), &form.token, &phc, &req).await {
        Ok(Some(completed)) => completed,
        Ok(None) => return reject_dead_token(db.as_ref(), &cfg, &form.token, &req).await,
        Err(e) => {
            tracing::error!(error = %e, "password_reset completion failed");
            return render_form(&cfg, &form.token, Some("internal error"));
        }
    };
    let revoked = completed.counts;

    tracing::info!(
        user_id = completed.user_id.as_str(),
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
    user_id: zeroship_core::user_id::UserId,
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
            ..AuditEvent::from_request(req)
        },
    )
    .await?;

    let idp_sessions = conn
        .execute(
            "DELETE FROM zeroship.idp_sessions WHERE user_id = $1",
            &[&completed.user_id.as_str()],
        )
        .await
        .map_err(|e| AuthError::Db(format!("password_reset delete zeroship.idp_sessions: {e}")))?;

    let gateway_sessions = conn
        .execute(
            "DELETE FROM zeroship.gateway_sessions WHERE user_id = $1",
            &[&completed.user_id.as_str()],
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
            ..AuditEvent::from_request(req)
        },
    )
    .await?;

    Ok(Some(ResetCompletion {
        user_id: completed.user_id,
        email: completed.email,
        counts,
    }))
}

/// Refuse a reset token that has no live row. Both the pre-check and the
/// atomic consume land here, so the two arms are indistinguishable from
/// outside: same audit event, same page, same status. Only the cost differs.
async fn reject_dead_token(
    db: &compio_postgres::Client,
    cfg: &AuthConfig,
    token: &str,
    req: &HttpRequest,
) -> HttpResponse {
    audit::emit(
        db,
        &AuditEvent {
            event_type: "password_changed",
            outcome: "failure",
            auth_method: Some("password_reset"),
            detail: serde_json::json!({ "reason": "token_invalid_or_expired" }),
            ..AuditEvent::from_request(req)
        },
    )
    .await;
    render_form(cfg, token, Some("reset link invalid or expired"))
}

fn render_form(cfg: &AuthConfig, token: &str, error: Option<&str>) -> HttpResponse {
    render_form_with_status(cfg, token, error, StatusCode::OK)
}

fn render_form_with_status(
    _cfg: &AuthConfig,
    token: &str,
    error: Option<&str>,
    status: StatusCode,
) -> HttpResponse {
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
    let mut resp = HttpResponse::build(status);
    resp.content_type("text/html; charset=utf-8");
    resp.header("Cache-Control", "no-store");
    resp.header("Pragma", "no-cache");
    resp.header(
        "Content-Security-Policy",
        headers::content_security_policy_with_script_nonce(&script_nonce),
    );
    resp.header(SET_COOKIE, csrf::set_cookie(&csrf_token));
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
