//! Differential COPY-subprotocol tests against `tokio-postgres` 0.7.18.
//!
//! Both drivers use the same `PostgreSQL` server, tables, statements, session
//! settings, and protocol version. COPY OUT comparisons concatenate chunks
//! because chunk boundaries are chosen independently of the byte contract.
//! Server table state is read through the fixture connection after each driver
//! runs, rather than trusting either COPY implementation to describe its own
//! writes.
//!
//! The corpus guard at the bottom pins nine cases, their kind/format census,
//! dense nonempty witnesses, and the exact divergence set. Compio-postgres
//! deliberately waits through `ReadyForQuery` before reporting COPY success;
//! tokio-postgres reports the preceding provisional `CommandComplete` count.
//! The deferred-constraint case pins that documented correctness extension.
//!
//! Dropping an unfinished COPY IN sink makes both drivers send
//! `CopyFail("") + Sync`, but both public APIs also discard the corresponding
//! server `ErrorResponse`. Its SQLSTATE and message therefore cannot be compared
//! here. The observable contract is still pinned: the partial row is rolled
//! back and the same session answers its next query.

#![allow(clippy::future_not_send)]

use std::collections::BTreeSet;
use std::future::Future;
use std::time::Duration;

use bytes::{BufMut as _, Bytes, BytesMut};
use compio_postgres::config::ProtocolVersion;
use futures_util::{SinkExt as _, StreamExt as _};

#[allow(unused_imports)]
use crate::common;

const SESSION_SQL: &str = "SET client_encoding = 'UTF8';
    SET DateStyle = 'ISO, YMD';
    SET IntervalStyle = 'postgres';
    SET standard_conforming_strings = on";
const TEXT_COPY_INPUT: &[&[u8]] = &[
    b"1\talpha\t\\",
    b"N\n2\ttab\\tline\\n",
    b"slash\\\\end\t20\n3\t\\N\t-30\n",
];
const CONSTRAINT_ERROR_INPUT: &[&[u8]] = &[b"1\n", b"1\n", b"2\n"];
const DEFERRED_CONSTRAINT_INPUT: &[&[u8]] = &[b"314", b"159\n"];
const MALFORMED_BINARY_INPUT: &[&[u8]] = &[
    b"PGCOPY\n\xff\r\n\0\0\0\0\0\0\0\0\0",
    b"\0\x01\0\0\0\x04\0\0\0\x01",
    b"\0\x01\xff\xff\xff\xfe",
    b"\xff\xff",
];
const WIDE_COPY_OUT: &str =
    "COPY (SELECT g, repeat('x', 500) FROM generate_series(1, 5000) g) TO STDOUT";
const WIDE_COPY_ROWS: usize = 5_000;
const ABANDON_CHUNKS: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CaseKind {
    CopyOut,
    CopyInSuccess,
    CopyInError,
    Abandonment,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CopyEncoding {
    Text,
    Binary,
}

#[derive(Clone, Copy, Debug)]
struct Case {
    name: &'static str,
    kind: CaseKind,
    encoding: CopyEncoding,
}

const CASES: [Case; 9] = [
    Case {
        name: "COPY OUT text",
        kind: CaseKind::CopyOut,
        encoding: CopyEncoding::Text,
    },
    Case {
        name: "COPY OUT binary",
        kind: CaseKind::CopyOut,
        encoding: CopyEncoding::Binary,
    },
    Case {
        name: "COPY IN text",
        kind: CaseKind::CopyInSuccess,
        encoding: CopyEncoding::Text,
    },
    Case {
        name: "COPY IN binary",
        kind: CaseKind::CopyInSuccess,
        encoding: CopyEncoding::Binary,
    },
    Case {
        name: "COPY IN constraint error",
        kind: CaseKind::CopyInError,
        encoding: CopyEncoding::Text,
    },
    Case {
        name: "COPY IN deferred constraint",
        kind: CaseKind::CopyInError,
        encoding: CopyEncoding::Text,
    },
    Case {
        name: "COPY IN malformed binary",
        kind: CaseKind::CopyInError,
        encoding: CopyEncoding::Binary,
    },
    Case {
        name: "COPY IN dropped sink",
        kind: CaseKind::Abandonment,
        encoding: CopyEncoding::Text,
    },
    Case {
        name: "COPY OUT abandoned stream",
        kind: CaseKind::Abandonment,
        encoding: CopyEncoding::Text,
    },
];

#[derive(Clone, Debug)]
struct Tables {
    output: String,
    text_input: String,
    binary_input: String,
    constraint_error: String,
    deferred_constraint: String,
    malformed_binary: String,
    cancelled_input: String,
}

impl Tables {
    fn unique() -> Self {
        Self {
            output: common::test_object_name("cpg_diff_copy_output"),
            text_input: common::test_object_name("cpg_diff_copy_text_input"),
            binary_input: common::test_object_name("cpg_diff_copy_binary_input"),
            constraint_error: common::test_object_name("cpg_diff_copy_constraint"),
            deferred_constraint: common::test_object_name("cpg_diff_copy_deferred"),
            malformed_binary: common::test_object_name("cpg_diff_copy_malformed"),
            cancelled_input: common::test_object_name("cpg_diff_copy_cancelled"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct StoredRow {
    id: i32,
    payload: Option<String>,
    amount: Option<i64>,
}

fn expected_input_rows() -> Vec<StoredRow> {
    vec![
        StoredRow {
            id: 1,
            payload: Some("alpha".to_owned()),
            amount: None,
        },
        StoredRow {
            id: 2,
            payload: Some("tab\tline\nslash\\end".to_owned()),
            amount: Some(20),
        },
        StoredRow {
            id: 3,
            payload: None,
            amount: Some(-30),
        },
    ]
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FailurePhase {
    Write(usize),
    Finish,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum CopyCompletion {
    Rows(u64),
    Error {
        phase: FailurePhase,
        sqlstate: Option<String>,
    },
}

impl CopyCompletion {
    const fn classification(&self) -> &'static str {
        match self {
            Self::Rows(_) => "rows",
            Self::Error { .. } => "error",
        }
    }

    const fn rows(&self) -> Option<u64> {
        match self {
            Self::Rows(rows) => Some(*rows),
            Self::Error { .. } => None,
        }
    }

    const fn phase(&self) -> Option<FailurePhase> {
        match self {
            Self::Rows(_) => None,
            Self::Error { phase, .. } => Some(*phase),
        }
    }

    fn sqlstate(&self) -> Option<&str> {
        match self {
            Self::Rows(_) => None,
            Self::Error { sqlstate, .. } => sqlstate.as_deref(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ProbeOutcome {
    Value(i32),
    Error(Option<String>),
}

#[derive(Clone, Debug)]
struct CopyOutObservation {
    chunks: usize,
    bytes: Vec<u8>,
}

#[derive(Clone, Debug)]
struct SuccessfulCopyInObservation {
    completion: CopyCompletion,
    rows: Vec<StoredRow>,
}

#[derive(Clone, Debug)]
struct FailedCopyInObservation {
    completion: CopyCompletion,
    writes_attempted: usize,
    writes_accepted: usize,
    table_rows: i64,
    reuse: ProbeOutcome,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum WriteOutcome {
    Accepted,
    Error(Option<String>),
}

#[derive(Clone, Debug)]
struct CancelledCopyInObservation {
    write: WriteOutcome,
    table_rows: i64,
    reuse: ProbeOutcome,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum EarlyStreamEnd {
    Clean,
    Error(Option<String>),
}

#[derive(Clone, Debug)]
struct AbandonedCopyOutObservation {
    chunks_read: usize,
    bytes_read: usize,
    rows_read: usize,
    early_end: Option<EarlyStreamEnd>,
    reuse: ProbeOutcome,
}

#[derive(Clone, Debug)]
struct DriverObservations {
    text_out: CopyOutObservation,
    binary_out: CopyOutObservation,
    text_in: SuccessfulCopyInObservation,
    binary_in: SuccessfulCopyInObservation,
    constraint_error: FailedCopyInObservation,
    deferred_constraint: FailedCopyInObservation,
    malformed_binary: FailedCopyInObservation,
    cancelled_input: CancelledCopyInObservation,
    abandoned_output: AbandonedCopyOutObservation,
}

#[derive(Debug)]
struct Difference {
    case: &'static str,
    aspect: &'static str,
    ours: String,
    theirs: String,
}

fn fixture_sql(tables: &Tables) -> String {
    format!(
        "CREATE TABLE {output} (id int4 PRIMARY KEY, payload text, note text);
         INSERT INTO {output} VALUES
            (1, 'plain', NULL),
            (2, 'tab' || chr(9) || 'line' || chr(10) || 'slash' || chr(92) || 'end', 'x'),
            (3, NULL, 'tail');
         CREATE TABLE {text_input} (id int4, payload text, amount int8);
         CREATE TABLE {binary_input} (id int4, payload text, amount int8);
         CREATE TABLE {constraint_error} (id int4 PRIMARY KEY);
         CREATE TABLE {deferred_constraint} (
             parent_id int4 REFERENCES {output} (id)
                 DEFERRABLE INITIALLY DEFERRED
         );
         CREATE TABLE {malformed_binary} (id int4);
         CREATE TABLE {cancelled_input} (id int4, payload text)",
        output = tables.output,
        text_input = tables.text_input,
        binary_input = tables.binary_input,
        constraint_error = tables.constraint_error,
        deferred_constraint = tables.deferred_constraint,
        malformed_binary = tables.malformed_binary,
        cancelled_input = tables.cancelled_input,
    )
}

fn cleanup_sql(tables: &Tables) -> String {
    format!(
        "DROP TABLE {deferred_constraint}, {output}, {text_input}, {binary_input}, \
         {constraint_error}, {malformed_binary}, {cancelled_input}",
        deferred_constraint = tables.deferred_constraint,
        output = tables.output,
        text_input = tables.text_input,
        binary_input = tables.binary_input,
        constraint_error = tables.constraint_error,
        malformed_binary = tables.malformed_binary,
        cancelled_input = tables.cancelled_input,
    )
}

fn reset_mutable_tables_sql(tables: &Tables) -> String {
    format!(
        "TRUNCATE {text_input}, {binary_input}, {constraint_error}, \
         {deferred_constraint}, {malformed_binary}, {cancelled_input}",
        text_input = tables.text_input,
        binary_input = tables.binary_input,
        constraint_error = tables.constraint_error,
        deferred_constraint = tables.deferred_constraint,
        malformed_binary = tables.malformed_binary,
        cancelled_input = tables.cancelled_input,
    )
}

fn text_copy_out_sql(tables: &Tables) -> String {
    format!(
        "COPY (SELECT id, payload, note FROM {} ORDER BY id) TO STDOUT",
        tables.output
    )
}

fn binary_copy_out_sql(tables: &Tables) -> String {
    format!(
        "COPY (SELECT id, payload, note FROM {} ORDER BY id) \
         TO STDOUT (FORMAT binary)",
        tables.output
    )
}

fn text_copy_in_sql(tables: &Tables) -> String {
    format!(
        "COPY {} (id, payload, amount) FROM STDIN",
        tables.text_input
    )
}

fn binary_copy_in_sql(tables: &Tables) -> String {
    format!(
        "COPY {} (id, payload, amount) FROM STDIN (FORMAT binary)",
        tables.binary_input
    )
}

fn constraint_copy_in_sql(tables: &Tables) -> String {
    format!("COPY {} (id) FROM STDIN", tables.constraint_error)
}

fn deferred_constraint_copy_in_sql(tables: &Tables) -> String {
    format!("COPY {} (parent_id) FROM STDIN", tables.deferred_constraint)
}

fn malformed_binary_copy_in_sql(tables: &Tables) -> String {
    format!(
        "COPY {} (id) FROM STDIN (FORMAT binary)",
        tables.malformed_binary
    )
}

fn cancelled_copy_in_sql(tables: &Tables) -> String {
    format!("COPY {} (id, payload) FROM STDIN", tables.cancelled_input)
}

fn on_tokio<T, F, Fut>(url: String, run: F) -> T
where
    T: Send + 'static,
    F: FnOnce(tokio_postgres::Client) -> Fut + Send + 'static,
    Fut: Future<Output = T> + 'static,
{
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let handle = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build the tokio runtime");
        let result = runtime.block_on(async move {
            let (client, connection) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
                .await
                .expect("tokio-postgres connect");
            let driver = tokio::spawn(async move {
                let _ = connection.await;
            });
            let result = run(client).await;
            let _ = driver.await;
            result
        });
        let _ = sender.send(result);
    });

    match receiver.recv_timeout(Duration::from_secs(30)) {
        Ok(result) => {
            handle.join().expect("the tokio thread panicked");
            result
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            handle.join().expect("the tokio thread panicked");
            unreachable!("the tokio thread exited without its observation")
        }
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            panic!("the tokio COPY oracle exceeded its 30-second watchdog")
        }
    }
}

async fn compio_client() -> compio_postgres::Client {
    let url = common::test_url();
    let mut config: compio_postgres::Config = url.parse().expect("the test DSN parses");
    config.max_protocol_version(ProtocolVersion::V3_0);
    let (client, connection) = config
        .connect(common::suite_tls())
        .await
        .unwrap_or_else(|error| common::postgres_unreachable(&url, &error));
    compio::runtime::spawn(async move {
        let _ = connection.run().await;
    })
    .detach();
    assert_eq!(
        client.protocol_version(),
        ProtocolVersion::V3_0,
        "the differential subject did not use tokio-postgres's protocol version"
    );
    client
}

async fn tokio_copy_out(client: &tokio_postgres::Client, statement: &str) -> CopyOutObservation {
    let stream = client
        .copy_out(statement)
        .await
        .unwrap_or_else(|error| panic!("tokio COPY OUT start: {error}"));
    futures_util::pin_mut!(stream);
    let mut chunks = 0;
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.unwrap_or_else(|error| panic!("tokio COPY OUT data: {error}"));
        chunks += 1;
        bytes.extend_from_slice(&chunk);
    }
    CopyOutObservation { chunks, bytes }
}

async fn compio_copy_out(client: &compio_postgres::Client, statement: &str) -> CopyOutObservation {
    let stream = client
        .copy_out(statement)
        .await
        .unwrap_or_else(|error| panic!("compio COPY OUT start: {error}"));
    futures_util::pin_mut!(stream);
    let mut chunks = 0;
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.unwrap_or_else(|error| panic!("compio COPY OUT data: {error}"));
        chunks += 1;
        bytes.extend_from_slice(&chunk);
    }
    CopyOutObservation { chunks, bytes }
}

async fn tokio_text_copy_in(
    client: &tokio_postgres::Client,
    tables: &Tables,
) -> SuccessfulCopyInObservation {
    client
        .batch_execute(&format!("TRUNCATE {}", tables.text_input))
        .await
        .expect("tokio truncate text COPY table");
    let sink = client
        .copy_in(&text_copy_in_sql(tables))
        .await
        .expect("tokio text COPY IN start");
    futures_util::pin_mut!(sink);
    for (index, chunk) in TEXT_COPY_INPUT.iter().enumerate() {
        if let Err(error) = sink.as_mut().send(Bytes::copy_from_slice(chunk)).await {
            return SuccessfulCopyInObservation {
                completion: CopyCompletion::Error {
                    phase: FailurePhase::Write(index),
                    sqlstate: error.code().map(|code| code.code().to_owned()),
                },
                rows: Vec::new(),
            };
        }
    }
    let completion = match sink.as_mut().finish().await {
        Ok(rows) => CopyCompletion::Rows(rows),
        Err(error) => CopyCompletion::Error {
            phase: FailurePhase::Finish,
            sqlstate: error.code().map(|code| code.code().to_owned()),
        },
    };
    SuccessfulCopyInObservation {
        completion,
        rows: Vec::new(),
    }
}

async fn compio_text_copy_in(
    client: &compio_postgres::Client,
    tables: &Tables,
) -> SuccessfulCopyInObservation {
    client
        .batch_execute(&format!("TRUNCATE {}", tables.text_input))
        .await
        .expect("compio truncate text COPY table");
    let sink = client
        .copy_in::<_, Bytes>(&text_copy_in_sql(tables))
        .await
        .expect("compio text COPY IN start");
    futures_util::pin_mut!(sink);
    for (index, chunk) in TEXT_COPY_INPUT.iter().enumerate() {
        if let Err(error) = sink.as_mut().send(Bytes::copy_from_slice(chunk)).await {
            return SuccessfulCopyInObservation {
                completion: CopyCompletion::Error {
                    phase: FailurePhase::Write(index),
                    sqlstate: error.code().map(|code| code.code().to_owned()),
                },
                rows: Vec::new(),
            };
        }
    }
    let completion = match sink.as_mut().finish().await {
        Ok(rows) => CopyCompletion::Rows(rows),
        Err(error) => CopyCompletion::Error {
            phase: FailurePhase::Finish,
            sqlstate: error.code().map(|code| code.code().to_owned()),
        },
    };
    SuccessfulCopyInObservation {
        completion,
        rows: Vec::new(),
    }
}

async fn tokio_binary_copy_in(
    client: &tokio_postgres::Client,
    tables: &Tables,
) -> SuccessfulCopyInObservation {
    use tokio_postgres::binary_copy::BinaryCopyInWriter;
    use tokio_postgres::types::Type;

    client
        .batch_execute(&format!("TRUNCATE {}", tables.binary_input))
        .await
        .expect("tokio truncate binary COPY table");
    let sink = client
        .copy_in(&binary_copy_in_sql(tables))
        .await
        .expect("tokio binary COPY IN start");
    let writer = BinaryCopyInWriter::new(sink, &[Type::INT4, Type::TEXT, Type::INT8]);
    futures_util::pin_mut!(writer);
    for (index, row) in expected_input_rows().iter().enumerate() {
        let payload = row.payload.as_deref();
        if let Err(error) = writer
            .as_mut()
            .write(&[&row.id, &payload, &row.amount])
            .await
        {
            return SuccessfulCopyInObservation {
                completion: CopyCompletion::Error {
                    phase: FailurePhase::Write(index),
                    sqlstate: error.code().map(|code| code.code().to_owned()),
                },
                rows: Vec::new(),
            };
        }
    }
    let completion = match writer.as_mut().finish().await {
        Ok(rows) => CopyCompletion::Rows(rows),
        Err(error) => CopyCompletion::Error {
            phase: FailurePhase::Finish,
            sqlstate: error.code().map(|code| code.code().to_owned()),
        },
    };
    SuccessfulCopyInObservation {
        completion,
        rows: Vec::new(),
    }
}

async fn compio_binary_copy_in(
    client: &compio_postgres::Client,
    tables: &Tables,
) -> SuccessfulCopyInObservation {
    use compio_postgres::binary_copy::BinaryCopyInWriter;
    use compio_postgres::types::Type;

    client
        .batch_execute(&format!("TRUNCATE {}", tables.binary_input))
        .await
        .expect("compio truncate binary COPY table");
    let sink = client
        .copy_in::<_, Bytes>(&binary_copy_in_sql(tables))
        .await
        .expect("compio binary COPY IN start");
    let writer = BinaryCopyInWriter::new(sink, &[Type::INT4, Type::TEXT, Type::INT8]);
    futures_util::pin_mut!(writer);
    for (index, row) in expected_input_rows().iter().enumerate() {
        let payload = row.payload.as_deref();
        if let Err(error) = writer
            .as_mut()
            .write(&[&row.id, &payload, &row.amount])
            .await
        {
            return SuccessfulCopyInObservation {
                completion: CopyCompletion::Error {
                    phase: FailurePhase::Write(index),
                    sqlstate: error.code().map(|code| code.code().to_owned()),
                },
                rows: Vec::new(),
            };
        }
    }
    let completion = match writer.as_mut().finish().await {
        Ok(rows) => CopyCompletion::Rows(rows),
        Err(error) => CopyCompletion::Error {
            phase: FailurePhase::Finish,
            sqlstate: error.code().map(|code| code.code().to_owned()),
        },
    };
    SuccessfulCopyInObservation {
        completion,
        rows: Vec::new(),
    }
}

fn tokio_error_completion(error: &tokio_postgres::Error, phase: FailurePhase) -> CopyCompletion {
    CopyCompletion::Error {
        phase,
        sqlstate: error.code().map(|code| code.code().to_owned()),
    }
}

fn compio_error_completion(error: &compio_postgres::Error, phase: FailurePhase) -> CopyCompletion {
    CopyCompletion::Error {
        phase,
        sqlstate: error.code().map(|code| code.code().to_owned()),
    }
}

async fn tokio_failed_copy_in(
    client: &tokio_postgres::Client,
    table: &str,
    statement: &str,
    chunks: &'static [&'static [u8]],
    sentinel: i32,
) -> FailedCopyInObservation {
    client
        .batch_execute(&format!("TRUNCATE {table}"))
        .await
        .expect("tokio truncate failing COPY table");
    let mut sink = Box::pin(
        client
            .copy_in(statement)
            .await
            .expect("tokio failing COPY IN start"),
    );
    let mut completion = None;
    let mut writes_attempted = 0;
    let mut writes_accepted = 0;
    for (index, chunk) in chunks.iter().enumerate() {
        writes_attempted += 1;
        if let Err(error) = sink.as_mut().send(Bytes::from_static(chunk)).await {
            completion = Some(tokio_error_completion(&error, FailurePhase::Write(index)));
            break;
        }
        writes_accepted += 1;
    }
    let completion = if let Some(completion) = completion {
        completion
    } else {
        match sink.as_mut().finish().await {
            Ok(rows) => CopyCompletion::Rows(rows),
            Err(error) => tokio_error_completion(&error, FailurePhase::Finish),
        }
    };
    drop(sink);
    let reuse = tokio_probe(client, sentinel).await;
    FailedCopyInObservation {
        completion,
        writes_attempted,
        writes_accepted,
        table_rows: -1,
        reuse,
    }
}

async fn compio_failed_copy_in(
    client: &compio_postgres::Client,
    table: &str,
    statement: &str,
    chunks: &'static [&'static [u8]],
    sentinel: i32,
) -> FailedCopyInObservation {
    client
        .batch_execute(&format!("TRUNCATE {table}"))
        .await
        .expect("compio truncate failing COPY table");
    let mut sink = Box::pin(
        client
            .copy_in::<_, Bytes>(statement)
            .await
            .expect("compio failing COPY IN start"),
    );
    let mut completion = None;
    let mut writes_attempted = 0;
    let mut writes_accepted = 0;
    for (index, chunk) in chunks.iter().enumerate() {
        writes_attempted += 1;
        if let Err(error) = sink.as_mut().send(Bytes::from_static(chunk)).await {
            completion = Some(compio_error_completion(&error, FailurePhase::Write(index)));
            break;
        }
        writes_accepted += 1;
    }
    let completion = if let Some(completion) = completion {
        completion
    } else {
        match sink.as_mut().finish().await {
            Ok(rows) => CopyCompletion::Rows(rows),
            Err(error) => compio_error_completion(&error, FailurePhase::Finish),
        }
    };
    drop(sink);
    let reuse = compio_probe(client, sentinel).await;
    FailedCopyInObservation {
        completion,
        writes_attempted,
        writes_accepted,
        table_rows: -1,
        reuse,
    }
}

async fn tokio_probe(client: &tokio_postgres::Client, sentinel: i32) -> ProbeOutcome {
    match client.query_one("SELECT $1::int4", &[&sentinel]).await {
        Ok(row) => ProbeOutcome::Value(row.get(0)),
        Err(error) => ProbeOutcome::Error(error.code().map(|code| code.code().to_owned())),
    }
}

async fn compio_probe(client: &compio_postgres::Client, sentinel: i32) -> ProbeOutcome {
    match client
        .query_one_scalar::<i32, _>("SELECT $1::int4", &[&sentinel])
        .await
    {
        Ok(value) => ProbeOutcome::Value(value),
        Err(error) => ProbeOutcome::Error(error.code().map(|code| code.code().to_owned())),
    }
}

async fn tokio_cancelled_copy_in(
    client: &tokio_postgres::Client,
    tables: &Tables,
) -> CancelledCopyInObservation {
    client
        .batch_execute(&format!("TRUNCATE {}", tables.cancelled_input))
        .await
        .expect("tokio truncate cancelled COPY table");
    let write = {
        let sink = client
            .copy_in(&cancelled_copy_in_sql(tables))
            .await
            .expect("tokio cancelled COPY IN start");
        futures_util::pin_mut!(sink);
        match sink
            .as_mut()
            .send(Bytes::from_static(b"7\tpartial-row\n"))
            .await
        {
            Ok(()) => WriteOutcome::Accepted,
            Err(error) => WriteOutcome::Error(error.code().map(|code| code.code().to_owned())),
        }
    };
    CancelledCopyInObservation {
        write,
        table_rows: -1,
        reuse: tokio_probe(client, 43_003).await,
    }
}

async fn compio_cancelled_copy_in(
    client: &compio_postgres::Client,
    tables: &Tables,
) -> CancelledCopyInObservation {
    client
        .batch_execute(&format!("TRUNCATE {}", tables.cancelled_input))
        .await
        .expect("compio truncate cancelled COPY table");
    let write = {
        let sink = client
            .copy_in::<_, Bytes>(&cancelled_copy_in_sql(tables))
            .await
            .expect("compio cancelled COPY IN start");
        futures_util::pin_mut!(sink);
        match sink
            .as_mut()
            .send(Bytes::from_static(b"7\tpartial-row\n"))
            .await
        {
            Ok(()) => WriteOutcome::Accepted,
            Err(error) => WriteOutcome::Error(error.code().map(|code| code.code().to_owned())),
        }
    };
    CancelledCopyInObservation {
        write,
        table_rows: -1,
        reuse: compio_probe(client, 43_003).await,
    }
}

#[allow(clippy::naive_bytecount)]
fn newline_count(bytes: &[u8]) -> usize {
    bytes.iter().filter(|byte| **byte == b'\n').count()
}

async fn tokio_abandoned_copy_out(client: &tokio_postgres::Client) -> AbandonedCopyOutObservation {
    let mut stream = Box::pin(
        client
            .copy_out(WIDE_COPY_OUT)
            .await
            .expect("tokio abandoned COPY OUT start"),
    );
    let mut chunks_read = 0;
    let mut bytes_read = 0;
    let mut rows_read = 0;
    let mut early_end = None;
    for _ in 0..ABANDON_CHUNKS {
        match stream.next().await {
            Some(Ok(chunk)) => {
                chunks_read += 1;
                bytes_read += chunk.len();
                rows_read += newline_count(&chunk);
            }
            Some(Err(error)) => {
                early_end = Some(EarlyStreamEnd::Error(
                    error.code().map(|code| code.code().to_owned()),
                ));
                break;
            }
            None => {
                early_end = Some(EarlyStreamEnd::Clean);
                break;
            }
        }
    }
    drop(stream);
    AbandonedCopyOutObservation {
        chunks_read,
        bytes_read,
        rows_read,
        early_end,
        reuse: tokio_probe(client, 44_004).await,
    }
}

async fn compio_abandoned_copy_out(
    client: &compio_postgres::Client,
) -> AbandonedCopyOutObservation {
    let mut stream = Box::pin(
        client
            .copy_out(WIDE_COPY_OUT)
            .await
            .expect("compio abandoned COPY OUT start"),
    );
    let mut chunks_read = 0;
    let mut bytes_read = 0;
    let mut rows_read = 0;
    let mut early_end = None;
    for _ in 0..ABANDON_CHUNKS {
        match stream.next().await {
            Some(Ok(chunk)) => {
                chunks_read += 1;
                bytes_read += chunk.len();
                rows_read += newline_count(&chunk);
            }
            Some(Err(error)) => {
                early_end = Some(EarlyStreamEnd::Error(
                    error.code().map(|code| code.code().to_owned()),
                ));
                break;
            }
            None => {
                early_end = Some(EarlyStreamEnd::Clean);
                break;
            }
        }
    }
    drop(stream);
    AbandonedCopyOutObservation {
        chunks_read,
        bytes_read,
        rows_read,
        early_end,
        reuse: compio_probe(client, 44_004).await,
    }
}

fn tokio_observations(url: String, tables: Tables) -> DriverObservations {
    on_tokio(url, move |client| async move {
        client
            .batch_execute(SESSION_SQL)
            .await
            .expect("set deterministic COPY session on tokio-postgres");
        let text_out = tokio_copy_out(&client, &text_copy_out_sql(&tables)).await;
        let binary_out = tokio_copy_out(&client, &binary_copy_out_sql(&tables)).await;
        let text_in = tokio_text_copy_in(&client, &tables).await;
        let binary_in = tokio_binary_copy_in(&client, &tables).await;
        let constraint_error = tokio_failed_copy_in(
            &client,
            &tables.constraint_error,
            &constraint_copy_in_sql(&tables),
            CONSTRAINT_ERROR_INPUT,
            41_001,
        )
        .await;
        let deferred_constraint = tokio_failed_copy_in(
            &client,
            &tables.deferred_constraint,
            &deferred_constraint_copy_in_sql(&tables),
            DEFERRED_CONSTRAINT_INPUT,
            41_501,
        )
        .await;
        let malformed_binary = tokio_failed_copy_in(
            &client,
            &tables.malformed_binary,
            &malformed_binary_copy_in_sql(&tables),
            MALFORMED_BINARY_INPUT,
            42_002,
        )
        .await;
        let cancelled_input = tokio_cancelled_copy_in(&client, &tables).await;
        let abandoned_output = tokio_abandoned_copy_out(&client).await;
        DriverObservations {
            text_out,
            binary_out,
            text_in,
            binary_in,
            constraint_error,
            deferred_constraint,
            malformed_binary,
            cancelled_input,
            abandoned_output,
        }
    })
}

async fn compio_observations(tables: &Tables) -> DriverObservations {
    let client = compio_client().await;
    client
        .batch_execute(SESSION_SQL)
        .await
        .expect("set deterministic COPY session on compio-postgres");
    let text_out = compio_copy_out(&client, &text_copy_out_sql(tables)).await;
    let binary_out = compio_copy_out(&client, &binary_copy_out_sql(tables)).await;
    let text_in = compio_text_copy_in(&client, tables).await;
    let binary_in = compio_binary_copy_in(&client, tables).await;
    let constraint_error = compio_failed_copy_in(
        &client,
        &tables.constraint_error,
        &constraint_copy_in_sql(tables),
        CONSTRAINT_ERROR_INPUT,
        41_001,
    )
    .await;
    let deferred_constraint = compio_failed_copy_in(
        &client,
        &tables.deferred_constraint,
        &deferred_constraint_copy_in_sql(tables),
        DEFERRED_CONSTRAINT_INPUT,
        41_501,
    )
    .await;
    let malformed_binary = compio_failed_copy_in(
        &client,
        &tables.malformed_binary,
        &malformed_binary_copy_in_sql(tables),
        MALFORMED_BINARY_INPUT,
        42_002,
    )
    .await;
    let cancelled_input = compio_cancelled_copy_in(&client, tables).await;
    let abandoned_output = compio_abandoned_copy_out(&client).await;
    DriverObservations {
        text_out,
        binary_out,
        text_in,
        binary_in,
        constraint_error,
        deferred_constraint,
        malformed_binary,
        cancelled_input,
        abandoned_output,
    }
}

async fn stored_rows(client: &compio_postgres::Client, table: &str) -> Vec<StoredRow> {
    client
        .query(
            &format!("SELECT id, payload, amount FROM {table} ORDER BY id"),
            &[],
        )
        .await
        .unwrap_or_else(|error| panic!("read COPY table {table}: {error}"))
        .into_iter()
        .map(|row| StoredRow {
            id: row.get(0),
            payload: row.get(1),
            amount: row.get(2),
        })
        .collect()
}

async fn table_count(client: &compio_postgres::Client, table: &str) -> i64 {
    client
        .query_one_scalar(&format!("SELECT count(*)::int8 FROM {table}"), &[])
        .await
        .unwrap_or_else(|error| panic!("count COPY table {table}: {error}"))
}

async fn populate_server_state(
    fixture: &compio_postgres::Client,
    tables: &Tables,
    observations: &mut DriverObservations,
) {
    observations.text_in.rows = stored_rows(fixture, &tables.text_input).await;
    observations.binary_in.rows = stored_rows(fixture, &tables.binary_input).await;
    observations.constraint_error.table_rows = table_count(fixture, &tables.constraint_error).await;
    observations.deferred_constraint.table_rows =
        table_count(fixture, &tables.deferred_constraint).await;
    observations.malformed_binary.table_rows = table_count(fixture, &tables.malformed_binary).await;
    observations.cancelled_input.table_rows = table_count(fixture, &tables.cancelled_input).await;
}

fn push_binary_field(buf: &mut BytesMut, value: Option<&[u8]>) {
    match value {
        Some(value) => {
            buf.put_i32(i32::try_from(value.len()).expect("test field fits an i32"));
            buf.put_slice(value);
        }
        None => buf.put_i32(-1),
    }
}

fn expected_binary_copy_out() -> Vec<u8> {
    let mut buf = BytesMut::new();
    buf.put_slice(b"PGCOPY\n\xff\r\n\0");
    buf.put_i32(0);
    buf.put_i32(0);
    for (id, payload, note) in [
        (1_i32, Some(&b"plain"[..]), None),
        (2_i32, Some(&b"tab\tline\nslash\\end"[..]), Some(&b"x"[..])),
        (3_i32, None, Some(&b"tail"[..])),
    ] {
        buf.put_i16(3);
        push_binary_field(&mut buf, Some(&id.to_be_bytes()));
        push_binary_field(&mut buf, payload);
        push_binary_field(&mut buf, note);
    }
    buf.put_i16(-1);
    buf.to_vec()
}

#[derive(Debug, PartialEq, Eq)]
struct BinaryLayout {
    header_len: usize,
    flags: i32,
    extension_len: i32,
    tuple_fields: Vec<i16>,
    field_lengths: Vec<i32>,
    null_fields: usize,
    trailer: i16,
    trailing_bytes: usize,
}

fn take_i16(bytes: &[u8], cursor: &mut usize) -> i16 {
    let raw: [u8; 2] = bytes
        .get(*cursor..*cursor + 2)
        .unwrap_or_else(|| panic!("binary COPY ended before i16 at offset {cursor}"))
        .try_into()
        .expect("the slice is exactly two bytes");
    *cursor += 2;
    i16::from_be_bytes(raw)
}

fn take_i32(bytes: &[u8], cursor: &mut usize) -> i32 {
    let raw: [u8; 4] = bytes
        .get(*cursor..*cursor + 4)
        .unwrap_or_else(|| panic!("binary COPY ended before i32 at offset {cursor}"))
        .try_into()
        .expect("the slice is exactly four bytes");
    *cursor += 4;
    i32::from_be_bytes(raw)
}

fn binary_layout(bytes: &[u8]) -> BinaryLayout {
    const MAGIC: &[u8] = b"PGCOPY\n\xff\r\n\0";
    assert_eq!(bytes.get(..MAGIC.len()), Some(MAGIC), "binary COPY magic");
    let mut cursor = MAGIC.len();
    let flags = take_i32(bytes, &mut cursor);
    let extension_len = take_i32(bytes, &mut cursor);
    assert!(
        extension_len >= 0,
        "binary COPY extension length is negative"
    );
    cursor += usize::try_from(extension_len).expect("nonnegative extension length");
    let header_len = cursor;
    let mut tuple_fields = Vec::new();
    let mut field_lengths = Vec::new();
    let mut null_fields = 0;
    let trailer = loop {
        let fields = take_i16(bytes, &mut cursor);
        if fields == -1 {
            break fields;
        }
        assert!(fields >= 0, "binary COPY tuple has a negative field count");
        tuple_fields.push(fields);
        for _ in 0..fields {
            let length = take_i32(bytes, &mut cursor);
            field_lengths.push(length);
            match length {
                -1 => null_fields += 1,
                length if length >= 0 => {
                    cursor += usize::try_from(length).expect("nonnegative field length");
                    assert!(
                        cursor <= bytes.len(),
                        "binary COPY field extends past the payload"
                    );
                }
                _ => panic!("binary COPY field has an invalid negative length"),
            }
        }
    };
    BinaryLayout {
        header_len,
        flags,
        extension_len,
        tuple_fields,
        field_lengths,
        null_fields,
        trailer,
        trailing_bytes: bytes.len() - cursor,
    }
}

#[allow(clippy::too_many_lines)]
fn field_differences(ours: &DriverObservations, theirs: &DriverObservations) -> Vec<Difference> {
    let mut differences = Vec::new();
    macro_rules! compare {
        ($case:literal, $aspect:literal, $ours:expr, $theirs:expr) => {{
            let ours_value = &$ours;
            let theirs_value = &$theirs;
            if ours_value != theirs_value {
                differences.push(Difference {
                    case: $case,
                    aspect: $aspect,
                    ours: format!("{ours_value:?}"),
                    theirs: format!("{theirs_value:?}"),
                });
            }
        }};
    }
    macro_rules! compare_completion {
        ($case:literal, $ours:expr, $theirs:expr) => {{
            compare!(
                $case,
                "completion",
                $ours.classification(),
                $theirs.classification()
            );
            compare!($case, "row count", $ours.rows(), $theirs.rows());
            compare!($case, "failure phase", $ours.phase(), $theirs.phase());
            compare!($case, "SQLSTATE", $ours.sqlstate(), $theirs.sqlstate());
        }};
    }

    compare!(
        "COPY OUT text",
        "concatenated bytes",
        ours.text_out.bytes,
        theirs.text_out.bytes
    );
    compare!(
        "COPY OUT binary",
        "concatenated bytes",
        ours.binary_out.bytes,
        theirs.binary_out.bytes
    );
    compare_completion!(
        "COPY IN text",
        ours.text_in.completion,
        theirs.text_in.completion
    );
    compare!(
        "COPY IN text",
        "table state",
        ours.text_in.rows,
        theirs.text_in.rows
    );
    compare_completion!(
        "COPY IN binary",
        ours.binary_in.completion,
        theirs.binary_in.completion
    );
    compare!(
        "COPY IN binary",
        "table state",
        ours.binary_in.rows,
        theirs.binary_in.rows
    );
    compare_completion!(
        "COPY IN constraint error",
        ours.constraint_error.completion,
        theirs.constraint_error.completion
    );
    compare!(
        "COPY IN constraint error",
        "writes attempted",
        ours.constraint_error.writes_attempted,
        theirs.constraint_error.writes_attempted
    );
    compare!(
        "COPY IN constraint error",
        "writes accepted",
        ours.constraint_error.writes_accepted,
        theirs.constraint_error.writes_accepted
    );
    compare!(
        "COPY IN constraint error",
        "table state",
        ours.constraint_error.table_rows,
        theirs.constraint_error.table_rows
    );
    compare!(
        "COPY IN constraint error",
        "session reuse",
        ours.constraint_error.reuse,
        theirs.constraint_error.reuse
    );
    compare_completion!(
        "COPY IN deferred constraint",
        ours.deferred_constraint.completion,
        theirs.deferred_constraint.completion
    );
    compare!(
        "COPY IN deferred constraint",
        "writes attempted",
        ours.deferred_constraint.writes_attempted,
        theirs.deferred_constraint.writes_attempted
    );
    compare!(
        "COPY IN deferred constraint",
        "writes accepted",
        ours.deferred_constraint.writes_accepted,
        theirs.deferred_constraint.writes_accepted
    );
    compare!(
        "COPY IN deferred constraint",
        "table state",
        ours.deferred_constraint.table_rows,
        theirs.deferred_constraint.table_rows
    );
    compare!(
        "COPY IN deferred constraint",
        "session reuse",
        ours.deferred_constraint.reuse,
        theirs.deferred_constraint.reuse
    );
    compare_completion!(
        "COPY IN malformed binary",
        ours.malformed_binary.completion,
        theirs.malformed_binary.completion
    );
    compare!(
        "COPY IN malformed binary",
        "writes attempted",
        ours.malformed_binary.writes_attempted,
        theirs.malformed_binary.writes_attempted
    );
    compare!(
        "COPY IN malformed binary",
        "writes accepted",
        ours.malformed_binary.writes_accepted,
        theirs.malformed_binary.writes_accepted
    );
    compare!(
        "COPY IN malformed binary",
        "table state",
        ours.malformed_binary.table_rows,
        theirs.malformed_binary.table_rows
    );
    compare!(
        "COPY IN malformed binary",
        "session reuse",
        ours.malformed_binary.reuse,
        theirs.malformed_binary.reuse
    );
    compare!(
        "COPY IN dropped sink",
        "write before drop",
        ours.cancelled_input.write,
        theirs.cancelled_input.write
    );
    compare!(
        "COPY IN dropped sink",
        "table state",
        ours.cancelled_input.table_rows,
        theirs.cancelled_input.table_rows
    );
    compare!(
        "COPY IN dropped sink",
        "session reuse",
        ours.cancelled_input.reuse,
        theirs.cancelled_input.reuse
    );
    compare!(
        "COPY OUT abandoned stream",
        "ended before drop",
        ours.abandoned_output.early_end,
        theirs.abandoned_output.early_end
    );
    compare!(
        "COPY OUT abandoned stream",
        "session reuse",
        ours.abandoned_output.reuse,
        theirs.abandoned_output.reuse
    );
    differences
}

fn documented_divergences() -> BTreeSet<(&'static str, &'static str)> {
    // PostgreSQL can reject a deferred constraint after CommandComplete, at
    // the Sync boundary. Tokio-postgres 0.7.18 returns that provisional count;
    // compio-postgres deliberately drains through ReadyForQuery and returns the
    // authoritative 23503 instead (`copy_in_failure.rs` pins the behavior).
    BTreeSet::from([
        ("COPY IN deferred constraint", "completion"),
        ("COPY IN deferred constraint", "row count"),
        ("COPY IN deferred constraint", "failure phase"),
        ("COPY IN deferred constraint", "SQLSTATE"),
    ])
}

fn assert_case_census(differences: &[Difference]) {
    assert_eq!(
        CASES.len(),
        9,
        "a COPY differential case silently disappeared"
    );
    let names: BTreeSet<_> = CASES.iter().map(|case| case.name).collect();
    assert_eq!(names.len(), CASES.len(), "COPY case names must be unique");

    let kind_census = (
        CASES
            .iter()
            .filter(|case| case.kind == CaseKind::CopyOut)
            .count(),
        CASES
            .iter()
            .filter(|case| case.kind == CaseKind::CopyInSuccess)
            .count(),
        CASES
            .iter()
            .filter(|case| case.kind == CaseKind::CopyInError)
            .count(),
        CASES
            .iter()
            .filter(|case| case.kind == CaseKind::Abandonment)
            .count(),
    );
    assert_eq!(
        kind_census,
        (2, 2, 3, 2),
        "the corpus stopped covering every COPY outcome family"
    );
    let encoding_census = (
        CASES
            .iter()
            .filter(|case| case.encoding == CopyEncoding::Text)
            .count(),
        CASES
            .iter()
            .filter(|case| case.encoding == CopyEncoding::Binary)
            .count(),
    );
    assert_eq!(
        encoding_census,
        (6, 3),
        "the corpus stopped covering both COPY encodings"
    );

    let expected = documented_divergences();
    let mut agree = 0;
    let mut deliberate = 0;
    let mut finding = 0;
    for case in CASES {
        let case_differences: Vec<_> = differences
            .iter()
            .filter(|difference| difference.case == case.name)
            .collect();
        if case_differences.is_empty() {
            agree += 1;
        } else if case_differences
            .iter()
            .all(|difference| expected.contains(&(difference.case, difference.aspect)))
        {
            deliberate += 1;
        } else {
            finding += 1;
        }
    }
    assert_eq!(
        (agree, deliberate, finding),
        (8, 1, 0),
        "the COPY case outcome census changed"
    );
}

#[allow(clippy::too_many_lines)]
fn assert_dense_evidence(observations: &DriverObservations) {
    assert_eq!(TEXT_COPY_INPUT.len(), 3, "text COPY IN chunk census");
    assert_eq!(
        TEXT_COPY_INPUT.concat(),
        b"1\talpha\t\\N\n2\ttab\\tline\\nslash\\\\end\t20\n3\t\\N\t-30\n"
    );
    assert_eq!(
        CONSTRAINT_ERROR_INPUT.len(),
        3,
        "constraint failure must include a write after the violating row"
    );
    assert_eq!(CONSTRAINT_ERROR_INPUT[0], b"1\n");
    assert_eq!(CONSTRAINT_ERROR_INPUT[1], b"1\n");
    assert_eq!(CONSTRAINT_ERROR_INPUT[2], b"2\n");
    assert_eq!(CONSTRAINT_ERROR_INPUT.concat(), b"1\n1\n2\n");
    assert_eq!(DEFERRED_CONSTRAINT_INPUT.len(), 2);
    assert_eq!(DEFERRED_CONSTRAINT_INPUT[0], b"314");
    assert_eq!(DEFERRED_CONSTRAINT_INPUT[1], b"159\n");
    assert_eq!(
        MALFORMED_BINARY_INPUT.len(),
        4,
        "binary failure must include a write after the malformed tuple"
    );
    assert_eq!(MALFORMED_BINARY_INPUT.concat().len(), 37);
    assert_eq!(MALFORMED_BINARY_INPUT[0].len(), 19);
    assert_eq!(
        MALFORMED_BINARY_INPUT[0],
        b"PGCOPY\n\xff\r\n\0\0\0\0\0\0\0\0\0"
    );
    assert_eq!(MALFORMED_BINARY_INPUT[1], b"\0\x01\0\0\0\x04\0\0\0\x01");
    assert_eq!(MALFORMED_BINARY_INPUT[2], b"\0\x01\xff\xff\xff\xfe");
    assert_eq!(MALFORMED_BINARY_INPUT[3], b"\xff\xff");

    let expected_text = b"1\tplain\t\\N\n\
                          2\ttab\\tline\\nslash\\\\end\tx\n\
                          3\t\\N\ttail\n";
    assert_eq!(observations.text_out.bytes, expected_text);
    assert!(
        observations.text_out.chunks > 0,
        "text COPY OUT yielded no chunks"
    );

    let expected_binary = expected_binary_copy_out();
    assert_eq!(observations.binary_out.bytes, expected_binary);
    assert_eq!(observations.binary_out.bytes.len(), 103);
    assert!(
        observations.binary_out.chunks > 0,
        "binary COPY OUT yielded no chunks"
    );
    assert_eq!(
        binary_layout(&observations.binary_out.bytes),
        BinaryLayout {
            header_len: 19,
            flags: 0,
            extension_len: 0,
            tuple_fields: vec![3, 3, 3],
            field_lengths: vec![4, 5, -1, 4, 18, 1, 4, -1, 4],
            null_fields: 2,
            trailer: -1,
            trailing_bytes: 0,
        }
    );

    let expected_rows = expected_input_rows();
    assert_eq!(observations.text_in.completion, CopyCompletion::Rows(3));
    assert_eq!(observations.text_in.rows, expected_rows);
    assert_eq!(observations.binary_in.completion, CopyCompletion::Rows(3));
    assert_eq!(observations.binary_in.rows, expected_rows);

    assert_eq!(
        observations.constraint_error.completion,
        CopyCompletion::Error {
            phase: FailurePhase::Finish,
            sqlstate: Some("23505".to_owned()),
        }
    );
    assert_eq!(observations.constraint_error.writes_attempted, 3);
    assert_eq!(observations.constraint_error.writes_accepted, 3);
    assert_eq!(observations.constraint_error.table_rows, 0);
    assert_eq!(
        observations.constraint_error.reuse,
        ProbeOutcome::Value(41_001)
    );
    assert_eq!(
        observations.malformed_binary.completion,
        CopyCompletion::Error {
            phase: FailurePhase::Finish,
            sqlstate: Some("22P04".to_owned()),
        }
    );
    assert_eq!(observations.malformed_binary.writes_attempted, 4);
    assert_eq!(observations.malformed_binary.writes_accepted, 4);
    assert_eq!(observations.malformed_binary.table_rows, 0);
    assert_eq!(
        observations.malformed_binary.reuse,
        ProbeOutcome::Value(42_002)
    );

    assert_eq!(observations.cancelled_input.write, WriteOutcome::Accepted);
    assert_eq!(observations.cancelled_input.table_rows, 0);
    assert_eq!(
        observations.cancelled_input.reuse,
        ProbeOutcome::Value(43_003)
    );

    assert_eq!(observations.abandoned_output.chunks_read, ABANDON_CHUNKS);
    assert!(observations.abandoned_output.bytes_read > 0);
    assert!(
        (1..WIDE_COPY_ROWS).contains(&observations.abandoned_output.rows_read),
        "COPY OUT was not abandoned partway: {:?}",
        observations.abandoned_output
    );
    assert_eq!(observations.abandoned_output.early_end, None);
    assert_eq!(
        observations.abandoned_output.reuse,
        ProbeOutcome::Value(44_004)
    );
}

fn assert_documented_divergence_evidence(ours: &DriverObservations, theirs: &DriverObservations) {
    assert_eq!(
        ours.deferred_constraint.completion,
        CopyCompletion::Error {
            phase: FailurePhase::Finish,
            sqlstate: Some("23503".to_owned()),
        }
    );
    assert_eq!(ours.deferred_constraint.writes_attempted, 2);
    assert_eq!(ours.deferred_constraint.writes_accepted, 2);
    assert_eq!(ours.deferred_constraint.table_rows, 0);
    assert_eq!(ours.deferred_constraint.reuse, ProbeOutcome::Value(41_501));

    assert_eq!(
        theirs.deferred_constraint.completion,
        CopyCompletion::Rows(1)
    );
    assert_eq!(theirs.deferred_constraint.writes_attempted, 2);
    assert_eq!(theirs.deferred_constraint.writes_accepted, 2);
    assert_eq!(theirs.deferred_constraint.table_rows, 0);
    assert_eq!(
        theirs.deferred_constraint.reuse,
        ProbeOutcome::Value(41_501)
    );
}

/// Every observable part of valid COPY IN/OUT either agrees with tokio-postgres
/// or remains in the exact documented `ReadyForQuery` divergence set.
#[compio::test]
async fn copy_subprotocol_matches_tokio_or_a_documented_divergence() {
    let fixture = compio_client().await;
    let tables = Tables::unique();
    fixture
        .batch_execute(&fixture_sql(&tables))
        .await
        .expect("create the shared COPY fixtures");

    let mut theirs = tokio_observations(common::plaintext_url(), tables.clone());
    populate_server_state(&fixture, &tables, &mut theirs).await;
    fixture
        .batch_execute(&reset_mutable_tables_sql(&tables))
        .await
        .expect("clear tokio COPY state before the compio cases");
    let mut ours = Box::pin(compio::time::timeout(
        Duration::from_secs(30),
        compio_observations(&tables),
    ))
    .await
    .expect("the compio COPY subject exceeded its 30-second watchdog");
    populate_server_state(&fixture, &tables, &mut ours).await;

    fixture
        .batch_execute(&cleanup_sql(&tables))
        .await
        .expect("drop the shared COPY fixtures");

    let differences = field_differences(&ours, &theirs);
    let expected = documented_divergences();
    let findings: Vec<_> = differences
        .iter()
        .filter(|difference| !expected.contains(&(difference.case, difference.aspect)))
        .collect();
    assert!(
        findings.is_empty(),
        "FINDING: COPY behavior diverged outside the documented set:\n{}",
        findings
            .iter()
            .map(|difference| format!(
                "case={} aspect={} compio-postgres={} tokio-postgres={}",
                difference.case, difference.aspect, difference.ours, difference.theirs
            ))
            .collect::<Vec<_>>()
            .join("\n")
    );
    let actual: BTreeSet<_> = differences
        .iter()
        .map(|difference| (difference.case, difference.aspect))
        .collect();
    assert_eq!(
        actual, expected,
        "a documented COPY divergence was not exercised, or changed classification"
    );
    assert_case_census(&differences);
    assert_dense_evidence(&ours);
    assert_dense_evidence(&theirs);
    assert_documented_divergence_evidence(&ours, &theirs);
}
