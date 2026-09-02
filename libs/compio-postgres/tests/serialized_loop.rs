// This file chooses its own transport and cannot run under
// `--features suite-over-tls`, which forces every helper onto the encrypted
// server: `serialized_loop` reaches its loop through a deliberately
// unsplittable PLAINTEXT socket, and the sslmode files assert what happens
// when a connector and a config disagree. Running them in that mode would
// measure the mode, not the claim.
#![cfg(not(feature = "suite-over-tls"))]

//! Behaviour on the SERIALIZED connection loop, reached without TLS.
//!
//! `Connection::run` picks its loop by whether the transport splits into owned
//! halves. **Both transports this crate ships split** - a plain socket, and
//! rustls since `tls_sansio` drives the session directly rather than through an
//! adapter that owns the socket (`maybe_tls_stream.rs:208`, `connection.rs:652`).
//! So both take the MULTIPLEXED loop. What still reaches the serialized one is
//! a custom `TlsConnect` whose stream answers `Err` to `try_into_split` - which
//! in practice means these tests and nothing else.
//!
//! THIS PARAGRAPH SAID THE OPPOSITE UNTIL 2026-09-02: that "rustls keeps shared
//! session state" so "every TLS connection takes the serialized one", and that
//! the only way in was to stand up a TLS server. That was true until the
//! 2026-08-24 fix, whose whole point was that the cause "was not that rustls
//! state cannot be shared" but that the adapter could never hand the socket
//! back. Reading the stale version, one would conclude that running the suite
//! over TLS exercises this loop. It does not - `suite-over-tls` runs on the
//! multiplexed loop too, and `a_notification_reaches_an_idle_tls_connection`
//! in `tests/tls_live.rs` passes precisely because TLS is now multiplexed
//! (the serialized idle step reads no socket, so that test cannot pass on it).
//!
//! The loops are not equivalent, and they once diverged with nothing going red
//! (task #49). `test_utils::connect_serialized` makes the serialized loop
//! reachable over plaintext. These tests are therefore not merely a parity net
//! for the refactor that will delete that loop - they are its ONLY coverage,
//! and the 25 of them are all that stands behind it.

use compio_postgres::test_utils::connect_serialized;
use compio_postgres::{Client, Config};

#[allow(dead_code)]
mod common;

async fn serialized_client_with_probe() -> (Client, std::rc::Rc<std::cell::Cell<bool>>) {
    let url = common::test_url();
    let config: Config = url.parse().expect("test DSN did not parse");
    let (client, connection, split_refused) = connect_serialized(&config)
        .await
        .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));
    compio::runtime::spawn(async move {
        if let Err(error) = connection.run().await {
            eprintln!(
                "serialized connection error: {}",
                common::error_chain(&error)
            );
        }
    })
    .detach();
    (client, split_refused)
}

async fn serialized_client() -> Client {
    serialized_client_with_probe().await.0
}

/// The harness must actually be on the serialized loop, or everything below
/// silently measures the multiplexed one and proves nothing about TLS.
///
/// The evidence is the SPLIT REFUSAL itself, recorded by the socket when
/// `Connection::run` asks it to split. Nothing else here discriminates: a
/// stable backend pid, a working query and a committed transaction are all
/// equally true on the multiplexed loop, so a test asserting those would pass
/// whichever loop it had reached.
#[compio::test]
async fn the_harness_is_really_on_the_serialized_loop() {
    let (client, split_refused) = serialized_client_with_probe().await;
    let first: i32 = client
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .expect("first query")
        .get(0);
    let second: i32 = client
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .expect("second query")
        .get(0);
    assert_eq!(first, second, "the harness reconnected between queries");
    assert!(
        split_refused.get(),
        "Connection::run never asked this socket to split, so these tests are \
         not on the serialized loop and prove nothing about TLS"
    );
}

/// Ordinary queries, parameters and row decoding work on this loop.
#[compio::test]
async fn queries_and_parameters_work_on_the_serialized_loop() {
    let client = serialized_client().await;
    let row = client
        .query_one("SELECT $1::int8 + 1, $2::text", &[&41i64, &"hello"])
        .await
        .expect("parameterised query");
    assert_eq!(row.get::<_, i64>(0), 42);
    assert_eq!(row.get::<_, &str>(1), "hello");
}

/// A server error must surface with its SQLSTATE and leave the session usable.
#[compio::test]
async fn an_error_leaves_the_serialized_session_usable() {
    let client = serialized_client().await;
    let Err(error) = client.query_one("SELECT 1/0", &[]).await else {
        panic!("division by zero must fail");
    };
    assert_eq!(
        error.code().map(|code| code.code()),
        Some("22012"),
        "the error lost its SQLSTATE: {}",
        common::error_chain(&error)
    );
    let row = client
        .query_one("SELECT 1::int4", &[])
        .await
        .expect("the session must survive a server error");
    assert_eq!(row.get::<_, i32>(0), 1);
}

/// Transactions commit and roll back on this loop.
#[compio::test]
async fn transactions_work_on_the_serialized_loop() {
    let mut client = serialized_client().await;
    let table = common::test_object_name("cpg serialized tx");
    client
        .batch_execute(&format!(
            "DROP TABLE IF EXISTS {table}; CREATE TABLE {table}(id int)"
        ))
        .await
        .expect("fixture");

    let transaction = client.transaction().await.expect("begin");
    transaction
        .execute(&format!("INSERT INTO {table} VALUES (1)"), &[])
        .await
        .expect("insert");
    transaction.rollback().await.expect("rollback");

    let row = client
        .query_one(&format!("SELECT count(*)::int8 FROM {table}"), &[])
        .await
        .expect("count");
    assert_eq!(row.get::<_, i64>(0), 0, "the rollback did not take");

    let _ = client
        .batch_execute(&format!("DROP TABLE IF EXISTS {table}"))
        .await;
}

/// COPY OUT on this loop.
///
/// The serialized loop has needed COPY-specific fixes of its own ("drain
/// stashed batches before each serialized copy startup read", "charge the read
/// clock for a copy response no producer answers"), so this is the surface
/// most likely to regress when the two loops merge.
#[compio::test]
async fn copy_out_works_on_the_serialized_loop() {
    use futures_util::TryStreamExt;

    let client = serialized_client().await;
    let stream = client
        .copy_out("COPY (SELECT g, repeat('x', 100) FROM generate_series(1, 500) g) TO STDOUT")
        .await
        .expect("copy_out");
    let chunks: Vec<bytes::Bytes> = stream.try_collect().await.expect("copy chunks");
    let bytes: usize = chunks.iter().map(bytes::Bytes::len).sum();
    let lines = chunks
        .iter()
        .flat_map(|chunk| chunk.iter())
        .filter(|byte| **byte == b'\n')
        .count();

    assert_eq!(lines, 500, "every row must arrive; got {bytes} bytes");

    // The session must still answer afterwards, which is what the serialized
    // COPY fixes were about.
    let row = client
        .query_one("SELECT 1::int4", &[])
        .await
        .expect("the session must survive a COPY OUT");
    assert_eq!(row.get::<_, i32>(0), 1);
}

/// COPY IN on this loop, including that the session recovers afterwards.
#[compio::test]
async fn copy_in_works_on_the_serialized_loop() {
    use futures_util::SinkExt;

    let client = serialized_client().await;
    let table = common::test_object_name("cpg serialized copyin");
    client
        .batch_execute(&format!(
            "DROP TABLE IF EXISTS {table}; CREATE TABLE {table}(id int, pad text)"
        ))
        .await
        .expect("fixture");

    let sink = client
        .copy_in::<_, bytes::Bytes>(&format!("COPY {table} FROM STDIN"))
        .await
        .expect("copy_in");
    let mut sink = std::pin::pin!(sink);
    for id in 1..=500 {
        sink.feed(bytes::Bytes::from(format!("{id}\tpad-{id}\n")))
            .await
            .expect("copy feed");
    }
    let written = sink.finish().await.expect("copy finish");
    assert_eq!(written, 500, "COPY IN reported the wrong count");

    let row = client
        .query_one(&format!("SELECT count(*)::int8 FROM {table}"), &[])
        .await
        .expect("count after copy");
    assert_eq!(row.get::<_, i64>(0), 500, "the rows did not land");

    let _ = client
        .batch_execute(&format!("DROP TABLE IF EXISTS {table}"))
        .await;
}

/// PostgreSQL can fail after CopyInResponse but before it reads the first
/// frontend COPY frame. The opening extended-query Sync then completes error
/// recovery, while the driver's already-queued CopyDone + Sync earns one more
/// ReadyForQuery. That protocol-required second reply belongs to the same COPY
/// exchange and must not be mistaken for the next request's response.
#[compio::test]
async fn a_post_copy_in_response_error_does_not_leave_a_second_ready_for_query() {
    let (client, split_refused) = serialized_client_with_probe().await;
    let table = common::test_object_name("cpg post copy response error");
    let function = common::test_object_name("cpg post copy response error function");
    let trigger = common::test_object_name("cpg post copy response error trigger");

    client
        .batch_execute(&format!(
            "CREATE TEMPORARY TABLE {table} (n int);
             CREATE FUNCTION pg_temp.{function}() RETURNS trigger
             LANGUAGE plpgsql AS $$
             BEGIN
                 PERFORM pg_sleep(1);
                 RAISE EXCEPTION USING
                     ERRCODE = 'P0001',
                     MESSAGE = 'post-G failure';
             END
             $$;
             CREATE TRIGGER {trigger}
             BEFORE INSERT ON {table}
             FOR EACH STATEMENT EXECUTE FUNCTION pg_temp.{function}();"
        ))
        .await
        .expect("create delayed COPY IN failure fixture");

    let sink = client
        .copy_in::<_, bytes::Bytes>(&format!("COPY {table} FROM STDIN"))
        .await
        .expect("PostgreSQL must enter COPY IN before the trigger fails");
    let mut sink = std::pin::pin!(sink);
    let error = compio::time::timeout(std::time::Duration::from_secs(5), sink.as_mut().finish())
        .await
        .expect("COPY IN failure did not finish")
        .expect_err("the delayed trigger unexpectedly accepted COPY IN");
    assert_eq!(
        error.code().map(compio_postgres::error::SqlState::code),
        Some("P0001"),
        "COPY IN lost the post-G server error: {}",
        common::error_chain(&error)
    );
    assert!(
        split_refused.get(),
        "this test is not on the serialized loop"
    );

    let follow_up = compio::time::timeout(
        std::time::Duration::from_secs(5),
        client.query_one("SELECT 7::int4", &[]),
    )
    .await
    .expect("the post-G COPY error stranded the serialized session");
    let row = follow_up.unwrap_or_else(|error| {
        panic!(
            "a post-G COPY error left a second ReadyForQuery: {}",
            common::error_chain(&error)
        )
    });
    assert_eq!(row.get::<_, i32>(0), 7);
}

/// Portal paging on this loop.
#[compio::test]
async fn portal_paging_works_on_the_serialized_loop() {
    let mut client = serialized_client().await;
    let transaction = client.transaction().await.expect("begin");
    let statement = transaction
        .prepare("SELECT g::int4 FROM generate_series(1, 10) g ORDER BY g")
        .await
        .expect("prepare");
    let portal = transaction.bind(&statement, &[]).await.expect("bind");

    let mut seen = Vec::new();
    loop {
        let rows = transaction
            .query_portal(&portal, 3)
            .await
            .expect("query_portal");
        let empty = rows.is_empty();
        for row in &rows {
            seen.push(row.get::<_, i32>(0));
        }
        if rows.len() < 3 || empty {
            break;
        }
    }
    assert_eq!(
        seen,
        (1..=10).collect::<Vec<i32>>(),
        "paging on the serialized loop lost or reordered rows"
    );
}

/// A notice raised by a statement the caller is awaiting must be delivered.
///
/// This is NOT the idle case. An IDLE serialized connection never reads, so a
/// notification arriving between requests is not delivered at all - that is
/// IO-2, still open in task #49. A notice raised BY the statement being
/// awaited travels while the loop is already reading, so it must arrive, and
/// pinning that keeps the fix for the idle case from being mistaken for the
/// whole of notice handling.
#[compio::test]
async fn a_notice_raised_by_an_awaited_statement_is_delivered() {
    use futures_util::StreamExt;

    let url = common::test_url();
    let config: Config = url.parse().expect("test DSN did not parse");
    let (client, mut connection, split_refused) =
        compio_postgres::test_utils::connect_serialized(&config)
            .await
            .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));
    let mut messages = connection.notifications();
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();

    client
        .batch_execute("DO $$ BEGIN RAISE NOTICE 'serialized notice'; END $$")
        .await
        .expect("raise notice");

    let delivered = compio::time::timeout(std::time::Duration::from_secs(5), messages.next()).await;
    assert!(
        split_refused.get(),
        "this test is not on the serialized loop"
    );
    match delivered {
        Ok(Some(compio_postgres::AsyncMessage::Notice(notice))) => {
            assert_eq!(notice.message(), "serialized notice");
        }
        other => panic!("the notice was not delivered on the serialized loop: {other:?}"),
    }
}

/// Dropping a query future mid-flight must not strand the session.
///
/// The serialized loop cannot read and write at once, so an abandoned request
/// is the case most likely to leave it mid-frame. If the session survives, the
/// next query answers; if it does not, this hangs rather than returning a
/// wrong value, which is why the assertion is bounded.
///
/// The sleep is SHORT on purpose. PostgreSQL runs one query at a time per
/// connection, so abandoning a long one leaves the server busy with it and the
/// follow-up cannot be answered until it finishes - a bound shorter than the
/// sleep then fails for a reason that has nothing to do with the driver. An
/// earlier version of this test used pg_sleep(30) against a 10s bound and
/// reported a stranded session that was simply still working.
#[compio::test]
async fn an_abandoned_query_leaves_the_serialized_session_usable() {
    let client = serialized_client().await;

    {
        let pending = client.query_one("SELECT pg_sleep(1)", &[]);
        // Give the request time to reach the wire, then abandon it.
        let _ = compio::time::timeout(std::time::Duration::from_millis(150), pending).await;
    }

    let recovered = compio::time::timeout(
        std::time::Duration::from_secs(10),
        client.query_one("SELECT 7::int4", &[]),
    )
    .await;

    match recovered {
        Ok(Ok(row)) => assert_eq!(row.get::<_, i32>(0), 7),
        Ok(Err(error)) => {
            // A retired session is an acceptable outcome; a WRONG ANSWER is
            // not, and neither is a hang.
            eprintln!(
                "the abandoned query retired the session: {}",
                common::error_chain(&error)
            );
        }
        Err(_) => panic!(
            "the serialized session neither answered nor failed after an \
             abandoned query; it is stranded mid-frame"
        ),
    }
}

/// A read timeout fires on this loop, and retires the session rather than
/// leaving it half-read.
///
/// The timeout is armed only while a response is owed, and the serialized loop
/// is where that bookkeeping is most delicate: it cannot read and write at
/// once, so the deadline has to be started and cleared around a single
/// interleaved sequence. A server that is merely slow must trip it; the point
/// of the assertion is that the call RETURNS rather than hanging.
#[compio::test]
async fn a_read_timeout_fires_and_retires_the_serialized_session() {
    let url = common::test_url();
    let mut config: Config = url.parse().expect("test DSN did not parse");
    config.read_timeout(std::time::Duration::from_millis(250));

    let (client, connection, split_refused) =
        compio_postgres::test_utils::connect_serialized(&config)
            .await
            .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();

    // Well inside the 250ms budget: the deadline must not fire on a healthy
    // exchange, or the assertion below would pass for the wrong reason.
    let row = client
        .query_one("SELECT 1::int4", &[])
        .await
        .expect("a prompt query must not trip the read timeout");
    assert_eq!(row.get::<_, i32>(0), 1);
    assert!(
        split_refused.get(),
        "this test is not on the serialized loop"
    );

    // Now outlast it. The generous outer bound makes this a hang detector: a
    // 250ms deadline that fires at all will fire long before 10s, and load
    // only pushes the measured time up.
    let started = std::time::Instant::now();
    let outcome = compio::time::timeout(
        std::time::Duration::from_secs(10),
        client.query_one("SELECT pg_sleep(3)", &[]),
    )
    .await
    .expect("the read timeout did not fire; the serialized session hung");
    let elapsed = started.elapsed();

    let Err(error) = outcome else {
        panic!("a query outlasting the read timeout must not succeed");
    };
    assert!(
        error.is_read_timeout(),
        "the failure was not reported as a read timeout: {}",
        common::error_chain(&error)
    );
    assert!(
        elapsed < std::time::Duration::from_secs(3),
        "the deadline fired after {elapsed:?}, which is not before the query \
         would have finished on its own"
    );
}

/// A cancelled query surfaces as 57014 on this loop, and the session survives
/// it.
///
/// The cancel itself travels on a SEPARATE connection - that part is
/// transport-independent. What is specific to this loop is the aftermath: the
/// running statement's ErrorResponse has to be read and matched to the request
/// that is still awaiting it, on a loop that cannot read and write at once. A
/// driver that mishandles it strands the session rather than returning an
/// error, so the follow-up query is the real assertion.
#[compio::test]
async fn a_cancelled_query_leaves_the_serialized_session_usable() {
    let url = common::test_url();
    let config: Config = url.parse().expect("test DSN did not parse");
    let (client, connection, split_refused) =
        compio_postgres::test_utils::connect_serialized(&config)
            .await
            .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();

    // Establish the session before timing anything, so the assertion below
    // cannot be satisfied by a connection that never got started.
    client
        .query_one("SELECT 1::int4", &[])
        .await
        .expect("the session must work before it is cancelled");
    assert!(
        split_refused.get(),
        "this test is not on the serialized loop"
    );

    let token = client.cancel_token();
    let canceller = compio::runtime::spawn(async move {
        // Long enough that the query is certainly executing, short enough that
        // the test does not sit on it.
        compio::time::sleep(std::time::Duration::from_millis(300)).await;
        token.cancel_query(common::suite_tls()).await
    });

    let outcome = compio::time::timeout(
        std::time::Duration::from_secs(20),
        client.query_one("SELECT pg_sleep(10)", &[]),
    )
    .await
    .expect("the cancelled query neither returned nor failed; the session is stranded");
    let cancel_result = canceller.await;

    let Err(error) = outcome else {
        panic!(
            "a cancelled query must not report success; the cancel itself said {cancel_result:?}"
        );
    };
    assert_eq!(
        error.code().map(compio_postgres::error::SqlState::code),
        Some("57014"),
        "the cancellation did not surface as query_canceled: {}",
        common::error_chain(&error)
    );

    let row = client
        .query_one("SELECT 2::int4", &[])
        .await
        .expect("the session must survive a cancelled query");
    assert_eq!(row.get::<_, i32>(0), 2);
}
