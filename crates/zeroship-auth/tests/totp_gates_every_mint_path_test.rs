//! A confirmed TOTP credential must gate EVERY IdP-session mint, not just the
//! one behind `/login`.
//!
//! Three handlers reach `sessions::create` without a password: the magic-link
//! same-device redeem, the magic-link cross-device completion, and the `/link`
//! account-link confirm. `sessions::create` writes whatever `amr`/`acr` the
//! caller hands it, so nothing below the handler can re-impose a second factor
//! the handler skipped. A magic link is exactly the credential a compromised
//! mailbox yields, which makes the second factor the control that is supposed
//! to survive that compromise.
//!
//! Each test seeds a user with a CONFIRMED TOTP credential, drives the mint to
//! the point where a session would appear, and asserts that no session cookie
//! is issued and the challenge page is served instead. Each then completes the
//! challenge and asserts the resulting session's `amr`/`acr` name the factors
//! that were actually used - a magic-link user must not end up holding a
//! session that claims a password login.
//!
//! Skipped unless a test database is available; the skip is announced so
//! `tests/run_auth_suite.sh` can tell a skip from a pass.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use compio_postgres::{connect, NoTls};
use ntex::http::header::{LOCATION, SET_COOKIE};
use ntex::web;
use uuid::Uuid;

use zeroship_auth::config::AuthConfig;
use zeroship_core::config::{Secret, SourceKind};
use zeroship_auth::csrf;
use zeroship_auth::identity::linker::PendingLink;
use zeroship_auth::identity::{password, totp};
use zeroship_auth::server;
use zeroship_auth::store::{totp as totp_store, users};
use zeroship_mailer::{Email, Mailer, MailerError, MessageId};

/// Hex-encoded 32-byte at-rest key for the TOTP secret. Passed to the booted
/// config so the test can encrypt a seeded secret the handler can decrypt.
const TOTP_ENC_KEY_HEX: &str =
    "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
const STASH_KEY: &str = "test-stash-key-not-for-prod-32bytes!";
const LINK_PASSWORD: &str = "correct-horse-battery-staple";

// The challenge page's form target. Serving it is the observable difference
// between "second factor demanded" and "session minted".
const CHALLENGE_MARKER: &str = "action=\"/login/2fa\"";

#[derive(Debug, Default)]
struct CaptureMailer {
    sent: Mutex<Vec<Email>>,
}

impl CaptureMailer {
    fn last_magic_link(&self) -> String {
        let sent = self.sent.lock().expect("lock sent mail");
        let email = sent.last().expect("captured magic email");
        email
            .text
            .split_whitespace()
            .find(|part| part.contains("/magic/verify?"))
            .expect("magic verify link in text email")
            .trim_matches(|ch| matches!(ch, '<' | '>' | '"' | '\''))
            .to_string()
    }
}

#[async_trait]
impl Mailer for CaptureMailer {
    async fn send(
        &self,
        _db: &compio_postgres::Client,
        msg: Email,
    ) -> Result<MessageId, MailerError> {
        let mut sent = self.sent.lock().expect("lock sent mail");
        sent.push(msg);
        Ok(MessageId(format!("captured-{}", sent.len())))
    }
}

struct Fixture {
    srv: ntex::web::test::TestServer,
    auth_base: String,
    pg: Arc<compio_postgres::Client>,
    http: cyper::Client,
    mailer: Arc<CaptureMailer>,
}

impl Fixture {
    #[allow(clippy::future_not_send)]
    async fn boot() -> Option<Self> {
        let db_url = zeroship_core::config::test_database_url_opt()?;

        let (pg_client, pg_connection) = connect(&db_url, NoTls).await.expect("connect pg");
        compio::runtime::spawn(async move {
            if let Err(e) = pg_connection.run().await {
                eprintln!("[totp_gates_every_mint_path] pg connection driver: {e}");
            }
        })
        .detach();
        let pg = Arc::new(pg_client);

        let mut cfg = AuthConfig::parse_from([
            "zeroship-auth",
            "--addr",
            "127.0.0.1:0",
            "--mail-from-email",
            "test@zeroship.test",
            "--mail-from-name",
            "Test",
            "--public-url",
            "http://auth.test",
        ]);
        // Secrets carry no value flag; supply each in the shape an in-memory
        // literal resolves to (see crates/zeroship-auth/tests/common/mod.rs).
        cfg.settings.database_url = Secret::supplied(SourceKind::Env, Some(db_url.clone()));
        cfg.settings.stash_signing_key =
            Secret::supplied(SourceKind::Env, Some(STASH_KEY.to_owned()));
        cfg.settings.totp_enc_key =
            Secret::supplied(SourceKind::Env, Some(TOTP_ENC_KEY_HEX.to_owned()));
        let cfg = Arc::new(cfg);

        let mailer = Arc::new(CaptureMailer::default());
        let mailer_state: Arc<dyn Mailer> = mailer.clone();
        let cfg_state = cfg.clone();
        let pg_state = pg.clone();
        let srv = web::test::server(move || {
            let cfg_state = cfg_state.clone();
            let pg_state = pg_state.clone();
            let mailer_state = mailer_state.clone();
            async move {
                web::App::new()
                    .state(cfg_state)
                    .state(pg_state)
                    .state(mailer_state)
                    .configure(server::configure(false, false))
            }
        })
        .await;
        let auth_base = srv.url("").trim_end_matches('/').to_string();

        Some(Self {
            srv,
            auth_base,
            pg,
            http: cyper::Client::new(),
            mailer,
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.auth_base)
    }

    /// Rewrite an absolute magic link (issued against `--public-url`) onto the
    /// ephemeral test-server origin.
    fn magic_url(&self, link: &str) -> String {
        let parsed = url::Url::parse(link).expect("magic link URL");
        format!(
            "{}{}?{}",
            self.auth_base,
            parsed.path(),
            parsed.query().unwrap_or("")
        )
    }

    #[allow(clippy::future_not_send)]
    async fn post_form(&self, path: &str, cookie: &str, body: String) -> cyper::Response {
        self.http
            .request(http::Method::POST, self.url(path))
            .expect("build request")
            .header("content-type", "application/x-www-form-urlencoded")
            .expect("content-type")
            .header("cookie", cookie)
            .expect("cookie")
            .body(body)
            .send()
            .await
            .expect("send request")
    }
}

fn totp_key() -> [u8; 32] {
    totp::key_from_config(TOTP_ENC_KEY_HEX).expect("totp key")
}

/// Seed a user carrying a CONFIRMED TOTP credential. Returns the user row and
/// the raw shared secret so the test can compute a live code.
#[allow(clippy::future_not_send)]
async fn seed_user_with_totp(
    pg: &compio_postgres::Client,
    email: &str,
    password_hash: Option<&str>,
) -> (users::UserRow, Vec<u8>) {
    let user = users::create(pg, email, "Totp Gate", password_hash)
        .await
        .expect("create user");
    let secret = totp::generate_secret();
    let encrypted = totp::encrypt_secret(&totp_key(), user.id, &secret).expect("encrypt secret");
    totp_store::enroll(pg, user.id, &encrypted, false)
        .await
        .expect("enroll totp");
    let (_, hashes) = totp::generate_backup_codes().expect("backup codes");
    totp_store::confirm(pg, user.id, &hashes)
        .await
        .expect("confirm totp");
    assert!(
        totp_store::is_enabled(pg, user.id).await.expect("is_enabled"),
        "seeded credential must be confirmed, else the test proves nothing"
    );
    (user, secret)
}

fn live_code(secret: &[u8]) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs();
    totp::code_at(secret, now)
}

fn read_set_cookie(resp: &cyper::Response, name: &str) -> Option<String> {
    for hv in resp.headers().get_all(SET_COOKIE) {
        let Ok(s) = hv.to_str() else { continue };
        let first = s.split(';').next().unwrap_or("");
        if let Some((n, v)) = first.split_once('=') {
            if n.trim() == name && !v.trim().is_empty() {
                return Some(v.trim().to_string());
            }
        }
    }
    None
}

fn location(resp: &cyper::Response) -> String {
    resp.headers()
        .get(LOCATION)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("")
        .to_string()
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

fn link_param(link: &str, key: &str) -> String {
    url::Url::parse(link)
        .expect("parse magic link")
        .query_pairs()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.into_owned())
        .unwrap_or_else(|| panic!("missing {key} in {link}"))
}

/// Assert the response demands a second factor rather than handing out a
/// session: no session cookie, a challenge cookie, and the challenge form.
#[allow(clippy::future_not_send)]
async fn assert_demands_second_factor(resp: cyper::Response, what: &str) -> String {
    let status = resp.status().as_u16();
    let session = read_set_cookie(&resp, "__Host-zsidp_session");
    let challenge = read_set_cookie(&resp, "__Host-zsidp_2fa");
    let csrf_cookie = read_set_cookie(&resp, "__Host-zsidp_csrf");
    let body = resp.text().await.expect("read body");
    assert!(
        session.is_none(),
        "{what}: a confirmed second factor must block the session mint, \
         but a __Host-zsidp_session cookie was issued (status {status})"
    );
    assert_eq!(status, 200, "{what}: expected the challenge page, body={body}");
    assert!(
        body.contains(CHALLENGE_MARKER),
        "{what}: expected the TOTP challenge form, body={body}"
    );
    let challenge = challenge.unwrap_or_else(|| panic!("{what}: no __Host-zsidp_2fa challenge cookie"));
    let csrf_cookie = csrf_cookie.unwrap_or_else(|| panic!("{what}: no __Host-zsidp_csrf cookie"));
    format!("__Host-zsidp_2fa={challenge}; __Host-zsidp_csrf={csrf_cookie}")
}

/// Read the single session row for `user_id` as `(auth_method, amr, acr)`.
#[allow(clippy::future_not_send)]
async fn session_claims(
    pg: &compio_postgres::Client,
    user_id: Uuid,
) -> (String, Vec<String>, Option<String>) {
    let row = pg
        .query_one(
            "SELECT auth_method, amr, acr FROM zeroship.idp_sessions WHERE user_id = $1",
            &[&user_id],
        )
        .await
        .expect("load minted session");
    (row.get("auth_method"), row.get("amr"), row.try_get("acr").ok())
}

#[allow(clippy::future_not_send)]
async fn cleanup(pg: &compio_postgres::Client, email: &str) {
    for sql in [
        "DELETE FROM zeroship.idp_sessions WHERE user_id IN (SELECT id FROM zeroship.users WHERE email = $1::citext)",
        "DELETE FROM zeroship.federated_identities WHERE user_id IN (SELECT id FROM zeroship.users WHERE email = $1::citext)",
        "DELETE FROM zeroship.totp_backup_codes WHERE user_id IN (SELECT id FROM zeroship.users WHERE email = $1::citext)",
        "DELETE FROM zeroship.totp_credentials WHERE user_id IN (SELECT id FROM zeroship.users WHERE email = $1::citext)",
        "DELETE FROM zeroship.magic_completions WHERE email = $1::citext",
        "DELETE FROM zeroship.magic_links WHERE email = $1::citext",
        "DELETE FROM zeroship.users WHERE email = $1::citext",
    ] {
        let _ = pg.execute(sql, &[&email]).await;
    }
}

/// Issue a magic link for `email` and return the requesting device's
/// `__Host-zsidp_magic_csrf` nonce, its `__Host-zsidp_csrf` token, and the emailed link.
#[allow(clippy::future_not_send)]
async fn start_magic(fx: &Fixture, email: &str, return_to: &str) -> (String, String, String) {
    let csrf_token = csrf::generate_token();
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("csrf", &csrf_token)
        .append_pair("email", email)
        .append_pair("return_to", return_to)
        .finish();
    let resp = fx
        .post_form("/magic/start", &format!("__Host-zsidp_csrf={csrf_token}"), body)
        .await;
    assert_eq!(resp.status().as_u16(), 200, "magic start");
    let magic_nonce =
        read_set_cookie(&resp, "__Host-zsidp_magic_csrf").expect("magic csrf cookie on start");
    let requester_csrf = read_set_cookie(&resp, "__Host-zsidp_csrf").unwrap_or(csrf_token);
    (magic_nonce, requester_csrf, fx.mailer.last_magic_link())
}

// --- magic same-device redeem -------------------------------------------

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn magic_same_device_redeem_demands_second_factor() {
    let Some(fx) = Fixture::boot().await else {
        zeroship_test_support::skip(
            "[totp_gates_every_mint_path same-device] skip (need a test database (set PG_TEST_URL or run tests/provision_test_backends.sh))",
        );
        return;
    };
    let email = format!("totp-magic-same-{}@zeroship.test", Uuid::new_v4().simple());
    let (user, secret) = seed_user_with_totp(&fx.pg, &email, None).await;
    let return_to = native_authorize_return_to();

    let (magic_nonce, _, link) = start_magic(&fx, &email, &return_to).await;

    // The redeeming browser presents the requesting device's nonce -> same-device.
    let verify = fx
        .http
        .request(http::Method::GET, fx.magic_url(&link))
        .expect("build /magic/verify")
        .header("cookie", format!("__Host-zsidp_magic_csrf={magic_nonce}"))
        .expect("cookie")
        .send()
        .await
        .expect("send /magic/verify");
    assert_eq!(verify.status().as_u16(), 200);

    let token = link_param(&link, "token");
    let redeem_body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("csrf", &magic_nonce)
        .append_pair("token", &token)
        .append_pair("return_to", &return_to)
        .finish();
    let redeem = fx
        .post_form(
            "/magic/verify/redeem",
            &format!("__Host-zsidp_magic_csrf={magic_nonce}"),
            redeem_body,
        )
        .await;

    let challenge_cookies =
        assert_demands_second_factor(redeem, "magic same-device redeem").await;

    let csrf_token = challenge_cookies
        .split("__Host-zsidp_csrf=")
        .nth(1)
        .expect("csrf in challenge cookies")
        .to_string();

    // A mistyped code must not strand the user: the challenge cookie survives
    // the rejection, so the next attempt still has a factor-1 attestation to
    // complete against. Without this the magic link, already consumed at
    // challenge time, would be spent on a single keystroke.
    let wrong_body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("csrf", &csrf_token)
        .append_pair("code", "000000")
        .append_pair("return_to", &return_to)
        .finish();
    let wrong = fx
        .post_form("/login/2fa", &challenge_cookies, wrong_body)
        .await;
    assert_eq!(wrong.status().as_u16(), 401, "a wrong code is rejected");
    assert!(
        read_set_cookie(&wrong, "__Host-zsidp_session").is_none(),
        "a wrong code must not mint a session"
    );

    // The second factor completes the login, and the session says so.
    let complete_body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("csrf", &csrf_token)
        .append_pair("code", &live_code(&secret))
        .append_pair("return_to", &return_to)
        .finish();
    let done = fx
        .post_form("/login/2fa", &challenge_cookies, complete_body)
        .await;
    assert_eq!(done.status().as_u16(), 303, "second factor completes login");
    assert_eq!(location(&done), return_to);
    assert!(read_set_cookie(&done, "__Host-zsidp_session").is_some());

    let (auth_method, amr, acr) = session_claims(&fx.pg, user.id).await;
    assert_eq!(auth_method, "magic");
    assert_eq!(amr, vec!["magic".to_string(), "otp".to_string()]);
    assert_eq!(acr.as_deref(), Some("urn:zeroship:magic"));

    cleanup(&fx.pg, &email).await;
    drop(fx.srv);
}

// --- magic cross-device completion ---------------------------------------

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn magic_cross_device_complete_demands_second_factor() {
    let Some(fx) = Fixture::boot().await else {
        zeroship_test_support::skip(
            "[totp_gates_every_mint_path cross-device] skip (need a test database (set PG_TEST_URL or run tests/provision_test_backends.sh))",
        );
        return;
    };
    let email = format!("totp-magic-cross-{}@zeroship.test", Uuid::new_v4().simple());
    let (user, secret) = seed_user_with_totp(&fx.pg, &email, None).await;
    let return_to = native_authorize_return_to();

    let (requester_nonce, requester_csrf, link) = start_magic(&fx, &email, &return_to).await;

    // A DIFFERENT browser opens the link (no matching nonce) -> cross-device.
    let verify = fx
        .http
        .request(http::Method::GET, fx.magic_url(&link))
        .expect("build /magic/verify")
        .send()
        .await
        .expect("send /magic/verify");
    assert_eq!(verify.status().as_u16(), 200);
    let redeem_nonce =
        read_set_cookie(&verify, "__Host-zsidp_magic_csrf").expect("redeeming device nonce");

    let token = link_param(&link, "token");
    let redeem_body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("csrf", &redeem_nonce)
        .append_pair("token", &token)
        .append_pair("return_to", &return_to)
        .finish();
    let redeem = fx
        .post_form(
            "/magic/verify/redeem",
            &format!("__Host-zsidp_magic_csrf={redeem_nonce}"),
            redeem_body,
        )
        .await;
    assert_eq!(redeem.status().as_u16(), 200, "cross-device shows a code");
    assert!(
        read_set_cookie(&redeem, "__Host-zsidp_session").is_none(),
        "the redeeming device never gets a session on the cross-device path"
    );

    let code: String = fx
        .pg
        .query_one(
            "SELECT code FROM zeroship.magic_completions \
             WHERE csrf_nonce = $1 AND email = $2::citext",
            &[&requester_nonce, &email.as_str()],
        )
        .await
        .expect("load cross-device code")
        .get("code");

    let complete_body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("csrf", &requester_csrf)
        .append_pair("csrf_nonce", &requester_nonce)
        .append_pair("return_to", &return_to)
        .append_pair("code", &code)
        .finish();
    let complete = fx
        .post_form(
            "/magic/complete",
            &format!("__Host-zsidp_csrf={requester_csrf}"),
            complete_body,
        )
        .await;

    let challenge_cookies =
        assert_demands_second_factor(complete, "magic cross-device complete").await;

    let csrf_token = challenge_cookies
        .split("__Host-zsidp_csrf=")
        .nth(1)
        .expect("csrf in challenge cookies")
        .to_string();
    let second_factor_body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("csrf", &csrf_token)
        .append_pair("code", &live_code(&secret))
        .append_pair("return_to", &return_to)
        .finish();
    let done = fx
        .post_form("/login/2fa", &challenge_cookies, second_factor_body)
        .await;
    assert_eq!(done.status().as_u16(), 303, "second factor completes login");
    assert_eq!(location(&done), return_to);
    assert!(read_set_cookie(&done, "__Host-zsidp_session").is_some());

    let (auth_method, amr, acr) = session_claims(&fx.pg, user.id).await;
    assert_eq!(auth_method, "magic");
    assert_eq!(amr, vec!["magic".to_string(), "otp".to_string()]);
    assert_eq!(acr.as_deref(), Some("urn:zeroship:magic"));

    cleanup(&fx.pg, &email).await;
    drop(fx.srv);
}

// --- /link account-link confirm ------------------------------------------

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn link_confirm_demands_second_factor_before_linking() {
    let Some(fx) = Fixture::boot().await else {
        zeroship_test_support::skip("[totp_gates_every_mint_path link] skip (need a test database (set PG_TEST_URL or run tests/provision_test_backends.sh))");
        return;
    };
    let email = format!("totp-link-{}@zeroship.test", Uuid::new_v4().simple());
    let phc = password::hash(LINK_PASSWORD).expect("hash password");
    let (user, secret) = seed_user_with_totp(&fx.pg, &email, Some(&phc)).await;
    let return_to = native_authorize_return_to();

    let subject = format!("gh-{}", Uuid::new_v4().simple());
    let exp_unix = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_secs(),
    )
    .expect("unix seconds")
        + 600;
    let pending = PendingLink {
        user_id: user.id,
        provider: "github".into(),
        subject: subject.clone(),
        email: email.clone(),
        return_to: Some(return_to.clone()),
        exp_unix,
    };
    let token = pending.encode(STASH_KEY.as_bytes());

    let csrf_token = csrf::generate_token();
    let link_body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("csrf", &csrf_token)
        .append_pair("token", &token)
        .append_pair("password", LINK_PASSWORD)
        .finish();
    let resp = fx
        .post_form("/link", &format!("__Host-zsidp_csrf={csrf_token}"), link_body)
        .await;

    let challenge_cookies = assert_demands_second_factor(resp, "/link confirm").await;

    // The federated identity is the durable prize here: linking it hands the
    // holder of that upstream account a login path that bypasses the local
    // password. It must not exist until the second factor lands.
    let linked_before: i64 = fx
        .pg
        .query_one(
            "SELECT COUNT(*) FROM zeroship.federated_identities WHERE user_id = $1",
            &[&user.id],
        )
        .await
        .expect("count identities")
        .get(0);
    assert_eq!(
        linked_before, 0,
        "/link must not write the identity row before the second factor"
    );

    let csrf_token = challenge_cookies
        .split("__Host-zsidp_csrf=")
        .nth(1)
        .expect("csrf in challenge cookies")
        .to_string();
    let second_factor_body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("csrf", &csrf_token)
        .append_pair("code", &live_code(&secret))
        .append_pair("return_to", &return_to)
        .finish();
    let done = fx
        .post_form("/login/2fa", &challenge_cookies, second_factor_body)
        .await;
    assert_eq!(done.status().as_u16(), 303, "second factor completes the link");
    assert_eq!(location(&done), return_to);
    assert!(read_set_cookie(&done, "__Host-zsidp_session").is_some());

    let linked_after: i64 = fx
        .pg
        .query_one(
            "SELECT COUNT(*) FROM zeroship.federated_identities \
             WHERE user_id = $1 AND provider = 'github' AND subject = $2",
            &[&user.id, &subject.as_str()],
        )
        .await
        .expect("count identities")
        .get(0);
    assert_eq!(linked_after, 1, "the link lands once the second factor passes");

    let (auth_method, amr, acr) = session_claims(&fx.pg, user.id).await;
    assert_eq!(auth_method, "github");
    assert_eq!(
        amr,
        vec!["oauth".to_string(), "pwd".to_string(), "otp".to_string()]
    );
    assert_eq!(acr.as_deref(), Some("urn:zeroship:github"));

    cleanup(&fx.pg, &email).await;
    drop(fx.srv);
}
