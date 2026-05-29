//! OAuth 2.0 Device Authorization Grant UI regression tests.
//!
//! Live Hydra checks skip unless `AUTH_DB_URL` and `HYDRA_ADMIN_URL` are set.

mod common;

use std::sync::Arc;

use ntex::web;
use uuid::Uuid;

use common::{assert_redirect, location, test_auth_config};
use zeroship_auth::headers::SecurityHeaders;
use zeroship_auth::hydra_client::types::OAuth2Client;
use zeroship_auth::hydra_client::HydraAdmin;
use zeroship_auth::sessions::login as session_cookie;
use zeroship_auth::server;
use zeroship_auth::store::{migrations, sessions as session_store, users};

#[derive(Debug, serde::Deserialize)]
struct DeviceAuthorizationResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    #[serde(default)]
    verification_uri_complete: Option<String>,
    interval: i64,
    expires_in: i64,
}

#[allow(clippy::future_not_send)]
async fn boot() -> Option<(
    ntex::web::test::TestServer,
    String,
    HydraAdmin,
    String,
    Arc<compio_postgres::Client>,
)> {
    let (Ok(db_url), Ok(hydra_admin_url)) = (
        std::env::var("AUTH_DB_URL"),
        std::env::var("HYDRA_ADMIN_URL"),
    ) else {
        eprintln!("[device_grant] skip (need AUTH_DB_URL + HYDRA_ADMIN_URL)");
        return None;
    };
    let hydra_public = std::env::var("HYDRA_PUBLIC_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:4444".to_string());

    let (pg_client, pg_connection) =
        compio_postgres::connect(&db_url, compio_postgres::NoTls)
            .await
            .expect("connect pg");
    compio::runtime::spawn(async move {
        if let Err(e) = pg_connection.run().await {
            eprintln!("[device_grant] pg connection driver: {e}");
        }
    })
    .detach();
    migrations::migrate(&pg_client).await.expect("migrate");
    let pg = Arc::new(pg_client);

    let admin = HydraAdmin::new(&hydra_admin_url);
    let cfg = Arc::new(test_auth_config(&db_url, &hydra_admin_url, &hydra_public));
    let admin_state = admin.clone();
    let cfg_state = cfg.clone();
    let pg_state = pg.clone();
    let srv = web::test::server(move || {
        let admin_state = admin_state.clone();
        let cfg_state = cfg_state.clone();
        let pg_state = pg_state.clone();
        async move {
            web::App::new()
                .state(admin_state)
                .state(cfg_state)
                .state(pg_state)
                .middleware(SecurityHeaders)
                .configure(server::configure(false, false))
        }
    })
    .await;
    let auth_base = srv.url("").trim_end_matches('/').to_string();

    Some((srv, auth_base, admin, hydra_public, pg))
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn device_route_renders_and_rejects_bad_input() {
    let Some((srv, auth_base, _admin, _hydra_public, _pg)) = boot().await else {
        return;
    };
    let http = cyper::Client::new();

    let resp = http
        .request(http::Method::GET, format!("{auth_base}/device"))
        .expect("build GET /device")
        .send()
        .await
        .expect("send GET /device");
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.expect("GET /device body");
    assert!(
        body.contains("Enter the code shown on your device"),
        "form heading missing: {body}"
    );

    let resp = http
        .request(http::Method::POST, format!("{auth_base}/device"))
        .expect("build POST /device empty")
        .header("content-type", "application/x-www-form-urlencoded")
        .expect("content-type")
        .body("user_code=")
        .send()
        .await
        .expect("send POST /device empty");
    assert_eq!(resp.status().as_u16(), 400);

    let resp = http
        .request(http::Method::POST, format!("{auth_base}/device"))
        .expect("build POST /device bogus")
        .header("content-type", "application/x-www-form-urlencoded")
        .expect("content-type")
        .body("user_code=BOGUS-CODE")
        .send()
        .await
        .expect("send POST /device bogus");
    assert_eq!(resp.status().as_u16(), 400);

    drop(srv);
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn device_user_code_redirects_anonymous_browser_to_login() {
    let Some((srv, auth_base, admin, hydra_public, pg)) = boot().await else {
        return;
    };
    let http = cyper::Client::new();
    let client_id = format!("zeroship-cli-test-{}", Uuid::new_v4().simple());

    admin
        .create_client(&OAuth2Client {
            client_id: client_id.clone(),
            client_name: Some("zeroship CLI test".into()),
            client_secret: None,
            grant_types: vec![
                "urn:ietf:params:oauth:grant-type:device_code".into(),
                "refresh_token".into(),
            ],
            response_types: vec![],
            redirect_uris: vec![],
            post_logout_redirect_uris: vec![],
            scope: "openid offline_access apps:deploy apps:read".into(),
            token_endpoint_auth_method: "none".into(),
            subject_type: "public".into(),
            access_token_strategy: None,
            id_token_signed_response_alg: Some("EdDSA".into()),
            audience: vec![],
            skip_consent: true,
            require_consent: false,
            require_logout_consent: false,
            frontchannel_logout_uri: None,
            backchannel_logout_uri: None,
        })
        .await
        .expect("create device client");

    let device_body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("client_id", &client_id)
        .append_pair("scope", "openid offline_access apps:deploy apps:read")
        .finish();
    let resp = http
        .request(http::Method::POST, format!("{hydra_public}/oauth2/device/auth"))
        .expect("build POST /oauth2/device/auth")
        .header("content-type", "application/x-www-form-urlencoded")
        .expect("content-type")
        .body(device_body)
        .send()
        .await
        .expect("send POST /oauth2/device/auth");
    let status = resp.status().as_u16();
    let body = resp.text().await.expect("device auth body");
    if status == 404 || status == 405 {
        let _ = admin.delete_client(&client_id).await;
        drop(srv);
        panic!("hydra device authorization endpoint is not enabled: {status} {body}");
    }
    assert!(
        (200..300).contains(&status),
        "device authorization failed: {status} {body}"
    );
    let authz: DeviceAuthorizationResponse = serde_json::from_str(&body)
        .unwrap_or_else(|e| panic!("decode device response: {e}: {body}"));
    assert!(!authz.device_code.is_empty());
    assert!(!authz.user_code.is_empty());
    assert!(!authz.verification_uri.is_empty());
    if let Some(complete_uri) = &authz.verification_uri_complete {
        assert!(!complete_uri.is_empty());
    }
    assert!(authz.interval > 0);
    assert!(authz.expires_in > 0);

    let post_body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("user_code", &authz.user_code)
        .finish();
    let resp = http
        .request(http::Method::POST, format!("{auth_base}/device"))
        .expect("build POST /device")
        .header("content-type", "application/x-www-form-urlencoded")
        .expect("content-type")
        .body(post_body)
        .send()
        .await
        .expect("send POST /device");
    assert_redirect(&resp, "POST /device valid user_code");
    assert_eq!(location(&resp), "/login");

    let email = format!("device-grant-{client_id}@zeroship.test");
    let user = users::create(&pg, &email, "Device Grant User", None)
        .await
        .expect("create device grant user");
    let session = session_store::create(
        &pg,
        &session_store::CreateSession {
            user_id: user.id,
            auth_method: "password",
            amr: vec!["pwd".into()],
            acr: None,
            idle_minutes: session_cookie::IDLE_MINUTES,
            absolute_hours: session_cookie::ABSOLUTE_HOURS,
        },
    )
    .await
    .expect("create local auth session");
    let post_body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("user_code", &authz.user_code)
        .finish();
    let resp = http
        .request(http::Method::POST, format!("{auth_base}/device"))
        .expect("build signed-in POST /device")
        .header("content-type", "application/x-www-form-urlencoded")
        .expect("content-type")
        .header("cookie", session_cookie::set_cookie(&session.id, false))
        .expect("cookie")
        .body(post_body)
        .send()
        .await
        .expect("send signed-in POST /device");
    assert_redirect(&resp, "POST /device signed-in valid user_code");
    assert_ne!(location(&resp), "/login");

    let rows = pg
        .query(
            "SELECT COUNT(*)::BIGINT AS n \
             FROM auth.audit_events \
             WHERE user_id = $1 AND event_type = 'device_grant'",
            &[&user.id],
        )
        .await
        .expect("count device grant audit rows");
    assert_eq!(rows[0].get::<_, i64>("n"), 1);

    let _ = admin.delete_client(&client_id).await;
    let _ = pg
        .execute("DELETE FROM auth.sessions WHERE id = $1", &[&session.id])
        .await;
    let _ = pg
        .execute("DELETE FROM auth.users WHERE id = $1", &[&user.id])
        .await;
    drop(srv);
}
