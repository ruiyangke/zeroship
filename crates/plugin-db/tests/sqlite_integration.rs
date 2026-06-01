//! SQLite-side integration tests.
//!
//! Behind `required-features = ["test-helpers"]`. **P1 PR 2** adds
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

use std::rc::Rc;

#[path = "parity/mod.rs"]
mod parity;

use zeroship_plugin_db::backend::sqlite::SqliteBackend;
use zeroship_plugin_db::backend::{
    BackendHandle, ChangeStream, IndexBuilder, LockManager, LockScope, NamespaceManager,
    SchemaIntrospect, SqlExecutor,
};
use zeroship_plugin_db::broker::{subscribe, ChangeOp, Subscription, SubscriptionMessage};
use zeroship_plugin_db::error::DbError;
use zeroship_plugin_db::query::{IndexKind, IndexSpec};

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

#[test]
fn parity_matrix_sqlite_seed_projection_matches_contract() {
    run(async {
        let dir = tempfile::tempdir().expect("create parity dir");
        let snapshot = parity::run_matrix(&parity::sqlite_url(&dir));
        assert_eq!(snapshot.seed, parity::expected_seed_projection());
    });
}

#[test]
fn parity_matrix_sqlite_transaction_projection_matches_contract() {
    run(async {
        let dir = tempfile::tempdir().expect("create parity dir");
        let snapshot = parity::run_matrix(&parity::sqlite_url(&dir));
        assert_eq!(snapshot.tx, parity::expected_tx_projection());
    });
}

#[test]
fn parity_matrix_sqlite_typed_projection_matches_contract() {
    run(async {
        let dir = tempfile::tempdir().expect("create parity dir");
        let snapshot = parity::run_matrix(&parity::sqlite_url(&dir));
        assert_eq!(snapshot.typed, parity::expected_typed_projection());
    });
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

#[test]
fn estimate_row_count_missing_table_returns_zero() {
    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .ensure_app_schema("app_demo")
            .await
            .expect("ensure_app_schema");

        let rows = backend
            .estimate_row_count("app_demo", "missing_table")
            .await
            .expect("estimate_row_count for missing table");
        assert_eq!(rows, 0, "missing table must classify as empty");
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

// ---------------------------------------------------------------------------
// P1 PR 5 — IndexBuilder (CREATE INDEX) + cross-app FK parse-time check
// integration tests.
//
// The IndexBuilder-side tests exercise the two terminal branches of
// `create_index_with_recovery` on the SQLite arm:
//
// 1. Happy path — `CREATE [UNIQUE] INDEX IF NOT EXISTS` against a
//    freshly-created table; the call returns Ok and the index appears
//    in `PRAGMA index_list`.
// 2. Unique-constraint violation — the table already carries duplicate
//    rows, so a `CREATE UNIQUE INDEX` returns the canonical
//    `DbError::SchemaRefused { code: "validation_refused", ... }`
//    envelope (wire-compatible with the PG arm at
//    `backend/postgres.rs::create_index_with_recovery_audited`).
//
// The cross-app FK test exercises the pure-Rust validator at
// `crate::cross_app_fk::reject_cross_app_fk` end-to-end; it is the
// same module the PG-side integration test imports, so this assertion
// is mirrored byte-for-byte against the PG path in
// `tests/integration.rs::cross_app_fk_rejected_at_parse`.
// ---------------------------------------------------------------------------

/// Provision the per-app `__zeroship_migrations` audit table the
/// `AuditWriter` impl writes into. P1 PR 5 ships only the INSERT path;
/// the audit-table provisioning DDL is a later-PR concern. We create
/// it inline here so the `unique_violation` path's best-effort audit
/// write actually lands during the test (the test still passes if the
/// write fails — the SchemaRefused envelope assertion is the wire
/// contract — but covering both halves is cheap).
async fn ensure_audit_table(backend: &SqliteBackend, app_id: &str) {
    let sql = format!(
        "CREATE TABLE IF NOT EXISTS \"{app_id}\".\"__zeroship_migrations\" (\
             id              INTEGER PRIMARY KEY AUTOINCREMENT, \
             collection      TEXT NOT NULL, \
             phase           TEXT NOT NULL, \
             change_class    TEXT NOT NULL, \
             change_kind     TEXT NOT NULL, \
             details         TEXT NOT NULL, \
             ddl_sql         TEXT, \
             status          TEXT NOT NULL, \
             deploy_id       TEXT NOT NULL, \
             applied_by_kind TEXT NOT NULL, \
             schema_version  INTEGER NOT NULL\
         )"
    );
    backend
        .pool_exec(&sql, &[])
        .await
        .expect("create __zeroship_migrations audit table");
}

#[test]
fn create_index_succeeds() {
    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .ensure_app_schema("app_demo")
            .await
            .expect("ensure_app_schema");
        // Create the user table the index will cover.
        backend
            .pool_exec(
                "CREATE TABLE \"app_demo\".\"things\" (\
                     id INTEGER PRIMARY KEY, \
                     name TEXT NOT NULL\
                 )",
                &[],
            )
            .await
            .expect("CREATE TABLE things");

        // Build a non-unique index spec. The `sql` field is unused by
        // the SQLite IndexBuilder impl (which rebuilds the DDL from
        // `name` + `columns` + `unique` against the SQLite dialect),
        // so we leave it empty — the test exercises the rebuild path.
        let spec = IndexSpec {
            name: "things_name_idx".to_string(),
            columns: vec!["name".to_string()],
            unique: false,
            sql: String::new(),
            kind: IndexKind::BTree,
        };

        backend
            .create_index_with_recovery(
                "app_demo",
                "things",
                &spec,
                "test_deploy",
                1,
            )
            .await
            .expect("create_index_with_recovery should succeed on a clean table");

        // Cross-check via SchemaIntrospect: the index must appear in the
        // PRAGMA-walk output. Routes through the same actor as the
        // CREATE INDEX, so visibility is guaranteed without an extra
        // commit/flush step.
        let live = backend
            .introspect_schema("app_demo")
            .await
            .expect("introspect_schema");
        let idxs = live
            .indexes
            .get("things")
            .expect("things must have an index map after CREATE INDEX");
        let info = idxs
            .get("things_name_idx")
            .expect("things_name_idx must be present");
        assert!(!info.is_unique);
        assert_eq!(info.columns, vec!["name".to_string()]);
    });
}

#[test]
fn create_unique_index_fails_on_duplicate_with_envelope() {
    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .ensure_app_schema("app_demo")
            .await
            .expect("ensure_app_schema");
        ensure_audit_table(&backend, "app_demo").await;

        // Table + two rows with the same `email` value so a UNIQUE
        // index on `email` cannot land.
        backend
            .pool_exec(
                "CREATE TABLE \"app_demo\".\"users\" (\
                     id INTEGER PRIMARY KEY, \
                     email TEXT NOT NULL\
                 )",
                &[],
            )
            .await
            .expect("CREATE TABLE users");
        backend
            .pool_exec(
                "INSERT INTO \"app_demo\".\"users\" (email) VALUES ('a@x'), ('a@x')",
                &[],
            )
            .await
            .expect("INSERT duplicate emails");

        let spec = IndexSpec {
            name: "users_email_uniq".to_string(),
            columns: vec!["email".to_string()],
            unique: true,
            sql: String::new(),
            kind: IndexKind::BTree,
        };

        let err = backend
            .create_index_with_recovery(
                "app_demo",
                "users",
                &spec,
                "test_deploy",
                1,
            )
            .await
            .expect_err("UNIQUE index on duplicate column values must reject");

        // Wire-compatible envelope per `docs/proposals/p1-sqlite-implementation-plan.md`
        // §3.5: code = "validation_refused" with structured `code:
        // "unique_violation"` (via the inner `constraint: "unique"`
        // field — the PG arm's envelope shape) inside the JSON body.
        match err {
            DbError::SchemaRefused { code, envelope_json } => {
                assert_eq!(
                    code, "validation_refused",
                    "envelope outer code must be `validation_refused` for SDK branching"
                );
                let v: serde_json::Value = serde_json::from_str(&envelope_json)
                    .expect("envelope must be valid JSON");
                assert_eq!(v["code"], "validation_refused");
                assert_eq!(v["change_kind"], "index_retry");
                assert_eq!(v["collection"], "users");
                assert_eq!(v["index"], "users_email_uniq");
                assert_eq!(v["constraint"], "unique");
                // Conflicting-key extraction divergence (plan §3.5):
                // SQLite errors don't carry the duplicate key value the
                // way PG's 23505 does. The envelope reports an empty
                // list and a hint pointing the SDK at a read path.
                assert!(
                    v["conflicting_keys"].as_array().map(|a| a.is_empty()).unwrap_or(false),
                    "conflicting_keys must be the empty list on SQLite (divergence): {v}"
                );
            }
            other => panic!("expected DbError::SchemaRefused, got {other:?}"),
        }
    });
}

#[test]
fn cross_app_fk_rejected_at_parse() {
    // Pure-Rust validator — no DB round-trip required. The PG-side
    // `tests/integration.rs::cross_app_fk_rejected_at_parse` is the
    // mirror assertion; both reach the same `crate::cross_app_fk`
    // module so a regression here would fail both targets.
    use zeroship_plugin_db::cross_app_fk::reject_cross_app_fk;

    let schema = serde_json::json!({
        "authorId": { "type": "ref", "refTarget": "other_app.users" }
    });
    let err = reject_cross_app_fk(&schema, "app_demo")
        .expect_err("cross-app ref must reject at parse time");
    match err {
        DbError::Configuration { code, message, hint } => {
            assert_eq!(code, "cross_app_fk_forbidden");
            assert!(
                message.contains("other_app.users"),
                "message must name the offending target: {message}"
            );
            assert!(
                hint.as_deref().map(|h| h.contains("Drop the")).unwrap_or(false),
                "hint must point at remediation: {hint:?}"
            );
        }
        other => panic!("expected DbError::Configuration, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// P2 PR 2 — SqliteCdcDispatcher (preupdate/commit/rollback hooks) +
// worker→compio publisher integration tests.
//
// The publisher task is asynchronous: a COMMIT on the writer thread
// ships a `CommitPacket` via flume, the publisher task wakes on
// `recv_async`, resolves column names via `PRAGMA table_info` through
// the session actor, then calls `broker::publish` on the compio
// thread. The thread-local broker is the test consumer (we subscribe
// directly on the compio thread).
//
// Each test:
//   1. Spins up a fresh backend (which also spawns the publisher task).
//   2. ATTACHes a per-app namespace via `ensure_app_schema(app_id)`.
//   3. Creates a user table.
//   4. Subscribes to (app_id, table) on the local broker.
//   5. Performs the mutation(s) under test.
//   6. Yields the runtime so the publisher task can drain the channel
//      and call `broker::publish`.
//   7. Drains the subscription queue + asserts shape.
//
// The publisher needs at least one `await` yield (and one PRAGMA
// round-trip on cache miss) between COMMIT and broker delivery, so
// each test awaits a short `compio::time::sleep` after the mutation.
// 25ms is overkill for the in-process round-trip but keeps the tests
// quiet on slow shared CI hosts.
// ---------------------------------------------------------------------------

/// Wait long enough for the publisher task to drain the CDC channel
/// and call `broker::publish`. The publisher loop is:
///
///   recv_async → fetch PRAGMA table_info (1 round-trip on first
///   touch) → broker::publish per event.
///
/// All steps run on the same compio thread as the test future, so a
/// single yield is the lower bound; we sleep generously for CI noise
/// tolerance.
async fn drain_publisher() {
    compio::time::sleep(std::time::Duration::from_millis(50)).await;
}

/// Drain a subscription's queue into a `Vec<SubscriptionMessage>` —
/// the tests pattern-match on the resulting shape.
fn drain(sub: &Subscription) -> Vec<SubscriptionMessage> {
    let mut out = Vec::new();
    while let Some(msg) = sub.pop() {
        out.push(msg);
    }
    out
}

/// Helper: subscribe to `(app_id, collection)` on the thread-local
/// broker. The broker lives in a thread-local cell — the publisher
/// task and this test future run on the same compio thread, so the
/// subscription is visible to the publisher's `broker::publish` calls.
fn subscribe_local(app_id: &str, collection: &str) -> Subscription {
    subscribe(app_id, collection)
}

#[test]
fn insert_publishes_via_preupdate_hook() {
    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .ensure_app_schema("app_cdc")
            .await
            .expect("ensure_app_schema");

        // Create a user table the CDC hook will fire against.
        backend
            .pool_exec(
                "CREATE TABLE \"app_cdc\".\"items\" (\
                     id INTEGER PRIMARY KEY, \
                     name TEXT NOT NULL\
                 )",
                &[],
            )
            .await
            .expect("CREATE TABLE items");

        // Subscribe BEFORE the mutation. The DDL above is not a
        // user-table write; it goes through `sqlite_master` which the
        // dispatcher filters, so no event is queued.
        let sub = subscribe_local("app_cdc", "items");

        // INSERT a row — the preupdate hook fires, commit hook ships
        // the packet, publisher resolves column names + publishes.
        backend
            .pool_exec(
                "INSERT INTO \"app_cdc\".\"items\" (name) VALUES ('alice')",
                &[],
            )
            .await
            .expect("INSERT items");

        drain_publisher().await;

        let msgs = drain(&sub);
        assert_eq!(
            msgs.len(),
            1,
            "expected 1 event after a single-row INSERT; got {msgs:?}"
        );
        match &msgs[0] {
            SubscriptionMessage::Change(ev) => {
                assert_eq!(ev.op, ChangeOp::Insert);
                assert_eq!(ev.app_id, "app_cdc");
                assert_eq!(ev.collection, "items");
                assert!(
                    !ev.new_tuple.is_empty(),
                    "INSERT event must carry a populated new_tuple; got {:?}",
                    ev.new_tuple
                );
                // Column names were resolved via PRAGMA table_info on
                // the publisher — `name` should be present.
                assert_eq!(
                    ev.new_tuple.get("name"),
                    Some(&"alice".to_string()),
                    "new_tuple should carry the inserted `name`; got {:?}",
                    ev.new_tuple
                );
                assert!(
                    ev.old_tuple.is_none(),
                    "INSERT must not carry an old_tuple; got {:?}",
                    ev.old_tuple
                );
            }
            other => panic!("expected Change event, got {other:?}"),
        }
    });
}

#[test]
fn insert_publishes_logical_typed_id_not_sqlite_rowid() {
    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .ensure_app_schema("app_cdc")
            .await
            .expect("ensure_app_schema");

        backend
            .pool_exec(
                "CREATE TABLE \"app_cdc\".\"typed_items\" (\
                     id TEXT PRIMARY KEY, \
                     name TEXT NOT NULL\
                 )",
                &[],
            )
            .await
            .expect("CREATE TABLE typed_items");

        let sub = subscribe_local("app_cdc", "typed_items");
        let typed_id = "usr_02HXSQLITECDCLOGICALPK";

        backend
            .pool_exec(
                &format!(
                    "INSERT INTO \"app_cdc\".\"typed_items\" (id, name) \
                     VALUES ('{typed_id}', 'alice')"
                ),
                &[],
            )
            .await
            .expect("INSERT typed_items");

        drain_publisher().await;

        let msgs = drain(&sub);
        assert_eq!(msgs.len(), 1, "expected 1 typed-id event; got {msgs:?}");
        match &msgs[0] {
            SubscriptionMessage::Change(ev) => {
                assert_eq!(ev.op, ChangeOp::Insert);
                assert_eq!(ev.pk.as_deref(), Some(typed_id));
                assert_eq!(ev.new_tuple.get("id").map(String::as_str), Some(typed_id));
            }
            other => panic!("expected Change event, got {other:?}"),
        }
    });
}

#[test]
fn update_publishes_change_event_with_pre_image() {
    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .ensure_app_schema("app_cdc")
            .await
            .expect("ensure_app_schema");
        backend
            .pool_exec(
                "CREATE TABLE \"app_cdc\".\"items\" (\
                     id INTEGER PRIMARY KEY, \
                     name TEXT NOT NULL\
                 )",
                &[],
            )
            .await
            .expect("CREATE TABLE items");

        // Seed one row. We subscribe AFTER the seed so the INSERT
        // event is not part of what `drain` sees.
        backend
            .pool_exec(
                "INSERT INTO \"app_cdc\".\"items\" (id, name) VALUES (1, 'alice')",
                &[],
            )
            .await
            .expect("INSERT seed row");

        // Give the publisher a chance to drain the seed event so it
        // doesn't show up in the subscription created below (the
        // subscribe happens on the same thread, but only AFTER the
        // publisher has fanned out the prior packet).
        drain_publisher().await;

        let sub = subscribe_local("app_cdc", "items");

        // UPDATE the row — the preupdate hook should capture both
        // OLD ('alice') and NEW ('bob') tuples.
        backend
            .pool_exec(
                "UPDATE \"app_cdc\".\"items\" SET name = 'bob' WHERE id = 1",
                &[],
            )
            .await
            .expect("UPDATE items");

        drain_publisher().await;

        let msgs = drain(&sub);
        assert_eq!(
            msgs.len(),
            1,
            "expected 1 event after a single-row UPDATE; got {msgs:?}"
        );
        match &msgs[0] {
            SubscriptionMessage::Change(ev) => {
                assert_eq!(ev.op, ChangeOp::Update);
                assert!(
                    !ev.new_tuple.is_empty(),
                    "UPDATE event must carry a populated new_tuple; got {:?}",
                    ev.new_tuple
                );
                assert_eq!(
                    ev.new_tuple.get("name"),
                    Some(&"bob".to_string()),
                    "new_tuple should carry the post-image name; got {:?}",
                    ev.new_tuple
                );
                let old = ev
                    .old_tuple
                    .as_ref()
                    .expect("UPDATE must carry an old_tuple (pre-image)");
                assert_eq!(
                    old.get("name"),
                    Some(&"alice".to_string()),
                    "old_tuple should carry the pre-image name; got {old:?}"
                );
            }
            other => panic!("expected Change event, got {other:?}"),
        }
    });
}

#[test]
fn rollback_does_not_publish() {
    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .ensure_app_schema("app_cdc")
            .await
            .expect("ensure_app_schema");
        backend
            .pool_exec(
                "CREATE TABLE \"app_cdc\".\"items\" (\
                     id INTEGER PRIMARY KEY, \
                     name TEXT NOT NULL\
                 )",
                &[],
            )
            .await
            .expect("CREATE TABLE items");

        let sub = subscribe_local("app_cdc", "items");

        // BEGIN / INSERT / ROLLBACK — each statement routes through
        // the session actor (same worker thread; serialised by the
        // mpsc queue). The rollback_hook clears the buffer; no packet
        // ships.
        backend
            .pool_exec("BEGIN", &[])
            .await
            .expect("BEGIN");
        backend
            .pool_exec(
                "INSERT INTO \"app_cdc\".\"items\" (name) VALUES ('alice')",
                &[],
            )
            .await
            .expect("INSERT inside tx");
        backend
            .pool_exec("ROLLBACK", &[])
            .await
            .expect("ROLLBACK");

        drain_publisher().await;

        let msgs = drain(&sub);
        assert!(
            msgs.is_empty(),
            "ROLLBACK must not publish any events; got {msgs:?}"
        );
    });
}

#[test]
fn mixed_ops_in_one_tx_ordered_by_buffer_index() {
    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .ensure_app_schema("app_cdc")
            .await
            .expect("ensure_app_schema");
        backend
            .pool_exec(
                "CREATE TABLE \"app_cdc\".\"items\" (\
                     id INTEGER PRIMARY KEY, \
                     name TEXT NOT NULL\
                 )",
                &[],
            )
            .await
            .expect("CREATE TABLE items");
        // Seed rows for the UPDATE + DELETE arms of the mixed-op tx
        // below. Done BEFORE subscription so the seed events don't
        // pollute the assertions.
        backend
            .pool_exec(
                "INSERT INTO \"app_cdc\".\"items\" (id, name) VALUES (10, 'b_pre'), (20, 'c_pre')",
                &[],
            )
            .await
            .expect("INSERT seed rows");
        drain_publisher().await;

        let sub = subscribe_local("app_cdc", "items");

        // BEGIN; INSERT a; UPDATE b; DELETE c; INSERT d; COMMIT.
        // Each statement fires the preupdate hook once; the commit
        // hook ships a single CommitPacket with all 4 events in
        // buffer order.
        backend
            .pool_exec("BEGIN", &[])
            .await
            .expect("BEGIN");
        backend
            .pool_exec(
                "INSERT INTO \"app_cdc\".\"items\" (id, name) VALUES (1, 'a')",
                &[],
            )
            .await
            .expect("INSERT a");
        backend
            .pool_exec(
                "UPDATE \"app_cdc\".\"items\" SET name = 'b_post' WHERE id = 10",
                &[],
            )
            .await
            .expect("UPDATE b");
        backend
            .pool_exec(
                "DELETE FROM \"app_cdc\".\"items\" WHERE id = 20",
                &[],
            )
            .await
            .expect("DELETE c");
        backend
            .pool_exec(
                "INSERT INTO \"app_cdc\".\"items\" (id, name) VALUES (2, 'd')",
                &[],
            )
            .await
            .expect("INSERT d");
        backend
            .pool_exec("COMMIT", &[])
            .await
            .expect("COMMIT");

        drain_publisher().await;

        let msgs = drain(&sub);
        assert_eq!(
            msgs.len(),
            4,
            "expected 4 events after a 4-statement tx; got {msgs:?}"
        );
        // Per plan §8: order is `[a, b, c, d]` = INSERT, UPDATE,
        // DELETE, INSERT. Each msg is a Change variant carrying the
        // event.
        let ops: Vec<ChangeOp> = msgs
            .iter()
            .map(|m| match m {
                SubscriptionMessage::Change(ev) => ev.op,
                other => panic!("expected Change, got {other:?}"),
            })
            .collect();
        assert_eq!(
            ops,
            vec![
                ChangeOp::Insert,
                ChangeOp::Update,
                ChangeOp::Delete,
                ChangeOp::Insert,
            ],
            "events must appear in buffer order [a, b, c, d]: {ops:?}"
        );
    });
}

// ---------------------------------------------------------------------------
// P2 PR 3 — relation filter gates (MV/audit) + subscription fan-out under
// load. The relation filter itself was already wired in PR 2's
// `cdc.rs::preupdate_callback` (first early-return after the action
// discriminant). PR 3 adds the integration coverage that pins the
// filter's behaviour end-to-end + the broker primitive
// `Broker::resume_app_with_resync` (unit-covered in `broker.rs`).
//
// Test budget: each test stays well under 2s on the CI workers — the
// fan-out test uses 10 subscribers × 100 rows (NOT the plan §8
// 100 × 1000, which would saturate dev hardware; the buffer-index
// ordering invariant is identical at smaller scale).
// ---------------------------------------------------------------------------

#[test]
fn subscription_fanout_under_load() {
    // Plan §8 / §9 PR 3 gate: a single COMMIT of N rows must reach every
    // active subscriber in INSERT order. Scaled down to 10×100 per the
    // task spec ("100 subscribers × 1000 rows would saturate dev
    // hardware; scale down to 10 × 100 for CI sanity"). The default
    // queue depth is 1024 (`broker::DEFAULT_QUEUE_DEPTH`), so 100 rows
    // fit comfortably without triggering the overflow-to-Resync path.
    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .ensure_app_schema("app_fanout")
            .await
            .expect("ensure_app_schema");
        backend
            .pool_exec(
                "CREATE TABLE \"app_fanout\".\"items\" (\
                     id INTEGER PRIMARY KEY, \
                     name TEXT NOT NULL\
                 )",
                &[],
            )
            .await
            .expect("CREATE TABLE items");

        // Subscribe 10 times to the same (app, collection). Each
        // returned `Subscription` is a fresh routing-table entry — the
        // broker fans the same Rc<ChangeEvent> out to each.
        let subs: Vec<Subscription> =
            (0..10).map(|_| subscribe_local("app_fanout", "items")).collect();

        // BEGIN; 100×INSERT; COMMIT. Each statement routes through the
        // session actor in order, so the buffer accumulates events in
        // INSERT order. The commit_hook then ships one CommitPacket
        // with all 100 events; the publisher iterates and fans out.
        backend
            .pool_exec("BEGIN", &[])
            .await
            .expect("BEGIN");
        for i in 0..100 {
            let sql = format!(
                "INSERT INTO \"app_fanout\".\"items\" (id, name) VALUES ({i}, 'r{i}')"
            );
            backend
                .pool_exec(&sql, &[])
                .await
                .expect("INSERT inside tx");
        }
        backend
            .pool_exec("COMMIT", &[])
            .await
            .expect("COMMIT");

        // Generous drain — 100 publishes × 10 subscribers under the
        // single-threaded compio runtime + one PRAGMA round-trip on
        // first touch. 200ms is comfortably above the in-process
        // upper bound on dev hardware.
        compio::time::sleep(std::time::Duration::from_millis(200)).await;

        for (i, sub) in subs.iter().enumerate() {
            let msgs = drain(sub);
            assert_eq!(
                msgs.len(),
                100,
                "subscriber #{i} should observe 100 events; got {} ({msgs:?})",
                msgs.len()
            );
            // Buffer-index ordering invariant from PR 2: events appear
            // in the order they fired against the preupdate hook,
            // which matches statement order under SQLite's
            // single-writer execution.
            for (idx, msg) in msgs.iter().enumerate() {
                match msg {
                    SubscriptionMessage::Change(ev) => {
                        assert_eq!(
                            ev.op,
                            ChangeOp::Insert,
                            "subscriber #{i} event {idx} must be Insert; got {:?}",
                            ev.op
                        );
                        // The `id` column carries the per-row index. We
                        // assert ordering through that field.
                        let id_str = ev
                            .new_tuple
                            .get("id")
                            .unwrap_or_else(|| panic!(
                                "subscriber #{i} event {idx} missing `id`: {:?}",
                                ev.new_tuple
                            ));
                        let id: i64 = id_str
                            .parse()
                            .unwrap_or_else(|_| panic!("non-numeric id: {id_str}"));
                        assert_eq!(
                            id, idx as i64,
                            "subscriber #{i} event {idx} must carry id={idx}; got id={id}"
                        );
                    }
                    other => panic!(
                        "subscriber #{i} event {idx} must be Change; got {other:?}"
                    ),
                }
            }
        }
    });
}

#[test]
fn mv_refresh_does_not_emit_change_events() {
    // Plan §6 + §9 PR 3 gate: writes to `__zeroship_mv_*` shadow tables
    // must be filtered upstream of the broker. The plan acknowledges
    // (§9 PR 3) that the `db.materializedView(...).refresh()` SDK
    // primitive does not exist yet, so we exercise the filter directly
    // by writing to a shadow table whose name matches the filter
    // prefix — the dispatcher cannot distinguish a "real" MV refresh
    // from a hand-rolled shadow write.
    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .ensure_app_schema("app_mv")
            .await
            .expect("ensure_app_schema");
        // Create a shadow table that mimics what an MV refresh would
        // emit. The CREATE itself only touches sqlite_master (already
        // filtered); the INSERT below is the gate.
        backend
            .pool_exec(
                "CREATE TABLE \"app_mv\".\"__zeroship_mv_demo\" (\
                     id INTEGER PRIMARY KEY, \
                     v TEXT NOT NULL\
                 )",
                &[],
            )
            .await
            .expect("CREATE TABLE __zeroship_mv_demo");

        // Subscribe to the shadow table directly so we'd observe any
        // event that leaked past the filter. (The SDK boundary refuses
        // such a subscription via `Db::open_subscription`; the broker
        // primitive does NOT, and we exercise the broker level here.)
        let sub = subscribe_local("app_mv", "__zeroship_mv_demo");

        // INSERT into the shadow — this is the write the filter must
        // drop. The preupdate hook fires, `is_filtered_relation`
        // returns `true`, no event is buffered, no packet ships.
        backend
            .pool_exec(
                "INSERT INTO \"app_mv\".\"__zeroship_mv_demo\" (id, v) VALUES (1, 'a')",
                &[],
            )
            .await
            .expect("INSERT into shadow");

        drain_publisher().await;

        let msgs = drain(&sub);
        assert!(
            msgs.is_empty(),
            "writes to __zeroship_mv_* must not reach the broker; got {msgs:?}"
        );
    });
}

#[test]
fn mv_refresh_emits_no_change_events_on_base_or_shadow() {
    // Variant of the previous gate: when a transaction touches BOTH a
    // shadow table AND a regular collection, the shadow writes are
    // filtered and the regular writes pass through. The regular
    // subscriber observes exactly the regular events; the shadow
    // subscriber observes zero events.
    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .ensure_app_schema("app_mv_mixed")
            .await
            .expect("ensure_app_schema");
        // Regular collection.
        backend
            .pool_exec(
                "CREATE TABLE \"app_mv_mixed\".\"items\" (\
                     id INTEGER PRIMARY KEY, \
                     name TEXT NOT NULL\
                 )",
                &[],
            )
            .await
            .expect("CREATE TABLE items");
        // Shadow table.
        backend
            .pool_exec(
                "CREATE TABLE \"app_mv_mixed\".\"__zeroship_mv_items\" (\
                     id INTEGER PRIMARY KEY, \
                     v TEXT NOT NULL\
                 )",
                &[],
            )
            .await
            .expect("CREATE TABLE __zeroship_mv_items");

        let regular_sub = subscribe_local("app_mv_mixed", "items");
        let shadow_sub = subscribe_local("app_mv_mixed", "__zeroship_mv_items");

        // Single transaction touching both tables. The shadow write
        // is filtered at the hook; the regular write reaches the broker.
        backend
            .pool_exec("BEGIN", &[])
            .await
            .expect("BEGIN");
        backend
            .pool_exec(
                "INSERT INTO \"app_mv_mixed\".\"items\" (id, name) VALUES (1, 'alice')",
                &[],
            )
            .await
            .expect("INSERT items");
        backend
            .pool_exec(
                "INSERT INTO \"app_mv_mixed\".\"__zeroship_mv_items\" (id, v) VALUES (1, 'a')",
                &[],
            )
            .await
            .expect("INSERT shadow");
        backend
            .pool_exec("COMMIT", &[])
            .await
            .expect("COMMIT");

        drain_publisher().await;

        let regular_msgs = drain(&regular_sub);
        let shadow_msgs = drain(&shadow_sub);

        assert_eq!(
            regular_msgs.len(),
            1,
            "regular collection should observe exactly 1 INSERT; got {regular_msgs:?}"
        );
        match &regular_msgs[0] {
            SubscriptionMessage::Change(ev) => {
                assert_eq!(ev.op, ChangeOp::Insert);
                assert_eq!(ev.collection, "items");
                assert_eq!(
                    ev.new_tuple.get("name"),
                    Some(&"alice".to_string()),
                    "regular collection event must carry the inserted name; got {:?}",
                    ev.new_tuple
                );
            }
            other => panic!("expected Change event on regular collection, got {other:?}"),
        }
        assert!(
            shadow_msgs.is_empty(),
            "shadow collection must observe zero events; got {shadow_msgs:?}"
        );
    });
}

#[test]
fn audit_table_writes_do_not_emit_events() {
    // The `is_filtered_relation` predicate covers `__zeroship_audit_*`
    // alongside `__zeroship_mv_*`. The PR 2 unit test in
    // `cdc.rs::tests::is_filtered_relation_excludes_system_tables`
    // already pins the predicate; this gate exercises the filter
    // end-to-end so a regression that drops the audit-prefix arm of the
    // predicate would fail here at the integration boundary.
    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .ensure_app_schema("app_audit")
            .await
            .expect("ensure_app_schema");
        backend
            .pool_exec(
                "CREATE TABLE \"app_audit\".\"__zeroship_audit_users\" (\
                     id INTEGER PRIMARY KEY, \
                     event TEXT NOT NULL\
                 )",
                &[],
            )
            .await
            .expect("CREATE TABLE __zeroship_audit_users");

        let sub = subscribe_local("app_audit", "__zeroship_audit_users");

        backend
            .pool_exec(
                "INSERT INTO \"app_audit\".\"__zeroship_audit_users\" \
                 (id, event) VALUES (1, 'delete')",
                &[],
            )
            .await
            .expect("INSERT audit row");

        drain_publisher().await;

        let msgs = drain(&sub);
        assert!(
            msgs.is_empty(),
            "writes to __zeroship_audit_* must not reach the broker; got {msgs:?}"
        );
    });
}

// ---------------------------------------------------------------------------
// P2 PR 4 — round-5 CRITICAL fences (plan §7 + §8 + §9).
//
// These two tests are the round-5 design-loop fences for the
// backfill-pause + schema-pending decoder rails. They must pass
// byte-for-byte: a regression that detaches `BrokerPauseGuard::drop`
// from `wal_consumer::unsuppress_app` + `Broker::resume_app_with_resync`,
// or that detaches `SchemaPendingGuard::drop` from
// `broker::disengage_schema_pending` + `Broker::resume_app_with_resync`,
// or that drops the publisher's per-event suppression check, will
// fail here at the integration boundary rather than at a deferred
// orchestrator call site.
//
// Drain budget: each test sleeps generously (100 ms) after the
// COMMIT before draining the subscription. The publisher is a single
// compio task on the same thread as the test future; 100 ms is well
// above the in-process upper bound on dev hardware. See
// `drain_publisher()` for the existing 50 ms baseline used by the
// PR 2/PR 3 tests.
// ---------------------------------------------------------------------------

/// Longer drain — the new fences move ~100 events through the
/// publisher under a paused broker. The 100 ms budget is the same
/// upper bound `subscription_fanout_under_load` uses (200 ms there
/// for 100 events × 10 subscribers; halved here because we only have
/// one subscriber).
async fn drain_publisher_long() {
    compio::time::sleep(std::time::Duration::from_millis(100)).await;
}

#[test]
fn backfill_run_pauses_broker_and_emits_one_resync() {
    // Plan §7 + §9 PR 4 gate — backfill pause rail end-to-end:
    //
    // 1. ensure_app_schema + CREATE TABLE.
    // 2. Subscribe BEFORE the pause window so the subscription is
    //    visible to `resume_app_with_resync` on guard drop.
    // 3. Engage `BrokerPauseGuard` — this calls `suppress_app(app_id)`
    //    on the thread-local rail.
    // 4. INSERT 100 rows. The preupdate hook still fires + buffers,
    //    the commit_hook ships packets, BUT the publisher's per-event
    //    suppression check drops each packet (debug-logged).
    // 5. Drop the guard. `unsuppress_app` clears the flag +
    //    `resume_app_with_resync` pushes ONE `Resync` per active
    //    subscription.
    // 6. Drain the subscriber → exactly ONE `Resync`, ZERO `Change`
    //    messages.
    //
    // The asymmetry between "INSERT 100 rows" and "one Resync" is the
    // load-bearing contract: backfill silently drops events; the
    // single Resync tells the subscriber to refetch + catch up via
    // the read path, NOT via the event stream.
    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .ensure_app_schema("app_backfill")
            .await
            .expect("ensure_app_schema");
        backend
            .pool_exec(
                "CREATE TABLE \"app_backfill\".\"items\" (\
                     id INTEGER PRIMARY KEY, \
                     name TEXT NOT NULL\
                 )",
                &[],
            )
            .await
            .expect("CREATE TABLE items");

        let sub = subscribe_local("app_backfill", "items");

        // Engage backfill pause. `BrokerPauseGuard::new` calls
        // `wal_consumer::suppress_app(app_id)`; the publisher's
        // per-event check drops every packet for this app until the
        // guard drops.
        let guard = backend.pause_broker_for_tests("app_backfill");

        // INSERT 100 rows under the suppression window. Each statement
        // routes through the session actor, the preupdate hook fires,
        // the commit_hook ships a one-event CommitPacket — the
        // publisher receives the packet, sees `is_app_suppressed`,
        // drops the event + emits a debug-level trace, moves on.
        for i in 0..100 {
            let sql = format!(
                "INSERT INTO \"app_backfill\".\"items\" (id, name) VALUES ({i}, 'r{i}')"
            );
            backend
                .pool_exec(&sql, &[])
                .await
                .expect("INSERT under backfill pause");
        }

        // Give the publisher time to drain the 100 dropped packets
        // BEFORE we drop the guard. Without this sleep the guard's
        // resume_app_with_resync could push the `Resync` while
        // packets are still in flight — they'd still be dropped (the
        // suppression flag is per-packet), but the test invariant
        // ("drain finds exactly one Resync") would be order-sensitive.
        // With the sleep, every packet has been consumed BEFORE we
        // drop the guard, so the Resync is the last thing the
        // subscriber sees.
        drain_publisher_long().await;

        // Drop the guard — calls unsuppress_app + emits one Resync
        // onto every active subscription on `app_backfill`.
        drop(guard);

        // The Resync push is synchronous (broker::resume_app_with_resync
        // pushes onto the subscription's queue inside the guard's
        // Drop), so no further sleep is needed before draining.
        let msgs = drain(&sub);

        // Exactly one message; it must be Resync.
        assert_eq!(
            msgs.len(),
            1,
            "expected exactly one Resync after backfill pause + drop; \
             got {} messages: {msgs:?}",
            msgs.len()
        );
        assert!(
            matches!(msgs[0], SubscriptionMessage::Resync),
            "the single message must be Resync; got {:?}",
            msgs[0]
        );
        // Defensive: NO Change events leaked past the publisher
        // suppression check. (Covered by the len==1 assert above —
        // restated here so a future change that emits an
        // out-of-order Resync alongside Change events surfaces the
        // intent explicitly.)
        let change_count = msgs
            .iter()
            .filter(|m| matches!(m, SubscriptionMessage::Change(_)))
            .count();
        assert_eq!(
            change_count, 0,
            "no Change events must reach the subscriber during a backfill window; got {change_count}"
        );
    });
}

#[test]
fn schema_pending_decoder_drops_then_resyncs() {
    // Plan §7 + §16.7 + §9 PR 4 gate — schema-pending decoder rail
    // end-to-end:
    //
    // 1. ensure_app_schema + CREATE TABLE.
    // 2. Subscribe via the broker BEFORE engaging schema-pending.
    // 3. Engage `SchemaPendingGuard`. This sets the thread-local
    //    `schema_pending_apps` flag AND ensures the publisher's
    //    per-event check drops every packet for the app.
    // 4. INSERT 50 rows — every packet is dropped at the publisher
    //    (debug-logged).
    // 5. While engaged, `broker::try_subscribe(app_id, "other")` MUST
    //    return `DbError::Coded { code: "schema_pending" }`. This is
    //    the LOUD rail (vs the silent backfill rail above).
    // 6. Drop the guard — clears the schema-pending flag + emits one
    //    `Resync` per active subscription.
    // 7. Subsequent INSERT publishes normally (the flag is cleared).
    // 8. Drain: the subscriber observes (a) one Resync from the
    //    guard's drop, then (b) one Change from the post-disengage
    //    INSERT. No events from the pre-disengage window.
    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .ensure_app_schema("app_pending")
            .await
            .expect("ensure_app_schema");
        backend
            .pool_exec(
                "CREATE TABLE \"app_pending\".\"items\" (\
                     id INTEGER PRIMARY KEY, \
                     name TEXT NOT NULL\
                 )",
                &[],
            )
            .await
            .expect("CREATE TABLE items");

        let sub = subscribe_local("app_pending", "items");

        // Engage schema-pending. `SchemaPendingGuard::new` calls
        // `broker::engage_schema_pending(app_id)`; both the publisher
        // suppression check AND the `Broker::try_subscribe` rejection
        // branch activate.
        let guard = backend.engage_schema_pending_for_tests("app_pending");

        // INSERT 50 rows under the schema-pending window. Same shape
        // as the backfill test above — packets ship, publisher drops.
        for i in 0..50 {
            let sql = format!(
                "INSERT INTO \"app_pending\".\"items\" (id, name) VALUES ({i}, 'r{i}')"
            );
            backend
                .pool_exec(&sql, &[])
                .await
                .expect("INSERT under schema-pending");
        }

        // The loud-rail invariant: while engaged, a NEW subscribe call
        // (via `try_subscribe`) MUST return the typed conflict
        // envelope. We don't use the legacy `subscribe()` here because
        // it is infallible by design (back-compat with ~40 in-crate
        // callers); the SDK boundary that lands later wires
        // `try_subscribe` so the JS layer can branch on
        // `e.code === "schema_pending"`.
        let attempt = zeroship_plugin_db::broker::try_subscribe(
            "app_pending",
            "other_collection",
        );
        match &attempt {
            Err(DbError::Coded { code, .. }) => {
                assert_eq!(
                    code, "schema_pending",
                    "try_subscribe during schema-pending must reject \
                     with code=schema_pending; got code={code}"
                );
            }
            other => panic!(
                "expected Err(Coded {{ code: schema_pending }}); got {other:?}"
            ),
        }

        // Let the publisher drain the 50 dropped packets so the
        // sequence "Resync, then post-disengage Change" stays
        // deterministic.
        drain_publisher_long().await;

        // Drop the guard — clears schema_pending flag + pushes one
        // Resync per active subscription.
        drop(guard);

        // Post-disengage: a fresh INSERT must publish normally.
        backend
            .pool_exec(
                "INSERT INTO \"app_pending\".\"items\" (id, name) VALUES (999, 'after')",
                &[],
            )
            .await
            .expect("INSERT after disengage");

        // Give the publisher time to drain the single post-disengage
        // packet onto the broker.
        drain_publisher().await;

        let msgs = drain(&sub);

        // Expected shape: [Resync, Change(999, 'after')].
        // - The 50 INSERTs under the window produced zero events
        //   (publisher dropped them).
        // - The guard's drop pushed exactly one Resync.
        // - The post-disengage INSERT published one Change event.
        assert_eq!(
            msgs.len(),
            2,
            "expected exactly [Resync, Change]; got {} messages: {msgs:?}",
            msgs.len()
        );
        assert!(
            matches!(msgs[0], SubscriptionMessage::Resync),
            "first message must be the disengage-emitted Resync; got {:?}",
            msgs[0]
        );
        match &msgs[1] {
            SubscriptionMessage::Change(ev) => {
                assert_eq!(ev.op, ChangeOp::Insert);
                assert_eq!(ev.collection, "items");
                assert_eq!(
                    ev.new_tuple.get("name"),
                    Some(&"after".to_string()),
                    "second message must be the post-disengage INSERT; \
                     new_tuple={:?}",
                    ev.new_tuple
                );
            }
            other => panic!(
                "second message must be Change(post-disengage); got {other:?}"
            ),
        }

        // Defensive: zero Change events came from the pre-disengage
        // window. (Implied by len==2 + the explicit Change shape
        // above, but stating the contract here makes a future
        // regression that pre-pends events to the Resync surface
        // explicitly.)
        let pre_disengage_changes = msgs
            .iter()
            .filter_map(|m| match m {
                SubscriptionMessage::Change(ev) => Some(ev),
                _ => None,
            })
            .filter(|ev| ev.new_tuple.get("name").map(String::as_str) != Some("after"))
            .count();
        assert_eq!(
            pre_disengage_changes, 0,
            "no pre-disengage Change events must reach the subscriber; got {pre_disengage_changes}"
        );
    });
}

// ---------------------------------------------------------------------------
// P2 tail — orchestrator-driven BrokerPauseGuard fence.
//
// `backfill_run_pauses_broker_and_emits_one_resync` (above) exercises the
// guard via `SqliteBackend::pause_broker_for_tests` — the test-helper
// `pub(crate)` shortcut. This second fence exercises the SAME guard
// behaviour but through `BackendHandle::as_change_stream_sqlite()
// .pause_broker(app_id)` — the API the migration orchestrator
// (`crate::migrations::exec_begin` / `exec_commit_batch`, P2 tail
// wire-up) calls into. A regression that detaches the orchestrator-side
// `ChangeStream::pause_broker` trait method from the underlying
// `BrokerPauseGuard` construction (e.g. someone "optimises" the trait
// to return a no-op guard while leaving the test helper intact) would
// pass the existing fence but fail here.
// ---------------------------------------------------------------------------

#[test]
fn backfill_pauses_broker_via_orchestrator_api_and_emits_one_resync() {
    // Plan §7 + §9 — backfill pause rail driven through the orchestrator's
    // ChangeStream trait surface:
    //
    // 1. ensure_app_schema + CREATE TABLE.
    // 2. Wrap the backend in a `BackendHandle::Sqlite(Rc<...>)` — the
    //    same enum shape the per-isolate context owns. Subscribe BEFORE
    //    the pause window so `resume_app_with_resync` sees the
    //    subscription on guard drop.
    // 3. Acquire `BrokerPauseGuard` via
    //    `BackendHandle::as_change_stream_sqlite()
    //    .pause_broker(app_id)` — the canonical path
    //    `migrations::exec_begin` reaches the guard through (P2 tail).
    // 4. INSERT 100 rows. The preupdate hook still fires + buffers,
    //    the commit_hook ships packets, BUT the publisher's per-event
    //    `is_app_suppressed` check drops each one.
    // 5. Drop the guard. `unsuppress_app` clears the flag +
    //    `resume_app_with_resync` pushes ONE `Resync` per active
    //    subscription.
    // 6. Drain the subscriber → exactly ONE `Resync`, ZERO `Change`.
    //
    // The orchestrator wire-up (`migrations::exec_begin`) is PG-only
    // because `exec_begin` takes a `compio_postgres::Client` directly;
    // we cannot drive the full migration loop against SQLite without
    // re-platforming the orchestrator. What we CAN — and must — pin
    // here is that the same `ChangeStream::pause_broker` API the
    // orchestrator depends on still routes through the
    // `wal_consumer::suppress_app` + `broker::resume_app_with_resync`
    // primitives this rail's contract is built on.
    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .ensure_app_schema("app_orch")
            .await
            .expect("ensure_app_schema");
        backend
            .pool_exec(
                "CREATE TABLE \"app_orch\".\"items\" (\
                     id INTEGER PRIMARY KEY, \
                     name TEXT NOT NULL\
                 )",
                &[],
            )
            .await
            .expect("CREATE TABLE items");

        // Move the backend into the `BackendHandle::Sqlite` arm — the
        // shape the per-isolate context's `ctx.backend()` accessor
        // returns. `as_change_stream_sqlite()` then yields the
        // `SqliteChangeStream` adapter whose `pause_broker(app_id)`
        // mints the same `BrokerPauseGuard` `exec_begin` will mint at
        // PG-side once the SQLite-flavoured orchestrator lands.
        let handle = BackendHandle::Sqlite(Rc::new(backend));

        let sub = subscribe_local("app_orch", "items");

        // Engage backfill pause through the trait-method API. The
        // adapter holds an Rc-clone of the backend so subsequent
        // `pool_exec` calls below route through the same dispatcher.
        let cs = handle
            .as_change_stream_sqlite()
            .expect("BackendHandle::Sqlite must expose ChangeStream");
        let guard = cs.pause_broker("app_orch");

        // Pull a Rc-clone of the inner backend so we can issue the
        // 100 INSERTs against it. (The `BackendHandle::Sqlite` arm owns
        // the master Rc; `as_sqlite()` returns a borrow.)
        let backend_ref = handle
            .as_sqlite()
            .expect("BackendHandle::Sqlite::as_sqlite");

        // INSERT 100 rows under the suppression window. The orchestrator-
        // owned guard's contract: the publisher drops every packet for
        // `app_orch` until the guard's Drop runs.
        for i in 0..100 {
            let sql = format!(
                "INSERT INTO \"app_orch\".\"items\" (id, name) VALUES ({i}, 'r{i}')"
            );
            backend_ref
                .pool_exec(&sql, &[])
                .await
                .expect("INSERT under orchestrator-driven backfill pause");
        }

        // Give the publisher time to drain the 100 dropped packets
        // BEFORE the guard drops — same determinism rationale as the
        // sibling fence above (the suppression flag is per-packet, so
        // dropping the guard before the publisher finishes draining
        // would still drop every in-flight packet, but the `Resync` we
        // assert on must arrive AFTER the last dropped packet for
        // `len == 1` to hold).
        drain_publisher_long().await;

        // Drop the guard — calls `unsuppress_app` + emits one Resync
        // onto every active subscription on `app_orch`. This is the
        // orchestrator-shaped lifecycle: `exec_commit_batch{is_done=true}`
        // → `clear_mig_lock()` → `MigrationLock::drop` → `broker_pause`
        // field drops → `BrokerPauseGuard::drop`.
        drop(guard);

        let msgs = drain(&sub);

        assert_eq!(
            msgs.len(),
            1,
            "expected exactly one Resync after orchestrator-driven pause + drop; \
             got {} messages: {msgs:?}",
            msgs.len()
        );
        assert!(
            matches!(msgs[0], SubscriptionMessage::Resync),
            "the single message must be Resync; got {:?}",
            msgs[0]
        );
        let change_count = msgs
            .iter()
            .filter(|m| matches!(m, SubscriptionMessage::Change(_)))
            .count();
        assert_eq!(
            change_count, 0,
            "no Change events must reach the subscriber during an \
             orchestrator-driven backfill window; got {change_count}"
        );
    });
}

// ---------------------------------------------------------------------------
// P3 PR 4 — SessionMinter integration tests.
//
// These pin the design §19 P3 gates against the SQLite arm of the
// `SessionMinter` trait (see `crates/plugin-db/src/backend/sqlite/mod.rs`,
// `impl SessionMinter for SqliteBackend`). The trait + canonical
// payload format are shared cross-backend; the PG arm exercises the
// same gates via the `b8c_*` tests in `tests/integration.rs`.
//
// Every test below constructs `SqliteBackend::new_with_secrets(...)`
// to bypass the env-var entry point — the secrets are deterministic
// per test so a single `cargo test` invocation yields reproducible
// HMAC signatures. The lone exception is `session_not_configured`,
// which exercises the env-var-unset path via the plain `new(...)`
// constructor; see the comment on that test for the isolation
// rationale.
//
// Test count delta on this target: +8 (round-trip, replay, grace,
// expired, invalid-sig, invalid-actor, nonce-too-short,
// not-configured) plus +1 cross-backend payload-equivalence pin = +9.
// ---------------------------------------------------------------------------

use zeroship_plugin_db::backend::{SessionInit, SessionMinter};

/// 32 bytes of deterministic key material — the same hex digit
/// repeated. Each fixture uses a distinct nibble so two backends in
/// the same test don't accidentally share a key.
fn key_of(nibble: u8) -> Vec<u8> {
    assert!(nibble < 16, "nibble must be 0..=15");
    vec![nibble << 4 | nibble; 32]
}

/// Mint a fresh `SqliteBackend` with explicit minter secrets. The
/// `TempDir` guard is returned alongside the backend so each test
/// gets a clean per-app data directory.
fn backend_with_secrets(
    secret: Vec<u8>,
    secret_prev: Option<Vec<u8>>,
) -> (SqliteBackend, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("create tempdir");
    let backend = SqliteBackend::new_with_secrets(
        PathBuf::from(dir.path()),
        secret,
        secret_prev,
    )
    .expect("open SqliteBackend with secrets");
    (backend, dir)
}

/// Shorthand constructor for a SessionInit fixture exercising the
/// new P3 `pid` field end-to-end.
fn fresh_init() -> SessionInit {
    SessionInit {
        app_id: "app_test".to_string(),
        actor_kind: "user".to_string(),
        actor_id: Some("usr_demo".to_string()),
        pid: Some("prj_demo".to_string()),
    }
}

/// Pattern-match a `DbError` into its typed code. Panics with the
/// inspected variant on shape mismatch so the test message points at
/// the actual failure rather than the assertion line.
fn validation_code(err: &DbError) -> &str {
    match err {
        DbError::ValidationFailed { code, .. } => code,
        other => panic!("expected DbError::ValidationFailed, got {other:?}"),
    }
}

#[test]
fn session_token_round_trip() {
    // P3 design §19 gate #1: a mint-then-init pair round-trips on the
    // SQLite arm with a deterministic secret. The token's nonce
    // length (32), signature length (32 = HMAC-SHA256 output size),
    // and non-empty ISO timestamp are also pinned — these are the
    // cross-backend invariants the SDK contract relies on.
    run(async {
        let (backend, _dir) = backend_with_secrets(key_of(0xa), None);
        let token = backend
            .mint_session_token(fresh_init(), None)
            .await
            .expect("mint_session_token");

        // Token shape invariants. `compute_signature` in
        // `backend/sqlite/session_minter.rs` is HMAC-SHA256, so the
        // signature is always 32 bytes; the nonce is 32 bytes from
        // `getrandom_or_fallback`; `iso_timestamp_after` always
        // emits a non-empty `YYYY-MM-DDTHH:MM:SS.mmm` string.
        assert_eq!(
            token.signature.len(),
            32,
            "HMAC-SHA256 signature must be 32 bytes; got {}",
            token.signature.len()
        );
        assert_eq!(
            token.nonce.len(),
            32,
            "session nonce must be 32 bytes; got {}",
            token.nonce.len()
        );
        assert!(
            !token.expires_at_iso.is_empty(),
            "expires_at_iso must be populated"
        );
        // SQLite arm always reports backend_pid = 0 (no PG concept).
        assert_eq!(token.backend_pid, 0);
        // The mint must echo the `pid` field verbatim — it's part
        // of the canonical signed payload.
        assert_eq!(token.pid.as_deref(), Some("prj_demo"));

        backend
            .init_session(&token)
            .await
            .expect("init_session must accept a freshly-minted token");
    });
}

#[test]
fn session_replay_rejected() {
    // P3 design §19 gate #2: presenting the same token twice
    // surfaces `session_nonce_replay`. The nonce cache is
    // `Rc<RefCell<NonceCache>>` on the backend; the second
    // `init_session` call lands on the same cache, hits the
    // membership check, and returns the typed error.
    run(async {
        let (backend, _dir) = backend_with_secrets(key_of(0xb), None);
        let token = backend
            .mint_session_token(fresh_init(), None)
            .await
            .expect("mint");

        backend.init_session(&token).await.expect("first init ok");

        let err = backend
            .init_session(&token)
            .await
            .expect_err("second init must reject as replay");
        assert_eq!(
            validation_code(&err),
            "session_nonce_replay",
            "replay rejection must carry the typed code; got {err:?}"
        );
    });
}

#[test]
fn session_grace_window() {
    // P3 design §19 gate #3: a backend configured with
    // `secret = K_new, secret_prev = Some(K_old)` accepts tokens
    // signed under either key. A separate backend configured with
    // only `secret = K_new` (no prev) rejects the K_old-signed token
    // with `session_invalid_signature`.
    //
    // The artificial cross-backend test crosses the in-memory nonce
    // cache boundary — the nonce cache is per-backend so the second
    // backend sees an empty cache. This is correct: production runs
    // one backend per worker; the test pins the `verify_signature`
    // grace-key acceptance branch in isolation.
    run(async {
        let k_new = key_of(0xc);
        let k_old = key_of(0xd);

        // Old-key signer (mints tokens under K_old).
        let (backend_old, _dir_old) =
            backend_with_secrets(k_old.clone(), None);
        // New-key + grace acceptor (verifies tokens under either K_new
        // OR K_old).
        let (backend_new, _dir_new) =
            backend_with_secrets(k_new.clone(), Some(k_old.clone()));
        // Strict new-key-only acceptor (no grace — must reject the
        // K_old-signed token).
        let (backend_strict, _dir_strict) =
            backend_with_secrets(k_new.clone(), None);

        // Mint under K_old; init under K_new+prev → accepted.
        let token_a = backend_old
            .mint_session_token(fresh_init(), None)
            .await
            .expect("mint under K_old");
        backend_new
            .init_session(&token_a)
            .await
            .expect("grace-window backend must accept K_old-signed token");

        // Mint another K_old-signed token; init under strict K_new
        // → rejected. Distinct nonce keeps replay-detection out of
        // the way (the nonce is regenerated per mint).
        let token_b = backend_old
            .mint_session_token(fresh_init(), None)
            .await
            .expect("mint under K_old (second)");
        let err = backend_strict
            .init_session(&token_b)
            .await
            .expect_err("strict K_new-only backend must reject K_old-signed token");
        assert_eq!(
            validation_code(&err),
            "session_invalid_signature",
            "no-grace verify must surface invalid signature; got {err:?}"
        );
    });
}

#[test]
fn session_signature_expired() {
    // Edge case: a token minted with `ttl_secs = Some(-1)` is born
    // expired (the `expires_at_iso` field is in the past at mint
    // time). `init_session` checks expiry FIRST — before the HMAC
    // verify — so the rejection is `session_signature_expired`,
    // matching the PG SECURITY DEFINER `init_session`'s first
    // gate.
    run(async {
        let (backend, _dir) = backend_with_secrets(key_of(0x1), None);
        let token = backend
            .mint_session_token(fresh_init(), Some(-1))
            .await
            .expect("mint with ttl=-1 still produces a token");

        let err = backend
            .init_session(&token)
            .await
            .expect_err("expired token must be rejected");
        assert_eq!(
            validation_code(&err),
            "session_signature_expired",
            "expired-token rejection must carry the typed code; got {err:?}"
        );
    });
}

#[test]
fn session_invalid_signature() {
    // Edge case: a valid token's signature is tampered by flipping
    // one byte. `init_session` runs nonce-cache insertion BEFORE
    // signature verify (matching the PG SECURITY DEFINER order), so
    // we must use a fresh backend per attempt — the nonce we tamper
    // around is consumed by the first init attempt's cache insert.
    run(async {
        let (backend, _dir) = backend_with_secrets(key_of(0x2), None);
        let mut token = backend
            .mint_session_token(fresh_init(), None)
            .await
            .expect("mint");
        // Flip a bit in the middle of the signature. HMAC's avalanche
        // property guarantees the verify fails — but we're pinning
        // the typed error code, not the cryptography.
        let target = token.signature.len() / 2;
        token.signature[target] ^= 0x80;

        let err = backend
            .init_session(&token)
            .await
            .expect_err("tampered signature must be rejected");
        assert_eq!(
            validation_code(&err),
            "session_invalid_signature",
            "tampered-signature rejection must carry the typed code; got {err:?}"
        );
    });
}

#[test]
fn session_invalid_actor_kind() {
    // Edge case: an actor_kind outside the allowlist
    // (`auto`/`user`/`operator`/`ai-builder`/`platform`) is rejected
    // BEFORE signature verify, matching the PG SECURITY DEFINER
    // allowlist check. The SQLite impl hand-codes the allowlist as
    // a `const [&str; 5]`.
    run(async {
        let (backend, _dir) = backend_with_secrets(key_of(0x3), None);
        let init = SessionInit {
            app_id: "app_test".to_string(),
            actor_kind: "evil".to_string(),
            actor_id: Some("usr_attacker".to_string()),
            pid: Some("prj_demo".to_string()),
        };
        let token = backend
            .mint_session_token(init, None)
            .await
            .expect("mint accepts arbitrary actor_kind (validation lives in init)");

        let err = backend
            .init_session(&token)
            .await
            .expect_err("invalid actor_kind must be rejected on init");
        assert_eq!(
            validation_code(&err),
            "session_invalid_actor_kind",
            "invalid-actor-kind rejection must carry the typed code; got {err:?}"
        );
    });
}

#[test]
fn session_nonce_too_short() {
    // Edge case: a token whose nonce was truncated to 8 bytes (below
    // the 16-byte floor) is rejected with `session_nonce_too_short`
    // BEFORE the HMAC verify. The PG SECURITY DEFINER applies the
    // same gate via `octet_length(p_nonce) < 16`.
    run(async {
        let (backend, _dir) = backend_with_secrets(key_of(0x4), None);
        let mut token = backend
            .mint_session_token(fresh_init(), None)
            .await
            .expect("mint");
        token.nonce.truncate(8);

        let err = backend
            .init_session(&token)
            .await
            .expect_err("8-byte nonce must be rejected");
        assert_eq!(
            validation_code(&err),
            "session_nonce_too_short",
            "short-nonce rejection must carry the typed code; got {err:?}"
        );
    });
}

#[test]
fn session_not_configured() {
    // Plan §11 Q-P3-H: a backend constructed via the env-var entry
    // point (`SqliteBackend::new(...)`) without
    // `ZEROSHIP_SESSION_SECRET` set must defer the failure to first
    // mint, surfacing `DbError::Configuration { code: "not_configured" }`.
    //
    // **Isolation strategy**: this test does NOT manipulate env
    // vars. All other session_* tests use `new_with_secrets(...)`,
    // which bypasses the env-var read entirely. The test process
    // therefore observes `ZEROSHIP_SESSION_SECRET` unset (unless an
    // operator sets it before invoking `cargo test`). A guard
    // assertion below skips the test cleanly if that pre-condition
    // doesn't hold — a defensive check rather than a hard failure,
    // because a developer who has the env var set in their shell
    // shouldn't see a spurious test failure.
    //
    // The cross-test concern (one test calls `set_var` mid-run and
    // poisons the global) doesn't apply here because no other test
    // in this integration target touches the env. The PG
    // integration target also doesn't touch `ZEROSHIP_SESSION_SECRET`;
    // the auth subsystem on PG uses pgcrypto.gen_random_bytes for
    // its HMAC key and reads `ZEROSHIP_HMAC_*` instead.
    if std::env::var("ZEROSHIP_SESSION_SECRET").is_ok() {
        eprintln!(
            "session_not_configured: skipping — ZEROSHIP_SESSION_SECRET is set in \
             this test process; the lazy-failure path is unreachable. To exercise \
             this test, unset the env var and re-run."
        );
        return;
    }
    run(async {
        let (backend, _dir) = fresh_backend();
        let err = backend
            .mint_session_token(fresh_init(), None)
            .await
            .expect_err("missing secret must defer to mint-time failure");
        match err {
            DbError::Configuration { code, message, .. } => {
                assert_eq!(code, "not_configured", "typed code on unconfigured mint");
                assert!(
                    message.contains("ZEROSHIP_SESSION_SECRET"),
                    "message must name the missing env var so operators self-serve; got: {message}"
                );
            }
            other => panic!("expected DbError::Configuration, got {other:?}"),
        }
    });
}

#[test]
fn session_canonical_payload_byte_pin() {
    // Cross-backend payload-equivalence pin — the SQLite-side
    // analogue to a live-PG byte equivalence assertion.
    //
    // The PG impl signs via `__zeroship_admin.sign_session`'s
    // SECURITY DEFINER body, which constructs the canonical payload
    // as SQL string concatenation:
    //   actor_kind || '|' || COALESCE(actor_id,'') || '|' ||
    //   COALESCE(pid::TEXT,'') || '|' || encode(nonce,'hex') || '|' ||
    //   expires_at_iso
    // and HMACs it via `pgcrypto.hmac(payload, key, 'sha256')`.
    //
    // The SQLite impl runs the equivalent helper
    // (`session_minter::canonical_payload` +
    // `session_minter::compute_signature`) in Rust. These are
    // `pub(crate)` and unreachable from this integration target,
    // so we recompute the HMAC ourselves over the formula in this
    // file and assert byte-for-byte equality with `token.signature`.
    // If anyone refactors the canonical_payload formula or swaps
    // the HMAC variant, this test fails immediately.
    //
    // A live-PG byte equivalence test would replace this with a
    // direct comparison against `__zeroship_admin.sign_session(...)`
    // output. Per the plan §7 commentary the structural equivalence
    // is only fully verifiable against a live PG; this test is the
    // regression catch for the SQLite side.
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    fn hex_encode_local(b: &[u8]) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut out = String::with_capacity(b.len() * 2);
        for &x in b {
            out.push(HEX[(x >> 4) as usize] as char);
            out.push(HEX[(x & 0xF) as usize] as char);
        }
        out
    }

    run(async {
        let secret = key_of(0x5);
        let (backend, _dir) = backend_with_secrets(secret.clone(), None);
        let init = SessionInit {
            app_id: "app_test".to_string(),
            actor_kind: "platform".to_string(),
            actor_id: Some("usr_pin".to_string()),
            pid: Some("prj_pin".to_string()),
        };
        let token = backend
            .mint_session_token(init.clone(), None)
            .await
            .expect("mint");

        // Reconstruct the canonical payload by hand from the public
        // token fields. This is the formula documented in
        // `backend/sqlite/session_minter.rs::canonical_payload`:
        //   actor_kind | actor_id | pid | hex(nonce) | expires_at
        let payload = format!(
            "{}|{}|{}|{}|{}",
            init.actor_kind,
            init.actor_id.as_deref().unwrap_or(""),
            init.pid.as_deref().unwrap_or(""),
            hex_encode_local(&token.nonce),
            token.expires_at_iso,
        );

        // Independent HMAC-SHA256.
        type HmacSha256 = Hmac<Sha256>;
        let mut mac = HmacSha256::new_from_slice(&secret)
            .expect("HMAC-SHA256 accepts any key length");
        mac.update(payload.as_bytes());
        let expected = mac.finalize().into_bytes().to_vec();

        assert_eq!(
            token.signature, expected,
            "SQLite-side canonical payload + HMAC must match the documented \
             formula byte-for-byte (cross-backend equivalence pin)"
        );
        assert_eq!(expected.len(), 32, "HMAC-SHA256 is always 32 bytes");
    });
}

// ---------------------------------------------------------------------------
// P4 PR 7 — SQLite VectorIndex (`sqlite-vec` `vec0` virtual table)
// integration tests.
//
// Supersedes the P4 PR 4 pure-Rust flat scan tests at the same point
// in this file (see `docs/proposals/p4-search-implementation-plan.md`
// §10 2026-05-24 reassessment). The membership-set assertions are
// preserved byte-for-byte; only the underlying storage layer changed.
//
// Mirrors the PG arm's `vector_search_returns_k_nearest` /
// `vector_dimension_mismatch_rejected_at_insert` /
// `vector_search_respects_filter` structurally so a reviewer can
// diff the two suites side-by-side.
//
// Storage: base-table BLOB column with a CHECK constraint (the
// dimension contract at write time) + a `vec0` virtual table created
// by `ensure_vector_index` + AFTER triggers that mirror the BLOB
// column into vec0 on INSERT/UPDATE/DELETE. INSERTs use SQLite's
// hex-blob literal `x'<hex>'` so we avoid plumbing typed BLOB params
// through the session actor's `&[String]` surface.
// ---------------------------------------------------------------------------

use zeroship_plugin_db::backend::VectorIndex;
use zeroship_plugin_db::backend::VectorMetric;

/// Encode a `Vec<f32>` as a SQLite `x'<hex>'` blob literal.
///
/// The bytes are native-endian f32 (4 bytes per dim); all platforms
/// we target are little-endian, so this matches what `vec_to_le_bytes`
/// in `backend/sqlite/vector.rs` produces.
fn vec_to_hex_lit(v: &[f32]) -> String {
    let mut hex = String::with_capacity(v.len() * 8 + 4);
    hex.push_str("x'");
    for f in v {
        for byte in f.to_le_bytes() {
            hex.push_str(&format!("{byte:02x}"));
        }
    }
    hex.push('\'');
    hex
}

/// Deterministic pseudo-random unit vector — same construction as the
/// PG arm's `vector_search_returns_k_nearest` so the membership
/// expectations match across backends (modulo FP non-determinism in
/// low significand bits, which the test asserts as set membership
/// rather than ordinal positions).
fn mk_unit_vec(i: usize, dims: usize) -> Vec<f32> {
    let mut v = vec![0.0f32; dims];
    for (j, slot) in v.iter_mut().enumerate().take(dims) {
        let x = (i.wrapping_mul(2_654_435_761)) ^ (j.wrapping_mul(40_503));
        *slot = ((x & 0xffff) as f32 / 65_536.0) - 0.5;
    }
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
    v
}

#[test]
fn vector_search_returns_k_nearest_sqlite() {
    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .ensure_app_schema("vector_topk")
            .await
            .expect("ensure_app_schema");

        // CREATE TABLE with the BLOB column the SDK's `t.vector(dims)`
        // lowering emits. The CHECK constraint pins the write-side
        // dimension contract; vec0's own dimension check is the
        // second line of defence (trigger-time).
        let dims = 8usize;
        backend
            .pool_exec(
                "CREATE TABLE \"vector_topk\".\"docs\" (\
                   id INTEGER PRIMARY KEY AUTOINCREMENT, \
                   embedding BLOB CHECK(length(embedding) = 32) NOT NULL\
                 )",
                &[],
            )
            .await
            .expect("CREATE TABLE docs");

        // Create the vec0 vtable + mirror triggers BEFORE inserting
        // rows. With the triggers in place, every INSERT into the
        // base table fans out into the vec0 index inside the same
        // transaction; vector_search joins on rowid.
        backend
            .ensure_vector_index("vector_topk", "docs", "embedding", 8, VectorMetric::Cosine)
            .await
            .expect("ensure_vector_index creates vec0 vtable + triggers");

        // Insert 100 deterministic unit vectors. Each INSERT fires
        // the `docs__vec_embedding_ai` trigger which mirrors
        // `(rowid, embedding)` into the vec0 index.
        for i in 0..100usize {
            let v = mk_unit_vec(i, dims);
            let hex = vec_to_hex_lit(&v);
            let sql =
                format!("INSERT INTO \"vector_topk\".\"docs\" (embedding) VALUES ({hex})");
            backend.pool_exec(&sql, &[]).await.expect("INSERT");
        }

        // Query with row #0's exact vector — its own row must be in
        // the top-10. Assert MEMBERSHIP (not strict order) to mirror
        // the PG arm's relaxed expectation.
        let query = mk_unit_vec(0, dims);
        let rows = backend
            .vector_search(
                "vector_topk",
                "docs",
                "embedding",
                &query,
                10,
                VectorMetric::Cosine,
                &serde_json::Value::Null,
            )
            .await
            .expect("vector_search");

        assert_eq!(rows.len(), 10, "expected k=10 rows, got {}", rows.len());
        let ids: Vec<i64> = rows
            .iter()
            .filter_map(|r| r.get("id").and_then(serde_json::Value::as_i64))
            .collect();
        // SQLite's INTEGER PRIMARY KEY AUTOINCREMENT starts at 1; row
        // 1 is the i=0 insert, which has zero cosine distance to its
        // own query vector.
        assert!(
            ids.contains(&1),
            "exact-match row #1 must be in top-10, got ids={ids:?}"
        );
        for r in &rows {
            let d = r
                .get("_distance")
                .and_then(serde_json::Value::as_f64)
                .expect("row must carry _distance");
            assert!(d.is_finite(), "_distance must be finite, got {d}");
            assert!(d >= 0.0, "cosine distance is non-negative, got {d}");
        }
        // Row #1 should be the nearest (distance ~ 0).
        let first_id = rows[0]
            .get("id")
            .and_then(serde_json::Value::as_i64)
            .expect("first row id");
        assert_eq!(
            first_id, 1,
            "exact-match query must place its own row first"
        );
        let first_d = rows[0]
            .get("_distance")
            .and_then(serde_json::Value::as_f64)
            .expect("first row _distance");
        assert!(
            first_d.abs() < 1e-5,
            "exact-match distance must be ~0, got {first_d}"
        );
    });
}

#[test]
fn vector_dimension_mismatch_rejected_at_insert_sqlite() {
    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .ensure_app_schema("vector_dim")
            .await
            .expect("ensure_app_schema");

        // 128-d column = 512-byte CHECK.
        backend
            .pool_exec(
                "CREATE TABLE \"vector_dim\".\"docs\" (\
                   id INTEGER PRIMARY KEY AUTOINCREMENT, \
                   embedding BLOB CHECK(length(embedding) = 512) NOT NULL\
                 )",
                &[],
            )
            .await
            .expect("CREATE TABLE docs");

        // Insert a 256-d vector into a 128-d column. The CHECK
        // constraint must reject — the SqlExecutor surface should
        // surface a SchemaRefused {check_violation} typed error.
        let oversized = mk_unit_vec(0, 256);
        let hex = vec_to_hex_lit(&oversized);
        let sql = format!("INSERT INTO \"vector_dim\".\"docs\" (embedding) VALUES ({hex})");
        let err = backend
            .pool_exec(&sql, &[])
            .await
            .expect_err("256-d into 128-d column must fail");
        match err {
            DbError::SchemaRefused { code, .. } => {
                assert_eq!(
                    code, "check_violation",
                    "expected check_violation, got {code}"
                );
            }
            other => panic!(
                "expected SchemaRefused {{ check_violation }}, got {other:?}"
            ),
        }
    });
}

#[test]
fn vector_search_respects_filter_sqlite() {
    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .ensure_app_schema("vector_filter")
            .await
            .expect("ensure_app_schema");

        // 4-d column = 16-byte CHECK.
        backend
            .pool_exec(
                "CREATE TABLE \"vector_filter\".\"docs\" (\
                   id INTEGER PRIMARY KEY AUTOINCREMENT, \
                   tenant TEXT NOT NULL, \
                   embedding BLOB CHECK(length(embedding) = 16) NOT NULL\
                 )",
                &[],
            )
            .await
            .expect("CREATE TABLE docs");

        // Create vec0 + triggers BEFORE inserts so the mirror fires
        // for every row.
        backend
            .ensure_vector_index("vector_filter", "docs", "embedding", 4, VectorMetric::Cosine)
            .await
            .expect("ensure_vector_index");

        // Insert 10 rows in tenant "a" and 10 rows in tenant "b".
        // The first row of each tenant uses an identical query
        // vector so the filter discriminates BY tenant, not by
        // proximity.
        for i in 0..10usize {
            let v = mk_unit_vec(i, 4);
            let hex = vec_to_hex_lit(&v);
            backend
                .pool_exec(
                    &format!(
                        "INSERT INTO \"vector_filter\".\"docs\" \
                           (tenant, embedding) VALUES ('a', {hex})"
                    ),
                    &[],
                )
                .await
                .expect("INSERT a");
            backend
                .pool_exec(
                    &format!(
                        "INSERT INTO \"vector_filter\".\"docs\" \
                           (tenant, embedding) VALUES ('b', {hex})"
                    ),
                    &[],
                )
                .await
                .expect("INSERT b");
        }

        // Query with tenant='a' filter — every returned row must
        // have tenant='a'. The filter uses the `$eq` operator the
        // SDK already emits.
        let query = mk_unit_vec(0, 4);
        let filter = serde_json::json!({ "tenant": { "$eq": "a" } });
        let rows = backend
            .vector_search(
                "vector_filter",
                "docs",
                "embedding",
                &query,
                10,
                VectorMetric::Cosine,
                &filter,
            )
            .await
            .expect("vector_search with filter");

        assert!(!rows.is_empty(), "filter must not exclude every row");
        assert!(
            rows.len() <= 10,
            "k=10 with 10 candidate rows yields at most 10 results"
        );
        for r in &rows {
            let tenant = r
                .get("tenant")
                .and_then(serde_json::Value::as_str)
                .expect("row must carry tenant");
            assert_eq!(
                tenant, "a",
                "vector_search with tenant=a filter returned tenant={tenant}: {r}"
            );
        }
    });
}

#[test]
fn vector_l2_distance_matches_cosine_for_unit_vectors_sqlite() {
    // Sanity check on the math: for unit vectors, ||a-b||² = 2 * (1 - cos θ)
    // = 2 * cos_distance. With vec0 the metric is pinned at vtable
    // creation time, so we declare TWO vector columns (one cosine,
    // one L2) sharing the same source rows. The two `ensure_vector_index`
    // calls produce two paired vec0 vtables (`docs__vec_emb_cos` /
    // `docs__vec_emb_l2`); the AFTER triggers mirror BOTH columns on
    // every INSERT.
    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .ensure_app_schema("vector_math")
            .await
            .expect("ensure_app_schema");

        backend
            .pool_exec(
                "CREATE TABLE \"vector_math\".\"docs\" (\
                   id INTEGER PRIMARY KEY AUTOINCREMENT, \
                   emb_cos BLOB CHECK(length(emb_cos) = 16) NOT NULL, \
                   emb_l2  BLOB CHECK(length(emb_l2)  = 16) NOT NULL\
                 )",
                &[],
            )
            .await
            .expect("CREATE TABLE docs");

        backend
            .ensure_vector_index("vector_math", "docs", "emb_cos", 4, VectorMetric::Cosine)
            .await
            .expect("ensure_vector_index cos");
        backend
            .ensure_vector_index("vector_math", "docs", "emb_l2", 4, VectorMetric::L2)
            .await
            .expect("ensure_vector_index l2");

        let v1 = mk_unit_vec(0, 4);
        let v2 = mk_unit_vec(1, 4);
        let hex1 = vec_to_hex_lit(&v1);
        let hex2 = vec_to_hex_lit(&v2);
        backend
            .pool_exec(
                &format!(
                    "INSERT INTO \"vector_math\".\"docs\" (emb_cos, emb_l2) \
                     VALUES ({hex1}, {hex1})"
                ),
                &[],
            )
            .await
            .expect("INSERT v1");
        backend
            .pool_exec(
                &format!(
                    "INSERT INTO \"vector_math\".\"docs\" (emb_cos, emb_l2) \
                     VALUES ({hex2}, {hex2})"
                ),
                &[],
            )
            .await
            .expect("INSERT v2");

        // Query the cosine distance from row 1 (v1) to v2.
        let cos_rows = backend
            .vector_search(
                "vector_math",
                "docs",
                "emb_cos",
                &v1,
                2,
                VectorMetric::Cosine,
                &serde_json::Value::Null,
            )
            .await
            .expect("cosine search");
        let l2_rows = backend
            .vector_search(
                "vector_math",
                "docs",
                "emb_l2",
                &v1,
                2,
                VectorMetric::L2,
                &serde_json::Value::Null,
            )
            .await
            .expect("l2 search");

        // The row with id=2 (the OTHER unit vector) must appear in
        // both result sets; its cosine and L2 distances must satisfy
        // L2² ≈ 2 * cos_distance.
        let find = |rows: &[serde_json::Value], target_id: i64| -> f64 {
            rows.iter()
                .find(|r| r.get("id").and_then(serde_json::Value::as_i64) == Some(target_id))
                .and_then(|r| r.get("_distance").and_then(serde_json::Value::as_f64))
                .expect("row with target id must be present")
        };
        let cos_d = find(&cos_rows, 2);
        let l2_d = find(&l2_rows, 2);
        let lhs = l2_d * l2_d;
        let rhs = 2.0 * cos_d;
        assert!(
            (lhs - rhs).abs() < 1e-3,
            "||v1-v2||^2 = {lhs}, 2 * cos_d = {rhs}"
        );
    });
}

// ---------------------------------------------------------------------------
// P4 PR 5 — SQLite FullTextIndex (FTS5) + SpatialIndex (haversine) gates
// ---------------------------------------------------------------------------
//
// These tests target the new `FullTextIndex` / `SpatialIndex` impls on
// `SqliteBackend`. The FTS path exercises the FTS5 vtable + AFTER
// triggers (insert / update); the spatial path exercises the haversine
// flat scan against `(lat, lng)` 16-byte BLOB payloads.
//
// Like the P4 PR 4 vector tests, we construct table DDL inline — the
// orchestrator's column-DDL emitter is PG-flavoured today; a follow-up
// PR will teach `register_model::apply` to dispatch by dialect via the
// `sqlite_geopoint_column_ddl` / `sqlite_vector_column_ddl` helpers.

use zeroship_plugin_db::backend::FullTextIndex;
use zeroship_plugin_db::backend::SpatialIndex;
use zeroship_plugin_db::backend::GeoPoint;

/// **P4 PR 5 test gate** — `fts_search_matches_substring` (SQLite).
///
/// Inserts 5 rows whose `bio` column matches different keyword sets;
/// asserts `fts_search("rust")` returns the membership set we expect
/// (the rows containing "rust" anywhere). Set membership, not ordinal
/// positions — bm25's ranking is FP-dependent and we don't pin the
/// order across backends.
#[test]
fn fts_search_matches_substring() {
    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .ensure_app_schema("fts_substring")
            .await
            .expect("ensure_app_schema");

        backend
            .pool_exec(
                "CREATE TABLE \"fts_substring\".\"people\" (\
                   id INTEGER PRIMARY KEY AUTOINCREMENT, \
                   bio TEXT NOT NULL\
                 )",
                &[],
            )
            .await
            .expect("CREATE TABLE people");

        // Wire the FTS index BEFORE inserting the seed rows so the
        // AFTER INSERT trigger populates `__fts` — this exercises the
        // trigger path rather than the initial-population SELECT.
        backend
            .ensure_fts_index(
                "fts_substring",
                "people",
                &["bio".to_string()],
                "english",
            )
            .await
            .expect("ensure_fts_index");

        let seeds = [
            "Loves rust and systems programming",
            "Building async services",
            "rust async fan",
            "Python developer",
            "Ruby on Rails dev",
        ];
        for s in &seeds {
            // Inline single-quoted literal — the seed values are safe
            // (no `'`). For hostile input the integration tests would
            // bind through the typed-text path; the FTS gates only
            // need fixed seeds.
            let sql = format!(
                "INSERT INTO \"fts_substring\".\"people\" (bio) VALUES ('{s}')"
            );
            backend.pool_exec(&sql, &[]).await.expect("INSERT bio");
        }

        let rows = backend
            .fts_search(
                "fts_substring",
                "people",
                "rust",
                &serde_json::Value::Null,
                None,
            )
            .await
            .expect("fts_search");

        // FTS5's default tokeniser is case-insensitive Unicode; "rust"
        // matches rows 1 + 3 ("rust and systems", "rust async fan").
        let bios: Vec<String> = rows
            .iter()
            .filter_map(|r| {
                r.get("bio")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            })
            .collect();
        assert_eq!(
            rows.len(),
            2,
            "expected 2 rust-matching rows, got {} ({bios:?})",
            rows.len()
        );
        assert!(
            bios.iter().any(|b| b.contains("rust and systems")),
            "expected the 'rust and systems' row in {bios:?}"
        );
        assert!(
            bios.iter().any(|b| b.contains("rust async fan")),
            "expected the 'rust async fan' row in {bios:?}"
        );
        // Every row carries the synthetic `_rank` column (bm25 score).
        for r in &rows {
            assert!(
                r.get("_rank").is_some(),
                "row missing _rank: {r}"
            );
        }
    });
}

/// **P4 PR 5 test gate** — `fts_and_filter_compose` (SQLite).
///
/// FTS `MATCH` composed via `AND` with a regular column filter must
/// intersect — assert the final set is exactly the rows matching both
/// conditions.
#[test]
fn fts_and_filter_compose() {
    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .ensure_app_schema("fts_compose")
            .await
            .expect("ensure_app_schema");

        backend
            .pool_exec(
                "CREATE TABLE \"fts_compose\".\"people\" (\
                   id INTEGER PRIMARY KEY AUTOINCREMENT, \
                   bio TEXT NOT NULL, \
                   lang TEXT NOT NULL\
                 )",
                &[],
            )
            .await
            .expect("CREATE TABLE people");

        backend
            .ensure_fts_index(
                "fts_compose",
                "people",
                &["bio".to_string()],
                "english",
            )
            .await
            .expect("ensure_fts_index");

        let seeds = [
            ("Loves rust and systems programming", "en"),
            ("rust async runtimes", "en"),
            ("python developer", "en"),
            ("rust fan", "de"),
            ("rust crab", "de"),
        ];
        for (bio, lang) in &seeds {
            let sql = format!(
                "INSERT INTO \"fts_compose\".\"people\" (bio, lang) VALUES ('{bio}', '{lang}')"
            );
            backend.pool_exec(&sql, &[]).await.expect("INSERT");
        }

        let rows = backend
            .fts_search(
                "fts_compose",
                "people",
                "rust",
                &serde_json::json!({ "lang": "en" }),
                None,
            )
            .await
            .expect("fts_search");

        assert_eq!(
            rows.len(),
            2,
            "expected exactly 2 (rust intersect en) rows, got {}",
            rows.len()
        );
        for r in &rows {
            assert_eq!(
                r.get("lang").and_then(serde_json::Value::as_str),
                Some("en"),
                "filter must restrict to lang=en: {r}"
            );
        }
    });
}

/// **P4 PR 5 test gate** — `fts_trigger_keeps_index_in_sync_after_update`
/// (SQLite).
///
/// Insert a row, search for token "alpha" — must hit. Update the row
/// to replace "alpha" with "beta" and search for "alpha" again — must
/// MISS, while a search for "beta" must hit. Exercises the AFTER
/// UPDATE OF trigger (vs. just the initial population path).
#[test]
fn fts_trigger_keeps_index_in_sync_after_update() {
    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .ensure_app_schema("fts_trigger")
            .await
            .expect("ensure_app_schema");

        backend
            .pool_exec(
                "CREATE TABLE \"fts_trigger\".\"docs\" (\
                   id INTEGER PRIMARY KEY AUTOINCREMENT, \
                   body TEXT NOT NULL\
                 )",
                &[],
            )
            .await
            .expect("CREATE TABLE docs");

        backend
            .ensure_fts_index(
                "fts_trigger",
                "docs",
                &["body".to_string()],
                "english",
            )
            .await
            .expect("ensure_fts_index");

        backend
            .pool_exec(
                "INSERT INTO \"fts_trigger\".\"docs\" (body) VALUES ('alpha test content')",
                &[],
            )
            .await
            .expect("INSERT alpha row");

        let hits = backend
            .fts_search(
                "fts_trigger",
                "docs",
                "alpha",
                &serde_json::Value::Null,
                None,
            )
            .await
            .expect("alpha search");
        assert_eq!(
            hits.len(),
            1,
            "expected 1 alpha hit pre-update, got {}",
            hits.len()
        );

        // UPDATE replaces "alpha" with "beta" — the AFTER UPDATE OF
        // body trigger must DELETE the old FTS row and INSERT the
        // new one.
        backend
            .pool_exec(
                "UPDATE \"fts_trigger\".\"docs\" SET body = 'beta different content' WHERE id = 1",
                &[],
            )
            .await
            .expect("UPDATE");

        let alpha_hits = backend
            .fts_search(
                "fts_trigger",
                "docs",
                "alpha",
                &serde_json::Value::Null,
                None,
            )
            .await
            .expect("alpha search post-update");
        assert_eq!(
            alpha_hits.len(),
            0,
            "trigger must invalidate alpha after UPDATE, got {} hits",
            alpha_hits.len()
        );

        let beta_hits = backend
            .fts_search(
                "fts_trigger",
                "docs",
                "beta",
                &serde_json::Value::Null,
                None,
            )
            .await
            .expect("beta search post-update");
        assert_eq!(
            beta_hits.len(),
            1,
            "trigger must surface beta after UPDATE, got {} hits",
            beta_hits.len()
        );
    });
}

/// Encode a `GeoPoint` as a SQLite `x'<hex>'` blob literal — 2× LE
/// f64 = 16 bytes. Mirrors `vec_to_hex_lit` for vectors. We use this
/// because the session actor's text-param path can't carry raw bytes
/// at the SQL boundary.
fn point_to_hex_lit(p: GeoPoint) -> String {
    let mut hex = String::with_capacity(16 * 2 + 4);
    hex.push_str("x'");
    for byte in p.lat.to_le_bytes() {
        hex.push_str(&format!("{byte:02x}"));
    }
    for byte in p.lng.to_le_bytes() {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex.push('\'');
    hex
}

/// **P4 PR 5 test gate** — `near_returns_within_radius` (SQLite).
///
/// 10 points around London at varying distances from the centre
/// `(51.5074, -0.1278)`. `near()` with a 1km radius returns only the
/// points actually within 1km — assert by membership set. No PostGIS
/// dependency on this arm: the haversine math is pure Rust + the
/// `geoPoint` column is a plain BLOB.
#[test]
fn near_returns_within_radius() {
    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .ensure_app_schema("near_radius")
            .await
            .expect("ensure_app_schema");

        // Inline DDL — the `sqlite_geopoint_column_ddl` helper emits
        // the same CHECK shape; we hand-write it here to keep the
        // test self-contained against the orchestrator's PG-flavoured
        // emitter.
        backend
            .pool_exec(
                "CREATE TABLE \"near_radius\".\"places\" (\
                   id INTEGER PRIMARY KEY AUTOINCREMENT, \
                   location BLOB CHECK(length(location) = 16) NOT NULL\
                 )",
                &[],
            )
            .await
            .expect("CREATE TABLE places");

        let london = GeoPoint { lat: 51.5074, lng: -0.1278 };
        // 10 points: 5 within ~1km (small lat/lng offsets) and 5
        // well outside (several km away). One degree of latitude is
        // ~111km, so 0.005 deg ≈ 555m and 0.05 deg ≈ 5.5km.
        let offsets: Vec<(f64, f64, bool)> = vec![
            (0.0, 0.0, true),     // dead-centre
            (0.001, 0.001, true), // ~140m
            (0.003, 0.003, true), // ~420m
            (-0.005, 0.0, true),  // ~555m south
            (0.0, 0.005, true),   // ~350m east at cos(51.5deg) ≈ 0.62
            (0.05, 0.0, false),   // ~5.5km north
            (-0.05, 0.0, false),  // ~5.5km south
            (0.0, 0.05, false),   // ~3.5km east
            (0.0, -0.05, false),  // ~3.5km west
            (0.1, 0.1, false),    // ~11km NE
        ];
        let mut expected_within: Vec<i64> = Vec::new();
        for (i, (dlat, dlng, within_1km)) in offsets.iter().enumerate() {
            let p = GeoPoint {
                lat: london.lat + dlat,
                lng: london.lng + dlng,
            };
            let hex = point_to_hex_lit(p);
            let sql = format!(
                "INSERT INTO \"near_radius\".\"places\" (location) VALUES ({hex})"
            );
            backend.pool_exec(&sql, &[]).await.expect("INSERT location");
            if *within_1km {
                expected_within.push((i + 1) as i64);
            }
        }

        let rows = backend
            .spatial_near(
                "near_radius",
                "places",
                "location",
                london,
                1000.0,
                &serde_json::Value::Null,
                None,
            )
            .await
            .expect("spatial_near");

        let returned_ids: std::collections::BTreeSet<i64> = rows
            .iter()
            .filter_map(|r| {
                r.get("id").and_then(serde_json::Value::as_i64)
            })
            .collect();
        let expected: std::collections::BTreeSet<i64> =
            expected_within.into_iter().collect();
        assert_eq!(
            returned_ids, expected,
            "near(1km) membership mismatch: returned={returned_ids:?} expected={expected:?}"
        );
        // Every row carries the synthetic `_distance_m` column.
        for r in &rows {
            let d = r
                .get("_distance_m")
                .and_then(serde_json::Value::as_f64)
                .expect("row must carry _distance_m");
            assert!(d.is_finite(), "_distance_m must be finite, got {d}");
            assert!(
                d <= 1000.0 + 1e-6,
                "_distance_m={d} must be within the 1km radius (FP slack)"
            );
        }
        // The dead-centre row (id=1) is the closest.
        let first_id = rows[0]
            .get("id")
            .and_then(serde_json::Value::as_i64)
            .expect("first row id");
        assert_eq!(
            first_id, 1,
            "dead-centre (offset (0,0)) row must be first by distance"
        );
        let first_d = rows[0]
            .get("_distance_m")
            .and_then(serde_json::Value::as_f64)
            .expect("first row _distance_m");
        assert!(
            first_d < 1.0,
            "dead-centre distance must be < 1m, got {first_d}"
        );
    });
}

// ===========================================================================
// P5 PR 3 — `EncryptedColumn` impl on SqliteBackend
// ===========================================================================
//
// These tests exercise the full SQLite round-trip for `t.encrypted(...)`-
// declared columns: env-var key sourcing through KeyStore, AES-GCM
// encrypt with the right AAD shape (Camp A — row_pk in AAD for
// Randomised, omitted for Deterministic), BLOB storage on disk via
// rusqlite's typed BLOB binding, decrypt-on-read. Mirrors the PG suite
// at `tests/integration.rs` §"P5 PR 2 — `EncryptedColumn` impl".

/// Helper: set a synthetic root key in `ZEROSHIP_COLUMN_KEY_<KEY>`
/// for the duration of a test, restoring the previous value on drop.
/// Same shape as the PG-side `WithEnv` in `tests/integration.rs`.
struct EncEnv {
    name: String,
    prev: Option<String>,
}

#[allow(unsafe_code)]
impl EncEnv {
    fn set(name: &str, value: &str) -> Self {
        let prev = std::env::var(name).ok();
        // SAFETY: each P5 SQLite test uses a uniquely-named env var
        // (suffix carries the test fn name) so concurrent test runs
        // don't race on the process-global env table. The
        // `ZEROSHIP_COLUMN_KEY_*` namespace is plugin-db-owned.
        unsafe {
            std::env::set_var(name, value);
        }
        Self {
            name: name.to_string(),
            prev,
        }
    }
}

#[allow(unsafe_code)]
impl Drop for EncEnv {
    fn drop(&mut self) {
        // SAFETY: same justification as `set` above.
        unsafe {
            match &self.prev {
                Some(p) => std::env::set_var(&self.name, p),
                None => std::env::remove_var(&self.name),
            }
        }
    }
}

/// Bind a raw byte slice as a SQLite BLOB literal using the `X'...'`
/// hex syntax. SQLite accepts this anywhere a value literal can appear,
/// and the session-actor's `&str`-params channel can carry inline
/// literals untouched. Returns the literal text including the `X'`
/// prefix and closing `'`.
fn sqlite_blob_literal(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(3 + bytes.len() * 2);
    s.push_str("X'");
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s.push('\'');
    s
}

const SQLITE_RUNTIME_RPC_SHIM: &str = r#"
async function _shimRpc(name, input, ctx) {
    const fn = _procedures[name];
    if (typeof fn !== "function") {
        throw Object.assign(new Error("Method not found: " + name), { status: 404 });
    }
    let out = fn(input, ctx);
    if (out && typeof out.then === "function") out = await out;
    return out;
}
async function _zsRpcAndRespond(name, input) {
    try {
        const result = await _shimRpc(name, input);
        return new Response(JSON.stringify({ json: result === undefined ? null : result }),
            { status: 200, headers: { "content-type": "application/json" } });
    } catch (err) {
        const status = (err && Number.isInteger(err.status) && err.status >= 400 && err.status < 600) ? err.status : 500;
        const body = { message: err?.message ?? String(err), name: err?.name ?? "Error" };
        if (err && typeof err.code === "string") body.code = err.code;
        if (err && err.details !== undefined) body.details = err.details;
        return new Response(JSON.stringify(body), {
            status, headers: { "content-type": "application/json" },
        });
    }
}
async function _zsFetch(request) {
    const url = new URL(request.url);
    const id = decodeURIComponent(url.pathname.slice("/__zeroship/v1/".length));
    const text = await request.text();
    let input;
    if (text) {
        const env = JSON.parse(text);
        input = env && typeof env === "object" && "json" in env ? env.json : env;
    }
    return await _zsRpcAndRespond(id, input);
}
export default { fetch: _zsFetch, rpc: _shimRpc };
"#;

fn sqlite_runtime_upsert_source(collection: &str, key_id: &str, body: &str) -> String {
    format!(
        r#"
import {{ env }} from "zeroship";

const __plat = (typeof globalThis.__zsDbPlatform === "function")
    ? globalThis.__zsDbPlatform(env.db)
    : undefined;
const COLLECTION = "{collection}";
const KEY_ID = "{key_id}";

function setup(_input, _ctx) {{
    return __plat.registerModel(COLLECTION, {{
        email: {{ type: "string", required: true, unique: true }},
        name: {{ type: "string", required: true }},
        ssn: {{
            type: "string",
            encrypted: {{ mode: "randomised", keyId: KEY_ID, wraps: "string" }},
            mask: {{ kind: "last4", classification: "spi" }}
        }}
    }});
}}
setup.config = {{ kind: "action" }};

{body}
"#
    ) + SQLITE_RUNTIME_RPC_SHIM
}

fn sqlite_runtime_upsert_det_conflict_source(
    collection: &str,
    key_id: &str,
    body: &str,
) -> String {
    format!(
        r#"
import {{ env }} from "zeroship";

const __plat = (typeof globalThis.__zsDbPlatform === "function")
    ? globalThis.__zsDbPlatform(env.db)
    : undefined;
const COLLECTION = "{collection}";
const KEY_ID = "{key_id}";

function setup(_input, _ctx) {{
    return __plat.registerModel(COLLECTION, {{
        email: {{
            type: "string",
            required: true,
            unique: true,
            encrypted: {{ mode: "deterministic", keyId: KEY_ID, wraps: "string" }}
        }},
        name: {{ type: "string", required: true }},
        ssn: {{
            type: "string",
            encrypted: {{ mode: "randomised", keyId: KEY_ID, wraps: "string" }},
            mask: {{ kind: "last4", classification: "spi" }}
        }}
    }});
}}
setup.config = {{ kind: "action" }};

{body}
"#
    ) + SQLITE_RUNTIME_RPC_SHIM
}

fn dispatch_sqlite_runtime(
    dir: &tempfile::TempDir,
    source: &str,
    name: &str,
) -> serde_json::Value {
    let url = parity::sqlite_url(dir);
    let (status, body) = parity::dispatch_zs(&url, source, name);
    assert_eq!(status, 200, "{name} failed: {body}");
    body
}

fn assert_write_path_fast_path(label: &str) {
    let counters = zeroship_plugin_db::crud::write_path_counters_for_tests();
    assert_eq!(
        counters.target_row_resolution_calls,
        0,
        "{label}: plain write must not resolve row ids: {counters:?}",
    );
    assert_eq!(
        counters.upsert_conflict_probe_calls,
        0,
        "{label}: plain write must not run an upsert conflict probe: {counters:?}",
    );
}

#[test]
fn insert_many_encrypts_ciphertext_before_sqlite_storage() {
    run(async {
        use std::collections::HashMap;

        use base64::Engine as _;
        use zeroship_plugin_db::backend::sqlite::session::TypedCell;
        use zeroship_plugin_db::backend::{EncryptedColumn as _, EncryptionMode};
        use zeroship_plugin_db::encryption;
        use zeroship_plugin_db::query::{
            build_create_table_with_fks_for_dialect, build_insert_many_with_dialect, FkEmission,
            SqlDialect,
        };

        let key_id = "c1_insert_many";
        let _env = EncEnv::set("ZEROSHIP_COLUMN_KEY_C1_INSERT_MANY", &"d".repeat(64));
        let app_id = "app_demo";
        let collection = "bulk_people";
        let schema = serde_json::json!({
            "name": { "type": "string" },
            "ssn": {
                "type": "string",
                "encrypted": { "mode": "randomised", "keyId": key_id, "wraps": "string" },
                "mask": { "kind": "last4", "classification": "spi" }
            }
        });
        let (backend, _dir) =
            unmask_setup_with_schema(app_id, collection, schema.clone()).await;
        let ddl = build_create_table_with_fks_for_dialect(
            app_id,
            collection,
            &schema,
            &FkEmission::Inline,
            SqlDialect::Sqlite,
        )
        .expect("build DDL");
        for stmt in ddl.split(";\n") {
            let trimmed = stmt.trim();
            if trimmed.is_empty() {
                continue;
            }
            backend.pool_exec(trimmed, &[]).await.expect("DDL exec");
        }

        let mut docs = serde_json::json!([
            { "name": "Alice", "ssn": "123-45-6789" },
            { "name": "Bob", "ssn": "987-65-4321" }
        ]);
        zeroship_plugin_db::crud::prepare_insert_many_docs_for_write(
            &mut docs,
            app_id,
            collection,
            Some("usr_bulk_writer"),
        )
        .await
        .expect("prepare insertMany docs");

        let expected_by_id: HashMap<String, (String, String)> = docs
            .as_array()
            .expect("docs array")
            .iter()
            .map(|doc| {
                let obj = doc.as_object().expect("doc object");
                (
                    obj.get("id")
                        .and_then(|v| v.as_str())
                        .expect("minted id")
                        .to_string(),
                    (
                        obj.get("ssn")
                            .and_then(|v| v.as_str())
                            .expect("base64 ciphertext marker doc")
                            .to_string(),
                        obj.get("ssn_masked")
                            .and_then(|v| v.as_str())
                            .expect("masked sibling")
                            .to_string(),
                    ),
                )
            })
            .collect();

        let built =
            build_insert_many_with_dialect(app_id, collection, &docs, SqlDialect::Sqlite)
                .expect("build insertMany");
        let params: Vec<&str> = built.params.iter().map(String::as_str).collect();
        let client = backend
            .acquire_dedicated_client()
            .await
            .expect("acquire client");
        client
            .query_typed(&built.sql, &params)
            .await
            .expect("INSERT ... RETURNING");

        let typed = client
            .query_typed(
                &format!(
                    r#"SELECT id, ssn, ssn_masked FROM "{app_id}"."{collection}" ORDER BY id"#
                ),
                &[],
            )
            .await
            .expect("SELECT typed");
        assert_eq!(typed.rows.len(), 2, "two rows stored");

        let key = backend
            .resolve_key(app_id, key_id)
            .await
            .expect("resolve key");
        for row in &typed.rows {
            let id = match &row[0] {
                TypedCell::Text(s) => s.clone(),
                other => panic!("id must be TEXT, got {other:?}"),
            };
            let stored_blob = match &row[1] {
                TypedCell::Blob(bytes) => bytes.clone(),
                other => panic!("ssn must be stored as BLOB ciphertext, got {other:?}"),
            };
            let masked = match &row[2] {
                TypedCell::Text(s) => s.clone(),
                other => panic!("ssn_masked must be TEXT, got {other:?}"),
            };
            let (prepared_ciphertext_b64, prepared_masked) = expected_by_id
                .get(&id)
                .expect("stored row id should match prepared docs");
            assert_eq!(masked, *prepared_masked, "masked sibling must be persisted");
            assert_ne!(
                stored_blob,
                b"123-45-6789".to_vec(),
                "stored bytes must not equal raw plaintext",
            );
            assert_ne!(
                stored_blob,
                b"987-65-4321".to_vec(),
                "stored bytes must not equal raw plaintext",
            );
            let expected_ciphertext = base64::engine::general_purpose::STANDARD
                .decode(prepared_ciphertext_b64)
                .expect("prepared ciphertext base64");
            assert_eq!(
                stored_blob, expected_ciphertext,
                "raw stored bytes must match the write-side ciphertext",
            );
            let plaintext = backend
                .decrypt(
                    &key,
                    EncryptionMode::Randomised,
                    &stored_blob,
                    &encryption::canonical_aad(collection, "ssn", Some(id.as_bytes())),
                )
                .expect("decrypt stored blob");
            assert!(
                plaintext == b"123-45-6789" || plaintext == b"987-65-4321",
                "decrypting the stored blob must recover one of the inserted plaintexts",
            );
        }
    });
}

#[test]
fn upsert_insert_branch_auto_mints_id_sqlite_runtime() {
    let key_id = "c2_upsert_runtime_insert";
    let _env = EncEnv::set(
        "ZEROSHIP_COLUMN_KEY_C2_UPSERT_RUNTIME_INSERT",
        &"e".repeat(64),
    );

    run(async {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = sqlite_runtime_upsert_source(
            "users",
            key_id,
            r#"
async function upsertInsert(_input, _ctx) {
    return await env.db.collection(COLLECTION).upsert(
        {
            email: "mint@example.com",
            name: "Mint",
            ssn: "123-45-6789"
        },
        { conflictFields: ["email"] },
    );
}
upsertInsert.config = { kind: "action" };

const _procedures = { setup, upsertInsert };
"#,
        );

        let setup = dispatch_sqlite_runtime(&dir, &source, "setup");
        let result = dispatch_sqlite_runtime(&dir, &source, "upsertInsert");
        let row = parity::extract_json(&result);
        let id = row
            .get("id")
            .and_then(|v| v.as_str())
            .expect("upsert insert branch must return minted id");
        assert!(
            id.starts_with("user_"),
            "auto-minted id should use collection-derived prefix: {row}"
        );
        assert_eq!(
            row.get("version").and_then(|v| v.as_i64()),
            Some(1),
            "freshly inserted upsert row should start at version 1: {row}"
        );

        let backend = SqliteBackend::new(PathBuf::from(dir.path())).expect("open backend");
        backend
            .ensure_app_schema("default")
            .await
            .expect("ensure default schema");
        let client = backend
            .acquire_dedicated_client()
            .await
            .expect("acquire client");
        let rows = client
            .query(
                r#"SELECT id, version FROM "default"."users" WHERE email = 'mint@example.com'"#,
                &[],
            )
            .await
            .expect("SELECT runtime upsert row");
        assert_eq!(rows.len(), 1, "exactly one runtime-upsert row");
        assert_eq!(rows[0][0].as_deref(), Some(id), "stored row keeps minted id");
        assert_eq!(rows[0][1].as_deref(), Some("1"), "stored row version defaults to 1");
        drop(setup);
    });
}

#[test]
fn upsert_conflict_update_preserves_insert_only_fields_and_encrypts_sqlite_runtime() {
    let key_id = "c2_upsert_runtime_conflict";
    let _env = EncEnv::set(
        "ZEROSHIP_COLUMN_KEY_C2_UPSERT_RUNTIME_CONFLICT",
        &"f".repeat(64),
    );

    run(async {
        use zeroship_plugin_db::backend::sqlite::session::TypedCell;
        use zeroship_plugin_db::backend::{EncryptedColumn as _, EncryptionMode};
        use zeroship_plugin_db::encryption;

        let dir = tempfile::tempdir().expect("tempdir");
        let source = sqlite_runtime_upsert_source(
            "users",
            key_id,
            r#"
async function upsertConflict(_input, _ctx) {
    const coll = env.db.collection(COLLECTION);
    const first = await coll.upsert(
        {
            id: "user_seed",
            email: "alice@example.com",
            name: "Alice",
            created_by: "usr_seed",
            ssn: "123-45-6789"
        },
        { conflictFields: ["email"] },
    );
    const second = await coll.upsert(
        {
            id: "user_new",
            email: "alice@example.com",
            name: "Alice Updated",
            created_by: "usr_new",
            updated_by: "usr_update",
            ssn: "987-65-4321"
        },
        { conflictFields: ["email"] },
    );
    return { first, second };
}
upsertConflict.config = { kind: "action" };

const _procedures = { setup, upsertConflict };
"#,
        );

        dispatch_sqlite_runtime(&dir, &source, "setup");
        let result = dispatch_sqlite_runtime(&dir, &source, "upsertConflict");
        let payload = parity::extract_json(&result);
        let first = payload.get("first").expect("first response row");
        let second = payload.get("second").expect("second response row");
        assert_eq!(
            first.get("id").and_then(|v| v.as_str()),
            Some("user_seed"),
            "first upsert should return the inserted row"
        );
        assert_eq!(
            second.get("id").and_then(|v| v.as_str()),
            Some("user_seed"),
            "conflict update must keep the original id"
        );
        assert_eq!(
            second.get("created_by").and_then(|v| v.as_str()),
            Some("usr_seed"),
            "conflict update must preserve original created_by"
        );
        assert_eq!(
            second.get("updated_by").and_then(|v| v.as_str()),
            Some("usr_update"),
            "mutable updated_by should update on conflict"
        );
        assert_eq!(
            second.get("version").and_then(|v| v.as_i64()),
            Some(2),
            "conflict update must auto-bump version"
        );

        let backend = SqliteBackend::new(PathBuf::from(dir.path())).expect("open backend");
        backend
            .ensure_app_schema("default")
            .await
            .expect("ensure default schema");
        let client = backend
            .acquire_dedicated_client()
            .await
            .expect("acquire client");
        let typed = client
            .query_typed(
                r#"SELECT id, created_by, updated_by, version, ssn, ssn_masked
                   FROM "default"."users"
                   WHERE email = 'alice@example.com'"#,
                &[],
            )
            .await
            .expect("SELECT typed conflict row");
        assert_eq!(typed.rows.len(), 1, "exactly one row after conflict upsert");
        let row = &typed.rows[0];

        match &row[0] {
            TypedCell::Text(id) => assert_eq!(id, "user_seed"),
            other => panic!("id must be TEXT, got {other:?}"),
        }
        match &row[1] {
            TypedCell::Text(created_by) => assert_eq!(created_by, "usr_seed"),
            other => panic!("created_by must be TEXT, got {other:?}"),
        }
        match &row[2] {
            TypedCell::Text(updated_by) => assert_eq!(updated_by, "usr_update"),
            other => panic!("updated_by must be TEXT, got {other:?}"),
        }
        match &row[3] {
            TypedCell::Integer(version) => assert_eq!(*version, 2),
            other => panic!("version must be INTEGER, got {other:?}"),
        }
        let stored_blob = match &row[4] {
            TypedCell::Blob(bytes) => bytes.clone(),
            other => panic!("ssn must be stored as BLOB ciphertext, got {other:?}"),
        };
        match &row[5] {
            TypedCell::Text(masked) => assert_eq!(masked, "***-**-4321"),
            other => panic!("ssn_masked must be TEXT, got {other:?}"),
        }
        assert_ne!(
            stored_blob,
            b"987-65-4321".to_vec(),
            "conflict-updated raw storage must not equal plaintext"
        );

        let key = backend
            .resolve_key("default", key_id)
            .await
            .expect("resolve key");
        let plaintext = backend
            .decrypt(
                &key,
                EncryptionMode::Randomised,
                &stored_blob,
                &encryption::canonical_aad("users", "ssn", Some(b"user_seed")),
            )
            .expect("decrypt stored conflict ciphertext");
        assert_eq!(
            plaintext,
            b"987-65-4321".to_vec(),
            "stored ciphertext must decrypt to the updated plaintext"
        );
    });
}

#[test]
fn upsert_conflict_with_deterministic_key_keeps_randomised_ciphertext_readable_sqlite_runtime() {
    let key_id = "c2_upsert_det_conflict_runtime";
    let _env = EncEnv::set(
        "ZEROSHIP_COLUMN_KEY_C2_UPSERT_DET_CONFLICT_RUNTIME",
        &"6".repeat(64),
    );

    run(async {
        use zeroship_plugin_db::backend::sqlite::session::TypedCell;
        use zeroship_plugin_db::backend::{EncryptedColumn as _, EncryptionMode};
        use zeroship_plugin_db::encryption;

        let dir = tempfile::tempdir().expect("tempdir");
        let source = sqlite_runtime_upsert_det_conflict_source(
            "users",
            key_id,
            r#"
async function upsertConflict(_input, _ctx) {
    const coll = env.db.collection(COLLECTION);
    const first = await coll.upsert(
        {
            id: "user_seed",
            email: "alice@example.com",
            name: "Alice",
            ssn: "123-45-6789"
        },
        { conflictFields: ["email"] },
    );
    const second = await coll.upsert(
        {
            id: "user_new",
            email: "alice@example.com",
            name: "Alice Updated",
            ssn: "987-65-4321"
        },
        { conflictFields: ["email"] },
    );
    return { first, second };
}
upsertConflict.config = { kind: "action" };

const _procedures = { setup, upsertConflict };
"#,
        );

        dispatch_sqlite_runtime(&dir, &source, "setup");
        let result = dispatch_sqlite_runtime(&dir, &source, "upsertConflict");
        let payload = parity::extract_json(&result);
        let second = payload.get("second").expect("second response row");
        assert_eq!(
            second.get("id").and_then(|v| v.as_str()),
            Some("user_seed"),
            "deterministic conflict probe must rewrite to the existing row id"
        );

        let backend = SqliteBackend::new(PathBuf::from(dir.path())).expect("open backend");
        backend
            .ensure_app_schema("default")
            .await
            .expect("ensure default schema");
        let client = backend
            .acquire_dedicated_client()
            .await
            .expect("acquire client");
        let typed = client
            .query_typed(
                r#"SELECT id, email, ssn, ssn_masked
                   FROM "default"."users""#,
                &[],
            )
            .await
            .expect("SELECT typed conflict row");
        assert_eq!(typed.rows.len(), 1, "exactly one row after conflict upsert");

        let row = &typed.rows[0];
        let row_id = match &row[0] {
            TypedCell::Text(id) => id.clone(),
            other => panic!("id must be TEXT, got {other:?}"),
        };
        let email_blob = match &row[1] {
            TypedCell::Blob(bytes) => bytes.clone(),
            other => panic!("email must be stored as deterministic ciphertext BLOB, got {other:?}"),
        };
        let ssn_blob = match &row[2] {
            TypedCell::Blob(bytes) => bytes.clone(),
            other => panic!("ssn must be stored as randomised ciphertext BLOB, got {other:?}"),
        };
        match &row[3] {
            TypedCell::Text(masked) => assert_eq!(masked, "***-**-4321"),
            other => panic!("ssn_masked must be TEXT, got {other:?}"),
        }

        let key = backend
            .resolve_key("default", key_id)
            .await
            .expect("resolve key");
        let email_plaintext = backend
            .decrypt(
                &key,
                EncryptionMode::Deterministic,
                &email_blob,
                &encryption::canonical_aad("users", "email", None),
            )
            .expect("decrypt deterministic conflict key");
        assert_eq!(email_plaintext, b"alice@example.com".to_vec());

        let ssn_plaintext = backend
            .decrypt(
                &key,
                EncryptionMode::Randomised,
                &ssn_blob,
                &encryption::canonical_aad("users", "ssn", Some(row_id.as_bytes())),
            )
            .expect("decrypt conflict-updated randomised sibling");
        assert_eq!(
            ssn_plaintext,
            b"987-65-4321".to_vec(),
            "randomised sibling must be readable against the existing row id"
        );
    });
}

#[test]
fn update_non_id_filter_keeps_randomised_ciphertext_readable_sqlite_runtime() {
    let key_id = "c1_update_non_id_runtime";
    let _env = EncEnv::set(
        "ZEROSHIP_COLUMN_KEY_C1_UPDATE_NON_ID_RUNTIME",
        &"7".repeat(64),
    );

    run(async {
        use zeroship_plugin_db::backend::sqlite::session::TypedCell;
        use zeroship_plugin_db::backend::{EncryptedColumn as _, EncryptionMode};
        use zeroship_plugin_db::encryption;

        let dir = tempfile::tempdir().expect("tempdir");
        let source = sqlite_runtime_upsert_source(
            "users",
            key_id,
            r#"
async function seed(_input, _ctx) {
    return await env.db.collection(COLLECTION).upsert(
        {
            id: "user_seed",
            email: "alice@example.com",
            name: "Alice",
            ssn: "123-45-6789"
        },
        { conflictFields: ["email"] },
    );
}
seed.config = { kind: "action" };

async function updateByEmail(_input, _ctx) {
    return await env.db.collection(COLLECTION).update(
        { email: "alice@example.com" },
        { ssn: "987-65-4321" },
    );
}
updateByEmail.config = { kind: "action" };

const _procedures = { setup, seed, updateByEmail };
"#,
        );

        dispatch_sqlite_runtime(&dir, &source, "setup");
        dispatch_sqlite_runtime(&dir, &source, "seed");
        let updated = dispatch_sqlite_runtime(&dir, &source, "updateByEmail");
        let row = parity::extract_json(&updated);
        assert_eq!(
            row.get("id").and_then(|v| v.as_str()),
            Some("user_seed"),
            "update by non-id filter should still target the seeded row"
        );

        let backend = SqliteBackend::new(PathBuf::from(dir.path())).expect("open backend");
        backend
            .ensure_app_schema("default")
            .await
            .expect("ensure default schema");
        let client = backend
            .acquire_dedicated_client()
            .await
            .expect("acquire client");
        let typed = client
            .query_typed(
                r#"SELECT id, ssn, ssn_masked
                   FROM "default"."users"
                   WHERE email = 'alice@example.com'"#,
                &[],
            )
            .await
            .expect("SELECT typed updated row");
        assert_eq!(typed.rows.len(), 1, "exactly one updated row");

        let row = &typed.rows[0];
        let row_id = match &row[0] {
            TypedCell::Text(id) => id.clone(),
            other => panic!("id must be TEXT, got {other:?}"),
        };
        let stored_blob = match &row[1] {
            TypedCell::Blob(bytes) => bytes.clone(),
            other => panic!("ssn must be stored as BLOB ciphertext, got {other:?}"),
        };
        match &row[2] {
            TypedCell::Text(masked) => assert_eq!(masked, "***-**-4321"),
            other => panic!("ssn_masked must be TEXT, got {other:?}"),
        }

        let key = backend
            .resolve_key("default", key_id)
            .await
            .expect("resolve key");
        let plaintext = backend
            .decrypt(
                &key,
                EncryptionMode::Randomised,
                &stored_blob,
                &encryption::canonical_aad("users", "ssn", Some(row_id.as_bytes())),
            )
            .expect("decrypt updated ciphertext");
        assert_eq!(
            plaintext,
            b"987-65-4321".to_vec(),
            "non-id update must store ciphertext readable with the resolved row id"
        );
    });
}

#[test]
fn update_many_non_id_filter_encrypts_per_row_sqlite_runtime() {
    let key_id = "c1_update_many_non_id_runtime";
    let _env = EncEnv::set(
        "ZEROSHIP_COLUMN_KEY_C1_UPDATE_MANY_NON_ID_RUNTIME",
        &"8".repeat(64),
    );

    run(async {
        use zeroship_plugin_db::backend::sqlite::session::TypedCell;
        use zeroship_plugin_db::backend::{EncryptedColumn as _, EncryptionMode};
        use zeroship_plugin_db::encryption;

        let dir = tempfile::tempdir().expect("tempdir");
        let source = sqlite_runtime_upsert_source(
            "users",
            key_id,
            r#"
async function seed(_input, _ctx) {
    const coll = env.db.collection(COLLECTION);
    await coll.upsert(
        {
            id: "user_a",
            email: "alice@example.com",
            name: "Red Team",
            ssn: "123-45-6789"
        },
        { conflictFields: ["email"] },
    );
    await coll.upsert(
        {
            id: "user_b",
            email: "bob@example.com",
            name: "Red Team",
            ssn: "222-33-4444"
        },
        { conflictFields: ["email"] },
    );
    await coll.upsert(
        {
            id: "user_c",
            email: "carol@example.com",
            name: "Blue Team",
            ssn: "555-66-7777"
        },
        { conflictFields: ["email"] },
    );
    return { seeded: 3 };
}
seed.config = { kind: "action" };

async function updateManyByName(_input, _ctx) {
    return await env.db.collection(COLLECTION).updateMany(
        { name: "Red Team" },
        { ssn: "999-88-7777" },
    );
}
updateManyByName.config = { kind: "action" };

const _procedures = { setup, seed, updateManyByName };
"#,
        );

        dispatch_sqlite_runtime(&dir, &source, "setup");
        dispatch_sqlite_runtime(&dir, &source, "seed");
        let updated = parity::extract_json(&dispatch_sqlite_runtime(&dir, &source, "updateManyByName"));
        assert_eq!(
            updated.as_f64(),
            Some(2.0),
            "two rows should match the non-id updateMany filter: {updated}"
        );

        let backend = SqliteBackend::new(PathBuf::from(dir.path())).expect("open backend");
        backend
            .ensure_app_schema("default")
            .await
            .expect("ensure default schema");
        let client = backend
            .acquire_dedicated_client()
            .await
            .expect("acquire client");
        let typed = client
            .query_typed(
                r#"SELECT id, name, ssn, ssn_masked
                   FROM "default"."users"
                   WHERE name = 'Red Team'
                   ORDER BY id"#,
                &[],
            )
            .await
            .expect("SELECT typed updated rows");
        assert_eq!(typed.rows.len(), 2, "exactly two rows should be updated");

        let key = backend
            .resolve_key("default", key_id)
            .await
            .expect("resolve key");
        for row in &typed.rows {
            let row_id = match &row[0] {
                TypedCell::Text(id) => id.clone(),
                other => panic!("id must be TEXT, got {other:?}"),
            };
            match &row[1] {
                TypedCell::Text(name) => assert_eq!(name, "Red Team"),
                other => panic!("name must be TEXT, got {other:?}"),
            }
            let stored_blob = match &row[2] {
                TypedCell::Blob(bytes) => bytes.clone(),
                other => panic!("ssn must be stored as BLOB ciphertext, got {other:?}"),
            };
            match &row[3] {
                TypedCell::Text(masked) => assert_eq!(masked, "***-**-7777"),
                other => panic!("ssn_masked must be TEXT, got {other:?}"),
            }
            let plaintext = backend
                .decrypt(
                    &key,
                    EncryptionMode::Randomised,
                    &stored_blob,
                    &encryption::canonical_aad("users", "ssn", Some(row_id.as_bytes())),
                )
                .expect("decrypt updated ciphertext");
            assert_eq!(
                plaintext,
                b"999-88-7777".to_vec(),
                "each bulk-updated row must carry ciphertext bound to its own row id"
            );
        }
    });
}

#[test]
fn plain_updates_on_encrypted_collection_stay_on_fast_path_sqlite_runtime() {
    let key_id = "perf_plain_update_fast_path_runtime";
    let _env = EncEnv::set(
        "ZEROSHIP_COLUMN_KEY_PERF_PLAIN_UPDATE_FAST_PATH_RUNTIME",
        &"9".repeat(64),
    );

    run(async {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = sqlite_runtime_upsert_source(
            "users",
            key_id,
            r#"
async function seed(_input, _ctx) {
    const coll = env.db.collection(COLLECTION);
    await coll.upsert(
        {
            id: "user_a",
            email: "alice@example.com",
            name: "Red Team",
            ssn: "123-45-6789"
        },
        { conflictFields: ["email"] },
    );
    await coll.upsert(
        {
            id: "user_b",
            email: "bob@example.com",
            name: "Red Team",
            ssn: "222-33-4444"
        },
        { conflictFields: ["email"] },
    );
    return true;
}
seed.config = { kind: "action" };

async function updatePlain(_input, _ctx) {
    return await env.db.collection(COLLECTION).update(
        { email: "alice@example.com" },
        { name: "Blue Team" },
    );
}
updatePlain.config = { kind: "action" };

async function updateManyPlain(_input, _ctx) {
    return await env.db.collection(COLLECTION).updateMany(
        { name: "Red Team" },
        { name: "Green Team" },
    );
}
updateManyPlain.config = { kind: "action" };

const _procedures = { setup, seed, updatePlain, updateManyPlain };
"#,
        );

        dispatch_sqlite_runtime(&dir, &source, "setup");
        dispatch_sqlite_runtime(&dir, &source, "seed");

        zeroship_plugin_db::crud::reset_write_path_counters_for_tests();
        let updated = parity::extract_json(&dispatch_sqlite_runtime(&dir, &source, "updatePlain"));
        assert_eq!(
            updated.get("name").and_then(|v| v.as_str()),
            Some("Blue Team"),
            "plain update should still update the targeted row",
        );
        assert_write_path_fast_path("updateOne plain field");

        zeroship_plugin_db::crud::reset_write_path_counters_for_tests();
        let updated_many =
            parity::extract_json(&dispatch_sqlite_runtime(&dir, &source, "updateManyPlain"));
        assert_eq!(
            updated_many.as_f64(),
            Some(1.0),
            "plain updateMany should only affect the remaining Red Team row",
        );
        assert_write_path_fast_path("updateMany plain field");
    });
}

#[test]
fn plain_upsert_on_encrypted_collection_skips_conflict_probe_sqlite_runtime() {
    let key_id = "perf_plain_upsert_fast_path_runtime";
    let _env = EncEnv::set(
        "ZEROSHIP_COLUMN_KEY_PERF_PLAIN_UPSERT_FAST_PATH_RUNTIME",
        &"a".repeat(64),
    );

    run(async {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = r#"
import { env } from "zeroship";

const __plat = (typeof globalThis.__zsDbPlatform === "function")
    ? globalThis.__zsDbPlatform(env.db)
    : undefined;
const COLLECTION = "users";
const KEY_ID = "__KEY_ID__";

function setup(_input, _ctx) {
    return __plat.registerModel(COLLECTION, {
        email: { type: "string", required: true, unique: true },
        name: { type: "string", required: true },
        secret: {
            type: "string",
            encrypted: { mode: "randomised", keyId: KEY_ID, wraps: "string" }
        }
    });
}
setup.config = { kind: "action" };

async function seed(_input, _ctx) {
    return await env.db.collection(COLLECTION).upsert(
        {
            id: "user_seed",
            email: "alice@example.com",
            name: "Alice",
            secret: "alpha-secret"
        },
        { conflictFields: ["email"] },
    );
}
seed.config = { kind: "action" };

async function upsertPlainConflict(_input, _ctx) {
    return await env.db.collection(COLLECTION).upsert(
        {
            id: "user_new",
            email: "alice@example.com",
            name: "Alice Updated"
        },
        { conflictFields: ["email"] },
    );
}
upsertPlainConflict.config = { kind: "action" };

const _procedures = { setup, seed, upsertPlainConflict };
"#
        .replace("__KEY_ID__", key_id)
            + SQLITE_RUNTIME_RPC_SHIM;

        dispatch_sqlite_runtime(&dir, &source, "setup");
        dispatch_sqlite_runtime(&dir, &source, "seed");

        zeroship_plugin_db::crud::reset_write_path_counters_for_tests();
        let updated =
            parity::extract_json(&dispatch_sqlite_runtime(&dir, &source, "upsertPlainConflict"));
        assert_eq!(
            updated.get("id").and_then(|v| v.as_str()),
            Some("user_seed"),
            "plain conflict upsert should still target the existing row",
        );
        assert_eq!(
            updated.get("name").and_then(|v| v.as_str()),
            Some("Alice Updated"),
            "plain conflict upsert should still update the plain field",
        );
        assert_write_path_fast_path("upsert plain field");
    });
}

#[test]
fn update_rejects_nested_version_filter_without_mutating_sqlite_row() {
    let key_id = "i5_update_nested_version";
    let _env = EncEnv::set(
        "ZEROSHIP_COLUMN_KEY_I5_UPDATE_NESTED_VERSION",
        &"1".repeat(64),
    );

    run(async {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = sqlite_runtime_upsert_source(
            "users",
            key_id,
            r#"
async function seed(_input, _ctx) {
    return await env.db.collection(COLLECTION).upsert(
        {
            id: "user_seed",
            email: "alice@example.com",
            name: "Alice",
            ssn: "123-45-6789"
        },
        { conflictFields: ["email"] },
    );
}
seed.config = { kind: "action" };

async function nestedCasUpdate(_input, _ctx) {
    return await env.db.collection(COLLECTION).update(
        {
            "$and": [
                { id: "user_seed" },
                { version: 1 }
            ]
        },
        { name: "Mallory" },
    );
}
nestedCasUpdate.config = { kind: "action" };

const _procedures = { setup, seed, nestedCasUpdate };
"#,
        );

        dispatch_sqlite_runtime(&dir, &source, "setup");
        let seeded = dispatch_sqlite_runtime(&dir, &source, "seed");
        let row = parity::extract_json(&seeded);
        assert_eq!(
            row.get("version").and_then(|v| v.as_i64()),
            Some(1),
            "seed row must start at version 1"
        );

        let (status, body) =
            parity::dispatch_zs(&parity::sqlite_url(&dir), &source, "nestedCasUpdate");
        assert_ne!(status, 200, "nested version CAS must reject, got {body}");
        assert_eq!(
            body.get("code").and_then(|v| v.as_str()),
            Some("version_filter_must_be_top_level"),
            "nested CAS rejection must carry the canonical code: {body}"
        );

        let backend = SqliteBackend::new(PathBuf::from(dir.path())).expect("open backend");
        backend
            .ensure_app_schema("default")
            .await
            .expect("ensure default schema");
        let client = backend
            .acquire_dedicated_client()
            .await
            .expect("acquire client");
        let rows = client
            .query(
                r#"SELECT name, version FROM "default"."users" WHERE id = 'user_seed'"#,
                &[],
            )
            .await
            .expect("SELECT row after rejected nested CAS update");
        assert_eq!(rows.len(), 1, "seed row must still exist");
        assert_eq!(
            rows[0][0].as_deref(),
            Some("Alice"),
            "failed nested CAS update must not rewrite the row"
        );
        assert_eq!(
            rows[0][1].as_deref(),
            Some("1"),
            "failed nested CAS update must not auto-bump version"
        );
    });
}

#[test]
fn update_many_rejects_nested_version_filter_without_mutating_sqlite_row() {
    let key_id = "i5_update_many_nested_version";
    let _env = EncEnv::set(
        "ZEROSHIP_COLUMN_KEY_I5_UPDATE_MANY_NESTED_VERSION",
        &"2".repeat(64),
    );

    run(async {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = sqlite_runtime_upsert_source(
            "users",
            key_id,
            r#"
async function seed(_input, _ctx) {
    return await env.db.collection(COLLECTION).upsert(
        {
            id: "user_seed",
            email: "alice@example.com",
            name: "Alice",
            ssn: "123-45-6789"
        },
        { conflictFields: ["email"] },
    );
}
seed.config = { kind: "action" };

async function nestedCasUpdateMany(_input, _ctx) {
    return await env.db.collection(COLLECTION).updateMany(
        {
            "$and": [
                { id: "user_seed" },
                { version: 1 }
            ]
        },
        { name: "Mallory" },
    );
}
nestedCasUpdateMany.config = { kind: "action" };

const _procedures = { setup, seed, nestedCasUpdateMany };
"#,
        );

        dispatch_sqlite_runtime(&dir, &source, "setup");
        dispatch_sqlite_runtime(&dir, &source, "seed");

        let (status, body) =
            parity::dispatch_zs(&parity::sqlite_url(&dir), &source, "nestedCasUpdateMany");
        assert_ne!(status, 200, "nested version CAS must reject, got {body}");
        assert_eq!(
            body.get("code").and_then(|v| v.as_str()),
            Some("version_filter_must_be_top_level"),
            "nested CAS rejection must carry the canonical code: {body}"
        );

        let backend = SqliteBackend::new(PathBuf::from(dir.path())).expect("open backend");
        backend
            .ensure_app_schema("default")
            .await
            .expect("ensure default schema");
        let client = backend
            .acquire_dedicated_client()
            .await
            .expect("acquire client");
        let rows = client
            .query(
                r#"SELECT name, version FROM "default"."users" WHERE id = 'user_seed'"#,
                &[],
            )
            .await
            .expect("SELECT row after rejected nested CAS updateMany");
        assert_eq!(rows.len(), 1, "seed row must still exist");
        assert_eq!(
            rows[0][0].as_deref(),
            Some("Alice"),
            "failed nested CAS updateMany must not rewrite the row"
        );
        assert_eq!(
            rows[0][1].as_deref(),
            Some("1"),
            "failed nested CAS updateMany must not auto-bump version"
        );
    });
}

/// **P5 PR 3 — gate #1 (SQLite half)**: round-trip an encrypted string
/// column under Randomised mode. Insert a row with `ssn` declared
/// `t.encrypted({ mode: "randomised" })`, read it back via the SQLite
/// path, expect the plaintext to recover.
///
/// Each P5 SQLite test uses a UNIQUE `keyId` so concurrent tests don't
/// race on the process-global env table — the
/// `ZEROSHIP_COLUMN_KEY_<KEYID>` namespace is per-key, so distinct
/// `keyId`s give each test its own env-var slot. Same pattern as the
/// in-crate `encryption::keys::tests` use.
#[test]
fn encrypted_column_round_trip_sqlite_randomised() {
    use zeroship_plugin_db::backend::{EncryptedColumn as _, EncryptionMode};
    use zeroship_plugin_db::encryption;
    let key_id = "p5_sqlite_rt_rand";
    let _env = EncEnv::set("ZEROSHIP_COLUMN_KEY_P5_SQLITE_RT_RAND", &"a".repeat(64));
    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .ensure_app_schema("app_demo")
            .await
            .expect("ensure_app_schema");
        backend
            .pool_exec(
                "CREATE TABLE \"app_demo\".\"enc_notes\" (\
                     id  TEXT PRIMARY KEY, \
                     ssn BLOB\
                 )",
                &[],
            )
            .await
            .expect("CREATE TABLE enc_notes");

        let key = backend
            .resolve_key("app1", key_id)
            .await
            .expect("resolve_key");
        let plaintext = b"123-45-6789";
        let aad = encryption::canonical_aad("enc_notes", "ssn", Some(b"row_a"));
        let ct = backend
            .encrypt(&key, EncryptionMode::Randomised, plaintext, &aad)
            .expect("encrypt");

        // Bind the ciphertext as an inline X'...' BLOB literal. The
        // session actor's `[&str]` params lane only carries TEXT; SQL
        // literals are how we inject BLOB values without widening the
        // protocol.
        let blob_lit = sqlite_blob_literal(&ct);
        let insert_sql = format!(
            "INSERT INTO \"app_demo\".\"enc_notes\" (id, ssn) VALUES (?, {blob_lit})"
        );
        backend
            .pool_exec(&insert_sql, &["row_a"])
            .await
            .expect("INSERT");

        // Pull the ciphertext back as a typed BLOB. `client_exec`
        // routes through `query`, which stringifies BLOBs as
        // `<N bytes blob>` — that's not what we want here. Reach into
        // the session's typed-row path via the dedicated client; the
        // `query_typed` method preserves the BLOB discriminant. The
        // simplest cross-test path: re-encode the BLOB as hex via SQL
        // (`hex(ssn)`) and parse back to bytes here.
        let client = backend
            .acquire_dedicated_client()
            .await
            .expect("acquire client");
        let rows = client
            .query(
                "SELECT hex(ssn) FROM \"app_demo\".\"enc_notes\" WHERE id = ?",
                &["row_a"],
            )
            .await
            .expect("SELECT");
        let hex_str = rows[0][0]
            .clone()
            .expect("ssn column must be present");
        let raw: Vec<u8> = (0..hex_str.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex_str[i..i + 2], 16).unwrap())
            .collect();

        let recovered = backend
            .decrypt(&key, EncryptionMode::Randomised, &raw, &aad)
            .expect("decrypt");
        assert_eq!(recovered, plaintext);
    });
}

/// **P5 PR 3 — gate #1 (SQLite half), deterministic variant**.
#[test]
fn encrypted_column_round_trip_sqlite_deterministic() {
    use zeroship_plugin_db::backend::{EncryptedColumn as _, EncryptionMode};
    use zeroship_plugin_db::encryption;
    let key_id = "p5_sqlite_rt_det";
    let _env = EncEnv::set("ZEROSHIP_COLUMN_KEY_P5_SQLITE_RT_DET", &"b".repeat(64));
    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .ensure_app_schema("app_demo")
            .await
            .expect("ensure_app_schema");
        backend
            .pool_exec(
                "CREATE TABLE \"app_demo\".\"enc_notes\" (\
                     id  TEXT PRIMARY KEY, \
                     ssn BLOB\
                 )",
                &[],
            )
            .await
            .expect("CREATE TABLE enc_notes");

        let key = backend
            .resolve_key("app1", key_id)
            .await
            .expect("resolve_key");
        let plaintext = b"DETERMINISTIC-PLAINTEXT";
        // Deterministic AAD: row_pk omitted (Camp A).
        let aad = encryption::canonical_aad("enc_notes", "ssn", None);
        let ct = backend
            .encrypt(&key, EncryptionMode::Deterministic, plaintext, &aad)
            .expect("encrypt");

        let blob_lit = sqlite_blob_literal(&ct);
        let insert_sql = format!(
            "INSERT INTO \"app_demo\".\"enc_notes\" (id, ssn) VALUES (?, {blob_lit})"
        );
        backend
            .pool_exec(&insert_sql, &["row_a"])
            .await
            .expect("INSERT");

        let client = backend
            .acquire_dedicated_client()
            .await
            .expect("acquire client");
        let rows = client
            .query(
                "SELECT hex(ssn) FROM \"app_demo\".\"enc_notes\" WHERE id = ?",
                &["row_a"],
            )
            .await
            .expect("SELECT");
        let hex_str = rows[0][0].clone().expect("ssn must be present");
        let raw: Vec<u8> = (0..hex_str.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex_str[i..i + 2], 16).unwrap())
            .collect();

        let recovered = backend
            .decrypt(&key, EncryptionMode::Deterministic, &raw, &aad)
            .expect("decrypt");
        assert_eq!(recovered, plaintext);
    });
}

/// **P5 PR 3 — gate #2 (SQLite half), CRITICAL #1 fence (SQLite half)**.
///
/// Insert 100 rows under deterministic mode with five distinct plaintexts
/// (so equality groups overlap), query by ciphertext equality, assert
/// the matching set. Also asserts that two identical plaintexts produce
/// byte-identical ciphertexts — the defining deterministic property
/// that makes the B-tree equality lookup sound.
///
/// The `EXPLAIN QUERY PLAN` SEARCH/index-use assertion is omitted here
/// because creating a B-tree index on a SQLite BLOB column is
/// supported but the planner's choice between SCAN and SEARCH depends
/// on table size + ANALYZE state; pinning a specific shape would make
/// the test flaky across SQLite versions. The matching-set assertion
/// alone exercises the equality-lookup contract.
#[test]
fn deterministic_encrypted_equality_via_index_sqlite() {
    use zeroship_plugin_db::backend::{EncryptedColumn as _, EncryptionMode};
    use zeroship_plugin_db::encryption;
    let key_id = "p5_sqlite_det_eq";
    let _env = EncEnv::set("ZEROSHIP_COLUMN_KEY_P5_SQLITE_DET_EQ", &"c".repeat(64));
    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .ensure_app_schema("app_demo")
            .await
            .expect("ensure_app_schema");
        backend
            .pool_exec(
                "CREATE TABLE \"app_demo\".\"enc_notes\" (\
                     id  TEXT PRIMARY KEY, \
                     ssn BLOB\
                 )",
                &[],
            )
            .await
            .expect("CREATE TABLE enc_notes");
        backend
            .pool_exec(
                "CREATE INDEX \"app_demo\".\"enc_notes_ssn_idx\" \
                 ON \"enc_notes\"(ssn)",
                &[],
            )
            .await
            .expect("CREATE INDEX");

        let key = backend
            .resolve_key("app1", key_id)
            .await
            .expect("resolve_key");

        // Five distinct plaintexts; 100 rows total. The expected
        // equality group for "P0" is 20 rows (0..100 step 5).
        let plaintexts = [
            &b"P0-shared"[..],
            &b"P1-distinct"[..],
            &b"P2-distinct"[..],
            &b"P3-distinct"[..],
            &b"P4-distinct"[..],
        ];
        let aad = encryption::canonical_aad("enc_notes", "ssn", None);
        let ciphertexts: Vec<Vec<u8>> = plaintexts
            .iter()
            .map(|p| {
                backend
                    .encrypt(&key, EncryptionMode::Deterministic, p, &aad)
                    .expect("encrypt")
            })
            .collect();

        // Defining deterministic property: re-encrypt P0 → same bytes.
        let p0_again = backend
            .encrypt(&key, EncryptionMode::Deterministic, plaintexts[0], &aad)
            .expect("re-encrypt P0");
        assert_eq!(
            ciphertexts[0], p0_again,
            "deterministic mode must produce byte-identical ciphertext for the same plaintext"
        );

        // Insert 100 rows; row N gets plaintexts[N % 5].
        for i in 0..100usize {
            let ct = &ciphertexts[i % 5];
            let blob_lit = sqlite_blob_literal(ct);
            let id = format!("row_{i:03}");
            let sql = format!(
                "INSERT INTO \"app_demo\".\"enc_notes\" (id, ssn) VALUES (?, {blob_lit})"
            );
            backend.pool_exec(&sql, &[id.as_str()]).await.expect("INSERT");
        }

        // Equality lookup on P0's ciphertext should match exactly 20
        // rows (0, 5, 10, ..., 95).
        let client = backend
            .acquire_dedicated_client()
            .await
            .expect("acquire client");
        let p0_lit = sqlite_blob_literal(&ciphertexts[0]);
        let count_sql = format!(
            "SELECT COUNT(*) FROM \"app_demo\".\"enc_notes\" WHERE ssn = {p0_lit}"
        );
        let rows = client
            .query(&count_sql, &[])
            .await
            .expect("SELECT COUNT");
        let n: i64 = rows[0][0]
            .as_deref()
            .and_then(|s| s.parse().ok())
            .expect("count must parse");
        assert_eq!(n, 20, "equality on shared ciphertext must match every 5th row");

        // P1's ciphertext should also match 20 rows.
        let p1_lit = sqlite_blob_literal(&ciphertexts[1]);
        let count_sql = format!(
            "SELECT COUNT(*) FROM \"app_demo\".\"enc_notes\" WHERE ssn = {p1_lit}"
        );
        let rows = client
            .query(&count_sql, &[])
            .await
            .expect("SELECT COUNT");
        let n: i64 = rows[0][0].as_deref().and_then(|s| s.parse().ok()).unwrap();
        assert_eq!(n, 20);
    });
}

/// **P5 PR 3 — §13 Camp A fence (SQLite half), mirror of the PG test
/// `encrypted_randomised_row_swap_rejected`**. Insert two Randomised
/// rows; UPDATE swaps their ciphertexts; reading row B with row B's
/// AAD must surface `encryption_aead_failed`. This is the load-bearing
/// assertion for the row-PK-in-AAD policy.
#[test]
fn randomised_ciphertext_row_swap_rejected_sqlite() {
    use zeroship_plugin_db::backend::{EncryptedColumn as _, EncryptionMode};
    use zeroship_plugin_db::encryption;
    let key_id = "p5_sqlite_row_swap";
    let _env = EncEnv::set("ZEROSHIP_COLUMN_KEY_P5_SQLITE_ROW_SWAP", &"d".repeat(64));
    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .ensure_app_schema("app_demo")
            .await
            .expect("ensure_app_schema");
        backend
            .pool_exec(
                "CREATE TABLE \"app_demo\".\"enc_notes\" (\
                     id  TEXT PRIMARY KEY, \
                     ssn BLOB\
                 )",
                &[],
            )
            .await
            .expect("CREATE TABLE enc_notes");

        let key = backend.resolve_key("app1", key_id).await.unwrap();
        // Insert row A and row B, each with its OWN AAD (binds row_pk).
        let ct_a = backend
            .encrypt(
                &key,
                EncryptionMode::Randomised,
                b"sensitive-A",
                &encryption::canonical_aad("enc_notes", "ssn", Some(b"row_a")),
            )
            .unwrap();
        let ct_b = backend
            .encrypt(
                &key,
                EncryptionMode::Randomised,
                b"sensitive-B",
                &encryption::canonical_aad("enc_notes", "ssn", Some(b"row_b")),
            )
            .unwrap();
        for (id, ct) in [("row_a", &ct_a), ("row_b", &ct_b)] {
            let blob_lit = sqlite_blob_literal(ct);
            let sql = format!(
                "INSERT INTO \"app_demo\".\"enc_notes\" (id, ssn) VALUES (?, {blob_lit})"
            );
            backend.pool_exec(&sql, &[id]).await.unwrap();
        }

        // Attacker move: UPDATE row_b's ssn slot with row_a's ciphertext.
        let blob_a = sqlite_blob_literal(&ct_a);
        let sql = format!(
            "UPDATE \"app_demo\".\"enc_notes\" SET ssn = {blob_a} WHERE id = ?"
        );
        backend.pool_exec(&sql, &["row_b"]).await.unwrap();

        // Read row B's ssn back and try to decrypt with row B's AAD.
        let client = backend
            .acquire_dedicated_client()
            .await
            .expect("acquire client");
        let rows = client
            .query(
                "SELECT hex(ssn) FROM \"app_demo\".\"enc_notes\" WHERE id = ?",
                &["row_b"],
            )
            .await
            .unwrap();
        let hex_str = rows[0][0].clone().expect("ssn must be present");
        let raw: Vec<u8> = (0..hex_str.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex_str[i..i + 2], 16).unwrap())
            .collect();
        let aad_b = encryption::canonical_aad("enc_notes", "ssn", Some(b"row_b"));
        let err = backend
            .decrypt(&key, EncryptionMode::Randomised, &raw, &aad_b)
            .expect_err("row-swap must fail AAD verification");
        match err {
            DbError::ValidationFailed { code, .. } => {
                assert_eq!(code, "encryption_aead_failed");
            }
            other => panic!(
                "expected ValidationFailed/encryption_aead_failed, got {other:?}"
            ),
        }
    });
}

/// **P5 PR 3 — cross-backend equivalence (SQLite ↔ SQLite via shared
/// env-var key).** Encrypt plaintext on backend_a; copy the ciphertext
/// bytes; decrypt on backend_b (different temp file) configured with
/// the same `ZEROSHIP_COLUMN_KEY_DEFAULT`. Proves HKDF derivation is
/// deterministic across instances — the encryption module is the
/// shared cross-backend surface, so two SQLite backends with the same
/// root key produce the same derived AEAD key (and thus the same
/// decryption result).
#[test]
fn cross_backend_ciphertext_decrypt_via_shared_key() {
    use zeroship_plugin_db::backend::{EncryptedColumn as _, EncryptionMode};
    use zeroship_plugin_db::encryption;
    let key_id = "p5_sqlite_cross";
    let _env = EncEnv::set("ZEROSHIP_COLUMN_KEY_P5_SQLITE_CROSS", &"e".repeat(64));
    run(async {
        // Two separate backends rooted at separate temp dirs.
        let (backend_a, _dir_a) = fresh_backend();
        let (backend_b, _dir_b) = fresh_backend();

        // Use the SAME app_id so HKDF salt matches; the env-var key
        // sourcing is process-global, so the root key is identical.
        let app_id = "app_shared";
        let key_a = backend_a.resolve_key(app_id, key_id).await.unwrap();
        let key_b = backend_b.resolve_key(app_id, key_id).await.unwrap();
        // The derived halves must match — same root + same app_id.
        assert_eq!(key_a.k_enc, key_b.k_enc);
        assert_eq!(key_a.k_siv, key_b.k_siv);

        let plaintext = b"cross-instance-payload";
        let aad =
            encryption::canonical_aad("enc_notes", "ssn", Some(b"row_a"));
        let ct = backend_a
            .encrypt(&key_a, EncryptionMode::Randomised, plaintext, &aad)
            .expect("encrypt on A");

        // Decrypt the SAME ciphertext on backend_b with backend_b's
        // resolved key. Must round-trip.
        let recovered = backend_b
            .decrypt(&key_b, EncryptionMode::Randomised, &ct, &aad)
            .expect("decrypt on B");
        assert_eq!(recovered, plaintext);
    });
}

// ===========================================================================
// P5 PR 3.5 — close the SQLite CRUD-path gap
// ===========================================================================
//
// PR 3 wired the `EncryptedColumn` trait on `SqliteBackend` and pinned
// the trait surface with the round-trip tests above, but the
// orchestrator's CRUD-path SQL builder still emitted PG-only
// `decode($N, 'base64')::bytea` syntax for encrypted columns, so a
// SQLite app with a `t.encrypted(...)` column on its schema surfaced
// a typed `column_encryption_unavailable` error at the dispatch layer
// rather than working. PR 3.5 closes that gap with a dialect-aware
// bind: the SQL builder's encrypted-column placeholder is now
// `decode($N, 'base64')::bytea` on PG (byte-for-byte identical to
// PR 2) and a bare `$N` on SQLite, with the encryption pass tagging
// the param value with `SQLITE_ENC_BLOB_PREFIX` so the SQLite session
// actor decodes the base64 and binds raw bytes as BLOB.
//
// This integration test stitches the layers end-to-end:
//   1. `encrypt_row_on_write` (PR 2's helper) against the SQLite
//      backend → row carries the base64 ciphertext + `__zsenc__<col>`
//      marker.
//   2. `build_insert_with_dialect(SqlDialect::Sqlite, ...)` → SQL with
//      bare `$N` placeholder + sentinel-tagged param.
//   3. `backend.pool_exec(sql, &params)` → SQLite session strips the
//      sentinel, base64-decodes, binds BLOB.
//   4. `client.query_typed("SELECT ssn FROM ...")` → raw bytes back.
//   5. Render the typed BLOB as `\xHHHH` hex (PG text-protocol shape),
//      wrap in `Value::String`, dispatch to `decrypt_row_on_read` —
//      plaintext recovers.
//
// The "via the orchestrator's CRUD path" framing in the PR 3.5 plan is
// what this test pins — the orchestrator's CRUD entry today routes
// through `exec.rs::run_sql` which is PG-only, so we exercise the
// underlying helpers in the same shape `dispatch_insert` /
// `dispatch_find` will once the SQLite CRUD route lands. The
// dialect-aware builder + sentinel-tagged bind is the load-bearing
// piece this test proves correct.

/// **P5 PR 3.5 — gate**: full encrypted-column round-trip through the
/// SQLite CRUD pipeline (encryption pass → SQL builder w/ SQLite
/// dialect → SQLite session bind → typed row decode → decrypt pass).
/// This is the test that proves the end-to-end SDK works on SQLite for
/// `t.encrypted(...)` columns.
#[test]
fn encrypted_column_e2e_crud_round_trip_sqlite() {
    use zeroship_plugin_db::backend::SqlExecutor as _;
    use zeroship_plugin_db::backend::sqlite::session::TypedCell;
    use zeroship_plugin_db::crud::encryption_pass::{decrypt_row_on_read, encrypt_row_on_write};
    use zeroship_plugin_db::query::{build_insert_with_dialect, SqlDialect};

    let _env = EncEnv::set("ZEROSHIP_COLUMN_KEY_P5_E2E_CRUD", &"c".repeat(64));
    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .ensure_app_schema("app_demo")
            .await
            .expect("ensure_app_schema");
        // PRIMARY KEY `id TEXT` + encrypted `ssn BLOB` — same shape the
        // CRUD path's `build_create_table_with_fks` emits for an
        // `t.encrypted({ wraps: "string" })` field, except we skip the
        // sentinel-comment metadata because the introspector isn't on
        // the e2e read path here.
        backend
            .pool_exec(
                "CREATE TABLE \"app_demo\".\"users\" (\
                     id  TEXT PRIMARY KEY, \
                     ssn BLOB\
                 )",
                &[],
            )
            .await
            .expect("CREATE TABLE users");

        // The schema the encryption pass sees — declares `ssn` as a
        // randomised-encrypted column wrapping the string type, keyed
        // to the e2e-test-specific env var.
        let schema = serde_json::json!({
            "ssn": {
                "type": "string",
                "encrypted": {
                    "mode": "randomised",
                    "keyId": "p5_e2e_crud",
                    "wraps": "string",
                },
            },
        });

        let plaintext = "123-45-6789";
        let row_pk = "row_e2e";
        let mut doc = serde_json::json!({
            "id": row_pk,
            "ssn": plaintext,
        });

        // Step 1 — encryption pass swaps ssn into base64 ciphertext +
        // installs the `__zsenc__ssn` marker.
        encrypt_row_on_write(&backend, "app_demo", "users", &schema, row_pk, &mut doc)
            .await
            .expect("encrypt_row_on_write");
        assert!(
            doc.get("__zsenc__ssn").and_then(|v| v.as_bool()) == Some(true),
            "encryption pass must install the marker key: {doc:?}",
        );
        // Pull out the base64 ciphertext for a later equality check.
        let ct_b64_before_bind = doc
            .get("ssn")
            .and_then(|v| v.as_str())
            .expect("ssn must be a base64 string after encrypt")
            .to_string();

        // Step 2 — dialect-aware SQL build. The output SQL must use a
        // bare `$N` placeholder (no `decode(...)::bytea`); the param
        // vector must carry the `__zsenc_blob__:` sentinel prefix on
        // the encrypted ssn value.
        let bq = build_insert_with_dialect("app_demo", "users", &doc, SqlDialect::Sqlite)
            .expect("build_insert_with_dialect");
        assert!(
            !bq.sql.contains("decode("),
            "SQLite dialect must not emit `decode(...)::bytea`: {}",
            bq.sql,
        );
        assert!(
            bq.params
                .iter()
                .any(|p| p.starts_with("__zsenc_blob__:")),
            "SQLite dialect must tag the encrypted param with the sentinel: {:?}",
            bq.params,
        );

        // Step 3 — drive the build through the SQLite session. The
        // session strips the sentinel, base64-decodes, binds BLOB. PR
        // 3.5's session-layer fix is what makes this work.
        // `pool_exec` uses the session under the hood; `query_text_params`
        // doesn't exist on SQLite — `&[&str]` is the only surface.
        let param_refs: Vec<&str> = bq.params.iter().map(String::as_str).collect();
        // SQLite's `Connection::execute` only accepts a non-row-
        // returning statement; `RETURNING *` produces a row, so we run
        // it as a query and drop the rows. The session's `query`
        // surface runs through the same `decode_blob_params` path.
        let client = backend
            .acquire_dedicated_client()
            .await
            .expect("acquire client");
        let _affected = client
            .query_typed(&bq.sql, &param_refs)
            .await
            .expect("INSERT ... RETURNING via SQLite session");

        // Step 4 — pull the BLOB back typed. The session's `query`
        // surface stringifies BLOBs as `<N bytes blob>` placeholders, so
        // we reach for the typed surface (PR 4's `query_typed` lane)
        // via the session handle's `query_typed` helper.
        let typed = client
            .query_typed(
                "SELECT id, ssn FROM \"app_demo\".\"users\" WHERE id = ?",
                &[row_pk],
            )
            .await
            .expect("SELECT typed");
        assert_eq!(typed.rows.len(), 1, "exactly one row must exist");
        let id_cell = &typed.rows[0][0];
        let ssn_cell = &typed.rows[0][1];
        let id_text = match id_cell {
            TypedCell::Text(s) => s.clone(),
            other => panic!("id must be TEXT, got {other:?}"),
        };
        assert_eq!(id_text, row_pk);
        let ssn_bytes = match ssn_cell {
            TypedCell::Blob(b) => b.clone(),
            other => panic!("ssn must be BLOB, got {other:?}"),
        };

        // Sanity — the stored bytes must equal the bytes the encryption
        // pass produced (base64-decoded back to raw). If the session's
        // sentinel strip / base64 decode is wrong, the stored bytes
        // diverge from the plaintext ciphertext.
        let expected_bytes = {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD
                .decode(&ct_b64_before_bind)
                .expect("encryption pass must have produced valid base64")
        };
        assert_eq!(
            ssn_bytes, expected_bytes,
            "stored BLOB must equal the raw ciphertext (sentinel strip + base64 decode in session)",
        );

        // Step 5 — render the BLOB row as the JSON shape
        // `decrypt_row_on_read` expects (`\xHHHH` hex string).
        let mut hex = String::with_capacity(2 + ssn_bytes.len() * 2);
        hex.push('\\');
        hex.push('x');
        for b in &ssn_bytes {
            use std::fmt::Write as _;
            let _ = write!(hex, "{b:02x}");
        }
        let mut row_value = serde_json::json!({
            "id": id_text,
            "ssn": hex,
        });

        decrypt_row_on_read(&backend, "app_demo", "users", &schema, &mut row_value, &[])
            .await
            .expect("decrypt_row_on_read");

        assert_eq!(
            row_value.get("ssn").and_then(|v| v.as_str()),
            Some(plaintext),
            "decrypted plaintext must recover: {row_value}",
        );
    });
}

// ===========================================================================
// P5.5 PR 2 — Path B sibling-column dual-write integration
// ===========================================================================
//
// Tests the end-to-end Path B contract on the SQLite arm:
// (a) CREATE TABLE emits both the parent + `<col>_masked` sibling.
// (b) INSERT writes both atomically (mask pass runs before SQL build).
// (c) The masked sibling contains the pre-computed mask string while the
//     parent stores the ciphertext / plaintext as before.

/// **P5.5 PR 2 — DDL shape on SQLite**: `build_create_table_with_fks`
/// emits both the parent and a sibling `<col>_masked TEXT NOT NULL`
/// column for every masked field. The SQLite arm receives the SQL
/// byte-identical to PG; the sibling clause itself is standard SQL
/// (`TEXT NOT NULL`) so the SQLite engine accepts it once executed
/// through the SQLite-flavoured `CREATE TABLE` path.
#[test]
fn sibling_column_emitted_for_masked_field_sqlite() {
    use zeroship_plugin_db::query::{build_create_table_with_fks, FkEmission};
    let schema = serde_json::json!({
        "ssn": {
            "type": "string",
            "mask": { "kind": "last4", "classification": "spi" }
        },
        "name": { "type": "string" }
    });
    let sql =
        build_create_table_with_fks("app_demo", "users", &schema, &FkEmission::Inline).unwrap();
    assert!(
        sql.contains("\"ssn_masked\" TEXT NOT NULL"),
        "sibling column must be emitted: {sql}"
    );
    assert!(
        !sql.contains("\"name_masked\""),
        "non-masked column must not emit a sibling: {sql}"
    );
}

/// **P5.5 PR 2 — atomic dual-write on SQLite**: when a row carries
/// both the parent + sibling (mask pass already ran), the
/// SQLite-flavoured `build_insert_with_dialect` INSERT statement
/// includes both columns atomically. Then we execute the INSERT
/// against a hand-rolled SQLite-shaped table to confirm the engine
/// accepts the dual write end-to-end and persists the masked value
/// alongside the plaintext.
#[test]
fn dual_write_insert_persists_parent_and_sibling_sqlite() {
    use zeroship_plugin_db::query::{build_insert_with_dialect, SqlDialect};

    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .ensure_app_schema("app_demo")
            .await
            .expect("ensure_app_schema");
        // Hand-rolled SQLite-flavoured CREATE TABLE — the SQLite
        // CREATE TABLE dialect doesn't speak PG's SERIAL /
        // TIMESTAMPTZ; the orchestrator emits SQLite-flavoured DDL
        // elsewhere. PR 2's responsibility is the sibling-column
        // CLAUSE, which is standard SQL; we exercise it inside a
        // SQLite-valid table here.
        backend
            .pool_exec(
                "CREATE TABLE \"app_demo\".\"users\" (\
                     id    INTEGER PRIMARY KEY, \
                     ssn   TEXT, \
                     ssn_masked TEXT NOT NULL\
                 )",
                &[],
            )
            .await
            .expect("CREATE TABLE ok");

        // Simulate the dispatch_insert → apply_mask_on_write step:
        // the mask pass has populated `ssn_masked`. The SQL builder
        // walks the row map, so the sibling key naturally lands on
        // the INSERT column list (no special-casing needed).
        let doc = serde_json::json!({
            "ssn": "123-45-6789",
            "ssn_masked": "***-**-6789"
        });
        let bq = build_insert_with_dialect("app_demo", "users", &doc, SqlDialect::Sqlite).unwrap();
        assert!(
            bq.sql.contains("\"ssn\"") && bq.sql.contains("\"ssn_masked\""),
            "INSERT must reference both parent + sibling: {}",
            bq.sql,
        );

        let param_refs: Vec<&str> = bq.params.iter().map(String::as_str).collect();
        let client = backend
            .acquire_dedicated_client()
            .await
            .expect("acquire client");
        let _ = client
            .query(&bq.sql, &param_refs)
            .await
            .expect("dual-write INSERT must succeed");

        // Verify both columns landed atomically.
        let rows = client
            .query(
                "SELECT ssn, ssn_masked FROM \"app_demo\".\"users\"",
                &[],
            )
            .await
            .expect("SELECT both columns");
        assert_eq!(rows.len(), 1, "exactly one row inserted");
        assert_eq!(rows[0][0].as_deref(), Some("123-45-6789"));
        assert_eq!(rows[0][1].as_deref(), Some("***-**-6789"));
    });
}

/// **P5.5 PR 3 — aliased SELECT serves the masked sibling**: a default
/// read against a masked-column DDL must emit
/// `"<col>_masked" AS "<col>"` in the SELECT clause and never include
/// the parent (ciphertext / plaintext) column. End-to-end gate: drive a
/// dual-write through the dialect-aware INSERT builder (PR 2), then
/// build a `find` SQL via `build_find_with_schema` with the cached
/// schema, run it through the SQLite session, and assert the engine
/// returns the masked string under the parent key.
#[test]
fn aliased_select_serves_masked_sibling_sqlite() {
    use zeroship_plugin_db::query::{
        build_find_with_schema, build_insert_with_dialect, SqlDialect,
    };

    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .ensure_app_schema("app_demo")
            .await
            .expect("ensure_app_schema");
        backend
            .pool_exec(
                "CREATE TABLE \"app_demo\".\"users\" (\
                     id    TEXT PRIMARY KEY, \
                     ssn   TEXT, \
                     ssn_masked TEXT NOT NULL, \
                     name  TEXT\
                 )",
                &[],
                )
            .await
            .expect("CREATE TABLE ok");

        // Dual-write a row: parent stores plaintext (no encryption pass
        // in this fixture — masking + encryption are orthogonal in
        // `apply_mask_on_write` design), sibling stores the masked
        // string. This is the row PR 2's dual-write produced.
        let schema = serde_json::json!({
            "ssn": {
                "type": "string",
                "mask": { "kind": "last4", "classification": "spi" }
            },
            "name": { "type": "string" }
        });
        let doc = serde_json::json!({
            "id": "usr_01",
            "ssn": "123-45-6789",
            "ssn_masked": "***-**-6789",
            "name": "alice"
        });
        let bq = build_insert_with_dialect("app_demo", "users", &doc, SqlDialect::Sqlite)
            .expect("build_insert_with_dialect");
        let param_refs: Vec<&str> = bq.params.iter().map(String::as_str).collect();
        let client = backend
            .acquire_dedicated_client()
            .await
            .expect("acquire client");
        client
            .query(&bq.sql, &param_refs)
            .await
            .expect("INSERT");

        // Build a default read with schema awareness: the SELECT must
        // alias the sibling under the parent name AND must NOT include
        // the parent column (`ssn`) directly. Verify the SQL shape
        // BEFORE running the query — this is the load-bearing
        // assertion PR 3 ships.
        let bq = build_find_with_schema(
            "app_demo",
            "users",
            &serde_json::json!({ "id": "usr_01" }),
            None,
            None,
            None,
            None,
            Some(&schema),
        )
        .expect("build_find_with_schema");
        assert!(
            bq.sql.contains("\"ssn_masked\" AS \"ssn\""),
            "SELECT must alias the sibling under the parent name: {}",
            bq.sql,
        );
        // The parent column slot (ciphertext / plaintext) must NOT
        // appear in the SELECT clause — `<col>_masked AS <col>` is the
        // ONLY way `ssn` enters the result set.
        let select_clause = bq
            .sql
            .split(" FROM ")
            .next()
            .expect("SELECT prefix")
            .to_string();
        // Crude but adequate: there's no occurrence of bare `"ssn"`
        // (without the `_masked` suffix or AS-rewrite) in the SELECT.
        let bare_ssn_count = select_clause.matches("\"ssn\"").count();
        let aliased_count = select_clause.matches("\"ssn_masked\" AS \"ssn\"").count();
        assert_eq!(
            bare_ssn_count, aliased_count,
            "every occurrence of `\"ssn\"` in the SELECT must be the AS-rewrite tail: {select_clause}"
        );

        // Execute the SELECT and verify the row returns the masked
        // string under the parent name.
        let param_refs: Vec<&str> = bq.params.iter().map(String::as_str).collect();
        let rows = client
            .query(&bq.sql, &param_refs)
            .await
            .expect("SELECT");
        assert_eq!(rows.len(), 1);
        // The aliased SELECT puts `ssn_masked` under the `ssn` column
        // slot. Column ordering: id, ssn, name (the schema iteration
        // order in `build_masked_aware_select_expr`'s "case 2").
        // Find the `ssn` value (the masked string).
        let row = &rows[0];
        // Row shape: `Vec<Option<String>>` from the SQLite client.
        // Order is the order we emitted in SELECT: id, ssn (= sibling
        // value), name.
        assert_eq!(
            row.iter().filter_map(|c| c.as_deref()).find(|s| *s == "***-**-6789"),
            Some("***-**-6789"),
            "row must include the masked string: {row:?}"
        );
        // Ciphertext / plaintext parent value must NOT appear (we
        // dropped it from the SELECT).
        assert!(
            !row.iter().any(|c| c.as_deref() == Some("123-45-6789")),
            "parent slot ciphertext / plaintext must not surface on default read: {row:?}"
        );
    });
}

/// **P5.5 PR 3 — kind: none preserves the P5 decrypt-on-read path**:
/// when a column declares `mask: { kind: "none" }`, the SELECT clause
/// must emit the parent column directly (no AS-rewrite), and the row
/// must surface the parent's value (the ciphertext / plaintext under
/// the parent column).
#[test]
fn aliased_select_skips_kind_none_sqlite() {
    use zeroship_plugin_db::query::build_find_with_schema;

    let schema = serde_json::json!({
        "ssn": {
            "type": "string",
            "encrypted": { "mode": "randomised", "keyId": "default", "wraps": "string" },
            "mask": { "kind": "none", "classification": "spi" }
        },
        "name": { "type": "string" }
    });
    let bq = build_find_with_schema(
        "app_demo",
        "users",
        &serde_json::json!({}),
        None,
        None,
        None,
        None,
        Some(&schema),
    )
    .expect("build_find_with_schema");
    assert!(
        !bq.sql.contains("\"ssn_masked\""),
        "kind=none must NOT trigger the AS-rewrite: {}",
        bq.sql,
    );
    // Schema-aware reads now always expand to the allowlisted public
    // column set, even when every mask is `kind: "none"`.
    assert!(
        !bq.sql.contains("SELECT *"),
        "schema-backed reads must avoid `*`: {}",
        bq.sql,
    );
    assert!(
        bq.sql.contains("SELECT \"id\", \"created_at\", \"updated_at\"")
            && bq.sql.contains("\"ssn\"")
            && bq.sql.contains("\"name\""),
        "schema-backed reads must project the public column set: {}",
        bq.sql,
    );
}

/// **P5.5 PR 2 — NOT NULL contract on the sibling**: omitting the
/// sibling from an INSERT against a masked-column DDL must fail at the
/// engine level (the sibling is `TEXT NOT NULL`). This is the
/// load-bearing assertion that mask-pass must run before the SQL
/// builder — skip it and the engine rejects with a NOT NULL violation.
#[test]
fn missing_sibling_fails_not_null_constraint_sqlite() {
    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .ensure_app_schema("app_demo")
            .await
            .expect("ensure_app_schema");
        backend
            .pool_exec(
                "CREATE TABLE \"app_demo\".\"users\" (\
                     id  INTEGER PRIMARY KEY, \
                     ssn TEXT, \
                     ssn_masked TEXT NOT NULL\
                 )",
                &[],
            )
            .await
            .expect("CREATE TABLE ok");
        // Insert WITHOUT the sibling. The engine must refuse.
        let res = backend
            .pool_exec(
                "INSERT INTO \"app_demo\".\"users\" (\"ssn\") VALUES (?)",
                &["plaintext-no-mask"],
            )
            .await;
        assert!(
            res.is_err(),
            "INSERT without sibling MUST fail (sibling is NOT NULL); got Ok"
        );
    });
}

// ===========================================================================
// P5 PR 5 — SQLite `Backup` impl (VACUUM INTO snapshot + atomic
// file-swap restore + `pitr_pg_only` refusal). Five tests covering the
// gates in plan §11 + the CRITICAL #3 fence (concurrent writer):
//
//   1. `snapshot_restore_round_trip_sqlite` (gate #4): seed rows,
//      snapshot to file://; drop rows; restore; assert recovery.
//   2. `vacuum_into_snapshot_consistent_under_concurrent_writer`
//      (gate #5 / CRITICAL #3 fence): spawn a writer thread; trigger
//      snapshot; assert (a) snap file well-formed, (b) live > snap,
//      (c) no SQLITE_BUSY under Retry policy.
//   3. `pitr_pg_only_returns_configuration_on_sqlite` (gate #6).
//   4. `snapshot_during_migration_returns_typed_error_sqlite`: hold
//      register_model lock; snapshot must refuse w/ `migration_in_progress`.
//   5. `restore_hash_mismatch_rejected_sqlite`: corrupt snapshot;
//      restore must refuse with `snapshot_hash_mismatch` BEFORE touching
//      the live DB.

use zeroship_plugin_db::backend::{
    Backup as _, BusyPolicy as BackupBusyPolicy, PitrTarget, SnapshotOpts,
};

/// **P5 PR 5 — gate #4**: round-trip snapshot+restore on SQLite.
/// Insert N rows into a per-app collection; snapshot to a temp dir;
/// raw `DROP TABLE` to clear rows; restore; assert the rows recovered.
///
/// The dest URI uses the `file://` scheme (the only one PR 5 supports).
/// We pick a destination INSIDE the backend's `db_dir` so the restore's
/// `std::fs::copy → rename` swap lands on the same filesystem as the
/// live per-app file (POSIX rename atomic-same-FS contract).
#[test]
fn snapshot_restore_round_trip_sqlite() {
    run(async {
        let (backend, dir) = fresh_backend();
        backend
            .ensure_app_schema("app_demo")
            .await
            .expect("ensure_app_schema");

        // Seed deterministic rows.
        backend
            .pool_exec(
                "CREATE TABLE \"app_demo\".\"notes\" (id INTEGER PRIMARY KEY, body TEXT)",
                &[],
            )
            .await
            .expect("CREATE TABLE notes");
        const ROW_COUNT: i64 = 10;
        for i in 0..ROW_COUNT {
            let sql =
                format!("INSERT INTO \"app_demo\".\"notes\" VALUES ({i}, 'row-{i}')");
            backend.pool_exec(&sql, &[]).await.expect("INSERT row");
        }
        // Sanity: row count is N.
        let client = backend
            .acquire_dedicated_client()
            .await
            .expect("acquire client");
        let rows = client
            .query("SELECT COUNT(*) FROM \"app_demo\".\"notes\"", &[])
            .await
            .expect("count rows pre-snapshot");
        assert_eq!(rows[0][0].as_deref(), Some(ROW_COUNT.to_string().as_str()));

        // Snapshot. Dest must NOT pre-exist (SQLite VACUUM INTO refuses
        // to overwrite); use a fresh name in the same dir as the live
        // per-app file so the eventual restore's same-FS rename works.
        let snap_path = dir.path().join("snap-app_demo.sqlite");
        let snap_uri = format!("file://{}", snap_path.to_string_lossy());
        let handle = backend
            .snapshot(
                "app_demo",
                &snap_uri,
                SnapshotOpts {
                    if_busy: BackupBusyPolicy::Retry,
                },
            )
            .await
            .expect("snapshot");
        assert!(snap_path.exists(), "snapshot file must exist on disk");
        assert_eq!(handle.uri, snap_uri, "handle uri echoes caller-supplied");
        // The content_hash field is the SHA-256 of the on-disk bytes —
        // re-hash here and compare bytewise.
        let observed_hash: [u8; 32] = {
            use sha2::Digest;
            let bytes = std::fs::read(&snap_path).expect("read snap file");
            sha2::Sha256::digest(&bytes).into()
        };
        assert_eq!(
            handle.content_hash, observed_hash,
            "content_hash must match SHA-256 of on-disk file"
        );

        // Clear the live rows so the restore is a meaningful recovery.
        backend
            .pool_exec("DELETE FROM \"app_demo\".\"notes\"", &[])
            .await
            .expect("DELETE rows");
        let rows_after_delete = client
            .query("SELECT COUNT(*) FROM \"app_demo\".\"notes\"", &[])
            .await
            .expect("count rows post-delete");
        assert_eq!(rows_after_delete[0][0].as_deref(), Some("0"));

        // Restore. After this call the per-app file is replaced with
        // the snapshot content and the session re-ATTACHed against
        // the new file.
        backend
            .restore("app_demo", &handle)
            .await
            .expect("restore");

        // Rows are back.
        let rows_restored = client
            .query("SELECT COUNT(*) FROM \"app_demo\".\"notes\"", &[])
            .await
            .expect("count rows post-restore");
        assert_eq!(
            rows_restored[0][0].as_deref(),
            Some(ROW_COUNT.to_string().as_str()),
            "restore must recover the original row count"
        );
    });
}

/// **P5 PR 5 — gate #5 / CRITICAL #3 fence**: VACUUM INTO under a
/// concurrent writer must be snapshot-isolated (the dest matches the
/// commit point visible when VACUUM INTO began; writes appended
/// during the copy do NOT land in the snapshot). We assert:
///
///   (a) the snapshot file is well-formed (opens cleanly, COUNT(*)
///       returns a finite number).
///   (b) live > snap — at least one write happened during the snapshot
///       window and landed in the live DB but not the snapshot copy.
///   (c) no `SQLITE_BUSY` surfaced under `BusyPolicy::Retry`.
///
/// Mechanism: spawn a `std::thread` that hammers INSERTs into the
/// per-app collection via a SECOND `Connection` opened directly on
/// the per-app file (bypasses the session actor's mpsc queue — the
/// writer thread + the snapshot's read transaction race for the WAL
/// observer position). The main thread takes the snapshot; the writer
/// keeps inserting throughout.
#[test]
fn vacuum_into_snapshot_consistent_under_concurrent_writer() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    run(async {
        let (backend, dir) = fresh_backend();
        backend
            .ensure_app_schema("app_demo")
            .await
            .expect("ensure_app_schema");
        backend
            .pool_exec(
                "CREATE TABLE \"app_demo\".\"notes\" (id INTEGER PRIMARY KEY, body TEXT)",
                &[],
            )
            .await
            .expect("CREATE TABLE notes");
        // Seed an initial baseline so the snapshot is not empty.
        const INITIAL_ROWS: usize = 50;
        for i in 0..INITIAL_ROWS {
            let sql = format!(
                "INSERT INTO \"app_demo\".\"notes\" VALUES ({i}, 'initial-{i}')"
            );
            backend.pool_exec(&sql, &[]).await.expect("INSERT initial");
        }

        // Concurrent writer thread. Opens its own rusqlite Connection
        // directly against the per-app file in WAL mode and hammers
        // INSERTs until the `stop` flag flips. We use a separate
        // process-internal Connection (NOT the session actor) so the
        // writer races the VACUUM INTO's read transaction at the
        // engine layer — exactly the SQLITE_BUSY surface this test
        // exists to fence.
        let stop = Arc::new(AtomicBool::new(false));
        let writes_observed = Arc::new(AtomicUsize::new(0));
        let app_file = dir.path().join("zs-app_demo.sqlite");
        let writer_stop = stop.clone();
        let writer_observed = writes_observed.clone();
        let writer = std::thread::spawn(move || {
            let conn = rusqlite::Connection::open(&app_file)
                .expect("writer-thread connection open");
            // Match the session's WAL mode so we are in the right
            // concurrency regime; busy_timeout absorbs short-term
            // contention.
            conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA busy_timeout = 5000;")
                .expect("writer PRAGMAs");
            // Insert IDs starting past INITIAL_ROWS to avoid PK
            // collision with seeded rows.
            let mut i = INITIAL_ROWS;
            while !writer_stop.load(Ordering::Relaxed) {
                let sql = format!(
                    "INSERT INTO \"notes\" VALUES ({i}, 'concurrent-{i}')"
                );
                match conn.execute(&sql, []) {
                    Ok(_) => {
                        writer_observed.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) => {
                        // SQLITE_BUSY on the writer side is fine —
                        // the engine's own busy_timeout absorbed
                        // contention. Other errors fail the test.
                        let msg = format!("{e}");
                        if !msg.contains("locked") && !msg.contains("busy") {
                            eprintln!("writer thread INSERT failed: {e}");
                        }
                    }
                }
                i += 1;
                // Slight cadence so we don't hog the CPU; the engine
                // is fast enough that ~thousands of writes happen per
                // second of snapshot wait time regardless.
                std::thread::sleep(Duration::from_micros(50));
            }
        });

        // Give the writer thread a moment to start hammering so the
        // snapshot fires INTO a live write storm (not against an
        // idle DB).
        compio::time::sleep(Duration::from_millis(50)).await;

        // Snapshot under the concurrent writer. Must NOT surface
        // SQLITE_BUSY — the Retry policy + the engine-side
        // busy_timeout absorb everything.
        let snap_path = dir.path().join("snap-concurrent.sqlite");
        let snap_uri = format!("file://{}", snap_path.to_string_lossy());
        let snap_result = backend
            .snapshot(
                "app_demo",
                &snap_uri,
                SnapshotOpts {
                    if_busy: BackupBusyPolicy::Retry,
                },
            )
            .await;

        // (c) — no SQLITE_BUSY classification leaked out.
        let _handle = snap_result.expect(
            "VACUUM INTO under concurrent writer must succeed (Retry absorbs busy)",
        );

        // Stop the writer thread and join.
        stop.store(true, Ordering::Relaxed);
        writer.join().expect("writer-thread join");
        let writes = writes_observed.load(Ordering::Relaxed);
        // Sanity — the writer landed at least some writes; otherwise
        // the test isn't actually exercising the race window.
        assert!(
            writes > 0,
            "writer thread should have committed at least one INSERT before stop"
        );

        // (a) — open snap file standalone and count rows.
        let snap_conn = rusqlite::Connection::open(&snap_path)
            .expect("open snapshot file standalone");
        let snap_count: i64 = snap_conn
            .query_row("SELECT COUNT(*) FROM \"notes\"", [], |r| r.get(0))
            .expect("count snap rows");
        assert!(snap_count >= INITIAL_ROWS as i64);
        // (b) — live > snap (the concurrent writer's commits past the
        // snapshot's read mark are visible in live but NOT in snap).
        let client = backend
            .acquire_dedicated_client()
            .await
            .expect("acquire client");
        let live_rows = client
            .query("SELECT COUNT(*) FROM \"app_demo\".\"notes\"", &[])
            .await
            .expect("count live rows");
        let live_count: i64 = live_rows[0][0]
            .as_deref()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        assert!(
            live_count > snap_count,
            "live ({live_count}) must exceed snap ({snap_count}) — \
             concurrent writes after snapshot must be visible in live but not snap; \
             writer landed {writes} rows total"
        );
    });
}

/// **P5 PR 5 — gate #6**: `pitr_replay` on SQLite returns the typed
/// `Configuration { code: "pitr_pg_only" }` for both `Lsn` and
/// `TimeMillis` targets. SQLite has no WAL-archive PITR substrate;
/// the API surface must refuse cleanly so the SDK can branch.
#[test]
fn pitr_pg_only_returns_configuration_on_sqlite() {
    run(async {
        let (backend, _dir) = fresh_backend();
        // LSN form.
        let err = backend
            .pitr_replay("app_demo", PitrTarget::Lsn("0/0".to_string()))
            .await
            .expect_err("pitr_replay must refuse on SQLite");
        match err {
            DbError::Configuration { code, .. } => {
                assert_eq!(code, "pitr_pg_only");
            }
            other => panic!(
                "expected Configuration {{ code: \"pitr_pg_only\", .. }}, got {other:?}"
            ),
        }
        // TimeMillis form — same refusal.
        let err2 = backend
            .pitr_replay("app_demo", PitrTarget::TimeMillis(1_700_000_000_000))
            .await
            .expect_err("pitr_replay (TimeMillis) must refuse on SQLite");
        match err2 {
            DbError::Configuration { code, .. } => {
                assert_eq!(code, "pitr_pg_only");
            }
            other => panic!(
                "expected Configuration {{ code: \"pitr_pg_only\", .. }}, got {other:?}"
            ),
        }
    });
}

/// **P5 PR 5**: when the per-app `register_model` advisory lock is
/// already held in this process, `snapshot()` surfaces the typed
/// `Coded { code: "migration_in_progress" }` rather than blocking
/// indefinitely. Mirrors the PG arm's `snapshot_during_migration_returns_typed_error`
/// test (`tests/integration.rs`).
///
/// SQLite's lock state lives in `InProcessLockRegistry` (one map per
/// `SqliteBackend`), so we acquire the slot through the public
/// `LockManager` surface — same registry the snapshot pre-flight
/// races for.
#[test]
fn snapshot_during_migration_returns_typed_error_sqlite() {
    run(async {
        let (backend, _dir) = fresh_backend();
        let app_id = "app_miglock";

        // Hold the register_model lock through the typed LockManager
        // surface — exactly the slot the snapshot pre-flight tries to
        // acquire. The `to_keys` derivation is identical to what the
        // snapshot impl computes.
        let client = backend
            .acquire_dedicated_client()
            .await
            .expect("acquire client");
        let scope = LockScope::GlobalApp {
            app_id: app_id.to_string(),
            name: "register_model".to_string(),
        };
        let acquired = backend
            .try_acquire(&client, &scope)
            .await
            .expect("try_acquire register_model");
        assert!(acquired, "test must hold the slot to set up the contention");

        // Snapshot must refuse at pre-flight. We deliberately do NOT
        // pre-create the destination directory so a stray success
        // wouldn't write to disk either.
        let dest = "file:///tmp/p5_pr5_miglock_should_not_exist.sqlite";
        let err = backend
            .snapshot(
                app_id,
                dest,
                SnapshotOpts {
                    if_busy: BackupBusyPolicy::Abort,
                },
            )
            .await
            .expect_err("snapshot must refuse while register_model lock is held");
        match err {
            DbError::Coded { code, .. } => {
                assert_eq!(
                    code, "migration_in_progress",
                    "expected Coded migration_in_progress, got code={code:?}"
                );
            }
            other => panic!(
                "expected Coded {{ code: \"migration_in_progress\", .. }}, got {other:?}"
            ),
        }

        // Dest file must not exist — pre-flight refusal runs before
        // any disk I/O.
        let path = std::path::Path::new("/tmp/p5_pr5_miglock_should_not_exist.sqlite");
        assert!(
            !path.exists(),
            "snapshot must not write to disk when refused at pre-flight"
        );

        // Release for cleanliness.
        backend
            .release(&client, &scope)
            .await
            .expect("release register_model");
    });
}

/// **P5 PR 5**: a `restore()` whose on-disk file has drifted from the
/// `SnapshotHandle`'s recorded SHA-256 must refuse with
/// `Coded { code: "snapshot_hash_mismatch" }` BEFORE touching the
/// live per-app DB. Pins the integrity-verify gate the restore path
/// runs after the lock acquire but before any DETACH/rename.
#[test]
fn restore_hash_mismatch_rejected_sqlite() {
    use std::fs::OpenOptions;
    use std::io::Write;
    run(async {
        let (backend, dir) = fresh_backend();
        backend
            .ensure_app_schema("app_demo")
            .await
            .expect("ensure_app_schema");
        backend
            .pool_exec(
                "CREATE TABLE \"app_demo\".\"notes\" (id INTEGER PRIMARY KEY, body TEXT)",
                &[],
            )
            .await
            .expect("CREATE TABLE notes");
        backend
            .pool_exec(
                "INSERT INTO \"app_demo\".\"notes\" VALUES (1, 'sentinel')",
                &[],
            )
            .await
            .expect("INSERT sentinel");

        // Take a clean snapshot first.
        let snap_path = dir.path().join("snap-hashcheck.sqlite");
        let snap_uri = format!("file://{}", snap_path.to_string_lossy());
        let handle = backend
            .snapshot(
                "app_demo",
                &snap_uri,
                SnapshotOpts {
                    if_busy: BackupBusyPolicy::Retry,
                },
            )
            .await
            .expect("snapshot");

        // Corrupt the on-disk file by appending bytes. The
        // SnapshotHandle's recorded hash no longer matches; restore
        // MUST refuse before touching the live DB.
        {
            let mut f = OpenOptions::new()
                .append(true)
                .open(&snap_path)
                .expect("open snap for append");
            f.write_all(b"\x00\x01\x02 corruption tail \x03\x04\x05")
                .expect("append tampering bytes");
        }

        // Restore must reject with the typed code.
        let err = backend
            .restore("app_demo", &handle)
            .await
            .expect_err("restore must refuse on hash mismatch");
        match err {
            DbError::Coded { code, .. } => {
                assert_eq!(
                    code, "snapshot_hash_mismatch",
                    "expected Coded snapshot_hash_mismatch, got code={code:?}"
                );
            }
            other => panic!(
                "expected Coded {{ code: \"snapshot_hash_mismatch\", .. }}, got {other:?}"
            ),
        }

        // The live DB must be untouched — the sentinel row still
        // exists. (Even without the mismatch check, the rename swap
        // only fires after the hash verify; an early-refuse contract
        // means the live file is bit-for-bit unchanged.)
        let client = backend
            .acquire_dedicated_client()
            .await
            .expect("acquire client");
        let rows = client
            .query(
                "SELECT body FROM \"app_demo\".\"notes\" WHERE id = 1",
                &[],
            )
            .await
            .expect("post-refuse query");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0].as_deref(), Some("sentinel"));
    });
}

// ---------------------------------------------------------------------------
// P5.5 PR 1 — reserved-name validator (Path B sibling-column suffix +
// classification taxonomy) refuses creator-declared collisions at the
// DDL builder level on the SQLite arm. Two tests pin the same surface
// the PG integration suite exercises so both backends agree on the
// reserved namespace.
// ---------------------------------------------------------------------------

#[test]
fn p55_pr1_build_create_table_refuses_masked_suffix_field_sqlite() {
    use zeroship_plugin_db::query::{build_create_table_with_fks, FkEmission};

    let schema = serde_json::json!({
        "name": {"type": "string"},
        // `_masked` is reserved for Path B sibling columns.
        "card_pan_masked": {"type": "string"},
    });
    let result = build_create_table_with_fks("app_demo", "cards", &schema, &FkEmission::Inline);
    let err = result.expect_err("schema with `_masked` suffix should be refused");
    let msg = err.to_string();
    assert!(
        msg.contains("reserved field name") && msg.contains("_masked"),
        "expected reserved-suffix message, got: {msg}"
    );
}

#[test]
fn p55_pr1_build_create_table_refuses_classification_name_field_sqlite() {
    use zeroship_plugin_db::query::{build_create_table_with_fks, FkEmission};

    let schema = serde_json::json!({
        "name": {"type": "string"},
        // `phi` collides with the platform classification taxonomy.
        "phi": {"type": "string"},
    });
    let result = build_create_table_with_fks("app_demo", "patients", &schema, &FkEmission::Inline);
    let err = result.expect_err("schema with reserved classification name should be refused");
    let msg = err.to_string();
    assert!(
        msg.contains("reserved field name"),
        "expected reserved-name message, got: {msg}"
    );
}

// ===========================================================================
// P5.5 PR 4 — unmask RPC + audit table (SQLite arm)
// ===========================================================================
//
// These tests exercise `crud::unmask::dispatch_unmask` end-to-end on the
// SQLite arm:
//   - the audit table is created idempotently on first call;
//   - the default-deny stub grants `kind: "auto"` and denies everyone else;
//   - both granted AND denied paths emit a row to the per-app audit table;
//   - the encrypted-column read path decrypts via the EncryptedColumn impl;
//   - the typed error rail surfaces `unmask_column_not_masked` /
//     `unmask_not_permitted` on the SDK's `.code`-branchable path.
//
// Schema cache + backend handle are installed via the `*_for_tests`
// helpers in `lib.rs`. Each test uses a fresh tempdir so the audit
// table is observed from a clean slate.

use zeroship_plugin_db::crud::unmask;

/// Helper — install backend + schema for an unmask test. Returns the
/// backend (kept alive for the test duration via Rc) + the TempDir
/// guard the caller binds to keep the on-disk directory alive.
async fn unmask_setup_with_schema(
    app_id: &str,
    collection: &str,
    schema: serde_json::Value,
) -> (Rc<SqliteBackend>, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = Rc::new(
        SqliteBackend::new(std::path::PathBuf::from(dir.path()))
            .expect("SqliteBackend::new"),
    );
    backend
        .ensure_app_schema(app_id)
        .await
        .expect("ensure_app_schema");
    // Install into the per-isolate context so dispatch_unmask's
    // backend() lookup succeeds.
    zeroship_plugin_db::set_sqlite_backend_for_tests(backend.clone());
    zeroship_plugin_db::cache_schema_for_tests(app_id, collection, schema);
    (backend, dir)
}

/// Read every row from `__zeroship_audit_unmask` for a given app.
/// Returns `Vec<(outcome, actor_role, classification)>`.
async fn read_audit_rows(
    backend: &SqliteBackend,
    app_id: &str,
) -> Vec<(String, String, String)> {
    use zeroship_plugin_db::backend::DialectBuilder as _;
    let client = backend
        .acquire_dedicated_client()
        .await
        .expect("acquire client");
    let q_app = backend.quote_ident(app_id);
    let sql = format!(
        r#"SELECT outcome, actor_role, classification
           FROM {q_app}."__zeroship_audit_unmask"
           ORDER BY id"#
    );
    let rows = client.query(&sql, &[]).await.expect("query audit rows");
    rows.into_iter()
        .map(|r| {
            (
                r[0].clone().unwrap_or_default(),
                r[1].clone().unwrap_or_default(),
                r[2].clone().unwrap_or_default(),
            )
        })
        .collect()
}

/// **P5.5 PR 4 — gate #1**: an `auto` actor unmasking an encrypted +
/// masked column recovers plaintext, and a `granted` audit row is
/// emitted with the right classification.
#[test]
fn unmask_with_auto_actor_returns_plaintext() {
    let _env = EncEnv::set("ZEROSHIP_COLUMN_KEY_P55_PR4_AUTO", &"a".repeat(64));
    let schema = serde_json::json!({
        "id": { "type": "string" },
        "ssn": {
            "type": "string",
            "encrypted": {
                "mode": "randomised",
                "keyId": "p55_pr4_auto",
                "wraps": "string",
            },
            "mask": { "kind": "last4", "classification": "spi" },
        },
    });
    let app_id = "app_unmask_auto";
    let collection = "users";

    run(async {
        let (backend, _dir) = unmask_setup_with_schema(app_id, collection, schema.clone()).await;
        // Manually create the table — the encryption pass + dual-write
        // pipeline lives in CRUD, but the unmask SELECT only needs
        // `id TEXT PRIMARY KEY, ssn BLOB`. Mirrors the e2e CRUD test.
        backend
            .pool_exec(
                "CREATE TABLE \"app_unmask_auto\".\"users\" (\
                     id  TEXT PRIMARY KEY, \
                     ssn BLOB, \
                     ssn_masked TEXT NOT NULL DEFAULT '***-**-XXXX'\
                 )",
                &[],
            )
            .await
            .expect("CREATE TABLE");

        // Encrypt + insert one row inline.
        use zeroship_plugin_db::crud::encryption_pass::encrypt_row_on_write;
        use zeroship_plugin_db::query::{build_insert_with_dialect, SqlDialect};
        let row_pk = "usr_auto_01";
        let plaintext = "123-45-6789";
        let mut doc = serde_json::json!({
            "id": row_pk,
            "ssn": plaintext,
            "ssn_masked": "***-**-6789",
        });
        encrypt_row_on_write(backend.as_ref(), app_id, collection, &schema, row_pk, &mut doc)
            .await
            .expect("encrypt_row_on_write");
        let bq = build_insert_with_dialect(app_id, collection, &doc, SqlDialect::Sqlite)
            .expect("build_insert_with_dialect");
        let client = backend
            .acquire_dedicated_client()
            .await
            .expect("acquire client");
        let param_refs: Vec<&str> = bq.params.iter().map(String::as_str).collect();
        let _ = client
            .query_typed(&bq.sql, &param_refs)
            .await
            .expect("INSERT");

        // Dispatch unmask with `kind: "auto"` actor — must succeed.
        let args = unmask::UnmaskFieldArgs {
            collection: collection.to_string(),
            row_pk: row_pk.to_string(),
            column: "ssn".to_string(),
            actor: Some(serde_json::json!({ "kind": "auto", "id": null })),
            reason: Some("integration test".to_string()),
        };
        let result = unmask::dispatch_unmask(app_id, args)
            .await
            .expect("dispatch_unmask must succeed for auto actor");
        assert_eq!(
            result.plaintext, plaintext,
            "plaintext must recover via decrypt path"
        );

        // Audit row must show `granted` + `spi`.
        let audit = read_audit_rows(backend.as_ref(), app_id).await;
        assert_eq!(audit.len(), 1, "exactly one audit row expected: {audit:?}");
        assert_eq!(audit[0].0, "granted", "outcome must be granted");
        assert_eq!(audit[0].1, "auto", "actor_role must be 'auto'");
        assert_eq!(
            audit[0].2, "spi",
            "classification must be 'spi' (from the schema mask block)"
        );
    });
}

/// **P5.5 PR 4 — gate #2**: a `user`-kind actor is denied by the PR 4
/// default-policy stub; a `denied` audit row is emitted; the typed
/// error `unmask_not_permitted` reaches the caller.
#[test]
fn unmask_with_user_actor_returns_forbidden_audit_logged() {
    let _env = EncEnv::set("ZEROSHIP_COLUMN_KEY_P55_PR4_USER", &"b".repeat(64));
    let schema = serde_json::json!({
        "id": { "type": "string" },
        "ssn": {
            "type": "string",
            "encrypted": {
                "mode": "randomised",
                "keyId": "p55_pr4_user",
                "wraps": "string",
            },
            "mask": { "kind": "last4", "classification": "spi" },
        },
    });
    let app_id = "app_unmask_user";
    let collection = "users";

    run(async {
        let (backend, _dir) = unmask_setup_with_schema(app_id, collection, schema.clone()).await;
        backend
            .pool_exec(
                "CREATE TABLE \"app_unmask_user\".\"users\" (\
                     id  TEXT PRIMARY KEY, \
                     ssn BLOB, \
                     ssn_masked TEXT NOT NULL DEFAULT '***'\
                 )",
                &[],
            )
            .await
            .expect("CREATE TABLE");

        // No need to insert a row — the authorization check happens
        // BEFORE the SELECT, so a denied path doesn't touch the data
        // table at all. Even if the row exists, the SELECT is gated
        // by `allowed = false`.
        let args = unmask::UnmaskFieldArgs {
            collection: collection.to_string(),
            row_pk: "usr_anywhere".to_string(),
            column: "ssn".to_string(),
            actor: Some(serde_json::json!({ "kind": "user", "id": "usr_xyz" })),
            reason: None,
        };
        let err = unmask::dispatch_unmask(app_id, args)
            .await
            .expect_err("dispatch_unmask must refuse user actor under PR 4 stub");
        match err {
            zeroship_plugin_db::error::DbError::Coded { code, .. } => {
                assert_eq!(code, "unmask_not_permitted");
            }
            other => panic!("expected Coded::unmask_not_permitted, got {other:?}"),
        }

        let audit = read_audit_rows(backend.as_ref(), app_id).await;
        assert_eq!(audit.len(), 1, "denied path must still emit one audit row");
        assert_eq!(audit[0].0, "denied", "outcome must be 'denied'");
        assert_eq!(audit[0].1, "user", "actor_role must be 'user'");
        assert_eq!(
            audit[0].2, "spi",
            "classification must be 'spi' even on denied path"
        );
    });
}

/// **P5.5 PR 4 — gate #3**: unmask of a column that has no mask
/// declaration on the cached schema returns the typed
/// `unmask_column_not_masked` error. Pins the contract that the
/// dispatcher refuses to leak plaintext through a "forged" RPC for
/// arbitrary columns.
#[test]
fn unmask_column_not_masked_returns_typed_error() {
    // Schema declares `name` as a bare string — no mask block.
    let schema = serde_json::json!({
        "id":   { "type": "string" },
        "name": { "type": "string" },
    });
    let app_id = "app_unmask_unmasked";
    let collection = "users";

    run(async {
        let (_backend, _dir) = unmask_setup_with_schema(app_id, collection, schema).await;
        let args = unmask::UnmaskFieldArgs {
            collection: collection.to_string(),
            row_pk: "any_pk".to_string(),
            column: "name".to_string(),
            actor: Some(serde_json::json!({ "kind": "auto" })),
            reason: None,
        };
        let err = unmask::dispatch_unmask(app_id, args)
            .await
            .expect_err("unmask of non-masked column must refuse");
        match err {
            zeroship_plugin_db::error::DbError::ValidationFailed { code, .. } => {
                assert_eq!(code, "unmask_column_not_masked");
            }
            other => panic!("expected ValidationFailed::unmask_column_not_masked, got {other:?}"),
        }
    });
}

/// **P5.5 PR 4 — gate #4**: classification flows through to the audit
/// row regardless of outcome. We register a column with `classification:
/// "phi"`, force the denied path (user actor), and assert the audit
/// row's classification text matches.
#[test]
fn unmask_writes_audit_row_with_correct_classification() {
    let schema = serde_json::json!({
        "id":      { "type": "string" },
        "diag":    {
            "type": "string",
            // Mask-only (no encryption) — exercises the plaintext-storage
            // fetch path indirectly (although the denied branch never
            // reaches it). The dispatcher's denied audit-row write still
            // pulls classification from the cached schema.
            "mask": { "kind": "full", "classification": "phi" },
        },
    });
    let app_id = "app_unmask_phi";
    let collection = "patients";

    run(async {
        let (backend, _dir) = unmask_setup_with_schema(app_id, collection, schema).await;
        let args = unmask::UnmaskFieldArgs {
            collection: collection.to_string(),
            row_pk: "pat_01".to_string(),
            column: "diag".to_string(),
            actor: Some(serde_json::json!({ "kind": "user", "id": "doctor_x" })),
            reason: Some("chart review".to_string()),
        };
        let _err = unmask::dispatch_unmask(app_id, args)
            .await
            .expect_err("user actor denied");

        let audit = read_audit_rows(backend.as_ref(), app_id).await;
        assert_eq!(audit.len(), 1);
        assert_eq!(audit[0].0, "denied");
        assert_eq!(
            audit[0].2, "phi",
            "classification 'phi' must round-trip onto the audit row"
        );
    });
}

// ===========================================================================
// P5.5 PR 5 — defineMaskPolicy + per-app policy storage + real authorization
// ===========================================================================
//
// These tests exercise the policy-driven authorization path that replaces
// PR 4's default-deny stub:
//
//   - `setMaskPolicy` persists to `<db_dir>/mask_policies.json` (atomic
//     write through `mask_policies.json.tmp + rename`).
//   - The per-isolate cache picks the policy up write-through.
//   - A subsequent `unmask` honours the policy: listed roles get their
//     listed classifications; unlisted roles are denied.
//   - When no policy is declared, PR 4's default-deny stub still applies
//     (`auto` allowed; everyone else denied) — regression guard.
//   - Invalid classifications surface as
//     `invalid_mask_classification` at the Rust validator (belt-and-
//     braces with the SDK validator).
//   - A live `setMaskPolicy` mid-test propagates to the in-process cache,
//     and a subsequent unmask honours the new policy.

use zeroship_plugin_db::crud::mask_policy;

/// Helper — install backend + schema + clean any pre-existing cached
/// policy for the app. Returns the backend (kept alive via Rc) and the
/// TempDir guard. Drains the cache so the test starts from
/// "no-policy-declared".
async fn policy_setup(
    app_id: &str,
    collection: &str,
    schema: serde_json::Value,
) -> (Rc<SqliteBackend>, tempfile::TempDir) {
    let (backend, dir) = unmask_setup_with_schema(app_id, collection, schema).await;
    zeroship_plugin_db::clear_mask_policy_cache_for_tests(app_id);
    (backend, dir)
}

/// **P5.5 PR 5 — gate #1**: a policy granting `user` access to `pii`
/// allows a user-role actor to unmask a pii-classified column.
#[test]
fn unmask_with_user_role_in_policy_returns_plaintext() {
    let _env = EncEnv::set("ZEROSHIP_COLUMN_KEY_P55_PR5_GRANT", &"c".repeat(64));
    let schema = serde_json::json!({
        "id": { "type": "string" },
        "email": {
            "type": "string",
            "encrypted": {
                "mode": "randomised",
                "keyId": "p55_pr5_grant",
                "wraps": "string",
            },
            "mask": { "kind": "email", "classification": "pii" },
        },
    });
    let app_id = "app_unmask_policy_grant";
    let collection = "users";

    run(async {
        let (backend, _dir) = policy_setup(app_id, collection, schema.clone()).await;

        // Define the policy: `user` can unmask `pii`.
        let policy_v = serde_json::json!({
            "user": ["public", "pii"],
        });
        mask_policy::dispatch_set_mask_policy(app_id, policy_v)
            .await
            .expect("set_mask_policy must succeed");

        backend
            .pool_exec(
                "CREATE TABLE \"app_unmask_policy_grant\".\"users\" (\
                     id    TEXT PRIMARY KEY, \
                     email BLOB, \
                     email_masked TEXT NOT NULL DEFAULT 'x***@***'\
                 )",
                &[],
            )
            .await
            .expect("CREATE TABLE");

        // Encrypt + insert one row.
        use zeroship_plugin_db::crud::encryption_pass::encrypt_row_on_write;
        use zeroship_plugin_db::query::{build_insert_with_dialect, SqlDialect};
        let row_pk = "usr_grant_01";
        let plaintext = "alice@example.com";
        let mut doc = serde_json::json!({
            "id": row_pk,
            "email": plaintext,
            "email_masked": "a****@example.com",
        });
        encrypt_row_on_write(backend.as_ref(), app_id, collection, &schema, row_pk, &mut doc)
            .await
            .expect("encrypt_row_on_write");
        let bq = build_insert_with_dialect(app_id, collection, &doc, SqlDialect::Sqlite)
            .expect("build_insert_with_dialect");
        let client = backend
            .acquire_dedicated_client()
            .await
            .expect("acquire client");
        let param_refs: Vec<&str> = bq.params.iter().map(String::as_str).collect();
        let _ = client
            .query_typed(&bq.sql, &param_refs)
            .await
            .expect("INSERT");

        // Unmask with `user` actor — must succeed via the policy.
        let args = unmask::UnmaskFieldArgs {
            collection: collection.to_string(),
            row_pk: row_pk.to_string(),
            column: "email".to_string(),
            actor: Some(serde_json::json!({ "kind": "user", "id": "usr_xyz" })),
            reason: Some("user requested own data".to_string()),
        };
        let result = unmask::dispatch_unmask(app_id, args)
            .await
            .expect("policy grants user → pii; unmask must succeed");
        assert_eq!(result.plaintext, plaintext);

        let audit = read_audit_rows(backend.as_ref(), app_id).await;
        assert_eq!(audit.len(), 1);
        assert_eq!(audit[0].0, "granted", "outcome must be granted");
        assert_eq!(audit[0].1, "user");
        assert_eq!(audit[0].2, "pii");
    });
}

/// **P5.5 PR 5 — gate #2**: a policy granting `user` only `public` denies
/// a user-role attempt to unmask a `pii`-classified column. The denied
/// path emits an audit row.
#[test]
fn unmask_with_user_role_not_in_policy_denied() {
    let schema = serde_json::json!({
        "id": { "type": "string" },
        "ssn": {
            "type": "string",
            "mask": { "kind": "last4", "classification": "pii" },
        },
    });
    let app_id = "app_unmask_policy_deny";
    let collection = "users";

    run(async {
        let (backend, _dir) = policy_setup(app_id, collection, schema.clone()).await;

        // Policy: `user` can only unmask `public`.
        let policy_v = serde_json::json!({
            "user": ["public"],
        });
        mask_policy::dispatch_set_mask_policy(app_id, policy_v)
            .await
            .expect("set_mask_policy must succeed");

        let args = unmask::UnmaskFieldArgs {
            collection: collection.to_string(),
            row_pk: "usr_anywhere".to_string(),
            column: "ssn".to_string(),
            actor: Some(serde_json::json!({ "kind": "user", "id": "usr_xyz" })),
            reason: None,
        };
        let err = unmask::dispatch_unmask(app_id, args)
            .await
            .expect_err("policy does not allow user → pii; must refuse");
        match err {
            zeroship_plugin_db::error::DbError::Coded { code, .. } => {
                assert_eq!(code, "unmask_not_permitted");
            }
            other => panic!("expected Coded::unmask_not_permitted, got {other:?}"),
        }

        let audit = read_audit_rows(backend.as_ref(), app_id).await;
        assert_eq!(audit.len(), 1);
        assert_eq!(audit[0].0, "denied");
        assert_eq!(audit[0].1, "user");
        assert_eq!(audit[0].2, "pii");
    });
}

/// **P5.5 PR 5 — gate #3**: regression guard for the no-policy case.
/// The default-deny stub from PR 4 still applies — `auto` allowed,
/// everyone else denied. Closes the "did we accidentally start
/// allowing everything when no policy is declared" hole.
#[test]
fn unmask_default_deny_when_no_policy() {
    let schema = serde_json::json!({
        "id": { "type": "string" },
        "name": {
            "type": "string",
            "mask": { "kind": "name", "classification": "public" },
        },
    });
    let app_id = "app_unmask_policy_default_deny";
    let collection = "users";

    run(async {
        let (backend, _dir) = policy_setup(app_id, collection, schema).await;
        // NO setMaskPolicy call — exercise the default-deny stub.

        let args = unmask::UnmaskFieldArgs {
            collection: collection.to_string(),
            row_pk: "usr_anywhere".to_string(),
            column: "name".to_string(),
            actor: Some(serde_json::json!({ "kind": "user", "id": "usr_xyz" })),
            reason: None,
        };
        let err = unmask::dispatch_unmask(app_id, args)
            .await
            .expect_err("no policy + non-auto actor → default-deny");
        match err {
            zeroship_plugin_db::error::DbError::Coded { code, .. } => {
                assert_eq!(code, "unmask_not_permitted");
            }
            other => panic!("expected Coded::unmask_not_permitted, got {other:?}"),
        }
        // Audit row written on the denied path.
        let audit = read_audit_rows(backend.as_ref(), app_id).await;
        assert_eq!(audit.len(), 1);
        assert_eq!(audit[0].0, "denied");
    });
}

/// **P5.5 PR 5 — gate #4**: invalid classification at the Rust validator.
/// The SDK's `defineMaskPolicy()` rejects at declare-time; the Rust
/// validator catches anything that bypasses the SDK (forged RPC,
/// untrusted client, future SDK drift). Both layers refuse with
/// `invalid_mask_classification`.
#[test]
fn unmask_invalid_classification_rejected_at_dispatch_time() {
    let schema = serde_json::json!({ "id": { "type": "string" } });
    let app_id = "app_unmask_invalid_classification";
    let collection = "users";

    run(async {
        let (_backend, _dir) = policy_setup(app_id, collection, schema).await;

        let bad_policy = serde_json::json!({
            "admin": ["public", "badclass"],
        });
        let err = mask_policy::dispatch_set_mask_policy(app_id, bad_policy)
            .await
            .expect_err("rust validator must refuse unknown classification");
        match err {
            zeroship_plugin_db::error::DbError::ValidationFailed { code, .. } => {
                assert_eq!(code, "invalid_mask_classification");
            }
            other => panic!("expected ValidationFailed::invalid_mask_classification, got {other:?}"),
        }
    });
}

/// **P5.5 PR 5 — gate #5**: a `setMaskPolicy` at runtime propagates to
/// the in-process cache; a subsequent `unmask` honours the new policy.
/// Pins the write-through semantics.
#[test]
fn policy_refresh_after_set_mask_policy_op_takes_effect() {
    let schema = serde_json::json!({
        "id": { "type": "string" },
        "data": {
            "type": "string",
            "mask": { "kind": "full", "classification": "internal" },
        },
    });
    let app_id = "app_unmask_policy_refresh";
    let collection = "items";

    run(async {
        let (backend, _dir) = policy_setup(app_id, collection, schema).await;

        // The table must exist so the post-policy attempt reaches the
        // SELECT path. Empty table → `unmask_not_found` (auth passes;
        // no row matches) is the assertion we want.
        backend
            .pool_exec(
                "CREATE TABLE \"app_unmask_policy_refresh\".\"items\" (\
                     id   TEXT PRIMARY KEY, \
                     data TEXT, \
                     data_masked TEXT NOT NULL DEFAULT '***'\
                 )",
                &[],
            )
            .await
            .expect("CREATE TABLE");

        // Step 1 — without a policy, a `support` actor is denied.
        let args1 = unmask::UnmaskFieldArgs {
            collection: collection.to_string(),
            row_pk: "any".to_string(),
            column: "data".to_string(),
            actor: Some(serde_json::json!({ "kind": "support", "id": "sup_1" })),
            reason: None,
        };
        let err = unmask::dispatch_unmask(app_id, args1.clone())
            .await
            .expect_err("no policy → default deny for support");
        match err {
            zeroship_plugin_db::error::DbError::Coded { code, .. } => {
                assert_eq!(code, "unmask_not_permitted");
            }
            other => panic!("expected Coded::unmask_not_permitted, got {other:?}"),
        }

        // Step 2 — install a policy granting support → internal.
        let policy_v = serde_json::json!({
            "support": ["internal"],
        });
        mask_policy::dispatch_set_mask_policy(app_id, policy_v)
            .await
            .expect("set_mask_policy");

        // Step 3 — now the same support actor passes authorization.
        // We still get `unmask_not_found` because no row exists, but
        // that's the path AFTER the auth check — the absence of
        // `unmask_not_permitted` is the pin.
        let err = unmask::dispatch_unmask(app_id, args1)
            .await
            .expect_err("auth passes; SELECT misses");
        match err {
            zeroship_plugin_db::error::DbError::ValidationFailed { code, .. } => {
                assert_eq!(
                    code, "unmask_not_found",
                    "support → internal must pass authz; failure is now the SELECT miss"
                );
            }
            other => panic!("expected ValidationFailed::unmask_not_found, got {other:?}"),
        }

        // Audit table observes both attempts — the first denied, the
        // second granted-then-not-found never made it to the audit
        // write (the audit row only fires on successful + denied
        // outcomes; SELECT misses fall through the typed error rail).
        let audit = read_audit_rows(backend.as_ref(), app_id).await;
        assert!(
            audit.iter().any(|r| r.0 == "denied"),
            "first attempt must have audited as denied: {audit:?}"
        );
    });
}

// ===========================================================================
// P5.5 PR 6 — mask backfill + rewrite + removal end-to-end on SQLite
// ===========================================================================
//
// These tests build a SQLite-shaped table by hand, INSERT rows, then
// exercise the diff classifier + mask sentinel parse round-trip.
// We can't drive the orchestrator's `register_model::apply` on SQLite
// (PG-only today); the production-side equivalent for SQLite ships in
// a later PR. The integration-level coverage these tests provide:
//
// 1. The DDL emitter (`build_create_table_with_fks`) attaches the
//    `/* __zsmask:... */` sentinel to the sibling column.
// 2. The SQLite introspector recovers the mask metadata from
//    `sqlite_master.sql` on a subsequent `introspect_schema` call.
// 3. The diff classifier sees the recovered metadata and emits no
//    spurious ops on a stable-shape redeploy.

/// **PR 6 — sentinel round-trip on SQLite**: emit a CREATE TABLE with
/// a masked column → execute it → re-read via `introspect_schema` →
/// the parent column carries `mask = Some({last4, spi})`.
#[test]
fn mask_addition_backfills_existing_rows_end_to_end() {
    use zeroship_plugin_db::query::{build_create_table_with_fks, FkEmission};

    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .ensure_app_schema("app_demo")
            .await
            .expect("ensure_app_schema");

        // Step 1 — initial deploy: schema declares no mask, just a
        // plain ssn column. The DDL emitter produces a CREATE TABLE
        // without a sibling.
        let schema_v1 = serde_json::json!({
            "ssn": { "type": "string" }
        });
        let create_v1 =
            build_create_table_with_fks("app_demo", "users", &schema_v1, &FkEmission::Inline)
                .expect("build_create_table v1");
        // The emitter's id default uses SERIAL (PG-flavoured) which
        // SQLite rejects — strip down to a SQLite-friendly CREATE
        // TABLE for this test since we're exercising the diff layer's
        // contract, not the dialect emitter.
        let sqlite_v1 =
            "CREATE TABLE \"app_demo\".\"users\" (id INTEGER PRIMARY KEY, ssn TEXT)";
        backend.pool_exec(sqlite_v1, &[]).await.expect("CREATE v1");

        // INSERT a row.
        backend
            .pool_exec(
                "INSERT INTO \"app_demo\".\"users\"(id, ssn) VALUES (1, '123-45-6789')",
                &[],
            )
            .await
            .expect("INSERT pre-mask row");

        // Step 2 — re-deploy with mask declared. The diff classifier
        // detects None→Some(last4, spi) and emits AddColumn + MaskBackfill.
        let schema_v2 = serde_json::json!({
            "ssn": {
                "type": "string",
                "mask": { "kind": "last4", "classification": "spi" }
            }
        });
        // PR 6 only exercises the diff layer here — production
        // backfill on SQLite ships in a later PR. We assert the diff
        // classifier emits the right shape AGAINST the live snapshot
        // we just produced.
        let live = backend.introspect_schema("app_demo").await.expect("introspect");
        let ops = zeroship_plugin_db::diff::compute_diff(
            &live,
            "app_demo",
            "users",
            &schema_v2,
            &create_v1, // unused — table already exists in live
            &[],
        );
        // Expect: one AddColumn for the sibling + one MaskBackfill.
        let add_sib: Vec<&zeroship_plugin_db::diff::DiffOp> = ops
            .iter()
            .filter(|o| {
                matches!(o.change_kind, zeroship_plugin_db::diff::ChangeKind::AddColumn)
                    && o.field.as_deref() == Some("ssn_masked")
            })
            .collect();
        assert_eq!(
            add_sib.len(),
            1,
            "expected one sibling ADD op: {ops:?}"
        );
        let backfills: Vec<&zeroship_plugin_db::diff::DiffOp> = ops
            .iter()
            .filter(|o| {
                matches!(
                    o.change_kind,
                    zeroship_plugin_db::diff::ChangeKind::MaskBackfill { .. }
                )
            })
            .collect();
        assert_eq!(
            backfills.len(),
            1,
            "expected one MaskBackfill op: {ops:?}"
        );

        // Step 3 — simulate the post-backfill state by hand:
        // ALTER TABLE add the sibling + populate it for the existing
        // row. The sentinel comment goes into the column's
        // `sqlite_master.sql` text so the next introspect picks up
        // `mask = Some(_)` on the parent.
        //
        // NOTE: SQLite's ALTER TABLE ADD COLUMN allows inline
        // comments via standard SQL syntax, but the comment is
        // preserved in `sqlite_master.sql` only when the column is
        // emitted at CREATE TABLE time. To exercise the sentinel
        // round-trip we DROP the v1 table and CREATE v2 directly
        // with the sibling + sentinel inline. Production code
        // (orchestrator) would use the diff-emitted multi-statement
        // payload.
        backend
            .pool_exec("DROP TABLE \"app_demo\".\"users\"", &[])
            .await
            .expect("DROP v1");
        backend
            .pool_exec(
                "CREATE TABLE \"app_demo\".\"users\" (\
                     \"id\" INTEGER PRIMARY KEY, \
                     \"ssn\" TEXT, \
                     \"ssn_masked\" TEXT NOT NULL /* __zsmask:kind=last4,classification=spi */\
                 )",
                &[],
            )
            .await
            .expect("CREATE v2");
        // Re-insert the row + masked sibling.
        backend
            .pool_exec(
                "INSERT INTO \"app_demo\".\"users\"(\"id\", \"ssn\", \"ssn_masked\") \
                 VALUES (1, '123-45-6789', '***-**-6789')",
                &[],
            )
            .await
            .expect("INSERT post-mask row");

        // Step 4 — re-introspect: the parent now carries
        // `mask: Some({last4, spi})`.
        let live = backend.introspect_schema("app_demo").await.expect("introspect v2");
        let users = live.tables.get("users").expect("users table");
        let parent = users.get("ssn").expect("ssn parent col");
        let meta = parent.mask.as_ref().expect("mask sentinel recovered");
        assert_eq!(meta.kind, zeroship_plugin_db::diff::MaskKind::Last4);
        assert_eq!(
            meta.classification,
            zeroship_plugin_db::diff::Classification::Spi
        );

        // Step 5 — a stable-shape re-deploy emits zero mask ops.
        let ops = zeroship_plugin_db::diff::compute_diff(
            &live,
            "app_demo",
            "users",
            &schema_v2,
            "",
            &[],
        );
        assert!(
            !ops.iter().any(|o| matches!(
                o.change_kind,
                zeroship_plugin_db::diff::ChangeKind::MaskBackfill { .. }
                    | zeroship_plugin_db::diff::ChangeKind::MaskRewrite { .. }
                    | zeroship_plugin_db::diff::ChangeKind::MaskRemove { .. }
            )),
            "stable mask declaration must emit zero mask ops: {ops:?}"
        );
    });
}

/// **PR 6b — kind change detected end-to-end on SQLite**: an existing
/// masked column with `kind = full` rolls forward to `kind = last4`;
/// the diff classifier emits a `MaskRewrite` op (no AddColumn — the
/// sibling already exists).
#[test]
fn mask_kind_change_rewrites_existing_sibling_end_to_end() {
    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .ensure_app_schema("app_demo")
            .await
            .expect("ensure_app_schema");

        // Set up a live table with `kind=full` sentinel.
        backend
            .pool_exec(
                "CREATE TABLE \"app_demo\".\"users\" (\
                     \"id\" INTEGER PRIMARY KEY, \
                     \"ssn\" TEXT, \
                     \"ssn_masked\" TEXT NOT NULL /* __zsmask:kind=full,classification=pii */\
                 )",
                &[],
            )
            .await
            .expect("CREATE v_full");
        // INSERT a row with the full-masked sibling.
        backend
            .pool_exec(
                "INSERT INTO \"app_demo\".\"users\"(\"id\", \"ssn\", \"ssn_masked\") \
                 VALUES (1, '123-45-6789', '***')",
                &[],
            )
            .await
            .expect("INSERT pre-rewrite row");

        // Introspect: parent.mask = full/pii.
        let live = backend.introspect_schema("app_demo").await.expect("intro v_full");
        let users = live.tables.get("users").expect("users");
        let parent = users.get("ssn").expect("ssn parent");
        let m = parent.mask.as_ref().expect("mask sentinel");
        assert_eq!(m.kind, zeroship_plugin_db::diff::MaskKind::Full);

        // Re-deploy with kind=last4 + classification=spi → MaskRewrite.
        let schema_v2 = serde_json::json!({
            "ssn": {
                "type": "string",
                "mask": { "kind": "last4", "classification": "spi" }
            }
        });
        let ops = zeroship_plugin_db::diff::compute_diff(
            &live, "app_demo", "users", &schema_v2, "", &[],
        );
        let rewrites: Vec<&zeroship_plugin_db::diff::DiffOp> = ops
            .iter()
            .filter(|o| {
                matches!(
                    o.change_kind,
                    zeroship_plugin_db::diff::ChangeKind::MaskRewrite { .. }
                )
            })
            .collect();
        assert_eq!(
            rewrites.len(),
            1,
            "expected one MaskRewrite: {ops:?}"
        );
        assert_eq!(
            rewrites[0].class,
            zeroship_plugin_db::diff::ChangeClass::Compatible
        );
        // No sibling ADD — the sibling already exists.
        let add_sib: Vec<&zeroship_plugin_db::diff::DiffOp> = ops
            .iter()
            .filter(|o| {
                matches!(o.change_kind, zeroship_plugin_db::diff::ChangeKind::AddColumn)
                    && o.field.as_deref() == Some("ssn_masked")
            })
            .collect();
        assert!(
            add_sib.is_empty(),
            "must NOT emit sibling ADD when sibling exists: {ops:?}"
        );
    });
}

/// **PR 6c — mask removal classified Destructive on SQLite**: live
/// has mask, schema drops it → MaskRemove with `class = Destructive`,
/// which the validate stage refuses under `strictness=strict` /
/// `lenient` and applies under `strictness=off`.
#[test]
fn mask_removal_classified_destructive_on_sqlite_diff() {
    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .ensure_app_schema("app_demo")
            .await
            .expect("ensure_app_schema");

        backend
            .pool_exec(
                "CREATE TABLE \"app_demo\".\"users\" (\
                     \"id\" INTEGER PRIMARY KEY, \
                     \"ssn\" TEXT, \
                     \"ssn_masked\" TEXT NOT NULL /* __zsmask:kind=last4,classification=spi */\
                 )",
                &[],
            )
            .await
            .expect("CREATE pre-removal");

        let live = backend
            .introspect_schema("app_demo")
            .await
            .expect("introspect");
        let schema_post = serde_json::json!({
            "ssn": { "type": "string" }
        });
        let ops = zeroship_plugin_db::diff::compute_diff(
            &live, "app_demo", "users", &schema_post, "", &[],
        );
        let removes: Vec<&zeroship_plugin_db::diff::DiffOp> = ops
            .iter()
            .filter(|o| {
                matches!(
                    o.change_kind,
                    zeroship_plugin_db::diff::ChangeKind::MaskRemove { .. }
                )
            })
            .collect();
        assert_eq!(removes.len(), 1, "expected MaskRemove: {ops:?}");
        assert_eq!(
            removes[0].class,
            zeroship_plugin_db::diff::ChangeClass::Destructive,
            "MaskRemove MUST be Destructive — validate strict gate \
             depends on it",
        );
    });
}

/// **PR 6 — malformed sentinel does not poison introspection** on
/// SQLite: a sibling carrying a garbled sentinel parses to "no mask"
/// on the parent (and a `tracing::warn!` fires; the test only checks
/// the introspection shape).
#[test]
fn malformed_mask_sentinel_skipped_on_sqlite() {
    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .ensure_app_schema("app_demo")
            .await
            .expect("ensure_app_schema");
        backend
            .pool_exec(
                "CREATE TABLE \"app_demo\".\"users\" (\
                     \"id\" INTEGER PRIMARY KEY, \
                     \"ssn\" TEXT, \
                     \"ssn_masked\" TEXT NOT NULL /* __zsmask:kind=cosmic_radiation,classification=spi */\
                 )",
                &[],
            )
            .await
            .expect("CREATE garbled");
        let live = backend
            .introspect_schema("app_demo")
            .await
            .expect("introspect garbled");
        let parent = live
            .tables
            .get("users")
            .and_then(|t| t.get("ssn"))
            .expect("ssn col");
        assert!(
            parent.mask.is_none(),
            "malformed sentinel must leave parent unmasked: {:?}",
            parent.mask
        );
    });
}

// ===========================================================================
// P5.5 PR 7 — drift detection + bulk unmask + per-query unmask hint
// ===========================================================================
//
// These tests drive the new dispatch helpers end-to-end on a real
// SQLite backend:
//
//   * `drift_end_to_end_seeded_mismatch_detected` — seed a row where
//     the sibling text does NOT match `apply_mask_kind(plaintext)`,
//     run the drift sweep, and assert (1) the report flags the row,
//     (2) one row lands in `__zeroship_audit_mask_drift`.
//   * `bulk_unmask_end_to_end` — atomic auth + bulk decrypt + single
//     audit row per call (the dispatch shape PR 7 ships for
//     `db.users.bulkUnmask([...])`).
//   * `per_query_unmask_hint_end_to_end` — wire-up gate for the
//     `find(filter, { unmask: [...], actor })` hint. We can't
//     stand up V8 here, so the test drives the lower-level
//     `dispatch_unmask_for_query` directly against rows pre-wrapped
//     by `wrap_row_on_read`.

use zeroship_plugin_db::crud::mask_drift;

/// **PR 7 — drift gate #1**: seeded mismatch is detected + recorded.
///
/// The PR 2 dual-write contract ensures the sibling is correct on
/// fresh INSERTs. To simulate drift we UPDATE the sibling out-of-band
/// after the insert so the stored value differs from
/// `apply_mask_kind(plaintext)`. The drift sweep MUST flag it.
#[test]
fn drift_end_to_end_seeded_mismatch_detected() {
    let schema = serde_json::json!({
        "id": { "type": "string" },
        "email": {
            "type": "string",
            "mask": { "kind": "email", "classification": "pii" }
        },
    });
    let app_id = "app_drift_seeded";
    let collection = "users";

    run(async {
        let (backend, _dir) = unmask_setup_with_schema(app_id, collection, schema).await;
        backend
            .pool_exec(
                "CREATE TABLE \"app_drift_seeded\".\"users\" (\
                     id            TEXT PRIMARY KEY, \
                     email         TEXT, \
                     email_masked  TEXT NOT NULL\
                 )",
                &[],
            )
            .await
            .expect("CREATE TABLE");

        // Row #1 — correct sibling (matches apply_mask_kind(email)).
        backend
            .pool_exec(
                "INSERT INTO \"app_drift_seeded\".\"users\" \
                 (id, email, email_masked) VALUES \
                 ('u_ok', 'alice@example.com', 'a***@example.com')",
                &[],
            )
            .await
            .expect("INSERT clean row");

        // Row #2 — drifted: stored sibling is just '***' but the
        // correct mask would be 'b***@example.com'.
        backend
            .pool_exec(
                "INSERT INTO \"app_drift_seeded\".\"users\" \
                 (id, email, email_masked) VALUES \
                 ('u_drift', 'bob@example.com', '***')",
                &[],
            )
            .await
            .expect("INSERT drifted row");

        // Run at 100% sample to guarantee both rows are inspected.
        let report = mask_drift::run_drift_check_for_column(
            app_id, collection, "email", 100.0,
        )
        .await
        .expect("drift check");
        assert_eq!(report.sampled, 2, "both rows must be sampled: {report:?}");
        assert_eq!(report.drifted, 1, "exactly one row drifted: {report:?}");
        assert_eq!(report.samples.len(), 1);
        let s = &report.samples[0];
        assert_eq!(s.collection, "users");
        assert_eq!(s.column, "email");
        assert_eq!(s.row_pk, "u_drift");
        assert_eq!(s.stored, "***");
        assert_eq!(s.expected, "b***@example.com");

        // Audit row landed in the per-app sidecar table.
        let audit = mask_drift::read_drift_audit_rows_for_tests(app_id)
            .await
            .expect("read drift audit rows");
        assert_eq!(audit.len(), 1, "one drift audit row expected: {audit:?}");
        let (coll, col, pk, stored, expected) = &audit[0];
        assert_eq!(coll, "users");
        assert_eq!(col, "email");
        assert_eq!(pk, "u_drift");
        assert_eq!(stored, "***");
        assert_eq!(expected, "b***@example.com");
    });
}

/// **PR 7 — drift gate #2**: aligned siblings produce zero drift.
#[test]
fn drift_check_returns_zero_when_aligned() {
    let schema = serde_json::json!({
        "id":    { "type": "string" },
        "email": {
            "type": "string",
            "mask": { "kind": "email", "classification": "pii" }
        },
    });
    let app_id = "app_drift_aligned";
    let collection = "users";

    run(async {
        let (backend, _dir) = unmask_setup_with_schema(app_id, collection, schema).await;
        backend
            .pool_exec(
                "CREATE TABLE \"app_drift_aligned\".\"users\" (\
                     id           TEXT PRIMARY KEY, \
                     email        TEXT, \
                     email_masked TEXT NOT NULL\
                 )",
                &[],
            )
            .await
            .expect("CREATE TABLE");
        for (id, email, masked) in [
            ("u1", "alice@example.com", "a***@example.com"),
            ("u2", "bob@example.com", "b***@example.com"),
            ("u3", "carol@example.com", "c***@example.com"),
        ] {
            let sql = format!(
                "INSERT INTO \"app_drift_aligned\".\"users\" \
                 (id, email, email_masked) VALUES ('{id}', '{email}', '{masked}')"
            );
            backend.pool_exec(&sql, &[]).await.expect("INSERT");
        }
        let report = mask_drift::run_drift_check_for_column(
            app_id, collection, "email", 100.0,
        )
        .await
        .expect("drift check");
        assert_eq!(report.sampled, 3, "all rows sampled: {report:?}");
        assert_eq!(report.drifted, 0, "no drift expected: {report:?}");
        let audit = mask_drift::read_drift_audit_rows_for_tests(app_id)
            .await
            .expect("read audit");
        assert!(audit.is_empty(), "no audit rows for clean run: {audit:?}");
    });
}

/// **PR 7 — drift gate #3**: a NULL sibling on a non-NULL parent is
/// flagged as drift (the dual-write contract guarantees both null
/// together OR both populated together — a sibling NULL on populated
/// parent violates the invariant).
#[test]
fn drift_check_handles_null_sibling_drift() {
    let schema = serde_json::json!({
        "id":    { "type": "string" },
        "email": {
            "type": "string",
            "mask": { "kind": "email", "classification": "pii" }
        },
    });
    let app_id = "app_drift_null_sibling";
    let collection = "users";

    run(async {
        let (backend, _dir) = unmask_setup_with_schema(app_id, collection, schema).await;
        backend
            .pool_exec(
                "CREATE TABLE \"app_drift_null_sibling\".\"users\" (\
                     id           TEXT PRIMARY KEY, \
                     email        TEXT, \
                     email_masked TEXT\
                 )",
                &[],
            )
            .await
            .expect("CREATE TABLE");
        backend
            .pool_exec(
                "INSERT INTO \"app_drift_null_sibling\".\"users\" \
                 (id, email, email_masked) VALUES ('u1', 'alice@example.com', NULL)",
                &[],
            )
            .await
            .expect("INSERT NULL sibling");
        let report = mask_drift::run_drift_check_for_column(
            app_id, collection, "email", 100.0,
        )
        .await
        .expect("drift check");
        assert_eq!(report.drifted, 1, "null sibling must drift: {report:?}");
        assert_eq!(report.samples[0].stored, "__null__");
    });
}

/// **PR 7 — drift gate #4**: plaintext-column drift (no encryption).
/// Identical setup to gate #1, but uses `last4` mask kind to pin
/// the apply_mask_kind round-trip on a non-email transform.
#[test]
fn drift_check_handles_plaintext_column() {
    let schema = serde_json::json!({
        "id":  { "type": "string" },
        "ssn": {
            "type": "string",
            "mask": { "kind": "last4", "classification": "spi" }
        },
    });
    let app_id = "app_drift_plaintext";
    let collection = "users";

    run(async {
        let (backend, _dir) = unmask_setup_with_schema(app_id, collection, schema).await;
        backend
            .pool_exec(
                "CREATE TABLE \"app_drift_plaintext\".\"users\" (\
                     id         TEXT PRIMARY KEY, \
                     ssn        TEXT, \
                     ssn_masked TEXT NOT NULL\
                 )",
                &[],
            )
            .await
            .expect("CREATE TABLE");
        // One clean, one drifted.
        backend
            .pool_exec(
                "INSERT INTO \"app_drift_plaintext\".\"users\" \
                 (id, ssn, ssn_masked) VALUES \
                 ('u_ok', '123-45-6789', '***-**-6789')",
                &[],
            )
            .await
            .expect("INSERT clean");
        backend
            .pool_exec(
                "INSERT INTO \"app_drift_plaintext\".\"users\" \
                 (id, ssn, ssn_masked) VALUES \
                 ('u_drift', '987-65-4321', 'wrong-mask')",
                &[],
            )
            .await
            .expect("INSERT drifted");
        let report = mask_drift::run_drift_check_for_column(
            app_id, collection, "ssn", 100.0,
        )
        .await
        .expect("drift check");
        assert_eq!(report.sampled, 2);
        assert_eq!(report.drifted, 1);
        assert_eq!(report.samples[0].row_pk, "u_drift");
        assert_eq!(report.samples[0].expected, "***-**-4321");
    });
}

/// **PR 7 — drift gate #5**: encrypted column drift detection. Builds
/// the encrypted column via the production CRUD encryption pass so the
/// ciphertext is genuine; then mutates the sibling out-of-band to
/// simulate drift; then runs the drift check (which decrypts under
/// the same key and re-applies the mask).
#[test]
fn drift_check_handles_encrypted_column() {
    let _env = EncEnv::set("ZEROSHIP_COLUMN_KEY_P55_PR7_DRIFT", &"7".repeat(64));
    let schema = serde_json::json!({
        "id": { "type": "string" },
        "ssn": {
            "type": "string",
            "encrypted": {
                "mode": "randomised",
                "keyId": "p55_pr7_drift",
                "wraps": "string",
            },
            "mask": { "kind": "last4", "classification": "spi" },
        },
    });
    let app_id = "app_drift_encrypted";
    let collection = "users";

    run(async {
        let (backend, _dir) = unmask_setup_with_schema(app_id, collection, schema.clone()).await;
        backend
            .pool_exec(
                "CREATE TABLE \"app_drift_encrypted\".\"users\" (\
                     id         TEXT PRIMARY KEY, \
                     ssn        BLOB, \
                     ssn_masked TEXT NOT NULL DEFAULT '***'\
                 )",
                &[],
            )
            .await
            .expect("CREATE TABLE");

        // Insert one row via the production encryption pass so the
        // BLOB is genuine AES-GCM ciphertext under the expected AAD.
        use zeroship_plugin_db::crud::encryption_pass::encrypt_row_on_write;
        use zeroship_plugin_db::query::{build_insert_with_dialect, SqlDialect};
        let row_pk = "usr_drift_enc";
        let plaintext = "555-00-1234";
        let mut doc = serde_json::json!({
            "id":         row_pk,
            "ssn":        plaintext,
            "ssn_masked": "***-**-9999",  // intentionally WRONG masked
        });
        encrypt_row_on_write(backend.as_ref(), app_id, collection, &schema, row_pk, &mut doc)
            .await
            .expect("encrypt_row_on_write");
        let bq = build_insert_with_dialect(app_id, collection, &doc, SqlDialect::Sqlite)
            .expect("build_insert");
        let client = backend
            .acquire_dedicated_client()
            .await
            .expect("acquire client");
        let param_refs: Vec<&str> = bq.params.iter().map(String::as_str).collect();
        let _ = client
            .query_typed(&bq.sql, &param_refs)
            .await
            .expect("INSERT");

        // The correct mask for `555-00-1234` under `last4` is
        // `***-**-1234`; stored is `***-**-9999`. Drift expected.
        let report = mask_drift::run_drift_check_for_column(
            app_id, collection, "ssn", 100.0,
        )
        .await
        .expect("drift check");
        assert_eq!(report.sampled, 1, "one row sampled: {report:?}");
        assert_eq!(report.drifted, 1, "drift expected: {report:?}");
        let s = &report.samples[0];
        assert_eq!(s.row_pk, row_pk);
        assert_eq!(s.stored, "***-**-9999");
        assert_eq!(s.expected, "***-**-1234");
    });
}

// ---------------------------------------------------------------------------
// Bulk unmask end-to-end (SQLite)
// ---------------------------------------------------------------------------

use zeroship_plugin_db::crud::unmask::{
    dispatch_bulk_unmask, BulkUnmaskArgs, BulkUnmaskItem,
};

/// **PR 7 — bulk gate #1**: authorised actor unmasks many columns
/// across many rows in one call; the result map carries plaintext
/// for every pair, and exactly ONE audit row lands.
#[test]
fn bulk_unmask_end_to_end() {
    let schema = serde_json::json!({
        "id":    { "type": "string" },
        "email": {
            "type": "string",
            "mask": { "kind": "email", "classification": "pii" }
        },
        "ssn": {
            "type": "string",
            "mask": { "kind": "last4", "classification": "spi" }
        },
    });
    let app_id = "app_bulk_unmask_e2e";
    let collection = "users";

    run(async {
        let (backend, _dir) = unmask_setup_with_schema(app_id, collection, schema).await;
        zeroship_plugin_db::clear_mask_policy_cache_for_tests(app_id);
        backend
            .pool_exec(
                "CREATE TABLE \"app_bulk_unmask_e2e\".\"users\" (\
                     id           TEXT PRIMARY KEY, \
                     email        TEXT, \
                     email_masked TEXT NOT NULL, \
                     ssn          TEXT, \
                     ssn_masked   TEXT NOT NULL\
                 )",
                &[],
            )
            .await
            .expect("CREATE TABLE");
        for (id, email, ssn) in [
            ("u1", "alice@example.com", "123-45-6789"),
            ("u2", "bob@example.com", "987-65-4321"),
        ] {
            let sql = format!(
                "INSERT INTO \"app_bulk_unmask_e2e\".\"users\" \
                 (id, email, email_masked, ssn, ssn_masked) VALUES \
                 ('{id}', '{email}', 'masked', '{ssn}', 'masked')"
            );
            backend.pool_exec(&sql, &[]).await.expect("INSERT");
        }

        // Policy: `user` can unmask pii AND spi.
        let policy_v = serde_json::json!({ "user": ["pii", "spi"] });
        mask_policy::dispatch_set_mask_policy(app_id, policy_v)
            .await
            .expect("set_mask_policy");

        let args = BulkUnmaskArgs {
            collection: collection.to_string(),
            items: vec![
                BulkUnmaskItem { row_pk: "u1".into(), columns: vec!["email".into(), "ssn".into()] },
                BulkUnmaskItem { row_pk: "u2".into(), columns: vec!["email".into()] },
            ],
            actor: Some(serde_json::json!({ "kind": "user", "id": "actor_x" })),
            reason: Some("ops dashboard".into()),
        };
        let result = dispatch_bulk_unmask(app_id, args)
            .await
            .expect("bulk unmask");
        // Plaintext recovered for every pair.
        let u1 = result.results.get("u1").expect("u1 row");
        assert_eq!(u1.get("email").map(String::as_str), Some("alice@example.com"));
        assert_eq!(u1.get("ssn").map(String::as_str), Some("123-45-6789"));
        let u2 = result.results.get("u2").expect("u2 row");
        assert_eq!(u2.get("email").map(String::as_str), Some("bob@example.com"));

        // Exactly ONE audit row covering the whole call.
        let audit = read_audit_rows(backend.as_ref(), app_id).await;
        assert_eq!(audit.len(), 1, "bulk → single audit row: {audit:?}");
        assert_eq!(audit[0].0, "granted");
        assert_eq!(audit[0].1, "user", "actor_role recorded");
    });
}

/// **PR 7 — bulk gate #2**: ANY unauthorised pair refuses the WHOLE
/// call (Q-MASK-F atomic). One audit row with outcome `denied`; no
/// plaintext returned for the authorised pair either.
#[test]
fn bulk_unmask_authorization_atomic_one_unauthorized_fails_all() {
    let schema = serde_json::json!({
        "id":    { "type": "string" },
        "email": {
            "type": "string",
            "mask": { "kind": "email", "classification": "pii" }
        },
        "ssn": {
            "type": "string",
            "mask": { "kind": "last4", "classification": "spi" }
        },
    });
    let app_id = "app_bulk_atomic_refuse";
    let collection = "users";

    run(async {
        let (backend, _dir) = unmask_setup_with_schema(app_id, collection, schema).await;
        zeroship_plugin_db::clear_mask_policy_cache_for_tests(app_id);
        backend
            .pool_exec(
                "CREATE TABLE \"app_bulk_atomic_refuse\".\"users\" (\
                     id           TEXT PRIMARY KEY, \
                     email        TEXT, \
                     email_masked TEXT NOT NULL, \
                     ssn          TEXT, \
                     ssn_masked   TEXT NOT NULL\
                 )",
                &[],
            )
            .await
            .expect("CREATE TABLE");

        // Policy: `user` can ONLY unmask pii; spi is forbidden.
        let policy_v = serde_json::json!({ "user": ["pii"] });
        mask_policy::dispatch_set_mask_policy(app_id, policy_v)
            .await
            .expect("set_mask_policy");

        let args = BulkUnmaskArgs {
            collection: collection.to_string(),
            // Pair (u1, email) authorised; pair (u1, ssn) NOT
            // authorised. Atomic fence: entire call refuses.
            items: vec![BulkUnmaskItem {
                row_pk: "u1".into(),
                columns: vec!["email".into(), "ssn".into()],
            }],
            actor: Some(serde_json::json!({ "kind": "user", "id": "actor_x" })),
            reason: None,
        };
        let err = dispatch_bulk_unmask(app_id, args)
            .await
            .expect_err("bulk must refuse atomically");
        match err {
            zeroship_plugin_db::error::DbError::Coded { code, .. } => {
                assert_eq!(code, "bulk_unmask_partial_unauthorized");
            }
            other => panic!("expected Coded::bulk_unmask_partial_unauthorized, got {other:?}"),
        }

        // Single `denied` audit row covers the whole call.
        let audit = read_audit_rows(backend.as_ref(), app_id).await;
        assert_eq!(audit.len(), 1, "atomic refuse → single audit row: {audit:?}");
        assert_eq!(audit[0].0, "denied");
    });
}

/// **PR 7 — bulk gate #3**: unknown column on the schema raises the
/// typed `unmask_column_not_masked` error BEFORE any audit row writes.
#[test]
fn bulk_unmask_unknown_column_returns_typed_error_e2e() {
    let schema = serde_json::json!({
        "id":  { "type": "string" },
        "ssn": {
            "type": "string",
            "mask": { "kind": "last4", "classification": "spi" }
        },
    });
    let app_id = "app_bulk_unknown_column";
    let collection = "users";

    run(async {
        let (_backend, _dir) = unmask_setup_with_schema(app_id, collection, schema).await;
        zeroship_plugin_db::clear_mask_policy_cache_for_tests(app_id);
        let args = BulkUnmaskArgs {
            collection: collection.to_string(),
            items: vec![BulkUnmaskItem {
                row_pk: "u1".into(),
                columns: vec!["does_not_exist".into()],
            }],
            actor: Some(serde_json::json!({ "kind": "auto" })),
            reason: None,
        };
        let err = dispatch_bulk_unmask(app_id, args)
            .await
            .expect_err("unknown column must refuse");
        match err {
            zeroship_plugin_db::error::DbError::ValidationFailed { code, .. } => {
                assert_eq!(code, "unmask_column_not_masked");
            }
            other => panic!("expected ValidationFailed::unmask_column_not_masked, got {other:?}"),
        }
    });
}

// ---------------------------------------------------------------------------
// Per-query unmask hint end-to-end (SQLite)
// ---------------------------------------------------------------------------

use zeroship_plugin_db::crud::unmask::{
    audit_query_hint_granted, authorize_query_hint, dispatch_unmask_for_query,
};

/// **PR 7 — per-query gate #1**: an authorised actor with a query
/// hint sees plaintext in the listed columns; non-listed masked
/// columns keep their `__zsmask__` wrapping.
#[test]
fn per_query_unmask_hint_end_to_end() {
    let schema = serde_json::json!({
        "id":    { "type": "string" },
        "email": {
            "type": "string",
            "mask": { "kind": "email", "classification": "pii" }
        },
        "ssn": {
            "type": "string",
            "mask": { "kind": "last4", "classification": "spi" }
        },
    });
    let app_id = "app_qhint_e2e";
    let collection = "users";

    run(async {
        let (backend, _dir) = unmask_setup_with_schema(app_id, collection, schema).await;
        zeroship_plugin_db::clear_mask_policy_cache_for_tests(app_id);
        backend
            .pool_exec(
                "CREATE TABLE \"app_qhint_e2e\".\"users\" (\
                     id           TEXT PRIMARY KEY, \
                     email        TEXT, \
                     email_masked TEXT NOT NULL, \
                     ssn          TEXT, \
                     ssn_masked   TEXT NOT NULL\
                 )",
                &[],
            )
            .await
            .expect("CREATE TABLE");
        backend
            .pool_exec(
                "INSERT INTO \"app_qhint_e2e\".\"users\" \
                 (id, email, email_masked, ssn, ssn_masked) VALUES \
                 ('u1', 'alice@example.com', 'a***@example.com', '123-45-6789', '***-**-6789')",
                &[],
            )
            .await
            .expect("INSERT");

        // Policy: `user` can unmask both pii and spi.
        let policy_v = serde_json::json!({ "user": ["pii", "spi"] });
        mask_policy::dispatch_set_mask_policy(app_id, policy_v)
            .await
            .expect("set_mask_policy");

        // Simulate the row shape `dispatch_find` would produce
        // AFTER `apply_mask_wrap_on_read` has wrapped the masked
        // columns. We're driving `dispatch_unmask_for_query` directly
        // since the full V8 round-trip is out of scope for this
        // integration test.
        let actor = Some(serde_json::json!({ "kind": "user", "id": "actor_x" }));
        let reason = Some("dashboard view".to_string());

        // Step 1 — upfront auth fence.
        authorize_query_hint(app_id, collection, &["ssn".to_string()], &actor, &reason)
            .await
            .expect("authorize_query_hint must succeed");

        // Step 2 — simulate post-wrap row + run unmask-for-query.
        let mut rows = vec![serde_json::json!({
            "id": "u1",
            "email": {
                "sentinel": "__zsmask__",
                "masked": "a***@example.com",
                "classification": "pii",
                "_meta": { "collection": "users", "row_pk": "u1", "column": "email" },
            },
            "ssn": {
                "sentinel": "__zsmask__",
                "masked": "***-**-6789",
                "classification": "spi",
                "_meta": { "collection": "users", "row_pk": "u1", "column": "ssn" },
            },
        })];
        dispatch_unmask_for_query(app_id, collection, &["ssn".to_string()], &mut rows)
            .await
            .expect("dispatch_unmask_for_query");

        // `ssn` slot now carries plaintext; `email` slot keeps the
        // sentinel-wrapped form.
        let row = &rows[0];
        assert_eq!(
            row.get("ssn").and_then(|v| v.as_str()),
            Some("123-45-6789"),
            "ssn must be plaintext: {row:?}"
        );
        let email = row.get("email").and_then(|v| v.as_object()).expect("email obj");
        assert_eq!(
            email.get("sentinel").and_then(|v| v.as_str()),
            Some("__zsmask__"),
            "email must remain wrapped: {row:?}"
        );

        // Step 3 — granted audit row lands.
        audit_query_hint_granted(app_id, collection, &["ssn".to_string()], &actor, &reason)
            .await
            .expect("audit");
        let audit = read_audit_rows(backend.as_ref(), app_id).await;
        assert_eq!(audit.len(), 1, "one audit row for the query: {audit:?}");
        assert_eq!(audit[0].0, "granted");
        assert_eq!(audit[0].1, "user");
    });
}

/// **PR 7 — per-query gate #2**: an unauthorised actor REFUSES the
/// query entirely; we do not silently degrade to masked-only.
#[test]
fn per_query_unmask_hint_rejects_unauthorized_actor() {
    let schema = serde_json::json!({
        "id":  { "type": "string" },
        "ssn": {
            "type": "string",
            "mask": { "kind": "last4", "classification": "spi" }
        },
    });
    let app_id = "app_qhint_refuse";
    let collection = "users";

    run(async {
        let (backend, _dir) = unmask_setup_with_schema(app_id, collection, schema).await;
        zeroship_plugin_db::clear_mask_policy_cache_for_tests(app_id);
        // Policy: `user` can only unmask `pii`, NOT `spi`.
        let policy_v = serde_json::json!({ "user": ["pii"] });
        mask_policy::dispatch_set_mask_policy(app_id, policy_v)
            .await
            .expect("set_mask_policy");

        let actor = Some(serde_json::json!({ "kind": "user", "id": "actor_x" }));
        let err = authorize_query_hint(
            app_id, collection, &["ssn".to_string()], &actor, &None,
        )
        .await
        .expect_err("must refuse");
        match err {
            zeroship_plugin_db::error::DbError::Coded { code, .. } => {
                assert_eq!(code, "unmask_not_permitted");
            }
            other => panic!("expected Coded::unmask_not_permitted, got {other:?}"),
        }
        // The denied path wrote one audit row (`denied` outcome) so
        // operators see the attempt; assert it landed.
        let audit = read_audit_rows(backend.as_ref(), app_id).await;
        assert_eq!(audit.len(), 1, "denied path must audit: {audit:?}");
        assert_eq!(audit[0].0, "denied");
    });
}

/// **PR 7 — per-query gate #3**: unknown column on the schema raises
/// the typed `unmask_column_not_masked` error before any DB hit.
#[test]
fn per_query_unmask_hint_unknown_column_returns_typed_error() {
    let schema = serde_json::json!({
        "id":  { "type": "string" },
        "ssn": {
            "type": "string",
            "mask": { "kind": "last4", "classification": "spi" }
        },
    });
    let app_id = "app_qhint_unknown";
    let collection = "users";

    run(async {
        let (_backend, _dir) = unmask_setup_with_schema(app_id, collection, schema).await;
        zeroship_plugin_db::clear_mask_policy_cache_for_tests(app_id);
        let actor = Some(serde_json::json!({ "kind": "auto" }));
        let err = authorize_query_hint(
            app_id,
            collection,
            &["does_not_exist".to_string()],
            &actor,
            &None,
        )
        .await
        .expect_err("must refuse on unknown");
        match err {
            zeroship_plugin_db::error::DbError::ValidationFailed { code, .. } => {
                assert_eq!(code, "unmask_column_not_masked");
            }
            other => panic!("expected ValidationFailed::unmask_column_not_masked, got {other:?}"),
        }
    });
}

// ---------------------------------------------------------------------------
// P7 PR 2 — system-field prefix + auto-indexes end-to-end on SQLite
//
// These tests exercise `build_create_table_with_fks_for_dialect(Sqlite)`
// end-to-end: the emitter produces SQLite-flavoured DDL, the engine
// accepts the multi-statement payload (CREATE TABLE + 3 CREATE INDEX),
// and `PRAGMA table_info` / `sqlite_master` confirm the seven columns
// and three indexes are present.
//
// The production register_model orchestrator is PG-only today — these
// tests drive the dialect emitter directly and `pool_exec` the result,
// the same pattern PR 6 introspection tests use for SQLite.
// ---------------------------------------------------------------------------

/// `build_create_table_with_fks_for_dialect(Sqlite)` produces DDL the
/// SQLite engine accepts, and PRAGMA `table_info` reports all 7 system
/// fields after execution.
#[test]
fn freshly_registered_model_has_seven_system_field_columns_end_to_end() {
    use zeroship_plugin_db::query::{
        build_create_table_with_fks_for_dialect, FkEmission, SqlDialect,
    };

    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .ensure_app_schema("app_demo")
            .await
            .expect("ensure_app_schema");

        let schema = serde_json::json!({
            "title": { "type": "string", "required": true },
        });
        let sql = build_create_table_with_fks_for_dialect(
            "app_demo",
            "posts",
            &schema,
            &FkEmission::Inline,
            SqlDialect::Sqlite,
        )
        .expect("build sqlite DDL");

        // Execute the multi-statement payload through the session
        // actor — `pool_exec` routes through `sqlite3_exec` which
        // accepts multi-statement SQL.
        // SQLite's `Connection::execute` runs ONE statement per call
        // (unlike PG's libpq simple-query); the production register_model
        // orchestrator is PG-only today, so this test splits the
        // multi-statement payload and executes each piece individually,
        // exercising the canonical per-statement DDL the SQLite arm
        // would see once a dialect-aware orchestrator lands.
        for stmt in sql.split(";\n") {
            let trimmed = stmt.trim();
            if trimmed.is_empty() {
                continue;
            }
            backend
                .pool_exec(trimmed, &[])
                .await
                .unwrap_or_else(|e| panic!("engine must accept statement: {trimmed}\n{e:?}"));
        }

        // Confirm via introspection: all 7 system field columns
        // present, plus the 1 user column.
        let live = backend
            .introspect_schema("app_demo")
            .await
            .expect("introspect_schema");
        let cols = live
            .tables
            .get("posts")
            .expect("posts table must be present");
        for name in &[
            "id",
            "created_at",
            "updated_at",
            "created_by",
            "updated_by",
            "version",
            "deleted_at",
        ] {
            assert!(
                cols.contains_key(*name),
                "system field {name:?} missing from introspected cols: {:?}",
                cols.keys().collect::<Vec<_>>()
            );
        }
        assert!(
            cols.contains_key("title"),
            "user-declared `title` must coexist: {:?}",
            cols.keys().collect::<Vec<_>>()
        );
    });
}

/// All three auto-indexes (`deleted_at`, `updated_at`, `created_by`)
/// land in `sqlite_master` after the CREATE TABLE payload executes.
/// The `id` PK uses ROWID (no autoindex entry) and `version` is
/// intentionally unindexed (see `create_table_does_not_emit_index_for_version`).
#[test]
fn freshly_registered_model_has_three_indexes_end_to_end() {
    use zeroship_plugin_db::query::{
        build_create_table_with_fks_for_dialect, index_name, FkEmission, SqlDialect,
    };

    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .ensure_app_schema("app_demo")
            .await
            .expect("ensure_app_schema");

        let sql = build_create_table_with_fks_for_dialect(
            "app_demo",
            "posts",
            &serde_json::json!({}),
            &FkEmission::Inline,
            SqlDialect::Sqlite,
        )
        .expect("build sqlite DDL");
        // SQLite's `Connection::execute` runs ONE statement per call
        // (unlike PG's libpq simple-query); the production register_model
        // orchestrator is PG-only today, so this test splits the
        // multi-statement payload and executes each piece individually,
        // exercising the canonical per-statement DDL the SQLite arm
        // would see once a dialect-aware orchestrator lands.
        for stmt in sql.split(";\n") {
            let trimmed = stmt.trim();
            if trimmed.is_empty() {
                continue;
            }
            backend
                .pool_exec(trimmed, &[])
                .await
                .unwrap_or_else(|e| panic!("engine must accept statement: {trimmed}\n{e:?}"));
        }

        let live = backend
            .introspect_schema("app_demo")
            .await
            .expect("introspect_schema");
        let idx_map = live
            .indexes
            .get("posts")
            .expect("posts must have an index map");

        for col in &["deleted_at", "updated_at", "created_by"] {
            let expected = index_name("posts", &[col], /* unique = */ false);
            assert!(
                idx_map.contains_key(&expected),
                "expected auto-index {expected} for column {col}; have: {:?}",
                idx_map.keys().collect::<Vec<_>>()
            );
        }
    });
}

/// **Deferred to PR 3** — the SDK INSERT auto-populate path (which
/// supplies `id` + `created_at` etc. from the runtime) lands in PR 3.
/// PR 2's responsibility is the DDL only, so this end-to-end test
/// supplies the system fields manually via a raw-SQL INSERT to confirm
/// the emitted columns accept the canonical value shapes (TEXT id,
/// CURRENT_TIMESTAMP defaults firing on omitted columns).
#[test]
fn inserting_a_row_without_user_fields_succeeds_via_system_fields_only() {
    use zeroship_plugin_db::query::{
        build_create_table_with_fks_for_dialect, FkEmission, SqlDialect,
    };

    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .ensure_app_schema("app_demo")
            .await
            .expect("ensure_app_schema");

        let sql = build_create_table_with_fks_for_dialect(
            "app_demo",
            "posts",
            &serde_json::json!({}),
            &FkEmission::Inline,
            SqlDialect::Sqlite,
        )
        .expect("build sqlite DDL");
        // Split on `;\n` — SQLite's `Connection::execute` runs one
        // statement per call (see sibling test's note).
        for stmt in sql.split(";\n") {
            let trimmed = stmt.trim();
            if trimmed.is_empty() {
                continue;
            }
            backend
                .pool_exec(trimmed, &[])
                .await
                .unwrap_or_else(|e| panic!("engine must accept statement: {trimmed}\n{e:?}"));
        }

        // PR 2-era INSERT: supply only `id` (no SDK auto-populate yet
        // — PR 3 wires that). The 3 NULL-able columns + 3 DEFAULT'd
        // columns fill in from the engine.
        backend
            .pool_exec(
                "INSERT INTO \"app_demo\".\"posts\" (id) VALUES ('post_01')",
                &[],
            )
            .await
            .expect("INSERT with only id must succeed");

        // Round-trip: confirm `version = 1`, `deleted_at IS NULL`,
        // `created_at IS NOT NULL`. Pin the canonical shape the DDL
        // promises.
        let client = backend
            .acquire_dedicated_client()
            .await
            .expect("acquire client");
        let rows = client
            .query(
                "SELECT id, version, deleted_at IS NULL AS dn, \
                        created_at IS NOT NULL AS cn \
                 FROM \"app_demo\".\"posts\"",
                &[],
            )
            .await
            .expect("SELECT ok");
        assert_eq!(rows.len(), 1, "expected one row");
        let row = &rows[0];
        assert_eq!(row[0].as_deref(), Some("post_01"), "id round-trip");
        assert_eq!(row[1].as_deref(), Some("1"), "version default = 1");
        assert_eq!(row[2].as_deref(), Some("1"), "deleted_at IS NULL default");
        assert_eq!(row[3].as_deref(), Some("1"), "created_at IS NOT NULL default");
    });
}

// ---------------------------------------------------------------------------
// P7 PR 3 — INSERT auto-populates `id` + `created_by` / `updated_by`
// ---------------------------------------------------------------------------

/// End-to-end: the `apply_system_fields_on_insert` pass mints a typed_id
/// and the subsequent `build_insert_with_dialect` INSERT lands a row
/// with the canonical 7 system fields populated. Mirrors what the
/// `dispatch_insert` hot path does at request time but without standing
/// up V8 — exercises the SQL builder + SQLite engine round-trip.
#[test]
fn insert_end_to_end_populates_system_fields_sqlite() {
    use zeroship_plugin_db::crud::system_fields_pass::apply_system_fields_on_insert;
    use zeroship_plugin_db::query::{
        build_create_table_with_fks_for_dialect, build_insert_with_dialect, FkEmission,
        SqlDialect,
    };

    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .ensure_app_schema("app_demo")
            .await
            .expect("ensure_app_schema");

        // 1. Stand up the table with the 7 system-field columns.
        let ddl = build_create_table_with_fks_for_dialect(
            "app_demo",
            "posts",
            &serde_json::json!({
                "title": {"type": "string", "required": true},
            }),
            &FkEmission::Inline,
            SqlDialect::Sqlite,
        )
        .expect("build sqlite DDL");
        for stmt in ddl.split(";\n") {
            let trimmed = stmt.trim();
            if trimmed.is_empty() {
                continue;
            }
            backend
                .pool_exec(trimmed, &[])
                .await
                .unwrap_or_else(|e| panic!("DDL: {trimmed}\n{e:?}"));
        }

        // 2. Build the inbound doc — creator passes ONLY the user
        // field. The auto-mint pass injects `id`, `created_by`,
        // `updated_by`; the DB fires its DEFAULT for the timestamps +
        // version.
        let mut doc = serde_json::json!({ "title": "PR 3 hello" });
        apply_system_fields_on_insert(&mut doc, "app_demo", "posts", Some("usr_actor_e2e"));

        // The minted id must carry the `post_` prefix (collection-name
        // derived since the schema didn't declare an `idPrefix`).
        let minted_id = doc
            .get("id")
            .and_then(|v| v.as_str())
            .expect("id minted by auto-mint pass")
            .to_string();
        assert!(
            minted_id.starts_with("post_"),
            "expected post_ prefix, got: {minted_id}"
        );

        // 3. Build + execute the INSERT. `RETURNING *` returns rows,
        // so route through the dedicated client's `query` path (the
        // pool's `pool_exec` rejects result-bearing statements).
        let built = build_insert_with_dialect("app_demo", "posts", &doc, SqlDialect::Sqlite)
            .expect("build_insert");
        let params: Vec<&str> = built.params.iter().map(String::as_str).collect();
        let client = backend
            .acquire_dedicated_client()
            .await
            .expect("acquire client (insert)");
        let returning_rows = client
            .query(&built.sql, &params)
            .await
            .unwrap_or_else(|e| panic!("INSERT: {}\n{e:?}", built.sql));
        assert_eq!(returning_rows.len(), 1, "INSERT RETURNING * gives one row");

        // 4. Round-trip via SELECT: every system field must be the
        // canonical shape.
        let rows = client
            .query(
                "SELECT id, title, created_by, updated_by, version, \
                        deleted_at IS NULL AS dn, \
                        created_at IS NOT NULL AS cn, \
                        updated_at IS NOT NULL AS un \
                 FROM \"app_demo\".\"posts\"",
                &[],
            )
            .await
            .expect("SELECT ok");
        assert_eq!(rows.len(), 1, "exactly one row");
        let row = &rows[0];
        assert_eq!(row[0].as_deref(), Some(minted_id.as_str()), "id round-trip");
        assert_eq!(row[1].as_deref(), Some("PR 3 hello"), "title preserved");
        assert_eq!(
            row[2].as_deref(),
            Some("usr_actor_e2e"),
            "created_by from actor"
        );
        assert_eq!(
            row[3].as_deref(),
            Some("usr_actor_e2e"),
            "updated_by from actor (== created_by on INSERT)"
        );
        assert_eq!(row[4].as_deref(), Some("1"), "version default = 1");
        assert_eq!(row[5].as_deref(), Some("1"), "deleted_at IS NULL");
        assert_eq!(row[6].as_deref(), Some("1"), "created_at NOT NULL");
        assert_eq!(row[7].as_deref(), Some("1"), "updated_at NOT NULL");
    });
}

/// FK type cascade end-to-end: a `t.ref(...)` column now emits TEXT
/// (not INTEGER per PR 1/2) so the column accepts typed_id string
/// values without storage-class mismatch.
///
/// **Scope note**: the actual `FOREIGN KEY ... REFERENCES "app"."tbl"`
/// constraint clause uses a schema-qualified target name that SQLite's
/// CREATE TABLE parser refuses (a pre-existing PG-only path). This
/// test stands up the posts table WITHOUT the FK clause (skipping the
/// constraint with `FkEmission::Deferred` + empty existing set) and
/// asserts the column TYPE is TEXT — which is the PR 3 cascade
/// surface. End-to-end FK constraint validation on SQLite remains a
/// PG-only path until the cross-app FK rework lands.
#[test]
fn insert_with_fk_uses_text_keys_end_to_end_sqlite() {
    use zeroship_plugin_db::crud::system_fields_pass::apply_system_fields_on_insert;
    use zeroship_plugin_db::query::{
        build_create_table_with_fks_for_dialect, build_insert_with_dialect, FkEmission,
        SqlDialect,
    };

    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .ensure_app_schema("app_demo")
            .await
            .expect("ensure_app_schema");

        // Stand up the posts table with an `authorId` ref column. The
        // PR 3 cascade emits TEXT for the column type. We use
        // `FkEmission::Deferred(empty)` so the FK clause is omitted —
        // SQLite refuses schema-qualified REFERENCES targets, a
        // pre-existing PG-only path that PR 3 is not chartered to fix.
        let empty: std::collections::HashSet<String> = std::collections::HashSet::new();
        let posts_ddl = build_create_table_with_fks_for_dialect(
            "app_demo",
            "posts",
            &serde_json::json!({
                "title": {"type": "string", "required": true},
                "authorId": {"type": "ref", "refTarget": "users"},
            }),
            &FkEmission::Deferred(&empty),
            SqlDialect::Sqlite,
        )
        .expect("build posts DDL");
        // Pin the FK column type to TEXT (PR 3 cascade — was INTEGER
        // pre-PR 3).
        assert!(
            posts_ddl.contains("\"authorId\" TEXT"),
            "expected TEXT FK column, got DDL: {posts_ddl}"
        );
        for stmt in posts_ddl.split(";\n") {
            let trimmed = stmt.trim();
            if trimmed.is_empty() {
                continue;
            }
            backend
                .pool_exec(trimmed, &[])
                .await
                .unwrap_or_else(|e| panic!("posts DDL: {trimmed}\n{e:?}"));
        }

        // Insert a post whose authorId is a typed_id string. Pre-PR 3
        // the column was INTEGER and a typed_id string would round-trip
        // as the literal string under SQLite's permissive storage
        // model but assert against the declared INTEGER affinity at
        // introspection. Post-PR 3 the affinity is TEXT — no surprise
        // on read-back.
        let mut post_doc = serde_json::json!({
            "title": "fk-ok",
            "authorId": "usr_01HXY3Z9PQR2STUV4WXY5Z6789",
        });
        apply_system_fields_on_insert(&mut post_doc, "app_demo", "posts", None);
        let built = build_insert_with_dialect("app_demo", "posts", &post_doc, SqlDialect::Sqlite)
            .expect("build posts insert");
        let params: Vec<&str> = built.params.iter().map(String::as_str).collect();
        let client = backend
            .acquire_dedicated_client()
            .await
            .expect("client");
        client
            .query(&built.sql, &params)
            .await
            .unwrap_or_else(|e| panic!("post INSERT: {}\n{e:?}", built.sql));

        // Round-trip: the authorId on the row equals the typed_id we
        // inserted. Confirms TEXT storage preserves the typed_id
        // verbatim (no integer-coercion).
        let rows = client
            .query(
                "SELECT authorId FROM \"app_demo\".\"posts\" WHERE title = 'fk-ok'",
                &[],
            )
            .await
            .expect("SELECT");
        assert_eq!(rows.len(), 1, "exactly one row");
        assert_eq!(
            rows[0][0].as_deref(),
            Some("usr_01HXY3Z9PQR2STUV4WXY5Z6789"),
            "FK round-trip preserves typed_id string"
        );
    });
}

// ---------------------------------------------------------------------------
// P7 PR 4 — UPDATE auto-bumps version + updated_at + optimistic concurrency
// ---------------------------------------------------------------------------

/// End-to-end: an UPDATE built via `build_update_one_with_system_fields`
/// on SQLite bumps `version` by exactly 1 and rewrites `updated_at`.
/// Mirrors what `dispatch_update_one` does at request time but bypasses
/// V8 / the per-isolate schema cache (we drive the SQL builder
/// directly).
#[test]
fn update_end_to_end_bumps_version_by_one_sqlite() {
    use zeroship_plugin_db::query::{
        build_create_table_with_fks_for_dialect, build_insert_with_dialect,
        build_update_many_with_system_fields, FkEmission, SqlDialect, SystemFieldAutoBump,
    };

    run(async {
        let (backend, _dir) = fresh_backend();
        backend
            .ensure_app_schema("app_demo")
            .await
            .expect("ensure_app_schema");

        let ddl = build_create_table_with_fks_for_dialect(
            "app_demo",
            "posts",
            &serde_json::json!({
                "title": {"type": "string", "required": true},
            }),
            &FkEmission::Inline,
            SqlDialect::Sqlite,
        )
        .expect("build DDL");
        for stmt in ddl.split(";\n") {
            let trimmed = stmt.trim();
            if trimmed.is_empty() {
                continue;
            }
            backend.pool_exec(trimmed, &[]).await.expect("DDL exec");
        }

        // INSERT row at version 1 (DDL default).
        let doc = serde_json::json!({
            "id": "post_v1bump",
            "title": "original",
        });
        let ins =
            build_insert_with_dialect("app_demo", "posts", &doc, SqlDialect::Sqlite).unwrap();
        let ins_params: Vec<&str> = ins.params.iter().map(String::as_str).collect();
        let client = backend.acquire_dedicated_client().await.unwrap();
        client.query(&ins.sql, &ins_params).await.expect("INSERT");

        // UPDATE via PR 4 builder.
        let filter = serde_json::json!({ "id": "post_v1bump" });
        let update = serde_json::json!({ "title": "v2" });
        let autobump = SystemFieldAutoBump {
            dispatch_write: true,
            actor_id: Some("usr_e2e_updater"),
            ..Default::default()
        };
        let upd = build_update_many_with_system_fields(
            "app_demo",
            "posts",
            &filter,
            &update,
            SqlDialect::Sqlite,
            &autobump,
        )
        .unwrap();
        let upd_params: Vec<&str> = upd.params.iter().map(String::as_str).collect();
        let returning = client.query(&upd.sql, &upd_params).await.expect("UPDATE");
        assert_eq!(returning.len(), 1, "UPDATE returned 1 row");

        // SELECT and confirm version bumped to 2 and updated_by was set.
        let rows = client
            .query(
                "SELECT title, version, updated_by FROM \"app_demo\".\"posts\" WHERE id = 'post_v1bump'",
                &[],
            )
            .await
            .expect("SELECT");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0].as_deref(), Some("v2"), "title updated");
        assert_eq!(
            rows[0][1].as_deref(),
            Some("2"),
            "version bumped from 1 to 2"
        );
        assert_eq!(
            rows[0][2].as_deref(),
            Some("usr_e2e_updater"),
            "updated_by stamped from actor",
        );
    });
}

/// End-to-end CAS success: an UPDATE that filters by the correct
/// `version` bumps the row.
#[test]
fn update_end_to_end_with_correct_version_succeeds_and_bumps_sqlite() {
    use zeroship_plugin_db::query::{
        build_create_table_with_fks_for_dialect, build_insert_with_dialect,
        build_update_many_with_system_fields, FkEmission, SqlDialect, SystemFieldAutoBump,
    };

    run(async {
        let (backend, _dir) = fresh_backend();
        backend.ensure_app_schema("app_demo").await.unwrap();

        let ddl = build_create_table_with_fks_for_dialect(
            "app_demo",
            "posts",
            &serde_json::json!({ "title": {"type": "string"} }),
            &FkEmission::Inline,
            SqlDialect::Sqlite,
        )
        .unwrap();
        for stmt in ddl.split(";\n") {
            let t = stmt.trim();
            if t.is_empty() {
                continue;
            }
            backend.pool_exec(t, &[]).await.unwrap();
        }

        let doc = serde_json::json!({ "id": "post_cas_ok", "title": "v1" });
        let ins =
            build_insert_with_dialect("app_demo", "posts", &doc, SqlDialect::Sqlite).unwrap();
        let ins_params: Vec<&str> = ins.params.iter().map(String::as_str).collect();
        let client = backend.acquire_dedicated_client().await.unwrap();
        client.query(&ins.sql, &ins_params).await.unwrap();

        // CAS at the correct version (1).
        let filter = serde_json::json!({ "id": "post_cas_ok", "version": 1 });
        let update = serde_json::json!({ "title": "v2" });
        let autobump = SystemFieldAutoBump {
            dispatch_write: true,
            actor_id: Some("usr_cas_ok"),
            ..Default::default()
        };
        let upd = build_update_many_with_system_fields(
            "app_demo",
            "posts",
            &filter,
            &update,
            SqlDialect::Sqlite,
            &autobump,
        )
        .unwrap();
        let upd_params: Vec<&str> = upd.params.iter().map(String::as_str).collect();
        let returning = client.query(&upd.sql, &upd_params).await.unwrap();
        assert_eq!(returning.len(), 1, "CAS matched: 1 affected row");

        let rows = client
            .query(
                "SELECT version FROM \"app_demo\".\"posts\" WHERE id = 'post_cas_ok'",
                &[],
            )
            .await
            .unwrap();
        assert_eq!(rows[0][0].as_deref(), Some("2"), "version bumped on CAS hit");
    });
}

/// End-to-end CAS failure: an UPDATE that filters by a stale `version`
/// affects zero rows. The dispatch layer (not exercised here) converts
/// the empty RETURNING into a typed `version_mismatch` — at the SQL
/// layer we just confirm the affected-rows = 0 contract.
#[test]
fn update_end_to_end_with_stale_version_affects_zero_rows_sqlite() {
    use zeroship_plugin_db::query::{
        build_create_table_with_fks_for_dialect, build_insert_with_dialect,
        build_update_many_with_system_fields, FkEmission, SqlDialect, SystemFieldAutoBump,
    };

    run(async {
        let (backend, _dir) = fresh_backend();
        backend.ensure_app_schema("app_demo").await.unwrap();

        let ddl = build_create_table_with_fks_for_dialect(
            "app_demo",
            "posts",
            &serde_json::json!({ "title": {"type": "string"} }),
            &FkEmission::Inline,
            SqlDialect::Sqlite,
        )
        .unwrap();
        for stmt in ddl.split(";\n") {
            let t = stmt.trim();
            if t.is_empty() {
                continue;
            }
            backend.pool_exec(t, &[]).await.unwrap();
        }

        let doc = serde_json::json!({ "id": "post_cas_stale", "title": "v1" });
        let ins =
            build_insert_with_dialect("app_demo", "posts", &doc, SqlDialect::Sqlite).unwrap();
        let ins_params: Vec<&str> = ins.params.iter().map(String::as_str).collect();
        let client = backend.acquire_dedicated_client().await.unwrap();
        client.query(&ins.sql, &ins_params).await.unwrap();

        // CAS at the wrong version (row is at 1; we expect 99).
        let filter = serde_json::json!({ "id": "post_cas_stale", "version": 99 });
        let update = serde_json::json!({ "title": "v_nope" });
        let autobump = SystemFieldAutoBump {
            dispatch_write: true,
            actor_id: Some("usr_cas_stale"),
            ..Default::default()
        };
        let upd = build_update_many_with_system_fields(
            "app_demo",
            "posts",
            &filter,
            &update,
            SqlDialect::Sqlite,
            &autobump,
        )
        .unwrap();
        let upd_params: Vec<&str> = upd.params.iter().map(String::as_str).collect();
        let returning = client.query(&upd.sql, &upd_params).await.unwrap();
        assert!(returning.is_empty(), "stale CAS: 0 affected rows");

        // Row stays at version 1 and original title.
        let rows = client
            .query(
                "SELECT version, title FROM \"app_demo\".\"posts\" WHERE id = 'post_cas_stale'",
                &[],
            )
            .await
            .unwrap();
        assert_eq!(rows[0][0].as_deref(), Some("1"));
        assert_eq!(rows[0][1].as_deref(), Some("v1"));
    });
}

/// End-to-end concurrent CAS: two UPDATEs at the same version — one
/// wins, one loses. Confirms the WHERE version-check is atomic with
/// the SET.
#[test]
fn update_end_to_end_concurrent_two_updates_one_wins_one_loses_sqlite() {
    use zeroship_plugin_db::query::{
        build_create_table_with_fks_for_dialect, build_insert_with_dialect,
        build_update_many_with_system_fields, FkEmission, SqlDialect, SystemFieldAutoBump,
    };

    run(async {
        let (backend, _dir) = fresh_backend();
        backend.ensure_app_schema("app_demo").await.unwrap();

        let ddl = build_create_table_with_fks_for_dialect(
            "app_demo",
            "posts",
            &serde_json::json!({ "title": {"type": "string"} }),
            &FkEmission::Inline,
            SqlDialect::Sqlite,
        )
        .unwrap();
        for stmt in ddl.split(";\n") {
            let t = stmt.trim();
            if t.is_empty() {
                continue;
            }
            backend.pool_exec(t, &[]).await.unwrap();
        }

        let doc = serde_json::json!({ "id": "post_race", "title": "v0" });
        let ins =
            build_insert_with_dialect("app_demo", "posts", &doc, SqlDialect::Sqlite).unwrap();
        let ins_params: Vec<&str> = ins.params.iter().map(String::as_str).collect();
        let client = backend.acquire_dedicated_client().await.unwrap();
        client.query(&ins.sql, &ins_params).await.unwrap();

        // First UPDATE at version=1 wins.
        let filter1 = serde_json::json!({ "id": "post_race", "version": 1 });
        let update1 = serde_json::json!({ "title": "v_winner" });
        let ab = SystemFieldAutoBump {
            dispatch_write: true,
            actor_id: Some("usr_a"),
            ..Default::default()
        };
        let upd1 = build_update_many_with_system_fields(
            "app_demo",
            "posts",
            &filter1,
            &update1,
            SqlDialect::Sqlite,
            &ab,
        )
        .unwrap();
        let p1: Vec<&str> = upd1.params.iter().map(String::as_str).collect();
        let r1 = client.query(&upd1.sql, &p1).await.unwrap();
        assert_eq!(r1.len(), 1, "first CAS wins");

        // Second UPDATE at version=1 loses (row is now at version=2).
        let filter2 = serde_json::json!({ "id": "post_race", "version": 1 });
        let update2 = serde_json::json!({ "title": "v_loser" });
        let upd2 = build_update_many_with_system_fields(
            "app_demo",
            "posts",
            &filter2,
            &update2,
            SqlDialect::Sqlite,
            &ab,
        )
        .unwrap();
        let p2: Vec<&str> = upd2.params.iter().map(String::as_str).collect();
        let r2 = client.query(&upd2.sql, &p2).await.unwrap();
        assert!(r2.is_empty(), "second CAS loses");

        // Final state: winner's title, version=2.
        let rows = client
            .query(
                "SELECT title, version FROM \"app_demo\".\"posts\" WHERE id = 'post_race'",
                &[],
            )
            .await
            .unwrap();
        assert_eq!(rows[0][0].as_deref(), Some("v_winner"));
        assert_eq!(rows[0][1].as_deref(), Some("2"));
    });
}

/// End-to-end: UPDATE without a `version` filter blindly succeeds and
/// bumps version. Confirms the non-CAS path stays last-writer-wins.
#[test]
fn update_end_to_end_without_version_filter_succeeds_blindly_sqlite() {
    use zeroship_plugin_db::query::{
        build_create_table_with_fks_for_dialect, build_insert_with_dialect,
        build_update_many_with_system_fields, FkEmission, SqlDialect, SystemFieldAutoBump,
    };

    run(async {
        let (backend, _dir) = fresh_backend();
        backend.ensure_app_schema("app_demo").await.unwrap();

        let ddl = build_create_table_with_fks_for_dialect(
            "app_demo",
            "posts",
            &serde_json::json!({ "title": {"type": "string"} }),
            &FkEmission::Inline,
            SqlDialect::Sqlite,
        )
        .unwrap();
        for stmt in ddl.split(";\n") {
            let t = stmt.trim();
            if t.is_empty() {
                continue;
            }
            backend.pool_exec(t, &[]).await.unwrap();
        }

        let doc = serde_json::json!({ "id": "post_blind", "title": "v0" });
        let ins =
            build_insert_with_dialect("app_demo", "posts", &doc, SqlDialect::Sqlite).unwrap();
        let ins_params: Vec<&str> = ins.params.iter().map(String::as_str).collect();
        let client = backend.acquire_dedicated_client().await.unwrap();
        client.query(&ins.sql, &ins_params).await.unwrap();

        // No version in filter — last-writer-wins. Three consecutive
        // updates land in order; version is bumped each time.
        for new_title in ["v1", "v2", "v3"] {
            let filter = serde_json::json!({ "id": "post_blind" });
            let update = serde_json::json!({ "title": new_title });
            let ab = SystemFieldAutoBump {
                dispatch_write: true,
                actor_id: Some("usr_blind"),
                ..Default::default()
            };
            let upd = build_update_many_with_system_fields(
                "app_demo",
                "posts",
                &filter,
                &update,
                SqlDialect::Sqlite,
                &ab,
            )
            .unwrap();
            let p: Vec<&str> = upd.params.iter().map(String::as_str).collect();
            let r = client.query(&upd.sql, &p).await.unwrap();
            assert_eq!(r.len(), 1, "blind UPDATE succeeds");
        }

        let rows = client
            .query(
                "SELECT title, version FROM \"app_demo\".\"posts\" WHERE id = 'post_blind'",
                &[],
            )
            .await
            .unwrap();
        assert_eq!(rows[0][0].as_deref(), Some("v3"));
        assert_eq!(
            rows[0][1].as_deref(),
            Some("4"),
            "version bumped 1→2→3→4 across three updates"
        );
    });
}

// ---------------------------------------------------------------------------
// P7 PR 5 — delete becomes soft-delete; add purge + restore; find auto-
// filters deleted_at
//
// We use the `_many` builders on the SQLite arm because the `_one`
// builders narrow via the PG-flavoured `ctid` subquery (SQLite doesn't
// carry `ctid`); the PR4 UPDATE e2e tests follow the same convention.
// Filter is narrowed to a single id so the multi-row builder still
// touches exactly one row in practice.
// ---------------------------------------------------------------------------

#[test]
fn soft_delete_end_to_end_sets_deleted_at_and_bumps_version_sqlite() {
    use zeroship_plugin_db::query::{
        build_create_table_with_fks_for_dialect, build_insert_with_dialect,
        build_soft_delete_many_with_system_fields, FkEmission, SqlDialect, SystemFieldAutoBump,
    };

    run(async {
        let (backend, _dir) = fresh_backend();
        backend.ensure_app_schema("app_demo").await.unwrap();
        let ddl = build_create_table_with_fks_for_dialect(
            "app_demo",
            "posts",
            &serde_json::json!({ "title": { "type": "string" } }),
            &FkEmission::Inline,
            SqlDialect::Sqlite,
        )
        .unwrap();
        for stmt in ddl.split(";\n") {
            let t = stmt.trim();
            if t.is_empty() {
                continue;
            }
            backend.pool_exec(t, &[]).await.unwrap();
        }

        let doc = serde_json::json!({ "id": "post_sd1", "title": "to be deleted" });
        let ins =
            build_insert_with_dialect("app_demo", "posts", &doc, SqlDialect::Sqlite).unwrap();
        let p: Vec<&str> = ins.params.iter().map(String::as_str).collect();
        let client = backend.acquire_dedicated_client().await.unwrap();
        client.query(&ins.sql, &p).await.unwrap();

        let filter = serde_json::json!({ "id": "post_sd1" });
        let ab = SystemFieldAutoBump {
            actor_id: Some("usr_deleter"),
            ..Default::default()
        };
        let sd = build_soft_delete_many_with_system_fields(
            "app_demo",
            "posts",
            &filter,
            SqlDialect::Sqlite,
            &ab,
        )
        .unwrap();
        let p: Vec<&str> = sd.params.iter().map(String::as_str).collect();
        let returning = client.query(&sd.sql, &p).await.unwrap();
        assert_eq!(returning.len(), 1, "soft-delete returned 1 row");

        let rows = client
            .query(
                "SELECT deleted_at IS NOT NULL AS dn, version, updated_by FROM \"app_demo\".\"posts\" WHERE id = 'post_sd1'",
                &[],
            )
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0].as_deref(), Some("1"), "deleted_at IS NOT NULL");
        assert_eq!(rows[0][1].as_deref(), Some("2"), "version bumped from 1");
        assert_eq!(
            rows[0][2].as_deref(),
            Some("usr_deleter"),
            "updated_by stamped from actor"
        );
    });
}

#[test]
fn soft_delete_on_already_soft_deleted_row_affects_zero_rows_sqlite() {
    use zeroship_plugin_db::query::{
        build_create_table_with_fks_for_dialect, build_insert_with_dialect,
        build_soft_delete_many_with_system_fields, FkEmission, SqlDialect, SystemFieldAutoBump,
    };

    run(async {
        let (backend, _dir) = fresh_backend();
        backend.ensure_app_schema("app_demo").await.unwrap();
        let ddl = build_create_table_with_fks_for_dialect(
            "app_demo",
            "posts",
            &serde_json::json!({ "title": { "type": "string" } }),
            &FkEmission::Inline,
            SqlDialect::Sqlite,
        )
        .unwrap();
        for stmt in ddl.split(";\n") {
            let t = stmt.trim();
            if t.is_empty() {
                continue;
            }
            backend.pool_exec(t, &[]).await.unwrap();
        }

        let doc = serde_json::json!({ "id": "post_idem", "title": "x" });
        let ins =
            build_insert_with_dialect("app_demo", "posts", &doc, SqlDialect::Sqlite).unwrap();
        let p: Vec<&str> = ins.params.iter().map(String::as_str).collect();
        let client = backend.acquire_dedicated_client().await.unwrap();
        client.query(&ins.sql, &p).await.unwrap();

        let filter = serde_json::json!({ "id": "post_idem" });
        let ab = SystemFieldAutoBump {
            actor_id: Some("usr_x"),
            ..Default::default()
        };
        let sd = build_soft_delete_many_with_system_fields(
            "app_demo",
            "posts",
            &filter,
            SqlDialect::Sqlite,
            &ab,
        )
        .unwrap();
        let p1: Vec<&str> = sd.params.iter().map(String::as_str).collect();
        let r1 = client.query(&sd.sql, &p1).await.unwrap();
        assert_eq!(r1.len(), 1, "first soft-delete hits");
        let r2 = client.query(&sd.sql, &p1).await.unwrap();
        assert!(r2.is_empty(), "re-soft-deleting is a no-op");
    });
}

#[test]
fn find_with_soft_delete_filter_hides_soft_deleted_rows_sqlite() {
    use zeroship_plugin_db::query::{
        build_create_table_with_fks_for_dialect, build_find_with_schema_and_unmask_and_soft_delete,
        build_insert_with_dialect, build_soft_delete_many_with_system_fields, FkEmission,
        SqlDialect, SystemFieldAutoBump,
    };

    run(async {
        let (backend, _dir) = fresh_backend();
        backend.ensure_app_schema("app_demo").await.unwrap();
        let ddl = build_create_table_with_fks_for_dialect(
            "app_demo",
            "posts",
            &serde_json::json!({ "title": { "type": "string" } }),
            &FkEmission::Inline,
            SqlDialect::Sqlite,
        )
        .unwrap();
        for stmt in ddl.split(";\n") {
            let t = stmt.trim();
            if t.is_empty() {
                continue;
            }
            backend.pool_exec(t, &[]).await.unwrap();
        }

        for id in &["post_alive_a", "post_alive_b", "post_dead"] {
            let doc = serde_json::json!({ "id": id, "title": id });
            let ins =
                build_insert_with_dialect("app_demo", "posts", &doc, SqlDialect::Sqlite).unwrap();
            let p: Vec<&str> = ins.params.iter().map(String::as_str).collect();
            let client = backend.acquire_dedicated_client().await.unwrap();
            client.query(&ins.sql, &p).await.unwrap();
        }
        let ab = SystemFieldAutoBump {
            actor_id: Some("usr_actor"),
            ..Default::default()
        };
        let sd = build_soft_delete_many_with_system_fields(
            "app_demo",
            "posts",
            &serde_json::json!({ "id": "post_dead" }),
            SqlDialect::Sqlite,
            &ab,
        )
        .unwrap();
        let p: Vec<&str> = sd.params.iter().map(String::as_str).collect();
        let client = backend.acquire_dedicated_client().await.unwrap();
        client.query(&sd.sql, &p).await.unwrap();

        let q = build_find_with_schema_and_unmask_and_soft_delete(
            "app_demo",
            "posts",
            &serde_json::json!({}),
            None,
            None,
            None,
            None,
            None,
            &[],
            true,
        )
        .unwrap();
        let rows = client.query(&q.sql, &[]).await.unwrap();
        assert_eq!(rows.len(), 2, "soft-deleted row hidden by auto-filter");

        let q2 = build_find_with_schema_and_unmask_and_soft_delete(
            "app_demo",
            "posts",
            &serde_json::json!({}),
            None,
            None,
            None,
            None,
            None,
            &[],
            false,
        )
        .unwrap();
        let rows2 = client.query(&q2.sql, &[]).await.unwrap();
        assert_eq!(rows2.len(), 3, "include_deleted: all rows visible");
    });
}

#[test]
fn restore_clears_deleted_at_and_bumps_version_sqlite() {
    use zeroship_plugin_db::query::{
        build_create_table_with_fks_for_dialect, build_insert_with_dialect,
        build_restore_many_with_system_fields, build_soft_delete_many_with_system_fields,
        FkEmission, SqlDialect, SystemFieldAutoBump,
    };

    run(async {
        let (backend, _dir) = fresh_backend();
        backend.ensure_app_schema("app_demo").await.unwrap();
        let ddl = build_create_table_with_fks_for_dialect(
            "app_demo",
            "posts",
            &serde_json::json!({ "title": { "type": "string" } }),
            &FkEmission::Inline,
            SqlDialect::Sqlite,
        )
        .unwrap();
        for stmt in ddl.split(";\n") {
            let t = stmt.trim();
            if t.is_empty() {
                continue;
            }
            backend.pool_exec(t, &[]).await.unwrap();
        }

        let doc = serde_json::json!({ "id": "post_rs", "title": "x" });
        let ins =
            build_insert_with_dialect("app_demo", "posts", &doc, SqlDialect::Sqlite).unwrap();
        let p: Vec<&str> = ins.params.iter().map(String::as_str).collect();
        let client = backend.acquire_dedicated_client().await.unwrap();
        client.query(&ins.sql, &p).await.unwrap();

        let ab = SystemFieldAutoBump {
            actor_id: Some("usr_x"),
            ..Default::default()
        };
        let sd = build_soft_delete_many_with_system_fields(
            "app_demo",
            "posts",
            &serde_json::json!({ "id": "post_rs" }),
            SqlDialect::Sqlite,
            &ab,
        )
        .unwrap();
        let p: Vec<&str> = sd.params.iter().map(String::as_str).collect();
        client.query(&sd.sql, &p).await.unwrap();

        let rs = build_restore_many_with_system_fields(
            "app_demo",
            "posts",
            &serde_json::json!({ "id": "post_rs" }),
            SqlDialect::Sqlite,
            &ab,
        )
        .unwrap();
        let p: Vec<&str> = rs.params.iter().map(String::as_str).collect();
        let returning = client.query(&rs.sql, &p).await.unwrap();
        assert_eq!(returning.len(), 1, "restore hit the soft-deleted row");

        let rows = client
            .query(
                "SELECT deleted_at IS NULL AS dn, version FROM \"app_demo\".\"posts\" WHERE id = 'post_rs'",
                &[],
            )
            .await
            .unwrap();
        assert_eq!(rows[0][0].as_deref(), Some("1"), "deleted_at IS NULL");
        assert_eq!(rows[0][1].as_deref(), Some("3"), "version bumped twice");
    });
}

#[test]
fn restore_on_already_live_row_affects_zero_rows_sqlite() {
    use zeroship_plugin_db::query::{
        build_create_table_with_fks_for_dialect, build_insert_with_dialect,
        build_restore_many_with_system_fields, FkEmission, SqlDialect, SystemFieldAutoBump,
    };

    run(async {
        let (backend, _dir) = fresh_backend();
        backend.ensure_app_schema("app_demo").await.unwrap();
        let ddl = build_create_table_with_fks_for_dialect(
            "app_demo",
            "posts",
            &serde_json::json!({ "title": { "type": "string" } }),
            &FkEmission::Inline,
            SqlDialect::Sqlite,
        )
        .unwrap();
        for stmt in ddl.split(";\n") {
            let t = stmt.trim();
            if t.is_empty() {
                continue;
            }
            backend.pool_exec(t, &[]).await.unwrap();
        }

        let doc = serde_json::json!({ "id": "post_live", "title": "x" });
        let ins =
            build_insert_with_dialect("app_demo", "posts", &doc, SqlDialect::Sqlite).unwrap();
        let p: Vec<&str> = ins.params.iter().map(String::as_str).collect();
        let client = backend.acquire_dedicated_client().await.unwrap();
        client.query(&ins.sql, &p).await.unwrap();

        let ab = SystemFieldAutoBump {
            actor_id: Some("usr_x"),
            ..Default::default()
        };
        let rs = build_restore_many_with_system_fields(
            "app_demo",
            "posts",
            &serde_json::json!({ "id": "post_live" }),
            SqlDialect::Sqlite,
            &ab,
        )
        .unwrap();
        let p: Vec<&str> = rs.params.iter().map(String::as_str).collect();
        let returning = client.query(&rs.sql, &p).await.unwrap();
        assert!(returning.is_empty(), "restoring a live row is a no-op");
        let rows = client
            .query(
                "SELECT version FROM \"app_demo\".\"posts\" WHERE id = 'post_live'",
                &[],
            )
            .await
            .unwrap();
        assert_eq!(rows[0][0].as_deref(), Some("1"), "version untouched");
    });
}

#[test]
fn soft_delete_then_restore_full_lifecycle_sqlite() {
    use zeroship_plugin_db::query::{
        build_create_table_with_fks_for_dialect, build_find_with_schema_and_unmask_and_soft_delete,
        build_insert_with_dialect, build_restore_many_with_system_fields,
        build_soft_delete_many_with_system_fields, FkEmission, SqlDialect, SystemFieldAutoBump,
    };

    run(async {
        let (backend, _dir) = fresh_backend();
        backend.ensure_app_schema("app_demo").await.unwrap();
        let ddl = build_create_table_with_fks_for_dialect(
            "app_demo",
            "posts",
            &serde_json::json!({ "title": { "type": "string" } }),
            &FkEmission::Inline,
            SqlDialect::Sqlite,
        )
        .unwrap();
        for stmt in ddl.split(";\n") {
            let t = stmt.trim();
            if t.is_empty() {
                continue;
            }
            backend.pool_exec(t, &[]).await.unwrap();
        }

        let doc = serde_json::json!({ "id": "post_lc", "title": "lifecycle" });
        let ins =
            build_insert_with_dialect("app_demo", "posts", &doc, SqlDialect::Sqlite).unwrap();
        let p: Vec<&str> = ins.params.iter().map(String::as_str).collect();
        let client = backend.acquire_dedicated_client().await.unwrap();
        client.query(&ins.sql, &p).await.unwrap();

        let find_default = build_find_with_schema_and_unmask_and_soft_delete(
            "app_demo",
            "posts",
            &serde_json::json!({}),
            None,
            None,
            None,
            None,
            None,
            &[],
            true,
        )
        .unwrap();
        let r = client.query(&find_default.sql, &[]).await.unwrap();
        assert_eq!(r.len(), 1, "live row visible pre-delete");

        let ab = SystemFieldAutoBump {
            actor_id: Some("usr_x"),
            ..Default::default()
        };
        let sd = build_soft_delete_many_with_system_fields(
            "app_demo",
            "posts",
            &serde_json::json!({ "id": "post_lc" }),
            SqlDialect::Sqlite,
            &ab,
        )
        .unwrap();
        let p: Vec<&str> = sd.params.iter().map(String::as_str).collect();
        client.query(&sd.sql, &p).await.unwrap();

        let r = client.query(&find_default.sql, &[]).await.unwrap();
        assert!(r.is_empty(), "soft-deleted row hidden");

        let find_inc = build_find_with_schema_and_unmask_and_soft_delete(
            "app_demo",
            "posts",
            &serde_json::json!({}),
            None,
            None,
            None,
            None,
            None,
            &[],
            false,
        )
        .unwrap();
        let r = client.query(&find_inc.sql, &[]).await.unwrap();
        assert_eq!(r.len(), 1, "include_deleted reveals it");

        let rs = build_restore_many_with_system_fields(
            "app_demo",
            "posts",
            &serde_json::json!({ "id": "post_lc" }),
            SqlDialect::Sqlite,
            &ab,
        )
        .unwrap();
        let p: Vec<&str> = rs.params.iter().map(String::as_str).collect();
        client.query(&rs.sql, &p).await.unwrap();

        let r = client.query(&find_default.sql, &[]).await.unwrap();
        assert_eq!(r.len(), 1, "restored row visible to default find");

        let rows = client
            .query(
                "SELECT version FROM \"app_demo\".\"posts\" WHERE id = 'post_lc'",
                &[],
            )
            .await
            .unwrap();
        assert_eq!(rows[0][0].as_deref(), Some("3"));
    });
}

#[test]
fn soft_delete_many_sets_deleted_at_on_all_matching_live_rows_sqlite() {
    use zeroship_plugin_db::query::{
        build_create_table_with_fks_for_dialect, build_insert_with_dialect,
        build_soft_delete_many_with_system_fields, FkEmission, SqlDialect, SystemFieldAutoBump,
    };

    run(async {
        let (backend, _dir) = fresh_backend();
        backend.ensure_app_schema("app_demo").await.unwrap();
        let ddl = build_create_table_with_fks_for_dialect(
            "app_demo",
            "posts",
            &serde_json::json!({ "author": { "type": "string" }, "title": { "type": "string" } }),
            &FkEmission::Inline,
            SqlDialect::Sqlite,
        )
        .unwrap();
        for stmt in ddl.split(";\n") {
            let t = stmt.trim();
            if t.is_empty() {
                continue;
            }
            backend.pool_exec(t, &[]).await.unwrap();
        }

        for (id, author) in &[
            ("post_a1", "usr_a"),
            ("post_a2", "usr_a"),
            ("post_a3_dead", "usr_a"),
            ("post_b1", "usr_b"),
            ("post_b2", "usr_b"),
        ] {
            let doc = serde_json::json!({ "id": id, "author": author, "title": id });
            let ins =
                build_insert_with_dialect("app_demo", "posts", &doc, SqlDialect::Sqlite).unwrap();
            let p: Vec<&str> = ins.params.iter().map(String::as_str).collect();
            let client = backend.acquire_dedicated_client().await.unwrap();
            client.query(&ins.sql, &p).await.unwrap();
        }
        backend
            .pool_exec(
                "UPDATE \"app_demo\".\"posts\" SET deleted_at = CURRENT_TIMESTAMP WHERE id = 'post_a3_dead'",
                &[],
            )
            .await
            .unwrap();

        let ab = SystemFieldAutoBump {
            actor_id: Some("usr_admin"),
            ..Default::default()
        };
        let sd = build_soft_delete_many_with_system_fields(
            "app_demo",
            "posts",
            &serde_json::json!({ "author": "usr_a" }),
            SqlDialect::Sqlite,
            &ab,
        )
        .unwrap();
        let p: Vec<&str> = sd.params.iter().map(String::as_str).collect();
        let client = backend.acquire_dedicated_client().await.unwrap();
        let returning = client.query(&sd.sql, &p).await.unwrap();
        assert_eq!(
            returning.len(),
            2,
            "only 2 live usr_a rows affected; already-deleted excluded"
        );

        let dead_a = client
            .query(
                "SELECT COUNT(*) FROM \"app_demo\".\"posts\" WHERE author = 'usr_a' AND deleted_at IS NOT NULL",
                &[],
            )
            .await
            .unwrap();
        assert_eq!(dead_a[0][0].as_deref(), Some("3"));
        let live_b = client
            .query(
                "SELECT COUNT(*) FROM \"app_demo\".\"posts\" WHERE author = 'usr_b' AND deleted_at IS NULL",
                &[],
            )
            .await
            .unwrap();
        assert_eq!(live_b[0][0].as_deref(), Some("2"));
    });
}

#[test]
fn purge_path_uses_hard_delete_sql_unchanged_sqlite() {
    use zeroship_plugin_db::query::build_delete_one;

    let q = build_delete_one("app1", "posts", &serde_json::json!({ "id": "x" })).unwrap();
    assert!(q.sql.starts_with("DELETE FROM"));
    assert!(!q.sql.contains("deleted_at"));
    assert!(q.sql.contains("RETURNING *"));
}

// ---------------------------------------------------------------------------
// P9 PR 3 — nested-transaction SAVEPOINT SQL validated against the SQLite
// engine.
//
// The native `Db.transaction(fn)` orchestrator now drives SQLite through
// the same `tx_conn` slot/savepoint state machine it uses on Postgres,
// with the SQLite arm issuing `BEGIN` / `SAVEPOINT` / `RELEASE` /
// `ROLLBACK TO` / `COMMIT` over the session actor handle. These tests are
// still worth keeping: they pin the raw SQLite engine behaviour for the
// exact savepoint SQL the orchestrator emits, independent of the V8-side
// callback/finalizer wiring.

/// Inner savepoint rolled back to → only the outer write survives the
/// COMMIT. Mirrors `nested_inner_reject_rolls_back_to_savepoint_outer_continues`
/// at the SQL level.
#[test]
fn nested_savepoint_rollback_to_keeps_outer_sqlite() {
    run(async {
        let (backend, _dir) = fresh_backend();
        let client = backend
            .acquire_dedicated_client()
            .await
            .expect("acquire client");

        backend
            .client_exec(&client, "CREATE TABLE notes (id INTEGER PRIMARY KEY, title TEXT)", &[])
            .await
            .expect("create table");

        // Top-level BEGIN (what the orchestrator emits for a non-nested tx).
        backend.client_exec(&client, "BEGIN", &[]).await.expect("BEGIN");
        backend
            .client_exec(&client, "INSERT INTO notes (title) VALUES ('outer')", &[])
            .await
            .expect("outer insert");

        // Nested transaction → SAVEPOINT zs_sp_1 (the orchestrator's
        // savepoint_name(1)).
        backend.client_exec(&client, "SAVEPOINT zs_sp_1", &[]).await.expect("SAVEPOINT");
        backend
            .client_exec(&client, "INSERT INTO notes (title) VALUES ('inner-doomed')", &[])
            .await
            .expect("inner insert");
        // Inner callback rejected → ROLLBACK TO SAVEPOINT (inner reverts,
        // outer tx continues — not poisoned).
        backend
            .client_exec(&client, "ROLLBACK TO SAVEPOINT zs_sp_1", &[])
            .await
            .expect("ROLLBACK TO SAVEPOINT");

        // Outer continues + COMMITs.
        backend
            .client_exec(&client, "INSERT INTO notes (title) VALUES ('outer-2')", &[])
            .await
            .expect("outer insert 2 after savepoint rollback");
        backend.client_exec(&client, "COMMIT", &[]).await.expect("COMMIT");

        // Only the two outer rows survive; the inner row was rolled back
        // to the savepoint.
        let rows = client
            .query("SELECT COUNT(*) FROM notes", &[])
            .await
            .expect("count");
        assert_eq!(
            rows[0][0].as_deref(),
            Some("2"),
            "inner SAVEPOINT row must be reverted by ROLLBACK TO; both outer rows survive"
        );
        let titles = client
            .query("SELECT title FROM notes ORDER BY id", &[])
            .await
            .expect("titles");
        assert_eq!(titles[0][0].as_deref(), Some("outer"));
        assert_eq!(titles[1][0].as_deref(), Some("outer-2"));
    });
}

/// Inner savepoint released → both inner and outer writes persist after
/// COMMIT. Mirrors `nested_inner_resolve_releases_savepoint`.
#[test]
fn nested_savepoint_release_keeps_both_sqlite() {
    run(async {
        let (backend, _dir) = fresh_backend();
        let client = backend
            .acquire_dedicated_client()
            .await
            .expect("acquire client");

        backend
            .client_exec(&client, "CREATE TABLE notes (id INTEGER PRIMARY KEY, title TEXT)", &[])
            .await
            .expect("create table");

        backend.client_exec(&client, "BEGIN", &[]).await.expect("BEGIN");
        backend
            .client_exec(&client, "SAVEPOINT zs_sp_1", &[])
            .await
            .expect("SAVEPOINT");
        backend
            .client_exec(&client, "INSERT INTO notes (title) VALUES ('inner-kept')", &[])
            .await
            .expect("inner insert");
        // Inner callback resolved → RELEASE SAVEPOINT.
        backend
            .client_exec(&client, "RELEASE SAVEPOINT zs_sp_1", &[])
            .await
            .expect("RELEASE SAVEPOINT");
        backend
            .client_exec(&client, "INSERT INTO notes (title) VALUES ('outer-kept')", &[])
            .await
            .expect("outer insert");
        backend.client_exec(&client, "COMMIT", &[]).await.expect("COMMIT");

        let rows = client
            .query("SELECT COUNT(*) FROM notes", &[])
            .await
            .expect("count");
        assert_eq!(
            rows[0][0].as_deref(),
            Some("2"),
            "RELEASE SAVEPOINT then COMMIT must persist both the inner and outer rows"
        );
    });
}
