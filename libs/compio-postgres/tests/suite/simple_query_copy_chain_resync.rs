//! A rejected COPY-OUT result must still be drained far enough to find a later
//! COPY-IN statement in the same simple-query batch.
//!
//! PostgreSQL runs every statement in one simple `Query` message before its
//! single trailing `ReadyForQuery`. A terminal `COPY ... TO STDOUT` completes
//! without frontend help, but if it is followed by `COPY ... FROM STDIN`, the
//! server stops and waits. Returning at the first `CopyOutResponse` therefore
//! hides the later `CopyInResponse` that the driver must abort.
//!
//! The control changes only the second COPY direction to `TO STDOUT`. Both
//! copies then finish autonomously, so same-client reuse was safe before the
//! fix and must remain safe when the recovery code is mutation-removed.

use compio_postgres::error::SqlState;
use compio_postgres::{Client, SimpleQueryMessage, SimpleQueryStream};
use futures_util::TryStreamExt;
use std::time::Duration;

#[allow(unused_imports)]
use crate::common;

const TEST_TIMEOUT: Duration = Duration::from_secs(20);
const ABORT_MARKER: &str = "simple query execution cannot supply COPY data";

fn test_url() -> String {
    common::test_url()
}

async fn connect_client(url: &str) -> Client {
    let (client, connection) = compio_postgres::connect(url, common::suite_tls())
        .await
        .expect("connect to PostgreSQL");
    compio::runtime::spawn(async move {
        if let Err(error) = connection.run().await {
            eprintln!("connection error: {error}");
        }
    })
    .detach();
    client
}

async fn copy_table(client: &Client, suffix: &str) -> String {
    let table = common::test_object_name(&format!("cpg_simple_copy_chain_{suffix}"));
    client
        .batch_execute(&format!(
            "CREATE TEMPORARY TABLE {table} (v int); INSERT INTO {table} VALUES (1)"
        ))
        .await
        .expect("create and seed the COPY table");
    table
}

async fn assert_simple_scalar(stream: SimpleQueryStream, expected: &str, failure: &str) {
    let messages: Vec<_> = stream.try_collect().await.expect(failure);
    let value = messages.iter().find_map(|message| match message {
        SimpleQueryMessage::Row(row) => row.get(0),
        _ => None,
    });
    assert_eq!(value, Some(expected));
}

#[compio::test]
async fn batch_execute_finds_copy_in_after_copy_out_and_recovers() {
    compio::time::timeout(TEST_TIMEOUT, async {
        let url = test_url();
        let client = connect_client(&url).await;
        let table = copy_table(&client, "batch").await;

        let failure = client
            .batch_execute(&format!(
                "COPY {table} TO STDOUT; COPY {table} FROM STDIN"
            ))
            .await
            .expect_err("batch_execute cannot return COPY output or feed COPY input");

        let value: i32 = client
            .query_one_scalar("SELECT 42::int4", &[])
            .await
            .expect("the hidden COPY-IN desynchronised batch_execute's session");
        assert_eq!(value, 42);
        assert_eq!(failure.code(), Some(&SqlState::QUERY_CANCELED));
        assert!(
            common::error_chain(&failure).contains(ABORT_MARKER),
            "the later COPY-IN was not aborted by this driver: {}",
            common::error_chain(&failure)
        );
        assert_eq!(
            client.transaction_status(),
            Some(compio_postgres::TransactionStatus::Idle)
        );
    })
    .await
    .expect("batch simple-query COPY chain exceeded its watchdog");
}

#[compio::test]
async fn simple_query_finds_copy_in_after_copy_out_and_recovers() {
    compio::time::timeout(TEST_TIMEOUT, async {
        let url = test_url();
        let client = connect_client(&url).await;
        let table = copy_table(&client, "stream").await;

        let failure = client
            .simple_query(&format!(
                "COPY {table} TO STDOUT; COPY {table} FROM STDIN"
            ))
            .await
            .expect_err("simple_query cannot return COPY output or feed COPY input");

        let value: i32 = client
            .query_one_scalar("SELECT 43::int4", &[])
            .await
            .expect("the hidden COPY-IN desynchronised simple_query's session");
        assert_eq!(value, 43);
        assert_eq!(failure.code(), Some(&SqlState::QUERY_CANCELED));
        assert!(
            common::error_chain(&failure).contains(ABORT_MARKER),
            "the later COPY-IN was not aborted by this driver: {}",
            common::error_chain(&failure)
        );
    })
    .await
    .expect("streaming simple-query COPY chain exceeded its watchdog");
}

/// CONTROL: changing only the second direction keeps the server out of every
/// frontend-input state. The API still rejects COPY output, but its session is
/// already at `ReadyForQuery` before the following request runs.
#[compio::test]
async fn two_copy_out_statements_were_already_synchronised() {
    compio::time::timeout(TEST_TIMEOUT, async {
        let url = test_url();
        let client = connect_client(&url).await;
        let table = copy_table(&client, "control").await;

        client
            .batch_execute(&format!(
                "COPY {table} TO STDOUT; COPY {table} TO STDOUT"
            ))
            .await
            .expect_err("batch_execute unexpectedly decoded COPY output");

        let value: i32 = client
            .query_one_scalar("SELECT 44::int4", &[])
            .await
            .expect("two autonomous COPY-OUT statements desynchronised the session");
        assert_eq!(value, 44);
        assert_eq!(
            client.transaction_status(),
            Some(compio_postgres::TransactionStatus::Idle)
        );
    })
    .await
    .expect("two-COPY-OUT control exceeded its watchdog");
}

/// Dropping the raw stream after an ordinary result must not abandon a later
/// COPY-IN statement that PostgreSQL has not reached yet. The large first row
/// makes PostgreSQL flush that result before `pg_sleep`, so the stream can be
/// dropped and the follow-up query can be queued while the server is still
/// between the yielded item and `CopyInResponse`.
#[compio::test]
async fn dropping_a_partial_simple_query_stream_recovers_a_later_copy_in() {
    compio::time::timeout(TEST_TIMEOUT, async {
        let url = test_url();
        let client = connect_client(&url).await;
        let table = copy_table(&client, "dropped_stream").await;

        let stream = client
            .simple_query_raw(&format!(
                "SELECT repeat('x', 32768); SELECT pg_sleep(0.5); \
                 COPY {table} FROM STDIN"
            ))
            .await
            .expect("start the multi-statement simple query");
        let mut stream = Box::pin(stream);
        stream
            .as_mut()
            .try_next()
            .await
            .expect("read the first ordinary response item")
            .expect("the first statement produced no response item");
        let follow_up = client
            .simple_query_raw("SELECT 45")
            .await
            .expect("enqueue the ordinary follow-up before dropping the first stream");
        drop(stream);

        assert_simple_scalar(
            follow_up,
            "45",
            "dropping the stream abandoned its later COPY-IN response",
        )
        .await;
        assert_eq!(
            client.transaction_status(),
            Some(compio_postgres::TransactionStatus::Idle)
        );
    })
    .await
    .expect("dropped simple-query stream recovery exceeded its watchdog");
}

/// CONTROL: changing only the final COPY direction makes every remaining
/// statement server-driven, so the connection was already reusable without an
/// abandoned-stream drain and must stay green when that drain is removed.
#[compio::test]
async fn dropping_a_partial_simple_query_stream_before_copy_out_stays_usable() {
    compio::time::timeout(TEST_TIMEOUT, async {
        let url = test_url();
        let client = connect_client(&url).await;
        let table = copy_table(&client, "dropped_stream_control").await;

        let stream = client
            .simple_query_raw(&format!(
                "SELECT repeat('x', 32768); SELECT pg_sleep(0.5); \
                 COPY {table} TO STDOUT"
            ))
            .await
            .expect("start the control simple query");
        let mut stream = Box::pin(stream);
        stream
            .as_mut()
            .try_next()
            .await
            .expect("read the first ordinary control response item")
            .expect("the control's first statement produced no response item");
        let follow_up = client
            .simple_query_raw("SELECT 45")
            .await
            .expect("enqueue the control follow-up before dropping the first stream");
        drop(stream);

        assert_simple_scalar(
            follow_up,
            "45",
            "an autonomous COPY-OUT response desynchronised the session",
        )
        .await;
        assert_eq!(
            client.transaction_status(),
            Some(compio_postgres::TransactionStatus::Idle)
        );
    })
    .await
    .expect("dropped COPY-OUT control exceeded its watchdog");
}

#[compio::test]
async fn drop_recovery_stays_usable_when_copy_in_is_rejected_before_start() {
    compio::time::timeout(TEST_TIMEOUT, async {
        let url = test_url();
        let client = connect_client(&url).await;
        let missing = common::test_object_name("cpg_simple_copy_chain_missing");

        let stream = client
            .simple_query_raw(&format!(
                "SELECT repeat('x', 32768); SELECT pg_sleep(0.5); \
                 COPY {missing} FROM STDIN"
            ))
            .await
            .expect("start the rejected COPY-IN batch");
        let mut stream = Box::pin(stream);
        stream
            .as_mut()
            .try_next()
            .await
            .expect("read the first response before the rejected COPY-IN")
            .expect("the first statement produced no response item");
        drop(stream);

        let value: i32 = client
            .query_one_scalar("SELECT 47::int4", &[])
            .await
            .expect("drop recovery poisoned a COPY-IN rejected before copy mode");
        assert_eq!(value, 47);
    })
    .await
    .expect("rejected COPY-IN drop recovery exceeded its watchdog");
}

#[compio::test]
async fn drop_recovery_tracks_nonconforming_string_escapes_before_copy_in() {
    compio::time::timeout(TEST_TIMEOUT, async {
        let url = test_url();
        let client = connect_client(&url).await;
        let table = copy_table(&client, "backslash_strings").await;
        client
            .batch_execute("SET standard_conforming_strings = off")
            .await
            .expect("enable ordinary-string backslash escapes");

        // PostgreSQL treats the quote after the backslash as escaped, so the
        // first statement's second literal ends after `2`. A lexer assuming
        // standard-conforming strings instead opens a new literal there and
        // incorrectly hides the real COPY statement inside it.
        let stream = client
            .simple_query_raw(&format!(
                r"SELECT repeat('x', 32768), 'x\'; SELECT 2';
                  SELECT pg_sleep(0.5); COPY {table} FROM STDIN"
            ))
            .await
            .expect("start the backslash-string COPY-IN batch");
        let mut stream = Box::pin(stream);
        stream
            .as_mut()
            .try_next()
            .await
            .expect("read the first backslash-string response item")
            .expect("the backslash-string statement produced no response item");
        drop(stream);

        let value: i32 = client
            .query_one_scalar("SELECT 48::int4", &[])
            .await
            .expect("the string-mode mismatch hid a later COPY-IN response");
        assert_eq!(value, 48);
    })
    .await
    .expect("backslash-string COPY-IN recovery exceeded its watchdog");
}

#[compio::test]
async fn drop_recovery_skips_unicode_dollar_quotes_before_copy_in() {
    compio::time::timeout(TEST_TIMEOUT, async {
        let url = test_url();
        let client = connect_client(&url).await;
        let table = copy_table(&client, "unicode_dollar_quote").await;

        let stream = client
            .simple_query_raw(&format!(
                "SELECT repeat('x', 32768), $é$'$é$; \
                 SELECT pg_sleep(0.5); COPY {table} FROM STDIN"
            ))
            .await
            .expect("start the Unicode dollar-quote COPY-IN batch");
        let mut stream = Box::pin(stream);
        stream
            .as_mut()
            .try_next()
            .await
            .expect("read the first Unicode dollar-quote response item")
            .expect("the Unicode dollar-quote statement produced no response item");
        drop(stream);

        let value: i32 = client
            .query_one_scalar("SELECT 49::int4", &[])
            .await
            .expect("the Unicode dollar quote hid a later COPY-IN response");
        assert_eq!(value, 49);
    })
    .await
    .expect("Unicode dollar-quote COPY-IN recovery exceeded its watchdog");
}

#[compio::test]
async fn speculative_copy_recovery_does_not_abort_a_healthy_transaction() {
    compio::time::timeout(TEST_TIMEOUT, async {
        let url = test_url();
        let client = connect_client(&url).await;
        client
            .batch_execute("SET standard_conforming_strings = on; BEGIN")
            .await
            .expect("start transaction with standard strings");

        client
            .simple_query(r"SELECT 'a\''; COPY t FROM STDIN'; SELECT 1")
            .await
            .expect("the alternate string mode changed query semantics");

        let value: i32 = client
            .query_one_scalar("SELECT 50::int4", &[])
            .await
            .expect("speculative COPY recovery aborted a healthy transaction");
        assert_eq!(value, 50);
        client.batch_execute("ROLLBACK").await.expect("rollback");
    })
    .await
    .expect("speculative COPY transaction control exceeded its watchdog");
}
