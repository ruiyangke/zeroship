//! Sustained connection and pool churn must release both halves of every
//! session: the driver's live registry entry and PostgreSQL's backend.

use compio_postgres::error::SqlState;
use compio_postgres::{Client, Config, Pool, PoolConfig};
use std::future::Future;
use std::task::{Context, Waker};
use std::time::{Duration, Instant};

#[allow(unused_imports)]
use crate::common;

const CLEAN_CONNECTION_ITERATIONS: usize = 50;
const POOL_ITERATIONS: usize = 10;
const BAD_CONNECTION_ITERATIONS: usize = 10;
const POOL_WIDTH: usize = 2;
const ACTIVITY_TIMEOUT: Duration = Duration::from_secs(5);
const RELEASE_TIMEOUT: Duration = Duration::from_secs(5);
const WATCHDOG: Duration = Duration::from_secs(45);
const POLL_INTERVAL: Duration = Duration::from_millis(5);
// Measured 2026-08-24 after a warm build: 7.52s plaintext and 6.93s TLS for
// the test body (9.33s and 10.48s command wall-clock including recompilation).
const IN_FLIGHT_SQL: &str = "SELECT pg_advisory_lock(hashtextextended($1, 0)) \
    /* cpg_connection_churn_in_flight */";

fn tagged_config(url: &str, application_name: &str) -> Config {
    let mut config: Config = url.parse().expect("the test DSN did not parse");
    config.application_name(application_name);
    config
}

async fn open_client(config: &Config, url: &str) -> Client {
    // This path must use the suite connector. TLS has Rc-shared read/write
    // halves and a dedicated spawned reader inside the detached connection
    // driver, so plaintext churn alone would not exercise that ownership path.
    let (client, connection) = config
        .connect(common::suite_tls())
        .await
        .unwrap_or_else(|error| common::postgres_unreachable(url, &error));
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();
    client
}

async fn tagged_backend_count(observer: &Client, application_name: &str) -> i64 {
    observer
        .query_one_scalar(
            "SELECT count(*)::int8
             FROM pg_stat_activity
             WHERE datname = current_database()
               AND application_name = $1
               AND pid <> pg_backend_pid()",
            &[&application_name],
        )
        .await
        .expect("count tagged PostgreSQL backends")
}

async fn wait_for_backend_baseline(
    observer: &Client,
    application_name: &str,
    baseline: i64,
) -> i64 {
    let deadline = Instant::now() + RELEASE_TIMEOUT;
    loop {
        let observed = tagged_backend_count(observer, application_name).await;
        if observed == baseline || Instant::now() >= deadline {
            return observed;
        }
        compio::time::sleep(POLL_INTERVAL).await;
    }
}

async fn wait_for_live_baseline(baseline: usize) -> usize {
    let deadline = Instant::now() + RELEASE_TIMEOUT;
    loop {
        let observed = compio_postgres::live_connections();
        if observed == baseline || Instant::now() >= deadline {
            return observed;
        }
        compio::time::sleep(POLL_INTERVAL).await;
    }
}

async fn wait_for_advisory_lock_wait(
    observer: &Client,
    application_name: &str,
    process_id: i32,
) -> bool {
    let deadline = Instant::now() + ACTIVITY_TIMEOUT;
    loop {
        let waiting: bool = observer
            .query_one_scalar(
                "SELECT EXISTS (
                     SELECT 1
                     FROM pg_stat_activity
                     WHERE datname = current_database()
                       AND application_name = $1
                       AND pid = $2
                       AND state = 'active'
                       AND wait_event_type = 'Lock'
                       AND wait_event = 'advisory'
                       AND query LIKE '%cpg_connection_churn_in_flight%'
                 )",
                &[&application_name, &process_id],
            )
            .await
            .expect("observe the in-flight advisory-lock query");
        if waiting {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        compio::time::sleep(POLL_INTERVAL).await;
    }
}

fn sample_peak(peak_live: &mut usize) {
    *peak_live = (*peak_live).max(compio_postgres::live_connections());
}

async fn run_churn() {
    let started = Instant::now();
    let url = common::test_url();
    let application_name = common::test_object_name("cpg_connection_churn");

    let initial_live = compio_postgres::live_connections();
    assert_eq!(
        initial_live, 0,
        "connection churn started with {initial_live} live registry entries; \
         this one-test target must start from a clean baseline"
    );

    let config = tagged_config(&url, &application_name);
    let observer = open_client(&config, &url).await;
    let live_with_observer = compio_postgres::live_connections();
    assert_eq!(
        live_with_observer,
        initial_live + 1,
        "opening the counting connection changed live_connections from \
         {initial_live} to {live_with_observer}, not by one"
    );
    let server_baseline = tagged_backend_count(&observer, &application_name).await;

    let mut churn_opened = 0usize;
    let mut peak_live = live_with_observer;
    let mut held_lock_names = Vec::with_capacity(BAD_CONNECTION_ITERATIONS);

    for iteration in 0..CLEAN_CONNECTION_ITERATIONS {
        let client = open_client(&config, &url).await;
        churn_opened += 1;
        sample_peak(&mut peak_live);

        if iteration == 0 {
            let observed = tagged_backend_count(&observer, &application_name).await;
            assert_eq!(
                observed,
                server_baseline + 1,
                "the pg_stat_activity filter did not see exactly one tagged churn \
                 client (baseline {server_baseline}, observed {observed})"
            );
        }

        let sent = iteration as i32;
        let returned: i32 = client
            .query_one_scalar("SELECT $1::int4", &[&sent])
            .await
            .expect("run a trivial query on a churned connection");
        assert_eq!(returned, sent, "the trivial churn query changed its value");
        drop(client);
    }

    for iteration in 0..POOL_ITERATIONS {
        let mut pool_config = PoolConfig::new();
        pool_config.max_size(POOL_WIDTH).min_idle(POOL_WIDTH);
        let pool = Pool::connect_with_config(config.clone(), pool_config)
            .await
            .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));
        let pool_opened = pool.metrics().connections_created.get() as usize;
        assert_eq!(
            pool_opened, POOL_WIDTH,
            "pool churn iteration {iteration} opened {pool_opened} physical \
             connections instead of {POOL_WIDTH}"
        );
        churn_opened += pool_opened;
        sample_peak(&mut peak_live);

        let first = pool
            .acquire()
            .await
            .expect("check out the first pool connection");
        let second = pool
            .acquire()
            .await
            .expect("check out the second pool connection");
        assert_ne!(
            first.process_id(),
            second.process_id(),
            "pool churn iteration {iteration} did not hold two physical backends"
        );
        sample_peak(&mut peak_live);

        let (first_result, second_result) = futures_util::future::join(
            first.query_one_scalar::<i32, _>("SELECT 1::int4", &[]),
            second.query_one_scalar::<i32, _>("SELECT 2::int4", &[]),
        )
        .await;
        assert_eq!(first_result.expect("use the first pool connection"), 1);
        assert_eq!(second_result.expect("use the second pool connection"), 2);
        drop(first);
        drop(second);
        drop(pool);
    }

    for iteration in 0..BAD_CONNECTION_ITERATIONS {
        let lock_name = format!("{application_name}_in_flight_{iteration}");
        let locked: bool = observer
            .query_one_scalar(
                "SELECT pg_try_advisory_lock(hashtextextended($1, 0))",
                &[&lock_name],
            )
            .await
            .expect("acquire the churn blocker advisory lock");
        assert!(
            locked,
            "another session owned churn advisory lock {iteration}"
        );

        let client = open_client(&config, &url).await;
        churn_opened += 1;
        sample_peak(&mut peak_live);
        // PostgreSQL defaults this to zero, which means a backend blocked in a
        // long-running statement does not inspect the client socket until its
        // next read or write. A short check interval lets the server observe
        // this test's shutdown while the advisory lock keeps the statement
        // from reaching a happy completion.
        client
            .batch_execute("SET client_connection_check_interval = '100ms'")
            .await
            .expect("enable bounded peer checks for the in-flight churn query");
        let process_id = client.process_id();
        let statement = client
            .prepare(IN_FLIGHT_SQL)
            .await
            .expect("prepare the in-flight churn query");
        let mut query = Box::pin(client.query_raw(&statement, std::iter::once(&lock_name)));
        {
            let mut context = Context::from_waker(Waker::noop());
            assert!(
                query.as_mut().poll(&mut context).is_pending(),
                "in-flight churn query {iteration} completed while its advisory \
                 lock was held"
            );
        }
        drop(query);

        assert!(
            wait_for_advisory_lock_wait(&observer, &application_name, process_id).await,
            "in-flight churn query {iteration} never appeared in pg_stat_activity \
             as an active advisory-lock wait"
        );

        // The server has proved the statement is executing. Dropping the last
        // Client now must retire both the dedicated reader and its backend.
        drop(client);
        drop(statement);
        // Keep the query unable to finish normally until the server-side leak
        // check passes. Unlocking here would let a client whose drop failed to
        // close its socket reach a happy result and hide the bad release path.
        held_lock_names.push(lock_name);
    }

    for iteration in 0..BAD_CONNECTION_ITERATIONS {
        let client = open_client(&config, &url).await;
        churn_opened += 1;
        sample_peak(&mut peak_live);
        let error = client
            .query("SELECT 1 / 0", &[])
            .await
            .expect_err("division by zero unexpectedly succeeded");
        assert_eq!(
            error.code(),
            Some(&SqlState::DIVISION_BY_ZERO),
            "statement-error churn iteration {iteration} returned the wrong error: {error:?}"
        );
        drop(client);
    }

    let expected_churn_opened =
        CLEAN_CONNECTION_ITERATIONS + POOL_ITERATIONS * POOL_WIDTH + BAD_CONNECTION_ITERATIONS * 2;
    assert_eq!(
        churn_opened, expected_churn_opened,
        "the churn accounting omitted a physical connection"
    );

    let peak_above_baseline = peak_live.saturating_sub(live_with_observer);
    assert!(
        peak_above_baseline >= POOL_WIDTH,
        "connection churn was serialized into too little overlap: baseline with \
         observer {live_with_observer}, peak {peak_live}, required at least {}",
        live_with_observer + POOL_WIDTH
    );

    let observed_server =
        wait_for_backend_baseline(&observer, &application_name, server_baseline).await;
    if observed_server != server_baseline {
        let leaked = observed_server.saturating_sub(server_baseline);
        panic!(
            "server-side connection churn leaked {leaked} of {churn_opened} backends \
             after {:?}: baseline {server_baseline}, observed {observed_server}, \
             peak live_connections {peak_live}",
            RELEASE_TIMEOUT
        );
    }
    for (iteration, lock_name) in held_lock_names.iter().enumerate() {
        let unlocked: bool = observer
            .query_one_scalar(
                "SELECT pg_advisory_unlock(hashtextextended($1, 0))",
                &[lock_name],
            )
            .await
            .expect("release a churn blocker advisory lock");
        assert!(unlocked, "churn advisory lock {iteration} was not held");
    }

    let observed_live = wait_for_live_baseline(live_with_observer).await;
    if observed_live != live_with_observer {
        let leaked = observed_live.saturating_sub(live_with_observer);
        panic!(
            "client-side connection churn leaked {leaked} of {churn_opened} registry \
             entries after {:?}: baseline {live_with_observer}, observed \
             {observed_live}, peak {peak_live}",
            RELEASE_TIMEOUT
        );
    }

    drop(observer);
    let drained = compio_postgres::drain_connections(RELEASE_TIMEOUT).await;
    let final_live = compio_postgres::live_connections();
    if !drained || final_live != initial_live {
        let total_opened = churn_opened + 1;
        let leaked = final_live.saturating_sub(initial_live);
        panic!(
            "final connection drain leaked {leaked} of {total_opened} registry \
             entries after {:?}: baseline {initial_live}, observed {final_live}, \
             peak {peak_live}",
            RELEASE_TIMEOUT
        );
    }

    println!(
        "connection churn measurement: opened={} churn={} observer=1 \
         peak_live_connections={} peak_above_observer_baseline={} elapsed_ms={}",
        churn_opened + 1,
        churn_opened,
        peak_live,
        peak_above_baseline,
        started.elapsed().as_millis()
    );
}

#[compio::test]
async fn sustained_connection_and_pool_churn_leaves_nothing_behind() {
    let started = Instant::now();
    if compio::time::timeout(WATCHDOG, run_churn()).await.is_err() {
        panic!(
            "connection churn exceeded its {:?} watchdog after {:?}; \
             live_connections observed at timeout: {}",
            WATCHDOG,
            started.elapsed(),
            compio_postgres::live_connections()
        );
    }
}
