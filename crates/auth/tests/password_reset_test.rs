//! Live-PG roundtrip for `auth::identity::password_reset`.
//!
//! Skipped unless `AUTH_DB_URL` is set. Each test scopes itself with a
//! random email so concurrent runs don't collide; the cleanup at the end
//! removes every row the test inserted.

use std::sync::Arc;
use std::sync::Mutex;

use clap::Parser;
use compio_postgres::{connect, Client, NoTls};
use ntex::http::header::SET_COOKIE;
use ntex::web::{self, test};
use serde::Deserialize;
use uuid::Uuid;

use zeroship_auth::config::AuthConfig;
use zeroship_auth::hydra_client::HydraAdmin;
use zeroship_auth::identity::{magic_link, password, password_reset};
use zeroship_auth::store::{sessions, users};

fn test_cfg(db_url: &str) -> AuthConfig {
    let mut cfg = AuthConfig::parse_from([
        "zeroship-auth",
        "--db-url",
        db_url,
        "--dev-insecure",
        "--stash-signing-key",
        "test-stash-key-not-for-prod-32bytes!",
    ]);
    cfg.resolve(zeroship_core::config::AuthSection::default());
    cfg
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

#[derive(Debug, Default)]
struct MockHydraState {
    deleted_subjects: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct DeleteLoginSessionsQuery {
    subject: String,
}

#[allow(clippy::future_not_send)]
async fn mock_delete_login_sessions(
    query: web::types::Query<DeleteLoginSessionsQuery>,
    state: web::types::State<Arc<Mutex<MockHydraState>>>,
) -> web::HttpResponse {
    state
        .lock()
        .expect("lock hydra state")
        .deleted_subjects
        .push(query.subject.clone());
    web::HttpResponse::NoContent().finish()
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
    Some(client)
}

async fn pg_connect(dsn: &str) -> Client {
    let (client, connection) = connect(dsn, NoTls).await.expect("connect");
    compio::runtime::spawn(async move {
        if let Err(e) = connection.run().await {
            eprintln!("password_reset test pg connection error: {e}");
        }
    })
    .detach();
    client
}

async fn install_magic_links_insert_delay(client: &Client) {
    client
        .execute(
            "CREATE OR REPLACE FUNCTION auth.test_sleep_before_magic_link_insert() \
             RETURNS trigger LANGUAGE plpgsql AS $$ \
             BEGIN \
                 PERFORM pg_sleep(0.2); \
                 RETURN NEW; \
             END \
             $$",
            &[],
        )
        .await
        .expect("create insert delay function");
    client
        .execute(
            "DROP TRIGGER IF EXISTS test_sleep_before_magic_link_insert ON auth.magic_links",
            &[],
        )
        .await
        .expect("drop stale insert delay trigger");
    client
        .execute(
            "CREATE TRIGGER test_sleep_before_magic_link_insert \
             BEFORE INSERT ON auth.magic_links \
             FOR EACH ROW EXECUTE FUNCTION auth.test_sleep_before_magic_link_insert()",
            &[],
        )
        .await
        .expect("create insert delay trigger");
}

async fn drop_magic_links_insert_delay(client: &Client) {
    client
        .execute(
            "DROP TRIGGER IF EXISTS test_sleep_before_magic_link_insert ON auth.magic_links",
            &[],
        )
        .await
        .ok();
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
            expected_credential_version: None,
            idle_minutes: 30,
            absolute_hours: 12,
        },
    )
    .await
    .expect("seed idp session");

    client
        .execute(
            "INSERT INTO auth.gateway_sessions \
                (user_id, app_id, email, name, email_verified, idle_expires_at, abs_expires_at) \
             VALUES ($1, $2, $3::citext, $4, true, NOW() + INTERVAL '30 minutes', NOW() + INTERVAL '12 hours')",
            &[&user.id, &"app_reset_revoke_test", &email, &"Test"],
        )
        .await
        .expect("seed gateway session");
    client
        .execute(
            "INSERT INTO auth.console_sessions \
                (user_id, email, name, email_verified, idle_expires_at, abs_expires_at) \
             VALUES ($1, $2::citext, $3, true, NOW() + INTERVAL '30 minutes', NOW() + INTERVAL '12 hours')",
            &[&user.id, &email, &"Test"],
        )
        .await
        .expect("seed console session");

    let issued = password_reset::issue(&client, &email)
        .await
        .expect("issue reset token");

    let hydra_state = Arc::new(Mutex::new(MockHydraState::default()));
    let hydra_state_for_srv = hydra_state.clone();
    let hydra_srv = web::test::server(move || {
        let hydra_state = hydra_state_for_srv.clone();
        async move {
            web::App::new().state(hydra_state).service(
                web::resource("/admin/oauth2/auth/sessions/login")
                    .route(web::delete().to(mock_delete_login_sessions)),
            )
        }
    })
    .await;
    let admin = HydraAdmin::new(hydra_srv.url("").trim_end_matches('/').to_string());

    let pg = Arc::new(client);
    let cfg = Arc::new(test_cfg(&dsn));
    let cfg_state = cfg.clone();
    let pg_state = pg.clone();
    let admin_state = admin.clone();
    let app = test::init_service(
        web::App::new().state(cfg_state).state(pg_state).state(admin_state).service(
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

    let deleted_subjects = hydra_state
        .lock()
        .expect("lock hydra state")
        .deleted_subjects
        .clone();
    assert_eq!(
        deleted_subjects,
        vec![user.id.to_string()],
        "password reset must delete hydra login sessions for the reset subject"
    );

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
            &[&user.id],
        )
        .await
        .expect("count gateway sessions")
        .get(0);
    let console_count: i64 = pg
        .query_one(
            "SELECT COUNT(*) FROM auth.console_sessions WHERE user_id = $1",
            &[&user.id],
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
#[allow(clippy::future_not_send)]
async fn reset_post_consumes_magic_login_state_for_same_email() {
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
        "reset-consume-magic-{}@zeroship.test",
        Uuid::new_v4().simple()
    );
    let user = users::create(&client, &email, "Test", None)
        .await
        .expect("seed user");
    let magic = magic_link::issue(&client, &email, "login")
        .await
        .expect("issue magic login");
    let reset = password_reset::issue(&client, &email)
        .await
        .expect("issue reset token");

    let csrf_nonce = format!("reset-clears-completion-{}", Uuid::new_v4().simple());
    client
        .execute(
            "INSERT INTO auth.magic_completions \
                (csrf_nonce, code, email, login_challenge, expires_at) \
             VALUES ($1, $2, $3::citext, $4, NOW() + INTERVAL '5 minutes')",
            &[&csrf_nonce, &"123456", &email, &"lc-reset-clears-completion"],
        )
        .await
        .expect("seed magic completion");

    let hydra_state = Arc::new(Mutex::new(MockHydraState::default()));
    let hydra_state_for_srv = hydra_state.clone();
    let hydra_srv = web::test::server(move || {
        let hydra_state = hydra_state_for_srv.clone();
        async move {
            web::App::new().state(hydra_state).service(
                web::resource("/admin/oauth2/auth/sessions/login")
                    .route(web::delete().to(mock_delete_login_sessions)),
            )
        }
    })
    .await;
    let admin = HydraAdmin::new(hydra_srv.url("").trim_end_matches('/').to_string());

    let pg = Arc::new(client);
    let cfg = Arc::new(test_cfg(&dsn));
    let app = test::init_service(
        web::App::new()
            .state(cfg.clone())
            .state(pg.clone())
            .state(admin)
            .service(
                web::resource("/reset")
                    .route(web::get().to(zeroship_auth::ui::reset::get))
                    .route(web::post().to(zeroship_auth::ui::reset::post)),
            ),
    )
    .await;

    let get_req =
        test::TestRequest::get().uri(&format!("/reset?token={}", reset.raw)).to_request();
    let get_resp = test::call_service(&app, get_req).await;
    assert_eq!(get_resp.status().as_u16(), 200);
    let csrf = read_set_cookie(get_resp.headers(), "zsidp_csrf")
        .expect("zsidp_csrf cookie set on GET /reset");

    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("csrf", &csrf)
        .append_pair("token", &reset.raw)
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

    let old_magic = magic_link::redeem_pending(pg.as_ref(), &magic.raw)
        .await
        .expect("redeem old magic login after reset");
    assert!(
        old_magic.is_none(),
        "password reset must consume outstanding login-purpose magic links"
    );

    let completions_left: i64 = pg
        .query_one(
            "SELECT COUNT(*) FROM auth.magic_completions WHERE email = $1::citext",
            &[&email],
        )
        .await
        .expect("count magic completions")
        .get(0);
    assert_eq!(
        completions_left, 0,
        "password reset must clear cross-device magic completions for the email"
    );

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
async fn concurrent_issue_leaves_one_active_reset_token() {
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
        "reset-concurrent-{}@zeroship.test",
        Uuid::new_v4().simple()
    );
    let user = users::create(&client, &email, "Test", None)
        .await
        .expect("seed user");
    install_magic_links_insert_delay(&client).await;

    let client_a = pg_connect(&dsn).await;
    let client_b = pg_connect(&dsn).await;
    let email_a = email.clone();
    let email_b = email.clone();
    let issue_a =
        compio::runtime::spawn(async move { password_reset::issue(&client_a, &email_a).await });
    let issue_b =
        compio::runtime::spawn(async move { password_reset::issue(&client_b, &email_b).await });

    issue_a.await.expect("join issue A").expect("issue A");
    issue_b.await.expect("join issue B").expect("issue B");

    drop_magic_links_insert_delay(&client).await;

    let active_count: i64 = client
        .query_one(
            "SELECT COUNT(*) FROM auth.magic_links \
             WHERE email = $1::citext AND purpose = 'reset' AND consumed_at IS NULL",
            &[&email],
        )
        .await
        .expect("count active reset links")
        .get(0);
    assert_eq!(
        active_count, 1,
        "concurrent issue must leave exactly one active reset token"
    );

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
async fn complete_rolls_back_token_consume_with_transaction() {
    let Some(client) = pg().await else {
        eprintln!("skipping password_reset_test (no AUTH_DB_URL)");
        return;
    };

    let email = format!(
        "reset-rollback-{}@zeroship.test",
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
    let issued = password_reset::issue(&client, &email)
        .await
        .expect("issue reset token");
    let new_hash = password::hash("new reset password phrase")
        .expect("hash new password");

    client.execute("BEGIN", &[]).await.expect("begin");
    let completed = password_reset::complete(&client, &issued.raw, &new_hash)
        .await
        .expect("complete reset")
        .expect("token should complete inside transaction");
    assert_eq!(completed.user_id, user.id);
    client.execute("ROLLBACK", &[]).await.expect("rollback");

    let row = client
        .query_one(
            "SELECT ml.consumed_at IS NULL AS token_unconsumed, \
                    u.password_hash = $2 AS password_unchanged \
             FROM auth.magic_links ml \
             JOIN auth.users u ON u.email = ml.email \
             WHERE ml.email = $1::citext AND ml.purpose = 'reset'",
            &[&email, &old_hash],
        )
        .await
        .expect("load reset state");
    let token_unconsumed: bool = row.get("token_unconsumed");
    let password_unchanged: bool = row.get("password_unchanged");
    assert!(
        token_unconsumed,
        "rolled-back password reset must leave token unconsumed"
    );
    assert!(
        password_unchanged,
        "rolled-back password reset must leave password hash unchanged"
    );

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
