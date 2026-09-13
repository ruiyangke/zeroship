//! Shared buckets settle refill, capacity and contention in PostgreSQL.

#![allow(
    clippy::future_not_send,
    reason = "fixtures belong to their compio runtime"
)]

use crate::common::database::Database;
use compio_postgres::Transaction;
use zeroship_authn::rate_limit::{consume_state, Consumption, Quota};

const KEY: &str = "login:recipient@example.test";

async fn drained_bucket(transaction: &Transaction<'_>, elapsed_secs: f64) {
    transaction.execute(
        "INSERT INTO zeroship.rate_limits (bucket_key, tokens, updated_at) \
         VALUES ($1, 0.0::REAL, NOW() - $2::DOUBLE PRECISION * INTERVAL '1 second') \
         ON CONFLICT (bucket_key) DO UPDATE SET tokens = EXCLUDED.tokens, updated_at = EXCLUDED.updated_at",
        &[&KEY, &elapsed_secs],
    ).await.unwrap();
}

fn assert_consumption(actual: Consumption, consumed: bool, remaining_tokens: f64) {
    assert_eq!(actual.consumed, consumed);
    assert!(
        (actual.remaining_tokens - remaining_tokens).abs() < f64::EPSILON,
        "expected {remaining_tokens} remaining tokens, got {}",
        actual.remaining_tokens,
    );
}

#[compio::test]
async fn service_roles_refill_at_the_configured_rate() {
    Database::run(async |database| {
        for role in ["zeroship_auth", "zeroship_control"] {
            let mut client = database.connect_as(role).await;
            let transaction = client.transaction().await.unwrap();
            let quota = Quota {
                capacity: 3.0,
                refill_per_sec: 1.0,
            };
            drained_bucket(&transaction, 0.75).await;
            assert_consumption(
                consume_state(&transaction, KEY, quota).await.unwrap(),
                false,
                0.75,
            );
            drained_bucket(&transaction, 1.25).await;
            assert_consumption(
                consume_state(&transaction, KEY, quota).await.unwrap(),
                true,
                0.25,
            );
            transaction.rollback().await.unwrap();
        }
    })
    .await;
}

#[compio::test]
async fn an_idle_bucket_cannot_bank_more_than_its_capacity() {
    Database::run(async |database| {
        let mut client = database.connect_as("zeroship_auth").await;
        let transaction = client.transaction().await.unwrap();
        let quota = Quota {
            capacity: 3.0,
            refill_per_sec: 1.0,
        };
        drained_bucket(&transaction, 3_600.0).await;
        for remaining in [2.0, 1.0, 0.0] {
            assert_consumption(
                consume_state(&transaction, KEY, quota).await.unwrap(),
                true,
                remaining,
            );
        }
        assert_consumption(
            consume_state(&transaction, KEY, quota).await.unwrap(),
            false,
            0.0,
        );
        transaction.rollback().await.unwrap();
    })
    .await;
}

#[compio::test]
async fn concurrent_first_claims_cannot_overdraw_a_bucket_or_report_a_store_outage() {
    Database::run(async |database| {
        let mut admin = database.connect().await;
        let first = database.connect_as("zeroship_auth").await;
        let second = database.connect_as("zeroship_control").await;
        let mut pids = Vec::new();
        for client in [&first, &second] {
            pids.push(
                client
                    .query_one("SELECT pg_backend_pid()", &[])
                    .await
                    .unwrap()
                    .get::<_, i32>(0),
            );
        }
        let held = admin.transaction().await.unwrap();
        held.batch_execute("LOCK TABLE zeroship.rate_limits IN SHARE MODE")
            .await
            .unwrap();
        let quota = Quota {
            capacity: 1.0,
            refill_per_sec: 0.0,
        };
        let (left, right, blocked) = futures::join!(
            consume_state(&first, KEY, quota),
            consume_state(&second, KEY, quota),
            async {
                let blocked = database.wait_until_blocked(&pids).await;
                held.commit().await.unwrap();
                blocked
            },
        );
        assert!(
            blocked,
            "both consumes must reach PostgreSQL before the fixture releases them"
        );
        let left = left.unwrap();
        let right = right.unwrap();
        assert_ne!(left.consumed, right.consumed);
        assert_eq!(left.remaining_tokens, 0.0);
        assert_eq!(right.remaining_tokens, 0.0);
        let rows = admin
            .query(
                "SELECT bucket_key, tokens::double precision FROM zeroship.rate_limits",
                &[],
            )
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get::<_, String>(0), KEY);
        assert_eq!(rows[0].get::<_, f64>(1), 0.0);
        assert_consumption(consume_state(&first, KEY, quota).await.unwrap(), false, 0.0);
        assert_consumption(
            consume_state(&second, "login:other@example.test", quota)
                .await
                .unwrap(),
            true,
            0.0,
        );
    })
    .await;
}
