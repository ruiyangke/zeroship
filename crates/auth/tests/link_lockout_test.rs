//! Regression for f6.1 — a locked account must NOT be able to confirm an
//! account link via `/link`, even when it supplies the CORRECT password.
//!
//! The bug this guards against: the L5 account-lockout / eligibility gate
//! is applied on `/login`, magic, and oauth, but a missed sibling could
//! leave `/link` un-gated. If `/link` skipped the lockout check, a locked
//! account holder (or an attacker who has triggered lockout but knows the
//! password) could still link a federated identity and mint a fresh session
//! (302 → hydra redirect_to). The eligibility gate must reject the locked
//! account BEFORE any session/identity is created.
//!
//! Rule-A faithfulness: we seed a user with the CORRECT password and then
//! lock it. We drive the REAL `/link` POST end to end (CSRF → token decode →
//! hydra get_login → password verify → eligibility). With the lockout gate
//! present the request is rejected; without it the correct password would
//! produce a 302 success. We do NOT pre-seed any "rejected" state — the
//! production code path itself must do the rejecting.

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
use zeroship_auth::store::users;

const CORRECT_PASSWORD: &str = "correct link password phrase";

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
            "client_id": "link-lockout-client",
            "client_name": "Link Lockout Client",
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
        "request_url": "https://auth.zeroship.ai/oauth2/auth?client_id=link-lockout-client",
        "requested_scope": ["openid"],
        "requested_access_token_audience": [],
        "session_id": null,
        "oidc_context": null
    }))
}

// If the link POST ever reached `accept_login` for a locked account, it would
// hit this and produce a 302 success — exactly what the lockout gate must
// prevent. Its presence makes a regression unambiguous.
#[allow(clippy::future_not_send)]
async fn mock_accept_login() -> web::HttpResponse {
    web::HttpResponse::Ok().json(&json!({
        "redirect_to": "https://auth.zeroship.ai/oauth2/auth?login_verifier=accepted"
    }))
}

#[ntex::test]
#[allow(clippy::future_not_send)]
async fn locked_account_cannot_link_with_correct_password() {
    let db_url = match std::env::var("AUTH_DB_URL") {
        Ok(db_url) => db_url,
        Err(_) => {
            eprintln!("skipping link_lockout_test (no AUTH_DB_URL)");
            return;
        }
    };
    let (pg_client, pg_connection) = connect(&db_url, NoTls).await.expect("connect");
    compio::runtime::spawn(async move {
        if let Err(e) = pg_connection.run().await {
            eprintln!("[link_lockout_test] pg connection driver: {e}");
        }
    })
    .detach();

    let email = format!("link-lockout-{}@zeroship.test", Uuid::new_v4().simple());
    let phc = password::hash(CORRECT_PASSWORD).expect("hash password");
    let user = users::create(&pg_client, &email, "Link Lockout", Some(&phc))
        .await
        .expect("seed user");

    // Lock the account 1 hour into the future — the SAME state the L5
    // lockout flow lands a user in after repeated failed logins. This is
    // production-shaped account state, not a pre-seeded "rejected" verdict.
    pg_client
        .execute(
            "UPDATE zeroship.users SET locked_until = NOW() + INTERVAL '1 hour' WHERE id = $1",
            &[&user.id],
        )
        .await
        .expect("lock user");

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
        web::App::new()
            .service(
                web::resource("/admin/oauth2/auth/requests/login")
                    .route(web::get().to(mock_get_login)),
            )
            .service(
                web::resource("/admin/oauth2/auth/requests/login/accept")
                    .route(web::put().to(mock_accept_login)),
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
    let csrf = read_set_cookie(get_resp.headers(), "zsidp_csrf").expect("csrf cookie");

    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("csrf", &csrf)
        .append_pair("token", &token)
        .append_pair("password", CORRECT_PASSWORD)
        .finish();
    let post_req = test::TestRequest::post()
        .uri("/link")
        .header("content-type", "application/x-www-form-urlencoded")
        .header("cookie", format!("zsidp_csrf={csrf}"))
        .set_payload(body)
        .to_request();
    let post_resp = test::call_service(&app, post_req).await;
    let status = post_resp.status().as_u16();

    // A locked account MUST NOT get a 302 (link success / session mint),
    // even with the correct password.
    assert_ne!(
        status, 302,
        "locked account was allowed to link with correct password (lockout gate missing)"
    );
    assert_eq!(
        status, 401,
        "locked account should be rejected at /link with 401"
    );

    let session_cookie = read_set_cookie(post_resp.headers(), "zsidp_session");
    assert!(
        session_cookie.is_none(),
        "no session cookie may be minted for a locked account"
    );

    // The identity row must NOT have been created for a locked account.
    let linked = pg
        .query(
            "SELECT 1 FROM zeroship.federated_identities WHERE user_id = $1",
            &[&user.id],
        )
        .await
        .expect("query identities");
    assert!(
        linked.is_empty(),
        "locked account must not have a federated_identities row created"
    );

    // Cleanup.
    pg.execute(
        "DELETE FROM zeroship.federated_identities WHERE user_id = $1",
        &[&user.id],
    )
    .await
    .ok();
    pg.execute(
        "DELETE FROM zeroship.audit_events WHERE actor_user_id = $1",
        &[&user.id],
    )
    .await
    .ok();
    pg.execute("DELETE FROM zeroship.users WHERE id = $1", &[&user.id])
        .await
        .ok();
}
