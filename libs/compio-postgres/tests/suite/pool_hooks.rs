//! Live coverage for pool connection lifecycle hooks.

use compio_postgres::config::TargetSessionAttrs;
use compio_postgres::error::SqlState;
use compio_postgres::{Config, Pool, PoolConfig};
use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

#[allow(unused_imports)]
use crate::common;

fn test_url() -> String {
    common::test_url()
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
                    .batch_execute("SET cpg_hooks_after_connect_failure.marker = 'poisoned'")
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
    assert!(
        row.get::<_, bool>(0),
        "replacement inherited failed hook state"
    );
    assert_eq!(calls.get(), 3);
    assert_eq!(pool.metrics.evictions.get(), 1);

    drop(replacement);
    drop(held);
}

/// A rejected candidate is replaced by the NEXT IDLE one, and the borrower
/// never receives the session the hook turned down.
///
/// Two warm connections, not one, so both the rejected candidate and its
/// replacement come out of the idle set. That is what keeps this a test of the
/// recycling path: with a single warm entry the replacement would be a freshly
/// opened connection, which `before_acquire` no longer inspects, and the second
/// hook call this asserts would never happen.
#[compio::test]
async fn before_acquire_false_discards_and_retries() {
    let url = test_url();
    let calls = Rc::new(Cell::new(0));
    let rejected_pid = Rc::new(Cell::new(None));
    let hook_calls = Rc::clone(&calls);
    let hook_rejected_pid = Rc::clone(&rejected_pid);
    let mut config = config(2, 2);
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
    assert!(
        row.get::<_, bool>(1),
        "borrower received the rejected session"
    );
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
    // Still 1: the cancelled checkout took the only warm entry with it
    // (`total_count` is 0 above), so this second checkout is served by a
    // freshly opened connection, which `before_acquire` does not inspect. The
    // point of this test is the capacity slot, asserted below and above -- the
    // hook count is incidental to it.
    assert_eq!(calls.get(), 1);
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
    assert_ne!(
        client.process_id(),
        first_pid,
        "rejected session was reused"
    );
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
    // NAME THE REFUSAL. `is_err()` plus `calls == 0` was the whole assertion
    // until 2026-08-23, and total failure satisfies both at once: an
    // unreachable server, a URL that does not parse, a pool-construction error
    // all produce an error AND a hook that never ran. The test would then have
    // asserted an ordering while its evidence was "nothing happened".
    let cause = common::error_chain(
        &outcome.expect_err("a writable server satisfied target_session_attrs=read-only"),
    );
    assert!(
        cause.contains("target session attributes"),
        "the pool failed for a reason other than the probe, so nothing here is about \
         ordering: {cause}"
    );
    assert_eq!(
        calls.get(),
        0,
        "after_connect ran on a connection the probe had already rejected"
    );

    // THE MIRROR, which is what turns "nothing happened" into evidence. One
    // variable changes -- the enum -- and against the same server the probe
    // must now pass, the connection must be produced, and the hook must run
    // exactly once. Without this arm a driver that could not connect at all
    // would satisfy everything above.
    let mut permissive: Config = url.parse().expect("parse the live PostgreSQL URL");
    permissive.target_session_attrs(TargetSessionAttrs::Any);
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
    let pool = Pool::connect_with_config(permissive, pool_config)
        .await
        .expect("target_session_attrs=any must accept the same writable server");
    let client = pool.get().await.expect("check out the accepted session");
    assert_eq!(
        calls.get(),
        1,
        "after_connect did not run for a connection the probe accepted, so the zero above \
         says nothing about ordering"
    );
    drop(client);
}

/// `before_acquire` is a RECYCLING check: it is not consulted for a connection
/// the pool just opened.
///
/// This is the contract `sqlx` states outright -- "This is _not_ invoked for
/// new connections. Use `after_connect` for those." -- and that `deadpool`
/// gets structurally by having `recycle` apply only to recycled objects. This
/// driver used to consult it on both, and the difference is not cosmetic.
///
/// WHAT THE OLD BEHAVIOUR COST. A freshly connected client has been accepted by
/// `after_connect` microseconds earlier, so a hook that answers from connection
/// state cannot answer differently for it -- but `Ok(false)` still hit the
/// acquisition loop's `continue`. With no idle entry to find, the next
/// iteration opened ANOTHER connection, offered it, was refused again, and so
/// on until `acquire_timeout`. Every iteration paid a full TCP connect plus
/// startup handshake, so a single `get()` became sustained load on the server.
/// MEASURED 2026-08-23 before the fix: 3 hook calls and 3 physical connections
/// inside a 300ms timeout -- about 10/s, extrapolating to roughly 300
/// connections for one `get()` at the default 30s `acquire_timeout`. That was a
/// LOWER bound; the box sat at load ~25, which slows each connect and so
/// lowers the count in a fixed window.
///
/// The hook that provokes it is not exotic. "Reject if the server is in
/// recovery" is a normal thing to write, and it is false for every connection
/// while a failover lasts.
///
/// WHY THIS TEST HAS NO TIMING IN IT. The old test could only report a RATE,
/// and it had to bound the storm with a short `acquire_timeout` to terminate at
/// all. The fixed contract is a deterministic statement instead: the hook is
/// never called, and the fresh client is handed over. It fails on the old code
/// by TIMING OUT rather than by measuring anything.
#[compio::test]
async fn before_acquire_is_not_consulted_for_a_freshly_connected_client() {
    let url = test_url();
    let calls = Rc::new(Cell::new(0));
    let hook_calls = Rc::clone(&calls);

    // `min_idle` of 0 still warms ONE connection -- `connect_with_config` uses
    // `min_idle.max(1)` so the constructor proves the connection settings. That
    // single warm entry is the recycled candidate the hook legitimately sees
    // below; the replacement for it is the fresh one that it must not see.
    let mut config = config(2, 0);
    // Short so that, on the pre-fix code, this test fails in under a second
    // instead of storming for the 30s default.
    config.acquire_timeout(Duration::from_millis(300));
    config.before_acquire(move |_client| {
        let hook_calls = Rc::clone(&hook_calls);
        Box::pin(async move {
            hook_calls.set(hook_calls.get() + 1);
            Ok(false)
        })
    });
    let pool = connect_pool(&url, config).await;

    let client = pool.get().await.expect(
        "a rejecting before_acquire must not block the FRESH connection opened to replace the \
         candidate it rejected: the hook is a recycling check, and consulting it here reopens \
         until acquire_timeout",
    );

    assert_eq!(
        calls.get(),
        1,
        "the hook must see the one recycled candidate and nothing else; a second call means the \
         freshly opened replacement was offered to it too"
    );
    assert_eq!(
        pool.metrics.connections_created.get(),
        2,
        "the warm-up connection plus its replacement"
    );
    assert_eq!(pool.metrics.evictions.get(), 1);

    // It is a usable client, not merely a returned handle.
    let row = client
        .query_one("SELECT 1::int4", &[])
        .await
        .expect("the handed-out connection must work");
    assert_eq!(row.get::<_, i32>(0), 1);
    drop(client);
}
