//! Parametric tests over the `[ours]` rows of proposal §13.
//!
//! Each `#[ntex::test]` covers one mitigation `crates/auth` owns:
//!
//! 1. `login_csrf_missing_field_rejected` — POST /login w/o `csrf` form field
//! 2. `login_csrf_mismatched_token_rejected` — bogus `csrf` form field
//! 3. `login_clickjacking_headers_present` — CSP `frame-ancestors 'none'` + XFO DENY
//! 4. `login_referrer_policy_set` — `Referrer-Policy: no-referrer`
//! 5. `login_rate_limit_kicks_in` — `LOGIN_EIP` bucket throttles the 6th attempt
//! 6. `session_id_rotates_post_login_success` — session cookie value differs across two logins
//!
//! Rows like "Open redirect on `redirect_uri`" and "Refresh-token reuse" are
//! [hydra]-owned per §13 and live outside this harness.
//!
//! Every test skips when `AUTH_DB_URL` and `HYDRA_ADMIN_URL` aren't both set
//! (mirroring `e2e_password.rs`).

use uuid::Uuid;

mod common;
use common::{
    cleanup_rate_limits_like, cleanup_user, location, read_set_cookie, rewrite_to_hydra_loopback,
    CookieJar, Fixture,
};

// ─── Tests ───────────────────────────────────────────────────────────────

/// §13 "Login CSRF": POST /login without the `csrf` form field must reject.
/// The handler short-circuits before any DB or hydra call, returning the
/// re-rendered login form at status 400.
#[ntex::test]
async fn login_csrf_missing_field_rejected() {
    let Some(fx) = Fixture::boot("threat").await else {
        eprintln!("[threat_model::csrf_missing] skip (need AUTH_DB_URL + HYDRA_ADMIN_URL)");
        return;
    };

    let challenge = fx.fresh_challenge().await;
    let login_url = format!("{}/login?login_challenge={challenge}", fx.auth_base);

    // First GET /login so we have a cookie — but we intentionally omit the
    // `csrf` form field, so the cookie/form match must fail.
    let resp = fx
        .http
        .request(http::Method::GET, &login_url)
        .expect("build GET")
        .send()
        .await
        .expect("send GET");
    let csrf_cookie = read_set_cookie(&resp, "zsidp_csrf")
        .expect("zsidp_csrf cookie set on GET /login");

    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("email", "anyone@zeroship.test")
        .append_pair("password", "any-password-no-csrf-1234567890")
        .finish();
    let resp = fx
        .http
        .request(http::Method::POST, &login_url)
        .expect("build POST")
        .header("content-type", "application/x-www-form-urlencoded")
        .expect("ct")
        .header("cookie", format!("zsidp_csrf={csrf_cookie}"))
        .expect("cookie")
        .body(body)
        .send()
        .await
        .expect("send POST");

    // ntex's `Form` extractor rejects with 400 when a required field is
    // missing. The handler-level CSRF check would also produce 400; either
    // shape proves the request was not accepted.
    assert_eq!(
        resp.status().as_u16(),
        400,
        "POST /login with no `csrf` form field must be 400"
    );

    fx.cleanup().await;
}

/// §13 "Login CSRF": form `csrf` field ≠ cookie token → reject.
#[ntex::test]
async fn login_csrf_mismatched_token_rejected() {
    let Some(fx) = Fixture::boot("threat").await else {
        eprintln!("[threat_model::csrf_mismatch] skip (need AUTH_DB_URL + HYDRA_ADMIN_URL)");
        return;
    };

    let challenge = fx.fresh_challenge().await;
    let login_url = format!("{}/login?login_challenge={challenge}", fx.auth_base);

    let resp = fx
        .http
        .request(http::Method::GET, &login_url)
        .expect("build GET")
        .send()
        .await
        .expect("send GET");
    let csrf_cookie = read_set_cookie(&resp, "zsidp_csrf")
        .expect("zsidp_csrf cookie set on GET /login");

    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("csrf", "completely-different-bogus-token-value")
        .append_pair("email", "anyone@zeroship.test")
        .append_pair("password", "any-password-mismatch-1234567890")
        .finish();
    let resp = fx
        .http
        .request(http::Method::POST, &login_url)
        .expect("build POST")
        .header("content-type", "application/x-www-form-urlencoded")
        .expect("ct")
        .header("cookie", format!("zsidp_csrf={csrf_cookie}"))
        .expect("cookie")
        .body(body)
        .send()
        .await
        .expect("send POST");

    // `render_login_error("invalid request", 400)` on cookie/form mismatch.
    assert_eq!(
        resp.status().as_u16(),
        400,
        "POST /login with mismatched csrf must render 400"
    );
    let body_text = resp.text().await.expect("body");
    assert!(
        body_text.contains("invalid request"),
        "rendered error body should mention 'invalid request'; got: {body_text}"
    );

    fx.cleanup().await;
}

/// §13 "Clickjacking on consent": login pages must set `X-Frame-Options:
/// DENY` and a CSP carrying `frame-ancestors 'none'`. The same headers
/// apply across login/signup/consent per §14.
#[ntex::test]
async fn login_clickjacking_headers_present() {
    let Some(fx) = Fixture::boot("threat").await else {
        eprintln!("[threat_model::clickjacking] skip (need AUTH_DB_URL + HYDRA_ADMIN_URL)");
        return;
    };

    let challenge = fx.fresh_challenge().await;
    let login_url = format!("{}/login?login_challenge={challenge}", fx.auth_base);
    let resp = fx
        .http
        .request(http::Method::GET, &login_url)
        .expect("build GET")
        .send()
        .await
        .expect("send GET");

    let xfo = resp
        .headers()
        .get("x-frame-options")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert_eq!(
        xfo.to_ascii_uppercase(),
        "DENY",
        "X-Frame-Options must be DENY on /login; got {xfo:?}"
    );

    let csp = resp
        .headers()
        .get("content-security-policy")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        csp.replace(' ', "").contains("frame-ancestors'none'"),
        "CSP must include frame-ancestors 'none' on /login; got {csp:?}"
    );

    fx.cleanup().await;
}

/// §13 row "Token leakage via Referer": Referrer-Policy: no-referrer on all
/// `crates/auth` UI pages.
#[ntex::test]
async fn login_referrer_policy_set() {
    let Some(fx) = Fixture::boot("threat").await else {
        eprintln!("[threat_model::referrer] skip (need AUTH_DB_URL + HYDRA_ADMIN_URL)");
        return;
    };

    let challenge = fx.fresh_challenge().await;
    let login_url = format!("{}/login?login_challenge={challenge}", fx.auth_base);
    let resp = fx
        .http
        .request(http::Method::GET, &login_url)
        .expect("build GET")
        .send()
        .await
        .expect("send GET");

    let rp = resp
        .headers()
        .get("referrer-policy")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert_eq!(
        rp.to_ascii_lowercase(),
        "no-referrer",
        "Referrer-Policy must be no-referrer on /login; got {rp:?}"
    );

    fx.cleanup().await;
}

/// §13 "Brute force / credential stuffing": `LOGIN_EIP` bucket (capacity 5)
/// must throttle the 6th login attempt within a 15-min window for the same
/// `(email, ip)` tuple.
#[ntex::test]
async fn login_rate_limit_kicks_in() {
    let Some(fx) = Fixture::boot("threat").await else {
        eprintln!("[threat_model::rate_limit] skip (need AUTH_DB_URL + HYDRA_ADMIN_URL)");
        return;
    };

    // Unique email so we don't collide with concurrent / leftover state in
    // the bucket from other tests.
    let email = format!("ratelimit-{}@zeroship.test", Uuid::new_v4().simple());

    // Clean the EIP bucket entries for this email explicitly. The IP bucket
    // (login:ip:127.0.0.1) is shared across all tests this session; its
    // capacity (60/hour) is large enough that the other tests in this
    // binary won't drain it ahead of us.
    let eip_pat = format!("login:eip:{email}:%");
    let email_pat = format!("login:email:{email}");
    cleanup_rate_limits_like(&fx.pg, &[&eip_pat, &email_pat]).await;

    let mut last_status: u16 = 0;
    for i in 1..=6 {
        let challenge = fx.fresh_challenge().await;
        let login_url = format!("{}/login?login_challenge={challenge}", fx.auth_base);

        let resp = fx
            .http
            .request(http::Method::GET, &login_url)
            .expect("build GET")
            .send()
            .await
            .expect("send GET");
        let csrf = read_set_cookie(&resp, "zsidp_csrf")
            .expect("zsidp_csrf cookie set on GET /login");

        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("csrf", &csrf)
            .append_pair("email", &email)
            .append_pair("password", "wrong-but-irrelevant-1234567890")
            .finish();
        let resp = fx
            .http
            .request(http::Method::POST, &login_url)
            .expect("build POST")
            .header("content-type", "application/x-www-form-urlencoded")
            .expect("ct")
            .header("cookie", format!("zsidp_csrf={csrf}"))
            .expect("cookie")
            .body(body)
            .send()
            .await
            .expect("send POST");
        last_status = resp.status().as_u16();
        eprintln!("[threat_model::rate_limit] attempt {i} → {last_status}");
        if i <= 5 {
            assert_eq!(
                last_status, 401,
                "attempt {i} should fail w/ invalid credentials (401), got {last_status}"
            );
        }
    }
    assert_eq!(
        last_status, 429,
        "6th attempt for same (email, ip) must be rate-limited (429); got {last_status}"
    );

    // Cleanup the EIP bucket so re-runs of this test don't carry state.
    cleanup_rate_limits_like(&fx.pg, &[&eip_pat, &email_pat]).await;

    fx.cleanup().await;
}

/// Drive one full successful POST /login round-trip and return the
/// `zsidp_session` cookie value. Helper for the rotation test.
//
// `Fixture` carries `!Send` ntex/cyper handles.
#[allow(clippy::future_not_send)]
async fn one_login(fx: &Fixture, email: &str, password: &str) -> String {
    let challenge = fx.fresh_challenge().await;
    let login_url = format!("{}/login?login_challenge={challenge}", fx.auth_base);

    let resp = fx
        .http
        .request(http::Method::GET, &login_url)
        .expect("build GET")
        .send()
        .await
        .expect("send GET");
    assert!(
        resp.status().is_success(),
        "GET /login expected 200, got {}",
        resp.status()
    );
    let csrf = read_set_cookie(&resp, "zsidp_csrf")
        .expect("zsidp_csrf cookie set on GET /login");

    let mut jar = CookieJar::default();
    jar.absorb(&resp);

    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("csrf", &csrf)
        .append_pair("email", email)
        .append_pair("password", password)
        .finish();
    let resp = fx
        .http
        .request(http::Method::POST, &login_url)
        .expect("build POST")
        .header("content-type", "application/x-www-form-urlencoded")
        .expect("ct")
        .header("cookie", jar.header())
        .expect("cookie")
        .body(body)
        .send()
        .await
        .expect("send POST");
    assert_eq!(
        resp.status().as_u16(),
        302,
        "POST /login expected 302 success, got {}",
        resp.status()
    );
    let sid = read_set_cookie(&resp, "zsidp_session")
        .expect("zsidp_session cookie set on POST /login success");

    // Drain the hydra redirect so the consumed login_challenge doesn't
    // linger in a half-completed state — important for the second
    // login, which needs a new challenge.
    let to_hydra = location(&resp);
    if !to_hydra.is_empty() {
        let to_hydra_local = rewrite_to_hydra_loopback(&to_hydra);
        // Best-effort — we don't assert on the outcome; we just want
        // hydra to clean up the consumed challenge.
        let _ = fx
            .http
            .request(http::Method::GET, &to_hydra_local)
            .expect("build hydra follow")
            .send()
            .await;
    }
    sid
}

/// §13 "Session fixation": `zsidp_session` rotates on successful
/// login. Drive two separate logins for the same user and assert the cookie
/// values differ.
#[ntex::test]
async fn session_id_rotates_post_login_success() {
    let Some(fx) = Fixture::boot("threat").await else {
        eprintln!("[threat_model::rotate] skip (need AUTH_DB_URL + HYDRA_ADMIN_URL)");
        return;
    };

    // Seed a user.
    let email = format!("rotate-{}@zeroship.test", Uuid::new_v4().simple());
    let password = "rotation-test-password-1234567890";
    let phc = compio::runtime::spawn_blocking({
        let pw = password.to_string();
        move || zeroship_auth::identity::password::hash(&pw)
    })
    .await
    .expect("hash spawn")
    .expect("hash ok");
    fx.pg
        .execute(
            "INSERT INTO auth.users (email, name, password_hash) VALUES ($1::citext, $2, $3)",
            &[&email.as_str(), &"Rotate User", &phc.as_str()],
        )
        .await
        .expect("insert user");

    // Reset EIP/email buckets for this email so two back-to-back logins
    // both succeed.
    let eip_pat = format!("login:eip:{email}:%");
    let email_pat = format!("login:email:{email}");
    cleanup_rate_limits_like(&fx.pg, &[&eip_pat, &email_pat]).await;

    let sid1 = one_login(&fx, &email, password).await;
    let sid2 = one_login(&fx, &email, password).await;

    assert_ne!(
        sid1, sid2,
        "session id must rotate on each successful login (§13 session fixation)"
    );
    assert!(!sid1.is_empty(), "first sid must be non-empty");
    assert!(!sid2.is_empty(), "second sid must be non-empty");

    // Cleanup.
    cleanup_user(&fx.pg, &email).await;
    cleanup_rate_limits_like(&fx.pg, &[&eip_pat, &email_pat]).await;

    fx.cleanup().await;
}
