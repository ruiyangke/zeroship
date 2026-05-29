use std::sync::{Arc, Mutex};

use clap::Parser;
use compio_postgres::{connect, NoTls};
use ntex::http::header::SET_COOKIE;
use ntex::web::{self, test};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use zeroship_auth::config::AuthConfig;
use zeroship_auth::hydra_client::HydraAdmin;
use zeroship_auth::identity::password;
use zeroship_auth::store::{migrations, users};

const CHALLENGE: &str = "skip-disabled-challenge";
const ACCEPT_REDIRECT: &str = "https://client.example/callback?code=accepted";
const REJECT_REDIRECT: &str = "https://client.example/callback?error=access_denied";

fn test_cfg(db_url: &str, hydra_admin: &str) -> AuthConfig {
    AuthConfig::parse_from([
        "zeroship-auth",
        "--db-url",
        db_url,
        "--hydra-admin-url",
        hydra_admin,
        "--hydra-public-url",
        hydra_admin,
        "--insecure-dev",
        "--stash-signing-key",
        "test-stash-key-not-for-prod-32bytes!",
    ])
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

#[derive(Debug)]
struct MockHydraState {
    login_request: Value,
    accept_records: Vec<Value>,
    reject_records: Vec<Value>,
}

#[derive(Debug, Deserialize)]
struct LoginChallengeQuery {
    login_challenge: String,
}

#[allow(clippy::future_not_send)]
async fn mock_get_login(
    query: web::types::Query<LoginChallengeQuery>,
    state: web::types::State<Arc<Mutex<MockHydraState>>>,
) -> web::HttpResponse {
    if query.login_challenge != CHALLENGE {
        return web::HttpResponse::BadRequest().body("unexpected challenge");
    }
    let request = state.lock().expect("lock hydra state").login_request.clone();
    web::HttpResponse::Ok().json(&request)
}

#[allow(clippy::future_not_send)]
async fn mock_accept_login(
    query: web::types::Query<LoginChallengeQuery>,
    body: web::types::Json<Value>,
    state: web::types::State<Arc<Mutex<MockHydraState>>>,
) -> web::HttpResponse {
    state
        .lock()
        .expect("lock hydra state")
        .accept_records
        .push(json!({ "challenge": query.login_challenge, "body": body.into_inner() }));
    web::HttpResponse::Ok().json(&json!({ "redirect_to": ACCEPT_REDIRECT }))
}

#[allow(clippy::future_not_send)]
async fn mock_reject_login(
    query: web::types::Query<LoginChallengeQuery>,
    body: web::types::Json<Value>,
    state: web::types::State<Arc<Mutex<MockHydraState>>>,
) -> web::HttpResponse {
    state
        .lock()
        .expect("lock hydra state")
        .reject_records
        .push(json!({ "challenge": query.login_challenge, "body": body.into_inner() }));
    web::HttpResponse::Ok().json(&json!({ "redirect_to": REJECT_REDIRECT }))
}

#[compio::test]
#[allow(clippy::future_not_send)]
async fn skip_login_rejects_disabled_user_subject() {
    let db_url = match std::env::var("AUTH_DB_URL") {
        Ok(db_url) => db_url,
        Err(_) => {
            eprintln!("skipping login_skip_test (no AUTH_DB_URL)");
            return;
        }
    };
    let (pg_client, pg_connection) = connect(&db_url, NoTls).await.expect("connect");
    compio::runtime::spawn(async move {
        if let Err(e) = pg_connection.run().await {
            eprintln!("[login_skip_test] pg connection driver: {e}");
        }
    })
    .detach();
    migrations::migrate(&pg_client).await.expect("migrate");
    let email = format!("skip-disabled-{}@zeroship.test", Uuid::new_v4().simple());
    let user = users::create(&pg_client, &email, "Skip Disabled", None)
        .await
        .expect("seed user");
    pg_client
        .execute(
            "UPDATE auth.users SET disabled_at = NOW() WHERE id = $1",
            &[&user.id],
        )
        .await
        .expect("disable user");
    let pg = Arc::new(pg_client);

    let hydra_state = Arc::new(Mutex::new(MockHydraState {
        login_request: login_request(user.id, true),
        accept_records: Vec::new(),
        reject_records: Vec::new(),
    }));
    let hydra_state_for_srv = hydra_state.clone();
    let hydra_srv = web::test::server(move || {
        let hydra_state = hydra_state_for_srv.clone();
        async move {
            web::App::new()
                .state(hydra_state)
                .service(
                    web::resource("/admin/oauth2/auth/requests/login")
                        .route(web::get().to(mock_get_login)),
                )
                .service(
                    web::resource("/admin/oauth2/auth/requests/login/accept")
                        .route(web::put().to(mock_accept_login)),
                )
                .service(
                    web::resource("/admin/oauth2/auth/requests/login/reject")
                        .route(web::put().to(mock_reject_login)),
                )
        }
    })
    .await;
    let hydra_admin = hydra_srv.url("").trim_end_matches('/').to_string();
    let admin = HydraAdmin::new(hydra_admin.clone());
    let cfg = Arc::new(test_cfg(&db_url, &hydra_admin));

    let app = test::init_service(
        web::App::new()
            .state(admin)
            .state(cfg)
            .state(pg.clone())
            .service(web::resource("/login").route(web::get().to(zeroship_auth::ui::login::get))),
    )
    .await;

    let req = test::TestRequest::get()
        .uri(&format!("/login?login_challenge={CHALLENGE}"))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 302);

    let state = hydra_state.lock().expect("lock hydra state");
    assert!(
        state.accept_records.is_empty(),
        "skip-login must not accept disabled subjects"
    );
    assert_eq!(state.reject_records.len(), 1, "disabled subject must be rejected");
    assert_eq!(
        state.reject_records[0]["body"]["error"],
        "access_denied",
        "reject_login should receive an access_denied error"
    );
    drop(state);

    pg.execute("DELETE FROM auth.users WHERE id = $1", &[&user.id])
        .await
        .ok();
}

#[compio::test]
#[allow(clippy::future_not_send)]
async fn password_login_rejects_locked_user_without_session() {
    let db_url = match std::env::var("AUTH_DB_URL") {
        Ok(db_url) => db_url,
        Err(_) => {
            eprintln!("skipping login_skip_test (no AUTH_DB_URL)");
            return;
        }
    };
    let (pg_client, pg_connection) = connect(&db_url, NoTls).await.expect("connect");
    compio::runtime::spawn(async move {
        if let Err(e) = pg_connection.run().await {
            eprintln!("[login_skip_test] pg connection driver: {e}");
        }
    })
    .detach();
    migrations::migrate(&pg_client).await.expect("migrate");
    let email = format!("pwd-locked-{}@zeroship.test", Uuid::new_v4().simple());
    let phc = password::hash("correct locked password phrase").expect("hash password");
    let user = users::create(&pg_client, &email, "Password Locked", Some(&phc))
        .await
        .expect("seed user");
    pg_client
        .execute(
            "UPDATE auth.users SET locked_until = NOW() + INTERVAL '1 hour' WHERE id = $1",
            &[&user.id],
        )
        .await
        .expect("lock user");
    let pg = Arc::new(pg_client);

    let hydra_state = Arc::new(Mutex::new(MockHydraState {
        login_request: login_request(user.id, false),
        accept_records: Vec::new(),
        reject_records: Vec::new(),
    }));
    let hydra_state_for_srv = hydra_state.clone();
    let hydra_srv = web::test::server(move || {
        let hydra_state = hydra_state_for_srv.clone();
        async move {
            web::App::new()
                .state(hydra_state)
                .service(
                    web::resource("/admin/oauth2/auth/requests/login")
                        .route(web::get().to(mock_get_login)),
                )
                .service(
                    web::resource("/admin/oauth2/auth/requests/login/accept")
                        .route(web::put().to(mock_accept_login)),
                )
                .service(
                    web::resource("/admin/oauth2/auth/requests/login/reject")
                        .route(web::put().to(mock_reject_login)),
                )
        }
    })
    .await;
    let hydra_admin = hydra_srv.url("").trim_end_matches('/').to_string();
    let admin = HydraAdmin::new(hydra_admin.clone());
    let cfg = Arc::new(test_cfg(&db_url, &hydra_admin));

    let app = test::init_service(
        web::App::new()
            .state(admin)
            .state(cfg)
            .state(pg.clone())
            .service(
                web::resource("/login")
                    .route(web::get().to(zeroship_auth::ui::login::get))
                    .route(web::post().to(zeroship_auth::ui::login::post)),
            ),
    )
    .await;

    let get_req = test::TestRequest::get()
        .uri(&format!("/login?login_challenge={CHALLENGE}"))
        .to_request();
    let get_resp = test::call_service(&app, get_req).await;
    assert_eq!(get_resp.status().as_u16(), 200);
    let csrf = read_set_cookie(get_resp.headers(), "zsidp_csrf").expect("csrf cookie");

    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("csrf", &csrf)
        .append_pair("email", &email)
        .append_pair("password", "correct locked password phrase")
        .finish();
    let post_req = test::TestRequest::post()
        .uri(&format!("/login?login_challenge={CHALLENGE}"))
        .header("content-type", "application/x-www-form-urlencoded")
        .header("cookie", format!("zsidp_csrf={csrf}"))
        .set_payload(body)
        .to_request();
    let post_resp = test::call_service(&app, post_req).await;
    assert_eq!(post_resp.status().as_u16(), 403);
    let body = String::from_utf8(test::read_body(post_resp).await.to_vec()).expect("utf8 body");
    assert!(
        body.contains("account temporarily locked"),
        "locked password login must render the generic locked error; body={body}"
    );

    let state = hydra_state.lock().expect("lock hydra state");
    assert!(
        state.accept_records.is_empty(),
        "locked password login must not accept the hydra challenge"
    );
    drop(state);

    let session_count: i64 = pg
        .query_one(
            "SELECT COUNT(*) FROM auth.sessions WHERE user_id = $1",
            &[&user.id],
        )
        .await
        .expect("count sessions")
        .get(0);
    assert_eq!(session_count, 0, "locked password login must not create a session");

    pg.execute("DELETE FROM auth.audit_events WHERE user_id = $1", &[&user.id])
        .await
        .ok();
    pg.execute("DELETE FROM auth.users WHERE id = $1", &[&user.id])
        .await
        .ok();
}

fn login_request(subject: Uuid, skip: bool) -> Value {
    json!({
        "challenge": CHALLENGE,
        "skip": skip,
        "subject": subject.to_string(),
        "client": {
            "client_id": "skip-test-client",
            "client_name": "Skip Test Client",
            "grant_types": ["authorization_code"],
            "response_types": ["code"],
            "redirect_uris": ["https://client.example/callback"],
            "post_logout_redirect_uris": [],
            "scope": "openid",
            "token_endpoint_auth_method": "client_secret_basic",
            "subject_type": "public",
            "audience": [],
            "skip_consent": true,
            "require_consent": false,
            "require_logout_consent": false
        },
        "request_url": "https://auth.zeroship.ai/oauth2/auth?client_id=skip-test-client",
        "requested_scope": ["openid"],
        "requested_access_token_audience": [],
        "session_id": "hydra-session",
        "oidc_context": null
    })
}
