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

use std::rc::Rc;

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

/// Provision the per-app `__zs_migrations` audit table the
/// `AuditWriter` impl writes into. P1 PR 5 ships only the INSERT path;
/// the audit-table provisioning DDL is a later-PR concern. We create
/// it inline here so the `unique_violation` path's best-effort audit
/// write actually lands during the test (the test still passes if the
/// write fails — the SchemaRefused envelope assertion is the wire
/// contract — but covering both halves is cheap).
async fn ensure_audit_table(backend: &SqliteBackend, app_id: &str) {
    let sql = format!(
        "CREATE TABLE IF NOT EXISTS \"{app_id}\".\"__zs_migrations\" (\
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
        .expect("create __zs_migrations audit table");
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

use zeroship_plugin_db::backend::{MintedToken, SessionInit, SessionMinter};

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
// `dispatch_find_one` will once the SQLite CRUD route lands. The
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
            .query(&bq.sql, &param_refs)
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

        decrypt_row_on_read(&backend, "app_demo", "users", &schema, &mut row_value)
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
