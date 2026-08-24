use compio_postgres::error::SqlState;
use compio_postgres::types::ToSql;
use compio_postgres::{Client, NoTls, QueryEvent, QueryOutcome};
use futures_channel::mpsc;
use futures_util::{FutureExt, StreamExt};
use std::future::Future;
use std::task::{Context, Waker};
use std::time::{Duration, Instant};

mod common;

const EVENT_TIMEOUT: Duration = Duration::from_secs(5);
const OBJECT_TEST_TIMEOUT: Duration = Duration::from_secs(30);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);

fn test_url() -> String {
    common::test_url()
}

async fn connect() -> Client {
    let url = test_url();
    let (client, connection) = compio_postgres::connect(&url, common::suite_tls())
        .await
        .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));
    compio::runtime::spawn(async move {
        if let Err(error) = connection.run().await {
            eprintln!("connection error: {}", common::error_chain(&error));
        }
    })
    .detach();
    client
}

async fn drop_test_table(table: &str) -> Result<(), String> {
    let url = test_url();
    let sql = format!(
        "SET lock_timeout = '4s'; SET statement_timeout = '4s'; \
         DROP TABLE IF EXISTS {table}"
    );

    compio::time::timeout(CLEANUP_TIMEOUT, async {
        let (client, connection) = compio_postgres::connect(&url, common::suite_tls())
            .await
            .map_err(|error| common::error_chain(&error))?;
        compio::runtime::spawn(async move {
            if let Err(error) = connection.run().await {
                eprintln!("cleanup connection error: {}", common::error_chain(&error));
            }
        })
        .detach();
        client
            .batch_execute(&sql)
            .await
            .map_err(|error| common::error_chain(&error))
    })
    .await
    .map_err(|_| format!("dropping {table} exceeded its cleanup timeout"))?
}

async fn run_with_table_cleanup(
    table: &str,
    test: futures_util::future::LocalBoxFuture<'_, ()>,
) {
    let outcome = match compio::time::timeout(
        OBJECT_TEST_TIMEOUT,
        std::panic::AssertUnwindSafe(test).catch_unwind(),
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(_) => Err(Box::new(format!(
            "test using {table} exceeded its {OBJECT_TEST_TIMEOUT:?} timeout"
        )) as Box<dyn std::any::Any + Send>),
    };
    let cleanup = match std::panic::AssertUnwindSafe(drop_test_table(table))
        .catch_unwind()
        .await
    {
        Ok(cleanup) => cleanup,
        Err(_) => Err(format!("cleanup for {table} panicked")),
    };

    match outcome {
        Ok(()) => cleanup.unwrap_or_else(|error| panic!("failed to clean up {table}: {error}")),
        Err(panic) => {
            if let Err(error) = cleanup {
                eprintln!("failed to clean up {table} after test failure: {error}");
            }
            std::panic::resume_unwind(panic);
        }
    }
}

async fn next_event(
    events: &mut mpsc::UnboundedReceiver<QueryEvent>,
    sql: &str,
) -> QueryEvent {
    compio::time::timeout(EVENT_TIMEOUT, async {
        loop {
            let event = events
                .next()
                .await
                .expect("query observer closed before the target event");
            if event.sql() == sql {
                return event;
            }
        }
    })
    .await
    .expect("query observer did not report the target SQL")
}

async fn events_through(
    events: &mut mpsc::UnboundedReceiver<QueryEvent>,
    terminal_sql: &str,
) -> Vec<QueryEvent> {
    compio::time::timeout(EVENT_TIMEOUT, async {
        let mut collected = Vec::new();
        loop {
            let event = events
                .next()
                .await
                .expect("query observer closed before the terminal event");
            let done = event.sql() == terminal_sql;
            collected.push(event);
            if done {
                return collected;
            }
        }
    })
    .await
    .expect("query observer did not reach the terminal SQL")
}

#[compio::test]
async fn observer_without_threshold_reports_every_query() {
    const SQL: &str = "SELECT 10::int4 /* cpg_obs_no_threshold */";

    let client = connect().await;
    let mut events = client.query_events();

    client.simple_query(SQL).await.unwrap();

    let event = next_event(&mut events, SQL).await;
    assert_eq!(event.outcome(), &QueryOutcome::Success);
}

#[compio::test]
async fn observer_threshold_filters_query_below_cutoff() {
    const SQL: &str = "SELECT 11::int4 /* cpg_obs_below_threshold */";

    let client = connect().await;
    let mut events = client.query_events_with_threshold(Duration::MAX);

    compio::time::timeout(EVENT_TIMEOUT, client.simple_query(SQL))
        .await
        .expect("below-threshold query did not complete")
        .unwrap();

    // Replacing the observer drops the old sender after all prior work has
    // completed, giving absence a deterministic barrier instead of a timer.
    let replacement = client.query_events();
    let observed = compio::time::timeout(EVENT_TIMEOUT, events.next())
        .await
        .expect("replaced query observer did not close");
    assert!(observed.is_none(), "below-threshold query was reported");
    drop(replacement);
}

#[compio::test]
async fn observer_threshold_reports_query_above_cutoff() {
    const SQL: &str = "SELECT pg_sleep(0.100) /* cpg_obs_above_threshold */";
    const THRESHOLD: Duration = Duration::from_millis(50);

    let client = connect().await;
    let mut events = client.query_events_with_threshold(THRESHOLD);

    client.simple_query(SQL).await.unwrap();

    let event = next_event(&mut events, SQL).await;
    assert!(event.elapsed() >= THRESHOLD);
    assert_eq!(event.outcome(), &QueryOutcome::Success);
}

#[compio::test]
async fn zero_threshold_reports_every_query() {
    const SQL: &str = "SELECT 12::int4 /* cpg_obs_zero_threshold */";

    let client = connect().await;
    let mut events = client.query_events_with_threshold(Duration::ZERO);

    client.simple_query(SQL).await.unwrap();

    let event = next_event(&mut events, SQL).await;
    assert_eq!(event.outcome(), &QueryOutcome::Success);
}

#[compio::test]
async fn observer_reports_sql_elapsed_success_rows_without_bound_values() {
    const SQL: &str =
        "SELECT $1::text AS value FROM generate_series(1, 3) /* cpg_obs_success */";
    const SECRET: &str = "cpg_obs_bound_secret_7f36";

    let client = connect().await;
    let mut events = client.query_events();

    let started = Instant::now();
    let rows = client.query(SQL, &[&SECRET]).await.unwrap();
    let outer_elapsed = started.elapsed();
    assert_eq!(rows.len(), 3);

    let event = next_event(&mut events, SQL).await;
    assert_eq!(event.outcome(), &QueryOutcome::Success);
    assert_eq!(event.rows(), Some(3));
    assert!(
        event.elapsed() <= outer_elapsed,
        "observer elapsed time included work after query completion"
    );
    assert_eq!(event.sql(), SQL);
    assert!(
        !format!("{event:?}").contains(SECRET),
        "observer event exposed a bound parameter value"
    );
}

#[compio::test]
async fn observer_reports_cancelled_once_when_row_stream_drops_early() {
    const SQL: &str =
        "SELECT i::int4 FROM generate_series(1, 10000) AS i /* cpg_obs_early_drop */";
    const BARRIER: &str = "SELECT 1::int4 /* cpg_obs_early_drop_barrier */";

    let client = connect().await;
    let mut events = client.query_events();
    let statement = client.prepare(SQL).await.unwrap();
    let barrier = client.prepare(BARRIER).await.unwrap();

    let mut stream = Box::pin(
        client
            .query_raw(&statement, std::iter::empty::<&i32>())
            .await
            .unwrap(),
    );
    let first = stream
        .as_mut()
        .next()
        .await
        .expect("stream ended before its first row")
        .unwrap();
    assert_eq!(first.get::<_, i32>(0), 1);
    drop(stream);

    client.query(&barrier, &[]).await.unwrap();
    let observed = events_through(&mut events, BARRIER).await;
    let target: Vec<_> = observed
        .iter()
        .filter(|event| event.sql() == SQL)
        .collect();
    assert_eq!(target.len(), 1, "early drop emitted zero or multiple events");
    assert_eq!(target[0].outcome(), &QueryOutcome::Cancelled);
    assert_eq!(target[0].rows(), None);
}

#[compio::test]
async fn observer_reports_database_error_once() {
    const SQL: &str = "SELECT 1 / $1::int4 /* cpg_obs_database_error */";
    const BARRIER: &str = "SELECT 2::int4 /* cpg_obs_database_error_barrier */";

    let client = connect().await;
    let mut events = client.query_events();
    let statement = client.prepare(SQL).await.unwrap();
    let barrier = client.prepare(BARRIER).await.unwrap();

    let error = client
        .query(&statement, &[&0_i32])
        .await
        .expect_err("division by zero unexpectedly succeeded");
    assert_eq!(
        error.code(),
        Some(&compio_postgres::error::SqlState::DIVISION_BY_ZERO)
    );

    client.query(&barrier, &[]).await.unwrap();
    let observed = events_through(&mut events, BARRIER).await;
    let target: Vec<_> = observed
        .iter()
        .filter(|event| event.sql() == SQL)
        .collect();
    assert_eq!(target.len(), 1, "database error emitted zero or multiple events");
    assert_eq!(target[0].rows(), None);
    match target[0].outcome() {
        QueryOutcome::DatabaseError { code } => assert_eq!(
            code.as_ref(),
            Some(&compio_postgres::error::SqlState::DIVISION_BY_ZERO)
        ),
        other => panic!("expected database error observation, got {other:?}"),
    }
}

#[compio::test]
async fn observer_reports_server_query_canceled_as_cancelled() {
    const SQL: &str = "SELECT pg_sleep(3) /* cpg_obs_server_cancelled */";
    const BARRIER: &str = "SELECT 3::int4 /* cpg_obs_server_cancelled_barrier */";

    compio::time::timeout(Duration::from_secs(10), async {
        let client = connect().await;
        let mut events = client.query_events();

        client
            .batch_execute("SET statement_timeout = '50ms'")
            .await
            .unwrap();
        let error = client
            .query(SQL, &[])
            .await
            .expect_err("statement_timeout did not cancel pg_sleep");
        assert_eq!(error.code(), Some(&SqlState::QUERY_CANCELED));
        client
            .batch_execute("RESET statement_timeout")
            .await
            .unwrap();

        client.simple_query(BARRIER).await.unwrap();
        let observed = events_through(&mut events, BARRIER).await;
        let target: Vec<_> = observed
            .iter()
            .filter(|event| event.sql() == SQL)
            .collect();
        assert_eq!(
            target.len(),
            1,
            "server cancellation emitted zero or multiple events"
        );
        assert_eq!(target[0].outcome(), &QueryOutcome::Cancelled);
        assert_eq!(target[0].rows(), None);
    })
    .await
    .expect("server-cancellation observer claim exceeded its watchdog");
}

#[compio::test]
async fn observer_reports_each_portal_chunk_once() {
    const SQL: &str =
        "SELECT i::int4 FROM generate_series(1, 5) AS i /* cpg_obs_portal_chunks */";
    const BARRIER: &str = "SELECT 3::int4 /* cpg_obs_portal_barrier */";

    let mut client = connect().await;
    let mut events = client.query_events();
    let transaction = client.transaction().await.unwrap();
    let statement = transaction.prepare(SQL).await.unwrap();
    let barrier = transaction.prepare(BARRIER).await.unwrap();
    let portal = transaction.bind(&statement, &[]).await.unwrap();

    let first = transaction.query_portal(&portal, 2).await.unwrap();
    let second = transaction.query_portal(&portal, 2).await.unwrap();
    let third = transaction.query_portal(&portal, 2).await.unwrap();
    assert_eq!([first.len(), second.len(), third.len()], [2, 2, 1]);

    transaction.query(&barrier, &[]).await.unwrap();
    let observed = events_through(&mut events, BARRIER).await;
    let target: Vec<_> = observed
        .iter()
        .filter(|event| event.sql() == SQL)
        .collect();
    assert_eq!(target.len(), 3, "portal execution did not emit once per chunk");
    assert_eq!(
        target.iter().map(|event| event.rows()).collect::<Vec<_>>(),
        [Some(2), Some(2), Some(1)]
    );
    assert!(
        target
            .iter()
            .all(|event| event.outcome() == &QueryOutcome::Success)
    );

    transaction.rollback().await.unwrap();
}

#[compio::test]
async fn observer_reports_copy_in_once_with_inserted_rows() {
    use bytes::Bytes;
    use futures_util::SinkExt;

    const BARRIER: &str = "SELECT 4::int4 /* cpg_obs_copy_in_barrier */";

    let table = common::test_object_name("cpg_obs_copy_in");
    let sql = format!("COPY {table} (n) FROM STDIN");
    run_with_table_cleanup(
        &table,
        async {
            let client = connect().await;
            client
                .execute(&format!("DROP TABLE IF EXISTS {table}"), &[])
                .await
                .unwrap();
            client
                .execute(&format!("CREATE TABLE {table} (n int NOT NULL)"), &[])
                .await
                .unwrap();

            let mut events = client.query_events();
            let barrier = client.prepare(BARRIER).await.unwrap();
            let sink = client.copy_in::<_, Bytes>(sql.as_str()).await.unwrap();
            let mut sink = Box::pin(sink);
            sink.as_mut()
                .send(Bytes::from_static(b"1\n2\n3\n"))
                .await
                .unwrap();
            assert_eq!(sink.as_mut().finish().await.unwrap(), 3);

            client.query(&barrier, &[]).await.unwrap();
            let observed = events_through(&mut events, BARRIER).await;
            let target: Vec<_> = observed
                .iter()
                .filter(|event| event.sql() == sql.as_str())
                .collect();
            assert_eq!(target.len(), 1, "COPY IN emitted zero or multiple events");
            assert_eq!(target[0].outcome(), &QueryOutcome::Success);
            assert_eq!(target[0].rows(), Some(3));
        }
        .boxed_local(),
    )
    .await;
}

#[compio::test]
async fn observer_reports_copy_out_once_with_exported_rows() {
    const BARRIER: &str = "SELECT 5::int4 /* cpg_obs_copy_out_barrier */";

    let table = common::test_object_name("cpg_obs_copy_out");
    let sql = format!("COPY {table} TO STDOUT");
    run_with_table_cleanup(
        &table,
        async {
            let client = connect().await;
            client
                .execute(&format!("DROP TABLE IF EXISTS {table}"), &[])
                .await
                .unwrap();
            client
                .execute(&format!("CREATE TABLE {table} (n int NOT NULL)"), &[])
                .await
                .unwrap();
            client
                .execute(&format!("INSERT INTO {table} VALUES (1), (2), (3)"), &[])
                .await
                .unwrap();

            let mut events = client.query_events();
            let barrier = client.prepare(BARRIER).await.unwrap();
            let mut stream = Box::pin(client.copy_out(sql.as_str()).await.unwrap());
            let mut bytes = 0usize;
            while let Some(chunk) = stream.as_mut().next().await {
                bytes += chunk.unwrap().len();
            }
            assert!(bytes > 0, "COPY OUT returned no data");
            drop(stream);

            client.query(&barrier, &[]).await.unwrap();
            let observed = events_through(&mut events, BARRIER).await;
            let target: Vec<_> = observed
                .iter()
                .filter(|event| event.sql() == sql.as_str())
                .collect();
            assert_eq!(target.len(), 1, "COPY OUT emitted zero or multiple events");
            assert_eq!(target[0].outcome(), &QueryOutcome::Success);
            assert_eq!(target[0].rows(), Some(3));
        }
        .boxed_local(),
    )
    .await;
}

#[compio::test]
async fn observer_reports_dropped_in_flight_future_cancelled_once() {
    const SQL: &str = "SELECT pg_advisory_lock(hashtextextended($1, 0)) \
        /* cpg_obs_cancelled_future */";
    const BARRIER: &str = "SELECT 6::int4 /* cpg_obs_cancelled_future_barrier */";
    compio::time::timeout(Duration::from_secs(10), async {
        let lock_name = common::test_object_name("cpg_obs_cancelled_future_lock");
        let client = connect().await;
        let blocker = connect().await;
        let got_lock: bool = blocker
            .query_one_scalar(
                "SELECT pg_try_advisory_lock(hashtextextended($1, 0))",
                &[&lock_name],
            )
            .await
            .unwrap();
        assert!(got_lock, "another process owns this test's advisory lock");

        let mut events = client.query_events();
        let statement = client.prepare(SQL).await.unwrap();
        let barrier = client.prepare(BARRIER).await.unwrap();
        let mut future = Box::pin(client.query_raw(&statement, std::iter::once(&lock_name)));
        let waker = Waker::noop();
        let mut context = Context::from_waker(waker);
        assert!(
            future.as_mut().poll(&mut context).is_pending(),
            "advisory-lock query completed while the lock was held"
        );
        drop(future);

        compio::time::timeout(EVENT_TIMEOUT, async {
            loop {
                let waiting: bool = blocker
                    .query_one_scalar(
                        "SELECT EXISTS (\
                             SELECT 1 FROM pg_stat_activity \
                             WHERE pid = $1 \
                               AND state = 'active' \
                               AND wait_event_type = 'Lock' \
                               AND wait_event = 'advisory' \
                               AND query LIKE '%cpg_obs_cancelled_future%'\
                         )",
                        &[&client.process_id()],
                    )
                    .await
                    .unwrap();
                if waiting {
                    return;
                }
            }
        })
        .await
        .expect("dropped future never reached PostgreSQL as an executing statement");

        let unlocked: bool = blocker
            .query_one_scalar(
                "SELECT pg_advisory_unlock(hashtextextended($1, 0))",
                &[&lock_name],
            )
            .await
            .unwrap();
        assert!(unlocked);

        client.query(&barrier, &[]).await.unwrap();
        let observed = events_through(&mut events, BARRIER).await;
        let target: Vec<_> = observed
            .iter()
            .filter(|event| event.sql() == SQL)
            .collect();
        assert_eq!(target.len(), 1, "dropped future emitted zero or multiple events");
        assert_eq!(target[0].outcome(), &QueryOutcome::Cancelled);
        assert_eq!(target[0].rows(), None);
    })
    .await
    .expect("dropped-future observer test exceeded its watchdog");
}

#[compio::test]
async fn replacing_observer_preserves_the_in_flight_requests_receiver() {
    const SQL: &str =
        "SELECT pg_advisory_lock(hashtextextended($1, 0)) \
         /* cpg_obs_replacement_in_flight */";
    const BARRIER: &str = "SELECT 8::int4 /* cpg_obs_replacement_barrier */";

    compio::time::timeout(Duration::from_secs(10), async {
        let lock_name = common::test_object_name("cpg_obs_replacement_in_flight_lock");
        let client = connect().await;
        let blocker = connect().await;
        let got_lock: bool = blocker
            .query_one_scalar(
                "SELECT pg_try_advisory_lock(hashtextextended($1, 0))",
                &[&lock_name],
            )
            .await
            .unwrap();
        assert!(got_lock, "another process owns this test's advisory lock");

        let mut original_events = client.query_events();
        let statement = client.prepare(SQL).await.unwrap();
        let lock_params: [&(dyn ToSql + Sync); 1] = [&lock_name];
        let mut query = Box::pin(client.query(&statement, &lock_params));
        let waker = Waker::noop();
        let mut context = Context::from_waker(waker);
        assert!(
            query.as_mut().poll(&mut context).is_pending(),
            "the blocked query completed during its enqueue poll"
        );

        let mut replacement_events = client.query_events();
        let unlocked: bool = blocker
            .query_one_scalar(
                "SELECT pg_advisory_unlock(hashtextextended($1, 0))",
                &[&lock_name],
            )
            .await
            .unwrap();
        assert!(unlocked);

        query.await.expect("the in-flight observed query failed");
        client.simple_query(BARRIER).await.unwrap();

        let original = next_event(&mut original_events, SQL).await;
        assert_eq!(original.outcome(), &QueryOutcome::Success);

        let replacement = events_through(&mut replacement_events, BARRIER).await;
        assert!(
            replacement.iter().all(|event| event.sql() != SQL),
            "the replacement observer stole an already-in-flight request"
        );
    })
    .await
    .expect("observer-replacement claim exceeded its watchdog");
}

#[compio::test]
async fn replacing_observer_preserves_prepared_statement_attribution() {
    const SQL: &str = "SELECT 81::int4 /* cpg_obs_replacement_prepared */";
    const BARRIER: &str = "SELECT 82::int4 /* cpg_obs_replacement_prepared_barrier */";

    compio::time::timeout(Duration::from_secs(10), async {
        let client = connect().await;
        let _original_events = client.query_events();
        let statement = client.prepare(SQL).await.unwrap();

        let mut replacement_events = client.query_events();
        client.query(&statement, &[]).await.unwrap();
        client.simple_query(BARRIER).await.unwrap();

        let observed = events_through(&mut replacement_events, BARRIER).await;
        let attributed = observed
            .iter()
            .filter(|event| event.sql() == SQL)
            .collect::<Vec<_>>();
        assert_eq!(
            attributed.len(),
            1,
            "observer replacement forgot the prepared statement's SQL mapping"
        );
        assert_eq!(attributed[0].outcome(), &QueryOutcome::Success);
    })
    .await
    .expect("prepared-statement replacement claim exceeded its watchdog");
}

#[compio::test]
async fn reinstalling_observer_after_disconnect_preserves_statement_attribution() {
    const SQL: &str = "SELECT 87::int4 /* cpg_obs_reinstall_prepared */";
    const BARRIER: &str = "SELECT 88::int4 /* cpg_obs_reinstall_prepared_barrier */";

    compio::time::timeout(Duration::from_secs(10), async {
        let client = connect().await;
        let original_events = client.query_events();
        let statement = client.prepare(SQL).await.unwrap();

        drop(original_events);
        client.simple_query("").await.unwrap();

        let mut replacement_events = client.query_events();
        client.query(&statement, &[]).await.unwrap();
        client.simple_query(BARRIER).await.unwrap();

        let observed = events_through(&mut replacement_events, BARRIER).await;
        assert_eq!(
            observed.iter().filter(|event| event.sql() == SQL).count(),
            1,
            "observer reinstall forgot the prepared statement's SQL mapping"
        );
    })
    .await
    .expect("prepared-statement observer reinstall claim exceeded its watchdog");
}

#[compio::test]
async fn observer_queue_does_not_backpressure_and_consumer_can_reenter() {
    use futures_util::future;

    const QUEUED: &str = "SELECT 7::int4 /* cpg_obs_unpolled_queue */";
    const FIRST: &str = "SELECT 8::int4 /* cpg_obs_reentry_first */";
    const REENTERED: &str = "SELECT 9::int4 /* cpg_obs_reentered_query */";
    const QUEUE_LEN: usize = 64;

    let client = connect().await;
    let mut events = client.query_events();
    let queued = client.prepare(QUEUED).await.unwrap();
    let first = client.prepare(FIRST).await.unwrap();
    let reentered = client.prepare(REENTERED).await.unwrap();

    compio::time::timeout(EVENT_TIMEOUT, async {
        for _ in 0..QUEUE_LEN {
            client.query(&queued, &[]).await.unwrap();
        }
    })
    .await
    .expect("an undrained observer backpressured query execution");

    let mut queued_events = 0usize;
    while queued_events < QUEUE_LEN {
        let event = compio::time::timeout(EVENT_TIMEOUT, events.next())
            .await
            .expect("queued observer events stopped arriving")
            .expect("query observer closed while draining its queue");
        if event.sql() == QUEUED {
            queued_events += 1;
        }
    }

    let producer = client.query(&first, &[]);
    let consumer = async {
        let event = next_event(&mut events, FIRST).await;
        assert_eq!(event.outcome(), &QueryOutcome::Success);
        let rows = client.query(&reentered, &[]).await.unwrap();
        assert_eq!(rows[0].get::<_, i32>(0), 9);
    };
    let (produced, ()) = compio::time::timeout(EVENT_TIMEOUT, future::join(producer, consumer))
        .await
        .expect("observer consumption or re-entrant query deadlocked");
    assert_eq!(produced.unwrap()[0].get::<_, i32>(0), 8);

    let reentered_event = next_event(&mut events, REENTERED).await;
    assert_eq!(reentered_event.outcome(), &QueryOutcome::Success);
}
