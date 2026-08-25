//! `Row::raw_value` must distinguish a column that is not there from a column
//! that is there and NULL.
//!
//! These are different facts and only one of them is the caller's mistake. The
//! accessor answered `None` to both, so a caller reading by NAME - which is
//! every caller that did not just enumerate `row.columns()` - saw a renamed or
//! mistyped column as a legitimate SQL NULL. Measured against PostgreSQL 16.14
//! before the fix:
//!
//! ```text
//! raw_value("details")  [column exists, value is SQL NULL] = None
//! raw_value("nope")     [no such column]                   = None
//! ```
//!
//! Live rather than synthetic: `row_for_test` builds the same `Row`, but the
//! claim is about what a real server's `RowDescription` plus `DataRow` produce,
//! and a fixture cannot be wrong about a wire format in the same direction the
//! driver is.

use compio_postgres::{Client, Error};
use std::time::Duration;

#[allow(dead_code)]
mod common;

const TEST_TIMEOUT: Duration = Duration::from_secs(20);

fn test_url() -> String {
    common::test_url()
}

async fn connect(url: &str) -> Result<Client, Error> {
    let (client, connection) = compio_postgres::connect(url, common::suite_tls()).await?;
    compio::runtime::spawn(async move {
        if let Err(error) = connection.run().await {
            eprintln!("connection error: {}", common::error_chain(&error));
        }
    })
    .detach();
    Ok(client)
}

async fn connected() -> Client {
    let url = test_url();
    match compio::time::timeout(TEST_TIMEOUT, connect(&url)).await {
        Ok(Ok(client)) => client,
        Ok(Err(error)) => common::postgres_unreachable(&url, &error),
        Err(_) => panic!("connecting to {} timed out", common::redact_dsn(&url)),
    }
}

/// The claim: a name the row does not carry is an error, and a SQL NULL is not.
#[compio::test]
async fn a_missing_column_is_not_a_sql_null() {
    let client = connected().await;
    let row = client
        .query_one("SELECT NULL::jsonb AS details, 7::int4 AS other", &[])
        .await
        .expect("the probe query runs");

    assert_eq!(
        row.raw_value("details")
            .expect("`details` is a column of this row"),
        None,
        "a column that IS there and holds SQL NULL must read as Ok(None)"
    );

    let missing = row
        .raw_value("detials")
        .expect_err("a column this row does not carry must not read as a value");
    assert!(
        missing.to_string().contains("detials"),
        "the refusal must name the column that was not found, got: {missing}"
    );
}

/// The control, differing in ONE variable: the column is present and NOT null.
///
/// "Missing columns error" could otherwise be satisfied by erroring on
/// everything, or by an accessor that stopped returning bytes at all. This
/// pins the value path - by name AND by index, since only the name path does
/// the lookup that can fail.
#[compio::test]
async fn a_present_non_null_column_still_reads_its_bytes() {
    let client = connected().await;
    let row = client
        .query_one("SELECT NULL::jsonb AS details, 7::int4 AS other", &[])
        .await
        .expect("the probe query runs");

    assert_eq!(
        row.raw_value("other")
            .expect("`other` is a column of this row"),
        Some(&7i32.to_be_bytes()[..]),
        "a present non-null int4 must read back as its 4 big-endian bytes"
    );
    assert_eq!(
        row.raw_value(1).expect("index 1 is in range"),
        Some(&7i32.to_be_bytes()[..]),
        "the index path must agree with the name path"
    );
    assert_eq!(
        row.raw_value(0).expect("index 0 is in range"),
        None,
        "the index path must report the SQL NULL as Ok(None), not as an error"
    );
    assert!(
        row.raw_value(2).is_err(),
        "an out-of-range index is as absent as an unknown name"
    );
}
