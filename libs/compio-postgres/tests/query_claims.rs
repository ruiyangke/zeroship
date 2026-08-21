//! Runtime claims made by the query and COPY APIs.

use bytes::Bytes;
use compio_postgres::{Client, NoTls};
use futures_util::{SinkExt, TryStreamExt};
use std::time::Duration;

#[allow(dead_code)]
mod common;

const TEST_WATCHDOG: Duration = Duration::from_secs(10);

fn test_url() -> String {
    common::env::get(common::env::TestEnvKey::PgTestUrl)
        .unwrap_or_else(|| "postgres://postgres:zeroship@localhost:5440/zeroship".to_string())
}

async fn connect() -> Client {
    let url = test_url();
    let (client, connection) = compio_postgres::connect(&url, NoTls)
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

#[compio::test]
async fn copy_in_close_commits_input_and_keeps_the_client_usable() {
    compio::time::timeout(TEST_WATCHDOG, async {
        let client = connect().await;
        let process_id = client.process_id();
        client
            .batch_execute("CREATE TEMP TABLE query_claims_copy_close (n int4 NOT NULL)")
            .await
            .expect("create COPY close fixture");

        let sink = client
            .copy_in::<_, Bytes>("COPY query_claims_copy_close (n) FROM STDIN")
            .await
            .expect("start COPY input");
        let mut sink = Box::pin(sink);
        sink.as_mut()
            .send(Bytes::from_static(b"11\n31\n"))
            .await
            .expect("send COPY input");
        sink.as_mut().close().await.expect("close COPY input");

        let row = client
            .query_one(
                "SELECT pg_backend_pid(), count(*)::int8, sum(n)::int8 \
                 FROM query_claims_copy_close",
                &[],
            )
            .await
            .expect("reuse the same client after closing COPY input");
        assert_eq!(row.get::<_, i32>(0), process_id);
        assert_eq!(row.get::<_, i64>(1), 2);
        assert_eq!(row.get::<_, i64>(2), 42);
    })
    .await
    .expect("COPY close claim exceeded its watchdog");
}

#[compio::test]
async fn query_text_params_coerces_by_position_and_decodes_binary_results() {
    compio::time::timeout(TEST_WATCHDOG, async {
        let client = connect().await;
        client
            .batch_execute(
                "CREATE TEMP TABLE query_claims_text_query (\
                     n int4 NOT NULL, enabled bool NOT NULL\
                 )",
            )
            .await
            .expect("create query_text_params fixture");

        let rows = client
            .query_text_params(
                "INSERT INTO query_claims_text_query (n, enabled) \
                 VALUES ($1, $2) RETURNING n, enabled",
                &["42", "true"],
            )
            .await
            .expect("text parameters did not coerce to their target columns");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get::<_, i32>("n"), 42);
        assert!(rows[0].get::<_, bool>("enabled"));
    })
    .await
    .expect("query_text_params claim exceeded its watchdog");
}

#[compio::test]
async fn execute_text_params_coerces_nulls_and_returns_the_affected_count() {
    compio::time::timeout(TEST_WATCHDOG, async {
        let client = connect().await;
        client
            .batch_execute(
                "CREATE TEMP TABLE query_claims_text_execute (\
                     n int4 NOT NULL, optional_n int4\
                 )",
            )
            .await
            .expect("create execute_text_params fixture");

        let affected = client
            .execute_text_params(
                "INSERT INTO query_claims_text_execute (n, optional_n) VALUES ($1, $2)",
                &[Some("42".to_string()), None],
            )
            .await
            .expect("execute text parameters");
        assert_eq!(affected, 1);

        let row = client
            .query_one(
                "SELECT n, optional_n FROM query_claims_text_execute",
                &[],
            )
            .await
            .expect("read execute_text_params result");
        assert_eq!(row.get::<_, i32>("n"), 42);
        assert_eq!(row.get::<_, Option<i32>>("optional_n"), None);
    })
    .await
    .expect("execute_text_params claim exceeded its watchdog");
}

#[compio::test]
async fn row_stream_reports_affected_rows_only_after_exhaustion() {
    compio::time::timeout(TEST_WATCHDOG, async {
        let client = connect().await;
        client
            .batch_execute(
                "CREATE TEMP TABLE query_claims_rows_affected (n int4 NOT NULL); \
                 INSERT INTO query_claims_rows_affected VALUES (1), (2), (3)",
            )
            .await
            .expect("create rows_affected fixture");

        let stream = client
            .query_raw(
                "UPDATE query_claims_rows_affected SET n = n + 10 RETURNING n",
                std::iter::empty::<&i32>(),
            )
            .await
            .expect("start UPDATE row stream");
        let mut stream = Box::pin(stream);
        assert_eq!(stream.as_ref().get_ref().rows_affected(), None);

        let mut returned = Vec::new();
        while let Some(row) = stream
            .as_mut()
            .try_next()
            .await
            .expect("consume UPDATE row stream")
        {
            returned.push(row.get::<_, i32>(0));
            assert_eq!(
                stream.as_ref().get_ref().rows_affected(),
                None,
                "rows_affected became visible before stream exhaustion"
            );
        }
        returned.sort_unstable();
        assert_eq!(returned, [11, 12, 13]);
        assert_eq!(stream.as_ref().get_ref().rows_affected(), Some(3));
    })
    .await
    .expect("RowStream rows_affected claim exceeded its watchdog");
}
