//! What a caller is told when `COPY ... FROM STDIN` fails part way through.
//!
//! A COPY failure is reported by the SERVER, after rows the client already
//! streamed and stopped thinking about. That makes it the easiest place for a
//! driver to lose the diagnosis: the sink accepted every write without
//! complaint, and the error only surfaces when the stream is finished. If it
//! arrived as a bare disconnect the caller would have no way to tell a
//! malformed row from a dropped connection - and this session already found
//! two other paths where exactly that happened.
//!
//! MEASURED: it does not. The server's own SQLSTATE and message come back, and
//! the connection is still usable afterwards. These tests hold that.

#[allow(unused_imports)]
use crate::common;
use bytes::Bytes;
use common::{suite_tls, test_object_name, test_url};
use compio_postgres::{Client, CopyInSink, Error};
use futures_util::{Sink, SinkExt};
use std::future::poll_fn;
use std::pin::Pin;
use std::time::Duration;

async fn connected() -> Client {
    let (client, connection) = compio_postgres::connect(&test_url(), suite_tls())
        .await
        .expect("connect to the test server");
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();
    client
}

/// A table with a primary key and a NOT NULL, so several distinct server-side
/// failures are reachable against the same fixture.
async fn fixture(client: &Client) -> String {
    let table = test_object_name("copy_fail");
    client
        .batch_execute(&format!(
            "CREATE TABLE {table} (id int primary key, v text not null)"
        ))
        .await
        .expect("create the fixture table");
    table
}

/// Stream `lines` into `table` and return whatever finishing it reports.
async fn copy_lines(client: &Client, table: &str, lines: &[&str]) -> Result<u64, Error> {
    let sink = client
        .copy_in(&format!("COPY {table} FROM STDIN"))
        .await
        .expect("the COPY starts; the rows are what fail");
    futures_util::pin_mut!(sink);
    for line in lines {
        // A write failing here is legitimate - the server can reject the
        // stream before the client stops sending - so it is not an error in
        // this helper, only a reason to stop early and let `finish` report.
        if sink.send(Bytes::from((*line).to_owned())).await.is_err() {
            break;
        }
    }
    sink.finish().await
}

/// The connection has to survive a rejected COPY. A driver that left the
/// session wedged would turn one bad row into a dead connection.
async fn assert_still_usable(client: &Client, after: &str) {
    let one: i32 = client
        .query_one_scalar("SELECT 1::int4", &[])
        .await
        .unwrap_or_else(|error| panic!("the connection was unusable after {after}: {error}"));
    assert_eq!(one, 1);
}

async fn terminated_copy() -> (Client, Client, i32, Pin<Box<CopyInSink<Bytes>>>) {
    let victim = connected().await;
    let killer = connected().await;
    let pid: i32 = victim
        .query_one_scalar("SELECT pg_backend_pid()", &[])
        .await
        .expect("read the COPY backend PID");
    victim
        .batch_execute("CREATE TEMPORARY TABLE cpg_copy_producer_diagnosis (v int)")
        .await
        .expect("create the COPY termination fixture");
    let sink = victim
        .copy_in("COPY cpg_copy_producer_diagnosis (v) FROM STDIN")
        .await
        .expect("start COPY before terminating its backend");
    (victim, killer, pid, Box::pin(sink))
}

fn large_copy_rows(value: i32) -> Bytes {
    Bytes::from(format!("{value}\n").repeat(3_000))
}

async fn terminate_copy(killer: &Client, victim: &Client, pid: i32) {
    killer
        .batch_execute(&format!("SELECT pg_terminate_backend({pid})"))
        .await
        .expect("terminate the COPY backend");
    compio::time::timeout(Duration::from_secs(5), async {
        loop {
            if victim.is_closed() {
                break;
            }
            compio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("the terminated COPY connection task stayed open");
}

fn assert_copy_rejection(error: &Error, operation: &str) {
    assert_eq!(
        error.code().map(|code| code.code()),
        Some("57P01"),
        "{operation} discarded SQLSTATE 57P01: {}",
        common::error_chain(error),
    );
    assert!(
        !error.is_closed(),
        "{operation} replaced PostgreSQL's termination diagnosis with connection closed"
    );
}

#[compio::test]
async fn copy_in_poll_ready_prefers_the_queued_server_error() {
    let (victim, killer, pid, mut sink) = terminated_copy().await;
    terminate_copy(&killer, &victim, pid).await;

    let error = poll_fn(|cx| Sink::poll_ready(sink.as_mut(), cx))
        .await
        .expect_err("poll_ready accepted data after PostgreSQL rejected COPY input");
    assert_copy_rejection(&error, "poll_ready");
    drop(sink);
}

#[compio::test]
async fn copy_in_start_send_prefers_the_queued_server_error() {
    let (victim, killer, pid, mut sink) = terminated_copy().await;
    poll_fn(|cx| Sink::poll_ready(sink.as_mut(), cx))
        .await
        .expect("reserve producer capacity before terminating the backend");
    terminate_copy(&killer, &victim, pid).await;

    let error = Sink::start_send(sink.as_mut(), large_copy_rows(2))
        .expect_err("start_send accepted data after PostgreSQL rejected COPY input");
    assert_copy_rejection(&error, "start_send");
    drop(sink);
}

#[compio::test]
async fn copy_in_buffered_flush_prefers_the_queued_server_error() {
    let (victim, killer, pid, mut sink) = terminated_copy().await;
    Sink::start_send(sink.as_mut(), Bytes::from_static(b"2\n"))
        .expect("buffer the row that will be flushed after termination");
    terminate_copy(&killer, &victim, pid).await;

    let error = poll_fn(|cx| Sink::poll_flush(sink.as_mut(), cx))
        .await
        .expect_err("buffered flush accepted data after PostgreSQL rejected COPY input");
    assert_copy_rejection(&error, "buffered flush");
    drop(sink);
}

#[compio::test]
async fn copy_in_empty_flush_prefers_the_queued_server_error() {
    let (victim, killer, pid, mut sink) = terminated_copy().await;
    terminate_copy(&killer, &victim, pid).await;

    let error = poll_fn(|cx| Sink::poll_flush(sink.as_mut(), cx))
        .await
        .expect_err("empty flush reported success after PostgreSQL rejected COPY input");
    assert_copy_rejection(&error, "empty flush");
    drop(sink);
}

#[compio::test]
async fn a_malformed_row_reports_the_servers_own_error() {
    let client = connected().await;
    let table = fixture(&client).await;

    let error = copy_lines(
        &client,
        &table,
        &["0\tv0\n", "1\tv1\n", "notanint\tv2\n", "3\tv3\n"],
    )
    .await
    .expect_err("a non-integer in an int column cannot be accepted");

    assert_eq!(
        error.code().map(|code| code.code().to_owned()).as_deref(),
        Some("22P02"),
        "expected invalid_text_representation; got {error}"
    );
    let cause = std::error::Error::source(&error)
        .map(ToString::to_string)
        .unwrap_or_default();
    assert!(
        cause.contains("notanint"),
        "the error does not say WHICH value was rejected, so the caller \
         cannot find the bad row: {cause}"
    );

    assert_still_usable(&client, "a malformed COPY row").await;
    let _ = client
        .batch_execute(&format!("DROP TABLE IF EXISTS {table}"))
        .await;
}

/// A constraint violation is a different failure at a different stage - the
/// rows parse, and the server rejects them on insert - so it is worth its own
/// case rather than assuming one COPY error path serves both.
#[compio::test]
async fn a_constraint_violation_reports_its_sqlstate_and_detail() {
    let client = connected().await;
    let table = fixture(&client).await;

    let error = copy_lines(&client, &table, &["1\ta\n", "1\tb\n"])
        .await
        .expect_err("a duplicate primary key cannot be accepted");

    assert_eq!(
        error.code().map(|code| code.code().to_owned()).as_deref(),
        Some("23505"),
        "expected unique_violation; got {error}"
    );
    let cause = std::error::Error::source(&error)
        .map(ToString::to_string)
        .unwrap_or_default();
    assert!(
        cause.contains("Key (id)=(1)"),
        "the error drops the DETAIL naming the conflicting key: {cause}"
    );

    assert_still_usable(&client, "a COPY constraint violation").await;
    let _ = client
        .batch_execute(&format!("DROP TABLE IF EXISTS {table}"))
        .await;
}

/// THE CONTROL. The same fixture and the same helper with VALID rows must
/// succeed and report the row count - otherwise the two tests above would pass
/// for a COPY that never worked at all.
#[compio::test]
async fn valid_rows_copy_and_report_their_count() {
    let client = connected().await;
    let table = fixture(&client).await;

    let copied = copy_lines(&client, &table, &["1\ta\n", "2\tb\n", "3\tc\n"])
        .await
        .expect("three well-formed rows");
    assert_eq!(copied, 3, "COPY reported the wrong row count");

    let stored: i64 = client
        .query_one_scalar(&format!("SELECT count(*) FROM {table}"), &[])
        .await
        .expect("count the rows");
    assert_eq!(stored, 3, "the rows COPY reported are not the rows stored");

    let _ = client
        .batch_execute(&format!("DROP TABLE IF EXISTS {table}"))
        .await;
}

/// `finish` takes a pinned mutable reference rather than consuming the sink,
/// and `Sink::poll_close` promises a closed sink once it returns success. A
/// second terminal poll therefore observes local API state, not transport
/// death. It must remain idempotent, while attempts to add more data must not
/// claim that the still-usable PostgreSQL session closed.
#[compio::test]
async fn a_finished_copy_sink_does_not_report_connection_closed() {
    let client = connected().await;
    let table = fixture(&client).await;
    let sink = client
        .copy_in(&format!("COPY {table} FROM STDIN"))
        .await
        .expect("start the COPY");
    futures_util::pin_mut!(sink);
    sink.as_mut()
        .send(Bytes::from_static(b"1\ta\n"))
        .await
        .expect("send the COPY row");

    assert_eq!(sink.as_mut().finish().await.expect("finish the COPY"), 1);
    assert_eq!(
        sink.as_mut()
            .finish()
            .await
            .expect("repeat finish on the finished COPY"),
        1,
        "repeat finish forgot the completed COPY row count"
    );
    poll_fn(|cx| Sink::poll_close(sink.as_mut(), cx))
        .await
        .expect("close the already-finished COPY");
    poll_fn(|cx| Sink::poll_flush(sink.as_mut(), cx))
        .await
        .expect("flush the already-finished COPY");

    let error = poll_fn(|cx| Sink::poll_ready(sink.as_mut(), cx))
        .await
        .expect_err("a finished COPY accepted another row");
    assert!(
        !error.is_closed(),
        "finished COPY sink state was misreported as connection closed: {}",
        common::error_chain(&error),
    );
    assert_eq!(error.to_string(), "COPY IN sink is already finished");

    assert_still_usable(&client, "reusing a finished COPY sink").await;
    let _ = client
        .batch_execute(&format!("DROP TABLE IF EXISTS {table}"))
        .await;
}

/// A rejected COPY must leave NOTHING behind: it is one statement, so its
/// earlier rows go with the failure. Asserting this separately matters because
/// a driver that recovered the session by committing what it had would pass
/// every assertion above and silently half-load the table.
#[compio::test]
async fn a_failed_copy_stores_no_rows_at_all() {
    let client = connected().await;
    let table = fixture(&client).await;

    copy_lines(
        &client,
        &table,
        &["1\ta\n", "2\tb\n", "notanint\tc\n", "4\td\n"],
    )
    .await
    .expect_err("the third row is malformed");

    let stored: i64 = client
        .query_one_scalar(&format!("SELECT count(*) FROM {table}"), &[])
        .await
        .expect("count the rows");
    assert_eq!(
        stored, 0,
        "a failed COPY left {stored} rows behind; the two valid rows before \
         the bad one were committed"
    );

    let _ = client
        .batch_execute(&format!("DROP TABLE IF EXISTS {table}"))
        .await;
}

/// `CommandComplete` precedes the extended-protocol `Sync`. The implicit
/// transaction can still fail at that boundary when a deferred constraint is
/// checked, so the provisional row count is not success yet.
#[compio::test]
async fn copy_in_waits_for_sync_before_reporting_success() {
    let client = connected().await;
    let parent = test_object_name("copy_sync_parent");
    let child = test_object_name("copy_sync_child");
    client
        .batch_execute(&format!(
            "CREATE TABLE {parent} (id int PRIMARY KEY);
             CREATE TABLE {child} (
                 parent_id int REFERENCES {parent} (id)
                     DEFERRABLE INITIALLY DEFERRED
             )"
        ))
        .await
        .expect("create the deferred COPY fixture");

    let sink = client
        .copy_in(&format!("COPY {child} (parent_id) FROM STDIN"))
        .await
        .expect("start COPY IN before its deferred constraint is checked");
    futures_util::pin_mut!(sink);
    sink.as_mut()
        .send(Bytes::from_static(b"314159\n"))
        .await
        .expect("send the row before its deferred constraint is checked");
    let result = sink.as_mut().finish().await;
    assert!(
        !matches!(result, Ok(1)),
        "COPY IN reported success before Sync committed its implicit transaction: 1"
    );
    let error = result.expect_err("COPY IN accepted a deferred foreign-key violation");
    assert_eq!(
        error.code().map(|code| code.code()),
        Some("23503"),
        "the Sync failure lost its SQLSTATE: {}",
        common::error_chain(&error),
    );

    let stored: i64 = client
        .query_one_scalar(&format!("SELECT count(*) FROM {child}"), &[])
        .await
        .expect("the deferred COPY IN failure poisoned its connection");
    assert_eq!(
        stored, 0,
        "the failed implicit transaction committed its row"
    );

    let _ = client
        .batch_execute(&format!("DROP TABLE IF EXISTS {child}, {parent}"))
        .await;
}
