//! Integration tests for compio-postgres.
//!
//! Ported from zeroship-pg's integration suite. Each `#[compio::test]` opens
//! a fresh connection (via the `connect` helper), spawns the connection
//! driver onto compio's runtime, and exercises one slice of the API.
//!
//! Run with:
//!   docker compose up -d postgres
//!   PG_TEST_URL='postgres://postgres:zeroship@localhost:5440/zeroship' \
//!       cargo test -p compio-postgres --test integration -- --test-threads=1

use compio_postgres::error::SqlState;
use compio_postgres::{Client, Error, NoTls, Pool};

fn test_url() -> String {
    std::env::var("PG_TEST_URL")
        .unwrap_or_else(|_| "postgres://postgres:zeroship@localhost:5440/zeroship".to_string())
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

async fn require_pg() -> String {
    let url = test_url();
    match connect(&url).await {
        Ok(_client) => url, // Client dropped -> driver task exits
        Err(e) => {
            eprintln!("Skipping — Postgres not reachable: {e}");
            std::process::exit(0);
        }
    }
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
    // Drop closes the client — driver task exits gracefully.
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
    let url = require_pg().await;
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
    let url = require_pg().await;
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
    let url = require_pg().await;
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
    let float_s: f32 = 3.14;
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
    // Custom config: max_size=2, very short connection_timeout so the test
    // doesn't wait 30 s for the exhaustion error.
    let config = compio_postgres::PoolConfig {
        max_size: 2,
        min_idle: 0,
        connection_timeout: std::time::Duration::from_millis(200),
        ..compio_postgres::PoolConfig::default()
    };
    let pool = Pool::connect_with_config(&url, config).await.unwrap();

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
// This drives many on-demand connects with a 1 ms connection_timeout (far
// shorter than a real TCP+startup handshake), so each one is cancelled
// mid-flight. Pre-fix: leaked permits inflate `total_count()` up to max_size.
// Post-fix: every cancelled connect releases its permit via RAII Drop, so only
// the genuinely-held warm connection remains counted.
// ---------------------------------------------------------------------------

#[compio::test]
async fn get_cancellation_during_connect_does_not_leak_permits() {
    let url = require_pg().await;
    let config = compio_postgres::PoolConfig {
        max_size: 4,
        min_idle: 0,
        // Shorter than a real TCP+startup connect, so on-demand connects get
        // cancelled mid-flight by the pool's own timeout.
        connection_timeout: std::time::Duration::from_millis(1),
        // Large, so acquiring the already-warm connection never does a network
        // round-trip (no validation / dirty barrier) and completes well under
        // the 1 ms budget.
        validation_bypass: std::time::Duration::from_secs(60),
        ..compio_postgres::PoolConfig::default()
    };
    let pool = Pool::connect_with_config(&url, config).await.unwrap();

    // The warm-up opened exactly one connection (min_idle=0 -> warm = max(0,1)
    // = 1). Hold it so `idle` is empty and every further get() must take the
    // on-demand connect path.
    let c1 = pool.get().await.expect("warm connection acquires locally");
    assert_eq!(pool.total_count(), 1, "warm-up should open exactly one conn");

    // Drive many cancelled on-demand connects. Each returns Err(timeout) after
    // being dropped mid-`connect_one().await`.
    for _ in 0..100 {
        let r = pool.get().await;
        assert!(
            r.is_err(),
            "1 ms on-demand connect should time out, not succeed"
        );
    }

    // Primary invariant: `total_count()` must not exceed the genuinely-held
    // connections (just c1). Pre-fix this climbs to max_size (4) and sticks.
    assert_eq!(
        pool.total_count(),
        1,
        "cancelled on-demand connects leaked permits: total_count()={}",
        pool.total_count()
    );

    // Liveness: the pool must not be bricked. Returning c1 makes a warm idle
    // entry available; a fresh get() must reclaim it locally (no network
    // round-trip, so it fits the 1 ms budget) and succeed.
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
