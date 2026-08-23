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
            .query_one("SELECT n, optional_n FROM query_claims_text_execute", &[])
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

/// `query_scalar` rejects a multi-column query WHATEVER the row count.
///
/// Its arity check reads `rows.first()`, so it was decided by the DATA rather
/// than by the statement: `SELECT 1, 2` errored, and `SELECT 1, 2 WHERE false`
/// -- the same query, the same two columns -- returned `Ok(vec![])`. A caller
/// whose test fixture happened to be empty got a green, and the error arrived
/// once real rows existed. That is the worst shape for a wrong verdict: it
/// hides in exactly the setup people write tests against.
///
/// The column count is a property of the RowDescription and is known before any
/// row arrives, so both arms below are answerable without data.
#[compio::test]
async fn query_scalar_rejects_extra_columns_even_with_no_rows() {
    compio::time::timeout(TEST_WATCHDOG, async {
        let client = connect().await;

        // The populated error was captured only to be INTERPOLATED INTO the
        // empty case's failure message until 2026-08-23 - never inspected - so
        // the test proved both calls erred, not that they erred for the same
        // reason. Both must name the column count, which is the claim.
        let populated = common::error_chain(
            &client
                .query_scalar::<i32, _>("SELECT 1, 2", &[])
                .await
                .expect_err("two columns cannot be one scalar"),
        );
        assert!(
            populated.contains("unexpected number of columns"),
            "a two-column scalar query must fail on the column count: {populated:?}"
        );

        let empty = common::error_chain(
            &client
                .query_scalar::<i32, _>("SELECT 1, 2 WHERE false", &[])
                .await
                .expect_err(
                    "an empty result set hid the arity error the populated one reports; \
                     the column count does not depend on the rows",
                ),
        );
        assert_eq!(
            empty, populated,
            "the same arity violation must read the same with and without rows"
        );

        // The one-variable partner: a genuine single-column query must still
        // work, or "reject extra columns" is satisfied by rejecting everything.
        let ok: Vec<i32> = client
            .query_scalar("SELECT g FROM generate_series(1, 3) g", &[])
            .await
            .expect("a single-column query is what this API is for");
        assert_eq!(ok, vec![1, 2, 3]);

        let none: Vec<i32> = client
            .query_scalar("SELECT 1 WHERE false", &[])
            .await
            .expect("an empty single-column result is not an error");
        assert!(none.is_empty());
    })
    .await
    .expect("query_scalar arity test exceeded its watchdog");
}

/// `query_opt_scalar` rules on arity the same way, and for the same reason.
///
/// It read the column count off the returned row, so with no row there was
/// nothing to read and a two-column query returning nothing was accepted. Both
/// scalar helpers had this; neither had any test at all, which is how a public
/// API keeps a data-dependent verdict.
#[compio::test]
async fn query_opt_scalar_rejects_extra_columns_even_with_no_rows() {
    compio::time::timeout(TEST_WATCHDOG, async {
        let client = connect().await;

        // THREE BARE `is_err()` HERE UNTIL 2026-08-23, for three DIFFERENT
        // rules: two about columns and, below, one about rows. "An error
        // occurred" cannot tell them apart, so a helper that collapsed the two
        // verdicts into one satisfied all three. The driver does distinguish
        // them -- `unexpected number of columns` and `unexpected number of
        // rows` -- so each assertion now names its own and denies the other.
        for (sql, why) in [
            ("SELECT 1, 2", "two columns cannot be one scalar"),
            (
                "SELECT 1, 2 WHERE false",
                "an empty result set hid the arity error the populated one reports",
            ),
        ] {
            let cause = common::error_chain(
                &client
                    .query_opt_scalar::<i32, _>(sql, &[])
                    .await
                    .expect_err(why),
            );
            assert!(
                cause.contains("unexpected number of columns"),
                "{sql}: {why}, but the refusal said {cause:?}"
            );
            assert!(
                !cause.contains("unexpected number of rows"),
                "{sql}: an arity failure was reported as a row-count failure: {cause:?}"
            );
        }

        // One-variable partners: the shapes this API exists to serve.
        assert_eq!(
            client
                .query_opt_scalar::<i32, _>("SELECT 7", &[])
                .await
                .expect("a single-column single-row query is what this is for"),
            Some(7)
        );
        assert_eq!(
            client
                .query_opt_scalar::<i32, _>("SELECT 7 WHERE false", &[])
                .await
                .expect("no row is None, not an error"),
            None
        );
        // And the row-count rule it inherits from query_opt is still enforced -
        // as a ROW-count rule. The column here is single, so a helper reporting
        // an arity failure would be wrong about which rule it applied, and the
        // denial below is what separates this case from the two above.
        let cause = common::error_chain(
            &client
                .query_opt_scalar::<i32, _>("SELECT g FROM generate_series(1, 2) g", &[])
                .await
                .expect_err("query_opt_scalar must still refuse more than one row"),
        );
        assert!(
            cause.contains("unexpected number of rows"),
            "two rows of one column must fail on the row count, not on {cause:?}"
        );
        assert!(
            !cause.contains("unexpected number of columns"),
            "a row-count failure was reported as an arity failure: {cause:?}"
        );
    })
    .await
    .expect("query_opt_scalar arity test exceeded its watchdog");
}

/// What `Row::raw_size_bytes` counts, stated in numbers rather than prose.
///
/// It had no caller anywhere in this repository and no test, and its doc said
/// only "the raw size of the row in bytes". That reading is wrong in the
/// direction that matters: it is the length-prefixed FIELD DATA, and the
/// `DataRow` tag, message length and field count -- 7 bytes per row -- are not
/// in it. A caller metering ingress from it undercounts every row, and small
/// rows by more than half.
///
/// Each case below is a different composition of the same formula, so a change
/// that broke one part of it cannot hide: a payload, a longer payload, a NULL
/// whose length field is counted with no payload, and two columns summed.
#[compio::test]
async fn raw_size_bytes_counts_field_data_and_not_the_frame() {
    compio::time::timeout(TEST_WATCHDOG, async {
        let client = connect().await;

        for (sql, expected, why) in [
            ("SELECT 'x'::text", 5usize, "4-byte length + 1-byte payload"),
            ("SELECT 'xyz'::text", 7, "4 + 3"),
            (
                "SELECT NULL::text",
                4,
                "the length field alone; NULL has no payload",
            ),
            (
                "SELECT 'x'::text, 'yz'::text",
                11,
                "(4 + 1) + (4 + 2), summed",
            ),
        ] {
            let row = client.query_one(sql, &[]).await.expect("query the fixture");
            assert_eq!(row.raw_size_bytes(), expected, "{sql}: {why}");
        }

        // WHAT THIS TEST DOES NOT PIN: the 7 bytes of frame overhead
        // (`DataRow` tag 1 + length 4 + field count 2) that `raw_size_bytes`
        // excludes. A block here used to claim it did, asserting
        // `row.raw_size_bytes() + 7 == 12` for `SELECT 'x'::text` and calling
        // that "a relationship rather than a second literal". It is not one:
        // the assertion reduces to `raw_size_bytes() == 5`, which the table's
        // first case already checks on the identical query, and both 7 and 12
        // were hand-written. Nothing measured a frame, so a change to the
        // overhead could not have moved it. Pinning it needs a scripted peer
        // whose bytes are known -- `tests/hostile_peer.rs` has that machinery
        // and this file does not -- so the honest statement is that the four
        // cases above pin the FIELD accounting and nothing here pins the
        // frame's.
    })
    .await
    .expect("raw_size_bytes test exceeded its watchdog");
}

/// A short value list is refused before it can corrupt the binary stream.
///
/// `write_raw` documents this panic, and nothing tested it -- the crate has no
/// `should_panic` test at all. What the assert prevents is not a tidy error but
/// a SILENT wire corruption: the field count is written from `types.len()`
/// BEFORE the values are encoded, and the encode loop `zip`s values against
/// types, so it stops at the shorter one. Remove the assert and a short list
/// emits a tuple claiming N fields while carrying fewer, and PostgreSQL parses
/// the next tuple's bytes as the remainder of this one.
///
/// A panic is the right answer here, unlike the `DataRow` arity case in
/// `src/row.rs`: that input comes from the PEER and must never panic, this one
/// is the caller's own argument list, so it is a programmer error in the sense
/// slice indexing is. tokio-postgres asserts here too.
#[compio::test]
#[should_panic(expected = "expected 2 values but got 1")]
async fn binary_copy_write_refuses_a_short_value_list() {
    use compio_postgres::binary_copy::BinaryCopyInWriter;
    use compio_postgres::types::Type;

    let client = connect().await;
    client
        .batch_execute("CREATE TEMP TABLE query_claims_binary_arity (a int4, b text)")
        .await
        .expect("create the binary COPY fixture");
    let sink = client
        .copy_in("COPY query_claims_binary_arity FROM STDIN BINARY")
        .await
        .expect("enter binary COPY");
    let mut writer = Box::pin(BinaryCopyInWriter::new(sink, &[Type::INT4, Type::TEXT]));

    // Two columns were declared; one value is offered.
    writer
        .as_mut()
        .write(&[&1_i32])
        .await
        .expect("unreachable: the arity assert fires first");
}

/// `RowStream::columns` is available BEFORE any row is consumed.
///
/// That is the property `Client::query_scalar` depends on: it reads the arity
/// from the STATEMENT rather than from `rows.first()`, so a two-column query
/// returning no rows is still refused. If `columns()` ever came to depend on
/// having polled a row, the scalar helpers would silently return to deciding
/// by the data -- and their own tests would not catch it, because those use
/// queries that DO return rows.
///
/// So this asserts the empty case explicitly, and pins that the answer is the
/// same before and after the stream is drained.
#[compio::test]
async fn row_stream_columns_are_known_before_any_row_is_read() {
    compio::time::timeout(TEST_WATCHDOG, async {
        let client = connect().await;

        // A query that yields NOTHING. Nothing can be learned from its rows.
        let stream = client
            .query_raw(
                "SELECT 1::int4 AS a, 'x'::text AS b WHERE false",
                std::iter::empty::<i32>(),
            )
            .await
            .expect("start an empty row stream");
        let mut stream = std::pin::pin!(stream);

        let before: Vec<String> = stream
            .columns()
            .iter()
            .map(|column| format!("{}:{}", column.name(), column.type_()))
            .collect();
        assert_eq!(
            before,
            vec!["a:int4".to_string(), "b:text".to_string()],
            "the RowDescription was not available before the first poll"
        );

        // Drain it -- there is nothing to drain -- and the answer must not move.
        while stream
            .try_next()
            .await
            .expect("empty stream drains")
            .is_some()
        {}
        let after: Vec<String> = stream
            .columns()
            .iter()
            .map(|column| format!("{}:{}", column.name(), column.type_()))
            .collect();
        assert_eq!(
            before, after,
            "the column list changed as the stream drained"
        );

        // ONE-VARIABLE PARTNER: a query that does return rows must report the
        // same shape, or "columns are known early" could be satisfied by
        // reporting something constant.
        let populated = client
            .query_raw(
                "SELECT 1::int4 AS a, 'x'::text AS b",
                std::iter::empty::<i32>(),
            )
            .await
            .expect("start a populated row stream");
        let populated = std::pin::pin!(populated);
        let names: Vec<String> = populated
            .columns()
            .iter()
            .map(|column| format!("{}:{}", column.name(), column.type_()))
            .collect();
        assert_eq!(names, before);
    })
    .await
    .expect("row-stream columns test exceeded its watchdog");
}
