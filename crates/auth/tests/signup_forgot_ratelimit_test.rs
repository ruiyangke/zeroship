//! Live-PG regression tests for `/signup` and `/forgot` rate limits.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use clap::Parser;
use compio_postgres::{connect, NoTls};
use ntex::http::header::{LOCATION, SET_COOKIE};
use ntex::web::{self, test};
use uuid::Uuid;

use zeroship_auth::config::AuthConfig;
use zeroship_auth::store::{users};
use zeroship_mailer::{Email, Mailer, MailerError, MessageId};

#[derive(Debug, Default)]
struct CountingMailer {
    sends: AtomicUsize,
}

impl CountingMailer {
    fn count(&self) -> usize {
        self.sends.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Mailer for CountingMailer {
    async fn send(
        &self,
        _db: &compio_postgres::Client,
        _msg: Email,
    ) -> Result<MessageId, MailerError> {
        let n = self.sends.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(MessageId(format!("test-message-{n}")))
    }
}

fn test_cfg(db_url: &str) -> AuthConfig {
    let mut cfg = AuthConfig::parse_from([
        "zeroship-auth",
        "--db-url",
        db_url,
        "--dev-insecure",
        "--stash-signing-key",
        "test-stash-key-not-for-prod-32bytes!",
    ]);
    cfg.resolve(zeroship_core::config::AuthSection::default());
    cfg
}

fn read_set_cookie(headers: &ntex::http::HeaderMap, name: &str) -> Option<String> {
    for hv in headers.get_all(SET_COOKIE) {
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

fn unique_loopback() -> IpAddr {
    let bytes = *Uuid::new_v4().as_bytes();
    IpAddr::V4(Ipv4Addr::new(
        127,
        bytes[0].max(1),
        bytes[1].max(1),
        bytes[2].max(1),
    ))
}

#[allow(clippy::future_not_send)]
async fn pg() -> Option<(String, compio_postgres::Client)> {
    let dsn = std::env::var("AUTH_DB_URL").ok()?;
    let (client, connection) = connect(&dsn, NoTls).await.expect("connect");
    compio::runtime::spawn(async move {
        if let Err(e) = connection.run().await {
            eprintln!("signup_forgot_ratelimit_test pg connection error: {e}");
        }
    })
    .detach();
    Some((dsn, client))
}

#[compio::test]
#[allow(clippy::future_not_send)]
async fn signup_post_throttles_after_ip_bucket_capacity() {
    let Some((dsn, client)) = pg().await else {
        eprintln!("skipping signup_forgot_ratelimit_test (no AUTH_DB_URL)");
        return;
    };

    let prefix = format!("signup-rl-{}", Uuid::new_v4().simple());
    let pg = Arc::new(client);
    let cfg = Arc::new(test_cfg(&dsn));
    let mailer = Arc::new(CountingMailer::default());
    let mailer_state: Arc<dyn Mailer> = mailer.clone();
    let app = test::init_service(
        web::App::new()
            .state(cfg.clone())
            .state(pg.clone())
            .state(mailer_state)
            .service(
                web::resource("/signup")
                    .route(web::get().to(zeroship_auth::ui::signup::get))
                    .route(web::post().to(zeroship_auth::ui::signup::post)),
            ),
    )
    .await;

    let get_resp = test::call_service(
        &app,
        test::TestRequest::get().uri("/signup").to_request(),
    )
    .await;
    assert_eq!(get_resp.status().as_u16(), 200);
    let csrf = read_set_cookie(get_resp.headers(), "zsidp_csrf")
        .expect("zsidp_csrf cookie set on GET /signup");

    let peer = SocketAddr::new(unique_loopback(), 49152);
    for i in 0..11 {
        let email = format!("{prefix}-{i}@zeroship.test");
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("csrf", &csrf)
            .append_pair("name", "Rate Limit")
            .append_pair("email", &email)
            .append_pair("password", "correct horse battery staple")
            .finish();
        let resp = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/signup")
                .peer_addr(peer)
                // ntex's `TestRequest::peer_addr` does not propagate to
                // `req.peer_addr()` (its own test asserts it stays None),
                // so the handler can't see a per-test socket peer. The
                // handler keys its rate-limit on the *forwarded* client IP
                // (auth runs behind the gateway), so we inject uniqueness
                // via X-Forwarded-For — otherwise every test would share
                // the single `signup_ip:0.0.0.0` bucket and drain it.
                .header("x-forwarded-for", peer.ip().to_string())
                .header("content-type", "application/x-www-form-urlencoded")
                .header("cookie", format!("zsidp_csrf={csrf}"))
                .set_payload(body)
                .to_request(),
        )
        .await;
        assert_eq!(
            resp.status().as_u16(),
            302,
            "signup response {i} must preserve the normal redirect shape"
        );
    }

    let like = format!("{prefix}-%");
    let created: i64 = pg
        .query_one(
            "SELECT COUNT(*) FROM zeroship.users WHERE email::text LIKE $1",
            &[&like],
        )
        .await
        .expect("count created signup users")
        .get(0);
    assert_eq!(created, 10, "11th signup must be throttled before insert");

    pg.execute(
        "DELETE FROM zeroship.magic_links WHERE email::text LIKE $1",
        &[&like],
    )
    .await
    .ok();
    pg.execute("DELETE FROM zeroship.users WHERE email::text LIKE $1", &[&like])
        .await
        .ok();
}

#[compio::test]
#[allow(clippy::future_not_send)]
async fn signup_non_duplicate_create_error_renders_error_page() {
    let Some((dsn, client)) = pg().await else {
        eprintln!("skipping signup_forgot_ratelimit_test (no AUTH_DB_URL)");
        return;
    };

    client
        .execute(
            "ALTER TABLE zeroship.users \
             DROP CONSTRAINT IF EXISTS auth_users_signup_m3_name_check",
            &[],
        )
        .await
        .expect("drop stale test constraint");
    client
        .execute(
            "ALTER TABLE zeroship.users \
             ADD CONSTRAINT auth_users_signup_m3_name_check CHECK (name <> 'M3_FAIL')",
            &[],
        )
        .await
        .expect("add test constraint");

    let email = format!("signup-m3-{}@zeroship.test", Uuid::new_v4().simple());
    let pg = Arc::new(client);
    let cfg = Arc::new(test_cfg(&dsn));
    let mailer = Arc::new(CountingMailer::default());
    let mailer_state: Arc<dyn Mailer> = mailer.clone();
    let app = test::init_service(
        web::App::new()
            .state(cfg.clone())
            .state(pg.clone())
            .state(mailer_state)
            .service(
                web::resource("/signup")
                    .route(web::get().to(zeroship_auth::ui::signup::get))
                    .route(web::post().to(zeroship_auth::ui::signup::post)),
            ),
    )
    .await;

    let get_resp = test::call_service(
        &app,
        test::TestRequest::get().uri("/signup").to_request(),
    )
    .await;
    assert_eq!(get_resp.status().as_u16(), 200);
    let csrf = read_set_cookie(get_resp.headers(), "zsidp_csrf")
        .expect("zsidp_csrf cookie set on GET /signup");

    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("csrf", &csrf)
        .append_pair("name", "M3_FAIL")
        .append_pair("email", &email)
        .append_pair("password", "correct horse battery staple")
        .finish();
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/signup")
            .peer_addr(SocketAddr::new(unique_loopback(), 49154))
            .header("content-type", "application/x-www-form-urlencoded")
            .header("cookie", format!("zsidp_csrf={csrf}"))
            .set_payload(body)
            .to_request(),
    )
    .await;
    assert_eq!(
        resp.status().as_u16(),
        200,
        "non-23505 create failure must render an error page, not redirect"
    );
    assert!(
        resp.headers().get(LOCATION).is_none(),
        "non-23505 create failure must not return /login redirect"
    );
    assert_eq!(
        mailer.count(),
        0,
        "failed signup must not issue verification email"
    );

    let created: i64 = pg
        .query_one(
            "SELECT COUNT(*) FROM zeroship.users WHERE email = $1::citext",
            &[&email],
        )
        .await
        .expect("count users")
        .get(0);
    assert_eq!(created, 0, "failed signup must not create user");

    pg.execute(
        "ALTER TABLE zeroship.users \
         DROP CONSTRAINT IF EXISTS auth_users_signup_m3_name_check",
        &[],
    )
    .await
    .ok();
    pg.execute(
        "DELETE FROM zeroship.audit_events \
         WHERE event_type = 'signup_failed' \
           AND detail->>'reason' = 'users_create_failed'",
        &[],
    )
    .await
    .ok();
}

#[compio::test]
#[allow(clippy::future_not_send)]
async fn forgot_post_throttles_after_email_bucket_capacity() {
    let Some((dsn, client)) = pg().await else {
        eprintln!("skipping signup_forgot_ratelimit_test (no AUTH_DB_URL)");
        return;
    };

    let email = format!("forgot-rl-{}@zeroship.test", Uuid::new_v4().simple());
    let user = users::create(&client, &email, "Forgot Test", None)
        .await
        .expect("seed user");

    let pg = Arc::new(client);
    let cfg = Arc::new(test_cfg(&dsn));
    let mailer = Arc::new(CountingMailer::default());
    let mailer_state: Arc<dyn Mailer> = mailer.clone();
    let app = test::init_service(
        web::App::new()
            .state(cfg.clone())
            .state(pg.clone())
            .state(mailer_state)
            .service(
                web::resource("/forgot")
                    .route(web::get().to(zeroship_auth::ui::forgot::get))
                    .route(web::post().to(zeroship_auth::ui::forgot::post)),
            ),
    )
    .await;

    let get_resp = test::call_service(
        &app,
        test::TestRequest::get().uri("/forgot").to_request(),
    )
    .await;
    assert_eq!(get_resp.status().as_u16(), 200);
    let csrf = read_set_cookie(get_resp.headers(), "zsidp_csrf")
        .expect("zsidp_csrf cookie set on GET /forgot");

    let peer = SocketAddr::new(unique_loopback(), 49153);
    for i in 0..6 {
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("csrf", &csrf)
            .append_pair("email", &email)
            .finish();
        let resp = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/forgot")
                .peer_addr(peer)
                // See signup test: forwarded IP, not socket peer.
                .header("x-forwarded-for", peer.ip().to_string())
                .header("content-type", "application/x-www-form-urlencoded")
                .header("cookie", format!("zsidp_csrf={csrf}"))
                .set_payload(body)
                .to_request(),
        )
        .await;
        assert_eq!(resp.status().as_u16(), 200, "forgot response {i}");
    }

    assert_eq!(
        mailer.count(),
        5,
        "6th forgot request must be throttled before email send"
    );

    pg.execute(
        "DELETE FROM zeroship.magic_links WHERE email = $1::citext",
        &[&email],
    )
    .await
    .ok();
    pg.execute("DELETE FROM zeroship.audit_events WHERE actor_user_id = $1", &[&user.id])
        .await
        .ok();
    pg.execute("DELETE FROM zeroship.users WHERE id = $1", &[&user.id])
        .await
        .ok();
}
