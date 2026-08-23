// `connect_pool` nests several `compio::time::timeout` wrappers around the
// pool handshake, and rustc computes the layout of that async body as one
// query chain. The default depth is not enough for it; without this the
// target fails to compile from a COLD cache, which incremental builds hide.
#![recursion_limit = "256"]

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
use futures_util::FutureExt;
use std::time::Duration;

mod common;

fn test_url() -> String {
    common::env::get(common::env::TestEnvKey::PgTestUrl)
        .unwrap_or_else(|| "postgres://postgres:zeroship@localhost:5440/zeroship".to_string())
}

const TEST_TIMEOUT: Duration = Duration::from_secs(30);
const POOL_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);

async fn connect_pool(url: &str, config: PoolConfig) -> Pool {
    match compio::time::timeout(
        POOL_CONNECT_TIMEOUT,
        Pool::connect_with_pool_config(url, config),
    )
    .await
    {
        Ok(Ok(pool)) => pool,
        Ok(Err(error)) => common::postgres_unreachable(url, &error),
        Err(_) => panic!(
            "pool connection exceeded its {} second timeout",
            POOL_CONNECT_TIMEOUT.as_secs()
        ),
    }
}

async fn drop_test_schema(pool: &Pool, schema: &str) -> Result<(), String> {
    let sql = format!(
        "ROLLBACK; SET lock_timeout = '4s'; SET statement_timeout = '4s'; \
         DROP SCHEMA IF EXISTS {schema} CASCADE"
    );
    let cleanup = async {
        let client = pool
            .get()
            .await
            .map_err(|error| common::error_chain(&error))?;
        client
            .batch_execute(&sql)
            .await
            .map_err(|error| common::error_chain(&error))
    }
    .boxed_local();
    compio::time::timeout(CLEANUP_TIMEOUT, cleanup)
    .await
    .map_err(|_| format!("dropping {schema} exceeded its cleanup timeout"))?
}

#[compio::test]
async fn a_raw_begin_does_not_leak_to_the_next_borrower() {
    let url = test_url();
    let schema = common::test_object_name("cpg_pool_tx_isolation");
    let table = common::test_object_name("cpg_pool_tx_isolation_table");
    let relation = format!("{schema}.{table}");
    // One connection, so the release and the next acquisition are guaranteed to
    // be the same backend - without that the test can pass by being handed a
    // different, clean connection.
    let mut config = PoolConfig::new();
    config.max_size(1).min_idle(1);
    let pool = connect_pool(&url, config).await;

    let outcome = match compio::time::timeout(
        TEST_TIMEOUT,
        std::panic::AssertUnwindSafe(async {
            {
                let client = pool.get().await.expect("first checkout");
                client
                    .batch_execute(&format!(
                        "DROP SCHEMA IF EXISTS {schema} CASCADE; CREATE SCHEMA {schema};
                         CREATE TABLE {relation} (id int);"
                    ))
                    .await
                    .expect("seed schema");
            }

            // Borrower 1 opens a transaction with RAW SQL - no `Transaction`
            // guard is involved, which is the case that has no other protection,
            // and releases without committing.
            let leaked_pid: i32 = {
                let client = pool.get().await.expect("second checkout");
                client
                    .batch_execute(&format!("BEGIN; INSERT INTO {relation} VALUES (1);"))
                    .await
                    .expect("open a raw transaction and write in it");
                let rows = client
                    .query("SELECT pg_backend_pid() AS pid", &[])
                    .await
                    .expect("read the backend pid inside the leaked transaction");
                rows[0].get("pid")
            };

            // Borrower 2 gets the same backend. If the transaction leaked it
            // sees its own uncommitted row and reports an assigned transaction
            // id.
            let client = pool.get().await.expect("third checkout");

            // THE SAME BACKEND, ASSERTED. The comment above says `max_size(1)`
            // guarantees this. It does not: evict-and-reconnect stays inside a
            // pool of one, and a pool that threw the dirty connection away and
            // dialled a fresh one would satisfy every assertion below while
            // never rolling anything back. Without this the test cannot tell
            // "the pool cleaned the leak" from "the pool replaced the
            // connection", which is the whole claim. Its sibling
            // `a_terminated_backend_is_not_handed_to_the_next_borrower` already
            // asserts the negative of this; the two isolation tests did not.
            let reused_pid: i32 = client
                .query("SELECT pg_backend_pid() AS pid", &[])
                .await
                .expect("read the next borrower's backend pid")[0]
                .get("pid");
            assert_eq!(
                reused_pid, leaked_pid,
                "the pool replaced the dirty connection instead of cleaning it, so nothing \
                 below is evidence that a raw BEGIN gets rolled back"
            );

            let rows = client
                .query(
                    &format!("SELECT count(*)::int8 AS n FROM {relation}"),
                    &[],
                )
                .await
                .expect("count rows");
            let visible: i64 = rows[0].get("n");
            // `.unwrap_or(false)` here until 2026-08-23, which made the probe's
            // FAILURE value identical to its PASS value: a borrower handed a
            // session inside an aborted transaction fails this query with
            // 25P02, and the test read that as "not in a transaction" and
            // passed. The fixture could not represent the difference it
            // asserts. A probe that cannot run is a failed test, not a false.
            let in_transaction: bool = client
                .query(
                    "SELECT (pg_current_xact_id_if_assigned() IS NOT NULL) AS b",
                    &[],
                )
                .await
                .expect("the transaction-state probe must run, not report false by failing")[0]
                .get("b");

            assert_eq!(
                visible, 0,
                "the next borrower inherited an uncommitted row, so the pool handed back \
                 a connection still inside a raw transaction"
            );
            assert!(
                !in_transaction,
                "the next borrower is inside a transaction it never opened"
            );
        })
        .catch_unwind(),
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(_) => Err(Box::new(format!(
            "pool transaction-isolation test exceeded its {TEST_TIMEOUT:?} timeout"
        )) as Box<dyn std::any::Any + Send>),
    };

    let cleanup = match std::panic::AssertUnwindSafe(drop_test_schema(&pool, &schema))
        .catch_unwind()
        .await
    {
        Ok(cleanup) => cleanup,
        Err(_) => Err(format!("cleanup for {schema} panicked")),
    };
    match outcome {
        Ok(()) => cleanup
            .unwrap_or_else(|error| panic!("failed to clean up {schema}: {error}")),
        Err(panic) => {
            if let Err(error) = cleanup {
                eprintln!("failed to clean up {schema} after test failure: {error}");
            }
            std::panic::resume_unwind(panic);
        }
    }
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
    let mut config = PoolConfig::new();
    config.max_size(1).min_idle(1);
    let pool = connect_pool(&url, config).await;

    // Borrower 1 poisons the session: the divide-by-zero aborts the
    // transaction, and the release happens with the server still in that
    // state.
    let poisoned_pid: i32 = {
        let client = pool.get().await.expect("first checkout");
        let pid: i32 = client
            .query("SELECT pg_backend_pid() AS pid", &[])
            .await
            .expect("read the backend pid before poisoning it")[0]
            .get("pid");
        let err = client
            .batch_execute("BEGIN; SELECT 1/0;")
            .await
            .expect_err("dividing by zero must fail");
        assert_eq!(
            err.code().map(compio_postgres::error::SqlState::code),
            Some("22012"),
            "expected division_by_zero to be what aborted the transaction"
        );
        pid
    };

    // Borrower 2 gets the same backend. An inherited aborted transaction
    // shows up as 25P02 on a statement that has nothing to do with the first.
    let client = pool.get().await.expect("second checkout");

    // THE SAME BACKEND, ASSERTED, for the reason recorded on
    // `a_raw_begin_does_not_leak_to_the_next_borrower`: `max_size(1)` does not
    // make the next borrower the same session, and a pool that discarded the
    // poisoned connection and dialled a fresh one would answer `SELECT 1`
    // perfectly while never having rolled anything back.
    let reused_pid: i32 = client
        .query("SELECT pg_backend_pid() AS pid", &[])
        .await
        .unwrap_or_else(|e| {
            panic!(
                "the next borrower inherited an aborted transaction: {}",
                common::error_chain(&e)
            )
        })[0]
        .get("pid");
    assert_eq!(
        reused_pid, poisoned_pid,
        "the pool replaced the poisoned connection instead of clearing it, so this test is \
         not evidence that an aborted transaction gets rolled back on release"
    );

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
    let mut config = PoolConfig::new();
    config
        .max_size(1)
        .min_idle(1)
        // Force the alive-check: without this the pool may skip validation on
        // a connection it saw moments ago, and the test would prove nothing.
        .validation_bypass(std::time::Duration::ZERO);
    let pool = connect_pool(&url, config).await;

    let pid: i32 = {
        let client = pool.get().await.expect("first checkout");
        let rows = client
            .query("SELECT pg_backend_pid()::int4 AS pid", &[])
            .await
            .expect("read the backend pid");
        rows[0].get("pid")
    };

    let (killer, connection) = compio::time::timeout(
        POOL_CONNECT_TIMEOUT,
        compio_postgres::connect(&url, compio_postgres::NoTls),
    )
    .await
    .expect("second connection exceeded its timeout")
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
