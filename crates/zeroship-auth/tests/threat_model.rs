//! Login CSRF, framing, rate limits and session rotation through the auth server.

use crate::common;
use common::{
    auth_server::AuthServer, database::Database, read_set_cookie, CookieJar, TEST_CONSOLE_ORIGIN,
};

const CLIENT_IP: &str = "192.0.2.10";

fn login_url(fx: &AuthServer, return_to: &str) -> String {
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("return_to", return_to)
        .finish();
    format!("{}/login?{query}", fx.auth_base)
}

// ─── Tests ───────────────────────────────────────────────────────────────

/// §13 "Login CSRF": POST /login without the `csrf` form field must reject.
/// The handler short-circuits before any DB-backed login work, returning the
/// re-rendered login form at status 400.
#[ntex::test]
async fn login_csrf_missing_field_rejected() {
    Database::run(async |database| {
        let fx = AuthServer::start(database).await;

        let return_to = fx.fresh_challenge();
        let login_url = login_url(&fx, &return_to);

        // First GET /login so we have a cookie — but we intentionally omit the
        // `csrf` form field, so the cookie/form match must fail.
        let resp = fx
            .http
            .request(http::Method::GET, &login_url)
            .expect("build GET")
            .send()
            .await
            .expect("send GET");
        let csrf_cookie = read_set_cookie(&resp, "__Host-zsidp_csrf")
            .expect("__Host-zsidp_csrf cookie set on GET /login");

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
            .header("cookie", format!("__Host-zsidp_csrf={csrf_cookie}"))
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
    })
    .await;
}

/// §13 "Login CSRF": form `csrf` field ≠ cookie token → reject.
#[ntex::test]
async fn login_csrf_mismatched_token_rejected() {
    Database::run(async |database| {
        let fx = AuthServer::start(database).await;

        let return_to = fx.fresh_challenge();
        let login_url = login_url(&fx, &return_to);

        let resp = fx
            .http
            .request(http::Method::GET, &login_url)
            .expect("build GET")
            .send()
            .await
            .expect("send GET");
        let csrf_cookie = read_set_cookie(&resp, "__Host-zsidp_csrf")
            .expect("__Host-zsidp_csrf cookie set on GET /login");

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
            .header("cookie", format!("__Host-zsidp_csrf={csrf_cookie}"))
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
    })
    .await;
}

/// Immersive iframe login pivot (design §4.3/§6.3, §9): the framed login routes
/// (`/login`, `/signup`, the interactive `/consent` render) must NOW emit a
/// relaxed `frame-ancestors 'self' <console origin>` (read from the
/// `frame_ancestor_origins` config the fixture configures — NOT hard-coded) and
/// DROP `X-Frame-Options` entirely, while every OTHER auth route keeps the
/// fail-closed `X-Frame-Options: DENY` + `frame-ancestors 'none'`. This REPLACES
/// the old "all login pages set XFO DENY + frame-ancestors 'none'" assertion —
/// it is also the home of the regression test for the browser-enforced
/// `frame-ancestors` gate that supersedes the deleted gateway credential-oracle
/// first-party gate. A test that would have PASSED before the pivot (which set
/// XFO DENY on /login) FAILS now, and vice-versa.
#[ntex::test]
async fn login_clickjacking_headers_present() {
    Database::run(async |database| {
        let fx = AuthServer::start(database).await;

        // (a) The FRAMED route `/login` GET: relaxed frame-ancestors, NO XFO.
        let return_to = fx.fresh_challenge();
        let login_url = login_url(&fx, &return_to);
        let resp = fx
            .http
            .request(http::Method::GET, &login_url)
            .expect("build GET")
            .send()
            .await
            .expect("send GET");

        assert!(
            resp.headers().get("x-frame-options").is_none(),
            "framed /login must NOT carry X-Frame-Options (a legacy UA honoring it \
             would refuse the frame the CSP allows, §6.3)"
        );
        let csp = resp
            .headers()
            .get("content-security-policy")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        // Read the admitted origin from CONFIG, not a literal in this assertion.
        assert!(
            csp.contains(&format!("frame-ancestors 'self' {TEST_CONSOLE_ORIGIN}")),
            "framed /login CSP must allow 'self' + the configured console origin; got {csp:?}"
        );
        assert!(
            !csp.replace(' ', "").contains("frame-ancestors'none'"),
            "framed /login must NOT keep frame-ancestors 'none'; got {csp:?}"
        );

        // (b) A NON-framed route (`/healthz`) keeps the fail-closed default. We use
        // /healthz because it needs no auth context and is always registered; the
        // route-aware middleware decides framed-vs-strict purely on the path.
        let health_url = format!("{}/healthz", fx.auth_base);
        let resp = fx
            .http
            .request(http::Method::GET, &health_url)
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
            "a non-framed route must keep X-Frame-Options: DENY; got {xfo:?}"
        );
        let csp = resp
            .headers()
            .get("content-security-policy")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            csp.replace(' ', "").contains("frame-ancestors'none'"),
            "a non-framed route must keep frame-ancestors 'none'; got {csp:?}"
        );

        // (c) `/oauth/google` is NOT in the framed set — the federated IdP page is
        // never framed (it stays a popup). The path predicate is the single source
        // of truth for which routes relax; assert it directly so the federated
        // bounce can never accidentally inherit the relax even if google were
        // enabled in this fixture.
        assert!(
            !zeroship_auth::headers::is_framed_route_for_test("/oauth/google/start"),
            "/oauth/google must NOT be a framed route"
        );
        assert!(
            zeroship_auth::headers::is_framed_route_for_test("/login"),
            "/login must be a framed route"
        );
    })
    .await;
}

/// §13 row "Token leakage via Referer": Referrer-Policy: no-referrer on all
/// `crates/auth` UI pages.
#[ntex::test]
async fn login_referrer_policy_set() {
    Database::run(async |database| {
        let fx = AuthServer::start(database).await;

        let return_to = fx.fresh_challenge();
        let login_url = login_url(&fx, &return_to);
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
    })
    .await;
}

/// Repeated rejected credentials from the same identity must hit the login rate limit.
#[ntex::test]
async fn login_rate_limit_kicks_in() {
    Database::run(async |database| {
        let fx = AuthServer::start(database).await;

        let email = "ratelimit@zeroship.test".to_owned();
        let xff_ip = CLIENT_IP.to_owned();

        let mut last_status: u16 = 0;
        for i in 1..=6 {
            let return_to = fx.fresh_challenge();
            let login_url = login_url(&fx, &return_to);

            let resp = fx
                .http
                .request(http::Method::GET, &login_url)
                .expect("build GET")
                .header("X-Forwarded-For", xff_ip.as_str())
                .expect("xff")
                .send()
                .await
                .expect("send GET");
            let csrf = read_set_cookie(&resp, "__Host-zsidp_csrf")
                .expect("__Host-zsidp_csrf cookie set on GET /login");

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
                .header("cookie", format!("__Host-zsidp_csrf={csrf}"))
                .expect("cookie")
                .header("X-Forwarded-For", xff_ip.as_str())
                .expect("xff")
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
    })
    .await;
}

/// Drive one full successful POST /login round-trip and return the
/// `__Host-zsidp_session` cookie value. Helper for the rotation test.
//
// `AuthServer` carries `!Send` ntex/cyper handles.
#[allow(clippy::future_not_send)]
async fn one_login(fx: &AuthServer, email: &str, password: &str, xff_ip: &str) -> String {
    let return_to = fx.fresh_challenge();
    let login_url = login_url(fx, &return_to);

    let resp = fx
        .http
        .request(http::Method::GET, &login_url)
        .expect("build GET")
        .header("X-Forwarded-For", xff_ip)
        .expect("xff")
        .send()
        .await
        .expect("send GET");
    assert!(
        resp.status().is_success(),
        "GET /login expected 200, got {}",
        resp.status()
    );
    let csrf = read_set_cookie(&resp, "__Host-zsidp_csrf")
        .expect("__Host-zsidp_csrf cookie set on GET /login");

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
        .header("X-Forwarded-For", xff_ip)
        .expect("xff")
        .body(body)
        .send()
        .await
        .expect("send POST");
    assert_eq!(
        resp.status().as_u16(),
        303,
        "POST /login expected 303 success, got {}",
        resp.status()
    );
    read_set_cookie(&resp, "__Host-zsidp_session")
        .expect("__Host-zsidp_session cookie set on POST /login success")
}

/// Section 13 "Session fixation": `__Host-zsidp_session` rotates on successful
/// login. Drive two separate logins for the same user and assert the cookie
/// values differ.
#[ntex::test]
async fn session_id_rotates_post_login_success() {
    Database::run(async |database| {
        let fx = AuthServer::start(database).await;

        // Seed a user.
        let email = "rotate@zeroship.test".to_owned();
        let xff_ip = CLIENT_IP.to_owned();
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
                "INSERT INTO zeroship.users (email, name, password_hash) VALUES ($1::citext, $2, $3)",
                &[&email.as_str(), &"Rotate User", &phc.as_str()],
            )
            .await
            .expect("insert user");


        let sid1 = one_login(&fx, &email, password, &xff_ip).await;
        let sid2 = one_login(&fx, &email, password, &xff_ip).await;

        assert_ne!(
            sid1, sid2,
            "session id must rotate on each successful login (§13 session fixation)"
        );
        assert!(!sid1.is_empty(), "first sid must be non-empty");
        assert!(!sid2.is_empty(), "second sid must be non-empty");
    }).await;
}
