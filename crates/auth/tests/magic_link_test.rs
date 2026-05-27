//! Live-PG roundtrip for `auth::identity::magic_link`.
//!
//! Skipped unless `AUTH_DB_URL` is set. Each test scopes itself with a
//! random email so concurrent runs don't collide; the cleanup at the end
//! removes every row that test inserted.

use compio_postgres::{connect, NoTls};
use uuid::Uuid;

use zeroship_auth::identity::magic_link;
use zeroship_auth::store::migrations;

// `compio_postgres::Client` is `!Send` — the futures inherit that
// structurally. The lint is informational, not actionable here.
#[allow(clippy::future_not_send)]
async fn pg() -> Option<compio_postgres::Client> {
    let dsn = std::env::var("AUTH_DB_URL").ok()?;
    let (client, connection) = connect(&dsn, NoTls).await.expect("connect");
    compio::runtime::spawn(async move {
        if let Err(e) = connection.run().await {
            eprintln!("magic_link test pg connection error: {e}");
        }
    })
    .detach();
    migrations::migrate(&client).await.expect("migrate");
    Some(client)
}

#[compio::test]
async fn issue_then_redeem_happy_path() {
    let Some(client) = pg().await else {
        eprintln!("skipping magic_link_test (no AUTH_DB_URL)");
        return;
    };

    let email = format!("magic-happy-{}@example.test", Uuid::new_v4().simple());
    let issued = magic_link::issue(&client, &email, "login")
        .await
        .expect("issue");
    assert!(!issued.raw.is_empty(), "raw token must be non-empty");
    assert!(
        !issued.csrf_nonce.is_empty(),
        "csrf nonce must be non-empty"
    );

    let redeemed = magic_link::redeem(&client, &issued.raw)
        .await
        .expect("redeem")
        .expect("redeem should return Some on first call");
    assert_eq!(redeemed.email, email);
    assert_eq!(redeemed.csrf_nonce, issued.csrf_nonce);
    assert_eq!(redeemed.purpose, "login");

    // Cleanup.
    client
        .execute(
            "DELETE FROM auth.magic_links WHERE email = $1::citext",
            &[&email],
        )
        .await
        .ok();
}

#[compio::test]
async fn second_redeem_returns_none() {
    let Some(client) = pg().await else {
        eprintln!("skipping magic_link_test (no AUTH_DB_URL)");
        return;
    };

    let email = format!("magic-once-{}@example.test", Uuid::new_v4().simple());
    let issued = magic_link::issue(&client, &email, "login")
        .await
        .expect("issue");

    let first = magic_link::redeem(&client, &issued.raw)
        .await
        .expect("redeem 1");
    assert!(first.is_some(), "first redeem must succeed");

    let second = magic_link::redeem(&client, &issued.raw)
        .await
        .expect("redeem 2");
    assert!(
        second.is_none(),
        "second redeem must return None (single-use)"
    );

    client
        .execute(
            "DELETE FROM auth.magic_links WHERE email = $1::citext",
            &[&email],
        )
        .await
        .ok();
}

#[compio::test]
async fn expired_token_returns_none() {
    let Some(client) = pg().await else {
        eprintln!("skipping magic_link_test (no AUTH_DB_URL)");
        return;
    };

    let email = format!("magic-expired-{}@example.test", Uuid::new_v4().simple());
    let issued = magic_link::issue(&client, &email, "login")
        .await
        .expect("issue");

    // Force the row's expiry into the past.
    client
        .execute(
            "UPDATE auth.magic_links SET expires_at = NOW() - INTERVAL '1 minute' \
             WHERE email = $1::citext AND consumed_at IS NULL",
            &[&email],
        )
        .await
        .expect("force expiry");

    let attempt = magic_link::redeem(&client, &issued.raw)
        .await
        .expect("redeem");
    assert!(attempt.is_none(), "expired token must NOT redeem");

    client
        .execute(
            "DELETE FROM auth.magic_links WHERE email = $1::citext",
            &[&email],
        )
        .await
        .ok();
}

#[compio::test]
async fn new_issue_supersedes_previous_unconsumed() {
    let Some(client) = pg().await else {
        eprintln!("skipping magic_link_test (no AUTH_DB_URL)");
        return;
    };

    let email = format!("magic-super-{}@example.test", Uuid::new_v4().simple());
    let first = magic_link::issue(&client, &email, "login")
        .await
        .expect("issue 1");
    let _second = magic_link::issue(&client, &email, "login")
        .await
        .expect("issue 2");

    // The first raw token must no longer be redeemable.
    let attempt = magic_link::redeem(&client, &first.raw)
        .await
        .expect("redeem old");
    assert!(
        attempt.is_none(),
        "previous unconsumed token must be invalidated by a fresh issue"
    );

    client
        .execute(
            "DELETE FROM auth.magic_links WHERE email = $1::citext",
            &[&email],
        )
        .await
        .ok();
}
