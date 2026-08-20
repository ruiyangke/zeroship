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
use compio_postgres::{Client, Error, NoTls, Pool, PoolConfig, TransactionStatus};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

mod common;

fn test_url() -> String {
    common::env::get(common::env::TestEnvKey::PgTestUrl)
        .unwrap_or_else(|| "postgres://postgres:zeroship@localhost:5440/zeroship".to_string())
}

/// Longest identifier PostgreSQL stores (NAMEDATALEN - 1). It truncates
/// anything longer without failing, which would quietly map two long test
/// names onto one schema.
const MAX_IDENT_LEN: usize = 63;

const SCHEMA_PREFIX: &str = "cpg_";

/// Names the private schema belonging to the calling test.
///
/// libtest runs each test on a thread named after the test - at any
/// `--test-threads` setting, serial runs included - so the thread name is a
/// per-test identifier that a newly added test gets for free and cannot forget
/// to declare. The `unnamed` fallback only applies to a thread the test body
/// spawned itself; such a thread shares the schema of whichever test is
/// running, so open connections from the test's own thread.
///
/// The name is sanitised to `[a-z0-9_]` so it needs no quoting, and carries a
/// hash of the full test name so that cutting the readable part down to
/// `MAX_IDENT_LEN` cannot make two schemas collide.
fn test_schema() -> String {
    let name = std::thread::current().name().unwrap_or("unnamed").to_owned();

    let mut hasher = DefaultHasher::new();
    name.hash(&mut hasher);
    let digest = hasher.finish();

    // Fixed cost of the wrapper: prefix, the separator before the digest, and
    // the digest's 16 hex characters.
    let budget = MAX_IDENT_LEN - SCHEMA_PREFIX.len() - 1 - 16;
    let readable: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .take(budget)
        .collect();

    format!("{SCHEMA_PREFIX}{readable}_{digest:016x}")
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
    let (client, connection) = compio_postgres::connect(url, NoTls).await?;
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
/// The schema is reset here rather than dropped when the test ends: a test
/// that panics never reaches its own teardown, and reclaiming at the start
/// makes each run self-healing. The set of schemas is bounded by the set of
/// test names, so they do not accumulate across runs.
///
/// Because the schema name depends only on the test name, two `cargo test`
/// processes pointed at one database would reset each other's schemas
/// mid-run. Give each concurrent run its own database via `PG_TEST_URL`.
///
/// A schema does not isolate everything: LISTEN/NOTIFY channels, advisory
/// locks and replication slots are database-wide. A test using one of those
/// still has to pick a name no other test can be holding - see
/// `notify_delivered_on_idle_listener`.
///
/// THE `None` ARM IS NOW UNREACHABLE. A database this crate cannot reach panics
/// here rather than announcing a skip, so the 38 callers keep their
/// `let Some(url) = require_pg().await else { return; }` and never take the
/// else. The shape is left alone deliberately: this change is about whether the
/// tests RUN, and rewriting 38 call sites would put what they ASSERT in the
/// same diff.
async fn require_pg() -> Option<String> {
    let url = test_url();
    let client = match connect(&url).await {
        Ok(client) => client,
        // `process::exit(0)` would have ended the WHOLE binary with a success
        // status the moment one test could not reach Postgres, discarding every
        // result already produced. A panic ends only this test, so its siblings
        // and any failure already reported still stand - and unlike the skip
        // that used to be here, the run goes red.
        Err(e) => common::postgres_unreachable(&url, &e),
    };

    let schema = test_schema();
    client
        .execute(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE"), &[])
        .await
        .unwrap();
    client
        .execute(&format!("CREATE SCHEMA {schema}"), &[])
        .await
        .unwrap();

    // Client dropped -> driver task exits.
    Some(schema_scoped_url(&url, &schema))
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
    let Some(url) = require_pg().await else { return };
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
    let Some(url) = require_pg().await else { return };
    let client = connect(&url).await.unwrap();
    // Verify the client is alive and usable.
    assert!(!client.is_closed());
    let rows = client.query("SELECT 1::int4", &[]).await.unwrap();
    assert_eq!(rows.len(), 1);
    // Drop closes the client — driver task exits gracefully.
    drop(client);
}

// ---------------------------------------------------------------------------
// 2. simple_query
// ---------------------------------------------------------------------------

#[compio::test]
async fn simple_query() {
    let Some(url) = require_pg().await else { return };
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
    let Some(url) = require_pg().await else { return };
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
    let Some(url) = require_pg().await else { return };
    let client = connect(&url).await.unwrap();

    // Clean up from any prior failed run
    client
        .execute("DROP TABLE IF EXISTS test_crud", &[])
        .await
        .unwrap();

    // Create
    client
        .execute(
            "CREATE TABLE test_crud (id serial PRIMARY KEY, name text NOT NULL)",
            &[],
        )
        .await
        .unwrap();

    // Insert
    let affected = client
        .execute("INSERT INTO test_crud (name) VALUES ($1)", &[&"alice"])
        .await
        .unwrap();
    assert_eq!(affected, 1);

    let affected = client
        .execute("INSERT INTO test_crud (name) VALUES ($1)", &[&"bob"])
        .await
        .unwrap();
    assert_eq!(affected, 1);

    // Select
    let rows = client
        .query("SELECT id, name FROM test_crud ORDER BY id", &[])
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].get::<_, &str>("name"), "alice");
    assert_eq!(rows[1].get::<_, &str>("name"), "bob");

    // Drop
    client
        .execute("DROP TABLE test_crud", &[])
        .await
        .unwrap();
}

// ---------------------------------------------------------------------------
// 5. transaction_commit
// ---------------------------------------------------------------------------

#[compio::test]
async fn transaction_commit() {
    let Some(url) = require_pg().await else { return };
    let mut client = connect(&url).await.unwrap();

    client
        .execute("DROP TABLE IF EXISTS test_tx_commit", &[])
        .await
        .unwrap();
    client
        .execute(
            "CREATE TABLE test_tx_commit (id serial PRIMARY KEY, val text)",
            &[],
        )
        .await
        .unwrap();

    {
        let tx = client.transaction().await.unwrap();
        tx.execute("INSERT INTO test_tx_commit (val) VALUES ($1)", &[&"one"])
            .await
            .unwrap();
        tx.execute("INSERT INTO test_tx_commit (val) VALUES ($1)", &[&"two"])
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }

    // Data should persist after commit
    let rows = client
        .query("SELECT val FROM test_tx_commit ORDER BY id", &[])
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].get::<_, &str>("val"), "one");
    assert_eq!(rows[1].get::<_, &str>("val"), "two");

    client
        .execute("DROP TABLE test_tx_commit", &[])
        .await
        .unwrap();
}

// ---------------------------------------------------------------------------
// 6. transaction_rollback_on_drop
// ---------------------------------------------------------------------------

#[compio::test]
async fn transaction_rollback_on_drop() {
    let Some(url) = require_pg().await else { return };
    let mut client = connect(&url).await.unwrap();

    client
        .execute("DROP TABLE IF EXISTS test_tx_rollback", &[])
        .await
        .unwrap();
    client
        .execute(
            "CREATE TABLE test_tx_rollback (id serial PRIMARY KEY, val text)",
            &[],
        )
        .await
        .unwrap();

    // Begin transaction, insert, then drop without commit.
    // compio-postgres's Transaction handles rollback internally via Drop.
    {
        let tx = client.transaction().await.unwrap();
        tx.execute(
            "INSERT INTO test_tx_rollback (val) VALUES ($1)",
            &[&"ghost"],
        )
        .await
        .unwrap();
        // tx dropped without commit — Drop impl enqueues ROLLBACK.
    }

    // Client should still be usable for subsequent queries — this is the
    // key observable that replaces the legacy `needs_rollback` flag.
    let rows = client
        .query("SELECT val FROM test_tx_rollback", &[])
        .await
        .unwrap();
    assert_eq!(rows.len(), 0, "expected ghost row to have been rolled back");

    client
        .execute("DROP TABLE test_tx_rollback", &[])
        .await
        .unwrap();
}

// ---------------------------------------------------------------------------
// 7. error_handling
// ---------------------------------------------------------------------------

#[compio::test]
async fn error_handling() {
    let Some(url) = require_pg().await else { return };
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

// ---------------------------------------------------------------------------
// 8. pool_basic
// ---------------------------------------------------------------------------

#[compio::test]
async fn pool_basic() {
    let Some(url) = require_pg().await else { return };
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
    let Some(url) = require_pg().await else { return };
    let pool = Pool::connect(&url, 2).await.unwrap();

    // Use the pool 5 times — should reuse connections, not create new ones each time
    for i in 0..5 {
        let val: i32 = i;
        let rows = pool
            .query("SELECT $1::int4 as v", &[&val])
            .await
            .unwrap();
        assert_eq!(rows[0].get::<_, i32>("v"), i);
    }
}

// ---------------------------------------------------------------------------
// 10. null_values
// ---------------------------------------------------------------------------

#[compio::test]
async fn null_values() {
    let Some(url) = require_pg().await else { return };
    let client = connect(&url).await.unwrap();

    let rows = client
        .query("SELECT NULL::text as val", &[])
        .await
        .unwrap();
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
    // Replace the password in the URL with a wrong one.
    // The default URL uses `zeroship@` as the password-host separator.
    let bad_url = url
        .replace(":zeroship@", ":wrong_password_xyz@")
        .replace(":test@", ":wrong_password_xyz@");

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
    let Some(url) = require_pg().await else { return };
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
    let Some(url) = require_pg().await else { return };
    let pool = Pool::connect(&url, 5).await.unwrap();

    // Acquire 5 connections, run a query on each, verify all succeed
    let mut results = Vec::new();
    for i in 0..5i32 {
        let conn = pool.get().await.unwrap();
        let rows = conn
            .query("SELECT $1::int4 as val", &[&i])
            .await
            .unwrap();
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
    let Some(url) = require_pg().await else { return };
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
    let Some(url) = require_pg().await else { return };
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
            &format!(
                "SELECT small_num, value, score, flag FROM {COMPLEX_TABLE} WHERE name = $1"
            ),
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
    let Some(url) = require_pg().await else { return };
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
    let Some(url) = require_pg().await else { return };
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
    let Some(url) = require_pg().await else { return };
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
    let Some(url) = require_pg().await else { return };
    let mut client = connect(&url).await.unwrap();

    client
        .batch_execute("CREATE TABLE savepoint_scope (n int)")
        .await
        .unwrap();

    {
        let mut tx = client.transaction().await.unwrap();
        {
            let mut outer = tx.savepoint("s").await.unwrap();
            outer
                .execute("INSERT INTO savepoint_scope VALUES (1)", &[])
                .await
                .unwrap();
            {
                let inner = outer.savepoint("s").await.unwrap();
                inner
                    .execute("INSERT INTO savepoint_scope VALUES (2)", &[])
                    .await
                    .unwrap();
                inner.rollback().await.unwrap();
            }
            outer.rollback().await.unwrap();
        }
        tx.commit().await.unwrap();
    }

    let rows = client
        .query("SELECT n FROM savepoint_scope ORDER BY n", &[])
        .await
        .unwrap();
    let kept: Vec<i32> = rows.iter().map(|r| r.get::<_, i32>(0)).collect();
    assert!(
        kept.is_empty(),
        "the outer rollback rolled back to the inner savepoint and kept {kept:?}"
    );

    client
        .batch_execute("DROP TABLE savepoint_scope")
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
    let Some(url) = require_pg().await else { return };
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
    let Some(url) = require_pg().await else { return };
    let mut client = connect(&url).await.unwrap();

    client
        .batch_execute("CREATE TABLE savepoint_recovery (n int)")
        .await
        .unwrap();

    {
        let mut tx = client.transaction().await.unwrap();
        tx.execute("INSERT INTO savepoint_recovery VALUES (1)", &[])
            .await
            .unwrap();
        {
            let sp = tx.savepoint("attempt").await.unwrap();
            sp.execute("INSERT INTO savepoint_recovery VALUES ('not an int')", &[])
                .await
                .expect_err("the statement must fail and abort the subtransaction");
            sp.rollback()
                .await
                .expect("rolling back an aborted subtransaction must recover it");
        }
        tx.execute("INSERT INTO savepoint_recovery VALUES (3)", &[])
            .await
            .expect("the enclosing transaction must be usable again");
        tx.commit().await.unwrap();
    }

    let rows = client
        .query("SELECT n FROM savepoint_recovery ORDER BY n", &[])
        .await
        .unwrap();
    let kept: Vec<i32> = rows.iter().map(|r| r.get::<_, i32>(0)).collect();
    assert_eq!(kept, vec![1, 3]);

    // A transaction that owns no savepoint still rolls back as one unit.
    {
        let tx = client.transaction().await.unwrap();
        tx.execute("INSERT INTO savepoint_recovery VALUES (4)", &[])
            .await
            .unwrap();
        tx.rollback()
            .await
            .expect("a savepoint-free transaction still rolls back");
    }

    let rows = client
        .query("SELECT n FROM savepoint_recovery ORDER BY n", &[])
        .await
        .unwrap();
    let kept: Vec<i32> = rows.iter().map(|r| r.get::<_, i32>(0)).collect();
    assert_eq!(kept, vec![1, 3]);

    client
        .batch_execute("DROP TABLE savepoint_recovery")
        .await
        .unwrap();
}

/// The control for `commit`, which the same pairing rules govern: releasing a
/// savepoint keeps its work and hands it to the enclosing transaction.
#[compio::test]
async fn a_committed_savepoint_keeps_its_work() {
    let Some(url) = require_pg().await else { return };
    let mut client = connect(&url).await.unwrap();

    client
        .batch_execute("CREATE TABLE savepoint_commit (n int)")
        .await
        .unwrap();

    {
        let mut tx = client.transaction().await.unwrap();
        {
            let sp = tx.savepoint("keep").await.unwrap();
            sp.execute("INSERT INTO savepoint_commit VALUES (1)", &[])
                .await
                .unwrap();
            sp.commit().await.unwrap();
        }
        tx.commit().await.unwrap();
    }

    let rows = client
        .query("SELECT n FROM savepoint_commit", &[])
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);

    client
        .batch_execute("DROP TABLE savepoint_commit")
        .await
        .unwrap();
}

// ---------------------------------------------------------------------------
// 19. error_recovery_in_transaction
// ---------------------------------------------------------------------------

#[compio::test]
async fn error_recovery_in_transaction() {
    let Some(url) = require_pg().await else { return };
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

    // INSERT with duplicate primary key — triggers unique_violation
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
    let Some(url) = require_pg().await else { return };
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
    let Some(url) = require_pg().await else { return };
    // max_size=2 with a very short connection_timeout, so the test does not
    // wait 30 s to observe the exhaustion error.
    //
    // `min_idle: 2`, NOT 0, and that is load-bearing. `connection_timeout`
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
    let config = compio_postgres::PoolConfig {
        max_size: 2,
        min_idle: 2,
        connection_timeout: std::time::Duration::from_millis(200),
        ..compio_postgres::PoolConfig::default()
    };
    let pool = Pool::connect_with_config(&url, config).await.unwrap();
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
    let Some(url) = require_pg().await else { return };
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
    let Some(url) = require_pg().await else { return };
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

    let Some(url) = require_pg().await else { return };
    let config = compio_postgres::PoolConfig {
        max_size: 4,
        min_idle: 0,
        // Normal timeout: cancellation is driven by dropping the future, not by
        // the timer firing, so this value is irrelevant to the race (and safely
        // long so a genuine acquire never times out).
        connection_timeout: std::time::Duration::from_secs(30),
        // Large, so acquiring the already-warm connection never does a network
        // round-trip (no validation / dirty barrier).
        validation_bypass: std::time::Duration::from_secs(60),
        ..compio_postgres::PoolConfig::default()
    };
    let pool = Pool::connect_with_config(&url, config).await.unwrap();

    // The warm-up opened exactly one connection (min_idle=0 -> warm = max(0,1)
    // = 1). Hold it so `idle` is empty and every further get() must take the
    // on-demand connect path.
    let c1 = pool.get().await.expect("warm connection acquires locally");
    assert_eq!(pool.total_count(), 1, "warm-up should open exactly one conn");

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
// between the wake and the woken waiter's re-poll pops the idle entry first —
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

    let Some(url) = require_pg().await else { return };

    // Single slot so "the freed connection" is unambiguous, and the only way a
    // fresh caller can get one is by intercepting the entry freed for waiter A.
    let config = compio_postgres::PoolConfig {
        max_size: 1,
        min_idle: 0,
        connection_timeout: std::time::Duration::from_secs(5),
        // Large so reacquiring the warm entry never does a network round-trip
        // (no validation / dirty barrier) — keeps the interleaving synchronous
        // and deterministic.
        validation_bypass: std::time::Duration::from_secs(60),
        ..compio_postgres::PoolConfig::default()
    };
    // Warm-up opens exactly one connection (min_idle=0 -> warm = max(0,1) = 1).
    let pool = Rc::new(Pool::connect_with_config(&url, config).await.unwrap());
    assert_eq!(pool.total_count(), 1, "warm-up should open exactly one conn");

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
            let c = pool.get().await.expect("waiter A must obtain the freed conn");
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
// freed connection into the front waiter's slot and wakes it — but if that
// waiter's `get()` future is DROPPED/cancelled before it polls the entry out,
// the connection must be re-homed (back to `idle`, or to the next live waiter),
// NOT lost. A leaked entry here would be a worse bug than the unfairness we are
// fixing.
//
// Accounting: the reclaim must NOT decrement `active` (it was never incremented
// for this waiter — `active += 1` happens only when a waiter actually takes the
// entry) and must NOT decrement `total` (the connection is still alive).
//
// In compio, dropping a task's `JoinHandle` (instead of `.detach()`-ing it)
// cancels the task: its future is dropped without further polling. So we drop
// A's handle AFTER the connection lands in A's slot but BEFORE A is polled to
// take it — driving exactly the cancel-with-stranded-entry path.
// ---------------------------------------------------------------------------

#[compio::test]
async fn handed_off_connection_is_reclaimed_if_waiter_is_cancelled() {
    use std::rc::Rc;

    let Some(url) = require_pg().await else { return };

    let config = compio_postgres::PoolConfig {
        max_size: 1,
        min_idle: 0,
        connection_timeout: std::time::Duration::from_secs(5),
        validation_bypass: std::time::Duration::from_secs(60),
        ..compio_postgres::PoolConfig::default()
    };
    let pool = Rc::new(Pool::connect_with_config(&url, config).await.unwrap());
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
            "reclaim never happened — the handed-off connection was LEAKED \
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
    assert_eq!(pool.active_count(), 1, "fresh caller took the reclaimed conn");
    let rows = c.query("SELECT 7::int4 AS v", &[]).await.unwrap();
    assert_eq!(rows[0].get::<_, i32>("v"), 7);
    drop(c);
    assert_eq!(pool.active_count(), 0);
    assert_eq!(pool.total_count(), 1, "no connection lost or leaked overall");
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

    let Some(url) = require_pg().await else { return };

    // Listener connection A. Register the async-message sink BEFORE spawning
    // run(), then LISTEN on a channel.
    let (client_a, mut conn_a) = compio_postgres::connect(&url, NoTls).await.unwrap();
    let mut notifications = conn_a.notifications();
    compio::runtime::spawn(async move {
        if let Err(e) = conn_a.run().await {
            eprintln!("listener connection error: {e}");
        }
    })
    .detach();

    // Unique channel name so concurrent test runs don't cross-deliver.
    let chan = format!("zs_notify_test_{}", std::process::id());
    client_a
        .batch_execute(&format!("LISTEN {chan}"))
        .await
        .unwrap();

    // Notifier connection B fires the NOTIFY. A issues NO further query after
    // its LISTEN — the notification must arrive purely from A's idle read.
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

    let Some(url) = require_pg().await else { return };
    let client = connect(&url).await.unwrap();

    client
        .execute("DROP TABLE IF EXISTS copy_deadlock_test", &[])
        .await
        .unwrap();
    client
        .execute("CREATE TABLE copy_deadlock_test (id int, n int)", &[])
        .await
        .unwrap();

    // Stream a large text-COPY body that the server rejects. The first row is
    // a PARSE error ("notanint" is not valid for `n int`); the server reports
    // it with an ErrorResponse. We then keep streaming a large volume of
    // further rows so the client is still writing long after the server has
    // produced its error and stopped draining — exactly the condition that
    // wedges the serialized loop (server's send buffer fills with the
    // ErrorResponse while the client floods; both block). PG buffers a lot of
    // COPY input before surfacing the error, so the volume must be large
    // (~hundreds of KB) to exceed the socket buffers.
    //
    // `feed` (not `send`) is used for the bulk rows: `send` force-flushes a
    // CopyData frame per call, while `feed` lets `CopyInSink` batch into ~4 KB
    // frames — without it, this is hundreds of thousands of tiny io_uring
    // writes and the test is dominated by syscall latency rather than the
    // deadlock it is meant to probe.
    let copy_fut = async {
        let sink = client
            .copy_in::<_, Bytes>("COPY copy_deadlock_test (id, n) FROM STDIN")
            .await?;
        let mut sink = pin!(sink);

        sink.feed(Bytes::from_static(b"1\tnotanint\n")).await?;
        for i in 0..200_000i64 {
            sink.feed(Bytes::from(format!("{i}\t{i}\n"))).await?;
        }
        sink.finish().await
    };

    // A deadlock manifests as the copy future never completing. Bound it
    // generously — the multiplexed loop completes well within this, while the
    // serialized loop wedges forever (RED-proven).
    let outcome = compio::time::timeout(std::time::Duration::from_secs(15), copy_fut).await;

    match outcome {
        Err(_) => panic!(
            "COPY-1: copy_in deadlocked (timed out) — the loop never read the \
             server's ErrorResponse while streaming COPY frames"
        ),
        Ok(Ok(rows)) => panic!(
            "expected the COPY parse error to surface, but the copy succeeded \
             with {rows} rows"
        ),
        Ok(Err(e)) => {
            // GREEN: the error was surfaced (read concurrently with the
            // writes) rather than deadlocking. Any server DbError proves the
            // read raced the write.
            assert!(
                e.as_db_error().is_some() || e.code().is_some(),
                "expected a server DbError from the failed COPY, got: {e}"
            );
        }
    }

    // The connection must remain usable (not wedged) after the failed copy.
    // Drop the failed client's borrow and run a fresh query on a NEW client to
    // confirm the server side recovered and the table is intact/empty.
    let verify = connect(&url).await.unwrap();
    let rows = verify
        .query("SELECT count(*)::int8 AS c FROM copy_deadlock_test", &[])
        .await
        .unwrap();
    assert_eq!(
        rows[0].get::<_, i64>("c"),
        0,
        "a rejected COPY must leave no rows committed"
    );
    verify
        .execute("DROP TABLE copy_deadlock_test", &[])
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
// (serialized) — the second query cannot begin on the server until the first
// finishes regardless. The serialized loop also already drains its queued
// requests right after the first read, so the only difference is a single
// round-trip's worth of latency (sub-millisecond on localhost). A timing
// assertion was therefore tried and rejected as inherently non-discriminating;
// this asserts the achievable, deterministic property instead: correctness of
// many concurrent in-flight requests.
//
// We drive several queries via `join_all` on the same `&Client`. Each
// `simple_query` enqueues its request synchronously on first poll (before
// awaiting its response), so all are outstanding at once — exercising the
// multiplexed loop with a full pipeline of overlapping requests.
// ---------------------------------------------------------------------------

#[compio::test]
async fn concurrent_queries_are_pipelined() {
    use futures_util::future::join_all;

    let Some(url) = require_pg().await else { return };
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
    // value — proving the multiplexed loop routed the overlapping responses to
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

    let Some(url) = require_pg().await else { return };
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
// `Ok(())` promptly — never hang.
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
// other half — a leak on a WRITE-error exit against a half-open / partitioned
// peer — cannot be forced reliably against a live PG without a custom
// man-in-the-middle socket, and is covered by code review (the teardown block
// runs on every exit path, including `?`-propagated errors).
// ---------------------------------------------------------------------------

#[compio::test]
async fn multiplexed_clean_shutdown_completes_without_hang() {
    let Some(url) = require_pg().await else { return };

    let (client, connection) = compio_postgres::connect(&url, NoTls).await.unwrap();
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
             — read task likely left parked / teardown hung",
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
    use std::task::{Context, Poll, Waker};
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

    let Some(url) = require_pg().await else { return };
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
            if finished { "completed" } else { "dropped in flight" },
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

    let Some(url) = require_pg().await else { return };
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
        (
            "prepare",
            client.prepare(bad_sql).await.unwrap_err(),
        ),
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
    Pool::connect_with_config(
        url,
        PoolConfig {
            max_size: 1,
            min_idle: 1,
            ..PoolConfig::default()
        },
    )
    .await
    .unwrap()
}

#[compio::test]
async fn released_open_transaction_is_not_inherited_by_the_next_borrower() {
    let Some(url) = require_pg().await else {
        return;
    };
    let pool = single_connection_pool(&url).await;

    {
        let client = pool.get().await.unwrap();
        client
            .batch_execute("CREATE TABLE tx_leak (id int)")
            .await
            .unwrap();
    }

    // Open a transaction with raw SQL and write inside it, then release the
    // connection without committing or rolling back.
    {
        let client = pool.get().await.unwrap();
        client.batch_execute("BEGIN").await.unwrap();
        client
            .execute("INSERT INTO tx_leak VALUES (1)", &[])
            .await
            .unwrap();
        assert_eq!(
            client.transaction_status(),
            TransactionStatus::InTransaction,
            "the session should be inside a transaction before release"
        );
    }

    let client = pool.get().await.unwrap();
    assert_eq!(
        client.transaction_status(),
        TransactionStatus::Idle,
        "the next borrower inherited an open transaction"
    );
    // The uncommitted row is visible only from inside the transaction that
    // wrote it, so seeing it proves this borrower is still in that
    // transaction.
    let rows: i64 = client
        .query_one_scalar("SELECT count(*) FROM tx_leak", &[])
        .await
        .unwrap();
    assert_eq!(rows, 0, "the next borrower saw the previous one's uncommitted row");
}

#[compio::test]
async fn released_aborted_transaction_is_not_inherited_by_the_next_borrower() {
    let Some(url) = require_pg().await else {
        return;
    };
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
        assert_eq!(
            client.transaction_status(),
            TransactionStatus::Failed,
            "the session should be in an aborted transaction before release"
        );
    }

    let client = pool.get().await.unwrap();
    let one: i32 = client.query_one_scalar("SELECT 1", &[]).await.unwrap();
    assert_eq!(one, 1, "the next borrower inherited an aborted transaction");
    assert_eq!(client.transaction_status(), TransactionStatus::Idle);
}

#[compio::test]
async fn release_rollback_keeps_session_state_the_next_borrower_may_rely_on() {
    let Some(url) = require_pg().await else {
        return;
    };
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
            .query_one_scalar("SELECT pg_try_advisory_lock(hashtext($1)::int4)", &[&schema])
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
    assert_eq!(client.transaction_status(), TransactionStatus::Idle);

    let held: i64 = client
        .query_one_scalar(
            "SELECT count(*) FROM pg_locks \
             WHERE locktype = 'advisory' AND pid = pg_backend_pid()",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(held, 1, "the release path dropped a session-scoped advisory lock");

    let app_name: String = client
        .query_one_scalar("SELECT current_setting('application_name')", &[])
        .await
        .unwrap();
    assert_eq!(app_name, "cpg_release_state", "the release path reset a session GUC");

    let bumped: i32 = client.query_one_scalar(&statement, &[&41i32]).await.unwrap();
    assert_eq!(bumped, 42, "the release path deallocated a prepared statement");
}

// ---------------------------------------------------------------------------
// TLS: a connection string that requires encryption must fail against this
// plaintext server rather than quietly connect in the clear. WHY it fails
// differs by build, and both arms are asserted below - the reason is the part
// an operator reads.
// ---------------------------------------------------------------------------

#[compio::test]
async fn sslmode_require_fails_closed_over_a_plaintext_server() {
    let Some(url) = require_pg().await else {
        return;
    };
    let sep = if url.contains('?') { '&' } else { '?' };
    let require = format!("{url}{sep}sslmode=require");

    // `NoTls` explicitly: no build of this crate lets NoTls satisfy `require`.
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
    let Some(url) = require_pg().await else {
        return;
    };
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
    let tag = "cpg_runtime_lifetime";
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
        backends.push(tagged_backends(&url, tag));
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
const FD_PROBE_TEST: &str = "a_torn_down_runtime_leaks_a_bounded_number_of_descriptors";

/// Set in the re-executed child. Its presence selects the measuring arm.
///
/// The spelling lives in the sealed key enum, not here. `clippy.toml` denies
/// `std::env::var_os`, and the one place in this crate permitted to read the
/// environment is `common::env::get`, which takes the key rather than a name -
/// so the read below cannot use a local `&str` constant, and keeping one for
/// the WRITE side alone would be the same literal in two files.
const FD_PROBE_CHILD: common::env::TestEnvKey = common::env::TestEnvKey::FdProbeChild;

/// Marks the child's machine-readable result line.
const FD_PROBE_MARKER: &str = "FD-PROBE-SERIES ";

/// The descriptor half of the invariant above, which `crate::release` does NOT
/// fix and is not trying to.
///
/// Measured 2026-08-20 against the same binary with `Socket::release_handle`
/// forced to `None`: the backend series went `[1,2,3,4,5,6]` and the fd series
/// stayed `[10,16,22,28,34,40]` - byte for byte what it is with the release in
/// place. The release ends the SESSION; the descriptor is co-owned by an
/// io_uring submission that `Runtime::drop` never reclaims (`crate::live` has
/// the mechanism), so it stays for the life of the process either way.
///
/// Three per runtime is a known, accepted cost, and this bound is what makes it
/// accepted rather than unmeasured. It multiplies: `crates/auth` runs 254 tests
/// in ONE process (`crates/auth/tests/main.rs`), so at this rate that binary
/// ends holding ~760 descriptors. That is under the 1024 soft `RLIMIT_NOFILE`
/// many CI images still ship - with no room for a second connection per test,
/// which several of them open. What it would surface as is EMFILE in a test
/// unrelated to whatever raised the cost.
///
/// The bound is three and not six because the test above measures TWO runtime
/// teardowns per iteration: `tagged_backends` builds a `Runtime` and a
/// connection of its own to ask the server its question. Its series therefore
/// reads `[6,6,6,6,6]` for the same underlying cost, which is why the number
/// this asserts is measured here rather than taken from what that one prints.
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
fn a_torn_down_runtime_leaks_a_bounded_number_of_descriptors() {
    if common::env::get(FD_PROBE_CHILD).is_some() {
        measure_and_report_fd_series();
        return;
    }

    let exe = std::env::current_exe().expect("current_exe");
    let output = std::process::Command::new(&exe)
        .args(["--exact", FD_PROBE_TEST, "--nocapture", "--test-threads=1"])
        .env(FD_PROBE_CHILD.name(), "1")
        .output()
        .expect("re-exec the test binary");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "the isolated child failed.\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );

    // Searched for anywhere in the line, not as a prefix: libtest writes
    // `test <name> ... ` without a newline and the child's first `println!`
    // lands on the end of it, so the marker is mid-line on the run that
    // matters.
    let series = stdout
        .lines()
        .find_map(|line| line.split_once(FD_PROBE_MARKER))
        .map(|(_, rest)| rest)
        .unwrap_or_else(|| {
            panic!("child printed no {FD_PROBE_MARKER} line.\n--- stdout ---\n{stdout}")
        });
    let fds: Vec<i64> = series
        .split(',')
        .map(|n| n.trim().parse().expect("fd count"))
        .collect();
    assert!(fds.len() >= 2, "need at least two samples, got {fds:?}");

    // Signed, so a count that goes DOWN is a number this reports rather than a
    // panic inside the assertion that was supposed to describe it.
    let per_runtime: Vec<i64> = fds.windows(2).map(|w| w[1] - w[0]).collect();
    println!("open fds in the isolated child: {fds:?}");
    println!("descriptors leaked per runtime: {per_runtime:?}");

    const BUDGET: i64 = 3;
    assert!(
        per_runtime.iter().all(|&d| d <= BUDGET),
        "a torn-down runtime leaks at most {BUDGET} descriptors; got {per_runtime:?} \
         from {fds:?}. Read the doc comment before raising this."
    );
}

/// The child arm of [`a_torn_down_runtime_leaks_a_bounded_number_of_descriptors`].
///
/// Opens and drops one runtime per iteration and prints the descriptor count
/// after each, on a line the parent parses. Sampling `/proc/self/fd` is only
/// meaningful here because the parent invoked this process with `--exact` and
/// `--test-threads=1`, so no other test shares it.
fn measure_and_report_fd_series() {
    let url = test_url();
    let tag = "cpg_fd_probe";
    let sep = if url.contains('?') { '&' } else { '?' };
    let tagged = format!("{url}{sep}application_name={tag}");

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
        fds.push(open_fds());
    }

    let series: Vec<String> = fds.iter().map(ToString::to_string).collect();
    println!("{FD_PROBE_MARKER}{}", series.join(","));
}

/// The one-variable partner. The test above would also pass if the connection
/// were never opened - `postgres_unreachable` guards the total failure, but a
/// connection that closes too EARLY reads identically to one that closes on
/// time. This asserts the count is 1 while the runtime is still running, so
/// the 0s above mean "released", not "never taken".
#[test]
fn a_connection_is_visible_to_the_server_while_its_runtime_runs() {
    let url = test_url();
    let tag = "cpg_runtime_lifetime_live";
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
                &[&tag],
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
    let Some(url) = require_pg().await else { return };
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
    let err = compio_postgres::connect(
        "postgres://postgres:zeroship@127.0.0.1:1/zeroship",
        NoTls,
    )
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
    let template = "cpg_template_lifetime_src";
    let clone = "cpg_template_lifetime_clone";
    // The session issuing the clone must not itself be ON the template, or it
    // would be the one other session and this would fail on its own connection.
    let admin_url = format!("{base}/postgres");
    let template_url = format!("{base}/{template}");

    let admin_sql = |stmts: Vec<String>| {
        let admin_url = admin_url.clone();
        let rt = compio::runtime::Runtime::new().expect("cannot create runtime");
        rt.block_on(async move {
            let admin = match connect(&admin_url).await {
                Ok(admin) => admin,
                Err(e) => common::postgres_unreachable(&admin_url, &e),
            };
            let mut last = Ok(0);
            for stmt in stmts {
                last = admin.execute(&stmt, &[]).await;
                if last.is_err() {
                    break;
                }
            }
            last
        })
    };

    // Fresh both ways: a leftover clone from an earlier run would make the
    // CREATE fail, and a leftover template would make it pass without this
    // test's own connection ever having been on it.
    admin_sql(vec![
        format!("DROP DATABASE IF EXISTS {clone} WITH (FORCE)"),
        format!("DROP DATABASE IF EXISTS {template} WITH (FORCE)"),
        format!("CREATE DATABASE {template}"),
    ])
    .expect("provisioning the template");

    // One test's shape: open a connection on the template, use it, end the
    // runtime.
    let rt = compio::runtime::Runtime::new().expect("cannot create runtime");
    rt.block_on(async {
        let client = connect(&template_url).await.expect("connect to the template");
        client.execute("SELECT 1", &[]).await.unwrap();
    });
    drop(rt);

    // The next test's fixture.
    let result = admin_sql(vec![format!(
        "CREATE DATABASE {clone} WITH TEMPLATE {template}"
    )]);

    // Clean up before asserting, so a failure does not also leave two databases.
    let _ = admin_sql(vec![
        format!("DROP DATABASE IF EXISTS {clone} WITH (FORCE)"),
        format!("DROP DATABASE IF EXISTS {template} WITH (FORCE)"),
    ]);

    if let Err(e) = result {
        // The crate's `Display` for a server error is the bare "db error"; the
        // SQLSTATE and the server's sentence are in the source. Without it this
        // failure cannot be told apart from a permissions problem or a typo in
        // the database name, which is the whole difference between a regression
        // test and a red light.
        let cause = std::error::Error::source(&e)
            .map(ToString::to_string)
            .unwrap_or_default();
        assert_eq!(
            e.code(),
            Some(&SqlState::OBJECT_IN_USE),
            "expected 55006 from the template clone, got: {e}: {cause}"
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
    let Some(url) = require_pg().await else { return };
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
