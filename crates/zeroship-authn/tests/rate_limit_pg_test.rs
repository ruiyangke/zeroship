//! Shared token-bucket refill and capacity tests against live `PostgreSQL`.

#![allow(clippy::future_not_send)]

use compio_postgres::{Client, NoTls, Transaction};
use zeroship_authn::rate_limit::{consume_state, Consumption, Quota};

const FIXTURE_LOCK: i64 = 7_523_000_002;
const FIXTURE_DDL: &str = "CREATE SCHEMA IF NOT EXISTS zeroship; \
    CREATE TABLE IF NOT EXISTS zeroship.rate_limits ( \
        bucket_key TEXT PRIMARY KEY, \
        tokens REAL NOT NULL, \
        updated_at TIMESTAMPTZ NOT NULL \
    )";

fn database_url_or_skip() -> Option<String> {
    match zeroship_core::test_env!("PG_TEST_URL") {
        Some(url) if !url.trim().is_empty() => Some(url),
        _ => {
            zeroship_test_support::skip(
                "no test database for PostgreSQL rate limiter (set PG_TEST_URL)",
            );
            None
        }
    }
}

async fn connect_or_skip() -> Option<Client> {
    let url = database_url_or_skip()?;
    let (client, connection) = match compio_postgres::connect(&url, NoTls).await {
        Ok(connected) => connected,
        Err(error) => {
            zeroship_test_support::skip(&format!(
                "cannot connect to PostgreSQL rate-limit database from PG_TEST_URL: {error}"
            ));
            return None;
        }
    };
    compio::runtime::spawn(async move {
        if let Err(error) = connection.run().await {
            eprintln!("[rate_limit_pg_test] connection driver: {error}");
        }
    })
    .detach();
    Some(client)
}

async fn ensure_fixture(client: &Client) {
    client
        .execute("SELECT pg_advisory_lock($1)", &[&FIXTURE_LOCK])
        .await
        .expect("take the rate-limit fixture lock");
    let established = client.batch_execute(FIXTURE_DDL).await;
    client
        .execute("SELECT pg_advisory_unlock($1)", &[&FIXTURE_LOCK])
        .await
        .expect("release the rate-limit fixture lock");
    established.expect("establish the rate-limit table fixture");
}

async fn seed_drained_bucket(transaction: &Transaction<'_>, key: &str, elapsed_secs: f64) {
    transaction
        .execute(
            "INSERT INTO zeroship.rate_limits (bucket_key, tokens, updated_at) \
             VALUES ($1, 0.0::REAL, NOW() - $2::DOUBLE PRECISION * INTERVAL '1 second')",
            &[&key, &elapsed_secs],
        )
        .await
        .expect("seed a drained rate-limit bucket");
}

async fn reset_drained_bucket(transaction: &Transaction<'_>, key: &str, elapsed_secs: f64) {
    transaction
        .execute(
            "UPDATE zeroship.rate_limits \
             SET tokens = 0.0::REAL, \
                 updated_at = NOW() - $2::DOUBLE PRECISION * INTERVAL '1 second' \
             WHERE bucket_key = $1",
            &[&key, &elapsed_secs],
        )
        .await
        .expect("reset a drained rate-limit bucket");
}

fn unique_key(case: &str) -> String {
    format!("test:rate-limit:{case}:{}", uuid::Uuid::new_v4().simple())
}

fn assert_consumption(actual: Consumption, consumed: bool, remaining_tokens: f64) {
    assert_eq!(actual.consumed, consumed);
    assert!(
        (actual.remaining_tokens - remaining_tokens).abs() < f64::EPSILON,
        "expected {remaining_tokens} remaining tokens, got {}",
        actual.remaining_tokens
    );
}

#[compio::test]
async fn postgres_limiter_refills_at_configured_rate() {
    let Some(mut client) = connect_or_skip().await else {
        return;
    };
    ensure_fixture(&client).await;
    let transaction = client.transaction().await.expect("begin refill test");
    let key = unique_key("refill");
    let quota = Quota {
        capacity: 3.0,
        refill_per_sec: 1.0,
    };

    seed_drained_bucket(&transaction, &key, 0.75).await;
    assert_consumption(
        consume_state(&transaction, &key, quota)
            .await
            .expect("consume before one token refills"),
        false,
        0.75,
    );

    reset_drained_bucket(&transaction, &key, 1.25).await;
    assert_consumption(
        consume_state(&transaction, &key, quota)
            .await
            .expect("consume after one token refills"),
        true,
        0.25,
    );
    transaction.rollback().await.expect("rollback refill test");
}

#[compio::test]
async fn postgres_limiter_caps_tokens_after_long_idle() {
    let Some(mut client) = connect_or_skip().await else {
        return;
    };
    ensure_fixture(&client).await;
    let transaction = client.transaction().await.expect("begin capacity test");
    let key = unique_key("capacity");
    let quota = Quota {
        capacity: 3.0,
        refill_per_sec: 1.0,
    };

    seed_drained_bucket(&transaction, &key, 3_600.0).await;
    for remaining_tokens in [2.0, 1.0, 0.0] {
        assert_consumption(
            consume_state(&transaction, &key, quota)
                .await
                .expect("consume within the capacity"),
            true,
            remaining_tokens,
        );
    }
    assert_consumption(
        consume_state(&transaction, &key, quota)
            .await
            .expect("consume after the capacity is exhausted"),
        false,
        0.0,
    );
    transaction
        .rollback()
        .await
        .expect("rollback capacity test");
}
