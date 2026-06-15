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
use clap::Parser;
use compio_postgres::{connect, NoTls};
use ntex::web;
use serde_json::json;
use uuid::Uuid;
use zeroship_auth::config::AuthConfig;
use zeroship_mailer::suppressions;

/// Build an `AuthConfig` for the webhook tests. Only the
/// `postmark_webhook_*` fields are interesting; everything else takes
/// the clap default declared in `AuthConfig`. Going through
/// `parse_from` means new fields added in future phases land with
/// their defaults — no fixture-sync churn.
fn test_cfg(user: Option<&str>, pass: Option<&str>) -> AuthConfig {
    let mut args: Vec<String> = vec![
        "zeroship-auth".to_string(),
        // Required by clap (no default on `--db-url`). Webhook tests
        // bring their own pg client and only use AuthConfig for the
        // basic-auth comparison, so any value parses.
        "--db-url".to_string(),
        String::new(),
        "--dev-insecure".to_string(),
    ];
    if let Some(u) = user {
        args.push("--postmark-webhook-user".to_string());
        args.push(u.to_string());
    }
    if let Some(p) = pass {
        args.push("--postmark-webhook-password".to_string());
        args.push(p.to_string());
    }
    let mut cfg = AuthConfig::parse_from(args);
    cfg.resolve(zeroship_core::config::AuthSection::default());
    cfg
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
        "DELETE FROM zeroship.email_suppressions WHERE email = $1::citext",
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
        "DELETE FROM zeroship.email_suppressions WHERE email = $1::citext",
        &[&email],
    )
    .await
    .ok();
    drop(srv);
}
