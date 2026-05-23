//! SQLite-side integration tests.
//!
//! Behind `required-features = ["sqlite", "test-helpers"]` so the
//! default-feature build never compiles this file. **P1 PR 2** adds
//! the first four behaviour tests — they exercise the
//! `SqliteSession` actor end-to-end:
//!
//! - bootstrap PRAGMAs land (`journal_mode = wal`, `busy_timeout = 5000`)
//! - `SqlExecutor::pool_exec` round-trips DDL + DML
//! - `SqlExecutor::client_exec` round-trips DDL + DML through a
//!   handle returned by `acquire_dedicated_client`
//!
//! Each test spins up a per-test `tempfile::TempDir` and constructs a
//! `SqliteBackend::new(db_dir)` directly — we deliberately bypass the
//! per-isolate context plumbing (which the PR 3+ NamespaceManager
//! work threads through) so PR 2's tests pin the actor's behaviour in
//! isolation. PR 3-5 add the higher-level orchestrator mirror.

use std::path::PathBuf;

use zeroship_plugin_db::backend::sqlite::SqliteBackend;
use zeroship_plugin_db::backend::SqlExecutor;

/// Spin up a fresh `SqliteBackend` rooted at a per-test temp dir.
///
/// Returns the backend + the `TempDir` guard — keep the guard alive
/// for the duration of the test so the dir survives until the
/// SqliteBackend's session drops (the worker thread closes the
/// connection on drop, which writes the final WAL checkpoint).
fn fresh_backend() -> (SqliteBackend, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("create tempdir");
    let backend =
        SqliteBackend::new(PathBuf::from(dir.path())).expect("open SqliteBackend");
    (backend, dir)
}

/// Drive a future to completion on a fresh compio runtime. The
/// integration target has no global runtime — each `#[test]` builds
/// its own so tests stay isolated.
fn run<F: std::future::Future>(f: F) -> F::Output {
    compio::runtime::Runtime::new()
        .expect("compio runtime build")
        .block_on(f)
}

/// Read the value of a single-column scalar PRAGMA back from the
/// session.
///
/// SQLite's `PRAGMA <name>` syntax returns a single one-column row;
/// the column name is the pragma's name (e.g. `journal_mode`,
/// `timeout`). The session's `query` helper materialises each cell
/// as `Option<String>` already, which is the right shape for PRAGMA
/// inspection at this layer. PR 4's `SchemaIntrospect` impl will
/// replace this with a proper typed surface; PR 2's tests use the
/// session directly via the `test-helpers`-gated handle accessor.
async fn pragma_value(backend: &SqliteBackend, pragma: &str) -> String {
    let client = backend
        .acquire_dedicated_client()
        .await
        .expect("acquire client");
    let sql = format!("PRAGMA {pragma}");
    let rows = client
        .query(&sql, &[])
        .await
        .expect("PRAGMA query");
    assert_eq!(rows.len(), 1, "PRAGMA {pragma} must return exactly one row");
    rows[0][0].clone().unwrap_or_default()
}

#[test]
fn pragma_journal_mode_is_wal_after_open() {
    run(async {
        let (backend, _dir) = fresh_backend();
        let mode = pragma_value(&backend, "journal_mode").await;
        // SQLite returns "wal" (lowercase) from `PRAGMA journal_mode`.
        assert_eq!(
            mode.to_ascii_lowercase(),
            "wal",
            "boot PRAGMA should have set journal_mode = WAL"
        );
    });
}

#[test]
fn pragma_busy_timeout_set() {
    run(async {
        let (backend, _dir) = fresh_backend();
        let timeout = pragma_value(&backend, "busy_timeout").await;
        assert_eq!(
            timeout, "5000",
            "boot PRAGMA should have set busy_timeout = 5000"
        );
    });
}

#[test]
fn pool_exec_round_trip() {
    run(async {
        let (backend, _dir) = fresh_backend();
        // DDL — execute returns 0 rows affected for CREATE TABLE.
        backend
            .pool_exec("CREATE TABLE t (x INTEGER)", &[])
            .await
            .expect("CREATE TABLE");
        // DML — INSERT one row, expect affected = 1.
        let n = backend
            .pool_exec("INSERT INTO t VALUES (1)", &[])
            .await
            .expect("INSERT");
        assert_eq!(n, 1, "INSERT INTO t VALUES (1) should affect 1 row");
    });
}

#[test]
fn client_exec_round_trip() {
    run(async {
        let (backend, _dir) = fresh_backend();
        let client = backend
            .acquire_dedicated_client()
            .await
            .expect("acquire_dedicated_client");
        // DDL via the handle — both paths route through the same
        // actor, so DDL on the client must be visible to subsequent
        // pool_exec calls (and vice-versa).
        backend
            .client_exec(&client, "CREATE TABLE t2 (y INTEGER)", &[])
            .await
            .expect("CREATE TABLE via client");
        let n = backend
            .client_exec(&client, "INSERT INTO t2 VALUES (42)", &[])
            .await
            .expect("INSERT via client");
        assert_eq!(n, 1, "INSERT via client should affect 1 row");

        // Cross-check: pool_exec on the same backend sees the same
        // table (single-writer actor — there is no isolation
        // between client and pool surfaces).
        let n2 = backend
            .pool_exec("INSERT INTO t2 VALUES (43)", &[])
            .await
            .expect("INSERT via pool sees client-DDL'd table");
        assert_eq!(n2, 1);
    });
}
