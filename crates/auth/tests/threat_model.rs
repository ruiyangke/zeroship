//! Parametric tests over the `[ours]` rows of proposal §13.
//!
//! Each `#[ntex::test]` covers one mitigation `crates/auth` owns:
//!
//! 1. `login_csrf_missing_field_rejected` — POST /login w/o `csrf` form field
//! 2. `login_csrf_mismatched_token_rejected` — bogus `csrf` form field
//! 3. `login_clickjacking_headers_present` — CSP `frame-ancestors 'none'` + XFO DENY
//! 4. `login_referrer_policy_set` — `Referrer-Policy: no-referrer`
//! 5. `login_rate_limit_kicks_in` — LOGIN_EIP bucket throttles the 6th attempt
//! 6. `session_id_rotates_post_login_success` — session cookie value differs across two logins
//!
//! Rows like "Open redirect on redirect_uri" and "Refresh-token reuse" are
//! [hydra]-owned per §13 and live outside this harness.
//!
//! Every test skips when `AUTH_DB_URL` and `AUTH_HYDRA_ADMIN` aren't both set
//! (mirroring `e2e_password.rs`).

use std::sync::Arc;
use std::time::Duration;

use ntex::web;
use uuid::Uuid;

use zeroship_auth::config::AuthConfig;
use zeroship_auth::hydra_client::types::OAuth2Client;
use zeroship_auth::hydra_client::HydraAdmin;
use zeroship_auth::server;
use zeroship_auth::store::migrations;

// ─── Helpers (duplicated from e2e_password.rs / enum_defense.rs to keep
//     the test file self-contained — see U8 brief Option A). ──────────────

fn extract_query_param(raw_url: &str, key: &str) -> Option<String> {
    let parsed = url::Url::parse(raw_url).ok()?;
    parsed
        .query_pairs()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
}

#[derive(Default)]
struct Jar {
    inner: std::collections::HashMap<String, String>,
}

impl Jar {
    fn absorb(&mut self, resp: &cyper::Response) {
        for hv in resp.headers().get_all(http::header::SET_COOKIE) {
            let Ok(s) = hv.to_str() else { continue };
            let first = s.split(';').next().unwrap_or("");
            if let Some((name, value)) = first.split_once('=') {
                let name = name.trim();
                let value = value.trim();
                if name.is_empty() {
                    continue;
                }
                if value.is_empty() {
                    self.inner.remove(name);
                } else {
                    self.inner.insert(name.to_string(), value.to_string());
                }
            }
        }
    }

    fn header(&self) -> String {
        let mut parts: Vec<String> =
            self.inner.iter().map(|(k, v)| format!("{k}={v}")).collect();
        parts.sort();
        parts.join("; ")
    }
}

fn read_set_cookie(resp: &cyper::Response, name: &str) -> Option<String> {
    for hv in resp.headers().get_all(http::header::SET_COOKIE) {
        let Ok(s) = hv.to_str() else { continue };
        let first = s.split(';').next().unwrap_or("");
        if let Some((n, v)) = first.split_once('=') {
            if n.trim() == name {
                return Some(v.trim().to_string());
            }
        }
    }
    None
}

fn location(resp: &cyper::Response) -> String {
    resp.headers()
        .get(http::header::LOCATION)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("")
        .to_string()
}

fn rewrite_to_hydra_loopback(raw_url: &str) -> String {
    for prefix in ["https://auth.zeroship.ai", "http://auth.zeroship.ai"] {
        if let Some(rest) = raw_url.strip_prefix(prefix) {
            return format!("http://127.0.0.1:4444{rest}");
        }
    }
    raw_url.to_string()
}

async fn fresh_login_challenge(
    http: &cyper::Client,
    hydra_public: &str,
    client_id: &str,
    redirect_uri: &str,
) -> String {
    let q = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("client_id", client_id)
        .append_pair("response_type", "code")
        .append_pair("scope", "openid")
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("state", &format!("st-{}", Uuid::new_v4().simple()))
        .append_pair("nonce", &format!("nc-{}", Uuid::new_v4().simple()))
        .append_pair("code_challenge", "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA")
        .append_pair("code_challenge_method", "S256")
        .finish();
    let url = format!("{hydra_public}/oauth2/auth?{q}");
    let resp = http
        .request(http::Method::GET, &url)
        .expect("build /oauth2/auth")
        .send()
        .await
        .expect("send /oauth2/auth");
    let loc = location(&resp);
    extract_query_param(&loc, "login_challenge")
        .unwrap_or_else(|| panic!("hydra /oauth2/auth → /login redirect carries no login_challenge: {loc}"))
}

/// Common bootstrap. Returns `None` if env-skip applies.
#[allow(dead_code)] // `test_secret` retained for future tests that need to drive the token endpoint
struct Fixture {
    srv: ntex::web::test::TestServer,
    auth_base: String,
    admin: HydraAdmin,
    pg: Arc<compio_postgres::Client>,
    http: cyper::Client,
    test_client_id: String,
    test_secret: String,
    test_redirect: &'static str,
    hydra_public: String,
}

impl Fixture {
    async fn boot() -> Option<Self> {
        let (Ok(db_url), Ok(hydra_admin_url)) = (
            std::env::var("AUTH_DB_URL"),
            std::env::var("AUTH_HYDRA_ADMIN"),
        ) else {
            return None;
        };
        let hydra_public = std::env::var("AUTH_HYDRA_PUBLIC_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:4444".to_string());

        let (pg_client, pg_connection) =
            compio_postgres::connect(&db_url, compio_postgres::NoTls)
                .await
                .expect("connect pg");
        compio::runtime::spawn(async move {
            if let Err(e) = pg_connection.run().await {
                eprintln!("[threat_model] pg connection driver: {e}");
            }
        })
        .detach();
        migrations::migrate(&pg_client).await.expect("migrate");
        let pg = Arc::new(pg_client);

        let admin = HydraAdmin::new(&hydra_admin_url);
        let cfg = Arc::new(AuthConfig {
            addr: "127.0.0.1:0".to_string(),
            db_url: db_url.clone(),
            hydra_admin: hydra_admin_url.clone(),
            hydra_public: hydra_public.clone(),
            clients_config: "ops/auth-clients.example.toml".to_string(),
            bootstrap: false,
            insecure_dev: true,
        });
        let admin_state = admin.clone();
        let cfg_state = cfg.clone();
        let db_state = pg.clone();
        let srv = web::test::server(move || {
            let admin_state = admin_state.clone();
            let cfg_state = cfg_state.clone();
            let db_state = db_state.clone();
            async move {
                web::App::new()
                    .state(admin_state)
                    .state(cfg_state)
                    .state(db_state)
                    .configure(server::configure)
            }
        })
        .await;
        let auth_base = srv.url("").trim_end_matches('/').to_string();

        // Register a fresh hydra OIDC client per fixture so tests are
        // isolated. `skip_consent=true` lets the rotation test reach the
        // post-login session cookie without rendering /consent.
        let test_client_id = format!("threat-{}", Uuid::new_v4().simple());
        let test_secret = "threat-test-secret".to_string();
        let test_redirect: &'static str = "http://127.0.0.1:9999/cb";
        admin
            .create_client(&OAuth2Client {
                client_id: test_client_id.clone(),
                client_name: Some("threat test".into()),
                client_secret: Some(test_secret.clone()),
                grant_types: vec!["authorization_code".into()],
                response_types: vec!["code".into()],
                redirect_uris: vec![test_redirect.into()],
                post_logout_redirect_uris: vec![],
                scope: "openid".into(),
                token_endpoint_auth_method: "client_secret_post".into(),
                subject_type: "public".into(),
                access_token_strategy: None,
                id_token_signed_response_alg: Some("RS256".into()),
                audience: vec![],
                skip_consent: true,
                require_consent: false,
                require_logout_consent: false,
                frontchannel_logout_uri: None,
                backchannel_logout_uri: None,
            })
            .await
            .expect("create test client");

        Some(Self {
            srv,
            auth_base,
            admin,
            pg,
            http: cyper::Client::new(),
            test_client_id,
            test_secret,
            test_redirect,
            hydra_public,
        })
    }

    async fn cleanup(self) {
        let _ = self.admin.delete_client(&self.test_client_id).await;
        compio::time::sleep(Duration::from_millis(50)).await;
        drop(self.srv);
    }

    async fn fresh_challenge(&self) -> String {
        fresh_login_challenge(
            &self.http,
            &self.hydra_public,
            &self.test_client_id,
            self.test_redirect,
        )
        .await
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────

/// §13 "Login CSRF": POST /login without the `csrf` form field must reject.
/// The handler short-circuits before any DB or hydra call, returning the
/// re-rendered login form at status 400.
#[ntex::test]
async fn login_csrf_missing_field_rejected() {
    let Some(fx) = Fixture::boot().await else {
        eprintln!("[threat_model::csrf_missing] skip (need AUTH_DB_URL + AUTH_HYDRA_ADMIN)");
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

    fx.cleanup().await;
}

/// §13 "Login CSRF": form `csrf` field ≠ cookie token → reject.
#[ntex::test]
async fn login_csrf_mismatched_token_rejected() {
    let Some(fx) = Fixture::boot().await else {
        eprintln!("[threat_model::csrf_mismatch] skip (need AUTH_DB_URL + AUTH_HYDRA_ADMIN)");
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

    fx.cleanup().await;
}

/// §13 "Clickjacking on consent": login pages must set `X-Frame-Options:
/// DENY` and a CSP carrying `frame-ancestors 'none'`. The same headers
/// apply across login/signup/consent per §14.
#[ntex::test]
async fn login_clickjacking_headers_present() {
    let Some(fx) = Fixture::boot().await else {
        eprintln!("[threat_model::clickjacking] skip (need AUTH_DB_URL + AUTH_HYDRA_ADMIN)");
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
    let Some(fx) = Fixture::boot().await else {
        eprintln!("[threat_model::referrer] skip (need AUTH_DB_URL + AUTH_HYDRA_ADMIN)");
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

/// §13 "Brute force / credential stuffing": LOGIN_EIP bucket (capacity 5)
/// must throttle the 6th login attempt within a 15-min window for the same
/// `(email, ip)` tuple.
#[ntex::test]
async fn login_rate_limit_kicks_in() {
    let Some(fx) = Fixture::boot().await else {
        eprintln!("[threat_model::rate_limit] skip (need AUTH_DB_URL + AUTH_HYDRA_ADMIN)");
        return;
    };

    // Unique email so we don't collide with concurrent / leftover state in
    // the bucket from other tests.
    let email = format!("ratelimit-{}@zeroship.test", Uuid::new_v4().simple());

    // Clean the EIP bucket entries for this email explicitly. The IP bucket
    // (login:ip:127.0.0.1) is shared across all tests this session; its
    // capacity (60/hour) is large enough that the other tests in this
    // binary won't drain it ahead of us.
    fx.pg
        .execute(
            "DELETE FROM auth.rate_limits WHERE bucket_key LIKE $1",
            &[&format!("login:eip:{email}:%")],
        )
        .await
        .ok();
    fx.pg
        .execute(
            "DELETE FROM auth.rate_limits WHERE bucket_key = $1",
            &[&format!("login:email:{email}")],
        )
        .await
        .ok();

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
    fx.pg
        .execute(
            "DELETE FROM auth.rate_limits WHERE bucket_key LIKE $1",
            &[&format!("login:eip:{email}:%")],
        )
        .await
        .ok();
    fx.pg
        .execute(
            "DELETE FROM auth.rate_limits WHERE bucket_key = $1",
            &[&format!("login:email:{email}")],
        )
        .await
        .ok();

    fx.cleanup().await;
}

/// Drive one full successful POST /login round-trip and return the
/// `__Host-zsidp_session` cookie value. Helper for the rotation test.
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
    let csrf = read_set_cookie(&resp, "__Host-zsidp_csrf")
        .expect("__Host-zsidp_csrf cookie set on GET /login");

    let mut jar = Jar::default();
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
    let sid = read_set_cookie(&resp, "__Host-zsidp_session")
        .expect("__Host-zsidp_session cookie set on POST /login success");

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

/// §13 "Session fixation": `__Host-zsidp_session` rotates on successful
/// login. Drive two separate logins for the same user and assert the cookie
/// values differ.
#[ntex::test]
async fn session_id_rotates_post_login_success() {
    let Some(fx) = Fixture::boot().await else {
        eprintln!("[threat_model::rotate] skip (need AUTH_DB_URL + AUTH_HYDRA_ADMIN)");
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
    fx.pg
        .execute(
            "DELETE FROM auth.rate_limits WHERE bucket_key LIKE $1 \
              OR bucket_key = $2",
            &[
                &format!("login:eip:{email}:%"),
                &format!("login:email:{email}"),
            ],
        )
        .await
        .ok();

    let sid1 = one_login(&fx, &email, password).await;
    let sid2 = one_login(&fx, &email, password).await;

    assert_ne!(
        sid1, sid2,
        "session id must rotate on each successful login (§13 session fixation)"
    );
    assert!(!sid1.is_empty(), "first sid must be non-empty");
    assert!(!sid2.is_empty(), "second sid must be non-empty");

    // Cleanup.
    fx.pg
        .execute(
            "DELETE FROM auth.sessions WHERE user_id IN \
             (SELECT id FROM auth.users WHERE email = $1::citext)",
            &[&email.as_str()],
        )
        .await
        .ok();
    fx.pg
        .execute(
            "DELETE FROM auth.users WHERE email = $1::citext",
            &[&email.as_str()],
        )
        .await
        .ok();
    fx.pg
        .execute(
            "DELETE FROM auth.rate_limits WHERE bucket_key LIKE $1 \
              OR bucket_key = $2",
            &[
                &format!("login:eip:{email}:%"),
                &format!("login:email:{email}"),
            ],
        )
        .await
        .ok();

    fx.cleanup().await;
}
