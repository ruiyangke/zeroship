//! Integration tests for compio-postgres.
//!
//! Ported from zeroship-pg's integration suite. Each `#[compio::test]` opens
//! a fresh connection (via the `connect` helper), spawns the connection
//! driver onto compio's runtime, and exercises one slice of the API.
//!
//! Every test works inside a private schema of its own (see `require_pg`), so
//! the suite runs at full parallelism against one database.
//!
//! Run with:
//!   docker compose up -d postgres
//!   PG_TEST_URL='postgres://postgres:zeroship@localhost:5440/zeroship' \
//!       cargo test -p compio-postgres --test integration

use compio_postgres::error::SqlState;
use compio_postgres::types::{IsNull, ToSql, Type, to_sql_checked};
use compio_postgres::{
    Client, Config, Error, NoTls, Pool, PoolConfig, QueryOutcome, Row, SimpleQueryMessage,
    TransactionStatus, Uncached,
};
use std::fmt;
use std::ops::Deref;
use std::sync::mpsc;
use std::time::Duration;

#[allow(unused_imports)]
use crate::common;

fn test_url() -> String {
    common::test_url()
}

/// Names the private schema belonging to the calling test.
///
/// libtest runs each test on a thread named after the test - at any
/// `--test-threads` setting, serial runs included - so the thread name is a
/// per-test identifier that a newly added test gets for free and cannot forget
/// to declare. The `unnamed` fallback only applies to a thread the test body
/// spawned itself; such a thread shares the schema of whichever test is
/// running, so open connections from the test's own thread.
///
/// [`common::test_object_name`] adds the process discriminator and owns all
/// sanitising, hashing, and PostgreSQL identifier-length budgeting. Keeping
/// that rule in one place matters: appending a PID here after filling all 63
/// bytes would let PostgreSQL silently truncate the unique part away.
fn test_schema() -> String {
    let thread = std::thread::current();
    let name = thread.name().unwrap_or("unnamed");
    common::test_object_name(&format!("cpg_{name}"))
}

const ADMIN_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
// THESE ARE HANG DETECTORS, NOT PERFORMANCE ASSERTIONS. Each one exists so a
// fixture that never completes fails with a sentence instead of wedging the
// run; none of them is a claim about how long the work should take. A budget
// tight enough to be exceeded by machine load therefore buys nothing and costs
// a false red.
//
// `ADMIN_STATEMENT_TIMEOUT` was 5s and a `CREATE SCHEMA` -- normally about a
// millisecond -- blew through it on 2026-08-23 at load 16.4, with a peer
// project's test suite as the top consumer. The same statement passed in 0.25s
// in isolation moments later. See the load table on
// `read_timeout::copy_input_time_is_not_charged_as_server_read_silence` for the
// measured version of this effect; this is the same failure in a fixture.
//
// A genuine hang still fails, just later, and the per-test watchdogs bound the
// run regardless.
const ADMIN_STATEMENT_TIMEOUT: Duration = Duration::from_secs(30);
const ADMIN_BATCH_TIMEOUT: Duration = Duration::from_secs(60);
const ADMIN_DRIVER_TIMEOUT: Duration = Duration::from_secs(30);
const ADMIN_CLEANUP_WAIT: Duration = Duration::from_secs(30);

#[derive(Debug)]
struct AdminSqlError {
    detail: String,
    sqlstate: Option<String>,
}

impl AdminSqlError {
    fn plain(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
            sqlstate: None,
        }
    }

    fn database(context: &str, error: &Error) -> Self {
        Self {
            detail: format!("{context}: {}", common::error_chain(error)),
            sqlstate: error.code().map(|code| code.code().to_owned()),
        }
    }
}

impl fmt::Display for AdminSqlError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.detail.fmt(formatter)
    }
}

/// Executes administrative SQL with a bound at every layer which can wait.
///
/// Database DDL is deliberately outside a transaction. `lock_timeout` turns
/// another session's object lock into an error, `statement_timeout` bounds the
/// server operation as a whole, the compio timeout also covers protocol stalls,
/// and callers which run this on a cleanup thread have their own receive bound.
fn execute_admin_sql_bounded(
    url: &str,
    statements: &[String],
    continue_after_error: bool,
) -> Result<(), AdminSqlError> {
    let mut config: Config = url
        .parse()
        .map_err(|error: Error| AdminSqlError::database("parse admin URL", &error))?;
    config.connect_timeout(ADMIN_CONNECT_TIMEOUT);

    let runtime = compio::runtime::Runtime::new()
        .map_err(|error| AdminSqlError::plain(format!("create cleanup runtime: {error}")))?;
    let outcome = runtime.block_on(compio::time::timeout(ADMIN_BATCH_TIMEOUT, async move {
        let (client, connection) = config
            .connect(common::suite_tls())
            .await
            .map_err(|error| AdminSqlError::database("connect admin client", &error))?;
        let driver = compio::runtime::spawn(async move { connection.run().await });

        let timeout_ms = ADMIN_STATEMENT_TIMEOUT.as_millis();
        let mut failures = Vec::new();
        let mut sqlstate = None;
        if let Err(error) = client
            .batch_execute(&format!(
                "SET lock_timeout = '{timeout_ms}ms'; \
                 SET statement_timeout = '{timeout_ms}ms'"
            ))
            .await
        {
            sqlstate = error.code().map(|code| code.code().to_owned());
            failures.push(format!(
                "install administrative SQL timeouts: {}",
                common::error_chain(&error)
            ));
        } else {
            for statement in statements {
                if let Err(error) = client.execute(statement, &[]).await {
                    if sqlstate.is_none() {
                        sqlstate = error.code().map(|code| code.code().to_owned());
                    }
                    failures.push(format!("{statement}: {}", common::error_chain(&error)));
                    if !continue_after_error {
                        break;
                    }
                }
            }
        }

        drop(client);
        if compio::time::timeout(ADMIN_DRIVER_TIMEOUT, driver)
            .await
            .is_err()
        {
            failures.push("admin connection driver exceeded its shutdown timeout".to_string());
        }

        if failures.is_empty() {
            Ok(())
        } else {
            Err(AdminSqlError {
                detail: failures.join("; "),
                sqlstate,
            })
        }
    }));

    outcome.map_err(|_| {
        AdminSqlError::plain(format!(
            "administrative SQL exceeded its {} second outer timeout",
            ADMIN_BATCH_TIMEOUT.as_secs()
        ))
    })?
}

/// Runs object cleanup even when the owning test unwinds.
///
/// `Drop` cannot await, so the async client lives on a fresh OS thread and the
/// test thread waits only on a bounded channel receive. A cleanup failure makes
/// an otherwise-green test red; while another panic is already unwinding it is
/// printed instead, avoiding a double-panic abort that would hide the original
/// assertion.
struct BoundedSqlCleanup {
    label: String,
    url: String,
    statements: Vec<String>,
}

impl BoundedSqlCleanup {
    fn new(label: impl Into<String>, url: String, statements: Vec<String>) -> Self {
        Self {
            label: label.into(),
            url,
            statements,
        }
    }

    fn report_failure(label: &str, detail: String) {
        let message = format!("failed to clean up {label}: {detail}");
        if std::thread::panicking() {
            eprintln!("{message}");
        } else {
            panic!("{message}");
        }
    }
}

impl Drop for BoundedSqlCleanup {
    fn drop(&mut self) {
        let label = self.label.clone();
        let url = std::mem::take(&mut self.url);
        let statements = std::mem::take(&mut self.statements);
        let (sender, receiver) = mpsc::sync_channel(1);

        let cleanup_thread = std::thread::Builder::new()
            .name("cpg-object-cleanup".to_string())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    execute_admin_sql_bounded(&url, &statements, true)
                }))
                .unwrap_or_else(|payload| {
                    let detail = payload
                        .downcast_ref::<&str>()
                        .map(|message| (*message).to_string())
                        .or_else(|| payload.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "cleanup thread panicked".to_string());
                    Err(AdminSqlError::plain(detail))
                });
                let _ = sender.send(result);
            });

        if let Err(error) = cleanup_thread {
            Self::report_failure(&label, format!("spawn cleanup thread: {error}"));
            return;
        }

        match receiver.recv_timeout(ADMIN_CLEANUP_WAIT) {
            Ok(Ok(())) => {}
            Ok(Err(error)) => Self::report_failure(&label, error.to_string()),
            Err(error) => Self::report_failure(
                &label,
                format!(
                    "cleanup did not finish within {} seconds: {error}",
                    ADMIN_CLEANUP_WAIT.as_secs()
                ),
            ),
        }
    }
}

/// A schema-scoped connection URL whose schema is removed at test teardown.
struct TestUrl {
    scoped: String,
    _cleanup: BoundedSqlCleanup,
}

impl Deref for TestUrl {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.scoped
    }
}

impl fmt::Display for TestUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.scoped.fmt(formatter)
    }
}

/// Confines every connection opened from `url` to `schema`.
///
/// The startup packet carries the `search_path`, so this holds for pooled
/// connections as well as for plain clients - a pool opens its connections
/// itself and offers no post-connect hook to run `SET` on.
fn schema_scoped_url(url: &str, schema: &str) -> String {
    let sep = if url.contains('?') { '&' } else { '?' };
    format!("{url}{sep}options=-c%20search_path%3D{schema}")
}

/// Open a client and spawn its driver on the compio runtime.
async fn connect(url: &str) -> Result<Client, Error> {
    let (client, connection) = compio_postgres::connect(url, common::suite_tls()).await?;
    compio::runtime::spawn(async move {
        if let Err(e) = connection.run().await {
            eprintln!("connection error: {e}");
        }
    })
    .detach();
    Ok(client)
}

/// Open a client whose implicit raw-SQL prepared-statement cache has the
/// requested per-connection capacity.
async fn connect_with_statement_cache(url: &str, capacity: usize) -> Result<Client, Error> {
    let mut config: Config = url.parse()?;
    config.statement_cache_capacity(capacity);
    let (client, connection) = config.connect(common::suite_tls()).await?;
    compio::runtime::spawn(async move {
        if let Err(e) = connection.run().await {
            eprintln!("connection error: {e}");
        }
    })
    .detach();
    Ok(client)
}

/// Open one cached connection whose Nth exact-SQL execution is eligible for
/// promotion. The threshold is programmatic-only, not a libpq parameter.
async fn connect_with_statement_cache_threshold(
    url: &str,
    capacity: usize,
    threshold: usize,
) -> Result<Client, Error> {
    let mut config: Config = url.parse()?;
    config.statement_cache_capacity(capacity);
    config.statement_cache_execution_threshold(
        std::num::NonZeroUsize::new(threshold).expect("test thresholds are nonzero"),
    );
    let (client, connection) = config.connect(common::suite_tls()).await?;
    compio::runtime::spawn(async move {
        if let Err(e) = connection.run().await {
            eprintln!("connection error: {e}");
        }
    })
    .detach();
    Ok(client)
}

/// Checks that Postgres is reachable and hands back a URL scoped to a schema
/// this test alone owns.
///
/// Tests share fixed object names (`pg_complex_test` and friends) and cargo
/// runs them concurrently, so in one shared schema they race: two tests
/// creating the same table collide on `pg_type`'s name index, and one test's
/// rows land in another's result set. A schema per test keeps the names but
/// removes the sharing.
///
/// The schema name includes the process identity, so two `cargo test`
/// processes pointed at one database cannot reset or use each other's schema.
/// [`TestUrl`] owns bounded teardown rather than relying on the next run's
/// pre-test `DROP`: PID-unique leftovers would otherwise accumulate forever.
/// The initial drop remains only as recovery for a killed test whose PID was
/// later reused.
///
/// A schema does not isolate everything: LISTEN/NOTIFY channels, advisory
/// locks and replication slots are database-wide. A test using one of those
/// still has to pick a name no other test can be holding - see
/// `notify_delivered_on_idle_listener`.
///
/// THERE IS NO SKIP. A database this crate cannot reach panics here, so this
/// returns a `TestUrl` rather than an `Option`.
///
/// It used to return `Option` with every caller writing
/// `let Some(url) = require_pg().await else { return; }`. The `None` arm became
/// unreachable when the skip was replaced by a panic, and was left in place
/// deliberately at the time - rewriting 93 call sites in that commit would have
/// put what the tests ASSERT in the same diff as whether they RUN. This is that
/// rewrite, on its own.
///
/// Removing the arm matters beyond tidiness: an `Option` here advertises that a
/// test may skip, and 93 `else { return }` branches are a standing invitation to
/// make `None` reachable again - at which point 93 tests become silent no-ops
/// that still report green. That is the exact failure this crate's `live-tls-tests`
/// feature exists to prevent, described in its `Cargo.toml` comment.
async fn require_pg() -> TestUrl {
    let url = test_url();
    let client = match compio::time::timeout(ADMIN_CONNECT_TIMEOUT, connect(&url)).await {
        Ok(Ok(client)) => client,
        // `process::exit(0)` would have ended the WHOLE binary with a success
        // status the moment one test could not reach Postgres, discarding every
        // result already produced. A panic ends only this test, so its siblings
        // and any failure already reported still stand - and unlike the skip
        // that used to be here, the run goes red.
        Ok(Err(error)) => common::postgres_unreachable(&url, &error),
        Err(_) => panic!(
            "PostgreSQL connection exceeded the {} second test-fixture timeout",
            ADMIN_CONNECT_TIMEOUT.as_secs()
        ),
    };

    let schema = test_schema();
    let drop_schema = format!("DROP SCHEMA IF EXISTS {schema} CASCADE");
    let cleanup = BoundedSqlCleanup::new(
        format!("test schema {schema}"),
        url.clone(),
        vec![drop_schema.clone()],
    );

    compio::time::timeout(
        ADMIN_STATEMENT_TIMEOUT,
        client.batch_execute(&format!(
            "SET lock_timeout = '{}ms'; SET statement_timeout = '{}ms'",
            ADMIN_STATEMENT_TIMEOUT.as_millis(),
            ADMIN_STATEMENT_TIMEOUT.as_millis()
        )),
    )
    .await
    .expect("installing fixture timeouts exceeded its outer timeout")
    .unwrap();
    compio::time::timeout(ADMIN_STATEMENT_TIMEOUT, client.execute(&drop_schema, &[]))
        .await
        .expect("stale-schema cleanup exceeded its fixture timeout")
        .unwrap();
    compio::time::timeout(
        ADMIN_STATEMENT_TIMEOUT,
        client.execute(&format!("CREATE SCHEMA {schema}"), &[]),
    )
    .await
    .expect("schema creation exceeded its fixture timeout")
    .unwrap();

    // Client dropped -> driver task exits.
    drop(client);
    TestUrl {
        scoped: schema_scoped_url(&url, &schema),
        _cleanup: cleanup,
    }
}

// ---------------------------------------------------------------------------
// 0. test_isolation_is_per_schema
//
// Guards the isolation the rest of the suite depends on. Everything below
// creates objects under fixed names, so if a connection ever lands somewhere
// other than this test's own schema the suite goes back to racing itself:
// concurrent tests collide on `pg_type`'s name index and read each other's
// rows. Asserting `current_schema()` catches that directly, rather than
// waiting for the intermittent collision to reappear.
//
// Pools are covered too, because they open their own connections and take the
// `search_path` from the startup packet like any other client.
// ---------------------------------------------------------------------------

#[compio::test]
async fn test_isolation_is_per_schema() {
    let url = require_pg().await;
    let expected = test_schema();

    let client = connect(&url).await.unwrap();
    let rows = client
        .query("SELECT current_schema()::text AS s", &[])
        .await
        .unwrap();
    assert_eq!(
        rows[0].get::<_, &str>("s"),
        expected,
        "a plain client escaped this test's schema"
    );

    let pool = Pool::connect(&url, 2).await.unwrap();
    let conn = pool.get().await.unwrap();
    let rows = conn
        .query("SELECT current_schema()::text AS s", &[])
        .await
        .unwrap();
    assert_eq!(
        rows[0].get::<_, &str>("s"),
        expected,
        "a pooled connection escaped this test's schema"
    );
}

// ---------------------------------------------------------------------------
// 1. connect_and_close
// ---------------------------------------------------------------------------

#[compio::test]
async fn connect_and_close() {
    let url = require_pg().await;
    let client = connect(&url).await.unwrap();
    // Verify the client is alive and usable.
    assert!(!client.is_closed());
    let rows = client.query("SELECT 1::int4", &[]).await.unwrap();
    assert_eq!(rows.len(), 1);
    // Drop closes the client - driver task exits gracefully.
    drop(client);
}

// ---------------------------------------------------------------------------
// 2. simple_query
// ---------------------------------------------------------------------------

#[compio::test]
async fn simple_query() {
    let url = require_pg().await;
    let client = connect(&url).await.unwrap();

    let rows = client
        .query("SELECT 1 as num, 'hello' as greeting", &[])
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);

    let row = &rows[0];
    let num: i32 = row.get("num");
    let greeting: &str = row.get("greeting");
    assert_eq!(num, 1);
    assert_eq!(greeting, "hello");
}

// ---------------------------------------------------------------------------
// 3. parameterized_query
// ---------------------------------------------------------------------------

#[compio::test]
async fn parameterized_query() {
    let url = require_pg().await;
    let client = connect(&url).await.unwrap();

    let val: i32 = 42;
    let rows = client
        .query("SELECT $1::int4 as val", &[&val])
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);

    let result: i32 = rows[0].get("val");
    assert_eq!(result, 42);
}

// ---------------------------------------------------------------------------
// 4. create_table_insert_select_drop
// ---------------------------------------------------------------------------

#[compio::test]
async fn create_table_insert_select_drop() {
    let url = require_pg().await;
    let client = connect(&url).await.unwrap();
    let table = common::test_object_name("test_crud");

    // Clean up from any prior failed run
    client
        .execute(&format!("DROP TABLE IF EXISTS {table}"), &[])
        .await
        .unwrap();

    // Create
    client
        .execute(
            &format!("CREATE TABLE {table} (id serial PRIMARY KEY, name text NOT NULL)"),
            &[],
        )
        .await
        .unwrap();

    // Insert
    let affected = client
        .execute(
            &format!("INSERT INTO {table} (name) VALUES ($1)"),
            &[&"alice"],
        )
        .await
        .unwrap();
    assert_eq!(affected, 1);

    let affected = client
        .execute(
            &format!("INSERT INTO {table} (name) VALUES ($1)"),
            &[&"bob"],
        )
        .await
        .unwrap();
    assert_eq!(affected, 1);

    // Select
    let rows = client
        .query(&format!("SELECT id, name FROM {table} ORDER BY id"), &[])
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].get::<_, &str>("name"), "alice");
    assert_eq!(rows[1].get::<_, &str>("name"), "bob");

    // Drop
    client
        .execute(&format!("DROP TABLE {table}"), &[])
        .await
        .unwrap();
}

// ---------------------------------------------------------------------------
// 5. transaction_commit
// ---------------------------------------------------------------------------

#[compio::test]
async fn transaction_commit() {
    let url = require_pg().await;
    let mut client = connect(&url).await.unwrap();
    let table = common::test_object_name("test_tx_commit");

    client
        .execute(&format!("DROP TABLE IF EXISTS {table}"), &[])
        .await
        .unwrap();
    client
        .execute(
            &format!("CREATE TABLE {table} (id serial PRIMARY KEY, val text)"),
            &[],
        )
        .await
        .unwrap();

    {
        let tx = client.transaction().await.unwrap();
        tx.execute(&format!("INSERT INTO {table} (val) VALUES ($1)"), &[&"one"])
            .await
            .unwrap();
        tx.execute(&format!("INSERT INTO {table} (val) VALUES ($1)"), &[&"two"])
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }

    // Data should persist after commit
    let rows = client
        .query(&format!("SELECT val FROM {table} ORDER BY id"), &[])
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].get::<_, &str>("val"), "one");
    assert_eq!(rows[1].get::<_, &str>("val"), "two");

    client
        .execute(&format!("DROP TABLE {table}"), &[])
        .await
        .unwrap();
}

// ---------------------------------------------------------------------------
// 6. transaction_rollback_on_drop
// ---------------------------------------------------------------------------

#[compio::test]
async fn transaction_rollback_on_drop() {
    let url = require_pg().await;
    let mut client = connect(&url).await.unwrap();
    let table = common::test_object_name("test_tx_rollback");

    client
        .execute(&format!("DROP TABLE IF EXISTS {table}"), &[])
        .await
        .unwrap();
    client
        .execute(
            &format!("CREATE TABLE {table} (id serial PRIMARY KEY, val text)"),
            &[],
        )
        .await
        .unwrap();

    // Begin transaction, insert, then drop without commit.
    // compio-postgres's Transaction handles rollback internally via Drop.
    {
        let tx = client.transaction().await.unwrap();
        tx.execute(
            &format!("INSERT INTO {table} (val) VALUES ($1)"),
            &[&"ghost"],
        )
        .await
        .unwrap();
        // tx dropped without commit - Drop impl enqueues ROLLBACK.
    }

    // Client should still be usable for subsequent queries - this is the
    // key observable that replaces the legacy `needs_rollback` flag.
    let rows = client
        .query(&format!("SELECT val FROM {table}"), &[])
        .await
        .unwrap();
    assert_eq!(rows.len(), 0, "expected ghost row to have been rolled back");

    client
        .execute(&format!("DROP TABLE {table}"), &[])
        .await
        .unwrap();
}

// ---------------------------------------------------------------------------
// 7. error_handling
// ---------------------------------------------------------------------------

#[compio::test]
async fn error_handling() {
    let url = require_pg().await;
    let client = connect(&url).await.unwrap();

    // Query a nonexistent table
    let err = client
        .query("SELECT * FROM nonexistent_table_xyz", &[])
        .await
        .unwrap_err();

    match err.code() {
        Some(code) if code == &SqlState::UNDEFINED_TABLE => {}
        other => panic!("expected 'undefined_table' SQLSTATE, got {other:?}: {err}"),
    }

    // Connection should still be usable after error
    let rows = client.query("SELECT 1 as ok", &[]).await.unwrap();
    assert_eq!(rows[0].get::<_, i32>("ok"), 1);
}

#[compio::test]
async fn simple_query_stream_ends_after_reporting_a_database_error() {
    use futures_util::StreamExt;

    let url = require_pg().await;
    let client = connect(&url).await.unwrap();
    let stream = client.simple_query_raw("SELECT 1 / 0").await.unwrap();
    let mut stream = std::pin::pin!(stream);

    let error = stream
        .next()
        .await
        .expect("the stream ended before reporting the database error")
        .unwrap_err();
    assert_eq!(error.code(), Some(&SqlState::DIVISION_BY_ZERO));
    assert!(
        stream.next().await.is_none(),
        "the stream produced another item after its database error"
    );
    assert_eq!(client.transaction_status(), Some(TransactionStatus::Idle));
}

// ---------------------------------------------------------------------------
// 8. pool_basic
// ---------------------------------------------------------------------------

#[compio::test]
async fn pool_basic() {
    let url = require_pg().await;
    let pool = Pool::connect(&url, 4).await.unwrap();

    let rows = pool.query("SELECT 42 as answer", &[]).await.unwrap();
    assert_eq!(rows[0].get::<_, i32>("answer"), 42);

    let affected = pool.execute("SELECT 1", &[]).await.unwrap();
    // SELECT returns 1 row in the command tag
    assert_eq!(affected, 1);
}

// ---------------------------------------------------------------------------
// 9. pool_reuse
// ---------------------------------------------------------------------------

#[compio::test]
async fn pool_reuse() {
    let url = require_pg().await;
    let pool = Pool::connect(&url, 2).await.unwrap();

    // Use the pool 5 times - should reuse connections, not create new ones each time
    for i in 0..5 {
        let val: i32 = i;
        let rows = pool.query("SELECT $1::int4 as v", &[&val]).await.unwrap();
        assert_eq!(rows[0].get::<_, i32>("v"), i);
    }
}

// ---------------------------------------------------------------------------
// 10. null_values
// ---------------------------------------------------------------------------

#[compio::test]
async fn null_values() {
    let url = require_pg().await;
    let client = connect(&url).await.unwrap();

    let rows = client.query("SELECT NULL::text as val", &[]).await.unwrap();
    assert_eq!(rows.len(), 1);

    let val: Option<&str> = rows[0].get("val");
    assert!(val.is_none(), "expected None for NULL::text, got {val:?}");
}

// ---------------------------------------------------------------------------
// 11. wrong_password
// ---------------------------------------------------------------------------

#[compio::test]
async fn wrong_password() {
    let url = test_url();
    // Located structurally, not by a literal. This used to replace the exact
    // string `:zeroship@`, which is right for the default plaintext DSN and
    // matches nothing else: against any other server the "bad" DSN was the
    // GOOD one, the connection succeeded, and the test failed claiming the
    // server had accepted a wrong password.
    let bad_url = common::with_password(&url, "wrong_password_xyz")
        .expect("the test DSN carries no password to make wrong");
    assert_ne!(
        bad_url, url,
        "the password was not replaced, so this would test the opposite of its name"
    );

    let err = match connect(&bad_url).await {
        Err(e) => e,
        Ok(_) => panic!("expected connection to fail with wrong password"),
    };

    // compio-postgres classifies authentication failures as either an
    // auth-kind error (client-side failure) or a DB error (server refused
    // via ErrorResponse). Both are acceptable. We check that the error is
    // not a generic closed/connect failure by confirming it carries a
    // meaningful source.
    let msg = format!("{err}");
    assert!(
        msg.contains("auth")
            || msg.contains("password")
            || err.code().is_some()
            || err.as_db_error().is_some(),
        "expected auth/password-related error, got: {err}"
    );
}

// ---------------------------------------------------------------------------
// Helper: create the complex test table
// ---------------------------------------------------------------------------

const COMPLEX_TABLE: &str = "pg_complex_test";

async fn create_complex_table(client: &Client) {
    client
        .execute(&format!("DROP TABLE IF EXISTS {COMPLEX_TABLE}"), &[])
        .await
        .unwrap();
    client
        .execute(
            &format!(
                "CREATE TABLE {COMPLEX_TABLE} (
                    id SERIAL PRIMARY KEY,
                    name TEXT NOT NULL,
                    value BIGINT DEFAULT 0,
                    data BYTEA,
                    flag BOOLEAN DEFAULT false,
                    score DOUBLE PRECISION,
                    small_num SMALLINT,
                    created_at TIMESTAMPTZ DEFAULT NOW()
                )"
            ),
            &[],
        )
        .await
        .unwrap();
}

async fn drop_complex_table(client: &Client) {
    client
        .execute(&format!("DROP TABLE IF EXISTS {COMPLEX_TABLE}"), &[])
        .await
        .unwrap();
}

// ---------------------------------------------------------------------------
// 12. large_result_set
// ---------------------------------------------------------------------------

#[compio::test]
async fn large_result_set() {
    let url = require_pg().await;
    let client = connect(&url).await.unwrap();

    create_complex_table(&client).await;

    // INSERT 1000 rows
    for i in 0..1000i64 {
        client
            .execute(
                &format!("INSERT INTO {COMPLEX_TABLE} (name, value) VALUES ($1, $2)"),
                &[&format!("row_{i}"), &i],
            )
            .await
            .unwrap();
    }

    // SELECT all
    let rows = client
        .query(
            &format!("SELECT id, name, value FROM {COMPLEX_TABLE} ORDER BY id"),
            &[],
        )
        .await
        .unwrap();

    assert_eq!(rows.len(), 1000);

    // Verify a sampling of values
    for (idx, row) in rows.iter().enumerate() {
        let name: &str = row.get("name");
        let value: i64 = row.get("value");
        assert_eq!(name, format!("row_{idx}"));
        assert_eq!(value, idx as i64);
    }

    drop_complex_table(&client).await;
}

// ---------------------------------------------------------------------------
// 13. concurrent_connections
// ---------------------------------------------------------------------------

#[compio::test]
async fn concurrent_connections() {
    let url = require_pg().await;
    let pool = Pool::connect(&url, 5).await.unwrap();

    // Acquire 5 connections, run a query on each, verify all succeed
    let mut results = Vec::new();
    for i in 0..5i32 {
        let conn = pool.get().await.unwrap();
        let rows = conn.query("SELECT $1::int4 as val", &[&i]).await.unwrap();
        results.push(rows[0].get::<_, i32>("val"));
        // conn dropped -> returned to pool
    }

    assert_eq!(results, vec![0, 1, 2, 3, 4]);
}

// ---------------------------------------------------------------------------
// 14. text_types
// ---------------------------------------------------------------------------

#[compio::test]
async fn text_types() {
    let url = require_pg().await;
    let client = connect(&url).await.unwrap();

    create_complex_table(&client).await;

    // Empty string
    client
        .execute(
            &format!("INSERT INTO {COMPLEX_TABLE} (name) VALUES ($1)"),
            &[&""],
        )
        .await
        .unwrap();

    // Unicode: emoji + CJK
    let unicode_str = "Hello 🌍🎉 你好世界 こんにちは";
    client
        .execute(
            &format!("INSERT INTO {COMPLEX_TABLE} (name) VALUES ($1)"),
            &[&unicode_str],
        )
        .await
        .unwrap();

    // Very long string (10KB)
    let long_str = "A".repeat(10 * 1024);
    client
        .execute(
            &format!("INSERT INTO {COMPLEX_TABLE} (name) VALUES ($1)"),
            &[&long_str.as_str()],
        )
        .await
        .unwrap();

    // Special chars: quotes, backslashes, newlines
    let special_str = "it's a \"test\"\\with\nnewlines\tand\ttabs";
    client
        .execute(
            &format!("INSERT INTO {COMPLEX_TABLE} (name) VALUES ($1)"),
            &[&special_str],
        )
        .await
        .unwrap();

    // SELECT all back and verify
    let rows = client
        .query(
            &format!("SELECT name FROM {COMPLEX_TABLE} ORDER BY id"),
            &[],
        )
        .await
        .unwrap();

    assert_eq!(rows.len(), 4);
    assert_eq!(rows[0].get::<_, &str>("name"), "");
    assert_eq!(rows[1].get::<_, &str>("name"), unicode_str);
    assert_eq!(rows[2].get::<_, &str>("name"), long_str.as_str());
    assert_eq!(rows[3].get::<_, &str>("name"), special_str);

    drop_complex_table(&client).await;
}

// ---------------------------------------------------------------------------
// 15. numeric_types
// ---------------------------------------------------------------------------

#[compio::test]
async fn numeric_types() {
    let url = require_pg().await;
    let client = connect(&url).await.unwrap();

    create_complex_table(&client).await;

    let small: i16 = -123;
    let medium: i32 = 42_000;
    let large: i64 = 9_000_000_000i64;
    // These are arbitrary round-trip test inputs, not attempts to write PI/E -
    // clippy::approx_constant would have us "fix" the value under test.
    #[allow(clippy::approx_constant)]
    let float_s: f32 = 3.14;
    #[allow(clippy::approx_constant)]
    let float_d: f64 = 2.718281828459045;
    let flag: bool = true;

    client
        .execute(
            &format!(
                "INSERT INTO {COMPLEX_TABLE} (name, small_num, value, score, flag) \
                 VALUES ($1, $2, $3, $4, $5)"
            ),
            &[&"numeric_test", &small, &large, &float_d, &flag],
        )
        .await
        .unwrap();

    let rows = client
        .query(
            &format!("SELECT small_num, value, score, flag FROM {COMPLEX_TABLE} WHERE name = $1"),
            &[&"numeric_test"],
        )
        .await
        .unwrap();

    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.get::<_, i16>("small_num"), small);
    assert_eq!(row.get::<_, i64>("value"), large);
    assert_eq!(row.get::<_, f64>("score"), float_d);
    assert_eq!(row.get::<_, bool>("flag"), flag);

    // Test i32 and f32 via direct SELECT with casts
    let rows = client
        .query(
            "SELECT $1::int4 as i, $2::float4 as f",
            &[&medium, &float_s],
        )
        .await
        .unwrap();
    assert_eq!(rows[0].get::<_, i32>("i"), medium);
    assert_eq!(rows[0].get::<_, f32>("f"), float_s);

    drop_complex_table(&client).await;
}

// ---------------------------------------------------------------------------
// 16. binary_data
// ---------------------------------------------------------------------------

#[compio::test]
async fn binary_data() {
    let url = require_pg().await;
    let client = connect(&url).await.unwrap();

    create_complex_table(&client).await;

    // Binary data with null bytes, 0xFF, and various byte patterns
    let binary: Vec<u8> = (0..=255).collect();
    // Also test a chunk with embedded nulls
    let mut with_nulls = vec![0u8, 1, 0, 0, 255, 254, 0, 128];
    with_nulls.extend_from_slice(&binary);

    client
        .execute(
            &format!("INSERT INTO {COMPLEX_TABLE} (name, data) VALUES ($1, $2)"),
            &[&"binary_test", &with_nulls.as_slice()],
        )
        .await
        .unwrap();

    let rows = client
        .query(
            &format!("SELECT data FROM {COMPLEX_TABLE} WHERE name = $1"),
            &[&"binary_test"],
        )
        .await
        .unwrap();

    assert_eq!(rows.len(), 1);
    let data: &[u8] = rows[0].get("data");
    assert_eq!(data, with_nulls.as_slice());

    drop_complex_table(&client).await;
}

// ---------------------------------------------------------------------------
// 17. multiple_statements_sequential
// ---------------------------------------------------------------------------

#[compio::test]
async fn multiple_statements_sequential() {
    let url = require_pg().await;
    let client = connect(&url).await.unwrap();

    create_complex_table(&client).await;

    // Run 20 different queries on the same connection
    for i in 0..20i64 {
        client
            .execute(
                &format!("INSERT INTO {COMPLEX_TABLE} (name, value) VALUES ($1, $2)"),
                &[&format!("seq_{i}"), &i],
            )
            .await
            .unwrap();
    }

    // Also do 20 SELECT queries
    for i in 0..20i64 {
        let rows = client
            .query(
                &format!("SELECT value FROM {COMPLEX_TABLE} WHERE name = $1"),
                &[&format!("seq_{i}")],
            )
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get::<_, i64>("value"), i);
    }

    // Verify connection still healthy
    assert!(!client.is_closed());

    drop_complex_table(&client).await;
}

// ---------------------------------------------------------------------------
// 18. transaction_rollback_explicit
// ---------------------------------------------------------------------------

#[compio::test]
async fn transaction_rollback_explicit() {
    let url = require_pg().await;
    let mut client = connect(&url).await.unwrap();

    create_complex_table(&client).await;

    // BEGIN, INSERT, explicit ROLLBACK
    {
        let tx = client.transaction().await.unwrap();
        tx.execute(
            &format!("INSERT INTO {COMPLEX_TABLE} (name) VALUES ($1)"),
            &[&"should_not_exist"],
        )
        .await
        .unwrap();
        tx.rollback().await.unwrap();
    }

    // Verify data not present
    let rows = client
        .query(
            &format!("SELECT name FROM {COMPLEX_TABLE} WHERE name = $1"),
            &[&"should_not_exist"],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 0);

    // Connection is usable
    assert!(!client.is_closed());

    drop_complex_table(&client).await;
}

// ---------------------------------------------------------------------------
// Savepoint identifiers
// ---------------------------------------------------------------------------

#[compio::test]
async fn savepoint_name_with_a_space_is_quoted() {
    let url = require_pg().await;
    let mut client = connect(&url).await.unwrap();

    let mut tx = client.transaction().await.unwrap();
    let result = match tx.savepoint("my savepoint").await {
        Ok(savepoint) => savepoint.commit().await,
        Err(error) => Err(error),
    };
    tx.rollback().await.unwrap();

    if let Err(error) = result {
        panic!(
            "a legal savepoint identifier containing a space was rejected: {}",
            common::error_chain(&error)
        );
    }
}

#[compio::test]
async fn mixed_case_savepoint_keeps_its_exact_name_and_rolls_back_rows() {
    let url = require_pg().await;
    let mut client = connect(&url).await.unwrap();
    let table = common::test_object_name("cpg_tx_mixed_case");

    client
        .batch_execute(&format!("CREATE TABLE {table} (n int)"))
        .await
        .unwrap();

    let exact_rollback = {
        let mut tx = client.transaction().await.unwrap();
        let savepoint = tx.savepoint("MyPoint").await.unwrap();
        savepoint
            .execute(&format!("INSERT INTO {table} VALUES (1)"), &[])
            .await
            .unwrap();

        let result = savepoint
            .batch_execute(r#"ROLLBACK TO SAVEPOINT "MyPoint""#)
            .await;
        if result.is_ok() {
            savepoint.commit().await.unwrap();
        } else {
            // Recover the transaction on the unfixed implementation so the
            // deliberate RED run can still drop its table before asserting.
            savepoint.rollback().await.unwrap();
        }
        tx.commit().await.unwrap();
        result
    };

    let rows = client
        .query(&format!("SELECT n FROM {table}"), &[])
        .await
        .unwrap();

    client
        .batch_execute(&format!("DROP TABLE {table}"))
        .await
        .unwrap();

    assert!(
        rows.is_empty(),
        "ROLLBACK TO the exact mixed-case name kept rows: {rows:?}"
    );
    if let Err(error) = exact_rollback {
        panic!(
            "the savepoint was not created with its exact mixed-case name: {}",
            common::error_chain(&error)
        );
    }
}

#[compio::test]
async fn savepoint_name_with_a_double_quote_is_escaped() {
    let url = require_pg().await;
    let mut client = connect(&url).await.unwrap();

    let mut tx = client.transaction().await.unwrap();
    let result = match tx.savepoint("quoted\"point").await {
        Ok(savepoint) => {
            let result = savepoint
                .batch_execute(r#"ROLLBACK TO SAVEPOINT "quoted""point""#)
                .await;
            if result.is_ok() {
                savepoint.commit().await.unwrap();
            } else {
                savepoint.rollback().await.unwrap();
            }
            result
        }
        Err(error) => Err(error),
    };
    tx.rollback().await.unwrap();

    if let Err(error) = result {
        panic!(
            "a legal savepoint identifier containing a double quote was rejected: {}",
            common::error_chain(&error)
        );
    }
}

#[compio::test]
async fn semicolon_in_savepoint_name_does_not_split_the_simple_query() {
    let url = require_pg().await;
    let mut client = connect(&url).await.unwrap();
    let table = common::test_object_name("cpg_tx_semicolon");
    let injected_savepoint = format!("point; INSERT INTO {table} VALUES (99); --");

    client
        .batch_execute(&format!("CREATE TABLE {table} (n int)"))
        .await
        .unwrap();

    let injected_rows = {
        let mut tx = client.transaction().await.unwrap();
        let savepoint = tx.savepoint(&injected_savepoint).await.unwrap();
        savepoint.rollback().await.unwrap();
        let count: i64 = tx
            .query_one(&format!("SELECT count(*) FROM {table}"), &[])
            .await
            .unwrap()
            .get(0);
        tx.rollback().await.unwrap();
        count
    };

    client
        .batch_execute(&format!("DROP TABLE {table}"))
        .await
        .unwrap();

    assert_eq!(
        injected_rows, 0,
        "the savepoint name was parsed as extra simple-query statements"
    );
}

#[compio::test]
async fn inner_savepoint_rollback_keeps_outer_work() {
    let url = require_pg().await;
    let mut client = connect(&url).await.unwrap();
    let table = common::test_object_name("cpg_tx_nested");

    client
        .batch_execute(&format!("CREATE TABLE {table} (n int)"))
        .await
        .unwrap();

    {
        let mut tx = client.transaction().await.unwrap();
        tx.execute(&format!("INSERT INTO {table} VALUES (1)"), &[])
            .await
            .unwrap();
        {
            let mut outer = tx.transaction().await.unwrap();
            outer
                .execute(&format!("INSERT INTO {table} VALUES (2)"), &[])
                .await
                .unwrap();
            {
                let inner = outer.transaction().await.unwrap();
                inner
                    .execute(&format!("INSERT INTO {table} VALUES (3)"), &[])
                    .await
                    .unwrap();
                inner.rollback().await.unwrap();
            }
            outer
                .execute(&format!("INSERT INTO {table} VALUES (4)"), &[])
                .await
                .unwrap();
            outer.commit().await.unwrap();
        }
        tx.commit().await.unwrap();
    }

    let rows = client
        .query(&format!("SELECT n FROM {table} ORDER BY n"), &[])
        .await
        .unwrap();
    let kept: Vec<i32> = rows.iter().map(|row| row.get(0)).collect();

    client
        .batch_execute(&format!("DROP TABLE {table}"))
        .await
        .unwrap();

    assert_eq!(kept, vec![1, 2, 4]);
}

// ---------------------------------------------------------------------------
// Savepoint scope: what a rolled-back nested transaction leaves behind
// ---------------------------------------------------------------------------

/// Rolling a savepoint back must end its scope on the server too.
///
/// `ROLLBACK TO SAVEPOINT x` undoes the work but LEAVES `x` defined - the
/// documented behaviour, and the reason "roll back to it again later" is a
/// thing you can do. `Transaction::rollback` issued only that, so a savepoint
/// whose Rust value had been consumed stayed on the server's savepoint stack.
///
/// PostgreSQL resolves a savepoint name to the most recently established one,
/// so the leftover shadows an enclosing savepoint of the same name and the
/// enclosing `rollback` rolls back to the INNER scope: the outer statements it
/// was supposed to discard survive, and then commit. Two nesting levels
/// naming one savepoint is what this test builds, because that is where the
/// leftover changes an outcome rather than merely accumulating.
#[compio::test]
async fn a_rolled_back_savepoint_does_not_shadow_an_enclosing_one() {
    let url = require_pg().await;
    let mut client = connect(&url).await.unwrap();
    let table = common::test_object_name("savepoint_scope");

    client
        .batch_execute(&format!("CREATE TABLE {table} (n int)"))
        .await
        .unwrap();

    {
        let mut tx = client.transaction().await.unwrap();
        {
            let mut outer = tx.savepoint("s").await.unwrap();
            outer
                .execute(&format!("INSERT INTO {table} VALUES (1)"), &[])
                .await
                .unwrap();
            {
                let inner = outer.savepoint("s").await.unwrap();
                inner
                    .execute(&format!("INSERT INTO {table} VALUES (2)"), &[])
                    .await
                    .unwrap();
                inner.rollback().await.unwrap();
            }
            outer.rollback().await.unwrap();
        }
        tx.commit().await.unwrap();
    }

    let rows = client
        .query(&format!("SELECT n FROM {table} ORDER BY n"), &[])
        .await
        .unwrap();
    let kept: Vec<i32> = rows.iter().map(|r| r.get::<_, i32>(0)).collect();
    assert!(
        kept.is_empty(),
        "the outer rollback rolled back to the inner savepoint and kept {kept:?}"
    );

    client
        .batch_execute(&format!("DROP TABLE {table}"))
        .await
        .unwrap();
}

/// The same leftover, asserted directly: after `rollback`, the savepoint the
/// transaction owned is gone from the server.
///
/// Without this the scope test above could be satisfied by anything that
/// happens to reorder the names. `3B001 invalid_savepoint_specification` is
/// the server saying the name is no longer defined.
#[compio::test]
async fn a_rolled_back_savepoint_is_no_longer_defined() {
    let url = require_pg().await;
    let mut client = connect(&url).await.unwrap();

    let mut tx = client.transaction().await.unwrap();
    {
        let sp = tx.savepoint("s").await.unwrap();
        sp.rollback().await.unwrap();
    }

    let err = tx
        .batch_execute("ROLLBACK TO SAVEPOINT s")
        .await
        .expect_err("the savepoint must not still be defined");
    assert_eq!(
        err.code(),
        Some(&SqlState::S_E_INVALID_SPECIFICATION),
        "expected the server to report an undefined savepoint, got: {err}"
    );
}

/// The control for both: a savepoint rollback still recovers a transaction
/// whose statement failed, the enclosing transaction stays usable, and a
/// plain (savepoint-free) rollback still discards its work.
///
/// Recovering from a failed statement is what savepoints are FOR, and it is
/// the case a too-strict fix breaks: the subtransaction is in an aborted
/// state when `rollback` runs, so anything issued before `ROLLBACK TO` - a
/// `RELEASE`, say - is refused by the server and the recovery fails. The
/// plain-rollback half fails if the same treatment is applied to a
/// transaction that owns no savepoint.
#[compio::test]
async fn a_savepoint_rollback_recovers_a_failed_statement() {
    let url = require_pg().await;
    let mut client = connect(&url).await.unwrap();
    let table = common::test_object_name("savepoint_recovery");

    client
        .batch_execute(&format!("CREATE TABLE {table} (n int)"))
        .await
        .unwrap();

    {
        let mut tx = client.transaction().await.unwrap();
        tx.execute(&format!("INSERT INTO {table} VALUES (1)"), &[])
            .await
            .unwrap();
        {
            let sp = tx.savepoint("attempt").await.unwrap();
            sp.execute(&format!("INSERT INTO {table} VALUES ('not an int')"), &[])
                .await
                .expect_err("the statement must fail and abort the subtransaction");
            sp.rollback()
                .await
                .expect("rolling back an aborted subtransaction must recover it");
        }
        tx.execute(&format!("INSERT INTO {table} VALUES (3)"), &[])
            .await
            .expect("the enclosing transaction must be usable again");
        tx.commit().await.unwrap();
    }

    let rows = client
        .query(&format!("SELECT n FROM {table} ORDER BY n"), &[])
        .await
        .unwrap();
    let kept: Vec<i32> = rows.iter().map(|r| r.get::<_, i32>(0)).collect();
    assert_eq!(kept, vec![1, 3]);

    // A transaction that owns no savepoint still rolls back as one unit.
    {
        let tx = client.transaction().await.unwrap();
        tx.execute(&format!("INSERT INTO {table} VALUES (4)"), &[])
            .await
            .unwrap();
        tx.rollback()
            .await
            .expect("a savepoint-free transaction still rolls back");
    }

    let rows = client
        .query(&format!("SELECT n FROM {table} ORDER BY n"), &[])
        .await
        .unwrap();
    let kept: Vec<i32> = rows.iter().map(|r| r.get::<_, i32>(0)).collect();
    assert_eq!(kept, vec![1, 3]);

    client
        .batch_execute(&format!("DROP TABLE {table}"))
        .await
        .unwrap();
}

/// The control for `commit`, which the same pairing rules govern: releasing a
/// savepoint keeps its work and hands it to the enclosing transaction.
#[compio::test]
async fn a_committed_savepoint_keeps_its_work() {
    let url = require_pg().await;
    let mut client = connect(&url).await.unwrap();
    let table = common::test_object_name("savepoint_commit");

    client
        .batch_execute(&format!("CREATE TABLE {table} (n int)"))
        .await
        .unwrap();

    {
        let mut tx = client.transaction().await.unwrap();
        {
            let sp = tx.savepoint("keep").await.unwrap();
            sp.execute(&format!("INSERT INTO {table} VALUES (1)"), &[])
                .await
                .unwrap();
            sp.commit().await.unwrap();
        }
        tx.commit().await.unwrap();
    }

    let rows = client
        .query(&format!("SELECT n FROM {table}"), &[])
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);

    client
        .batch_execute(&format!("DROP TABLE {table}"))
        .await
        .unwrap();
}

// ---------------------------------------------------------------------------
// 19. error_recovery_in_transaction
// ---------------------------------------------------------------------------

#[compio::test]
async fn error_recovery_in_transaction() {
    let url = require_pg().await;
    let client = connect(&url).await.unwrap();

    create_complex_table(&client).await;

    // BEGIN
    client.execute("BEGIN", &[]).await.unwrap();

    // INSERT (succeeds)
    client
        .execute(
            &format!("INSERT INTO {COMPLEX_TABLE} (name, value) VALUES ($1, $2)"),
            &[&"good_row", &1i64],
        )
        .await
        .unwrap();

    // INSERT with duplicate primary key - triggers unique_violation
    let err = client
        .execute(
            &format!("INSERT INTO {COMPLEX_TABLE} (id, name) VALUES (1, $1)"),
            &[&"dup_id"],
        )
        .await;

    // The insert should fail
    assert!(err.is_err(), "expected constraint violation");

    // ROLLBACK to recover. The server is in a failed-transaction state;
    // any query other than ROLLBACK/COMMIT errors with SQLSTATE 25P02
    // ("in_failed_sql_transaction"). ROLLBACK always succeeds.
    client.execute("ROLLBACK", &[]).await.unwrap();

    // Verify connection is usable again
    let rows = client.query("SELECT 1 as ok", &[]).await.unwrap();
    assert_eq!(rows[0].get::<_, i32>("ok"), 1);

    // Verify the good_row was rolled back too
    let rows = client
        .query(
            &format!("SELECT name FROM {COMPLEX_TABLE} WHERE name = $1"),
            &[&"good_row"],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 0);

    drop_complex_table(&client).await;
}

// ---------------------------------------------------------------------------
// 20. null_in_params
// ---------------------------------------------------------------------------

#[compio::test]
async fn null_in_params() {
    let url = require_pg().await;
    let client = connect(&url).await.unwrap();

    let rows = client
        .query("SELECT $1::text as val", &[&None::<&str>])
        .await
        .unwrap();

    assert_eq!(rows.len(), 1);
    let val: Option<&str> = rows[0].get("val");
    assert!(val.is_none(), "expected NULL, got {val:?}");
}

// ---------------------------------------------------------------------------
// 21. pool_exhaustion
// ---------------------------------------------------------------------------

#[compio::test]
async fn pool_exhaustion() {
    let url = require_pg().await;
    // max_size=2 with a very short acquire_timeout, so the test does not
    // wait 30 s to observe the exhaustion error.
    //
    // `min_idle: 2`, NOT 0, and that is load-bearing. `acquire_timeout`
    // bounds the WHOLE of `get()` - opening a connection as well as waiting for
    // one - so with an empty pool the first two acquisitions had to complete a
    // TCP connect, a startup exchange and SCRAM-SHA-256 (4096 PBKDF2 rounds, in
    // a debug build) inside the same 200 ms budget meant for the exhaustion
    // wait. That made a test about CAPACITY fail on a busy machine because of
    // LATENCY: observed once at 74 s of suite time under load, and passing 3/3
    // in isolation on the same commit.
    //
    // Warming both connections up front removes the unrelated variable. The two
    // acquisitions below now come from `idle` and open no sockets, so the only
    // thing the 200 ms budget times is the third `get()`, which is what the
    // test is named after.
    let mut config = compio_postgres::PoolConfig::new();
    config
        .max_size(2)
        .min_idle(2)
        .acquire_timeout(std::time::Duration::from_millis(200));
    let pool = Pool::connect_with_pool_config(&url, config).await.unwrap();
    assert_eq!(
        pool.idle_count(),
        2,
        "warm-up must fill the pool, or the acquisitions below are timing a connect"
    );

    // Acquire 2 connections without returning them
    let _c1 = pool.get().await.unwrap();
    let _c2 = pool.get().await.unwrap();

    // Third acquisition should fail with connection timeout (pool exhausted).
    let err = pool.get().await;
    match err {
        Err(e) => {
            // The pool wraps its timeout error in Error::connect; check that
            // the message mentions timeout/pool so we know it's not some other
            // unrelated failure.
            let msg = format!("{e}");
            assert!(
                msg.contains("connect") || msg.contains("timeout") || msg.contains("pool"),
                "expected pool exhaustion error, got: {e}"
            );
        }
        Ok(_) => panic!("expected pool exhaustion error, but got a connection"),
    }
}

// ---------------------------------------------------------------------------
// 22. returning_clause
// ---------------------------------------------------------------------------

#[compio::test]
async fn returning_clause() {
    let url = require_pg().await;
    let client = connect(&url).await.unwrap();

    create_complex_table(&client).await;

    // INSERT with RETURNING
    let rows = client
        .query(
            &format!(
                "INSERT INTO {COMPLEX_TABLE} (name, value) VALUES ($1, $2) RETURNING id, value"
            ),
            &[&"ret_test", &42i64],
        )
        .await
        .unwrap();

    assert_eq!(rows.len(), 1);
    let id: i32 = rows[0].get("id");
    let value: i64 = rows[0].get("value");
    assert!(id > 0, "expected positive id, got {id}");
    assert_eq!(value, 42);

    drop_complex_table(&client).await;
}

// ---------------------------------------------------------------------------
// 23. update_with_returning
// ---------------------------------------------------------------------------

#[compio::test]
async fn update_with_returning() {
    let url = require_pg().await;
    let client = connect(&url).await.unwrap();

    create_complex_table(&client).await;

    // Insert a row with value=10
    client
        .execute(
            &format!("INSERT INTO {COMPLEX_TABLE} (name, value) VALUES ($1, $2)"),
            &[&"upd_test", &10i64],
        )
        .await
        .unwrap();

    // UPDATE with RETURNING
    let rows = client
        .query(
            &format!(
                "UPDATE {COMPLEX_TABLE} SET value = value + 1 WHERE name = $1 RETURNING value"
            ),
            &[&"upd_test"],
        )
        .await
        .unwrap();

    assert_eq!(rows.len(), 1);
    let value: i64 = rows[0].get("value");
    assert_eq!(value, 11);

    drop_complex_table(&client).await;
}

// ---------------------------------------------------------------------------
// 24. get_cancellation_during_connect_does_not_leak_permits (POOL-1)
//
// Regression: `Pool::get` wraps `get_inner` in `compio::time::timeout`, a
// `select!` that DROPS the inner future when the timer wins. On the on-demand
// connect path the capacity permit is the hand-maintained `total` counter,
// incremented before `connect_one().await` and decremented only by the
// `Err(_)` match arm. When the get() future is cancelled mid-connect, neither
// arm runs, so the `+1` is never undone -> `total` is permanently inflated.
// After `max_size` such cancellations the create-gate (`total < max_size`)
// never fires again and every get() times out: the pool is bricked.
//
// Cancellation here is driven DETERMINISTICALLY by dropping the get() future
// mid-connect (the same mechanism `timeout` uses, and what test 26 does for
// the waiter path), NOT by racing a 1 ms timeout against a real connect. On
// the cooperative single-threaded runtime we poll a fresh get() future until
// it has taken the on-demand path and reserved its permit (total_count() == 2:
// past the `total < max_size` gate, parked in `connect_one().await`), then
// drop it. Pre-fix the reserved `+1` leaks; post-fix the `PermitGuard` Drop
// releases it (back to 1). No timing assumption -> no flakiness on fast hosts.
// ---------------------------------------------------------------------------

#[compio::test]
async fn get_cancellation_during_connect_does_not_leak_permits() {
    use futures_util::poll;

    let url = require_pg().await;
    let mut config = compio_postgres::PoolConfig::new();
    config
        .max_size(4)
        .min_idle(0)
        // Normal timeout: cancellation is driven by dropping the future, not by
        // the timer firing, so this value is irrelevant to the race (and safely
        // long so a genuine acquire never times out).
        .acquire_timeout(std::time::Duration::from_secs(30))
        // Large, so acquiring the already-warm connection never does a network
        // round-trip (no validation / dirty barrier).
        .validation_bypass(std::time::Duration::from_secs(60));
    let pool = Pool::connect_with_pool_config(&url, config).await.unwrap();

    // The warm-up opened exactly one connection (min_idle=0 -> warm = max(0,1)
    // = 1). Hold it so `idle` is empty and every further get() must take the
    // on-demand connect path.
    let c1 = pool.get().await.expect("warm connection acquires locally");
    assert_eq!(
        pool.total_count(),
        1,
        "warm-up should open exactly one conn"
    );

    // Drive many cancelled on-demand connects. Each reserves a permit then is
    // dropped while parked in `connect_one().await`.
    for i in 0..100 {
        // Box::pin so an explicit `drop(fut)` genuinely drops the future (the
        // cancellation), not just a `Pin<&mut _>` borrow of it.
        let mut fut = Box::pin(pool.get());

        // Poll until the on-demand connect has reserved its permit. The first
        // poll pops idle (empty), passes the `total < max_size` gate, reserves
        // (`total` -> 2) and parks in the TCP connect (Pending); bound the
        // spins so a regression that resolves synchronously fails loudly
        // instead of hanging.
        let mut spins = 0;
        loop {
            match poll!(fut.as_mut()) {
                std::task::Poll::Pending => {}
                std::task::Poll::Ready(_) => {
                    panic!("on-demand connect resolved instead of parking (iter {i})")
                }
            }
            if pool.total_count() == 2 {
                break;
            }
            spins += 1;
            assert!(
                spins < 1000,
                "on-demand connect never reserved its permit (iter {i}, \
                 total_count()={})",
                pool.total_count()
            );
            // Let the runtime advance the parked connect's submission.
            yield_n(1).await;
        }
        assert_eq!(
            pool.total_count(),
            2,
            "permit must be reserved while the connect is in flight (iter {i})"
        );

        // Drop the in-flight get() future -> cancellation. The PermitGuard's
        // Drop must release the reserved permit.
        drop(fut);

        assert_eq!(
            pool.total_count(),
            1,
            "cancelled on-demand connect leaked its permit (iter {i}): \
             total_count()={}",
            pool.total_count()
        );
    }

    // Primary invariant: after 100 deterministic cancellations only the
    // genuinely-held warm connection (c1) remains counted. Pre-fix this climbs
    // to max_size (4) and sticks.
    assert_eq!(
        pool.total_count(),
        1,
        "cancelled on-demand connects leaked permits: total_count()={}",
        pool.total_count()
    );

    // Liveness: the pool must not be bricked. Returning c1 makes a warm idle
    // entry available; a fresh get() must reclaim it locally and succeed.
    drop(c1);
    let c2 = pool
        .get()
        .await
        .expect("pool bricked: get() fails after cancellations even with an idle conn");
    drop(c2);
    assert_eq!(
        pool.total_count(),
        1,
        "post-recovery total_count() should still be 1, got {}",
        pool.total_count()
    );
}

// ---------------------------------------------------------------------------
// 25. freed_connection_goes_to_front_waiter_not_a_barging_fresh_caller (POOL-2)
//
// FIFO fairness regression. When a `PooledClient` drops, the freed entry must
// go to the connection that has been queued LONGEST (the front parked waiter),
// not to a fresh caller that wanders in afterwards.
//
// Pre-fix `return_client` pushed the freed entry onto the shared `idle` vec and
// only *advisory-woke* the front waiter. A fresh caller entering `get_inner`
// between the wake and the woken waiter's re-poll pops the idle entry first -
// barging ahead of the longer-queued waiter. Under load the parked waiter is
// repeatedly barged -> starvation.
//
// Deterministic on the single-threaded cooperative runtime: control only moves
// at `.await` points, so we can drive the exact interleaving that exposes the
// barge.
//
// Setup: max_size=1 (a single slot). Hold c1. Spawn task A which parks as the
// sole waiter. Drop c1 (frees the slot, targeting waiter A). BEFORE yielding to
// A, a fresh caller C in the main task calls get(). Pre-fix C synchronously
// pops the idle entry and wins -> order is ['C', 'A']. Post-fix the freed entry
// went straight into A's slot (not idle), so C finds nothing, parks behind A,
// and A acquires first -> order is ['A', 'C'].
// ---------------------------------------------------------------------------

/// A future that yields control to the scheduler exactly `n` times, then
/// resolves. Each yield returns `Pending` after self-waking, so other ready
/// tasks (e.g. a just-woken pool waiter) get a chance to run before this task
/// is polled again. Used to step the cooperative single-threaded runtime
/// deterministically.
struct YieldNow(u32);
impl std::future::Future for YieldNow {
    type Output = ();
    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<()> {
        if self.0 == 0 {
            std::task::Poll::Ready(())
        } else {
            self.0 -= 1;
            cx.waker().wake_by_ref();
            std::task::Poll::Pending
        }
    }
}

async fn yield_n(n: u32) {
    YieldNow(n).await;
}

#[compio::test]
async fn freed_connection_goes_to_front_waiter_not_a_barging_fresh_caller() {
    use std::cell::RefCell;
    use std::rc::Rc;

    let url = require_pg().await;

    // Single slot so "the freed connection" is unambiguous, and the only way a
    // fresh caller can get one is by intercepting the entry freed for waiter A.
    let mut config = compio_postgres::PoolConfig::new();
    config
        .max_size(1)
        .min_idle(0)
        .acquire_timeout(std::time::Duration::from_secs(5))
        // Large so reacquiring the warm entry never does a network round-trip
        // (no validation / dirty barrier) - keeps the interleaving synchronous
        // and deterministic.
        .validation_bypass(std::time::Duration::from_secs(60));
    // Warm-up opens exactly one connection (min_idle=0 -> warm = max(0,1) = 1).
    let pool = Rc::new(Pool::connect_with_pool_config(&url, config).await.unwrap());
    assert_eq!(
        pool.total_count(),
        1,
        "warm-up should open exactly one conn"
    );

    // Acquire and hold the only slot. idle now empty, pool full.
    let c1 = pool.get().await.expect("warm connection acquires locally");
    assert_eq!(pool.idle_count(), 0);
    assert_eq!(pool.active_count(), 1);
    assert_eq!(pool.total_count(), 1);

    // Records who acquired the connection, in order.
    let order: Rc<RefCell<Vec<char>>> = Rc::new(RefCell::new(Vec::new()));
    // Set by task A right before it awaits get(), so the main task can confirm
    // A actually reached the parking point before proceeding.
    let a_reached_get = Rc::new(std::cell::Cell::new(false));

    // Task A: the front (and initially only) waiter. It parks waiting for the
    // single slot.
    let a_handle = {
        let pool = Rc::clone(&pool);
        let order = Rc::clone(&order);
        let a_reached_get = Rc::clone(&a_reached_get);
        compio::runtime::spawn(async move {
            a_reached_get.set(true);
            let c = pool
                .get()
                .await
                .expect("waiter A must obtain the freed conn");
            order.borrow_mut().push('A');
            // Hold briefly, then release so a later waiter (C) can proceed.
            yield_n(2).await;
            drop(c);
        })
    };

    // Let A run until it parks as a waiter. A sets `a_reached_get` then awaits
    // get(), whose Waiter::poll registers a slot and returns Pending. Yield
    // until the waiter is registered (defensive loop with a bounded ceiling so
    // a regression can't hang the suite).
    let mut spins = 0;
    while pool.pending_count() == 0 {
        yield_n(1).await;
        spins += 1;
        assert!(spins < 1000, "task A never parked as a waiter");
    }
    assert!(a_reached_get.get(), "task A should have reached get()");
    assert_eq!(pool.pending_count(), 1, "exactly one parked waiter (A)");
    assert_eq!(pool.idle_count(), 0);

    // Free the only slot. This targets the front waiter A. Pre-fix the entry is
    // pushed to `idle` (and A is advisory-woken); post-fix it is deposited into
    // A's slot and A is woken, with nothing left in `idle`.
    drop(c1);

    // CRITICAL: before yielding to A, a FRESH caller C (never parked) tries to
    // acquire. Pre-fix this synchronously pops the idle entry and barges A.
    let c = pool.get().await;
    order.borrow_mut().push('C');
    drop(c);

    // Drain: let A (and anything else) finish.
    a_handle
        .await
        .unwrap_or_else(|e| std::panic::resume_unwind(e));

    // The longest-queued waiter (A) must have won the freed connection first.
    // Pre-fix: ['C', 'A'] (fresh caller barged). Post-fix: ['A', 'C'].
    assert_eq!(
        *order.borrow(),
        vec!['A', 'C'],
        "front waiter A must receive the freed connection before a fresh caller C; \
         got {:?} (a value of ['C','A'] means the fresh caller barged the parked waiter)",
        *order.borrow()
    );

    // Pool is back to a clean single-connection state.
    assert_eq!(pool.active_count(), 0, "all clients released");
    assert_eq!(pool.total_count(), 1, "no connection lost or leaked");
    assert_eq!(pool.idle_count(), 1, "the one connection is idle");
    assert_eq!(pool.pending_count(), 0, "no waiters left");
}

// ---------------------------------------------------------------------------
// 26. handed_off_connection_is_reclaimed_if_waiter_is_cancelled (POOL-2 edge)
//
// The dangerous edge case of the direct hand-off: `return_client` deposits a
// freed connection into the front waiter's slot and wakes it - but if that
// waiter's `get()` future is DROPPED/cancelled before it polls the entry out,
// the connection must be re-homed (back to `idle`, or to the next live waiter),
// NOT lost. A leaked entry here would be a worse bug than the unfairness we are
// fixing.
//
// Accounting: the reclaim must NOT decrement `active` (it was never incremented
// for this waiter - `active += 1` happens only when a waiter actually takes the
// entry) and must NOT decrement `total` (the connection is still alive).
//
// In compio, dropping a task's `JoinHandle` (instead of `.detach()`-ing it)
// cancels the task: its future is dropped without further polling. So we drop
// A's handle AFTER the connection lands in A's slot but BEFORE A is polled to
// take it - driving exactly the cancel-with-stranded-entry path.
// ---------------------------------------------------------------------------

#[compio::test]
async fn handed_off_connection_is_reclaimed_if_waiter_is_cancelled() {
    use std::rc::Rc;

    let url = require_pg().await;

    let mut config = compio_postgres::PoolConfig::new();
    config
        .max_size(1)
        .min_idle(0)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .validation_bypass(std::time::Duration::from_secs(60));
    let pool = Rc::new(Pool::connect_with_pool_config(&url, config).await.unwrap());
    assert_eq!(pool.total_count(), 1);

    // Hold the only slot.
    let c1 = pool.get().await.expect("warm connection acquires locally");
    assert_eq!(pool.idle_count(), 0);
    assert_eq!(pool.active_count(), 1);

    // Task A parks as the sole waiter. If it is ever polled after a hand-off it
    // would take the entry and bump `active`; we cancel it before that happens.
    let a_handle = {
        let pool = Rc::clone(&pool);
        compio::runtime::spawn(async move {
            let _c = pool.get().await.expect("(unreached) A is cancelled first");
            // Hold forever-ish if somehow reached, so a bug is visible.
            yield_n(1_000_000).await;
        })
    };

    // Let A park.
    let mut spins = 0;
    while pool.pending_count() == 0 {
        yield_n(1).await;
        spins += 1;
        assert!(spins < 1000, "task A never parked as a waiter");
    }
    assert_eq!(pool.pending_count(), 1, "exactly one parked waiter (A)");

    // Free the slot: the entry is deposited DIRECTLY into A's slot (not idle),
    // A is woken (scheduled), and A is popped from the wait queue.
    drop(c1);
    assert_eq!(
        pool.idle_count(),
        0,
        "freed entry went to A's slot, not idle"
    );
    assert_eq!(pool.active_count(), 0, "no active client during hand-off");
    assert_eq!(pool.total_count(), 1, "connection still alive");
    assert_eq!(pool.pending_count(), 0, "A popped from queue on hand-off");

    // Cancel A BEFORE it is polled to take the entry. Dropping the JoinHandle
    // cancels the task; its future (holding the Waiter) is dropped, firing
    // Waiter::drop -> reclaim of the stranded entry. The cancellation runnable
    // executes on a scheduler turn, so yield until the reclaim lands (bounded).
    drop(a_handle);
    let mut spins = 0;
    while pool.idle_count() == 0 {
        yield_n(1).await;
        spins += 1;
        assert!(
            spins < 1000,
            "reclaim never happened - the handed-off connection was LEAKED \
             (idle={}, active={}, total={})",
            pool.idle_count(),
            pool.active_count(),
            pool.total_count()
        );
    }

    // The reclaimed connection is back in idle, with accounting intact: not
    // active (the cancelled waiter never took it), not lost from total.
    assert_eq!(pool.idle_count(), 1, "reclaimed entry returned to idle");
    assert_eq!(
        pool.active_count(),
        0,
        "reclaim must NOT have bumped/left active set"
    );
    assert_eq!(
        pool.total_count(),
        1,
        "reclaim must NOT drop the connection from total"
    );
    assert_eq!(pool.pending_count(), 0, "no waiters left");

    // And the reclaimed connection is fully usable: a fresh get() reclaims it
    // locally (no network round-trip under the 60 s bypass) and runs a query.
    let c = pool.get().await.expect("reclaimed connection is reusable");
    assert_eq!(
        pool.active_count(),
        1,
        "fresh caller took the reclaimed conn"
    );
    let rows = c.query("SELECT 7::int4 AS v", &[]).await.unwrap();
    assert_eq!(rows[0].get::<_, i32>("v"), 7);
    drop(c);
    assert_eq!(pool.active_count(), 0);
    assert_eq!(
        pool.total_count(),
        1,
        "no connection lost or leaked overall"
    );
}

// ---------------------------------------------------------------------------
// 27. notify_delivered_on_idle_listener (IO-2)
//
// A pure-listener connection must receive LISTEN/NOTIFY notifications without
// issuing any further query of its own. The serialized run-loop only ever
// reads the socket while a request response is in flight or after the client
// sends a new request; an idle listener never reads, so a NOTIFY arriving on
// the wire sits unread in the kernel buffer forever.
//
// Pre-fix: the timeout fires (notification never delivered) -> RED.
// Post-fix: the multiplexed loop carries a read future even while idle, so the
// NotificationResponse is read and routed to the async channel promptly -> the
// receive completes within the timeout -> GREEN.
// ---------------------------------------------------------------------------

#[compio::test]
async fn notify_delivered_on_idle_listener() {
    use futures_util::StreamExt;

    let url = require_pg().await;

    // Listener connection A. Register the async-message sink BEFORE spawning
    // run(), then LISTEN on a channel.
    let (client_a, mut conn_a) = compio_postgres::connect(&url, common::suite_tls())
        .await
        .unwrap();
    let mut notifications = conn_a.notifications();
    compio::runtime::spawn(async move {
        if let Err(e) = conn_a.run().await {
            eprintln!("listener connection error: {e}");
        }
    })
    .detach();

    // Unique channel name so concurrent test runs don't cross-deliver. The
    // shared helper owns the process suffix and identifier-length rule.
    let chan = common::test_object_name("zs_notify_test");
    client_a
        .batch_execute(&format!("LISTEN {chan}"))
        .await
        .unwrap();

    // Notifier connection B fires the NOTIFY. A issues NO further query after
    // its LISTEN - the notification must arrive purely from A's idle read.
    let client_b = connect(&url).await.unwrap();
    client_b
        .batch_execute(&format!("NOTIFY {chan}, 'hello-from-b'"))
        .await
        .unwrap();

    // The crux: wait for A's async receiver to yield WITHOUT A querying again.
    // Wrap in a timeout so a non-delivering (serialized) loop fails fast as a
    // timeout rather than hanging the suite.
    let received = compio::time::timeout(std::time::Duration::from_secs(5), notifications.next())
        .await
        .expect(
            "IO-2: idle listener never received the notification within 5s \
             (serialized loop does not read while idle)",
        );

    match received {
        Some(compio_postgres::AsyncMessage::Notification(n)) => {
            assert_eq!(n.channel(), chan, "notification arrived on wrong channel");
            assert_eq!(n.payload(), "hello-from-b", "wrong notification payload");
        }
        other => panic!("expected a Notification, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// 28. copy_in_error_does_not_deadlock (COPY-1 / IO-1)
//
// On a large COPY FROM STDIN where the server rejects rows mid-stream (here a
// NOT NULL violation), the server emits an ErrorResponse and stops draining.
// Its receive buffer fills; meanwhile the client keeps writing COPY frames.
// The serialized loop never reads while streaming COPY frames, so both sides
// block on a full socket buffer -> permanent deadlock; the copy future never
// returns and the connection never goes back to the pool.
//
// Pre-fix: the whole copy (send + finish) deadlocks -> the timeout fires -> RED.
// Post-fix: the multiplexed loop reads the ErrorResponse concurrently with the
// writes, so finish() returns Err(DbError) promptly -> GREEN. We assert it is
// an ERROR (not a timeout): correctness is "surface the failure", not "hang".
// ---------------------------------------------------------------------------

#[compio::test]
async fn copy_in_error_does_not_deadlock() {
    use bytes::Bytes;
    use futures_util::SinkExt;
    use std::pin::pin;

    let url = require_pg().await;
    let client = connect(&url).await.unwrap();
    let table = common::test_object_name("cpg_copy_rejected");

    client
        .execute(
            &format!("CREATE TEMPORARY TABLE {table} (id int, n int)"),
            &[],
        )
        .await
        .unwrap();

    // Stream a large text-COPY body that the server rejects. The first row is
    // a PARSE error ("notanint" is not valid for `n int`); the server reports
    // it with an ErrorResponse. We then keep streaming a large volume of
    // further rows so the client is still writing long after the server has
    // produced its error and stopped draining - exactly the condition that
    // wedges the serialized loop (server's send buffer fills with the
    // ErrorResponse while the client floods; both block). PG buffers a lot of
    // COPY input before surfacing the error, so the volume must be large
    // (~hundreds of KB) to exceed the socket buffers.
    //
    // `feed` (not `send`) is used for the bulk rows: `send` force-flushes a
    // CopyData frame per call, while `feed` lets `CopyInSink` batch into ~4 KB
    // frames - without it, this is hundreds of thousands of tiny io_uring
    // writes and the test is dominated by syscall latency rather than the
    // deadlock it is meant to probe.
    let copy_fut = async {
        let sink = client
            .copy_in::<_, Bytes>(&format!("COPY {table} (id, n) FROM STDIN"))
            .await?;
        let mut sink = pin!(sink);

        sink.feed(Bytes::from_static(b"1\tnotanint\n")).await?;
        for i in 0..200_000i64 {
            sink.feed(Bytes::from(format!("{i}\t{i}\n"))).await?;
        }
        sink.finish().await
    };

    // A deadlock manifests as the copy future never completing. Bound it
    // generously - the multiplexed loop completes well within this, while the
    // serialized loop wedges forever (RED-proven).
    let outcome = compio::time::timeout(std::time::Duration::from_secs(15), copy_fut).await;

    match outcome {
        Err(_) => panic!(
            "COPY-1: copy_in deadlocked (timed out) - the loop never read the \
             server's ErrorResponse while streaming COPY frames"
        ),
        Ok(Ok(rows)) => panic!(
            "expected the COPY parse error to surface, but the copy succeeded \
             with {rows} rows"
        ),
        Ok(Err(e)) => {
            assert_eq!(
                e.code(),
                Some(&SqlState::INVALID_TEXT_REPRESENTATION),
                "the COPY parse error lost its server SQLSTATE: {}",
                common::error_chain(&e),
            );
        }
    }

    // Recovery is a property of this protocol session, not of the server as a
    // whole. Reuse the exact connection that PostgreSQL rejected mid-COPY.
    let rows = client
        .query(&format!("SELECT count(*)::int8 AS c FROM {table}"), &[])
        .await
        .expect("the rejected COPY poisoned its connection");
    assert_eq!(
        rows[0].get::<_, i64>("c"),
        0,
        "a rejected COPY must leave no rows committed"
    );
    client
        .execute(&format!("DROP TABLE {table}"), &[])
        .await
        .unwrap();
}

/// Dropping an unfinished COPY IN must abort that COPY and resynchronize the
/// same protocol session before the next request is answered.
#[compio::test]
async fn dropped_copy_in_sink_recovers_the_same_connection() {
    use bytes::Bytes;
    use futures_util::SinkExt;

    let url = require_pg().await;
    let client = connect(&url).await.unwrap();
    let table = common::test_object_name("cpg_copy_drop_in");

    client
        .execute(&format!("CREATE TEMPORARY TABLE {table} (n int)"), &[])
        .await
        .unwrap();

    {
        let sink = client
            .copy_in::<_, Bytes>(&format!("COPY {table} (n) FROM STDIN"))
            .await
            .unwrap();
        let mut sink = Box::pin(sink);
        sink.as_mut()
            .send(Bytes::from_static(b"1\n2\n"))
            .await
            .unwrap();
        // No finish: CopyInReceiver must turn this drop into CopyFail + Sync.
    }

    let rows = compio::time::timeout(
        std::time::Duration::from_secs(5),
        client.query(&format!("SELECT count(*)::int8 AS n FROM {table}"), &[]),
    )
    .await
    .expect("the query after dropping CopyInSink timed out")
    .expect("dropping CopyInSink poisoned its connection");
    assert_eq!(rows[0].get::<_, i64>("n"), 0);

    client
        .execute(&format!("DROP TABLE {table}"), &[])
        .await
        .unwrap();
}

#[compio::test]
async fn panicking_copy_input_does_not_leak_partial_bytes_into_the_next_item() {
    use bytes::{Buf, Bytes};
    use futures_util::{FutureExt, SinkExt};
    use std::panic::AssertUnwindSafe;

    enum ScriptedBuf {
        PanicAfterChunk { advanced: bool },
        Good(Bytes),
    }

    impl Buf for ScriptedBuf {
        fn remaining(&self) -> usize {
            match self {
                Self::PanicAfterChunk { advanced } => usize::from(!advanced) * b"stale\n".len(),
                Self::Good(bytes) => bytes.remaining(),
            }
        }

        fn chunk(&self) -> &[u8] {
            match self {
                Self::PanicAfterChunk { advanced: false } => b"stale\n",
                Self::PanicAfterChunk { advanced: true } => b"",
                Self::Good(bytes) => bytes.chunk(),
            }
        }

        fn advance(&mut self, count: usize) {
            match self {
                Self::PanicAfterChunk { advanced } => {
                    assert_eq!(count, b"stale\n".len());
                    *advanced = true;
                    panic!("scripted Buf panic after its chunk was copied");
                }
                Self::Good(bytes) => bytes.advance(count),
            }
        }
    }

    compio::time::timeout(std::time::Duration::from_secs(10), async {
        let url = require_pg().await;
        let client = connect(&url).await.unwrap();
        let table = common::test_object_name("cpg_copy_panicking_buf");
        client
            .batch_execute(&format!("CREATE TEMP TABLE {table} (value text NOT NULL)"))
            .await
            .unwrap();
        let copy = format!("COPY {table} (value) FROM STDIN");

        let sink = client.copy_in::<_, ScriptedBuf>(&copy).await.unwrap();
        let mut sink = Box::pin(sink);
        sink.as_mut()
            .feed(ScriptedBuf::Good(Bytes::from_static(b"prefix\n")))
            .await
            .expect("seed the COPY buffer before the panicking item");

        let panic = AssertUnwindSafe(
            sink.as_mut()
                .feed(ScriptedBuf::PanicAfterChunk { advanced: false }),
        )
        .catch_unwind()
        .await;
        assert!(panic.is_err(), "the scripted input did not panic");

        sink.as_mut()
            .send(ScriptedBuf::Good(Bytes::from_static(b"clean\n")))
            .await
            .expect("send the item after the panic");
        sink.as_mut().finish().await.expect("finish COPY input");

        let query = format!("SELECT value FROM {table} ORDER BY ctid");
        let values = client
            .query(&query, &[])
            .await
            .expect("read rows after the panicking COPY item")
            .iter()
            .map(|row| row.get::<_, &str>(0).to_string())
            .collect::<Vec<_>>();
        assert_eq!(values, ["prefix", "clean"]);
    })
    .await
    .expect("panicking COPY input test exceeded its watchdog");
}

#[compio::test]
async fn failed_or_panicking_large_copy_input_preserves_its_buffered_predecessor() {
    use bytes::{Buf, Bytes};
    use futures_util::{FutureExt, SinkExt};
    use std::cell::Cell;
    use std::panic::AssertUnwindSafe;

    enum ScriptedBuf {
        LengthOverflow,
        PanicWhileFraming { remaining_calls: Cell<usize> },
        Good(Bytes),
    }

    impl Buf for ScriptedBuf {
        fn remaining(&self) -> usize {
            match self {
                Self::LengthOverflow => usize::MAX,
                Self::PanicWhileFraming { remaining_calls } => {
                    let call = remaining_calls.get();
                    remaining_calls.set(call + 1);
                    if call == 0 {
                        4097
                    } else {
                        panic!("scripted Buf panic while framing a chained COPY item");
                    }
                }
                Self::Good(bytes) => bytes.remaining(),
            }
        }

        fn chunk(&self) -> &[u8] {
            match self {
                Self::LengthOverflow | Self::PanicWhileFraming { .. } => {
                    unreachable!("the scripted failure happens before COPY reads a chunk")
                }
                Self::Good(bytes) => bytes.chunk(),
            }
        }

        fn advance(&mut self, count: usize) {
            match self {
                Self::LengthOverflow | Self::PanicWhileFraming { .. } => {
                    unreachable!("the scripted failure happens before COPY advances the item")
                }
                Self::Good(bytes) => bytes.advance(count),
            }
        }
    }

    compio::time::timeout(std::time::Duration::from_secs(10), async {
        let url = require_pg().await;
        let client = connect(&url).await.unwrap();
        let table = common::test_object_name("cpg_copy_panicking_frame");
        client
            .batch_execute(&format!("CREATE TEMP TABLE {table} (value text NOT NULL)"))
            .await
            .unwrap();
        let copy = format!("COPY {table} (value) FROM STDIN");
        let sink = client.copy_in::<_, ScriptedBuf>(&copy).await.unwrap();
        let mut sink = Box::pin(sink);

        sink.as_mut()
            .feed(ScriptedBuf::Good(Bytes::from_static(b"error-prefix\n")))
            .await
            .unwrap();
        sink.as_mut()
            .feed(ScriptedBuf::LengthOverflow)
            .await
            .expect_err("oversized input did not return an encoding error");
        sink.as_mut()
            .feed(ScriptedBuf::Good(Bytes::from_static(b"after-error\n")))
            .await
            .unwrap();
        sink.as_mut().flush().await.unwrap();

        sink.as_mut()
            .feed(ScriptedBuf::Good(Bytes::from_static(b"panic-prefix\n")))
            .await
            .unwrap();
        let panic = AssertUnwindSafe(sink.as_mut().feed(ScriptedBuf::PanicWhileFraming {
            remaining_calls: Cell::new(0),
        }))
        .catch_unwind()
        .await;
        assert!(panic.is_err(), "the scripted framing input did not panic");
        sink.as_mut()
            .send(ScriptedBuf::Good(Bytes::from_static(b"clean\n")))
            .await
            .unwrap();
        sink.as_mut().finish().await.unwrap();

        let query = format!("SELECT value FROM {table} ORDER BY ctid");
        let values = client
            .query(&query, &[])
            .await
            .unwrap()
            .iter()
            .map(|row| row.get::<_, &str>(0).to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            values,
            ["error-prefix", "after-error", "panic-prefix", "clean"]
        );
    })
    .await
    .expect("panicking COPY framing test exceeded its watchdog");
}

/// A caller may stop consuming COPY OUT before PostgreSQL has sent the body.
/// The driver must keep draining that response so the following request stays
/// aligned with its own backend messages.
#[compio::test]
async fn dropped_copy_out_stream_recovers_the_same_connection() {
    use futures_util::StreamExt;

    let url = require_pg().await;
    let client = connect(&url).await.unwrap();
    let table = common::test_object_name("cpg_copy_drop_out");

    client
        .batch_execute(&format!(
            "CREATE TEMPORARY TABLE {table} AS
             SELECT i::int AS n, repeat('x', 1024)::text AS payload
             FROM generate_series(1, 16384) AS i"
        ))
        .await
        .unwrap();

    {
        let stream = client
            .copy_out(&format!("COPY {table} TO STDOUT"))
            .await
            .unwrap();
        let mut stream = Box::pin(stream);
        let first = stream
            .as_mut()
            .next()
            .await
            .expect("COPY OUT ended before yielding data")
            .unwrap();
        assert!(!first.is_empty());
        // Drop with megabytes still queued on the server.
    }

    let row = compio::time::timeout(
        std::time::Duration::from_secs(5),
        client.query_one("SELECT 42::int4", &[]),
    )
    .await
    .expect("the query after dropping CopyOutStream timed out")
    .expect("dropping CopyOutStream poisoned its connection");
    assert_eq!(row.get::<_, i32>(0), 42);

    client
        .execute(&format!("DROP TABLE {table}"), &[])
        .await
        .unwrap();
}

/// PostgreSQL can fail after COPY OUT has already yielded data. The stream
/// must surface that ErrorResponse with its SQLSTATE and the trailing Sync
/// must still restore the connection.
#[compio::test]
async fn copy_out_error_surfaces_and_recovers_the_same_connection() {
    use futures_util::StreamExt;

    let url = require_pg().await;
    let client = connect(&url).await.unwrap();

    let stream = client
        .copy_out(
            "COPY (
                SELECT 1000 / (1000 - i)
                FROM generate_series(1, 2000) AS i
             ) TO STDOUT",
        )
        .await
        .unwrap();
    let mut stream = Box::pin(stream);
    let mut chunks = 0usize;
    let error = loop {
        match stream.as_mut().next().await {
            Some(Ok(chunk)) => {
                assert!(!chunk.is_empty());
                chunks += 1;
            }
            Some(Err(error)) => break error,
            None => panic!("COPY OUT ended without reporting division by zero"),
        }
    };
    assert!(
        chunks > 0,
        "the COPY failed before exercising its data stream"
    );
    assert_eq!(error.code(), Some(&SqlState::DIVISION_BY_ZERO));
    drop(stream);

    let row = client
        .query_one("SELECT 42::int4", &[])
        .await
        .expect("failed COPY OUT poisoned its connection");
    assert_eq!(row.get::<_, i32>(0), 42);
}

/// COPY OUT data completion is not command completion. PostgreSQL sends
/// CopyDone before it finishes the executor, and an AFTER trigger for a
/// data-modifying COPY query can still fail after the copied row was sent.
#[compio::test]
async fn copy_out_waits_for_the_final_command_status_after_copy_done() {
    use futures_util::StreamExt;

    let url = require_pg().await;
    let client = connect(&url).await.unwrap();
    let table = common::test_object_name("cpg_copy_out_late_failure");
    let function = common::test_object_name("cpg_copy_out_late_failure_function");
    let trigger = common::test_object_name("cpg_copy_out_late_failure_trigger");

    client
        .batch_execute(&format!(
            "CREATE TEMPORARY TABLE {table} (n int);
             CREATE FUNCTION pg_temp.{function}() RETURNS trigger
             LANGUAGE plpgsql AS $$
             BEGIN
                 RAISE EXCEPTION USING
                     ERRCODE = 'P1234',
                     MESSAGE = 'late COPY OUT failure';
             END
             $$;
             CREATE TRIGGER {trigger}
             AFTER INSERT ON {table}
             FOR EACH ROW EXECUTE FUNCTION pg_temp.{function}();"
        ))
        .await
        .unwrap();

    let stream = client
        .copy_out(&format!(
            "COPY (
                 INSERT INTO {table} VALUES (7)
                 RETURNING n
             ) TO STDOUT"
        ))
        .await
        .expect("start the COPY OUT before its late executor failure");
    let mut stream = Box::pin(stream);

    let first = stream
        .as_mut()
        .next()
        .await
        .expect("COPY OUT ended before returning its row")
        .expect("COPY OUT failed before returning its row");
    assert_eq!(first, b"7\n"[..]);

    let error = stream
        .as_mut()
        .next()
        .await
        .expect("COPY OUT reported success before its final command status")
        .expect_err("COPY OUT discarded its late executor failure");
    assert_eq!(
        error.code().map(|code| code.code()),
        Some("P1234"),
        "the late COPY OUT failure lost its SQLSTATE: {}",
        common::error_chain(&error),
    );
    drop(stream);

    let row = client
        .query_one(&format!("SELECT count(*)::int8 FROM {table}"), &[])
        .await
        .expect("the late COPY OUT failure poisoned its connection");
    assert_eq!(
        row.get::<_, i64>(0),
        0,
        "the failed data-modifying COPY OUT committed its inserted row"
    );
}

/// CommandComplete ends the COPY command, but the extended-protocol Sync that
/// follows can still fail while committing its implicit transaction. A
/// deferred constraint is checked at exactly that boundary, so COPY OUT must
/// not report EOF until ReadyForQuery proves the Sync succeeded.
#[compio::test]
async fn copy_out_waits_for_sync_before_reporting_eof() {
    use futures_util::StreamExt;

    let url = require_pg().await;
    let client = connect(&url).await.unwrap();
    let parent = common::test_object_name("cpg_copy_out_sync_parent");
    let child = common::test_object_name("cpg_copy_out_sync_child");

    client
        .batch_execute(&format!(
            "CREATE TEMPORARY TABLE {parent} (id int PRIMARY KEY);
             CREATE TEMPORARY TABLE {child} (
                 parent_id int REFERENCES {parent} (id)
                     DEFERRABLE INITIALLY DEFERRED
             );"
        ))
        .await
        .unwrap();

    let stream = client
        .copy_out(&format!(
            "COPY (
                 INSERT INTO {child} VALUES (314159)
                 RETURNING parent_id
             ) TO STDOUT"
        ))
        .await
        .expect("start COPY OUT before its deferred constraint is checked");
    let mut stream = Box::pin(stream);

    let first = stream
        .as_mut()
        .next()
        .await
        .expect("COPY OUT ended before returning its row")
        .expect("COPY OUT failed before returning its row");
    assert_eq!(first, b"314159\n"[..]);

    let error = stream
        .as_mut()
        .next()
        .await
        .expect("COPY OUT reported EOF before Sync committed its implicit transaction")
        .expect_err("COPY OUT accepted a deferred foreign-key violation");
    assert_eq!(
        error.code(),
        Some(&SqlState::FOREIGN_KEY_VIOLATION),
        "the Sync failure lost its SQLSTATE: {}",
        common::error_chain(&error),
    );
    drop(stream);

    let count: i64 = client
        .query_one_scalar(&format!("SELECT count(*)::int8 FROM {child}"), &[])
        .await
        .expect("the deferred COPY OUT failure poisoned its connection");
    assert_eq!(
        count, 0,
        "the failed implicit transaction committed its row"
    );
}

/// Buffers larger than the coalescing threshold become independent CopyData
/// messages. Enough of them must cross the connection task without loss,
/// reordering, or an early CopyDone.
#[compio::test]
async fn copy_in_spans_many_copy_data_frames() {
    use bytes::Bytes;
    use futures_util::SinkExt;

    const FRAMES: i32 = 64;
    const ROWS_PER_FRAME: i32 = 2000;

    let url = require_pg().await;
    let client = connect(&url).await.unwrap();
    let table = common::test_object_name("cpg_copy_many_frames");
    client
        .execute(&format!("CREATE TEMPORARY TABLE {table} (n int)"), &[])
        .await
        .unwrap();

    let sink = client
        .copy_in::<_, Bytes>(&format!("COPY {table} (n) FROM STDIN"))
        .await
        .unwrap();
    let mut sink = Box::pin(sink);
    for frame in 0..FRAMES {
        let first = frame * ROWS_PER_FRAME;
        let mut chunk = String::new();
        for n in first..first + ROWS_PER_FRAME {
            use std::fmt::Write;
            writeln!(chunk, "{n}").unwrap();
        }
        assert!(chunk.len() > 4096);
        sink.as_mut().send(Bytes::from(chunk)).await.unwrap();
    }
    assert_eq!(
        sink.as_mut().finish().await.unwrap(),
        (FRAMES * ROWS_PER_FRAME) as u64
    );
    drop(sink);

    let row = client
        .query_one(
            &format!("SELECT count(*)::int8, min(n), max(n) FROM {table}"),
            &[],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, i64>(0), (FRAMES * ROWS_PER_FRAME) as i64);
    assert_eq!(row.get::<_, i32>(1), 0);
    assert_eq!(row.get::<_, i32>(2), FRAMES * ROWS_PER_FRAME - 1);

    client
        .execute(&format!("DROP TABLE {table}"), &[])
        .await
        .unwrap();
}

/// Binary COPY must distinguish a non-NULL value with a zero-byte encoding
/// from NULL in both directions.
#[compio::test]
async fn binary_copy_round_trips_empty_and_null_fields() {
    use compio_postgres::binary_copy::{BinaryCopyInWriter, BinaryCopyOutStream};
    use compio_postgres::types::Type;
    use futures_util::StreamExt;

    let url = require_pg().await;
    let client = connect(&url).await.unwrap();
    let table = common::test_object_name("cpg_copy_binary_fields");
    client
        .execute(
            &format!(
                "CREATE TEMPORARY TABLE {table} (
                id int,
                empty text NOT NULL,
                missing text
            )"
            ),
            &[],
        )
        .await
        .unwrap();

    let sink = client
        .copy_in(&format!("COPY {table} FROM STDIN BINARY"))
        .await
        .unwrap();
    let mut writer = Box::pin(BinaryCopyInWriter::new(
        sink,
        &[Type::INT4, Type::TEXT, Type::TEXT],
    ));
    writer
        .as_mut()
        .write(&[&1_i32, &"", &None::<&str>])
        .await
        .unwrap();
    assert_eq!(writer.as_mut().finish().await.unwrap(), 1);
    drop(writer);

    let sql_row = client
        .query_one(
            &format!("SELECT octet_length(empty), missing IS NULL FROM {table}"),
            &[],
        )
        .await
        .unwrap();
    assert_eq!(sql_row.get::<_, i32>(0), 0);
    assert!(sql_row.get::<_, bool>(1));

    let stream = client
        .copy_out(&format!("COPY {table} TO STDOUT BINARY"))
        .await
        .unwrap();
    let mut rows = Box::pin(BinaryCopyOutStream::new(
        stream,
        &[Type::INT4, Type::TEXT, Type::TEXT],
    ));
    let row = rows
        .as_mut()
        .next()
        .await
        .expect("binary COPY OUT returned no row")
        .unwrap();
    assert_eq!(row.get::<i32>(0), 1);
    assert_eq!(row.get::<&str>(1), "");
    assert_eq!(row.get::<Option<&str>>(2), None);
    assert!(rows.as_mut().next().await.is_none());
    drop(rows);

    client
        .execute(&format!("DROP TABLE {table}"), &[])
        .await
        .unwrap();
}

/// Binary COPY OUT of many rows of DIFFERENT widths, against the real server.
///
/// `BinaryCopyOutStream` parses one tuple per `CopyData` chunk and now REFUSES
/// a chunk with bytes left over, on the protocol's guarantee that a backend
/// sends "zero or more CopyData messages (always one per row)" in copy-out
/// mode. That guarantee is the peer's, so this is the test that says the peer
/// we actually ship against keeps it -- and keeps it where it would be easiest
/// not to: rows whose encoded size varies from a few bytes to a few hundred,
/// enough of them that the response crosses socket reads and arrives as
/// several decoder batches rather than one.
///
/// `binary_copy_round_trips_empty_and_null_fields` above copies a SINGLE row,
/// so it cannot see a framing decision at all. Without this one the refusal
/// would be pinned only by a scripted peer, which is the wrong place to learn
/// that a real one trips it.
#[compio::test]
async fn binary_copy_out_of_many_variable_width_rows_arrives_one_tuple_per_frame() {
    use compio_postgres::binary_copy::BinaryCopyOutStream;
    use compio_postgres::types::Type;
    use futures_util::TryStreamExt;

    const ROWS: i32 = 500;

    let url = require_pg().await;
    let client = connect(&url).await.unwrap();
    let table = common::test_object_name("cpg_copy_binary_widths");
    client
        .execute(
            &format!("CREATE TEMPORARY TABLE {table} (n int4, v text)"),
            &[],
        )
        .await
        .unwrap();
    client
        .execute(
            &format!(
                "INSERT INTO {table} \
             SELECT g, repeat('x', g % 300) FROM generate_series(0, $1 - 1) AS g",
            ),
            &[&ROWS],
        )
        .await
        .unwrap();

    let stream = client
        .copy_out(&format!(
            "COPY (SELECT n, v FROM {table} ORDER BY n) TO STDOUT BINARY"
        ))
        .await
        .unwrap();
    let mut rows = Box::pin(BinaryCopyOutStream::new(stream, &[Type::INT4, Type::TEXT]));

    let mut seen = 0i32;
    while let Some(row) = rows
        .try_next()
        .await
        .expect("real PostgreSQL framing was refused by the one-tuple-per-chunk check")
    {
        assert_eq!(row.get::<i32>(0), seen, "rows arrived out of order");
        assert_eq!(
            row.get::<&str>(1).len(),
            (seen % 300) as usize,
            "row {seen} came back the wrong width"
        );
        seen += 1;
    }
    assert_eq!(
        seen, ROWS,
        "binary COPY OUT delivered {seen} of {ROWS} rows"
    );
    drop(rows);

    client
        .execute(&format!("DROP TABLE {table}"), &[])
        .await
        .unwrap();
}

/// A successful COPY is still transactional: ROLLBACK must discard its rows
/// and leave the same connection ready for later work.
#[compio::test]
async fn copy_in_inside_transaction_is_rolled_back() {
    use bytes::Bytes;
    use futures_util::SinkExt;

    let url = require_pg().await;
    let mut client = connect(&url).await.unwrap();
    let table = common::test_object_name("cpg_copy_transaction");
    client
        .execute(&format!("CREATE TEMPORARY TABLE {table} (n int)"), &[])
        .await
        .unwrap();

    {
        let transaction = client.transaction().await.unwrap();
        let sink = transaction
            .copy_in::<_, Bytes>(&format!("COPY {table} (n) FROM STDIN"))
            .await
            .unwrap();
        let mut sink = Box::pin(sink);
        sink.as_mut()
            .send(Bytes::from_static(b"1\n2\n3\n"))
            .await
            .unwrap();
        assert_eq!(sink.as_mut().finish().await.unwrap(), 3);
        drop(sink);

        let row = transaction
            .query_one(&format!("SELECT count(*)::int8 FROM {table}"), &[])
            .await
            .unwrap();
        assert_eq!(row.get::<_, i64>(0), 3);
        transaction.rollback().await.unwrap();
    }

    let row = client
        .query_one(&format!("SELECT count(*)::int8 FROM {table}"), &[])
        .await
        .unwrap();
    assert_eq!(row.get::<_, i64>(0), 0);
    client
        .execute(&format!("DROP TABLE {table}"), &[])
        .await
        .unwrap();
}

/// A COPY error aborts its transaction, but an explicit ROLLBACK must still
/// consume the failed COPY's ReadyForQuery and recover the session.
#[compio::test]
async fn failed_copy_in_transaction_can_be_rolled_back() {
    use bytes::Bytes;
    use futures_util::SinkExt;

    let url = require_pg().await;
    let mut client = connect(&url).await.unwrap();
    let table = common::test_object_name("cpg_copy_failed_transaction");
    client
        .execute(
            &format!("CREATE TEMPORARY TABLE {table} (n int PRIMARY KEY)"),
            &[],
        )
        .await
        .unwrap();

    {
        let transaction = client.transaction().await.unwrap();
        let sink = transaction
            .copy_in::<_, Bytes>(&format!("COPY {table} (n) FROM STDIN"))
            .await
            .unwrap();
        let mut sink = Box::pin(sink);
        sink.as_mut()
            .send(Bytes::from_static(b"1\n1\n"))
            .await
            .unwrap();
        let error = sink
            .as_mut()
            .finish()
            .await
            .expect_err("duplicate COPY input unexpectedly succeeded");
        assert_eq!(error.code(), Some(&SqlState::UNIQUE_VIOLATION));
        drop(sink);
        transaction.rollback().await.unwrap();
    }

    let row = client
        .query_one(&format!("SELECT count(*)::int8 FROM {table}"), &[])
        .await
        .expect("ROLLBACK did not recover the connection after failed COPY");
    assert_eq!(row.get::<_, i64>(0), 0);
    client
        .execute(&format!("DROP TABLE {table}"), &[])
        .await
        .unwrap();
}

// ---------------------------------------------------------------------------
// 29. concurrent_queries_are_pipelined (IO-3)
//
// Multiple queries issued concurrently on ONE client/connection must all
// complete with correct, in-order results. The multiplexed loop writes the
// later requests immediately (without waiting for the earlier response to
// start arriving) and reads the responses concurrently; this test guards
// against any corruption / misrouting / hang in that multi-outstanding-request
// path (FIFO response routing, the read channel, the pending_responses gate).
//
// On the timing of the pipelining win: it is NOT observable on a single
// PostgreSQL connection. A backend executes one connection's messages strictly
// in order, so two `pg_sleep(0.4)` always take ~0.8s wall-clock whether the
// second request is written up-front (multiplexed) or after the first response
// (serialized) - the second query cannot begin on the server until the first
// finishes regardless. The serialized loop also already drains its queued
// requests right after the first read, so the only difference is a single
// round-trip's worth of latency (sub-millisecond on localhost). A timing
// assertion was therefore tried and rejected as inherently non-discriminating;
// this asserts the achievable, deterministic property instead: correctness of
// many concurrent in-flight requests.
//
// We drive several queries via `join_all` on the same `&Client`. Each
// `simple_query` enqueues its request synchronously on first poll (before
// awaiting its response), so all are outstanding at once - exercising the
// multiplexed loop with a full pipeline of overlapping requests.
// ---------------------------------------------------------------------------

#[compio::test]
async fn concurrent_queries_are_pipelined() {
    use futures_util::future::join_all;

    let url = require_pg().await;
    let client = connect(&url).await.unwrap();

    // 32 distinct queries, each returning its own index, all issued at once.
    let futs = (0..32i32).map(|i| {
        let client = &client;
        async move {
            let rows = client.query("SELECT $1::int4 AS v", &[&i]).await.unwrap();
            rows[0].get::<_, i32>("v")
        }
    });
    let results = join_all(futs).await;

    // Every concurrently-issued request must come back with its own correct
    // value - proving the multiplexed loop routed the overlapping responses to
    // the right callers (FIFO), with no corruption, loss, or hang.
    assert_eq!(results, (0..32i32).collect::<Vec<_>>());

    // And a couple of concurrent simple_query streams resolve correctly too.
    let (a, b) = futures_util::future::join(
        client.query("SELECT 'a'::text AS x", &[]),
        client.query("SELECT 'b'::text AS x", &[]),
    )
    .await;
    assert_eq!(a.unwrap()[0].get::<_, &str>("x"), "a");
    assert_eq!(b.unwrap()[0].get::<_, &str>("x"), "b");
}

// ---------------------------------------------------------------------------
// Pipelined failure routing
//
// Every helper below sends one Parse + Bind + Describe + Execute + Sync batch
// per future before that future first awaits a response. `join_all` therefore
// makes every request outstanding on the same Client at once, while the
// distinct echo values make a response routed to the wrong caller observable.
// ---------------------------------------------------------------------------

const PIPELINED_FAILURE_WATCHDOG: std::time::Duration = std::time::Duration::from_secs(10);
const FAILURE_PIPELINE_LEN: usize = 7;
const MISSING_PIPELINE_VALUE: i32 = i32::MIN;
const ECHO_SQL: &str = "SELECT $1::int4 AS v";

// A constant `SELECT 1 / 0::int4` is folded while PostgreSQL plans at Bind:
// its backend response has ParseComplete followed by ErrorResponse, with no
// BindComplete. Taking the zero from an execution-time row instead produces
// ParseComplete + BindComplete + RowDescription before 22012, so this really
// exercises an Execute-stage failure rather than duplicating the Bind case.
const EXECUTE_DIVISION_BY_ZERO_SQL: &str =
    "SELECT 1 / n::int4 AS v FROM generate_series(0, 0) AS g(n)";

const PREPARE_SYNTAX_ERROR_SQL: &str = "SELEC 1::int4 AS v";
const PREPARE_MISSING_RELATION_SQL: &str =
    "SELECT 1::int4 AS v FROM cpg_pipeline_relation_that_does_not_exist";

#[derive(Clone, Copy, Debug)]
enum PipelineExpectation {
    Value(i32),
    SqlState(&'static SqlState),
}

fn first_pipeline_value(rows: Vec<Row>) -> i32 {
    rows.first()
        .map_or(MISSING_PIPELINE_VALUE, |row| row.get::<_, i32>("v"))
}

fn assert_pipeline_result(
    context: &str,
    actual: Result<i32, Error>,
    expected: PipelineExpectation,
) {
    match (actual, expected) {
        (Ok(actual), PipelineExpectation::Value(expected)) => {
            assert_eq!(actual, expected, "{context} received another caller's row")
        }
        (Err(actual), PipelineExpectation::SqlState(expected)) => assert_eq!(
            actual.code(),
            Some(expected),
            "{context} received another caller's error: {actual:?}"
        ),
        (Ok(actual), PipelineExpectation::SqlState(expected)) => panic!(
            "{context} returned value {actual} instead of SQLSTATE {}",
            expected.code()
        ),
        (Err(actual), PipelineExpectation::Value(expected)) => panic!(
            "{context} returned SQLSTATE {} instead of its value {expected}: {actual:?}",
            actual.code().map_or("<none>", SqlState::code)
        ),
    }
}

async fn execute_failure_pipeline(
    client: &Client,
    failure_index: usize,
    value_base: i32,
) -> Vec<Result<i32, Error>> {
    futures_util::future::join_all((0..FAILURE_PIPELINE_LEN).map(|index| async move {
        if index == failure_index {
            client
                .query_typed(EXECUTE_DIVISION_BY_ZERO_SQL, &[])
                .await
                .map(first_pipeline_value)
        } else {
            let value = value_base + index as i32;
            client
                .query_typed(ECHO_SQL, &[(&value, Type::INT4)])
                .await
                .map(first_pipeline_value)
        }
    }))
    .await
}

async fn bind_failure_pipeline(
    client: &Client,
    failure_index: usize,
    value_base: i32,
) -> Vec<Result<i32, Error>> {
    futures_util::future::join_all((0..FAILURE_PIPELINE_LEN).map(|index| async move {
        let value = value_base + index as i32;
        let encoded = if index == failure_index {
            "not-an-int4".to_string()
        } else {
            value.to_string()
        };
        client
            .query_text_params(ECHO_SQL, &[encoded.as_str()])
            .await
            .map(first_pipeline_value)
    }))
    .await
}

async fn prepare_failure_pipeline(
    client: &Client,
    failure_index: usize,
    value_base: i32,
) -> Vec<Result<i32, Error>> {
    futures_util::future::join_all((0..FAILURE_PIPELINE_LEN).map(|index| async move {
        if index == failure_index {
            client
                .query_typed(PREPARE_SYNTAX_ERROR_SQL, &[])
                .await
                .map(first_pipeline_value)
        } else {
            let value = value_base + index as i32;
            client
                .query_typed(ECHO_SQL, &[(&value, Type::INT4)])
                .await
                .map(first_pipeline_value)
        }
    }))
    .await
}

async fn multiple_failure_pipeline(client: &Client, value_base: i32) -> Vec<Result<i32, Error>> {
    futures_util::future::join_all((0..FAILURE_PIPELINE_LEN).map(|index| async move {
        match index {
            1 => client
                .query_typed(EXECUTE_DIVISION_BY_ZERO_SQL, &[])
                .await
                .map(first_pipeline_value),
            3 => client
                .query_text_params(ECHO_SQL, &["not-an-int4"])
                .await
                .map(first_pipeline_value),
            5 => client
                .query_typed(PREPARE_MISSING_RELATION_SQL, &[])
                .await
                .map(first_pipeline_value),
            _ => {
                let value = value_base + index as i32;
                client
                    .query_typed(ECHO_SQL, &[(&value, Type::INT4)])
                    .await
                    .map(first_pipeline_value)
            }
        }
    }))
    .await
}

async fn assert_autocommit_connection_is_reusable(client: &Client, expected: i32) {
    let actual = client
        .query_typed(ECHO_SQL, &[(&expected, Type::INT4)])
        .await
        .map(first_pipeline_value)
        .expect("fresh query failed after the pipelined autocommit error");
    assert_eq!(actual, expected, "fresh query received stale pipeline data");
    assert_eq!(
        client.transaction_status(),
        Some(TransactionStatus::Idle),
        "autocommit pipeline did not drain to an idle transaction status"
    );
}

async fn begin_pipeline_transaction(client: &Client) {
    client.batch_execute("BEGIN").await.unwrap();
    assert_eq!(
        client.transaction_status(),
        Some(TransactionStatus::InTransaction)
    );
}

async fn assert_failed_transaction_then_recover(client: &Client, expected: i32) {
    // ErrorResponse reaches its caller before the trailing ReadyForQuery has
    // necessarily updated transaction_status(). This empty query is a FIFO
    // barrier and is valid even in an aborted transaction block.
    client.simple_query("").await.unwrap();
    assert_eq!(
        client.transaction_status(),
        Some(TransactionStatus::Failed),
        "pipelined transaction error did not leave the block failed"
    );

    let error = client
        .query_typed(ECHO_SQL, &[(&expected, Type::INT4)])
        .await
        .expect_err("a fresh statement unexpectedly ran inside the failed transaction");
    assert_eq!(error.code(), Some(&SqlState::IN_FAILED_SQL_TRANSACTION));

    client.batch_execute("ROLLBACK").await.unwrap();
    assert_eq!(client.transaction_status(), Some(TransactionStatus::Idle));
    assert_autocommit_connection_is_reusable(client, expected).await;
}

#[compio::test]
async fn pipelined_execute_failure_routes_by_caller_at_every_position() {
    compio::time::timeout(PIPELINED_FAILURE_WATCHDOG, async {
        let url = require_pg().await;
        let client = connect(&url).await.unwrap();

        for (round, failure_index) in [0, FAILURE_PIPELINE_LEN / 2, FAILURE_PIPELINE_LEN - 1]
            .into_iter()
            .enumerate()
        {
            let value_base = 1_000 + round as i32 * 100;
            let results = execute_failure_pipeline(&client, failure_index, value_base).await;
            for (index, result) in results.into_iter().enumerate() {
                let expected = if index == failure_index {
                    PipelineExpectation::SqlState(&SqlState::DIVISION_BY_ZERO)
                } else {
                    PipelineExpectation::Value(value_base + index as i32)
                };
                assert_pipeline_result(
                    &format!("execute pipeline round {round} request {index}"),
                    result,
                    expected,
                );
            }
            assert_autocommit_connection_is_reusable(&client, 1_900 + round as i32).await;
        }
    })
    .await
    .expect("pipelined Execute-stage failure test exceeded its watchdog");
}

#[compio::test]
async fn pipelined_bind_failure_routes_by_caller_and_preserves_neighbours() {
    compio::time::timeout(PIPELINED_FAILURE_WATCHDOG, async {
        let url = require_pg().await;
        let client = connect(&url).await.unwrap();
        let failure_index = FAILURE_PIPELINE_LEN / 2;
        let value_base = 2_000;

        let results = bind_failure_pipeline(&client, failure_index, value_base).await;
        for (index, result) in results.into_iter().enumerate() {
            let expected = if index == failure_index {
                PipelineExpectation::SqlState(&SqlState::INVALID_TEXT_REPRESENTATION)
            } else {
                PipelineExpectation::Value(value_base + index as i32)
            };
            assert_pipeline_result(&format!("bind pipeline request {index}"), result, expected);
        }
        assert_autocommit_connection_is_reusable(&client, 2_900).await;
    })
    .await
    .expect("pipelined Bind-stage failure test exceeded its watchdog");
}

#[compio::test]
async fn pipelined_prepare_failure_routes_by_caller_and_preserves_neighbours() {
    compio::time::timeout(PIPELINED_FAILURE_WATCHDOG, async {
        let url = require_pg().await;
        let client = connect(&url).await.unwrap();
        let failure_index = FAILURE_PIPELINE_LEN / 2;
        let value_base = 3_000;

        let results = prepare_failure_pipeline(&client, failure_index, value_base).await;
        for (index, result) in results.into_iter().enumerate() {
            let expected = if index == failure_index {
                PipelineExpectation::SqlState(&SqlState::SYNTAX_ERROR)
            } else {
                PipelineExpectation::Value(value_base + index as i32)
            };
            assert_pipeline_result(
                &format!("prepare pipeline request {index}"),
                result,
                expected,
            );
        }
        assert_autocommit_connection_is_reusable(&client, 3_900).await;
    })
    .await
    .expect("pipelined Parse-stage failure test exceeded its watchdog");
}

#[compio::test]
async fn pipelined_multiple_failures_keep_their_own_errors_and_rows() {
    compio::time::timeout(PIPELINED_FAILURE_WATCHDOG, async {
        let url = require_pg().await;
        let client = connect(&url).await.unwrap();
        let value_base = 4_000;

        let results = multiple_failure_pipeline(&client, value_base).await;
        for (index, result) in results.into_iter().enumerate() {
            let expected = match index {
                1 => PipelineExpectation::SqlState(&SqlState::DIVISION_BY_ZERO),
                3 => PipelineExpectation::SqlState(&SqlState::INVALID_TEXT_REPRESENTATION),
                5 => PipelineExpectation::SqlState(&SqlState::UNDEFINED_TABLE),
                _ => PipelineExpectation::Value(value_base + index as i32),
            };
            assert_pipeline_result(
                &format!("multiple-failure pipeline request {index}"),
                result,
                expected,
            );
        }
        assert_autocommit_connection_is_reusable(&client, 4_900).await;
    })
    .await
    .expect("multiple-failure pipeline test exceeded its watchdog");
}

#[compio::test]
async fn pipelined_failures_inside_transactions_abort_only_later_requests() {
    compio::time::timeout(PIPELINED_FAILURE_WATCHDOG, async {
        let url = require_pg().await;
        let client = connect(&url).await.unwrap();

        // Execute-stage failure at the first, middle, and last position. Rows
        // before it still belong to their callers; syntactically valid requests
        // after it are rejected by the failed transaction block with 25P02.
        for (round, failure_index) in [0, FAILURE_PIPELINE_LEN / 2, FAILURE_PIPELINE_LEN - 1]
            .into_iter()
            .enumerate()
        {
            begin_pipeline_transaction(&client).await;
            let value_base = 5_000 + round as i32 * 100;
            let results = execute_failure_pipeline(&client, failure_index, value_base).await;
            for (index, result) in results.into_iter().enumerate() {
                let expected = if index < failure_index {
                    PipelineExpectation::Value(value_base + index as i32)
                } else if index == failure_index {
                    PipelineExpectation::SqlState(&SqlState::DIVISION_BY_ZERO)
                } else {
                    PipelineExpectation::SqlState(&SqlState::IN_FAILED_SQL_TRANSACTION)
                };
                assert_pipeline_result(
                    &format!("transaction execute round {round} request {index}"),
                    result,
                    expected,
                );
            }
            assert_failed_transaction_then_recover(&client, 5_900 + round as i32).await;
        }

        begin_pipeline_transaction(&client).await;
        let failure_index = FAILURE_PIPELINE_LEN / 2;
        let results = bind_failure_pipeline(&client, failure_index, 6_000).await;
        for (index, result) in results.into_iter().enumerate() {
            let expected = if index < failure_index {
                PipelineExpectation::Value(6_000 + index as i32)
            } else if index == failure_index {
                PipelineExpectation::SqlState(&SqlState::INVALID_TEXT_REPRESENTATION)
            } else {
                PipelineExpectation::SqlState(&SqlState::IN_FAILED_SQL_TRANSACTION)
            };
            assert_pipeline_result(
                &format!("transaction bind request {index}"),
                result,
                expected,
            );
        }
        assert_failed_transaction_then_recover(&client, 6_900).await;

        begin_pipeline_transaction(&client).await;
        let results = prepare_failure_pipeline(&client, failure_index, 7_000).await;
        for (index, result) in results.into_iter().enumerate() {
            let expected = if index < failure_index {
                PipelineExpectation::Value(7_000 + index as i32)
            } else if index == failure_index {
                PipelineExpectation::SqlState(&SqlState::SYNTAX_ERROR)
            } else {
                PipelineExpectation::SqlState(&SqlState::IN_FAILED_SQL_TRANSACTION)
            };
            assert_pipeline_result(
                &format!("transaction prepare request {index}"),
                result,
                expected,
            );
        }
        assert_failed_transaction_then_recover(&client, 7_900).await;

        // Only the first server error is allowed to run. Every later request
        // is syntactically valid, including the ones that would otherwise fail
        // during Bind, Parse analysis, or Execute, so each is rejected with
        // 25P02 before it can produce its own ordinary result.
        begin_pipeline_transaction(&client).await;
        let results = multiple_failure_pipeline(&client, 8_000).await;
        for (index, result) in results.into_iter().enumerate() {
            let expected = match index {
                0 => PipelineExpectation::Value(8_000),
                1 => PipelineExpectation::SqlState(&SqlState::DIVISION_BY_ZERO),
                _ => PipelineExpectation::SqlState(&SqlState::IN_FAILED_SQL_TRANSACTION),
            };
            assert_pipeline_result(
                &format!("transaction multiple-failure request {index}"),
                result,
                expected,
            );
        }
        assert_failed_transaction_then_recover(&client, 8_900).await;
    })
    .await
    .expect("explicit-transaction pipeline failure test exceeded its watchdog");
}

// ---------------------------------------------------------------------------
// 30. concurrent_large_bidirectional_queries_do_not_deadlock (MUX-DEADLOCK-1)
//
// Regression for the cap-1-read-channel + blocking-flush deadlock in the
// multiplexed loop. Many large queries are issued concurrently on ONE
// `Client`; each sends a ~4 MB bytea param (a large WRITE that fills the
// kernel send buffer) and selects back a much larger result the server
// floods concurrently (filling ITS send buffer once the client stalls
// reading). The result is fanned out over many rows so each individual
// DataRow frame stays well under the 64 MB MAX_MESSAGE_SIZE cap (a single
// 64 MB+ field would be rejected as oversize, masking the deadlock behind an
// io error; and a single giant frame would not wedge anyway, since the read
// task drains the socket continuously while assembling one frame).
//
// Pre-fix the main loop did `write_half.flush().await?` SEQUENTIALLY without
// draining the read channel, so this cycle wedged:
//   flush-blocked (client send buffer full)
//     -> server recv buffer full -> server send blocked
//       -> client not reading -> read task's cap-1 send().await blocked
//         -> main loop never drains the read channel -> flush never resumes.
// Sequentially the same queries finish in a few seconds.
//
// Post-fix the flush is interleaved with read-channel draining (a cancel-safe
// select carrying the owned flush future against the read branch), so reads
// keep the socket draining while the large write completes -> no deadlock.
//
// Wrapped in a 20 s timeout so a wedged (pre-fix) loop fails fast as a
// timeout (RED) instead of hanging the whole suite; post-fix it completes
// well under the budget with every blob echoed back byte-for-byte (GREEN).
// ---------------------------------------------------------------------------

#[compio::test]
async fn concurrent_large_bidirectional_queries_do_not_deadlock() {
    use futures_util::future::join_all;

    const BLOB_LEN: usize = 16 * 1024 * 1024; // ~16 MB param per query (< 64 MB cap)
    const ROWS: i32 = 3; // result ~= 48 MB, fanned over 3 frames of ~16 MB
    const N: usize = 16; // concurrent in-flight queries on one connection

    let url = require_pg().await;
    let client = connect(&url).await.unwrap();

    // Distinct payloads so a misrouted/corrupted response is caught, not just
    // a hang. Byte i of blob k = (i + k) mod 256.
    let blobs: Vec<Vec<u8>> = (0..N)
        .map(|k| {
            (0..BLOB_LEN)
                .map(|i| u8::try_from((i + k) % 256).unwrap())
                .collect()
        })
        .collect();

    let futs = blobs.iter().enumerate().map(|(k, blob)| {
        let client = &client;
        async move {
            // generate_series echoes the 4 MB param back on every row, so the
            // server emits ~ROWS x the param size: a large result it floods
            // while we are still writing other queries' large params.
            let rows = client
                .query(
                    "SELECT $1::bytea AS b FROM generate_series(1, $2::int4)",
                    &[blob, &ROWS],
                )
                .await
                .unwrap();
            let echoed: Vec<Vec<u8>> = rows.iter().map(|r| r.get("b")).collect();
            (k, echoed)
        }
    });

    // Pre-fix: deadlock -> this times out (RED). Post-fix: completes (GREEN).
    let outcome = compio::time::timeout(std::time::Duration::from_secs(20), join_all(futs)).await;

    let results = outcome.expect(
        "concurrent large bidirectional queries DEADLOCKED on one connection \
         (multiplexed flush did not interleave read-draining) -- timed out",
    );

    // Every concurrently-issued query came back, in order, with its own blob
    // echoed byte-for-byte on every row: no hang, no loss, no misrouting, no
    // corruption.
    assert_eq!(results.len(), N);
    for (k, echoed) in results {
        assert_eq!(echoed.len(), ROWS as usize, "query {k} wrong row count");
        for (r, row) in echoed.iter().enumerate() {
            assert_eq!(row.len(), BLOB_LEN, "query {k} row {r} truncated");
            assert_eq!(row, &blobs[k], "query {k} row {r} corrupted or misrouted");
        }
    }
}

// ---------------------------------------------------------------------------
// 31. multiplexed_clean_shutdown_completes_without_hang (MUX-1 / MUX-4)
//
// When the `Client` is dropped, the multiplexed driver must (a) send
// Terminate, (b) emit a clean TCP FIN via the write half's `shutdown`, (c)
// stop the dedicated read task, and (d) have `Connection::run` resolve
// `Ok(())` promptly - never hang.
//
// We RETAIN the connection task's JoinHandle (instead of the detaching
// `connect` helper) so we can observe the task actually finishing. Dropping
// the client closes the request channel and releases the socket; the driver
// drains, attempts its goodbye, cancels the read task in its teardown block,
// and returns Ok whether or not that goodbye landed. A 5 s
// timeout turns any teardown hang (e.g. a read task left parked forever, or a
// shutdown that wedges) into a fast, loud failure.
//
// This is the deterministically-testable slice of the teardown fix. The
// other half - a leak on a WRITE-error exit against a half-open / partitioned
// peer - cannot be forced reliably against a live PG without a custom
// man-in-the-middle socket, and is covered by code review (the teardown block
// runs on every exit path, including `?`-propagated errors).
// ---------------------------------------------------------------------------

#[compio::test]
async fn multiplexed_clean_shutdown_completes_without_hang() {
    let url = require_pg().await;

    let (client, connection) = compio_postgres::connect(&url, common::suite_tls())
        .await
        .unwrap();
    // Retain the handle so we can await the driver's own clean exit.
    let conn_handle = compio::runtime::spawn(async move { connection.run().await });

    // Use the connection so the multiplexed loop is fully live (responses
    // queue exercised, read task streaming).
    let rows = client.query("SELECT 42::int4 AS v", &[]).await.unwrap();
    assert_eq!(rows[0].get::<_, i32>("v"), 42);

    // Drop the client: the request channel closes AND `ConnectionRelease`
    // shuts the socket down on the spot, so the driver's Terminate normally
    // does NOT reach the server - `run` still has to stop the read task and
    // return Ok(()). Before that release existed this returned
    // `Err(BrokenPipe)`, because the driver propagated the failed goodbye.
    drop(client);

    // The driver must finish on its own, promptly, with a clean Ok. A hang
    // here (parked read task / wedged shutdown) trips the timeout.
    // A timeout here means a hang (parked read task / wedged shutdown).
    let join_result = compio::time::timeout(std::time::Duration::from_secs(5), conn_handle)
        .await
        .expect(
            "multiplexed driver did not shut down within 5s after client drop \
             - read task likely left parked / teardown hung",
        );
    // Task ran to completion (not cancelled / panicked) ...
    let run_result = join_result.expect("connection task panicked or was cancelled");
    // ... and the multiplexed clean-shutdown path returned Ok.
    run_result.expect("clean shutdown should resolve Ok(())");
}

// ---------------------------------------------------------------------------
// 32. cancelled_prepare_does_not_leak_a_server_statement
//
// `prepare` picks a name, queues `Parse + Describe + Sync`, then awaits the
// response. The name only becomes a `Statement` - the thing whose `Drop` sends
// `Close S` - once the whole exchange succeeds. Drop the future in between and
// the server keeps a prepared statement no client handle names any more, for
// the life of the session.
//
// The test counts `pg_prepared_statements` around a prepare that is polled a
// fixed number of times and then dropped wherever it got to. It sweeps the
// cut point across every suspension point the exchange has - one poll queues
// the Parse and cannot yet have seen ParseComplete (the driver task has had no
// chance to run), and each further poll, with a yield to the runtime in
// between, advances one response. The last cut lets the prepare finish, where
// the `Statement` is expected to close its own name; that arm is the control
// which shows the counting itself does not manufacture a difference.
//
// `simple_query` afterwards is a barrier: requests reach the server in FIFO
// order, so its completion proves the server has executed the Parse.
//
// The probe statement returns a builtin type, so `prepare` runs no nested
// typeinfo lookups. A composite or enum column would prepare a typeinfo
// statement that the client legitimately caches for its lifetime, and that
// cached statement would read as a leak here.
//
// Both counts are taken by a query that is itself a named prepared statement,
// so each sees exactly one statement of its own; the difference is what
// matters. The count query's own `Close` is queued before it returns (the
// `Statement` dies with the rows), and Close travels the same FIFO, so the
// second count cannot be inflated by the first.
// ---------------------------------------------------------------------------

#[compio::test]
async fn cancelled_prepare_does_not_leak_a_server_statement() {
    use std::future::Future;
    use std::pin::pin;
    use std::task::{Context, Waker};
    use std::time::Duration;

    async fn count(client: &Client) -> i64 {
        client
            .query_one_scalar("SELECT count(*) FROM pg_prepared_statements", &[])
            .await
            .unwrap()
    }

    /// Polls `fut` at most `polls` times, sleeping between polls so the driver
    /// task can deliver the next response, then drops it. Returns whether it
    /// ran to completion. A no-op waker is fine because the re-polling is on
    /// this loop's schedule, not the future's.
    async fn poll_then_drop<F: Future>(fut: F, polls: usize) -> bool {
        let mut fut = pin!(fut);
        let mut cx = Context::from_waker(Waker::noop());
        for _ in 0..polls {
            if fut.as_mut().poll(&mut cx).is_ready() {
                return true;
            }
            compio::time::sleep(Duration::from_millis(2)).await;
        }
        false
    }

    let url = require_pg().await;
    let client = connect(&url).await.unwrap();

    // One cut per suspension point: ParseComplete, ParameterDescription,
    // RowDescription, plus a run to completion.
    let mut ever_cancelled = false;
    for polls in 1..=5 {
        let before = count(&client).await;

        let finished = poll_then_drop(client.prepare("SELECT 1 AS cancel_probe"), polls).await;
        ever_cancelled |= !finished;
        assert!(
            polls > 1 || !finished,
            "one poll resolved a prepare - the driver task cannot have run, so \
             the test is no longer measuring what it claims"
        );

        // Barrier: FIFO ordering means the server has executed the Parse by
        // the time this returns.
        client.simple_query("").await.unwrap();

        let after = count(&client).await;
        assert_eq!(
            after,
            before,
            "a prepare cut after {polls} poll(s) ({}) left {} statement(s) on \
             the server that no client handle can close",
            if finished {
                "completed"
            } else {
                "dropped in flight"
            },
            after - before
        );
    }

    assert!(
        ever_cancelled,
        "no cut landed mid-flight - the test stopped exercising cancellation"
    );
}

// ---------------------------------------------------------------------------
// 33. frontend_encode_failure_is_not_blamed_on_the_server
//
// A query string with an interior NUL cannot be encoded as the C string the
// Parse message carries, so the request never reaches the server. Reporting
// that as `Kind::Parse` ("error parsing response from server") points the
// reader at a response that was never received; the failure is ours, in the
// frontend encoder, which is what `Kind::Encode` says. `prepare` already
// classifies the same call correctly - this pins the extended-query entry
// points to the same answer.
// ---------------------------------------------------------------------------

#[compio::test]
async fn frontend_encode_failure_is_not_blamed_on_the_server() {
    use compio_postgres::types::Type;

    let url = require_pg().await;
    let client = connect(&url).await.unwrap();

    let bad_sql = "SELECT $1::text -- \u{0} interior nul";

    let cases: Vec<(&str, Error)> = vec![
        (
            "query_typed",
            client
                .query_typed(bad_sql, &[(&"x", Type::TEXT)])
                .await
                .unwrap_err(),
        ),
        (
            "execute_typed",
            client
                .execute_typed(bad_sql, &[(&"x", Type::TEXT)])
                .await
                .unwrap_err(),
        ),
        (
            "query_text_params",
            client.query_text_params(bad_sql, &["x"]).await.unwrap_err(),
        ),
        (
            "execute_text_params",
            client
                .execute_text_params(bad_sql, &[Some("x".to_string())])
                .await
                .unwrap_err(),
        ),
        ("prepare", client.prepare(bad_sql).await.unwrap_err()),
    ];

    for (api, err) in cases {
        assert_eq!(
            err.to_string(),
            "error encoding message to server",
            "{api} blamed the server for a frontend encoding failure: {err:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Pool release hygiene: an open transaction must not survive into the next
// borrower.
//
// `Transaction`'s Drop already covers the typed API - it borrows the client
// mutably, so the `PooledClient` cannot be released while a `Transaction`
// lives, and Drop queues a ROLLBACK. Nothing covered a transaction opened as
// raw SQL (`BEGIN` via batch_execute / execute), which is what these tests
// pin.
// ---------------------------------------------------------------------------

/// A pool sized to exactly one connection, so a release and the next
/// acquisition are guaranteed to be the same backend session.
async fn single_connection_pool(url: &str) -> Pool {
    let mut config = PoolConfig::new();
    config
        .max_size(1)
        .min_idle(1)
        .validation_bypass(std::time::Duration::from_secs(60));
    Pool::connect_with_pool_config(url, config).await.unwrap()
}

#[compio::test]
async fn released_open_transaction_is_not_inherited_by_the_next_borrower() {
    let url = require_pg().await;
    let pool = single_connection_pool(&url).await;
    let table = common::test_object_name("tx_leak");

    {
        let client = pool.get().await.unwrap();
        client
            .batch_execute(&format!("CREATE TABLE {table} (id int)"))
            .await
            .unwrap();
    }

    // Open a transaction and write inside it in one simple-Query message,
    // then release the connection without committing or rolling back.
    {
        let client = pool.get().await.unwrap();
        client
            .batch_execute(&format!("BEGIN; INSERT INTO {table} VALUES (1); SELECT 1"))
            .await
            .unwrap();
        assert_eq!(
            client.transaction_status(),
            Some(TransactionStatus::InTransaction),
            "the session should be inside a transaction before release"
        );
    }

    let client = pool.get().await.unwrap();
    assert_eq!(
        client.transaction_status(),
        Some(TransactionStatus::Idle),
        "the next borrower inherited an open transaction"
    );
    // The uncommitted row is visible only from inside the transaction that
    // wrote it, so seeing it proves this borrower is still in that
    // transaction.
    let rows: i64 = client
        .query_one_scalar(&format!("SELECT count(*) FROM {table}"), &[])
        .await
        .unwrap();
    assert_eq!(
        rows, 0,
        "the next borrower saw the previous one's uncommitted row"
    );
}

#[compio::test]
async fn released_transaction_aborted_by_a_later_batch_is_not_inherited_by_the_next_borrower() {
    let url = require_pg().await;
    let pool = single_connection_pool(&url).await;

    // A failed statement inside a transaction leaves the session in the
    // "aborted" state, where every further statement is rejected until a
    // rollback. Releasing there would hand the next borrower a connection
    // that answers 25P02 to everything.
    {
        let client = pool.get().await.unwrap();
        client.batch_execute("BEGIN").await.unwrap();
        client
            .batch_execute("SELECT * FROM no_such_table_here")
            .await
            .unwrap_err();
        // ErrorResponse is delivered immediately; wait separately for the
        // trailing ReadyForQuery that makes the failed status authoritative.
        client.simple_query("").await.unwrap();
        assert_eq!(
            client.transaction_status(),
            Some(TransactionStatus::Failed),
            "the session should be in an aborted transaction before release"
        );
    }

    let client = pool.get().await.unwrap();
    let one: i32 = client.query_one_scalar("SELECT 1", &[]).await.unwrap();
    assert_eq!(one, 1, "the next borrower inherited an aborted transaction");
    assert_eq!(client.transaction_status(), Some(TransactionStatus::Idle));
}

#[compio::test]
async fn released_transaction_aborted_in_one_batch_is_not_inherited_by_the_next_borrower() {
    let url = require_pg().await;
    let pool = single_connection_pool(&url).await;

    {
        let client = pool.get().await.unwrap();
        let error = client
            .batch_execute("BEGIN; SELECT 1 / 0")
            .await
            .unwrap_err();
        assert_eq!(error.code(), Some(&SqlState::DIVISION_BY_ZERO));
    }

    let client = pool.get().await.unwrap();
    let one: i32 = match client.query_one_scalar("SELECT 1::int4", &[]).await {
        Ok(one) => one,
        Err(error) => panic!(
            "the next borrower inherited the aborted transaction: SQLSTATE {} ({error})",
            error.code().map_or("<none>", SqlState::code)
        ),
    };
    assert_eq!(one, 1);
    assert_eq!(client.transaction_status(), Some(TransactionStatus::Idle));
}

#[compio::test]
async fn older_response_stream_does_not_hide_a_later_failed_transaction() {
    use futures_util::StreamExt;

    let url = require_pg().await;
    let pool = single_connection_pool(&url).await;

    {
        let client = pool.get().await.unwrap();
        let older = client.simple_query_raw("").await.unwrap();
        let mut older = Box::pin(older);

        let first = older
            .next()
            .await
            .expect("the empty query response was not delivered")
            .unwrap();
        assert!(matches!(first, SimpleQueryMessage::CommandComplete(0)));

        let error = client
            .batch_execute("BEGIN; SELECT 1 / 0")
            .await
            .unwrap_err();
        assert_eq!(error.code(), Some(&SqlState::DIVISION_BY_ZERO));

        assert!(
            older.next().await.is_none(),
            "the older empty-query stream did not end at ReadyForQuery"
        );
    }

    let client = pool.get().await.unwrap();
    let one: i32 = match client.query_one_scalar("SELECT 1::int4", &[]).await {
        Ok(one) => one,
        Err(error) => panic!(
            "the older stream hid the failed transaction: SQLSTATE {} ({error})",
            error.code().map_or("<none>", SqlState::code)
        ),
    };
    assert_eq!(one, 1);
    assert_eq!(client.transaction_status(), Some(TransactionStatus::Idle));
}

#[compio::test]
async fn dropped_unpolled_begin_stream_is_not_inherited_by_the_next_borrower() {
    let url = require_pg().await;
    let pool = single_connection_pool(&url).await;

    {
        let client = pool.get().await.unwrap();
        let stream = client.simple_query_raw("BEGIN").await.unwrap();
        drop(stream);
    }

    let client = pool.get().await.unwrap();
    let one: i32 = client
        .query_one_scalar("SELECT 1::int4", &[])
        .await
        .unwrap();
    assert_eq!(one, 1);
    assert_eq!(
        client.transaction_status(),
        Some(TransactionStatus::Idle),
        "the next borrower inherited the unpolled stream's transaction"
    );
}

#[compio::test]
async fn errored_batch_in_an_implicit_transaction_rolls_back_session_changes_and_reports_idle() {
    let url = require_pg().await;
    let pool = single_connection_pool(&url).await;
    let channel = test_schema();
    let table = common::test_object_name("batch_error_temp");
    let backend_pid;

    {
        let client = pool.get().await.unwrap();
        backend_pid = client.process_id();
        client
            .batch_execute("SET application_name = 'cpg_before_error'")
            .await
            .unwrap();

        let error = client
            .batch_execute(&format!(
                "SET application_name = 'cpg_during_error'; \
                 LISTEN {channel}; \
                 CREATE TEMP TABLE {table} (id int); \
                 INSERT INTO {table} VALUES (0); \
                 SELECT 1 / id FROM {table}"
            ))
            .await
            .unwrap_err();
        assert_eq!(error.code(), Some(&SqlState::DIVISION_BY_ZERO));
        // The SQLSTATE is returned at ErrorResponse, before PostgreSQL's
        // trailing ReadyForQuery necessarily reaches the connection task.
        // This empty query is a FIFO barrier and changes no transaction state.
        client.simple_query("").await.unwrap();
        assert_eq!(
            client.transaction_status(),
            Some(TransactionStatus::Idle),
            "an errored implicit transaction must end idle"
        );
    }

    let client = pool.get().await.unwrap();
    assert_eq!(
        client.process_id(),
        backend_pid,
        "the pool recycled a connection that PostgreSQL reported idle"
    );
    let application_name: String = client
        .query_one_scalar("SELECT current_setting('application_name')", &[])
        .await
        .unwrap();
    assert_eq!(application_name, "cpg_before_error");

    let listeners: i64 = client
        .query_one_scalar(
            "SELECT count(*)::int8 \
             FROM pg_listening_channels() AS channels(channel) \
             WHERE channel = $1",
            &[&channel],
        )
        .await
        .unwrap();
    assert_eq!(listeners, 0, "LISTEN survived the implicit rollback");

    let temp_table: Option<String> = client
        .query_one_scalar("SELECT to_regclass($1)::text", &[&table])
        .await
        .unwrap();
    assert!(
        temp_table.is_none(),
        "the temporary table survived the implicit rollback"
    );
}

#[compio::test]
async fn released_session_changes_in_an_aborted_transaction_are_rolled_back() {
    let url = require_pg().await;
    let pool = single_connection_pool(&url).await;
    let channel = test_schema();
    let table = common::test_object_name("aborted_batch_temp");

    {
        let client = pool.get().await.unwrap();
        client
            .batch_execute("SET application_name = 'cpg_before_aborted_batch'")
            .await
            .unwrap();
        let error = client
            .batch_execute(&format!(
                "BEGIN; \
                 SET application_name = 'cpg_in_aborted_batch'; \
                 LISTEN {channel}; \
                 CREATE TEMP TABLE {table} (id int); \
                 INSERT INTO {table} VALUES (0); \
                 SELECT 1 / id FROM {table}"
            ))
            .await
            .unwrap_err();
        assert_eq!(error.code(), Some(&SqlState::DIVISION_BY_ZERO));
        // Preserve immediate error delivery while making the status assertion
        // wait for the failed batch's trailing ReadyForQuery.
        client.simple_query("").await.unwrap();
        assert_eq!(client.transaction_status(), Some(TransactionStatus::Failed));
    }

    let client = pool.get().await.unwrap();
    let one: i32 = client
        .query_one_scalar("SELECT 1::int4", &[])
        .await
        .unwrap();
    assert_eq!(one, 1);
    assert_eq!(client.transaction_status(), Some(TransactionStatus::Idle));

    let application_name: String = client
        .query_one_scalar("SELECT current_setting('application_name')", &[])
        .await
        .unwrap();
    assert_eq!(application_name, "cpg_before_aborted_batch");

    let listeners: i64 = client
        .query_one_scalar(
            "SELECT count(*)::int8 \
             FROM pg_listening_channels() AS channels(channel) \
             WHERE channel = $1",
            &[&channel],
        )
        .await
        .unwrap();
    assert_eq!(listeners, 0, "LISTEN survived the release rollback");

    let temp_table: Option<String> = client
        .query_one_scalar("SELECT to_regclass($1)::text", &[&table])
        .await
        .unwrap();
    assert!(
        temp_table.is_none(),
        "the temporary table survived the release rollback"
    );
}

#[compio::test]
async fn clean_release_hands_off_same_connection_without_queuing_rollback() {
    use std::rc::Rc;

    let url = require_pg().await;
    let pool = Rc::new(single_connection_pool(&url).await);
    let client = pool.get().await.unwrap();
    let backend_pid = client.process_id();
    let one: i32 = client
        .query_one_scalar("SELECT 1::int4", &[])
        .await
        .unwrap();
    assert_eq!(one, 1);
    assert_eq!(client.transaction_status(), Some(TransactionStatus::Idle));
    assert!(!client.is_dirty());

    let waiter = {
        let pool = Rc::clone(&pool);
        compio::runtime::spawn(async move {
            let client = pool.get().await.expect("the waiting borrower acquires");
            (client.process_id(), client.is_dirty())
        })
    };

    let mut spins = 0;
    while pool.pending_count() == 0 {
        yield_n(1).await;
        spins += 1;
        assert!(spins < 1000, "the waiting borrower never parked");
    }

    drop(client);
    let (next_pid, dirty) = waiter
        .await
        .unwrap_or_else(|error| std::panic::resume_unwind(error));
    assert_eq!(
        next_pid, backend_pid,
        "a clean connection was recycled instead of handed off"
    );
    assert!(!dirty, "a clean release needlessly queued ROLLBACK");
}

#[compio::test]
async fn release_rollback_keeps_session_state_the_next_borrower_may_rely_on() {
    let url = require_pg().await;
    let pool = single_connection_pool(&url).await;

    // Session-scoped state a borrower is allowed to hand off across a
    // release: an advisory lock (crates/plugin-db's LockGuard does exactly
    // this), a prepared statement (the driver's own type-info cache holds
    // these for the life of the Client), and a session GUC. Clearing the
    // transaction must not clear any of them - `DISCARD ALL` / `RESET ALL` /
    // `DEALLOCATE ALL` would.
    //
    // The advisory-lock key is database-wide, so it is derived from this
    // test's own schema name to keep it clear of the rest of the suite. The
    // acquisition is the non-blocking form: if a backend left over from an
    // earlier run still held the key, `pg_advisory_lock` would park here with
    // no timeout, which is a hang rather than a test result.
    let schema = test_schema();
    let statement;
    {
        let client = pool.get().await.unwrap();
        let got: bool = client
            .query_one_scalar(
                "SELECT pg_try_advisory_lock(hashtext($1)::int4)",
                &[&schema],
            )
            .await
            .unwrap();
        assert!(got, "another session is holding this test's advisory key");
        client
            .batch_execute("SET application_name = 'cpg_release_state'")
            .await
            .unwrap();
        statement = client.prepare("SELECT $1::int4 + 1").await.unwrap();

        // Leave a transaction open so the release path has to clear it.
        client.batch_execute("BEGIN").await.unwrap();
    }

    let client = pool.get().await.unwrap();
    assert_eq!(client.transaction_status(), Some(TransactionStatus::Idle));

    let held: i64 = client
        .query_one_scalar(
            "SELECT count(*) FROM pg_locks \
             WHERE locktype = 'advisory' AND pid = pg_backend_pid()",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(
        held, 1,
        "the release path dropped a session-scoped advisory lock"
    );

    let app_name: String = client
        .query_one_scalar("SELECT current_setting('application_name')", &[])
        .await
        .unwrap();
    assert_eq!(
        app_name, "cpg_release_state",
        "the release path reset a session GUC"
    );

    let bumped: i32 = client
        .query_one_scalar(&statement, &[&41i32])
        .await
        .unwrap();
    assert_eq!(
        bumped, 42,
        "the release path deallocated a prepared statement"
    );
}

// ---------------------------------------------------------------------------
// TLS: a connection string that requires encryption must fail against this
// plaintext server rather than quietly connect in the clear. WHY it fails
// differs by build, and both arms are asserted below - the reason is the part
// an operator reads.
// ---------------------------------------------------------------------------

#[compio::test]
async fn sslmode_require_fails_closed_over_a_plaintext_server() {
    // Called for its fixture setup and its reachability check; this test uses
    // a different URL below, so the returned one is deliberately dropped.
    let _ = require_pg().await;
    // A server with TLS switched OFF, not merely a connection that is not
    // using it - the second assertion below turns on the server answering `N`
    // to `SSLRequest`. Under `--features suite-over-tls` the ordinary test URL
    // names an ENCRYPTED server, where `sslmode=require` is satisfied and this
    // test would be asserting the opposite of its own name.
    let url = common::tls_disabled_url();
    let sep = if url.contains('?') { '&' } else { '?' };
    let require = format!("{url}{sep}sslmode=require");

    // `NoTls` explicitly, and NOT `common::suite_tls()`: the claim is that no
    // build of this crate lets NoTls satisfy `require`, so the transport is
    // the subject of the test rather than a detail of how it connects. Handing
    // it the suite connector under `--features suite-over-tls` made it fail on
    // attestation instead, which asserts something else entirely.
    let err = compio_postgres::connect(&require, NoTls)
        .await
        .err()
        .expect("sslmode=require must not succeed over a plaintext connection");
    // "could not be negotiated", NOT "handshake failed". The two are separate
    // error kinds because `sslmode=prefer` retries a failed HANDSHAKE in
    // plaintext and must retry nothing else; nothing was handshaken here, so
    // this is the negotiation kind. Getting the pair backwards would give
    // `prefer` a plaintext retry after a refusal it should have accepted on
    // the same socket.
    assert_eq!(err.to_string(), "TLS could not be negotiated");

    let err = Pool::connect(&require, 2)
        .await
        .expect_err("the pool must not satisfy sslmode=require in the clear");
    // `Error`'s own Display is a category; the cause carries the detail.
    let cause = std::error::Error::source(&err)
        .map(ToString::to_string)
        .unwrap_or_default();

    // Without the `tls` feature the pool has no connector at all, and says so
    // before opening a socket. With it, the pool builds a rustls connector,
    // gets as far as `SSLRequest`, and the server's `N` is the failure.
    #[cfg(not(feature = "tls"))]
    assert!(
        cause.contains("sslmode=require") && cause.contains("`tls` feature"),
        "the refusal should name the unsatisfiable setting and why, got: {cause}"
    );
    #[cfg(feature = "tls")]
    assert!(
        cause.contains("does not support SSL"),
        "the failure should be the server's refusal, got: {cause}"
    );
}

/// `sslnegotiation=direct` under `sslmode=prefer` is the one TLS combination no
/// build can serve: a mode that permits plaintext must not drive a TLS-only
/// handshake, because a direct handshake sends no `SSLRequest` and so has no
/// negotiation to fall back from. libpq rejects the pairing in
/// `connectOptions2`; this driver rejects it in `Config::validate_tls_settings`,
/// which every entry point calls - so the pool answers before spending its
/// retry budget, and `Config::connect` answers before opening a socket.
#[compio::test]
async fn sslnegotiation_direct_under_prefer_is_rejected_by_the_pool() {
    let url = require_pg().await;
    let sep = if url.contains('?') { '&' } else { '?' };
    let direct = format!("{url}{sep}sslmode=prefer&sslnegotiation=direct");

    let err = Pool::connect(&direct, 2)
        .await
        .expect_err("prefer + direct is unsatisfiable in every build");
    let cause = std::error::Error::source(&err)
        .map(ToString::to_string)
        .unwrap_or_default();
    assert!(
        cause.contains("sslnegotiation=direct"),
        "the refusal should name the unsatisfiable setting, got: {cause}"
    );
}

// ---------------------------------------------------------------------------
// Connection lifetime
//
// The invariant these two tests hold is a LIFETIME, not a ceiling: a
// connection opened inside a `compio` runtime is gone from the server when
// that runtime ends, not when the process ends. A bounded-but-nonzero series
// satisfies "stays under max_connections" and still breaks every caller that
// needs the session released - `CREATE DATABASE ... TEMPLATE x` refuses while
// ONE other session is on `x`.
//
// They are plain `#[test]`, not `#[compio::test]`: the runtime is the subject,
// so the test has to build and drop it itself and then look at the server from
// outside it.
// ---------------------------------------------------------------------------

/// How many backends on `url` carry `application_name = tag`.
///
/// Runs in a runtime of its own, which it also drops - so if the leak this
/// guards against ever came back, the observer would be leaking too. That is
/// deliberate: the observer's own connections are untagged and therefore never
/// counted, so a broken observer inflates nothing.
fn tagged_backends(url: &str, tag: &str) -> i64 {
    let rt = compio::runtime::Runtime::new().expect("cannot create runtime");
    rt.block_on(async {
        let client = connect(url).await.unwrap();
        let rows = client
            .query(
                "SELECT count(*)::int8 AS n FROM pg_stat_activity WHERE application_name = $1",
                &[&tag],
            )
            .await
            .unwrap();
        rows[0].get::<_, i64>("n")
    })
}

/// Descriptors this process holds open. A socket that outlives its runtime
/// shows up here as well as in `pg_stat_activity`, so the pair separates "the
/// server dropped the session" from "we still own the fd".
fn open_fds() -> usize {
    std::fs::read_dir("/proc/self/fd")
        .expect("/proc/self/fd")
        .count()
}

#[test]
fn a_connection_does_not_outlive_the_runtime_that_opened_it() {
    let url = test_url();
    let tag = common::test_object_name("cpg_runtime_lifetime");
    let sep = if url.contains('?') { '&' } else { '?' };
    let tagged = format!("{url}{sep}application_name={tag}");

    let mut backends = Vec::new();
    let mut fds = Vec::new();
    for _ in 0..6 {
        let rt = compio::runtime::Runtime::new().expect("cannot create runtime");
        rt.block_on(async {
            let client = match connect(&tagged).await {
                Ok(client) => client,
                Err(e) => common::postgres_unreachable(&tagged, &e),
            };
            let rows = client.query("SELECT 1::int4 AS one", &[]).await.unwrap();
            assert_eq!(rows[0].get::<_, i32>("one"), 1);
        });
        drop(rt);
        backends.push(tagged_backends(&url, &tag));
        fds.push(open_fds());
    }

    println!("tagged backends after each runtime drop: {backends:?}");
    println!("open fds after each runtime drop:        {fds:?}");

    assert_eq!(
        backends,
        vec![0; 6],
        "every runtime opened one connection and dropped it; the server should \
         hold none between iterations, got {backends:?}"
    );

    // The fd series is printed, not asserted, and the test below is why: in
    // THIS process the number moves for reasons that have nothing to do with
    // the runtime being dropped.
}

/// Name of the test below, needed as a literal because it re-executes itself.
const FD_PROBE_TEST: &str =
    "a_torn_down_runtime_leaks_two_descriptors_plus_one_per_live_connection";

/// The name the HARNESS knows that test by, which is what `--exact` matches.
///
/// This file is a module inside the consolidated suite binary, so the harness
/// calls the test `integration::<name>` rather than `<name>`. The bare literal
/// selected nothing, the child ran `0 tests`, and the parent then failed on the
/// missing probe line rather than on anything about descriptors - a filter miss
/// wearing the costume of a leak. Derived from `module_path!` rather than
/// spelled with a prefix so that moving this file again cannot silently
/// reintroduce it; libtest omits the crate root, hence dropping the first
/// segment. Empty means this file is a crate root of its own, where the bare
/// name is already right.
fn fd_probe_test_filter() -> String {
    match module_path!().split_once("::") {
        Some((_crate_root, module)) if !module.is_empty() => format!("{module}::{FD_PROBE_TEST}"),
        _ => FD_PROBE_TEST.to_owned(),
    }
}

/// Set in the re-executed child. Its presence selects the measuring arm.
///
/// The spelling lives in the sealed key enum, not here. `clippy.toml` denies
/// `std::env::var_os`, and the one place in this crate permitted to read the
/// environment is `common::env::get`, which takes the key rather than a name -
/// so the read below cannot use a local `&str` constant, and keeping one for
/// the WRITE side alone would be the same literal in two files.
const FD_PROBE_CHILD: common::env::TestEnvKey = common::env::TestEnvKey::FdProbeChild;

/// Marks the child's machine-readable result lines: `<marker><arm> <csv>`.
const FD_PROBE_MARKER: &str = "FD-PROBE-SERIES ";

/// Create/drop cycles per arm. Twenty, so a per-iteration constant is a slope
/// and not a pair of readings that happen to differ.
const FD_PROBE_ITERATIONS: usize = 20;

/// What a torn-down runtime leaks on its own, once a single in-flight
/// submission has stopped it being reclaimed: the `io_uring` ring and the
/// eventfd the driver notifies through. Measured, not assumed - see the arms
/// below and the identity table in the doc comment.
const LEAKED_RUNTIME_FDS: i64 = 2;

#[cfg(not(feature = "suite-over-tls"))]
const ONE_CONNECTION_ABRUPT_FDS: i64 = 1 + LEAKED_RUNTIME_FDS;
#[cfg(feature = "suite-over-tls")]
const ONE_CONNECTION_ABRUPT_FDS: i64 = 2;

/// The descriptor half of the invariant above, which `crate::release` does NOT
/// fix and is not trying to.
///
/// Measured 2026-08-20 against the same binary with `Socket::release_handle`
/// forced to `None`: the backend series went `[1,2,3,4,5,6]` and the fd series
/// stayed `[10,16,22,28,34,40]` - byte for byte what it is with the release in
/// place. The release ends the SESSION, not the descriptor.
///
/// # What actually leaks, and why it is not one descriptor
///
/// Re-measured 2026-08-20 by reading `/proc/self/fd` targets rather than
/// counting entries, over 24 create/drop cycles in a process doing nothing
/// else. Over plaintext, one detached connection per runtime leaks exactly
/// three, and they are not three sockets:
///
/// ```text
/// anon_inode:[io_uring]   the ring
/// anon_inode:[eventfd]    the driver's notify handle
/// socket:[...]            the connection
/// ```
///
/// So the unit that leaks is THE WHOLE RUNTIME plus one descriptor per
/// connection that still had a submission in flight. Two connections per
/// runtime leak four, not six - measured `[4,4,4,...]` over 24 cycles - which
/// is why the arms below assert `connections + 2` and not a flat budget. The
/// prior version of this asserted `<= 3`, a number that is only the truth at
/// one connection per runtime; a two-connection test would have tripped it,
/// and the message told the reader to raise the bound.
///
/// TLS has a different exact one-connection shape. After its synchronous
/// `close_notify` plus socket shutdown, the measured remainder is the eventfd
/// and socket, `[2,2,2,...]`, with no ring descriptor. Two TLS connections
/// still measure four, and the drained arm still measures zero. The
/// mode-specific assertion below remains exact; it is not an upper bound that
/// can hide a new descriptor.
///
/// The mechanism is not postgres and is not this crate. A bare
/// `compio::net::TcpStream` read on a detached task leaks the same three.
/// `compio_runtime::runtime::Submit` holds a strong `Rc<RuntimeInner>`, so a
/// submission that is still pending at teardown makes `Runtime::drop` see
/// `Rc::strong_count > 1` and take its early return without calling
/// `scheduler.clear()`. What is left is an Rc cycle - `RuntimeInner` ->
/// `Scheduler` -> task -> `Submit` -> `RuntimeInner` - so the `Proactor`, and
/// with it the ring and the eventfd, is never dropped. Confirmed by patching
/// that drop to print the count: 2 on the leaking arm, 1 on a detached task
/// that is pending on something other than a submission, which leaks nothing.
/// That early return is not a bug to delete, either: forcing the clear made
/// live queries fail, because `Runtime::drop` also runs for the transient
/// handles `Submit` clones mid-run.
///
/// # It is reclaimable, and the drained arm is the proof
///
/// Any teardown that leaves no submission in flight leaks zero. Awaiting the
/// driver task, cancelling it and letting the runtime reap the cancellation,
/// and [`compio_postgres::drain_connections`] all measured `[0,0,0,...]` over
/// 24 cycles. The drained arm below is the one this crate ships an API for, so
/// it is the one that is guarded: without it, nothing here would notice
/// `drain_connections` silently ceasing to drain.
///
/// # Why the cost is worth a guard at all
///
/// It multiplies across a consolidated test binary and never comes back. This
/// crate's own `integration` target, 98 tests in one process at
/// `--test-threads=1`, was watched from outside on 2026-08-20: the count went
/// `7 -> 301`, ending on 198 `anon_inode` (99 runtimes x 2) and 109 sockets.
/// `crates/auth/tests/main.rs` is 257 tests in one process. What the cost
/// surfaces as, when it does, is EMFILE against a 1024 soft `RLIMIT_NOFILE` in
/// a test unrelated to whatever raised it.
///
/// # What this does NOT catch
///
/// Nothing outside a test process. Every `Runtime::new` in `crates/` and
/// `libs/` outside a `tests/` directory is inside a `#[cfg(test)]` module;
/// the services build one runtime per thread and hold it for the life of the
/// process, so this cost is zero in production by construction, and a change
/// that made a service tear runtimes down in a loop would not go red here.
///
/// # Why this re-executes itself
///
/// `/proc/self/fd` is per-PROCESS, and libtest runs this file's tests on
/// several threads of one process by default. Sibling tests opening and
/// closing their own connections move the count underneath the loop, so the
/// per-drop delta measures them too. Measured 2026-08-20, same binary, same
/// database, same box, `--test-threads` the only variable:
///
///   default:            fds [78,122,165,170,171,176]  deltas [44,43,5,1,5]
///   --test-threads=1:   fds [10, 16, 22, 28, 34, 40]  deltas [6,6,6,6,6]
///
/// (That serial series reads six per iteration for a three-per-runtime cost
/// because the test above tears down TWO runtimes per iteration:
/// `tagged_backends` builds one of its own to ask the server its question.)
///
/// An earlier version of this asserted the budget inline and passed only
/// because it had been run serially - and it did not merely mis-measure, it
/// PANICKED with "attempt to subtract with overflow" when a sibling closed
/// more descriptors than the runtime leaked and the count went DOWN.
///
/// Tagging the descriptors the way the backend count is tagged does not rescue
/// it: the siblings connect to the same database on the same port, so nothing
/// observable on the socket separates their descriptors from this test's.
/// Machine load is not the contaminant either - other PROCESSES cannot appear
/// in `/proc/self/fd` - so the fix is not to tolerate the churn but to remove
/// it, by doing the measuring in a child process that runs this test and
/// nothing else. That is isolated by construction rather than by a convention
/// the next runner has to know.
#[test]
fn a_torn_down_runtime_leaks_two_descriptors_plus_one_per_live_connection() {
    if common::env::get(FD_PROBE_CHILD).is_some() {
        measure_and_report_fd_series();
        return;
    }

    let exe = std::env::current_exe().expect("current_exe");
    let output = std::process::Command::new(&exe)
        .args([
            "--exact",
            &fd_probe_test_filter(),
            "--nocapture",
            "--test-threads=1",
        ])
        .env(FD_PROBE_CHILD.name(), "1")
        .output()
        .expect("re-exec the test binary");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "the isolated child failed.\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );

    // One connection per runtime, undrained. Plaintext retains the ring,
    // eventfd, and socket; TLS retains only the eventfd and socket.
    assert_leak_per_runtime(&stdout, "one-connection", ONE_CONNECTION_ABRUPT_FDS);
    // Two, undrained. This is the arm the old flat budget of three would have
    // failed, and the reason the expectation is a law rather than a number.
    assert_leak_per_runtime(&stdout, "two-connections", 2 + LEAKED_RUNTIME_FDS);
    // Drained before the runtime goes. Nothing is in flight, so nothing is
    // stranded - including the runtime itself.
    assert_leak_per_runtime(&stdout, "drained", 0);
}

/// Parses one arm's series out of the child's stdout and asserts every
/// create/drop cycle moved the descriptor count by exactly `expected`.
///
/// Exact, not an upper bound, in both directions on purpose. A HIGHER number
/// means a teardown now strands more submissions than it did. A LOWER one
/// means the cost this file documents at length has gone away - most likely a
/// compio upgrade - and the doc comment above is now wrong, which is worth a
/// red run rather than a quietly passing `<=`.
fn assert_leak_per_runtime(stdout: &str, arm: &str, expected: i64) {
    // Searched for anywhere in the line, not as a prefix: libtest writes
    // `test <name> ... ` without a newline and the child's first `println!`
    // lands on the end of it, so the marker is mid-line on the run that
    // matters.
    let needle = format!("{FD_PROBE_MARKER}{arm} ");
    let Some((_, series)) = stdout.lines().find_map(|line| line.split_once(&needle)) else {
        panic!("child printed no `{needle}` line.\n--- stdout ---\n{stdout}")
    };
    let fds: Vec<i64> = series
        .split(',')
        .map(|n| n.trim().parse().expect("fd count"))
        .collect();
    assert_eq!(
        fds.len(),
        FD_PROBE_ITERATIONS + 1,
        "arm {arm} should report a baseline plus {FD_PROBE_ITERATIONS} samples, got {fds:?}"
    );

    // Signed, so a count that goes DOWN is a number this reports rather than a
    // panic inside the assertion that was supposed to describe it.
    let per_runtime: Vec<i64> = fds.windows(2).map(|w| w[1] - w[0]).collect();
    println!("[{arm}] open fds in the isolated child: {fds:?}");
    println!("[{arm}] descriptors leaked per runtime: {per_runtime:?}");

    assert!(
        per_runtime.iter().all(|&d| d == expected),
        "arm {arm}: every runtime teardown should move the descriptor count by \
         exactly {expected}; got {per_runtime:?} from {fds:?}. Read the doc \
         comment before changing this number."
    );
}

/// The child arm of
/// [`a_torn_down_runtime_leaks_two_descriptors_plus_one_per_live_connection`].
///
/// Runs the three arms back to back in this one process and prints a series
/// per arm on a line the parent parses. Sampling `/proc/self/fd` is only
/// meaningful here because the parent invoked this process with `--exact` and
/// `--test-threads=1`, so no other test shares it.
/// Each arm runs on a thread of its own, and that is load-bearing rather than
/// tidy. [`compio_postgres::live_connections`] counts per THREAD, and a
/// connection abandoned by an abrupt teardown is never dropped, so its guard
/// never decrements. Run the drained arm on a thread the abrupt arms have
/// already used and `drain_connections` waits out its whole timeout on
/// connections that no longer exist - which is exactly how this was first
/// written, and it failed with "the drivers did not finish". Threads run one
/// at a time here; `/proc/self/fd` is per-process, so the counts still compose.
fn measure_and_report_fd_series() {
    let url = test_url();
    for (arm, connections, teardown) in [
        ("one-connection", 1, Teardown::Abrupt),
        ("two-connections", 2, Teardown::Abrupt),
        ("drained", 1, Teardown::Drained),
    ] {
        let url = url.clone();
        let fds = std::thread::spawn(move || fd_series(&url, connections, teardown))
            .join()
            .expect("fd probe arm panicked");
        report_fd_series(arm, &fds);
    }
}

/// How the arm ends its `block_on` before the runtime is dropped.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Teardown {
    /// Let `block_on` return with the driver tasks still parked on a read.
    /// This is what every `#[compio::test]` in the tree does today.
    Abrupt,
    /// Drop the clients and wait for the drivers to finish, so no submission
    /// is in flight when the runtime goes.
    Drained,
}

/// Opens `connections` connections inside a fresh runtime, tears the runtime
/// down `teardown`-wise, and does that [`FD_PROBE_ITERATIONS`] times.
///
/// Returns the descriptor count before the first runtime and after each
/// teardown, so the caller gets exactly one delta per cycle. The baseline is
/// included rather than warmed away: a one-time cost on the first cycle is
/// something this should name, not absorb.
fn fd_series(url: &str, connections: usize, teardown: Teardown) -> Vec<usize> {
    let sep = if url.contains('?') { '&' } else { '?' };
    let tag = common::test_object_name("cpg_fd_probe");
    let tagged = format!("{url}{sep}application_name={tag}");

    let mut fds = Vec::with_capacity(FD_PROBE_ITERATIONS + 1);
    fds.push(open_fds());
    for _ in 0..FD_PROBE_ITERATIONS {
        let rt = compio::runtime::Runtime::new().expect("cannot create runtime");
        rt.block_on(async {
            let mut clients = Vec::with_capacity(connections);
            for _ in 0..connections {
                let client = match connect(&tagged).await {
                    Ok(client) => client,
                    Err(e) => common::postgres_unreachable(&tagged, &e),
                };
                let rows = client.query("SELECT 1::int4 AS one", &[]).await.unwrap();
                assert_eq!(rows[0].get::<_, i32>("one"), 1);
                clients.push(client);
            }
            if teardown == Teardown::Drained {
                drop(clients);
                assert!(
                    compio_postgres::drain_connections(std::time::Duration::from_secs(5)).await,
                    "the drivers did not finish; a live handle would keep them counted"
                );
            }
        });
        drop(rt);
        fds.push(open_fds());
    }
    fds
}

/// Prints one arm's series on the machine-readable line the parent parses.
fn report_fd_series(arm: &str, fds: &[usize]) {
    let series: Vec<String> = fds.iter().map(ToString::to_string).collect();
    println!("{FD_PROBE_MARKER}{arm} {}", series.join(","));
}

/// The one-variable partner. The test above would also pass if the connection
/// were never opened - `postgres_unreachable` guards the total failure, but a
/// connection that closes too EARLY reads identically to one that closes on
/// time. This asserts the count is 1 while the runtime is still running, so
/// the 0s above mean "released", not "never taken".
#[test]
fn a_connection_is_visible_to_the_server_while_its_runtime_runs() {
    let url = test_url();
    let tag = common::test_object_name("cpg_runtime_lifetime_live");
    let sep = if url.contains('?') { '&' } else { '?' };
    let tagged = format!("{url}{sep}application_name={tag}");

    let rt = compio::runtime::Runtime::new().expect("cannot create runtime");
    let seen = rt.block_on(async {
        let client = match connect(&tagged).await {
            Ok(client) => client,
            Err(e) => common::postgres_unreachable(&tagged, &e),
        };
        let rows = client
            .query(
                "SELECT count(*)::int8 AS n FROM pg_stat_activity WHERE application_name = $1",
                &[&tag.as_str()],
            )
            .await
            .unwrap();
        rows[0].get::<_, i64>("n")
    });
    drop(rt);

    assert_eq!(seen, 1, "the live connection should see itself");
}

/// A server-sent refusal must reach the reader with the SQLSTATE it carried.
///
/// `compio_postgres::Error`'s own `Display` renders EVERY `Kind::Db` as the
/// literal string `"db error"`, and `postgres_unreachable` used to format only
/// that. Measured 2026-08-20 with `crate::release` disabled against a live,
/// healthy server at its `max_connections` ceiling: nine tests in this file
/// failed reporting `error: db error` and told the reader to provision a
/// database that was already up. `53300` never appeared in the output.
///
/// The error here is a real one off the wire rather than a synthesised chain,
/// because the property under test is that the `DbError` at the bottom of a
/// real `Error` is found and read.
#[compio::test]
async fn a_server_refusal_reaches_the_reader_with_its_sqlstate() {
    let url = require_pg().await;
    let client = connect(&url).await.unwrap();

    let err = client
        .query("SELECT * FROM a_table_that_does_not_exist", &[])
        .await
        .expect_err("querying a missing table must fail");

    // What the old formatting produced, and all it produced.
    assert_eq!(err.to_string(), "db error");

    let chain = common::error_chain(&err);
    assert!(
        chain.contains("42P01"),
        "the chain must carry the SQLSTATE the server sent, got {chain:?}"
    );
    assert!(
        chain.contains("a_table_that_does_not_exist"),
        "the chain must carry the server's message, got {chain:?}"
    );
    assert!(
        common::server_answered(&err),
        "a DbError means PostgreSQL composed and sent this, so it answered"
    );
}

/// The one-variable partner. The test above proves the chain walk RUNS; only
/// this proves it DISCRIMINATES. A `server_answered` that returned `true`
/// unconditionally would pass the test above and would put "the server
/// ANSWERED, so it is running" on top of a connection refused - the same class
/// of wrong claim, pointed the other way.
///
/// Port 1 is dialled rather than a closed high port: the low ports are
/// reserved, so nothing can be listening there by accident and make this pass
/// for the wrong reason.
#[compio::test]
async fn nothing_listening_is_not_reported_as_a_server_answer() {
    let err = compio_postgres::connect("postgres://postgres:zeroship@127.0.0.1:1/zeroship", NoTls)
        .await
        .err()
        .expect("nothing listens on port 1");

    assert!(
        !common::server_answered(&err),
        "no server replied, so the provisioning advice is the correct one"
    );
    let chain = common::error_chain(&err);
    assert!(
        !chain.contains("SQLSTATE"),
        "a transport failure carries no SQLSTATE to print, got {chain:?}"
    );
}

/// The consequence that motivated this, stated as the thing a caller actually
/// wants to do.
///
/// `CREATE DATABASE <new> WITH TEMPLATE <src>` is refused - SQLSTATE 55006,
/// "source database is being accessed by other users" - while ANY other session
/// is on `src`. ONE is enough. So a suite that clones a template between tests
/// needs the previous test's connection to be GONE, and a count that merely
/// stays under `max_connections` does not give it that. This is the
/// discriminating consequence of a lifetime over a ceiling, and the reason the
/// invariant above is written the way it is.
///
/// It does NOT generalise to "a per-test template clone is now safe". It shows
/// only that a connection whose CLIENT has been dropped stops blocking one. A
/// fixture that keeps a `Client` or a `Pool` alive across tests still holds a
/// session on the template and still blocks the clone - the release is tied to
/// the client's lifetime, which is the point, not to the test's.
///
/// Uses a template of its own rather than the suite's database: every other
/// test here is on that one, and at full parallelism one of them would be the
/// "1 other session", which would make this fail for a reason that has nothing
/// to do with what it asserts.
#[test]
fn a_template_clone_is_not_blocked_by_the_previous_runtime() {
    let url = test_url();
    let (base, _) = url.rsplit_once('/').expect("a database in the DSN");
    let template = common::test_object_name("cpg_template_lifetime_src");
    let clone = common::test_object_name("cpg_template_lifetime_clone");
    // The session issuing the clone must not itself be ON the template, or it
    // would be the one other session and this would fail on its own connection.
    let admin_url = format!("{base}/postgres");
    let template_url = format!("{base}/{template}");

    let cleanup_statements = vec![
        format!("DROP DATABASE IF EXISTS {clone} WITH (FORCE)"),
        format!("DROP DATABASE IF EXISTS {template} WITH (FORCE)"),
    ];
    // Armed before provisioning, so every panic path attempts both drops.
    let cleanup = BoundedSqlCleanup::new(
        format!("template databases {template} and {clone}"),
        admin_url.clone(),
        cleanup_statements.clone(),
    );

    // Fresh both ways: a leftover clone from an earlier run would make the
    // CREATE fail, and a leftover template would make it pass without this
    // test's own connection ever having been on it. This is recovery for a
    // killed process whose PID was reused, not the normal cleanup path.
    execute_admin_sql_bounded(&admin_url, &cleanup_statements, true)
        .expect("removing stale template fixtures");
    execute_admin_sql_bounded(&admin_url, &[format!("CREATE DATABASE {template}")], false)
        .expect("provisioning the template");

    // One test's shape: open a connection on the template, use it, end the
    // runtime.
    let rt = compio::runtime::Runtime::new().expect("cannot create runtime");
    rt.block_on(compio::time::timeout(ADMIN_BATCH_TIMEOUT, async {
        let client = connect(&template_url)
            .await
            .expect("connect to the template");
        client.execute("SELECT 1", &[]).await.unwrap();
    }))
    .expect("template connection exceeded its outer timeout");
    drop(rt);

    // The next test's fixture.
    let result = execute_admin_sql_bounded(
        &admin_url,
        &[format!("CREATE DATABASE {clone} WITH TEMPLATE {template}")],
        false,
    );

    // Explicit on the ordinary path; the guard's Drop also covers every panic
    // above. Both database drops are attempted even if the first one fails.
    drop(cleanup);

    if let Err(cause) = result {
        assert_eq!(
            cause.sqlstate.as_deref(),
            Some(SqlState::OBJECT_IN_USE.code()),
            "expected 55006 from the template clone, got: {cause}"
        );
        panic!(
            "the previous runtime's connection still holds the template open, \
             so a per-test clone cannot run: {cause}"
        );
    }
}

/// A result set far larger than the driver's single-frame cap must still
/// stream, because that cap bounds ONE message and not the run of messages a
/// query answers with.
///
/// `read_backend` used to refill the socket whenever a partial message sat at
/// the tail of the read buffer, instead of returning the complete prefix the
/// way tokio-postgres's `decode` does. A dense run of small `DataRow`s
/// therefore accumulated untouched until `BufStream::fill` refused a request
/// above `MAX_MESSAGE_SIZE`, and the query died with `message too large`
/// though its biggest single message was 16 KB. Measured before the fix: the
/// buffer reached exactly 67108864 bytes and the next refill was refused.
///
/// THE ROW WIDTH IS LOAD-BEARING, NOT A ROUND NUMBER. 16373 payload bytes
/// makes each `DataRow` exactly 16384 bytes on the wire (1 tag + 4 length + 2
/// field count + 4 field length + payload), which is `READ_CHUNK`. Rows and
/// reads then advance in lockstep, so the leftover at the tail does not land in
/// the under-5-bytes window that let the old code drain by luck. At an
/// unaligned width it does: 4000-byte payloads drained every ~800 chunks and an
/// earlier draft of this test PASSED against the bug, peaking at 13 MB. Change
/// the width and this stops exercising anything.
///
/// THIS IS NOT A SCHEDULE-INDEPENDENT GUARD, and must not be read as one.
/// `READ_CHUNK` is a MAXIMUM: `AsyncRead::read` may return any positive count,
/// so a short read can still walk the tail into the escape window and let even
/// the old decoder drain. The guarantee lives in
/// `codec::tests::a_partial_tail_does_not_hold_back_the_complete_messages_before_it`,
/// which scripts the chunks and so removes the transport from the experiment.
/// What this test adds is the end-to-end fact that a real PostgreSQL streaming
/// a real result set past the cap is served.
///
/// 8000 rows of 16384 bytes is 131072000 bytes, about 125 MiB against a 64 MiB
/// cap, and PostgreSQL sends it with no async message to break the run.
/// `query` materialises every row, so this is a framing test that costs real
/// memory rather than a bounded-memory streaming test.
#[compio::test]
async fn a_result_set_larger_than_the_single_frame_cap_still_streams() {
    let url = require_pg().await;
    let client = connect(&url).await.unwrap();

    let rows = client
        .query(
            "SELECT repeat('x', 16373) AS payload FROM generate_series(1, 8000)",
            &[],
        )
        .await
        .unwrap_or_else(|e| panic!("125 MiB result set refused: {}", common::error_chain(&e)));

    assert_eq!(rows.len(), 8000, "wrong row count for the large result set");
    assert_eq!(
        rows[7999].get::<_, &str>("payload").len(),
        16373,
        "last row truncated"
    );
}

/// `max_size` is a ceiling, so a pool must never open or hand out more
/// connections than it.
///
/// `Pool::connect(url, n)` sets `max_size` and leaves every other knob at its
/// default, including `min_idle: 2`. Warmup opened `min_idle.max(1)`
/// connections with no reference to `max_size`, so `Pool::connect(url, 1)`
/// came back holding two and handed out both. Measured before the fix:
/// `total=2 idle=2`, and a second checkout succeeded while the first was
/// still held. A per-tenant connection budget that the pool silently doubles
/// is not a budget.
///
/// The convenience constructor is the only one that can produce this, because
/// it is the only one that lets a caller set `max_size` without also seeing
/// `min_idle`.
#[compio::test]
async fn a_pool_never_opens_more_connections_than_its_max_size() {
    let url = require_pg().await;

    let pool = Pool::connect(&url, 1).await.unwrap();
    assert_eq!(
        pool.total_count(),
        1,
        "warmup opened more connections than max_size"
    );

    let held = pool.get().await.unwrap();
    let second = compio::time::timeout(std::time::Duration::from_secs(2), pool.get()).await;
    assert!(
        second.is_err(),
        "a second connection was handed out while max_size=1 was already checked out"
    );
    drop(held);
}

/// The control for the test above: a pool whose `max_size` leaves room for the
/// default `min_idle` must still warm up to `min_idle`, not be clamped down to
/// one connection. Without this, "never exceed max_size" could be satisfied by
/// warming a single connection always, which would cost every pool its warm
/// start.
#[compio::test]
async fn a_pool_with_room_still_warms_up_to_min_idle() {
    let url = require_pg().await;

    let expected = PoolConfig::default().get_min_idle();
    let pool = Pool::connect(&url, 8).await.unwrap();
    assert_eq!(
        pool.total_count(),
        expected,
        "warmup did not reach the default min_idle when max_size allowed it"
    );
}

/// The two configurations the pool refuses must actually be refused, and must
/// say so at construction rather than at the first checkout.
///
/// Both were added with the warm-set fix and neither had coverage. The
/// `max_size = 0` arm matters most: without it the pool constructs `Ok`,
/// never opens a connection, and then parks every `get()` on a waiter nothing
/// can wake, so the caller pays the full `acquire_timeout` - 30s by
/// default - to learn what the constructor already knew.
///
/// `min_idle > max_size` is reachable without setting `min_idle`, because
/// `PoolConfig::default()` supplies 2 when a caller lowers only `max_size`.
#[compio::test]
async fn a_pool_refuses_a_configuration_it_cannot_honour() {
    let url = require_pg().await;

    let mut zero_config = PoolConfig::new();
    zero_config.max_size(0).min_idle(0);
    let zero = Pool::connect_with_pool_config(&url, zero_config).await;
    assert!(
        zero.is_err(),
        "a pool with max_size 0 was constructed; every checkout on it would time out"
    );

    let mut inverted_config = PoolConfig::new();
    inverted_config.max_size(1).min_idle(4);
    let inverted = Pool::connect_with_pool_config(&url, inverted_config).await;
    assert!(
        inverted.is_err(),
        "a pool was constructed with min_idle above max_size"
    );

    // The control: the shape that reaches the refusal by accident must still
    // work once the two numbers agree.
    let mut ok_config = PoolConfig::new();
    ok_config.max_size(1).min_idle(1);
    let ok = Pool::connect_with_pool_config(&url, ok_config).await;
    assert!(ok.is_ok(), "a coherent single-connection pool was refused");
}

/// Dropping the client is the ordinary way to close a connection, and the
/// connection task must report that as success.
///
/// `Connection::run`'s documented use is `spawn(async { if let Err(e) =
/// connection.run().await { log(e) } })` - the shape every caller in this
/// repo and in tokio-postgres's own README uses. If a routine close resolves
/// to `Err`, every application logs a connection error on every close, and a
/// REAL failure becomes indistinguishable from shutting down. The shutdown
/// path already treats an undeliverable `Terminate` as a courtesy rather than
/// an error for exactly this reason (`connection.rs`, the `terminate_sent`
/// branch), so `run` resolving to `Err` here defeats that intent.
#[compio::test]
async fn dropping_the_client_closes_the_connection_without_an_error() {
    let url = require_pg().await;

    let (client, connection) = compio_postgres::connect(&url, common::suite_tls())
        .await
        .unwrap();
    let task = compio::runtime::spawn(async move { connection.run().await });

    client.execute("SELECT 1", &[]).await.unwrap();
    drop(client);

    let outcome = task.await.expect("the connection task panicked");
    if let Err(e) = outcome {
        panic!(
            "a clean drop resolved run() to an error: {}",
            common::error_chain(&e)
        );
    }
}

/// The clean-close rule must not swallow a genuine failure.
///
/// With the client still alive and holding an in-flight query, losing the
/// backend under it is a connection error and must be reported. Without this
/// test the clean-close rule could be satisfied by absorbing read errors as
/// well as undeliverable housekeeping writes.
#[compio::test]
async fn losing_the_backend_under_a_live_client_is_still_an_error() {
    let url = require_pg().await;

    let (client, connection) = compio_postgres::connect(&url, common::suite_tls())
        .await
        .unwrap();
    let task = compio::runtime::spawn(async move { connection.run().await });

    // Terminating our own backend mid-statement leaves the request in the
    // response queue and closes the socket under it, so the read terminal is
    // reached with work outstanding.
    let killed = client
        .simple_query("SELECT pg_terminate_backend(pg_backend_pid())")
        .await;
    assert!(killed.is_err(), "the backend survived its own termination");

    let outcome = task.await.expect("the connection task panicked");
    assert!(
        outcome.is_err(),
        "losing the backend under a live client was reported as a clean close"
    );

    // The client remains alive until after the driver reports the failure.
    drop(client);
}

/// An implicit rollback is drop-time housekeeping, just like statement close.
/// Its delivery is not promised once the last client releases the socket.
#[compio::test]
async fn dropping_an_unfinished_transaction_and_the_client_closes_without_an_error() {
    let url = require_pg().await;

    let (mut client, connection) = compio_postgres::connect(&url, common::suite_tls())
        .await
        .unwrap();
    let task = compio::runtime::spawn(async move { connection.run().await });

    let transaction = client.transaction().await.unwrap();
    drop(transaction);
    drop(client);

    let outcome = task.await.expect("the connection task panicked");
    if let Err(error) = outcome {
        panic!(
            "dropping an unfinished transaction and its client failed run(): {}",
            common::error_chain(&error)
        );
    }
}

/// Forgetting the guard leaves the server transaction open, so closing the
/// client itself must make PostgreSQL roll its uncommitted row back.
#[compio::test]
async fn dropping_the_client_with_a_server_transaction_open_rolls_back() {
    let url = require_pg().await;
    let table = common::test_object_name("cpg_tx_client_drop");

    let observer = connect(&url).await.unwrap();
    observer
        .batch_execute(&format!("CREATE TABLE {table} (n int)"))
        .await
        .unwrap();

    let (mut client, connection) = compio_postgres::connect(&url, common::suite_tls())
        .await
        .unwrap();
    let task = compio::runtime::spawn(async move { connection.run().await });
    let backend_pid: i32 = client
        .query_one_scalar("SELECT pg_backend_pid()", &[])
        .await
        .unwrap();

    let transaction = client.transaction().await.unwrap();
    transaction
        .execute(&format!("INSERT INTO {table} VALUES (1)"), &[])
        .await
        .unwrap();

    // Bypass Transaction::drop on purpose: the server still owns the open
    // transaction when dropping the last client closes the connection.
    std::mem::forget(transaction);
    drop(client);
    let task_outcome = compio::time::timeout(std::time::Duration::from_secs(5), task).await;

    let row_count: i64 = observer
        .query_one_scalar(&format!("SELECT count(*) FROM {table}"), &[])
        .await
        .unwrap();

    observer
        .batch_execute("SET lock_timeout = '5s'")
        .await
        .unwrap();
    observer
        .batch_execute(&format!("DROP TABLE {table}"))
        .await
        .unwrap();
    let session_count: i64 = observer
        .query_one_scalar(
            "SELECT count(*) FROM pg_stat_activity WHERE pid = $1",
            &[&backend_pid],
        )
        .await
        .unwrap();

    assert_eq!(
        row_count, 0,
        "the disconnected transaction committed its row"
    );
    assert_eq!(
        session_count, 0,
        "the client was dropped but its PostgreSQL session stayed open"
    );

    let join_result = task_outcome.expect("the connection task did not close within 5 seconds");
    let outcome = join_result.expect("the connection task panicked");
    if let Err(error) = outcome {
        panic!(
            "closing a client with an open transaction failed run(): {}",
            common::error_chain(&error)
        );
    }
}

/// Portal close is the same discarded-response housekeeping class as
/// statement close. Dropping it must not turn client shutdown into an error.
#[compio::test]
async fn dropping_a_bound_portal_and_its_client_closes_without_an_error() {
    let url = require_pg().await;

    let (mut client, connection) = compio_postgres::connect(&url, common::suite_tls())
        .await
        .unwrap();
    let task = compio::runtime::spawn(async move { connection.run().await });

    let statement = client.prepare("SELECT $1::INT4").await.unwrap();
    let transaction = client.transaction().await.unwrap();
    let portal = transaction.bind(&statement, &[&1_i32]).await.unwrap();
    drop(portal);
    drop(transaction);
    drop(statement);
    drop(client);

    let outcome = task.await.expect("the connection task panicked");
    if let Err(error) = outcome {
        panic!(
            "dropping a bound portal and its client failed run(): {}",
            common::error_chain(&error)
        );
    }
}

/// A real request remains fallible even if the last client drops after queuing
/// it. The response stream outlives the client and still represents an awaited
/// operation; only explicitly marked drop-time housekeeping may be absorbed.
#[compio::test]
async fn an_awaited_query_queued_before_client_drop_still_reports_its_write_error() {
    let url = require_pg().await;

    let (client, connection) = compio_postgres::connect(&url, common::suite_tls())
        .await
        .unwrap();
    let observer = client.simple_query_raw("SELECT 1").await.unwrap();
    drop(client);

    let outcome = connection.run().await;
    assert!(
        outcome.is_err(),
        "an awaited query write was treated as fire-and-forget housekeeping"
    );
    drop(observer);
}

// ---------------------------------------------------------------------------
// Prepared-statement behavior and regressions.
// ---------------------------------------------------------------------------

/// Find the server-side names for exact SQL without preparing another
/// statement as part of the lookup. The simple protocol leaves prepare.rs's
/// global name counter untouched.
async fn prepared_statement_names(client: &Client, sql: &str) -> Vec<String> {
    let escaped = sql.replace('\'', "''");
    let messages = client
        .simple_query(&format!(
            "SELECT name FROM pg_prepared_statements WHERE statement = '{escaped}'"
        ))
        .await
        .unwrap();

    messages
        .iter()
        .filter_map(|message| match message {
            SimpleQueryMessage::Row(row) => row.get(0),
            _ => None,
        })
        .map(str::to_string)
        .collect()
}

async fn prepared_statement_name(client: &Client, sql: &str) -> String {
    let names = prepared_statement_names(client, sql).await;
    assert_eq!(
        names.len(),
        1,
        "expected exactly one prepared statement for {sql:?}, got {names:?}"
    );
    names[0].clone()
}

fn driver_statement_id(name: &str) -> usize {
    name.strip_prefix('s')
        .and_then(|id| id.parse().ok())
        .expect("driver statement names are s followed by a usize")
}

/// Raw text for a PostgreSQL domain parameter. `postgres-types` deliberately
/// does not guess that an arbitrary Rust string satisfies a user-defined
/// domain, while these cache tests need the server to decode that exact OID.
#[derive(Debug)]
struct DomainText(&'static str);

impl ToSql for DomainText {
    fn to_sql(
        &self,
        _: &Type,
        out: &mut bytes::BytesMut,
    ) -> Result<IsNull, Box<dyn std::error::Error + Sync + Send>> {
        out.extend_from_slice(self.0.as_bytes());
        Ok(IsNull::No)
    }

    fn accepts(_: &Type) -> bool {
        true
    }

    to_sql_checked!();
}

/// Reserve enough consecutive names on this session that unrelated parallel
/// tests cannot advance prepare.rs's process-global counter past all of them
/// between the probe and the colliding Parse.
async fn reserve_driver_statement_names(client: &Client, after: usize, value: i32) -> usize {
    use std::fmt::Write;

    const RESERVATIONS: usize = 4096;
    let mut sql = String::with_capacity(RESERVATIONS * 40);
    for offset in 1..=RESERVATIONS {
        let id = after.wrapping_add(offset);
        writeln!(&mut sql, "PREPARE s{id} AS SELECT {value}::int4;").unwrap();
    }
    client.batch_execute(&sql).await.unwrap();
    RESERVATIONS
}

async fn prepared_statement_count(client: &Client) -> usize {
    client
        .simple_query("SELECT count(*) FROM pg_prepared_statements")
        .await
        .unwrap()
        .iter()
        .find_map(|message| match message {
            SimpleQueryMessage::Row(row) => row.get(0),
            _ => None,
        })
        .expect("count query returns one row")
        .parse()
        .unwrap()
}

/// Two callers can miss the internal type-info statement cache together.
/// The winner remains cached; the losing server statement must be closed once
/// the cache lock elects the winner.
#[compio::test]
async fn concurrent_typeinfo_cache_loser_is_closed() {
    use std::time::Duration;

    compio::time::timeout(Duration::from_secs(10), async {
        let url = require_pg().await;
        let client = connect(&url).await.unwrap();
        let type_name = common::test_object_name("cpg_typeinfo_race");
        client
            .batch_execute(&format!(
                "DROP TYPE IF EXISTS pg_temp.{type_name} CASCADE; \
                 CREATE TYPE pg_temp.{type_name} AS ENUM ('value')"
            ))
            .await
            .unwrap();

        let query = format!("SELECT 'value'::pg_temp.{type_name}");
        let (first, second) = futures_util::future::join(
            client.prepare(query.as_str()),
            client.prepare(query.as_str()),
        )
        .await;
        let first = first.unwrap();
        let second = second.unwrap();
        drop((first, second));
        client.simple_query("").await.unwrap();

        const TYPEINFO_QUERY: &str = "\
SELECT t.typname, t.typtype, t.typelem, r.rngsubtype, t.typbasetype, n.nspname, t.typrelid
FROM pg_catalog.pg_type t
LEFT OUTER JOIN pg_catalog.pg_range r ON r.rngtypid = t.oid
INNER JOIN pg_catalog.pg_namespace n ON t.typnamespace = n.oid
WHERE t.oid = $1
";
        assert_eq!(
            prepared_statement_names(&client, TYPEINFO_QUERY)
                .await
                .len(),
            1,
            "concurrent type-info cache loser leaked its server statement"
        );
        client
            .batch_execute(&format!("DROP TYPE pg_temp.{type_name} CASCADE"))
            .await
            .unwrap();
    })
    .await
    .expect("type-info cache-loser claim test exceeded its 10 second deadline");
}

/// Capacity zero is the compatibility mode: every raw SQL call prepares its
/// own statement, just as it did before the opt-in cache existed. Keep the
/// returned rows alive so their Statement clones keep all four server names
/// observable at once.
#[compio::test]
async fn statement_cache_capacity_zero_prepares_every_call() {
    let url = test_url();
    let client = connect_with_statement_cache(&url, 0).await.unwrap();

    const SQL: &str = "SELECT 51::int4 AS cpg_cache_disabled";
    let mut results = Vec::new();
    for _ in 0..4 {
        results.push(client.query(SQL, &[]).await.unwrap());
    }

    assert_eq!(prepared_statement_names(&client, SQL).await.len(), 4);

    drop(results);
    client.simple_query("").await.unwrap();
    assert!(prepared_statement_names(&client, SQL).await.is_empty());
}

/// Capacity zero disables the entire admission mechanism, not merely prepared
/// entry retention. A configured threshold therefore cannot send early calls
/// through the unnamed statement slot.
#[compio::test]
async fn statement_cache_capacity_zero_ignores_the_execution_threshold() {
    let url = test_url();
    let client = connect_with_statement_cache_threshold(&url, 0, 3)
        .await
        .unwrap();

    const SQL: &str = "SELECT 87::int4 AS cpg_cache_zero_threshold";
    let first = client.query(SQL, &[]).await.unwrap();
    let second = client.query(SQL, &[]).await.unwrap();
    assert_eq!(first[0].get::<_, i32>(0), 87);
    assert_eq!(second[0].get::<_, i32>(0), 87);
    assert_eq!(
        prepared_statement_names(&client, SQL).await.len(),
        2,
        "a zero-capacity cache still applied its execution threshold"
    );
}

/// An enabled cache prepares one server statement and reuses it for every
/// identical raw SQL string on this connection.
#[compio::test]
async fn statement_cache_reuses_identical_sql() {
    let url = test_url();
    let client = connect_with_statement_cache(&url, 2).await.unwrap();

    const SQL: &str = "SELECT 52::int4 AS cpg_cache_hit";
    let mut results = vec![client.query(SQL, &[]).await.unwrap()];
    assert_eq!(prepared_statement_names(&client, SQL).await.len(), 1);
    for _ in 1..8 {
        let rows = client.query(SQL, &[]).await.unwrap();
        assert_eq!(rows[0].get::<_, i32>(0), 52);
        results.push(rows);
    }

    assert_eq!(prepared_statement_names(&client, SQL).await.len(), 1);

    drop(results);
    client.simple_query("").await.unwrap();
    assert_eq!(prepared_statement_names(&client, SQL).await.len(), 1);
}

/// The suite mode reaches ordinary connections rather than only the helpers
/// that opt into a cache explicitly. Keeping both results alive makes a
/// no-op mode observable: capacity zero leaves two server statements here.
#[cfg(feature = "suite-with-statement-cache")]
#[compio::test]
async fn suite_statement_cache_mode_reuses_identical_sql() {
    let url = test_url();
    let client = connect(&url).await.unwrap();

    const SQL: &str = "SELECT 53::int4 AS cpg_suite_cache_mode";
    let first = client.query(SQL, &[]).await.unwrap();
    let cached_name = prepared_statement_name(&client, SQL).await;
    let second = client.query(SQL, &[]).await.unwrap();

    assert_eq!(first[0].get::<_, i32>(0), 53);
    assert_eq!(second[0].get::<_, i32>(0), 53);
    assert_eq!(
        prepared_statement_names(&client, SQL).await,
        vec![cached_name.clone()],
        "the second execution did not reuse the first server statement"
    );

    drop((first, second));
    client.simple_query("").await.unwrap();
    assert_eq!(
        prepared_statement_names(&client, SQL).await,
        vec![cached_name],
        "the ordinary connection did not retain the statement in its cache"
    );
}

/// The target SQL observes itself while it is executing. A named Parse is
/// already visible in `pg_prepared_statements` at that point; an unnamed Parse
/// is not, so this cannot pass merely because a transient name was closed
/// before a later probe.
#[compio::test]
async fn statement_cache_promotes_on_the_execution_threshold() {
    let url = test_url();
    let client = connect_with_statement_cache_threshold(&url, 2, 3)
        .await
        .unwrap();

    const SQL: &str = "SELECT count(*)::int8 FROM pg_prepared_statements \
        WHERE statement = $1::text AND NOT from_sql \
        /* cpg_cache_execution_threshold_three */";

    for execution in 1..3 {
        let live: i64 = client.query_one_scalar(SQL, &[&SQL]).await.unwrap();
        assert_eq!(live, 0, "execution {execution} was sent with a name");
        assert!(prepared_statement_names(&client, SQL).await.is_empty());
    }

    let live: i64 = client.query_one_scalar(SQL, &[&SQL]).await.unwrap();
    assert_eq!(live, 1, "the threshold execution was not named");
    assert_eq!(prepared_statement_names(&client, SQL).await.len(), 1);
}

/// Refreshing a candidate must move its one LRU entry rather than append a
/// duplicate. Otherwise a later capacity eviction can discard the refreshed
/// SQL's execution count and postpone its promotion indefinitely.
#[compio::test]
async fn statement_cache_candidate_lru_refreshes_repeated_sql() {
    compio::time::timeout(Duration::from_secs(60), async {
        let url = test_url();
        let client = connect_with_statement_cache_threshold(&url, 1, 4)
            .await
            .unwrap();

        const HOT_SQL: &str = "SELECT count(*)::int8 FROM pg_prepared_statements \
            WHERE statement = $1::text AND NOT from_sql \
            /* cpg_cache_candidate_lru_hot */";

        let first_live: i64 = client.query_one_scalar(HOT_SQL, &[&HOT_SQL]).await.unwrap();
        assert_eq!(first_live, 0, "the first execution was unexpectedly named");

        // HOT plus these 99 one-shot statements fills the bounded 100-entry
        // admission cache without promoting any cold SQL.
        for index in 0..99i32 {
            let sql = format!("SELECT {index}::int4 /* cpg_cache_candidate_lru_cold_{index} */");
            let value: i32 = client.query_one_scalar(sql.as_str(), &[]).await.unwrap();
            assert_eq!(value, index, "cold fixture {index} returned the wrong row");
        }

        let second_live: i64 = client.query_one_scalar(HOT_SQL, &[&HOT_SQL]).await.unwrap();
        assert_eq!(
            second_live, 0,
            "the second execution crossed a four-use threshold"
        );

        let newcomer: i32 = client
            .query_one_scalar(
                "SELECT 100::int4 /* cpg_cache_candidate_lru_newcomer */",
                &[],
            )
            .await
            .unwrap();
        assert_eq!(newcomer, 100, "the eviction-triggering query did not run");

        let third_live: i64 = client.query_one_scalar(HOT_SQL, &[&HOT_SQL]).await.unwrap();
        assert_eq!(third_live, 0, "the third execution was unexpectedly named");
        let fourth_live: i64 = client.query_one_scalar(HOT_SQL, &[&HOT_SQL]).await.unwrap();
        assert_eq!(
            fourth_live, 1,
            "eviction forgot the refreshed candidate's prior executions"
        );
        assert_eq!(
            prepared_statement_names(&client, HOT_SQL).await.len(),
            1,
            "the threshold execution did not remain in the statement cache"
        );
    })
    .await
    .expect("candidate-LRU refresh claim exceeded its 60 second deadline");
}

#[compio::test]
async fn statement_cache_execution_threshold_one_promotes_immediately() {
    let url = test_url();
    let client = connect_with_statement_cache_threshold(&url, 2, 1)
        .await
        .unwrap();

    const SQL: &str = "SELECT count(*)::int8 FROM pg_prepared_statements \
        WHERE statement = $1::text AND NOT from_sql \
        /* cpg_cache_execution_threshold_one */";

    let first_live: i64 = client.query_one_scalar(SQL, &[&SQL]).await.unwrap();
    assert_eq!(first_live, 1);
    let first_name = prepared_statement_name(&client, SQL).await;

    let second_live: i64 = client.query_one_scalar(SQL, &[&SQL]).await.unwrap();
    assert_eq!(second_live, 1);
    assert_eq!(prepared_statement_name(&client, SQL).await, first_name);
}

/// Once SQL has crossed the admission threshold, losing its server-side
/// statement must not charge it the threshold again. The retry itself observes
/// whether it ran under a name, so a transient unnamed Parse cannot satisfy
/// the assertion after being closed.
#[compio::test]
async fn statement_cache_stale_reprepare_keeps_admission() {
    use std::time::Duration;

    compio::time::timeout(Duration::from_secs(10), async {
        let url = test_url();
        let client = connect_with_statement_cache_threshold(&url, 2, 3)
            .await
            .unwrap();

        const SQL: &str = "SELECT count(*)::int8 FROM pg_prepared_statements \
            WHERE statement = $1::text AND NOT from_sql \
            /* cpg_cache_stale_keeps_admission */";

        for expected_live in [0_i64, 0, 1] {
            let live: i64 = client.query_one_scalar(SQL, &[&SQL]).await.unwrap();
            assert_eq!(live, expected_live);
        }
        client.batch_execute("DEALLOCATE ALL").await.unwrap();

        let live: i64 = client.query_one_scalar(SQL, &[&SQL]).await.unwrap();
        assert_eq!(live, 1, "stale admitted SQL returned to probation");
        assert_eq!(prepared_statement_names(&client, SQL).await.len(), 1);
    })
    .await
    .expect("stale-admission claim test exceeded its 10 second deadline");
}

#[compio::test]
async fn statement_cache_does_not_count_wrong_parameter_arity() {
    let url = test_url();
    let client = connect_with_statement_cache_threshold(&url, 2, 2)
        .await
        .unwrap();

    const SQL: &str = "SELECT count(*)::int8 FROM pg_prepared_statements \
        WHERE statement = $1::text AND NOT from_sql \
        /* cpg_cache_threshold_rejected_execution */";

    client
        .query(SQL, &[])
        .await
        .expect_err("the SQL requires one parameter");

    let first_live: i64 = client.query_one_scalar(SQL, &[&SQL]).await.unwrap();
    assert_eq!(
        first_live, 0,
        "wrong parameter arity earned execution credit"
    );
    let second_live: i64 = client.query_one_scalar(SQL, &[&SQL]).await.unwrap();
    assert_eq!(second_live, 1);
}

#[compio::test]
async fn statement_cache_execution_threshold_applies_to_execute() {
    let url = test_url();
    let client = connect_with_statement_cache_threshold(&url, 2, 2)
        .await
        .unwrap();
    let table = common::test_object_name("cpg_cache_execute_seen");
    client
        .batch_execute(&format!("CREATE TEMP TABLE {table} (value int8)"))
        .await
        .unwrap();

    let sql = format!(
        "INSERT INTO {table}(value) \
        SELECT count(*)::int8 FROM pg_prepared_statements \
        WHERE statement = $1::text AND NOT from_sql \
        /* cpg_cache_threshold_execute */"
    );

    assert_eq!(
        client
            .execute(sql.as_str(), &[&sql.as_str()])
            .await
            .unwrap(),
        1
    );
    assert!(prepared_statement_names(&client, &sql).await.is_empty());
    assert_eq!(
        client
            .execute(sql.as_str(), &[&sql.as_str()])
            .await
            .unwrap(),
        1
    );
    assert_eq!(prepared_statement_names(&client, &sql).await.len(), 1);

    let rows = client
        .query(&format!("SELECT value FROM {table} ORDER BY ctid"), &[])
        .await
        .unwrap();
    let observed = rows
        .iter()
        .map(|row| row.get::<_, i64>(0))
        .collect::<Vec<_>>();
    assert_eq!(observed, [0, 1]);
}

#[compio::test]
async fn statement_cache_execution_threshold_applies_to_transaction_bind() {
    let url = test_url();
    let mut client = connect_with_statement_cache_threshold(&url, 2, 2)
        .await
        .unwrap();
    let transaction = client.transaction().await.unwrap();

    const SQL: &str = "SELECT $1::int4 /* cpg_cache_threshold_bind */";
    let first = transaction.bind(SQL, &[&7_i32]).await.unwrap();
    assert!(
        prepared_statement_names(transaction.client(), SQL)
            .await
            .is_empty()
    );
    let rows = transaction.query_portal(&first, 0).await.unwrap();
    assert_eq!(rows[0].get::<_, i32>(0), 7);
    assert!(
        prepared_statement_names(transaction.client(), SQL)
            .await
            .is_empty()
    );
    drop(first);

    let second = transaction.bind(SQL, &[&8_i32]).await.unwrap();
    assert_eq!(
        prepared_statement_names(transaction.client(), SQL)
            .await
            .len(),
        1
    );
    let rows = transaction.query_portal(&second, 0).await.unwrap();
    assert_eq!(rows[0].get::<_, i32>(0), 8);
    drop(second);
    transaction.rollback().await.unwrap();
}

#[compio::test]
async fn statement_cache_execution_threshold_applies_to_copy_out() {
    use futures_util::StreamExt;

    let url = test_url();
    let client = connect_with_statement_cache_threshold(&url, 2, 2)
        .await
        .unwrap();

    const SQL: &str = "COPY (SELECT 1::int4) TO STDOUT \
        /* cpg_cache_threshold_copy_out */";
    for execution in 1..=2 {
        let stream = client.copy_out(SQL).await.unwrap();
        let mut stream = Box::pin(stream);
        let mut bytes = 0;
        while let Some(chunk) = stream.as_mut().next().await {
            bytes += chunk.unwrap().len();
        }
        assert!(bytes > 0);
        assert_eq!(
            prepared_statement_names(&client, SQL).await.len(),
            usize::from(execution == 2)
        );
    }
}

/// SQL-level `EXECUTE` can report `26000/FetchPreparedStatement` after the
/// extended-protocol Bind succeeded. That diagnosis belongs to the inner SQL
/// statement, so it must not evict the still-valid cached COPY API wrapper.
#[compio::test]
async fn post_bind_error_keeps_copy_out_statement_cached() {
    let url = test_url();
    let client = connect_with_statement_cache(&url, 2).await.unwrap();
    let target = common::test_object_name("cpg_copy_out_inner_execute");
    let sql = format!("EXECUTE {target} /* cpg_copy_out_post_bind_cache */");

    client
        .batch_execute(&format!("PREPARE {target} AS SELECT 1::int4"))
        .await
        .expect("prepare the SQL-level target");

    let initial = match client.copy_out(&sql).await {
        Ok(_) => panic!("copy_out accepted a row-producing EXECUTE"),
        Err(error) => error,
    };
    assert!(
        initial.code().is_none(),
        "the initial COPY refusal unexpectedly came from PostgreSQL: {}",
        common::error_chain(&initial)
    );
    assert_eq!(
        prepared_statement_names(&client, &sql).await.len(),
        1,
        "the COPY wrapper was not cached before the post-Bind error"
    );

    client
        .batch_execute(&format!("DEALLOCATE {target}"))
        .await
        .expect("remove only the SQL-level target");
    let error = match client.copy_out(&sql).await {
        Ok(_) => panic!("copy_out accepted EXECUTE of a missing statement"),
        Err(error) => error,
    };
    assert_eq!(
        error.code(),
        Some(&SqlState::INVALID_SQL_STATEMENT_NAME),
        "the missing inner statement reported the wrong error: {}",
        common::error_chain(&error)
    );
    assert_eq!(
        error
            .as_db_error()
            .and_then(compio_postgres::error::DbError::routine),
        Some("FetchPreparedStatement"),
        "the fixture did not reach the stale-statement provenance"
    );
    client
        .simple_query("")
        .await
        .expect("drain any statement-cache cleanup");
    assert_eq!(
        prepared_statement_names(&client, &sql).await.len(),
        1,
        "a post-Bind inner EXECUTE error evicted the valid COPY wrapper"
    );
}

#[compio::test]
async fn statement_cache_execution_count_resets_after_prepared_eviction() {
    let url = test_url();
    let client = connect_with_statement_cache_threshold(&url, 1, 2)
        .await
        .unwrap();

    const SQL_A: &str = "SELECT count(*)::int8 FROM pg_prepared_statements \
        WHERE statement = $1::text AND NOT from_sql \
        /* cpg_cache_threshold_eviction_a */";
    const SQL_B: &str = "SELECT count(*)::int8 FROM pg_prepared_statements \
        WHERE statement = $1::text AND NOT from_sql \
        /* cpg_cache_threshold_eviction_b */";

    let a_first: i64 = client.query_one_scalar(SQL_A, &[&SQL_A]).await.unwrap();
    let a_second: i64 = client.query_one_scalar(SQL_A, &[&SQL_A]).await.unwrap();
    assert_eq!((a_first, a_second), (0, 1));

    let b_first: i64 = client.query_one_scalar(SQL_B, &[&SQL_B]).await.unwrap();
    let b_second: i64 = client.query_one_scalar(SQL_B, &[&SQL_B]).await.unwrap();
    assert_eq!((b_first, b_second), (0, 1));
    assert!(prepared_statement_names(&client, SQL_A).await.is_empty());

    let a_after_eviction: i64 = client.query_one_scalar(SQL_A, &[&SQL_A]).await.unwrap();
    assert_eq!(a_after_eviction, 0, "eviction retained admission history");
    let a_readmitted: i64 = client.query_one_scalar(SQL_A, &[&SQL_A]).await.unwrap();
    assert_eq!(a_readmitted, 1);
}

#[compio::test]
async fn clear_type_cache_refreshes_implicitly_cached_statement_metadata() {
    use compio_postgres::types::Kind;

    let url = require_pg().await;
    let client = connect_with_statement_cache(&url, 2).await.unwrap();
    let type_name = common::test_object_name("cpg_cache_enum");

    client
        .batch_execute(&format!("CREATE TYPE {type_name} AS ENUM ('before')"))
        .await
        .unwrap();

    let sql = format!("SELECT 'before'::{type_name} AS value");
    let before_rows = client.query(sql.as_str(), &[]).await.unwrap();
    let before = before_rows[0].columns()[0].type_().clone();

    client
        .batch_execute(&format!("ALTER TYPE {type_name} ADD VALUE 'after'"))
        .await
        .unwrap();
    let catalog_variants: String = client
        .query_one_scalar(&format!("SELECT enum_range(NULL::{type_name})::text"), &[])
        .await
        .unwrap();

    let stale_rows = client.query(sql.as_str(), &[]).await.unwrap();
    let stale = stale_rows[0].columns()[0].type_().clone();

    const BUILTIN_SQL: &str = "SELECT 71::int4 AS cache_survivor";
    let builtin_rows = client.query(BUILTIN_SQL, &[]).await.unwrap();
    assert_eq!(builtin_rows[0].get::<_, i32>(0), 71);

    drop((before_rows, stale_rows));
    client.clear_type_cache();
    client.simple_query("").await.unwrap();
    assert!(prepared_statement_names(&client, &sql).await.is_empty());
    assert_eq!(
        prepared_statement_names(&client, BUILTIN_SQL).await.len(),
        1
    );

    let cleared_rows = client.query(sql.as_str(), &[]).await.unwrap();
    let cleared = cleared_rows[0].columns()[0].type_().clone();

    // The one-shot path must agree with the cache repopulated after the clear,
    // or the two raw-SQL preparation paths have diverged.
    let fresh_rows = client
        .query(&Uncached::new(sql.as_str()), &[])
        .await
        .unwrap();
    let fresh = fresh_rows[0].columns()[0].type_().clone();

    eprintln!(
        "catalog={catalog_variants}; before oid={} kind={:?}; after ALTER oid={} kind={:?}; after clear oid={} kind={:?}; uncached oid={} kind={:?}",
        before.oid(),
        before.kind(),
        stale.oid(),
        stale.kind(),
        cleared.oid(),
        cleared.kind(),
        fresh.oid(),
        fresh.kind(),
    );

    assert_eq!(catalog_variants, "{before,after}");
    assert_eq!(stale.oid(), before.oid());
    assert_eq!(cleared.oid(), before.oid());
    assert_eq!(fresh.oid(), before.oid());
    assert_eq!(stale.kind(), &Kind::Enum(vec!["before".to_string()]));
    let expected = Kind::Enum(vec!["before".to_string(), "after".to_string()]);
    assert_eq!(cleared.kind(), &expected);
    assert_eq!(fresh.kind(), &expected);

    drop((builtin_rows, cleared_rows, fresh_rows));
    client
        .batch_execute(&format!("DROP TYPE {type_name}"))
        .await
        .unwrap();
}

#[compio::test]
async fn statement_cache_is_per_connection() {
    let url = test_url();
    let first = connect_with_statement_cache(&url, 1).await.unwrap();
    let second = connect_with_statement_cache(&url, 1).await.unwrap();

    const SQL: &str = "SELECT 60::int4 AS cpg_cache_per_connection";
    drop(first.query(SQL, &[]).await.unwrap());
    drop(second.query(SQL, &[]).await.unwrap());

    assert_eq!(prepared_statement_names(&first, SQL).await.len(), 1);
    assert_eq!(prepared_statement_names(&second, SQL).await.len(), 1);
}

#[compio::test]
async fn statement_cache_evicts_the_least_recently_used_sql() {
    let url = test_url();
    let client = connect_with_statement_cache(&url, 2).await.unwrap();

    const SQL_A: &str = "SELECT 61::int4 AS cpg_cache_lru_a";
    const SQL_B: &str = "SELECT 62::int4 AS cpg_cache_lru_b";
    const SQL_C: &str = "SELECT 63::int4 AS cpg_cache_lru_c";

    drop(client.query(SQL_A, &[]).await.unwrap());
    drop(client.query(SQL_B, &[]).await.unwrap());
    drop(client.query(SQL_A, &[]).await.unwrap());
    drop(client.query(SQL_C, &[]).await.unwrap());
    client.simple_query("").await.unwrap();

    assert_eq!(prepared_statement_names(&client, SQL_A).await.len(), 1);
    assert!(prepared_statement_names(&client, SQL_B).await.is_empty());
    assert_eq!(prepared_statement_names(&client, SQL_C).await.len(), 1);
}

/// Eviction releases the cache's clone immediately, while rows from an
/// outstanding use keep that statement alive until their last clone drops.
#[compio::test]
async fn statement_cache_eviction_closes_after_outstanding_clones() {
    let url = test_url();
    let client = connect_with_statement_cache(&url, 1).await.unwrap();

    const SQL_A: &str = "SELECT 53::int4 AS cpg_cache_evicted";
    const SQL_B: &str = "SELECT 54::int4 AS cpg_cache_retained";

    let rows_a = client.query(SQL_A, &[]).await.unwrap();
    assert_eq!(prepared_statement_names(&client, SQL_A).await.len(), 1);

    let rows_b = client.query(SQL_B, &[]).await.unwrap();
    drop(rows_b);
    client.simple_query("").await.unwrap();

    assert_eq!(prepared_statement_names(&client, SQL_A).await.len(), 1);
    assert_eq!(prepared_statement_names(&client, SQL_B).await.len(), 1);

    drop(rows_a);
    client.simple_query("").await.unwrap();
    assert!(prepared_statement_names(&client, SQL_A).await.is_empty());
    assert_eq!(prepared_statement_names(&client, SQL_B).await.len(), 1);
}

/// Protocol Close of a statement also closes every portal made from it. The
/// cache may evict its clone, but a caller-owned Portal must keep the server
/// statement alive until that portal is dropped.
#[compio::test]
async fn statement_cache_eviction_waits_for_a_bound_portal() {
    let url = test_url();
    let mut client = connect_with_statement_cache(&url, 1).await.unwrap();
    let transaction = client.transaction().await.unwrap();

    const SQL_A: &str = "SELECT $1::int4 AS cpg_cache_portal_owner";
    const SQL_B: &str = "SELECT $1::int4 AS cpg_cache_portal_evictor";
    let portal_a = transaction.bind(SQL_A, &[&81_i32]).await.unwrap();
    let name_a = prepared_statement_name(transaction.client(), SQL_A).await;

    let portal_b = transaction.bind(SQL_B, &[&82_i32]).await.unwrap();
    assert_eq!(
        prepared_statement_names(transaction.client(), SQL_A).await,
        vec![name_a.clone()],
        "eviction closed a statement while its portal still named it"
    );

    let rows = transaction.query_portal(&portal_a, 0).await.unwrap();
    assert_eq!(rows[0].get::<_, i32>(0), 81);
    drop((rows, portal_b));
    assert_eq!(
        prepared_statement_names(transaction.client(), SQL_A).await,
        vec![name_a],
        "executing the portal released its statement ownership early"
    );

    drop(portal_a);
    transaction.client().simple_query("").await.unwrap();
    assert!(
        prepared_statement_names(transaction.client(), SQL_A)
            .await
            .is_empty()
    );
    transaction.rollback().await.unwrap();
}

/// Concurrent cold misses may race through Parse/Describe, but cache
/// insertion elects one winner and drops every losing Statement. Both callers
/// execute the winner, so no losing server name remains live.
#[compio::test]
async fn statement_cache_concurrent_misses_keep_one_statement() {
    let url = test_url();
    let client = connect_with_statement_cache(&url, 2).await.unwrap();

    const SQL: &str = "SELECT 55::int4 AS cpg_cache_concurrent";
    let (first, second) =
        futures_util::future::join(client.query(SQL, &[]), client.query(SQL, &[])).await;
    let first = first.unwrap();
    let second = second.unwrap();

    assert_eq!(first[0].get::<_, i32>(0), 55);
    assert_eq!(second[0].get::<_, i32>(0), 55);
    assert_eq!(prepared_statement_names(&client, SQL).await.len(), 1);

    drop((first, second));
    client.simple_query("").await.unwrap();
    assert_eq!(prepared_statement_names(&client, SQL).await.len(), 1);
}

/// Both callers can clone the same cached Statement before either missing-name
/// response is delivered. They must each recover without evicting the other's
/// replacement, and the race must converge on one live server name.
#[compio::test]
async fn statement_cache_concurrent_stale_callers_share_one_replacement() {
    let url = test_url();
    let client = connect_with_statement_cache(&url, 2).await.unwrap();

    const SQL: &str = "SELECT 83::int4 AS cpg_cache_concurrent_stale";
    drop(client.query(SQL, &[]).await.unwrap());
    client.batch_execute("DEALLOCATE ALL").await.unwrap();

    let (first, second) =
        futures_util::future::join(client.query(SQL, &[]), client.query(SQL, &[])).await;
    let first = first.unwrap();
    let second = second.unwrap();
    assert_eq!(first[0].get::<_, i32>(0), 83);
    assert_eq!(second[0].get::<_, i32>(0), 83);
    assert_eq!(
        prepared_statement_names(&client, SQL).await.len(),
        1,
        "concurrent recovery left more than one cached server statement"
    );
}

/// `Uncached` skips both cache lookup and insertion for exactly this use. The
/// same-SQL arm proves it does not silently reuse the cached Statement; the
/// unique-SQL arm proves it does not populate an empty slot.
#[compio::test]
async fn statement_cache_bypass_is_one_shot() {
    let url = test_url();
    let client = connect_with_statement_cache(&url, 3).await.unwrap();

    const CACHED_SQL: &str = "SELECT 56::int4 AS cpg_cache_bypass_existing";
    const ONE_SHOT_SQL: &str = "SELECT 57::int4 AS cpg_cache_bypass_one_shot";

    let cached_rows = client.query(CACHED_SQL, &[]).await.unwrap();
    drop(cached_rows);
    assert_eq!(prepared_statement_names(&client, CACHED_SQL).await.len(), 1);

    let bypass_rows = client.query(&Uncached::new(CACHED_SQL), &[]).await.unwrap();
    assert_eq!(prepared_statement_names(&client, CACHED_SQL).await.len(), 2);

    drop(bypass_rows);
    client.simple_query("").await.unwrap();
    assert_eq!(prepared_statement_names(&client, CACHED_SQL).await.len(), 1);

    let one_shot_rows = client
        .query(&Uncached::new(ONE_SHOT_SQL), &[])
        .await
        .unwrap();
    assert_eq!(one_shot_rows[0].get::<_, i32>(0), 57);
    drop(one_shot_rows);
    client.simple_query("").await.unwrap();
    assert!(
        prepared_statement_names(&client, ONE_SHOT_SQL)
            .await
            .is_empty()
    );
}

/// A cached plan whose result shape changed is replaced before the stale-plan
/// error escapes to the raw-SQL caller.
#[compio::test]
async fn statement_cache_retries_stale_result_shape_once_after_0a000() {
    let url = test_url();
    let client = connect_with_statement_cache(&url, 2).await.unwrap();
    let table = common::test_object_name("cpg_cache_plan_shape");
    let sql = format!("SELECT * FROM {table}");

    client
        .batch_execute(&format!(
            "DROP TABLE IF EXISTS {table}; \
             CREATE TEMP TABLE {table} (id int4); \
             INSERT INTO {table} VALUES (58)"
        ))
        .await
        .unwrap();

    let first_rows = client.query(sql.as_str(), &[]).await.unwrap();
    assert_eq!(first_rows[0].get::<_, i32>("id"), 58);
    drop(first_rows);

    client
        .batch_execute(&format!(
            "ALTER TABLE {table} ADD COLUMN label text NOT NULL DEFAULT 'fresh'"
        ))
        .await
        .unwrap();

    let refreshed_rows = client.query(sql.as_str(), &[]).await.unwrap();
    assert_eq!(refreshed_rows[0].len(), 2);
    assert_eq!(refreshed_rows[0].get::<_, i32>("id"), 58);
    assert_eq!(refreshed_rows[0].get::<_, &str>("label"), "fresh");
    assert_eq!(prepared_statement_names(&client, &sql).await.len(), 1);

    assert_eq!(
        simple_query_scalar_i32(&client, "SELECT 59::int4")
            .await
            .unwrap(),
        59
    );
    client
        .batch_execute(&format!("DROP TABLE {table}"))
        .await
        .unwrap();
}

/// PostgreSQL decodes domain parameters before it asks the plan cache to
/// revalidate a fixed result descriptor. The first domain check therefore ran
/// even though this Bind later reports 0A000. Propagate that first error rather
/// than replaying input code whose nontransactional effects cannot be undone.
#[compio::test]
async fn statement_cache_does_not_retry_0a000_after_parameter_input() {
    let url = test_url();
    let client = connect_with_statement_cache(&url, 3).await.unwrap();
    let table = common::test_object_name("cpg_cache_domain_shape");
    let sequence = common::test_object_name("cpg_cache_domain_0a000_seq");
    let function = common::test_object_name("cpg_cache_domain_0a000_check");
    let domain = common::test_object_name("cpg_cache_domain_0a000");
    client
        .batch_execute(&format!(
            "CREATE TEMP SEQUENCE {sequence}; \
             CREATE FUNCTION pg_temp.{function}(value text) \
             RETURNS boolean LANGUAGE plpgsql VOLATILE AS $function$ \
             BEGIN \
               PERFORM nextval('pg_temp.{sequence}'); \
               RETURN true; \
             END \
             $function$; \
             CREATE DOMAIN pg_temp.{domain} AS text \
             CHECK (pg_temp.{function}(VALUE)); \
             CREATE TEMP TABLE {table} (id int4); \
             INSERT INTO {table} VALUES (73)"
        ))
        .await
        .unwrap();

    let sql = format!(
        "SELECT {table}.*, \
        $1::pg_temp.{domain}::text AS bound \
        FROM {table}"
    );
    let warm = client
        .query(sql.as_str(), &[&DomainText("warm")])
        .await
        .unwrap();
    assert_eq!(warm[0].len(), 2);
    drop(warm);
    let stale_name = prepared_statement_name(&client, &sql).await;

    client
        .batch_execute(&format!(
            "ALTER TABLE {table} ADD COLUMN label text NOT NULL DEFAULT 'fresh'"
        ))
        .await
        .unwrap();

    let error = client
        .query(sql.as_str(), &[&DomainText("stale")])
        .await
        .expect_err("parameterized stale-plan input was replayed");
    assert_eq!(error.code(), Some(&SqlState::FEATURE_NOT_SUPPORTED));
    assert_eq!(
        error.as_db_error().and_then(|error| error.routine()),
        Some("RevalidateCachedQuery")
    );
    let after_error: i64 = client
        .query_one_scalar(&format!("SELECT last_value FROM pg_temp.{sequence}"), &[])
        .await
        .unwrap();
    assert_eq!(
        after_error, 2,
        "the failed call ran its domain input more than once"
    );
    assert!(
        prepared_statement_names(&client, &sql).await.is_empty(),
        "the unsafe stale entry was not invalidated"
    );

    let refreshed = client
        .query(sql.as_str(), &[&DomainText("fresh")])
        .await
        .unwrap();
    assert_eq!(refreshed[0].len(), 3);
    assert_eq!(refreshed[0].get::<_, i32>("id"), 73);
    assert_eq!(refreshed[0].get::<_, &str>("label"), "fresh");
    assert_eq!(refreshed[0].get::<_, &str>("bound"), "fresh");
    assert_ne!(prepared_statement_name(&client, &sql).await, stale_name);

    let after_fresh: i64 = client
        .query_one_scalar(&format!("SELECT last_value FROM pg_temp.{sequence}"), &[])
        .await
        .unwrap();
    assert_eq!(after_fresh, 3);
}

/// PostgreSQL aborts an open transaction after 0A000. Repreparing there
/// cannot heal the transaction, and must not replace the original diagnostic
/// with the failed transaction's 25P02.
#[compio::test]
async fn statement_cache_does_not_retry_0a000_inside_a_transaction() {
    let url = test_url();
    let client = connect_with_statement_cache(&url, 2).await.unwrap();
    let table = common::test_object_name("cpg_cache_plan_shape_in_tx");
    let sql = format!("SELECT * FROM {table}");

    client
        .batch_execute(&format!(
            "DROP TABLE IF EXISTS {table}; \
             CREATE TEMP TABLE {table} (id int4); \
             INSERT INTO {table} VALUES (68)"
        ))
        .await
        .unwrap();
    drop(client.query(sql.as_str(), &[]).await.unwrap());

    client
        .batch_execute(&format!(
            "ALTER TABLE {table} ADD COLUMN label text NOT NULL DEFAULT 'transaction'"
        ))
        .await
        .unwrap();

    client.batch_execute("BEGIN").await.unwrap();
    assert_eq!(
        client.transaction_status(),
        Some(TransactionStatus::InTransaction)
    );

    let stale_error = client.query(sql.as_str(), &[]).await.unwrap_err();
    assert_eq!(stale_error.code(), Some(&SqlState::FEATURE_NOT_SUPPORTED));
    assert_eq!(
        stale_error.as_db_error().map(|error| error.message()),
        Some("cached plan must not change result type")
    );

    // ErrorResponse can precede ReadyForQuery. This barrier makes the
    // server's failed-transaction status authoritative before we assert it.
    client.simple_query("").await.unwrap();
    assert_eq!(client.transaction_status(), Some(TransactionStatus::Failed));
    client.batch_execute("ROLLBACK").await.unwrap();

    let refreshed_rows = client.query(sql.as_str(), &[]).await.unwrap();
    assert_eq!(refreshed_rows[0].len(), 2);
    assert_eq!(refreshed_rows[0].get::<_, i32>("id"), 68);
    assert_eq!(refreshed_rows[0].get::<_, &str>("label"), "transaction");
}

/// ReadyForQuery reports `T` after SAVEPOINT, not only after a bare BEGIN. A
/// genuine missing-name failure there aborts the block and must remain the
/// caller's one server attempt rather than becoming a futile 25P02 retry.
#[compio::test]
async fn statement_cache_does_not_retry_after_a_savepoint() {
    let url = test_url();
    let client = connect_with_statement_cache(&url, 2).await.unwrap();

    const SQL: &str = "SELECT 85::int4 AS cpg_cache_savepoint";
    drop(client.query(SQL, &[]).await.unwrap());
    client
        .batch_execute("BEGIN; SAVEPOINT cpg_cache_retry_savepoint; DEALLOCATE ALL")
        .await
        .unwrap();
    assert_eq!(
        client.transaction_status(),
        Some(TransactionStatus::InTransaction),
        "SAVEPOINT did not leave the server status at T"
    );

    let error = client.query(SQL, &[]).await.unwrap_err();
    assert_eq!(error.code(), Some(&SqlState::INVALID_SQL_STATEMENT_NAME));
    client.simple_query("").await.unwrap();
    assert_eq!(
        client.transaction_status(),
        Some(TransactionStatus::Failed),
        "the missing-name Bind did not publish failed status E"
    );

    client.batch_execute("ROLLBACK").await.unwrap();
    let refreshed = client.query(SQL, &[]).await.unwrap();
    assert_eq!(refreshed[0].get::<_, i32>(0), 85);
}

/// Once ReadyForQuery has reported `E`, PostgreSQL rejects every ordinary
/// command until recovery. The cache must preserve that 25P02 and must not
/// mistake the block for an idle session eligible for stale replay.
#[compio::test]
async fn statement_cache_knows_an_existing_transaction_is_aborted() {
    let url = test_url();
    let client = connect_with_statement_cache(&url, 2).await.unwrap();

    const SQL: &str = "SELECT 86::int4 AS cpg_cache_already_failed";
    drop(client.query(SQL, &[]).await.unwrap());
    let cached_name = prepared_statement_name(&client, SQL).await;

    client.batch_execute("BEGIN").await.unwrap();
    let division_error = client.query("SELECT 1 / 0", &[]).await.unwrap_err();
    assert_eq!(division_error.code(), Some(&SqlState::DIVISION_BY_ZERO));
    client.simple_query("").await.unwrap();
    assert_eq!(client.transaction_status(), Some(TransactionStatus::Failed));

    let failed_error = client.query(SQL, &[]).await.unwrap_err();
    assert_eq!(
        failed_error.code(),
        Some(&SqlState::IN_FAILED_SQL_TRANSACTION)
    );
    client.batch_execute("ROLLBACK").await.unwrap();
    assert_eq!(
        prepared_statement_name(&client, SQL).await,
        cached_name,
        "a failed transaction displaced a healthy cached statement"
    );
    let rows = client.query(SQL, &[]).await.unwrap();
    assert_eq!(rows[0].get::<_, i32>(0), 86);
}

/// The same recovery as `statement_cache_retries_stale_result_shape_once_
/// after_0a000`, with ONE variable changed: an unrelated request is in flight
/// on the connection when the stale execution begins.
///
/// The retry used to be gated on `transaction_status() == Some(Idle)`, read
/// BEFORE the operation was sent. That accessor answers `None` -- "ask again
/// after the next round trip" -- whenever ANY transaction-capable request has
/// not reached its `ReadyForQuery`, so on a pipelined or concurrently used
/// connection the gate was never satisfied and the recovery was inert. A
/// `0A000` the sequential test proves is absorbed reached the caller instead,
/// and the two runs are indistinguishable from the outside: nothing logs, the
/// cache still self-heals on the NEXT call, and only this one query fails.
///
/// The peer is a server-side sleep, so the request is genuinely outstanding
/// rather than merely enqueued -- an undrained `RowStream` does NOT hold one,
/// because the connection task consumes a response whether or not its caller
/// polls. The precondition is asserted, not assumed: if `transaction_status()`
/// is not `None` when the stale query is fired, this test never exercised the
/// gate it exists for and says so rather than passing green.
#[compio::test]
async fn statement_cache_retries_stale_result_shape_while_a_peer_request_is_in_flight() {
    let url = test_url();
    let client = std::rc::Rc::new(connect_with_statement_cache(&url, 3).await.unwrap());
    let table = common::test_object_name("cpg_cache_shape_busy");
    let sql = format!("SELECT * FROM {table}");

    client
        .batch_execute(&format!(
            "CREATE TEMP TABLE {table} (id int4); INSERT INTO {table} VALUES (71)"
        ))
        .await
        .unwrap();
    let first_rows = client.query(sql.as_str(), &[]).await.unwrap();
    assert_eq!(first_rows[0].len(), 1);
    drop(first_rows);
    assert_eq!(prepared_statement_names(&client, &sql).await.len(), 1);

    client
        .batch_execute(&format!(
            "ALTER TABLE {table} ADD COLUMN label text NOT NULL DEFAULT 'busy'"
        ))
        .await
        .unwrap();

    let peer = {
        let client = std::rc::Rc::clone(&client);
        compio::runtime::spawn(async move { client.query("SELECT pg_sleep(1)", &[]).await })
    };

    // Poll for the precondition instead of sleeping a guessed interval: the
    // peer needs several round trips before its Execute is outstanding, and a
    // fixed wait would be a load-dependent coin flip in both directions.
    let deadline = std::time::Instant::now() + ADMIN_STATEMENT_TIMEOUT;
    while client.transaction_status().is_some() {
        assert!(
            std::time::Instant::now() < deadline,
            "the peer request never went in flight, so the busy gate was never exercised"
        );
        compio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(
        client.transaction_status(),
        None,
        "precondition: the stale execution must start while a peer request is in flight"
    );

    let refreshed_rows = client
        .query(sql.as_str(), &[])
        .await
        .expect("a busy connection must still absorb the stale-plan 0A000");
    assert_eq!(refreshed_rows[0].len(), 2);
    assert_eq!(refreshed_rows[0].get::<_, i32>("id"), 71);
    assert_eq!(refreshed_rows[0].get::<_, &str>("label"), "busy");
    assert_eq!(prepared_statement_names(&client, &sql).await.len(), 1);

    peer.await.unwrap().unwrap();
}

/// The retry barrier under SUSTAINED concurrency, which the single-peer test
/// above cannot reach.
///
/// That test pins the pre-send gate. This one pins the barrier itself, and the
/// two fail for different reasons. `query::sync` waits for its own
/// `ReadyForQuery`, and requests are FIFO, so in a closed system every earlier
/// request has already retired by then and the shared `transaction_status()`
/// reads `Some(Idle)` -- which is why a closed test cannot tell the two
/// instruments apart. Keep peers arriving and the count is nonzero at that
/// instant instead, `transaction_status()` answers `None`, and the barrier
/// declines forever. Reading the status byte off the barrier's OWN
/// `ReadyForQuery` is what makes the answer independent of unrelated traffic.
///
/// This arm is LOAD-MEASURED, not scripted: it reports the rounds it ruled on
/// and the peer queries that were in flight across them, and requires every
/// round to have recovered. Measured on both instruments: 40/40 recovered with
/// the status byte, 0/40 with the shared accessor, three consecutive runs each.
#[compio::test]
async fn statement_cache_retries_stale_result_shape_under_sustained_concurrency() {
    const ROUNDS: usize = 20;
    const PEERS: usize = 4;

    let url = test_url();
    let client = std::rc::Rc::new(connect_with_statement_cache(&url, 8).await.unwrap());
    let table = common::test_object_name("cpg_cache_shape_load");
    let sql = format!("SELECT * FROM {table}");
    client
        .batch_execute(&format!(
            "CREATE TEMP TABLE {table} (id int4); INSERT INTO {table} VALUES (72)"
        ))
        .await
        .unwrap();

    let stop = std::rc::Rc::new(std::cell::Cell::new(false));
    let mut peers = Vec::new();
    for peer in 0..PEERS {
        let client = std::rc::Rc::clone(&client);
        let stop = std::rc::Rc::clone(&stop);
        peers.push(compio::runtime::spawn(async move {
            // A server-side sleep, so each peer is genuinely outstanding
            // rather than merely enqueued.
            let sql = format!("SELECT {peer}::int4 AS cpg_load_peer, pg_sleep(0.002)");
            let mut sent = 0u32;
            while !stop.get() && client.query(sql.as_str(), &[]).await.is_ok() {
                sent += 1;
            }
            sent
        }));
    }

    let mut recovered = 0usize;
    let mut escaped = Vec::new();
    for round in 0..ROUNDS {
        client.query(sql.as_str(), &[]).await.unwrap();
        client
            .batch_execute(&format!(
                "ALTER TABLE {table} ADD COLUMN c_{round} int4 DEFAULT {round}"
            ))
            .await
            .unwrap();
        match client.query(sql.as_str(), &[]).await {
            Ok(rows) => {
                assert_eq!(rows[0].get::<_, i32>("id"), 72);
                recovered += 1;
            }
            Err(error) => escaped.push(format!("round {round}: {:?}", error.code())),
        }
    }

    stop.set(true);
    let mut peer_queries = 0u32;
    for peer in peers {
        peer_queries += peer.await.unwrap();
    }

    println!(
        "sustained stale-plan recovery: {recovered} of {ROUNDS} rounds recovered \
         behind {peer_queries} peer queries from {PEERS} peers"
    );
    assert!(
        escaped.is_empty(),
        "{} of {ROUNDS} rounds let the stale-plan error escape: {escaped:?}",
        escaped.len()
    );
    assert_eq!(recovered, ROUNDS);
    assert!(
        peer_queries >= u32::try_from(ROUNDS).unwrap(),
        "only {peer_queries} peer queries ran, so the connection was not \
         meaningfully busy and this arm ruled on nothing"
    );
}

/// An explicit Statement is a caller-owned object, even when the connection's
/// implicit SQL cache is enabled. Replacing it would silently change the
/// metadata and identity the caller chose to retain.
#[compio::test]
async fn statement_cache_does_not_reprepare_an_explicit_statement() {
    let url = test_url();
    let client = connect_with_statement_cache(&url, 2).await.unwrap();
    let table = common::test_object_name("cpg_explicit_plan_shape");
    let sql = format!("SELECT * FROM {table}");

    client
        .batch_execute(&format!(
            "CREATE TEMP TABLE {table} (id int4); \
             INSERT INTO {table} VALUES (70)"
        ))
        .await
        .unwrap();
    let statement = client.prepare(sql.as_str()).await.unwrap();
    drop(client.query(&statement, &[]).await.unwrap());

    client
        .batch_execute(&format!(
            "ALTER TABLE {table} ADD COLUMN label text NOT NULL DEFAULT 'caller-owned'"
        ))
        .await
        .unwrap();

    let stale_error = client.query(&statement, &[]).await.unwrap_err();
    assert_eq!(stale_error.code(), Some(&SqlState::FEATURE_NOT_SUPPORTED));

    let replacement = client.prepare(sql.as_str()).await.unwrap();
    let refreshed_rows = client.query(&replacement, &[]).await.unwrap();
    assert_eq!(refreshed_rows[0].len(), 2);
    assert_eq!(refreshed_rows[0].get::<_, i32>("id"), 70);
    assert_eq!(refreshed_rows[0].get::<_, &str>("label"), "caller-owned");
}

/// Cache configuration is not cache provenance. A fresh raw-SQL prepare has
/// no stale entry to heal, so its first 26000 is reported without a retry.
#[compio::test]
async fn statement_cache_does_not_retry_a_cold_26000() {
    use futures_util::StreamExt;
    use std::time::Duration;

    let url = test_url();
    let client = connect_with_statement_cache(&url, 2).await.unwrap();
    let mut events = client.query_events();

    const SQL: &str = "EXECUTE cpg_cache_cold_missing";
    const BARRIER_SQL: &str = "SELECT 71::int4 AS cpg_cache_cold_retry_barrier";
    let error = client.query(SQL, &[]).await.unwrap_err();
    assert_eq!(error.code(), Some(&SqlState::INVALID_SQL_STATEMENT_NAME));

    client.simple_query(BARRIER_SQL).await.unwrap();
    let mut failed_attempts = 0;
    loop {
        let event = compio::time::timeout(Duration::from_secs(5), events.next())
            .await
            .expect("query observer did not reach the cold-miss barrier")
            .expect("query observer closed before the cold-miss barrier");
        if event.sql() == SQL
            && matches!(
                event.outcome(),
                QueryOutcome::DatabaseError { code: Some(code) }
                    if code == &SqlState::INVALID_SQL_STATEMENT_NAME
            )
        {
            failed_attempts += 1;
        }
        if event.sql() == BARRIER_SQL {
            break;
        }
    }
    assert_eq!(failed_attempts, 1, "a cold prepare is not retry eligible");
}

/// A SQLSTATE is not proof that PostgreSQL's outer prepared-statement lookup
/// produced it. Domain input runs during Bind before plan acquisition, so this
/// application-raised 26000 crosses the phase check but must still propagate
/// without evicting or replaying the healthy cached statement.
#[compio::test]
async fn statement_cache_requires_server_provenance_before_retrying_26000() {
    let url = test_url();
    let client = connect_with_statement_cache(&url, 2).await.unwrap();
    let sequence = common::test_object_name("cpg_cache_domain_26000_seq");
    let function = common::test_object_name("cpg_cache_domain_26000_check");
    let domain = common::test_object_name("cpg_cache_domain_26000");
    client
        .batch_execute(&format!(
            "CREATE TEMP SEQUENCE {sequence}; \
             CREATE FUNCTION pg_temp.{function}(value text) \
             RETURNS boolean LANGUAGE plpgsql VOLATILE AS $function$ \
             BEGIN \
               IF value = 'raise' THEN \
                 PERFORM nextval('pg_temp.{sequence}'); \
                 RAISE EXCEPTION USING \
                   ERRCODE = '26000', MESSAGE = 'domain input raised 26000'; \
               END IF; \
               RETURN true; \
             END \
             $function$; \
             CREATE DOMAIN pg_temp.{domain} AS text \
             CHECK (pg_temp.{function}(VALUE))"
        ))
        .await
        .unwrap();

    let sql = format!("SELECT $1::pg_temp.{domain}");
    drop(
        client
            .query(sql.as_str(), &[&DomainText("ok")])
            .await
            .unwrap(),
    );
    let cached_name = prepared_statement_name(&client, &sql).await;

    let error = client
        .query(sql.as_str(), &[&DomainText("raise")])
        .await
        .unwrap_err();
    assert_eq!(error.code(), Some(&SqlState::INVALID_SQL_STATEMENT_NAME));
    assert_eq!(
        error.as_db_error().and_then(|error| error.routine()),
        Some("exec_stmt_raise"),
        "the fixture did not raise 26000 from application code"
    );

    let side_effects: i64 = client
        .query_one_scalar(&format!("SELECT last_value FROM pg_temp.{sequence}"), &[])
        .await
        .unwrap();
    assert_eq!(side_effects, 1, "Bind parameter input ran more than once");
    assert_eq!(
        prepared_statement_name(&client, &sql).await,
        cached_name,
        "an application-raised 26000 evicted a healthy cached statement"
    );
}

/// SQL `EXECUTE` runs after the outer protocol Bind completed. Its 26000 names
/// the SQL-level target, not the driver's still-live cached statement, so the
/// call must propagate the first failure without replaying it.
#[compio::test]
async fn statement_cache_does_not_retry_26000_after_bind_complete() {
    use futures_util::StreamExt;
    use std::time::Duration;

    let url = test_url();
    let client = connect_with_statement_cache(&url, 2).await.unwrap();
    let mut events = client.query_events();

    const BARRIER_SQL: &str = "SELECT 69::int4 AS cpg_cache_retry_barrier";
    let target = common::test_object_name("cpg_cache_retry_target");
    let sql = format!("EXECUTE {target}");
    client
        .batch_execute(&format!("PREPARE {target} AS SELECT 66::int4"))
        .await
        .unwrap();
    let warm_rows = client.query(&sql, &[]).await.unwrap();
    assert_eq!(warm_rows[0].get::<_, i32>(0), 66);
    let cached_name = prepared_statement_name(&client, &sql).await;

    client
        .batch_execute(&format!("DEALLOCATE {target}"))
        .await
        .unwrap();

    let second = compio::time::timeout(Duration::from_secs(5), client.query(sql.as_str(), &[]))
        .await
        .expect("the cached execution did not return its server error");
    let second_error = second.expect_err("the execution-time 26000 was hidden");
    assert_eq!(
        second_error.code(),
        Some(&SqlState::INVALID_SQL_STATEMENT_NAME)
    );

    client.simple_query(BARRIER_SQL).await.unwrap();
    let mut failed_attempts = 0;
    loop {
        let event = compio::time::timeout(Duration::from_secs(5), events.next())
            .await
            .expect("query observer did not reach the retry barrier")
            .expect("query observer closed before the retry barrier");
        if event.sql() == sql
            && matches!(
                event.outcome(),
                QueryOutcome::DatabaseError { code: Some(code) }
                    if code == &SqlState::INVALID_SQL_STATEMENT_NAME
            )
        {
            failed_attempts += 1;
        }
        if event.sql() == BARRIER_SQL {
            break;
        }
    }
    assert_eq!(
        failed_attempts, 1,
        "an execution-time 26000 must not replay the outer statement"
    );
    assert_eq!(
        prepared_statement_name(&client, &sql).await,
        cached_name,
        "an execution-time 26000 evicted the still-live outer statement"
    );
}

/// The retry cut is exactly `BindComplete`, not the first row or command tag.
/// This cached CALL binds successfully, commits one INSERT, then dynamic SQL
/// raises PostgreSQL's genuine `FetchPreparedStatement` 26000. Replaying after
/// that point would commit a second row even though the caller only made one
/// call.
#[compio::test]
async fn statement_cache_never_replays_a_committed_effect_after_bind_complete() {
    let url = test_url();
    let client = connect_with_statement_cache(&url, 2).await.unwrap();
    let table = common::test_object_name("cpg_cache_retry_rows");
    let procedure = common::test_object_name("cpg_cache_retry_procedure");
    let missing = common::test_object_name("cpg_cache_retry_missing");
    let sql = format!("CALL pg_temp.{procedure}($1)");

    client
        .batch_execute(&format!(
            "CREATE TEMP TABLE {table} (attempt int4 NOT NULL); \
             CREATE PROCEDURE pg_temp.{procedure}(run bool) \
             LANGUAGE plpgsql AS $procedure$ \
             BEGIN \
               IF run THEN \
                 INSERT INTO {table} VALUES (1); \
                 COMMIT; \
                 EXECUTE 'EXECUTE {missing}'; \
               END IF; \
             END \
             $procedure$"
        ))
        .await
        .unwrap();

    assert_eq!(client.execute(&sql, &[&false]).await.unwrap(), 0);
    let error = client.execute(&sql, &[&true]).await.unwrap_err();
    assert_eq!(error.code(), Some(&SqlState::INVALID_SQL_STATEMENT_NAME));
    assert_eq!(
        error.as_db_error().and_then(|error| error.routine()),
        Some("FetchPreparedStatement"),
        "the fixture did not reach PostgreSQL's prepared-statement lookup"
    );

    let committed: i64 = client
        .query_one_scalar(&format!("SELECT count(*) FROM {table}"), &[])
        .await
        .unwrap();
    assert_eq!(
        committed, 1,
        "one cached call committed its side effect more than once"
    );
}

/// A borrower can disappear after its cached execution reaches the request
/// queue but before it receives PostgreSQL's error. The connection task still
/// drains that response, so it must also retire the statement before returning
/// the physical session to another borrower.
#[compio::test]
async fn cancelled_cached_plan_error_is_not_handed_to_the_next_borrower() {
    use std::future::Future;
    use std::task::{Context, Waker};
    use std::time::Duration;

    let url = require_pg().await;
    let mut connection_config: Config = url.parse().unwrap();
    connection_config.statement_cache_capacity(2);
    let mut pool_config = PoolConfig::new();
    pool_config
        .max_size(1)
        .min_idle(1)
        .validation_bypass(Duration::from_secs(60));
    let pool = Pool::connect_with_config(connection_config, pool_config)
        .await
        .unwrap();
    let table = common::test_object_name("cpg_cancelled_cache_plan_shape");
    let sql = format!("SELECT * FROM {table}");

    let backend_pid;
    {
        let client = pool.get().await.unwrap();
        backend_pid = client.process_id();
        client
            .batch_execute(&format!(
                "CREATE TEMP TABLE {table} (id int4); \
                 INSERT INTO {table} VALUES (60)"
            ))
            .await
            .unwrap();

        let first_rows = client.query(sql.as_str(), &[]).await.unwrap();
        assert_eq!(first_rows[0].get::<_, i32>("id"), 60);
        drop(first_rows);

        client
            .batch_execute(&format!(
                "ALTER TABLE {table} ADD COLUMN label text NOT NULL DEFAULT 'borrower'"
            ))
            .await
            .unwrap();

        let mut stale = Box::pin(client.query_raw(sql.as_str(), std::iter::empty::<&i32>()));
        let mut context = Context::from_waker(Waker::noop());
        assert!(
            stale.as_mut().poll(&mut context).is_pending(),
            "the connection task ran during the caller's first poll, so the \
             cancellation cut no longer precedes PostgreSQL's response"
        );
        drop(stale);
    }

    let client = pool.get().await.unwrap();
    assert_eq!(
        client.process_id(),
        backend_pid,
        "the pool replaced the physical session instead of testing cache reuse"
    );
    let refreshed_rows = client.query(sql.as_str(), &[]).await.unwrap();
    assert_eq!(refreshed_rows[0].len(), 2);
    assert_eq!(refreshed_rows[0].get::<_, i32>("id"), 60);
    assert_eq!(refreshed_rows[0].get::<_, &str>("label"), "borrower");
}

/// If server-side state is cleared behind the cache, the first use reparses
/// the missing Statement on the same still-usable connection.
#[compio::test]
async fn statement_cache_retries_a_statement_missing_after_deallocate_all() {
    let url = test_url();
    let client = connect_with_statement_cache(&url, 2).await.unwrap();

    const SQL: &str = "SELECT 64::int4 AS cpg_cache_deallocated";
    let first_rows = client.query(SQL, &[]).await.unwrap();
    assert_eq!(first_rows[0].get::<_, i32>(0), 64);
    drop(first_rows);

    client.batch_execute("DEALLOCATE ALL").await.unwrap();
    assert!(prepared_statement_names(&client, SQL).await.is_empty());

    let refreshed_rows = client.query(SQL, &[]).await.unwrap();
    assert_eq!(refreshed_rows[0].get::<_, i32>(0), 64);
    assert_eq!(prepared_statement_names(&client, SQL).await.len(), 1);

    assert_eq!(
        simple_query_scalar_i32(&client, "SELECT 65::int4")
            .await
            .unwrap(),
        65
    );
}

/// COPY IN has its own pre-Bind stale-cache recovery arm. Losing that arm
/// leaves the cached name invalid after `DEALLOCATE ALL`, even though ordinary
/// query recovery still works.
#[compio::test]
async fn stale_cached_copy_in_reprepares_before_bind() {
    use bytes::Bytes;
    use futures_util::SinkExt;

    let url = test_url();
    let client = connect_with_statement_cache(&url, 2).await.unwrap();
    let table = common::test_object_name("cpg_stale_cached_copy_in");

    client
        .batch_execute(&format!("CREATE TEMP TABLE {table} (n int4 NOT NULL)"))
        .await
        .unwrap();
    let sql = format!("COPY {table} (n) FROM STDIN /* cpg_stale_cached_copy_in */");

    let first = client.copy_in::<_, Bytes>(&sql).await.unwrap();
    let mut first = Box::pin(first);
    first
        .as_mut()
        .send(Bytes::from_static(b"17\n"))
        .await
        .unwrap();
    assert_eq!(first.as_mut().finish().await.unwrap(), 1);
    drop(first);
    assert_eq!(prepared_statement_names(&client, &sql).await.len(), 1);

    client.batch_execute("DEALLOCATE ALL").await.unwrap();
    assert!(prepared_statement_names(&client, &sql).await.is_empty());

    let retried = client.copy_in::<_, Bytes>(&sql).await.unwrap();
    let mut retried = Box::pin(retried);
    retried
        .as_mut()
        .send(Bytes::from_static(b"23\n"))
        .await
        .unwrap();
    assert_eq!(retried.as_mut().finish().await.unwrap(), 1);
    drop(retried);

    assert_eq!(prepared_statement_names(&client, &sql).await.len(), 1);
    assert_eq!(
        simple_query_scalar_i32(&client, &format!("SELECT sum(n)::int4 FROM {table}"))
            .await
            .unwrap(),
        40
    );
}

/// COPY OUT has a separate pre-Bind stale-cache recovery arm from both query
/// and COPY IN, so exercise its cached name independently.
#[compio::test]
async fn stale_cached_copy_out_reprepares_before_bind() {
    use futures_util::StreamExt;

    let url = test_url();
    let client = connect_with_statement_cache(&url, 2).await.unwrap();

    const SQL: &str = "COPY (SELECT 73::int4) TO STDOUT /* cpg_stale_cached_copy_out */";
    let first = client.copy_out(SQL).await.unwrap();
    let mut first = Box::pin(first);
    let mut first_body = Vec::new();
    while let Some(chunk) = first.as_mut().next().await {
        first_body.extend_from_slice(&chunk.unwrap());
    }
    drop(first);
    assert_eq!(first_body, b"73\n");
    assert_eq!(prepared_statement_names(&client, SQL).await.len(), 1);

    client.batch_execute("DEALLOCATE ALL").await.unwrap();
    assert!(prepared_statement_names(&client, SQL).await.is_empty());

    let retried = client.copy_out(SQL).await.unwrap();
    let mut retried = Box::pin(retried);
    let mut retried_body = Vec::new();
    while let Some(chunk) = retried.as_mut().next().await {
        retried_body.extend_from_slice(&chunk.unwrap());
    }
    drop(retried);

    assert_eq!(retried_body, b"73\n");
    assert_eq!(prepared_statement_names(&client, SQL).await.len(), 1);
}

/// `DISCARD ALL` includes `DEALLOCATE ALL`, so it invalidates protocol-level
/// prepared names just as directly as the narrower command. The next cached
/// use must recover on the same session.
#[compio::test]
async fn statement_cache_retries_a_statement_missing_after_discard_all() {
    let url = test_url();
    let client = connect_with_statement_cache(&url, 2).await.unwrap();

    const SQL: &str = "SELECT 84::int4 AS cpg_cache_discarded";
    let first = client.query(SQL, &[]).await.unwrap();
    assert_eq!(first[0].get::<_, i32>(0), 84);
    drop(first);

    client.batch_execute("DISCARD ALL").await.unwrap();
    assert!(prepared_statement_names(&client, SQL).await.is_empty());

    let refreshed = client.query(SQL, &[]).await.unwrap();
    assert_eq!(refreshed[0].get::<_, i32>(0), 84);
    assert_eq!(prepared_statement_names(&client, SQL).await.len(), 1);
}

/// A missing cached DML statement failed before execution. Its replacement
/// therefore applies the mutation once, with the original bound value.
#[compio::test]
async fn statement_cache_retries_execute_without_double_applying() {
    let url = test_url();
    let client = connect_with_statement_cache(&url, 2).await.unwrap();
    let table = common::test_object_name("cpg_cache_execute_retry");

    client
        .batch_execute(&format!("CREATE TEMP TABLE {table} (n int4 NOT NULL)"))
        .await
        .unwrap();
    client
        .execute(&format!("INSERT INTO {table} VALUES (0)"), &[])
        .await
        .unwrap();

    let sql = format!("UPDATE {table} SET n = n + $1");
    assert_eq!(client.execute(sql.as_str(), &[&1_i32]).await.unwrap(), 1);
    client.batch_execute("DEALLOCATE ALL").await.unwrap();
    assert_eq!(client.execute(sql.as_str(), &[&2_i32]).await.unwrap(), 1);

    let value: i32 = client
        .query_one_scalar(&format!("SELECT n FROM {table}"), &[])
        .await
        .unwrap();
    assert_eq!(value, 3, "the failed cached execution applied no mutation");
}

async fn simple_query_scalar_i32(client: &Client, sql: &str) -> Result<i32, Error> {
    let messages = client.simple_query(sql).await?;
    Ok(messages
        .iter()
        .find_map(|message| match message {
            SimpleQueryMessage::Row(row) => row.get(0),
            _ => None,
        })
        .expect("scalar query returns one row")
        .parse()
        .unwrap())
}

/// Prepare- and execute-time server errors end with Sync/ReadyForQuery. The
/// connection task must drain that terminator even though the caller stops at
/// ErrorResponse.
#[compio::test]
async fn prepared_statement_server_errors_do_not_poison_the_connection() {
    use compio_postgres::types::Type;

    let url = require_pg().await;
    let client = connect(&url).await.unwrap();
    let result_shape_table = common::test_object_name("cpg_prep_result_shape");
    let dropped_table = common::test_object_name("cpg_prep_dropped");
    let typed_table = common::test_object_name("cpg_prep_typed");

    client
        .batch_execute(&format!(
            "DROP TABLE IF EXISTS {result_shape_table}; \
             CREATE TEMP TABLE {result_shape_table} (id int4); \
             INSERT INTO {result_shape_table} VALUES (1)"
        ))
        .await
        .unwrap();
    let result_shape = client
        .prepare(&format!("SELECT * FROM {result_shape_table}"))
        .await
        .unwrap();
    client
        .batch_execute(&format!(
            "ALTER TABLE {result_shape_table} ADD COLUMN label text"
        ))
        .await
        .unwrap();
    let shape_error = client.query(&result_shape, &[]).await.unwrap_err();
    let shape_code = shape_error.code().map(SqlState::code).map(str::to_string);
    let shape_detail = common::error_chain(&shape_error);
    let recovered_after_shape: i32 = client
        .query_one_scalar("SELECT 45::int4", &[])
        .await
        .unwrap();
    drop(result_shape);
    client
        .batch_execute(&format!("DROP TABLE {result_shape_table}"))
        .await
        .unwrap();

    client
        .batch_execute(&format!(
            "DROP TABLE IF EXISTS {dropped_table}; \
             CREATE TEMP TABLE {dropped_table} (id int4)"
        ))
        .await
        .unwrap();
    let dropped = client
        .prepare(&format!("SELECT * FROM {dropped_table}"))
        .await
        .unwrap();
    client
        .batch_execute(&format!("DROP TABLE {dropped_table}"))
        .await
        .unwrap();
    let dropped_error = client.query(&dropped, &[]).await.unwrap_err();
    let dropped_code = dropped_error.code().map(SqlState::code).map(str::to_string);
    let dropped_detail = common::error_chain(&dropped_error);
    let recovered_after_drop: i32 = client
        .query_one_scalar("SELECT 46::int4", &[])
        .await
        .unwrap();
    drop(dropped);

    client
        .batch_execute(&format!(
            "DROP TABLE IF EXISTS {typed_table}; \
             CREATE TEMP TABLE {typed_table} (n int4)"
        ))
        .await
        .unwrap();
    let typed_error = client
        .prepare_typed(
            &format!("INSERT INTO {typed_table} (n) VALUES ($1)"),
            &[Type::TEXT],
        )
        .await
        .unwrap_err();
    let typed_code = typed_error.code().map(SqlState::code).map(str::to_string);
    let typed_detail = common::error_chain(&typed_error);
    let recovered_after_typed: i32 = client
        .query_one_scalar("SELECT 47::int4", &[])
        .await
        .unwrap();
    client
        .batch_execute(&format!("DROP TABLE {typed_table}"))
        .await
        .unwrap();

    assert_eq!(shape_code.as_deref(), Some("0A000"), "{shape_detail}");
    assert_eq!(dropped_code.as_deref(), Some("42P01"), "{dropped_detail}");
    assert_eq!(typed_code.as_deref(), Some("42804"), "{typed_detail}");
    assert_eq!(
        (
            recovered_after_shape,
            recovered_after_drop,
            recovered_after_typed,
        ),
        (45, 46, 47)
    );
}

/// Public prepare() does not deduplicate SQL. Each call owns a distinct server
/// statement, and only dropping the last clone closes that statement's name.
#[compio::test]
async fn identical_sql_has_distinct_names_and_each_drop_closes_one() {
    let url = require_pg().await;
    let client = connect(&url).await.unwrap();

    const SQL: &str = "SELECT 48::int4 AS cpg_prep_duplicate_sql";
    let first = client.prepare(SQL).await.unwrap();
    let first_clone = first.clone();
    let second = client.prepare(SQL).await.unwrap();
    let mut while_both_live = prepared_statement_names(&client, SQL).await;
    while_both_live.sort();

    drop(first);
    client.simple_query("").await.unwrap();
    let while_clone_live = prepared_statement_names(&client, SQL).await;

    drop(first_clone);
    client.simple_query("").await.unwrap();
    let after_first_drop = prepared_statement_names(&client, SQL).await;
    let second_value: i32 = client.query_one_scalar(&second, &[]).await.unwrap();

    drop(second);
    client.simple_query("").await.unwrap();
    let after_second_drop = prepared_statement_names(&client, SQL).await;

    assert_eq!(
        while_both_live.len(),
        2,
        "same SQL was unexpectedly deduplicated"
    );
    assert_ne!(while_both_live[0], while_both_live[1]);
    assert_eq!(while_clone_live.len(), 2);
    assert_eq!(after_first_drop.len(), 1);
    assert_eq!(second_value, 48);
    assert!(after_second_drop.is_empty());
}

/// PostgreSQL prepared statements are session-scoped, not
/// transaction-scoped, so a Statement prepared in a rolled-back transaction
/// remains valid on the same Client.
#[compio::test]
async fn statement_prepared_in_rolled_back_transaction_remains_usable() {
    let url = require_pg().await;
    let mut client = connect(&url).await.unwrap();

    let transaction = client.transaction().await.unwrap();
    let statement = transaction.prepare("SELECT $1::int4 + 1").await.unwrap();
    transaction.rollback().await.unwrap();

    let value: i32 = client
        .query_one_scalar(&statement, &[&48_i32])
        .await
        .unwrap();
    let recovered: i32 = client
        .query_one_scalar("SELECT 50::int4", &[])
        .await
        .unwrap();
    drop(statement);
    client.simple_query("").await.unwrap();

    assert_eq!(value, 49);
    assert_eq!(recovered, 50);
}

/// A `Statement` carries the session that prepared it. Passing it to another
/// live client is a local ownership error, not `PostgreSQL`'s `26000`, and a
/// statement whose owner is gone has a distinct diagnosis.
#[compio::test]
async fn foreign_statement_is_rejected_locally() {
    let url = Box::pin(require_pg()).await;
    let owner = connect(&url).await.unwrap();
    let other = connect(&url).await.unwrap();
    let statement = owner.prepare("SELECT $1::int4 + 1").await.unwrap();

    let foreign = other
        .query(&statement, &[&41_i32])
        .await
        .expect_err("a foreign statement reached PostgreSQL");
    assert_eq!(
        foreign.to_string(),
        "prepared statement belongs to a different connection"
    );
    assert!(
        foreign.as_db_error().is_none(),
        "the ownership error came from PostgreSQL rather than the driver"
    );
    assert_eq!(foreign.code(), None, "a local error must have no SQLSTATE");

    drop(owner);
    let dropped = other
        .query(&statement, &[&41_i32])
        .await
        .expect_err("a statement with no owner reached PostgreSQL");
    assert_eq!(
        dropped.to_string(),
        "prepared statement's owning connection has been dropped"
    );
    assert!(
        dropped.as_db_error().is_none(),
        "the dropped-owner error came from PostgreSQL rather than the driver"
    );
    assert_eq!(dropped.code(), None, "a local error must have no SQLSTATE");

    let recovered: i32 = other
        .query_one_scalar("SELECT 42::int4", &[])
        .await
        .unwrap();
    assert_eq!(recovered, 42, "a local refusal damaged the other client");
}

/// The ownership check must preserve every wrapper that uses the same
/// `InnerClient`: direct `Client` calls, `Transaction`, and a `PooledClient`
/// borrow.
#[compio::test]
async fn statement_works_on_its_owner_through_transaction_and_pool() {
    let url = Box::pin(require_pg()).await;
    let mut client = connect(&url).await.unwrap();
    let statement = client.prepare("SELECT $1::int4 + 1").await.unwrap();

    let direct: i32 = client
        .query_one_scalar(&statement, &[&40_i32])
        .await
        .unwrap();
    let transaction = client.transaction().await.unwrap();
    let through_transaction: i32 = transaction
        .query_one(&statement, &[&41_i32])
        .await
        .unwrap()
        .get(0);
    transaction.commit().await.unwrap();

    let pool = single_connection_pool(&url).await;
    let mut pooled = Box::pin(pool.get()).await.unwrap();
    let pooled_statement = pooled.prepare("SELECT $1::int4 + 1").await.unwrap();
    let through_pool: i32 = pooled
        .query_one_scalar(&pooled_statement, &[&42_i32])
        .await
        .unwrap();
    let pooled_transaction = pooled.transaction().await.unwrap();
    let through_pooled_transaction: i32 = pooled_transaction
        .query_one(&pooled_statement, &[&43_i32])
        .await
        .unwrap()
        .get(0);
    pooled_transaction.commit().await.unwrap();

    assert_eq!(
        (
            direct,
            through_transaction,
            through_pool,
            through_pooled_transaction,
        ),
        (41, 42, 43, 44)
    );
}

/// SQL PREPARE and protocol Parse share one namespace. A collision must return
/// PostgreSQL's original error without closing the statement which already
/// owned the generated name.
#[compio::test]
async fn generated_name_collision_preserves_existing_statement() {
    let url = require_pg().await;
    let client = connect(&url).await.unwrap();

    const PROBE_SQL: &str = "SELECT 8675309::int4 AS cpg_prep_name_probe";
    let probe = client.prepare(PROBE_SQL).await.unwrap();
    let probe_name = prepared_statement_name(&client, PROBE_SQL).await;
    let probe_id = driver_statement_id(&probe_name);

    drop(probe);
    client.simple_query("").await.unwrap();
    let reserved = reserve_driver_statement_names(&client, probe_id, 99).await;
    assert_eq!(prepared_statement_count(&client).await, reserved);
    client.batch_execute("BEGIN").await.unwrap();

    let prepared = client
        .prepare("SELECT 1::int4 AS cpg_prep_collision_result")
        .await;
    let prepare_code = prepared
        .as_ref()
        .err()
        .and_then(Error::code)
        .map(SqlState::code)
        .map(str::to_string);
    let collided_name = prepared
        .as_ref()
        .err()
        .and_then(Error::as_db_error)
        .and_then(|error| error.message().split('"').nth(1))
        .map(str::to_string);
    client.batch_execute("ROLLBACK").await.unwrap();

    // After rolling back PostgreSQL's expected failed-transaction state, the
    // ErrorResponse and trailing ReadyForQuery must leave this same connection
    // aligned.
    let recovered_after_prepare = simple_query_scalar_i32(&client, "SELECT 41::int4")
        .await
        .unwrap();

    let existing_value = match collided_name.as_ref() {
        Some(name) => Some(simple_query_scalar_i32(&client, &format!("EXECUTE {name}")).await),
        None => None,
    };

    // On the regression path EXECUTE fails because the guard deleted the
    // collided statement. Prove that error does not poison the protocol.
    let recovered_after_execute = simple_query_scalar_i32(&client, "SELECT 42::int4")
        .await
        .unwrap();

    drop(prepared);
    client.batch_execute("DEALLOCATE ALL").await.unwrap();

    assert_eq!(recovered_after_prepare, 41);
    assert_eq!(recovered_after_execute, 42);
    assert_eq!(prepare_code.as_deref(), Some("42P05"));
    let existing_outcome = match existing_value {
        Some(Ok(value)) => format!("value {value}"),
        Some(Err(error)) => format!("error {:?}: {}", error.code(), common::error_chain(&error)),
        None => "prepare did not return a colliding statement name".to_string(),
    };
    assert_eq!(
        existing_outcome, "value 99",
        "prepare destroyed the statement which owned the collided name"
    );
}

/// Cancellation before ParseComplete is the ambiguous collision case: cleanup
/// must wait to learn whether this Parse acquired the name before sending
/// Close, or it can delete the statement which caused 42P05.
#[compio::test]
async fn cancelled_name_collision_preserves_existing_statement() {
    use std::future::Future;
    use std::pin::pin;
    use std::task::{Context, Waker};

    let url = require_pg().await;
    let client = connect(&url).await.unwrap();

    const PROBE_SQL: &str = "SELECT 7654321::int4 AS cpg_prep_cancel_name_probe";
    let probe = client.prepare(PROBE_SQL).await.unwrap();
    let probe_name = prepared_statement_name(&client, PROBE_SQL).await;
    let probe_id = driver_statement_id(&probe_name);

    drop(probe);
    client.simple_query("").await.unwrap();
    let reserved = reserve_driver_statement_names(&client, probe_id, 98).await;
    assert_eq!(prepared_statement_count(&client).await, reserved);

    {
        let mut prepare = pin!(client.prepare("SELECT 2::int4"));
        let mut cx = Context::from_waker(Waker::noop());
        assert!(
            prepare.as_mut().poll(&mut cx).is_pending(),
            "the first poll ran the connection task and stopped testing cancellation"
        );
    }

    client.simple_query("").await.unwrap();
    let remaining = prepared_statement_count(&client).await;
    let recovered = simple_query_scalar_i32(&client, "SELECT 51::int4")
        .await
        .unwrap();
    client.batch_execute("DEALLOCATE ALL").await.unwrap();

    assert_eq!(recovered, 51);
    assert_eq!(
        remaining, reserved,
        "cancelled colliding Parse closed a statement it never owned"
    );
}

/// PostgreSQL 14 added multiranges, and postgres-types exposes
/// Kind::Multirange. User-defined multiranges must carry the same subtype
/// metadata as their corresponding user-defined ranges.
#[compio::test]
async fn custom_multirange_resolves_its_element_type() {
    use compio_postgres::types::{Kind, Type};

    let url = require_pg().await;
    let client = connect(&url).await.unwrap();
    let range = common::test_object_name("cpg_prep_range");
    let multirange = common::test_object_name("cpg_prep_multirange");

    client
        .batch_execute(&format!(
            "DROP TYPE IF EXISTS {range} CASCADE; \
             DROP TYPE IF EXISTS {multirange} CASCADE; \
             CREATE TYPE {range} AS RANGE ( \
                 subtype = int4, \
                 multirange_type_name = {multirange} \
             )"
        ))
        .await
        .unwrap();

    let statement = client
        .prepare(&format!("SELECT '{{}}'::{multirange}"))
        .await
        .unwrap();
    let actual = statement.columns()[0].type_().kind().clone();

    let recovered: i32 = client
        .query_one_scalar("SELECT 44::int4", &[])
        .await
        .unwrap();

    drop(statement);
    client
        .batch_execute(&format!(
            "DROP TYPE IF EXISTS {range} CASCADE; \
             DROP TYPE IF EXISTS {multirange} CASCADE"
        ))
        .await
        .unwrap();

    assert_eq!(recovered, 44);
    assert_eq!(actual, Kind::Multirange(Type::INT4));
}

/// A COPY the server refuses to START must not poison its connection.
///
/// `copy_in` on a missing table is rejected before any CopyInResponse: the
/// server answers the Bind/Execute with `42P01` and never enters COPY mode.
/// The driver's failure path still has to leave the session synchronized, or
/// every later request on that connection reads a message it did not expect.
///
/// Asserted on the NEXT query over the SAME client, because that is where the
/// damage shows; the COPY's own error is correct either way.
#[compio::test]
async fn rejected_copy_start_does_not_poison_the_connection() {
    use bytes::Bytes;

    let url = require_pg().await;
    let client = connect(&url).await.unwrap();

    let err = client
        .copy_in::<_, Bytes>("COPY cpg_copy_no_such_table (n) FROM STDIN")
        .await
        .err()
        .expect("a COPY into a missing table cannot start");
    assert_eq!(
        err.code(),
        Some(&SqlState::UNDEFINED_TABLE),
        "expected 42P01 from the refused COPY start: {}",
        common::error_chain(&err)
    );

    let row = compio::time::timeout(
        std::time::Duration::from_secs(5),
        client.query_one("SELECT 1::int4 AS n", &[]),
    )
    .await
    .expect("the connection hung after a refused COPY start")
    .expect("the refused COPY start poisoned its connection");
    assert_eq!(row.get::<_, i32>("n"), 1);
}

/// The resolution between `application_name` and `fallback_application_name`
/// only matters if it reaches the startup packet, so ask the server which name
/// the session ended up with rather than trusting the config accessor.
#[compio::test]
async fn fallback_application_name_names_the_session() {
    let url = require_pg().await;
    let sep = if url.contains('?') { '&' } else { '?' };

    let session_name = |dsn: String| async move {
        let mut config: Config = dsn.parse().unwrap();
        config.statement_cache_capacity(0);
        let (client, connection) = config.connect(common::suite_tls()).await.unwrap();
        compio::runtime::spawn(async move {
            let _ = connection.run().await;
        })
        .detach();
        client
            .query_one("SHOW application_name", &[])
            .await
            .unwrap()
            .get::<_, String>(0)
    };

    assert_eq!(
        session_name(format!("{url}{sep}fallback_application_name=faller")).await,
        "faller",
        "the fallback did not reach the startup packet"
    );

    // One variable apart: with a primary present the fallback must not win.
    assert_eq!(
        session_name(format!(
            "{url}{sep}application_name=primary&fallback_application_name=faller"
        ))
        .await,
        "primary",
        "the fallback displaced an application_name the caller set"
    );
}

/// The same abandonment as `dropped_copy_in_sink_recovers_the_same_connection`,
/// but through a pool, because that is where this class of damage has actually
/// cost us: a refused COPY start once left two unowned messages on the wire and
/// the entry went back to the pool to break every later borrower.
///
/// Asserted by reacquiring and checking the backend PID is unchanged, so the
/// test fails if the pool silently replaced a broken session instead of
/// returning a healthy one.
#[compio::test]
async fn an_abandoned_copy_in_returns_a_usable_entry_to_the_pool() {
    use bytes::Bytes;
    use futures_util::SinkExt;

    let url = require_pg().await;
    let mut pool_config = PoolConfig::new();
    pool_config.max_size(1).min_idle(1);
    let pool = Pool::connect_with_pool_config(&url, pool_config)
        .await
        .unwrap();
    let table = common::test_object_name("cpg_pooled_abandoned_copy");

    let borrowed_pid = {
        let client = pool.get().await.unwrap();
        let pid = client.process_id();
        client
            .batch_execute(&format!("CREATE TABLE IF NOT EXISTS {table} (n int4)"))
            .await
            .unwrap();
        {
            let mut sink = std::pin::pin!(
                client
                    .copy_in::<_, Bytes>(&format!("COPY {table} (n) FROM STDIN"))
                    .await
                    .expect("the COPY must start")
            );
            sink.send(Bytes::from_static(b"1\n"))
                .await
                .expect("stream one row so the COPY is in flight");
            // Dropped without finish, then the pooled client is returned below.
        }
        pid
    };

    let client = compio::time::timeout(std::time::Duration::from_secs(5), pool.get())
        .await
        .expect("reacquiring after an abandoned COPY hung")
        .expect("the pool refused to hand back an entry");
    assert_eq!(
        client.process_id(),
        borrowed_pid,
        "the pool replaced the session instead of returning the one under test"
    );
    let row = client
        .query_one("SELECT 1::int4 AS n", &[])
        .await
        .expect("the abandoned COPY poisoned the pooled entry");
    assert_eq!(row.get::<_, i32>("n"), 1);

    client
        .batch_execute(&format!("DROP TABLE IF EXISTS {table}"))
        .await
        .unwrap();
}
#[compio::test]
async fn statement_cache_threshold_one_does_not_cache_wrong_parameter_arity() {
    compio::time::timeout(std::time::Duration::from_secs(10), async {
        let url = test_url();
        let client = connect_with_statement_cache_threshold(&url, 2, 1)
            .await
            .unwrap();

        const SQL: &str = "SELECT count(*)::int8 FROM pg_prepared_statements \
            WHERE statement = $1::text AND NOT from_sql \
            /* cpg_cache_threshold_one_rejected_execution */";

        client
            .query(SQL, &[])
            .await
            .expect_err("the SQL requires one parameter");
        assert!(
            prepared_statement_names(&client, SQL).await.is_empty(),
            "wrong parameter arity populated the threshold-one statement cache"
        );

        let first_valid: i64 = client.query_one_scalar(SQL, &[&SQL]).await.unwrap();
        assert_eq!(
            first_valid, 1,
            "the first validated execution did not earn threshold-one admission"
        );
    })
    .await
    .expect("threshold-one rejected-execution test exceeded its watchdog");
}

#[compio::test]
async fn abandoned_copy_in_startup_rejection_does_not_poison_the_next_operation() {
    use bytes::Bytes;
    use std::future::Future;
    use std::task::{Context, Poll, Waker};

    compio::time::timeout(std::time::Duration::from_secs(10), async {
        let url = require_pg().await;
        let target = connect(&url).await.unwrap();
        let blocker = connect(&url).await.unwrap();
        let table = common::test_object_name("cpg_copy_abandoned_startup");

        target
            .batch_execute(&format!("CREATE TABLE {table} (value text NOT NULL)"))
            .await
            .unwrap();
        let statement = target
            .prepare(&format!("COPY {table} (value) FROM STDIN"))
            .await
            .unwrap();
        blocker
            .batch_execute(&format!(
                "BEGIN; LOCK TABLE {table} IN ACCESS EXCLUSIVE MODE"
            ))
            .await
            .unwrap();

        let mut startup = Box::pin(target.copy_in::<_, Bytes>(&statement));
        let mut context = Context::from_waker(Waker::noop());
        assert!(
            matches!(startup.as_mut().poll(&mut context), Poll::Pending),
            "COPY startup completed while its table lock was held"
        );
        drop(startup);

        loop {
            let waiting: bool = blocker
                .query_one_scalar(
                    "SELECT EXISTS (\
                         SELECT 1 FROM pg_stat_activity \
                         WHERE pid = $1 \
                           AND state = 'active' \
                           AND wait_event_type = 'Lock'\
                     )",
                    &[&target.process_id()],
                )
                .await
                .expect("inspect the abandoned COPY backend");
            if waiting {
                break;
            }
        }

        blocker
            .batch_execute(&format!("DROP TABLE {table}; COMMIT"))
            .await
            .unwrap();

        let answer: i32 = target
            .query_one_scalar("SELECT 42::int4", &[])
            .await
            .expect("abandoned rejected COPY poisoned the same connection");
        assert_eq!(answer, 42);
    })
    .await
    .expect("abandoned COPY startup recovery exceeded its watchdog");
}

/// The empty query string through every extended-protocol entry point.
///
/// Each of these drives an arm of `query.rs`'s response state machine that
/// nothing else in the suite reaches: an empty statement earns `NoData` from
/// its `Describe` and `EmptyQueryResponse` from its `Execute`, so
/// `query_typed` / `query_text_params` must return an empty `RowStream` rather
/// than fall through to `unexpected_message`, and `execute` / `execute_typed` /
/// `execute_text_params` must report zero rows rather than an error.
///
/// Measured with `cargo llvm-cov` on 2026-08-23: `query.rs` lines 305-307, 376,
/// 381 and 464 were unexecuted by the WHOLE suite before this existed, so the
/// claim "those arms are correct" rested on reading alone. It also pins that
/// the session is still usable afterwards, which is the part that breaks if one
/// of them ever stops consuming through its `ReadyForQuery`.
#[compio::test]
async fn an_empty_query_is_accepted_by_every_extended_protocol_entry_point() {
    compio::time::timeout(PIPELINED_FAILURE_WATCHDOG, async {
        let url = require_pg().await;
        let client = connect(&url).await.unwrap();

        assert!(
            client.query_typed("", &[]).await.unwrap().is_empty(),
            "query_typed returned rows for an empty statement"
        );
        assert_eq!(client.execute_typed("", &[]).await.unwrap(), 0);
        assert!(
            client.query_text_params("", &[]).await.unwrap().is_empty(),
            "query_text_params returned rows for an empty statement"
        );
        assert_eq!(client.execute_text_params("", &[]).await.unwrap(), 0);

        let empty = client
            .prepare("")
            .await
            .expect("prepare an empty statement");
        assert_eq!(client.execute(&empty, &[]).await.unwrap(), 0);
        assert!(client.query(&empty, &[]).await.unwrap().is_empty());

        // `execute*` over a statement that DOES produce rows: the `DataRow`
        // arms these paths must skip rather than refuse.
        assert_eq!(
            client
                .execute_text_params("SELECT $1::int4", &[Some("5".to_string())])
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            client.execute_typed("SELECT 1, 2, 3", &[]).await.unwrap(),
            1
        );

        assert!(!client.is_closed(), "an empty query retired the session");
        assert_autocommit_connection_is_reusable(&client, 7_900).await;
    })
    .await
    .expect("empty-query entry-point test exceeded its watchdog");
}
