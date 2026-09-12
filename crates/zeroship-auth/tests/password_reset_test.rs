//! Live-PG roundtrip for `auth::identity::password_reset`.
//!
//! Skipped unless a test database is available (`PG_TEST_URL` or the TOML overlay). Each test scopes itself with a
//! random email so concurrent runs don't collide; the cleanup at the end
//! removes every row the test inserted.

use crate::common;

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
/// every concurrent run. `Quota::RESET_IP` is 30 tokens refilling at 30/hour,
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
async fn pg() -> compio_postgres::Client {
    let dsn = crate::common::test_database_url();
    let (client, connection) = connect(&dsn, NoTls).await.expect("connect");
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

// The handler-level reset tests use `#[ntex::test]` because ntex test services
// need a running System.
#[ntex::test]
#[allow(clippy::future_not_send)]
async fn reset_post_revokes_all_sessions_and_audits_counts() {
    let dsn = crate::common::test_database_url();
    let client = pg().await;

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
    // An app row needs a project, and a project needs an organization. Nothing
    // here asserts on authority, so the organization is left member-less.
    let project_id = common::unowned_project(&client).await;
    client
        .execute(
            "INSERT INTO zeroship.apps (id, name, plan_id, project_id, organization_id) \
             SELECT $1, $2, $3, p.id, p.organization_id FROM zeroship.projects p WHERE p.id = $4",
            &[
                &gw_app_id,
                &format!("reset-revoke-app-{}", gw_app_id.simple()),
                &plan_id,
                &project_id
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
    let dsn = crate::common::test_database_url();
    let client = pg().await;

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
/// lives in `crates/zeroship-gateway/tests/auth_token_anchors_test.rs::\
/// cookie_mint_writes_identity_so_reset_evicts_cookie_session`, which pre-seeds
/// NOTHING in `app_user_identities`. Pre-seeding it HERE is what masked F1, so
/// the cross-crate faithful coverage is the gateway test, not this one.
#[ntex::test]
#[allow(clippy::future_not_send)]
async fn reset_post_revokes_app_session_anchor_and_writes_family_marker() {
    let dsn = crate::common::test_database_url();
    let client = pg().await;

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
    // An app row needs a project, and a project needs an organization. Nothing
    // here asserts on authority, so the organization is left member-less.
    let project_id = common::unowned_project(&client).await;
    client
        .execute(
            "INSERT INTO zeroship.apps (id, name, plan_id, project_id, organization_id) \
             SELECT $1, $2, $3, p.id, p.organization_id FROM zeroship.projects p WHERE p.id = $4",
            &[
                &app_id,
                &format!("anchor-app-{}", app_id.simple()),
                &plan_id,
                &project_id,
            ],
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
/// `app_user_identities`) and `refresh_family_markers` (drawn from the live
/// `zeroship.sessions` rows joined to their grant). For a non-brokered app
/// client those two sources carry the SAME `(client_id, sub)` pair -
/// `app_user_identities.pairwise_sub` and `zeroship.grants.subject` are both
/// `Issuer::pairwise_subject(user_id, sector_identifier)`, written by
/// `mint_access_token` and `establish_session` respectively. PostgreSQL
/// refuses that:
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
/// `offline_access` (`crates/zeroship-gateway/src/oidc_rp.rs:189`,
/// `crates/zeroship-gateway/src/browser_auth.rs:176`), so one BFF app login writes both
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
    let dsn = crate::common::test_database_url();
    let client = pg().await;

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
    // An app row needs a project, and a project needs an organization. Nothing
    // here asserts on authority, so the organization is left member-less.
    let project_id = common::unowned_project(&client).await;
    client
        .execute(
            "INSERT INTO zeroship.apps (id, name, plan_id, project_id, organization_id) \
             SELECT $1, $2, $3, p.id, p.organization_id FROM zeroship.projects p WHERE p.id = $4",
            &[
                &app_id,
                &format!("refresh-app-{}", app_id.simple()),
                &plan_id,
                &project_id,
            ],
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

    // The live session the same login established (`offline_access` is always
    // requested by the gateway), whose GRANT carries that SAME `sub`. The
    // collision this test exists for is between the grant's subject and the
    // pairwise identity row above, which are two spellings of one value.
    let scopes = vec!["openid".to_string(), "offline_access".to_string()];
    let grant_id = zeroship_auth::session_store::upsert_grant(
        &client,
        user.id,
        &zeroship_auth::session_store::Audience::App {
            client_id: client_id.clone(),
        },
        &pairwise_sub,
        &scopes,
        None,
    )
    .await
    .expect("seed grant");
    client
        .execute(
            "INSERT INTO zeroship.sessions \
                (id, person_id, audience_kind, client_id, grant_id, kind, \
                 credential_epoch, secret_hash, secret_key_version, scopes, \
                 idle_expires_at, absolute_expires_at) \
             VALUES ($1, $2, 'app', $3, $4, 'browser', 0, $5, 1, $6, \
                     NOW() + INTERVAL '7 days', NOW() + INTERVAL '30 days')",
            &[
                &zeroship_core::typed_id::new_session_id(),
                &user.id,
                &client_id,
                &grant_id,
                &Uuid::new_v4().as_bytes().to_vec(),
                &scopes,
            ],
        )
        .await
        .expect("insert session");

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
            ("DELETE FROM zeroship.sessions WHERE person_id = $1", ()),
            ("DELETE FROM zeroship.grants WHERE person_id = $1", ()),
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
