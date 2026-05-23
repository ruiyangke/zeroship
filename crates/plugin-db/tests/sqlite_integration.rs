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
use zeroship_plugin_db::backend::{
    LockManager, LockScope, NamespaceManager, SchemaIntrospect, SqlExecutor,
};
use zeroship_plugin_db::error::DbError;

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

// ---------------------------------------------------------------------------
// P1 PR 4 — LockManager (in-process registry) + SchemaIntrospect
// (PRAGMA-walk) integration tests.
//
// The lock-side tests exercise the three legacy primitive routes
// (`acquire_advisory_lock` / `try_acquire_advisory_lock` /
// `release_advisory_lock`) and the typed `try_acquire_with_backoff`
// surface, end-to-end against the same `InProcessLockRegistry` the
// SqliteBackend owns. The introspect-side tests exercise the
// PRAGMA-walk catalog inspection: empty schema → empty LiveSchema, and
// CREATE TABLE round-trip → declared columns appear with correct types
// + nullability.
// ---------------------------------------------------------------------------

#[test]
fn lock_try_acquire_blocks_second() {
    run(async {
        let (backend, _dir) = fresh_backend();
        let client = backend
            .acquire_dedicated_client()
            .await
            .expect("acquire client");
        // First acquire on a fresh registry must succeed — the legacy
        // primitive returns Ok(()) per the `acquire_advisory_lock`
        // contract.
        backend
            .acquire_advisory_lock(&client, "key1", "key2")
            .await
            .expect("first acquire_advisory_lock");

        // A try_acquire with the same `(key1, key2)` must observe the
        // slot as held — `Ok(false)` is the contended return. We use
        // a fresh handle (from `acquire_dedicated_client`) to mirror
        // the "different client, same keys" intent of the plan spec;
        // SqliteBackend ignores the client argument by design (§7.2)
        // but the call shape stays faithful to the PG side.
        let other_client = backend
            .acquire_dedicated_client()
            .await
            .expect("acquire other client");
        let got = backend
            .try_acquire_advisory_lock(&other_client, "key1", "key2")
            .await
            .expect("try_acquire_advisory_lock");
        assert!(
            !got,
            "second try_acquire on a held slot must return Ok(false) (got = {got})"
        );
    });
}

#[test]
fn lock_release_unblocks() {
    run(async {
        let (backend, _dir) = fresh_backend();
        let client = backend
            .acquire_dedicated_client()
            .await
            .expect("acquire client");
        backend
            .acquire_advisory_lock(&client, "key1", "key2")
            .await
            .expect("first acquire");
        backend
            .release_advisory_lock(&client, "key1", "key2")
            .await
            .expect("release");
        // After release the slot is free — try_acquire flips it back
        // to held and returns Ok(true).
        let got = backend
            .try_acquire_advisory_lock(&client, "key1", "key2")
            .await
            .expect("try_acquire after release");
        assert!(
            got,
            "try_acquire after release must return Ok(true) (got = {got})"
        );
    });
}

#[test]
fn lock_acquire_with_backoff_exhausts_into_contention_error() {
    // Per plan §2.4 + spec: hold a slot, then call the typed
    // `try_acquire_with_backoff` against the same scope. The
    // five-attempt schedule (0/50/200/500/1000ms = ~1.75s) must
    // exhaust and surface `DbError::LockContention`, which `to_op_error`
    // maps to the wire-code `lock_not_available`.
    //
    // We use `LockScope::GlobalApp` (the only production-shaped
    // variant) so the `to_keys` derivation matches what
    // register-model bootstrap would emit; `LockScope::LocalApp`
    // would derive identical keys (the visibility class is a
    // backend-arm classification, not a key-shape one — see the
    // `LockScope::to_keys` rustdoc).
    run(async {
        let (backend, _dir) = fresh_backend();
        let client = backend
            .acquire_dedicated_client()
            .await
            .expect("acquire client");
        let scope = LockScope::GlobalApp {
            app_id: "app_demo".to_string(),
            name: "register_model".to_string(),
        };

        // Hold the slot via the underlying primitive — the typed
        // `try_acquire_with_backoff` will then loop against a
        // permanently-held registry slot.
        let (k1, k2) = scope.to_keys();
        let got = backend
            .try_acquire_advisory_lock(&client, &k1, &k2)
            .await
            .expect("hold slot via try_acquire_advisory_lock");
        assert!(got, "initial hold must succeed");

        // Now exercise the typed surface — it loops 5 times on
        // try_acquire_advisory_lock (which observes the held slot →
        // Ok(false)), then surfaces LockContention. The whole call
        // takes ~1.75s in the worst case; the test budget is fine
        // with that.
        let err = backend
            .try_acquire_with_backoff(&client, &scope)
            .await
            .expect_err("backoff loop must exhaust into contention error");
        match err {
            DbError::LockContention { message } => {
                assert!(
                    message.contains("app_demo"),
                    "contention message should mention the scope's app_id: {message}"
                );
                assert!(
                    message.contains("register_model"),
                    "contention message should mention the scope name: {message}"
                );
            }
            other => panic!(
                "expected DbError::LockContention after backoff exhaustion, got {other:?}"
            ),
        }

        // Sanity: the wire code surfaces as `lock_not_available`
        // through `to_op_error`. We don't reach into `to_op_error`
        // here (it's a private mapping) — the message-shape assertion
        // above is the test-level invariant; the
        // `lock_not_available` mapping is covered by the lib-level
        // `op_code` tests in `crate::error`.
    });
}

#[test]
fn introspect_empty_schema_yields_empty_live_schema() {
    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .ensure_app_schema("app_demo")
            .await
            .expect("ensure_app_schema");
        let live = backend
            .introspect_schema("app_demo")
            .await
            .expect("introspect_schema on empty namespace");
        // No user tables → empty `tables` / `indexes` / `foreign_keys`.
        // The `LiveSchema` shape uses HashMap so "empty" is `is_empty()`.
        assert!(
            live.tables.is_empty(),
            "empty namespace must yield empty `tables`; got {:?}",
            live.tables.keys().collect::<Vec<_>>()
        );
        assert!(
            live.indexes.is_empty(),
            "empty namespace must yield empty `indexes`"
        );
        assert!(
            live.foreign_keys.is_empty(),
            "empty namespace must yield empty `foreign_keys`"
        );
    });
}

#[test]
fn introspect_after_create_table_round_trip() {
    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .ensure_app_schema("app_demo")
            .await
            .expect("ensure_app_schema");

        // Create a small table with a mix of NULL and NOT NULL
        // columns, plus a non-PK index, so the introspect output
        // exercises every PRAGMA branch.
        let client = backend
            .acquire_dedicated_client()
            .await
            .expect("acquire client");
        backend
            .client_exec(
                &client,
                "CREATE TABLE \"app_demo\".\"items\" (\
                     id INTEGER PRIMARY KEY, \
                     name TEXT NOT NULL, \
                     payload TEXT\
                 )",
                &[],
            )
            .await
            .expect("CREATE TABLE items");
        backend
            .client_exec(
                &client,
                "CREATE INDEX \"app_demo\".\"items_name_idx\" ON \"items\"(name)",
                &[],
            )
            .await
            .expect("CREATE INDEX items_name_idx");

        let live = backend
            .introspect_schema("app_demo")
            .await
            .expect("introspect_schema after CREATE TABLE");

        // Table must be observed.
        let cols = live
            .tables
            .get("items")
            .expect("items table must be present in LiveSchema");
        assert_eq!(cols.len(), 3, "items has 3 columns, got {:?}", cols.keys().collect::<Vec<_>>());

        // Type strings: SQLite returns the declared affinity uppercase
        // ("INTEGER" / "TEXT"). The diff classifier reads these
        // stringly — the PG impl populates `format_type(...)` results
        // here; SQLite populates the affinity name directly per plan
        // §3.4 ("populate `pg_type` with SQLite affinity names").
        let id_col = cols.get("id").expect("id column");
        assert_eq!(id_col.pg_type, "INTEGER");
        // `id INTEGER PRIMARY KEY` is a special SQLite case — it's an
        // alias for ROWID, NOT-NULL-implicit only when the row has a
        // value. PRAGMA `table_info.notnull` returns 0 here even
        // though the column is effectively NOT NULL; we faithfully
        // report what the engine surfaces.
        assert!(
            !id_col.not_null,
            "PRAGMA table_info reports notnull=0 for INTEGER PRIMARY KEY (ROWID alias)"
        );

        let name_col = cols.get("name").expect("name column");
        assert_eq!(name_col.pg_type, "TEXT");
        assert!(name_col.not_null, "name was declared NOT NULL");

        let payload_col = cols.get("payload").expect("payload column");
        assert_eq!(payload_col.pg_type, "TEXT");
        assert!(!payload_col.not_null, "payload was declared nullable");

        // Index must be observed — the non-PK `items_name_idx` should
        // appear; the implicit `sqlite_autoindex_*` for `INTEGER
        // PRIMARY KEY` does NOT appear because INTEGER PRIMARY KEY
        // uses the ROWID and doesn't create an autoindex entry.
        let idxs = live
            .indexes
            .get("items")
            .expect("items must have an index map");
        let idx_info = idxs
            .get("items_name_idx")
            .expect("items_name_idx must be present");
        assert!(!idx_info.is_unique, "items_name_idx is non-unique");
        assert_eq!(idx_info.columns, vec!["name".to_string()]);

        // No FKs declared → no FK entries.
        assert!(
            live.foreign_keys.get("items").is_none(),
            "items has no declared FKs"
        );

        // estimate_row_count on the empty table is 0.
        let n = backend
            .estimate_row_count("app_demo", "items")
            .await
            .expect("estimate_row_count");
        assert_eq!(n, 0, "freshly-created table has 0 rows");
    });
}
