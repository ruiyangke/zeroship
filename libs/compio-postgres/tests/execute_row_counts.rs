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

use compio_postgres::{Client, NoTls};

#[allow(dead_code)]
mod common;

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

/// Every row-affecting command reports its real count, and DDL reports zero.
#[compio::test]
async fn execute_reports_the_count_from_each_command_tag_shape() {
    let Some(url) = test_url() else {
        eprintln!("PG_TEST_URL unset; skipping");
        return;
    };
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
    let Some(url) = test_url() else {
        eprintln!("PG_TEST_URL unset; skipping");
        return;
    };
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
