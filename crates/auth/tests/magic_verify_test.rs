//! Live-PG checks for `/magic/verify` token-consumption boundaries.

use std::sync::Arc;

use compio_postgres::{connect, NoTls};
use ntex::web::{self, test};
use uuid::Uuid;

use zeroship_auth::config::AuthConfig;
use zeroship_core::config::{Secret, SourceKind};
use zeroship_auth::identity::magic_link;

fn test_cfg(db_url: &str) -> AuthConfig {
    // A secret has no value flag - that is the point of the conversion - so the
    // fixture supplies each one in the shape an in-memory literal resolves to,
    // which is byte-for-byte what ZEROSHIP_AUTH_<NAME>=<value> produces. The
    // environment itself is process-global and would race sibling tests.
    let mut cfg = AuthConfig::parse_from(["zeroship-auth"]);
    cfg.settings.database_url = Secret::supplied(SourceKind::Env, Some(db_url.to_owned()));
    cfg.settings.stash_signing_key = Secret::supplied(
        SourceKind::Env,
        Some("test-stash-key-not-for-prod-32bytes!".to_owned()),
    );
    cfg
}

#[allow(clippy::future_not_send)]
async fn pg() -> Option<compio_postgres::Client> {
    let dsn = zeroship_core::declared_env!(external, "AUTH_DB_URL", zeroship_core::config::TestHarness)?;
    let (client, connection) = connect(&dsn, NoTls).await.expect("connect");
    compio::runtime::spawn(async move {
        if let Err(e) = connection.run().await {
            eprintln!("magic_verify_test pg connection error: {e}");
        }
    })
    .detach();
    Some(client)
}

#[compio::test]
#[allow(clippy::future_not_send)]
async fn verify_get_without_magic_cookie_does_not_consume_token() {
    let dsn = match zeroship_core::declared_env!(external, "AUTH_DB_URL", zeroship_core::config::TestHarness) {
        Some(dsn) => dsn,
        None => {
            zeroship_test_support::skip("skipping magic_verify_test (no AUTH_DB_URL)");
            return;
        }
    };
    let Some(client) = pg().await else {
        zeroship_test_support::skip("skipping magic_verify_test (no AUTH_DB_URL)");
        return;
    };

    let email = format!("magic-scanner-{}@zeroship.test", Uuid::new_v4().simple());
    let issued = magic_link::issue(&client, &email, "login")
        .await
        .expect("issue magic token");

    let pg = Arc::new(client);
    let cfg = Arc::new(test_cfg(&dsn));
    let app = test::init_service(
        web::App::new()
            .state(cfg)
            .state(pg.clone())
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
             FROM zeroship.magic_links \
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
            "SELECT COUNT(*) FROM zeroship.magic_completions WHERE email = $1::citext",
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
            "SELECT COUNT(*) FROM zeroship.users WHERE email = $1::citext",
            &[&email],
        )
        .await
        .expect("count users")
        .get(0);
    assert_eq!(users, 0, "cookie-less GET must not create a user");

    pg.execute(
        "DELETE FROM zeroship.magic_links WHERE email = $1::citext",
        &[&email],
    )
    .await
    .ok();
}
