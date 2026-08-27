//! What happens when PostgreSQL kills the backend out from under a live
//! operation.
//!
//! Termination is already covered twice in this suite -- a backend that
//! terminates ITSELF during a `simple_query` (`tests/integration.rs`), and one
//! terminated BETWEEN pool checkouts so the pool's alive-validation path runs
//! (`tests/pool_transaction_isolation.rs`). Neither reaches a backend that dies
//! while an operation is mid-flight, and that is where this driver's bugs have
//! actually lived: the COPY producer teardown, the paused read obligation, the
//! split-reader accounting. Those states carry the most machinery and unwind
//! the least often.
//!
//! A server-side kill is also the one trigger a scripted peer cannot reproduce
//! honestly. The hostile-peer suite ends a connection by closing a socket on
//! cue; this arrives as a real RST against a real in-flight request, with the
//! server's own ErrorResponse racing it.
//!
//! THE LEAK CHECK IS THE ASSERTION THAT MATTERS, not the error. "The call
//! returned an error" is satisfied by a driver that errors AND strands the
//! socket, which is the exact shape the bugs in this area had. Each test
//! therefore pins three outcomes apart: a hang (the watchdog), a false success
//! (the error assertion), and a leak (`live_connections` back at baseline).
//!
//! WHAT THESE DO NOT COVER: a backend that dies without sending anything (a
//! `SIGKILL` of the postmaster child, or the network dropping), which arrives
//! as a bare RST with no ErrorResponse racing it. `pg_terminate_backend` is a
//! polite kill and the server does try to say so first.

use bytes::Bytes;
use compio_postgres::error::SqlState;
use compio_postgres::{Client, Config, Pool, PoolConfig};
use futures_util::{SinkExt, TryStreamExt};
use std::time::{Duration, Instant};

#[allow(unused_imports)]
use crate::common;

/// Long enough that a loaded machine cannot trip it, short enough that a real
/// hang fails this test rather than the whole run. Deliberately generous: see
/// the load table on
/// `read_timeout::copy_input_time_is_not_charged_as_server_read_silence` for
/// what a tight wall-clock budget costs against a live server.
const WATCHDOG: Duration = Duration::from_secs(20);
const POOL_WIDTH: usize = 3;
const RECOVERY_ROUNDS: usize = 4;
const SETTLE_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(5);

fn test_url() -> String {
    common::test_url()
}

/// Kill `pid` from a second session, because the first one is busy.
async fn terminate(killer: &Client, pid: i32) {
    killer
        .execute("SELECT pg_terminate_backend($1)", &[&pid])
        .await
        .expect("terminate the victim backend");
}

async fn tagged_backend_pids(observer: &Client, application_name: &str) -> Vec<i32> {
    observer
        .query(
            "SELECT pid::int4
             FROM pg_stat_activity
             WHERE datname = current_database()
               AND application_name = $1
               AND pid <> pg_backend_pid()
             ORDER BY pid",
            &[&application_name],
        )
        .await
        .expect("list the pool's tagged PostgreSQL backends")
        .iter()
        .map(|row| row.get(0))
        .collect()
}

async fn wait_for_no_tagged_backends(observer: &Client, application_name: &str) -> Vec<i32> {
    let deadline = Instant::now() + SETTLE_TIMEOUT;
    loop {
        let pids = tagged_backend_pids(observer, application_name).await;
        if pids.is_empty() || Instant::now() >= deadline {
            return pids;
        }
        compio::time::sleep(POLL_INTERVAL).await;
    }
}

async fn wait_for_live_connections(expected: usize) -> usize {
    let deadline = Instant::now() + SETTLE_TIMEOUT;
    loop {
        let observed = compio_postgres::live_connections();
        if observed == expected || Instant::now() >= deadline {
            return observed;
        }
        compio::time::sleep(POLL_INTERVAL).await;
    }
}

async fn terminate_tagged_backend(killer: &Client, application_name: &str, pid: i32) -> i32 {
    let terminated = killer
        .query(
            "SELECT pid::int4, pg_terminate_backend(pid)
             FROM pg_stat_activity
             WHERE datname = current_database()
               AND application_name = $1
               AND pid = $2
               AND pid <> pg_backend_pid()",
            &[&application_name, &pid],
        )
        .await
        .expect("terminate the tagged pooled backend");
    assert_eq!(
        terminated.len(),
        1,
        "the application_name and PID selector did not resolve to exactly one owned backend"
    );

    let selected_pid: i32 = terminated[0].get(0);
    let was_terminated: bool = terminated[0].get(1);
    assert_eq!(selected_pid, pid);
    assert!(
        was_terminated,
        "PostgreSQL did not terminate tagged backend {pid}"
    );
    selected_pid
}

async fn wait_for_tagged_pg_sleep(
    observer: &Client,
    application_name: &str,
    pid: i32,
    marker: &str,
) {
    compio::time::timeout(SETTLE_TIMEOUT, async {
        loop {
            let running: bool = observer
                .query_one_scalar(
                    "SELECT EXISTS (
                         SELECT 1
                         FROM pg_stat_activity
                         WHERE datname = current_database()
                           AND application_name = $1
                           AND pid = $2
                           AND state = 'active'
                           AND wait_event_type = 'Timeout'
                           AND wait_event = 'PgSleep'
                           AND position($3 in query) > 0
                     )",
                    &[&application_name, &pid, &marker],
                )
                .await
                .expect("poll pg_stat_activity for the tagged pooled query");
            if running {
                return;
            }
            compio::time::sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .expect("the tagged pooled query never reached pg_sleep");
}

fn assert_dead_lease_was_discarded(pool: &Pool) -> (usize, usize) {
    let idle_after_drop = pool.idle_count();
    let total_after_drop = pool.total_count();
    assert_eq!(pool.active_count(), 0, "the dead lease remained active");
    assert_eq!(
        idle_after_drop, 0,
        "the dead lease was returned to the idle set"
    );
    assert_eq!(
        total_after_drop, 0,
        "the dead lease kept its pool capacity slot"
    );
    assert_eq!(pool.metrics.connections_created.get(), 1);
    assert_eq!(pool.metrics.evictions.get(), 1);

    (idle_after_drop, total_after_drop)
}

async fn assert_replacement_uses_a_new_backend(pool: &Pool, killed_pid: i32) -> i32 {
    let replacement = pool
        .get()
        .await
        .expect("check out a replacement for the dead pooled backend");
    let replacement_pid: i32 = replacement
        .query_one_scalar("SELECT pg_backend_pid()::int4", &[])
        .await
        .unwrap_or_else(|error| {
            panic!(
                "the next checkout did not receive a working connection: {}",
                common::error_chain(&error)
            )
        });
    assert_ne!(
        replacement_pid, killed_pid,
        "the next checkout ran on the backend that PostgreSQL terminated"
    );
    drop(replacement);

    assert_eq!(pool.active_count(), 0);
    assert_eq!(pool.idle_count(), 1);
    assert_eq!(pool.total_count(), 1);
    assert_eq!(pool.metrics.connections_created.get(), 2);
    assert_eq!(pool.metrics.evictions.get(), 1);

    replacement_pid
}

/// A backend killed while a COPY is streaming must fail the COPY and give the
/// socket back.
#[compio::test]
async fn a_backend_killed_mid_copy_fails_the_copy_and_releases_the_connection() {
    let url = test_url();
    let baseline = compio_postgres::live_connections();

    let (victim, victim_connection) =
        match compio_postgres::connect(&url, common::suite_tls()).await {
            Ok(pair) => pair,
            Err(error) => common::postgres_unreachable(&url, &error),
        };
    let victim_driver = compio::runtime::spawn(async move { victim_connection.run().await });
    let (killer, killer_connection) =
        match compio_postgres::connect(&url, common::suite_tls()).await {
            Ok(pair) => pair,
            Err(error) => common::postgres_unreachable(&url, &error),
        };
    let killer_driver = compio::runtime::spawn(async move { killer_connection.run().await });

    let pid: i32 = victim
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .expect("read the victim's backend pid")
        .get(0);

    victim
        .batch_execute("CREATE TEMPORARY TABLE cpg_kill_copy (n int)")
        .await
        .expect("create the COPY target");
    let sink = victim
        .copy_in::<_, Bytes>("COPY cpg_kill_copy (n) FROM STDIN")
        .await
        .expect("enter COPY input mode");
    let mut sink = std::pin::pin!(sink);
    sink.as_mut()
        .send(Bytes::from_static(b"1\n"))
        .await
        .expect("the first COPY row precedes the kill");

    terminate(&killer, pid).await;

    // `send` may well succeed after the kill -- it writes into a local buffer
    // and the RST has not necessarily arrived -- so the outcome under test is
    // the PAIR, not either half. What must not happen is the COPY reporting
    // rows committed to a backend that no longer exists.
    let outcome = compio::time::timeout(WATCHDOG, async {
        let _ = sink.as_mut().send(Bytes::from_static(b"2\n")).await;
        sink.as_mut().finish().await
    })
    .await
    .expect("the COPY hung after its backend was terminated");
    assert!(
        outcome.is_err(),
        "the COPY reported success against a terminated backend: {outcome:?}"
    );

    drop(victim);
    let _ = victim_driver.await;
    drop(killer);
    let _ = killer_driver.await;
    assert_eq!(
        compio_postgres::live_connections(),
        baseline,
        "a backend killed mid-COPY stranded its socket"
    );
}

/// A server ErrorResponse that arrived before COPY's producer disconnected is
/// the diagnosis. `finish` must not replace its SQLSTATE with a local closed
/// error merely because the connection task has retired by the time it polls.
///
/// RED: `CopyInSink::poll_finish` first polls its disconnected sender and
/// returns `Error::closed()` without reading the already queued FATAL response.
#[compio::test]
async fn copy_finish_preserves_a_queued_fatal_response() {
    compio::time::timeout(WATCHDOG, async {
        let url = test_url();
        let baseline = compio_postgres::live_connections();

        let (victim, victim_connection) =
            match compio_postgres::connect(&url, common::suite_tls()).await {
                Ok(pair) => pair,
                Err(error) => common::postgres_unreachable(&url, &error),
            };
        let victim_driver = compio::runtime::spawn(async move { victim_connection.run().await });
        let (killer, killer_connection) =
            match compio_postgres::connect(&url, common::suite_tls()).await {
                Ok(pair) => pair,
                Err(error) => common::postgres_unreachable(&url, &error),
            };
        let killer_driver = compio::runtime::spawn(async move { killer_connection.run().await });

        let pid: i32 = victim
            .query_one_scalar("SELECT pg_backend_pid()::int4", &[])
            .await
            .expect("read the victim's backend pid");
        victim
            .batch_execute("CREATE TEMPORARY TABLE cpg_copy_fatal (n int)")
            .await
            .expect("create the COPY target");

        let sink = victim
            .copy_in::<_, Bytes>("COPY cpg_copy_fatal (n) FROM STDIN")
            .await
            .expect("enter COPY input mode");
        let mut sink = std::pin::pin!(sink);
        sink.as_mut()
            .send(Bytes::from_static(b"1\n"))
            .await
            .expect("send a row before terminating the backend");

        terminate(&killer, pid).await;
        compio::time::timeout(SETTLE_TIMEOUT, async {
            while !victim.is_closed() {
                compio::time::sleep(POLL_INTERVAL).await;
            }
        })
        .await
        .expect("the terminated COPY connection did not retire");

        let error = sink
            .as_mut()
            .finish()
            .await
            .expect_err("COPY completed after PostgreSQL terminated its backend");
        if error.code() != Some(&SqlState::ADMIN_SHUTDOWN) {
            panic!(
                "COPY IN lost SQLSTATE 57P01 after backend termination: {}",
                common::error_chain(&error)
            );
        }

        drop(victim);
        let _ = victim_driver.await;
        drop(killer);
        let _ = killer_driver.await;
        assert_eq!(
            compio_postgres::live_connections(),
            baseline,
            "the queued-FATAL COPY test stranded a connection"
        );
    })
    .await
    .expect("the queued-FATAL COPY test exceeded its watchdog");
}

/// A backend killed while rows are still streaming must fail the stream and
/// give the socket back.
#[compio::test]
async fn a_backend_killed_mid_row_stream_fails_the_stream_and_releases_the_connection() {
    let url = test_url();
    let baseline = compio_postgres::live_connections();

    let (victim, victim_connection) =
        match compio_postgres::connect(&url, common::suite_tls()).await {
            Ok(pair) => pair,
            Err(error) => common::postgres_unreachable(&url, &error),
        };
    let victim_driver = compio::runtime::spawn(async move { victim_connection.run().await });
    let (killer, killer_connection) =
        match compio_postgres::connect(&url, common::suite_tls()).await {
            Ok(pair) => pair,
            Err(error) => common::postgres_unreachable(&url, &error),
        };
    let killer_driver = compio::runtime::spawn(async move { killer_connection.run().await });

    let pid: i32 = victim
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .expect("read the victim's backend pid")
        .get(0);

    // BOTH HALVES OF THIS QUERY ARE LOAD-BEARING, and each replaces a version
    // that made the test pass while proving nothing.
    //
    // `pg_sleep` keeps the server mid-production, so the kill lands during
    // execution rather than after it. It must be in the SELECT LIST: the first
    // version used `WHERE pg_sleep(0.02) IS NULL`, and `void IS NULL` is FALSE,
    // so the predicate filtered out every row. The stream yielded nothing, a
    // second poll of the exhausted stream returned an error, and the assertion
    // below held whether or not the backend was ever killed.
    //
    // `repeat('x', 100000)` is what makes the rows actually STREAM. PostgreSQL
    // buffers its output and, with small rows, flushes nothing until the query
    // completes -- measured: time-to-first-row was 11us for a query taking
    // 4020ms, i.e. the whole result set landed at once and the kill hit an
    // already-finished backend, delivering 200 of 200 rows. At 100 KB per row
    // the buffer fills repeatedly and the first row arrives in ~20ms, so the
    // remaining rows are genuinely still in flight.
    //
    // Only the no-kill control caught either of these.
    let stream = victim
        .query_raw(
            "SELECT repeat('x', 100000), pg_sleep(0.02) FROM generate_series(1, 100) g",
            std::iter::empty::<i32>(),
        )
        .await
        .expect("start the row stream");
    let mut stream = std::pin::pin!(stream);

    let first = stream
        .try_next()
        .await
        .expect("the first row precedes the kill");
    assert!(
        first.is_some(),
        "the query yielded no rows at all, so nothing below is about termination"
    );

    terminate(&killer, pid).await;

    let mut delivered = 1usize;
    let outcome = compio::time::timeout(WATCHDOG, async {
        loop {
            match stream.try_next().await {
                Ok(Some(_)) => {
                    delivered += 1;
                    continue;
                }
                Ok(None) => return Ok(()),
                Err(error) => return Err(error),
            }
        }
    })
    .await
    .expect("the row stream hung after its backend was terminated");
    assert!(
        outcome.is_err(),
        "the row stream ended cleanly against a terminated backend after \
         {delivered} of 100 rows: a truncated result set reported as success"
    );

    drop(victim);
    let _ = victim_driver.await;
    drop(killer);
    let _ = killer_driver.await;
    assert_eq!(
        compio_postgres::live_connections(),
        baseline,
        "a backend killed mid-stream stranded its socket"
    );
}

/// Every backend killed while IDLE in one pool must be replaced on checkout.
///
/// This is the idle-then-explicitly-checked-out case. The two tests above kill
/// live non-pooled connections while they are checked out and mid-operation;
/// the tests below cover pooled connections killed while checked out.
///
/// IT DOES NOT REST ON THE ALIVE-CHECK, which is worth knowing because this
/// test leaves `PoolConfig::validation_bypass` at its 500 ms default and a
/// reader would reasonably suspect it only passes because the kill took longer
/// than that. MEASURED 2026-08-24: re-run with the bypass set to 600 SECONDS -
/// so every checkout skips validation - it still passes. A connection handed
/// out dead under the bypass fails on first use and the pool evicts and
/// retries, so recovery here is a property of the pool rather than of the
/// timing this test happens to produce.
#[compio::test]
async fn all_idle_pool_backends_are_replaced_after_mass_termination() {
    compio::time::timeout(WATCHDOG, async {
        let url = test_url();
        let baseline = compio_postgres::live_connections();
        let application_name = common::test_object_name("cpg_pool_mass_recovery");
        let killer_application_name = common::test_object_name("cpg_pool_mass_recovery_killer");

        let mut connection_config: Config = url.parse().expect("the test DSN did not parse");
        connection_config.application_name(&application_name);
        let mut pool_config = PoolConfig::new();
        pool_config
            .max_size(POOL_WIDTH)
            .min_idle(POOL_WIDTH)
            .acquire_timeout(SETTLE_TIMEOUT);
        let pool = Pool::connect_with_config(connection_config, pool_config)
            .await
            .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));

        let mut pooled_clients = Vec::with_capacity(POOL_WIDTH);
        let mut pooled_pids = Vec::with_capacity(POOL_WIDTH);
        for _ in 0..POOL_WIDTH {
            let client = pool.get().await.expect("check out every warm pool entry");
            let pid: i32 = client
                .query_one_scalar("SELECT pg_backend_pid()::int4", &[])
                .await
                .expect("use the warm pool entry and read its backend pid");
            pooled_pids.push(pid);
            pooled_clients.push(client);
        }
        pooled_pids.sort_unstable();
        pooled_pids.dedup();
        assert_eq!(
            pooled_pids.len(),
            POOL_WIDTH,
            "holding every lease did not reach every physical pool entry"
        );
        assert_eq!(pool.active_count(), POOL_WIDTH);
        assert_eq!(pool.idle_count(), 0);
        assert_eq!(pool.total_count(), POOL_WIDTH);

        drop(pooled_clients);
        assert_eq!(pool.active_count(), 0);
        assert_eq!(
            pool.idle_count(),
            POOL_WIDTH,
            "the used pool entries were not all idle before termination"
        );
        assert_eq!(pool.total_count(), POOL_WIDTH);

        let mut killer_config: Config = url.parse().expect("the test DSN did not parse");
        killer_config.application_name(&killer_application_name);
        let (killer, killer_connection) = killer_config
            .connect(common::suite_tls())
            .await
            .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));
        let killer_driver = compio::runtime::spawn(async move { killer_connection.run().await });

        let tagged_before = tagged_backend_pids(&killer, &application_name).await;
        assert_eq!(
            tagged_before, pooled_pids,
            "the application_name selector did not resolve to exactly the owned pool backends"
        );

        // `test_object_name` includes this test process's PID and a hash. The
        // exact pre-kill PID equality above, plus current_database and the
        // killer exclusion below, ensures this one statement cannot terminate
        // a concurrent suite run's backends or the separate killer connection.
        let terminated = killer
            .query(
                "WITH victims AS MATERIALIZED (
                     SELECT pid
                     FROM pg_stat_activity
                     WHERE datname = current_database()
                       AND application_name = $1
                       AND pid <> pg_backend_pid()
                 )
                 SELECT pid::int4, pg_terminate_backend(pid)
                 FROM victims
                 ORDER BY pid",
                &[&application_name],
            )
            .await
            .expect("terminate every tagged pooled backend in one statement");
        let mut terminated_pids = Vec::with_capacity(terminated.len());
        for row in terminated {
            let pid: i32 = row.get(0);
            let was_terminated: bool = row.get(1);
            assert!(
                was_terminated,
                "PostgreSQL did not terminate tagged backend {pid}"
            );
            terminated_pids.push(pid);
        }
        assert_eq!(
            terminated_pids, pooled_pids,
            "the mass termination did not reach exactly the warm pool backends"
        );

        let tagged_after = wait_for_no_tagged_backends(&killer, &application_name).await;
        assert!(
            tagged_after.is_empty(),
            "tagged pooled backends were still alive after termination: {tagged_after:?}"
        );
        assert_eq!(
            pool.idle_count(),
            POOL_WIDTH,
            "the pool removed dead idle entries before checkout exercised recovery"
        );
        assert_eq!(pool.active_count(), 0);
        assert_eq!(pool.total_count(), POOL_WIDTH);

        // This first query is the recovery contract. `get()` must discard all
        // dead idle candidates and the query must run on a fresh connection.
        let first_recovered = pool
            .get()
            .await
            .expect("recover the pool after every idle backend died");
        let post_kill_pid: i32 = first_recovered
            .query_one_scalar("SELECT pg_backend_pid()::int4", &[])
            .await
            .unwrap_or_else(|error| {
                panic!(
                    "the first post-kill query received a dead pool entry: {}",
                    common::error_chain(&error)
                )
            });
        assert!(
            !terminated_pids.contains(&post_kill_pid),
            "the post-kill query ran on terminated backend {post_kill_pid}"
        );

        let mut recovered_clients = vec![first_recovered];
        let mut recovered_pids = vec![post_kill_pid];
        for _ in 1..POOL_WIDTH {
            let client = pool
                .get()
                .await
                .expect("refill the pool after mass termination");
            let pid: i32 = client
                .query_one_scalar("SELECT pg_backend_pid()::int4", &[])
                .await
                .expect("query through a replacement pool connection");
            assert!(
                !terminated_pids.contains(&pid),
                "a replacement query ran on terminated backend {pid}"
            );
            recovered_pids.push(pid);
            recovered_clients.push(client);
        }
        recovered_pids.sort_unstable();
        recovered_pids.dedup();
        assert_eq!(
            recovered_pids.len(),
            POOL_WIDTH,
            "the recovered pool did not contain the configured number of physical connections"
        );
        drop(recovered_clients);

        let created_after_recovery = pool.metrics.connections_created.get();
        let evictions_after_recovery = pool.metrics.evictions.get();
        assert_eq!(created_after_recovery, (POOL_WIDTH * 2) as u64);
        assert_eq!(evictions_after_recovery, POOL_WIDTH as u64);
        assert_eq!(pool.idle_count(), POOL_WIDTH);
        assert_eq!(pool.active_count(), 0);
        assert_eq!(pool.total_count(), POOL_WIDTH);

        let expected_live = baseline + POOL_WIDTH + 1;
        let settled_live = wait_for_live_connections(expected_live).await;
        assert_eq!(
            settled_live, expected_live,
            "live connections did not settle to the replacement pool plus its separate observer"
        );

        for round in 1..=RECOVERY_ROUNDS {
            let mut clients = Vec::with_capacity(POOL_WIDTH);
            let mut round_pids = Vec::with_capacity(POOL_WIDTH);
            for _ in 0..POOL_WIDTH {
                let client = pool
                    .get()
                    .await
                    .unwrap_or_else(|error| panic!("recovery round {round} failed: {error}"));
                let pid: i32 = client
                    .query_one_scalar("SELECT pg_backend_pid()::int4", &[])
                    .await
                    .unwrap_or_else(|error| {
                        panic!(
                            "recovery round {round} query failed: {}",
                            common::error_chain(&error)
                        )
                    });
                round_pids.push(pid);
                clients.push(client);
            }
            round_pids.sort_unstable();
            assert_eq!(
                round_pids, recovered_pids,
                "recovery round {round} replaced healthy connections"
            );
            drop(clients);

            assert_eq!(pool.idle_count(), POOL_WIDTH);
            assert_eq!(pool.active_count(), 0);
            assert_eq!(pool.total_count(), POOL_WIDTH);
            assert_eq!(
                pool.metrics.connections_created.get(),
                created_after_recovery,
                "recovery round {round} opened another connection"
            );
            assert_eq!(pool.metrics.evictions.get(), evictions_after_recovery);
            assert_eq!(
                compio_postgres::live_connections(),
                expected_live,
                "recovery round {round} grew the live connection count"
            );
        }

        println!(
            "mass idle pool recovery: tagged_before={} tagged_after={} killed_pids={:?} \
             post_kill_pid={} recovered_pids={:?} connections_created={} evictions={} \
             live_connections={} rounds={}",
            tagged_before.len(),
            tagged_after.len(),
            terminated_pids,
            post_kill_pid,
            recovered_pids,
            created_after_recovery,
            evictions_after_recovery,
            settled_live,
            RECOVERY_ROUNDS
        );

        pool.close().await;
        let live_after_pool_close = wait_for_live_connections(baseline + 1).await;
        assert_eq!(live_after_pool_close, baseline + 1);
        drop(killer);
        let _ = killer_driver.await;
        let final_live = wait_for_live_connections(baseline).await;
        assert_eq!(
            final_live, baseline,
            "mass pool recovery left connection tasks alive"
        );
    })
    .await
    .expect("mass idle pool recovery exceeded its watchdog");
}

/// A pooled backend killed while its lease is checked out but idle must fail
/// that lease's next query and lose its capacity slot when the lease is
/// dropped. This reaches the release-path ruling that idle-pool recovery does
/// not: the dead `PooledClient` itself returns to the pool.
#[compio::test]
async fn a_checked_out_idle_pool_backend_is_discarded_after_termination() {
    compio::time::timeout(WATCHDOG, async {
        let url = test_url();
        let baseline = compio_postgres::live_connections();
        let application_name = common::test_object_name("cpg_checked_out_idle_kill");
        let killer_application_name = common::test_object_name("cpg_checked_out_idle_killer");

        let mut connection_config: Config = url.parse().expect("the test DSN did not parse");
        connection_config.application_name(&application_name);
        let mut pool_config = PoolConfig::new();
        pool_config
            .max_size(1)
            .min_idle(1)
            .acquire_timeout(SETTLE_TIMEOUT);
        let pool = Pool::connect_with_config(connection_config, pool_config)
            .await
            .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));

        let borrower = pool.get().await.expect("check out the only pool entry");
        let killed_pid: i32 = borrower
            .query_one_scalar("SELECT pg_backend_pid()::int4", &[])
            .await
            .expect("read the checked-out backend PID");
        assert_eq!(pool.active_count(), 1);
        assert_eq!(pool.idle_count(), 0);
        assert_eq!(pool.total_count(), 1);

        let mut killer_config: Config = url.parse().expect("the test DSN did not parse");
        killer_config.application_name(&killer_application_name);
        let (killer, killer_connection) = killer_config
            .connect(common::suite_tls())
            .await
            .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));
        let killer_driver = compio::runtime::spawn(async move { killer_connection.run().await });

        assert_eq!(
            tagged_backend_pids(&killer, &application_name).await,
            vec![killed_pid],
            "the application_name selector did not resolve to the checked-out pool backend"
        );
        assert_eq!(
            terminate_tagged_backend(&killer, &application_name, killed_pid).await,
            killed_pid
        );
        let tagged_after = wait_for_no_tagged_backends(&killer, &application_name).await;
        assert!(
            tagged_after.is_empty(),
            "the checked-out backend remained alive after termination: {tagged_after:?}"
        );

        let failure = borrower
            .query_one_scalar::<i32, _>("SELECT 42::int4", &[])
            .await
            .expect_err("a query on the terminated checked-out backend reported success");
        let failure_kind = format!("{failure:?}");
        let failure_chain = common::error_chain(&failure);
        let failure_sqlstate = failure.code().map(|code| code.code().to_owned());
        assert!(
            failure.is_closed(),
            "the idle-in-hand query did not report a closed connection: {failure_chain}"
        );
        assert_eq!(
            failure_sqlstate, None,
            "the idle-in-hand failure unexpectedly carried a SQLSTATE"
        );

        drop(borrower);
        let (idle_after_drop, total_after_drop) = assert_dead_lease_was_discarded(&pool);
        let replacement_pid = assert_replacement_uses_a_new_backend(&pool, killed_pid).await;

        println!(
            "checked-out idle pool termination: killed_pid={killed_pid} \
             next_pid={replacement_pid} error_kind={failure_kind} \
             sqlstate={failure_sqlstate:?} idle_after_drop={idle_after_drop} \
             total_after_drop={total_after_drop}"
        );

        pool.close().await;
        assert_eq!(
            wait_for_live_connections(baseline + 1).await,
            baseline + 1,
            "the checked-out idle pool left a connection task alive"
        );
        drop(killer);
        let _ = killer_driver.await;
        assert_eq!(
            wait_for_live_connections(baseline).await,
            baseline,
            "the checked-out idle termination test leaked its killer connection"
        );
    })
    .await
    .expect("checked-out idle pool termination exceeded its watchdog");
}

/// A pooled backend killed while one of its queries is executing must report
/// the server's FATAL response to that borrower, and the NEXT checkout must
/// land on a live backend.
///
/// A FATAL response is itself PostgreSQL's protocol-level declaration that the
/// session is ending. The reader may not have observed the following EOF when
/// the query future wakes, so response dispatch must publish retirement before
/// the borrower can synchronously return its lease. Otherwise `idle_count()`
/// reports a known-dead connection as available, `total_count()` retains a
/// phantom slot, and `min_idle` can be satisfied by a corpse until a later
/// checkout happens to clean it up.
#[compio::test]
async fn a_checked_out_pool_backend_killed_mid_query_is_discarded() {
    compio::time::timeout(WATCHDOG, async {
        let url = test_url();
        let baseline = compio_postgres::live_connections();
        let application_name = common::test_object_name("cpg_checked_out_mid_query_kill");
        let killer_application_name = common::test_object_name("cpg_checked_out_mid_query_killer");
        let query_marker = common::test_object_name("cpg_checked_out_mid_query_marker");

        let mut connection_config: Config = url.parse().expect("the test DSN did not parse");
        connection_config.application_name(&application_name);
        let mut pool_config = PoolConfig::new();
        pool_config
            .max_size(1)
            .min_idle(1)
            .acquire_timeout(SETTLE_TIMEOUT);
        let pool = Pool::connect_with_config(connection_config, pool_config)
            .await
            .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));

        let borrower = pool.get().await.expect("check out the only pool entry");
        let killed_pid: i32 = borrower
            .query_one_scalar("SELECT pg_backend_pid()::int4", &[])
            .await
            .expect("read the checked-out backend PID");
        assert_eq!(pool.active_count(), 1);
        assert_eq!(pool.idle_count(), 0);
        assert_eq!(pool.total_count(), 1);

        let mut killer_config: Config = url.parse().expect("the test DSN did not parse");
        killer_config.application_name(&killer_application_name);
        let (killer, killer_connection) = killer_config
            .connect(common::suite_tls())
            .await
            .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));
        let killer_driver = compio::runtime::spawn(async move { killer_connection.run().await });

        assert_eq!(
            tagged_backend_pids(&killer, &application_name).await,
            vec![killed_pid],
            "the application_name selector did not resolve to the checked-out pool backend"
        );

        let query = format!("SELECT pg_sleep(10) /* {query_marker} */");
        let kill_mid_query = async {
            wait_for_tagged_pg_sleep(&killer, &application_name, killed_pid, &query_marker).await;
            terminate_tagged_backend(&killer, &application_name, killed_pid).await
        };
        let (query_result, selected_pid) =
            futures_util::future::join(borrower.batch_execute(&query), kill_mid_query).await;
        assert_eq!(selected_pid, killed_pid);

        let failure = query_result
            .expect_err("the query completed after its checked-out pooled backend was killed");
        let failure_kind = format!("{failure:?}");
        let failure_chain = common::error_chain(&failure);
        let failure_sqlstate = failure.code().map(|code| code.code().to_owned());
        // Drop immediately after the FATAL reaches the caller. Waiting for the
        // backend to disappear first would give the reader task time to see
        // EOF and could hide a release path that briefly redeposits the corpse.
        drop(borrower);
        println!(
            "checked-out mid-query release observation: killed_pid={killed_pid} \
             error_kind={failure_kind} sqlstate={failure_sqlstate:?} \
             idle_after_drop={} total_after_drop={} error_chain={failure_chain}",
            pool.idle_count(),
            pool.total_count()
        );
        let (idle_after_drop, total_after_drop) = assert_dead_lease_was_discarded(&pool);
        let replacement_pid = assert_replacement_uses_a_new_backend(&pool, killed_pid).await;
        assert_eq!(
            failure.code(),
            Some(&SqlState::ADMIN_SHUTDOWN),
            "the mid-query kill lost PostgreSQL's FATAL response: {failure_chain}"
        );

        // The KILLED pid must be gone. Not "no tagged backends remain": the
        // replacement acquired above carries the same application_name, so
        // that stronger claim can only hold before a replacement exists.
        let tagged_after = tagged_backend_pids(&killer, &application_name).await;
        assert!(
            !tagged_after.contains(&killed_pid),
            "the killed mid-query backend {killed_pid} is still alive: {tagged_after:?}"
        );

        println!(
            "checked-out mid-query pool termination: killed_pid={killed_pid} \
             next_pid={replacement_pid} error_kind={failure_kind} \
             sqlstate={failure_sqlstate:?} idle_after_drop={idle_after_drop} \
             total_after_drop={total_after_drop}"
        );

        pool.close().await;
        assert_eq!(
            wait_for_live_connections(baseline + 1).await,
            baseline + 1,
            "the checked-out mid-query pool left a connection task alive"
        );
        drop(killer);
        let _ = killer_driver.await;
        assert_eq!(
            wait_for_live_connections(baseline).await,
            baseline,
            "the checked-out mid-query termination test leaked its killer connection"
        );
    })
    .await
    .expect("checked-out mid-query pool termination exceeded its watchdog");
}
