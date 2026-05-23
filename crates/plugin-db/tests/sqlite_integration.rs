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
use zeroship_plugin_db::backend::{NamespaceManager, SqlExecutor};

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

// ---------------------------------------------------------------------------
// P1 PR 3 — NamespaceManager (ATTACH) integration tests.
//
// Each test exercises a behaviour the SqliteBackend's
// `ensure_app_schema` impl is responsible for:
//
// 1. The ATTACH lands and the per-app file exists on disk; the alias
//    is queryable via the SQLite catalog.
// 2. A second `ensure_app_schema` call for the same app_id is a no-op
//    (idempotent guard via `app_id_cache` — without it, SQLite errors
//    on the duplicate ATTACH).
// 3. Two attached app aliases are visible as separate namespaces — a
//    table created in `app_a` is NOT visible from `app_b`, which is
//    the per-app isolation property `ensure_app_schema` exists to
//    establish.
// ---------------------------------------------------------------------------

#[test]
fn ensure_app_schema_attaches_file() {
    run(async {
        let (backend, dir) = fresh_backend();
        backend
            .ensure_app_schema("app_demo")
            .await
            .expect("ensure_app_schema");

        // The per-app file should now exist on disk.
        let expected = dir.path().join("zs-app_demo.sqlite");
        assert!(
            expected.exists(),
            "per-app sqlite file should exist: {expected:?}"
        );

        // The alias should be queryable. `SELECT name FROM
        // "app_demo".sqlite_master` returns the (empty) catalog of
        // the freshly-attached database — the SELECT itself
        // succeeding is the assertion (a missing alias surfaces as
        // `no such database: app_demo`).
        let client = backend
            .acquire_dedicated_client()
            .await
            .expect("acquire client");
        let rows = client
            .query("SELECT name FROM \"app_demo\".sqlite_master", &[])
            .await
            .expect("query attached sqlite_master");
        // Freshly attached database has no user tables yet.
        assert!(
            rows.is_empty(),
            "freshly attached db should have no sqlite_master rows; got {rows:?}"
        );
    });
}

#[test]
fn ensure_app_schema_idempotent() {
    run(async {
        let (backend, _dir) = fresh_backend();
        // First call attaches.
        backend
            .ensure_app_schema("app_demo")
            .await
            .expect("first ensure_app_schema");
        // Second call must NOT surface "database app_demo is already
        // in use" — the cache (or the error-suppression fallback)
        // should short-circuit it to Ok.
        backend
            .ensure_app_schema("app_demo")
            .await
            .expect("second ensure_app_schema must be idempotent");
    });
}

#[test]
fn ensure_app_schema_isolates_per_app() {
    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .ensure_app_schema("app_a")
            .await
            .expect("attach app_a");
        backend
            .ensure_app_schema("app_b")
            .await
            .expect("attach app_b");

        // Create a table inside the `app_a` namespace.
        backend
            .pool_exec("CREATE TABLE \"app_a\".\"t\" (x INTEGER)", &[])
            .await
            .expect("CREATE TABLE in app_a");

        // The table must be visible in `app_a`'s catalog.
        let client = backend
            .acquire_dedicated_client()
            .await
            .expect("acquire client");
        let rows_a = client
            .query(
                "SELECT name FROM \"app_a\".sqlite_master WHERE type = 'table'",
                &[],
            )
            .await
            .expect("query app_a sqlite_master");
        assert_eq!(rows_a.len(), 1, "app_a should see exactly one table");
        assert_eq!(rows_a[0][0].as_deref(), Some("t"));

        // The table must NOT be visible in `app_b`'s catalog —
        // per-file isolation is the entire point of the ATTACH
        // layout. Each app's `sqlite_master` is its own namespace.
        let rows_b = client
            .query(
                "SELECT name FROM \"app_b\".sqlite_master WHERE type = 'table'",
                &[],
            )
            .await
            .expect("query app_b sqlite_master");
        assert!(
            rows_b.is_empty(),
            "app_b must not see app_a's tables; got {rows_b:?}"
        );
    });
}
