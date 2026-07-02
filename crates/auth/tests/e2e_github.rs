//! End-to-end GitHub federation flow against the in-process `crates/auth`
//! server + an in-process mock GitHub provider.
//!
//! Tests skip cleanly without `AUTH_DB_URL`:
//!
//!   1. `github_native_callback_resumes_authorize_with_session_cookie` —
//!      happy path. The mock returns a verified primary non-noreply email;
//!      the auth server should create a fresh user + identity row and 303
//!      back to the native OP authorize URL.
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
use zeroship_auth::identity::linker::PendingLink;
use zeroship_auth::identity::password;
use zeroship_auth::server;
use zeroship_auth::store::users;

mod common;
use common::mock_provider::{MockProvider, MockUser, ProviderMode};
use common::{location, read_set_cookie, test_auth_config, CookieJar};

const TEST_STASH_KEY: &[u8] = b"test-stash-key-not-for-prod-32bytes!";

fn github_numeric_subject() -> String {
    let n = 100_000_000_u128 + (Uuid::new_v4().as_u128() % 800_000_000_u128);
    n.to_string()
}

fn native_authorize_return_to() -> String {
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("client_id", &format!("native-{}", Uuid::new_v4().simple()))
        .append_pair("response_type", "code")
        .append_pair("scope", "openid email")
        .append_pair("redirect_uri", "https://app.zeroship.test/callback")
        .append_pair("state", &format!("st-{}", Uuid::new_v4().simple()))
        .append_pair("nonce", &format!("nc-{}", Uuid::new_v4().simple()))
        .append_pair(
            "code_challenge",
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        )
        .append_pair("code_challenge_method", "S256")
        .finish();
    format!("/oauth2/authorize?{query}")
}

fn relative_query_param(raw_path: &str, key: &str) -> Option<String> {
    let parsed = url::Url::parse(&format!("http://auth.test{raw_path}")).ok()?;
    parsed
        .query_pairs()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.into_owned())
}

// ─── Shared bootstrap ────────────────────────────────────────────────────

struct NativeGithubFixture {
    auth_base: String,
    pg: Arc<compio_postgres::Client>,
    http: cyper::Client,
    srv: ntex::web::test::TestServer,
}

impl NativeGithubFixture {
    #[allow(clippy::future_not_send)]
    async fn boot(mock: &MockProvider) -> Option<Self> {
        let Ok(db_url) = std::env::var("AUTH_DB_URL") else {
            return None;
        };

        let (pg_client, pg_connection) =
            compio_postgres::connect(&db_url, compio_postgres::NoTls)
                .await
                .expect("connect pg");
        compio::runtime::spawn(async move {
            if let Err(e) = pg_connection.run().await {
                eprintln!("[e2e_github native] pg connection driver: {e}");
            }
        })
        .detach();
        let pg = Arc::new(pg_client);

        let mut cfg_inner = test_auth_config(&db_url);
        cfg_inner.github_client_id = Some("mock-github-client".into());
        cfg_inner.github_client_secret = Some("mock-github-secret".into());
        cfg_inner.github_redirect_uri = "http://placeholder/oauth/github/callback".to_string();
        cfg_inner.github_authorize_url = mock.github_authorize_url();
        cfg_inner.github_token_url = mock.github_token_url();
        cfg_inner.github_user_url = mock.github_user_url();
        cfg_inner.github_emails_url = mock.github_emails_url();
        let cfg = Arc::new(cfg_inner);

        let cfg_state = cfg.clone();
        let db_state = pg.clone();
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
                    .configure(server::configure(false, true))
            }
        })
        .await;
        let auth_base = srv.url("").trim_end_matches('/').to_string();

        Some(Self {
            auth_base,
            pg,
            http: cyper::Client::new(),
            srv,
        })
    }

    #[allow(clippy::future_not_send)]
    async fn cleanup(self) {
        compio::time::sleep(Duration::from_millis(50)).await;
        drop(self.srv);
    }
}

// ─── Happy path ──────────────────────────────────────────────────────────

#[ntex::test]
async fn github_native_callback_resumes_authorize_with_session_cookie() {
    let test_email = format!(
        "e2e-github-native-{}@zeroship.test",
        Uuid::new_v4().simple()
    );
    let mock_user = MockUser {
        subject: github_numeric_subject(),
        email: test_email.clone(),
        email_verified: true,
        name: Some("Native GitHub User".into()),
        picture: Some("https://avatars.githubusercontent.com/native.png".into()),
        login: Some("native-callback-login".into()),
        additional_emails: Vec::new(),
    };
    let mock = MockProvider::start(ProviderMode::GitHub, mock_user.clone()).await;

    let Some(fx) = NativeGithubFixture::boot(&mock).await else {
        eprintln!("[e2e_github native callback] skip (need AUTH_DB_URL)");
        return;
    };

    let base_return_to = native_authorize_return_to();
    let return_to_after_prompt = format!("{base_return_to}&idp_hint=github");
    let return_to = format!("{return_to_after_prompt}&prompt=login");
    let start_query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("return_to", &return_to)
        .finish();
    let start_url = format!("{}/oauth/github/start?{start_query}", fx.auth_base);
    let resp = fx
        .http
        .request(http::Method::GET, &start_url)
        .expect("build /oauth/github/start")
        .send()
        .await
        .expect("send /oauth/github/start");
    assert_eq!(resp.status().as_u16(), 302);
    let stash_cookie = read_set_cookie(&resp, "zsidp_github_stash")
        .expect("stash cookie on /oauth/github/start");
    let mock_authorize_loc = location(&resp);
    assert_eq!(
        common::extract_query_param(&mock_authorize_loc, "prompt").as_deref(),
        Some("login")
    );
    assert_eq!(
        common::extract_query_param(&mock_authorize_loc, "max_age"),
        None
    );

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
    assert_eq!(
        resp.status().as_u16(),
        303,
        "native callback should use see-other re-entry"
    );
    let final_loc = location(&resp);
    assert_eq!(final_loc, return_to_after_prompt);
    assert!(
        !final_loc.contains("login_verifier"),
        "native callback must not continue through accept_login"
    );
    let session_cookie =
        read_set_cookie(&resp, "zsidp_session").expect("session cookie on native callback");
    assert!(!session_cookie.is_empty());

    let user_rows = fx
        .pg
        .query(
            "SELECT id FROM zeroship.users WHERE email = $1::citext",
            &[&test_email.as_str()],
        )
        .await
        .expect("user select");
    assert_eq!(user_rows.len(), 1, "native callback creates one user row");
    let user_id: uuid::Uuid = user_rows[0].get("id");

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
async fn github_native_confirmation_bounce_carries_return_to() {
    let test_email = format!(
        "e2e-github-link-native-{}@zeroship.test",
        Uuid::new_v4().simple()
    );
    let mock_user = MockUser {
        subject: github_numeric_subject(),
        email: test_email.clone(),
        email_verified: true,
        name: Some("Native Link User".into()),
        picture: Some("https://avatars.githubusercontent.com/link-native.png".into()),
        login: Some("native-link-login".into()),
        additional_emails: Vec::new(),
    };
    let mock = MockProvider::start(ProviderMode::GitHub, mock_user.clone()).await;

    let Some(fx) = NativeGithubFixture::boot(&mock).await else {
        eprintln!("[e2e_github native link] skip (need AUTH_DB_URL)");
        return;
    };

    let phc = password::hash("existing password").expect("hash password");
    let existing = users::create(&fx.pg, &test_email, "Existing Native Link", Some(&phc))
        .await
        .expect("seed existing user");

    let return_to = native_authorize_return_to();
    let start_query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("return_to", &return_to)
        .finish();
    let start_url = format!("{}/oauth/github/start?{start_query}", fx.auth_base);
    let resp = fx
        .http
        .request(http::Method::GET, &start_url)
        .expect("build /oauth/github/start")
        .send()
        .await
        .expect("send /oauth/github/start");
    assert_eq!(resp.status().as_u16(), 302);
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
    assert_eq!(resp.status().as_u16(), 302);
    let link_loc = location(&resp);
    assert!(
        link_loc.starts_with("/link?"),
        "confirmation bounce should target /link: {link_loc}"
    );
    let token = relative_query_param(&link_loc, "token").expect("pending token");
    let decoded = PendingLink::decode(&token, TEST_STASH_KEY).expect("decode pending token");
    assert_eq!(decoded.user_id, existing.id);
    assert_eq!(decoded.return_to.as_deref(), Some(return_to.as_str()));

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
        "confirmation bounce must not create an identity row"
    );

    fx.pg
        .execute(
            "DELETE FROM zeroship.idp_sessions WHERE user_id = $1",
            &[&existing.id],
        )
        .await
        .ok();
    fx.pg
        .execute("DELETE FROM zeroship.users WHERE id = $1", &[&existing.id])
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

    let Some(fx) = NativeGithubFixture::boot(&mock).await else {
        eprintln!("[e2e_github noreply] skip (need AUTH_DB_URL)");
        return;
    };
    eprintln!("[e2e_github noreply] auth server at {}", fx.auth_base);

    // Drive start → mock /authorize → /callback.
    let return_to = native_authorize_return_to();
    let start_query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("return_to", &return_to)
        .finish();
    let start_url = format!("{}/oauth/github/start?{start_query}", fx.auth_base);
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
    // friendly body) and clears the stash.
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

    let Some(fx) = NativeGithubFixture::boot(&mock).await else {
        eprintln!("[e2e_github unverified] skip (need AUTH_DB_URL)");
        return;
    };
    eprintln!("[e2e_github unverified] auth server at {}", fx.auth_base);

    let return_to = native_authorize_return_to();
    let start_query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("return_to", &return_to)
        .finish();
    let start_url = format!("{}/oauth/github/start?{start_query}", fx.auth_base);
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
