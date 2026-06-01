use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::Parser;
use compio_postgres::{connect, NoTls};
use ntex::http::header::SET_COOKIE;
use ntex::web::{self, test};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use zeroship_auth::config::AuthConfig;
use zeroship_auth::hydra_client::HydraAdmin;
use zeroship_auth::identity::linker::{PendingLink, PENDING_LINK_TTL_SECS};
use zeroship_auth::identity::password;
use zeroship_auth::store::{users};

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

#[derive(Debug, Deserialize)]
struct LoginChallengeQuery {
    login_challenge: String,
}

#[allow(clippy::future_not_send)]
async fn mock_get_login(query: web::types::Query<LoginChallengeQuery>) -> web::HttpResponse {
    web::HttpResponse::Ok().json(&json!({
        "challenge": query.login_challenge,
        "skip": false,
        "subject": "",
        "client": {
            "client_id": "link-limit-client",
            "client_name": "Link Limit Client",
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
        "request_url": "https://auth.zeroship.ai/oauth2/auth?client_id=link-limit-client",
        "requested_scope": ["openid"],
        "requested_access_token_audience": [],
        "session_id": null,
        "oidc_context": null
    }))
}

// Uses `web::test::server` to stand up a mock hydra; that requires the
// ntex runtime/System, so this must run under `#[ntex::test]` (not
// `#[compio::test]`, which has no System — the server start panics with
// "System is not running"). `compio_postgres` and `compio::runtime` still
// work under the ntex runtime (it is compio-backed in this stack).
#[ntex::test]
#[allow(clippy::future_not_send)]
async fn link_wrong_password_is_limited_by_fifth_attempt() {
    let db_url = match std::env::var("AUTH_DB_URL") {
        Ok(db_url) => db_url,
        Err(_) => {
            eprintln!("skipping link_ratelimit_test (no AUTH_DB_URL)");
            return;
        }
    };
    let (pg_client, pg_connection) = connect(&db_url, NoTls).await.expect("connect");
    compio::runtime::spawn(async move {
        if let Err(e) = pg_connection.run().await {
            eprintln!("[link_ratelimit_test] pg connection driver: {e}");
        }
    })
    .detach();

    let email = format!("link-limit-{}@zeroship.test", Uuid::new_v4().simple());
    let phc = password::hash("correct link password phrase").expect("hash password");
    let user = users::create(&pg_client, &email, "Link Limit", Some(&phc))
        .await
        .expect("seed user");
    let pg = Arc::new(pg_client);
    let cfg = Arc::new(test_cfg(&db_url));
    let pending = PendingLink {
        user_id: user.id,
        provider: "github".into(),
        subject: format!("github-{}", Uuid::new_v4().simple()),
        email: email.clone(),
        login_challenge: format!("challenge-{}", Uuid::new_v4().simple()),
        exp_unix: i64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_secs()
                + PENDING_LINK_TTL_SECS,
        )
        .expect("exp fits i64"),
    };
    let token = pending.encode(cfg.stash_signing_key.as_bytes());

    let hydra_srv = web::test::server(|| async {
        web::App::new().service(
            web::resource("/admin/oauth2/auth/requests/login")
                .route(web::get().to(mock_get_login)),
        )
    })
    .await;
    let admin = HydraAdmin::new(hydra_srv.url("").trim_end_matches('/').to_string());

    let app = test::init_service(
        web::App::new()
            .state(cfg.clone())
            .state(pg.clone())
            .state(admin)
            .service(
                web::resource("/link")
                    .route(web::get().to(zeroship_auth::ui::link::get))
                    .route(web::post().to(zeroship_auth::ui::link::post)),
            ),
    )
    .await;

    let get_req = test::TestRequest::get()
        .uri(&format!("/link?token={token}"))
        .to_request();
    let get_resp = test::call_service(&app, get_req).await;
    assert_eq!(get_resp.status().as_u16(), 200);
    let mut csrf = read_set_cookie(get_resp.headers(), "zsidp_csrf").expect("csrf cookie");

    for attempt in 1..=6 {
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("csrf", &csrf)
            .append_pair("token", &token)
            .append_pair("password", &format!("wrong password phrase {attempt}"))
            .finish();
        let post_req = test::TestRequest::post()
            .uri("/link")
            .header("content-type", "application/x-www-form-urlencoded")
            .header("cookie", format!("zsidp_csrf={csrf}"))
            .set_payload(body)
            .to_request();
        let post_resp = test::call_service(&app, post_req).await;
        let status = post_resp.status().as_u16();
        let next_csrf = read_set_cookie(post_resp.headers(), "zsidp_csrf").unwrap_or(csrf);
        let response_body =
            String::from_utf8(test::read_body(post_resp).await.to_vec()).expect("utf8 body");

        if attempt < 5 {
            assert_eq!(status, 401, "attempt {attempt} should still verify the password");
            assert!(
                response_body.contains("invalid password"),
                "attempt {attempt} should render invalid password"
            );
        } else {
            assert_eq!(status, 429, "attempt {attempt} should be rate limited");
            assert!(
                response_body.contains("too many attempts"),
                "attempt {attempt} should render rate-limit copy"
            );
        }
        csrf = next_csrf;
    }

    let like = format!("link_attempt:{}:%", user.id);
    pg.execute("DELETE FROM zeroship.rate_limits WHERE bucket_key LIKE $1", &[&like])
        .await
        .ok();
    pg.execute("DELETE FROM zeroship.audit_events WHERE actor_user_id = $1", &[&user.id])
        .await
        .ok();
    pg.execute("DELETE FROM zeroship.users WHERE id = $1", &[&user.id])
        .await
        .ok();
}
