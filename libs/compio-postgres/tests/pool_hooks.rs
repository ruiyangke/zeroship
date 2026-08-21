//! Live coverage for pool connection lifecycle hooks.

use compio_postgres::error::SqlState;
use compio_postgres::config::TargetSessionAttrs;
use compio_postgres::{Config, Pool, PoolConfig};
use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

#[allow(dead_code)]
mod common;

fn test_url() -> String {
    common::env::get(common::env::TestEnvKey::PgTestUrl)
        .unwrap_or_else(|| "postgres://postgres:zeroship@localhost:5440/zeroship".to_string())
}

fn config(max_size: usize, min_idle: usize) -> PoolConfig {
    let mut config = PoolConfig::new();
    config
        .max_size(max_size)
        .min_idle(min_idle)
        .validation_bypass(Duration::from_secs(60));
    config
}

async fn connect_pool(url: &str, config: PoolConfig) -> Pool {
    Pool::connect_with_pool_config(url, config)
        .await
        .unwrap_or_else(|error| common::postgres_unreachable(url, &error))
}

#[compio::test]
async fn after_connect_runs_once_for_a_reused_connection() {
    let url = test_url();
    let calls = Rc::new(Cell::new(0));
    let hook_calls = Rc::clone(&calls);
    let mut config = config(1, 1);
    config.after_connect(move |client| {
        let hook_calls = Rc::clone(&hook_calls);
        Box::pin(async move {
            client.simple_query("").await?;
            hook_calls.set(hook_calls.get() + 1);
            Ok(())
        })
    });
    let pool = connect_pool(&url, config).await;

    let mut backend_pid = None;
    for _ in 0..4 {
        let client = pool.get().await.unwrap();
        match backend_pid {
            Some(expected) => assert_eq!(client.process_id(), expected),
            None => backend_pid = Some(client.process_id()),
        }
    }

    assert_eq!(
        calls.get(),
        1,
        "after_connect ran per checkout instead of per physical connection"
    );
}

#[compio::test]
async fn after_connect_initializes_a_session_guc() {
    let url = test_url();
    let mut config = config(1, 1);
    config.after_connect(|client| {
        Box::pin(async move {
            client
                .batch_execute("SET cpg_hooks_after_connect.marker = 'installed'")
                .await
        })
    });
    let pool = connect_pool(&url, config).await;

    let rows = pool
        .query(
            "SELECT current_setting('cpg_hooks_after_connect.marker')",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(rows[0].get::<_, &str>(0), "installed");
}

#[compio::test]
async fn after_connect_failure_discards_the_connection() {
    let url = test_url();
    let calls = Rc::new(Cell::new(0));
    let failed_pid = Rc::new(Cell::new(None));
    let hook_calls = Rc::clone(&calls);
    let hook_failed_pid = Rc::clone(&failed_pid);
    let mut config = config(2, 1);
    config.after_connect(move |client| {
        let invocation = hook_calls.get() + 1;
        hook_calls.set(invocation);
        if invocation == 2 {
            hook_failed_pid.set(Some(client.process_id()));
        }
        Box::pin(async move {
            if invocation == 2 {
                client
                    .batch_execute(
                        "SET cpg_hooks_after_connect_failure.marker = 'poisoned'",
                    )
                    .await?;
                client.batch_execute("SELECT 1 / 0").await
            } else {
                client.simple_query("").await.map(|_| ())
            }
        })
    });
    let pool = connect_pool(&url, config).await;
    let held = pool.get().await.unwrap();

    let error = pool
        .get()
        .await
        .expect_err("a connection whose after_connect failed was handed out");
    assert_eq!(error.code(), Some(&SqlState::DIVISION_BY_ZERO));
    assert_eq!(pool.total_count(), 1, "failed connection leaked a slot");
    assert_eq!(pool.active_count(), 1, "failed connection became active");

    let replacement = pool.get().await.unwrap();
    let rejected_pid = failed_pid
        .get()
        .expect("the failing hook did not record its backend");
    assert_ne!(
        replacement.process_id(),
        rejected_pid,
        "the failed connection was reused"
    );
    let row = replacement
        .query_one(
            "SELECT current_setting(\
                 'cpg_hooks_after_connect_failure.marker', true\
             ) IS NULL",
            &[],
        )
        .await
        .unwrap();
    assert!(row.get::<_, bool>(0), "replacement inherited failed hook state");
    assert_eq!(calls.get(), 3);
    assert_eq!(pool.metrics.evictions.get(), 1);

    drop(replacement);
    drop(held);
}

#[compio::test]
async fn before_acquire_false_discards_and_retries() {
    let url = test_url();
    let calls = Rc::new(Cell::new(0));
    let rejected_pid = Rc::new(Cell::new(None));
    let hook_calls = Rc::clone(&calls);
    let hook_rejected_pid = Rc::clone(&rejected_pid);
    let mut config = config(1, 1);
    config.before_acquire(move |client| {
        let hook_calls = Rc::clone(&hook_calls);
        let hook_rejected_pid = Rc::clone(&hook_rejected_pid);
        Box::pin(async move {
            let invocation = hook_calls.get() + 1;
            hook_calls.set(invocation);
            if invocation == 1 {
                client
                    .batch_execute("SET cpg_hooks_before_acquire.marker = 'rejected'")
                    .await?;
                hook_rejected_pid.set(Some(client.process_id()));
                Ok(false)
            } else {
                client.simple_query("").await?;
                Ok(true)
            }
        })
    });
    let pool = connect_pool(&url, config).await;

    let client = pool.get().await.unwrap();
    let rejected_pid = rejected_pid
        .get()
        .expect("before_acquire did not inspect the first candidate");
    assert_ne!(client.process_id(), rejected_pid);
    assert_eq!(calls.get(), 2, "replacement skipped before_acquire");
    assert_eq!(pool.total_count(), 1);
    assert_eq!(pool.active_count(), 1);
    assert_eq!(pool.idle_count(), 0);
    assert_eq!(pool.metrics.connections_created.get(), 2);
    assert_eq!(pool.metrics.evictions.get(), 1);
    let row = client
        .query_one(
            "SELECT 42::int4, \
             current_setting('cpg_hooks_before_acquire.marker', true) IS NULL",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, i32>(0), 42);
    assert!(row.get::<_, bool>(1), "borrower received the rejected session");
}

#[compio::test]
async fn cancelling_an_async_hook_releases_its_capacity_slot() {
    let url = test_url();
    let calls = Rc::new(Cell::new(0));
    let hook_calls = Rc::clone(&calls);
    let mut pool_config = config(1, 1);
    pool_config.before_acquire(move |client| {
        let invocation = hook_calls.get() + 1;
        hook_calls.set(invocation);
        Box::pin(async move {
            if invocation == 1 {
                compio::time::sleep(Duration::from_secs(5)).await;
            }
            client.simple_query("").await?;
            Ok(true)
        })
    });
    pool_config.acquire_timeout(Duration::from_secs(1));
    let pool = connect_pool(&url, pool_config).await;

    pool.get()
        .await
        .expect_err("checkout outlived its acquire_timeout inside a hook");
    assert_eq!(calls.get(), 1);
    assert_eq!(pool.active_count(), 0);
    assert_eq!(pool.idle_count(), 0);
    assert_eq!(pool.total_count(), 0, "cancelled hook leaked its slot");
    assert_eq!(pool.metrics.timeouts.get(), 1);

    let client = pool.get().await.unwrap();
    assert_eq!(calls.get(), 2);
    assert_eq!(pool.total_count(), 1);
    assert_eq!(pool.active_count(), 1);
    let row = client.query_one("SELECT 7::int4", &[]).await.unwrap();
    assert_eq!(row.get::<_, i32>(0), 7);
}

#[compio::test]
async fn after_release_false_discards_the_dirty_session() {
    let url = test_url();
    let calls = Rc::new(Cell::new(0));
    let hook_calls = Rc::clone(&calls);
    let mut config = config(1, 1);
    config.after_release(move |_client| {
        hook_calls.set(hook_calls.get() + 1);
        false
    });
    let pool = connect_pool(&url, config).await;

    let first_pid = {
        let client = pool.get().await.unwrap();
        client
            .batch_execute("SET cpg_hooks_after_release.marker = 'dirty'")
            .await
            .unwrap();
        client.process_id()
    };

    assert_eq!(calls.get(), 1, "after_release did not run on return");
    assert_eq!(pool.active_count(), 0);
    assert_eq!(pool.idle_count(), 0, "rejected connection became idle");
    assert_eq!(pool.total_count(), 0, "rejected connection kept its slot");
    assert_eq!(pool.metrics.evictions.get(), 1);

    let client = pool.get().await.unwrap();
    assert_ne!(client.process_id(), first_pid, "rejected session was reused");
    let row = client
        .query_one(
            "SELECT current_setting('cpg_hooks_after_release.marker', true) IS NULL",
            &[],
        )
        .await
        .unwrap();
    assert!(row.get::<_, bool>(0), "next borrower inherited dirty state");
    assert_eq!(calls.get(), 1, "after_release ran before the second return");
    drop(client);
    assert_eq!(calls.get(), 2, "after_release missed the second return");
}

/// The `target_session_attrs` probe runs BEFORE `after_connect`.
///
/// Both landed the same day and both hook into connection setup, so the
/// ordering is easy to invert and nothing else would notice: `connect_one`
/// goes through `Config::connect`, which runs the probe inside `connect_raw`
/// before the Connection is packaged, and only then does the pool run its
/// hook.
///
/// Getting this backwards would spend session setup -- `SET ROLE`,
/// `search_path`, a `statement_timeout` -- on a host that is about to be
/// discarded for failing the probe. Asserted by demanding `read-only` from a
/// writable server: the probe must reject it, and the hook must never see it.
#[compio::test]
async fn the_session_attrs_probe_runs_before_after_connect() {
    let url = test_url();
    let mut connection_config: Config = url.parse().expect("parse the live PostgreSQL URL");
    connection_config.target_session_attrs(TargetSessionAttrs::ReadOnly);
    let calls = Rc::new(Cell::new(0));
    let hook_calls = Rc::clone(&calls);

    let mut pool_config = config(1, 1);
    pool_config.after_connect(move |_client| {
        let hook_calls = Rc::clone(&hook_calls);
        Box::pin(async move {
            hook_calls.set(hook_calls.get() + 1);
            Ok(())
        })
    });

    // The live server is writable, so every candidate fails the read-only
    // requirement and no connection is ever produced.
    let outcome = Pool::connect_with_config(connection_config, pool_config).await;
    assert!(
        outcome.is_err(),
        "a writable server satisfied target_session_attrs=read-only"
    );
    assert_eq!(
        calls.get(),
        0,
        "after_connect ran on a connection the probe had already rejected"
    );
}
