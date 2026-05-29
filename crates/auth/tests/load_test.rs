//! Auth `/login` throughput load test (P6-U4).
//!
//! Boots `crates/auth` in-process via `ntex::web::test::server`, seeds N
//! users with a *single* shared Argon2id PHC, pre-fetches N `login_challenge`
//! tokens from hydra, then fires N parallel `GET /login` → `POST /login`
//! flows through `futures::future::join_all`. Reports p50, p99, and
//! throughput; asserts loose regression bounds.
//!
//! This is regression detection — not a benchmarking framework. Argon2id at
//! OWASP 2026 params runs ~100 ms per verify; with N=50 parallel logins,
//! ideal wall time is `N * 100 ms / parallelism`. In practice the cyper
//! HTTP/1 client pool caps in-flight requests per host (so we don't actually
//! see 32-way parallelism even on a 32-core box), and total wall time lands
//! around 1.5-2 s on a quiet workstation, throughput ~25-30 RPS.
//!
//! The asserted bounds are LOOSE on purpose — they catch a 5-10× regression,
//! not a 2× drift. On a measured baseline of ~28 RPS / p99 1.8 s, we assert
//! `> 10 RPS` and `p99 < 5 s`. Tighten only after gathering CI-stable
//! baselines across runners.
//!
//! Triple-gated so it never runs in normal `cargo test`:
//!   - `AUTH_LOAD_TEST=1` must be set
//!   - `AUTH_DB_URL` must point at a live PG with the auth schema
//!   - `HYDRA_ADMIN_URL` must point at a reachable hydra admin endpoint
//!
//!
//! Manual smoke:
//! ```text
//!   AUTH_LOAD_TEST=1 \
//!   AUTH_DB_URL=postgres://postgres:zeroship@localhost:5441/zeroship \
//!   HYDRA_ADMIN_URL=http://localhost:4445 \
//!     cargo test -p zeroship-auth --test load_test -- --nocapture
//! ```

use std::time::{Duration, Instant};

use futures::future::join_all;
use uuid::Uuid;

mod common;
use common::{read_set_cookie, Fixture};

/// Number of parallel login flows. Tuned so that with Argon2id at ~100 ms
/// per verify on a 4-core box, total wall time stays under ~2 s. Raising
/// this also stresses the `LOGIN_IP` rate-limit bucket (capacity 60); keep
/// N strictly below 60 unless the bucket is widened or partitioned.
const N: usize = 50;

/// The shared password every seeded user logs in with. Hashed once, written
/// to all N rows.
const PASSWORD: &str = "load-test-secure-password-1234567890";

/// Loose regression bounds. The point is to catch a `30 RPS → 5 RPS` cliff,
/// not to enforce a contract — tighten after a baseline is established.
/// Measured baseline on a 32-core workstation: p99 ≈ 1.8 s, ~28 RPS (cyper
/// HTTP/1 pool caps in-flight per host, so per-request latency stacks).
const P99_BUDGET: Duration = Duration::from_millis(5000);
const RPS_FLOOR: f64 = 10.0;

#[ntex::test]
async fn auth_login_throughput() {
    // 0. Triple env-gate. Skip silently (test still passes) unless ALL of
    //    AUTH_LOAD_TEST, AUTH_DB_URL, and HYDRA_ADMIN_URL are set.
    if std::env::var("AUTH_LOAD_TEST").is_err() {
        eprintln!("[load_test] skip (set AUTH_LOAD_TEST=1 to run)");
        return;
    }
    if std::env::var("AUTH_DB_URL").is_err() || std::env::var("HYDRA_ADMIN_URL").is_err() {
        eprintln!("[load_test] skip (need AUTH_DB_URL + HYDRA_ADMIN_URL)");
        return;
    }

    // 1. Boot the shared fixture: PG client + migrations, in-process auth
    //    server, fresh hydra OIDC client. `skip_consent=true` is set inside
    //    Fixture::boot so /login → 302 directly without our /consent step.
    let fixture = Fixture::boot("load")
        .await
        .expect("Fixture::boot returned None despite env gates");
    let auth_base = fixture.auth_base.clone();
    let http = fixture.http.clone();
    let pg = fixture.pg.clone();

    // 2. Hash the password ONCE on a blocking thread. Hashing N times would
    //    cost N × 100 ms ≈ 5 s of setup; the verifier doesn't care that
    //    every row has the same salt+digest.
    let phc = compio::runtime::spawn_blocking(|| {
        zeroship_auth::identity::password::hash(PASSWORD)
    })
    .await
    .expect("hash spawn_blocking")
    .expect("argon2 hash");

    // 3. Seed N users. Each gets a unique email so the LOGIN_EMAIL bucket
    //    (10/hr) never throttles, and IP bucket pressure is the only
    //    shared throttle (capacity 60 — see ratelimit::Bucket::LOGIN_IP).
    let mut emails: Vec<String> = Vec::with_capacity(N);
    for i in 0..N {
        let email = format!("loadtest-{i}-{}@zeroship.test", Uuid::new_v4().simple());
        pg.execute(
            "INSERT INTO auth.users (email, name, password_hash, email_verified_at) \
             VALUES ($1::citext, $2, $3, NOW())",
            &[&email.as_str(), &"Load Test", &phc.as_str()],
        )
        .await
        .expect("seed user");
        emails.push(email);
    }

    // 4. Drain LOGIN_IP + LOGIN_EIP + LOGIN_EMAIL buckets so prior runs
    //    don't bleed in. Best-effort — the schema is pre-applied by
    //    Liquibase before the test connects, so the table exists.
    pg.execute(
        "DELETE FROM auth.rate_limits WHERE bucket_key LIKE 'login:%'",
        &[],
    )
    .await
    .expect("drain login rate-limit rows");

    // 5. Pre-fetch N login_challenges. Sequential because we're NOT timing
    //    this — the wall-time budget covers only the parallel /login flows.
    //    Each fresh_challenge() round-trips hydra's /oauth2/auth which
    //    yields a fresh challenge per call (hydra mints a new one even for
    //    identical params).
    let mut challenges: Vec<String> = Vec::with_capacity(N);
    for _ in 0..N {
        challenges.push(fixture.fresh_challenge().await);
    }

    // 6. Fire N parallel GET+POST /login flows. Each future is independent
    //    (own CSRF cookie, own challenge, own email). They share the cyper
    //    Client — its connection pool serialises HTTP/1 requests per
    //    connection, but spawns up multiple connections under load.
    //
    //    We measure per-request elapsed (GET+POST round-trip) AND total
    //    wall time (start → all futures resolved) so throughput reflects
    //    the actual parallelism the server delivered.
    let start = Instant::now();
    let mut tasks = Vec::with_capacity(N);
    for i in 0..N {
        let email = emails[i].clone();
        let challenge = challenges[i].clone();
        let http = http.clone();
        let auth_base = auth_base.clone();
        tasks.push(async move {
            let t0 = Instant::now();
            let login_url = format!("{auth_base}/login?login_challenge={challenge}");

            // GET /login → set CSRF cookie, render form.
            let get_resp = http
                .request(http::Method::GET, &login_url)
                .expect("build GET /login")
                .send()
                .await
                .expect("send GET /login");
            assert!(
                get_resp.status().is_success(),
                "GET /login expected 200, got {}",
                get_resp.status()
            );
            let csrf = read_set_cookie(&get_resp, "zsidp_csrf")
                .expect("zsidp_csrf cookie on GET /login");

            // POST /login → on success, 302 to hydra accept_login redirect_to.
            let body = url::form_urlencoded::Serializer::new(String::new())
                .append_pair("csrf", &csrf)
                .append_pair("email", &email)
                .append_pair("password", PASSWORD)
                .finish();
            let cookie_header = format!("zsidp_csrf={csrf}");
            let post_resp = http
                .request(http::Method::POST, &login_url)
                .expect("build POST /login")
                .header("content-type", "application/x-www-form-urlencoded")
                .expect("content-type")
                .header("cookie", &cookie_header)
                .expect("cookie header")
                .body(body)
                .send()
                .await
                .expect("send POST /login");
            (t0.elapsed(), post_resp.status().as_u16())
        });
    }
    let results: Vec<(Duration, u16)> = join_all(tasks).await;
    let total_elapsed = start.elapsed();

    // 7. Stats. p99 on N=50 lands on index 49 (max sample) — that's OK for
    //    regression purposes; it captures the slowest verify under
    //    contention.
    let mut latencies: Vec<Duration> = results.iter().map(|(d, _)| *d).collect();
    latencies.sort();
    let p50 = latencies[N / 2];
    let p99 = latencies[(N * 99 / 100).min(N - 1)];
    #[allow(clippy::cast_precision_loss)]
    let throughput = (N as f64) / total_elapsed.as_secs_f64();

    let mut ok = 0usize;
    let mut redirect = 0usize;
    let mut other = 0usize;
    for (_, status) in &results {
        match *status {
            200..=299 => ok += 1,
            300..=399 => redirect += 1,
            _ => other += 1,
        }
    }

    eprintln!("──── /login load test ────");
    eprintln!("N={N}, total={total_elapsed:?}");
    eprintln!("p50={p50:?}, p99={p99:?}");
    eprintln!("throughput={throughput:.1} RPS");
    eprintln!("statuses: {ok} 2xx, {redirect} 3xx, {other} other");

    // 8. Cleanup BEFORE assertions so a failure still leaves the DB clean.
    for email in &emails {
        let _ = pg
            .execute(
                "DELETE FROM auth.sessions WHERE user_id IN \
                 (SELECT id FROM auth.users WHERE email = $1::citext)",
                &[&email.as_str()],
            )
            .await;
        let _ = pg
            .execute(
                "DELETE FROM auth.users WHERE email = $1::citext",
                &[&email.as_str()],
            )
            .await;
    }
    let _ = pg
        .execute(
            "DELETE FROM auth.rate_limits WHERE bucket_key LIKE 'login:%'",
            &[],
        )
        .await;
    fixture.cleanup().await;

    // 9. Loose regression-detection bounds. All N must redirect to hydra
    //    (login success); any 4xx means CSRF/seed/rate-limit broke and the
    //    latency numbers below are meaningless.
    assert_eq!(
        redirect, N,
        "expected all {N} logins to 302 to hydra; got {redirect} 3xx, {ok} 2xx, {other} other"
    );
    assert!(
        p99 < P99_BUDGET,
        "p99 = {p99:?}, budget < {P99_BUDGET:?} — auth /login may have regressed"
    );
    assert!(
        throughput > RPS_FLOOR,
        "throughput = {throughput:.1} RPS, floor > {RPS_FLOOR} — auth /login throughput may have regressed"
    );
}
