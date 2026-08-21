//! Live PostgreSQL coverage for the pool's client command deadline.
//!
//! These tests hold one `PooledClient` across the timeout and the follow-up
//! query. A replacement connection would hide response-drain bugs, which are
//! the highest-risk failure mode for an out-of-band CancelRequest.

use compio_postgres::error::SqlState;
use compio_postgres::{Config, Pool, PoolConfig};
use std::time::{Duration, Instant};

#[allow(dead_code)]
mod common;

const OUTER_WATCHDOG: Duration = Duration::from_secs(5);

fn test_url() -> String {
    let url = common::env::get(common::env::TestEnvKey::PgTestUrl)
        .unwrap_or_else(|| "postgres://postgres:zeroship@localhost:5440/zeroship".to_string());
    let separator = if url.contains('?') { '&' } else { '?' };
    format!("{url}{separator}sslmode=disable")
}

async fn connect_pool(command_timeout: Duration) -> Pool {
    let url = test_url();
    let connection_config: Config = url.parse().expect("parse PG_TEST_URL");
    let mut pool_config = PoolConfig::new();
    pool_config
        .max_size(1)
        .min_idle(1)
        .command_timeout(command_timeout);

    Pool::connect_with_config(connection_config, pool_config)
        .await
        .unwrap_or_else(|error| common::postgres_unreachable(&url, &error))
}

#[test]
fn command_timeout_is_opt_in_pool_policy() {
    let mut config = PoolConfig::new();
    assert_eq!(config.get_command_timeout(), None);

    config.command_timeout(Duration::from_millis(250));
    assert_eq!(
        config.get_command_timeout(),
        Some(Duration::from_millis(250))
    );
}

#[compio::test]
async fn overrun_is_cancelled_and_the_same_client_remains_usable() {
    compio::time::timeout(OUTER_WATCHDOG, async {
        let pool = connect_pool(Duration::from_millis(100)).await;
        let mut client = pool.get().await.expect("check out the only connection");
        let backend_pid = client.process_id();

        let started = Instant::now();
        let error = client
            .command(async |client| client.query("SELECT 1::int4 FROM pg_sleep(3)", &[]).await)
            .await
            .expect_err("pg_sleep outlived the client command deadline");

        assert!(
            error.is_command_timeout(),
            "deadline must have its own error classification, got {error:?}"
        );
        assert_eq!(
            error.code(),
            None,
            "a client deadline must not masquerade as PostgreSQL's 57014"
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the local timer fired, but no CancelRequest interrupted pg_sleep"
        );

        let row = client
            .query_one("SELECT pg_backend_pid(), 42::int4", &[])
            .await
            .expect("the timed-out client's response stream was not drained");
        assert_eq!(row.get::<_, i32>(0), backend_pid);
        assert_eq!(row.get::<_, i32>(1), 42);
    })
    .await
    .expect("timeout cancellation or same-client recovery hung");
}

#[compio::test]
async fn timeout_inside_raw_transaction_rolls_back_before_same_client_reuse() {
    compio::time::timeout(OUTER_WATCHDOG, async {
        let pool = connect_pool(Duration::from_millis(100)).await;
        let mut client = pool.get().await.expect("check out the only connection");
        let backend_pid = client.process_id();

        let error = client
            .command(async |client| {
                client
                    .batch_execute("BEGIN; SELECT pg_sleep(3)")
                    .await
            })
            .await
            .expect_err("the command inside BEGIN outlived its deadline");
        assert!(error.is_command_timeout());

        let row = client
            .query_one("SELECT pg_backend_pid(), 42::int4", &[])
            .await
            .expect("timeout recovery left the held client in failed transaction state");
        assert_eq!(row.get::<_, i32>(0), backend_pid);
        assert_eq!(row.get::<_, i32>(1), 42);
    })
    .await
    .expect("transaction timeout recovery hung");
}

#[compio::test]
async fn command_inside_the_deadline_is_untouched() {
    compio::time::timeout(OUTER_WATCHDOG, async {
        let pool = connect_pool(Duration::from_millis(750)).await;
        let mut client = pool.get().await.expect("check out the only connection");
        let backend_pid = client.process_id();

        let row = client
            .command(async |client| {
                client
                    .query_one("SELECT pg_backend_pid(), 7::int4 FROM pg_sleep(0.01)", &[])
                    .await
            })
            .await
            .expect("an in-budget command was cancelled");

        assert_eq!(row.get::<_, i32>(0), backend_pid);
        assert_eq!(row.get::<_, i32>(1), 7);
        assert!(!client.is_closed());
    })
    .await
    .expect("an in-budget command exceeded the outer test watchdog");
}

#[compio::test]
async fn direct_pooled_client_query_does_not_enter_command_scope() {
    compio::time::timeout(OUTER_WATCHDOG, async {
        let pool = connect_pool(Duration::from_millis(50)).await;
        let client = pool.get().await.expect("check out the only connection");
        let backend_pid = client.process_id();

        let started = Instant::now();
        let row = client
            .query_one(
                "SELECT pg_backend_pid(), 42::int4 FROM pg_sleep(0.20)",
                &[],
            )
            .await
            .expect("a direct pooled-client query inherited the command deadline");

        assert!(
            started.elapsed() >= Duration::from_millis(150),
            "the query did not expose three configured command budgets"
        );
        assert_eq!(row.get::<_, i32>(0), backend_pid);
        assert_eq!(row.get::<_, i32>(1), 42);
        assert!(!client.is_closed());
    })
    .await
    .expect("direct pooled-client deadline control exceeded its watchdog");
}

#[compio::test]
async fn server_statement_timeout_remains_a_server_error() {
    compio::time::timeout(OUTER_WATCHDOG, async {
        let pool = connect_pool(Duration::from_millis(750)).await;
        let mut client = pool.get().await.expect("check out the only connection");

        let error = client
            .command(async |client| {
                client
                    .batch_execute(
                        "SET statement_timeout = '50ms'; \
                         SELECT pg_sleep(3)",
                    )
                    .await
            })
            .await
            .expect_err("PostgreSQL statement_timeout did not fire");

        assert_eq!(error.code(), Some(&SqlState::QUERY_CANCELED));
        assert!(
            !error.is_command_timeout(),
            "PostgreSQL's 57014 must not be classified as the client deadline"
        );
    })
    .await
    .expect("server statement_timeout exceeded the outer test watchdog");
}

#[compio::test]
async fn pool_convenience_queries_enter_the_command_scope() {
    compio::time::timeout(OUTER_WATCHDOG, async {
        let pool = connect_pool(Duration::from_millis(100)).await;
        let before = pool
            .query("SELECT pg_backend_pid()", &[])
            .await
            .expect("read the warm connection's backend PID")[0]
            .get::<_, i32>(0);

        let error = pool
            .query("SELECT 1::int4 FROM pg_sleep(3)", &[])
            .await
            .expect_err("Pool::query bypassed the configured command deadline");
        assert!(error.is_command_timeout());

        let after = pool
            .query("SELECT pg_backend_pid()", &[])
            .await
            .expect("the pool convenience path did not drain its response")[0]
            .get::<_, i32>(0);
        assert_eq!(
            after, before,
            "Pool::query discarded a session whose cancellation recovered cleanly"
        );
    })
    .await
    .expect("pool convenience command cancellation hung");
}
