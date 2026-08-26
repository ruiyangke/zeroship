//! Live-PG roundtrip for `auth::identity::password_reset`.
//!
//! Skipped unless a test database is available (`PG_TEST_URL` or the TOML overlay). Each test scopes itself with a
//! random email so concurrent runs don't collide; the cleanup at the end
//! removes every row the test inserted.

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;

use compio_postgres::{connect, Client, NoTls};
use ntex::http::header::SET_COOKIE;
use ntex::web::{self, test};
use uuid::Uuid;

use zeroship_auth::config::AuthConfig;
use zeroship_core::config::{Secret, SourceKind};
use zeroship_auth::identity::{magic_link, password, password_reset};
use zeroship_auth::store::{sessions, users};

fn test_cfg(db_url: &str) -> AuthConfig {
    // A secret has no value flag - that is the point of the conversion - so the
    // fixture supplies each one in the shape an in-memory literal resolves to,
    // which is byte-for-byte what ZEROSHIP_AUTH_<NAME>=<value> produces. The
    // environment itself is process-global and would race sibling tests.
    let mut cfg = AuthConfig::parse_from(["zeroship-auth"]);
    cfg.settings.database_url = Secret::supplied(SourceKind::Env, Some(db_url.to_owned()));
    cfg.settings.stash_signing_key = Secret::supplied(
        SourceKind::Env,
        Some("test-stash-key-not-for-prod-32bytes!".to_owned()),
    );
    cfg
}

/// A per-call loopback address for the `x-forwarded-for` header every `/reset`
/// POST below carries.
///
/// `reset::post` keys its rate limit on `reset_ip:{client_ip}`, and
/// `headers::client_ip` reads THAT HEADER ALONE - `TestRequest::peer_addr` does
/// not reach `req.peer_addr()`, so a request without the header resolves to
/// `0.0.0.0` and lands in one bucket shared by every request, every test and
/// every concurrent run. `Bucket::RESET_IP` is 30 tokens refilling at 30/hour,
/// so that bucket does not recover inside a test session: once two runs have
/// drained it, every later run on the same database keeps taking 429 where
/// these tests assert 302, for an hour, whether or not anything is running
/// concurrently.
///
/// MEASURED 2026-08-20, five concurrent pairs of the four DDL-installing auth
/// modules on one shared database: of this file's three `reset_post_*` tests,
/// two failed in 10 of 10 runs and the third in 9, every one of them
/// `left: 429, right: 302` with a `password_reset_throttled` audit row naming
/// bucket `reset_per_ip`.
///
/// `signup_forgot_ratelimit_test` carries the same helper for the same reason.
fn unique_loopback() -> IpAddr {
    let bytes = *Uuid::new_v4().as_bytes();
    IpAddr::V4(Ipv4Addr::new(
        127,
        bytes[0].max(1),
        bytes[1].max(1),
        bytes[2].max(1),
    ))
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
    let dsn = zeroship_core::config::test_database_url_opt()?;
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

// `zeroship.magic_links` is shared with every concurrent run, so the name is
// per-call and the `WHEN` clause scopes the trigger to this test's own email.
// Doing only the first is the trap: a uniquely named trigger still fires - and
// sleeps 0.2s - inside the peer run's inserts.
//
// This helper was a byte-for-byte copy of `magic_link_test`'s, down to the
// object name `test_sleep_before_magic_link_insert`, so the two collided on one
// table even inside a SINGLE run. That was invisible only because these are
// modules of one `tests/main.rs` binary and the gate passes `--test-threads 1`:
// accidental isolation, exactly what this change removes the dependence on.
//
// The full argument, and the model it follows, is in `magic_link_test.rs`.
async fn install_magic_links_insert_delay(client: &Client, email: &str) -> String {
    let name = format!(
        "test_sleep_before_reset_link_insert_{}",
        Uuid::new_v4().simple()
    );
    client
        .execute(
            &format!(
                "CREATE FUNCTION zeroship.{name}() \
                 RETURNS trigger LANGUAGE plpgsql AS $$ \
                 BEGIN \
                     PERFORM pg_sleep(0.2); \
                     RETURN NEW; \
                 END \
                 $$"
            ),
            &[],
        )
        .await
        .expect("create insert delay function");
    client
        .execute(
            &format!(
                "CREATE TRIGGER {name} \
                 BEFORE INSERT ON zeroship.magic_links \
                 FOR EACH ROW WHEN (NEW.email = '{email}'::citext) \
                 EXECUTE FUNCTION zeroship.{name}()"
            ),
            &[],
        )
        .await
        .expect("create insert delay trigger");
    name
}

async fn drop_magic_links_insert_delay(client: &Client, name: &str) {
    client
        .execute(
            &format!("DROP TRIGGER IF EXISTS {name} ON zeroship.magic_links"),
            &[],
        )
        .await
        .ok();
    client
        .execute(&format!("DROP FUNCTION IF EXISTS zeroship.{name}()"), &[])
        .await
        .ok();
}

// The handler-level reset tests use `#[ntex::test]` because ntex test services
// need a running System.
#[ntex::test]
#[allow(clippy::future_not_send)]
async fn reset_post_revokes_all_sessions_and_audits_counts() {
    let dsn = match zeroship_core::config::test_database_url_opt() {
        Some(dsn) => dsn,
        None => {
            zeroship_test_support::skip("skipping password_reset_test (no test database (set PG_TEST_URL or run tests/provision_test_backends.sh))");
            return;
        }
    };
    let Some(client) = pg().await else {
        zeroship_test_support::skip("skipping password_reset_test (no test database (set PG_TEST_URL or run tests/provision_test_backends.sh))");
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
    let csrf = read_set_cookie(get_resp.headers(), "__Host-zsidp_csrf")
        .expect("__Host-zsidp_csrf cookie set on GET /reset");

    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("csrf", &csrf)
        .append_pair("token", &issued.raw)
        .append_pair("password", "new reset password phrase")
        .finish();
    let post_req = test::TestRequest::post()
        .uri("/reset")
        // Not `peer_addr`: the handler keys its rate limit on the FORWARDED ip
        // alone, and without this header every run shares `reset_ip:0.0.0.0`.
        // See `unique_loopback` for the measurement.
        .header("x-forwarded-for", unique_loopback().to_string())
        .header("content-type", "application/x-www-form-urlencoded")
        .header("cookie", format!("__Host-zsidp_csrf={csrf}"))
        .set_payload(body)
        .to_request();
    let post_resp = test::call_service(&app, post_req).await;
    assert_eq!(post_resp.status().as_u16(), 302);

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

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn reset_post_consumes_magic_login_state_for_same_email() {
    let dsn = match zeroship_core::config::test_database_url_opt() {
        Some(dsn) => dsn,
        None => {
            zeroship_test_support::skip("skipping password_reset_test (no test database (set PG_TEST_URL or run tests/provision_test_backends.sh))");
            return;
        }
    };
    let Some(client) = pg().await else {
        zeroship_test_support::skip("skipping password_reset_test (no test database (set PG_TEST_URL or run tests/provision_test_backends.sh))");
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

    let pg = Arc::new(client);
    let cfg = Arc::new(test_cfg(&dsn));
    let app = test::init_service(
        web::App::new()
            .state(cfg.clone())
            .state(pg.clone())
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
    let csrf = read_set_cookie(get_resp.headers(), "__Host-zsidp_csrf")
        .expect("__Host-zsidp_csrf cookie set on GET /reset");

    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("csrf", &csrf)
        .append_pair("token", &reset.raw)
        .append_pair("password", "new reset password phrase")
        .finish();
    let post_req = test::TestRequest::post()
        .uri("/reset")
        // Not `peer_addr`: the handler keys its rate limit on the FORWARDED ip
        // alone, and without this header every run shares `reset_ip:0.0.0.0`.
        // See `unique_loopback` for the measurement.
        .header("x-forwarded-for", unique_loopback().to_string())
        .header("content-type", "application/x-www-form-urlencoded")
        .header("cookie", format!("__Host-zsidp_csrf={csrf}"))
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
    let dsn = match zeroship_core::config::test_database_url_opt() {
        Some(dsn) => dsn,
        None => {
            zeroship_test_support::skip("skipping password_reset_test (no test database (set PG_TEST_URL or run tests/provision_test_backends.sh))");
            return;
        }
    };
    let Some(client) = pg().await else {
        zeroship_test_support::skip("skipping password_reset_test (no test database (set PG_TEST_URL or run tests/provision_test_backends.sh))");
        return;
    };

    let email = format!(
        "reset-concurrent-{}@zeroship.test",
        Uuid::new_v4().simple()
    );
    let user = users::create(&client, &email, "Test", None)
        .await
        .expect("seed user");
    let insert_delay = install_magic_links_insert_delay(&client, &email).await;

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

    drop_magic_links_insert_delay(&client, &insert_delay).await;

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
        zeroship_test_support::skip("skipping password_reset_test (no test database (set PG_TEST_URL or run tests/provision_test_backends.sh))");
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
        zeroship_test_support::skip("skipping password_reset_test (no test database (set PG_TEST_URL or run tests/provision_test_backends.sh))");
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
/// `gateway_sessions`. It NEVER:
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
    let dsn = match zeroship_core::config::test_database_url_opt() {
        Some(dsn) => dsn,
        None => {
            zeroship_test_support::skip("skipping password_reset_test (no test database (set PG_TEST_URL or run tests/provision_test_backends.sh))");
            return;
        }
    };
    let Some(client) = pg().await else {
        zeroship_test_support::skip("skipping password_reset_test (no test database (set PG_TEST_URL or run tests/provision_test_backends.sh))");
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
                (client_id, client_name, redirect_uris, scopes) \
             VALUES ($1, $2, $3, $4)",
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
    //
    // DERIVED through the production function, not invented. An invented seed
    // still satisfies the marker assertion below (the teardown copies whatever
    // it finds), so the pair "seed X, assert marker == X" holds for a value no
    // live cookie could ever carry. Deriving it is what makes the assertion say
    // something about the real subject rather than about itself.
    let pairwise_sub = zeroship_core::auth::derive_pairwise(
        &zeroship_core::crypto::derive_key("password-reset-test-salt"),
        &user.id.to_string(),
        &format!("https://{client_id}.zeroship.localhost"),
    );
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

    let pg = Arc::new(client);
    let cfg = Arc::new(test_cfg(&dsn));
    let app = test::init_service(
        web::App::new()
            .state(cfg.clone())
            .state(pg.clone())
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
    let csrf = read_set_cookie(get_resp.headers(), "__Host-zsidp_csrf")
        .expect("__Host-zsidp_csrf cookie set on GET /reset");

    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("csrf", &csrf)
        .append_pair("token", &issued.raw)
        .append_pair("password", "new reset password phrase")
        .finish();
    let post_req = test::TestRequest::post()
        .uri("/reset")
        // Not `peer_addr`: the handler keys its rate limit on the FORWARDED ip
        // alone, and without this header every run shares `reset_ip:0.0.0.0`.
        // See `unique_loopback` for the measurement.
        .header("x-forwarded-for", unique_loopback().to_string())
        .header("content-type", "application/x-www-form-urlencoded")
        .header("cookie", format!("__Host-zsidp_csrf={csrf}"))
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
    //
    //     Read the marker's `sub` back and compare, rather than counting rows
    //     that match the seed: counting cannot distinguish "no marker" from
    //     "a marker under some other subject", and a marker keyed on anything
    //     but the subject the cookie carries revokes nothing.
    let markers = pg
        .query(
            "SELECT sub FROM zeroship.token_revocations WHERE client_id = $1",
            &[&client_id],
        )
        .await
        .expect("read family markers");
    assert_eq!(
        markers.len(),
        1,
        "password reset must write exactly one family marker for the app client"
    );
    let marker_sub: String = markers[0].get("sub");
    assert_eq!(
        marker_sub, pairwise_sub,
        "the family marker must be keyed on the per-app pairwise subject the \
         cookie carries, not on any other identifier"
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

/// Regression: a password reset must still take effect when the user holds a
/// live refresh token at an app they also have a pairwise identity row for.
///
/// `password_reset::complete` writes its family markers from TWO data-modifying
/// CTEs that both `INSERT ... ON CONFLICT (client_id, sub) DO UPDATE` into
/// `zeroship.token_revocations`: `wrapper_family_markers` (drawn from
/// `app_user_identities`) and `refresh_family_markers` (drawn from
/// `oauth_refresh_tokens`). For a non-brokered app client those two sources
/// carry the SAME `(client_id, sub)` pair - `app_user_identities.pairwise_sub`
/// and `oauth_refresh_tokens.sub` are both
/// `Issuer::pairwise_subject(user_id, sector_identifier)`
/// (`crates/auth/src/oidc/authorization_code.rs:1442` and
/// `crates/auth/src/oidc/refresh.rs:376`). PostgreSQL refuses that:
///
///   ERROR:  ON CONFLICT DO UPDATE command cannot affect row a second time
///
/// and the error aborts the WHOLE statement, so NOTHING lands: the password is
/// not changed, the reset token is not consumed, no family marker is written,
/// no anchor is revoked. The handler renders "internal error" and every
/// existing session survives - the exact outcome a password reset exists to
/// prevent.
///
/// This is the DEFAULT path, not an exotic one: the gateway always requests
/// `offline_access` (`crates/gateway/src/oidc_rp.rs:189`,
/// `crates/gateway/src/browser_auth.rs:176`), so one BFF app login writes both
/// rows.
///
/// The assertion is the SECURITY property - the OLD PASSWORD STOPS WORKING -
/// checked through `credentials::verify_password_credentials`, the same
/// function `/login` calls. Asserting on a marker row or on `revoked_at`
/// instead would pass on a reset that silently did nothing, because a reset
/// that never ran leaves no contradictory row behind.
///
/// The sibling test above (`reset_post_revokes_app_session_anchor_and_writes_\
/// family_marker`) seeds the identity row but NO refresh token, so the two
/// CTEs never collide there and it is green throughout this bug.
#[ntex::test]
#[allow(clippy::future_not_send)]
async fn reset_still_applies_when_user_holds_a_refresh_token_for_the_same_app() {
    let dsn = match zeroship_core::config::test_database_url_opt() {
        Some(dsn) => dsn,
        None => {
            zeroship_test_support::skip("skipping password_reset_test (no test database (set PG_TEST_URL or run tests/provision_test_backends.sh))");
            return;
        }
    };
    let Some(client) = pg().await else {
        zeroship_test_support::skip("skipping password_reset_test (no test database (set PG_TEST_URL or run tests/provision_test_backends.sh))");
        return;
    };

    const OLD_PASSWORD: &str = "old reset password phrase";
    const NEW_PASSWORD: &str = "new reset password phrase";

    let email = format!("reset-refresh-{}@zeroship.test", Uuid::new_v4().simple());
    let user = users::create(&client, &email, "Test", None)
        .await
        .expect("seed user");
    let old_hash = password::hash(OLD_PASSWORD).expect("hash old password");
    users::update_password_hash(&client, user.id, &old_hash)
        .await
        .expect("set old password");

    // Per-app OAuth client + app (the anchor and identity rows FK to them).
    let client_id = format!("oac_refresh_{}", Uuid::new_v4().simple());
    client
        .execute(
            "INSERT INTO zeroship.oauth_clients \
                (client_id, client_name, redirect_uris, scopes) \
             VALUES ($1, $2, $3, $4)",
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
            &[&app_id, &format!("refresh-app-{}", app_id.simple()), &plan_id, &"k"],
        )
        .await
        .expect("insert app");

    // ONE pairwise subject, written to BOTH tables - which is what a single
    // real code exchange does. Derived through the production function so the
    // collision is the production collision, not one this test invented.
    let pairwise_sub = zeroship_core::auth::derive_pairwise(
        &zeroship_core::crypto::derive_key("password-reset-test-salt"),
        &user.id.to_string(),
        &format!("https://{client_id}.zeroship.localhost"),
    );
    client
        .execute(
            "INSERT INTO zeroship.app_user_identities \
                (app_client_id, global_user_id, pairwise_sub) \
             VALUES ($1, $2, $3)",
            &[&client_id, &user.id, &pairwise_sub],
        )
        .await
        .expect("insert app_user_identity");

    // The live refresh family the same login issued (`offline_access` is always
    // requested by the gateway), carrying that SAME `sub`.
    client
        .execute(
            "INSERT INTO zeroship.oauth_refresh_tokens \
                (token_hash, hash_key_version, refresh_family_id, client_id, user_id, \
                 sub, granted_scopes, family_granted_scopes, expires_at, \
                 family_absolute_expires_at) \
             VALUES ($1, 1, $2, $3, $4, $5, $6, $6, \
                     NOW() + INTERVAL '7 days', NOW() + INTERVAL '30 days')",
            &[
                &Uuid::new_v4().as_bytes().to_vec(),
                &format!("rfam_{}", Uuid::new_v4().simple()),
                &client_id,
                &user.id,
                &pairwise_sub,
                &vec!["openid".to_string(), "offline_access".to_string()],
            ],
        )
        .await
        .expect("insert refresh token");

    let issued = password_reset::issue(&client, &email)
        .await
        .expect("issue reset token");

    let pg = Arc::new(client);
    let cfg = Arc::new(test_cfg(&dsn));
    let app = test::init_service(
        web::App::new()
            .state(cfg.clone())
            .state(pg.clone())
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
    let csrf = read_set_cookie(get_resp.headers(), "__Host-zsidp_csrf")
        .expect("__Host-zsidp_csrf cookie set on GET /reset");

    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("csrf", &csrf)
        .append_pair("token", &issued.raw)
        .append_pair("password", NEW_PASSWORD)
        .finish();
    let post_req = test::TestRequest::post()
        .uri("/reset")
        .header("x-forwarded-for", unique_loopback().to_string())
        .header("content-type", "application/x-www-form-urlencoded")
        .header("cookie", format!("__Host-zsidp_csrf={csrf}"))
        .set_payload(body)
        .to_request();
    let post_resp = test::call_service(&app, post_req).await;

    // THE SECURITY ASSERTION. Run FIRST, before the status check, so a failure
    // reports the property that matters rather than the symptom.
    //
    // `verify_password_credentials` is the production credential gate
    // `/login` calls. A fresh random email plus a per-call loopback IP keeps
    // this out of every shared rate-limit bucket.
    let login_ip = unique_loopback().to_string();
    let http_req = test::TestRequest::default()
        .header("x-forwarded-for", login_ip.as_str())
        .to_http_request();
    let old_password_still_works = zeroship_auth::identity::credentials::verify_password_credentials(
        pg.as_ref(),
        &http_req,
        &client_id,
        &login_ip,
        &email,
        OLD_PASSWORD,
    )
    .await;
    let old_password_accepted = old_password_still_works.is_ok();

    // Clean up before asserting so a red run does not strand rows for the next
    // one (this test shares its database with concurrent runs).
    let cleanup = async {
        for (sql, ()) in [
            ("DELETE FROM zeroship.oauth_refresh_tokens WHERE user_id = $1", ()),
            ("DELETE FROM zeroship.app_session_anchors WHERE global_user_id = $1", ()),
            ("DELETE FROM zeroship.audit_events WHERE actor_user_id = $1", ()),
        ] {
            pg.execute(sql, &[&user.id]).await.ok();
        }
        pg.execute(
            "DELETE FROM zeroship.app_user_identities WHERE app_client_id = $1",
            &[&client_id],
        )
        .await
        .ok();
        pg.execute(
            "DELETE FROM zeroship.token_revocations WHERE client_id = $1",
            &[&client_id],
        )
        .await
        .ok();
        pg.execute("DELETE FROM zeroship.apps WHERE id = $1", &[&app_id]).await.ok();
        pg.execute(
            "DELETE FROM zeroship.oauth_clients WHERE client_id = $1",
            &[&client_id],
        )
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
    };
    cleanup.await;

    assert!(
        !old_password_accepted,
        "SECURITY: after a completed password reset the OLD password must no \
         longer authenticate. It still does, because the reset statement aborted \
         with `ON CONFLICT DO UPDATE command cannot affect row a second time` \
         (two CTEs upserting the same (client_id, sub) into token_revocations) \
         and rolled back everything, including the password change."
    );
    assert_eq!(
        post_resp.status().as_u16(),
        302,
        "a successful /reset POST redirects to /login; 200 means it re-rendered \
         the form with an error"
    );
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
        zeroship_test_support::skip("skipping password_reset_test (no test database (set PG_TEST_URL or run tests/provision_test_backends.sh))");
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
        zeroship_test_support::skip("skipping password_reset_test (no test database (set PG_TEST_URL or run tests/provision_test_backends.sh))");
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
