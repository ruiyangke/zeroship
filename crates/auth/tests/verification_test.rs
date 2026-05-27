//! Live-PG roundtrip for `auth::identity::verification`.
//!
//! Skipped unless `AUTH_DB_URL` is set. Each test scopes itself with a
//! random email so concurrent runs don't collide; the cleanup at the end
//! removes every row that test inserted (verifications + the seeded user).

use compio_postgres::{connect, NoTls};
use uuid::Uuid;

use zeroship_auth::identity::verification;
use zeroship_auth::store::{migrations, users};

// `compio_postgres::Client` is `!Send` — the futures inherit that
// structurally. The lint is informational, not actionable here.
#[allow(clippy::future_not_send)]
async fn pg() -> Option<compio_postgres::Client> {
    let dsn = std::env::var("AUTH_DB_URL").ok()?;
    let (client, connection) = connect(&dsn, NoTls).await.expect("connect");
    compio::runtime::spawn(async move {
        if let Err(e) = connection.run().await {
            eprintln!("verification test pg connection error: {e}");
        }
    })
    .detach();
    migrations::migrate(&client).await.expect("migrate");
    Some(client)
}

#[compio::test]
async fn issue_then_redeem_roundtrip() {
    let Some(client) = pg().await else {
        eprintln!("skipping verification_test (no AUTH_DB_URL)");
        return;
    };

    let email = format!("verify-{}@zeroship.test", Uuid::new_v4().simple());
    let user = users::create(&client, &email, "Test", None)
        .await
        .expect("seed user");

    let issued = verification::issue(&client, user.id, &email)
        .await
        .expect("issue");
    assert!(!issued.raw.is_empty(), "raw token must be non-empty");

    let redeemed = verification::redeem(&client, &issued.raw)
        .await
        .expect("redeem")
        .expect("redeem should return Some on first call");
    assert_eq!(redeemed.user_id, user.id);
    assert_eq!(
        redeemed.email.to_ascii_lowercase(),
        email.to_ascii_lowercase()
    );

    // Single-use: second redeem returns None.
    let second = verification::redeem(&client, &issued.raw)
        .await
        .expect("redeem 2");
    assert!(
        second.is_none(),
        "second redeem must return None (single-use)"
    );

    // Cleanup.
    client
        .execute(
            "DELETE FROM auth.email_verifications WHERE user_id = $1",
            &[&user.id],
        )
        .await
        .ok();
    client
        .execute("DELETE FROM auth.users WHERE id = $1", &[&user.id])
        .await
        .ok();
}

#[compio::test]
async fn new_issue_supersedes_previous() {
    let Some(client) = pg().await else {
        eprintln!("skipping verification_test (no AUTH_DB_URL)");
        return;
    };

    let email = format!(
        "verify-supersede-{}@zeroship.test",
        Uuid::new_v4().simple()
    );
    let user = users::create(&client, &email, "Test", None)
        .await
        .expect("seed user");

    let first = verification::issue(&client, user.id, &email)
        .await
        .expect("issue 1");
    let second = verification::issue(&client, user.id, &email)
        .await
        .expect("issue 2");

    // First token is now invalid (superseded by the second issue).
    let r1 = verification::redeem(&client, &first.raw)
        .await
        .expect("redeem first");
    assert!(
        r1.is_none(),
        "first token must be invalidated by a fresh issue"
    );

    // Second token still works.
    let r2 = verification::redeem(&client, &second.raw)
        .await
        .expect("redeem second");
    assert!(r2.is_some(), "second token must still redeem");

    // Cleanup.
    client
        .execute(
            "DELETE FROM auth.email_verifications WHERE user_id = $1",
            &[&user.id],
        )
        .await
        .ok();
    client
        .execute("DELETE FROM auth.users WHERE id = $1", &[&user.id])
        .await
        .ok();
}
