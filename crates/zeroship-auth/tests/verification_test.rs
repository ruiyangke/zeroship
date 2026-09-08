//! Live-PG roundtrip for `auth::identity::verification`.
//!
//! Requires a live PostgreSQL (`PG_TEST_URL` or the TOML overlay). A run
//! that cannot reach one is REFUSED, not skipped.
//!
//! Each test scopes itself with a random email so concurrent runs don't
//! collide; the cleanup at the end removes every row that test inserted
//! (verifications + the seeded user).

use compio_postgres::{connect, Client, NoTls};
use uuid::Uuid;

use zeroship_auth::identity::verification;
use zeroship_auth::store::{users};

// `compio_postgres::Client` is `!Send` — the futures inherit that
// structurally. The lint is informational, not actionable here.
#[allow(clippy::future_not_send)]
async fn pg() -> compio_postgres::Client {
    let dsn = crate::common::test_database_url();
    let (client, connection) = connect(&dsn, NoTls).await.expect("connect");
    compio::runtime::spawn(async move {
        if let Err(e) = connection.run().await {
            eprintln!("verification test pg connection error: {e}");
        }
    })
    .detach();
    client
}

async fn pg_connect(dsn: &str) -> Client {
    let (client, connection) = connect(dsn, NoTls).await.expect("connect");
    compio::runtime::spawn(async move {
        if let Err(e) = connection.run().await {
            eprintln!("verification test pg connection error: {e}");
        }
    })
    .detach();
    client
}

// `zeroship.email_verifications` is shared with every concurrent run, so the
// name is per-call and the `WHEN` clause scopes the trigger to the one user
// this test seeded. Renaming alone is not enough: a uniquely named trigger
// still sleeps 0.2s inside the peer run's inserts into the same table.
//
// The full argument, and the model it follows, is in `magic_link_test.rs`.
async fn install_verifications_insert_delay(client: &Client, user_id: Uuid) -> String {
    let name = format!(
        "test_sleep_before_verification_insert_{}",
        Uuid::new_v4().simple()
    );
    client
        .execute(
            &format!(
                "CREATE FUNCTION zeroship.{name}() \
                 RETURNS trigger LANGUAGE plpgsql AS $$ \
                 BEGIN \
                     PERFORM pg_sleep(0.2); \
                     RETURN NEW; \
                 END \
                 $$"
            ),
            &[],
        )
        .await
        .expect("create insert delay function");
    client
        .execute(
            &format!(
                "CREATE TRIGGER {name} \
                 BEFORE INSERT ON zeroship.email_verifications \
                 FOR EACH ROW WHEN (NEW.user_id = '{user_id}'::uuid) \
                 EXECUTE FUNCTION zeroship.{name}()"
            ),
            &[],
        )
        .await
        .expect("create insert delay trigger");
    name
}

async fn drop_verifications_insert_delay(client: &Client, name: &str) {
    client
        .execute(
            &format!("DROP TRIGGER IF EXISTS {name} ON zeroship.email_verifications"),
            &[],
        )
        .await
        .ok();
    client
        .execute(&format!("DROP FUNCTION IF EXISTS zeroship.{name}()"), &[])
        .await
        .ok();
}

#[compio::test]
async fn concurrent_issue_leaves_one_active_verification_token() {
    let dsn = crate::common::test_database_url();
    let client = pg().await;

    let email = format!(
        "verify-concurrent-{}@zeroship.test",
        Uuid::new_v4().simple()
    );
    let user = users::create(&client, &email, "Test", None)
        .await
        .expect("seed user");
    let insert_delay = install_verifications_insert_delay(&client, user.id).await;

    let client_a = pg_connect(&dsn).await;
    let client_b = pg_connect(&dsn).await;
    let email_a = email.clone();
    let email_b = email.clone();
    let user_id = user.id;
    let issue_a =
        compio::runtime::spawn(async move { verification::issue(&client_a, user_id, &email_a).await });
    let issue_b =
        compio::runtime::spawn(async move { verification::issue(&client_b, user_id, &email_b).await });

    issue_a.await.expect("join issue A").expect("issue A");
    issue_b.await.expect("join issue B").expect("issue B");

    drop_verifications_insert_delay(&client, &insert_delay).await;

    let active_count: i64 = client
        .query_one(
            "SELECT COUNT(*) FROM zeroship.email_verifications \
             WHERE user_id = $1 AND consumed_at IS NULL",
            &[&user.id],
        )
        .await
        .expect("count active verification links")
        .get(0);
    assert_eq!(
        active_count, 1,
        "concurrent issue must leave exactly one active verification token"
    );

    client
        .execute(
            "DELETE FROM zeroship.email_verifications WHERE user_id = $1",
            &[&user.id],
        )
        .await
        .ok();
    client
        .execute("DELETE FROM zeroship.users WHERE id = $1", &[&user.id])
        .await
        .ok();
}

#[compio::test]
async fn issue_then_redeem_roundtrip() {
    let client = pg().await;

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
            "DELETE FROM zeroship.email_verifications WHERE user_id = $1",
            &[&user.id],
        )
        .await
        .ok();
    client
        .execute("DELETE FROM zeroship.users WHERE id = $1", &[&user.id])
        .await
        .ok();
}

#[compio::test]
async fn redeem_and_mark_verified_rolls_back_token_consume_with_transaction() {
    let client = pg().await;

    let email = format!(
        "verify-rollback-{}@zeroship.test",
        Uuid::new_v4().simple()
    );
    let user = users::create(&client, &email, "Test", None)
        .await
        .expect("seed user");
    let issued = verification::issue(&client, user.id, &email)
        .await
        .expect("issue");

    client.execute("BEGIN", &[]).await.expect("begin");
    let redeemed = verification::redeem_and_mark_verified(&client, &issued.raw)
        .await
        .expect("redeem and mark verified")
        .expect("token should redeem inside transaction");
    assert_eq!(redeemed.user_id, user.id);
    client.execute("ROLLBACK", &[]).await.expect("rollback");

    let row = client
        .query_one(
            "SELECT ev.consumed_at IS NULL AS token_unconsumed, \
                    u.email_verified_at IS NULL AS user_unverified \
             FROM zeroship.email_verifications ev \
             JOIN zeroship.users u ON u.id = ev.user_id \
             WHERE ev.user_id = $1",
            &[&user.id],
        )
        .await
        .expect("load verification state");
    let token_unconsumed: bool = row.get("token_unconsumed");
    let user_unverified: bool = row.get("user_unverified");
    assert!(
        token_unconsumed,
        "rolled-back verification must leave token unconsumed"
    );
    assert!(
        user_unverified,
        "rolled-back verification must leave user unverified"
    );

    client
        .execute(
            "DELETE FROM zeroship.email_verifications WHERE user_id = $1",
            &[&user.id],
        )
        .await
        .ok();
    client
        .execute("DELETE FROM zeroship.users WHERE id = $1", &[&user.id])
        .await
        .ok();
}

#[compio::test]
async fn new_issue_supersedes_previous() {
    let client = pg().await;

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
            "DELETE FROM zeroship.email_verifications WHERE user_id = $1",
            &[&user.id],
        )
        .await
        .ok();
    client
        .execute("DELETE FROM zeroship.users WHERE id = $1", &[&user.id])
        .await
        .ok();
}
