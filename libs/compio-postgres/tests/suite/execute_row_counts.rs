//! `execute` must report the row count PostgreSQL actually sent.
//!
//! The count is parsed out of the `CommandComplete` tag by taking its last
//! space-separated token (`query.rs::extract_row_affected`). That is correct
//! but not obviously so, and every way of getting it wrong is SILENT -- the
//! caller receives a plausible number rather than an error:
//!
//! - `INSERT 0 5` carries an OID in the MIDDLE and the count LAST, so an
//!   implementation reading the wrong token returns 0 for every insert.
//! - `CREATE TABLE` has no count at all; its last token is `TABLE`, which does
//!   not parse, and 0 is the right answer. An implementation that treated an
//!   unparseable tail as an error instead would fail ordinary DDL.
//!
//! The existing assertions in `integration.rs` all check a count of 1 or 0.
//! Those cannot tell a correct count from an off-by-one, from a constant 1, or
//! from a count that happens to match the number of PARAMETERS. Everything
//! below uses counts greater than one, and the DDL case is the one-variable
//! partner that keeps "returns the last number it can find" from passing.

use compio_postgres::Client;
use compio_postgres::types::{ToSql, Type};
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

/// Every row-affecting command reports its real count, and DDL reports zero.
#[compio::test]
async fn execute_reports_the_count_from_each_command_tag_shape() {
    let url = test_url();
    let client = connect_client(&url).await;
    let table = common::test_object_name("cpg_rowcount");

    // DDL: the tag is `CREATE TABLE`, whose last token does not parse. Zero is
    // the answer, and reaching it must not be an error.
    assert_eq!(
        client
            .execute(
                &format!("CREATE TEMPORARY TABLE {table} (id int primary key, v int)"),
                &[]
            )
            .await
            .expect("DDL must succeed"),
        0,
        "a CREATE TABLE tag carries no count, so it must report 0"
    );

    // INSERT: tag is `INSERT <oid> <count>`. Three rows, so a reader taking the
    // middle token would say 0 and a constant-1 reader would say 1.
    assert_eq!(
        client
            .execute(
                &format!("INSERT INTO {table} (id, v) VALUES (1, 10), (2, 20), (3, 30)"),
                &[]
            )
            .await
            .expect("insert three rows"),
        3,
        "INSERT reports the count, not the OID"
    );

    // UPDATE: tag is `UPDATE <count>` -- a different shape from INSERT, with
    // no OID field, so it exercises a second layout.
    assert_eq!(
        client
            .execute(&format!("UPDATE {table} SET v = v + 1 WHERE id <= 2"), &[])
            .await
            .expect("update two rows"),
        2,
        "UPDATE reports the number of rows it changed"
    );

    // An UPDATE matching nothing is 0 rows and NOT an error -- the arm most
    // likely to be conflated with the DDL zero above.
    assert_eq!(
        client
            .execute(&format!("UPDATE {table} SET v = 0 WHERE id = 999"), &[])
            .await
            .expect("an update matching nothing still succeeds"),
        0,
        "an UPDATE that matches no rows reports 0"
    );

    // DELETE: `DELETE <count>`.
    assert_eq!(
        client
            .execute(&format!("DELETE FROM {table} WHERE id <= 2"), &[])
            .await
            .expect("delete two rows"),
        2,
        "DELETE reports the number of rows it removed"
    );

    // SELECT through `execute`: tag is `SELECT <count>`, so the count is the
    // rows the query PRODUCED even though execute discards them.
    assert_eq!(
        client
            .execute("SELECT * FROM generate_series(1, 4)", &[])
            .await
            .expect("select four rows"),
        4,
        "SELECT through execute reports the rows it produced"
    );
}

/// The count tracks the DATA, not the number of parameters.
///
/// A single statement with two parameters affecting three rows separates the
/// two numbers, which the existing single-row assertions cannot.
#[compio::test]
async fn the_count_is_rows_affected_not_parameters_supplied() {
    let url = test_url();
    let client = connect_client(&url).await;
    let table = common::test_object_name("cpg_rowcount_params");

    client
        .batch_execute(&format!(
            "CREATE TEMPORARY TABLE {table} (id int primary key, v int)"
        ))
        .await
        .expect("create table");
    client
        .batch_execute(&format!(
            "INSERT INTO {table} (id, v) VALUES (1, 5), (2, 5), (3, 5), (4, 9)"
        ))
        .await
        .expect("seed rows");

    // Two parameters, three rows affected. 3 != 2 and 3 != 1.
    let affected = client
        .execute(
            &format!("UPDATE {table} SET v = $1 WHERE v = $2"),
            &[&7i32, &5i32],
        )
        .await
        .expect("update by value");
    assert_eq!(
        affected, 3,
        "the count must be rows affected (3), not parameters supplied (2)"
    );
}

/// The explicitly typed convenience path has its own client wrapper and must
/// preserve counts greater than one rather than collapsing every success to 1.
#[compio::test]
async fn execute_typed_reports_a_multirow_select_count() {
    compio::time::timeout(Duration::from_secs(10), async {
        let url = test_url();
        let client = connect_client(&url).await;
        let limit = 4i32;
        let params: [(&(dyn ToSql + Sync), Type); 1] = [(&limit, Type::INT4)];

        let affected = client
            .execute_typed(
                "SELECT g::int4 FROM generate_series(1, $1::int4) AS g",
                &params,
            )
            .await
            .expect("execute_typed must accept the explicit parameter type");
        assert_eq!(
            affected, 4,
            "execute_typed collapsed a four-row command tag to another count"
        );
    })
    .await
    .expect("typed execute row-count claim exceeded its 10 second deadline");
}

/// Text-format parameter execution takes a separate wrapper path. A count of
/// five separates the server's command tag from both constant 1 and the single
/// supplied parameter.
#[compio::test]
async fn execute_text_params_reports_a_multirow_select_count() {
    compio::time::timeout(Duration::from_secs(10), async {
        let url = test_url();
        let client = connect_client(&url).await;

        let affected = client
            .execute_text_params(
                "SELECT g::int4 FROM generate_series(1, $1::int4) AS g",
                &[Some("5".to_owned())],
            )
            .await
            .expect("execute_text_params must accept a text-format integer");
        assert_eq!(
            affected, 5,
            "execute_text_params collapsed a five-row command tag to another count"
        );
    })
    .await
    .expect("text-parameter execute row-count claim exceeded its 10 second deadline");
}

/// After exhaustion, `rows_affected` must not still read `None`.
///
/// `RowStream::rows_affected` documents itself as returning "`None` until the
/// stream has been exhausted", which tells a caller that once the stream ends
/// the value is available. An empty query breaks that: the server answers
/// `EmptyQueryResponse` and then `ReadyForQuery` with no `CommandComplete` in
/// between, nothing ever sets the field, and it stays `None` on a stream that
/// IS exhausted. `None` then means two different things and the caller cannot
/// tell them apart -- the same conflation `Row::raw_value` was fixed for.
///
/// `execute("")` already answers 0 for the same query, so 0 is the consistent
/// answer here too.
#[compio::test]
async fn rows_affected_is_available_once_an_empty_query_stream_is_exhausted() {
    use futures_util::TryStreamExt;

    let url = test_url();
    let client = connect_client(&url).await;

    let stream = client
        .query_raw("", std::iter::empty::<&i32>())
        .await
        .expect("an empty query is accepted");
    let mut stream = Box::pin(stream);
    while stream.as_mut().try_next().await.expect("drain").is_some() {}

    assert_eq!(
        stream.as_ref().get_ref().rows_affected(),
        Some(0),
        "an exhausted empty-query stream still reported None, so None means \
         both `not finished` and `no count was sent`"
    );

    // Control, one variable away: a stream that DID carry a CommandComplete
    // reports its real count, so the assertion above cannot be satisfied by
    // hardcoding Some(0).
    let stream = client
        .query_raw(
            "SELECT * FROM generate_series(1, 3)",
            std::iter::empty::<&i32>(),
        )
        .await
        .expect("non-empty query");
    let mut stream = Box::pin(stream);
    while stream.as_mut().try_next().await.expect("drain").is_some() {}
    assert_eq!(stream.as_ref().get_ref().rows_affected(), Some(3));
}
