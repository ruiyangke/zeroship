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
    let pool = match Pool::connect_with_config(&url, config).await {
        Ok(pool) => pool,
        Err(e) => common::postgres_unreachable(&url, &e),
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

/// An ABORTED transaction must not leak either.
///
/// Distinct from the raw `BEGIN` above, and not covered by it: after a
/// statement fails inside a transaction PostgreSQL enters the failed-
/// transaction state, where every subsequent command is refused with `25P02
/// current transaction is aborted` until a ROLLBACK arrives. A borrower that
/// inherits that state cannot run anything at all -- the connection looks
/// alive and answers every query with the same error.
#[compio::test]
async fn an_aborted_transaction_does_not_leak_to_the_next_borrower() {
    let url = test_url();
    let config = PoolConfig {
        max_size: 1,
        min_idle: 1,
        ..PoolConfig::default()
    };
    let pool = match Pool::connect_with_config(&url, config).await {
        Ok(pool) => pool,
        Err(e) => common::postgres_unreachable(&url, &e),
    };

    // Borrower 1 poisons the session: the divide-by-zero aborts the
    // transaction, and the release happens with the server still in that
    // state.
    {
        let client = pool.get().await.expect("first checkout");
        let err = client
            .batch_execute("BEGIN; SELECT 1/0;")
            .await
            .expect_err("dividing by zero must fail");
        assert_eq!(
            err.code().map(compio_postgres::error::SqlState::code),
            Some("22012"),
            "expected division_by_zero to be what aborted the transaction"
        );
    }

    // Borrower 2 gets the same backend. An inherited aborted transaction
    // shows up as 25P02 on a statement that has nothing to do with the first.
    let client = pool.get().await.expect("second checkout");
    let rows = client
        .query("SELECT 1::int4 AS n", &[])
        .await
        .unwrap_or_else(|e| {
            panic!(
                "the next borrower inherited an aborted transaction: {}",
                common::error_chain(&e)
            )
        });
    assert_eq!(rows[0].get::<_, i32>("n"), 1);
}

/// A backend killed underneath the pool must not be handed out as if alive.
///
/// The connection is not "dirty" in the pool's sense -- nothing was left
/// queued on it -- so this exercises the alive-validation path rather than the
/// dirty barrier. `pg_terminate_backend` is issued from a SECOND connection,
/// because the pool is capped at one.
#[compio::test]
async fn a_terminated_backend_is_not_handed_to_the_next_borrower() {
    let url = test_url();
    let config = PoolConfig {
        max_size: 1,
        min_idle: 1,
        // Force the alive-check: without this the pool may skip validation on
        // a connection it saw moments ago, and the test would prove nothing.
        validation_bypass: std::time::Duration::ZERO,
        ..PoolConfig::default()
    };
    let pool = match Pool::connect_with_config(&url, config).await {
        Ok(pool) => pool,
        Err(e) => common::postgres_unreachable(&url, &e),
    };

    let pid: i32 = {
        let client = pool.get().await.expect("first checkout");
        let rows = client
            .query("SELECT pg_backend_pid()::int4 AS pid", &[])
            .await
            .expect("read the backend pid");
        rows[0].get("pid")
    };

    let (killer, connection) = compio_postgres::connect(&url, compio_postgres::NoTls)
        .await
        .expect("second connection to issue the terminate");
    let driver = compio::runtime::spawn(async move { connection.run().await });
    killer
        .execute("SELECT pg_terminate_backend($1)", &[&pid])
        .await
        .expect("terminate the pooled backend");

    // The pooled entry is now dead. A checkout must evict and replace it, not
    // hand back the corpse.
    let client = pool.get().await.expect("checkout after the backend was killed");
    let rows = client
        .query("SELECT pg_backend_pid()::int4 AS pid", &[])
        .await
        .unwrap_or_else(|e| {
            panic!(
                "the pool handed out the terminated backend: {}",
                common::error_chain(&e)
            )
        });
    assert_ne!(
        rows[0].get::<_, i32>("pid"),
        pid,
        "the pool reused the pid it had just watched die"
    );
    drop(driver);
}
