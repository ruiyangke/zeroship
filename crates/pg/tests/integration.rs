use appbase_pg::{Conn, Error, Pool};

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
