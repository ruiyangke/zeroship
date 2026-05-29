//! Live-PG checks for `/magic/verify` token-consumption boundaries.

use std::sync::Arc;

use clap::Parser;
use compio_postgres::{connect, NoTls};
use ntex::web::{self, test};
use uuid::Uuid;

use zeroship_auth::config::AuthConfig;
use zeroship_auth::hydra_client::HydraAdmin;
use zeroship_auth::identity::magic_link;
use zeroship_auth::store::migrations;

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

#[allow(clippy::future_not_send)]
async fn pg() -> Option<compio_postgres::Client> {
    let dsn = std::env::var("AUTH_DB_URL").ok()?;
    let (client, connection) = connect(&dsn, NoTls).await.expect("connect");
    compio::runtime::spawn(async move {
        if let Err(e) = connection.run().await {
            eprintln!("magic_verify_test pg connection error: {e}");
        }
    })
    .detach();
    migrations::migrate(&client).await.expect("migrate");
    Some(client)
}

#[compio::test]
#[allow(clippy::future_not_send)]
async fn verify_get_without_magic_cookie_does_not_consume_token() {
    let dsn = match std::env::var("AUTH_DB_URL") {
        Ok(dsn) => dsn,
        Err(_) => {
            eprintln!("skipping magic_verify_test (no AUTH_DB_URL)");
            return;
        }
    };
    let Some(client) = pg().await else {
        eprintln!("skipping magic_verify_test (no AUTH_DB_URL)");
        return;
    };

    let email = format!("magic-scanner-{}@zeroship.test", Uuid::new_v4().simple());
    let issued = magic_link::issue(&client, &email, "login")
        .await
        .expect("issue magic token");

    let pg = Arc::new(client);
    let cfg = Arc::new(test_cfg(&dsn));
    let admin = HydraAdmin::new("http://127.0.0.1:1");
    let app = test::init_service(
        web::App::new()
            .state(cfg)
            .state(pg.clone())
            .state(admin)
            .service(
                web::resource("/magic/verify")
                    .route(web::get().to(zeroship_auth::ui::magic::verify)),
            ),
    )
    .await;

    let req = test::TestRequest::get()
        .uri(&format!(
            "/magic/verify?token={}&login_challenge=lc-scanner",
            issued.raw
        ))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status().as_u16(), 200);

    let row = pg
        .query_one(
            "SELECT consumed_pending_at IS NOT NULL AS pending, \
                    consumed_at IS NOT NULL AS consumed \
             FROM auth.magic_links \
             WHERE email = $1::citext AND purpose = 'login'",
            &[&email],
        )
        .await
        .expect("load magic link");
    let pending: bool = row.get("pending");
    let consumed: bool = row.get("consumed");
    assert!(!pending, "cookie-less GET must not reserve the token");
    assert!(!consumed, "cookie-less GET must not consume the token");

    let completions: i64 = pg
        .query_one(
            "SELECT COUNT(*) FROM auth.magic_completions WHERE email = $1::citext",
            &[&email],
        )
        .await
        .expect("count magic completions")
        .get(0);
    assert_eq!(
        completions, 0,
        "cookie-less GET must not create a cross-device completion"
    );

    let users: i64 = pg
        .query_one(
            "SELECT COUNT(*) FROM auth.users WHERE email = $1::citext",
            &[&email],
        )
        .await
        .expect("count users")
        .get(0);
    assert_eq!(users, 0, "cookie-less GET must not create a user");

    pg.execute(
        "DELETE FROM auth.magic_links WHERE email = $1::citext",
        &[&email],
    )
    .await
    .ok();
}
