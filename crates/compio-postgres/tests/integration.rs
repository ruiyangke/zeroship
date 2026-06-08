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

    let url = require_pg().await;

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

    let url = require_pg().await;

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

    let url = require_pg().await;

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

    let url = require_pg().await;
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

    let url = require_pg().await;
    let client = connect(&url).await.unwrap();

    const BLOB_LEN: usize = 16 * 1024 * 1024; // ~16 MB param per query (< 64 MB cap)
    const ROWS: i32 = 3; // result ~= 48 MB, fanned over 3 frames of ~16 MB
    const N: usize = 16; // concurrent in-flight queries on one connection

    // Distinct payloads so a misrouted/corrupted response is caught, not just
    // a hang. Byte i of blob k = (i + k) as u8.
    let blobs: Vec<Vec<u8>> = (0..N)
        .map(|k| (0..BLOB_LEN).map(|i| (i + k) as u8).collect())
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
// the client closes the request channel; the driver drains, terminates,
// FINs, cancels the read task in its teardown block, and returns Ok. A 5 s
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
    let url = require_pg().await;

    let (client, connection) = compio_postgres::connect(&url, NoTls).await.unwrap();
    // Retain the handle so we can await the driver's own clean exit.
    let conn_handle = compio::runtime::spawn(async move { connection.run().await });

    // Use the connection so the multiplexed loop is fully live (responses
    // queue exercised, read task streaming).
    let rows = client.query("SELECT 42::int4 AS v", &[]).await.unwrap();
    assert_eq!(rows[0].get::<_, i32>("v"), 42);

    // Drop the client: request channel closes -> driver sends Terminate, FINs,
    // stops the read task, and `run` returns Ok(()).
    drop(client);

    // The driver must finish on its own, promptly, with a clean Ok. A hang
    // here (parked read task / wedged shutdown) trips the timeout.
    let outcome = compio::time::timeout(std::time::Duration::from_secs(5), conn_handle).await;

    match outcome {
        Ok(join_result) => {
            // Task ran to completion (not cancelled/panicked) ...
            let run_result = join_result.expect("connection task panicked or was cancelled");
            // ... and the multiplexed clean-shutdown path returned Ok.
            run_result.expect("clean shutdown should resolve Ok(())");
        }
        Err(_) => panic!(
            "multiplexed driver did not shut down within 5s after client drop \
             — read task likely left parked / teardown hung"
        ),
    }
}
