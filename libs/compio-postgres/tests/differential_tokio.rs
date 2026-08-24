//! Differential tests: this driver against the one it was ported from.
//!
//! `compio-postgres` is a hand-written port of `tokio-postgres` onto a
//! different I/O model. Most of the defects found while hardening it were not
//! in the ported logic but in what the port had to rewrite, and the cheapest
//! way to see a rewrite diverge is to ask both crates the same question and
//! compare the answers.
//!
//! WHAT THIS DOES NOT TEST. Both crates decode values with the same
//! `postgres-types` code, so agreeing on `SELECT 1::int4` proves nothing about
//! either. The cases here are ones each driver implements ITSELF: how a
//! command tag becomes a rows-affected count, and which SQLSTATE and message a
//! failure carries. Those are the surfaces a port writes from scratch.
//!
//! `tokio-postgres` is a DEV-dependency only, permitted by the 2026-08-24
//! decision recorded in AGENTS.md. It never enters a shipped binary, and
//! `tests/zero_tokio_gate.sh` still refuses a normal or build dependency on
//! tokio.
//!
//! The two runtimes never mix: tokio-postgres runs on its own thread with a
//! current-thread tokio runtime and returns plain data over a channel, so no
//! tokio reactor is ever installed on a compio thread.

use compio_postgres::NoTls;

#[allow(dead_code)]
mod common;

/// What both drivers are asked to report for one statement.
///
/// Deliberately plain data - no driver types cross the channel, so the
/// comparison cannot accidentally compare two views of the same object.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    /// `execute` succeeded, reporting this many rows affected.
    Rows(u64),
    /// The server refused it with this SQLSTATE.
    SqlState(String),
    /// It failed without a SQLSTATE, i.e. not a server error.
    LocalFailure,
}

/// Statements whose command tag or error is the driver's own work to parse.
///
/// Each is paired with why it is here; a case nobody can explain is a case
/// nobody will maintain.
fn cases() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "SELECT 1 WHERE false",
            "an empty result still carries a command tag, and a driver that \
             reads no count from it must report zero rather than guess",
        ),
        (
            "CREATE TEMPORARY TABLE cpg_diff (id int)",
            "DDL has a tag with no row count at all",
        ),
        (
            "INSERT INTO cpg_diff VALUES (1), (2), (3)",
            "INSERT reports its count in the SECOND field of its tag, not the \
             first - the field a naive parser reads is the oid",
        ),
        (
            "UPDATE cpg_diff SET id = id + 1 WHERE id > 1",
            "UPDATE reports a count in the usual place",
        ),
        (
            "DELETE FROM cpg_diff WHERE id > 100",
            "a statement that matches nothing still succeeds, with zero",
        ),
        (
            "SELECT 1/0",
            "division by zero: SQLSTATE 22012, a server error mid-execution",
        ),
        (
            "SELECT * FROM cpg_no_such_table_anywhere",
            "an undefined table: SQLSTATE 42P01, raised at parse time",
        ),
        (
            "INSERT INTO cpg_diff VALUES ('notanint')",
            "a bad literal: SQLSTATE 22P02, raised during parameter analysis",
        ),
        (
            "SELECT 1; SELECT 2",
            "two statements in one execute: both drivers must agree on whether \
             this is allowed and what it reports",
        ),
    ]
}

/// Run every case through tokio-postgres, on its own thread and runtime.
fn tokio_outcomes(url: String, statements: Vec<&'static str>) -> Vec<Outcome> {
    let (sender, receiver) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build the tokio runtime");
        let outcomes = runtime.block_on(async move {
            let (client, connection) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
                .await
                .expect("tokio-postgres connect");
            let driver = tokio::spawn(async move {
                let _ = connection.await;
            });

            let mut outcomes = Vec::new();
            for statement in statements {
                outcomes.push(match client.execute(statement, &[]).await {
                    Ok(rows) => Outcome::Rows(rows),
                    Err(error) => match error.code() {
                        Some(code) => Outcome::SqlState(code.code().to_owned()),
                        None => Outcome::LocalFailure,
                    },
                });
            }
            drop(client);
            let _ = driver.await;
            outcomes
        });
        let _ = sender.send(outcomes);
    });
    handle.join().expect("the tokio thread panicked");
    receiver.recv().expect("no outcomes came back from tokio")
}

async fn compio_outcomes(url: &str, statements: &[&'static str]) -> Vec<Outcome> {
    let (client, connection) = compio_postgres::connect(url, NoTls)
        .await
        .unwrap_or_else(|error| common::postgres_unreachable(url, &error));
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();

    let mut outcomes = Vec::new();
    for statement in statements {
        outcomes.push(match client.execute(*statement, &[]).await {
            Ok(rows) => Outcome::Rows(rows),
            Err(error) => match error.code() {
                Some(code) => Outcome::SqlState(code.code().to_owned()),
                None => Outcome::LocalFailure,
            },
        });
    }
    outcomes
}

/// Both drivers, the same statements, the same server, compared case by case.
#[compio::test]
async fn both_drivers_agree_on_command_tags_and_sqlstates() {
    let url = common::test_url();
    let cases = cases();
    let statements: Vec<&'static str> = cases.iter().map(|(sql, _)| *sql).collect();

    // Separate sessions, so the TEMPORARY table is created independently in
    // each and neither run depends on the other's leftovers.
    let theirs = tokio_outcomes(url.clone(), statements.clone());
    let ours = compio_outcomes(&url, &statements).await;

    assert_eq!(
        ours.len(),
        theirs.len(),
        "the two runs answered different numbers of statements"
    );

    let mut divergences = Vec::new();
    for (index, (sql, why)) in cases.iter().enumerate() {
        if ours[index] != theirs[index] {
            divergences.push(format!(
                "  {sql}\n    ours: {:?}\n    tokio-postgres: {:?}\n    matters because {why}",
                ours[index], theirs[index]
            ));
        }
    }
    assert!(
        divergences.is_empty(),
        "this driver disagrees with the one it was ported from:\n{}",
        divergences.join("\n")
    );
}

/// A TEMPORARY table lives in a per-SESSION schema, so its name carries an
/// ordinal that differs between the two connections - `pg_temp_3` against
/// `pg_temp_4`. That is server state, not driver output, and comparing it
/// would fail every run while proving nothing. Only the ordinal is erased.
fn normalise_schema(schema: Option<&str>) -> Option<String> {
    schema.map(|name| {
        if name.starts_with("pg_temp_") {
            "pg_temp_N".to_owned()
        } else {
            name.to_owned()
        }
    })
}

/// Every field of a server error, from both drivers.
///
/// Rendered to strings so the comparison cannot compare two views of one
/// object: these values crossed a thread boundary as plain data.
#[derive(Debug, PartialEq, Eq)]
struct Fields {
    severity: String,
    code: String,
    message: String,
    detail: Option<String>,
    hint: Option<String>,
    position: Option<String>,
    where_: Option<String>,
    schema: Option<String>,
    table: Option<String>,
    column: Option<String>,
    datatype: Option<String>,
    constraint: Option<String>,
    routine: Option<String>,
    line_present: bool,
    file_present: bool,
}

/// Failures chosen to populate DIFFERENT field sets.
///
/// A single failing statement would compare one shape and call the parser
/// checked. A constraint violation carries schema/table/constraint; a syntax
/// error carries a position; a bad column carries a column name; a domain
/// violation carries a datatype. Between them nearly every optional field is
/// exercised at least once.
fn error_cases() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "SELECT * FROM cpg_absent_relation",
            "undefined table - message and code only, so it pins the required fields",
        ),
        (
            "SELECT 1 FROM WHERE",
            "syntax error - carries a position, the field whose parse this crate got wrong once",
        ),
        (
            "INSERT INTO cpg_diff_err VALUES (1)",
            "not-null / constraint violation - carries schema, table and constraint",
        ),
        (
            "SELECT cpg_absent_column FROM cpg_diff_err",
            "undefined column - carries a column-ish diagnostic",
        ),
        (
            "SELECT 'x'::integer",
            "invalid text representation - carries a datatype-flavoured message",
        ),
        (
            "DO $$ BEGIN RAISE EXCEPTION 'boom' USING HINT = 'try less', DETAIL = 'the detail'; END $$",
            "a raised exception - the only reliable way to force DETAIL and HINT together",
        ),
    ]
}

const ERROR_FIXTURE: &str = "CREATE TEMPORARY TABLE cpg_diff_err (id int, tag text NOT NULL, CONSTRAINT cpg_diff_uq UNIQUE (id))";

fn tokio_fields(url: String, statements: Vec<&'static str>) -> Vec<Option<Fields>> {
    let (sender, receiver) = std::sync::mpsc::channel();
    let handle =
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build the tokio runtime");
            let collected =
                runtime.block_on(async move {
                    let (client, connection) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
                        .await
                        .expect("tokio-postgres connect");
                    let driver = tokio::spawn(async move {
                        let _ = connection.await;
                    });
                    client
                        .execute(ERROR_FIXTURE, &[])
                        .await
                        .expect("fixture table");

                    let mut collected = Vec::new();
                    for statement in statements {
                        collected.push(client.execute(statement, &[]).await.err().and_then(
                            |error| {
                                error.as_db_error().map(|db| Fields {
                                    severity: db.severity().to_owned(),
                                    code: db.code().code().to_owned(),
                                    message: db.message().to_owned(),
                                    detail: db.detail().map(str::to_owned),
                                    hint: db.hint().map(str::to_owned),
                                    position: db.position().map(|position| format!("{position:?}")),
                                    where_: db.where_().map(str::to_owned),
                                    schema: normalise_schema(db.schema()),
                                    table: db.table().map(str::to_owned),
                                    column: db.column().map(str::to_owned),
                                    datatype: db.datatype().map(str::to_owned),
                                    constraint: db.constraint().map(str::to_owned),
                                    routine: db.routine().map(str::to_owned),
                                    line_present: db.line().is_some(),
                                    file_present: db.file().is_some(),
                                })
                            },
                        ));
                    }
                    drop(client);
                    let _ = driver.await;
                    collected
                });
            let _ = sender.send(collected);
        });
    handle.join().expect("the tokio thread panicked");
    receiver.recv().expect("no fields came back from tokio")
}

async fn compio_fields(url: &str, statements: &[&'static str]) -> Vec<Option<Fields>> {
    let (client, connection) = compio_postgres::connect(url, NoTls)
        .await
        .unwrap_or_else(|error| common::postgres_unreachable(url, &error));
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();
    client
        .execute(ERROR_FIXTURE, &[])
        .await
        .expect("fixture table");

    let mut collected = Vec::new();
    for statement in statements {
        collected.push(
            client
                .execute(*statement, &[])
                .await
                .err()
                .and_then(|error| {
                    error.as_db_error().map(|db| Fields {
                        severity: db.severity().to_owned(),
                        code: db.code().code().to_owned(),
                        message: db.message().to_owned(),
                        detail: db.detail().map(str::to_owned),
                        hint: db.hint().map(str::to_owned),
                        position: db.position().map(|position| format!("{position:?}")),
                        where_: db.where_().map(str::to_owned),
                        schema: normalise_schema(db.schema()),
                        table: db.table().map(str::to_owned),
                        column: db.column().map(str::to_owned),
                        datatype: db.datatype().map(str::to_owned),
                        constraint: db.constraint().map(str::to_owned),
                        routine: db.routine().map(str::to_owned),
                        line_present: db.line().is_some(),
                        file_present: db.file().is_some(),
                    })
                }),
        );
    }
    collected
}

/// Both drivers parse ErrorResponse themselves. They must agree field for
/// field.
#[compio::test]
async fn both_drivers_agree_on_every_error_field() {
    let url = common::test_url();
    let cases = error_cases();
    let statements: Vec<&'static str> = cases.iter().map(|(sql, _)| *sql).collect();

    let theirs = tokio_fields(url.clone(), statements.clone());
    let ours = compio_fields(&url, &statements).await;

    let mut divergences = Vec::new();
    for (index, (sql, why)) in cases.iter().enumerate() {
        if ours[index] != theirs[index] {
            divergences.push(format!(
                "  {sql}\n    ours: {:?}\n    tokio-postgres: {:?}\n    matters because {why}",
                ours[index], theirs[index]
            ));
        }
    }
    assert!(
        divergences.is_empty(),
        "the two hand-written ErrorResponse parsers disagree:\n{}",
        divergences.join("\n")
    );

    // The cases must actually have produced errors, or the comparison above
    // is agreement about nothing.
    let errors = ours.iter().filter(|fields| fields.is_some()).count();
    assert_eq!(
        errors,
        cases.len(),
        "expected every case to fail with a server error, got {errors} of {}",
        cases.len()
    );
}
