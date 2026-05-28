//! Postmark webhook handler — Basic auth verification + suppression handling.
//!
//! Live-PG tests. Each test connects to the `AUTH_DB_URL` PG, runs
//! migrations, spins up an `ntex::web::test::server` that registers JUST
//! the `/webhooks/postmark` route (with config + DB state), then POSTs
//! representative payloads via `cyper::Client`.
//!
//! Skips silently when `AUTH_DB_URL` is unset (CI without PG fixture).
//!
//! Note: every `compio_postgres::connect` returns `(Client, Connection)`
//! and the connection driver MUST be `spawn`+`detach`ed on the compio
//! runtime or the first query hangs forever — mirrors the pattern in
//! `mailer_test.rs` and `migrations_smoke.rs`.

use std::sync::Arc;

use base64::{engine::general_purpose::STANDARD, Engine as _};
use compio_postgres::{connect, NoTls};
use ntex::web;
use serde_json::json;
use uuid::Uuid;
use zeroship_auth::config::AuthConfig;
use zeroship_auth::store::{migrations, suppressions};

/// Build an `AuthConfig` with sensible defaults for webhook tests. Only
/// the `postmark_webhook_*` fields are interesting; everything else is
/// stubbed.
fn test_cfg(user: Option<&str>, pass: Option<&str>) -> AuthConfig {
    AuthConfig {
        addr: "127.0.0.1:0".to_string(),
        db_url: String::new(),
        hydra_admin: "http://127.0.0.1:4445".to_string(),
        hydra_public: "http://127.0.0.1:4444".to_string(),
        clients_config: "ops/auth-clients.example.toml".to_string(),
        bootstrap: false,
        insecure_dev: true,
        google_client_id: None,
        google_client_secret: None,
        google_redirect_uri: "https://auth.zeroship.ai/oauth/google/callback".to_string(),
        google_auth_url: "https://accounts.google.com/o/oauth2/v2/auth".to_string(),
        google_token_url: "https://oauth2.googleapis.com/token".to_string(),
        google_jwks_url: "https://www.googleapis.com/oauth2/v3/certs".to_string(),
        google_issuer: "https://accounts.google.com".to_string(),
        github_client_id: None,
        github_client_secret: None,
        github_redirect_uri: "https://auth.zeroship.ai/oauth/github/callback".to_string(),
        github_authorize_url: "https://github.com/login/oauth/authorize".to_string(),
        github_token_url: "https://github.com/login/oauth/access_token".to_string(),
        github_user_url: "https://api.github.com/user".to_string(),
        github_emails_url: "https://api.github.com/user/emails".to_string(),
        stash_signing_key: "test-stash-key-not-for-prod-32bytes!".to_string(),
        mailer: "stdout".to_string(),
        smtp_host: None,
        smtp_port: 587,
        smtp_username: None,
        smtp_password: None,
        smtp_starttls: true,
        resend_api_key: None,
        mail_from_email: "test@zeroship.test".to_string(),
        mail_from_name: "Test".to_string(),
        public_url: "http://localhost:0".to_string(),
        postmark_webhook_user: user.map(String::from),
        postmark_webhook_password: pass.map(String::from),
    }
}

/// Boot PG + register the `/webhooks/postmark` route. Returns `None` if
/// `AUTH_DB_URL` is unset (test then logs `skip` and exits).
//
// ntex's `TestServer` future is intentionally `!Send` (holds per-worker
// state in `Rc`s); the test helper inherits that.
#[allow(clippy::future_not_send)]
async fn boot(
    user: Option<&str>,
    pass: Option<&str>,
) -> Option<(ntex::web::test::TestServer, Arc<compio_postgres::Client>)> {
    let dsn = std::env::var("AUTH_DB_URL").ok()?;
    let (client, connection) = connect(&dsn, NoTls).await.expect("connect");
    compio::runtime::spawn(async move {
        if let Err(e) = connection.run().await {
            eprintln!("[postmark_webhook_test] pg connection driver: {e}");
        }
    })
    .detach();
    migrations::migrate(&client).await.expect("migrate");
    let pg = Arc::new(client);

    let cfg = Arc::new(test_cfg(user, pass));
    let cfg_state = cfg.clone();
    let pg_state = pg.clone();
    let srv = web::test::server(move || {
        let cfg_state = cfg_state.clone();
        let pg_state = pg_state.clone();
        async move {
            web::App::new()
                .state(cfg_state)
                .state(pg_state)
                .service(
                    web::resource("/webhooks/postmark")
                        .route(web::post().to(zeroship_auth::ui::webhooks::postmark)),
                )
        }
    })
    .await;
    Some((srv, pg))
}

#[ntex::test]
async fn postmark_webhook_handles_hard_bounce() {
    let Some((srv, pg)) = boot(Some("hookuser"), Some("hookpass")).await else {
        eprintln!("skip (no AUTH_DB_URL)");
        return;
    };

    let email = format!("bounce-{}@zeroship.test", Uuid::new_v4().simple());
    let auth = format!("Basic {}", STANDARD.encode("hookuser:hookpass"));
    let body = serde_json::to_vec(&json!({
        "RecordType": "Bounce",
        "Email": email,
        "Type": "HardBounce",
        "Description": "Recipient address rejected: User unknown",
        "ID": 9_999_999_i64,
    }))
    .unwrap();

    let http = cyper::Client::new();
    let resp = http
        .request(http::Method::POST, srv.url("/webhooks/postmark"))
        .expect("build POST")
        .header("authorization", auth.as_str())
        .expect("auth header")
        .header("content-type", "application/json")
        .expect("content-type")
        .body(body)
        .send()
        .await
        .expect("send");
    assert_eq!(
        resp.status().as_u16(),
        200,
        "hard bounce should return 200 OK"
    );

    let suppressed = suppressions::is_suppressed(&pg, &email)
        .await
        .expect("is_suppressed");
    assert!(suppressed, "hard bounce must add the address to suppressions");

    // Cleanup so the test is re-runnable.
    pg.execute(
        "DELETE FROM auth.email_suppressions WHERE email = $1::citext",
        &[&email],
    )
    .await
    .ok();
    drop(srv);
}

#[ntex::test]
async fn postmark_webhook_rejects_bad_auth() {
    let Some((srv, _pg)) = boot(Some("hookuser"), Some("hookpass")).await else {
        eprintln!("skip (no AUTH_DB_URL)");
        return;
    };

    let http = cyper::Client::new();

    // Case 1 — wrong password.
    let bad = format!("Basic {}", STANDARD.encode("hookuser:wrongpass"));
    let resp = http
        .request(http::Method::POST, srv.url("/webhooks/postmark"))
        .expect("build POST")
        .header("authorization", bad.as_str())
        .expect("auth header")
        .header("content-type", "application/json")
        .expect("content-type")
        .body(b"{}".to_vec())
        .send()
        .await
        .expect("send");
    assert_eq!(
        resp.status().as_u16(),
        401,
        "wrong password must yield 401, got {}",
        resp.status()
    );

    // Case 2 — no Authorization header at all.
    let resp = http
        .request(http::Method::POST, srv.url("/webhooks/postmark"))
        .expect("build POST")
        .header("content-type", "application/json")
        .expect("content-type")
        .body(b"{}".to_vec())
        .send()
        .await
        .expect("send");
    assert_eq!(
        resp.status().as_u16(),
        401,
        "missing auth must yield 401, got {}",
        resp.status()
    );

    // Case 3 — credentials not configured server-side (separate boot
    //         with `None`/`None`). Even a correct-looking header must 401
    //         because the server has nothing to compare against.
    let Some((srv_unconfigured, _pg2)) = boot(None, None).await else {
        eprintln!("skip second-stage (no AUTH_DB_URL)");
        return;
    };
    let some_auth = format!("Basic {}", STANDARD.encode("anyone:anything"));
    let resp = http
        .request(
            http::Method::POST,
            srv_unconfigured.url("/webhooks/postmark"),
        )
        .expect("build POST")
        .header("authorization", some_auth.as_str())
        .expect("auth header")
        .header("content-type", "application/json")
        .expect("content-type")
        .body(b"{}".to_vec())
        .send()
        .await
        .expect("send");
    assert_eq!(
        resp.status().as_u16(),
        401,
        "unconfigured server must yield 401 even with auth header, got {}",
        resp.status()
    );

    drop(srv);
    drop(srv_unconfigured);
}

#[ntex::test]
async fn postmark_webhook_handles_spam_complaint() {
    let Some((srv, pg)) = boot(Some("hookuser"), Some("hookpass")).await else {
        eprintln!("skip (no AUTH_DB_URL)");
        return;
    };

    let email = format!("complaint-{}@zeroship.test", Uuid::new_v4().simple());
    let auth = format!("Basic {}", STANDARD.encode("hookuser:hookpass"));
    let body = serde_json::to_vec(&json!({
        "RecordType": "SpamComplaint",
        "Email": email,
        "Description": "User clicked This Is Spam in their mail client",
    }))
    .unwrap();

    let http = cyper::Client::new();
    let resp = http
        .request(http::Method::POST, srv.url("/webhooks/postmark"))
        .expect("build POST")
        .header("authorization", auth.as_str())
        .expect("auth header")
        .header("content-type", "application/json")
        .expect("content-type")
        .body(body)
        .send()
        .await
        .expect("send");
    assert_eq!(
        resp.status().as_u16(),
        200,
        "spam complaint should return 200 OK"
    );

    let suppressed = suppressions::is_suppressed(&pg, &email)
        .await
        .expect("is_suppressed");
    assert!(
        suppressed,
        "spam complaint must add the address to suppressions"
    );

    // Soft-bounce assertion piggybacks on this fixture: a soft bounce
    // for a DIFFERENT address must NOT add a suppression.
    let soft_email = format!("soft-{}@zeroship.test", Uuid::new_v4().simple());
    let soft_body = serde_json::to_vec(&json!({
        "RecordType": "Bounce",
        "Email": soft_email,
        "Type": "SoftBounce",
        "Description": "Mailbox full",
    }))
    .unwrap();
    let resp = http
        .request(http::Method::POST, srv.url("/webhooks/postmark"))
        .expect("build POST")
        .header("authorization", auth.as_str())
        .expect("auth header")
        .header("content-type", "application/json")
        .expect("content-type")
        .body(soft_body)
        .send()
        .await
        .expect("send");
    assert_eq!(
        resp.status().as_u16(),
        200,
        "soft bounce should return 200 OK"
    );
    let suppressed = suppressions::is_suppressed(&pg, &soft_email)
        .await
        .expect("is_suppressed");
    assert!(
        !suppressed,
        "soft bounce must NOT add a suppression (transient failure)"
    );

    // Cleanup.
    pg.execute(
        "DELETE FROM auth.email_suppressions WHERE email = $1::citext",
        &[&email],
    )
    .await
    .ok();
    drop(srv);
}
