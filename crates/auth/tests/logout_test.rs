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
use std::sync::Mutex;

use ntex::web::{self, test};
use serde::Deserialize;
use uuid::Uuid;

use zeroship_auth::csrf;
use zeroship_auth::hydra_client::HydraAdmin;
use zeroship_auth::mailer::{Mailer, StdoutMailer};
use zeroship_auth::server;
use zeroship_auth::sessions::login as session_cookie;
use zeroship_auth::store::{migrations, sessions, users};

mod common;
use common::test_auth_config;

#[derive(Debug, Clone)]
struct MockLogoutState {
    subject: String,
    hydra_sid: String,
    redirect_to: String,
    accepted: Arc<Mutex<Vec<String>>>,
}

#[derive(Debug, Deserialize)]
struct LogoutChallengeQuery {
    logout_challenge: String,
}

#[allow(clippy::future_not_send)]
async fn mock_get_logout(
    query: web::types::Query<LogoutChallengeQuery>,
    state: web::types::State<MockLogoutState>,
) -> web::HttpResponse {
    web::HttpResponse::Ok().json(&serde_json::json!({
        "subject": state.subject,
        "sid": state.hydra_sid,
        "request_url": format!("https://hydra.example/logout?logout_challenge={}", query.logout_challenge),
        "rp_initiated": true,
        "client": null,
    }))
}

#[allow(clippy::future_not_send)]
async fn mock_accept_logout(
    query: web::types::Query<LogoutChallengeQuery>,
    state: web::types::State<MockLogoutState>,
) -> web::HttpResponse {
    state
        .accepted
        .lock()
        .expect("lock accepted challenges")
        .push(query.logout_challenge.clone());
    web::HttpResponse::Ok().json(&serde_json::json!({
        "redirect_to": state.redirect_to,
    }))
}

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

#[compio::test]
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
    migrations::migrate(&pg_client).await.expect("migrate");
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
            idle_minutes: session_cookie::IDLE_MINUTES,
            absolute_hours: session_cookie::ABSOLUTE_HOURS,
        },
    )
    .await
    .expect("seed local session");
    let pg = Arc::new(pg_client);

    let accepted = Arc::new(Mutex::new(Vec::new()));
    let redirect_to = "https://rp.example/logout-done".to_string();
    let hydra_state = MockLogoutState {
        subject: user.id.to_string(),
        hydra_sid: Uuid::new_v4().to_string(),
        redirect_to: redirect_to.clone(),
        accepted: accepted.clone(),
    };
    let hydra_srv = web::test::server(move || {
        let hydra_state = hydra_state.clone();
        async move {
            web::App::new()
                .state(hydra_state)
                .service(
                    web::resource("/admin/oauth2/auth/requests/logout")
                        .route(web::get().to(mock_get_logout)),
                )
                .service(
                    web::resource("/admin/oauth2/auth/requests/logout/accept")
                        .route(web::put().to(mock_accept_logout)),
                )
        }
    })
    .await;

    let admin = HydraAdmin::new(hydra_srv.url("").trim_end_matches('/').to_string());
    let cfg = Arc::new(test_auth_config(
        &db_url,
        hydra_srv.url("").trim_end_matches('/'),
        "http://127.0.0.1:4444",
    ));
    let app = test::init_service(
        web::App::new()
            .state(admin)
            .state(cfg.clone())
            .state(pg.clone())
            .service(
                web::resource("/logout")
                    .route(web::post().to(zeroship_auth::ui::logout::post)),
            ),
    )
    .await;

    let csrf = csrf::generate_token();
    let challenge = format!("logout-{}", Uuid::new_v4().simple());
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("csrf", &csrf)
        .append_pair("logout_challenge", &challenge)
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

    let revoked: bool = pg
        .query_one(
            "SELECT revoked_at IS NOT NULL AS revoked FROM auth.sessions WHERE id = $1",
            &[&session.id],
        )
        .await
        .expect("load local session")
        .get("revoked");
    assert!(revoked, "logout must revoke the local session cookie id");
    assert_eq!(
        accepted.lock().expect("lock accepted challenges").as_slice(),
        &[challenge],
        "logout should still accept the Hydra logout challenge"
    );

    pg.execute("DELETE FROM auth.audit_events WHERE user_id = $1", &[&user.id])
        .await
        .ok();
    pg.execute("DELETE FROM auth.sessions WHERE id = $1", &[&session.id])
        .await
        .ok();
    pg.execute("DELETE FROM auth.users WHERE id = $1", &[&user.id])
        .await
        .ok();
}
