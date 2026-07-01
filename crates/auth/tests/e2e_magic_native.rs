//! Magic-link native-completion coverage for de-Hydra P3.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use clap::Parser;
use compio_postgres::{connect, NoTls};
use ntex::http::header::{LOCATION, SET_COOKIE};
use ntex::web;
use serde::Deserialize;
use uuid::Uuid;

use zeroship_auth::config::AuthConfig;
use zeroship_auth::csrf;
use zeroship_auth::hydra_client::HydraAdmin;
use zeroship_auth::server;
use zeroship_mailer::{Email, Mailer, MailerError, MessageId};

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

    fn count(&self) -> usize { self.sent.lock().expect("lock sent mail").len() }
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

#[derive(Debug, Default)]
struct MockHydraState {
    accepts: AtomicUsize,
    challenges: Mutex<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct AcceptLoginQuery {
    login_challenge: String,
}

async fn mock_accept_login(
    query: web::types::Query<AcceptLoginQuery>,
    state: web::types::State<Arc<MockHydraState>>,
) -> web::HttpResponse {
    state.accepts.fetch_add(1, Ordering::SeqCst);
    state
        .challenges
        .lock()
        .expect("lock hydra challenges")
        .push(query.login_challenge.clone());
    web::HttpResponse::Ok()
        .content_type("application/json")
        .body(r#"{"redirect_to":"https://hydra.example/after-login?login_verifier=ok"}"#)
}

struct MagicFixture {
    srv: ntex::web::test::TestServer,
    _hydra_srv: ntex::web::test::TestServer,
    auth_base: String,
    pg: Arc<compio_postgres::Client>,
    http: cyper::Client,
    mailer: Arc<CaptureMailer>,
    hydra_state: Arc<MockHydraState>,
}

impl MagicFixture {
    #[allow(clippy::future_not_send)]
    async fn boot() -> Option<Self> {
        let db_url = match std::env::var("AUTH_DB_URL") {
            Ok(db_url) => db_url,
            Err(_) => return None,
        };

        let (pg_client, pg_connection) = connect(&db_url, NoTls).await.expect("connect pg");
        compio::runtime::spawn(async move {
            if let Err(e) = pg_connection.run().await {
                eprintln!("[e2e_magic_native] pg connection driver: {e}");
            }
        })
        .detach();
        let pg = Arc::new(pg_client);

        let hydra_state = Arc::new(MockHydraState::default());
        let hydra_state_for_srv = hydra_state.clone();
        let hydra_srv = web::test::server(move || {
            let hydra_state = hydra_state_for_srv.clone();
            async move {
                web::App::new().state(hydra_state).service(
                    web::resource("/admin/oauth2/auth/requests/login/accept")
                        .route(web::put().to(mock_accept_login)),
                )
            }
        })
        .await;
        let hydra_admin_url = hydra_srv.url("").trim_end_matches('/').to_string();

        let mut cfg = AuthConfig::parse_from([
            "zeroship-auth",
            "--addr",
            "127.0.0.1:0",
            "--db-url",
            &db_url,
            "--hydra-admin-url",
            &hydra_admin_url,
            "--hydra-public-url",
            "http://127.0.0.1:4444",
            "--dev-insecure",
            "--stash-signing-key",
            "test-stash-key-not-for-prod-32bytes!",
            "--mail-from-email",
            "test@zeroship.test",
            "--mail-from-name",
            "Test",
            "--public-url",
            "http://auth.test",
        ]);
        cfg.resolve(zeroship_core::config::AuthSection::default());
        let cfg = Arc::new(cfg);

        let mailer = Arc::new(CaptureMailer::default());
        let mailer_state: Arc<dyn Mailer> = mailer.clone();
        let admin = HydraAdmin::new(&hydra_admin_url);
        let cfg_state = cfg.clone();
        let pg_state = pg.clone();
        let admin_state = admin.clone();
        let srv = web::test::server(move || {
            let cfg_state = cfg_state.clone();
            let pg_state = pg_state.clone();
            let mailer_state = mailer_state.clone();
            let admin_state = admin_state.clone();
            async move {
                web::App::new()
                    .state(cfg_state)
                    .state(pg_state)
                    .state(mailer_state)
                    .state(admin_state)
                    .configure(server::configure(false, false))
            }
        })
        .await;
        let auth_base = srv.url("").trim_end_matches('/').to_string();

        Some(Self {
            srv,
            _hydra_srv: hydra_srv,
            auth_base,
            pg,
            http: cyper::Client::new(),
            mailer,
            hydra_state,
        })
    }

    fn magic_url(&self, path_and_query: &str) -> String {
        let parsed = url::Url::parse(path_and_query).expect("magic link URL");
        format!("{}{}?{}", self.auth_base, parsed.path(), parsed.query().unwrap_or(""))
    }

    fn hydra_accept_count(&self) -> usize {
        self.hydra_state.accepts.load(Ordering::SeqCst)
    }

    fn hydra_challenges(&self) -> Vec<String> {
        self.hydra_state
            .challenges
            .lock()
            .expect("lock hydra challenges")
            .clone()
    }
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

fn read_set_cookie(resp: &cyper::Response, name: &str) -> Option<String> {
    for hv in resp.headers().get_all(SET_COOKIE) {
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

fn location(resp: &cyper::Response) -> String {
    resp.headers()
        .get(LOCATION)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("")
        .to_string()
}

fn link_param(link: &str, key: &str) -> String {
    let parsed = url::Url::parse(link).expect("parse magic link");
    parsed
        .query_pairs()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.into_owned())
        .unwrap_or_else(|| panic!("missing {key} in {link}"))
}

fn csrf_cookie() -> String {
    let csrf = csrf::generate_token();
    format!("zsidp_csrf={csrf}")
}

fn cookie_value(cookie_header: &str) -> &str {
    cookie_header
        .split_once('=')
        .map(|(_, value)| value)
        .expect("cookie header has value")
}

async fn start_magic(
    fx: &MagicFixture,
    email: &str,
    target_key: &str,
    target_value: &str,
) -> (cyper::Response, String) {
    let csrf_cookie = csrf_cookie();
    let csrf = cookie_value(&csrf_cookie).to_string();
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("csrf", &csrf)
        .append_pair("email", email)
        .append_pair(target_key, target_value)
        .finish();
    let resp = fx
        .http
        .request(http::Method::POST, &format!("{}/magic/start", fx.auth_base))
        .expect("build /magic/start")
        .header("content-type", "application/x-www-form-urlencoded")
        .expect("content-type")
        .header("cookie", csrf_cookie)
        .expect("cookie")
        .body(body)
        .send()
        .await
        .expect("send /magic/start");
    (resp, csrf)
}

async fn cleanup_email(pg: &compio_postgres::Client, email: &str) {
    let _ = pg
        .execute(
            "DELETE FROM zeroship.idp_sessions WHERE user_id IN \
             (SELECT id FROM zeroship.users WHERE email = $1::citext)",
            &[&email],
        )
        .await;
    let _ = pg
        .execute(
            "DELETE FROM zeroship.magic_completions WHERE email = $1::citext",
            &[&email],
        )
        .await;
    let _ = pg
        .execute(
            "DELETE FROM zeroship.magic_links WHERE email = $1::citext",
            &[&email],
        )
        .await;
    let _ = pg
        .execute(
            "DELETE FROM zeroship.users WHERE email = $1::citext",
            &[&email],
        )
        .await;
}

async fn assert_magic_start_rejects_invalid_return_to(fx: &MagicFixture, bad_return_to: &str) {
    let email = format!(
        "magic-invalid-return-to-{}@zeroship.test",
        Uuid::new_v4().simple()
    );
    let (resp, _) = start_magic(fx, &email, "return_to", bad_return_to).await;
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.expect("read invalid request body");
    assert!(
        body.contains("invalid_request"),
        "bad return_to should render InvalidRequest, body={body}"
    );

    let magic_links: i64 = fx
        .pg
        .query_one(
            "SELECT COUNT(*) FROM zeroship.magic_links WHERE email = $1::citext",
            &[&email.as_str()],
        )
        .await
        .expect("count magic links")
        .get(0);
    let completions: i64 = fx
        .pg
        .query_one(
            "SELECT COUNT(*) FROM zeroship.magic_completions WHERE email = $1::citext",
            &[&email.as_str()],
        )
        .await
        .expect("count magic completions")
        .get(0);
    assert_eq!(magic_links, 0, "invalid return_to must not issue a token");
    assert_eq!(
        completions, 0,
        "invalid return_to must not persist completion"
    );
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn magic_same_device_native_resumes_authorize_without_accept_login() {
    let Some(fx) = MagicFixture::boot().await else {
        eprintln!("[e2e_magic_native same-device] skip (need AUTH_DB_URL)");
        return;
    };
    let email = format!("magic-native-same-{}@zeroship.test", Uuid::new_v4().simple());
    let return_to = native_authorize_return_to();

    let (start_resp, _) = start_magic(&fx, &email, "return_to", &return_to).await;
    assert_eq!(start_resp.status().as_u16(), 200);
    let magic_cookie = read_set_cookie(&start_resp, "zsidp_magic_csrf")
        .expect("magic csrf cookie on start");
    let link = fx.mailer.last_magic_link();
    assert_eq!(link_param(&link, "return_to"), return_to);

    let verify_resp = fx
        .http
        .request(http::Method::GET, &fx.magic_url(&link))
        .expect("build /magic/verify")
        .header("cookie", format!("zsidp_magic_csrf={magic_cookie}"))
        .expect("cookie")
        .send()
        .await
        .expect("send /magic/verify");
    assert_eq!(verify_resp.status().as_u16(), 200);

    let token = link_param(&link, "token");
    let redeem_body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("csrf", &magic_cookie)
        .append_pair("token", &token)
        .append_pair("return_to", &return_to)
        .finish();
    let redeem_resp = fx
        .http
        .request(
            http::Method::POST,
            &format!("{}/magic/verify/redeem", fx.auth_base),
        )
        .expect("build /magic/verify/redeem")
        .header("content-type", "application/x-www-form-urlencoded")
        .expect("content-type")
        .header("cookie", format!("zsidp_magic_csrf={magic_cookie}"))
        .expect("cookie")
        .body(redeem_body)
        .send()
        .await
        .expect("send /magic/verify/redeem");

    assert_eq!(redeem_resp.status().as_u16(), 303);
    assert_eq!(location(&redeem_resp), return_to);
    assert!(read_set_cookie(&redeem_resp, "zsidp_session").is_some());
    assert_eq!(fx.hydra_accept_count(), 0, "native arm must not call Hydra");

    cleanup_email(&fx.pg, &email).await;
    drop(fx.srv);
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn magic_cross_device_native_resumes_authorize_without_accept_login() {
    let Some(fx) = MagicFixture::boot().await else {
        eprintln!("[e2e_magic_native cross-device] skip (need AUTH_DB_URL)");
        return;
    };
    let email = format!("magic-native-cross-{}@zeroship.test", Uuid::new_v4().simple());
    let return_to = native_authorize_return_to();

    let (start_resp, _) = start_magic(&fx, &email, "return_to", &return_to).await;
    assert_eq!(start_resp.status().as_u16(), 200);
    let requester_magic_cookie = read_set_cookie(&start_resp, "zsidp_magic_csrf")
        .expect("requester magic csrf cookie");
    let requester_csrf = read_set_cookie(&start_resp, "zsidp_csrf").expect("requester csrf");
    let link = fx.mailer.last_magic_link();

    let verify_resp = fx
        .http
        .request(http::Method::GET, &fx.magic_url(&link))
        .expect("build cross-device /magic/verify")
        .send()
        .await
        .expect("send cross-device /magic/verify");
    assert_eq!(verify_resp.status().as_u16(), 200);
    let redeem_magic_cookie = read_set_cookie(&verify_resp, "zsidp_magic_csrf")
        .expect("redeeming device magic csrf cookie");
    assert_ne!(redeem_magic_cookie, requester_magic_cookie);

    let token = link_param(&link, "token");
    let redeem_body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("csrf", &redeem_magic_cookie)
        .append_pair("token", &token)
        .append_pair("return_to", &return_to)
        .finish();
    let redeem_resp = fx
        .http
        .request(
            http::Method::POST,
            &format!("{}/magic/verify/redeem", fx.auth_base),
        )
        .expect("build cross-device redeem")
        .header("content-type", "application/x-www-form-urlencoded")
        .expect("content-type")
        .header("cookie", format!("zsidp_magic_csrf={redeem_magic_cookie}"))
        .expect("cookie")
        .body(redeem_body)
        .send()
        .await
        .expect("send cross-device redeem");
    assert_eq!(redeem_resp.status().as_u16(), 200);
    assert_eq!(fx.hydra_accept_count(), 0, "redeem device must not call Hydra");

    let code: String = fx
        .pg
        .query_one(
            "SELECT code FROM zeroship.magic_completions \
             WHERE csrf_nonce = $1 AND email = $2::citext",
            &[&requester_magic_cookie, &email.as_str()],
        )
        .await
        .expect("load cross-device code")
        .get("code");
    let complete_body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("csrf", &requester_csrf)
        .append_pair("csrf_nonce", &requester_magic_cookie)
        .append_pair("return_to", &return_to)
        .append_pair("code", &code)
        .finish();
    let complete_resp = fx
        .http
        .request(http::Method::POST, &format!("{}/magic/complete", fx.auth_base))
        .expect("build /magic/complete")
        .header("content-type", "application/x-www-form-urlencoded")
        .expect("content-type")
        .header("cookie", format!("zsidp_csrf={requester_csrf}"))
        .expect("cookie")
        .body(complete_body)
        .send()
        .await
        .expect("send /magic/complete");

    assert_eq!(complete_resp.status().as_u16(), 303);
    assert_eq!(location(&complete_resp), return_to);
    assert!(read_set_cookie(&complete_resp, "zsidp_session").is_some());
    assert_eq!(fx.hydra_accept_count(), 0, "native complete must not call Hydra");

    cleanup_email(&fx.pg, &email).await;
    drop(fx.srv);
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn magic_hydra_arm_still_accepts_login_and_redirects() {
    let Some(fx) = MagicFixture::boot().await else {
        eprintln!("[e2e_magic_native hydra regression] skip (need AUTH_DB_URL)");
        return;
    };
    let email = format!("magic-hydra-{}@zeroship.test", Uuid::new_v4().simple());
    let login_challenge = format!("lc-{}", Uuid::new_v4().simple());

    let (start_resp, _) = start_magic(&fx, &email, "login_challenge", &login_challenge).await;
    assert_eq!(start_resp.status().as_u16(), 200);
    let magic_cookie = read_set_cookie(&start_resp, "zsidp_magic_csrf")
        .expect("magic csrf cookie on hydra start");
    let link = fx.mailer.last_magic_link();
    assert_eq!(link_param(&link, "login_challenge"), login_challenge);

    let token = link_param(&link, "token");
    let redeem_body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("csrf", &magic_cookie)
        .append_pair("token", &token)
        .append_pair("login_challenge", &login_challenge)
        .finish();
    let redeem_resp = fx
        .http
        .request(
            http::Method::POST,
            &format!("{}/magic/verify/redeem", fx.auth_base),
        )
        .expect("build hydra redeem")
        .header("content-type", "application/x-www-form-urlencoded")
        .expect("content-type")
        .header("cookie", format!("zsidp_magic_csrf={magic_cookie}"))
        .expect("cookie")
        .body(redeem_body)
        .send()
        .await
        .expect("send hydra redeem");

    assert_eq!(redeem_resp.status().as_u16(), 302);
    assert_eq!(
        location(&redeem_resp),
        "https://hydra.example/after-login?login_verifier=ok"
    );
    assert!(read_set_cookie(&redeem_resp, "zsidp_session").is_some());
    assert_eq!(fx.hydra_accept_count(), 1);
    assert_eq!(fx.hydra_challenges(), vec![login_challenge.clone()]);

    cleanup_email(&fx.pg, &email).await;
    drop(fx.srv);
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn magic_start_rejects_open_redirect_return_to_without_persisting() {
    let Some(fx) = MagicFixture::boot().await else {
        eprintln!("[e2e_magic_native open-redirect] skip (need AUTH_DB_URL)");
        return;
    };

    for bad_return_to in ["//evil.com", "https://evil.com"] {
        let email = format!(
            "magic-open-redirect-{}@zeroship.test",
            Uuid::new_v4().simple()
        );
        let (resp, _) = start_magic(&fx, &email, "return_to", bad_return_to).await;
        assert_eq!(resp.status().as_u16(), 200);
        let body = resp.text().await.expect("read invalid request body");
        assert!(
            body.contains("invalid_request"),
            "bad return_to should render InvalidRequest, body={body}"
        );

        let magic_links: i64 = fx
            .pg
            .query_one(
                "SELECT COUNT(*) FROM zeroship.magic_links WHERE email = $1::citext",
                &[&email.as_str()],
            )
            .await
            .expect("count magic links")
            .get(0);
        let completions: i64 = fx
            .pg
            .query_one(
                "SELECT COUNT(*) FROM zeroship.magic_completions WHERE email = $1::citext",
                &[&email.as_str()],
            )
            .await
            .expect("count magic completions")
            .get(0);
        assert_eq!(magic_links, 0, "invalid return_to must not issue a token");
        assert_eq!(completions, 0, "invalid return_to must not persist completion");
    }

    assert_eq!(fx.mailer.count(), 0, "invalid return_to must not send mail");
    assert_eq!(fx.hydra_accept_count(), 0);
    drop(fx.srv);
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn magic_start_rejects_wrong_path_and_crlf_return_to_without_persisting() {
    let Some(fx) = MagicFixture::boot().await else {
        eprintln!("[e2e_magic_native invalid-return-to] skip (need AUTH_DB_URL)");
        return;
    };

    for bad_return_to in [
        "/me",
        "/oauth2/authorize?client_id=oac_123\r\nLocation: https://evil.com",
    ] {
        assert_magic_start_rejects_invalid_return_to(&fx, bad_return_to).await;
    }

    assert_eq!(fx.mailer.count(), 0, "invalid return_to must not send mail");
    assert_eq!(fx.hydra_accept_count(), 0);
    drop(fx.srv);
}
