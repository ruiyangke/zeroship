use zeroship_pg::{Conn, Error, Pool};

fn test_url() -> String {
    std::env::var("PG_TEST_URL")
        .unwrap_or_else(|_| "postgres://postgres:test@localhost:5434/postgres".to_string())
}

async fn require_pg() -> String {
    let url = test_url();
    match Conn::connect(&url).await {
        Ok(conn) => {
            let _ = conn.close().await;
            url
        }
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
    let conn = Conn::connect(&url).await.unwrap();
    assert_eq!(conn.status(), b'I');
    conn.close().await.unwrap();
}

// ---------------------------------------------------------------------------
// 2. simple_query
// ---------------------------------------------------------------------------

#[compio::test]
async fn simple_query() {
    let url = require_pg().await;
    let mut conn = Conn::connect(&url).await.unwrap();

    let rows = conn.query("SELECT 1 as num, 'hello' as greeting", &[]).await.unwrap();
    assert_eq!(rows.len(), 1);

    let row = &rows[0];
    let num: i32 = row.get("num");
    let greeting: &str = row.get("greeting");
    assert_eq!(num, 1);
    assert_eq!(greeting, "hello");

    conn.close().await.unwrap();
}

// ---------------------------------------------------------------------------
// 3. parameterized_query
// ---------------------------------------------------------------------------

#[compio::test]
async fn parameterized_query() {
    let url = require_pg().await;
    let mut conn = Conn::connect(&url).await.unwrap();

    let val: i32 = 42;
    let rows = conn.query("SELECT $1::int4 as val", &[&val]).await.unwrap();
    assert_eq!(rows.len(), 1);

    let result: i32 = rows[0].get("val");
    assert_eq!(result, 42);

    conn.close().await.unwrap();
}

// ---------------------------------------------------------------------------
// 4. create_table_insert_select_drop
// ---------------------------------------------------------------------------

#[compio::test]
async fn create_table_insert_select_drop() {
    let url = require_pg().await;
    let mut conn = Conn::connect(&url).await.unwrap();

    // Clean up from any prior failed run
    conn.execute("DROP TABLE IF EXISTS test_crud", &[]).await.unwrap();

    // Create
    conn.execute("CREATE TABLE test_crud (id serial PRIMARY KEY, name text NOT NULL)", &[])
        .await
        .unwrap();

    // Insert
    let affected = conn
        .execute("INSERT INTO test_crud (name) VALUES ($1)", &[&"alice"])
        .await
        .unwrap();
    assert_eq!(affected, 1);

    let affected = conn
        .execute("INSERT INTO test_crud (name) VALUES ($1)", &[&"bob"])
        .await
        .unwrap();
    assert_eq!(affected, 1);

    // Select
    let rows = conn.query("SELECT id, name FROM test_crud ORDER BY id", &[]).await.unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].get::<&str>("name"), "alice");
    assert_eq!(rows[1].get::<&str>("name"), "bob");

    // Drop
    conn.execute("DROP TABLE test_crud", &[]).await.unwrap();
    conn.close().await.unwrap();
}

// ---------------------------------------------------------------------------
// 5. transaction_commit
// ---------------------------------------------------------------------------

#[compio::test]
async fn transaction_commit() {
    let url = require_pg().await;
    let mut conn = Conn::connect(&url).await.unwrap();

    conn.execute("DROP TABLE IF EXISTS test_tx_commit", &[]).await.unwrap();
    conn.execute("CREATE TABLE test_tx_commit (id serial PRIMARY KEY, val text)", &[])
        .await
        .unwrap();

    {
        let mut tx = conn.begin().await.unwrap();
        tx.execute("INSERT INTO test_tx_commit (val) VALUES ($1)", &[&"one"])
            .await
            .unwrap();
        tx.execute("INSERT INTO test_tx_commit (val) VALUES ($1)", &[&"two"])
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }

    // Data should persist after commit
    let rows = conn
        .query("SELECT val FROM test_tx_commit ORDER BY id", &[])
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].get::<&str>("val"), "one");
    assert_eq!(rows[1].get::<&str>("val"), "two");

    conn.execute("DROP TABLE test_tx_commit", &[]).await.unwrap();
    conn.close().await.unwrap();
}

// ---------------------------------------------------------------------------
// 6. transaction_rollback_on_drop
// ---------------------------------------------------------------------------

#[compio::test]
async fn transaction_rollback_on_drop() {
    let url = require_pg().await;
    let mut conn = Conn::connect(&url).await.unwrap();

    conn.execute("DROP TABLE IF EXISTS test_tx_rollback", &[]).await.unwrap();
    conn.execute("CREATE TABLE test_tx_rollback (id serial PRIMARY KEY, val text)", &[])
        .await
        .unwrap();

    // Begin transaction, insert, then drop without commit
    {
        let mut tx = conn.begin().await.unwrap();
        tx.execute("INSERT INTO test_tx_rollback (val) VALUES ($1)", &[&"ghost"])
            .await
            .unwrap();
        // Drop tx without commit — should set needs_rollback
    }

    // Connection should have needs_rollback set
    assert!(conn.needs_rollback);

    // Manually send ROLLBACK to restore the connection to a usable state
    conn.execute("ROLLBACK", &[]).await.unwrap();
    conn.needs_rollback = false;

    // Data should NOT persist
    let rows = conn
        .query("SELECT val FROM test_tx_rollback", &[])
        .await
        .unwrap();
    assert_eq!(rows.len(), 0);

    conn.execute("DROP TABLE test_tx_rollback", &[]).await.unwrap();
    conn.close().await.unwrap();
}

// ---------------------------------------------------------------------------
// 7. error_handling
// ---------------------------------------------------------------------------

#[compio::test]
async fn error_handling() {
    let url = require_pg().await;
    let mut conn = Conn::connect(&url).await.unwrap();

    // Query a nonexistent table
    let err = conn
        .query("SELECT * FROM nonexistent_table_xyz", &[])
        .await
        .unwrap_err();

    match &err {
        Error::Postgres { code, .. } => {
            assert_eq!(code, "42P01", "expected 'undefined_table' SQLSTATE, got {code}");
        }
        other => panic!("expected Error::Postgres, got: {other}"),
    }

    // Connection should still be usable after error
    let rows = conn.query("SELECT 1 as ok", &[]).await.unwrap();
    assert_eq!(rows[0].get::<i32>("ok"), 1);

    conn.close().await.unwrap();
}

// ---------------------------------------------------------------------------
// 8. pool_basic
// ---------------------------------------------------------------------------

#[compio::test]
async fn pool_basic() {
    let url = require_pg().await;
    let pool = Pool::connect(&url, 4).await.unwrap();

    let rows = pool.query("SELECT 42 as answer", &[]).await.unwrap();
    assert_eq!(rows[0].get::<i32>("answer"), 42);

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
        let rows = pool.query("SELECT $1::int4 as v", &[&val]).await.unwrap();
        assert_eq!(rows[0].get::<i32>("v"), i);
    }

    // If pool had max_size=2 and we used it sequentially 5 times, it should
    // still work (reusing the one eager connection).
}

// ---------------------------------------------------------------------------
// 10. null_values
// ---------------------------------------------------------------------------

#[compio::test]
async fn null_values() {
    let url = require_pg().await;
    let mut conn = Conn::connect(&url).await.unwrap();

    let rows = conn.query("SELECT NULL::text as val", &[]).await.unwrap();
    assert_eq!(rows.len(), 1);

    let val: Option<&str> = rows[0].get("val");
    assert!(val.is_none(), "expected None for NULL::text, got {val:?}");

    conn.close().await.unwrap();
}

// ---------------------------------------------------------------------------
// 11. wrong_password
// ---------------------------------------------------------------------------

#[compio::test]
async fn wrong_password() {
    let url = test_url();
    // Replace the password in the URL with a wrong one
    let bad_url = url.replace("test@", "wrong_password_xyz@");

    let err = match Conn::connect(&bad_url).await {
        Err(e) => e,
        Ok(_) => panic!("expected connection to fail with wrong password"),
    };
    match &err {
        Error::Auth(_) | Error::Postgres { .. } => {
            // Both are acceptable — depends on how the server responds
        }
        other => panic!("expected Auth or Postgres error, got: {other}"),
    }
}

// ---------------------------------------------------------------------------
// Helper: create the complex test table
// ---------------------------------------------------------------------------

const COMPLEX_TABLE: &str = "pg_complex_test";

async fn create_complex_table(conn: &mut Conn) {
    conn.execute(&format!("DROP TABLE IF EXISTS {COMPLEX_TABLE}"), &[])
        .await
        .unwrap();
    conn.execute(
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

async fn drop_complex_table(conn: &mut Conn) {
    conn.execute(&format!("DROP TABLE IF EXISTS {COMPLEX_TABLE}"), &[])
        .await
        .unwrap();
}

// ---------------------------------------------------------------------------
// 12. large_result_set
// ---------------------------------------------------------------------------

#[compio::test]
async fn large_result_set() {
    let url = require_pg().await;
    let mut conn = Conn::connect(&url).await.unwrap();

    create_complex_table(&mut conn).await;

    // INSERT 1000 rows
    for i in 0..1000i64 {
        conn.execute(
            &format!("INSERT INTO {COMPLEX_TABLE} (name, value) VALUES ($1, $2)"),
            &[&format!("row_{i}"), &i],
        )
        .await
        .unwrap();
    }

    // SELECT all
    let rows = conn
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

    drop_complex_table(&mut conn).await;
    conn.close().await.unwrap();
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
        let mut conn = pool.get().await.unwrap();
        let rows = conn.query("SELECT $1::int4 as val", &[&i]).await.unwrap();
        results.push(rows[0].get::<i32>("val"));
    }

    assert_eq!(results, vec![0, 1, 2, 3, 4]);
}

// ---------------------------------------------------------------------------
// 14. text_types
// ---------------------------------------------------------------------------

#[compio::test]
async fn text_types() {
    let url = require_pg().await;
    let mut conn = Conn::connect(&url).await.unwrap();

    create_complex_table(&mut conn).await;

    // Empty string
    conn.execute(
        &format!("INSERT INTO {COMPLEX_TABLE} (name) VALUES ($1)"),
        &[&""],
    )
    .await
    .unwrap();

    // Unicode: emoji + CJK
    let unicode_str = "Hello 🌍🎉 你好世界 こんにちは";
    conn.execute(
        &format!("INSERT INTO {COMPLEX_TABLE} (name) VALUES ($1)"),
        &[&unicode_str],
    )
    .await
    .unwrap();

    // Very long string (10KB)
    let long_str = "A".repeat(10 * 1024);
    conn.execute(
        &format!("INSERT INTO {COMPLEX_TABLE} (name) VALUES ($1)"),
        &[&long_str.as_str()],
    )
    .await
    .unwrap();

    // Special chars: quotes, backslashes, newlines
    let special_str = "it's a \"test\"\\with\nnewlines\tand\ttabs";
    conn.execute(
        &format!("INSERT INTO {COMPLEX_TABLE} (name) VALUES ($1)"),
        &[&special_str],
    )
    .await
    .unwrap();

    // SELECT all back and verify
    let rows = conn
        .query(
            &format!("SELECT name FROM {COMPLEX_TABLE} ORDER BY id"),
            &[],
        )
        .await
        .unwrap();

    assert_eq!(rows.len(), 4);
    assert_eq!(rows[0].get::<&str>("name"), "");
    assert_eq!(rows[1].get::<&str>("name"), unicode_str);
    assert_eq!(rows[2].get::<&str>("name"), long_str.as_str());
    assert_eq!(rows[3].get::<&str>("name"), special_str);

    drop_complex_table(&mut conn).await;
    conn.close().await.unwrap();
}

// ---------------------------------------------------------------------------
// 15. numeric_types
// ---------------------------------------------------------------------------

#[compio::test]
async fn numeric_types() {
    let url = require_pg().await;
    let mut conn = Conn::connect(&url).await.unwrap();

    create_complex_table(&mut conn).await;

    let small: i16 = -123;
    let medium: i32 = 42_000;
    let large: i64 = 9_000_000_000i64;
    let float_s: f32 = 3.14;
    let float_d: f64 = 2.718281828459045;
    let flag: bool = true;

    conn.execute(
        &format!(
            "INSERT INTO {COMPLEX_TABLE} (name, small_num, value, score, flag) \
             VALUES ($1, $2, $3, $4, $5)"
        ),
        &[&"numeric_test", &small, &large, &float_d, &flag],
    )
    .await
    .unwrap();

    let rows = conn
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
    assert_eq!(row.get::<i16>("small_num"), small);
    assert_eq!(row.get::<i64>("value"), large);
    assert_eq!(row.get::<f64>("score"), float_d);
    assert_eq!(row.get::<bool>("flag"), flag);

    // Test i32 and f32 via direct SELECT with casts
    let rows = conn
        .query("SELECT $1::int4 as i, $2::float4 as f", &[&medium, &float_s])
        .await
        .unwrap();
    assert_eq!(rows[0].get::<i32>("i"), medium);
    assert_eq!(rows[0].get::<f32>("f"), float_s);

    drop_complex_table(&mut conn).await;
    conn.close().await.unwrap();
}

// ---------------------------------------------------------------------------
// 16. binary_data
// ---------------------------------------------------------------------------

#[compio::test]
async fn binary_data() {
    let url = require_pg().await;
    let mut conn = Conn::connect(&url).await.unwrap();

    create_complex_table(&mut conn).await;

    // Binary data with null bytes, 0xFF, and various byte patterns
    let binary: Vec<u8> = (0..=255).collect();
    // Also test a chunk with embedded nulls
    let mut with_nulls = vec![0u8, 1, 0, 0, 255, 254, 0, 128];
    with_nulls.extend_from_slice(&binary);

    conn.execute(
        &format!("INSERT INTO {COMPLEX_TABLE} (name, data) VALUES ($1, $2)"),
        &[&"binary_test", &with_nulls.as_slice()],
    )
    .await
    .unwrap();

    let rows = conn
        .query(
            &format!("SELECT data FROM {COMPLEX_TABLE} WHERE name = $1"),
            &[&"binary_test"],
        )
        .await
        .unwrap();

    assert_eq!(rows.len(), 1);
    let data: &[u8] = rows[0].get("data");
    assert_eq!(data, with_nulls.as_slice());

    drop_complex_table(&mut conn).await;
    conn.close().await.unwrap();
}

// ---------------------------------------------------------------------------
// 17. multiple_statements_sequential
// ---------------------------------------------------------------------------

#[compio::test]
async fn multiple_statements_sequential() {
    let url = require_pg().await;
    let mut conn = Conn::connect(&url).await.unwrap();

    create_complex_table(&mut conn).await;

    // Run 20 different queries on the same connection
    for i in 0..20i64 {
        conn.execute(
            &format!("INSERT INTO {COMPLEX_TABLE} (name, value) VALUES ($1, $2)"),
            &[&format!("seq_{i}"), &i],
        )
        .await
        .unwrap();
    }

    // Also do 20 SELECT queries
    for i in 0..20i64 {
        let rows = conn
            .query(
                &format!("SELECT value FROM {COMPLEX_TABLE} WHERE name = $1"),
                &[&format!("seq_{i}")],
            )
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get::<i64>("value"), i);
    }

    // Verify connection still healthy
    assert_eq!(conn.status(), b'I');

    drop_complex_table(&mut conn).await;
    conn.close().await.unwrap();
}

// ---------------------------------------------------------------------------
// 18. transaction_rollback_explicit
// ---------------------------------------------------------------------------

#[compio::test]
async fn transaction_rollback_explicit() {
    let url = require_pg().await;
    let mut conn = Conn::connect(&url).await.unwrap();

    create_complex_table(&mut conn).await;

    // BEGIN, INSERT, explicit ROLLBACK
    {
        let mut tx = conn.begin().await.unwrap();
        tx.execute(
            &format!("INSERT INTO {COMPLEX_TABLE} (name) VALUES ($1)"),
            &[&"should_not_exist"],
        )
        .await
        .unwrap();
        tx.rollback().await.unwrap();
    }

    // Verify data not present
    let rows = conn
        .query(
            &format!("SELECT name FROM {COMPLEX_TABLE} WHERE name = $1"),
            &[&"should_not_exist"],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 0);

    // Connection should be idle
    assert_eq!(conn.status(), b'I');

    drop_complex_table(&mut conn).await;
    conn.close().await.unwrap();
}

// ---------------------------------------------------------------------------
// 19. error_recovery_in_transaction
// ---------------------------------------------------------------------------

#[compio::test]
async fn error_recovery_in_transaction() {
    let url = require_pg().await;
    let mut conn = Conn::connect(&url).await.unwrap();

    create_complex_table(&mut conn).await;

    // BEGIN
    conn.execute("BEGIN", &[]).await.unwrap();
    assert_eq!(conn.status(), b'T');

    // INSERT (succeeds)
    conn.execute(
        &format!("INSERT INTO {COMPLEX_TABLE} (name, value) VALUES ($1, $2)"),
        &[&"good_row", &1i64],
    )
    .await
    .unwrap();

    // INSERT with constraint violation: name is NOT NULL, so pass a duplicate
    // primary key to cause a unique violation
    let err = conn
        .execute(
            &format!("INSERT INTO {COMPLEX_TABLE} (id, name) VALUES (1, $1)"),
            &[&"dup_id"],
        )
        .await;

    // The insert should fail (duplicate key or we can also trigger NOT NULL)
    assert!(err.is_err(), "expected constraint violation");

    // Transaction should be in error state
    assert_eq!(conn.status(), b'E', "expected transaction error state");

    // ROLLBACK to recover
    conn.execute("ROLLBACK", &[]).await.unwrap();
    assert_eq!(conn.status(), b'I');

    // Verify connection is usable again
    let rows = conn.query("SELECT 1 as ok", &[]).await.unwrap();
    assert_eq!(rows[0].get::<i32>("ok"), 1);

    // Verify the good_row was rolled back too
    let rows = conn
        .query(
            &format!("SELECT name FROM {COMPLEX_TABLE} WHERE name = $1"),
            &[&"good_row"],
        )
        .await
        .unwrap();
    assert_eq!(rows.len(), 0);

    drop_complex_table(&mut conn).await;
    conn.close().await.unwrap();
}

// ---------------------------------------------------------------------------
// 20. null_in_params
// ---------------------------------------------------------------------------

#[compio::test]
async fn null_in_params() {
    let url = require_pg().await;
    let mut conn = Conn::connect(&url).await.unwrap();

    let rows = conn
        .query("SELECT $1::text as val", &[&None::<&str>])
        .await
        .unwrap();

    assert_eq!(rows.len(), 1);
    let val: Option<&str> = rows[0].get("val");
    assert!(val.is_none(), "expected NULL, got {val:?}");

    conn.close().await.unwrap();
}

// ---------------------------------------------------------------------------
// 21. pool_exhaustion
// ---------------------------------------------------------------------------

#[compio::test]
async fn pool_exhaustion() {
    let url = require_pg().await;
    let pool = Pool::connect(&url, 2).await.unwrap();

    // Acquire 2 connections without returning them
    let _c1 = pool.get().await.unwrap();
    let _c2 = pool.get().await.unwrap();

    // Third acquisition should fail
    let err = pool.get().await;
    match err {
        Err(Error::Pool(_)) => { /* expected */ }
        Err(other) => panic!("expected Error::Pool, got: {other}"),
        Ok(_) => panic!("expected pool exhaustion error, but got a connection"),
    }
}

// ---------------------------------------------------------------------------
// 22. returning_clause
// ---------------------------------------------------------------------------

#[compio::test]
async fn returning_clause() {
    let url = require_pg().await;
    let mut conn = Conn::connect(&url).await.unwrap();

    create_complex_table(&mut conn).await;

    // INSERT with RETURNING
    let rows = conn
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

    drop_complex_table(&mut conn).await;
    conn.close().await.unwrap();
}

// ---------------------------------------------------------------------------
// 23. update_with_returning
// ---------------------------------------------------------------------------

#[compio::test]
async fn update_with_returning() {
    let url = require_pg().await;
    let mut conn = Conn::connect(&url).await.unwrap();

    create_complex_table(&mut conn).await;

    // Insert a row with value=10
    conn.execute(
        &format!("INSERT INTO {COMPLEX_TABLE} (name, value) VALUES ($1, $2)"),
        &[&"upd_test", &10i64],
    )
    .await
    .unwrap();

    // UPDATE with RETURNING
    let rows = conn
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

    drop_complex_table(&mut conn).await;
    conn.close().await.unwrap();
}
