//! Differential `ErrorResponse` field tests against `tokio-postgres` 0.7.18.
//!
//! Both drivers execute the same statements, against the same named objects,
//! on the same `PostgreSQL` server. The compio connection is capped at protocol
//! 3.0 to match upstream, and both sessions receive the same settings before a
//! case runs. Only owned Rust data crosses the tokio thread boundary.
//!
//! `PostgreSQL` sends different optional-field subsets for different failures.
//! The cases deliberately discriminate those subsets: for example, a unique
//! violation has a constraint but no column, while a not-null violation has a
//! column but no constraint. Agreement on `None` is checked for every case as
//! well as agreement on populated values.
//!
//! `DbError::position` combines the wire's original position (`P`) and its
//! internal position/query pair (`p` + `q`) in an enum. The two crates expose
//! separate Rust types with that same shape, so this test decomposes both into
//! the three underlying values before comparing them. Neither API exposes the
//! raw nonlocalized severity (`V`); both expose its parsed `Severity`, which is
//! compared independently from the localized severity string (`S`).

#![allow(clippy::future_not_send)]

use std::future::Future;

use bytes::Bytes;
use compio_postgres::config::ProtocolVersion;
use futures_util::SinkExt;

#[allow(unused_imports)]
use crate::common;

const SESSION_TIMEOUT: &str = "SET statement_timeout = '50ms'";
const RESET_TIMEOUT: &str = "SET statement_timeout = 0";

#[derive(Clone, Copy, Debug)]
enum FailureKind {
    Execute,
    CopyIn(&'static [u8]),
}

#[derive(Clone, Debug)]
struct ErrorCase {
    name: &'static str,
    statement: String,
    before: Option<&'static str>,
    after: Option<&'static str>,
    kind: FailureKind,
}

impl ErrorCase {
    fn execute(name: &'static str, statement: impl Into<String>) -> Self {
        Self {
            name,
            statement: statement.into(),
            before: None,
            after: None,
            kind: FailureKind::Execute,
        }
    }

    fn timed(name: &'static str, statement: impl Into<String>) -> Self {
        Self {
            name,
            statement: statement.into(),
            before: Some(SESSION_TIMEOUT),
            after: Some(RESET_TIMEOUT),
            kind: FailureKind::Execute,
        }
    }

    fn copy_in(name: &'static str, statement: impl Into<String>, data: &'static [u8]) -> Self {
        Self {
            name,
            statement: statement.into(),
            before: None,
            after: None,
            kind: FailureKind::CopyIn(data),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ErrorObservation {
    name: &'static str,
    code: String,
    severity: String,
    nonlocalized_severity: Option<String>,
    message: String,
    detail: Option<String>,
    hint: Option<String>,
    position: Option<u32>,
    internal_position: Option<u32>,
    internal_query: Option<String>,
    where_: Option<String>,
    schema: Option<String>,
    table: Option<String>,
    column: Option<String>,
    datatype: Option<String>,
    constraint: Option<String>,
    file: Option<String>,
    line: Option<u32>,
    routine: Option<String>,
}

fn on_tokio<T, F, Fut>(url: String, run: F) -> T
where
    T: Send + 'static,
    F: FnOnce(tokio_postgres::Client) -> Fut + Send + 'static,
    Fut: Future<Output = T> + 'static,
{
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build the tokio runtime");
        runtime.block_on(async move {
            let (client, connection) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
                .await
                .expect("tokio-postgres connect");
            let driver = tokio::spawn(async move {
                let _ = connection.await;
            });
            let result = run(client).await;
            let _ = driver.await;
            result
        })
    })
    .join()
    .expect("the tokio thread panicked")
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

fn fixture_sql(schema: &str) -> String {
    format!(
        "CREATE SCHEMA {schema};
         CREATE TABLE {schema}.unique_values (
             id integer CONSTRAINT error_unique UNIQUE
         );
         INSERT INTO {schema}.unique_values VALUES (1);
         CREATE TABLE {schema}.not_null_values (value integer NOT NULL);
         CREATE TABLE {schema}.parent_values (id integer PRIMARY KEY);
         CREATE TABLE {schema}.child_values (
             parent_id integer CONSTRAINT error_foreign_key
                 REFERENCES {schema}.parent_values (id)
         );
         CREATE TABLE {schema}.check_values (
             value integer CONSTRAINT error_positive CHECK (value > 0)
         );
         CREATE TABLE {schema}.copy_values (value integer);
         CREATE DOMAIN {schema}.positive_integer AS integer
             CONSTRAINT positive_integer_check CHECK (VALUE > 0)"
    )
}

fn session_sql(schema: &str) -> String {
    format!(
        "SET application_name = 'compio-postgres differential server errors';
         SET client_encoding = 'UTF8';
         SET lc_messages = 'C';
         SET search_path TO {schema}, pg_catalog;
         SET statement_timeout = 0"
    )
}

fn error_cases() -> Vec<ErrorCase> {
    vec![
        ErrorCase::execute("unique constraint", "INSERT INTO unique_values VALUES (1)"),
        ErrorCase::execute(
            "not-null constraint",
            "INSERT INTO not_null_values VALUES (NULL)",
        ),
        ErrorCase::execute(
            "foreign-key constraint",
            "INSERT INTO child_values VALUES (999)",
        ),
        ErrorCase::execute("check constraint", "INSERT INTO check_values VALUES (-1)"),
        ErrorCase::execute("syntax error", "SELEC 1"),
        ErrorCase::execute("undefined table", "SELECT * FROM missing_values"),
        ErrorCase::execute(
            "undefined column",
            "SELECT missing_column FROM unique_values",
        ),
        ErrorCase::execute("division by zero", "SELECT 1 / 0"),
        ErrorCase::execute(
            "PL/pgSQL raise",
            r"DO $body$
BEGIN
    RAISE EXCEPTION USING
        ERRCODE = 'P0001',
        MESSAGE = 'differential raised failure',
        DETAIL = 'differential detail',
        HINT = 'differential hint';
END;
$body$",
        ),
        // RAISE supplies DETAIL/HINT/context but does not put `p` + `q` on
        // the wire. A failing internally generated statement does, so it is a
        // separate case rather than pretending the RAISE carried those tags.
        ErrorCase::execute(
            "PL/pgSQL internal query",
            r"DO $body$
BEGIN
    PERFORM 1 FROM missing_internal_relation;
END;
$body$",
        ),
        ErrorCase::execute("type cast", "SELECT 'not_an_integer'::integer"),
        ErrorCase::execute("domain constraint", "SELECT (-1)::positive_integer"),
        ErrorCase::timed("statement timeout", "SELECT pg_sleep(1)"),
        ErrorCase::copy_in(
            "COPY input",
            "COPY copy_values (value) FROM STDIN",
            b"not_an_integer\n",
        ),
    ]
}

fn tokio_observation(name: &'static str, db: &tokio_postgres::error::DbError) -> ErrorObservation {
    let (position, internal_position, internal_query) = match db.position() {
        Some(tokio_postgres::error::ErrorPosition::Original(position)) => {
            (Some(*position), None, None)
        }
        Some(tokio_postgres::error::ErrorPosition::Internal { position, query }) => {
            (None, Some(*position), Some(query.clone()))
        }
        None => (None, None, None),
    };
    ErrorObservation {
        name,
        code: db.code().code().to_owned(),
        severity: db.severity().to_owned(),
        nonlocalized_severity: db.parsed_severity().map(|severity| severity.to_string()),
        message: db.message().to_owned(),
        detail: db.detail().map(str::to_owned),
        hint: db.hint().map(str::to_owned),
        position,
        internal_position,
        internal_query,
        where_: db.where_().map(str::to_owned),
        schema: db.schema().map(str::to_owned),
        table: db.table().map(str::to_owned),
        column: db.column().map(str::to_owned),
        datatype: db.datatype().map(str::to_owned),
        constraint: db.constraint().map(str::to_owned),
        file: db.file().map(str::to_owned),
        line: db.line(),
        routine: db.routine().map(str::to_owned),
    }
}

fn compio_observation(
    name: &'static str,
    db: &compio_postgres::error::DbError,
) -> ErrorObservation {
    let (position, internal_position, internal_query) = match db.position() {
        Some(compio_postgres::error::ErrorPosition::Original(position)) => {
            (Some(*position), None, None)
        }
        Some(compio_postgres::error::ErrorPosition::Internal { position, query }) => {
            (None, Some(*position), Some(query.clone()))
        }
        None => (None, None, None),
    };
    ErrorObservation {
        name,
        code: db.code().code().to_owned(),
        severity: db.severity().to_owned(),
        nonlocalized_severity: db.parsed_severity().map(|severity| severity.to_string()),
        message: db.message().to_owned(),
        detail: db.detail().map(str::to_owned),
        hint: db.hint().map(str::to_owned),
        position,
        internal_position,
        internal_query,
        where_: db.where_().map(str::to_owned),
        schema: db.schema().map(str::to_owned),
        table: db.table().map(str::to_owned),
        column: db.column().map(str::to_owned),
        datatype: db.datatype().map(str::to_owned),
        constraint: db.constraint().map(str::to_owned),
        file: db.file().map(str::to_owned),
        line: db.line(),
        routine: db.routine().map(str::to_owned),
    }
}

async fn tokio_case(client: &tokio_postgres::Client, case: &ErrorCase) -> ErrorObservation {
    if let Some(before) = case.before {
        client
            .batch_execute(before)
            .await
            .unwrap_or_else(|error| panic!("{}: tokio prelude: {error}", case.name));
    }

    let error = match case.kind {
        FailureKind::Execute => match client.execute(case.statement.as_str(), &[]).await {
            Ok(rows) => panic!("{}: tokio statement succeeded with {rows} rows", case.name),
            Err(error) => error,
        },
        FailureKind::CopyIn(data) => {
            let sink = client
                .copy_in(case.statement.as_str())
                .await
                .unwrap_or_else(|error| panic!("{}: tokio COPY start: {error}", case.name));
            futures_util::pin_mut!(sink);
            match sink.as_mut().send(Bytes::from_static(data)).await {
                Err(error) => error,
                Ok(()) => match sink.as_mut().finish().await {
                    Ok(rows) => {
                        panic!("{}: tokio COPY input succeeded with {rows} rows", case.name)
                    }
                    Err(error) => error,
                },
            }
        }
    };

    if let Some(after) = case.after {
        client
            .batch_execute(after)
            .await
            .unwrap_or_else(|reset| panic!("{}: tokio reset after {error}: {reset}", case.name));
    }
    let db = error
        .as_db_error()
        .unwrap_or_else(|| panic!("{}: tokio returned a non-DbError: {error}", case.name));
    tokio_observation(case.name, db)
}

async fn compio_case(client: &compio_postgres::Client, case: &ErrorCase) -> ErrorObservation {
    if let Some(before) = case.before {
        client
            .batch_execute(before)
            .await
            .unwrap_or_else(|error| panic!("{}: compio prelude: {error}", case.name));
    }

    let error = match case.kind {
        FailureKind::Execute => match client.execute(case.statement.as_str(), &[]).await {
            Ok(rows) => panic!("{}: compio statement succeeded with {rows} rows", case.name),
            Err(error) => error,
        },
        FailureKind::CopyIn(data) => {
            let sink = client
                .copy_in::<_, Bytes>(case.statement.as_str())
                .await
                .unwrap_or_else(|error| panic!("{}: compio COPY start: {error}", case.name));
            futures_util::pin_mut!(sink);
            match sink.as_mut().send(Bytes::from_static(data)).await {
                Err(error) => error,
                Ok(()) => match sink.as_mut().finish().await {
                    Ok(rows) => panic!(
                        "{}: compio COPY input succeeded with {rows} rows",
                        case.name
                    ),
                    Err(error) => error,
                },
            }
        }
    };

    if let Some(after) = case.after {
        client
            .batch_execute(after)
            .await
            .unwrap_or_else(|reset| panic!("{}: compio reset after {error}: {reset}", case.name));
    }
    let db = error
        .as_db_error()
        .unwrap_or_else(|| panic!("{}: compio returned a non-DbError: {error}", case.name));
    compio_observation(case.name, db)
}

fn tokio_observations(
    url: String,
    session: String,
    cases: Vec<ErrorCase>,
) -> Vec<ErrorObservation> {
    on_tokio(url, move |client| async move {
        client
            .batch_execute(&session)
            .await
            .expect("set deterministic error session on tokio-postgres");
        let mut observations = Vec::with_capacity(cases.len());
        for case in &cases {
            observations.push(tokio_case(&client, case).await);
        }
        observations
    })
}

async fn compio_observations(session: &str, cases: &[ErrorCase]) -> Vec<ErrorObservation> {
    let client = compio_client().await;
    client
        .batch_execute(session)
        .await
        .expect("set deterministic error session on compio-postgres");
    let mut observations = Vec::with_capacity(cases.len());
    for case in cases {
        observations.push(compio_case(&client, case).await);
    }
    observations
}

fn assert_driver_agreement(ours: &[ErrorObservation], theirs: &[ErrorObservation]) {
    assert_eq!(
        ours.len(),
        theirs.len(),
        "the drivers answered different numbers of error cases"
    );
    let mut divergences = Vec::new();
    for (ours, theirs) in ours.iter().zip(theirs) {
        if ours.name != theirs.name {
            divergences.push(format!(
                "case identity: compio-postgres={:?}, tokio-postgres={:?}",
                ours.name, theirs.name
            ));
            continue;
        }

        macro_rules! compare_field {
            ($field:ident) => {
                if ours.$field != theirs.$field {
                    divergences.push(format!(
                        "{} `{}`: compio-postgres={:?}, tokio-postgres={:?}",
                        ours.name,
                        stringify!($field),
                        ours.$field,
                        theirs.$field
                    ));
                }
            };
        }

        compare_field!(code);
        compare_field!(severity);
        compare_field!(nonlocalized_severity);
        compare_field!(message);
        compare_field!(detail);
        compare_field!(hint);
        compare_field!(position);
        compare_field!(internal_position);
        compare_field!(internal_query);
        compare_field!(where_);
        compare_field!(schema);
        compare_field!(table);
        compare_field!(column);
        compare_field!(datatype);
        compare_field!(constraint);
        compare_field!(file);
        compare_field!(line);
        compare_field!(routine);
    }
    assert!(
        divergences.is_empty(),
        "the two ErrorResponse surfaces diverged:\n{}",
        divergences.join("\n")
    );
}

fn observed<'a>(observations: &'a [ErrorObservation], name: &str) -> &'a ErrorObservation {
    observations
        .iter()
        .find(|observation| observation.name == name)
        .unwrap_or_else(|| panic!("no error observation named {name}"))
}

fn assert_field_space_was_exercised(observations: &[ErrorObservation], schema: &str) {
    assert_eq!(observations.len(), 14, "an error case silently disappeared");
    for observation in observations {
        assert_eq!(observation.severity, "ERROR", "{}", observation.name);
        assert_eq!(
            observation.nonlocalized_severity.as_deref(),
            Some("ERROR"),
            "{} did not exercise the nonlocalized severity field",
            observation.name
        );
        assert!(!observation.message.is_empty(), "{}", observation.name);
        assert!(observation.file.is_some(), "{}", observation.name);
        assert!(observation.line.is_some(), "{}", observation.name);
        assert!(observation.routine.is_some(), "{}", observation.name);
    }

    let unique = observed(observations, "unique constraint");
    assert_eq!(unique.code, "23505");
    assert_eq!(unique.schema.as_deref(), Some(schema));
    assert_eq!(unique.table.as_deref(), Some("unique_values"));
    assert_eq!(unique.constraint.as_deref(), Some("error_unique"));
    assert!(unique.detail.is_some());
    assert_eq!(unique.column, None);

    let not_null = observed(observations, "not-null constraint");
    assert_eq!(not_null.code, "23502");
    assert_eq!(not_null.table.as_deref(), Some("not_null_values"));
    assert_eq!(not_null.column.as_deref(), Some("value"));
    assert_eq!(not_null.constraint, None);

    let foreign_key = observed(observations, "foreign-key constraint");
    assert_eq!(foreign_key.code, "23503");
    assert_eq!(foreign_key.constraint.as_deref(), Some("error_foreign_key"));
    assert!(foreign_key.detail.is_some());

    let check = observed(observations, "check constraint");
    assert_eq!(check.code, "23514");
    assert_eq!(check.constraint.as_deref(), Some("error_positive"));
    assert!(check.detail.is_some());

    for (name, code) in [
        ("syntax error", "42601"),
        ("undefined table", "42P01"),
        ("undefined column", "42703"),
        ("type cast", "22P02"),
    ] {
        let observation = observed(observations, name);
        assert_eq!(observation.code, code, "{name}");
        assert!(observation.position.is_some(), "{name}");
    }

    let division = observed(observations, "division by zero");
    assert_eq!(division.code, "22012");
    assert_eq!(division.detail, None);
    assert_eq!(division.hint, None);
    assert_eq!(division.position, None);
    assert_eq!(division.internal_position, None);
    assert_eq!(division.internal_query, None);
    assert_eq!(division.where_, None);
    assert_eq!(division.schema, None);
    assert_eq!(division.table, None);
    assert_eq!(division.column, None);
    assert_eq!(division.datatype, None);
    assert_eq!(division.constraint, None);

    let raised = observed(observations, "PL/pgSQL raise");
    assert_eq!(raised.code, "P0001");
    assert_eq!(raised.detail.as_deref(), Some("differential detail"));
    assert_eq!(raised.hint.as_deref(), Some("differential hint"));
    assert!(raised.where_.is_some());

    let internal = observed(observations, "PL/pgSQL internal query");
    assert_eq!(internal.code, "42P01");
    assert_eq!(internal.position, None);
    assert!(internal.internal_position.is_some());
    assert_eq!(
        internal.internal_query.as_deref(),
        Some("SELECT 1 FROM missing_internal_relation")
    );
    assert!(internal.where_.is_some());

    let domain = observed(observations, "domain constraint");
    assert_eq!(domain.code, "23514");
    assert_eq!(domain.schema.as_deref(), Some(schema));
    assert_eq!(domain.datatype.as_deref(), Some("positive_integer"));
    assert_eq!(domain.constraint.as_deref(), Some("positive_integer_check"));

    let timeout = observed(observations, "statement timeout");
    assert_eq!(timeout.code, "57014");

    let copy = observed(observations, "COPY input");
    assert_eq!(copy.code, "22P02");
    assert!(
        copy.where_
            .as_deref()
            .is_some_and(|context| context.contains("COPY copy_values")),
        "COPY did not surface its server context: {:?}",
        copy.where_
    );
}

/// Every public `DbError` field agrees with tokio-postgres across distinct
/// server-error shapes, including an error delivered while COPY is active.
#[compio::test]
async fn every_server_error_field_matches_tokio_postgres() {
    let setup = compio_client().await;
    let schema = common::test_object_name("cpg_differential_server_errors");
    setup
        .batch_execute(&fixture_sql(&schema))
        .await
        .expect("create the shared error fixture");

    let cases = error_cases();
    let session = session_sql(&schema);
    let theirs = tokio_observations(common::plaintext_url(), session.clone(), cases.clone());
    let ours = compio_observations(&session, &cases).await;

    setup
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .expect("drop the shared error fixture");

    assert_driver_agreement(&ours, &theirs);
    assert_field_space_was_exercised(&ours, &schema);
}
