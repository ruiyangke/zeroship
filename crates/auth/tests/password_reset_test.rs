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

async fn seed_test_plan(client: &Client) -> &'static str {
    let plan_id = "password-reset-test-plan";
    client
        .execute(
            "INSERT INTO zeroship.plans \
                 (id, name, base_fee_cents, included_units, spend_limit_default_cents, \
                  runtime_limits_json) \
             VALUES ($1, 'Password Reset Test Plan', 0, 0, 0, \
                     '{\"cpu_ms\":1000,\"wall_ms\":5000,\"memory_mb\":128,\"concurrency\":10}'::jsonb) \
             ON CONFLICT (id) DO NOTHING",
            &[&plan_id],
        )
        .await
        .expect("seed password reset test plan");
    plan_id
}

async fn install_magic_links_insert_delay(client: &Client) {
    client
        .execute(
            "CREATE OR REPLACE FUNCTION zeroship.test_sleep_before_magic_link_insert() \
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
            "DROP TRIGGER IF EXISTS test_sleep_before_magic_link_insert ON zeroship.magic_links",
            &[],
        )
        .await
        .expect("drop stale insert delay trigger");
    client
        .execute(
            "CREATE TRIGGER test_sleep_before_magic_link_insert \
             BEFORE INSERT ON zeroship.magic_links \
             FOR EACH ROW EXECUTE FUNCTION zeroship.test_sleep_before_magic_link_insert()",
            &[],
        )
        .await
        .expect("create insert delay trigger");
}

async fn drop_magic_links_insert_delay(client: &Client) {
    client
        .execute(
            "DROP TRIGGER IF EXISTS test_sleep_before_magic_link_insert ON zeroship.magic_links",
            &[],
        )
        .await
        .ok();
}

// Mock-hydra `web::test::server` needs the ntex runtime/System; run under
// `#[ntex::test]` not `#[compio::test]` ("System is not running" otherwise).
#[ntex::test]
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

    // gateway_sessions.app_id is UUID + FK → apps(id); seed a real app row.
    let gw_app_id = Uuid::new_v4();
    let plan_id = seed_test_plan(&client).await;
    client
        .execute(
            "INSERT INTO zeroship.apps (id, name, plan_id, api_key) VALUES ($1, $2, $3, $4)",
            &[
                &gw_app_id,
                &format!("reset-revoke-app-{}", gw_app_id.simple()),
                &plan_id,
                &"k",
            ],
        )
        .await
        .expect("seed app");
    client
        .execute(
            "INSERT INTO zeroship.gateway_sessions \
                (user_id, app_id, email, name, email_verified, idle_expires_at, abs_expires_at) \
             VALUES ($1, $2, $3::citext, $4, true, NOW() + INTERVAL '30 minutes', NOW() + INTERVAL '12 hours')",
            &[&user.id, &gw_app_id, &email, &"Test"],
        )
        .await
        .expect("seed gateway session");

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
            "SELECT COUNT(*) FROM zeroship.idp_sessions WHERE user_id = $1",
            &[&user.id],
        )
        .await
        .expect("count idp sessions")
        .get(0);
    let gateway_count: i64 = pg
        .query_one(
            "SELECT COUNT(*) FROM zeroship.gateway_sessions WHERE user_id = $1",
            &[&user.id],
        )
        .await
        .expect("count gateway sessions")
        .get(0);
    assert_eq!(idp_count, 0, "IdP sessions must be deleted");
    assert_eq!(gateway_count, 0, "gateway sessions must be deleted");

    let password_changed: i64 = pg
        .query_one(
            "SELECT COUNT(*) FROM zeroship.audit_events \
             WHERE actor_user_id = $1 AND event_type = 'password_changed' AND outcome = 'success'",
            &[&user.id],
        )
        .await
        .expect("count password_changed audit")
        .get(0);
    let sessions_revoked: i64 = pg
        .query_one(
            "SELECT COUNT(*) FROM zeroship.audit_events \
             WHERE actor_user_id = $1 AND event_type = 'sessions_revoked_after_password_reset' \
               AND outcome = 'success'",
            &[&user.id],
        )
        .await
        .expect("count sessions_revoked audit")
        .get(0);
    assert_eq!(password_changed, 1);
    assert_eq!(sessions_revoked, 1);

    pg.execute("DELETE FROM zeroship.audit_events WHERE actor_user_id = $1", &[&user.id])
        .await
        .ok();
    pg.execute(
        "DELETE FROM zeroship.magic_links WHERE email = $1::citext",
        &[&email],
    )
    .await
    .ok();
    pg.execute("DELETE FROM zeroship.users WHERE id = $1", &[&user.id])
        .await
        .ok();
    pg.execute("DELETE FROM zeroship.apps WHERE id = $1", &[&gw_app_id])
        .await
        .ok();
}

// Mock-hydra `web::test::server` needs the ntex runtime/System; see above.
#[ntex::test]
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
            "INSERT INTO zeroship.magic_completions \
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
            "SELECT COUNT(*) FROM zeroship.magic_completions WHERE email = $1::citext",
            &[&email],
        )
        .await
        .expect("count magic completions")
        .get(0);
    assert_eq!(
        completions_left, 0,
        "password reset must clear cross-device magic completions for the email"
    );

    pg.execute("DELETE FROM zeroship.audit_events WHERE actor_user_id = $1", &[&user.id])
        .await
        .ok();
    pg.execute(
        "DELETE FROM zeroship.magic_links WHERE email = $1::citext",
        &[&email],
    )
    .await
    .ok();
    pg.execute("DELETE FROM zeroship.users WHERE id = $1", &[&user.id])
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
            "SELECT COUNT(*) FROM zeroship.magic_links \
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
            "DELETE FROM zeroship.magic_links WHERE email = $1::citext",
            &[&email],
        )
        .await
        .ok();
    client
        .execute("DELETE FROM zeroship.users WHERE id = $1", &[&user.id])
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
            "DELETE FROM zeroship.magic_links WHERE email = $1::citext",
            &[&email],
        )
        .await
        .ok();
    client
        .execute("DELETE FROM zeroship.users WHERE id = $1", &[&user.id])
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
             FROM zeroship.magic_links ml \
             JOIN zeroship.users u ON u.email = ml.email \
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
            "DELETE FROM zeroship.magic_links WHERE email = $1::citext",
            &[&email],
        )
        .await
        .ok();
    client
        .execute("DELETE FROM zeroship.users WHERE id = $1", &[&user.id])
        .await
        .ok();
}

/// Regression for security finding H1: password reset must DURABLY terminate
/// the gateway app-session tier, not just the IdP login session.
///
/// Before the fix, `complete_password_reset_tx` deleted only `idp_sessions` +
/// `gateway_sessions` and called Hydra `delete_login_sessions`. It NEVER:
///   - wrote the `(client_id, pairwise_sub)` family marker into
///     `zeroship.token_revocations` (the SOLE gate the gateway's stateless
///     `__Host-zeroship_app_session` cookie consults), so a live app-session
///     cookie kept validating after the victim's reset; and
///   - revoked the user's `zeroship.app_session_anchors` rows, so the 30-day
///     `__Host-zeroship_app_anchor` survived and `GET /session?mint=1` could
///     re-mint a fresh cookie — resurrecting the session the reset was meant
///     to kill.
///
/// This drives the REAL `/reset` POST handler and asserts the three
/// teardown invariants the gateway relies on:
///   1. every anchor for the user is now `revoked_at IS NOT NULL`
///      (so `anchors::read_live` returns None → `?mint=1` fails closed),
///   2. a `token_revocations` family marker exists for the app's
///      `(client_id, pairwise_sub)` (so live cookies are rejected), and
///   3. `users.credential_version` was bumped (IdP-leg defense in depth).
///
/// Pre-fix this FAILS at assertion (1) (anchor stays live).
///
/// **Scope (security finding F1).** This is the AUTH-LEG unit: it pre-seeds the
/// `app_user_identities` row to verify the reset CTE *given* a per-app identity
/// mapping exists. It deliberately does NOT prove that mapping gets written in
/// the first place — the auth service holds neither the `pairwise_salt` nor the
/// route sector, so it cannot mint a gateway cookie. The FAITHFUL end-to-end —
/// REAL `POST /__zeroship/auth/session` cookie mint (which must itself persist the
/// identity row) → REAL `password_reset::complete` → family marker present —
/// lives in `crates/gateway/tests/auth_token_anchors_test.rs::\
/// cookie_mint_writes_identity_so_reset_evicts_cookie_session`, which pre-seeds
/// NOTHING in `app_user_identities`. Pre-seeding it HERE is what masked F1, so
/// the cross-crate faithful coverage is the gateway test, not this one.
#[ntex::test]
#[allow(clippy::future_not_send)]
async fn reset_post_revokes_app_session_anchor_and_writes_family_marker() {
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

    let email = format!("reset-anchor-{}@zeroship.test", Uuid::new_v4().simple());
    let user = users::create(&client, &email, "Test", None)
        .await
        .expect("seed user");
    let old_hash = password::hash("old reset password phrase").expect("hash old password");
    users::update_password_hash(&client, user.id, &old_hash)
        .await
        .expect("set old password");

    let cred_version_before: i64 = client
        .query_one(
            "SELECT credential_version FROM zeroship.users WHERE id = $1",
            &[&user.id],
        )
        .await
        .expect("read credential_version")
        .get(0);

    // Seed the per-app OAuth client + app the anchor FKs require.
    let client_id = format!("oac_anchor_{}", Uuid::new_v4().simple());
    client
        .execute(
            "INSERT INTO zeroship.oauth_clients \
                (client_id, client_name, redirect_uris, scopes, hydra_client_id) \
             VALUES ($1, $2, $3, $4, $1)",
            &[
                &client_id,
                &format!("Client {client_id}"),
                &vec![format!("https://{client_id}.example/cb")],
                &vec!["openid".to_string()],
            ],
        )
        .await
        .expect("insert oauth client");

    let app_id = Uuid::new_v4();
    let plan_id = seed_test_plan(&client).await;
    client
        .execute(
            "INSERT INTO zeroship.apps (id, name, plan_id, api_key) VALUES ($1, $2, $3, $4)",
            &[&app_id, &format!("anchor-app-{}", app_id.simple()), &plan_id, &"k"],
        )
        .await
        .expect("insert app");

    // The gateway stores the per-app pairwise subject the cookie carries here;
    // the reset teardown must reuse it as the family-marker `sub`.
    let pairwise_sub = format!("pws_anchor_{}", Uuid::new_v4().simple());
    client
        .execute(
            "INSERT INTO zeroship.app_user_identities \
                (app_client_id, global_user_id, pairwise_sub) \
             VALUES ($1, $2, $3)",
            &[&client_id, &user.id, &pairwise_sub],
        )
        .await
        .expect("insert app_user_identity");

    // Seed the live 30-day reload-recovery anchor (the attacker's resurrection
    // credential).
    let anchor_id = Uuid::new_v4();
    client
        .execute(
            "INSERT INTO zeroship.app_session_anchors \
                (id, app_id, client_id, global_user_id, refresh_token_enc, \
                 refresh_family_id, abs_expires_at) \
             VALUES ($1, $2, $3, $4, $5, $6, NOW() + INTERVAL '30 days')",
            &[
                &anchor_id,
                &app_id,
                &client_id,
                &user.id,
                &b"enc-refresh".to_vec(),
                &format!("rfam_{}", Uuid::new_v4().simple()),
            ],
        )
        .await
        .expect("insert anchor");

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

    // (1) The anchor MUST be revoked — `read_live` requires `revoked_at IS NULL`,
    //     so this is what makes `?mint=1` fail closed. THIS is the pre-fix break.
    let live_anchors: i64 = pg
        .query_one(
            "SELECT COUNT(*) FROM zeroship.app_session_anchors \
             WHERE global_user_id = $1 AND revoked_at IS NULL",
            &[&user.id],
        )
        .await
        .expect("count live anchors")
        .get(0);
    assert_eq!(
        live_anchors, 0,
        "password reset must revoke every app_session_anchor for the user \
         (else ?mint=1 resurrects the session)"
    );

    // (2) A family marker must exist for (client_id, pairwise_sub) so any live
    //     app-session cookie is rejected from now on.
    let marker_count: i64 = pg
        .query_one(
            "SELECT COUNT(*) FROM zeroship.token_revocations \
             WHERE client_id = $1 AND sub = $2",
            &[&client_id, &pairwise_sub],
        )
        .await
        .expect("count family markers")
        .get(0);
    assert_eq!(
        marker_count, 1,
        "password reset must write the (client_id, pairwise_sub) family marker"
    );

    // (3) credential_version bumped — IdP-leg defense in depth.
    let cred_version_after: i64 = pg
        .query_one(
            "SELECT credential_version FROM zeroship.users WHERE id = $1",
            &[&user.id],
        )
        .await
        .expect("read credential_version after")
        .get(0);
    assert!(
        cred_version_after > cred_version_before,
        "password reset must bump credential_version (was {cred_version_before}, \
         now {cred_version_after})"
    );

    // Cleanup (children first; anchors/identities also CASCADE off the app).
    pg.execute(
        "DELETE FROM zeroship.token_revocations WHERE client_id = $1",
        &[&client_id],
    )
    .await
    .ok();
    pg.execute(
        "DELETE FROM zeroship.app_session_anchors WHERE global_user_id = $1",
        &[&user.id],
    )
    .await
    .ok();
    pg.execute(
        "DELETE FROM zeroship.app_user_identities WHERE app_client_id = $1",
        &[&client_id],
    )
    .await
    .ok();
    pg.execute("DELETE FROM zeroship.apps WHERE id = $1", &[&app_id])
        .await
        .ok();
    pg.execute(
        "DELETE FROM zeroship.oauth_clients WHERE client_id = $1",
        &[&client_id],
    )
    .await
    .ok();
    pg.execute("DELETE FROM zeroship.audit_events WHERE actor_user_id = $1", &[&user.id])
        .await
        .ok();
    pg.execute(
        "DELETE FROM zeroship.magic_links WHERE email = $1::citext",
        &[&email],
    )
    .await
    .ok();
    pg.execute("DELETE FROM zeroship.users WHERE id = $1", &[&user.id])
        .await
        .ok();
}

/// Regression for security finding L4: a password-reset token must bind to the
/// IMMUTABLE `user_id` captured at issue time, not re-resolve its target by an
/// `email` JOIN at `complete()` time.
///
/// Pre-fix, `complete()` ran `JOIN zeroship.users u ON u.email = ml.email`, so
/// the account whose password gets set is whoever owns the email NOW — not the
/// account the reset was issued for. If any future email-change / account-recycle
/// path moves an email between accounts after a reset is outstanding, the
/// outstanding token silently sets the password of the account that inherited
/// the address.
///
/// This test simulates that reassignment directly (the same mutation a future
/// email-change feature would perform): issue a reset for victim A's email,
/// move that email to attacker-controlled account B, then `complete()`. The
/// reset MUST NOT set B's password.
///
/// Pre-fix this FAILS — `complete()` returns `Some { user_id: B }` and B's
/// password hash becomes the reset hash (cross-account takeover).
///
/// Post-fix `complete()` filters on the issue-time `user_id` (A); since A no
/// longer owns the email row, the candidate resolves to A's id and the token is
/// consumed against A only — B is never touched.
#[compio::test]
async fn complete_binds_issue_time_user_not_current_email_owner() {
    let Some(client) = pg().await else {
        eprintln!("skipping password_reset_test (no AUTH_DB_URL)");
        return;
    };

    let suffix = Uuid::new_v4().simple();
    let email_a = format!("reset-l4-victim-{suffix}@zeroship.test");
    let email_b = format!("reset-l4-attacker-{suffix}@zeroship.test");

    // Victim A: the account the reset is legitimately issued for.
    let user_a = users::create(&client, &email_a, "Victim A", None)
        .await
        .expect("seed user A");
    let a_old_hash = password::hash("victim-a old password phrase")
        .expect("hash A old");
    users::update_password_hash(&client, user_a.id, &a_old_hash)
        .await
        .expect("set A old password");

    // Attacker B: a DIFFERENT account. Parked on a placeholder email for now.
    let email_b_parked = format!("reset-l4-attacker-parked-{suffix}@zeroship.test");
    let user_b = users::create(&client, &email_b_parked, "Attacker B", None)
        .await
        .expect("seed user B");
    let b_old_hash = password::hash("attacker-b old password phrase")
        .expect("hash B old");
    users::update_password_hash(&client, user_b.id, &b_old_hash)
        .await
        .expect("set B old password");

    // 1. Issue a reset legitimately for victim A's email.
    let issued = password_reset::issue(&client, &email_a)
        .await
        .expect("issue reset for A");

    // 2. Email reassignment between issue and complete: A's email is freed and
    //    granted to attacker B. This is exactly the mutation a future
    //    email-change / account-recycle feature performs; today no such path
    //    exists, so we apply it by hand to exercise the latent retargeting.
    client
        .execute(
            "UPDATE zeroship.users SET email = $1::citext WHERE id = $2",
            &[&format!("reset-l4-victim-freed-{suffix}@zeroship.test"), &user_a.id],
        )
        .await
        .expect("free A's email");
    client
        .execute(
            "UPDATE zeroship.users SET email = $1::citext WHERE id = $2",
            &[&email_a, &user_b.id],
        )
        .await
        .expect("reassign A's email to B");
    let _ = &email_b; // (kept for readability; B now holds email_a)

    // 3. Complete the outstanding reset. It must NOT retarget to B.
    let new_hash = password::hash("attacker-chosen new password phrase")
        .expect("hash new");
    let completed = password_reset::complete(&client, &issued.raw, &new_hash)
        .await
        .expect("complete must not error");

    // The reset must never resolve to attacker B.
    if let Some(ref c) = completed {
        assert_ne!(
            c.user_id, user_b.id,
            "reset issued for A must NOT retarget to B after email reassignment"
        );
    }

    // Authoritative check: B's password hash is unchanged.
    let b_hash_now: String = client
        .query_one(
            "SELECT password_hash FROM zeroship.users WHERE id = $1",
            &[&user_b.id],
        )
        .await
        .expect("load B hash")
        .get("password_hash");
    assert_eq!(
        b_hash_now, b_old_hash,
        "attacker B's password MUST remain unchanged by a reset issued for A"
    );

    // Cleanup.
    client
        .execute(
            "DELETE FROM zeroship.magic_links WHERE email IN ($1::citext, $2::citext)",
            &[&email_a, &email_b],
        )
        .await
        .ok();
    client
        .execute(
            "DELETE FROM zeroship.users WHERE id = ANY($1)",
            &[&vec![user_a.id, user_b.id]],
        )
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
            "DELETE FROM zeroship.magic_links WHERE email = $1::citext",
            &[&email],
        )
        .await
        .ok();
    client
        .execute("DELETE FROM zeroship.users WHERE id = $1", &[&user.id])
        .await
        .ok();
}
