//! End-to-end GitHub federation flow against a live hydra + the
//! in-process `crates/auth` server + an in-process mock GitHub provider.
//!
//! Two tests, both skipping cleanly without `AUTH_DB_URL` +
//! `AUTH_HYDRA_ADMIN`:
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
//!      MUST NOT create an `auth.users` row.

use std::sync::Arc;
use std::time::Duration;

use ntex::web;
use uuid::Uuid;

use zeroship_auth::config::AuthConfig;
use zeroship_auth::headers::SecurityHeaders;
use zeroship_auth::hydra_client::types::OAuth2Client;
use zeroship_auth::hydra_client::HydraAdmin;
use zeroship_auth::server;
use zeroship_auth::store::migrations;

mod common;
use common::mock_provider::{MockProvider, MockUser, ProviderMode};
use common::{
    assert_redirect, extract_query_param, location, read_set_cookie, CookieJar,
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
                eprintln!("[e2e_github] pg connection driver: {e}");
            }
        })
        .detach();
        migrations::migrate(&pg_client).await.expect("migrate");
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

        let cfg = Arc::new(AuthConfig {
            addr: "127.0.0.1:0".to_string(),
            db_url: db_url.clone(),
            hydra_admin: hydra_admin_url.clone(),
            hydra_public: hydra_public.clone(),
            clients_config: "ops/auth-clients.example.toml".to_string(),
            bootstrap: false,
            insecure_dev: true,
            google_client_id: None,
            google_client_secret: None,
            google_redirect_uri: "https://auth.zeroship.ai/oauth/google/callback".to_string(),
            google_auth_url: "https://accounts.google.com/o/oauth2/v2/auth".to_string(),
            google_token_url: "https://oauth2.googleapis.com/token".to_string(),
            google_jwks_url: "https://www.googleapis.com/oauth2/v3/certs".to_string(),
            google_issuer: "https://accounts.google.com".to_string(),
            github_client_id: Some("mock-github-client".into()),
            github_client_secret: Some("mock-github-secret".into()),
            // The placeholder is rewritten test-side after the mock's
            // /authorize redirects to it — same trick as e2e_google.
            github_redirect_uri: "http://placeholder/oauth/github/callback".to_string(),
            github_authorize_url: mock.github_authorize_url(),
            github_token_url: mock.github_token_url(),
            github_user_url: mock.github_user_url(),
            github_emails_url: mock.github_emails_url(),
            stash_signing_key: "test-stash-key-not-for-prod-32bytes!".to_string(),
            mailer: "stdout".to_string(),
            smtp_host: None,
            smtp_port: 587,
            smtp_username: None,
            smtp_password: None,
            smtp_starttls: true,
            resend_api_key: None,
            mail_from_email: "test@zeroship.test".to_string(),
            mail_from_name: "Test".to_string(),
            public_url: "http://localhost:0".to_string(),
            postmark_webhook_user: None,
            postmark_webhook_password: None,
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
        eprintln!("[e2e_github happy] skip (need AUTH_DB_URL + AUTH_HYDRA_ADMIN)");
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
    let stash_cookie = read_set_cookie(&resp, "__Host-zsidp_github_stash")
        .expect("__Host-zsidp_github_stash on /oauth/github/start");

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
    jar.set("__Host-zsidp_github_stash", &stash_cookie);
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
            "SELECT id, name FROM auth.users WHERE email = $1::citext",
            &[&test_email.as_str()],
        )
        .await
        .expect("user select");
    assert_eq!(user_rows.len(), 1, "exactly one user row");
    let user_id: uuid::Uuid = user_rows[0].get("id");
    let user_name: String = user_rows[0].get("name");
    assert_eq!(user_name, "E2E GitHub User");

    let identity_rows = fx
        .pg
        .query(
            "SELECT user_id FROM auth.identities WHERE provider = $1 AND subject = $2",
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
            "DELETE FROM auth.identities WHERE user_id = $1",
            &[&user_id],
        )
        .await
        .ok();
    fx.pg
        .execute(
            "DELETE FROM auth.sessions WHERE user_id = $1",
            &[&user_id],
        )
        .await
        .ok();
    fx.pg
        .execute("DELETE FROM auth.users WHERE id = $1", &[&user_id])
        .await
        .ok();
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
    let noreply_email = "456789012+e2e@users.noreply.github.com".to_string();
    let mock_user = MockUser {
        subject: "456789012".to_string(),
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
        eprintln!("[e2e_github noreply] skip (need AUTH_DB_URL + AUTH_HYDRA_ADMIN)");
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
    let stash_cookie = read_set_cookie(&resp, "__Host-zsidp_github_stash")
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
    jar.set("__Host-zsidp_github_stash", &stash_cookie);
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
    assert!(
        body.to_lowercase().contains("github") || body.to_lowercase().contains("sign-in failed"),
        "error page should mention github or sign-in failed: body={body}"
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
                "SELECT id FROM auth.users WHERE email = $1::citext",
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
            "SELECT id FROM auth.identities WHERE provider = $1 AND subject = $2",
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
