//! `/reset` GET + POST handlers.
//!
//! GET renders the new-password form with the reset token in a hidden
//! field. POST validates CSRF + password length, atomically redeems the
//! reset token (single-use), Argon2-hashes the new password on a
//! `spawn_blocking` worker (the event loop stays free), updates
//! `auth.users.password_hash`, emits a `password_changed` audit event,
//! and redirects to `/login`.
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
use crate::identity::{password, password_reset};
use crate::store::users;
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
/// reset token, hash the new password, update the user, audit,
/// redirect to `/login`.
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

    // 3. Atomic single-use redeem.
    let redeemed = match password_reset::redeem(db.as_ref(), &form.token).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            audit::emit(
                db.as_ref(),
                &AuditEvent {
                    event_type: "password_changed",
                    outcome: "failure",
                    auth_method: Some("password_reset"),
                    detail: serde_json::json!({ "reason": "token_invalid_or_expired" }),
                    ..Default::default()
                },
            )
            .await;
            return render_form(&cfg, &form.token, Some("reset link invalid or expired"));
        }
        Err(e) => {
            tracing::error!(error = %e, "password_reset redeem db error");
            return render_form(&cfg, &form.token, Some("internal error"));
        }
    };

    // 4. Look up the user by the email recovered from the token.
    let user = match users::find_by_email(db.as_ref(), &redeemed.email).await {
        Ok(Some(u)) => u,
        Ok(None) => {
            tracing::warn!(email = %redeemed.email, "password_reset: redeemed token but user not found");
            return render_form(&cfg, &form.token, Some("user not found"));
        }
        Err(e) => {
            tracing::error!(error = %e, "password_reset users::find_by_email failed");
            return render_form(&cfg, &form.token, Some("internal error"));
        }
    };

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

    // 6. Persist the new hash.
    if let Err(e) = users::update_password_hash(db.as_ref(), user.id, &phc).await {
        tracing::error!(error = %e, user_id = %user.id, "password_reset update failed");
        return render_form(&cfg, &form.token, Some("internal error"));
    }

    // 7. Audit `password_changed` success.
    audit::emit(
        db.as_ref(),
        &AuditEvent {
            event_type: "password_changed",
            outcome: "success",
            user_id: Some(&user.id),
            auth_method: Some("password_reset"),
            ..Default::default()
        },
    )
    .await;

    // 8. Redirect to /login. The user signs in fresh with the new
    //    credential — we intentionally don't auto-mint a session here
    //    (a reset link clicked from a different browser shouldn't
    //    silently log you in on that other browser).
    let mut resp = HttpResponse::Found();
    resp.header(LOCATION, HeaderValue::from_static("/login"));
    resp.finish()
}

fn render_form(cfg: &AuthConfig, token: &str, error: Option<&str>) -> HttpResponse {
    let csrf_token = csrf::generate_token();
    let page = ResetPage {
        token,
        csrf: &csrf_token,
        error,
    };
    let body = page
        .render()
        .unwrap_or_else(|_| "<h1>error</h1>".to_string());
    let mut resp = HttpResponse::Ok();
    resp.content_type("text/html; charset=utf-8");
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
        fn parse(q: &str) -> Result<ResetQuery, serde::de::value::Error> {
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
}
