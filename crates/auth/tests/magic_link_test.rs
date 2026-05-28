//! Live-PG roundtrip for `auth::identity::magic_link`.
//!
//! Skipped unless `AUTH_DB_URL` is set. Each test scopes itself with a
//! random email so concurrent runs don't collide; the cleanup at the end
//! removes every row that test inserted.

use std::time::Duration;

use compio_postgres::{connect, Client, NoTls};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use zeroship_auth::identity::magic_link;
use zeroship_auth::store::migrations;
use zeroship_auth::ui::magic::completions_store::{self, ConsumeError};

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

async fn pg_connect(dsn: &str) -> Client {
    let (client, connection) = connect(dsn, NoTls).await.expect("connect");
    compio::runtime::spawn(async move {
        if let Err(e) = connection.run().await {
            eprintln!("magic_link test pg connection error: {e}");
        }
    })
    .detach();
    client
}

async fn install_magic_links_insert_delay(client: &Client) {
    client
        .execute(
            "CREATE OR REPLACE FUNCTION auth.test_sleep_before_magic_link_insert() \
             RETURNS trigger LANGUAGE plpgsql AS $$ \
             BEGIN \
                 PERFORM pg_sleep(0.2); \
                 RETURN NEW; \
             END \
             $$",
            &[],
        )
        .await
        .expect("create insert delay function");
    client
        .execute(
            "DROP TRIGGER IF EXISTS test_sleep_before_magic_link_insert ON auth.magic_links",
            &[],
        )
        .await
        .expect("drop stale insert delay trigger");
    client
        .execute(
            "CREATE TRIGGER test_sleep_before_magic_link_insert \
             BEFORE INSERT ON auth.magic_links \
             FOR EACH ROW EXECUTE FUNCTION auth.test_sleep_before_magic_link_insert()",
            &[],
        )
        .await
        .expect("create insert delay trigger");
}

async fn drop_magic_links_insert_delay(client: &Client) {
    client
        .execute(
            "DROP TRIGGER IF EXISTS test_sleep_before_magic_link_insert ON auth.magic_links",
            &[],
        )
        .await
        .ok();
}

async fn install_magic_completion_reserve_delay(client: &Client) {
    client
        .execute(
            "CREATE OR REPLACE FUNCTION auth.test_sleep_before_magic_completion_reserve() \
             RETURNS trigger LANGUAGE plpgsql AS $$ \
             BEGIN \
                 IF NEW.consumed_pending_at IS NOT NULL \
                    AND OLD.consumed_pending_at IS NULL THEN \
                     PERFORM pg_sleep(0.2); \
                 END IF; \
                 RETURN NEW; \
             END \
             $$",
            &[],
        )
        .await
        .expect("create completion reserve delay function");
    client
        .execute(
            "DROP TRIGGER IF EXISTS test_sleep_before_magic_completion_reserve \
             ON auth.magic_completions",
            &[],
        )
        .await
        .expect("drop stale completion reserve delay trigger");
    client
        .execute(
            "CREATE TRIGGER test_sleep_before_magic_completion_reserve \
             BEFORE UPDATE OF consumed_pending_at ON auth.magic_completions \
             FOR EACH ROW EXECUTE FUNCTION auth.test_sleep_before_magic_completion_reserve()",
            &[],
        )
        .await
        .expect("create completion reserve delay trigger");
}

async fn drop_magic_completion_reserve_delay(client: &Client) {
    client
        .execute(
            "DROP TRIGGER IF EXISTS test_sleep_before_magic_completion_reserve \
             ON auth.magic_completions",
            &[],
        )
        .await
        .ok();
}

fn sha256(s: &str) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    h.finalize().into()
}

#[compio::test]
async fn wrong_code_does_not_mutate_reserved_completion() {
    let dsn = match std::env::var("AUTH_DB_URL") {
        Ok(dsn) => dsn,
        Err(_) => {
            eprintln!("skipping magic_link_test (no AUTH_DB_URL)");
            return;
        }
    };
    let Some(client) = pg().await else {
        eprintln!("skipping magic_link_test (no AUTH_DB_URL)");
        return;
    };

    let csrf_nonce = format!("completion-reserved-{}", Uuid::new_v4().simple());
    let email = format!("magic-reserved-{}@example.test", Uuid::new_v4().simple());
    let login_challenge = format!("lc-{}", Uuid::new_v4().simple());
    let code = "123456";

    client
        .execute(
            "INSERT INTO auth.magic_completions \
                (csrf_nonce, code, email, login_challenge, expires_at) \
             VALUES ($1, $2, $3::citext, $4, NOW() + INTERVAL '5 minutes')",
            &[&csrf_nonce, &code, &email, &login_challenge],
        )
        .await
        .expect("insert completion row");
    install_magic_completion_reserve_delay(&client).await;

    let correct_client = pg_connect(&dsn).await;
    let wrong_client = pg_connect(&dsn).await;
    let correct_nonce = csrf_nonce.clone();
    let wrong_nonce = csrf_nonce.clone();
    let correct = compio::runtime::spawn(async move {
        completions_store::consume_pending(&correct_client, &correct_nonce, code).await
    });
    compio::time::sleep(Duration::from_millis(50)).await;
    let wrong = compio::runtime::spawn(async move {
        completions_store::consume_pending(&wrong_client, &wrong_nonce, "000000").await
    });

    let correct = correct
        .await
        .expect("join correct consume")
        .expect("correct consume");
    assert_eq!(correct.email, email);
    let wrong = wrong
        .await
        .expect("join wrong consume")
        .expect_err("wrong code must not consume reserved completion");
    assert!(
        matches!(wrong, ConsumeError::WrongCode),
        "wrong code racing a reservation should return WrongCode, got {wrong:?}"
    );

    drop_magic_completion_reserve_delay(&client).await;

    let row = client
        .query_one(
            "SELECT attempts, \
                    consumed_pending_at IS NOT NULL AS pending, \
                    consumed_at IS NOT NULL AS consumed \
             FROM auth.magic_completions \
             WHERE csrf_nonce = $1",
            &[&csrf_nonce],
        )
        .await
        .expect("load completion row");
    let attempts: i16 = row.get("attempts");
    let pending: bool = row.get("pending");
    let consumed: bool = row.get("consumed");
    assert_eq!(
        attempts, 0,
        "wrong-code update must not increment attempts on a reserved row"
    );
    assert!(pending, "correct code should reserve the completion");
    assert!(!consumed, "completion should not be finalized by consume_pending");

    client
        .execute(
            "DELETE FROM auth.magic_completions WHERE csrf_nonce = $1",
            &[&csrf_nonce],
        )
        .await
        .ok();
}

#[compio::test]
async fn concurrent_issue_leaves_one_active_token() {
    let dsn = match std::env::var("AUTH_DB_URL") {
        Ok(dsn) => dsn,
        Err(_) => {
            eprintln!("skipping magic_link_test (no AUTH_DB_URL)");
            return;
        }
    };
    let Some(client) = pg().await else {
        eprintln!("skipping magic_link_test (no AUTH_DB_URL)");
        return;
    };

    let email = format!("magic-concurrent-{}@example.test", Uuid::new_v4().simple());
    install_magic_links_insert_delay(&client).await;

    let client_a = pg_connect(&dsn).await;
    let client_b = pg_connect(&dsn).await;
    let email_a = email.clone();
    let email_b = email.clone();
    let issue_a =
        compio::runtime::spawn(async move { magic_link::issue(&client_a, &email_a, "login").await });
    let issue_b =
        compio::runtime::spawn(async move { magic_link::issue(&client_b, &email_b, "login").await });

    issue_a.await.expect("join issue A").expect("issue A");
    issue_b.await.expect("join issue B").expect("issue B");

    drop_magic_links_insert_delay(&client).await;

    let active_count: i64 = client
        .query_one(
            "SELECT COUNT(*) FROM auth.magic_links \
             WHERE email = $1::citext AND purpose = $2 AND consumed_at IS NULL",
            &[&email, &"login"],
        )
        .await
        .expect("count active magic links")
        .get(0);
    assert_eq!(
        active_count, 1,
        "concurrent issue must leave exactly one active magic-link token"
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

    let redeemed = magic_link::redeem_pending(&client, &issued.raw)
        .await
        .expect("redeem")
        .expect("redeem should return Some on first call");
    assert_eq!(redeemed.email, email);
    assert_eq!(redeemed.csrf_nonce, issued.csrf_nonce);
    assert_eq!(redeemed.purpose, "login");
    assert!(
        magic_link::finalize_consume(&client, &redeemed.token_hash)
            .await
            .expect("finalize consume"),
        "finalize should update the pending row"
    );

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

    let first = magic_link::redeem_pending(&client, &issued.raw)
        .await
        .expect("redeem 1");
    let first = first.expect("first redeem must succeed");
    assert!(
        magic_link::finalize_consume(&client, &first.token_hash)
            .await
            .expect("finalize consume"),
        "finalize should update the pending row"
    );

    let second = magic_link::redeem_pending(&client, &issued.raw)
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
async fn pending_consume_can_be_cleared_and_retried_before_finalize() {
    let Some(client) = pg().await else {
        eprintln!("skipping magic_link_test (no AUTH_DB_URL)");
        return;
    };

    let email = format!("magic-pending-retry-{}@example.test", Uuid::new_v4().simple());
    let issued = magic_link::issue(&client, &email, "login")
        .await
        .expect("issue");

    let first = magic_link::redeem_pending(&client, &issued.raw)
        .await
        .expect("redeem pending 1")
        .expect("first redeem should reserve token");
    assert_eq!(first.email, email);

    let rows = client
        .query(
            "SELECT consumed_pending_at IS NOT NULL AS pending, \
                    consumed_at IS NOT NULL AS consumed \
             FROM auth.magic_links \
             WHERE token_hash = $1",
            &[&first.token_hash.as_slice()],
        )
        .await
        .expect("load pending row");
    assert_eq!(rows.len(), 1, "magic link row should exist");
    let pending: bool = rows[0].get("pending");
    let consumed: bool = rows[0].get("consumed");
    assert!(pending, "redeem_pending should set consumed_pending_at");
    assert!(!consumed, "redeem_pending must not finalize consumed_at");

    assert!(
        magic_link::clear_consume_pending(&client, &first.token_hash)
            .await
            .expect("clear consume pending"),
        "clear should update the pending row"
    );

    let second = magic_link::redeem_pending(&client, &issued.raw)
        .await
        .expect("redeem pending 2")
        .expect("same token should be retriable after clear");
    assert_eq!(second.email, email);

    assert!(
        magic_link::finalize_consume(&client, &second.token_hash)
            .await
            .expect("finalize consume"),
        "finalize should set consumed_at"
    );

    let third = magic_link::redeem_pending(&client, &issued.raw)
        .await
        .expect("redeem after finalize");
    assert!(third.is_none(), "finalized token should not redeem again");

    client
        .execute(
            "DELETE FROM auth.magic_links WHERE email = $1::citext",
            &[&email],
        )
        .await
        .ok();
}

#[compio::test]
async fn second_redeem_while_pending_returns_in_flight() {
    let Some(client) = pg().await else {
        eprintln!("skipping magic_link_test (no AUTH_DB_URL)");
        return;
    };

    let email = format!("magic-inflight-{}@example.test", Uuid::new_v4().simple());
    let issued = magic_link::issue(&client, &email, "login")
        .await
        .expect("issue");

    let first = magic_link::redeem_pending(&client, &issued.raw)
        .await
        .expect("redeem pending 1")
        .expect("first redeem should reserve token");

    let err = magic_link::redeem_pending(&client, &issued.raw)
        .await
        .expect_err("second redeem during pending window should be in-flight");
    assert!(
        matches!(err, magic_link::RedeemError::InFlight),
        "second pending redeem should return InFlight, got {err:?}"
    );

    magic_link::clear_consume_pending(&client, &first.token_hash)
        .await
        .expect("clear consume pending");

    client
        .execute(
            "DELETE FROM auth.magic_links WHERE email = $1::citext",
            &[&email],
        )
        .await
        .ok();
}

#[compio::test]
async fn redeem_rejects_reset_purpose_row_without_consuming_it() {
    let Some(client) = pg().await else {
        eprintln!("skipping magic_link_test (no AUTH_DB_URL)");
        return;
    };

    let email = format!("magic-reset-purpose-{}@example.test", Uuid::new_v4().simple());
    let raw_token = format!("reset-token-{}", Uuid::new_v4().simple());
    let token_hash = sha256(&raw_token);
    let csrf_nonce = "reset-no-csrf-nonce";
    let purpose = "reset";

    client
        .execute(
            "INSERT INTO auth.magic_links \
                (token_hash, email, csrf_nonce, purpose, expires_at) \
             VALUES ($1, $2::citext, $3, $4, NOW() + INTERVAL '60 minutes')",
            &[&token_hash.as_slice(), &email, &csrf_nonce, &purpose],
        )
        .await
        .expect("insert reset-purpose row");

    let attempt = magic_link::redeem_pending(&client, &raw_token)
        .await
        .expect("redeem reset-purpose row via magic login");
    assert!(
        attempt.is_none(),
        "reset-purpose row must not redeem through magic-link login"
    );

    let rows = client
        .query(
            "SELECT consumed_at IS NULL AS still_unconsumed \
             FROM auth.magic_links \
             WHERE token_hash = $1 AND email = $2::citext AND purpose = $3",
            &[&token_hash.as_slice(), &email, &purpose],
        )
        .await
        .expect("load reset-purpose row after rejected redeem");
    assert_eq!(rows.len(), 1, "test row must still exist");
    let still_unconsumed: bool = rows[0].get("still_unconsumed");
    assert!(
        still_unconsumed,
        "rejected reset-purpose row must remain unconsumed"
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

    let attempt = magic_link::redeem_pending(&client, &issued.raw)
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
    let attempt = magic_link::redeem_pending(&client, &first.raw)
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

#[compio::test]
async fn magic_completion_invalidates_after_five_wrong_codes() {
    let Some(client) = pg().await else {
        eprintln!("skipping magic_link_test (no AUTH_DB_URL)");
        return;
    };

    let csrf_nonce = format!("completion-attempts-{}", Uuid::new_v4().simple());
    let email = format!("magic-complete-{}@example.test", Uuid::new_v4().simple());
    let login_challenge = format!("lc-{}", Uuid::new_v4().simple());
    let code = "123456";

    client
        .execute(
            "INSERT INTO auth.magic_completions \
                (csrf_nonce, code, email, login_challenge, expires_at) \
             VALUES ($1, $2, $3::citext, $4, NOW() + INTERVAL '5 minutes')",
            &[&csrf_nonce, &code, &email, &login_challenge],
        )
        .await
        .expect("insert completion row");

    for i in 1..=4 {
        let err = completions_store::consume_pending(&client, &csrf_nonce, "000000")
            .await
            .expect_err("wrong code must fail before invalidation");
        assert!(
            matches!(err, ConsumeError::WrongCode),
            "wrong attempt {i} should return WrongCode, got {err:?}"
        );
    }

    let err = completions_store::consume_pending(&client, &csrf_nonce, "000000")
        .await
        .expect_err("fifth wrong code must fail and invalidate");
    assert!(
        matches!(err, ConsumeError::WrongCode),
        "fifth wrong attempt should return WrongCode, got {err:?}"
    );

    let rows = client
        .query(
            "SELECT consumed_at IS NOT NULL AS consumed \
             FROM auth.magic_completions \
             WHERE csrf_nonce = $1",
            &[&csrf_nonce],
        )
        .await
        .expect("load completion row");
    assert_eq!(rows.len(), 1, "completion row should still exist");
    let consumed: bool = rows[0].get("consumed");
    assert!(
        consumed,
        "fifth wrong completion attempt must invalidate the row"
    );

    let err = completions_store::consume_pending(&client, &csrf_nonce, code)
        .await
        .expect_err("correct code must not redeem after invalidation");
    assert!(
        matches!(err, ConsumeError::WrongCode),
        "correct code after invalidation should return WrongCode, got {err:?}"
    );

    client
        .execute(
            "DELETE FROM auth.magic_completions WHERE csrf_nonce = $1",
            &[&csrf_nonce],
        )
        .await
        .ok();
}
