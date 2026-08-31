//! A `query_opt` that refuses a second row must drain the complete result.
//!
//! Returning `Err(row_count)` at the SECOND row abandons the `RowStream` while
//! PostgreSQL may still be executing the statement. The connection run loop
//! keeps the protocol in step, but a later `ErrorResponse` then has no receiver
//! and the caller sees the local row-count symptom instead of the server's
//! diagnosis. The accessor must remember multiplicity and keep polling until
//! the result ends: a server error wins, while clean completion still returns
//! the row-count error.
//!
//! `tests/query_claims.rs` already covers the COLUMN arity of
//! `query_opt_scalar`. Nothing covered the ROW count path or what the session
//! looks like afterwards.
//!
//! The resynchronisation assertions use a session whose `pg_backend_pid()` is
//! checked to be unchanged, so "the driver quietly opened a new connection"
//! cannot be mistaken for "the driver resynchronised this one".
//!
//! The two diagnostic tests are direct mutations: restoring the old early
//! return in either accessor changes its result from SQLSTATE 22012 to the
//! code-less local row-count error. The session-resynchronisation test remains
//! useful independently: pointing its follow-up query at the same range as the
//! abandoned result makes its assertion fail, so its distinct ranges are load
//! bearing rather than decoration.

use compio_postgres::types::{ToSql, Type};
use compio_postgres::{Client, error::SqlState};
use std::time::Duration;

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

/// A row-count refusal is only the final verdict once PostgreSQL has finished
/// the result. A later server error is the actual diagnosis of the statement.
#[compio::test]
async fn query_opt_prefers_a_later_server_error_to_row_count() {
    let url = test_url();
    let client = connect_client(&url).await;

    let failure = client
        .query_opt("SELECT 10 / (3 - g) FROM generate_series(1, 3) AS g", &[])
        .await
        .expect_err("the third row must fail with division by zero");

    assert_eq!(
        failure.code(),
        Some(&SqlState::DIVISION_BY_ZERO),
        "query_opt discarded SQLSTATE 22012 after seeing a second row: {failure}"
    );
}

/// The explicitly typed path has its own row-draining loop and must make the
/// same diagnostic choice as `query_opt`.
#[compio::test]
async fn query_typed_opt_prefers_a_later_server_error_to_row_count() {
    let url = test_url();
    let client = connect_client(&url).await;

    let failure = client
        .query_typed_opt("SELECT 10 / (3 - g) FROM generate_series(1, 3) AS g", &[])
        .await
        .expect_err("the third row must fail with division by zero");

    assert_eq!(
        failure.code(),
        Some(&SqlState::DIVISION_BY_ZERO),
        "query_typed_opt discarded SQLSTATE 22012 after seeing a second row: {failure}"
    );
}

/// A clean multi-row result must reach the typed accessor's local row-count
/// verdict. The existing later-error test cannot distinguish that verdict from
/// an implementation which simply forgets that it saw the second row.
#[compio::test]
async fn query_typed_opt_refuses_a_clean_multirow_result() {
    compio::time::timeout(Duration::from_secs(10), async {
        let url = test_url();
        let client = connect_client(&url).await;
        let limit = 3i32;
        let params: [(&(dyn ToSql + Sync), Type); 1] = [(&limit, Type::INT4)];
        const SQL: &str = "SELECT g::int4 FROM generate_series(1, $1::int4) AS g";

        let rows = client
            .query_typed(SQL, &params)
            .await
            .expect("the control query must produce a clean result");
        let values = rows
            .iter()
            .map(|row| row.get::<_, i32>(0))
            .collect::<Vec<_>>();
        assert_eq!(
            values,
            vec![1, 2, 3],
            "the fixture did not actually return multiple clean rows"
        );

        let failure = client
            .query_typed_opt(SQL, &params)
            .await
            .expect_err("query_typed_opt accepted three rows as one");
        assert!(
            failure.code().is_none(),
            "a clean multi-row result should produce the local row-count error: {failure}"
        );
    })
    .await
    .expect("typed optional row-count claim exceeded its 10 second deadline");
}

/// `query_one` delegates the multiplicity check to `query_opt`; checking only
/// its zero-row arm would still let it silently return the first of many rows.
#[compio::test]
async fn query_one_refuses_a_clean_multirow_result() {
    compio::time::timeout(Duration::from_secs(10), async {
        let url = test_url();
        let client = connect_client(&url).await;
        const SQL: &str = "SELECT g::int4 FROM generate_series(41, 42) AS g";

        let rows = client
            .query(SQL, &[])
            .await
            .expect("the control query must produce a clean result");
        let values = rows
            .iter()
            .map(|row| row.get::<_, i32>(0))
            .collect::<Vec<_>>();
        assert_eq!(
            values,
            vec![41, 42],
            "the fixture did not actually return two clean rows"
        );

        let failure = client
            .query_one(SQL, &[])
            .await
            .expect_err("query_one accepted two rows and returned the first");
        assert!(
            failure.code().is_none(),
            "a clean multi-row result should produce the local row-count error: {failure}"
        );
    })
    .await
    .expect("query_one row-count claim exceeded its 10 second deadline");
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
