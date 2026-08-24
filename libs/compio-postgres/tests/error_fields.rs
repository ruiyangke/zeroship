//! The `ErrorResponse` fields PostgreSQL sends, as the caller sees them.
//!
//! `DbError` exposes the whole libpq set -- severity, detail, hint, position,
//! where, schema, table, column, datatype, constraint, file, line, routine --
//! and every accessor was present. Nothing checked that they carry the RIGHT
//! field. A mapping that read the wrong tag byte would surface a table name as
//! a constraint, or a column as a datatype, silently: the accessors are all
//! `Option<&str>`, so a swap type-checks and returns something plausible.
//!
//! THE DISCRIMINATION IS BETWEEN ERRORS, NOT WITHIN ONE. Asserting "the fields
//! are populated" would pass on a mapping that filled them all from one source.
//! PostgreSQL sends DIFFERENT subsets for different failures, so the test uses
//! that: a unique violation carries a constraint and NO column, a not-null
//! violation carries a column and NO constraint. Each one's absent field is the
//! other's present field, which is what a swapped mapping cannot satisfy.
//!
//! The expectations come from psql with `VERBOSITY verbose`, which prints the
//! same fields off the same wire message:
//!
//! ```text
//! ERROR:  23505: duplicate key value violates unique constraint "ef_pkey"
//! DETAIL:  Key (a)=(1) already exists.
//! SCHEMA NAME:  pg_temp_4
//! TABLE NAME:  ef
//! CONSTRAINT NAME:  ef_pkey
//! LOCATION:  _bt_check_unique, nbtinsert.c:666
//!
//! ERROR:  23502: null value in column "b" of relation "ef2" ...
//! SCHEMA NAME:  pg_temp_4
//! TABLE NAME:  ef2
//! COLUMN NAME:  b
//! ```

use compio_postgres::{Client, NoTls};
use std::time::Duration;

#[allow(dead_code)]
mod common;

const WATCHDOG: Duration = Duration::from_secs(30);

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
            eprintln!("connection error: {error}");
        }
    })
    .detach();
    client
}

#[compio::test]
async fn a_server_error_carries_the_fields_postgresql_sent() {
    compio::time::timeout(WATCHDOG, async {
        let client = connect().await;
        client
            .batch_execute("CREATE TEMP TABLE ef (a int PRIMARY KEY, b int NOT NULL CHECK (b > 0))")
            .await
            .expect("create the error fixture");
        client
            .execute("INSERT INTO ef VALUES (1, 1)", &[])
            .await
            .expect("seed the row the duplicate collides with");

        // ---- unique violation: constraint present, column absent ----
        let unique = client
            .execute("INSERT INTO ef VALUES (1, 1)", &[])
            .await
            .expect_err("a duplicate primary key must fail");
        let unique = unique
            .as_db_error()
            .expect("a server-sent error must survive as a DbError");

        assert_eq!(unique.code().code(), "23505", "unique_violation SQLSTATE");
        assert_eq!(unique.table(), Some("ef"));
        assert_eq!(unique.constraint(), Some("ef_pkey"));
        assert!(
            unique.schema().is_some_and(|s| s.starts_with("pg_temp")),
            "a temp table's schema is a pg_temp_N, got {:?}",
            unique.schema()
        );
        assert!(unique.detail().is_some(), "the server sent a DETAIL line");
        // Location, which only VERBOSITY verbose shows in psql.
        assert!(unique.routine().is_some(), "the server sent a routine");
        assert!(unique.file().is_some(), "the server sent a source file");
        assert!(unique.line().is_some(), "the server sent a source line");
        // THE DISCRIMINATOR: this error has no column, and a mapping that read
        // the constraint tag as a column would put "ef_pkey" here.
        assert_eq!(
            unique.column(),
            None,
            "a unique violation names no column; a swapped mapping would fill it"
        );

        // ---- not-null violation: column present, constraint absent ----
        let not_null = client
            .execute("INSERT INTO ef VALUES (2, NULL)", &[])
            .await
            .expect_err("a NULL in a NOT NULL column must fail");
        let not_null = not_null
            .as_db_error()
            .expect("a server-sent error must survive as a DbError");

        assert_eq!(
            not_null.code().code(),
            "23502",
            "not_null_violation SQLSTATE"
        );
        assert_eq!(not_null.table(), Some("ef"));
        assert_eq!(not_null.column(), Some("b"));
        // The mirror of the assertion above: the fields swap between the two
        // errors, so neither can be satisfied by a mapping that ignores tags.
        assert_eq!(
            not_null.constraint(),
            None,
            "a not-null violation names no constraint; a swapped mapping would fill it"
        );

        // ---- check violation: constraint again, on a different failure ----
        let check = client
            .execute("INSERT INTO ef VALUES (3, -5)", &[])
            .await
            .expect_err("a failing CHECK must fail");
        let check = check
            .as_db_error()
            .expect("a server-sent error must survive as a DbError");
        assert_eq!(check.code().code(), "23514", "check_violation SQLSTATE");
        assert_eq!(check.constraint(), Some("ef_b_check"));
        assert_eq!(check.column(), None);
    })
    .await
    .expect("error-field test exceeded its watchdog");
}
