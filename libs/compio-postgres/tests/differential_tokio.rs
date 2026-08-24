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

    let (client, mut connection) = compio_postgres::connect(url, NoTls)
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
    let theirs = tokio_notices(url.clone());
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

    let (client, connection) = compio_postgres::connect(&url, NoTls)
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

    let theirs = tokio_described(url.clone(), statements.clone());

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

    let (client, connection) = compio_postgres::connect(&url, NoTls)
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
    let theirs = tokio_copy_out(url.clone(), sql.clone());

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

    let (client, connection) = compio_postgres::connect(&url, NoTls)
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
    let theirs_count = tokio_copy_in(url.clone(), theirs_table.clone(), body.clone());

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

    let (mut client, connection) = compio_postgres::connect(&url, NoTls)
        .await
        .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();

    let mut divergences = Vec::new();
    for page in PAGE_SIZES {
        let theirs = tokio_paging(url.clone(), page);

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
