//! `/logout` regression coverage.
//!
//! Pre-fix (bug #2): `crates/auth/src/server.rs` did not register a
//! handler for `/logout`, so any GET / POST returned 404. This blocked
//! every RP-initiated logout (`ops/hydra.yaml.urls.logout` 302s the
//! user-agent here).
//!
//! Post-fix: GET `/logout?logout_challenge=…` is wired to a handler. If
//! hydra rejects the challenge as invalid we render a 400 HTML error
//! page; if hydra accepts the challenge we render a 200 confirmation
//! form. Either way: NOT 404.
//!
//! The "happy path with a real hydra-issued logout_challenge" test
//! lives in the integration e2e harness — it requires driving a full
//! login flow first to obtain an `id_token` to hand to
//! `/oauth2/sessions/logout` (the only way to mint a real
//! logout_challenge). That coverage is the followup.

use std::sync::Arc;

use ntex::web;

use zeroship_auth::hydra_client::HydraAdmin;
use zeroship_auth::mailer::{Mailer, StdoutMailer};
use zeroship_auth::server;
use zeroship_auth::store::migrations;

mod common;
use common::test_auth_config;

#[ntex::test]
async fn logout_route_is_registered_returns_not_404() {
    let (Ok(db_url), Ok(hydra_admin_url)) = (
        std::env::var("AUTH_DB_URL"),
        std::env::var("AUTH_HYDRA_ADMIN"),
    ) else {
        eprintln!("[logout_test] skip (need AUTH_DB_URL + AUTH_HYDRA_ADMIN)");
        return;
    };
    let hydra_public = std::env::var("AUTH_HYDRA_PUBLIC_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:4444".to_string());

    let (pg_client, pg_connection) = compio_postgres::connect(&db_url, compio_postgres::NoTls)
        .await
        .expect("connect pg");
    compio::runtime::spawn(async move {
        if let Err(e) = pg_connection.run().await {
            eprintln!("[logout_test] pg connection driver: {e}");
        }
    })
    .detach();
    migrations::migrate(&pg_client).await.expect("migrate");
    let pg = Arc::new(pg_client);

    let admin = HydraAdmin::new(&hydra_admin_url);
    let cfg = Arc::new(test_auth_config(&db_url, &hydra_admin_url, &hydra_public));
    let admin_state = admin.clone();
    let cfg_state = cfg.clone();
    let db_state = pg.clone();
    let mailer_state: Arc<dyn Mailer> = Arc::new(StdoutMailer);
    let srv = web::test::server(move || {
        let admin_state = admin_state.clone();
        let cfg_state = cfg_state.clone();
        let db_state = db_state.clone();
        let mailer_state = mailer_state.clone();
        async move {
            web::App::new()
                .state(admin_state)
                .state(cfg_state)
                .state(db_state)
                .state(mailer_state)
                .configure(server::configure(false, false))
        }
    })
    .await;
    let auth_base = srv.url("").trim_end_matches('/').to_string();

    // Drive `/logout?logout_challenge=does-not-exist`. The handler is
    // registered, so the response is the 400 HTML error page hydra
    // surfaced when it couldn't resolve the fake challenge. Pre-fix
    // the route was unmapped → ntex returned 404. The regression-
    // failing assertion is on the absence of 404.
    let http = cyper::Client::new();
    let resp = http
        .request(
            http::Method::GET,
            &format!("{auth_base}/logout?logout_challenge=test-fake-challenge-does-not-exist"),
        )
        .expect("build GET /logout")
        .send()
        .await
        .expect("send GET /logout");

    let status = resp.status().as_u16();
    assert_ne!(
        status, 404,
        "/logout MUST be registered — pre-fix it returned 404, blocking every RP-initiated logout (hydra's urls.logout points here)"
    );

    // Post-fix we either render a confirm form (200) when hydra
    // accepts the challenge OR an error page (400) when it doesn't.
    // The fake challenge above lands in the 400 arm.
    assert!(
        status == 200 || status == 400,
        "expected 200 (form) or 400 (hydra rejected fake challenge); got {status}"
    );

    // Body is HTML, not the ntex 404 fallback.
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        ct.starts_with("text/html"),
        "expected text/html (the handler's response), got content-type {ct:?}"
    );
}

/// POST `/logout` without a valid CSRF cookie must NOT be a 404 (i.e.
/// the POST handler is registered alongside GET). Pre-fix only the
/// missing GET route was visible to users, but the missing POST was
/// the deeper bug — even if a user hand-crafted a POST, ntex would
/// 405 (route exists, method missing). Cover that.
#[ntex::test]
async fn logout_post_is_registered_returns_not_404_or_405() {
    let (Ok(db_url), Ok(hydra_admin_url)) = (
        std::env::var("AUTH_DB_URL"),
        std::env::var("AUTH_HYDRA_ADMIN"),
    ) else {
        eprintln!("[logout_test] skip (need AUTH_DB_URL + AUTH_HYDRA_ADMIN)");
        return;
    };
    let hydra_public = std::env::var("AUTH_HYDRA_PUBLIC_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:4444".to_string());

    let (pg_client, pg_connection) = compio_postgres::connect(&db_url, compio_postgres::NoTls)
        .await
        .expect("connect pg");
    compio::runtime::spawn(async move {
        if let Err(e) = pg_connection.run().await {
            eprintln!("[logout_test] pg connection driver: {e}");
        }
    })
    .detach();
    migrations::migrate(&pg_client).await.expect("migrate");
    let pg = Arc::new(pg_client);

    let admin = HydraAdmin::new(&hydra_admin_url);
    let cfg = Arc::new(test_auth_config(&db_url, &hydra_admin_url, &hydra_public));
    let admin_state = admin.clone();
    let cfg_state = cfg.clone();
    let db_state = pg.clone();
    let mailer_state: Arc<dyn Mailer> = Arc::new(StdoutMailer);
    let srv = web::test::server(move || {
        let admin_state = admin_state.clone();
        let cfg_state = cfg_state.clone();
        let db_state = db_state.clone();
        let mailer_state = mailer_state.clone();
        async move {
            web::App::new()
                .state(admin_state)
                .state(cfg_state)
                .state(db_state)
                .state(mailer_state)
                .configure(server::configure(false, false))
        }
    })
    .await;
    let auth_base = srv.url("").trim_end_matches('/').to_string();

    let http = cyper::Client::new();
    let body = "csrf=missing&logout_challenge=fake";
    let resp = http
        .request(http::Method::POST, &format!("{auth_base}/logout"))
        .expect("build POST /logout")
        .header("content-type", "application/x-www-form-urlencoded")
        .expect("content-type")
        .body(body)
        .send()
        .await
        .expect("send POST /logout");

    let status = resp.status().as_u16();
    assert_ne!(
        status, 404,
        "POST /logout must be registered — pre-fix it returned 404"
    );
    assert_ne!(
        status, 405,
        "POST /logout must accept POST — pre-fix the route was missing entirely"
    );
    // Without a CSRF cookie the handler renders the 400 error page.
    assert_eq!(status, 400, "expected 400 (CSRF rejection), got {status}");
}
