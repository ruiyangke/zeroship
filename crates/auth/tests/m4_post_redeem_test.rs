#![allow(dead_code)]

mod common;

use std::future::Future;
use std::sync::Arc;

use ntex::http::header::{CACHE_CONTROL, SET_COOKIE};
use ntex::http::HeaderMap;
use ntex::service::{Pipeline, Service};
use ntex::web::{self, test};

use common::test_auth_config;
use zeroship_auth::headers::SecurityHeaders;
use zeroship_auth::identity::{magic_link, verification};
use zeroship_auth::server;
use zeroship_auth::store::users;
use uuid::Uuid;

macro_rules! init_app {
    ($ctx:expr) => {
        test::init_service(
            web::App::new()
                .state($ctx.cfg.clone())
                .state($ctx.pg.clone())
                .state($ctx.refresh_pool.clone())
                .middleware(SecurityHeaders::default())
                .configure(server::configure(false, false)),
        )
        .await
    };
}

fn run_compio<F: Future<Output = ()>>(future: F) {
    let mut proactor = compio::driver::ProactorBuilder::new();
    proactor.driver_type(compio::driver::DriverType::Poll);
    let mut builder = compio::runtime::RuntimeBuilder::new();
    builder.with_proactor(proactor);
    builder
        .build()
        .expect("cannot create polling compio runtime")
        .block_on(future);
}

struct M4TestCtx {
    cfg: Arc<zeroship_auth::config::AuthConfig>,
    pg: Arc<compio_postgres::Client>,
    refresh_pool: zeroship_auth::oidc::refresh::RefreshSessionPool,
}

impl M4TestCtx {
    #[allow(clippy::future_not_send)]
    async fn boot() -> Option<Self> {
        let db_url = std::env::var("AUTH_DB_URL")
            .or_else(|_| std::env::var("PG_TEST_URL"))
            .ok()?;
        let (pg_client, pg_connection) =
            compio_postgres::connect(&db_url, compio_postgres::NoTls)
                .await
                .expect("connect pg");
        compio::runtime::spawn(async move {
            if let Err(e) = pg_connection.run().await {
                eprintln!("[m4_post_redeem_test] pg connection driver: {e}");
            }
        })
        .detach();
        let pg = Arc::new(pg_client);

        let cfg = Arc::new(test_auth_config(&db_url));
        let refresh_pool = zeroship_auth::oidc::refresh::RefreshSessionPool::new(db_url, 4);

        Some(Self {
            cfg,
            pg,
            refresh_pool,
        })
    }

    #[allow(clippy::future_not_send)]
    async fn seed_verification(&self) -> (Uuid, String, String) {
        let email = format!("m4-verify-{}@zeroship.test", Uuid::new_v4().simple());
        let user = users::create(&self.pg, &email, "M4 Verify", None)
            .await
            .expect("seed verify user");
        let issued = verification::issue(&self.pg, user.id, &email)
            .await
            .expect("issue verification token");
        (user.id, email, issued.raw)
    }

    #[allow(clippy::future_not_send)]
    async fn seed_magic(&self) -> (String, magic_link::IssuedToken) {
        let email = format!("m4-magic-{}@zeroship.test", Uuid::new_v4().simple());
        let issued = magic_link::issue(&self.pg, &email, "login")
            .await
            .expect("issue magic token");
        (email, issued)
    }

    #[allow(clippy::future_not_send)]
    async fn cleanup_email(&self, email: &str) {
        let _ = self
            .pg
            .execute(
                "DELETE FROM zeroship.idp_sessions WHERE user_id IN \
                 (SELECT id FROM zeroship.users WHERE email = $1::citext)",
                &[&email],
            )
            .await;
        let _ = self
            .pg
            .execute(
                "DELETE FROM zeroship.audit_events WHERE actor_user_id IN \
                 (SELECT id FROM zeroship.users WHERE email = $1::citext)",
                &[&email],
            )
            .await;
        let _ = self
            .pg
            .execute(
                "DELETE FROM zeroship.magic_completions WHERE email = $1::citext",
                &[&email],
            )
            .await;
        let _ = self
            .pg
            .execute(
                "DELETE FROM zeroship.magic_links WHERE email = $1::citext",
                &[&email],
            )
            .await;
        let _ = self
            .pg
            .execute(
                "DELETE FROM zeroship.email_verifications WHERE email = $1::citext",
                &[&email],
            )
            .await;
        let _ = self
            .pg
            .execute("DELETE FROM zeroship.users WHERE email = $1::citext", &[&email])
            .await;
    }

}

#[test]
fn verify_get_renders_interstitial_does_not_consume_token() {
    run_compio(async {
    let Some(ctx) = M4TestCtx::boot().await else {
        eprintln!("skipping m4_post_redeem_test (no AUTH_DB_URL or PG_TEST_URL)");
        return;
    };
    let (user_id, email, token) = ctx.seed_verification().await;
    let app = init_app!(&ctx);

    let resp = call_get(&app, &format!("/verify?token={token}")).await;
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(header(resp.headers(), CACHE_CONTROL), "no-store");
    let body = read_body(resp).await;
    assert!(body.contains(r#"<form method="POST" action="/verify/redeem">"#));
    assert!(body.contains(&format!(r#"name="token" value="{token}""#)));

    let remaining: i64 = ctx
        .pg
        .query_one(
            "SELECT COUNT(*) FROM zeroship.email_verifications \
             WHERE user_id = $1 AND consumed_at IS NULL",
            &[&user_id],
        )
        .await
        .expect("count verification rows")
        .get(0);
    assert_eq!(remaining, 1, "GET /verify must not consume the token");

    ctx.cleanup_email(&email).await;
    });
}

#[test]
fn verify_post_redeem_consumes_token_and_marks_verified() {
    run_compio(async {
    let Some(ctx) = M4TestCtx::boot().await else {
        eprintln!("skipping m4_post_redeem_test (no AUTH_DB_URL or PG_TEST_URL)");
        return;
    };
    let (user_id, email, token) = ctx.seed_verification().await;
    let app = init_app!(&ctx);
    let csrf = csrf_from_verify_get(&app, &token).await;

    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("csrf", &csrf)
        .append_pair("token", &token)
        .finish();
    let resp = call_post_form(
        &app,
        "/verify/redeem",
        body,
        Some(format!("zsidp_csrf={csrf}")),
    )
    .await;
    assert_eq!(resp.status().as_u16(), 200);
    let html = read_body(resp).await;
    assert!(html.contains("Email verified"));

    let verified: bool = ctx
        .pg
        .query_one(
            "SELECT email_verified_at IS NOT NULL AS verified FROM zeroship.users WHERE id = $1",
            &[&user_id],
        )
        .await
        .expect("load user")
        .get("verified");
    assert!(verified, "POST /verify/redeem must mark email verified");

    ctx.cleanup_email(&email).await;
    });
}

#[test]
fn verify_post_redeem_with_invalid_csrf_rejected() {
    run_compio(async {
    let Some(ctx) = M4TestCtx::boot().await else {
        eprintln!("skipping m4_post_redeem_test (no AUTH_DB_URL or PG_TEST_URL)");
        return;
    };
    let (_, email, token) = ctx.seed_verification().await;
    let app = init_app!(&ctx);
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("csrf", "wrong")
        .append_pair("token", &token)
        .finish();

    let resp = call_post_form(&app, "/verify/redeem", body, None).await;
    assert_eq!(resp.status().as_u16(), 403);

    ctx.cleanup_email(&email).await;
    });
}

#[test]
fn verify_post_redeem_with_invalid_token_renders_error_page() {
    run_compio(async {
    let Some(ctx) = M4TestCtx::boot().await else {
        eprintln!("skipping m4_post_redeem_test (no AUTH_DB_URL or PG_TEST_URL)");
        return;
    };
    let app = init_app!(&ctx);
    let request_id = format!("m4-invalid-{}", Uuid::new_v4().simple());
    let token = format!("missing-{}", Uuid::new_v4().simple());
    let csrf = csrf_from_verify_get(&app, &token).await;
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("csrf", &csrf)
        .append_pair("token", &token)
        .finish();

    let resp = call_post_form_with_request_id(
        &app,
        "/verify/redeem",
        body,
        Some(format!("zsidp_csrf={csrf}")),
        &request_id,
    )
    .await;
    assert_eq!(resp.status().as_u16(), 200);
    let html = read_body(resp).await;
    assert!(html.contains("session expired"));
    assert_eq!(
        verification_failure_audit_count(&ctx.pg, &request_id).await,
        1
    );
    });
}

#[test]
fn verify_post_redeem_idempotent_second_call_returns_error() {
    run_compio(async {
    let Some(ctx) = M4TestCtx::boot().await else {
        eprintln!("skipping m4_post_redeem_test (no AUTH_DB_URL or PG_TEST_URL)");
        return;
    };
    let (_, email, token) = ctx.seed_verification().await;
    let app = init_app!(&ctx);
    let csrf = csrf_from_verify_get(&app, &token).await;
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("csrf", &csrf)
        .append_pair("token", &token)
        .finish();

    let first = call_post_form(
        &app,
        "/verify/redeem",
        body.clone(),
        Some(format!("zsidp_csrf={csrf}")),
    )
    .await;
    assert_eq!(first.status().as_u16(), 200);

    let second = call_post_form(
        &app,
        "/verify/redeem",
        body,
        Some(format!("zsidp_csrf={csrf}")),
    )
    .await;
    assert_eq!(second.status().as_u16(), 200);
    let html = read_body(second).await;
    assert!(html.contains("session expired"));

    ctx.cleanup_email(&email).await;
    });
}

#[test]
fn reset_get_html_includes_history_replace_state_script() {
    run_compio(async {
    let Some(ctx) = M4TestCtx::boot().await else {
        eprintln!("skipping m4_post_redeem_test (no AUTH_DB_URL or PG_TEST_URL)");
        return;
    };
    let app = init_app!(&ctx);

    let resp = call_get(&app, "/reset?token=ABC").await;
    assert_eq!(resp.status().as_u16(), 200);
    let body = read_body(resp).await;
    assert!(body.contains("window.history.replaceState"));
    });
}

#[test]
fn cache_control_no_store_on_all_three_interstitials() {
    run_compio(async {
    let Some(ctx) = M4TestCtx::boot().await else {
        eprintln!("skipping m4_post_redeem_test (no AUTH_DB_URL or PG_TEST_URL)");
        return;
    };
    let app = init_app!(&ctx);

    let verify = call_get(&app, "/verify?token=ABC").await;
    assert_eq!(header(verify.headers(), CACHE_CONTROL), "no-store");

    let magic = call_get(
        &app,
        "/magic/verify?token=ABC&return_to=/oauth2/authorize?client_id=oac_123",
    )
    .await;
    assert_eq!(header(magic.headers(), CACHE_CONTROL), "no-store");

    let reset = call_get(&app, "/reset?token=ABC").await;
    assert_eq!(header(reset.headers(), CACHE_CONTROL), "no-store");
    });
}

#[allow(clippy::future_not_send)]
async fn call_get<S, E>(app: &Pipeline<S>, uri: &str) -> ntex::web::WebResponse
where
    S: Service<ntex::http::Request, Response = ntex::web::WebResponse, Error = E>,
    E: std::fmt::Debug,
{
    test::call_service(app, test::TestRequest::get().uri(uri).to_request()).await
}

#[allow(clippy::future_not_send)]
async fn call_get_with_cookie<S, E>(
    app: &Pipeline<S>,
    uri: &str,
    cookie: String,
) -> ntex::web::WebResponse
where
    S: Service<ntex::http::Request, Response = ntex::web::WebResponse, Error = E>,
    E: std::fmt::Debug,
{
    test::call_service(
        app,
        test::TestRequest::get()
            .uri(uri)
            .header("cookie", cookie)
            .to_request(),
    )
    .await
}

#[allow(clippy::future_not_send)]
async fn call_post_form<S, E>(
    app: &Pipeline<S>,
    uri: &str,
    body: String,
    cookie: Option<String>,
) -> ntex::web::WebResponse
where
    S: Service<ntex::http::Request, Response = ntex::web::WebResponse, Error = E>,
    E: std::fmt::Debug,
{
    let mut req = test::TestRequest::post()
        .uri(uri)
        .header("content-type", "application/x-www-form-urlencoded");
    if let Some(cookie) = cookie {
        req = req.header("cookie", cookie);
    }
    test::call_service(app, req.set_payload(body).to_request()).await
}

#[allow(clippy::future_not_send)]
async fn call_post_form_with_request_id<S, E>(
    app: &Pipeline<S>,
    uri: &str,
    body: String,
    cookie: Option<String>,
    request_id: &str,
) -> ntex::web::WebResponse
where
    S: Service<ntex::http::Request, Response = ntex::web::WebResponse, Error = E>,
    E: std::fmt::Debug,
{
    let mut req = test::TestRequest::post()
        .uri(uri)
        .header("content-type", "application/x-www-form-urlencoded")
        .header("x-request-id", request_id);
    if let Some(cookie) = cookie {
        req = req.header("cookie", cookie);
    }
    test::call_service(app, req.set_payload(body).to_request()).await
}

#[allow(clippy::future_not_send)]
async fn csrf_from_verify_get<S, E>(app: &Pipeline<S>, token: &str) -> String
where
    S: Service<ntex::http::Request, Response = ntex::web::WebResponse, Error = E>,
    E: std::fmt::Debug,
{
    let resp = call_get(app, &format!("/verify?token={token}")).await;
    assert_eq!(resp.status().as_u16(), 200);
    read_set_cookie(resp.headers(), "zsidp_csrf").expect("zsidp_csrf cookie set on GET /verify")
}

async fn verification_failure_audit_count(
    pg: &compio_postgres::Client,
    request_id: &str,
) -> i64 {
    pg.query_one(
        "SELECT COUNT(*) FROM zeroship.audit_events \
         WHERE event_type = 'verification_redeemed' \
           AND outcome = 'failure' \
           AND detail->>'reason' = 'invalid_or_expired' \
           AND request_id = $1",
        &[&request_id],
    )
    .await
    .expect("count verification failure audit")
    .get(0)
}

async fn read_body(resp: ntex::web::WebResponse) -> String {
    let body = test::read_body(resp).await;
    String::from_utf8(body.to_vec()).expect("utf8 response body")
}

fn header(headers: &HeaderMap, name: ntex::http::header::HeaderName) -> &str {
    headers
        .get(name)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("")
}

fn read_set_cookie(headers: &HeaderMap, name: &str) -> Option<String> {
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

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|idx| idx + 4)
}
