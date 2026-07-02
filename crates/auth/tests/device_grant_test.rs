//! OAuth 2.0 Device Authorization Grant UI regression tests.

mod common;

use std::sync::Arc;

use ed25519_dalek::SigningKey;
use ntex::web;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use common::{assert_redirect, cleanup_rate_limits_like, location, test_auth_config};
use zeroship_auth::headers::SecurityHeaders;
use zeroship_auth::oidc::{Issuer, ACCESS_TOKEN_TYP};
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

#[derive(Debug, serde::Deserialize)]
struct DeviceTokenResponse {
    access_token: String,
    token_type: String,
    expires_in: u64,
    scope: String,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
}

#[allow(clippy::future_not_send)]
async fn boot_native() -> Option<(
    ntex::web::test::TestServer,
    String,
    Arc<compio_postgres::Client>,
    Arc<Issuer>,
)> {
    let db_url = std::env::var("AUTH_DB_URL")
        .or_else(|_| std::env::var("CONTROL_TEST_DB"))
        .ok()?;
    let (pg_client, pg_connection) =
        compio_postgres::connect(&db_url, compio_postgres::NoTls)
            .await
            .expect("connect pg");
    compio::runtime::spawn(async move {
        if let Err(e) = pg_connection.run().await {
            eprintln!("[device_grant_native] pg connection driver: {e}");
        }
    })
    .detach();
    let pg = Arc::new(pg_client);

    let cfg = Arc::new(test_auth_config(&db_url));
    let issuer = Arc::new(test_issuer());
    issuer
        .publish_active_key(&pg)
        .await
        .expect("publish active OP key");

    let cfg_state = cfg.clone();
    let pg_state = pg.clone();
    let issuer_state = issuer.clone();
    let refresh_pool_state =
        zeroship_auth::oidc::refresh::RefreshSessionPool::new(db_url.clone(), 4);
    let srv = web::test::server(move || {
        let cfg_state = cfg_state.clone();
        let pg_state = pg_state.clone();
        let issuer_state = issuer_state.clone();
        let refresh_pool_state = refresh_pool_state.clone();
        async move {
            web::App::new()
                .state(cfg_state)
                .state(pg_state)
                .state(issuer_state)
                .state(refresh_pool_state)
                .middleware(SecurityHeaders::default())
                .configure(server::configure(false, false))
        }
    })
    .await;
    let auth_base = srv.url("").trim_end_matches('/').to_string();

    Some((srv, auth_base, pg, issuer))
}

fn test_issuer() -> Issuer {
    let signing = SigningKey::from_bytes(&[51u8; 32]);
    Issuer::from_signing_key(&signing, [13u8; 32], "https://auth.zeroship.test/oauth2".into())
        .expect("issuer")
}

fn sha256_hex(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}

fn unique_test_client_ip() -> String {
    let id = Uuid::new_v4();
    let bytes = id.as_bytes();
    let ip = format!("10.{}.{}.{}", bytes[0], bytes[1], bytes[2]);
    ip.parse::<std::net::IpAddr>()
        .expect("generated test client IP must parse");
    ip
}

fn assert_user_code_format(user_code: &str) {
    const USER_CODE_ALPHABET: &[u8] = b"BCDFGHJKLMNPQRSTVWXZ";

    assert_eq!(
        user_code.len(),
        14,
        "device user_code must use the new 4-4-4 format"
    );
    for (idx, ch) in user_code.bytes().enumerate() {
        match idx {
            4 | 9 => assert_eq!(ch, b'-', "dash separator at byte {idx}: {user_code}"),
            _ => assert!(
                USER_CODE_ALPHABET.contains(&ch),
                "unexpected user_code character {:?} at byte {idx}: {user_code}",
                char::from(ch)
            ),
        }
    }
    let entropy_bits = 12.0_f64 * 20.0_f64.log2();
    assert!(
        entropy_bits > 40.0,
        "12 chars from the 20-symbol alphabet must exceed 40 bits"
    );
}

async fn insert_native_device_client(
    pg: &compio_postgres::Client,
    client_id: &str,
    client_name: &str,
    scopes: &[&str],
) {
    let scope_values: Vec<String> = scopes.iter().map(|scope| (*scope).to_string()).collect();
    let empty_redirects: Vec<String> = Vec::new();
    pg.execute(
        "INSERT INTO zeroship.oauth_clients \
            (client_id, client_name, redirect_uris, scopes, skip_consent, \
             refresh_allowed, token_endpoint_auth_method) \
         VALUES ($1, $2, $3, $4, TRUE, FALSE, 'none')",
        &[&client_id, &client_name, &empty_redirects, &scope_values],
    )
    .await
    .expect("insert native device client");
}

async fn request_device_authorization(
    http: &cyper::Client,
    auth_base: &str,
    client_id: &str,
    scope: &str,
) -> DeviceAuthorizationResponse {
    request_device_authorization_with_scope(http, auth_base, client_id, Some(scope)).await
}

async fn request_device_authorization_with_scope(
    http: &cyper::Client,
    auth_base: &str,
    client_id: &str,
    scope: Option<&str>,
) -> DeviceAuthorizationResponse {
    let mut form = url::form_urlencoded::Serializer::new(String::new());
    form.append_pair("client_id", client_id);
    if let Some(scope) = scope {
        form.append_pair("scope", scope);
    }
    let resp = http
        .request(
            http::Method::POST,
            format!("{auth_base}/oauth2/device/authorization"),
        )
        .expect("build OP device authorization")
        .header("content-type", "application/x-www-form-urlencoded")
        .expect("content-type")
        .body(form.finish())
        .send()
        .await
        .expect("send OP device authorization");
    let status = resp.status().as_u16();
    let body = resp.text().await.expect("OP device authorization body");
    assert!(
        (200..300).contains(&status),
        "device authorization failed: {status} {body}"
    );
    serde_json::from_str(&body)
        .unwrap_or_else(|e| panic!("decode OP device response: {e}: {body}"))
}

/// GET `/device` and pull the freshly-minted CSRF token out of the
/// `Set-Cookie: zsidp_csrf=…` header. The fixture runs `--dev-insecure`, so the
/// cookie name has no `__Host-` prefix (`csrf::COOKIE_NAME_DEV`). The same token
/// is what the rendered form embeds in its hidden `csrf` field, so a faithful
/// double-submit POST presents this value in BOTH the cookie header and the
/// form body.
#[allow(clippy::future_not_send)]
async fn fetch_csrf_token(http: &cyper::Client, auth_base: &str) -> String {
    fetch_device_page(http, auth_base, "/device").await.0
}

#[allow(clippy::future_not_send)]
async fn fetch_device_page(http: &cyper::Client, auth_base: &str, path: &str) -> (String, String) {
    let resp = http
        .request(http::Method::GET, format!("{auth_base}{path}"))
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
    let csrf = raw
        .split(';')
        .next()
        .and_then(|kv| kv.trim().strip_prefix("zsidp_csrf="))
        .map(str::to_string)
        .unwrap_or_else(|| panic!("GET /device must set a zsidp_csrf cookie; got: {raw:?}"));
    let body = resp.text().await.expect("GET /device body");
    (csrf, body)
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn device_route_renders_and_rejects_bad_input() {
    let Some((srv, auth_base, _pg, _issuer)) = boot_native().await else {
        eprintln!("[device_grant] skip (need AUTH_DB_URL or CONTROL_TEST_DB)");
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

    // An over-length code is rejected by the handler's format check with 400
    // before lookup. A well-formed unknown code reaches the pending-code lookup
    // instead; this assertion pins the format-level rejection.
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
async fn device_authorization_user_code_uses_high_entropy_format() {
    let Some((srv, auth_base, pg, _issuer)) = boot_native().await else {
        eprintln!("[device_grant_entropy] skip (need AUTH_DB_URL or CONTROL_TEST_DB)");
        return;
    };
    let http = cyper::Client::new();
    let client_id = format!("zeroship-cli-entropy-{}", Uuid::new_v4().simple());
    insert_native_device_client(&pg, &client_id, "native device entropy test", &["openid"]).await;

    let authz = request_device_authorization(&http, &auth_base, &client_id, "openid").await;
    assert_user_code_format(&authz.user_code);

    let _ = pg
        .execute(
            "DELETE FROM zeroship.device_grants WHERE user_code = $1",
            &[&authz.user_code],
        )
        .await;
    let _ = pg
        .execute("DELETE FROM zeroship.oauth_clients WHERE client_id = $1", &[&client_id])
        .await;
    drop(srv);
}

/// Regression for M1: failed `/device` user-code guesses must not be
/// unbounded. Pre-fix, every wrong code below returned ordinary 400 form
/// errors forever. Post-fix, the existing `zeroship.rate_limits` token bucket
/// rejects the 6th failed signed-in guess for the same `(user, trusted IP)`,
/// while the real code is still accepted because successful approvals do not
/// consume the failed-attempt bucket.
#[ntex::test]
#[allow(clippy::future_not_send)]
async fn device_post_rate_limits_failed_user_code_guesses_but_allows_correct_code() {
    let Some((srv, auth_base, pg, _issuer)) = boot_native().await else {
        eprintln!("[device_grant_ratelimit] skip (need AUTH_DB_URL or CONTROL_TEST_DB)");
        return;
    };
    let http = cyper::Client::new();
    let client_id = format!("zeroship-cli-ratelimit-{}", Uuid::new_v4().simple());
    insert_native_device_client(
        &pg,
        &client_id,
        "native device rate-limit test",
        &["openid", "apps:read"],
    )
    .await;
    let authz = request_device_authorization(&http, &auth_base, &client_id, "openid").await;

    let email = format!("device-ratelimit-{client_id}@zeroship.test");
    let user = users::create(&pg, &email, "Device Rate Limit User", None)
        .await
        .expect("create device rate-limit user");
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
    .expect("create device rate-limit session");

    let xff_ip = unique_test_client_ip();
    let user_ip_key = format!("device:user_ip:{}:{xff_ip}", user.id);
    let ip_key = format!("device:ip:{xff_ip}");
    cleanup_rate_limits_like(&pg, &[&user_ip_key, &ip_key]).await;

    let wrong_code = if authz.user_code == "BCDF-GHJK-LMNP" {
        "BCDF-GHJK-LMNQ"
    } else {
        "BCDF-GHJK-LMNP"
    };
    assert_user_code_format(wrong_code);

    let mut last_status = 0_u16;
    for attempt in 1..=6 {
        let csrf = fetch_csrf_token(&http, &auth_base).await;
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("user_code", wrong_code)
            .append_pair("confirm", "authorize")
            .append_pair("csrf", &csrf)
            .finish();
        let resp = http
            .request(http::Method::POST, format!("{auth_base}/device"))
            .expect("build wrong-code POST /device")
            .header("content-type", "application/x-www-form-urlencoded")
            .expect("content-type")
            .header(
                "cookie",
                format!(
                    "{}; zsidp_csrf={csrf}",
                    session_cookie::set_cookie(&session.id, true)
                ),
            )
            .expect("cookie")
            .header("x-forwarded-for", format!("198.51.100.1, {xff_ip}"))
            .expect("xff")
            .body(body)
            .send()
            .await
            .expect("send wrong-code POST /device");
        last_status = resp.status().as_u16();
        if attempt <= 5 {
            assert_eq!(
                last_status, 400,
                "wrong user_code attempt {attempt} should fail normally before the bucket is empty"
            );
        }
    }
    assert_eq!(
        last_status, 429,
        "6th wrong user_code attempt for the same signed-in user and trusted IP must be throttled"
    );

    let csrf = fetch_csrf_token(&http, &auth_base).await;
    let correct_body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("user_code", &authz.user_code)
        .append_pair("confirm", "authorize")
        .append_pair("csrf", &csrf)
        .finish();
    let approve = http
        .request(http::Method::POST, format!("{auth_base}/device"))
        .expect("build correct-code POST /device")
        .header("content-type", "application/x-www-form-urlencoded")
        .expect("content-type")
        .header(
            "cookie",
            format!(
                "{}; zsidp_csrf={csrf}",
                session_cookie::set_cookie(&session.id, true)
            ),
        )
        .expect("cookie")
        .header("x-forwarded-for", format!("198.51.100.1, {xff_ip}"))
        .expect("xff")
        .body(correct_body)
        .send()
        .await
        .expect("send correct-code POST /device");
    assert_eq!(
        approve.status().as_u16(),
        200,
        "a correct user_code must still approve even after failed guesses are throttled"
    );
    let body = approve.text().await.expect("correct-code body");
    assert!(body.contains("Device approved"), "{body}");

    cleanup_rate_limits_like(&pg, &[&user_ip_key, &ip_key]).await;
    let _ = pg
        .execute("DELETE FROM zeroship.idp_sessions WHERE id = $1", &[&session.id])
        .await;
    let _ = pg
        .execute("DELETE FROM zeroship.users WHERE id = $1", &[&user.id])
        .await;
    let _ = pg
        .execute("DELETE FROM zeroship.oauth_clients WHERE client_id = $1", &[&client_id])
        .await;
    drop(srv);
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn device_authorization_omitted_scope_defaults_to_openid_only() {
    let Some((srv, auth_base, pg, _issuer)) = boot_native().await else {
        eprintln!("[device_grant_scope] skip (need AUTH_DB_URL or CONTROL_TEST_DB)");
        return;
    };
    let http = cyper::Client::new();
    let client_id = format!("zeroship-cli-scope-{}", Uuid::new_v4().simple());
    insert_native_device_client(
        &pg,
        &client_id,
        "native device scope test",
        &["openid", "email", "profile", "apps:read"],
    )
    .await;

    let authz =
        request_device_authorization_with_scope(&http, &auth_base, &client_id, None).await;
    let device_code_hash = sha256_hex(&authz.device_code);
    let row = pg
        .query_one(
            "SELECT scope FROM zeroship.device_grants WHERE device_code_hash = $1",
            &[&device_code_hash],
        )
        .await
        .expect("stored device grant scope");
    assert_eq!(
        row.get::<_, String>("scope"),
        "openid",
        "pre-fix omitted scope copied the client's full allowlist into the grant"
    );

    let _ = pg
        .execute(
            "DELETE FROM zeroship.device_grants WHERE device_code_hash = $1",
            &[&device_code_hash],
        )
        .await;
    let _ = pg
        .execute("DELETE FROM zeroship.oauth_clients WHERE client_id = $1", &[&client_id])
        .await;
    drop(srv);
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn native_device_confirmation_shows_client_scopes_and_requires_confirm() {
    let Some((srv, auth_base, pg, _issuer)) = boot_native().await else {
        eprintln!("[device_grant_confirm] skip (need AUTH_DB_URL or CONTROL_TEST_DB)");
        return;
    };
    let http = cyper::Client::new();
    let client_id = format!("zeroship-cli-confirm-{}", Uuid::new_v4().simple());
    let client_name = "zeroship CLI confirm test";
    insert_native_device_client(&pg, &client_id, client_name, &["openid", "apps:read"]).await;
    let authz =
        request_device_authorization(&http, &auth_base, &client_id, "openid apps:read").await;

    let (csrf_token, page) =
        fetch_device_page(&http, &auth_base, &format!("/device?user_code={}", authz.user_code))
            .await;
    assert!(page.contains(client_name), "{page}");
    assert!(page.contains(&client_id), "{page}");
    assert!(page.contains("Verify your identity"), "{page}");
    assert!(page.contains("apps:read"), "{page}");
    assert!(page.contains(r#"name="confirm" value="authorize""#), "{page}");

    let email = format!("device-confirm-{client_id}@zeroship.test");
    let user = users::create(&pg, &email, "Device Confirm User", None)
        .await
        .expect("create device confirm user");
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

    let no_confirm = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("user_code", &authz.user_code)
        .append_pair("csrf", &csrf_token)
        .finish();
    let resp = http
        .request(http::Method::POST, format!("{auth_base}/device"))
        .expect("build no-confirm POST /device")
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
        .body(no_confirm)
        .send()
        .await
        .expect("send no-confirm POST /device");
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.expect("no-confirm body");
    assert!(body.contains(client_name), "{body}");

    let rows = pg
        .query_one(
            "SELECT status FROM zeroship.device_grants WHERE user_code = $1",
            &[&authz.user_code],
        )
        .await
        .expect("device grant still pending");
    assert_eq!(
        rows.get::<_, String>("status"),
        "pending",
        "pre-fix a code-only POST approved immediately without an explicit confirmation"
    );

    let confirm = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("user_code", &authz.user_code)
        .append_pair("confirm", "authorize")
        .append_pair("csrf", &csrf_token)
        .finish();
    let resp = http
        .request(http::Method::POST, format!("{auth_base}/device"))
        .expect("build confirm POST /device")
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
        .body(confirm)
        .send()
        .await
        .expect("send confirm POST /device");
    assert_eq!(resp.status().as_u16(), 200);
    assert!(resp.text().await.expect("confirm body").contains("Device approved"));

    let _ = pg
        .execute("DELETE FROM zeroship.idp_sessions WHERE id = $1", &[&session.id])
        .await;
    let _ = pg
        .execute("DELETE FROM zeroship.users WHERE id = $1", &[&user.id])
        .await;
    let _ = pg
        .execute("DELETE FROM zeroship.oauth_clients WHERE client_id = $1", &[&client_id])
        .await;
    drop(srv);
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn native_device_grant_approves_via_auth_session_and_polls_op_token() {
    let Some((srv, auth_base, pg, issuer)) = boot_native().await else {
        eprintln!("[device_grant_native] skip (need AUTH_DB_URL or CONTROL_TEST_DB)");
        return;
    };
    let http = cyper::Client::new();
    let client_id = format!("zeroship-cli-native-{}", Uuid::new_v4().simple());
    insert_native_device_client(&pg, &client_id, "native device test", &["apps:read"]).await;

    let device_body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("client_id", &client_id)
        .append_pair("scope", "apps:read")
        .finish();
    let resp = http
        .request(
            http::Method::POST,
            format!("{auth_base}/oauth2/device/authorization"),
        )
        .expect("build OP device authorization")
        .header("content-type", "application/x-www-form-urlencoded")
        .expect("content-type")
        .body(device_body)
        .send()
        .await
        .expect("send OP device authorization");
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.expect("OP device authorization body");
    let authz: DeviceAuthorizationResponse =
        serde_json::from_str(&body).unwrap_or_else(|e| panic!("decode OP device response: {e}: {body}"));
    assert!(authz.verification_uri.ends_with("/device"));
    assert!(
        authz
            .verification_uri_complete
            .as_deref()
            .is_some_and(|uri| uri.contains(&authz.user_code)),
        "verification_uri_complete should carry the user_code: {authz:?}"
    );

    let pending_body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair(
            "grant_type",
            "urn:ietf:params:oauth:grant-type:device_code",
        )
        .append_pair("device_code", &authz.device_code)
        .append_pair("client_id", &client_id)
        .finish();
    let pending = http
        .request(http::Method::POST, format!("{auth_base}/oauth2/token"))
        .expect("build pending OP token poll")
        .header("content-type", "application/x-www-form-urlencoded")
        .expect("content-type")
        .body(pending_body)
        .send()
        .await
        .expect("send pending OP token poll");
    assert_eq!(pending.status().as_u16(), 400);
    let pending_json: serde_json::Value =
        serde_json::from_str(&pending.text().await.expect("pending body"))
            .expect("pending body json");
    assert_eq!(pending_json["error"], "authorization_pending");

    let email = format!("native-device-{client_id}@zeroship.test");
    let user = users::create(&pg, &email, "Native Device User", None)
        .await
        .expect("create native device user");
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
    .expect("create native device auth session");

    let csrf_token = fetch_csrf_token(&http, &auth_base).await;
    let approve_body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("user_code", &authz.user_code)
        .append_pair("confirm", "authorize")
        .append_pair("csrf", &csrf_token)
        .finish();
    let approve = http
        .request(http::Method::POST, format!("{auth_base}/device"))
        .expect("build native /device approve")
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
        .body(approve_body)
        .send()
        .await
        .expect("send native /device approve");
    assert_eq!(
        approve.status().as_u16(),
        200,
        "native device approval must render success"
    );
    assert!(
        approve.headers().get(http::header::LOCATION).is_none(),
        "native device approval must not redirect"
    );
    let approve_body = approve.text().await.expect("approve body");
    assert!(approve_body.contains("Device approved"), "{approve_body}");

    let device_code_hash = sha256_hex(&authz.device_code);
    pg.execute(
        "UPDATE zeroship.device_grants \
         SET last_polled_at = NOW() - INTERVAL '6 seconds' \
         WHERE device_code_hash = $1",
        &[&device_code_hash],
    )
    .await
    .expect("age native device poll timestamp");

    let token_body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair(
            "grant_type",
            "urn:ietf:params:oauth:grant-type:device_code",
        )
        .append_pair("device_code", &authz.device_code)
        .append_pair("client_id", &client_id)
        .finish();
    let token_resp = http
        .request(http::Method::POST, format!("{auth_base}/oauth2/token"))
        .expect("build approved OP token poll")
        .header("content-type", "application/x-www-form-urlencoded")
        .expect("content-type")
        .body(token_body)
        .send()
        .await
        .expect("send approved OP token poll");
    assert_eq!(token_resp.status().as_u16(), 200);
    let token_body = token_resp.text().await.expect("approved token body");
    let token: DeviceTokenResponse = serde_json::from_str(&token_body)
        .unwrap_or_else(|e| panic!("decode OP token response: {e}: {token_body}"));
    assert_eq!(token.token_type, "Bearer");
    assert_eq!(token.scope, "apps:read");
    assert!(token.expires_in > 0);
    assert!(token.id_token.is_none(), "device grant v1 must not mint nonce-less id_token");
    assert!(token.refresh_token.is_none());
    let claims = issuer
        .verify_access_token(&token.access_token)
        .expect("verify OP access token");
    assert_eq!(claims.client_id, client_id);
    assert_eq!(claims.aud, "zeroship");
    assert_eq!(claims.scope, "apps:read");
    assert_eq!(jsonwebtoken::decode_header(&token.access_token).unwrap().typ.as_deref(), Some(ACCESS_TOKEN_TYP));

    let rows = pg
        .query(
            "SELECT COUNT(*)::BIGINT AS n \
             FROM zeroship.device_grants \
             WHERE device_code_hash = $1",
            &[&device_code_hash],
        )
        .await
        .expect("count native device grants after token poll");
    assert_eq!(rows[0].get::<_, i64>("n"), 0, "device code must be one-use");

    let _ = pg
        .execute("DELETE FROM zeroship.idp_sessions WHERE id = $1", &[&session.id])
        .await;
    let _ = pg
        .execute("DELETE FROM zeroship.users WHERE id = $1", &[&user.id])
        .await;
    let _ = pg
        .execute("DELETE FROM zeroship.oauth_clients WHERE client_id = $1", &[&client_id])
        .await;
    drop(srv);
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn device_user_code_redirects_anonymous_browser_to_login() {
    let Some((srv, auth_base, pg, _issuer)) = boot_native().await else {
        eprintln!("[device_grant] skip (need AUTH_DB_URL or CONTROL_TEST_DB)");
        return;
    };
    let http = cyper::Client::new();
    let client_id = format!("zeroship-cli-test-{}", Uuid::new_v4().simple());

    insert_native_device_client(
        &pg,
        &client_id,
        "zeroship CLI test",
        &["openid", "offline_access", "apps:deploy", "apps:read"],
    )
    .await;
    let authz = request_device_authorization(
        &http,
        &auth_base,
        &client_id,
        "openid offline_access apps:deploy apps:read",
    )
    .await;
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
        .append_pair("confirm", "authorize")
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
        .append_pair("confirm", "authorize")
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
    assert_eq!(resp.status().as_u16(), 200);
    assert!(resp.headers().get(http::header::LOCATION).is_none());

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

    let _ = pg
        .execute("DELETE FROM zeroship.idp_sessions WHERE id = $1", &[&session.id])
        .await;
    let _ = pg
        .execute("DELETE FROM zeroship.users WHERE id = $1", &[&user.id])
        .await;
    let _ = pg
        .execute("DELETE FROM zeroship.oauth_clients WHERE client_id = $1", &[&client_id])
        .await;
    drop(srv);
}

/// Regression for finding M2: `POST /device` (RFC 8628 device confirmation — an
/// identity-conferring, state-changing grant) MUST enforce the same
/// `__Host-zsidp_csrf` double-submit token as every sibling auth form handler.
///
/// Pre-fix the handler had no CSRF check at all, so a signed-in POST carrying a
/// valid `user_code` + session cookie but NO csrf token was accepted and
/// completed the device grant and wrote a `device_grant` audit row — a
/// cross-site request-forgery foothold. Post-fix the same POST is rejected with
/// 403 and writes no grant, while a faithful double-submit POST (matching csrf
/// cookie + field) still succeeds.
#[ntex::test]
#[allow(clippy::future_not_send)]
async fn device_post_requires_csrf_token() {
    let Some((srv, auth_base, pg, _issuer)) = boot_native().await else {
        eprintln!("[device_grant] skip (need AUTH_DB_URL or CONTROL_TEST_DB)");
        return;
    };
    let http = cyper::Client::new();
    let client_id = format!("zeroship-cli-csrf-{}", Uuid::new_v4().simple());

    insert_native_device_client(
        &pg,
        &client_id,
        "zeroship CLI csrf test",
        &["openid", "offline_access", "apps:deploy", "apps:read"],
    )
    .await;
    let authz = request_device_authorization(
        &http,
        &auth_base,
        &client_id,
        "openid offline_access apps:deploy apps:read",
    )
    .await;

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
        .append_pair("confirm", "authorize")
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
    assert_eq!(
        resp.status().as_u16(),
        200,
        "a valid double-submit POST must complete the grant, not bounce to /login"
    );
    assert!(resp.headers().get(http::header::LOCATION).is_none());

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

    let _ = pg
        .execute("DELETE FROM zeroship.idp_sessions WHERE id = $1", &[&session.id])
        .await;
    let _ = pg
        .execute("DELETE FROM zeroship.users WHERE id = $1", &[&user.id])
        .await;
    let _ = pg
        .execute("DELETE FROM zeroship.oauth_clients WHERE client_id = $1", &[&client_id])
        .await;
    drop(srv);
}
