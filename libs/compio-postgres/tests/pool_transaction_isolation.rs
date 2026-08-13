//! A pooled connection must not hand its successor an open transaction.
//!
//! This lives in its own target, and uses ONLY pre-existing driver API, so that
//! it survives a full revert of the transaction-status feature.
//!
//! The tests in `integration.rs` that cover the same fix import
//! `TransactionStatus` and call `transaction_status()`. That is right for
//! pinning the new behaviour precisely, but it means a revert of the whole
//! feature produces a COMPILE ERROR rather than a failing test - and a build
//! break reads as "the tests are stale", which is the wrong signal to hand
//! whoever is bisecting. This file fails the way a regression should: red, with
//! a message naming the behaviour.
//!
//! It asserts through visible effects only - a row count and a transaction-id
//! probe - so the only thing that can break it is the behaviour itself.

use compio_postgres::{Pool, PoolConfig};

mod common;

fn test_url() -> String {
    common::env::get(common::env::TestEnvKey::PgTestUrl)
        .unwrap_or_else(|| "postgres://postgres:zeroship@localhost:5440/zeroship".to_string())
}

/// Schema of its own, so this target can run beside `integration.rs` without
/// either tripping over the other's fixed object names.
const SCHEMA: &str = "cpg_pool_tx_isolation";

#[compio::test]
async fn a_raw_begin_does_not_leak_to_the_next_borrower() {
    let url = test_url();
    // One connection, so the release and the next acquisition are guaranteed to
    // be the same backend - without that the test can pass by being handed a
    // different, clean connection.
    let config = PoolConfig {
        max_size: 1,
        min_idle: 1,
        ..PoolConfig::default()
    };
    let Ok(pool) = Pool::connect_with_config(&url, config).await else {
        common::skip("pool_transaction_isolation (no reachable database)");
        return;
    };

    {
        let client = pool.get().await.expect("first checkout");
        client
            .batch_execute(&format!(
                "DROP SCHEMA IF EXISTS {SCHEMA} CASCADE; CREATE SCHEMA {SCHEMA};
                 CREATE TABLE {SCHEMA}.t (id int);"
            ))
            .await
            .expect("seed schema");
    }

    // Borrower 1 opens a transaction with RAW SQL - no `Transaction` guard is
    // involved, which is the case that has no other protection - and releases
    // without committing.
    {
        let client = pool.get().await.expect("second checkout");
        client
            .batch_execute(&format!("BEGIN; INSERT INTO {SCHEMA}.t VALUES (1);"))
            .await
            .expect("open a raw transaction and write in it");
    }

    // Borrower 2 gets the same backend. If the transaction leaked it sees its
    // own uncommitted row and reports an assigned transaction id.
    let client = pool.get().await.expect("third checkout");
    let rows = client
        .query(&format!("SELECT count(*)::int8 AS n FROM {SCHEMA}.t"), &[])
        .await
        .expect("count rows");
    let visible: i64 = rows[0].get("n");
    let in_transaction: bool = client
        .query(
            "SELECT (pg_current_xact_id_if_assigned() IS NOT NULL) AS b",
            &[],
        )
        .await
        .map(|r| r[0].get("b"))
        .unwrap_or(false);

    let _ = client
        .batch_execute(&format!(
            "ROLLBACK; DROP SCHEMA IF EXISTS {SCHEMA} CASCADE;"
        ))
        .await;

    assert_eq!(
        visible, 0,
        "the next borrower inherited an uncommitted row, so the pool handed back \
         a connection still inside a raw transaction"
    );
    assert!(
        !in_transaction,
        "the next borrower is inside a transaction it never opened"
    );
}
