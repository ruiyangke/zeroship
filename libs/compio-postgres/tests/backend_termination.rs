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
use compio_postgres::{Client, Config, Pool, PoolConfig};
use futures_util::{SinkExt, TryStreamExt};
use std::time::{Duration, Instant};

#[allow(dead_code)]
mod common;

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
/// they do not cover a pooled connection killed while checked out.
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
