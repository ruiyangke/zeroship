//! Behaviour on the SERIALIZED connection loop, reached without TLS.
//!
//! `Connection::run` picks its loop by whether the transport splits into owned
//! halves. A plain socket splits and takes the multiplexed loop; a TLS stream
//! cannot - rustls keeps shared session state - so every TLS connection takes
//! the serialized one. The loops are not equivalent, so behaviour proven over
//! plaintext is not thereby proven over TLS, and until now the only way into
//! the serialized loop from a test was to stand up a TLS server. That is why
//! the two diverged with nothing going red (task #49).
//!
//! `test_utils::connect_serialized` makes that loop reachable over plaintext.
//! These tests are the parity net for the refactor that will delete the
//! serialized loop: they must keep passing when TLS and plaintext share one.

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
