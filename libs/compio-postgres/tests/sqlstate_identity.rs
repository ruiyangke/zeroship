//! The named `SqlState` constants must match the codes PostgreSQL really sends.
//!
//! `src/error/sqlstate.rs` is a GENERATED table -- 269 lines of constants, and
//! the lowest-covered file in the crate at around 5%. Coverage there is a
//! meaningless number (one line per constant, and no test needs all of them),
//! but a transcription error in it would be invisible in exactly the way that
//! matters: `SqlState::UNIQUE_VIOLATION` carrying the wrong five characters
//! still compiles, still compares equal to itself, and still reads correctly in
//! every test that uses the constant on both sides.
//!
//! So the oracle has to be the SERVER. Each case below provokes a real error
//! and asserts the constant this crate exposes equals the code PostgreSQL put
//! on the wire. A wrong constant fails here and nowhere else.
//!
//! This also covers the plumbing: `Error::as_db_error` has to actually find the
//! `DbError` in the source chain for any of it to be readable.

use compio_postgres::error::SqlState;
use compio_postgres::{Client, NoTls};

#[allow(dead_code)]
mod common;

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

/// Provoke `sql` and return the SQLSTATE the server attached.
async fn sqlstate_of(client: &Client, sql: &str) -> SqlState {
    let error = client
        .query(sql, &[])
        .await
        .err()
        .unwrap_or_else(|| panic!("expected {sql} to fail"));
    error
        .as_db_error()
        .unwrap_or_else(|| panic!("{sql} failed without a DbError: {error}"))
        .code()
        .clone()
}

/// Errors reachable with no schema at all.
#[compio::test]
async fn schema_free_errors_carry_the_constants_this_crate_names() {
    let url = test_url();
    let client = connect_client(&url).await;

    let cases: &[(&str, &SqlState)] = &[
        (
            "SELECT * FROM a_table_that_does_not_exist",
            &SqlState::UNDEFINED_TABLE,
        ),
        ("SELECT 1/0", &SqlState::DIVISION_BY_ZERO),
        ("SELECT 'abc'::int4", &SqlState::INVALID_TEXT_REPRESENTATION),
        (
            "SELECT a_function_that_does_not_exist()",
            &SqlState::UNDEFINED_FUNCTION,
        ),
        (
            "SELECT nosuchcolumn FROM (SELECT 1 AS x) AS t",
            &SqlState::UNDEFINED_COLUMN,
        ),
        (
            "SELECT 2147483647::int4 + 1::int4",
            &SqlState::NUMERIC_VALUE_OUT_OF_RANGE,
        ),
    ];

    for (sql, expected) in cases {
        let actual = sqlstate_of(&client, sql).await;
        assert_eq!(
            actual.code(),
            expected.code(),
            "{sql} produced SQLSTATE {} but this crate names {} for it",
            actual.code(),
            expected.code()
        );
    }
}

/// Constraint violations, which need a table and are the codes callers branch
/// on most often.
#[compio::test]
async fn constraint_violations_carry_the_constants_this_crate_names() {
    let url = test_url();
    let client = connect_client(&url).await;

    // Temporary, so the shared review database keeps no residue even if this
    // test fails part way through.
    client
        .batch_execute(
            "CREATE TEMPORARY TABLE sqlstate_probe (
                 id     int PRIMARY KEY,
                 parent int REFERENCES sqlstate_probe(id),
                 needed int NOT NULL,
                 small  int CHECK (small < 10)
             )",
        )
        .await
        .expect("create the probe table");

    client
        .batch_execute("INSERT INTO sqlstate_probe (id, needed, small) VALUES (1, 1, 1)")
        .await
        .expect("seed a row");

    let cases: &[(&str, &SqlState)] = &[
        (
            "INSERT INTO sqlstate_probe (id, needed, small) VALUES (1, 1, 1)",
            &SqlState::UNIQUE_VIOLATION,
        ),
        (
            "INSERT INTO sqlstate_probe (id, needed, small) VALUES (2, NULL, 1)",
            &SqlState::NOT_NULL_VIOLATION,
        ),
        (
            "INSERT INTO sqlstate_probe (id, needed, small) VALUES (3, 1, 99)",
            &SqlState::CHECK_VIOLATION,
        ),
        (
            "INSERT INTO sqlstate_probe (id, parent, needed, small) VALUES (4, 999, 1, 1)",
            &SqlState::FOREIGN_KEY_VIOLATION,
        ),
    ];

    for (sql, expected) in cases {
        let actual = sqlstate_of(&client, sql).await;
        assert_eq!(
            actual.code(),
            expected.code(),
            "{sql} produced SQLSTATE {} but this crate names {} for it",
            actual.code(),
            expected.code()
        );
    }
}

/// A code the generated table does not know is carried through verbatim.
///
/// `from_code` falls back to an `Other` variant rather than losing the value,
/// which is what keeps a driver usable against a server newer than its table.
/// `RAISE` lets the server pick the code, so this is the real path rather than
/// a constructed `SqlState`.
#[compio::test]
async fn an_unknown_sqlstate_is_carried_through_rather_than_discarded() {
    let url = test_url();
    let client = connect_client(&url).await;

    // "ZZ999" is in the user-defined range and is not a PostgreSQL code, so it
    // cannot be in the generated table.
    let error = client
        .batch_execute("DO $$ BEGIN RAISE EXCEPTION USING ERRCODE = 'ZZ999'; END $$")
        .await
        .expect_err("RAISE must fail the statement");
    let db_error = error
        .as_db_error()
        .unwrap_or_else(|| panic!("no DbError: {error}"));

    assert_eq!(
        db_error.code().code(),
        "ZZ999",
        "an unrecognised SQLSTATE must survive verbatim"
    );

    // Control, one variable away: a code the table DOES know still resolves to
    // its named constant through the same path.
    let error = client
        .batch_execute("DO $$ BEGIN RAISE EXCEPTION USING ERRCODE = '23505'; END $$")
        .await
        .expect_err("RAISE must fail the statement");
    assert_eq!(
        error.as_db_error().expect("a DbError").code(),
        &SqlState::UNIQUE_VIOLATION,
        "a known SQLSTATE raised the same way must resolve to its constant"
    );
}
