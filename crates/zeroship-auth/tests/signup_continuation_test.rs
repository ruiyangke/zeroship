//! `/signup` reached the way the product reaches it, plus the enumeration
//! defense that reach must not cost.
//!
//! THE REGRESSION. `/login` renders `href="/signup?return_to={{ return_to }}"`
//! from its OWN sanitized continuation, which on a plain visit is
//! `return_to::SAFE_DEFAULT` (`/me`). `/signup` used to require that the
//! continuation parse as an `/oauth2/authorize` request AND that exactly one
//! copy of it arrive, so `/me` was an error and so was an absent one. The
//! login page's own link answered `400 invalid request` on a page that still
//! drew the form, the CSRF token and a banner -- it looked usable and could
//! not be used, and a bare `/signup` behaved the same way.
//!
//! Every test below takes the signup URL OUT OF THE RENDERED LOGIN PAGE. A
//! test that composes the URL instead is testing its own author's idea of a
//! valid continuation, which is exactly how this shipped.
//!
//! Requires a live PostgreSQL (`PG_TEST_URL` or the TOML overlay). A run
//! that cannot reach one is REFUSED, not skipped.

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;

use async_trait::async_trait;
use compio_postgres::{connect, NoTls};
use ntex::http::header::{LOCATION, SET_COOKIE};
use ntex::web::{self, test};
use uuid::Uuid;

use zeroship_auth::config::AuthConfig;
use zeroship_core::config::{Secret, SourceKind};
use zeroship_mailer::{Email, Mailer, MailerError, MessageId};

const PASSWORD: &str = "correct horse battery staple";

#[derive(Debug, Default)]
struct NoopMailer;

#[async_trait]
impl Mailer for NoopMailer {
    async fn send(
        &self,
        _db: &compio_postgres::Client,
        _msg: Email,
    ) -> Result<MessageId, MailerError> {
        Ok(MessageId("test-message".to_string()))
    }
}

fn test_cfg(db_url: &str) -> AuthConfig {
    // Same shape as the sibling signup tests: a resolved `Secret` is what
    // `ZEROSHIP_AUTH_<NAME>=<value>` produces, and the environment is
    // process-global so a literal here cannot race a concurrent test.
    let mut cfg = AuthConfig::parse_from(["zeroship-auth"]);
    cfg.settings.database_url = Secret::supplied(SourceKind::Env, Some(db_url.to_owned()));
    cfg.settings.stash_signing_key = Secret::supplied(
        SourceKind::Env,
        Some("test-stash-key-not-for-prod-32bytes!".to_owned()),
    );
    cfg
}

#[allow(clippy::future_not_send)]
async fn pg() -> (String, compio_postgres::Client) {
    let dsn =
        crate::common::test_database_url();
    let (client, connection) = connect(&dsn, NoTls).await.expect("connect");
    compio::runtime::spawn(async move {
        if let Err(e) = connection.run().await {
            eprintln!("signup_continuation_test pg connection error: {e}");
        }
    })
    .detach();
    (dsn, client)
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

fn location(headers: &ntex::http::HeaderMap) -> String {
    headers
        .get(LOCATION)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("")
        .to_string()
}

/// The first `href="/signup..."` on the page, un-escaped the way a browser
/// would read it. Deliberately NOT a composed URL -- see the module docs.
fn signup_href(login_html: &str) -> String {
    let start = login_html
        .find("href=\"/signup")
        .expect("the login page must offer a /signup link");
    let rest = &login_html[start + "href=\"".len()..];
    let end = rest.find('"').expect("unterminated href");
    rest[..end].replace("&amp;", "&").replace("&#x27;", "'")
}

/// The `value` of `<input name="NAME" ...>`, un-escaped.
fn form_field(html: &str, name: &str) -> String {
    let needle = format!("name=\"{name}\"");
    let at = html
        .find(&needle)
        .unwrap_or_else(|| panic!("no input named {name} in the rendered form"));
    let tag_start = html[..at].rfind('<').expect("input tag start");
    let tag_end = at + html[at..].find('>').expect("input tag end");
    let tag = &html[tag_start..tag_end];
    let vstart = tag.find("value=\"").expect("input has no value") + "value=\"".len();
    let vend = vstart + tag[vstart..].find('"').expect("unterminated value");
    tag[vstart..vend]
        .replace("&amp;", "&")
        .replace("&#x27;", "'")
        .replace("&quot;", "\"")
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

fn signup_body(csrf: &str, return_to: &str, email: &str) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .append_pair("csrf", csrf)
        .append_pair("return_to", return_to)
        .append_pair("name", "Continuation Test")
        .append_pair("email", email)
        .append_pair("password", PASSWORD)
        .finish()
}

async fn cleanup(pg: &compio_postgres::Client, email: &str) {
    pg.execute(
        "DELETE FROM zeroship.email_verifications WHERE email = $1::citext",
        &[&email],
    )
    .await
    .ok();
    pg.execute(
        "DELETE FROM zeroship.audit_events \
         WHERE actor_user_id IN (SELECT id FROM zeroship.users WHERE email = $1::citext)",
        &[&email],
    )
    .await
    .ok();
    pg.execute(
        "DELETE FROM zeroship.users WHERE email = $1::citext",
        &[&email],
    )
    .await
    .ok();
}

macro_rules! signup_app {
    ($cfg:expr, $pg:expr, $mailer:expr) => {
        test::init_service(
            web::App::new()
                .state($cfg)
                .state($pg)
                .state($mailer)
                .service(
                    web::resource("/login")
                        .route(web::get().to(zeroship_auth::ui::login::get))
                        .route(web::post().to(zeroship_auth::ui::login::post)),
                )
                .service(
                    web::resource("/signup")
                        .route(web::get().to(zeroship_auth::ui::signup::get))
                        .route(web::post().to(zeroship_auth::ui::signup::post)),
                ),
        )
        .await
    };
}

/// Steps 1 and 2 of the creator's walk: follow the login page's own link,
/// then submit the form it returns. Before the fix, step 1 was HTTP 400.
#[compio::test]
#[allow(clippy::future_not_send)]
async fn the_login_pages_own_signup_link_creates_an_account() {
    let (dsn, client) = pg().await;
    let pg = Arc::new(client);
    let cfg = Arc::new(test_cfg(&dsn));
    let mailer: Arc<dyn Mailer> = Arc::new(NoopMailer);
    let app = signup_app!(cfg.clone(), pg.clone(), mailer);

    // 1. The login page, as a signed-out visitor sees it.
    let login = test::call_service(&app, test::TestRequest::get().uri("/login").to_request()).await;
    assert_eq!(login.status().as_u16(), 200, "GET /login");
    let login_html = String::from_utf8(test::read_body(login).await.to_vec()).expect("login html");
    let href = signup_href(&login_html);
    assert!(
        href.starts_with("/signup"),
        "expected a /signup link, got {href}"
    );

    // 2. That exact URL. THIS is the assertion the bug fails: it answered 400
    //    "invalid request" because `/me` is not an `/oauth2/authorize` target.
    let get = test::call_service(&app, test::TestRequest::get().uri(&href).to_request()).await;
    assert_eq!(
        get.status().as_u16(),
        200,
        "the login page's own signup link must reach the form, got {} for {href}",
        get.status()
    );
    let csrf = read_set_cookie(get.headers(), "__Host-zsidp_csrf").expect("csrf cookie");
    let form_html = String::from_utf8(test::read_body(get).await.to_vec()).expect("signup html");
    assert!(
        !form_html.contains("<div class=\"error\">"),
        "the signup form must not render an error banner: {form_html}"
    );
    let echoed = form_field(&form_html, "return_to");
    assert!(
        !echoed.is_empty(),
        "the form must echo a continuation to sign in against"
    );

    // 3. Submit it. A real row, and a redirect that carries the continuation.
    let email = format!("signup-link-{}@zeroship.test", Uuid::new_v4().simple());
    let resp = test::call_service(
        &app,
        test::TestRequest::post()
            .uri("/signup")
            .header("x-forwarded-for", unique_loopback().to_string())
            .header("content-type", "application/x-www-form-urlencoded")
            .header("cookie", format!("__Host-zsidp_csrf={csrf}"))
            .set_payload(signup_body(&csrf, &echoed, &email))
            .to_request(),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 302, "POST /signup");
    assert_eq!(
        location(resp.headers()),
        "/login?return_to=%2Fme",
        "signup must send the new account back to the continuation it was given"
    );

    let rows: i64 = pg
        .query_one(
            "SELECT COUNT(*) FROM zeroship.users WHERE email = $1::citext",
            &[&email],
        )
        .await
        .expect("count signup user")
        .get(0);
    assert_eq!(rows, 1, "signup must have created the user");

    cleanup(pg.as_ref(), &email).await;
}

/// A bare `/signup` -- no continuation at all. `from_inputs` demanded exactly
/// one target and zero is not one, so `return_to: Option<String>` was a lie
/// and this was 400 too.
#[compio::test]
#[allow(clippy::future_not_send)]
async fn a_bare_signup_url_renders_the_form() {
    let (dsn, client) = pg().await;
    let pg = Arc::new(client);
    let cfg = Arc::new(test_cfg(&dsn));
    let mailer: Arc<dyn Mailer> = Arc::new(NoopMailer);
    let app = signup_app!(cfg, pg, mailer);

    let resp = test::call_service(&app, test::TestRequest::get().uri("/signup").to_request()).await;
    assert_eq!(resp.status().as_u16(), 200, "GET /signup with no return_to");
    let html = String::from_utf8(test::read_body(resp).await.to_vec()).expect("signup html");
    assert!(
        !html.contains("<div class=\"error\">"),
        "a bare /signup must not render an error banner: {html}"
    );
    assert_eq!(
        form_field(&html, "return_to"),
        "/me",
        "an absent continuation takes the safe default"
    );
}

/// The account-enumeration defense, at the byte level.
///
/// A duplicate-email INSERT must be indistinguishable from a fresh one:
/// same status, same `Location`, same body. If any of the three diverges,
/// `/signup` is an oracle for "is this address registered".
///
/// Both signups run through the SAME app, the same continuation and the same
/// rate-limit bucket, so the only variable is whether the address already
/// exists.
#[compio::test]
#[allow(clippy::future_not_send)]
async fn a_duplicate_signup_is_indistinguishable_from_a_fresh_one() {
    let (dsn, client) = pg().await;
    let pg = Arc::new(client);
    let cfg = Arc::new(test_cfg(&dsn));
    let mailer: Arc<dyn Mailer> = Arc::new(NoopMailer);
    let app = signup_app!(cfg, pg.clone(), mailer);
    let ip = unique_loopback().to_string();

    // One GET per POST: the form mints a fresh CSRF token each time, exactly
    // as a browser would receive it.
    let post = |email: String, ip: String| {
        let app = &app;
        async move {
            let get =
                test::call_service(app, test::TestRequest::get().uri("/signup").to_request()).await;
            let csrf = read_set_cookie(get.headers(), "__Host-zsidp_csrf").expect("csrf cookie");
            let resp = test::call_service(
                app,
                test::TestRequest::post()
                    .uri("/signup")
                    .header("x-forwarded-for", ip)
                    .header("content-type", "application/x-www-form-urlencoded")
                    .header("cookie", format!("__Host-zsidp_csrf={csrf}"))
                    .set_payload(signup_body(&csrf, "/me", &email))
                    .to_request(),
            )
            .await;
            let status = resp.status().as_u16();
            let loc = location(resp.headers());
            let body = test::read_body(resp).await.to_vec();
            (status, loc, body)
        }
    };

    let taken = format!("signup-dup-{}@zeroship.test", Uuid::new_v4().simple());
    let unused = format!("signup-new-{}@zeroship.test", Uuid::new_v4().simple());

    let (first_status, _, _) = post(taken.clone(), ip.clone()).await;
    assert_eq!(first_status, 302, "the first signup must succeed");

    let (dup_status, dup_loc, dup_body) = post(taken.clone(), ip.clone()).await;
    let (fresh_status, fresh_loc, fresh_body) = post(unused.clone(), ip.clone()).await;

    assert_eq!(
        dup_status, fresh_status,
        "duplicate and fresh signups must share a status"
    );
    assert_eq!(
        dup_loc, fresh_loc,
        "duplicate and fresh signups must share a Location"
    );
    assert_eq!(
        dup_body, fresh_body,
        "duplicate and fresh signups must share a body"
    );

    // The defense is only interesting if the duplicate really was refused.
    let rows: i64 = pg
        .query_one(
            "SELECT COUNT(*) FROM zeroship.users WHERE email = $1::citext",
            &[&taken],
        )
        .await
        .expect("count duplicate user")
        .get(0);
    assert_eq!(rows, 1, "the duplicate must not have created a second row");

    cleanup(pg.as_ref(), &taken).await;
    cleanup(pg.as_ref(), &unused).await;
}

/// The mechanism the defense above rides on, asserted directly.
///
/// `ui/signup.rs` decides "duplicate" by `e.db_code() == Some("23505")`.
/// `AuthError::DbCode` is the ONLY variant carrying a code, so a `users::create`
/// that flattens the SQLSTATE into a string makes that arm unreachable without
/// changing a single line at the call site -- which is exactly how the defense
/// was lost. This test fails on that shape and passes on this one.
#[compio::test]
#[allow(clippy::future_not_send)]
async fn a_duplicate_insert_reports_its_sqlstate() {
    let (_dsn, client) = pg().await;
    let email = format!("signup-code-{}@zeroship.test", Uuid::new_v4().simple());

    zeroship_auth::store::users::create(&client, &email, "Continuation Test", None)
        .await
        .expect("first insert");
    let err = zeroship_auth::store::users::create(&client, &email, "Continuation Test", None)
        .await
        .expect_err("a duplicate email must not insert twice");

    assert_eq!(
        err.db_code(),
        Some("23505"),
        "the duplicate-email SQLSTATE must survive the error mapping, got {err}"
    );

    cleanup(&client, &email).await;
}

/// Tolerating a continuation is not the same as trusting one: an off-origin
/// target must be REPLACED by the safe default, never echoed into the form
/// and never sent in a `Location`.
#[compio::test]
#[allow(clippy::future_not_send)]
async fn an_off_origin_continuation_is_replaced_not_echoed() {
    let (dsn, client) = pg().await;
    let pg = Arc::new(client);
    let cfg = Arc::new(test_cfg(&dsn));
    let mailer: Arc<dyn Mailer> = Arc::new(NoopMailer);
    let app = signup_app!(cfg, pg.clone(), mailer);

    for bad in ["//evil.example", "https://evil.example", "/\\evil.example"] {
        let uri = format!(
            "/signup?{}",
            url::form_urlencoded::Serializer::new(String::new())
                .append_pair("return_to", bad)
                .finish()
        );
        let resp = test::call_service(&app, test::TestRequest::get().uri(&uri).to_request()).await;
        assert_eq!(resp.status().as_u16(), 200, "GET {uri}");
        let csrf = read_set_cookie(resp.headers(), "__Host-zsidp_csrf").expect("csrf cookie");
        let html = String::from_utf8(test::read_body(resp).await.to_vec()).expect("signup html");
        assert_eq!(
            form_field(&html, "return_to"),
            "/me",
            "{bad} must be replaced by the safe default, not echoed"
        );

        let email = format!("signup-evil-{}@zeroship.test", Uuid::new_v4().simple());
        let resp = test::call_service(
            &app,
            test::TestRequest::post()
                .uri(&uri)
                .header("x-forwarded-for", unique_loopback().to_string())
                .header("content-type", "application/x-www-form-urlencoded")
                .header("cookie", format!("__Host-zsidp_csrf={csrf}"))
                .set_payload(signup_body(&csrf, bad, &email))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status().as_u16(), 302, "POST /signup with {bad}");
        assert_eq!(
            location(resp.headers()),
            "/login?return_to=%2Fme",
            "{bad} must never reach a Location header"
        );
        cleanup(pg.as_ref(), &email).await;
    }
}
