//! `Transaction` forwards most of its surface straight to the borrowed
//! `Client`, and three of those forwards are reached by no test at all:
//! `execute_typed`, `copy_out` and `cancel_token`.
//!
//! A forwarding method is the easiest place in the crate to be silently wrong,
//! because the obvious assertion does not test the thing that matters. Every
//! one of these returns a plausible value even when it is forwarding to the
//! wrong place: `execute_typed` still reports one row affected, `copy_out`
//! still yields bytes, `cancel_token` still hands back a token. What a
//! misdirected forward changes is WHICH SESSION the work lands on, and that is
//! invisible to a test that only inspects the return value.
//!
//! So each test below asserts transaction membership rather than the result:
//!
//! - `execute_typed`'s insert must disappear on ROLLBACK. Work sent down
//!   another session would survive it.
//! - `copy_out` must see rows the surrounding transaction has not committed.
//!   Another session could not see them at all.
//! - `cancel_token` must cancel a query running on the transaction's own
//!   backend, which is the only claim a token can make that a wrong PID fails.
//!
//! The commit arm in the first test is the one-variable control: same table,
//! same statement, same parameters, opposite ending. Without it, "the row is
//! gone" also passes for an `execute_typed` that silently inserted nothing.

use compio_postgres::Client;
use compio_postgres::error::SqlState;
use compio_postgres::types::Type;
use futures_util::TryStreamExt;
use std::time::Duration;

#[allow(unused_imports)]
use crate::common;

const OPERATION_TIMEOUT: Duration = Duration::from_secs(20);

fn test_url() -> String {
    common::test_url()
}

/// Cancellation needs a TCP session it can reconnect to, matching
/// `cancel_request.rs`.
fn plaintext_url() -> String {
    let url = test_url();
    let separator = if url.contains('?') { '&' } else { '?' };
    format!("{url}{separator}sslmode=disable")
}

async fn connect_client(url: &str) -> Client {
    let (client, connection) = compio_postgres::connect(url, common::suite_tls())
        .await
        .expect("connect to PostgreSQL");
    compio::runtime::spawn(async move {
        if let Err(error) = connection.run().await {
            eprintln!("connection error: {}", common::error_chain(&error));
        }
    })
    .detach();
    client
}

async fn row_count(client: &Client, table: &str) -> i64 {
    client
        .query_one_scalar(&format!("SELECT count(*) FROM {table}"), &[])
        .await
        .expect("count the rows the table currently holds")
}

/// `Transaction::execute_typed` must run inside the transaction, so its insert
/// is undone by ROLLBACK and kept by COMMIT.
#[compio::test]
async fn execute_typed_inside_a_transaction_is_rolled_back_with_it() {
    let url = test_url();
    let mut client = connect_client(&url).await;
    let table = common::test_object_name("cpg_txn_exec_typed");
    client
        .batch_execute(&format!("CREATE TEMPORARY TABLE {table} (v int)"))
        .await
        .expect("create the temporary table on this session");

    let insert = format!("INSERT INTO {table} (v) VALUES ($1)");

    let transaction = client.transaction().await.expect("begin the transaction");
    let affected = transaction
        .execute_typed(&insert, &[(&7i32, Type::INT4)])
        .await
        .expect("execute_typed inside the transaction");
    assert_eq!(
        affected, 1,
        "execute_typed must report the one row it inserted"
    );
    transaction
        .rollback()
        .await
        .expect("roll the transaction back");

    assert_eq!(
        row_count(&client, &table).await,
        0,
        "the row inserted by execute_typed survived ROLLBACK, so it was not \
         sent on the transaction's own session"
    );

    // Control: identical in every respect but the ending.
    let transaction = client.transaction().await.expect("begin the transaction");
    transaction
        .execute_typed(&insert, &[(&7i32, Type::INT4)])
        .await
        .expect("execute_typed inside the committed transaction");
    transaction.commit().await.expect("commit the transaction");

    assert_eq!(
        row_count(&client, &table).await,
        1,
        "execute_typed inserted nothing, which would make the ROLLBACK arm \
         above pass for the wrong reason"
    );
}

/// `Transaction::copy_out` must read through the transaction, so it observes
/// rows that transaction has not committed.
#[compio::test]
async fn copy_out_inside_a_transaction_sees_that_transactions_uncommitted_rows() {
    let url = test_url();
    let mut client = connect_client(&url).await;
    let table = common::test_object_name("cpg_txn_copy_out");
    client
        .batch_execute(&format!("CREATE TEMPORARY TABLE {table} (v int)"))
        .await
        .expect("create the temporary table on this session");

    let transaction = client.transaction().await.expect("begin the transaction");
    transaction
        .batch_execute(&format!("INSERT INTO {table} (v) VALUES (11), (22)"))
        .await
        .expect("insert the uncommitted rows");

    let stream = transaction
        .copy_out(&format!(
            "COPY (SELECT v FROM {table} ORDER BY v) TO STDOUT"
        ))
        .await
        .expect("start COPY OUT inside the transaction");
    let mut stream = Box::pin(stream);
    let mut body = Vec::new();
    while let Some(chunk) = stream
        .as_mut()
        .try_next()
        .await
        .expect("read the COPY OUT stream")
    {
        body.extend_from_slice(&chunk);
    }

    assert_eq!(
        String::from_utf8(body).expect("COPY OUT text is UTF-8"),
        "11\n22\n",
        "copy_out did not see the transaction's uncommitted rows, so it did \
         not read through the transaction"
    );

    transaction
        .rollback()
        .await
        .expect("roll the transaction back");
    assert_eq!(
        row_count(&client, &table).await,
        0,
        "the rows survived ROLLBACK, so draining copy_out ended the \
         transaction early"
    );

    // What this does NOT catch: a COMMIT issued AFTER the stream is handed
    // back but before it is drained. That mutation was tried and the test
    // stayed green, because the COMMIT is queued behind an unfinished COPY and
    // fails on its own. Only a commit that lands BEFORE the copy starts moves
    // the assertion above.
}

async fn wait_until_pg_sleep_is_running(observer: &Client, pid: i32, marker: &str) {
    compio::time::timeout(OPERATION_TIMEOUT, async {
        loop {
            let running: bool = observer
                .query_one_scalar(
                    "SELECT EXISTS (\
                         SELECT 1 \
                         FROM pg_stat_activity \
                         WHERE pid = $1 \
                           AND state = 'active' \
                           AND wait_event_type = 'Timeout' \
                           AND wait_event = 'PgSleep' \
                           AND query LIKE '%' || $2 || '%'\
                     )",
                    &[&pid, &marker],
                )
                .await
                .expect("poll pg_stat_activity for the target query");
            if running {
                return;
            }
        }
    })
    .await
    .expect("the transaction's pg_sleep never appeared in pg_stat_activity");
}

/// `Transaction::cancel_token` must describe the transaction's own backend.
/// A token carrying any other PID cancels nothing, and `pg_sleep` runs to
/// completion instead of failing with 57014.
#[compio::test]
async fn a_transactions_cancel_token_cancels_that_transactions_query() {
    const MARKER: &str = "cpg_txn_cancel_token";

    let url = plaintext_url();
    let mut client = connect_client(&url).await;
    let observer = connect_client(&url).await;
    let pid = client.process_id();

    let transaction = client.transaction().await.expect("begin the transaction");
    let token = transaction.cancel_token();

    let cancel_task = compio::runtime::spawn(async move {
        wait_until_pg_sleep_is_running(&observer, pid, MARKER).await;
        token
            .cancel_query(common::suite_tls())
            .await
            .expect("send the CancelRequest");
    });

    let sleep_sql = format!("SELECT pg_sleep(30) /* {MARKER} */");
    let query = transaction.batch_execute(&sleep_sql);
    let (query_result, cancel_result) = compio::time::timeout(
        OPERATION_TIMEOUT,
        futures_util::future::join(query, cancel_task),
    )
    .await
    .expect("the transaction's own token did not interrupt its pg_sleep");

    cancel_result.expect("the CancelRequest task panicked or was cancelled");
    let error = query_result.expect_err("pg_sleep must fail once cancelled");
    assert_eq!(
        error.code().map(SqlState::code),
        Some("57014"),
        "expected 57014 query_canceled, got {}",
        common::error_chain(&error)
    );
}
