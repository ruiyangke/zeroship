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
use zeroship_auth::store::{sessions as session_store, users};

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
    let pg = Arc::new(pg_client);

    let admin = HydraAdmin::new(&hydra_admin_url);
    let cfg = Arc::new(test_auth_config(&db_url, &hydra_admin_url, &hydra_public));
    let admin_state = admin.clone();
    let cfg_state = cfg.clone();
    let pg_state = pg.clone();
    let refresh_pool_state =
        zeroship_auth::op::refresh::RefreshSessionPool::new(db_url.clone(), 4);
    let srv = web::test::server(move || {
        let admin_state = admin_state.clone();
        let cfg_state = cfg_state.clone();
        let pg_state = pg_state.clone();
        let refresh_pool_state = refresh_pool_state.clone();
        async move {
            web::App::new()
                .state(admin_state)
                .state(cfg_state)
                .state(pg_state)
                .state(refresh_pool_state)
                .middleware(SecurityHeaders::default())
                .configure(server::configure(false, false))
        }
    })
    .await;
    let auth_base = srv.url("").trim_end_matches('/').to_string();

    Some((srv, auth_base, admin, hydra_public, pg))
}

/// GET `/device` and pull the freshly-minted CSRF token out of the
/// `Set-Cookie: zsidp_csrf=…` header. The fixture runs `--dev-insecure`, so the
/// cookie name has no `__Host-` prefix (`csrf::COOKIE_NAME_DEV`). The same token
/// is what the rendered form embeds in its hidden `csrf` field, so a faithful
/// double-submit POST presents this value in BOTH the cookie header and the
/// form body.
#[allow(clippy::future_not_send)]
async fn fetch_csrf_token(http: &cyper::Client, auth_base: &str) -> String {
    let resp = http
        .request(http::Method::GET, format!("{auth_base}/device"))
        .expect("build GET /device for csrf")
        .send()
        .await
        .expect("send GET /device for csrf");
    let raw = resp
        .headers()
        .get("set-cookie")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("")
        .to_string();
    // `zsidp_csrf=<token>; Path=/; SameSite=Strict; Max-Age=3600`
    raw.split(';')
        .next()
        .and_then(|kv| kv.trim().strip_prefix("zsidp_csrf="))
        .map(str::to_string)
        .unwrap_or_else(|| panic!("GET /device must set a zsidp_csrf cookie; got: {raw:?}"))
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

    // CSRF is enforced BEFORE input validation (fail-closed): a POST with no
    // csrf token is rejected with 403 regardless of the body.
    let resp = http
        .request(http::Method::POST, format!("{auth_base}/device"))
        .expect("build POST /device no-csrf")
        .header("content-type", "application/x-www-form-urlencoded")
        .expect("content-type")
        .body("user_code=ABCD-EFGH")
        .send()
        .await
        .expect("send POST /device no-csrf");
    assert_eq!(
        resp.status().as_u16(),
        403,
        "a POST /device without a csrf token must be rejected with 403"
    );

    // With a valid csrf token, the input-validation checks are reachable.
    let csrf_token = fetch_csrf_token(&http, &auth_base).await;
    let csrf_cookie = format!("zsidp_csrf={csrf_token}");

    let empty_body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("user_code", "")
        .append_pair("csrf", &csrf_token)
        .finish();
    let resp = http
        .request(http::Method::POST, format!("{auth_base}/device"))
        .expect("build POST /device empty")
        .header("content-type", "application/x-www-form-urlencoded")
        .expect("content-type")
        .header("cookie", csrf_cookie.clone())
        .expect("cookie")
        .body(empty_body)
        .send()
        .await
        .expect("send POST /device empty");
    assert_eq!(resp.status().as_u16(), 400);

    // An over-length code is rejected by the handler's format check
    // (`valid_user_code`, MAX_USER_CODE_BYTES) with 400 BEFORE any Hydra
    // round-trip. Note: a *well-formed* but unknown code (e.g. "BOGUS-CODE")
    // can NOT be rejected here — Hydra's `/oauth2/device/verify` issues a
    // device_challenge for any syntactically-acceptable code and only
    // validates it at `accept_device_user_code` (which requires a signed-in
    // session). So an anonymous browser submitting a well-formed unknown code
    // is correctly sent to /login (302), exercised by the next test. Here we
    // assert the format-level 400 rejection.
    let overlong = "A".repeat(64);
    let overlong_body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("user_code", &overlong)
        .append_pair("csrf", &csrf_token)
        .finish();
    let resp = http
        .request(http::Method::POST, format!("{auth_base}/device"))
        .expect("build POST /device overlong")
        .header("content-type", "application/x-www-form-urlencoded")
        .expect("content-type")
        .header("cookie", csrf_cookie)
        .expect("cookie")
        .body(overlong_body)
        .send()
        .await
        .expect("send POST /device overlong");
    assert_eq!(
        resp.status().as_u16(),
        400,
        "an over-length user_code must be rejected at the format check"
    );

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

    // Anonymous (no session) POST with a VALID csrf token: passes the CSRF gate
    // then bounces to /login because there's no signed-in session.
    let anon_csrf = fetch_csrf_token(&http, &auth_base).await;
    let post_body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("user_code", &authz.user_code)
        .append_pair("csrf", &anon_csrf)
        .finish();
    let resp = http
        .request(http::Method::POST, format!("{auth_base}/device"))
        .expect("build POST /device")
        .header("content-type", "application/x-www-form-urlencoded")
        .expect("content-type")
        .header("cookie", format!("zsidp_csrf={anon_csrf}"))
        .expect("cookie")
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
            expected_credential_version: Some(user.credential_version),
            idle_minutes: session_cookie::IDLE_MINUTES,
            absolute_hours: session_cookie::ABSOLUTE_HOURS,
        },
    )
    .await
    .expect("create local auth session");
    // Mint a CSRF token (GET /device sets the cookie + renders the same value
    // into the hidden field). The double-submit POST must echo it in BOTH the
    // cookie header and the form body, exactly like a real browser submission.
    let csrf_token = fetch_csrf_token(&http, &auth_base).await;
    let post_body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("user_code", &authz.user_code)
        .append_pair("csrf", &csrf_token)
        .finish();
    let resp = http
        .request(http::Method::POST, format!("{auth_base}/device"))
        .expect("build signed-in POST /device")
        .header("content-type", "application/x-www-form-urlencoded")
        .expect("content-type")
        // The boot() config runs with `--dev-insecure`, so the handler's
        // `parse_cookie` looks for the dev cookie name (`zsidp_session`).
        // The cookie we present must use the SAME mode (insecure_dev=true),
        // otherwise it is named `__Host-zsidp_session` and the handler can't
        // find it — `current_session` returns None and we (wrongly) redirect
        // to /login. Pass `true` to match the dev config.
        .header(
            "cookie",
            format!(
                "{}; zsidp_csrf={csrf_token}",
                session_cookie::set_cookie(&session.id, true)
            ),
        )
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
             FROM zeroship.audit_events \
             WHERE actor_user_id = $1 AND event_type = 'device_grant'",
            &[&user.id],
        )
        .await
        .expect("count device grant audit rows");
    assert_eq!(rows[0].get::<_, i64>("n"), 1);

    let _ = admin.delete_client(&client_id).await;
    let _ = pg
        .execute("DELETE FROM zeroship.idp_sessions WHERE id = $1", &[&session.id])
        .await;
    let _ = pg
        .execute("DELETE FROM zeroship.users WHERE id = $1", &[&user.id])
        .await;
    drop(srv);
}

/// Regression for finding M2: `POST /device` (RFC 8628 device confirmation — an
/// identity-conferring, state-changing grant) MUST enforce the same
/// `__Host-zsidp_csrf` double-submit token as every sibling auth form handler.
///
/// Pre-fix the handler had no CSRF check at all, so a signed-in POST carrying a
/// valid `user_code` + session cookie but NO csrf token was accepted and
/// redirected to Hydra (302, away from `/login`) and wrote a `device_grant`
/// audit row — a cross-site request-forgery foothold. Post-fix the same POST is
/// rejected with 403 and writes no grant, while a faithful double-submit POST
/// (matching csrf cookie + field) still succeeds.
#[ntex::test]
#[allow(clippy::future_not_send)]
async fn device_post_requires_csrf_token() {
    let Some((srv, auth_base, admin, hydra_public, pg)) = boot().await else {
        return;
    };
    let http = cyper::Client::new();
    let client_id = format!("zeroship-cli-csrf-{}", Uuid::new_v4().simple());

    admin
        .create_client(&OAuth2Client {
            client_id: client_id.clone(),
            client_name: Some("zeroship CLI csrf test".into()),
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
    assert!(
        (200..300).contains(&status),
        "device authorization failed: {status} {body}"
    );
    let authz: DeviceAuthorizationResponse =
        serde_json::from_str(&body).unwrap_or_else(|e| panic!("decode device response: {e}: {body}"));

    let email = format!("device-csrf-{client_id}@zeroship.test");
    let user = users::create(&pg, &email, "Device CSRF User", None)
        .await
        .expect("create device csrf user");
    let session = session_store::create(
        &pg,
        &session_store::CreateSession {
            user_id: user.id,
            auth_method: "password",
            amr: vec!["pwd".into()],
            acr: None,
            expected_credential_version: Some(user.credential_version),
            idle_minutes: session_cookie::IDLE_MINUTES,
            absolute_hours: session_cookie::ABSOLUTE_HOURS,
        },
    )
    .await
    .expect("create local auth session");

    // ── Attack: signed-in POST with a valid user_code but NO csrf token. This
    //    is the forged cross-site submission. It MUST be rejected (403) and must
    //    NOT bind the device or write a device_grant audit row.
    let no_csrf_body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("user_code", &authz.user_code)
        .finish();
    let resp = http
        .request(http::Method::POST, format!("{auth_base}/device"))
        .expect("build no-csrf POST /device")
        .header("content-type", "application/x-www-form-urlencoded")
        .expect("content-type")
        // Session cookie present (the victim is signed in) but no csrf cookie.
        .header("cookie", session_cookie::set_cookie(&session.id, true))
        .expect("cookie")
        .body(no_csrf_body)
        .send()
        .await
        .expect("send no-csrf POST /device");
    assert_eq!(
        resp.status().as_u16(),
        403,
        "a signed-in POST /device WITHOUT a csrf token must be rejected with 403 \
         (pre-fix it was accepted → CSRF), got {}",
        resp.status().as_u16()
    );

    let rows = pg
        .query(
            "SELECT COUNT(*)::BIGINT AS n \
             FROM zeroship.audit_events \
             WHERE actor_user_id = $1 AND event_type = 'device_grant'",
            &[&user.id],
        )
        .await
        .expect("count device grant audit rows after no-csrf attempt");
    assert_eq!(
        rows[0].get::<_, i64>("n"),
        0,
        "the rejected no-csrf POST must NOT have bound the device grant"
    );

    // ── Control: the SAME signed-in POST with a matching csrf cookie + field
    //    still completes the device grant (legitimate access is preserved).
    let csrf_token = fetch_csrf_token(&http, &auth_base).await;
    let csrf_body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("user_code", &authz.user_code)
        .append_pair("csrf", &csrf_token)
        .finish();
    let resp = http
        .request(http::Method::POST, format!("{auth_base}/device"))
        .expect("build csrf POST /device")
        .header("content-type", "application/x-www-form-urlencoded")
        .expect("content-type")
        .header(
            "cookie",
            format!(
                "{}; zsidp_csrf={csrf_token}",
                session_cookie::set_cookie(&session.id, true)
            ),
        )
        .expect("cookie")
        .body(csrf_body)
        .send()
        .await
        .expect("send csrf POST /device");
    assert_redirect(&resp, "POST /device signed-in with matching csrf");
    assert_ne!(
        location(&resp),
        "/login",
        "a valid double-submit POST must complete the grant, not bounce to /login"
    );

    let rows = pg
        .query(
            "SELECT COUNT(*)::BIGINT AS n \
             FROM zeroship.audit_events \
             WHERE actor_user_id = $1 AND event_type = 'device_grant'",
            &[&user.id],
        )
        .await
        .expect("count device grant audit rows after csrf attempt");
    assert_eq!(
        rows[0].get::<_, i64>("n"),
        1,
        "the valid double-submit POST must bind exactly one device grant"
    );

    let _ = admin.delete_client(&client_id).await;
    let _ = pg
        .execute("DELETE FROM zeroship.idp_sessions WHERE id = $1", &[&session.id])
        .await;
    let _ = pg
        .execute("DELETE FROM zeroship.users WHERE id = $1", &[&user.id])
        .await;
    drop(srv);
}
