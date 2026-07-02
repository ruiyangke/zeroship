//! Account-enumeration timing/shape defense (proposal §13 "Account
//! enumeration" row).
//!
//! Asserts that `POST /login` returns identical responses (HTTP status, body
//! length, and median wall-clock latency within 2×) for two failure paths:
//!
//! 1. "Wrong password on a real user" — exercises Argon2 verify against the
//!    user's stored PHC.
//! 2. "Any password on a non-existent user" — falls through to the dummy
//!    PHC branch in `crates/auth/src/ui/login.rs::post`.
//!
//! If these diverge in status, body length, or wall time, the dummy-hash
//! arm has regressed and an attacker can probe for valid emails.
//!
//! Skips when `AUTH_DB_URL` is unset.

use std::sync::Arc;
use std::time::{Duration, Instant};

use ntex::web;
use uuid::Uuid;

use zeroship_auth::headers::SecurityHeaders;
use zeroship_auth::server;

mod common;
use common::{native_authorize_return_to, read_set_cookie, test_auth_config, CookieJar};

fn median(durations: &mut [Duration]) -> Duration {
    durations.sort();
    durations[durations.len() / 2]
}

/// Drive `POST /login` once with the given (challenge, email, password) and
/// return (status, `body_len`, elapsed). Each call also performs the
/// `GET /login` round-trip so the form CSRF + cookie are freshly minted.
//
// cyper client is `!Send` (per-thread connection handle).
#[allow(clippy::future_not_send)]
async fn one_failure(
    http: &cyper::Client,
    auth_base: &str,
    return_to: &str,
    email: &str,
    password: &str,
) -> (u16, usize, Duration) {
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("return_to", return_to)
        .finish();
    let login_url = format!("{auth_base}/login?{query}");
    let resp = http
        .request(http::Method::GET, &login_url)
        .expect("build GET /login")
        .send()
        .await
        .expect("send GET /login");
    assert!(
        resp.status().is_success(),
        "GET /login expected 200, got {}",
        resp.status()
    );
    let csrf = read_set_cookie(&resp, "zsidp_csrf")
        .expect("zsidp_csrf cookie set on GET /login");
    let mut jar = CookieJar::default();
    jar.set("zsidp_csrf", &csrf);

    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("csrf", &csrf)
        .append_pair("email", email)
        .append_pair("password", password)
        .finish();
    let t0 = Instant::now();
    let resp = http
        .request(http::Method::POST, &login_url)
        .expect("build POST /login")
        .header("content-type", "application/x-www-form-urlencoded")
        .expect("content-type")
        .header("cookie", jar.header())
        .expect("cookie header")
        .body(body)
        .send()
        .await
        .expect("send POST /login");
    let elapsed = t0.elapsed();
    let status = resp.status().as_u16();
    let body_bytes = resp.text().await.expect("body");
    (status, body_bytes.len(), elapsed)
}

// ─── Test ────────────────────────────────────────────────────────────────

const N_PAIRS: usize = 4;

#[ntex::test]
async fn login_failure_responses_are_indistinguishable() {
    // 0. Env-skip check.
    let Ok(db_url) = std::env::var("AUTH_DB_URL") else {
        eprintln!("[enum_defense] skip (need AUTH_DB_URL)");
        return;
    };

    // 1. Connect PG.
    let (pg_client, pg_connection) = compio_postgres::connect(&db_url, compio_postgres::NoTls)
        .await
        .expect("connect pg");
    compio::runtime::spawn(async move {
        if let Err(e) = pg_connection.run().await {
            eprintln!("[enum_defense] pg connection driver: {e}");
        }
    })
    .detach();
    let pg_client = Arc::new(pg_client);

    // 2. Boot the auth server in-process.
    let cfg = Arc::new(test_auth_config(&db_url));
    let cfg_state = cfg.clone();
    let db_state = pg_client.clone();
    let refresh_pool_state =
        zeroship_auth::oidc::refresh::RefreshSessionPool::new(db_url.clone(), 4);
    let srv = web::test::server(move || {
        let cfg_state = cfg_state.clone();
        let db_state = db_state.clone();
        let refresh_pool_state = refresh_pool_state.clone();
        async move {
            web::App::new()
                .state(cfg_state)
                .state(db_state)
                .state(refresh_pool_state)
                .middleware(SecurityHeaders::default())
                .configure(server::configure(false, false))
        }
    })
    .await;
    let auth_base = srv.url("").trim_end_matches('/').to_string();

    // 3. Build a stable native authorization request target.
    let test_client_id = format!("enum-{}", Uuid::new_v4().simple());
    let test_redirect = "http://127.0.0.1:9999/cb";

    // 4. Seed a real user we can fail against.
    let real_email = format!("real-{}@zeroship.test", Uuid::new_v4().simple());
    let real_password = "right-password-1234567890";
    let phc = compio::runtime::spawn_blocking({
        let pw = real_password.to_string();
        move || zeroship_auth::identity::password::hash(&pw)
    })
    .await
    .expect("hash spawn")
    .expect("hash ok");
    pg_client
        .execute(
            "INSERT INTO zeroship.users (email, name, password_hash) VALUES ($1::citext, $2, $3)",
            &[&real_email.as_str(), &"Real User", &phc.as_str()],
        )
        .await
        .expect("insert real user");

    let http = cyper::Client::new();

    // 5. Drive N total request pairs. Argon2 is ~100 ms per verify so keep
    //    this small; 4 wrong-pw + 4 missing-user = 8 verifies ≈ 800 ms.
    //
    //    Drain the LOGIN_EIP bucket entries for these specific email keys
    //    first so each request pair gets a fresh budget; we use unique
    //    emails per ghost iteration, but the same `real_email` across
    //    wrong-pw iterations.
    pg_client
        .execute(
            "DELETE FROM zeroship.rate_limits WHERE bucket_key LIKE 'login:%@zeroship.test%' \
              OR bucket_key LIKE 'login:%real-%' \
              OR bucket_key LIKE 'login:%ghost-%'",
            &[],
        )
        .await
        .ok();

    let mut wrong_pw_times: Vec<Duration> = Vec::with_capacity(N_PAIRS);
    let mut missing_times: Vec<Duration> = Vec::with_capacity(N_PAIRS);
    let mut wrong_pw_resps: Vec<(u16, usize)> = Vec::with_capacity(N_PAIRS);
    let mut missing_resps: Vec<(u16, usize)> = Vec::with_capacity(N_PAIRS);

    for i in 0..N_PAIRS {
        // Same return target for BOTH arms so the rendered form's hidden inputs
        // / form-action URL are byte-identical between paths. Use a fresh
        // target per iteration so visible nonce/state values never collide.
        let return_to = native_authorize_return_to(&test_client_id, test_redirect);

        // ── wrong password on existing user ──────────────────────────────
        let (status_w, len_w, elapsed_w) =
            one_failure(&http, &auth_base, &return_to, &real_email, "totally-wrong-password-xyz").await;
        wrong_pw_times.push(elapsed_w);
        wrong_pw_resps.push((status_w, len_w));
        eprintln!("[enum_defense] iter {i}: wrong-pw status={status_w} body_len={len_w} t={elapsed_w:?}");

        // ── any password on a missing user ───────────────────────────────
        let ghost_email = format!("ghost-{}@zeroship.test", Uuid::new_v4().simple());
        let (status_m, len_m, elapsed_m) =
            one_failure(&http, &auth_base, &return_to, &ghost_email, "totally-wrong-password-xyz").await;
        missing_times.push(elapsed_m);
        missing_resps.push((status_m, len_m));
        eprintln!("[enum_defense] iter {i}: missing-user status={status_m} body_len={len_m} t={elapsed_m:?}");
    }

    // 6. Assertions.
    //
    // 6a. Status code identical across paths.
    for i in 0..N_PAIRS {
        assert_eq!(
            wrong_pw_resps[i].0, missing_resps[i].0,
            "iter {i}: wrong-pw status {} ≠ missing-user status {}",
            wrong_pw_resps[i].0, missing_resps[i].0
        );
    }

    // 6b. Body length identical across paths. The CSRF token re-issued on
    //     each failure is fixed-length (~22 base64url chars), and both
    //     arms render the same template with the same `client_name` and
    //     the same "invalid email or password" error string, so lengths
    //     are deterministic. A divergence here = enumeration channel.
    for i in 0..N_PAIRS {
        assert_eq!(
            wrong_pw_resps[i].1, missing_resps[i].1,
            "iter {i}: wrong-pw body length {} ≠ missing-user body length {}",
            wrong_pw_resps[i].1, missing_resps[i].1
        );
    }

    // 6c. Timing — medians should be within 2× of each other. Both paths
    //     run an Argon2id verify (~100 ms); divergence here means the
    //     dummy-hash branch isn't actually exercised on the missing-user
    //     path.
    let med_wrong = median(&mut wrong_pw_times.clone());
    let med_missing = median(&mut missing_times.clone());
    let wrong_s = med_wrong.as_secs_f64();
    let missing_s = med_missing.as_secs_f64();
    assert!(
        wrong_s > 0.0 && missing_s > 0.0,
        "timing read 0; arm broken: wrong={med_wrong:?} missing={med_missing:?}"
    );
    let ratio = missing_s / wrong_s;
    assert!(
        ratio > 0.5 && ratio < 2.0,
        "timing ratio missing/wrong = {ratio} (medians: wrong={med_wrong:?}, \
         missing={med_missing:?}); dummy-hash arm not equivalent to real verify"
    );

    // 7. Cleanup.
    pg_client
        .execute(
            "DELETE FROM zeroship.users WHERE email = $1::citext",
            &[&real_email.as_str()],
        )
        .await
        .ok();
    pg_client
        .execute(
            "DELETE FROM zeroship.rate_limits WHERE bucket_key LIKE 'login:%@zeroship.test%' \
              OR bucket_key LIKE 'login:%real-%' \
              OR bucket_key LIKE 'login:%ghost-%'",
            &[],
        )
        .await
        .ok();

    compio::time::sleep(Duration::from_millis(50)).await;
    drop(srv);
}
