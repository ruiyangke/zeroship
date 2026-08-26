//! A `query_opt` that refuses a second row must leave the session in step.
//!
//! `Client::query_opt` returns `Err(row_count)` the moment a SECOND row
//! arrives, which means it abandons the `RowStream` with rows still queued on
//! the wire. That is the interesting part: the refusal is correct and
//! deliberate, but it hands the connection back mid-result. If those queued
//! rows were not drained, the NEXT query on the same session would read them
//! and return someone else's data -- a wrong answer with no error anywhere.
//!
//! `tests/query_claims.rs` already covers the COLUMN arity of
//! `query_opt_scalar`. Nothing covered the ROW count path or what the session
//! looks like afterwards.
//!
//! Every assertion below is on a session whose `pg_backend_pid()` is checked to
//! be unchanged, so "the driver quietly opened a new connection" cannot be
//! mistaken for "the driver resynchronised this one".
//!
//! WHAT THE MUTATIONS HERE DID AND DID NOT ESTABLISH, since a guard nobody has
//! seen fail is worth exactly as much as its evidence. Pointing the follow-up
//! query at the SAME range the abandoned result used turns the first test red,
//! so its assertion is live and the distinctness of the two ranges is load
//! bearing rather than decoration.
//!
//! I could NOT construct the failure itself. The drain is a property of the
//! run loop's structure: it routes a response through to `ReadyForQuery`
//! whether or not a receiver still exists, so leftover rows are unreachable
//! without restructuring that loop rather than flipping a condition in it. So
//! read this file as a regression guard against a future change to that
//! structure, NOT as evidence that the hazard was ever reachable. If you do
//! rework the run loop, this is one of the tests that should be able to catch
//! you, and it is worth re-checking that it still can.

use compio_postgres::Client;

#[allow(unused_imports)]
use crate::common;

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

async fn backend_pid(client: &Client) -> i32 {
    client
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .expect("read the backend pid")
        .get::<_, i32>(0)
}

/// After `query_opt` refuses a multi-row result, the session still answers
/// correctly.
#[compio::test]
async fn a_row_count_refusal_leaves_the_session_usable_and_in_step() {
    let url = test_url();
    let client = connect_client(&url).await;
    let pid_before = backend_pid(&client).await;

    // Three rows into a one-row accessor. The rows are distinctive so a
    // leftover would be recognisable in the next query's output rather than
    // merely wrong.
    client
        .query_opt("SELECT g FROM generate_series(101, 103) AS g", &[])
        .await
        .expect_err("query_opt must refuse a second row");

    // The crux: the very next query must return ITS OWN rows.
    let rows = client
        .query("SELECT g FROM generate_series(1, 3) AS g", &[])
        .await
        .expect("the session must still be usable after a row-count refusal");
    let values: Vec<i32> = rows.iter().map(|row| row.get(0)).collect();
    assert_eq!(
        values,
        vec![1, 2, 3],
        "the next query read rows left over from the abandoned result"
    );

    assert_eq!(
        backend_pid(&client).await,
        pid_before,
        "this must be the SAME backend; a replaced connection would explain a \
         clean read without the driver having resynchronised anything"
    );
}

/// One variable away: exactly one row is not a refusal, and the session is
/// equally usable afterwards.
///
/// Without this, the test above could be satisfied by a driver that tore the
/// connection down and rebuilt it on every `query_opt`.
#[compio::test]
async fn a_single_row_query_opt_succeeds_and_leaves_the_session_usable() {
    let url = test_url();
    let client = connect_client(&url).await;
    let pid_before = backend_pid(&client).await;

    let row = client
        .query_opt("SELECT 42", &[])
        .await
        .expect("one row is not a refusal")
        .expect("one row is not None");
    assert_eq!(row.get::<_, i32>(0), 42);

    let rows = client
        .query("SELECT g FROM generate_series(1, 3) AS g", &[])
        .await
        .expect("session usable");
    let values: Vec<i32> = rows.iter().map(|row| row.get(0)).collect();
    assert_eq!(values, vec![1, 2, 3]);

    assert_eq!(backend_pid(&client).await, pid_before, "same backend");
}

/// Zero rows is `Ok(None)`, not a refusal -- the third arm of the same method.
#[compio::test]
async fn zero_rows_is_none_rather_than_a_row_count_error() {
    let url = test_url();
    let client = connect_client(&url).await;

    let outcome = client
        .query_opt("SELECT 1 WHERE false", &[])
        .await
        .expect("zero rows is not an error for query_opt");
    assert!(outcome.is_none(), "zero rows must be None");

    // `query_one` is the same call with the None turned into an error, so the
    // two arms are pinned against each other rather than in isolation.
    client
        .query_one("SELECT 1 WHERE false", &[])
        .await
        .expect_err("query_one must refuse zero rows");
}
