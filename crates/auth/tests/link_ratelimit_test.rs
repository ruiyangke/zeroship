use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::Parser;
use compio_postgres::{connect, NoTls};
use ntex::http::header::SET_COOKIE;
use ntex::web::{self, test};
use uuid::Uuid;

use zeroship_auth::config::AuthConfig;
use zeroship_auth::hydra_client::HydraAdmin;
use zeroship_auth::identity::linker::{PendingLink, PENDING_LINK_TTL_SECS};
use zeroship_auth::identity::password;
use zeroship_auth::store::{migrations, users};

fn test_cfg(db_url: &str) -> AuthConfig {
    AuthConfig::parse_from([
        "zeroship-auth",
        "--db-url",
        db_url,
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

#[compio::test]
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
    migrations::migrate(&pg_client).await.expect("migrate");

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

    let app = test::init_service(
        web::App::new()
            .state(cfg.clone())
            .state(pg.clone())
            .state(HydraAdmin::new("http://127.0.0.1:1"))
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
    pg.execute("DELETE FROM auth.rate_limits WHERE bucket_key LIKE $1", &[&like])
        .await
        .ok();
    pg.execute("DELETE FROM auth.audit_events WHERE user_id = $1", &[&user.id])
        .await
        .ok();
    pg.execute("DELETE FROM auth.users WHERE id = $1", &[&user.id])
        .await
        .ok();
}
