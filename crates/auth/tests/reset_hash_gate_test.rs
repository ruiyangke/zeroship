//! `/reset` POST must not pay for Argon2 on a token it is going to refuse,
//! and must rate-limit like every other password-hashing handler in the crate.
//!
//! Left unguarded, the handler hashes the submitted password on a
//! `spawn_blocking` worker before the reset token is ever examined. One
//! harvested CSRF pair (double-submit, so a single pair is reusable) plus a
//! garbage token then buys an attacker 19 MiB and a blocking slot per
//! request, out of a pool shared with `/login` and `/link`.
//!
//! The property under test is "the hasher was not reached", not "the response
//! came back quickly": a wall-clock assertion would be load-sensitive. The
//! tests read `password::hash_calls()` around a single request instead.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use clap::Parser;
use compio_postgres::{connect, Client, NoTls};
use ntex::http::header::SET_COOKIE;
use ntex::web::{self, test};
use uuid::Uuid;

use zeroship_auth::config::AuthConfig;
use zeroship_auth::identity::{password, password_reset};
use zeroship_auth::store::users;

/// `hash_calls()` is process-global, so a test reading a delta across one
/// request must be the only test hashing while it does. Cargo runs a test
/// binary multi-threaded unless told otherwise, so every test here holds
/// this for its whole body.
fn hash_counter_guard() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(PoisonError::into_inner)
}

fn test_cfg(db_url: &str) -> AuthConfig {
    let mut cfg = AuthConfig::parse_from([
        "zeroship-auth",
        "--db-url",
        db_url,
        "--stash-signing-key",
        "test-stash-key-not-for-prod-32bytes!",
    ]);
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

/// A distinct client IP per test: the reset limiter is keyed on it, so
/// sharing one would make these tests consume each other's budget.
fn unique_ip() -> String {
    let b = *Uuid::new_v4().as_bytes();
    format!("10.{}.{}.{}", b[0], b[1], b[2])
}

#[allow(clippy::future_not_send)]
async fn pg_connect(dsn: &str) -> Client {
    let (client, connection) = connect(dsn, NoTls).await.expect("connect");
    compio::runtime::spawn(async move {
        if let Err(e) = connection.run().await {
            eprintln!("[reset_hash_gate_test] pg connection driver: {e}");
        }
    })
    .detach();
    client
}

fn db_url() -> String {
    std::env::var("AUTH_DB_URL").expect("AUTH_DB_URL is required for reset_hash_gate_test")
}

/// Build the `/reset` service and harvest a CSRF pair from the GET render.
/// The GET is the whole cost of setting up an attack: the token in the URL is
/// never looked at, and the double-submit cookie/field pair is reusable.
macro_rules! reset_service_and_csrf {
    ($cfg:expr, $pg:expr) => {{
        let app = test::init_service(web::App::new().state($cfg).state($pg).service(
            web::resource("/reset")
                .route(web::get().to(zeroship_auth::ui::reset::get))
                .route(web::post().to(zeroship_auth::ui::reset::post)),
        ))
        .await;
        let get_resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/reset?token=harvest-csrf-only")
                .to_request(),
        )
        .await;
        assert_eq!(get_resp.status().as_u16(), 200);
        let csrf = read_set_cookie(get_resp.headers(), "__Host-zsidp_csrf")
            .expect("__Host-zsidp_csrf cookie set on GET /reset");
        (app, csrf)
    }};
}

fn reset_post(csrf: &str, token: &str, password: &str, ip: &str) -> test::TestRequest {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("csrf", csrf)
        .append_pair("token", token)
        .append_pair("password", password)
        .finish();
    test::TestRequest::post()
        .uri("/reset")
        .header("content-type", "application/x-www-form-urlencoded")
        .header("cookie", format!("__Host-zsidp_csrf={csrf}"))
        .header("x-forwarded-for", ip)
        .set_payload(body)
}

/// The defect: a token with no live row must be refused before the password
/// is hashed. Pre-fix the handler hashes first and the counter moves.
// `hash_counter_guard` must be held across the request under test - that's
// exactly the point (see the doc comment on it above): the counter delta is
// only meaningful if no other test's hashing overlaps this one's request.
#[allow(clippy::await_holding_lock)]
#[ntex::test]
#[allow(clippy::future_not_send)]
async fn reset_post_refuses_a_dead_token_without_hashing() {
    let _guard = hash_counter_guard();
    let dsn = db_url();
    let pg = Arc::new(pg_connect(&dsn).await);
    let cfg = Arc::new(test_cfg(&dsn));
    let ip = unique_ip();
    let (app, csrf) = reset_service_and_csrf!(cfg.clone(), pg.clone());

    let before = password::hash_calls();
    let resp = test::call_service(
        &app,
        reset_post(
            &csrf,
            "this-token-was-never-issued",
            "a fifteen plus character passphrase",
            &ip,
        )
        .to_request(),
    )
    .await;
    let after = password::hash_calls();

    assert_eq!(resp.status().as_u16(), 200, "refusal re-renders the form");
    let body = String::from_utf8(test::read_body(resp).await.to_vec()).expect("utf8 body");
    assert!(
        body.contains("reset link invalid or expired"),
        "a dead token must render the invalid-link copy; got: {body}"
    );
    assert_eq!(
        after, before,
        "a dead reset token must be refused before the Argon2 hash: \
         hash_calls went {before} -> {after}"
    );

    let like = format!("reset_ip:{ip}");
    pg.execute(
        "DELETE FROM zeroship.rate_limits WHERE bucket_key = $1",
        &[&like],
    )
    .await
    .ok();
}

/// The refusal must not be bought by breaking the real path: a live token
/// still completes, and that path does pay for the hash.
// See the allow on `reset_post_refuses_a_dead_token_without_hashing` above.
#[allow(clippy::await_holding_lock)]
#[ntex::test]
#[allow(clippy::future_not_send)]
async fn reset_post_with_a_live_token_still_completes() {
    let _guard = hash_counter_guard();
    let dsn = db_url();
    let client = pg_connect(&dsn).await;

    let email = format!("reset-hash-gate-{}@zeroship.test", Uuid::new_v4().simple());
    let old_phc = password::hash("old reset password phrase").expect("hash old password");
    let user = users::create(&client, &email, "Hash Gate", Some(&old_phc))
        .await
        .expect("seed user");
    let issued = password_reset::issue(&client, &email)
        .await
        .expect("issue reset token");

    let pg = Arc::new(client);
    let cfg = Arc::new(test_cfg(&dsn));
    let ip = unique_ip();
    let (app, csrf) = reset_service_and_csrf!(cfg.clone(), pg.clone());

    let before = password::hash_calls();
    let resp = test::call_service(
        &app,
        reset_post(
            &csrf,
            &issued.raw,
            "brand new reset password phrase",
            &ip,
        )
        .to_request(),
    )
    .await;
    let after = password::hash_calls();

    assert_eq!(resp.status().as_u16(), 302, "a live token redirects to /login");
    assert!(
        after > before,
        "the accepted path must still hash the new password"
    );

    let stored: String = pg
        .query_one(
            "SELECT password_hash FROM zeroship.users WHERE id = $1",
            &[&user.id],
        )
        .await
        .expect("read password hash")
        .get(0);
    assert!(
        password::verify("brand new reset password phrase", &stored).expect("verify new password"),
        "the new password must be the stored credential"
    );

    let live: i64 = pg
        .query_one(
            "SELECT COUNT(*) FROM zeroship.magic_links \
             WHERE email = $1::citext AND purpose = 'reset' AND consumed_at IS NULL",
            &[&email],
        )
        .await
        .expect("count live reset tokens")
        .get(0);
    assert_eq!(live, 0, "the reset token must be consumed");

    pg.execute(
        "DELETE FROM zeroship.audit_events WHERE actor_user_id = $1",
        &[&user.id],
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
    pg.execute(
        "DELETE FROM zeroship.rate_limits WHERE bucket_key = $1",
        &[&format!("reset_ip:{ip}")],
    )
    .await
    .ok();
}

/// The pre-check bounds the cost of one refused request; the limiter bounds
/// how many an IP gets. `/reset` was the only password-hashing handler in the
/// crate without one.
// See the allow on `reset_post_refuses_a_dead_token_without_hashing` above.
#[allow(clippy::await_holding_lock)]
#[ntex::test]
#[allow(clippy::future_not_send)]
async fn reset_post_is_rate_limited_per_ip() {
    let _guard = hash_counter_guard();
    let dsn = db_url();
    let pg = Arc::new(pg_connect(&dsn).await);
    let cfg = Arc::new(test_cfg(&dsn));
    let ip = unique_ip();
    let (app, csrf) = reset_service_and_csrf!(cfg.clone(), pg.clone());

    // One past the bucket capacity: the last attempt must be refused by the
    // limiter rather than by the token check.
    let capacity = 30;
    for attempt in 1..=capacity + 1 {
        let resp = test::call_service(
            &app,
            reset_post(
                &csrf,
                "this-token-was-never-issued",
                "a fifteen plus character passphrase",
                &ip,
            )
            .to_request(),
        )
        .await;
        let status = resp.status().as_u16();
        let body = String::from_utf8(test::read_body(resp).await.to_vec()).expect("utf8 body");

        if attempt <= capacity {
            assert_eq!(status, 200, "attempt {attempt} is within budget");
            assert!(
                body.contains("reset link invalid or expired"),
                "attempt {attempt} should be refused by the token check, not the limiter"
            );
        } else {
            assert_eq!(status, 429, "attempt {attempt} must be rate limited");
            assert!(
                body.contains("too many attempts"),
                "attempt {attempt} should render the rate-limit copy; got: {body}"
            );
        }
    }

    pg.execute(
        "DELETE FROM zeroship.rate_limits WHERE bucket_key = $1",
        &[&format!("reset_ip:{ip}")],
    )
    .await
    .ok();
}
