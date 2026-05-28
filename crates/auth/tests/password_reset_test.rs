//! Live-PG roundtrip for `auth::identity::password_reset`.
//!
//! Skipped unless `AUTH_DB_URL` is set. Each test scopes itself with a
//! random email so concurrent runs don't collide; the cleanup at the end
//! removes every row the test inserted.

use std::sync::Arc;

use clap::Parser;
use compio_postgres::{connect, NoTls};
use ntex::http::header::SET_COOKIE;
use ntex::web::{self, test};
use uuid::Uuid;

use zeroship_auth::config::AuthConfig;
use zeroship_auth::identity::password;
use zeroship_auth::identity::password_reset;
use zeroship_auth::store::{migrations, sessions, users};

fn test_cfg(db_url: &str) -> AuthConfig {
    AuthConfig::parse_from([
        "zeroship-auth",
        "--db-url",
        db_url,
        "--insecure-dev",
        "--stash-signing-key",
        "test-stash-key-not-for-prod-32bytes!",
    ])
}

fn read_set_cookie(headers: &ntex::http::HeaderMap, name: &str) -> Option<String> {
    for hv in headers.get_all(SET_COOKIE) {
        let Ok(s) = hv.to_str() else { continue };
        let Some((cookie_name, rest)) = s.split_once('=') else {
            continue;
        };
        if cookie_name == name {
            return Some(rest.split(';').next().unwrap_or("").to_string());
        }
    }
    None
}

// `compio_postgres::Client` is `!Send` — the futures inherit that
// structurally. The lint is informational, not actionable here.
#[allow(clippy::future_not_send)]
async fn pg() -> Option<compio_postgres::Client> {
    let dsn = std::env::var("AUTH_DB_URL").ok()?;
    let (client, connection) = connect(&dsn, NoTls).await.expect("connect");
    compio::runtime::spawn(async move {
        if let Err(e) = connection.run().await {
            eprintln!("password_reset test pg connection error: {e}");
        }
    })
    .detach();
    migrations::migrate(&client).await.expect("migrate");
    Some(client)
}

#[compio::test]
#[allow(clippy::future_not_send)]
async fn reset_post_revokes_all_sessions_and_audits_counts() {
    let dsn = match std::env::var("AUTH_DB_URL") {
        Ok(dsn) => dsn,
        Err(_) => {
            eprintln!("skipping password_reset_test (no AUTH_DB_URL)");
            return;
        }
    };
    let Some(client) = pg().await else {
        eprintln!("skipping password_reset_test (no AUTH_DB_URL)");
        return;
    };

    let email = format!(
        "reset-revoke-sessions-{}@zeroship.test",
        Uuid::new_v4().simple()
    );
    let user = users::create(&client, &email, "Test", None)
        .await
        .expect("seed user");
    let old_hash = password::hash("old reset password phrase")
        .expect("hash old password");
    users::update_password_hash(&client, user.id, &old_hash)
        .await
        .expect("set old password");

    sessions::create(
        &client,
        &sessions::CreateSession {
            user_id: user.id,
            auth_method: "password",
            amr: vec!["pwd".to_string()],
            acr: None,
            idle_minutes: 30,
            absolute_hours: 12,
        },
    )
    .await
    .expect("seed idp session");

    let user_id_text = user.id.to_string();
    client
        .execute(
            "INSERT INTO auth.gateway_sessions \
                (user_id, app_id, email, name, email_verified, idle_expires_at, abs_expires_at) \
             VALUES ($1, $2, $3::citext, $4, true, NOW() + INTERVAL '30 minutes', NOW() + INTERVAL '12 hours')",
            &[&user_id_text, &"app_reset_revoke_test", &email, &"Test"],
        )
        .await
        .expect("seed gateway session");
    client
        .execute(
            "INSERT INTO auth.console_sessions \
                (user_id, email, name, email_verified, idle_expires_at, abs_expires_at) \
             VALUES ($1, $2::citext, $3, true, NOW() + INTERVAL '30 minutes', NOW() + INTERVAL '12 hours')",
            &[&user_id_text, &email, &"Test"],
        )
        .await
        .expect("seed console session");

    let issued = password_reset::issue(&client, &email)
        .await
        .expect("issue reset token");

    let pg = Arc::new(client);
    let cfg = Arc::new(test_cfg(&dsn));
    let cfg_state = cfg.clone();
    let pg_state = pg.clone();
    let app = test::init_service(
        web::App::new().state(cfg_state).state(pg_state).service(
            web::resource("/reset")
                .route(web::get().to(zeroship_auth::ui::reset::get))
                .route(web::post().to(zeroship_auth::ui::reset::post)),
        ),
    )
    .await;

    let get_req =
        test::TestRequest::get().uri(&format!("/reset?token={}", issued.raw)).to_request();
    let get_resp = test::call_service(&app, get_req).await;
    assert_eq!(get_resp.status().as_u16(), 200);
    let csrf = read_set_cookie(get_resp.headers(), "zsidp_csrf")
        .expect("zsidp_csrf cookie set on GET /reset");

    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("csrf", &csrf)
        .append_pair("token", &issued.raw)
        .append_pair("password", "new reset password phrase")
        .finish();
    let post_req = test::TestRequest::post()
        .uri("/reset")
        .header("content-type", "application/x-www-form-urlencoded")
        .header("cookie", format!("zsidp_csrf={csrf}"))
        .set_payload(body)
        .to_request();
    let post_resp = test::call_service(&app, post_req).await;
    assert_eq!(post_resp.status().as_u16(), 302);

    let idp_count: i64 = pg
        .query_one(
            "SELECT COUNT(*) FROM auth.sessions WHERE user_id = $1",
            &[&user.id],
        )
        .await
        .expect("count idp sessions")
        .get(0);
    let gateway_count: i64 = pg
        .query_one(
            "SELECT COUNT(*) FROM auth.gateway_sessions WHERE user_id = $1",
            &[&user_id_text],
        )
        .await
        .expect("count gateway sessions")
        .get(0);
    let console_count: i64 = pg
        .query_one(
            "SELECT COUNT(*) FROM auth.console_sessions WHERE user_id = $1",
            &[&user_id_text],
        )
        .await
        .expect("count console sessions")
        .get(0);
    assert_eq!(idp_count, 0, "IdP sessions must be deleted");
    assert_eq!(gateway_count, 0, "gateway sessions must be deleted");
    assert_eq!(console_count, 0, "console sessions must be deleted");

    let password_changed: i64 = pg
        .query_one(
            "SELECT COUNT(*) FROM auth.audit_events \
             WHERE user_id = $1 AND event_type = 'password_changed' AND outcome = 'success'",
            &[&user.id],
        )
        .await
        .expect("count password_changed audit")
        .get(0);
    let sessions_revoked: i64 = pg
        .query_one(
            "SELECT COUNT(*) FROM auth.audit_events \
             WHERE user_id = $1 AND event_type = 'sessions_revoked_after_password_reset' \
               AND outcome = 'success'",
            &[&user.id],
        )
        .await
        .expect("count sessions_revoked audit")
        .get(0);
    assert_eq!(password_changed, 1);
    assert_eq!(sessions_revoked, 1);

    pg.execute("DELETE FROM auth.audit_events WHERE user_id = $1", &[&user.id])
        .await
        .ok();
    pg.execute(
        "DELETE FROM auth.magic_links WHERE email = $1::citext",
        &[&email],
    )
    .await
    .ok();
    pg.execute("DELETE FROM auth.users WHERE id = $1", &[&user.id])
        .await
        .ok();
}

#[compio::test]
async fn issue_then_redeem_roundtrip() {
    let Some(client) = pg().await else {
        eprintln!("skipping password_reset_test (no AUTH_DB_URL)");
        return;
    };

    let email = format!("reset-{}@zeroship.test", Uuid::new_v4().simple());
    let user = users::create(&client, &email, "Test", None)
        .await
        .expect("seed user");

    let issued = password_reset::issue(&client, &email)
        .await
        .expect("issue");
    assert!(!issued.raw.is_empty(), "raw token must be non-empty");

    let redeemed = password_reset::redeem(&client, &issued.raw)
        .await
        .expect("redeem")
        .expect("first redeem should succeed");
    assert_eq!(
        redeemed.email.to_ascii_lowercase(),
        email.to_ascii_lowercase()
    );

    // Single-use: second redeem returns None.
    let second = password_reset::redeem(&client, &issued.raw)
        .await
        .expect("redeem 2");
    assert!(
        second.is_none(),
        "second redeem must return None (single-use)"
    );

    // Cleanup.
    client
        .execute(
            "DELETE FROM auth.magic_links WHERE email = $1::citext",
            &[&email],
        )
        .await
        .ok();
    client
        .execute("DELETE FROM auth.users WHERE id = $1", &[&user.id])
        .await
        .ok();
}

#[compio::test]
async fn new_issue_supersedes_previous_reset_token() {
    let Some(client) = pg().await else {
        eprintln!("skipping password_reset_test (no AUTH_DB_URL)");
        return;
    };

    let email = format!(
        "reset-supersede-{}@zeroship.test",
        Uuid::new_v4().simple()
    );
    let user = users::create(&client, &email, "Test", None)
        .await
        .expect("seed user");

    let first = password_reset::issue(&client, &email)
        .await
        .expect("issue 1");
    let second = password_reset::issue(&client, &email)
        .await
        .expect("issue 2");

    // First token must be invalidated by the second issue.
    let r1 = password_reset::redeem(&client, &first.raw)
        .await
        .expect("redeem first");
    assert!(
        r1.is_none(),
        "previous unconsumed reset token must be invalidated by a fresh issue"
    );

    // Second still works.
    let r2 = password_reset::redeem(&client, &second.raw)
        .await
        .expect("redeem second");
    assert!(r2.is_some(), "fresh reset token must still redeem");

    // Cleanup.
    client
        .execute(
            "DELETE FROM auth.magic_links WHERE email = $1::citext",
            &[&email],
        )
        .await
        .ok();
    client
        .execute("DELETE FROM auth.users WHERE id = $1", &[&user.id])
        .await
        .ok();
}
