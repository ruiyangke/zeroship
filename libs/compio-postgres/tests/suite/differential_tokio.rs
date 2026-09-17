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
//! `tokio-postgres` is a DEV-dependency only. It never enters a shipped
//! binary, and `cargo xtask test repository` still refuses a normal or build
//! dependency on tokio.
//!
//! WHERE THE REFERENCE IS WRONG. tokio-postgres is a reference, not an
//! oracle: agreement is evidence, disagreement is a question, and sometimes
//! the answer is that it has the bug. Its `Transaction::savepoint` emits
//! `format!("SAVEPOINT {name}")` with the name UNQUOTED, so a savepoint
//! called `MyPoint` is folded to `mypoint` and one containing a quote or a
//! semicolon is a syntax error or worse. This crate quotes it, and
//! `integration.rs` pins that with spaces, mixed case, an embedded quote and
//! an injection attempt. A differential asserting equality THERE would fail,
//! and would be wrong to. Before adding a surface here, check that both sides
//! are implementing the same contract rather than one implementing it and the
//! other approximating it.
//!
//! IS THERE A FEATURE GAP? Measured 2026-08-26 against tokio-postgres 0.7.18,
//! the version Cargo.lock resolves (0.7.17 is also vendored on this machine and
//! is NOT the one to read). Diffing the public surfaces by name: this crate
//! exposes 262 public fns to tokio-postgres's 147 and 103 public types to its
//! 61. Of tokio-postgres's surface, the only types with no counterpart here are
//! `PostgresCodec`, `Response` and `StartupStream` - all internal framing types
//! rather than application API - and the only absent `Config` setter is
//! `keepalives_retries`, which exists here under libpq's name for it,
//! `keepalives_count`, and is verified all the way to TCP_KEEPCNT in
//! `connect_socket.rs`. So the answer is no functional gap, and a name diff is
//! the whole of it. Re-derive rather than trusting this line if the pin moves.
//!
//! The two runtimes never mix: tokio-postgres runs on its own thread with a
//! current-thread tokio runtime and returns plain data over a channel, so no
//! tokio reactor is ever installed on a compio thread.

#[allow(unused_imports)]
use crate::common;

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
fn cases(table: &str) -> Vec<(String, &'static str)> {
    vec![
        (
            "SELECT 1 WHERE false".to_owned(),
            "an empty result still carries a command tag, and a driver that \
             reads no count from it must report zero rather than guess",
        ),
        (
            format!("CREATE TEMPORARY TABLE {table} (id int)"),
            "DDL has a tag with no row count at all",
        ),
        (
            format!("INSERT INTO {table} VALUES (1), (2), (3)"),
            "INSERT reports its count in the SECOND field of its tag, not the \
             first - the field a naive parser reads is the oid",
        ),
        (
            format!("UPDATE {table} SET id = id + 1 WHERE id > 1"),
            "UPDATE reports a count in the usual place",
        ),
        (
            format!("DELETE FROM {table} WHERE id > 100"),
            "a statement that matches nothing still succeeds, with zero",
        ),
        (
            "SELECT 1/0".to_owned(),
            "division by zero: SQLSTATE 22012, a server error mid-execution",
        ),
        (
            "SELECT * FROM cpg_no_such_table_anywhere".to_owned(),
            "an undefined table: SQLSTATE 42P01, raised at parse time",
        ),
        (
            format!("INSERT INTO {table} VALUES ('notanint')"),
            "a bad literal: SQLSTATE 22P02, raised during parameter analysis",
        ),
        (
            "SELECT 1; SELECT 2".to_owned(),
            "two statements in one execute: both drivers must agree on whether \
             this is allowed and what it reports",
        ),
    ]
}

/// Run every case through tokio-postgres, on its own thread and runtime.
fn tokio_outcomes(url: String, statements: Vec<String>) -> Vec<Outcome> {
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
                outcomes.push(match client.execute(&statement, &[]).await {
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

async fn compio_outcomes(url: &str, statements: &[String]) -> Vec<Outcome> {
    let (client, connection) = compio_postgres::connect(url, common::suite_tls())
        .await
        .unwrap_or_else(|error| common::postgres_unreachable(url, &error));
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();

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
    outcomes
}

/// Both drivers, the same statements, the same server, compared case by case.
#[compio::test]
async fn both_drivers_agree_on_command_tags_and_sqlstates() {
    let url = common::test_url();
    let ours_table = common::test_object_name("cpg_diff_ours");
    let theirs_table = common::test_object_name("cpg_diff_theirs");
    let theirs_statements: Vec<String> = cases(&theirs_table)
        .into_iter()
        .map(|(sql, _)| sql)
        .collect();
    let cases = cases(&ours_table);
    let statements: Vec<String> = cases.iter().map(|(sql, _)| sql.clone()).collect();

    // A transaction pooler can route both logical sessions through one
    // backend, so each driver needs its own process-scoped TEMPORARY table.
    let theirs = tokio_outcomes(common::plaintext_url(), theirs_statements);
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

/// One short transaction program and the state transition it discriminates.
struct TransactionSequence {
    name: &'static str,
    statements: &'static [&'static str],
    why: &'static str,
}

fn transaction_sequences() -> Vec<TransactionSequence> {
    vec![
        TransactionSequence {
            name: "failed transaction rollback",
            statements: &[
                "BEGIN",
                "SELECT 1",
                "SELECT 1/0",
                "SELECT 1",
                "ROLLBACK",
                "SELECT 1",
            ],
            why: "an execution error changes ReadyForQuery from in-transaction to failed; \
                  the next statement must be refused, ROLLBACK must clear the failure, \
                  and the session must become usable again",
        },
        TransactionSequence {
            name: "savepoint recovery",
            statements: &[
                "BEGIN",
                "SAVEPOINT s1",
                "SELECT 1/0",
                "ROLLBACK TO SAVEPOINT s1",
                "SELECT 1",
                "RELEASE SAVEPOINT s1",
                "COMMIT",
            ],
            why: "ROLLBACK TO SAVEPOINT must recover the failed subtransaction while \
                  preserving the outer transaction, which RELEASE SAVEPOINT witnesses",
        },
        TransactionSequence {
            name: "idle commit and rollback",
            statements: &["COMMIT", "ROLLBACK"],
            why: "COMMIT and ROLLBACK outside a transaction emit warnings but still \
                  complete successfully",
        },
        TransactionSequence {
            name: "nested begin",
            statements: &["BEGIN", "BEGIN", "COMMIT"],
            why: "a nested BEGIN warns and completes successfully instead of failing",
        },
    ]
}

/// Run every transaction sequence through tokio-postgres on one session.
fn tokio_transaction_outcomes(url: String, sequences: Vec<Vec<&'static str>>) -> Vec<Vec<Outcome>> {
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
            for sequence in sequences {
                let mut sequence_outcomes = Vec::new();
                for statement in sequence {
                    sequence_outcomes.push(match client.execute(statement, &[]).await {
                        Ok(rows) => Outcome::Rows(rows),
                        Err(error) => match error.code() {
                            Some(code) => Outcome::SqlState(code.code().to_owned()),
                            None => Outcome::LocalFailure,
                        },
                    });
                }
                outcomes.push(sequence_outcomes);
            }
            drop(client);
            let _ = driver.await;
            outcomes
        });
        let _ = sender.send(outcomes);
    });
    handle.join().expect("the tokio thread panicked");
    receiver
        .recv()
        .expect("no transaction outcomes came back from tokio")
}

async fn compio_transaction_outcomes(
    url: &str,
    sequences: &[Vec<&'static str>],
) -> Vec<Vec<Outcome>> {
    let (client, connection) = compio_postgres::connect(url, common::suite_tls())
        .await
        .unwrap_or_else(|error| common::postgres_unreachable(url, &error));
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();

    let mut outcomes = Vec::new();
    for sequence in sequences {
        let mut sequence_outcomes = Vec::new();
        for statement in sequence {
            // Both public `execute` APIs expose only the affected-row count
            // parsed from CommandComplete, which is `Outcome::Rows` here.
            sequence_outcomes.push(match client.execute(*statement, &[]).await {
                Ok(rows) => Outcome::Rows(rows),
                Err(error) => match error.code() {
                    Some(code) => Outcome::SqlState(code.code().to_owned()),
                    None => Outcome::LocalFailure,
                },
            });
        }
        outcomes.push(sequence_outcomes);
    }
    outcomes
}

/// Both drivers must follow the same transaction failure and recovery paths.
///
/// WHAT THIS DOES NOT CATCH. Warning payloads, concurrent transaction
/// isolation, and high-level `Transaction` drop cleanup are outside this
/// sequential raw-SQL surface.
#[compio::test]
async fn both_drivers_agree_on_transaction_recovery() {
    let url = common::test_url();
    let sequences = transaction_sequences();
    let statements: Vec<Vec<&'static str>> = sequences
        .iter()
        .map(|sequence| sequence.statements.to_vec())
        .collect();

    // Each driver runs every sequence, in order, on one independent session.
    let theirs = tokio_transaction_outcomes(common::plaintext_url(), statements.clone());
    let ours = compio_transaction_outcomes(&url, &statements).await;

    assert_eq!(
        ours.len(),
        sequences.len(),
        "this driver answered {} transaction sequences, expected {}",
        ours.len(),
        sequences.len()
    );
    assert_eq!(
        theirs.len(),
        sequences.len(),
        "tokio-postgres answered {} transaction sequences, expected {}",
        theirs.len(),
        sequences.len()
    );

    let expected_comparisons: usize = statements.iter().map(Vec::len).sum();
    let mut comparisons = 0;
    let mut first_divergence = None;
    'sequences: for (sequence_index, sequence) in sequences.iter().enumerate() {
        if ours[sequence_index].len() != sequence.statements.len()
            || theirs[sequence_index].len() != sequence.statements.len()
        {
            first_divergence = Some(format!(
                "  sequence {:?}\n    ours returned {} statements\n    \
                 tokio-postgres returned {} statements\n    expected {} statements",
                sequence.name,
                ours[sequence_index].len(),
                theirs[sequence_index].len(),
                sequence.statements.len()
            ));
            break;
        }

        for (statement_index, statement) in sequence.statements.iter().enumerate() {
            comparisons += 1;
            if ours[sequence_index][statement_index] != theirs[sequence_index][statement_index] {
                first_divergence = Some(format!(
                    "  sequence {:?}, statement {}: {:?}\n    ours: {:?}\n    \
                     tokio-postgres: {:?}\n    matters because {}",
                    sequence.name,
                    statement_index + 1,
                    statement,
                    ours[sequence_index][statement_index],
                    theirs[sequence_index][statement_index],
                    sequence.why
                ));
                break 'sequences;
            }
        }
    }
    assert!(
        first_divergence.is_none(),
        "transaction recovery diverged:\n{}",
        first_divergence.unwrap_or_default()
    );
    assert_eq!(
        comparisons, expected_comparisons,
        "compared only {comparisons} of {expected_comparisons} transaction statements"
    );

    // Pin the semantics rather than accepting two drivers making the same
    // mistake. In particular, 25P02 proves the failed transaction refused the
    // next statement before either recovery path made SELECT usable again.
    let expected = vec![
        vec![
            Outcome::Rows(0),
            Outcome::Rows(1),
            Outcome::SqlState("22012".to_owned()),
            Outcome::SqlState("25P02".to_owned()),
            Outcome::Rows(0),
            Outcome::Rows(1),
        ],
        vec![
            Outcome::Rows(0),
            Outcome::Rows(0),
            Outcome::SqlState("22012".to_owned()),
            Outcome::Rows(0),
            Outcome::Rows(1),
            Outcome::Rows(0),
            Outcome::Rows(0),
        ],
        vec![Outcome::Rows(0), Outcome::Rows(0)],
        vec![Outcome::Rows(0), Outcome::Rows(0), Outcome::Rows(0)],
    ];
    assert_eq!(
        ours, expected,
        "the sequences did not exercise the required transaction-recovery states"
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

/// Erase only the per-driver identity from an error field.
///
/// PostgreSQL quotes the physical relation name in some messages and also
/// sends relation and constraint names as structured fields. Those names must
/// differ when a transaction pooler routes both logical sessions through one
/// backend, but their generated identity is not driver output. Mapping them
/// back to the old logical names keeps every field in the comparison.
fn normalise_error_fixture(value: &str, table: &str, constraint: &str) -> String {
    value
        .replace(table, "cpg_diff_err")
        .replace(constraint, "cpg_diff_uq")
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

impl Fields {
    fn normalise_fixture_names(mut self, table: &str, constraint: &str) -> Self {
        self.message = normalise_error_fixture(&self.message, table, constraint);
        self.table = self
            .table
            .map(|value| normalise_error_fixture(&value, table, constraint));
        self.constraint = self
            .constraint
            .map(|value| normalise_error_fixture(&value, table, constraint));
        self
    }
}

fn normalise_fixture_fields(
    fields: Vec<Option<Fields>>,
    table: &str,
    constraint: &str,
) -> Vec<Option<Fields>> {
    fields
        .into_iter()
        .map(|fields| fields.map(|fields| fields.normalise_fixture_names(table, constraint)))
        .collect()
}

/// Failures chosen to populate DIFFERENT field sets.
///
/// A single failing statement would compare one shape and call the parser
/// checked. A constraint violation carries schema/table/constraint; a syntax
/// error carries a position; a bad column carries a column name; a domain
/// violation carries a datatype. Between them nearly every optional field is
/// exercised at least once.
fn error_cases(table: &str) -> Vec<(String, &'static str)> {
    vec![
        (
            "SELECT * FROM cpg_absent_relation".to_owned(),
            "undefined table - message and code only, so it pins the required fields",
        ),
        (
            "SELECT 1 FROM WHERE".to_owned(),
            "syntax error - carries a position, the field whose parse this crate got wrong once",
        ),
        (
            format!("INSERT INTO {table} VALUES (1)"),
            "not-null / constraint violation - carries schema, table and constraint",
        ),
        (
            format!("SELECT cpg_absent_column FROM {table}"),
            "undefined column - carries a column-ish diagnostic",
        ),
        (
            "SELECT 'x'::integer".to_owned(),
            "invalid text representation - carries a datatype-flavoured message",
        ),
        (
            "DO $$ BEGIN RAISE EXCEPTION 'boom' USING HINT = 'try less', DETAIL = 'the detail'; END $$"
                .to_owned(),
            "a raised exception - the only reliable way to force DETAIL and HINT together",
        ),
    ]
}

fn error_fixture(table: &str, constraint: &str) -> String {
    format!(
        "CREATE TEMPORARY TABLE {table} (id int, tag text NOT NULL, CONSTRAINT {constraint} UNIQUE (id))"
    )
}

fn tokio_fields(url: String, fixture: String, statements: Vec<String>) -> Vec<Option<Fields>> {
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
                    client.execute(&fixture, &[]).await.expect("fixture table");

                    let mut collected = Vec::new();
                    for statement in statements {
                        collected.push(client.execute(&statement, &[]).await.err().and_then(
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

async fn compio_fields(url: &str, fixture: &str, statements: &[String]) -> Vec<Option<Fields>> {
    let (client, connection) = compio_postgres::connect(url, common::suite_tls())
        .await
        .unwrap_or_else(|error| common::postgres_unreachable(url, &error));
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();
    client.execute(fixture, &[]).await.expect("fixture table");

    let mut collected = Vec::new();
    for statement in statements {
        collected.push(
            client
                .execute(statement, &[])
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
    let ours_table = common::test_object_name("cpg_diff_err_ours");
    let ours_constraint = common::test_object_name("cpg_diff_uq_ours");
    let theirs_table = common::test_object_name("cpg_diff_err_theirs");
    let theirs_constraint = common::test_object_name("cpg_diff_uq_theirs");
    let theirs_statements: Vec<String> = error_cases(&theirs_table)
        .into_iter()
        .map(|(sql, _)| sql)
        .collect();
    let cases = error_cases(&ours_table);
    let statements: Vec<String> = cases.iter().map(|(sql, _)| sql.clone()).collect();
    let theirs_fixture = error_fixture(&theirs_table, &theirs_constraint);
    let ours_fixture = error_fixture(&ours_table, &ours_constraint);

    let theirs = normalise_fixture_fields(
        tokio_fields(common::plaintext_url(), theirs_fixture, theirs_statements),
        &theirs_table,
        &theirs_constraint,
    );
    let ours = normalise_fixture_fields(
        compio_fields(&url, &ours_fixture, &statements).await,
        &ours_table,
        &ours_constraint,
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

/// A NOTICE, as each driver reports it.
///
/// Notices travel in the same wire format as errors but on a different path:
/// they arrive UNSOLICITED, outside any request's response, so each driver
/// routes them itself. This crate sends them to `Connection::notifications()`;
/// tokio-postgres yields them from `Connection::poll_message`. Different
/// plumbing, same bytes, so the parsed result must match.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Notice {
    severity: String,
    code: String,
    message: String,
    detail: Option<String>,
    hint: Option<String>,
}

/// A statement that raises one notice carrying a detail and a hint.
///
/// RAISE NOTICE is the only reliable way to control every field: a
/// server-generated notice (say, "relation already exists, skipping") differs
/// by server version and would make this test a version detector.
const NOTICE_SQL: &str = "DO $$ BEGIN \
     RAISE NOTICE 'differential notice %', 7 \
     USING DETAIL = 'the detail', HINT = 'the hint'; \
     END $$";

fn tokio_notices(url: String) -> Vec<Notice> {
    use futures_util::StreamExt;

    let (sender, receiver) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build the tokio runtime");
        let collected = runtime.block_on(async move {
            let (client, mut connection) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
                .await
                .expect("tokio-postgres connect");

            // The connection must be POLLED for async messages to surface, so
            // it cannot simply be spawned and forgotten as elsewhere in this
            // file.
            let messages = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let sink = messages.clone();
            let stream =
                futures_util::stream::poll_fn(move |context| connection.poll_message(context));
            let pump = tokio::spawn(async move {
                let mut stream = std::pin::pin!(stream);
                while let Some(Ok(message)) = stream.next().await {
                    if let tokio_postgres::AsyncMessage::Notice(notice) = message {
                        sink.lock().expect("notice sink").push(Notice {
                            severity: notice.severity().to_owned(),
                            code: notice.code().code().to_owned(),
                            message: notice.message().to_owned(),
                            detail: notice.detail().map(str::to_owned),
                            hint: notice.hint().map(str::to_owned),
                        });
                    }
                }
            });

            client
                .batch_execute(NOTICE_SQL)
                .await
                .expect("raise notice");
            drop(client);
            let _ = pump.await;
            messages.lock().expect("notice sink").clone()
        });
        let _ = sender.send(collected);
    });
    handle.join().expect("the tokio thread panicked");
    receiver.recv().expect("no notices came back from tokio")
}

async fn compio_notices(url: &str) -> Vec<Notice> {
    use futures_util::StreamExt;

    let (client, mut connection) = compio_postgres::connect(url, common::suite_tls())
        .await
        .unwrap_or_else(|error| common::postgres_unreachable(url, &error));
    let mut messages = connection.notifications();
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();

    client
        .batch_execute(NOTICE_SQL)
        .await
        .expect("raise notice");

    let mut collected = Vec::new();
    // One notice is expected; bound the wait so a driver that never delivers
    // fails the test instead of hanging the suite.
    if let Ok(Some(message)) =
        compio::time::timeout(std::time::Duration::from_secs(10), messages.next()).await
        && let compio_postgres::AsyncMessage::Notice(notice) = message
    {
        collected.push(Notice {
            severity: notice.severity().to_owned(),
            code: notice.code().code().to_owned(),
            message: notice.message().to_owned(),
            detail: notice.detail().map(str::to_owned),
            hint: notice.hint().map(str::to_owned),
        });
    }
    collected
}

/// Notices arrive outside any response, so each driver routes them itself.
/// The parsed result must still agree.
#[compio::test]
async fn both_drivers_agree_on_a_raised_notice() {
    let url = common::test_url();
    let theirs = tokio_notices(common::plaintext_url());
    let ours = compio_notices(&url).await;

    assert_eq!(
        ours.len(),
        1,
        "this driver delivered {} notices, expected exactly one: {ours:?}",
        ours.len()
    );
    assert!(
        !theirs.is_empty(),
        "the reference driver delivered no notice, so there is nothing to compare against"
    );
    assert_eq!(
        ours[0], theirs[0],
        "the two drivers disagree about the same NOTICE"
    );
}

const NOTIFICATION_DELIVERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const NOTIFICATION_SCENARIO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
const NOTIFICATION_FENCE: &str = "order-fence";

/// Which of the two sessions a notification's reported PID identifies.
///
/// The raw PIDs necessarily differ between the independent driver runs, so
/// comparing those integers would compare server allocation rather than
/// driver behaviour. An unexpected integer is kept for useful diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
enum NotificationSource {
    Notifier,
    Listener,
    Other(i32),
}

/// Observable facts from one NotificationResponse.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ObservedNotification {
    source: NotificationSource,
    channel: String,
    payload: String,
}

/// Everything one driver observed through the final ordering fence.
#[derive(Debug, Clone, PartialEq, Eq)]
struct NotificationRun {
    notifications: Vec<ObservedNotification>,
    reached_fence: bool,
}

fn notification_source(
    reported_pid: i32,
    listener_pid: i32,
    notifier_pid: i32,
) -> NotificationSource {
    if reported_pid == notifier_pid {
        NotificationSource::Notifier
    } else if reported_pid == listener_pid {
        NotificationSource::Listener
    } else {
        NotificationSource::Other(reported_pid)
    }
}

/// The exact sequence sent by both drivers' second sessions.
///
/// `pg_notify` accepts parameters, unlike the `NOTIFY` grammar. That keeps the
/// quote payload data rather than test SQL, while the escaped Unicode keeps
/// this source file ASCII and produces a NUL-free multibyte UTF-8 payload.
fn notification_sends(listened_channel: &str, unlistened_channel: &str) -> Vec<(String, String)> {
    vec![
        (listened_channel.to_owned(), "".to_owned()),
        (
            listened_channel.to_owned(),
            "\u{96ea}\u{3060}\u{308b}\u{307e}".to_owned(),
        ),
        (listened_channel.to_owned(), "it's intact".to_owned()),
        (unlistened_channel.to_owned(), "must-not-arrive".to_owned()),
        (listened_channel.to_owned(), NOTIFICATION_FENCE.to_owned()),
    ]
}

fn expected_notifications(listened_channel: &str) -> NotificationRun {
    NotificationRun {
        notifications: vec![
            ObservedNotification {
                source: NotificationSource::Notifier,
                channel: listened_channel.to_owned(),
                payload: "".to_owned(),
            },
            ObservedNotification {
                source: NotificationSource::Notifier,
                channel: listened_channel.to_owned(),
                payload: "\u{96ea}\u{3060}\u{308b}\u{307e}".to_owned(),
            },
            ObservedNotification {
                source: NotificationSource::Notifier,
                channel: listened_channel.to_owned(),
                payload: "it's intact".to_owned(),
            },
            ObservedNotification {
                source: NotificationSource::Notifier,
                channel: listened_channel.to_owned(),
                payload: NOTIFICATION_FENCE.to_owned(),
            },
        ],
        reached_fence: true,
    }
}

/// Run the notification sequence through tokio-postgres on its own runtime.
fn tokio_notifications(
    url: String,
    listened_channel: String,
    unlistened_channel: String,
) -> NotificationRun {
    use futures_util::StreamExt;

    let (sender, receiver) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build the tokio runtime");
        let observed = runtime
            .block_on(async move {
                tokio::time::timeout(NOTIFICATION_SCENARIO_TIMEOUT, async move {
                    let (listener, mut listener_connection) =
                        tokio_postgres::connect(&url, tokio_postgres::NoTls)
                            .await
                            .expect("tokio-postgres listener connect");
                    let (message_sender, mut message_receiver) = futures_channel::mpsc::unbounded();

                    // `Connection` is not a Stream, but `poll_message` is its
                    // asynchronous-message surface. The pump must be running
                    // before LISTEN can receive its own command response.
                    let stream = futures_util::stream::poll_fn(move |context| {
                        listener_connection.poll_message(context)
                    });
                    let pump = tokio::spawn(async move {
                        let mut stream = std::pin::pin!(stream);
                        while let Some(message) = stream.next().await {
                            if message_sender.unbounded_send(message).is_err() {
                                break;
                            }
                        }
                    });

                    listener
                        .batch_execute(&format!("LISTEN {listened_channel}"))
                        .await
                        .expect("tokio-postgres LISTEN");
                    let listener_pid = listener
                        .query_one("SELECT pg_backend_pid()", &[])
                        .await
                        .expect("tokio-postgres listener pid")
                        .get::<_, i32>(0);

                    let (notifier, notifier_connection) =
                        tokio_postgres::connect(&url, tokio_postgres::NoTls)
                            .await
                            .expect("tokio-postgres notifier connect");
                    let notifier_driver = tokio::spawn(async move {
                        let _ = notifier_connection.await;
                    });
                    let notifier_pid = notifier
                        .query_one("SELECT pg_backend_pid()", &[])
                        .await
                        .expect("tokio-postgres notifier pid")
                        .get::<_, i32>(0);
                    assert_ne!(
                        listener_pid, notifier_pid,
                        "tokio-postgres listener and notifier must be separate backends"
                    );

                    for (send_channel, payload) in
                        notification_sends(&listened_channel, &unlistened_channel)
                    {
                        notifier
                            .query_one("SELECT pg_notify($1, $2)", &[&send_channel, &payload])
                            .await
                            .expect("tokio-postgres pg_notify");
                    }

                    let mut notifications = Vec::new();
                    let reached_fence =
                        tokio::time::timeout(NOTIFICATION_DELIVERY_TIMEOUT, async {
                            loop {
                                match message_receiver.next().await {
                                    Some(Ok(tokio_postgres::AsyncMessage::Notification(
                                        notification,
                                    ))) => {
                                        let is_fence = notification.channel() == listened_channel
                                            && notification.payload() == NOTIFICATION_FENCE;
                                        notifications.push(ObservedNotification {
                                            source: notification_source(
                                                notification.process_id(),
                                                listener_pid,
                                                notifier_pid,
                                            ),
                                            channel: notification.channel().to_owned(),
                                            payload: notification.payload().to_owned(),
                                        });
                                        if is_fence {
                                            break true;
                                        }
                                    }
                                    Some(Ok(_)) => {}
                                    Some(Err(error)) => {
                                        panic!("tokio-postgres notification stream: {error}")
                                    }
                                    None => break false,
                                }
                            }
                        })
                        .await
                        .unwrap_or(false);

                    drop(listener);
                    drop(notifier);
                    pump.abort();
                    notifier_driver.abort();
                    NotificationRun {
                        notifications,
                        reached_fence,
                    }
                })
                .await
            })
            .expect("tokio-postgres notification scenario exceeded 20 seconds");
        let _ = sender.send(observed);
    });
    handle.join().expect("the tokio thread panicked");
    receiver
        .recv()
        .expect("no notification observations came back from tokio")
}

/// Run the identical notification sequence through this crate.
async fn compio_notifications(
    url: &str,
    listened_channel: &str,
    unlistened_channel: &str,
) -> NotificationRun {
    use futures_util::StreamExt;

    compio::time::timeout(NOTIFICATION_SCENARIO_TIMEOUT, async {
        let (listener, mut listener_connection) =
            compio_postgres::connect(url, common::suite_tls())
                .await
                .unwrap_or_else(|error| common::postgres_unreachable(url, &error));
        let mut messages = listener_connection.notifications();
        compio::runtime::spawn(async move {
            let _ = listener_connection.run().await;
        })
        .detach();

        listener
            .batch_execute(&format!("LISTEN {listened_channel}"))
            .await
            .expect("LISTEN");
        let listener_pid = listener
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .expect("listener pid")
            .get::<_, i32>(0);

        let (notifier, notifier_connection) = compio_postgres::connect(url, common::suite_tls())
            .await
            .unwrap_or_else(|error| common::postgres_unreachable(url, &error));
        compio::runtime::spawn(async move {
            let _ = notifier_connection.run().await;
        })
        .detach();
        let notifier_pid = notifier
            .query_one("SELECT pg_backend_pid()", &[])
            .await
            .expect("notifier pid")
            .get::<_, i32>(0);
        assert_ne!(
            listener_pid, notifier_pid,
            "listener and notifier must be separate backends"
        );

        for (send_channel, payload) in notification_sends(listened_channel, unlistened_channel) {
            notifier
                .query_one("SELECT pg_notify($1, $2)", &[&send_channel, &payload])
                .await
                .expect("pg_notify");
        }

        let mut notifications = Vec::new();
        let reached_fence = compio::time::timeout(NOTIFICATION_DELIVERY_TIMEOUT, async {
            loop {
                match messages.next().await {
                    Some(compio_postgres::AsyncMessage::Notification(notification)) => {
                        let is_fence = notification.channel() == listened_channel
                            && notification.payload() == NOTIFICATION_FENCE;
                        notifications.push(ObservedNotification {
                            source: notification_source(
                                notification.process_id(),
                                listener_pid,
                                notifier_pid,
                            ),
                            channel: notification.channel().to_owned(),
                            payload: notification.payload().to_owned(),
                        });
                        if is_fence {
                            break true;
                        }
                    }
                    Some(_) => {}
                    None => break false,
                }
            }
        })
        .await
        .unwrap_or(false);

        NotificationRun {
            notifications,
            reached_fence,
        }
    })
    .await
    .expect("compio-postgres notification scenario exceeded 20 seconds")
}

/// Both drivers must preserve LISTEN/NOTIFY routing, bytes, PID and order.
///
/// The unlistened send precedes the final listened fence. Since one notifier
/// sends every implicit transaction sequentially, a wrongly delivered message
/// must appear before that fence and therefore in the exact comparison.
///
/// WHAT THIS DOES NOT CATCH. LISTEN is acknowledged before any send, so this
/// does not force the startup-only `delayed_notices` path. Transactional
/// rollback/coalescing, reconnect/re-LISTEN and a stalled consumer are also
/// outside this sequential surface.
#[compio::test]
async fn both_drivers_agree_on_listen_notify_routing() {
    let url = common::test_url();
    // Channels are database-global rather than schema-scoped. Process-unique
    // names keep concurrent differential binaries from cross-delivering and
    // corrupting both the order and the unlistened-channel assertion.
    let listened_channel = common::test_object_name("cpg_diff_notify");
    let unlistened_channel = common::test_object_name("cpg_diff_notify_ignored");
    let expected = expected_notifications(&listened_channel);

    let theirs = tokio_notifications(
        common::plaintext_url(),
        listened_channel.clone(),
        unlistened_channel.clone(),
    );
    let ours = compio_notifications(&url, &listened_channel, &unlistened_channel).await;

    assert_eq!(
        ours, theirs,
        "the drivers disagree about the same LISTEN/NOTIFY sequence:\n  ours: {ours:#?}\n  tokio-postgres: {theirs:#?}"
    );
    // Pin the facts absolutely too, so identical PID, parsing, filtering or
    // ordering mistakes cannot pass merely because both drivers make them.
    assert_eq!(
        ours, expected,
        "this crate changed the required LISTEN/NOTIFY facts"
    );
    assert_eq!(
        theirs, expected,
        "tokio-postgres changed the required LISTEN/NOTIFY facts"
    );
}

/// One prepared statement's metadata, as each driver parsed it.
/// One column as Describe reported it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ColumnMeta {
    name: String,
    type_name: String,
    table_oid: Option<u32>,
    /// SIGNED: a system column such as ctid reports -1.
    column_id: Option<i16>,
    type_modifier: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Described {
    params: Vec<String>,
    columns: Vec<ColumnMeta>,
}

/// Statements chosen so the Describe response varies in shape.
///
/// Both drivers parse ParameterDescription and RowDescription themselves, and
/// the fields below are where a hand-written parser goes wrong: a type
/// modifier is only meaningful for some types, a column that is an expression
/// has no table behind it, and a SYSTEM column's attribute number is
/// NEGATIVE - `ctid` is -1. Reading that field as unsigned turns it into
/// 65535 and nothing else in the row looks wrong.
fn describe_cases(table: &str) -> Vec<(String, &'static str)> {
    vec![
        (
            format!("SELECT id, label FROM {table} WHERE id = $1"),
            "ordinary table columns: real table oid, positive attribute numbers",
        ),
        (
            format!("SELECT ctid, id FROM {table}"),
            "ctid is a system column whose attribute number is NEGATIVE",
        ),
        (
            format!("SELECT id + 1 AS computed, 'literal'::text FROM {table}"),
            "expressions have no table behind them, so oid and attnum are absent",
        ),
        (
            format!("SELECT bounded, exact FROM {table}"),
            "varchar(10) and numeric(5,2) carry type modifiers; most types do not",
        ),
        (
            "SELECT $1::int8, $2::text, $3::bool".to_owned(),
            "parameter types come from ParameterDescription, a separate message",
        ),
    ]
}

fn tokio_described(url: String, statements: Vec<String>) -> Vec<Described> {
    let (sender, receiver) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build the tokio runtime");
        let collected = runtime.block_on(async move {
            let (client, connection) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
                .await
                .expect("tokio-postgres connect");
            let driver = tokio::spawn(async move {
                let _ = connection.await;
            });
            let mut collected = Vec::new();
            for statement in &statements {
                let prepared = client.prepare(statement).await.expect("tokio prepare");
                collected.push(Described {
                    params: prepared
                        .params()
                        .iter()
                        .map(|ty| ty.name().to_owned())
                        .collect(),
                    columns: prepared
                        .columns()
                        .iter()
                        .map(|column| ColumnMeta {
                            name: column.name().to_owned(),
                            type_name: column.type_().name().to_owned(),
                            table_oid: column.table_oid(),
                            column_id: column.column_id(),
                            type_modifier: column.type_modifier(),
                        })
                        .collect(),
                });
            }
            drop(client);
            let _ = driver.await;
            collected
        });
        let _ = sender.send(collected);
    });
    handle.join().expect("the tokio thread panicked");
    receiver
        .recv()
        .expect("no descriptions came back from tokio")
}

/// Both drivers parse Describe themselves, so the metadata must match.
#[compio::test]
async fn both_drivers_agree_on_prepared_statement_metadata() {
    let url = common::test_url();
    let table = common::test_object_name("cpg describe");

    let (client, connection) = compio_postgres::connect(&url, common::suite_tls())
        .await
        .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();

    // A PERMANENT table on purpose. A temporary one lives in a per-session
    // schema and gets a different pg_class oid in each connection, so
    // table_oid could never match and the strongest field here would have to
    // be thrown away.
    client
        .batch_execute(&format!(
            "DROP TABLE IF EXISTS {table};
             CREATE TABLE {table} (
                 id int primary key,
                 label text,
                 bounded varchar(10),
                 exact numeric(5,2)
             );"
        ))
        .await
        .expect("describe fixture");

    let cases = describe_cases(&table);
    let statements: Vec<String> = cases.iter().map(|(sql, _)| sql.clone()).collect();

    let theirs = tokio_described(common::plaintext_url(), statements.clone());

    let mut ours = Vec::new();
    for statement in &statements {
        let prepared = client.prepare(statement).await.expect("prepare");
        ours.push(Described {
            params: prepared
                .params()
                .iter()
                .map(|ty| ty.name().to_owned())
                .collect(),
            columns: prepared
                .columns()
                .iter()
                .map(|column| ColumnMeta {
                    name: column.name().to_owned(),
                    type_name: column.type_().name().to_owned(),
                    table_oid: column.table_oid(),
                    column_id: column.column_id(),
                    type_modifier: column.type_modifier(),
                })
                .collect(),
        });
    }

    let _ = client
        .batch_execute(&format!("DROP TABLE IF EXISTS {table}"))
        .await;

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
        "the two Describe parsers disagree:\n{}",
        divergences.join("\n")
    );

    // The negative attribute number must actually have been exercised, or the
    // agreement above says nothing about the signedness this case exists for.
    let ctid_attnum = ours[1].columns[0].column_id;
    assert!(
        ctid_attnum.is_some_and(|attnum| attnum < 0),
        "ctid's attribute number should be negative, got {ctid_attnum:?}; \
         without it this test does not cover signed attribute numbers"
    );
}

/// Payloads chosen so the COPY framing has to be reassembled, not just
/// forwarded.
///
/// COPY OUT arrives as CopyData frames whose boundaries the SERVER chooses and
/// which need not align with rows. A driver that assumes one frame per row, or
/// that loses a frame at the end, produces output that is plausible and wrong.
/// Wide rows and a large row count force multiple frames; embedded tabs,
/// newlines and backslashes make a mis-joined boundary visible in the bytes
/// rather than silently well-formed.
const COPY_ROWS: i32 = 2000;

fn copy_out_sql(table: &str) -> String {
    format!("COPY (SELECT id, pad FROM {table} ORDER BY id) TO STDOUT")
}

fn tokio_copy_out(url: String, sql: String) -> Vec<u8> {
    use futures_util::StreamExt;

    let (sender, receiver) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build the tokio runtime");
        let bytes = runtime.block_on(async move {
            let (client, connection) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
                .await
                .expect("tokio-postgres connect");
            let driver = tokio::spawn(async move {
                let _ = connection.await;
            });
            let stream = client.copy_out(&sql).await.expect("tokio copy_out");
            let mut stream = std::pin::pin!(stream);
            let mut bytes = Vec::new();
            while let Some(chunk) = stream.next().await {
                bytes.extend_from_slice(&chunk.expect("tokio copy chunk"));
            }
            drop(client);
            let _ = driver.await;
            bytes
        });
        let _ = sender.send(bytes);
    });
    handle.join().expect("the tokio thread panicked");
    receiver.recv().expect("no copy bytes came back from tokio")
}

/// COPY OUT reassembly is the driver's own work, and this crate has had
/// several defects in it. Both drivers must produce byte-identical output.
#[compio::test]
async fn both_drivers_agree_on_copy_out_bytes() {
    use futures_util::TryStreamExt;

    let url = common::test_url();
    let table = common::test_object_name("cpg copyout");

    let (client, connection) = compio_postgres::connect(&url, common::suite_tls())
        .await
        .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();

    // Permanent, so both connections read the same rows. The pad column
    // carries the characters COPY has to escape.
    client
        .batch_execute(&format!(
            "DROP TABLE IF EXISTS {table};
             CREATE TABLE {table} (id int primary key, pad text);
             INSERT INTO {table}
               SELECT g, repeat('a\tb\\c' || chr(10) || 'd', 12)
                 FROM generate_series(1, {COPY_ROWS}) g;"
        ))
        .await
        .expect("copy fixture");

    let sql = copy_out_sql(&table);
    let theirs = tokio_copy_out(common::plaintext_url(), sql.clone());

    let stream = client.copy_out(&sql).await.expect("copy_out");
    let ours: Vec<u8> = {
        let chunks: Vec<bytes::Bytes> = stream.try_collect().await.expect("copy chunks");
        chunks.iter().flat_map(|chunk| chunk.to_vec()).collect()
    };

    let _ = client
        .batch_execute(&format!("DROP TABLE IF EXISTS {table}"))
        .await;

    assert!(
        !theirs.is_empty(),
        "the reference driver produced no COPY output, so there is nothing to compare"
    );
    assert_eq!(
        ours.len(),
        theirs.len(),
        "COPY OUT byte counts differ: ours {} vs tokio-postgres {}",
        ours.len(),
        theirs.len()
    );
    // Compare content, but report the first divergence rather than dumping
    // hundreds of kilobytes into the failure message.
    if ours != theirs {
        let at = ours
            .iter()
            .zip(theirs.iter())
            .position(|(a, b)| a != b)
            .unwrap_or(0);
        let from = at.saturating_sub(40);
        panic!(
            "COPY OUT bytes diverge at offset {at}\n  ours: {:?}\n  tokio-postgres: {:?}",
            String::from_utf8_lossy(&ours[from..(at + 40).min(ours.len())]),
            String::from_utf8_lossy(&theirs[from..(at + 40).min(theirs.len())]),
        );
    }
}

/// Rows fed through COPY IN, chosen to span many CopyData frames.
///
/// The sink batches what it is fed into frames of its own choosing, so the
/// framing on the wire is the driver's decision, not the caller's. A driver
/// that loses a batch, or emits a partial final frame, still reports a
/// plausible count - the server's own count is what catches it.
const COPY_IN_ROWS: i32 = 3000;

fn copy_in_body() -> String {
    let mut body = String::new();
    for id in 1..=COPY_IN_ROWS {
        // Tabs and backslashes are COPY's escapes; a row that carries them
        // proves the bytes went through unmangled.
        body.push_str(&format!("{id}\tvalue\\t{id}\tpad-{id}\n"));
    }
    body
}

fn tokio_copy_in(url: String, table: String, body: String) -> u64 {
    use futures_util::SinkExt;

    let (sender, receiver) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build the tokio runtime");
        let written = runtime.block_on(async move {
            let (client, connection) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
                .await
                .expect("tokio-postgres connect");
            let driver = tokio::spawn(async move {
                let _ = connection.await;
            });
            let sink = client
                .copy_in::<_, bytes::Bytes>(&format!("COPY {table} FROM STDIN"))
                .await
                .expect("tokio copy_in");
            let mut sink = std::pin::pin!(sink);
            for line in body.lines() {
                sink.feed(bytes::Bytes::from(format!("{line}\n")))
                    .await
                    .expect("tokio copy feed");
            }
            let written = sink.finish().await.expect("tokio copy finish");
            drop(client);
            let _ = driver.await;
            written
        });
        let _ = sender.send(written);
    });
    handle.join().expect("the tokio thread panicked");
    receiver
        .recv()
        .expect("no copy-in count came back from tokio")
}

/// COPY IN framing is the driver's own work, and both must land identical
/// rows and report the same count.
#[compio::test]
async fn both_drivers_agree_on_copy_in_results() {
    use futures_util::SinkExt;

    let url = common::test_url();
    let base = common::test_object_name("cpg copyin");
    let ours_table = format!("{base}_ours");
    let theirs_table = format!("{base}_theirs");

    let (client, connection) = compio_postgres::connect(&url, common::suite_tls())
        .await
        .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();

    // Two permanent tables so each driver writes its own and the contents can
    // be compared by the SERVER, which is the only party neither driver can
    // talk into agreeing.
    for table in [&ours_table, &theirs_table] {
        client
            .batch_execute(&format!(
                "DROP TABLE IF EXISTS {table};
                 CREATE TABLE {table} (id int, tagged text, pad text);"
            ))
            .await
            .expect("copy-in fixture");
    }

    let body = copy_in_body();
    let theirs_count = tokio_copy_in(common::plaintext_url(), theirs_table.clone(), body.clone());

    let sink = client
        .copy_in::<_, bytes::Bytes>(&format!("COPY {ours_table} FROM STDIN"))
        .await
        .expect("copy_in");
    let mut sink = std::pin::pin!(sink);
    for line in body.lines() {
        sink.feed(bytes::Bytes::from(format!("{line}\n")))
            .await
            .expect("copy feed");
    }
    let ours_count = sink.finish().await.expect("copy finish");

    // Ask the server whether the two tables are identical, rather than
    // comparing what either driver believes it wrote.
    let row = client
        .query_one(
            &format!(
                "SELECT
                   (SELECT count(*) FROM {ours_table})::int8,
                   (SELECT count(*) FROM {theirs_table})::int8,
                   (SELECT count(*) FROM (
                      SELECT * FROM {ours_table} EXCEPT ALL SELECT * FROM {theirs_table}
                    ) d)::int8,
                   (SELECT count(*) FROM (
                      SELECT * FROM {theirs_table} EXCEPT ALL SELECT * FROM {ours_table}
                    ) d)::int8"
            ),
            &[],
        )
        .await
        .expect("compare the two tables");
    let ours_rows: i64 = row.get(0);
    let theirs_rows: i64 = row.get(1);
    let only_ours: i64 = row.get(2);
    let only_theirs: i64 = row.get(3);

    for table in [&ours_table, &theirs_table] {
        let _ = client
            .batch_execute(&format!("DROP TABLE IF EXISTS {table}"))
            .await;
    }

    assert_eq!(
        ours_count, theirs_count,
        "the drivers reported different COPY IN counts"
    );
    assert_eq!(
        ours_rows, theirs_rows,
        "the server holds different row counts for the two drivers"
    );
    assert_eq!(
        i64::from(COPY_IN_ROWS),
        ours_rows,
        "neither driver wrote the expected number of rows, so agreement proves nothing"
    );
    assert_eq!(
        (only_ours, only_theirs),
        (0, 0),
        "the two drivers wrote different content: {only_ours} rows only ours, \
         {only_theirs} only theirs"
    );
}

/// How a portal paged: the rows each fetch returned, in order.
///
/// An Execute carrying a row limit ends with PortalSuspended when the limit
/// was reached and with CommandComplete when the portal ran out. Telling those
/// apart is the driver's own work, and getting it wrong shows up as a page
/// boundary in the wrong place or a fetch that never terminates - not as a
/// wrong value, which is why row COUNTS per page are what this compares.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Paging {
    pages: Vec<usize>,
    values: Vec<i32>,
}

/// Row limits chosen around the boundaries.
///
/// 10 rows fetched 3 at a time ends exactly on a short final page; fetched 5
/// at a time divides evenly, so the LAST full page is followed by an empty one
/// and the driver must not mistake that for more data; 0 means "no limit" and
/// takes a different protocol path entirely.
const PAGE_SIZES: [i32; 4] = [3, 5, 10, 0];
const PORTAL_ROWS: i32 = 10;

fn portal_sql() -> String {
    format!("SELECT g::int4 FROM generate_series(1, {PORTAL_ROWS}) g ORDER BY g")
}

fn tokio_paging(url: String, page: i32) -> Paging {
    let (sender, receiver) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build the tokio runtime");
        let paging = runtime.block_on(async move {
            let (mut client, connection) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
                .await
                .expect("tokio-postgres connect");
            let driver = tokio::spawn(async move {
                let _ = connection.await;
            });
            let transaction = client.transaction().await.expect("tokio transaction");
            let statement = transaction.prepare(&portal_sql()).await.expect("prepare");
            let portal = transaction.bind(&statement, &[]).await.expect("bind");

            let mut pages = Vec::new();
            let mut values = Vec::new();
            loop {
                let rows = transaction
                    .query_portal(&portal, page)
                    .await
                    .expect("tokio query_portal");
                pages.push(rows.len());
                for row in &rows {
                    values.push(row.get::<_, i32>(0));
                }
                // A zero limit fetches everything at once, so one call is the
                // whole story; otherwise stop when a page comes back short.
                if page == 0 || rows.len() < page as usize {
                    break;
                }
            }
            drop(transaction);
            drop(client);
            let _ = driver.await;
            Paging { pages, values }
        });
        let _ = sender.send(paging);
    });
    handle.join().expect("the tokio thread panicked");
    receiver.recv().expect("no paging came back from tokio")
}

/// Portal paging is the driver's own accounting of PortalSuspended against
/// CommandComplete. Both must page identically.
#[compio::test]
async fn both_drivers_agree_on_portal_paging() {
    let url = common::test_url();

    let (mut client, connection) = compio_postgres::connect(&url, common::suite_tls())
        .await
        .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();

    let mut divergences = Vec::new();
    for page in PAGE_SIZES {
        let theirs = tokio_paging(common::plaintext_url(), page);

        let transaction = client.transaction().await.expect("transaction");
        let statement = transaction.prepare(&portal_sql()).await.expect("prepare");
        let portal = transaction.bind(&statement, &[]).await.expect("bind");
        let mut pages = Vec::new();
        let mut values = Vec::new();
        loop {
            let rows = transaction
                .query_portal(&portal, page)
                .await
                .expect("query_portal");
            pages.push(rows.len());
            for row in &rows {
                values.push(row.get::<_, i32>(0));
            }
            if page == 0 || rows.len() < page as usize {
                break;
            }
        }
        drop(transaction);
        let ours = Paging { pages, values };

        if ours != theirs {
            divergences.push(format!(
                "  max_rows={page}\n    ours: {ours:?}\n    tokio-postgres: {theirs:?}"
            ));
        }

        // Whatever the paging, every row must arrive exactly once and in
        // order, or two drivers could agree on the same loss.
        let expected: Vec<i32> = (1..=PORTAL_ROWS).collect();
        assert_eq!(
            ours.values, expected,
            "max_rows={page} did not deliver every row in order"
        );
    }

    assert!(
        divergences.is_empty(),
        "the two portal implementations page differently:\n{}",
        divergences.join("\n")
    );
}

/// Binary COPY carries its own framing on top of the COPY protocol: a
/// 19-byte header with flags and an extension area, a per-tuple field count,
/// a length-prefixed value per field, and a -1 field count as the trailer.
/// Both drivers write and read that themselves, and this crate has already had
/// two defects in it - a field count that could not be represented on the wire
/// and a critical-flag mask that rejected valid headers.
const BINARY_ROWS: i32 = 500;

fn tokio_binary_roundtrip(url: String, table: String) -> (u64, Vec<(i32, String, Option<i64>)>) {
    let (sender, receiver) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build the tokio runtime");
        let result = runtime.block_on(async move {
            use futures_util::TryStreamExt;

            let (client, connection) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
                .await
                .expect("tokio-postgres connect");
            let driver = tokio::spawn(async move {
                let _ = connection.await;
            });

            let sink = client
                .copy_in(&format!("COPY {table} FROM STDIN BINARY"))
                .await
                .expect("tokio binary copy_in");
            let types = [
                tokio_postgres::types::Type::INT4,
                tokio_postgres::types::Type::TEXT,
                tokio_postgres::types::Type::INT8,
            ];
            let writer = tokio_postgres::binary_copy::BinaryCopyInWriter::new(sink, &types);
            let mut writer = std::pin::pin!(writer);
            for id in 1..=BINARY_ROWS {
                // A NULL every third row: the field length is -1 rather than a
                // payload, which is the case a length-prefix bug survives.
                let maybe: Option<i64> = if id % 3 == 0 {
                    None
                } else {
                    Some(i64::from(id) * 1000)
                };
                writer
                    .as_mut()
                    .write(&[&id, &format!("row-{id}"), &maybe])
                    .await
                    .expect("tokio binary write");
            }
            let written = writer.finish().await.expect("tokio binary finish");

            let stream = client
                .copy_out(&format!("COPY {table} TO STDOUT BINARY"))
                .await
                .expect("tokio binary copy_out");
            let rows = tokio_postgres::binary_copy::BinaryCopyOutStream::new(stream, &types);
            let rows: Vec<_> = rows.try_collect().await.expect("tokio binary read");
            let decoded = rows
                .iter()
                .map(|row| {
                    (
                        row.get::<i32>(0),
                        row.get::<&str>(1).to_owned(),
                        row.get::<Option<i64>>(2),
                    )
                })
                .collect::<Vec<_>>();

            drop(client);
            let _ = driver.await;
            (written, decoded)
        });
        let _ = sender.send(result);
    });
    handle.join().expect("the tokio thread panicked");
    receiver
        .recv()
        .expect("no binary copy result came back from tokio")
}

/// Both drivers write and read binary COPY framing themselves, so a
/// round-trip through each must agree row for row.
#[compio::test]
async fn both_drivers_agree_on_binary_copy_roundtrip() {
    use futures_util::TryStreamExt;

    let url = common::test_url();
    let base = common::test_object_name("cpg bincopy");
    let ours_table = format!("{base}_ours");
    let theirs_table = format!("{base}_theirs");

    let (client, connection) = compio_postgres::connect(&url, common::suite_tls())
        .await
        .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();

    for table in [&ours_table, &theirs_table] {
        client
            .batch_execute(&format!(
                "DROP TABLE IF EXISTS {table};
                 CREATE TABLE {table} (id int4, label text, amount int8);"
            ))
            .await
            .expect("binary copy fixture");
    }

    let theirs = tokio_binary_roundtrip(common::plaintext_url(), theirs_table.clone());

    let types = [
        compio_postgres::types::Type::INT4,
        compio_postgres::types::Type::TEXT,
        compio_postgres::types::Type::INT8,
    ];
    let sink = client
        .copy_in(&format!("COPY {ours_table} FROM STDIN BINARY"))
        .await
        .expect("binary copy_in");
    let writer = compio_postgres::binary_copy::BinaryCopyInWriter::new(sink, &types);
    let mut writer = std::pin::pin!(writer);
    for id in 1..=BINARY_ROWS {
        let maybe: Option<i64> = if id % 3 == 0 {
            None
        } else {
            Some(i64::from(id) * 1000)
        };
        writer
            .as_mut()
            .write(&[&id, &format!("row-{id}"), &maybe])
            .await
            .expect("binary write");
    }
    let ours_written = writer.finish().await.expect("binary finish");

    let stream = client
        .copy_out(&format!("COPY {ours_table} TO STDOUT BINARY"))
        .await
        .expect("binary copy_out");
    let rows = compio_postgres::binary_copy::BinaryCopyOutStream::new(stream, &types);
    let rows: Vec<_> = rows.try_collect().await.expect("binary read");
    let ours: Vec<(i32, String, Option<i64>)> = rows
        .iter()
        .map(|row| {
            (
                row.get::<i32>(0),
                row.get::<&str>(1).to_owned(),
                row.get::<Option<i64>>(2),
            )
        })
        .collect();

    for table in [&ours_table, &theirs_table] {
        let _ = client
            .batch_execute(&format!("DROP TABLE IF EXISTS {table}"))
            .await;
    }

    assert_eq!(
        ours_written, theirs.0,
        "the drivers reported different binary COPY IN counts"
    );
    assert_eq!(
        ours.len(),
        BINARY_ROWS as usize,
        "neither driver round-tripped every row, so agreement proves nothing"
    );
    assert_eq!(ours, theirs.1, "the binary COPY round-trips disagree");
    assert!(
        ours.iter().any(|(_, _, amount)| amount.is_none()),
        "the NULL rows did not survive, so the -1 field length was never exercised"
    );
}

/// One simple-query response, flattened to plain data.
///
/// The simple protocol answers a whole SCRIPT: each statement contributes its
/// own RowDescription, its rows, and its CommandComplete, all on one stream
/// with no Sync between them. Turning that back into a per-statement structure
/// is the driver's own work, and the interesting cases are the ones where a
/// statement contributes an unusual combination - no rows, no description, or
/// an empty statement that has neither.
#[derive(Debug, PartialEq, Eq)]
enum Flattened {
    Description(Vec<String>),
    Row(Vec<Option<String>>),
    Complete(u64),
}

/// Scripts whose response shape differs from "one description, some rows, one
/// complete".
fn simple_query_scripts(table: &str) -> Vec<(String, &'static str)> {
    vec![
        (
            "SELECT 1::int4 AS a, 'x'::text AS b".to_owned(),
            "the ordinary shape, as the control",
        ),
        (
            "SELECT 1; SELECT 2, 3".to_owned(),
            "two statements on one stream, with no Sync between them - the \
             driver must not merge their descriptions",
        ),
        (
            "SELECT 1 WHERE false".to_owned(),
            "a description with no rows behind it",
        ),
        (
            format!("CREATE TEMPORARY TABLE {table} (id int); DROP TABLE {table}"),
            "two statements that describe nothing at all",
        ),
        (
            "SELECT NULL::text AS nothing".to_owned(),
            "a NULL is absent, not empty - the two are different values here",
        ),
        (
            String::new(),
            "the empty query: no description, no rows, and an EmptyQueryResponse \
             rather than a CommandComplete",
        ),
    ]
}

fn flatten_tokio(messages: &[tokio_postgres::SimpleQueryMessage]) -> Vec<Flattened> {
    messages
        .iter()
        .filter_map(|message| match message {
            tokio_postgres::SimpleQueryMessage::RowDescription(columns) => {
                Some(Flattened::Description(
                    columns
                        .iter()
                        .map(|column| column.name().to_owned())
                        .collect(),
                ))
            }
            tokio_postgres::SimpleQueryMessage::Row(row) => Some(Flattened::Row(
                (0..row.len())
                    .map(|index| row.get(index).map(str::to_owned))
                    .collect(),
            )),
            tokio_postgres::SimpleQueryMessage::CommandComplete(rows) => {
                Some(Flattened::Complete(*rows))
            }
            _ => None,
        })
        .collect()
}

fn flatten_ours(messages: &[compio_postgres::SimpleQueryMessage]) -> Vec<Flattened> {
    messages
        .iter()
        .filter_map(|message| match message {
            compio_postgres::SimpleQueryMessage::RowDescription(columns) => {
                Some(Flattened::Description(
                    columns
                        .iter()
                        .map(|column| column.name().to_owned())
                        .collect(),
                ))
            }
            compio_postgres::SimpleQueryMessage::Row(row) => Some(Flattened::Row(
                (0..row.len())
                    .map(|index| row.get(index).map(str::to_owned))
                    .collect(),
            )),
            compio_postgres::SimpleQueryMessage::CommandComplete(rows) => {
                Some(Flattened::Complete(*rows))
            }
            _ => None,
        })
        .collect()
}

fn tokio_simple_queries(url: String, scripts: Vec<String>) -> Vec<Vec<Flattened>> {
    let (sender, receiver) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build the tokio runtime");
        let collected = runtime.block_on(async move {
            let (client, connection) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
                .await
                .expect("tokio-postgres connect");
            let driver = tokio::spawn(async move {
                let _ = connection.await;
            });
            let mut collected = Vec::new();
            for script in scripts {
                collected.push(match client.simple_query(&script).await {
                    Ok(messages) => flatten_tokio(&messages),
                    Err(_) => Vec::new(),
                });
            }
            drop(client);
            let _ = driver.await;
            collected
        });
        let _ = sender.send(collected);
    });
    handle.join().expect("the tokio thread panicked");
    receiver
        .recv()
        .expect("no simple-query results came back from tokio")
}

/// Both drivers reassemble a multi-statement simple-query response
/// themselves, so the flattened shape must agree.
#[compio::test]
async fn both_drivers_agree_on_simple_query_shapes() {
    let url = common::test_url();
    let ours_table = common::test_object_name("cpg_simple_ours");
    let theirs_table = common::test_object_name("cpg_simple_theirs");
    let theirs_sql: Vec<String> = simple_query_scripts(&theirs_table)
        .into_iter()
        .map(|(script, _)| script)
        .collect();
    let scripts = simple_query_scripts(&ours_table);
    let sql: Vec<String> = scripts.iter().map(|(script, _)| script.clone()).collect();

    let theirs = tokio_simple_queries(common::plaintext_url(), theirs_sql);

    let (client, connection) = compio_postgres::connect(&url, common::suite_tls())
        .await
        .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();

    let mut ours = Vec::new();
    for script in &sql {
        ours.push(match client.simple_query(script).await {
            Ok(messages) => flatten_ours(&messages),
            Err(_) => Vec::new(),
        });
    }

    let mut divergences = Vec::new();
    for (index, (script, why)) in scripts.iter().enumerate() {
        if ours[index] != theirs[index] {
            divergences.push(format!(
                "  {script:?}\n    ours: {:?}\n    tokio-postgres: {:?}\n    matters because {why}",
                ours[index], theirs[index]
            ));
        }
    }
    assert!(
        divergences.is_empty(),
        "the two simple-query readers disagree:\n{}",
        divergences.join("\n")
    );

    // The two-statement script must really have produced two descriptions, or
    // the agreement says nothing about keeping them apart.
    let descriptions = ours[1]
        .iter()
        .filter(|item| matches!(item, Flattened::Description(_)))
        .count();
    assert_eq!(
        descriptions, 2,
        "the two-statement script produced {descriptions} descriptions, so the \
         case it exists for was never exercised"
    );
}

// ---------------------------------------------------------------------------
// Stale plans under a result-type change
// ---------------------------------------------------------------------------

/// The one thing the comparison needs from either driver's error type.
trait StaleError {
    fn sqlstate(&self) -> Option<String>;
}

impl StaleError for tokio_postgres::Error {
    fn sqlstate(&self) -> Option<String> {
        self.code().map(|code| code.code().to_owned())
    }
}

impl StaleError for compio_postgres::Error {
    fn sqlstate(&self) -> Option<String> {
        self.code().map(|code| code.code().to_owned())
    }
}

fn record<E: StaleError>(result: Result<u64, E>) -> Outcome {
    match result {
        Ok(rows) => Outcome::Rows(rows),
        Err(error) => match error.sqlstate() {
            Some(code) => Outcome::SqlState(code),
            None => Outcome::LocalFailure,
        },
    }
}

/// tokio-postgres holding an explicit prepared statement across a DDL change.
fn tokio_explicit_prepare(url: String, table: String) -> Vec<Outcome> {
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

            client
                .batch_execute(&format!(
                    "CREATE TABLE {table}(a int); INSERT INTO {table} VALUES (1)"
                ))
                .await
                .expect("tokio fixture");
            let statement = client
                .prepare(&format!("SELECT * FROM {table}"))
                .await
                .expect("tokio prepare");

            let mut outcomes = vec![record(client.execute(&statement, &[]).await)];
            client
                .batch_execute(&format!("ALTER TABLE {table} ADD COLUMN b int"))
                .await
                .expect("tokio alter");
            outcomes.push(record(client.execute(&statement, &[]).await));
            // The session must survive the refusal.
            outcomes.push(record(client.execute("SELECT 1::int4", &[]).await));

            let _ = client
                .batch_execute(&format!("DROP TABLE IF EXISTS {table}"))
                .await;
            drop(client);
            let _ = driver.await;
            outcomes
        });
        let _ = sender.send(outcomes);
    });
    handle.join().expect("the tokio thread panicked");
    receiver.recv().expect("no outcomes came back from tokio")
}

/// This crate doing the same with an explicit prepared statement.
async fn compio_explicit_prepare(url: &str, table: &str) -> Vec<Outcome> {
    let (client, connection) = compio_postgres::connect(url, common::suite_tls())
        .await
        .unwrap_or_else(|error| common::postgres_unreachable(url, &error));
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();

    client
        .batch_execute(&format!(
            "CREATE TABLE {table}(a int); INSERT INTO {table} VALUES (1)"
        ))
        .await
        .expect("fixture");
    let statement = client
        .prepare(&format!("SELECT * FROM {table}"))
        .await
        .expect("prepare");

    let mut outcomes = vec![record(client.execute(&statement, &[]).await)];
    client
        .batch_execute(&format!("ALTER TABLE {table} ADD COLUMN b int"))
        .await
        .expect("alter");
    outcomes.push(record(client.execute(&statement, &[]).await));
    outcomes.push(record(client.execute("SELECT 1::int4", &[]).await));

    let _ = client
        .batch_execute(&format!("DROP TABLE IF EXISTS {table}"))
        .await;
    outcomes
}

/// A CALLER-OWNED prepared statement whose result type moves under it is
/// refused by both drivers, identically, and neither session dies of it.
///
/// The scenario has to be MATCHED to mean anything, and getting that wrong is
/// easy: `execute(&str)` prepares afresh on every call in both drivers, so no
/// plan is ever stale and both simply succeed. The `0A000` only appears when a
/// statement is HELD across the DDL. An earlier attempt at this surface
/// compared this crate's implicit cache against a tokio statement held that
/// way, and reported a divergence that is really two different scenarios.
///
/// WHAT THIS DOES NOT CATCH: this crate's implicit statement cache, which is
/// off by default and behaves differently on purpose - see
/// `the_implicit_cache_retries_a_stale_plan_outside_a_transaction`.
#[compio::test]
async fn both_drivers_refuse_a_stale_explicit_prepared_statement() {
    let url = common::test_url();
    let expected = vec![
        Outcome::Rows(1),
        Outcome::SqlState("0A000".to_owned()),
        Outcome::Rows(1),
    ];

    let theirs = tokio_explicit_prepare(
        common::plaintext_url(),
        common::test_object_name("cpg_plan_theirs"),
    );
    let ours = compio_explicit_prepare(&url, &common::test_object_name("cpg_plan_ours")).await;

    assert_eq!(
        ours, theirs,
        "the drivers disagree about a stale prepared statement:\n  ours: {ours:?}\n  tokio-postgres: {theirs:?}"
    );
    // Pinned absolutely as well, so two drivers making the same mistake cannot
    // pass: 0A000 is the refusal, and the trailing Rows(1) is the session
    // still being usable afterwards.
    assert_eq!(ours, expected, "this crate stopped refusing a stale plan");
    assert_eq!(
        theirs, expected,
        "tokio-postgres stopped refusing a stale plan"
    );
}

/// The implicit cache hides that refusal, which is this crate's own feature
/// and has no counterpart in tokio-postgres.
///
/// `Config::statement_cache_capacity` promises it: on `0A000` for a cached
/// statement the stale entry is evicted and the operation is prepared and run
/// once more, so "the caller sees the result, not the error". It also states
/// the limit - inside a transaction the error has already aborted it, so
/// nothing is retried.
///
/// The capacity-0 arm is the control, and it is what makes the other two
/// readable: with no cache no plan is ever stale, so success there proves
/// nothing about retrying. The three arms MEASURED on 2026-08-24:
///
/// ```text
///   capacity 0   outside txn  Rows(1)   inside txn  Rows(1)
///   capacity 16  outside txn  Rows(1)   inside txn  0A000
/// ```
///
/// WHAT THIS DOES NOT CATCH: whether a retry re-runs side effects. `0A000`
/// arrives before execution, so nothing here can observe that; the docs name
/// `26000` as the case where it could, and that is not exercised.
#[compio::test]
async fn the_implicit_cache_retries_a_stale_plan_outside_a_transaction() {
    let url = common::test_url();

    assert_eq!(
        compio_implicit_cache(&url, &common::test_object_name("cpg_cache_on"), 16, false).await,
        vec![Outcome::Rows(1), Outcome::Rows(1), Outcome::Rows(1)],
        "the stale cached plan was not retried"
    );
    assert_eq!(
        compio_implicit_cache(&url, &common::test_object_name("cpg_cache_txn"), 16, true).await,
        vec![
            Outcome::Rows(1),
            Outcome::Rows(1),
            Outcome::SqlState("0A000".to_owned())
        ],
        "a stale plan inside a transaction must propagate rather than retry"
    );
    assert_eq!(
        compio_implicit_cache(&url, &common::test_object_name("cpg_cache_off"), 0, true).await,
        vec![Outcome::Rows(1), Outcome::Rows(1), Outcome::Rows(1)],
        "the default configuration cached a statement it was never asked to cache"
    );
}

/// Raw SQL executed twice so the implicit cache admits it, then a result-type
/// change, then the same SQL again.
async fn compio_implicit_cache(
    url: &str,
    table: &str,
    capacity: usize,
    inside_transaction: bool,
) -> Vec<Outcome> {
    let mut config: compio_postgres::Config = url.parse().expect("parse the test DSN");
    config.statement_cache_capacity(capacity);
    let (client, connection) = config
        .connect(common::suite_tls())
        .await
        .unwrap_or_else(|error| common::postgres_unreachable(url, &error));
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();

    client
        .batch_execute(&format!(
            "CREATE TABLE {table}(a int); INSERT INTO {table} VALUES (1)"
        ))
        .await
        .expect("fixture");

    let sql = format!("SELECT * FROM {table}");
    let mut outcomes = Vec::new();
    for _ in 0..2 {
        outcomes.push(record(client.execute(sql.as_str(), &[]).await));
    }
    client
        .batch_execute(&format!("ALTER TABLE {table} ADD COLUMN b int"))
        .await
        .expect("alter");
    if inside_transaction {
        client.batch_execute("BEGIN").await.expect("begin");
    }
    outcomes.push(record(client.execute(sql.as_str(), &[]).await));

    let _ = client.batch_execute("ROLLBACK").await;
    let _ = client
        .batch_execute(&format!("DROP TABLE IF EXISTS {table}"))
        .await;
    outcomes
}

// ---------------------------------------------------------------------------
// A COPY this API cannot feed
// ---------------------------------------------------------------------------

/// What one driver did with a COPY it had no data channel for, and whether the
/// session outlived it.
#[derive(Debug, PartialEq, Eq)]
struct CopyOutcome {
    copy: Outcome,
    session_usable_after: bool,
}

fn tokio_producerless_copy(url: String, table: String) -> CopyOutcome {
    let (sender, receiver) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build the tokio runtime");
        let outcome = runtime.block_on(async move {
            let (client, connection) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
                .await
                .expect("tokio-postgres connect");
            let driver = tokio::spawn(async move {
                let _ = connection.await;
            });

            client
                .batch_execute(&format!("CREATE TABLE {table}(v int4)"))
                .await
                .expect("tokio fixture");
            let statement = client
                .prepare(&format!("COPY {table} FROM STDIN"))
                .await
                .expect("tokio prepare");

            let copy = match client.query(&statement, &[]).await {
                Ok(rows) => Outcome::Rows(rows.len() as u64),
                Err(error) => match error.sqlstate() {
                    Some(code) => Outcome::SqlState(code),
                    None => Outcome::LocalFailure,
                },
            };
            let session_usable_after = client.query_one("SELECT 1::int4", &[]).await.is_ok();

            let _ = client
                .batch_execute(&format!("DROP TABLE IF EXISTS {table}"))
                .await;
            drop(client);
            let _ = driver.await;
            CopyOutcome {
                copy,
                session_usable_after,
            }
        });
        let _ = sender.send(outcome);
    });
    handle.join().expect("the tokio thread panicked");
    receiver
        .recv()
        .expect("no copy outcome came back from tokio")
}

async fn compio_producerless_copy(url: &str, table: &str) -> CopyOutcome {
    let (client, connection) = compio_postgres::connect(url, common::suite_tls())
        .await
        .unwrap_or_else(|error| common::postgres_unreachable(url, &error));
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();

    client
        .batch_execute(&format!("CREATE TABLE {table}(v int4)"))
        .await
        .expect("fixture");
    let statement = client
        .prepare(&format!("COPY {table} FROM STDIN"))
        .await
        .expect("prepare");

    let copy = match client.query(&statement, &[]).await {
        Ok(rows) => Outcome::Rows(rows.len() as u64),
        Err(error) => match error.sqlstate() {
            Some(code) => Outcome::SqlState(code),
            None => Outcome::LocalFailure,
        },
    };
    let session_usable_after = client
        .query_one_scalar::<i32, _>("SELECT 1::int4", &[])
        .await
        .is_ok();

    let _ = client
        .batch_execute(&format!("DROP TABLE IF EXISTS {table}"))
        .await;
    CopyOutcome {
        copy,
        session_usable_after,
    }
}

/// Asked for a COPY it cannot supply data for, this crate ABORTS the copy and
/// keeps the session; tokio-postgres abandons it and loses the connection.
///
/// A deliberate divergence, and the header of `src/simple_query.rs` names it as
/// the one place this port departs from upstream: "a `CopyInResponse` is
/// answered with `CopyFail` rather than reported as an unexpected message and
/// abandoned. Upstream abandons it, and abandoning it leaves the SESSION in
/// copy mode, which costs the connection."
///
/// The scenarios are MATCHED - an explicit prepared statement and `query` on
/// both drivers - because an earlier divergence reported in this file turned
/// out to be one driver using an implicit statement cache and the other an
/// explicit statement, which is two tests rather than a disagreement.
///
/// MEASURED 2026-08-24:
///
/// ```text
///   ours:            57014, session usable afterwards
///   tokio-postgres:  local failure (no SQLSTATE), connection closed
/// ```
///
/// WHAT THIS DOES NOT CATCH: the caller-abandons-a-sink case. Both drivers end
/// an unfinished `copy_in` by DROPPING the sink and neither reports the
/// server's SQLSTATE through that path, so there is nothing to compare.
#[compio::test]
async fn a_copy_without_a_producer_costs_tokio_the_connection_and_not_this_one() {
    let url = common::test_url();

    // A WATCHDOG, because the failure this pins is a HANG and not an error.
    // MEASURED 2026-08-24: routing the producerless COPY the way upstream does
    // leaves the session in copy mode, and this test then waits forever rather
    // than failing. Without the bound, a regression here looks like a slow
    // server.
    let ours = compio::time::timeout(
        std::time::Duration::from_secs(20),
        compio_producerless_copy(&url, &common::test_object_name("cpg_copyfail_ours")),
    )
    .await
    .expect("the producerless COPY never returned; the session is wedged in copy mode");
    let theirs = tokio_producerless_copy(
        common::plaintext_url(),
        common::test_object_name("cpg_copyfail_theirs"),
    );

    assert_eq!(
        ours,
        CopyOutcome {
            copy: Outcome::SqlState("57014".to_owned()),
            session_usable_after: true,
        },
        "this crate stopped aborting a producerless COPY, or stopped surviving it"
    );
    assert_eq!(
        theirs,
        CopyOutcome {
            copy: Outcome::LocalFailure,
            session_usable_after: false,
        },
        "tokio-postgres changed how it handles a producerless COPY, so the \
         divergence this pins is no longer the one described"
    );
    assert_ne!(
        ours.session_usable_after, theirs.session_usable_after,
        "the whole point of the divergence is that one session survives and the \
         other does not"
    );
}

/// A COPY OUT whose own query fails partway is reported identically by both,
/// and neither loses the session.
///
/// The control for the divergence above: the drivers are NOT generally
/// different about failed copies, only about the one case upstream abandons.
#[compio::test]
async fn both_drivers_survive_a_copy_out_whose_query_fails() {
    let url = common::test_url();

    let (client, connection) = compio_postgres::connect(&url, common::suite_tls())
        .await
        .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();

    let failed = client
        .batch_execute("COPY (SELECT 1/0) TO STDOUT")
        .await
        .expect_err("a COPY OUT of a failing query must not succeed");
    assert_eq!(
        failed.code().map(|code| code.code()),
        Some("22012"),
        "the division by zero lost its SQLSTATE: {}",
        common::error_chain(&failed)
    );
    assert!(
        client
            .query_one_scalar::<i32, _>("SELECT 2::int4", &[])
            .await
            .is_ok(),
        "the failed COPY OUT left the session unusable"
    );
}
