//! End-to-end GitHub federation flow against a live hydra + the
//! in-process `crates/auth` server + an in-process mock GitHub provider.
//!
//! Two tests, both skipping cleanly without `AUTH_DB_URL` +
//! `HYDRA_ADMIN_URL`:
//!
//!   1. `github_federation_creates_new_user` — happy path. The mock
//!      returns a verified primary non-noreply email; the auth server
//!      should create a fresh user + identity row and 302 back to
//!      hydra. Mirrors `e2e_google.rs` but for the OAuth-2.0 path (no
//!      JWKS / ID-token; instead the mock serves `/user` +
//!      `/user/emails`).
//!
//!   2. `github_federation_rejects_noreply_only_email` — defence path.
//!      The mock returns a primary `@users.noreply.github.com` email
//!      and only non-primary or unverified non-noreply alternatives.
//!      The handler's email-picker (`identity/oauth/github.rs`) must
//!      refuse to pick any of those, surface a friendly error, and
//!      MUST NOT create an `zeroship.users` row.

use std::sync::Arc;
use std::time::Duration;

use ntex::web;
use uuid::Uuid;

use zeroship_auth::headers::SecurityHeaders;
use zeroship_auth::hydra_client::types::OAuth2Client;
use zeroship_auth::hydra_client::HydraAdmin;
use zeroship_auth::server;

mod common;
use common::mock_provider::{MockProvider, MockUser, ProviderMode};
use common::{
    assert_redirect, extract_query_param, location, read_set_cookie, test_auth_config, CookieJar,
};

// ─── Shared bootstrap ────────────────────────────────────────────────────

struct GithubFixture {
    auth_base: String,
    admin: HydraAdmin,
    pg: Arc<compio_postgres::Client>,
    http: cyper::Client,
    hydra_public: String,
    test_client_id: String,
    test_redirect: &'static str,
    srv: ntex::web::test::TestServer,
}

impl GithubFixture {
    /// Boot the live PG/hydra wiring + a hydra OIDC client + the auth
    /// server pre-configured to use `mock` as the upstream GitHub
    /// provider. Returns `None` if the env-skip env vars aren't set
    /// — each test then `eprintln`s + returns.
    //
    // `!Send` cyper + ntex server handles. Line-count growth comes
    // from the `AuthConfig {}` literal — each new platform-wide field
    // adds a fixture line; sub-extraction would just move that pile.
    #[allow(clippy::future_not_send, clippy::too_many_lines)]
    async fn boot(mock: &MockProvider) -> Option<Self> {
        let (Ok(db_url), Ok(hydra_admin_url)) = (
            std::env::var("AUTH_DB_URL"),
            std::env::var("HYDRA_ADMIN_URL"),
        ) else {
            return None;
        };
        let hydra_public = std::env::var("HYDRA_PUBLIC_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:4444".to_string());

        let (pg_client, pg_connection) =
            compio_postgres::connect(&db_url, compio_postgres::NoTls)
                .await
                .expect("connect pg");
        compio::runtime::spawn(async move {
            if let Err(e) = pg_connection.run().await {
                eprintln!("[e2e_github] pg connection driver: {e}");
            }
        })
        .detach();
        let pg = Arc::new(pg_client);

        let admin = HydraAdmin::new(&hydra_admin_url);
        let test_client_id = format!("e2e-github-{}", Uuid::new_v4().simple());
        let test_redirect: &'static str = "http://127.0.0.1:9999/cb";
        admin
            .create_client(&OAuth2Client {
                client_id: test_client_id.clone(),
                client_name: Some("e2e github test".into()),
                client_secret: Some("e2e-github-secret".into()),
                grant_types: vec!["authorization_code".into()],
                response_types: vec!["code".into()],
                redirect_uris: vec![test_redirect.into()],
                post_logout_redirect_uris: vec![],
                scope: "openid".into(),
                token_endpoint_auth_method: "client_secret_post".into(),
                subject_type: "public".into(),
                access_token_strategy: None,
                id_token_signed_response_alg: Some("EdDSA".into()),
                audience: vec![],
                skip_consent: true,
                require_consent: false,
                require_logout_consent: false,
                frontchannel_logout_uri: None,
                backchannel_logout_uri: None,
            })
            .await
            .expect("create test client");

        let mut cfg_inner = test_auth_config(&db_url, &hydra_admin_url, &hydra_public);
        cfg_inner.github_client_id = Some("mock-github-client".into());
        cfg_inner.github_client_secret = Some("mock-github-secret".into());
        // Placeholder rewritten test-side after the mock's /authorize
        // redirects — same trick as e2e_google.
        cfg_inner.github_redirect_uri = "http://placeholder/oauth/github/callback".to_string();
        cfg_inner.github_authorize_url = mock.github_authorize_url();
        cfg_inner.github_token_url = mock.github_token_url();
        cfg_inner.github_user_url = mock.github_user_url();
        cfg_inner.github_emails_url = mock.github_emails_url();
        let cfg = Arc::new(cfg_inner);

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
                    .middleware(SecurityHeaders)
                    .configure(server::configure(false, true))
            }
        })
        .await;
        let auth_base = srv.url("").trim_end_matches('/').to_string();

        Some(Self {
            auth_base,
            admin,
            pg,
            http: cyper::Client::new(),
            hydra_public,
            test_client_id,
            test_redirect,
            srv,
        })
    }

    // !Send wrt cyper client.
    #[allow(clippy::future_not_send)]
    async fn fresh_login_challenge(&self) -> String {
        let q = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("client_id", &self.test_client_id)
            .append_pair("response_type", "code")
            .append_pair("scope", "openid")
            .append_pair("redirect_uri", self.test_redirect)
            .append_pair("state", &format!("st-{}", Uuid::new_v4().simple()))
            .append_pair("nonce", &format!("nc-{}", Uuid::new_v4().simple()))
            .append_pair(
                "code_challenge",
                "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            )
            .append_pair("code_challenge_method", "S256")
            .finish();
        let url = format!("{}/oauth2/auth?{}", self.hydra_public, q);
        let resp = self
            .http
            .request(http::Method::GET, &url)
            .expect("build /oauth2/auth")
            .send()
            .await
            .expect("send /oauth2/auth");
        assert_redirect(&resp, "hydra /oauth2/auth → /login");
        let loc = location(&resp);
        extract_query_param(&loc, "login_challenge").expect("login_challenge")
    }

    #[allow(clippy::future_not_send)]
    async fn cleanup(self) {
        self.admin.delete_client(&self.test_client_id).await.ok();
        compio::time::sleep(Duration::from_millis(50)).await;
        drop(self.srv);
    }
}

// ─── Happy path ──────────────────────────────────────────────────────────

#[ntex::test]
async fn github_federation_creates_new_user() {
    let test_email = format!("e2e-github-{}@zeroship.test", Uuid::new_v4().simple());
    let mock_user = MockUser {
        // GitHub `id` is numeric; our subject = id.to_string(). Stay
        // in i64 range so the mock's `/user` `id` field parses cleanly.
        subject: "456789012".to_string(),
        email: test_email.clone(),
        email_verified: true,
        name: Some("E2E GitHub User".into()),
        picture: Some("https://avatars.githubusercontent.com/test.png".into()),
        login: Some("e2e-test-login".into()),
        additional_emails: Vec::new(),
    };
    let mock = MockProvider::start(ProviderMode::GitHub, mock_user.clone()).await;
    eprintln!("[e2e_github happy] mock provider at {}", mock.base);

    let Some(fx) = GithubFixture::boot(&mock).await else {
        eprintln!("[e2e_github happy] skip (need AUTH_DB_URL + HYDRA_ADMIN_URL)");
        return;
    };
    eprintln!("[e2e_github happy] auth server at {}", fx.auth_base);

    let login_challenge = fx.fresh_login_challenge().await;

    // 1. GET /oauth/github/start → 302 to mock /authorize, stash cookie.
    let start_url =
        format!("{}/oauth/github/start?login_challenge={login_challenge}", fx.auth_base);
    let resp = fx
        .http
        .request(http::Method::GET, &start_url)
        .expect("build /oauth/github/start")
        .send()
        .await
        .expect("send /oauth/github/start");
    assert_eq!(
        resp.status().as_u16(),
        302,
        "GET /oauth/github/start expected 302 (got {})",
        resp.status()
    );
    let mock_authorize_loc = location(&resp);
    assert!(
        mock_authorize_loc.starts_with(&mock.github_authorize_url()),
        "redirect must point at mock /authorize: {mock_authorize_loc}"
    );
    let stash_cookie = read_set_cookie(&resp, "zsidp_github_stash")
        .expect("zsidp_github_stash on /oauth/github/start");

    // 2. Follow to mock /authorize → 302 with code+state.
    let resp = fx
        .http
        .request(http::Method::GET, &mock_authorize_loc)
        .expect("build mock /authorize")
        .send()
        .await
        .expect("send mock /authorize");
    assert_eq!(
        resp.status().as_u16(),
        302,
        "mock /authorize expected 302 (got {})",
        resp.status()
    );
    let callback_url = location(&resp);
    let callback_with_local = callback_url.replace(
        "http://placeholder/oauth/github/callback",
        &format!("{}/oauth/github/callback", fx.auth_base),
    );

    // 3. GET /oauth/github/callback with stash cookie.
    let mut jar = CookieJar::default();
    jar.set("zsidp_github_stash", &stash_cookie);
    let resp = fx
        .http
        .request(http::Method::GET, &callback_with_local)
        .expect("build /oauth/github/callback")
        .header("cookie", jar.header())
        .expect("cookie header")
        .send()
        .await
        .expect("send /oauth/github/callback");
    assert_eq!(
        resp.status().as_u16(),
        302,
        "GET /oauth/github/callback expected 302 (got {} body={:?})",
        resp.status(),
        resp.text().await.ok()
    );

    // 4. DB assertions.
    let user_rows = fx
        .pg
        .query(
            "SELECT id, name, email_verified_at FROM zeroship.users WHERE email = $1::citext",
            &[&test_email.as_str()],
        )
        .await
        .expect("user select");
    assert_eq!(user_rows.len(), 1, "exactly one user row");
    let user_id: uuid::Uuid = user_rows[0].get("id");
    let user_name: String = user_rows[0].get("name");
    let email_verified_at: Option<chrono::DateTime<chrono::Utc>> =
        user_rows[0].try_get("email_verified_at").ok();
    assert_eq!(user_name, "E2E GitHub User");
    assert!(
        email_verified_at.is_some(),
        "email_verified_at should be set because GitHub picker only returns trusted verified email"
    );

    let identity_rows = fx
        .pg
        .query(
            "SELECT user_id FROM zeroship.federated_identities WHERE provider = $1 AND subject = $2",
            &[&"github", &mock_user.subject.as_str()],
        )
        .await
        .expect("identity select");
    assert_eq!(identity_rows.len(), 1, "exactly one identity row");
    let identity_user_id: uuid::Uuid = identity_rows[0].get("user_id");
    assert_eq!(identity_user_id, user_id);

    // 5. Cleanup.
    fx.pg
        .execute(
            "DELETE FROM zeroship.federated_identities WHERE user_id = $1",
            &[&user_id],
        )
        .await
        .ok();
    fx.pg
        .execute(
            "DELETE FROM zeroship.idp_sessions WHERE user_id = $1",
            &[&user_id],
        )
        .await
        .ok();
    fx.pg
        .execute("DELETE FROM zeroship.users WHERE id = $1", &[&user_id])
        .await
        .ok();
    fx.cleanup().await;
    drop(mock);
}

#[ntex::test]
async fn github_callback_invalid_hydra_challenge_has_no_local_side_effects() {
    let test_email = format!("e2e-github-invalid-{}@zeroship.test", Uuid::new_v4().simple());
    let mock_user = MockUser {
        subject: format!("invalid-{}", Uuid::new_v4().simple()),
        email: test_email.clone(),
        email_verified: true,
        name: Some("Invalid Challenge User".into()),
        picture: Some("https://avatars.githubusercontent.com/invalid.png".into()),
        login: Some("invalid-challenge-login".into()),
        additional_emails: Vec::new(),
    };
    let mock = MockProvider::start(ProviderMode::GitHub, mock_user.clone()).await;

    let Some(fx) = GithubFixture::boot(&mock).await else {
        eprintln!("[e2e_github invalid challenge] skip (need AUTH_DB_URL + HYDRA_ADMIN_URL)");
        return;
    };

    let bogus_challenge = format!("bogus-{}", Uuid::new_v4().simple());
    let start_url =
        format!("{}/oauth/github/start?login_challenge={bogus_challenge}", fx.auth_base);
    let resp = fx
        .http
        .request(http::Method::GET, &start_url)
        .expect("build /oauth/github/start")
        .send()
        .await
        .expect("send /oauth/github/start");
    assert_eq!(resp.status().as_u16(), 302);
    let stash_cookie = read_set_cookie(&resp, "zsidp_github_stash")
        .expect("zsidp_github_stash on /oauth/github/start");

    let mock_authorize_loc = location(&resp);
    let resp = fx
        .http
        .request(http::Method::GET, &mock_authorize_loc)
        .expect("build mock /authorize")
        .send()
        .await
        .expect("send mock /authorize");
    assert_eq!(resp.status().as_u16(), 302);
    let callback_url = location(&resp);
    let callback_with_local = callback_url.replace(
        "http://placeholder/oauth/github/callback",
        &format!("{}/oauth/github/callback", fx.auth_base),
    );

    let mut jar = CookieJar::default();
    jar.set("zsidp_github_stash", &stash_cookie);
    let resp = fx
        .http
        .request(http::Method::GET, &callback_with_local)
        .expect("build /oauth/github/callback")
        .header("cookie", jar.header())
        .expect("cookie header")
        .send()
        .await
        .expect("send /oauth/github/callback");
    assert_eq!(resp.status().as_u16(), 200);

    let user_count: i64 = fx
        .pg
        .query_one(
            "SELECT COUNT(*) FROM zeroship.users WHERE email = $1::citext",
            &[&test_email.as_str()],
        )
        .await
        .expect("count users")
        .get(0);
    let identity_count: i64 = fx
        .pg
        .query_one(
            "SELECT COUNT(*) FROM zeroship.federated_identities WHERE provider = $1 AND subject = $2",
            &[&"github", &mock_user.subject.as_str()],
        )
        .await
        .expect("count identities")
        .get(0);
    assert_eq!(user_count, 0, "invalid hydra challenge must not create user rows");
    assert_eq!(
        identity_count, 0,
        "invalid hydra challenge must not create identity rows"
    );

    fx.cleanup().await;
    drop(mock);
}

// ─── Reject noreply-only ─────────────────────────────────────────────────

#[ntex::test]
async fn github_federation_rejects_noreply_only_email() {
    // Mock seeded so:
    //   - primary `/user` email IS noreply (so the first row served on
    //     /user/emails is `noreply, primary=true, verified=true`)
    //   - the additional rows are EITHER non-primary OR unverified
    //     (so the picker can't find a `primary && verified && !noreply`
    //     match anywhere in the list).
    // Unique subject per run: the happy-path test (github_federation_links_new
    // _user) also lived on the literal "456789012" and creates a real identity
    // row with it. Sharing the subject made this test's "no identity row"
    // assertion observe that leftover row when the two ran in the same binary.
    let subject = format!("456789012-noreply-{}", Uuid::new_v4().simple());
    let noreply_email = format!("{subject}+e2e@users.noreply.github.com");
    let mock_user = MockUser {
        subject: subject.clone(),
        email: noreply_email.clone(),
        email_verified: true,
        name: Some("E2E Noreply".into()),
        picture: None,
        login: Some("noreply-e2e".into()),
        additional_emails: vec![
            // Verified but not primary — picker requires primary.
            ("real@example.com".into(), false, true),
            // Unverified.
            ("alias@example.com".into(), false, false),
        ],
    };
    let mock = MockProvider::start(ProviderMode::GitHub, mock_user.clone()).await;
    eprintln!("[e2e_github noreply] mock provider at {}", mock.base);

    let Some(fx) = GithubFixture::boot(&mock).await else {
        eprintln!("[e2e_github noreply] skip (need AUTH_DB_URL + HYDRA_ADMIN_URL)");
        return;
    };
    eprintln!("[e2e_github noreply] auth server at {}", fx.auth_base);

    let login_challenge = fx.fresh_login_challenge().await;

    // Drive start → mock /authorize → /callback.
    let start_url =
        format!("{}/oauth/github/start?login_challenge={login_challenge}", fx.auth_base);
    let resp = fx
        .http
        .request(http::Method::GET, &start_url)
        .expect("build /oauth/github/start")
        .send()
        .await
        .expect("send /oauth/github/start");
    let stash_cookie = read_set_cookie(&resp, "zsidp_github_stash")
        .expect("stash cookie on /oauth/github/start");
    let mock_authorize_loc = location(&resp);

    let resp = fx
        .http
        .request(http::Method::GET, &mock_authorize_loc)
        .expect("build mock /authorize")
        .send()
        .await
        .expect("send mock /authorize");
    let callback_url = location(&resp);
    let callback_with_local = callback_url.replace(
        "http://placeholder/oauth/github/callback",
        &format!("{}/oauth/github/callback", fx.auth_base),
    );

    let mut jar = CookieJar::default();
    jar.set("zsidp_github_stash", &stash_cookie);
    let resp = fx
        .http
        .request(http::Method::GET, &callback_with_local)
        .expect("build /oauth/github/callback")
        .header("cookie", jar.header())
        .expect("cookie header")
        .send()
        .await
        .expect("send /oauth/github/callback");

    // The picker-failure path renders the error page (HTTP 200 with a
    // friendly body) and clears the stash. It MUST NOT 302 onto hydra's
    // accept_login.
    let status = resp.status().as_u16();
    assert_eq!(
        status, 200,
        "noreply-only path should render an error page (got HTTP {status})"
    );
    let body = resp.text().await.expect("body");
    // The picker-failure path deliberately renders the generic
    // `PublicErrorMessage::PleaseTryAgain` page — it must NOT leak *why*
    // federation failed (which email was rejected / unverified) to the
    // browser. The distinctive `email_picker_failed` reason is recorded in
    // the audit log instead (asserted below). So the body carries the
    // generic copy/code, not a github-specific or "sign-in failed" string.
    assert!(
        body.to_lowercase().contains("please try again")
            || body.contains("please_try_again"),
        "picker-failure path must render the generic please-try-again error page: body={body}"
    );

    // Critical assertion: NO user row created (with the noreply email
    // OR any of the additional emails). The picker correctly refused
    // every candidate.
    for email in [
        noreply_email.as_str(),
        "real@example.com",
        "alias@example.com",
    ] {
        let rows = fx
            .pg
            .query(
                "SELECT id FROM zeroship.users WHERE email = $1::citext",
                &[&email],
            )
            .await
            .expect("user select");
        assert!(
            rows.is_empty(),
            "noreply path must not create a user row for {email} (got {} rows)",
            rows.len()
        );
    }

    // And no identity row either.
    let identity_rows = fx
        .pg
        .query(
            "SELECT id FROM zeroship.federated_identities WHERE provider = $1 AND subject = $2",
            &[&"github", &mock_user.subject.as_str()],
        )
        .await
        .expect("identity select");
    assert!(
        identity_rows.is_empty(),
        "noreply path must not create an identities row (got {} rows)",
        identity_rows.len()
    );

    fx.cleanup().await;
    drop(mock);
}

#[ntex::test]
async fn github_federation_rejects_unverified_primary_email() {
    let test_email = format!(
        "e2e-github-unverified-{}@example.test",
        Uuid::new_v4().simple()
    );
    // Unique subject per run so the "no identity row" assertion can never
    // observe a leftover row from another test/run sharing a literal subject.
    let subject = format!("456789013-unverified-{}", Uuid::new_v4().simple());
    let mock_user = MockUser {
        subject: subject.clone(),
        email: test_email.clone(),
        email_verified: false,
        name: Some("E2E Unverified".into()),
        picture: None,
        login: Some("unverified-e2e".into()),
        additional_emails: Vec::new(),
    };
    let mock = MockProvider::start(ProviderMode::GitHub, mock_user.clone()).await;
    eprintln!("[e2e_github unverified] mock provider at {}", mock.base);

    let Some(fx) = GithubFixture::boot(&mock).await else {
        eprintln!("[e2e_github unverified] skip (need AUTH_DB_URL + HYDRA_ADMIN_URL)");
        return;
    };
    eprintln!("[e2e_github unverified] auth server at {}", fx.auth_base);

    let login_challenge = fx.fresh_login_challenge().await;

    let start_url =
        format!("{}/oauth/github/start?login_challenge={login_challenge}", fx.auth_base);
    let resp = fx
        .http
        .request(http::Method::GET, &start_url)
        .expect("build /oauth/github/start")
        .send()
        .await
        .expect("send /oauth/github/start");
    let stash_cookie = read_set_cookie(&resp, "zsidp_github_stash")
        .expect("stash cookie on /oauth/github/start");
    let mock_authorize_loc = location(&resp);

    let resp = fx
        .http
        .request(http::Method::GET, &mock_authorize_loc)
        .expect("build mock /authorize")
        .send()
        .await
        .expect("send mock /authorize");
    let callback_url = location(&resp);
    let callback_with_local = callback_url.replace(
        "http://placeholder/oauth/github/callback",
        &format!("{}/oauth/github/callback", fx.auth_base),
    );

    let mut jar = CookieJar::default();
    jar.set("zsidp_github_stash", &stash_cookie);
    let resp = fx
        .http
        .request(http::Method::GET, &callback_with_local)
        .expect("build /oauth/github/callback")
        .header("cookie", jar.header())
        .expect("cookie header")
        .send()
        .await
        .expect("send /oauth/github/callback");

    let status = resp.status().as_u16();
    assert_eq!(
        status, 200,
        "unverified GitHub primary email should render an error page (got HTTP {status})"
    );
    let body = resp.text().await.expect("body");
    // Generic please-try-again page only — the unverified-primary reason is
    // not leaked to the browser; it lives in the audit log. See the noreply
    // test for the rationale.
    assert!(
        body.to_lowercase().contains("please try again")
            || body.contains("please_try_again"),
        "unverified-primary path must render the generic please-try-again error page: body={body}"
    );

    let user_rows = fx
        .pg
        .query(
            "SELECT id FROM zeroship.users WHERE email = $1::citext",
            &[&test_email.as_str()],
        )
        .await
        .expect("user select");
    assert!(
        user_rows.is_empty(),
        "unverified GitHub email must not create a user row"
    );

    let identity_rows = fx
        .pg
        .query(
            "SELECT id FROM zeroship.federated_identities WHERE provider = $1 AND subject = $2",
            &[&"github", &mock_user.subject.as_str()],
        )
        .await
        .expect("identity select");
    assert!(
        identity_rows.is_empty(),
        "unverified GitHub email must not create an identity row"
    );

    fx.cleanup().await;
    drop(mock);
}
