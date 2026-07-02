//! `/logout` regression coverage.
//!
//! Pre-fix (bug #2): `crates/auth/src/server.rs` did not register a
//! handler for `/logout`, so any GET / POST returned 404.
//!
//! Post-fix: GET `/logout` is wired to the native confirmation form and POST
//! `/logout` handles local session revocation. Either way: NOT 404.

use std::sync::Arc;

use ed25519_dalek::SigningKey;
use ntex::web::{self, test};
use uuid::Uuid;

use zeroship_auth::csrf;
use zeroship_auth::oidc::Issuer;
use zeroship_auth::server;
use zeroship_auth::sessions::login as session_cookie;
use zeroship_auth::store::{sessions, users};
use zeroship_mailer::{Mailer, StdoutMailer};

mod common;
use common::test_auth_config;

#[ntex::test]
async fn logout_route_is_registered_returns_not_404() {
    let Ok(db_url) = std::env::var("AUTH_DB_URL") else {
        eprintln!("[logout_test] skip (need AUTH_DB_URL)");
        return;
    };

    let (pg_client, pg_connection) = compio_postgres::connect(&db_url, compio_postgres::NoTls)
        .await
        .expect("connect pg");
    compio::runtime::spawn(async move {
        if let Err(e) = pg_connection.run().await {
            eprintln!("[logout_test] pg connection driver: {e}");
        }
    })
    .detach();
    let pg = Arc::new(pg_client);

    let cfg = Arc::new(test_auth_config(&db_url));
    let issuer = Arc::new(
        Issuer::from_signing_key(
            &SigningKey::from_bytes(&[44u8; 32]),
            [3u8; 32],
            "https://auth.zeroship.test/oauth2".to_string(),
        )
        .expect("issuer"),
    );
    let cfg_state = cfg.clone();
    let db_state = pg.clone();
    let issuer_state = issuer.clone();
    let mailer_state: Arc<dyn Mailer> = Arc::new(StdoutMailer);
    let refresh_pool_state =
        zeroship_auth::oidc::refresh::RefreshSessionPool::new(db_url.clone(), 4);
    let srv = web::test::server(move || {
        let cfg_state = cfg_state.clone();
        let db_state = db_state.clone();
        let issuer_state = issuer_state.clone();
        let mailer_state = mailer_state.clone();
        let refresh_pool_state = refresh_pool_state.clone();
        async move {
            web::App::new()
                .state(cfg_state)
                .state(db_state)
                .state(issuer_state)
                .state(mailer_state)
                .state(refresh_pool_state)
                .configure(server::configure(false, false))
        }
    })
    .await;
    let auth_base = srv.url("").trim_end_matches('/').to_string();

    // Drive `/logout`. The handler is registered, so the response is the
    // confirmation form. Pre-fix the route was unmapped -> ntex returned 404.
    // The regression-failing assertion is on the absence of 404.
    let http = cyper::Client::new();
    let resp = http
        .request(http::Method::GET, &format!("{auth_base}/logout"))
        .expect("build GET /logout")
        .send()
        .await
        .expect("send GET /logout");

    let status = resp.status().as_u16();
    assert_ne!(
        status, 404,
        "/logout MUST be registered — pre-fix it returned 404"
    );

    assert!(
        status == 200 || status == 400,
        "expected 200 (form) or 400 (error page); got {status}"
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
    let Ok(db_url) = std::env::var("AUTH_DB_URL") else {
        eprintln!("[logout_test] skip (need AUTH_DB_URL)");
        return;
    };

    let (pg_client, pg_connection) = compio_postgres::connect(&db_url, compio_postgres::NoTls)
        .await
        .expect("connect pg");
    compio::runtime::spawn(async move {
        if let Err(e) = pg_connection.run().await {
            eprintln!("[logout_test] pg connection driver: {e}");
        }
    })
    .detach();
    let pg = Arc::new(pg_client);

    let cfg = Arc::new(test_auth_config(&db_url));
    let issuer = Arc::new(
        Issuer::from_signing_key(
            &SigningKey::from_bytes(&[44u8; 32]),
            [3u8; 32],
            "https://auth.zeroship.test/oauth2".to_string(),
        )
        .expect("issuer"),
    );
    let cfg_state = cfg.clone();
    let db_state = pg.clone();
    let issuer_state = issuer.clone();
    let mailer_state: Arc<dyn Mailer> = Arc::new(StdoutMailer);
    let refresh_pool_state =
        zeroship_auth::oidc::refresh::RefreshSessionPool::new(db_url.clone(), 4);
    let srv = web::test::server(move || {
        let cfg_state = cfg_state.clone();
        let db_state = db_state.clone();
        let issuer_state = issuer_state.clone();
        let mailer_state = mailer_state.clone();
        let refresh_pool_state = refresh_pool_state.clone();
        async move {
            web::App::new()
                .state(cfg_state)
                .state(db_state)
                .state(issuer_state)
                .state(mailer_state)
                .state(refresh_pool_state)
                .configure(server::configure(false, false))
        }
    })
    .await;
    let auth_base = srv.url("").trim_end_matches('/').to_string();

    let http = cyper::Client::new();
    let body = "csrf=missing";
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

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn logout_post_revokes_local_session_cookie() {
    let db_url = match std::env::var("AUTH_DB_URL") {
        Ok(db_url) => db_url,
        Err(_) => {
            eprintln!("[logout_test] skip (need AUTH_DB_URL)");
            return;
        }
    };

    let (pg_client, pg_connection) = compio_postgres::connect(&db_url, compio_postgres::NoTls)
        .await
        .expect("connect pg");
    compio::runtime::spawn(async move {
        if let Err(e) = pg_connection.run().await {
            eprintln!("[logout_test] pg connection driver: {e}");
        }
    })
    .detach();
    let email = format!("logout-local-{}@zeroship.test", Uuid::new_v4().simple());
    let user = users::create(&pg_client, &email, "Logout User", None)
        .await
        .expect("seed user");
    let session = sessions::create(
        &pg_client,
        &sessions::CreateSession {
            user_id: user.id,
            auth_method: "password",
            amr: vec!["pwd".to_string()],
            acr: None,
            expected_credential_version: Some(user.credential_version),
            idle_minutes: session_cookie::IDLE_MINUTES,
            absolute_hours: session_cookie::ABSOLUTE_HOURS,
        },
    )
    .await
    .expect("seed local session");
    let pg = Arc::new(pg_client);

    let cfg = Arc::new(test_auth_config(&db_url));
    let issuer = Arc::new(
        Issuer::from_signing_key(
            &SigningKey::from_bytes(&[44u8; 32]),
            [3u8; 32],
            "https://auth.zeroship.test/oauth2".to_string(),
        )
        .expect("issuer"),
    );
    let app = test::init_service(
        web::App::new()
            .state(cfg.clone())
            .state(pg.clone())
            .state(issuer)
            .service(
                web::resource("/logout")
                    .route(web::post().to(zeroship_auth::ui::logout::post)),
            ),
    )
    .await;

    let csrf = csrf::generate_token();
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("csrf", &csrf)
        .finish();
    let req = test::TestRequest::post()
        .uri("/logout")
        .header("content-type", "application/x-www-form-urlencoded")
        .header(
            "cookie",
            format!("zsidp_csrf={csrf}; zsidp_session={}", session.id),
        )
        .set_payload(body)
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 302);
    let location = resp
        .headers()
        .get(ntex::http::header::LOCATION)
        .and_then(|h| h.to_str().ok());
    assert_eq!(location, Some("/login"));

    let revoked: bool = pg
        .query_one(
            "SELECT revoked_at IS NOT NULL AS revoked FROM zeroship.idp_sessions WHERE id = $1",
            &[&session.id],
        )
        .await
        .expect("load local session")
        .get("revoked");
    assert!(revoked, "logout must revoke the local session cookie id");

    pg.execute("DELETE FROM zeroship.audit_events WHERE actor_user_id = $1", &[&user.id])
        .await
        .ok();
    pg.execute("DELETE FROM zeroship.idp_sessions WHERE id = $1", &[&session.id])
        .await
        .ok();
    pg.execute("DELETE FROM zeroship.users WHERE id = $1", &[&user.id])
        .await
        .ok();
}
