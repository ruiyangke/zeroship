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
use compio_postgres::{Client, NoTls};
use std::time::Duration;

#[allow(dead_code)]
mod common;

const TEST_TIMEOUT: Duration = Duration::from_secs(20);
const ABORT_MARKER: &str = "simple query execution cannot supply COPY data";

fn test_url() -> Option<String> {
    common::env::get(common::env::TestEnvKey::PgTestUrl)
}

async fn connect_client(url: &str) -> Client {
    let (client, connection) = compio_postgres::connect(url, NoTls)
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

#[compio::test]
async fn batch_execute_finds_copy_in_after_copy_out_and_recovers() {
    compio::time::timeout(TEST_TIMEOUT, async {
        let Some(url) = test_url() else {
            return;
        };
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
        let Some(url) = test_url() else {
            return;
        };
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
        let Some(url) = test_url() else {
            return;
        };
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
