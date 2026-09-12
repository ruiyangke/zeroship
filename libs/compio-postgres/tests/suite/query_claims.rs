//! Runtime claims made by the query and COPY APIs.

use bytes::Bytes;
use compio_postgres::error::SqlState;
use compio_postgres::{Client, Pool};
use futures_util::{SinkExt, TryStreamExt};
use std::time::Duration;

#[allow(unused_imports)]
use crate::common;

const TEST_WATCHDOG: Duration = Duration::from_secs(10);

fn test_url() -> String {
    common::test_url()
}

async fn connect() -> Client {
    let url = test_url();
    let (client, connection) = compio_postgres::connect(&url, common::suite_tls())
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
        let table = common::test_object_name("query_claims_copy_close");
        client
            .batch_execute(&format!("CREATE TEMP TABLE {table} (n int4 NOT NULL)"))
            .await
            .expect("create COPY close fixture");

        let sink = client
            .copy_in::<_, Bytes>(&format!("COPY {table} (n) FROM STDIN"))
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
                &format!("SELECT pg_backend_pid(), count(*)::int8, sum(n)::int8 FROM {table}"),
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
        let table = common::test_object_name("query_claims_text_query");
        client
            .batch_execute(&format!(
                "CREATE TEMP TABLE {table} (\
                     n int4 NOT NULL, enabled bool NOT NULL\
                 )"
            ))
            .await
            .expect("create query_text_params fixture");

        let rows = client
            .query_text_params(
                &format!("INSERT INTO {table} (n, enabled) VALUES ($1, $2) RETURNING n, enabled"),
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
async fn pool_query_text_params_casts_text_values_to_column_types() {
    compio::time::timeout(
        TEST_WATCHDOG,
        Box::pin(async {
            let url = test_url();
            let pool = Pool::connect(&url, 1)
                .await
                .expect("connect one-slot text-parameter pool");
            let table = common::test_object_name("query_claims_pool_text_cast");
            Box::pin(pool.batch_execute(&format!(
                "CREATE TEMP TABLE {table} (n int4 NOT NULL, enabled bool NOT NULL)"
            )))
            .await
            .expect("create pool query_text_params fixture");

            let rows = Box::pin(pool.query_text_params(
                &format!("INSERT INTO {table} (n, enabled) VALUES ($1, $2) RETURNING n, enabled"),
                &["42", "true"],
            ))
            .await
            .expect("pool text parameters did not cast to their target columns");
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].get::<_, i32>("n"), 42);
            assert!(rows[0].get::<_, bool>("enabled"));
        }),
    )
    .await
    .expect("pool query_text_params cast claim exceeded its watchdog");
}

#[compio::test]
async fn pool_query_text_params_returns_its_lease() {
    compio::time::timeout(
        TEST_WATCHDOG,
        Box::pin(async {
            let url = test_url();
            let pool = Pool::connect(&url, 1)
                .await
                .expect("connect one-slot text-parameter pool");

            let rows =
                Box::pin(pool.query_text_params(
                    "SELECT pg_backend_pid() AS pid, $1::int4 AS value",
                    &["42"],
                ))
                .await
                .expect("query through the pool text-parameter wrapper");
            assert_eq!(rows.len(), 1);
            let process_id = rows[0].get::<_, i32>("pid");
            assert_eq!(rows[0].get::<_, i32>("value"), 42);
            assert_eq!(pool.active_count(), 0);
            assert_eq!(pool.idle_count(), 1);
            assert_eq!(pool.total_count(), 1);

            let client = Box::pin(pool.acquire())
                .await
                .expect("the text-parameter wrapper did not return its lease");
            assert_eq!(
                client.process_id(),
                process_id,
                "the one-slot pool replaced rather than returned the queried session"
            );
        }),
    )
    .await
    .expect("pool query_text_params lease claim exceeded its watchdog");
}

#[compio::test]
async fn transaction_query_text_params_scopes_set_local_to_every_exit() {
    compio::time::timeout(
        TEST_WATCHDOG,
        Box::pin(async {
            const LOCAL_TIMEOUT: &str = "43210ms";

            let mut client = connect().await;
            let baseline: String = client
                .query_one_scalar("SELECT current_setting('statement_timeout')", &[])
                .await
                .expect("read the session's statement_timeout baseline");
            assert_ne!(
                baseline, LOCAL_TIMEOUT,
                "the test needs a local timeout distinct from the session baseline"
            );

            for exit in ["commit", "rollback", "drop"] {
                let transaction = client.transaction().await.expect("begin transaction");
                transaction
                    .batch_execute(&format!("SET LOCAL statement_timeout = '{LOCAL_TIMEOUT}'"))
                    .await
                    .expect("set the transaction-local timeout");

                let rows = transaction
                    .query_text_params(
                        "SELECT $1::int4 AS value, \
                            current_setting('statement_timeout') AS local_timeout",
                        &["42"],
                    )
                    .await
                    .expect("query text parameters inside the transaction");
                assert_eq!(rows.len(), 1);
                assert_eq!(rows[0].get::<_, i32>("value"), 42);
                assert_eq!(rows[0].get::<_, String>("local_timeout"), LOCAL_TIMEOUT);

                match exit {
                    "commit" => transaction.commit().await.expect("commit transaction"),
                    "rollback" => transaction.rollback().await.expect("rollback transaction"),
                    "drop" => drop(transaction),
                    _ => unreachable!(),
                }

                let restored: String = client
                    .query_one_scalar("SELECT current_setting('statement_timeout')", &[])
                    .await
                    .unwrap_or_else(|error| panic!("query after {exit} failed: {error}"));
                assert_eq!(
                    restored, baseline,
                    "SET LOCAL statement_timeout survived transaction {exit}"
                );
            }
        }),
    )
    .await
    .expect("transaction query_text_params scope claim exceeded its watchdog");
}

#[compio::test]
async fn execute_text_params_coerces_nulls_and_returns_the_affected_count() {
    compio::time::timeout(TEST_WATCHDOG, async {
        let client = connect().await;
        let table = common::test_object_name("query_claims_text_execute");
        client
            .batch_execute(&format!(
                "CREATE TEMP TABLE {table} (\
                     n int4 NOT NULL, optional_n int4\
                 )"
            ))
            .await
            .expect("create execute_text_params fixture");

        let affected = client
            .execute_text_params(
                &format!("INSERT INTO {table} (n, optional_n) VALUES ($1, $2)"),
                &[Some("42".to_string()), None],
            )
            .await
            .expect("execute text parameters");
        assert_eq!(affected, 1);

        let row = client
            .query_one(&format!("SELECT n, optional_n FROM {table}"), &[])
            .await
            .expect("read execute_text_params result");
        assert_eq!(row.get::<_, i32>("n"), 42);
        assert_eq!(row.get::<_, Option<i32>>("optional_n"), None);
    })
    .await
    .expect("execute_text_params claim exceeded its watchdog");
}

/// Bind's parameter count is a wire `u16`. The text adapter must surface that
/// local serialization refusal as an encode error and leave its scratch buffer
/// out of the next request.
#[compio::test]
async fn execute_text_params_reports_bind_count_overflow_without_poisoning_the_client() {
    Box::pin(compio::time::timeout(TEST_WATCHDOG, async {
        let client = connect().await;
        let process_id = client.process_id();
        let params = vec![None::<String>; usize::from(u16::MAX) + 1];

        let error = client
            .execute_text_params("SELECT 1", &params)
            .await
            .expect_err("more than u16::MAX parameters were encoded");
        assert_eq!(
            error.to_string(),
            "error encoding message to server",
            "bind count overflow was classified as a server response: {}",
            common::error_chain(&error)
        );

        let observed_process_id: i32 = client
            .query_one_scalar("SELECT pg_backend_pid()", &[])
            .await
            .expect("bind overflow poisoned the next request");
        assert_eq!(observed_process_id, process_id);
    }))
    .await
    .expect("execute_text_params bind-overflow claim exceeded its watchdog");
}

/// A server-side cast refusal arrives through the response stream, not the
/// frontend encoder. Preserve its SQLSTATE and drain through `ReadyForQuery` so
/// the same physical session remains usable.
#[compio::test]
async fn execute_text_params_preserves_server_cast_errors() {
    Box::pin(compio::time::timeout(TEST_WATCHDOG, async {
        let client = connect().await;
        let process_id = client.process_id();

        let error = client
            .execute_text_params("SELECT $1::int4", &[Some("not-an-int4".to_string())])
            .await
            .expect_err("PostgreSQL accepted invalid int4 text");
        assert_eq!(
            error.code(),
            Some(&SqlState::INVALID_TEXT_REPRESENTATION),
            "execute_text_params replaced the server's cast error: {}",
            common::error_chain(&error)
        );

        let observed_process_id: i32 = client
            .query_one_scalar("SELECT pg_backend_pid()", &[])
            .await
            .expect("server cast error poisoned the next request");
        assert_eq!(observed_process_id, process_id);
    }))
    .await
    .expect("execute_text_params cast-error claim exceeded its watchdog");
}

/// Dropping the unrun `Connection` closes the request receiver before this API
/// can enqueue. That local closure must remain distinguishable from a protocol
/// response error.
#[compio::test]
async fn execute_text_params_reports_a_closed_connection_before_enqueue() {
    Box::pin(compio::time::timeout(TEST_WATCHDOG, async {
        let url = test_url();
        let (client, connection) = compio_postgres::connect(&url, common::suite_tls())
            .await
            .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));
        drop(connection);

        let error = client
            .execute_text_params("SELECT 1", &[])
            .await
            .expect_err("execute_text_params enqueued through a dropped Connection");
        assert!(
            error.is_closed(),
            "a closed request channel was reported as {}",
            common::error_chain(&error)
        );
    }))
    .await
    .expect("execute_text_params closed-connection claim exceeded its watchdog");
}

#[compio::test]
async fn row_stream_reports_affected_rows_only_after_exhaustion() {
    compio::time::timeout(TEST_WATCHDOG, async {
        let client = connect().await;
        let table = common::test_object_name("query_claims_rows_affected");
        client
            .batch_execute(&format!(
                "CREATE TEMP TABLE {table} (n int4 NOT NULL); \
                 INSERT INTO {table} VALUES (1), (2), (3)"
            ))
            .await
            .expect("create rows_affected fixture");

        let stream = client
            .query_raw(
                &format!("UPDATE {table} SET n = n + 10 RETURNING n"),
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
        // whose bytes are known -- `libs/compio-postgres/tests/suite/hostile_peer.rs` has that machinery
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
    let table = common::test_object_name("query_claims_binary_arity");
    client
        .batch_execute(&format!("CREATE TEMP TABLE {table} (a int4, b text)"))
        .await
        .expect("create the binary COPY fixture");
    let sink = client
        .copy_in(&format!("COPY {table} FROM STDIN BINARY"))
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

/// A row `write_raw` REFUSED must leave nothing of itself in the writer.
///
/// The arity case above is the caller's mistake and panics. This is the other
/// half: the arity is right and one VALUE is refused, which returns `Err`. By
/// then `write_raw` has already put the tuple's field count and that value's
/// four-byte length placeholder into the writer's shared buffer, and nothing
/// rolls either back. The next row the caller writes is appended behind that
/// stub, and `finish` hands the whole buffer to PostgreSQL.
///
/// The damage is SILENT, not a failed COPY. The stub parses: one field of
/// length zero is a legal empty `text`, so PostgreSQL inserts a row the caller
/// was told it had failed to write, `finish` reports one row more than the
/// caller wrote, and no error is raised anywhere. A caller that logs the
/// per-row error and carries on -- the obvious thing to do with a
/// per-row-fallible API -- gets one junk row per refusal.
///
/// A wrong type is used because it is the cheapest way to fail
/// `to_sql_checked`: it rejects on `accepts` BEFORE calling `to_sql`, so not
/// one byte of value data is written and the leftover is purely `write_raw`'s
/// own bookkeeping. `a_binary_copy_row_refused_after_writing_bytes_leaves_no_
/// garbage` below covers the case where `to_sql` itself writes and then fails.
#[compio::test]
async fn a_refused_binary_copy_row_leaves_nothing_behind() {
    use compio_postgres::binary_copy::BinaryCopyInWriter;
    use compio_postgres::types::Type;

    compio::time::timeout(TEST_WATCHDOG, async {
        let client = connect().await;
        let table = common::test_object_name("query_claims_binary_rollback");
        client
            .batch_execute(&format!("CREATE TEMP TABLE {table} (v text)"))
            .await
            .expect("create the binary COPY fixture");

        let sink = client
            .copy_in(&format!("COPY {table} FROM STDIN BINARY"))
            .await
            .expect("enter binary COPY");
        let mut writer = Box::pin(BinaryCopyInWriter::new(sink, &[Type::TEXT]));

        writer
            .as_mut()
            .write(&[&"alpha"])
            .await
            .expect("a text value into a text column");

        // Right arity, wrong type: the assert does not fire, `to_sql_checked`
        // refuses on `accepts`, and `write_raw` returns.
        writer
            .as_mut()
            .write(&[&1_i32])
            .await
            .expect_err("an i32 is not a text and must be refused");

        writer
            .as_mut()
            .write(&[&"gamma"])
            .await
            .expect("the writer must still take good rows after a refused one");

        let written = writer
            .as_mut()
            .finish()
            .await
            .expect("finish the binary COPY");

        let rows: Vec<String> = client
            .query(&format!("SELECT v FROM {table} ORDER BY v"), &[])
            .await
            .expect("read back the copied rows")
            .iter()
            .map(|row| row.get::<_, String>(0))
            .collect();

        assert_eq!(
            rows,
            vec!["alpha".to_string(), "gamma".to_string()],
            "a refused row still reached the table: only the two rows that were \
             ACCEPTED may land"
        );
        assert_eq!(
            written, 2,
            "finish counted rows the caller was told had failed"
        );
    })
    .await
    .expect("refused-binary-row test exceeded its watchdog");
}

/// The one-variable control for the test above: same table, same three writes,
/// same `finish`, with the middle value CORRECT.
///
/// It pins that the rollback removes the refused row and nothing else. A fix
/// that cleared the buffer, or that poisoned the writer on the first error,
/// would satisfy the test above and fail this one; so would a fix that dropped
/// the row before the failure or the row after it.
#[compio::test]
async fn an_accepted_binary_copy_row_between_two_others_is_kept() {
    use compio_postgres::binary_copy::BinaryCopyInWriter;
    use compio_postgres::types::Type;

    compio::time::timeout(TEST_WATCHDOG, async {
        let client = connect().await;
        let table = common::test_object_name("query_claims_binary_control");
        client
            .batch_execute(&format!("CREATE TEMP TABLE {table} (v text)"))
            .await
            .expect("create the binary COPY control fixture");

        let sink = client
            .copy_in(&format!("COPY {table} FROM STDIN BINARY"))
            .await
            .expect("enter binary COPY");
        let mut writer = Box::pin(BinaryCopyInWriter::new(sink, &[Type::TEXT]));

        writer.as_mut().write(&[&"alpha"]).await.expect("first row");
        // THE ONE VARIABLE: a text where the test above passes an i32.
        writer.as_mut().write(&[&"beta"]).await.expect("second row");
        writer.as_mut().write(&[&"gamma"]).await.expect("third row");

        let written = writer
            .as_mut()
            .finish()
            .await
            .expect("finish the binary COPY");

        let rows: Vec<String> = client
            .query(&format!("SELECT v FROM {table} ORDER BY v"), &[])
            .await
            .expect("read back the copied rows")
            .iter()
            .map(|row| row.get::<_, String>(0))
            .collect();

        assert_eq!(
            rows,
            vec!["alpha".to_string(), "beta".to_string(), "gamma".to_string()],
            "an accepted row was dropped"
        );
        assert_eq!(written, 3, "finish undercounted accepted rows");
    })
    .await
    .expect("accepted-binary-row control exceeded its watchdog");
}

/// The same defect where `to_sql` writes bytes BEFORE it fails, which is the
/// shape that puts a genuinely partial row on the wire.
///
/// `to_sql` is handed the writer's buffer directly and may append to it any
/// number of times before deciding it cannot encode the value -- a length
/// ceiling reached mid-encode, a nested composite whose last member is out of
/// range. Whatever it wrote stays, ahead of a length placeholder still reading
/// zero, and the tuple that follows starts inside those bytes.
///
/// Here the failure is LOUD rather than silent, and that is not better: the
/// stub's stray bytes are read as the next tuple's field count, PostgreSQL
/// rejects the stream, and the rows the caller successfully wrote are lost
/// along with the one it was told had failed.
#[compio::test]
async fn a_binary_copy_row_refused_after_writing_bytes_leaves_no_garbage() {
    use bytes::BufMut;
    use compio_postgres::binary_copy::BinaryCopyInWriter;
    use compio_postgres::types::{IsNull, ToSql, Type, to_sql_checked};

    /// Appends to the shared buffer and only then refuses.
    #[derive(Debug)]
    struct RefusedMidEncode;

    impl ToSql for RefusedMidEncode {
        fn to_sql(
            &self,
            _: &Type,
            out: &mut bytes::BytesMut,
        ) -> Result<IsNull, Box<dyn std::error::Error + Sync + Send>> {
            // Two bytes, because that is exactly the width of the field count
            // the next tuple is supposed to begin with.
            out.put_slice(b"\x7f\x7f");
            Err("refused after writing".into())
        }

        fn accepts(ty: &Type) -> bool {
            *ty == Type::TEXT
        }

        to_sql_checked!();
    }

    compio::time::timeout(TEST_WATCHDOG, async {
        let client = connect().await;
        let table = common::test_object_name("query_claims_binary_partial");
        client
            .batch_execute(&format!("CREATE TEMP TABLE {table} (v text)"))
            .await
            .expect("create the partial-encode fixture");

        let sink = client
            .copy_in(&format!("COPY {table} FROM STDIN BINARY"))
            .await
            .expect("enter binary COPY");
        let mut writer = Box::pin(BinaryCopyInWriter::new(sink, &[Type::TEXT]));

        writer
            .as_mut()
            .write(&[&"alpha"])
            .await
            .expect("a text value into a text column");
        writer
            .as_mut()
            .write(&[&RefusedMidEncode])
            .await
            .expect_err("a value whose to_sql fails must be refused");
        writer
            .as_mut()
            .write(&[&"gamma"])
            .await
            .expect("the writer must still take good rows after a refused one");

        let written = writer
            .as_mut()
            .finish()
            .await
            .expect("a COPY carrying only accepted rows must be accepted");

        let rows: Vec<String> = client
            .query(&format!("SELECT v FROM {table} ORDER BY v"), &[])
            .await
            .expect("read back the copied rows")
            .iter()
            .map(|row| row.get::<_, String>(0))
            .collect();

        assert_eq!(
            rows,
            vec!["alpha".to_string(), "gamma".to_string()],
            "bytes from a refused row reached the wire"
        );
        assert_eq!(written, 2, "finish counted a row that was never encoded");
    })
    .await
    .expect("partial-encode binary row test exceeded its watchdog");
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

/// `query_one_scalar` rules on arity from the STATEMENT, so its diagnostic does
/// not depend on whether the table happened to have rows.
///
/// Both queries below select two columns, which is wrong either way. Before this
/// change the zero-row case reported a ROW-count error, blaming the data for a
/// mistake in the query text; only the one-row case named the columns. Nothing
/// was wrongly accepted - `query_one` guarantees a row exists, so the check
/// always ran - but the two answers disagreed about what was wrong.
#[compio::test]
async fn query_one_scalar_names_the_columns_regardless_of_row_count() {
    let client = connect().await;

    for sql in [
        "SELECT 1::int4, 2::int4 WHERE false",
        "SELECT 1::int4, 2::int4",
    ] {
        let error = client
            .query_one_scalar::<i32, _>(sql, &[])
            .await
            .expect_err("a two-column query is not a scalar");
        assert!(
            format!("{error}").contains("columns"),
            "`{sql}` should name the column count, got: {error}"
        );
    }
}

/// THE CONTROL, one variable: a genuine one-column query with no row must still
/// report a ROW-count problem, not a column one. Reporting "columns" for
/// everything would satisfy the test above.
#[compio::test]
async fn query_one_scalar_still_reports_a_missing_row_as_a_row_problem() {
    let client = connect().await;

    let error = client
        .query_one_scalar::<i32, _>("SELECT 1::int4 WHERE false", &[])
        .await
        .expect_err("no row is still an error");
    assert!(
        format!("{error}").contains("rows"),
        "a one-column query with no row is a row problem, got: {error}"
    );
}

/// The OTHER half of `query_one_scalar`'s row contract, and the sibling
/// asymmetry that hid it: `query_opt_scalar` carries a two-row assertion, while
/// `query_one_scalar` had only the columns test and the no-row one above.
///
/// The no-row case does not hold the `rows.len() != 1` guard. With that guard
/// disabled an empty result still fails, because `.next().ok_or_else(row_count)`
/// one line later raises the same error - so both tests above stay green
/// without it. Only MORE than one row separates the two paths, and there a
/// missing guard is silent: `query_one_scalar` would return the FIRST of
/// several rows and call it the answer.
///
/// Measured 2026-09-03: disabling the guard left the lib (771) and suite (798)
/// suites entirely green.
#[compio::test]
async fn query_one_scalar_refuses_more_than_one_row() {
    let client = connect().await;

    let cause = common::error_chain(
        &client
            .query_one_scalar::<i32, _>("SELECT g FROM generate_series(1, 2) g", &[])
            .await
            .expect_err("query_one_scalar must refuse more than one row"),
    );
    assert!(
        cause.contains("unexpected number of rows"),
        "two rows of one column must fail on the row count, not on {cause:?}"
    );
    assert!(
        !cause.contains("unexpected number of columns"),
        "a row-count failure was reported as an arity failure: {cause:?}"
    );

    // Control, one variable: the same query shape with a single row is exactly
    // what the helper is for, so the refusal turns on the row count alone.
    assert_eq!(
        client
            .query_one_scalar::<i32, _>("SELECT g FROM generate_series(1, 1) g", &[])
            .await
            .expect("a single-column single-row query is what this is for"),
        1
    );
}
